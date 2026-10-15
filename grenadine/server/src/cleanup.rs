//! What deleted agent sessions left behind. Deleting a session stops its
//! processes and removes its worktree and Bazel output bases, but a
//! session can also leave containers, files elsewhere, pushed branches
//! and the like. A Haiku run reads the deleted session's transcript in the
//! background and reports those as cleanup items, which the user can
//! delete or hand off to a new session that cleans them up.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use grenadine_core::api::{
    AgentEvent, AgentEventKind, AgentSession, CleanupState, NewAgent, NewCleanupSession,
    PermissionMode, ServerEvent,
};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::sync::State;

/// How long a scan may take before it's killed.
const SCAN_TIMEOUT: Duration = Duration::from_secs(600);

/// What a scan may spend, in USD.
const SCAN_BUDGET_USD: &str = "0.50";

/// About how much of the transcript a scan gets, in bytes. Its beginning
/// and end are kept, and the full transcript is there to read.
const DIGEST_LIMIT: usize = 200_000;

/// How much of each prompt, text and tool call the digest keeps.
const PROMPT_LIMIT: usize = 2_000;
const TEXT_LIMIT: usize = 300;
const RESULT_LIMIT: usize = 300;

/// Read-only tools, so that a scan can only look.
const ALLOWED_TOOLS: &[&str] = &[
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
    "Bash(git ls-remote:*)",
];

const INSTRUCTIONS: &str = "\
A coding agent session was just deleted. Its git worktree has been removed, \
its processes stopped, and the Bazel output bases of the workspaces in the \
worktree deleted. Find anything else the session created that may still \
exist and that the user might want to clean up. For example: docker or \
podman containers, images, volumes and networks; files and directories \
outside the worktree (in /tmp, the home directory, caches); other git \
worktrees or clones; branches or tags pushed to remotes; installed packages \
or tools; systemd units, cron jobs or background services.

Work from the transcript digest below. Use tools only to read the full \
transcript, when the digest is cut short or leaves out what subagents did, \
or to check whether something still exists. You can only look; never try \
to change or remove anything.

