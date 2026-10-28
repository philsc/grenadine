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
    draft_signal_from(key, String::new())
}

/// Like `draft_signal`, but starting out as `initial` when there's no
/// draft, which is only kept once it's changed. E.g. the box that edits a
/// comment starts with the comment, and leaves no draft behind until it's
/// actually edited.
pub fn draft_signal_from(key: String, initial: String) -> RwSignal<String> {
    let draft = RwSignal::new(
        storage()
            .and_then(|s| s.get_item(&key).ok().flatten())
            .unwrap_or_else(|| initial.clone()),
    );
    Effect::new(move |_| {
        let Some(s) = storage() else { return };
        draft.with(|d| {
            let _ = if d.trim().is_empty() || *d == initial {
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

/// Drops the stored draft `id`. Done right away rather than left to the
/// draft's Effect, which may be gone, e.g. once a comment was sent, so that
/// its box doesn't come back with a comment that's already posted.
fn drop_draft(id: &str) {
    if let Some(s) = storage() {
        let _ = s.remove_item(&comment_draft_key(id));
    }
}

/// The comment boxes on the page, and the other changes started from it,
/// by draft name: which boxes are open and show their preview, which
/// changes are being sent and why a send failed. Provided above the parts
/// of the page that are rebuilt when the PR is refetched, so that all of
/// that survives it: an open box even before anything was typed into it,
/// and a send that a sync of another change interrupts.
#[derive(Clone, Copy)]
pub struct OpenBoxes {
    open: RwSignal<HashSet<String>>,
    previews: RwSignal<HashSet<String>>,
    sending: RwSignal<HashSet<String>>,
    errors: RwSignal<HashMap<String, String>>,
}

/// Inserts `name` into `set`, or removes it.
fn toggle(set: RwSignal<HashSet<String>>, name: &str, on: bool) {
    set.try_update(|s| {
        if on {
            s.insert(name.to_owned());
        } else {
            s.remove(name);
        }
    });
}

impl OpenBoxes {
    pub fn new() -> OpenBoxes {
        OpenBoxes {
            open: RwSignal::new(Default::default()),
            previews: RwSignal::new(Default::default()),
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
        toggle(self.open, draft, open);
    }

    /// Whether the box for `draft` shows its preview.
    pub fn preview(self, draft: &str) -> bool {
        self.previews.with(|p| p.contains(draft))
    }

    pub fn set_preview(self, draft: &str, on: bool) {
        toggle(self.previews, draft, on);
    }

    /// Whether the change named `name` is being sent.
    pub fn sending(self, name: &str) -> bool {
        self.sending.with(|s| s.contains(name))
    }

    /// Why the last send of the change named `name` failed.
    pub fn error(self, name: &str) -> Option<String> {
        self.errors.with(|e| e.get(name).cloned())
    }

    fn set_error(self, name: &str, error: Option<String>) {
        self.errors.try_update(|e| match error {
            Some(error) => e.insert(name.to_owned(), error),
            None => e.remove(name),
        });
    }

    /// Sends `change`, named `name`, unless a change by that name is being
    /// sent already. Notes that it's being sent meanwhile and why it
    /// failed if it does, and runs `on_ok` once it succeeded. The component
    /// that started it may be gone by then, its thread folded or the page
    /// rebuilt by a sync, so `on_ok` must not need it.
    pub fn send(
        self,
        name: &str,
        change: impl Future<Output = api::Result<()>> + 'static,
        on_ok: impl FnOnce() + 'static,
    ) {
        if self.sending.with_untracked(|s| s.contains(name)) {
            return;
        }
        let name = name.to_owned();
        toggle(self.sending, &name, true);
        self.set_error(&name, None);
        spawn_local(async move {
            match change.await {
                Ok(()) => on_ok(),
                Err(e) => self.set_error(&name, Some(e)),
            }
            toggle(self.sending, &name, false);
        });
    }

    /// Closes the box for `draft` and drops its draft.
    pub fn close(self, draft: &str) {
        drop_draft(draft);
        self.set(draft, false);
        self.set_preview(draft, false);
        self.set_error(draft, None);
    }

    /// Closes the boxes whose drafts' names start with `prefix` and that
    /// `keep` doesn't keep, open or only stored, and drops their drafts.
    pub fn close_unless(self, prefix: &str, keep: impl Fn(&str) -> bool) {
        let mut names: HashSet<String> = self.open.with_untracked(|o| {
            o.iter()
                .filter(|n| n.starts_with(prefix))
                .cloned()
                .collect()
        });
        let drafts = comment_draft_key("");
        if let Some(s) = storage() {
            for i in 0..s.length().unwrap_or(0) {
                if let Some(name) = s.key(i).ok().flatten().and_then(|k| {
                    k.strip_prefix(&drafts)
                        .filter(|n| n.starts_with(prefix))
                        .map(str::to_owned)
                }) {
                    names.insert(name);
                }
            }
        }
        for name in names.iter().filter(|n| !keep(n)) {
            self.close(name);
        }
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

/// A markdown text box with Write and Preview tabs. `draft` names the box
/// in `OpenBoxes`, which keeps whether it shows its preview, and `repo` is
/// where markdown references in the preview point. `on_submit` runs on
/// Ctrl+Enter.
#[component]
pub fn MarkdownEditor(
    text: RwSignal<String>,
    draft: String,
    repo: String,
    #[prop(into)] label: String,
    #[prop(into)] placeholder: String,
    #[prop(default = 4)] rows: u32,
    #[prop(optional)] on_submit: Option<Callback<()>>,
    #[prop(optional)] autofocus: bool,
) -> impl IntoView {
    let boxes = expect_context::<OpenBoxes>();
    let draft = StoredValue::new(draft);
    let preview = Signal::derive(move || draft.with_value(|d| boxes.preview(d)));
    let set_preview = move |on: bool| draft.with_value(|d| boxes.set_preview(d, on));
    let textarea = NodeRef::<leptos::html::Textarea>::new();
    if autofocus {
        Effect::new(move |_| {
            if let Some(t) = textarea.get() {
                let _ = t.focus();
            }
        });
    }
    view! {
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
            class="markdown-editor"
            rows=rows
            aria-label=label
            placeholder=placeholder
            class:hidden=move || preview.get()
            prop:value=move || text.get()
            on:input=move |ev| text.set(event_target_value(&ev))
            on:keydown=move |ev| {
                if let Some(cb) = on_submit
                    && submits(&ev)
                {
                    ev.prevent_default();
                    cb.run(());
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
    }
}

/// A comment box. `draft` names its draft (see `comment_draft_key`) and
/// its state in `OpenBoxes`, and `repo` is where markdown references in
/// the preview point. `alt` adds a second button that sends the comment
/// another way. `initial` is the text to start with when there's no
/// draft. Cancel, which is disabled while a send is in flight, and sending
/// close the box in `OpenBoxes` and drop its draft.
#[component]
pub fn Composer(
    draft: String,
    repo: String,
    #[prop(into)] label: String,
    #[prop(into)] placeholder: String,
    send: SendComment,
    #[prop(optional_no_strip)] alt: Option<(String, SendComment)>,
    #[prop(optional, into)] initial: Option<String>,
    #[prop(optional)] autofocus: bool,
) -> impl IntoView {
    let boxes = expect_context::<OpenBoxes>();
    let text = draft_signal_from(comment_draft_key(&draft), initial.unwrap_or_default());
    let busy = {
        let draft = draft.clone();
        Signal::derive(move || boxes.sending(&draft))
    };
    let error = {
        let draft = draft.clone();
        Signal::derive(move || boxes.error(&draft))
    };
    let (alt_label, alt_send) = alt.unzip();
    let (send, alt_send) = (StoredValue::new(send), StoredValue::new(alt_send));
    let draft = StoredValue::new(draft);
    let empty = move || text.with(|t| t.trim().is_empty());
    // Sends with the main button's action, or with `alt`'s.
    let submit_with = move |use_alt: bool| {
        if empty() || busy.get_untracked() {
            return;
        }
        let body = text.get_untracked();
        let fut = match alt_send.get_value().filter(|_| use_alt) {
            Some(alt) => alt(body),
            None => send.with_value(|send| send(body)),
        };
        let name = draft.get_value();
        // The draft stays stored until the send succeeded, so a failed one
        // doesn't lose the text.
        boxes.send(&name.clone(), fut, move || {
            text.try_set(String::new());
            boxes.close(&name);
        });
    };
    let submit = Callback::new(move |()| submit_with(false));
    view! {
        <form class="comment-composer" on:submit=move |ev| {
            ev.prevent_default();
            submit.run(());
        }>
            <MarkdownEditor
                text=text
                draft=draft.get_value()
                repo=repo
                label=label.clone()
                placeholder=placeholder
                on_submit=submit
                autofocus=autofocus
            />
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
            <div class="form-buttons">
                <button type="submit" class="primary" disabled=move || empty() || busy.get()>
                    {move || if busy.get() { "Sending…".to_owned() } else { label.clone() }}
                </button>
                {alt_label.map(|l| view! {
                    <button type="button" disabled=move || empty() || busy.get()
                        on:click=move |_| submit_with(true)>{l}</button>
                })}
                <button type="button" disabled=move || busy.get() on:click=move |_| {
                    text.set(String::new());
                    draft.with_value(|d| boxes.close(d));
                }>"Cancel"</button>
                <span class="muted composer-hint">{send_hint()}</span>
            </div>
        </form>
    }
}

/// A button that opens a comment box in its place, e.g. Reply. The box
/// closes on Cancel and once the comment was sent. `send_label` is the
/// box's main button, `label` by default, and `alt` its second one, see
/// `Composer`. `children` are more buttons shown next to the button while
/// the box is closed.
#[component]
pub fn ComposerToggle(
    draft: String,
    #[prop(into)] label: String,
    #[prop(into)] placeholder: String,
    send: SendComment,
    #[prop(optional, into)] send_label: Option<String>,
    #[prop(optional)] alt: Option<(String, SendComment)>,
    #[prop(optional)] children: Option<ChildrenFn>,
) -> impl IntoView {
    let repo = expect_context::<PrLinks>().repo;
    let boxes = expect_context::<OpenBoxes>();
    let open = {
        let draft = draft.clone();
        Signal::derive(move || boxes.is_open(&draft))
    };
    let send_label = send_label.unwrap_or_else(|| label.clone());
    move || {
        if open.get() {
            view! {
                <Composer
                    draft=draft.clone()
                    repo=repo.clone()
                    label=send_label.clone()
                    placeholder=placeholder.clone()
                    send=send.clone()
                    alt=alt.clone()
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
