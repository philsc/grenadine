//! The Devin backend. Every prompt runs one `devin acp` process that
//! speaks JSON-RPC 2.0, one message per line: the session is created or
//! resumed (which replays its updates), the mode and model are set, and
//! `session/prompt` streams `session/update` notifications until its
//! response ends the turn. Tool uses that need the user arrive as
//! `session/request_permission` and `elicitation/create` requests, which
//! the page answers through the same approval flow as Claude's.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use grenadine_core::api::{
    AgentEventKind, AgentModelInfo, AgentSession, AgentStatus, ApprovalKind, ApprovalOption,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use crate::agent::{
    Backend, Decision, JobKind, JobSpec, JobTools, ModelList, Stopper, Turn, TurnSink,
    await_approval, backend_command, queue_next, stderr_tail,
};
use crate::mcp::ALLOWED_TOOLS;
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

/// How Devin runs: its executable and the models of its background jobs.
pub struct DevinConfig {
    /// The `devin` executable.
    pub exe: PathBuf,
    /// The model summaries and cleanup scans run on.
    pub job_model: String,
    /// The model checks for stale cleanup items run on.
    pub prune_model: String,
}

/// The Devin backend: how it runs and what it offers.
pub struct Devin {
    config: DevinConfig,
    /// The models Devin offers.
    models: ModelList,
}

impl Devin {
    pub fn new(config: DevinConfig) -> Devin {
        Devin {
            config,
            models: ModelList::default(),
        }
    }

    /// The model `kind` of job runs on.
    fn job_model(&self, kind: JobKind) -> &str {
        match kind {
            JobKind::Summary | JobKind::Scan => &self.config.job_model,
            JobKind::Prune => &self.config.prune_model,
        }
    }
}

impl Backend for Devin {
    fn models(&self) -> BoxFuture<'_, Result<Vec<AgentModelInfo>>> {
        self.models
            .get("Devin", || list_models(&self.config.exe))
            .boxed()
    }

    fn turn<'a>(
        &'a self,
        state: &'a Arc<State>,
        session: &'a AgentSession,
        t: Turn<'a>,
        sink: TurnSink<'a>,
    ) -> BoxFuture<'a, Result<AgentStatus>> {
        turn(&self.config.exe, state, session, t, sink).boxed()
    }

    fn structured<'a>(
        &'a self,
        job: &'a JobSpec<'a>,
        cwd: &'a Path,
    ) -> BoxFuture<'a, Result<Value>> {
        structured(&self.config.exe, self.job_model(job.kind), job, cwd).boxed()
    }
}

/// A JSON-RPC request id. The client's are numbers and Devin's are
/// strings, so they can't collide.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum RequestId {
    Number(u64),
    String(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::String(s) => f.write_str(s),
        }
    }
}

/// A message `devin acp` writes, one per line.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Incoming {
    /// A request the agent makes of the client.
    Request(Request),
    /// The answer to one of the client's requests.
    Response(Response),
    /// A notification, such as `session/update` or Devin's own
    /// `_cognition.ai/*` ones.
    Notification(Notification),
}

#[derive(Debug, Deserialize)]
struct Request {
    id: RequestId,
    method: String,
    #[serde(default)]
    params: Value,
}

/// A response; its id is matched against the client's requests, whatever
/// its shape.
#[derive(Debug, Deserialize)]
struct Response {
    id: RequestId,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct Notification {
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    #[serde(default)]
    message: String,
}

/// `T`, or its default for `null`.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A count that may come as any JSON number.
fn count<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.map(|n| n as u64))
}

/// `T` from a result or params.
fn parse<T: DeserializeOwned>(v: Value) -> Result<T> {
    Ok(serde_json::from_value(v)?)
}

/// A `session/update` notification's params.
#[derive(Debug, Deserialize)]
struct SessionNotification {
    update: SessionUpdate,
}

/// What a `session/update` reports. `_meta` holds Devin's `cognition.ai/*`
/// extensions.
#[derive(Debug, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
enum SessionUpdate {
    AgentMessageChunk {
        content: ContentBlock,
        #[serde(rename = "_meta", default)]
        meta: Value,
    },
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
    UsageUpdate(UsageUpdate),
    /// Thoughts, plans, mode changes and whatever else the transcript
    /// doesn't show.
    #[serde(other)]
    Other,
}

impl SessionUpdate {
    fn meta(&self) -> Option<&Value> {
        match self {
            SessionUpdate::AgentMessageChunk { meta, .. } => Some(meta),
            SessionUpdate::ToolCall(c) => Some(&c.meta),
            SessionUpdate::ToolCallUpdate(u) => Some(&u.meta),
            SessionUpdate::UsageUpdate(u) => Some(&u.meta),
            SessionUpdate::Other => None,
        }
    }

    /// The subagent the update is about, by its id; `None` for the
    /// session's own agent, whose id is "root".
    fn subagent(&self) -> Option<&str> {
        self.meta()
            .and_then(|m| extension(m, "subagent_context"))
            .and_then(|c| c.get("parentAgentId"))
            .and_then(Value::as_str)
            .filter(|p| *p != "root")
    }
}

/// A piece of content; only text is shown.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCall {
    tool_call_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    raw_input: Value,
    #[serde(rename = "_meta", default)]
    meta: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallUpdate {
    tool_call_id: String,
    #[serde(default)]
    status: Option<ToolCallStatus>,
    #[serde(default, deserialize_with = "null_default")]
    content: Vec<ToolCallContent>,
    #[serde(rename = "_meta", default)]
    meta: Value,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolCallStatus {
    Completed,
    Failed,
    /// Pending, in progress, or one ACP adds.
    #[serde(other)]
    Other,
}

/// What a tool call produced; only content is shown, not diffs.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToolCallContent {
    Content {
        content: ContentBlock,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct UsageUpdate {
    #[serde(default, deserialize_with = "count")]
    used: Option<u64>,
    #[serde(default, deserialize_with = "count")]
    size: Option<u64>,
    #[serde(rename = "_meta", default)]
    meta: Value,
}

/// The `cognition.ai/<key>` extension in a `_meta`.
fn extension<'a>(meta: &'a Value, key: &str) -> Option<&'a Value> {
    meta.get(format!("cognition.ai/{key}"))
}

/// The answer to `session/new`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewSessionResponse {
    session_id: String,
    /// Read leniently: only the model option matters, and the others may
    /// come in shapes this doesn't know.
    #[serde(default, deserialize_with = "null_default")]
    config_options: Vec<Value>,
}

/// The answer to `session/load` and the requests that set the mode or a
/// config option; it may be `null`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigResponse {
    #[serde(default, deserialize_with = "null_default")]
    config_options: Vec<Value>,
}

/// A model the `model` config option offers.
#[derive(Debug, Deserialize)]
struct ConfigValue {
    value: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
}

/// The `model` config option among a session answer's.
fn model_option(options: &[Value]) -> Option<&Value> {
    options
        .iter()
        .find(|o| o.get("id").and_then(Value::as_str) == Some("model"))
}

/// The models the session's `model` config option offers; entries that
/// aren't models are skipped, and grouped ones are taken out of their
/// groups.
fn models(options: &[Value]) -> Vec<AgentModelInfo> {
    fn entries<'a>(options: &'a Value, out: &mut Vec<&'a Value>) {
        for o in options.as_array().into_iter().flatten() {
            match o.get("options") {
                Some(group) => entries(group, out),
                None => out.push(o),
            }
        }
    }
    let mut found = Vec::new();
    if let Some(o) = model_option(options).and_then(|o| o.get("options")) {
        entries(o, &mut found);
    }
    found
        .into_iter()
        .filter_map(|m| serde_json::from_value::<ConfigValue>(m.clone()).ok())
        .map(|m| AgentModelInfo {
            value: m.value,
            display_name: m.name,
            description: m.description.unwrap_or_default(),
        })
        .collect()
}

