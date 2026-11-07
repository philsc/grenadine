//! One PR: its title and description, the version picker, the review
//! comments that don't fit into the diff, and the diff.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use grenadine_core::api::{
    Person, PrDetail, PrKey, Review, ReviewComment, ReviewInfo, Side, Stack, Version, VersionKind,
};
use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};

use crate::composer::{Composer, ComposerToggle, OpenBoxes, SendComment, sender};
use crate::diffview::Commenting;
use crate::diffview::DiffView;
use crate::inboxes::{load_flag, save_flag, storage};
use crate::review::{
    CheckFolds, MergeChoice, MergeTarget, PrSidebar, ReviewChoice, ReviewPanel, conversation,
    merge_box, merge_warning,
};
use crate::{GithubLink, Updates, api, github_url, hash_for, highlight, markdown};

/// A review comment and its replies.
#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub root: ReviewComment,
    pub replies: Vec<ReviewComment>,
}

/// Where in a file a thread goes. Lines are 1-based, like GitHub's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Anchor {
    File,
    Line(Side, u32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    pub anchor: Anchor,
    pub thread: Arc<Thread>,
}

/// Threads that can't be shown in the current diff, with the version each
/// was made on.
#[derive(Clone, Debug, PartialEq)]
pub struct Unplaced {
    pub thread: Arc<Thread>,
    pub version: Option<u32>,
}

/// The two versions being compared. 0 is "Base".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub base: u32,
    pub head: u32,
}

fn threads(comments: &[ReviewComment]) -> Vec<Thread> {
    let mut roots: Vec<Thread> = Vec::new();
    let mut index: HashMap<u64, usize> = HashMap::new();
    for c in comments.iter().filter(|c| c.in_reply_to.is_none()) {
        index.insert(c.id, roots.len());
        roots.push(Thread {
            root: c.clone(),
            replies: Vec::new(),
        });
    }
    for c in comments.iter().filter(|c| c.in_reply_to.is_some()) {
        match c.in_reply_to.and_then(|id| index.get(&id)) {
            Some(&i) => roots[i].replies.push(c.clone()),
            // The root is gone; show the reply on its own.
            None => roots.push(Thread {
                root: c.clone(),
                replies: Vec::new(),
            }),
        }
    }
    roots
}

/// Decides where each thread goes for the selected diff:
/// - inline on the left at the line it was made on, when the diff's left
///   side is the version it was made on;
/// - inline, when Head is the commit the comment was made on;
/// - inline at GitHub's mapped-forward line, when Head is the latest version;
/// - otherwise in the side panel.
///
/// Left-side comments were made against the PR's base at the time, so they
/// only go inline when the diff's left side is Head's merge-base.
pub fn place(
    comments: &[ReviewComment],
    versions: &[Version],
    sel: Selection,
) -> (HashMap<String, Vec<Placed>>, Vec<Unplaced>) {
    let mut inline: HashMap<String, Vec<Placed>> = HashMap::new();
    let mut panel = Vec::new();
    let head = &versions[sel.head as usize - 1];
    let is_latest = sel.head as usize == versions.len();
    for thread in threads(comments) {
        let c = &thread.root;
        let anchor = if sel.base != 0 && c.original_commit == versions[sel.base as usize - 1].sha {
            // Made on the version the diff's left side shows: its
            // right-side lines are that version's own.
            if c.on_file {
                Some(Anchor::File)
            } else {
                match (c.original_line, c.side) {
                    (Some(l), Side::Right) => Some(Anchor::Line(Side::Left, l)),
                    _ => None,
                }
            }
        } else {
            let line = if c.original_commit == head.sha {
                c.original_line
            } else if is_latest {
                c.line
            } else {
                None
            };
            if c.on_file && (c.original_commit == head.sha || is_latest) {
                Some(Anchor::File)
            } else {
                match (line, c.side) {
                    (Some(l), Side::Right) => Some(Anchor::Line(Side::Right, l)),
                    (Some(l), Side::Left) if sel.base == 0 => Some(Anchor::Line(Side::Left, l)),
                    _ => None,
                }
            }
        };
        let thread = Arc::new(thread);
        match anchor {
            Some(anchor) => inline
                .entry(thread.root.path.clone())
                .or_default()
                .push(Placed { anchor, thread }),
            None => panel.push(Unplaced {
                version: versions
                    .iter()
                    .find(|v| v.sha == thread.root.original_commit)
                    .map(|v| v.number),
                thread,
            }),
        }
    }
    (inline, panel)
}

