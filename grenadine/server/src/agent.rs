//! Claude Code sessions driven from the UI. Each session works in its own
//! git worktree; every prompt runs one `claude -p` process that resumes
//! the session, and its stream-json output becomes the session's
//! transcript. Each prompt comes with a permission mode; Claude asks about
//! the tool uses that the mode doesn't decide on its own through the MCP
//! tool in `mcp.rs`, which waits for the user's answer from the page.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use grenadine_core::api::{
    AgentEvent, AgentEventKind, AgentModelInfo, AgentSession, AgentStatus, Landed, NewAgent,
    PatchHunk, PermissionMode, PrKey, ServerEvent, WrapUp, WrapUpTarget,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{OnceCell, broadcast, oneshot};

use crate::git::Repo;
use crate::sync::State;

/// The ref the trunk's tip is fetched into before a session branches off it.
const TRUNK_REF: &str = "refs/grenadine/agent/trunk";

/// How long Claude Code waits for an MCP tool call, in milliseconds. The
/// permission tool waits for a person, so make it the most Claude Code
/// allows (about 24 days). 0 doesn't mean "never" here; it becomes 1s.
const MCP_TOOL_TIMEOUT_MS: &str = "2147483647";

/// How long Claude Code lets an MCP tool call go without a response or
/// progress before it gives up, in milliseconds. It's 5 minutes for HTTP
/// servers by default, which would cut off a person who's away from the
/// page. 0 turns the check off.
const MCP_TOOL_IDLE_TIMEOUT_MS: &str = "0";

/// How much of claude's stderr is kept to explain a crash.
const STDERR_TAIL: usize = 4096;

/// How long a deleted session's processes get to exit before they're
/// killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// What Claude is asked when there's uncommitted work to wrap up.
const COMMIT_PROMPT: &str = "Wrap up: commit all of your changes with descriptive commit \
     messages that follow this repository's conventions. Don't change anything else.";

/// A wrap-up in progress.
#[derive(Clone, Debug)]
struct Pending {
    target: WrapUpTarget,
    /// The mode and model Claude's turns for the wrap-up go with.
    mode: PermissionMode,
    model: Option<String>,
    /// Claude was already asked to get the worktree ready once.
    asked: bool,
}

/// Where a wrap-up stands after a try at landing.
#[derive(Debug, PartialEq)]
enum Step {
    Landed(Landed),
    /// Claude has to fix the worktree first, as `prompt` tells it.
    Ask {
        reason: String,
        prompt: String,
    },
    /// The branch is on GitHub; the PR is opened next.
    Pushed {
        base: String,
        messages: Vec<String>,
    },
}

/// The user's answer to a permission request.
#[derive(Debug, Default)]
pub struct Decision {
    pub allow: bool,
    pub message: Option<String>,
    /// The mode an approved plan goes on in.
    pub mode: Option<PermissionMode>,
    /// The answers to `AskUserQuestion`, by question.
    pub answers: Option<BTreeMap<String, String>>,
}

pub struct Agents {
    /// The `claude` executable.
    claude: PathBuf,
    /// Worktrees go to `<dir>/<owner>/<name>/<id>`.
    worktrees: PathBuf,
    /// Where Claude Code reaches this server, e.g. `http://127.0.0.1:8765`.
    base_url: String,
    /// Whether each session's processes go into their own systemd user
    /// slice.
    slices: bool,
    live: std::sync::Mutex<HashMap<String, Live>>,
    /// The models Claude Code offers, once it has listed them.
    models: OnceCell<Vec<AgentModelInfo>>,
    /// The scans of deleted sessions for what they left behind.
    pub scans: crate::cleanup::Scans,
    /// Whether Haiku summarizes each new session's first prompt.
    pub summaries: std::sync::atomic::AtomicBool,
}

/// What only exists while the server runs: the event channel open pages
/// listen to, queued prompts and the pending permission requests.
struct Live {
    events: broadcast::Sender<AgentEvent>,
    /// Authenticates the session's MCP endpoint.
    secret: String,
    /// Prompts with their mode and model.
    queue: VecDeque<(String, PermissionMode, Option<String>)>,
    /// Whether a task is running the queued prompts.
    running: bool,
    /// The current claude process.
    pid: Option<u32>,
    /// The user stopped the current turn.
    interrupted: bool,
    /// Permission requests by tool use ID.
    pending: HashMap<String, oneshot::Sender<Decision>>,
    /// The wrap-up in progress, if any.
    wrap_up: Option<Pending>,
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
            wrap_up: None,
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

pub fn now() -> i64 {
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

/// Whether processes can be put into systemd user slices.
pub fn systemd_available() -> bool {
    std::process::Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The systemd user slice the session's processes run in. A cgroup holds on
/// to daemons that leave claude's process group, like Bazel's server, so
/// stopping the slice stops them too. Dashes nest slices, so the ID loses
/// them; every session's slice is then inside `grenadine-agent.slice`.
fn slice(id: &str) -> String {
    format!("grenadine-agent-{}.slice", id.replace('-', ""))
}

/// A process's state and controlling terminal, from `/proc/<pid>/stat`.
fn proc_stat(pid: u32) -> Option<(char, i64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name before them can contain anything, even `)`.
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace();
    let state = fields.next()?.chars().next()?;
    // ppid, pgrp, session, then tty_nr.
    let tty = fields.nth(3)?.parse().ok()?;
    Some((state, tty))
}

fn alive(pid: u32) -> bool {
    proc_stat(pid).is_some_and(|(state, _)| !matches!(state, 'Z' | 'X'))
}

/// Stops the processes working in `dir` that have no terminal, first
/// asking, then killing the ones that don't exit in time. A person's shell
/// there has a terminal and keeps going. `dir` has its symlinks resolved,
/// like the working directories. Returns the output bases of the Bazel
/// servers among them.
fn stop_processes_in(dir: &Path) -> Vec<PathBuf> {
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .filter(|&pid| {
            pid != me
                && proc_stat(pid).is_some_and(|(_, tty)| tty == 0)
                && std::fs::read_link(format!("/proc/{pid}/cwd"))
                    .is_ok_and(|cwd| cwd.starts_with(dir))
        })
        .collect();
    let output_bases = pids
        .iter()
        .filter_map(|pid| std::fs::read(format!("/proc/{pid}/cmdline")).ok())
        .flat_map(|cmdline| {
            cmdline
                .split(|&b| b == 0)
                .filter_map(|arg| arg.strip_prefix(b"--output_base="))
                .map(|base| PathBuf::from(std::ffi::OsStr::from_bytes(base)))
                .collect::<Vec<_>>()
        })
        .collect();
    let signal = |pids: &[u32], signal| {
        for &pid in pids {
            // SAFETY: kill has no memory-safety preconditions.
            unsafe {
                libc::kill(pid as libc::pid_t, signal);
            }
        }
    };
    signal(&pids, libc::SIGTERM);
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        pids.retain(|&pid| alive(pid));
        if pids.is_empty() || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    signal(&pids, libc::SIGKILL);
    output_bases
}

/// Where Bazel puts output bases unless told otherwise.
fn bazel_output_user_roots() -> Vec<PathBuf> {
    let Ok(user) = std::env::var("USER") else {
        return Vec::new();
    };
    let caches = [
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|c| !c.is_empty())
            .map(PathBuf::from),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")),
    ];
    let mut roots: Vec<_> = caches
        .into_iter()
        .flatten()
        .map(|c| c.join("bazel").join(format!("_bazel_{user}")))
        .collect();
    roots.dedup();
    roots
}

/// The output bases in `roots` whose workspace `belongs`. Bazel names the
/// workspace in each one's `DO_NOT_BUILD_HERE`.
fn output_bases(roots: &[PathBuf], belongs: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = roots
        .iter()
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flatten()
        .filter_map(|e| {
            let base = e.ok()?.path();
            let workspace = std::fs::read_to_string(base.join("DO_NOT_BUILD_HERE")).ok()?;
            belongs(Path::new(workspace.trim_end())).then_some(base)
        })
        .collect();
    bases.sort();
    bases.dedup();
    bases
}

/// Removes `dir` even though Bazel made parts of it read-only.
fn remove_tree(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fn make_writable(dir: &Path) {
        // Symlinks aren't followed, so nothing outside `dir` changes.
        let Ok(meta) = std::fs::symlink_metadata(dir) else {
            return;
        };
        if !meta.is_dir() {
            return;
        }
        let mode = meta.permissions().mode();
        if mode & 0o700 != 0o700 {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o700));
        }
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            make_writable(&e.path());
        }
    }
    if std::fs::remove_dir_all(dir).is_ok() {
        return Ok(());
    }
    make_writable(dir);
    std::fs::remove_dir_all(dir)
}

