//! Claude Code sessions: a sidebar listing them by repository, a form that
//! starts one, and a session's page with its live transcript, permission
//! requests and a box for follow-up prompts.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use grenadine_core::api::{
    AgentBackend, AgentBackendInfo, AgentEvent, AgentEventKind, AgentModelInfo, AgentPr, AgentPrs,
    AgentSession, AgentStatus, Approval, ApprovalKind, ApprovalOption, CiStatus, CleanupState,
    CostUnit, GENERAL, Landed, NewAgent, NewCleanupSession, PatchHunk, PermissionMode, PrKey,
    RepoSettings, SessionKind, WorktreeFile, WrapUp, WrapUpTarget,
};
use grenadine_core::edit::{self, Fragment};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use wasm_bindgen::prelude::*;

use crate::composer::draft_signal;
use crate::diffview::{EditDiff, WorktreeDiff};
use crate::drafts::{self, Draft, Drafts};
use crate::image_drafts;
use crate::images::{self, AttachButton, Attachments};
use crate::inboxes::storage;
use crate::settings::{Settings, SettingsButton};
use crate::{GithubLink, Route, Updates, api, github_url, hash_for, highlight, markdown};

fn agent_hash(id: &str) -> String {
    format!("#/agents/{id}")
}

/// The page of the session `id`, which is `kind`.
pub fn session_hash(id: &str, kind: SessionKind) -> String {
    match kind {
        SessionKind::Interactive => agent_hash(id),
        SessionKind::AutoReview => crate::auto_reviews::hash(id),
    }
}

pub fn go(hash: &str) {
    let _ = window().location().set_hash(hash);
}

pub fn status_label(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Creating => "creating worktree",
        AgentStatus::Idle => "idle",
        AgentStatus::Running => "working…",
        AgentStatus::AwaitingApproval => "needs approval",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Failed => "failed",
    }
}

pub fn status_class(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Creating => "agent-status running",
        AgentStatus::Idle => "agent-status",
        AgentStatus::Running => "agent-status running",
        AgentStatus::AwaitingApproval => "agent-status waiting",
        AgentStatus::Interrupted => "agent-status",
        AgentStatus::Failed => "agent-status failed",
    }
}

/// `amount` in `unit`, as the page shows it.
pub fn cost(amount: f64, unit: CostUnit) -> String {
    match unit {
        CostUnit::Usd => format!("${amount:.2}"),
        CostUnit::Acu => format!("{amount:.3} ACU"),
    }
}

/// The backends the server offers, loaded once for the page; a model
/// picker loads them again when they came without models.
pub(crate) fn backends() -> LocalResource<api::Result<Vec<AgentBackendInfo>>> {
    expect_context()
}

/// The sessions, refetched when they change.
pub(crate) fn sessions() -> LocalResource<api::Result<Vec<AgentSession>>> {
    expect_context()
}

