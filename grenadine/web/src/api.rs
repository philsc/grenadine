//! Talks to the grenadine server. Tree comparisons and blobs never change,
//! so they are cached for the life of the page.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use gloo_net::http::{Request, Response};
use grenadine_core::api::{
    AgentMessage, AgentModelInfo, AgentSession, Approval, Blob, BlobsRequest, BlobsResponse,
    Changes, CleanupState, InboxEdit, InboxWithPrs, NewAgent, NewCleanupSession, NewComment,
    NewThread, PermissionMode, PrDetail, PrKey, PrMissing, PromptImage, SubmitReview, SyncStatus,
    TerminalInfo, WorktreeChanges, WrapUp,
};

pub type Result<T> = std::result::Result<T, String>;

thread_local! {
    static CHANGES: RefCell<HashMap<(String, String, String), Arc<Changes>>> = RefCell::default();
    static BLOBS: RefCell<HashMap<String, Arc<Blob>>> = RefCell::default();
}

async fn check(resp: std::result::Result<Response, gloo_net::Error>) -> Result<Response> {
    let resp = resp.map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(resp)
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Err(format!("{status}: {text}"))
    }
}

pub async fn inboxes() -> Result<Vec<InboxWithPrs>> {
    check(Request::get("/api/inboxes").send().await)
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())
}

pub async fn create_inbox(edit: &InboxEdit) -> Result<()> {
    let req = Request::post("/api/inboxes")
        .json(edit)
        .map_err(|e| e.to_string())?;
    check(req.send().await).await.map(|_| ())
}

pub async fn update_inbox(id: i64, edit: &InboxEdit) -> Result<()> {
    let req = Request::put(&format!("/api/inboxes/{id}"))
        .json(edit)
        .map_err(|e| e.to_string())?;
    check(req.send().await).await.map(|_| ())
}

pub async fn delete_inbox(id: i64) -> Result<()> {
    check(Request::delete(&format!("/api/inboxes/{id}")).send().await)
        .await
        .map(|_| ())
}

/// What `/api/pr` answered for a PR that isn't synced.
#[derive(Clone, Debug, PartialEq)]
pub enum PrResponse {
    Ready(Box<PrDetail>),
    /// The repository isn't one of the server's configured repos.
    NotConfigured,
    /// Known but not synced yet; a sync may be in flight.
    Pending,
    /// The last sync attempt failed; carries its error.
    Failed(String),
}

pub async fn pr(key: &PrKey) -> Result<PrResponse> {
    let resp = Request::get(&format!("/api/pr/{}/{}", key.repo, key.number))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status() == 404 {
        let missing: PrMissing = resp.json().await.unwrap_or(PrMissing::NotSynced);
        return Ok(match missing {
            PrMissing::NotConfigured => PrResponse::NotConfigured,
            PrMissing::NotSynced => PrResponse::Pending,
            PrMissing::SyncFailed(e) => PrResponse::Failed(e),
        });
    }
    if !resp.ok() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("{status}: {text}"));
    }
    resp.json()
        .await
        .map(|d| PrResponse::Ready(Box::new(d)))
        .map_err(|e| e.to_string())
}

pub async fn sync_status() -> Result<SyncStatus> {
    check(Request::get("/api/sync").send().await)
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())
}

/// The files that differ between two commits.
pub async fn changes(repo: &str, from: &str, to: &str) -> Result<Arc<Changes>> {
    let cache_key = (repo.to_owned(), from.to_owned(), to.to_owned());
    if let Some(c) = CHANGES.with_borrow(|m| m.get(&cache_key).cloned()) {
        return Ok(c);
    }
    let url = format!("/api/changes?repo={repo}&from={from}&to={to}");
    let changes: Changes = check(Request::get(&url).send().await)
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let changes = Arc::new(changes);
    CHANGES.with_borrow_mut(|m| m.insert(cache_key, changes.clone()));
    Ok(changes)
}