/// The number of resolved threads and of all threads made on each version,
/// by version number. Threads made on commits that aren't versions aren't
/// counted.
fn thread_counts(comments: &[ReviewComment], versions: &[Version]) -> HashMap<u32, (u32, u32)> {
    let numbers: HashMap<&str, u32> = versions
        .iter()
        .map(|v| (v.sha.as_str(), v.number))
        .collect();
    let mut counts: HashMap<u32, (u32, u32)> = HashMap::new();
    // Pending threads aren't threads until the review is submitted.
    for t in threads(comments).into_iter().filter(|t| !t.root.pending) {
        if let Some(&n) = numbers.get(t.root.original_commit.as_str()) {
            let (resolved, total) = counts.entry(n).or_default();
            *resolved += u32::from(t.root.resolved);
            *total += 1;
        }
    }
    counts
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// What the page says for a PR that hasn't synced yet. While background
/// syncs are paused to keep GitHub's rate limit, the poller won't sync it
/// until the reset.
fn pending_message(key: &PrKey, paused_until: Option<i64>, now: i64) -> String {
    if let Some(t) = paused_until.filter(|&t| t > now) {
        format!(
            "{}#{} hasn't synced yet. It may be waiting for GitHub's rate limit to reset at {}.",
            key.repo,
            key.number,
            crate::topbar::hm(t)
        )
    } else {
        format!("{}#{} hasn't synced yet", key.repo, key.number)
    }
}

#[component]
pub fn PrView(key: PrKey) -> impl IntoView {
    let updates = expect_context::<Updates>();
    // Changes only when this PR, or any after a reconnect, changed.
    let rev = {
        let key = key.clone();
        Memo::new(move |prev: Option<&u64>| {
            updates.pr.with(|(k, n)| {
                if k.as_ref().is_none_or(|k| k == &key) {
                    *n
                } else {
                    prev.copied().unwrap_or(0)
                }
            })
        })
    };
    let pr = {
        let key = key.clone();
        LocalResource::new(move || {
            rev.track();
            let key = key.clone();
            async move { api::pr(&key).await }
        })
    };
    // `None` until the user picks versions: Base against the latest.
    let chosen = RwSignal::new(None::<Selection>);
    let side_by_side = RwSignal::new(load_flag("side-by-side", true));
    let inline_changes = RwSignal::new(load_flag("inline-changes", true));
    let rebase_older = RwSignal::new(load_flag("rebase-older-version", false));
    let github = github_url(&key);
    // Kept here, outside what syncs and version changes rebuild.
    provide_context(Folded(RwSignal::new(HashMap::new())));
    provide_context(CheckFolds(RwSignal::new(HashMap::new())));
    provide_context(OpenBoxes::new());
    provide_context(ReviewChoice::new());
    provide_context(MergeChoice::new());

    // The sidebar is a resizable column on wide windows, which is
    // remembered, and an overlay the button opens on narrow ones, which
    // isn't.
    let page = page_width();
    let wide = Memo::new(move |_| page.get() >= DIFF_MIN + SIDEBAR_MIN);
    let collapsed = RwSignal::new(load_flag("pr-sidebar-collapsed", false));
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        if wide.get() {
            open.set(false);
        }
    });
    let shown = move || {
        if wide.get() {
            !collapsed.get()
        } else {
            open.get()
        }
    };
    let toggle = move |_| {
        if wide.get_untracked() {
            collapsed.update(|c| *c = !*c);
            save_flag("pr-sidebar-collapsed", collapsed.get_untracked());
        } else {
            open.update(|o| *o = !*o);
        }
    };
    // The width the divider last dragged to, shared by all PRs; `None` is
    // the default split. It's written when a drag ends, so a window too
    // narrow for it shows it clamped rather than losing it.
    let saved = RwSignal::new(saved_width());
    let width = Signal::derive(move || sidebar_width(saved.get(), page.get(), wide.get()));
    // The click the browser fires on the common ancestor when a drag ends
    // away from the divider would close the overlay, so the next click
    // after a drag is ignored here.
    let dragged = StoredValue::new_local(false);
    let drag = StoredValue::new_local(None::<(WindowListenerHandle, WindowListenerHandle)>);
    let on_resize_start = Callback::new(move |e: web_sys::MouseEvent| {
        e.prevent_default();
        let (x, w) = (
            e.client_x() as f64,
            sidebar_width(
                saved.get_untracked(),
                page.get_untracked(),
                wide.get_untracked(),
            ),
        );
        if let Some(body) = document().body() {
            let _ = body.class_list().add_1("resizing-sidebar");
        }
        let on_move = window_event_listener(leptos::ev::mousemove, move |e| {
            saved.set(Some(sidebar_width(
                Some(w + x - e.client_x() as f64),
                page.get_untracked(),
                wide.get_untracked(),
            )));
        });
        let on_up = window_event_listener(leptos::ev::mouseup, move |_| {
            if let (Some(s), Some(w)) = (storage(), saved.get_untracked()) {
                let _ = s.set_item(WIDTH_KEY, &w.to_string());
            }
            if let Some(body) = document().body() {
                let _ = body.class_list().remove_1("resizing-sidebar");
            }
            drag.update_value(|d| {
                if let Some((a, b)) = d.take() {
                    a.remove();
                    b.remove();
                }
            });
            dragged.set_value(true);
            // The click that follows the mouseup is dispatched before
            // timers run; releasing the mouse outside the window fires
            // none, so the flag resets itself for the next click.
            set_timeout(move || dragged.set_value(false), Duration::ZERO);
        });
        drag.set_value(Some((on_move, on_up)));
    });
    let on_resize_reset = Callback::new(move |_: web_sys::MouseEvent| {
        saved.set(None);
        if let Some(s) = storage() {
            let _ = s.remove_item(WIDTH_KEY);
        }
    });
    // Escape and clicks outside the overlay's sidebar close it.
    let on_key = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" {
            open.set(false);
        }
    });
    let on_click = window_event_listener(leptos::ev::click, move |ev| {
        if dragged.get_value() {
            dragged.set_value(false);
            return;
        }
        if !open.get_untracked() || wide.get_untracked() {
            return;
        }
        let inside = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|t| t.closest(".pr-sidebar, .sidebar-toggle").ok().flatten())
            .is_some();
        if !inside {
            open.set(false);
        }
    });
    on_cleanup(move || {
        on_key.remove();
        on_click.remove();
    });

    // Whether the PR loaded. A Memo, so a refetch that leaves it true
    // doesn't remount the sidebar: the agents panel pinned under its
    // body holds the sessions' live event streams.
    let ready = Memo::new(move |_| matches!(pr.get(), Some(Ok(api::PrResponse::Ready(_)))));
    // While the sidebar shows, its toggle sits in the sidebar's header;
    // while hidden, it floats clear of `.main`'s scrollbar at the page's
    // top-right.
    let button = move |floating: bool| {
        view! {
            <button type="button" class="sidebar-toggle" class:floating=floating
                aria-label=move || if shown() { "Hide sidebar" } else { "Show sidebar" }
                aria-expanded=move || shown().to_string()
                on:click=toggle>
                <svg
                    viewBox="0 0 16 16"
                    width="14"
                    height="14"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.5"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    aria-hidden="true"
                >
                    <path d=move || if shown() { "M6 4l4 4-4 4" } else { "M10 4l-4 4 4 4" } />
                </svg>
            </button>
        }
    };
    let panel_key = key.clone();

    view! {
        {move || {
            match pr.get() {
            None => view! { <main class="main"><div class="empty">"Loading…"</div></main> }.into_any(),
            Some(Err(e)) => view! { <main class="main"><div class="empty error">{format!("Can't load {}#{}: {e}", key.repo, key.number)}</div></main> }.into_any(),
            Some(Ok(api::PrResponse::Pending)) => {
                // The closure reads `updates.sync` so only this text, not the
                // whole view, re-renders when the sync status changes.
                let key = key.clone();
                view! { <main class="main"><div class="empty">{move || pending_message(&key, updates.sync.with(|s| s.paused_until), (js_sys::Date::now() / 1000.0) as i64)}</div></main> }.into_any()
            }
            Some(Ok(api::PrResponse::NotConfigured)) => view! {
                <main class="main"><div class="empty">{format!("{} isn't configured in grenadine. ", key.repo)}<a href=github.clone() target="_blank" rel="noopener">"See it on GitHub"</a></div></main>
            }.into_any(),
            Some(Ok(api::PrResponse::Failed(e))) => view! {
                <main class="main"><div class="empty error">{format!("Can't sync {}#{}: {e}. ", key.repo, key.number)}<a href=github.clone() target="_blank" rel="noopener">"See it on GitHub"</a></div></main>
            }.into_any(),
            Some(Ok(api::PrResponse::Ready(pr))) => {
                let pr = Arc::new(*pr);
                prefetch(&pr);
                view! {
                    <main class="main"><PrBody pr=pr.clone() chosen=chosen side_by_side=side_by_side inline_changes=inline_changes rebase_older=rebase_older /></main>
                }.into_any()
            }
        }
        }}
        {move || {
            let panel_key = panel_key.clone();
            if !ready.get() {
                ().into_any()
            } else if shown() {
                view! {
                    <PrSidebar overlay=Signal::derive(move || !wide.get())
                        width=width
                        on_resize_start=on_resize_start
                        on_resize_reset=on_resize_reset>
                        <div class="pr-sidebar-body">
                            <div class="sidebar-head">
                                <h2>"Reviewers"</h2>
                                {button(false)}
                            </div>
                            {move || match pr.get() {
                                Some(Ok(api::PrResponse::Ready(detail))) => {
                                    crate::review::sidebar_body(&detail)
                                }
                                _ => ().into_any(),
                            }}
                        </div>
                        <crate::pr_agents::PrAgentPanel key=panel_key />
                    </PrSidebar>
                }
                .into_any()
            } else {
                button(true).into_any()
            }
        }}
    }
}

/// The narrowest the diff is allowed to get by the sidebar widening.
const DIFF_MIN: f64 = 1180.0;
/// The narrowest the sidebar is allowed to get, on column and overlay
/// alike.
const SIDEBAR_MIN: f64 = 320.0;
/// The diff's width before a drag moves it; the sidebar's default is the
/// rest of the page.
const DIFF_DEFAULT: f64 = 1600.0;
/// The sliver of the page the overlay's sidebar leaves visible at its
/// widest.
const OVERLAY_GAP: f64 = 48.0;
/// Where the dragged sidebar width is kept, for all PRs.
const WIDTH_KEY: &str = "grenadine.pr-sidebar-width";

