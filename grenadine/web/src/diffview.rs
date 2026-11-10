//! The diff between two commits, computed in the browser from blobs the
//! server sends, and the diffs of an agent's file edits and worktree.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use grenadine_core::api::{Blob, ChangeStatus, FileChange, NewThread, PrKey, Side, WorktreeFile};
use grenadine_core::diff::{self, Hunk, Line};
use grenadine_core::edit::Fragment;
use grenadine_core::inline::{self, Aligned};
use grenadine_core::rebase;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::api;
use crate::composer::{Composer, OpenBoxes, sender};
use crate::highlight::{self, Highlighted, highlight_lines, mark, plain_lines};
use crate::inboxes::load_flag;
use crate::pr::{Anchor, Placed, ThreadView, range_label};

const CONTEXT: usize = 3;

/// That the lines of a PR's diff can be commented on, and which PR's.
#[derive(Clone, PartialEq)]
pub struct Commenting {
    pub key: PrKey,
}

impl Commenting {
    /// The name of the draft of a comment on `line` (1-based) of `side`,
    /// or on the whole file.
    fn draft(&self, path: &str, at: Option<(Side, u32)>) -> String {
        let at = match at {
            Some((Side::Left, l)) => format!("L{l}"),
            Some((Side::Right, l)) => format!("R{l}"),
            None => "file".to_owned(),
        };
        format!("{}{at}", self.draft_prefix(path))
    }

    /// What the names of the drafts of comments on `path` start with.
    fn draft_prefix(&self, path: &str) -> String {
        format!("line.{}#{}:{path}:", self.key.repo, self.key.number)
    }
}

/// A file's comment hooks: where its comments go, and the boxes they're
/// written in.
#[derive(Clone)]
struct FileCommenting {
    commenting: Commenting,
    path: String,
    boxes: OpenBoxes,
}

impl FileCommenting {
    /// The lines (1-based) whose comment boxes are open; tracked.
    fn open_lines(&self) -> HashSet<(Side, u32)> {
        let prefix = self.commenting.draft_prefix(&self.path);
        self.boxes
            .open_with_prefix(&prefix)
            .iter()
            .filter_map(|name| {
                let at = name.strip_prefix(&prefix)?;
                let side = match at.get(..1)? {
                    "L" => Side::Left,
                    "R" => Side::Right,
                    _ => return None,
                };
                Some((side, at[1..].parse().ok()?))
            })
            .collect()
    }

    /// Opens the comment box for the line whose number cell is in or under
    /// the click's target.
    fn open_clicked(&self, ev: &leptos::ev::MouseEvent) {
        let Some(cell) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|t| t.closest("td.commentable").ok().flatten())
        else {
            return;
        };
        let side = match cell.get_attribute("data-side").as_deref() {
            Some("L") => Side::Left,
            Some("R") => Side::Right,
            _ => return,
        };
        let Some(line) = cell.get_attribute("data-line").and_then(|l| l.parse().ok()) else {
            return;
        };
        self.boxes
            .set(&self.commenting.draft(&self.path, Some((side, line))), true);
    }

    /// A row with the comment box for `at`, shown while it's open.
    fn box_row(&self, at: Option<(Side, u32)>, colspan: u32) -> AnyView {
        let draft = self.commenting.draft(&self.path, at);
        let boxes = self.boxes;
        let this = self.clone();
        (move || {
            boxes.is_open(&draft).then(|| {
                view! {
                    <tr class="comment-row composer-row">
                        <td colspan=colspan>{this.composer(at)}</td>
                    </tr>
                }
            })
        })
        .into_any()
    }

    /// The box for a comment on `at`, or on the whole file.
    fn composer(&self, at: Option<(Side, u32)>) -> impl IntoView + use<> {
        let draft = self.commenting.draft(&self.path, at);
        let (key, path) = (self.commenting.key.clone(), self.path.clone());
        let send = sender(move |body| {
            let key = key.clone();
            let thread = NewThread {
                path: path.clone(),
                side: at.map_or(Side::Right, |(s, _)| s),
                line: at.map(|(_, l)| l),
                body,
            };
            async move { api::add_review_thread(&key, &thread).await }
        });
        view! {
            <Composer
                draft=draft
                repo=self.commenting.key.repo.clone()
                label="Add to review"
                placeholder=if at.is_some() { "Comment on this line…" } else { "Comment on this file…" }
                send=send
                autofocus=true
            />
        }
    }
}

/// Files with more changed lines than this start collapsed.
const BIG_FILE: usize = 3000;

/// The old and new blob of one side of a file change.
type BlobPair = (Option<Arc<Blob>>, Option<Arc<Blob>>);

/// What a rebase did to a file upstream touched, worked out once per diff.
struct RebaseInfo {
    /// The file's upstream change between the two bases.
    upstream: BlobPair,
    /// The older version's changes replayed onto the newer base; a
    /// synthetic blob with no id.
    rebased: Option<Arc<Blob>>,
    /// Per line of the older version's text: its 0-based line in
    /// `rebased`, `None` when the merge dropped it.
    ours_to_merged: Option<OursToMerged>,
}

/// Where each line of the older version's text landed in the merged
/// text; `None` when the merge dropped it.
type OursToMerged = Arc<[Option<usize>]>;

/// What `stats` works out: added and removed lines, change blocks, the
/// side-by-side row count, and whether every changed line is the rebase's.
struct Stats {
    added: usize,
    removed: usize,
    blocks: usize,
    /// The rows the side-by-side view needs for the changed lines: each
    /// block's taller side.
    split_rows: usize,
    all_rebase: bool,
}

struct FileData {
    change: FileChange,
    old: Option<Arc<Blob>>,
    new: Option<Arc<Blob>>,
    /// What the rebase did to this file, when the diff spans one and
    /// upstream touched it.
    rebase: Option<RebaseInfo>,
    /// The stats per mode, filled on first use. Files with no rebase info
    /// look the same in both modes and share slot 0.
    stats: [OnceLock<Stats>; 2],
}

impl FileData {
    /// The diff's stats in `rebase_older` mode, computed once per mode.
    fn stats(&self, rebase_older: bool) -> &Stats {
        let slot = (self.rebase.is_some() && rebase_older) as usize;
        self.stats[slot].get_or_init(|| stats(self, rebase_older))
    }
}

