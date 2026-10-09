//! Claude Code sessions driven from the UI. Each session works in its own
//! git worktree; every prompt runs one `claude -p` process that resumes
//! the session, and its stream-json output becomes the session's
//! transcript. Claude asks for permission to use tools through the MCP
//! tool in `mcp.rs`, which waits for the user's answer from the page.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use grenadine_core::api::{
    AgentEvent, AgentEventKind, AgentSession, AgentStatus, NewAgent, PrKey, ServerEvent,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, oneshot};

use crate::sync::State;

/// The ref the trunk's tip is fetched into before a session branches off it.
const TRUNK_REF: &str = "refs/grenadine/agent/trunk";

/// How long Claude Code waits for an MCP tool call, in milliseconds. The
/// permission tool waits for a person, so make it a day.
const MCP_TOOL_TIMEOUT_MS: &str = "86400000";

/// How much of claude's stderr is kept to explain a crash.
const STDERR_TAIL: usize = 4096;

/// The user's answer to a permission request.
#[derive(Debug)]
pub struct Decision {
    pub allow: bool,
    pub message: Option<String>,
}

pub struct Agents {
    /// The `claude` executable.
    claude: PathBuf,
    /// Worktrees go to `<dir>/<owner>/<name>/<id>`.
    worktrees: PathBuf,
    /// Where Claude Code reaches this server, e.g. `http://127.0.0.1:8765`.
    base_url: String,
    live: std::sync::Mutex<HashMap<String, Live>>,
}

/// What only exists while the server runs: the event channel open pages
/// listen to, queued prompts and the pending permission requests.
struct Live {
    events: broadcast::Sender<AgentEvent>,
    /// Authenticates the session's MCP endpoint.
    secret: String,
    queue: VecDeque<String>,
    /// Whether a task is running the queued prompts.
    running: bool,
    /// The current claude process.
    pid: Option<u32>,
    /// The user stopped the current turn.
    interrupted: bool,
    /// Permission requests by tool use ID.
    pending: HashMap<String, oneshot::Sender<Decision>>,
}