/// Fetches the blobs that aren't cached yet.
pub async fn load_blobs(repo: &str, ids: impl IntoIterator<Item = String>) -> Result<()> {
    let mut missing: Vec<String> = ids
        .into_iter()
        .filter(|id| !BLOBS.with_borrow(|m| m.contains_key(id)))
        .collect();
    missing.sort();
    missing.dedup();
    // Keep requests to a reasonable size.
    for chunk in missing.chunks(200) {
        let body = BlobsRequest {
            repo: repo.to_owned(),
            ids: chunk.to_vec(),
        };
        let req = Request::post("/api/blobs")
            .json(&body)
            .map_err(|e| e.to_string())?;
        let resp: BlobsResponse = check(req.send().await)
            .await?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        BLOBS.with_borrow_mut(|m| {
            for (id, blob) in resp.blobs {
                m.insert(id, Arc::new(blob));
            }
        });
    }
    Ok(())
}

/// A blob that `load_blobs` fetched.
pub fn blob(id: &str) -> Option<Arc<Blob>> {
    BLOBS.with_borrow(|m| m.get(id).cloned())
}

/// The configured repositories.
pub async fn repos() -> Result<Vec<String>> {
    check(Request::get("/api/repos").send().await)
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())
}

/// The models Claude Code offers; empty when it can't list them.
pub async fn agent_models() -> Result<Vec<AgentModelInfo>> {
    check(Request::get("/api/agents/models").send().await)
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())
}

pub async fn agents() -> Result<Vec<AgentSession>> {
    check(Request::get("/api/agents").send().await)
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())
}

/// `None` when there is no such session.
pub async fn agent(id: &str) -> Result<Option<AgentSession>> {
    let resp = Request::get(&format!("/api/agents/{id}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status() == 404 {
        return Ok(None);
    }
    check(Ok(resp))
        .await?
        .json()
        .await
        .map(Some)
        .map_err(|e| e.to_string())
}

/// The unstaged changes in a session's worktree.
pub async fn agent_changes(id: &str) -> Result<WorktreeChanges> {
    check(
        Request::get(&format!("/api/agents/{id}/changes"))
            .send()
            .await,
    )
    .await?
    .json()
    .await
    .map_err(|e| e.to_string())
}

/// The session's open terminals, oldest first.
pub async fn terminals(id: &str) -> Result<Vec<TerminalInfo>> {
    check(
        Request::get(&format!("/api/agents/{id}/terminals"))
            .send()
            .await,
    )
    .await?
    .json()
    .await
    .map_err(|e| e.to_string())
}

/// Starts a login shell in the session's worktree.
pub async fn open_terminal(id: &str) -> Result<TerminalInfo> {
    check(
        Request::post(&format!("/api/agents/{id}/terminals"))
            .send()
            .await,
    )
    .await?
    .json()
    .await
    .map_err(|e| e.to_string())
}

pub async fn close_terminal(id: &str, terminal: u64) -> Result<()> {
    check(
        Request::delete(&format!("/api/agents/{id}/terminals/{terminal}"))
            .send()
            .await,
    )
    .await
    .map(|_| ())
}

pub async fn create_agent(req: &NewAgent) -> Result<AgentSession> {
    let req = Request::post("/api/agents")
        .json(req)
        .map_err(|e| e.to_string())?;
    check(req.send().await)
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())
}

pub async fn send_agent_message(
    id: &str,
    prompt: String,
    images: Vec<PromptImage>,
    permission_mode: PermissionMode,
    model: Option<String>,
) -> Result<()> {
    let req = Request::post(&format!("/api/agents/{id}/messages"))
        .json(&AgentMessage {
            prompt,
            permission_mode,
            model,
            images,
        })
        .map_err(|e| e.to_string())?;
    check(req.send().await).await.map(|_| ())
}

pub async fn interrupt_agent(id: &str) -> Result<()> {
    check(
        Request::post(&format!("/api/agents/{id}/interrupt"))
            .send()
            .await,
    )
    .await
    .map(|_| ())
}

