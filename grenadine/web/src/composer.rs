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

/// The comment boxes open on the page, by draft name. Provided above the
/// parts of the page that are rebuilt when the PR is refetched, so that an
/// open box survives that even before anything was typed into it.
#[derive(Clone, Copy)]
pub struct OpenBoxes(RwSignal<std::collections::HashSet<String>>);

impl OpenBoxes {
    /// Starts out with the boxes that hold drafts open.
    pub fn new() -> OpenBoxes {
        let prefix = comment_draft_key("");
        let mut open = std::collections::HashSet::new();
        if let Some(s) = storage() {
            for i in 0..s.length().unwrap_or(0) {
                if let Some(name) = s
                    .key(i)
                    .ok()
                    .flatten()
                    .and_then(|k| k.strip_prefix(&prefix).map(str::to_owned))
                {
                    open.insert(name);
                }
            }
        }
        OpenBoxes(RwSignal::new(open))
    }

    /// Whether the box for `draft` is open: opened and not closed since,
    /// or holding a draft when the page loaded.
    pub fn is_open(self, draft: &str) -> bool {
        self.0.with(|open| open.contains(draft))
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

/// Closes the box for `draft` and drops its draft. This goes through
/// storage and `boxes`, which outlive the box itself: a refetch of the PR
/// may replace the box while its comment is being sent.
fn close_box(boxes: OpenBoxes, draft: &str) {
    if let Some(s) = storage() {
        let _ = s.remove_item(&comment_draft_key(draft));
    }
    boxes.set(draft, false);
    boxes.set(&preview_name(draft), false);
}

/// The name under which `OpenBoxes` notes that the box for `draft` shows
/// its preview, so that a refetch doesn't switch it back.
fn preview_name(draft: &str) -> String {
    format!("preview:{draft}")
}

/// Whether a key sends the comment: Ctrl+Enter, or Cmd+Enter on a Mac.
/// Plain Enter adds a line, as comments are usually several lines long.
fn submits(ev: &web_sys::KeyboardEvent) -> bool {
    ev.key() == "Enter" && (ev.ctrl_key() || ev.meta_key()) && !ev.is_composing()
}

/// A comment box. `draft` names its draft (see `comment_draft_key`) and
/// `repo` is where markdown references in the preview point. `alt` adds a
/// second button that sends the comment another way. `initial` is the text
/// to start with when there's no draft. The box is one of `OpenBoxes`, by
/// its draft's name: Cancel, and sending, close it and drop the draft.
#[component]
pub fn Composer(
    draft: String,
    repo: String,
    #[prop(into)] label: String,
    #[prop(into)] placeholder: String,
    send: SendComment,
    #[prop(optional)] alt: Option<(String, SendComment)>,
    #[prop(optional)] initial: Option<String>,
    #[prop(optional)] autofocus: bool,
) -> impl IntoView {
    let boxes = expect_context::<OpenBoxes>();
    let text = draft_signal(comment_draft_key(&draft));
    let draft = StoredValue::new(draft);
    if let Some(initial) = initial.filter(|_| text.with_untracked(|t| t.trim().is_empty())) {
        text.set(initial);
    }
    let (alt_label, alt_send) = alt.unzip();
    let alt_send = StoredValue::new(alt_send);
    let shown = preview_name(&draft.get_value());
    let preview = {
        let shown = shown.clone();
        Signal::derive(move || boxes.is_open(&shown))
    };
    let shown = StoredValue::new(shown);
    let set_preview = move |on: bool| shown.with_value(|s| boxes.set(s, on));
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
    // Sends with the main button's action, or with `alt`'s.
    let submit_with = move |use_alt: bool| {
        if empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        let body = text.get_untracked();
        let fut = match alt_send.with_value(|a| a.clone()).filter(|_| use_alt) {
            Some(alt) => alt(body),
            None => send.with_value(|send| send(body)),
        };
        let draft = draft.get_value();
        spawn_local(async move {
            match fut.await {
                Ok(()) => {
                    let _ = text.try_set(String::new());
                    close_box(boxes, &draft);
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
            let _ = busy.try_set(false);
        });
    };
    let submit = move || submit_with(false);
    view! {
        <form class="comment-composer" on:submit=move |ev| {
            ev.prevent_default();
            submit();
        }>
            <div class="composer-tabs" role="tablist">
                <button type="button" role="tab" class:active=move || !preview.get()
                    aria-selected=move || (!preview.get()).to_string()
                    on:click=move |_| set_preview(false)>"Write"</button>
                <button type="button" role="tab" class:active=move || preview.get()
                    aria-selected=move || preview.get().to_string()
                    on:click=move |_| set_preview(true)>"Preview"</button>
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
                {alt_label.map(|l| view! {
                    <button type="button" disabled=move || empty() || busy.get() on:click=move |_| submit_with(true)>{l}</button>
                })}
                <button type="button" on:click=move |_| {
                    text.set(String::new());
                    draft.with_value(|d| close_box(boxes, d));
                }>"Cancel"</button>
                <span class="muted composer-hint">"Ctrl+Enter sends"</span>
            </div>
        </form>
    }
}
