//! The top bar: the wordmark and the tabs on the left, the poller's sync
//! status and the settings' gear on the right. The Inboxes, Agents and
//! Auto-reviews tabs count what needs the user, and link to the page they
//! last showed while another tab is on; hovering the status shows what is
//! syncing or what went wrong.

use std::collections::BTreeSet;
use std::time::Duration;

use grenadine_core::api::{
    AgentSession, AgentStatus, AutoReviewEntry, AutoReviewState, PrKey, SessionKind, SyncPhase,
    SyncStatus, SyncingPr,
};
use leptos::prelude::*;

use crate::global_settings::{GlobalSettingsButton, GlobalSettingsDialog};
use crate::tabs::{self, Tab};
use crate::{Route, Updates, api, drafts};

/// `ts` (Unix seconds) as local "HH:MM".
pub(crate) fn hm(ts: i64) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts as f64 * 1000.0));
        format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
    }
    // The tests don't run under wasm; UTC is close enough there.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mins = ts.rem_euclid(86400) / 60;
        format!("{:02}:{:02}", mins / 60, mins % 60)
    }
}

/// `ts` (Unix seconds) as local "HH:MM:SS".
pub(crate) fn hms(ts: i64) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts as f64 * 1000.0));
        format!(
            "{:02}:{:02}:{:02}",
            d.get_hours(),
            d.get_minutes(),
            d.get_seconds()
        )
    }
    // The tests don't run under wasm; UTC is close enough there.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let secs = ts.rem_euclid(86400);
        format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    }
}

fn label(status: &SyncStatus, now: i64) -> String {
    match status.phase {
        SyncPhase::Idle if status.paused_until.is_some_and(|t| t > now) => {
            format!("Paused until {}", hm(status.paused_until.unwrap()))
        }
        SyncPhase::Searching => "Checking GitHub…".into(),
        SyncPhase::Syncing => {
            if status.remaining == 1 {
                "Syncing 1 PR…".into()
            } else {
                format!("Syncing {} PRs…", status.remaining)
            }
        }
        SyncPhase::Idle => match (&status.last_error, status.last_finished) {
            (Some(_), Some(at)) => format!("Sync failed {}", crate::ago(now, at)),
            (Some(_), None) => "Sync failed".into(),
            (None, Some(at)) => format!("Synced {}", crate::ago(now, at)),
            (None, None) => "Synced".into(),
        },
    }
}

/// Interactive sessions waiting on the user, including ones they already
/// looked at.
fn agents_waiting(sessions: &[AgentSession]) -> usize {
    sessions
        .iter()
        .filter(|s| {
            s.kind == SessionKind::Interactive
                && match s.status {
                    AgentStatus::Idle
                    | AgentStatus::AwaitingApproval
                    | AgentStatus::Interrupted
                    | AgentStatus::Failed => true,
                    AgentStatus::Creating | AgentStatus::Running => false,
                }
        })
        .count()
}

/// Reviews whose session the user can still look at or clean up; deleting
/// the session clears the entry.
fn reviews_waiting(reviews: &[AutoReviewEntry]) -> usize {
    reviews
        .iter()
        .filter(|r| {
            r.session.is_some()
                && match r.state {
                    AutoReviewState::Unposted | AutoReviewState::Failed => true,
                    AutoReviewState::Running
                    | AutoReviewState::Posted
                    | AutoReviewState::Cancelled => false,
                }
        })
        .count()
}

fn pr_line(pr: &SyncingPr) -> String {
    match &pr.title {
        Some(title) => format!("{}#{} {title}", pr.key.repo, pr.key.number),
        None => format!("{}#{}", pr.key.repo, pr.key.number),
    }
}

