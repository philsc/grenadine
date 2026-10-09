//! Claude Code sessions: the list with a form that starts one, and a
//! session's page with its live transcript, permission requests and a box
//! for follow-up prompts.

use std::collections::HashMap;

use grenadine_core::api::{
    AgentEvent, AgentEventKind, AgentSession, AgentStatus, Approval, NewAgent, PrKey,
};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;

use crate::{Updates, api, hash_for, markdown};

fn agent_hash(id: &str) -> String {
    format!("#/agents/{id}")
}

fn go(hash: &str) {
    let _ = window().location().set_hash(hash);
}

fn status_label(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Idle => "idle",
        AgentStatus::Running => "working…",
        AgentStatus::AwaitingApproval => "needs approval",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Failed => "failed",
    }
}

fn status_class(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Idle => "agent-status",
        AgentStatus::Running => "agent-status running",
        AgentStatus::AwaitingApproval => "agent-status waiting",
        AgentStatus::Interrupted => "agent-status",
        AgentStatus::Failed => "agent-status failed",
    }
}

fn cost(usd: f64) -> String {
    format!("${usd:.2}")
}

#[component]
fn StatusBadge(status: AgentStatus) -> impl IntoView {
    view! { <span class=status_class(status)>{status_label(status)}</span> }
}

#[component]
pub fn AgentList(
    /// Fills in the form to start a session on this PR.
    pr: Option<PrKey>,
) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let sessions = LocalResource::new(move || {
        updates.agents.track();
        api::agents()
    });

    view! {
        <div class="agents">
            <NewSession pr=pr />
            <h2>"Sessions"</h2>
            {move || match sessions.get() {
                None => view! { <p class="muted">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                Some(Ok(list)) if list.is_empty() => {
                    view! { <p class="muted">"No sessions yet."</p> }.into_any()
                }
                Some(Ok(list)) => view! {
                    <ul class="agent-list">
                        {list.into_iter().map(|a| view! {
                            <li>
                                <a class="agent-item" href=agent_hash(&a.id)>
                                    <span class="agent-title">
                                        <StatusBadge status=a.status />
                                        {a.title.clone()}
                                    </span>
                                    <span class="agent-meta muted">
                                        {match a.pr {
                                            Some(n) => format!("{}#{n}", a.repo),
                                            None => a.repo.clone(),
                                        }}
                                        {format!(" · {} · {}", a.branch, cost(a.cost_usd))}
                                    </span>
                                </a>
                            </li>
                        }).collect_view()}
                    </ul>
                }.into_any(),
            }}
        </div>
    }
}

#[component]
fn NewSession(pr: Option<PrKey>) -> impl IntoView {
    let repos = LocalResource::new(api::repos);
    let repo = RwSignal::new(pr.as_ref().map(|k| k.repo.clone()).unwrap_or_default());
    let number = RwSignal::new(pr.map(|k| k.number.to_string()).unwrap_or_default());
    let prompt = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);

    // Default to the first repository once they're known.
    Effect::new(move |_| {
        if let Some(Ok(list)) = repos.get()
            && repo.with_untracked(String::is_empty)
            && let Some(first) = list.first()
        {
            repo.set(first.clone());
        }
    });

    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let pr = match number.get_untracked().trim() {
            "" => None,
            n => match n.trim_start_matches('#').parse() {
                Ok(n) => Some(n),
                Err(_) => {
                    error.set(Some("The PR must be a number.".into()));
                    return;
                }
            },
        };
        let req = NewAgent {
            repo: repo.get_untracked(),
            pr,
            prompt: prompt.get_untracked(),
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::create_agent(&req).await {
                Ok(session) => go(&agent_hash(&session.id)),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    view! {
        <form class="agent-form" on:submit=submit>
            <h2>"New session"</h2>
            <div class="agent-form-row">
                <label>"Repository"
                    <select
                        prop:value=move || repo.get()
                        on:change=move |ev| repo.set(event_target_value(&ev))
                    >
                        {move || repos.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|r| {
                            let selected = repo.get_untracked() == r;
                            view! { <option value=r.clone() selected=selected>{r.clone()}</option> }
                        }).collect_view()}
                    </select>
                </label>
                <label>"Start from PR"
                    <input
                        type="text"
                        class="agent-pr"
                        placeholder="trunk"
                        prop:value=move || number.get()
                        on:input=move |ev| number.set(event_target_value(&ev))
                    />
                </label>
            </div>
            <textarea
                class="agent-prompt"
                rows="5"
                placeholder="What should Claude do?"
                aria-label="Prompt"
                prop:value=move || prompt.get()
                on:input=move |ev| prompt.set(event_target_value(&ev))
            ></textarea>
            <p class="hint muted">"The session gets its own git worktree on a new branch. Claude asks here before it uses a tool."</p>
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
            <div class="form-buttons">
                <button
                    type="submit"
                    class="primary"
                    disabled=move || busy.get() || prompt.with(|p| p.trim().is_empty())
                >
                    {move || if busy.get() { "Creating worktree…" } else { "Start" }}
                </button>
            </div>
        </form>
    }
}

