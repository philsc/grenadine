//! A local log of page memory samples, kept in the page and in session
//! storage so that a memory report can be matched to what the page was
//! doing. Session storage has a quota of its own, so a growing log can't
//! crowd out the local-storage drafts and settings, and it still
//! survives a reload or a crash restore of the tab. Samples are taken every 30 s, on every hash navigation, and a
//! few seconds after one; a diff finishing loading logs its size.
//!
//! The browser-dependent parts are wasm32-only; the log's shapes and its
//! buffer are tested on the host.

use std::cell::RefCell;
use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// How many entries the log keeps; the oldest are dropped.
const MAX_ENTRIES: usize = 4000;

/// How often a `sample` entry is recorded.
#[cfg(target_arch = "wasm32")]
const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// How long after a `nav` entry its `sample` follows.
#[cfg(target_arch = "wasm32")]
const AFTER_NAV: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(target_arch = "wasm32")]
const KEY: &str = "grenadine-memlog";
#[cfg(target_arch = "wasm32")]
const PREVIOUS_KEY: &str = "grenadine-memlog-previous";

/// The tab's session storage.
#[cfg(target_arch = "wasm32")]
fn storage() -> Option<web_sys::Storage> {
    leptos::prelude::window().session_storage().ok().flatten()
}

/// One cache's size. Only the parts a cache has show in the JSON: blobs
/// and highlighted lines count bytes, changes counts files.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Count {
    pub entries: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,
}

/// Every cache's `Count`, keyed like the JSON shape.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Caches {
    pub blobs: Count,
    pub changes: Count,
    pub highlight: Count,
    pub shapes: Count,
}

/// A loaded diff's size, on `diff` entries only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    pub files: usize,
    pub changed_lines: usize,
}

/// One log line: `sample` for a periodic or post-navigation sample,
/// `nav` for a hash change, `diff` for a finished diff.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix milliseconds.
    pub at: u64,
    pub kind: String,
    /// The page's `location.hash` when recorded.
    pub route: String,
    /// `WebAssembly.Memory`'s buffer size.
    pub wasm_bytes: u64,
    /// How many elements the document holds.
    pub dom_elements: u32,
    pub caches: Caches,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<Diff>,
}

thread_local! {
    static LOG: RefCell<VecDeque<Entry>> = RefCell::default();
    /// The previous page load's log, moved aside at `start`.
    static PREVIOUS: RefCell<Vec<Entry>> = RefCell::default();
}

/// Appends `entry`, dropping the oldest once the log is full.
fn push(log: &mut VecDeque<Entry>, entry: Entry) {
    while log.len() >= MAX_ENTRIES {
        log.pop_front();
    }
    log.push_back(entry);
}

/// What the caches hold right now.
pub fn caches() -> Caches {
    let (blobs_entries, blobs_bytes) = crate::api::blob_cache_stats();
    let (changes_entries, changes_files) = crate::api::changes_cache_stats();
    let (highlight_entries, highlight_bytes) = crate::highlight::cache_stats();
    Caches {
        blobs: Count {
            entries: blobs_entries,
            bytes: Some(blobs_bytes),
            files: None,
        },
        changes: Count {
            entries: changes_entries,
            bytes: None,
            files: Some(changes_files),
        },
        highlight: Count {
            entries: highlight_entries,
            bytes: Some(highlight_bytes),
            files: None,
        },
        shapes: Count {
            entries: crate::diffview::shape_cache_entries(),
            bytes: None,
            files: None,
        },
    }
}

/// The download file's `{"current": [...], "previous": [...]}`.
fn download_json(current: &[Entry], previous: &[Entry]) -> String {
    serde_json::json!({"current": current, "previous": previous}).to_string()
}

/// The wasm heap's and the document's current sizes.
#[cfg(target_arch = "wasm32")]
pub fn page_size() -> (u64, u32) {
    use wasm_bindgen::JsCast;

    let memory: js_sys::WebAssembly::Memory = wasm_bindgen::memory().unchecked_into();
    let wasm_bytes = memory
        .buffer()
        .unchecked_into::<js_sys::ArrayBuffer>()
        .byte_length() as u64;
    // The live collection, so no NodeList of every element is allocated
    // on a huge page.
    let dom_elements = leptos::prelude::document()
        .get_elements_by_tag_name("*")
        .length();
    (wasm_bytes, dom_elements)
}

/// Zero on the host, where there is no page to measure.
#[cfg(not(target_arch = "wasm32"))]
pub fn page_size() -> (u64, u32) {
    (0, 0)
}

#[cfg(target_arch = "wasm32")]
fn record(kind: &str, diff: Option<Diff>) {
    let (wasm_bytes, dom_elements) = page_size();
    let entry = Entry {
        at: js_sys::Date::now() as u64,
        kind: kind.to_owned(),
        route: leptos::prelude::window()
            .location()
            .hash()
            .unwrap_or_default(),
        wasm_bytes,
        dom_elements,
        caches: caches(),
        diff,
    };
    LOG.with_borrow_mut(|log| {
        push(log, entry);
        if let Some(storage) = storage()
            && let Ok(json) = serde_json::to_string(&*log)
        {
            let _ = storage.set_item(KEY, &json);
        }
    });
}

