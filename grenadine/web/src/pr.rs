//! One PR: its title and description, the version picker, the review
//! comments that don't fit into the diff, and the diff.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use grenadine_core::api::{
    EditPr, EditText, Person, PrDetail, PrKey, Review, ReviewComment, ReviewInfo, Side, Stack,
    Version, VersionKind,
};
use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};

use crate::composer::{
    Composer, ComposerToggle, OpenBoxes, SendComment, comment_draft_key, draft_signal_from, sender,
};
use crate::diffview::Commenting;
use crate::diffview::DiffView;
use crate::inboxes::{load_flag, save_flag, storage};
use crate::review::{
    CheckFolds, MergeChoice, MergeTarget, PrSidebar, ReviewChoice, ReviewPanel, ReviewerEdit,
    conversation, merge_box, merge_warning,
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
    /// The first line of a multi-line thread's range, in the anchor's
    /// coordinates; `None` for a single-line thread.
    pub start: Option<(Side, u32)>,
    /// Set when the anchor is the fallback: the thread can't sit on its
    /// lines in the selected diff, so it shows on its file instead.
    pub fallback: Option<Fallback>,
    pub thread: Arc<Thread>,
}

/// What a fallback thread's header needs: the version it was made on and
/// whether GitHub calls it outdated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fallback {
    pub version: Option<u32>,
    /// GitHub has no line for the thread on the PR's head: a later push
    /// changed the lines it was made on.
    pub outdated: bool,
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
/// - on its file, when the diff's files include it (`diff_paths`; `None`
///   while the diff hasn't loaded);
/// - otherwise in the side panel.
///
/// Left-side comments were made against the PR's base at the time, so they
/// only go inline when the diff's left side is Head's merge-base.
pub fn place(
    comments: &[ReviewComment],
    versions: &[Version],
    sel: Selection,
    diff_paths: Option<&HashSet<String>>,
) -> (HashMap<String, Vec<Placed>>, Vec<Unplaced>) {
    let mut inline: HashMap<String, Vec<Placed>> = HashMap::new();
    let mut panel = Vec::new();
    let head = &versions[sel.head as usize - 1];
    let is_latest = sel.head as usize == versions.len();
    for thread in threads(comments) {
        let c = &thread.root;
        let (anchor, start) =
            if sel.base != 0 && c.original_commit == versions[sel.base as usize - 1].sha {
                // Made on the version the diff's left side shows: its
                // right-side lines are that version's own. A range starting
                // on the right lands there too; one starting on the left is
                // in the merge-base's coordinates, which this view doesn't
                // show.
                let anchor = if c.on_file {
                    Some(Anchor::File)
                } else {
                    match (c.original_line, c.side) {
                        (Some(l), Side::Right) => Some(Anchor::Line(Side::Left, l)),
                        _ => None,
                    }
                };
                let start = match (c.original_start_line, c.start_side.unwrap_or(c.side)) {
                    (Some(l), Side::Right) => Some((Side::Left, l)),
                    _ => None,
                };
                (anchor, start)
            } else {
                let line = if c.original_commit == head.sha {
                    c.original_line
                } else if is_latest {
                    c.line
                } else {
                    None
                };
                let anchor = if c.on_file && (c.original_commit == head.sha || is_latest) {
                    Some(Anchor::File)
                } else {
                    match (line, c.side) {
                        (Some(l), Side::Right) => Some(Anchor::Line(Side::Right, l)),
                        (Some(l), Side::Left) if sel.base == 0 => Some(Anchor::Line(Side::Left, l)),
                        _ => None,
                    }
                };
                let start_line = if c.original_commit == head.sha {
                    c.original_start_line
                } else {
                    c.start_line
                };
                let start = match (start_line, c.start_side.unwrap_or(c.side)) {
                    (Some(l), Side::Right) => Some((Side::Right, l)),
                    (Some(l), Side::Left) if sel.base == 0 => Some((Side::Left, l)),
                    _ => None,
                };
                (anchor, start)
            };
        // A range ending where it starts is a single line.
        let start = start.filter(|&s| anchor != Some(Anchor::Line(s.0, s.1)));
        let thread = Arc::new(thread);
        let version = || {
            versions
                .iter()
                .find(|v| v.sha == thread.root.original_commit)
                .map(|v| v.number)
        };
        match anchor {
            Some(anchor) => inline
                .entry(thread.root.path.clone())
                .or_default()
                .push(Placed {
                    anchor,
                    start,
                    fallback: None,
                    thread,
                }),
            None if diff_paths.is_some_and(|p| p.contains(&thread.root.path)) => inline
                .entry(thread.root.path.clone())
                .or_default()
                .push(Placed {
                    anchor: Anchor::File,
                    start: None,
                    fallback: Some(Fallback {
                        version: version(),
                        outdated: thread.root.line.is_none() && !thread.root.on_file,
                    }),
                    thread,
                }),
            None => panel.push(Unplaced {
                version: version(),
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

/// The label GitHub shows a multi-line thread, like "Lines +26 to +29";
/// the left side's lines read "-10". A range across the sides reads
/// "Lines -10 to +14".
pub fn range_label(start: (Side, u32), end: (Side, u32)) -> String {
    let line = |(side, l)| format!("{}{l}", if side == Side::Left { "-" } else { "+" });
    format!("Lines {} to {}", line(start), line(end))
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
    // Set by `PrBody` when it mounts.
    let pinned = RwSignal::new(0);
    let side_by_side = RwSignal::new(load_flag("side-by-side", true));
    let inline_changes = RwSignal::new(load_flag("inline-changes", true));
    let rebase_older = RwSignal::new(load_flag("rebase-older-version", false));
    let github = github_url(&key);
    // Kept here, outside what version changes rebuild.
    provide_context(Folded(RwSignal::new(HashMap::new())));
    provide_context(CheckFolds(RwSignal::new(HashMap::new())));
    provide_context(ReviewerEdit {
        picks: RwSignal::new(None),
        search: RwSignal::new(String::new()),
        optimistic: RwSignal::new(None),
        error: RwSignal::new(None),
    });
    provide_context(OpenBoxes::new());
    provide_context(ReviewChoice::new());
    provide_context(MergeChoice::new());
    // The page's clock, ticking so the relative times stay fresh.
    let now = RwSignal::new((js_sys::Date::now() / 1000.0) as i64);
    let clock = set_interval_with_handle(
        move || now.set((js_sys::Date::now() / 1000.0) as i64),
        Duration::from_secs(30),
    );
    on_cleanup(move || {
        if let Ok(h) = clock {
            h.clear();
        }
    });
    provide_context(Now(now));

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

    // The PR's detail once it's ready. A Memo, so a refetch that holds
    // the same data — most syncs change nothing a page shows — fires
    // nothing downstream.
    let detail = Memo::new(move |_| match pr.get() {
        Some(Ok(api::PrResponse::Ready(d))) => Some(Arc::new(*d)),
        _ => None,
    });
    // A refetch that leaves it true remounts nothing gated on it: the
    // main column's scroll and the agents panel's live event streams
    // both live under it.
    let ready = Memo::new(move |_| detail.get().is_some());
    Effect::new(move |_| {
        if let Some(d) = detail.get() {
            prefetch(&d);
        }
    });
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
    let review_key = key.clone();
    let auto_review_key = key.clone();
    // The viewer's own open PR, which gets an Auto-review section.
    let own_open = Memo::new(move |_| {
        detail.with(|d| {
            d.as_ref().is_some_and(|d| {
                d.summary.state == "OPEN"
                    && d.review
                        .as_ref()
                        .is_some_and(|r| r.viewer == d.summary.author)
            })
        })
    });
    // The PR's latest synced commit, the one a review would start from.
    let head = Memo::new(move |_| {
        detail.with(|d| {
            d.as_ref().and_then(|d| {
                d.versions
                    .iter()
                    .rev()
                    .find(|v| !v.missing)
                    .map(|v| v.sha.clone())
            })
        })
    });
    // The latest detail, for PrBody: while it stays mounted a refetch
    // that leaves the PR unready keeps the last one rather than a gap.
    let current = Memo::new(move |prev| {
        detail
            .get()
            .or_else(|| prev.cloned())
            .expect("PrBody only mounts once the PR is ready")
    });

    view! {
        // `.main` outlives the states the PR moves through; unmounting
        // it would lose the scroll position.
        <main class="main">
        {move || {
            if ready.get() {
                ().into_any()
            } else {
                match pr.get() {
                None => view! { <div class="empty">"Loading…"</div> }.into_any(),
                Some(Err(e)) => view! { <div class="empty error">{format!("Can't load {}#{}: {e}", key.repo, key.number)}</div> }.into_any(),
                Some(Ok(api::PrResponse::Pending)) => {
                    // The closure reads `updates.sync` so only this text, not the
                    // whole view, re-renders when the sync status changes.
                    let key = key.clone();
                    view! { <div class="empty">{move || pending_message(&key, updates.sync.with(|s| s.paused_until), (js_sys::Date::now() / 1000.0) as i64)}</div> }.into_any()
                }
                Some(Ok(api::PrResponse::NotConfigured)) => view! {
                    <div class="empty">{format!("{} isn't configured in grenadine. ", key.repo)}<a href=github.clone() target="_blank" rel="noopener">"See it on GitHub"</a></div>
                }.into_any(),
                Some(Ok(api::PrResponse::Failed(e))) => view! {
                    <div class="empty error">{format!("Can't sync {}#{}: {e}. ", key.repo, key.number)}<a href=github.clone() target="_blank" rel="noopener">"See it on GitHub"</a></div>
                }.into_any(),
                Some(Ok(api::PrResponse::Ready(_))) => ().into_any(),
            }
            }
        }}
        {move || {
            ready.get().then(|| {
                view! {
                    <PrBody pr=current.into() chosen=chosen side_by_side=side_by_side inline_changes=inline_changes rebase_older=rebase_older pinned=pinned sidebar=Signal::derive(shown) />
                }
            })
        }}
        </main>
        {move || {
            let panel_key = panel_key.clone();
            let review_key = review_key.clone();
            let auto_review_key = auto_review_key.clone();
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
                                {button(false)}
                            </div>
                            // Up top, so merging after a review needs no
                            // scrolling back up. Merging is the only thing
                            // here that needs `ReviewTarget`; provided
                            // page-wide it would reach the agents panel's
                            // diffs.
                            {move || detail.get().map(|d| {
                                provide_context(ReviewTarget {
                                    key: d.summary.key.clone(),
                                    reviewing: Signal::derive(move || {
                                        detail.with(|d| d.as_ref().is_some_and(|d| {
                                            d.review.as_ref().is_some_and(|r| r.pending_review.is_some())
                                        }))
                                    }),
                                });
                                view! {
                                    {merge_section(&d, pinned.get(), true)}
                                    {merge_warning()}
                                }
                            })}
                            // Its own closure, so that a refetch of the PR
                            // doesn't remount it.
                            {move || own_open.get().then(|| view! {
                                <crate::auto_reviews::PrSection key=auto_review_key.clone() head=head />
                            })}
                            <crate::review::SidebarReview
                                pr=Signal::derive(move || match pr.get() {
                                    Some(Ok(api::PrResponse::Ready(d))) => Some(*d),
                                    _ => None,
                                })
                                key=review_key
                            />
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
    /// The viewer has a pending review on the PR; tracked, since a sync
    /// can flip it.
    pub(crate) reviewing: Signal<bool>,
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
    pr: Signal<Arc<PrDetail>>,
    chosen: RwSignal<Option<Selection>>,
    side_by_side: RwSignal<bool>,
    inline_changes: RwSignal<bool>,
    rebase_older: RwSignal<bool>,
    /// The version the right side stays on while the user picks none.
    /// `PrView` holds it so the sidebar's merge box can merge it too.
    pinned: RwSignal<u32>,
    /// The sidebar shows, and with it the merge box; the main column
    /// shows the merge box only while it's hidden.
    sidebar: Signal<bool>,
) -> impl IntoView {
    let first = pr.get_untracked();
    let repo = first.summary.key.repo.clone();
    provide_context(PrLinks { repo: repo.clone() });
    provide_context(ReviewTarget {
        key: first.summary.key.clone(),
        reviewing: Memo::new(move |_| {
            pr.get()
                .review
                .as_ref()
                .is_some_and(|r| r.pending_review.is_some())
        })
        .into(),
    });
    // The boxes editing the PR's title and description. Their drafts are
    // named like the comment boxes' so that a resync's re-render doesn't
    // close them; the text each edit started from is kept under the
    // draft's `.base` key (see `edit_base`).
    let boxes = expect_context::<OpenBoxes>();
    let title_draft = format!(
        "edit-title.{}#{}",
        first.summary.key.repo, first.summary.key.number
    );
    let body_draft = format!(
        "edit-body.{}#{}",
        first.summary.key.repo, first.summary.key.number
    );
    drop_edit_base(boxes, title_draft.clone());
    drop_edit_base(boxes, body_draft.clone());
    let title_text =
        draft_signal_from(comment_draft_key(&title_draft), first.summary.title.clone());
    let title_editing = {
        let title_draft = title_draft.clone();
        Signal::derive(move || boxes.is_open(&title_draft))
    };
    let title_sending = {
        let title_draft = title_draft.clone();
        Signal::derive(move || boxes.sending(&title_draft))
    };
    let title_error = {
        let title_draft = title_draft.clone();
        Signal::derive(move || boxes.error(&title_draft))
    };
    let title_input = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |_| {
        if let Some(i) = title_input.get() {
            let _ = i.focus();
        }
    });
    let save_title = {
        let (key, title_draft) = (first.summary.key.clone(), title_draft.clone());
        Callback::new(move |()| {
            if title_text.with(|t| t.trim().is_empty()) || title_sending.get_untracked() {
                return;
            }
            let edit = EditPr {
                title: Some(EditText {
                    old: edit_base(&title_draft, &pr.get_untracked().summary.title),
                    new: title_text.get_untracked(),
                }),
                body: None,
            };
            let (key, title_draft) = (key.clone(), title_draft.clone());
            boxes.send(
                &title_draft.clone(),
                async move { api::edit_pr(&key, &edit).await },
                move || boxes.close(&title_draft),
            );
        })
    };
    let cancel_title = {
        let title_draft = title_draft.clone();
        Callback::new(move |()| {
            title_text.set(String::new());
            boxes.close(&title_draft);
        })
    };
    // Whether the viewer may edit the PR's title and description.
    let can_update = Memo::new(move |_| pr.get().review.as_ref().is_some_and(|r| r.can_update));
    // Edits of comments that are no longer pending, because they were
    // deleted or their review submitted or discarded, e.g. on github.com,
    // can't be saved anymore.
    Effect::new(move |_| {
        let pr = pr.get();
        let pending: HashSet<String> = pr
            .comments
            .iter()
            .filter(|c| c.pending)
            .map(|c| edit_draft(&pr.summary.key, c.id))
            .collect();
        expect_context::<OpenBoxes>()
            .close_unless(&edit_prefix(&pr.summary.key), |d| pending.contains(d));
    });
    // The version the right side stays on while the user picks none, so
    // a later push doesn't move the diff they're reading.
    pinned.set(first.versions.len() as u32);
    let selection = Memo::new(move |_| {
        let pr = pr.get();
        let latest = pr.versions.len() as u32;
        let default = Selection {
            base: 0,
            head: pinned.get().clamp(1, latest.max(1)),
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
    });
    // Only when both sides sit on different bases does a rebase bring in
    // upstream changes.
    let upstream = Memo::new(move |_| {
        let pr = pr.get();
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
    });
    // What the diff compares; a Memo so a sync that didn't move it
    // doesn't reload it.
    let diff_args = Memo::new(move |_| {
        let pr = pr.get();
        if pr.versions.is_empty() {
            return None;
        }
        let sel = selection.get();
        let head = &pr.versions[sel.head as usize - 1];
        let left = if sel.base == 0 {
            head.merge_base.clone()
        } else {
            Some(pr.versions[sel.base as usize - 1].sha.clone())
        };
        left.map(|left| (left, head.sha.clone(), upstream.get()))
    });
    provide_context(VersionJump { chosen, selection });
    // The paths of the files the loaded diff shows, set by DiffView;
    // `None` until it has loaded them.
    let diff_paths = RwSignal::new(None::<Arc<HashSet<String>>>);
    // Where the threads go for the selected diff: inline per file, and
    // the panel of ones that can't be shown.
    let placed = Memo::new(move |_| {
        let pr = pr.get();
        if pr.versions.is_empty() {
            return Default::default();
        }
        place(
            &pr.comments,
            &pr.versions,
            selection.get(),
            diff_paths.get().as_deref(),
        )
    });
    let inline = Memo::new(move |_| Arc::new(placed.get().0));
    let panel = Memo::new(move |_| placed.get().1);
    // GitHub anchors new comments to the PR's diff from the merge base
    // to the head, and refuses lines outside it. Against an earlier
    // version, a line can be changed in the diff shown but not in
    // GitHub's, e.g. one the latest push changed back.
    let commenting = Memo::new(move |_| {
        let pr = pr.get();
        (pr.review.is_some()
            && pr.summary.state == "OPEN"
            && selection.get().base == 0
            && selection.get().head as usize == pr.versions.len())
        .then(|| Commenting {
            key: pr.summary.key.clone(),
        })
    });
    let versions = Memo::new(move |_| pr.get().versions.clone());
    let comments = Memo::new(move |_| pr.get().comments.clone());
    let reviewed = Memo::new(move |_| {
        let pr = pr.get();
        last_reviewed(&pr.versions, pr.review.as_ref())
    });
    // A push doesn't move the diff; the notice offers the new version.
    let new_version = Memo::new(move |_| {
        let latest = pr.get().versions.len() as u32;
        (latest > pinned.get()).then_some(latest)
    });
    let has_review = Memo::new(move |_| {
        let pr = pr.get();
        pr.review.is_some() && pr.summary.state == "OPEN"
    });
    let pending_count = Memo::new(move |_| pr.get().comments.iter().filter(|c| c.pending).count());
    let own = Memo::new(move |_| {
        let pr = pr.get();
        pr.review
            .as_ref()
            .is_some_and(|i| i.viewer == pr.summary.author)
    });
    let no_versions = Memo::new(move |_| pr.with(|p| p.versions.is_empty()));

    // File headers stick just below the toolbar, so its height is kept in
    // a CSS variable. Wrapping and the review panel change that height. It
    // lives on the document root and is never removed: the CSS only reads
    // it inside `.pr`, and a detached toolbar's reports would be wrong.
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
            // A toolbar whose page was left reports a height of 0.
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

    // A zero-height marker just above the toolbar reports when the
    // toolbar is pinned: while the marker sits above the scroller's top
    // edge, the toolbar below it is stuck. The title row only renders
    // then; above it the real h1 is still visible.
    let stuck = RwSignal::new(false);
    let marker = NodeRef::<leptos::html::Div>::new();
    let observing = StoredValue::new_local(
        None::<(
            web_sys::IntersectionObserver,
            Closure<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>,
        )>,
    );
    Effect::new(move |_| {
        let Some(marker) = marker.get() else {
            return;
        };
        if observing.with_value(|o| o.is_some()) {
            return;
        }
        let Ok(Some(root)) = marker.closest(".main") else {
            return;
        };
        let options = web_sys::IntersectionObserverInit::new();
        options.set_root(Some(&root));
        options.set_threshold_f64(0.0);
        let on_intersect = Closure::<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>::new(
            move |entries: js_sys::Array, _| {
                let Some(entry) = entries
                    .get(0)
                    .dyn_into::<web_sys::IntersectionObserverEntry>()
                    .ok()
                else {
                    return;
                };
                // Out of view both ways is possible: above the scroller once
                // scrolled into the diff, below it before the toolbar pins.
                let above = entry
                    .root_bounds()
                    .is_some_and(|root| entry.bounding_client_rect().top() < root.top());
                stuck.set(!entry.is_intersecting() && above);
            },
        );
        if let Ok(observer) = web_sys::IntersectionObserver::new_with_options(
            on_intersect.as_ref().unchecked_ref(),
            &options,
        ) {
            observer.observe(&marker);
            observing.set_value(Some((observer, on_intersect)));
        }
    });
    on_cleanup(move || {
        observing.try_with_value(|o| {
            if let Some((observer, _)) = o {
                observer.disconnect();
            }
        });
    });

    view! {
        <article class="pr">
            {move || {
                let pr = pr.get();
                let s = &pr.summary;
                let (title_draft, title_now, number, url) = (
                    title_draft.clone(),
                    s.title.clone(),
                    s.key.number,
                    s.url.clone(),
                );
                view! {
                    <header class="pr-header">
                        {move || {
                            let title = title_text;
                            if title_editing.get() {
                                view! {
                                    <form class="edit-title" on:submit=move |ev| {
                                        ev.prevent_default();
                                        save_title.run(());
                                    }>
                                        <input
                                            type="text"
                                            node_ref=title_input
                                            aria-label="Edit the title"
                                            prop:value=move || title.get()
                                            on:input=move |ev| title.set(event_target_value(&ev))
                                            on:keydown=move |ev| {
                                                if ev.key() == "Escape" {
                                                    ev.prevent_default();
                                                    cancel_title.run(());
                                                }
                                            }
                                        />
                                        <button type="submit" class="primary"
                                            disabled=move || title.with(|t| t.trim().is_empty()) || title_sending.get()>
                                            {move || if title_sending.get() { "Saving…" } else { "Save" }}
                                        </button>
                                        <button type="button" disabled=move || title_sending.get()
                                            on:click=move |_| cancel_title.run(())>"Cancel"</button>
                                    </form>
                                    {move || title_error.get().map(|e| view! { <p class="error">{e}</p> })}
                                }
                                .into_any()
                            } else {
                                view! {
                                    <div class="pr-title-row">
                                        <h1>{title_now.clone()} " "
                                            <span class="muted pr-number">{format!("#{number}")}</span>
                                            <GithubLink url=url.clone() />
                                        </h1>
                                        {can_update.get().then(|| {
                                            let (t, title_draft) =
                                                (title_now.clone(), title_draft.clone());
                                            view! {
                                                <button type="button" class="link edit-pr"
                                                    on:click=move |_| {
                                                        set_edit_base(&title_draft, &t);
                                                        title_text.set(t.clone());
                                                        boxes.set(&title_draft, true);
                                                    }>"Edit"</button>
                                            }
                                        })}
                                    </div>
                                }
                                .into_any()
                            }
                        }}
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
                }
                .into_any()
            }}
            {move || {
                let pr = pr.get();
                pr.stack
                    .clone()
                    .filter(|st| st.prs.len() > 1)
                    .map(|st| {
                        let s = &pr.summary;
                        view! { <StackList stack=st repo=s.key.repo.clone() current=s.key.number /> }
                            .into_any()
                    })
            }}
            {move || {
                let pr = pr.get();
                let (body_draft, repo) =
                    (body_draft.clone(), pr.summary.key.repo.clone());
                if boxes.is_open(&body_draft) {
                    let (key, send_draft) = (pr.summary.key.clone(), body_draft.clone());
                    let fallback = pr.body.clone();
                    let initial = edit_base(&body_draft, &fallback);
                    let send = sender(move |body| {
                        let edit = EditPr {
                            title: None,
                            body: Some(EditText {
                                old: edit_base(&send_draft, &fallback),
                                new: body,
                            }),
                        };
                        let key = key.clone();
                        async move { api::edit_pr(&key, &edit).await }
                    });
                    view! {
                        <section class="description">
                            <Composer
                                draft=body_draft.clone()
                                repo=repo.clone()
                                label="Save"
                                placeholder="Describe the PR…"
                                send=send
                                initial=initial
                                autofocus=true
                                allow_empty=true
                            />
                        </section>
                    }
                    .into_any()
                } else {
                    let html = if pr.body.trim().is_empty() {
                        "<p class=\"muted\">No description provided.</p>".to_owned()
                    } else {
                        markdown::to_html(&pr.body, &pr.summary.key.repo)
                    };
                    view! {
                        <section class="description">
                            {can_update.get().then(|| {
                                let (b, body_draft) = (pr.body.clone(), body_draft.clone());
                                view! {
                                    <button type="button" class="link edit-pr description-edit"
                                        on:click=move |_| {
                                            set_edit_base(&body_draft, &b);
                                            boxes.set(&body_draft, true);
                                        }>"Edit"</button>
                                }
                            })}
                            <div class="markdown" inner_html=html></div>
                        </section>
                    }
                    .into_any()
                }
            }}
            {move || pr.with(|p| p.review.clone()).map(|info| conversation(&info).into_any())}
            {move || (!sidebar.get()).then(|| view! {
                {move || merge_section(&pr.get(), pinned.get(), false)}
                {merge_warning()}
            })}
            <div class="toolbar-marker" aria-hidden="true" node_ref=marker></div>
            <div class="toolbar" node_ref=toolbar>
                {move || {
                    stuck.get().then(|| {
                        let pr = pr.get();
                        let s = &pr.summary;
                        view! {
                            <div class="toolbar-title">
                                <span class=format!("state state-{}", state_label(&s.state, s.queued))>{state_label(&s.state, s.queued)}</span>
                                {s.title.clone()} " " <span class="muted pr-number">{format!("#{}", s.key.number)}</span><GithubLink url=s.url.clone() />
                            </div>
                        }
                        .into_any()
                    })
                }}
                <VersionPicker versions=versions comments=comments reviewed=reviewed chosen=chosen selection=selection />
                {move || {
                    has_review.get().then(|| {
                        view! { <ReviewPanel pending=pending_count own=own /> }
                    })
                }}
                {move || {
                    new_version.get().map(|n| view! {
                        <p class="new-version-notice">
                            {format!("Version {n} was pushed")}
                            " "
                            <button type="button" class="link" on:click=move |_| {
                                pinned.set(n);
                                chosen.set(None);
                            }>"Show it"</button>
                        </p>
                    })
                }}
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
            {move || {
                if no_versions.get() {
                    return view! { <p class="empty">"This PR has no versions yet."</p> }.into_any();
                }
                match diff_args.get() {
                    None => view! { <p class="empty error">"The merge-base of this version is unknown."</p> }.into_any(),
                    Some((left, right, upstream)) => view! {
                        <CommentPanel threads=panel />
                        <DiffView
                            repo=repo.clone()
                            left=left
                            right=right
                            upstream=upstream
                            comments=inline
                            commenting=commenting
                            side_by_side=side_by_side.into()
                            inline_changes=inline_changes.into()
                            rebase_older=rebase_older.into()
                            diff_paths=diff_paths
                        />
                    }
                    .into_any(),
                }
            }}
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

/// The merge box of an open PR with review data, in the sidebar or the
/// main column.
fn merge_section(pr: &PrDetail, pinned: u32, sidebar: bool) -> Option<AnyView> {
    (pr.summary.state == "OPEN")
        .then_some(pr.review.as_ref())
        .flatten()
        .map(|info| merge_box(info, merge_target(pr, pinned), sidebar).into_any())
}

/// `head` is the version the merge expects at the tip: the pinned one,
/// so GitHub refuses a merge for a push the page hasn't shown.
fn merge_target(pr: &PrDetail, head: u32) -> MergeTarget {
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
        head_oid: pr
            .versions
            .get(head.max(1) as usize - 1)
            .map(|v| v.sha.clone()),
        base_ref: pr.base_ref.clone(),
        head_ref: pr.head_ref.clone(),
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
    versions: Memo<Vec<Version>>,
    comments: Memo<Vec<ReviewComment>>,
    reviewed: Memo<Option<u32>>,
    chosen: RwSignal<Option<Selection>>,
    selection: Memo<Selection>,
) -> impl IntoView {
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
    let missing = move |n: u32| n > 0 && versions.with_untracked(|v| v[n as usize - 1].missing);
    let label = move || {
        let s = selection.get();
        let versions = versions.get();
        let name = |n: u32| {
            if n == 0 {
                "Base".to_owned()
            } else {
                format!("v{n} ({})", short(&versions[n as usize - 1].sha))
            }
        };
        if versions.is_empty() {
            "No versions".to_owned()
        } else {
            format!("{} → {}", name(s.base), name(s.head))
        }
    };
    let set_base = move |n: u32| {
        let s = selection.get_untracked();
        let latest = versions.with_untracked(|v| v.len() as u32);
        // Moving Base past Head pulls Head to the latest version.
        let head = if n >= s.head { latest } else { s.head };
        chosen.set(Some(Selection { base: n, head }));
    };
    let set_head = move |n: u32| {
        let s = selection.get_untracked();
        let base = if n <= s.base { 0 } else { s.base };
        chosen.set(Some(Selection { base, head: n }));
    };

    let rows = move || {
        let versions = versions.get();
        let latest = versions.len() as u32;
        let counts = thread_counts(&comments.get(), &versions);
        let reviewed = reviewed.get();
        (0..=latest)
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
                                    {v.pushed_at.clone().map(|t| view! { <RelTime at=t class="muted vtime" /> })}
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
            .collect_view()
    };

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

/// The page's clock, provided by `PrView` so the relative times on it
/// tick together.
#[derive(Clone, Copy)]
pub(crate) struct Now(pub RwSignal<i64>);

/// `at`, an RFC 3339 timestamp, as a relative time like "5m ago" that
/// ticks with the page's clock, and its local time for a title. `None`
/// when `at` isn't a timestamp, like a pending comment's empty time.
fn rel(at: &str) -> Option<(impl Fn() -> String + Clone + use<>, String)> {
    let secs = crate::rfc3339_secs(at)?;
    let clock = use_context::<Now>();
    let text = move || {
        let now = clock.map_or_else(|| (js_sys::Date::now() / 1000.0) as i64, |n| n.0.get());
        crate::ago(now, secs)
    };
    Some((text, crate::local_datetime(secs)))
}

/// `at`, an RFC 3339 timestamp, shown as a relative time like "5m ago"
/// with its local time as the title. Renders nothing when `at` isn't a
/// timestamp.
#[component]
pub(crate) fn RelTime(at: String, #[prop(optional)] class: &'static str) -> impl IntoView {
    rel(&at).map(|(text, title)| view! { <span class=class title=title>{text}</span> })
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
    let time = rel(&at);
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
                {time.map(|(text, title)| view! {
                    <a class="muted" href=url target="_blank" rel="noopener" title=title>{text}</a>
                })}
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

/// The localStorage key of the text an edit named `draft` started from.
/// It's kept next to the draft so that a box a reload restored still
/// refuses to overwrite what changed on GitHub since the edit started.
fn edit_base_key(draft: &str) -> String {
    format!("{}.base", comment_draft_key(draft))
}

/// Marks the text the edit named `draft` started from.
fn set_edit_base(draft: &str, text: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(&edit_base_key(draft), text);
    }
}

/// The text the edit named `draft` started from, or `current` when none
/// was kept.
fn edit_base(draft: &str, current: &str) -> String {
    storage()
        .and_then(|s| s.get_item(&edit_base_key(draft)).ok().flatten())
        .unwrap_or_else(|| current.to_owned())
}

/// Drops the edit's base once its box is closed, so a later edit of the
/// same field starts fresh.
fn drop_edit_base(boxes: OpenBoxes, draft: String) {
    Effect::new(move |_| {
        if boxes.is_open(&draft) {
            return;
        }
        if let Some(s) = storage() {
            let _ = s.remove_item(&edit_base_key(&draft));
        }
    });
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
        // On success the comment's view re-renders once the PR has synced.
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
pub fn ThreadView(
    thread: Arc<Thread>,
    #[prop(optional_no_strip)] lines: Option<String>,
) -> impl IntoView {
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
                {lines.map(|lines| view! { <span class="muted thread-lines">{lines}</span> })}
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
        Callback::new(move |_: leptos::ev::MouseEvent| {
            let (key, thread) = (key.clone(), thread.clone());
            busy.set(true);
            error.set(None);
            leptos::task::spawn_local(async move {
                // On success the thread's view re-renders once the PR has
                // synced, and until then the button stays busy. This
                // component may be gone by the time the answer comes.
                if let Err(e) = api::set_thread_resolved(&key, &thread, !resolved).await {
                    error.try_set(Some(e));
                    busy.try_set(false);
                }
            });
        })
    };
    let reply = {
        let (key, thread) = (key.clone(), thread.clone());
        move |review: bool| -> SendComment {
            let (key, thread) = (key.clone(), thread.clone());
            sender(move |body| {
                let (key, thread) = (key.clone(), thread.clone());
                async move { api::reply_to_thread(&key, &thread, body, review).await }
            })
        }
    };
    let can_resolve = root.can_resolve;
    view! {
        <div class="thread-actions">
            {move || {
                // While a review is pending, replies go into it unless
                // asked otherwise.
                let (label, send, alt) = if target.reviewing.get() {
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
                view! {
                    <ComposerToggle draft=format!("reply.{thread}") label="Reply" placeholder="Reply…"
                        send_label=label send=send alt=alt>
                        {can_resolve.then(|| view! {
                            <button type="button" disabled=move || busy.get() on:click=move |e| toggle.run(e)>
                                {if resolved { "Unresolve" } else { "Resolve" }}
                            </button>
                        })}
                    </ComposerToggle>
                }
            }}
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
        </div>
    }
    .into_any()
}

/// The diff's selection, for the buttons that jump a thread to the
/// version it was made on.
#[derive(Clone, Copy)]
struct VersionJump {
    chosen: RwSignal<Option<Selection>>,
    selection: Memo<Selection>,
}

/// What a thread shown away from its lines says about where it was made:
/// `children` — its path, line or range — followed by an "on vN" button
/// that diffs vN against the current base (or Base), or "on an unknown
/// version" when no version has its commit. The side panel and the
/// file-level fallback threads share it.
#[component]
pub(crate) fn ThreadWhere(version: Option<u32>, children: Children) -> impl IntoView {
    let jump = expect_context::<VersionJump>();
    view! {
        {children()}
        " "
        {match version {
            Some(v) => view! {
                <button
                    class="link"
                    on:click=move |_| {
                        let s = jump.selection.get_untracked();
                        let base = if s.base < v { s.base } else { 0 };
                        jump.chosen.set(Some(Selection { base, head: v }));
                    }
                >
                    {format!("on v{v}")}
                </button>
            }
            .into_any(),
            None => view! { <span class="muted">"on an unknown version"</span> }.into_any(),
        }}
    }
}

#[component]
fn CommentPanel(threads: Memo<Vec<Unplaced>>) -> impl IntoView {
    move || {
        let threads = threads.get();
        if threads.is_empty() {
            return ().into_any();
        }
        let count = threads.len();
        let items = threads
            .into_iter()
            .map(|u| {
                let c = &u.thread.root;
                let line = c.original_line.map(|l| format!(":{l}")).unwrap_or_default();
                let lines = match (c.original_start_line, c.original_line) {
                    (Some(s), Some(e)) if (c.start_side.unwrap_or(c.side), s) != (c.side, e) => {
                        Some(range_label(
                            (c.start_side.unwrap_or(c.side), s),
                            (c.side, e),
                        ))
                    }
                    _ => None,
                };
                let path = format!("{}{line}", c.path);
                view! {
                    <li>
                        <div class="panel-where">
                            <ThreadWhere version=u.version>
                                <code>{path}</code>
                            </ThreadWhere>
                        </div>
                        <ThreadView thread=u.thread.clone() lines=lines />
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
            start_side: None,
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
            can_edit_reviewers: false,
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
                stack: None,
            },
            pending_review: None,
            can_update: true,
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
        let (inline, panel) = place(&comments, &versions, Selection { base: 0, head: 3 }, None);
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
        let (inline, panel) = place(&comments, &versions, Selection { base: 1, head: 2 }, None);
        assert_eq!(
            anchors(&inline),
            [
                (1, Anchor::Line(Side::Left, 10)),
                (2, Anchor::Line(Side::Right, 20))
            ]
        );
        assert_eq!(panel.len(), 1);

        // v2 → v3: v2's own thread anchors the left too.
        let (inline, _) = place(&comments, &versions, Selection { base: 2, head: 3 }, None);
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

        let (inline, panel) = place(&comments, &versions, Selection { base: 1, head: 3 }, None);
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

    fn starts(inline: &HashMap<String, Vec<Placed>>) -> Vec<(u64, Option<(Side, u32)>)> {
        let mut out: Vec<_> = inline
            .values()
            .flatten()
            .map(|p| (p.thread.root.id, p.start))
            .collect();
        out.sort_by_key(|(id, _)| *id);
        out
    }

    #[test]
    fn placement_sets_a_ranges_start() {
        let versions = [version(1, "a"), version(2, "b")];
        // Made on the head: the start is its original_start_line.
        let mut on_head = comment(1, "b", Side::Right, 20, Some(20));
        on_head.original_start_line = Some(16);
        // Mapped forward from v1: the start is start_line.
        let mut mapped = comment(2, "a", Side::Right, 10, Some(30));
        mapped.start_line = Some(26);
        // A range ending where it starts is a single line.
        let mut single = comment(3, "b", Side::Right, 5, Some(5));
        single.original_start_line = Some(5);
        // A range crossing the sides starts on the left.
        let mut cross = comment(4, "b", Side::Right, 22, Some(22));
        cross.original_start_line = Some(9);
        cross.start_side = Some(Side::Left);
        let comments = [on_head, mapped, single, cross];

        let (inline, _) = place(&comments, &versions, Selection { base: 0, head: 2 }, None);
        assert_eq!(
            starts(&inline),
            [
                (1, Some((Side::Right, 16))),
                (2, Some((Side::Right, 26))),
                (3, None),
                (4, Some((Side::Left, 9))),
            ]
        );
    }

    #[test]
    fn placement_of_a_ranges_start_on_the_left_version() {
        let versions = [version(1, "a"), version(2, "b")];
        // A range made on v1 starts on the left with it.
        let mut ranged = comment(1, "a", Side::Right, 10, None);
        ranged.original_start_line = Some(8);
        // A range starting on the left lands on the merge-base, which a
        // diff between versions doesn't show.
        let mut cross = comment(2, "a", Side::Right, 12, None);
        cross.original_start_line = Some(9);
        cross.start_side = Some(Side::Left);
        // A left-side start on a head comment has the same problem.
        let mut head_cross = comment(3, "b", Side::Right, 20, Some(20));
        head_cross.original_start_line = Some(9);
        head_cross.start_side = Some(Side::Left);
        let comments = [ranged, cross, head_cross];

        let (inline, _) = place(&comments, &versions, Selection { base: 1, head: 2 }, None);
        assert_eq!(
            anchors(&inline),
            [
                (1, Anchor::Line(Side::Left, 10)),
                (2, Anchor::Line(Side::Left, 12)),
                (3, Anchor::Line(Side::Right, 20)),
            ]
        );
        assert_eq!(
            starts(&inline),
            [(1, Some((Side::Left, 8))), (2, None), (3, None)]
        );
    }

    fn fallbacks(inline: &HashMap<String, Vec<Placed>>) -> Vec<(u64, Anchor, Option<Fallback>)> {
        let mut out: Vec<_> = inline
            .values()
            .flatten()
            .map(|p| (p.thread.root.id, p.anchor, p.fallback))
            .collect();
        out.sort_by_key(|(id, _, _)| *id);
        out
    }

    #[test]
    fn placement_falls_back_to_the_file() {
        let versions = [version(1, "a"), version(2, "b"), version(3, "c")];
        // GitHub reports the thread outdated: it has no mapped line.
        let comments = [comment(1, "a", Side::Right, 10, None)];
        let paths = |p: &str| HashSet::from([p.to_owned()]);

        // Its file in the diff: a file-level anchor marked outdated, its
        // version for the header.
        let (inline, panel) = place(
            &comments,
            &versions,
            Selection { base: 0, head: 3 },
            Some(&paths("f.rs")),
        );
        assert_eq!(
            fallbacks(&inline),
            [(
                1,
                Anchor::File,
                Some(Fallback {
                    version: Some(1),
                    outdated: true
                })
            )]
        );
        assert_eq!(inline["f.rs"][0].start, None);
        assert!(panel.is_empty());

        // A file the diff doesn't show, or a diff not yet loaded: the
        // side panel, as before.
        let (inline, panel) = place(
            &comments,
            &versions,
            Selection { base: 0, head: 3 },
            Some(&paths("other.rs")),
        );
        assert!(inline.is_empty());
        assert_eq!(panel.len(), 1);
        let (inline, panel) = place(&comments, &versions, Selection { base: 0, head: 3 }, None);
        assert!(inline.is_empty());
        assert_eq!(panel.len(), 1);
        assert_eq!(panel[0].version, Some(1));
    }

    #[test]
    fn placement_fallback_is_outdated_only_without_a_mapped_line() {
        let versions = [version(1, "a"), version(2, "b"), version(3, "c")];
        let paths = HashSet::from(["f.rs".to_owned()]);
        let comments = [
            // Mapped forward, but Head isn't the latest.
            comment(1, "a", Side::Right, 10, Some(12)),
            // A merge-base comment made on v1, unplaceable between
            // versions.
            comment(2, "a", Side::Left, 7, Some(7)),
        ];

        // Base → v2, not the latest: neither anchors a line, and neither
        // is outdated.
        let (inline, panel) = place(
            &comments,
            &versions,
            Selection { base: 0, head: 2 },
            Some(&paths),
        );
        assert_eq!(
            fallbacks(&inline),
            [
                (
                    1,
                    Anchor::File,
                    Some(Fallback {
                        version: Some(1),
                        outdated: false
                    })
                ),
                (
                    2,
                    Anchor::File,
                    Some(Fallback {
                        version: Some(1),
                        outdated: false
                    })
                ),
            ]
        );
        assert!(panel.is_empty());
    }

    #[test]
    fn placement_fallback_keeps_the_old_path_of_a_rename() {
        let versions = [version(1, "a"), version(2, "b")];
        // Made on the old path of a file the diff renamed: the diff lists
        // both paths, and the thread stays keyed by the old one so the
        // file view picks it up.
        let mut outdated = comment(1, "a", Side::Right, 10, None);
        outdated.path = "old.rs".into();
        let paths = HashSet::from(["new.rs".to_owned(), "old.rs".to_owned()]);

        let (inline, panel) = place(
            &[outdated],
            &versions,
            Selection { base: 0, head: 2 },
            Some(&paths),
        );
        assert_eq!(
            fallbacks(&inline),
            [(
                1,
                Anchor::File,
                Some(Fallback {
                    version: Some(1),
                    outdated: true
                })
            )]
        );
        assert!(inline.contains_key("old.rs"));
        assert!(panel.is_empty());
    }

    #[test]
    fn range_label_formats_githubs_label() {
        assert_eq!(
            range_label((Side::Right, 26), (Side::Right, 29)),
            "Lines +26 to +29"
        );
        assert_eq!(
            range_label((Side::Left, 10), (Side::Left, 14)),
            "Lines -10 to -14"
        );
        assert_eq!(
            range_label((Side::Left, 10), (Side::Right, 14)),
            "Lines -10 to +14"
        );
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
