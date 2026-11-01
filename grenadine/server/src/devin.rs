//! The Devin backend. Every prompt runs one `devin acp` process that
//! speaks JSON-RPC 2.0, one message per line: the session is created or
//! resumed (which replays its updates), the mode and model are set, and
//! `session/prompt` streams `session/update` notifications until its
//! response ends the turn. Tool uses that need the user arrive as
//! `session/request_permission` and `elicitation/create` requests, which
//! the page answers through the same approval flow as Claude's.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use grenadine_core::api::{
    AgentEventKind, AgentModelInfo, AgentSession, AgentStatus, ApprovalOption,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{OnceCell, mpsc};

use crate::agent::{
    Backend, Decision, Stopper, StructuredJob, Turn, TurnSink, backend_command, queue_next,
    request_approval, stderr_tail,
};
use crate::sync::State;

/// The `initialize` params: the ACP version and that the client can't
/// read or write files, has no terminal, and answers form elicitations.
fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false,
            "elicitation": {"form": {}},
        }
    })
}

/// The Devin backend: the `devin` executable and what it offers.
pub struct Devin {
    /// The `devin` executable.
    exe: PathBuf,
    /// The models Devin offers, once it has listed them.
    models: OnceCell<Vec<AgentModelInfo>>,
}

impl Devin {
    pub fn new(exe: PathBuf) -> Devin {
        Devin {
            exe,
            models: OnceCell::new(),
        }
    }
}

impl Backend for Devin {
    /// The models Devin offers, listed once and kept; none while it
    /// can't list them.
    fn models(&self) -> BoxFuture<'_, Vec<AgentModelInfo>> {
        async move {
            match self.models.get_or_try_init(|| list_models(&self.exe)).await {
                Ok(models) => models.clone(),
                Err(e) => {
                    tracing::warn!("listing Devin's models: {e:#}");
                    Vec::new()
                }
            }
        }
        .boxed()
    }

    fn turn<'a>(
        &'a self,
        state: &'a Arc<State>,
        session: &'a AgentSession,
        t: Turn<'a>,
        sink: TurnSink<'a>,
    ) -> BoxFuture<'a, Result<AgentStatus>> {
        turn(&self.exe, state, session, t, sink).boxed()
    }

    /// Summaries and cleanup scans run on Claude Code.
    fn structured<'a>(&'a self, job: &'a StructuredJob<'a>) -> BoxFuture<'a, Result<Value>> {
        async move { bail!("{} can't run on Devin", job.name) }.boxed()
    }
}

/// A `devin acp` process's stdin, shared by the turn and the tasks that
/// answer its requests.
struct Acp {
    writer: mpsc::UnboundedSender<Value>,
    /// The next request id; the agent's request ids are strings, ours are
    /// numbers so they can't collide.
    next: AtomicU64,
}

impl Acp {
    /// Sends one JSON message.
    fn send(&self, message: Value) {
        let _ = self.writer.send(message);
    }

    /// Sends a request and returns its id.
    fn request(&self, method: &str, params: Value) -> u64 {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        id
    }

    /// Sends a notification.
    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Answers a request the agent made.
    fn respond(&self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    /// Tells a request the agent made that the method is unknown.
    fn respond_unknown(&self, id: &Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "unknown method"},
        }));
    }
}

/// What the pump makes of one stdout line.
enum Msg {
    /// A response to a client request: its id and the result or error.
    Response(u64, Result<Value, String>),
    /// A request the agent makes of the client: its id and method+params.
    Request(Value),
    /// A `session/update` notification's update.
    Update(Value),
    /// Anything else, such as `_cognition.ai/*` notifications.
    Ignored,
    /// Stdout closed.
    Eof,
}

/// Reads the process's stdout and feeds the parsed lines to a channel;
/// the turn reads them in order while requests answer on their own tasks.
fn pump(stdout: tokio::process::ChildStdout) -> mpsc::UnboundedReceiver<Msg> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let msg = match lines.next_line().await {
                Ok(Some(line)) => {
                    let Ok(m) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if m["method"] == "session/update" {
                        Msg::Update(m["params"]["update"].clone())
                    } else if m["method"].is_string() && m["id"].is_null() {
                        Msg::Ignored
                    } else if m["method"].is_string() {
                        Msg::Request(m)
                    } else if m["id"].is_u64() {
                        let id = m["id"].as_u64().unwrap();
                        let r = if m["error"].is_object() {
                            Err(m["error"]["message"].as_str().unwrap_or("error").to_owned())
                        } else {
                            Ok(m["result"].clone())
                        };
                        Msg::Response(id, r)
                    } else {
                        Msg::Ignored
                    }
                }
                Ok(None) | Err(_) => Msg::Eof,
            };
            let eof = matches!(msg, Msg::Eof);
            if tx.send(msg).is_err() || eof {
                return;
            }
        }
    });
    rx
}