/// The older version's text merged onto the newer base, for the rebased
/// left side of the diff, and where each of the older version's lines
/// landed in it. `None` when a side is binary.
fn rebased_blob(upstream: &BlobPair, old: &Option<Arc<Blob>>) -> Option<(Arc<Blob>, OursToMerged)> {
    if [&upstream.0, &upstream.1, old]
        .into_iter()
        .any(|b| b.as_ref().is_some_and(|b| b.text.is_none()))
    {
        return None;
    }
    let base_old = rebase::lines(text(&upstream.0).unwrap_or_default());
    let base_new_text = text(&upstream.1).unwrap_or_default();
    let base_new = rebase::lines(base_new_text);
    let old_text = text(old).unwrap_or_default();
    let merged = rebase::merge3(&base_old, &rebase::lines(old_text), &base_new);
    let ours_to_merged: OursToMerged = merged.ours_to_merged.into();
    let mut merged_text = merged.lines.join("\n");
    let newline = match old.as_ref().and_then(|b| b.text.as_deref()) {
        Some(t) => t.ends_with('\n'),
        None => base_new_text.ends_with('\n'),
    };
    if newline && !merged_text.is_empty() {
        merged_text.push('\n');
    }
    Some((
        Arc::new(Blob {
            size: merged_text.len() as u64,
            text: Some(merged_text),
        }),
        ours_to_merged,
    ))
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
                let old = blob(&f.old_blob);
                Arc::new(FileData {
                    change: f.clone(),
                    old: old.clone(),
                    new: blob(&f.new_blob),
                    rebase: up_by_path.get(f.path()).map(|u| {
                        let upstream = (blob(&u.old_blob), blob(&u.new_blob));
                        let (rebased, ours_to_merged) = rebased_blob(&upstream, &old)
                            .map(|(b, m)| (Some(b), Some(m)))
                            .unwrap_or_default();
                        RebaseInfo {
                            rebased,
                            ours_to_merged,
                            upstream,
                        }
                    }),
                    stats: [OnceLock::new(), OnceLock::new()],
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
    #[prop(into)] comments: Signal<Arc<HashMap<String, Vec<Placed>>>>,
    side_by_side: Signal<bool>,
    inline_changes: Signal<bool>,
    rebase_older: Signal<bool>,
    #[prop(into)] commenting: Signal<Option<Commenting>>,
) -> impl IntoView {
    let has_upstream = upstream.is_some();
    // What the last diff still had queued can wait for this one's files.
    highlight::demote();
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
            })}</p> }
            .into_any()
        }
        Some(Ok((files, hidden))) => {
            start_reveal();
            provide_context(Measurer::new());
            let files = Arc::new(files);
            // Files with no change of the PR's own — which depends on the
            // mode — join the files the rebase alone changed.
            view! {
                <div class="diff-summary muted">
                    {{
                        let files = files.clone();
                        move || format!("{} files changed", files.len() - hidden_by_rebase(&files, rebase_older.get()))
                    }}
                    {{
                        let files = files.clone();
                        move || {
                            let total = hidden + hidden_by_rebase(&files, rebase_older.get());
                            (total > 0).then(|| view! {
                                <span>{format!(" · {total} files changed only by the rebase are hidden")}</span>
                            })
                        }
                    }}
                    {has_upstream.then(|| view! {
                        <span class="legend"><span class="upstream-swatch"></span>" changes brought in by the rebase"</span>
                        <span class="legend"><span class="mixed-swatch"></span>" changes mixed with the rebase"</span>
                    })}
                </div>
                {{
                    let files = files.clone();
                    move || {
                        let total = hidden + hidden_by_rebase(&files, rebase_older.get());
                        (files.len() == hidden_by_rebase(&files, rebase_older.get())).then(|| view! {
                            <p class="empty">{format!("No changes.{}", if total > 0 {
                                format!(" {total} files changed only by the rebase are hidden.")
                            } else {
                                String::new()
                            })}</p>
                        })
                    }
                }}
                <ul class="file-index">
                    {{
                        let files = files.clone();
                        move || files
                            .iter()
                            .filter(|f| !f.stats(rebase_older.get()).all_rebase)
                            .map(|f| {
                                let path = f.change.path().to_owned();
                                view! { <li><a href="#" on:click=move |ev| { ev.prevent_default(); scroll_to(&path) }>{f.change.path().to_owned()}</a></li> }
                            })
                            .collect_view()
                    }}
                </ul>
                {files.iter().map(|f| {
                    // The file's threads; a Memo so only a file whose
                    // threads changed re-renders.
                    let placed = {
                        let f = f.clone();
                        Memo::new(move |_| {
                            let comments = comments.get();
                            let mut placed =
                                comments.get(f.change.path()).cloned().unwrap_or_default();
                            // Threads made on the diff's left version are keyed by
                            // the file's path as of that version: a renamed file's
                            // old path.
                            if let Some(old) = f.change.old_path.as_deref() && old != f.change.path()
                            {
                                placed.extend(
                                    comments
                                        .get(old)
                                        .into_iter()
                                        .flatten()
                                        .filter(|p| {
                                            matches!(
                                                p.anchor,
                                                Anchor::Line(Side::Left, _) | Anchor::File
                                            )
                                        })
                                        .cloned(),
                                );
                            }
                            placed
                        })
                    };
                    view! { <FileView file=f.clone() comments=placed side_by_side=side_by_side inline_changes=inline_changes rebase_older=rebase_older commenting=commenting /> }
                }).collect_view()}
            }
            .into_any()
        }
    }
}

