//! The GitHub API: searches, PR details, PRs by branch, the head branch's
//! activity log and review comments.

use std::collections::BTreeMap;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use chrono::DateTime;
use grenadine_core::api::{
    Check, CiStatus, IssueComment, MergeInfo, MergeMethod, MergeRequest, NewThread, Person, PrKey,
    Review, ReviewComment, ReviewEvent, ReviewInfo, ReviewState, Side, StackPr, SubmitReview,
    ThreadCounts,
};
use grenadine_core::versions::{Activity, ActivityKind, ForcePush};
use reqwest::header;
use serde::Deserialize;
use serde_json::{Value, json};

pub const API: &str = "https://api.github.com";

/// Reads the token of the `gh` CLI's login.
pub fn gh_token() -> Result<String> {
    let out = Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .output()
        .context("failed to run `gh auth token`; is the gh CLI installed?")?;
    if !out.status.success() {
        bail!(
            "`gh auth token` failed ({}); run `gh auth login`",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

#[derive(Clone)]
pub struct GitHub {
    http: reqwest::Client,
    api: String,
}

/// One PR that a search found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub key: PrKey,
    pub title: String,
    pub author: String,
    /// OPEN, CLOSED or MERGED.
    pub state: String,
    pub is_draft: bool,
    pub url: String,
    pub updated_at: String,
    pub head_oid: String,
    pub threads: ThreadCounts,
    /// `None` when the head has no checks.
    pub ci: Option<CiStatus>,
    /// APPROVED, CHANGES_REQUESTED or REVIEW_REQUIRED; `None` when GitHub
    /// has no decision.
    pub review_decision: Option<String>,
}

/// Everything about a PR that its sync needs from GraphQL.
#[derive(Clone, Debug)]
pub struct PrData {
    pub title: String,
    pub body: String,
    pub author: String,
    pub state: String,
    pub is_draft: bool,
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
    pub base_ref: String,
    pub head_ref: String,
    pub head_oid: String,
    /// `owner/name` of the repository the head branch lives in; `None` when
    /// it was deleted.
    pub head_repo: Option<String>,
    pub force_pushes: Vec<ForcePush>,
    /// The PR's commits in order, each with the time its first check suite
    /// was created (a stand-in for when it was pushed).
    pub commits: Vec<(String, Option<i64>)>,
    pub review: ReviewInfo,
    /// The comments of the viewer's pending review.
    pub pending: Vec<ReviewComment>,
}

/// A review thread of a PR, as `GitHub::review_threads` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadState {
    /// The thread's GraphQL ID.
    pub id: String,
    pub resolved: bool,
    pub resolved_by: Option<Person>,
    /// The viewer may resolve the thread, or unresolve it if it's resolved.
    pub can_resolve: bool,
    pub side: Side,
}

/// Which branch of a PR `GitHub::prs_by_ref` matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefField {
    Head,
    Base,
}

/// An open or merged PR that `GitHub::prs_by_ref` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefPr {
    /// Its `parent` is always `None`; the stack walk fills it in.
    pub pr: StackPr,
    pub base_ref: String,
    pub head_ref: String,
    /// The head branch lives in a fork, so no PR can target it.
    pub cross_repo: bool,
    pub merged_at: Option<String>,
}

/// Where a PR that `GitHub::pr_statuses` looked up stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrStatus {
    /// OPEN, CLOSED or MERGED.
    pub state: String,
    /// Whether it conflicts with its base; `None` while GitHub is still
    /// working that out.
    pub conflicting: Option<bool>,
}

/// Parses an RFC 3339 timestamp into Unix seconds.
pub fn unix(ts: &str) -> Result<i64> {
    Ok(DateTime::parse_from_rfc3339(ts)
        .with_context(|| format!("bad timestamp {ts}"))?
        .timestamp())
}

/// Builds the GitHub search query for an inbox filter: `is:pr`, the
/// filter parenthesised because AND binds tighter than OR, the
/// repositories OR'd into one `repo:` group because space-separated
/// `repo:` qualifiers would AND, and any `sort:` tokens moved to the
/// end. A group is omitted when it would be empty.
pub fn search_query(filter: &str, repos: &[String]) -> String {
    let (sorts, terms): (Vec<&str>, Vec<&str>) = filter
        .split_whitespace()
        .partition(|t| t.starts_with("sort:"));
    let mut parts = vec!["is:pr".to_owned()];
    if !terms.is_empty() {
        parts.push(format!("({})", terms.join(" ")));
    }
    if !repos.is_empty() {
        let group = repos
            .iter()
            .map(|r| format!("repo:{r}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        parts.push(format!("({group})"));
    }
    parts.extend(sorts.iter().map(|s| s.to_string()));
    parts.join(" ")
}

pub const SEARCH_LIMIT: usize = 100;

/// What a search's PR fields are, beyond the PR's metadata: its review
/// decision, its review threads and the checks on its head.
const SEARCH_STATUS_FIELDS: &str = "reviewDecision
    reviewThreads(first: 100) { totalCount nodes { isResolved } }
    commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }";

/// Maps a check run's `CheckStatusState` and `CheckConclusionState`.
/// Checks that finished without passing or failing, e.g. skipped ones,
/// have no status.
fn check_run_status(status: &str, conclusion: &str) -> Option<CiStatus> {
    if status != "COMPLETED" {
        return Some(CiStatus::Running);
    }
    match conclusion {
        "SUCCESS" => Some(CiStatus::Passing),
        "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
            Some(CiStatus::Failing)
        }
        _ => None,
    }
}

fn merge_method(s: &str) -> Option<MergeMethod> {
    match s {
        "MERGE" => Some(MergeMethod::Merge),
        "SQUASH" => Some(MergeMethod::Squash),
        "REBASE" => Some(MergeMethod::Rebase),
        _ => None,
    }
}

/// Maps a `StatusState` of a commit's check rollup.
fn ci_status(state: &str) -> Option<CiStatus> {
    match state {
        "SUCCESS" => Some(CiStatus::Passing),
        "FAILURE" | "ERROR" => Some(CiStatus::Failing),
        "PENDING" | "EXPECTED" => Some(CiStatus::Running),
        _ => None,
    }
}