/// The window's width, kept current. It decides both whether the
/// sidebar's a column or an overlay and how wide it may get. The PR page
/// comes and goes, so the listener is removed on unmount.
fn page_width() -> RwSignal<f64> {
    let width = || {
        window()
            .inner_width()
            .ok()
            .and_then(|w| w.as_f64())
            .unwrap_or(0.0)
    };
    let page = RwSignal::new(width());
    let on_resize = window_event_listener_untyped("resize", move |_| page.set(width()));
    on_cleanup(move || on_resize.remove());
    page
}

/// The sidebar width kept from the last drag; `None` is the default
/// split.
fn saved_width() -> Option<f64> {
    storage()
        .and_then(|s| s.get_item(WIDTH_KEY).ok().flatten())
        .and_then(|w| w.parse().ok())
}

/// The width the sidebar shows at. A `wide` page clamps it to what the
/// diff's minimum leaves, an overlay to the page minus a sliver, and the
/// minimum wins where even that isn't room. `saved` too wide is clamped
/// rather than dropped, so widening the window brings it back.
fn sidebar_width(saved: Option<f64>, page: f64, wide: bool) -> f64 {
    let max = if wide {
        page - DIFF_MIN
    } else {
        page - OVERLAY_GAP
    };
    saved
        .unwrap_or(page - DIFF_DEFAULT)
        .min(max)
        .max(SIDEBAR_MIN)
}

/// Threads the user folded or unfolded, by root comment ID. Threads not in
/// here are folded exactly when they're resolved.
#[derive(Clone, Copy)]
struct Folded(RwSignal<HashMap<u64, bool>>);

/// What the markdown of one PR needs: its repo for links.
#[derive(Clone)]
pub(crate) struct PrLinks {
    pub(crate) repo: String,
}

/// The PR whose page this is, for the comments and actions that change it
/// on GitHub. Missing on pages that only show diffs, e.g. an agent's.
#[derive(Clone)]
pub(crate) struct ReviewTarget {
    pub(crate) key: PrKey,
    /// The viewer has a pending review on the PR.
    pub(crate) reviewing: bool,
}

/// Warms the caches for the diffs most likely to be looked at next, down
/// to their highlighting.
fn prefetch(pr: &PrDetail) {
    let v = &pr.versions;
    let mut pairs = Vec::new();
    if let Some(last) = v.last() {
        if let Some(mb) = &last.merge_base {
            pairs.push((mb.clone(), last.sha.clone()));
        }
        if v.len() >= 2 {
            pairs.push((v[v.len() - 2].sha.clone(), last.sha.clone()));
        }
    }
    let repo = pr.summary.key.repo.clone();
    leptos::task::spawn_local(async move {
        for (from, to) in pairs {
            if let Ok(changes) = api::changes(&repo, &from, &to).await {
                let ids = changes
                    .files
                    .iter()
                    .flat_map(|f| f.old_blob.iter().chain(&f.new_blob))
                    .cloned();
                let _ = api::load_blobs(&repo, ids.collect::<Vec<_>>()).await;
                for f in &changes.files {
                    let old_path = f.old_path.as_deref().unwrap_or(f.path());
                    for (path, id) in [(old_path, &f.old_blob), (f.path(), &f.new_blob)] {
                        if let Some((id, blob)) =
                            id.as_deref().and_then(|id| Some((id, api::blob(id)?)))
                            && blob.text.is_some()
                        {
                            highlight::request(path, id, blob, false, |_| {});
                        }
                    }
                }
            }
        }
    });
}

