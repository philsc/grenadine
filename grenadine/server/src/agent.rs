//! Agent sessions driven from the UI. Each session works in its own git
//! worktree; every prompt runs one turn with the session's `Backend` (the
//! Claude Code backend is in `claude.rs`), and the turn's output becomes
//! the session's transcript. Each prompt comes with a permission mode;
//! the backend asks about the tool uses that the mode doesn't decide on
//! its own, and the user answers on the page.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use futures::future::BoxFuture;
use grenadine_core::api::{
    AgentBackend, AgentBackendInfo, AgentEvent, AgentEventKind, AgentModelInfo, AgentSession,
    AgentStatus, ApprovalOption, GENERAL, Landed, LandedPr, NewAgent, PermissionMode, PrKey,
    PromptImage, ServerEvent, WrapUp, WrapUpTarget,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, BufReader};
use tokio::sync::{broadcast, oneshot};

use crate::claude;
use crate::devin;
use crate::git::Repo;
use crate::reflow::reflow;
use crate::sync::State;

/// The ref the trunk's tip is fetched into before a session branches off it.
const TRUNK_REF: &str = "refs/grenadine/agent/trunk";

/// The most images one prompt may come with.
pub const MAX_IMAGES: usize = 20;

/// The largest image a prompt may come with, in bytes. The Claude API
/// takes images up to 5 MB.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The image types Claude takes, with the extensions they're stored with.
const IMAGE_TYPES: [(&str, &str); 4] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
];

/// How long a deleted session's processes get to exit before they're
/// killed.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a backend that couldn't list its models goes before it's
/// asked again, so that one that hangs doesn't hold up every request.
const LISTING_RETRY: Duration = Duration::from_secs(5 * 60);

/// How much of a backend's stderr is kept to explain a crash.
const STDERR_TAIL: usize = 4096;

/// What Claude is asked when there's uncommitted work to wrap up.
const COMMIT_PROMPT: &str = "Wrap up: commit all of your changes with descriptive commit \
     messages that follow this repository's conventions. Don't change anything else.";

/// What Claude is asked when there's uncommitted work to open PRs for.
const PR_COMMIT_PROMPT: &str = "Wrap up: commit all of your changes with descriptive commit \
     messages that follow this repository's conventions. Each commit becomes its own pull \
     request in a stack, so make each one a self-contained change that builds and passes the \
     tests on its own. Don't change anything else.";

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
    /// Each commit is on GitHub on its own branch; the PRs that aren't
    /// open yet are opened next, the bottom one into `base`.
    Pushed {
        base: String,
        /// The branch and message of each commit, oldest first.
        commits: Vec<(String, String)>,
    },
}

/// The user's answer to a permission request.
#[derive(Debug, Default)]
pub struct Decision {
    pub allow: bool,
    pub message: Option<String>,
    /// The mode an approved plan goes on in.
    pub mode: Option<PermissionMode>,
    /// The id of the offered option the user picked, if the request
    /// offered any.
    pub option: Option<String>,
    /// The answers to `AskUserQuestion`, by question.
    pub answers: Option<BTreeMap<String, String>>,
}

/// An agent backend: what runs sessions' turns and background jobs.
/// Adding one takes an impl of this and an arm in `Agents::backend`; what
/// it offers that never changes, like its permission modes, is on
/// `AgentBackend`.
pub trait Backend: Send + Sync {
    /// The models it offers; an error while they can't be listed.
    fn models(&self) -> BoxFuture<'_, Result<Vec<AgentModelInfo>>>;

    /// Runs `turn` of `session` and returns the status the session ends
    /// up in; `sink` gets what the turn's output means for the transcript.
    fn turn<'a>(
        &'a self,
        state: &'a Arc<State>,
        session: &'a AgentSession,
        turn: Turn<'a>,
        sink: TurnSink<'a>,
    ) -> BoxFuture<'a, Result<AgentStatus>>;

    /// Runs `job` and returns its answer, in the shape of its schema.
    fn structured<'a>(&'a self, job: &'a StructuredJob<'a>) -> BoxFuture<'a, Result<Value>>;
}

/// One prompt's turn: what it says and how it runs.
pub struct Turn<'a> {
    pub prompt: &'a str,
    pub images: &'a [String],
    pub mode: &'a PermissionMode,
    /// `None` or `default` keeps the backend's default.
    pub model: Option<String>,
    /// The session's grenadine MCP endpoint.
    pub mcp_url: &'a str,
    /// What the agent is told beyond the prompt: Claude gets them
    /// appended to its system prompt, Devin as the prompt's first block.
    pub notes: &'a [String],
}

/// A run that answers in a JSON schema's shape without a session, for the
/// background jobs like summaries and cleanup scans.
pub struct StructuredJob<'a> {
    /// What it's called in errors.
    pub name: &'a str,
    pub model: &'a str,
    pub schema: &'a Value,
    pub input: &'a str,
    /// The tools it has, comma-separated; `None` leaves the backend's.
    pub tools: Option<&'a str>,
    /// The tools it may use without asking.
    pub allowed: &'a [&'a str],
    /// What it may spend, in USD.
    pub budget_usd: &'a str,
    /// Whether to load only the user's settings, not a project's.
    pub setting_sources_user: bool,
    /// A directory it may read besides `cwd`.
    pub add_dir: Option<&'a Path>,
    pub cwd: &'a Path,
    /// How long it may take before it's killed.
    pub timeout: Duration,
}

/// A backend's models: listed once and kept, or the listing's failure
/// kept for `LISTING_RETRY`. Whoever asks while it lists waits for that
/// listing.
#[derive(Default)]
pub struct ModelList(tokio::sync::Mutex<Option<Listing>>);

enum Listing {
    Listed(Vec<AgentModelInfo>),
    Failed { at: Instant, error: String },
}

impl ModelList {
    /// The models, from `list` unless they're known; `name` is the
    /// backend's, for the log.
    pub async fn get<F>(&self, name: &str, list: impl FnOnce() -> F) -> Result<Vec<AgentModelInfo>>
    where
        F: Future<Output = Result<Vec<AgentModelInfo>>>,
    {
        let mut listing = self.0.lock().await;
        match &*listing {
            Some(Listing::Listed(models)) => return Ok(models.clone()),
            Some(Listing::Failed { at, error }) if at.elapsed() < LISTING_RETRY => {
                bail!("{error}")
            }
            _ => {}
        }
        match list().await {
            Ok(models) => {
                *listing = Some(Listing::Listed(models.clone()));
                Ok(models)
            }
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!("listing {name}'s models: {error}");
                *listing = Some(Listing::Failed {
                    at: Instant::now(),
                    error: error.clone(),
                });
                bail!("{error}")
            }
        }
    }
}

/// Stops a turn the way Ctrl-C would. Interrupting a turn again stops it
/// again.
#[derive(Clone)]
pub struct Stopper(Arc<dyn Fn() + Send + Sync>);

impl Stopper {
    pub fn new(stop: impl Fn() + Send + Sync + 'static) -> Stopper {
        Stopper(Arc::new(stop))
    }

    /// Sends `child` SIGINT.
    pub fn sigint(child: &tokio::process::Child) -> Stopper {
        let pid = child.id();
        Stopper::new(move || {
            if let Some(pid) = pid {
                interrupt_pid(pid);
            }
        })
    }

    pub fn stop(&self) {
        (self.0)()
    }
}

/// A command that runs a backend's `exe` for a turn of `session`: in its
/// worktree, in its systemd slice if it has one, outside the server's
/// process group, with its stdio piped, killed when it's dropped.
pub fn backend_command(
    state: &State,
    session: &AgentSession,
    exe: &Path,
) -> tokio::process::Command {
    // `systemd-run --scope` execs `exe`, so the PID stays its own.
    let mut cmd = if let Some(slice) = state.agents.slice_of(&session.id) {
        let mut cmd = tokio::process::Command::new("systemd-run");
        cmd.args(["--user", "--scope", "--quiet", "--collect"])
            .arg(format!("--slice={slice}"))
            .arg("--")
            .arg(exe);
        cmd
    } else {
        tokio::process::Command::new(exe)
    };
    cmd.current_dir(&session.worktree)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Keep it out of the terminal's process group, like git in
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
    cmd
}

/// Reads all of a backend's `stderr`; the task returns its end, to
/// explain a crash.
pub fn stderr_tail(
    mut stderr: impl AsyncRead + Unpin + Send + 'static,
) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        let text = String::from_utf8_lossy(&buf).trim().to_owned();
        let start = text.len().saturating_sub(STDERR_TAIL);
        let start = (start..=text.len())
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        text[start..].to_owned()
    })
}

/// A backend's output, line by line. When the server shuts down
/// meanwhile, the turn is stopped once and the output read on until it
/// ends.
pub struct OutputLines<R> {
    lines: tokio::io::Lines<BufReader<R>>,
    stopper: Stopper,
    stopping: bool,
}

impl<R: AsyncRead + Unpin> OutputLines<R> {
    pub fn new(out: R, stopper: Stopper) -> OutputLines<R> {
        OutputLines {
            lines: BufReader::new(out).lines(),
            stopper,
            stopping: false,
        }
    }

