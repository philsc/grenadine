//! Claude Code sessions: a sidebar listing them by repository, a form that
//! starts one, and a session's page with its live transcript, permission
//! requests and a box for follow-up prompts.

use std::collections::{BTreeMap, HashMap};

use grenadine_core::api::{
    AgentEvent, AgentEventKind, AgentSession, AgentStatus, Approval, Landed, NewAgent, PatchHunk,
    PermissionMode, PrKey, WrapUp, WrapUpTarget,
};
use grenadine_core::edit::{self, Fragment};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use wasm_bindgen::prelude::*;

use crate::diffview::EditDiff;
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

/// Where a session's work went, in a few words.
fn landed_label(landed: &Landed) -> String {
    match landed {
        Landed::Checkout { sha } => format!("merged {}", &sha[..sha.len().min(8)]),
        Landed::PullRequest { number, .. } => format!("PR #{number}"),
    }
}

/// `landed_label`, linking to the PR.
#[component]
fn LandedBadge(landed: Landed) -> impl IntoView {
    let label = landed_label(&landed);
    match landed {
        Landed::Checkout { .. } => view! { <span class="agent-landed">{label}</span> }.into_any(),
        Landed::PullRequest { url, .. } => view! {
            <a class="agent-landed" href=url target="_blank">{label}</a>
        }
        .into_any(),
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
                            if let Some(landed) = &a.landed {
                                meta.push(landed_label(landed));
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

/// The permission mode the next prompt goes with: `initial` if given,
/// otherwise the last one picked, Auto at first.
fn mode_signal(initial: Option<PermissionMode>) -> RwSignal<PermissionMode> {
    let mode = RwSignal::new(initial.unwrap_or_else(|| {
        storage()
            .and_then(|s| s.get_item(MODE_KEY).ok().flatten())
            .and_then(|v| PermissionMode::parse(&v))
            .unwrap_or(PermissionMode::Auto)
    }));
    Effect::new(move |_| {
        let mode = mode.get();
        if let Some(s) = storage() {
            let _ = s.set_item(MODE_KEY, mode.as_str());
        }
    });
    mode
}

/// The modes the turn can go on in once a plan is approved.
const AFTER_PLAN: [PermissionMode; 3] = [
    PermissionMode::Manual,
    PermissionMode::AcceptEdits,
    PermissionMode::Auto,
];

#[component]
fn ModeSelect(
    mode: RwSignal<PermissionMode>,
    #[prop(default = &PermissionMode::ALL)] modes: &'static [PermissionMode],
    #[prop(default = "Permissions")] label: &'static str,
) -> impl IntoView {
    view! {
        <select
            class="agent-mode"
            aria-label=label
            title="What Claude may do without asking here"
            prop:value=move || mode.get().as_str()
            on:change=move |ev| {
                if let Some(m) = PermissionMode::parse(&event_target_value(&ev)) {
                    mode.set(m);
                }
            }
        >
            {modes.iter().map(|&m| view! {
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
    // New sessions start out planning, whatever was picked last.
    let mode = mode_signal(Some(PermissionMode::Plan));
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
                <p class="hint muted">"The session gets its own git worktree on a new branch. Claude asks here before it uses a tool, unless the permissions let it go ahead: Accept edits allows file edits, and Auto allows whatever Claude Code's classifier deems safe. Plan first has Claude look around without changing anything, then asks you to approve its plan before it goes ahead. Every prompt can pick different permissions."</p>
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
        "ExitPlanMode" => "plan",
        "AskUserQuestion" => {
            return input["questions"][0]["question"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        }
        _ => "",
    };
    match input.get(field).and_then(|v| v.as_str()) {
        Some(s) => s.lines().next().unwrap_or_default().to_owned(),
        None => String::new(),
    }
}

/// The file a file tool changes and the pieces of it that change, or `None`
/// for other tools. The `patch` from the tool's result places the pieces in
/// the file; without it, they are numbered from the first line.
fn edit_fragments(
    name: &str,
    input: &serde_json::Value,
    patch: &[PatchHunk],
) -> Option<(String, Vec<Fragment>)> {
    let text =
        |v: &serde_json::Value, field: &str| v[field].as_str().unwrap_or_default().to_owned();
    let fragments = match name {
        "Edit" | "MultiEdit" | "Write" if !patch.is_empty() => edit::from_patch(patch),
        "Edit" => vec![edit::from_strings(
            &text(input, "old_string"),
            &text(input, "new_string"),
        )],
        "MultiEdit" => input["edits"]
            .as_array()?
            .iter()
            .map(|e| edit::from_strings(&text(e, "old_string"), &text(e, "new_string")))
            .collect(),
        "Write" => vec![edit::from_strings("", &text(input, "content"))],
        _ => return None,
    };
    Some((input["file_path"].as_str()?.to_owned(), fragments))
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
    // Approving a plan also picks the mode the follow-ups go with.
    let mode = mode_signal(None);

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
                        <AgentHeader session=session mode=mode />
                        <Transcript session=session events=events streaming=streaming mode=mode />
                        <Composer session=session usage=usage mode=mode />
                    }.into_any()
                }
            }}
        </div>
    }
}

#[component]
fn AgentHeader(session: Signal<AgentSession>, mode: RwSignal<PermissionMode>) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let wrapping = RwSignal::new(false);
    let busy = Signal::derive(move || {
        wrapping.get()
            || session.with(|s| {
                matches!(
                    s.status,
                    AgentStatus::Running | AgentStatus::AwaitingApproval
                )
            })
    });
    // Claude may get prompts to commit or to resolve conflicts first;
    // they go with the mode the composer has picked.
    let wrap_up = move |target| {
        let id = session.with_untracked(|s| s.id.clone());
        let req = WrapUp {
            target,
            permission_mode: mode.get_untracked(),
        };
        wrapping.set(true);
        error.set(None);
        spawn_local(async move {
            if let Err(e) = api::wrap_up_agent(&id, &req).await {
                error.set(Some(e));
            }
            wrapping.set(false);
        });
    };
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
                        {s.landed.clone().map(|l| view! { " · " <LandedBadge landed=l /> })}
                        <span class="agent-actions">
                            <button
                                class="link"
                                title="Rebase the session's commits onto the server's checkout and fast-forward it"
                                disabled=busy
                                on:click=move |_| wrap_up(WrapUpTarget::Checkout)
                            >"Merge into checkout"</button>
                            <button
                                class="link"
                                title="Push the session's branch to GitHub and open a pull request"
                                disabled=busy
                                on:click=move |_| wrap_up(WrapUpTarget::PullRequest)
                            >"Open PR"</button>
                            <button class="link danger" on:click=delete>"Delete"</button>
                        </span>
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
    mode: RwSignal<PermissionMode>,
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
                        patch,
                    } => Some((id.clone(), (content.clone(), *is_error, patch.clone()))),
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
        session.with(|s| {
            matches!(
                s.status,
                AgentStatus::Running | AgentStatus::AwaitingApproval
            )
        })
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
                            let value: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
                            let summary = tool_summary(&name, &value);
                            let result = Signal::derive(move || results.with(|r| r.get(&tool).cloned()));
                            let is_edit = edit_fragments(&name, &value, &[]).is_some();
                            let body = {
                                let name = name.clone();
                                move || {
                                    let patch = result.with(|r| r.as_ref().map(|(_, _, p)| p.clone()));
                                    match edit_fragments(&name, &value, &patch.unwrap_or_default()) {
                                        Some((path, fragments)) => view! { <EditDiff path=path fragments=fragments /> }.into_any(),
                                        None => view! { <pre class="tool-input">{pretty(&input)}</pre> }.into_any(),
                                    }
                                }
                            };
                            view! {
                                <details class="tool">
                                    <summary>
                                        <span class="tool-name">{name}</span>
                                        " " <code>{summary}</code>
                                        {move || match result.get() {
                                            None => view! { <span class="muted">" …"</span> }.into_any(),
                                            Some((_, true, _)) => view! { <span class="error">" failed"</span> }.into_any(),
                                            Some(_) => ().into_any(),
                                        }}
                                    </summary>
                                    {body}
                                    // A successful edit's result only repeats the diff.
                                    {move || result.get()
                                        .filter(|(_, is_error, _)| *is_error || !is_edit)
                                        .map(|(content, is_error, _)| view! {
                                            <pre class="tool-result" class:error=is_error>{content}</pre>
                                        })}
                                </details>
                            }.into_any()
                        }
                        AgentEventKind::ApprovalRequested { id: tool, tool_name, input } if tool_name == "ExitPlanMode" => {
                            let value: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
                            let plan = value["plan"].as_str().unwrap_or_default();
                            view! {
                                <PlanCard
                                    agent=id
                                    tool=tool
                                    plan=markdown::to_html(plan, &repo)
                                    resolved=resolved
                                    waiting=waiting
                                    mode=mode
                                />
                            }.into_any()
                        }
                        AgentEventKind::ApprovalRequested { id: tool, tool_name, input } if tool_name == "AskUserQuestion" => {
                            let questions = serde_json::from_str::<Questions>(&input)
                                .map(|q| q.questions)
                                .unwrap_or_default();
                            view! {
                                <QuestionCard
                                    agent=id
                                    tool=tool
                                    questions=questions
                                    resolved=resolved
                                    waiting=waiting
                                />
                            }.into_any()
                        }
                        AgentEventKind::ApprovalRequested { id: tool, tool_name, input } => {
                            let value = serde_json::from_str(&input).unwrap_or_default();
                            let summary = tool_summary(&tool_name, &value);
                            let diff = edit_fragments(&tool_name, &value, &[]);
                            view! {
                                <ApprovalCard
                                    agent=id
                                    tool=tool
                                    tool_name=tool_name
                                    summary=summary
                                    input=pretty(&input)
                                    diff=diff
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
                        AgentEventKind::Landed(Landed::Checkout { sha }) => view! {
                            <p class="turn-end wrapped-up">
                                "Merged into the checkout at " <code>{sha}</code>
                            </p>
                        }.into_any(),
                        AgentEventKind::Landed(Landed::PullRequest { number, url }) => view! {
                            <p class="turn-end wrapped-up">
                                "Opened " <a href=url target="_blank">{format!("PR #{number}")}</a>
                            </p>
                        }.into_any(),
                        AgentEventKind::WrapUpFailed(e) => view! {
                            <pre class="turn-error">{format!("Couldn't wrap up: {e}")}</pre>
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

/// Sends the user's answer to a permission request.
fn send_approval(
    agent: String,
    approval: Approval,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        if let Err(e) = api::approve(&agent, &approval).await {
            error.set(Some(e));
        }
        busy.set(false);
    });
}

/// `ExitPlanMode`: Claude's plan, to approve or to send back with
/// feedback.
#[component]
fn PlanCard(
    agent: String,
    tool: String,
    /// The plan as HTML.
    plan: String,
    resolved: Memo<HashMap<String, bool>>,
    waiting: Signal<bool>,
    /// The composer's mode, which moves on from planning with an approval.
    mode: RwSignal<PermissionMode>,
) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let feedback = RwSignal::new(String::new());
    let then = RwSignal::new(PermissionMode::Auto);
    let answer = {
        let tool = tool.clone();
        move |allow: bool| {
            let message = Some(feedback.get_untracked())
                .filter(|f| !f.trim().is_empty())
                .unwrap_or_else(|| "The user wants you to keep planning.".into());
            let approval = Approval {
                id: tool.clone(),
                allow,
                message: (!allow).then_some(message),
                mode: allow.then(|| then.get_untracked()),
                answers: None,
            };
            if allow && mode.get_untracked() == PermissionMode::Plan {
                mode.set(then.get_untracked());
            }
            send_approval(agent.clone(), approval, busy, error);
        }
    };
    let keep = answer.clone();
    let state = Signal::derive(move || resolved.with(|r| r.get(&tool).copied()));
    let pending = move || state.get().is_none() && waiting.get();
    view! {
        <div class="approval plan" class:pending=pending>
            <p>
                {move || match state.get() {
                    Some(true) => "Plan approved",
                    Some(false) => "Plan sent back",
                    None if waiting.get() => "Claude's plan is waiting for your approval",
                    None => "Unanswered plan",
                }}
            </p>
            <div class="plan-text markdown" inner_html=plan></div>
            {move || pending().then(|| {
                let (approve, keep) = (answer.clone(), keep.clone());
                view! {
                    <textarea
                        class="plan-feedback"
                        rows="2"
                        aria-label="Feedback"
                        placeholder="What should change? Claude gets this when it keeps planning."
                        prop:value=move || feedback.get()
                        on:input=move |ev| feedback.set(event_target_value(&ev))
                    ></textarea>
                    <div class="form-buttons">
                        <label>"Then " <ModeSelect mode=then modes=&AFTER_PLAN label="After the plan" /></label>
                        <button class="primary" disabled=move || busy.get() on:click=move |_| approve(true)>"Approve plan"</button>
                        <button disabled=move || busy.get() on:click=move |_| keep(false)>"Keep planning"</button>
                    </div>
                }
            })}
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
        </div>
    }
}