#[component]
fn PrBody(
    pr: Arc<PrDetail>,
    chosen: RwSignal<Option<Selection>>,
    side_by_side: RwSignal<bool>,
    inline_changes: RwSignal<bool>,
    rebase_older: RwSignal<bool>,
) -> impl IntoView {
    provide_context(PrLinks {
        repo: pr.summary.key.repo.clone(),
    });
    provide_context(ReviewTarget {
        key: pr.summary.key.clone(),
        reviewing: pr
            .review
            .as_ref()
            .is_some_and(|r| r.pending_review.is_some()),
    });
    // Edits of comments that are no longer pending, because they were
    // deleted or their review submitted or discarded, e.g. on github.com,
    // can't be saved anymore.
    let pending: HashSet<String> = pr
        .comments
        .iter()
        .filter(|c| c.pending)
        .map(|c| edit_draft(&pr.summary.key, c.id))
        .collect();
    expect_context::<OpenBoxes>()
        .close_unless(&edit_prefix(&pr.summary.key), |d| pending.contains(d));
    let latest = pr.versions.len() as u32;
    let selection = {
        let pr = pr.clone();
        Memo::new(move |_| {
            let default = Selection {
                base: 0,
                head: latest,
            };
            match chosen.get() {
                // Selections that no longer make sense fall back to the default.
                Some(s)
                    if s.head >= 1
                        && s.head <= latest
                        && s.base < s.head
                        && !pr.versions[s.head as usize - 1].missing
                        && (s.base == 0 || !pr.versions[s.base as usize - 1].missing) =>
                {
                    s
                }
                _ => default,
            }
        })
    };
    // Only when both sides sit on different bases does a rebase bring in
    // upstream changes.
    let upstream = {
        let pr = pr.clone();
        Memo::new(move |_| {
            if pr.versions.is_empty() {
                return None;
            }
            let sel = selection.get();
            let head = &pr.versions[sel.head as usize - 1];
            (sel.base != 0)
                .then(|| {
                    (
                        pr.versions[sel.base as usize - 1].merge_base.clone(),
                        head.merge_base.clone(),
                    )
                })
                .and_then(|(a, b)| Some((a?, b?)))
                .filter(|(a, b)| a != b)
        })
    };
    let s = &pr.summary;
    let review = pr.review.as_ref();
    let reviewed = last_reviewed(&pr.versions, review);
    let body_html = if pr.body.trim().is_empty() {
        "<p class=\"muted\">No description provided.</p>".to_owned()
    } else {
        markdown::to_html(&pr.body, &s.key.repo)
    };

    let diff = {
        let pr = pr.clone();
        move || {
            if pr.versions.is_empty() {
                return view! { <p class="empty">"This PR has no versions yet."</p> }.into_any();
            }
            let sel = selection.get();
            let head = &pr.versions[sel.head as usize - 1];
            let left = if sel.base == 0 {
                head.merge_base.clone()
            } else {
                Some(pr.versions[sel.base as usize - 1].sha.clone())
            };
            let Some(left) = left else {
                return view! { <p class="empty error">"The merge-base of this version is unknown."</p> }.into_any();
            };
            let upstream = upstream.get();
            let (inline, panel) = place(&pr.comments, &pr.versions, sel);
            // GitHub anchors new comments to the PR's diff from the merge
            // base to the head, and refuses lines outside it. Against an
            // earlier version, a line can be changed in the diff shown but
            // not in GitHub's, e.g. one the latest push changed back.
            let commenting = (pr.review.is_some()
                && pr.summary.state == "OPEN"
                && sel.base == 0
                && sel.head == latest)
                .then(|| Commenting {
                    key: pr.summary.key.clone(),
                });
            view! {
                <CommentPanel threads=panel chosen=chosen selection=selection />
                <DiffView
                    repo=pr.summary.key.repo.clone()
                    left=left
                    right=head.sha.clone()
                    upstream=upstream
                    comments=Arc::new(inline)
                    commenting=commenting
                    side_by_side=side_by_side.into()
                    inline_changes=inline_changes.into()
                    rebase_older=rebase_older.into()
                />
            }
            .into_any()
        }
    };

    // File headers stick just below the toolbar, so its height is kept in
    // a CSS variable. Wrapping and the review panel change that height. A
    // sync rebuilds PrBody and the old instance's cleanup can run after
    // the new one has set the variable, so it lives on the document root
    // and is never removed; the CSS only reads it inside `.pr`.
    let toolbar = NodeRef::<leptos::html::Div>::new();
    let watching = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn FnMut()>)>);
    Effect::new(move |_| {
        let Some(toolbar) = toolbar.get() else {
            return;
        };
        if watching.with_value(|w| w.is_some()) {
            return;
        }
        let el = toolbar.clone();
        let root = || {
            document()
                .document_element()
                .map(|e| e.unchecked_into::<web_sys::HtmlElement>())
        };
        let update = move || {
            // A remounted PrBody's observer can still fire on its
            // detached toolbar, which reports a height of 0.
            if !el.is_connected() {
                return;
            }
            if let Some(root) = root() {
                let _ = root
                    .style()
                    .set_property("--toolbar-height", &format!("{}px", el.offset_height()));
            }
        };
        // The observer's first callback is asynchronous; without this
        // the variable stays unset until it fires.
        update();
        let on_resize = Closure::<dyn FnMut()>::new(update);
        if let Ok(observer) = web_sys::ResizeObserver::new(on_resize.as_ref().unchecked_ref()) {
            observer.observe(&toolbar);
            watching.set_value(Some((observer, on_resize)));
        }
    });
    on_cleanup(move || {
        watching.try_with_value(|w| {
            if let Some((observer, _)) = w {
                observer.disconnect();
            }
        });
    });

    view! {
        <article class="pr">
            <header class="pr-header">
                <h1>{s.title.clone()} " " <span class="muted pr-number">{format!("#{}", s.key.number)}</span><GithubLink url=s.url.clone() /></h1>
                <p class="muted">
                    <span class=format!("state state-{}", state_label(&s.state, s.queued))>{state_label(&s.state, s.queued)}</span>
                    {s.is_draft.then(|| view! { <span class="badge">"draft"</span> })}
                    {format!(" {} wants to merge {} into {} in {}", s.author, pr.head_ref, pr.base_ref, s.key.repo)}
                    " · " <a href=format!("#/agents/new/{}/{}", s.key.repo, s.key.number)>"Start an agent on this PR"</a>
                </p>
                {pr.approximate.then(|| view! {
                    <p class="banner warn">"GitHub has no push log for this PR, so its versions were reconstructed from force-push events and may not match every push."</p>
                })}
                {pr.drift.clone().map(|d| view! { <p class="banner warn">{d}</p> })}
                {pr.sync_error.clone().map(|e| view! { <p class="banner error">{format!("The last sync failed: {e}")}</p> })}
            </header>
            {pr.stack.clone().filter(|st| st.prs.len() > 1).map(|st| view! {
                <StackList stack=st repo=s.key.repo.clone() current=s.key.number />
            })}
            <section class="description markdown" inner_html=body_html></section>
            {review.map(conversation)}
            {review.filter(|_| s.state == "OPEN").map(|info| merge_box(info, merge_target(&pr)))}
            {merge_warning()}
            <div class="toolbar" node_ref=toolbar>
                <VersionPicker versions=pr.versions.clone() comments=pr.comments.clone() reviewed=reviewed chosen=chosen selection=selection />
                {pr.review.as_ref().filter(|_| s.state == "OPEN").map(|info| view! {
                    <ReviewPanel
                        pending=pr.comments.iter().filter(|c| c.pending).count()
                        own=info.viewer == s.author
                    />
                })}
                <label class="toggle">
                    <input
                        type="checkbox"
                        prop:checked=move || side_by_side.get()
                        on:change=move |ev| {
                            let on = event_target_checked(&ev);
                            side_by_side.set(on);
                            save_flag("side-by-side", on);
                        }
                    />
                    " Side by side"
                </label>
                <label class="toggle">
                    <input
                        type="checkbox"
                        prop:checked=move || inline_changes.get()
                        on:change=move |ev| {
                            let on = event_target_checked(&ev);
                            inline_changes.set(on);
                            save_flag("inline-changes", on);
                        }
                    />
                    " Inline changes"
                </label>
                <label
                    class="toggle"
                    title=move || if upstream.get().is_some() {
                        "Show the older version's changes replayed onto the newer version's base."
                    } else {
                        "Both sides have the same base, so there's nothing to rebase."
                    }
                >
                    <input
                        type="checkbox"
                        prop:disabled=move || upstream.get().is_none()
                        prop:checked=move || rebase_older.get()
                        on:change=move |ev| {
                            let on = event_target_checked(&ev);
                            rebase_older.set(on);
                            save_flag("rebase-older-version", on);
                        }
                    />
                    " Rebase older version"
                </label>
            </div>
            {diff}
        </article>
    }
}

/// The badge text and class of a PR's state: an open PR in a merge queue
/// shows as queued, since GitHub has no QUEUED state.
fn state_label(state: &str, queued: bool) -> String {
    match state == "OPEN" && queued {
        true => "queued".to_owned(),
        false => state.to_lowercase(),
    }
}

fn merge_target(pr: &PrDetail) -> MergeTarget {
    let n = pr.summary.key.number;
    let stack = pr
        .stack
        .as_ref()
        .map(|s| s.prs.as_slice())
        .unwrap_or_default();
    let open = |number: u64| {
        stack
            .iter()
            .any(|p| p.number == number && p.state == "OPEN")
    };
    MergeTarget {
        title: pr.summary.title.clone(),
        body: pr.body.clone(),
        is_draft: pr.summary.is_draft,
        head_oid: pr.versions.last().map(|v| v.sha.clone()),
        base_ref: pr.base_ref.clone(),
        open_parent: stack
            .iter()
            .find(|p| p.number == n)
            .and_then(|p| p.parent)
            .filter(|&p| open(p)),
        stack_base: pr
            .stack
            .as_ref()
            .map_or_else(|| pr.base_ref.clone(), |s| s.base_ref.clone()),
        has_children: stack
            .iter()
            .any(|p| p.parent == Some(n) && p.state == "OPEN"),
        unresolved_threads: threads(&pr.comments)
            .iter()
            .filter(|t| t.root.thread_id.is_some() && !t.root.pending && !t.root.resolved)
            .count(),
    }
}

/// The horizontal centre of a lane in the stack graph, in pixels.
fn lane_x(lane: usize) -> usize {
    8 + lane * 16
}

/// The PR's stack, top first, drawn like `git log --graph`.
#[component]
fn StackList(stack: Stack, repo: String, current: u64) -> impl IntoView {
    let rows = stack.graph();
    let lanes = rows
        .iter()
        .flat_map(|r| {
            std::iter::once(r.lane)
                .chain(r.through.iter().copied())
                .chain(r.joins.iter().copied())
        })
        .max()
        .unwrap_or(0)
        + 1;
    let width = format!("width: {}px", lane_x(lanes) - 4);
    let more = |text: &'static str| {
        let width = width.clone();
        view! {
            <div class="stack-row">
                <span class="stack-graph" style=width></span>
                <span class="muted">{text}</span>
            </div>
        }
    };
    let rows = rows.into_iter().map(|row| {
        let at = |lane: usize| format!("left: {}px", lane_x(lane));
        let graph = view! {
            <span class="stack-graph" style=width.clone()>
                {row.through.iter().map(|&l| view! { <span class="lane-line" style=at(l)></span> }).collect_view()}
                {row.joins.iter().map(|&l| view! {
                    <span class="lane-join" style=format!("left: {}px; width: {}px", lane_x(row.lane), lane_x(l) - lane_x(row.lane) + 2)></span>
                }).collect_view()}
                {row.up.then(|| view! { <span class="lane-up" style=at(row.lane)></span> })}
                {row.down.then(|| view! { <span class="lane-down" style=at(row.lane)></span> })}
                <span class="lane-dot" class:base=row.pr.is_none() class:current=row.pr.is_some_and(|i| stack.prs[i].number == current) style=at(row.lane)></span>
            </span>
        };
        let label = match row.pr {
            None if stack.more_ancestors => view! { <span class="muted">"… more not shown"</span> }.into_any(),
            None => view! { <span class="mono muted">{stack.base_ref.clone()}</span> }.into_any(),
            Some(i) => {
                let p = &stack.prs[i];
                let text = format!("#{} {}", p.number, p.title);
                let hash = hash_for(&PrKey { repo: repo.clone(), number: p.number });
                let state = state_label(&p.state, p.queued);
                view! {
                    {if p.number == current {
                        view! { <strong>{text}</strong> }.into_any()
                    } else {
                        view! { <a href=hash>{text}</a><GithubLink url=p.url.clone() /> }.into_any()
                    }}
                    " "
                    <span class=format!("state state-{state}")>{state.clone()}</span>
                    {p.is_draft.then(|| view! { <span class="badge">"draft"</span> })}
                }
                .into_any()
            }
        };
        view! { <div class="stack-row">{graph}<span class="stack-label">{label}</span></div> }
    }).collect_view();

    view! {
        <section class="stack">
            {stack.more_descendants.then(|| more("… more not shown"))}
            {rows}
        </section>
    }
}