    /// The next line; `None` once the output ends.
    pub async fn next(&mut self, state: &State) -> Result<Option<String>> {
        loop {
            tokio::select! {
                line = self.lines.next_line() => return Ok(line?),
                _ = state.shutdown.cancelled(), if !self.stopping => {
                    self.stopping = true;
                    self.stopper.stop();
                }
            }
        }
    }
}

pub struct Agents {
    /// The Claude Code backend.
    claude: claude::Claude,
    /// The Devin backend.
    devin: devin::Devin,
    /// Worktrees go to `<dir>/<owner>/<name>/<id>`.
    worktrees: PathBuf,
    /// The images sent with prompts go to `<dir>/<id>/<name>`, where `dir`
    /// is `images` next to `worktrees`.
    images: PathBuf,
    /// Where Claude Code reaches this server, e.g. `http://127.0.0.1:8765`.
    base_url: String,
    /// Whether each session's processes go into their own systemd user
    /// slice.
    slices: bool,
    live: std::sync::Mutex<HashMap<String, Live>>,
    /// The scans of deleted sessions for what they left behind.
    pub scans: crate::cleanup::Scans,
    /// Whether Haiku summarizes each new session's first prompt.
    pub summaries: std::sync::atomic::AtomicBool,
    /// The shells opened from the sessions' pages.
    pub terminals: crate::terminal::Terminals,
}

/// A permission request waiting for the user's answer.
struct PendingRequest {
    /// Answers the request's waiter.
    tx: oneshot::Sender<Decision>,
    /// The answers the backend offers beyond allow/deny.
    options: Vec<ApprovalOption>,
}

/// What only exists while the server runs: the event channel open pages
/// listen to, queued prompts and the pending permission requests.
struct Live {
    events: broadcast::Sender<AgentEvent>,
    /// Authenticates the session's MCP endpoint.
    secret: String,
    /// Prompts waiting for their turn.
    queue: VecDeque<Queued>,
    /// Whether a task is running the queued prompts.
    running: bool,
    /// Stops the current turn.
    stopper: Option<Stopper>,
    /// The user stopped the current turn.
    interrupted: bool,
    /// Permission requests by tool use ID.
    pending: HashMap<String, PendingRequest>,
    /// The mode and model the running turn goes with, for follow-ups a
    /// denied request queues.
    turn: Option<(PermissionMode, Option<String>)>,
    /// The wrap-up in progress, if any.
    wrap_up: Option<Pending>,
}

/// A prompt with the names of its images, and the mode and model its turn
/// goes with.
struct Queued {
    prompt: String,
    images: Vec<String>,
    mode: PermissionMode,
    model: Option<String>,
}

