//! The grenadine web UI: a landing page with the inboxes of PRs, a page for
//! one PR with its version picker and diff, and the Claude Code sessions
//! started from the UI.

mod agents;
mod api;
mod diffview;
mod events;
mod highlight;
mod inboxes;
mod markdown;
mod pr;
mod theme;
mod topbar;

use grenadine_core::api::{PrKey, ServerEvent, SyncStatus};
use leptos::prelude::*;
use wasm_bindgen::prelude::*;

/// Bumped when the server says something changed, so views refetch.
#[derive(Clone, Copy)]
pub struct Updates {
    pub inboxes: RwSignal<u64>,
    /// The PR that changed last, with a counter so that the same PR changing
    /// twice still notifies.
    pub pr: RwSignal<(Option<PrKey>, u64)>,
    /// The poller's status, shown in the top bar.
    pub sync: RwSignal<SyncStatus>,
    /// Bumped when an agent session was created, deleted or changed status.
    pub agents: RwSignal<u64>,
}

/// What the URL's hash shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Inboxes,
    /// `#/owner/name/number`.
    Pr(PrKey),
    /// `#/agents`, `#/agents/new/owner/name` to start a session in a
    /// repository, or `#/agents/new/owner/name/number` to start one on a PR.
    Agents {
        repo: Option<String>,
        pr: Option<u64>,
    },
    /// `#/agents/<id>`.
    Agent(String),
}

fn pr_key(owner: &str, name: &str, number: &str) -> Option<PrKey> {
    Some(PrKey {
        repo: format!("{owner}/{name}"),
        number: number.parse().ok()?,
    })
}

fn route_from_hash(hash: &str) -> Route {
    let path = hash.trim_start_matches('#').trim_start_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    let route = match parts[..] {
        ["agents"] => Some(Route::Agents {
            repo: None,
            pr: None,
        }),
        ["agents", "new", owner, name] if !owner.is_empty() && !name.is_empty() => {
            Some(Route::Agents {
                repo: Some(format!("{owner}/{name}")),
                pr: None,
            })
        }
        ["agents", "new", owner, name, number] => {
            pr_key(owner, name, number).map(|k| Route::Agents {
                repo: Some(k.repo),
                pr: Some(k.number),
            })
        }
        ["agents", id] if !id.is_empty() => Some(Route::Agent(id.to_owned())),
        [owner, name, number] => pr_key(owner, name, number).map(Route::Pr),
        _ => None,
    };
    route.unwrap_or(Route::Inboxes)
}

pub fn hash_for(key: &PrKey) -> String {
    format!("#/{}/{}", key.repo, key.number)
}

fn current_hash() -> String {
    window().location().hash().unwrap_or_default()
}

/// Fetches the sync status; the server's lag fallback sends InboxesChanged,
/// so this also covers a dropped SyncStatus event.
fn refetch_sync(updates: Updates) {
    leptos::task::spawn_local(async move {
        if let Ok(s) = api::sync_status().await {
            updates.sync.set(s);
        }
    });
}

/// Subscribes to the server's events for the life of the page.
fn listen(updates: Updates) {
    let on_message = move |data: String| match serde_json::from_str::<ServerEvent>(&data) {
        Ok(ServerEvent::InboxesChanged) => {
            updates.inboxes.update(|n| *n += 1);
            updates.agents.update(|n| *n += 1);
            refetch_sync(updates);
        }
        Ok(ServerEvent::PrChanged(key)) => updates.pr.update(|(k, n)| {
            *k = Some(key);
            *n += 1;
        }),
        Ok(ServerEvent::SyncStatus(s)) => updates.sync.set(s),
        Ok(ServerEvent::AgentsChanged) => updates.agents.update(|n| *n += 1),
        Err(_) => {}
    };
    // After a reconnect (e.g. the server restarted or the machine slept)
    // refetch everything.
    let on_open = move || {
        updates.inboxes.update(|n| *n += 1);
        updates.agents.update(|n| *n += 1);
        refetch_sync(updates);
    };
    events::subscribe("/api/events".into(), on_message, on_open);
}

fn pr_of(route: &Route) -> Option<PrKey> {
    match route {
        Route::Pr(key) => Some(key.clone()),
        _ => None,
    }
}