fn kind_label(v: &Version) -> &'static str {
    match v.kind {
        VersionKind::Initial => "opened",
        VersionKind::Push => "push",
        VersionKind::ForcePush => "force push",
    }
}

/// A person's avatar and name. `guess` marks a pusher that is only the
/// commit's author.
#[component]
pub(crate) fn PersonChip(person: Person, guess: bool) -> impl IntoView {
    let label = person
        .name
        .clone()
        .or_else(|| person.login.clone())
        .unwrap_or_default();
    let title = if guess {
        format!("{label} authored this commit; who pushed it is unknown")
    } else {
        person
            .login
            .clone()
            .filter(|l| Some(l) != person.name.as_ref())
            .map(|l| format!("{label} ({l})"))
            .unwrap_or_else(|| label.clone())
    };
    view! {
        <span class="person" class:guess=guess title=title>
            {match person.avatar_url {
                Some(url) => view! { <img class="avatar" src=url alt="" /> }.into_any(),
                None => view! { <span class="avatar avatar-generic"></span> }.into_any(),
            }}
            <span>{label}</span>
        </span>
    }
}

/// The `YYYY-MM-DDTHH:MM:SS` prefix of a timestamp, or `None` when the
/// string is shorter. `Version::pushed_at` and `Review::submitted_at` are
/// both UTC, so the prefixes compare chronologically despite their
/// different zone suffixes.
fn utc_prefix(t: &str) -> Option<&str> {
    t.get(..19)
}

/// The number of the latest version the viewer reviewed. The version
/// whose SHA is the review's commit wins when it was pushed at or before
/// the review. When the commit points at a version pushed after the
/// review, the newest version pushed before the review wins instead.
fn last_reviewed(versions: &[Version], review: Option<&ReviewInfo>) -> Option<u32> {
    let info = review?;
    let mut mine: Vec<&Review> = info
        .reviews
        .iter()
        .filter(|r| r.author.login.as_deref() == Some(info.viewer.as_str()))
        .collect();
    // RFC 3339 strings sort chronologically.
    mine.sort_by(|a, b| b.submitted_at.cmp(&a.submitted_at));
    for r in mine {
        let Some(commit) = r.commit.as_deref() else {
            continue;
        };
        let Some(v) = versions.iter().find(|v| v.sha == commit) else {
            continue;
        };
        let submitted = utc_prefix(&r.submitted_at);
        let pushed = v.pushed_at.as_deref().and_then(utc_prefix);
        match (pushed, submitted) {
            (Some(p), Some(s)) if p > s => {
                let fallback = versions
                    .iter()
                    .filter(|v| {
                        v.pushed_at
                            .as_deref()
                            .and_then(utc_prefix)
                            .is_some_and(|p| p <= s)
                    })
                    .max_by_key(|v| v.number);
                if let Some(v) = fallback {
                    return Some(v.number);
                }
            }
            _ => return Some(v.number),
        }
    }
    None
}

#[component]
fn VersionPicker(
    versions: Vec<Version>,
    comments: Vec<ReviewComment>,
    reviewed: Option<u32>,
    chosen: RwSignal<Option<Selection>>,
    selection: Memo<Selection>,
) -> impl IntoView {
    let latest = versions.len() as u32;
    let counts = thread_counts(&comments, &versions);
    let versions = Arc::new(versions);

    // Clicks outside the picker and Escape close it.
    let details = NodeRef::<leptos::html::Details>::new();
    let close = move || {
        if let Some(el) = details.get_untracked() {
            let _ = el.remove_attribute("open");
        }
    };
    let on_click = window_event_listener(leptos::ev::click, move |ev| {
        let inside = details.get_untracked().is_some_and(|el| {
            ev.target()
                .and_then(|t| t.dyn_into::<web_sys::Node>().ok())
                .is_some_and(|t| el.contains(Some(&t)))
        });
        if !inside {
            close();
        }
    });
    let on_key = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" {
            close();
        }
    });
    on_cleanup(move || {
        on_click.remove();
        on_key.remove();
    });
    let missing = {
        let versions = versions.clone();
        move |n: u32| n > 0 && versions[n as usize - 1].missing
    };
    let label = {
        let versions = versions.clone();
        move || {
            let s = selection.get();
            let name = |n: u32| {
                if n == 0 {
                    "Base".to_owned()
                } else {
                    format!("v{n} ({})", short(&versions[n as usize - 1].sha))
                }
            };
            if latest == 0 {
                "No versions".to_owned()
            } else {
                format!("{} → {}", name(s.base), name(s.head))
            }
        }
    };
    let set_base = move |n: u32| {
        let s = selection.get_untracked();
        // Moving Base past Head pulls Head to the latest version.
        let head = if n >= s.head { latest } else { s.head };
        chosen.set(Some(Selection { base: n, head }));
    };
    let set_head = move |n: u32| {
        let s = selection.get_untracked();
        let base = if n <= s.base { 0 } else { s.base };
        chosen.set(Some(Selection { base, head: n }));
    };

    let rows = (0..=latest)
        .map(|n| {
            let v = (n > 0).then(|| versions[n as usize - 1].clone());
            let base_disabled = n == latest || missing(n);
            let head_disabled = n == 0 || missing(n);
            let pusher = v.as_ref().and_then(|v| {
                let guess = v.pushed_by_is_guess;
                v.pushed_by
                    .clone()
                    .map(|person| view! { <PersonChip person=person guess=guess /> })
            });
            let threads = counts.get(&n).map(|&(resolved, total)| {
                view! {
                    <span class:all-resolved=resolved == total
                        title=format!("{resolved} of {total} threads resolved")>
                        {format!("{resolved}/{total}")}
                    </span>
                }
            });
            view! {
                <tr class:missing=v.as_ref().is_some_and(|v| v.missing)
                    class:reviewed=reviewed == Some(n)>
                    <td class="radio">
                        <input type="radio" name="base" disabled=base_disabled
                            prop:checked=move || selection.get().base == n
                            on:change=move |_| set_base(n) />
                    </td>
                    <td class="radio">
                        <input type="radio" name="head" disabled=head_disabled
                            prop:checked=move || selection.get().head == n
                            on:change=move |_| set_head(n) />
                    </td>
                    <td class="version">
                        {match &v {
                            None => view! { <span>"Base"</span> }.into_any(),
                            Some(v) => view! {
                                <span class="vnum">{format!("{n}")}</span>
                                " "
                                <code title=v.sha.clone()>{short(&v.sha).to_owned()}</code>
                                <span class="muted vkind">{kind_label(v)}</span>
                                {v.pushed_at.clone().map(|t| view! { <span class="muted vtime">{t.replace('T', " ").trim_end_matches("+00:00").to_owned()}</span> })}
                                {(reviewed == Some(n)).then(|| view! { <span class="vreviewed" title="The latest version you reviewed">"last reviewed"</span> })}
                                {v.missing.then(|| view! { <span class="error">"missing"</span> })}
                            }.into_any(),
                        }}
                    </td>
                    <td class="pushed-by">{pusher}</td>
                    <td class="threads">{threads}</td>
                </tr>
            }
        })
        .collect_view();

    view! {
        <details class="picker" node_ref=details>
            <summary>{label}</summary>
            <table class="versions">
                <thead><tr><th>"Base"</th><th>"Head"</th><th>"Version"</th><th>"Pushed by"</th><th>"Threads"</th></tr></thead>
                <tbody>{rows}</tbody>
            </table>
        </details>
    }
}