/// How many of `files` have no change of the PR's own in `rebase_older`
/// mode; they hide and count with the rebase-only files.
fn hidden_by_rebase(files: &[Arc<FileData>], rebase_older: bool) -> usize {
    files
        .iter()
        .filter(|f| f.stats(rebase_older).all_rebase)
        .count()
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

/// Scrolls `path`'s file to its scroll-margin offset and keeps scrolling
/// until it stays there: for a while after a collapse the layout keeps
/// moving the file, since files the browser skipped lay out at their real
/// heights once they're near the viewport. User input stops it.
fn rescroll_to(path: String) {
    rescroll_step(path, 60, 0, Rc::new(RescrollGuard::new()));
}

fn rescroll_step(path: String, tries: u8, stable: u8, guard: Rc<RescrollGuard>) {
    if guard.cancelled.get() {
        return;
    }
    let Some(section) = document().get_element_by_id(&file_id(&path)) else {
        return;
    };
    let Some(main) = section.closest(".main").ok().flatten() else {
        return;
    };
    let margin = window()
        .get_computed_style(&section)
        .ok()
        .flatten()
        .and_then(|s| s.get_property_value("scroll-margin-top").ok())
        .and_then(|v| v.strip_suffix("px").and_then(|v| v.parse().ok()))
        .unwrap_or(0.0);
    let top = section.get_bounding_client_rect().top();
    let target = main.get_bounding_client_rect().top() + margin;
    let stable = if (top - target).abs() <= 1.0 {
        stable + 1
    } else {
        section.scroll_into_view();
        0
    };
    if stable >= 8 || tries == 0 {
        return;
    }
    set_timeout(
        move || rescroll_step(path, tries - 1, stable, guard),
        Duration::from_millis(50),
    );
}

/// The user scroll intents that stop a `rescroll_to` loop. Dropping the
/// guard removes the listeners.
struct RescrollGuard {
    cancelled: Rc<Cell<bool>>,
    handles: Vec<WindowListenerHandle>,
}

impl RescrollGuard {
    fn new() -> Self {
        let cancelled = Rc::new(Cell::new(false));
        let handles = ["wheel", "touchstart", "keydown", "mousedown"]
            .into_iter()
            .map(|event| {
                let cancelled = cancelled.clone();
                window_event_listener_untyped(event, move |_| cancelled.set(true))
            })
            .collect();
        Self { cancelled, handles }
    }
}

impl Drop for RescrollGuard {
    fn drop(&mut self) {
        for handle in self.handles.drain(..) {
            handle.remove();
        }
    }
}

/// Everything needed to draw a file's diff.
struct Layout {
    shape: Arc<Shape>,
    hunks: Vec<Hunk>,
    skipped_after: Option<diff::Skip>,
    old_html: Vec<String>,
    new_html: Vec<String>,
    /// The file lines that the first old and new line are, when the texts
    /// are only part of the file.
    old_start: usize,
    new_start: usize,
}

fn text(b: &Option<Arc<Blob>>) -> Option<&str> {
    b.as_ref().and_then(|b| b.text.as_deref())
}

/// What a diff looks like apart from its lines' HTML and its expanded
/// gaps, so that it is worked out once whether the lines are highlighted
/// yet or not.
struct Shape {
    blocks: Vec<rebase::Block>,
    aligned: Vec<Vec<Aligned>>,
    old_len: usize,
    new_len: usize,
    /// Per old and new line: whether the change is the PR's, the rebase's,
    /// or the two mixed. Unchanged lines are `Kind::Pr`.
    old_kinds: Vec<rebase::Kind>,
    new_kinds: Vec<rebase::Kind>,
    marks: Vec<Mark>,
}

/// A paired line's old and new indexes and the char ranges that changed on
/// each side.
struct Mark {
    old: usize,
    new: usize,
    dels: Vec<Range<usize>>,
    inss: Vec<Range<usize>>,
}

/// A shape's sides' blob IDs.
type ShapeKey = (String, String);

/// How many shapes are kept.
const SHAPES: usize = 500;

thread_local! {
    /// Shapes by their sides' blob IDs, oldest first.
    static SHAPE_CACHE: RefCell<VecDeque<(ShapeKey, Arc<Shape>)>> = RefCell::default();
}

/// The file's shape, kept for the life of the page when the file has no
/// upstream change, so that coming back to a diff doesn't redo it. Files
/// with rebase info differ between the modes and are never cached.
fn cached_shape(file: &FileData, old_id: &str, new_id: &str, rebase_older: bool) -> Arc<Shape> {
    if file.rebase.is_some() {
        return Arc::new(shape(file, rebase_older));
    }
    let key = (old_id.to_owned(), new_id.to_owned());
    if let Some(s) =
        SHAPE_CACHE.with_borrow(|c| c.iter().find(|(k, _)| *k == key).map(|(_, s)| s.clone()))
    {
        return s;
    }
    let s = Arc::new(shape(file, rebase_older));
    SHAPE_CACHE.with_borrow_mut(|c| {
        if c.len() >= SHAPES {
            c.pop_front();
        }
        c.push_back((key, s.clone()));
    });
    s
}

/// The old text `rebase_older` picks: the rebased text when it exists,
/// else the older version's own.
fn old_text(file: &FileData, rebase_older: bool) -> &str {
    if rebase_older && let Some(t) = file.rebase.as_ref().and_then(|i| text(&i.rebased)) {
        return t;
    }
    text(&file.old).unwrap_or_default()
}

/// The per-line kinds of the file's diff in `rebase_older` mode, the diff
/// being `old` to `new` split into `blocks`.
fn line_kinds(
    file: &FileData,
    old: &[&str],
    new: &[&str],
    blocks: &[rebase::Block],
    rebase_older: bool,
) -> (Vec<rebase::Kind>, Vec<rebase::Kind>) {
    let Some(info) = &file.rebase else {
        return (
            vec![rebase::Kind::Pr; old.len()],
            vec![rebase::Kind::Pr; new.len()],
        );
    };
    let base_old = rebase::lines(text(&info.upstream.0).unwrap_or_default());
    let base_new = rebase::lines(text(&info.upstream.1).unwrap_or_default());
    if !rebase_older {
        let kinds = rebase::classify(&base_old, old, &base_new, new);
        return (kinds.old, kinds.new);
    }
    let v2 = rebase::lines(text(&file.old).unwrap_or_default());
    let merged = rebase::merge3(&base_old, &v2, &base_new);
    let block_kinds = rebase::rebased_block_kinds(&merged, new, blocks, &base_old, &base_new);
    let mut old_kinds = vec![rebase::Kind::Pr; old.len()];
    let mut new_kinds = vec![rebase::Kind::Pr; new.len()];
    for (k, b) in blocks.iter().enumerate() {
        for i in b.old.clone() {
            old_kinds[i] = block_kinds[k];
        }
        for i in b.new.clone() {
            new_kinds[i] = block_kinds[k];
        }
    }
    (old_kinds, new_kinds)
}

fn shape(file: &FileData, rebase_older: bool) -> Shape {
    let old = rebase::lines(old_text(file, rebase_older));
    let new = rebase::lines(text(&file.new).unwrap_or_default());
    let blocks = rebase::change_blocks(&old, &new);
    let (old_kinds, new_kinds) = line_kinds(file, &old, &new, &blocks, rebase_older);
    shape_lines(&old, &new, blocks, old_kinds, new_kinds)
}

/// Works out the diff from `old` to `new` split into `blocks`, with the
/// per-line kinds given.
fn shape_lines(
    old: &[&str],
    new: &[&str],
    blocks: Vec<rebase::Block>,
    old_kinds: Vec<rebase::Kind>,
    new_kinds: Vec<rebase::Kind>,
) -> Shape {
    let aligned: Vec<Vec<Aligned>> = blocks.iter().map(|b| inline::align(old, new, b)).collect();
    let marks = aligned
        .iter()
        .flatten()
        .filter_map(|r| match (r.paired, r.old, r.new) {
            (true, Some(o), Some(n)) => {
                let (dels, inss) = inline::changed_ranges(old[o], new[n]);
                Some(Mark {
                    old: o,
                    new: n,
                    dels,
                    inss,
                })
            }
            _ => None,
        })
        .collect();
    Shape {
        blocks,
        aligned,
        old_len: old.len(),
        new_len: new.len(),
        old_kinds,
        new_kinds,
        marks,
    }
}

/// Works out the diff of two texts without rebase info.
fn shape_texts(old: &str, new: &str) -> Shape {
    let (old, new) = (rebase::lines(old), rebase::lines(new));
    let blocks = rebase::change_blocks(&old, &new);
    shape_lines(
        &old,
        &new,
        blocks,
        vec![rebase::Kind::Pr; old.len()],
        vec![rebase::Kind::Pr; new.len()],
    )
}

/// Lays out `shape` with the given lines' HTML, highlighted or plain, and
/// the gaps' expansion state.
fn layout(
    shape: Arc<Shape>,
    expand: &[diff::Expand],
    mut old_html: Vec<String>,
    mut new_html: Vec<String>,
) -> Layout {
    for m in &shape.marks {
        if let Some(h) = old_html.get_mut(m.old) {
            *h = mark(h, &m.dels, "word-del");
        }
        if let Some(h) = new_html.get_mut(m.new) {
            *h = mark(h, &m.inss, "word-add");
        }
    }
    let hunks = diff::hunks(
        &shape.blocks,
        &shape.aligned,
        shape.old_len,
        shape.new_len,
        CONTEXT,
        expand,
    );
    let skipped_after = diff::skipped_after(&shape.blocks, expand, shape.old_len, CONTEXT);
    Layout {
        shape,
        hunks,
        skipped_after,
        old_html,
        new_html,
        old_start: 0,
        new_start: 0,
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

/// How many rows of diffs render in one task; files past that wait for a
/// later one, so the first ones paint without waiting for the rest.
const ROWS_PER_TASK: usize = 500;

/// The height of a diff row, in pixels, as the stylesheet sets it.
const ROW_HEIGHT: usize = 20;

/// The height of a file's header including its bottom border, in pixels,
/// as the stylesheet sets it.
const HEADER_HEIGHT: usize = 33;

/// The diff rows a file shows: the changed lines plus the context around
/// each block and its hidden-lines row. A collapsed file shows none.
fn estimated_rows(stats: &Stats, expanded: bool, side_by_side: bool) -> usize {
    if !expanded {
        return 0;
    }
    let changed = if side_by_side {
        stats.split_rows
    } else {
        stats.added + stats.removed
    };
    changed + stats.blocks * (2 * CONTEXT + 1)
}

/// The pixel height a file's section takes: the header plus its rows.
fn estimated_height(stats: &Stats, expanded: bool, side_by_side: bool) -> usize {
    HEADER_HEIGHT + estimated_rows(stats, expanded, side_by_side) * ROW_HEIGHT
}

/// Resolves on the next animation frame.
async fn animation_frame() {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = window().request_animation_frame(&resolve);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// A file waiting for its measuring turn.
struct Queued {
    rows: usize,
    measuring: RwSignal<bool>,
    /// Set while the file sits in the queue so its triggers don't enqueue
    /// it twice; cleared once its turn comes.
    enqueued: Rc<Cell<bool>>,
}

struct MeasurerInner {
    /// Files waiting to be measured, in order.
    queue: VecDeque<Queued>,
    /// The width the last settled resize left.
    width: Cell<f64>,
    running: Cell<bool>,
}

thread_local! {
    /// The measuring state of each live measurer, by id.
    static MEASURERS: RefCell<HashMap<usize, MeasurerInner>> = RefCell::default();
    static NEXT_MEASURER: Cell<usize> = const { Cell::new(0) };
}

/// Lays files out in the background so the browser records their real
/// heights: `content-visibility: auto` remembers a laid-out file's size,
/// which keeps the scrollable height — and the scrollbar — steady.
/// Without it, off-screen files count as their estimated size until
/// scrolled into view.
#[derive(Clone, Copy)]
struct Measurer {
    /// Which `MEASURERS` entry the queue lives in.
    id: usize,
    /// Bumps when the window's width changed, after the resize events
    /// settle; the files track it as a trigger.
    width_epoch: RwSignal<usize>,
}

impl Measurer {
    /// A measurer for one diff. Its window resize listener bumps the
    /// width epoch 200ms after the last event when the width changed, so
    /// the files re-measure at the new width; cleanup stops the loop and
    /// removes the listener.
    fn new() -> Self {
        let id = NEXT_MEASURER.replace(NEXT_MEASURER.get() + 1);
        let width_epoch = RwSignal::new(0);
        MEASURERS.with_borrow_mut(|m| {
            m.insert(
                id,
                MeasurerInner {
                    queue: VecDeque::new(),
                    width: Cell::new(
                        window()
                            .inner_width()
                            .ok()
                            .and_then(|w| w.as_f64())
                            .unwrap_or_default(),
                    ),
                    running: Cell::new(false),
                },
            );
        });
        let resizes = Arc::new(AtomicU32::new(0));
        let on_resize = window_event_listener_untyped("resize", move |_| {
            let n = resizes.fetch_add(1, Ordering::Relaxed) + 1;
            let resizes = resizes.clone();
            set_timeout(
                move || {
                    let alive = MEASURERS.with_borrow(|m| m.contains_key(&id));
                    if !alive || resizes.load(Ordering::Relaxed) != n {
                        return;
                    }
                    let width = window()
                        .inner_width()
                        .ok()
                        .and_then(|w| w.as_f64())
                        .unwrap_or_default();
                    let changed = MEASURERS.with_borrow_mut(|m| {
                        m.get_mut(&id).is_some_and(|i| {
                            let changed = width != i.width.get();
                            if changed {
                                i.width.set(width);
                            }
                            changed
                        })
                    });
                    if changed {
                        width_epoch.try_update(|e| *e += 1);
                    }
                },
                Duration::from_millis(200),
            );
        });
        on_cleanup(move || {
            MEASURERS.with_borrow_mut(|m| m.remove(&id));
            on_resize.remove();
        });
        Measurer { id, width_epoch }
    }

    /// Queues `measuring` to be set for two frames so the browser lays
    /// the file out and records its height, with files totalling up to
    /// `ROWS_PER_TASK` rows measured per turn.
    fn enqueue(&self, rows: usize, measuring: RwSignal<bool>, enqueued: Rc<Cell<bool>>) {
        let id = self.id;
        let spawn = MEASURERS.with_borrow_mut(|m| {
            let Some(inner) = m.get_mut(&id) else {
                return false;
            };
            inner.queue.push_back(Queued {
                rows,
                measuring,
                enqueued,
            });
            !inner.running.replace(true)
        });
        if !spawn {
            return;
        }
        leptos::task::spawn_local(async move {
            loop {
                if !MEASURERS.with_borrow(|m| m.contains_key(&id)) {
                    break;
                }
                let mut chunk = Vec::new();
                let mut budget = ROWS_PER_TASK;
                MEASURERS.with_borrow_mut(|m| {
                    let Some(inner) = m.get_mut(&id) else {
                        return;
                    };
                    // A file bigger than a whole turn's budget still
                    // measures, on its own.
                    while inner
                        .queue
                        .front()
                        .is_some_and(|q| q.rows <= budget || chunk.is_empty())
                    {
                        let q = inner.queue.pop_front().unwrap();
                        budget = budget.saturating_sub(q.rows);
                        chunk.push(q);
                    }
                });
                if chunk.is_empty() {
                    break;
                }
                for q in &chunk {
                    q.measuring.try_set(true);
                    q.enqueued.set(false);
                }
                // Two frames: the first lays the files out, the second is
                // past the observer that records their sizes.
                animation_frame().await;
                animation_frame().await;
                for q in &chunk {
                    q.measuring.try_set(false);
                }
            }
            MEASURERS.with_borrow_mut(|m| {
                if let Some(i) = m.get_mut(&id) {
                    i.running.set(false);
                }
            });
        });
    }
}

thread_local! {
    /// Files waiting to render: their row counts and their flags.
    static REVEAL: RefCell<VecDeque<(usize, RwSignal<bool>)>> = RefCell::default();
    /// The rows that may still render in the current task.
    static BUDGET: Cell<usize> = const { Cell::new(ROWS_PER_TASK) };
    static REVEALING: Cell<bool> = const { Cell::new(false) };
}

/// Starts over for a new diff: forgets the files the last one still had
/// waiting, and gives the current task a full budget.
fn start_reveal() {
    REVEAL.with_borrow_mut(VecDeque::clear);
    BUDGET.set(ROWS_PER_TASK);
}

/// Whether a file of `rows` rows renders now. If not, `shown` is set once
/// its turn comes, after the files before it.
fn reveal(rows: usize, shown: RwSignal<bool>) -> bool {
    let budget = BUDGET.get();
    // A file bigger than a whole task's budget still renders, on its own.
    if REVEAL.with_borrow(VecDeque::is_empty) && (rows <= budget || budget == ROWS_PER_TASK) {
        BUDGET.set(budget.saturating_sub(rows));
        return true;
    }
    REVEAL.with_borrow_mut(|q| q.push_back((rows, shown)));
    if !REVEALING.replace(true) {
        leptos::task::spawn_local(async {
            loop {
                highlight::next_task().await;
                BUDGET.set(ROWS_PER_TASK);
                while let Some((rows, shown)) = REVEAL.with_borrow_mut(|q| {
                    let budget = BUDGET.get();
                    let fits = q
                        .front()
                        .is_some_and(|&(rows, _)| rows <= budget || budget == ROWS_PER_TASK);
                    fits.then(|| q.pop_front()).flatten()
                }) {
                    BUDGET.set(BUDGET.get().saturating_sub(rows));
                    shown.try_set(true);
                }
                if REVEAL.with_borrow(VecDeque::is_empty) {
                    REVEALING.set(false);
                    return;
                }
            }
        });
    }
    false
}

/// Counts added and removed lines and change blocks without laying the
/// diff out, and whether every changed line is the rebase's —
/// `rebase_older` picks which diff is counted.
fn stats(file: &FileData, rebase_older: bool) -> Stats {
    let binary = [&file.old, &file.new]
        .iter()
        .any(|b| b.as_ref().is_some_and(|b| b.text.is_none()));
    if binary {
        return Stats {
            added: 0,
            removed: 0,
            blocks: 0,
            split_rows: 0,
            all_rebase: false,
        };
    }
    let o = rebase::lines(old_text(file, rebase_older));
    let n = rebase::lines(text(&file.new).unwrap_or_default());
    let blocks = rebase::change_blocks(&o, &n);
    let added = blocks.iter().map(|b| b.new.len()).sum();
    let removed = blocks.iter().map(|b| b.old.len()).sum();
    let (old_kinds, new_kinds) = line_kinds(file, &o, &n, &blocks, rebase_older);
    // Only a file upstream touched can be wholly the rebase's.
    let all_rebase = file.rebase.is_some()
        && blocks.iter().all(|b| {
            b.old.clone().all(|i| old_kinds[i] == rebase::Kind::Rebase)
                && b.new.clone().all(|i| new_kinds[i] == rebase::Kind::Rebase)
        });
    Stats {
        added,
        removed,
        blocks: blocks.len(),
        split_rows: blocks.iter().map(|b| b.old.len().max(b.new.len())).sum(),
        all_rebase,
    }
}

/// One side of a file, highlighted in the background.
struct SideLines {
    path: String,
    /// The blob ID, or a hash of the contents when there is none.
    id: String,
    /// `None` for a missing or binary side.
    blob: Option<Arc<Blob>>,
    lines: RwSignal<Option<Highlighted>>,
}

impl SideLines {
    /// Queues the highlighting right away if `expanded`; otherwise it waits
    /// until the file is.
    fn new(path: &str, id: &Option<String>, blob: &Option<Arc<Blob>>, expanded: bool) -> Self {
        let blob = blob.clone().filter(|b| b.text.is_some());
        let id = id.clone().unwrap_or_else(|| {
            let mut h = DefaultHasher::new();
            text(&blob).hash(&mut h);
            format!("{:016x}", h.finish())
        });
        let lines = match &blob {
            None => Some(Arc::new(Vec::new())),
            Some(_) => highlight::cached(path, &id),
        };
        let side = SideLines {
            path: path.to_owned(),
            id,
            blob,
            lines: RwSignal::new(lines),
        };
        if expanded {
            side.request(true);
        }
        side
    }

    /// Whether the highlighted lines are in; tracked.
    fn ready(&self) -> bool {
        self.lines.with(Option::is_some)
    }

    /// Queues the highlighting, unless it's done.
    fn request(&self, urgent: bool) {
        let Some(blob) = &self.blob else { return };
        if self.lines.with_untracked(Option::is_some) {
            return;
        }
        let lines = self.lines;
        highlight::request(&self.path, &self.id, blob.clone(), urgent, move |h| {
            lines.try_set(Some(h));
        });
    }

    /// The lines' HTML: highlighted if ready, plain otherwise.
    fn html(&self) -> Vec<String> {
        match self.lines.get_untracked() {
            Some(h) => (*h).clone(),
            None => plain_lines(text(&self.blob).unwrap_or_default()),
        }
    }
}

/// The user's per-path fold choices for one agent session, kept in the
/// tab's session storage as a JSON map so that they survive reloads.
#[derive(Clone)]
pub struct Folds {
    key: String,
}

impl Folds {
    pub fn new(session: &str) -> Self {
        Self {
            key: format!("worktree-folds.{session}"),
        }
    }

    fn read(&self) -> HashMap<String, bool> {
        window()
            .session_storage()
            .ok()
            .flatten()
            .and_then(|s| s.get_item(&self.key).ok().flatten())
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    pub fn get(&self, path: &str) -> Option<bool> {
        self.read().get(path).copied()
    }

    pub fn set(&self, path: &str, expanded: bool) {
        let Some(storage) = window().session_storage().ok().flatten() else {
            return;
        };
        let mut folds = self.read();
        folds.insert(path.to_owned(), expanded);
        if let Ok(json) = serde_json::to_string(&folds) {
            let _ = storage.set_item(&self.key, &json);
        }
    }
}

/// The status the file header shows. In the rebased mode the left side is
/// the merged text: a file the merge produced that the newer version also
/// has is modified, whatever the raw statuses say.
fn status_shown(file: &FileData, rebase_older: bool) -> &'static str {
    if rebase_older && file.rebase.is_some() {
        let Some(left) = file.rebase.as_ref().and_then(|i| text(&i.rebased)) else {
            return status_label(file.change.status);
        };
        return match (!left.is_empty(), file.new.is_some()) {
            (true, true) => "modified",
            (false, true) => "added",
            (_, false) => "deleted",
        };
    }
    status_label(file.change.status)
}

#[component]
fn FileView(
    file: Arc<FileData>,
    #[prop(into)] comments: Signal<Vec<Placed>>,
    side_by_side: Signal<bool>,
    inline_changes: Signal<bool>,
    rebase_older: Signal<bool>,
    #[prop(into)] commenting: Signal<Option<Commenting>>,
    #[prop(optional)] folds: Option<Folds>,
) -> impl IntoView {
    let c = file.change.clone();
    // Pages that only show diffs, like an agent's worktree, have no
    // comment boxes and no context for them either.
    let boxes = use_context::<OpenBoxes>();
    let path = c.path().to_owned();
    let fc = move || {
        Option::zip(commenting.get(), boxes).map(|(commenting, boxes)| FileCommenting {
            commenting,
            path: path.clone(),
            boxes,
        })
    };
    let binary = [&file.old, &file.new]
        .iter()
        .any(|b| b.as_ref().is_some_and(|b| b.text.is_none()));
    let initial = file.stats(rebase_older.get_untracked());
    let expanded = RwSignal::new(
        folds
            .as_ref()
            .and_then(|f| f.get(c.path()))
            .unwrap_or(!initial.all_rebase && initial.added + initial.removed <= BIG_FILE),
    );
    // Roughly: the changed lines and the context around each block.
    let rows = if expanded.get_untracked() {
        initial.added + initial.removed + initial.blocks * (2 * CONTEXT + 1)
    } else {
        0
    };
    let shown = RwSignal::new(false);
    shown.set(reveal(rows, shown));
    // The measurer lays the file out off-screen so its real height is
    // recorded; the triggers enqueue it once per layout change.
    let measuring = RwSignal::new(false);
    if let Some(measurer) = use_context::<Measurer>() {
        let file = file.clone();
        let enqueued = Rc::new(Cell::new(false));
        Effect::new(move |_| {
            let mode = rebase_older.get();
            let stats = file.stats(mode);
            // Hidden files have no box to measure, and ones still waiting
            // to render just a placeholder would measure that.
            let needs = shown.get() && !stats.all_rebase;
            expanded.track();
            side_by_side.track();
            inline_changes.track();
            measurer.width_epoch.track();
            comments.track();
            if !needs || enqueued.replace(true) {
                return;
            }
            measurer.enqueue(
                estimated_rows(
                    stats,
                    expanded.get_untracked(),
                    side_by_side.get_untracked(),
                ),
                measuring,
                enqueued.clone(),
            );
        });
    }
    let old_path = c.old_path.clone().unwrap_or_else(|| c.path().to_owned());
    let old_lines = SideLines::new(&old_path, &c.old_blob, &file.old, expanded.get_untracked());
    let new_lines = SideLines::new(c.path(), &c.new_blob, &file.new, expanded.get_untracked());
    let rebased_lines = file
        .rebase
        .as_ref()
        .and_then(|i| i.rebased.clone())
        .map(|b| SideLines::new(&old_path, &None, &Some(b), expanded.get_untracked()));
    let title = match (c.status, &c.old_path, &c.new_path) {
        (ChangeStatus::Renamed | ChangeStatus::Copied, Some(o), Some(n)) => format!("{o} → {n}"),
        _ => c.path().to_owned(),
    };
    let file_comments = move || {
        let file_comments: Vec<_> = comments
            .get()
            .iter()
            .filter(|p| p.anchor == Anchor::File)
            .cloned()
            .collect();
        (!file_comments.is_empty()).then(|| {
            view! {
                <div class="file-comments">{file_comments.into_iter().map(|p| view! { <ThreadView thread=p.thread /> }).collect_view()}</div>
            }
        })
    };
    // The lines revealed by the file's "expand" buttons, per gap.
    let expand = RwSignal::new(vec![diff::Expand::default(); initial.blocks + 1]);
    let body = {
        let file = file.clone();
        let fc = fc.clone();
        // Laid out plain on first expansion, and again once both sides are
        // highlighted, the rebase mode flips, or the gaps' expansion
        // changes; the key says which.
        let cache = StoredValue::new_local(None::<(bool, bool, Vec<diff::Expand>, Arc<Layout>)>);
        // The mode the expansion belongs to; it resets when the mode
        // changes the diff's blocks.
        let expand_mode = StoredValue::new_local(None::<bool>);
        move || {
            if !expanded.get() {
                return ().into_any();
            }
            if !shown.get() {
                let height =
                    estimated_rows(file.stats(rebase_older.get()), true, side_by_side.get())
                        * ROW_HEIGHT;
                return view! { <div class="diff-placeholder" style=format!("height: {height}px")></div> }
                    .into_any();
            }
            if binary {
                let size = |b: &Option<Arc<Blob>>| {
                    b.as_ref()
                        .map(|b| b.size.to_string())
                        .unwrap_or_else(|| "–".into())
                };
                return view! { <p class="binary muted">{format!("Binary file: {} → {} bytes", size(&file.old), size(&file.new))}</p> }.into_any();
            }
            let mode = rebase_older.get();
            // The other mode reshapes the diff; the gaps' expansion
            // doesn't carry over.
            if expand_mode.get_value() != Some(mode) {
                expand_mode.set_value(Some(mode));
                expand.set(vec![diff::Expand::default(); file.stats(mode).blocks + 1]);
            }
            // The left side is the rebased text in rebased mode, the older
            // version's own blob otherwise.
            let left_lines = match mode.then_some(rebased_lines.as_ref()).flatten() {
                Some(l) => l,
                None => &old_lines,
            };
            let highlighted = left_lines.ready() && new_lines.ready();
            if !highlighted {
                left_lines.request(true);
                new_lines.request(true);
            }
            let exp = expand.get();
            let cached = cache.with_value(|c| {
                c.as_ref()
                    .filter(|(h, m, e, _)| *h == highlighted && *m == mode && *e == exp)
                    .map(|(_, _, _, l)| l.clone())
            });
            let l = cached.unwrap_or_else(|| {
                let shape = cached_shape(&file, &left_lines.id, &new_lines.id, mode);
                let l = Arc::new(layout(shape, &exp, left_lines.html(), new_lines.html()));
                cache.set_value(Some((highlighted, mode, exp.clone(), l.clone())));
                l
            });
            // In rebased mode the left side's lines are the merged text:
            // threads anchored to the older version's lines move along.
            let map = mode
                .then(|| file.rebase.as_ref().and_then(|i| i.ours_to_merged.clone()))
                .flatten();
            let placed_src = comments.get();
            let (placed, dropped) = remap_left(&placed_src, map.as_deref());
            let fc = fc();
            if side_by_side.get() {
                split_view(
                    &l,
                    &placed,
                    &dropped,
                    inline_changes,
                    fc.as_ref(),
                    Some(expand),
                )
                .into_any()
            } else {
                unified_view(
                    &l,
                    &placed,
                    &dropped,
                    inline_changes,
                    fc.as_ref(),
                    Some(expand),
                )
                .into_any()
            }
        }
    };
    view! {
        <section class="file" id=file_id(c.path()) class:measuring=measuring prop:hidden={
            let f = file.clone();
            move || f.stats(rebase_older.get()).all_rebase
        } style:contain-intrinsic-size={
            let f = file.clone();
            move || format!(
                "auto {}px",
                estimated_height(f.stats(rebase_older.get()), expanded.get(), side_by_side.get())
            )
        }>
            <header class="file-header" on:click={
                let path = c.path().to_owned();
                move |_| {
                    // The header is stuck when the file's top has scrolled
                    // above it. Collapsing such a file would leave the view
                    // somewhere in the files below, so the collapsed file is
                    // scrolled back to the top.
                    let section = document().get_element_by_id(&file_id(&path));
                    let stuck = section
                        .as_ref()
                        .and_then(|s| {
                            s.query_selector(".file-header").ok().flatten().map(|h| {
                                s.get_bounding_client_rect().top()
                                    < h.get_bounding_client_rect().top()
                            })
                        })
                        .unwrap_or(false);
                    let collapsing = expanded.get_untracked();
                    expanded.update(|e| *e = !*e);
                    if let Some(folds) = &folds {
                        folds.set(&path, expanded.get_untracked());
                    }
                    if collapsing && stuck {
                        rescroll_to(path.clone());
                    }
                }
            }>
                <span class="chevron">{move || if expanded.get() { "▾" } else { "▸" }}</span>
                <span class={
                    let f = file.clone();
                    move || format!("status status-{}", status_shown(&f, rebase_older.get()).replace(' ', "-"))
                }>{{
                    let f = file.clone();
                    move || status_shown(&f, rebase_older.get())
                }}</span>
                <span class="path">{title}</span>
                <span class="stats"><span class="plus">{{
                    let f = file.clone();
                    move || format!("+{}", f.stats(rebase_older.get()).added)
                }}</span>" "<span class="minus">{{
                    let f = file.clone();
                    move || format!("−{}", f.stats(rebase_older.get()).removed)
                }}</span></span>
                {{
                    let fc = fc.clone();
                    move || {
                        fc().map(|fc| {
                            let draft = fc.commenting.draft(&fc.path, None);
                            view! {
                                <button type="button" class="link file-comment-button" on:click=move |ev| {
                                    // The header's own click folds the file.
                                    ev.stop_propagation();
                                    fc.boxes.set(&draft, true);
                                }>"Comment on file"</button>
                            }
                        })
                    }
                }}
            </header>
            {file_comments}
            {move || {
                fc().map(|fc| {
                    let draft = fc.commenting.draft(&fc.path, None);
                    move || fc.boxes.is_open(&draft).then(|| view! { <div class="file-comments">{fc.composer(None)}</div> })
                })
            }}
            {body}
        </section>
    }
}

/// The unstaged changes in an agent's worktree, laid out like a PR's
/// unified diff. A file keeps its view until its contents change, and its
/// fold is remembered per path for the session in this tab.
#[component]
pub fn WorktreeDiff(files: Signal<Vec<WorktreeFile>>, session: String) -> impl IntoView {
    let inline_changes = Signal::stored(load_flag("inline-changes", true));
    let side_by_side = Signal::stored(false);
    let rebase_older = Signal::stored(false);
    let folds = Folds::new(&session);
    provide_context(Measurer::new());
    let key = |f: &WorktreeFile| {
        let mut h = DefaultHasher::new();
        f.hash(&mut h);
        h.finish()
    };
    view! {
        <For each=move || files.get() key=key let:f>
            {
                let file = Arc::new(FileData {
                    change: f.change,
                    old: f.old.map(Arc::new),
                    new: f.new.map(Arc::new),
                    rebase: None,
                    stats: [OnceLock::new(), OnceLock::new()],
                });
                view! { <FileView file=file comments=Signal::stored(vec![]) side_by_side=side_by_side inline_changes=inline_changes rebase_older=rebase_older commenting=Signal::stored(None) folds=folds.clone() /> }
            }
        </For>
    }
}

/// The change an agent's file tool made to `path`, laid out like a PR's
/// unified diff.
#[component]
pub fn EditDiff(path: String, fragments: Vec<Fragment>) -> impl IntoView {
    let inline_changes = Signal::stored(load_flag("inline-changes", true));
    view! {
        <div class="edit-diff">
            {fragments.iter().map(|f| {
                let l = Layout {
                    old_start: f.old_start,
                    new_start: f.new_start,
                    ..layout(
                        Arc::new(shape_texts(&f.old, &f.new)),
                        &[],
                        highlight_lines(&path, &f.old),
                        highlight_lines(&path, &f.new),
                    )
                };
                unified_view(&l, &[], &[], inline_changes, None, None)
            }).collect_view()}
        </div>
    }
}

/// The threads of `comments` with their left-side lines moved onto the
/// merged text `map` describes, and the ones whose line the merge
/// dropped, still at their original line.
fn remap_left(comments: &[Placed], map: Option<&[Option<usize>]>) -> (Vec<Placed>, Vec<Placed>) {
    let Some(map) = map else {
        return (comments.to_vec(), Vec::new());
    };
    let mut placed = Vec::new();
    let mut dropped = Vec::new();
    for p in comments {
        let mut p = p.clone();
        if let Anchor::Line(Side::Left, l) = p.anchor {
            match map.get(l.saturating_sub(1) as usize).copied().flatten() {
                Some(m) => p.anchor = Anchor::Line(Side::Left, m as u32 + 1),
                None => {
                    dropped.push(p);
                    continue;
                }
            }
        }
        if let Some((Side::Left, l)) = p.start {
            // A start the merge dropped leaves a single-line thread.
            p.start = map
                .get(l.saturating_sub(1) as usize)
                .copied()
                .flatten()
                .map(|m| (Side::Left, m as u32 + 1));
        }
        placed.push(p);
    }
    (placed, dropped)
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

/// One row of a diff table, as HTML, and what goes under it.
struct Row {
    class: String,
    cells: String,
    threads: Vec<Placed>,
    /// The lines (1-based) whose comment boxes go under the row.
    lines: Vec<(Side, u32)>,
}

impl Row {
    /// A hidden stretch's row: how many lines it hides and, unless
    /// `expandable` is false, the buttons that reveal them. `gaps` is the
    /// file's gap count, so the first and last gaps offer only the
    /// direction that has lines to reveal.
    fn skip(skip: &diff::Skip, gaps: usize, colspan: u32, expandable: bool) -> Option<Row> {
        let n = skip.old.len();
        (n > 0).then(|| {
            let mut buttons = String::new();
            if expandable {
                let (first, last) = (skip.gap == 0, skip.gap == gaps - 1);
                // Gap k is after block k - 1 and before block k; a file
                // with no blocks only expands down from its start.
                let dirs: &[&str] = if n <= EXPAND_LINES {
                    &["all"]
                } else if last {
                    &["down"]
                } else if first {
                    &["up"]
                } else {
                    &["up", "down"]
                };
                for dir in dirs {
                    let (glyph, label) = match *dir {
                        "up" => ("↑", "Expand up"),
                        "down" => ("↓", "Expand down"),
                        _ => ("↕", "Expand all"),
                    };
                    let _ = write!(
                        buttons,
                        "<button type=\"button\" class=\"expand\" data-gap=\"{}\" data-dir=\"{dir}\" \
                         title=\"{label}\" aria-label=\"{label}\">{glyph}</button>",
                        skip.gap
                    );
                }
            }
            Row {
                class: "skip".to_owned(),
                cells: format!("<td colspan=\"{colspan}\">{buttons}⋯ {n} unchanged lines</td>"),
                threads: Vec::new(),
                lines: Vec::new(),
            }
        })
    }

    /// A row of lines; the threads are for any of `at`, and so are the
    /// comment boxes.
    fn new(
        class: String,
        cells: String,
        threads: Vec<&Placed>,
        at: &[(Side, Option<usize>)],
    ) -> Row {
        Row {
            class,
            cells,
            threads: threads.into_iter().cloned().collect(),
            lines: at
                .iter()
                .filter_map(|&(side, line)| Some((side, line? as u32 + 1)))
                .collect(),
        }
    }
}

/// A diff table's row group: a run of plain rows, which goes in as HTML that
/// the browser parses much faster than it could be built element by
/// element, or a row with threads or open comment boxes under it, which are
/// components.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Group {
    Plain(usize, usize),
    Row(usize),
}

/// The row groups of `rows`, given the lines with open comment boxes.
fn groups(rows: &[Row], open: &HashSet<(Side, u32)>) -> Vec<Group> {
    let mut groups = Vec::new();
    let mut start = 0;
    for (i, r) in rows.iter().enumerate() {
        if !r.threads.is_empty() || r.lines.iter().any(|l| open.contains(l)) {
            if start < i {
                groups.push(Group::Plain(start, i));
            }
            groups.push(Group::Row(i));
            start = i + 1;
        }
    }
    if start < rows.len() {
        groups.push(Group::Plain(start, rows.len()));
    }
    groups
}

/// A thread's range label for its header, like "Lines +26 to +29";
/// `None` for single-line and file-level threads.
fn thread_lines(p: &Placed) -> Option<String> {
    match (p.start, p.anchor) {
        (Some(s), Anchor::Line(side, line)) => Some(range_label(s, (side, line))),
        _ => None,
    }
}

fn group_view(rows: &[Row], group: Group, fc: Option<&FileCommenting>) -> AnyView {
    match group {
        Group::Plain(start, end) => {
            let mut html = String::new();
            for r in &rows[start..end] {
                let _ = write!(html, "<tr class=\"{}\">{}</tr>", r.class, r.cells);
            }
            view! { <tbody inner_html=html></tbody> }.into_any()
        }
        Group::Row(i) => {
            let r = &rows[i];
            let threads = (!r.threads.is_empty()).then(|| {
                view! {
                    <tr class="comment-row">
                        <td colspan=4>
                            {r.threads.iter().map(|p| view! { <ThreadView thread=p.thread.clone() lines=thread_lines(p) /> }).collect_view()}
                        </td>
                    </tr>
                }
            });
            let boxes = fc.map(|fc| {
                r.lines
                    .iter()
                    .map(|&at| fc.box_row(Some(at), 4))
                    .collect_view()
            });
            view! {
                <tbody>
                    <tr class=r.class.clone() inner_html=r.cells.clone()></tr>
                    {threads}
                    {boxes}
                </tbody>
            }
            .into_any()
        }
    }
}

/// The lines an "expand" button reveals per click.
const EXPAND_LINES: usize = 20;

/// A diff table of `rows`. Clicking a commentable line number opens a
/// comment box under its row; clicking an "expand" button reveals hidden
/// lines through `expand`.
fn diff_table(
    class: &'static str,
    rows: Vec<Row>,
    inline_changes: Signal<bool>,
    fc: Option<FileCommenting>,
    expand: Option<RwSignal<Vec<diff::Expand>>>,
) -> impl IntoView + use<> {
    let rows = Arc::new(rows);
    let each = {
        let (rows, fc) = (rows.clone(), fc.clone());
        move || {
            let open = fc
                .as_ref()
                .map(FileCommenting::open_lines)
                .unwrap_or_default();
            groups(&rows, &open)
        }
    };
    let on_click = {
        let fc = fc.clone();
        move |ev: leptos::ev::MouseEvent| {
            let button = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .and_then(|t| t.closest("button[data-gap]").ok().flatten());
            if let (Some(expand), Some(button)) = (expand, button) {
                if let Some(gap) = button
                    .get_attribute("data-gap")
                    .and_then(|g| g.parse::<usize>().ok())
                {
                    expand.update(|e| {
                        if let Some(e) = e.get_mut(gap) {
                            match button.get_attribute("data-dir").as_deref() {
                                Some("up") => e.above_next += EXPAND_LINES,
                                Some("down") => e.below_prev += EXPAND_LINES,
                                _ => {
                                    e.below_prev = usize::MAX;
                                    e.above_next = usize::MAX;
                                }
                            }
                        }
                    });
                }
                ev.stop_propagation();
                return;
            }
            if let Some(fc) = &fc {
                fc.open_clicked(&ev);
            }
        }
    };
    view! {
        <table class=class class:inline-changes=move || inline_changes.get() on:click=on_click>
            <For each=each key=|g| *g children=move |g| group_view(&rows, g, fc.as_ref()) />
        </table>
    }
}

/// Marks the rows each unresolved multi-line thread spans with a
/// `commented-range` class. A start the diff doesn't show marks only the
/// end's row; a thread whose end isn't shown marks nothing.
fn mark_ranges(rows: &mut [Row], comments: &[Placed]) {
    for p in comments {
        let (Some(start), Anchor::Line(end_side, end_line)) = (p.start, p.anchor) else {
            continue;
        };
        if p.thread.root.resolved {
            continue;
        }
        let Some(end) = rows
            .iter()
            .position(|r| r.lines.contains(&(end_side, end_line)))
        else {
            continue;
        };
        let first = rows
            .iter()
            .position(|r| r.lines.contains(&start))
            .filter(|&i| i <= end)
            .unwrap_or(end);
        for r in &mut rows[first..=end] {
            if !r.class.split(' ').any(|c| c == "skip") {
                r.class.push_str(" commented-range");
            }
        }
    }
}

/// The line number cell for 0-based `line` of `side`. Clicking a
/// commentable one opens a comment box on the line.
fn ln_cell(
    fc: Option<&FileCommenting>,
    class: &str,
    side: Side,
    line: Option<usize>,
    shown: String,
) -> String {
    match (fc, line) {
        (Some(_), Some(l)) => {
            let side = if side == Side::Left { "L" } else { "R" };
            format!(
                "<td class=\"{class} commentable\" title=\"Comment on this line\" \
                 data-side=\"{side}\" data-line=\"{}\">{shown}</td>",
                l + 1
            )
        }
        _ => format!("<td class=\"{class}\">{shown}</td>"),
    }
}

/// The threads `lc` didn't place plus `dropped`, in line order.
fn rest_and_dropped<'a>(
    lc: LineComments<'a>,
    dropped: &'a [Placed],
) -> Vec<(Side, u32, &'a Placed)> {
    let mut rest = lc.rest();
    rest.extend(dropped.iter().filter_map(|p| match p.anchor {
        Anchor::Line(side, line) => Some((side, line, p)),
        _ => None,
    }));
    rest.sort_by_key(|(side, line, _)| (*line, *side == Side::Right));
    rest
}

fn outside_comments(rest: Vec<(Side, u32, &Placed)>) -> Option<AnyView> {
    (!rest.is_empty()).then(|| {
        view! {
            <div class="outside-comments">
                <p class="muted">"Comments on lines this diff doesn't show:"</p>
                {rest.into_iter().map(|(side, line, p)| view! {
                    <div class="outside">
                        <code class="muted">{format!("{} line {line}", if side == Side::Left { "old" } else { "new" })}</code>
                        <ThreadView thread=p.thread.clone() lines=thread_lines(p) />
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

fn html(lines: &[String], n: Option<usize>) -> &str {
    n.and_then(|n| lines.get(n)).map_or("", String::as_str)
}

/// The extra class a row or cell of a changed line of `kind` gets.
fn kind_class(kind: rebase::Kind) -> &'static str {
    match kind {
        rebase::Kind::Rebase => " upstream",
        rebase::Kind::Mixed => " mixed",
        rebase::Kind::Pr => "",
    }
}

fn unified_view(
    l: &Layout,
    comments: &[Placed],
    dropped: &[Placed],
    inline_changes: Signal<bool>,
    fc: Option<&FileCommenting>,
    expand: Option<RwSignal<Vec<diff::Expand>>>,
) -> impl IntoView + use<> {
    let mut lc = LineComments::new(comments);
    let mut rows = Vec::new();
    let gaps = l.shape.blocks.len() + 1;
    let expandable = expand.is_some();
    for h in &l.hunks {
        if let Some(skip) = &h.skipped_before {
            rows.extend(Row::skip(skip, gaps, 4, expandable));
        }
        for line in &h.lines {
            let (old, new, kind, line_kind, code) = match *line {
                Line::Context { old, new } => (
                    Some(old),
                    Some(new),
                    "ctx",
                    rebase::Kind::Pr,
                    html(&l.new_html, Some(new)),
                ),
                Line::Removed { old, .. } => (
                    Some(old),
                    None,
                    "del",
                    l.shape.old_kinds[old],
                    html(&l.old_html, Some(old)),
                ),
                Line::Added { new, .. } => (
                    None,
                    Some(new),
                    "add",
                    l.shape.new_kinds[new],
                    html(&l.new_html, Some(new)),
                ),
            };
            let shaded = kind_class(line_kind);
            let marker = match kind {
                "del" => "−",
                "add" => "+",
                _ => " ",
            };
            let cells = format!(
                "{}{}<td class=\"marker\">{marker}</td><td class=\"code{shaded}\">{code}</td>",
                ln_cell(fc, "ln", Side::Left, old, num(old.map(|o| o + l.old_start))),
                ln_cell(
                    fc,
                    "ln",
                    Side::Right,
                    new,
                    num(new.map(|n| n + l.new_start))
                ),
            );
            let mut threads = lc.take(Side::Left, old);
            threads.extend(lc.take(Side::Right, new));
            rows.push(Row::new(
                format!("line {kind}{shaded}"),
                cells,
                threads,
                &[(Side::Left, old), (Side::Right, new)],
            ));
        }
    }
    if let Some(skip) = &l.skipped_after {
        rows.extend(Row::skip(skip, gaps, 4, expandable));
    }
    mark_ranges(&mut rows, comments);
    let outside = outside_comments(rest_and_dropped(lc, dropped));
    view! {
        {diff_table("diff unified", rows, inline_changes, fc.cloned(), expand)}
        {outside}
    }
}

fn split_view(
    l: &Layout,
    comments: &[Placed],
    dropped: &[Placed],
    inline_changes: Signal<bool>,
    fc: Option<&FileCommenting>,
    expand: Option<RwSignal<Vec<diff::Expand>>>,
) -> impl IntoView + use<> {
    let mut lc = LineComments::new(comments);
    let mut rows = Vec::new();
    let gaps = l.shape.blocks.len() + 1;
    let expandable = expand.is_some();
    for h in &l.hunks {
        if let Some(skip) = &h.skipped_before {
            rows.extend(Row::skip(skip, gaps, 4, expandable));
        }
        for r in &h.rows {
            // Each side's line has its own kind; a row is upstream when
            // every changed side is, and mixed when any side is.
            let ok = r.old.map(|i| l.shape.old_kinds[i]);
            let nk = r.new.map(|i| l.shape.new_kinds[i]);
            let sides: Vec<rebase::Kind> = [ok, nk].into_iter().flatten().collect();
            let shaded = if sides.contains(&rebase::Kind::Mixed) {
                " mixed"
            } else if !sides.is_empty() && sides.iter().all(|k| *k == rebase::Kind::Rebase) {
                " upstream"
            } else {
                ""
            };
            let (lk, rk) = match (r.block, r.old, r.new) {
                (None, _, _) => ("ctx", "ctx"),
                (Some(_), o, n) => (
                    if o.is_some() { "del" } else { "blank" },
                    if n.is_some() { "add" } else { "blank" },
                ),
            };
            let cells = format!(
                "{}<td class=\"code {lk}{}\">{}</td>{}<td class=\"code {rk}{}\">{}</td>",
                ln_cell(fc, &format!("ln {lk}"), Side::Left, r.old, num(r.old)),
                kind_class(ok.unwrap_or(rebase::Kind::Pr)),
                html(&l.old_html, r.old),
                ln_cell(fc, &format!("ln {rk}"), Side::Right, r.new, num(r.new)),
                kind_class(nk.unwrap_or(rebase::Kind::Pr)),
                html(&l.new_html, r.new),
            );
            let mut threads = lc.take(Side::Left, r.old);
            threads.extend(lc.take(Side::Right, r.new));
            rows.push(Row::new(
                format!("line{shaded}"),
                cells,
                threads,
                &[(Side::Left, r.old), (Side::Right, r.new)],
            ));
        }
    }
    if let Some(skip) = &l.skipped_after {
        rows.extend(Row::skip(skip, gaps, 4, expandable));
    }
    mark_ranges(&mut rows, comments);
    let outside = outside_comments(rest_and_dropped(lc, dropped));
    view! {
        {diff_table("diff split", rows, inline_changes, fc.cloned(), expand)}
        {outside}
    }
}

#[cfg(test)]
mod tests {
    use crate::pr::Thread;

    use super::*;

    fn placed(id: u64, anchor: Anchor, start: Option<(Side, u32)>) -> Placed {
        Placed {
            anchor,
            start,
            thread: Arc::new(Thread {
                root: grenadine_core::api::ReviewComment {
                    id,
                    in_reply_to: None,
                    author: "a".into(),
                    body: "b".into(),
                    path: "f.rs".into(),
                    original_commit: "c".into(),
                    original_line: None,
                    original_start_line: None,
                    line: None,
                    start_line: None,
                    start_side: None,
                    side: Side::Right,
                    on_file: false,
                    created_at: String::new(),
                    url: String::new(),
                    resolved: false,
                    resolved_by: None,
                    thread_id: None,
                    can_resolve: false,
                    pending: false,
                    node_id: None,
                },
                replies: Vec::new(),
            }),
        }
    }

    fn row(lines: &[(Side, u32)], threads: usize) -> Row {
        Row {
            class: "line".into(),
            cells: String::new(),
            threads: (0..threads as u64)
                .map(|id| placed(id, Anchor::File, None))
                .collect(),
            lines: lines.to_vec(),
        }
    }

    #[test]
    fn rows_with_threads_or_open_boxes_get_groups_of_their_own() {
        let rows = [
            row(&[(Side::Left, 1), (Side::Right, 1)], 0),
            row(&[(Side::Right, 2)], 1),
            row(&[(Side::Right, 3)], 0),
            row(&[(Side::Left, 4)], 0),
            row(&[], 0),
        ];
        assert_eq!(
            groups(&rows, &HashSet::new()),
            [Group::Plain(0, 1), Group::Row(1), Group::Plain(2, 5)]
        );
        let open = HashSet::from([(Side::Left, 4), (Side::Left, 1)]);
        assert_eq!(
            groups(&rows, &open),
            [
                Group::Row(0),
                Group::Row(1),
                Group::Plain(2, 3),
                Group::Row(3),
                Group::Plain(4, 5)
            ]
        );
    }

    #[test]
    fn remap_left_remaps_and_drops_starts() {
        // Merged lines 1 and 5 stand where lines 1 and 3 were; line 2 is
        // gone.
        let map: Vec<Option<usize>> = vec![Some(0), None, Some(4)];
        let comments = [
            placed(1, Anchor::Line(Side::Left, 3), Some((Side::Left, 1))),
            // The start the merge dropped leaves a single-line thread.
            placed(2, Anchor::Line(Side::Left, 3), Some((Side::Left, 2))),
            // The end the merge dropped drops the thread.
            placed(3, Anchor::Line(Side::Left, 2), Some((Side::Left, 1))),
            // A right-side range isn't touched.
            placed(4, Anchor::Line(Side::Right, 9), Some((Side::Right, 8))),
        ];
        let (placed_out, dropped) = remap_left(&comments, Some(&map));
        assert_eq!(
            placed_out
                .iter()
                .map(|p| (p.thread.root.id, p.anchor, p.start))
                .collect::<Vec<_>>(),
            [
                (1, Anchor::Line(Side::Left, 5), Some((Side::Left, 1))),
                (2, Anchor::Line(Side::Left, 5), None),
                (4, Anchor::Line(Side::Right, 9), Some((Side::Right, 8))),
            ]
        );
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].thread.root.id, 3);
    }

    #[test]
    fn remap_left_without_a_map_is_a_noop() {
        let comments = [placed(
            1,
            Anchor::Line(Side::Left, 3),
            Some((Side::Left, 1)),
        )];
        let (placed_out, dropped) = remap_left(&comments, None);
        assert_eq!(placed_out, comments);
        assert!(dropped.is_empty());
    }

    fn marked(rows: &[Row]) -> Vec<bool> {
        rows.iter()
            .map(|r| r.class.contains("commented-range"))
            .collect()
    }

    fn skip_row() -> Row {
        Row {
            class: "skip".into(),
            ..row(&[], 0)
        }
    }

    #[test]
    fn mark_ranges_marks_the_rows_a_thread_spans() {
        let mut rows = vec![
            row(&[(Side::Right, 25)], 0),
            row(&[(Side::Right, 26)], 0),
            row(&[(Side::Right, 27)], 0),
            row(&[(Side::Right, 28)], 0),
            row(&[(Side::Right, 29)], 0),
            row(&[(Side::Right, 30)], 0),
        ];
        mark_ranges(
            &mut rows,
            &[placed(
                1,
                Anchor::Line(Side::Right, 29),
                Some((Side::Right, 26)),
            )],
        );
        assert_eq!(marked(&rows), [false, true, true, true, true, false]);
    }

    #[test]
    fn mark_ranges_marks_a_cross_side_range() {
        let mut rows = vec![
            row(&[(Side::Left, 10), (Side::Right, 12)], 0),
            row(&[(Side::Left, 11)], 0),
            row(&[(Side::Right, 14)], 0),
        ];
        mark_ranges(
            &mut rows,
            &[placed(
                1,
                Anchor::Line(Side::Right, 14),
                Some((Side::Left, 10)),
            )],
        );
        assert_eq!(marked(&rows), [true, true, true]);
    }

    #[test]
    fn mark_ranges_skips_resolved_and_unshown_starts() {
        let mut resolved = placed(1, Anchor::Line(Side::Right, 4), Some((Side::Right, 1)));
        Arc::get_mut(&mut resolved.thread).unwrap().root.resolved = true;
        let mut rows = vec![
            row(&[(Side::Right, 1)], 0),
            skip_row(),
            row(&[(Side::Right, 3)], 0),
            row(&[(Side::Right, 4)], 0),
            row(&[(Side::Right, 5)], 0),
        ];
        mark_ranges(
            &mut rows,
            &[
                resolved,
                // A range spanning a skip row doesn't mark it.
                placed(2, Anchor::Line(Side::Right, 4), Some((Side::Right, 1))),
                // A range whose start is hidden marks only its end.
                placed(3, Anchor::Line(Side::Right, 5), Some((Side::Right, 2))),
            ],
        );
        assert_eq!(marked(&rows), [true, false, true, true, true]);
    }
}
