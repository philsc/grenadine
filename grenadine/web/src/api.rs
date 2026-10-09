//! Talks to the grenadine server. Tree comparisons and blobs never change,
//! so they are cached for the life of the page.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use gloo_net::http::{Request, Response};
use grenadine_core::api::{
    AgentMessage, AgentSession, Approval, Blob, BlobsRequest, BlobsResponse, Changes, InboxEdit,
    InboxWithPrs, NewAgent, PrDetail, PrKey, PrMissing, SyncStatus,
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

pub async fn send_agent_message(id: &str, prompt: String) -> Result<()> {
    let req = Request::post(&format!("/api/agents/{id}/messages"))
        .json(&AgentMessage { prompt })
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
