//! Types exchanged as JSON between the server and the web UI.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A named GitHub search filter whose matching PRs are listed together.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inbox {
    pub id: i64,
    pub name: String,
    /// The GitHub search filter as the user wrote it. The server wraps
    /// it in `is:pr (...)`, ORs the configured repositories into a
    /// `repo:` group and moves any `sort:` tokens to the end.
    pub filter: String,
    pub position: i64,
    /// Whether the inbox's PRs count in the top bar's total.
    #[serde(default)]
    pub count_in_total: bool,
    /// Whether the polls sync the inbox's PRs. One without it is listed
    /// from its search results, and its PRs sync when a page watches
    /// them.
    #[serde(default)]
    pub sync_in_background: bool,
}

/// The body of a request that creates or edits an inbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEdit {
    pub name: String,
    pub filter: String,
    pub position: Option<i64>,
    #[serde(default)]
    pub count_in_total: bool,
    #[serde(default)]
    pub sync_in_background: bool,
}

/// A configured repository's settings. Every field has a default, so
/// settings saved before a field existed still load.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoSettings {
    /// The skill that reviews of the viewer's PRs run, without its `/`;
    /// `None` reviews them with a plain prompt.
    pub review_skill: Option<String>,
    /// Which of the viewer's PRs get reviewed.
    pub auto_review: AutoReview,
    /// The backend that reviews of the viewer's PRs run on.
    pub review_backend: AgentBackend,
    /// The model reviews run with on each backend, an
    /// `AgentModelInfo::value`. A backend without one uses its default.
    pub review_models: BTreeMap<AgentBackend, String>,
    /// Review a PR again when new commits are pushed to it.
    pub rereview_on_push: bool,
    /// Unix seconds. PRs opened before this aren't reviewed, so turning
    /// reviews on doesn't review every PR that's already open. The
    /// server sets it; what a page sends is ignored.
    pub auto_review_since: Option<i64>,
    /// The skill a new session's first prompt runs, without its `/`. A
    /// prompt that starts with `/` runs its own skill instead. `None`
    /// sends the prompt as written.
    pub session_skill: Option<String>,
    /// The permissions the New session form starts with, per backend. A
    /// backend without one starts in `PermissionMode::plan`.
    pub session_modes: BTreeMap<AgentBackend, PermissionMode>,
}

/// Which of the viewer's open PRs an auto-review session reviews.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoReview {
    Off,
    /// Those that aren't drafts; a draft once it's marked ready.
    #[default]
    NonDraft,
    /// Drafts too.
    IncludingDrafts,
}

/// How a review the server started went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoReviewState {
    /// Its session is working on it.
    Running,
    /// Its session posted on the PR, and was deleted.
    Posted,
    /// Its session finished without posting anything on the PR; the
    /// session is kept.
    Unposted,
    /// Its session couldn't start, failed or was interrupted; the
    /// session, if any, is kept.
    Failed,
    /// Its session was deleted before it finished.
    Cancelled,
}

/// A review of one of the viewer's PRs that the server started.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutoReviewEntry {
    pub id: i64,
    pub key: PrKey,
    pub title: String,
    pub url: String,
    /// The commit reviewed.
    pub head_oid: String,
    pub state: AutoReviewState,
    /// The session doing the review; `None` once it's deleted.
    pub session: Option<String>,
    /// The session's status while it exists.
    pub status: Option<AgentStatus>,
    /// Unix seconds.
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub cost: f64,
    pub cost_unit: CostUnit,
    /// Why it failed, for `Failed`.
    pub error: Option<String>,
}

/// Identifies a PR across the configured repositories.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PrKey {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSummary {
    pub key: PrKey,
    pub title: String,
    pub author: String,
    /// OPEN, CLOSED or MERGED.
    pub state: String,
    pub is_draft: bool,
    pub updated_at: String,
    pub url: String,
    pub version_count: u32,
    /// Whether the PR's full metadata has been synced into the database;
    /// an unsynced entry only carries the search's metadata.
    pub synced: bool,
    /// Set when the last sync failed before the PR ever synced. After a
    /// successful sync the error lives on `PrDetail` instead.
    pub sync_error: Option<String>,
    /// Where the PR sits in its stack. `None` when it isn't stacked or
    /// hasn't synced.
    pub stack: Option<StackSummary>,
    /// The PR's review threads as of the last inbox search; `None` when
    /// unknown, e.g. outside an inbox.
    #[serde(default)]
    pub threads: Option<ThreadCounts>,
    /// The checks on the PR's head as of the last inbox search; `None`
    /// when it has none or when unknown.
    #[serde(default)]
    pub ci: Option<CiStatus>,
    /// GitHub's review decision as of the PR's last sync, or of the last
    /// inbox search before the first sync. `None` when there's none.
    #[serde(default)]
    pub review_decision: Option<ReviewDecision>,
    /// In a merge queue; GitHub's `state` stays OPEN while one is.
    #[serde(default)]
    pub queued: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadCounts {
    pub resolved: u32,
    pub total: u32,
}

/// The combined state of the checks on a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CiStatus {
    Passing,
    Failing,
    Running,
}

/// GitHub's `PullRequestReviewDecision` of a PR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
    /// A value grenadine doesn't know yet.
    #[serde(other)]
    Unknown,
}

impl ReviewDecision {
    pub fn label(self) -> &'static str {
        match self {
            ReviewDecision::Approved => "Approved",
            ReviewDecision::ChangesRequested => "Changes requested",
            ReviewDecision::ReviewRequired => "Review required",
            ReviewDecision::Unknown => "Unknown review decision",
        }
    }

    /// The CSS class that colours the decision.
    pub fn class(self) -> &'static str {
        match self {
            ReviewDecision::Approved => "approved",
            ReviewDecision::ChangesRequested => "changes",
            ReviewDecision::ReviewRequired => "required",
            ReviewDecision::Unknown => "unknown",
        }
    }
}

/// GitHub's `MergeStateStatus` of a PR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeState {
    /// The head is out of date with the base.
    Behind,
    /// Requirements such as reviews or required checks aren't met.
    Blocked,
    Clean,
    /// The PR conflicts with its base.
    Dirty,
    Draft,
    /// Mergeable with passing checks and pre-receive hooks.
    HasHooks,
    /// Mergeable, but not every check passed.
    Unstable,
    /// GitHub is still working it out, or a value grenadine doesn't know
    /// yet.
    #[serde(other)]
    Unknown,
}

