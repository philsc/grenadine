//! The Auto-reviews tab: the reviews of the viewer's own PRs that the
//! server started, each in an unattended session of its own. Also the
//! section of a PR's sidebar that shows its latest review and asks for
//! another.

use grenadine_core::api::{AutoReviewEntry, AutoReviewState, AutoRunKind, PrKey};
use leptos::prelude::*;

use crate::agents::{cost, status_class, status_label};
use crate::{GithubLink, Route, Updates, ago, api, github_url, hash_for};

/// The tab's page.
pub const LIST: &str = "#/auto-reviews";

/// The page of the review that session `id` is doing.
pub fn hash(id: &str) -> String {
    format!("{LIST}/{id}")
}

fn state_label(kind: AutoRunKind, s: AutoReviewState) -> &'static str {
    let fix = kind == AutoRunKind::Fix;
    match s {
        AutoReviewState::Queued => "queued",
        AutoReviewState::Running if fix => "addressing…",
        AutoReviewState::Running => "reviewing…",
        // A fixing run that answered the comments and changed nothing.
        AutoReviewState::Posted if fix => "answered",
        AutoReviewState::Posted => "posted",
        AutoReviewState::Pushed => "pushed",
        AutoReviewState::Superseded => "superseded",
        AutoReviewState::Unposted if fix => "did nothing",
        AutoReviewState::Unposted => "didn't post",
        AutoReviewState::Failed => "failed",
        AutoReviewState::Cancelled => "cancelled",
    }
}

fn state_class(s: AutoReviewState) -> &'static str {
    match s {
        AutoReviewState::Queued => "agent-status waiting",
        AutoReviewState::Running => "agent-status running",
        AutoReviewState::Posted | AutoReviewState::Pushed => "agent-status posted",
        AutoReviewState::Unposted => "agent-status waiting",
        AutoReviewState::Failed => "agent-status failed",
        AutoReviewState::Superseded | AutoReviewState::Cancelled => "agent-status",
    }
}

/// The reviews, newest first, down the left of the tab.
#[component]
pub fn Sidebar(route: Signal<Route>) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let reviews = LocalResource::new(move || {
        updates.agents.track();
        updates.auto_reviews.track();
        api::auto_reviews()
    });
    view! {
        <aside class="agent-sidebar">
            <h2 class="agent-repo-name"><span>"Reviews of your PRs"</span></h2>
            {move || match reviews.get() {
                None => view! { <p class="muted pad">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <p class="error pad">{e}</p> }.into_any(),
                Some(Ok(list)) if list.is_empty() => view! {
                    <p class="agent-none muted">"No reviews yet."</p>
                }.into_any(),
                Some(Ok(list)) => view! {
                    <ul class="agent-list">
                        {list.into_iter().map(|r| view! { <Entry review=r route=route /> }).collect_view()}
                    </ul>
                }.into_any(),
            }}
        </aside>
    }
}

#[component]
fn Entry(review: AutoReviewEntry, route: Signal<Route>) -> impl IntoView {
    let session = review.session.clone();
    let selected = {
        let session = session.clone();
        move || route.with(|r| matches!(r, Route::AutoReview(id) if Some(id) == session.as_ref()))
    };
    // A posted review's session is gone; its review is on the PR.
    let href = match &session {
        Some(id) => hash(id),
        None => hash_for(&review.key),
    };
    let badge = badge(&review);
    let mut meta = vec![
        review.head_oid.chars().take(8).collect::<String>(),
        cost(review.cost, review.cost_unit),
    ];
    match (review.kind, review.manual) {
        (AutoRunKind::Fix, _) => meta.push(match review.round {
            Some(round) => format!("addressing comments, round {round}"),
            None => "addressing comments".into(),
        }),
        (AutoRunKind::Review, true) => meta.push("asked for".into()),
        (AutoRunKind::Review, false) => {}
    }
    let meta = meta.join(" · ");
    view! {
        <li class="agent-entry">
            // Links can't nest, so the row's link is an overlay over the
            // li; the body sits next to it so the GitHub icon can be
            // clicked.
            <a class="agent-item overlay" class:selected=selected href=href title=review.error.clone()></a>
            <div class="agent-item-body">
                <span class="agent-title" title=review.title.clone()>{review.title.clone()}</span>
                <span class="agent-status-line">
                    {badge}
                    <span class="agent-meta muted">
                        {format!("{}#{}", review.key.repo, review.key.number)}
                        <GithubLink url=github_url(&review.key) />
                        {format!(" · {meta}")}
                    </span>
                </span>
            </div>
        </li>
    }
}

