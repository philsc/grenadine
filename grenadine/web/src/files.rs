//! Files of any kind that go with a prompt, beyond the images the model
//! sees in the prompt itself (see `images`). Each is uploaded as soon as
//! it's attached and shows as a chip with its upload's progress; the
//! prompt goes with the uploads' ids, and the agent reads the files from
//! disk. The finished uploads are kept with the prompt's draft in
//! localStorage, so they survive leaving the form and reloads. Uploads
//! still going, or failed, stay in this page with their draft's key, so a
//! composer that's back for the draft shows them again.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

pub use grenadine_core::api::human_size;
use grenadine_core::api::{MAX_ATTACHMENTS, MAX_UPLOAD_BYTES, Upload};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;

use crate::inboxes::storage;

/// Where a file attached to a prompt stands.
#[derive(Clone, Debug, PartialEq)]
pub enum FileState {
    /// Being uploaded; how much is done, from 0 to 1.
    Uploading(f64),
    /// Uploaded, ready to go with the prompt.
    Ready(Upload),
    /// The upload failed, for the reason given; it can be tried again.
    Failed(String),
    /// The file can't go with the prompt, for the reason given, e.g.
    /// because it's too big; it's not uploaded.
    Refused(String),
    /// The server no longer has the upload, e.g. because it expired.
    Expired(Upload),
}

/// A file attached to a prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptFile {
    /// Tells the files apart while they're attached.
    key: u64,
    pub name: String,
    pub size: u64,
    pub state: FileState,
}

/// A file picked in this page whose upload isn't done: it's still going,
/// or it failed and can be tried again.
struct Picked {
    file: web_sys::File,
    /// The draft the file is attached to, an `image_drafts` key.
    draft: String,
    /// The request uploading the file, to abort it; `None` once it ended.
    xhr: Option<web_sys::XmlHttpRequest>,
    /// Why the upload failed, once it did.
    error: Option<String>,
}

thread_local! {
    static NEXT_KEY: Cell<u64> = const { Cell::new(0) };
    /// The files picked in this page whose uploads aren't done, by their
    /// keys.
    static PICKED: RefCell<HashMap<u64, Picked>> = RefCell::new(HashMap::new());
    /// The file lists of the composers shown now, by their drafts' keys.
    /// Uploads find theirs here, so one that ends after its composer is
    /// gone and back lands in the new one.
    static LIVE: RefCell<HashMap<String, RwSignal<Vec<PromptFile>>>> =
        RefCell::new(HashMap::new());
}

fn next_key() -> u64 {
    NEXT_KEY.with(|k| {
        let key = k.get();
        k.set(key + 1);
        key
    })
}

/// The localStorage key of the uploads kept for the draft `key`, which is
/// an `image_drafts` key.
fn storage_key(key: &str) -> String {
    format!("prompt-files.{key}")
}

