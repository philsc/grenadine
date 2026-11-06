//! The diff between two commits, computed in the browser from blobs the
//! server sends, and the diffs of an agent's file edits and worktree.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, OnceLock};

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
use crate::pr::{Anchor, Placed, Thread, ThreadView};

const CONTEXT: usize = 3;

/// That the lines of a PR's diff can be commented on, and which PR's.
#[derive(Clone)]
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
}

/// What `stats` works out: added and removed lines, change blocks, and
/// whether every changed line is the rebase's.
struct Stats {
    added: usize,
    removed: usize,
    blocks: usize,
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
/// left side of the diff. `None` when a side is binary.
fn rebased_blob(upstream: &BlobPair, old: &Option<Arc<Blob>>) -> Option<Arc<Blob>> {
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
    let mut merged_text = merged.lines.join("\n");
    let newline = match old.as_ref().and_then(|b| b.text.as_deref()) {
        Some(t) => t.ends_with('\n'),
        None => base_new_text.ends_with('\n'),
    };
    if newline && !merged_text.is_empty() {
        merged_text.push('\n');
    }
    Some(Arc::new(Blob {
        size: merged_text.len() as u64,
        text: Some(merged_text),
    }))
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
                        RebaseInfo {
                            rebased: rebased_blob(&upstream, &old),
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
    comments: Arc<HashMap<String, Vec<Placed>>>,
    side_by_side: Signal<bool>,
    inline_changes: Signal<bool>,
    rebase_older: Signal<bool>,
    #[prop(optional_no_strip)] commenting: Option<Commenting>,
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
            let comments = comments.clone();
            start_reveal();
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
                    let placed = comments.get(f.change.path()).cloned().unwrap_or_default();
                    view! { <FileView file=f.clone() comments=placed side_by_side=side_by_side inline_changes=inline_changes rebase_older=rebase_older commenting=commenting.clone() /> }
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

/// Everything needed to draw a file's diff.
struct Layout {
    shape: Arc<Shape>,
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

/// What a diff looks like apart from its lines' HTML, so that it is worked
/// out once whether the lines are highlighted yet or not.
struct Shape {
    hunks: Vec<Hunk>,
    skipped_after: usize,
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
    shape_lines(&old, &new, &blocks, old_kinds, new_kinds)
}

/// Works out the diff from `old` to `new` split into `blocks`, with the
/// per-line kinds given.
fn shape_lines(
    old: &[&str],
    new: &[&str],
    blocks: &[rebase::Block],
    old_kinds: Vec<rebase::Kind>,
    new_kinds: Vec<rebase::Kind>,
) -> Shape {
    let aligned: Vec<Vec<Aligned>> = blocks.iter().map(|b| inline::align(old, new, b)).collect();
    let hunks = diff::hunks(blocks, &aligned, old.len(), new.len(), CONTEXT);
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
        skipped_after: diff::skipped_after(&hunks, old.len()),
        hunks,
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
        &blocks,
        vec![rebase::Kind::Pr; old.len()],
        vec![rebase::Kind::Pr; new.len()],
    )
}

/// Lays out `shape` with the given lines' HTML, highlighted or plain.
fn layout(shape: Arc<Shape>, mut old_html: Vec<String>, mut new_html: Vec<String>) -> Layout {
    for m in &shape.marks {
        if let Some(h) = old_html.get_mut(m.old) {
            *h = mark(h, &m.dels, "word-del");
        }
        if let Some(h) = new_html.get_mut(m.new) {
            *h = mark(h, &m.inss, "word-add");
        }
    }
    Layout {
        shape,
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
    comments: Vec<Placed>,
    side_by_side: Signal<bool>,
    inline_changes: Signal<bool>,
    rebase_older: Signal<bool>,
    #[prop(optional_no_strip)] commenting: Option<Commenting>,
    #[prop(optional)] folds: Option<Folds>,
) -> impl IntoView {
    let c = file.change.clone();
    let fc = commenting.map(|commenting| FileCommenting {
        commenting,
        path: c.path().to_owned(),
        boxes: expect_context::<OpenBoxes>(),
    });
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
    let comments = Arc::new(comments);
    let file_comments: Vec<_> = comments
        .iter()
        .filter(|p| p.anchor == Anchor::File)
        .cloned()
        .collect();
    let body = {
        let file = file.clone();
        let comments = comments.clone();
        let fc = fc.clone();
        // Laid out plain on first expansion, and again once both sides are
        // highlighted or the rebase mode flips; the flags say which.
        let cache = StoredValue::new_local(None::<(bool, bool, Arc<Layout>)>);
        move || {
            if !expanded.get() {
                return ().into_any();
            }
            if !shown.get() {
                let style = format!("height: {}px", rows * ROW_HEIGHT);
                return view! { <div class="diff-placeholder" style=style></div> }.into_any();
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
            let cached = cache.with_value(|c| {
                c.as_ref()
                    .filter(|(h, m, _)| *h == highlighted && *m == mode)
                    .map(|(_, _, l)| l.clone())
            });
            let l = cached.unwrap_or_else(|| {
                let shape = cached_shape(&file, &left_lines.id, &new_lines.id, mode);
                let l = Arc::new(layout(shape, left_lines.html(), new_lines.html()));
                cache.set_value(Some((highlighted, mode, l.clone())));
                l
            });
            if side_by_side.get() {
                split_view(&l, &comments, inline_changes, fc.as_ref()).into_any()
            } else {
                unified_view(&l, &comments, inline_changes, fc.as_ref()).into_any()
            }
        }
    };
    view! {
        <section class="file" id=file_id(c.path()) prop:hidden={
            let f = file.clone();
            move || f.stats(rebase_older.get()).all_rebase
        }>
            <header class="file-header" on:click={
                let path = c.path().to_owned();
                move |_| {
                    expanded.update(|e| *e = !*e);
                    if let Some(folds) = &folds {
                        folds.set(&path, expanded.get_untracked());
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
                {fc.clone().map(|fc| {
                    let draft = fc.commenting.draft(&fc.path, None);
                    view! {
                        <button type="button" class="link file-comment-button" on:click=move |ev| {
                            // The header's own click folds the file.
                            ev.stop_propagation();
                            fc.boxes.set(&draft, true);
                        }>"Comment on file"</button>
                    }
                })}
            </header>
            {(!file_comments.is_empty()).then(|| view! {
                <div class="file-comments">{file_comments.into_iter().map(|p| view! { <ThreadView thread=p.thread /> }).collect_view()}</div>
            })}
            {fc.map(|fc| {
                let draft = fc.commenting.draft(&fc.path, None);
                move || fc.boxes.is_open(&draft).then(|| view! { <div class="file-comments">{fc.composer(None)}</div> })
            })}
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
                view! { <FileView file=file comments=vec![] side_by_side=side_by_side inline_changes=inline_changes rebase_older=rebase_older folds=folds.clone() /> }
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
                        highlight_lines(&path, &f.old),
                        highlight_lines(&path, &f.new),
                    )
                };
                unified_view(&l, &[], inline_changes, None)
            }).collect_view()}
        </div>
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

/// One row of a diff table, as HTML, and what goes under it.
struct Row {
    class: String,
    cells: String,
    threads: Vec<Arc<Thread>>,
    /// The lines (1-based) whose comment boxes go under the row.
    lines: Vec<(Side, u32)>,
}

impl Row {
    fn skip(n: usize, colspan: u32) -> Option<Row> {
        (n > 0).then(|| Row {
            class: "skip".to_owned(),
            cells: format!("<td colspan=\"{colspan}\">⋯ {n} unchanged lines</td>"),
            threads: Vec::new(),
            lines: Vec::new(),
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
            threads: threads.into_iter().map(|p| p.thread.clone()).collect(),
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
                            {r.threads.iter().map(|t| view! { <ThreadView thread=t.clone() /> }).collect_view()}
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

/// A diff table of `rows`. Clicking a commentable line number opens a
/// comment box under its row.
fn diff_table(
    class: &'static str,
    rows: Vec<Row>,
    inline_changes: Signal<bool>,
    fc: Option<FileCommenting>,
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
    inline_changes: Signal<bool>,
    fc: Option<&FileCommenting>,
) -> impl IntoView + use<> {
    let mut lc = LineComments::new(comments);
    let mut rows = Vec::new();
    for h in &l.shape.hunks {
        rows.extend(Row::skip(h.skipped_before, 4));
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
    rows.extend(Row::skip(l.shape.skipped_after, 4));
    let outside = outside_comments(lc.rest());
    view! {
        {diff_table("diff unified", rows, inline_changes, fc.cloned())}
        {outside}
    }
}

fn split_view(
    l: &Layout,
    comments: &[Placed],
    inline_changes: Signal<bool>,
    fc: Option<&FileCommenting>,
) -> impl IntoView + use<> {
    let mut lc = LineComments::new(comments);
    let mut rows = Vec::new();
    for h in &l.shape.hunks {
        rows.extend(Row::skip(h.skipped_before, 4));
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
    rows.extend(Row::skip(l.shape.skipped_after, 4));
    let outside = outside_comments(lc.rest());
    view! {
        {diff_table("diff split", rows, inline_changes, fc.cloned())}
        {outside}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(lines: &[(Side, u32)], threads: usize) -> Row {
        let thread = |id| {
            Arc::new(Thread {
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
            })
        };
        Row {
            class: "line".into(),
            cells: String::new(),
            threads: (0..threads as u64).map(thread).collect(),
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
}