/// One PR of a stack, as GitHub reported it when the stack was fetched.
/// All PRs of a stack target the same repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackPr {
    pub number: u64,
    pub title: String,
    /// OPEN or MERGED.
    pub state: String,
    pub is_draft: bool,
    pub url: String,
    pub updated_at: String,
    pub head_oid: String,
    /// The PR whose head branch this PR targets. `None` for the bottom PR,
    /// which targets `Stack::base_ref`.
    pub parent: Option<u64>,
    /// In a merge queue.
    #[serde(default)]
    pub queued: bool,
}

/// A PR's ancestors down to the branch the bottom one targets, the PR
/// itself, and all of its descendants. Sibling branches of the ancestors
/// are left out because they don't affect the PR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stack {
    /// The branch the bottom PR targets, e.g. `main`.
    pub base_ref: String,
    pub prs: Vec<StackPr>,
    /// The walk stopped before reaching the bottom of the stack.
    pub more_ancestors: bool,
    /// The walk stopped before finding every descendant.
    pub more_descendants: bool,
}

/// A PR of a stack and its parent, enough to group and order stack-mates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackMember {
    pub number: u64,
    pub parent: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackSummary {
    /// The open PRs from the bottom of the stack up to and including this
    /// one. 0 when this PR isn't open.
    pub position: u32,
    /// The open PRs on the longest path through this PR.
    pub length: u32,
    /// Every PR of the stack, this one included.
    pub members: Vec<StackMember>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxWithPrs {
    pub inbox: Inbox,
    pub prs: Vec<PrSummary>,
    /// Unix seconds. When the inbox's search last succeeded; `None` when
    /// it never did, e.g. because the inbox was just created.
    #[serde(default)]
    pub searched_at: Option<i64>,
    /// Set when the inbox's last search failed, e.g. because of a typo in
    /// the filter.
    pub error: Option<String>,
    /// Set when GitHub found more PRs than were fetched.
    pub truncated: Option<Truncation>,
}

/// How much of an inbox's search result was fetched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Truncation {
    /// How many hits grenadine fetched.
    pub fetched: u32,
    /// How many GitHub found.
    pub found: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VersionKind {
    /// The PR's head when it was opened.
    Initial,
    /// One commit that a fast-forward push added.
    Push,
    /// The new head after a force push.
    ForcePush,
}

/// A GitHub user, or a git author that isn't linked to one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// `None` for a git author without a GitHub account.
    pub login: Option<String>,
    /// The display name; for a git author without a GitHub account, the
    /// name from the commit.
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// 1-based. Version 0 is the implicit "Base".
    pub number: u32,
    pub sha: String,
    /// The merge-base of `sha` and the tip of the PR's target branch. `None`
    /// when the commit is missing.
    pub merge_base: Option<String>,
    pub kind: VersionKind,
    /// When the push that created this version happened, if known.
    pub pushed_at: Option<String>,
    /// Who pushed this version, if known.
    pub pushed_by: Option<Person>,
    /// `pushed_by` is the commit's author because the pusher is unknown.
    pub pushed_by_is_guess: bool,
    /// The commit could not be fetched; the version can't be selected.
    pub missing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    /// The side as GitHub's `DiffSide` names it.
    pub fn as_github(self) -> &'static str {
        match self {
            Side::Left => "LEFT",
            Side::Right => "RIGHT",
        }
    }

    /// The side GitHub names `s`; anything but `"LEFT"` is the right side,
    /// which is also GitHub's default.
    pub fn from_github(s: &str) -> Side {
        if s == "LEFT" { Side::Left } else { Side::Right }
    }
}

/// A review comment on a line (or a file) of the PR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub in_reply_to: Option<u64>,
    pub author: String,
    pub body: String,
    pub path: String,
    /// The commit the comment was made on.
    pub original_commit: String,
    /// The line in `original_commit` (or its parent for the left side).
    pub original_line: Option<u32>,
    pub original_start_line: Option<u32>,
    /// The line mapped onto the PR's current head by GitHub; `None` when the
    /// comment is outdated.
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub side: Side,
    /// True for a comment on the whole file rather than on lines.
    pub on_file: bool,
    pub created_at: String,
    pub url: String,
    /// Whether the thread this comment starts is resolved. Only meaningful
    /// on a thread's root comment.
    #[serde(default)]
    pub resolved: bool,
    /// Who resolved the thread this comment starts, if known. Only
    /// meaningful on a thread's root comment.
    #[serde(default)]
    pub resolved_by: Option<Person>,
    /// The GraphQL ID of the thread this comment starts, which resolving
    /// it needs. Only set on a thread's root comment.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// The viewer may resolve the thread this comment starts, or unresolve
    /// it if it's resolved. Only meaningful on a thread's root comment.
    #[serde(default)]
    pub can_resolve: bool,
    /// The comment is part of the viewer's pending review: only the viewer
    /// sees it until the review is submitted.
    #[serde(default)]
    pub pending: bool,
    /// The comment's GraphQL ID, which editing and deleting a pending
    /// comment need. Only set on pending comments.
    #[serde(default)]
    pub node_id: Option<String>,
}

/// The body of a request that comments on a PR or edits a pending
/// comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewComment {
    pub body: String,
}

/// The body of a request that replies to a review thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewReply {
    pub body: String,
    /// Add the reply to the viewer's pending review, starting one if
    /// needed, rather than posting it right away.
    #[serde(default)]
    pub review: bool,
}

/// The body of a request that adds a thread to the viewer's pending
/// review, starting one if needed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewThread {
    pub path: String,
    pub side: Side,
    /// The 1-based line on `side`; `None` comments on the whole file.
    pub line: Option<u32>,
    pub body: String,
}

/// The body of a request that merges a PR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRequest {
    pub method: MergeMethod,
    /// The head the viewer reviewed. GitHub refuses the merge when the PR's
    /// head has moved on since.
    pub head_oid: String,
    /// The merge commit's title and body; `None` leaves them to GitHub.
    pub title: Option<String>,
    pub body: Option<String>,
    /// Delete the head branch once merged.
    pub delete_branch: bool,
}