impl Live {
    fn new() -> Live {
        Live {
            events: broadcast::channel(1024).0,
            secret: random_hex(16),
            queue: VecDeque::new(),
            running: false,
            stopper: None,
            interrupted: false,
            pending: HashMap::new(),
            turn: None,
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
pub fn uuid() -> String {
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
/// their branches, and the general sessions' directories that no session
/// owns, then the directories that are left empty. Must run
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
    // Even sessions this build can't run own what they own.
    let owned: std::collections::BTreeSet<String> = db
        .agents_unchecked()?
        .into_iter()
        .map(|(s, _)| s.branch)
        .collect();
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
            if db.agent_unchecked(id)?.is_some() {
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
    // General sessions' directories aren't worktrees of any clone.
    for dir in std::fs::read_dir(root.join(GENERAL))
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = dir.path();
        let Some(id) = path
            .file_name()
            .and_then(|id| id.to_str())
            .filter(|id| valid_id(id))
        else {
            continue;
        };
        if db.agent_unchecked(id)?.is_some() {
            continue;
        }
        match remove_tree(&path) {
            Ok(()) => tracing::info!("removed orphaned directory {}", path.display()),
            Err(e) => tracing::warn!("can't remove {}: {e}", path.display()),
        }
    }
    // Only the `owner/name` levels; the worktrees themselves are gone. A
    // general session's directory may well be empty, and isn't one of them.
    for owner in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        if owner.file_name() == GENERAL {
            let _ = std::fs::remove_dir(owner.path());
            continue;
        }
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
        // The claude and devin executables the backends run.
        exes: (PathBuf, PathBuf),
        worktrees: PathBuf,
        base_url: String,
        slices: bool,
        scans: bool,
        summaries: bool,
    ) -> Result<Agents> {
        db.interrupt_agents()?;
        // The backend runs in the worktrees, so a relative path would
        // resolve differently there.
        let worktrees = std::path::absolute(&worktrees)?;
        Ok(Agents {
            claude: claude::Claude::new(exes.0),
            devin: devin::Devin::new(exes.1),
            images: worktrees.with_file_name("images"),
            worktrees,
            base_url,
            slices,
            live: std::sync::Mutex::default(),
            scans: crate::cleanup::Scans::new(scans),
            summaries: summaries.into(),
            terminals: crate::terminal::Terminals::default(),
        })
    }

    /// The systemd user slice the session's processes go into, if they go
    /// into one.
    pub fn slice_of(&self, id: &str) -> Option<String> {
        self.slices.then(|| slice(id))
    }

    /// What runs the turns and background jobs of `backend`.
    pub fn backend(&self, backend: AgentBackend) -> &dyn Backend {
        match backend {
            AgentBackend::Claude => &self.claude,
            AgentBackend::Devin => &self.devin,
        }
    }

    /// Where the images sent with prompts go.
    pub fn images(&self) -> &Path {
        &self.images
    }

    /// Runs `f` on the session's live state, creating it if needed.
    fn with_live<T>(&self, id: &str, f: impl FnOnce(&mut Live) -> T) -> T {
        let mut live = self.live.lock().unwrap();
        f(live.entry(id.to_owned()).or_insert_with(Live::new))
    }

    /// Lets `interrupt` stop the current turn with `stopper`. An
    /// interrupt that came before the turn's process existed stops it now.
    pub fn set_stopper(&self, id: &str, stopper: Stopper) {
        let interrupted = self.with_live(id, |l| {
            l.stopper = Some(stopper.clone());
            l.interrupted
        });
        if interrupted {
            stopper.stop();
        }
    }

    /// Whether the user stopped the current turn.
    pub fn interrupted(&self, id: &str) -> bool {
        self.with_live(id, |l| l.interrupted)
    }

    /// The secret the session's MCP endpoint expects.
    pub fn secret(&self, id: &str) -> String {
        self.with_live(id, |l| l.secret.clone())
    }

    pub fn subscribe(&self, id: &str) -> broadcast::Receiver<AgentEvent> {
        self.with_live(id, |l| l.events.subscribe())
    }

    /// The backends sessions can pick, with their models: the default
    /// one always, its models empty when they can't be listed, and the
    /// others when they can list theirs.
    pub async fn backends(&self) -> Vec<AgentBackendInfo> {
        let listed =
            futures::future::join_all(AgentBackend::ALL.map(|b| self.backend(b).models())).await;
        AgentBackend::ALL
            .into_iter()
            .zip(listed)
            .filter_map(|(backend, listed)| match listed {
                Ok(models) => Some(backend.info(models)),
                Err(_) if backend == AgentBackend::default() => Some(backend.info(Vec::new())),
                Err(_) => None,
            })
            .collect()
    }

    /// Whether `backend` lets prompts ask for `model`: its default, or
    /// one it lists. While it can't list them, there's no telling, so any
    /// that can't pass for a flag goes and the backend itself rejects one
    /// it doesn't have.
    ///
    /// Only `backend` is asked, and a backend that can't list its models
    /// still takes prompts that keep its default.
    pub async fn offers(&self, backend: AgentBackend, model: Option<&str>) -> bool {
        let Some(m) = model else { return true };
        match self.backend(backend).models().await {
            Ok(models) if !models.is_empty() => models.iter().any(|i| i.value == m),
            _ => !m.is_empty() && !m.starts_with('-'),
        }
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

/// Creates the session's worktree, or a plain directory for a general
/// session, and starts working on its first prompt.
pub async fn create(state: &Arc<State>, req: NewAgent) -> Result<AgentSession> {
    if req.prompt.trim().is_empty() {
        bail!("the prompt is empty");
    }
    let id = uuid();
    let worktree = state.agents.worktrees.join(&req.repo).join(&id);
    let (branch, base_sha) = if req.repo == GENERAL {
        if req.pr.is_some() {
            bail!("a general session can't start from a PR");
        }
        tokio::fs::create_dir_all(&worktree).await?;
        (String::new(), String::new())
    } else {
        let branch = session_branch(&id);
        let base_sha = add_worktree(state, &req, &branch, &worktree).await?;
        (branch, base_sha)
    };

    let session = AgentSession {
        id: id.clone(),
        backend: req.backend,
        repo: req.repo,
        pr: req.pr,
        title: title(&req.prompt),
        summary: None,
        branch,
        worktree: worktree.to_string_lossy().into_owned(),
        base_sha,
        status: AgentStatus::Idle,
        created_at: now(),
        cost: 0.0,
        cost_unit: req.backend.cost_unit(),
        context_tokens: None,
        context_window: None,
        model: None,
        landed: None,
        prs: None,
        unstaged: 0,
    };
    state.db.create_agent(&session)?;
    state.send(ServerEvent::AgentsChanged);
    let images = save_images(state, &id, &req.images).await?;
    crate::summary::start(state, &id, &req.prompt);
    send_with_images(
        state,
        &id,
        req.prompt,
        images,
        req.permission_mode,
        req.model,
    );
    Ok(session)
}

/// Adds the worktree of a new session in `req.repo` at `worktree`, with
/// `branch` checked out at the trunk or the PR's head, and returns the
/// commit it starts from.
async fn add_worktree(
    state: &State,
    req: &NewAgent,
    branch: &str,
    worktree: &Path,
) -> Result<String> {
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

    let _guard = cloned.git_lock.lock().await;
    let repo = cloned.repo.clone();
    let (branch, worktree) = (branch.to_owned(), worktree.to_owned());
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
    .await?
}

/// Queues a prompt; it runs once the turns before it are done, with
/// `model` or else the backend's default.
pub fn send(
    state: &Arc<State>,
    id: &str,
    prompt: String,
    mode: PermissionMode,
    model: Option<String>,
) {
    send_with_images(state, id, prompt, Vec::new(), mode, model);
}

/// Queues a prompt with the images `save_images` saved for it.
pub fn send_with_images(
    state: &Arc<State>,
    id: &str,
    prompt: String,
    images: Vec<String>,
    mode: PermissionMode,
    model: Option<String>,
) {
    let start = state.agents.with_live(id, |l| {
        l.queue.push_back(Queued {
            prompt,
            images,
            mode,
            model,
        });
        !std::mem::replace(&mut l.running, true)
    });
    if start {
        tokio::spawn(run(state.clone(), id.to_owned()));
    }
}

/// Why the agent can't take `images`, if it can't.
pub fn check_images(images: &[PromptImage]) -> Option<String> {
    if images.len() > MAX_IMAGES {
        return Some(format!("a prompt can have at most {MAX_IMAGES} images"));
    }
    for image in images {
        if extension(&image.media_type).is_none() {
            return Some(format!("{} images aren't supported", image.media_type));
        }
        // Base64 takes 4 characters for every 3 bytes.
        if image.data.len() / 4 * 3 > MAX_IMAGE_BYTES {
            return Some(format!(
                "images can be at most {} MB",
                MAX_IMAGE_BYTES / 1024 / 1024
            ));
        }
    }
    None
}

/// The extension images of `media_type` are stored with.
fn extension(media_type: &str) -> Option<&'static str> {
    IMAGE_TYPES
        .iter()
        .find(|(t, _)| *t == media_type)
        .map(|(_, ext)| *ext)
}

/// The media type of the image stored as `name`.
pub(crate) fn media_type(name: &str) -> Option<&'static str> {
    let (_, ext) = name.rsplit_once('.')?;
    IMAGE_TYPES.iter().find(|(_, e)| *e == ext).map(|(t, _)| *t)
}

/// Stores the images sent with one of the session's prompts, which
/// `check_images` took, and returns their names.
pub async fn save_images(state: &State, id: &str, images: &[PromptImage]) -> Result<Vec<String>> {
    if images.is_empty() {
        return Ok(Vec::new());
    }
    let dir = state.agents.images.join(id);
    tokio::fs::create_dir_all(&dir).await?;
    let mut names = Vec::new();
    for image in images {
        let ext = extension(&image.media_type)
            .ok_or_else(|| anyhow!("{} images aren't supported", image.media_type))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .context("the image isn't valid base64")?;
        let name = format!("{}.{ext}", random_hex(16));
        tokio::fs::write(dir.join(&name), bytes).await?;
        names.push(name);
    }
    Ok(names)
}

/// The file of the session's image `name` and its media type, if `name`
/// could be one of its images.
pub fn image_file(state: &State, id: &str, name: &str) -> Option<(PathBuf, &'static str)> {
    let (stem, _) = name.rsplit_once('.')?;
    if !valid_id(id) || stem.len() != 32 || !stem.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((state.agents.images.join(id).join(name), media_type(name)?))
}

/// Removes the images of sessions that are gone, e.g. those of sessions
/// deleted while the server was down.
pub fn remove_orphaned_images(db: &crate::db::Db, images: &Path) -> Result<()> {
    let Ok(dirs) = std::fs::read_dir(images) else {
        return Ok(());
    };
    let owned: std::collections::BTreeSet<String> = db
        .agents_unchecked()?
        .into_iter()
        .map(|(s, _)| s.id)
        .collect();
    for dir in dirs.flatten() {
        let name = dir.file_name().to_string_lossy().into_owned();
        if valid_id(&name) && !owned.contains(&name) {
            remove_tree(&dir.path())?;
        }
    }
    Ok(())
}

/// Stops the current turn and drops the queued prompts.
pub fn interrupt(state: &State, id: &str) {
    let (stopper, pending) = state.agents.with_live(id, |l| {
        l.queue.clear();
        l.interrupted = l.running;
        (l.stopper.clone(), std::mem::take(&mut l.pending))
    });
    if let Some(stopper) = stopper {
        stopper.stop();
    }
    for (tool_use, pending) in pending {
        let _ = pending.tx.send(Decision {
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
/// `options` are the answers the backend offers beyond allow/deny.
pub fn request_approval(
    state: &State,
    id: &str,
    tool_use: &str,
    tool_name: &str,
    input: &Value,
    options: Vec<ApprovalOption>,
) -> Result<oneshot::Receiver<Decision>> {
    let (tx, rx) = oneshot::channel();
    state.agents.with_live(id, |l| {
        l.pending.insert(
            tool_use.to_owned(),
            PendingRequest {
                tx,
                options: options.clone(),
            },
        );
    });
    emit(
        state,
        id,
        AgentEventKind::ApprovalRequested {
            id: tool_use.to_owned(),
            tool_name: tool_name.to_owned(),
            input: input.to_string(),
            options,
        },
    )?;
    set_status(state, id, AgentStatus::AwaitingApproval);
    Ok(rx)
}

/// The answer with id `option` that the waiting permission request
/// `tool_use` offers, if it offers one.
pub fn offered_option(
    state: &State,
    id: &str,
    tool_use: &str,
    option: &str,
) -> Option<ApprovalOption> {
    state.agents.with_live(id, |l| {
        l.pending
            .get(tool_use)?
            .options
            .iter()
            .find(|o| o.id == option)
            .cloned()
    })
}

/// Queues `prompt` next, before the ones already waiting, with the
/// running turn's mode and model unless the follow-up plans again.
pub fn queue_next(state: &State, id: &str, prompt: String, plan: bool) {
    state.agents.with_live(id, |l| {
        // An interrupted turn doesn't send the denial's feedback again.
        if l.interrupted {
            return;
        }
        let (mode, model) = l
            .turn
            .clone()
            .unwrap_or_else(|| (PermissionMode::plan(), None));
        l.queue.push_front(Queued {
            prompt,
            images: Vec::new(),
            mode: if plan { PermissionMode::plan() } else { mode },
            model,
        });
    });
}

/// Forgets a permission request when its waiter stops waiting.
struct PendingGuard<'a> {
    state: &'a State,
    id: &'a str,
    tool_use: &'a str,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.state
            .agents
            .with_live(self.id, |l| l.pending.remove(self.tool_use));
    }
}

/// Asks the user about the tool use `tool_use`, as `request_approval`
/// does, and waits for the answer; `None` when the request couldn't be
/// made or the server shuts down first. However the wait ends, including
/// when it's dropped with the turn or the connection that waits, the
/// request is forgotten, so it no longer counts as pending.
pub async fn await_approval(
    state: &State,
    id: &str,
    tool_use: &str,
    tool_name: &str,
    input: &Value,
    options: Vec<ApprovalOption>,
) -> Option<Decision> {
    let rx = match request_approval(state, id, tool_use, tool_name, input, options) {
        Ok(rx) => rx,
        Err(e) => {
            tracing::warn!("agent {id}: {e:#}");
            return None;
        }
    };
    let _guard = PendingGuard {
        state,
        id,
        tool_use,
    };
    tokio::select! {
        d = rx => d.ok(),
        _ = state.shutdown.cancelled() => None,
    }
}

/// Passes the user's answer on to the waiting permission request. Returns
/// false when no such request is waiting.
pub fn approve(state: &State, id: &str, tool_use: &str, decision: Decision) -> Result<bool> {
    let (tx, still_pending) = state.agents.with_live(id, |l| {
        let tx = l.pending.remove(tool_use).map(|p| p.tx);
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

/// Stops the session and removes it along with its worktree and branch, or
/// a general session's directory. Returns false when there is no such
/// session. A session whose backend this build doesn't know goes too, but
/// isn't scanned.
pub async fn delete(state: &Arc<State>, id: &str) -> Result<bool> {
    let Some((session, known)) = state.db.agent_unchecked(id)? else {
        return Ok(false);
    };
    // Without its clone the worktree and branch would stay behind with
    // nothing left to find them by.
    let general = session.repo == GENERAL;
    let cloned = state.repos.get(&session.repo).cloned();
    if !general && cloned.is_none() && Path::new(&session.worktree).exists() {
        bail!(
            "{repo} isn't configured; restart the server with --repo for {repo} to delete this session",
            repo = session.repo
        );
    }
    // The transcript goes with the session; the scan reads it after.
    let events = state.db.agent_events(id)?;
    let scanned = session.clone();
    interrupt(state, id);
    // The worktree's processes that have a terminal are left alone below,
    // but these shells go with the session.
    state.agents.terminals.close_all(id).await;
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
    } else if general {
        let dir = PathBuf::from(&session.worktree);
        match tokio::task::spawn_blocking(move || remove_tree(&dir)).await? {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(anyhow!(e).context(format!("can't remove {}", session.worktree)));
            }
            _ => {}
        }
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
    match tokio::fs::remove_dir_all(state.agents.images.join(id)).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            tracing::warn!("can't remove the images of agent {id}: {e}");
        }
        _ => {}
    }
    state.db.delete_agent(id)?;
    state.agents.live.lock().unwrap().remove(id);
    state.agents.terminals.forget(id);
    state.send(ServerEvent::AgentsChanged);
    if known {
        crate::cleanup::scan(state, scanned, events);
    }
    Ok(true)
}

/// Runs the queued prompts one turn at a time until the queue is empty,
/// then goes on with a wrap-up that waited for them.
async fn run(state: Arc<State>, id: String) {
    let mut last = AgentStatus::Idle;
    loop {
        // Only a wrap-up that waited for this task is the task's to go on
        // with; one that comes once it stops running is wrap_up's.
        let (prompt, waiting) = state.agents.with_live(&id, |l| {
            let prompt = l.queue.pop_front();
            l.running = prompt.is_some();
            l.interrupted = false;
            l.turn = prompt.as_ref().map(|q| (q.mode.clone(), q.model.clone()));
            (prompt, l.wrap_up.is_some())
        });
        let Some(queued) = prompt else {
            if !waiting {
                return;
            }
            if last == AgentStatus::Idle {
                continue_wrap_up(&state, &id).await;
            } else if state.agents.with_live(&id, |l| l.wrap_up.take()).is_some() {
                let agent = match state.db.agent(&id) {
                    Ok(Some(s)) => s.backend.agent_name(),
                    _ => AgentBackend::default().agent_name(),
                };
                let _ = emit(
                    &state,
                    &id,
                    AgentEventKind::WrapUpFailed(format!("{agent}'s turn didn't finish")),
                );
            }
            return;
        };
        let status = match turn(&state, &id, queued).await {
            Ok(status) => status,
            Err(e) => {
                tracing::warn!("agent {id}: {e:#}");
                let _ = emit(&state, &id, AgentEventKind::Error(format!("{e:#}")));
                AgentStatus::Failed
            }
        };
        let interrupted = state.agents.with_live(&id, |l| {
            l.stopper = None;
            l.turn = None;
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

/// Starts landing the session's work, after the turns already queued.
/// Returns false when the session is busy with another wrap-up.
pub async fn wrap_up(state: &Arc<State>, id: &str, req: WrapUp) -> bool {
    let start = state.agents.with_live(id, |l| {
        if l.wrap_up.is_some() {
            return None;
        }
        l.wrap_up = Some(Pending {
            target: req.target,
            mode: req.permission_mode,
            model: req.model,
            asked: false,
        });
        // A running task goes on with the wrap-up once its turns are done.
        Some(!l.running)
    });
    match start {
        None => false,
        Some(start) => {
            if start {
                continue_wrap_up(state, id).await;
            }
            true
        }
    }
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
        Landed::PullRequest { .. } | Landed::Stack { .. } => {
            if let Some(session) = state.db.agent(id)? {
                for pr in landed.prs() {
                    let key = PrKey {
                        repo: session.repo.clone(),
                        number: pr.number,
                    };
                    state.db.add_agent_pr(id, &key, "OPEN")?;
                }
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
    if session.repo == GENERAL {
        bail!("a general session isn't in a repository, so it has nothing to land");
    }
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
    // The PRs an earlier wrap-up opened stay the bottom of the stack.
    let opened = session.landed.as_ref().map(Landed::prs).unwrap_or_default();
    let count = opened.len();
    let step = {
        let _guard = cloned.git_lock.lock().await;
        let repo = cloned.repo.clone();
        let session = session.clone();
        tokio::task::spawn_blocking(move || match target {
            WrapUpTarget::Checkout => land_on_checkout(&repo, &session),
            WrapUpTarget::PullRequest => push_stack(&repo, &session, pr_branch, count),
        })
        .await??
    };
    let Step::Pushed { base, commits } = step else {
        return Ok(step);
    };
    // The push updated the PRs already open; each new commit gets a PR
    // into the branch of the commit below it.
    let mut prs = opened;
    let before = prs.len();
    let mut below = base;
    for (i, (branch, message)) in commits.into_iter().enumerate() {
        if i >= prs.len() {
            let (title, body) = pr_text(&message);
            match state
                .github
                .create_pr(&session.repo, &branch, &below, &title, &body)
                .await
            {
                Ok((number, url)) => prs.push(LandedPr { number, url }),
                Err(e) => {
                    // The next wrap-up goes on from the PRs opened so far.
                    if prs.len() > before {
                        let numbers = prs
                            .iter()
                            .map(|pr| format!("#{}", pr.number))
                            .collect::<Vec<_>>()
                            .join(", ");
                        record_landed(state, id, &Landed::from_prs(prs))?;
                        return Err(e.context(format!("opened only {numbers}")));
                    }
                    return Err(e);
                }
            }
        }
        below = branch;
    }
    Ok(Step::Landed(Landed::from_prs(prs)))
}

/// The branch the `i`th commit of a session's stack, from 0, is pushed to:
/// the session's own branch for the bottom one.
fn stack_branch(branch: &str, i: usize) -> String {
    match i {
        0 => branch.to_owned(),
        _ => format!("{branch}-{}", i + 1),
    }
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

/// Pushes each of the session's commits to GitHub on its own branch. The
/// bottom PR goes into `pr_branch`, or the default branch. `opened` PRs are
/// open already, one for each of the bottom commits.
fn push_stack(
    repo: &Repo,
    session: &AgentSession,
    pr_branch: Option<String>,
    opened: usize,
) -> Result<Step> {
    let worktree = Path::new(&session.worktree);
    if let Some(reason) = repo.worktree_unfinished(worktree)? {
        return Ok(Step::Ask {
            reason,
            prompt: PR_COMMIT_PROMPT.into(),
        });
    }
    let head = repo.worktree_head(worktree)?;
    let commits = repo.commits(&session.base_sha, &head)?;
    if commits.is_empty() {
        bail!("there are no commits to push");
    }
    if commits.len() < opened {
        bail!(
            "the session has {} commits but opened {opened} PRs, one per commit",
            commits.len()
        );
    }
    let commits: Vec<(String, String, String)> = commits
        .into_iter()
        .enumerate()
        .map(|(i, (sha, message))| (sha, stack_branch(&session.branch, i), message))
        .collect();
    let refs: Vec<(String, String)> = commits
        .iter()
        .map(|(sha, branch, _)| (sha.clone(), branch.clone()))
        .collect();
    repo.push_commits(&refs)?;
    let base = match pr_branch {
        Some(b) => b,
        None => repo.default_branch()?,
    };
    let commits = commits
        .into_iter()
        .map(|(_, branch, message)| (branch, message))
        .collect();
    Ok(Step::Pushed { base, commits })
}

/// What Claude is told about the session's work that wrapping up already
/// landed. Wrapping up runs outside Claude's turns, so without it Claude
/// doesn't know about the PRs it opened.
fn landed_note(session: &AgentSession) -> Option<String> {
    let note = match session.landed.as_ref()? {
        Landed::Checkout { sha } => format!(
            "The user had Grenadine, the app running this session, land this session's commits \
             on their checkout, up to {sha}. Later commits land when the user wraps up again."
        ),
        landed => {
            let prs = landed
                .prs()
                .iter()
                .enumerate()
                .map(|(i, pr)| {
                    let branch = stack_branch(&session.branch, i);
                    format!("- #{} {} from branch `{branch}`", pr.number, pr.url)
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "The user had Grenadine, the app running this session, open these pull requests \
                 from this session's commits, one per commit, each stacked on the one before \
                 it:\n{prs}\nThese are this session's PRs. Don't open other PRs for this work or \
                 push it to other branches. To put new commits on them, ask the user to click \
                 Open PR again: that pushes each commit to its PR's branch and opens a PR for \
                 each new commit."
            )
        }
    };
    Some(note)
}

/// What Claude is told about the session's terminal tabs, when it has any.
fn terminals_note(state: &State, id: &str) -> Option<String> {
    let open = state.agents.terminals.list(id);
    if open.is_empty() {
        return None;
    }
    let names: Vec<_> = open.iter().map(|t| format!("Terminal {}", t.id)).collect();
    Some(format!(
        "The user has terminal tabs open in this session's page: {}. They are login shells in \
         this session's working directory. When you ask the user to run a command in one, call \
         the grenadine MCP server's wait_for_terminal tool to wait for it and read its output, \
         instead of asking them to paste it. read_terminal reads a terminal at any time.",
        names.join(", ")
    ))
}

/// A PR's title and body for a commit with `message`. The body is
/// rewrapped at 72 columns so GitHub's squash merge doesn't wrap it again.
fn pr_text(message: &str) -> (String, String) {
    let (subject, body) = message.split_once('\n').unwrap_or((message, ""));
    (subject.trim().to_owned(), reflow(body.trim(), 72))
}

/// Runs one prompt with the session's backend and returns the status the
/// session ends up in.
async fn turn(state: &Arc<State>, id: &str, queued: Queued) -> Result<AgentStatus> {
    let Queued {
        prompt,
        images,
        mode,
        model,
    } = queued;
    let session = state
        .db
        .agent(id)?
        .ok_or_else(|| anyhow!("the session is gone"))?;
    emit(state, id, AgentEventKind::Prompt(prompt.clone()))?;
    if !images.is_empty() {
        emit(state, id, AgentEventKind::Images(images.clone()))?;
    }
    set_status(state, id, AgentStatus::Running);
    // Nothing of a general session's is lost with its directory, so one
    // that went away comes back.
    if session.repo == GENERAL {
        tokio::fs::create_dir_all(&session.worktree).await?;
    }

    let mcp_url = format!(
        "{}/mcp/{id}/{}",
        state.agents.base_url,
        state.agents.secret(id)
    );
    let notes: Vec<_> = [landed_note(&session), terminals_note(state, id)]
        .into_iter()
        .flatten()
        .collect();
    let sink = TurnSink::new(state, id, &session.repo);
    let turn = Turn {
        prompt: &prompt,
        images: &images,
        mode: &mode,
        model,
        mcp_url: &mcp_url,
        notes: &notes,
    };
    state
        .agents
        .backend(session.backend)
        .turn(state, &session, turn, sink)
        .await
}

/// What a backend's turn feeds its output into: the session's transcript
/// and the bookkeeping that goes with it.
pub struct TurnSink<'a> {
    state: &'a State,
    id: &'a str,
    repo: &'a str,
    /// Whether the turn ended, and successfully.
    ended: Option<bool>,
    /// The cost already added to the session's.
    billed: f64,
    /// Whether the last event shown ended the turn.
    last_was_end: bool,
    prs: OpenedPrs,
}

impl<'a> TurnSink<'a> {
    pub fn new(state: &'a State, id: &'a str, repo: &'a str) -> TurnSink<'a> {
        TurnSink {
            state,
            id,
            repo,
            ended: None,
            billed: 0.0,
            last_was_end: false,
            prs: OpenedPrs::default(),
        }
    }

    /// The backend created (or resumed) its session, which later turns
    /// resume by `backend_session_id`, with `model` if it's known.
    pub fn started(&mut self, backend_session_id: &str, model: Option<String>) -> Result<()> {
        self.state
            .db
            .set_backend_session(self.id, backend_session_id)?;
        // The window is only known once a turn's result reports it; the
        // model's name doesn't tell, since Claude Code leaves `[1m]` off
        // it.
        if let Some(model) = model {
            self.state.db.set_agent_model(self.id, &model)?;
            emit(self.state, self.id, AgentEventKind::Model(model))?;
        }
        Ok(())
    }

    /// Stores `kind` in the transcript the way a turn's events are kept:
    /// images into the database, usage into the session's row, costs
    /// billed and repeated turn ends suppressed.
    pub fn event(&mut self, mut kind: AgentEventKind) -> Result<()> {
        store_images(self.state, self.id, &mut kind)?;
        if let AgentEventKind::Usage { tokens, window } = kind {
            return record_usage(self.state, self.id, tokens, window);
        }
        if let AgentEventKind::TurnEnded { error, cost } = &kind {
            // The cost is the process's total so far, and a process can
            // report several results: one more for each background task
            // that finishes after the answer.
            if let Some(cost) = cost {
                self.state.db.add_agent_cost(self.id, cost - self.billed)?;
                self.billed = *cost;
            }
            let repeat = self.ended == Some(true) && self.last_was_end && error.is_none();
            self.ended = Some(error.is_none());
            if repeat {
                return Ok(());
            }
        }
        self.last_was_end = matches!(kind, AgentEventKind::TurnEnded { .. });
        let opened = self.prs.see(self.repo, &kind);
        emit(self.state, self.id, kind)?;
        record_prs(self.state, self.id, &opened)
    }

    /// How the turn went once the backend's output is done: `unfinished`
    /// explains a turn that never reported an end.
    pub fn finish(self, unfinished: impl FnOnce() -> String) -> Result<AgentStatus> {
        let interrupted = self.state.agents.interrupted(self.id);
        Ok(match self.ended {
            Some(true) => AgentStatus::Idle,
            Some(false) => AgentStatus::Failed,
            None if interrupted => {
                emit(
                    self.state,
                    self.id,
                    AgentEventKind::TurnEnded {
                        error: Some("interrupted".into()),
                        cost: None,
                    },
                )?;
                AgentStatus::Interrupted
            }
            None => {
                emit(self.state, self.id, AgentEventKind::Error(unfinished()))?;
                AgentStatus::Failed
            }
        })
    }
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
            AgentEventKind::Landed(landed) => landed
                .prs()
                .into_iter()
                .map(|pr| PrKey {
                    repo: repo.to_owned(),
                    number: pr.number,
                })
                .collect(),
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

/// Stores an image of session `id`'s transcript and returns its new id.
pub fn store_image(state: &State, id: &str, mime: &str, data: &[u8]) -> Result<String> {
    let image = uuid();
    state.db.add_agent_image(id, &image, mime, data)?;
    Ok(image)
}

/// Moves the images of a tool result from its `data:` URLs into the
/// database, so that the transcript only carries their ids. Images that
/// don't decode are dropped.
fn store_images(state: &State, id: &str, kind: &mut AgentEventKind) -> Result<()> {
    let AgentEventKind::ToolResult { images, .. } = kind else {
        return Ok(());
    };
    let mut stored = Vec::new();
    for url in images.drain(..) {
        let Some((mime, data)) = url
            .strip_prefix("data:")
            .and_then(|u| u.split_once(";base64,"))
        else {
            continue;
        };
        let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data) else {
            continue;
        };
        stored.push(store_image(state, id, mime, &data)?);
    }
    *images = stored;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn uuids() {
        let id = uuid();
        assert!(valid_id(&id), "{id}");
        assert_eq!(&id[14..15], "4");
        assert_ne!(id, uuid());
        assert!(!valid_id("../../etc"));
    }

    /// Prompts carry the backend's mode names as strings; a message
    /// without one is rejected.
    #[test]
    fn permission_modes() {
        let msg: grenadine_core::api::AgentMessage =
            serde_json::from_str(r#"{"prompt":"hi","permission_mode":"auto"}"#).unwrap();
        assert_eq!(msg.permission_mode.as_str(), "auto");
        assert_eq!(serde_json::to_value(&msg.permission_mode).unwrap(), "auto");
        assert!(
            serde_json::from_str::<grenadine_core::api::AgentMessage>(r#"{"prompt":"hi"}"#)
                .is_err()
        );
    }

    #[test]
    fn titles() {
        assert_eq!(title("\n  Fix the bug  \nin detail"), "Fix the bug");
        assert_eq!(title(&"é".repeat(100)), format!("{}…", "é".repeat(80)));
        assert_eq!(title(""), "");
    }

    /// A state whose agents run `script` as claude, with the git fixture's
    /// clone configured.
    pub(crate) fn state_with_claude(
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
            "devin".into(),
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

    /// A prompt's images go to claude as content blocks of a stream-json
    /// message, stay for the transcript and go with the session.
    #[tokio::test]
    async fn sends_images() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> {log}
cat >> {log}
echo '{{"type":"system","subtype":"init","session_id":"s","model":"claude-test"}}'
echo '{{"type":"result","subtype":"success","is_error":false,"result":"ok"}}'
"#,
            log = log.display()
        );
        let state = state_with_claude(&fx, dir.path(), &script);
        let session = start(&state, "plain text").await;
        let id = &session.id;
        std::fs::remove_file(&log).unwrap();

        let png = PromptImage {
            media_type: "image/png".into(),
            data: "iVBORw0K".into(),
        };
        assert_eq!(check_images(std::slice::from_ref(&png)), None);
        let bmp = PromptImage {
            media_type: "image/bmp".into(),
            ..png.clone()
        };
        assert!(check_images(&[bmp]).is_some());
        let names = save_images(&state, id, &[png]).await.unwrap();
        send_with_images(
            &state,
            id,
            "what's this?".into(),
            names.clone(),
            PermissionMode::from("auto"),
            None,
        );
        let events = wait_for_turns(&state, id, 2).await;
        assert!(
            events.contains(&AgentEventKind::Images(names.clone())),
            "{events:?}"
        );

        let log = std::fs::read_to_string(&log).unwrap();
        let (args, input) = log.split_once('\n').unwrap();
        assert!(args.contains("--input-format stream-json"), "{args}");
        let message: Value = serde_json::from_str(input).unwrap();
        assert_eq!(
            message["message"]["content"],
            json!([
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0K"}},
                {"type": "text", "text": "what's this?"},
            ])
        );

        let (file, media_type) = image_file(&state, id, &names[0]).unwrap();
        assert_eq!(media_type, "image/png");
        assert_eq!(
            std::fs::read(&file).unwrap(),
            [0x89, b'P', b'N', b'G', b'\r', b'\n']
        );
        assert!(image_file(&state, id, "../../etc/passwd").is_none());
        assert!(delete(&state, id).await.unwrap());
        assert!(!file.exists());
    }

    /// Reads the prompt's text from the stream-json message on stdin into
    /// `$prompt`, for fake claudes in shell. Only for text without quotes,
    /// backslashes or newlines, which JSON would escape.
    const READ_PROMPT: &str = r#"prompt=$(sed -n 's/.*"text":"\([^"]*\)".*/\1/p')"#;

    #[tokio::test]
    async fn runs_turns_in_a_worktree_and_resumes() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let script = format!(
            r#"#!/bin/sh
{READ_PROMPT}
echo "$* | $prompt | $(cat a)" >> {log}
echo '{{"type":"system","subtype":"init","session_id":"s","model":"claude-test"}}'
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"Hi"}}}},"parent_tool_use_id":null}}'
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Hi there"}}],"usage":{{"input_tokens":10,"output_tokens":5}}}},"parent_tool_use_id":null}}'
echo '{{"type":"result","subtype":"success","is_error":false,"result":"Hi there","total_cost_usd":0.25,"modelUsage":{{"claude-test":{{"contextWindow":200000}}}}}}'
"#,
            log = log.display()
        );
        let state = state_with_claude(&fx, dir.path(), &script);

        let session = create(
            &state,
            NewAgent {
                backend: AgentBackend::Claude,
                repo: "owner/name".into(),
                pr: None,
                prompt: "-first".into(),
                permission_mode: PermissionMode::from("auto"),
                model: Some("sonnet".into()),
                images: Vec::new(),
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
            PermissionMode::from("manual"),
            None,
        );
        let transcript = wait_for_turns(&state, &session.id, 2).await;

        let turn = |prompt: &str| {
            [
                AgentEventKind::Prompt(prompt.into()),
                AgentEventKind::Text("Hi there".into()),
                AgentEventKind::TurnEnded {
                    error: None,
                    cost: Some(0.25),
                },
            ]
        };
        assert_eq!(transcript, [turn("-first"), turn("second")].concat());
        let session = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(session.status, AgentStatus::Idle);
        assert_eq!(session.cost, 0.5);
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
        assert!(
            lines[1].contains(
                " --allowedTools mcp__grenadine__list_terminals mcp__grenadine__read_terminal \
                 mcp__grenadine__wait_for_terminal mcp__grenadine__show_image --mcp-config "
            ),
            "{log}"
        );
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

    /// Background tasks finishing after the answer make claude report more
    /// results, each with the process's cost so far. The session shows one
    /// end per answer and is charged the total once.
    #[tokio::test]
    async fn shows_repeated_results_once() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let script = r#"#!/bin/sh
cat > /dev/null
echo '{"type":"system","subtype":"init","session_id":"s","model":"claude-test"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"First"}]},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.25}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Second"}]},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.5}'
echo '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.5}'
echo '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.5}'
"#;
        let state = state_with_claude(&fx, dir.path(), script);

        let session = create(
            &state,
            NewAgent {
                backend: AgentBackend::Claude,
                repo: "owner/name".into(),
                pr: None,
                prompt: "go".into(),
                permission_mode: PermissionMode::from("auto"),
                model: None,
                images: Vec::new(),
            },
        )
        .await
        .unwrap();
        let transcript = wait_for_turns(&state, &session.id, 2).await;

        let ended = |cost| AgentEventKind::TurnEnded {
            error: None,
            cost: Some(cost),
        };
        assert_eq!(
            transcript,
            [
                AgentEventKind::Prompt("go".into()),
                AgentEventKind::Text("First".into()),
                ended(0.25),
                AgentEventKind::Text("Second".into()),
                ended(0.5),
            ]
        );
        let session = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(session.status, AgentStatus::Idle);
        assert_eq!(session.cost, 0.5);
    }

    /// A general session runs in an empty directory of its own, outside any
    /// clone, has nothing to land, and takes its directory when deleted.
    #[tokio::test]
    async fn runs_a_general_session_in_a_plain_directory() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let script = format!(
            r#"#!/bin/sh
cat > /dev/null
echo "$(pwd) $(ls -A | wc -l) $(git rev-parse --is-inside-work-tree 2>/dev/null || echo none)" >> {log}
echo '{{"type":"result","subtype":"success","is_error":false,"result":"","total_cost_usd":0.25}}'
"#,
            log = log.display()
        );
        let state = state_with_claude(&fx, dir.path(), &script);
        let new = |pr| NewAgent {
            backend: AgentBackend::Claude,
            repo: GENERAL.into(),
            pr,
            prompt: "what's a monad?".into(),
            permission_mode: PermissionMode::from("auto"),
            model: None,
            images: Vec::new(),
        };
        let err = create(&state, new(Some(1))).await.unwrap_err().to_string();
        assert!(err.contains("can't start from a PR"), "{err}");

        let session = create(&state, new(None)).await.unwrap();
        assert_eq!(
            (session.branch.as_str(), session.base_sha.as_str()),
            ("", "")
        );
        let worktree = PathBuf::from(&session.worktree);
        assert_eq!(
            worktree,
            state.agents.worktrees.join(GENERAL).join(&session.id)
        );
        wait_for_turns(&state, &session.id, 1).await;
        let log = std::fs::read_to_string(&log).unwrap();
        let cwd = worktree.canonicalize().unwrap();
        assert_eq!(log.trim(), format!("{} 0 none", cwd.display()));

        let err = land(&state, &session.id, WrapUpTarget::Checkout)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing to land"), "{err}");

        assert!(delete(&state, &session.id).await.unwrap());
        assert!(!worktree.exists());
        assert_eq!(state.db.agent(&session.id).unwrap(), None);
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
                backend: AgentBackend::Claude,
                repo: "owner/name".into(),
                pr: None,
                prompt: "please write a note\nin notes.txt".into(),
                permission_mode: PermissionMode::from("auto"),
                model: None,
                images: Vec::new(),
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
                backend: AgentBackend::Claude,
                repo: "owner/name".into(),
                pr: None,
                prompt: "Run the e2e tests".into(),
                permission_mode: PermissionMode::from("auto"),
                model: None,
                images: Vec::new(),
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
                backend: AgentBackend::Claude,
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
                backend: AgentBackend::Claude,
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
pwd > {dir}/prune_cwd
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
            title: "Run the e2e tests".into(),
            branch: "b".into(),
            worktree: "/w".into(),
            base_sha: "s".into(),
            ..AgentSession::fixture("gone", "owner/name")
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
        assert!(args.contains(" --setting-sources user "), "{args}");
        assert!(args.contains(" Bash(gh pr view:*) "), "{args}");
        // In the repo's clone, so that git and gh find the repo.
        let cwd = std::fs::read_to_string(dir.path().join("prune_cwd")).unwrap();
        assert_eq!(
            std::fs::canonicalize(cwd.trim()).unwrap(),
            std::fs::canonicalize(&fx.clone.path).unwrap()
        );
        let input = std::fs::read_to_string(dir.path().join("prune_input")).unwrap();
        assert!(
            input.contains("The working directory is the clone of owner/name"),
            "{input}"
        );
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
                backend: AgentBackend::Claude,
                repo: "owner/name".into(),
                pr: None,
                prompt: "build".into(),
                permission_mode: PermissionMode::from("auto"),
                model: None,
                images: Vec::new(),
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
            title: "t".into(),
            branch: session_branch(id),
            worktree: worktree.to_string_lossy().into_owned(),
            base_sha: "0".repeat(40),
            created_at: now(),
            ..AgentSession::fixture(id, repo)
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

        // One of a backend this build doesn't know can be deleted too.
        let unknown = uuid();
        state
            .db
            .create_agent(&session_row(&unknown, GENERAL, &dir.path().join("u")))
            .unwrap();
        state.db.set_agent_backend_raw(&unknown, "devin", "acu");
        assert!(delete(&state, &unknown).await.unwrap());
        assert!(state.db.agent_unchecked(&unknown).unwrap().is_none());
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
        // A session of a backend this build doesn't know still owns its
        // worktree.
        db.set_agent_backend_raw(&owned, "devin", "acu");
        let elsewhere = dir.path().join("elsewhere");
        fx.clone
            .worktree_add(&elsewhere, "grenadine/agent/other", "HEAD")
            .unwrap();
        std::fs::create_dir_all(worktrees.join("owner/empty")).unwrap();
        // General sessions' directories aren't worktrees, and an owned one
        // stays even when empty.
        let (general, general_orphan) = (uuid(), uuid());
        let general_dir = worktrees.join(GENERAL);
        for id in [&general, &general_orphan] {
            std::fs::create_dir_all(general_dir.join(id)).unwrap();
        }
        std::fs::write(general_dir.join(&general_orphan).join("f"), "").unwrap();
        db.create_agent(&session_row(&general, GENERAL, &general_dir.join(&general)))
            .unwrap();

        remove_orphaned_worktrees(&db, &repos, &worktrees).unwrap();
        assert!(general_dir.join(&general).exists());
        assert!(!general_dir.join(&general_orphan).exists());
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
                backend: AgentBackend::Claude,
                repo: "owner/name".into(),
                pr: None,
                prompt: "go".into(),
                permission_mode: PermissionMode::from("auto"),
                model: None,
                images: Vec::new(),
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
        assert_eq!(state.db.backend_session(&session.id).unwrap(), None);
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
                backend: AgentBackend::Claude,
                repo: "owner/name".into(),
                pr: None,
                prompt: "go".into(),
                permission_mode: PermissionMode::from("auto"),
                model: None,
                images: Vec::new(),
            },
        )
        .await
        .unwrap();
        send(
            &state,
            &session.id,
            "queued".into(),
            PermissionMode::from("auto"),
            None,
        );
        for _ in 0..100 {
            if state.agents.with_live(&session.id, |l| l.stopper.is_some()) {
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
            pr_text("Fix it\n\nBecause."),
            ("Fix it".into(), "Because.".into())
        );
        assert_eq!(pr_text("Two"), ("Two".into(), "".into()));
        let long = "word ".repeat(15);
        assert_eq!(
            pr_text(&format!("Three\n\n{}", long.trim_end())),
            (
                "Three".into(),
                format!("{}\nword", "word ".repeat(14).trim_end())
            )
        );
    }

    #[test]
    fn stack_branches() {
        assert_eq!(stack_branch("grenadine/agent/x", 0), "grenadine/agent/x");
        assert_eq!(stack_branch("grenadine/agent/x", 1), "grenadine/agent/x-2");
        assert_eq!(stack_branch("grenadine/agent/x", 2), "grenadine/agent/x-3");
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
            images: Vec::new(),
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
    /// other prompts. Prompts are logged to `log`, and the arguments to
    /// `log`.args.
    fn wrap_up_claude(log: &Path) -> String {
        format!(
            r#"#!/bin/sh
echo "$@" >> {log}.args
{READ_PROMPT}
echo "$prompt" >> {log}
set -- $prompt
case "$1" in
  write) echo "$3" > "$2" ;;
  commit) echo "$3" > "$2" && git add -A && git commit -qm "Change $2" ;;
  Wrap) case "$prompt" in "{COMMIT_PROMPT}"|"{PR_COMMIT_PROMPT}") git add -A && git commit -qm "Wrapped up" ;; esac ;;
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
                backend: AgentBackend::Claude,
                repo: "owner/name".into(),
                pr: None,
                prompt: prompt.into(),
                permission_mode: PermissionMode::from("auto"),
                model: None,
                images: Vec::new(),
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
            permission_mode: PermissionMode::from("auto"),
            model: None,
        };
        assert!(wrap_up(state, id, req).await);
        wrapped(state, id).await
    }

    /// Waits for the session's wrap-up and returns how it ended.
    async fn wrapped(state: &Arc<State>, id: &str) -> AgentEventKind {
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
    async fn wraps_up_after_the_running_turn() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, clone) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;

        // The wrap-up waits for the turn and lands what it committed.
        send(
            &state,
            id,
            "commit b more".into(),
            PermissionMode::from("auto"),
            None,
        );
        let req = || WrapUp {
            target: WrapUpTarget::Checkout,
            permission_mode: PermissionMode::from("auto"),
            model: None,
        };
        assert!(wrap_up(&state, id, req()).await);
        // One wrap-up at a time.
        assert!(!wrap_up(&state, id, req()).await);
        let AgentEventKind::Landed(Landed::Checkout { sha }) = wrapped(&state, id).await else {
            panic!("{:?}", state.db.agent_events(id));
        };
        assert_eq!(run(&clone, &["rev-parse", "HEAD"]), sha);
        assert_eq!(std::fs::read_to_string(clone.join("b")).unwrap(), "more\n");
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(!log.contains(COMMIT_PROMPT), "{log}");
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
            PermissionMode::from("auto"),
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

    #[tokio::test]
    async fn pushes_a_branch_per_commit() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;
        send(
            &state,
            id,
            "commit c more".into(),
            PermissionMode::from("auto"),
            None,
        );
        // What's left uncommitted becomes a third commit.
        send(
            &state,
            id,
            "write d last".into(),
            PermissionMode::from("auto"),
            None,
        );
        wait_for_turns(&state, id, 3).await;

        // The test state's GitHub is unreachable, so only the push works.
        let event = wrap(&state, id, WrapUpTarget::PullRequest).await;
        assert!(
            matches!(event, AgentEventKind::WrapUpFailed(_)),
            "{event:?}"
        );
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(log.contains(PR_COMMIT_PROMPT), "{log}");
        let worktree = Path::new(&session.worktree);
        let tip = |branch: &str| {
            run(
                &fx.upstream,
                &["rev-parse", &format!("refs/heads/{branch}")],
            )
        };
        let branch = &session.branch;
        assert_eq!(tip(branch), run(worktree, &["rev-parse", "HEAD~2"]));
        assert_eq!(
            tip(&format!("{branch}-2")),
            run(worktree, &["rev-parse", "HEAD~"])
        );
        assert_eq!(
            tip(&format!("{branch}-3")),
            run(worktree, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn notes_what_landed() {
        let pr = |number| LandedPr {
            number,
            url: format!("https://github.com/o/n/pull/{number}"),
        };
        let session = |landed| AgentSession {
            branch: "grenadine/agent/abc".into(),
            landed,
            ..session_row("abcdef01-2345", "o/n", Path::new("/w"))
        };
        assert_eq!(landed_note(&session(None)), None);

        let note = landed_note(&session(Some(Landed::Checkout { sha: "f00".into() }))).unwrap();
        assert!(note.contains("checkout, up to f00"), "{note}");

        let note = landed_note(&session(Some(Landed::from_prs(vec![pr(4)])))).unwrap();
        assert!(
            note.contains("- #4 https://github.com/o/n/pull/4 from branch `grenadine/agent/abc`\n"),
            "{note}"
        );

        let note = landed_note(&session(Some(Landed::from_prs(vec![pr(4), pr(5)])))).unwrap();
        assert!(
            note.contains("#4 https://github.com/o/n/pull/4 from branch `grenadine/agent/abc`\n")
        );
        assert!(
            note.contains("#5 https://github.com/o/n/pull/5 from branch `grenadine/agent/abc-2`\n"),
            "{note}"
        );
    }

    #[tokio::test]
    async fn tells_claude_about_open_terminals() {
        let mut state = crate::sync::test_state();
        Arc::get_mut(&mut state).unwrap().agents.terminals =
            crate::terminal::Terminals::new("/bin/sh".into());
        let dir = tempfile::tempdir().unwrap();
        let session = session_row(&uuid(), "o/n", dir.path());
        assert_eq!(terminals_note(&state, &session.id), None);
        crate::terminal::open(&state, &session).unwrap();
        crate::terminal::open(&state, &session).unwrap();
        let note = terminals_note(&state, &session.id).unwrap();
        assert!(
            note.contains("this session's page: Terminal 1, Terminal 2."),
            "{note}"
        );
        state.agents.terminals.close_all(&session.id).await;
    }

    #[tokio::test]
    async fn tells_claude_about_the_prs_it_opened() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;
        let args = || std::fs::read_to_string(dir.path().join("log.args")).unwrap();
        assert!(!args().contains("--append-system-prompt"), "{}", args());

        // The test state's GitHub is unreachable, so the PR is recorded as
        // wrapping up would have.
        let landed = Landed::PullRequest {
            number: 24,
            url: "https://github.com/owner/name/pull/24".into(),
        };
        record_landed(&state, id, &landed).unwrap();
        send(
            &state,
            id,
            "what PR?".into(),
            PermissionMode::from("auto"),
            None,
        );
        wait_for_turns(&state, id, 2).await;
        let args = args();
        let last = args
            .split("--append-system-prompt")
            .nth(1)
            .unwrap_or_default();
        assert!(
            last.contains(&format!(
                "#24 https://github.com/owner/name/pull/24 from branch `{}`",
                session.branch
            )),
            "{args}"
        );
    }
}

#[cfg(test)]
mod devin_tests {
    use std::os::unix::fs::PermissionsExt as _;

    use grenadine_core::api::NewAgent;

    use super::*;

    /// A state whose agents run `script` as devin.
    fn state_with_devin(fx: &crate::git::tests::Fixture, dir: &Path, script: &str) -> Arc<State> {
        let devin = dir.join("devin");
        std::fs::write(&devin, script).unwrap();
        std::fs::set_permissions(&devin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        crate::sync::test_state_with_agents(
            [(fx.clone.slug.clone(), cloned)].into_iter().collect(),
            "claude".into(),
            devin,
            dir.join("worktrees"),
        )
    }

    /// A fake `devin acp` that answers the handshake and one prompt with
    /// a text answer, asking permission for an `exec` call first. Every
    /// client message is logged to `log`.
    fn fake_devin(dir: &Path, session: &str) -> String {
        fake_devin_slow(dir, session, "0")
    }

    /// `fake_devin`, but `delay` seconds slow to answer `session/new`.
    fn fake_devin_slow(dir: &Path, session: &str, delay: &str) -> String {
        format!(
            r#"#!/bin/sh
log={log}
while IFS= read -r line; do
  echo "$line" >> "$log"
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  case "$line" in
  *'"initialize"'*) echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"protocolVersion\":1}}}}" ;;
  *'"session/new"'*) sleep {delay}; echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"sessionId\":\"{session}\",\"configOptions\":[{{\"id\":\"model\",\"currentValue\":\"swe-2\",\"options\":[]}}]}}}}" ;;
  *'"session/load"'*)
    echo '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"replayed"}}}}}}}}'
    echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"configOptions\":[{{\"id\":\"model\",\"currentValue\":\"swe-2\"}}]}}}}" ;;
  *'"session/set_mode"'* | *'"session/set_config_option"'*) echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"configOptions\":[{{\"id\":\"model\",\"currentValue\":\"swe-2\"}}]}}}}" ;;
  *'"session/prompt"'*)
    echo '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"Hi"}}}}}}}}'
    echo '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"tool_call","toolCallId":"exec:0#t","rawInput":{{"command":"touch f"}},"_meta":{{"cognition.ai/inferenceToolName":"exec"}}}}}}}}'
    echo '{{"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{{"sessionId":"{session}","toolCall":{{"toolCallId":"exec:0#t"}},"options":[{{"optionId":"allow_once","name":"Allow","kind":"allow_once"}},{{"optionId":"reject_once","name":"Reject","kind":"reject_once"}}]}}}}'
    ;;
  *'"id":"perm-1"'*)
    echo '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"tool_call_update","toolCallId":"exec:0#t","status":"completed","content":[{{"type":"content","content":{{"type":"text","text":"done"}}}}]}}}}}}'
    echo '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"usage_update","used":100,"size":200000,"_meta":{{"cognition.ai/totalAcuCost":0.25}}}}}}}}'
    echo "{{\"jsonrpc\":\"2.0\",\"id\":$pid,\"result\":{{\"stopReason\":\"end_turn\"}}}}"
    ;;
  *'"session/cancel"'*)
    echo "{{\"jsonrpc\":\"2.0\",\"id\":$pid,\"result\":{{\"stopReason\":\"cancelled\"}}}}"
    ;;
  esac
  case "$line" in *'"session/prompt"'*) pid=$id;; esac
