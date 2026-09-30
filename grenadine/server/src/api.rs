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
    BlobsRequest, BlobsResponse, Changes, InboxEdit, InboxWithPrs, PrDetail, PrKey,
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

fn check_edit(edit: &InboxEdit) -> Result<(), Response> {
    if edit.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "the inbox needs a name").into_response());
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
        return Ok(r);
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
        return Ok(r);
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

async fn pr(
    AxState(state): St,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> ApiResult<Response> {
    let key = PrKey {
        repo: format!("{owner}/{name}"),
        number,
    };
    Ok(match state.db.pr(&key)? {
        Some(pr) => Json::<PrDetail>(pr).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    })
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
}
