//! The Auto-reviews tab: the reviews of the viewer's own PRs that the
//! server started, each in an unattended session of its own. Also the
//! section of a PR's sidebar that shows its latest review and asks for
//! another.

use grenadine_core::api::{AutoReviewDetail, AutoReviewEntry, AutoReviewState, PrKey};
use leptos::prelude::*;

use crate::agents::{cost, status_class, status_label};
use crate::{GithubLink, Route, Updates, ago, api, github_url, hash_for};

/// The tab's page.
pub const LIST: &str = "#/auto-reviews";

/// The page of the review that session `id` is doing.
pub fn hash(id: &str) -> String {
    format!("{LIST}/{id}")
}

/// The page of the review `id`, which shows its session while it has one
/// and what the session said once it's deleted.
pub fn page_hash(id: i64) -> String {
    format!("{LIST}/review/{id}")
}

fn state_label(s: AutoReviewState) -> &'static str {
    match s {
        AutoReviewState::Queued => "queued",
        AutoReviewState::Running => "reviewing…",
        AutoReviewState::Posted => "posted",
        AutoReviewState::Unposted => "didn't post",
        AutoReviewState::Failed => "failed",
        AutoReviewState::Cancelled => "cancelled",
    }
}

fn state_class(s: AutoReviewState) -> &'static str {
    match s {
        AutoReviewState::Queued => "agent-status waiting",
        AutoReviewState::Running => "agent-status running",
        AutoReviewState::Posted => "agent-status posted",
        AutoReviewState::Unposted => "agent-status waiting",
        AutoReviewState::Failed => "agent-status failed",
        AutoReviewState::Cancelled => "agent-status",
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
    let id = review.id;
    let selected = move || {
        route.with(|r| match r {
            Route::AutoReviewPage(page) => *page == id,
            Route::AutoReview(s) => Some(s) == session.as_ref(),
            _ => false,
        })
    };
    let href = page_hash(id);
    let badge = badge(&review);
    let mut meta = vec![
        review.head_oid.chars().take(8).collect::<String>(),
        cost(review.cost, review.cost_unit),
    ];
    if review.manual {
        meta.push("asked for".into());
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
                "whose review is on GitHub is deleted, but its transcript stays on the review's "
                "page; one that failed or didn't post is kept so you can see why."
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
        </div>
    }
}

/// What a review's page shows, so that refetches only remount it when
/// that changes.
#[derive(Clone, PartialEq)]
enum Shown {
    Loading,
    Failed(String),
    Missing,
    /// The session doing the review, by its id.
    Live(String),
    /// The review without a session, and what its deleted one said if
    /// that was kept.
    Ended(Box<AutoReviewDetail>),
}

/// The page of one review: its session while there is one, then what
/// the session said.
#[component]
pub fn ReviewPage(id: i64) -> impl IntoView {
    let updates = expect_context::<Updates>();
    // Whether the review's session is gone and the review has ended or its
    // transcript was kept, which is written once, so that there is
    // nothing left to refetch. Until then, any session's change can be
    // this review's.
    let settled = StoredValue::new(false);
    // The resource can't stop tracking itself, since an AsyncDerived
    // keeps the sources of its earlier runs; a memo drops them, so this
    // one leaves the updates behind once the review is settled.
    let changed = Memo::new(move |prev: Option<&(u64, u64)>| match prev {
        Some(&prev) if settled.get_value() => prev,
        _ => (updates.agents.get(), updates.auto_reviews.get()),
    });
    let detail = LocalResource::new(move || {
        changed.track();
        api::auto_review(id)
    });
    let shown = Memo::new(move |prev: Option<&Shown>| match detail.get() {
        None => prev.cloned().unwrap_or(Shown::Loading),
        Some(Err(e)) => Shown::Failed(e),
        Some(Ok(None)) => Shown::Missing,
        Some(Ok(Some(d))) => match d.review.session.clone() {
            Some(session) => Shown::Live(session),
            None => {
                if d.saved.is_some() || d.review.finished_at.is_some() {
                    settled.set_value(true);
                }
                Shown::Ended(Box::new(d))
            }
        },
    });
    move || match shown.get() {
        Shown::Loading => {
            view! { <main class="main"><p class="muted pad">"Loading…"</p></main> }.into_any()
        }
        Shown::Failed(e) => {
            view! { <main class="main"><p class="error pad">{e}</p></main> }.into_any()
        }
        Shown::Missing => view! {
            <main class="main"><p class="muted pad">"There is no such review."</p></main>
        }
        .into_any(),
        Shown::Live(session) => {
            view! { <crate::agents::AgentView id=session read_only=true /> }.into_any()
        }
        Shown::Ended(d) => ended(*d),
    }
}

