//! The HTTP API used by the web UI, and the UI's static files.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State as AxState};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use futures::Stream;
use grenadine_core::api::{
    BlobsRequest, BlobsResponse, Changes, InboxEdit, InboxWithPrs, PrDetail, PrKey, PrMissing,
};
use serde::Deserialize;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::assets;
use crate::sync::State;

type St = AxState<Arc<State>>;

/// An error that becomes a 500 response.
struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", self.0)).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: Arc<State>) -> Router {
    Router::new()
        .route("/api/inboxes", get(inboxes).post(create_inbox))
        .route("/api/inboxes/{id}", put(update_inbox).delete(delete_inbox))
        .route("/api/pr/{owner}/{name}/{number}", get(pr))
        .route("/api/changes", get(changes))
        .route("/api/blobs", post(blobs))
        .route("/api/sync", get(sync_status))
        .route("/api/events", get(events))
        .fallback(get(assets::serve))
        .with_state(state)
}

async fn inboxes(AxState(state): St) -> ApiResult<Json<Vec<InboxWithPrs>>> {
    let mut out = Vec::new();
    for inbox in state.db.inboxes()? {
        out.push(InboxWithPrs {
            prs: state.db.inbox_prs(inbox.id)?,
            error: state.db.inbox_error(inbox.id)?,
            inbox,
        });
    }
    Ok(Json(out))
}

fn check_edit(edit: &InboxEdit) -> Result<(), (StatusCode, &'static str)> {
    if edit.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "the inbox needs a name"));
    }
    Ok(())
}

/// Tells pages about an inbox change and searches again right away.
fn inboxes_changed(state: &State) {
    state.send(grenadine_core::api::ServerEvent::InboxesChanged);
    state.poke.notify_one();
}

async fn create_inbox(AxState(state): St, Json(edit): Json<InboxEdit>) -> ApiResult<Response> {
    if let Err(r) = check_edit(&edit) {
        return Ok(r.into_response());
    }
    let id = state.db.create_inbox(&edit)?;
    inboxes_changed(&state);
    Ok(Json(id).into_response())
}