const PR_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      title body url state isDraft createdAt updatedAt
      author { login }
      baseRefName headRefName headRefOid
      headRepository { nameWithOwner }
      timelineItems(first: 100, itemTypes: [HEAD_REF_FORCE_PUSHED_EVENT]) {
        nodes {
          ... on HeadRefForcePushedEvent {
            createdAt
            beforeCommit { oid }
            afterCommit { oid }
            actor { login avatarUrl ... on User { name } }
          }
        }
      }
      commits(first: 100) {
        nodes { commit { oid checkSuites(first: 10) { nodes { createdAt } } } }
      }
      reviewDecision mergeable mergeStateStatus viewerCanMergeAsAdmin
      reviews(first: 100) {
        nodes { state body submittedAt url author { ...Who } }
      }
      pending: reviews(states: [PENDING], first: 1) {
        nodes {
          id viewerDidAuthor
          comments(first: 100) {
            nodes {
              id databaseId body path line startLine originalLine originalStartLine
              subjectType createdAt url
              replyTo { databaseId }
              originalCommit { oid }
            }
          }
        }
      }
      reviewRequests(first: 20) {
        nodes { requestedReviewer { ... on User { login } ... on Team { name } } }
      }
      comments(last: 100) {
        nodes { body createdAt url author { ...Who } }
      }
      head: commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              state
              contexts(first: 100) {
                nodes {
                  ... on CheckRun { name status conclusion detailsUrl }
                  ... on StatusContext { context state targetUrl }
                }
              }
            }
          }
        }
      }
    }
    mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed
    viewerDefaultMergeMethod deleteBranchOnMerge
  }
  viewer { login }
}

fragment Who on Actor { login avatarUrl ... on User { name } }
"#;

const THREADS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id diffSide
          isResolved viewerCanResolve viewerCanUnresolve
          resolvedBy { login name avatarUrl }
          comments(first: 1) { nodes { databaseId } }
        }
      }
    }
  }
}
"#;

/// The nodes of the connection at `ptr`.
fn nodes<'a>(v: &'a Value, ptr: &str) -> impl Iterator<Item = &'a Value> {
    v.pointer(ptr)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// The author of a review or comment; GitHub reports deleted users as
/// `null`, and shows them as "ghost".
fn author_at(v: &Value) -> Person {
    person_at(&v["author"]).unwrap_or_else(|| Person {
        login: Some("ghost".to_owned()),
        ..Person::default()
    })
}

/// The reviews, the conversation, the checks and the merge state from a
/// `PR_QUERY` response; `data` is the whole response, `pr` its pull
/// request.
fn review_info(data: &Value, pr: &Value) -> ReviewInfo {
    let reviews = nodes(pr, "/reviews/nodes")
        .filter_map(|r| {
            let state = match str_at(r, "/state") {
                "APPROVED" => ReviewState::Approved,
                "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
                "COMMENTED" => ReviewState::Commented,
                "DISMISSED" => ReviewState::Dismissed,
                // Pending reviews haven't been submitted yet.
                _ => return None,
            };
            Some(Review {
                author: author_at(r),
                state,
                body: str_at(r, "/body").to_owned(),
                submitted_at: str_at(r, "/submittedAt").to_owned(),
                url: str_at(r, "/url").to_owned(),
            })
        })
        .collect();
    let requested = nodes(pr, "/reviewRequests/nodes")
        .filter_map(|n| {
            let r = &n["requestedReviewer"];
            Some(str_at(r, "/login"))
                .filter(|s| !s.is_empty())
                .or_else(|| Some(str_at(r, "/name")).filter(|s| !s.is_empty()))
                .map(str::to_owned)
        })
        .collect();
    let comments = nodes(pr, "/comments/nodes")
        .map(|c| IssueComment {
            author: author_at(c),
            body: str_at(c, "/body").to_owned(),
            created_at: str_at(c, "/createdAt").to_owned(),
            url: str_at(c, "/url").to_owned(),
        })
        .collect();
    let rollup = pr
        .pointer("/head/nodes/0/commit/statusCheckRollup")
        .unwrap_or(&Value::Null);
    let checks = nodes(rollup, "/contexts/nodes")
        .filter_map(|c| {
            let opt = |ptr: &str| {
                Some(str_at(c, ptr))
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            };
            if let Some(name) = opt("/name") {
                Some(Check {
                    name,
                    status: check_run_status(str_at(c, "/status"), str_at(c, "/conclusion")),
                    url: opt("/detailsUrl"),
                })
            } else {
                Some(Check {
                    name: opt("/context")?,
                    status: ci_status(str_at(c, "/state")),
                    url: opt("/targetUrl"),
                })
            }
        })
        .collect();
    let repo = &data["repository"];
    let methods = [
        ("/mergeCommitAllowed", MergeMethod::Merge),
        ("/squashMergeAllowed", MergeMethod::Squash),
        ("/rebaseMergeAllowed", MergeMethod::Rebase),
    ]
    .into_iter()
    .filter(|(ptr, _)| repo.pointer(ptr).and_then(Value::as_bool) == Some(true))
    .map(|(_, m)| m)
    .collect();
    ReviewInfo {
        viewer: str_at(data, "/viewer/login").to_owned(),
        decision: Some(str_at(pr, "/reviewDecision"))
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        reviews,
        requested,
        comments,
        checks,
        ci: ci_status(str_at(rollup, "/state")),
        merge: MergeInfo {
            // GitHub says UNKNOWN until it has tried merging.
            conflicting: match str_at(pr, "/mergeable") {
                "CONFLICTING" => Some(true),
                "MERGEABLE" => Some(false),
                _ => None,
            },
            state: str_at(pr, "/mergeStateStatus").to_owned(),
            methods,
            default_method: merge_method(str_at(repo, "/viewerDefaultMergeMethod")),
            delete_branch_on_merge: repo["deleteBranchOnMerge"].as_bool().unwrap_or(false),
            can_merge_as_admin: pr["viewerCanMergeAsAdmin"].as_bool().unwrap_or(false),
        },
        pending_review: None,
    }
}

