//! One PR: its title and description, the version picker, the review
//! comments that don't fit into the diff, and the diff.

use std::collections::HashMap;
use std::sync::Arc;

use grenadine_core::api::{PrDetail, PrKey, ReviewComment, Side, Version, VersionKind};
use leptos::prelude::*;

use crate::diffview::DiffView;
use crate::sidebar::{load_flag, save_flag};
use crate::{Updates, api, markdown};

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

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

#[component]
pub fn PrView(key: PrKey) -> impl IntoView {
    let updates = expect_context::<Updates>();
    // Changes only when this PR changed.
    let rev = {
        let key = key.clone();
        Memo::new(move |prev: Option<&u64>| {
            updates.pr.with(|(k, n)| {
                if k.as_ref() == Some(&key) {
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

    move || {
        match pr.get() {
        None => view! { <div class="empty">"Loading…"</div> }.into_any(),
        Some(Err(e)) => view! { <div class="empty error">{format!("Can't load {}#{}: {e}", key.repo, key.number)}</div> }.into_any(),
        Some(Ok(None)) => view! { <div class="empty">{format!("{}#{} hasn't synced yet", key.repo, key.number)}</div> }.into_any(),
        Some(Ok(Some(pr))) => {
            let pr = Arc::new(pr);
            prefetch(&pr);
            view! { <PrBody pr=pr chosen=chosen side_by_side=side_by_side /> }.into_any()
        }
    }
    }
}

/// Warms the caches for the diffs most likely to be looked at next.
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
            }
        }
    });
}

#[component]
fn PrBody(
    pr: Arc<PrDetail>,
    chosen: RwSignal<Option<Selection>>,
    side_by_side: RwSignal<bool>,
) -> impl IntoView {
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
    let s = &pr.summary;
    let body_html = if pr.body.trim().is_empty() {
        "<p class=\"muted\">No description provided.</p>".to_owned()
    } else {
        markdown::to_html(&pr.body)
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
            // Only when both sides sit on different bases does a rebase
            // bring in upstream changes.
            let upstream = (sel.base != 0)
                .then(|| {
                    (
                        pr.versions[sel.base as usize - 1].merge_base.clone(),
                        head.merge_base.clone(),
                    )
                })
                .and_then(|(a, b)| Some((a?, b?)))
                .filter(|(a, b)| a != b);
            let (inline, panel) = place(&pr.comments, &pr.versions, sel);
            view! {
                <CommentPanel threads=panel chosen=chosen selection=selection />
                <DiffView
                    repo=pr.summary.key.repo.clone()
                    left=left
                    right=head.sha.clone()
                    upstream=upstream
                    comments=Arc::new(inline)
                    side_by_side=side_by_side.into()
                />
            }
            .into_any()
        }
    };

    view! {
        <article class="pr">
            <header class="pr-header">
                <h1>{s.title.clone()} " " <a class="muted pr-number" href=s.url.clone() target="_blank" rel="noopener">{format!("#{}", s.key.number)}</a></h1>
                <p class="muted">
                    <span class=format!("state state-{}", s.state.to_lowercase())>{s.state.to_lowercase()}</span>
                    {s.is_draft.then(|| view! { <span class="badge">"draft"</span> })}
                    {format!(" {} wants to merge {} into {} in {}", s.author, pr.head_ref, pr.base_ref, s.key.repo)}
                </p>
                {pr.approximate.then(|| view! {
                    <p class="banner warn">"GitHub has no push log for this PR, so its versions were reconstructed from force-push events and may not match every push."</p>
                })}
                {pr.drift.clone().map(|d| view! { <p class="banner warn">{d}</p> })}
                {pr.sync_error.clone().map(|e| view! { <p class="banner error">{format!("The last sync failed: {e}")}</p> })}
            </header>
            <section class="description markdown" inner_html=body_html></section>
            <div class="toolbar">
                <VersionPicker versions=pr.versions.clone() chosen=chosen selection=selection />
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
            </div>
            {diff}
        </article>
    }
}

fn kind_label(v: &Version) -> &'static str {
    match v.kind {
        VersionKind::Initial => "opened",
        VersionKind::Push => "push",
        VersionKind::ForcePush => "force push",
    }
}

#[component]
fn VersionPicker(
    versions: Vec<Version>,
    chosen: RwSignal<Option<Selection>>,
    selection: Memo<Selection>,
) -> impl IntoView {
    let latest = versions.len() as u32;
    let versions = Arc::new(versions);
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
            view! {
                <tr class:missing=v.as_ref().is_some_and(|v| v.missing)>
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
                                {v.missing.then(|| view! { <span class="error">"missing"</span> })}
                            }.into_any(),
                        }}
                    </td>
                </tr>
            }
        })
        .collect_view();

    view! {
        <details class="picker">
            <summary>{label}</summary>
            <table class="versions">
                <thead><tr><th>"Base"</th><th>"Head"</th><th>"Version"</th></tr></thead>
                <tbody>{rows}</tbody>
            </table>
        </details>
    }
}

#[component]
pub fn CommentBody(comment: ReviewComment) -> impl IntoView {
    view! {
        <div class="comment">
            <div class="comment-meta">
                <strong>{comment.author.clone()}</strong>
                " "
                <a class="muted" href=comment.url.clone() target="_blank" rel="noopener">{comment.created_at.replace('T', " ").trim_end_matches('Z').to_owned()}</a>
            </div>
            <div class="markdown" inner_html=markdown::to_html(&comment.body)></div>
        </div>
    }
}

#[component]
pub fn ThreadView(thread: Arc<Thread>) -> impl IntoView {
    view! {
        <div class="thread">
            <CommentBody comment=thread.root.clone() />
            {thread.replies.iter().map(|r| view! { <CommentBody comment=r.clone() /> }).collect_view()}
        </div>
    }
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
    use super::*;

    fn version(n: u32, sha: &str) -> Version {
        Version {
            number: n,
            sha: sha.into(),
            merge_base: Some("mb".into()),
            kind: VersionKind::Push,
            pushed_at: None,
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

        // v1 → v2: the comment made on v2 goes inline at its original line.
        let (inline, panel) = place(&comments, &versions, Selection { base: 1, head: 2 });
        assert_eq!(anchors(&inline), [(2, Anchor::Line(Side::Right, 20))]);
        assert_eq!(panel.len(), 2);

        // v2 → v3: left-side comments don't fit when the left isn't the base.
        let (inline, _) = place(&comments, &versions, Selection { base: 2, head: 3 });
        assert_eq!(anchors(&inline), [(1, Anchor::Line(Side::Right, 12))]);
    }

    #[test]
    fn replies_join_their_thread() {
        let mut reply = comment(2, "a", Side::Right, 1, None);
        reply.in_reply_to = Some(1);
        let t = threads(&[comment(1, "a", Side::Right, 1, None), reply]);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].replies.len(), 1);
    }
}
