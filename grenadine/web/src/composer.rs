//! A box to write a comment in, with a preview. What's written is kept as
//! a draft in localStorage until it's sent.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::inboxes::storage;
use crate::pr::PrLinks;
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

/// The comment boxes on the page, by draft name: which are open, which
/// are sending and why a send failed. Provided above the parts of the page
/// that are rebuilt when the PR is refetched, so that all of that survives
/// it: an open box even before anything was typed into it, and a send that
/// a sync of another change interrupts.
#[derive(Clone, Copy)]
pub struct OpenBoxes {
    open: RwSignal<HashSet<String>>,
    sending: RwSignal<HashSet<String>>,
    errors: RwSignal<HashMap<String, String>>,
}

impl OpenBoxes {
    pub fn new() -> OpenBoxes {
        OpenBoxes {
            open: RwSignal::new(Default::default()),
            sending: RwSignal::new(Default::default()),
            errors: RwSignal::new(Default::default()),
        }
    }

    /// Whether the box for `draft` is open: opened and not closed since,
    /// or holding a draft.
    pub fn is_open(self, draft: &str) -> bool {
        self.open.with(|open| open.contains(draft)) || has_draft(draft)
    }

    // The setters ignore a page that's gone, which a send may finish after.

    pub fn set(self, draft: &str, open: bool) {
        self.open.try_update(|o| {
            if open {
                o.insert(draft.to_owned());
            } else {
                o.remove(draft);
            }
        });
    }

    pub fn sending(self, draft: &str) -> bool {
        self.sending.with(|s| s.contains(draft))
    }

    fn set_sending(self, draft: &str, sending: bool) {
        self.sending.try_update(|s| {
            if sending {
                s.insert(draft.to_owned());
            } else {
                s.remove(draft);
            }
        });
    }

    /// Why the last send from the box for `draft` failed.
    pub fn error(self, draft: &str) -> Option<String> {
        self.errors.with(|e| e.get(draft).cloned())
    }

    fn set_error(self, draft: &str, error: Option<String>) {
        self.errors.try_update(|e| match error {
            Some(error) => e.insert(draft.to_owned(), error),
            None => e.remove(draft),
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

/// What runs once a comment was sent. Not a `Callback`, since the
/// component that passed it may be gone by the time the server answers.
pub type OnSent = Arc<dyn Fn() + Send + Sync>;

/// Whether a key sends the comment: Ctrl+Enter, or Cmd+Enter on a Mac.
/// Plain Enter adds a line, as comments are usually several lines long.
fn submits(ev: &web_sys::KeyboardEvent) -> bool {
    ev.key() == "Enter" && (ev.ctrl_key() || ev.meta_key()) && !ev.is_composing()
}

/// The hint for the keys that send a comment, as a Mac or anything else
/// writes them.
fn send_hint() -> &'static str {
    let platform = js_sys::Reflect::get(&window(), &"navigator".into())
        .and_then(|n| js_sys::Reflect::get(&n, &"platform".into()))
        .ok()
        .and_then(|p| p.as_string())
        .unwrap_or_default();
    if platform.starts_with("Mac") {
        "⌘+Enter sends"
    } else {
        "Ctrl+Enter sends"
    }
}

/// A comment box. `draft` names its draft (see `comment_draft_key`) and
/// its state in `OpenBoxes`, and `repo` is where markdown references in
/// the preview point. `on_cancel` adds a Cancel button, which is disabled
/// while a send is in flight; `on_sent` runs once the comment was sent.
#[component]
pub fn Composer(
    draft: String,
    repo: String,
    #[prop(into)] label: String,
    #[prop(into)] placeholder: String,
    send: SendComment,
    #[prop(optional)] on_cancel: Option<Callback<()>>,
    #[prop(optional)] on_sent: Option<OnSent>,
    #[prop(optional)] autofocus: bool,
) -> impl IntoView {
    let text = draft_signal(comment_draft_key(&draft));
    let preview = RwSignal::new(false);
    let boxes = expect_context::<OpenBoxes>();
    let busy = {
        let draft = draft.clone();
        Signal::derive(move || boxes.sending(&draft))
    };
    let error = {
        let draft = draft.clone();
        Signal::derive(move || boxes.error(&draft))
    };
    let textarea = NodeRef::<leptos::html::Textarea>::new();
    if autofocus {
        Effect::new(move |_| {
            if let Some(t) = textarea.get() {
                let _ = t.focus();
            }
        });
    }
    let send = StoredValue::new(send);
    let (draft, on_sent) = (StoredValue::new(draft), StoredValue::new(on_sent));
    let empty = move || text.with(|t| t.trim().is_empty());
    let submit = move || {
        if empty() || busy.get_untracked() {
            return;
        }
        let (draft, on_sent) = (draft.get_value(), on_sent.get_value());
        boxes.set_sending(&draft, true);
        boxes.set_error(&draft, None);
        let fut = send.with_value(|send| send(text.get_untracked()));
        // By the time the server answers, this box may be gone: its thread
        // folded, or the page rebuilt by a sync. So what outlives it is
        // kept in `boxes`, and the rest is set with `try_set`.
        spawn_local(async move {
            match fut.await {
                Ok(()) => {
                    // Dropped here rather than by the draft's Effect, which
                    // may be gone, so the box doesn't come back with a
                    // comment that's already posted.
                    if let Some(s) = storage() {
                        let _ = s.remove_item(&comment_draft_key(&draft));
                    }
                    text.try_set(String::new());
                    preview.try_set(false);
                    if let Some(f) = on_sent {
                        f();
                    }
                }
                // The draft is still stored, so the text isn't lost.
                Err(e) => boxes.set_error(&draft, Some(e)),
            }
            boxes.set_sending(&draft, false);
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
                    <button type="button" disabled=move || busy.get() on:click=move |_| {
                        text.set(String::new());
                        boxes.set_error(&draft.get_value(), None);
                        cb.run(());
                    }>"Cancel"</button>
                })}
                <span class="muted composer-hint">{send_hint()}</span>
            </div>
        </form>
    }
}

/// A button that opens a comment box in its place, e.g. Reply. The box
/// closes on Cancel and once the comment was sent. `children` are more
/// buttons shown next to it while the box is closed.
#[component]
pub fn ComposerToggle(
    draft: String,
    #[prop(into)] label: String,
    #[prop(into)] placeholder: String,
    send: SendComment,
    #[prop(optional)] children: Option<ChildrenFn>,
) -> impl IntoView {
    let repo = expect_context::<PrLinks>().repo;
    let boxes = expect_context::<OpenBoxes>();
    let open = {
        let draft = draft.clone();
        Signal::derive(move || boxes.is_open(&draft))
    };
    let close = {
        let draft = draft.clone();
        move || boxes.set(&draft, false)
    };
    let on_sent: OnSent = Arc::new(close.clone());
    let on_cancel = Callback::new(move |()| close());
    move || {
        if open.get() {
            view! {
                <Composer
                    draft=draft.clone()
                    repo=repo.clone()
                    label=label.clone()
                    placeholder=placeholder.clone()
                    send=send.clone()
                    on_cancel=on_cancel
                    on_sent=on_sent.clone()
                    autofocus=true
                />
            }
            .into_any()
        } else {
            let draft = draft.clone();
            view! {
                <div class="form-buttons">
                    <button type="button" on:click=move |_| boxes.set(&draft, true)>{label.clone()}</button>
                    {children.clone().map(|c| c())}
                </div>
            }
            .into_any()
        }
    }
}