/// The response to a merge that went through.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Merged {
    /// What went wrong after merging, e.g. deleting the branch.
    pub warning: Option<String>,
}

/// The body of a request that adds a PR to its merge queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enqueue {
    /// The head the viewer reviewed. GitHub refuses the enqueue when the
    /// PR's head has moved on since.
    pub head_oid: String,
    /// Join ahead of the PRs already queued.
    pub jump: bool,
}

/// The body of a request that makes a PR join its merge queue once its
/// requirements are met.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoMerge {
    /// The head the viewer reviewed. GitHub refuses the request when the
    /// PR's head has moved on since.
    pub head_oid: String,
}

/// What submitting a review says about the PR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

/// Why a review can't be submitted as it is, if it can't: Request changes
/// needs a summary, and Comment needs a summary or `pending` comments.
pub fn submit_blocker(event: ReviewEvent, body: &str, pending: usize) -> Option<&'static str> {
    let empty = body.trim().is_empty();
    match event {
        ReviewEvent::RequestChanges if empty => Some("Say what needs to change."),
        ReviewEvent::Comment if empty && pending == 0 => {
            Some("Write a summary or add comments to the review.")
        }
        _ => None,
    }
}

/// The body of a request that submits the viewer's review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitReview {
    pub event: ReviewEvent,
    /// The review's summary; may be empty.
    pub body: String,
}

/// The body of a 404 response to `GET /api/pr/{owner}/{name}/{number}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrMissing {
    /// The repository isn't one of the server's `--repo`s.
    NotConfigured,
    /// Configured, but not synced yet. An on-demand sync was kicked off
    /// for PRs no inbox covers.
    NotSynced,
    /// The sync attempt failed; carries its error.
    SyncFailed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrDetail {
    pub summary: PrSummary,
    pub body: String,
    pub base_ref: String,
    pub head_ref: String,
    pub versions: Vec<Version>,
    pub comments: Vec<ReviewComment>,
    /// `None` until a sync fetched it.
    pub stack: Option<Stack>,
    /// The push history couldn't be read from GitHub's activity log, so the
    /// versions were reconstructed heuristically.
    pub approximate: bool,
    /// Set when the latest recomputation disagreed with the versions stored
    /// before it.
    pub drift: Option<String>,
    /// Set when the last sync of this PR failed.
    pub sync_error: Option<String>,
    /// Reviews, the conversation, checks and whether the PR can be merged.
    /// `None` until a sync fetched them.
    #[serde(default)]
    pub review: Option<ReviewInfo>,
}

/// The verdict of a submitted review.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
}

/// A submitted review of a PR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub author: Person,
    pub state: ReviewState,
    /// The review's summary; empty when it has none.
    pub body: String,
    pub submitted_at: String,
    pub url: String,
    /// The PR head the review was submitted against.
    #[serde(default)]
    pub commit: Option<String>,
}

/// A comment on the PR itself rather than on its code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueComment {
    pub author: Person,
    pub body: String,
    pub created_at: String,
    pub url: String,
}

/// One check run or commit status on the PR's head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    /// `None` for a check that finished without passing or failing, e.g.
    /// one that was skipped.
    pub status: Option<CiStatus>,
    pub url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    /// The method as GitHub's `PullRequestMergeMethod` names it.
    pub fn as_github(self) -> &'static str {
        match self {
            MergeMethod::Merge => "MERGE",
            MergeMethod::Squash => "SQUASH",
            MergeMethod::Rebase => "REBASE",
        }
    }

    /// The method GitHub names `s`, if any.
    pub fn from_github(s: &str) -> Option<MergeMethod> {
        match s {
            "MERGE" => Some(MergeMethod::Merge),
            "SQUASH" => Some(MergeMethod::Squash),
            "REBASE" => Some(MergeMethod::Rebase),
            _ => None,
        }
    }
}

/// GitHub's `MergeQueueEntryState`: where in the queue the PR is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueState {
    /// Waiting for the queue's checks to run.
    AwaitingChecks,
    /// The queue is locked while entries ahead are merged.
    Locked,
    /// The queue is ready to merge the PR.
    Mergeable,
    /// In line, waiting its turn.
    Queued,
    /// The queue can't merge the PR.
    Unmergeable,
    /// A value grenadine doesn't know yet.
    #[serde(other)]
    Unknown,
}

/// The PR's place in its merge queue, when it's in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    /// Its 1-based place, the first to merge being 1.
    pub position: u32,
    pub state: QueueState,
}

/// How the PR relates to the merge queue its base branch requires.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeQueueInfo {
    /// The method the queue merges with, when GitHub reports it.
    pub method: Option<MergeMethod>,
    pub entry: Option<QueueEntry>,
    /// The PR joins the queue once its requirements are met.
    pub auto_merge: bool,
    /// The viewer may switch auto-merge on or off.
    pub can_enable_auto_merge: bool,
    pub can_disable_auto_merge: bool,
}

/// Whether and how the PR can be merged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeInfo {
    /// Whether the PR conflicts with its base; `None` while GitHub is
    /// still working that out.
    pub conflicting: Option<bool>,
    pub state: MergeState,
    /// The merge methods the repository allows, in GitHub's order.
    pub methods: Vec<MergeMethod>,
    /// The method GitHub preselects for the viewer.
    pub default_method: Option<MergeMethod>,
    /// The repository deletes head branches once their PRs are merged.
    pub delete_branch_on_merge: bool,
    /// The viewer may merge despite failing requirements.
    pub can_merge_as_admin: bool,
    /// The base branch's rules require every review thread to be
    /// resolved before merging.
    #[serde(default)]
    pub requires_conversation_resolution: bool,
    /// `Some` when the base branch requires merging through a merge
    /// queue.
    #[serde(default)]
    pub queue: Option<MergeQueueInfo>,
}

