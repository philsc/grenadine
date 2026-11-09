//! A PR's reviews, its conversation and whether it can be merged.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use grenadine_core::api::{
    AutoMerge, Check, CiStatus, Enqueue, IssueComment, MergeMethod, MergeRequest, MergeState,
    MergeStatus, Person, PrDetail, PrKey, QueueState, RequestedReviewer, Review, ReviewDecision,
    ReviewEvent, ReviewInfo, ReviewState, ReviewerCandidates, ReviewerChange, StackMergePr,
    SubmitReview, TeamRef, reviewer_change, submit_blocker,
};
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::api;
use crate::composer::{
    ComposerToggle, MarkdownEditor, OpenBoxes, comment_draft_key, draft_signal, sender,
};
use crate::hash_for;
use crate::inboxes::CiIcon;
use crate::pr::{CommentFrame, PersonChip, PrLinks, ReviewTarget};

/// Each reviewer's verdict: their latest approval, change request or
/// dismissal, or their latest comment when they gave no verdict. The PR's
/// author is left out, since replying to a thread makes a comment review.
/// Ordered by when each reviewer first reviewed.
pub fn latest_reviews<'a>(reviews: &'a [Review], author: &str) -> Vec<&'a Review> {
    let mut out: Vec<&Review> = Vec::new();
    for r in reviews {
        if r.author.login.as_deref() == Some(author) {
            continue;
        }
        match out.iter_mut().find(|o| o.author.login == r.author.login) {
            Some(o) => {
                if r.state != ReviewState::Commented || o.state == ReviewState::Commented {
                    *o = r;
                }
            }
            None => out.push(r),
        }
    }
    out
}

/// Why GitHub blocks merging the PR, each reason the way GitHub's merge
/// box words it, `unresolved` being the PR's unresolved review threads.
/// GitHub's API reports no reasons, so they're worked out from the
/// branch's rules and the PR's state; when none is known a catch-all
/// stands in.
fn block_reasons(info: &ReviewInfo, unresolved: usize) -> Vec<String> {
    let mut reasons = Vec::new();
    match info.decision {
        Some(ReviewDecision::ReviewRequired) => reasons.push("it needs an approving review".into()),
        Some(ReviewDecision::ChangesRequested) => reasons.push("changes were requested".into()),
        _ => {}
    }
    if info.merge.requires_conversation_resolution && unresolved > 0 {
        let s = if unresolved == 1 { "" } else { "s" };
        reasons.push(format!("{unresolved} conversation{s} must be resolved"));
    }
    if reasons.is_empty() {
        reasons.push("required checks or rules aren't satisfied".into());
    }
    reasons
}

/// The queue entry's state in words, for "In the merge queue".
fn queue_state_words(state: QueueState) -> &'static str {
    match state {
        QueueState::AwaitingChecks => "waiting for checks",
        QueueState::Locked => "locked",
        QueueState::Mergeable => "mergeable",
        QueueState::Queued => "queued",
        QueueState::Unmergeable => "unmergeable",
        QueueState::Unknown => "state unknown",
    }
}

/// One line about whether the PR can be merged: a CSS class, `ok`,
/// `warn` or `bad`, and the text. `unresolved` is how many review
/// threads are unresolved.
pub fn merge_status(info: &ReviewInfo, unresolved: usize) -> (&'static str, String) {
    if let Some(entry) = info.merge.queue.as_ref().and_then(|q| q.entry) {
        let class = match entry.state {
            QueueState::Mergeable => "ok",
            QueueState::Unmergeable => "bad",
            _ => "warn",
        };
        return (
            class,
            format!(
                "In the merge queue: position {} ({}).",
                entry.position,
                queue_state_words(entry.state)
            ),
        );
    }
    let queue = info.merge.queue.as_ref();
    if queue.is_some_and(|q| q.auto_merge) {
        return ("ok", "Will join the merge queue when ready.".into());
    }
    if info.merge.conflicting == Some(true) {
        return (
            "bad",
            "This branch has conflicts that must be resolved.".into(),
        );
    }
    match info.merge.state {
        MergeState::Clean | MergeState::HasHooks if queue.is_some() => {
            ("ok", "Ready to join the merge queue.".into())
        }
        MergeState::Clean | MergeState::HasHooks => ("ok", "Ready to merge.".into()),
        MergeState::Unstable if queue.is_some() => (
            "warn",
            "Can join the merge queue, but not every check passed.".into(),
        ),
        MergeState::Unstable => ("warn", "Can be merged, but not every check passed.".into()),
        MergeState::Behind => ("warn", "This branch is out of date with its base.".into()),
        MergeState::Draft => ("warn", "This PR is a draft.".into()),
        MergeState::Dirty => (
            "bad",
            "This branch has conflicts that must be resolved.".into(),
        ),
        MergeState::Blocked => (
            "bad",
            format!(
                "Merging is blocked: {}.",
                block_reasons(info, unresolved).join("; ")
            ),
        ),
        MergeState::Unknown => (
            "warn",
            "GitHub is still working out whether this can be merged.".into(),
        ),
    }
}

fn verb(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "requested changes",
        ReviewState::Commented => "reviewed",
        ReviewState::Dismissed => "reviewed (dismissed)",
    }
}

fn state_class(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "review-state approved",
        ReviewState::ChangesRequested => "review-state changes",
        ReviewState::Commented | ReviewState::Dismissed => "review-state commented",
    }
}

/// One row of the sidebar's reviewer list.
enum Reviewer<'a> {
    /// Reviewed; the verdict is their latest review's state.
    Reviewed(&'a Review),
    /// Asked to review; the verdict is "Pending".
    Requested(&'a RequestedReviewer),
}

/// The sidebar's reviewers: everyone who reviewed, in first-review
/// order, then everyone asked who hasn't. `requested` is what shows as
/// asked, which differs from the PR's while a change is in flight. A
/// person asked again after reviewing shows once, as pending, since a
/// new verdict is awaited.
fn reviewers<'a>(
    reviews: &'a [Review],
    requested: &'a [RequestedReviewer],
    author: &str,
) -> Vec<Reviewer<'a>> {
    let mut out: Vec<Reviewer> = latest_reviews(reviews, author)
        .into_iter()
        .map(Reviewer::Reviewed)
        .collect();
    for r in requested {
        if let RequestedReviewer::Person(p) = r
            && let Some(login) = p.login.as_deref()
            && let Some(i) = out.iter().position(
                |o| matches!(o, Reviewer::Reviewed(v) if v.author.login.as_deref() == Some(login)),
            )
        {
            out.remove(i);
        }
        out.push(Reviewer::Requested(r));
    }
    out
}

/// The verdict a review state shows in the sidebar.
fn verdict(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "Approved",
        ReviewState::ChangesRequested => "Changes requested",
        ReviewState::Commented => "Commented",
        ReviewState::Dismissed => "Dismissed",
    }
}

/// One of the groups the sidebar sorts the checks into, in show order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CheckGroup {
    Failing,
    Running,
    Passed,
    Skipped,
}

impl CheckGroup {
    fn of(status: Option<CiStatus>) -> CheckGroup {
        match status {
            Some(CiStatus::Failing) => CheckGroup::Failing,
            Some(CiStatus::Running) => CheckGroup::Running,
            Some(CiStatus::Passing) => CheckGroup::Passed,
            None => CheckGroup::Skipped,
        }
    }

    fn label(self) -> &'static str {
        match self {
            CheckGroup::Failing => "Failing",
            CheckGroup::Running => "Running",
            CheckGroup::Passed => "Passed",
            CheckGroup::Skipped => "Skipped",
        }
    }

    /// The icon the group's header shows, if the status has one.
    fn icon(self) -> Option<CiStatus> {
        match self {
            CheckGroup::Failing => Some(CiStatus::Failing),
            CheckGroup::Running => Some(CiStatus::Running),
            CheckGroup::Passed => Some(CiStatus::Passing),
            CheckGroup::Skipped => None,
        }
    }

    /// Whether the group starts expanded: failing and running checks
    /// are what needs attention.
    fn expanded(self) -> bool {
        matches!(self, CheckGroup::Failing | CheckGroup::Running)
    }
}

/// The checks split into their groups, in group order, each sorted by
/// name. Empty groups are left out.
fn check_groups(checks: &[Check]) -> Vec<(CheckGroup, Vec<Check>)> {
    [
        CheckGroup::Failing,
        CheckGroup::Running,
        CheckGroup::Passed,
        CheckGroup::Skipped,
    ]
    .into_iter()
    .filter_map(|g| {
        let mut group: Vec<Check> = checks
            .iter()
            .filter(|c| CheckGroup::of(c.status) == g)
            .cloned()
            .collect();
        group.sort_by_key(|c| c.name.to_lowercase());
        (!group.is_empty()).then_some((g, group))
    })
    .collect()
}

/// The check groups the user expanded or collapsed, keyed by group.
/// Groups not in here use their default. Provided in `PrView` so it
/// survives the sidebar body's rebuilds when the PR's detail changes.
#[derive(Clone, Copy)]
pub(crate) struct CheckFolds(pub RwSignal<HashMap<CheckGroup, bool>>);

fn avatar(person: &Person) -> AnyView {
    match &person.avatar_url {
        Some(url) => view! { <img class="avatar" src=url.clone() alt="" /> }.into_any(),
        None => view! { <span class="avatar avatar-generic"></span> }.into_any(),
    }
}

/// The icon that marks a team in the reviewer list.
#[component]
fn TeamIcon() -> impl IntoView {
    view! {
        <span class="team-icon" title="Team">
            <svg
                viewBox="0 0 16 16"
                width="14"
                height="14"
                fill="none"
                stroke="currentColor"
                stroke-width="1.5"
                stroke-linecap="round"
                stroke-linejoin="round"
                aria-hidden="true"
            >
                <path d="M5.5 3.5a2 2 0 1 1 0 4 2 2 0 0 1 0-4zM1.5 13.5v-.5c0-2 1.8-3.5 4-3.5s4 1.5 4 3.5v.5M11 3.7a2 2 0 1 1 0 3.8M14.5 13.5v-.5c0-1.8-1.4-3.2-3.3-3.5" />
            </svg>
        </span>
    }
}