/// Records a `diff` entry for a diff that finished loading.
pub fn diff(files: usize, changed_lines: usize) {
    #[cfg(target_arch = "wasm32")]
    record(
        "diff",
        Some(Diff {
            files,
            changed_lines,
        }),
    );
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (files, changed_lines);
}

/// No-op on the host, where there is no page to sample.
#[cfg(not(target_arch = "wasm32"))]
pub fn start() {}

/// Moves any previous page load's log aside, records the first sample,
/// and starts the periodic sampling and the navigation listener. The
/// interval and the listener live for the page's lifetime.
#[cfg(target_arch = "wasm32")]
pub fn start() {
    use leptos::prelude::{set_interval_with_handle, set_timeout, window_event_listener_untyped};

    if let Some(storage) = storage()
        && let Ok(Some(json)) = storage.get_item(KEY)
    {
        let _ = storage.set_item(PREVIOUS_KEY, &json);
        PREVIOUS.with_borrow_mut(|p| *p = serde_json::from_str(&json).unwrap_or_default());
    }
    record("sample", None);
    let _ = set_interval_with_handle(|| record("sample", None), SAMPLE_EVERY);
    let _handle = window_event_listener_untyped("hashchange", move |_| {
        record("nav", None);
        set_timeout(move || record("sample", None), AFTER_NAV);
    });
}

/// How many entries the current and the previous log hold.
pub fn lengths() -> (usize, usize) {
    (
        LOG.with_borrow(VecDeque::len),
        PREVIOUS.with_borrow(Vec::len),
    )
}

/// Downloads the log and the previous page load's as one JSON file.
#[cfg(target_arch = "wasm32")]
pub fn download() {
    use wasm_bindgen::JsCast;

    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let json = LOG.with_borrow(|log| {
        PREVIOUS.with_borrow(|prev| download_json(&log.iter().cloned().collect::<Vec<_>>(), prev))
    });
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("application/json");
    let Ok(blob) = web_sys::Blob::new_with_str_sequence_and_options(
        &js_sys::Array::of1(&json.into()).into(),
        &options,
    ) else {
        return;
    };
    let Ok(url) = web_sys::Url::create_object_url_with_blob(&blob) else {
        return;
    };
    let now = js_sys::Date::new_0();
    let name = format!(
        "grenadine-memlog-{:04}{:02}{:02}-{:02}{:02}{:02}.json",
        now.get_full_year(),
        now.get_month() + 1,
        now.get_date(),
        now.get_hours(),
        now.get_minutes(),
        now.get_seconds(),
    );
    if let Ok(anchor) = document.create_element("a") {
        let anchor: web_sys::HtmlAnchorElement = anchor.unchecked_into();
        anchor.set_href(&url);
        anchor.set_download(&name);
        anchor.click();
    }
    let _ = web_sys::Url::revoke_object_url(&url);
}

/// No-op on the host, where there is no document to download to.
#[cfg(not(target_arch = "wasm32"))]
pub fn download() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str) -> Entry {
        Entry {
            at: 1,
            kind: kind.to_owned(),
            route: "#/".to_owned(),
            wasm_bytes: 0,
            dom_elements: 0,
            caches: Caches::default(),
            diff: None,
        }
    }

    #[test]
    fn the_full_log_drops_its_oldest() {
        let mut log = VecDeque::new();
        for i in 0..MAX_ENTRIES + 5 {
            let mut e = entry("sample");
            e.at = i as u64;
            push(&mut log, e);
        }
        assert_eq!(log.len(), MAX_ENTRIES);
        assert_eq!(log.front().unwrap().at, 5);
    }

    #[test]
    fn entry_json_shape() {
        let mut e = entry("sample");
        e.caches.blobs = Count {
            entries: 2,
            bytes: Some(10),
            files: None,
        };
        e.caches.changes = Count {
            entries: 3,
            bytes: None,
            files: Some(5),
        };
        e.caches.shapes = Count {
            entries: 7,
            bytes: None,
            files: None,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "sample");
        assert_eq!(v["route"], "#/");
        assert_eq!(
            v["caches"]["blobs"],
            serde_json::json!({"entries": 2, "bytes": 10})
        );
        assert_eq!(
            v["caches"]["changes"],
            serde_json::json!({"entries": 3, "files": 5})
        );
        assert_eq!(v["caches"]["shapes"], serde_json::json!({"entries": 7}));
        assert!(v.get("diff").is_none());

        e.diff = Some(Diff {
            files: 4,
            changed_lines: 9,
        });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            v["diff"],
            serde_json::json!({"files": 4, "changed_lines": 9})
        );
    }

    #[test]
    fn download_json_shape() {
        let v: serde_json::Value =
            serde_json::from_str(&download_json(&[entry("nav")], &[entry("sample")])).unwrap();
        assert_eq!(v["current"][0]["kind"], "nav");
        assert_eq!(v["previous"][0]["kind"], "sample");
    }
}