/// The page of a review whose session is gone or hasn't started.
fn ended(d: AutoReviewDetail) -> AnyView {
    let AutoReviewDetail {
        review,
        saved,
        events,
    } = d;
    let now = (js_sys::Date::now() / 1000.0) as i64;
    let mut meta = vec![
        review.head_oid.chars().take(8).collect::<String>(),
        cost(review.cost, review.cost_unit),
    ];
    meta.push(match review.finished_at {
        Some(t) => ago(now, t),
        None if review.state == AutoReviewState::Queued => {
            format!("asked {}", ago(now, review.started_at))
        }
        None => format!("started {}", ago(now, review.started_at)),
    });
    // The session's title says it's a review, as it did while it ran.
    let title = match &saved {
        Some(s) => s.title.clone(),
        None => format!("Review #{}: {}", review.key.number, review.title),
    };
    let note = match (&saved, review.state) {
        (Some(_), _) => None,
        (None, AutoReviewState::Queued) => {
            Some("Waits to start until one of the running reviews ends.")
        }
        (None, AutoReviewState::Running) => Some("Starting its session…"),
        (None, _) => Some("This review's session was deleted, and its transcript wasn't kept."),
    };
    view! {
        <div class="agent-page">
            <div class="agent-panes">
                <main class="main">
                    <div class="agent">
                        <header class="agent-header">
                            <h1>{title}</h1>
                            <p class="muted">
                                {badge(&review)}
                                " "
                                <a href=hash_for(&review.key)>{format!("{}#{}", review.key.repo, review.key.number)}</a>
                                <GithubLink url=github_url(&review.key) />
                                {format!(" · {}", meta.join(" · "))}
                            </p>
                            {review.error.clone().map(|e| view! { <p class="muted auto-review-error">{e}</p> })}
                            {note.map(|n| view! { <p class="muted auto-review-note">{n}</p> })}
                        </header>
                        {saved.map(|s| view! { <crate::agents::SavedTranscript session=s events=events /> })}
                    </div>
                </main>
            </div>
        </div>
    }
    .into_any()
}

/// The badge of `review`: its session's status while it runs.
fn badge(review: &AutoReviewEntry) -> AnyView {
    match (review.state, review.status) {
        (AutoReviewState::Running, Some(status)) => {
            view! { <span class=status_class(status)>{status_label(status)}</span> }.into_any()
        }
        (state, _) => {
            view! { <span class=state_class(state)>{state_label(state)}</span> }.into_any()
        }
    }
}

/// The section of the sidebar of one of the viewer's open PRs: how its
/// latest review went, and a button that asks for another. `head` is the
/// PR's latest synced commit.
#[component]
pub fn PrSection(key: PrKey, #[prop(into)] head: Signal<Option<String>>) -> impl IntoView {
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
    let send = move |cancel: Option<i64>, key: PrKey| {
        if sending.get_untracked() {
            return;
        }
        sending.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            let done = match cancel {
                Some(id) => api::cancel_auto_review(id).await,
                None => api::ask_auto_review(&key).await,
            };
            sending.set(false);
            if let Err(e) = done {
                error.set(Some(e));
            }
            reviews.refetch();
        });
    };
    let body = move || {
        let latest = match reviews.get() {
            None => return view! { <p class="muted">"Loading…"</p> }.into_any(),
            Some(Err(e)) => return view! { <p class="error">{e}</p> }.into_any(),
            Some(Ok(list)) => list.into_iter().next(),
        };
        let now = (js_sys::Date::now() / 1000.0) as i64;
        let key = key.clone();
        let Some(r) = latest else {
            return view! {
                <p class="muted">"Not reviewed yet."</p>
                <div class="form-buttons">
                    <button type="button" prop:disabled=move || sending.get()
                        on:click=move |_| send(None, key.clone())>"Review now"</button>
                </div>
            }
            .into_any();
        };
        let short = r.head_oid.chars().take(8).collect::<String>();
        let active = matches!(r.state, AutoReviewState::Queued | AutoReviewState::Running);
        let queued = r.state == AutoReviewState::Queued;
        let mut meta = vec![short];
        // Nothing's spent before it starts, and little early on.
        if !active || r.cost > 0.0 {
            meta.push(cost(r.cost, r.cost_unit));
        }
        meta.push(match r.finished_at {
            Some(t) if !active => ago(now, t),
            _ if queued => format!("asked {}", ago(now, r.started_at)),
            _ => format!("started {}", ago(now, r.started_at)),
        });
        let moved = !active && head.get().is_some_and(|h| h != r.head_oid);
        let session = (active && r.session.is_some()).then(|| {
            view! { <a class="auto-review-session" href=page_hash(r.id)>"Open session"</a> }
        });
        let buttons = if active {
            let id = r.id;
            view! {
                {session}
                <button type="button" prop:disabled=move || sending.get()
                    on:click=move |_| send(Some(id), key.clone())>"Cancel"</button>
            }
            .into_any()
        } else {
            view! {
                <button type="button" prop:disabled=move || sending.get()
                    on:click=move |_| send(None, key.clone())>"Review again"</button>
            }
            .into_any()
        };
        view! {
            <p class="auto-review-state">
                {badge(&r)}
                <span class="muted">{meta.join(" · ")}</span>
            </p>
            {r.error.clone().map(|e| view! { <p class="muted auto-review-error">{e}</p> })}
            {queued.then(|| view! {
                <p class="muted">"Waits for one of the running reviews to end."</p>
            })}
            {moved.then(|| view! { <p class="muted">"A newer version was pushed since."</p> })}
            <div class="form-buttons">{buttons}</div>
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