/// One reviewer row's four grid cells: the icon (avatar, team icon or
/// nothing), the name, the verdict and the re-request button. Only a
/// reviewer who already reviewed, on a PR the viewer can edit, gets one.
fn reviewer_row(
    reviewer: &Reviewer,
    editable: bool,
    rerequest: Callback<Person>,
) -> impl IntoView + use<> {
    let (icon, name, text, class) = match reviewer {
        Reviewer::Reviewed(r) => (
            avatar(&r.author),
            r.author.name.clone().or_else(|| r.author.login.clone()),
            verdict(r.state),
            state_class(r.state),
        ),
        Reviewer::Requested(RequestedReviewer::Person(p)) => (
            avatar(p),
            p.name.clone().or_else(|| p.login.clone()),
            "Pending",
            "review-state pending",
        ),
        Reviewer::Requested(RequestedReviewer::Team(t)) => (
            view! { <TeamIcon /> }.into_any(),
            Some(t.name.clone()),
            "Pending",
            "review-state pending",
        ),
        Reviewer::Requested(RequestedReviewer::Legacy(name)) => (
            view! { <span></span> }.into_any(),
            Some(name.clone()),
            "Pending",
            "review-state pending",
        ),
    };
    let action = match (editable, reviewer) {
        (true, Reviewer::Reviewed(r)) => {
            let p = r.author.clone();
            view! {
                <button type="button" class="reviewer-rerequest" title="Re-request review"
                    aria-label="Re-request review"
                    on:click=move |_| rerequest.run(p.clone())>
                    <svg
                        viewBox="0 0 16 16"
                        width="13"
                        height="13"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.5"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                        aria-hidden="true"
                    >
                        <path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9" />
                        <path d="M13.5 1.5v3h-3" />
                    </svg>
                </button>
            }
            .into_any()
        }
        _ => view! { <span></span> }.into_any(),
    };
    view! {
        <span class="reviewer-icon">{icon}</span>
        <span class="reviewer-name">{name.unwrap_or_default()}</span>
        <span class=class>{text}</span>
        {action}
    }
}

/// What the reviewer picker has ticked.
#[derive(Clone, Default)]
pub struct Picks {
    /// What was ticked when it opened.
    pub initial_users: BTreeSet<String>,
    pub initial_teams: BTreeSet<String>,
    /// What's ticked now.
    pub users: BTreeSet<String>,
    pub teams: BTreeSet<String>,
}

/// The reviewer change the sidebar has in flight: the picker's
/// selection, the optimistic list and the last error. Provided in
/// `PrView` so they survive the sidebar body's rebuilds when the PR's
/// detail changes.
#[derive(Clone, Copy)]
pub(crate) struct ReviewerEdit {
    /// The picker's ticks while it's open; `None` while it's closed.
    pub picks: RwSignal<Option<Picks>>,
    /// The search text the picker's box last emitted.
    pub search: RwSignal<String>,
    /// The request the last change shows: the synced list it was made
    /// against, and the list to show until a sync brings new data.
    /// `None` shows the synced list.
    pub optimistic: RwSignal<Option<(Vec<RequestedReviewer>, Vec<RequestedReviewer>)>>,
    /// Why the last change failed, until the next one starts.
    pub error: RwSignal<Option<String>>,
}

/// The ticks the picker opens with: what's asked for already.
fn picks_from(requested: &[RequestedReviewer]) -> Picks {
    let users: BTreeSet<String> = requested
        .iter()
        .filter_map(|r| match r {
            RequestedReviewer::Person(p) => p.login.clone(),
            _ => None,
        })
        .collect();
    let teams: BTreeSet<String> = requested
        .iter()
        .filter_map(|r| match r {
            // A team without a slug can't be removed; it's not ticked,
            // so the picker shows it only as a disabled extra row.
            RequestedReviewer::Team(t) if !t.slug.is_empty() => Some(t.slug.clone()),
            _ => None,
        })
        .collect();
    Picks {
        initial_users: users.clone(),
        initial_teams: teams.clone(),
        users,
        teams,
    }
}

/// Asks the server for `change`, showing `overlay` as the PR's
/// requested reviewers meanwhile. `synced` is the list the change was
/// made against; a sync that brings a different one ends the overlay.
/// A failure reverts to the synced list and shows why.
fn send_reviewer_change(
    edit: ReviewerEdit,
    key: PrKey,
    synced: Vec<RequestedReviewer>,
    overlay: Vec<RequestedReviewer>,
    change: ReviewerChange,
) {
    edit.error.set(None);
    edit.optimistic.set(Some((synced, overlay)));
    leptos::task::spawn_local(async move {
        if let Err(e) = api::change_reviewers(&key, &change).await {
            edit.optimistic.set(None);
            edit.error.set(Some(e));
        }
    });
}

/// Sends the change the picker's ticks make, in one request; sends
/// nothing when nothing changed. `synced` is the requested list the
/// ticks started from, `lookup` turns an added login or slug back into
/// what shows for it.
fn apply_picks(
    edit: ReviewerEdit,
    key: PrKey,
    synced: Vec<RequestedReviewer>,
    person: impl Fn(&str) -> Person,
    team: impl Fn(&str) -> TeamRef,
) {
    let Some(picks) = edit.picks.get_untracked() else {
        return;
    };
    edit.picks.set(None);
    let list = |s: &BTreeSet<String>| s.iter().cloned().collect::<Vec<String>>();
    let change = reviewer_change(
        &list(&picks.initial_users),
        &list(&picks.initial_teams),
        &list(&picks.users),
        &list(&picks.teams),
    );
    if change == ReviewerChange::default() {
        return;
    }
    let remove_users: BTreeSet<&str> = change.remove_users.iter().map(String::as_str).collect();
    let remove_teams: BTreeSet<&str> = change.remove_teams.iter().map(String::as_str).collect();
    let mut overlay: Vec<RequestedReviewer> = synced
        .iter()
        .filter(|r| match r {
            RequestedReviewer::Person(p) => {
                !p.login.as_deref().is_some_and(|l| remove_users.contains(l))
            }
            RequestedReviewer::Team(t) => !remove_teams.contains(t.slug.as_str()),
            RequestedReviewer::Legacy(_) => true,
        })
        .cloned()
        .collect();
    overlay.extend(
        change
            .add_users
            .iter()
            .map(|l| RequestedReviewer::Person(person(l))),
    );
    overlay.extend(
        change
            .add_teams
            .iter()
            .map(|s| RequestedReviewer::Team(team(s))),
    );
    send_reviewer_change(edit, key, synced, overlay, change);
}

