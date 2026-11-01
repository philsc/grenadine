//! The Claude Code backend. Every prompt runs one `claude -p` process
//! that resumes the session and reads the prompt as a stream-json
//! message, and its stream-json output becomes the session's transcript.
//! Background jobs like summaries and cleanup scans run `claude -p` with
//! `--output-format json --json-schema` instead.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use grenadine_core::api::{AgentEventKind, AgentModelInfo, AgentSession, AgentStatus, PatchHunk};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::agent::{
    Backend, JobKind, JobSpec, JobTools, ModelList, OutputLines, Stopper, Turn, TurnSink,
};
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

/// The `initialize` control request the Agent SDK starts with. Claude Code
/// answers it without calling a model.
const INITIALIZE: &str =
    r#"{"type":"control_request","request_id":"models","request":{"subtype":"initialize"}}"#;

/// The Claude Code backend: the `claude` executable and what it offers.
pub struct Claude {
    /// The `claude` executable.
    exe: PathBuf,
    /// The models Claude Code offers.
    models: ModelList,
}

impl Claude {
    pub fn new(exe: PathBuf) -> Claude {
        Claude {
            exe,
            models: ModelList::default(),
        }
    }
}

impl Backend for Claude {
    fn models(&self) -> BoxFuture<'_, Result<Vec<AgentModelInfo>>> {
        self.models
            .get("Claude Code", || list_models(&self.exe))
            .boxed()
    }

    fn turn<'a>(
        &'a self,
        state: &'a std::sync::Arc<State>,
        session: &'a AgentSession,
        t: Turn<'a>,
        sink: TurnSink<'a>,
    ) -> BoxFuture<'a, Result<AgentStatus>> {
        turn(&self.exe, state, session, t, sink).boxed()
    }

    fn structured<'a>(
        &'a self,
        job: &'a JobSpec<'a>,
        cwd: &'a Path,
    ) -> BoxFuture<'a, Result<Value>> {
        structured(&self.exe, job, cwd).boxed()
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

/// Runs `t` as a `claude -p` turn of `session` and returns the status the
/// session ends up in; `sink` gets what the turn's output means for the
/// transcript.
async fn turn(
    claude: &Path,
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
    let mut cmd = crate::agent::backend_command(state, session, claude);
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
    .args(crate::mcp::ALLOWED_TOOLS)
    .arg("--mcp-config")
    .arg(mcp.to_string())
    .args(
        (!notes.is_empty())
            .then(|| notes.join("\n\n"))
            .into_iter()
            .flat_map(|note| ["--append-system-prompt".to_owned(), note]),
    )
    .env("MCP_TOOL_TIMEOUT", MCP_TOOL_TIMEOUT_MS)
    .env(
        "CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT",
        MCP_TOOL_IDLE_TIMEOUT_MS,
    );
    if let Some(model) = model.filter(|m| m != "default") {
        cmd.arg("--model").arg(model);
    }
    let input = user_message(prompt, &state.agents.images().join(id), images).await?;
    let mut child = cmd
        .spawn()
        .with_context(|| format!("can't run {}", claude.display()))?;
    let stopper = Stopper::sigint(&child);
    state.agents.set_stopper(id, stopper.clone());
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

    let stderr = crate::agent::stderr_tail(child.stderr.take().unwrap());
    let mut lines = OutputLines::new(child.stdout.take().unwrap(), stopper);
    while let Some(line) = lines.next(state).await? {
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
    let stderr = stderr.await.unwrap_or_default();
    sink.finish(|| {
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
                    parent: None,
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

/// The model `kind` of job runs on.
fn job_model(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Summary | JobKind::Scan => "haiku",
        JobKind::Prune => "sonnet",
    }
}

/// Runs `claude -p --output-format json --json-schema` for `job` on its
/// input in `cwd` and returns its `structured_output`.
async fn structured(claude: &Path, job: &JobSpec<'_>, cwd: &Path) -> Result<Value> {
    let name = job.kind.name();
    let mut cmd = tokio::process::Command::new(claude);
    cmd.args([
        "-p",
        "--model",
        job_model(job.kind),
        "--output-format",
        "json",
    ])
    .arg("--json-schema")
    .arg(job.schema.to_string())
    .args(["--no-session-persistence", "--strict-mcp-config"])
    // Not the project settings of a clone it runs in, which could
    // allow more tools or add hooks.
    .args(["--setting-sources", "user"]);
    let (tools, allowed): (_, &[&str]) = match job.tools {
        JobTools::None => ("", &[]),
        JobTools::ReadOnly => ("Read,Glob,Grep,Bash", crate::cleanup::READ_ONLY_TOOLS),
    };
    cmd.arg("--tools").arg(tools);
    cmd.args(["--permission-mode", "dontAsk", "--max-budget-usd"])
        .arg(job.budget.usd.to_string());
    if let Some(add_dir) = job.add_dir {
        cmd.arg("--add-dir").arg(add_dir);
    }
    // Last, since it takes every value up to the next flag.
    if !allowed.is_empty() {
        cmd.arg("--allowedTools").args(allowed);
    }
    cmd.current_dir(cwd)
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
        .map_err(|_| anyhow!("the {name} took longer than {}s", job.timeout.as_secs()))??;
    structured_output(name, &out.stdout).with_context(|| {
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
    use grenadine_core::api::AgentBackend;

    use super::*;
    use crate::agent::tests::state_with_claude;

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
                    parent: None,
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

    /// Claude Code lists its models in its answer to `initialize`, as
    /// 2.1 writes it, shortened. The backend it shows offers them.
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

    /// Without a list, the models are empty and prompts can ask for any
    /// model that can't pass for a flag; Claude Code rejects one it
    /// doesn't have.
    #[tokio::test]
    async fn offers_any_model_without_a_list() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_claude(&fx, dir.path(), "#!/bin/sh\nexit 1\n");
        let backends = state.agents.backends().await;
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].models, []);
        let offers = async |model| state.agents.offers(AgentBackend::Claude, model).await;
        assert!(offers(None).await);
        assert!(offers(Some("opus")).await);
        assert!(!offers(Some("--help")).await);
        assert!(!offers(Some("")).await);
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