/// The tab's page when no review is picked.
#[component]
pub fn Overview() -> impl IntoView {
    view! {
        <div class="auto-reviews-overview">
            <h1>"Auto-reviews"</h1>
            <p>
                "Grenadine reviews the pull requests you open in its repositories, each in an "
                "unattended agent session that posts its review on the pull request. A session "
                "whose review is on GitHub is deleted; one that failed or didn't post is kept so "
                "you can see why."
            </p>
            <p class="muted">
                "Which pull requests get reviewed, and the skill a review runs, are in each "
                "repository's settings: the gear next to its name in the Agents tab."
            </p>
            <p class="muted">
                "To review one of your pull requests now, use Review now in the Auto-review "
                "section of its page's sidebar. A pull request has at most one review queued "
                "or running at a time."
            </p>
            <p class="muted">
                "Address comments, in the same section, starts a session that addresses the "
                "open comments on the pull request and on the ones stacked on it, pushes the "
                "changes, and answers the comments. If you push to one of them meanwhile, its "
                "push is refused and yours stays. One runs per stack at a time."
            </p>
        </div>
    }
}

/// The badge of `review`: its session's status while it runs.
fn badge(review: &AutoReviewEntry) -> AnyView {
    match (review.state, review.status) {
        (AutoReviewState::Running, Some(status)) => {
            view! { <span class=status_class(status)>{status_label(status)}</span> }.into_any()
        }
        (state, _) => {
            view! { <span class=state_class(state)>{state_label(review.kind, state)}</span> }
                .into_any()
        }
    }
}

