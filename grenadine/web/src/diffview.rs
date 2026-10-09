//! The diff between two commits, computed in the browser from blobs the
//! server sends.

use std::collections::HashMap;
use std::sync::Arc;

use grenadine_core::api::{Blob, ChangeStatus, FileChange, Side};
use grenadine_core::diff::{self, Hunk, Line};
use grenadine_core::inline::{self, Aligned};
use grenadine_core::rebase;
use leptos::prelude::*;

use crate::api;
use crate::highlight::{highlight_lines, mark};
use crate::pr::{Anchor, Placed, ThreadView};

const CONTEXT: usize = 3;

/// Files with more changed lines than this start collapsed.
const BIG_FILE: usize = 3000;

/// The old and new blob of one side of a file change.
type BlobPair = (Option<Arc<Blob>>, Option<Arc<Blob>>);

struct FileData {
    change: FileChange,
    old: Option<Arc<Blob>>,
    new: Option<Arc<Blob>>,
    /// The same file's upstream change between the two bases, when the
    /// diff spans a rebase and upstream touched the file.
    upstream: Option<BlobPair>,
}

fn blob(id: &Option<String>) -> Option<Arc<Blob>> {
    id.as_deref().and_then(api::blob)
}

async fn load(
    repo: String,
    left: String,
    right: String,
    upstream: Option<(String, String)>,
) -> api::Result<(Vec<Arc<FileData>>, usize)> {
    let changes = api::changes(&repo, &left, &right).await?;
    let up_changes = match &upstream {
        Some((a, b)) => Some(api::changes(&repo, a, b).await?),
        None => None,
    };
    let mut up_by_path: HashMap<&str, &FileChange> = HashMap::new();
    for f in up_changes.iter().flat_map(|c| &c.files) {
        up_by_path.insert(f.path(), f);
        if let Some(old) = &f.old_path {
            up_by_path.entry(old).or_insert(f);
        }
    }

    // Between two different bases the interdiff also shows every file upstream
    // changed; only the files the PR itself touched in either version are
    // interesting.
    let (files, hidden) = match &upstream {
        Some((a, b)) => {
            let pr_a = api::changes(&repo, a, &left).await?;
            let pr_b = api::changes(&repo, b, &right).await?;
            rebase::touched_by_pr(&changes.files, &pr_a.files, &pr_b.files)
        }
        None => (changes.files.clone(), 0),
    };

    let mut ids = Vec::new();
    for f in &files {
        ids.extend(f.old_blob.iter().chain(&f.new_blob).cloned());
        if let Some(u) = up_by_path.get(f.path()) {
            ids.extend(u.old_blob.iter().chain(&u.new_blob).cloned());
        }
    }
    api::load_blobs(&repo, ids).await?;

    Ok((
        files
            .iter()
        .map(|f| {
            Arc::new(FileData {
                change: f.clone(),
                old: blob(&f.old_blob),
                new: blob(&f.new_blob),
                upstream: up_by_path
                    .get(f.path())
                    .map(|u| (blob(&u.old_blob), blob(&u.new_blob))),
            })
        })
        .collect(),
        hidden,
    ))
}

#[component]
pub fn DiffView(
    repo: String,
    left: String,
    right: String,
    upstream: Option<(String, String)>,
    comments: Arc<HashMap<String, Vec<Placed>>>,
    side_by_side: Signal<bool>,
    inline_changes: Signal<bool>,
) -> impl IntoView {
    let has_upstream = upstream.is_some();
    let files = LocalResource::new(move || {
        load(repo.clone(), left.clone(), right.clone(), upstream.clone())
    });
    move || match files.get() {
        None => view! { <p class="empty">"Loading the diff…"</p> }.into_any(),
        Some(Err(e)) => {
            view! { <p class="empty error">{format!("Can't load the diff: {e}")}</p> }.into_any()
        }
        Some(Ok((files, hidden))) if files.is_empty() => {
            view! { <p class="empty">{format!("No changes.{}", if hidden > 0 {
                format!(" {hidden} files changed only by the rebase are hidden.")
            } else {
                String::new()
            })}</p> }.into_any()
        }
        Some(Ok((files, hidden))) => {
            let comments = comments.clone();
            view! {
                <div class="diff-summary muted">
                    {format!("{} files changed", files.len())}
                    {(hidden > 0).then(|| view! {
                        <span>{format!(" · {hidden} files changed only by the rebase are hidden")}</span>
                    })}
                    {has_upstream.then(|| view! {
                        <span class="legend"><span class="upstream-swatch"></span>" changes brought in by the rebase"</span>
                    })}
                </div>
                <ul class="file-index">
                    {files.iter().map(|f| {
                        let path = f.change.path().to_owned();
                        view! { <li><a href="#" on:click=move |ev| { ev.prevent_default(); scroll_to(&path) }>{f.change.path().to_owned()}</a></li> }
                    }).collect_view()}
                </ul>
                {files.iter().map(|f| {
                    let placed = comments.get(f.change.path()).cloned().unwrap_or_default();
                    view! { <FileView file=f.clone() comments=placed side_by_side=side_by_side inline_changes=inline_changes /> }
                }).collect_view()}
            }
            .into_any()
        }
    }
}