/// The model the session's `model` config option is set to.
fn current_model(options: &[Value]) -> Option<String> {
    model_option(options)?
        .get("currentValue")?
        .as_str()
        .map(str::to_owned)
}

/// The answer to `session/prompt`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptResponse {
    stop_reason: String,
}

/// A `session/request_permission` request's params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionRequest {
    tool_call: PermissionToolCall,
    options: Vec<PermissionOption>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionToolCall {
    tool_call_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "_meta", default)]
    meta: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionOption {
    option_id: String,
    name: String,
    kind: ApprovalKind,
}

/// An `elicitation/create` request's params: a form whose properties are
/// the questions, with each question's text as its description.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Elicitation {
    requested_schema: ElicitationSchema,
    #[serde(rename = "_meta", default)]
    meta: Value,
}

#[derive(Debug, Default, Deserialize)]
struct ElicitationSchema {
    #[serde(default)]
    properties: BTreeMap<String, ElicitationProperty>,
}

#[derive(Debug, Deserialize)]
struct ElicitationProperty {
    #[serde(default)]
    description: Option<String>,
}

/// A `devin acp` process's stdin, shared by the turn and the tasks that
/// answer its requests.
struct Acp {
    writer: mpsc::UnboundedSender<Value>,
    /// The next request id.
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
    fn respond(&self, id: &RequestId, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    /// Answers a request the agent made with an error.
    fn respond_error(&self, id: &RequestId, code: i64, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }));
    }
}

/// A running `devin acp`: its stdin behind a writer task, its stdout as
/// parsed messages and its stderr's tail.
struct Rpc {
    child: tokio::process::Child,
    acp: Arc<Acp>,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    stderr: tokio::task::JoinHandle<String>,
    /// Closes stdin.
    close: oneshot::Sender<()>,
}

impl Rpc {
    /// Starts `cmd`, a `devin acp`, with its stdio piped.
    fn spawn(mut cmd: tokio::process::Command) -> std::io::Result<Rpc> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stderr = stderr_tail(child.stderr.take().unwrap());
        let mut stdin = child.stdin.take().unwrap();
        let (writer, mut write_rx) = mpsc::unbounded_channel::<Value>();
        let (close, closed) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut closed = std::pin::pin!(closed);
            loop {
                tokio::select! {
                    m = write_rx.recv() => {
                        let Some(m) = m else { return };
                        if stdin.write_all(format!("{m}\n").as_bytes()).await.is_err() {
                            return;
                        }
                    }
                    _ = &mut closed => return,
                }
            }
        });
        let stdout = child.stdout.take().unwrap();
        let (tx, incoming) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<Incoming>(&line) {
                    Ok(m) => {
                        if tx.send(m).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("devin wrote a line that isn't JSON-RPC ({e}): {line}")
                    }
                }
            }
        });
        Ok(Rpc {
            child,
            acp: Arc::new(Acp {
                writer,
                next: AtomicU64::new(0),
            }),
            incoming,
            stderr,
            close,
        })
    }

    /// Sends `method` and returns its result. Requests devin makes
    /// meanwhile are declined, and anything else is dropped.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.call_with(method, params, decline).await
    }

    /// Sends `method` and returns its result; whatever else arrives
    /// meanwhile goes to `other`.
    async fn call_with(
        &mut self,
        method: &str,
        params: Value,
        mut other: impl FnMut(&Arc<Acp>, Incoming) -> Result<()>,
    ) -> Result<Value> {
        let id = self.acp.request(method, params);
        while let Some(m) = self.incoming.recv().await {
            match m {
                Incoming::Response(r) if r.id == RequestId::Number(id) => {
                    return match r.error {
                        Some(e) if e.message.is_empty() => Err(anyhow!("{method} failed")),
                        Some(e) => Err(anyhow!("{method}: {}", e.message)),
                        None => Ok(r.result),
                    };
                }
                m => other(&self.acp, m)?,
            }
        }
        bail!("devin closed its stdout")
    }

    /// Kills devin; returns its stderr's tail.
    async fn kill(mut self) -> String {
        let _ = self.child.kill().await;
        self.stderr.await.unwrap_or_default()
    }

    /// Closes stdin, which lets devin exit on its own, and kills it if it
    /// doesn't in time; returns how it exited and its stderr's tail.
    async fn close(mut self) -> Result<(std::process::ExitStatus, String)> {
        let _ = self.close.send(());
        let exit = tokio::select! {
            s = self.child.wait() => s?,
            _ = tokio::time::sleep(crate::agent::STOP_TIMEOUT) => {
                let _ = self.child.kill().await;
                self.child.wait().await?
            }
        };
        Ok((exit, self.stderr.await.unwrap_or_default()))
    }
}

/// Declines whatever the agent asks while nothing else answers it.
fn decline(acp: &Arc<Acp>, m: Incoming) -> Result<()> {
    if let Incoming::Request(r) = m {
        acp.respond_error(&r.id, -32601, "unknown method");
    }
    Ok(())
}

/// How long listing the models may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a throwaway session gets to be deleted.
const DELETE_TIMEOUT: Duration = Duration::from_secs(5);

/// The models Devin offers, from a throwaway `devin acp` session's model
/// config option; the session is deleted again so it doesn't stay in the
/// user's Devin history.
async fn list_models(exe: &Path) -> Result<Vec<AgentModelInfo>> {
    list_models_within(exe, LIST_TIMEOUT).await
}

/// `list_models`, giving up after `timeout`.
async fn list_models_within(exe: &Path, timeout: Duration) -> Result<Vec<AgentModelInfo>> {
    // It goes away with the listing, however that ends.
    let dir = tempfile::Builder::new()
        .prefix("grenadine-models-")
        .tempdir()
        .context("can't create a directory for listing Devin's models")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("acp").current_dir(dir.path());
    let mut rpc = Rpc::spawn(cmd).with_context(|| format!("running {}", exe.display()))?;
    let mut session = None;
    let listed = tokio::time::timeout(timeout, async {
        rpc.call("initialize", initialize_params()).await?;
        let result = rpc
            .call("session/new", json!({"cwd": dir.path(), "mcpServers": []}))
            .await?;
        let created: NewSessionResponse = parse(result)?;
        session = Some(created.session_id);
        Ok(models(&created.config_options))
    })
    .await;
    // The session only existed to ask; it doesn't belong in the user's
    // history, whether or not the listing worked. An error here doesn't
    // fail the listing.
    if let Some(session) = session {
        let delete = rpc.call("session/delete", json!({"sessionId": session}));
        let _ = tokio::time::timeout(DELETE_TIMEOUT, delete).await;
    }
    let listed = listed
        .map_err(|_| anyhow!("devin didn't list its models in time"))
        .and_then(|r| r);
    match listed {
        Ok(models) => Ok(models),
        // What devin said explains it, e.g. that the user isn't logged in.
        Err(e) => {
            let stderr = rpc.kill().await;
            if stderr.is_empty() {
                Err(e)
            } else {
                Err(anyhow!("{e:#}:\n{stderr}"))
            }
        }
    }
}