/// The section of the sidebar of one of the viewer's open PRs: how its
/// latest review and its latest fixing run went, and buttons that ask for
/// another of each. `head` is the PR's latest synced commit, `open` how
/// many of its comments a fixing run would address, and `stacked` whether
/// open PRs are stacked on it, whose comments a fixing run addresses too.
#[component]
pub fn PrSection(
    key: PrKey,
    #[prop(into)] head: Signal<Option<String>>,
    #[prop(into)] open: Signal<usize>,
    #[prop(into)] stacked: Signal<bool>,
) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let reviews = {
        let key = key.clone();
        LocalResource::new(move || {
            updates.agents.track();
            updates.auto_reviews.track();
            let key = key.clone();
            async move { api::pr_auto_reviews(&key).await }
        })
    };
    let error = RwSignal::new(None::<String>);
    // A request on its way, so a second click doesn't send another.
    let sending = RwSignal::new(false);
    let send = move |cancel: Option<i64>, kind: AutoRunKind, key: PrKey| {
        if sending.get_untracked() {
            return;
        }
        sending.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            let done = match (cancel, kind) {
                (Some(id), _) => api::cancel_auto_review(id).await,
                (None, AutoRunKind::Review) => api::ask_auto_review(&key).await,
                (None, AutoRunKind::Fix) => api::ask_auto_fix(&key).await,
            };
            sending.set(false);
            if let Err(e) = done {
                error.set(Some(e));
            }
            reviews.refetch();
        });
    };
    // The latest run of `kind` and the button that asks for another.
    let part = move |kind: AutoRunKind, latest: Option<AutoReviewEntry>, key: PrKey| {
        let fix = kind == AutoRunKind::Fix;
        let now = (js_sys::Date::now() / 1000.0) as i64;
        let ask = {
            let key = key.clone();
            move |label: &'static str| {
                // Nothing to address here or above.
                let idle = move || fix && open.get() == 0 && !stacked.get();
                let key = key.clone();
                view! {
                    <button type="button" prop:disabled=move || sending.get() || idle()
                        on:click=move |_| send(None, kind, key.clone())>{label}</button>
                }
            }
        };
        let comments = fix.then(|| {
            view! {
                <p class="muted auto-fix-open">
                    {move || match (open.get(), stacked.get()) {
                        (0, false) => "No open comments.".to_owned(),
                        (0, true) => "No open comments on this PR.".to_owned(),
                        (1, _) => "1 open comment.".to_owned(),
                        (n, _) => format!("{n} open comments."),
                    }}
                </p>
            }
        });
        let Some(r) = latest else {
            return view! {
                {(!fix).then(|| view! { <p class="muted">"Not reviewed yet."</p> })}
                {comments}
                <div class="form-buttons">
                    {ask(if fix { "Address comments" } else { "Review now" })}
                </div>
            }
            .into_any();
        };
        let short = r.head_oid.chars().take(8).collect::<String>();
        let active = matches!(r.state, AutoReviewState::Queued | AutoReviewState::Running);
        let queued = r.state == AutoReviewState::Queued;
        let mut meta = vec![short];
        if let Some(round) = r.round {
            meta.push(format!("round {round}"));
        }
        // Nothing's spent before it starts, and little early on.
        if !active || r.cost > 0.0 {
            meta.push(cost(r.cost, r.cost_unit));
        }
        meta.push(match r.finished_at {
            Some(t) if !active => ago(now, t),
            _ if queued => format!("asked {}", ago(now, r.started_at)),
            _ => format!("started {}", ago(now, r.started_at)),
        });
        // A fixing run moves the head itself.
        let moved = !fix && !active && head.get().is_some_and(|h| h != r.head_oid);
        let session = r.session.clone().filter(|_| active).map(|id| {
            view! { <a class="auto-review-session" href=hash(&id)>"Open session"</a> }
        });
        let buttons = if active {
            let id = r.id;
            view! {
                {session}
                <button type="button" prop:disabled=move || sending.get()
                    on:click=move |_| send(Some(id), kind, key.clone())>"Cancel"</button>
            }
            .into_any()
        } else {
            ask(if fix {
                "Address comments"
            } else {
                "Review again"
            })
            .into_any()
        };
        let waits = match fix {
            true => "Waits for a free slot, and for a fixing run in this stack to end.",
            false => "Waits for one of the running reviews to end.",
        };
        let superseded = r.state == AutoReviewState::Superseded;
        view! {
            <p class="auto-review-state">
                {badge(&r)}
                <span class="muted">{meta.join(" · ")}</span>
            </p>
            {r.error.clone().map(|e| view! { <p class="muted auto-review-error">{e}</p> })}
            {queued.then(|| view! { <p class="muted">{waits}</p> })}
            {moved.then(|| view! { <p class="muted">"A newer version was pushed since."</p> })}
            {superseded.then(|| view! {
                <p class="muted">"The PR was pushed to while it ran, so its changes were dropped."</p>
            })}
            {(!active).then_some(comments)}
            <div class="form-buttons">{buttons}</div>
        }
        .into_any()
    };
    let body = move || {
        let list = match reviews.get() {
            None => return view! { <p class="muted">"Loading…"</p> }.into_any(),
            Some(Err(e)) => return view! { <p class="error">{e}</p> }.into_any(),
            Some(Ok(list)) => list,
        };
        let latest = |kind| list.iter().find(|r| r.kind == kind).cloned();
        view! {
            <div class="auto-review-part">
                {part(AutoRunKind::Review, latest(AutoRunKind::Review), key.clone())}
            </div>
            <div class="auto-review-part auto-fix">
                <h3>"Comments"</h3>
                {part(AutoRunKind::Fix, latest(AutoRunKind::Fix), key.clone())}
            </div>
        }
        .into_any()
    };
    view! {
        <section class="auto-review-box">
            <h2>"Auto-review"</h2>
            {body}
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
        </section>
    }
}