/// Follows a session's event stream while the page is open. Events arrive
/// in `seq` order; a reconnect replays the transcript, which is skipped up
/// to the last event seen.
fn follow(id: &str, events: RwSignal<Vec<AgentEvent>>, streaming: RwSignal<String>) {
    let Ok(source) = web_sys::EventSource::new(&format!("/api/agents/{id}/events")) else {
        return;
    };
    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            let Some(event) = e
                .data()
                .as_string()
                .and_then(|d| serde_json::from_str::<AgentEvent>(&d).ok())
            else {
                return;
            };
            match event.seq {
                None => {
                    if let AgentEventKind::TextDelta(t) = &event.kind {
                        streaming.update(|s| s.push_str(t));
                    }
                }
                Some(seq) => {
                    let last = events.with_untracked(|e| e.last().and_then(|e| e.seq));
                    if last.is_some_and(|l| seq <= l) {
                        return;
                    }
                    // The streamed text is complete once anything else
                    // comes along.
                    streaming.set(String::new());
                    events.update(|e| e.push(event));
                }
            }
        });
    source.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    let source = StoredValue::new_local(source);
    // The handler lives as long as the view.
    StoredValue::new_local(on_message);
    on_cleanup(move || {
        source.try_with_value(|s| s.close());
    });
}

/// A one-line summary of a tool call's input, e.g. the command or the path.
fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let field = match name {
        "Bash" => "command",
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => "file_path",
        "Grep" | "Glob" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Task" | "Agent" => "description",
        _ => "",
    };
    match input.get(field).and_then(|v| v.as_str()) {
        Some(s) => s.lines().next().unwrap_or_default().to_owned(),
        None => String::new(),
    }
}

fn pretty(json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(json)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| json.to_owned())
}

/// Where loading the session stands.
#[derive(Clone, Debug, PartialEq)]
enum Load {
    Loading,
    Failed(String),
    Missing,
    Ready,
}

#[component]
pub fn AgentView(id: String) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let session = {
        let id = id.clone();
        LocalResource::new(move || {
            updates.agents.track();
            let id = id.clone();
            async move { api::agent(&id).await }
        })
    };
    // Refetches only update the parts that show the session's state, so a
    // half-written follow-up or an open tool call survives them.
    let load = Memo::new(move |prev: Option<&Load>| match session.get() {
        None => prev.cloned().unwrap_or(Load::Loading),
        Some(Err(e)) => Load::Failed(e),
        Some(Ok(None)) => Load::Missing,
        Some(Ok(Some(_))) => Load::Ready,
    });
    let current = Memo::new(move |prev: Option<&Option<AgentSession>>| {
        session
            .get()
            .and_then(Result::ok)
            .flatten()
            .or_else(|| prev.cloned().flatten())
    });
    let events = RwSignal::new(Vec::<AgentEvent>::new());
    let streaming = RwSignal::new(String::new());
    follow(&id, events, streaming);

    view! {
        <div class="agent">
            {move || match load.get() {
                Load::Loading => view! { <p class="muted">"Loading…"</p> }.into_any(),
                Load::Failed(e) => view! { <p class="error">{e}</p> }.into_any(),
                Load::Missing => view! {
                    <p class="muted">"There is no such session. " <a href="#/agents">"See all sessions"</a></p>
                }.into_any(),
                Load::Ready => {
                    // Ready means a session was loaded, and `current` keeps it.
                    let session = Signal::derive(move || current.get().expect("a loaded session"));
                    view! {
                        <AgentHeader session=session />
                        <Transcript session=session events=events streaming=streaming />
                        <Composer session=session />
                    }.into_any()
                }
            }}
        </div>
    }
}