/// What the turn remembers of a tool call: the transcript's name and
/// input for it, and the text its updates have reported.
#[derive(Clone, Default)]
struct Tool {
    name: String,
    /// What Devin calls the call, e.g. "Ran echo".
    title: Option<String>,
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
fn plan_input(call: &ToolCall) -> Value {
    let mut input = call.raw_input.clone();
    let body = extension(&call.meta, "planFilePath")
        .and_then(Value::as_str)
        .and_then(|p| std::fs::read_to_string(p).ok());
    if let (Some(body), Some(fields)) = (body, input.as_object_mut()) {
        fields.insert("plan".into(), strip_front_matter(&body).into());
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

/// The turn's tool calls by their ids, shared with the tasks that answer
/// the requests about them.
type Tools = Arc<Mutex<HashMap<String, Tool>>>;

/// The events a `session/update` means for the transcript, tracked across
/// a turn.
struct Updates {
    /// The agent's answer so far, flushed as one Text when something else
    /// happens.
    text: String,
    tools: Tools,
    /// The turn process's running ACU total, from the latest usage.
    acu: Option<f64>,
}

impl Updates {
    fn new() -> Updates {
        Updates {
            text: String::new(),
            tools: Tools::default(),
            acu: None,
        }
    }

    /// Ends the answer in progress; the `Text` event if it says anything.
    fn flush(&mut self) -> Vec<AgentEventKind> {
        if self.text.is_empty() {
            Vec::new()
        } else {
            vec![AgentEventKind::Text(std::mem::take(&mut self.text))]
        }
    }

    /// The events one `session/update` means; updates of subagents are
    /// skipped, except their tool calls, which name the subagent.
    fn update(&mut self, u: &SessionUpdate) -> Vec<AgentEventKind> {
        let parent = u.subagent();
        if parent.is_some() && !matches!(u, SessionUpdate::ToolCall(_)) {
            return Vec::new();
        }
        match u {
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text { text },
                ..
            } => {
                self.text += text;
                vec![AgentEventKind::TextDelta(text.clone())]
            }
            SessionUpdate::ToolCall(call) => {
                let inference = extension(&call.meta, "inferenceToolName")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = tool_name(inference).to_owned();
                let input = if name == "ExitPlanMode" {
                    plan_input(call)
                } else {
                    call.raw_input.clone()
                };
                // A subagent's calls never get updates.
                if parent.is_none() {
                    self.tools.lock().unwrap().insert(
                        call.tool_call_id.clone(),
                        Tool {
                            name: name.clone(),
                            title: call.title.clone(),
                            input: input.clone(),
                            content: String::new(),
                        },
                    );
                }
                // A subagent's call doesn't end the root's answer.
                let text = if parent.is_none() {
                    self.flush()
                } else {
                    Vec::new()
                };
                text.into_iter()
                    .chain([AgentEventKind::ToolUse {
                        id: call.tool_call_id.clone(),
                        name,
                        input: input.to_string(),
                        parent: parent.map(str::to_owned),
                    }])
                    .collect()
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let mut events = self.flush();
                let mut tools = self.tools.lock().unwrap();
                let Some(tool) = tools.get_mut(&update.tool_call_id) else {
                    return events;
                };
                // Devin repeats the full output across updates, so the
                // latest non-empty text wins over accumulation.
                let text = update
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolCallContent::Content {
                            content: ContentBlock::Text { text },
                        } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    tool.content = text;
                }
                if let Some(status @ (ToolCallStatus::Completed | ToolCallStatus::Failed)) =
                    &update.status
                {
                    events.push(AgentEventKind::ToolResult {
                        id: update.tool_call_id.clone(),
                        content: tool.content.clone(),
                        is_error: *status == ToolCallStatus::Failed,
                        patch: Vec::new(),
                        images: Vec::new(),
                    });
                }
                events
            }
            SessionUpdate::UsageUpdate(usage) => {
                self.acu = extension(&usage.meta, "totalAcuCost").and_then(Value::as_f64);
                vec![AgentEventKind::Usage {
                    tokens: usage.used,
                    window: usage.size,
                }]
            }
            SessionUpdate::AgentMessageChunk { .. } | SessionUpdate::Other => Vec::new(),
        }
    }
}

/// What a turn tracks while it talks to devin.
struct TurnCtx<'a, 'b> {
    state: &'b Arc<State>,
    id: &'b str,
    sink: &'b mut TurnSink<'a>,
    /// The update→event mapping's state.
    updates: Updates,
    /// The tasks that answer the agent's requests, and the one that stops
    /// the turn when the server shuts down; they end with the turn.
    tasks: JoinSet<()>,
    /// While a `session/load` replays the conversation, its updates are
    /// discarded.
    loading: bool,
}

impl TurnCtx<'_, '_> {
    /// Takes what devin sent while the turn waited for a response.
    fn handle(&mut self, acp: &Arc<Acp>, m: Incoming) -> Result<()> {
        match m {
            Incoming::Notification(n) if n.method == "session/update" => {
                let n = match serde_json::from_value::<SessionNotification>(n.params) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!("agent {}: an update that doesn't parse: {e}", self.id);
                        return Ok(());
                    }
                };
                if !self.loading {
                    for e in self.updates.update(&n.update) {
                        self.sink.event(e)?;
                    }
                    // ACUs are billed as they're spent, so a turn that
                    // never ends pays for what it used too.
                    if let Some(total) = self.updates.acu {
                        self.sink.bill(total)?;
                    }
                }
            }
            Incoming::Request(r) => self.request(acp, r),
            Incoming::Notification(_) | Incoming::Response(_) => {}
        }
        Ok(())
    }

    /// Answers the request `r` the agent made, on a task of its own.
    fn request(&mut self, acp: &Arc<Acp>, r: Request) {
        let (acp, state, id, tools) = (
            acp.clone(),
            self.state.clone(),
            self.id.to_owned(),
            self.updates.tools.clone(),
        );
        match r.method.as_str() {
            "session/request_permission" => match parse::<PermissionRequest>(r.params) {
                Ok(p) => {
                    self.tasks.spawn(permission(acp, state, id, tools, r.id, p));
                }
                Err(e) => acp.respond_error(&r.id, -32602, &format!("{e}")),
            },
            "elicitation/create" => match parse::<Elicitation>(r.params) {
                Ok(e) => {
                    self.tasks
                        .spawn(elicitation(acp, state, id, tools, r.id, e));
                }
                Err(e) => acp.respond_error(&r.id, -32602, &format!("{e}")),
            },
            _ => acp.respond_error(&r.id, -32601, "unknown method"),
        }
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
    let mut cmd = backend_command(state, session, exe);
    cmd.arg("acp");
    let mut rpc = Rpc::spawn(cmd).with_context(|| format!("can't run {}", exe.display()))?;
    let (result, acu) = {
        let mut ctx = TurnCtx {
            state,
            id: &session.id,
            sink: &mut sink,
            updates: Updates::new(),
            tasks: JoinSet::new(),
            loading: false,
        };
        let result = run_turn(&mut rpc, session, &t, &mut ctx).await;
        (result, ctx.updates.acu)
    };
    // The turn is over; stdin closing lets devin go on its own.
    let (exit, stderr) = rpc.close().await?;
    // What devin said on its way out explains why it went, e.g. that the
    // user isn't logged in.
    let explain = |message: String| {
        if stderr.is_empty() {
            format!("{message} ({exit})")
        } else {
            format!("{message} ({exit}):\n{stderr}")
        }
    };
    let error = match result.map_err(|e| anyhow!(explain(format!("{e:#}"))))? {
        Prompted::No => None,
        Prompted::Stopped(reason) if reason == "end_turn" => Some(None),
        Prompted::Stopped(reason) => Some(Some(reason)),
        Prompted::Failed(e) => Some(Some(explain(format!("{e:#}")))),
    };
    if let Some(error) = error {
        sink.event(AgentEventKind::TurnEnded { error, cost: acu })?;
    }
    sink.finish(|| explain("devin didn't finish the turn".into()))
}