fn file_id(path: &str) -> String {
    format!(
        "file-{}",
        path.replace(|c: char| !c.is_ascii_alphanumeric(), "-")
    )
}

fn scroll_to(path: &str) {
    if let Some(el) = document().get_element_by_id(&file_id(path)) {
        el.scroll_into_view();
    }
}

/// Everything needed to draw a file's diff.
struct Layout {
    old_html: Vec<String>,
    new_html: Vec<String>,
    hunks: Vec<Hunk>,
    skipped_after: usize,
    /// Per change block: whether upstream made the same change.
    upstream: Vec<bool>,
}

fn text(b: &Option<Arc<Blob>>) -> Option<&str> {
    b.as_ref().and_then(|b| b.text.as_deref())
}

fn layout(file: &FileData) -> Layout {
    let (old, new) = (
        text(&file.old).unwrap_or_default(),
        text(&file.new).unwrap_or_default(),
    );
    let (old_lines, new_lines) = (rebase::lines(old), rebase::lines(new));
    let blocks = rebase::change_blocks(&old_lines, &new_lines);
    let upstream = match &file.upstream {
        Some((a, b)) => {
            let (ua, ub) = (text(a).unwrap_or_default(), text(b).unwrap_or_default());
            rebase::from_upstream(
                &old_lines,
                &new_lines,
                &blocks,
                &rebase::lines(ua),
                &rebase::lines(ub),
            )
        }
        None => vec![false; blocks.len()],
    };
    let aligned: Vec<Vec<Aligned>> = blocks
        .iter()
        .map(|b| inline::align(&old_lines, &new_lines, b))
        .collect();
    let hunks = diff::hunks(&blocks, &aligned, old_lines.len(), new_lines.len(), CONTEXT);
    let mut old_html = highlight_lines(file.change.old_path.as_deref().unwrap_or_default(), old);
    let mut new_html = highlight_lines(file.change.path(), new);
    for rows in &aligned {
        for r in rows {
            let (true, Some(o), Some(n)) = (r.paired, r.old, r.new) else {
                continue;
            };
            let (dels, inss) = inline::changed_ranges(old_lines[o], new_lines[n]);
            if let Some(h) = old_html.get_mut(o) {
                *h = mark(h, &dels, "word-del");
            }
            if let Some(h) = new_html.get_mut(n) {
                *h = mark(h, &inss, "word-add");
            }
        }
    }
    Layout {
        old_html,
        new_html,
        skipped_after: diff::skipped_after(&hunks, old_lines.len()),
        hunks,
        upstream,
    }
}

fn status_label(s: ChangeStatus) -> &'static str {
    match s {
        ChangeStatus::Added => "added",
        ChangeStatus::Deleted => "deleted",
        ChangeStatus::Modified => "modified",
        ChangeStatus::Renamed => "renamed",
        ChangeStatus::Copied => "copied",
        ChangeStatus::TypeChanged => "type changed",
    }
}

/// Counts added and removed lines without laying the diff out.
fn stats(file: &FileData) -> (usize, usize, bool) {
    let (old, new) = (
        text(&file.old).unwrap_or_default(),
        text(&file.new).unwrap_or_default(),
    );
    let (o, n) = (rebase::lines(old), rebase::lines(new));
    let blocks = rebase::change_blocks(&o, &n);
    let added = blocks.iter().map(|b| b.new.len()).sum();
    let removed = blocks.iter().map(|b| b.old.len()).sum();
    let all_upstream = match &file.upstream {
        // Upstream touched the file too, and nothing else changed in it
        // (e.g. an empty file that upstream added).
        Some(_) if blocks.is_empty() => true,
        Some((a, b)) => {
            let (ua, ub) = (text(a).unwrap_or_default(), text(b).unwrap_or_default());
            rebase::from_upstream(&o, &n, &blocks, &rebase::lines(ua), &rebase::lines(ub))
                .into_iter()
                .all(|m| m)
        }
        _ => false,
    };
    (added, removed, all_upstream)
}

