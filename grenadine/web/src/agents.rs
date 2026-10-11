//! Claude Code sessions: a sidebar listing them by repository, a form that
//! starts one, and a session's page with its live transcript, permission
//! requests and a box for follow-up prompts.

use std::collections::HashMap;

use grenadine_core::api::{
    AgentEvent, AgentEventKind, AgentSession, AgentStatus, Approval, NewAgent, PermissionMode,
    PrKey,
};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;

use crate::inboxes::storage;
use crate::{Route, Updates, api, hash_for, markdown};

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

/// A token count in a few characters, e.g. `950`, `41.5k` or `1M`.
fn format_tokens(n: u64) -> String {
    let short = |v: f64| {
        let s = format!("{v:.1}");
        s.strip_suffix(".0").map(str::to_owned).unwrap_or(s)
    };
    let k = n as f64 / 1_000.0;
    if n < 1_000 {
        n.to_string()
    } else if k < 999.95 {
        format!("{}k", short(k))
    } else {
        format!("{}M", short(k / 1_000.0))
    }
}

/// The colour of the context meter at `percent` full.
fn usage_level(percent: f64) -> &'static str {
    if percent < 25.0 {
        "green"
    } else if percent < 50.0 {
        "yellow"
    } else if percent <= 75.0 {
        "orange"
    } else {
        "red"
    }
}

#[component]
fn StatusBadge(status: AgentStatus) -> impl IntoView {
    view! { <span class=status_class(status)>{status_label(status)}</span> }
}

/// The sessions grouped by repository: every configured repository in
/// order, even without sessions, then any other repository a session is in.
fn by_repo(repos: Vec<String>, sessions: Vec<AgentSession>) -> Vec<(String, Vec<AgentSession>)> {
    let mut groups: Vec<(String, Vec<AgentSession>)> =
        repos.into_iter().map(|r| (r, Vec::new())).collect();
    for s in sessions {
        match groups.iter_mut().find(|(r, _)| *r == s.repo) {
            Some((_, list)) => list.push(s),
            None => groups.push((s.repo.clone(), vec![s])),
        }
    }
    groups
}

/// The sessions down the left of the agents' pages, a section per
/// repository.
#[component]
pub fn Sidebar(
    /// The page shown, to highlight its session.
    route: Signal<Route>,
) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let repos = LocalResource::new(api::repos);
    let sessions = LocalResource::new(move || {
        updates.agents.track();
        api::agents()
    });

    view! {
        <aside class="agent-sidebar">
            <a
                class="agent-new"
                class:selected=move || route.with(|r| matches!(r, Route::Agents { .. }))
                href="#/agents"
            >"+ New session"</a>
            {move || match (repos.get(), sessions.get()) {
                (Some(Err(e)), _) | (_, Some(Err(e))) => {
                    view! { <p class="error pad">{e}</p> }.into_any()
                }
                (Some(Ok(repos)), Some(Ok(sessions))) => by_repo(repos, sessions)
                    .into_iter()
                    .map(|(repo, list)| view! { <RepoSessions repo=repo list=list route=route /> })
                    .collect_view()
                    .into_any(),
                _ => view! { <p class="muted pad">"Loading…"</p> }.into_any(),
            }}
        </aside>
    }
}

