//! Agent sessions driven from the UI. Each session works in its own git
//! worktree; every prompt runs one turn with the session's `Backend` (the
//! Claude Code backend is in `claude.rs`), and the turn's output becomes
//! the session's transcript. Each prompt comes with a permission mode;
//! the backend asks about the tool uses that the mode doesn't decide on
//! its own, and the user answers on the page.

use std::cell::OnceCell;
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
    AgentStatus, ApprovalOption, GENERAL, Landed, LandedPr, NewAgent, PermissionMode, PrDetail,
    PrKey, PromptImage, ServerEvent, SessionKind, WrapUp, WrapUpTarget,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, BufReader};
use tokio::sync::{broadcast, oneshot};

use crate::claude;
use crate::devin;
use crate::git::{REF_PREFIX, Repo};
use crate::github::StackLookup;
use crate::pr_plan::{
    Above, Planned, PrSpec, StackOp, chain, chain_above, check_source, plan_prs, plan_stack,
};
use crate::reflow::reflow;
use crate::sync::State;

/// The ref the trunk's tip is fetched into before a session branches off it.
const TRUNK_REF: &str = "refs/grenadine/agent/trunk";
/// The ref a pull request's target branch is fetched into while a session's
/// commits are pushed. One ref per repository is enough because pushing
/// holds the repository's git lock.
const TARGET_REF: &str = "refs/grenadine/agent/target";
/// Where the head branch of each PR a session took over is fetched, under
/// the PR's number, before the session's commits first replace it.
const LEASE_REF: &str = "refs/grenadine/agent/lease";
/// Where `manage_prs` puts the heads of the PRs a session takes over,
/// under each PR's number, for the agent to rebase or cherry-pick.
pub(crate) const STACK_REF: &str = "refs/grenadine/agent/stack";

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
    Landed {
        landed: Landed,
        /// What landing did besides opening or updating the PRs, e.g.
        /// closing a PR the mapping dropped.
        notes: Vec<String>,
    },
    /// Claude has to fix the worktree first, as `prompt` tells it.
    Ask { reason: String, prompt: String },
    /// The agent picks the new pull requests' branch names first, as
    /// `prompt` asks it to.
    NameBranches { prompt: String },
    /// Each commit is on GitHub on its branch; the PRs that aren't open
    /// yet are opened next, the bottom one into `trunk`.
    Pushed {
        trunk: String,
        /// Each commit with its branch and PR, bottom first.
        planned: Vec<Planned>,
        /// The session's open PRs the plan leaves out.
        dropped: Vec<LandedPr>,
        /// The tip the commits land on top of, when merged PRs moved the
        /// stack's lower bound past the session's base.
        rebased: Option<String>,
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
    pub answers: Option<BTreeMap<String, Vec<String>>>,
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

    /// Runs `job` in `cwd`, on a model of its own for the job's kind, and
    /// returns the answer, in the shape of the job's schema.
    fn structured<'a>(
        &'a self,
        job: &'a JobSpec<'a>,
        cwd: &'a Path,
    ) -> BoxFuture<'a, Result<Value>>;
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

/// What a background job is for. Each backend runs each kind on a model
/// of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    /// A title for a new session's first prompt.
    Summary,
    /// What a deleted session left behind.
    Scan,
    /// Which cleanup items are stale.
    Prune,
}

impl JobKind {
    /// What it's called in errors.
    pub fn name(self) -> &'static str {
        match self {
            JobKind::Summary => "summary",
            JobKind::Scan => "scan",
            JobKind::Prune => "prune",
        }
    }
}

/// What a background job may use besides its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobTools {
    /// Nothing.
    None,
    /// Reading files, and the read-only commands that `cleanup` lists.
    ReadOnly,
}

/// What a background job may spend on each backend, in the backend's
/// cost unit.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    /// On Claude Code, in USD.
    pub usd: f64,
    /// On Devin, in ACU.
    pub acu: f64,
}

/// A background job, like a summary or a cleanup scan, that answers in a
/// JSON schema's shape without a session: what it's for, what it may use
/// and spend, and where it runs.
pub struct JobSpec<'a> {
    pub kind: JobKind,
    pub schema: &'a Value,
    pub input: &'a str,
    pub tools: JobTools,
    pub budget: Budget,
    /// A directory it may read besides where it runs, on a backend that
    /// can.
    pub add_dir: Option<&'a Path>,
    /// The clone it runs in, so that it can ask git about the repo; Devin,
    /// which has no `--setting-sources user`, loads the clone's own rules
    /// and hooks there. Without one, it runs in a private directory of its
    /// own: the backends load instructions, settings and even hooks from
    /// where they run, and a shared directory like /tmp could hold anyone's.
    pub clone: Option<&'a Path>,
    /// How long it may take before it's killed.
    pub timeout: Duration,
}

/// The error of a job on a backend that isn't offered, e.g. because it
/// can't list its models right now.
#[derive(Debug)]
pub struct NotOffered(pub AgentBackend);

impl std::fmt::Display for NotOffered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} isn't offered", self.0.label())
    }
}

impl std::error::Error for NotOffered {}

/// The error of listing a backend's models when its executable isn't
/// there, with what the listing said.
#[derive(Debug)]
pub struct NotInstalled(pub String);

impl std::fmt::Display for NotInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NotInstalled {}