/// The viewer's pending review in a `PR_QUERY` response: its ID and its
/// comments.
fn pending_review(pr: &Value, viewer: &str) -> (Option<String>, Vec<ReviewComment>) {
    let Some(review) = nodes(pr, "/pending/nodes").find(|r| r["viewerDidAuthor"] == true) else {
        return (None, Vec::new());
    };
    let line = |c: &Value, ptr: &str| c.pointer(ptr).and_then(Value::as_u64).map(|n| n as u32);
    let comments = nodes(review, "/comments/nodes")
        .filter_map(|c| {
            Some(ReviewComment {
                id: c["databaseId"].as_u64()?,
                in_reply_to: c.pointer("/replyTo/databaseId").and_then(Value::as_u64),
                author: viewer.to_owned(),
                body: str_at(c, "/body").to_owned(),
                path: str_at(c, "/path").to_owned(),
                original_commit: str_at(c, "/originalCommit/oid").to_owned(),
                original_line: line(c, "/originalLine"),
                original_start_line: line(c, "/originalStartLine"),
                line: line(c, "/line"),
                start_line: line(c, "/startLine"),
                // Comments don't say which side they're on, their threads
                // do; the sync fills it in from those.
                side: Side::Right,
                on_file: str_at(c, "/subjectType") == "FILE",
                created_at: str_at(c, "/createdAt").to_owned(),
                url: str_at(c, "/url").to_owned(),
                resolved: false,
                resolved_by: None,
                thread_id: None,
                can_resolve: false,
                pending: true,
                node_id: Some(str_at(c, "/id").to_owned()),
            })
        })
        .collect();
    (Some(str_at(review, "/id").to_owned()), comments)
}

/// How many users or commits one batched GraphQL lookup asks for.
const LOOKUP_BATCH: usize = 50;

/// A user from GraphQL: `login`, `avatarUrl` and, for users (not bots),
/// `name`. `None` for an empty `login`.
fn person_at(v: &Value) -> Option<Person> {
    let login = str_at(v, "/login");
    if login.is_empty() {
        return None;
    }
    let opt = |ptr: &str| {
        Some(str_at(v, ptr))
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some(Person {
        login: Some(login.to_owned()),
        name: opt("/name"),
        avatar_url: opt("/avatarUrl"),
    })
}

fn split_repo(repo: &str) -> Result<(&str, &str)> {
    repo.split_once('/')
        .ok_or_else(|| anyhow!("bad repo {repo}"))
}

#[derive(Deserialize)]
struct RestComment {
    id: u64,
    in_reply_to_id: Option<u64>,
    user: Option<Login>,
    body: String,
    path: String,
    original_commit_id: String,
    original_line: Option<u32>,
    original_start_line: Option<u32>,
    line: Option<u32>,
    start_line: Option<u32>,
    side: Option<String>,
    subject_type: Option<String>,
    created_at: String,
    html_url: String,
}

#[derive(Deserialize)]
struct Login {
    login: String,
    #[serde(default)]
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct RestActivity {
    before: String,
    after: String,
    timestamp: String,
    activity_type: String,
    actor: Option<Login>,
}

fn str_at<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default()
}

/// The URL of the `rel="next"` page in a `Link` header.
fn next_link(headers: &header::HeaderMap) -> Option<String> {
    let link = headers.get(header::LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_owned()
        })
    })
}