#[component]
fn AgentHeader(session: Signal<AgentSession>) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let delete = move |_| {
        let confirmed = window()
            .confirm_with_message(
                "Delete this session? Its worktree and branch are deleted too, with any changes that weren't pushed.",
            )
            .unwrap_or(false);
        if !confirmed {
            return;
        }
        let id = session.with_untracked(|s| s.id.clone());
        spawn_local(async move {
            match api::delete_agent(&id).await {
                Ok(()) => go("#/agents"),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <header class="agent-header">
            {move || {
                let s = session.get();
                let from = match s.pr {
                    Some(n) => {
                        let key = PrKey {
                            repo: s.repo.clone(),
                            number: n,
                        };
                        view! { <a href=hash_for(&key)>{format!("{}#{n}", s.repo)}</a> }.into_any()
                    }
                    None => view! { <span>{format!("{} trunk", s.repo)}</span> }.into_any(),
                };
                view! {
                    <h1>{s.title.clone()}</h1>
                    <p class="muted">
                        <StatusBadge status=s.status />
                        " from " {from} " at " <code>{s.base_sha.chars().take(8).collect::<String>()}</code>
                        " on " <code>{s.branch.clone()}</code>
                        {format!(" · {}", cost(s.cost_usd))}
                        <button class="link danger" on:click=delete>"Delete"</button>
                    </p>
                    <p class="muted agent-worktree"><code>{s.worktree.clone()}</code></p>
                }
            }}
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
        </header>
    }
}

#[component]
fn Transcript(
    session: Signal<AgentSession>,
    events: RwSignal<Vec<AgentEvent>>,
    streaming: RwSignal<String>,
) -> impl IntoView {
    // Tool results and approval answers are shown with the event they
    // belong to.
    let results = Memo::new(move |_| {
        events.with(|events| {
            events
                .iter()
                .filter_map(|e| match &e.kind {
                    AgentEventKind::ToolResult {
                        id,
                        content,
                        is_error,
                    } => Some((id.clone(), (content.clone(), *is_error))),
                    _ => None,
                })
                .collect::<HashMap<_, _>>()
        })
    });
    let resolved = Memo::new(move |_| {
        events.with(|events| {
            events
                .iter()
                .filter_map(|e| match &e.kind {
                    AgentEventKind::ApprovalResolved { id, allow } => Some((id.clone(), *allow)),
                    _ => None,
                })
                .collect::<HashMap<_, _>>()
        })
    });
    // The page may hear of a request before it refetches the status.
    let waiting = Signal::derive(move || {
        session.with(|s| matches!(s.status, AgentStatus::Running | AgentStatus::AwaitingApproval))
    });
    let (repo, id) = session.with_untracked(|s| (s.repo.clone(), s.id.clone()));

    // Keeps the newest output in view unless the user scrolled up.
    let end = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        events.track();
        streaming.track();
        let Some(end) = end.get() else { return };
        let Some(main) = end.closest(".main").ok().flatten() else {
            return;
        };
        let below = main.scroll_height() - main.scroll_top() - main.client_height();
        if below < 400 {
            main.set_scroll_top(main.scroll_height());
        }
    });

    view! {
        <div class="transcript">
            <For
                each=move || events.get()
                key=|e| e.seq
                children=move |e| {
                    let repo = repo.clone();
                    let id = id.clone();
                    match e.kind {
                        AgentEventKind::Prompt(p) => view! {
                            <div class="turn-prompt">{p}</div>
                        }.into_any(),
                        AgentEventKind::Text(t) => view! {
                            <div class="turn-text markdown" inner_html=markdown::to_html(&t, &repo)></div>
                        }.into_any(),
                        AgentEventKind::ToolUse { id: tool, name, input } => {
                            let value = serde_json::from_str(&input).unwrap_or_default();
                            let summary = tool_summary(&name, &value);
                            let result = Signal::derive(move || results.with(|r| r.get(&tool).cloned()));
                            view! {
                                <details class="tool">
                                    <summary>
                                        <span class="tool-name">{name}</span>
                                        " " <code>{summary}</code>
                                        {move || match result.get() {
                                            None => view! { <span class="muted">" …"</span> }.into_any(),
                                            Some((_, true)) => view! { <span class="error">" failed"</span> }.into_any(),
                                            Some(_) => ().into_any(),
                                        }}
                                    </summary>
                                    <pre class="tool-input">{pretty(&input)}</pre>
                                    {move || result.get().map(|(content, is_error)| view! {
                                        <pre class="tool-result" class:error=is_error>{content}</pre>
                                    })}
                                </details>
                            }.into_any()
                        }
                        AgentEventKind::ApprovalRequested { id: tool, tool_name, input } => {
                            let value = serde_json::from_str(&input).unwrap_or_default();
                            let summary = tool_summary(&tool_name, &value);
                            view! {
                                <ApprovalCard
                                    agent=id
                                    tool=tool
                                    tool_name=tool_name
                                    summary=summary
                                    input=pretty(&input)
                                    resolved=resolved
                                    waiting=waiting
                                />
                            }.into_any()
                        }
                        AgentEventKind::TurnEnded { error: Some(e), .. } => view! {
                            <p class="turn-end error">{format!("The turn ended: {e}")}</p>
                        }.into_any(),
                        AgentEventKind::TurnEnded { error: None, cost_usd } => view! {
                            <p class="turn-end muted">{format!("Done · {}", cost(cost_usd.unwrap_or_default()))}</p>
                        }.into_any(),
                        AgentEventKind::Error(e) => view! {
                            <pre class="turn-error">{e}</pre>
                        }.into_any(),
                        AgentEventKind::ToolResult { .. }
                        | AgentEventKind::ApprovalResolved { .. }
                        | AgentEventKind::TextDelta(_) => ().into_any(),
                    }
                }
            />
            {move || {
                let text = streaming.get();
                (!text.is_empty()).then(|| view! { <div class="turn-text streaming">{text}</div> })
            }}
            <div node_ref=end></div>
        </div>
    }
}