#[component]
fn FileView(
    file: Arc<FileData>,
    comments: Vec<Placed>,
    side_by_side: Signal<bool>,
    inline_changes: Signal<bool>,
) -> impl IntoView {
    let c = &file.change;
    let binary = [&file.old, &file.new]
        .iter()
        .any(|b| b.as_ref().is_some_and(|b| b.text.is_none()));
    let (added, removed, all_upstream) = if binary { (0, 0, false) } else { stats(&file) };
    let expanded = RwSignal::new(!all_upstream && added + removed <= BIG_FILE);
    let title = match (c.status, &c.old_path, &c.new_path) {
        (ChangeStatus::Renamed | ChangeStatus::Copied, Some(o), Some(n)) => format!("{o} → {n}"),
        _ => c.path().to_owned(),
    };
    let comments = Arc::new(comments);
    let file_comments: Vec<_> = comments
        .iter()
        .filter(|p| p.anchor == Anchor::File)
        .cloned()
        .collect();
    let body = {
        let file = file.clone();
        let comments = comments.clone();
        // Computed once, on first expansion.
        let cache = StoredValue::new_local(None::<Arc<Layout>>);
        move || {
            if !expanded.get() {
                return ().into_any();
            }
            if binary {
                let size = |b: &Option<Arc<Blob>>| {
                    b.as_ref()
                        .map(|b| b.size.to_string())
                        .unwrap_or_else(|| "–".into())
                };
                return view! { <p class="binary muted">{format!("Binary file: {} → {} bytes", size(&file.old), size(&file.new))}</p> }.into_any();
            }
            let l = cache.with_value(|l| l.clone()).unwrap_or_else(|| {
                let l = Arc::new(layout(&file));
                cache.set_value(Some(l.clone()));
                l
            });
            if side_by_side.get() {
                split_view(&l, &comments, inline_changes).into_any()
            } else {
                unified_view(&l, &comments, inline_changes).into_any()
            }
        }
    };
    view! {
        <section class="file" id=file_id(c.path())>
            <header class="file-header" on:click=move |_| expanded.update(|e| *e = !*e)>
                <span class="chevron">{move || if expanded.get() { "▾" } else { "▸" }}</span>
                <span class=format!("status status-{}", status_label(c.status).replace(' ', "-"))>{status_label(c.status)}</span>
                <span class="path">{title}</span>
                {all_upstream.then(|| view! { <span class="badge upstream-badge">"only rebase changes"</span> })}
                <span class="stats"><span class="plus">{format!("+{added}")}</span>" "<span class="minus">{format!("−{removed}")}</span></span>
            </header>
            {(!file_comments.is_empty()).then(|| view! {
                <div class="file-comments">{file_comments.into_iter().map(|p| view! { <ThreadView thread=p.thread /> }).collect_view()}</div>
            })}
            {body}
        </section>
    }
}

/// The threads anchored on each line, and those on lines the diff doesn't
/// show.
struct LineComments<'a> {
    by_line: HashMap<(Side, u32), Vec<&'a Placed>>,
}

impl<'a> LineComments<'a> {
    fn new(comments: &'a [Placed]) -> Self {
        let mut by_line: HashMap<(Side, u32), Vec<&Placed>> = HashMap::new();
        for p in comments {
            if let Anchor::Line(side, line) = p.anchor {
                by_line.entry((side, line)).or_default().push(p);
            }
        }
        LineComments { by_line }
    }

    /// Takes the threads for a 0-based line.
    fn take(&mut self, side: Side, line: Option<usize>) -> Vec<&'a Placed> {
        line.and_then(|l| self.by_line.remove(&(side, l as u32 + 1)))
            .unwrap_or_default()
    }

    fn rest(self) -> Vec<(Side, u32, &'a Placed)> {
        let mut rest: Vec<_> = self
            .by_line
            .into_iter()
            .flat_map(|((side, line), ps)| ps.into_iter().map(move |p| (side, line, p)))
            .collect();
        rest.sort_by_key(|(side, line, _)| (*line, *side == Side::Right));
        rest
    }
}

fn threads_row(threads: Vec<&Placed>, colspan: u32) -> Option<AnyView> {
    (!threads.is_empty()).then(|| {
        view! {
            <tr class="comment-row">
                <td colspan=colspan>
                    {threads.into_iter().map(|p| view! { <ThreadView thread=p.thread.clone() /> }).collect_view()}
                </td>
            </tr>
        }
        .into_any()
    })
}