/// The gear and the popover it opens for changing who's asked to
/// review: GitHub's suggested reviewers first, then the assignable
/// users and the organization's teams matching the search. Ticks
/// collect in `ReviewerEdit::picks`; closing — Escape, a click
/// outside or the gear again — sends the change they make.
#[component]
fn ReviewerPicker(
    key: PrKey,
    /// The requested reviewers the ticks start from.
    requested: Signal<Vec<RequestedReviewer>>,
) -> impl IntoView {
    let edit = expect_context::<ReviewerEdit>();
    let stored_key = StoredValue::new(key);
    let synced = move || {
        edit.optimistic
            .get_untracked()
            .map(|(base, _)| base)
            .unwrap_or_else(|| requested.get_untracked())
    };
    // What an added login or slug shows as: the candidates' data when
    // known, else what's already listed, else the bare login or slug.
    let candidates = LocalResource::new(move || {
        let key = stored_key.get_value();
        let q = edit.search.get();
        async move { api::reviewer_candidates(&key, &q).await }
    });
    let lookup_person = {
        move |login: &str| {
            let found = |c: &ReviewerCandidates| {
                c.suggested
                    .iter()
                    .chain(&c.users)
                    .find(|p| p.login.as_deref() == Some(login))
                    .cloned()
            };
            candidates
                .get_untracked()
                .and_then(|r| r.ok().and_then(|c| found(&c)))
                .or_else(|| {
                    requested.get_untracked().iter().find_map(|r| match r {
                        RequestedReviewer::Person(p) if p.login.as_deref() == Some(login) => {
                            Some(p.clone())
                        }
                        _ => None,
                    })
                })
                .unwrap_or(Person {
                    login: Some(login.to_owned()),
                    ..Person::default()
                })
        }
    };
    let lookup_team = {
        move |slug: &str| {
            let found = |c: &ReviewerCandidates| c.teams.iter().find(|t| t.slug == slug).cloned();
            candidates
                .get_untracked()
                .and_then(|r| r.ok().and_then(|c| found(&c)))
                .or_else(|| {
                    requested.get_untracked().iter().find_map(|r| match r {
                        RequestedReviewer::Team(t) if t.slug == slug => Some(t.clone()),
                        _ => None,
                    })
                })
                .unwrap_or(TeamRef {
                    name: slug.to_owned(),
                    slug: slug.to_owned(),
                })
        }
    };
    let apply = move || {
        apply_picks(
            edit,
            stored_key.get_value(),
            synced(),
            lookup_person,
            lookup_team,
        )
    };
    let apply_outside = apply;
    let apply_close = apply;
    // Escape and clicks outside close the picker and apply the ticks.
    let root = NodeRef::<leptos::html::Div>::new();
    let on_click = window_event_listener(leptos::ev::click, move |ev| {
        if edit.picks.with_untracked(|p| p.is_none()) {
            return;
        }
        let inside = root.get_untracked().is_some_and(|el| {
            ev.target()
                .and_then(|t| t.dyn_into::<web_sys::Node>().ok())
                .is_some_and(|t| el.contains(Some(&t)))
        });
        if !inside {
            apply_outside();
        }
    });
    let on_key = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" && edit.picks.with_untracked(|p| p.is_some()) {
            apply_close();
        }
    });
    on_cleanup(move || {
        on_click.remove();
        on_key.remove();
    });
    // The search text is debounced: only a pause refetches candidates.
    let seq = RwSignal::new(0u64);
    let on_input = move |ev| {
        let text = event_target_value(&ev);
        seq.update(|s| *s += 1);
        let s = seq.get_untracked();
        set_timeout(
            move || {
                if seq.get_untracked() == s {
                    edit.search.set(text);
                }
            },
            Duration::from_millis(250),
        );
    };
    // The popover mounts and unmounts only on `is_open` flips; its DOM
    // — search box, list and scroll — survives every tick.
    let is_open = Memo::new(move |_| edit.picks.with(|p| p.is_some()));
    let user_row = {
        move |p: Person| {
            let login = p.login.clone().unwrap_or_default();
            let ticked = login.clone();
            view! {
                <label class="picker-row">
                    <input type="checkbox"
                        prop:checked=move || {
                            edit.picks.with(|p| {
                                p.as_ref().is_some_and(|p| p.users.contains(&ticked))
                            })
                        }
                        on:change={
                            let login = login.clone();
                            move |_| edit.picks.update(|p| {
                                if let Some(p) = p.as_mut()
                                    && !p.users.remove(&login)
                                {
                                    p.users.insert(login.clone());
                                }
                            })
                        } />
                    {avatar(&p)}
                    <span class="picker-name">
                        {p.name.clone().or(p.login.clone()).unwrap_or_default()}
                    </span>
                </label>
            }
        }
    };
    let team_row = move |t: TeamRef, removable: bool| {
        let slug = t.slug.clone();
        let ticked = slug.clone();
        view! {
            <label class="picker-row" class:disabled=!removable>
                <input type="checkbox" disabled=!removable
                    prop:checked=move || {
                        edit.picks.with(|p| {
                            p.as_ref().is_some_and(|p| p.teams.contains(&ticked))
                        })
                    }
                    on:change=move |_| edit.picks.update(|p| {
                        if let Some(p) = p.as_mut()
                            && !p.teams.remove(&slug)
                        {
                            p.teams.insert(slug.clone());
                        }
                    }) />
                {view! { <TeamIcon /> }.into_any()}
                <span class="picker-name">{t.name.clone()}</span>
            </label>
        }
    };
    view! {
        <div class="reviewer-edit" node_ref=root>
            <button type="button" class="reviewer-gear" title="Edit reviewers"
                aria-label="Edit reviewers"
                on:click=move |_| {
                    if edit.picks.with_untracked(|p| p.is_some()) {
                        apply();
                    } else {
                        edit.error.set(None);
                        edit.search.set(String::new());
                        edit.picks.set(Some(picks_from(&requested.get_untracked())));
                    }
                }>
                <svg
                    viewBox="0 0 16 16"
                    width="13"
                    height="13"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.5"
                    stroke-linecap="round"
                    aria-hidden="true"
                >
                    <circle cx="8" cy="8" r="2.5" />
                    <path d="M8 1v2M8 13v2M1 8h2M13 8h2M3 3l1.5 1.5M11.5 11.5 13 13M13 3l-1.5 1.5M4.5 11.5 3 13" />
                </svg>
            </button>
            {move || is_open.get().then(|| view! {
                <div class="reviewer-popover">
                    <input type="text" class="picker-search" placeholder="Find reviewers…"
                        prop:value=move || edit.search.get()
                        on:input=on_input />
                    <div class="picker-list">
                    {move || match candidates.get() {
                        None => view! { <p class="muted">"Loading…"</p> }.into_any(),
                        Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                        Some(Ok(c)) => {
                            let c = c.clone();
                            let requested = requested.get();
                            // The suggested reviewers first, then the
                            // assignable users and the teams; requested
                            // ones the query missed stay listed, ticked.
                            let shown_users: BTreeSet<&str> = c
                                .suggested
                                .iter()
                                .chain(&c.users)
                                .filter_map(|p| p.login.as_deref())
                                .collect();
                            let shown_teams: BTreeSet<&str> =
                                c.teams.iter().map(|t| t.slug.as_str()).collect();
                            let extra_users = requested.iter().filter_map(|r| match r {
                                RequestedReviewer::Person(p)
                                    if !p.login.as_deref().is_some_and(|l| shown_users.contains(l)) =>
                                {
                                    Some(p.clone())
                                }
                                _ => None,
                            });
                            let extra_teams = requested.iter().filter_map(|r| match r {
                                RequestedReviewer::Team(t)
                                    if !shown_teams.contains(t.slug.as_str()) =>
                                {
                                    Some((t.clone(), !t.slug.is_empty()))
                                }
                                _ => None,
                            });
                            view! {
                                {c.suggested.iter().cloned().map(&user_row).collect_view()}
                                {c.users.iter().cloned().map(&user_row).collect_view()}
                                {extra_users.map(&user_row).collect_view()}
                                {c.teams.iter().cloned().map(|t| team_row(t, true)).collect_view()}
                                {extra_teams.map(|(t, removable)| team_row(t, removable)).collect_view()}
                                {(c.suggested.is_empty() && c.users.is_empty() && c.teams.is_empty())
                                    .then(|| view! { <p class="muted">"No reviewers found"</p> })}
                            }.into_any()
                        }
                    }}
                    </div>
                </div>
            })}
        </div>
    }
}

/// The review decision and the reviewers, the sidebar's top section.
/// Unlike the checks — `sidebar_body` rebuilds them with the detail —
/// it mounts once per `PrView`, so the reviewer picker keeps its open
/// state, listeners and input focus across syncs. On an open PR the
/// viewer may update, a gear opens the picker and who already reviewed
/// gets a re-request button. Renders nothing without review data.
#[component]
pub fn SidebarReview(
    /// The PR's detail, or `None` while it loads.
    pr: Signal<Option<PrDetail>>,
    key: PrKey,
) -> impl IntoView {
    let edit = expect_context::<ReviewerEdit>();
    let key = StoredValue::new(key);
    let info = Memo::new(move |_| pr.get().and_then(|d| d.review));
    let author = Memo::new(move |_| pr.get().map(|d| d.summary.author).unwrap_or_default());
    let has_review = Memo::new(move |_| info.with(|i| i.is_some()));
    let editable = Memo::new(move |_| {
        pr.with(|d| {
            d.as_ref().is_some_and(|d| {
                d.summary.state == "OPEN" && d.review.as_ref().is_some_and(|i| i.can_edit_reviewers)
            })
        })
    });
    // A sync that brings a different requested list ends the overlay.
    Effect::new(move |_| {
        let clear = edit.optimistic.with(|o| {
            o.as_ref().is_some_and(|(base, _)| {
                info.with(|i| i.as_ref().is_some_and(|i| i.requested != *base))
            })
        });
        if clear {
            edit.optimistic.set(None);
        }
    });
    // A PR the picker can't act on anymore loses it without sending.
    Effect::new(move |_| {
        if !editable.get() && edit.picks.with_untracked(|p| p.is_some()) {
            edit.picks.set(None);
        }
    });
    // The list shows the optimistic overlay while its change is in
    // flight, else the synced requests.
    let requested = Memo::new(move |_| {
        edit.optimistic
            .get()
            .map(|(_, overlay)| overlay)
            .unwrap_or_else(|| {
                info.with(|i| i.as_ref().map(|i| i.requested.clone()).unwrap_or_default())
            })
    });
    let synced = move || {
        edit.optimistic
            .get_untracked()
            .map(|(base, _)| base)
            .unwrap_or_else(|| {
                info.with_untracked(|i| i.as_ref().map(|i| i.requested.clone()).unwrap_or_default())
            })
    };
    let rerequest = Callback::new(move |p: Person| {
        let login = p.login.clone().unwrap_or_default();
        let base = synced();
        let mut overlay = base.clone();
        overlay.push(RequestedReviewer::Person(p));
        send_reviewer_change(
            edit,
            key.get_value(),
            base,
            overlay,
            ReviewerChange {
                add_users: vec![login],
                ..ReviewerChange::default()
            },
        );
    });
    view! {
        {move || has_review.get().then(move || {
            let picker = move || {
                editable.get().then(move || {
                    view! { <ReviewerPicker key=key.get_value() requested=requested.into() /> }
                })
            };
            let list = move || match info.get() {
                Some(i) => {
                    let requested = requested.get();
                    let author = author.get();
                    let rows = reviewers(&i.reviews, &requested, &author);
                    if rows.is_empty() {
                        view! { <p class="muted">"No reviewers"</p> }.into_any()
                    } else {
                        let editable = editable.get();
                        view! {
                            <div class="reviewers">
                                {rows.iter().map(|r| reviewer_row(r, editable, rerequest)).collect_view()}
                            </div>
                        }
                        .into_any()
                    }
                }
                None => view! { <p class="muted">"No reviewers"</p> }.into_any(),
            };
            // A decision grenadine doesn't know says nothing useful.
            let decision = move || {
                info.with(|i| {
                    i.as_ref().and_then(|i| {
                        i.decision
                            .filter(|d| *d != ReviewDecision::Unknown)
                            .map(|d| {
                                view! { <p class=format!("review-decision {}", d.class())>{d.label()}</p> }
                            })
                    })
                })
            };
            view! {
                <section class="sidebar-review">
                    <div class="sidebar-head">
                        <h2>"Reviewers"</h2>
                        {picker}
                    </div>
                    {decision}
                    {move || edit.error.get().map(|e| view! { <p class="error">{e}</p> })}
                    {list}
                </section>
            }
        })}
    }
}

/// The checks on the PR's head, grouped by state. Each group's header
/// collapses or expands it; what the user changed lives in `CheckFolds`.
fn sidebar_checks(info: &ReviewInfo) -> AnyView {
    let CheckFolds(folds) = expect_context::<CheckFolds>();
    if info.checks.is_empty() {
        return view! {
            <section class="sidebar-checks">
                <h2>"Checks"</h2>
                <p class="muted">"No checks"</p>
            </section>
        }
        .into_any();
    }
    let groups = check_groups(&info.checks)
        .into_iter()
        .map(|(group, checks)| {
            let open = move || {
                folds.with(|f| {
                    f.get(&group).copied().unwrap_or_else(|| group.expanded())
                })
            };
            let count = checks.len();
            let row = |c: &Check| {
                let icon = c.status.map(|ci| view! { <CiIcon ci=ci /> });
                let name = match &c.url {
                    Some(url) => view! { <a href=url.clone() target="_blank" rel="noopener">{c.name.clone()}</a> }.into_any(),
                    None => view! { <span>{c.name.clone()}</span> }.into_any(),
                };
                view! { <li class="check">{icon}{name}</li> }
            };
            view! {
                <div class="check-group">
                    <button type="button" class="check-group-header"
                        aria-expanded=move || open().to_string()
                        on:click=move |_| folds.update(|f| {
                            let open = f.entry(group).or_insert_with(|| group.expanded());
                            *open = !*open;
                        })>
                        {group.icon().map(|ci| view! { <CiIcon ci=ci /> })}
                        {format!("{} ({count})", group.label())}
                    </button>
                    {move || open().then(|| view! {
                        <ul class="check-list">
                            {checks.iter().map(&row).collect_view()}
                        </ul>
                    })}
                </div>
            }
        })
        .collect_view();
    view! {
        <section class="sidebar-checks">
            <h2>"Checks"</h2>
            <div class="check-groups">{groups}</div>
        </section>
    }
    .into_any()
}

