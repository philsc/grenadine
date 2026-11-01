//! The diff between two commits, computed in the browser from blobs the
//! server sends, and the diffs of an agent's file edits and worktree.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use grenadine_core::api::{Blob, ChangeStatus, FileChange, NewThread, PrKey, Side, WorktreeFile};
use grenadine_core::diff::{self, Hunk, Line};
use grenadine_core::edit::Fragment;
use grenadine_core::inline::{self, Aligned};
use grenadine_core::rebase;
use leptos::prelude::*;

use crate::api;
use crate::composer::{Composer, OpenBoxes, sender};
use crate::highlight::{self, Highlighted, highlight_lines, mark, plain_lines};
use crate::inboxes::load_flag;
use crate::pr::{Anchor, Placed, ThreadView};

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
        format!("line.{}#{}:{path}:{at}", self.key.repo, self.key.number)
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
    /// The line number cell for 0-based `line` of `side`, which opens a
    /// comment box on the line when clicked.
    fn ln(
        this: Option<&Self>,
        class: String,
        side: Side,
        line: Option<usize>,
        shown: String,
    ) -> AnyView {
        match (this, line) {
            (Some(fc), Some(l)) => {
                let draft = fc.commenting.draft(&fc.path, Some((side, l as u32 + 1)));
                let boxes = fc.boxes;
                view! {
                    <td class=format!("{class} commentable") title="Comment on this line"
                        on:click=move |_| boxes.set(&draft, true)>{shown}</td>
                }
                .into_any()
            }
            _ => view! { <td class=class>{shown}</td> }.into_any(),
        }
    }

    /// The comment boxes for 0-based lines of a row, shown while open.
    fn boxes_row(
        this: Option<&Self>,
        at: &[(Side, Option<usize>)],
        colspan: u32,
    ) -> Option<AnyView> {
        let fc = this?;
        let rows: Vec<AnyView> = at
            .iter()
            .filter_map(|&(side, line)| Some((side, line? as u32 + 1)))
            .map(|(side, line)| fc.box_row(Some((side, line)), colspan))
            .collect();
        (!rows.is_empty()).then(|| rows.into_any())
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
                    view! { <FileView file=f.clone() comments=placed side_by_side=side_by_side inline_changes=inline_changes commenting=commenting.clone() /> }
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
    /// Per change block: whether upstream made the same change.
    upstream: Vec<bool>,
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
/// upstream change, so that coming back to a diff doesn't redo it.
fn cached_shape(file: &FileData, old_id: &str, new_id: &str) -> Arc<Shape> {
    if file.upstream.is_some() {
        return Arc::new(shape(file));
    }
    let key = (old_id.to_owned(), new_id.to_owned());
    if let Some(s) =
        SHAPE_CACHE.with_borrow(|c| c.iter().find(|(k, _)| *k == key).map(|(_, s)| s.clone()))
    {
        return s;
    }
    let s = Arc::new(shape(file));
    SHAPE_CACHE.with_borrow_mut(|c| {
        if c.len() >= SHAPES {
            c.pop_front();
        }
        c.push_back((key, s.clone()));
    });
    s
}

fn shape(file: &FileData) -> Shape {
    let upstream = file
        .upstream
        .as_ref()
        .map(|(a, b)| (text(a).unwrap_or_default(), text(b).unwrap_or_default()));
    shape_texts(
        text(&file.old).unwrap_or_default(),
        text(&file.new).unwrap_or_default(),
        upstream,
    )
}

/// Works out the diff from `old` to `new`. `upstream` is the same file's
/// upstream change.
fn shape_texts(old: &str, new: &str, upstream: Option<(&str, &str)>) -> Shape {
    let (old_lines, new_lines) = (rebase::lines(old), rebase::lines(new));
    let blocks = rebase::change_blocks(&old_lines, &new_lines);
    let upstream = match upstream {
        Some((ua, ub)) => rebase::from_upstream(
            &old_lines,
            &new_lines,
            &blocks,
            &rebase::lines(ua),
            &rebase::lines(ub),
        ),
        None => vec![false; blocks.len()],
    };
    let aligned: Vec<Vec<Aligned>> = blocks
        .iter()
        .map(|b| inline::align(&old_lines, &new_lines, b))
        .collect();
    let hunks = diff::hunks(&blocks, &aligned, old_lines.len(), new_lines.len(), CONTEXT);
    let marks = aligned
        .iter()
        .flatten()
        .filter_map(|r| match (r.paired, r.old, r.new) {
            (true, Some(o), Some(n)) => {
                let (dels, inss) = inline::changed_ranges(old_lines[o], new_lines[n]);
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
        skipped_after: diff::skipped_after(&hunks, old_lines.len()),
        hunks,
        upstream,
        marks,
    }
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
/// diff out.
fn stats(file: &FileData) -> (usize, usize, usize, bool) {
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
    (added, removed, blocks.len(), all_upstream)
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

#[component]
fn FileView(
    file: Arc<FileData>,
    comments: Vec<Placed>,
    side_by_side: Signal<bool>,
    inline_changes: Signal<bool>,
    #[prop(optional_no_strip)] commenting: Option<Commenting>,
) -> impl IntoView {
    let c = &file.change;
    let fc = commenting.map(|commenting| FileCommenting {
        commenting,
        path: c.path().to_owned(),
        boxes: expect_context::<OpenBoxes>(),
    });
    let binary = [&file.old, &file.new]
        .iter()
        .any(|b| b.as_ref().is_some_and(|b| b.text.is_none()));
    let (added, removed, blocks, all_upstream) = if binary {
        (0, 0, 0, false)
    } else {
        stats(&file)
    };
    let expanded = RwSignal::new(!all_upstream && added + removed <= BIG_FILE);
    // Roughly: the changed lines and the context around each block.
    let rows = if expanded.get_untracked() {
        added + removed + blocks * (2 * CONTEXT + 1)
    } else {
        0
    };
    let shown = RwSignal::new(false);
    shown.set(reveal(rows, shown));
    let old_path = c.old_path.as_deref().unwrap_or(c.path());
    let old_lines = SideLines::new(old_path, &c.old_blob, &file.old, expanded.get_untracked());
    let new_lines = SideLines::new(c.path(), &c.new_blob, &file.new, expanded.get_untracked());
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
        // highlighted; the flag says which.
        let cache = StoredValue::new_local(None::<(bool, Arc<Layout>)>);
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
            let highlighted = old_lines.ready() && new_lines.ready();
            if !highlighted {
                old_lines.request(true);
                new_lines.request(true);
            }
            let cached = cache.with_value(|c| {
                c.as_ref()
                    .filter(|(h, _)| *h == highlighted)
                    .map(|(_, l)| l.clone())
            });
            let l = cached.unwrap_or_else(|| {
                let shape = cached_shape(&file, &old_lines.id, &new_lines.id);
                let l = Arc::new(layout(shape, old_lines.html(), new_lines.html()));
                cache.set_value(Some((highlighted, l.clone())));
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
        <section class="file" id=file_id(c.path())>
            <header class="file-header" on:click=move |_| expanded.update(|e| *e = !*e)>
                <span class="chevron">{move || if expanded.get() { "▾" } else { "▸" }}</span>
                <span class=format!("status status-{}", status_label(c.status).replace(' ', "-"))>{status_label(c.status)}</span>
                <span class="path">{title}</span>
                {all_upstream.then(|| view! { <span class="badge upstream-badge">"only rebase changes"</span> })}
                <span class="stats"><span class="plus">{format!("+{added}")}</span>" "<span class="minus">{format!("−{removed}")}</span></span>
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
/// unified diff. A file keeps its view, and whether it is collapsed, until
/// its contents change.
#[component]
pub fn WorktreeDiff(files: Signal<Vec<WorktreeFile>>) -> impl IntoView {
    let inline_changes = Signal::stored(load_flag("inline-changes", true));
    let side_by_side = Signal::stored(false);
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
                    upstream: None,
                });
                view! { <FileView file=file comments=vec![] side_by_side=side_by_side inline_changes=inline_changes /> }
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
                        Arc::new(shape_texts(&f.old, &f.new, None)),
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
    fc: Option<&FileCommenting>,
) -> impl IntoView + use<> {
    let mut lc = LineComments::new(comments);
    let mut rows: Vec<AnyView> = Vec::new();
    for h in &l.shape.hunks {
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
            let upstream = block.is_some_and(|b| l.shape.upstream[b]);
            let marker = match kind {
                "del" => "−",
                "add" => "+",
                _ => " ",
            };
            rows.push(
                view! {
                    <tr class=format!("line {kind}") class:upstream=upstream>
                        {FileCommenting::ln(fc, "ln".into(), Side::Left, old, num(old.map(|o| o + l.old_start)))}
                        {FileCommenting::ln(fc, "ln".into(), Side::Right, new, num(new.map(|n| n + l.new_start)))}
                        <td class="marker">{marker}</td>
                        <td class="code" inner_html=code></td>
                    </tr>
                }
                .into_any(),
            );
            let mut threads = lc.take(Side::Left, old);
            threads.extend(lc.take(Side::Right, new));
            rows.extend(threads_row(threads, 4));
            rows.extend(FileCommenting::boxes_row(
                fc,
                &[(Side::Left, old), (Side::Right, new)],
                4,
            ));
        }
    }
    rows.extend(skip_row(l.shape.skipped_after, 4));
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
    fc: Option<&FileCommenting>,
) -> impl IntoView + use<> {
    let mut lc = LineComments::new(comments);
    let mut rows: Vec<AnyView> = Vec::new();
    for h in &l.shape.hunks {
        rows.extend(skip_row(h.skipped_before, 4));
        for r in &h.rows {
            let upstream = r.block.is_some_and(|b| l.shape.upstream[b]);
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
                        {FileCommenting::ln(fc, format!("ln {lk}"), Side::Left, r.old, num(r.old))}
                        <td class=format!("code {lk}") inner_html=html(&l.old_html, r.old)></td>
                        {FileCommenting::ln(fc, format!("ln {rk}"), Side::Right, r.new, num(r.new))}
                        <td class=format!("code {rk}") inner_html=html(&l.new_html, r.new)></td>
                    </tr>
                }
                .into_any(),
            );
            let mut threads = lc.take(Side::Left, r.old);
            threads.extend(lc.take(Side::Right, r.new));
            rows.extend(threads_row(threads, 4));
            rows.extend(FileCommenting::boxes_row(
                fc,
                &[(Side::Left, r.old), (Side::Right, r.new)],
                4,
            ));
        }
    }
    rows.extend(skip_row(l.shape.skipped_after, 4));
    let outside = outside_comments(lc.rest());
    view! {
        <table class="diff split" class:inline-changes=move || inline_changes.get()><tbody>{rows}</tbody></table>
        {outside}
    }
}