/// The models Devin offers, from a throwaway `devin acp` session's model
/// config option; the session is deleted again so it doesn't stay in the
/// user's Devin history.
async fn list_models(exe: &Path) -> Result<Vec<AgentModelInfo>> {
    let dir = std::env::temp_dir().join(format!("grenadine-models-{}", crate::agent::uuid()));
    std::fs::create_dir_all(&dir).with_context(|| format!("can't create {}", dir.display()))?;
    let mut child = tokio::process::Command::new(exe)
        .arg("acp")
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {}", exe.display()))?;
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    /// Sends `request` and returns its result; requests the agent makes
    /// are declined and other lines skipped.
    async fn call(
        stdin: &mut tokio::process::ChildStdin,
        lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        });
        stdin.write_all(format!("{request}\n").as_bytes()).await?;
        while let Some(line) = lines.next_line().await? {
            let Ok(m) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if m["id"].as_u64() == Some(id) && m["method"].is_null() {
                if m["error"].is_object() {
                    bail!("{}", m["error"]["message"].as_str().unwrap_or("error"));
                }
                return Ok(m["result"].clone());
            }
            if m["method"].is_string() && !m["id"].is_null() {
                let answer = json!({
                    "jsonrpc": "2.0", "id": m["id"],
                    "error": {"code": -32601, "message": "unknown method"},
                });
                stdin.write_all(format!("{answer}\n").as_bytes()).await?;
            }
        }
        bail!("devin exited without answering {method}")
    }
    let models = async {
        call(&mut stdin, &mut lines, 1, "initialize", initialize_params()).await?;
        let dir_str = dir.to_string_lossy();
        let session = call(
            &mut stdin,
            &mut lines,
            2,
            "session/new",
            json!({"cwd": dir_str, "mcpServers": []}),
        )
        .await?;
        let models = session["configOptions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|o| o["id"] == "model")
            .map(|o| {
                o["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| {
                        Some(AgentModelInfo {
                            value: m["value"].as_str()?.to_owned(),
                            display_name: m["name"].as_str()?.to_owned(),
                            description: m["description"].as_str().unwrap_or_default().to_owned(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // The session only existed to ask; it doesn't belong in the
        // user's history. An error here doesn't fail the listing.
        let _ = call(
            &mut stdin,
            &mut lines,
            3,
            "session/delete",
            json!({"sessionId": session["sessionId"]}),
        )
        .await;
        Ok(models)
    };
    let models = tokio::time::timeout(Duration::from_secs(30), models)
        .await
        .context("devin didn't list its models in time")?;
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&dir);
    models
}

/// The current value of the `model` config option in a session answer.
fn current_model(result: &Value) -> Option<String> {
    result["configOptions"]
        .as_array()?
        .iter()
        .find(|o| o["id"] == "model")?["currentValue"]
        .as_str()
        .map(str::to_owned)
}

/// What the turn remembers of a tool call: the transcript's name and
/// input for it, and the text its updates have reported.
#[derive(Clone, Default)]
struct Tool {
    name: String,
    input: Value,
    content: String,
}

/// The transcript names Devin's tool calls come under.
fn tool_name(inference: &str) -> &str {
    match inference {
        "exec" => "Bash",
        "read" => "Read",
        "edit" => "Edit",
        "write" => "Write",
        "ask_user_question" => "AskUserQuestion",
        "exit_plan_mode" => "ExitPlanMode",
        "grep" => "Grep",
        "glob" => "Glob",
        other => other,
    }
}

/// The plan an `exit_plan_mode` call carries: the plan file's contents
/// without its front matter, or else the `plan` in its input.
fn plan_input(update: &Value) -> Value {
    let mut input = update["rawInput"].clone();
    let path = update["_meta"]["cognition.ai/planFilePath"].as_str();
    if let Some(body) = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| strip_front_matter(&s).to_owned())
    {
        input["plan"] = body.into();
    }
    input
}

/// The markdown after a plan file's `---`-framed front matter.
fn strip_front_matter(text: &str) -> &str {
    let text = text.trim_start();
    let Some(rest) = text.strip_prefix("---") else {
        return text;
    };
    match rest.find("\n---") {
        Some(end) => rest[end + 4..].trim_start(),
        None => text,
    }
}

/// The events a `session/update` means for the transcript, tracked across
/// a turn.
struct Updates {
    /// The agent's answer so far, flushed as one Text when something else
    /// happens.
    text: String,
    /// The turn's tool calls, by their ids, shared with the tasks that
    /// answer the requests about them.
    tools: Arc<Mutex<HashMap<String, Tool>>>,
    /// The turn process's running ACU total, from the latest usage.
    acu: Option<f64>,
}

impl Updates {
    fn new() -> Updates {
        Updates {
            text: String::new(),
            tools: Arc::new(Mutex::new(HashMap::new())),
            acu: None,
        }
    }
}

impl Updates {
    /// Ends the answer in progress; the `Text` event if it says anything.
    fn flush(&mut self) -> Vec<AgentEventKind> {
        if self.text.is_empty() {
            Vec::new()
        } else {
            vec![AgentEventKind::Text(std::mem::take(&mut self.text))]
        }
    }

    /// The events one `session/update` means; updates of subagents are
    /// skipped.
    fn update(&mut self, u: &Value) -> Vec<AgentEventKind> {
        if u["_meta"]["cognition.ai/subagent_context"]["parentAgentId"]
            .as_str()
            .is_some_and(|p| p != "root")
        {
            return Vec::new();
        }
        match u["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                let text = u["content"]["text"].as_str().unwrap_or_default();
                self.text += text;
                vec![AgentEventKind::TextDelta(text.to_owned())]
            }
            Some("tool_call") => {
                let id = u["toolCallId"].as_str().unwrap_or_default().to_owned();
                let inference = u["_meta"]["cognition.ai/inferenceToolName"]
                    .as_str()
                    .unwrap_or_default();
                let name = tool_name(inference).to_owned();
                let input = if name == "ExitPlanMode" {
                    plan_input(u)
                } else {
                    u["rawInput"].clone()
                };
                self.tools.lock().unwrap().insert(
                    id.clone(),
                    Tool {
                        name: name.clone(),
                        input: input.clone(),
                        content: String::new(),
                    },
                );
                self.flush()
                    .into_iter()
                    .chain([AgentEventKind::ToolUse {
                        id,
                        name,
                        input: input.to_string(),
                    }])
                    .collect()
            }
            Some("tool_call_update") => {
                let id = u["toolCallId"].as_str().unwrap_or_default();
                let status = u["status"].as_str().unwrap_or_default();
                let mut events = self.flush();
                let tool = self.tools.lock().unwrap().get_mut(id).cloned();
                if let Some(mut tool) = tool {
                    // Devin repeats the full output across updates, so the
                    // latest non-empty text wins over accumulation.
                    let text = u["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|c| c["type"] == "content")
                        .filter_map(|c| c["content"]["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        tool.content = text;
                    }
                    if matches!(status, "completed" | "failed") {
                        events.push(AgentEventKind::ToolResult {
                            id: id.to_owned(),
                            content: tool.content.clone(),
                            is_error: status == "failed",
                            patch: Vec::new(),
                            images: Vec::new(),
                        });
                    }
                    self.tools.lock().unwrap().insert(id.to_owned(), tool);
                }
                events
            }
            Some("usage_update") => {
                self.acu = u["_meta"]["cognition.ai/totalAcuCost"].as_f64();
                vec![AgentEventKind::Usage {
                    tokens: u["used"].as_u64(),
                    window: u["size"].as_u64(),
                }]
            }
            _ => Vec::new(),
        }
    }
}

/// What a turn tracks between its request/response exchanges.
struct TurnCtx<'a, 'b> {
    state: &'b Arc<State>,
    id: &'b str,
    sink: &'b mut TurnSink<'a>,
    /// The update→event mapping's state.
    updates: Updates,
    /// The turn's tool calls, shared with the request tasks.
    tools: Arc<Mutex<HashMap<String, Tool>>>,
    /// Responses to requests that haven't been awaited yet, by id.
    responses: HashMap<u64, Result<Value, String>>,
    /// While a `session/load` replays the conversation, its updates are
    /// discarded.
    loading: bool,
    /// `session/cancel` was already sent.
    cancelled: bool,
    /// Devin's session id, once `session/new` or `session/load` answered.
    backend_session: String,
}

/// The process's pipes and how to stop it, threaded through a turn.
struct Pipes {
    acp: Arc<Acp>,
    pump: mpsc::UnboundedReceiver<Msg>,
    cancel_rx: mpsc::UnboundedReceiver<()>,
}

/// Sends `method` and returns its result, meanwhile feeding updates to
/// the sink, requests to their own tasks, and `session/cancel` on stop.
async fn call(
    io: &mut Pipes,
    ctx: &mut TurnCtx<'_, '_>,
    method: &str,
    params: Value,
) -> Result<Value> {
    let acp = io.acp.clone();
    let state = ctx.state.clone();
    let id = ctx.id;
    let pump = &mut io.pump;
    let cancel_rx = &mut io.cancel_rx;
    let request_id = acp.request(method, params);
    loop {
        if let Some(r) = ctx.responses.remove(&request_id) {
            return r.map_err(|e| anyhow::anyhow!(e));
        }
        tokio::select! {
            msg = pump.recv() => match msg {
                Some(Msg::Response(id, r)) => {
                    if id == request_id {
                        return r.map_err(|e| anyhow::anyhow!(e));
                    }
                    ctx.responses.insert(id, r);
                }
                Some(Msg::Update(u)) => {
                    if !ctx.loading {
                        for e in ctx.updates.update(&u) {
                            ctx.sink.event(e)?;
                        }
                    }
                }
                Some(Msg::Request(m)) => request(
                    acp.clone(),
                    state.clone(),
                    id.to_owned(),
                    Arc::clone(&ctx.tools),
                    m,
                ),
                Some(Msg::Ignored) => {}
                Some(Msg::Eof) | None => bail!("devin closed its stdout"),
            },
            _ = cancel_rx.recv(), if !ctx.cancelled => {
                ctx.cancelled = true;
                cancel_turn(&acp, ctx);
            },
            _ = state.shutdown.cancelled(), if !ctx.cancelled => {
                ctx.cancelled = true;
                cancel_turn(&acp, ctx);
            },
        }
    }
}

/// Asks Devin to stop the turn.
fn cancel_turn(acp: &Arc<Acp>, ctx: &TurnCtx) {
    if !ctx.backend_session.is_empty() {
        acp.notify("session/cancel", json!({"sessionId": ctx.backend_session}));
    }
}

/// Runs `t` as a `devin acp` turn of `session` and returns the status the
/// session ends up in; `sink` gets what the turn's output means for the
/// transcript.
async fn turn(
    exe: &Path,
    state: &Arc<State>,
    session: &AgentSession,
    t: Turn<'_>,
    mut sink: TurnSink<'_>,
) -> Result<AgentStatus> {
    let id = &session.id;
    let mut child = backend_command(state, session, exe)
        .arg("acp")
        .spawn()
        .with_context(|| format!("can't run {}", exe.display()))?;
    let stderr_task = stderr_tail(child.stderr.take().unwrap());
    let mut stdin = child.stdin.take().unwrap();
    let (writer, mut write_rx) = mpsc::unbounded_channel::<Value>();
    let (close_tx, close_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut close_rx = std::pin::pin!(close_rx);
        loop {
            tokio::select! {
                m = write_rx.recv() => match m {
                    Some(m) => {
                        if stdin
                            .write_all(format!("{m}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    None => return,
                },
                _ = &mut close_rx => return,
            }
        }
    });
    let acp = Arc::new(Acp {
        writer,
        next: AtomicU64::new(0),
    });
    let pump = pump(child.stdout.take().unwrap());
    let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<()>();
    let mut io = Pipes {
        acp,
        pump,
        cancel_rx,
    };
    let result = {
        let updates = Updates::new();
        let mut ctx = TurnCtx {
            state,
            id,
            sink: &mut sink,
            tools: updates.tools.clone(),
            updates,
            responses: HashMap::new(),
            loading: false,
            cancelled: false,
            backend_session: String::new(),
        };
        run_turn(&mut io, cancel_tx, session, &t, &mut ctx).await
    };
    // The turn is over; stdin closing lets devin go on its own.
    let _ = close_tx.send(());
    let exit = tokio::select! {
        s = child.wait() => s?,
        _ = tokio::time::sleep(crate::agent::STOP_TIMEOUT) => {
            let _ = child.kill().await;
            child.wait().await?
        }
    };
    let stderr = stderr_task.await.unwrap_or_default();
    result?;
    sink.finish(|| {
        let mut message = format!("devin exited without finishing the turn ({exit})");
        if !stderr.is_empty() {
            message += &format!(":\n{stderr}");
        }
        message
    })
}

/// The handshake and the prompt of one turn.
async fn run_turn(
    io: &mut Pipes,
    cancel_tx: mpsc::UnboundedSender<()>,
    session: &AgentSession,
    t: &Turn<'_>,
    ctx: &mut TurnCtx<'_, '_>,
) -> Result<()> {
    let acp = io.acp.clone();
    let state = ctx.state.clone();
    let id = &session.id;
    call(io, ctx, "initialize", initialize_params()).await?;
    let mcp = json!([{
        "type": "http",
        "name": "grenadine",
        "url": t.mcp_url,
        "headers": [],
    }]);
    let mut model;
    if let Some(backend) = state.db.backend_session(id)? {
        // Resuming replays the whole conversation as updates; the
        // transcript already has them.
        ctx.loading = true;
        let result = call(
            io,
            ctx,
            "session/load",
            json!({"sessionId": backend, "cwd": session.worktree, "mcpServers": mcp}),
        )
        .await?;
        ctx.loading = false;
        ctx.backend_session = backend;
        model = current_model(&result);
    } else {
        let result = call(
            io,
            ctx,
            "session/new",
            json!({"cwd": session.worktree, "mcpServers": mcp}),
        )
        .await?;
        ctx.backend_session = result["sessionId"].as_str().unwrap_or_default().to_owned();
        model = current_model(&result);
    }
    // Interrupt and shutdown now know how to stop the turn. An interrupt
    // that beat the session's setup lands here, and a later one lands in
    // `call`'s select, so check between the steps.
    // Stopping again only sends to the channel again, which the turn
    // ignores once it cancelled.
    state.agents.set_stopper(
        id,
        Stopper::new(move || {
            let _ = cancel_tx.send(());
        }),
    );
    if state.agents.interrupted(id) && !ctx.cancelled {
        ctx.cancelled = true;
        cancel_turn(&acp, ctx);
    }
    if ctx.cancelled {
        return Ok(());
    }
    call(
        io,
        ctx,
        "session/set_mode",
        json!({"sessionId": ctx.backend_session, "modeId": t.mode.as_str()}),
    )
    .await?;
    if ctx.cancelled {
        return Ok(());
    }
    if let Some(m) = t.model.as_deref().filter(|m| *m != "default") {
        let result = call(
            io,
            ctx,
            "session/set_config_option",
            json!({"sessionId": ctx.backend_session, "configId": "model", "value": m}),
        )
        .await?;
        model = current_model(&result).or(model);
    }
    ctx.sink.started(&ctx.backend_session.clone(), model)?;
    if ctx.cancelled {
        return Ok(());
    }

    // The prompt's blocks: the notes first, then the images, then the
    // text.
    let mut blocks = Vec::new();
    if !t.notes.is_empty() {
        blocks.push(json!({"type": "text", "text": t.notes.join("\n\n")}));
    }
    for name in t.images {
        let bytes = tokio::fs::read(state.agents.images().join(id).join(name))
            .await
            .with_context(|| format!("can't read the image {name}"))?;
        blocks.push(json!({
            "type": "image",
            "mimeType": crate::agent::media_type(name).unwrap_or("image/png"),
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }));
    }
    if !t.prompt.trim().is_empty() {
        blocks.push(json!({"type": "text", "text": t.prompt}));
    }
    let result = call(
        io,
        ctx,
        "session/prompt",
        json!({"sessionId": ctx.backend_session, "prompt": blocks}),
    )
    .await;
    for e in ctx.updates.flush() {
        ctx.sink.event(e)?;
    }
    let (error, cost) = match result {
        Ok(r) => match r["stopReason"].as_str().unwrap_or_default() {
            // The interrupt's TurnEnded comes from `finish`.
            "cancelled" => return Ok(()),
            "end_turn" => (None, ctx.updates.acu),
            other => (Some(other.to_owned()), ctx.updates.acu),
        },
        Err(e) => (Some(format!("{e:#}")), ctx.updates.acu),
    };
    ctx.sink.event(AgentEventKind::TurnEnded { error, cost })
}

/// Answers the request `m` the agent made, on its own task.
fn request(
    acp: Arc<Acp>,
    state: Arc<State>,
    id: String,
    tools: Arc<Mutex<HashMap<String, Tool>>>,
    m: Value,
) {
    match m["method"].as_str() {
        Some("session/request_permission") => {
            tokio::spawn(permission(acp, state, id, tools, m));
        }
        Some("elicitation/create") => {
            tokio::spawn(elicitation(acp, state, id, tools, m));
        }
        _ => acp.respond_unknown(&m["id"]),
    }
}

/// The offered `(id, name, kind)` triples of a permission request.
fn permission_options(params: &Value) -> Vec<(String, String, String)> {
    params["options"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|o| {
            (
                o["optionId"].as_str().unwrap_or_default().to_owned(),
                o["name"].as_str().unwrap_or_default().to_owned(),
                o["kind"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// The id of the first offered option of `kind`.
fn first_option(options: &[(String, String, String)], kind: &str) -> Option<String> {
    options
        .iter()
        .find(|(_, _, k)| k == kind)
        .map(|(i, _, _)| i.clone())
}

/// The `optionId` the user's decision picks: their explicit choice, a
/// plan approval's `plan_<mode>` option, the first allow, or the first
/// reject.
fn decide(d: &Decision, tool: &Tool, options: &[(String, String, String)]) -> Option<String> {
    if let Some(o) = &d.option {
        return Some(o.clone());
    }
    if d.allow {
        if tool.name == "ExitPlanMode"
            && let Some(m) = &d.mode
        {
            let want = format!("plan_{}", m.as_str().replace('-', "_"));
            if options.iter().any(|(i, _, _)| *i == want) {
                return Some(want);
            }
        }
        first_option(options, "allow_once")
    } else {
        first_option(options, "reject_once")
    }
    .or_else(|| options.first().map(|(i, _, _)| i.clone()))
}

/// Answers a `session/request_permission` request.
async fn permission(
    acp: Arc<Acp>,
    state: Arc<State>,
    id: String,
    tools: Arc<Mutex<HashMap<String, Tool>>>,
    m: Value,
) {
    let rpc_id = m["id"].clone();
    let params = &m["params"];
    let tool_call = params["toolCall"]["toolCallId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let options = permission_options(params);
    // Grenadine's own MCP tools never ask the user.
    let grenadine = params["toolCall"]["_meta"]["cognition.ai/permissionRule"]
        .as_str()
        .is_some_and(|r| r.starts_with("mcp__grenadine__"));
    let outcome = if grenadine {
        json!({"outcome": "selected", "optionId": first_option(&options, "allow_once")})
    } else {
        let tool = tools
            .lock()
            .unwrap()
            .get(&tool_call)
            .cloned()
            .unwrap_or_default();
        let rx = match request_approval(
            &state,
            &id,
            &tool_call,
            &tool.name,
            &tool.input,
            options
                .iter()
                .map(|(i, n, k)| ApprovalOption {
                    id: i.clone(),
                    label: n.clone(),
                    kind: k.clone(),
                })
                .collect(),
        ) {
            Ok(rx) => rx,
            Err(e) => {
                tracing::warn!("agent {id}: {e:#}");
                acp.respond(&rpc_id, json!({"outcome": {"outcome": "cancelled"}}));
                return;
            }
        };
        let decision = tokio::select! {
            d = rx => d.ok(),
            _ = state.shutdown.cancelled() => None,
        };
        match decision {
            None => json!({"outcome": {"outcome": "cancelled"}}),
            Some(d) => {
                if !d.allow
                    && let Some(message) = d.message.clone().filter(|m| !m.trim().is_empty())
                {
                    // The feedback becomes the next prompt; a rejected
                    // plan keeps planning.
                    queue_next(&state, &id, message, tool.name == "ExitPlanMode");
                }
                json!({"outcome": {"outcome": "selected", "optionId": decide(&d, &tool, &options)}})
            }
        }
    };
    acp.respond(&rpc_id, outcome);
}

/// Answers an `elicitation/create` request, the questions of a
/// `ask_user_question` tool call.
async fn elicitation(
    acp: Arc<Acp>,
    state: Arc<State>,
    id: String,
    tools: Arc<Mutex<HashMap<String, Tool>>>,
    m: Value,
) {
    let rpc_id = m["id"].clone();
    let params = &m["params"];
    let tool_call = params["_meta"]["cognition.ai/toolCallId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let input = tools
        .lock()
        .unwrap()
        .get(&tool_call)
        .map(|t| t.input.clone())
        .unwrap_or(Value::Null);
    let rx = match request_approval(
        &state,
        &id,
        &tool_call,
        "AskUserQuestion",
        &input,
        Vec::new(),
    ) {
        Ok(rx) => rx,
        Err(e) => {
            tracing::warn!("agent {id}: {e:#}");
            acp.respond(&rpc_id, json!({"action": "decline"}));
            return;
        }
    };
    let decision = tokio::select! {
        d = rx => d.ok(),
        _ = state.shutdown.cancelled() => None,
    };
    let answer = match decision {
        Some(d) if d.allow => {
            let answers = d.answers.unwrap_or_default();
            // The schema's questions are keyed q0, q1, ...; their text is
            // each property's description, which the answers are keyed by.
            let content: serde_json::Map<_, _> = params["requestedSchema"]["properties"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(qid, prop)| {
                    let q = prop["description"].as_str()?;
                    answers.get(q).map(|a| (qid.clone(), json!(a)))
                })
                .collect();
            json!({"action": "accept", "content": content})
        }
        _ => json!({"action": "decline"}),
    };
    acp.respond(&rpc_id, answer);
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{AgentBackend, PermissionMode};

    use super::*;

    /// An update as `devin acp` writes it inside a session/update's
    /// `update` field.
    fn update(body: &str) -> Value {
        serde_json::from_str(body).unwrap()
    }

    /// The events a stream of updates yields, in order.
    fn events(updates: &[&str]) -> Vec<AgentEventKind> {
        let mut u = Updates::new();
        let mut events = Vec::new();
        for body in updates {
            events.extend(u.update(&update(body)));
        }
        events.extend(u.flush());
        events
    }

    /// Lines as `devin acp` 3000.11 writes them, shortened.
    #[test]
    fn maps_updates_to_events() {
        assert_eq!(
            events(&[
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"I'll do"}}"#,
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":" it."}}"#,
                r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"secret"}}"#,
                r#"{"sessionUpdate":"tool_call","toolCallId":"exec:0#abc","title":"Ran echo","kind":"execute","rawInput":{"command":"echo hi"},"_meta":{"cognition.ai/inferenceToolName":"exec"}}"#,
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"exec:0#abc","status":"in_progress","_meta":{"cognition.ai/inferenceToolName":"exec"}}"#,
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"exec:0#abc","status":"completed","content":[{"type":"content","content":{"type":"text","text":"hi\n"}}]}"#,
                r#"{"sessionUpdate":"usage_update","used":13889,"size":262000,"_meta":{"cognition.ai/totalAcuCost":0.0037,"cognition.ai/subagent_context":{"parentAgentId":"root"}}}"#,
            ]),
            [
                AgentEventKind::TextDelta("I'll do".into()),
                AgentEventKind::TextDelta(" it.".into()),
                AgentEventKind::Text("I'll do it.".into()),
                AgentEventKind::ToolUse {
                    id: "exec:0#abc".into(),
                    name: "Bash".into(),
                    input: r#"{"command":"echo hi"}"#.into(),
                },
                AgentEventKind::ToolResult {
                    id: "exec:0#abc".into(),
                    content: "hi\n".into(),
                    is_error: false,
                    patch: Vec::new(),
                    images: Vec::new(),
                },
                AgentEventKind::Usage {
                    tokens: Some(13889),
                    window: Some(262000),
                },
            ]
        );
    }

    /// A tool's output repeats across updates, so the result keeps the
    /// latest non-empty text, as /tmp/acp-probe/run1.log shows.
    #[test]
    fn a_repeated_output_is_kept_once() {
        assert_eq!(
            events(&[
                r#"{"sessionUpdate":"tool_call","toolCallId":"exec:0#8f29","title":"Ran echo","kind":"execute","rawInput":{"command":"echo hello"},"_meta":{"cognition.ai/inferenceToolName":"exec"}}"#,
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"exec:0#8f29","status":"in_progress","_meta":{"cognition.ai/inferenceToolName":"exec"}}"#,
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"exec:0#8f29","status":"in_progress","content":[{"type":"content","content":{"type":"text","text":"hello"}}]}"#,
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"exec:0#8f29","status":"in_progress","content":[{"type":"content","content":{"type":"text","text":"hello"}}]}"#,
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"exec:0#8f29","status":"completed","content":[{"type":"content","content":{"type":"text","text":"hello"}}]}"#,
            ])
            .last()
            .unwrap(),
            &AgentEventKind::ToolResult {
                id: "exec:0#8f29".into(),
                content: "hello".into(),
                is_error: false,
                patch: Vec::new(),
                images: Vec::new(),
            }
        );
    }

    /// /tmp/acp-probe/run2.log's completed exec reports its exit message.
    #[test]
    fn a_completed_exec_reports_its_exit() {
        assert_eq!(
            events(&[
                r#"{"sessionUpdate":"tool_call","toolCallId":"exec:0#b","rawInput":{"command":"sleep 30"},"_meta":{"cognition.ai/inferenceToolName":"exec"}}"#,
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"exec:0#b","status":"completed","content":[{"type":"content","content":{"type":"text","text":"Exited with code 0"}}]}"#,
            ])
            .last()
            .unwrap(),
            &AgentEventKind::ToolResult {
                id: "exec:0#b".into(),
                content: "Exited with code 0".into(),
                is_error: false,
                patch: Vec::new(),
                images: Vec::new(),
            }
        );
    }

    /// A failed tool call's result carries the error.
    #[test]
    fn a_failed_tool_call_is_an_error_result() {
        assert_eq!(
            events(&[
                r#"{"sessionUpdate":"tool_call","toolCallId":"exec:0#a","rawInput":{"command":"rm"},"_meta":{"cognition.ai/inferenceToolName":"exec"}}"#,
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"exec:0#a","status":"failed","content":[{"type":"content","content":{"type":"text","text":"no"}}]}"#,
            ])
            .last()
            .unwrap(),
            &AgentEventKind::ToolResult {
                id: "exec:0#a".into(),
                content: "no".into(),
                is_error: true,
                patch: Vec::new(),
                images: Vec::new(),
            }
        );
    }

    /// An `exit_plan_mode` call's input gets the plan file's contents
    /// without its front matter.
    #[test]
    fn an_exit_plan_reads_the_plan_file() {
        let dir = std::env::temp_dir().join(format!("grenadine-plan-{}", crate::agent::uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("plan.md");
        std::fs::write(
            &plan,
            "---\nagent: devin-local\n---\n# The plan\n\nDo the thing.\n",
        )
        .unwrap();
        let u = update(&format!(
            r#"{{"sessionUpdate":"tool_call","toolCallId":"exit_plan_mode:0#f","rawInput":{{"plan":"summary"}},"_meta":{{"cognition.ai/inferenceToolName":"exit_plan_mode","cognition.ai/planFilePath":"{}"}}}}"#,
            plan.display()
        ));
        let mut updates = Updates::new();
        let events = updates.update(&u);
        assert_eq!(
            events.last().unwrap(),
            &AgentEventKind::ToolUse {
                id: "exit_plan_mode:0#f".into(),
                name: "ExitPlanMode".into(),
                input: r##"{"plan":"# The plan\n\nDo the thing.\n"}"##.into(),
            }
        );
        std::fs::remove_dir_all(&dir).ok();
        // Without a readable file the input's `plan` stays.
        let u = update(
            r#"{"sessionUpdate":"tool_call","toolCallId":"exit_plan_mode:0#g","rawInput":{"plan":"summary"},"_meta":{"cognition.ai/inferenceToolName":"exit_plan_mode","cognition.ai/planFilePath":"/nonexistent"}}"#,
        );
        let events = updates.update(&u);
        assert_eq!(
            events.last().unwrap(),
            &AgentEventKind::ToolUse {
                id: "exit_plan_mode:0#g".into(),
                name: "ExitPlanMode".into(),
                input: r#"{"plan":"summary"}"#.into(),
            }
        );
    }

    /// Updates of a subagent are skipped; the root's usage still counts.
    #[test]
    fn subagent_updates_are_skipped() {
        let events = events(&[
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"sub"},"_meta":{"cognition.ai/subagent_context":{"parentAgentId":"sub-1"}}}"#,
            r#"{"sessionUpdate":"usage_update","used":100,"size":200,"_meta":{"cognition.ai/totalAcuCost":0.5,"cognition.ai/subagent_context":{"parentAgentId":"sub-1"}}}"#,
        ]);
        assert_eq!(events, []);
        let mut u = Updates::new();
        u.update(&update(
            r#"{"sessionUpdate":"usage_update","used":100,"size":200,"_meta":{"cognition.ai/totalAcuCost":0.5,"cognition.ai/subagent_context":{"parentAgentId":"root"}}}"#,
        ));
        assert_eq!(u.acu, Some(0.5));
    }

    /// The decision picks the option the page chose, a plan's mode
    /// option, or falls back to allow/reject.
    #[test]
    fn decisions_pick_options() {
        let options = vec![
            (
                "plan_accept_edits".to_owned(),
                "Yes, implement plan and accept edits".to_owned(),
                "allow_once".to_owned(),
            ),
            (
                "plan_bypass".to_owned(),
                "Yes, implement plan and bypass".to_owned(),
                "allow_once".to_owned(),
            ),
            (
                "reject_once".to_owned(),
                "No".to_owned(),
                "reject_once".to_owned(),
            ),
        ];
        let plan = Tool {
            name: "ExitPlanMode".into(),
            input: json!({"plan": "p"}),
            content: String::new(),
        };
        assert_eq!(
            decide(
                &Decision {
                    allow: true,
                    mode: Some(PermissionMode::from("bypass")),
                    ..Default::default()
                },
                &plan,
                &options
            ),
            Some("plan_bypass".into())
        );
        assert_eq!(
            decide(
                &Decision {
                    allow: true,
                    mode: Some(PermissionMode::from("accept-edits")),
                    ..Default::default()
                },
                &plan,
                &options
            ),
            Some("plan_accept_edits".into())
        );
        assert_eq!(
            decide(
                &Decision {
                    allow: false,
                    ..Default::default()
                },
                &plan,
                &options
            ),
            Some("reject_once".into())
        );
        assert_eq!(
            decide(
                &Decision {
                    allow: true,
                    option: Some("reject_once".into()),
                    ..Default::default()
                },
                &plan,
                &options
            ),
            Some("reject_once".into())
        );
    }

    /// A fake `devin acp` for the listing: it answers the handshake and
    /// logs every request's method to `log`.
    #[test]
    fn lists_models_and_deletes_the_asking_session() {
        let dir = std::env::temp_dir().join(format!("grenadine-list-{}", crate::agent::uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("methods");
        let exe = dir.join("devin");
        std::fs::write(
            &exe,
            format!(
                r#"#!/bin/sh
while IFS= read -r line; do
  echo "$line" | grep -o '"method":"[^"]*"' >> {log}
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  case "$line" in
  *'"initialize"'*) echo '{{"jsonrpc":"2.0","id":'$id',"result":{{"protocolVersion":1}}}}' ;;
  *'"session/new"'*) echo '{{"jsonrpc":"2.0","id":'$id',"result":{{"sessionId":"list-tulip","configOptions":[{{"id":"model","currentValue":"swe-2","options":[{{"value":"swe-2","name":"SWE-2"}},{{"value":"swe-2-high","name":"SWE-2 High"}}]}}]}}}}' ;;
  *'"session/delete"'*) echo '{{"jsonrpc":"2.0","id":'$id',"result":{{}}}}' ;;
  esac
done
"#,
                log = log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let models = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(list_models(&exe))
            .unwrap();
        assert_eq!(
            models
                .iter()
                .map(|m| (m.value.as_str(), m.display_name.as_str()))
                .collect::<Vec<_>>(),
            [("swe-2", "SWE-2"), ("swe-2-high", "SWE-2 High")]
        );
        let methods = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            methods.matches("method").count(),
            3,
            "the listing ran initialize, session/new and session/delete: {methods}"
        );
        assert!(methods.contains("session/delete"), "{methods}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Devin's modes and cost unit.
    #[test]
    fn devin_modes() {
        let devin = AgentBackend::Devin;
        assert_eq!(
            devin.modes(),
            [
                ("accept-edits", "Code"),
                ("smart", "Smart"),
                ("ask", "Ask"),
                ("plan", "Plan first"),
                ("bypass", "Bypass permissions"),
            ]
        );
        assert_eq!(devin.default_mode(), "accept-edits");
        assert_eq!(devin.after_plan(), ["accept-edits", "bypass"]);
        assert_eq!(devin.cost_unit(), grenadine_core::api::CostUnit::Acu);
    }
}