/// The models `backend` offers: `None` until the backends are in, an
/// empty list when they can't be.
pub(crate) fn listed_models(backend: Signal<AgentBackend>) -> Memo<Option<Vec<AgentModelInfo>>> {
    Memo::new(move |_| {
        let backend = backend.get();
        backends().get().map(|r| {
            r.unwrap_or_default()
                .into_iter()
                .find(|i| i.backend == backend)
                .map(|i| i.models)
                .unwrap_or_default()
        })
    })
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
fn LandedBadge(landed: Landed, repo: String) -> impl IntoView {
    let label = landed_label(&landed);
    match landed.prs().pop() {
        None => view! { <span class="agent-landed">{label}</span> }.into_any(),
        Some(top) => {
            // A session in no repository has no in-app page for its PR.
            let hash = (repo != GENERAL).then(|| {
                hash_for(&PrKey {
                    repo,
                    number: top.number,
                })
            });
            match hash {
                Some(hash) => view! {
                    <a class="agent-landed" href=hash>{label}</a>
                    <GithubLink url=top.url.clone() />
                }
                .into_any(),
                None => view! {
                    <a class="agent-landed" href=top.url target="_blank" rel="noopener noreferrer">
                        {label}
                    </a>
                }
                .into_any(),
            }
        }
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
pub fn delete_session(
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
                image_drafts::forget(&image_drafts::agent_key(&id));
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
/// while any open one conflicts with its base or has failing checks,
/// when it needs attention.
#[component]
fn PrCount(prs: AgentPrs) -> impl IntoView {
    let AgentPrs {
        merged,
        total,
        conflicting,
        failing,
    } = prs;
    let done = total > 0 && merged == total;
    let conflict = conflicting + failing > 0;
    let noun = if total == 1 { "PR" } else { "PRs" };
    let mut title = format!("{merged} of {total} {noun} merged");
    if conflicting > 0 {
        title.push_str(&format!(", {conflicting} with conflicts"));
    }
    if failing > 0 {
        title.push_str(&format!(", {failing} with failing checks"));
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
    let repos = LocalResource::new(api::repos);
    let all = sessions();
    // Reviews have a tab of their own.
    let sessions = Memo::new(move |_| {
        all.get().map(|r| {
            r.map(|list| {
                list.into_iter()
                    .filter(|s| s.kind == SessionKind::Interactive)
                    .collect::<Vec<_>>()
            })
        })
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
    // The repository whose settings are open.
    let settings = RwSignal::new(None::<String>);
    // A draft whose session started while the page was away, e.g. a
    // reload or a closed tab mid-request, becomes the session's entry.
    Effect::new(move |_| {
        let Some(Ok(list)) = sessions.get() else {
            return;
        };
        for s in &list {
            let Some(d) = &s.draft else { continue };
            if !drafts.contains(d) {
                continue;
            }
            let open = route.with_untracked(|r| matches!(r, Route::Draft(x) if *x == *d));
            adopt_start_mode(d, &s.id);
            drafts.remove(d);
            if open {
                go(&agent_hash(&s.id));
            }
        }
    });

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
                    .map(|g| view! { <RepoSessions group=g route=route error=error settings=settings /> })
                    .collect_view()
                    .into_any(),
                _ => view! { <p class="muted pad">"Loading…"</p> }.into_any(),
            }}
            <CleanupSection route=route />
            <Settings open=settings />
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
    let model = model_signal(Signal::stored(AgentBackend::Claude));
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);

    let items = move || match cleanup.get() {
        Some(Ok(c)) => c.items,
        _ => Vec::new(),
    };
    let all = move || {
        let items = items();
        !items.is_empty() && picked.with(|p| items.iter().all(|i| p.contains(&i.id)))
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
            backend: AgentBackend::Claude,
            model: model_arg(model),
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::create_cleanup_session(&req).await {
                Ok(session) => {
                    // The server starts it out planning.
                    if let Some(s) = storage() {
                        let _ = s.set_item(&mode_key(&session.id), PermissionMode::plan().as_str());
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
            {move || (!items().is_empty()).then(|| view! {
                <label class="cleanup-pick cleanup-all">
                    <input
                        type="checkbox"
                        prop:checked=all
                        prop:indeterminate=move || !all() && picked.with(|p| !p.is_empty())
                        on:change=move |ev| {
                            if event_target_checked(&ev) {
                                picked.set(items().iter().map(|i| i.id).collect());
                            } else {
                                picked.set(Vec::new());
                            }
                        }
                    />
                    "Select all"
                </label>
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
                <label>"Model" <ModelSelect model=model backend=Signal::stored(AgentBackend::Claude) /></label>
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
    settings: RwSignal<Option<String>>,
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
    let configurable = repo != GENERAL;
    view! {
        <section class="agent-repo">
            <h2 class="agent-repo-name">
                <span>{repo_label(&repo).to_owned()}</span>
                {configurable.then(|| view! { <SettingsButton repo=repo.clone() open=settings /> })}
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
                            let mut meta: Vec<String> = [a.branch.clone(), cost(a.cost, a.cost_unit)]
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
                    let label = d.backend.mode_label(d.mode.as_str());
                    let mut meta = vec![label.unwrap_or(d.mode.as_str()).to_owned()];
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
    // While its session is starting the draft can't be discarded.
    let status = {
        let id = id.clone();
        move || {
            if drafts.is_starting(&id) {
                "Starting…"
            } else {
                "Draft"
            }
        }
    };
    let discardable = {
        let id = id.clone();
        move || !drafts.is_starting(&id)
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
                    <span class="agent-status draft"><PencilIcon />{status}</span>
                    <span class="agent-meta muted">{meta}</span>
                </span>
            </a>
            <button
                class="icon agent-delete"
                class:hidden=move || !discardable()
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

/// The mode a draft's session starts in, kept until the session's page
/// can take it over: the server shows the session before the start's
/// answer reaches the page, which would otherwise lose the mode.
fn start_mode_key(draft: &str) -> String {
    format!("grenadine.agent-start-mode.{draft}")
}

/// Moves a draft's start mode onto the session it became, once: a
/// second call finds no start key and leaves what the page has stored
/// since alone.
fn adopt_start_mode(draft: &str, session: &str) {
    if let Some(s) = storage()
        && let Ok(Some(mode)) = s.get_item(&start_mode_key(draft))
    {
        let _ = s.set_item(&mode_key(session), &mode);
        let _ = s.remove_item(&start_mode_key(draft));
    }
}

/// The mode a prompt to `backend` goes with when `stored` was picked
/// last: `stored` if the backend offers it, or else its default.
fn offered_mode(backend: AgentBackend, stored: Option<&str>) -> PermissionMode {
    match stored {
        Some(m) if backend.has_mode(m) => PermissionMode::new(m),
        _ => PermissionMode::new(backend.default_mode()),
    }
}

/// The permission mode the session's next prompt goes with: the last
/// one picked for it, or the backend's default mode. `backend` is `None`
/// until the session is in; until then the stored mode is neither checked
/// nor stored again.
fn mode_signal(id: &str, backend: Signal<Option<AgentBackend>>) -> RwSignal<PermissionMode> {
    let key = mode_key(id);
    let stored = storage().and_then(|s| s.get_item(&key).ok().flatten());
    let mode = RwSignal::new(match (backend.get_untracked(), stored) {
        (Some(b), stored) => offered_mode(b, stored.as_deref()),
        (None, Some(stored)) => PermissionMode::new(stored),
        (None, None) => offered_mode(AgentBackend::default(), None),
    });
    Effect::new(move |_| {
        let Some(backend) = backend.get() else { return };
        let m = mode.get_untracked();
        let offered = offered_mode(backend, Some(m.as_str()));
        if offered != m {
            mode.set(offered);
        }
    });
    Effect::new(move |_| {
        let mode = mode.get();
        if backend.with(Option::is_some)
            && let Some(s) = storage()
        {
            let _ = s.set_item(&key, mode.as_str());
        }
    });
    mode
}

/// Picks one of `backend`'s permission modes, or with `after_plan`, one
/// a turn can go on in once a plan is approved. `picked`, if given, is
/// set once the user picks a mode.
#[component]
fn ModeSelect(
    mode: RwSignal<PermissionMode>,
    backend: Signal<AgentBackend>,
    #[prop(default = false)] after_plan: bool,
    #[prop(default = "Permissions")] label: &'static str,
    #[prop(optional)] picked: Option<RwSignal<bool>>,
) -> impl IntoView {
    view! {
        <select
            class="agent-mode"
            aria-label=label
            title=move || format!("What {} may do without asking here", backend.get().agent_name())
            prop:value=move || mode.with(|m| m.as_str().to_owned())
            on:change=move |ev| {
                mode.set(PermissionMode::new(event_target_value(&ev)));
                if let Some(picked) = picked {
                    picked.set(true);
                }
            }
        >
            {move || {
                let backend = backend.get();
                backend
                    .modes()
                    .iter()
                    .filter(move |(m, _)| !after_plan || backend.allows_after_plan(m))
                    .map(|&(value, label)| view! {
                        <option value=value selected=move || mode.with(|m| m.as_str() == value)>{label}</option>
                    })
                    .collect_view()
            }}
        </select>
    }
}

const MODEL_KEY: &str = "grenadine.agent-model";
const BACKEND_KEY: &str = "grenadine.agent-backend";

/// What the model picker shows for the backend's configured default.
pub(crate) const DEFAULT_MODEL: &str = "default";

/// The models a picker of `backend`'s models offers: `models`, a
/// "Default" entry first unless the list has one, and `picked` marked
/// "(not listed)" when the list doesn't have it.
pub(crate) fn model_options(
    mut options: Vec<AgentModelInfo>,
    backend: AgentBackend,
    picked: &str,
) -> Vec<AgentModelInfo> {
    if !options.iter().any(|m| m.value == DEFAULT_MODEL) {
        options.insert(
            0,
            AgentModelInfo {
                value: DEFAULT_MODEL.into(),
                display_name: "Default".into(),
                description: format!("{}'s configured default", backend.label()),
            },
        );
    }
    if !options.iter().any(|m| m.value == picked) {
        options.push(AgentModelInfo {
            display_name: format!("{picked} (not listed)"),
            description: format!("{} didn't list this model", backend.label()),
            value: picked.to_owned(),
        });
    }
    options
}

/// Each backend's model memory, so a model never lands on a backend that
/// doesn't offer it.
fn model_key(backend: AgentBackend) -> String {
    format!("{MODEL_KEY}.{}", backend.as_str())
}

/// The model picked last for `backend`, an `AgentModelInfo::value`; its
/// default at first.
pub(crate) fn last_model(backend: AgentBackend) -> String {
    storage()
        .and_then(|s| s.get_item(&model_key(backend)).ok().flatten())
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned())
}

fn save_model(backend: AgentBackend, model: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(&model_key(backend), model);
    }
}

/// The backend picked last; the default at first.
pub(crate) fn last_backend() -> AgentBackend {
    storage()
        .and_then(|s| s.get_item(BACKEND_KEY).ok().flatten())
        .and_then(|v| AgentBackend::parse(&v))
        .unwrap_or_default()
}

fn save_backend(backend: AgentBackend) {
    if let Some(s) = storage() {
        let _ = s.set_item(BACKEND_KEY, backend.as_str());
    }
}

/// The model the next prompt goes with: the last one picked for the
/// backend, again whenever the backend changes. `ModelSelect` saves the
/// picks.
fn model_signal(backend: Signal<AgentBackend>) -> RwSignal<String> {
    let model = RwSignal::new(last_model(backend.get_untracked()));
    Effect::watch(
        move || backend.get(),
        move |b, _, _| model.set(last_model(*b)),
        false,
    );
    model
}

/// Where a session's half-written follow-up is kept, so it survives
/// switching to another session and back.
fn draft_key(id: &str) -> String {
    format!("grenadine.agent-draft.{id}")
}

/// The model a request asks for; `None` leaves it to the backend's
/// default.
fn model_arg(model: RwSignal<String>) -> Option<String> {
    Some(model.get_untracked()).filter(|m| m != DEFAULT_MODEL)
}

/// Picks one of `backend`'s models. Until they're listed, or when they
/// can't be, its configured default is offered, and the model picked
/// last stays, marked as not listed.
#[component]
fn ModelSelect(model: RwSignal<String>, backend: Signal<AgentBackend>) -> impl IntoView {
    let models = listed_models(backend);
    // Models that couldn't be listed when the page loaded may be now.
    if models.get_untracked().is_some_and(|m| m.is_empty()) {
        backends().refetch();
    }
    let options = Memo::new(move |_| {
        model_options(
            models.get().unwrap_or_default(),
            backend.get(),
            &model.get(),
        )
    });
    // A pick that's no longer offered goes back to the default. Without
    // a list there's no telling, and the pick stays.
    Effect::new(move |_| {
        let stale = models.with(|ms| {
            ms.as_ref().is_some_and(|ms| {
                !ms.is_empty()
                    && model
                        .with_untracked(|m| m != DEFAULT_MODEL && !ms.iter().any(|i| &i.value == m))
            })
        });
        if stale {
            model.set(DEFAULT_MODEL.into());
        }
    });
    view! {
        <select
            class="agent-model-select"
            aria-label="Model"
            title=move || format!("The model {} runs with", backend.get().agent_name())
            prop:value=move || model.get()
            on:change=move |ev| {
                // Only a pick is saved, under the backend it was picked
                // for; a model reset because the backend doesn't offer it
                // isn't, so it can't overwrite another backend's pick.
                let picked = event_target_value(&ev);
                save_model(backend.get_untracked(), &picked);
                model.set(picked);
            }
        >
            {move || options.get().into_iter().map(|m| {
                let value = m.value.clone();
                let selected = move || model.with(|v| *v == value);
                view! {
                    <option value=m.value title=m.description selected=selected>{m.display_name}</option>
                }
            }).collect_view()}
        </select>
    }
}

/// How the permissions work for `backend`, for the new session form.
fn permissions_hint(backend: AgentBackend) -> String {
    let agent = backend.agent_name();
    let plan = backend
        .mode_label(PermissionMode::plan().as_str())
        .unwrap_or("Plan");
    format!(
        " {agent} asks here before it uses a tool, unless the permissions let it go ahead. \
         {plan} has {agent} look around without changing anything, then asks you to approve its \
         plan before it goes ahead. Every prompt can pick different permissions and a different \
         model."
    )
}

/// Whether a key in a prompt sends it: Enter does, Shift+Enter adds a line,
/// and Enter that picks an input method's candidate does neither.
fn submits(ev: &web_sys::KeyboardEvent) -> bool {
    ev.key() == "Enter" && !ev.shift_key() && !ev.is_composing()
}

/// The prompt a session's first turn gets: `skill` applied to `prompt`,
/// unless the prompt starts with a `/` and so names its own skill, or
/// there is none.
fn with_skill(prompt: &str, skill: Option<&str>) -> String {
    let trimmed = prompt.trim_start();
    match (skill, trimmed.starts_with('/')) {
        (Some(skill), false) => format!("/{skill} {trimmed}"),
        _ => prompt.to_owned(),
    }
}

/// The mode the repository starts a session of `backend` with; `plan`
/// until its settings say otherwise.
fn repo_default_mode(settings: Option<&RepoSettings>, backend: AgentBackend) -> PermissionMode {
    settings
        .and_then(|s| s.session_modes.get(&backend).cloned())
        .filter(|m| backend.has_mode(m.as_str()))
        .unwrap_or_else(PermissionMode::plan)
}

/// The form that starts a session, filled in from the draft with id
/// `draft` and kept in it as it changes. With `pr`, the session is
/// locked to that PR: its repository and "Start from PR" aren't offered,
/// and the draft takes them whatever it held.
#[component]
pub fn NewSession(draft: String, #[prop(optional)] pr: Option<PrKey>) -> impl IntoView {
    let drafts = expect_context::<Drafts>();
    let updates = expect_context::<Updates>();
    let repos = LocalResource::new(api::repos);
    let saved = drafts.open(&draft);
    // Drafts saved before they defaulted to `GENERAL` have no repository.
    let repo = RwSignal::new(match &pr {
        Some(key) => key.repo.clone(),
        None if saved.repo.is_empty() => GENERAL.to_owned(),
        None => saved.repo,
    });
    let number = RwSignal::new(match &pr {
        Some(key) => key.number.to_string(),
        None => saved.pr,
    });
    let prompt = RwSignal::new(saved.prompt);
    // The backend the draft picked, and the one the form goes with: a
    // backend the server doesn't offer, e.g. Devin once it can't list its
    // models, can't start the session, so the form goes with the first
    // one it does. Only a pick is kept in the draft, so a backend that's
    // missing for a while gets it back.
    let picked = RwSignal::new(saved.backend);
    let backend = Memo::new(move |_| {
        let picked = picked.get();
        match backends().get() {
            Some(Ok(list)) if !list.iter().any(|i| i.backend == picked) => {
                list.first().map_or(picked, |i| i.backend)
            }
            _ => picked,
        }
    });
    let mode = RwSignal::new(offered_mode(saved.backend, Some(saved.mode.as_str())));
    // Whether the user picked the mode; until then it follows the
    // repository's default for the backend.
    let mode_picked = RwSignal::new(saved.mode_picked);
    // The repository's session defaults, refetched when they change and
    // tagged with the repository they were fetched for; a session in no
    // repository has none, and a failed fetch counts as none.
    let settings = LocalResource::new(move || {
        updates.repo_settings.track();
        let repo = repo.get();
        async move {
            if repo == GENERAL {
                (GENERAL.to_owned(), None)
            } else {
                (repo.clone(), api::repo_settings(&repo).await.ok())
            }
        }
    });
    // The fetched repository's settings while they are the current
    // repository's; `None` while the fetch for a newly picked repository
    // is still in flight, so nothing reads the previous repository's.
    let current_settings = Memo::new(move |_| {
        settings
            .get()
            .and_then(|(fetched, s)| (fetched == repo.get()).then_some(s))
    });
    // The mode the repository starts a session of this backend with;
    // `plan` until it says otherwise.
    let default_mode = Memo::new(move |_| {
        repo_default_mode(current_settings.get().flatten().as_ref(), backend.get())
    });
    // The skill the repository starts a session's first prompt with.
    let default_skill = Memo::new(move |_| {
        current_settings
            .get()
            .flatten()
            .and_then(|s| s.session_skill)
    });
    // An unpicked mode follows the repository's default for the backend;
    // a picked one the backend doesn't offer goes back to its default.
    Effect::new(move |_| {
        let m = mode.get_untracked();
        let offered = if mode_picked.get() {
            offered_mode(backend.get(), Some(m.as_str()))
        } else {
            default_mode.get()
        };
        if offered != m {
            mode.set(offered);
        }
    });
    // The draft's model goes until the backend changes.
    let model = model_signal(backend.into());
    model.set(saved.model);
    // The images pasted so far, kept with the draft in IndexedDB.
    let images = image_drafts::draft_images(image_drafts::new_session_key(&draft));
    let error = RwSignal::new(None::<String>);
    let textarea = NodeRef::<leptos::html::Textarea>::new();
    // The prompt gets the focus, with the caret at the end of any draft text.
    Effect::new(move |_| {
        if let Some(t) = textarea.get() {
            let _ = t.focus();
            let end = t.value().encode_utf16().count() as u32;
            let _ = t.set_selection_range(end, end);
        }
    });
    let id = StoredValue::new(draft.clone());

    Effect::new(move |_| {
        let (repo, pr, prompt, backend, mode, mode_picked, model) = (
            repo.get(),
            number.get(),
            prompt.get(),
            picked.get(),
            mode.get(),
            mode_picked.get(),
            model.get(),
        );
        drafts.update(&id.get_value(), |d| {
            *d = Draft {
                repo,
                pr,
                prompt,
                backend,
                mode,
                mode_picked,
                model,
                has_images: images.with(|i| !i.is_empty()),
                ..d.clone()
            }
        });
    });
    // A form left without writing anything isn't worth keeping.
    on_cleanup(move || drafts.remove_if_blank(&draft));
    // Whether the draft's session is being started; it outlives the form,
    // which may be gone by the time the start answers.
    let busy = move || drafts.is_starting(&id.get_value());

    let general = Memo::new(move |_| repo.with(|r| r == GENERAL));

    // Whether the backends are known to be none: a start without one
    // fails, so the form says so instead.
    let none_installed = move || matches!(backends().get(), Some(Ok(list)) if list.is_empty());

    let start = move || {
        if busy() || none_installed() || prompt.with_untracked(|p| p.trim().is_empty()) {
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
        let known = current_settings.get_untracked();
        let repo = repo.get_untracked();
        let backend = backend.get_untracked();
        let mode = mode.get_untracked();
        let mode_picked = mode_picked.get_untracked();
        let raw_prompt = prompt.get_untracked();
        let model = model_arg(model);
        let images = images.get_untracked();
        let id = id.get_value();
        drafts.set_starting(&id, true);
        // The last attempt's error doesn't carry into this one.
        drafts.set_start_error(&id, None);
        error.set(None);
        spawn_local(async move {
            // A start asked for before the repository's settings arrived
            // fetches them itself, so the session gets the repository's
            // skill and mode. It lives in the task because the start
            // outlives the form.
            let settings = match known {
                Some(s) => s,
                None if repo == GENERAL => None,
                None => api::repo_settings(&repo).await.ok(),
            };
            let req = NewAgent {
                backend,
                repo,
                pr,
                prompt: with_skill(
                    &raw_prompt,
                    settings.as_ref().and_then(|s| s.session_skill.as_deref()),
                ),
                permission_mode: if mode_picked {
                    mode
                } else {
                    repo_default_mode(settings.as_ref(), backend)
                },
                model,
                images,
                draft: Some(id.clone()),
            };
            // The session shows before this answer arrives, and its page
            // would lose the mode it starts in without the start key.
            if let Some(s) = storage() {
                let _ = s.set_item(&start_mode_key(&id), req.permission_mode.as_str());
            }
            // The form may be unmounted by the time the start answers, so
            // only what outlives it is touched here.
            match api::create_agent(&req).await {
                Ok(session) => {
                    drafts.remove(&id);
                    // Its follow-ups go on in the mode it started in.
                    adopt_start_mode(&id, &session.id);
                    // Go to the session only while its draft is still
                    // open; the user may have moved on while it started.
                    if crate::current_hash() == drafts::hash(&id) {
                        go(&agent_hash(&session.id));
                    }
                }
                Err(e) => {
                    if let Some(s) = storage() {
                        let _ = s.remove_item(&start_mode_key(&id));
                    }
                    drafts.set_start_error(&id, Some(e));
                }
            }
            drafts.set_starting(&id, false);
        });
    };

    // Locked to a PR the run hints collapse behind a summary.
    let locked = pr.is_some();
    view! {
        <div class="agents">
            <form
                class="agent-form"
                class:locked=pr.is_some()
                on:submit=move |ev| {
                    ev.prevent_default();
                    start();
                }
                on:dragover=images::drag_over
                on:drop=move |ev| images::drop(ev, images, error)
            >
                <h2>"New session"</h2>
                {pr.clone().map(|key| view! {
                    <p class="muted">{format!("Starts from #{} in {}", key.number, key.repo)}</p>
                })}
                <div class="agent-form-row">
                    {pr.is_none().then(|| view! {
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
                    })}
                    {move || {
                        let list = backends().get().and_then(Result::ok).unwrap_or_default();
                        // Hidden only when there's nothing else to pick.
                        let only = matches!(list.as_slice(), [i] if i.backend == backend.get());
                        (!list.is_empty() && !only).then(|| view! {
                            <label>"Backend"
                                <select
                                    prop:value=move || backend.get().as_str()
                                    on:change=move |ev| {
                                        let b = AgentBackend::parse(&event_target_value(&ev)).unwrap_or_default();
                                        save_backend(b);
                                        picked.set(b);
                                    }
                                >
                                    {list.into_iter().map(|i| {
                                        let b = i.backend;
                                        view! {
                                            <option value=b.as_str() selected=move || backend.get() == b>{b.label()}</option>
                                        }
                                    }).collect_view()}
                                </select>
                            </label>
                        })
                    }}
                    {pr.is_none().then(|| view! {
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
                    })}
                    <label>"Permissions"
                        <ModeSelect mode=mode backend=backend.into() picked=mode_picked />
                    </label>
                    <label>"Model" <ModelSelect model=model backend=backend.into() /></label>
                </div>
                <textarea
                    node_ref=textarea
                    class="agent-prompt"
                    rows="5"
                    placeholder=move || format!("What should {} do? (Enter starts, Shift+Enter for a new line)", backend.get().agent_name())
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
                {move || {
                    let hints = view! {
                        <p class="hint muted">{move || if general.get() { "The session gets an empty directory of its own, in no repository, for questions that aren't about one." } else { "The session gets its own git worktree on a new branch." }} {move || permissions_hint(backend.get())}</p>
                        {move || default_skill.get().map(|skill| view! {
                            <p class="hint muted">
                                {format!("Starts as /{skill} …, unless the prompt starts with a /skill of its own.")}
                            </p>
                        })}
                    };
                    if locked {
                        view! {
                            <details class="session-hints">
                                <summary>{move || mode.with(|m| format!("How this session runs · {}", backend.get().mode_label(m.as_str()).unwrap_or_else(|| m.as_str())))}</summary>
                                {hints}
                            </details>
                        }.into_any()
                    } else {
                        hints.into_any()
                    }
                }}
                {move || {
                    let list = backends().get().and_then(Result::ok).unwrap_or_default();
                    let b = backend.get();
                    list.into_iter()
                        .find(|i| i.backend == b)
                        .and_then(|i| i.error)
                        .map(|e| {
                            let first = e.lines().next().unwrap_or_default().to_owned();
                            view! {
                                <p class="hint" title=e>
                                    {format!("{} couldn't list its models: {first}", b.agent_name())}
                                </p>
                            }
                        })
                }}
                {move || error.get().or_else(|| drafts.start_error(&id.get_value())).map(|e| view! { <p class="error">{e}</p> })}
                {move || none_installed().then(|| view! { <p class="error">"No agent backend is installed on the server."</p> })}
                <div class="form-buttons">
                    <button
                        type="submit"
                        class="primary"
                        disabled=move || busy() || none_installed() || prompt.with(|p| p.trim().is_empty())
                    >
                        {move || if busy() { "Starting…" } else { "Start" }}
                    </button>
                    <AttachButton images=images error=error />
                </div>
            </form>
        </div>
    }
}

/// Something that arrived over the stream and waits for the next frame.
enum Incoming {
    Event(AgentEvent),
    /// The stream (re)opened.
    Opened,
}

/// How a batch changes the text being streamed.
enum Streaming {
    Keep,
    Replace(String),
    Append(String),
}

/// What the queued messages change, applied in one update per signal.
struct Batch {
    events: Vec<Arc<AgentEvent>>,
    streaming: Streaming,
    usage: Option<(Option<u64>, Option<u64>)>,
    model: Option<String>,
    /// The newest `at` the messages carried, stored or ephemeral.
    last_event: Option<i64>,
}

/// Turns queued stream messages into the signals' next values, as though
/// each had been applied in order. `last_seq` is the transcript's last
/// seq: a stored event repeats from there, so both it and a seq repeated
/// inside the batch are skipped. A stored event or a reopen ends the
/// text being streamed: deltas before it are discarded.
fn drain(queue: Vec<Incoming>, last_seq: Option<u64>) -> Batch {
    let mut batch = Batch {
        events: Vec::new(),
        streaming: Streaming::Keep,
        usage: None,
        model: None,
        last_event: None,
    };
    let mut last = last_seq;
    // Text appended since the last clear.
    let mut delta = String::new();
    let mut cleared = false;
    for incoming in queue {
        match incoming {
            Incoming::Opened => {
                delta.clear();
                cleared = true;
            }
            Incoming::Event(event) => {
                if let Some(at) = event.at {
                    batch.last_event = Some(at);
                }
                match event.seq {
                    None => match event.kind {
                        AgentEventKind::TextDelta(t) => delta.push_str(&t),
                        AgentEventKind::Usage { tokens, window } => {
                            let (t, w) = batch.usage.get_or_insert((None, None));
                            *t = tokens.or(*t);
                            *w = window.or(*w);
                        }
                        AgentEventKind::Model(m) => batch.model = Some(m),
                        _ => {}
                    },
                    Some(seq) => {
                        if last.is_some_and(|l| seq <= l) {
                            continue;
                        }
                        last = Some(seq);
                        // The streamed text is complete once anything else
                        // comes along.
                        delta.clear();
                        cleared = true;
                        batch.events.push(Arc::new(event));
                    }
                }
            }
        }
    }
    batch.streaming = match (cleared, delta.is_empty()) {
        (false, false) => Streaming::Append(delta),
        (false, true) => Streaming::Keep,
        (true, _) => Streaming::Replace(delta),
    };
    batch
}

/// Follows a session's event stream while the page is open. Events arrive
/// in `seq` order; a reconnect replays the transcript, which is skipped up
/// to the last event seen. `usage` gets the context window's tokens and
/// size as Claude Code reports them, `model` the model it runs, and
/// `last_event` the time of the newest event seen.
///
/// Messages queue up and flush once per frame, so replaying a long
/// transcript is one update per signal per frame rather than one per
/// stored event.
fn follow(
    id: &str,
    events: RwSignal<Vec<Arc<AgentEvent>>>,
    streaming: RwSignal<String>,
    usage: RwSignal<(Option<u64>, Option<u64>)>,
    model: RwSignal<Option<String>>,
    last_event: RwSignal<Option<i64>>,
) {
    let queue = StoredValue::new(Vec::<Incoming>::new());
    let scheduled = StoredValue::new(false);
    let flush = move || {
        scheduled.set_value(false);
        let mut pending = Vec::new();
        queue.update_value(|q| std::mem::swap(q, &mut pending));
        if pending.is_empty() {
            return;
        }
        let last = events.with_untracked(|e| e.last().and_then(|e| e.seq));
        let batch = drain(pending, last);
        if !batch.events.is_empty() {
            events.update(|e| e.extend(batch.events));
        }
        match batch.streaming {
            Streaming::Keep => {}
            Streaming::Replace(s) => streaming.set(s),
            Streaming::Append(t) => streaming.update(|s| s.push_str(&t)),
        }
        if let Some((tokens, window)) = batch.usage {
            usage.update(|(t, w)| {
                *t = tokens.or(*t);
                *w = window.or(*w);
            });
        }
        if let Some(m) = batch.model {
            model.set(Some(m));
        }
        if batch.last_event.is_some() && last_event.get_untracked() != batch.last_event {
            last_event.set(batch.last_event);
        }
    };
    let push = move |incoming: Incoming| {
        queue.update_value(|q| q.push(incoming));
        if !scheduled.get_value() {
            scheduled.set_value(true);
            request_animation_frame(flush);
        }
    };
    let on_message = move |data: String| {
        let Ok(event) = serde_json::from_str::<AgentEvent>(&data) else {
            return;
        };
        push(Incoming::Event(event));
    };
    // Text streamed on a connection that died is incomplete; the replay
    // brings the finished message.
    let on_open = move || push(Incoming::Opened);
    crate::events::subscribe(
        format!("/api/agents/{id}/events").into(),
        on_message,
        on_open,
    );
}

/// `command` without a leading `cd` into `worktree`, which changes nothing
/// because commands already run there.
fn strip_cd<'a>(command: &'a str, worktree: &str) -> &'a str {
    if worktree.is_empty() {
        return command;
    }
    let Some(rest) = command.trim_start().strip_prefix("cd") else {
        return command;
    };
    if !rest.starts_with(char::is_whitespace) {
        return command;
    }
    let rest = rest.trim_start();
    let (dir, rest) = if let Some(r) = rest.strip_prefix('"') {
        match r.split_once('"') {
            Some(parts) => parts,
            None => return command,
        }
    } else if let Some(r) = rest.strip_prefix('\'') {
        match r.split_once('\'') {
            Some(parts) => parts,
            None => return command,
        }
    } else {
        match rest.find(char::is_whitespace) {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        }
    };
    if dir != worktree && dir.strip_suffix('/') != Some(worktree) {
        return command;
    }
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix("&&").or_else(|| rest.strip_prefix(';')) else {
        return command;
    };
    let rest = rest.trim_start();
    if rest.is_empty() { command } else { rest }
}

/// `path` relative to `worktree` when it is inside it, or the path unchanged.
/// `worktree` may end in a `/`. A `path` equal to `worktree` becomes "."; a
/// path outside `worktree`, including a sibling sharing a prefix like
/// "<worktree>-other", stays absolute. The result never has `..` or a `./`.
fn relative_to<'a>(path: &'a str, worktree: &str) -> &'a str {
    let worktree = worktree.trim_end_matches('/');
    if worktree.is_empty() {
        return path;
    }
    if path == worktree {
        return ".";
    }
    match path
        .strip_prefix(worktree)
        .and_then(|r| r.strip_prefix('/'))
    {
        Some(rest) if !rest.is_empty() => rest,
        Some(_) => ".",
        None => path,
    }
}

/// The full path for a summary that `relative_to` shortened, so a tooltip can
/// show it; `None` when the summary shows the path or the input has no path.
/// A file tool's `file_path` wins over a search tool's `path`.
fn summary_title(input: &serde_json::Value, summary: &str) -> Option<String> {
    if let Some(path) = input["file_path"].as_str() {
        return (path != summary).then(|| path.to_owned());
    }
    input["path"]
        .as_str()
        .filter(|path| !summary.contains(*path))
        .map(str::to_owned)
}

/// A one-line summary of a tool call's input, e.g. the command or the path.
/// `worktree` hides a redundant leading `cd` into it in a `Bash` command and
/// shortens a file tool's or a search tool's path inside it.
fn tool_summary(name: &str, input: &serde_json::Value, worktree: Option<&str>) -> String {
    let field = match name {
        "Bash" => "command",
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => "file_path",
        "Grep" | "Glob" | "find_file_by_name" => {
            let pattern = input["query"]
                .as_str()
                .or_else(|| input["pattern"].as_str());
            let Some(pattern) = pattern else {
                return String::new();
            };
            let mut summary = pattern.lines().next().unwrap_or_default().to_owned();
            let path = input["path"].as_str().unwrap_or_default();
            let path = match worktree {
                Some(worktree) => relative_to(path, worktree),
                None => path,
            };
            if !path.is_empty() && path != "." {
                summary.push_str(" in ");
                summary.push_str(path);
            }
            let filter = input["file_pattern"]
                .as_str()
                .or_else(|| input["glob"].as_str())
                .unwrap_or_default();
            if !filter.is_empty() {
                summary.push_str(" (");
                summary.push_str(filter);
                summary.push(')');
            }
            return summary;
        }
        "WebFetch" | "webfetch" => "url",
        "WebSearch" | "web_search" => "query",
        "Task" | "Agent" => "description",
        "run_subagent" => "title",
        "Skill" | "skill" => "skill",
        "ExitPlanMode" => "plan",
        "AskUserQuestion" => {
            return question_parts(
                input["questions"][0]["question"]
                    .as_str()
                    .unwrap_or_default(),
            )
            .0
            .to_owned();
        }
        _ => "",
    };
    match input.get(field).and_then(|v| v.as_str()) {
        Some(s) => {
            let s = match worktree {
                Some(worktree) if name == "Bash" => strip_cd(s, worktree),
                Some(worktree) if field == "file_path" => relative_to(s, worktree),
                _ => s,
            };
            s.lines().next().unwrap_or_default().to_owned()
        }
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

/// A Bash call's command and, when the call carries other inputs, a line
/// naming them, `key: value` pairs joined by " · ". Strings are bare;
/// other values are compact JSON.
fn bash_view(input: &serde_json::Value) -> Option<(String, Option<String>)> {
    let object = input.as_object()?;
    let command = object.get("command")?.as_str()?.to_owned();
    let fields: Vec<String> = object
        .iter()
        .filter(|(key, _)| key.as_str() != "command")
        .map(|(key, value)| match value.as_str() {
            Some(s) => format!("{key}: {s}"),
            None => format!(
                "{key}: {}",
                serde_json::to_string(value).unwrap_or_default()
            ),
        })
        .collect();
    let fields = (!fields.is_empty()).then(|| fields.join(" · "));
    Some((command, fields))
}

/// A tool call's input. A Bash command is highlighted as bash, with a
/// toggle for the raw input; anything else is the input pretty-printed
/// and highlighted as JSON.
#[component]
fn ToolInput(name: String, input: String) -> impl IntoView {
    let json = {
        let text = pretty(&input);
        highlight::highlight_code("json", &text).unwrap_or_else(|| highlight::escape(&text))
    };
    let value: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
    let view = (name == "Bash").then(|| bash_view(&value)).flatten();
    let Some((command, fields)) = view else {
        return view! { <pre class="tool-input" inner_html=json></pre> }.into_any();
    };
    let raw = RwSignal::new(false);
    let command =
        highlight::highlight_code("bash", &command).unwrap_or_else(|| highlight::escape(&command));
    view! {
        <div class="tool-input-wrap">
            <button
                type="button"
                class="tool-input-toggle"
                aria-pressed=move || raw.get().to_string()
                on:click=move |_| raw.update(|r| *r = !*r)
            >
                "JSON"
            </button>
            {move || {
                if raw.get() {
                    view! { <pre class="tool-input" inner_html=json.clone()></pre> }.into_any()
                } else {
                    view! {
                        <pre class="tool-input" inner_html=command.clone()></pre>
                        {fields.clone().map(|f| view! { <p class="tool-fields muted">{f}</p> })}
                    }
                        .into_any()
                }
            }}
        </div>
    }
    .into_any()
}

/// Where loading the session stands.
#[derive(Clone, Debug, PartialEq)]
enum Load {
    Loading,
    Failed(String),
    Missing,
    Ready,
}

/// A session's page. A review's page is `read_only`: nobody prompts it,
/// so it has no composer, and the session can only be stopped or
/// deleted.
#[component]
pub fn AgentView(
    id: String,
    #[prop(optional)] read_only: bool,
    /// Fits the session into a narrow sidebar: a slim header, no changes
    /// pane and no terminal panel.
    #[prop(optional)]
    compact: bool,
) -> impl IntoView {
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
    let events = RwSignal::new(Vec::<Arc<AgentEvent>>::new());
    let streaming = RwSignal::new(String::new());
    let live_usage = RwSignal::new((None, None));
    let live_model = RwSignal::new(None);
    let last_event = RwSignal::new(None::<i64>);
    follow(&id, events, streaming, live_usage, live_model, last_event);
    // Approving a plan also picks the mode the follow-ups go with.
    let backend = Signal::derive(move || current.with(|s| s.as_ref().map(|s| s.backend)));
    let mode = mode_signal(&id, backend);
    let model = model_signal(Signal::derive(move || backend.get().unwrap_or_default()));
    // Whether the session is a general one, so the pane below only
    // remounts when that flips, not on every session update.
    let general =
        Memo::new(move |_| current.with(|s| s.as_ref().is_some_and(|s| s.repo == GENERAL)));
    // The session's repo for the pane below, likewise narrow; a
    // session's repo never changes.
    let repo = Memo::new(move |_| {
        current.with(|s| s.as_ref().map(|s| s.repo.clone()).unwrap_or_default())
    });

    // The latest prompt starts the current turn: the activity bar times
    // it and the transcript's timers cover only calls after it. The
    // pointer comparison keeps a new event from resending the same one.
    let turn = Memo::new_with_compare(
        move |_| {
            events.with(|events| {
                events
                    .iter()
                    .rev()
                    .find(|e| matches!(e.kind, AgentEventKind::Prompt(_)))
                    .cloned()
            })
        },
        |prev, cur| match (prev, cur) {
            (Some(Some(p)), Some(Some(c))) => !Arc::ptr_eq(p, c),
            (Some(None), Some(None)) => false,
            _ => true,
        },
    );

    // The page's clock for the timers that show how long the agent has
    // been at something; it only ticks while it is working.
    let now = RwSignal::new(js_sys::Date::now() as i64);
    Effect::new(move |_| {
        let working = current.with(|s| {
            s.as_ref().is_some_and(|s| {
                matches!(
                    s.status,
                    AgentStatus::Creating | AgentStatus::Running | AgentStatus::AwaitingApproval
                )
            })
        });
        if !working {
            return;
        }
        let tick = move || now.set(js_sys::Date::now() as i64);
        tick();
        let handle = set_interval_with_handle(tick, Duration::from_secs(1));
        on_cleanup(move || {
            if let Ok(h) = handle {
                h.clear();
            }
        });
    });

    // A session shows on the page of its kind; a view embedded in
    // another page doesn't redirect that page.
    Effect::new(move || {
        if compact {
            return;
        }
        if let Some(s) = current.get() {
            let review = s.kind == SessionKind::AutoReview;
            if review != read_only {
                let _ = window().location().replace(&session_hash(&s.id, s.kind));
            }
        }
    });
    let terminal_id = id.clone();
    view! {
        <div class="agent-page" class:compact=compact>
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
                                    {if compact {
                                        view! {
                                            <header class="agent-header compact">
                                                {move || {
                                                    let s = session.get();
                                                    let title = s.summary.clone().or(Some(s.title.clone()))
                                                        .filter(|t| !t.is_empty())
                                                        .unwrap_or_else(|| "New session".into());
                                                    view! {
                                                        <h1>{title}</h1>
                                                        <p class="muted">
                                                            <StatusBadge status=s.status />
                                                            " "
                                                            <a href=session_hash(&s.id, s.kind)>"Open in Agents tab"</a>
                                                        </p>
                                                    }
                                                }}
                                            </header>
                                        }.into_any()
                                    } else {
                                        view! { <AgentHeader session=session /> }.into_any()
                                    }}
                                    <Transcript session=session events=events streaming=streaming mode=mode model=model turn=turn now=now />
                                    <div class="agent-footer">
                                        <ActivityBar session=session events=events streaming=streaming last_event=last_event turn=turn now=now />
                                        {if read_only {
                                            view! { <ReviewActions session=session /> }.into_any()
                                        } else {
                                            view! {
                                                <Composer session=session usage=usage active_model=active_model mode=mode model=model />
                                                <AgentActions session=session mode=mode model=model compact=compact />
                                            }.into_any()
                                        }}
                                    </div>
                                }.into_any()
                            }
                        }}
                    </div>
                </main>
                // A general session's directory isn't a git worktree, and a
                // compact view has no room for the changes.
                {move || {
                    (load.get() == Load::Ready && !general.get() && !compact).then(|| view! { <ChangesPane id=id.clone() repo=repo.get_untracked() events=events /> })
                }}
            </div>
            {move || (load.get() == Load::Ready && !compact).then(|| view! {
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
fn ChangesPane(id: String, repo: String, events: RwSignal<Vec<Arc<AgentEvent>>>) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let session = id.clone();
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
                    <AssociatedPrs id=session.clone() repo=repo.clone() />
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
                    <WorktreeDiff files=files session=session.clone() />
                </aside>
            }
        })
    }
}

/// The icon class and tooltip of a PR a session opened. A queued PR
/// shows as queued before a conflict does, and a conflict outranks the
/// checks; a closed PR shows none of them.
fn pr_state(pr: &AgentPr) -> (&'static str, &'static str) {
    match pr.state.as_str() {
        "MERGED" => ("merged", "Merged"),
        "CLOSED" => ("closed", "Closed without merging"),
        _ if pr.queued => ("queued", "In the merge queue"),
        _ if pr.conflicting => ("conflict", "Conflicts with its base"),
        _ => match pr.ci {
            Some(CiStatus::Failing) => ("failing", "Checks failing"),
            Some(CiStatus::Running) => ("running", "Checks running"),
            Some(CiStatus::Passing) => ("passing", "Checks passing"),
            None => ("open", "No checks"),
        },
    }
}

/// The PRs the session started from or opened, above the unstaged
/// changes. Nothing renders until the first fetch comes back with at
/// least one.
#[component]
fn AssociatedPrs(id: String, repo: String) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let prs = LocalResource::new(move || {
        updates.agents.track();
        let id = id.clone();
        async move { api::agent_prs(&id).await }
    });
    // Keeps the last list while refetching so that the rows don't
    // flicker.
    let loaded = Memo::new(move |prev: Option<&Vec<AgentPr>>| {
        prs.get()
            .and_then(|r| r.ok())
            .or_else(|| prev.cloned())
            .unwrap_or_default()
    });

    move || {
        let list = loaded.get();
        (!list.is_empty()).then(|| {
            view! {
                <section class="associated-prs">
                    <h2>"Associated PRs"</h2>
                    <ul>
                        {list
                            .into_iter()
                            .map(|pr| {
                                let (class, tooltip) = pr_state(&pr);
                                let label = match pr.key.repo == repo {
                                    true => format!("#{}", pr.key.number),
                                    false => format!("{}#{}", pr.key.repo, pr.key.number),
                                };
                                view! {
                                    <li>
                                        <a href=hash_for(&pr.key)>
                                            <span class=format!("pr-state {class}") title=tooltip>
                                                <PrIcon />
                                            </span>
                                            {label} " " {pr.title.clone().unwrap_or_default()}
                                        </a>
                                        <GithubLink url=github_url(&pr.key) />
                                    </li>
                                }
                            })
                            .collect_view()}
                    </ul>
                </section>
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
                            {format!(" · {}", cost(s.cost, s.cost_unit))}
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
                        view! {
                            <a href=hash_for(&key)>{format!("{}#{n}", s.repo)}</a>
                            <GithubLink url=github_url(&key) />
                        }.into_any()
                    }
                    None => view! { <span>{format!("{} trunk", s.repo)}</span> }.into_any(),
                };
                view! {
                    <h1>{s.title.clone()}</h1>
                    <p class="muted">
                        <StatusBadge status=s.status />
                        " from " {from}
                        // Empty while the worktree is being created.
                        {(!s.base_sha.is_empty()).then(|| {
                            view! { " at " <code>{s.base_sha.chars().take(8).collect::<String>()}</code> }
                        })}
                        " on " <code>{s.branch.clone()}</code>
                        {format!(" · {}", cost(s.cost, s.cost_unit))}
                        {s.landed.clone().map(|l| view! { " · " <LandedBadge landed=l repo=s.repo.clone() /> })}
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
    /// Whether the session shows embedded in another page: deleting it
    /// mustn't navigate that page away.
    #[prop(optional)]
    compact: bool,
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
        session.with_untracked(|s| {
            if compact {
                delete_session(s, || {}, move |e| error.set(Some(e)));
            } else {
                delete_session(s, || go("#/agents"), move |e| error.set(Some(e)));
            }
        });
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

/// What a review's page offers instead of the composer: stopping the
/// review and deleting the session.
#[component]
fn ReviewActions(session: Signal<AgentSession>) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let busy = move || {
        session.with(|s| {
            matches!(
                s.status,
                AgentStatus::Creating | AgentStatus::Running | AgentStatus::AwaitingApproval
            )
        })
    };
    let stop = move |_| {
        let id = session.with_untracked(|s| s.id.clone());
        spawn_local(async move {
            if let Err(e) = api::interrupt_agent(&id).await {
                error.set(Some(e));
            }
        });
    };
    let delete = move |_| {
        session.with_untracked(|s| {
            delete_session(
                s,
                || go(crate::auto_reviews::LIST),
                move |e| error.set(Some(e)),
            )
        });
    };
    view! {
        {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
        <div class="agent-actions">
            <span class="muted review-note">
                "This review runs unattended; tools that need approval are denied."
            </span>
            <Show when=busy>
                <button class="link" on:click=stop>"Stop"</button>
            </Show>
            <button class="link danger" on:click=delete>"Delete"</button>
        </div>
    }
}

/// The events to show, without the repeated ends of a turn that older
/// servers stored: one for each background task that finished after the
/// answer.
fn shown(events: &[Arc<AgentEvent>]) -> Vec<Arc<AgentEvent>> {
    let ended = |e: &Arc<AgentEvent>| matches!(e.kind, AgentEventKind::TurnEnded { .. });
    events
        .iter()
        .enumerate()
        .filter(|&(i, e)| {
            !(matches!(e.kind, AgentEventKind::TurnEnded { error: None, .. })
                && i > 0
                && ended(&events[i - 1]))
        })
        .map(|(_, e)| e.clone())
        .collect()
}

/// The pieces of a `ToolResult` event's `kind`, when it is one.
fn tool_result(e: &AgentEvent) -> Option<(&str, bool, &[PatchHunk], &[String])> {
    let AgentEventKind::ToolResult {
        content,
        is_error,
        patch,
        images,
        ..
    } = &e.kind
    else {
        return None;
    };
    Some((content, *is_error, patch, images))
}

/// A duration in milliseconds as the page shows it: `42s` under a
/// minute, `1m 42s` under an hour, `1h 05m` past it.
fn elapsed(ms: i64) -> String {
    let secs = (ms / 1000).max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, secs % 3600 / 60)
    }
}

/// How long a finished tool call took, when it's worth showing: both ends
/// timed and ten seconds or more.
fn tool_duration(start: Option<i64>, end: Option<i64>) -> Option<i64> {
    let took = end? - start?;
    (took >= 10_000).then_some(took)
}

/// What the activity bar shows the agent doing.
#[derive(Clone, Debug, PartialEq)]
enum Activity {
    /// A top-level tool call has no result yet; `more` counts the other
    /// unfinished calls.
    Tool {
        name: String,
        summary: String,
        since: Option<i64>,
        more: usize,
    },
    /// The newest event is a subagent's tool call.
    Subagent {
        name: String,
        summary: String,
        since: Option<i64>,
    },
    /// Text is streaming in.
    Writing { since: Option<i64> },
    /// Nothing else is going on.
    Thinking { since: Option<i64> },
}

/// Picks what the agent is doing from its transcript. `last_event` is the
/// newest stored event's time. `worktree` hides a redundant leading `cd`
/// into it in `Bash` summaries. Only calls since the latest prompt count
/// as the turn's; a subagent's calls never get results, so they can't be
/// unfinished `Tool`s.
fn current_activity(
    events: &[Arc<AgentEvent>],
    streaming: bool,
    last_event: Option<i64>,
    worktree: &str,
) -> Activity {
    let turn = events
        .iter()
        .rposition(|e| matches!(e.kind, AgentEventKind::Prompt(_)))
        .map(|i| i + 1)
        .unwrap_or(0);
    let results: Vec<&str> = events[turn..]
        .iter()
        .filter_map(|e| match &e.kind {
            AgentEventKind::ToolResult { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let unfinished: Vec<&Arc<AgentEvent>> = events[turn..]
        .iter()
        .filter(|e| {
            matches!(&e.kind, AgentEventKind::ToolUse { id, parent: None, .. }
                if !results.contains(&id.as_str()))
        })
        .collect();
    let summary = |name: &str, input: &str| {
        tool_summary(
            name,
            &serde_json::from_str(input).unwrap_or_default(),
            Some(worktree),
        )
    };
    if let Some(e) = unfinished.last() {
        let AgentEventKind::ToolUse { name, input, .. } = &e.kind else {
            unreachable!()
        };
        return Activity::Tool {
            name: name.clone(),
            summary: summary(name, input),
            since: e.at,
            more: unfinished.len() - 1,
        };
    }
    if streaming {
        return Activity::Writing { since: last_event };
    }
    if let Some(e) = events.last()
        && let AgentEventKind::ToolUse {
            name,
            input,
            parent: Some(_),
            ..
        } = &e.kind
    {
        return Activity::Subagent {
            name: name.clone(),
            summary: summary(name, input),
            since: e.at,
        };
    }
    Activity::Thinking {
        since: events.iter().rev().find_map(|e| e.at).or(last_event),
    }
}

/// The line above the composer that shows what the agent is doing, while
/// it works or waits for an approval. Everything it reads from the
/// transcript sits behind a memo, so the `now` tick only reformats the
/// times rather than rescanning it.
#[component]
fn ActivityBar(
    session: Signal<AgentSession>,
    events: RwSignal<Vec<Arc<AgentEvent>>>,
    streaming: RwSignal<String>,
    last_event: RwSignal<Option<i64>>,
    turn: Memo<Option<Arc<AgentEvent>>>,
    now: RwSignal<i64>,
) -> impl IntoView {
    // Whether text is streaming in; a delta alone doesn't recompute the
    // activity below.
    let streaming_text = Memo::new(move |_| streaming.with(|s| !s.is_empty()));
    let stored_at = Memo::new(move |_| events.with(|e| e.last().and_then(|e| e.at)));
    let activity = Memo::new(move |_| {
        let worktree = session.with(|s| s.worktree.clone());
        events.with(|e| current_activity(e, streaming_text.get(), stored_at.get(), &worktree))
    });
    let approval_at = Memo::new(move |_| {
        events.with(|events| {
            events
                .iter()
                .rev()
                .find(|e| matches!(e.kind, AgentEventKind::ApprovalRequested { .. }))
                .and_then(|e| e.at)
        })
    });
    let turn_at = Signal::derive(move || turn.get().and_then(|e| e.at));
    view! {
        {move || {
            match session.with(|s| s.status) {
                AgentStatus::Creating => {
                    view! {
                        <p class="activity">
                            <span class="spinner"></span>
                            "Creating worktree…"
                        </p>
                    }.into_any()
                }
                AgentStatus::AwaitingApproval => {
                    let waited = approval_at.get().map(|at| elapsed(now.get() - at));
                    view! {
                        <p class="activity waiting">
                            "Waiting for your approval"
                            {waited.map(|d| format!(" · {d}"))}
                        </p>
                    }.into_any()
                }
                AgentStatus::Running => {
                    let activity = activity.get();
                    let mut parts = Vec::new();
                    let since = match &activity {
                        Activity::Tool { name, summary, since, more } => {
                            let mut label = name.clone();
                            if !summary.is_empty() {
                                label = format!("{label} · {summary}");
                            }
                            if *more > 0 {
                                label = format!("{label} +{more} more");
                            }
                            parts.push(label);
                            *since
                        }
                        Activity::Subagent { name, summary, since } => {
                            let mut label = format!("Subagent · {name}");
                            if !summary.is_empty() {
                                label = format!("{label} · {summary}");
                            }
                            parts.push(label);
                            *since
                        }
                        Activity::Writing { since } => {
                            parts.push("Writing…".to_owned());
                            *since
                        }
                        Activity::Thinking { since } => {
                            parts.push("Thinking…".to_owned());
                            *since
                        }
                    };
                    if let Some(d) = since.map(|at| elapsed(now.get() - at)) {
                        parts.push(d);
                    }
                    if let Some(d) = turn_at.get().map(|at| elapsed(now.get() - at)) {
                        parts.push(format!("turn {d}"));
                    }
                    if let Some(ago) = last_event.get() {
                        let quiet = now.get() - ago;
                        if quiet >= 30_000 {
                            parts.push(format!("last event {} ago", elapsed(quiet)));
                        }
                    }
                    view! {
                        <p class="activity">
                            <span class="spinner"></span>
                            {parts.join(" · ")}
                        </p>
                    }.into_any()
                }
                _ => ().into_any(),
            }
        }}
    }
}

#[component]
fn Transcript(
    session: Signal<AgentSession>,
    events: RwSignal<Vec<Arc<AgentEvent>>>,
    streaming: RwSignal<String>,
    mode: RwSignal<PermissionMode>,
    model: RwSignal<String>,
    turn: Memo<Option<Arc<AgentEvent>>>,
    now: RwSignal<i64>,
) -> impl IntoView {
    // Tool results and approval answers are shown with the event they
    // belong to. The map compares by pointer rather than by content, so a
    // rebuild doesn't re-read every result's text.
    let results = Memo::new_with_compare(
        move |_| {
            events.with(|events| {
                events
                    .iter()
                    .filter_map(|e| match &e.kind {
                        AgentEventKind::ToolResult { id, .. } => Some((id.clone(), e.clone())),
                        _ => None,
                    })
                    .collect::<HashMap<_, _>>()
            })
        },
        |prev, cur| match (prev, cur) {
            (None, None) => false,
            (Some(p), Some(c)) => {
                p.len() != c.len()
                    || c.iter()
                        .any(|(id, e)| p.get(id).is_none_or(|o| !Arc::ptr_eq(o, e)))
            }
            _ => true,
        },
    );
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
    // Calls from before the current turn that have no result belong to
    // interrupted turns, so they don't get a running timer.
    let turn_start = Signal::derive(move || turn.get().and_then(|e| e.seq));
    // The page may hear of a request before it refetches the status.
    let waiting = Signal::derive(move || {
        session.with(|s| {
            matches!(
                s.status,
                AgentStatus::Creating | AgentStatus::Running | AgentStatus::AwaitingApproval
            )
        })
    });
    let (repo, id, unit, backend, worktree) = session.with_untracked(|s| {
        (
            s.repo.clone(),
            s.id.clone(),
            s.cost_unit,
            s.backend,
            s.worktree.clone(),
        )
    });

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
                each=move || events.with(|events| shown(events))
                key=|e| e.seq
                children=move |e| {
                    let repo = repo.clone();
                    let id = id.clone();
                    let at = e.at;
                    let seq = e.seq;
                    match &e.kind {
                        // A prompt can be only images.
                        AgentEventKind::Prompt(p) if p.trim().is_empty() => ().into_any(),
                        AgentEventKind::Prompt(p)
                        | AgentEventKind::ApprovalResolved { message: Some(p), .. } => view! {
                            <div class="turn-prompt">{p.clone()}</div>
                        }.into_any(),
                        AgentEventKind::Images(names) => view! {
                            <div class="turn-images">
                                {names.iter().map(|name| {
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
                            <div class="turn-text markdown" inner_html=markdown::to_html(t, &repo)></div>
                        }.into_any(),
                        AgentEventKind::ToolUse { parent: Some(_), .. } => ().into_any(),
                        AgentEventKind::ToolUse { id: tool, name, input, .. } => {
                            let value: serde_json::Value = serde_json::from_str(input).unwrap_or_default();
                            let summary = tool_summary(name, &value, Some(&worktree));
                            let title = summary_title(&value, &summary);
                            let tool = tool.clone();
                            let result = Memo::new_with_compare(
                                move |_| results.with(|r| r.get(&tool).cloned()),
                                |prev, cur| match (prev, cur) {
                                    (Some(Some(p)), Some(Some(c))) => !Arc::ptr_eq(p, c),
                                    (Some(None), Some(None)) => false,
                                    _ => true,
                                },
                            );
                            let is_edit = edit_fragments(name, &value, &[]).is_some();
                            let body = {
                                let name = name.clone();
                                let input = input.clone();
                                move || {
                                    let patch = result.with(|r| {
                                        r.as_ref()
                                            .and_then(|e| tool_result(e))
                                            .map(|(_, _, p, _)| p.to_vec())
                                    });
                                    match edit_fragments(&name, &value, &patch.unwrap_or_default()) {
                                        Some((path, fragments)) => view! { <EditDiff path=path fragments=fragments /> }.into_any(),
                                        None => view! { <ToolInput name=name.clone() input=input.clone() /> }.into_any(),
                                    }
                                }
                            };
                            view! {
                                <details class="tool">
                                    <summary>
                                        <span class="tool-name">{name.clone()}</span>
                                        " " <code title=title>{summary}</code>
                                        {move || {
                                            let done = result.with(|r| {
                                                r.as_ref().map(|e| {
                                                    (
                                                        tool_result(e)
                                                            .map(|(_, is_error, _, _)| is_error),
                                                        e.at,
                                                    )
                                                })
                                            });
                                            match done {
                                                // Still running, in this turn: a
                                                // spinner and, when the call is
                                                // timed, how long it has taken.
                                                None if waiting.get()
                                                    && turn_start
                                                        .get()
                                                        .is_none_or(|t| {
                                                            seq.is_some_and(|s| s > t)
                                                        }) =>
                                                {
                                                    view! {
                                                        <span class="spinner"></span>
                                                        {at.map(|at| view! {
                                                            <span class="muted">" "{elapsed(now.get() - at)}</span>
                                                        })}
                                                    }
                                                    .into_any()
                                                }
                                                None => {
                                                    view! { <span class="muted">" …"</span> }
                                                        .into_any()
                                                }
                                                Some((is_error, result_at)) => view! {
                                                    {(is_error == Some(true)).then(|| {
                                                        view! { <span class="error">" failed"</span> }
                                                    })}
                                                    {tool_duration(at, result_at).map(|d| view! {
                                                        <span class="muted">" · "{elapsed(d)}</span>
                                                    })}
                                                }
                                                .into_any(),
                                            }
                                        }}
                                    </summary>
                                    {body}
                                    // A successful edit's result only repeats the diff.
                                    {move || result.with(|r| r.as_ref()
                                        .and_then(|e| tool_result(e))
                                        .filter(|(_, is_error, _, _)| *is_error || !is_edit)
                                        .map(|(content, is_error, _, _)| view! {
                                            <pre class="tool-result" class:error=is_error>{content.to_owned()}</pre>
                                        }))}
                                </details>
                                // Outside the details, so that screenshots show without
                                // opening the tool call.
                                {move || {
                                    let images = result.with(|r| {
                                        r.as_ref()
                                            .and_then(|e| tool_result(e))
                                            .map(|(_, _, _, i)| i.to_vec())
                                    });
                                    images.unwrap_or_default().into_iter().map(|image| view! {
                                        <TranscriptImage id=image caption=None />
                                    }).collect_view()
                                }}
                            }.into_any()
                        }
                        AgentEventKind::Image { id: image, caption } => view! {
                            <TranscriptImage id=image.clone() caption=caption.clone() />
                        }.into_any(),
                        AgentEventKind::ApprovalRequested { id: tool, tool_name, input, .. } if tool_name == "ExitPlanMode" => {
                            let value: serde_json::Value = serde_json::from_str(input).unwrap_or_default();
                            let plan = value["plan"].as_str().unwrap_or_default();
                            let open = {
                                let tool = tool.clone();
                                Signal::derive(move || open_plan.with(|p| p.as_ref() == Some(&tool)))
                            };
                            view! {
                                <PlanCard
                                    agent=id
                                    tool=tool.clone()
                                    plan=markdown::to_html(plan, &repo)
                                    backend=backend
                                    resolved=resolved
                                    open=open
                                    waiting=waiting
                                    mode=mode
                                    model=model
                                />
                            }.into_any()
                        }
                        AgentEventKind::ApprovalRequested { id: tool, tool_name, input, .. } if tool_name == "AskUserQuestion" => {
                            let questions = serde_json::from_str::<Questions>(input)
                                .map(|q| q.questions)
                                .unwrap_or_default();
                            view! {
                                <QuestionCard
                                    agent=id
                                    tool=tool.clone()
                                    backend=backend
                                    repo=repo.clone()
                                    questions=questions
                                    resolved=resolved
                                    waiting=waiting
                                />
                            }.into_any()
                        }
                        AgentEventKind::ApprovalRequested { id: tool, tool_name, input, options } => {
                            let value = serde_json::from_str(input).unwrap_or_default();
                            let summary = tool_summary(tool_name, &value, Some(&worktree));
                            let title = summary_title(&value, &summary);
                            let diff = edit_fragments(tool_name, &value, &[]);
                            view! {
                                <ApprovalCard
                                    agent=id
                                    tool=tool.clone()
                                    backend=backend
                                    tool_name=tool_name.clone()
                                    summary=summary
                                    title=title
                                    input=input.clone()
                                    diff=diff
                                    options=options.clone()
                                    resolved=resolved
                                    waiting=waiting
                                />
                            }.into_any()
                        }
                        AgentEventKind::TurnEnded { error: Some(e), .. } => view! {
                            <p class="turn-end error">{format!("The turn ended: {e}")}</p>
                        }.into_any(),
                        AgentEventKind::TurnEnded { error: None, cost: c } => view! {
                            <p class="turn-end muted">{format!("Done · {}", cost(c.unwrap_or_default(), unit))}</p>
                        }.into_any(),
                        AgentEventKind::Error(e) => view! {
                            <pre class="turn-error">{e.clone()}</pre>
                        }.into_any(),
                        AgentEventKind::Landed(Landed::Checkout { sha }) => view! {
                            <p class="turn-end wrapped-up">
                                "Merged into the checkout at " <code>{sha.clone()}</code>
                            </p>
                        }.into_any(),
                        AgentEventKind::Landed(landed) => {
                            let links = landed
                                .prs()
                                .into_iter()
                                .enumerate()
                                .map(|(i, pr)| {
                                    // A session in no repository has no in-app page for its PR.
                                    let hash = (repo != GENERAL).then(|| {
                                        hash_for(&PrKey {
                                            repo: repo.clone(),
                                            number: pr.number,
                                        })
                                    });
                                    view! {
                                        {(i > 0).then_some(", ")}
                                        {match hash {
                                            Some(hash) => view! {
                                                <a href=hash>{format!("PR #{}", pr.number)}</a>
                                                <GithubLink url=pr.url.clone() />
                                            }
                                            .into_any(),
                                            None => view! {
                                                <a href=pr.url.clone() target="_blank" rel="noopener noreferrer">
                                                    {format!("PR #{}", pr.number)}
                                                </a>
                                            }
                                            .into_any(),
                                        }}
                                    }
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
    /// The session's backend, for the modes the turn can go on in.
    backend: AgentBackend,
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
    // The mode the backend prefers the turn to go on in.
    let then = RwSignal::new(PermissionMode::new(backend.plan_default_mode()));
    let state = {
        let tool = tool.clone();
        Signal::derive(move || resolved.with(|r| r.get(&tool).copied()))
    };
    let answer = move |allow: bool| {
        let message = Some(feedback.get_untracked()).filter(|f| !f.trim().is_empty());
        let next = if allow {
            then.get_untracked()
        } else {
            PermissionMode::plan()
        };
        if allow && mode.get_untracked() == PermissionMode::plan() {
            mode.set(next.clone());
        }
        if waiting.get_untracked() {
            // Without feedback the server tells Claude to keep planning.
            let approval = Approval {
                id: tool.clone(),
                allow,
                message,
                mode: allow.then_some(next),
                option: None,
                answers: None,
            };
            send_approval(agent.clone(), approval, busy, error);
            return;
        }
        // The turn that asked is gone, so the answer starts a new one.
        let prompt = if allow {
            match message {
                Some(m) => format!(
                    "I approve the plan. Carry it out, keeping the following in mind:\n\n{}",
                    m.trim()
                ),
                None => "I approve the plan. Carry it out.".to_string(),
            }
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
                    Some(true) => "Plan approved".to_owned(),
                    Some(false) => "Plan sent back".to_owned(),
                    None if !open.get() => "Unanswered plan".to_owned(),
                    None if waiting.get() => format!("{}'s plan is waiting for your approval", backend.agent_name()),
                    None => "The turn ended before the plan was answered. Answering it continues the session.".to_owned(),
                }}
            </p>
            <div class="plan-text markdown" inner_html=plan></div>
            {move || pending().then(|| view! {
                <textarea
                    class="plan-feedback"
                    rows="2"
                    aria-label="Feedback"
                    placeholder=format!("Anything to add? {} gets this with your approval, or when it keeps planning.", backend.agent_name())
                    prop:value=move || feedback.get()
                    on:input=move |ev| feedback.set(event_target_value(&ev))
                ></textarea>
                <div class="form-buttons">
                    <label>"Then " <ModeSelect mode=then backend=Signal::stored(backend) after_plan=true label="After the plan" /></label>
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
    #[serde(default, rename = "multiSelect", alias = "multi_select")]
    multi_select: bool,
}

#[derive(Clone, Deserialize)]
struct QuestionOption {
    label: String,
    #[serde(default)]
    description: String,
}

/// A question's title, its first line, and the Markdown after it, if any.
fn question_parts(question: &str) -> (&str, Option<&str>) {
    let (title, body) = question.split_once('\n').unwrap_or((question, ""));
    let body = body.trim();
    (title.trim(), (!body.is_empty()).then_some(body))
}

/// The answer to a question: the picked options in their order, then
/// whatever the user wrote.
fn answer_values(q: &Question, picked: &[String], other: &str) -> Vec<String> {
    q.options
        .iter()
        .map(|o| o.label.as_str())
        .filter(|l| picked.iter().any(|p| p == l))
        .chain(Some(other.trim()).filter(|o| !o.is_empty()))
        .map(str::to_owned)
        .collect()
}

/// `AskUserQuestion`: Claude's questions with their options and room for
/// another answer.
#[component]
fn QuestionCard(
    agent: String,
    tool: String,
    /// The session's backend, for what the card calls the agent.
    backend: AgentBackend,
    /// The session's repository, for links in a question's Markdown.
    repo: String,
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
                        let answer = picked.with(|p| other.with(|o| answer_values(q, p, o)));
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
                option: None,
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
                    Some(true) => "Answered".to_owned(),
                    Some(false) => "Not answered".to_owned(),
                    None if waiting.get() => format!("{} asks", backend.agent_name()),
                    None => "Unanswered".to_owned(),
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
                    let (title, body) = question_parts(&q.question);
                    let body = body.map(|b| markdown::to_html(b, &repo));
                    view! {
                        <fieldset class="question">
                            <legend>
                                {(!q.header.is_empty()).then(|| view! { <span class="badge">{q.header}</span> " " })}
                                {title.to_owned()}
                            </legend>
                            {body.map(|html| view! {
                                <div class="question-body markdown" inner_html=html></div>
                            })}
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
                    <form on:submit=move |ev| {
                        ev.prevent_default();
                        answer(true);
                    }>
                        {fields}
                        <div class="form-buttons">
                            <button type="submit" class="primary" disabled=move || busy.get() || !complete()>"Answer"</button>
                            <button type="button" disabled=move || busy.get() on:click=move |_| skip(false)>"Skip"</button>
                        </div>
                    </form>
                }.into_any()
            } else {
                questions.with_value(|qs| qs.iter().map(|q| {
                    let (title, body) = question_parts(&q.question);
                    let body = body.map(|b| markdown::to_html(b, &repo));
                    view! {
                        <p class="muted">{title.to_owned()}</p>
                        {body.map(|html| view! {
                            <div class="question-body markdown" inner_html=html></div>
                        })}
                    }
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
    /// The session's backend, for what the card calls the agent.
    backend: AgentBackend,
    tool_name: String,
    summary: String,
    /// The full path a shortened `summary` stands for, shown as a tooltip.
    title: Option<String>,
    input: String,
    /// What a file tool would change, shown instead of `input`.
    diff: Option<(String, Vec<Fragment>)>,
    /// The answers the backend offers beyond allow/deny.
    options: Vec<ApprovalOption>,
    resolved: Memo<HashMap<String, bool>>,
    /// Whether the backend is running and so may wait for an answer;
    /// requests from before a restart stay unanswered.
    waiting: Signal<bool>,
) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let answer = {
        let tool = tool.clone();
        move |allow: bool, option: Option<String>| {
            let approval = Approval {
                id: tool.clone(),
                allow,
                message: None,
                mode: None,
                option,
                answers: None,
            };
            send_approval(agent.clone(), approval, busy, error);
        }
    };
    let deny = answer.clone();
    let options = StoredValue::new(options);
    let state = Signal::derive(move || resolved.with(|r| r.get(&tool).copied()));
    view! {
        <div class="approval" class:pending=move || state.get().is_none() && waiting.get()>
            <p>
                {move || match state.get() {
                    Some(true) => "Allowed ".to_owned(),
                    Some(false) => "Denied ".to_owned(),
                    None if waiting.get() => format!("{} wants to use ", backend.agent_name()),
                    None => "Unanswered: ".to_owned(),
                }}
                <span class="tool-name">{tool_name.clone()}</span> " " <code title=title>{summary}</code>
            </p>
            {move || (state.get().is_none() && waiting.get()).then(|| {
                let (answer, deny) = (answer.clone(), deny.clone());
                view! {
                    {match diff.clone() {
                        Some((path, fragments)) => view! { <EditDiff path=path fragments=fragments /> }.into_any(),
                        None => view! { <ToolInput name=tool_name.clone() input=input.clone() /> }.into_any(),
                    }}
                    {move || {
                        let options = options.get_value();
                        if options.is_empty() {
                            let (allow, deny) = (answer.clone(), deny.clone());
                            view! {
                                <div class="form-buttons">
                                    <button class="primary" disabled=move || busy.get() on:click=move |_| allow(true, None)>"Allow"</button>
                                    <button disabled=move || busy.get() on:click=move |_| deny(false, None)>"Deny"</button>
                                </div>
                            }.into_any()
                        } else {
                            let mut primary = true;
                            view! {
                                <div class="form-buttons">
                                    {options.into_iter().map(|o| {
                                        let class = if primary && o.kind == ApprovalKind::AllowOnce {
                                            primary = false;
                                            "primary"
                                        } else {
                                            ""
                                        };
                                        // The server goes by the option's kind
                                        // either way.
                                        let (id, allow) = (o.id.clone(), o.kind.is_allow());
                                        let answer = answer.clone();
                                        view! {
                                            <button class=class disabled=move || busy.get() on:click=move |_| answer(allow, Some(id.clone()))>{o.label}</button>
                                        }
                                    }).collect_view()}
                                </div>
                            }.into_any()
                        }
                    }}
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
    let backend = Signal::derive(move || session.with(|s| s.backend));
    // The follow-up being written in this session.
    let prompt = id.with_value(|id| draft_signal(draft_key(id)));
    // The images pasted so far, kept with the draft in IndexedDB.
    let images = id.with_value(|id| image_drafts::draft_images(image_drafts::agent_key(id)));
    let error = RwSignal::new(None::<String>);
    let blank = move || prompt.with(|p| p.trim().is_empty()) && images.with(Vec::is_empty);
    let working = move || {
        session.with(|s| {
            matches!(
                s.status,
                AgentStatus::Creating | AgentStatus::Running | AgentStatus::AwaitingApproval
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
                    image_drafts::forget(&image_drafts::agent_key(&id.get_value()));
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
                placeholder=move || if working() { "Queue a follow-up… (Enter sends, Shift+Enter for a new line)".to_owned() } else { format!("Reply to {}… (Enter sends, Shift+Enter for a new line)", backend.get().agent_name()) }
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
                <ModeSelect mode=mode backend=backend />
                <ModelSelect model=model backend=backend />
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
    fn bash_view_splits_command_from_other_fields() {
        let input = serde_json::json!({
            "command": "make test",
            "description": "run the tests",
            "timeout": 5000,
        });
        assert_eq!(
            bash_view(&input),
            Some((
                "make test".to_owned(),
                Some("description: run the tests · timeout: 5000".to_owned()),
            )),
        );
    }

    #[test]
    fn bash_view_has_no_fields_with_only_a_command() {
        let input = serde_json::json!({"command": "ls"});
        assert_eq!(bash_view(&input), Some(("ls".to_owned(), None)));
    }

    #[test]
    fn bash_view_rejects_a_missing_or_non_string_command() {
        assert_eq!(bash_view(&serde_json::json!({"timeout": 5000})), None);
        assert_eq!(bash_view(&serde_json::json!({"command": 42})), None);
        assert_eq!(bash_view(&serde_json::json!("ls")), None);
    }

    #[test]
    fn shows_a_turn_end_once() {
        let event = |seq, kind| AgentEvent {
            seq: Some(seq),
            at: None,
            kind,
        };
        let ended = |error: Option<&str>| AgentEventKind::TurnEnded {
            error: error.map(Into::into),
            cost: Some(1.0),
        };
        let events = [
            event(1, AgentEventKind::Text("a".into())),
            event(2, ended(None)),
            event(3, ended(None)),
            event(4, ended(Some("failed"))),
            event(5, ended(None)),
            event(6, AgentEventKind::Text("b".into())),
            event(7, ended(None)),
        ]
        .map(Arc::new);
        let seqs: Vec<_> = shown(&events).iter().filter_map(|e| e.seq).collect();
        assert_eq!(seqs, [1, 2, 4, 6, 7]);
    }

    fn event(seq: Option<u64>, kind: AgentEventKind) -> Incoming {
        Incoming::Event(AgentEvent {
            seq,
            at: None,
            kind,
        })
    }

    fn seqs(batch: &Batch) -> Vec<u64> {
        batch.events.iter().filter_map(|e| e.seq).collect()
    }

    #[test]
    fn drain_dedups_stored_events() {
        // Across the boundary, against the transcript's last seq.
        let batch = drain(
            vec![
                event(Some(4), AgentEventKind::Text("a".into())),
                event(Some(5), AgentEventKind::Text("b".into())),
                event(Some(6), AgentEventKind::Text("c".into())),
            ],
            Some(5),
        );
        assert_eq!(seqs(&batch), [6]);
        // And inside the batch itself.
        let batch = drain(
            vec![
                event(Some(6), AgentEventKind::Text("a".into())),
                event(Some(7), AgentEventKind::Text("b".into())),
                event(Some(6), AgentEventKind::Text("c".into())),
                event(Some(7), AgentEventKind::Text("d".into())),
            ],
            None,
        );
        assert_eq!(seqs(&batch), [6, 7]);
    }

    #[test]
    fn drain_clears_deltas_on_stored_events() {
        // A delta before a stored event is discarded; one after accrues.
        let batch = drain(
            vec![
                event(None, AgentEventKind::TextDelta("a".into())),
                event(Some(1), AgentEventKind::Text("b".into())),
                event(None, AgentEventKind::TextDelta("c".into())),
                event(None, AgentEventKind::TextDelta("d".into())),
            ],
            None,
        );
        assert!(matches!(batch.streaming, Streaming::Replace(s) if s == "cd"));
        // With nothing after, the streamed text just clears.
        let batch = drain(
            vec![
                event(None, AgentEventKind::TextDelta("a".into())),
                event(Some(1), AgentEventKind::Text("b".into())),
            ],
            None,
        );
        assert!(matches!(batch.streaming, Streaming::Replace(s) if s.is_empty()));
        // Deltas alone append.
        let batch = drain(
            vec![
                event(None, AgentEventKind::TextDelta("a".into())),
                event(None, AgentEventKind::TextDelta("b".into())),
            ],
            None,
        );
        assert!(matches!(batch.streaming, Streaming::Append(s) if s == "ab"));
        assert!(matches!(drain(vec![], None).streaming, Streaming::Keep));
    }

    #[test]
    fn drain_clears_deltas_on_open() {
        let batch = drain(
            vec![
                event(None, AgentEventKind::TextDelta("a".into())),
                Incoming::Opened,
                event(None, AgentEventKind::TextDelta("b".into())),
            ],
            None,
        );
        assert!(matches!(batch.streaming, Streaming::Replace(s) if s == "b"));
        let batch = drain(
            vec![
                event(None, AgentEventKind::TextDelta("a".into())),
                Incoming::Opened,
            ],
            None,
        );
        assert!(matches!(batch.streaming, Streaming::Replace(s) if s.is_empty()));
    }

    #[test]
    fn drain_merges_usage() {
        let batch = drain(
            vec![
                event(
                    None,
                    AgentEventKind::Usage {
                        tokens: Some(1),
                        window: None,
                    },
                ),
                event(
                    None,
                    AgentEventKind::Usage {
                        tokens: Some(2),
                        window: None,
                    },
                ),
                event(
                    None,
                    AgentEventKind::Usage {
                        tokens: None,
                        window: Some(3),
                    },
                ),
            ],
            None,
        );
        assert_eq!(batch.usage, Some((Some(2), Some(3))));
    }

    #[test]
    fn drain_keeps_the_latest_model() {
        let batch = drain(
            vec![
                event(None, AgentEventKind::Model("a".into())),
                event(None, AgentEventKind::Model("b".into())),
            ],
            None,
        );
        assert_eq!(batch.model.as_deref(), Some("b"));
    }

    #[test]
    fn drain_reports_the_newest_event_time() {
        let timed = |seq, at, kind| Incoming::Event(AgentEvent { seq, at, kind });
        // Stored and ephemeral events both count, skipped repeats too;
        // the newest `at` wins.
        let batch = drain(
            vec![
                timed(Some(1), Some(1_000), AgentEventKind::Text("a".into())),
                timed(Some(2), Some(2_000), AgentEventKind::Text("b".into())),
                timed(None, Some(3_000), AgentEventKind::TextDelta("c".into())),
                timed(None, None, AgentEventKind::TextDelta("d".into())),
            ],
            Some(1),
        );
        assert_eq!(batch.last_event, Some(3_000));
        // No times at all leaves it unset.
        let batch = drain(
            vec![event(None, AgentEventKind::TextDelta("a".into()))],
            None,
        );
        assert_eq!(batch.last_event, None);
    }

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
    fn strips_the_cd_into_the_worktree() {
        for command in [
            "cd /w/t && x",
            r#"cd "/w/t" && x"#,
            "cd '/w/t'; x",
            "cd /w/t/ && x",
            "  cd /w/t   &&   x",
        ] {
            assert_eq!(strip_cd(command, "/w/t"), "x", "{command}");
        }
        for command in [
            "cd /w/t/sub && x",
            "cd /other && x",
            "cd /w/t",
            "cd /w/t && ",
            "x && cd /w/t",
            "cd /w/tt && x",
        ] {
            assert_eq!(strip_cd(command, "/w/t"), command, "{command}");
        }
        assert_eq!(strip_cd("cd /w/t && x", ""), "cd /w/t && x");
    }

    #[test]
    fn splits_a_questions_title_and_body() {
        assert_eq!(question_parts("Tea?"), ("Tea?", None));
        assert_eq!(
            question_parts("Implement this plan?\n\n- one\n- two\n"),
            ("Implement this plan?", Some("- one\n- two"))
        );
        assert_eq!(question_parts("Tea?   \n\nMilk?"), ("Tea?", Some("Milk?")));
        assert_eq!(question_parts("Title\n   \n"), ("Title", None));
    }

    #[test]
    fn tool_summaries() {
        let input = serde_json::json!({"command": "ls -la\nmore", "description": "List"});
        assert_eq!(tool_summary("Bash", &input, None), "ls -la");
        let input = serde_json::json!({"command": "cd /w/t && ls\nmore"});
        assert_eq!(tool_summary("Bash", &input, Some("/w/t")), "ls");
        assert_eq!(tool_summary("Bash", &input, None), "cd /w/t && ls");
        let input = serde_json::json!({"file_path": "/w/a.rs"});
        assert_eq!(tool_summary("Edit", &input, None), "/w/a.rs");
        assert_eq!(tool_summary("mcp__x__y", &input, None), "");
        let input = serde_json::json!({"plan": "# Plan\n\nSteps"});
        assert_eq!(tool_summary("ExitPlanMode", &input, None), "# Plan");
        let input = serde_json::json!({"questions": [{"question": "Tea?"}]});
        assert_eq!(tool_summary("AskUserQuestion", &input, None), "Tea?");
        let input = serde_json::json!({"questions": [{"question": "Implement this plan?\n\n- one\n- two"}]});
        assert_eq!(
            tool_summary("AskUserQuestion", &input, None),
            "Implement this plan?"
        );
        let input = serde_json::json!({"skill": "pr-title"});
        assert_eq!(tool_summary("Skill", &input, None), "pr-title");
        assert_eq!(tool_summary("skill", &input, None), "pr-title");
        assert_eq!(tool_summary("skill", &serde_json::json!({}), None), "");
        let input = serde_json::json!({"query": "rust leptos"});
        assert_eq!(tool_summary("web_search", &input, None), "rust leptos");
        let input = serde_json::json!({"url": "https://example.com"});
        assert_eq!(
            tool_summary("webfetch", &input, None),
            "https://example.com"
        );
        let input = serde_json::json!({"pattern": "**/*.rs"});
        assert_eq!(tool_summary("find_file_by_name", &input, None), "**/*.rs");
        let input = serde_json::json!({"query": "a|b", "path": "/w/t/src"});
        assert_eq!(tool_summary("Grep", &input, Some("/w/t")), "a|b in src");
        let input = serde_json::json!({"query": "a|b", "path": "/w/t/src", "file_pattern": "*.rs"});
        assert_eq!(
            tool_summary("Grep", &input, Some("/w/t")),
            "a|b in src (*.rs)"
        );
        let input = serde_json::json!({"query": "a|b", "path": "/w/t"});
        assert_eq!(tool_summary("Grep", &input, Some("/w/t")), "a|b");
        let input = serde_json::json!({"query": "a|b", "path": "/etc"});
        assert_eq!(tool_summary("Grep", &input, Some("/w/t")), "a|b in /etc");
        let input = serde_json::json!({"query": "a|b"});
        assert_eq!(tool_summary("Grep", &input, Some("/w/t")), "a|b");
        let input = serde_json::json!({"pattern": "fn main", "path": "/w/t/x", "glob": "*.rs"});
        assert_eq!(
            tool_summary("Grep", &input, Some("/w/t")),
            "fn main in x (*.rs)"
        );
        let input = serde_json::json!({"query": "a\nb"});
        assert_eq!(tool_summary("Grep", &input, None), "a");
        let input = serde_json::json!({"query": "**/*.rs"});
        assert_eq!(tool_summary("find_file_by_name", &input, None), "**/*.rs");
        assert_eq!(tool_summary("Grep", &serde_json::json!({}), None), "");
        let input = serde_json::json!({"title": "Explore auth", "task": "Find it"});
        assert_eq!(tool_summary("run_subagent", &input, None), "Explore auth");
    }

    #[test]
    fn paths_relative_to_the_worktree() {
        assert_eq!(relative_to("/w/t/a/b.rs", "/w/t"), "a/b.rs");
        assert_eq!(relative_to("/w/t/a.rs", "/w/t/"), "a.rs");
        assert_eq!(relative_to("/w/t", "/w/t"), ".");
        assert_eq!(relative_to("/w/t/", "/w/t"), ".");
        assert_eq!(relative_to("/etc/hosts", "/w/t"), "/etc/hosts");
        assert_eq!(relative_to("/w/t-other/x", "/w/t"), "/w/t-other/x");
        assert_eq!(relative_to("/w/t/a.rs", ""), "/w/t/a.rs");
    }

    #[test]
    fn tool_summaries_make_paths_relative() {
        for name in ["Read", "Edit"] {
            let input = serde_json::json!({"file_path": "/w/t/a.rs"});
            assert_eq!(tool_summary(name, &input, Some("/w/t")), "a.rs", "{name}");
            assert_eq!(tool_summary(name, &input, None), "/w/t/a.rs", "{name}");
        }
        let input = serde_json::json!({"file_path": "/etc/hosts"});
        assert_eq!(tool_summary("Read", &input, Some("/w/t")), "/etc/hosts");
    }

    #[test]
    fn summary_titles() {
        let input = serde_json::json!({"file_path": "/w/t/a.rs"});
        assert_eq!(summary_title(&input, "a.rs"), Some("/w/t/a.rs".to_owned()));
        assert_eq!(summary_title(&input, "/w/t/a.rs"), None);
        let input = serde_json::json!({"command": "ls"});
        assert_eq!(summary_title(&input, "ls"), None);
        let input = serde_json::json!({"query": "a", "path": "/w/t/src"});
        assert_eq!(
            summary_title(&input, "a in src"),
            Some("/w/t/src".to_owned())
        );
        let input = serde_json::json!({"query": "a", "path": "/etc"});
        assert_eq!(summary_title(&input, "a in /etc"), None);
    }

    #[test]
    fn answers() {
        let question = |flag: &str| {
            serde_json::from_value::<Question>(serde_json::json!({
                "question": "Drinks?",
                "options": [{"label": "Tea"}, {"label": "Coffee"}],
                flag: true,
            }))
            .unwrap()
        };
        for flag in ["multiSelect", "multi_select"] {
            assert!(question(flag).multi_select, "{flag}");
        }
        let q = question("multiSelect");
        let picked = ["Coffee".to_owned(), "Tea".to_owned()];
        assert_eq!(answer_values(&q, &picked, ""), ["Tea", "Coffee"]);
        assert_eq!(
            answer_values(&q, &picked[..1], " Water "),
            ["Coffee", "Water"]
        );
        assert_eq!(answer_values(&q, &[], "  "), Vec::<String>::new());
    }

    #[test]
    fn groups_sessions_by_repo() {
        let groups = by_repo(
            vec!["o/a".into(), "o/b".into()],
            vec![
                AgentSession::fixture("1", "o/b"),
                AgentSession::fixture("2", "o/gone"),
                AgentSession::fixture("3", "o/b"),
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
    fn prompt_with_skill() {
        assert_eq!(with_skill("bar", Some("foo")), "/foo bar");
        assert_eq!(with_skill("/baz bar", Some("foo")), "/baz bar");
        assert_eq!(with_skill("  /baz bar", Some("foo")), "  /baz bar");
        assert_eq!(with_skill("bar", None), "bar");
        assert_eq!(with_skill("bar\nbaz", Some("foo")), "/foo bar\nbaz");
    }

    #[test]
    fn repo_default_modes() {
        let settings = RepoSettings {
            session_modes: [
                (AgentBackend::Claude, PermissionMode::from("auto")),
                (AgentBackend::Devin, PermissionMode::from("accept-edits")),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert_eq!(
            repo_default_mode(Some(&settings), AgentBackend::Claude).as_str(),
            "auto"
        );
        assert_eq!(
            repo_default_mode(Some(&settings), AgentBackend::Devin).as_str(),
            "accept-edits"
        );
        // A mode the backend doesn't have, or none at all, falls back to
        // plan.
        let other = RepoSettings {
            session_modes: [(AgentBackend::Devin, PermissionMode::from("auto"))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            repo_default_mode(Some(&other), AgentBackend::Devin).as_str(),
            "plan"
        );
        assert_eq!(
            repo_default_mode(None, AgentBackend::Claude).as_str(),
            "plan"
        );
    }

    #[test]
    fn pretty_prints_json() {
        assert_eq!(pretty(r#"{"a":1}"#), "{\n  \"a\": 1\n}");
        assert_eq!(pretty("not json"), "not json");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(elapsed(0), "0s");
        assert_eq!(elapsed(59_999), "59s");
        assert_eq!(elapsed(60_000), "1m 00s");
        assert_eq!(elapsed(102_000), "1m 42s");
        assert_eq!(elapsed(3_599_999), "59m 59s");
        assert_eq!(elapsed(3_600_000), "1h 00m");
        assert_eq!(elapsed(3_900_000), "1h 05m");
    }

    #[test]
    fn tool_durations_worth_showing() {
        assert_eq!(tool_duration(None, Some(20_000)), None);
        assert_eq!(tool_duration(Some(1_000), None), None);
        assert_eq!(tool_duration(Some(1_000), Some(10_999)), None);
        assert_eq!(tool_duration(Some(1_000), Some(11_000)), Some(10_000));
        assert_eq!(tool_duration(Some(0), Some(125_000)), Some(125_000));
    }

    #[test]
    fn picks_the_current_activity() {
        let prompt = || AgentEvent {
            seq: Some(1),
            at: Some(1_000),
            kind: AgentEventKind::Prompt("do it".into()),
        };
        let call_with = |seq, id: &str, parent, command: &str| AgentEvent {
            seq: Some(seq),
            at: Some(seq as i64 * 1_000),
            kind: AgentEventKind::ToolUse {
                id: id.into(),
                name: "Bash".into(),
                input: format!(r#"{{"command":"{command}"}}"#),
                parent,
            },
        };
        let call = |seq, id: &str, parent| call_with(seq, id, parent, &format!("cmd {id}"));
        let result = |seq, id: &str| AgentEvent {
            seq: Some(seq),
            at: Some(seq as i64 * 1_000),
            kind: AgentEventKind::ToolResult {
                id: id.into(),
                content: "done".into(),
                is_error: false,
                patch: Vec::new(),
                images: Vec::new(),
            },
        };

        // A single unfinished call.
        let events = [prompt(), call(2, "a", None)];
        assert_eq!(
            current_activity(&events.map(Arc::new), false, Some(2_000), ""),
            Activity::Tool {
                name: "Bash".into(),
                summary: "cmd a".into(),
                since: Some(2_000),
                more: 0,
            }
        );

        // A `cd` into the worktree prefixing the command isn't shown.
        let events = [prompt(), call_with(2, "a", None, "cd /w/t && cmd a")];
        assert_eq!(
            current_activity(&events.map(Arc::new), false, Some(2_000), "/w/t"),
            Activity::Tool {
                name: "Bash".into(),
                summary: "cmd a".into(),
                since: Some(2_000),
                more: 0,
            }
        );

        // Two parallel calls: the newest shows, the other is counted.
        let events = [prompt(), call(2, "a", None), call(3, "b", None)];
        assert_eq!(
            current_activity(&events.map(Arc::new), false, Some(3_000), ""),
            Activity::Tool {
                name: "Bash".into(),
                summary: "cmd b".into(),
                since: Some(3_000),
                more: 1,
            }
        );

        // A finished call leaves streaming text: writing.
        let events = [prompt(), call(2, "a", None), result(3, "a")];
        assert_eq!(
            current_activity(&events.map(Arc::new), true, Some(3_000), ""),
            Activity::Writing { since: Some(3_000) }
        );

        // A subagent's newest call shows as such, never as an unfinished
        // tool call.
        let events = [
            prompt(),
            call(2, "a", None),
            result(3, "a"),
            call(4, "sub", Some("task".into())),
        ];
        assert_eq!(
            current_activity(&events.map(Arc::new), false, Some(4_000), ""),
            Activity::Subagent {
                name: "Bash".into(),
                summary: "cmd sub".into(),
                since: Some(4_000),
            }
        );
        // An older subagent call isn't an unfinished tool either; only
        // top-level calls are.
        let events = [prompt(), call(2, "sub", Some("task".into()))];
        assert_eq!(
            current_activity(&events.map(Arc::new), false, Some(2_000), ""),
            Activity::Subagent {
                name: "Bash".into(),
                summary: "cmd sub".into(),
                since: Some(2_000),
            }
        );

        // Nothing going on: thinking since the newest timed event.
        let events = [prompt(), result(3, "a")];
        assert_eq!(
            current_activity(&events.map(Arc::new), false, Some(3_000), ""),
            Activity::Thinking { since: Some(3_000) }
        );

        // Calls from before the latest prompt are finished business.
        let events = [
            call(1, "a", None),
            AgentEvent {
                seq: Some(2),
                at: Some(2_000),
                kind: AgentEventKind::Prompt("again".into()),
            },
        ];
        assert_eq!(
            current_activity(&events.map(Arc::new), false, Some(2_000), ""),
            Activity::Thinking { since: Some(2_000) }
        );
    }

    #[test]
    fn queued_outranks_a_conflict_and_the_checks() {
        let pr = |queued| AgentPr {
            key: PrKey {
                repo: "o/n".into(),
                number: 7,
            },
            title: None,
            state: "OPEN".into(),
            ci: Some(CiStatus::Failing),
            conflicting: true,
            queued,
        };
        assert_eq!(pr_state(&pr(true)), ("queued", "In the merge queue"));
        assert_eq!(
            pr_state(&pr(false)),
            ("conflict", "Conflicts with its base")
        );
        // Closed and merged PRs don't show the queue flag.
        let mut merged = pr(true);
        merged.state = "MERGED".into();
        assert_eq!(pr_state(&merged), ("merged", "Merged"));
    }
}