fn remove_output_bases(bases: Vec<PathBuf>) {
    for base in bases {
        match remove_tree(&base) {
            Ok(()) => tracing::info!("removed Bazel output base {}", base.display()),
            Err(e) => tracing::warn!("can't remove Bazel output base {}: {e}", base.display()),
        }
    }
}

/// Removes the Bazel output bases of worktrees in `worktrees` that are
/// gone, e.g. those of sessions deleted before their output bases were.
pub fn remove_orphaned_output_bases(worktrees: &Path) {
    let Ok(worktrees) = worktrees
        .canonicalize()
        .or_else(|_| std::path::absolute(worktrees))
    else {
        return;
    };
    remove_output_bases(output_bases(&bazel_output_user_roots(), |workspace| {
        workspace.starts_with(&worktrees) && !workspace.exists()
    }));
}

/// The branch a new session with `id` works on.
fn session_branch(id: &str) -> String {
    format!("grenadine/agent/{}", &id[..8])
}

/// Removes the worktrees in `worktrees` that no session owns, e.g. those
/// of sessions whose repo wasn't configured when they were deleted, with
/// their branches, then the directories that are left empty. Must run
/// before sessions can be created, which add their worktree first.
pub fn remove_orphaned_worktrees(
    db: &crate::db::Db,
    repos: &BTreeMap<String, Arc<crate::sync::ClonedRepo>>,
    worktrees: &Path,
) -> Result<()> {
    let Ok(root) = worktrees.canonicalize() else {
        return Ok(());
    };
    // Git keeps the path the worktree was added at, which needn't be the
    // canonical one.
    let added = std::path::absolute(worktrees)?;
    let owned: std::collections::BTreeSet<String> =
        db.agents()?.into_iter().map(|s| s.branch).collect();
    for (slug, cloned) in repos {
        let repo = &cloned.repo;
        let dirs = [root.join(slug), added.join(slug)];
        let listed = repo.worktrees()?;
        let mut checked_out: std::collections::BTreeSet<String> =
            listed.iter().filter_map(|(_, b)| b.clone()).collect();
        for (path, branch) in listed {
            let Some(id) = path.file_name().and_then(|id| id.to_str()).filter(|id| {
                valid_id(id) && path.parent().is_some_and(|p| dirs.iter().any(|d| d == p))
            }) else {
                continue;
            };
            if db.agent(id)?.is_some() {
                continue;
            }
            if let Err(e) = repo.worktree_remove_only(&path) {
                tracing::warn!("can't remove worktree {}: {e:#}", path.display());
                continue;
            }
            tracing::info!("removed orphaned worktree {}", path.display());
            if let Some(branch) = branch {
                checked_out.remove(&branch);
            }
            // Its agent may have switched it to another branch, or
            // detached it; the session's branch is the one to delete.
            let branch = session_branch(id);
            if owned.contains(&branch) || checked_out.contains(&branch) {
                continue;
            }
            match repo.branch_delete(&branch) {
                Ok(()) => tracing::info!("deleted orphaned branch {branch}"),
                Err(e) => tracing::warn!("can't delete branch {branch}: {e:#}"),
            }
        }
    }
    // Only the `owner/name` levels; the worktrees themselves are gone.
    for owner in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        for name in std::fs::read_dir(owner.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let _ = std::fs::remove_dir(name.path());
        }
        let _ = std::fs::remove_dir(owner.path());
    }
    Ok(())
}

impl Agents {
    /// Marks the sessions that were running when the server last stopped as
    /// interrupted. With `slices`, each session's processes go into their
    /// own systemd user slice. With `scans`, deleting a session scans its
    /// transcript for what it left behind. With `summaries`, Haiku
    /// summarizes each new session's first prompt.
    pub fn new(
        db: &crate::db::Db,
        claude: PathBuf,
        worktrees: PathBuf,
        base_url: String,
        slices: bool,
        scans: bool,
        summaries: bool,
    ) -> Result<Agents> {
        db.interrupt_agents()?;
        Ok(Agents {
            claude,
            // Claude Code runs in the worktrees, so a relative path would
            // resolve differently there.
            worktrees: std::path::absolute(&worktrees)?,
            base_url,
            slices,
            live: std::sync::Mutex::default(),
            models: OnceCell::new(),
            scans: crate::cleanup::Scans::new(scans),
            summaries: summaries.into(),
        })
    }