impl GitHub {
    /// A client for the API at `api`, normally `API`. Tests point it at a
    /// fake GitHub, or at a dead port so requests fail fast.
    pub fn new(token: &str, api: &str) -> Result<GitHub> {
        let mut headers = header::HeaderMap::new();
        let mut auth = header::HeaderValue::from_str(&format!("Bearer {token}"))?;
        auth.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth);
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            header::HeaderValue::from_static("2022-11-28"),
        );
        let http = reqwest::Client::builder()
            .user_agent("grenadine")
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(GitHub {
            http,
            api: api.to_owned(),
        })
    }

    /// Runs a GraphQL query. Returns `data` and the `errors`, if any.
    async fn graphql(&self, query: &str, variables: Value) -> Result<(Value, Vec<Value>)> {
        let resp: Value = self
            .http
            .post(format!("{}/graphql", self.api))
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let errors = resp
            .get("errors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok((resp.get("data").cloned().unwrap_or(Value::Null), errors))
    }

    /// Runs a GraphQL mutation. Unlike a query's, any error fails it: a
    /// change must not half succeed silently.
    async fn mutate(&self, query: &str, variables: Value) -> Result<Value> {
        let (data, errors) = self.graphql(query, variables).await?;
        if let Some(e) = errors.first() {
            bail!(
                "GitHub refused the change: {}",
                e["message"].as_str().unwrap_or("unknown error")
            );
        }
        Ok(data)
    }

    /// The PR's GraphQL ID, which mutations on the PR itself need.
    pub async fn pr_id(&self, key: &PrKey) -> Result<String> {
        let (owner, name) = split_repo(&key.repo)?;
        let data = self
            .mutate(
                "query($owner: String!, $name: String!, $number: Int!) {
                   repository(owner: $owner, name: $name) { pullRequest(number: $number) { id } }
                 }",
                json!({ "owner": owner, "name": name, "number": key.number }),
            )
            .await?;
        Some(str_at(&data, "/repository/pullRequest/id"))
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("can't find {}#{}", key.repo, key.number))
    }

    /// The PR's GraphQL ID and the viewer's pending review on it, if any.
    pub async fn pending_review(&self, key: &PrKey) -> Result<(String, Option<String>)> {
        let (owner, name) = split_repo(&key.repo)?;
        let data = self
            .mutate(
                "query($owner: String!, $name: String!, $number: Int!) {
                   repository(owner: $owner, name: $name) {
                     pullRequest(number: $number) {
                       id
                       reviews(states: [PENDING], first: 1) { nodes { id viewerDidAuthor } }
                     }
                   }
                 }",
                json!({ "owner": owner, "name": name, "number": key.number }),
            )
            .await?;
        let pr = data
            .pointer("/repository/pullRequest")
            .filter(|v| !v.is_null())
            .ok_or_else(|| anyhow!("can't find {}#{}", key.repo, key.number))?;
        let review = nodes(pr, "/reviews/nodes")
            .find(|r| r["viewerDidAuthor"] == true)
            .map(|r| str_at(r, "/id").to_owned());
        Ok((str_at(pr, "/id").to_owned(), review))
    }

    /// The viewer's pending review on the PR, started if there is none.
    pub async fn ensure_pending_review(&self, key: &PrKey) -> Result<String> {
        let (pr_id, review) = self.pending_review(key).await?;
        if let Some(id) = review {
            return Ok(id);
        }
        let data = self
            .mutate(
                "mutation($pr: ID!) {
                   addPullRequestReview(input: { pullRequestId: $pr }) { pullRequestReview { id } }
                 }",
                json!({ "pr": pr_id }),
            )
            .await?;
        Some(str_at(&data, "/addPullRequestReview/pullRequestReview/id"))
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("GitHub didn't start a review"))
    }

    /// Adds a reply to a review thread: to the pending review `review` if
    /// given, and otherwise posted right away.
    pub async fn reply_to_thread(
        &self,
        thread_id: &str,
        body: &str,
        review: Option<&str>,
    ) -> Result<()> {
        self.mutate(
            "mutation($thread: ID!, $body: String!, $review: ID) {
               addPullRequestReviewThreadReply(input: {
                 pullRequestReviewThreadId: $thread, body: $body, pullRequestReviewId: $review
               }) {
                 clientMutationId
               }
             }",
            json!({ "thread": thread_id, "body": body, "review": review }),
        )
        .await?;
        Ok(())
    }

    /// Adds a thread to the pending review `review`.
    pub async fn add_review_thread(&self, review: &str, t: &NewThread) -> Result<()> {
        let side = match t.side {
            Side::Left => "LEFT",
            Side::Right => "RIGHT",
        };
        self.mutate(
            "mutation($review: ID!, $path: String!, $body: String!, $line: Int, $side: DiffSide,
                      $subject: PullRequestReviewThreadSubjectType) {
               addPullRequestReviewThread(input: {
                 pullRequestReviewId: $review, path: $path, body: $body, line: $line, side: $side,
                 subjectType: $subject
               }) {
                 clientMutationId
               }
             }",
            json!({
                "review": review,
                "path": t.path,
                "body": t.body,
                "line": t.line,
                "side": t.line.map(|_| side),
                "subject": if t.line.is_some() { "LINE" } else { "FILE" },
            }),
        )
        .await?;
        Ok(())
    }

    /// Edits a pending review comment by its GraphQL ID.
    pub async fn edit_review_comment(&self, comment: &str, body: &str) -> Result<()> {
        self.mutate(
            "mutation($comment: ID!, $body: String!) {
               updatePullRequestReviewComment(input: { pullRequestReviewCommentId: $comment, body: $body }) {
                 clientMutationId
               }
             }",
            json!({ "comment": comment, "body": body }),
        )
        .await?;
        Ok(())
    }

    /// Deletes a pending review comment by its GraphQL ID.
    pub async fn delete_review_comment(&self, comment: &str) -> Result<()> {
        self.mutate(
            "mutation($comment: ID!) {
               deletePullRequestReviewComment(input: { id: $comment }) { clientMutationId }
             }",
            json!({ "comment": comment }),
        )
        .await?;
        Ok(())
    }

    /// Submits the viewer's pending review, or, without one, reviews the PR
    /// with no comments.
    pub async fn submit_review(&self, key: &PrKey, s: &SubmitReview) -> Result<()> {
        let event = match s.event {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        };
        match self.pending_review(key).await? {
            (_, Some(review)) => {
                self.mutate(
                    "mutation($review: ID!, $event: PullRequestReviewEvent!, $body: String) {
                       submitPullRequestReview(input: { pullRequestReviewId: $review, event: $event, body: $body }) {
                         clientMutationId
                       }
                     }",
                    json!({ "review": review, "event": event, "body": s.body }),
                )
                .await?
            }
            (pr, None) => {
                self.mutate(
                    "mutation($pr: ID!, $event: PullRequestReviewEvent!, $body: String) {
                       addPullRequestReview(input: { pullRequestId: $pr, event: $event, body: $body }) {
                         clientMutationId
                       }
                     }",
                    json!({ "pr": pr, "event": event, "body": s.body }),
                )
                .await?
            }
        };
        Ok(())
    }

    /// Merges the PR. Returns a warning when it merged but its head branch
    /// couldn't be deleted.
    pub async fn merge(&self, key: &PrKey, m: &MergeRequest) -> Result<Option<String>> {
        let (owner, name) = split_repo(&key.repo)?;
        let data = self
            .mutate(
                "query($owner: String!, $name: String!, $number: Int!) {
                   repository(owner: $owner, name: $name) {
                     pullRequest(number: $number) { id headRef { id } }
                   }
                 }",
                json!({ "owner": owner, "name": name, "number": key.number }),
            )
            .await?;
        let pr = data
            .pointer("/repository/pullRequest")
            .filter(|v| !v.is_null())
            .ok_or_else(|| anyhow!("can't find {}#{}", key.repo, key.number))?;
        let method = match m.method {
            MergeMethod::Merge => "MERGE",
            MergeMethod::Squash => "SQUASH",
            MergeMethod::Rebase => "REBASE",
        };
        self.mutate(
            "mutation($pr: ID!, $method: PullRequestMergeMethod!, $head: GitObjectID!,
                      $title: String, $body: String) {
               mergePullRequest(input: {
                 pullRequestId: $pr, mergeMethod: $method, expectedHeadOid: $head,
                 commitHeadline: $title, commitBody: $body
               }) {
                 clientMutationId
               }
             }",
            json!({
                "pr": str_at(pr, "/id"),
                "method": method,
                "head": m.head_oid,
                "title": m.title,
                "body": m.body,
            }),
        )
        .await?;
        let head_ref = str_at(pr, "/headRef/id");
        if !m.delete_branch || head_ref.is_empty() {
            return Ok(None);
        }
        let deleted = self
            .mutate(
                "mutation($ref: ID!) { deleteRef(input: { refId: $ref }) { clientMutationId } }",
                json!({ "ref": head_ref }),
            )
            .await;
        Ok(deleted
            .err()
            .map(|e| format!("Merged, but the branch wasn't deleted: {e:#}")))
    }

    /// Discards the viewer's pending review and its comments.
    pub async fn discard_review(&self, key: &PrKey) -> Result<()> {
        let (_, Some(review)) = self.pending_review(key).await? else {
            return Ok(());
        };
        self.mutate(
            "mutation($review: ID!) {
               deletePullRequestReview(input: { pullRequestReviewId: $review }) { clientMutationId }
             }",
            json!({ "review": review }),
        )
        .await?;
        Ok(())
    }

    /// Resolves a review thread, or unresolves it.
    pub async fn set_thread_resolved(&self, thread_id: &str, resolved: bool) -> Result<()> {
        let field = if resolved {
            "resolveReviewThread"
        } else {
            "unresolveReviewThread"
        };
        self.mutate(
            &format!(
                "mutation($thread: ID!) {{ {field}(input: {{ threadId: $thread }}) {{ clientMutationId }} }}"
            ),
            json!({ "thread": thread_id }),
        )
        .await?;
        Ok(())
    }

    /// Comments on a PR (or an issue) by its GraphQL ID.
    pub async fn add_comment(&self, subject_id: &str, body: &str) -> Result<()> {
        self.mutate(
            "mutation($subject: ID!, $body: String!) {
               addComment(input: { subjectId: $subject, body: $body }) { clientMutationId }
             }",
            json!({ "subject": subject_id, "body": body }),
        )
        .await?;
        Ok(())
    }

    /// Runs several searches in one GraphQL request. Each result is either
    /// the PRs found, in GitHub's order, or the error for that search.
    /// `ISSUE_ADVANCED` is what makes AND, OR and parentheses work; plain
    /// `ISSUE` finds nothing for a query with parentheses.
    pub async fn search(
        &self,
        queries: &[String],
    ) -> Result<Vec<std::result::Result<Vec<Hit>, String>>> {
        if queries.is_empty() {
            return Ok(Vec::new());
        }
        let mut gql = String::from("query(");
        gql += &(0..queries.len())
            .map(|i| format!("$q{i}: String!"))
            .collect::<Vec<_>>()
            .join(", ");
        gql += ") {\n";
        let mut vars = serde_json::Map::new();
        for (i, q) in queries.iter().enumerate() {
            gql += &format!(
                "s{i}: search(type: ISSUE_ADVANCED, query: $q{i}, first: {SEARCH_LIMIT}) {{
                   nodes {{ ... on PullRequest {{ number title author {{ login }} state isDraft url updatedAt headRefOid repository {{ nameWithOwner }} {SEARCH_STATUS_FIELDS} }} }}
                 }}\n"
            );
            vars.insert(format!("q{i}"), json!(q));
        }
        gql += "}";
        let (data, errors) = self.graphql(&gql, Value::Object(vars)).await?;
        let mut error_for: BTreeMap<String, String> = BTreeMap::new();
        for e in &errors {
            let alias = e
                .pointer("/path/0")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let msg = e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            error_for
                .entry(alias.to_owned())
                .or_insert_with(|| msg.to_owned());
        }
        Ok((0..queries.len())
            .map(|i| {
                let alias = format!("s{i}");
                let Some(nodes) = data
                    .pointer(&format!("/{alias}/nodes"))
                    .and_then(Value::as_array)
                else {
                    return Err(error_for
                        .get(&alias)
                        .or_else(|| error_for.get(""))
                        .cloned()
                        .unwrap_or_else(|| "search failed".to_owned()));
                };
                Ok(nodes
                    .iter()
                    .filter(|n| n.get("number").is_some())
                    .map(|n| Hit {
                        key: PrKey {
                            repo: str_at(n, "/repository/nameWithOwner").to_owned(),
                            number: n["number"].as_u64().unwrap_or_default(),
                        },
                        title: str_at(n, "/title").to_owned(),
                        author: Some(str_at(n, "/author/login"))
                            .filter(|s| !s.is_empty())
                            .unwrap_or("ghost")
                            .to_owned(),
                        state: str_at(n, "/state").to_owned(),
                        is_draft: n["isDraft"].as_bool().unwrap_or_default(),
                        url: str_at(n, "/url").to_owned(),
                        updated_at: str_at(n, "/updatedAt").to_owned(),
                        head_oid: str_at(n, "/headRefOid").to_owned(),
                        threads: ThreadCounts {
                            // Only the first 100 threads say whether
                            // they're resolved; that's plenty in practice.
                            resolved: n
                                .pointer("/reviewThreads/nodes")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter(|t| t["isResolved"].as_bool() == Some(true))
                                .count() as u32,
                            total: n
                                .pointer("/reviewThreads/totalCount")
                                .and_then(Value::as_u64)
                                .unwrap_or_default() as u32,
                        },
                        ci: ci_status(str_at(n, "/commits/nodes/0/commit/statusCheckRollup/state")),
                        review_decision: Some(str_at(n, "/reviewDecision"))
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    })
                    .collect())
            })
            .collect())
    }

    /// The open and merged PRs of `repo` whose head (or base) branch is
    /// each of `refs`, all in one GraphQL request.
    pub async fn prs_by_ref(
        &self,
        repo: &str,
        field: RefField,
        refs: &[String],
    ) -> Result<Vec<Vec<RefPr>>> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow!("bad repo {repo}"))?;
        let arg = match field {
            RefField::Head => "headRefName",
            RefField::Base => "baseRefName",
        };
        let mut gql = String::from("query($owner: String!, $name: String!");
        for i in 0..refs.len() {
            gql += &format!(", $r{i}: String!");
        }
        gql += ") {\n  repository(owner: $owner, name: $name) {\n";
        let mut vars = serde_json::Map::new();
        vars.insert("owner".into(), json!(owner));
        vars.insert("name".into(), json!(name));
        for (i, r) in refs.iter().enumerate() {
            gql += &format!(
                "    p{i}: pullRequests({arg}: $r{i}, states: [OPEN, MERGED], first: 100) {{
                       nodes {{ number title state isDraft url updatedAt mergedAt headRefOid
                                headRefName baseRefName isCrossRepository }}
                     }}\n"
            );
            vars.insert(format!("r{i}"), json!(r));
        }
        gql += "  }\n}";
        let (data, errors) = self.graphql(&gql, Value::Object(vars)).await?;
        (0..refs.len())
            .map(|i| {
                let nodes = data
                    .pointer(&format!("/repository/p{i}/nodes"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        anyhow!(
                            "can't look up PRs by branch in {repo}: {}",
                            errors
                                .first()
                                .and_then(|e| e["message"].as_str())
                                .unwrap_or("no data")
                        )
                    })?;
                Ok(nodes
                    .iter()
                    .filter(|n| n.get("number").is_some())
                    .map(|n| RefPr {
                        pr: StackPr {
                            number: n["number"].as_u64().unwrap_or_default(),
                            title: str_at(n, "/title").to_owned(),
                            state: str_at(n, "/state").to_owned(),
                            is_draft: n["isDraft"].as_bool().unwrap_or_default(),
                            url: str_at(n, "/url").to_owned(),
                            updated_at: str_at(n, "/updatedAt").to_owned(),
                            head_oid: str_at(n, "/headRefOid").to_owned(),
                            parent: None,
                        },
                        base_ref: str_at(n, "/baseRefName").to_owned(),
                        head_ref: str_at(n, "/headRefName").to_owned(),
                        cross_repo: n["isCrossRepository"].as_bool().unwrap_or_default(),
                        merged_at: Some(str_at(n, "/mergedAt"))
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    })
                    .collect())
            })
            .collect()
    }

    pub async fn pr(&self, key: &PrKey) -> Result<PrData> {
        let (owner, name) = split_repo(&key.repo)?;
        let (data, errors) = self
            .graphql(
                PR_QUERY,
                json!({ "owner": owner, "name": name, "number": key.number }),
            )
            .await?;
        let pr = data
            .pointer("/repository/pullRequest")
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                anyhow!(
                    "can't read {}#{}: {}",
                    key.repo,
                    key.number,
                    errors
                        .first()
                        .and_then(|e| e["message"].as_str())
                        .unwrap_or("not found")
                )
            })?;

        let mut force_pushes = Vec::new();
        for n in pr
            .pointer("/timelineItems/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let after = str_at(n, "/afterCommit/oid");
            if after.is_empty() {
                continue;
            }
            force_pushes.push(ForcePush {
                timestamp: unix(str_at(n, "/createdAt"))?,
                before: Some(str_at(n, "/beforeCommit/oid"))
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                after: after.to_owned(),
                actor: n.get("actor").and_then(person_at),
            });
        }
        let commits = pr
            .pointer("/commits/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|n| {
                let first_suite = n
                    .pointer("/commit/checkSuites/nodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|s| unix(str_at(s, "/createdAt")).ok())
                    .min();
                (str_at(n, "/commit/oid").to_owned(), first_suite)
            })
            .collect();

        let review = review_info(&data, pr);
        let (pending_review, pending) = pending_review(pr, &review.viewer);
        Ok(PrData {
            title: str_at(pr, "/title").to_owned(),
            body: str_at(pr, "/body").to_owned(),
            author: Some(str_at(pr, "/author/login"))
                .filter(|s| !s.is_empty())
                .unwrap_or("ghost")
                .to_owned(),
            state: str_at(pr, "/state").to_owned(),
            is_draft: pr["isDraft"].as_bool().unwrap_or(false),
            url: str_at(pr, "/url").to_owned(),
            created_at: str_at(pr, "/createdAt").to_owned(),
            updated_at: str_at(pr, "/updatedAt").to_owned(),
            base_ref: str_at(pr, "/baseRefName").to_owned(),
            head_ref: str_at(pr, "/headRefName").to_owned(),
            head_oid: str_at(pr, "/headRefOid").to_owned(),
            head_repo: Some(str_at(pr, "/headRepository/nameWithOwner"))
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            force_pushes,
            commits,
            review: ReviewInfo {
                pending_review,
                ..review
            },
            pending,
        })
    }

    /// Opens a pull request from `head` into `base`, both branches of
    /// `repo`. Returns its number and URL.
    pub async fn create_pr(
        &self,
        repo: &str,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<(u64, String)> {
        let resp = self
            .http
            .post(format!("{}/repos/{repo}/pulls", self.api))
            .json(&json!({ "head": head, "base": base, "title": title, "body": body }))
            .send()
            .await?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            let reason = v["errors"][0]["message"]
                .as_str()
                .or(v["message"].as_str())
                .unwrap_or("no reason given");
            bail!("GitHub didn't open the PR ({status}): {reason}");
        }
        let number = v["number"]
            .as_u64()
            .ok_or_else(|| anyhow!("GitHub's response has no PR number"))?;
        Ok((number, str_at(&v, "/html_url").to_owned()))
    }

    /// GETs every page of a REST list.
    async fn get_all<T: serde::de::DeserializeOwned>(&self, url: String) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut next = Some(url);
        while let Some(url) = next.take() {
            let resp = self.http.get(&url).send().await?.error_for_status()?;
            next = next_link(resp.headers());
            out.extend(resp.json::<Vec<T>>().await?);
        }
        Ok(out)
    }

    /// The activity log of a branch, oldest first. Empty when GitHub has no
    /// log for it, e.g. because the repository is gone.
    pub async fn activity(&self, repo: &str, branch: &str) -> Result<Vec<Activity>> {
        let url = format!(
            "{}/repos/{repo}/activity?ref=refs/heads/{branch}&direction=asc&per_page=100",
            self.api
        );
        let raw: Vec<RestActivity> = match self.get_all(url).await {
            Ok(raw) => raw,
            Err(e)
                if e.downcast_ref::<reqwest::Error>()
                    .and_then(reqwest::Error::status)
                    == Some(reqwest::StatusCode::NOT_FOUND) =>
            {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let mut out = raw
            .into_iter()
            .map(|a| {
                Ok(Activity {
                    timestamp: unix(&a.timestamp)?,
                    kind: match a.activity_type.as_str() {
                        "push" => ActivityKind::Push,
                        "force_push" => ActivityKind::ForcePush,
                        "branch_creation" => ActivityKind::BranchCreation,
                        "branch_deletion" => ActivityKind::BranchDeletion,
                        _ => ActivityKind::Other,
                    },
                    before: a.before,
                    after: a.after,
                    actor: a.actor.map(|u| Person {
                        login: Some(u.login),
                        name: None,
                        avatar_url: u.avatar_url,
                    }),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // `direction=asc` should already give this order; don't rely on it.
        out.sort_by_key(|a| a.timestamp);
        Ok(out)
    }

    pub async fn review_comments(&self, key: &PrKey) -> Result<Vec<ReviewComment>> {
        let url = format!(
            "{}/repos/{}/pulls/{}/comments?per_page=100",
            self.api, key.repo, key.number
        );
        let raw: Vec<RestComment> = self.get_all(url).await?;
        Ok(raw
            .into_iter()
            .map(|c| ReviewComment {
                id: c.id,
                in_reply_to: c.in_reply_to_id,
                author: c
                    .user
                    .map(|u| u.login)
                    .unwrap_or_else(|| "ghost".to_owned()),
                body: c.body,
                path: c.path,
                original_commit: c.original_commit_id,
                original_line: c.original_line,
                original_start_line: c.original_start_line,
                line: c.line,
                start_line: c.start_line,
                side: if c.side.as_deref() == Some("LEFT") {
                    Side::Left
                } else {
                    Side::Right
                },
                on_file: c.subject_type.as_deref() == Some("file"),
                created_at: c.created_at,
                url: c.html_url,
                resolved: false,
                resolved_by: None,
                thread_id: None,
                can_resolve: false,
                pending: false,
                node_id: None,
            })
            .collect())
    }

    /// The PR's review threads, keyed by the ID of each one's first comment.
    pub async fn review_threads(&self, key: &PrKey) -> Result<BTreeMap<u64, ThreadState>> {
        let (owner, name) = split_repo(&key.repo)?;
        let mut out = BTreeMap::new();
        let mut after: Option<String> = None;
        loop {
            let (data, errors) = self
                .graphql(
                    THREADS_QUERY,
                    json!({ "owner": owner, "name": name, "number": key.number, "after": after }),
                )
                .await?;
            let threads = data
                .pointer("/repository/pullRequest/reviewThreads")
                .filter(|v| !v.is_null())
                .ok_or_else(|| {
                    anyhow!(
                        "can't read the review threads of {}#{}: {}",
                        key.repo,
                        key.number,
                        errors
                            .first()
                            .and_then(|e| e["message"].as_str())
                            .unwrap_or("not found")
                    )
                })?;
            out.extend(nodes(threads, "/nodes").filter_map(|t| {
                let id = t.pointer("/comments/nodes/0/databaseId")?.as_u64()?;
                let resolved = t["isResolved"].as_bool() == Some(true);
                let can = if resolved {
                    "viewerCanUnresolve"
                } else {
                    "viewerCanResolve"
                };
                Some((
                    id,
                    ThreadState {
                        id: str_at(t, "/id").to_owned(),
                        resolved,
                        resolved_by: resolved.then(|| person_at(&t["resolvedBy"])).flatten(),
                        can_resolve: t[can].as_bool() == Some(true),
                        side: if str_at(t, "/diffSide") == "LEFT" {
                            Side::Left
                        } else {
                            Side::Right
                        },
                    },
                ))
            }));
            if threads
                .pointer("/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                != Some(true)
            {
                return Ok(out);
            }
            after = Some(str_at(threads, "/pageInfo/endCursor").to_owned());
        }
    }

    /// Looks up users by login. Logins GitHub doesn't know as a user, e.g.
    /// bots, are missing from the result.
    pub async fn users(&self, logins: &[String]) -> Result<BTreeMap<String, Person>> {
        let mut out = BTreeMap::new();
        for chunk in logins.chunks(LOOKUP_BATCH) {
            let params = (0..chunk.len())
                .map(|i| format!("$l{i}: String!"))
                .collect::<Vec<_>>()
                .join(", ");
            let fields: String = (0..chunk.len())
                .map(|i| format!("u{i}: user(login: $l{i}) {{ login name avatarUrl }}\n"))
                .collect();
            let vars: serde_json::Map<String, Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, l)| (format!("l{i}"), json!(l)))
                .collect();
            // A login that isn't a user is an error for its alias only.
            let (data, _) = self
                .graphql(
                    &format!("query({params}) {{\n{fields}}}"),
                    Value::Object(vars),
                )
                .await?;
            for (i, login) in chunk.iter().enumerate() {
                if let Some(p) = data.get(format!("u{i}")).and_then(person_at) {
                    out.insert(login.clone(), p);
                }
            }
        }
        Ok(out)
    }

    /// The states of PRs and whether they conflict with their bases. PRs
    /// GitHub can't find, e.g. in a repository the token can't see, are
    /// missing from the result.
    pub async fn pr_statuses(&self, keys: &[PrKey]) -> Result<BTreeMap<PrKey, PrStatus>> {
        let mut out = BTreeMap::new();
        for chunk in keys.chunks(LOOKUP_BATCH) {
            let params = (0..chunk.len())
                .map(|i| format!("$o{i}: String!, $n{i}: String!, $x{i}: Int!"))
                .collect::<Vec<_>>()
                .join(", ");
            let fields: String = (0..chunk.len())
                .map(|i| {
                    format!(
                        "p{i}: repository(owner: $o{i}, name: $n{i}) {{ pullRequest(number: $x{i}) {{ state mergeable }} }}\n"
                    )
                })
                .collect();
            let mut vars = serde_json::Map::new();
            for (i, key) in chunk.iter().enumerate() {
                let (owner, name) = split_repo(&key.repo)?;
                vars.insert(format!("o{i}"), json!(owner));
                vars.insert(format!("n{i}"), json!(name));
                vars.insert(format!("x{i}"), json!(key.number));
            }
            // A PR that can't be found is an error for its alias only.
            let (data, _) = self
                .graphql(
                    &format!("query({params}) {{\n{fields}}}"),
                    Value::Object(vars),
                )
                .await?;
            for (i, key) in chunk.iter().enumerate() {
                let state = str_at(&data, &format!("/p{i}/pullRequest/state"));
                if state.is_empty() {
                    continue;
                }
                // GitHub says UNKNOWN until it has tried merging.
                let conflicting = match str_at(&data, &format!("/p{i}/pullRequest/mergeable")) {
                    "CONFLICTING" => Some(true),
                    "MERGEABLE" => Some(false),
                    _ => None,
                };
                out.insert(
                    key.clone(),
                    PrStatus {
                        state: state.to_owned(),
                        conflicting,
                    },
                );
            }
        }
        Ok(out)
    }

    /// The authors of commits by SHA. A commit whose author isn't linked to
    /// a GitHub user gets the name from the commit and no login. Commits
    /// GitHub doesn't have are missing from the result.
    pub async fn commit_authors(
        &self,
        repo: &str,
        shas: &[String],
    ) -> Result<BTreeMap<String, Person>> {
        let (owner, name) = split_repo(repo)?;
        let mut out = BTreeMap::new();
        for chunk in shas.chunks(LOOKUP_BATCH) {
            let mut params = vec!["$owner: String!".to_owned(), "$name: String!".to_owned()];
            params.extend((0..chunk.len()).map(|i| format!("$o{i}: GitObjectID!")));
            let fields: String = (0..chunk.len())
                .map(|i| {
                    format!(
                        "c{i}: object(oid: $o{i}) {{ ... on Commit {{ author {{ name user {{ login name avatarUrl }} }} }} }}\n"
                    )
                })
                .collect();
            let mut vars = serde_json::Map::new();
            vars.insert("owner".into(), json!(owner));
            vars.insert("name".into(), json!(name));
            for (i, sha) in chunk.iter().enumerate() {
                vars.insert(format!("o{i}"), json!(sha));
            }
            let (data, _) = self
                .graphql(
                    &format!(
                        "query({}) {{\nrepository(owner: $owner, name: $name) {{\n{fields}}}\n}}",
                        params.join(", ")
                    ),
                    Value::Object(vars),
                )
                .await?;
            for (i, sha) in chunk.iter().enumerate() {
                let Some(author) = data
                    .pointer(&format!("/repository/c{i}/author"))
                    .filter(|v| !v.is_null())
                else {
                    continue;
                };
                let person = author.get("user").and_then(person_at).or_else(|| {
                    Some(str_at(author, "/name"))
                        .filter(|s| !s.is_empty())
                        .map(|n| Person {
                            login: None,
                            name: Some(n.to_owned()),
                            avatar_url: None,
                        })
                });
                if let Some(p) = person {
                    out.insert(sha.clone(), p);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_scoped_to_the_repos() {
        assert_eq!(
            search_query(" is:open author:@me ", &["a/b".into(), "c/d".into()]),
            "is:pr (is:open author:@me) (repo:a/b OR repo:c/d)"
        );
        // An OR in the filter stays inside its parentheses; `sort:`
        // tokens move to the end, in order.
        assert_eq!(
            search_query(
                "author:@me (x:y OR z:w) sort:updated-desc sort:comments",
                &["a/b".into(), "c/d".into()]
            ),
            "is:pr (author:@me (x:y OR z:w)) (repo:a/b OR repo:c/d) sort:updated-desc sort:comments"
        );
        assert_eq!(
            search_query("is:open", &["a/b".into()]),
            "is:pr (is:open) (repo:a/b)"
        );
        assert_eq!(search_query("is:open", &[]), "is:pr (is:open)");
        assert_eq!(search_query(" ", &["a/b".into()]), "is:pr (repo:a/b)");
        assert_eq!(search_query("", &[]), "is:pr");
    }

    #[test]
    fn parses_link_headers() {
        let mut h = header::HeaderMap::new();
        h.insert(
            header::LINK,
            header::HeaderValue::from_static(
                r#"<https://x/?page=2>; rel="next", <https://x/?page=5>; rel="last""#,
            ),
        );
        assert_eq!(next_link(&h).as_deref(), Some("https://x/?page=2"));
        h.insert(
            header::LINK,
            header::HeaderValue::from_static(r#"<https://x/?page=1>; rel="prev""#),
        );
        assert_eq!(next_link(&h), None);
    }

    #[test]
    fn ci_states() {
        assert_eq!(ci_status("SUCCESS"), Some(CiStatus::Passing));
        assert_eq!(ci_status("FAILURE"), Some(CiStatus::Failing));
        assert_eq!(ci_status("ERROR"), Some(CiStatus::Failing));
        assert_eq!(ci_status("PENDING"), Some(CiStatus::Running));
        assert_eq!(ci_status("EXPECTED"), Some(CiStatus::Running));
        assert_eq!(ci_status(""), None);
    }

    #[test]
    fn reads_reviews_checks_and_the_merge_state() {
        let data = json!({
            "viewer": { "login": "me" },
            "repository": {
                "mergeCommitAllowed": false,
                "squashMergeAllowed": true,
                "rebaseMergeAllowed": true,
                "viewerDefaultMergeMethod": "REBASE",
                "deleteBranchOnMerge": true,
                "pullRequest": {
                    "reviewDecision": "REVIEW_REQUIRED",
                    "mergeable": "UNKNOWN",
                    "mergeStateStatus": "BLOCKED",
                    "viewerCanMergeAsAdmin": true,
                    "reviews": { "nodes": [
                        { "state": "PENDING", "body": "", "submittedAt": null, "url": "u0",
                          "author": { "login": "me" } },
                        { "state": "CHANGES_REQUESTED", "body": "No", "submittedAt": "t1",
                          "url": "u1", "author": null },
                    ] },
                    "reviewRequests": { "nodes": [
                        { "requestedReviewer": { "login": "a" } },
                        { "requestedReviewer": { "name": "team" } },
                        { "requestedReviewer": null },
                    ] },
                    "comments": { "nodes": [
                        { "body": "hi", "createdAt": "t2", "url": "u2",
                          "author": { "login": "b", "name": "B", "avatarUrl": "av" } },
                    ] },
                    "head": { "nodes": [ { "commit": { "statusCheckRollup": {
                        "state": "PENDING",
                        "contexts": { "nodes": [
                            { "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS",
                              "detailsUrl": "d" },
                            { "name": "test", "status": "IN_PROGRESS", "conclusion": null },
                            { "name": "docs", "status": "COMPLETED", "conclusion": "SKIPPED" },
                            { "context": "ci/legacy", "state": "ERROR", "targetUrl": null },
                        ] },
                    } } } ] },
                },
            },
        });
        let info = review_info(&data, &data["repository"]["pullRequest"]);
        assert_eq!(info.viewer, "me");
        assert_eq!(info.decision.as_deref(), Some("REVIEW_REQUIRED"));
        let [review] = &info.reviews[..] else {
            panic!("{:?}", info.reviews)
        };
        assert_eq!(review.state, ReviewState::ChangesRequested);
        assert_eq!(review.author.login.as_deref(), Some("ghost"));
        assert_eq!(info.requested, ["a", "team"]);
        assert_eq!(info.comments[0].author.name.as_deref(), Some("B"));
        let checks: Vec<_> = info
            .checks
            .iter()
            .map(|c| (c.name.as_str(), c.status, c.url.as_deref()))
            .collect();
        assert_eq!(
            checks,
            [
                ("lint", Some(CiStatus::Passing), Some("d")),
                ("test", Some(CiStatus::Running), None),
                ("docs", None, None),
                ("ci/legacy", Some(CiStatus::Failing), None),
            ]
        );
        assert_eq!(info.ci, Some(CiStatus::Running));
        assert_eq!(
            info.merge,
            MergeInfo {
                conflicting: None,
                state: "BLOCKED".into(),
                methods: vec![MergeMethod::Squash, MergeMethod::Rebase],
                default_method: Some(MergeMethod::Rebase),
                delete_branch_on_merge: true,
                can_merge_as_admin: true,
            }
        );
    }

    #[test]
    fn timestamps() {
        assert_eq!(unix("1970-01-01T00:01:00Z").unwrap(), 60);
        assert!(unix("nope").is_err());
    }
}