#[component]
fn RepoSessions(repo: String, list: Vec<AgentSession>, route: Signal<Route>) -> impl IntoView {
    let new = format!("#/agents/new/{repo}");
    view! {
        <section class="agent-repo">
            <h2 class="agent-repo-name">
                <span>{repo.clone()}</span>
                <a href=new.clone() title=format!("New session in {repo}")>"+"</a>
            </h2>
            {if list.is_empty() {
                view! {
                    <p class="agent-none muted">
                        "No agents in this repository yet. " <a href=new>"Start one"</a>
                    </p>
                }.into_any()
            } else {
                view! {
                    <ul class="agent-list">
                        {list.into_iter().map(|a| {
                            let id = a.id.clone();
                            let selected = move || route.with(|r| matches!(r, Route::Agent(s) if *s == id));
                            let mut meta = vec![a.branch.clone(), cost(a.cost_usd)];
                            if let Some(n) = a.pr {
                                meta.insert(0, format!("#{n}"));
                            }
                            view! {
                                <li>
                                    <a class="agent-item" class:selected=selected href=agent_hash(&a.id)>
                                        <span class="agent-title">
                                            <StatusBadge status=a.status />
                                            {a.title.clone()}
                                        </span>
                                        <span class="agent-meta muted">{meta.join(" · ")}</span>
                                    </a>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </section>
    }
}

const MODE_KEY: &str = "grenadine.agent-permission-mode";

/// The permission mode the next prompt goes with: the last one picked,
/// Auto at first.
fn mode_signal() -> RwSignal<PermissionMode> {
    let mode = RwSignal::new(
        storage()
            .and_then(|s| s.get_item(MODE_KEY).ok().flatten())
            .and_then(|v| PermissionMode::parse(&v))
            .unwrap_or(PermissionMode::Auto),
    );
    Effect::new(move |_| {
        let mode = mode.get();
        if let Some(s) = storage() {
            let _ = s.set_item(MODE_KEY, mode.as_str());
        }
    });
    mode
}

#[component]
fn ModeSelect(mode: RwSignal<PermissionMode>) -> impl IntoView {
    view! {
        <select
            class="agent-mode"
            aria-label="Permissions"
            title="What Claude may do without asking here"
            prop:value=move || mode.get().as_str()
            on:change=move |ev| {
                if let Some(m) = PermissionMode::parse(&event_target_value(&ev)) {
                    mode.set(m);
                }
            }
        >
            {PermissionMode::ALL.into_iter().map(|m| view! {
                <option value=m.as_str() selected=move || mode.get() == m>{m.label()}</option>
            }).collect_view()}
        </select>
    }
}

/// The form that starts a session, filled in with `repo` and `pr` if given.
#[component]
pub fn NewSession(repo: Option<String>, pr: Option<u64>) -> impl IntoView {
    let repos = LocalResource::new(api::repos);
    let repo = RwSignal::new(repo.unwrap_or_default());
    let number = RwSignal::new(pr.map(|n| n.to_string()).unwrap_or_default());
    let prompt = RwSignal::new(String::new());
    let mode = mode_signal();
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

    let start = move || {
        if busy.get_untracked() || prompt.with_untracked(|p| p.trim().is_empty()) {
            return;
        }
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
            permission_mode: mode.get_untracked(),
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
        <div class="agents">
            <form class="agent-form" on:submit=move |ev| {
                ev.prevent_default();
                start();
            }>
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
                    <label>"Permissions" <ModeSelect mode=mode /></label>
                </div>
                <textarea
                    class="agent-prompt"
                    rows="5"
                    placeholder="What should Claude do? (Ctrl+Enter starts)"
                    aria-label="Prompt"
                    prop:value=move || prompt.get()
                    on:input=move |ev| prompt.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" && (ev.ctrl_key() || ev.meta_key()) {
                            ev.prevent_default();
                            start();
                        }
                    }
                ></textarea>
                <p class="hint muted">"The session gets its own git worktree on a new branch. Claude asks here before it uses a tool, unless the permissions let it go ahead: Accept edits allows file edits, and Auto allows whatever Claude Code's classifier deems safe. Every prompt can pick different permissions."</p>
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
        </div>
    }
}

/// Follows a session's event stream while the page is open. Events arrive
/// in `seq` order; a reconnect replays the transcript, which is skipped up
/// to the last event seen. `usage` gets the context window's tokens and
/// size as Claude Code reports them.
fn follow(
    id: &str,
    events: RwSignal<Vec<AgentEvent>>,
    streaming: RwSignal<String>,
    usage: RwSignal<(Option<u64>, Option<u64>)>,
) {
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
                None => match event.kind {
                    AgentEventKind::TextDelta(t) => streaming.update(|s| s.push_str(&t)),
                    AgentEventKind::Usage { tokens, window } => usage.update(|(t, w)| {
                        *t = tokens.or(*t);
                        *w = window.or(*w);
                    }),
                    _ => {}
                },
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
    let live_usage = RwSignal::new((None, None));
    follow(&id, events, streaming, live_usage);

    view! {
        <div class="agent">
            {move || match load.get() {
                Load::Loading => view! { <p class="muted">"Loading…"</p> }.into_any(),
                Load::Failed(e) => view! { <p class="error">{e}</p> }.into_any(),
                Load::Missing => view! {
                    <p class="muted">"There is no such session. " <a href="#/agents">"Start a new one"</a></p>
                }.into_any(),
                Load::Ready => {
                    // Ready means a session was loaded, and `current` keeps it.
                    let session = Signal::derive(move || current.get().expect("a loaded session"));
                    // The stream's values are newer than the loaded session's.
                    let usage = Signal::derive(move || {
                        let (tokens, window) = live_usage.get();
                        session.with(|s| (tokens.or(s.context_tokens), window.or(s.context_window)))
                    });
                    view! {
                        <AgentHeader session=session />
                        <Transcript session=session events=events streaming=streaming />
                        <Composer session=session usage=usage />
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

    // Keeps the newest output in view, starting with the bottom of the
    // transcript, until the user scrolls up. Scrolling back down follows
    // it again. Whether to follow is decided by where the user scrolled
    // rather than by how far the bottom is, since replaying a long
    // transcript adds a lot of it at once.
    let end = NodeRef::<leptos::html::Div>::new();
    let follow = StoredValue::new(true);
    let on_scroll = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
        let Some(main) = e
            .current_target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        let below = main.scroll_height() - main.scroll_top() - main.client_height();
        follow.set_value(below < 40);
    });
    let on_scroll = StoredValue::new_local(on_scroll);
    let main = StoredValue::new_local(None::<web_sys::Element>);
    Effect::new(move |_| {
        events.track();
        streaming.track();
        let Some(end) = end.get() else { return };
        let Some(el) = end.closest(".main").ok().flatten() else {
            return;
        };
        if main.with_value(Option::is_none) {
            on_scroll.with_value(|f| {
                let _ = el.add_event_listener_with_callback("scroll", f.as_ref().unchecked_ref());
            });
            main.set_value(Some(el.clone()));
        }
        if follow.get_value() {
            el.set_scroll_top(el.scroll_height());
        }
    });
    on_cleanup(move || {
        let Some(Some(m)) = main.try_get_value() else {
            return;
        };
        on_scroll.try_with_value(|f| {
            let _ = m.remove_event_listener_with_callback("scroll", f.as_ref().unchecked_ref());
        });
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
                        | AgentEventKind::TextDelta(_)
                        | AgentEventKind::Usage { .. } => ().into_any(),
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
fn Composer(
    session: Signal<AgentSession>,
    usage: Signal<(Option<u64>, Option<u64>)>,
) -> impl IntoView {
    let prompt = RwSignal::new(String::new());
    let mode = mode_signal();
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
        let mode = mode.get_untracked();
        spawn_local(async move {
            match api::send_agent_message(&id.get_value(), text, mode).await {
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
                <ModeSelect mode=mode />
                <button type="submit" class="primary" disabled=move || prompt.with(|p| p.trim().is_empty())>"Send"</button>
                {move || working().then(|| view! { <button type="button" on:click=stop>"Stop"</button> })}
                <ContextMeter usage=usage />
            </div>
        </form>
    }
}

/// How full the context window is, at the right of the composer's buttons.
#[component]
fn ContextMeter(usage: Signal<(Option<u64>, Option<u64>)>) -> impl IntoView {
    move || {
        let (Some(tokens), window) = usage.get() else {
            return ().into_any();
        };
        let Some(window) = window.filter(|w| *w > 0) else {
            return view! {
                <div class="context-usage" title="Tokens in the context window">
                    <span class="context-text">{format_tokens(tokens)}</span>
                </div>
            }
            .into_any();
        };
        let percent = tokens as f64 * 100.0 / window as f64;
        view! {
            <div
                class="context-usage"
                role="meter"
                aria-label="Context"
                aria-valuemin="0"
                aria-valuemax="100"
                aria-valuenow=format!("{percent:.0}")
                title=format!("{percent:.0}% of the context window")
            >
                <span class="context-bar">
                    <span
                        class=format!("context-fill {}", usage_level(percent))
                        style=format!("width: {:.1}%", percent.min(100.0))
                    ></span>
                </span>
                <span class="context-text">
                    {format!("{} / {}", format_tokens(tokens), format_tokens(window))}
                </span>
            </div>
        }
        .into_any()
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

    fn session(id: &str, repo: &str) -> AgentSession {
        AgentSession {
            id: id.into(),
            repo: repo.into(),
            pr: None,
            title: String::new(),
            branch: String::new(),
            worktree: String::new(),
            base_sha: String::new(),
            status: AgentStatus::Idle,
            created_at: 0,
            cost_usd: 0.0,
            context_tokens: None,
            context_window: None,
        }
    }

    #[test]
    fn groups_sessions_by_repo() {
        let groups = by_repo(
            vec!["o/a".into(), "o/b".into()],
            vec![
                session("1", "o/b"),
                session("2", "o/gone"),
                session("3", "o/b"),
            ],
        );
        let ids: Vec<(&str, Vec<&str>)> = groups
            .iter()
            .map(|(r, l)| (r.as_str(), l.iter().map(|s| s.id.as_str()).collect()))
            .collect();
        assert_eq!(
            ids,
            [
                ("o/a", vec![]),
                ("o/b", vec!["1", "3"]),
                ("o/gone", vec!["2"])
            ]
        );
    }

    #[test]
    fn formats_tokens() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(41_503), "41.5k");
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(999_990), "1M");
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(1_260_000), "1.3M");
    }

    #[test]
    fn usage_levels() {
        assert_eq!(usage_level(0.0), "green");
        assert_eq!(usage_level(24.9), "green");
        assert_eq!(usage_level(25.0), "yellow");
        assert_eq!(usage_level(50.0), "orange");
        assert_eq!(usage_level(75.0), "orange");
        assert_eq!(usage_level(75.1), "red");
        assert_eq!(usage_level(120.0), "red");
    }

    #[test]
    fn pretty_prints_json() {
        assert_eq!(pretty(r#"{"a":1}"#), "{\n  \"a\": 1\n}");
        assert_eq!(pretty("not json"), "not json");
    }
}
