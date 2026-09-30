//! The top bar: the wordmark on the left, a theme picker and the poller's
//! sync status on the right. Hovering the status shows what is syncing or
//! what went wrong.

use std::time::Duration;

use grenadine_core::api::{SyncPhase, SyncStatus, SyncingPr};
use leptos::prelude::*;

use crate::{Updates, theme::{self, Theme}};

/// A relative timestamp like "5m ago" for the status line.
fn ago(now: i64, then: i64) -> String {
    let secs = (now - then).max(0);
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn label(status: &SyncStatus, now: i64) -> String {
    match status.phase {
        SyncPhase::Searching => "Checking GitHub…".into(),
        SyncPhase::Syncing => {
            if status.remaining == 1 {
                "Syncing 1 PR…".into()
            } else {
                format!("Syncing {} PRs…", status.remaining)
            }
        }
        SyncPhase::Idle => match (&status.last_error, status.last_finished) {
            (Some(_), Some(at)) => format!("Sync failed {}", ago(now, at)),
            (Some(_), None) => "Sync failed".into(),
            (None, Some(at)) => format!("Synced {}", ago(now, at)),
            (None, None) => "Synced".into(),
        },
    }
}

fn pr_line(pr: &SyncingPr) -> String {
    match &pr.title {
        Some(title) => format!("{}#{} {title}", pr.key.repo, pr.key.number),
        None => format!("{}#{}", pr.key.repo, pr.key.number),
    }
}

#[component]
pub fn Topbar() -> impl IntoView {
    let updates = expect_context::<Updates>();
    let theme = theme::signal();

    let now = RwSignal::new((js_sys::Date::now() / 1000.0) as i64);
    let handle =
        set_interval_with_handle(move || now.set((js_sys::Date::now() / 1000.0) as i64), Duration::from_secs(30));
    on_cleanup(move || {
        if let Ok(h) = handle {
            h.clear();
        }
    });

    view! {
        <header class="topbar">
            <span class="topbar-brand">"grenadine"</span>
            <span class="topbar-right">
                <select
                    class="theme-picker"
                    aria-label="Theme"
                    prop:value=move || theme.get().as_str()
                    on:change=move |ev| theme.set(Theme::parse(&event_target_value(&ev)))
                >
                    <option value="auto">"Auto"</option>
                    <option value="light">"Light"</option>
                    <option value="dark">"Dark"</option>
                </select>
            {move || {
                let status = updates.sync.get();
                let at = now.get();
                let error = status.phase == SyncPhase::Idle && status.last_error.is_some();
                let popover = match status.phase {
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
                };
                view! {
                    <span class="sync-status" class:error=error>
                        {label(&status, at)}
                        {popover.map(|p| view! { <div class="sync-popover">{p}</div> })}
                    </span>
                }
            }}
            </span>
        </header>
    }
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{PrKey, SyncPhase, SyncStatus, SyncingPr};

    use super::*;

    #[test]
    fn ago_ranges() {
        assert_eq!(ago(100, 100), "just now");
        assert_eq!(ago(100, 200), "just now");
        assert_eq!(ago(119, 60), "just now");
        assert_eq!(ago(120, 60), "1m ago");
        assert_eq!(ago(3659, 60), "59m ago");
        assert_eq!(ago(3660, 60), "1h ago");
        assert_eq!(ago(86400 + 60, 60), "1d ago");
        assert_eq!(ago(86400 * 5 + 60, 60), "5d ago");
    }

    fn idle(error: Option<&str>, finished: Option<i64>) -> SyncStatus {
        SyncStatus {
            phase: SyncPhase::Idle,
            remaining: 0,
            in_flight: vec![],
            last_finished: finished,
            last_error: error.map(str::to_owned),
        }
    }

    #[test]
    fn labels() {
        assert_eq!(label(&SyncStatus::default(), 0), "Checking GitHub…");
        assert_eq!(label(&idle(None, None), 0), "Synced");
        assert_eq!(label(&idle(None, Some(40)), 100), "Synced 1m ago");
        assert_eq!(label(&idle(Some("boom"), Some(100)), 100), "Sync failed just now");
        assert_eq!(label(&idle(Some("boom"), None), 100), "Sync failed");

        let mut s = idle(None, None);
        s.phase = SyncPhase::Syncing;
        s.remaining = 1;
        assert_eq!(label(&s, 0), "Syncing 1 PR…");
        s.remaining = 3;
        assert_eq!(label(&s, 0), "Syncing 3 PRs…");
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
}
