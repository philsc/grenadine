//! A minimal MCP server (JSON-RPC over streamable HTTP, JSON responses
//! only). Its tool `approve` is what Claude Code calls through
//! `--permission-prompt-tool` before it uses a tool. The call waits until
//! the user allows or denies the tool use on the page. The user's answers
//! to `AskUserQuestion` and the mode an approved plan goes on in come back
//! with the approval.
//!
//! The terminal tools let Claude read the session's terminal tabs, e.g. to
//! see what a command it asked the user to run printed. `show_image` puts
//! an image, such as a screenshot, into the transcript.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::Json;
use axum::extract::{Path, State as AxState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use grenadine_core::api::{AgentEventKind, PermissionMode};
use serde_json::{Value, json};

use crate::agent::{self, Decision};
use crate::sync::State;

/// How many lines of a terminal Claude gets unless it asks for more.
const LINES: usize = 200;

/// How long a wait for a terminal lasts unless Claude says otherwise.
const WAIT: Duration = Duration::from_secs(600);

/// The longest a wait for a terminal lasts.
const MAX_WAIT: Duration = Duration::from_secs(3600);

/// How long a terminal at its prompt has to be quiet for a wait without
/// `until` to end, unless Claude says otherwise.
const IDLE: Duration = Duration::from_secs(3);

/// The largest image `show_image` shows.
const MAX_IMAGE: u64 = 20 << 20;

/// Forgets the permission request when Claude Code hangs up without an
/// answer, e.g. because the turn was interrupted.
struct PendingGuard<'a> {
    state: &'a State,
    id: &'a str,
    tool_use: &'a str,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        agent::drop_approval(self.state, self.id, self.tool_use);
    }
}

fn reply(id: &Value, result: Value) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn error(id: &Value, code: i64, message: &str) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
        .into_response()
}

/// The tool's answer when the user allows a tool use. `AskUserQuestion`
/// takes the user's answers as part of its input, and `setMode` switches
/// the turn out of plan mode when `ExitPlanMode` is allowed.
fn allowed(
    input: &Value,
    mode: Option<PermissionMode>,
    answers: Option<BTreeMap<String, String>>,
) -> Value {
    let mut input = input.clone();
    if let (Some(answers), Some(fields)) = (answers, input.as_object_mut()) {
        fields.insert("answers".into(), json!(answers));
    }
    let mut answer = json!({"behavior": "allow", "updatedInput": input});
    if let Some(mode) = mode {
        answer["updatedPermissions"] = json!([{
            "type": "setMode",
            "mode": mode.as_str(),
            "destination": "session",
        }]);
    }
    answer
}

/// What Claude is told when the user denies a tool use without saying why.
fn denied(tool_name: &str) -> &'static str {
    match tool_name {
        "ExitPlanMode" => "The user wants you to keep planning.",
        "AskUserQuestion" => "The user didn't answer.",
        _ => "The user denied this tool use.",
    }
}

