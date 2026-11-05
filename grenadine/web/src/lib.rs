//! The grenadine web UI: a landing page with the inboxes of PRs, a page for
//! one PR with its version picker and diff, and the Claude Code sessions
//! started from the UI.

mod agents;
mod api;
mod auto_reviews;
mod composer;
mod debug;
mod diffview;
mod drafts;
mod events;
mod global_settings;
mod highlight;
mod image_drafts;
mod images;
mod inboxes;
mod markdown;
mod pr;
mod review;
mod settings;
mod terminal;
mod theme;
mod topbar;

use grenadine_core::api::{GENERAL, PrKey, ServerEvent, SyncStatus};
use leptos::prelude::*;
use wasm_bindgen::prelude::*;

/// Bumped when the server says something changed, so views refetch.
#[derive(Clone, Copy)]
pub struct Updates {
    pub inboxes: RwSignal<u64>,
    /// The PR that changed last, with a counter so that the same PR changing
    /// twice still notifies. `None` after a reconnect, when any PR may have.
    pub pr: RwSignal<(Option<PrKey>, u64)>,
    /// The poller's status, shown in the top bar.
    pub sync: RwSignal<SyncStatus>,
    /// Bumped when an agent session was created, deleted or changed status.
    pub agents: RwSignal<u64>,
    /// Bumped when the cleanup items or the scans for them changed.
    pub cleanup: RwSignal<u64>,
    /// Bumped when a session's terminal opened or closed.
    pub terminals: RwSignal<u64>,
    /// Bumped when a repository's settings changed.
    pub repo_settings: RwSignal<u64>,
    /// Bumped when a review of the viewer's PRs started or ended.
    pub auto_reviews: RwSignal<u64>,
}

/// What the URL's hash shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Inboxes,
    /// `#/owner/name/number`.
    Pr(PrKey),
    /// `#/agents`, `#/agents/new/owner/name` to start a session in a
    /// repository, `#/agents/new/general` to start one in none, or
    /// `#/agents/new/owner/name/number` to start one on a PR.
    /// Each opens a new draft.
    Agents {
        repo: Option<String>,
        pr: Option<u64>,
    },
    /// `#/agents/draft/<id>`: a new session's form.
    Draft(String),
    /// `#/agents/<id>`.
    Agent(String),
    /// `#/agents/cleanup`: what deleted sessions left behind.
    Cleanup,
    /// `#/auto-reviews`: the reviews of the viewer's PRs.
    AutoReviews,
    /// `#/auto-reviews/<id>`: the session doing one of them.
    AutoReview(String),
    /// `#/debug`: what the server spends on GitHub.
    Debug,
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
        ["agents", "new", GENERAL] => Some(Route::Agents {
            repo: Some(GENERAL.to_owned()),
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
        ["agents", "draft", id] if !id.is_empty() => Some(Route::Draft(id.to_owned())),
        ["agents", "cleanup"] => Some(Route::Cleanup),
        ["agents", id] if !id.is_empty() => Some(Route::Agent(id.to_owned())),
        ["auto-reviews"] => Some(Route::AutoReviews),
        ["auto-reviews", id] if !id.is_empty() => Some(Route::AutoReview(id.to_owned())),
        ["debug"] => Some(Route::Debug),
        [owner, name, number] => pr_key(owner, name, number).map(Route::Pr),
        _ => None,
    };
    route.unwrap_or(Route::Inboxes)
}

pub fn hash_for(key: &PrKey) -> String {
    format!("#/{}/{}", key.repo, key.number)
}

/// The PR's page on GitHub.
pub fn github_url(key: &PrKey) -> String {
    format!("https://github.com/{}/pull/{}", key.repo, key.number)
}

/// The icon after a PR's in-app link that opens the same page on GitHub.
/// It's empty: the `.gh-link` CSS draws the icon.
#[component]
pub fn GithubLink(url: String) -> impl IntoView {
    view! {
        <a
            class="gh-link"
            href=url
            target="_blank"
            rel="noopener noreferrer"
            title="Open on GitHub"
            aria-label="Open on GitHub"
        ></a>
    }
}

pub(crate) fn current_hash() -> String {
    window().location().hash().unwrap_or_default()
}

/// The form's links open a new draft each, in place of the link in the
/// history, so that going back skips them.
fn open_draft(route: Route, drafts: drafts::Drafts) -> Route {
    let Route::Agents { repo, pr } = route else {
        return route;
    };
    let id = drafts.create(repo, pr);
    let _ = window().location().replace(&drafts::hash(&id));
    Route::Draft(id)
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

/// Subscribes to the server's events for the life of the page. `url`
/// carries the watched PR, so that the server syncs the PR the page
/// shows.
fn listen(updates: Updates, url: Signal<String>) {
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
        Ok(ServerEvent::CleanupChanged) => updates.cleanup.update(|n| *n += 1),
        Ok(ServerEvent::TerminalsChanged(_)) => updates.terminals.update(|n| *n += 1),
        Ok(ServerEvent::RepoSettingsChanged(_)) => updates.repo_settings.update(|n| *n += 1),
        Ok(ServerEvent::AutoReviewsChanged) => updates.auto_reviews.update(|n| *n += 1),
        Err(_) => {}
    };
    // After a reconnect (e.g. the server restarted or the machine slept)
    // refetch everything.
    let on_open = move || {
        updates.inboxes.update(|n| *n += 1);
        updates.pr.update(|(k, n)| {
            *k = None;
            *n += 1;
        });
        updates.agents.update(|n| *n += 1);
        updates.cleanup.update(|n| *n += 1);
        updates.terminals.update(|n| *n += 1);
        updates.repo_settings.update(|n| *n += 1);
        updates.auto_reviews.update(|n| *n += 1);
        refetch_sync(updates);
    };
    events::subscribe(url, on_message, on_open);
}