/// Someone asked to review a PR who hasn't yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestedReviewer {
    /// A user or a bot.
    Person(Person),
    Team(String),
    /// A reviewer from PR data saved before reviewers had kinds. The next
    /// sync replaces it.
    #[serde(untagged)]
    Legacy(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewInfo {
    /// The login of the user grenadine acts as.
    pub viewer: String,
    /// `None` when the repository doesn't require reviews and nobody
    /// reviewed.
    pub decision: Option<ReviewDecision>,
    /// Submitted reviews, oldest first.
    pub reviews: Vec<Review>,
    /// Users and teams asked to review who haven't yet.
    pub requested: Vec<RequestedReviewer>,
    /// Comments on the PR itself, oldest first.
    pub comments: Vec<IssueComment>,
    /// The checks on the PR's head and their combined state.
    pub checks: Vec<Check>,
    pub ci: Option<CiStatus>,
    pub merge: MergeInfo,
    /// The GraphQL ID of the viewer's pending review, if they started one.
    /// Its comments are among the PR's comments, marked `pending`.
    #[serde(default)]
    pub pending_review: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    TypeChanged,
}

/// One file that differs between two trees.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileChange {
    pub status: ChangeStatus,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub old_blob: Option<String>,
    pub new_blob: Option<String>,
}

impl FileChange {
    /// The path to show and to match comments against.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub files: Vec<FileChange>,
}

/// A file whose working copy in an agent's worktree differs from the
/// index, or that git doesn't track yet.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorktreeFile {
    /// The blob ids are left out; the contents come along instead.
    pub change: FileChange,
    /// The index's version; `None` for untracked files.
    pub old: Option<Blob>,
    /// The working copy; `None` for deleted files.
    pub new: Option<Blob>,
}

/// The changes `git add` would stage in an agent's worktree.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeChanges {
    pub files: Vec<WorktreeFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobsRequest {
    pub repo: String,
    pub ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Blob {
    /// `None` for binary content.
    pub text: Option<String>,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobsResponse {
    pub blobs: BTreeMap<String, Blob>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncPhase {
    #[default]
    Searching,
    Syncing,
    Idle,
}

/// A PR whose sync is running right now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncingPr {
    pub key: PrKey,
    /// `None` until the PR's first sync stored it.
    pub title: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatus {
    pub phase: SyncPhase,
    /// PRs of the current poll that haven't finished syncing, including the
    /// ones in flight.
    pub remaining: usize,
    pub in_flight: Vec<SyncingPr>,
    /// Unix seconds when the last poll ended.
    pub last_finished: Option<i64>,
    /// Set when the last poll failed as a whole.
    pub last_error: Option<String>,
    /// Unix seconds until which background syncs pause to stay within
    /// GitHub's rate limit; `None` when nothing is paused.
    #[serde(default)]
    pub paused_until: Option<i64>,
}

/// How urgently a GitHub request is needed. Less urgent requests pause
/// first: the viewer's own actions go last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RequestPriority {
    /// A page's action or a PR it opened.
    User,
    /// The poller's inbox searches.
    Search,
    /// Syncs and lookups nobody is waiting for.
    Background,
}

/// Which of GitHub's rate limits a request spends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ApiResource {
    /// The GraphQL API's points.
    GraphQl,
    /// The REST API's requests.
    Rest,
}

/// What the server spent on GitHub and what the limits look like, for
/// the Debug tab.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubUsage {
    /// Unix seconds when the snapshot was taken.
    pub now: i64,
    /// The last known state of each resource's limit.
    pub limits: Vec<RateLimitInfo>,
    /// Last hour, grouped by (kind, resource, priority), sorted by
    /// points desc then requests desc.
    pub kinds: Vec<RequestKindUsage>,
    /// 60 entries, oldest first, the last one is the current minute.
    pub minutes: Vec<MinuteUsage>,
    /// The last responses' rate-limit headers, newest first, at most
    /// 100.
    pub reports: Vec<RateLimitReport>,
}

/// One of GitHub's counters for a resource, as its most-spent report
/// this window described it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitInfo {
    pub resource: ApiResource,
    pub limit: u32,
    pub remaining: u32,
    pub used: u32,
    /// Unix seconds when the window resets.
    pub reset: i64,
    /// How many of the recent responses on this API this counter
    /// answered.
    pub answered: u32,
    /// How many recent responses on this API the reports hold.
    pub responses: u32,
    /// What grenadine's requests that this counter answered spent since
    /// this window started (reset - 3600): GraphQL points, or REST
    /// requests.
    pub ours: u32,
    /// The tiers that are paused right now.
    pub paused: Vec<RequestPriority>,
}

/// A kind of request's use over the last hour.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestKindUsage {
    /// What the request was for, e.g. "search" or "pr".
    pub kind: String,
    pub resource: ApiResource,
    pub priority: RequestPriority,
    pub requests: u32,
    /// The GraphQL points spent; `None` where they can't be known, e.g.
    /// mutations and REST calls.
    pub points: Option<u32>,
    /// How many were refused without being sent to keep the limit.
    pub deferred: u32,
}

/// One response's rate-limit headers and what the request named.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitReport {
    /// Unix seconds when the response came in.
    pub at: i64,
    pub resource: ApiResource,
    pub kind: String,
    /// The `owner/name` repos the request named, sorted; `owner/*` for
    /// an org: or user: search qualifier; empty when it named none.
    pub repos: Vec<String>,
    pub limit: u32,
    pub remaining: u32,
    pub used: u32,
    pub reset: i64,
}

/// One minute's spending.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinuteUsage {
    /// Unix seconds at the minute's start.
    pub start: i64,
    pub graphql_points: u32,
    pub rest_requests: u32,
}

/// The machine's resources and what the sessions use, for the Debug
/// tab's System section.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemStats {
    /// Unix seconds when the snapshot was taken.
    pub now: i64,
    pub memory: MemoryStats,
    pub load: LoadStats,
    /// How much of the last 10 s tasks stalled on each resource;
    /// `None` per resource whose /proc/pressure file is missing.
    pub pressure: PressureStats,
    /// The filesystems grenadine keeps its data on.
    pub disks: Vec<DiskUsage>,
    pub server: ServerStats,
    /// `None` when there is no systemd user manager or the agent
    /// slices' cgroup can't be found.
    pub sessions: Option<Vec<SessionResources>>,
    /// What the agent slices no session owns add up to.
    pub leftover: Option<LeftoverResources>,
}

/// What /proc/meminfo reported.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStats {
    pub total_bytes: u64,
    /// Free plus what the kernel can reclaim; the rest is in use.
    pub available_bytes: u64,
    /// The page cache's part of `available_bytes`.
    pub cached_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_free_bytes: u64,
}