impl Live {
    fn new() -> Live {
        Live {
            events: broadcast::channel(1024).0,
            secret: random_hex(16),
            queue: VecDeque::new(),
            running: false,
            pid: None,
            interrupted: false,
            pending: HashMap::new(),
        }
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use std::io::Read as _;
    let mut buf = [0; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("can't read /dev/urandom");
    buf
}

fn random_hex(n: usize) -> String {
    random_bytes::<32>()[..n]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A random (version 4) UUID, the format Claude Code's session IDs need.
fn uuid() -> String {
    let mut b = random_bytes::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// Whether `id` could be a session ID; keeps paths and URLs well-formed.
pub fn valid_id(id: &str) -> bool {
    id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// The session's title: the prompt's first line, shortened.
fn title(prompt: &str) -> String {
    let line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    match line.char_indices().nth(80) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_owned(),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Asks a process to stop the way Ctrl-C would.
fn interrupt_pid(pid: u32) {
    // SAFETY: kill has no memory-safety preconditions.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGINT);
    }
}

impl Agents {
    /// Marks the sessions that were running when the server last stopped as
    /// interrupted.
    pub fn new(
        db: &crate::db::Db,
        claude: PathBuf,
        worktrees: PathBuf,
        base_url: String,
    ) -> Result<Agents> {
        db.interrupt_agents()?;
        Ok(Agents {
            claude,
            // Claude Code runs in the worktrees, so a relative path would
            // resolve differently there.
            worktrees: std::path::absolute(&worktrees)?,
            base_url,
            live: std::sync::Mutex::default(),
        })
    }

    /// Runs `f` on the session's live state, creating it if needed.
    fn with_live<T>(&self, id: &str, f: impl FnOnce(&mut Live) -> T) -> T {
        let mut live = self.live.lock().unwrap();
        f(live.entry(id.to_owned()).or_insert_with(Live::new))
    }

    /// The secret the session's MCP endpoint expects.
    pub fn secret(&self, id: &str) -> String {
        self.with_live(id, |l| l.secret.clone())
    }

    pub fn subscribe(&self, id: &str) -> broadcast::Receiver<AgentEvent> {
        self.with_live(id, |l| l.events.subscribe())
    }
}

/// Stores an event (unless it's ephemeral) and sends it to open pages. The
/// live lock is held throughout so that pages see events in `seq` order.
pub fn emit(state: &State, id: &str, kind: AgentEventKind) -> Result<()> {
    let mut live = state.agents.live.lock().unwrap();
    let live = live.entry(id.to_owned()).or_insert_with(Live::new);
    let seq = if kind.is_ephemeral() {
        None
    } else {
        Some(state.db.append_agent_event(id, &kind)?)
    };
    let _ = live.events.send(AgentEvent { seq, kind });
    Ok(())
}

pub fn set_status(state: &State, id: &str, status: AgentStatus) {
    if let Err(e) = state.db.set_agent_status(id, status) {
        tracing::warn!("agent {id}: {e:#}");
    }
    state.send(ServerEvent::AgentsChanged);
}

/// Creates the session's worktree and starts working on its first prompt.
pub async fn create(state: &Arc<State>, req: NewAgent) -> Result<AgentSession> {
    if req.prompt.trim().is_empty() {
        bail!("the prompt is empty");
    }
    let cloned = state
        .repos
        .get(&req.repo)
        .ok_or_else(|| anyhow!("{} is not a configured repository", req.repo))?
        .clone();
    let pr_head = match req.pr {
        Some(number) => {
            let key = PrKey {
                repo: req.repo.clone(),
                number,
            };
            let pr = state
                .db
                .pr(&key)?
                .ok_or_else(|| anyhow!("{}#{number} hasn't synced yet", req.repo))?;
            let head = pr
                .versions
                .iter()
                .rev()
                .find(|v| !v.missing)
                .ok_or_else(|| anyhow!("{}#{number} has no commits", req.repo))?;
            Some(head.sha.clone())
        }
        None => None,
    };

    let id = uuid();
    let branch = format!("grenadine/agent/{}", &id[..8]);
    let worktree = state.agents.worktrees.join(&req.repo).join(&id);
    let base_sha = {
        let _guard = cloned.git_lock.lock().await;
        let repo = cloned.repo.clone();
        let (branch, worktree) = (branch.clone(), worktree.clone());
        tokio::task::spawn_blocking(move || -> Result<String> {
            let base = match pr_head {
                Some(sha) => sha,
                None => repo.fetch_default_branch(TRUNK_REF)?,
            };
            if let Some(parent) = worktree.parent() {
                std::fs::create_dir_all(parent)?;
            }
            repo.worktree_add(&worktree, &branch, &base)?;
            Ok(base)
        })
        .await??
    };

    let session = AgentSession {
        id: id.clone(),
        repo: req.repo,
        pr: req.pr,
        title: title(&req.prompt),
        branch,
        worktree: worktree.to_string_lossy().into_owned(),
        base_sha,
        status: AgentStatus::Idle,
        created_at: now(),
        cost_usd: 0.0,
    };
    state.db.create_agent(&session)?;
    state.send(ServerEvent::AgentsChanged);
    send(state, &id, req.prompt);
    Ok(session)
}

/// Queues a prompt; it runs once the turns before it are done.
pub fn send(state: &Arc<State>, id: &str, prompt: String) {
    let start = state.agents.with_live(id, |l| {
        l.queue.push_back(prompt);
        !std::mem::replace(&mut l.running, true)
    });
    if start {
        tokio::spawn(run(state.clone(), id.to_owned()));
    }
}

/// Stops the current turn and drops the queued prompts.
pub fn interrupt(state: &State, id: &str) {
    let pending = state.agents.with_live(id, |l| {
        l.queue.clear();
        l.interrupted = l.running;
        if let Some(pid) = l.pid {
            interrupt_pid(pid);
        }
        std::mem::take(&mut l.pending)
    });
    for (tool_use, tx) in pending {
        let _ = tx.send(Decision {
            allow: false,
            message: Some("The user interrupted the turn.".into()),
        });
        let _ = emit(
            state,
            id,
            AgentEventKind::ApprovalResolved {
                id: tool_use,
                allow: false,
            },
        );
    }
}

/// Registers a permission request; the receiver gets the user's answer.
pub fn request_approval(
    state: &State,
    id: &str,
    tool_use: &str,
    tool_name: &str,
    input: &Value,
) -> Result<oneshot::Receiver<Decision>> {
    let (tx, rx) = oneshot::channel();
    state.agents.with_live(id, |l| {
        l.pending.insert(tool_use.to_owned(), tx);
    });
    emit(
        state,
        id,
        AgentEventKind::ApprovalRequested {
            id: tool_use.to_owned(),
            tool_name: tool_name.to_owned(),
            input: input.to_string(),
        },
    )?;
    set_status(state, id, AgentStatus::AwaitingApproval);
    Ok(rx)
}

/// Forgets a permission request that Claude Code stopped waiting for.
pub fn drop_approval(state: &State, id: &str, tool_use: &str) {
    state.agents.with_live(id, |l| l.pending.remove(tool_use));
}

/// Passes the user's answer on to the waiting permission request. Returns
/// false when no such request is waiting.
pub fn approve(state: &State, id: &str, tool_use: &str, decision: Decision) -> Result<bool> {
    let (tx, still_pending) = state.agents.with_live(id, |l| {
        let tx = l.pending.remove(tool_use);
        (tx, !l.pending.is_empty())
    });
    let Some(tx) = tx else {
        return Ok(false);
    };
    let allow = decision.allow;
    if tx.send(decision).is_err() {
        return Ok(false);
    }
    emit(
        state,
        id,
        AgentEventKind::ApprovalResolved {
            id: tool_use.to_owned(),
            allow,
        },
    )?;
    if !still_pending {
        set_status(state, id, AgentStatus::Running);
    }
    Ok(true)
}

/// Stops the session and removes it along with its worktree and branch.
/// Returns false when there is no such session.
pub async fn delete(state: &Arc<State>, id: &str) -> Result<bool> {
    let Some(session) = state.db.agent(id)? else {
        return Ok(false);
    };
    interrupt(state, id);
    if let Some(cloned) = state.repos.get(&session.repo).cloned() {
        let _guard = cloned.git_lock.lock().await;
        let repo = cloned.repo.clone();
        let worktree = PathBuf::from(&session.worktree);
        tokio::task::spawn_blocking(move || {
            repo.worktree_remove(&worktree, &session.branch)
        })
        .await??;
    }
    state.db.delete_agent(id)?;
    state.agents.live.lock().unwrap().remove(id);
    state.send(ServerEvent::AgentsChanged);
    Ok(true)
}

/// Runs the queued prompts one turn at a time until the queue is empty.
async fn run(state: Arc<State>, id: String) {
    loop {
        let prompt = state.agents.with_live(&id, |l| {
            let prompt = l.queue.pop_front();
            l.running = prompt.is_some();
            l.interrupted = false;
            prompt
        });
        let Some(prompt) = prompt else {
            return;
        };
        let status = match turn(&state, &id, prompt).await {
            Ok(status) => status,
            Err(e) => {
                tracing::warn!("agent {id}: {e:#}");
                let _ = emit(&state, &id, AgentEventKind::Error(format!("{e:#}")));
                AgentStatus::Failed
            }
        };
        let interrupted = state.agents.with_live(&id, |l| {
            l.pid = None;
            l.interrupted
        });
        let status = if interrupted {
            AgentStatus::Interrupted
        } else {
            status
        };
        // A deleted session has no row to update.
        if state.db.agent(&id).ok().flatten().is_some() {
            set_status(&state, &id, status);
        }
    }
}

/// Runs one prompt and returns the status the session ends up in.
async fn turn(state: &Arc<State>, id: &str, prompt: String) -> Result<AgentStatus> {
    let session = state
        .db
        .agent(id)?
        .ok_or_else(|| anyhow!("the session is gone"))?;
    emit(state, id, AgentEventKind::Prompt(prompt.clone()))?;
    set_status(state, id, AgentStatus::Running);

    let mcp = json!({
        "mcpServers": {
            "grenadine": {
                "type": "http",
                "url": format!("{}/mcp/{id}/{}", state.agents.base_url, state.agents.secret(id)),
            }
        }
    });
    let mut cmd = tokio::process::Command::new(&state.agents.claude);
    cmd.arg("-p");
    if state.db.agent_started(id)? {
        cmd.args(["--resume", id]);
    } else {
        cmd.args(["--session-id", id]);
    }
    cmd.args([
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "mcp__grenadine__approve",
    ])
    .arg("--mcp-config")
    .arg(mcp.to_string())
    .current_dir(&session.worktree)
    .env("MCP_TOOL_TIMEOUT", MCP_TOOL_TIMEOUT_MS)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    // Keep claude out of the terminal's process group, like git in
    // `Repo::command`, so Ctrl-C on the server doesn't reach it directly.
    // SAFETY: setsid is async-signal-safe and the closure touches no
    // memory outside the call.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("can't run {}", state.agents.claude.display()))?;
    if let Some(pid) = child.id() {
        // An interrupt that came before the process existed applies now.
        if state.agents.with_live(id, |l| {
            l.pid = Some(pid);
            l.interrupted
        }) {
            interrupt_pid(pid);
        }
    }
    // The prompt goes through stdin so that one starting with `-` isn't
    // taken for a flag.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(prompt.as_bytes()).await?;
    drop(stdin);

    let mut stderr = child.stderr.take().unwrap();
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        let text = String::from_utf8_lossy(&buf).trim().to_owned();
        let start = text.len().saturating_sub(STDERR_TAIL);
        let start = (start..=text.len())
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        text[start..].to_owned()
    });

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut ended = None;
    let mut stopping = false;
    loop {
        let line = tokio::select! {
            line = lines.next_line() => line?,
            _ = state.shutdown.cancelled(), if !stopping => {
                stopping = true;
                if let Some(pid) = child.id() {
                    interrupt_pid(pid);
                }
                continue;
            }
        };
        let Some(line) = line else { break };
        match parse_line(&line) {
            Parsed::Started => state.db.set_agent_started(id)?,
            Parsed::Events(events) => {
                for kind in events {
                    if let AgentEventKind::TurnEnded { error, cost_usd } = &kind {
                        if let Some(cost) = cost_usd {
                            state.db.add_agent_cost(id, *cost)?;
                        }
                        ended = Some(error.is_none());
                    }
                    emit(state, id, kind)?;
                }
            }
        }
    }
    let exit = child.wait().await?;
    let stderr = stderr_task.await.unwrap_or_default();
    Ok(match ended {
        Some(true) => AgentStatus::Idle,
        Some(false) => AgentStatus::Failed,
        None if state.agents.with_live(id, |l| l.interrupted) => {
            emit(
                state,
                id,
                AgentEventKind::TurnEnded {
                    error: Some("interrupted".into()),
                    cost_usd: None,
                },
            )?;
            AgentStatus::Interrupted
        }
        None => {
            let mut message = format!("claude exited without finishing the turn ({exit})");
            if !stderr.is_empty() {
                message += &format!(":\n{stderr}");
            }
            emit(state, id, AgentEventKind::Error(message))?;
            AgentStatus::Failed
        }
    })
}

/// What one line of `--output-format stream-json` means for the transcript.
#[derive(Debug, PartialEq)]
enum Parsed {
    /// Claude Code created (or resumed) its session.
    Started,
    Events(Vec<AgentEventKind>),
}

/// The text of a tool result, whose content is a string or a list of blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().unwrap_or_default().to_owned(),
                Some(other) => format!("[{other}]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Picks the parts of a stream-json line that the transcript shows. Lines
/// of subagents (with a `parent_tool_use_id`) and unknown lines are
/// skipped, so a newer Claude Code degrades to showing less.
fn parse_line(line: &str) -> Parsed {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Parsed::Events(Vec::new());
    };
    if !v["parent_tool_use_id"].is_null() {
        return Parsed::Events(Vec::new());
    }
    let blocks = || v["message"]["content"].as_array().into_iter().flatten();
    let events = match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => return Parsed::Started,
        Some("stream_event") => {
            let delta = &v["event"]["delta"];
            match (v["event"]["type"].as_str(), delta["type"].as_str()) {
                (Some("content_block_delta"), Some("text_delta")) => delta["text"]
                    .as_str()
                    .map(|t| AgentEventKind::TextDelta(t.to_owned()))
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            }
        }
        Some("assistant") => blocks()
            .filter_map(|b| match b["type"].as_str()? {
                "text" => Some(AgentEventKind::Text(b["text"].as_str()?.to_owned())),
                "tool_use" => Some(AgentEventKind::ToolUse {
                    id: b["id"].as_str()?.to_owned(),
                    name: b["name"].as_str()?.to_owned(),
                    input: b["input"].to_string(),
                }),
                _ => None,
            })
            .collect(),
        Some("user") => blocks()
            .filter(|b| b["type"] == "tool_result")
            .map(|b| AgentEventKind::ToolResult {
                id: b["tool_use_id"].as_str().unwrap_or_default().to_owned(),
                content: result_text(&b["content"]),
                is_error: b["is_error"].as_bool().unwrap_or(false),
            })
            .collect(),
        Some("result") => {
            let error = v["is_error"].as_bool().unwrap_or(false).then(|| {
                v["result"]
                    .as_str()
                    .filter(|r| !r.is_empty())
                    .or(v["subtype"].as_str())
                    .unwrap_or("error")
                    .to_owned()
            });
            vec![AgentEventKind::TurnEnded {
                error,
                cost_usd: v["total_cost_usd"].as_f64(),
            }]
        }
        _ => Vec::new(),
    };
    Parsed::Events(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids() {
        let id = uuid();
        assert!(valid_id(&id), "{id}");
        assert_eq!(&id[14..15], "4");
        assert_ne!(id, uuid());
        assert!(!valid_id("../../etc"));
    }

    #[test]
    fn titles() {
        assert_eq!(title("\n  Fix the bug  \nin detail"), "Fix the bug");
        assert_eq!(title(&"é".repeat(100)), format!("{}…", "é".repeat(80)));
        assert_eq!(title(""), "");
    }

    fn events(line: &str) -> Vec<AgentEventKind> {
        match parse_line(line) {
            Parsed::Events(e) => e,
            Parsed::Started => panic!("{line} started a session"),
        }
    }

    /// Lines as Claude Code 2.1 writes them, shortened.
    #[test]
    fn parses_stream_json() {
        assert_eq!(
            parse_line(r#"{"type":"system","subtype":"init","cwd":"/w","session_id":"s","tools":[]}"#),
            Parsed::Started
        );
        assert_eq!(
            events(r#"{"type":"system","subtype":"hook_started","session_id":"s"}"#),
            []
        );
        assert_eq!(
            events(
                r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}},"parent_tool_use_id":null}"#
            ),
            [AgentEventKind::TextDelta("Hel".into())]
        );
        assert_eq!(
            events(
                r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}},"parent_tool_use_id":null}"#
            ),
            []
        );
        assert_eq!(
            events(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":""},{"type":"text","text":"Hello"},{"type":"tool_use","id":"toolu_1","name":"Write","input":{"file_path":"/w/b.txt","content":"hello\n"},"caller":{"type":"direct"}}]},"parent_tool_use_id":null,"session_id":"s"}"#
            ),
            [
                AgentEventKind::Text("Hello".into()),
                AgentEventKind::ToolUse {
                    id: "toolu_1".into(),
                    name: "Write".into(),
                    input: r#"{"content":"hello\n","file_path":"/w/b.txt"}"#.into(),
                },
            ]
        );
        assert_eq!(
            events(
                r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"No shell for you","is_error":true}]},"parent_tool_use_id":null}"#
            ),
            [AgentEventKind::ToolResult {
                id: "toolu_1".into(),
                content: "No shell for you".into(),
                is_error: true,
            }]
        );
        assert_eq!(
            events(
                r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_2","type":"tool_result","content":[{"type":"text","text":"a"},{"type":"image"}]}]},"parent_tool_use_id":null}"#
            ),
            [AgentEventKind::ToolResult {
                id: "toolu_2".into(),
                content: "a\n[image]".into(),
                is_error: false,
            }]
        );
        assert_eq!(
            events(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"parent_tool_use_id":null}"#
            ),
            []
        );
        assert_eq!(
            events(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"sub"}]},"parent_tool_use_id":"toolu_9"}"#
            ),
            []
        );
        assert_eq!(
            events(
                r#"{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.5}"#
            ),
            [AgentEventKind::TurnEnded {
                error: None,
                cost_usd: Some(0.5)
            }]
        );
        assert_eq!(
            events(r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#),
            [AgentEventKind::TurnEnded {
                error: Some("error_during_execution".into()),
                cost_usd: None
            }]
        );
        assert_eq!(events("not json"), []);
    }

    /// A state whose agents run `script` as claude, with the git fixture's
    /// clone configured.
    fn state_with_claude(
        fx: &crate::git::tests::Fixture,
        dir: &std::path::Path,
        script: &str,
    ) -> Arc<State> {
        use std::os::unix::fs::PermissionsExt as _;
        let claude = dir.join("claude");
        std::fs::write(&claude, script).unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        crate::sync::test_state_with_agents(
            [(fx.clone.slug.clone(), cloned)].into_iter().collect(),
            claude,
            dir.join("worktrees"),
        )
    }

    /// Waits until the session has finished `turns` turns and is no longer
    /// running.
    async fn wait_for_turns(state: &State, id: &str, turns: usize) -> Vec<AgentEventKind> {
        for _ in 0..100 {
            let events: Vec<_> = state
                .db
                .agent_events(id)
                .unwrap()
                .into_iter()
                .map(|e| e.kind)
                .collect();
            let ended = events
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        AgentEventKind::TurnEnded { .. } | AgentEventKind::Error(_)
                    )
                })
                .count();
            let running = state.agents.with_live(id, |l| l.running);
            if ended >= turns && !running {
                return events;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the session didn't finish {turns} turns");
    }

    #[tokio::test]
    async fn runs_turns_in_a_worktree_and_resumes() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let script = format!(
            r#"#!/bin/sh
prompt=$(cat)
echo "$* | $prompt | $(cat a)" >> {log}
echo '{{"type":"system","subtype":"init","session_id":"s"}}'
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"Hi"}}}},"parent_tool_use_id":null}}'
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Hi there"}}]}},"parent_tool_use_id":null}}'
echo '{{"type":"result","subtype":"success","is_error":false,"result":"Hi there","total_cost_usd":0.25}}'
"#,
            log = log.display()
        );
        let state = state_with_claude(&fx, dir.path(), &script);

        let session = create(
            &state,
            NewAgent {
                repo: "owner/name".into(),
                pr: None,
                prompt: "-first".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(session.title, "-first");
        assert_eq!(
            session.base_sha,
            crate::git::tests::run(&fx.upstream, &["rev-parse", "HEAD"])
        );
        let mut events = state.agents.subscribe(&session.id);
        wait_for_turns(&state, &session.id, 1).await;
        send(&state, &session.id, "second".into());
        let transcript = wait_for_turns(&state, &session.id, 2).await;

        let turn = |prompt: &str| {
            [
                AgentEventKind::Prompt(prompt.into()),
                AgentEventKind::Text("Hi there".into()),
                AgentEventKind::TurnEnded {
                    error: None,
                    cost_usd: Some(0.25),
                },
            ]
        };
        assert_eq!(transcript, [turn("-first"), turn("second")].concat());
        let session = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(session.status, AgentStatus::Idle);
        assert_eq!(session.cost_usd, 0.5);

        // The first turn creates the session, the second resumes it, both
        // in the worktree.
        let id = &session.id;
        let log = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert!(lines[0].starts_with(&format!("-p --session-id {id} ")), "{log}");
        assert!(lines[0].ends_with(" | -first | 1"), "{log}");
        assert!(lines[1].starts_with(&format!("-p --resume {id} ")), "{log}");
        assert!(lines[1].contains("--permission-prompt-tool mcp__grenadine__approve"));
        assert!(lines[1].ends_with(" | second | 1"), "{log}");

        // Pages get the stored events and the streamed text.
        let mut live = Vec::new();
        while let Ok(e) = events.try_recv() {
            live.push(e);
        }
        assert!(live.contains(&AgentEvent {
            seq: None,
            kind: AgentEventKind::TextDelta("Hi".into())
        }));
        // At least the second turn's, which started after subscribing.
        assert!(live.iter().filter(|e| e.seq.is_some()).count() >= 3);

        assert!(delete(&state, id).await.unwrap());
        assert!(!std::path::Path::new(&session.worktree).exists());
        assert_eq!(state.db.agent(id).unwrap(), None);
        assert!(!delete(&state, id).await.unwrap());
    }

    #[tokio::test]
    async fn reports_a_crash() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_claude(&fx, dir.path(), "#!/bin/sh\necho boom >&2\nexit 3\n");
        let session = create(
            &state,
            NewAgent {
                repo: "owner/name".into(),
                pr: None,
                prompt: "go".into(),
            },
        )
        .await
        .unwrap();
        let transcript = wait_for_turns(&state, &session.id, 1).await;
        let [AgentEventKind::Prompt(_), AgentEventKind::Error(e)] = &transcript[..] else {
            panic!("{transcript:?}");
        };
        assert!(e.contains("exit status: 3") && e.ends_with("boom"), "{e}");
        let session = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(session.status, AgentStatus::Failed);
        // Never started, so the next turn creates the session again.
        assert!(!state.db.agent_started(&session.id).unwrap());
    }

    #[tokio::test]
    async fn interrupts_a_turn() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        // Stands in for a turn that runs until it's interrupted.
        let state = state_with_claude(
            &fx,
            dir.path(),
            "#!/bin/sh\ntrap 'echo stopped >&2; exit 0' INT\ncat >/dev/null\nwhile :; do sleep 0.05; done\n",
        );
        let session = create(
            &state,
            NewAgent {
                repo: "owner/name".into(),
                pr: None,
                prompt: "go".into(),
            },
        )
        .await
        .unwrap();
        send(&state, &session.id, "queued".into());
        for _ in 0..100 {
            if state.agents.with_live(&session.id, |l| l.pid.is_some()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        interrupt(&state, &session.id);
        let transcript = wait_for_turns(&state, &session.id, 1).await;
        // The queued prompt never ran.
        assert_eq!(
            transcript
                .iter()
                .filter(|e| matches!(e, AgentEventKind::Prompt(_)))
                .count(),
            1
        );
        let session = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(session.status, AgentStatus::Interrupted);
    }
}