/// A comment's RFC 3339 time in UTC as shown, e.g. "2026-01-02 03:04:05".
fn comment_time(ts: &str) -> String {
    ts.replace('T', " ").trim_end_matches('Z').to_owned()
}

/// A comment: its author (the children), its time linked to the comment
/// on GitHub, and its body rendered like the description, if it has one.
/// `pending` marks a comment of the viewer's pending review, and `actions`
/// are shown after its time. While `editor`'s signal is on, its view is
/// shown instead of the body.
#[component]
pub(crate) fn CommentFrame(
    at: String,
    url: String,
    body: String,
    children: Children,
    #[prop(optional)] pending: bool,
    #[prop(optional)] actions: Option<AnyView>,
    #[prop(optional)] editor: Option<(Signal<bool>, ViewFn)>,
) -> impl IntoView {
    let links = expect_context::<PrLinks>();
    let html = (!body.trim().is_empty()).then(|| markdown::to_html(&body, &links.repo));
    let body = move || match editor.as_ref().filter(|(on, _)| on.get()) {
        Some((_, view)) => Some(view.run()),
        None => html.clone().map(|html| {
            view! {
                <div class="markdown" inner_html=html></div>
            }
            .into_any()
        }),
    };
    view! {
        <div class="comment" class:pending=pending>
            <div class="comment-meta">
                {children()}
                " "
                <a class="muted" href=url target="_blank" rel="noopener">{comment_time(&at)}</a>
                {actions}
            </div>
            {body}
        </div>
    }
}

/// What the names of the drafts that edit pending comments on `key`
/// start with.
fn edit_prefix(key: &PrKey) -> String {
    format!("edit.{}#{}.", key.repo, key.number)
}

/// The name of the draft that edits the pending comment `id` on `key`.
fn edit_draft(key: &PrKey, id: u64) -> String {
    format!("{}{id}", edit_prefix(key))
}

/// A review comment. A pending one has a badge, and Edit and Delete.
#[component]
pub fn CommentBody(comment: ReviewComment) -> impl IntoView {
    let pending = comment
        .node_id
        .clone()
        .filter(|_| comment.pending)
        .zip(use_context::<ReviewTarget>());
    let Some((node, target)) = pending else {
        return view! {
            <CommentFrame at=comment.created_at url=comment.url body=comment.body>
                <strong>{comment.author}</strong>
            </CommentFrame>
        }
        .into_any();
    };
    let links = expect_context::<PrLinks>();
    // Open while the comment is being edited, in the same set as the boxes
    // for new comments so that a refetch doesn't close it.
    let boxes = expect_context::<OpenBoxes>();
    let draft = edit_draft(&target.key, comment.id);
    let editing = {
        let draft = draft.clone();
        Signal::derive(move || boxes.is_open(&draft))
    };
    let editor = {
        let (draft, body, key, node) = (
            draft.clone(),
            comment.body.clone(),
            target.key.clone(),
            node.clone(),
        );
        ViewFn::from(move || {
            let (key, node) = (key.clone(), node.clone());
            let send = sender(move |body| {
                let (key, node) = (key.clone(), node.clone());
                async move { api::edit_review_comment(&key, &node, body).await }
            });
            view! {
                <Composer
                    draft=draft.clone()
                    repo=links.repo.clone()
                    label="Save"
                    placeholder="Edit the comment…"
                    send=send
                    initial=body.clone()
                    autofocus=true
                />
            }
        })
    };
    // Named like the edit's draft, so that each disables the other.
    let deleting = format!("delete.{draft}");
    let busy = {
        let (draft, deleting) = (draft.clone(), deleting.clone());
        Signal::derive(move || boxes.sending(&draft) || boxes.sending(&deleting))
    };
    let error = {
        let deleting = deleting.clone();
        Signal::derive(move || boxes.error(&deleting))
    };
    let delete = move |_| {
        if !window()
            .confirm_with_message("Delete this pending comment?")
            .unwrap_or(false)
        {
            return;
        }
        let (key, node) = (target.key.clone(), node.clone());
        // On success the page is rebuilt once the PR has synced.
        boxes.send(
            &deleting,
            async move { api::delete_review_comment(&key, &node).await },
            || {},
        );
    };
    let actions = view! {
        <span class="comment-actions">
            <button type="button" class="link" disabled=move || busy.get()
                on:click=move |_| boxes.set(&draft, true)>"Edit"</button>
            <button type="button" class="link" disabled=move || busy.get() on:click=delete>"Delete"</button>
            {move || error.get().map(|e| view! { <span class="error">{e}</span> })}
        </span>
    }
    .into_any();
    view! {
        <CommentFrame at=comment.created_at url=comment.url body=comment.body pending=true
            actions=actions editor=(editing, editor)>
            <strong>{comment.author}</strong>
            " " <span class="badge pending-badge">"Pending"</span>
        </CommentFrame>
    }
    .into_any()
}

/// A thread under a header that folds and unfolds it. Resolved threads
/// start out folded.
#[component]
pub fn ThreadView(thread: Arc<Thread>) -> impl IntoView {
    let Folded(folded) = expect_context::<Folded>();
    let root = &thread.root;
    let (id, resolved) = (root.id, root.resolved);
    let collapsed = move || folded.with(|m| m.get(&id).copied().unwrap_or(resolved));
    let count = 1 + thread.replies.len();
    let snippet = root
        .body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned();
    let status = resolved.then(|| match root.resolved_by.clone() {
        Some(person) => view! {
            <span class="thread-resolved">"Resolved by " <PersonChip person=person guess=false /></span>
        }
        .into_any(),
        None => view! { <span class="thread-resolved">"Resolved"</span> }.into_any(),
    });
    let comments = {
        let thread = thread.clone();
        move || {
            (!collapsed()).then(|| {
                view! {
                    <CommentBody comment=thread.root.clone() />
                    {thread.replies.iter().map(|r| view! { <CommentBody comment=r.clone() /> }).collect_view()}
                    <ThreadActions root=thread.root.clone() />
                }
            })
        }
    };
    view! {
        <div class="thread" class:resolved=resolved class:collapsed=collapsed>
            <button
                type="button"
                class="thread-header"
                aria-expanded=move || (!collapsed()).to_string()
                on:click=move |_| {
                    let fold = !collapsed();
                    folded.update(|m| {
                        m.insert(id, fold);
                    });
                }
            >
                <span class="thread-toggle">{move || if collapsed() { "▸" } else { "▾" }}</span>
                <strong>{root.author.clone()}</strong>
                <span class="muted">{if count == 1 { "1 comment".to_owned() } else { format!("{count} comments") }}</span>
                {move || collapsed().then(|| view! { <span class="muted thread-snippet">{snippet.clone()}</span> })}
                {status}
            </button>
            {comments}
        </div>
    }
}

