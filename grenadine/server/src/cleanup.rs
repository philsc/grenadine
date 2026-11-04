//! What deleted agent sessions left behind. Deleting a session stops its
//! processes and removes its worktree and Bazel output bases, but a
//! session can also leave containers, files elsewhere, pushed branches
//! and the like. A background job on the session's backend reads the
//! deleted session's transcript and reports those as cleanup items, which
//! the user can delete or hand off to a new session that cleans them up.
//! Every five minutes, a job on each backend checks its items and deletes
//! the ones that are gone or taken care of already.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use grenadine_core::api::{
    AgentBackend, AgentEvent, AgentEventKind, AgentSession, CleanupItem, CleanupState, GENERAL,
    NewAgent, NewCleanupSession, PermissionMode, ServerEvent,
};
use serde_json::{Value, json};

use crate::agent::{Budget, JobKind, JobSpec, JobTools, NotOffered};
use crate::sync::State;

/// How long a scan may take before it's killed.
const SCAN_TIMEOUT: Duration = Duration::from_secs(600);

/// What a scan may spend.
const SCAN_BUDGET: Budget = Budget { usd: 2.0, acu: 1.0 };

/// How long a scan waits to try again while its backend isn't offered,
/// as long as a failed listing of the backend's models is kept.
const SCAN_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(5 * 60)
};

/// How often a scan tries while its backend isn't offered.
const SCAN_TRIES: u32 = 7;

/// How often the cleanup items are checked for stale ones.
const PRUNE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How long a check for stale items may take before it's killed.
const PRUNE_TIMEOUT: Duration = Duration::from_secs(300);

/// What a check for stale items may spend.
const PRUNE_BUDGET: Budget = Budget { usd: 1.0, acu: 0.5 };

/// About how much of the transcript a scan gets, in bytes. Its beginning
/// and end are kept, and the full transcript is there to read.
const DIGEST_LIMIT: usize = 200_000;

/// How much of each prompt, text and tool call the digest keeps.
const PROMPT_LIMIT: usize = 2_000;
const TEXT_LIMIT: usize = 300;
const RESULT_LIMIT: usize = 300;

/// Read-only tools, so that a scan can only look, as Claude Code's
/// allowed tools. Not `gh api` or bare `git branch`, which can change
/// things too, and not `git ls-remote` or `git log`, whose options can run
/// commands or write files. Claude Code only matches these by prefix, so
/// the `bash_hook` checks each command's options as well.
pub(crate) const READ_ONLY_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "Bash(ls:*)",
    "Bash(stat:*)",
    "Bash(du:*)",
    "Bash(file:*)",
    "Bash(docker ps:*)",
    "Bash(docker images:*)",
    "Bash(docker volume ls:*)",
    "Bash(docker network ls:*)",
    "Bash(podman ps:*)",
    "Bash(podman images:*)",
    "Bash(systemctl --user list-units:*)",
    "Bash(git cat-file:*)",
    "Bash(git branch --contains:*)",
    "Bash(git branch --list:*)",
    "Bash(gh pr view:*)",
    "Bash(gh pr list:*)",
    "Bash(gh repo view:*)",
];

/// What a scan on `backend` is asked to do.
fn instructions(backend: AgentBackend) -> String {
    // Devin keeps its sessions where a scan can't read them.
    let tools = match backend {
        AgentBackend::Claude => {
            "Use tools only to read the full transcript, when the digest is cut short \
             or leaves out what subagents did, or to check whether something still \
             exists."
        }
        AgentBackend::Devin => "Use tools only to check whether something still exists.",
    };
    let agent = backend.label();
    format!(
        "\
A coding agent session was just deleted. Its git worktree has been removed, \
its processes stopped, and the Bazel output bases of the workspaces in the \
worktree deleted. Find anything else the session created that may still \
exist and that the user might want to clean up. For example: docker or \
podman containers, images, volumes and networks; files and directories \
outside the worktree (in /tmp, the home directory, caches); other git \
worktrees or clones; branches or tags pushed to remotes; installed packages \
or tools; systemd units, cron jobs or background services.

Work from the transcript digest below. {tools} You can only look; never try \
to change or remove anything.

Leave out things inside the worktree, Bazel's shared caches, {agent}'s \
own files, and things the session removed again itself. Report each finding \
with a one-line title and a markdown detail that names exactly what it is \
and where, quotes the evidence from the transcript, and says how to remove \
it. If there's nothing, report no items."
    )
}

