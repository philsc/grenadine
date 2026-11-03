//! The HTTP API used by the web UI, and the UI's static files.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, Path, Query, State as AxState};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use futures::Stream;
use grenadine_core::api::{
    AgentBackendInfo, AgentEvent, AgentMessage, AgentSession, Approval, AutoReviewEntry,
    BlobsRequest, BlobsResponse, Changes, CleanupState, GENERAL, InboxEdit, InboxWithPrs,
    MergeRequest, Merged, NewAgent, NewCleanupSession, NewComment, NewReply, NewThread,
    PermissionMode, PrDetail, PrKey, PrMissing, RepoSettings, ServerEvent, SubmitReview,
    WorktreeChanges, WrapUp, submit_blocker,
};
use serde::Deserialize;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::agent::{self, Decision};
use crate::github::ReviewItem;
use crate::sync::State;
use crate::{assets, auto_review, cleanup, mcp, terminal};

type St = AxState<Arc<State>>;

/// How big a prompt's request may be: room for its images, which base64
/// makes a third bigger, and its text.
const PROMPT_BODY_LIMIT: usize = agent::MAX_IMAGES * agent::MAX_IMAGE_BYTES / 3 * 4 + (1 << 20);

/// An error that becomes a response with its status, 500 unless the
/// request was at fault.
struct ApiError(StatusCode, anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, format!("{:#}", self.1)).into_response()
    }
}

/// An error the request is at fault for.
fn refuse(status: StatusCode, msg: String) -> ApiError {
    ApiError(status, anyhow::anyhow!(msg))
}

type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: Arc<State>) -> Router {
    api_routes()
        .layer(axum::middleware::from_fn(same_origin_writes))
        .route("/mcp/{id}/{secret}", post(mcp::handle))
        .fallback(get(assets::serve))
        .with_state(state)
}

/// The routes of the UI's API. Their writes are only taken from the
/// server's own pages, see `same_origin_writes`.
fn api_routes() -> Router<Arc<State>> {
    Router::new()
        .route("/api/inboxes", get(inboxes).post(create_inbox))
        .route("/api/inboxes/{id}", put(update_inbox).delete(delete_inbox))
        .route("/api/pr/{owner}/{name}/{number}", get(pr))
        .route(
            "/api/pr/{owner}/{name}/{number}/comments",
            post(add_comment),
        )
        .route(
            "/api/pr/{owner}/{name}/{number}/threads/{thread}/replies",
            post(reply_to_thread),
        )
        .route(
            "/api/pr/{owner}/{name}/{number}/threads/{thread}/resolve",
            post(resolve_thread).delete(unresolve_thread),
        )
        .route(
            "/api/pr/{owner}/{name}/{number}/review",
            post(submit_review).delete(discard_review),
        )
        .route("/api/pr/{owner}/{name}/{number}/merge", post(merge))
        .route(
            "/api/pr/{owner}/{name}/{number}/review/comments",
            post(add_review_thread),
        )
        .route(
            "/api/pr/{owner}/{name}/{number}/review/comments/{comment}",
            axum::routing::patch(edit_review_comment).delete(delete_review_comment),
        )
        .route("/api/changes", get(changes))
        .route("/api/blobs", post(blobs))
        .route("/api/sync", get(sync_status))
        .route("/api/events", get(events))
        .route("/api/repos", get(repos))
        .route(
            "/api/repos/{owner}/{name}/settings",
            get(repo_settings).put(set_repo_settings),
        )
        .route("/api/auto-reviews", get(auto_reviews))
        .route(
            "/api/agents",
            get(agents)
                .post(create_agent)
                .layer(DefaultBodyLimit::max(PROMPT_BODY_LIMIT)),
        )
        .route("/api/agents/backends", get(agent_backends))
        .route("/api/agents/{id}", get(agent).delete(delete_agent))
        .route(
            "/api/agents/{id}/messages",
            post(agent_message).layer(DefaultBodyLimit::max(PROMPT_BODY_LIMIT)),
        )
        .route("/api/agents/{id}/images/{name}", get(agent_image))
        .route("/api/agents/{id}/interrupt", post(interrupt_agent))
        .route("/api/agents/{id}/approvals", post(approve))
        .route("/api/agents/{id}/wrap-up", post(wrap_up_agent))
        .route("/api/agents/{id}/events", get(agent_events))
        .route("/api/agents/{id}/changes", get(agent_changes))
        .route("/api/agents/{id}/prs", get(agent_prs))
        .route("/api/images/{id}", get(image))
        .route(
            "/api/agents/{id}/terminals",
            get(terminals).post(open_terminal),
        )
        .route(
            "/api/agents/{id}/terminals/{terminal}",
            axum::routing::delete(close_terminal),
        )
        .route(
            "/api/agents/{id}/terminals/{terminal}/ws",
            get(terminal_socket),
        )
        .route("/api/cleanup", get(cleanup))
        .route("/api/cleanup/sessions", post(create_cleanup_session))
        .route(
            "/api/cleanup/{id}",
            axum::routing::delete(delete_cleanup_item),
        )
}

