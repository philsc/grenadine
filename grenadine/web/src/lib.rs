//! The grenadine web UI: a landing page with the inboxes of PRs, and a page
//! for one PR with its version picker and diff.

mod api;
mod diffview;
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
}

/// Parses `#/owner/name/number`.
fn key_from_hash(hash: &str) -> Option<PrKey> {
    let mut parts = hash
        .trim_start_matches('#')
        .trim_start_matches('/')
        .split('/');
    let (owner, name, number) = (parts.next()?, parts.next()?, parts.next()?);
    Some(PrKey {
        repo: format!("{owner}/{name}"),
        number: number.parse().ok()?,
    })
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
    let Ok(source) = web_sys::EventSource::new("/api/events") else {
        return;
    };
    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            let Some(data) = e.data().as_string() else {
                return;
            };
            match serde_json::from_str::<ServerEvent>(&data) {
                Ok(ServerEvent::InboxesChanged) => {
                    updates.inboxes.update(|n| *n += 1);
                    refetch_sync(updates);
                }
                Ok(ServerEvent::PrChanged(key)) => updates.pr.update(|(k, n)| {
                    *k = Some(key);
                    *n += 1;
                }),
                Ok(ServerEvent::SyncStatus(s)) => updates.sync.set(s),
                Err(_) => {}
            }
        });
    source.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
    // After a reconnect (e.g. the server restarted) refetch everything.
    let on_open = Closure::<dyn FnMut()>::new(move || {
        updates.inboxes.update(|n| *n += 1);
        refetch_sync(updates);
    });
    source.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    on_open.forget();
    std::mem::forget(source);
}

#[component]
fn App() -> impl IntoView {
    let updates = Updates {
        inboxes: RwSignal::new(0),
        pr: RwSignal::new((None, 0)),
        sync: RwSignal::new(SyncStatus::default()),
    };
    provide_context(updates);
    listen(updates);

    // The PR in the URL, if any. Without one, the inboxes are shown.
    let selected = RwSignal::new(key_from_hash(&current_hash()));
    // The PR shown most recently, highlighted when going back to the inboxes.
    let last = RwSignal::new(selected.get_untracked());
    let handle = window_event_listener_untyped("hashchange", move |_| {
        let key = key_from_hash(&current_hash());
        if key.is_some() {
            last.set(key.clone());
        }
        selected.set(key);
    });
    on_cleanup(move || handle.remove());
    let on_pr = Signal::derive(move || selected.with(Option::is_some));

    // The inboxes stay mounted while a PR is shown so that going back is
    // instant. Hiding them loses their scroll position, so remember it.
    let inboxes_ref = NodeRef::<leptos::html::Main>::new();
    let scroll = StoredValue::new(0);
    Effect::new(move |_| {
        if !on_pr.get()
            && let Some(main) = inboxes_ref.get_untracked()
        {
            main.set_scroll_top(scroll.get_value());
        }
    });

    view! {
        <div class="page">
            <topbar::Topbar on_pr=on_pr />
            <main
                class="main"
                class:hidden=on_pr
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