/// /proc/loadavg and the core count.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoadStats {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub cpus: u32,
}

/// Pressure stall averages over the last 10 s, in percent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PressureStats {
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    pub io: Option<f64>,
}

/// One filesystem's space, as statvfs reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsage {
    /// What the path is, e.g. "Worktrees".
    pub label: String,
    pub path: String,
    pub total_bytes: u64,
    /// Free for unprivileged users (f_bavail).
    pub free_bytes: u64,
}

/// The server process's own use.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStats {
    pub rss_bytes: u64,
    pub threads: u32,
    pub open_fds: u32,
    /// Unix seconds when the server started.
    pub started_at: i64,
}

/// What one session's systemd slice holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionResources {
    pub id: String,
    pub title: String,
    pub status: AgentStatus,
    /// Anonymous memory plus the kernel's, from the cgroup's
    /// memory.stat.
    pub process_bytes: u64,
    /// Page cache charged to the slice.
    pub cache_bytes: u64,
    pub processes: u32,
    /// Percent of one core since the previous sample, can exceed 100;
    /// `None` on the first sample.
    pub cpu_percent: Option<f64>,
}

/// What the agent slices that belong to no session add up to, e.g. ones
/// a deleted session left running.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftoverResources {
    pub slices: u32,
    pub process_bytes: u64,
    pub cache_bytes: u64,
    pub processes: u32,
}

/// Pushed to the page over server-sent events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerEvent {
    /// The inbox list or the PRs in an inbox changed.
    InboxesChanged,
    /// A PR's metadata, versions or comments changed.
    PrChanged(PrKey),
    /// The whole sync status, sent on every change so a missed event
    /// self-heals.
    SyncStatus(SyncStatus),
    /// An agent session was created, deleted or changed its status.
    AgentsChanged,
    /// A cleanup item was found, deleted or handed off, or a scan started
    /// or ended.
    CleanupChanged,
    /// A terminal was opened in the agent session with this ID, or one of
    /// its terminals closed.
    TerminalsChanged(String),
    /// The settings of this repository (`owner/name`) changed.
    RepoSettingsChanged(String),
    /// A review of the viewer's PRs started, finished or went away.
    AutoReviewsChanged,
}

/// A login shell in an agent session's worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInfo {
    /// Counts up from 1 within the session.
    pub id: u64,
}

/// The size a page fits a terminal to, sent as a WebSocket text message.
/// Binary messages carry the terminal's input and output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

/// Something a deleted agent session left behind outside its worktree,
/// as a scan of its transcript found it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupItem {
    pub id: i64,
    /// The session's `owner/name`.
    pub repo: String,
    pub session_id: String,
    pub session_title: String,
    /// One line, e.g. "Docker container grenadine-e2e".
    pub title: String,
    /// Markdown: what was created, where, and how to remove it.
    pub detail: String,
    /// Unix seconds.
    pub created_at: i64,
    /// The backend of the session it was found in.
    #[serde(default)]
    pub backend: AgentBackend,
    /// The agent session the item was handed off to for cleaning up.
    pub handed_off_to: Option<String>,
}

/// The cleanup items and the scans still looking for more.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupState {
    pub items: Vec<CleanupItem>,
    /// The titles of the deleted sessions whose scans are running.
    pub scanning: Vec<String>,
}

/// The body of a request that hands cleanup items off to a new agent
/// session, which starts in plan mode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewCleanupSession {
    pub ids: Vec<i64>,
    /// The repo the session's worktree is in.
    pub repo: String,
    /// Which agent backend runs the session's turns.
    #[serde(default)]
    pub backend: AgentBackend,
    /// An `AgentModelInfo::value` of the backend; `None` keeps its
    /// default.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// The session's worktree is being created before its first turn
    /// runs.
    Creating,
    /// Waiting for a prompt.
    Idle,
    /// The agent is working on a prompt.
    Running,
    /// The agent is waiting for the user to allow or deny a tool call.
    AwaitingApproval,
    /// The user stopped the last turn, or the server restarted during it.
    Interrupted,
    /// The last turn ended with an error.
    Failed,
}

/// An agent session working in its own git worktree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentSession {
    /// The session's ID, a UUID.
    pub id: String,
    /// Which agent runs the session's turns.
    #[serde(default)]
    pub backend: AgentBackend,
    /// `owner/name`.
    pub repo: String,
    /// The PR whose head the worktree started from; `None` for trunk.
    pub pr: Option<u64>,
    /// The first line of the first prompt.
    pub title: String,
    /// A short summary of the first prompt, written in the background;
    /// `None` until it's written, or if writing it failed.
    #[serde(default)]
    pub summary: Option<String>,
    /// The local branch checked out in the worktree.
    pub branch: String,
    pub worktree: String,
    pub base_sha: String,
    pub status: AgentStatus,
    /// Unix seconds.
    pub created_at: i64,
    /// What all of the session's turns cost, as the backend reported it,
    /// in `cost_unit`.
    pub cost: f64,
    /// The unit `cost` is in.
    #[serde(default)]
    pub cost_unit: CostUnit,
    /// How many tokens the context holds after the latest assistant
    /// message; `None` until the backend reports it.
    pub context_tokens: Option<u64>,
    /// The size of the model's context window, in tokens.
    pub context_window: Option<u64>,
    /// The model the backend runs the session with, as it names it;
    /// `None` until it starts.
    #[serde(default)]
    pub model: Option<String>,
    /// Where the session's work was last landed by wrapping up.
    #[serde(default)]
    pub landed: Option<Landed>,
    /// The PRs the session opened; `None` when it opened none.
    #[serde(default)]
    pub prs: Option<AgentPrs>,
    /// How many files in the worktree have unstaged changes, as
    /// `WorktreeChanges` has them; 0 once the worktree is gone.
    #[serde(default)]
    pub unstaged: u32,
    /// Who started the session.
    #[serde(default)]
    pub kind: SessionKind,
    /// The draft the session started from, if any.
    #[serde(default)]
    pub draft: Option<String>,
}

/// Who started an agent session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// The user, from the Agents tab.
    #[default]
    Interactive,
    /// The server, to review one of the viewer's PRs unattended. It's
    /// listed in the Auto-reviews tab rather than the Agents tab.
    AutoReview,
}