Leave out things inside the worktree, Bazel's shared caches, Claude Code's \
own files, and things the session removed again itself. Report each finding \
with a one-line title and a markdown detail that names exactly what it is \
and where, quotes the evidence from the transcript, and says how to remove \
it. If there's nothing, report no items.";

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
        let found = match run(&state, &session, &events).await {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!("scanning {} for cleanup: {e:#}", session.id);
                vec![(
                    format!("Couldn't scan \u{201c}{}\u{201d}", session.title),
                    format!(
                        "Looking for what the session left behind failed:\n\n```\n{e:#}\n```\n\n\
                         Its full transcript may still be at `{}`.",
                        transcript_path(&session).display()
                    ),
                )]
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

/// Asks Haiku what the session left behind, as `(title, detail)` pairs.
async fn run(
    state: &State,
    session: &AgentSession,
    events: &[AgentEvent],
) -> Result<Vec<(String, String)>> {
    let transcript = transcript_path(session);
    let mut input = format!(
        "{INSTRUCTIONS}\n\nThe session \u{201c}{}\u{201d} worked on {} in the worktree {}.\n",
        session.title, session.repo, session.worktree
    );
    if transcript.exists() {
        input += &format!("Its full transcript is {}.\n", transcript.display());
    }
    input += "\n# Transcript digest\n\n";
    input += &digest(events);

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
    // The worktree is gone, so the scan runs in a directory of its own.
    let dir = scan_dir(&session.id)?;
    let mut cmd = tokio::process::Command::new(state.agents.claude());
    cmd.args(["-p", "--model", "haiku", "--output-format", "json"])
        .arg("--json-schema")
        .arg(schema.to_string())
        .args([
            "--no-session-persistence",
            "--strict-mcp-config",
            "--tools",
            "Read,Glob,Grep,Bash",
            "--permission-mode",
            "dontAsk",
            "--max-budget-usd",
            SCAN_BUDGET_USD,
        ]);
    if let Some(projects) = transcript.parent().filter(|_| transcript.exists()) {
        cmd.arg("--add-dir").arg(projects);
    }
    // Last, since it takes every value up to the next flag.
    cmd.arg("--allowedTools")
        .args(ALLOWED_TOOLS)
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let result = async {
        let mut child = cmd
            .spawn()
            .with_context(|| format!("can't run {}", state.agents.claude().display()))?;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).await?;
        drop(stdin);
        let out = tokio::time::timeout(SCAN_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| anyhow!("the scan took longer than {}s", SCAN_TIMEOUT.as_secs()))??;
        parse_result(&out.stdout).with_context(|| {
            let stderr = String::from_utf8_lossy(&out.stderr);
            format!("claude exited with {}: {}", out.status, stderr.trim())
        })
    }
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn scan_dir(id: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("grenadine-cleanup-{id}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("can't create {}", dir.display()))?;
    Ok(dir)
}

/// The items in `claude -p --output-format json --json-schema` output.
fn parse_result(stdout: &[u8]) -> Result<Vec<(String, String)>> {
    let result: Value = serde_json::from_slice(stdout).context("no result")?;
    if result["is_error"].as_bool() != Some(false) {
        let why = result["result"]
            .as_str()
            .or(result["subtype"].as_str())
            .unwrap_or("unknown error");
        bail!("the scan failed: {why}");
    }
    let items = result["structured_output"]["items"]
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

/// `s` cut to about `n` bytes.
fn cut(s: &str, n: usize) -> String {
    let s = s.trim();
    match (n..s.len()).find(|&i| s.is_char_boundary(i)) {
        Some(i) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// A compact transcript: the prompts, Claude's text, and each tool call
/// that could create something with the start of its result. Reads and
/// searches are left out.
fn digest(events: &[AgentEvent]) -> String {
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
            AgentEventKind::Text(t) => out += &format!("Claude: {}\n", cut(t, TEXT_LIMIT)),
            AgentEventKind::ToolUse { id, name, input } => {
                let input: Value = serde_json::from_str(input).unwrap_or(Value::Null);
                let call = match name.as_str() {
                    "Read" | "Glob" | "Grep" | "LS" | "TodoWrite" => continue,
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
                out += &call;
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
fn hand_off_prompt(items: &[grenadine_core::api::CleanupItem]) -> String {
    let mut prompt = String::from(
        "Clean up what deleted agent sessions left behind\n\n\
         A scan of the sessions' transcripts found the items below. Check that each one \
         still exists and is safe to remove, e.g. that nothing else uses it, before you \
         remove it, and leave out anything that isn't. This worktree is only a place to \
         work from; the items are elsewhere on this machine or on remotes.\n",
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
            repo: req.repo,
            pr: None,
            prompt: hand_off_prompt(&items),
            permission_mode: PermissionMode::Plan,
            model: req.model,
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
        AgentEvent { seq: Some(1), kind }
    }

    fn tool(id: &str, name: &str, input: Value) -> AgentEvent {
        event(AgentEventKind::ToolUse {
            id: id.into(),
            name: name.into(),
            input: input.to_string(),
        })
    }

    fn result(id: &str, content: &str, is_error: bool) -> AgentEvent {
        event(AgentEventKind::ToolResult {
            id: id.into(),
            content: content.into(),
            is_error,
            patch: Vec::new(),
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
            digest(&events),
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
        let d = digest(&events);
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
        assert_eq!(
            parse_result(ok.to_string().as_bytes()).unwrap(),
            [("Container e2e".to_owned(), "`docker rm -f e2e`".to_owned())]
        );
        let failed = json!({"is_error": true, "subtype": "error_max_budget_usd"});
        let e = parse_result(failed.to_string().as_bytes()).unwrap_err();
        assert!(format!("{e:#}").contains("error_max_budget_usd"), "{e:#}");
        assert!(parse_result(b"").is_err());
    }

    #[test]
    fn transcript_path_follows_claude_code() {
        let session = AgentSession {
            id: "id".into(),
            repo: "o/n".into(),
            pr: None,
            title: "t".into(),
            branch: "b".into(),
            worktree: "/home/u/.local/share/w/o/n/id".into(),
            base_sha: "s".into(),
            status: grenadine_core::api::AgentStatus::Idle,
            created_at: 0,
            cost_usd: 0.0,
            context_tokens: None,
            context_window: None,
            model: None,
            landed: None,
        };
        assert!(
            transcript_path(&session).ends_with("projects/-home-u--local-share-w-o-n-id/id.jsonl")
        );
    }
}