pub async fn handle(
    AxState(state): AxState<Arc<State>>,
    Path((id, secret)): Path<(String, String)>,
    Json(msg): Json<Value>,
) -> Response {
    let known = agent::valid_id(&id) && state.db.agent(&id).ok().flatten().is_some();
    if !known || state.agents.secret(&id) != secret {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Notifications and responses get no answer.
    let Some(rpc_id) = msg.get("id").filter(|_| msg.get("method").is_some()) else {
        return StatusCode::ACCEPTED.into_response();
    };
    let params = &msg["params"];
    match msg["method"].as_str().unwrap_or_default() {
        "initialize" => reply(
            rpc_id,
            json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "grenadine", "version": "0"},
            }),
        ),
        "ping" => reply(rpc_id, json!({})),
        "tools/list" => reply(
            rpc_id,
            json!({"tools": [{
                "name": "approve",
                "description": "Asks the grenadine user whether a tool may be used.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "tool_name": {"type": "string"},
                        "input": {"type": "object"},
                        "tool_use_id": {"type": "string"},
                    },
                    "required": ["tool_name", "input"],
                },
            }, {
                "name": "list_terminals",
                "description": "Lists the terminal tabs open in this session's page, with the \
                    last line of each, usually its prompt. They are login shells in the \
                    session's working directory that the user types into.",
                "inputSchema": {"type": "object", "properties": {}},
                "annotations": {"readOnlyHint": true},
            }, {
                "name": "read_terminal",
                "description": "Reads what a terminal tab shows, as plain text, including its \
                    scrollback. Use it to see a command's output after asking the user to run \
                    it in a terminal, instead of asking them to paste it.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "terminal": {
                            "type": "integer",
                            "description": "The terminal's number, N in its tab's name \
                                \"Terminal N\". Defaults to the newest terminal.",
                        },
                        "lines": {
                            "type": "integer",
                            "description": "How many of the last lines to read. Defaults to 200.",
                        },
                    },
                },
                "annotations": {"readOnlyHint": true},
            }, {
                "name": "wait_for_terminal",
                "description": "Waits until a command run in a terminal tab finishes, then \
                    reads the terminal like read_terminal. Call it right after asking the \
                    user to run a command there; it also covers a command that is already \
                    running. It ends at the first of: the output since the call matches \
                    `until`; a command finished and the shell is back at its prompt; the \
                    shell is at its prompt and quiet for `idle_seconds` after a line of \
                    output, for commands too quick to see run; the shell exits; the timeout. \
                    The result says which.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "terminal": {
                            "type": "integer",
                            "description": "The terminal's number, N in its tab's name \
                                \"Terminal N\". Defaults to the newest terminal.",
                        },
                        "until": {
                            "type": "string",
                            "description": "A regex for the output since the call to match.",
                        },
                        "idle_seconds": {
                            "type": "number",
                            "description": "Defaults to 3 without `until`, else unset.",
                        },
                        "timeout_seconds": {
                            "type": "number",
                            "description": "Defaults to 600, at most 3600.",
                        },
                        "lines": {
                            "type": "integer",
                            "description": "How many of the last lines to read. Defaults to 200.",
                        },
                    },
                },
                "annotations": {"readOnlyHint": true},
            }, {
                "name": "show_image",
                "description": "Shows the user an image file, such as a screenshot, in \
                    the session's transcript. Use it whenever the user asks how something \
                    looks, or to show what a screenshot you took shows. PNG, JPEG, GIF and \
                    WebP are supported.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "The image's path, absolute or relative to the \
                                session's working directory.",
                        },
                        "caption": {
                            "type": "string",
                            "description": "A short caption shown below the image.",
                        },
                    },
                    "required": ["path"],
                },
            }]}),
        ),
        "tools/call" if params["name"] == "approve" => {
            let args = &params["arguments"];
            let tool_name = args["tool_name"].as_str().unwrap_or_default();
            let tool_use = match args["tool_use_id"].as_str() {
                Some(t) => t.to_owned(),
                None => format!("request-{rpc_id}"),
            };
            let decision =
                match agent::request_approval(&state, &id, &tool_use, tool_name, &args["input"]) {
                    Ok(rx) => {
                        let _guard = PendingGuard {
                            state: &state,
                            id: &id,
                            tool_use: &tool_use,
                        };
                        tokio::select! {
                            d = rx => d.ok(),
                            _ = state.shutdown.cancelled() => None,
                        }
                    }
                    Err(e) => {
                        tracing::warn!("agent {id}: {e:#}");
                        None
                    }
                };
            let answer = match decision {
                Some(Decision {
                    allow: true,
                    mode,
                    answers,
                    ..
                }) => allowed(&args["input"], mode, answers),
                Some(Decision {
                    allow: false,
                    message,
                    ..
                }) => json!({
                    "behavior": "deny",
                    "message": message.unwrap_or_else(|| denied(tool_name).into()),
                }),
                None => json!({"behavior": "deny", "message": "Nobody answered the request."}),
            };
            reply(
                rpc_id,
                json!({"content": [{"type": "text", "text": answer.to_string()}]}),
            )
        }
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let args = &params["arguments"];
            let result = match name {
                "show_image" => show_image(&state, &id, args).await,
                _ => terminal_tool(&state, &id, name, args).await,
            };
            let (text, failed) = match result {
                Ok(text) => (text, false),
                Err(e) => (format!("{e:#}"), true),
            };
            reply(
                rpc_id,
                json!({"content": [{"type": "text", "text": text}], "isError": failed}),
            )
        }
        method => error(rpc_id, -32601, &format!("unknown method {method}")),
    }
}

/// A non-negative number of seconds.
fn seconds(value: &Value) -> Option<Duration> {
    value
        .as_f64()
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
}

/// The MIME type of an image in a format browsers show, from its first
/// bytes.
fn image_mime(data: &[u8]) -> Option<&'static str> {
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