/// `AskUserQuestion`'s input.
#[derive(Deserialize)]
struct Questions {
    questions: Vec<Question>,
}

#[derive(Clone, Deserialize)]
struct Question {
    question: String,
    /// A short label for the question.
    #[serde(default)]
    header: String,
    #[serde(default)]
    options: Vec<QuestionOption>,
    #[serde(default, rename = "multiSelect")]
    multi_select: bool,
}

#[derive(Clone, Deserialize)]
struct QuestionOption {
    label: String,
    #[serde(default)]
    description: String,
}

/// The answer to a question the way Claude Code puts it together: the
/// picked options in their order, then whatever the user wrote.
fn answer_text(q: &Question, picked: &[String], other: &str) -> String {
    q.options
        .iter()
        .map(|o| o.label.as_str())
        .filter(|l| picked.iter().any(|p| p == l))
        .chain(Some(other.trim()).filter(|o| !o.is_empty()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `AskUserQuestion`: Claude's questions with their options and room for
/// another answer.
#[component]
fn QuestionCard(
    agent: String,
    tool: String,
    questions: Vec<Question>,
    resolved: Memo<HashMap<String, bool>>,
    waiting: Signal<bool>,
) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    // The options picked and the text written for each question.
    let picks: Vec<(RwSignal<Vec<String>>, RwSignal<String>)> = questions
        .iter()
        .map(|_| (RwSignal::new(Vec::new()), RwSignal::new(String::new())))
        .collect();
    let questions = StoredValue::new(questions);
    let picks = StoredValue::new(picks);
    let answers = move || {
        questions.with_value(|qs| {
            picks.with_value(|ps| {
                qs.iter()
                    .zip(ps)
                    .map(|(q, (picked, other))| {
                        let answer = picked.with(|p| other.with(|o| answer_text(q, p, o)));
                        (q.question.clone(), answer)
                    })
                    .collect::<BTreeMap<_, _>>()
            })
        })
    };
    let complete = move || answers().values().all(|a| !a.is_empty());
    let answer = {
        let tool = tool.clone();
        move |allow: bool| {
            let approval = Approval {
                id: tool.clone(),
                allow,
                message: (!allow).then(|| "The user didn't answer.".into()),
                mode: None,
                answers: allow.then(answers),
            };
            send_approval(agent.clone(), approval, busy, error);
        }
    };
    let skip = answer.clone();
    let name = StoredValue::new(tool.clone());
    let state = Signal::derive(move || resolved.with(|r| r.get(&tool).copied()));
    let pending = move || state.get().is_none() && waiting.get();
    view! {
        <div class="approval questions" class:pending=pending>
            <p>
                {move || match state.get() {
                    Some(true) => "Answered",
                    Some(false) => "Not answered",
                    None if waiting.get() => "Claude asks",
                    None => "Unanswered",
                }}
            </p>
            {move || if pending() {
                let (answer, skip) = (answer.clone(), skip.clone());
                let fields = questions.get_value().into_iter().zip(picks.get_value()).enumerate().map(|(i, (q, (picked, other)))| {
                    let group = format!("{}-{i}", name.get_value());
                    let multi = q.multi_select;
                    let options = q.options.into_iter().map(|o| {
                        let label = StoredValue::new(o.label.clone());
                        let checked = move || picked.with(|p| label.with_value(|l| p.contains(l)));
                        let toggle = move |_| {
                            let label = label.get_value();
                            if multi {
                                picked.update(|p| match p.iter().position(|l| *l == label) {
                                    Some(i) => {
                                        p.remove(i);
                                    }
                                    None => p.push(label),
                                });
                            } else {
                                picked.set(vec![label]);
                                other.set(String::new());
                            }
                        };
                        view! {
                            <label class="question-option">
                                <input
                                    type=if multi { "checkbox" } else { "radio" }
                                    name=group.clone()
                                    prop:checked=checked
                                    on:change=toggle
                                />
                                " " <span>{o.label}</span>
                                {(!o.description.is_empty()).then(|| view! {
                                    <span class="muted">{format!(" – {}", o.description)}</span>
                                })}
                            </label>
                        }
                    }).collect_view();
                    view! {
                        <fieldset class="question">
                            <legend>
                                {(!q.header.is_empty()).then(|| view! { <span class="tool-name">{q.header}</span> " " })}
                                {q.question}
                            </legend>
                            {options}
                            <input
                                type="text"
                                class="question-other"
                                placeholder="Something else…"
                                aria-label="Something else"
                                prop:value=move || other.get()
                                on:input=move |ev| {
                                    let text = event_target_value(&ev);
                                    // A single answer is either an option or the text.
                                    if !multi && !text.trim().is_empty() {
                                        picked.set(Vec::new());
                                    }
                                    other.set(text);
                                }
                            />
                        </fieldset>
                    }
                }).collect_view();
                view! {
                    {fields}
                    <div class="form-buttons">
                        <button class="primary" disabled=move || busy.get() || !complete() on:click=move |_| answer(true)>"Answer"</button>
                        <button disabled=move || busy.get() on:click=move |_| skip(false)>"Skip"</button>
                    </div>
                }.into_any()
            } else {
                questions.with_value(|qs| qs.iter().map(|q| view! {
                    <p class="muted">{q.question.clone()}</p>
                }).collect_view()).into_any()
            }}
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
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
    /// What a file tool would change, shown instead of `input`.
    diff: Option<(String, Vec<Fragment>)>,
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
                mode: None,
                answers: None,
            };
            send_approval(agent.clone(), approval, busy, error);
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
                    {match diff.clone() {
                        Some((path, fragments)) => view! { <EditDiff path=path fragments=fragments /> }.into_any(),
                        None => view! { <pre class="tool-input">{input.clone()}</pre> }.into_any(),
                    }}
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
    mode: RwSignal<PermissionMode>,
) -> impl IntoView {
    let prompt = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let working = move || {
        session.with(|s| {
            matches!(
                s.status,
                AgentStatus::Running | AgentStatus::AwaitingApproval
            )
        })
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
    fn edit_fragments_by_tool() {
        let path = || "/w/a.rs".to_owned();
        let input =
            serde_json::json!({"file_path": "/w/a.rs", "old_string": "a", "new_string": "b"});
        assert_eq!(
            edit_fragments("Edit", &input, &[]),
            Some((path(), vec![edit::from_strings("a", "b")]))
        );
        let input = serde_json::json!({"file_path": "/w/a.rs", "edits": [
            {"old_string": "a", "new_string": "b"},
            {"old_string": "c", "new_string": "d"},
        ]});
        assert_eq!(
            edit_fragments("MultiEdit", &input, &[]),
            Some((
                path(),
                vec![edit::from_strings("a", "b"), edit::from_strings("c", "d")]
            ))
        );
        let input = serde_json::json!({"file_path": "/w/a.rs", "content": "x\n"});
        assert_eq!(
            edit_fragments("Write", &input, &[]),
            Some((path(), vec![edit::from_strings("", "x\n")]))
        );
        let patch = [PatchHunk {
            old_start: 7,
            new_start: 7,
            lines: vec!["-a".into(), "+b".into()],
        }];
        assert_eq!(
            edit_fragments("Edit", &input, &patch),
            Some((path(), edit::from_patch(&patch)))
        );
        assert_eq!(edit_fragments("Read", &input, &patch), None);
    }

    #[test]
    fn tool_summaries() {
        let input = serde_json::json!({"command": "ls -la\nmore", "description": "List"});
        assert_eq!(tool_summary("Bash", &input), "ls -la");
        let input = serde_json::json!({"file_path": "/w/a.rs"});
        assert_eq!(tool_summary("Edit", &input), "/w/a.rs");
        assert_eq!(tool_summary("mcp__x__y", &input), "");
        let input = serde_json::json!({"plan": "# Plan\n\nSteps"});
        assert_eq!(tool_summary("ExitPlanMode", &input), "# Plan");
        let input = serde_json::json!({"questions": [{"question": "Tea?"}]});
        assert_eq!(tool_summary("AskUserQuestion", &input), "Tea?");
    }

    #[test]
    fn answers() {
        let q: Question = serde_json::from_value(serde_json::json!({
            "question": "Drinks?",
            "options": [{"label": "Tea"}, {"label": "Coffee"}],
            "multiSelect": true,
        }))
        .unwrap();
        let picked = ["Coffee".to_owned(), "Tea".to_owned()];
        assert_eq!(answer_text(&q, &picked, ""), "Tea, Coffee");
        assert_eq!(answer_text(&q, &picked[..1], " Water "), "Coffee, Water");
        assert_eq!(answer_text(&q, &[], "  "), "");
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
            landed: None,
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