done
"#,
            log = dir.join("devin_log").display(),
            session = session,
            delay = delay,
        )
    }

    /// Waits until the session has finished `turns` turns.
    async fn wait_for_turns(state: &State, id: &str, turns: usize) {
        for _ in 0..100 {
            let ended = state
                .db
                .agent_events(id)
                .unwrap()
                .iter()
                .filter(|e| {
                    matches!(
                        e.kind,
                        AgentEventKind::TurnEnded { .. } | AgentEventKind::Error(_)
                    )
                })
                .count();
            let running = state.agents.with_live(id, |l| l.running);
            if ended >= turns && !running {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("{id} didn't finish its turns");
    }

    /// Waits until the session's status is `AwaitingApproval`.
    async fn wait_for_status(state: &State, id: &str) {
        for _ in 0..200 {
            if state.db.agent(id).unwrap().unwrap().status == AgentStatus::AwaitingApproval {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{id} never waited for approval");
    }

    /// A devin that can't list its models leaves Devin out of the
    /// backends, but prompts with its default or any model that can't
    /// pass for a flag still go, and devin isn't asked again for a while.
    #[tokio::test]
    async fn a_failed_listing_is_kept() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let script = format!("#!/bin/sh\necho run >> {}\nexit 1\n", runs.display());
        let state = state_with_devin(&fx, dir.path(), &script);
        let backends: Vec<_> = state
            .agents
            .backends()
            .await
            .into_iter()
            .map(|i| i.backend)
            .collect();
        assert_eq!(backends, [AgentBackend::Claude]);
        assert!(state.agents.offers(AgentBackend::Devin, None).await);
        assert!(
            state
                .agents
                .offers(AgentBackend::Devin, Some("swe-2"))
                .await
        );
        assert!(
            !state
                .agents
                .offers(AgentBackend::Devin, Some("--help"))
                .await
        );
        assert_eq!(std::fs::read_to_string(&runs).unwrap(), "run\n");
    }

    /// A Devin session's turn goes over ACP: the session id is stored,
    /// its permission request waits on the page, and the turn's cost is
    /// in ACUs.
    #[tokio::test]
    async fn a_devin_turn_runs_and_resumes() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_devin(&fx, dir.path(), &fake_devin(dir.path(), "turn-slate"));
        let session = create(
            &state,
            NewAgent {
                backend: grenadine_core::api::AgentBackend::Devin,
                repo: fx.clone.slug.clone(),
                pr: None,
                prompt: "hi".into(),
                permission_mode: "accept-edits".into(),
                model: None,
                images: Vec::new(),
            },
        )
        .await
        .unwrap();

        // The approval the turn asks for gets its option back.
        wait_for_status(&state, &session.id).await;
        assert!(
            approve(
                &state,
                &session.id,
                "exec:0#t",
                Decision {
                    allow: true,
                    option: Some("allow_once".into()),
                    ..Default::default()
                },
            )
            .unwrap()
        );
        wait_for_turns(&state, &session.id, 1).await;
        let session = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(session.cost, 0.25);
        assert_eq!(session.cost_unit, grenadine_core::api::CostUnit::Acu);
        assert_eq!(session.model.as_deref(), Some("swe-2"));
        assert_eq!(
            state.db.backend_session(&session.id).unwrap().as_deref(),
            Some("turn-slate")
        );

        // The transcript has the answer and the tool call, not the load
        // replay the second turn gets.
        send(
            &state,
            &session.id,
            "again".into(),
            "accept-edits".into(),
            None,
        );
        wait_for_status(&state, &session.id).await;
        assert!(
            approve(
                &state,
                &session.id,
                "exec:0#t",
                Decision {
                    allow: true,
                    option: Some("allow_once".into()),
                    ..Default::default()
                },
            )
            .unwrap()
        );
        wait_for_turns(&state, &session.id, 2).await;
        let transcript = std::fs::read_to_string(dir.path().join("devin_log")).unwrap();
        assert!(transcript.contains("\"session/load\""));
        let texts: Vec<_> = state
            .db
            .agent_events(&session.id)
            .unwrap()
            .into_iter()
            .filter(|e| matches!(e.kind, AgentEventKind::Text(_)))
            .collect();
        assert_eq!(texts.len(), 2, "no replayed text duplicated");
    }

    /// A prompt asked to stop before its turn got to running it never
    /// reaches Devin: the interrupt that landed while the session was
    /// being set up ends it as Interrupted without a `session/prompt`.
    #[tokio::test]
    async fn an_early_interrupt_skips_the_prompt() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_devin(
            &fx,
            dir.path(),
            &fake_devin_slow(dir.path(), "early-oak", "0.5"),
        );
        let session = create(
            &state,
            NewAgent {
                backend: grenadine_core::api::AgentBackend::Devin,
                repo: fx.clone.slug.clone(),
                pr: None,
                prompt: "hi".into(),
                permission_mode: "accept-edits".into(),
                model: None,
                images: Vec::new(),
            },
        )
        .await
        .unwrap();
        // As soon as devin picked the session up — still busy answering
        // session/new — stop it.
        let log = dir.path().join("devin_log");
        for _ in 0..200 {
            if std::fs::read_to_string(&log)
                .unwrap_or_default()
                .contains("\"session/new\"")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        interrupt(&state, &session.id);
        wait_for_turns(&state, &session.id, 1).await;
        assert_eq!(
            state.db.agent(&session.id).unwrap().unwrap().status,
            AgentStatus::Interrupted
        );
        // Whatever point the interrupt landed, no prompt ran.
        let transcript = std::fs::read_to_string(&log).unwrap();
        assert!(
            !transcript.contains("\"session/prompt\""),
            "fake devin received a prompt: {transcript}"
        );
    }

    /// An interrupt asks Devin to cancel, which ends the turn.
    #[tokio::test]
    async fn an_interrupt_cancels() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        // The permission request never gets an answer; the interrupt's
        // session/cancel ends the turn.
        let state = state_with_devin(&fx, dir.path(), &fake_devin(dir.path(), "stop-pine"));
        let session = create(
            &state,
            NewAgent {
                backend: grenadine_core::api::AgentBackend::Devin,
                repo: fx.clone.slug.clone(),
                pr: None,
                prompt: "hi".into(),
                permission_mode: "accept-edits".into(),
                model: None,
                images: Vec::new(),
            },
        )
        .await
        .unwrap();
        wait_for_status(&state, &session.id).await;
        interrupt(&state, &session.id);
        wait_for_turns(&state, &session.id, 1).await;
        assert_eq!(
            state.db.agent(&session.id).unwrap().unwrap().status,
            AgentStatus::Interrupted
        );
        let transcript = std::fs::read_to_string(dir.path().join("devin_log")).unwrap();
        assert!(transcript.contains("\"session/cancel\""));
    }
}