impl SessionKind {
    /// The value the database names it with.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Interactive => "interactive",
            SessionKind::AutoReview => "auto_review",
        }
    }

    /// The kind `as_str` names; unknown ones are interactive.
    pub fn parse(s: &str) -> SessionKind {
        match s {
            "auto_review" => SessionKind::AutoReview,
            _ => SessionKind::Interactive,
        }
    }
}

impl AgentSession {
    /// An idle Claude session `id` in `repo` that has done nothing yet,
    /// for tests to change the fields they care about.
    #[doc(hidden)]
    pub fn fixture(id: &str, repo: &str) -> AgentSession {
        AgentSession {
            id: id.into(),
            backend: AgentBackend::Claude,
            repo: repo.into(),
            pr: None,
            title: String::new(),
            summary: None,
            branch: String::new(),
            worktree: String::new(),
            base_sha: String::new(),
            status: AgentStatus::Idle,
            created_at: 0,
            cost: 0.0,
            cost_unit: CostUnit::Usd,
            context_tokens: None,
            context_window: None,
            model: None,
            landed: None,
            prs: None,
            unstaged: 0,
            kind: SessionKind::Interactive,
            draft: None,
        }
    }
}

/// A PR an agent session opened, as of the last poll.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPr {
    pub key: PrKey,
    /// `None` until a poll looks it up.
    pub title: Option<String>,
    /// OPEN, CLOSED or MERGED.
    pub state: String,
    /// The checks on its head; `None` when it has none.
    pub ci: Option<CiStatus>,
    /// Whether it conflicted with its base at the last poll that could
    /// tell.
    pub conflicting: bool,
    /// In a merge queue.
    #[serde(default)]
    pub queued: bool,
}

/// How far the PRs an agent session opened have come. PRs closed without
/// merging count in neither number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPrs {
    pub merged: u32,
    pub total: u32,
    /// How many of the open ones conflict with their bases.
    #[serde(default)]
    pub conflicting: u32,
}

/// Which agent backend runs a session's turns.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackend {
    /// Claude Code.
    #[default]
    Claude,
    /// Devin.
    Devin,
}

impl AgentBackend {
    /// Every backend, in picker order.
    pub const ALL: [AgentBackend; 2] = [AgentBackend::Claude, AgentBackend::Devin];

    /// The value requests and the database name it with.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentBackend::Claude => "claude",
            AgentBackend::Devin => "devin",
        }
    }

    /// The backend `as_str` names, if any.
    pub fn parse(s: &str) -> Option<AgentBackend> {
        AgentBackend::ALL.into_iter().find(|b| b.as_str() == s)
    }

    /// What the backend picker calls it.
    pub fn label(self) -> &'static str {
        match self {
            AgentBackend::Claude => "Claude Code",
            AgentBackend::Devin => "Devin",
        }
    }

    /// What the page calls the agent in prose, e.g. "What Claude may do".
    pub fn agent_name(self) -> &'static str {
        match self {
            AgentBackend::Claude => "Claude",
            AgentBackend::Devin => "Devin",
        }
    }

    /// The unit the backend reports costs in.
    pub fn cost_unit(self) -> CostUnit {
        match self {
            AgentBackend::Claude => CostUnit::Usd,
            AgentBackend::Devin => CostUnit::Acu,
        }
    }

    /// The permission modes as `(value, label)`, in picker order. Every
    /// backend has one named "plan", `PermissionMode::plan`.
    pub fn modes(self) -> &'static [(&'static str, &'static str)] {
        match self {
            AgentBackend::Claude => &[
                // Ask before every tool use.
                ("manual", "Ask for everything"),
                // Allow file edits, ask for the rest.
                ("acceptEdits", "Accept edits"),
                // A classifier allows routine tool uses and asks for risky
                // ones.
                ("auto", "Auto"),
                // Research without changing anything, then ask the user to
                // approve a plan; the turn goes on in the mode picked with
                // the approval.
                ("plan", "Plan first"),
            ],
            AgentBackend::Devin => &[
                // Allow file edits, ask for the rest.
                ("accept-edits", "Code"),
                // Auto-approve actions the model judges safe.
                ("smart", "Smart"),
                // Answer questions without code changes.
                ("ask", "Ask"),
                // Plan changes before implementing, then ask the user to
                // approve; the turn goes on in the mode picked with the
                // approval.
                ("plan", "Plan first"),
                // Auto-approve all tool calls.
                ("bypass", "Bypass permissions"),
            ],
        }
    }

    /// The mode a session starts with until the user picks one.
    pub fn default_mode(self) -> &'static str {
        match self {
            AgentBackend::Claude => "auto",
            AgentBackend::Devin => "accept-edits",
        }
    }

    /// The modes a turn can go on in once a plan is approved, in picker
    /// order.
    pub fn after_plan(self) -> &'static [&'static str] {
        match self {
            AgentBackend::Claude => &["manual", "acceptEdits", "auto"],
            AgentBackend::Devin => &["accept-edits", "smart", "bypass"],
        }
    }

    /// The mode the plan card picks first.
    pub fn plan_default_mode(self) -> &'static str {
        match self {
            AgentBackend::Claude => "auto",
            AgentBackend::Devin => "smart",
        }
    }

    /// Whether `mode` is one of the backend's permission modes.
    pub fn has_mode(self, mode: &str) -> bool {
        self.modes().iter().any(|(m, _)| *m == mode)
    }

    /// Whether a turn can go on in `mode` once a plan is approved.
    pub fn allows_after_plan(self, mode: &str) -> bool {
        self.after_plan().contains(&mode)
    }

    /// What the picker calls `mode`, if the backend has it.
    pub fn mode_label(self, mode: &str) -> Option<&'static str> {
        self.modes()
            .iter()
            .find(|(m, _)| *m == mode)
            .map(|(_, label)| *label)
    }

    /// What the backend offers sessions with `models`, which are listed
    /// at runtime and are empty when they can't be.
    pub fn info(self, models: Vec<AgentModelInfo>) -> AgentBackendInfo {
        AgentBackendInfo {
            backend: self,
            models,
            error: None,
        }
    }
}

/// The unit a session's or a turn's cost is in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostUnit {
    /// US dollars.
    #[default]
    Usd,
    /// ACUs, Devin's usage unit.
    Acu,
}