fn skip_row(n: usize, colspan: u32) -> Option<AnyView> {
    (n > 0).then(|| {
        view! { <tr class="skip"><td colspan=colspan>{format!("⋯ {n} unchanged lines")}</td></tr> }.into_any()
    })
}

fn outside_comments(rest: Vec<(Side, u32, &Placed)>) -> Option<AnyView> {
    (!rest.is_empty()).then(|| {
        view! {
            <div class="outside-comments">
                <p class="muted">"Comments on lines this diff doesn't show:"</p>
                {rest.into_iter().map(|(side, line, p)| view! {
                    <div class="outside">
                        <code class="muted">{format!("{} line {line}", if side == Side::Left { "old" } else { "new" })}</code>
                        <ThreadView thread=p.thread.clone() />
                    </div>
                }).collect_view()}
            </div>
        }
        .into_any()
    })
}

fn num(n: Option<usize>) -> String {
    n.map(|n| (n + 1).to_string()).unwrap_or_default()
}

fn html(lines: &[String], n: Option<usize>) -> String {
    n.and_then(|n| lines.get(n)).cloned().unwrap_or_default()
}

fn unified_view(
    l: &Layout,
    comments: &[Placed],
    inline_changes: Signal<bool>,
) -> impl IntoView + use<> {
    let mut lc = LineComments::new(comments);
    let mut rows: Vec<AnyView> = Vec::new();
    for h in &l.hunks {
        rows.extend(skip_row(h.skipped_before, 4));
        for line in &h.lines {
            let (old, new, kind, block, code) = match *line {
                Line::Context { old, new } => (
                    Some(old),
                    Some(new),
                    "ctx",
                    None,
                    html(&l.new_html, Some(new)),
                ),
                Line::Removed { old, block } => (
                    Some(old),
                    None,
                    "del",
                    Some(block),
                    html(&l.old_html, Some(old)),
                ),
                Line::Added { new, block } => (
                    None,
                    Some(new),
                    "add",
                    Some(block),
                    html(&l.new_html, Some(new)),
                ),
            };
            let upstream = block.is_some_and(|b| l.upstream[b]);
            let marker = match kind {
                "del" => "−",
                "add" => "+",
                _ => " ",
            };
            rows.push(
                view! {
                    <tr class=format!("line {kind}") class:upstream=upstream>
                        <td class="ln">{num(old)}</td>
                        <td class="ln">{num(new)}</td>
                        <td class="marker">{marker}</td>
                        <td class="code" inner_html=code></td>
                    </tr>
                }
                .into_any(),
            );
            let mut threads = lc.take(Side::Left, old);
            threads.extend(lc.take(Side::Right, new));
            rows.extend(threads_row(threads, 4));
        }
    }
    rows.extend(skip_row(l.skipped_after, 4));
    let outside = outside_comments(lc.rest());
    view! {
        <table class="diff unified" class:inline-changes=move || inline_changes.get()><tbody>{rows}</tbody></table>
        {outside}
    }
}

fn split_view(
    l: &Layout,
    comments: &[Placed],
    inline_changes: Signal<bool>,
) -> impl IntoView + use<> {
    let mut lc = LineComments::new(comments);
    let mut rows: Vec<AnyView> = Vec::new();
    for h in &l.hunks {
        rows.extend(skip_row(h.skipped_before, 4));
        for r in &h.rows {
            let upstream = r.block.is_some_and(|b| l.upstream[b]);
            let (lk, rk) = match (r.block, r.old, r.new) {
                (None, _, _) => ("ctx", "ctx"),
                (Some(_), o, n) => (
                    if o.is_some() { "del" } else { "blank" },
                    if n.is_some() { "add" } else { "blank" },
                ),
            };
            rows.push(
                view! {
                    <tr class="line" class:upstream=upstream>
                        <td class=format!("ln {lk}")>{num(r.old)}</td>
                        <td class=format!("code {lk}") inner_html=html(&l.old_html, r.old)></td>
                        <td class=format!("ln {rk}")>{num(r.new)}</td>
                        <td class=format!("code {rk}") inner_html=html(&l.new_html, r.new)></td>
                    </tr>
                }
                .into_any(),
            );
            let mut threads = lc.take(Side::Left, r.old);
            threads.extend(lc.take(Side::Right, r.new));
            rows.extend(threads_row(threads, 4));
        }
    }
    rows.extend(skip_row(l.skipped_after, 4));
    let outside = outside_comments(lc.rest());
    view! {
        <table class="diff split" class:inline-changes=move || inline_changes.get()><tbody>{rows}</tbody></table>
        {outside}
    }
}