/// How to use the tools, for both scans and checks for stale items, and
/// where the job runs, for `repo`: its clone, or a directory of its own
/// when the server has none.
fn tool_notes(repo: &str, clone: Option<&Path>) -> String {
    let place = match clone {
        Some(clone) => format!(
            "The working directory is the clone of {repo} at {}, with `origin` on \
             GitHub, so `gh pr view NUMBER --json state,headRefName` works as it is.",
            clone.display()
        ),
        None => "There is no clone here. The repos are on GitHub as `owner/name`: use \
                 `gh pr view NUMBER --repo OWNER/NAME --json state,headRefName`."
            .to_owned(),
    };
    format!(
        "Run each shell command on its own, without pipes, `&&`, `;`, redirections, \
         `$` or globs: anything else is denied, and so are options that could change \
         things. The shell commands you can run are `ls`, `stat`, `du`, `file`, \
         `docker`/`podman` listings, `systemctl --user list-units`, `git cat-file`, \
         `git branch --contains`, `git branch --list`, `gh pr view`, `gh pr list` \
         and `gh repo view`. {place}"
    )
}

/// A read-only command a cleanup job may run: its leading words, and the
/// options it may take after them. Any other option is refused, since
/// some would run commands or write files, e.g. `git log --output=FILE`.
struct ReadOnlyCommand {
    words: &'static [&'static str],
    /// The single-letter options, which may come grouped, as in `ls -la`.
    short: &'static str,
    /// The long options, with or without `=VALUE`.
    long: &'static [&'static str],
}

impl ReadOnlyCommand {
    /// Whether `words` run this command with only its options.
    fn allows(&self, words: &[String]) -> bool {
        if words.len() < self.words.len() || !words.iter().zip(self.words).all(|(w, c)| w == c) {
            return false;
        }
        let mut options = true;
        words[self.words.len()..].iter().all(|w| {
            if !options || !w.starts_with('-') || w == "-" {
                return true;
            }
            if w == "--" {
                options = false;
                return true;
            }
            match w.strip_prefix("--") {
                Some(long) => self
                    .long
                    .contains(&long.split_once('=').map_or(long, |(name, _)| name)),
                None => w[1..].chars().all(|c| self.short.contains(c)),
            }
        })
    }
}

/// The commands the cleanup jobs may run, the read-only ones of
/// `READ_ONLY_TOOLS` with the options that only read. Not `gh`'s `--jq`,
/// whose `env` would show the environment.
const READ_ONLY_COMMANDS: &[ReadOnlyCommand] = &[
    ReadOnlyCommand {
        words: &["ls"],
        short: "aAdFhilLnRrSt1",
        long: &[
            "all",
            "almost-all",
            "directory",
            "human-readable",
            "recursive",
            "full-time",
        ],
    },
    ReadOnlyCommand {
        words: &["stat"],
        short: "cLt",
        long: &["format", "printf", "dereference", "terse"],
    },
    ReadOnlyCommand {
        words: &["du"],
        short: "achsLdx",
        long: &[
            "all",
            "apparent-size",
            "human-readable",
            "max-depth",
            "summarize",
            "total",
        ],
    },
    ReadOnlyCommand {
        words: &["file"],
        short: "bLiz",
        long: &["brief", "mime", "mime-type", "dereference"],
    },
    ReadOnlyCommand {
        words: &["docker", "ps"],
        short: "aqsn",
        long: &["all", "quiet", "size", "last", "filter", "no-trunc"],
    },
    ReadOnlyCommand {
        words: &["docker", "images"],
        short: "aqf",
        long: &["all", "quiet", "filter", "no-trunc", "digests"],
    },
    ReadOnlyCommand {
        words: &["docker", "volume", "ls"],
        short: "qf",
        long: &["quiet", "filter"],
    },
    ReadOnlyCommand {
        words: &["docker", "network", "ls"],
        short: "qf",
        long: &["quiet", "filter", "no-trunc"],
    },
    ReadOnlyCommand {
        words: &["podman", "ps"],
        short: "aqsn",
        long: &["all", "quiet", "size", "last", "filter", "no-trunc"],
    },
    ReadOnlyCommand {
        words: &["podman", "images"],
        short: "aqf",
        long: &["all", "quiet", "filter", "no-trunc", "digests"],
    },
    ReadOnlyCommand {
        words: &["systemctl", "--user", "list-units"],
        short: "at",
        long: &["all", "type", "state", "no-pager", "plain", "no-legend"],
    },
    ReadOnlyCommand {
        words: &["git", "cat-file"],
        short: "tesp",
        long: &[],
    },
    ReadOnlyCommand {
        words: &["git", "branch", "--contains"],
        short: "arv",
        long: &[
            "all",
            "contains",
            "format",
            "list",
            "merged",
            "no-merged",
            "remotes",
            "verbose",
        ],
    },
    ReadOnlyCommand {
        words: &["git", "branch", "--list"],
        short: "arv",
        long: &[
            "all",
            "contains",
            "format",
            "list",
            "merged",
            "no-merged",
            "remotes",
            "verbose",
        ],
    },
    ReadOnlyCommand {
        words: &["gh", "pr", "view"],
        short: "cR",
        long: &["comments", "json", "repo"],
    },
    ReadOnlyCommand {
        words: &["gh", "pr", "list"],
        short: "ABHLlRsS",
        long: &[
            "author", "base", "head", "json", "label", "limit", "repo", "search", "state",
        ],
    },
    ReadOnlyCommand {
        words: &["gh", "repo", "view"],
        short: "",
        long: &["json"],
    },
];

