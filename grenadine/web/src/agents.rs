//! Claude Code sessions: a sidebar listing them by repository, a form that
//! starts one, and a session's page with its live transcript, permission
//! requests and a box for follow-up prompts.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use grenadine_core::api::{
    AgentEvent, AgentEventKind, AgentModelInfo, AgentPrs, AgentSession, AgentStatus, Approval,
    CleanupState, GENERAL, Landed, NewAgent, NewCleanupSession, PatchHunk, PermissionMode, PrKey,
    PromptImage, WorktreeFile, WrapUp, WrapUpTarget,
};
use grenadine_core::edit::{self, Fragment};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use wasm_bindgen::prelude::*;

use crate::diffview::{EditDiff, WorktreeDiff};
use crate::drafts::{self, Draft, Drafts};
use crate::images::{self, AttachButton, Attachments};
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
        Landed::Stack { prs } => match (prs.first(), prs.last()) {
            (Some(bottom), Some(top)) => format!("PRs #{}–#{}", bottom.number, top.number),
            _ => "no PRs".into(),
        },
    }
}

/// `landed_label`, linking to the PR, or the top PR of a stack.
#[component]
fn LandedBadge(landed: Landed) -> impl IntoView {
    let label = landed_label(&landed);
    match landed.prs().pop() {
        None => view! { <span class="agent-landed">{label}</span> }.into_any(),
        Some(top) => view! {
            <a class="agent-landed" href=top.url target="_blank">{label}</a>
        }
        .into_any(),
    }
}

/// What a repository picker or the sidebar calls `repo`.
fn repo_label(repo: &str) -> &str {
    if repo == GENERAL { "General" } else { repo }
}

/// The configured repositories, then `GENERAL` for a session in none.
fn with_general(mut repos: Vec<String>) -> Vec<String> {
    repos.push(GENERAL.to_owned());
    repos
}

/// Asks before deleting `session`, then deletes it and calls `deleted`, or
/// `failed` with what went wrong.
fn delete_session(
    session: &AgentSession,
    deleted: impl FnOnce() + 'static,
    failed: impl FnOnce(String) + 'static,
) {
    let message = if session.repo == GENERAL {
        "Delete this session? Its directory is deleted too, with everything in it."
    } else {
        "Delete this session? Its worktree and branch are deleted too, with any changes that weren't pushed."
    };
    let confirmed = window().confirm_with_message(message).unwrap_or(false);
    let id = session.id.clone();
    if !confirmed {
        return;
    }
    spawn_local(async move {
        match api::delete_agent(&id).await {
            Ok(()) => {
                if let Some(s) = storage() {
                    let _ = s.remove_item(&draft_key(&id));
                    let _ = s.remove_item(&mode_key(&id));
                }
                crate::terminal::forget(&id);
                deleted()
            }
            Err(e) => failed(e),
        }
    });
}

#[component]
fn TrashIcon() -> impl IntoView {
    view! {
        <svg
            class="trash"
            viewBox="0 0 16 16"
            width="14"
            height="14"
            fill="none"
            stroke="currentColor"
            stroke-width="1.4"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
        >
            <path d="M2.5 4h11M6 4V2.5h4V4M3.8 4l.7 9.5h7l.7-9.5M6.6 6.5v4.5M9.4 6.5v4.5" />
        </svg>
    }
}

#[component]
fn PencilIcon() -> impl IntoView {
    view! {
        <svg
            viewBox="0 0 16 16"
            width="10"
            height="10"
            fill="none"
            stroke="currentColor"
            stroke-width="1.4"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
        >
            <path d="M10.5 2.5l3 3-8 8h-3v-3zM8.5 4.5l3 3" />
        </svg>
    }
}

#[component]
fn PrIcon() -> impl IntoView {
    view! {
        <svg
            viewBox="0 0 16 16"
            width="12"
            height="12"
            fill="none"
            stroke="currentColor"
            stroke-width="1.4"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
        >
            <circle cx="4" cy="3.5" r="1.5" />
            <circle cx="4" cy="12.5" r="1.5" />
            <circle cx="12" cy="12.5" r="1.5" />
            <path d="M4 5v6M12 11V6.5a2 2 0 0 0-2-2H7.5M9 3 7.5 4.5 9 6" />
        </svg>
    }
}

/// How many of the PRs a session opened are merged, e.g. "1/2". It turns
/// purple once they all are, when the session is likely done, and red
/// while any open one conflicts with its base, when it needs attention.
#[component]
fn PrCount(prs: AgentPrs) -> impl IntoView {
    let AgentPrs {
        merged,
        total,
        conflicting,
    } = prs;
    let done = total > 0 && merged == total;
    let conflict = conflicting > 0;
    let noun = if total == 1 { "PR" } else { "PRs" };
    let mut title = format!("{merged} of {total} {noun} merged");
    if conflict {
        title.push_str(&format!(", {conflicting} with conflicts"));
    }
    view! {
        <span class="agent-prs" class:done=done class:conflict=conflict title=title>
            <PrIcon />
            {format!("{merged}/{total}")}
        </span>
    }
}

#[component]
fn FileDiffIcon() -> impl IntoView {
    view! {
        <svg
            viewBox="0 0 16 16"
            width="12"
            height="12"
            fill="none"
            stroke="currentColor"
            stroke-width="1.4"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
        >
            <path d="M9.5 1.5H4a1 1 0 0 0-1 1v11a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1V5z" />
            <path d="M9.5 1.5V5H13M8 6.5v4M6 8.5h4M6 12.5h4" />
        </svg>
    }
}

/// How many files in the session's worktree have unstaged changes, i.e.
/// work it hasn't committed yet.
#[component]
fn UnstagedCount(count: u32) -> impl IntoView {
    let noun = if count == 1 { "file" } else { "files" };
    view! {
        <span class="agent-unstaged" title=format!("{count} {noun} with unstaged changes")>
            <FileDiffIcon />
            {count}
        </span>
    }
}

#[component]
fn StatusBadge(status: AgentStatus) -> impl IntoView {
    view! { <span class=status_class(status)>{status_label(status)}</span> }
}

