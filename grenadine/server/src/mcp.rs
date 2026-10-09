//! A minimal MCP server (JSON-RPC over streamable HTTP, JSON responses
//! only) with one tool, `approve`, that Claude Code calls through
//! `--permission-prompt-tool` before it uses a tool. The call waits until
//! the user allows or denies the tool use on the page.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State as AxState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::agent::{self, Decision};
use crate::sync::State;

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
            }]}),
        ),
        "tools/call" if params["name"] == "approve" => {
            let args = &params["arguments"];
            let tool_name = args["tool_name"].as_str().unwrap_or_default();
            let tool_use = match args["tool_use_id"].as_str() {
                Some(t) => t.to_owned(),
                None => format!("request-{rpc_id}"),
            };
            let decision = match agent::request_approval(
                &state,
                &id,
                &tool_use,
                tool_name,
                &args["input"],
            ) {
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
                Some(Decision { allow: true, .. }) => {
                    json!({"behavior": "allow", "updatedInput": args["input"]})
                }
                Some(Decision {
                    allow: false,
                    message,
                }) => json!({
                    "behavior": "deny",
                    "message": message.unwrap_or_else(|| "The user denied this tool use.".into()),
                }),
                None => json!({"behavior": "deny", "message": "Nobody answered the request."}),
            };
            reply(
                rpc_id,
                json!({"content": [{"type": "text", "text": answer.to_string()}]}),
            )
        }
        method => error(rpc_id, -32601, &format!("unknown method {method}")),
    }
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{AgentEventKind, AgentSession, AgentStatus};

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
                branch: "b".into(),
                worktree: "/w".into(),
                base_sha: "s".into(),
                status: AgentStatus::Running,
                created_at: 0,
                cost_usd: 0.0,
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
        assert_eq!(body["result"]["tools"][0]["name"], "approve");
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
        let secret = state.agents.secret(ID);
        let request = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "approve",
                "arguments": {
                    "tool_name": "Write",
                    "input": {"file_path": "/w/b.txt", "content": "hello\n"},
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
        let decision = Decision {
            allow,
            message: None,
        };
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
        assert_eq!(
            ask(&state, "toolu_2", false).await["behavior"],
            "deny"
        );
        // Answered requests are gone.
        let decision = Decision {
            allow: true,
            message: None,
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
                    allow: true
                },
                AgentEventKind::ApprovalRequested {
                    id: "toolu_2".into(),
                    tool_name: "Write".into(),
                    input: r#"{"content":"hello\n","file_path":"/w/b.txt"}"#.into(),
                },
                AgentEventKind::ApprovalResolved {
                    id: "toolu_2".into(),
                    allow: false
                },
            ]
        );
    }
}