/// The argument that has the server binary act as `bash_hook`.
pub const BASH_HOOK_ARG: &str = "cleanup-bash-hook";

/// Claude Code's PreToolUse hook for the cleanup jobs: reads the hook's
/// input on stdin and returns the exit status, 2 to block a Bash command
/// that `allowed_command` refuses. Claude Code's allowed tools only match
/// a command's prefix, so this keeps out the options that would run
/// commands or write files.
pub fn bash_hook() -> i32 {
    let mut input = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
    match hook_decision(&input) {
        Ok(()) => 0,
        Err(why) => {
            eprintln!("{why}");
            2
        }
    }
}

/// Whether the tool call in a PreToolUse hook's `input` may go ahead;
/// why not if it may not.
fn hook_decision(input: &str) -> Result<(), String> {
    let input: Value =
        serde_json::from_str(input).map_err(|e| format!("the hook's input doesn't parse: {e}"))?;
    if input["tool_name"] != "Bash" {
        return Ok(());
    }
    let command = input["tool_input"]["command"].as_str().unwrap_or_default();
    if allowed_command(command) {
        Ok(())
    } else {
        Err(format!(
            "Denied: only the listed read-only commands, with options that only read, \
             may run, one at a time: {command}"
        ))
    }
}

/// Whether the `gh` command `words` only asks github.com: no URLs, which
/// could name any host, and repos only as `OWNER/NAME`, not
/// `HOST/OWNER/NAME`.
fn stays_on_github(words: &[String]) -> bool {
    let repo = |r: &str| r.matches('/').count() <= 1;
    let mut repo_next = false;
    for (i, w) in words.iter().enumerate() {
        if w.contains("://") {
            return false;
        }
        let is_repo = std::mem::take(&mut repo_next)
            // `gh repo view` takes the repo as its argument.
            || (words.get(1).is_some_and(|c| c == "repo") && i >= 3 && !w.starts_with('-'));
        if is_repo && !repo(w) {
            return false;
        }
        match w.strip_prefix("--repo=") {
            Some(r) if !repo(r) => return false,
            Some(_) => {}
            None => {
                repo_next = w == "--repo"
                    || (!w.starts_with("--") && w.starts_with('-') && w.ends_with('R'))
            }
        }
    }
    true
}

/// Whether `cmd` is one of the read-only commands the cleanup jobs may
/// run, with only options that read. It's split into words the way a
/// shell would; what a shell would expand or run besides, like `$`,
/// substitutions, pipes and globs that could expand to options, is
/// refused, and so is anything that doesn't split.
pub(crate) fn allowed_command(cmd: &str) -> bool {
    if cmd.chars().any(|c| {
        matches!(
            c,
            '|' | '&' | ';' | '<' | '>' | '`' | '$' | '(' | ')' | '\\' | '#' | '\n' | '\r'
        )
    }) {
        return false;
    }
    let Some(words) = shlex::split(cmd) else {
        return false;
    };
    // A glob or brace expansion at the start of a word could expand to
    // file names that are options.
    if words.iter().any(|w| w.starts_with(['*', '?', '[', '{'])) {
        return false;
    }
    if words.first().is_some_and(|w| w == "gh") && !stays_on_github(&words) {
        return false;
    }
    READ_ONLY_COMMANDS.iter().any(|c| c.allows(&words))
}

/// What a check on `backend` for stale items is asked to do.
fn prune_instructions(backend: AgentBackend) -> String {
    let agent = backend.label();
    format!(
        "\
Scans of deleted coding agent sessions reported the cleanup items below as \
things the sessions may have left behind on this machine or on remotes. \
Some of them may be stale: the thing no longer exists, it was cleaned up \
already (for example by the cleanup session an item was handed off to), or \
it isn't something to clean up after all (for example it was inside the \
session's worktree, which is removed, or it's a shared cache, or {agent}'s \
own files).

Find the items that can be deleted from the list. Check with the tools \
whether each thing still exists rather than guessing from its description. \
You can only look; never try to change or remove anything. Keep an item \
when you aren't sure. Report each item to delete with its id and a short \
reason. If none can be deleted, report none.

A pull request that's merged or closed needs no cleaning up, and neither \
does its branch once the branch is gone from the remote. A commit that \
was never pushed needs none once `git cat-file -t` can't find it, or once \
a branch on the remote contains it."
    )
}