/// A repository's section in the sidebar: the ids of the drafts in it,
/// then its sessions.
struct RepoGroup {
    repo: String,
    drafts: Vec<String>,
    sessions: Vec<AgentSession>,
}

/// The group for `repo`, added at the end if there's none yet.
fn group<'a>(groups: &'a mut Vec<RepoGroup>, repo: &str) -> &'a mut RepoGroup {
    let i = match groups.iter().position(|g| g.repo == repo) {
        Some(i) => i,
        None => {
            groups.push(RepoGroup {
                repo: repo.to_owned(),
                drafts: Vec::new(),
                sessions: Vec::new(),
            });
            groups.len() - 1
        }
    };
    &mut groups[i]
}

/// The sessions, and the drafts given as `(repo, id)`, grouped by
/// repository: every one of `repos` in order, even without either, then any
/// other repository a session or draft is in.
fn by_repo(
    repos: Vec<String>,
    sessions: Vec<AgentSession>,
    drafts: Vec<(String, String)>,
) -> Vec<RepoGroup> {
    let mut groups = Vec::new();
    for r in repos {
        group(&mut groups, &r);
    }
    for s in sessions {
        group(&mut groups, &s.repo.clone()).sessions.push(s);
    }
    for (repo, id) in drafts {
        group(&mut groups, &repo).drafts.push(id);
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
    let drafts = expect_context::<Drafts>();
    // Only changes when a draft is added, removed or moved, so typing in
    // one doesn't redraw the sidebar.
    let listed = Memo::new(move |_| {
        route.with(|r| {
            let open = match r {
                Route::Draft(id) => Some(id.as_str()),
                _ => None,
            };
            drafts.listed(open)
        })
    });
    // Deleting from the sidebar failed.
    let error = RwSignal::new(None::<String>);

    view! {
        <aside class="agent-sidebar">
            <a class="agent-new" href="#/agents">"+ New session"</a>
            {move || error.get().map(|e| view! { <p class="error pad">{e}</p> })}
            {move || match (repos.get(), sessions.get()) {
                (Some(Err(e)), _) | (_, Some(Err(e))) => {
                    view! { <p class="error pad">{e}</p> }.into_any()
                }
                (Some(Ok(repos)), Some(Ok(sessions))) => by_repo(with_general(repos), sessions, listed.get())
                    .into_iter()
                    .map(|g| view! { <RepoSessions group=g route=route error=error /> })
                    .collect_view()
                    .into_any(),
                _ => view! { <p class="muted pad">"Loading…"</p> }.into_any(),
            }}
            <CleanupSection route=route />
        </aside>
    }
}

/// The cleanup items and the running scans, refetched when they change.
fn cleanup_resource() -> LocalResource<api::Result<CleanupState>> {
    let updates = expect_context::<Updates>();
    LocalResource::new(move || {
        updates.cleanup.track();
        api::cleanup()
    })
}

/// "1 session", "2 items", ….
fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// The sidebar's last section, always there: what deleted sessions left
/// behind, linking to the page that lists it.
#[component]
fn CleanupSection(route: Signal<Route>) -> impl IntoView {
    let cleanup = cleanup_resource();
    let summary = move || match cleanup.get() {
        Some(Ok(c)) => {
            let title = match c.items.len() {
                0 => "Nothing to clean up".to_owned(),
                n => format!("{} to clean up", count(n, "item")),
            };
            let meta = match c.scanning.len() {
                0 => "Left behind by deleted sessions".to_owned(),
                n => format!("Scanning {}…", count(n, "deleted session")),
            };
            (title, meta)
        }
        Some(Err(e)) => ("Cleanup".to_owned(), e),
        None => ("Cleanup".to_owned(), "Loading…".to_owned()),
    };
    view! {
        <section class="agent-repo agent-cleanup">
            <h2 class="agent-repo-name">"Cleanup"</h2>
            <ul class="agent-list">
                <li>
                    <a
                        class="agent-item"
                        class:selected=move || route.with(|r| *r == Route::Cleanup)
                        href="#/agents/cleanup"
                    >
                        <span class="agent-title">{move || summary().0}</span>
                        <span class="agent-meta muted">{move || summary().1}</span>
                    </a>
                </li>
            </ul>
        </section>
    }
}

/// A day, as the browser writes dates.
fn date(unix_seconds: i64) -> String {
    js_sys::Date::new(&JsValue::from_f64(unix_seconds as f64 * 1000.0))
        .to_locale_date_string("default", &JsValue::UNDEFINED)
        .into()
}

/// What deleted sessions left behind. Items can be deleted one by one, or
/// picked and handed off to a new session that plans how to clean them up.
#[component]
pub fn CleanupView() -> impl IntoView {
    let cleanup = cleanup_resource();
    let repos = LocalResource::new(api::repos);
    let picked = RwSignal::new(Vec::<i64>::new());
    // What items describe is outside any worktree, so the session needs
    // none unless a repository is picked.
    let repo = RwSignal::new(GENERAL.to_owned());
    let model = model_signal();
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);

    let items = move || match cleanup.get() {
        Some(Ok(c)) => c.items,
        _ => Vec::new(),
    };
    // Items that are gone can't stay picked.
    Effect::new(move |_| {
        if let Some(Ok(c)) = cleanup.get() {
            picked.update(|p| p.retain(|id| c.items.iter().any(|i| i.id == *id)));
        }
    });

    let delete = move |id: i64| {
        error.set(None);
        spawn_local(async move {
            if let Err(e) = api::delete_cleanup_item(id).await {
                error.set(Some(e));
            }
        });
    };
    let hand_off = move |_| {
        let ids = picked.get_untracked();
        if busy.get_untracked() || ids.is_empty() {
            return;
        }
        let req = NewCleanupSession {
            ids,
            repo: repo.get_untracked(),
            model: model_arg(model),
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::create_cleanup_session(&req).await {
                Ok(session) => {
                    // The server starts it out planning.
                    if let Some(s) = storage() {
                        let _ = s.set_item(&mode_key(&session.id), PermissionMode::Plan.as_str());
                    }
                    go(&agent_hash(&session.id));
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    view! {
        <div class="agents cleanup">
            <h2>"Cleanup"</h2>
            <p class="hint muted">"When a session is deleted, Claude Haiku reads its transcript for what it left behind outside its worktree, like containers, files elsewhere or pushed branches. Deleting an item here only forgets it. To get rid of what items describe, pick them and start a session that plans how to clean them up."</p>
            {move || cleanup.get().map(|c| match c {
                Err(e) => view! { <p class="error">{e}</p> }.into_any(),
                Ok(c) => view! {
                    {c.scanning.into_iter().map(|title| view! {
                        <p class="cleanup-scanning muted">{format!("Scanning \u{201c}{title}\u{201d}…")}</p>
                    }).collect_view()}
                    {c.items.is_empty().then(|| view! { <p class="muted">"Nothing to clean up."</p> })}
                }.into_any(),
            })}
            <ul class="cleanup-items">
                <For each=items key=|i| (i.id, i.handed_off_to.clone()) let:item>
                    {
                        let id = item.id;
                        let checked = move || picked.with(|p| p.contains(&id));
                        let toggle = move |ev| {
                            let on = event_target_checked(&ev);
                            picked.update(|p| {
                                p.retain(|i| *i != id);
                                if on {
                                    p.push(id);
                                }
                            });
                        };
                        let meta = format!(
                            "From \u{201c}{}\u{201d} in {} · {}",
                            item.session_title,
                            item.repo,
                            date(item.created_at)
                        );
                        view! {
                            <li class="cleanup-item">
                                <div class="cleanup-head">
                                    <label class="cleanup-pick">
                                        <input type="checkbox" prop:checked=checked on:change=toggle />
                                        <span class="cleanup-title">{item.title.clone()}</span>
                                    </label>
                                    <button
                                        class="link danger"
                                        title="Forget this item; what it describes stays"
                                        on:click=move |_| delete(id)
                                    >"Delete"</button>
                                </div>
                                <p class="cleanup-meta muted">
                                    {meta}
                                    {item.handed_off_to.clone().map(|s| view! {
                                        " · " <a href=agent_hash(&s)>"handed off"</a>
                                    })}
                                </p>
                                <div
                                    class="cleanup-detail markdown"
                                    inner_html=markdown::to_html(&item.detail, &item.repo)
                                ></div>
                            </li>
                        }
                    }
                </For>
            </ul>
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
            <div class="cleanup-hand-off">
                <label>"Repository"
                    <select
                        prop:value=move || repo.get()
                        on:change=move |ev| repo.set(event_target_value(&ev))
                    >
                        {move || repos.get().and_then(Result::ok).map(with_general).unwrap_or_default().into_iter().map(|r| {
                            let value = r.clone();
                            let selected = move || repo.with(|v| *v == value);
                            view! { <option value=r.clone() selected=selected>{repo_label(&r).to_owned()}</option> }
                        }).collect_view()}
                    </select>
                </label>
                <label>"Model" <ModelSelect model=model /></label>
                <button
                    class="primary"
                    disabled=move || busy.get() || picked.with(Vec::is_empty)
                    on:click=hand_off
                >
                    {move || match (busy.get(), picked.with(Vec::len)) {
                        (true, _) if repo.with(|r| r == GENERAL) => "Starting…".to_owned(),
                        (true, _) => "Creating worktree…".to_owned(),
                        (false, 0) => "Pick items to clean up".to_owned(),
                        (false, n) => format!("Clean up {} in a new session", count(n, "item")),
                    }}
                </button>
            </div>
        </div>
    }
}

/// A repository's drafts, then its sessions.
#[component]
fn RepoSessions(
    group: RepoGroup,
    route: Signal<Route>,
    error: RwSignal<Option<String>>,
) -> impl IntoView {
    let RepoGroup {
        repo,
        drafts,
        sessions: list,
    } = group;
    let new = format!("#/agents/new/{repo}");
    let (title, none) = if repo == GENERAL {
        (
            "New session in no repository".to_owned(),
            "No general sessions yet. ",
        )
    } else {
        (
            format!("New session in {repo}"),
            "No agents in this repository yet. ",
        )
    };
    view! {
        <section class="agent-repo">
            <h2 class="agent-repo-name">
                <span>{repo_label(&repo).to_owned()}</span>
                <a href=new.clone() title=title>"+"</a>
            </h2>
            {if list.is_empty() && drafts.is_empty() {
                view! {
                    <p class="agent-none muted">{none} <a href=new>"Start one"</a></p>
                }.into_any()
            } else {
                view! {
                    <ul class="agent-list">
                        {drafts.into_iter().map(|id| view! { <DraftEntry id=id route=route /> }).collect_view()}
                        {list.into_iter().map(|a| {
                            let id = a.id.clone();
                            let is_open = move |id: &str| {
                                route.with_untracked(|r| matches!(r, Route::Agent(s) if s == id))
                            };
                            let selected = {
                                let id = id.clone();
                                move || route.with(|r| matches!(r, Route::Agent(s) if *s == id))
                            };
                            let delete = {
                                let session = a.clone();
                                move |_| {
                                    error.set(None);
                                    // Its page can't show it any more.
                                    let leave = is_open(&id);
                                    delete_session(
                                        &session,
                                        move || if leave { go("#/agents") },
                                        move |e| error.set(Some(e)),
                                    );
                                }
                            };
                            let label = a.summary.clone().unwrap_or_else(|| a.title.clone());
                            // A general session has no branch.
                            let mut meta: Vec<String> = [a.branch.clone(), cost(a.cost_usd)]
                                .into_iter()
                                .filter(|m| !m.is_empty())
                                .collect();
                            if let Some(n) = a.pr {
                                meta.insert(0, format!("#{n}"));
                            }
                            // The PR count covers a wrap-up's PR.
                            if let Some(landed @ Landed::Checkout { .. }) = &a.landed {
                                meta.push(landed_label(landed));
                            }
                            view! {
                                <li class="agent-entry">
                                    <a class="agent-item" class:selected=selected href=agent_hash(&a.id)>
                                        <span class="agent-title" title=a.title.clone()>{label}</span>
                                        <span class="agent-status-line">
                                            <StatusBadge status=a.status />
                                            {(a.unstaged > 0).then(|| view! { <UnstagedCount count=a.unstaged /> })}
                                            {a.prs.map(|prs| view! { <PrCount prs /> })}
                                            <span class="agent-meta muted">{meta.join(" · ")}</span>
                                        </span>
                                    </a>
                                    <button
                                        class="icon agent-delete"
                                        title="Delete session"
                                        aria-label="Delete session"
                                        on:click=delete
                                    >
                                        <TrashIcon />
                                    </button>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </section>
    }
}

/// A new session still being written, linking to its form. It looks
/// unlike the sessions so that it's clear it hasn't started.
#[component]
fn DraftEntry(id: String, route: Signal<Route>) -> impl IntoView {
    let drafts = expect_context::<Drafts>();
    let href = drafts::hash(&id);
    // The prompt's first line, if anything was written yet.
    let title = {
        let id = id.clone();
        move || {
            drafts
                .with(&id, |d| d.prompt.trim().lines().next().map(str::to_owned))
                .flatten()
        }
    };
    let meta = {
        let id = id.clone();
        move || {
            drafts
                .with(&id, |d| {
                    let mut meta = vec![d.mode.label().to_owned()];
                    let pr = d.pr.trim().trim_start_matches('#');
                    if !pr.is_empty() {
                        meta.insert(0, format!("#{pr}"));
                    }
                    meta.join(" · ")
                })
                .unwrap_or_default()
        }
    };
    let selected = {
        let id = id.clone();
        move || route.with(|r| matches!(r, Route::Draft(s) if *s == id))
    };
    let discard = move |_| {
        let open = route.with_untracked(|r| matches!(r, Route::Draft(s) if *s == id));
        drafts.remove(&id);
        // Its form can't show it any more.
        if open {
            go("#/agents");
        }
    };
    view! {
        <li class="agent-entry">
            <a class="agent-draft" class:selected=selected href=href>
                {move || match title() {
                    Some(t) => view! { <span class="agent-title" title=t.clone()>{t.clone()}</span> }.into_any(),
                    None => view! { <span class="agent-title muted">"New session"</span> }.into_any(),
                }}
                <span class="agent-status-line">
                    <span class="agent-status draft"><PencilIcon />"Draft"</span>
                    <span class="agent-meta muted">{meta}</span>
                </span>
            </a>
            <button
                class="icon agent-delete"
                title="Discard draft"
                aria-label="Discard draft"
                on:click=discard
            >
                <TrashIcon />
            </button>
        </li>
    }
}

/// Where the permission mode a session's next prompt goes with is kept.
/// Each session has its own, so that starting one in plan mode doesn't
/// change what the others go on with.
fn mode_key(id: &str) -> String {
    format!("grenadine.agent-permission-mode.{id}")
}

/// The permission mode the session's next prompt goes with: the last one
/// picked for it, Auto at first.
fn mode_signal(id: &str) -> RwSignal<PermissionMode> {
    let key = mode_key(id);
    let mode = RwSignal::new(
        storage()
            .and_then(|s| s.get_item(&key).ok().flatten())
            .and_then(|v| PermissionMode::parse(&v))
            .unwrap_or(PermissionMode::Auto),
    );
    Effect::new(move |_| {
        let mode = mode.get();
        if let Some(s) = storage() {
            let _ = s.set_item(&key, mode.as_str());
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

const MODEL_KEY: &str = "grenadine.agent-model";

/// What the model picker shows for Claude Code's configured default.
const DEFAULT_MODEL: &str = "default";

/// The model picked last, an `AgentModelInfo::value`; Claude Code's
/// default at first.
pub(crate) fn last_model() -> String {
    storage()
        .and_then(|s| s.get_item(MODEL_KEY).ok().flatten())
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned())
}

fn save_model(model: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(MODEL_KEY, model);
    }
}

/// The model the next prompt goes with: the last one picked.
fn model_signal() -> RwSignal<String> {
    let model = RwSignal::new(last_model());
    Effect::new(move |_| model.with(|m| save_model(m)));
    model
}

/// Where a session's half-written follow-up is kept, so it survives
/// switching to another session and back.
fn draft_key(id: &str) -> String {
    format!("grenadine.agent-draft.{id}")
}

/// The follow-up being written in session `id`: the draft left there last,
/// kept as it changes and dropped once empty.
fn draft_signal(id: &str) -> RwSignal<String> {
    let key = draft_key(id);
    let draft = RwSignal::new(
        storage()
            .and_then(|s| s.get_item(&key).ok().flatten())
            .unwrap_or_default(),
    );
    Effect::new(move |_| {
        let Some(s) = storage() else { return };
        draft.with(|d| {
            let _ = if d.trim().is_empty() {
                s.remove_item(&key)
            } else {
                s.set_item(&key, d)
            };
        });
    });
    draft
}

/// The model a request asks for; `None` leaves it to Claude Code.
fn model_arg(model: RwSignal<String>) -> Option<String> {
    Some(model.get_untracked()).filter(|m| m != DEFAULT_MODEL)
}

/// Picks one of the models Claude Code offers. Until they're listed, or
/// when they can't be, only its default is offered.
#[component]
fn ModelSelect(model: RwSignal<String>) -> impl IntoView {
    let listed = LocalResource::new(api::agent_models);
    let models = Memo::new(move |_| {
        let mut models = listed.get().and_then(Result::ok).unwrap_or_default();
        if !models.iter().any(|m| m.value == DEFAULT_MODEL) {
            models.insert(
                0,
                AgentModelInfo {
                    value: DEFAULT_MODEL.into(),
                    display_name: "Default".into(),
                    description: "Claude Code's configured default".into(),
                },
            );
        }
        models
    });
    // A pick that's no longer offered goes back to the default.
    Effect::new(move |_| {
        if listed.get().is_some()
            && !models.with(|ms| model.with_untracked(|m| ms.iter().any(|i| &i.value == m)))
        {
            model.set(DEFAULT_MODEL.into());
        }
    });
    view! {
        <select
            class="agent-model-select"
            aria-label="Model"
            title="The model Claude runs with"
            prop:value=move || model.get()
            on:change=move |ev| model.set(event_target_value(&ev))
        >
            {move || models.get().into_iter().map(|m| {
                let value = m.value.clone();
                let selected = move || model.with(|v| *v == value);
                view! {
                    <option value=m.value title=m.description selected=selected>{m.display_name}</option>
                }
            }).collect_view()}
        </select>
    }
}

/// Whether a key in a prompt sends it: Enter does, Shift+Enter adds a line,
/// and Enter that picks an input method's candidate does neither.
fn submits(ev: &web_sys::KeyboardEvent) -> bool {
    ev.key() == "Enter" && !ev.shift_key() && !ev.is_composing()
}

/// The form that starts a session, filled in from the draft with id
/// `draft` and kept in it as it changes.
#[component]
pub fn NewSession(draft: String) -> impl IntoView {
    let drafts = expect_context::<Drafts>();
    let repos = LocalResource::new(api::repos);
    let saved = drafts.open(&draft);
    let repo = RwSignal::new(saved.repo);
    let number = RwSignal::new(saved.pr);
    let prompt = RwSignal::new(saved.prompt);
    let mode = RwSignal::new(saved.mode);
    let model = RwSignal::new(saved.model);
    // Drafts keep only the text; images last as long as the form.
    let images = RwSignal::new(Vec::<PromptImage>::new());
    let error = RwSignal::new(None::<String>);
    let id = StoredValue::new(draft.clone());

    Effect::new(move |_| {
        let (repo, pr, prompt, mode, model) = (
            repo.get(),
            number.get(),
            prompt.get(),
            mode.get(),
            model.get(),
        );
        drafts.update(&id.get_value(), |d| {
            *d = Draft {
                repo,
                pr,
                prompt,
                mode,
                model,
                ..d.clone()
            }
        });
    });
    // The model picked is the one the next draft starts with.
    Effect::watch(move || model.get(), |m, _, _| save_model(m), false);
    // A form left without writing anything isn't worth keeping.
    on_cleanup(move || drafts.remove_if_blank(&draft));
    let busy = RwSignal::new(false);

    // Default to the first repository once they're known.
    Effect::new(move |_| {
        if let Some(Ok(list)) = repos.get()
            && repo.with_untracked(String::is_empty)
            && let Some(first) = with_general(list).first()
        {
            repo.set(first.clone());
        }
    });
    let general = Memo::new(move |_| repo.with(|r| r == GENERAL));

    let start = move || {
        if busy.get_untracked() || prompt.with_untracked(|p| p.trim().is_empty()) {
            return;
        }
        let pr = match number.get_untracked().trim() {
            _ if general.get_untracked() => None,
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
            model: model_arg(model),
            images: images.get_untracked(),
        };
        let id = id.get_value();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::create_agent(&req).await {
                Ok(session) => {
                    drafts.remove(&id);
                    // Its follow-ups go on in the mode it started in.
                    if let Some(s) = storage() {
                        let _ = s.set_item(&mode_key(&session.id), req.permission_mode.as_str());
                    }
                    go(&agent_hash(&session.id));
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    view! {
        <div class="agents">
            <form
                class="agent-form"
                on:submit=move |ev| {
                    ev.prevent_default();
                    start();
                }
                on:dragover=images::drag_over
                on:drop=move |ev| images::drop(ev, images, error)
            >
                <h2>"New session"</h2>
                <div class="agent-form-row">
                    <label>"Repository"
                        <select
                            prop:value=move || repo.get()
                            on:change=move |ev| repo.set(event_target_value(&ev))
                        >
                            {move || repos.get().and_then(Result::ok).map(with_general).unwrap_or_default().into_iter().map(|r| {
                                let selected = repo.get_untracked() == r;
                                view! { <option value=r.clone() selected=selected>{repo_label(&r).to_owned()}</option> }
                            }).collect_view()}
                        </select>
                    </label>
                    <label>"Start from PR"
                        <input
                            type="text"
                            class="agent-pr"
                            placeholder=move || if general.get() { "" } else { "trunk" }
                            disabled=general
                            prop:value=move || number.get()
                            on:input=move |ev| number.set(event_target_value(&ev))
                        />
                    </label>
                    <label>"Permissions" <ModeSelect mode=mode /></label>
                    <label>"Model" <ModelSelect model=model /></label>
                </div>
                <textarea
                    class="agent-prompt"
                    rows="5"
                    placeholder="What should Claude do? (Enter starts, Shift+Enter for a new line)"
                    aria-label="Prompt"
                    prop:value=move || prompt.get()
                    on:input=move |ev| prompt.set(event_target_value(&ev))
                    on:paste=move |ev| images::paste(ev, images, error)
                    on:keydown=move |ev| {
                        if submits(&ev) {
                            ev.prevent_default();
                            start();
                        }
                    }
                ></textarea>
                <Attachments images=images />
                <p class="hint muted">{move || if general.get() { "The session gets an empty directory of its own, in no repository, for questions that aren't about one." } else { "The session gets its own git worktree on a new branch." }} " Claude asks here before it uses a tool, unless the permissions let it go ahead: Accept edits allows file edits, and Auto allows whatever Claude Code's classifier deems safe. Plan first has Claude look around without changing anything, then asks you to approve its plan before it goes ahead. Every prompt can pick different permissions and a different model."</p>
                {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                <div class="form-buttons">
                    <button
                        type="submit"
                        class="primary"
                        disabled=move || busy.get() || prompt.with(|p| p.trim().is_empty())
                    >
                        {move || match (busy.get(), general.get()) {
                            (false, _) => "Start",
                            (true, false) => "Creating worktree…",
                            (true, true) => "Starting…",
                        }}
                    </button>
                    <AttachButton images=images error=error />
                </div>
            </form>
        </div>
    }
}

/// Follows a session's event stream while the page is open. Events arrive
/// in `seq` order; a reconnect replays the transcript, which is skipped up
/// to the last event seen. `usage` gets the context window's tokens and
/// size as Claude Code reports them, and `model` the model it runs.
fn follow(
    id: &str,
    events: RwSignal<Vec<AgentEvent>>,
    streaming: RwSignal<String>,
    usage: RwSignal<(Option<u64>, Option<u64>)>,
    model: RwSignal<Option<String>>,
) {
    let on_message = move |data: String| {
        let Ok(event) = serde_json::from_str::<AgentEvent>(&data) else {
            return;
        };
        match event.seq {
            None => match event.kind {
                AgentEventKind::TextDelta(t) => streaming.update(|s| s.push_str(&t)),
                AgentEventKind::Usage { tokens, window } => usage.update(|(t, w)| {
                    *t = tokens.or(*t);
                    *w = window.or(*w);
                }),
                AgentEventKind::Model(m) => model.set(Some(m)),
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
    };
    // Text streamed on a connection that died is incomplete; the replay
    // brings the finished message.
    let on_open = move || streaming.set(String::new());
    crate::events::subscribe(format!("/api/agents/{id}/events"), on_message, on_open);
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
    let live_model = RwSignal::new(None);
    follow(&id, events, streaming, live_usage, live_model);
    // Approving a plan also picks the mode the follow-ups go with.
    let mode = mode_signal(&id);
    let model = model_signal();

    let terminal_id = id.clone();
    view! {
        <div class="agent-page">
            <div class="agent-panes">
                <main class="main">
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
                                let active_model = Signal::derive(move || {
                                    live_model.get().or_else(|| session.with(|s| s.model.clone()))
                                });
                                view! {
                                    <AgentHeader session=session />
                                    <Transcript session=session events=events streaming=streaming mode=mode model=model />
                                    <div class="agent-footer">
                                        <Composer session=session usage=usage active_model=active_model mode=mode model=model />
                                        <AgentActions session=session mode=mode model=model />
                                    </div>
                                }.into_any()
                            }
                        }}
                    </div>
                </main>
                // A general session's directory isn't a git worktree.
                {move || {
                    let general = current.with(|s| s.as_ref().is_some_and(|s| s.repo == GENERAL));
                    (load.get() == Load::Ready && !general).then(|| view! { <ChangesPane id=id.clone() events=events /> })
                }}
            </div>
            {move || (load.get() == Load::Ready).then(|| view! {
                <crate::terminal::TerminalPanel id=terminal_id.clone() />
            })}
        </div>
    }
}

/// The window width from which the worktree's changes fit next to the
/// transcript; `.changes-pane` in style.css uses the same.
const CHANGES_PANE_QUERY: &str = "(min-width: 1500px)";

/// How often at most the pane refetches while events stream in.
const CHANGES_THROTTLE: Duration = Duration::from_millis(500);

fn wide_enough() -> bool {
    window()
        .match_media(CHANGES_PANE_QUERY)
        .ok()
        .flatten()
        .is_some_and(|m| m.matches())
}

/// The unstaged changes in the session's worktree, next to the transcript
/// when the window has room for them. They are refetched as the session's
/// events come in, since those are when the agent changes files.
#[component]
fn ChangesPane(id: String, events: RwSignal<Vec<AgentEvent>>) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let wide = RwSignal::new(wide_enough());
    let on_resize = window_event_listener_untyped("resize", move |_| {
        let w = wide_enough();
        if wide.get_untracked() != w {
            wide.set(w);
        }
    });
    on_cleanup(move || on_resize.remove());

    let tick = RwSignal::new(0u32);
    let pending = StoredValue::new(false);
    Effect::new(move |_| {
        events.track();
        updates.agents.track();
        if !pending.get_value() {
            pending.set_value(true);
            set_timeout(
                move || {
                    pending.try_set_value(false);
                    tick.try_update(|t| *t += 1);
                },
                CHANGES_THROTTLE,
            );
        }
    });
    let changes = LocalResource::new(move || {
        tick.track();
        let (id, wide) = (id.clone(), wide.get());
        async move {
            match wide {
                true => api::agent_changes(&id).await.map(|c| Some(c.files)),
                false => Ok(None),
            }
        }
    });
    // Keeps the last changes while refetching so that the files don't
    // flicker.
    let loaded = Memo::new(
        move |prev: Option<&Option<Result<Vec<WorktreeFile>, String>>>| match changes.get() {
            Some(Ok(None)) => None,
            Some(r) => Some(r.map(Option::unwrap_or_default)),
            None => prev.cloned().flatten(),
        },
    );
    let files = Signal::derive(move || {
        loaded.with(|l| {
            l.as_ref()
                .and_then(|r| r.as_ref().ok())
                .cloned()
                .unwrap_or_default()
        })
    });

    move || {
        wide.get().then(|| {
            view! {
                <aside class="changes-pane">
                    <h2>"Unstaged changes"</h2>
                    {move || match loaded.get() {
                        None => view! { <p class="muted">"Loading…"</p> }.into_any(),
                        Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                        Some(Ok(f)) if f.is_empty() => {
                            view! { <p class="muted">"No unstaged changes."</p> }.into_any()
                        }
                        Some(Ok(f)) => view! {
                            <p class="muted">{format!("{} files changed", f.len())}</p>
                        }.into_any(),
                    }}
                    <WorktreeDiff files=files />
                </aside>
            }
        })
    }
}

#[component]
fn AgentHeader(session: Signal<AgentSession>) -> impl IntoView {
    view! {
        <header class="agent-header">
            {move || {
                let s = session.get();
                if s.repo == GENERAL {
                    return view! {
                        <h1>{s.title.clone()}</h1>
                        <p class="muted">
                            <StatusBadge status=s.status />
                            " in no repository"
                            {format!(" · {}", cost(s.cost_usd))}
                        </p>
                        <p class="muted agent-worktree"><code>{s.worktree.clone()}</code></p>
                    }
                    .into_any();
                }
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
                    </p>
                    <p class="muted agent-worktree"><code>{s.worktree.clone()}</code></p>
                }
                .into_any()
            }}
        </header>
    }
}

/// The session's wrap-up and delete buttons, under the composer so they stay
/// in reach however long the transcript gets.
#[component]
fn AgentActions(
    session: Signal<AgentSession>,
    mode: RwSignal<PermissionMode>,
    model: RwSignal<String>,
) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    // A wrap-up asked for during a turn waits for the turn, so the buttons
    // stay usable while Claude works.
    let wrapping = RwSignal::new(false);
    // Claude may get prompts to commit or to resolve conflicts first;
    // they go with the mode and model the composer has picked.
    let wrap_up = move |target| {
        let id = session.with_untracked(|s| s.id.clone());
        let req = WrapUp {
            target,
            permission_mode: mode.get_untracked(),
            model: model_arg(model),
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
        session
            .with_untracked(|s| delete_session(s, || go("#/agents"), move |e| error.set(Some(e))));
    };
    view! {
        {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
        <div class="agent-actions">
            // A general session has nothing to land.
            <Show when=move || session.with(|s| s.repo != GENERAL)>
                <button
                    class="link"
                    title="Rebase the session's commits onto the server's checkout and fast-forward it"
                    disabled=wrapping
                    on:click=move |_| wrap_up(WrapUpTarget::Checkout)
                >"Merge into checkout"</button>
                <button
                    class="link"
                    title="Push each of the session's commits to GitHub and open a stack of pull requests, one per commit"
                    disabled=wrapping
                    on:click=move |_| wrap_up(WrapUpTarget::PullRequest)
                >"Open PR"</button>
            </Show>
            <button class="link danger" on:click=delete>"Delete"</button>
        </div>
    }
}

#[component]
fn Transcript(
    session: Signal<AgentSession>,
    events: RwSignal<Vec<AgentEvent>>,
    streaming: RwSignal<String>,
    mode: RwSignal<PermissionMode>,
    model: RwSignal<String>,
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
                        images,
                    } => Some((
                        id.clone(),
                        (content.clone(), *is_error, patch.clone(), images.clone()),
                    )),
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
                    AgentEventKind::ApprovalResolved { id, allow, .. } => {
                        Some((id.clone(), *allow))
                    }
                    _ => None,
                })
                .collect::<HashMap<_, _>>()
        })
    });
    // The plan the last turn ended on, unanswered. Only it can still be
    // answered: once a server restart cuts the turn off, by starting a new
    // turn.
    let open_plan = Memo::new(move |_| {
        events.with(|events| {
            events.iter().fold(None, |plan, e| match &e.kind {
                AgentEventKind::ApprovalRequested { id, tool_name, .. }
                    if tool_name == "ExitPlanMode" =>
                {
                    Some(id.clone())
                }
                AgentEventKind::ApprovalResolved { id, .. } if plan.as_ref() == Some(id) => None,
                AgentEventKind::Prompt(_) => None,
                _ => plan,
            })
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
                        // A prompt can be only images.
                        AgentEventKind::Prompt(p) if p.trim().is_empty() => ().into_any(),
                        AgentEventKind::Prompt(p)
                        | AgentEventKind::ApprovalResolved { message: Some(p), .. } => view! {
                            <div class="turn-prompt">{p}</div>
                        }.into_any(),
                        AgentEventKind::Images(names) => view! {
                            <div class="turn-images">
                                {names.into_iter().map(|name| {
                                    let url = format!("/api/agents/{id}/images/{name}");
                                    view! {
                                        <a href=url.clone() target="_blank" rel="noopener">
                                            <img src=url.clone() alt="Image sent with the prompt" />
                                        </a>
                                    }
                                }).collect_view()}
                            </div>
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
                                    let patch = result.with(|r| r.as_ref().map(|(_, _, p, _)| p.clone()));
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
                                            Some((_, true, _, _)) => view! { <span class="error">" failed"</span> }.into_any(),
                                            Some(_) => ().into_any(),
                                        }}
                                    </summary>
                                    {body}
                                    // A successful edit's result only repeats the diff.
                                    {move || result.get()
                                        .filter(|(_, is_error, _, _)| *is_error || !is_edit)
                                        .map(|(content, is_error, _, _)| view! {
                                            <pre class="tool-result" class:error=is_error>{content}</pre>
                                        })}
                                </details>
                                // Outside the details, so that screenshots show without
                                // opening the tool call.
                                {move || {
                                    let images = result.with(|r| r.as_ref().map(|(_, _, _, i)| i.clone()));
                                    images.unwrap_or_default().into_iter().map(|image| view! {
                                        <TranscriptImage id=image caption=None />
                                    }).collect_view()
                                }}
                            }.into_any()
                        }
                        AgentEventKind::Image { id: image, caption } => view! {
                            <TranscriptImage id=image caption=caption />
                        }.into_any(),
                        AgentEventKind::ApprovalRequested { id: tool, tool_name, input } if tool_name == "ExitPlanMode" => {
                            let value: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
                            let plan = value["plan"].as_str().unwrap_or_default();
                            let open = {
                                let tool = tool.clone();
                                Signal::derive(move || open_plan.with(|p| p.as_ref() == Some(&tool)))
                            };
                            view! {
                                <PlanCard
                                    agent=id
                                    tool=tool
                                    plan=markdown::to_html(plan, &repo)
                                    resolved=resolved
                                    open=open
                                    waiting=waiting
                                    mode=mode
                                    model=model
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
                        AgentEventKind::Landed(landed) => {
                            let links = landed
                                .prs()
                                .into_iter()
                                .enumerate()
                                .map(|(i, pr)| view! {
                                    {(i > 0).then_some(", ")}
                                    <a href=pr.url target="_blank">
                                        {format!("PR #{}", pr.number)}
                                    </a>
                                })
                                .collect_view();
                            view! { <p class="turn-end wrapped-up">"Opened " {links}</p> }
                                .into_any()
                        }
                        AgentEventKind::WrapUpFailed(e) => view! {
                            <pre class="turn-error">{format!("Couldn't wrap up: {e}")}</pre>
                        }.into_any(),
                        AgentEventKind::ToolResult { .. }
                        | AgentEventKind::ApprovalResolved { .. }
                        | AgentEventKind::TextDelta(_)
                        | AgentEventKind::Usage { .. }
                        | AgentEventKind::Model(_) => ().into_any(),
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

/// An image in the transcript, which opens at full size in a new tab.
#[component]
fn TranscriptImage(id: String, caption: Option<String>) -> impl IntoView {
    let src = format!("/api/images/{id}");
    view! {
        <figure class="turn-image">
            <a href=src.clone() target="_blank">
                <img src=src.clone() alt=caption.clone().unwrap_or_default() loading="lazy" />
            </a>
            {caption.map(|c| view! { <figcaption>{c}</figcaption> })}
        </figure>
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
    /// Whether this is the plan the last turn ended on, still unanswered.
    open: Signal<bool>,
    waiting: Signal<bool>,
    /// The composer's mode, which moves on from planning with an approval.
    mode: RwSignal<PermissionMode>,
    /// The composer's model, for the turn that answers a stranded plan.
    model: RwSignal<String>,
) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let feedback = RwSignal::new(String::new());
    let then = RwSignal::new(PermissionMode::Auto);
    let state = {
        let tool = tool.clone();
        Signal::derive(move || resolved.with(|r| r.get(&tool).copied()))
    };
    let answer = move |allow: bool| {
        let message = Some(feedback.get_untracked()).filter(|f| !f.trim().is_empty());
        let next = if allow {
            then.get_untracked()
        } else {
            PermissionMode::Plan
        };
        if allow && mode.get_untracked() == PermissionMode::Plan {
            mode.set(next);
        }
        if waiting.get_untracked() {
            // Without feedback the server tells Claude to keep planning.
            let approval = Approval {
                id: tool.clone(),
                allow,
                message: message.filter(|_| !allow),
                mode: allow.then_some(next),
                answers: None,
            };
            send_approval(agent.clone(), approval, busy, error);
            return;
        }
        // The turn that asked is gone, so the answer starts a new one.
        let prompt = if allow {
            "I approve the plan. Carry it out.".to_string()
        } else {
            message.unwrap_or_else(|| "Keep planning.".to_string())
        };
        let (agent, model) = (agent.clone(), model_arg(model));
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            if let Err(e) = api::send_agent_message(&agent, prompt, Vec::new(), next, model).await {
                error.set(Some(e));
            }
            busy.set(false);
        });
    };
    let answer = StoredValue::new(answer);
    let pending = move || state.get().is_none() && open.get();
    view! {
        <div class="approval plan" class:pending=pending>
            <p>
                {move || match state.get() {
                    Some(true) => "Plan approved",
                    Some(false) => "Plan sent back",
                    None if !open.get() => "Unanswered plan",
                    None if waiting.get() => "Claude's plan is waiting for your approval",
                    None => "The turn ended before the plan was answered. Answering it continues the session.",
                }}
            </p>
            <div class="plan-text markdown" inner_html=plan></div>
            {move || pending().then(|| view! {
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
                    <button class="primary" disabled=move || busy.get() on:click=move |_| answer.with_value(|a| a(true))>"Approve plan"</button>
                    <button disabled=move || busy.get() on:click=move |_| answer.with_value(|a| a(false))>"Keep planning"</button>
                </div>
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
                message: None,
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
    active_model: Signal<Option<String>>,
    mode: RwSignal<PermissionMode>,
    model: RwSignal<String>,
) -> impl IntoView {
    let id = StoredValue::new(session.with_untracked(|s| s.id.clone()));
    let prompt = id.with_value(|id| draft_signal(id));
    let images = RwSignal::new(Vec::<PromptImage>::new());
    let error = RwSignal::new(None::<String>);
    let blank = move || prompt.with(|p| p.trim().is_empty()) && images.with(Vec::is_empty);
    let working = move || {
        session.with(|s| {
            matches!(
                s.status,
                AgentStatus::Running | AgentStatus::AwaitingApproval
            )
        })
    };

    let send = move || {
        if blank() {
            return;
        }
        let text = prompt.get_untracked();
        let attached = images.get_untracked();
        let mode = mode.get_untracked();
        let model = model_arg(model);
        spawn_local(async move {
            match api::send_agent_message(&id.get_value(), text, attached, mode, model).await {
                Ok(()) => {
                    prompt.set(String::new());
                    images.set(Vec::new());
                    error.set(None);
                }
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
        <form
            class="composer"
            on:submit=move |ev| {
                ev.prevent_default();
                send();
            }
            on:dragover=images::drag_over
            on:drop=move |ev| images::drop(ev, images, error)
        >
            <textarea
                rows="3"
                aria-label="Follow-up"
                placeholder=move || if working() { "Queue a follow-up… (Enter sends, Shift+Enter for a new line)" } else { "Reply to Claude… (Enter sends, Shift+Enter for a new line)" }
                prop:value=move || prompt.get()
                on:input=move |ev| prompt.set(event_target_value(&ev))
                on:paste=move |ev| images::paste(ev, images, error)
                on:keydown=move |ev| {
                    if submits(&ev) {
                        ev.prevent_default();
                        send();
                    }
                }
            ></textarea>
            <Attachments images=images />
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
            <div class="form-buttons">
                <ModeSelect mode=mode />
                <ModelSelect model=model />
                <button type="submit" class="primary" disabled=blank>"Send"</button>
                <AttachButton images=images error=error />
                {move || working().then(|| view! { <button type="button" on:click=stop>"Stop"</button> })}
                <div class="composer-status">
                    {move || active_model.get().map(|m| view! { <span class="agent-model" title="Model">{m}</span> })}
                    <ContextMeter usage=usage />
                </div>
            </div>
        </form>
    }
}

/// How full the context window is, at the right of the composer's buttons
/// after the model.
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
            summary: None,
            branch: String::new(),
            worktree: String::new(),
            base_sha: String::new(),
            status: AgentStatus::Idle,
            created_at: 0,
            cost_usd: 0.0,
            context_tokens: None,
            context_window: None,
            model: None,
            landed: None,
            prs: None,
            unstaged: 0,
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
            vec![
                ("o/b".into(), "d1".into()),
                ("o/new".into(), "d2".into()),
                ("o/b".into(), "d3".into()),
            ],
        );
        let ids: Vec<(&str, Vec<&str>, Vec<&str>)> = groups
            .iter()
            .map(|g| {
                (
                    g.repo.as_str(),
                    g.drafts.iter().map(String::as_str).collect(),
                    g.sessions.iter().map(|s| s.id.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            ids,
            [
                ("o/a", vec![], vec![]),
                ("o/b", vec!["d1", "d3"], vec!["1", "3"]),
                ("o/gone", vec![], vec!["2"]),
                ("o/new", vec!["d2"], vec![]),
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