/// Whether `e` came from trying to spawn an executable that isn't
/// there.
fn is_missing(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|s| s.downcast_ref::<std::io::Error>())
        .any(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// A fresh directory, gone again once it's dropped, for a background
/// process to run in. The backends load instructions, settings and hooks
/// from where they run and from its parents, so it's in a parent only
/// this user can write to, never in a shared one like /tmp.
pub fn private_dir() -> Result<tempfile::TempDir> {
    let parent = private_parent(
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME"),
    )?;
    Ok(tempfile::Builder::new()
        .prefix("grenadine-")
        .tempdir_in(&parent)?)
}

/// Where `private_dir` makes its directories: the runtime directory if
/// it's this user's alone, else `grenadine/jobs` in the cache directory,
/// which only this user may use.
fn private_parent(
    runtime: Option<std::ffi::OsString>,
    cache: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let set = |v: Option<std::ffi::OsString>| v.filter(|v| !v.is_empty()).map(PathBuf::from);
    // SAFETY: getuid can't fail and touches no memory.
    let uid = unsafe { libc::getuid() };
    if let Some(runtime) = set(runtime)
        && let Ok(m) = std::fs::metadata(&runtime)
        && m.is_dir()
        && m.uid() == uid
        && m.mode() & 0o077 == 0
    {
        return Ok(runtime);
    }
    let cache = set(cache)
        .or_else(|| set(home).map(|h| h.join(".cache")))
        .context("neither XDG_RUNTIME_DIR nor HOME is set")?;
    let dir = cache.join("grenadine").join("jobs");
    std::fs::create_dir_all(&dir).with_context(|| format!("can't create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("can't make {} private", dir.display()))?;
    Ok(dir)
}

/// A backend's models: listed once and kept, or the listing's failure
/// kept for `LISTING_RETRY`. A listing without any models counts as a
/// failure, since there's nothing to keep. Whoever asks while it lists waits for that
/// listing.
#[derive(Default)]
pub struct ModelList(tokio::sync::Mutex<Option<Listing>>);

enum Listing {
    Listed(Vec<AgentModelInfo>),
    /// A failure; `missing` marks the executable not being there.
    Failed {
        at: Instant,
        error: String,
        missing: bool,
    },
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
            Some(Listing::Failed { at, error, missing }) if at.elapsed() < LISTING_RETRY => {
                if *missing {
                    return Err(NotInstalled(error.clone()).into());
                }
                bail!("{error}")
            }
            _ => {}
        }
        let listed = list().await.and_then(|m| {
            if m.is_empty() {
                bail!("it listed none")
            } else {
                Ok(m)
            }
        });
        match listed {
            Ok(models) => {
                *listing = Some(Listing::Listed(models.clone()));
                Ok(models)
            }
            Err(e) => {
                let missing = is_missing(&e);
                let error = format!("{e:#}");
                tracing::warn!("listing {name}'s models: {error}");
                *listing = Some(Listing::Failed {
                    at: Instant::now(),
                    error: error.clone(),
                    missing,
                });
                if missing {
                    return Err(NotInstalled(error).into());
                }
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
    /// Whether each new session's first prompt is summarized.
    pub summaries: std::sync::atomic::AtomicBool,
    /// The shells opened from the sessions' pages.
    pub terminals: crate::terminal::Terminals,
    /// The sessions' cached unstaged-change counts.
    pub unstaged: crate::unstaged::Unstaged,
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
    /// An update_prs or land_on_checkout call is landing the session's
    /// work. Devin sends a tool call again when it runs over 120 s, so a
    /// second call must not land the same work at the same time.
    landing: bool,
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
            landing: false,
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

/// An id for a permission request the backend gave none, `prefix` and 64
/// random bits. Counters such as JSON-RPC ids start over when the backend
/// reconnects, and a repeated id would take an earlier request's answer.
pub fn approval_id(prefix: &str) -> String {
    format!("{prefix}-{}", random_hex(8))
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
    /// interrupted. `claude` is the executable Claude Code sessions run.
    /// With `slices`, each session's processes go into their own systemd
    /// user slice. With `scans`, deleting a session scans its transcript
    /// for what it left behind. With `summaries`, each new session's first
    /// prompt is summarized.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: &crate::db::Db,
        claude: PathBuf,
        devin: devin::DevinConfig,
        worktrees: PathBuf,
        base_url: String,
        slices: bool,
        scans: bool,
        summaries: bool,
    ) -> Result<Agents> {
        db.interrupt_agents()?;
        db.interrupt_auto_reviews(now())?;
        // The backend runs in the worktrees, so a relative path would
        // resolve differently there.
        let worktrees = std::path::absolute(&worktrees)?;
        Ok(Agents {
            claude: claude::Claude::new(claude),
            devin: devin::Devin::new(devin),
            images: worktrees.with_file_name("images"),
            worktrees,
            base_url,
            slices,
            live: std::sync::Mutex::default(),
            scans: crate::cleanup::Scans::new(scans),
            summaries: summaries.into(),
            terminals: crate::terminal::Terminals::default(),
            unstaged: crate::unstaged::Unstaged::default(),
        })
    }

    /// The systemd user slice the session's processes go into, if they go
    /// into one.
    pub fn slice_of(&self, id: &str) -> Option<String> {
        self.slices.then(|| slice(id))
    }

    /// Whether the sessions' processes go into their own systemd user
    /// slices.
    pub fn slices(&self) -> bool {
        self.slices
    }

    /// Where the sessions' worktrees are.
    pub fn worktrees(&self) -> &Path {
        &self.worktrees
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

    /// The backends sessions can pick, with their models: each backend
    /// whose executable is there, its models empty and `error` saying
    /// why when they can't be listed.
    pub async fn backends(&self) -> Vec<AgentBackendInfo> {
        let listed =
            futures::future::join_all(AgentBackend::ALL.map(|b| self.backend(b).models())).await;
        AgentBackend::ALL
            .into_iter()
            .zip(listed)
            .filter_map(|(backend, listed)| match listed {
                Ok(models) => Some(backend.info(models)),
                Err(e) if e.is::<NotInstalled>() => None,
                Err(e) => {
                    let mut info = backend.info(Vec::new());
                    info.error = Some(format!("{e:#}"));
                    Some(info)
                }
            })
            .collect()
    }

    /// Whether sessions can pick `backend`, as `backends` has it.
    pub async fn offered(&self, backend: AgentBackend) -> bool {
        match self.backend(backend).models().await {
            Err(e) => !e.is::<NotInstalled>(),
            Ok(_) => true,
        }
    }

    /// Runs `job` on `backend` and returns its answer, in the shape of its
    /// schema. A backend that isn't offered doesn't run it.
    pub async fn structured(&self, backend: AgentBackend, job: &JobSpec<'_>) -> Result<Value> {
        if !self.offered(backend).await {
            return Err(NotOffered(backend).into());
        }
        // It goes away with the job, however that ends.
        let dir = private_dir()
            .with_context(|| format!("can't create a directory for the {}", job.kind.name()))?;
        let cwd = job.clone.unwrap_or(dir.path());
        self.backend(backend).structured(job, cwd).await
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
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let seq = if kind.is_ephemeral() {
        None
    } else {
        Some(state.db.append_agent_event(id, &kind, at)?)
    };
    let _ = live.events.send(AgentEvent {
        seq,
        at: Some(at),
        kind,
    });
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
    state.agents.unstaged.queue(id);
    state.send(ServerEvent::AgentsChanged);
}

/// Creates the session's worktree, or a plain directory for a general
/// session, and starts working on its first prompt.
pub async fn create(state: &Arc<State>, req: NewAgent) -> Result<AgentSession> {
    create_session(state, req, SessionKind::Interactive, None).await
}

/// Creates a session as `create` does, of `kind`, titled `title` or
/// else after its first prompt. Only interactive sessions get a summary
/// of their prompt.
pub async fn create_session(
    state: &Arc<State>,
    req: NewAgent,
    kind: SessionKind,
    title: Option<String>,
) -> Result<AgentSession> {
    if req.prompt.trim().is_empty() {
        bail!("the prompt is empty");
    }
    let id = uuid();
    let worktree = state.agents.worktrees.join(&req.repo).join(&id);
    let (branch, status) = if req.repo == GENERAL {
        if req.pr.is_some() {
            bail!("a general session can't start from a PR");
        }
        (String::new(), AgentStatus::Idle)
    } else {
        // Checked now so these still fail on the form; the fetch and
        // worktree add happen when the first turn runs, under `Creating`.
        if !state.repos.contains_key(&req.repo) {
            bail!("{} is not a configured repository", req.repo);
        }
        pr_head(state, &req.repo, req.pr)?;
        (session_branch(&id), AgentStatus::Creating)
    };

    let session = AgentSession {
        id: id.clone(),
        backend: req.backend,
        repo: req.repo,
        pr: req.pr,
        title: title.unwrap_or_else(|| self::title(&req.prompt)),
        summary: None,
        branch,
        worktree: worktree.to_string_lossy().into_owned(),
        base_sha: String::new(),
        status,
        created_at: now(),
        cost: 0.0,
        cost_unit: req.backend.cost_unit(),
        context_tokens: None,
        context_window: None,
        model: None,
        landed: None,
        prs: None,
        unstaged: 0,
        kind,
        draft: req.draft.clone(),
    };
    // A second start with the same draft returns the first one's session.
    if let Some(d) = &req.draft
        && let Some(session) = state.db.agent_by_draft(d)?
    {
        return Ok(session);
    }
    match state.db.create_agent(&session) {
        Ok(()) => {}
        Err(e) if req.draft.is_some() && unique_violation(&e) => {
            // An insert that raced this one claimed the draft.
            return state
                .db
                .agent_by_draft(req.draft.as_deref().unwrap())?
                .ok_or(e);
        }
        Err(e) => return Err(e),
    }
    if session.pr.is_some() {
        if let Err(e) = take_stack_on_start(state, &session) {
            tracing::warn!("agent {id}: can't take over the PRs stacked on its PR: {e:#}");
        }
        // The next poll fills in the title right away.
        state.poke.notify_one();
    }
    state.send(ServerEvent::AgentsChanged);
    let images = save_images(state, &id, &req.images).await?;
    if kind == SessionKind::Interactive {
        crate::summary::start(state, &id, session.backend, &req.prompt);
    }
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

/// Makes a session that starts from a PR with a linear chain of open PRs
/// stacked on it manage that PR and all of the chain, taking them from
/// any session that managed them. A chain that forks takes nothing: the
/// start note tells the agent to pick one with `manage_prs`. So does one
/// whose PRs haven't synced yet, which a sync is started for.
fn take_stack_on_start(state: &Arc<State>, session: &AgentSession) -> Result<()> {
    let Some(number) = session.pr else {
        return Ok(());
    };
    let key = |number| PrKey {
        repo: session.repo.clone(),
        number,
    };
    let Some(pr) = state.db.pr(&key(number))? else {
        return Ok(());
    };
    let Some(stack) = &pr.stack else {
        return Ok(());
    };
    let Above::Linear(above) = chain_above(stack, number) else {
        return Ok(());
    };
    if above.is_empty() {
        return Ok(());
    }
    let mut prs = vec![taken_pr(&pr)?];
    for n in above {
        match state.db.pr(&key(n))? {
            Some(detail) => prs.push(taken_pr(&detail)?),
            None => {
                state.sync_once(key(n));
                return Ok(());
            }
        }
    }
    state.db.take_prs(
        &session.id,
        &session.repo,
        &Landed::from_prs(prs),
        Some(number),
    )?;
    Ok(())
}

/// The commit a session's worktree begins from: the head of the top PR
/// it manages when it took over PRs stacked on the PR it started from,
/// else `pr_head` of that PR.
fn start_head(state: &State, session: &AgentSession) -> Result<Option<String>> {
    if session.pr.is_some()
        && let [_, .., top] = &landed_prs(session)[..]
        && let Some(head) = &top.lease
    {
        return Ok(Some(head.clone()));
    }
    pr_head(state, &session.repo, session.pr)
}

/// The commit a session starting on `pr` begins from: the head of its
/// newest version with commits. `None` for a session starting on the
/// trunk.
fn pr_head(state: &State, repo: &str, pr: Option<u64>) -> Result<Option<String>> {
    let Some(number) = pr else { return Ok(None) };
    let key = PrKey {
        repo: repo.to_owned(),
        number,
    };
    let pr = state
        .db
        .pr(&key)?
        .ok_or_else(|| anyhow!("{repo}#{number} hasn't synced yet"))?;
    let head = pr
        .versions
        .iter()
        .rev()
        .find(|v| !v.missing)
        .ok_or_else(|| anyhow!("{repo}#{number} has no commits"))?;
    Ok(Some(head.sha.clone()))
}

/// Whether `e` is SQLite's UNIQUE constraint, e.g. a second session
/// insert claiming the same draft.
fn unique_violation(e: &anyhow::Error) -> bool {
    e.downcast_ref::<rusqlite::Error>().is_some_and(|e| {
        matches!(
            e,
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation
        )
    })
}

/// Creates `session`'s worktree and branch and records the commit they
/// start from, for a session whose worktree wasn't built when its row
/// was inserted. An earlier attempt that failed or died may have left
/// the worktree or the branch behind, so they go first.
async fn ensure_worktree(state: &Arc<State>, session: &AgentSession) -> Result<()> {
    let cloned = state
        .repos
        .get(&session.repo)
        .ok_or_else(|| anyhow!("{} is not a configured repository", session.repo))?
        .clone();
    let base = start_head(state, session)?;
    let _guard = cloned.git_lock.lock().await;
    // A delete that won the race for the lock shouldn't leave the
    // worktree and branch behind.
    if state.db.agent(&session.id)?.is_none() {
        bail!("the session is gone");
    }
    let repo = cloned.repo.clone();
    let worktree = PathBuf::from(&session.worktree);
    let branch = session.branch.clone();
    let pr = session.pr;
    let base = tokio::task::spawn_blocking(move || -> Result<String> {
        repo.worktree_remove(&worktree, &branch)?;
        let base = match base {
            Some(sha) => {
                if !repo.fetch_commits(std::slice::from_ref(&sha)).is_empty() {
                    bail!("can't fetch {sha}");
                }
                sha
            }
            None => repo.fetch_default_branch(TRUNK_REF)?,
        };
        if let Some(parent) = worktree.parent() {
            std::fs::create_dir_all(parent)?;
        }
        repo.worktree_add(&worktree, &branch, &base)?;
        if let Some(number) = pr {
            // `@{u}` in the worktree is the PR's target branch, so
            // `git merge-base @{u} HEAD` names the PR's base.
            repo.set_upstream(&branch, &format!("{REF_PREFIX}/{number}/target"))?;
        }
        Ok(base)
    })
    .await??;
    state.db.set_agent_base(&session.id, &base)?;
    state.send(ServerEvent::AgentsChanged);
    Ok(())
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
                answers: None,
                expired: false,
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
    let requested = emit(
        state,
        id,
        AgentEventKind::ApprovalRequested {
            id: tool_use.to_owned(),
            tool_name: tool_name.to_owned(),
            input: input.to_string(),
            options,
        },
    );
    // A request the page never heard of can't be answered.
    if let Err(e) = requested {
        state.agents.with_live(id, |l| l.pending.remove(tool_use));
        return Err(e);
    }
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
/// running turn's model, and its mode unless `mode` says otherwise.
pub fn queue_next(state: &State, id: &str, prompt: String, mode: Option<PermissionMode>) {
    state.agents.with_live(id, |l| {
        // An interrupted turn doesn't send the denial's feedback again.
        if l.interrupted {
            return;
        }
        let (turn_mode, model) = l
            .turn
            .clone()
            .unwrap_or_else(|| (PermissionMode::plan(), None));
        l.queue.push_front(Queued {
            prompt,
            images: Vec::new(),
            mode: mode.clone().unwrap_or(turn_mode),
            model,
        });
    });
}

/// Forgets a permission request when its waiter stops waiting. One
/// nobody answered expires, so its card stops offering answers, unless
/// it's a plan.
struct PendingGuard<'a> {
    state: &'a State,
    id: &'a str,
    tool_use: &'a str,
    tool_name: &'a str,
}

/// Whether an unanswered request of `tool_name` expires once nobody waits
/// for it. A plan stays open: answering it starts a new turn.
fn expires(tool_name: &str) -> bool {
    tool_name != "ExitPlanMode"
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        // `approve` and `interrupt` take the request out before they
        // answer it, so finding it here means nobody did.
        let (unanswered, still_pending) = self.state.agents.with_live(self.id, |l| {
            let unanswered = l.pending.remove(self.tool_use).is_some();
            (unanswered, !l.pending.is_empty())
        });
        if !unanswered || !expires(self.tool_name) {
            return;
        }
        expire_approval(self.state, self.id, self.tool_use);
        // The turn may have ended already; only a session still waiting
        // on this request goes back to running.
        let waiting = matches!(
            self.state.db.agent(self.id),
            Ok(Some(s)) if s.status == AgentStatus::AwaitingApproval
        );
        if waiting && !still_pending {
            set_status(self.state, self.id, AgentStatus::Running);
        }
    }
}

/// Tells the page that nobody can answer the request `tool_use` anymore.
fn expire_approval(state: &State, id: &str, tool_use: &str) {
    let expired = emit(
        state,
        id,
        AgentEventKind::ApprovalResolved {
            id: tool_use.to_owned(),
            allow: false,
            message: None,
            answers: None,
            expired: true,
        },
    );
    if let Err(e) = expired {
        tracing::warn!("agent {id}: {e:#}");
    }
}

/// Expires the permission requests the transcripts still show as
/// waiting, which nobody can answer after a restart. Plans stay open.
pub fn expire_orphaned_approvals(state: &State) -> Result<()> {
    let mut open: Vec<(String, String)> = Vec::new();
    for (session, kind) in state.db.approval_events()? {
        match kind {
            AgentEventKind::ApprovalRequested { id, tool_name, .. } if expires(&tool_name) => {
                open.push((session, id))
            }
            AgentEventKind::ApprovalResolved { id, .. } => {
                // Ids could repeat before they were random, so a
                // resolution closes the latest request with its id.
                if let Some(i) = open.iter().rposition(|(s, o)| *s == session && *o == id) {
                    open.remove(i);
                }
            }
            _ => {}
        }
    }
    for (session, tool_use) in open {
        expire_approval(state, &session, &tool_use);
    }
    Ok(())
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
    if matches!(state.db.agent(id), Ok(Some(s)) if s.kind == SessionKind::AutoReview) {
        return Some(deny_unattended(state, id, tool_use, tool_name, input));
    }
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
        tool_name,
    };
    tokio::select! {
        d = rx => d.ok(),
        _ = state.shutdown.cancelled() => None,
    }
}

/// What an unattended session is told when it asks for a tool.
pub const UNATTENDED_DENIAL: &str = "This is an unattended review that nobody can answer or \
     approve tools for. Go on without this tool.";

/// Denies a request of an unattended session right away, showing the
/// request and its denial in the transcript as if the user had answered.
fn deny_unattended(
    state: &State,
    id: &str,
    tool_use: &str,
    tool_name: &str,
    input: &Value,
) -> Decision {
    for kind in [
        AgentEventKind::ApprovalRequested {
            id: tool_use.to_owned(),
            tool_name: tool_name.to_owned(),
            input: input.to_string(),
            options: Vec::new(),
        },
        AgentEventKind::ApprovalResolved {
            id: tool_use.to_owned(),
            allow: false,
            message: Some(UNATTENDED_DENIAL.to_owned()),
            answers: None,
            expired: false,
        },
    ] {
        if let Err(e) = emit(state, id, kind) {
            tracing::warn!("agent {id}: {e:#}");
        }
    }
    Decision {
        allow: false,
        message: Some(UNATTENDED_DENIAL.to_owned()),
        ..Decision::default()
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
    let answers = if allow {
        decision.answers.clone()
    } else {
        None
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
            answers,
            expired: false,
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
    // Before the interrupt, which would end the review on its own.
    let reviewing = session.kind == SessionKind::AutoReview;
    if reviewing {
        state.db.cancel_auto_review(id, now())?;
    }
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
    // The server's own git runs under this lock, e.g. a worktree add,
    // and stopping its processes halfway leaves its ref locks behind.
    let guard = match &cloned {
        Some(c) => Some(c.git_lock.lock().await),
        None => None,
    };
    let worktree = PathBuf::from(&session.worktree);
    let worktree = worktree.canonicalize().unwrap_or(worktree);
    let stopped = {
        let worktree = worktree.clone();
        tokio::task::spawn_blocking(move || stop_processes_in(&worktree)).await?
    };
    if let Some(cloned) = &cloned {
        let _guard = guard;
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
    if reviewing {
        state.db.auto_review_session_deleted(id)?;
    }
    state.db.delete_agent(id)?;
    state.agents.live.lock().unwrap().remove(id);
    state.agents.terminals.forget(id);
    state.agents.unstaged.forget(id);
    state.send(ServerEvent::AgentsChanged);
    if reviewing {
        state.send(ServerEvent::AutoReviewsChanged);
    }
    // A review only reads its worktree, so it leaves nothing behind.
    if known && !reviewing {
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
                crate::auto_review::session_done(&state, &id, last);
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
    // A landing tool call lands the same work; it wins.
    if state.agents.with_live(id, |l| l.landing) {
        state.agents.with_live(id, |l| l.wrap_up = None);
        let _ = emit(
            state,
            id,
            AgentEventKind::WrapUpFailed(
                "the agent is landing this session's work already; wrap up again once it's \
                 done"
                    .into(),
            ),
        );
        return;
    }
    let name_branches = pending.target == WrapUpTarget::PullRequest;
    let outcome = match land(state, id, pending.target, None, name_branches).await {
        Ok(Step::Landed { landed, notes }) => {
            for note in &notes {
                tracing::info!("agent {id}: {note}");
            }
            record_landed(state, id, &landed).map(|()| landed)
        }
        Ok(Step::Ask { prompt, .. }) if !pending.asked => {
            state.agents.with_live(id, |l| {
                if let Some(w) = &mut l.wrap_up {
                    w.asked = true;
                }
            });
            send(state, id, prompt, pending.mode, pending.model);
            return;
        }
        // The agent names the new pull requests' branches itself: its
        // update_prs call lands them, so the wrap-up is done here.
        Ok(Step::NameBranches { prompt }) => {
            state.agents.with_live(id, |l| l.wrap_up = None);
            send(state, id, prompt, pending.mode, pending.model);
            return;
        }
        Ok(Step::Ask { reason, .. }) => Err(anyhow!("{reason}")),
        Ok(Step::Pushed { .. }) => Err(pushed_unopened()),
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

/// Why a landing that pushed its commits but opened none of its PRs
/// failed.
fn pushed_unopened() -> anyhow::Error {
    anyhow!("the PR wasn't opened")
}

/// `s` with its first letter capitalized.
fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// What the agent is told when the worktree isn't ready to land: why, and
/// what to do before calling `tool` again, as `prompt`'s instruction
/// without the "Wrap up: " it carries for the backend.
fn ask_again(reason: &str, prompt: &str, tool: &str) -> anyhow::Error {
    let instruction = prompt.strip_prefix("Wrap up: ").unwrap_or(prompt);
    anyhow!(
        "{}\n\n{} Then call `{tool}` again.",
        capitalized(reason),
        capitalized(instruction)
    )
}

/// Marks the session as landing until it's dropped, see `begin_landing`.
pub struct Landing {
    state: Arc<State>,
    id: String,
}

impl Drop for Landing {
    fn drop(&mut self) {
        if let Some(l) = self.state.agents.live.lock().unwrap().get_mut(&self.id) {
            l.landing = false;
        }
    }
}

/// Marks the session's work as landing and returns the guard clearing the
/// mark when it's dropped. It fails while the user wraps the session up
/// or another landing call is in progress.
pub fn begin_landing(state: &Arc<State>, id: &str) -> Result<Landing> {
    state.agents.with_live(id, |l| {
        if l.wrap_up.is_some() {
            bail!(
                "the user is wrapping up this session from Grenadine already; wait for that to \
                 finish"
            );
        }
        if l.landing {
            bail!(
                "another update_prs or land_on_checkout call is landing this session's work \
                 already; wait for it to finish"
            );
        }
        l.landing = true;
        Ok(Landing {
            state: state.clone(),
            id: id.to_owned(),
        })
    })
}

/// Lands the session's work on `target` right away, for the session's own
/// `update_prs` and `land_on_checkout` MCP tools. Unlike `wrap_up`, which
/// runs once the turns in front of it are done, this runs inside the turn
/// that calls it, so it must not wait for one. Its caller holds a
/// `Landing` from `begin_landing`. Its errors are the tool's result, told
/// to the agent.
pub async fn land_now(
    state: &Arc<State>,
    id: &str,
    target: WrapUpTarget,
    prs: Option<Vec<PrSpec>>,
    _landing: &Landing,
) -> Result<(Landed, Vec<String>)> {
    let session = state
        .db
        .agent(id)?
        .ok_or_else(|| anyhow!("the session is gone"))?;
    if session.kind == SessionKind::AutoReview {
        bail!("an unattended review's work is never landed");
    }
    let tool = match target {
        WrapUpTarget::PullRequest => "update_prs",
        WrapUpTarget::Checkout => "land_on_checkout",
    };
    match land(state, id, target, prs, false).await? {
        Step::Landed { landed, notes } => {
            record_landed(state, id, &landed)?;
            emit(state, id, AgentEventKind::Landed(landed.clone()))?;
            Ok((landed, notes))
        }
        Step::Ask { reason, prompt } => Err(ask_again(&reason, &prompt, tool)),
        Step::NameBranches { .. } | Step::Pushed { .. } => Err(pushed_unopened()),
    }
}

fn record_landed(state: &Arc<State>, id: &str, landed: &Landed) -> Result<()> {
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
                    // The PR is new or was just pushed to, and the poll
                    // may take a while to reach it, or may leave it alone
                    // since no inbox shows it; its page shows it now.
                    state.resync(key);
                }
                // The next poll fills in the titles right away.
                state.poke.notify_one();
            }
        }
    }
    state.send(ServerEvent::AgentsChanged);
    Ok(())
}

/// One try at landing the session's work on `target`. `prs` maps the
/// session's commits to its pull requests, as `update_prs` takes it.
/// With `name_branches`, a plan that opens pull requests asks the agent
/// to name their branches first.
async fn land(
    state: &Arc<State>,
    id: &str,
    target: WrapUpTarget,
    prs: Option<Vec<PrSpec>>,
    name_branches: bool,
) -> Result<Step> {
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
    // branch, or, when its commits replace the PR's own, to the PR's
    // target branch.
    if target == WrapUpTarget::PullRequest {
        managed_elsewhere_check(state, &session, prs.as_deref())?;
    }
    let mut adopt = Adopt::default();
    let mut source = None;
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
            match adopts_source(&session, prs.as_deref()) {
                true => {
                    match landed_prs(&session).iter().find(|p| p.number == number) {
                        // The first time, the PR's branch may only be
                        // replaced where the last sync saw it.
                        None => {
                            source_stack_check(&session, &pr)?;
                            source = Some(taken_pr(&pr)?);
                        }
                        // Taken over but not pushed to yet: others may have
                        // stacked on it since.
                        Some(p) if p.lease.is_some() => source_stack_check(&session, &pr)?,
                        Some(_) => {}
                    }
                    adopt.source = Some(number);
                    Some(pr.base_ref)
                }
                false => Some(pr.head_ref),
            }
        }
        _ => None,
    };
    // Where the session's landed PRs stand: open ones can take new
    // commits and get retargeted, merged ones are frozen, and ones the
    // user closed are forgotten.
    let mut open = Vec::new();
    let mut merged = Vec::new();
    let mut closed = Vec::new();
    let mut taken = Vec::new();
    let mut statuses = BTreeMap::new();
    if target == WrapUpTarget::PullRequest {
        let mut landed = landed_prs(&session);
        // The source PR becomes the session's bottom PR.
        if let Some(source) = source {
            landed.insert(0, source);
        }
        taken = landed.iter().map(|pr| pr.branch.clone()).collect();
        if !landed.is_empty() {
            let keys: Vec<PrKey> = landed
                .iter()
                .map(|pr| PrKey {
                    repo: session.repo.clone(),
                    number: pr.number,
                })
                .collect();
            match state.github.pr_statuses(&keys).await {
                Ok(found) => statuses = found,
                // Without the states the landed PRs count as open, the
                // safest guess; a step that needs GitHub fails later.
                Err(e) => {
                    tracing::warn!("agent {id}: can't fetch the session's PRs' states: {e:#}")
                }
            }
            for pr in landed {
                let key = PrKey {
                    repo: session.repo.clone(),
                    number: pr.number,
                };
                match statuses.get(&key).map(|s| s.state.as_str()) {
                    Some("MERGED") => merged.push(pr),
                    // The user closed it; it's forgotten.
                    Some("CLOSED") => closed.push(pr.number),
                    _ => open.push(pr),
                }
            }
        }
        // The first push to each PR the session took over is leased
        // against where the last sync saw its branch.
        adopt.leases = open
            .iter()
            .filter_map(|pr| {
                Some(Lease {
                    number: pr.number,
                    head_ref: pr.branch.clone(),
                    expect: pr.lease.clone()?,
                })
            })
            .collect();
    }
    let heads: BTreeMap<u64, String> = statuses
        .iter()
        .map(|(key, status)| (key.number, status.head_oid.clone()))
        .collect();
    let step = {
        let _guard = cloned.git_lock.lock().await;
        let repo = cloned.repo.clone();
        let session = session.clone();
        let open = open.clone();
        tokio::task::spawn_blocking(move || match target {
            WrapUpTarget::Checkout => land_on_checkout(&repo, &session),
            WrapUpTarget::PullRequest => push_stack(
                &repo,
                &session,
                pr_branch,
                &adopt,
                &open,
                &merged,
                &heads,
                &taken,
                &closed,
                prs,
                name_branches,
            ),
        })
        .await??
    };
    let Step::Pushed {
        trunk,
        planned,
        dropped,
        rebased,
    } = step
    else {
        return Ok(step);
    };
    // The push updated the PRs already open; each commit without one
    // gets a PR into the branch of the commit below it.
    let mut prs: Vec<LandedPr> = Vec::new();
    let mut opened = 0;
    let mut below = trunk.clone();
    // Each entry's base: the branch of the one below it, or the trunk.
    let bases: Vec<String> = planned
        .iter()
        .map(|p| {
            let base = below.clone();
            below = p.branch.clone();
            base
        })
        .collect();
    for (p, base) in planned.iter().zip(&bases) {
        match &p.pr {
            // Pushed, so its lease is used up.
            Some(pr) => prs.push(LandedPr {
                lease: None,
                ..pr.clone()
            }),
            None => {
                let (title, body) = pr_text(&p.message);
                match state
                    .github
                    .create_pr(&session.repo, &p.branch, base, &title, &body)
                    .await
                {
                    Ok((number, url)) => {
                        opened += 1;
                        prs.push(LandedPr {
                            number,
                            url,
                            branch: p.branch.clone(),
                            lease: None,
                        });
                    }
                    Err(e) => {
                        // The next wrap-up goes on from the PRs opened so far.
                        if opened > 0 {
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
        }
    }
    let mut notes = Vec::new();
    // The kept PRs whose base moves, which GitHub may refuse inside a
    // stack.
    let retargeted: Vec<u64> = planned
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let pr = p.pr.as_ref()?;
            let status = statuses.get(&PrKey {
                repo: session.repo.clone(),
                number: pr.number,
            });
            match status {
                Some(s) if s.base_ref != bases[i] => Some(pr.number),
                _ => None,
            }
        })
        .collect();
    // The GitHub stack the PRs make, kept in sync where the repository
    // has stacked PRs. Stack work never fails the landing: the PRs are
    // fine either way.
    let mut stack_op = gh_stack_plan(state, &session, &open, &prs, &retargeted, &mut notes).await;
    if let StackOp::Rebuild { stack, .. } = &stack_op {
        // GitHub may refuse to change a stacked PR's base or to close
        // it, so it comes apart before either happens.
        if let Err(e) = state.github.unstack(&session.repo, *stack).await {
            notes.push(format!("Couldn't unstack the GitHub stack: {e:#}"));
            stack_op = StackOp::Keep;
        }
    }
    if let Err(e) = retarget_and_close(
        state, &session, &planned, &bases, &statuses, &dropped, &prs, &mut notes,
    )
    .await
    {
        // The PRs are open already, so they count as landed.
        record_landed(state, id, &Landed::from_prs(prs))?;
        return Err(e);
    }
    if !dropped.is_empty() {
        let _guard = cloned.git_lock.lock().await;
        let repo = cloned.repo.clone();
        let branches: Vec<String> = dropped.iter().map(|pr| pr.branch.clone()).collect();
        let result = tokio::task::spawn_blocking(move || repo.delete_branches(&branches)).await?;
        if let Err(e) = result {
            record_landed(state, id, &Landed::from_prs(prs))?;
            return Err(e.context("couldn't delete the dropped pull requests' branches"));
        }
    }
    gh_stack_sync(state, &session, &stack_op, &dropped, &prs, &mut notes).await;
    // The closed PRs' pages show them closed now; the kept ones are
    // synced as they're recorded.
    for pr in &dropped {
        state.resync(PrKey {
            repo: session.repo.clone(),
            number: pr.number,
        });
    }
    // The merged PRs' commits are landed; the next wrap-up continues
    // from the trunk's tip.
    if let Some(tip) = rebased {
        state.db.set_agent_base(id, &tip)?;
    }
    Ok(Step::Landed {
        landed: Landed::from_prs(prs),
        notes,
    })
}

/// Points the kept PRs at their new bases and closes the dropped ones.
/// Runs after all of the stack's PRs are open, and before the dropped
/// ones' branches are deleted: GitHub closes a PR whose base goes away.
#[allow(clippy::too_many_arguments)]
async fn retarget_and_close(
    state: &Arc<State>,
    session: &AgentSession,
    planned: &[Planned],
    bases: &[String],
    statuses: &BTreeMap<PrKey, crate::github::PrStatus>,
    dropped: &[LandedPr],
    prs: &[LandedPr],
    notes: &mut Vec<String>,
) -> Result<()> {
    for (p, base) in planned.iter().zip(bases) {
        let Some(pr) = &p.pr else { continue };
        let key = PrKey {
            repo: session.repo.clone(),
            number: pr.number,
        };
        // Only a base known to differ needs retargeting.
        let Some(status) = statuses.get(&key) else {
            continue;
        };
        if status.base_ref != *base {
            state
                .github
                .set_pr_base(&key, base)
                .await
                .with_context(|| format!("couldn't retarget #{} at {base}", pr.number))?;
            notes.push(format!("Retargeted #{} at {base}.", pr.number));
        }
    }
    for pr in dropped {
        let key = PrKey {
            repo: session.repo.clone(),
            number: pr.number,
        };
        let comment = match prs.is_empty() {
            true => "Grenadine dropped this pull request from its stack.".to_owned(),
            false => format!(
                "Grenadine dropped this pull request from its stack. The work continues in {}.",
                prs.iter()
                    .map(|pr| format!("#{}", pr.number))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let pr_id = state.github.pr_id(&key).await?;
        state
            .github
            .add_comment(&pr_id, &comment)
            .await
            .with_context(|| format!("couldn't comment on #{}", pr.number))?;
        state
            .github
            .close_pr(&key)
            .await
            .with_context(|| format!("couldn't close #{}", pr.number))?;
        notes.push(format!("Closed #{}, dropped from the stack.", pr.number));
    }
    Ok(())
}

/// Plans the GitHub stack work for the session's PRs after landing:
/// `open` the ones already open, `prs` all of them after this landing.
/// Never fails: anything that goes wrong becomes a note and `Keep`.
async fn gh_stack_plan(
    state: &Arc<State>,
    session: &AgentSession,
    open: &[LandedPr],
    prs: &[LandedPr],
    retargeted: &[u64],
    notes: &mut Vec<String>,
) -> StackOp {
    const OFF: &str = "This repository doesn't have GitHub's stacked pull requests turned \
                       on, so the pull requests are only chained by their base branches.";
    let mut found = |result: Result<StackLookup>| match result {
        Ok(StackLookup::Found(stack)) => Some(stack),
        Err(e) => {
            notes.push(format!("Couldn't look up the GitHub stack: {e:#}"));
            None
        }
        _ => None,
    };
    let ours_stack = match open.first() {
        Some(pr) => match state.github.stack_of(&session.repo, pr.number).await {
            Ok(StackLookup::Off) => {
                notes.push(OFF.into());
                return StackOp::Keep;
            }
            result => found(result),
        },
        None => None,
    };
    let mut source_stack = None;
    if ours_stack.is_none()
        && let Some(source) = session.pr
    {
        match state.github.stack_of(&session.repo, source).await {
            Ok(StackLookup::Off) => {
                notes.push(OFF.into());
                return StackOp::Keep;
            }
            result => source_stack = found(result),
        }
    }
    let ours: Vec<u64> = prs.iter().map(|pr| pr.number).collect();
    let session_prs: Vec<u64> = landed_prs(session)
        .iter()
        .map(|pr| pr.number)
        .chain(ours.iter().copied())
        .collect();
    plan_stack(
        &ours,
        &session_prs,
        session.pr,
        ours_stack.as_ref(),
        source_stack.as_ref(),
        retargeted,
    )
}

/// Runs the planned GitHub stack change after the PRs are opened,
/// retargeted and closed. Every failure only becomes a note.
async fn gh_stack_sync(
    state: &Arc<State>,
    session: &AgentSession,
    op: &StackOp,
    dropped: &[LandedPr],
    prs: &[LandedPr],
    notes: &mut Vec<String>,
) {
    let gh = &state.github;
    match op {
        StackOp::Keep => {}
        StackOp::Create(prs_to_stack)
        | StackOp::Rebuild {
            new: prs_to_stack, ..
        } if prs_to_stack.len() >= 2 => match gh.create_stack(&session.repo, prs_to_stack).await {
            Ok(number) => notes.push(format!(
                "GitHub stack #{number}: {}.",
                prs_to_stack
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Err(e) => {
                notes.push(format!("Couldn't create the GitHub stack: {e:#}"));
                if let StackOp::Rebuild { old, .. } = op {
                    let dropped: Vec<u64> = dropped.iter().map(|pr| pr.number).collect();
                    let restore: Vec<u64> = old
                        .iter()
                        .filter(|n| !dropped.contains(n))
                        .copied()
                        .collect();
                    match gh.create_stack(&session.repo, &restore).await {
                        Ok(number) => {
                            notes.push(format!("Restored the GitHub stack as #{number}."))
                        }
                        Err(e) => {
                            notes.push(format!("Couldn't restore the GitHub stack either: {e:#}"))
                        }
                    }
                }
            }
        },
        StackOp::Add { stack, prs: top } => {
            match gh.add_to_stack(&session.repo, *stack, top).await {
                Ok(()) => notes.push(format!(
                    "GitHub stack #{stack}: {}.",
                    prs.iter()
                        .map(|pr| format!("#{}", pr.number))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                Err(e) => {
                    notes.push(format!("Couldn't extend the GitHub stack: {e:#}"));
                }
            }
        }
        // A rebuild to a single PR needs no stack.
        StackOp::Rebuild { .. } | StackOp::Create(_) => {}
    }
}

/// The branch the `i`th commit of a session's stack, from 0, is pushed to:
/// the session's own branch for the bottom one.
pub(crate) fn stack_branch(branch: &str, i: usize) -> String {
    match i {
        0 => branch.to_owned(),
        _ => format!("{branch}-{}", i + 1),
    }
}

/// The session's landed PRs with their branches filled in: a PR landed
/// before branches were recorded gets the branch its position had.
pub(crate) fn landed_prs(session: &AgentSession) -> Vec<LandedPr> {
    session
        .landed
        .as_ref()
        .map(Landed::prs)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, mut pr)| {
            if pr.branch.is_empty() {
                pr.branch = stack_branch(&session.branch, i);
            }
            pr
        })
        .collect()
}

/// The PRs a session took over from others: the one it started from,
/// when the session's commits replace that PR's own commits instead of
/// stacking on them, e.g. after rebasing or amending it, and the ones
/// stacked on it that the session manages.
#[derive(Clone, Debug, Default)]
pub(crate) struct Adopt {
    /// The PR the session started from, when it replaces its commits.
    pub source: Option<u64>,
    /// The taken PRs the session hasn't pushed to yet.
    pub leases: Vec<Lease>,
}

/// A PR the session took over and hasn't pushed to yet: its branch is
/// only replaced while it's still at `expect`.
#[derive(Clone, Debug)]
pub(crate) struct Lease {
    pub number: u64,
    /// The PR's head branch.
    pub head_ref: String,
    /// Where the last sync saw the branch.
    pub expect: String,
}

/// `pr` as a PR the session takes over, leased against its head as the
/// last sync saw it.
fn taken_pr(pr: &PrDetail) -> Result<LandedPr> {
    let number = pr.summary.key.number;
    let head = synced_head(pr)
        .ok_or_else(|| anyhow!("#{number} has no synced head commit yet; try again later"))?;
    Ok(LandedPr {
        number,
        url: pr.summary.url.clone(),
        branch: pr.head_ref.clone(),
        lease: Some(head),
    })
}

/// The session's ID as the user sees it.
fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Refuses to push to or close PRs that another session took over from
/// this one: the session's source PR when it replaces its commits, any
/// PR `specs` names, and, without `specs`, any of the session's PRs that
/// moved, since their commits would otherwise go into new PRs.
fn managed_elsewhere_check(
    state: &State,
    session: &AgentSession,
    specs: Option<&[PrSpec]>,
) -> Result<()> {
    let ours: Vec<u64> = landed_prs(session).iter().map(|p| p.number).collect();
    let mut numbers: Vec<u64> = specs.into_iter().flatten().filter_map(|s| s.pr).collect();
    if adopts_source(session, specs) {
        numbers.extend(session.pr);
    }
    if specs.is_none() {
        numbers.extend(
            state
                .db
                .agent_pr_list(&session.id)?
                .into_iter()
                .filter(|p| p.state == "OPEN" && Some(p.key.number) != session.pr)
                .map(|p| p.key.number),
        );
    }
    numbers.retain(|n| !ours.contains(n));
    numbers.sort();
    numbers.dedup();
    let managers = state.db.pr_managers(&session.repo, &numbers, &session.id)?;
    if managers.is_empty() {
        return Ok(());
    }
    let mut by: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (number, id) in &managers {
        by.entry(id).or_default().push(format!("#{number}"));
    }
    let list = by
        .iter()
        .map(|(id, prs)| format!("{} by session {}", prs.join(", "), short_id(id)))
        .collect::<Vec<_>>()
        .join("; ");
    bail!(
        "another session took over pull requests this call would push to or close, and \
         manages them now: {list}. This session can't update them anymore. Pass `prs` naming \
         only the pull requests this session still manages{}",
        match ours.is_empty() {
            true => String::new(),
            false => format!(
                " ({})",
                ours.iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    )
}

/// Whether `update_prs` with `specs` replaces the commits of the PR the
/// session started from: once a commit is mapped to it, and from then
/// on. A session that manages PRs stacked on its source manages the
/// source too, so it replaces its commits from the start.
pub(crate) fn adopts_source(session: &AgentSession, specs: Option<&[PrSpec]>) -> bool {
    let Some(number) = session.pr else {
        return false;
    };
    landed_prs(session).iter().any(|pr| pr.number == number)
        || specs.is_some_and(|specs| specs.iter().any(|s| s.pr == Some(number)))
}

/// The PR's head as the last sync saw it.
fn synced_head(pr: &PrDetail) -> Option<String> {
    pr.versions
        .iter()
        .rev()
        .find(|v| !v.missing)
        .map(|v| v.sha.clone())
}

/// Refuses to replace the commits of the session's source PR while open
/// PRs that the session doesn't manage are stacked on it: their branches
/// would keep the old commits.
fn source_stack_check(session: &AgentSession, pr: &PrDetail) -> Result<()> {
    let ours: Vec<u64> = landed_prs(session).iter().map(|p| p.number).collect();
    let number = pr.summary.key.number;
    let above: Vec<String> = pr
        .stack
        .iter()
        .flat_map(|stack| &stack.prs)
        .filter(|p| p.parent == Some(number) && p.state == "OPEN" && !ours.contains(&p.number))
        .map(|p| format!("#{}", p.number))
        .collect();
    if !above.is_empty() {
        bail!(
            "other pull requests are stacked on #{number} ({}) and would keep its old commits, \
             so its commits can't be replaced; stack the new commits on #{number} instead, or \
             call manage_prs with the chain of pull requests above #{number} that this session \
             should manage, and update them too",
            above.join(", "),
        );
    }
    Ok(())
}

/// How the agent maps `commits` (bottom first) to PRs in `update_prs`'s
/// `prs`, with the mapping by position onto `open` as the suggestion, to
/// follow "call the update_prs tool with".
fn tips_prompt(commits: &[(String, String)], open: &[LandedPr]) -> String {
    let json = serde_json::to_string(
        &commits
            .iter()
            .enumerate()
            .map(|(i, (sha, _))| match open.get(i) {
                Some(pr) => serde_json::json!({"commit": sha, "pr": pr.number}),
                None => serde_json::json!({"commit": sha}),
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();
    let list = commits
        .iter()
        .map(|(sha, message)| {
            format!(
                "- {} {}",
                short(sha),
                message.lines().next().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prs = match open.is_empty() {
        true => String::new(),
        false => format!(
            " This session's open pull requests are {}.",
            open.iter()
                .map(|pr| format!("#{}", pr.number))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    format!(
        "`prs` set to `{json}`, giving each new pull request a `branch` that follows this \
         repository's branch naming conventions. Each entry names a pull request's tip \
         commit, bottom first: the pull request takes the commits after the tip below it up \
         to its own. That suggestion gives each commit its own pull request; to put several \
         commits into one pull request, leave out the entries of all but its top commit.{prs} \
         The commits, bottom first, are:\n{list}"
    )
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// Where a session's own commits start in the worktree's history.
enum Lower {
    /// After the merge base of the target and HEAD.
    MergeBase(String),
    /// After the commit the session started from, which HEAD still
    /// descends from.
    Base,
    /// After the target's tip, which HEAD is on top of but doesn't
    /// descend from the base anymore, e.g. after a rebase onto a
    /// force-pushed branch.
    Target,
    /// HEAD descends from neither.
    Unrelated,
}

/// Where the session's own commits start: after the merge base when the
/// target moved on and the worktree picked the move up, after the
/// session's base while the worktree still builds on it, and after the
/// target's tip when the worktree sits on it but no longer builds on the
/// base. A history that fits none of these doesn't say which commits are
/// the session's, so it isn't guessed.
fn session_lower(repo: &Repo, base: &str, target: &str, head: &str) -> Lower {
    if let Some(mb) = repo.merge_base(target, head)
        && repo.is_ancestor(base, &mb)
    {
        return Lower::MergeBase(mb);
    }
    if repo.is_ancestor(base, head) {
        return Lower::Base;
    }
    if repo.is_ancestor(target, head) {
        return Lower::Target;
    }
    Lower::Unrelated
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
            let upstream = match session_lower(repo, base, &onto, &head) {
                Lower::MergeBase(mb) => mb,
                // `Target` can't happen here: it means HEAD is on top of
                // the checkout, which this block has ruled out.
                Lower::Base | Lower::Target => base.clone(),
                Lower::Unrelated => {
                    return Ok(Step::Ask {
                        reason: format!(
                            "the commits don't build on the checkout at {} or on the session's \
                             base",
                            short(&onto)
                        ),
                        prompt: format!(
                            "Wrap up: rebase your commits onto {onto} with `git rebase --onto \
                             {onto} {base}`, resolve the conflicts, and check that the result \
                             still builds and passes the relevant tests. Finish the rebase and \
                             leave nothing uncommitted."
                        ),
                    });
                }
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
            Ok(()) => {
                return Ok(Step::Landed {
                    landed: Landed::Checkout { sha: head },
                    notes: Vec::new(),
                });
            }
            Err(_) if repo.head()? != onto => continue,
            Err(e) => return Err(e),
        }
    }
    bail!("the checkout kept moving")
}

/// Pushes each of the session's commits to GitHub on its PR's branch.
/// The bottom PR goes into `pr_branch`, or the default branch. Commits
/// already on the target branch aren't the session's own and don't get
/// pushed. `open` is the session's PRs still open on GitHub, bottom
/// first; `merged` the ones that merged, with their head commits in
/// `heads`. `specs` maps commits to PRs as `update_prs` takes it. When
/// `name_branches`, a plan with a new pull request isn't pushed: the
/// agent picks the new pull requests' branches first. With `adopt`'s
/// source, the session's commits replace the source PR's own, which
/// `pr_branch`, its target, doesn't have either; the first push to each
/// of its leased PRs only goes through while its branch is still where
/// the last sync saw it.
#[allow(clippy::too_many_arguments)]
fn push_stack(
    repo: &Repo,
    session: &AgentSession,
    pr_branch: Option<String>,
    adopt: &Adopt,
    open: &[LandedPr],
    merged: &[LandedPr],
    heads: &BTreeMap<u64, String>,
    taken: &[String],
    closed: &[u64],
    specs: Option<Vec<PrSpec>>,
    name_branches: bool,
) -> Result<Step> {
    let worktree = Path::new(&session.worktree);
    if let Some(reason) = repo.worktree_unfinished(worktree)? {
        return Ok(Step::Ask {
            reason,
            prompt: PR_COMMIT_PROMPT.into(),
        });
    }
    let head = repo.worktree_head(worktree)?;
    let trunk = match pr_branch {
        Some(b) => b,
        None => repo.default_branch()?,
    };
    // `session.base_sha` is the target branch's tip when the session was
    // created, so commits merged to it since then count from the merge
    // base instead.
    let target = repo
        .fetch_branch(&trunk, TARGET_REF)
        .with_context(|| format!("can't fetch the trunk branch {trunk}"))?;
    // Merged PRs are frozen: the work lands after them, on the trunk's
    // tip. Their commits still in the worktree have to be rebased away
    // first.
    let mut lower = session.base_sha.clone();
    // The source PR's commits are the session's own when it replaces
    // them: they start where the PR forked off its target.
    if adopt.source.is_some()
        && let Some(fork) = repo.merge_base(&target, &session.base_sha)
    {
        lower = fork;
    }
    let mut rebased = None;
    if !merged.is_empty() {
        let tip = &target;
        if !repo.is_ancestor(tip, &head) {
            let numbers = merged
                .iter()
                .map(|pr| format!("#{}", pr.number))
                .collect::<Vec<_>>()
                .join(", ");
            let top = merged.last().and_then(|pr| heads.get(&pr.number));
            let prompt = match top.filter(|oid| repo.is_ancestor(oid, &head)) {
                Some(upstream) => format!(
                    "Wrap up: rebase your remaining commits onto {tip} with `git rebase --onto \
                     {tip} {upstream}`, resolve the conflicts, and check that the result still \
                     builds and passes the relevant tests. Finish the rebase and leave nothing \
                     uncommitted."
                ),
                None => format!(
                    "Wrap up: drop the commits {numbers} merged from your history, and rebase \
                     your remaining commits onto {tip} with `git rebase --onto {tip} {}`, \
                     resolving any conflicts. Finish the rebase and leave nothing uncommitted.",
                    session.base_sha
                ),
            };
            return Ok(Step::Ask {
                reason: format!("{numbers} merged into {trunk}"),
                prompt,
            });
        }
        lower = tip.clone();
        rebased = Some(tip.clone());
    }
    // The base is ahead of the target when it moved to commits that were
    // never pushed, e.g. by landing on the checkout, so the commits after
    // it are still the session's own. A base that doesn't build on the
    // target means the branch moved under the session, e.g. it was
    // force-pushed: pushing to it would show the stale history's commits
    // in the PR, so the agent rebases onto the new tip first.
    let from = match session_lower(repo, &lower, &target, &head) {
        Lower::MergeBase(mb) => mb,
        Lower::Base if repo.is_ancestor(&target, &lower) => lower.clone(),
        Lower::Target => {
            rebased = Some(target.clone());
            target.clone()
        }
        // The source PR's branch is where the session started, but its
        // commits were rewritten: they replace the PR's own only when the
        // agent says so.
        // `Lower::Base` with the target at the base is handled above.
        Lower::Unrelated if session.pr.is_some() && adopt.source.is_none() && target == lower => {
            let number = session.pr.unwrap_or_default();
            return Ok(Step::Ask {
                reason: format!(
                    "the commits don't build on #{number}'s commits, the tip {} of {trunk}",
                    short(&target)
                ),
                prompt: format!(
                    "Wrap up: if your commits are meant to replace #{number}'s own, e.g. because \
                     you rebased or amended them, call `update_prs` with `prs` whose first entry \
                     names the top one of #{number}'s rewritten commits with `\"pr\": {number}`, \
                     followed by the tip commits of any new pull requests above it. Otherwise rebase your commits onto {target} with `git rebase \
                     --onto {target} <the commit your work starts after>`, resolve the \
                     conflicts, and check that the result still builds and passes the relevant \
                     tests. Finish the rebase and leave nothing uncommitted."
                ),
            });
        }
        Lower::Base | Lower::Unrelated => {
            return Ok(Step::Ask {
                reason: format!(
                    "the commits don't build on {trunk}'s tip {}",
                    short(&target)
                ),
                prompt: format!(
                    "Wrap up: rebase your commits onto {target} with `git rebase --onto {target} \
                     {lower}`, resolve the conflicts, and check that the result still builds and \
                     passes the relevant tests. Finish the rebase and leave nothing uncommitted."
                ),
            });
        }
    };
    let commits = repo.commits(&from, &head)?;
    if commits.is_empty() {
        bail!("there are no commits to push");
    }
    // A commit the target already has a patch-equivalent of isn't
    // pushed: its PR would be empty. A plain rebase against the target
    // drops it, so the agent runs one.
    let applied = repo.applied_upstream(&target, &head, &from)?;
    if !applied.is_empty() {
        let applied: std::collections::BTreeSet<&String> = applied.iter().collect();
        let list = commits
            .iter()
            .filter(|(sha, _)| applied.contains(sha))
            .map(|(sha, message)| {
                format!(
                    "{} {}",
                    short(sha),
                    message.lines().next().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(Step::Ask {
            reason: format!("{} of the commits are already on {trunk}", applied.len()),
            prompt: format!(
                "Wrap up: these commits are already on {trunk}: {list}. Rebase onto it with `git \
                 rebase {target}`, which drops them, resolve any conflicts, and check that the \
                 result still builds and passes the relevant tests. Finish the rebase and leave \
                 nothing uncommitted."
            ),
        });
    }
    // Without `prs` the commits only map one per PR onto the open PRs, or
    // a lone commit onto a new one; any other shape takes the agent
    // naming each PR's tip commit, since a PR may hold several commits.
    if specs.is_none() {
        let one_to_one = closed.is_empty() && commits.len() == open.len();
        let lone_new = closed.is_empty() && open.is_empty() && commits.len() == 1;
        if !one_to_one && (name_branches || !lone_new) {
            let prompt = tips_prompt(&commits, open);
            return Ok(match name_branches {
                true => Step::NameBranches {
                    prompt: format!(
                        "Wrap up: call the grenadine MCP server's update_prs tool with {prompt}"
                    ),
                },
                false => Step::Ask {
                    reason: format!(
                        "the {} commits don't map one per pull request onto the session's {} \
                         open pull requests",
                        commits.len(),
                        open.len()
                    ),
                    prompt: format!("Wrap up: pass {prompt}"),
                },
            });
        }
    }
    let (planned, dropped) = plan_prs(
        &session.branch,
        &commits,
        open,
        taken,
        closed,
        specs.as_deref(),
    )?;
    let leased: Vec<u64> = adopt.leases.iter().map(|l| l.number).collect();
    check_source(&planned, &dropped, adopt.source, &leased)?;
    let new: Vec<&Planned> = planned.iter().filter(|p| p.pr.is_none()).collect();
    let branches: Vec<String> = new.iter().map(|p| p.branch.clone()).collect();
    for branch in &branches {
        if !repo.valid_branch_name(branch) {
            bail!("`{branch}` isn't a valid branch name; pick another one for the `branch`");
        }
    }
    // A branch may exist on GitHub without any PR of this session using
    // it; pushing to it would move someone else's branch.
    let existing: Vec<String> = repo
        .remote_branches(&branches)?
        .into_iter()
        .filter(|b| !taken.contains(b))
        .collect();
    match existing.as_slice() {
        [] => {}
        [one] => bail!(
            "the branch `{one}` already exists on GitHub and isn't one this session opened; \
             pass `prs` with a different `branch` for the new pull request"
        ),
        many => bail!(
            "the branches {} already exist on GitHub and aren't ones this session opened; \
             pass `prs` with different `branch` names for the new pull requests",
            many.iter()
                .map(|b| format!("`{b}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
    let refs: Vec<(String, String)> = planned
        .iter()
        .map(|p| (p.sha.clone(), p.branch.clone()))
        .collect();
    // The first time a taken PR's branch is replaced, it has to be where
    // the last sync saw it: otherwise someone pushed to it since, or it's
    // a branch of a fork with the same name.
    let mut leases = Vec::new();
    for Lease {
        number,
        head_ref,
        expect,
    } in &adopt.leases
    {
        let tip = repo
            .fetch_branch(head_ref, &format!("{LEASE_REF}/{number}"))
            .with_context(|| format!("can't fetch #{number}'s branch {head_ref}"))?;
        if tip != *expect {
            bail!(
                "#{number}'s branch {head_ref} is at {} in this repository, not at {} as \
                 Grenadine last synced it; it may have been pushed to since, or come from a \
                 fork. Check the pull request before trying again",
                short(&tip),
                short(expect)
            );
        }
        leases.push((head_ref.clone(), tip));
    }
    repo.push_commits(&refs, &leases)?;
    Ok(Step::Pushed {
        trunk,
        planned,
        dropped,
        rebased,
    })
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
        // A stack the session took over and hasn't pushed to yet is the
        // start note's.
        _ if landed_prs(session).iter().all(|pr| pr.lease.is_some()) => return None,
        _ => {
            let prs = landed_prs(session)
                .iter()
                .map(|pr| format!("- #{} {} from branch `{}`", pr.number, pr.url, pr.branch))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "The user had Grenadine, the app running this session, open these pull requests \
                 from this session's commits, each stacked on the one before it:\n{prs}\nThese \
                 are this session's PRs. Don't open other PRs for this work or push it to other \
                 branches. To update them or open PRs for new commits, call the grenadine MCP \
                 server's update_prs tool; pass `prs`, naming each PR's tip commit, to drop, \
                 reorder or merge PRs. \
                 Grenadine keeps the PRs in a GitHub stack."
            )
        }
    };
    Some(note)
}

/// What agents working in a repository are told about their pull
/// requests: Grenadine keeps track of them, so they go through its MCP
/// tools rather than git.
fn prs_note(session: &AgentSession) -> Option<String> {
    if session.repo == GENERAL || session.kind == SessionKind::AutoReview {
        return None;
    }
    Some(
        "Grenadine, the app running this session, keeps track of this session's pull \
         requests. To open them or update them, call the grenadine MCP server's update_prs \
         tool; to land this session's commits on the user's checkout of the repository, call \
         its land_on_checkout tool. When opening new pull requests, pass `prs` and give each \
         new one a `branch` that follows this repository's branch naming conventions. Don't \
         `git push` or open pull requests any other way."
            .into(),
    )
}

/// What a session that started from a pull request knows about it: the
/// PR's synced detail and which commits are the PR's own, from the merge
/// base with its target branch to the session's base.
pub(crate) struct SessionPr {
    pub number: u64,
    /// The PR as the last sync stored it; `None` when it hasn't synced.
    pub detail: Option<PrDetail>,
    /// The commit the session's worktree started from; empty when the
    /// worktree doesn't exist yet.
    pub base_sha: String,
    /// The merge base of `base_sha` and the PR's target branch — for a
    /// stacked PR the parent PR's branch — so `merge_base..base_sha` are
    /// the PR's own commits, the same range its diff view shows. `None`
    /// when unknown.
    pub merge_base: Option<String>,
}

/// The `SessionPr` of a session started from a pull request; `None` for
/// a trunk or general session.
pub(crate) async fn session_pr(state: &Arc<State>, session: &AgentSession) -> Option<SessionPr> {
    let number = session.pr?;
    if session.repo == GENERAL {
        return None;
    }
    let key = PrKey {
        repo: session.repo.clone(),
        number,
    };
    let detail = state.db.pr(&key).ok().flatten();
    let base_sha = session.base_sha.clone();
    // A version's merge base is recomputed against the PR's current
    // target branch on each sync, so it matches the diff view's Base.
    let mut merge_base = detail
        .as_ref()
        .and_then(|pr| pr.versions.iter().find(|v| v.sha == base_sha))
        .and_then(|v| v.merge_base.clone());
    if merge_base.is_none()
        && !base_sha.is_empty()
        && let Some(repo) = state.repos.get(&session.repo).map(|c| c.repo.clone())
    {
        let target = format!("{REF_PREFIX}/{number}/target");
        let sha = base_sha.clone();
        merge_base = tokio::task::spawn_blocking(move || repo.merge_base(&target, &sha))
            .await
            .ok()
            .flatten();
    }
    Some(SessionPr {
        number,
        detail,
        base_sha,
        merge_base,
    })
}

/// What `manage_prs` would do: the session's PRs afterwards, bottom
/// first, the PR it then counts as started from, and which of the PRs
/// it adds other sessions manage now.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Takeover {
    pub prs: Vec<LandedPr>,
    pub source: Option<u64>,
    /// The PRs the session didn't manage before, bottom first.
    pub added: Vec<u64>,
    /// The sessions that manage some of `added`, with those PRs.
    pub from: BTreeMap<String, Vec<u64>>,
}

impl Takeover {
    /// What the approval card asks when the takeover takes PRs from
    /// other sessions; `None` when it takes none.
    pub fn question(&self) -> Option<String> {
        if self.from.is_empty() {
            return None;
        }
        let parts = self
            .from
            .iter()
            .map(|(id, prs)| {
                format!(
                    "{} from session {}",
                    prs.iter()
                        .map(|n| format!("#{n}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    short_id(id)
                )
            })
            .collect::<Vec<_>>()
            .join(" and ");
        Some(format!("Take {parts}?"))
    }
}

/// Plans `manage_prs` with `named`: those PRs, the PR the session started
/// from and the session's open PRs have to make one contiguous linear
/// chain of open PRs in their stack as Grenadine last synced it, which
/// the session then manages. Naming PRs below the session's moves the
/// PR it counts as started from down to the chain's bottom.
pub(crate) fn plan_takeover(
    state: &Arc<State>,
    session: &AgentSession,
    named: &[u64],
) -> Result<Takeover> {
    if session.repo == GENERAL {
        bail!("a general session isn't in a repository, so it has no pull requests");
    }
    if session.kind == SessionKind::AutoReview {
        bail!("an unattended review doesn't manage pull requests");
    }
    if named.is_empty() {
        bail!("`prs` must name at least one pull request");
    }
    let key = |number| PrKey {
        repo: session.repo.clone(),
        number,
    };
    // The session's PRs the user closed are forgotten, as landing does.
    let closed: Vec<u64> = state
        .db
        .agent_pr_list(&session.id)?
        .into_iter()
        .filter(|p| p.state == "CLOSED")
        .map(|p| p.key.number)
        .collect();
    let own: Vec<LandedPr> = landed_prs(session)
        .into_iter()
        .filter(|p| !closed.contains(&p.number))
        .collect();
    // Every PR named or the session's, with its synced detail.
    let mut details: BTreeMap<u64, PrDetail> = BTreeMap::new();
    let wanted: Vec<u64> = session
        .pr
        .into_iter()
        .chain(own.iter().map(|p| p.number))
        .chain(named.iter().copied())
        .collect();
    let mut unsynced = Vec::new();
    for &n in &wanted {
        if details.contains_key(&n) {
            continue;
        }
        match state.db.pr(&key(n))? {
            Some(d) => {
                details.insert(n, d);
            }
            None if own.iter().any(|p| p.number == n) => {}
            None => unsynced.push(n),
        }
    }
    if !unsynced.is_empty() {
        for &n in &unsynced {
            state.sync_once(key(n));
        }
        bail!(
            "{} hasn't synced yet; Grenadine started syncing it, so try again in a minute",
            unsynced
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // The stack they're in: that of the session's source if it's synced,
    // else that of any of them that has all of them.
    let stack = wanted
        .iter()
        .filter_map(|n| details.get(n)?.stack.as_ref())
        .find(|s| wanted.iter().all(|n| s.prs.iter().any(|p| p.number == *n)))
        .ok_or_else(|| {
            anyhow!(
                "{} aren't all in one stack as Grenadine last synced it",
                wanted
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let state_of = |n: u64| stack.prs.iter().find(|p| p.number == n).map(|p| &p.state);
    // The session's merged PRs stay below the chain, frozen.
    let (merged, current): (Vec<LandedPr>, Vec<LandedPr>) = own
        .iter()
        .cloned()
        .partition(|p| state_of(p.number).is_some_and(|s| s == "MERGED"));
    let mut members: Vec<u64> = session
        .pr
        .into_iter()
        .chain(current.iter().map(|p| p.number))
        .chain(named.iter().copied())
        .collect();
    members.sort();
    members.dedup();
    let ordered = chain(stack, &members)?;
    let mut prs = merged;
    let mut added = Vec::new();
    for &n in &ordered {
        match own.iter().find(|p| p.number == n) {
            Some(p) => prs.push(p.clone()),
            None => {
                prs.push(taken_pr(&details[&n])?);
                added.push(n);
            }
        }
    }
    if added.is_empty() {
        bail!("this session manages all of these pull requests already");
    }
    let bottom = ordered[0];
    let source = match session.pr.is_some() || !own.iter().any(|p| p.number == bottom) {
        true => Some(bottom),
        false => None,
    };
    let mut from: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for (n, id) in state.db.pr_managers(&session.repo, &added, &session.id)? {
        from.entry(id).or_default().push(n);
    }
    Ok(Takeover {
        prs,
        source,
        added,
        from,
    })
}

/// Carries out `plan_takeover`'s plan: fetches the added PRs' heads into
/// `STACK_REF`, leaving the worktree's HEAD alone, and moves the PRs to
/// the session from any that managed them. Returns what the agent is
/// told: each PR's commits and how to bring them into HEAD.
pub(crate) async fn take_over(
    state: &Arc<State>,
    session: &AgentSession,
    plan: Takeover,
) -> Result<String> {
    let cloned = state
        .repos
        .get(&session.repo)
        .ok_or_else(|| anyhow!("{} is not a configured repository", session.repo))?
        .clone();
    // Each managed PR's head, where it's known, and the commit its own
    // commits start after.
    let heads: Vec<Option<String>> = plan.prs.iter().map(|p| p.lease.clone()).collect();
    let bottom_base = plan.prs.first().and_then(|p| {
        let detail = state
            .db
            .pr(&PrKey {
                repo: session.repo.clone(),
                number: p.number,
            })
            .ok()
            .flatten()?;
        let head = p.lease.as_deref()?;
        detail
            .versions
            .iter()
            .find(|v| v.sha == head)?
            .merge_base
            .clone()
    });
    let in_head: Vec<bool> = {
        let _guard = cloned.git_lock.lock().await;
        let repo = cloned.repo.clone();
        let worktree = PathBuf::from(&session.worktree);
        let added: Vec<(u64, String)> = plan
            .prs
            .iter()
            .filter(|p| plan.added.contains(&p.number))
            .filter_map(|p| Some((p.number, p.lease.clone()?)))
            .collect();
        let heads = heads.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<bool>> {
            let shas: Vec<String> = added.iter().map(|(_, h)| h.clone()).collect();
            let missing = repo.fetch_commits(&shas);
            if !missing.is_empty() {
                bail!("can't fetch {}", missing.join(", "));
            }
            let refs: Vec<(String, String)> = added
                .iter()
                .map(|(n, h)| (format!("{STACK_REF}/{n}"), h.clone()))
                .collect();
            repo.update_refs(&refs, &[])?;
            let head = repo.worktree_head(&worktree).ok();
            Ok(heads
                .iter()
                .map(|h| match (h, &head) {
                    (Some(h), Some(head)) => repo.is_ancestor(h, head),
                    _ => false,
                })
                .collect())
        })
        .await??
    };
    state.db.take_prs(
        &session.id,
        &session.repo,
        &Landed::from_prs(plan.prs.clone()),
        plan.source,
    )?;
    state.poke.notify_one();
    state.send(ServerEvent::AgentsChanged);

    let mut below = bottom_base;
    let mut lines = Vec::new();
    for (i, p) in plan.prs.iter().enumerate() {
        let commits = match (&below, &heads[i]) {
            (Some(from), Some(head)) => format!("commits {from}..{head}"),
            (None, Some(head)) => format!("head {head}"),
            _ => "commits as this session last pushed them".into(),
        };
        let fetched = match plan.added.contains(&p.number) {
            true => format!(", fetched as {STACK_REF}/{}", p.number),
            false => String::new(),
        };
        let status = match in_head[i] {
            true => "; already in HEAD",
            false if heads[i].is_some() => "; not in HEAD yet",
            false => "",
        };
        lines.push(format!(
            "- #{} {} on branch `{}`: {commits}{fetched}{status}",
            p.number, p.url, p.branch
        ));
        below = heads[i].clone();
    }
    let numbers = plan
        .prs
        .iter()
        .map(|p| format!("#{}", p.number))
        .collect::<Vec<_>>()
        .join(", ");
    let taken = match plan.from.is_empty() {
        true => String::new(),
        false => format!(
            " Took {}; those sessions don't push to them anymore.",
            plan.question()
                .unwrap_or_default()
                .trim_start_matches("Take ")
                .trim_end_matches('?')
        ),
    };
    Ok(format!(
        "This session now manages {numbers}, bottom first, each stacked on the one before \
         it.{taken} HEAD wasn't changed. The pull requests:\n{}\nBring the commits that aren't \
         in HEAD yet onto HEAD, in order and keeping each pull request's commits together, \
         e.g. with `git cherry-pick <from>..<head>` for each, or by rebasing them. Then call \
         update_prs with `prs` holding one entry per pull request, bottom first, each naming \
         the pull request's top commit and its `\"pr\"`. Grenadine pushes each to its own \
         branch; the first push to each taken pull request only goes through while its branch \
         is still where Grenadine last synced it.",
        lines.join("\n")
    ))
}

/// What a session started from a pull request is told about it: which
/// commits are the PR's versus the session's own, that `@{u}` is the
/// PR's target branch, and that `session_pr` says more. `managed` is the
/// session's landed PRs: with PRs stacked on this one among them, the
/// session manages that stack, and without, it hears about open PRs
/// stacked on this one that it could take over.
fn pr_note(pr: &SessionPr, managed: &[LandedPr]) -> String {
    let Some(detail) = pr.detail.as_ref() else {
        return format!(
            "This session started from pull request #{}. It hasn't synced yet; the grenadine \
             MCP server's session_pr tool gives its details once it has.",
            pr.number
        );
    };
    let mut note = format!(
        "This session started from pull request #{} \"{}\" ({}), which merges `{}` into `{}`.",
        pr.number, detail.summary.title, detail.summary.url, detail.head_ref, detail.base_ref
    );
    let at = managed.iter().position(|p| p.number == pr.number);
    if let Some(at) = at
        && at + 1 < managed.len()
    {
        return note + &stack_note(pr, &managed[at..]);
    }
    if let Some(merge_base) = &pr.merge_base {
        note += &format!(
            " When the user says \"this PR\", they mean its commits {merge_base}..{}, the same \
             changes as its diff view, not everything since the default branch. Commits after \
             {} are this session's own work.",
            pr.base_sha, pr.base_sha
        );
    }
    note += " This branch's upstream is the pull request's target branch, so \
             `git merge-base @{u} HEAD` gives the PR's base. The grenadine MCP server's \
             session_pr tool gives these details and the PR's stack.";
    note += &format!(
        " New commits go into pull requests stacked on #{n}. To change #{n}'s own commits \
         instead, e.g. to rebase or amend them, rewrite them and call update_prs with `prs` \
         whose first entry names the top rewritten commit with `\"pr\": {n}`; Grenadine then \
         pushes them to `{}`.",
        detail.head_ref,
        n = pr.number
    );
    let head = detail
        .versions
        .iter()
        .rev()
        .find(|v| !v.missing)
        .map(|v| v.sha.as_str());
    if let Some(head) = head.filter(|h| *h != pr.base_sha) {
        note += &format!(
            " The pull request has since moved on to {head}, which this worktree \
             doesn't include."
        );
    }
    let list = |numbers: &[u64]| {
        numbers
            .iter()
            .map(|n| format!("#{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match detail.stack.as_ref().map(|s| chain_above(s, pr.number)) {
        Some(Above::Fork { at, children }) => {
            note += &format!(
                " Open pull requests are stacked on #{n}, but the stack forks: {} are all \
                 stacked on #{at}. So this session doesn't manage any of them, and #{n}'s \
                 commits can't be replaced while they're stacked on it. To change #{n}'s \
                 commits and update the pull requests above it too, call the grenadine MCP \
                 server's manage_prs tool with the one chain of them this session should \
                 manage.",
                list(&children),
                n = pr.number
            );
        }
        Some(Above::Linear(above)) if !above.is_empty() => {
            note += &format!(
                " The open pull requests {} are stacked on #{n}, and this session doesn't \
                 manage them, so #{n}'s commits can't be replaced while they're stacked on \
                 it. To change #{n}'s commits and update them too, call the grenadine MCP \
                 server's manage_prs tool with them first.",
                list(&above),
                n = pr.number
            );
        }
        _ => {}
    }
    note
}

/// What a session that manages `stack`, its source PR (`pr`) and the
/// PRs stacked on it, bottom first, is told about them: their branches
/// and commits, and that each gets its own entry in `update_prs`.
fn stack_note(pr: &SessionPr, stack: &[LandedPr]) -> String {
    let mut below = pr.merge_base.clone();
    let lines = stack
        .iter()
        .map(|p| {
            let range = match (&below, &p.lease) {
                (Some(from), Some(head)) => format!(", commits {from}..{head}"),
                (None, Some(head)) => format!(", head {head}"),
                _ => String::new(),
            };
            below = p.lease.clone();
            format!("- #{} {} on branch `{}`{range}", p.number, p.url, p.branch)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let numbers = stack
        .iter()
        .map(|p| format!("#{}", p.number))
        .collect::<Vec<_>>()
        .join(", ");
    let started = match stack.last().and_then(|p| p.lease.as_deref()) {
        Some(top) if top == pr.base_sha => format!(
            "This worktree started at the top one's head, {top}; commits after it are this \
             session's own work."
        ),
        _ => format!("This worktree started at {}.", pr.base_sha),
    };
    format!(
        " Open pull requests are stacked on it, and this session manages it and them, bottom \
         first, each on its own branch:\n{lines}\n{started} When the user says \"this \
         PR\", they mean #{}'s commits. To change any of these pull requests, rewrite their \
         commits in place, keeping each pull request's commits together and in order, and call \
         the grenadine MCP server's update_prs tool with `prs` holding one entry per pull \
         request, bottom first, each naming the pull request's top commit and its `\"pr\"` \
         ({numbers}). Grenadine pushes each to its own branch; the first push to each only \
         goes through while its branch is still where Grenadine last synced it. New commits \
         go into new pull requests stacked on the top one. The grenadine MCP server's \
         session_pr tool gives these details and the PRs' stack.",
        pr.number
    )
}

/// What agents are told about showing the user images: a Markdown image
/// pointing at a local file doesn't load in the user's page, so they go
/// through the MCP tool.
fn images_note() -> String {
    "To show the user an image, such as a screenshot, call the grenadine MCP server's \
     show_image tool. Markdown image links to local files don't load in the user's page."
        .into()
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
    let mut session = state
        .db
        .agent(id)?
        .ok_or_else(|| anyhow!("the session is gone"))?;
    emit(state, id, AgentEventKind::Prompt(prompt.clone()))?;
    if !images.is_empty() {
        emit(state, id, AgentEventKind::Images(images.clone()))?;
    }
    // Nothing of a general session's is lost with its directory, so one
    // that went away comes back. A repo session's worktree is made here,
    // not when its row is inserted, since the fetch can take a while.
    if session.repo == GENERAL {
        tokio::fs::create_dir_all(&session.worktree).await?;
    } else if session.base_sha.is_empty() {
        set_status(state, id, AgentStatus::Creating);
        ensure_worktree(state, &session).await?;
        // A delete or interrupt that won the race stops the turn here.
        session = state
            .db
            .agent(id)?
            .ok_or_else(|| anyhow!("the session is gone"))?;
        if state.agents.interrupted(id) {
            return Ok(AgentStatus::Interrupted);
        }
    }
    set_status(state, id, AgentStatus::Running);

    let mcp_url = format!(
        "{}/mcp/{id}/{}",
        state.agents.base_url,
        state.agents.secret(id)
    );
    let starting_pr = session_pr(state, &session).await;
    let notes: Vec<_> = [
        (session.kind == SessionKind::AutoReview).then(|| crate::auto_review::NOTE.to_owned()),
        prs_note(&session),
        starting_pr
            .as_ref()
            .map(|pr| pr_note(pr, &landed_prs(&session))),
        landed_note(&session),
        terminals_note(state, id),
        Some(images_note()),
    ]
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
    /// The session's worktree, looked up when text first needs it; `None`
    /// inside when the session's row is gone.
    worktree: OnceCell<Option<String>>,
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
            worktree: OnceCell::new(),
            prs: OpenedPrs::default(),
        }
    }

    /// The session's worktree, where assistant text's relative paths
    /// resolve.
    fn worktree(&self) -> Option<&str> {
        self.worktree
            .get_or_init(|| {
                self.state
                    .db
                    .agent(self.id)
                    .ok()
                    .flatten()
                    .map(|s| s.worktree)
            })
            .as_deref()
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
        let worktree = match kind {
            AgentEventKind::Text(_) => self.worktree(),
            _ => None,
        };
        store_images(self.state, self.id, worktree, &mut kind)?;
        if let AgentEventKind::Usage { tokens, window } = kind {
            return record_usage(self.state, self.id, tokens, window);
        }
        if let AgentEventKind::TurnEnded { error, cost } = &kind {
            // A process can report several results: one more for each
            // background task that finishes after the answer.
            if let Some(cost) = cost {
                self.bill(*cost)?;
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

    /// Adds what the turn's process has cost since it was last billed to
    /// the session's cost; `total` is the process's total so far. A total
    /// below what's billed already takes nothing back.
    pub fn bill(&mut self, total: f64) -> Result<()> {
        if total > self.billed {
            self.state.db.add_agent_cost(self.id, total - self.billed)?;
            self.billed = total;
        }
        Ok(())
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
            AgentEventKind::ToolUse {
                id, name, input, ..
            } if name == "Bash" => {
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
    // The next poll fills in the titles right away.
    state.poke.notify_one();
    state.send(ServerEvent::AgentsChanged);
    Ok(())
}

/// Stores an image of session `id`'s transcript and returns its new id.
pub fn store_image(state: &State, id: &str, mime: &str, data: &[u8]) -> Result<String> {
    let image = uuid();
    state.db.add_agent_image(id, &image, mime, data)?;
    Ok(image)
}

/// The largest image `read_image` reads.
const MAX_IMAGE: u64 = 20 << 20;

/// The MIME type of an image in a format browsers show, from its first
/// bytes.
pub fn image_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF8") {
        Some("image/gif")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Reads the image at `path`, relative to `worktree` unless `path` is
/// absolute, as `show_image` does: at most `MAX_IMAGE` bytes and a PNG,
/// JPEG, GIF or WebP.
pub fn read_image(worktree: &str, path: &str) -> Result<(&'static str, Vec<u8>)> {
    let path = Path::new(worktree).join(path);
    let size = std::fs::metadata(&path)
        .with_context(|| format!("Can't read {}", path.display()))?
        .len();
    if size > MAX_IMAGE {
        bail!("{} is larger than {} MB.", path.display(), MAX_IMAGE >> 20);
    }
    let data = std::fs::read(&path).with_context(|| format!("Can't read {}", path.display()))?;
    let mime = image_mime(&data)
        .with_context(|| format!("{} isn't a PNG, JPEG, GIF or WebP image.", path.display()))?;
    Ok((mime, data))
}

/// Whether `dest` names a URL rather than a local file, by having a
/// `scheme:` prefix.
fn has_url_scheme(dest: &str) -> bool {
    let Some(colon) = dest.find(':') else {
        return false;
    };
    let scheme = &dest[..colon];
    !scheme.is_empty()
        && scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The byte range of `dest` itself within `slice`, the source of one
/// image, when `dest` appears there literally after a `](`, bare or
/// inside `<>`. Escaped or reference-style destinations don't appear
/// literally, so they aren't found and the image is left alone.
fn image_dest_span(slice: &str, dest: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(open) = slice[from..].find("](") {
        let mut start = from + open + 2;
        while matches!(slice.as_bytes().get(start), Some(b' ' | b'\t' | b'\n')) {
            start += 1;
        }
        if let Some(after) = slice[start..]
            .strip_prefix('<')
            .and_then(|s| s.strip_prefix(dest))
            && after.starts_with('>')
        {
            return Some((start + 1, start + 1 + dest.len()));
        }
        if slice[start..].starts_with(dest)
            && matches!(
                slice.as_bytes().get(start + dest.len()),
                None | Some(b')' | b' ' | b'\t' | b'\n' | b'"' | b'\'')
            )
        {
            return Some((start, start + dest.len()));
        }
        from = start;
    }
    None
}

/// `text` with its Markdown images' local-file destinations replaced by
/// the URLs `store` gives them. A destination is a candidate when it
/// isn't empty, has no URL scheme and doesn't already point at
/// `/api/`; it's rewritten only when it appears in `text` literally and
/// `store` accepts it. Everything else stays byte-identical.
fn capture_markdown_images(text: &str, mut store: impl FnMut(&str) -> Option<String>) -> String {
    let options = pulldown_cmark::Options::ENABLE_TABLES
        | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
        | pulldown_cmark::Options::ENABLE_TASKLISTS
        | pulldown_cmark::Options::ENABLE_FOOTNOTES
        | pulldown_cmark::Options::ENABLE_GFM;
    let mut edits = Vec::new();
    for (event, range) in pulldown_cmark::Parser::new_ext(text, options).into_offset_iter() {
        let pulldown_cmark::Event::Start(pulldown_cmark::Tag::Image { dest_url, .. }) = event
        else {
            continue;
        };
        let dest: &str = dest_url.as_ref();
        if dest.is_empty() || dest.starts_with("/api/") || has_url_scheme(dest) {
            continue;
        }
        let Some((start, end)) = image_dest_span(&text[range.clone()], dest) else {
            continue;
        };
        if let Some(url) = store(dest) {
            edits.push((range.start + start..range.start + end, url));
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (range, url) in edits {
        out += &text[at..range.start];
        out += &url;
        at = range.end;
    }
    out += &text[at..];
    out
}

/// Keeps an event's images out of the transcript's text. A tool result's
/// `data:` URLs are moved into the database, which the event then lists
/// by id; ones that don't decode are dropped. Assistant text's Markdown
/// images whose destinations are files readable by `read_image` — under
/// `worktree` or absolute — are stored the same way and their
/// destinations rewritten to the `/api/images/` URLs the page loads;
/// anything else stays as written.
fn store_images(
    state: &State,
    id: &str,
    worktree: Option<&str>,
    kind: &mut AgentEventKind,
) -> Result<()> {
    match kind {
        AgentEventKind::ToolResult { images, .. } => {
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
        }
        AgentEventKind::Text(text) => {
            if let Some(worktree) = worktree {
                *text = capture_markdown_images(text, |dest| {
                    let (mime, data) = read_image(worktree, dest).ok()?;
                    let image = store_image(state, id, mime, &data).ok()?;
                    Some(format!("/api/images/{image}"))
                });
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use grenadine_core::api::{Stack, Version, VersionKind};
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

    /// A test state with no repositories and a claude that isn't there;
    /// sessions start in `GENERAL` with a plain directory.
    fn state_without_repos(dir: &std::path::Path) -> Arc<State> {
        crate::sync::test_state_with_agents(
            BTreeMap::new(),
            dir.join("claude"),
            "devin".into(),
            dir.join("worktrees"),
        )
    }

    /// Two starts with the same draft return the first one's session and
    /// create only one; a different draft, or none, starts one each.
    #[tokio::test]
    async fn a_draft_starts_one_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_without_repos(dir.path());
        let req = |draft: Option<&str>| NewAgent {
            draft: draft.map(str::to_owned),
            backend: AgentBackend::Claude,
            repo: GENERAL.into(),
            pr: None,
            prompt: "hi".into(),
            permission_mode: PermissionMode::from("auto"),
            model: None,
            images: Vec::new(),
        };
        let (a, b) = tokio::join!(
            create(&state, req(Some("d1"))),
            create(&state, req(Some("d1"))),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.id, b.id);
        assert_eq!(a.draft.as_deref(), Some("d1"));
        assert_eq!(state.db.agents().unwrap().len(), 1);
        // The first turn makes the general session's directory.
        let dir = state.agents.worktrees.join(GENERAL);
        for _ in 0..100 {
            if dir.is_dir() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1);

        let c = create(&state, req(Some("d2"))).await.unwrap();
        let d = create(&state, req(None)).await.unwrap();
        assert_ne!(a.id, c.id);
        assert_ne!(a.id, d.id);
        assert_eq!(c.draft.as_deref(), Some("d2"));
        assert_eq!(d.draft, None);
        assert_eq!(state.db.agents().unwrap().len(), 3);
    }

    /// A fake claude that ends its turn right away.
    const CLAUDE_OK: &str = r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"s","model":"claude-test"}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"ok"}'
"#;

    fn new_repo_session() -> NewAgent {
        NewAgent {
            backend: AgentBackend::Claude,
            repo: "owner/name".into(),
            pr: None,
            prompt: "hi".into(),
            permission_mode: PermissionMode::from("auto"),
            model: None,
            images: Vec::new(),
            draft: None,
        }
    }

    /// The session's row is there before its worktree is: `create`
    /// doesn't wait for the fetch, the first turn does.
    #[tokio::test]
    async fn a_repo_session_creates_its_worktree_in_the_first_turn() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_claude(&fx, dir.path(), CLAUDE_OK);
        let cloned = state.repos["owner/name"].clone();
        let _held = cloned.git_lock.lock().await;
        let session = create(&state, new_repo_session()).await.unwrap();
        assert_eq!(session.status, AgentStatus::Creating);
        assert!(session.base_sha.is_empty());
        assert!(!std::path::Path::new(&session.worktree).exists());
        drop(_held);
        wait_for_turns(&state, &session.id, 1).await;
        let done = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(done.status, AgentStatus::Idle);
        assert!(!done.base_sha.is_empty());
        assert!(std::path::Path::new(&done.worktree).join(".git").exists());
    }

    /// A delete that lands while the worktree is being created still
    /// removes everything once it gets the lock.
    #[tokio::test]
    async fn delete_during_creation_leaves_nothing_behind() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_claude(&fx, dir.path(), CLAUDE_OK);
        let cloned = state.repos["owner/name"].clone();
        let held = cloned.git_lock.lock().await;
        let session = create(&state, new_repo_session()).await.unwrap();
        let worktree = session.worktree.clone();
        let branch = session.branch.clone();
        let deleter = tokio::spawn({
            let state = state.clone();
            let id = session.id.clone();
            async move { delete(&state, &id).await }
        });
        // Give delete the moment to reach the lock.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(held);
        assert!(deleter.await.unwrap().unwrap());
        assert!(state.db.agent(&session.id).unwrap().is_none());
        assert!(!std::path::Path::new(&worktree).exists());
        assert_eq!(
            crate::git::tests::run(&fx.clone.path, &["branch", "--list", &branch]),
            ""
        );
    }

    /// A delete that lands while `git worktree add` is running waits for
    /// it to finish rather than stopping it halfway, which would leave
    /// its ref locks behind.
    #[tokio::test]
    async fn delete_during_worktree_add_lets_it_finish() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_claude(&fx, dir.path(), CLAUDE_OK);
        // `git worktree add` checks files out with a `reset` child whose
        // cwd is the new worktree; a smudge filter on one file marks
        // that it started and then sleeps, so the delete lands while the
        // add is in flight.
        let started = dir.path().join("started");
        let done = dir.path().join("done");
        let smudge = format!(
            "touch {}; sleep 1; touch {}",
            started.display(),
            done.display()
        );
        crate::git::tests::run(&fx.upstream, &["config", "filter.slow.smudge", &smudge]);
        crate::git::tests::run(&fx.upstream, &["config", "filter.slow.clean", "cat"]);
        crate::git::tests::commit(&fx.upstream, ".gitattributes", "*.big filter=slow\n");
        crate::git::tests::commit(&fx.upstream, "f.big", "data\n");
        crate::git::tests::run(
            &fx.clone.path,
            &["fetch", "-q", fx.upstream.to_str().unwrap(), "main"],
        );
        crate::git::tests::run(&fx.clone.path, &["config", "filter.slow.smudge", &smudge]);
        crate::git::tests::run(&fx.clone.path, &["config", "filter.slow.clean", "cat"]);
        let session = create(&state, new_repo_session()).await.unwrap();
        let worktree = session.worktree.clone();
        let branch = session.branch.clone();
        for _ in 0..500 {
            if started.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(started.exists(), "the smudge filter never ran");
        assert!(delete(&state, &session.id).await.unwrap());
        assert!(done.exists(), "the worktree add was stopped halfway");
        assert!(state.db.agent(&session.id).unwrap().is_none());
        assert!(!Path::new(&worktree).exists());
        assert_eq!(
            crate::git::tests::run(&fx.clone.path, &["branch", "--list", &branch]),
            ""
        );
        fn find_locks(dir: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    find_locks(&p, out);
                } else if e.file_name().to_string_lossy().ends_with(".lock") {
                    out.push(p);
                }
            }
        }
        let mut locks = Vec::new();
        find_locks(&fx.clone.path.join(".git/refs"), &mut locks);
        let packed = fx.clone.path.join(".git/packed-refs.lock");
        if packed.exists() {
            locks.push(packed);
        }
        assert!(locks.is_empty(), "stale ref locks: {locks:?}");
    }

    /// A worktree creation that fails fails the turn; the next prompt
    /// tries again.
    #[tokio::test]
    async fn a_failed_creation_retries_with_the_next_prompt() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_claude(&fx, dir.path(), CLAUDE_OK);
        // A file where the worktree's parent directory goes makes the
        // creation fail.
        let worktrees = dir.path().join("worktrees");
        std::fs::create_dir_all(&worktrees).unwrap();
        let blocker = worktrees.join("owner");
        std::fs::write(&blocker, "x").unwrap();
        let session = create(&state, new_repo_session()).await.unwrap();
        wait_for_turns(&state, &session.id, 1).await;
        let failed = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(failed.status, AgentStatus::Failed);
        assert!(failed.base_sha.is_empty());
        let events = state.db.agent_events(&session.id).unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, AgentEventKind::Error(_)))
        );

        std::fs::remove_file(&blocker).unwrap();
        send(
            &state,
            &session.id,
            "again".into(),
            PermissionMode::from("auto"),
            None,
        );
        wait_for_turns(&state, &session.id, 2).await;
        let done = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(done.status, AgentStatus::Idle);
        assert!(!done.base_sha.is_empty());
        assert!(std::path::Path::new(&done.worktree).join(".git").exists());
    }

    /// A session left `Creating` when the server stopped comes back
    /// interrupted; its next prompt builds the worktree.
    #[tokio::test]
    async fn a_creation_interrupted_comes_back_on_the_next_prompt() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_claude(&fx, dir.path(), CLAUDE_OK);
        let cloned = state.repos["owner/name"].clone();
        let held = cloned.git_lock.lock().await;
        let session = create(&state, new_repo_session()).await.unwrap();
        // The queued prompt dies with the server, as does `Creating`.
        interrupt(&state, &session.id);
        state.db.interrupt_agents().unwrap();
        assert_eq!(
            state.db.agent(&session.id).unwrap().unwrap().status,
            AgentStatus::Interrupted
        );
        drop(held);
        send(
            &state,
            &session.id,
            "go on".into(),
            PermissionMode::from("auto"),
            None,
        );
        wait_for_turns(&state, &session.id, 1).await;
        let done = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(done.status, AgentStatus::Idle);
        assert!(!done.base_sha.is_empty());
        assert!(std::path::Path::new(&done.worktree).join(".git").exists());
    }

    /// A state whose agents run `script` as claude, with the git fixture's
    /// clone configured.
    pub(crate) fn state_with_claude(
        fx: &crate::git::tests::Fixture,
        dir: &std::path::Path,
        script: &str,
    ) -> Arc<State> {
        let claude = dir.join("claude");
        crate::test_util::write_executable(&claude, script);
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
        for _ in 0..300 {
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
        // The notes in --append-system-prompt carry newlines of their
        // own; the input message is the log's last line.
        let args = log.lines().next().unwrap();
        let input = log.lines().last().unwrap();
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
    /// `$prompt`, for fake claudes in shell. Escaped quotes are
    /// unescaped; newlines stay escaped, so match single-line text.
    const READ_PROMPT: &str =
        r#"prompt=$(sed -n 's/.*"text":"\(\(\\.\|[^\\"]\)*\)".*/\1/p' | sed 's/\\"/"/g')"#;

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
                draft: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(session.title, "-first");
        let mut events = state.agents.subscribe(&session.id);
        wait_for_turns(&state, &session.id, 1).await;
        assert_eq!(
            state.db.agent(&session.id).unwrap().unwrap().base_sha,
            crate::git::tests::run(&fx.upstream, &["rev-parse", "HEAD"])
        );
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
        // The notes in --append-system-prompt carry newlines of their
        // own, so each logged call runs past one line; calls start with
        // "-p ".
        let mut calls = log.split("\n-p ");
        let first = calls.next().unwrap();
        let calls: Vec<String> = calls.map(|c| format!("-p {c}")).collect();
        let [second] = calls.as_slice() else {
            panic!("{log}")
        };
        assert!(
            first.starts_with(&format!("-p --session-id {id} ")),
            "{log}"
        );
        assert!(first.contains(" --permission-mode auto "), "{log}");
        assert!(first.contains(" --model sonnet "), "{log}");
        assert!(first.ends_with(" | -first | 1"), "{log}");
        assert!(second.starts_with(&format!("-p --resume {id} ")), "{log}");
        assert!(second.contains(" --permission-mode manual "), "{log}");
        assert!(!second.contains("--model"), "{log}");
        assert!(second.contains("--permission-prompt-tool mcp__grenadine__approve"));
        assert!(
            second.contains(
                " --allowedTools mcp__grenadine__list_terminals mcp__grenadine__read_terminal \
                 mcp__grenadine__wait_for_terminal mcp__grenadine__show_image \
                 mcp__grenadine__update_prs mcp__grenadine__land_on_checkout \
                 mcp__grenadine__session_pr mcp__grenadine__manage_prs --mcp-config "
            ),
            "{log}"
        );
        assert!(second.trim_end().ends_with(" | second | 1"), "{log}");

        // Pages get the stored events and the streamed text.
        let mut live = Vec::new();
        while let Ok(e) = events.try_recv() {
            live.push(e);
        }
        let has = |kind: AgentEventKind| {
            live.iter()
                .any(|e| e.seq.is_none() && e.at.is_some() && e.kind == kind)
        };
        assert!(has(AgentEventKind::TextDelta("Hi".into())));
        assert!(has(AgentEventKind::Usage {
            tokens: Some(15),
            window: None
        }));
        assert!(has(AgentEventKind::Model("claude-test".into())));
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
                draft: None,
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
            draft: None,
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

        let err = land(&state, &session.id, WrapUpTarget::Checkout, None, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing to land"), "{err}");

        assert!(delete(&state, &session.id).await.unwrap());
        assert!(!worktree.exists());
        assert_eq!(state.db.agent(&session.id).unwrap(), None);
    }

    /// Jobs' directories go in the runtime directory if it's private,
    /// else in a private one in the cache directory.
    #[test]
    fn private_dirs_have_private_parents() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("run");
        std::fs::create_dir(&runtime).unwrap();
        let home = dir.path().join("home");
        let some = |p: &Path| Some(p.as_os_str().to_owned());
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            private_parent(some(&runtime), None, some(&home)).unwrap(),
            runtime
        );
        // One that others may use doesn't count.
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o777)).unwrap();
        let jobs = private_parent(some(&runtime), None, some(&home)).unwrap();
        assert_eq!(jobs, home.join(".cache/grenadine/jobs"));
        let mode = std::fs::metadata(&jobs).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let cache = dir.path().join("cache");
        assert_eq!(
            private_parent(None, some(&cache), some(&home)).unwrap(),
            cache.join("grenadine/jobs")
        );
        assert!(private_parent(None, None, None).is_err());
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
  pwd > {dir}/summary_cwd
  cat > {dir}/summary_input
  echo '{{"type":"result","is_error":false,"structured_output":{{"summary":"Write a note"}}}}'
  ;;
*" --no-session-persistence "*)
  read -r request
  echo '{{"type":"control_response","response":{{"request_id":"models","response":{{"models":[{{"value":"claude-test","displayName":"Test","description":""}}]}}}}}}'
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
                draft: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(session.summary, None);

        let mut summary = None;
        for _ in 0..300 {
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
        // In a private directory of its own, which is gone again.
        let cwd = std::fs::read_to_string(dir.path().join("summary_cwd")).unwrap();
        let cwd = Path::new(cwd.trim());
        assert!(
            cwd.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("grenadine-"),
            "{}",
            cwd.display()
        );
        assert!(!cwd.exists(), "{} is left", cwd.display());
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
*" --no-session-persistence "*)
  read -r request
  echo '{{"type":"control_response","response":{{"request_id":"models","response":{{"models":[{{"value":"claude-test","displayName":"Test","description":""}}]}}}}}}'
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
                draft: None,
            },
        )
        .await
        .unwrap();
        wait_for_turns(&state, &session.id, 1).await;
        (fx, state, session)
    }

    /// Waits for the scans to end and returns the cleanup items.
    async fn wait_for_scans(state: &State) -> Vec<grenadine_core::api::CleanupItem> {
        for _ in 0..300 {
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
        assert!(!args.contains("git log"), "{args}");
        // Each Bash command goes through the hook, which checks its
        // options.
        assert!(args.contains("--settings {\"hooks\""), "{args}");
        assert!(args.contains(crate::cleanup::BASH_HOOK_ARG), "{args}");
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
        // The notes in --append-system-prompt carry newlines of their
        // own; a call's arguments are on its "-p " line.
        let last = turns.lines().rfind(|l| l.starts_with("-p ")).unwrap();
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
case " $* " in
*" --input-format "*)
  read -r request
  echo '{{"type":"control_response","response":{{"request_id":"models","response":{{"models":[{{"value":"claude-test","displayName":"Test","description":""}}]}}}}}}'
  ;;
*)
  echo "$*" > {dir}/prune_args
  pwd > {dir}/prune_cwd
  cat > {dir}/prune_input
  cat {dir}/answer
  ;;
esac
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
                draft: None,
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
                draft: None,
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
                draft: None,
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
        for _ in 0..750 {
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
            parent: None,
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
                branch: "b".into(),
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
                draft: None,
            },
        )
        .await
        .unwrap();
        wait_for_turns(state, &session.id, 1).await;
        // The turn built the worktree and set the base.
        state.db.agent(&session.id).unwrap().unwrap()
    }

    /// Records an open PR whose upstream head branch is at `head`, with
    /// the head fetched into the clone as a sync would have it.
    fn store_pr(
        state: &Arc<State>,
        fx: &crate::git::tests::Fixture,
        number: u64,
        branch: &str,
        head: &str,
    ) {
        store_pr_on(state, fx, number, branch, "main", head, None);
    }

    /// Records an open PR like `store_pr`, into `base` and in `stack`.
    fn store_pr_on(
        state: &Arc<State>,
        fx: &crate::git::tests::Fixture,
        number: u64,
        branch: &str,
        base: &str,
        head: &str,
        stack: Option<&Stack>,
    ) {
        use grenadine_core::api::{Version, VersionKind};
        crate::git::tests::run(
            &fx.clone.path,
            &[
                "fetch",
                "--quiet",
                "https://github.com/owner/name.git",
                branch,
            ],
        );
        state
            .db
            .store_sync(
                &crate::db::PrMeta {
                    key: PrKey {
                        repo: fx.clone.slug.clone(),
                        number,
                    },
                    title: "t".into(),
                    body: String::new(),
                    author: "me".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                    queued: false,
                    url: "u".into(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    base_ref: base.into(),
                    head_ref: branch.into(),
                    head_oid: head.into(),
                    ci: None,
                    review: None,
                    review_error: None,
                },
                &[Version {
                    number: 1,
                    sha: head.into(),
                    merge_base: None,
                    kind: VersionKind::Initial,
                    pushed_at: None,
                    pushed_by: None,
                    pushed_by_is_guess: false,
                    missing: false,
                }],
                &[],
                stack,
                false,
                None,
            )
            .unwrap();
    }

    /// Starts a session from PR `number`'s head and waits for its turn.
    async fn start_from_pr(state: &Arc<State>, prompt: &str, number: u64) -> AgentSession {
        let session = create(
            state,
            NewAgent {
                backend: AgentBackend::Claude,
                repo: "owner/name".into(),
                pr: Some(number),
                prompt: prompt.into(),
                permission_mode: PermissionMode::from("auto"),
                model: None,
                images: Vec::new(),
                draft: None,
            },
        )
        .await
        .unwrap();
        wait_for_turns(state, &session.id, 1).await;
        // The turn built the worktree and set the base.
        state.db.agent(&session.id).unwrap().unwrap()
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
        for _ in 0..300 {
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
        let err = land_now(
            &state,
            &session.id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, &session.id).unwrap(),
        )
        .await
        .unwrap_err();
        assert!(!format!("{err:#}").contains("no commits"), "{err:#}");
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
        // What's left uncommitted becomes a third commit once the
        // wrap-up asks the agent to commit it; the retried wrap-up then
        // asks it to name the new pull requests' branches instead of
        // pushing.
        send(
            &state,
            id,
            "write d last".into(),
            PermissionMode::from("auto"),
            None,
        );
        wait_for_turns(&state, id, 3).await;

        wrap(&state, id, WrapUpTarget::PullRequest).await;
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        let commit_at = log
            .find(PR_COMMIT_PROMPT)
            .unwrap_or_else(|| panic!("{log}"));
        let naming_at = log
            .find("giving each new pull request a `branch`")
            .unwrap_or_else(|| panic!("{log}"));
        assert!(commit_at < naming_at, "{log}");
        // The naming handoff ends the wrap-up without a failure, and
        // nothing was pushed yet.
        assert!(state.agents.with_live(id, |l| l.wrap_up.is_none()));
        assert!(
            state
                .db
                .agent_events(id)
                .unwrap()
                .iter()
                .all(|e| !matches!(e.kind, AgentEventKind::WrapUpFailed(_)))
        );
        assert_eq!(
            run(&fx.upstream, &["for-each-ref", "--format=%(refname)"]),
            "refs/heads/main"
        );

        // The agent's update_prs then pushes. The test state's GitHub is
        // unreachable, so only the push works.
        let worktree = Path::new(&session.worktree);
        let specs = ["HEAD~2", "HEAD~", "HEAD"]
            .iter()
            .map(|rev| PrSpec {
                commit: run(worktree, &["rev-parse", rev]),
                pr: None,
                branch: None,
            })
            .collect();
        land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            Some(specs),
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
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

    /// A `prs` entry's `branch` names the remote branch a new pull
    /// request's commit is pushed to.
    #[tokio::test]
    async fn pushes_to_the_branch_the_agent_picked() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let worktree = Path::new(&session.worktree);
        let sha = run(worktree, &["rev-parse", "HEAD"]);
        let specs = vec![PrSpec {
            commit: sha.clone(),
            pr: None,
            branch: Some("dev/someone/topic".into()),
        }];

        // The test state's GitHub is unreachable, so only the push works.
        land_now(
            &state,
            &session.id,
            WrapUpTarget::PullRequest,
            Some(specs),
            &begin_landing(&state, &session.id).unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            run(&fx.upstream, &["rev-parse", "refs/heads/dev/someone/topic"]),
            sha
        );
        assert_eq!(
            run(
                &fx.upstream,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    &format!("refs/heads/{}", session.branch)
                ],
            ),
            ""
        );
    }

    /// A `branch` that already exists on GitHub without being one of the
    /// session's is refused: pushing to it would move someone else's
    /// branch.
    #[tokio::test]
    async fn refuses_a_branch_already_on_github() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let worktree = Path::new(&session.worktree);
        run(&fx.upstream, &["branch", "dev/someone/taken"]);
        let specs = vec![PrSpec {
            commit: run(worktree, &["rev-parse", "HEAD"]),
            pr: None,
            branch: Some("dev/someone/taken".into()),
        }];

        let err = land_now(
            &state,
            &session.id,
            WrapUpTarget::PullRequest,
            Some(specs),
            &begin_landing(&state, &session.id).unwrap(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("already exists on GitHub"), "{text}");
        assert!(text.contains("dev/someone/taken"), "{text}");
    }

    /// A `branch` that isn't a valid ref name is refused before anything
    /// is pushed.
    #[tokio::test]
    async fn refuses_an_invalid_branch_name() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let worktree = Path::new(&session.worktree);
        let specs = vec![PrSpec {
            commit: run(worktree, &["rev-parse", "HEAD"]),
            pr: None,
            branch: Some("bad..name".into()),
        }];

        let err = land_now(
            &state,
            &session.id,
            WrapUpTarget::PullRequest,
            Some(specs),
            &begin_landing(&state, &session.id).unwrap(),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("isn't a valid branch name"),
            "{err:#}"
        );
        assert_eq!(
            run(&fx.upstream, &["for-each-ref", "--format=%(refname)"]),
            "refs/heads/main"
        );
    }

    /// The page's wrap-up hands the naming of the new pull requests'
    /// branches to the agent instead of pushing.
    #[tokio::test]
    async fn wrap_up_asks_the_agent_to_name_branches() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;

        wrap(&state, &session.id, WrapUpTarget::PullRequest).await;
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(
            log.contains("giving each new pull request a `branch`"),
            "{log}"
        );
        assert!(log.contains("update_prs"), "{log}");
        // Nothing was pushed; the agent's update_prs call lands the PRs.
        assert_eq!(
            run(&fx.upstream, &["for-each-ref", "--format=%(refname)"]),
            "refs/heads/main"
        );
        assert!(state.agents.with_live(&session.id, |l| l.wrap_up.is_none()));
    }

    /// A wrap-up whose plan only updates open pull requests pushes as
    /// before, without asking the agent for branch names.
    #[tokio::test]
    async fn wrap_up_updates_open_prs_without_naming() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;
        let pr = Landed::PullRequest {
            number: 24,
            url: "https://github.com/owner/name/pull/24".into(),
            branch: session.branch.clone(),
        };
        record_landed(&state, id, &pr).unwrap();

        let event = wrap(&state, id, WrapUpTarget::PullRequest).await;
        assert_eq!(event, AgentEventKind::Landed(pr));
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(!log.contains("update_prs tool with `prs`"), "{log}");
        let worktree = Path::new(&session.worktree);
        assert_eq!(
            run(
                &fx.upstream,
                &["rev-parse", &format!("refs/heads/{}", session.branch)]
            ),
            run(worktree, &["rev-parse", "HEAD"])
        );
    }

    /// The naming prompt carries the `prs` mapping to pass: each commit
    /// bottom first, with `pr` where it updates an open pull request.
    #[tokio::test]
    async fn wrap_up_names_branches_with_an_open_pr() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;
        let worktree = Path::new(&session.worktree);
        let pr = Landed::PullRequest {
            number: 24,
            url: "https://github.com/owner/name/pull/24".into(),
            branch: session.branch.clone(),
        };
        record_landed(&state, id, &pr).unwrap();
        send(
            &state,
            id,
            "commit c more".into(),
            PermissionMode::from("auto"),
            None,
        );
        wait_for_turns(&state, id, 2).await;

        wrap(&state, id, WrapUpTarget::PullRequest).await;
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        let bottom = run(worktree, &["rev-parse", "HEAD~"]);
        let top = run(worktree, &["rev-parse", "HEAD"]);
        let json = format!("[{{\"commit\":\"{bottom}\",\"pr\":24}},{{\"commit\":\"{top}\"}}]");
        assert!(log.contains(&format!("`prs` set to `{json}`")), "{log}");
        // The agent's update_prs call hasn't happened yet, so nothing
        // was pushed.
        assert_eq!(
            run(&fx.upstream, &["for-each-ref", "--format=%(refname)"]),
            "refs/heads/main"
        );
    }

    /// The session's own landing on the checkout, as `land_on_checkout`
    /// runs it inside the calling turn.
    #[tokio::test]
    async fn lands_now_into_the_checkout() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, clone) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;

        let (Landed::Checkout { sha }, _) = land_now(
            &state,
            id,
            WrapUpTarget::Checkout,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap() else {
            panic!("expected a checkout landing");
        };
        assert_eq!(run(&clone, &["rev-parse", "HEAD"]), sha);
        let landed = state.db.agent(id).unwrap().unwrap();
        assert_eq!(landed.landed, Some(Landed::Checkout { sha: sha.clone() }));
        assert_eq!(landed.base_sha, sha);
        let event = state
            .db
            .agent_events(id)
            .unwrap()
            .last()
            .unwrap()
            .kind
            .clone();
        assert_eq!(event, AgentEventKind::Landed(Landed::Checkout { sha }));

        // A checkout the session's commits conflict with tells the agent
        // to rebase and call the tool again, without queuing a prompt.
        commit(&clone, "b", "clone\n");
        send(
            &state,
            id,
            "commit b more".into(),
            PermissionMode::from("auto"),
            None,
        );
        wait_for_turns(&state, id, 2).await;
        let err = land_now(
            &state,
            id,
            WrapUpTarget::Checkout,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("Rebase your commits onto"), "{text}");
        assert!(text.contains("call `land_on_checkout` again"), "{text}");
        let prompts = state
            .db
            .agent_events(id)
            .unwrap()
            .iter()
            .filter(|e| matches!(e.kind, AgentEventKind::Prompt(_)))
            .count();
        assert_eq!(prompts, 2);
    }

    /// A landing's PRs sync right away, so their pages don't wait for a
    /// poll. The test's GitHub is unreachable, so each sync ends in a
    /// stored error.
    #[tokio::test]
    async fn landed_prs_sync_right_away() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let pr = |number| LandedPr {
            number,
            url: format!("https://github.com/owner/name/pull/{number}"),
            branch: format!("b{number}"),
            lease: None,
        };
        record_landed(&state, &session.id, &Landed::from_prs(vec![pr(24), pr(25)])).unwrap();
        for number in [24, 25] {
            let key = PrKey {
                repo: "owner/name".into(),
                number,
            };
            tokio::time::timeout(Duration::from_secs(10), async {
                while state.db.pr_sync_error(&key).unwrap().is_none() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("#{number} wasn't synced"));
        }
    }

    /// `update_prs` pushes the session's commits to their PRs' branches,
    /// an amended one included.
    #[tokio::test]
    async fn updates_the_prs_it_opened() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;
        let worktree = Path::new(&session.worktree);
        let tip = |branch: &str| {
            run(
                &fx.upstream,
                &["rev-parse", &format!("refs/heads/{branch}")],
            )
        };

        // An earlier landing opened the session's PR.
        let pr = Landed::PullRequest {
            number: 24,
            url: "https://github.com/owner/name/pull/24".into(),
            branch: session.branch.clone(),
        };
        record_landed(&state, id, &pr).unwrap();
        run(worktree, &["commit", "--amend", "-qm", "Amended b"]);

        let (landed, _) = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(landed, pr);
        assert_eq!(tip(&session.branch), run(worktree, &["rev-parse", "HEAD"]));
        assert_eq!(
            state.db.agent(id).unwrap().unwrap().landed,
            Some(pr.clone())
        );
        let event = state
            .db
            .agent_events(id)
            .unwrap()
            .last()
            .unwrap()
            .kind
            .clone();
        assert_eq!(event, AgentEventKind::Landed(pr));
    }

    /// A new commit's branch is pushed before its PR fails to open, and
    /// the PRs already open are kept.
    #[tokio::test]
    async fn pushes_new_commits_before_opening_their_prs() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;
        let worktree = Path::new(&session.worktree);
        let tip = |branch: &str| {
            run(
                &fx.upstream,
                &["rev-parse", &format!("refs/heads/{branch}")],
            )
        };
        let pr = Landed::PullRequest {
            number: 24,
            url: "https://github.com/owner/name/pull/24".into(),
            branch: session.branch.clone(),
        };
        record_landed(&state, id, &pr).unwrap();
        send(
            &state,
            id,
            "commit c more".into(),
            PermissionMode::from("auto"),
            None,
        );
        wait_for_turns(&state, id, 2).await;

        // The test state's GitHub is unreachable, so only the push works.
        let specs = vec![
            PrSpec {
                commit: run(worktree, &["rev-parse", "HEAD~"]),
                pr: Some(24),
                branch: None,
            },
            PrSpec {
                commit: run(worktree, &["rev-parse", "HEAD"]),
                pr: None,
                branch: None,
            },
        ];
        land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            Some(specs),
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(tip(&session.branch), run(worktree, &["rev-parse", "HEAD~"]));
        assert_eq!(
            tip(&format!("{}-2", session.branch)),
            run(worktree, &["rev-parse", "HEAD"])
        );
        assert_eq!(state.db.agent(id).unwrap().unwrap().landed, Some(pr));
    }

    /// A new PR whose `prs` entry names the top of two commits takes both:
    /// only its tip is pushed, to its branch.
    #[tokio::test]
    async fn pushes_a_new_pr_spanning_two_commits() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;
        let worktree = Path::new(&session.worktree);
        let pr = Landed::PullRequest {
            number: 24,
            url: "https://github.com/owner/name/pull/24".into(),
            branch: session.branch.clone(),
        };
        record_landed(&state, id, &pr).unwrap();
        for prompt in ["commit c more", "commit d last"] {
            send(
                &state,
                id,
                prompt.into(),
                PermissionMode::from("auto"),
                None,
            );
        }
        wait_for_turns(&state, id, 3).await;

        let specs = vec![
            PrSpec {
                commit: run(worktree, &["rev-parse", "HEAD~2"]),
                pr: Some(24),
                branch: None,
            },
            PrSpec {
                commit: run(worktree, &["rev-parse", "HEAD"]),
                pr: None,
                branch: Some("dev/me/both".into()),
            },
        ];
        // The test state's GitHub is unreachable, so only the push works.
        land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            Some(specs),
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        let tip = |branch: &str| {
            run(
                &fx.upstream,
                &[
                    "for-each-ref",
                    "--format=%(objectname)",
                    &format!("refs/heads/{branch}"),
                ],
            )
        };
        assert_eq!(
            tip(&session.branch),
            run(worktree, &["rev-parse", "HEAD~2"])
        );
        assert_eq!(tip("dev/me/both"), run(worktree, &["rev-parse", "HEAD"]));
        assert_eq!(tip(&format!("{}-2", session.branch)), "");
    }

    /// Commits that reached the default branch since the session started
    /// aren't the session's own, so they aren't pushed or stacked.
    #[tokio::test]
    async fn update_prs_skips_commits_already_on_the_default_branch() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;
        let worktree = Path::new(&session.worktree);
        let tip = |branch: &str| {
            run(
                &fx.upstream,
                &["rev-parse", &format!("refs/heads/{branch}")],
            )
        };

        // The session's worktree is rebased onto a default branch that
        // moved on since the session started.
        run(&fx.upstream, &["checkout", "-q", "main"]);
        commit(&fx.upstream, "c", "1\n");
        run(
            worktree,
            &[
                "fetch",
                "--quiet",
                "https://github.com/owner/name.git",
                "main",
            ],
        );
        run(worktree, &["rebase", "--quiet", "FETCH_HEAD"]);

        // The test state's GitHub is unreachable, so only the push works.
        land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(tip(&session.branch), run(worktree, &["rev-parse", "HEAD"]));
        assert_eq!(
            run(
                &fx.upstream,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    &format!("refs/heads/{}-2", session.branch)
                ],
            ),
            ""
        );
    }

    /// A session started from a PR whose branch was force-pushed pushes
    /// only its own commits once its worktree sits on the branch's new
    /// tip, and the session's base moves to it.
    #[tokio::test]
    async fn update_prs_after_the_pr_branch_was_force_pushed() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());

        // The PR's branch, then the session started from its head.
        run(&fx.upstream, &["checkout", "-qb", "pr-branch"]);
        let old = commit(&fx.upstream, "p", "1\n");
        store_pr(&state, &fx, 5, "pr-branch", &old);
        let session = start_from_pr(&state, "commit w agent", 5).await;
        let id = &session.id;
        let worktree = Path::new(&session.worktree);
        assert_eq!(session.base_sha, old);
        let pr = Landed::PullRequest {
            number: 24,
            url: "https://github.com/owner/name/pull/24".into(),
            branch: session.branch.clone(),
        };
        record_landed(&state, id, &pr).unwrap();

        // The PR's branch is force-pushed a rewritten commit, and the
        // worktree is rebased onto it.
        run(&fx.upstream, &["checkout", "-q", "main"]);
        run(&fx.upstream, &["branch", "-f", "pr-branch", "main"]);
        run(&fx.upstream, &["checkout", "-q", "pr-branch"]);
        let new = commit(&fx.upstream, "p2", "2\n");
        run(
            worktree,
            &[
                "fetch",
                "--quiet",
                "https://github.com/owner/name.git",
                "pr-branch",
            ],
        );
        run(
            worktree,
            &["rebase", "--quiet", "--onto", "FETCH_HEAD", &old],
        );

        let (landed, _) = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(landed, pr);
        let tip = |branch: &str| {
            run(
                &fx.upstream,
                &["rev-parse", &format!("refs/heads/{branch}")],
            )
        };
        assert_eq!(tip(&session.branch), run(worktree, &["rev-parse", "HEAD"]));
        assert_eq!(
            run(
                &fx.upstream,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    &format!("refs/heads/{}-2", session.branch)
                ],
            ),
            ""
        );
        assert_eq!(state.db.agent(id).unwrap().unwrap().base_sha, new);
    }

    /// A session still on the PR branch's old head after a force-push is
    /// told to rebase onto the new tip, and nothing is pushed.
    #[tokio::test]
    async fn update_prs_asks_for_a_rebase_onto_a_force_pushed_pr_branch() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());

        run(&fx.upstream, &["checkout", "-qb", "pr-branch"]);
        let old = commit(&fx.upstream, "p", "1\n");
        store_pr(&state, &fx, 5, "pr-branch", &old);
        let session = start_from_pr(&state, "commit w agent", 5).await;
        let id = &session.id;

        run(&fx.upstream, &["checkout", "-q", "main"]);
        run(&fx.upstream, &["branch", "-f", "pr-branch", "main"]);
        run(&fx.upstream, &["checkout", "-q", "pr-branch"]);
        commit(&fx.upstream, "p2", "2\n");

        let err = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("git rebase --onto"), "{text}");
        assert_eq!(
            run(
                &fx.upstream,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    &format!("refs/heads/{}", session.branch)
                ],
            ),
            ""
        );
        assert_eq!(state.db.agent(id).unwrap().unwrap().base_sha, old);
    }

    /// Sets up a session started from PR 5, whose `n` commits are on
    /// `pr-branch`, after which main moves on and the worktree rebases the
    /// PR's commits onto it. Returns the state, the session, the PR's old
    /// head and main's new tip. The upstream is left on main.
    async fn rebased_source_pr(
        fx: &crate::git::tests::Fixture,
        dir: &Path,
        n: usize,
    ) -> (Arc<State>, AgentSession, String, String) {
        use crate::git::tests::{commit, run};
        let (state, _) = wrap_up_state(fx, dir);
        run(&fx.upstream, &["checkout", "-qb", "pr-branch"]);
        let mut old = String::new();
        for i in 1..=n {
            old = commit(&fx.upstream, &format!("p{i}"), "1\n");
        }
        store_pr(&state, fx, 5, "pr-branch", &old);
        let session = start_from_pr(&state, "look around", 5).await;
        assert_eq!(session.base_sha, old);

        run(&fx.upstream, &["checkout", "-q", "main"]);
        let main = commit(&fx.upstream, "m", "m\n");
        let worktree = Path::new(&session.worktree);
        run(
            worktree,
            &[
                "fetch",
                "--quiet",
                "https://github.com/owner/name.git",
                "main",
            ],
        );
        run(worktree, &["rebase", "--quiet", "FETCH_HEAD"]);
        (state, session, old, main)
    }

    fn upstream_tip(fx: &crate::git::tests::Fixture, branch: &str) -> String {
        crate::git::tests::run(
            &fx.upstream,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                &format!("refs/heads/{branch}"),
            ],
        )
    }

    /// A session started from a PR that rebased the PR's commit replaces
    /// it on the PR's branch once the agent maps the commit to the PR, and
    /// opens no PR of its own.
    #[tokio::test]
    async fn update_prs_replaces_the_source_prs_rebased_commit() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _, main) = rebased_source_pr(&fx, dir.path(), 1).await;
        let id = &session.id;
        let worktree = Path::new(&session.worktree);
        let head = run(worktree, &["rev-parse", "HEAD"]);

        let specs = vec![PrSpec {
            commit: head.clone(),
            pr: Some(5),
            branch: None,
        }];
        let (landed, _) = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            Some(specs),
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            landed,
            Landed::PullRequest {
                number: 5,
                url: "u".into(),
                branch: "pr-branch".into(),
            }
        );
        assert_eq!(upstream_tip(&fx, "pr-branch"), head);
        assert_eq!(run(&fx.upstream, &["rev-parse", "pr-branch~1"]), main);
        assert_eq!(upstream_tip(&fx, &session.branch), "");

        // Amending it again updates the same PR without `prs`.
        run(worktree, &["commit", "--quiet", "--amend", "-m", "Amended"]);
        let amended = run(worktree, &["rev-parse", "HEAD"]);
        let (landed, _) = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(landed.prs()[0].number, 5);
        assert_eq!(upstream_tip(&fx, "pr-branch"), amended);
        assert_eq!(upstream_tip(&fx, &session.branch), "");
    }

    /// A source PR with several commits takes all of them back when its
    /// entry names the top one, instead of being split into several PRs.
    #[tokio::test]
    async fn update_prs_replaces_all_of_a_source_prs_commits() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _, main) = rebased_source_pr(&fx, dir.path(), 3).await;
        let id = &session.id;
        let head = run(Path::new(&session.worktree), &["rev-parse", "HEAD"]);

        let specs = vec![PrSpec {
            commit: head.clone(),
            pr: Some(5),
            branch: None,
        }];
        let (landed, _) = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            Some(specs),
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(landed.prs().len(), 1);
        assert_eq!(landed.prs()[0].number, 5);
        assert_eq!(upstream_tip(&fx, "pr-branch"), head);
        assert_eq!(run(&fx.upstream, &["rev-parse", "pr-branch~3"]), main);
        assert_eq!(upstream_tip(&fx, &session.branch), "");
    }

    /// Once the session took over its source PR, leaving the PR out of
    /// `prs` would close someone else's PR and delete its branch, so it's
    /// refused.
    #[tokio::test]
    async fn update_prs_never_drops_the_source_pr() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _, _) = rebased_source_pr(&fx, dir.path(), 1).await;
        let id = &session.id;
        let worktree = Path::new(&session.worktree);
        let head = run(worktree, &["rev-parse", "HEAD"]);
        let land = |specs: Vec<PrSpec>| {
            let state = state.clone();
            let id = id.clone();
            async move {
                let landing = begin_landing(&state, &id).unwrap();
                land_now(
                    &state,
                    &id,
                    WrapUpTarget::PullRequest,
                    Some(specs),
                    &landing,
                )
                .await
            }
        };
        land(vec![PrSpec {
            commit: head.clone(),
            pr: Some(5),
            branch: None,
        }])
        .await
        .unwrap();

        let top = commit(worktree, "q", "2\n");
        let err = land(vec![PrSpec {
            commit: top,
            pr: None,
            branch: Some("dev/me/all".into()),
        }])
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("#5 is the pull request this session started from"),
            "{text}"
        );
        assert_eq!(upstream_tip(&fx, "pr-branch"), head);
        assert_eq!(upstream_tip(&fx, "dev/me/all"), "");
    }

    /// Without the commit mapped to the source PR, a rebased source PR's
    /// commit isn't pushed, and the agent is told how to replace it.
    #[tokio::test]
    async fn update_prs_explains_how_to_replace_the_source_prs_commits() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, session, old, _) = rebased_source_pr(&fx, dir.path(), 1).await;
        let id = &session.id;

        let err = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("`\"pr\": 5`"), "{text}");
        assert!(!text.contains(&format!("--onto {old} {old}")), "{text}");
        assert_eq!(upstream_tip(&fx, "pr-branch"), old);
        assert_eq!(upstream_tip(&fx, &session.branch), "");
    }

    /// The source PR's branch isn't replaced when it moved since the last
    /// sync: someone else may have pushed to it.
    #[tokio::test]
    async fn update_prs_keeps_a_source_pr_branch_that_moved_since_the_sync() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _, _) = rebased_source_pr(&fx, dir.path(), 1).await;
        let id = &session.id;
        let head = run(Path::new(&session.worktree), &["rev-parse", "HEAD"]);

        run(&fx.upstream, &["checkout", "-q", "pr-branch"]);
        let theirs = commit(&fx.upstream, "q", "2\n");
        run(&fx.upstream, &["checkout", "-q", "main"]);

        let specs = vec![PrSpec {
            commit: head,
            pr: Some(5),
            branch: None,
        }];
        let err = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            Some(specs),
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("last synced"), "{text}");
        assert_eq!(upstream_tip(&fx, "pr-branch"), theirs);
    }

    /// A stack of open PRs `(number, parent, head)`, bottom first.
    fn open_stack(prs: &[(u64, Option<u64>, &str)]) -> Stack {
        Stack {
            base_ref: "main".into(),
            prs: prs
                .iter()
                .map(|(number, parent, head)| grenadine_core::api::StackPr {
                    number: *number,
                    title: "t".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                    url: "u".into(),
                    updated_at: String::new(),
                    head_oid: (*head).into(),
                    parent: *parent,
                    queued: false,
                })
                .collect(),
            more_ancestors: false,
            more_descendants: false,
        }
    }

    /// Pushes a chain of PRs to the upstream: #40 on `pr-a` into main,
    /// #41 on `pr-b` into `pr-a`, then `extra`'s PRs, each `(number,
    /// branch, parent number, parent branch)`, and returns the heads by
    /// PR. Nothing is stored; the upstream is left on main.
    fn push_chain(
        fx: &crate::git::tests::Fixture,
        extra: &[(u64, &str, u64, &str)],
    ) -> BTreeMap<u64, String> {
        use crate::git::tests::{commit, run};
        let mut heads = BTreeMap::new();
        let prs = [(40, "pr-a", 0, "main"), (41, "pr-b", 40, "pr-a")];
        for (number, branch, _, base) in prs.iter().chain(extra) {
            run(&fx.upstream, &["checkout", "-q", base]);
            run(&fx.upstream, &["checkout", "-qb", branch]);
            heads.insert(
                *number,
                commit(&fx.upstream, &format!("f{number}"), &format!("{number}\n")),
            );
        }
        run(&fx.upstream, &["checkout", "-q", "main"]);
        heads
    }

    /// Stores each of `prs`, `(number, branch, parent number, base)`, with
    /// `stack`.
    fn store_chain(
        state: &Arc<State>,
        fx: &crate::git::tests::Fixture,
        heads: &BTreeMap<u64, String>,
        prs: &[(u64, &str, u64, &str)],
        stack: &Stack,
    ) {
        for (number, branch, _, base) in prs {
            store_pr_on(
                state,
                fx,
                *number,
                branch,
                base,
                &heads[number],
                Some(stack),
            );
        }
    }

    const CHAIN: [(u64, &str, u64, &str); 3] = [
        (40, "pr-a", 0, "main"),
        (41, "pr-b", 40, "pr-a"),
        (42, "pr-c", 41, "pr-b"),
    ];

    /// The stack of `CHAIN` with `heads`.
    fn chain_stack(heads: &BTreeMap<u64, String>) -> Stack {
        open_stack(&[
            (40, None, &heads[&40]),
            (41, Some(40), &heads[&41]),
            (42, Some(41), &heads[&42]),
        ])
    }

    /// A session started from a PR with a linear chain of PRs stacked on
    /// it manages all of them, its worktree at the top's head, and hears
    /// about each one's commits (AGENT-STACK-CHAIN-2).
    #[tokio::test]
    async fn a_session_started_below_a_stack_manages_all_of_it() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let heads = push_chain(&fx, &[(42, "pr-c", 41, "pr-b")]);
        store_chain(&state, &fx, &heads, &CHAIN, &chain_stack(&heads));

        let session = start_from_pr(&state, "look around", 40).await;
        let managed: Vec<_> = landed_prs(&session)
            .into_iter()
            .map(|p| (p.number, p.branch, p.lease))
            .collect();
        assert_eq!(
            managed,
            [
                (40, "pr-a".to_owned(), Some(heads[&40].clone())),
                (41, "pr-b".to_owned(), Some(heads[&41].clone())),
                (42, "pr-c".to_owned(), Some(heads[&42].clone())),
            ]
        );
        assert_eq!(session.pr, Some(40));
        assert_eq!(session.base_sha, heads[&42]);
        assert_eq!(
            run(Path::new(&session.worktree), &["rev-parse", "HEAD"]),
            heads[&42]
        );

        let ctx = session_pr(&state, &session).await.unwrap();
        let note = pr_note(&ctx, &landed_prs(&session));
        assert!(note.contains("this session manages it and them"), "{note}");
        assert!(
            note.contains(&format!(
                "#41 u on branch `pr-b`, commits {}..{}",
                heads[&40], heads[&41]
            )),
            "{note}"
        );
        assert!(note.contains("(#40, #41, #42)"), "{note}");
        // Nothing was pushed yet, so there's nothing landed to note.
        assert_eq!(landed_note(&session), None);
    }

    /// A stack that forks above the starting PR isn't taken over, and the
    /// note says why and what to do (AGENT-STACK-FORK-1).
    #[tokio::test]
    async fn a_forked_stack_isnt_taken_over() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let prs = [
            (40, "pr-a", 0, "main"),
            (41, "pr-b", 40, "pr-a"),
            (43, "pr-d", 40, "pr-a"),
        ];
        let heads = push_chain(&fx, &prs[2..]);
        let stack = open_stack(&[
            (40, None, &heads[&40]),
            (41, Some(40), &heads[&41]),
            (43, Some(40), &heads[&43]),
        ]);
        store_chain(&state, &fx, &heads, &prs, &stack);

        let session = start_from_pr(&state, "look around", 40).await;
        assert_eq!(session.landed, None);
        assert_eq!(session.base_sha, heads[&40]);
        let ctx = session_pr(&state, &session).await.unwrap();
        let note = pr_note(&ctx, &landed_prs(&session));
        assert!(note.contains("the stack forks: #41, #43"), "{note}");
        assert!(note.contains("manage_prs"), "{note}");

        // Nor can the session manage both sides of the fork.
        let err = plan_takeover(&state, &session, &[41, 43]).unwrap_err();
        assert!(format!("{err:#}").contains("linear chain"), "{err:#}");
    }

    /// Sets up a session started from #40 before #41 and #42 were stacked
    /// on it, so that it manages nothing, and stores them afterwards.
    /// Returns the state, the session and the heads.
    async fn session_below_a_later_stack(
        fx: &crate::git::tests::Fixture,
        dir: &Path,
    ) -> (Arc<State>, AgentSession, BTreeMap<u64, String>) {
        let (state, _) = wrap_up_state(fx, dir);
        let heads = push_chain(fx, &[(42, "pr-c", 41, "pr-b")]);
        let alone = open_stack(&[(40, None, &heads[&40])]);
        store_chain(&state, fx, &heads, &CHAIN[..1], &alone);
        let session = start_from_pr(&state, "look around", 40).await;
        assert_eq!(session.landed, None);
        store_chain(&state, fx, &heads, &CHAIN, &chain_stack(&heads));
        (state, session, heads)
    }

    /// `manage_prs` takes only a contiguous linear chain
    /// (AGENT-STACK-CHAIN-1).
    #[tokio::test]
    async fn manage_prs_refuses_a_gap() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _) = session_below_a_later_stack(&fx, dir.path()).await;
        let err = plan_takeover(&state, &session, &[42]).unwrap_err();
        assert!(format!("{err:#}").contains("contiguous"), "{err:#}");
        // Naming only the PR it started from manages that alone.
        assert_eq!(plan_takeover(&state, &session, &[40]).unwrap().added, [40]);
        assert_eq!(state.db.agent(&session.id).unwrap().unwrap().landed, None);
    }

    /// Taking PRs another session manages asks first and moves them out
    /// of that session's PRs; that session then neither pushes to nor
    /// closes them (AGENT-STACK-XFER-1, AGENT-STACK-XFER-2).
    #[tokio::test]
    async fn manage_prs_moves_prs_from_another_session() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, session, heads) = session_below_a_later_stack(&fx, dir.path()).await;
        let worktree = Path::new(&session.worktree);
        // Another session opened #41 and #42 on #40.
        let other = start(&state, "other work").await;
        let theirs = |number: u64, branch: &str| LandedPr {
            number,
            url: "u".into(),
            branch: branch.into(),
            lease: None,
        };
        record_landed(
            &state,
            &other.id,
            &Landed::from_prs(vec![theirs(41, "pr-b"), theirs(42, "pr-c")]),
        )
        .unwrap();

        let plan = plan_takeover(&state, &session, &[41, 42]).unwrap();
        assert_eq!(plan.added, [40, 41, 42]);
        assert_eq!(plan.source, Some(40));
        assert_eq!(
            plan.question().unwrap(),
            format!("Take #41, #42 from session {}?", short_id(&other.id))
        );
        let text = take_over(&state, &session, plan).await.unwrap();
        assert!(text.contains("HEAD wasn't changed"), "{text}");
        assert!(
            text.contains(&format!(
                "#42 u on branch `pr-c`: commits {}..{}, fetched as {STACK_REF}/42; not in HEAD yet",
                heads[&41], heads[&42]
            )),
            "{text}"
        );
        assert!(text.contains("#40 u on branch `pr-a`"), "{text}");
        assert!(text.contains("already in HEAD"), "{text}");
        assert_eq!(run(worktree, &["rev-parse", "HEAD"]), heads[&40]);
        assert_eq!(
            run(&fx.clone.path, &["rev-parse", &format!("{STACK_REF}/42")]),
            heads[&42]
        );

        let mine = state.db.agent(&session.id).unwrap().unwrap();
        let numbers: Vec<u64> = landed_prs(&mine).iter().map(|p| p.number).collect();
        assert_eq!(numbers, [40, 41, 42]);
        let left = state.db.agent(&other.id).unwrap().unwrap();
        assert_eq!(left.landed, None);
        // It still shows them.
        let shown: Vec<u64> = state
            .db
            .agent_pr_list(&other.id)
            .unwrap()
            .iter()
            .map(|p| p.key.number)
            .collect();
        assert_eq!(shown, [41, 42]);

        // The other session's update_prs doesn't touch them.
        let other_worktree = Path::new(&other.worktree);
        let top = commit(other_worktree, "o", "o\n");
        for specs in [
            None,
            Some(vec![PrSpec {
                commit: top.clone(),
                pr: Some(41),
                branch: None,
            }]),
        ] {
            let err = land_now(
                &state,
                &other.id,
                WrapUpTarget::PullRequest,
                specs,
                &begin_landing(&state, &other.id).unwrap(),
            )
            .await
            .unwrap_err();
            let text = format!("{err:#}");
            assert!(
                text.contains(&format!("by session {}", short_id(&session.id))),
                "{text}"
            );
        }
        assert_eq!(upstream_tip(&fx, "pr-b"), heads[&41]);
        assert_eq!(upstream_tip(&fx, "pr-c"), heads[&42]);
    }

    /// Rewrites the worktree's history onto main as `a2` for #40's commit,
    /// then #41's and #42's changes, and returns the new commits.
    fn rewrite_chain(worktree: &Path) -> [String; 3] {
        use crate::git::tests::{commit, run};
        run(worktree, &["reset", "-q", "--hard", "HEAD~3"]);
        [
            commit(worktree, "f40", "40 amended\n"),
            commit(worktree, "f41", "41\n"),
            commit(worktree, "f42", "42\n"),
        ]
    }

    /// After taking over a stack, update_prs replaces the source PR's
    /// commits and updates the PRs above it on their own branches
    /// (AGENT-STACK-LEASE-1).
    #[tokio::test]
    async fn update_prs_updates_a_taken_stack() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let heads = push_chain(&fx, &[(42, "pr-c", 41, "pr-b")]);
        store_chain(&state, &fx, &heads, &CHAIN, &chain_stack(&heads));
        let session = start_from_pr(&state, "fix it", 40).await;
        let id = &session.id;
        let main = run(&fx.upstream, &["rev-parse", "main"]);
        let new = rewrite_chain(Path::new(&session.worktree));

        let specs = [40, 41, 42]
            .into_iter()
            .zip(&new)
            .map(|(pr, commit)| PrSpec {
                commit: commit.clone(),
                pr: Some(pr),
                branch: None,
            })
            .collect();
        let (landed, _) = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            Some(specs),
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap();
        let prs: Vec<_> = landed
            .prs()
            .into_iter()
            .map(|p| (p.number, p.branch, p.lease))
            .collect();
        assert_eq!(
            prs,
            [
                (40, "pr-a".to_owned(), None),
                (41, "pr-b".to_owned(), None),
                (42, "pr-c".to_owned(), None),
            ]
        );
        assert_eq!(upstream_tip(&fx, "pr-a"), new[0]);
        assert_eq!(upstream_tip(&fx, "pr-b"), new[1]);
        assert_eq!(upstream_tip(&fx, "pr-c"), new[2]);
        assert_eq!(run(&fx.upstream, &["rev-parse", "pr-a~1"]), main);
        assert_eq!(upstream_tip(&fx, &session.branch), "");
    }

    /// The first push to a taken PR doesn't go through when its branch
    /// moved since the sync, and then nothing is pushed.
    #[tokio::test]
    async fn update_prs_leases_each_taken_pr() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let heads = push_chain(&fx, &[(42, "pr-c", 41, "pr-b")]);
        store_chain(&state, &fx, &heads, &CHAIN, &chain_stack(&heads));
        let session = start_from_pr(&state, "fix it", 40).await;
        let id = &session.id;
        let new = rewrite_chain(Path::new(&session.worktree));

        run(&fx.upstream, &["checkout", "-q", "pr-b"]);
        let pushed = commit(&fx.upstream, "q", "theirs\n");
        run(&fx.upstream, &["checkout", "-q", "main"]);

        let specs = [40, 41, 42]
            .into_iter()
            .zip(&new)
            .map(|(pr, commit)| PrSpec {
                commit: commit.clone(),
                pr: Some(pr),
                branch: None,
            })
            .collect();
        let err = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            Some(specs),
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("#41's branch pr-b"), "{text}");
        assert!(text.contains("last synced"), "{text}");
        assert_eq!(upstream_tip(&fx, "pr-a"), heads[&40]);
        assert_eq!(upstream_tip(&fx, "pr-b"), pushed);
        assert_eq!(upstream_tip(&fx, "pr-c"), heads[&42]);
        // The leases stay for the next try.
        let session = state.db.agent(id).unwrap().unwrap();
        assert!(landed_prs(&session).iter().all(|p| p.lease.is_some()));
    }

    /// A commit the target already has a patch-equivalent of is called
    /// out for the agent to rebase away instead of being pushed.
    #[tokio::test]
    async fn update_prs_asks_to_rebase_away_a_commit_already_on_the_target() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit w agent").await;
        let id = &session.id;
        let worktree = Path::new(&session.worktree);

        // The default branch gains a commit the worktree then picks up
        // as a different sha.
        let merged = commit(&fx.upstream, "m", "1\n");
        run(
            worktree,
            &[
                "fetch",
                "--quiet",
                "https://github.com/owner/name.git",
                "main",
            ],
        );
        run(worktree, &["cherry-pick", &merged]);
        let picked = run(worktree, &["rev-parse", "HEAD"]);

        let err = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("already on main"), "{text}");
        assert!(text.contains(&picked[..12]), "{text}");
        assert!(text.contains("git rebase"), "{text}");
        assert_eq!(
            run(
                &fx.upstream,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    &format!("refs/heads/{}", session.branch)
                ],
            ),
            ""
        );
    }

    /// A checkout and a worktree that share no history with each other or
    /// with the session's base can't say which commits are the session's,
    /// so the agent is asked to rebase.
    #[tokio::test]
    async fn land_on_checkout_asks_when_histories_are_unrelated() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, clone) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit w agent").await;
        let worktree = Path::new(&session.worktree);

        // The checkout moves to a root commit, and the worktree resets
        // to another.
        run(&clone, &["checkout", "--orphan", "unrelated"]);
        run(&clone, &["commit", "-qm", "unrelated"]);
        let onto = run(&clone, &["rev-parse", "HEAD"]);
        run(worktree, &["checkout", "--orphan", "other"]);
        run(worktree, &["commit", "-qm", "other"]);

        let err = land_now(
            &state,
            &session.id,
            WrapUpTarget::Checkout,
            None,
            &begin_landing(&state, &session.id).unwrap(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("don't build on the checkout"), "{text}");
        assert!(text.contains("git rebase --onto"), "{text}");
        assert_eq!(run(&clone, &["rev-parse", "HEAD"]), onto);
    }

    /// A checkout behind the trunk with a commit of its own takes the
    /// session's commits rebased onto it: the worktree still builds on
    /// the session's base, so they start after it.
    #[tokio::test]
    async fn lands_on_a_checkout_behind_the_trunk() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        // The trunk moves on before the session starts, so the clone's
        // checkout is behind it.
        commit(&fx.upstream, "x", "1\n");
        let dir = tempfile::tempdir().unwrap();
        let (state, clone) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit w agent").await;
        commit(&clone, "l", "local\n");

        let (Landed::Checkout { sha }, _) = land_now(
            &state,
            &session.id,
            WrapUpTarget::Checkout,
            None,
            &begin_landing(&state, &session.id).unwrap(),
        )
        .await
        .unwrap() else {
            panic!("expected a checkout landing");
        };
        let worktree = Path::new(&session.worktree);
        assert_eq!(run(&clone, &["rev-parse", "HEAD"]), sha);
        assert_eq!(run(worktree, &["rev-parse", "HEAD"]), sha);
        // The checkout's own commit is the landed head's parent, so only
        // the session's commit was rebased onto it.
        assert_eq!(run(&clone, &["log", "--format=%s", "-2"]), "Change w\nl");
    }

    /// Commits landed on the checkout aren't pushed again: the session's
    /// base moved to them, so a follow-up lands in the PR alone.
    #[tokio::test]
    async fn update_prs_after_landing_on_the_checkout() {
        use crate::git::tests::run;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, clone) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;
        let worktree = Path::new(&session.worktree);
        let tip = |branch: &str| {
            run(
                &fx.upstream,
                &["rev-parse", &format!("refs/heads/{branch}")],
            )
        };

        // The first commit lands on the checkout.
        let (Landed::Checkout { sha }, _) = land_now(
            &state,
            id,
            WrapUpTarget::Checkout,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap() else {
            panic!("expected a checkout landing");
        };
        assert_eq!(run(&clone, &["rev-parse", "HEAD"]), sha);
        assert_eq!(state.db.agent(id).unwrap().unwrap().base_sha, sha);

        // A follow-up commit goes to the session's branch on its own.
        send(
            &state,
            id,
            "commit c more".into(),
            PermissionMode::from("auto"),
            None,
        );
        wait_for_turns(&state, id, 2).await;

        // The test state's GitHub is unreachable, so only the push works.
        land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(tip(&session.branch), run(worktree, &["rev-parse", "HEAD"]));
        assert_eq!(
            run(
                &fx.upstream,
                &["rev-parse", &format!("refs/heads/{}~1", session.branch)],
            ),
            sha
        );
        assert_eq!(
            run(
                &fx.upstream,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    &format!("refs/heads/{}-2", session.branch)
                ],
            ),
            ""
        );
    }

    /// A worktree that isn't ready gets an instruction, not a prompt.
    #[tokio::test]
    async fn update_prs_tells_the_agent_to_commit() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "write b agent").await;
        let id = &session.id;

        let err = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("uncommitted changes"), "{text}");
        assert!(text.contains("Commit all of your changes"), "{text}");
        assert!(!text.contains("Wrap up:"), "{text}");
        assert!(text.contains("call `update_prs` again"), "{text}");
        let prompts = state
            .db
            .agent_events(id)
            .unwrap()
            .iter()
            .filter(|e| matches!(e.kind, AgentEventKind::Prompt(_)))
            .count();
        assert_eq!(prompts, 1);
    }

    /// Without `prs`, fewer commits than open PRs don't say which PR
    /// lost its commits, so the agent is asked for each PR's tip.
    #[tokio::test]
    async fn update_prs_asks_for_tips_without_a_commit_per_pr() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let pr = |number| LandedPr {
            number,
            url: format!("https://github.com/owner/name/pull/{number}"),
            branch: String::new(),
            lease: None,
        };
        record_landed(&state, &session.id, &Landed::from_prs(vec![pr(24), pr(25)])).unwrap();

        let err = land_now(
            &state,
            &session.id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, &session.id).unwrap(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("The 1 commits don't map one per pull request onto the session's 2"),
            "{text}"
        );
        assert!(text.contains("Pass `prs` set to"), "{text}");
        assert!(text.contains("names a pull request's tip commit"), "{text}");
        assert!(text.contains("#24, #25"), "{text}");
        assert!(text.contains("Then call `update_prs` again."), "{text}");
    }

    /// Without `prs`, a commit added on top of an open PR doesn't say
    /// whether it's a new PR or part of the open one.
    #[tokio::test]
    async fn update_prs_asks_for_tips_for_a_new_commit() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;
        let pr = Landed::PullRequest {
            number: 24,
            url: "https://github.com/owner/name/pull/24".into(),
            branch: session.branch.clone(),
        };
        record_landed(&state, id, &pr).unwrap();
        send(
            &state,
            id,
            "commit c more".into(),
            PermissionMode::from("auto"),
            None,
        );
        wait_for_turns(&state, id, 2).await;

        let err = land_now(
            &state,
            id,
            WrapUpTarget::PullRequest,
            None,
            &begin_landing(&state, id).unwrap(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("The 2 commits don't map"), "{text}");
        assert_eq!(
            crate::git::tests::run(&fx.upstream, &["for-each-ref", "--format=%(refname)"]),
            "refs/heads/main"
        );
    }

    /// The tools wait when the user is wrapping the session up from the
    /// page already, and unattended reviews never land.
    #[tokio::test]
    async fn land_now_refuses_a_pending_wrap_up_and_reviews() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        state.agents.with_live(&session.id, |l| {
            l.wrap_up = Some(Pending {
                target: WrapUpTarget::Checkout,
                mode: PermissionMode::from("auto"),
                model: None,
                asked: false,
            });
        });
        let err = begin_landing(&state, &session.id).err().unwrap();
        assert!(
            format!("{err:#}").contains("wrapping up this session"),
            "{err:#}"
        );
        state.agents.with_live(&session.id, |l| l.wrap_up = None);

        let review = AgentSession {
            kind: SessionKind::AutoReview,
            ..session_row(&uuid(), "o/n", dir.path())
        };
        state.db.create_agent(&review).unwrap();
        for target in [WrapUpTarget::Checkout, WrapUpTarget::PullRequest] {
            let landing = begin_landing(&state, &review.id).unwrap();
            let err = land_now(&state, &review.id, target, None, &landing)
                .await
                .unwrap_err();
            assert!(format!("{err:#}").contains("review"), "{err:#}");
        }
    }

    /// One landing at a time: a second call fails while a landing's guard
    /// lives, and a dropped guard doesn't bring back a deleted session's
    /// entry.
    #[test]
    fn begin_landing_allows_only_one_landing() {
        let state = crate::sync::test_state();
        let id = uuid();
        let landing = begin_landing(&state, &id).unwrap();
        let err = begin_landing(&state, &id).err().unwrap();
        assert!(
            format!("{err:#}").contains("landing this session's work already"),
            "{err:#}"
        );
        drop(landing);
        let landing = begin_landing(&state, &id).unwrap();
        state.agents.live.lock().unwrap().remove(&id);
        drop(landing);
        assert!(!state.agents.live.lock().unwrap().contains_key(&id));
    }

    /// A wrap-up started while a landing tool call holds the session
    /// fails instead of landing the same work over it.
    #[tokio::test]
    async fn a_landing_fails_a_wrap_up() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = wrap_up_state(&fx, dir.path());
        let session = start(&state, "commit b agent").await;
        let id = &session.id;
        let _landing = begin_landing(&state, id).unwrap();
        let AgentEventKind::WrapUpFailed(message) = wrap(&state, id, WrapUpTarget::Checkout).await
        else {
            panic!("{:?}", state.db.agent_events(id));
        };
        assert!(
            message.contains("landing this session's work already"),
            "{message}"
        );
    }

    /// Repo sessions hear about the landing tools; general sessions and
    /// unattended reviews don't.
    #[test]
    fn tells_agents_about_the_landing_tools() {
        let session = |repo: &str, kind| AgentSession {
            repo: repo.into(),
            kind,
            ..session_row("abcdef01-2345", repo, Path::new("/w"))
        };
        let note = prs_note(&session("o/n", SessionKind::Interactive)).unwrap();
        assert!(note.contains("update_prs"), "{note}");
        assert!(note.contains("land_on_checkout"), "{note}");
        assert_eq!(prs_note(&session(GENERAL, SessionKind::Interactive)), None);
        assert_eq!(prs_note(&session("o/n", SessionKind::AutoReview)), None);
    }

    /// Sessions are told to show images through the MCP tool, not
    /// Markdown links to files.
    #[test]
    fn tells_agents_about_showing_images() {
        let note = images_note();
        assert!(note.contains("show_image"), "{note}");
    }

    /// A version of a stored PR, in the shape `store_sync` takes.
    fn pr_version(number: u32, sha: &str, merge_base: Option<&str>) -> Version {
        Version {
            number,
            sha: sha.into(),
            merge_base: merge_base.map(str::to_owned),
            kind: VersionKind::Initial,
            pushed_at: None,
            pushed_by: None,
            pushed_by_is_guess: false,
            missing: false,
        }
    }

    /// Records an open PR in repo `o/n` without fetching anything.
    fn store_pr_meta(state: &Arc<State>, number: u64, versions: &[Version], stack: Option<&Stack>) {
        state
            .db
            .store_sync(
                &crate::db::PrMeta {
                    key: PrKey {
                        repo: "o/n".into(),
                        number,
                    },
                    title: "The PR".into(),
                    body: String::new(),
                    author: "me".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                    queued: false,
                    url: format!("https://github.com/o/n/pull/{number}"),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    base_ref: "main".into(),
                    head_ref: "feature".into(),
                    head_oid: versions.last().map(|v| v.sha.clone()).unwrap_or_default(),
                    ci: None,
                    review: None,
                    review_error: None,
                },
                versions,
                &[],
                stack,
                false,
                None,
            )
            .unwrap();
    }

    /// A session in repo `o/n` started from PR `number` at `base_sha`.
    fn pr_session(number: u64, base_sha: &str, kind: SessionKind) -> AgentSession {
        AgentSession {
            pr: Some(number),
            base_sha: base_sha.into(),
            kind,
            ..session_row("abcdef01-2345", "o/n", Path::new("/w"))
        }
    }

    /// A session started from a PR hears which commits are the PR's own:
    /// the stored merge base with its target branch — a stacked PR's
    /// parent branch, not the trunk — through the session's base.
    #[tokio::test]
    async fn tells_the_agent_the_prs_commit_range() {
        let state = crate::sync::test_state();
        let base = "b".repeat(40);
        let parent_tip = "a".repeat(40);
        store_pr_meta(&state, 7, &[pr_version(1, &base, Some(&parent_tip))], None);

        for kind in [SessionKind::Interactive, SessionKind::AutoReview] {
            let session = pr_session(7, &base, kind);
            let ctx = session_pr(&state, &session).await.unwrap();
            assert_eq!(ctx.merge_base.as_deref(), Some(parent_tip.as_str()));
            let note = pr_note(&ctx, &[]);
            assert!(
                note.contains(
                    "pull request #7 \"The PR\" (https://github.com/o/n/pull/7), \
                     which merges `feature` into `main`"
                ),
                "{note}"
            );
            assert!(
                note.contains(&format!("its commits {parent_tip}..{base}")),
                "{note}"
            );
            assert!(
                note.contains(&format!("Commits after {base} are this session's own work")),
                "{note}"
            );
            assert!(note.contains("git merge-base @{u} HEAD"), "{note}");
            assert!(note.contains("session_pr"), "{note}");
            assert!(!note.contains("moved on"), "{note}");
        }
    }

    /// Without the version's merge base, the range comes from the PR's
    /// `target` ref in the clone.
    #[tokio::test]
    async fn the_pr_range_falls_back_to_the_target_ref() {
        use crate::git::tests::{commit, run};
        let f = crate::git::tests::fixture();
        let main = run(&f.upstream, &["rev-parse", "HEAD"]);
        run(&f.upstream, &["checkout", "-q", "-b", "feature"]);
        let head = commit(&f.upstream, "p", "1\n");
        run(&f.upstream, &["checkout", "-q", "main"]);
        f.clone.fetch_commits(std::slice::from_ref(&head));
        f.clone
            .update_refs(&[(format!("{REF_PREFIX}/7/target"), main.clone())], &[])
            .unwrap();
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: f.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let state =
            crate::sync::test_state_with([("o/n".to_owned(), cloned)].into_iter().collect());
        store_pr_meta(&state, 7, &[pr_version(1, &head, None)], None);

        let session = pr_session(7, &head, SessionKind::Interactive);
        let ctx = session_pr(&state, &session).await.unwrap();
        assert_eq!(ctx.merge_base.as_deref(), Some(main.as_str()));
        let note = pr_note(&ctx, &[]);
        assert!(
            note.contains(&format!("its commits {main}..{head}")),
            "{note}"
        );
    }

    /// A session whose PR moved on since it started hears that its
    /// worktree doesn't include the PR's new head.
    #[tokio::test]
    async fn the_pr_note_mentions_a_moved_head() {
        let state = crate::sync::test_state();
        let base = "b".repeat(40);
        let moved = "c".repeat(40);
        store_pr_meta(
            &state,
            7,
            &[
                pr_version(1, &base, Some(&"a".repeat(40))),
                pr_version(2, &moved, Some(&"a".repeat(40))),
            ],
            None,
        );

        let session = pr_session(7, &base, SessionKind::Interactive);
        let note = pr_note(&session_pr(&state, &session).await.unwrap(), &[]);
        assert!(
            note.contains(&format!(
                "The pull request has since moved on to {moved}, which this \
                 worktree doesn't include."
            )),
            "{note}"
        );
    }

    /// Trunk and general sessions get no PR note; an unsynced PR gets a
    /// shorter one.
    #[tokio::test]
    async fn only_pr_sessions_get_the_pr_note() {
        let state = crate::sync::test_state();

        let trunk = session_row("abcdef01-2345", "o/n", Path::new("/w"));
        assert!(session_pr(&state, &trunk).await.is_none());

        let general = AgentSession {
            repo: GENERAL.into(),
            pr: Some(7),
            ..session_row("abcdef01-2345", "o/n", Path::new("/w"))
        };
        assert!(session_pr(&state, &general).await.is_none());

        let unsynced = pr_session(9, &"b".repeat(40), SessionKind::Interactive);
        let ctx = session_pr(&state, &unsynced).await.unwrap();
        let note = pr_note(&ctx, &[]);
        assert!(note.contains("pull request #9"), "{note}");
        assert!(note.contains("hasn't synced yet"), "{note}");
        assert!(note.contains("session_pr"), "{note}");
    }

    /// A PR session's branch tracks the PR's target ref so `@{u}` is its
    /// base; a trunk session's has no upstream.
    #[tokio::test]
    async fn a_pr_sessions_branch_tracks_its_target() {
        use crate::git::tests::{commit, run};
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let (state, clone) = wrap_up_state(&fx, dir.path());
        run(&fx.upstream, &["checkout", "-qb", "pr-branch"]);
        let head = commit(&fx.upstream, "p", "1\n");
        store_pr(&state, &fx, 5, "pr-branch", &head);

        let session = start_from_pr(&state, "commit w agent", 5).await;
        assert_eq!(
            run(
                &clone,
                &[
                    "config",
                    "--get",
                    &format!("branch.{}.merge", session.branch)
                ]
            ),
            format!("{REF_PREFIX}/5/target")
        );
        assert_eq!(
            run(
                &clone,
                &[
                    "config",
                    "--get",
                    &format!("branch.{}.remote", session.branch)
                ]
            ),
            "."
        );

        let trunk = start(&state, "commit b agent").await;
        assert_eq!(
            run(
                &clone,
                &[
                    "config",
                    "--default",
                    "",
                    "--get",
                    &format!("branch.{}.merge", trunk.branch)
                ]
            ),
            ""
        );
    }

    /// `capture_markdown_images` with a store that takes every
    /// destination; returns the rewritten text and what it was asked
    /// for.
    fn rewrite(text: &str) -> (String, Vec<String>) {
        let mut seen = Vec::new();
        let out = capture_markdown_images(text, |dest| {
            seen.push(dest.to_owned());
            Some(format!("/api/images/{}", seen.len()))
        });
        (out, seen)
    }

    /// Relative and absolute local destinations are rewritten; `<>`-
    /// wrapped ones keep their brackets.
    #[test]
    fn captures_local_markdown_images() {
        let (out, seen) = rewrite("a ![i](shot.png) b");
        assert_eq!(out, "a ![i](/api/images/1) b");
        assert_eq!(seen, ["shot.png"]);

        let (out, _) = rewrite("![a](/abs/x.png)");
        assert_eq!(out, "![a](/api/images/1)");

        let (out, _) = rewrite("![a](<path with space.png>)");
        assert_eq!(out, "![a](</api/images/1>)");

        let (out, _) = rewrite("![a](x.png \"a title\")");
        assert_eq!(out, "![a](/api/images/1 \"a title\")");
    }

    /// URLs, stored images, declined destinations and images that don't
    /// parse as inline images stay byte-identical.
    #[test]
    fn leaves_other_markdown_images_alone() {
        for text in [
            "![a](https://e.com/x.png)",
            "![a](data:image/png;base64,AAAA)",
            "![a](file:///x.png)",
            "![a](/api/images/abc)",
            "```\n![a](x.png)\n```",
            "`![a](x.png)`",
            "![a][r]\n\n[r]: /x.png",
        ] {
            let (out, _) = rewrite(text);
            assert_eq!(out, text, "{text}");
        }
        let out = capture_markdown_images("![a](x.png)", |_| None);
        assert_eq!(out, "![a](x.png)");
    }

    /// In text with several images only the captured ones are rewritten;
    /// the rest of the text is untouched.
    #[test]
    fn captures_only_some_markdown_images() {
        let out = capture_markdown_images(
            "pre ![a](have.png) mid ![b](https://e.com/x.png) and ![c](two.png) post",
            |dest| (dest != "two.png").then(|| format!("/u/{dest}")),
        );
        assert_eq!(
            out,
            "pre ![a](/u/have.png) mid ![b](https://e.com/x.png) and ![c](two.png) post"
        );
    }

    /// Text events' Markdown images of local files are stored and
    /// rewritten to `/api/images/` URLs; missing and non-image files
    /// stay as written.
    #[test]
    fn captures_text_events_local_images() {
        let state = crate::sync::test_state();
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("shot.png"), b"\x89PNG\r\n\x1a\nrel").unwrap();
        let abs = dir.path().join("abs.png");
        std::fs::write(&abs, b"\x89PNG\r\n\x1a\nabs").unwrap();
        std::fs::write(work.join("notes.txt"), "hi").unwrap();

        let session = session_row(&uuid(), "o/n", &work);
        state.db.create_agent(&session).unwrap();
        let mut sink = TurnSink::new(&state, &session.id, "o/n");
        sink.event(AgentEventKind::Text(format!(
            "rel ![s](shot.png) abs ![a]({}) miss ![m](gone.png) txt ![t](notes.txt)",
            abs.display()
        )))
        .unwrap();

        let events = state.db.agent_events(&session.id).unwrap();
        let [event] = &events[..] else {
            panic!("{events:?}")
        };
        let AgentEventKind::Text(text) = &event.kind else {
            panic!("{event:?}")
        };
        let image_at = |marker: &str| {
            let at = text.find(marker).unwrap() + marker.len();
            text[at..at + 36].to_owned()
        };
        let rel = image_at("![s](/api/images/");
        let abs = image_at("![a](/api/images/");
        assert!(text.contains("![m](gone.png)"), "{text}");
        assert!(text.contains("![t](notes.txt)"), "{text}");
        assert_eq!(
            state.db.agent_image(&rel).unwrap(),
            Some(("image/png".into(), b"\x89PNG\r\n\x1a\nrel".to_vec()))
        );
        assert_eq!(
            state.db.agent_image(&abs).unwrap(),
            Some(("image/png".into(), b"\x89PNG\r\n\x1a\nabs".to_vec()))
        );
    }

    #[test]
    fn notes_what_landed() {
        let pr = |number| LandedPr {
            number,
            url: format!("https://github.com/o/n/pull/{number}"),
            branch: String::new(),
            lease: None,
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
        assert!(note.contains("update_prs"), "{note}");
        assert!(!note.contains("Open PR"), "{note}");
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
        assert!(args().contains("update_prs"), "{}", args());

        // The test state's GitHub is unreachable, so the PR is recorded as
        // wrapping up would have.
        let landed = Landed::PullRequest {
            number: 24,
            url: "https://github.com/owner/name/pull/24".into(),
            branch: session.branch.clone(),
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
        assert!(
            args.contains(&format!(
                "#24 https://github.com/owner/name/pull/24 from branch `{}`",
                session.branch
            )),
            "{args}"
        );
    }
}

#[cfg(test)]
pub(crate) mod devin_tests {
    use grenadine_core::api::NewAgent;
    use serde_json::json;

    use super::*;
    use crate::devin::{fake, fake_log};

    /// A state whose agents run `devin` as devin.
    pub(crate) fn state_with_devin(
        fx: &crate::git::tests::Fixture,
        dir: &Path,
        devin: PathBuf,
    ) -> Arc<State> {
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

    /// A session/update with a chunk of the answer.
    fn chunk(text: &str) -> Value {
        json!({"update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text},
        }})
    }

    /// A session/update with the process's ACU total.
    fn usage(acu: f64) -> Value {
        json!({"update": {
            "sessionUpdate": "usage_update",
            "used": 100,
            "size": 200000,
            "_meta": {"cognition.ai/totalAcuCost": acu},
        }})
    }

    /// The steps that start an `exec` call and ask permission for it,
    /// waiting for the answer unless `wait` is false.
    fn exec_asking(wait: bool) -> [Value; 2] {
        [
            json!({"update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "exec:0#t",
                "rawInput": {"command": "touch f"},
                "_meta": {"cognition.ai/inferenceToolName": "exec"},
            }}),
            json!({
                "request": "session/request_permission",
                "params": {
                    "sessionId": "s",
                    "toolCall": {"toolCallId": "exec:0#t"},
                    "options": [
                        {"optionId": "allow_once", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
                    ],
                },
                "wait": wait,
            }),
        ]
    }

    /// A scenario whose prompts take `steps`; resuming the session
    /// replays an answer.
    fn scenario(session: &str, steps: Vec<Value>) -> Value {
        json!({
            "session_id": session,
            "model": "swe-2",
            "load": [chunk("replayed")["update"]],
            "prompts": [{"steps": steps}],
        })
    }

    /// A Devin session that starts on `prompt`.
    async fn start(state: &Arc<State>, fx: &crate::git::tests::Fixture) -> AgentSession {
        create(
            state,
            NewAgent {
                backend: AgentBackend::Devin,
                repo: fx.clone.slug.clone(),
                pr: None,
                prompt: "hi".into(),
                permission_mode: "accept-edits".into(),
                model: None,
                images: Vec::new(),
                draft: None,
            },
        )
        .await
        .unwrap()
    }

    /// Waits until the session has finished `turns` turns.
    pub(crate) async fn wait_for_turns(state: &State, id: &str, turns: usize) {
        for _ in 0..300 {
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
    async fn wait_for_approval(state: &State, id: &str) {
        for _ in 0..1500 {
            if state.db.agent(id).unwrap().unwrap().status == AgentStatus::AwaitingApproval {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{id} never waited for approval");
    }

    /// Waits until the fake devin in `dir` was sent `method`.
    async fn wait_for_method(dir: &Path, method: &str) {
        for _ in 0..1500 {
            if fake_log(dir).iter().any(|m| m["method"] == method) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("devin was never sent {method}");
    }

    /// Allows the `exec` call with its option.
    fn allow(state: &State, id: &str) {
        let decision = Decision {
            allow: true,
            option: Some("allow_once".into()),
            ..Default::default()
        };
        assert!(approve(state, id, "exec:0#t", decision).unwrap());
    }

    /// A listing without models is kept like a failure, not for good.
    #[tokio::test]
    async fn an_empty_listing_is_a_failure() {
        let list = ModelList::default();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        for _ in 0..2 {
            let listed = list
                .get("Fake", || async {
                    calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(Vec::new())
                })
                .await;
            assert!(listed.is_err());
        }
        assert_eq!(calls.into_inner(), 1);
    }

    /// A devin that can't list its models stays offered, its error with
    /// it, and prompts with its default or any model that can't pass for
    /// a flag still go; devin isn't asked again for a while.
    #[tokio::test]
    async fn a_failed_listing_is_kept() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let devin = dir.path().join("devin");
        crate::test_util::write_executable(
            &devin,
            &format!("#!/bin/sh\necho run >> {}\nexit 1\n", runs.display()),
        );
        let state = state_with_devin(&fx, dir.path(), devin);
        let backends = state.agents.backends().await;
        // Only Devin is there: claude's executable isn't.
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].backend, AgentBackend::Devin);
        assert!(backends[0].models.is_empty());
        assert!(backends[0].error.is_some());
        assert!(state.agents.offered(AgentBackend::Devin).await);
        assert!(!state.agents.offered(AgentBackend::Claude).await);
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

    /// A backend whose executable isn't there isn't offered, on a
    /// repeated ask either.
    #[tokio::test]
    async fn a_missing_backend_isnt_offered() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let devin = fake(
            dir.path(),
            json!({"models": [{"value": "swe-2", "name": "SWE-2"}]}),
        );
        // claude's path doesn't exist.
        let state = state_with_devin(&fx, dir.path(), devin);
        let backends = state.agents.backends().await;
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].backend, AgentBackend::Devin);
        assert!(!state.agents.offered(AgentBackend::Claude).await);
        let backends = state.agents.backends().await;
        assert_eq!(backends.len(), 1);
        assert!(!state.agents.offered(AgentBackend::Claude).await);
    }

    /// A claude that can't list its models stays offered, its error
    /// with it.
    #[tokio::test]
    async fn a_failed_listing_offers_claude() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = tests::state_with_claude(&fx, dir.path(), "#!/bin/sh\nexit 1\n");
        let backends = state.agents.backends().await;
        // Only Claude is there: devin's executable isn't.
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].backend, AgentBackend::Claude);
        assert!(backends[0].models.is_empty());
        assert!(backends[0].error.is_some());
        assert!(state.agents.offered(AgentBackend::Claude).await);
    }

    /// A turn with a subagent costs what `agent_stopped` reports, which
    /// counts the subagent, rather than the root's total, whether it comes
    /// before the prompt's answer or after it, and it isn't billed twice.
    #[tokio::test]
    async fn a_devin_turn_costs_what_agent_stopped_reports() {
        let stopped = json!({
            "notify": "_cognition.ai/agent_stopped",
            "params": {"stats": {"acuCost": 0.4}},
        });
        let mut sub = usage(0.1);
        sub["update"]["_meta"]["cognition.ai/subagent_context"] = json!({"parentAgentId": "sub-1"});
        // A subagent's own agent_stopped doesn't count, and the wait goes
        // on for the root's.
        let sub_stopped = json!({
            "notify": "_cognition.ai/agent_stopped",
            "params": {
                "stats": {"acuCost": 0.9},
                "_meta": {"cognition.ai/subagent_context": {"parentAgentId": "sub-1"}},
            },
        });
        let end = json!({"end": "end_turn"});
        for (steps, cost) in [
            (
                vec![usage(0.25), sub.clone(), stopped.clone(), end.clone()],
                0.4,
            ),
            (
                vec![usage(0.25), sub.clone(), end.clone(), stopped.clone()],
                0.4,
            ),
            (
                vec![
                    usage(0.25),
                    sub.clone(),
                    sub_stopped,
                    end.clone(),
                    stopped.clone(),
                ],
                0.4,
            ),
            // What's billed never goes down.
            (
                vec![usage(0.5), sub.clone(), stopped.clone(), end.clone()],
                0.5,
            ),
        ] {
            let fx = crate::git::tests::fixture();
            let dir = tempfile::tempdir().unwrap();
            let devin = fake(dir.path(), scenario("turn-stopped", steps));
            let state = state_with_devin(&fx, dir.path(), devin);
            let session = start(&state, &fx).await;
            wait_for_turns(&state, &session.id, 1).await;
            let ended: Vec<_> = state
                .db
                .agent_events(&session.id)
                .unwrap()
                .into_iter()
                .filter_map(|e| match e.kind {
                    AgentEventKind::TurnEnded { cost, .. } => Some(cost),
                    _ => None,
                })
                .collect();
            assert_eq!(ended, [Some(cost)]);
            assert_eq!(state.db.agent(&session.id).unwrap().unwrap().cost, cost);
        }
    }

    /// A Devin session's turn goes over ACP: the session id is stored,
    /// its permission request waits on the page, and the turn's cost is
    /// in ACUs. The next turn resumes the session without the answers
    /// session/load replays.
    #[tokio::test]
    async fn a_devin_turn_runs_and_resumes() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let [tool_call, ask] = exec_asking(true);
        let steps = vec![
            chunk("Hi"),
            tool_call,
            ask,
            json!({"update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "exec:0#t",
                "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "done"}}],
            }}),
            usage(0.25),
            json!({"end": "end_turn"}),
        ];
        let devin = fake(dir.path(), scenario("turn-slate", steps));
        let state = state_with_devin(&fx, dir.path(), devin);
        let session = start(&state, &fx).await;

        wait_for_approval(&state, &session.id).await;
        allow(&state, &session.id);
        wait_for_turns(&state, &session.id, 1).await;
        let stored = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(stored.cost, 0.25);
        assert_eq!(stored.cost_unit, grenadine_core::api::CostUnit::Acu);
        assert_eq!(stored.model.as_deref(), Some("swe-2"));
        assert_eq!(
            state.db.backend_session(&session.id).unwrap().as_deref(),
            Some("turn-slate")
        );

        send(
            &state,
            &session.id,
            "again".into(),
            "accept-edits".into(),
            None,
        );
        wait_for_approval(&state, &session.id).await;
        allow(&state, &session.id);
        wait_for_turns(&state, &session.id, 2).await;
        let load = fake_log(dir.path())
            .into_iter()
            .find(|m| m["method"] == "session/load")
            .expect("the second turn resumed the session");
        assert_eq!(load["params"]["sessionId"], "turn-slate");
        let events = state.db.agent_events(&session.id).unwrap();
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                AgentEventKind::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Hi", "Hi"], "no replayed answer");
        // Each turn's process reports its own total.
        assert_eq!(state.db.agent(&session.id).unwrap().unwrap().cost, 0.5);
    }

    /// A prompt asked to stop before its turn got to running it never
    /// reaches Devin: the interrupt that landed while the session was
    /// being set up ends it as Interrupted without a `session/prompt`.
    #[tokio::test]
    async fn an_early_interrupt_skips_the_prompt() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let gate = dir.path().join("gate");
        let mut scenario = scenario("early-oak", vec![json!({"end": "end_turn"})]);
        scenario["gate"] = gate.to_str().unwrap().into();
        let devin = fake(dir.path(), scenario);
        let state = state_with_devin(&fx, dir.path(), devin);
        let session = start(&state, &fx).await;
        // Devin holds session/new until the interrupt is in.
        wait_for_method(dir.path(), "session/new").await;
        interrupt(&state, &session.id);
        std::fs::write(&gate, "").unwrap();
        wait_for_turns(&state, &session.id, 1).await;
        assert_eq!(
            state.db.agent(&session.id).unwrap().unwrap().status,
            AgentStatus::Interrupted
        );
        let methods: Vec<_> = fake_log(dir.path())
            .iter()
            .filter_map(|m| m["method"].as_str().map(str::to_owned))
            .collect();
        assert!(
            !methods.iter().any(|m| m == "session/prompt"),
            "fake devin received a prompt: {methods:?}"
        );
    }

    /// An interrupt asks Devin to cancel, which ends the turn, and the
    /// ACUs spent so far are billed.
    #[tokio::test]
    async fn an_interrupt_cancels() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        // Devin ignores the answer to its permission request, so only
        // session/cancel ends the turn.
        let [tool_call, ask] = exec_asking(false);
        let steps = vec![usage(0.125), tool_call, ask];
        let devin = fake(dir.path(), scenario("stop-pine", steps));
        let state = state_with_devin(&fx, dir.path(), devin);
        let session = start(&state, &fx).await;
        wait_for_approval(&state, &session.id).await;
        interrupt(&state, &session.id);
        wait_for_turns(&state, &session.id, 1).await;
        let stored = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(stored.status, AgentStatus::Interrupted);
        assert_eq!(stored.cost, 0.125);
        assert!(
            fake_log(dir.path())
                .iter()
                .any(|m| m["method"] == "session/cancel")
        );
        let ended: Vec<_> = state
            .db
            .agent_events(&session.id)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.kind {
                AgentEventKind::TurnEnded { error, .. } => Some(error),
                _ => None,
            })
            .collect();
        assert_eq!(ended, [Some("interrupted".to_owned())]);
    }

    /// A devin that dies while a permission request waits takes the
    /// request with it, and the error says what devin printed. The ACUs
    /// it reported are billed.
    #[tokio::test]
    async fn a_dead_devin_drops_its_approvals() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let [tool_call, ask] = exec_asking(false);
        let steps = vec![
            usage(0.0625),
            tool_call,
            ask,
            json!({"stderr": "Not logged in"}),
            json!({"exit": true}),
        ];
        let devin = fake(dir.path(), scenario("gone-fern", steps));
        let state = state_with_devin(&fx, dir.path(), devin);
        let session = start(&state, &fx).await;
        wait_for_turns(&state, &session.id, 1).await;
        let stored = state.db.agent(&session.id).unwrap().unwrap();
        assert_eq!(stored.status, AgentStatus::Failed);
        assert_eq!(stored.cost, 0.0625);
        let error = state
            .db
            .agent_events(&session.id)
            .unwrap()
            .into_iter()
            .find_map(|e| match e.kind {
                AgentEventKind::TurnEnded { error, .. } => error,
                _ => None,
            })
            .unwrap();
        assert!(error.contains("Not logged in"), "{error}");
        for _ in 0..1500 {
            if state
                .agents
                .with_live(&session.id, |l| l.pending.is_empty())
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the request outlived the turn");
    }
}

