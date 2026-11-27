//! Moving between the app's pages with the history API instead of
//! navigations to a new hash.
//!
//! WebKit on iOS 27 follows a hash navigation, by a link or by
//! `location`, with a load of the whole page when the page comes over
//! plain HTTP from the local network. `pushState` and `replaceState`
//! aren't navigations and don't. See
//! <https://github.com/ioBroker/ioBroker.vis-2/issues/743>.

use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue};

use crate::current_hash;

/// Goes to `hash`, a new entry in the history.
pub fn push(hash: &str) {
    if current_hash() == hash {
        return;
    }
    if let Ok(history) = window().history()
        && history
            .push_state_with_url(&JsValue::NULL, "", Some(hash))
            .is_ok()
    {
        announce();
    }
}

/// Goes to `hash` in place of the current entry in the history.
pub fn replace(hash: &str) {
    if current_hash() == hash {
        return;
    }
    replace_quietly(hash);
    announce();
}

/// Like `replace`, for a caller that shows the page of `hash` itself.
pub fn replace_quietly(hash: &str) {
    if let Ok(history) = window().history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(hash));
    }
}

/// Sends the `hashchange` event that the history API doesn't, once the
/// caller is done, as after a navigation.
fn announce() {
    leptos::task::spawn_local(async {
        // The first poll may run right away.
        let _ =
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(&JsValue::NULL)).await;
        if let Ok(event) = web_sys::Event::new("hashchange") {
            let _ = window().dispatch_event(&event);
        }
    });
}

/// Handles plain clicks on the app's own links with `push` for the life
/// of the current owner, wherever they are, rendered markdown included.
/// Other clicks are the browser's, so that Ctrl-click still opens a new
/// tab, for instance.
pub fn follow_links() {
    let handle = window_event_listener(leptos::ev::click, |ev| {
        if ev.default_prevented()
            || ev.button() != 0
            || ev.ctrl_key()
            || ev.meta_key()
            || ev.shift_key()
            || ev.alt_key()
        {
            return;
        }
        let Some(link) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|e| e.closest("a[href^='#/']").ok().flatten())
        else {
            return;
        };
        if link.has_attribute("target") || link.has_attribute("download") {
            return;
        }
        if let Some(hash) = link.get_attribute("href") {
            ev.prevent_default();
            push(&hash);
        }
    });
    on_cleanup(move || handle.remove());
}
