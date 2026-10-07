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
}

/// The body of a request that creates or edits an inbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEdit {
    pub name: String,
    pub filter: String,
    pub position: Option<i64>,
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
    /// Set when the inbox's last search failed, e.g. because of a typo in
    /// the filter.
    pub error: Option<String>,
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
    /// The commit could not be fetched; the version can't be selected.
    pub missing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Left,
    Right,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    TypeChanged,
}

/// One file that differs between two trees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobsRequest {
    pub repo: String,
    pub ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
}