impl CostUnit {
    /// The value the database names it with.
    pub fn as_str(self) -> &'static str {
        match self {
            CostUnit::Usd => "usd",
            CostUnit::Acu => "acu",
        }
    }

    /// The unit `as_str` names, if any.
    pub fn parse(s: &str) -> Option<CostUnit> {
        match s {
            "usd" => Some(CostUnit::Usd),
            "acu" => Some(CostUnit::Acu),
            _ => None,
        }
    }
}

/// What a backend offers sessions that is only known at runtime: its
/// models. Its permission modes are `AgentBackend::modes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBackendInfo {
    pub backend: AgentBackend,
    /// The models; empty when they can't be listed.
    pub models: Vec<AgentModelInfo>,
    /// Why the models couldn't be listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// How an agent decides about tool uses during a turn; the user picks it
/// with every prompt. Whatever the mode doesn't decide on its own is
/// asked on the page. The values are the backend's, in
/// `AgentBackend::modes`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionMode(String);

impl PermissionMode {
    pub fn new(mode: impl Into<String>) -> PermissionMode {
        PermissionMode(mode.into())
    }

    /// The value the backend names the mode with.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Research without changing anything, then ask the user to approve a
    /// plan; every backend's modes include one named "plan".
    pub fn plan() -> PermissionMode {
        PermissionMode("plan".into())
    }
}

impl From<&str> for PermissionMode {
    fn from(mode: &str) -> PermissionMode {
        PermissionMode(mode.to_owned())
    }
}

/// A model a backend offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentModelInfo {
    /// What the backend's model option takes, e.g. `sonnet` or
    /// `claude-opus-4-8`; `default` is its configured default.
    pub value: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
}

/// The `repo` of the sessions that aren't in any repository, which work in
/// a plain directory of their own, with no git. Repositories are
/// `owner/name`, so it can't name one.
pub const GENERAL: &str = "general";

/// The body of a request that starts an agent session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewAgent {
    /// Which agent backend runs the session's turns.
    #[serde(default)]
    pub backend: AgentBackend,
    pub repo: String,
    /// Start from this PR's head rather than from the trunk.
    pub pr: Option<u64>,
    pub prompt: String,
    /// One of the backend's `AgentBackend::modes`.
    pub permission_mode: PermissionMode,
    /// An `AgentModelInfo::value` of the backend; `None` keeps its
    /// default.
    #[serde(default)]
    pub model: Option<String>,
    /// Images that go with the prompt, e.g. pasted screenshots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<PromptImage>,
    /// The web draft the session starts from, if any. A second start with
    /// the same draft returns the session the first one started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<String>,
}

/// A follow-up prompt for an agent session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub prompt: String,
    /// One of the session's backend's `AgentBackend::modes`.
    pub permission_mode: PermissionMode,
    /// An `AgentModelInfo::value` of the backend; `None` keeps its
    /// default.
    #[serde(default)]
    pub model: Option<String>,
    /// Images that go with the prompt, e.g. pasted screenshots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<PromptImage>,
}

/// An image sent with a prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptImage {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub media_type: String,
    /// The image's bytes, base64-encoded.
    pub data: String,
}

/// Where wrapping up a session puts its commits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WrapUpTarget {
    /// On top of whatever the server's clone has checked out, a branch or
    /// a detached HEAD.
    #[default]
    Checkout,
    /// Pushed to GitHub with a pull request for them.
    PullRequest,
}

/// Asks to land an agent session's work. The agent may first get prompts
/// to commit its changes or to resolve conflicts; those turns go with
/// `permission_mode` and `model`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrapUp {
    pub target: WrapUpTarget,
    /// One of the session's backend's `AgentBackend::modes`.
    pub permission_mode: PermissionMode,
    /// An `AgentModelInfo::value` of the backend; `None` keeps its
    /// default.
    #[serde(default)]
    pub model: Option<String>,
}

/// Where a session's work was landed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Landed {
    /// The clone's checkout was fast-forwarded to `sha`.
    Checkout { sha: String },
    PullRequest {
        number: u64,
        url: String,
        /// The branch its commits are pushed to; empty when it was landed
        /// before the branch was recorded.
        #[serde(default)]
        branch: String,
    },
    /// A stack of PRs, one per commit, bottom first.
    Stack { prs: Vec<LandedPr> },
}

/// One PR of a `Landed::Stack`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandedPr {
    pub number: u64,
    pub url: String,
    /// The branch its commits are pushed to; empty when it was landed
    /// before the branch was recorded.
    #[serde(default)]
    pub branch: String,
}

impl Landed {
    /// The PRs the work went into, bottom of the stack first.
    pub fn prs(&self) -> Vec<LandedPr> {
        match self {
            Landed::Checkout { .. } => Vec::new(),
            Landed::PullRequest {
                number,
                url,
                branch,
            } => vec![LandedPr {
                number: *number,
                url: url.clone(),
                branch: branch.clone(),
            }],
            Landed::Stack { prs } => prs.clone(),
        }
    }

    /// `prs` as a `Landed`: a single PR stays a `PullRequest`.
    pub fn from_prs(mut prs: Vec<LandedPr>) -> Landed {
        if prs.len() == 1 {
            let LandedPr {
                number,
                url,
                branch,
            } = prs.remove(0);
            Landed::PullRequest {
                number,
                url,
                branch,
            }
        } else {
            Landed::Stack { prs }
        }
    }
}

/// The user's answer to an `ApprovalRequested` event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// The tool use's ID.
    pub id: String,
    /// Whether the tool use may go ahead. When `option` is set, the
    /// option's kind decides instead.
    pub allow: bool,
    /// Told to the agent when the tool call is denied, or along with an
    /// `ExitPlanMode` approval.
    pub message: Option<String>,
    /// For `ExitPlanMode`: the mode the turn goes on in once the plan is
    /// approved.
    #[serde(default)]
    pub mode: Option<PermissionMode>,
    /// Which of the request's `options` the user picked, an
    /// `ApprovalOption::id`; `None` for a plain allow/deny.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option: Option<String>,
    /// For `AskUserQuestion`: each question's answer keyed by the
    /// question's text, the picked option labels in option order, then
    /// the user's own text if any.
    #[serde(default)]
    pub answers: Option<BTreeMap<String, Vec<String>>>,
}