async fn update_inbox(
    AxState(state): St,
    Path(id): Path<i64>,
    Json(edit): Json<InboxEdit>,
) -> ApiResult<Response> {
    if let Err(r) = check_edit(&edit) {
        return Ok(r.into_response());
    }
    if !state.db.update_inbox(id, &edit)? {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    inboxes_changed(&state);
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn delete_inbox(AxState(state): St, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    if !state.db.delete_inbox(id)? {
        return Ok(StatusCode::NOT_FOUND);
    }
    inboxes_changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

fn missing(what: PrMissing) -> Response {
    (StatusCode::NOT_FOUND, Json(what)).into_response()
}

/// The PR's detail, or why there is none. A configured repository's PR
/// that no inbox covers is synced on demand: asking for it kicks off a
/// background sync and the page refetches on the PrChanged event.
async fn pr(
    AxState(state): St,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> ApiResult<Response> {
    let key = PrKey {
        repo: format!("{owner}/{name}"),
        number,
    };
    if !state.repos.contains_key(&key.repo) {
        return Ok(missing(PrMissing::NotConfigured));
    }
    // A PR no inbox covers gets a sync on every request, cached or not;
    // `sync_once` dedupes while one is already running.
    if !state.db.in_any_inbox(&key)? {
        state.sync_once(key.clone());
    }
    if let Some(pr) = state.db.pr(&key)? {
        prefetch_stack(&state, &pr)?;
        return Ok(Json::<PrDetail>(pr).into_response());
    }
    let what = match state.db.pr_sync_error(&key)? {
        Some(e) => PrMissing::SyncFailed(e),
        None => PrMissing::NotSynced,
    };
    Ok(missing(what))
}

/// Syncs the PR's stack-mates that changed since their last sync, going by
/// what the stack says about them, so that clicking through the stack is
/// fast. Stack-mates in an inbox are left to the poller.
fn prefetch_stack(state: &Arc<State>, pr: &PrDetail) -> ApiResult<()> {
    let Some(stack) = &pr.stack else {
        return Ok(());
    };
    for mate in stack.prs.iter().filter(|p| p.number != pr.summary.key.number) {
        let key = PrKey {
            repo: pr.summary.key.repo.clone(),
            number: mate.number,
        };
        // A sync stores a newer updatedAt than the stack saw, so only an
        // older one means the mate changed since.
        let stale = state
            .db
            .sync_mark(&key)?
            .is_none_or(|m| m.updated_at < mate.updated_at);
        if stale && !state.db.in_any_inbox(&key)? {
            state.sync_once(key);
        }
    }
    Ok(())
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Commits and blobs never change, so their responses can be cached forever.
fn immutable<T: IntoResponse>(body: T) -> Response {
    (
        [(header::CACHE_CONTROL, "public, max-age=31536000, immutable")],
        body,
    )
        .into_response()
}

#[derive(Deserialize)]
struct ChangesQuery {
    repo: String,
    from: String,
    to: String,
}

async fn changes(AxState(state): St, Query(q): Query<ChangesQuery>) -> ApiResult<Response> {
    if !is_sha(&q.from) || !is_sha(&q.to) {
        return Ok((
            StatusCode::BAD_REQUEST,
            "from and to must be full commit SHAs",
        )
            .into_response());
    }
    let Some(repo) = state.repo(&q.repo) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let files = tokio::task::spawn_blocking(move || repo.changed_files(&q.from, &q.to)).await??;
    Ok(immutable(Json(Changes { files })))
}

async fn blobs(AxState(state): St, Json(req): Json<BlobsRequest>) -> ApiResult<Response> {
    if !req.ids.iter().all(|id| is_sha(id)) {
        return Ok((StatusCode::BAD_REQUEST, "ids must be full blob SHAs").into_response());
    }
    let Some(repo) = state.repo(&req.repo) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let blobs = tokio::task::spawn_blocking(move || repo.blobs(&req.ids)).await??;
    Ok(Json(BlobsResponse { blobs }).into_response())
}

/// The poller's current progress; pages fetch it once and then follow the
/// SyncStatus events.
async fn sync_status(AxState(state): St) -> Json<grenadine_core::api::SyncStatus> {
    Json(state.sync_status.lock().unwrap().clone())
}

/// The SSE stream of server events; it ends on shutdown so that open
/// connections don't block the server's graceful shutdown.
fn event_stream(state: &State) -> impl Stream<Item = Result<Event, Infallible>> + use<> {
    let shutdown = state.shutdown.clone();
    futures::StreamExt::take_until(
        BroadcastStream::new(state.events.subscribe()).filter_map(|e| {
            // A lagging receiver misses events; tell it to refetch everything.
            let e = e.unwrap_or(grenadine_core::api::ServerEvent::InboxesChanged);
            Event::default().json_data(e).ok().map(Ok)
        }),
        shutdown.cancelled_owned(),
    )
}

async fn events(AxState(state): St) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(event_stream(&state)).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn events_stream_ends_on_shutdown() {
        let state = crate::sync::test_state();
        let mut stream = std::pin::pin!(event_stream(&state));
        state.shutdown.cancel();
        let next = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next()).await;
        assert!(matches!(next, Ok(None)));
    }

    #[tokio::test]
    async fn sync_status_returns_the_snapshot() {
        let state = crate::sync::test_state();
        let Json(s) = sync_status(AxState(state)).await;
        assert_eq!(s, grenadine_core::api::SyncStatus::default());
    }

    #[tokio::test]
    async fn diffs_do_not_wait_for_syncs() {
        let fx = crate::git::tests::fixture();
        let to = crate::git::tests::commit(&fx.clone.path, "b", "2\n");
        let from = crate::git::tests::run(&fx.clone.path, &["rev-parse", "HEAD~1"]);

        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let state = crate::sync::test_state_with(
            [(fx.clone.slug.clone(), cloned.clone())].into_iter().collect(),
        );

        let _guard = cloned.git_lock.lock().await;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            changes(
                AxState(state),
                Query(ChangesQuery {
                    repo: "owner/name".to_owned(),
                    from,
                    to,
                }),
            ),
        )
        .await
        .expect("changes blocked behind the sync lock")
        .map_err(|e| e.0)
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn body(response: Response) -> String {
        String::from_utf8(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn unconfigured_repo_is_not_configured() {
        let state = crate::sync::test_state();
        let response = pr(AxState(state), Path(("o".into(), "n".into(), 7)))
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(body(response).await, r#""not_configured""#);
    }

    /// A configured PR that no inbox covers gets an on-demand sync; the
    /// dummy GitHub client makes it fail and the error is then reported.
    #[tokio::test]
    async fn non_inbox_pr_syncs_on_demand() {
        let fx = crate::git::tests::fixture();
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let state = crate::sync::test_state_with(
            [(fx.clone.slug.clone(), cloned)].into_iter().collect(),
        );
        let response = pr(
            AxState(state.clone()),
            Path(("owner".into(), "name".into(), 7)),
        )
        .await
        .map_err(|e| e.0)
        .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(body(response).await, r#""not_synced""#);

        let key = PrKey {
            repo: "owner/name".into(),
            number: 7,
        };
        for _ in 0..50 {
            if state.db.pr_sync_error(&key).unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(state.db.pr_sync_error(&key).unwrap().is_some());
        let response = pr(AxState(state), Path(("owner".into(), "name".into(), 7)))
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert!(body(response).await.contains("sync_failed"));
    }

    /// Even a cached PR gets a fresh on-demand sync when no inbox covers
    /// it: the dead test API makes the background sync fail and the error
    /// lands on the PR.
    #[tokio::test]
    async fn cached_non_inbox_pr_resyncs_on_demand() {
        let fx = crate::git::tests::fixture();
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let state = crate::sync::test_state_with(
            [(fx.clone.slug.clone(), cloned)].into_iter().collect(),
        );
        let key = PrKey {
            repo: "owner/name".into(),
            number: 7,
        };
        state
            .db
            .store_sync(
                &crate::db::PrMeta {
                    key: key.clone(),
                    title: "t".into(),
                    body: "b".into(),
                    author: "a".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                    url: "u".into(),
                    created_at: "c".into(),
                    updated_at: "u".into(),
                    base_ref: "main".into(),
                    head_ref: "pr".into(),
                    head_oid: "h".into(),
                },
                &[],
                &[],
                None,
                false,
                None,
            )
            .unwrap();
        let response = pr(
            AxState(state.clone()),
            Path(("owner".into(), "name".into(), 7)),
        )
        .await
        .map_err(|e| e.0)
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        for _ in 0..50 {
            if state.db.pr_sync_error(&key).unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(state.db.pr_sync_error(&key).unwrap().is_some());
    }

    /// Opening a PR syncs its stack-mates that no inbox covers; the dead
    /// test API makes those syncs fail, which leaves an error behind.
    #[tokio::test]
    async fn stack_mates_are_prefetched() {
        let fx = crate::git::tests::fixture();
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let state = crate::sync::test_state_with(
            [(fx.clone.slug.clone(), cloned)].into_iter().collect(),
        );
        let key = |number| PrKey {
            repo: "owner/name".into(),
            number,
        };
        let mate = |number, parent| grenadine_core::api::StackPr {
            number,
            title: "t".into(),
            state: "OPEN".into(),
            is_draft: false,
            url: "u".into(),
            updated_at: "u".into(),
            head_oid: "h".into(),
            parent,
        };
        let stack = grenadine_core::api::Stack {
            base_ref: "main".into(),
            prs: vec![mate(7, Some(6)), mate(6, None), mate(8, Some(7))],
            more_ancestors: false,
            more_descendants: false,
        };
        state
            .db
            .set_inbox_results(
                1,
                Ok(&[
                    crate::github::Hit {
                        key: key(7),
                        title: "t".into(),
                        author: "a".into(),
                        state: "OPEN".into(),
                        is_draft: false,
                        url: "u".into(),
                        updated_at: "u".into(),
                        head_oid: "h".into(),
                    },
                    crate::github::Hit {
                        key: key(8),
                        title: "t".into(),
                        author: "a".into(),
                        state: "OPEN".into(),
                        is_draft: false,
                        url: "u".into(),
                        updated_at: "u".into(),
                        head_oid: "h".into(),
                    },
                ]),
            )
            .unwrap();
        state
            .db
            .store_sync(
                &crate::db::PrMeta {
                    key: key(7),
                    title: "t".into(),
                    body: "b".into(),
                    author: "a".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                    url: "u".into(),
                    created_at: "c".into(),
                    updated_at: "u".into(),
                    base_ref: "main".into(),
                    head_ref: "pr".into(),
                    head_oid: "h".into(),
                },
                &[],
                &[],
                Some(&stack),
                false,
                None,
            )
            .unwrap();
        let response = pr(AxState(state.clone()), Path(("owner".into(), "name".into(), 7)))
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        for _ in 0..50 {
            if state.db.pr_sync_error(&key(6)).unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(state.db.pr_sync_error(&key(6)).unwrap().is_some());
        // 8 is in an inbox, so it's the poller's.
        assert_eq!(state.db.pr_sync_error(&key(8)).unwrap(), None);
    }

    /// An inbox PR is the poller's: no on-demand sync starts for it.
    #[tokio::test]
    async fn inbox_pr_is_not_synced_on_demand() {
        let fx = crate::git::tests::fixture();
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let state = crate::sync::test_state_with(
            [(fx.clone.slug.clone(), cloned)].into_iter().collect(),
        );
        let key = PrKey {
            repo: "owner/name".into(),
            number: 7,
        };
        state
            .db
            .set_inbox_results(1, Ok(&[crate::github::Hit {
                key: key.clone(),
                title: "t".into(),
                author: "a".into(),
                state: "OPEN".into(),
                is_draft: false,
                url: "u".into(),
                updated_at: "u".into(),
                head_oid: "h".into(),
            }]))
            .unwrap();
        let response = pr(
            AxState(state.clone()),
            Path(("owner".into(), "name".into(), 7)),
        )
        .await
        .map_err(|e| e.0)
        .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(body(response).await, r#""not_synced""#);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(state.on_demand.lock().unwrap().is_empty());
    }
}
