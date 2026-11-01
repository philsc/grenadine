//! The Claude Code backend. Every prompt runs one `claude -p` process
//! that resumes the session and reads the prompt as a stream-json
//! message, and its stream-json output becomes the session's transcript.
//! Background jobs like summaries and cleanup scans run `claude -p` with
//! `--output-format json --json-schema` instead.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use grenadine_core::api::{
    AgentBackendInfo, AgentEventKind, AgentModelInfo, AgentSession, AgentStatus, PatchHunk,
    PermissionMode,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::OnceCell;

use crate::agent::TurnSink;
use crate::sync::State;

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

/// The grenadine MCP server's tools that Claude may use without asking.
const ALLOWED_TOOLS: [&str; 4] = [
    "mcp__grenadine__list_terminals",
    "mcp__grenadine__read_terminal",
    "mcp__grenadine__wait_for_terminal",
    "mcp__grenadine__show_image",
];

/// The `initialize` control request the Agent SDK starts with. Claude Code
/// answers it without calling a model.
const INITIALIZE: &str =
    r#"{"type":"control_request","request_id":"models","request":{"subtype":"initialize"}}"#;

/// The Claude Code backend: the `claude` executable and what it offers.
pub struct Claude {
    /// The `claude` executable.
    exe: PathBuf,
    /// The models Claude Code offers, once it has listed them.
    models: OnceCell<Vec<AgentModelInfo>>,
}

impl Claude {
    pub fn new(exe: PathBuf) -> Claude {
        Claude {
            exe,
            models: OnceCell::new(),
        }
    }

    /// The `claude` executable.
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// The models Claude Code offers, listed once and kept; none while it
    /// can't list them.
    pub async fn models(&self) -> Vec<AgentModelInfo> {
        match self.models.get_or_try_init(|| list_models(&self.exe)).await {
            Ok(models) => models.clone(),
            Err(e) => {
                tracing::warn!("listing Claude Code's models: {e:#}");
                Vec::new()
            }
        }
    }

    /// What Claude Code offers sessions, for the backends endpoint.
    pub async fn info(&self) -> AgentBackendInfo {
        grenadine_core::api::AgentBackend::Claude.info(self.models().await)
    }
}

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

/// The stream-json user message that gives Claude `prompt` with the images
/// in `dir` named `images`. The text goes last: Claude Code only takes a
/// prompt for a slash command when its last block is the command.
async fn user_message(prompt: &str, dir: &Path, images: &[String]) -> Result<String> {
    let mut content = Vec::new();
    for name in images {
        let bytes = tokio::fs::read(dir.join(name))
            .await
            .with_context(|| format!("can't read the image {name}"))?;
        content.push(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": crate::agent::media_type(name).unwrap_or("image/png"),
                "data": base64::engine::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    bytes,
                ),
            },
        }));
    }
    if !prompt.trim().is_empty() {
        content.push(json!({"type": "text", "text": prompt}));
    }
    let message = json!({
        "type": "user",
        "message": {"role": "user", "content": content},
        "parent_tool_use_id": null,
    });
    Ok(format!("{message}\n"))
}

/// One prompt's turn: what it says and how it runs.
pub struct Turn<'a> {
    pub prompt: &'a str,
    pub images: &'a [String],
    pub mode: &'a PermissionMode,
    pub model: Option<String>,
    /// The session's grenadine MCP endpoint.
    pub mcp_url: &'a str,
    /// Appended to Claude's system prompt.
    pub notes: &'a [String],
}

