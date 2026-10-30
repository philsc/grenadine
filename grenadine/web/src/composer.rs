//! A box to write a comment in, with a preview. What's written is kept as
//! a draft in localStorage until it's sent.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::inboxes::storage;
use crate::{api, markdown};

/// The text kept in localStorage under `key`: the draft left there last,
/// kept as it changes and dropped once empty.
pub fn draft_signal(key: String) -> RwSignal<String> {
    let draft = RwSignal::new(
        storage()
            .and_then(|s| s.get_item(&key).ok().flatten())
            .unwrap_or_default(),
    );
    Effect::new(move |_| {
        let Some(s) = storage() else { return };
        draft.with(|d| {
            let _ = if d.trim().is_empty() {
                s.remove_item(&key)
            } else {
                s.set_item(&key, d)
            };
        });
    });
    draft
}

/// The localStorage key of a comment draft. `id` tells drafts apart, e.g.
/// `reply.<thread>`.
pub fn comment_draft_key(id: &str) -> String {
    format!("grenadine.comment-draft.{id}")
}

/// Whether a comment draft is waiting under `id`.
pub fn has_draft(id: &str) -> bool {
    storage()
        .and_then(|s| s.get_item(&comment_draft_key(id)).ok().flatten())
        .is_some_and(|d| !d.trim().is_empty())
}

/// The comment boxes open on the page, by draft name. Provided above the
/// parts of the page that are rebuilt when the PR is refetched, so that an
/// open box survives that even before anything was typed into it.
#[derive(Clone, Copy)]
pub struct OpenBoxes(RwSignal<std::collections::HashSet<String>>);

impl OpenBoxes {
    pub fn new() -> OpenBoxes {
        OpenBoxes(RwSignal::new(Default::default()))
    }

    /// Whether the box for `draft` is open: opened and not closed since,
    /// or holding a draft.
    pub fn is_open(self, draft: &str) -> bool {
        self.0.with(|open| open.contains(draft)) || has_draft(draft)
    }

    pub fn set(self, draft: &str, open: bool) {
        self.0.update(|o| {
            if open {
                o.insert(draft.to_owned());
            } else {
                o.remove(draft);
            }
        });
    }
}

/// What sends the comment; it resolves once the server took it.
pub type SendComment =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = api::Result<()>>>> + Send + Sync>;

/// Wraps an async function as a `SendComment`.
pub fn sender<F, Fut>(f: F) -> SendComment
where
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = api::Result<()>> + 'static,
{
    Arc::new(move |body| Box::pin(f(body)))
}

/// Whether a key sends the comment: Ctrl+Enter, or Cmd+Enter on a Mac.
/// Plain Enter adds a line, as comments are usually several lines long.
fn submits(ev: &web_sys::KeyboardEvent) -> bool {
    ev.key() == "Enter" && (ev.ctrl_key() || ev.meta_key()) && !ev.is_composing()
}

/// A comment box. `draft` names its draft (see `comment_draft_key`) and
/// `repo` is where markdown references in the preview point. `on_cancel`
/// adds a Cancel button; `on_sent` runs once the comment was sent.
#[component]
pub fn Composer(
    draft: String,
    repo: String,
    #[prop(into)] label: String,
    #[prop(into)] placeholder: String,
    send: SendComment,
    #[prop(optional)] on_cancel: Option<Callback<()>>,
    #[prop(optional)] on_sent: Option<Callback<()>>,
    #[prop(optional)] autofocus: bool,
) -> impl IntoView {
    let text = draft_signal(comment_draft_key(&draft));
    let preview = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let textarea = NodeRef::<leptos::html::Textarea>::new();
    if autofocus {
        Effect::new(move |_| {
            if let Some(t) = textarea.get() {
                let _ = t.focus();
            }
        });
    }
    let send = StoredValue::new(send);
    let empty = move || text.with(|t| t.trim().is_empty());
    let submit = move || {
        if empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        let fut = send.with_value(|send| send(text.get_untracked()));
        spawn_local(async move {
            match fut.await {
                Ok(()) => {
                    text.set(String::new());
                    preview.set(false);
                    if let Some(cb) = on_sent {
                        cb.run(());
                    }
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <form class="comment-composer" on:submit=move |ev| {
            ev.prevent_default();
            submit();
        }>
            <div class="composer-tabs" role="tablist">
                <button type="button" role="tab" class:active=move || !preview.get()
                    aria-selected=move || (!preview.get()).to_string()
                    on:click=move |_| preview.set(false)>"Write"</button>
                <button type="button" role="tab" class:active=move || preview.get()
                    aria-selected=move || preview.get().to_string()
                    on:click=move |_| preview.set(true)>"Preview"</button>
            </div>
            <textarea
                node_ref=textarea
                rows="4"
                aria-label=label.clone()
                placeholder=placeholder
                class:hidden=move || preview.get()
                prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if submits(&ev) {
                        ev.prevent_default();
                        submit();
                    }
                }
            ></textarea>
            {move || preview.get().then(|| {
                let html = text.with(|t| {
                    if t.trim().is_empty() {
                        "<p class=\"muted\">Nothing to preview.</p>".to_owned()
                    } else {
                        markdown::to_html(t, &repo)
                    }
                });
                view! { <div class="markdown composer-preview" inner_html=html></div> }
            })}
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
            <div class="form-buttons">
                <button type="submit" class="primary" disabled=move || empty() || busy.get()>
                    {move || if busy.get() { "Sending…".to_owned() } else { label.clone() }}
                </button>
                {on_cancel.map(|cb| view! {
                    <button type="button" on:click=move |_| {
                        text.set(String::new());
                        cb.run(());
                    }>"Cancel"</button>
                })}
                <span class="muted composer-hint">"Ctrl+Enter sends"</span>
            </div>
        </form>
    }
}
