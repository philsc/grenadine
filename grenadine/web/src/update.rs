//! The server's update to a newer version: its section in the Debug tab,
//! and the bar below the top bar that follows it on every page.

use grenadine_core::api::{BusySession, CommitInfo, UpdatePhase, UpdateStatus};
use leptos::prelude::*;

use crate::{Updates, ago, api};

/// What the server last said about its update.
#[derive(Clone, Copy)]
pub struct Status {
    status: RwSignal<Option<UpdateStatus>>,
    /// The commit the server ran when the page loaded.
    loaded: RwSignal<Option<String>>,
}

impl Status {
    /// Follows the server's update for the life of the page.
    pub fn follow(updates: Updates) -> Status {
        let status = RwSignal::new(None);
        let loaded = RwSignal::new(None);
        // Answers can overtake each other; only the last request's counts.
        let requests = StoredValue::new(0u64);
        Effect::new(move |_| {
            updates.update.track();
            requests.update_value(|n| *n += 1);
            let request = requests.get_value();
            leptos::task::spawn_local(async move {
                let Ok(s) = api::update_status().await else {
                    return;
                };
                if requests.get_value() != request {
                    return;
                }
                if loaded.with_untracked(Option::is_none) {
                    loaded.set(sha(&s));
                }
                status.set(Some(s));
            });
        });
        Status { status, loaded }
    }
}

fn sha(s: &UpdateStatus) -> Option<String> {
    s.running.as_ref().map(|c| c.sha.clone())
}

/// Whether the server runs another commit than when the page loaded.
fn outdated(loaded: Option<&str>, status: &UpdateStatus) -> bool {
    match (loaded, &status.running) {
        (Some(loaded), Some(running)) => loaded != running.sha,
        _ => false,
    }
}

fn busy(phase: &UpdatePhase) -> bool {
    !matches!(phase, UpdatePhase::Idle | UpdatePhase::Failed { .. })
}

fn count(n: usize, what: &str) -> String {
    match n {
        1 => format!("1 {what}"),
        n => format!("{n} {what}s"),
    }
}

fn waiting_label(sessions: usize, terminals: usize) -> String {
    let mut text = format!("Update ready, waiting for {}", count(sessions, "session"));
    if terminals > 0 {
        text.push_str(&format!(
            "; the restart closes {}",
            count(terminals, "terminal")
        ));
    }
    text
}

fn confirm_text(sessions: &[BusySession], terminals: usize) -> String {
    let mut text = "Restart now? This stops these sessions:\n".to_owned();
    for s in sessions {
        let title = if s.title.is_empty() { &s.id } else { &s.title };
        text.push_str(&format!("\n• {title}"));
        if s.terminals > 0 {
            let running = count(s.terminals, "terminal");
            text.push_str(&format!(" (a command runs in {running})"));
        }
    }
    text.push_str("\n\nPrompts queued in them are lost.");
    if terminals > 0 {
        let closed = count(terminals, "open terminal");
        text.push_str(&format!(" {closed} will close."));
    }
    text
}

fn short(c: &CommitInfo) -> String {
    c.sha.chars().take(8).collect()
}

/// The update's progress on every page, and the offer to reload after it.
#[component]
pub fn UpdateBar() -> impl IntoView {
    let Status { status, loaded } = expect_context();
    let error = RwSignal::new(None::<String>);
    let cancel = move |_| {
        leptos::task::spawn_local(async move {
            error.set(api::cancel_update().await.err());
        });
    };
    let phase_view = move || {
        let phase = status.with(|s| s.as_ref().map(|s| s.phase.clone()))?;
        Some(match phase {
            UpdatePhase::Idle => return None,
            UpdatePhase::CheckingOut => view! { <span>"Updating…"</span> }.into_any(),
            UpdatePhase::Building => view! {
                <span>"Building the update…"</span>
                <button class="link" on:click=cancel>"Cancel"</button>
            }
            .into_any(),
            UpdatePhase::Restarting => view! { <span>"Restarting…"</span> }.into_any(),
            UpdatePhase::Waiting {
                sessions,
                terminals,
                ..
            } => {
                let label = waiting_label(sessions.len(), terminals);
                let force = move |_| {
                    if !window()
                        .confirm_with_message(&confirm_text(&sessions, terminals))
                        .unwrap_or(false)
                    {
                        return;
                    }
                    leptos::task::spawn_local(async move {
                        error.set(api::force_update().await.err());
                    });
                };
                view! {
                    <span>{label}</span>
                    <button class="link" on:click=force>"Restart now"</button>
                    <button class="link" on:click=cancel>"Cancel"</button>
                }
                .into_any()
            }
            UpdatePhase::Failed { error } => view! {
                <span>"The update failed"</span>
                <button class="link" on:click=cancel>"Dismiss"</button>
                <details><summary>"Details"</summary><pre>{error}</pre></details>
            }
            .into_any(),
        })
    };
    let reload = move || {
        let loaded = loaded.get();
        status
            .with(|s| s.as_ref().is_some_and(|s| outdated(loaded.as_deref(), s)))
            .then(|| {
                view! {
                    <span>"Grenadine was updated."</span>
                    <button
                        class="link"
                        on:click=move |_| { let _ = window().location().reload(); }
                    >
                        "Reload"
                    </button>
                }
            })
    };
    move || {
        let phase = phase_view();
        let reload = reload();
        let error = error.get();
        // An error is about a button that's gone once the phase is idle.
        (phase.is_some() || reload.is_some()).then(|| {
            view! {
                <div class="update-bar" role="status">
                    {phase}
                    {error.map(|e| view! { <span class="error">{e}</span> })}
                    {reload}
                </div>
            }
        })
    }
}