/// An answer a backend's permission request offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalOption {
    /// Sent back as `Approval::option`.
    pub id: String,
    /// What the button says.
    pub label: String,
    pub kind: ApprovalKind,
}

/// What kind of answer an `ApprovalOption` is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    /// A kind of the backend's own, which doesn't allow the tool use.
    #[serde(other)]
    Other,
}

impl ApprovalKind {
    /// Whether picking it lets the tool use go ahead.
    pub fn is_allow(self) -> bool {
        matches!(self, ApprovalKind::AllowOnce | ApprovalKind::AllowAlways)
    }
}

/// Something that happened in an agent session's transcript.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventKind {
    /// A prompt the user sent.
    Prompt(String),
    /// The images sent with the prompt before it, by their names under
    /// `/api/agents/{id}/images/`.
    Images(Vec<String>),
    /// A piece of assistant text while it streams in. Never stored; the
    /// complete `Text` follows.
    TextDelta(String),
    /// A complete block of assistant text, in Markdown.
    Text(String),
    ToolUse {
        id: String,
        name: String,
        /// The tool's input as JSON.
        input: String,
        /// The subagent that made the call, when it wasn't the session's
        /// own agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
        /// The change a file tool made, when the backend reports one.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        patch: Vec<PatchHunk>,
        /// The ids of the images in the result, such as screenshots, which
        /// `/api/images/{id}` serves.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    /// An image the agent showed the user with the `show_image` tool.
    Image {
        /// The id that `/api/images/{id}` serves the image under.
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
    },
    /// The agent wants to use a tool and waits for the user's `Approval`.
    ApprovalRequested {
        id: String,
        tool_name: String,
        /// The tool's input as JSON.
        input: String,
        /// The answers the backend offers; empty when allow/deny are the
        /// only choices.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<ApprovalOption>,
    },
    ApprovalResolved {
        id: String,
        allow: bool,
        /// What the user wrote when sending the request back.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// A turn ended; `error` is set when it failed. `cost` is in the
    /// session's `cost_unit`.
    TurnEnded {
        error: Option<String>,
        #[serde(alias = "cost_usd")]
        cost: Option<f64>,
    },
    /// How full the context window is. Never stored; the session keeps
    /// the latest values. `None` leaves a value unchanged.
    Usage {
        tokens: Option<u64>,
        window: Option<u64>,
    },
    /// The model the backend runs the session with. Never stored; the
    /// session keeps the latest.
    Model(String),
    /// Grenadine couldn't run the agent, or it crashed.
    Error(String),
    /// Wrapping up landed the session's work.
    Landed(Landed),
    /// Wrapping up stopped; carries why.
    WrapUpFailed(String),
}

/// One hunk of the change a file tool made, as the backend reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchHunk {
    /// The 1-based line the hunk starts on in the old file.
    pub old_start: usize,
    /// The 1-based line the hunk starts on in the new file.
    pub new_start: usize,
    /// The hunk's lines, each prefixed with ' ', '-' or '+'.
    pub lines: Vec<String>,
}

impl AgentEventKind {
    /// Whether the event is only streamed to open pages and never stored.
    pub fn is_ephemeral(&self) -> bool {
        matches!(
            self,
            AgentEventKind::TextDelta(_) | AgentEventKind::Usage { .. } | AgentEventKind::Model(_)
        )
    }
}

/// Sent over an agent session's event stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    /// The event's position in the transcript; `None` for ephemeral events.
    pub seq: Option<u64>,
    /// When the server emitted the event, in Unix milliseconds; `None`
    /// for events stored before times were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
    pub kind: AgentEventKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_review_needs() {
        use ReviewEvent::*;
        assert_eq!(submit_blocker(Approve, "", 0), None);
        assert!(submit_blocker(RequestChanges, " ", 3).is_some());
        assert_eq!(submit_blocker(RequestChanges, "Fix it", 0), None);
        assert!(submit_blocker(Comment, "", 0).is_some());
        assert_eq!(submit_blocker(Comment, "", 1), None);
        assert_eq!(submit_blocker(Comment, "Hm", 0), None);
    }

    #[test]
    fn sides_on_github() {
        for side in [Side::Left, Side::Right] {
            assert_eq!(Side::from_github(side.as_github()), side);
        }
        assert_eq!(Side::from_github(""), Side::Right);
    }

    #[test]
    fn merge_methods_on_github() {
        use MergeMethod::*;
        for m in [Merge, Squash, Rebase] {
            assert_eq!(MergeMethod::from_github(m.as_github()), Some(m));
        }
        assert_eq!(MergeMethod::from_github(""), None);
    }

    /// PR data saved before requested reviewers had kinds holds plain
    /// strings; it must still parse, and new values must round-trip.
    #[test]
    fn requested_reviewers_from_saved_json() {
        let legacy: Vec<RequestedReviewer> = serde_json::from_str(r#"["a","team"]"#).unwrap();
        assert_eq!(
            legacy,
            [
                RequestedReviewer::Legacy("a".into()),
                RequestedReviewer::Legacy("team".into()),
            ]
        );
        let reviewers = vec![
            RequestedReviewer::Person(Person {
                login: Some("a".into()),
                name: None,
                avatar_url: Some("av".into()),
            }),
            RequestedReviewer::Team("team".into()),
        ];
        let back: Vec<RequestedReviewer> =
            serde_json::from_str(&serde_json::to_string(&reviewers).unwrap()).unwrap();
        assert_eq!(back, reviewers);
    }

    /// `PermissionMode::plan`, the pickers' starting values and plan
    /// approvals rely on each backend's tables agreeing.
    #[test]
    fn backend_modes_agree() {
        for backend in AgentBackend::ALL {
            assert_eq!(AgentBackend::parse(backend.as_str()), Some(backend));
            assert!(
                backend.has_mode(PermissionMode::plan().as_str()),
                "{backend:?}"
            );
            assert!(backend.has_mode(backend.default_mode()), "{backend:?}");
            assert!(!backend.after_plan().is_empty(), "{backend:?}");
            for mode in backend.after_plan() {
                assert!(backend.has_mode(mode), "{backend:?} {mode}");
            }
        }
    }
}