/// How a turn's prompt went.
enum Prompted {
    /// It didn't run, or it was cancelled; `TurnSink::finish` tells which.
    No,
    /// Devin answered it with this stop reason.
    Stopped(String),
    /// Devin went away, or answered with an error.
    Failed(anyhow::Error),
}

/// The handshake and the prompt of one turn.
async fn run_turn(
    rpc: &mut Rpc,
    session: &AgentSession,
    t: &Turn<'_>,
    ctx: &mut TurnCtx<'_, '_>,
) -> Result<Prompted> {
    let state = ctx.state.clone();
    let id = &session.id;
    rpc.call_with("initialize", initialize_params(), |acp, m| {
        ctx.handle(acp, m)
    })
    .await?;
    let mcp = json!([{
        "type": "http",
        "name": "grenadine",
        "url": t.mcp_url,
        "headers": [],
    }]);
    let (backend_session, mut model) = if let Some(backend) = state.db.backend_session(id)? {
        // Resuming replays the whole conversation as updates; the
        // transcript already has them.
        ctx.loading = true;
        let result = rpc
            .call_with(
                "session/load",
                json!({"sessionId": backend, "cwd": session.worktree, "mcpServers": mcp}),
                |acp, m| ctx.handle(acp, m),
            )
            .await?;
        ctx.loading = false;
        let loaded = parse::<Option<ConfigResponse>>(result)?.unwrap_or_default();
        (backend, current_model(&loaded.config_options))
    } else {
        let result = rpc
            .call_with(
                "session/new",
                json!({"cwd": session.worktree, "mcpServers": mcp}),
                |acp, m| ctx.handle(acp, m),
            )
            .await?;
        let created: NewSessionResponse = parse(result)?;
        let model = current_model(&created.config_options);
        (created.session_id, model)
    };
    // Interrupts and shutdown now stop the turn with session/cancel; Devin
    // ignores one with nothing running, so stopping again is harmless.
    let stopper = {
        let (acp, backend_session) = (rpc.acp.clone(), backend_session.clone());
        Stopper::new(move || acp.notify("session/cancel", json!({"sessionId": backend_session})))
    };
    state.agents.set_stopper(id, stopper.clone());
    ctx.tasks.spawn({
        let state = state.clone();
        async move {
            state.shutdown.cancelled().await;
            stopper.stop();
        }
    });
    // An interrupt, or a shutdown, that came before the prompt started had
    // nothing to cancel, so the prompt doesn't start.
    let stopped = || state.agents.interrupted(id) || state.shutdown.is_cancelled();
    if stopped() {
        return Ok(Prompted::No);
    }
    rpc.call_with(
        "session/set_mode",
        json!({"sessionId": backend_session, "modeId": t.mode.as_str()}),
        |acp, m| ctx.handle(acp, m),
    )
    .await?;
    if let Some(m) = t.model.as_deref().filter(|m| *m != "default") {
        let result = rpc
            .call_with(
                "session/set_config_option",
                json!({"sessionId": backend_session, "configId": "model", "value": m}),
                |acp, m| ctx.handle(acp, m),
            )
            .await?;
        let set = parse::<Option<ConfigResponse>>(result)?.unwrap_or_default();
        model = current_model(&set.config_options).or(model);
    }
    ctx.sink.started(&backend_session, model)?;
    if stopped() {
        return Ok(Prompted::No);
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
    let result = rpc
        .call_with(
            "session/prompt",
            json!({"sessionId": backend_session, "prompt": blocks}),
            |acp, m| ctx.handle(acp, m),
        )
        .await;
    for e in ctx.updates.flush() {
        ctx.sink.event(e)?;
    }
    Ok(match result.and_then(parse::<PromptResponse>) {
        // The interrupt's TurnEnded comes from `finish`.
        Ok(r) if r.stop_reason == "cancelled" => Prompted::No,
        Ok(r) => Prompted::Stopped(r.stop_reason),
        Err(e) => Prompted::Failed(e),
    })
}

/// The offered answers of a permission request.
fn permission_options(request: &PermissionRequest) -> Vec<ApprovalOption> {
    request
        .options
        .iter()
        .map(|o| ApprovalOption {
            id: o.option_id.clone(),
            label: o.name.clone(),
            kind: o.kind,
        })
        .collect()
}

/// The id of the first offered option of `kind`.
fn first_option(options: &[ApprovalOption], kind: ApprovalKind) -> Option<String> {
    options
        .iter()
        .find(|o| o.kind == kind)
        .map(|o| o.id.clone())
}

/// The `optionId` the user's decision picks: their explicit choice, a
/// plan approval's `plan_<mode>` option, or an allow or a reject as the
/// decision says. With none of the decision's kind there's nothing to
/// pick, since another kind would do the opposite.
fn decide(d: &Decision, tool: &Tool, options: &[ApprovalOption]) -> Option<String> {
    if let Some(o) = &d.option {
        return Some(o.clone());
    }
    let first = |kinds: &[ApprovalKind]| kinds.iter().find_map(|&k| first_option(options, k));
    if d.allow {
        if tool.name == "ExitPlanMode"
            && let Some(m) = &d.mode
        {
            let want = format!("plan_{}", m.as_str().replace('-', "_"));
            if options.iter().any(|o| o.id == want) {
                return Some(want);
            }
        }
        first(&[ApprovalKind::AllowOnce, ApprovalKind::AllowAlways])
    } else {
        first(&[ApprovalKind::RejectOnce, ApprovalKind::RejectAlways])
    }
}

/// The prefix of grenadine's MCP tools, as Devin names them.
const GRENADINE_TOOLS: &str = "mcp__grenadine__";

/// Whether a permission request goes ahead without asking: one for
/// grenadine's MCP server, by the request's permission rule (Devin's is
/// `mcp__grenadine__*`) or the tool's name, whose tool `names` show to be
/// among those Claude may use without asking, `ALLOWED_TOOLS`. One for a
/// tool that can't be told, or for `approve`, asks.
fn auto_allowed<'a>(rule: Option<&'a str>, names: impl Iterator<Item = &'a str> + Clone) -> bool {
    let grenadine = rule
        .into_iter()
        .chain(names.clone())
        .any(|n| n.starts_with(GRENADINE_TOOLS));
    // Devin may call a tool by its full name, its bare one, or something
    // ending in either.
    let tools = ALLOWED_TOOLS
        .iter()
        .map(|t| t.trim_start_matches(GRENADINE_TOOLS))
        .chain(["approve"]);
    let named: Vec<_> = tools
        .filter(|t| {
            names.clone().any(|n| {
                let bare = n.trim_start_matches(GRENADINE_TOOLS);
                bare == *t || n.ends_with(&format!("__{t}")) || n.ends_with(&format!("/{t}"))
            })
        })
        .collect();
    grenadine && !named.is_empty() && !named.contains(&"approve")
}