    /// The `claude` executable.
    pub fn claude(&self) -> &Path {
        &self.claude
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

    /// The models Claude Code offers, listed once and kept; none while it
    /// can't list them.
    pub async fn models(&self) -> Vec<AgentModelInfo> {
        match self
            .models
            .get_or_try_init(|| list_models(&self.claude))
            .await
        {
            Ok(models) => models.clone(),
            Err(e) => {
                tracing::warn!("listing Claude Code's models: {e:#}");
                Vec::new()
            }
        }
    }

    /// Whether prompts may ask for `model`: Claude Code's default, or one
    /// it lists.
    pub async fn offers(&self, model: Option<&str>) -> bool {
        match model {
            None => true,
            Some(m) => self.models().await.iter().any(|i| i.value == m),
        }
    }
}

/// The `initialize` control request the Agent SDK starts with. Claude Code
/// answers it without calling a model.
const INITIALIZE: &str =
    r#"{"type":"control_request","request_id":"models","request":{"subtype":"initialize"}}"#;

/// Asks Claude Code for its models the way the Agent SDK does.
async fn list_models(claude: &Path) -> Result<Vec<AgentModelInfo>> {
    let mut child = tokio::process::Command::new(claude)
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {}", claude.display()))?;
    // Stdin stays open until the answer is in, so claude waits for more.
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(format!("{INITIALIZE}\n").as_bytes())
        .await?;
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let read = async {
        while let Some(line) = lines.next_line().await? {
            if let Some(models) = parse_models(&line) {
                return Ok(models);
            }
        }
        bail!("claude exited without listing its models")
    };
    let models = tokio::time::timeout(std::time::Duration::from_secs(30), read)
        .await
        .context("claude didn't list its models in time")?;
    drop(stdin);
    let _ = child.kill().await;
    models
}

/// The models in the answer to `INITIALIZE`, if `line` is that answer.
/// Entries that don't parse are skipped.
fn parse_models(line: &str) -> Option<Vec<AgentModelInfo>> {
    let v = serde_json::from_str::<Value>(line).ok()?;
    let response = &v["response"];
    if v["type"] != "control_response" || response["request_id"] != "models" {
        return None;
    }
    Some(
        response["response"]["models"]
            .as_array()?
            .iter()
            .filter_map(|m| serde_json::from_value(m.clone()).ok())
            .collect(),
    )
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

/// Stores how full the context window is and sends it to open pages.
fn record_usage(state: &State, id: &str, tokens: Option<u64>, window: Option<u64>) -> Result<()> {
    state.db.set_agent_usage(id, tokens, window)?;
    emit(state, id, AgentEventKind::Usage { tokens, window })
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
    let branch = session_branch(&id);
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
        summary: None,
        branch,
        worktree: worktree.to_string_lossy().into_owned(),
        base_sha,
        status: AgentStatus::Idle,
        created_at: now(),
        cost_usd: 0.0,
        context_tokens: None,
        context_window: None,
        model: None,
        landed: None,
        prs: None,
        unstaged: 0,
    };
    state.db.create_agent(&session)?;
    state.send(ServerEvent::AgentsChanged);
    crate::summary::start(state, &id, &req.prompt);
    send(state, &id, req.prompt, req.permission_mode, req.model);
    Ok(session)
}

/// Queues a prompt; it runs once the turns before it are done, with
/// `model` or else Claude Code's default.
pub fn send(
    state: &Arc<State>,
    id: &str,
    prompt: String,
    mode: PermissionMode,
    model: Option<String>,
) {
    let start = state.agents.with_live(id, |l| {
        l.queue.push_back((prompt, mode, model));
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
            ..Decision::default()
        });
        let _ = emit(
            state,
            id,
            AgentEventKind::ApprovalResolved {
                id: tool_use,
                allow: false,
                message: None,
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
    let message = if allow {
        None
    } else {
        decision.message.clone()
    };
    if tx.send(decision).is_err() {
        return Ok(false);
    }
    emit(
        state,
        id,
        AgentEventKind::ApprovalResolved {
            id: tool_use.to_owned(),
            allow,
            message,
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
    // Without its clone the worktree and branch would stay behind with
    // nothing left to find them by.
    let cloned = state.repos.get(&session.repo).cloned();
    if cloned.is_none() && Path::new(&session.worktree).exists() {
        bail!(
            "{repo} isn't configured; restart the server with --repo for {repo} to delete this session",
            repo = session.repo
        );
    }
    // The transcript goes with the session; the scan reads it after.
    let events = state.db.agent_events(id)?;
    let scanned = session.clone();
    interrupt(state, id);
    // Whatever claude started can outlive it, e.g. Bazel's server, which
    // would hold on to its memory for hours.
    if state.agents.slices {
        let _ = tokio::process::Command::new("systemctl")
            .args(["--user", "stop", "--quiet"])
            .arg(slice(id))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let worktree = PathBuf::from(&session.worktree);
    let worktree = worktree.canonicalize().unwrap_or(worktree);
    let stopped = {
        let worktree = worktree.clone();
        tokio::task::spawn_blocking(move || stop_processes_in(&worktree)).await?
    };
    if let Some(cloned) = cloned {
        let _guard = cloned.git_lock.lock().await;
        let repo = cloned.repo.clone();
        let worktree = PathBuf::from(&session.worktree);
        tokio::task::spawn_blocking(move || repo.worktree_remove(&worktree, &session.branch))
            .await??;
    }
    // Bazel keeps each workspace's build in an output base outside the
    // worktree, and workspaces can be anywhere in it. The stopped servers
    // name their output bases, which matters when they aren't in the
    // default place. Removing several GB takes a while, so it goes on in
    // a thread, which unlike spawn_blocking doesn't hold up shutting down.
    let mut roots = bazel_output_user_roots();
    roots.extend(
        stopped
            .iter()
            .filter_map(|b| b.parent().map(Path::to_owned)),
    );
    std::thread::spawn(move || {
        remove_output_bases(output_bases(&roots, |workspace| {
            workspace.starts_with(&worktree)
        }))
    });
    state.db.delete_agent(id)?;
    state.agents.live.lock().unwrap().remove(id);
    state.send(ServerEvent::AgentsChanged);
    crate::cleanup::scan(state, scanned, events);
    Ok(true)
}

/// Runs the queued prompts one turn at a time until the queue is empty,
/// then goes on with a wrap-up that waited for them.
async fn run(state: Arc<State>, id: String) {
    let mut last = AgentStatus::Idle;
    loop {
        let prompt = state.agents.with_live(&id, |l| {
            let prompt = l.queue.pop_front();
            l.running = prompt.is_some();
            l.interrupted = false;
            prompt
        });
        let Some((prompt, mode, model)) = prompt else {
            if last == AgentStatus::Idle {
                continue_wrap_up(&state, &id).await;
            } else if state.agents.with_live(&id, |l| l.wrap_up.take()).is_some() {
                let _ = emit(
                    &state,
                    &id,
                    AgentEventKind::WrapUpFailed("Claude's turn didn't finish".into()),
                );
            }
            return;
        };
        let status = match turn(&state, &id, prompt, mode, model).await {
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
        last = status;
    }
}

/// Starts landing the session's work. Returns false when the session is
/// busy with a turn or another wrap-up.
pub async fn wrap_up(state: &Arc<State>, id: &str, req: WrapUp) -> bool {
    let start = state.agents.with_live(id, |l| {
        if l.running || l.wrap_up.is_some() {
            return false;
        }
        l.wrap_up = Some(Pending {
            target: req.target,
            mode: req.permission_mode,
            model: req.model,
            asked: false,
        });
        true
    });
    if start {
        continue_wrap_up(state, id).await;
    }
    start
}

/// Tries to land the work of the session's wrap-up, if one is in
/// progress. When the worktree isn't ready, Claude is asked to fix it once
/// and the wrap-up goes on after that turn.
async fn continue_wrap_up(state: &Arc<State>, id: &str) {
    let Some(pending) = state.agents.with_live(id, |l| l.wrap_up.clone()) else {
        return;
    };
    let outcome = match land(state, id, pending.target).await {
        Ok(Step::Landed(landed)) => record_landed(state, id, &landed).map(|()| landed),
        Ok(Step::Ask { prompt, .. }) if !pending.asked => {
            state.agents.with_live(id, |l| {
                if let Some(w) = &mut l.wrap_up {
                    w.asked = true;
                }
            });
            send(state, id, prompt, pending.mode, pending.model);
            return;
        }
        Ok(Step::Ask { reason, .. }) => Err(anyhow!("{reason}")),
        Ok(Step::Pushed { .. }) => Err(anyhow!("the PR wasn't opened")),
        Err(e) => Err(e),
    };
    state.agents.with_live(id, |l| l.wrap_up = None);
    let kind = match outcome {
        Ok(landed) => AgentEventKind::Landed(landed),
        Err(e) => {
            tracing::warn!("agent {id}: wrapping up: {e:#}");
            AgentEventKind::WrapUpFailed(format!("{e:#}"))
        }
    };
    let _ = emit(state, id, kind);
}

fn record_landed(state: &State, id: &str, landed: &Landed) -> Result<()> {
    state.db.set_agent_landed(id, landed)?;
    // Wrapping up again lands only what came after.
    match landed {
        Landed::Checkout { sha } => state.db.set_agent_base(id, sha)?,
        Landed::PullRequest { number, .. } => {
            if let Some(session) = state.db.agent(id)? {
                let key = PrKey {
                    repo: session.repo,
                    number: *number,
                };
                state.db.add_agent_pr(id, &key, "OPEN")?;
            }
        }
    }
    state.send(ServerEvent::AgentsChanged);
    Ok(())
}

/// One try at landing the session's work on `target`.
async fn land(state: &Arc<State>, id: &str, target: WrapUpTarget) -> Result<Step> {
    let session = state
        .db
        .agent(id)?
        .ok_or_else(|| anyhow!("the session is gone"))?;
    let cloned = state
        .repos
        .get(&session.repo)
        .ok_or_else(|| anyhow!("{} is not a configured repository", session.repo))?
        .clone();
    // A session that started from a PR proposes its work to that PR's
    // branch.
    let pr_branch = match (target, session.pr) {
        (WrapUpTarget::PullRequest, Some(number)) => {
            let key = PrKey {
                repo: session.repo.clone(),
                number,
            };
            let pr = state
                .db
                .pr(&key)?
                .ok_or_else(|| anyhow!("{}#{number} isn't synced", session.repo))?;
            Some(pr.head_ref)
        }
        _ => None,
    };
    let step = {
        let _guard = cloned.git_lock.lock().await;
        let repo = cloned.repo.clone();
        let session = session.clone();
        tokio::task::spawn_blocking(move || match target {
            WrapUpTarget::Checkout => land_on_checkout(&repo, &session),
            WrapUpTarget::PullRequest => push_branch(&repo, &session, pr_branch),
        })
        .await??
    };
    let Step::Pushed { base, messages } = step else {
        return Ok(step);
    };
    // The push updated the PR that an earlier wrap-up opened.
    if let Some(landed @ Landed::PullRequest { .. }) = session.landed {
        return Ok(Step::Landed(landed));
    }
    let (title, body) = pr_text(&session.title, &messages);
    let (number, url) = state
        .github
        .create_pr(&session.repo, &session.branch, &base, &title, &body)
        .await?;
    Ok(Step::Landed(Landed::PullRequest { number, url }))
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// Rebases the worktree's commits onto the clone's checkout and
/// fast-forwards the checkout to them.
fn land_on_checkout(repo: &Repo, session: &AgentSession) -> Result<Step> {
    let worktree = Path::new(&session.worktree);
    if let Some(reason) = repo.worktree_unfinished(worktree)? {
        return Ok(Step::Ask {
            reason,
            prompt: COMMIT_PROMPT.into(),
        });
    }
    let base = &session.base_sha;
    // A checkout that moves in the meantime gets a second try.
    for _ in 0..2 {
        let head = repo.worktree_head(worktree)?;
        if head == *base {
            bail!("there are no commits to land");
        }
        let onto = repo.head()?;
        if !repo.is_ancestor(&onto, &head) {
            // Only the session's own commits move: those after its base,
            // or after the checkout's history if it was merged in already.
            let upstream = match repo.merge_base(&onto, &head) {
                Some(mb) if repo.is_ancestor(base, &mb) => mb,
                _ => base.clone(),
            };
            if !repo.worktree_rebase(worktree, &onto, &upstream)? {
                return Ok(Step::Ask {
                    reason: format!("the commits conflict with the checkout at {}", short(&onto)),
                    prompt: format!(
                        "Wrap up: rebase your commits onto {onto} with `git rebase --onto {onto} \
                         {upstream}`, resolve the conflicts, and check that the result still \
                         builds and passes the relevant tests. Finish the rebase and leave \
                         nothing uncommitted."
                    ),
                });
            }
        }
        let head = repo.worktree_head(worktree)?;
        match repo.merge_ff_only(&head) {
            Ok(()) => return Ok(Step::Landed(Landed::Checkout { sha: head })),
            Err(_) if repo.head()? != onto => continue,
            Err(e) => return Err(e),
        }
    }
    bail!("the checkout kept moving")
}

/// Pushes the session's branch to GitHub. The PR goes into `pr_branch`, or
/// the default branch.
fn push_branch(repo: &Repo, session: &AgentSession, pr_branch: Option<String>) -> Result<Step> {
    let worktree = Path::new(&session.worktree);
    if let Some(reason) = repo.worktree_unfinished(worktree)? {
        return Ok(Step::Ask {
            reason,
            prompt: COMMIT_PROMPT.into(),
        });
    }
    let head = repo.worktree_head(worktree)?;
    let messages = repo.messages(&session.base_sha, &head)?;
    if messages.is_empty() {
        bail!("there are no commits to push");
    }
    repo.push_branch(&session.branch)?;
    let base = match pr_branch {
        Some(b) => b,
        None => repo.default_branch()?,
    };
    Ok(Step::Pushed { base, messages })
}

/// A PR's title and body for commits with `messages`: the commit's own for
/// one commit, the session's title and the commits' subjects for more.
fn pr_text(title: &str, messages: &[String]) -> (String, String) {
    if let [message] = messages {
        let (subject, body) = message.split_once('\n').unwrap_or((message, ""));
        return (subject.trim().to_owned(), body.trim().to_owned());
    }
    let subjects = messages
        .iter()
        .map(|m| format!("- {}", m.lines().next().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("\n");
    (title.to_owned(), subjects)
}

/// Runs one prompt and returns the status the session ends up in.
async fn turn(
    state: &Arc<State>,
    id: &str,
    prompt: String,
    mode: PermissionMode,
    model: Option<String>,
) -> Result<AgentStatus> {
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
    // `systemd-run --scope` execs claude, so the PID stays claude's.
    let mut cmd = if state.agents.slices {
        let mut cmd = tokio::process::Command::new("systemd-run");
        cmd.args(["--user", "--scope", "--quiet", "--collect"])
            .arg(format!("--slice={}", slice(id)))
            .arg("--")
            .arg(&state.agents.claude);
        cmd
    } else {
        tokio::process::Command::new(&state.agents.claude)
    };
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
        "--permission-mode",
        mode.as_str(),
        "--permission-prompt-tool",
        "mcp__grenadine__approve",
    ])
    .arg("--mcp-config")
    .arg(mcp.to_string())
    .current_dir(&session.worktree)
    .env("MCP_TOOL_TIMEOUT", MCP_TOOL_TIMEOUT_MS)
    .env(
        "CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT",
        MCP_TOOL_IDLE_TIMEOUT_MS,
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    if let Some(model) = model.filter(|m| m != "default") {
        cmd.arg("--model").arg(model);
    }
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
    // taken for a flag. If claude exits without reading it, its exit status
    // and stderr below say why.
    let mut stdin = child.stdin.take().unwrap();
    if let Err(e) = stdin.write_all(prompt.as_bytes()).await
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(e.into());
    }
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
    let mut prs = OpenedPrs::default();
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
            Parsed::Started { window, model } => {
                state.db.set_agent_started(id)?;
                // A resumed session keeps the window its result reported,
                // unless it switched models.
                let before = state.db.agent(id)?;
                if before.is_some_and(|s| s.context_window.is_none() || s.model != model) {
                    record_usage(state, id, None, Some(window))?;
                }
                if let Some(model) = model {
                    state.db.set_agent_model(id, &model)?;
                    emit(state, id, AgentEventKind::Model(model))?;
                }
            }
            Parsed::Events(events) => {
                for kind in events {
                    if let AgentEventKind::Usage { tokens, window } = kind {
                        record_usage(state, id, tokens, window)?;
                        continue;
                    }
                    if let AgentEventKind::TurnEnded { error, cost_usd } = &kind {
                        if let Some(cost) = cost_usd {
                            state.db.add_agent_cost(id, *cost)?;
                        }
                        ended = Some(error.is_none());
                    }
                    let opened = prs.see(&session.repo, &kind);
                    emit(state, id, kind)?;
                    record_prs(state, id, &opened)?;
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

/// The PRs that GitHub URLs in `text` point to.
fn pr_urls(text: &str) -> Vec<PrKey> {
    text.match_indices("https://github.com/")
        .filter_map(|(i, prefix)| {
            let mut parts = text[i + prefix.len()..].splitn(4, '/');
            let (owner, name) = (parts.next()?, parts.next()?);
            if parts.next()? != "pull" || owner.is_empty() || name.is_empty() {
                return None;
            }
            let rest = parts.next()?;
            let digits = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            Some(PrKey {
                repo: format!("{owner}/{name}"),
                number: rest[..digits].parse().ok()?,
            })
        })
        .collect()
}

/// Picks out the PRs a session opens from its transcript's events: those
/// that wrapping up opened, and those whose URL a `gh pr create` in Bash
/// printed.
#[derive(Default)]
pub struct OpenedPrs {
    /// The tool uses that run `gh pr create`, until their results come.
    creating: std::collections::HashSet<String>,
}

impl OpenedPrs {
    /// The PRs that `kind` shows were opened in a session in `repo`.
    pub fn see(&mut self, repo: &str, kind: &AgentEventKind) -> Vec<PrKey> {
        match kind {
            AgentEventKind::ToolUse { id, name, input } if name == "Bash" => {
                let command = serde_json::from_str::<Value>(input)
                    .ok()
                    .and_then(|v| v["command"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                if command.contains("gh pr create") {
                    self.creating.insert(id.clone());
                }
                Vec::new()
            }
            AgentEventKind::ToolResult {
                id,
                content,
                is_error,
                ..
            } if self.creating.remove(id) && !is_error => {
                let mut prs = pr_urls(content);
                prs.dedup();
                prs
            }
            AgentEventKind::Landed(Landed::PullRequest { number, .. }) => vec![PrKey {
                repo: repo.to_owned(),
                number: *number,
            }],
            _ => Vec::new(),
        }
    }
}

/// Records that the session opened `prs`, which are open.
fn record_prs(state: &State, id: &str, prs: &[PrKey]) -> Result<()> {
    if prs.is_empty() {
        return Ok(());
    }
    for key in prs {
        state.db.add_agent_pr(id, key, "OPEN")?;
    }
    state.send(ServerEvent::AgentsChanged);
    Ok(())
}

/// What one line of `--output-format stream-json` means for the transcript.
#[derive(Debug, PartialEq)]
enum Parsed {
    /// Claude Code created (or resumed) its session with `model`; `window`
    /// is the context window that the model likely has.
    Started {
        window: u64,
        model: Option<String>,
    },
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

/// The hunks of a file tool's `structuredPatch`, or none when the result
/// has no patch.
fn patch(result: &Value) -> Vec<PatchHunk> {
    result["structuredPatch"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| {
            Some(PatchHunk {
                old_start: h["oldStart"].as_u64()? as usize,
                new_start: h["newStart"].as_u64()? as usize,
                lines: h["lines"]
                    .as_array()?
                    .iter()
                    .filter_map(|l| Some(l.as_str()?.to_owned()))
                    .collect(),
            })
        })
        .collect()
}

/// The context window of a model by its name, until a turn's result tells.
fn guess_window(model: &str) -> u64 {
    if model.ends_with("[1m]") {
        1_000_000
    } else {
        200_000
    }
}

/// How many tokens the context holds after an assistant message.
fn context_tokens(usage: &Value) -> Option<u64> {
    if !usage.is_object() {
        return None;
    }
    let fields = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ];
    Some(fields.iter().filter_map(|f| usage[f].as_u64()).sum())
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
        Some("system") if v["subtype"] == "init" => {
            let model = v["model"].as_str().unwrap_or_default();
            return Parsed::Started {
                window: guess_window(model),
                model: (!model.is_empty()).then(|| model.to_owned()),
            };
        }
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
            .chain(
                context_tokens(&v["message"]["usage"]).map(|tokens| AgentEventKind::Usage {
                    tokens: Some(tokens),
                    window: None,
                }),
            )
            .collect(),
        Some("user") => blocks()
            .filter(|b| b["type"] == "tool_result")
            .map(|b| AgentEventKind::ToolResult {
                id: b["tool_use_id"].as_str().unwrap_or_default().to_owned(),
                content: result_text(&b["content"]),
                is_error: b["is_error"].as_bool().unwrap_or(false),
                // A message carries one tool result, and its patch is
                // beside it.
                patch: patch(&v["tool_use_result"]),
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
            // The main model has the largest window; subagents' may be
            // smaller.
            let window = v["modelUsage"]
                .as_object()
                .into_iter()
                .flat_map(|m| m.values())
                .filter_map(|m| m["contextWindow"].as_u64())
                .max();
            let usage = window.map(|w| AgentEventKind::Usage {
                tokens: None,
                window: Some(w),
            });
            usage
                .into_iter()
                .chain([AgentEventKind::TurnEnded {
                    error,
                    cost_usd: v["total_cost_usd"].as_f64(),
                }])
                .collect()
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

    /// The page sends permission modes as JSON; Claude Code gets them as
    /// arguments. Both use its names.
    #[test]
    fn permission_modes() {
        for mode in PermissionMode::ALL {
            assert_eq!(serde_json::to_value(mode).unwrap(), mode.as_str());
            assert_eq!(PermissionMode::parse(mode.as_str()), Some(mode));
        }
        let msg: grenadine_core::api::AgentMessage =
            serde_json::from_str(r#"{"prompt":"hi"}"#).unwrap();
        assert_eq!(msg.permission_mode, PermissionMode::Manual);
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
            Parsed::Started { .. } => panic!("{line} started a session"),
        }
    }

    /// Lines as Claude Code 2.1 writes them, shortened.
    #[test]
    fn parses_stream_json() {
        assert_eq!(
            parse_line(
                r#"{"type":"system","subtype":"init","cwd":"/w","session_id":"s","tools":[],"model":"claude-opus-4-6"}"#
            ),
            Parsed::Started {
                window: 200_000,
                model: Some("claude-opus-4-6".into())
            }
        );
        assert_eq!(
            parse_line(
                r#"{"type":"system","subtype":"init","session_id":"s","model":"claude-opus-4-6[1m]"}"#
            ),
            Parsed::Started {
                window: 1_000_000,
                model: Some("claude-opus-4-6[1m]".into())
            }
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
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":""},{"type":"text","text":"Hello"},{"type":"tool_use","id":"toolu_1","name":"Write","input":{"file_path":"/w/b.txt","content":"hello\n"},"caller":{"type":"direct"}}],"usage":{"input_tokens":3,"cache_creation_input_tokens":1000,"cache_read_input_tokens":40000,"output_tokens":500,"service_tier":"standard"}},"parent_tool_use_id":null,"session_id":"s"}"#
            ),
            [
                AgentEventKind::Text("Hello".into()),
                AgentEventKind::ToolUse {
                    id: "toolu_1".into(),
                    name: "Write".into(),
                    input: r#"{"content":"hello\n","file_path":"/w/b.txt"}"#.into(),
                },
                AgentEventKind::Usage {
                    tokens: Some(41_503),
                    window: None,
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
                patch: vec![],
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
                patch: vec![],
            }]
        );
        assert_eq!(
            events(
                r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_3","type":"tool_result","content":"Updated"}]},"parent_tool_use_id":null,"tool_use_result":{"filePath":"/w/a.rs","structuredPatch":[{"oldStart":4,"oldLines":2,"newStart":4,"newLines":2,"lines":[" a","-b","+c"]}]}}"#
            ),
            [AgentEventKind::ToolResult {
                id: "toolu_3".into(),
                content: "Updated".into(),
                is_error: false,
                patch: vec![PatchHunk {
                    old_start: 4,
                    new_start: 4,
                    lines: vec![" a".into(), "-b".into(), "+c".into()],
                }],
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
                r#"{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.5,"modelUsage":{"claude-haiku-4-5":{"contextWindow":200000},"claude-opus-4-6":{"inputTokens":3,"contextWindow":1000000}}}"#
            ),
            [
                AgentEventKind::Usage {
                    tokens: None,
                    window: Some(1_000_000),
                },
                AgentEventKind::TurnEnded {
                    error: None,
                    cost_usd: Some(0.5)
                },
            ]
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

    /// Claude Code lists its models in its answer to `initialize`, as
    /// 2.1 writes it, shortened.
    #[tokio::test]
    async fn lists_models() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let script = format!(
            r#"#!/bin/sh
read -r request
echo "$* | $request" >> {log}
echo '{{"type":"system","subtype":"hook_started"}}'
echo '{{"type":"control_response","response":{{"subtype":"success","request_id":"models","response":{{"commands":[],"models":[{{"value":"default","resolvedModel":"claude-opus-5-5","displayName":"Default (recommended)","description":"Opus 5.5"}},{{"value":"sonnet","displayName":"Sonnet 5.5","description":"Efficient","supportsEffort":true}},{{"displayName":"no value"}}]}}}}}}'
"#,
            log = log.display()
        );
        let state = state_with_claude(&fx, dir.path(), &script);
        let models = state.agents.models().await;
        assert_eq!(
            models,
            [
                AgentModelInfo {
                    value: "default".into(),
                    display_name: "Default (recommended)".into(),
                    description: "Opus 5.5".into(),
                },
                AgentModelInfo {
                    value: "sonnet".into(),
                    display_name: "Sonnet 5.5".into(),
                    description: "Efficient".into(),
                },
            ]
        );
        assert!(state.agents.offers(None).await);
        assert!(state.agents.offers(Some("sonnet")).await);
        assert!(!state.agents.offers(Some("--help")).await);

        // Claude Code is asked once, without a prompt or a session to keep.
        let log = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            log,
            format!(
                "-p --input-format stream-json --output-format stream-json --verbose \
                 --no-session-persistence | {INITIALIZE}\n"
            )
        );
    }

    /// Without a list, prompts can only go with Claude Code's default.
    #[tokio::test]
    async fn offers_only_the_default_without_models() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_claude(&fx, dir.path(), "#!/bin/sh\nexit 1\n");
        assert_eq!(state.agents.models().await, []);
        assert!(state.agents.offers(None).await);
        assert!(!state.agents.offers(Some("sonnet")).await);
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
echo '{{"type":"system","subtype":"init","session_id":"s","model":"claude-test"}}'
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"Hi"}}}},"parent_tool_use_id":null}}'
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Hi there"}}],"usage":{{"input_tokens":10,"output_tokens":5}}}},"parent_tool_use_id":null}}'
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
                permission_mode: PermissionMode::Auto,
                model: Some("sonnet".into()),
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
        send(
            &state,
            &session.id,
            "second".into(),
            PermissionMode::Manual,
            None,
        );
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
        assert_eq!(session.context_tokens, Some(15));
        assert_eq!(session.context_window, Some(200_000));
        assert_eq!(session.model.as_deref(), Some("claude-test"));

        // The first turn creates the session, the second resumes it, both
        // in the worktree.
        let id = &session.id;
        let log = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert!(
            lines[0].starts_with(&format!("-p --session-id {id} ")),
            "{log}"
        );
        assert!(lines[0].contains(" --permission-mode auto "), "{log}");
        assert!(lines[0].contains(" --model sonnet "), "{log}");
        assert!(lines[0].ends_with(" | -first | 1"), "{log}");
        assert!(lines[1].starts_with(&format!("-p --resume {id} ")), "{log}");
        assert!(lines[1].contains(" --permission-mode manual "), "{log}");
        assert!(!lines[1].contains("--model"), "{log}");
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
        assert!(live.contains(&AgentEvent {
            seq: None,
            kind: AgentEventKind::Usage {
                tokens: Some(15),
                window: None
            }
        }));
        assert!(live.contains(&AgentEvent {
            seq: None,
            kind: AgentEventKind::Model("claude-test".into())
        }));
        // At least the second turn's, which started after subscribing.
        assert!(live.iter().filter(|e| e.seq.is_some()).count() >= 3);

        assert!(delete(&state, id).await.unwrap());
        assert!(!std::path::Path::new(&session.worktree).exists());
        assert_eq!(state.db.agent(id).unwrap(), None);
        assert!(!delete(&state, id).await.unwrap());
    }

    /// Haiku summarizes a new session's first prompt in the background,
    /// without tools.
    #[tokio::test]
    async fn summarizes_the_first_prompt() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let script = format!(
            r#"#!/bin/sh
case " $* " in
*" --json-schema "*)
  echo "$*" > {dir}/summary_args
  cat > {dir}/summary_input
  echo '{{"type":"result","is_error":false,"structured_output":{{"summary":"Write a note"}}}}'
  ;;
*)
  cat > /dev/null
  echo '{{"type":"result","subtype":"success","is_error":false,"result":"ok"}}'
  ;;
esac
"#,
            dir = dir.path().display()
        );
        let state = state_with_claude(&fx, dir.path(), &script);
        state
            .agents
            .summaries
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let session = create(
            &state,
            NewAgent {
                repo: "owner/name".into(),
                pr: None,
                prompt: "please write a note\nin notes.txt".into(),
                permission_mode: PermissionMode::Auto,
                model: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(session.summary, None);

        let mut summary = None;
        for _ in 0..100 {
            summary = state.db.agent(&session.id).unwrap().unwrap().summary;
            if summary.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(summary.as_deref(), Some("Write a note"));

        let args = std::fs::read_to_string(dir.path().join("summary_args")).unwrap();
        assert!(args.contains("--model haiku"), "{args}");
        assert!(args.contains("--tools  --permission-mode"), "{args}");
        let input = std::fs::read_to_string(dir.path().join("summary_input")).unwrap();
        assert!(
            input.ends_with("please write a note\nin notes.txt\n"),
            "{input}"
        );
        wait_for_turns(&state, &session.id, 1).await;
    }

    /// A session whose fake claude starts a container, and answers scans
    /// with `scan` after logging their arguments and input to `dir`.
    async fn scanned_session(
        dir: &Path,
        scan: &str,
    ) -> (crate::git::tests::Fixture, Arc<State>, AgentSession) {
        let fx = crate::git::tests::fixture();
        let script = format!(
            r#"#!/bin/sh
case " $* " in
*" --json-schema "*)
  echo "$*" > {dir}/scan_args
  cat > {dir}/scan_input
  {scan}
  ;;
*)
  echo "$*" >> {dir}/turn_args
  cat > /dev/null
  echo '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"docker run -d --name e2e pg"}}}}]}},"parent_tool_use_id":null}}'
  echo '{{"type":"result","subtype":"success","is_error":false,"result":"ok"}}'
  ;;
esac
"#,
            dir = dir.display()
        );
        let state = state_with_claude(&fx, dir, &script);
        state
            .agents
            .scans
            .enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let session = create(
            &state,
            NewAgent {
                repo: "owner/name".into(),
                pr: None,
                prompt: "Run the e2e tests".into(),
                permission_mode: PermissionMode::Auto,
                model: None,
            },
        )
        .await
        .unwrap();
        wait_for_turns(&state, &session.id, 1).await;
        (fx, state, session)
    }

    /// Waits for the scans to end and returns the cleanup items.
    async fn wait_for_scans(state: &State) -> Vec<grenadine_core::api::CleanupItem> {
        for _ in 0..200 {
            let c = crate::cleanup::cleanup_state(state).unwrap();
            if c.scanning.is_empty() {
                return c.items;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the scan didn't end");
    }

    /// Deleting a session has Haiku scan its transcript in the background
    /// for what it left behind, and the items can be handed off to a new
    /// session that plans how to clean them up.
    #[tokio::test]
    async fn delete_scans_for_leftovers_and_hands_them_off() {
        let dir = tempfile::tempdir().unwrap();
        let (_fx, state, session) = scanned_session(
            dir.path(),
            r#"sleep 1
  echo '{"type":"result","is_error":false,"structured_output":{"items":[{"title":"Container e2e","detail":"`docker rm -f e2e`"}]}}'"#,
        )
        .await;

        assert!(delete(&state, &session.id).await.unwrap());
        // The delete doesn't wait for the scan.
        let c = crate::cleanup::cleanup_state(&state).unwrap();
        assert_eq!(c.scanning, ["Run the e2e tests"]);
        assert_eq!(c.items, []);

        let items = wait_for_scans(&state).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Container e2e");
        assert_eq!(items[0].detail, "`docker rm -f e2e`");
        assert_eq!(items[0].session_id, session.id);
        assert_eq!(items[0].session_title, "Run the e2e tests");
        assert_eq!(items[0].handed_off_to, None);
        let args = std::fs::read_to_string(dir.path().join("scan_args")).unwrap();
        assert!(args.starts_with("-p --model haiku "), "{args}");
        assert!(args.contains(" --permission-mode dontAsk "), "{args}");
        assert!(args.contains(" --allowedTools Read Glob Grep "), "{args}");
        let input = std::fs::read_to_string(dir.path().join("scan_input")).unwrap();
        assert!(input.contains("$ docker run -d --name e2e pg\n"), "{input}");

        let cleaner = crate::cleanup::hand_off(
            &state,
            grenadine_core::api::NewCleanupSession {
                ids: vec![items[0].id],
                repo: "owner/name".into(),
                model: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            cleaner.title,
            "Clean up what deleted agent sessions left behind"
        );
        wait_for_turns(&state, &cleaner.id, 1).await;
        let turns = std::fs::read_to_string(dir.path().join("turn_args")).unwrap();
        let last = turns.lines().last().unwrap();
        assert!(
            last.contains(&format!("--session-id {}", cleaner.id)),
            "{last}"
        );
        assert!(last.contains(" --permission-mode plan "), "{last}");
        let prompt = state.db.agent_events(&cleaner.id).unwrap();
        let AgentEventKind::Prompt(prompt) = &prompt[0].kind else {
            panic!("{prompt:?}");
        };
        assert!(prompt.contains("## Container e2e\n"), "{prompt}");
        assert!(prompt.contains("`docker rm -f e2e`"), "{prompt}");
        let items = crate::cleanup::cleanup_state(&state).unwrap().items;
        assert_eq!(items[0].handed_off_to.as_deref(), Some(cleaner.id.as_str()));

        // Items that are gone can't be handed off.
        assert!(state.db.delete_cleanup_item(items[0].id).unwrap());
        let none = crate::cleanup::hand_off(
            &state,
            grenadine_core::api::NewCleanupSession {
                ids: vec![items[0].id],
                repo: "owner/name".into(),
                model: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(none, None);
    }

    /// Sonnet checks the cleanup items and the stale ones it names are
    /// deleted, but only ones it was asked about.
    #[tokio::test]
    async fn prune_deletes_stale_cleanup_items() {
        let dir = tempfile::tempdir().unwrap();
        let fx = crate::git::tests::fixture();
        let script = format!(
            r#"#!/bin/sh
echo "$*" > {dir}/prune_args
cat > {dir}/prune_input
cat {dir}/answer
"#,
            dir = dir.path().display()
        );
        let state = state_with_claude(&fx, dir.path(), &script);

        // No items, no Sonnet.
        assert_eq!(crate::cleanup::prune(&state).await.unwrap(), 0);
        assert!(!dir.path().join("prune_args").exists());

        let session = AgentSession {
            id: "gone".into(),
            repo: "owner/name".into(),
            pr: None,
            title: "Run the e2e tests".into(),
            summary: None,
            branch: "b".into(),
            worktree: "/w".into(),
            base_sha: "s".into(),
            status: AgentStatus::Idle,
            created_at: 0,
            cost_usd: 0.0,
            context_tokens: None,
            context_window: None,
            model: None,
            landed: None,
            prs: None,
            unstaged: 0,
        };
        state
            .db
            .add_cleanup_items(
                &session,
                &[
                    ("Container e2e".into(), "`docker rm -f e2e`".into()),
                    (
                        "Branch dev/e2e".into(),
                        "`git push -d origin dev/e2e`".into(),
                    ),
                ],
                now(),
            )
            .unwrap();
        let items = crate::cleanup::cleanup_state(&state).unwrap().items;
        let container = items.iter().find(|i| i.title == "Container e2e").unwrap();
        std::fs::write(
            dir.path().join("answer"),
            json!({
                "type": "result",
                "is_error": false,
                "structured_output": {"stale": [
                    {"id": container.id, "reason": "No such container"},
                    {"id": 9999, "reason": "made up"},
                ]},
            })
            .to_string(),
        )
        .unwrap();

        let mut events = state.events.subscribe();
        assert_eq!(crate::cleanup::prune(&state).await.unwrap(), 1);
        assert_eq!(
            events.try_recv().unwrap(),
            grenadine_core::api::ServerEvent::CleanupChanged
        );
        let left = crate::cleanup::cleanup_state(&state).unwrap().items;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].title, "Branch dev/e2e");

        let args = std::fs::read_to_string(dir.path().join("prune_args")).unwrap();
        assert!(args.starts_with("-p --model sonnet "), "{args}");
        assert!(args.contains(" --permission-mode dontAsk "), "{args}");
        let input = std::fs::read_to_string(dir.path().join("prune_input")).unwrap();
        for item in &items {
            assert!(
                input.contains(&format!("## Item {}: {}\n", item.id, item.title)),
                "{input}"
            );
            assert!(input.contains(&item.detail), "{input}");
        }
    }

    /// A scan that fails leaves an item saying so.
    #[tokio::test]
    async fn a_failed_scan_leaves_an_item() {
        let dir = tempfile::tempdir().unwrap();
        let (_fx, state, session) =
            scanned_session(dir.path(), "echo 'out of credit' >&2; exit 1").await;
        assert!(delete(&state, &session.id).await.unwrap());
        let items = wait_for_scans(&state).await;
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].title,
            "Couldn't scan \u{201c}Run the e2e tests\u{201d}"
        );
        assert!(
            items[0].detail.contains("out of credit"),
            "{}",
            items[0].detail
        );
    }

    /// Deleting a session stops what claude left running in the worktree,
    /// even outside its process group, the way Bazel's server is.
    #[tokio::test]
    async fn delete_stops_leftover_processes() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!(
            r#"#!/bin/sh
cat > /dev/null
setsid sleep 1000 < /dev/null > /dev/null 2>&1 &
echo $! > {pid}
echo '{{"type":"result","subtype":"success","is_error":false,"result":"ok"}}'
"#,
            pid = pid_file.display()
        );
        let state = state_with_claude(&fx, dir.path(), &script);
        let session = create(
            &state,
            NewAgent {
                repo: "owner/name".into(),
                pr: None,
                prompt: "build".into(),
                permission_mode: PermissionMode::Auto,
                model: None,
            },
        )
        .await
        .unwrap();
        wait_for_turns(&state, &session.id, 1).await;
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(alive(pid));

        assert!(delete(&state, &session.id).await.unwrap());
        assert!(!alive(pid));
    }

    /// A session row with its worktree at `worktree`, never started.
    fn session_row(id: &str, repo: &str, worktree: &Path) -> AgentSession {
        AgentSession {
            id: id.into(),
            repo: repo.into(),
            pr: None,
            title: "t".into(),
            summary: None,
            branch: session_branch(id),
            worktree: worktree.to_string_lossy().into_owned(),
            base_sha: "0".repeat(40),
            status: AgentStatus::Idle,
            created_at: now(),
            cost_usd: 0.0,
            context_tokens: None,
            context_window: None,
            model: None,
            landed: None,
            prs: None,
            unstaged: 0,
        }
    }

    /// A session whose repo isn't configured can't be deleted while its
    /// worktree is there, since nothing would remove it.
    #[tokio::test]
    async fn delete_keeps_a_session_whose_repo_isnt_configured() {
        let state = crate::sync::test_state();
        let dir = tempfile::tempdir().unwrap();
        let there = uuid();
        state
            .db
            .create_agent(&session_row(&there, "owner/gone", dir.path()))
            .unwrap();
        let err = delete(&state, &there).await.unwrap_err().to_string();
        assert!(err.contains("owner/gone isn't configured"), "{err}");
        assert!(state.db.agent(&there).unwrap().is_some());

        let gone = uuid();
        state
            .db
            .create_agent(&session_row(&gone, "owner/gone", &dir.path().join("x")))
            .unwrap();
        assert!(delete(&state, &gone).await.unwrap());
        assert!(state.db.agent(&gone).unwrap().is_none());
    }

    /// Worktrees no session owns go at startup with their branches, even
    /// when detached; the sessions' worktrees and everything outside the
    /// worktrees directory stay.
    #[test]
    fn removes_orphaned_worktrees() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let worktrees = dir.path().join("worktrees");
        let repo_dir = worktrees.join("owner/name");
        let db = crate::db::Db::in_memory();
        let repos = [(
            fx.clone.slug.clone(),
            Arc::new(crate::sync::ClonedRepo {
                repo: fx.clone.clone(),
                git_lock: tokio::sync::Mutex::new(()),
            }),
        )]
        .into_iter()
        .collect();
        let has_branch = |b: &str| {
            let r = format!("refs/heads/{b}");
            fx.clone.refs(&r).unwrap().contains_key(&r)
        };

        let (owned, orphan, detached) = (uuid(), uuid(), uuid());
        for id in [&owned, &orphan, &detached] {
            fx.clone
                .worktree_add(&repo_dir.join(id), &session_branch(id), "HEAD")
                .unwrap();
        }
        run(&repo_dir.join(&detached), &["checkout", "-q", "--detach"]);
        db.create_agent(&session_row(&owned, "owner/name", &repo_dir.join(&owned)))
            .unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fx.clone
            .worktree_add(&elsewhere, "grenadine/agent/other", "HEAD")
            .unwrap();
        std::fs::create_dir_all(worktrees.join("owner/empty")).unwrap();

        remove_orphaned_worktrees(&db, &repos, &worktrees).unwrap();
        assert!(repo_dir.join(&owned).exists());
        assert!(has_branch(&session_branch(&owned)));
        for id in [&orphan, &detached] {
            assert!(!repo_dir.join(id).exists(), "{id}");
            assert!(!has_branch(&session_branch(id)), "{id}");
        }
        assert!(elsewhere.exists());
        assert!(has_branch("grenadine/agent/other"));
        assert!(!worktrees.join("owner/empty").exists());
        assert_eq!(fx.clone.worktrees().unwrap().len(), 3);
    }

    /// A worktree's output bases are those of the workspaces in it, even
    /// nested ones, and come off despite their read-only parts.
    #[test]
    fn removes_a_worktrees_output_bases() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("_bazel_user");
        let worktree = dir.path().join("worktrees/abc");
        let base = |name: &str, workspace: &Path| {
            let base = root.join(name);
            std::fs::create_dir_all(base.join("execroot/_main")).unwrap();
            std::fs::write(
                base.join("DO_NOT_BUILD_HERE"),
                workspace.as_os_str().as_bytes(),
            )
            .unwrap();
            base
        };
        let nested = base("1", &worktree.join("sub/ws"));
        let at_root = base("2", &worktree);
        let sibling = base("3", &dir.path().join("worktrees/abc-2"));
        let other = base("4", Path::new("/elsewhere"));
        std::fs::create_dir_all(root.join("install")).unwrap();
        let read_only = nested.join("execroot/_main/external");
        std::fs::create_dir(&read_only).unwrap();
        std::fs::write(read_only.join("file"), "x").unwrap();
        std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o555)).unwrap();

        let bases = output_bases(&[root.clone(), dir.path().join("missing")], |w| {
            w.starts_with(&worktree)
        });
        assert_eq!(bases, [nested.clone(), at_root.clone()]);
        remove_output_bases(bases);
        assert!(!nested.exists());
        assert!(!at_root.exists());
        assert!(sibling.exists());
        assert!(other.exists());
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
                permission_mode: PermissionMode::default(),
                model: None,
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
                permission_mode: PermissionMode::default(),
                model: None,
            },
        )
        .await
        .unwrap();
        send(
            &state,
            &session.id,
            "queued".into(),
            PermissionMode::default(),
            None,
        );
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

    #[test]
    fn pr_texts() {
        assert_eq!(
            pr_text("t", &["Fix it\n\nBecause.".into()]),
            ("Fix it".into(), "Because.".into())
        );
        assert_eq!(
            pr_text("t", &["One\n\nx".into(), "Two".into()]),
            ("t".into(), "- One\n- Two".into())
        );
    }

    #[test]
    fn finds_pr_urls() {
        let key = |repo: &str, number| PrKey {
            repo: repo.into(),
            number,
        };
        assert_eq!(
            pr_urls(
                "https://github.com/o/n/pull/12\nsee https://github.com/a/b/pull/3/files \
                 https://github.com/o/n/issues/4 https://github.com/o/n/pull/x"
            ),
            [key("o/n", 12), key("a/b", 3)]
        );
    }

    #[test]
    fn finds_opened_prs() {
        let bash = |id: &str, command: &str| AgentEventKind::ToolUse {
            id: id.into(),
            name: "Bash".into(),
            input: json!({ "command": command }).to_string(),
        };
        let result = |id: &str, content: &str, is_error| AgentEventKind::ToolResult {
            id: id.into(),
            content: content.into(),
            is_error,
            patch: Vec::new(),
        };
        let events = [
            bash(
                "1",
                "git push origin HEAD:refs/heads/x && gh pr create --head x",
            ),
            bash("2", "gh pr view 5"),
            // Only the results of `gh pr create` count.
            result("2", "https://github.com/o/n/pull/5", false),
            result("1", "https://github.com/o/n/pull/6\n", false),
            bash("3", "gh pr create --head y"),
            result("3", "https://github.com/o/n/pull/7 already exists", true),
            AgentEventKind::Landed(Landed::PullRequest {
                number: 8,
                url: "https://github.com/o/n/pull/8".into(),
            }),
        ];
        let mut opened = OpenedPrs::default();
        let numbers: Vec<u64> = events
            .iter()
            .flat_map(|e| opened.see("o/n", e))
            .map(|k| k.number)
            .collect();
        assert_eq!(numbers, [6, 8]);
    }

    /// A claude that changes the worktree as its prompts say: `write`
    /// leaves `file` with `contents` uncommitted, `commit` commits it too,
    /// and the wrap-up's commit prompt commits everything. It ignores
    /// other prompts. Prompts are logged to `log`.
    fn wrap_up_claude(log: &Path) -> String {
        format!(
            r#"#!/bin/sh
prompt=$(cat)
echo "$prompt" >> {log}
set -- $prompt
case "$1" in
  write) echo "$3" > "$2" ;;
  commit) echo "$3" > "$2" && git add -A && git commit -qm "Change $2" ;;
  Wrap) case "$prompt" in "{COMMIT_PROMPT}") git add -A && git commit -qm "Wrapped up" ;; esac ;;