#[cfg(test)]
mod devin_job_tests {
    use super::devin_tests::{state_with_devin, wait_for_turns};
    use super::*;
    use crate::devin::{fake, fake_log};
    use serde_json::json;

    /// A fake devin whose prompts that contain `matching` reply with
    /// `answer` as JSON; other prompts just end.
    fn devin_job_fake(dir: &Path, matching: &str, answer: Value) -> PathBuf {
        fake(
            dir,
            json!({"models": [{"value": "swe", "name": "SWE"}], "prompts": [{"match": matching, "steps": [
                {"update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": answer.to_string()},
                }},
                {"end": "end_turn"},
            ]}]}),
        )
    }

    /// A Devin session's summary comes from a `devin acp` job.
    #[tokio::test]
    async fn a_devin_summary_goes_through_devin() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let devin = devin_job_fake(
            dir.path(),
            "Summarize it as a title",
            json!({"summary": "Write a note"}),
        );
        let state = state_with_devin(&fx, dir.path(), devin);
        state
            .agents
            .summaries
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let session = create(
            &state,
            NewAgent {
                backend: AgentBackend::Devin,
                repo: fx.clone.slug.clone(),
                pr: None,
                prompt: "please write a note".into(),
                permission_mode: "accept-edits".into(),
                model: None,
                images: Vec::new(),
                draft: None,
            },
        )
        .await
        .unwrap();
        let mut summary = None;
        for _ in 0..300 {
            summary = state.db.agent(&session.id).unwrap().unwrap().summary;
            if summary.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(summary.as_deref(), Some("Write a note"));
        wait_for_turns(&state, &session.id, 1).await;
    }

    /// Prune groups items by backend: each backend runs once with only
    /// its own item.
    #[tokio::test]
    async fn prunes_by_backend() {
        let dir = tempfile::tempdir().unwrap();
        let fx = crate::git::tests::fixture();
        let claude_script = format!(
            r#"#!/bin/sh
case " $* " in
*" --input-format "*)
  read -r request
  echo '{{"type":"control_response","response":{{"request_id":"models","response":{{"models":[{{"value":"claude-test","displayName":"Test","description":""}}]}}}}}}'
  ;;
*)
  echo "$*" >> {dir}/prune_args
  cat >> {dir}/prune_input
  cat {dir}/answer
  ;;