/// Puts the image at `args.path` into the session's transcript.
async fn show_image(state: &State, id: &str, args: &Value) -> anyhow::Result<String> {
    let path = args["path"].as_str().context("`path` is required")?;
    let session = state.db.agent(id)?.context("The session is gone.")?;
    let path = std::path::Path::new(&session.worktree).join(path);
    let size = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("Can't read {}", path.display()))?
        .len();
    if size > MAX_IMAGE {
        bail!("{} is larger than {} MB.", path.display(), MAX_IMAGE >> 20);
    }
    let data = tokio::fs::read(&path)
        .await
        .with_context(|| format!("Can't read {}", path.display()))?;
    let mime = image_mime(&data)
        .with_context(|| format!("{} isn't a PNG, JPEG, GIF or WebP image.", path.display()))?;
    let image = agent::store_image(state, id, mime, &data)?;
    let caption = args["caption"]
        .as_str()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    agent::emit(state, id, AgentEventKind::Image { id: image, caption })?;
    Ok("The user sees the image in the transcript.".into())
}

/// Runs one of the tools that read the session's terminals.
async fn terminal_tool(
    state: &State,
    id: &str,
    name: &str,
    args: &Value,
) -> anyhow::Result<String> {
    let terminals = &state.agents.terminals;
    let open = terminals.list(id);
    if name == "list_terminals" {
        if open.is_empty() {
            return Ok("No terminals are open.".into());
        }
        let list: Vec<_> = open
            .iter()
            .filter_map(|t| {
                let last = terminals.get(id, t.id)?.text(1);
                Some(format!("Terminal {}: {last}", t.id))
            })
            .collect();
        return Ok(list.join("\n"));
    }
    let number = match args["terminal"].as_u64() {
        Some(n) => n,
        None => {
            open.last()
                .context("No terminals are open. The user opens them below the transcript.")?
                .id
        }
    };
    let terminal = terminals
        .get(id, number)
        .with_context(|| format!("Terminal {number} isn't open."))?;
    let lines = args["lines"]
        .as_u64()
        .map_or(LINES, |n| usize::try_from(n).unwrap_or(usize::MAX));
    let text = match name {
        "read_terminal" => terminal.text(lines),
        "wait_for_terminal" => {
            let until = args["until"]
                .as_str()
                .map(fancy_regex::Regex::new)
                .transpose()
                .context("`until` isn't a valid regex")?;
            let idle = seconds(&args["idle_seconds"]).or(until.is_none().then_some(IDLE));
            let timeout = seconds(&args["timeout_seconds"])
                .unwrap_or(WAIT)
                .min(MAX_WAIT);
            let end = tokio::select! {
                end = terminal.wait(until.as_ref(), idle, timeout) => end,
                _ = state.shutdown.cancelled() => bail!("The server is shutting down."),
            };
            format!("{}\n\n{}", end.describe(), terminal.text(lines))
        }
        _ => bail!("There is no tool {name}."),
    };
    Ok(if text.is_empty() {
        "The terminal shows nothing yet.".into()
    } else {
        text
    })
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{AgentSession, AgentStatus};

    use super::*;

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    async fn call(state: &Arc<State>, secret: &str, msg: Value) -> (StatusCode, Value) {
        let response = handle(
            AxState(state.clone()),
            Path((ID.to_owned(), secret.to_owned())),
            Json(msg),
        )
        .await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    fn state() -> Arc<State> {
        let state = crate::sync::test_state();
        state
            .db
            .create_agent(&AgentSession {
                id: ID.into(),
                repo: "o/n".into(),
                pr: None,
                title: "t".into(),
                summary: None,
                branch: "b".into(),
                worktree: "/w".into(),
                base_sha: "s".into(),
                status: AgentStatus::Running,
                created_at: 0,
                cost_usd: 0.0,
                context_tokens: None,
                context_window: None,
                model: None,
                landed: None,
                prs: None,
                unstaged: 0,
            })
            .unwrap();
        state
    }

    #[tokio::test]
    async fn needs_the_secret() {
        let state = state();
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        let (status, _) = call(&state, "wrong", msg.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let secret = state.agents.secret(ID);
        let (status, body) = call(&state, &secret, msg).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"], json!({}));
    }

    #[tokio::test]
    async fn handshake() {
        let state = state();
        let secret = state.agents.secret(ID);
        let (_, body) = call(
            &state,
            &secret,
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                   "params": {"protocolVersion": "2025-11-25", "capabilities": {}}}),
        )
        .await;
        assert_eq!(body["id"], 0);
        assert_eq!(body["result"]["protocolVersion"], "2025-11-25");
        let (status, _) = call(
            &state,
            &secret,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let (_, body) = call(
            &state,
            &secret,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
        let names: Vec<_> = body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "approve",
                "list_terminals",
                "read_terminal",
                "wait_for_terminal",
                "show_image"
            ]
        );
        let (_, body) = call(
            &state,
            &secret,
            json!({"jsonrpc": "2.0", "id": "x", "method": "server/discover"}),
        )
        .await;
        assert_eq!(body["error"]["code"], -32601);
    }

    /// Calls the tool the way Claude Code 2.1 does and answers on the page.
    async fn ask(state: &Arc<State>, tool_use: &str, allow: bool) -> Value {
        let input = json!({"file_path": "/w/b.txt", "content": "hello\n"});
        let decision = Decision {
            allow,
            ..Decision::default()
        };
        ask_with(state, tool_use, "Write", input, decision).await
    }

    async fn ask_with(
        state: &Arc<State>,
        tool_use: &str,
        tool_name: &str,
        input: Value,
        decision: Decision,
    ) -> Value {
        let secret = state.agents.secret(ID);
        let request = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "approve",
                "arguments": {
                    "tool_name": tool_name,
                    "input": input,
                    "tool_use_id": tool_use,
                },
            },
        });
        let task = tokio::spawn({
            let state = state.clone();
            async move { call(&state, &secret, request).await }
        });
        for _ in 0..100 {
            if state.db.agent(ID).unwrap().unwrap().status == AgentStatus::AwaitingApproval {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(agent::approve(state, ID, tool_use, decision).unwrap());
        let (_, body) = task.await.unwrap();
        serde_json::from_str(body["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn approvals_wait_for_the_user() {
        let state = state();
        assert_eq!(
            ask(&state, "toolu_1", true).await,
            json!({"behavior": "allow", "updatedInput": {"file_path": "/w/b.txt", "content": "hello\n"}})
        );
        assert_eq!(
            state.db.agent(ID).unwrap().unwrap().status,
            AgentStatus::Running
        );
        assert_eq!(ask(&state, "toolu_2", false).await["behavior"], "deny");
        // Answered requests are gone.
        let decision = Decision {
            allow: true,
            ..Decision::default()
        };
        assert!(!agent::approve(&state, ID, "toolu_2", decision).unwrap());

        let kinds: Vec<_> = state
            .db
            .agent_events(ID)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                AgentEventKind::ApprovalRequested {
                    id: "toolu_1".into(),
                    tool_name: "Write".into(),
                    input: r#"{"content":"hello\n","file_path":"/w/b.txt"}"#.into(),
                },
                AgentEventKind::ApprovalResolved {
                    id: "toolu_1".into(),
                    allow: true,
                    message: None,
                },
                AgentEventKind::ApprovalRequested {
                    id: "toolu_2".into(),
                    tool_name: "Write".into(),
                    input: r#"{"content":"hello\n","file_path":"/w/b.txt"}"#.into(),
                },
                AgentEventKind::ApprovalResolved {
                    id: "toolu_2".into(),
                    allow: false,
                    message: None,
                },
            ]
        );
    }

    /// A plan sent back keeps the user's feedback, and Claude hears to keep
    /// planning when there is none.
    #[tokio::test]
    async fn plans_sent_back() {
        let state = state();
        let plan = json!({"plan": "# Plan"});
        let feedback = Decision {
            allow: false,
            message: Some("Add a title".into()),
            ..Decision::default()
        };
        let answer = ask_with(&state, "toolu_1", "ExitPlanMode", plan.clone(), feedback).await;
        assert_eq!(answer["message"], "Add a title");
        let silent = Decision::default();
        let answer = ask_with(&state, "toolu_2", "ExitPlanMode", plan, silent).await;
        assert_eq!(answer["message"], "The user wants you to keep planning.");

        let resolved: Vec<_> = state
            .db
            .agent_events(ID)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.kind {
                AgentEventKind::ApprovalResolved { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(resolved, [Some("Add a title".into()), None]);
    }

    /// Calls a terminal tool and returns its text and whether it failed.
    async fn tool(state: &Arc<State>, name: &str, arguments: Value) -> (String, bool) {
        let secret = state.agents.secret(ID);
        let request = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        });
        let (_, body) = call(state, &secret, request).await;
        let result = &body["result"];
        let text = result["content"][0]["text"].as_str().unwrap().to_owned();
        (text, result["isError"].as_bool().unwrap())
    }

    #[tokio::test]
    async fn reads_and_waits_for_terminals() {
        let mut state = state();
        Arc::get_mut(&mut state).unwrap().agents.terminals =
            crate::terminal::Terminals::new("/bin/sh".into());
        let dir = tempfile::tempdir().unwrap();
        let mut session = state.db.agent(ID).unwrap().unwrap();
        session.worktree = dir.path().to_str().unwrap().into();

        assert_eq!(
            tool(&state, "list_terminals", json!({})).await,
            ("No terminals are open.".into(), false)
        );
        let (text, failed) = tool(&state, "read_terminal", json!({})).await;
        assert!(
            failed && text.starts_with("No terminals are open."),
            "{text}"
        );

        let id = crate::terminal::open(&state, &session).unwrap().id;
        let terminal = state.agents.terminals.get(ID, id).unwrap();
        let (text, failed) = tool(&state, "read_terminal", json!({"terminal": id + 1})).await;
        assert!(
            failed && text == format!("Terminal {} isn't open.", id + 1),
            "{text}"
        );

        // A command started after the wait began, and one already running.
        for already_running in [false, true] {
            let run = || async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                terminal
                    .write(b"sleep 1; echo done-$((6 * 7))\n")
                    .await
                    .unwrap();
            };
            if already_running {
                run().await;
            }
            let wait = tokio::spawn({
                let state = state.clone();
                async move { tool(&state, "wait_for_terminal", json!({})).await }
            });
            if !already_running {
                run().await;
            }
            let (text, failed) = wait.await.unwrap();
            assert!(!failed, "{text}");
            assert!(
                text.starts_with("A command finished and the shell is back at its prompt."),
                "{text}"
            );
            assert!(text.contains("\ndone-42\n"), "{text}");
        }

        let wait = tokio::spawn({
            let state = state.clone();
            async move {
                let args = json!({"terminal": id, "until": r"ready-\d+", "lines": 1});
                tool(&state, "wait_for_terminal", args).await
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        terminal
            .write(b"echo ready-$((1 + 1)); sleep 30\n")
            .await
            .unwrap();
        let (text, _) = wait.await.unwrap();
        assert_eq!(text, "The output matched.\n\nready-2");
        let (text, _) = tool(&state, "read_terminal", json!({"lines": 2})).await;
        assert!(text.ends_with("\nready-2"), "{text}");

        let args = json!({"timeout_seconds": 0.3, "until": "never"});
        let (text, _) = tool(&state, "wait_for_terminal", args).await;
        assert!(text.starts_with("The wait timed out."), "{text}");

        // Another session doesn't see the terminal.
        let other = "99999999-2222-4333-8444-555555555555";
        let err = terminal_tool(&state, other, "read_terminal", &json!({"terminal": id}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), format!("Terminal {id} isn't open."));

        state.agents.terminals.close_all(ID).await;
    }

    /// The answers Claude Code 2.1 takes, as tried with a real `claude -p`.
    #[test]
    fn allowed_answers() {
        let plan = json!({"plan": "# Plan", "planFilePath": "/p.md"});
        assert_eq!(
            allowed(&plan, Some(PermissionMode::AcceptEdits), None),
            json!({
                "behavior": "allow",
                "updatedInput": plan,
                "updatedPermissions": [
                    {"type": "setMode", "mode": "acceptEdits", "destination": "session"},
                ],
            })
        );
        let questions = json!({"questions": [{"question": "Tea?", "options": []}]});
        let answers = BTreeMap::from([("Tea?".to_owned(), "Yes".to_owned())]);
        assert_eq!(
            allowed(&questions, None, Some(answers)),
            json!({
                "behavior": "allow",
                "updatedInput": {
                    "questions": [{"question": "Tea?", "options": []}],
                    "answers": {"Tea?": "Yes"},
                },
            })
        );
    }

    #[tokio::test]
    async fn shows_images() {
        let state = state();
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "hello").unwrap();

        let (message, failed) = tool(&state, "show_image", json!({"path": text})).await;
        assert!(failed && message.ends_with("isn't a PNG, JPEG, GIF or WebP image."));
        let (message, failed) = tool(&state, "show_image", json!({})).await;
        assert!(failed, "{message}");

        let (_, failed) = tool(
            &state,
            "show_image",
            json!({"path": png, "caption": " The page "}),
        )
        .await;
        assert!(!failed);
        let events = state.db.agent_events(ID).unwrap();
        let [event] = &events[..] else {
            panic!("{events:?}")
        };
        let AgentEventKind::Image { id, caption } = &event.kind else {
            panic!("{event:?}")
        };
        assert_eq!(caption.as_deref(), Some("The page"));
        assert_eq!(
            state.db.agent_image(id).unwrap(),
            Some(("image/png".into(), b"\x89PNG\r\n\x1a\nrest".to_vec()))
        );
    }
}