/// The sidebar body's checks, for open, closed and merged PRs alike.
/// The review section above them is `SidebarReview`, mounted once so
/// the reviewer picker survives these rebuilds.
pub fn sidebar_body(pr: &PrDetail) -> AnyView {
    match &pr.review {
        Some(info) => sidebar_checks(info),
        None => view! { <p class="muted">"No review information yet."</p> }.into_any(),
    }
}

/// The PR page's right sidebar. `overlay` floats it over the content on
/// windows too narrow for a column. The body's children are what the PR
/// shows; anything pinned under it is the caller's, so that it survives
/// the body's rebuilds when the PR's detail changes.
#[component]
pub fn PrSidebar(
    #[prop(into)] overlay: Signal<bool>,
    #[prop(into)] width: Signal<f64>,
    on_resize_start: Callback<web_sys::MouseEvent>,
    on_resize_reset: Callback<web_sys::MouseEvent>,
    children: Children,
) -> impl IntoView {
    view! {
        <aside
            class="pr-sidebar"
            class:overlay=move || overlay.get()
            style:width=move || format!("{}px", width.get())
        >
            <div
                class="sidebar-divider"
                role="separator"
                aria-orientation="vertical"
                title="Drag to resize"
                on:mousedown=move |e| on_resize_start.run(e)
                on:dblclick=move |e| on_resize_reset.run(e)
            ></div>
            {children()}
        </aside>
    }
}

/// One entry of the conversation.
enum Entry<'a> {
    Comment(&'a IssueComment),
    Review(&'a Review),
}

impl Entry<'_> {
    fn at(&self) -> &str {
        match self {
            Entry::Comment(c) => &c.created_at,
            Entry::Review(r) => &r.submitted_at,
        }
    }
}

/// The comments on the PR and the reviews, oldest first. Comment-only
/// reviews without a summary are left out: their comments are in the
/// diff.
fn entries(info: &ReviewInfo) -> Vec<Entry<'_>> {
    let mut out: Vec<Entry> = info.comments.iter().map(Entry::Comment).collect();
    out.extend(
        info.reviews
            .iter()
            .filter(|r| r.state != ReviewState::Commented || !r.body.trim().is_empty())
            .map(Entry::Review),
    );
    // RFC 3339 times in UTC sort as strings.
    out.sort_by(|a, b| a.at().cmp(b.at()));
    out
}

/// The comments on the PR itself and the reviews' summaries.
pub fn conversation(info: &ReviewInfo) -> impl IntoView + use<> {
    let items = entries(info)
        .into_iter()
        .map(|e| match e {
            Entry::Comment(c) => {
                let author = c.author.clone();
                view! {
                    <CommentFrame at=c.created_at.clone() url=c.url.clone() body=c.body.clone()>
                        <PersonChip person=author guess=false />
                    </CommentFrame>
                }
                .into_any()
            }
            Entry::Review(r) => {
                let (author, class, what) = (r.author.clone(), state_class(r.state), verb(r.state));
                view! {
                    <CommentFrame at=r.submitted_at.clone() url=r.url.clone() body=r.body.clone()>
                        <PersonChip person=author guess=false />
                        " " <span class=class>{what}</span>
                    </CommentFrame>
                }
                .into_any()
            }
        })
        .collect::<Vec<_>>();
    let count = items.len();
    view! {
        <details class="conversation" open>
            <summary>{match count {
                0 => "No comments".to_owned(),
                1 => "1 comment".to_owned(),
                n => format!("{n} comments"),
            }}</summary>
            {items}
            <NewComment />
        </details>
    }
}

/// A comment on the PR itself, behind a button until it's started.
#[component]
fn NewComment() -> impl IntoView {
    let ReviewTarget { key, .. } = expect_context::<ReviewTarget>();
    let draft = format!("comment.{}#{}", key.repo, key.number);
    let send = sender(move |body| {
        let key = key.clone();
        async move { api::add_comment(&key, body).await }
    });
    view! {
        <div class="new-comment">
            <ComposerToggle draft=draft label="Comment" placeholder="Comment on this PR…" send=send />
        </div>
    }
}

/// What the review panel shows, kept outside the views the PR's detail
/// drives. Its summary's draft and preview are in `OpenBoxes`.
#[derive(Clone, Copy)]
pub struct ReviewChoice {
    event: RwSignal<ReviewEvent>,
    open: RwSignal<bool>,
}

impl ReviewChoice {
    pub fn new() -> ReviewChoice {
        ReviewChoice {
            event: RwSignal::new(ReviewEvent::Comment),
            open: RwSignal::new(false),
        }
    }
}

/// The name of the draft of the summary of the viewer's review of `key`,
/// which also names submitting and discarding it in `OpenBoxes`.
fn summary_draft(key: &PrKey) -> String {
    format!("review.{}#{}", key.repo, key.number)
}

/// Submitting the viewer's review, or discarding it. `pending` counts the
/// comments in their pending review; `own` says the PR is the viewer's,
/// which they can't approve or request changes on.
#[component]
pub fn ReviewPanel(pending: Memo<usize>, own: Memo<bool>) -> impl IntoView {
    let ReviewTarget { key, reviewing } = expect_context::<ReviewTarget>();
    let links = expect_context::<PrLinks>();
    let boxes = expect_context::<OpenBoxes>();
    let draft = summary_draft(&key);
    let body = draft_signal(comment_draft_key(&draft));
    let ReviewChoice { event, open } = expect_context::<ReviewChoice>();
    let busy = {
        let draft = draft.clone();
        Signal::derive(move || boxes.sending(&draft))
    };
    let error = {
        let draft = draft.clone();
        Signal::derive(move || boxes.error(&draft))
    };
    let blocker = move || body.with(|b| submit_blocker(event.get(), b, pending.get()));
    let (key, name) = (StoredValue::new(key), StoredValue::new(draft.clone()));
    let submit = move |_| {
        if blocker().is_some() {
            return;
        }
        let review = SubmitReview {
            event: event.get_untracked(),
            body: body.get_untracked(),
        };
        let key = key.get_value();
        let name = name.get_value();
        boxes.send(
            &name.clone(),
            async move { api::submit_review(&key, &review).await },
            // The panel may be gone meanwhile; the summary's draft goes
            // either way.
            move || {
                body.try_set(String::new());
                boxes.close(&name);
                event.try_set(ReviewEvent::Comment);
                open.try_set(false);
            },
        );
    };
    let discard = move |_| {
        if !window()
            .confirm_with_message("Discard your pending review and its comments?")
            .unwrap_or(false)
        {
            return;
        }
        let key = key.get_value();
        name.with_value(|name| {
            boxes.send(name, async move { api::discard_review(&key).await }, || {})
        });
    };
    let choice = move |value: ReviewEvent, label: &'static str, hint: &'static str| {
        let disabled = own.get() && value != ReviewEvent::Comment;
        view! {
            <label class="review-choice" class:disabled=disabled title=if disabled { "You can't approve or request changes on your own PR" } else { hint }>
                <input type="radio" name="review-event" disabled=disabled
                    prop:checked=move || event.get() == value
                    on:change=move |_| event.set(value) />
                " " {label}
            </label>
        }
    };
    // Pointer presses outside the panel and Escape close the popup. It
    // listens to pointerdown, not click: the MarkdownEditor's tabs swap the
    // DOM under the pointer, so a click's target may already be detached and
    // `closest` can't place it in the panel; pointerdown also doesn't fire
    // when a selection drag that started in the textarea ends outside.
    let on_pointer = window_event_listener(leptos::ev::pointerdown, move |ev| {
        if !open.get_untracked() {
            return;
        }
        let inside = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|t| t.closest(".review-panel").ok().flatten())
            .is_some();
        if !inside {
            open.set(false);
        }
    });
    let on_key = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" {
            open.set(false);
        }
    });
    on_cleanup(move || {
        on_pointer.remove();
        on_key.remove();
    });
    view! {
        // Not a <details>: the panel remounts when the review comes or
        // goes, and a <details>' open state lives in the DOM.
        <div class="review-panel" class:open=move || open.get()>
            <button type="button" class="review-button" aria-expanded=move || open.get().to_string()
                on:click=move |_| open.update(|o| *o = !*o)>
                "Review"
                {move || (pending.get() > 0).then(|| view! { <span class="badge pending-count" title="Pending comments">{pending.get()}</span> })}
            </button>
            <div class="review-popup" class:hidden=move || !open.get()>
                <MarkdownEditor
                    text=body
                    draft=draft
                    repo=links.repo.clone()
                    label="Review summary"
                    placeholder="Leave a summary (optional for approvals)…"
                    rows=5
                />
                <div class="review-choices">
                    {choice(ReviewEvent::Comment, "Comment", "Feedback without approving")}
                    {choice(ReviewEvent::Approve, "Approve", "Approve merging these changes")}
                    {choice(ReviewEvent::RequestChanges, "Request changes", "Feedback that must be addressed before merging")}
                </div>
                {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                <div class="form-buttons">
                    <button type="button" class="primary" title=move || blocker().unwrap_or_default()
                        disabled=move || busy.get() || blocker().is_some() on:click=submit>
                        {move || if busy.get() { "Submitting…" } else { "Submit review" }}
                    </button>
                    {move || reviewing.get().then(|| view! {
                        <button type="button" class="danger" disabled=move || busy.get() on:click=discard>"Discard review"</button>
                    })}
                </div>
            </div>
        </div>
    }
}

