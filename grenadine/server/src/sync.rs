//! Keeps the database and the clones' refs in step with GitHub.
//!
//! Every poll runs all inbox searches in one request and then syncs each PR
//! that changed since its last sync. On the first poll after startup every
//! PR is synced, which recomputes all versions and so detects drift.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use grenadine_core::api::{
    Person, PrKey, ServerEvent, StackPr, SyncPhase, SyncStatus, SyncingPr, Version,
};
use grenadine_core::versions::{self, History};
use tokio::sync::{Mutex, Notify, broadcast};

use crate::db::{Db, PrMeta, SyncMark};
use crate::git::{REF_PREFIX, Repo};
use crate::github::{self, GitHub, PrData};
use crate::stack;

/// How many PRs sync at the same time.
const CONCURRENCY: usize = 4;

/// A configured clone and the lock that serializes sync's writes to it.
pub struct ClonedRepo {
    pub repo: Repo,
    /// Keeps sync's mutating git work (fetches, ref updates) on one clone
    /// from overlapping. Reads don't take it because git serves objects
    /// safely while a fetch is in progress.
    pub git_lock: Mutex<()>,
}

pub struct State {
    pub db: Db,
    pub github: GitHub,
    /// The configured clones by `owner/name`. Readers clone the `Repo`
    /// without locking; sync serializes its writes through `git_lock`.
    pub repos: BTreeMap<String, Arc<ClonedRepo>>,
    /// Matches the branches that stacks end at.
    pub trunk: fancy_regex::Regex,
    pub events: broadcast::Sender<ServerEvent>,
    /// The current poll's progress, shown in the UI's top bar.
    pub sync_status: std::sync::Mutex<SyncStatus>,
    /// Wakes the poller early, e.g. after an inbox was edited.
    pub poke: Notify,
    /// PRs a page requested a sync for that are being synced on demand
    /// right now, so a second request doesn't start the same sync again.
    pub on_demand: std::sync::Mutex<BTreeSet<PrKey>>,
    /// Cancelled on shutdown. The poller then starts no new syncs and the
    /// event streams end.
    pub shutdown: tokio_util::sync::CancellationToken,
}

impl State {
    pub fn send(&self, event: ServerEvent) {
        // Failing only means that no page is listening.
        let _ = self.events.send(event);
    }

    /// The clone for `owner/name`, for callers that only read from it.
    pub fn repo(&self, slug: &str) -> Option<Repo> {
        Some(self.repos.get(slug)?.repo.clone())
    }

    /// Mutates the sync status and broadcasts the new snapshot. Pages get
    /// the whole status every time so a missed event self-heals.
    fn update_status(&self, f: impl FnOnce(&mut SyncStatus)) {
        let snapshot = {
            let mut status = self.sync_status.lock().unwrap();
            f(&mut status);
            status.clone()
        };
        self.send(ServerEvent::SyncStatus(snapshot));
    }

    fn status_searching(&self) {
        self.update_status(|s| s.phase = SyncPhase::Searching);
    }

    fn status_stale(&self, n: usize) {
        self.update_status(|s| {
            s.remaining = n;
            if n > 0 {
                s.phase = SyncPhase::Syncing;
            }
        });
    }

    /// Marks `key` as in flight; its title may not be in the database yet.
    fn status_started(&self, key: &PrKey) {
        let pr = SyncingPr {
            key: key.clone(),
            title: self.db.pr_title(key).ok().flatten(),
        };
        self.update_status(|s| s.in_flight.push(pr));
    }

    fn status_finished(&self, key: &PrKey) {
        self.update_status(|s| {
            s.in_flight.retain(|p| &p.key != key);
            s.remaining = s.remaining.saturating_sub(1);
        });
    }

