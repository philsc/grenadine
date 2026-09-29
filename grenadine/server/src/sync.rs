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
use grenadine_core::api::{PrKey, ServerEvent, Version};
use grenadine_core::versions::{self, History};
use tokio::sync::{Mutex, Notify, broadcast};

use crate::db::{Db, PrMeta, SyncMark};
use crate::git::{REF_PREFIX, Repo};
use crate::github::{self, GitHub, PrData};

/// How many PRs sync at the same time.
const CONCURRENCY: usize = 4;

pub struct State {
    pub db: Db,
    pub github: GitHub,
    /// The configured clones by `owner/name`. The mutex keeps git operations
    /// on one clone from running concurrently.
    pub repos: BTreeMap<String, Arc<Mutex<Repo>>>,
    pub events: broadcast::Sender<ServerEvent>,
    /// Wakes the poller early, e.g. after an inbox was edited.
    pub poke: Notify,
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
    pub async fn repo(&self, slug: &str) -> Option<Repo> {
        Some(self.repos.get(slug)?.lock().await.clone())
    }
}

pub async fn run(state: Arc<State>, interval: Duration) {
    let mut first = true;
    loop {
        if state.shutdown.is_cancelled() {
            break;
        }
        if let Err(e) = poll(&state, first).await {
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
                let keys: Vec<PrKey> = found.iter().map(|h| h.key.clone()).collect();
                state.db.set_inbox_results(inbox.id, Ok(&keys))?;
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
    if !stale.is_empty() {
        tracing::info!("syncing {} PRs", stale.len());
    }
    futures::stream::iter(stale)
        .take_until(state.shutdown.cancelled())
        .for_each_concurrent(CONCURRENCY, |key| async move {
            if let Err(e) = sync_pr(state, &key).await {
                tracing::warn!("{}#{}: {e:#}", key.repo, key.number);
                let _ = state.db.store_sync_error(&key, &format!("{e:#}"));
            }
            state.send(ServerEvent::PrChanged(key));
        })
        .await;
    if sync_all {
        // Now that the PRs exist in the database, the inboxes can list them.
        state.send(ServerEvent::InboxesChanged);
    }
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
    let comments = state.github.review_comments(key).await?;

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
    let (computed, versions) = {
        let repo = repo.lock_owned().await;
        let old = old.clone();
        tokio::task::spawn_blocking(move || {
            compute_and_store(&repo, number, &base_ref, &history, &old)
        })
        .await??
    };

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
        computed.approximate,
        drifted.then_some((old_text.as_str(), new_text.as_str())),
    )?;
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
    let _ = rustls::crypto::ring::default_provider().install_default();
    Arc::new(State {
        db: Db::in_memory(),
        github: GitHub::new("dummy").unwrap(),
        repos: BTreeMap::new(),
        events: broadcast::channel(1).0,
        poke: Notify::new(),
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
}