/// Whether a PR with `conflicting`, `state` and `is_draft` may merge
/// now: `Ok(true)` when only by bypassing the branch's rules, or why it
/// may not. `methods` is how many merge methods the repository allows;
/// `conversations` blocks Blocked when the rules want `unresolved`
/// threads resolved.
fn mergeable(
    conflicting: Option<bool>,
    state: MergeState,
    is_draft: bool,
    methods: usize,
    conversations: bool,
    unresolved: usize,
    admin: bool,
) -> Result<bool, &'static str> {
    if is_draft {
        return Err("Drafts can't be merged.");
    }
    if methods == 0 {
        return Err("The repository allows no merge method.");
    }
    if conflicting == Some(true) {
        return Err("Resolve the conflicts first.");
    }
    match state {
        MergeState::Dirty => Err("Resolve the conflicts first."),
        MergeState::Clean | MergeState::HasHooks | MergeState::Unstable => Ok(false),
        MergeState::Blocked | MergeState::Behind if admin => Ok(true),
        MergeState::Blocked if conversations && unresolved > 0 => {
            Err("Resolve the conversations first.")
        }
        MergeState::Blocked => Err("The branch's rules block merging."),
        MergeState::Behind => Err("Bring the branch up to date first."),
        MergeState::Draft => Err("Drafts can't be merged."),
        MergeState::Unknown => Err("GitHub hasn't worked out yet whether this can be merged."),
    }
}

/// Whether the PR may be merged now: `Ok(true)` when only by bypassing the
/// branch's rules, which the viewer may do, or why it may not.
/// `unresolved` is how many review threads are unresolved.
pub fn can_merge(
    info: &ReviewInfo,
    is_draft: bool,
    unresolved: usize,
) -> Result<bool, &'static str> {
    let m = &info.merge;
    mergeable(
        m.conflicting,
        m.state,
        is_draft,
        m.methods.len(),
        m.requires_conversation_resolution,
        unresolved,
        m.can_merge_as_admin,
    )
}

/// Whether `p`, a PR below the merged one in its GitHub stack that
/// merging takes along, may merge: `Ok(Some(why))` when only by the
/// viewer bypassing the branch's rules, which `admin` allows, `Ok(None)`
/// when it may plainly, or `Err` why not.
fn below_mergeable(p: &StackMergePr, admin: bool) -> Result<Option<String>, String> {
    let n = p.number;
    if p.is_draft || p.state == MergeState::Draft {
        return Err(format!("#{n} is a draft."));
    }
    if p.conflicting == Some(true) || p.state == MergeState::Dirty {
        return Err(format!("#{n} conflicts with its base."));
    }
    let bypass = |why: String| match admin {
        true => Ok(Some(why)),
        false => Err(why),
    };
    match p.state {
        MergeState::Blocked => bypass(format!("#{n} is blocked by its branch's rules.")),
        MergeState::Behind => bypass(format!("#{n} is behind its base.")),
        MergeState::Unknown => Err(format!(
            "GitHub hasn't worked out yet whether #{n} can be merged."
        )),
        _ => Ok(None),
    }
}

/// A stack PR's readiness in a line: what's keeping it from merging, or
/// that nothing is.
fn below_status(p: &StackMergePr) -> &'static str {
    if p.is_draft || p.state == MergeState::Draft {
        return "Draft";
    }
    if p.conflicting == Some(true) || p.state == MergeState::Dirty {
        return "Conflicts";
    }
    match p.state {
        MergeState::Blocked => "Blocked by rules",
        MergeState::Behind => "Behind",
        MergeState::Unknown => "GitHub hasn't worked it out yet",
        _ => match p.ci {
            Some(CiStatus::Failing) => "CI failing",
            Some(CiStatus::Running) => "CI running",
            _ => "Ready",
        },
    }
}

/// Resolves `ms` milliseconds from now.
async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = web_sys::window()
            .expect("no window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// How long polling a merge gives up after, in milliseconds.
const MERGE_POLL_LIMIT: f64 = 10.0 * 60.0 * 1000.0;

/// Follows GitHub's asynchronous merge `uuid` until it lands, joins the
/// merge queue or fails. `warning` takes what went wrong after merging,
/// e.g. a branch that stayed.
async fn poll_merge(key: &PrKey, uuid: &str, warning: RwSignal<Option<String>>) -> api::Result<()> {
    let deadline = js_sys::Date::now() + MERGE_POLL_LIMIT;
    loop {
        let progress = api::merge_progress(key, uuid).await?;
        match progress.status {
            MergeStatus::Merged => {
                warning.try_set(progress.warning);
                return Ok(());
            }
            MergeStatus::Enqueued => return Ok(()),
            MergeStatus::Failed(why) => return Err(why),
            MergeStatus::Pending => {}
        }
        if js_sys::Date::now() > deadline {
            return Err(
                "GitHub hasn't finished the merge yet; the page updates once it does.".into(),
            );
        }
        sleep(2000).await;
    }
}

/// The merge method to offer first: GitHub's default for the viewer, which
/// follows the method they last merged with, else the first allowed.
pub fn pick_method(info: &ReviewInfo) -> Option<MergeMethod> {
    let allowed = |m: &MergeMethod| info.merge.methods.contains(m);
    info.merge
        .default_method
        .filter(allowed)
        .or(info.merge.methods.first().copied())
}

fn method_label(m: MergeMethod) -> &'static str {
    match m {
        MergeMethod::Merge => "Create a merge commit",
        MergeMethod::Squash => "Squash and merge",
        MergeMethod::Rebase => "Rebase and merge",
    }
}

/// What the merge box shows and what came of merging, kept above the
/// parts of the page that mount and unmount with it. `None` means the
/// default.
/// Sending the merge and why it failed are in `OpenBoxes`, under
/// `merge_name`.
#[derive(Clone, Copy)]
pub struct MergeChoice {
    method: RwSignal<Option<MergeMethod>>,
    /// The squash commit's message, once the viewer edits it.
    message: RwSignal<Option<(String, String)>>,
    delete_branch: RwSignal<Option<bool>>,
    /// The merge went through, so Merge stays disabled until the page
    /// shows the PR merged.
    merged: RwSignal<bool>,
    /// What went wrong after merging, e.g. deleting the branch.
    warning: RwSignal<Option<String>>,
    /// The direct-merge controls are revealed on a merge-queue branch.
    bypass: RwSignal<bool>,
}

impl MergeChoice {
    pub fn new() -> MergeChoice {
        MergeChoice {
            method: RwSignal::new(None),
            message: RwSignal::new(None),
            delete_branch: RwSignal::new(None),
            merged: RwSignal::new(false),
            warning: RwSignal::new(None),
            bypass: RwSignal::new(false),
        }
    }
}

/// The name of merging `key` in `OpenBoxes`.
fn merge_name(key: &PrKey) -> String {
    format!("merge.{}#{}", key.repo, key.number)
}

/// What went wrong after merging, as a banner. Outside the merge box,
/// which goes away once the page shows the PR merged.
pub fn merge_warning() -> impl IntoView {
    let MergeChoice { warning, .. } = expect_context::<MergeChoice>();
    move || {
        warning
            .get()
            .map(|w| view! { <p class="banner warn merge-warning">{w}</p> })
    }
}

/// The PR being merged, as the merge box needs it.
#[derive(Clone)]
pub struct MergeTarget {
    pub title: String,
    pub body: String,
    pub is_draft: bool,
    /// The latest version, which is what gets merged.
    pub head_oid: Option<String>,
    pub base_ref: String,
    /// Its head branch, which merging may delete.
    pub head_ref: String,
    /// The open PR whose branch this one targets, if it's stacked on one.
    pub open_parent: Option<u64>,
    /// The branch the bottom of the PR's stack targets.
    pub stack_base: String,
    /// Other open PRs target this one's branch.
    pub has_children: bool,
    /// Unresolved, submitted review threads.
    pub unresolved_threads: usize,
}