/// The scans: whether deleting a session starts one, and the ones running.
pub struct Scans {
    pub(crate) enabled: AtomicBool,
    /// The titles of the deleted sessions being scanned, by session ID.
    running: std::sync::Mutex<BTreeMap<String, String>>,
}

impl Scans {
    pub fn new(enabled: bool) -> Scans {
        Scans {
            enabled: AtomicBool::new(enabled),
            running: std::sync::Mutex::default(),
        }
    }
}

/// The cleanup items, and the sessions still being scanned.
pub fn cleanup_state(state: &State) -> Result<CleanupState> {
    let scanning = state
        .agents
        .scans
        .running
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    Ok(CleanupState {
        items: state.db.cleanup_items()?,
        scanning,
    })
}

/// Starts scanning what the deleted `session` left behind, from its
/// transcript `events`. Doesn't wait for the scan.
pub fn scan(state: &Arc<State>, session: AgentSession, events: Vec<AgentEvent>) {
    let scans = &state.agents.scans;
    if !scans.enabled.load(Ordering::Relaxed) {
        return;
    }
    scans
        .running
        .lock()
        .unwrap()
        .insert(session.id.clone(), session.title.clone());
    state.send(ServerEvent::CleanupChanged);
    let state = state.clone();
    tokio::spawn(async move {
        // A backend that isn't offered may well be again soon, e.g. once
        // it can list its models.
        let mut tries = 1;
        let found = loop {
            match run(&state, &session, &events).await {
                Ok(found) => break found,
                Err(e) if e.is::<NotOffered>() && tries < SCAN_TRIES => {
                    tries += 1;
                    tokio::select! {
                        _ = tokio::time::sleep(SCAN_RETRY) => {}
                        _ = state.shutdown.cancelled() => break failed_scan(&session, &e),
                    }
                }
                Err(e) => break failed_scan(&session, &e),
            }
        };
        if let Err(e) = state
            .db
            .add_cleanup_items(&session, &found, crate::agent::now())
        {
            tracing::warn!("recording cleanup items of {}: {e:#}", session.id);
        }
        state
            .agents
            .scans
            .running
            .lock()
            .unwrap()
            .remove(&session.id);
        state.send(ServerEvent::CleanupChanged);
    });
}

/// The item that says the scan of `session` failed with `e`.
fn failed_scan(session: &AgentSession, e: &anyhow::Error) -> Vec<(String, String)> {
    tracing::warn!("scanning {} for cleanup: {e:#}", session.id);
    let detail = match e.downcast_ref::<NotOffered>() {
        Some(NotOffered(backend)) => format!(
            "{0} couldn't be reached for about {1} minutes, e.g. because it couldn't list \
             its models, so nothing looked for what the session left behind. Check that \
             {0} works, then look for leftovers yourself or hand this item off to a session \
             that does.",
            backend.label(),
            (SCAN_RETRY * (SCAN_TRIES - 1)).as_secs() / 60,
        ),
        None => {
            let mut detail =
                format!("Looking for what the session left behind failed:\n\n```\n{e:#}\n```\n");
            // Devin keeps its transcripts where only Devin reads them.
            if session.backend == AgentBackend::Claude {
                detail += &format!(
                    "\nIts full transcript may still be at `{}`.",
                    transcript_path(session).display()
                );
            }
            detail
        }
    };
    vec![(
        format!("Couldn't scan \u{201c}{}\u{201d}", session.title),
        detail,
    )]
}

/// Where Claude Code keeps the session's full transcript: its config dir's
/// `projects/`, in a directory named after the working directory.
fn transcript_path(session: &AgentSession) -> PathBuf {
    let config = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude")
        });
    let project: String = session
        .worktree
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    config
        .join("projects")
        .join(project)
        .join(format!("{}.jsonl", session.id))
}

/// Asks the session's backend what the session left behind, as `(title,
/// detail)` pairs.
async fn run(
    state: &State,
    session: &AgentSession,
    events: &[AgentEvent],
) -> Result<Vec<(String, String)>> {
    let transcript = transcript_path(session);
    let clone = state.repo(&session.repo).map(|r| r.path);
    let place = if session.repo == GENERAL {
        format!(
            "in the directory {}, outside any repository",
            session.worktree
        )
    } else {
        format!("on {} in the worktree {}", session.repo, session.worktree)
    };
    let mut input = format!(
        "{}\n\n{}\n\n\
         The session \u{201c}{}\u{201d} worked {place}.\n",
        instructions(session.backend),
        tool_notes(&session.repo, clone.as_deref()),
        session.title,
    );
    if session.backend == AgentBackend::Claude && transcript.exists() {
        input += &format!("Its full transcript is {}.\n", transcript.display());
    }
    input += "\n# Transcript digest\n\n";
    input += &digest(events, session.backend.agent_name());

    let schema = json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "detail": {"type": "string"},
                    },
                    "required": ["title", "detail"],
                },
            },
        },
        "required": ["items"],
    });
    let projects = transcript.parent().filter(|_| transcript.exists());
    let output = ask(
        state,
        session.backend,
        &SCAN,
        &schema,
        &input,
        clone.as_deref(),
        projects,
    )
    .await?;
    parse_items(&output)
}