/// Replying to a thread and resolving it, under its comments. Not for a
/// thread of the viewer's pending review, which isn't on the PR yet.
#[component]
fn ThreadActions(root: ReviewComment) -> impl IntoView {
    let (Some(target), Some(thread)) = (use_context::<ReviewTarget>(), root.thread_id) else {
        return ().into_any();
    };
    if root.pending {
        return ().into_any();
    }
    let key = target.key;
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let resolved = root.resolved;
    let toggle = {
        let (key, thread) = (key.clone(), thread.clone());
        move |_| {
            let (key, thread) = (key.clone(), thread.clone());
            busy.set(true);
            error.set(None);
            leptos::task::spawn_local(async move {
                // On success the page is rebuilt once the PR has synced,
                // and until then the button stays busy. This component
                // may be gone by the time the answer comes.
                if let Err(e) = api::set_thread_resolved(&key, &thread, !resolved).await {
                    error.try_set(Some(e));
                    busy.try_set(false);
                }
            });
        }
    };
    let reply = |review: bool| -> SendComment {
        let (key, thread) = (key.clone(), thread.clone());
        sender(move |body| {
            let (key, thread) = (key.clone(), thread.clone());
            async move { api::reply_to_thread(&key, &thread, body, review).await }
        })
    };
    // While a review is pending, replies go into it unless asked otherwise.
    let (label, send, alt) = if target.reviewing {
        (
            "Add to review",
            reply(true),
            ("Reply now".to_owned(), reply(false)),
        )
    } else {
        (
            "Reply",
            reply(false),
            ("Add to review".to_owned(), reply(true)),
        )
    };
    let can_resolve = root.can_resolve;
    view! {
        <div class="thread-actions">
            <ComposerToggle draft=format!("reply.{thread}") label="Reply" placeholder="Reply…"
                send_label=label send=send alt=alt>
                {can_resolve.then(|| view! {
                    <button type="button" disabled=move || busy.get() on:click=toggle.clone()>
                        {if resolved { "Unresolve" } else { "Resolve" }}
                    </button>
                })}
            </ComposerToggle>
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
        </div>
    }
    .into_any()
}