/// The running version and the button to update it; nothing when the
/// server can't update itself.
#[component]
pub fn ServerSection() -> impl IntoView {
    let Status { status, .. } = expect_context();
    // A memo, so that an update's progress doesn't check again.
    let running = Memo::new(move |_| status.with(|s| s.as_ref()?.running.clone()));
    move || running.get().map(|running| view! { <Server running /> })
}

#[component]
fn Server(running: CommitInfo) -> impl IntoView {
    let Status { status, .. } = expect_context();
    let check = LocalResource::new(api::check_update);
    let error = RwSignal::new(None::<String>);
    let sending = RwSignal::new(false);
    let disabled =
        move || sending.get() || status.with(|s| s.as_ref().is_some_and(|s| busy(&s.phase)));
    let start = move |_| {
        sending.set(true);
        leptos::task::spawn_local(async move {
            error.set(api::start_update().await.err());
            sending.set(false);
        });
    };
    let label = move || match check.get() {
        Some(Ok(c)) if c.behind.is_empty() => "Rebuild and restart",
        _ => "Update and restart",
    };
    let now = (js_sys::Date::now() / 1000.0) as i64;
    view! {
        <section class="update-server">
            <h1>"Server"</h1>
            <p>
                {format!("Running {} ", short(&running))}
                <span class="muted">
                    {format!("{} · committed {}", running.title, ago(now, running.date))}
                </span>
            </p>
            {move || match check.get() {
                None => view! { <p class="muted">"Checking for updates…"</p> }.into_any(),
                Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                Some(Ok(c)) if c.behind.is_empty() => {
                    view! { <p>"Up to date with origin/main."</p> }.into_any()
                }
                Some(Ok(c)) => view! {
                    <p>{match c.behind.len() {
                        1 => "1 commit behind origin/main:".to_owned(),
                        n => format!("{n} commits behind origin/main:"),
                    }}</p>
                    <ul>
                        {c.behind.into_iter().map(|c| view! { <li>{c.title}</li> }).collect_view()}
                    </ul>
                }.into_any(),
            }}
            <button on:click=start disabled=disabled>{label}</button>
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(sha: Option<&str>) -> UpdateStatus {
        UpdateStatus {
            running: sha.map(|sha| CommitInfo {
                sha: sha.into(),
                title: "t".into(),
                date: 0,
            }),
            phase: UpdatePhase::Idle,
        }
    }

    #[test]
    fn offers_a_reload_for_another_commit() {
        assert!(outdated(Some("a"), &status(Some("b"))));
        assert!(!outdated(Some("a"), &status(Some("a"))));
        assert!(!outdated(None, &status(Some("a"))));
        assert!(!outdated(Some("a"), &status(None)));
    }

    #[test]
    fn confirmation_names_the_sessions() {
        let session = |id: &str, title: &str, terminals| BusySession {
            id: id.into(),
            title: title.into(),
            terminals,
        };
        let busy = [session("1", "Fix it", 0), session("2", "", 2)];
        let text = confirm_text(&busy, 3);
        assert!(
            text.contains("• Fix it\n• 2 (a command runs in 2 terminals)\n"),
            "{text}"
        );
        assert!(text.ends_with(" 3 open terminals will close."), "{text}");
        assert!(!confirm_text(&busy, 0).contains("open terminal"));
        assert_eq!(waiting_label(1, 0), "Update ready, waiting for 1 session");
        assert_eq!(
            waiting_label(2, 1),
            "Update ready, waiting for 2 sessions; the restart closes 1 terminal"
        );
    }
}