/// How a background job of the cleanup goes.
struct Job {
    kind: JobKind,
    budget: Budget,
    /// How long it may take before it's killed.
    timeout: Duration,
}

const SCAN: Job = Job {
    kind: JobKind::Scan,
    budget: SCAN_BUDGET,
    timeout: SCAN_TIMEOUT,
};

const PRUNE: Job = Job {
    kind: JobKind::Prune,
    budget: PRUNE_BUDGET,
    timeout: PRUNE_TIMEOUT,
};

/// Runs `job` on `backend` on `input` with only the read-only tools, in
/// `clone` if there is one, and returns its output as `schema` has it.
/// `add_dir` is a directory it may read too.
async fn ask(
    state: &State,
    backend: AgentBackend,
    job: &Job,
    schema: &Value,
    input: &str,
    clone: Option<&Path>,
    add_dir: Option<&Path>,
) -> Result<Value> {
    state
        .agents
        .structured(
            backend,
            &JobSpec {
                kind: job.kind,
                schema,
                input,
                tools: JobTools::ReadOnly,
                budget: job.budget,
                add_dir,
                clone,
                timeout: job.timeout,
            },
        )
        .await
}

/// The `(title, detail)` pairs in a scan's output.
fn parse_items(output: &Value) -> Result<Vec<(String, String)>> {
    let items = output["items"]
        .as_array()
        .ok_or_else(|| anyhow!("the result has no items"))?;
    Ok(items
        .iter()
        .filter_map(|i| {
            let title = i["title"].as_str()?.trim();
            (!title.is_empty()).then(|| {
                (
                    title.to_owned(),
                    i["detail"].as_str().unwrap_or_default().trim().to_owned(),
                )
            })
        })
        .collect())
}

/// Checks the cleanup items every `PRUNE_INTERVAL` and deletes the stale
/// ones, until the server shuts down.
pub async fn prune_loop(state: Arc<State>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(PRUNE_INTERVAL) => {}
            _ = state.shutdown.cancelled() => break,
        }
        if let Err(e) = prune(&state).await {
            tracing::warn!("pruning cleanup items: {e:#}");
        }
    }
}

/// Asks which cleanup items are gone or taken care of already, deletes
/// those and returns how many it deleted. Checks the items of each backend
/// with that backend, unless it isn't offered, and those of each repo in
/// its clone; doesn't run a job when there are no items.
pub(crate) async fn prune(state: &Arc<State>) -> Result<usize> {
    let items = state.db.cleanup_items()?;
    let mut by_group: BTreeMap<(AgentBackend, Option<PathBuf>), Vec<&CleanupItem>> =
        BTreeMap::new();
    for item in &items {
        let clone = state.repo(&item.repo).map(|r| r.path);
        by_group
            .entry((item.backend, clone))
            .or_default()
            .push(item);
    }
    let mut deleted = 0;
    let mut failed = None;
    for ((backend, clone), items) in by_group {
        if !state.agents.offered(backend).await {
            tracing::warn!(
                "{} isn't offered, skipping its cleanup items",
                backend.label()
            );
            continue;
        }
        match prune_in(state, backend, clone.as_deref(), &items).await {
            Ok(n) => deleted += n,
            Err(e) => failed = Some(e),
        }
    }
    if deleted > 0 {
        state.send(ServerEvent::CleanupChanged);
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(deleted),
    }
}