/// Merging the PR: the method, the squash message, deleting the branch.
/// `admin` forces the bypass styling when the viewer may skip rules the
/// state alone doesn't show, e.g. a merge queue.
fn merge_controls(info: &ReviewInfo, target: MergeTarget, admin: bool) -> impl IntoView + use<> {
    let ReviewTarget { key, .. } = expect_context::<ReviewTarget>();
    let boxes = expect_context::<OpenBoxes>();
    let MergeChoice {
        method,
        message,
        delete_branch,
        merged,
        warning,
        ..
    } = expect_context::<MergeChoice>();
    let name = StoredValue::new(merge_name(&key));
    let busy = move || name.with_value(|n| boxes.sending(n));
    let error = move || name.with_value(|n| boxes.error(n));
    // The open PRs below this one in its GitHub stack, which merging it
    // takes along, bottom first.
    let below = info
        .merge
        .stack
        .as_ref()
        .map(|s| s.below.as_slice())
        .unwrap_or_default()
        .to_vec();
    // In a GitHub stack, the merge lands on the stack's base; otherwise
    // on the PR's own base branch.
    let stack_base = info
        .merge
        .stack
        .as_ref()
        .map(|s| s.base_ref.clone())
        .unwrap_or_else(|| target.base_ref.clone());
    let count = below.len() + 1;
    let allowed = can_merge(info, target.is_draft, target.unresolved_threads);
    let belows: Vec<Result<Option<String>, String>> = below
        .iter()
        .map(|p| below_mergeable(p, info.merge.can_merge_as_admin))
        .collect();
    let blocked = allowed
        .err()
        .map(str::to_owned)
        .or_else(|| belows.iter().find_map(|r| r.clone().err()));
    let is_blocked = blocked.is_some();
    let stacked = count > 1;
    let fallback = pick_method(info);
    let chosen = move || method.get().or(fallback);
    let deletes = !info.merge.delete_branch_on_merge;
    // Deleting a branch that other PRs target would close them.
    let delete = move || delete_branch.get().unwrap_or(!target.has_children);
    let default_message = (
        format!("{} (#{})", target.title, key.number),
        target.body.clone(),
    );
    // Only whether it's being edited: the fields must not be rebuilt as
    // the message changes.
    let editing = Memo::new(move |_| message.with(Option::is_some));
    let editing = move || editing.get();
    let methods = info.merge.methods.clone();
    // The merge bypasses the rules when the PR's own state needs it or a
    // PR below it does.
    let admin = admin || allowed == Ok(true) || belows.iter().any(|r| matches!(r, Ok(Some(_))));
    let key = StoredValue::new(key);
    let head = target.head_oid.clone();
    // The PRs' numbers in the order they land, this one's last.
    let landing: Vec<u64> = below
        .iter()
        .map(|p| p.number)
        .chain(std::iter::once(key.with_value(|k| k.number)))
        .collect();
    let branches: Vec<String> = std::iter::once(target.head_ref.clone())
        .chain(below.iter().map(|p| p.head_ref.clone()))
        .collect();
    // The viewer may bypass the rules: the dialog says which ones,
    // including the PRs below it that need the bypass.
    let bypassed = admin.then(|| {
        let mut why = vec![merge_status(info, target.unresolved_threads).1];
        why.extend(belows.iter().filter_map(|r| r.clone().ok().flatten()));
        why.join("\n")
    });
    let merge_base = stack_base.clone();
    let merge = move |_| {
        let (Some(m), Some(head)) = (chosen(), head.clone()) else {
            return;
        };
        let (last, rest) = landing.split_last().expect("the merged PR lands");
        let numbers = match rest.is_empty() {
            true => format!("#{last}"),
            false => format!(
                "{}, then #{last}",
                rest.iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let what = format!("{} {numbers} into {merge_base}", method_label(m));
        let what = match &bypassed {
            Some(why) => format!("{what}, bypassing the branch's rules?\n\n{why}"),
            None => format!("{what}?"),
        };
        if !window().confirm_with_message(&what).unwrap_or(false) {
            return;
        }
        let (title, body) = message
            .get_untracked()
            .filter(|_| m == MergeMethod::Squash && !stacked)
            .unzip();
        let request = MergeRequest {
            method: m,
            head_oid: head,
            title,
            body,
            delete_branch: deletes && delete(),
            bypass_rules: admin,
        };
        warning.set(None);
        let key = key.get_value();
        let merge = async move {
            let done = api::merge(&key, &request).await?;
            poll_merge(&key, &done.uuid, warning).await
        };
        name.with_value(|n| {
            boxes.send(n, merge, move || {
                merged.try_set(true);
            })
        });
    };
    view! {
        <div class="merge-controls">
            {match below.is_empty() {
                false => stack_merge_banner(below.clone(), stack_base.clone(), key.with_value(|k| k.number)).into_any(),
                true => target.open_parent.map(|p| view! {
                    <p class="banner warn">{format!(
                        "This PR targets the branch of #{p}, so merging it puts its changes there, not into {}.",
                        target.stack_base
                    )}</p>
                }).into_any(),
            }}
            {blocked.map(|why| view! { <p class="muted merge-blocked">{why}</p> })}
            <div class="form-buttons">
                <select aria-label="Merge method"
                    on:change=move |ev| method.set(MergeMethod::from_github(&event_target_value(&ev)))>
                    {methods.into_iter().map(|m| view! {
                        <option value=m.as_github() prop:selected=move || chosen() == Some(m)>{method_label(m)}</option>
                    }).collect_view()}
                </select>
                <button type="button" class=if admin { "danger" } else { "primary" }
                    disabled=move || busy() || merged.get() || is_blocked || target.head_oid.is_none()
                    title=if admin { "Merges without waiting for the branch's rules, which you may bypass" } else { "" }
                    on:click=merge>
                    {move || match (busy(), merged.get(), admin) {
                        (true, _, _) => "Merging…".to_owned(),
                        (false, true, _) => "Merged".to_owned(),
                        (false, false, true) if count > 1 => format!("Merge {count} PRs, bypassing rules"),
                        (false, false, false) if count > 1 => format!("Merge {count} PRs"),
                        (false, false, true) => "Merge, bypassing rules".to_owned(),
                        (false, false, false) => "Merge".to_owned(),
                    }}
                </button>
                {deletes.then(|| view! {
                    <label class="toggle">
                        <input type="checkbox" prop:checked=delete
                            on:change=move |ev| delete_branch.set(Some(event_target_checked(&ev))) />
                        {match branches.len() {
                            1 => " Delete the branch".to_owned(),
                            _ => format!(" Delete the branches ({})", branches.join(", ")),
                        }}
                    </label>
                })}
                {move || (chosen() == Some(MergeMethod::Squash) && !editing() && !stacked).then(|| {
                    let default_message = default_message.clone();
                    view! {
                        <button type="button" class="link" on:click=move |_| message.set(Some(default_message.clone()))>"Edit the commit message"</button>
                    }
                })}
            </div>
            {move || (chosen() == Some(MergeMethod::Squash) && editing()).then(|| view! {
                <div class="merge-message">
                    <input type="text" aria-label="Commit title"
                        prop:value=move || message.with(|m| m.as_ref().map(|m| m.0.clone()).unwrap_or_default())
                        on:input=move |ev| message.update(|m| if let Some(m) = m { m.0 = event_target_value(&ev) }) />
                    <textarea rows="6" aria-label="Commit message"
                        prop:value=move || message.with(|m| m.as_ref().map(|m| m.1.clone()).unwrap_or_default())
                        on:input=move |ev| message.update(|m| if let Some(m) = m { m.1 = event_target_value(&ev) })></textarea>
                </div>
            })}
            {move || error().map(|e| view! { <p class="error">{e}</p> })}
        </div>
    }
}

/// That merging `number` merges `below`, the open PRs under it in its
/// GitHub stack, and a readiness row per one.
fn stack_merge_banner(
    below: Vec<StackMergePr>,
    base_ref: String,
    number: u64,
) -> impl IntoView + use<> {
    let repo = expect_context::<ReviewTarget>().key.repo;
    let links = below
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let key = PrKey {
                repo: repo.clone(),
                number: p.number,
            };
            let sep = match i {
                0 => "",
                i if i == below.len() - 1 => " and ",
                _ => ", ",
            };
            view! { {sep}<a href=hash_for(&key)>{format!("#{}", p.number)}</a> }
        })
        .collect_view();
    let rows = below
        .iter()
        .map(|p| {
            let key = PrKey {
                repo: repo.clone(),
                number: p.number,
            };
            view! {
                <li>
                    <a href=hash_for(&key)>{format!("#{} {}", p.number, p.title)}</a>
                    {format!(" — {}", below_status(p))}
                </li>
            }
        })
        .collect_view();
    view! {
        <p class="banner warn">{format!("Merging #{number} also merges ")}{links}" below it. Each lands on "{base_ref}" separately, in stack order, and if one can't merge, neither does."</p>
        <ul class="merge-stack">{rows}</ul>
    }
}