/// Runs `t` as a `claude -p` turn of `session` and returns the status the
/// session ends up in; `sink` gets what the turn's output means for the
/// transcript.
pub async fn turn(
    state: &State,
    session: &AgentSession,
    t: Turn<'_>,
    mut sink: TurnSink<'_>,
) -> Result<AgentStatus> {
    let Turn {
        prompt,
        images,
        mode,
        model,
        mcp_url,
        notes,
    } = t;
    let id = &session.id;
    let mcp = json!({
        "mcpServers": {
            "grenadine": {
                "type": "http",
                "url": mcp_url,
            }
        }
    });
    // `systemd-run --scope` execs claude, so the PID stays claude's.
    let mut cmd = if let Some(slice) = state.agents.slice_of(id) {
        let mut cmd = tokio::process::Command::new("systemd-run");
        cmd.args(["--user", "--scope", "--quiet", "--collect"])
            .arg(format!("--slice={slice}"))
            .arg("--")
            .arg(state.agents.claude());
        cmd
    } else {
        tokio::process::Command::new(state.agents.claude())
    };
    cmd.arg("-p");
    // Claude's session ID is grenadine's.
    if let Some(backend_session) = state.db.backend_session(id)? {
        cmd.args(["--resume", &backend_session]);
    } else {
        cmd.args(["--session-id", id]);
    }
    cmd.args([
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-mode",
        mode.as_str(),
        "--permission-prompt-tool",
        "mcp__grenadine__approve",
    ])
    .arg("--allowedTools")
    .args(ALLOWED_TOOLS)
    .arg("--mcp-config")
    .arg(mcp.to_string())
    .args(
        (!notes.is_empty())
            .then(|| notes.join("\n\n"))
            .into_iter()
            .flat_map(|note| ["--append-system-prompt".to_owned(), note]),
    )
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
    let input = user_message(prompt, &state.agents.images().join(id), images).await?;
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
        .with_context(|| format!("can't run {}", state.agents.claude().display()))?;
    if let Some(pid) = child.id() {
        // An interrupt that came before the process existed applies now.
        if state.agents.set_pid(id, pid) {
            crate::agent::interrupt_pid(pid);
        }
    }
    // The prompt goes through stdin so that one starting with `-` isn't
    // taken for a flag. If claude exits without reading it, its exit status
    // and stderr below say why.
    let mut stdin = child.stdin.take().unwrap();
    if let Err(e) = stdin.write_all(input.as_bytes()).await
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
    let mut stopping = false;
    loop {
        let line = tokio::select! {
            line = lines.next_line() => line?,
            _ = state.shutdown.cancelled(), if !stopping => {
                stopping = true;
                if let Some(pid) = child.id() {
                    crate::agent::interrupt_pid(pid);
                }
                continue;
            }
        };
        let Some(line) = line else { break };
        match parse_line(&line) {
            Parsed::Started { model } => sink.started(id, model)?,
            Parsed::Events(events) => {
                for kind in events {
                    sink.event(kind)?;
                }
            }
        }
    }
    let exit = child.wait().await?;
    let stderr = stderr_task.await.unwrap_or_default();
    sink.finish(state.agents.interrupted(id), || {
        let mut message = format!("claude exited without finishing the turn ({exit})");
        if !stderr.is_empty() {
            message += &format!(":\n{stderr}");
        }
        message
    })
}

/// What one line of `--output-format stream-json` means for the transcript.
#[derive(Debug, PartialEq)]
enum Parsed {
    /// Claude Code created (or resumed) its session with `model`.
    Started {
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

/// The images in a tool result as `data:` URLs, which `store_images`
/// replaces with ids before the result is stored.
fn result_images(content: &Value) -> Vec<String> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "image" && b["source"]["type"] == "base64")
        .filter_map(|b| {
            let source = &b["source"];
            Some(format!(
                "data:{};base64,{}",
                source["media_type"].as_str()?,
                source["data"].as_str()?
            ))
        })
        .collect()
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
                images: result_images(&b["content"]),
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
                    cost: v["total_cost_usd"].as_f64(),
                }])
                .collect()
        }
        _ => Vec::new(),
    };
    Parsed::Events(events)
}

/// A `claude -p` run that answers in a JSON schema's shape, for the
/// background jobs like summaries and cleanup scans.
pub struct StructuredJob<'a> {
    /// What it's called in errors.
    pub name: &'a str,
    pub model: &'a str,
    pub schema: &'a Value,
    pub input: &'a str,
    /// The `--tools` value; `None` passes no `--tools`.
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

/// Runs `claude -p --output-format json --json-schema` for `job` on its
/// input and returns its `structured_output`.
pub async fn structured(claude: &Path, job: &StructuredJob<'_>) -> Result<Value> {
    let mut cmd = tokio::process::Command::new(claude);
    cmd.args(["-p", "--model", job.model, "--output-format", "json"])
        .arg("--json-schema")
        .arg(job.schema.to_string())
        .args(["--no-session-persistence", "--strict-mcp-config"]);
    // Not the project settings of a clone it runs in, which could allow
    // more tools or add hooks.
    if job.setting_sources_user {
        cmd.args(["--setting-sources", "user"]);
    }
    if let Some(tools) = job.tools {
        cmd.arg("--tools").arg(tools);
    }
    cmd.args([
        "--permission-mode",
        "dontAsk",
        "--max-budget-usd",
        job.budget_usd,
    ]);
    if let Some(add_dir) = job.add_dir {
        cmd.arg("--add-dir").arg(add_dir);
    }
    // Last, since it takes every value up to the next flag.
    if !job.allowed.is_empty() {
        cmd.arg("--allowedTools").args(job.allowed);
    }
    cmd.current_dir(job.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("can't run {}", claude.display()))?;
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(job.input.as_bytes()).await?;
    drop(stdin);
    let out = tokio::time::timeout(job.timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow!(
                "the {} took longer than {}s",
                job.name,
                job.timeout.as_secs()
            )
        })??;
    structured_output(job.name, &out.stdout).with_context(|| {
        let stderr = String::from_utf8_lossy(&out.stderr);
        format!("claude exited with {}: {}", out.status, stderr.trim())
    })
}