    /// Starts a background sync for a PR no inbox covers, unless a sync
    /// for it is already running — either the poller's (read from the
    /// status's `in_flight`) or a previous on-demand request's (tracked in
    /// `on_demand`). Unlike the poller it doesn't touch the sync status:
    /// `remaining` counts only the current poll's PRs.
    pub fn sync_once(self: &Arc<State>, key: PrKey) {
        {
            let status = self.sync_status.lock().unwrap();
            if status.in_flight.iter().any(|p| p.key == key) {
                return;
            }
        }
        if !self.on_demand.lock().unwrap().insert(key.clone()) {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            let result = sync_pr(&state, &key).await;
            state.on_demand.lock().unwrap().remove(&key);
            if let Err(e) = result {
                tracing::warn!("{}#{}: {e:#}", key.repo, key.number);
                let _ = state.db.store_sync_error(&key, &format!("{e:#}"));
            }
            state.send(ServerEvent::PrChanged(key));
        });
    }

    /// Ends the poll and records its outcome.
    fn status_idle(&self, result: &Result<()>) {
        self.update_status(|s| {
            s.phase = SyncPhase::Idle;
            s.remaining = 0;
            s.in_flight.clear();
            s.last_finished = Some(chrono::Utc::now().timestamp());
            s.last_error = result.as_ref().err().map(|e| format!("{e:#}"));
        });
    }
}

pub async fn run(state: Arc<State>, interval: Duration) {
    let mut first = true;
    loop {
        if state.shutdown.is_cancelled() {
            break;
        }
        let result = poll(&state, first).await;
        state.status_idle(&result);
        if let Err(e) = result {
            tracing::warn!("poll failed: {e:#}");
        }
        first = false;
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = state.poke.notified() => {}
            _ = state.shutdown.cancelled() => break,
        }
    }
    tracing::info!("sync loop stopped");
}

async fn poll(state: &Arc<State>, sync_all: bool) -> Result<()> {
    state.status_searching();
    let inboxes = state.db.inboxes()?;
    let slugs: Vec<String> = state.repos.keys().cloned().collect();
    let queries: Vec<String> = inboxes
        .iter()
        .map(|i| github::search_query(&i.filter, &slugs))
        .collect();
    let results = tokio::select! {
        r = state.github.search(&queries) => r?,
        _ = state.shutdown.cancelled() => return Ok(()),
    };

    let mut hits = BTreeMap::new();
    for (inbox, result) in inboxes.iter().zip(results) {
        match result {
            Ok(found) => {
                let found: Vec<_> = found
                    .into_iter()
                    .filter(|h| state.repos.contains_key(&h.key.repo))
                    .collect();
                state.db.set_inbox_results(inbox.id, Ok(&found))?;
                hits.extend(found.into_iter().map(|h| (h.key.clone(), h)));
            }
            Err(e) => {
                tracing::warn!("inbox {:?} ({}): {e}", inbox.name, inbox.filter);
                state.db.set_inbox_results(inbox.id, Err(&e))?;
            }
        }
    }
    state.send(ServerEvent::InboxesChanged);

    let stale: Vec<PrKey> = hits
        .into_values()
        .filter(|h| {
            sync_all
                || state.db.sync_mark(&h.key).ok().flatten()
                    != Some(SyncMark {
                        updated_at: h.updated_at.clone(),
                        head_oid: h.head_oid.clone(),
                    })
        })
        .map(|h| h.key)
        .collect();
    state.status_stale(stale.len());
    if !stale.is_empty() {
        tracing::info!("syncing {} PRs", stale.len());
    }
    futures::stream::iter(stale)
        .take_until(state.shutdown.cancelled())
        .for_each_concurrent(CONCURRENCY, |key| async move {
            state.status_started(&key);
            let result = sync_pr(state, &key).await;
            state.status_finished(&key);
            if let Err(e) = result {
                tracing::warn!("{}#{}: {e:#}", key.repo, key.number);
                let _ = state.db.store_sync_error(&key, &format!("{e:#}"));
            }
            state.send(ServerEvent::PrChanged(key));
            // A synced or failed PR shows up differently in the inboxes.
            state.send(ServerEvent::InboxesChanged);
        })
        .await;
    Ok(())
}

/// The last commit that had been pushed when the PR was opened, going by
/// when each commit's first check suite ran. Only useful without the
/// activity log.
fn initial_guess(data: &PrData, created_at: i64) -> Option<String> {
    data.commits
        .iter()
        .rev()
        .find(|(_, suite)| suite.is_some_and(|t| t <= created_at))
        .map(|(sha, _)| sha.clone())
}