/// The events URL for the route: a PR page watches its PR, every other
/// page watches nothing.
fn events_url(route: &Route) -> String {
    match route {
        Route::Pr(key) => format!(
            "/api/events?watch={}",
            js_sys::encode_uri_component(&format!("pr/{}/{}", key.repo, key.number))
        ),
        _ => "/api/events".into(),
    }
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
        cleanup: RwSignal::new(0),
        terminals: RwSignal::new(0),
        repo_settings: RwSignal::new(0),
        auto_reviews: RwSignal::new(0),
    };
    provide_context(updates);

    let initial = route_from_hash(&current_hash());
    let open = match &initial {
        Route::Draft(id) => Some(id.clone()),
        _ => None,
    };
    let drafts = drafts::Drafts::load(open.as_deref());
    // Drop image drafts whose text draft is gone; the `agent.*` ones stay
    // like their text drafts do.
    image_drafts::sweep(move |key| {
        key.starts_with("agent.")
            || drafts
                .listed(open.as_deref())
                .iter()
                .any(|(_, id)| key == image_drafts::new_session_key(id))
    });
    provide_context(drafts);
    provide_context(global_settings::provide());
    provide_context(LocalResource::new(api::agent_backends));
    provide_context(LocalResource::new(move || {
        updates.agents.track();
        api::agents()
    }));
    let route = RwSignal::new(open_draft(initial, drafts));
    // A Memo so the connection only reopens when the URL actually
    // changes, e.g. between two PR pages.
    let url = Memo::new(move |_| route.with(events_url));
    listen(updates, url.into());
    // The PR shown most recently, highlighted when going back to the inboxes.
    let last = RwSignal::new(route.with_untracked(pr_of));
    let handle = window_event_listener_untyped("hashchange", move |_| {
        let r = open_draft(route_from_hash(&current_hash()), drafts);
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
            Route::Draft(_) | Route::Agent(_) | Route::Cleanup => Some(r.clone()),
            _ => None,
        })
    });
    // The sidebar stays mounted while going between the agents' pages.
    let on_agents = Memo::new(move |_| agents.with(Option::is_some));
    let reviews = Memo::new(move |_| {
        route.with(|r| match r {
            Route::AutoReviews | Route::AutoReview(_) => Some(r.clone()),
            _ => None,
        })
    });
    let on_reviews = Memo::new(move |_| reviews.with(Option::is_some));
    let on_debug = Memo::new(move |_| route.with(|r| *r == Route::Debug));

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
                <div class="pr-page"><pr::PrView key=key /></div>
            })}
            {move || on_agents.get().then(|| view! {
                <div class="agents-page">
                    <agents::Sidebar route=route.into() />
                    {move || agents.get().map(|r| match r {
                        // It has its own main, next to the worktree's changes.
                        Route::Agent(id) => view! { <agents::AgentView id=id /> }.into_any(),
                        Route::Draft(id) => view! {
                            <main class="main"><agents::NewSession draft=id /></main>
                        }.into_any(),
                        Route::Cleanup => view! { <main class="main"><agents::CleanupView /></main> }.into_any(),
                        _ => ().into_any(),
                    })}
                </div>
            })}
            {move || on_reviews.get().then(|| view! {
                <div class="agents-page">
                    <auto_reviews::Sidebar route=route.into() />
                    {move || reviews.get().map(|r| match r {
                        Route::AutoReview(id) => view! { <agents::AgentView id=id read_only=true /> }.into_any(),
                        _ => view! { <main class="main"><auto_reviews::Overview /></main> }.into_any(),
                    })}
                </div>
            })}
            {move || on_debug.get().then(|| view! {
                <main class="main"><debug::Debug /></main>
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
    highlight::warm();
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
            route_from_hash("#/agents/new/general"),
            Route::Agents {
                repo: Some(GENERAL.into()),
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
        assert_eq!(
            route_from_hash("#/agents/draft/abc"),
            Route::Draft("abc".into())
        );
        assert_eq!(route_from_hash("#/agents/abc"), Route::Agent("abc".into()));
        assert_eq!(route_from_hash("#/agents/cleanup"), Route::Cleanup);
        assert_eq!(route_from_hash("#/agents/"), Route::Inboxes);
        assert_eq!(route_from_hash("#/auto-reviews"), Route::AutoReviews);
        assert_eq!(
            route_from_hash("#/auto-reviews/abc"),
            Route::AutoReview("abc".into())
        );
        assert_eq!(route_from_hash("#/auto-reviews/"), Route::Inboxes);
        assert_eq!(route_from_hash("#/debug"), Route::Debug);
        // An owner called "agents" still has PRs.
        assert_eq!(
            route_from_hash("#/agents/n/7"),
            Route::Pr(key("agents/n", 7))
        );
    }

    #[test]
    fn github_urls() {
        assert_eq!(github_url(&key("o/n", 7)), "https://github.com/o/n/pull/7");
    }
}