/// The structured output in `claude -p --output-format json --json-schema`
/// output.
pub fn structured_output(name: &str, stdout: &[u8]) -> Result<Value> {
    let mut result: Value = serde_json::from_slice(stdout).context("no result")?;
    if result["is_error"].as_bool() != Some(false) {
        let why = result["result"]
            .as_str()
            .or(result["subtype"].as_str())
            .unwrap_or("unknown error");
        bail!("the {name} failed: {why}");
    }
    Ok(result["structured_output"].take())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use grenadine_core::api::AgentBackend;

    use super::*;

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
                model: Some("claude-opus-4-6".into())
            }
        );
        assert_eq!(
            parse_line(
                r#"{"type":"system","subtype":"init","session_id":"s","model":"claude-opus-4-6[1m]"}"#
            ),
            Parsed::Started {
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
                images: vec![],
            }]
        );
        assert_eq!(
            events(
                r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_2","type":"tool_result","content":[{"type":"text","text":"a"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBO"}}]}]},"parent_tool_use_id":null}"#
            ),
            [AgentEventKind::ToolResult {
                id: "toolu_2".into(),
                content: "a\n[image]".into(),
                is_error: false,
                patch: vec![],
                images: vec!["data:image/png;base64,iVBO".into()],
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
                images: vec![],
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
                    cost: Some(0.5)
                },
            ]
        );
        assert_eq!(
            events(r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#),
            [AgentEventKind::TurnEnded {
                error: Some("error_during_execution".into()),
                cost: None
            }]
        );
        assert_eq!(events("not json"), []);
    }

    /// A turn's end written before its cost was called `cost` still loads.
    #[test]
    fn reads_old_turn_ended_costs() {
        let event: AgentEventKind =
            serde_json::from_str(r#"{"turn_ended":{"error":null,"cost_usd":0.5}}"#).unwrap();
        assert_eq!(
            event,
            AgentEventKind::TurnEnded {
                error: None,
                cost: Some(0.5),
            }
        );
    }

    /// A state whose agents run `script` as claude, with the git fixture's
    /// clone configured.
    fn state_with_claude(fx: &crate::git::tests::Fixture, dir: &Path, script: &str) -> Arc<State> {
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

    /// Claude Code lists its models in its answer to `initialize`, as
    /// 2.1 writes it, shortened. The backend it shows offers its modes
    /// and the listed models.
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
        let models = [
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
        ];
        let backends = state.agents.backends().await;
        assert_eq!(backends.len(), 1);
        let info = &backends[0];
        assert_eq!(info.backend, AgentBackend::Claude);
        assert_eq!(info.label, "Claude Code");
        let modes: Vec<_> = info
            .modes
            .iter()
            .map(|m| (m.value.as_str(), m.label.as_str()))
            .collect();
        assert_eq!(
            modes,
            [
                ("manual", "Ask for everything"),
                ("acceptEdits", "Accept edits"),
                ("auto", "Auto"),
                ("plan", "Plan first"),
            ]
        );
        assert_eq!(info.default_mode, "auto");
        assert_eq!(info.after_plan, ["manual", "acceptEdits", "auto"]);
        assert_eq!(info.models, models);
        assert!(state.agents.offers(AgentBackend::Claude, None).await);
        assert!(
            state
                .agents
                .offers(AgentBackend::Claude, Some("sonnet"))
                .await
        );
        assert!(
            !state
                .agents
                .offers(AgentBackend::Claude, Some("--help"))
                .await
        );
        assert!(state.agents.has_mode(AgentBackend::Claude, &"auto".into()));
        assert!(!state.agents.has_mode(AgentBackend::Claude, &"nope".into()));

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

    /// Without a list, prompts can only go with Claude Code's default,
    /// and the models are empty.
    #[tokio::test]
    async fn offers_only_the_default_without_models() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_claude(&fx, dir.path(), "#!/bin/sh\nexit 1\n");
        let backends = state.agents.backends().await;
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].models, []);
        assert!(state.agents.offers(AgentBackend::Claude, None).await);
        assert!(
            !state
                .agents
                .offers(AgentBackend::Claude, Some("sonnet"))
                .await
        );
    }

    #[test]
    fn parses_structured_output() {
        let ok = json!({
            "is_error": false,
            "structured_output": {"items": []},
        });
        assert_eq!(
            structured_output("scan", ok.to_string().as_bytes()).unwrap(),
            json!({"items": []})
        );
        let failed = json!({"is_error": true, "subtype": "error_max_budget_usd"});
        let e = structured_output("scan", failed.to_string().as_bytes()).unwrap_err();
        assert!(format!("{e:#}").contains("error_max_budget_usd"), "{e:#}");
        assert!(structured_output("scan", b"").is_err());
    }
}