#[component]
pub fn Topbar(
    /// The page shown.
    route: Signal<Route>,
) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let settings_open = RwSignal::new(false);

    // How many distinct PRs the inboxes that count in the total list.
    let inboxes = LocalResource::new(move || {
        updates.inboxes.track();
        api::inboxes()
    });
    let total = move || {
        inboxes
            .get()
            .and_then(|r| r.ok())
            .map(|list| {
                list.iter()
                    .filter(|i| i.inbox.count_in_total)
                    .flat_map(|i| i.prs.iter().map(|p| p.key.clone()))
                    .collect::<BTreeSet<PrKey>>()
                    .len()
            })
            .unwrap_or(0)
    };

    // Sessions waiting on the user, and reviews whose session is kept.
    let sessions = crate::agents::sessions();
    let waiting = move || {
        sessions
            .get()
            .and_then(|r| r.ok())
            .map(|list| agents_waiting(&list))
            .unwrap_or(0)
    };
    let reviews = LocalResource::new(move || {
        updates.agents.track();
        updates.auto_reviews.track();
        api::auto_reviews()
    });
    let pending = move || {
        reviews
            .get()
            .and_then(|r| r.ok())
            .map(|list| reviews_waiting(&list))
            .unwrap_or(0)
    };

    let now = RwSignal::new((js_sys::Date::now() / 1000.0) as i64);
    let handle = set_interval_with_handle(
        move || now.set((js_sys::Date::now() / 1000.0) as i64),
        Duration::from_secs(30),
    );
    on_cleanup(move || {
        if let Ok(h) = handle {
            h.clear();
        }
    });

    // The page each tab last showed decides what it links to; a
    // remembered session that's gone or a draft that's started or was
    // discarded falls back to the tab's root. Until the sessions load, a
    // remembered session keeps its link.
    let memory = expect_context::<tabs::Memory>();
    let drafts = expect_context::<drafts::Drafts>();
    let exists = move |r: &Route| match r {
        Route::Draft(id) => drafts.with(id, |_| ()).is_some(),
        Route::Agent(id) | Route::AutoReview(id) => sessions
            .get()
            .and_then(|r| r.ok())
            .map(|list| list.iter().any(|s| s.id == *id))
            .unwrap_or(true),
        _ => true,
    };
    let href = move |tab| route.with(|r| tabs::href(tab, r, memory.get(tab).as_ref(), exists));
    let active = move |tab| route.with(|r| tabs::tab_of(r) == tab);

    // The tab's title carries the total too.
    Effect::new(move || {
        let title = match total() {
            0 => "grenadine".to_owned(),
            n => format!("({n}) grenadine"),
        };
        document().set_title(&title);
    });

    view! {
        <header class="topbar">
            <span class="topbar-left">
                <a class="topbar-brand" href="#/">"grenadine"</a>
                <nav class="topbar-tabs">
                    <a href=move || href(Tab::Inboxes) class:active=move || active(Tab::Inboxes)>
                        "Inboxes"
                        {move || (total() > 0).then(|| view! { <span class="count">{total()}</span> })}
                    </a>
                    <a href=move || href(Tab::Agents) class:active=move || active(Tab::Agents)>
                        "Agents"
                        {move || (waiting() > 0).then(|| view! { <span class="count">{waiting()}</span> })}
                    </a>
                    <a href=move || href(Tab::AutoReviews) class:active=move || active(Tab::AutoReviews)>
                        "Auto-reviews"
                        {move || (pending() > 0).then(|| view! { <span class="count">{pending()}</span> })}
                    </a>
                    <a href="#/debug" class:active=move || active(Tab::Debug)>"Debug"</a>
                </nav>
            </span>
            <span class="topbar-right">
            {move || {
                let status = updates.sync.get();
                let at = now.get();
                let error = status.phase == SyncPhase::Idle && status.last_error.is_some();
                let paused = status.phase == SyncPhase::Idle
                    && status.paused_until.is_some_and(|t| t > at);
                let popover = if paused {
                    Some(view! {
                        <p class="sync-paused">
                            "GitHub's rate limit is nearly used up. Background syncs pause when "
                            "less than 20% of it is left, and inbox searches when less than 10% "
                            "is left. Your own actions keep working until it runs out."
                        </p>
                        <p><a href="#/debug">"Details"</a></p>
                    }.into_any())
                } else {
                    match status.phase {
                        SyncPhase::Syncing if !status.in_flight.is_empty() => {
                            let lines = status.in_flight.iter().map(pr_line).collect::<Vec<_>>();
                            Some(view! {
                                <p class="muted">"Syncing now"</p>
                                {lines.into_iter().map(|l| view! { <p class="sync-pr">{l}</p> }).collect_view()}
                            }.into_any())
                        }
                        SyncPhase::Idle => status.last_error.clone().map(|e| {
                            view! { <p class="sync-error">{e}</p> }.into_any()
                        }),
                        _ => None,
                    }
                };
                view! {
                    <span class="sync-status" class:error=error class:warning=paused>
                        {label(&status, at)}
                        {popover.map(|p| view! { <div class="sync-popover">{p}</div> })}
                    </span>
                }
            }}
                <GlobalSettingsButton open=settings_open />
            </span>
            <GlobalSettingsDialog open=settings_open />
        </header>
    }
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{CostUnit, PrKey, SyncPhase, SyncStatus, SyncingPr};

    use super::*;

    fn idle(error: Option<&str>, finished: Option<i64>) -> SyncStatus {
        SyncStatus {
            phase: SyncPhase::Idle,
            remaining: 0,
            in_flight: vec![],
            last_finished: finished,
            last_error: error.map(str::to_owned),
            paused_until: None,
        }
    }

    #[test]
    fn labels() {
        assert_eq!(label(&SyncStatus::default(), 0), "Checking GitHub…");
        assert_eq!(label(&idle(None, None), 0), "Synced");
        assert_eq!(label(&idle(None, Some(40)), 100), "Synced 1m ago");
        assert_eq!(
            label(&idle(Some("boom"), Some(100)), 100),
            "Sync failed just now"
        );
        assert_eq!(label(&idle(Some("boom"), None), 100), "Sync failed");

        let mut s = idle(None, None);
        s.phase = SyncPhase::Syncing;
        s.remaining = 1;
        assert_eq!(label(&s, 0), "Syncing 1 PR…");
        s.remaining = 3;
        assert_eq!(label(&s, 0), "Syncing 3 PRs…");

        let mut s = idle(None, Some(40));
        s.paused_until = Some(1000);
        assert!(label(&s, 100).starts_with("Paused until "));
        // Past the reset the label is back to normal.
        assert_eq!(label(&s, 1000), "Synced 16m ago");
    }

    #[test]
    fn pr_lines() {
        let pr = SyncingPr {
            key: PrKey {
                repo: "o/n".into(),
                number: 123,
            },
            title: Some("Fix it".into()),
        };
        assert_eq!(pr_line(&pr), "o/n#123 Fix it");
        assert_eq!(pr_line(&SyncingPr { title: None, ..pr }), "o/n#123");
    }

    fn session(status: AgentStatus) -> AgentSession {
        AgentSession {
            status,
            ..AgentSession::fixture("s", "o/n")
        }
    }

    #[test]
    fn agents_waiting_counts() {
        for status in [
            AgentStatus::Idle,
            AgentStatus::AwaitingApproval,
            AgentStatus::Interrupted,
            AgentStatus::Failed,
        ] {
            assert_eq!(agents_waiting(&[session(status)]), 1, "{status:?}");
        }
        for status in [AgentStatus::Creating, AgentStatus::Running] {
            assert_eq!(agents_waiting(&[session(status)]), 0, "{status:?}");
        }
        // Auto-review sessions are counted under Auto-reviews instead.
        let mut s = session(AgentStatus::Idle);
        s.kind = SessionKind::AutoReview;
        assert_eq!(agents_waiting(&[s]), 0);
    }

    fn review(state: AutoReviewState) -> AutoReviewEntry {
        AutoReviewEntry {
            id: 1,
            key: PrKey {
                repo: "o/n".into(),
                number: 1,
            },
            title: "T".into(),
            url: "u".into(),
            head_oid: "h".into(),
            state,
            session: Some("s".into()),
            status: None,
            started_at: 0,
            finished_at: None,
            cost: 0.0,
            cost_unit: CostUnit::Usd,
            error: None,
        }
    }

    #[test]
    fn reviews_waiting_counts() {
        for state in [AutoReviewState::Unposted, AutoReviewState::Failed] {
            assert_eq!(reviews_waiting(&[review(state)]), 1, "{state:?}");
        }
        for state in [
            AutoReviewState::Running,
            AutoReviewState::Posted,
            AutoReviewState::Cancelled,
        ] {
            assert_eq!(reviews_waiting(&[review(state)]), 0, "{state:?}");
        }
        // Without its session there's nothing left to look at.
        for state in [AutoReviewState::Unposted, AutoReviewState::Failed] {
            let mut r = review(state);
            r.session = None;
            assert_eq!(reviews_waiting(&[r]), 0, "{state:?}");
        }
    }
}
