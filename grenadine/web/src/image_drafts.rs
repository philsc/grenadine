//! Prompt images kept with their draft in IndexedDB, so they survive
//! leaving the form and reloads like the draft text does. localStorage is
//! too small for them: one image can be ~4 MB of base64.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use grenadine_core::api::PromptImage;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

const DB: &str = "grenadine";
const STORE: &str = "prompt-images";

thread_local! {
    /// The open database, shared by the page's composers.
    static DATABASE: RefCell<Option<Rc<web_sys::IdbDatabase>>> = const { RefCell::new(None) };
    /// Set once IndexedDB failed, so it isn't tried again and again.
    static FAILED: Cell<bool> = const { Cell::new(false) };
}

/// The key a new-session draft's images are stored under.
pub fn new_session_key(id: &str) -> String {
    format!("new-session.{id}")
}

/// The key a session's follow-up images are stored under.
pub fn agent_key(id: &str) -> String {
    format!("agent.{id}")
}

fn warn(message: &str) {
    web_sys::console::warn_1(&format!("prompt images: {message}").into());
}

fn give_up(message: &str) -> Option<Rc<web_sys::IdbDatabase>> {
    warn(message);
    FAILED.with(|f| f.set(true));
    None
}

/// Resolves to `request`'s result, or `Err` when it errors.
async fn request(request: &web_sys::IdbRequest) -> Result<JsValue, JsValue> {
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        let ok = Closure::once_into_js(move || {
            let _ = resolve.call0(&JsValue::NULL);
        });
        let bad = Closure::once_into_js(move || {
            let _ = reject.call0(&JsValue::NULL);
        });
        request.set_onsuccess(Some(ok.unchecked_ref()));
        request.set_onerror(Some(bad.unchecked_ref()));
    });
    JsFuture::from(promise).await?;
    request.result()
}

/// The open database, or `None` when IndexedDB isn't there or failed.
/// `None` keeps the composers working in memory only.
async fn db() -> Option<Rc<web_sys::IdbDatabase>> {
    if let Some(db) = DATABASE.with(|d| d.borrow().clone()) {
        return Some(db);
    }
    if FAILED.with(Cell::get) {
        return None;
    }
    let Some(factory) = web_sys::window().and_then(|w| w.indexed_db().ok().flatten()) else {
        return give_up("IndexedDB is not available");
    };
    let Ok(open) = factory.open_with_u32(DB, 1) else {
        return give_up("can't open the IndexedDB database");
    };
    let create = {
        let open = open.clone();
        Closure::once_into_js(move || {
            if let Ok(db) = open.result() {
                let db: web_sys::IdbDatabase = db.unchecked_into();
                let _ = db.create_object_store(STORE);
            }
        })
    };
    open.set_onupgradeneeded(Some(create.unchecked_ref()));
    match request(&open).await {
        Ok(db) => {
            let db = Rc::new(db.unchecked_into::<web_sys::IdbDatabase>());
            DATABASE.with(|d| *d.borrow_mut() = Some(db.clone()));
            Some(db)
        }
        Err(e) => give_up(&format!("can't open the IndexedDB database: {e:?}")),
    }
}

fn store(
    db: &web_sys::IdbDatabase,
    mode: web_sys::IdbTransactionMode,
) -> Option<web_sys::IdbObjectStore> {
    db.transaction_with_str_and_mode(STORE, mode)
        .and_then(|t| t.object_store(STORE))
        .map_err(|e| warn(&format!("can't use the {STORE} store: {e:?}")))
        .ok()
}

/// The images stored under `key`, or `None` when there are none or
/// reading them failed.
async fn get(db: &web_sys::IdbDatabase, key: &str) -> Option<Vec<PromptImage>> {
    let store = store(db, web_sys::IdbTransactionMode::Readonly)?;
    let Ok(value) = request(&store.get(&JsValue::from_str(key)).ok()?).await else {
        warn("can't read the stored images");
        return None;
    };
    serde_json::from_str(&value.as_string()?).ok()
}

async fn put(db: &web_sys::IdbDatabase, key: &str, images: &[PromptImage]) {
    let Ok(json) = serde_json::to_string(images) else {
        return;
    };
    let Some(store) = store(db, web_sys::IdbTransactionMode::Readwrite) else {
        return;
    };
    let Ok(req) = store.put_with_key(&json.into(), &key.into()) else {
        warn("can't store the images");
        return;
    };
    if request(&req).await.is_err() {
        warn("can't store the images");
    }
}

async fn delete(db: &web_sys::IdbDatabase, key: &str) {
    let Some(store) = store(db, web_sys::IdbTransactionMode::Readwrite) else {
        return;
    };
    if let Ok(req) = store.delete(&key.into()) {
        let _ = request(&req).await;
    }
}

/// Every key in the store.
async fn keys(db: &web_sys::IdbDatabase) -> Vec<String> {
    let Some(store) = store(db, web_sys::IdbTransactionMode::Readonly) else {
        return Vec::new();
    };
    let Ok(req) = store.get_all_keys() else {
        return Vec::new();
    };
    request(&req)
        .await
        .ok()
        .map(|v| js_sys::Array::from(&v))
        .map(|keys| keys.iter().filter_map(|k| k.as_string()).collect())
        .unwrap_or_default()
}

/// The images being written under `key`: stored ones loaded in, and every
/// change written back once the load is done. Writing earlier could wipe
/// the stored list before it's read.
pub fn draft_images(key: String) -> RwSignal<Vec<PromptImage>> {
    let images = RwSignal::new(Vec::<PromptImage>::new());
    let loaded = RwSignal::new(false);
    spawn_local({
        let key = key.clone();
        async move {
            if let Some(db) = db().await
                && let Some(stored) = get(&db, &key).await
            {
                // Images added before the load finished come after the
                // stored ones.
                images.update(|list| {
                    let mut all = stored;
                    all.append(list);
                    *list = all;
                });
            }
            loaded.set(true);
        }
    });
    Effect::new(move |_| {
        let list = images.get();
        if !loaded.get() {
            return;
        }
        let key = key.clone();
        spawn_local(async move {
            let Some(db) = db().await else {
                return;
            };
            if list.is_empty() {
                delete(&db, &key).await;
            } else {
                put(&db, &key, &list).await;
            }
        });
    });
    images
}

/// Drops `key`'s stored images right away: the composer whose Effect
/// would clear them may already be gone.
pub fn forget(key: &str) {
    let key = key.to_owned();
    spawn_local(async move {
        if let Some(db) = db().await {
            delete(&db, &key).await;
        }
    });
}

/// Deletes the stored images `keep` rejects, e.g. `new-session.*` keys
/// whose draft is gone. Called once at page load.
pub fn sweep(keep: impl Fn(&str) -> bool + 'static) {
    spawn_local(async move {
        let Some(db) = db().await else {
            return;
        };
        for key in keys(&db).await {
            if !keep(&key) {
                delete(&db, &key).await;
            }
        }
    });
}