/// The uploads kept for the draft `key`.
fn stored(key: &str) -> Vec<Upload> {
    storage()
        .and_then(|s| s.get_item(&storage_key(key)).ok().flatten())
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// Keeps `uploads` for the draft `key`.
fn keep(key: &str, uploads: &[Upload]) {
    let Some(s) = storage() else { return };
    if uploads.is_empty() {
        let _ = s.remove_item(&storage_key(key));
    } else if let Ok(v) = serde_json::to_string(uploads) {
        let _ = s.set_item(&storage_key(key), &v);
    }
}

/// The files attached to a prompt whose draft is kept under `key`, an
/// `image_drafts` key.
#[derive(Clone, Copy)]
pub struct Files {
    pub list: RwSignal<Vec<PromptFile>>,
    key: StoredValue<String>,
}

impl Files {
    /// The draft `key`'s files: the uploads kept for it, each checked with
    /// the server, which may have let it expire, and those this page is
    /// still uploading or failed to; every change is kept.
    pub fn draft(key: String) -> Files {
        let mut list: Vec<PromptFile> = stored(&key)
            .into_iter()
            .map(|up| PromptFile {
                key: next_key(),
                name: up.name.clone(),
                size: up.size,
                state: FileState::Ready(up),
            })
            .collect();
        PICKED.with(|p| {
            let mut picked: Vec<_> = p
                .borrow()
                .iter()
                .filter(|(_, f)| f.draft == key)
                .map(|(&k, f)| PromptFile {
                    key: k,
                    name: f.file.name(),
                    size: f.file.size() as u64,
                    state: match &f.error {
                        Some(e) => FileState::Failed(e.clone()),
                        None => FileState::Uploading(0.0),
                    },
                })
                .collect();
            // In the order they were picked.
            picked.sort_by_key(|f| f.key);
            list.extend(picked);
        });
        let checks: Vec<(u64, Upload)> = list
            .iter()
            .filter_map(|f| match &f.state {
                FileState::Ready(up) => Some((f.key, up.clone())),
                _ => None,
            })
            .collect();
        let list = RwSignal::new(list);
        LIVE.with(|l| l.borrow_mut().insert(key.clone(), list));
        on_cleanup({
            let key = key.clone();
            move || {
                LIVE.with(|l| {
                    let mut l = l.borrow_mut();
                    if l.get(&key) == Some(&list) {
                        l.remove(&key);
                    }
                })
            }
        });
        for (file, up) in checks {
            spawn_local(async move {
                if let Ok(false) = crate::api::upload_exists(&up.id).await {
                    update_state(list, file, FileState::Expired(up));
                }
            });
        }
        Effect::new({
            let key = key.clone();
            move |_| {
                let kept: Vec<Upload> = list.with(|l| {
                    l.iter()
                        .filter_map(|f| match &f.state {
                            FileState::Ready(up) | FileState::Expired(up) => Some(up.clone()),
                            _ => None,
                        })
                        .collect()
                });
                keep(&key, &kept);
            }
        });
        Files {
            list,
            key: StoredValue::new(key),
        }
    }

    /// The ids of the uploads that go with the prompt.
    pub fn ids(self) -> Vec<String> {
        self.list.with_untracked(|l| {
            l.iter()
                .filter_map(|f| match &f.state {
                    FileState::Ready(up) => Some(up.id.clone()),
                    _ => None,
                })
                .collect()
        })
    }

    /// Whether a file isn't ready to go: it's still uploading, failed or
    /// expired. The prompt waits until it is, or it's removed.
    pub fn pending(self) -> bool {
        self.list.with(|l| pending(l))
    }

    /// `pending`, outside the reactive graph.
    pub fn pending_untracked(self) -> bool {
        self.list.with_untracked(|l| pending(l))
    }

    pub fn is_empty(self) -> bool {
        self.list.with(Vec::is_empty)
    }

    /// Attaches `files` and starts uploading them. Those the server would
    /// refuse, because they're too big or too many, are refused right
    /// away: a browser may report a refused upload as a failed one, with
    /// a retry that can't work.
    pub fn add(self, files: Vec<web_sys::File>) {
        for file in files {
            let key = next_key();
            let size = file.size() as u64;
            let refused = if size > MAX_UPLOAD_BYTES {
                Some(format!(
                    "Too big; files can be at most {}",
                    human_size(MAX_UPLOAD_BYTES)
                ))
            } else if self.list.with_untracked(Vec::len) >= MAX_ATTACHMENTS {
                Some(format!(
                    "Too many; a prompt can have at most {MAX_ATTACHMENTS} files"
                ))
            } else {
                None
            };
            self.list.update(|l| {
                l.push(PromptFile {
                    key,
                    name: file.name(),
                    size,
                    state: match &refused {
                        Some(e) => FileState::Refused(e.clone()),
                        None => FileState::Uploading(0.0),
                    },
                })
            });
            if refused.is_some() {
                continue;
            }
            let picked = Picked {
                file: file.clone(),
                draft: self.key.get_value(),
                xhr: None,
                error: None,
            };
            PICKED.with(|p| p.borrow_mut().insert(key, picked));
            upload(key, file);
        }
    }

    /// Takes the file `key` off the prompt, deleting its upload.
    fn remove(self, key: u64) {
        let state = self
            .list
            .with_untracked(|l| l.iter().find(|f| f.key == key).map(|f| f.state.clone()));
        self.list.update(|l| l.retain(|f| f.key != key));
        if let Some(Picked { xhr: Some(xhr), .. }) = PICKED.with(|p| p.borrow_mut().remove(&key)) {
            let _ = xhr.abort();
        }
        if let Some(FileState::Ready(up)) = state {
            spawn_local(async move {
                let _ = crate::api::delete_upload(&up.id).await;
            });
        }
    }

    /// Uploads the file `key` again after its upload failed.
    fn retry(self, key: u64) {
        let Some(file) = PICKED.with(|p| {
            let mut p = p.borrow_mut();
            let f = p.get_mut(&key)?;
            f.error = None;
            Some(f.file.clone())
        }) else {
            return;
        };
        update_state(self.list, key, FileState::Uploading(0.0));
        upload(key, file);
    }

    /// Takes off the files whose uploads went with the prompt as `ids`:
    /// they're the prompt's now. Files attached since it went stay. `key`
    /// is the draft's, passed in because the composer that owns these
    /// files may be gone by the time its prompt is sent.
    pub fn sent(self, key: &str, ids: &[String]) {
        let went = |up: &Upload| ids.contains(&up.id);
        self.list
            .update(|l| l.retain(|f| !matches!(&f.state, FileState::Ready(up) if went(up))));
        let mut kept = stored(key);
        kept.retain(|up| !went(up));
        keep(key, &kept);
    }
}

/// Whether a file in `list` isn't ready to go with the prompt.
fn pending(list: &[PromptFile]) -> bool {
    list.iter().any(|f| !matches!(f.state, FileState::Ready(_)))
}

/// Sets the state of the file `key` in `list`. `None` when `list` is gone
/// with its composer, `Some(false)` when the file was taken off.
fn update_state(list: RwSignal<Vec<PromptFile>>, key: u64, state: FileState) -> Option<bool> {
    list.try_update(|l| match l.iter_mut().find(|f| f.key == key) {
        Some(f) => {
            f.state = state;
            true
        }
        None => false,
    })
}

/// Sets the state of the file `key` attached to the draft `draft`, in the
/// composer shown for it. `None` when there's none, `Some(false)` when the
/// file was taken off.
fn set_state(draft: &str, key: u64, state: FileState) -> Option<bool> {
    let list = LIVE.with(|l| l.borrow().get(draft).copied())?;
    update_state(list, key, state)
}

/// Uploads the picked file `key`, showing its progress in its draft's
/// composer while there is one.
fn upload(key: u64, file: web_sys::File) {
    let Some(draft) = PICKED.with(|p| p.borrow().get(&key).map(|f| f.draft.clone())) else {
        return;
    };
    let fail = {
        let draft = draft.clone();
        move |e: String| {
            PICKED.with(|p| {
                if let Some(f) = p.borrow_mut().get_mut(&key) {
                    f.error = Some(e.clone());
                }
            });
            set_state(&draft, key, FileState::Failed(e));
        }
    };
    let Ok(xhr) = web_sys::XmlHttpRequest::new() else {
        return fail("the browser can't upload files".into());
    };
    let name = String::from(js_sys::encode_uri_component(&file.name()));
    if xhr
        .open("PUT", &format!("/api/uploads?name={name}"))
        .is_err()
    {
        return fail("the upload couldn't start".into());
    }
    if let Ok(target) = xhr.upload() {
        let draft = draft.clone();
        let progress =
            Closure::<dyn FnMut(web_sys::ProgressEvent)>::new(move |e: web_sys::ProgressEvent| {
                if e.length_computable() && e.total() > 0.0 {
                    set_state(&draft, key, FileState::Uploading(e.loaded() / e.total()));
                }
            });
        target.set_onprogress(Some(progress.into_js_value().unchecked_ref()));
    }
    let done = Closure::once_into_js({
        let xhr = xhr.clone();
        move || {
            let status = xhr.status().unwrap_or(0);
            let text = xhr.response_text().ok().flatten().unwrap_or_default();
            let state = match status {
                200 => match serde_json::from_str::<Upload>(&text) {
                    Ok(up) => FileState::Ready(up),
                    Err(e) => FileState::Failed(format!("the server's answer is garbled: {e}")),
                },
                0 => FileState::Failed("the upload failed".into()),
                _ => FileState::Failed(format!("{status}: {text}")),
            };
            PICKED.with(|p| {
                let mut p = p.borrow_mut();
                match &state {
                    FileState::Ready(_) => {
                        p.remove(&key);
                    }
                    FileState::Failed(e) => {
                        if let Some(f) = p.get_mut(&key) {
                            f.xhr = None;
                            f.error = Some(e.clone());
                        }
                    }
                    _ => {}
                }
            });
            // A composer that's gone finds the upload kept with its draft
            // when it's back.
            if set_state(&draft, key, state.clone()).is_none()
                && let FileState::Ready(up) = state
            {
                let mut kept = stored(&draft);
                kept.push(up);
                keep(&draft, &kept);
            }
        }
    });
    xhr.set_onloadend(Some(done.unchecked_ref()));
    if xhr.send_with_opt_blob(Some(&file)).is_err() {
        return fail("the upload couldn't start".into());
    }
    PICKED.with(|p| {
        if let Some(f) = p.borrow_mut().get_mut(&key) {
            f.xhr = Some(xhr);
        }
    });
}

/// Drops the uploads kept for the draft `key` and deletes them on the
/// server, e.g. when the draft is discarded, and stops the ones still
/// going. Uploads a prompt claimed are gone from there already.
pub fn forget(key: &str) {
    let picked: Vec<Picked> = PICKED.with(|p| {
        let mut p = p.borrow_mut();
        let keys: Vec<u64> = p
            .iter()
            .filter(|(_, f)| f.draft == key)
            .map(|(&k, _)| k)
            .collect();
        keys.into_iter().filter_map(|k| p.remove(&k)).collect()
    });
    for f in picked {
        if let Some(xhr) = f.xhr {
            let _ = xhr.abort();
        }
    }
    let uploads = stored(key);
    keep(key, &[]);
    for up in uploads {
        spawn_local(async move {
            let _ = crate::api::delete_upload(&up.id).await;
        });
    }
}

/// Forgets the uploads kept for the drafts `keep` rejects, e.g.
/// `new-session.*` keys whose draft is gone. Called once at page load.
pub fn sweep(keep: impl Fn(&str) -> bool) {
    let Some(s) = storage() else { return };
    let prefix = storage_key("");
    let len = s.length().unwrap_or(0);
    let keys: Vec<String> = (0..len)
        .filter_map(|i| s.key(i).ok().flatten())
        .filter_map(|k| k.strip_prefix(&prefix).map(str::to_owned))
        .collect();
    for key in keys {
        if !keep(&key) {
            forget(&key);
        }
    }
}

/// The files attached to a prompt, each with its upload's state and a
/// button that takes it off. Nothing while there are none.
#[component]
pub fn FileChips(files: Files) -> impl IntoView {
    view! {
        <div class="prompt-files" hidden=move || files.is_empty()>
            {move || {
                files
                    .list
                    .get()
                    .into_iter()
                    .map(|f| {
                        let key = f.key;
                        let (status, progress, retry, bad) = match &f.state {
                            FileState::Uploading(done) => (
                                format!("Uploading {:.0}%", done * 100.0),
                                Some(*done),
                                false,
                                false,
                            ),
                            FileState::Ready(_) => (human_size(f.size), None, false, false),
                            FileState::Failed(e) => (format!("Failed: {e}"), None, true, true),
                            FileState::Refused(e) => (e.clone(), None, false, true),
                            FileState::Expired(_) => {
                                ("Expired; attach it again".into(), None, false, true)
                            }
                        };
                        let remove = format!("Remove {}", f.name);
                        view! {
                            <div class="prompt-file" class:bad=bad title=f.name.clone()>
                                <span class="prompt-file-name">{f.name.clone()}</span>
                                <span class="prompt-file-status">{status}</span>
                                {progress.map(|p| view! { <progress max="1" value=p></progress> })}
                                {retry.then(|| view! {
                                    <button type="button" on:click=move |_| files.retry(key)>
                                        "Retry"
                                    </button>
                                })}
                                <button
                                    type="button"
                                    title=remove.clone()
                                    aria-label=remove
                                    on:click=move |_| files.remove(key)
                                >
                                    "×"
                                </button>
                            </div>
                        }
                    })
                    .collect_view()
            }}
        </div>
    }
}