#[component]
fn ApprovalCard(
    agent: String,
    tool: String,
    tool_name: String,
    summary: String,
    input: String,
    resolved: Memo<HashMap<String, bool>>,
    /// Whether Claude Code is running and so may wait for an answer;
    /// requests from before a restart stay unanswered.
    waiting: Signal<bool>,
) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let answer = {
        let tool = tool.clone();
        move |allow: bool| {
            let approval = Approval {
                id: tool.clone(),
                allow,
                message: None,
            };
            let agent = agent.clone();
            busy.set(true);
            spawn_local(async move {
                if let Err(e) = api::approve(&agent, &approval).await {
                    error.set(Some(e));
                }
                busy.set(false);
            });
        }
    };
    let deny = answer.clone();
    let state = Signal::derive(move || resolved.with(|r| r.get(&tool).copied()));
    view! {
        <div class="approval" class:pending=move || state.get().is_none() && waiting.get()>
            <p>
                {move || match state.get() {
                    Some(true) => "Allowed ",
                    Some(false) => "Denied ",
                    None if waiting.get() => "Claude wants to use ",
                    None => "Unanswered: ",
                }}
                <span class="tool-name">{tool_name}</span> " " <code>{summary}</code>
            </p>
            {move || (state.get().is_none() && waiting.get()).then(|| {
                let (allow, deny) = (answer.clone(), deny.clone());
                view! {
                    <pre class="tool-input">{input.clone()}</pre>
                    <div class="form-buttons">
                        <button class="primary" disabled=move || busy.get() on:click=move |_| allow(true)>"Allow"</button>
                        <button disabled=move || busy.get() on:click=move |_| deny(false)>"Deny"</button>
                    </div>
                }
            })}
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
        </div>
    }
}

#[component]
fn Composer(session: Signal<AgentSession>) -> impl IntoView {
    let prompt = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let working = move || {
        session.with(|s| matches!(s.status, AgentStatus::Running | AgentStatus::AwaitingApproval))
    };
    let id = StoredValue::new(session.with_untracked(|s| s.id.clone()));

    let send = move || {
        let text = prompt.get_untracked();
        if text.trim().is_empty() {
            return;
        }
        spawn_local(async move {
            match api::send_agent_message(&id.get_value(), text).await {
                Ok(()) => prompt.set(String::new()),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let stop = move |_| {
        spawn_local(async move {
            if let Err(e) = api::interrupt_agent(&id.get_value()).await {
                error.set(Some(e));
            }
        });
    };

    view! {
        <form class="composer" on:submit=move |ev| {
            ev.prevent_default();
            send();
        }>
            <textarea
                rows="3"
                aria-label="Follow-up"
                placeholder=move || if working() { "Queue a follow-up… (Ctrl+Enter sends)" } else { "Reply to Claude… (Ctrl+Enter sends)" }
                prop:value=move || prompt.get()
                on:input=move |ev| prompt.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if ev.key() == "Enter" && (ev.ctrl_key() || ev.meta_key()) {
                        ev.prevent_default();
                        send();
                    }
                }
            ></textarea>
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
            <div class="form-buttons">
                <button type="submit" class="primary" disabled=move || prompt.with(|p| p.trim().is_empty())>"Send"</button>
                {move || working().then(|| view! { <button type="button" on:click=stop>"Stop"</button> })}
            </div>
        </form>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_summaries() {
        let input = serde_json::json!({"command": "ls -la\nmore", "description": "List"});
        assert_eq!(tool_summary("Bash", &input), "ls -la");
        let input = serde_json::json!({"file_path": "/w/a.rs"});
        assert_eq!(tool_summary("Edit", &input), "/w/a.rs");
        assert_eq!(tool_summary("mcp__x__y", &input), "");
    }

    #[test]
    fn pretty_prints_json() {
        assert_eq!(pretty(r#"{"a":1}"#), "{\n  \"a\": 1\n}");
        assert_eq!(pretty("not json"), "not json");
    }
}