#[component]
fn App() -> impl IntoView {
    let updates = Updates {
        inboxes: RwSignal::new(0),
        pr: RwSignal::new((None, 0)),
        sync: RwSignal::new(SyncStatus::default()),
        agents: RwSignal::new(0),
    };
    provide_context(updates);
    listen(updates);

    let route = RwSignal::new(route_from_hash(&current_hash()));
    // The PR shown most recently, highlighted when going back to the inboxes.
    let last = RwSignal::new(route.with_untracked(pr_of));
    let handle = window_event_listener_untyped("hashchange", move |_| {
        let r = route_from_hash(&current_hash());
        if let Some(key) = pr_of(&r) {
            last.set(Some(key));
        }
        route.set(r);
    });
    on_cleanup(move || handle.remove());
    let on_inboxes = Memo::new(move |_| route.with(|r| *r == Route::Inboxes));
    let selected = Memo::new(move |_| route.with(pr_of));
    let agents = Memo::new(move |_| {
        route.with(|r| match r {
            Route::Agents { .. } | Route::Agent(_) => Some(r.clone()),
            _ => None,
        })
    });
    // The sidebar stays mounted while going between the agents' pages.
    let on_agents = Memo::new(move |_| agents.with(Option::is_some));

    // The inboxes stay mounted while another page is shown so that going
    // back is instant. Hiding them loses their scroll position, so
    // remember it.
    let inboxes_ref = NodeRef::<leptos::html::Main>::new();
    let scroll = StoredValue::new(0);
    Effect::new(move |_| {
        if on_inboxes.get()
            && let Some(main) = inboxes_ref.get_untracked()
        {
            main.set_scroll_top(scroll.get_value());
        }
    });

    view! {
        <div class="page">
            <topbar::Topbar route=route.into() />
            <main
                class="main"
                class:hidden=move || !on_inboxes.get()
                node_ref=inboxes_ref
                on:scroll=move |_| {
                    if let Some(main) = inboxes_ref.get_untracked() {
                        scroll.set_value(main.scroll_top());
                    }
                }
            >
                <inboxes::Inboxes last=last />
            </main>
            {move || selected.get().map(|key| view! {
                <main class="main"><pr::PrView key=key /></main>
            })}
            {move || on_agents.get().then(|| view! {
                <div class="agents-page">
                    <agents::Sidebar route=route.into() />
                    {move || agents.get().map(|r| view! {
                        <main class="main">
                            {match r {
                                Route::Agent(id) => view! { <agents::AgentView id=id /> }.into_any(),
                                Route::Agents { repo, pr } => {
                                    view! { <agents::NewSession repo=repo pr=pr /> }.into_any()
                                }
                                _ => ().into_any(),
                            }}
                        </main>
                    })}
                </div>
            })}
        </div>
    }
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    // The syntax colors for both themes.
    let document = document();
    if let Ok(style) = document.create_element("style") {
        style.set_text_content(Some(&highlight::stylesheet()));
        if let Some(head) = document.head() {
            let _ = head.append_child(&style);
        }
    }
    leptos::mount::mount_to_body(App);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(repo: &str, number: u64) -> PrKey {
        PrKey {
            repo: repo.into(),
            number,
        }
    }

    #[test]
    fn routes() {
        assert_eq!(route_from_hash(""), Route::Inboxes);
        assert_eq!(route_from_hash("#/"), Route::Inboxes);
        assert_eq!(route_from_hash("#/o/n/7"), Route::Pr(key("o/n", 7)));
        assert_eq!(route_from_hash("#/o/n/x"), Route::Inboxes);
        assert_eq!(
            route_from_hash("#/agents"),
            Route::Agents {
                repo: None,
                pr: None
            }
        );
        assert_eq!(
            route_from_hash("#/agents/new/o/n"),
            Route::Agents {
                repo: Some("o/n".into()),
                pr: None
            }
        );
        assert_eq!(
            route_from_hash("#/agents/new/o/n/7"),
            Route::Agents {
                repo: Some("o/n".into()),
                pr: Some(7)
            }
        );
        assert_eq!(route_from_hash("#/agents/abc"), Route::Agent("abc".into()));
        assert_eq!(route_from_hash("#/agents/"), Route::Inboxes);
        // An owner called "agents" still has PRs.
        assert_eq!(
            route_from_hash("#/agents/n/7"),
            Route::Pr(key("agents/n", 7))
        );
    }
}