esac
"#,
            dir = dir.path().display()
        );
        let claude = dir.path().join("claude");
        crate::test_util::write_executable(&claude, &claude_script);
        let devin = devin_job_fake(dir.path(), "", json!({"stale": []}));
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let state = crate::sync::test_state_with_agents(
            [(fx.clone.slug.clone(), cloned)].into_iter().collect(),
            claude,
            devin,
            dir.path().join("worktrees"),
        );
        let session = |id: &str, backend: AgentBackend| AgentSession {
            backend,
            ..AgentSession::fixture(id, &fx.clone.slug)
        };
        state
            .db
            .add_cleanup_items(
                &session("claude-s", AgentBackend::Claude),
                &[("Claude leftover".into(), "d".into())],
                now(),
            )
            .unwrap();
        state
            .db
            .add_cleanup_items(
                &session("devin-s", AgentBackend::Devin),
                &[("Devin leftover".into(), "d".into())],
                now(),
            )
            .unwrap();
        let items = crate::cleanup::cleanup_state(&state).unwrap().items;
        let claude_item = items.iter().find(|i| i.title == "Claude leftover").unwrap();
        std::fs::write(
            dir.path().join("answer"),
            json!({
                "type": "result",
                "is_error": false,
                "structured_output": {"stale": [{"id": claude_item.id, "reason": "gone"}]},
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(crate::cleanup::prune(&state).await.unwrap(), 1);
        let input = std::fs::read_to_string(dir.path().join("prune_input")).unwrap();
        assert!(input.contains("Claude leftover"), "{input}");
        assert!(!input.contains("Devin leftover"), "{input}");
        let prompts: Vec<_> = fake_log(dir.path())
            .into_iter()
            .filter(|m| m["method"] == "session/prompt")
            .map(|m| m["params"].to_string())
            .collect();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert!(prompts[0].contains("Devin leftover"), "{}", prompts[0]);
        assert!(!prompts[0].contains("Claude leftover"), "{}", prompts[0]);
        let left = crate::cleanup::cleanup_state(&state).unwrap().items;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].title, "Devin leftover");
    }

    /// A deleted Devin session is scanned by Devin, from the digest
    /// alone, in the clone, in ask mode, and its items are Devin's. A scan
    /// that fails points at no Claude transcript.
    #[tokio::test]
    async fn a_devin_scan_goes_through_devin() {
        let marker = "A coding agent session was just deleted";
        for (reply, items) in [
            (
                json!({"items": [{"title": "Container e2e", "detail": "`docker rm -f e2e`"}]})
                    .to_string(),
                "Container e2e",
            ),
            ("no JSON".to_owned(), "Couldn't scan \u{201c}hi\u{201d}"),
        ] {
            let fx = crate::git::tests::fixture();
            let dir = tempfile::tempdir().unwrap();
            let devin = fake(
                dir.path(),
                json!({
                    "models": [{"value": "swe", "name": "SWE"}],
                    "prompts": [
                        {"match": marker, "steps": [
                            {"update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": reply},
                            }},
                            {"end": "end_turn"},
                        ]},
                        {"steps": [
                            {"update": {
                                "sessionUpdate": "tool_call",
                                "toolCallId": "exec:0#d",
                                "rawInput": {"command": "docker run -d --name e2e pg"},
                                "_meta": {"cognition.ai/inferenceToolName": "exec"},
                            }},
                            {"end": "end_turn"},
                        ]},
                    ],
                }),
            );
            let state = state_with_devin(&fx, dir.path(), devin);
            state
                .agents
                .scans
                .enabled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let session = create(
                &state,
                NewAgent {
                    backend: AgentBackend::Devin,
                    repo: fx.clone.slug.clone(),
                    pr: None,
                    prompt: "hi".into(),
                    permission_mode: "accept-edits".into(),
                    model: None,
                    images: Vec::new(),
                    draft: None,
                },
            )
            .await
            .unwrap();
            wait_for_turns(&state, &session.id, 1).await;
            assert!(delete(&state, &session.id).await.unwrap());
            let mut found = Vec::new();
            for _ in 0..300 {
                let c = crate::cleanup::cleanup_state(&state).unwrap();
                if c.scanning.is_empty() {
                    found = c.items;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(found.len(), 1, "{found:?}");
            assert_eq!(found[0].title, items);
            assert_eq!(found[0].backend, AgentBackend::Devin);
            assert!(
                !found[0].detail.contains("transcript"),
                "{}",
                found[0].detail
            );

            let log = fake_log(dir.path());
            let prompt = log
                .iter()
                .find(|m| m["method"] == "session/prompt" && m.to_string().contains(marker))
                .unwrap()
                .to_string();
            assert!(prompt.contains("$ docker run -d --name e2e pg"), "{prompt}");
            assert!(!prompt.contains("full transcript is"), "{prompt}");
            assert!(!prompt.contains("ls-remote"), "{prompt}");
            // The job's session runs in the clone, in ask mode, and is gone
            // again.
            let job = log
                .iter()
                .rposition(|m| m["method"] == "session/new")
                .unwrap();
            assert_eq!(
                std::fs::canonicalize(log[job]["params"]["cwd"].as_str().unwrap()).unwrap(),
                std::fs::canonicalize(&fx.clone.path).unwrap()
            );
            let methods: Vec<_> = log[job..]
                .iter()
                .filter_map(|m| m["method"].as_str())
                .collect();
            assert!(methods.contains(&"session/delete"), "{methods:?}");
            assert!(
                log[job..]
                    .iter()
                    .any(|m| m["method"] == "session/set_mode" && m["params"]["modeId"] == "ask")
            );
        }
    }

    /// While Devin isn't offered, prune leaves its items alone and doesn't
    /// ask it anything.
    #[tokio::test]
    async fn prune_skips_devin_while_it_isnt_offered() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        // devin's path doesn't exist, so Devin isn't offered.
        let state = state_with_devin(&fx, dir.path(), dir.path().join("devin"));
        let session = AgentSession {
            backend: AgentBackend::Devin,
            ..AgentSession::fixture("devin-s", &fx.clone.slug)
        };
        state
            .db
            .add_cleanup_items(&session, &[("Devin leftover".into(), "d".into())], now())
            .unwrap();
        assert_eq!(crate::cleanup::prune(&state).await.unwrap(), 0);
        let left = crate::cleanup::cleanup_state(&state).unwrap().items;
        assert_eq!(left.len(), 1);
    }

    /// A scan of a Devin session tries again while Devin isn't offered,
    /// and then leaves an item that says what to do.
    #[tokio::test]
    async fn a_devin_scan_waits_for_devin() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        // devin's path doesn't exist, so Devin isn't offered.
        let state = state_with_devin(&fx, dir.path(), dir.path().join("devin"));
        state
            .agents
            .scans
            .enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let session = AgentSession {
            backend: AgentBackend::Devin,
            title: "Run the e2e tests".into(),
            ..AgentSession::fixture("devin-s", &fx.clone.slug)
        };
        crate::cleanup::scan(&state, session, Vec::new());
        let mut found = Vec::new();
        for _ in 0..300 {
            let c = crate::cleanup::cleanup_state(&state).unwrap();
            if c.scanning.is_empty() {
                found = c.items;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].title,
            "Couldn't scan \u{201c}Run the e2e tests\u{201d}"
        );
        assert!(
            found[0].detail.contains("Devin couldn't be reached"),
            "{}",
            found[0].detail
        );
    }
}