#[component]
fn CommentPanel(
    threads: Vec<Unplaced>,
    chosen: RwSignal<Option<Selection>>,
    selection: Memo<Selection>,
) -> impl IntoView {
    if threads.is_empty() {
        return ().into_any();
    }
    let count = threads.len();
    let items = threads
        .into_iter()
        .map(|u| {
            let c = &u.thread.root;
            let line = c.original_line.map(|l| format!(":{l}")).unwrap_or_default();
            let jump = u.version.map(|v| {
                view! {
                    <button
                        class="link"
                        on:click=move |_| {
                            let s = selection.get_untracked();
                            let base = if s.base < v { s.base } else { 0 };
                            chosen.set(Some(Selection { base, head: v }));
                        }
                    >
                        {format!("on v{v}")}
                    </button>
                }
            });
            view! {
                <li>
                    <div class="panel-where"><code>{format!("{}{line}", c.path)}</code> " " {jump}
                        {u.version.is_none().then(|| view! { <span class="muted">"on an unknown version"</span> })}
                    </div>
                    <ThreadView thread=u.thread.clone() />
                </li>
            }
        })
        .collect_view();
    view! {
        <details class="comment-panel">
            <summary>{format!("{count} comment threads on other versions")}</summary>
            <ul>{items}</ul>
        </details>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{MergeInfo, MergeState, ReviewState};

    use super::*;

    fn version(n: u32, sha: &str) -> Version {
        version_pushed(n, sha, None)
    }

    fn version_pushed(n: u32, sha: &str, pushed_at: Option<&str>) -> Version {
        Version {
            number: n,
            sha: sha.into(),
            merge_base: Some("mb".into()),
            kind: VersionKind::Push,
            pushed_at: pushed_at.map(str::to_owned),
            pushed_by: None,
            pushed_by_is_guess: false,
            missing: false,
        }
    }

    fn comment(
        id: u64,
        commit: &str,
        side: Side,
        original_line: u32,
        line: Option<u32>,
    ) -> ReviewComment {
        ReviewComment {
            id,
            in_reply_to: None,
            author: "a".into(),
            body: "b".into(),
            path: "f.rs".into(),
            original_commit: commit.into(),
            original_line: Some(original_line),
            original_start_line: None,
            line,
            start_line: None,
            side,
            on_file: false,
            created_at: String::new(),
            url: String::new(),
            resolved: false,
            resolved_by: None,
            thread_id: None,
            can_resolve: false,
            pending: false,
            node_id: None,
        }
    }

    fn review(login: &str, at: &str, commit: Option<&str>) -> Review {
        Review {
            author: Person {
                login: Some(login.into()),
                name: None,
                avatar_url: None,
            },
            state: ReviewState::Commented,
            body: String::new(),
            submitted_at: at.into(),
            url: String::new(),
            commit: commit.map(str::to_owned),
        }
    }

    fn review_info(reviews: Vec<Review>) -> ReviewInfo {
        ReviewInfo {
            viewer: "me".into(),
            decision: None,
            reviews,
            requested: vec![],
            comments: vec![],
            checks: vec![],
            ci: None,
            merge: MergeInfo {
                conflicting: Some(false),
                state: MergeState::Clean,
                methods: vec![],
                default_method: None,
                delete_branch_on_merge: false,
                can_merge_as_admin: false,
                requires_conversation_resolution: false,
                queue: None,
            },
            pending_review: None,
        }
    }

    fn anchors(inline: &HashMap<String, Vec<Placed>>) -> Vec<(u64, Anchor)> {
        let mut out: Vec<_> = inline
            .values()
            .flatten()
            .map(|p| (p.thread.root.id, p.anchor))
            .collect();
        out.sort_by_key(|(id, _)| *id);
        out
    }

    #[test]
    fn placement() {
        let versions = [version(1, "a"), version(2, "b"), version(3, "c")];
        let comments = [
            comment(1, "a", Side::Right, 10, Some(12)),
            comment(2, "b", Side::Right, 20, None),
            comment(3, "c", Side::Left, 5, Some(5)),
        ];

        // Base → latest: mapped forward, except the outdated one.
        let (inline, panel) = place(&comments, &versions, Selection { base: 0, head: 3 });
        assert_eq!(
            anchors(&inline),
            [
                (1, Anchor::Line(Side::Right, 12)),
                (3, Anchor::Line(Side::Left, 5))
            ]
        );
        assert_eq!(panel.len(), 1);
        assert_eq!(panel[0].version, Some(2));

        // v1 → v2: the comment made on v1 anchors the left, the one made
        // on v2 the right.
        let (inline, panel) = place(&comments, &versions, Selection { base: 1, head: 2 });
        assert_eq!(
            anchors(&inline),
            [
                (1, Anchor::Line(Side::Left, 10)),
                (2, Anchor::Line(Side::Right, 20))
            ]
        );
        assert_eq!(panel.len(), 1);

        // v2 → v3: v2's own thread anchors the left too.
        let (inline, _) = place(&comments, &versions, Selection { base: 2, head: 3 });
        assert_eq!(
            anchors(&inline),
            [
                (1, Anchor::Line(Side::Right, 12)),
                (2, Anchor::Line(Side::Left, 20))
            ]
        );
    }

    #[test]
    fn placement_on_the_left_version() {
        let versions = [version(1, "a"), version(2, "b"), version(3, "c")];
        let mut file_comment = comment(5, "a", Side::Right, 0, None);
        file_comment.on_file = true;
        let comments = [
            // Made on v1.
            comment(1, "a", Side::Right, 10, Some(12)),
            // A merge-base comment made on v1.
            comment(2, "a", Side::Left, 7, Some(7)),
            // Made on the version between the two diffed.
            comment(3, "b", Side::Right, 20, Some(21)),
            // Made on v3.
            comment(4, "c", Side::Right, 30, Some(30)),
            file_comment,
        ];

        let (inline, panel) = place(&comments, &versions, Selection { base: 1, head: 3 });
        assert_eq!(
            anchors(&inline),
            [
                // The thread made on v1 goes left once, not also right.
                (1, Anchor::Line(Side::Left, 10)),
                // The in-between version's thread maps forward to the
                // latest.
                (3, Anchor::Line(Side::Right, 21)),
                (4, Anchor::Line(Side::Right, 30)),
                (5, Anchor::File),
            ]
        );
        // The merge-base comment has no line on v1 itself.
        assert_eq!(panel.len(), 1);
        assert_eq!(panel[0].thread.root.id, 2);
        assert_eq!(panel[0].version, Some(1));
    }

    #[test]
    fn replies_join_their_thread() {
        let mut reply = comment(2, "a", Side::Right, 1, None);
        reply.in_reply_to = Some(1);
        let t = threads(&[comment(1, "a", Side::Right, 1, None), reply]);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].replies.len(), 1);
    }

    #[test]
    fn thread_counts_per_version() {
        let versions = [version(1, "a"), version(2, "b"), version(3, "c")];
        let resolved = |mut c: ReviewComment| {
            c.resolved = true;
            c
        };
        let mut reply = comment(5, "b", Side::Right, 1, None);
        reply.in_reply_to = Some(2);
        let comments = [
            resolved(comment(1, "a", Side::Right, 1, None)),
            comment(2, "a", Side::Right, 2, None),
            resolved(comment(3, "b", Side::Right, 1, None)),
            // Not a version.
            comment(4, "x", Side::Right, 1, None),
            // A reply doesn't count as a thread of its own.
            reply,
        ];
        let counts = thread_counts(&comments, &versions);
        assert_eq!(counts, HashMap::from([(1, (1, 2)), (2, (1, 1))]));
    }

    #[test]
    fn last_reviewed_picks_the_viewers_latest_review() {
        let versions = [version(1, "a"), version(2, "b"), version(3, "c")];
        let info = review_info(vec![
            review("me", "2026-01-01T00:00:00Z", Some("a")),
            review("other", "2026-01-02T00:00:00Z", Some("c")),
            review("me", "2026-01-03T00:00:00Z", Some("b")),
        ]);
        assert_eq!(last_reviewed(&versions, Some(&info)), Some(2));
    }

    #[test]
    fn last_reviewed_falls_back_past_a_nonversion_commit() {
        let versions = [version(1, "a"), version(2, "b")];
        let info = review_info(vec![
            review("me", "2026-01-01T00:00:00Z", Some("a")),
            // An approximate history folded this commit away.
            review("me", "2026-01-02T00:00:00Z", Some("x")),
        ]);
        assert_eq!(last_reviewed(&versions, Some(&info)), Some(1));
    }

    #[test]
    fn last_reviewed_is_none_without_a_matching_review() {
        let versions = [version(1, "a")];
        assert_eq!(last_reviewed(&versions, None), None);
        assert_eq!(last_reviewed(&versions, Some(&review_info(vec![]))), None);
        let info = review_info(vec![
            review("other", "2026-01-01T00:00:00Z", Some("a")),
            review("me", "2026-01-02T00:00:00Z", None),
        ]);
        assert_eq!(last_reviewed(&versions, Some(&info)), None);
    }

    #[test]
    fn last_reviewed_ignores_a_commit_pushed_after_the_review() {
        let versions = [
            version_pushed(1, "a", Some("2026-01-01T10:00:00+00:00")),
            version_pushed(2, "b", Some("2026-01-02T10:00:00+00:00")),
            version_pushed(3, "c", Some("2026-01-03T10:00:00+00:00")),
        ];
        let info = review_info(vec![review("me", "2026-01-02T12:00:00Z", Some("c"))]);
        assert_eq!(last_reviewed(&versions, Some(&info)), Some(2));
    }

    #[test]
    fn last_reviewed_counts_a_version_pushed_at_the_same_second() {
        let versions = [
            version_pushed(1, "a", Some("2026-01-01T10:00:00+00:00")),
            version_pushed(2, "b", Some("2026-01-02T10:00:00+00:00")),
        ];
        let info = review_info(vec![review("me", "2026-01-02T10:00:00Z", Some("b"))]);
        assert_eq!(last_reviewed(&versions, Some(&info)), Some(2));
    }

    #[test]
    fn last_reviewed_trusts_the_commit_without_a_push_time() {
        let versions = [version(1, "a"), version(2, "b")];
        let info = review_info(vec![review("me", "2026-01-01T00:00:00Z", Some("b"))]);
        assert_eq!(last_reviewed(&versions, Some(&info)), Some(2));
    }

    #[test]
    fn queued_prs_show_as_queued() {
        assert_eq!(state_label("OPEN", true), "queued");
        assert_eq!(state_label("OPEN", false), "open");
        // A queued flag on a merged PR means nothing.
        assert_eq!(state_label("MERGED", true), "merged");
        assert_eq!(state_label("CLOSED", false), "closed");
    }

    #[test]
    fn pending_message_is_plain_without_a_pause() {
        let key = PrKey {
            repo: "o/r".into(),
            number: 7,
        };
        assert_eq!(pending_message(&key, None, 100), "o/r#7 hasn't synced yet");
    }

    #[test]
    fn pending_message_names_the_reset_while_paused() {
        let key = PrKey {
            repo: "o/r".into(),
            number: 7,
        };
        // 14:39 UTC, how the non-wasm `hm` renders it.
        let t = 14 * 3600 + 39 * 60;
        assert_eq!(
            pending_message(&key, Some(t), t - 1),
            "o/r#7 hasn't synced yet. It may be waiting for GitHub's rate limit to reset at 14:39."
        );
    }

    #[test]
    fn pending_message_is_plain_once_the_pause_passed() {
        let key = PrKey {
            repo: "o/r".into(),
            number: 7,
        };
        let t = 14 * 3600 + 39 * 60;
        assert_eq!(pending_message(&key, Some(t), t), "o/r#7 hasn't synced yet");
        assert_eq!(
            pending_message(&key, Some(t), t + 1),
            "o/r#7 hasn't synced yet"
        );
    }

    #[test]
    fn sidebar_width_defaults_to_the_split() {
        assert_eq!(sidebar_width(None, 2400.0, true), 800.0);
        assert_eq!(sidebar_width(None, 1280.0, false), 320.0);
    }

    #[test]
    fn sidebar_width_clamps_the_column() {
        assert_eq!(sidebar_width(Some(100.0), 2400.0, true), 320.0);
        assert_eq!(sidebar_width(Some(2000.0), 2400.0, true), 1220.0);
        // A window too narrow for the saved width clamps it rather than
        // dropping it.
        assert_eq!(sidebar_width(Some(1000.0), 1600.0, true), 420.0);
    }

    #[test]
    fn sidebar_width_clamps_the_overlay() {
        assert_eq!(sidebar_width(Some(5000.0), 1280.0, false), 1232.0);
        // The minimum wins even on a page narrower than it.
        assert_eq!(sidebar_width(None, 300.0, false), 320.0);
    }
}