fn short(versions: &[Version]) -> String {
    versions
        .iter()
        .map(|v| format!("v{}={}", v.number, &v.sha[..v.sha.len().min(10)]))
        .collect::<Vec<_>>()
        .join(", ")
}

async fn sync_pr(state: &State, key: &PrKey) -> Result<()> {
    let repo = state
        .repos
        .get(&key.repo)
        .ok_or_else(|| anyhow!("{} is not a configured repository", key.repo))?
        .clone();
    let data = state.github.pr(key).await?;
    let activities = match &data.head_repo {
        Some(head_repo) => state.github.activity(head_repo, &data.head_ref).await?,
        None => Vec::new(),
    };
    let mut comments = state.github.review_comments(key).await?;
    let resolved = state.github.resolved_threads(key).await?;
    for c in &mut comments {
        c.resolved = resolved.contains(&c.id);
    }

    let created_at = github::unix(&data.created_at)?;
    let history = History {
        created_at,
        head: data.head_oid.clone(),
        initial_guess: initial_guess(&data, created_at),
        activities,
        force_pushes: data.force_pushes.clone(),
    };
    let old = state.db.versions(key)?;

    let number = key.number;
    let base_ref = data.base_ref.clone();
    let (computed, mut versions) = {
        let _guard = repo.git_lock.lock().await;
        let repo = repo.repo.clone();
        let old = old.clone();
        tokio::task::spawn_blocking(move || {
            compute_and_store(&repo, number, &base_ref, &history, &old)
        })
        .await??
    };
    // Who pushed is only shown, so failing to look it up doesn't fail the
    // sync.
    if let Err(e) = fill_pushers(state, &key.repo, &mut versions, &old).await {
        tracing::warn!("{}#{}: can't look up pushers: {e:#}", key.repo, key.number);
    }

    let old_shas: Vec<&str> = old.iter().map(|v| v.sha.as_str()).collect();
    let new_shas: Vec<&str> = versions.iter().map(|v| v.sha.as_str()).collect();
    // New pushes only append versions; anything else is drift.
    let drifted = !old.is_empty() && !new_shas.starts_with(&old_shas);
    let (old_text, new_text) = (short(&old), short(&versions));
    if drifted {
        tracing::warn!(
            "{}#{}: versions drifted from [{old_text}] to [{new_text}]",
            key.repo,
            key.number
        );
    }

    let me = StackPr {
        number: key.number,
        title: data.title.clone(),
        state: data.state.clone(),
        is_draft: data.is_draft,
        url: data.url.clone(),
        updated_at: data.updated_at.clone(),
        head_oid: data.head_oid.clone(),
        parent: None,
    };
    let head_ref =
        (data.head_repo.as_deref() == Some(key.repo.as_str())).then_some(data.head_ref.as_str());
    let lookup = |field, refs: Vec<String>| async move {
        state.github.prs_by_ref(&key.repo, field, &refs).await
    };
    // The stack is secondary; failing to fetch it keeps the stored one
    // rather than failing the whole sync.
    let stack = match stack::walk(me, &data.base_ref, head_ref, &state.trunk, lookup).await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("{}#{}: can't fetch the stack: {e:#}", key.repo, key.number);
            None
        }
    };

    let meta = PrMeta {
        key: key.clone(),
        title: data.title,
        body: data.body,
        author: data.author,
        state: data.state,
        is_draft: data.is_draft,
        url: data.url,
        created_at: data.created_at,
        updated_at: data.updated_at,
        base_ref: data.base_ref,
        head_ref: data.head_ref,
        head_oid: data.head_oid,
    };
    state.db.store_sync(
        &meta,
        &versions,
        &comments,
        stack.as_ref(),
        computed.approximate,
        drifted.then_some((old_text.as_str(), new_text.as_str())),
    )?;
    Ok(())
}