/// The answer to a `session/request_permission` request that picks
/// `option`; without one, it's cancelled.
fn permission_outcome(option: Option<String>) -> Value {
    match option {
        Some(o) => json!({"outcome": {"outcome": "selected", "optionId": o}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

/// Answers a `session/request_permission` request. It runs on a task the
/// turn owns, so a request still waiting when the turn ends is dropped,
/// and with it the card's pending request.
async fn permission(
    acp: Arc<Acp>,
    state: Arc<State>,
    id: String,
    tools: Tools,
    rpc_id: RequestId,
    request: PermissionRequest,
) {
    let tool_call = &request.tool_call.tool_call_id;
    let options = permission_options(&request);
    let tool = tools
        .lock()
        .unwrap()
        .get(tool_call)
        .cloned()
        .unwrap_or_default();
    let rule = extension(&request.tool_call.meta, "permissionRule").and_then(Value::as_str);
    let names = [
        Some(tool.name.as_str()),
        tool.title.as_deref(),
        request.tool_call.title.as_deref(),
        rule,
    ];
    let decision = if auto_allowed(rule, names.into_iter().flatten()) {
        Some(Decision {
            allow: true,
            ..Decision::default()
        })
    } else {
        await_approval(
            &state,
            &id,
            tool_call,
            &tool.name,
            &tool.input,
            options.clone(),
        )
        .await
    };
    let option = decision.and_then(|d| {
        if !d.allow
            && let Some(message) = d.message.clone().filter(|m| !m.trim().is_empty())
        {
            // The feedback becomes the next prompt; a rejected plan keeps
            // planning.
            queue_next(&state, &id, message, tool.name == "ExitPlanMode");
        }
        decide(&d, &tool, &options)
    });
    acp.respond(&rpc_id, permission_outcome(option));
}

/// Answers an `elicitation/create` request, the questions of a
/// `ask_user_question` tool call. Like `permission`, it runs on a task
/// the turn owns.
async fn elicitation(
    acp: Arc<Acp>,
    state: Arc<State>,
    id: String,
    tools: Tools,
    rpc_id: RequestId,
    request: Elicitation,
) {
    // The request names the tool call it asks for; without one, it gets
    // a card of its own.
    let tool_call = extension(&request.meta, "toolCallId")
        .and_then(Value::as_str)
        .map_or_else(|| format!("request-{rpc_id}"), str::to_owned);
    let input = tools
        .lock()
        .unwrap()
        .get(&tool_call)
        .map(|t| t.input.clone())
        .unwrap_or(Value::Null);
    let decision = await_approval(
        &state,
        &id,
        &tool_call,
        "AskUserQuestion",
        &input,
        Vec::new(),
    )
    .await;
    let answer = match decision {
        Some(d) if d.allow => {
            let answers = d.answers.unwrap_or_default();
            // The schema's questions are keyed q0, q1, ...; their text is
            // each property's description, which the answers are keyed by.
            let content: serde_json::Map<_, _> = request
                .requested_schema
                .properties
                .iter()
                .filter_map(|(qid, prop)| {
                    let a = answers.get(prop.description.as_deref()?)?;
                    Some((qid.clone(), json!(a)))
                })
                .collect();
            json!({"action": "accept", "content": content})
        }
        _ => json!({"action": "decline"}),
    };
    acp.respond(&rpc_id, answer);
}

/// The JSON object in `reply`: all of it, else the last fenced block
/// that holds one, else the last object in the text. A fence only counts
/// at the start of a line, so one quoted inside a JSON string doesn't.
fn parse_reply(name: &str, reply: &str) -> Result<Value> {
    let object = |s: &str| {
        serde_json::from_str::<Value>(s.trim())
            .ok()
            .filter(Value::is_object)
    };
    object(reply)
        .or_else(|| fenced(reply).iter().rev().find_map(|b| object(b)))
        .or_else(|| last_object(reply))
        .ok_or_else(|| {
            anyhow!(
                "the {name} didn't reply with JSON: {}",
                crate::cleanup::cut(reply, 300)
            )
        })
}

/// The contents of the fenced blocks in `text`, in order; an unclosed one
/// runs to the end.
fn fenced(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut open: Option<String> = None;
    for line in text.lines() {
        match open.as_mut() {
            None if line.starts_with("```") => open = Some(String::new()),
            None => {}
            Some(_) if line.starts_with("```") => blocks.extend(open.take()),
            Some(block) => {
                block.push_str(line);
                block.push('\n');
            }
        }
    }
    blocks.extend(open);
    blocks
}

/// The last JSON object in `text` that starts at a `{`, braces in prose
/// and objects inside it aside.
fn last_object(text: &str) -> Option<Value> {
    let mut found = None;
    let mut i = 0;
    while let Some(at) = text[i..].find('{') {
        let start = i + at;
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        match stream.next() {
            Some(Ok(v)) if v.is_object() => {
                found = Some(v);
                i = start + stream.byte_offset();
            }
            _ => i = start + 1,
        }
    }
    found
}

/// Runs `job` on `model` as a `devin acp` session in `cwd`, in ask mode, and returns
/// its reply parsed as JSON; the schema is only in the prompt. The job's
/// session is deleted, on every way out, so it doesn't stay in the user's
/// Devin history.
async fn structured(devin: &Path, model: &str, job: &JobSpec<'_>, cwd: &Path) -> Result<Value> {
    let mut cmd = tokio::process::Command::new(devin);
    cmd.arg("acp").current_dir(cwd);
    let mut rpc = Rpc::spawn(cmd).with_context(|| format!("can't run {}", devin.display()))?;
    let (result, session) = run_job(&mut rpc, model, job, cwd).await;
    if let Some(session) = session {
        // A prompt may still be running, e.g. after a timeout, and Devin
        // might not delete a session that's busy. It ignores a cancel with
        // nothing running.
        if result.is_err() {
            rpc.acp
                .notify("session/cancel", json!({"sessionId": session}));
        }
        // The session only existed for the job; it doesn't belong in the
        // user's history. A hung delete doesn't keep the job.
        let delete = rpc.call("session/delete", json!({"sessionId": session}));
        let _ = tokio::time::timeout(DELETE_TIMEOUT, delete).await;
    }
    rpc.kill().await;
    result
}

/// What a job tracks while its prompt runs.
struct JobRun<'a> {
    job: &'a JobSpec<'a>,
    /// Devin's session id.
    session: String,
    /// The root's text, reset when it starts a tool call.
    text: String,
    /// What it has spent, in ACU.
    acu: f64,
    /// The commands of the root's exec calls, by tool call id.
    execs: HashMap<String, String>,
}

impl JobRun<'_> {
    /// Takes what devin sent while the prompt ran; an error once the job
    /// has gone over its budget.
    fn handle(&mut self, acp: &Arc<Acp>, m: Incoming) -> Result<()> {
        match m {
            Incoming::Notification(n) if n.method == "session/update" => {
                if let Ok(n) = serde_json::from_value::<SessionNotification>(n.params) {
                    self.update(n.update);
                }
            }
            Incoming::Request(r) => self.request(acp, r),
            Incoming::Notification(_) | Incoming::Response(_) => {}
        }
        if self.acu > self.job.budget.acu {
            acp.notify("session/cancel", json!({"sessionId": self.session}));
            bail!(
                "the {} went over its budget of {} ACU",
                self.job.kind.name(),
                self.job.budget.acu
            );
        }
        Ok(())
    }

    fn update(&mut self, u: SessionUpdate) {
        if u.subagent().is_some() {
            return;
        }
        match u {
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text { text },
                ..
            } => self.text += &text,
            SessionUpdate::ToolCall(call) => {
                // Whatever came before wasn't the final answer.
                self.text.clear();
                if extension(&call.meta, "inferenceToolName").and_then(Value::as_str)
                    == Some("exec")
                {
                    let command = call.raw_input["command"].as_str().unwrap_or_default();
                    self.execs.insert(call.tool_call_id, command.to_owned());
                }
            }
            SessionUpdate::UsageUpdate(usage) => {
                if let Some(total) = extension(&usage.meta, "totalAcuCost").and_then(Value::as_f64)
                {
                    self.acu = total;
                }
            }
            _ => {}
        }
    }

    /// Allows an exec of a read-only command if the job may run those,
    /// rejects every other permission request, and declines questions.
    fn request(&mut self, acp: &Arc<Acp>, r: Request) {
        match r.method.as_str() {
            "session/request_permission" => match parse::<PermissionRequest>(r.params) {
                Ok(p) => {
                    let call = &p.tool_call.tool_call_id;
                    let command = extension(&p.tool_call.meta, "editableCommand")
                        .and_then(Value::as_str)
                        .or(self.execs.get(call).map(String::as_str));
                    let kind = match command {
                        Some(c)
                            if self.job.tools == JobTools::ReadOnly
                                && self.execs.contains_key(call)
                                && crate::cleanup::allowed_command(c) =>
                        {
                            ApprovalKind::AllowOnce
                        }
                        _ => ApprovalKind::RejectOnce,
                    };
                    let option = first_option(&permission_options(&p), kind);
                    acp.respond(&r.id, permission_outcome(option));
                }
                Err(e) => acp.respond_error(&r.id, -32602, &format!("{e}")),
            },
            "elicitation/create" => acp.respond(&r.id, json!({"action": "decline"})),
            _ => acp.respond_error(&r.id, -32601, "unknown method"),
        }
    }
}

/// The job's outcome, beside the session id it got to.
async fn run_job(
    rpc: &mut Rpc,
    model: &str,
    job: &JobSpec<'_>,
    cwd: &Path,
) -> (Result<Value>, Option<String>) {
    let mut session = None;
    let result = tokio::time::timeout(job.timeout, async {
        rpc.call("initialize", initialize_params()).await?;
        let result = rpc
            .call("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await?;
        let created: NewSessionResponse = parse(result)?;
        session = Some(created.session_id.clone());
        let id = created.session_id;
        rpc.call(
            "session/set_config_option",
            json!({"sessionId": id, "configId": "model", "value": model}),
        )
        .await?;
        rpc.call(
            "session/set_mode",
            json!({"sessionId": id, "modeId": "ask"}),
        )
        .await?;
        let mut run = JobRun {
            job,
            session: id.clone(),
            text: String::new(),
            acu: 0.0,
            execs: HashMap::new(),
        };
        let prompt = format!(
            "{}\n\nReply with only a JSON object, no other text, that matches this JSON schema:\n{}",
            job.input,
            serde_json::to_string_pretty(job.schema).unwrap_or_default()
        );
        let result = rpc
            .call_with(
                "session/prompt",
                json!({"sessionId": id, "prompt": [{"type": "text", "text": prompt}]}),
                |acp, m| run.handle(acp, m),
            )
            .await?;
        match parse::<PromptResponse>(result)?.stop_reason.as_str() {
            "end_turn" => parse_reply(job.kind.name(), &run.text),
            r => bail!("the {} stopped: {r}", job.kind.name()),
        }
    })
    .await;
    let result = result.unwrap_or_else(|_| {
        Err(anyhow!(
            "the {} took longer than {}s",
            job.kind.name(),
            job.timeout.as_secs()
        ))
    });
    (result, session)
}

/// The fake `devin acp` in //grenadine/testing, set up in `dir` to play
/// `scenario` (see fake_devin.py); what the client sends goes to
/// `dir/devin_log`. Returns the executable to run as devin.
#[cfg(test)]
pub fn fake(dir: &Path, mut scenario: Value) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let fake = std::path::absolute(std::env::var("FAKE_DEVIN").unwrap()).unwrap();
    scenario["log"] = dir.join("devin_log").to_str().unwrap().into();
    let path = dir.join("scenario.json");
    std::fs::write(&path, scenario.to_string()).unwrap();
    let exe = dir.join("devin");
    std::fs::write(
        &exe,
        format!(
            "#!/bin/sh\nexec {} --scenario={} \"$@\"\n",
            fake.display(),
            path.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    exe
}

/// The messages the client sent the fake devin in `dir`, without a last
/// line it's still writing.
#[cfg(test)]
pub fn fake_log(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("devin_log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{AgentBackend, PermissionMode};

    use super::*;

    /// An update as `devin acp` writes it inside a session/update's
    /// `update` field.
    fn update(body: &str) -> SessionUpdate {
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
                    parent: None,
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

    /// Devin's messages sort into requests, responses and notifications
    /// by their fields; updates the transcript doesn't show are `Other`,
    /// and one without a field it needs doesn't parse.
    #[test]
    fn messages_are_typed() {
        let incoming = |line: &str| serde_json::from_str::<Incoming>(line).unwrap();
        assert!(matches!(
            incoming(r#"{"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{}}"#),
            Incoming::Request(Request { id: RequestId::String(id), .. }) if id == "perm-1"
        ));
        assert!(matches!(
            incoming(r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"no"}}"#),
            Incoming::Response(Response {
                id: RequestId::Number(3),
                error: Some(_),
                ..
            })
        ));
        // An error without a message still answers its request.
        assert!(matches!(
            incoming(r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32603}}"#),
            Incoming::Response(Response {
                id: RequestId::Number(4),
                error: Some(_),
                ..
            })
        ));
        assert!(matches!(
            incoming(r#"{"jsonrpc":"2.0","method":"_cognition.ai/status","params":{}}"#),
            Incoming::Notification(_)
        ));
        assert!(matches!(
            update(r#"{"sessionUpdate":"plan","entries":[]}"#),
            SessionUpdate::Other
        ));
        assert!(
            serde_json::from_str::<SessionUpdate>(r#"{"sessionUpdate":"tool_call","rawInput":{}}"#)
                .is_err()
        );
    }

    /// Config options are read leniently: only the model matters, its
    /// grouped options are found, and others of any shape don't break the
    /// answer.
    #[test]
    fn config_options_are_lenient() {
        let created: NewSessionResponse = parse(json!({
            "sessionId": "s",
            "configOptions": [
                {"id": "effort", "currentValue": 3, "options": [{"group": "g", "name": "G", "options": []}]},
                {"id": "model", "currentValue": "swe-2", "options": [
                    {"value": "swe-2", "name": "SWE-2"},
                    {"group": "more", "name": "More", "options": [
                        {"value": "swe-2-high", "name": "SWE-2 High"},
                    ]},
                    {"name": "no value"},
                ]},
            ],
        }))
        .unwrap();
        let values: Vec<_> = models(&created.config_options)
            .into_iter()
            .map(|m| m.value)
            .collect();
        assert_eq!(values, ["swe-2", "swe-2-high"]);
        assert_eq!(
            current_model(&created.config_options).as_deref(),
            Some("swe-2")
        );
        let loaded = parse::<Option<ConfigResponse>>(json!({"configOptions": null})).unwrap();
        assert_eq!(current_model(&loaded.unwrap().config_options), None);
    }

    /// A tool call update with `null` content and usage in floats still
    /// count.
    #[test]
    fn updates_are_lenient() {
        let events = events(&[
            r#"{"sessionUpdate":"tool_call","toolCallId":"exec:0#n","rawInput":{},"_meta":{"cognition.ai/inferenceToolName":"exec"}}"#,
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"exec:0#n","status":"completed","content":null}"#,
            r#"{"sessionUpdate":"usage_update","used":12.0,"size":200000.0}"#,
        ]);
        assert!(matches!(
            events[1],
            AgentEventKind::ToolResult { ref id, .. } if id == "exec:0#n"
        ));
        assert_eq!(
            events[2],
            AgentEventKind::Usage {
                tokens: Some(12),
                window: Some(200000),
            }
        );
    }

    /// Grenadine's MCP tools that Claude may use without asking go ahead
    /// under Devin's `mcp__grenadine__*` rule; `approve`, a tool that
    /// isn't grenadine's, and one that can't be told apart ask.
    #[test]
    fn auto_allows_grenadine_tools() {
        let rule = Some("mcp__grenadine__*");
        let allowed = |names: &[&str]| auto_allowed(rule, names.iter().copied());
        assert!(allowed(&["mcp__grenadine__read_terminal"]));
        assert!(allowed(&["read_terminal", "grenadine/read_terminal"]));
        assert!(!allowed(&["mcp__grenadine__approve"]));
        assert!(!allowed(&["mcp__grenadine__read_terminal", "approve"]));
        assert!(!allowed(&["mcp__grenadine__*"]));
        assert!(!allowed(&[]));
        assert!(!auto_allowed(Some("exec"), ["read_terminal"].into_iter()));
        assert!(auto_allowed(
            None,
            ["mcp__grenadine__show_image"].into_iter()
        ));
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
                parent: None,
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
                parent: None,
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
        let option = |id: &str, label: &str, kind| ApprovalOption {
            id: id.into(),
            label: label.into(),
            kind,
        };
        let options = [
            option(
                "plan_accept_edits",
                "Yes, implement plan and accept edits",
                ApprovalKind::AllowOnce,
            ),
            option(
                "plan_bypass",
                "Yes, implement plan and bypass",
                ApprovalKind::AllowOnce,
            ),
            option("reject_once", "No", ApprovalKind::RejectOnce),
        ];
        let plan = Tool {
            name: "ExitPlanMode".into(),
            title: None,
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
                    allow: false,
                    option: Some("reject_once".into()),
                    ..Default::default()
                },
                &plan,
                &options
            ),
            Some("reject_once".into())
        );
        // Without an allow_once, an allow picks another allow, and with no
        // options at all there's nothing to pick.
        let always = [
            option("reject_once", "No", ApprovalKind::RejectOnce),
            option("allow_always", "Always", ApprovalKind::AllowAlways),
        ];
        let allow = Decision {
            allow: true,
            ..Default::default()
        };
        let tool = Tool::default();
        assert_eq!(decide(&allow, &tool, &always), Some("allow_always".into()));
        assert_eq!(decide(&allow, &tool, &[]), None);
        // A denial with only allows offered picks none of them.
        let deny = Decision::default();
        let allows = [option("allow_once", "Yes", ApprovalKind::AllowOnce)];
        assert_eq!(decide(&deny, &tool, &allows), None);
    }

    /// The listing asks a throwaway session for its models and deletes
    /// the session again.
    #[tokio::test]
    async fn lists_models_and_deletes_the_asking_session() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake(
            dir.path(),
            json!({
                "session_id": "list-tulip",
                "models": [
                    {"value": "swe-2", "name": "SWE-2"},
                    {"value": "swe-2-high", "name": "SWE-2 High", "description": "Slower"},
                ],
            }),
        );
        let models = list_models(&exe).await.unwrap();
        assert_eq!(
            models
                .iter()
                .map(|m| (
                    m.value.as_str(),
                    m.display_name.as_str(),
                    m.description.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("swe-2", "SWE-2", ""),
                ("swe-2-high", "SWE-2 High", "Slower")
            ]
        );
        assert_eq!(
            fake_log(dir.path())
                .iter()
                .map(|m| m["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["initialize", "session/new", "session/delete"]
        );
        assert_eq!(
            fake_log(dir.path())[2]["params"],
            json!({"sessionId": "list-tulip"})
        );
    }

    /// A listing that fails says what devin printed.
    #[tokio::test]
    async fn a_failed_listing_has_the_stderr() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("devin");
        std::fs::write(&exe, "#!/bin/sh\necho 'Not logged in' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = list_models(&exe).await.unwrap_err();
        assert!(format!("{e:#}").contains("Not logged in"), "{e:#}");
    }

    /// A listing that runs out of time fails, without leaving its
    /// directory behind.
    #[tokio::test]
    async fn a_listing_that_hangs_fails() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake(
            dir.path(),
            json!({"gate": dir.path().join("never").to_str().unwrap()}),
        );
        // Long enough for the fake to start and reach the gate on a slow
        // machine; a Python start-up alone can take most of a second.
        let e = list_models_within(&exe, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("in time"), "{e:#}");
        let methods: Vec<_> = fake_log(dir.path())
            .iter()
            .map(|m| m["method"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(methods, ["initialize", "session/new"]);
        let cwd = std::fs::read_to_string(dir.path().join("devin_log")).unwrap();
        let cwd = serde_json::from_str::<Value>(cwd.lines().nth(1).unwrap()).unwrap();
        let cwd = cwd["params"]["cwd"].as_str().unwrap().to_owned();
        assert!(!Path::new(&cwd).exists(), "{cwd} is left");
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

#[cfg(test)]
mod job_tests {
    use super::*;

    /// A session/update step with a chunk of the answer.
    fn chunk(text: &str) -> Value {
        json!({"update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text},
        }})
    }

    /// The steps that start an exec of `command` as `call` and ask
    /// permission for it.
    fn exec_asking(call: &str, command: &str) -> [Value; 2] {
        [
            json!({"update": {
                "sessionUpdate": "tool_call",
                "toolCallId": call,
                "rawInput": {"command": command},
                "_meta": {"cognition.ai/inferenceToolName": "exec"},
            }}),
            json!({
                "request": "session/request_permission",
                "params": {
                    "sessionId": "job-s",
                    "toolCall": {"toolCallId": call},
                    "options": [
                        {"optionId": "allow_once", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
                    ],
                },
            }),
        ]
    }

    /// A fake devin whose prompts take `steps`.
    fn fake_job(dir: &Path, steps: Vec<Value>) -> PathBuf {
        fake(
            dir,
            json!({"session_id": "job-s", "prompts": [{"steps": steps}]}),
        )
    }

    fn job() -> JobSpec<'static> {
        static SCHEMA: std::sync::LazyLock<Value> =
            std::sync::LazyLock::new(|| json!({"type": "object"}));
        JobSpec {
            kind: JobKind::Scan,
            schema: &SCHEMA,
            input: "do it",
            tools: JobTools::ReadOnly,
            budget: crate::agent::Budget { usd: 1.0, acu: 1.0 },
            add_dir: None,
            clone: None,
            timeout: Duration::from_secs(20),
        }
    }

    /// The methods the client sent the fake devin in `dir`, in order.
    fn methods(dir: &Path) -> Vec<String> {
        fake_log(dir)
            .iter()
            .filter_map(|m| m["method"].as_str().map(str::to_owned))
            .collect()
    }

    /// A job: the session id rides on every call, the reply is parsed,
    /// and the session is deleted afterwards.
    #[tokio::test]
    async fn a_job_runs_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_job(
            dir.path(),
            vec![chunk(r#"{"ok": true}"#), json!({"end": "end_turn"})],
        );
        let out = structured(&exe, "swe-2-high", &job(), dir.path())
            .await
            .unwrap();
        assert_eq!(out, json!({"ok": true}));
        assert_eq!(
            methods(dir.path()),
            [
                "initialize",
                "session/new",
                "session/set_config_option",
                "session/set_mode",
                "session/prompt",
                "session/delete",
            ]
        );
        let log = fake_log(dir.path());
        for m in &log[2..] {
            assert_eq!(m["params"]["sessionId"], "job-s", "{m}");
        }
        assert_eq!(log[2]["params"]["value"], "swe-2-high");
        assert_eq!(log[3]["params"]["modeId"], "ask");
    }

    /// A request devin makes while the job sets up is declined rather
    /// than left to stall the job.
    #[tokio::test]
    async fn declines_requests_while_setting_up() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake(
            dir.path(),
            json!({
                "session_id": "job-s",
                "ask_on_new": "fs/read_text_file",
                "prompts": [{"steps": [chunk(r#"{"ok": true}"#), {"end": "end_turn"}]}],
            }),
        );
        let mut job = job();
        job.timeout = Duration::from_secs(5);
        let out = structured(&exe, "swe-2-high", &job, dir.path())
            .await
            .unwrap();
        assert_eq!(out, json!({"ok": true}));
        let reply = fake_log(dir.path())
            .into_iter()
            .find(|m| m["id"] == "new-1")
            .unwrap();
        assert_eq!(reply["error"]["code"], -32601, "{reply}");
    }

    /// Over budget: session/cancel goes out and the error says so.
    #[tokio::test]
    async fn over_budget_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_job(
            dir.path(),
            vec![
                json!({"update": {
                    "sessionUpdate": "usage_update",
                    "used": 1,
                    "size": 2,
                    "_meta": {"cognition.ai/totalAcuCost": 2.5},
                }}),
                json!({"end": "end_turn"}),
            ],
        );
        let e = structured(&exe, "swe-2-high", &job(), dir.path())
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("budget"), "{e:#}");
        let methods = methods(dir.path());
        assert!(
            methods.contains(&"session/cancel".to_owned()),
            "{methods:?}"
        );
        assert_eq!(methods.last().unwrap(), "session/delete");
    }

    /// An exec the allowlist passes gets allow_once, one it doesn't gets
    /// reject_once, and an elicitation is declined.
    #[tokio::test]
    async fn answers_requests() {
        let dir = tempfile::tempdir().unwrap();
        let mut steps = Vec::new();
        steps.extend(exec_asking("t1", "ls -la"));
        steps.extend(exec_asking("t2", "rm -rf /"));
        steps.push(json!({
            "request": "elicitation/create",
            "params": {"sessionId": "job-s", "message": "pick"},
        }));
        steps.push(chunk(r#"{"a": 1}"#));
        steps.push(json!({"end": "end_turn"}));
        let exe = fake_job(dir.path(), steps);
        let out = structured(&exe, "swe-2-high", &job(), dir.path())
            .await
            .unwrap();
        assert_eq!(out, json!({"a": 1}));
        let log = std::fs::read_to_string(dir.path().join("devin_log")).unwrap();
        assert!(log.contains("allow_once"), "{log}");
        assert!(log.contains("reject_once"), "{log}");
        assert!(log.contains("decline"), "{log}");
    }

    /// A devin that never answers the prompt: timeout, and the prompt is
    /// cancelled before the session is deleted.
    #[tokio::test]
    async fn times_out() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_job(dir.path(), Vec::new());
        let mut job = job();
        job.timeout = Duration::from_millis(500);
        let e = structured(&exe, "swe-2-high", &job, dir.path())
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("took longer"), "{e:#}");
        assert_eq!(
            methods(dir.path())[4..],
            ["session/prompt", "session/cancel", "session/delete"]
        );
    }

    /// A subagent's tool call becomes a ToolUse with its parent, and isn't
    /// remembered for results.
    #[test]
    fn a_subagents_call_names_its_parent() {
        let mut u = Updates::new();
        let events = u.update(&serde_json::from_str(
            r#"{"sessionUpdate":"tool_call","toolCallId":"exec:0#a6e9","rawInput":{"command":"ls"},"_meta":{"cognition.ai/inferenceToolName":"exec","cognition.ai/subagent_context":{"parentAgentId":"a6e974a3"}}}"#,
        ).unwrap());
        assert_eq!(
            events,
            [AgentEventKind::ToolUse {
                id: "exec:0#a6e9".into(),
                name: "Bash".into(),
                input: r#"{"command":"ls"}"#.into(),
                parent: Some("a6e974a3".into()),
            }]
        );
        assert!(u.update(&serde_json::from_str(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"exec:0#a6e9","status":"completed","content":[{"type":"content","content":{"type":"text","text":"x"}}],"_meta":{"cognition.ai/subagent_context":{"parentAgentId":"a6e974a3"}}}"#,
        ).unwrap()).is_empty());
    }

    /// A subagent's tool call while the root writes its answer doesn't
    /// cut the answer in two.
    #[test]
    fn a_subagents_call_keeps_the_roots_text_whole() {
        let mut u = Updates::new();
        let mut events = Vec::new();
        for body in [
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Half"}}"#,
            r#"{"sessionUpdate":"tool_call","toolCallId":"exec:0#s","rawInput":{"command":"ls"},"_meta":{"cognition.ai/inferenceToolName":"exec","cognition.ai/subagent_context":{"parentAgentId":"sub-1"}}}"#,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":" whole"}}"#,
        ] {
            events.extend(u.update(&serde_json::from_str(body).unwrap()));
        }
        events.extend(u.flush());
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEventKind::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Half whole"]);
    }

    /// The reply parser takes the whole reply, the last fenced block
    /// with an object, or the last object in prose, and fails on garbage.
    #[test]
    fn extracts_the_json_reply() {
        let parse = |reply: &str| parse_reply("t", reply).unwrap();
        // A fence quoted inside a string of the JSON isn't one.
        let quoted = r#"{"items":[{"title":"t","detail":"Run:\n```sh\ndocker rm x\n```"}]}"#;
        assert_eq!(
            parse(quoted),
            json!({"items": [{"title": "t", "detail": "Run:\n```sh\ndocker rm x\n```"}]})
        );
        assert_eq!(
            parse("Here you go.\n```json\n{\"a\": 1}\n```\nDone."),
            json!({"a": 1})
        );
        // The last fenced block with an object wins, whatever its info.
        assert_eq!(
            parse("```jsonc\n{\"a\": 1}\n```\nor\n```\n{\"a\": 2}\n```\n```sh\nls\n```"),
            json!({"a": 2})
        );
        // Fences that aren't at the start of a line don't count, and an
        // unclosed one runs to the end.
        assert_eq!(
            parse("Use ``` for code.\n```json\n{\"a\": 3}\n"),
            json!({"a": 3})
        );
        // Braces in prose before the object, and objects inside it.
        assert_eq!(
            parse("use {x} like {this}: {\"a\": {\"b\": 4}} cheers"),
            json!({"a": {"b": 4}})
        );
        assert_eq!(parse("first {\"a\": 1}, then {\"b\": 5}"), json!({"b": 5}));
        // An unclosed fence with prose after the object.
        assert_eq!(parse("```json\n{\"a\": 6}\nThat's all."), json!({"a": 6}));
        assert!(parse_reply("t", "no json here").is_err());
        assert!(parse_reply("t", "[1, 2]").is_err());
    }
}