/// What a PR on a merge-queue branch offers next: joining the queue now,
/// joining it once its requirements are met, or why neither is on.
#[derive(Clone, Copy)]
enum QueueAction {
    Enqueue,
    AutoMerge,
    Blocked(&'static str),
}

/// The primary action on a merge-queue branch, or its refusal. Uses the
/// same refusals as `can_merge`.
fn queue_action(info: &ReviewInfo, is_draft: bool, unresolved: usize) -> QueueAction {
    let m = &info.merge;
    if is_draft || m.state == MergeState::Draft {
        return QueueAction::Blocked("Drafts can't be merged.");
    }
    if m.conflicting == Some(true) || m.state == MergeState::Dirty {
        return QueueAction::Blocked("Resolve the conflicts first.");
    }
    match m.state {
        MergeState::Clean | MergeState::HasHooks | MergeState::Unstable => QueueAction::Enqueue,
        MergeState::Behind => {
            if m.queue.as_ref().is_some_and(|q| q.can_enable_auto_merge) {
                QueueAction::AutoMerge
            } else {
                QueueAction::Blocked("Bring the branch up to date first.")
            }
        }
        MergeState::Blocked => {
            if m.queue.as_ref().is_some_and(|q| q.can_enable_auto_merge) {
                QueueAction::AutoMerge
            } else if m.requires_conversation_resolution && unresolved > 0 {
                QueueAction::Blocked("Resolve the conversations first.")
            } else {
                QueueAction::Blocked("The branch's rules block merging.")
            }
        }
        _ => QueueAction::Blocked("GitHub hasn't worked out yet whether this can be merged."),
    }
}

/// The name of the queue change of `key` in `OpenBoxes`.
fn queue_name(key: &PrKey) -> String {
    format!("queue.{}#{}", key.repo, key.number)
}

/// The merge box of a PR on a merge-queue branch: joining or leaving the
/// queue, and for admins jumping it or bypassing it, which reveals the
/// usual merge controls.
fn queue_controls(info: &ReviewInfo, target: MergeTarget) -> impl IntoView + use<> {
    let ReviewTarget { key, .. } = expect_context::<ReviewTarget>();
    let boxes = expect_context::<OpenBoxes>();
    let MergeChoice {
        bypass, warning, ..
    } = expect_context::<MergeChoice>();
    let queue = info
        .merge
        .queue
        .clone()
        .expect("queue controls need a merge-queue branch");
    // The open PRs below this one in its GitHub stack, which queueing it
    // takes along, bottom first.
    let below = info
        .merge
        .stack
        .as_ref()
        .map(|s| s.below.as_slice())
        .unwrap_or_default()
        .to_vec();
    let count = below.len() + 1;
    let numbers: Vec<String> = below
        .iter()
        .map(|p| format!("#{}", p.number))
        .chain(std::iter::once(format!("#{}", key.number)))
        .collect();
    let name = StoredValue::new(queue_name(&key));
    let busy = move || name.with_value(|n| boxes.sending(n));
    let error = move || name.with_value(|n| boxes.error(n));
    let key = StoredValue::new(key);
    let base_ref = target.base_ref.clone();
    let confirm = |what: String| window().confirm_with_message(&what).unwrap_or(false);
    let enqueue = {
        let head = target.head_oid.clone();
        let base_ref = base_ref.clone();
        move |jump: bool| {
            let Some(head) = head.clone() else {
                return;
            };
            let number = key.with_value(|k| k.number);
            let what = match (jump, numbers.len()) {
                (true, _) => {
                    format!("Add #{number} to the front of the merge queue for {base_ref}?")
                }
                (false, 1) => format!("Add #{number} to the merge queue for {base_ref}?"),
                (false, _) => format!(
                    "Add {} to the merge queue for {base_ref}?",
                    numbers.join(", ")
                ),
            };
            if !confirm(what) {
                return;
            }
            let request = Enqueue {
                head_oid: head,
                jump,
            };
            let key = key.get_value();
            let enqueue = async move {
                match api::enqueue(&key, &request).await? {
                    // A stacked PR queues through the merge-async API,
                    // which is polled like a merge.
                    Some(started) => poll_merge(&key, &started.uuid, warning).await,
                    None => Ok(()),
                }
            };
            name.with_value(|n| boxes.send(n, enqueue, || {}));
        }
    };
    let merge_when_ready = {
        let head = target.head_oid.clone();
        move |_| {
            let Some(head) = head.clone() else {
                return;
            };
            let number = key.with_value(|k| k.number);
            if !confirm(format!("Merge #{number} when its requirements are met?")) {
                return;
            }
            let request = AutoMerge { head_oid: head };
            let key = key.get_value();
            name.with_value(|n| {
                boxes.send(
                    n,
                    async move { api::enable_auto_merge(&key, &request).await },
                    || {},
                )
            });
        }
    };
    let cancel_auto_merge = move |_| {
        let number = key.with_value(|k| k.number);
        if !confirm(format!(
            "Stop #{number} joining the merge queue when ready?"
        )) {
            return;
        }
        let key = key.get_value();
        name.with_value(|n| {
            boxes.send(n, async move { api::disable_auto_merge(&key).await }, || {})
        });
    };
    let dequeue = move |_| {
        let number = key.with_value(|k| k.number);
        if !confirm(format!("Remove #{number} from the merge queue?")) {
            return;
        }
        let key = key.get_value();
        name.with_value(|n| boxes.send(n, async move { api::dequeue(&key).await }, || {}));
    };
    let method_line = queue.method.map(|m| match m {
        MergeMethod::Squash => "The merge queue squashes and merges this PR.",
        MergeMethod::Rebase => "The merge queue rebases and merges this PR.",
        MergeMethod::Merge => "The merge queue merges this PR with a merge commit.",
    });
    let action = queue_action(info, target.is_draft, target.unresolved_threads);
    let blocked = match action {
        QueueAction::Blocked(why) => Some(why),
        _ => None,
    };
    // GitHub decides whether a blocked PR may jump; the state only rules
    // out drafts, conflicts and not-yet-known states.
    let jumpable = info.merge.can_merge_as_admin
        && !target.is_draft
        && info.merge.conflicting != Some(true)
        && matches!(
            info.merge.state,
            MergeState::Clean
                | MergeState::HasHooks
                | MergeState::Unstable
                | MergeState::Blocked
                | MergeState::Behind
        );
    let admin = info.merge.can_merge_as_admin;
    let headless = target.head_oid.is_none();
    let in_queue = queue.entry.is_some();
    // GitHub's merge queue has no jump or merge-when-ready for a stack.
    let unstacked = info.merge.stack.is_none();
    let auto_merge = queue.auto_merge && !in_queue;
    let can_disable = queue.can_disable_auto_merge;
    let stack_base = info
        .merge
        .stack
        .as_ref()
        .map(|s| s.base_ref.clone())
        .unwrap_or_default();
    let info = info.clone();
    let bypass_target = target.clone();
    view! {
        <div class="merge-controls">
            {(!below.is_empty()).then(|| stack_merge_banner(below.clone(), stack_base.clone(), key.with_value(|k| k.number)))}
            {method_line.map(|line| view! { <p class="muted queue-method">{line}</p> })}
            {blocked.map(|why| view! { <p class="muted merge-blocked">{why}</p> })}
            <div class="form-buttons">
                {in_queue.then(|| view! {
                    <button type="button" class="danger" disabled=busy
                        on:click=dequeue>
                        {move || if busy() { "Removing…" } else { "Remove from queue" }}
                    </button>
                })}
                {auto_merge.then(|| view! {
                    <button type="button" disabled=move || busy() || !can_disable
                        title=if can_disable { "" } else { "You can't disable auto-merge" }
                        on:click=cancel_auto_merge>
                        {move || if busy() { "Cancelling…" } else { "Cancel" }}
                    </button>
                })}
                {(!in_queue && !auto_merge).then(|| {
                    match action {
                        QueueAction::Enqueue => {
                            let enqueue = enqueue.clone();
                            view! {
                                <button type="button" class="primary"
                                    disabled=move || busy() || headless
                                    on:click=move |_| enqueue(false)>
                                    {move || match (busy(), count) {
                                        (true, _) => "Adding…".to_owned(),
                                        (false, 1) => "Add to merge queue".to_owned(),
                                        (false, n) => format!("Add {n} PRs to merge queue"),
                                    }}
                                </button>
                            }.into_any()
                        }
                        QueueAction::AutoMerge if unstacked => view! {
                            <button type="button" class="primary"
                                disabled=move || busy() || headless
                                on:click=merge_when_ready>
                                {move || if busy() { "Enabling…" } else { "Merge when ready" }}
                            </button>
                        }.into_any(),
                        _ => ().into_any(),
                    }
                })}
                {(unstacked && jumpable && !in_queue && !auto_merge).then(|| {
                    let enqueue = enqueue.clone();
                    view! {
                        <button type="button" disabled=move || busy() || headless
                            title="Jumps ahead of the PRs already queued"
                            on:click=move |_| enqueue(true)>
                            "Add to front of queue"
                        </button>
                    }
                })}
                {(admin && !in_queue && !auto_merge).then(|| view! {
                    <button type="button" aria-expanded=move || bypass.get().to_string()
                        on:click=move |_| bypass.update(|b| *b = !*b)>
                        "Merge now, bypassing the queue…"
                    </button>
                })}
            </div>
            {move || bypass.get().then(|| merge_controls(&info, bypass_target.clone(), true).into_any())}
            {move || error().map(|e| view! { <p class="error">{e}</p> })}
        </div>
    }
}

/// Whether the PR can be merged and merging it. Only for open PRs; the
/// checks are in the sidebar.
pub fn merge_box(info: &ReviewInfo, target: MergeTarget) -> impl IntoView + use<> {
    let (class, text) = merge_status(info, target.unresolved_threads);
    let controls = if info.merge.queue.is_some() {
        queue_controls(info, target.clone()).into_any()
    } else {
        merge_controls(info, target, false).into_any()
    };
    view! {
        <section class="merge-box">
            <p class=format!("merge-status {class}")>{text}</p>
            {controls}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{MergeInfo, MergeQueueInfo, Person, QueueEntry};

    use super::*;

    fn review(login: &str, state: ReviewState, at: &str) -> Review {
        Review {
            author: Person {
                login: Some(login.into()),
                name: None,
                avatar_url: None,
            },
            state,
            body: String::new(),
            submitted_at: at.into(),
            url: String::new(),
            commit: None,
        }
    }

    fn info() -> ReviewInfo {
        ReviewInfo {
            viewer: "me".into(),
            decision: None,
            reviews: vec![],
            requested: vec![],
            can_edit_reviewers: false,
            comments: vec![],
            checks: vec![],
            ci: None,
            pending_review: None,
            merge: MergeInfo {
                conflicting: Some(false),
                state: MergeState::Clean,
                methods: vec![],
                default_method: None,
                delete_branch_on_merge: false,
                can_merge_as_admin: false,
                requires_conversation_resolution: false,
                queue: None,
                stack: None,
            },
        }
    }

    #[test]
    fn a_comment_does_not_override_a_verdict() {
        use ReviewState::*;
        let reviews = [
            review("a", Approved, "1"),
            review("b", Commented, "2"),
            review("a", Commented, "3"),
            review("author", Commented, "4"),
            review("b", ChangesRequested, "5"),
            review("c", Commented, "6"),
            review("c", Commented, "7"),
        ];
        let latest: Vec<_> = latest_reviews(&reviews, "author")
            .into_iter()
            .map(|r| {
                (
                    r.author.login.clone().unwrap(),
                    r.state,
                    r.submitted_at.clone(),
                )
            })
            .collect();
        assert_eq!(
            latest,
            [
                ("a".into(), Approved, "1".into()),
                ("b".into(), ChangesRequested, "5".into()),
                ("c".into(), Commented, "7".into()),
            ]
        );
    }

    #[test]
    fn merge_statuses() {
        let mut i = info();
        assert_eq!(merge_status(&i, 0).0, "ok");
        i.merge.state = MergeState::Blocked;
        i.decision = Some(ReviewDecision::ReviewRequired);
        assert_eq!(
            merge_status(&i, 0),
            (
                "bad",
                "Merging is blocked: it needs an approving review.".into()
            )
        );
        i.decision = Some(ReviewDecision::ChangesRequested);
        assert_eq!(
            merge_status(&i, 0).1,
            "Merging is blocked: changes were requested."
        );
        i.merge.state = MergeState::Dirty;
        assert_eq!(merge_status(&i, 0).0, "bad");
        assert!(merge_status(&i, 0).1.contains("conflicts"));
        i.merge.state = MergeState::Blocked;
        // Conflicts win over everything else.
        i.merge.conflicting = Some(true);
        assert_eq!(merge_status(&i, 0).0, "bad");
        assert!(merge_status(&i, 0).1.contains("conflicts"));
        i.merge.conflicting = None;
        i.merge.state = MergeState::Unknown;
        assert_eq!(merge_status(&i, 0).0, "warn");
    }

    #[test]
    fn why_merging_is_blocked() {
        let mut i = info();
        i.merge.state = MergeState::Blocked;
        assert_eq!(
            merge_status(&i, 0).1,
            "Merging is blocked: required checks or rules aren't satisfied."
        );
        // Every known reason is listed.
        i.decision = Some(ReviewDecision::ReviewRequired);
        i.merge.requires_conversation_resolution = true;
        assert_eq!(
            merge_status(&i, 2).1,
            "Merging is blocked: it needs an approving review; \
             2 conversations must be resolved."
        );
        // One unresolved conversation reads in the singular.
        i.decision = None;
        assert_eq!(
            merge_status(&i, 1).1,
            "Merging is blocked: 1 conversation must be resolved."
        );
        // Without unresolved conversations, or without the rule, the
        // reason isn't known.
        assert_eq!(
            merge_status(&i, 0).1,
            "Merging is blocked: required checks or rules aren't satisfied."
        );
        i.merge.requires_conversation_resolution = false;
        assert_eq!(
            merge_status(&i, 2).1,
            "Merging is blocked: required checks or rules aren't satisfied."
        );
    }

    #[test]
    fn when_a_pr_can_be_merged() {
        let mut i = info();
        i.merge.methods = vec![MergeMethod::Squash];
        assert_eq!(can_merge(&i, false, 0), Ok(false));
        assert!(can_merge(&i, true, 0).is_err());
        i.merge.state = MergeState::Unstable;
        assert_eq!(can_merge(&i, false, 0), Ok(false));
        i.merge.state = MergeState::Blocked;
        assert_eq!(
            can_merge(&i, false, 0),
            Err("The branch's rules block merging.")
        );
        i.merge.requires_conversation_resolution = true;
        assert_eq!(
            can_merge(&i, false, 1),
            Err("Resolve the conversations first.")
        );
        i.merge.can_merge_as_admin = true;
        assert_eq!(can_merge(&i, false, 1), Ok(true));
        // Conflicts can't be bypassed.
        i.merge.conflicting = Some(true);
        assert!(can_merge(&i, false, 0).is_err());
        i.merge.conflicting = None;
        i.merge.state = MergeState::Unknown;
        assert!(can_merge(&i, false, 0).is_err());
        i.merge.state = MergeState::Clean;
        i.merge.methods.clear();
        assert!(can_merge(&i, false, 0).is_err());
    }

    #[test]
    fn picks_a_merge_method() {
        use MergeMethod::*;
        let mut i = info();
        i.merge.methods = vec![Merge, Squash];
        assert_eq!(pick_method(&i), Some(Merge));
        i.merge.default_method = Some(Squash);
        assert_eq!(pick_method(&i), Some(Squash));
        // A method the repository no longer allows isn't offered.
        i.merge.default_method = Some(Rebase);
        assert_eq!(pick_method(&i), Some(Merge));
    }

    fn queue() -> MergeQueueInfo {
        MergeQueueInfo {
            method: Some(MergeMethod::Squash),
            entry: None,
            auto_merge: false,
            can_enable_auto_merge: false,
            can_disable_auto_merge: false,
        }
    }

    #[test]
    fn merge_queue_statuses() {
        let mut i = info();
        i.merge.queue = Some(queue());
        assert_eq!(
            merge_status(&i, 0),
            ("ok", "Ready to join the merge queue.".into())
        );
        i.merge.queue.as_mut().unwrap().auto_merge = true;
        assert_eq!(
            merge_status(&i, 0),
            ("ok", "Will join the merge queue when ready.".into())
        );
        i.merge.queue.as_mut().unwrap().entry = Some(QueueEntry {
            position: 3,
            state: QueueState::AwaitingChecks,
        });
        assert_eq!(
            merge_status(&i, 0),
            (
                "warn",
                "In the merge queue: position 3 (waiting for checks).".into()
            )
        );
        i.merge.queue.as_mut().unwrap().entry = Some(QueueEntry {
            position: 1,
            state: QueueState::Mergeable,
        });
        assert_eq!(merge_status(&i, 0).0, "ok");
        i.merge.queue.as_mut().unwrap().entry = Some(QueueEntry {
            position: 1,
            state: QueueState::Unmergeable,
        });
        assert_eq!(merge_status(&i, 0).0, "bad");
    }

    #[test]
    fn the_queue_action() {
        let mut i = info();
        i.merge.queue = Some(queue());
        assert!(matches!(queue_action(&i, false, 0), QueueAction::Enqueue));
        i.merge.state = MergeState::Unstable;
        assert!(matches!(queue_action(&i, false, 0), QueueAction::Enqueue));
        i.merge.state = MergeState::Blocked;
        assert_eq!(
            match queue_action(&i, false, 0) {
                QueueAction::Blocked(why) => Some(why),
                _ => None,
            },
            Some("The branch's rules block merging.")
        );
        i.merge.requires_conversation_resolution = true;
        assert!(matches!(
            queue_action(&i, false, 2),
            QueueAction::Blocked("Resolve the conversations first.")
        ));
        i.merge.queue.as_mut().unwrap().can_enable_auto_merge = true;
        assert!(matches!(queue_action(&i, false, 2), QueueAction::AutoMerge));
        i.merge.state = MergeState::Behind;
        assert!(matches!(queue_action(&i, false, 0), QueueAction::AutoMerge));
        i.merge.queue.as_mut().unwrap().can_enable_auto_merge = false;
        assert!(matches!(
            queue_action(&i, false, 0),
            QueueAction::Blocked("Bring the branch up to date first.")
        ));
        i.merge.state = MergeState::Clean;
        i.merge.conflicting = Some(true);
        assert!(matches!(
            queue_action(&i, false, 0),
            QueueAction::Blocked("Resolve the conflicts first.")
        ));
        i.merge.conflicting = Some(false);
        assert!(matches!(
            queue_action(&i, true, 0),
            QueueAction::Blocked("Drafts can't be merged.")
        ));
        i.merge.state = MergeState::Unknown;
        assert!(matches!(
            queue_action(&i, false, 0),
            QueueAction::Blocked(_)
        ));
    }

    #[test]
    fn groups_the_checks() {
        let check = |name: &str, status| Check {
            name: name.into(),
            status,
            url: None,
        };
        let groups: Vec<(CheckGroup, Vec<String>)> = check_groups(&[
            check("beta", Some(CiStatus::Passing)),
            check("e2e", Some(CiStatus::Failing)),
            check("Alpha", Some(CiStatus::Passing)),
            check("docs", None),
            check("Build", Some(CiStatus::Failing)),
        ])
        .into_iter()
        .map(|(g, c)| (g, c.into_iter().map(|c| c.name).collect()))
        .collect();
        // Groups come in a fixed order, sorted by name and without
        // empty ones (none is running here).
        assert_eq!(
            groups,
            [
                (
                    CheckGroup::Failing,
                    vec!["Build".to_owned(), "e2e".to_owned()]
                ),
                (
                    CheckGroup::Passed,
                    vec!["Alpha".to_owned(), "beta".to_owned()]
                ),
                (CheckGroup::Skipped, vec!["docs".to_owned()]),
            ]
        );
        assert!(check_groups(&[]).is_empty());
    }

    /// Who the sidebar lists: the reviewed in first-review order, then
    /// the requested. A re-requested reviewer shows once, as pending.
    #[test]
    fn reviewer_order_and_rerequests() {
        let mut i = info();
        i.reviews = vec![
            review("a", ReviewState::Approved, "1"),
            review("b", ReviewState::ChangesRequested, "2"),
        ];
        let person = |login: &str| {
            RequestedReviewer::Person(Person {
                login: Some(login.into()),
                name: None,
                avatar_url: None,
            })
        };
        i.requested = vec![
            person("c"),
            // Reviewed, then asked again: shows once, as pending.
            person("a"),
            RequestedReviewer::Team(TeamRef {
                name: "team".into(),
                slug: "team-slug".into(),
            }),
            RequestedReviewer::Legacy("old".into()),
        ];
        let rows: Vec<(Option<String>, &'static str)> =
            reviewers(&i.reviews, &i.requested, "author")
                .into_iter()
                .map(|r| match r {
                    Reviewer::Reviewed(v) => (v.author.login.clone(), verdict(v.state)),
                    Reviewer::Requested(RequestedReviewer::Person(p)) => {
                        (p.login.clone(), "Pending")
                    }
                    Reviewer::Requested(RequestedReviewer::Team(t)) => {
                        (Some(t.name.clone()), "Pending")
                    }
                    Reviewer::Requested(RequestedReviewer::Legacy(n)) => {
                        (Some(n.clone()), "Pending")
                    }
                })
                .collect();
        assert_eq!(
            rows,
            [
                (Some("b".into()), "Changes requested"),
                (Some("c".into()), "Pending"),
                (Some("a".into()), "Pending"),
                (Some("team".into()), "Pending"),
                (Some("old".into()), "Pending"),
            ]
        );
        // With neither reviews nor requests the list is empty.
        let empty = info();
        assert!(reviewers(&empty.reviews, &empty.requested, "author").is_empty());
    }

    #[test]
    fn conversation_skips_bare_comment_reviews() {
        let mut i = info();
        let mut summary = review("a", ReviewState::Commented, "2026-01-01T00:03:00Z");
        summary.body = "Overall fine".into();
        i.reviews = vec![
            review("a", ReviewState::Commented, "2026-01-01T00:01:00Z"),
            review("b", ReviewState::Approved, "2026-01-01T00:04:00Z"),
            summary,
        ];
        i.comments = vec![IssueComment {
            author: Person::default(),
            body: "hi".into(),
            created_at: "2026-01-01T00:02:00Z".into(),
            url: String::new(),
        }];
        let at: Vec<_> = entries(&i).iter().map(|e| e.at().to_owned()).collect();
        assert_eq!(
            at,
            [
                "2026-01-01T00:02:00Z",
                "2026-01-01T00:03:00Z",
                "2026-01-01T00:04:00Z"
            ]
        );
    }

    fn below_pr(number: u64, state: MergeState) -> StackMergePr {
        StackMergePr {
            number,
            title: format!("PR {number}"),
            head_ref: format!("dev/{number}"),
            is_draft: false,
            conflicting: Some(false),
            state,
            ci: None,
        }
    }

    #[test]
    fn below_mergeability() {
        // Clean PRs merge along; blocked and behind ones only with the
        // viewer's admin bypass.
        assert_eq!(
            below_mergeable(&below_pr(1, MergeState::Clean), false),
            Ok(None)
        );
        assert_eq!(
            below_mergeable(&below_pr(2, MergeState::Blocked), false),
            Err("#2 is blocked by its branch's rules.".into())
        );
        assert_eq!(
            below_mergeable(&below_pr(2, MergeState::Blocked), true),
            Ok(Some("#2 is blocked by its branch's rules.".into()))
        );
        assert_eq!(
            below_mergeable(&below_pr(3, MergeState::Behind), false),
            Err("#3 is behind its base.".into())
        );
        assert_eq!(
            below_mergeable(&below_pr(3, MergeState::Behind), true),
            Ok(Some("#3 is behind its base.".into()))
        );
        // Drafts, conflicts and undecided states block outright.
        let mut draft = below_pr(4, MergeState::Clean);
        draft.is_draft = true;
        assert_eq!(below_mergeable(&draft, true), Err("#4 is a draft.".into()));
        let mut conflict = below_pr(5, MergeState::Clean);
        conflict.conflicting = Some(true);
        assert_eq!(
            below_mergeable(&conflict, true),
            Err("#5 conflicts with its base.".into())
        );
        assert_eq!(
            below_mergeable(&below_pr(6, MergeState::Unknown), true),
            Err("GitHub hasn't worked out yet whether #6 can be merged.".into())
        );
    }
}
