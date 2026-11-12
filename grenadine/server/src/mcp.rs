//! A minimal MCP server (JSON-RPC over streamable HTTP, JSON responses
//! only). Its tool `approve` is what Claude Code calls through
//! `--permission-prompt-tool` before it uses a tool. The call waits until
//! the user allows or denies the tool use on the page. The user's answers
//! to `AskUserQuestion` and the mode an approved plan goes on in come back
//! with the approval.
//!
//! The terminal tools let Claude read the session's terminal tabs, e.g. to
//! see what a command it asked the user to run printed. `show_image` puts
//! an image, such as a screenshot, into the transcript. `update_prs` and
//! `land_on_checkout` land the session's work the way the page's wrap-up
//! does, from inside the agent's own turn. `handoff` starts a new
//! session, with a fresh context, once the user agrees.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::Json;
use axum::extract::{Path, State as AxState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use grenadine_core::api::{
    AgentBackend, AgentEventKind, AgentSession, GENERAL, Landed, NewAgent, PermissionMode, PrKey,
    SessionKind, WrapUpTarget,
};
use serde_json::{Value, json};

use crate::agent::{self, Decision};
use crate::sync::State;

/// How many lines of a terminal Claude gets unless it asks for more.
const LINES: usize = 200;

/// How long a wait for a terminal lasts unless Claude says otherwise.
const WAIT: Duration = Duration::from_secs(600);

/// The longest a wait for a terminal lasts.
const MAX_WAIT: Duration = Duration::from_secs(3600);

/// The longest a wait for a terminal lasts in a Devin session. Devin
/// gives an MCP tool call 120 s, then sends it again once and reports
/// the server as unreachable after the second try also runs out.
/// Progress notifications don't extend that.
const DEVIN_MAX_WAIT: Duration = Duration::from_secs(100);

/// How long a terminal at its prompt has to be quiet for a wait without
/// `until` to end, unless Claude says otherwise.
const IDLE: Duration = Duration::from_secs(3);

/// The tools agents may use without asking: all of this server's but
/// `approve`, which answers for a permission request.
pub const ALLOWED_TOOLS: [&str; 9] = [
    "mcp__grenadine__list_terminals",
    "mcp__grenadine__read_terminal",
    "mcp__grenadine__wait_for_terminal",
    "mcp__grenadine__show_image",
    "mcp__grenadine__update_prs",
    "mcp__grenadine__land_on_checkout",
    "mcp__grenadine__session_pr",
    "mcp__grenadine__manage_prs",
    "mcp__grenadine__handoff",
];

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
    answers: Option<BTreeMap<String, Vec<String>>>,
) -> Value {
    let mut input = input.clone();
    if let (Some(answers), Some(fields)) = (answers, input.as_object_mut()) {
        fields.insert(
            "answers".into(),
            json!(
                answers
                    .into_iter()
                    .map(|(q, a)| (q, a.join(", ")))
                    .collect::<BTreeMap<_, _>>()
            ),
        );
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

/// `ExitPlanMode`'s input with the notes the user approved the plan with
/// added to the plan. Claude Code repeats the approved plan to Claude, so
/// that's how Claude hears them.
fn with_notes(input: &Value, notes: Option<&str>) -> Value {
    let mut input = input.clone();
    if let (Some(notes), Some(plan)) = (
        notes.map(str::trim).filter(|n| !n.is_empty()),
        input["plan"].as_str(),
    ) {
        input["plan"] = format!(
            "{plan}\n\n---\n\nThe user approved this plan, asking you to keep the following in mind:\n\n{notes}"
        )
        .into();
    }
    input
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
    let session = agent::valid_id(&id)
        .then(|| state.db.agent(&id).ok().flatten())
        .flatten();
    let Some(session) = session.filter(|_| state.agents.secret(&id) == secret) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let backend = session.backend;
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
                            "description": timeout_description(backend),
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
            }, {
                "name": "update_prs",
                "description": "Pushes this session's commits to their PRs' branches, \
                    updates this session's open pull requests and opens the new ones, each \
                    stacked on the one before it, titled and described from the commit \
                    messages. To change a pull request, amend its commits and call this \
                    again. `prs` names each pull request's tip commit, bottom first; a pull \
                    request takes the commits after the tip below it up to its own, so one \
                    entry per commit gives each commit its own pull request, and the last \
                    entry must be the top commit. Open pull requests left out are closed and \
                    their branches deleted. Without `prs`, the commits map onto the open pull \
                    requests one per commit when there are as many of each, and a lone \
                    commit opens a pull request; otherwise the call asks for `prs`. New pull requests should be given a `branch` in \
                    `prs`, named following this repository's branch naming conventions. The \
                    pull requests are kept as a GitHub stack where the repository has them \
                    turned on. A session that started from a pull request stacks its pull \
                    requests on it; naming that pull request in the first entry of `prs` \
                    replaces its commits instead, e.g. after rebasing it. The pull requests \
                    this session manages include any it took over, with manage_prs or by \
                    starting from a pull request with others stacked on it: each keeps its \
                    own branch and needs its own entry in `prs`. Pull requests another \
                    session took over from this one can't be updated or closed anymore.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prs": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "commit": {
                                        "type": "string",
                                        "description": "A full or abbreviated SHA of the \
                                            pull request's tip commit; `pr` omitted opens \
                                            a new pull request.",
                                    },
                                    "pr": {"type": "integer"},
                                    "branch": {
                                        "type": "string",
                                        "description": "The branch a new pull request is \
                                            pushed from; only with `pr` omitted. Name it \
                                            following this repository's branch naming \
                                            conventions. Without it the branch is \
                                            grenadine/agent/<session>.",
                                    },
                                },
                                "required": ["commit"],
                            },
                        },
                    },
                },
            }, {
                "name": "land_on_checkout",
                "description": "Rebases this session's commits onto the user's checkout \
                    of the repository and fast-forwards the checkout to them.",
                "inputSchema": {"type": "object", "properties": {}},
            }, {
                "name": "session_pr",
                "description": "Describes the pull request this session started from: its \
                    number, title, URL, state, branches, the commit range of its own \
                    changes (from the merge base with its target branch, which for a \
                    stacked pull request is the parent's branch), this session's own \
                    commits, its stack, and the pull requests this session manages, \
                    bottom first, with their branches.",
                "inputSchema": {"type": "object", "properties": {}},
                "annotations": {"readOnlyHint": true},
            }, {
                "name": "manage_prs",
                "description": "Takes over pull requests this session didn't open, so \
                    that update_prs updates them too, e.g. to change the commits of the pull \
                    request this session started from while others are stacked on it. A \
                    session manages one linear stack: the pull requests named, the one this \
                    session started from and this session's open ones must make one \
                    contiguous chain, each stacked on the one before it. Naming pull \
                    requests below this session's moves the pull request it counts as \
                    started from down to the chain's bottom. Pull requests another session \
                    manages are taken from it once the user agrees. Their heads are fetched \
                    as refs and HEAD is left alone: the result lists each pull request's \
                    commits to bring onto HEAD before calling update_prs with one `prs` \
                    entry per pull request.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prs": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "description": "The numbers of the pull requests to take over.",
                        },
                    },
                    "required": ["prs"],
                },
            }, {
                "name": "handoff",
                "description": "Hands work off to a new agent session in Grenadine, with a \
                    fresh context: the new session sees only `prompt`, nothing of this \
                    session, so write it to stand on its own, with what the new session \
                    needs to know. It starts with this session's agent and model, in its own \
                    worktree on the trunk or on a pull request's head; it can't see this \
                    session's commits until they're pushed, so to build on them, call \
                    update_prs first and pass the pull request as `pr`. Unless this session \
                    bypasses permissions, the user sees the proposed session and may change \
                    it before it starts, or decline it. Returns once the new session has \
                    started; it doesn't wait for its work.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The new session's first prompt.",
                        },
                        "repo": {
                            "type": "string",
                            "description": format!(
                                "The repository to work in, `owner/name`, or `{GENERAL}` \
                                    for a session in no repository. Defaults to this \
                                    session's."
                            ),
                        },
                        "pr": {
                            "type": "integer",
                            "description": "Start from this pull request's head in `repo` \
                                instead of the trunk.",
                        },
                        "permission_mode": {
                            "type": "string",
                            "enum": backend.modes().iter().map(|(m, _)| *m).collect::<Vec<_>>(),
                            "description": "The permission mode the new session starts \
                                in. Defaults to this turn's.",
                        },
                    },
                    "required": ["prompt"],
                },
            }]}),
        ),
        "tools/call" if params["name"] == "approve" => {
            let args = &params["arguments"];
            let tool_name = args["tool_name"].as_str().unwrap_or_default();
            let tool_use = match args["tool_use_id"].as_str() {
                Some(t) => t.to_owned(),
                None => agent::approval_id("request"),
            };
            // Claude Code hanging up, e.g. because the turn was
            // interrupted, drops the wait and so the request.
            let decision = agent::await_approval(
                &state,
                &id,
                &tool_use,
                tool_name,
                &args["input"],
                Vec::new(),
            )
            .await;
            let answer = match decision {
                Some(Decision {
                    allow: true,
                    message,
                    mode,
                    answers,
                    ..
                }) => {
                    let input = if tool_name == "ExitPlanMode" {
                        with_notes(&args["input"], message.as_deref())
                    } else {
                        args["input"].clone()
                    };
                    allowed(&input, mode, answers)
                }
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
                "update_prs" => {
                    update_prs(&state, &id, &agent::approval_id("update_prs"), args).await
                }
                "land_on_checkout" => land_on_checkout(&state, &id).await,
                "session_pr" => session_pr(&state, &session).await,
                "manage_prs" => {
                    manage_prs(&state, &id, &agent::approval_id("manage_prs"), args).await
                }
                "handoff" => handoff(&state, &session, &agent::approval_id("handoff"), args).await,
                _ => terminal_tool(&state, &id, name, args, backend).await,
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

/// The longest a wait for a terminal lasts on the session's backend.
fn max_wait(backend: AgentBackend) -> Duration {
    match backend {
        AgentBackend::Claude => MAX_WAIT,
        AgentBackend::Devin => DEVIN_MAX_WAIT,
    }
}

/// How long a `wait_for_terminal` call waits: `timeout_seconds` or
/// `WAIT`, capped at what the session's backend gives a tool call.
fn wait_timeout(value: &Value, backend: AgentBackend) -> Duration {
    seconds(value).unwrap_or(WAIT).min(max_wait(backend))
}

/// What `wait_for_terminal`'s `timeout_seconds` tells the agent: the
/// default and the cap the session's backend allows.
fn timeout_description(backend: AgentBackend) -> String {
    match backend {
        AgentBackend::Claude => format!(
            "Defaults to {}, at most {}.",
            WAIT.as_secs(),
            MAX_WAIT.as_secs()
        ),
        AgentBackend::Devin => format!(
            "Defaults to {}, at most {}. Call the tool again to keep waiting.",
            DEVIN_MAX_WAIT.as_secs(),
            DEVIN_MAX_WAIT.as_secs()
        ),
    }
}

/// Puts the image at `args.path` into the session's transcript.
async fn show_image(state: &State, id: &str, args: &Value) -> anyhow::Result<String> {
    let path = args["path"].as_str().context("`path` is required")?;
    let session = state.db.agent(id)?.context("The session is gone.")?;
    let (mime, data) = agent::read_image(&session.worktree, path)?;
    let image = agent::store_image(state, id, mime, &data)?;
    let caption = args["caption"]
        .as_str()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    agent::emit(state, id, AgentEventKind::Image { id: image, caption })?;
    Ok("The user sees the image in the transcript.".into())
}

/// Pushes the session's commits to their branches and opens or updates
/// its pull requests, as the page's Open PR wrap-up does. A session that
/// started from a PR stacks its pull requests on that PR's branch, so it
/// asks the user first.
async fn update_prs(
    state: &Arc<State>,
    id: &str,
    tool_use: &str,
    args: &Value,
) -> anyhow::Result<String> {
    // The approval wait is the slow part; a resent call must be refused
    // before it shows a second approval card.
    let landing = agent::begin_landing(state, id)?;
    let session = state.db.agent(id)?.context("The session is gone.")?;
    let specs = match args.get("prs") {
        None => None,
        Some(prs) => Some(
            serde_json::from_value::<Vec<crate::pr_plan::PrSpec>>(prs.clone())
                .context("`prs` must be a list of {commit, pr, branch} objects")?,
        ),
    };
    // A fixing run was started to push to its PR, and nobody is there to
    // approve it.
    if let Some(number) = session.pr.filter(|_| session.kind != SessionKind::AutoFix) {
        let key = PrKey {
            repo: session.repo.clone(),
            number,
        };
        let pr = state.db.pr(&key).ok().flatten();
        // Replacing the PR's commits force-pushes its branch, which the
        // user may not own, so the card says so.
        let input = match agent::adopts_source(&session, specs.as_deref()) {
            true => json!({
                "pr": number,
                "replaces": format!("#{number}'s commits"),
                "branch": pr.as_ref().map(|pr| &pr.head_ref),
                "base": pr.as_ref().map(|pr| &pr.base_ref),
            }),
            false => json!({
                "pr": number,
                "branch": session.branch,
                "base": pr.as_ref().map(|pr| &pr.head_ref),
                "stack": format!("Joins #{number}'s GitHub stack when #{number} is its top or in no stack."),
            }),
        };
        let allowed = matches!(
            agent::await_approval(state, id, tool_use, "update_prs", &input, Vec::new()).await,
            Some(Decision { allow: true, .. })
        );
        if !allowed {
            bail!("The user didn't allow opening or updating pull requests onto #{number}.");
        }
    }
    let (landed, notes) =
        agent::land_now(state, id, WrapUpTarget::PullRequest, specs, &landing).await?;
    let mut out = landed
        .prs()
        .iter()
        .map(|pr| format!("#{} {} (branch `{}`)", pr.number, pr.url, pr.branch))
        .collect::<Vec<_>>()
        .join("\n");
    for note in notes {
        out += &format!("\n{note}");
    }
    Ok(out)
}

/// Takes over the pull requests `args.prs` names, asking the user first
/// when that takes them from other sessions.
async fn manage_prs(
    state: &Arc<State>,
    id: &str,
    tool_use: &str,
    args: &Value,
) -> anyhow::Result<String> {
    let named: Vec<u64> = serde_json::from_value(args["prs"].clone())
        .context("`prs` must be a list of pull request numbers")?;
    // Taking PRs while the session lands would change what it lands.
    let _landing = agent::begin_landing(state, id)?;
    let session = state.db.agent(id)?.context("The session is gone.")?;
    let plan = agent::plan_takeover(state, &session, &named)?;
    if let Some(question) = plan.question() {
        let input = json!({
            "take": question,
            "prs": plan.added,
            "manages": plan.prs.iter().map(|p| p.number).collect::<Vec<_>>(),
        });
        let allowed = matches!(
            agent::await_approval(state, id, tool_use, "manage_prs", &input, Vec::new()).await,
            Some(Decision { allow: true, .. })
        );
        if !allowed {
            bail!("The user didn't allow taking over the pull requests.");
        }
        // Another session may have landed while the user decided.
        let session = state.db.agent(id)?.context("The session is gone.")?;
        let again = agent::plan_takeover(state, &session, &named)?;
        if again.from != plan.from {
            bail!(
                "Which sessions manage these pull requests changed meanwhile; call manage_prs again."
            );
        }
        return agent::take_over(state, &session, again).await;
    }
    agent::take_over(state, &session, plan).await
}

/// The mode a Devin turn bypasses permissions in, where a handoff starts
/// without asking.
const BYPASS: &str = "bypass";

/// Starts the new session `args` proposes, handed off from `session`,
/// once the user agrees to it or to their edit of it.
async fn handoff(
    state: &Arc<State>,
    session: &AgentSession,
    tool_use: &str,
    args: &Value,
) -> anyhow::Result<String> {
    if session.kind.unattended() {
        bail!("{}", agent::UNATTENDED_DENIAL);
    }
    let prompt = args["prompt"].as_str().unwrap_or_default();
    if prompt.trim().is_empty() {
        bail!("`prompt` is empty");
    }
    let repo = match &args["repo"] {
        Value::Null => session.repo.clone(),
        v => v.as_str().context("`repo` must be a string")?.to_owned(),
    };
    let pr = match &args["pr"] {
        Value::Null => None,
        v => Some(v.as_u64().context("`pr` must be a pull request number")?),
    };
    // Before the user is asked, so a proposal that can't start isn't
    // shown.
    agent::check_start(state, &repo, pr)?;
    let backend = session.backend;
    let (turn_mode, model) = agent::turn_settings(state, &session.id)
        .unwrap_or_else(|| (PermissionMode::new(backend.default_mode()), None));
    let permission_mode = match &args["permission_mode"] {
        Value::Null => turn_mode.clone(),
        v => {
            let mode = v.as_str().unwrap_or_default();
            if !backend.has_mode(mode) {
                let modes: Vec<_> = backend.modes().iter().map(|(m, _)| *m).collect();
                bail!("`permission_mode` must be one of {}", modes.join(", "));
            }
            PermissionMode::new(mode)
        }
    };
    let mut req = NewAgent {
        backend,
        repo,
        pr,
        prompt: prompt.to_owned(),
        permission_mode,
        model,
        images: Vec::new(),
        draft: None,
    };
    let bypassed = backend == AgentBackend::Devin && turn_mode.as_str() == BYPASS;
    if !bypassed {
        let input = serde_json::to_value(&req)?;
        match agent::await_approval(state, &session.id, tool_use, "handoff", &input, Vec::new())
            .await
        {
            Some(Decision {
                allow: true,
                handoff,
                ..
            }) => {
                if let Some(edited) = handoff {
                    req = edited;
                }
            }
            Some(Decision {
                allow: false,
                message,
                ..
            }) => {
                let why = message.filter(|m| !m.trim().is_empty());
                match why {
                    Some(why) => bail!("The user declined the handoff: {why}"),
                    None => bail!("The user declined the handoff."),
                }
            }
            None => bail!("Nobody answered the handoff request."),
        }
    }
    let mode = req.permission_mode.clone();
    let child = agent::create_session(
        state,
        req,
        SessionKind::Interactive,
        None,
        Some(&session.id),
    )
    .await?;
    agent::emit(
        state,
        &session.id,
        AgentEventKind::HandedOff {
            id: child.id.clone(),
            title: child.title.clone(),
        },
    )?;
    let place = match child.pr {
        Some(n) => format!("{}#{n}", child.repo),
        None if child.repo == GENERAL => "no repository".to_owned(),
        None => format!("{} trunk", child.repo),
    };
    Ok(format!(
        "Started session {} \"{}\" from {place}, with {} in {} mode.",
        child.id,
        child.title,
        child.backend.agent_name(),
        child
            .backend
            .mode_label(mode.as_str())
            .unwrap_or(mode.as_str()),
    ))
}

/// Rebases the session's commits onto the user's checkout and
/// fast-forwards it, as the page's Land on checkout wrap-up does.
async fn land_on_checkout(state: &Arc<State>, id: &str) -> anyhow::Result<String> {
    let landing = agent::begin_landing(state, id)?;
    match agent::land_now(state, id, WrapUpTarget::Checkout, None, &landing).await? {
        (Landed::Checkout { sha }, _) => Ok(format!("Landed on the checkout at {sha}.")),
        _ => bail!("the session didn't land on the checkout"),
    }
}

/// The pull request the session started from, as JSON: its detail, the
/// range of its own commits, the session's own range, and its stack.
async fn session_pr(state: &Arc<State>, session: &AgentSession) -> anyhow::Result<String> {
    let Some(ctx) = agent::session_pr(state, session).await else {
        return Ok("This session didn't start from a pull request.".into());
    };
    let mut out = json!({
        "number": ctx.number,
        "base_sha": ctx.base_sha,
        "merge_base": ctx.merge_base,
        "pr_range": ctx
            .merge_base
            .as_ref()
            .map(|m| format!("{m}..{}", ctx.base_sha)),
        "session_range": format!("{}..HEAD", ctx.base_sha),
    });
    if let Some(detail) = &ctx.detail {
        out["title"] = json!(detail.summary.title);
        out["url"] = json!(detail.summary.url);
        out["state"] = json!(detail.summary.state);
        out["base_ref"] = json!(detail.base_ref);
        out["head_ref"] = json!(detail.head_ref);
        out["head"] = json!(
            detail
                .versions
                .iter()
                .rev()
                .find(|v| !v.missing)
                .map(|v| v.sha.as_str())
        );
        let managed = agent::landed_prs(session);
        if !managed.is_empty() {
            out["managed"] = json!(
                managed
                    .iter()
                    .map(|p| json!({"number": p.number, "url": p.url, "branch": p.branch}))
                    .collect::<Vec<_>>()
            );
        }
        if let Some(stack) = &detail.stack {
            out["stack"] = json!({
                "base_ref": stack.base_ref,
                "prs": stack
                    .prs
                    .iter()
                    .map(|p| json!({
                        "number": p.number,
                        "title": p.title,
                        "state": p.state,
                        "parent": p.parent,
                    }))
                    .collect::<Vec<_>>(),
            });
        }
    }
    Ok(serde_json::to_string_pretty(&out)?)
}

/// Runs one of the tools that read the session's terminals.
async fn terminal_tool(
    state: &State,
    id: &str,
    name: &str,
    args: &Value,
    backend: AgentBackend,
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
            let timeout = wait_timeout(&args["timeout_seconds"], backend);
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
    use grenadine_core::api::{AgentSession, AgentStatus, SessionKind, Stack};

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
        state_with(|_| {})
    }

    /// A state with the session `f` leaves of the fixture session.
    fn state_with(f: impl FnOnce(&mut AgentSession)) -> Arc<State> {
        let state = crate::sync::test_state();
        let mut session = AgentSession {
            title: "t".into(),
            branch: "b".into(),
            worktree: "/w".into(),
            base_sha: "s".into(),
            status: AgentStatus::Running,
            ..AgentSession::fixture(ID, "o/n")
        };
        f(&mut session);
        state.db.create_agent(&session).unwrap();
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
                "show_image",
                "update_prs",
                "land_on_checkout",
                "session_pr",
                "manage_prs",
                "handoff"
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

    /// `wait_for_terminal`'s `timeout_seconds` description states the
    /// default and the cap the session's backend allows.
    #[tokio::test]
    async fn tools_list_describes_the_backends_timeout() {
        for (backend, expected) in [
            (AgentBackend::Claude, "Defaults to 600, at most 3600."),
            (
                AgentBackend::Devin,
                "Defaults to 100, at most 100. Call the tool again to keep waiting.",
            ),
        ] {
            let state = state_with(|s| s.backend = backend);
            let secret = state.agents.secret(ID);
            let (_, body) = call(
                &state,
                &secret,
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            )
            .await;
            let description = body["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == "wait_for_terminal")
                .unwrap()["inputSchema"]["properties"]["timeout_seconds"]["description"]
                .as_str()
                .unwrap();
            assert_eq!(description, expected, "{backend:?}");
        }
    }

    /// `timeout_seconds` defaults to `WAIT` and is capped at what the
    /// session's backend gives a tool call.
    #[test]
    fn the_wait_timeout_fits_the_backend() {
        for (backend, cases) in [
            (
                AgentBackend::Claude,
                [(None, 600), (Some(900), 900), (Some(5000), 3600)],
            ),
            (
                AgentBackend::Devin,
                [(None, 100), (Some(900), 100), (Some(30), 30)],
            ),
        ] {
            for (value, expected) in cases {
                let value = value.map_or(Value::Null, |s| json!(s));
                assert_eq!(
                    wait_timeout(&value, backend),
                    Duration::from_secs(expected),
                    "{backend:?} {value}"
                );
            }
        }
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
        for _ in 0..1500 {
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
                    options: Vec::new(),
                },
                AgentEventKind::ApprovalResolved {
                    id: "toolu_1".into(),
                    allow: true,
                    message: None,
                    answers: None,
                    expired: false,
                },
                AgentEventKind::ApprovalRequested {
                    id: "toolu_2".into(),
                    tool_name: "Write".into(),
                    input: r#"{"content":"hello\n","file_path":"/w/b.txt"}"#.into(),
                    options: Vec::new(),
                },
                AgentEventKind::ApprovalResolved {
                    id: "toolu_2".into(),
                    allow: false,
                    message: None,
                    answers: None,
                    expired: false,
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

    /// The notes a plan is approved with reach Claude as part of the plan.
    #[tokio::test]
    async fn plans_approved_with_notes() {
        let state = state();
        let plan = json!({"plan": "# Plan", "planFilePath": "/p.md"});
        let notes = Decision {
            allow: true,
            message: Some(" Use tabs. \n".into()),
            ..Decision::default()
        };
        let answer = ask_with(&state, "toolu_1", "ExitPlanMode", plan.clone(), notes).await;
        assert_eq!(
            answer["updatedInput"],
            json!({
                "plan": "# Plan\n\n---\n\nThe user approved this plan, asking you to keep \
                    the following in mind:\n\nUse tabs.",
                "planFilePath": "/p.md",
            })
        );
        for (tool_use, message) in [("toolu_2", None), ("toolu_3", Some("  ".into()))] {
            let silent = Decision {
                allow: true,
                message,
                ..Decision::default()
            };
            let answer = ask_with(&state, tool_use, "ExitPlanMode", plan.clone(), silent).await;
            assert_eq!(answer["updatedInput"], plan);
        }
    }

    /// A state whose session `ID`, `f` applied, is in no repository, with
    /// backends that aren't there: the sessions it hands off to start in
    /// no repository and their turns fail without running anything.
    fn handoff_state(dir: &std::path::Path, f: impl FnOnce(&mut AgentSession)) -> Arc<State> {
        let state = crate::sync::test_state_with_agents(
            BTreeMap::new(),
            dir.join("claude"),
            dir.join("devin"),
            dir.join("worktrees"),
        );
        let mut session = AgentSession {
            title: "parent".into(),
            status: AgentStatus::Running,
            ..AgentSession::fixture(ID, GENERAL)
        };
        f(&mut session);
        state.db.create_agent(&session).unwrap();
        state
    }

    /// The sessions other than `ID`.
    fn handed_off(state: &State) -> Vec<AgentSession> {
        let mut sessions = state.db.agents().unwrap();
        sessions.retain(|s| s.id != ID);
        sessions
    }

    /// The `HandedOff` events in `ID`'s transcript.
    fn handed_off_events(state: &State) -> Vec<(String, String)> {
        state
            .db
            .agent_events(ID)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.kind {
                AgentEventKind::HandedOff { id, title } => Some((id, title)),
                _ => None,
            })
            .collect()
    }

    /// A handoff proposes a session with the turn's mode and model, and
    /// starts what the user made of it, linked both ways.
    #[tokio::test]
    async fn handoff_starts_the_users_edit() {
        let dir = tempfile::tempdir().unwrap();
        let state = handoff_state(dir.path(), |_| {});
        agent::set_turn_settings(
            &state,
            ID,
            (PermissionMode::from("acceptEdits"), Some("opus".into())),
        );
        let task = tokio::spawn({
            let state = state.clone();
            async move { tool(&state, "handoff", json!({"prompt": "Do the follow-up"})).await }
        });
        let ids = requested_ids(&state, 1).await;
        let input = state
            .db
            .agent_events(ID)
            .unwrap()
            .into_iter()
            .find_map(|e| match e.kind {
                AgentEventKind::ApprovalRequested {
                    tool_name, input, ..
                } if tool_name == "handoff" => Some(input),
                _ => None,
            })
            .unwrap();
        let proposal: NewAgent = serde_json::from_str(&input).unwrap();
        assert_eq!(
            proposal,
            NewAgent {
                backend: AgentBackend::Claude,
                repo: GENERAL.into(),
                pr: None,
                prompt: "Do the follow-up".into(),
                permission_mode: PermissionMode::from("acceptEdits"),
                model: Some("opus".into()),
                images: Vec::new(),
                draft: None,
            }
        );
        let edited = NewAgent {
            prompt: "Do it differently\nin detail".into(),
            permission_mode: PermissionMode::plan(),
            ..proposal
        };
        let decision = Decision {
            allow: true,
            handoff: Some(edited),
            ..Decision::default()
        };
        assert!(agent::approve(&state, ID, &ids[0], decision).unwrap());
        let (text, failed) = task.await.unwrap();
        assert!(!failed, "{text}");
        let children = handed_off(&state);
        assert_eq!(children.len(), 1, "{children:?}");
        let child = &children[0];
        assert_eq!(child.title, "Do it differently");
        assert_eq!(child.handed_off_from.as_deref(), Some(ID));
        assert_eq!(
            text,
            format!(
                "Started session {} \"Do it differently\" from no repository, with Claude in \
                 Plan first mode.",
                child.id
            )
        );
        assert_eq!(
            handed_off_events(&state),
            [(child.id.clone(), "Do it differently".to_owned())]
        );
    }

    /// A declined handoff starts nothing and tells the agent why.
    #[tokio::test]
    async fn handoff_declined() {
        let dir = tempfile::tempdir().unwrap();
        let state = handoff_state(dir.path(), |_| {});
        for (message, told) in [
            (
                Some("Not now".to_owned()),
                "The user declined the handoff: Not now",
            ),
            (None, "The user declined the handoff."),
        ] {
            let task = tokio::spawn({
                let state = state.clone();
                async move { tool(&state, "handoff", json!({"prompt": "x"})).await }
            });
            let n = if message.is_some() { 1 } else { 2 };
            let ids = requested_ids(&state, n).await;
            let decision = Decision {
                allow: false,
                message,
                ..Decision::default()
            };
            assert!(agent::approve(&state, ID, &ids[n - 1], decision).unwrap());
            assert_eq!(task.await.unwrap(), (told.to_owned(), true));
        }
        assert!(handed_off(&state).is_empty());
        assert!(handed_off_events(&state).is_empty());
    }

    /// A Devin turn that bypasses permissions hands off without asking.
    #[tokio::test]
    async fn handoff_without_asking_when_bypassing() {
        let dir = tempfile::tempdir().unwrap();
        let state = handoff_state(dir.path(), |s| s.backend = AgentBackend::Devin);
        agent::set_turn_settings(&state, ID, (PermissionMode::from(BYPASS), None));
        let (text, failed) = tool(
            &state,
            "handoff",
            json!({"prompt": "x", "permission_mode": "smart"}),
        )
        .await;
        assert!(!failed, "{text}");
        assert!(text.ends_with("with Devin in Smart mode."), "{text}");
        let children = handed_off(&state);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].backend, AgentBackend::Devin);
        assert!(requested_ids_now(&state).is_empty());
    }

    /// The approval requests in `ID`'s transcript so far.
    fn requested_ids_now(state: &State) -> Vec<String> {
        state
            .db
            .agent_events(ID)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.kind {
                AgentEventKind::ApprovalRequested { id, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    /// A handoff that can't start fails before the user is asked, and an
    /// unattended review can't hand off at all.
    #[tokio::test]
    async fn handoff_refuses_what_cant_start() {
        let dir = tempfile::tempdir().unwrap();
        let state = handoff_state(dir.path(), |_| {});
        for (args, error) in [
            (json!({"prompt": " "}), "`prompt` is empty"),
            (
                json!({"prompt": "x", "repo": "o/elsewhere"}),
                "o/elsewhere is not a configured repository",
            ),
            (
                json!({"prompt": "x", "pr": 7}),
                "a general session can't start from a PR",
            ),
            (
                json!({"prompt": "x", "permission_mode": "bypass"}),
                "`permission_mode` must be one of manual, acceptEdits, auto, plan",
            ),
        ] {
            assert_eq!(
                tool(&state, "handoff", args).await,
                (error.to_owned(), true)
            );
        }
        assert!(requested_ids_now(&state).is_empty());

        let state = handoff_state(dir.path(), |s| s.kind = SessionKind::AutoReview);
        let (text, failed) = tool(&state, "handoff", json!({"prompt": "x"})).await;
        assert!(failed);
        assert_eq!(text, agent::UNATTENDED_DENIAL);
        assert!(handed_off(&state).is_empty());
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
        // The login shell is still starting up when the terminal opens,
        // and a command typed before its first prompt shares that
        // prompt's line.
        for _ in 0..1500 {
            if terminal.waiting_at_prompt() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(terminal.waiting_at_prompt(), "the shell never prompted");
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
        let err = terminal_tool(
            &state,
            other,
            "read_terminal",
            &json!({"terminal": id}),
            AgentBackend::Claude,
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), format!("Terminal {id} isn't open."));

        state.agents.terminals.close_all(ID).await;
    }

    /// A session started from a PR asks before its pull requests stack on
    /// that PR's branch; a denied answer stops the push.
    #[tokio::test]
    async fn update_prs_asks_before_stacking_on_a_pr() {
        let state = state_with(|s| s.pr = Some(24));
        let call_task = tokio::spawn({
            let state = state.clone();
            async move { tool(&state, "update_prs", json!({})).await }
        });
        let mut requested = None;
        for _ in 0..1500 {
            requested = state
                .db
                .agent_events(ID)
                .unwrap()
                .iter()
                .find_map(|e| match &e.kind {
                    AgentEventKind::ApprovalRequested {
                        id,
                        tool_name,
                        input,
                        ..
                    } => Some((id.clone(), tool_name.clone(), input.clone())),
                    _ => None,
                });
            if requested.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (tool_use, tool_name, input) = requested.expect("no approval was requested");
        assert_eq!(tool_name, "update_prs");
        assert!(input.contains("\"branch\":\"b\""), "{input}");
        assert!(input.contains("\"pr\":24"), "{input}");
        // The PR isn't synced in this state, so its branch is unknown.
        assert!(input.contains("\"base\":null"), "{input}");

        let decision = Decision {
            allow: false,
            ..Decision::default()
        };
        assert!(agent::approve(&state, ID, &tool_use, decision).unwrap());
        let (text, failed) = call_task.await.unwrap();
        assert!(failed, "{text}");
        assert_eq!(
            text,
            "The user didn't allow opening or updating pull requests onto #24."
        );
        assert_eq!(state.db.agent(ID).unwrap().unwrap().landed, None);
    }

    /// Records PR `number` of `o/n` on `branch` at `head` in `stack`.
    fn store(state: &Arc<State>, number: u64, branch: &str, head: &str, stack: &Stack) {
        state
            .db
            .store_sync(
                &crate::db::PrMeta {
                    key: PrKey {
                        repo: "o/n".into(),
                        number,
                    },
                    title: "t".into(),
                    body: String::new(),
                    author: "me".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                    queued: false,
                    url: format!("https://github.com/o/n/pull/{number}"),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    base_ref: "main".into(),
                    head_ref: branch.into(),
                    head_oid: head.into(),
                    ci: None,
                    review: None,
                    review_error: None,
                },
                &[grenadine_core::api::Version {
                    number: 1,
                    sha: head.into(),
                    merge_base: None,
                    kind: grenadine_core::api::VersionKind::Initial,
                    pushed_at: None,
                    pushed_by: None,
                    pushed_by_is_guess: false,
                    missing: false,
                }],
                &[],
                Some(stack),
                false,
                None,
            )
            .unwrap();
    }

    /// Taking PRs that another session manages shows a card naming them
    /// and the session; refusing it leaves them with that session
    /// (AGENT-STACK-XFER-3).
    #[tokio::test]
    async fn manage_prs_asks_before_taking_another_sessions_prs() {
        let state = state_with(|s| s.pr = Some(40));
        let pr = |number: u64, parent: Option<u64>| grenadine_core::api::StackPr {
            number,
            title: "t".into(),
            state: "OPEN".into(),
            is_draft: false,
            url: String::new(),
            updated_at: String::new(),
            head_oid: format!("{number:040}"),
            parent,
            queued: false,
        };
        let stack = Stack {
            base_ref: "main".into(),
            prs: vec![pr(40, None), pr(41, Some(40)), pr(42, Some(41))],
            more_ancestors: false,
            more_descendants: false,
        };
        for (n, branch) in [(40, "a"), (41, "b"), (42, "c")] {
            store(&state, n, branch, &format!("{n:040}"), &stack);
        }
        let other = "22222222-2222-4333-8444-555555555555";
        state
            .db
            .create_agent(&AgentSession {
                branch: "o".into(),
                ..AgentSession::fixture(other, "o/n")
            })
            .unwrap();
        let theirs = |number: u64, branch: &str| grenadine_core::api::LandedPr {
            number,
            url: "u".into(),
            branch: branch.into(),
            lease: None,
        };
        let landed = Landed::from_prs(vec![theirs(41, "b"), theirs(42, "c")]);
        state.db.set_agent_landed(other, &landed).unwrap();

        let call_task = tokio::spawn({
            let state = state.clone();
            async move { tool(&state, "manage_prs", json!({"prs": [41, 42]})).await }
        });
        let ids = requested_ids(&state, 1).await;
        let input = state
            .db
            .agent_events(ID)
            .unwrap()
            .into_iter()
            .find_map(|e| match e.kind {
                AgentEventKind::ApprovalRequested {
                    tool_name, input, ..
                } if tool_name == "manage_prs" => Some(input),
                _ => None,
            })
            .unwrap();
        assert!(
            input.contains("Take #41, #42 from session 22222222?"),
            "{input}"
        );
        let decision = Decision {
            allow: false,
            ..Decision::default()
        };
        assert!(agent::approve(&state, ID, &ids[0], decision).unwrap());
        let (text, failed) = call_task.await.unwrap();
        assert!(failed, "{text}");
        assert_eq!(text, "The user didn't allow taking over the pull requests.");
        assert_eq!(state.db.agent(other).unwrap().unwrap().landed, Some(landed));
        assert_eq!(state.db.agent(ID).unwrap().unwrap().landed, None);
    }

    /// The ids of the requests in the transcript so far, waiting until
    /// there are `n`.
    async fn requested_ids(state: &Arc<State>, n: usize) -> Vec<String> {
        for _ in 0..1500 {
            let ids: Vec<_> = state
                .db
                .agent_events(ID)
                .unwrap()
                .into_iter()
                .filter_map(|e| match e.kind {
                    AgentEventKind::ApprovalRequested { id, .. } => Some(id),
                    _ => None,
                })
                .collect();
            if ids.len() >= n {
                return ids;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("fewer than {n} approvals were requested");
    }

    /// JSON-RPC ids start over when Claude Code reconnects, so the same
    /// id from a later connection still gets a request of its own, which
    /// an earlier answer doesn't resolve.
    #[tokio::test]
    async fn update_prs_requests_get_fresh_ids() {
        let state = state_with(|s| s.pr = Some(24));
        for n in 1..=2 {
            let call_task = tokio::spawn({
                let state = state.clone();
                // `tool` sends the same JSON-RPC id every time.
                async move { tool(&state, "update_prs", json!({})).await }
            });
            let ids = requested_ids(&state, n).await;
            let decision = Decision {
                allow: false,
                ..Decision::default()
            };
            assert!(agent::approve(&state, ID, &ids[n - 1], decision).unwrap());
            let (text, failed) = call_task.await.unwrap();
            assert!(failed, "{text}");
        }
        let ids = requested_ids(&state, 2).await;
        assert_ne!(ids[0], ids[1]);
        assert!(ids.iter().all(|i| i.starts_with("update_prs-")), "{ids:?}");
    }

    /// A request whose waiter hangs up expires, so the page stops offering
    /// answers nobody can take, and the session is running again. A plan
    /// stays open, since answering it starts a new turn.
    #[tokio::test]
    async fn a_dropped_request_expires_unless_its_a_plan() {
        let state = state();
        let secret = state.agents.secret(ID);
        for (n, (tool_use, tool_name)) in [("toolu_1", "Write"), ("toolu_2", "ExitPlanMode")]
            .into_iter()
            .enumerate()
        {
            let request = json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "approve",
                    "arguments": {"tool_name": tool_name, "input": {}, "tool_use_id": tool_use},
                },
            });
            let task = tokio::spawn({
                let (state, secret) = (state.clone(), secret.clone());
                async move { call(&state, &secret, request).await }
            });
            requested_ids(&state, n + 1).await;
            assert_eq!(
                state.db.agent(ID).unwrap().unwrap().status,
                AgentStatus::AwaitingApproval
            );
            // Claude Code hangs up.
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(!agent::approve(&state, ID, tool_use, Decision::default()).unwrap());
        }
        let resolved: Vec<_> = state
            .db
            .agent_events(ID)
            .unwrap()
            .into_iter()
            .filter(|e| matches!(e.kind, AgentEventKind::ApprovalResolved { .. }))
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            resolved,
            [AgentEventKind::ApprovalResolved {
                id: "toolu_1".into(),
                allow: false,
                message: None,
                answers: None,
                expired: true,
            }]
        );
        // The plan's request left the session waiting on it, as a turn
        // that ends on a plan does.
        assert_eq!(
            state.db.agent(ID).unwrap().unwrap().status,
            AgentStatus::AwaitingApproval
        );
    }

    /// After a restart, the requests the transcript leaves unresolved
    /// expire, except plans. A resolution closes the latest request with
    /// its id, as ids could repeat before they were random.
    #[tokio::test]
    async fn a_restart_expires_the_requests_left_open() {
        let state = state();
        let requested = |id: &str, tool_name: &str| AgentEventKind::ApprovalRequested {
            id: id.into(),
            tool_name: tool_name.into(),
            input: "{}".into(),
            options: Vec::new(),
        };
        let resolved = |id: &str, expired: bool| AgentEventKind::ApprovalResolved {
            id: id.into(),
            allow: !expired,
            message: None,
            answers: None,
            expired,
        };
        for kind in [
            requested("answered", "Bash"),
            resolved("answered", false),
            requested("repeated", "update_prs"),
            resolved("repeated", false),
            AgentEventKind::Text("hi".into()),
            requested("repeated", "update_prs"),
            requested("plan", "ExitPlanMode"),
        ] {
            state.db.append_agent_event(ID, &kind, 0).unwrap();
        }
        agent::expire_orphaned_approvals(&state).unwrap();
        let after: Vec<_> = state
            .db
            .agent_events(ID)
            .unwrap()
            .into_iter()
            .skip(7)
            .map(|e| e.kind)
            .collect();
        assert_eq!(after, [resolved("repeated", true)]);
        // A second restart finds nothing left to expire.
        agent::expire_orphaned_approvals(&state).unwrap();
        assert_eq!(state.db.agent_events(ID).unwrap().len(), 8);
    }

    /// Mapping a commit to the session's source PR asks to replace that
    /// PR's commits rather than to stack on it.
    #[tokio::test]
    async fn update_prs_asks_before_replacing_a_prs_commits() {
        let state = state_with(|s| s.pr = Some(24));
        let call_task = tokio::spawn({
            let state = state.clone();
            async move {
                tool(
                    &state,
                    "update_prs",
                    json!({"prs": [{"commit": "abcdef0", "pr": 24}]}),
                )
                .await
            }
        });
        let mut requested = None;
        for _ in 0..1500 {
            requested = state
                .db
                .agent_events(ID)
                .unwrap()
                .iter()
                .find_map(|e| match &e.kind {
                    AgentEventKind::ApprovalRequested { id, input, .. } => {
                        Some((id.clone(), input.clone()))
                    }
                    _ => None,
                });
            if requested.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (tool_use, input) = requested.expect("no approval was requested");
        assert!(input.contains("\"replaces\":\"#24's commits\""), "{input}");
        assert!(!input.contains("\"stack\""), "{input}");

        let decision = Decision {
            allow: false,
            ..Decision::default()
        };
        assert!(agent::approve(&state, ID, &tool_use, decision).unwrap());
        let (_, failed) = call_task.await.unwrap();
        assert!(failed);
    }

    /// Without a PR to push onto, `update_prs` doesn't ask; it fails here
    /// on the unconfigured repo, and reviews can't land either way.
    #[tokio::test]
    async fn landing_tools_fail_cleanly() {
        let state = state();
        let (text, failed) = tool(&state, "update_prs", json!({})).await;
        assert!(failed, "{text}");
        assert!(text.contains("is not a configured repository"), "{text}");
        let asked = state
            .db
            .agent_events(ID)
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, AgentEventKind::ApprovalRequested { .. }));
        assert!(!asked);

        let state = state_with(|s| s.kind = SessionKind::AutoReview);
        for name in ["update_prs", "land_on_checkout"] {
            let (text, failed) = tool(&state, name, json!({})).await;
            assert!(failed && text.contains("review"), "{name}: {text}");
        }
    }

    /// A fixing run was started to push to its PR, so `update_prs` goes
    /// on without asking, which nobody could answer; it fails here on
    /// the unconfigured repo. It can't land on the checkout or hand off.
    #[tokio::test]
    async fn a_fixing_run_pushes_without_asking() {
        let state = state_with(|s| {
            s.kind = SessionKind::AutoFix;
            s.pr = Some(24);
        });
        let (text, failed) = tool(&state, "update_prs", json!({})).await;
        assert!(failed, "{text}");
        assert!(!text.contains("didn't allow"), "{text}");
        let asked = state
            .db
            .agent_events(ID)
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, AgentEventKind::ApprovalRequested { .. }));
        assert!(!asked);
        let (text, failed) = tool(&state, "land_on_checkout", json!({})).await;
        assert!(
            failed && text.contains("only pushes to its pull requests"),
            "{text}"
        );
        let (text, failed) = tool(&state, "handoff", json!({"prompt": "Do it"})).await;
        assert!(failed && text == agent::UNATTENDED_DENIAL, "{text}");
    }

    /// A landing call that comes again while the first still runs, as
    /// Devin's MCP client resends them, is refused.
    #[tokio::test]
    async fn landing_tools_refuse_a_second_call() {
        let state = state_with(|s| s.pr = Some(24));
        let _landing = agent::begin_landing(&state, ID).unwrap();
        for name in ["update_prs", "land_on_checkout"] {
            let (text, failed) = tool(&state, name, json!({})).await;
            assert!(
                failed && text.contains("landing this session's work already"),
                "{name}: {text}"
            );
        }
        // The refused update_prs call showed no approval card.
        let asked = state
            .db
            .agent_events(ID)
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, AgentEventKind::ApprovalRequested { .. }));
        assert!(!asked);
    }

    /// The answers Claude Code 2.1 takes, as tried with a real `claude -p`.
    #[test]
    fn allowed_answers() {
        let plan = json!({"plan": "# Plan", "planFilePath": "/p.md"});
        assert_eq!(
            allowed(&plan, Some("acceptEdits".into()), None),
            json!({
                "behavior": "allow",
                "updatedInput": plan,
                "updatedPermissions": [
                    {"type": "setMode", "mode": "acceptEdits", "destination": "session"},
                ],
            })
        );
        let questions = json!({"questions": [{"question": "Tea?", "options": []}]});
        let answers = BTreeMap::from([("Tea?".to_owned(), vec!["Yes".to_owned()])]);
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
        let answers = BTreeMap::from([(
            "Tea?".to_owned(),
            vec!["Tea".to_owned(), "Coffee".to_owned()],
        )]);
        assert_eq!(
            allowed(&questions, None, Some(answers))["updatedInput"]["answers"],
            json!({"Tea?": "Tea, Coffee"})
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

    /// `session_pr` describes the PR the session started from, with the
    /// range of its own commits and its stack; a session without one
    /// gets a plain answer.
    #[tokio::test]
    async fn session_pr_describes_the_starting_pr() {
        use grenadine_core::api::{Stack, StackPr, Version, VersionKind};
        let base = "b".repeat(40);
        let merge_base = "a".repeat(40);
        let no_pr = state();
        let state = state_with(|s| {
            s.pr = Some(7);
            s.base_sha = base.clone();
        });
        let stack_pr = |number: u64, title: &str, parent: Option<u64>, head_oid: String| StackPr {
            number,
            title: title.into(),
            state: "OPEN".into(),
            is_draft: false,
            url: "u".into(),
            updated_at: "t".into(),
            head_oid,
            parent,
            queued: false,
        };
        state
            .db
            .store_sync(
                &crate::db::PrMeta {
                    key: PrKey {
                        repo: "o/n".into(),
                        number: 7,
                    },
                    title: "The PR".into(),
                    body: String::new(),
                    author: "me".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                    queued: false,
                    url: "https://github.com/o/n/pull/7".into(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    base_ref: "parent-branch".into(),
                    head_ref: "feature".into(),
                    head_oid: base.clone(),
                    ci: None,
                    review: None,
                    review_error: None,
                },
                &[Version {
                    number: 1,
                    sha: base.clone(),
                    merge_base: Some(merge_base.clone()),
                    kind: VersionKind::Initial,
                    pushed_at: None,
                    pushed_by: None,
                    pushed_by_is_guess: false,
                    missing: false,
                }],
                &[],
                Some(&Stack {
                    base_ref: "main".into(),
                    prs: vec![
                        stack_pr(6, "Parent", None, "h".into()),
                        stack_pr(7, "The PR", Some(6), base.clone()),
                    ],
                    more_ancestors: false,
                    more_descendants: false,
                }),
                false,
                None,
            )
            .unwrap();

        let (text, failed) = tool(&state, "session_pr", json!({})).await;
        assert!(!failed, "{text}");
        let out: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(out["number"], 7);
        assert_eq!(out["title"], "The PR");
        assert_eq!(out["url"], "https://github.com/o/n/pull/7");
        assert_eq!(out["state"], "OPEN");
        assert_eq!(out["base_ref"], "parent-branch");
        assert_eq!(out["head_ref"], "feature");
        assert_eq!(out["head"], base);
        assert_eq!(out["merge_base"], merge_base);
        assert_eq!(out["pr_range"], format!("{merge_base}..{base}"));
        assert_eq!(out["session_range"], format!("{base}..HEAD"));
        assert_eq!(out["stack"]["prs"][0]["number"], 6);
        assert_eq!(out["stack"]["prs"][1]["parent"], 6);

        let answer = tool(&no_pr, "session_pr", json!({})).await;
        assert_eq!(
            answer,
            (
                "This session didn't start from a pull request.".into(),
                false
            )
        );
    }
}
