//! The Auto-reviews tab: the reviews of the viewer's own PRs that the
//! server started, each in an unattended session of its own.

use grenadine_core::api::{AutoReviewEntry, AutoReviewState};
use leptos::prelude::*;

use crate::agents::{cost, status_class, status_label};
use crate::{GithubLink, Route, Updates, api, github_url, hash_for};

/// The tab's page.
pub const LIST: &str = "#/auto-reviews";

/// The page of the review that session `id` is doing.
pub fn hash(id: &str) -> String {
    format!("{LIST}/{id}")
}

fn state_label(s: AutoReviewState) -> &'static str {
    match s {
        AutoReviewState::Running => "reviewing…",
        AutoReviewState::Posted => "posted",
        AutoReviewState::Unposted => "didn't post",
        AutoReviewState::Failed => "failed",
        AutoReviewState::Cancelled => "cancelled",
    }
}

fn state_class(s: AutoReviewState) -> &'static str {
    match s {
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
    let selected = {
        let session = session.clone();
        move || route.with(|r| matches!(r, Route::AutoReview(id) if Some(id) == session.as_ref()))
    };
    // A posted review's session is gone; its review is on the PR.
    let href = match &session {
        Some(id) => hash(id),
        None => hash_for(&review.key),
    };
    let badge = match (review.state, review.status) {
        (AutoReviewState::Running, Some(status)) => {
            view! { <span class=status_class(status)>{status_label(status)}</span> }.into_any()
        }
        (state, _) => {
            view! { <span class=state_class(state)>{state_label(state)}</span> }.into_any()
        }
    };
    let meta = [
        review.head_oid.chars().take(8).collect::<String>(),
        cost(review.cost, review.cost_unit),
    ]
    .join(" · ");
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
        </div>
    }
}
