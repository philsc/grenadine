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
    let Some(repo) = state.repo(&q.repo).await else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let files = tokio::task::spawn_blocking(move || repo.changed_files(&q.from, &q.to)).await??;
    Ok(immutable(Json(Changes { files })))
}

async fn blobs(AxState(state): St, Json(req): Json<BlobsRequest>) -> ApiResult<Response> {
    if !req.ids.iter().all(|id| is_sha(id)) {
        return Ok((StatusCode::BAD_REQUEST, "ids must be full blob SHAs").into_response());
    }
    let Some(repo) = state.repo(&req.repo).await else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let blobs = tokio::task::spawn_blocking(move || repo.blobs(&req.ids)).await??;
    Ok(Json(BlobsResponse { blobs }).into_response())
}

async fn events(AxState(state): St) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|e| {
        // A lagging receiver misses events; tell it to refetch everything.
        let e = e.unwrap_or(grenadine_core::api::ServerEvent::InboxesChanged);
        Event::default().json_data(e).ok().map(Ok)
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