/// How long a cached user's name and avatar are used before they're
/// looked up again.
const USER_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// Completes the pushers of `versions`. Pushers from the push history get
/// their display names from the users cache, which looks up the users it
/// doesn't have on GitHub. Versions without a known pusher get their
/// commit's author as a guess. A commit's author never changes, so guesses
/// carry over from `old` versions with the same SHA.
async fn fill_pushers(
    state: &State,
    repo: &str,
    versions: &mut [Version],
    old: &[Version],
) -> Result<()> {
    let old_guesses: BTreeMap<&str, &Person> = old
        .iter()
        .filter(|v| v.pushed_by_is_guess)
        .filter_map(|v| Some((v.sha.as_str(), v.pushed_by.as_ref()?)))
        .collect();
    for v in versions.iter_mut().filter(|v| v.pushed_by.is_none()) {
        if let Some(p) = old_guesses.get(v.sha.as_str()) {
            v.pushed_by = Some((*p).clone());
            v.pushed_by_is_guess = true;
        }
    }
    let unknown: Vec<String> = versions
        .iter()
        .filter(|v| v.pushed_by.is_none())
        .map(|v| v.sha.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !unknown.is_empty() {
        let authors = state.github.commit_authors(repo, &unknown).await?;
        for v in versions.iter_mut().filter(|v| v.pushed_by.is_none()) {
            if let Some(p) = authors.get(&v.sha) {
                v.pushed_by = Some(p.clone());
                v.pushed_by_is_guess = true;
            }
        }
    }

    let logins: Vec<String> = versions
        .iter()
        .filter(|v| !v.pushed_by_is_guess)
        .filter_map(|v| v.pushed_by.as_ref()?.login.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let now = chrono::Utc::now().timestamp();
    let mut users = state.db.cached_users(&logins, now - USER_TTL_SECS)?;
    let stale: Vec<String> = logins
        .iter()
        .filter(|l| !users.contains_key(*l))
        .cloned()
        .collect();
    if !stale.is_empty() {
        let found = state.github.users(&stale).await?;
        state.db.store_users(&stale, &found, now)?;
        users.extend(stale.into_iter().map(|l| {
            let p = found.get(&l).cloned();
            (l, p)
        }));
    }
    for p in versions
        .iter_mut()
        .filter(|v| !v.pushed_by_is_guess)
        .filter_map(|v| v.pushed_by.as_mut())
    {
        let Some(Some(user)) = p.login.as_ref().and_then(|l| users.get(l)) else {
            continue;
        };
        p.name = user.name.clone();
        if p.avatar_url.is_none() {
            p.avatar_url = user.avatar_url.clone();
        }
    }
    Ok(())
}

/// Fetches the commits the history mentions, computes the versions and
/// points the PR's refs at them.
fn compute_and_store(
    repo: &Repo,
    number: u64,
    base_ref: &str,
    history: &History,
    old: &[Version],
) -> Result<(versions::Computed, Vec<Version>)> {
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    wanted.insert(history.head.clone());
    wanted.extend(history.initial_guess.clone());
    for a in &history.activities {
        wanted.insert(a.before.clone());
        wanted.insert(a.after.clone());
    }
    for f in &history.force_pushes {
        wanted.extend(f.before.clone());
        wanted.insert(f.after.clone());
    }
    wanted.retain(|s| !s.is_empty() && !s.bytes().all(|b| b == b'0'));
    let wanted: Vec<String> = wanted.into_iter().collect();
    repo.fetch_commits(&wanted);

    let prefix = format!("{REF_PREFIX}/{number}");
    let target_ref = format!("{prefix}/target");
    let target = repo
        .fetch_branch(base_ref, &target_ref)
        .with_context(|| format!("can't fetch the target branch {base_ref}"))?;

    let computed = versions::compute(history, repo);
    let versions: Vec<Version> = computed
        .versions
        .iter()
        .enumerate()
        .map(|(i, v)| Version {
            number: i as u32 + 1,
            sha: v.sha.clone(),
            merge_base: if v.missing {
                None
            } else {
                repo.merge_base(&v.sha, &target)
            },
            kind: v.kind,
            pushed_at: v
                .pushed_at
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| t.to_rfc3339()),
            pushed_by: v.pushed_by.clone(),
            pushed_by_is_guess: false,
            missing: v.missing,
        })
        .collect();

    let mut set: Vec<(String, String)> = versions
        .iter()
        .filter(|v| !v.missing)
        .map(|v| (format!("{prefix}/v{}", v.number), v.sha.clone()))
        .collect();
    // Commits that are no longer versions keep a ref, so that they're never
    // garbage-collected.
    let current: BTreeSet<&str> = versions.iter().map(|v| v.sha.as_str()).collect();
    set.extend(
        old.iter()
            .filter(|v| !current.contains(v.sha.as_str()) && repo.has_commit(&v.sha))
            .map(|v| (format!("{prefix}/orphan/{}", v.sha), v.sha.clone())),
    );
    let wanted_refs: BTreeSet<&str> = set.iter().map(|(r, _)| r.as_str()).collect();
    let delete: Vec<String> = repo
        .refs(&format!("{prefix}/"))?
        .into_keys()
        .filter(|r| {
            let name = r.rsplit('/').next().unwrap_or_default();
            // Only version refs go away; orphans and the target stay.
            name.starts_with('v')
                && r == &format!("{prefix}/{name}")
                && !wanted_refs.contains(r.as_str())
        })
        .collect();
    repo.update_refs(&set, &delete)?;
    Ok((computed, versions))
}

#[cfg(test)]
pub fn test_state() -> Arc<State> {
    test_state_with(BTreeMap::new())
}

#[cfg(test)]
pub fn test_state_with(repos: BTreeMap<String, Arc<ClonedRepo>>) -> Arc<State> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Arc::new(State {
        db: Db::in_memory(),
        github: GitHub::with_api("dummy", "http://127.0.0.1:9").unwrap(),
        repos,
        trunk: fancy_regex::Regex::new(stack::DEFAULT_TRUNK).unwrap(),
        events: broadcast::channel(1).0,
        sync_status: std::sync::Mutex::new(SyncStatus::default()),
        poke: Notify::new(),
        on_demand: std::sync::Mutex::new(BTreeSet::new()),
        shutdown: tokio_util::sync::CancellationToken::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_token_stops_the_loop() {
        let state = test_state();
        state.shutdown.cancel();
        tokio::time::timeout(
            Duration::from_secs(1),
            run(state, Duration::from_secs(3600)),
        )
        .await
        .expect("run did not stop after shutdown");
    }

    fn status(state: &State) -> SyncStatus {
        state.sync_status.lock().unwrap().clone()
    }

    #[test]
    fn sync_status_transitions() {
        let state = test_state();
        let a = PrKey {
            repo: "o/n".into(),
            number: 1,
        };
        let b = PrKey {
            repo: "o/n".into(),
            number: 2,
        };

        state.status_searching();
        assert_eq!(status(&state).phase, SyncPhase::Searching);

        state.status_stale(3);
        assert_eq!(status(&state).phase, SyncPhase::Syncing);
        assert_eq!(status(&state).remaining, 3);

        state.status_started(&a);
        state.status_started(&b);
        state.status_finished(&a);
        let s = status(&state);
        assert_eq!(s.remaining, 2);
        assert_eq!(
            s.in_flight,
            [SyncingPr {
                key: b.clone(),
                title: None
            }]
        );

        state.status_idle(&Ok(()));
        let s = status(&state);
        assert_eq!(s.phase, SyncPhase::Idle);
        assert_eq!(s.remaining, 0);
        assert!(s.in_flight.is_empty());
        assert!(s.last_finished.is_some());
        assert_eq!(s.last_error, None);

        let err: Result<()> = Err(anyhow!("boom"));
        state.status_idle(&err);
        assert_eq!(status(&state).last_error.as_deref(), Some("boom"));
        state.status_idle(&Ok(()));
        assert_eq!(status(&state).last_error, None);
    }

    #[test]
    fn sync_status_updates_broadcast() {
        let state = test_state();
        let mut rx = state.events.subscribe();
        state.status_searching();
        let event = rx.try_recv().unwrap();
        let ServerEvent::SyncStatus(s) = event else {
            panic!("expected a SyncStatus event, got {event:?}");
        };
        assert_eq!(s.phase, SyncPhase::Searching);
    }
}
