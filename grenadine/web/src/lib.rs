//! The grenadine web UI: inboxes of PRs on the left, the selected PR with
//! its version picker and diff on the right.

mod api;
mod diffview;
mod highlight;
mod markdown;
mod pr;
mod sidebar;

use grenadine_core::api::{PrKey, ServerEvent};
use leptos::prelude::*;
use wasm_bindgen::prelude::*;

/// Bumped when the server says something changed, so views refetch.
#[derive(Clone, Copy)]
pub struct Updates {
    pub inboxes: RwSignal<u64>,
    /// The PR that changed last, with a counter so that the same PR changing
    /// twice still notifies.
    pub pr: RwSignal<(Option<PrKey>, u64)>,
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
                Ok(ServerEvent::InboxesChanged) => updates.inboxes.update(|n| *n += 1),
                Ok(ServerEvent::PrChanged(key)) => updates.pr.update(|(k, n)| {
                    *k = Some(key);
                    *n += 1;
                }),
                Err(_) => {}
            }
        });
    source.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
    // After a reconnect (e.g. the server restarted) refetch everything.
    let on_open = Closure::<dyn FnMut()>::new(move || updates.inboxes.update(|n| *n += 1));
    source.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    on_open.forget();
    std::mem::forget(source);
}

#[component]
fn App() -> impl IntoView {
    let updates = Updates {
        inboxes: RwSignal::new(0),
        pr: RwSignal::new((None, 0)),
    };
    provide_context(updates);
    listen(updates);

    let selected = RwSignal::new(key_from_hash(&current_hash()));
    let handle = window_event_listener_untyped("hashchange", move |_| {
        selected.set(key_from_hash(&current_hash()));
    });
    on_cleanup(move || handle.remove());

    view! {
        <div class="app">
            <sidebar::Sidebar selected=selected />
            <main class="main">
                {move || match selected.get() {
                    Some(key) => view! { <pr::PrView key=key /> }.into_any(),
                    None => view! {
                        <div class="empty">"Pick a PR from an inbox on the left."</div>
                    }.into_any(),
                }}
            </main>
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