async fn inboxes(AxState(state): St) -> ApiResult<Json<Vec<InboxWithPrs>>> {
    let mut out = Vec::new();
    for inbox in state.db.inboxes()? {
        out.push(InboxWithPrs {
            prs: state.db.inbox_prs(inbox.id)?,
            error: state.db.inbox_error(inbox.id)?,
            truncated: state.db.inbox_truncation(inbox.id)?,
            inbox,
        });
    }
    Ok(Json(out))
}

fn check_edit(edit: &InboxEdit) -> Result<(), (StatusCode, String)> {
    if edit.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "the inbox needs a name".into()));
    }
    // A filter without gr: terms is GitHub's problem; one with them is
    // parsed now so that a bad one fails the save.
    if grenadine_core::filter::has_local_terms(&edit.filter)
        && let Err(e) = grenadine_core::filter::parse(&edit.filter)
    {
        return Err((StatusCode::BAD_REQUEST, e));
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

/// The PR a write goes to, if its repository is configured.
fn writable(owner: String, name: String, number: u64, state: &State) -> ApiResult<PrKey> {
    let key = PrKey {
        repo: format!("{owner}/{name}"),
        number,
    };
    if !state.repos.contains_key(&key.repo) {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            format!("{} is not a configured repository", key.repo),
        ));
    }
    Ok(key)
}

fn require_body(body: &str) -> ApiResult<()> {
    if body.trim().is_empty() {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "the comment is empty".into(),
        ));
    }
    Ok(())
}

/// Refuses `thread` unless it's one of the PR's review threads, so that a
/// request can only change the PR it names.
fn require_thread(state: &State, key: &PrKey, thread: &str) -> ApiResult<()> {
    if !state.db.has_thread(key, thread)? {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            format!(
                "{thread} is not a review thread of {}#{}",
                key.repo, key.number
            ),
        ));
    }
    Ok(())
}

/// Comments on the PR itself.
async fn add_comment(
    AxState(state): St,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Json(c): Json<NewComment>,
) -> ApiResult<Response> {
    let key = writable(owner, name, number, &state)?;
    require_body(&c.body)?;
    let id = state.github.pr_id(&key).await?;
    state.github.add_comment(&id, &c.body).await?;
    state.resync(key);
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn reply_to_thread(
    AxState(state): St,
    Path((owner, name, number, thread)): Path<(String, String, u64, String)>,
    Json(r): Json<NewReply>,
) -> ApiResult<Response> {
    let key = writable(owner, name, number, &state)?;
    require_thread(&state, &key, &thread)?;
    require_body(&r.body)?;
    if r.review {
        let _lock = state.lock_review(&key).await;
        let item = ReviewItem::Reply {
            thread: &thread,
            body: &r.body,
        };
        state.github.add_to_review(&key, item).await?;
    } else {
        state.github.reply_to_thread(&thread, &r.body, None).await?;
    }
    state.resync(key);
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn set_resolved(
    state: Arc<State>,
    (owner, name, number, thread): (String, String, u64, String),
    resolved: bool,
) -> ApiResult<Response> {
    let key = writable(owner, name, number, &state)?;
    require_thread(&state, &key, &thread)?;
    state.github.set_thread_resolved(&thread, resolved).await?;
    state.resync(key);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Refuses a thread that GitHub would refuse with an opaque error.
fn require_thread_place(t: &NewThread) -> ApiResult<()> {
    require_body(&t.body)?;
    if t.path.is_empty() {
        return Err(refuse(StatusCode::BAD_REQUEST, "the path is empty".into()));
    }
    if t.line == Some(0) {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "lines are numbered from 1".into(),
        ));
    }
    Ok(())
}

/// Refuses `comment` unless it's the GraphQL ID of one of the viewer's
/// pending comments on the PR, so that a request can only change the PR
/// it names.
fn require_pending_comment(state: &State, key: &PrKey, comment: &str) -> ApiResult<()> {
    if !state.db.has_pending_comment(key, comment)? {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            format!(
                "{comment} is not a pending comment on {}#{}",
                key.repo, key.number
            ),
        ));
    }
    Ok(())
}