esac
echo '{{"type":"system","subtype":"init","session_id":"s","model":"claude-test"}}'
echo '{{"type":"result","subtype":"success","is_error":false,"result":"ok"}}'
"#,
            log = log.display()
        )
    }

    /// A state for wrap-up tests and the clone's directory, which commits
    /// need an identity in.
    fn wrap_up_state(fx: &crate::git::tests::Fixture, dir: &Path) -> (Arc<State>, PathBuf) {
        use crate::git::tests::run;
        let clone = fx.clone.path.clone();
        run(&clone, &["config", "user.name", "t"]);
        run(&clone, &["config", "user.email", "t@example.com"]);
        let state = state_with_claude(fx, dir, &wrap_up_claude(&dir.join("log")));
        (state, clone)
    }

    /// Starts a session with `prompt` and waits for its turn.
    async fn start(state: &Arc<State>, prompt: &str) -> AgentSession {
        let session = create(
            state,
            NewAgent {
                repo: "owner/name".into(),
                pr: None,
                prompt: prompt.into(),
                permission_mode: PermissionMode::Auto,
                model: None,
            },
        )
        .await
        .unwrap();
        wait_for_turns(state, &session.id, 1).await;
        session
    }

    /// Wraps up the session and returns how it ended.
    async fn wrap(state: &Arc<State>, id: &str, target: WrapUpTarget) -> AgentEventKind {
        let req = WrapUp {
            target,
            permission_mode: PermissionMode::Auto,
            model: None,
        };
        assert!(wrap_up(state, id, req).await);
        for _ in 0..100 {
            let done = state
                .agents
                .with_live(id, |l| l.wrap_up.is_none() && !l.running);
            if done {
                let events = state.db.agent_events(id).unwrap();
                return events.into_iter().last().unwrap().kind;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the wrap-up didn't finish");
    }

    #[tokio::test]
    async fn wraps_up_into_the_checkout() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, clone) = wrap_up_state(&fx, dir.path());

        // Claude is asked to commit what it left uncommitted first.
        let session = start(&state, "write b agent").await;
        let id = &session.id;
        let AgentEventKind::Landed(Landed::Checkout { sha }) =
            wrap(&state, id, WrapUpTarget::Checkout).await
        else {
            panic!("{:?}", state.db.agent_events(id));
        };
        assert_eq!(run(&clone, &["rev-parse", "HEAD"]), sha);
        assert_eq!(run(&clone, &["branch", "--show-current"]), "main");
        assert_eq!(std::fs::read_to_string(clone.join("b")).unwrap(), "agent\n");
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(log.contains(COMMIT_PROMPT), "{log}");
        let landed = state.db.agent(id).unwrap().unwrap();
        assert_eq!(landed.landed, Some(Landed::Checkout { sha: sha.clone() }));
        assert_eq!(landed.base_sha, sha);
        assert_eq!(landed.status, AgentStatus::Idle);

        // The checkout moves on, detached. Wrapping up again rebases only
        // the new commit onto it.
        run(&clone, &["checkout", "-q", "--detach"]);
        let moved = commit(&clone, "c", "1\n");
        send(
            &state,
            id,
            "commit b more".into(),
            PermissionMode::Auto,
            None,
        );
        wait_for_turns(&state, id, 3).await;
        let AgentEventKind::Landed(Landed::Checkout { sha }) =
            wrap(&state, id, WrapUpTarget::Checkout).await
        else {
            panic!("{:?}", state.db.agent_events(id));
        };
        assert_eq!(run(&clone, &["rev-parse", "HEAD"]), sha);
        assert_eq!(run(&clone, &["branch", "--show-current"]), "");
        assert_eq!(run(&clone, &["rev-parse", "HEAD^"]), moved);
        assert_eq!(std::fs::read_to_string(clone.join("b")).unwrap(), "more\n");

        // Nothing new to land.
        assert_eq!(
            wrap(&state, id, WrapUpTarget::Checkout).await,
            AgentEventKind::WrapUpFailed("there are no commits to land".into())
        );
    }

    #[tokio::test]
    async fn asks_once_to_resolve_conflicts() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, clone) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit a agent").await;
        let before = commit(&clone, "a", "clone\n");

        // This claude ignores the request to rebase, so the second try
        // fails the same way.
        let AgentEventKind::WrapUpFailed(e) =
            wrap(&state, &session.id, WrapUpTarget::Checkout).await
        else {
            panic!("{:?}", state.db.agent_events(&session.id));
        };
        assert!(e.contains("conflict"), "{e}");
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert_eq!(
            log.matches("Wrap up: rebase your commits onto").count(),
            1,
            "{log}"
        );
        assert_eq!(run(&clone, &["rev-parse", "HEAD"]), before);
        let worktree = Path::new(&session.worktree);
        assert_eq!(fx.clone.worktree_unfinished(worktree).unwrap(), None);
    }

    #[tokio::test]
    async fn keeps_local_changes_in_the_checkout() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, clone) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        std::fs::write(clone.join("b"), "local\n").unwrap();
        let before = run(&clone, &["rev-parse", "HEAD"]);

        let event = wrap(&state, &session.id, WrapUpTarget::Checkout).await;
        assert!(
            matches!(event, AgentEventKind::WrapUpFailed(_)),
            "{event:?}"
        );
        assert_eq!(run(&clone, &["rev-parse", "HEAD"]), before);
        assert_eq!(std::fs::read_to_string(clone.join("b")).unwrap(), "local\n");
        assert_eq!(state.db.agent(&session.id).unwrap().unwrap().landed, None);
    }

    #[tokio::test]
    async fn pushes_the_branch_for_a_pr() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;

        // The test state's GitHub is unreachable, so only the push works.
        let event = wrap(&state, &session.id, WrapUpTarget::PullRequest).await;
        let AgentEventKind::WrapUpFailed(e) = event else {
            panic!("{event:?}");
        };
        assert!(!e.contains("no commits"), "{e}");
        let worktree = Path::new(&session.worktree);
        assert_eq!(
            run(
                &fx.upstream,
                &["rev-parse", &format!("refs/heads/{}", session.branch)]
            ),
            run(worktree, &["rev-parse", "HEAD"])
        );
    }
}
