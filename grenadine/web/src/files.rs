//! Files of any kind that go with a prompt, beyond the images the model
//! sees in the prompt itself (see `images`). Each is uploaded as soon as
//! it's attached and shows as a chip with its upload's progress; the
//! prompt goes with the uploads' ids, and the agent reads the files from
//! disk. The finished uploads are kept with the prompt's draft in
//! localStorage, so they survive leaving the form and reloads.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use grenadine_core::api::Upload;
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

thread_local! {
    static NEXT_KEY: Cell<u64> = const { Cell::new(0) };
    /// The files picked in this page by their keys, kept for retries, with
    /// the request uploading each, to abort it.
    static PICKED: RefCell<HashMap<u64, (web_sys::File, Option<web_sys::XmlHttpRequest>)>> =
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
    /// the server, which may have let it expire, and every change kept.
    pub fn draft(key: String) -> Files {
        let list: Vec<PromptFile> = stored(&key)
            .into_iter()
            .map(|up| PromptFile {
                key: next_key(),
                name: up.name.clone(),
                size: up.size,
                state: FileState::Ready(up),
            })
            .collect();
        let checks: Vec<(u64, Upload)> = list
            .iter()
            .filter_map(|f| match &f.state {
                FileState::Ready(up) => Some((f.key, up.clone())),
                _ => None,
            })
            .collect();
        let list = RwSignal::new(list);
        for (file, up) in checks {
            spawn_local(async move {
                if let Ok(false) = crate::api::upload_exists(&up.id).await {
                    set_state(list, file, FileState::Expired(up));
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

    /// Attaches `files` and starts uploading them.
    pub fn add(self, files: Vec<web_sys::File>) {
        for file in files {
            let key = next_key();
            self.list.update(|l| {
                l.push(PromptFile {
                    key,
                    name: file.name(),
                    size: file.size() as u64,
                    state: FileState::Uploading(0.0),
                })
            });
            PICKED.with(|p| p.borrow_mut().insert(key, (file.clone(), None)));
            upload(self, key, file);
        }
    }

    /// Takes the file `key` off the prompt, deleting its upload.
    fn remove(self, key: u64) {
        let state = self
            .list
            .with_untracked(|l| l.iter().find(|f| f.key == key).map(|f| f.state.clone()));
        self.list.update(|l| l.retain(|f| f.key != key));
        if let Some((_, Some(xhr))) = PICKED.with(|p| p.borrow_mut().remove(&key)) {
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
        let Some(file) = PICKED.with(|p| p.borrow().get(&key).map(|(f, _)| f.clone())) else {
            return;
        };
        set_state(self.list, key, FileState::Uploading(0.0));
        upload(self, key, file);
    }

    /// Clears the files once the prompt went with them: their uploads are
    /// the prompt's now.
    pub fn sent(self) {
        let keys: Vec<u64> = self
            .list
            .with_untracked(|l| l.iter().map(|f| f.key).collect());
        PICKED.with(|p| {
            let mut p = p.borrow_mut();
            for key in keys {
                p.remove(&key);
            }
        });
        self.list.set(Vec::new());
        keep(&self.key.get_value(), &[]);
    }
}

/// Whether a file in `list` isn't ready to go with the prompt.
fn pending(list: &[PromptFile]) -> bool {
    list.iter().any(|f| !matches!(f.state, FileState::Ready(_)))
}

/// Sets the state of the file `key` in `list`. `None` when `list` is gone
/// with its composer, `Some(false)` when the file was taken off.
fn set_state(list: RwSignal<Vec<PromptFile>>, key: u64, state: FileState) -> Option<bool> {
    list.try_update(|l| match l.iter_mut().find(|f| f.key == key) {
        Some(f) => {
            f.state = state;
            true
        }
        None => false,
    })
}

/// Uploads the file `key` of `files`, showing its progress.
fn upload(files: Files, key: u64, file: web_sys::File) {
    let list = files.list;
    let fail = move |e: String| {
        set_state(list, key, FileState::Failed(e));
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
        let progress =
            Closure::<dyn FnMut(web_sys::ProgressEvent)>::new(move |e: web_sys::ProgressEvent| {
                if e.length_computable() && e.total() > 0.0 {
                    set_state(list, key, FileState::Uploading(e.loaded() / e.total()));
                }
            });
        target.set_onprogress(Some(progress.into_js_value().unchecked_ref()));
    }
    let draft = files.key.get_value();
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
                if matches!(state, FileState::Ready(_)) {
                    p.remove(&key);
                } else if let Some((_, xhr)) = p.get_mut(&key) {
                    *xhr = None;
                }
            });
            // A composer that's gone finds the upload kept with its draft
            // when it's back.
            if set_state(list, key, state.clone()).is_none()
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
        if let Some((_, slot)) = p.borrow_mut().get_mut(&key) {
            *slot = Some(xhr);
        }
    });
}

/// Drops the uploads kept for the draft `key` and deletes them on the
/// server, e.g. when the draft is discarded. Uploads a prompt claimed are
/// gone from there already.
pub fn forget(key: &str) {
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

/// `bytes` for people, e.g. `1.2 MB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(12), "12 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1_258_291), "1.2 MB");
    }
}