/// Asks `backend`, in `clone`, which of `items` are stale, deletes those
/// and returns how many it deleted.
async fn prune_in(
    state: &State,
    backend: AgentBackend,
    clone: Option<&Path>,
    items: &[&CleanupItem],
) -> Result<usize> {
    let now = crate::agent::now();
    let mut input = format!(
        "{}\n\n{}\n\n# Items\n",
        prune_instructions(backend),
        tool_notes(&items[0].repo, clone)
    );
    for item in items {
        input += &format!(
            "\n## Item {}: {}\n\nFrom the session \u{201c}{}\u{201d} in {}, deleted about {} \
             minutes ago.\n",
            item.id,
            item.title,
            item.session_title,
            item.repo,
            (now - item.created_at).max(0) / 60,
        );
        if let Some(cleaner) = &item.handed_off_to {
            input += &format!("It was handed off to the cleanup session {cleaner}.\n");
        }
        input += &format!("\n{}\n", item.detail);
    }
    let schema = json!({
        "type": "object",
        "properties": {
            "stale": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer"},
                        "reason": {"type": "string"},
                    },
                    "required": ["id", "reason"],
                },
            },
        },
        "required": ["stale"],
    });
    let output = ask(state, backend, &PRUNE, &schema, &input, clone, None).await?;
    let mut deleted = 0;
    for (id, reason) in parse_stale(&output, items) {
        if state.db.delete_cleanup_item(id)? {
            let title = &items.iter().find(|i| i.id == id).unwrap().title;
            tracing::info!("deleted the stale cleanup item \u{201c}{title}\u{201d}: {reason}");
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// The `(id, reason)` pairs in a prune's output, of the `items` it was
/// asked about only.
fn parse_stale(output: &Value, items: &[&CleanupItem]) -> Vec<(i64, String)> {
    let stale = output["stale"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    stale
        .iter()
        .filter_map(|s| {
            let id = s["id"].as_i64()?;
            items.iter().any(|i| i.id == id).then(|| {
                (
                    id,
                    s["reason"].as_str().unwrap_or_default().trim().to_owned(),
                )
            })
        })
        .collect()
}

/// `s` cut to about `n` bytes.
pub(crate) fn cut(s: &str, n: usize) -> String {
    let s = s.trim();
    match (n..s.len()).find(|&i| s.is_char_boundary(i)) {
        Some(i) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// A compact transcript: the prompts, Claude's text, and each tool call
/// that could create something with the start of its result. Reads and
/// searches are left out.
fn digest(events: &[AgentEvent], agent: &str) -> String {
    let results: HashMap<&str, (&str, bool)> = events
        .iter()
        .filter_map(|e| match &e.kind {
            AgentEventKind::ToolResult {
                id,
                content,
                is_error,
                ..
            } => Some((id.as_str(), (content.as_str(), *is_error))),
            _ => None,
        })
        .collect();
    let mut out = String::new();
    for event in events {
        match &event.kind {
            AgentEventKind::Prompt(p) => {
                out += &format!("\n## Prompt\n\n{}\n\n", cut(p, PROMPT_LIMIT));
            }
            AgentEventKind::Text(t) => out += &format!("{agent}: {}\n", cut(t, TEXT_LIMIT)),
            AgentEventKind::ToolUse {
                id,
                name,
                input,
                parent,
            } => {
                let input: Value = serde_json::from_str(input).unwrap_or(Value::Null);
                let sub = if parent.is_some() { "(subagent) " } else { "" };
                let call = match name.as_str() {
                    "Read" | "Glob" | "Grep" | "LS" | "TodoWrite" | "find_file_by_name" => continue,
                    "Bash" => format!("$ {}", input["command"].as_str().unwrap_or_default()),
                    "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => format!(
                        "{name} {}",
                        input["file_path"]
                            .as_str()
                            .or(input["notebook_path"].as_str())
                            .unwrap_or_default()
                    ),
                    _ => format!("{name} {}", cut(&input.to_string(), TEXT_LIMIT)),
                };
                out += &format!("{sub}{call}");
                out += "\n";
                if let Some((content, is_error)) = results.get(id.as_str()) {
                    let content = cut(content, RESULT_LIMIT);
                    if *is_error {
                        out += &format!("  error: {content}\n");
                    } else if !content.is_empty() {
                        out += &format!("  → {content}\n");
                    }
                }
            }
            AgentEventKind::Error(e) => out += &format!("Error: {}\n", cut(e, TEXT_LIMIT)),
            AgentEventKind::Landed(landed) => out += &format!("Landed: {landed:?}\n"),
            _ => {}
        }
    }
    if out.len() <= DIGEST_LIMIT {
        return out;
    }
    let half = DIGEST_LIMIT / 2;
    let head = (0..=half)
        .rev()
        .find(|&i| out.is_char_boundary(i))
        .unwrap_or(0);
    let tail = (out.len() - half..out.len())
        .find(|&i| out.is_char_boundary(i))
        .unwrap_or(out.len());
    format!(
        "{}\n\n[… {} bytes left out; read the full transcript for them …]\n\n{}",
        &out[..head],
        tail - head,
        &out[tail..]
    )
}

/// The prompt that hands `items` off to a cleanup session.
fn hand_off_prompt(items: &[CleanupItem]) -> String {
    let mut prompt = String::from(
        "Clean up what deleted agent sessions left behind\n\n\
         A scan of the sessions' transcripts found the items below. Check that each one \
         still exists and is safe to remove, e.g. that nothing else uses it, before you \
         remove it, and leave out anything that isn't. Where this session runs is only a \
         place to work from; the items are elsewhere on this machine or on remotes.\n",
    );
    for item in items {
        prompt += &format!(
            "\n## {}\n\nFrom the session \u{201c}{}\u{201d} in {}.\n\n{}\n",
            item.title, item.session_title, item.repo, item.detail
        );
    }
    prompt
}

/// Starts a session, in plan mode, that cleans up the items `req` picks,
/// and marks them handed off to it. `None` when none of them exist.
pub async fn hand_off(state: &Arc<State>, req: NewCleanupSession) -> Result<Option<AgentSession>> {
    let items: Vec<_> = state
        .db
        .cleanup_items()?
        .into_iter()
        .filter(|i| req.ids.contains(&i.id))
        .collect();
    if items.is_empty() {
        return Ok(None);
    }
    let session = crate::agent::create(
        state,
        NewAgent {
            backend: req.backend,
            repo: req.repo,
            pr: None,
            prompt: hand_off_prompt(&items),
            permission_mode: PermissionMode::plan(),
            model: req.model,
            images: Vec::new(),
        },
    )
    .await?;
    let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
    state.db.set_cleanup_handed_off(&ids, &session.id)?;
    state.send(ServerEvent::CleanupChanged);
    Ok(Some(session))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: AgentEventKind) -> AgentEvent {
        AgentEvent {
            seq: Some(1),
            at: None,
            kind,
        }
    }

    fn tool(id: &str, name: &str, input: Value) -> AgentEvent {
        event(AgentEventKind::ToolUse {
            id: id.into(),
            name: name.into(),
            input: input.to_string(),
            parent: None,
        })
    }

    fn result(id: &str, content: &str, is_error: bool) -> AgentEvent {
        event(AgentEventKind::ToolResult {
            id: id.into(),
            content: content.into(),
            is_error,
            patch: Vec::new(),
            images: Vec::new(),
        })
    }

    #[test]
    fn digests_the_calls_that_create_things() {
        let events = [
            event(AgentEventKind::Prompt("Run the e2e tests".into())),
            event(AgentEventKind::Text("Starting a container.".into())),
            tool(
                "1",
                "Bash",
                json!({"command": "docker run -d --name e2e pg"}),
            ),
            result("1", "abc123\n", false),
            tool("2", "Read", json!({"file_path": "/etc/hosts"})),
            result("2", "127.0.0.1 localhost", false),
            tool(
                "3",
                "Write",
                json!({"file_path": "/tmp/notes.md", "content": "x"}),
            ),
            result("3", "", false),
            tool("4", "Bash", json!({"command": "rm /nope"})),
            result("4", "No such file", true),
            tool("5", "WebFetch", json!({"url": "https://example.com"})),
        ];
        assert_eq!(
            digest(&events, "Claude"),
            "\n## Prompt\n\nRun the e2e tests\n\n\
             Claude: Starting a container.\n\
             $ docker run -d --name e2e pg\n  → abc123\n\
             Write /tmp/notes.md\n\
             $ rm /nope\n  error: No such file\n\
             WebFetch {\"url\":\"https://example.com\"}\n"
        );
    }

    #[test]
    fn digest_keeps_the_beginning_and_end() {
        let events: Vec<_> = (0..20_000)
            .map(|i| tool("x", "Bash", json!({"command": format!("touch /tmp/f{i}")})))
            .collect();
        let d = digest(&events, "Claude");
        assert!(d.len() < DIGEST_LIMIT + 200, "{}", d.len());
        assert!(d.starts_with("$ touch /tmp/f0\n"));
        assert!(d.ends_with("$ touch /tmp/f19999\n"));
        assert!(d.contains("bytes left out"));
    }

    #[test]
    fn parses_results() {
        let ok = json!({
            "is_error": false,
            "structured_output": {"items": [
                {"title": " Container e2e ", "detail": "`docker rm -f e2e`"},
                {"title": "", "detail": "dropped"},
            ]},
        });
        let output = crate::claude::structured_output("scan", ok.to_string().as_bytes()).unwrap();
        assert_eq!(
            parse_items(&output).unwrap(),
            [("Container e2e".to_owned(), "`docker rm -f e2e`".to_owned())]
        );
        assert!(parse_items(&json!({})).is_err());
    }

    #[test]
    fn parses_stale_items() {
        let item = |id| CleanupItem {
            id,
            repo: "o/n".into(),
            session_id: "s".into(),
            session_title: "t".into(),
            title: "x".into(),
            detail: "d".into(),
            created_at: 0,
            backend: AgentBackend::Claude,
            handed_off_to: None,
        };
        let output = json!({"stale": [
            {"id": 1, "reason": " gone "},
            {"id": 9, "reason": "made up"},
            {"reason": "no id"},
            {"id": 2},
        ]});
        assert_eq!(
            parse_stale(&output, &[&item(1), &item(2), &item(3)]),
            [(1, "gone".to_owned()), (2, String::new())]
        );
        assert_eq!(parse_stale(&json!({}), &[&item(1)]), []);
    }

    #[test]
    fn transcript_path_follows_claude_code() {
        let session = AgentSession {
            worktree: "/home/u/.local/share/w/o/n/id".into(),
            ..AgentSession::fixture("id", "o/n")
        };
        assert!(
            transcript_path(&session).ends_with("projects/-home-u--local-share-w-o-n-id/id.jsonl")
        );
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;

    /// The hook lets through the commands `allowed_command` allows, and
    /// other tools.
    #[test]
    fn the_hook_checks_bash_commands() {
        let call = |tool: &str, command: &str| {
            json!({"tool_name": tool, "tool_input": {"command": command}}).to_string()
        };
        assert_eq!(hook_decision(&call("Bash", "ls -la")), Ok(()));
        assert_eq!(hook_decision(&call("Read", "")), Ok(()));
        for cmd in [
            "git ls-remote --upload-pack='touch ~/pwned' .",
            "git log --output=/home/me/.bashrc -1",
            "git branch --list -D x",
        ] {
            assert!(hook_decision(&call("Bash", cmd)).is_err(), "{cmd}");
        }
        assert!(hook_decision("not json").is_err());
    }

    /// The read-only commands the allowlist lets through, and what it
    /// keeps out: other commands, shell syntax, and options that run
    /// commands, write files or change things.
    #[test]
    fn allows_read_only_commands() {
        for cmd in [
            "ls -la /tmp",
            "ls",
            "stat /tmp/x",
            "du -sh /tmp/grenadine-e2e",
            "file /tmp/x",
            "docker ps -a",
            "docker volume ls --filter name=e2e",
            "systemctl --user list-units --all",
            "git cat-file -t abc123",
            "git branch --contains abc123 -r",
            "git branch --list 'dev/*'",
            "gh pr view 5 --json state",
            "gh pr view 5 --repo o/n --json state,headRefName",
            "gh pr list --head dev/x --state all",
            "gh pr list --head dev/a/b -R o/n",
            "gh repo view o/n --json name",
        ] {
            assert!(allowed_command(cmd), "{cmd}");
        }
        for cmd in [
            // Not on the list.
            "rm -rf /tmp/x",
            "lsblk",
            "gh",
            "git branch -D x",
            "git push origin :x",
            "git ls-remote origin",
            "git log -1",
            // Shell syntax.
            "ls; rm x",
            "ls $(pwd)",
            "ls `id`",
            "ls $HOME",
            "gh pr view 1 && rm x",
            "ls > /tmp/x",
            "ls *",
            "ls {--output=x,}",
            "ls \"unterminated",
            // Options that run commands or write files.
            "git ls-remote --upload-pack=\"touch ~/pwned\" .",
            "git ls-remote --upload-pack \"touch ~/pwned\" .",
            "git ls-remote -u \"touch ~/pwned\" .",
            "git ls-remote --receive-pack=x .",
            "git ls-remote --exec=x .",
            "git log --output=/home/me/.bashrc -1",
            "git log --output /home/me/.bashrc -1",
            "git log -1 --output=/tmp/x",
            "git log -p --ext-diff",
            "git log -p --textconv",
            "git log -p",
            "git cat-file --textconv HEAD:x",
            "git cat-file --filters HEAD:x",
            "git -c core.pager=sh log",
            "git -c core.sshCommand=x log",
            "git --config-env=core.pager=X log",
            "git log --config=x",
            "git -C /elsewhere log",
            "git branch --list -D x",
            "git branch --contains x --set-upstream-to=y",
            "git branch --list -m x y",
            "docker -H tcp://x ps",
            "docker ps --host x",
            "gh pr view 5 --web",
            // gh asking other hosts, or showing the environment.
            "gh pr view https://evil.example/o/n/pull/1",
            "gh pr view 5 -R evil.example/o/n",
            "gh pr view 5 -cR evil.example/o/n",
            "gh pr view 5 --repo evil.example/o/n",
            "gh pr view 5 --repo=evil.example/o/n",
            "gh pr list --repo evil.example/o/n",
            "gh repo view evil.example/o/n",
            "gh repo view https://evil.example/o/n",
            "gh pr view 5 --json state --jq env",
            "gh pr view 5 --json state -q env",
            "gh pr list --config x",
            "file -C -m x",
            "du --files0-from=/etc/shadow",
            "ls --output=x",
        ] {
            assert!(!allowed_command(cmd), "{cmd}");
        }
    }
}
