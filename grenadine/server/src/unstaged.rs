//! The sessions' unstaged-change counts, cached so listing sessions
//! doesn't run git per worktree on the request path. One worker recounts
//! the queued sessions and all of them every `SWEEP`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use grenadine_core::api::{ServerEvent, SessionKind};

use crate::sync::State;

/// How often every interactive session is recounted.
const SWEEP: Duration = Duration::from_secs(60);

/// The cached unstaged-change counts and the queue of sessions to
/// recount.
pub struct Unstaged {
    counts: std::sync::Mutex<HashMap<String, u32>>,
    /// The ids queued but not yet recounted; dedups `queue`.
    pending: std::sync::Mutex<HashSet<String>>,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    /// Taken by `run`.
    rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>,
}

impl Default for Unstaged {
    fn default() -> Unstaged {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Unstaged {
            counts: std::sync::Mutex::default(),
            pending: std::sync::Mutex::default(),
            tx,
            rx: std::sync::Mutex::new(Some(rx)),
        }
    }
}

impl Unstaged {
    /// The session's cached count; 0 on a miss.
    pub fn get(&self, id: &str) -> u32 {
        self.counts.lock().unwrap().get(id).copied().unwrap_or(0)
    }

    /// Queues the session for a recount; a no-op when it's already
    /// queued.
    pub fn queue(&self, id: &str) {
        if self.pending.lock().unwrap().insert(id.to_owned()) {
            let _ = self.tx.send(id.to_owned());
        }
    }

    /// Drops the session's count.
    pub fn forget(&self, id: &str) {
        self.counts.lock().unwrap().remove(id);
    }
}

/// Recounts one session's worktree, returning whether its count changed.
/// Unknown and non-interactive sessions keep their count, as do sessions
/// whose count git fails on; a missing worktree or repository counts 0.
pub async fn recount(state: &State, id: &str) -> bool {
    let session = match state.db.agent(id) {
        Ok(Some(s)) => s,
        Ok(None) => return false,
        Err(e) => {
            tracing::warn!("agent {id}: {e:#}");
            return false;
        }
    };
    if session.kind != SessionKind::Interactive {
        return false;
    }
    let count = match state.repo(&session.repo) {
        Some(repo) if std::path::Path::new(&session.worktree).is_dir() => {
            let worktree = session.worktree.clone();
            match tokio::task::spawn_blocking(move || {
                repo.worktree_unstaged_count(std::path::Path::new(&worktree))
            })
            .await
            {
                Ok(Ok(n)) => n.try_into().unwrap_or(u32::MAX),
                Ok(Err(e)) => {
                    tracing::warn!("agent {id}: {e:#}");
                    return false;
                }
                Err(e) => {
                    tracing::warn!("agent {id}: {e:#}");
                    return false;
                }
            }
        }
        _ => 0,
    };
    let mut counts = state.agents.unstaged.counts.lock().unwrap();
    if counts.get(id).copied().unwrap_or(0) == count {
        return false;
    }
    counts.insert(id.to_owned(), count);
    true
}

/// Queues every interactive session.
fn queue_all(state: &State) {
    match state.db.agents() {
        Ok(sessions) => {
            for s in sessions {
                if s.kind == SessionKind::Interactive {
                    state.agents.unstaged.queue(&s.id);
                }
            }
        }
        Err(e) => tracing::warn!("can't list the sessions: {e:#}"),
    }
}

/// Recounts the queued sessions one at a time and all of them every
/// `SWEEP`; the pages hear about the changes through AgentsChanged.
pub async fn run(state: Arc<State>) {
    let Some(mut rx) = state.agents.unstaged.rx.lock().unwrap().take() else {
        return;
    };
    // The first tick is immediate, so the startup counts queue here.
    let mut sweep = tokio::time::interval(SWEEP);
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = sweep.tick() => queue_all(&state),
            id = rx.recv() => {
                let Some(id) = id else { break };
                state.agents.unstaged.pending.lock().unwrap().remove(&id);
                if recount(&state, &id).await {
                    state.send(ServerEvent::AgentsChanged);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_dedups_and_forget_drops() {
        let unstaged = Unstaged::default();
        assert_eq!(unstaged.get("a"), 0);
        unstaged.counts.lock().unwrap().insert("a".into(), 3);
        unstaged.forget("a");
        assert_eq!(unstaged.get("a"), 0);

        unstaged.queue("a");
        unstaged.queue("a");
        unstaged.queue("b");
        assert!(unstaged.pending.lock().unwrap().contains("a"));
        let mut rx = unstaged.rx.lock().unwrap().take().unwrap();
        assert_eq!(rx.try_recv().unwrap(), "a");
        assert_eq!(rx.try_recv().unwrap(), "b");
        assert!(rx.try_recv().is_err());
    }
}