pub async fn approve(id: &str, approval: &Approval) -> Result<()> {
    let req = Request::post(&format!("/api/agents/{id}/approvals"))
        .json(approval)
        .map_err(|e| e.to_string())?;
    check(req.send().await).await.map(|_| ())
}

pub async fn delete_agent(id: &str) -> Result<()> {
    check(Request::delete(&format!("/api/agents/{id}")).send().await)
        .await
        .map(|_| ())
}

pub async fn cleanup() -> Result<CleanupState> {
    check(Request::get("/api/cleanup").send().await)
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())
}

pub async fn delete_cleanup_item(id: i64) -> Result<()> {
    check(Request::delete(&format!("/api/cleanup/{id}")).send().await)
        .await
        .map(|_| ())
}

/// Hands cleanup items off to a new agent session, in plan mode.
pub async fn create_cleanup_session(req: &NewCleanupSession) -> Result<AgentSession> {
    let req = Request::post("/api/cleanup/sessions")
        .json(req)
        .map_err(|e| e.to_string())?;
    check(req.send().await)
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())
}

pub async fn wrap_up_agent(id: &str, wrap_up: &WrapUp) -> Result<()> {
    let req = Request::post(&format!("/api/agents/{id}/wrap-up"))
        .json(wrap_up)
        .map_err(|e| e.to_string())?;
    check(req.send().await).await.map(|_| ())
}

fn pr_url(key: &PrKey) -> String {
    format!("/api/pr/{}/{}", key.repo, key.number)
}

/// Comments on the PR itself.
pub async fn add_comment(key: &PrKey, body: String) -> Result<()> {
    let req = Request::post(&format!("{}/comments", pr_url(key)))
        .json(&NewComment {
            body,
            review: false,
        })
        .map_err(|e| e.to_string())?;
    check(req.send().await).await.map(|_| ())
}

/// Replies to a thread: right away, or in the viewer's pending review if
/// `review`.
pub async fn reply_to_thread(key: &PrKey, thread: &str, body: String, review: bool) -> Result<()> {
    let req = Request::post(&format!("{}/threads/{thread}/replies", pr_url(key)))
        .json(&NewComment { body, review })
        .map_err(|e| e.to_string())?;
    check(req.send().await).await.map(|_| ())
}

pub async fn set_thread_resolved(key: &PrKey, thread: &str, resolved: bool) -> Result<()> {
    let url = format!("{}/threads/{thread}/resolve", pr_url(key));
    let req = if resolved {
        Request::post(&url)
    } else {
        Request::delete(&url)
    };
    check(req.send().await).await.map(|_| ())
}

/// Adds a thread to the viewer's pending review, starting one if needed.
pub async fn add_review_thread(key: &PrKey, thread: &NewThread) -> Result<()> {
    let req = Request::post(&format!("{}/review/comments", pr_url(key)))
        .json(thread)
        .map_err(|e| e.to_string())?;
    check(req.send().await).await.map(|_| ())
}

/// Edits a pending comment by its GraphQL ID.
pub async fn edit_review_comment(key: &PrKey, comment: &str, body: String) -> Result<()> {
    let req = Request::patch(&format!("{}/review/comments/{comment}", pr_url(key)))
        .json(&NewComment { body, review: true })
        .map_err(|e| e.to_string())?;
    check(req.send().await).await.map(|_| ())
}

pub async fn delete_review_comment(key: &PrKey, comment: &str) -> Result<()> {
    check(
        Request::delete(&format!("{}/review/comments/{comment}", pr_url(key)))
            .send()
            .await,
    )
    .await
    .map(|_| ())
}

pub async fn submit_review(key: &PrKey, review: &SubmitReview) -> Result<()> {
    let req = Request::post(&format!("{}/review", pr_url(key)))
        .json(review)
        .map_err(|e| e.to_string())?;
    check(req.send().await).await.map(|_| ())
}

pub async fn discard_review(key: &PrKey) -> Result<()> {
    check(
        Request::delete(&format!("{}/review", pr_url(key)))
            .send()
            .await,
    )
    .await
    .map(|_| ())
}