/// Adds a thread to the viewer's pending review, starting one if needed.
async fn add_review_thread(
    AxState(state): St,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Json(t): Json<NewThread>,
) -> ApiResult<Response> {
    let key = writable(owner, name, number, &state)?;
    require_thread_place(&t)?;
    {
        let _lock = state.lock_review(&key).await;
        state
            .github
            .add_to_review(&key, ReviewItem::Thread(&t))
            .await?;
    }
    state.resync(key);
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn edit_review_comment(
    AxState(state): St,
    Path((owner, name, number, comment)): Path<(String, String, u64, String)>,
    Json(c): Json<NewComment>,
) -> ApiResult<Response> {
    let key = writable(owner, name, number, &state)?;
    require_pending_comment(&state, &key, &comment)?;
    require_body(&c.body)?;
    {
        let _lock = state.lock_review(&key).await;
        state.github.edit_review_comment(&comment, &c.body).await?;
    }
    state.resync(key);
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn delete_review_comment(
    AxState(state): St,
    Path((owner, name, number, comment)): Path<(String, String, u64, String)>,
) -> ApiResult<Response> {
    let key = writable(owner, name, number, &state)?;
    require_pending_comment(&state, &key, &comment)?;
    {
        let _lock = state.lock_review(&key).await;
        state.github.delete_review_comment(&comment).await?;
    }
    state.resync(key);
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn submit_review(
    AxState(state): St,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Json(s): Json<SubmitReview>,
) -> ApiResult<Response> {
    let key = writable(owner, name, number, &state)?;
    {
        let _lock = state.lock_review(&key).await;
        let pending = state.github.pending_review(&key).await?;
        if let Some(why) = submit_blocker(s.event, &s.body, pending.comments) {
            return Err(refuse(StatusCode::BAD_REQUEST, why.into()));
        }
        state.github.submit_review(&pending, &s).await?;
    }
    state.resync(key);
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn merge(
    AxState(state): St,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Json(m): Json<MergeRequest>,
) -> ApiResult<Json<Merged>> {
    let key = writable(owner, name, number, &state)?;
    if !is_sha(&m.head_oid) {
        return Err(refuse(StatusCode::BAD_REQUEST, "bad head_oid".into()));
    }
    let merged = state.github.merge(&key, &m).await;
    // A refused merge may be about a change the page doesn't show yet,
    // such as a push to the head.
    state.resync(key);
    Ok(Json(Merged { warning: merged? }))
}

async fn discard_review(
    AxState(state): St,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> ApiResult<Response> {
    let key = writable(owner, name, number, &state)?;
    {
        let _lock = state.lock_review(&key).await;
        state.github.discard_review(&key).await?;
    }
    state.resync(key);
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn resolve_thread(
    AxState(state): St,
    Path(path): Path<(String, String, u64, String)>,
) -> ApiResult<Response> {
    set_resolved(state, path, true).await
}

async fn unresolve_thread(
    AxState(state): St,
    Path(path): Path<(String, String, u64, String)>,
) -> ApiResult<Response> {
    set_resolved(state, path, false).await
}

/// Syncs the PR's stack-mates that changed since their last sync, going by
/// what the stack says about them, so that clicking through the stack is
/// fast. Stack-mates in an inbox are left to the poller.
fn prefetch_stack(state: &Arc<State>, pr: &PrDetail) -> ApiResult<()> {
    let Some(stack) = &pr.stack else {
        return Ok(());
    };
    for mate in stack
        .prs
        .iter()
        .filter(|p| p.number != pr.summary.key.number)
    {
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

/// Commits, blobs and images never change, so their responses can be cached
/// forever.
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

/// Keep-alives as a named "ping" event rather than a comment, so that the
/// page sees them and can tell a quiet stream from a dead connection.
fn keep_alive() -> KeepAlive {
    KeepAlive::new().event(Event::default().event("ping"))
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
    Sse::new(event_stream(&state)).keep_alive(keep_alive())
}

/// The configured repositories, for the page's repository pickers.
async fn repos(AxState(state): St) -> Json<Vec<String>> {
    Json(state.repos.keys().cloned().collect())
}

async fn repo_settings(
    AxState(state): St,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    let repo = format!("{owner}/{name}");
    if !state.repos.contains_key(&repo) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    Ok(Json(state.db.repo_settings(&repo)?).into_response())
}

async fn set_repo_settings(
    AxState(state): St,
    Path((owner, name)): Path<(String, String)>,
    Json(settings): Json<RepoSettings>,
) -> ApiResult<Response> {
    let repo = format!("{owner}/{name}");
    if !state.repos.contains_key(&repo) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let old = state.db.repo_settings(&repo)?;
    let settings = match auto_review::to_save(&old, settings, agent::now()) {
        Ok(s) => s,
        Err(e) => return Ok((StatusCode::BAD_REQUEST, e).into_response()),
    };
    for (b, m) in &settings.review_models {
        if !state.agents.offers(*b, Some(m)).await {
            let message = format!("{} doesn't offer that model", b.label());
            return Ok((StatusCode::BAD_REQUEST, message).into_response());
        }
    }
    state.db.set_repo_settings(&repo, &settings)?;
    state.send(ServerEvent::RepoSettingsChanged(repo));
    // Reviews that were just turned on look for PRs right away.
    state.poke.notify_one();
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn auto_reviews(AxState(state): St) -> ApiResult<Json<Vec<AutoReviewEntry>>> {
    Ok(Json(state.db.auto_reviews()?))
}

async fn agents(AxState(state): St) -> ApiResult<Json<Vec<AgentSession>>> {
    Ok(Json(with_unstaged(&state, state.db.agents()?).await?))
}

/// Counts the unstaged changes in the sessions' worktrees. A worktree git
/// can't look at counts none, since the count isn't worth failing over.
async fn with_unstaged(state: &State, sessions: Vec<AgentSession>) -> ApiResult<Vec<AgentSession>> {
    let repos: Vec<_> = sessions.iter().map(|s| state.repo(&s.repo)).collect();
    Ok(tokio::task::spawn_blocking(move || {
        sessions
            .into_iter()
            .zip(repos)
            .map(|(mut session, repo)| {
                let worktree = std::path::Path::new(&session.worktree);
                if let Some(repo) = repo.filter(|_| worktree.is_dir()) {
                    match repo.worktree_unstaged_count(worktree) {
                        Ok(n) => session.unstaged = n.try_into().unwrap_or(u32::MAX),
                        Err(e) => tracing::warn!("agent {}: {e:#}", session.id),
                    }
                }
                session
            })
            .collect()
    })
    .await?)
}

/// The backends, with the models each offers.
async fn agent_backends(AxState(state): St) -> Json<Vec<AgentBackendInfo>> {
    Json(state.agents.backends().await)
}

const UNKNOWN_MODE: &str = "the backend has no such permission mode";
const UNKNOWN_OPTION: &str = "unknown approval option";

/// Whether `backend` offers `model` and `mode`; the response when not.
async fn offers(
    state: &State,
    backend: grenadine_core::api::AgentBackend,
    model: Option<&str>,
    mode: &PermissionMode,
) -> Option<Response> {
    if !backend.has_mode(mode.as_str()) {
        return Some((StatusCode::BAD_REQUEST, UNKNOWN_MODE).into_response());
    }
    if !state.agents.offers(backend, model).await {
        let message = format!("{} doesn't offer that model", backend.label());
        return Some((StatusCode::BAD_REQUEST, message).into_response());
    }
    None
}

/// Whether sessions can start in `repo`: a configured repository, or
/// none for a general session.
fn can_start_in(state: &State, repo: &str) -> bool {
    repo == GENERAL || state.repos.contains_key(repo)
}

async fn create_agent(AxState(state): St, Json(req): Json<NewAgent>) -> ApiResult<Response> {
    if req.prompt.trim().is_empty() {
        return Ok((StatusCode::BAD_REQUEST, "the prompt is empty").into_response());
    }
    if let Some(e) = agent::check_images(&req.images) {
        return Ok((StatusCode::BAD_REQUEST, e).into_response());
    }
    if let Some(r) = offers(
        &state,
        req.backend,
        req.model.as_deref(),
        &req.permission_mode,
    )
    .await
    {
        return Ok(r);
    }
    if !can_start_in(&state, &req.repo) {
        return Ok((StatusCode::BAD_REQUEST, "not a configured repository").into_response());
    }
    Ok(Json(agent::create(&state, req).await?).into_response())
}

async fn cleanup(AxState(state): St) -> ApiResult<Json<CleanupState>> {
    Ok(Json(cleanup::cleanup_state(&state)?))
}

async fn delete_cleanup_item(AxState(state): St, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    if !state.db.delete_cleanup_item(id)? {
        return Ok(StatusCode::NOT_FOUND);
    }
    state.send(ServerEvent::CleanupChanged);
    Ok(StatusCode::NO_CONTENT)
}

/// Hands cleanup items off to a new agent session, which starts in plan
/// mode.
async fn create_cleanup_session(
    AxState(state): St,
    Json(req): Json<NewCleanupSession>,
) -> ApiResult<Response> {
    if let Some(r) = offers(
        &state,
        req.backend,
        req.model.as_deref(),
        &PermissionMode::plan(),
    )
    .await
    {
        return Ok(r);
    }
    if !can_start_in(&state, &req.repo) {
        return Ok((StatusCode::BAD_REQUEST, "not a configured repository").into_response());
    }
    Ok(match cleanup::hand_off(&state, req).await? {
        Some(session) => Json(session).into_response(),
        None => (StatusCode::BAD_REQUEST, "no such cleanup items").into_response(),
    })
}

/// The session, if `id` names one.
fn find_agent(state: &State, id: &str) -> ApiResult<Option<AgentSession>> {
    if !agent::valid_id(id) {
        return Ok(None);
    }
    Ok(state.db.agent(id)?)
}

async fn agent(AxState(state): St, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(match find_agent(&state, &id)? {
        Some(a) => Json(with_unstaged(&state, vec![a]).await?.remove(0)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    })
}

/// The unstaged changes in the session's worktree; none once the worktree
/// is gone.
async fn agent_changes(AxState(state): St, Path(id): Path<String>) -> ApiResult<Response> {
    let Some(session) = find_agent(&state, &id)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let worktree = std::path::PathBuf::from(&session.worktree);
    let Some(repo) = state.repo(&session.repo).filter(|_| worktree.is_dir()) else {
        return Ok(Json(WorktreeChanges::default()).into_response());
    };
    let files = tokio::task::spawn_blocking(move || repo.worktree_changes(&worktree)).await??;
    Ok(Json(WorktreeChanges { files }).into_response())
}

/// The PRs the session opened, as of the last poll.
async fn agent_prs(AxState(state): St, Path(id): Path<String>) -> ApiResult<Response> {
    if find_agent(&state, &id)?.is_none() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    Ok(Json(state.db.agent_pr_list(&id)?).into_response())
}

async fn terminals(AxState(state): St, Path(id): Path<String>) -> ApiResult<Response> {
    if find_agent(&state, &id)?.is_none() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    Ok(Json(state.agents.terminals.list(&id)).into_response())
}

async fn open_terminal(AxState(state): St, Path(id): Path<String>) -> ApiResult<Response> {
    let Some(session) = find_agent(&state, &id)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    if !std::path::Path::new(&session.worktree).is_dir() {
        return Ok((StatusCode::CONFLICT, "the session's worktree is gone").into_response());
    }
    Ok(Json(terminal::open(&state, &session)?).into_response())
}

async fn close_terminal(
    AxState(state): St,
    Path((id, terminal)): Path<(String, u64)>,
) -> StatusCode {
    if state.agents.terminals.close(&id, terminal) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// Whether the request comes from one of the server's own pages. Browsers
/// let any page open a WebSocket to any server and only say where the
/// page is from, so without this any website could open a shell here.
fn same_origin(headers: &HeaderMap) -> bool {
    let value = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let (Some(origin), Some(host)) = (value(header::ORIGIN), value(header::HOST)) else {
        return false;
    };
    origin
        .split_once("://")
        .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
}

/// Refuses requests other than reads unless they come from the server's
/// own pages. Without this, any website open in the browser could post a
/// form here, e.g. to resolve a thread (cross-site request forgery).
/// Browsers send `Origin` with every such request, also with their own.
async fn same_origin_writes(req: axum::extract::Request, next: Next) -> Response {
    let read = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !read && !same_origin(req.headers()) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    next.run(req).await
}

async fn terminal_socket(
    AxState(state): St,
    Path((id, terminal)): Path<(String, u64)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin terminal connection").into_response();
    }
    let Some(t) = state.agents.terminals.get(&id, terminal) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ws.on_upgrade(move |socket| terminal::attach(state, t, socket))
}

async fn delete_agent(AxState(state): St, Path(id): Path<String>) -> ApiResult<StatusCode> {
    // Not `find_agent`: a session whose backend this build doesn't know
    // can still be deleted.
    if !agent::valid_id(&id) || !agent::delete(&state, &id).await? {
        return Ok(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn agent_message(
    AxState(state): St,
    Path(id): Path<String>,
    Json(msg): Json<AgentMessage>,
) -> ApiResult<Response> {
    let Some(session) = find_agent(&state, &id)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    // An image can say it all.
    if msg.prompt.trim().is_empty() && msg.images.is_empty() {
        return Ok((StatusCode::BAD_REQUEST, "the prompt is empty").into_response());
    }
    if let Some(e) = agent::check_images(&msg.images) {
        return Ok((StatusCode::BAD_REQUEST, e).into_response());
    }
    if let Some(r) = offers(
        &state,
        session.backend,
        msg.model.as_deref(),
        &msg.permission_mode,
    )
    .await
    {
        return Ok(r);
    }
    let images = agent::save_images(&state, &id, &msg.images).await?;
    agent::send_with_images(
        &state,
        &id,
        msg.prompt,
        images,
        msg.permission_mode,
        msg.model,
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// An image sent with one of the session's prompts. Images never change.
async fn agent_image(
    AxState(state): St,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    let Some((path, media_type)) = agent::image_file(&state, &id, &name) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => Ok(immutable(([(header::CONTENT_TYPE, media_type)], bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(StatusCode::NOT_FOUND.into_response())
        }
        Err(e) => Err(e.into()),
    }
}

async fn interrupt_agent(AxState(state): St, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if find_agent(&state, &id)?.is_none() {
        return Ok(StatusCode::NOT_FOUND);
    }
    agent::interrupt(&state, &id);
    Ok(StatusCode::NO_CONTENT)
}

/// Lands the session's work once its turns are done; the outcome shows up
/// in its transcript. 409 while it is already wrapping up.
async fn wrap_up_agent(
    AxState(state): St,
    Path(id): Path<String>,
    Json(req): Json<WrapUp>,
) -> ApiResult<Response> {
    let Some(session) = find_agent(&state, &id)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    if let Some(r) = offers(
        &state,
        session.backend,
        req.model.as_deref(),
        &req.permission_mode,
    )
    .await
    {
        return Ok(r);
    }
    if !agent::wrap_up(&state, &id, req).await {
        return Ok((StatusCode::CONFLICT, "the session is already wrapping up").into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Answers a permission request; 404 when it isn't waiting anymore.
async fn approve(
    AxState(state): St,
    Path(id): Path<String>,
    Json(approval): Json<Approval>,
) -> ApiResult<Response> {
    let Some(session) = find_agent(&state, &id)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    // The mode a plan approval carries goes on the turn, so it has to be
    // one the backend offers there.
    if let Some(mode) = &approval.mode
        && !session.backend.allows_after_plan(mode.as_str())
    {
        return Ok((StatusCode::BAD_REQUEST, UNKNOWN_MODE).into_response());
    }
    // A picked option says by its kind whether the tool use goes ahead.
    let allow = match &approval.option {
        None => approval.allow,
        Some(option) => match agent::offered_option(&state, &id, &approval.id, option) {
            Some(o) => o.kind.is_allow(),
            None => return Ok((StatusCode::BAD_REQUEST, UNKNOWN_OPTION).into_response()),
        },
    };
    let decision = Decision {
        allow,
        message: approval.message,
        mode: approval.mode,
        option: approval.option,
        answers: approval.answers,
    };
    Ok(if agent::approve(&state, &id, &approval.id, decision)? {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    })
}

/// The session's transcript so far, then its new events as they happen.
/// The stream ends when the page falls behind; the browser reconnects and
/// gets the whole transcript again.
fn agent_event_stream(
    state: &State,
    id: &str,
) -> ApiResult<impl Stream<Item = Result<Event, Infallible>> + use<>> {
    // Subscribe first so that nothing falls between the stored events and
    // the live ones; live events that were stored already are skipped.
    let live = state.agents.subscribe(id);
    let past = state.db.agent_events(id)?;
    let last = past.last().and_then(|e| e.seq).unwrap_or(0);
    let live = BroadcastStream::new(live)
        .map_while(Result::ok)
        .filter(move |e: &AgentEvent| e.seq.is_none_or(|s| s > last));
    let events = tokio_stream::iter(past)
        .chain(live)
        .filter_map(|e| Event::default().json_data(e).ok().map(Ok));
    Ok(futures::StreamExt::take_until(
        events,
        state.shutdown.clone().cancelled_owned(),
    ))
}

async fn agent_events(AxState(state): St, Path(id): Path<String>) -> ApiResult<Response> {
    if find_agent(&state, &id)?.is_none() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    Ok(Sse::new(agent_event_stream(&state, &id)?)
        .keep_alive(keep_alive())
        .into_response())
}

/// An image of a session's transcript. Images never change once stored.
async fn image(AxState(state): St, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(match state.db.agent_image(&id)? {
        Some((mime, data)) => immutable(([(header::CONTENT_TYPE, mime)], data)),
        None => StatusCode::NOT_FOUND.into_response(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_only_open_from_the_servers_pages() {
        let headers = |origin: Option<&str>, host: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::HOST, host.parse().unwrap());
            if let Some(o) = origin {
                h.insert(header::ORIGIN, o.parse().unwrap());
            }
            h
        };
        assert!(same_origin(&headers(
            Some("http://127.0.0.1:8765"),
            "127.0.0.1:8765"
        )));
        assert!(same_origin(&headers(
            Some("https://Box.example"),
            "box.example"
        )));
        assert!(!same_origin(&headers(
            Some("http://evil.example"),
            "127.0.0.1:8765"
        )));
        assert!(!same_origin(&headers(
            Some("http://127.0.0.1:9999"),
            "127.0.0.1:8765"
        )));
        assert!(!same_origin(&headers(Some("null"), "127.0.0.1:8765")));
        assert!(!same_origin(&headers(None, "127.0.0.1:8765")));
    }

    #[tokio::test]
    async fn writes_only_come_from_the_servers_pages() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router(crate::sync::test_state())).into_future());
        let http = reqwest::Client::new();
        let resolve = format!("http://{addr}/api/pr/o/n/1/threads/T/resolve");
        let status =
            |req: reqwest::RequestBuilder| async move { req.send().await.unwrap().status() };

        // A form on another site, or a request that doesn't say where it's
        // from.
        let evil = http.post(&resolve).header("Origin", "http://evil.example");
        assert_eq!(status(evil).await, StatusCode::FORBIDDEN);
        assert_eq!(status(http.delete(&resolve)).await, StatusCode::FORBIDDEN);
        let inbox = http
            .post(format!("http://{addr}/api/inboxes"))
            .header("Origin", "http://evil.example")
            .json(&serde_json::json!({ "name": "x", "query": "y" }));
        assert_eq!(status(inbox).await, StatusCode::FORBIDDEN);

        // The server's own pages get through, to find that o/n isn't
        // configured.
        let own = http
            .post(&resolve)
            .header("Origin", format!("http://{addr}"));
        assert_eq!(status(own).await, StatusCode::NOT_FOUND);
        // Reads need no origin.
        let inboxes = http.get(format!("http://{addr}/api/inboxes"));
        assert_eq!(status(inboxes).await, StatusCode::OK);
    }

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
            [(fx.clone.slug.clone(), cloned.clone())]
                .into_iter()
                .collect(),
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

    #[tokio::test]
    async fn agent_changes_come_from_the_worktree() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join("wt");
        fx.clone.worktree_add(&worktree, "x", "HEAD").unwrap();
        std::fs::write(worktree.join("a"), "2\n").unwrap();
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let state =
            crate::sync::test_state_with([(fx.clone.slug.clone(), cloned)].into_iter().collect());
        let id = "00000000-0000-0000-0000-000000000000";
        state
            .db
            .create_agent(&AgentSession {
                title: "t".into(),
                branch: "x".into(),
                worktree: worktree.to_string_lossy().into_owned(),
                base_sha: "s".into(),
                ..AgentSession::fixture(id, &fx.clone.slug)
            })
            .unwrap();
        let get = |id: &str| agent_changes(AxState(state.clone()), Path(id.to_owned()));

        let response = get(id).await.map_err(|e| e.0).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let changes: WorktreeChanges = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.files[0].change.path(), "a");
        // The listing counts them.
        let unstaged = || async {
            let Json(list) = agents(AxState(state.clone()))
                .await
                .map_err(|e| e.0)
                .unwrap();
            list[0].unstaged
        };
        assert_eq!(unstaged().await, 1);

        std::fs::remove_dir_all(&worktree).unwrap();
        let response = get(id).await.map_err(|e| e.0).unwrap();
        assert_eq!(body(response).await, r#"{"files":[]}"#);
        assert_eq!(unstaged().await, 0);

        let response = get("00000000-0000-0000-0000-000000000001")
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn agent_prs_come_from_the_polls() {
        let state = crate::sync::test_state();
        let id = "00000000-0000-0000-0000-000000000000";
        state
            .db
            .create_agent(&AgentSession::fixture(id, "o/n"))
            .unwrap();
        let get = |id: &str| agent_prs(AxState(state.clone()), Path(id.to_owned()));

        let response = get(id).await.map_err(|e| e.0).unwrap();
        assert_eq!(body(response).await, "[]");

        let key = PrKey {
            repo: "o/n".into(),
            number: 7,
        };
        state.db.add_agent_pr(id, &key, "OPEN").unwrap();
        state
            .db
            .set_agent_pr_state(
                &key,
                &crate::github::PrStatus {
                    state: "OPEN".into(),
                    conflicting: Some(false),
                    title: "Fix it".into(),
                    ci: Some(grenadine_core::api::CiStatus::Passing),
                },
            )
            .unwrap();

        let response = get(id).await.map_err(|e| e.0).unwrap();
        assert_eq!(
            body(response).await,
            r#"[{"key":{"repo":"o/n","number":7},"title":"Fix it","state":"OPEN","ci":"Passing","conflicting":false}]"#
        );

        let response = get("00000000-0000-0000-0000-000000000001")
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
    async fn repo_settings_are_only_for_configured_repos() {
        let fx = crate::git::tests::fixture();
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let state =
            crate::sync::test_state_with([(fx.clone.slug.clone(), cloned)].into_iter().collect());
        let (owner, name) = fx.clone.slug.split_once('/').unwrap();
        let path = || Path((owner.to_owned(), name.to_owned()));
        let mut events = state.events.subscribe();

        let response = repo_settings(AxState(state.clone()), path())
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let got: RepoSettings = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(got, RepoSettings::default());
        let skill = |skill: &str| RepoSettings {
            review_skill: Some(skill.into()),
            ..RepoSettings::default()
        };
        let response = set_repo_settings(AxState(state.clone()), path(), Json(skill("/review")))
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let saved = state.db.repo_settings(&fx.clone.slug).unwrap();
        assert_eq!(saved.review_skill.as_deref(), Some("review"));
        assert!(saved.auto_review_since.is_some());
        assert!(matches!(
            events.try_recv(),
            Ok(ServerEvent::RepoSettingsChanged(repo)) if repo == fx.clone.slug
        ));

        let other = || Path(("o".to_owned(), "n".to_owned()));
        let response = repo_settings(AxState(state.clone()), other())
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = set_repo_settings(
            AxState(state.clone()),
            other(),
            Json(RepoSettings::default()),
        )
        .await
        .map_err(|e| e.0)
        .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        // A bad skill isn't saved.
        let response = set_repo_settings(AxState(state.clone()), path(), Json(skill("a b")))
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // A permission mode the backend doesn't have isn't either.
        let bad_mode = RepoSettings {
            session_modes: [(grenadine_core::api::AgentBackend::Devin, "auto".into())]
                .into_iter()
                .collect(),
            ..RepoSettings::default()
        };
        let response = set_repo_settings(AxState(state.clone()), path(), Json(bad_mode))
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// A review model is saved only when the backend offers it.
    #[tokio::test]
    async fn repo_settings_check_the_review_model() {
        use std::os::unix::fs::PermissionsExt as _;
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        // A claude that lists "sonnet" and nothing else.
        let claude = dir.path().join("claude");
        std::fs::write(
            &claude,
            "#!/bin/sh\necho '{\"type\":\"control_response\",\"response\":{\"request_id\":\"models\",\"response\":{\"models\":[{\"value\":\"sonnet\",\"displayName\":\"Sonnet\",\"description\":\"d\"}]}}}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let state = crate::sync::test_state_with_agents(
            [(fx.clone.slug.clone(), cloned)].into_iter().collect(),
            claude,
            "devin".into(),
            dir.path().join("worktrees"),
        );
        let (owner, name) = fx.clone.slug.split_once('/').unwrap();
        let path = || Path((owner.to_owned(), name.to_owned()));
        let settings = |model: &str| {
            Json(RepoSettings {
                review_models: [(grenadine_core::api::AgentBackend::Claude, model.to_owned())]
                    .into_iter()
                    .collect(),
                ..RepoSettings::default()
            })
        };

        let response = set_repo_settings(AxState(state.clone()), path(), settings("sonnet"))
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let saved = state.db.repo_settings(&fx.clone.slug).unwrap();
        assert_eq!(
            saved.review_models[&grenadine_core::api::AgentBackend::Claude],
            "sonnet"
        );

        // One the backend doesn't list isn't saved.
        let response = set_repo_settings(AxState(state.clone()), path(), settings("nope"))
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body(response).await, "Claude Code doesn't offer that model");
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
        let state =
            crate::sync::test_state_with([(fx.clone.slug.clone(), cloned)].into_iter().collect());
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
        let state =
            crate::sync::test_state_with([(fx.clone.slug.clone(), cloned)].into_iter().collect());
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
                    ci: None,
                    review: None,
                    review_error: None,
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
        let state =
            crate::sync::test_state_with([(fx.clone.slug.clone(), cloned)].into_iter().collect());
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
                        threads: Default::default(),
                        ci: None,
                        review_decision: None,
                        facts: Default::default(),
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
                        threads: Default::default(),
                        ci: None,
                        review_decision: None,
                        facts: Default::default(),
                    },
                ]),
                None,
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
                    ci: None,
                    review: None,
                    review_error: None,
                },
                &[],
                &[],
                Some(&stack),
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
            if state.db.pr_sync_error(&key(6)).unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(state.db.pr_sync_error(&key(6)).unwrap().is_some());
        // 8 is in an inbox, so it's the poller's.
        assert_eq!(state.db.pr_sync_error(&key(8)).unwrap(), None);
    }

    /// A mode a plan approval carries that the turn can't go on in is a
    /// 400, like an unknown permission mode on a prompt.
    #[tokio::test]
    async fn an_unknown_after_plan_mode_is_rejected() {
        let state = crate::sync::test_state();
        let id = "00000000-0000-0000-0000-000000000000";
        state
            .db
            .create_agent(&AgentSession {
                title: "t".into(),
                branch: "x".into(),
                worktree: "/w".into(),
                base_sha: "s".into(),
                status: grenadine_core::api::AgentStatus::AwaitingApproval,
                ..AgentSession::fixture(id, "o/n")
            })
            .unwrap();
        let approval = |mode| Approval {
            id: "t".into(),
            allow: true,
            message: None,
            mode,
            option: None,
            answers: None,
        };
        let response = approve(
            AxState(state.clone()),
            Path(id.to_owned()),
            Json(approval(Some("plan".into()))),
        )
        .await
        .map_err(|e| e.0)
        .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = approve(
            AxState(state.clone()),
            Path(id.to_owned()),
            Json(approval(Some("auto".into()))),
        )
        .await
        .map_err(|e| e.0)
        .unwrap();
        // Valid, but nothing was waiting for an answer.
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = approve(AxState(state), Path(id.to_owned()), Json(approval(None)))
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// The option an answer picks decides whether the tool use goes
    /// ahead, whatever its `allow` says; an option the request didn't
    /// offer is a 400.
    #[tokio::test]
    async fn an_option_decides_the_answer() {
        use grenadine_core::api::{ApprovalKind, ApprovalOption};
        let state = crate::sync::test_state();
        let id = "00000000-0000-0000-0000-000000000001";
        state
            .db
            .create_agent(&AgentSession {
                title: "t".into(),
                branch: "x".into(),
                worktree: "/w".into(),
                base_sha: "s".into(),
                ..AgentSession::fixture(id, "o/n")
            })
            .unwrap();
        let option = |id: &str, kind| ApprovalOption {
            id: id.into(),
            label: id.into(),
            kind,
        };
        let rx = agent::request_approval(
            &state,
            id,
            "t",
            "Bash",
            &serde_json::json!({}),
            vec![
                option("allow_once", ApprovalKind::AllowOnce),
                option("reject_once", ApprovalKind::RejectOnce),
            ],
        )
        .unwrap();
        let approval = |option: &str| Approval {
            id: "t".into(),
            allow: true,
            message: None,
            mode: None,
            option: Some(option.into()),
            answers: None,
        };
        let response = approve(
            AxState(state.clone()),
            Path(id.to_owned()),
            Json(approval("allow_always")),
        )
        .await
        .map_err(|e| e.0)
        .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = approve(
            AxState(state.clone()),
            Path(id.to_owned()),
            Json(approval("reject_once")),
        )
        .await
        .map_err(|e| e.0)
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let decision = rx.await.unwrap();
        assert!(!decision.allow);
        assert_eq!(decision.option.as_deref(), Some("reject_once"));
    }

    /// A permission mode the backend doesn't list is a 400, like an
    /// unknown model.
    #[tokio::test]
    async fn an_unknown_permission_mode_is_rejected() {
        let state = crate::sync::test_state();
        let req = |mode: &str| NewAgent {
            backend: grenadine_core::api::AgentBackend::Claude,
            repo: GENERAL.into(),
            pr: None,
            prompt: "hi".into(),
            permission_mode: mode.into(),
            model: None,
            images: Vec::new(),
        };
        let response = create_agent(AxState(state.clone()), Json(req("nonsense")))
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body(response).await, UNKNOWN_MODE);
        let response = create_agent(AxState(state), Json(req("auto")))
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// An inbox PR is the poller's: no on-demand sync starts for it.
    #[tokio::test]
    async fn inbox_pr_is_not_synced_on_demand() {
        let fx = crate::git::tests::fixture();
        let cloned = Arc::new(crate::sync::ClonedRepo {
            repo: fx.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let state =
            crate::sync::test_state_with([(fx.clone.slug.clone(), cloned)].into_iter().collect());
        let key = PrKey {
            repo: "owner/name".into(),
            number: 7,
        };
        state
            .db
            .set_inbox_results(
                1,
                Ok(&[crate::github::Hit {
                    key: key.clone(),
                    title: "t".into(),
                    author: "a".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                    url: "u".into(),
                    updated_at: "u".into(),
                    head_oid: "h".into(),
                    threads: Default::default(),
                    ci: None,
                    review_decision: None,
                    facts: Default::default(),
                }]),
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
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(body(response).await, r#""not_synced""#);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(state.on_demand.lock().unwrap().is_empty());
    }
}
