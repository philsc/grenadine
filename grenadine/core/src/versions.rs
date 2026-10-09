//! Splits a PR's push history into versions.
//!
//! The rules:
//! - The head when the PR was opened is one version, however many commits
//!   the PR had.
//! - Every force push is one version: its new head.
//! - Every commit that a fast-forward push adds is its own version, even if
//!   several commits were pushed at once.
//!
//! The push history comes from GitHub's repository activity log, which
//! records every push to the head branch with its before and after SHAs. For
//! PRs the log doesn't cover (it only goes back to around March 2023, and is
//! gone with a deleted fork) the history is reconstructed from the PR's
//! force-push events, and the result is flagged as approximate.

use crate::api::{Person, VersionKind};

/// An entry of GitHub's repository activity log for the PR's head branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    /// Unix seconds.
    pub timestamp: i64,
    pub kind: ActivityKind,
    pub before: String,
    pub after: String,
    /// Who pushed.
    pub actor: Option<Person>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    Push,
    ForcePush,
    BranchCreation,
    BranchDeletion,
    /// Merge queue and pull request merges, among others; treated like a
    /// push.
    Other,
}

/// A `HeadRefForcePushedEvent` from the PR's timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForcePush {
    /// Unix seconds.
    pub timestamp: i64,
    /// `None` when GitHub no longer has the commit.
    pub before: Option<String>,
    pub after: String,
    /// Who force-pushed.
    pub actor: Option<Person>,
}

/// What is known about a PR's push history.
#[derive(Clone, Debug, Default)]
pub struct History {
    /// When the PR was opened, in Unix seconds.
    pub created_at: i64,
    /// The PR's current head.
    pub head: String,
    /// The activity log of the head branch, oldest first.
    pub activities: Vec<Activity>,
    /// The PR's force pushes, oldest first.
    pub force_pushes: Vec<ForcePush>,
    /// A guess at the head when the PR was opened, used when neither the
    /// activity log nor a force push tells.
    pub initial_guess: Option<String>,
}

/// Answers questions about the commit graph of the local clone.
pub trait CommitGraph {
    /// The commits of `to` that `from` doesn't have, following first parents
    /// only, oldest first. `None` when that can't be determined, e.g. because
    /// a commit is missing or `from` isn't an ancestor of `to`.
    fn first_parent_range(&self, from: &str, to: &str) -> Option<Vec<String>>;

    /// Whether the clone has the commit.
    fn has(&self, sha: &str) -> bool;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputedVersion {
    pub sha: String,
    pub kind: VersionKind,
    /// Unix seconds.
    pub pushed_at: Option<i64>,
    /// Who pushed, when the push history tells.
    pub pushed_by: Option<Person>,
    pub missing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Computed {
    pub versions: Vec<ComputedVersion>,
    /// The versions were reconstructed without the activity log.
    pub approximate: bool,
}

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

fn is_zero(sha: &str) -> bool {
    sha.is_empty() || sha.bytes().all(|b| b == b'0')
}

struct Builder<'a, G> {
    graph: &'a G,
    versions: Vec<ComputedVersion>,
}

impl<G: CommitGraph> Builder<'_, G> {
    fn push_one(
        &mut self,
        sha: &str,
        kind: VersionKind,
        pushed_at: Option<i64>,
        pushed_by: Option<&Person>,
    ) {
        self.versions.push(ComputedVersion {
            sha: sha.to_owned(),
            kind,
            pushed_at,
            pushed_by: pushed_by.cloned(),
            missing: !self.graph.has(sha),
        });
    }

    /// Adds one version per commit from `from` (exclusive) to `to`. When the
    /// commits in between can't be listed, `to` alone becomes a version, as
    /// if it had been force-pushed.
    fn push_range(
        &mut self,
        from: &str,
        to: &str,
        pushed_at: Option<i64>,
        pushed_by: Option<&Person>,
    ) {
        if from == to {
            return;
        }
        match self.graph.first_parent_range(from, to) {
            Some(commits) => {
                for sha in commits {
                    self.push_one(&sha, VersionKind::Push, pushed_at, pushed_by);
                }
            }
            None => self.push_one(to, VersionKind::ForcePush, pushed_at, pushed_by),
        }
    }

    fn cur(&self) -> Option<&str> {
        self.versions.last().map(|v| v.sha.as_str())
    }
}

/// Computes the versions of a PR. The result only depends on `history` and
/// the commit graph, so it is the same every time it is computed.
pub fn compute(history: &History, graph: &impl CommitGraph) -> Computed {
    let mut b = Builder {
        graph,
        versions: Vec::new(),
    };

    // The activity log covers the PR when it shows what the branch pointed
    // at when the PR was opened.
    let initial_activity = history.activities.iter().rfind(|a| {
        a.timestamp <= history.created_at
            && a.kind != ActivityKind::BranchDeletion
            && !is_zero(&a.after)
    });

    let approximate = match initial_activity {
        Some(initial) => {
            b.push_one(
                &initial.after,
                VersionKind::Initial,
                Some(initial.timestamp),
                initial.actor.as_ref(),
            );
            for a in history
                .activities
                .iter()
                .filter(|a| a.timestamp > history.created_at)
            {
                let cur = b.cur().unwrap_or(ZERO_SHA).to_owned();
                if a.kind == ActivityKind::BranchDeletion || is_zero(&a.after) || a.after == cur {
                    continue;
                }
                let fast_forward =
                    matches!(a.kind, ActivityKind::Push | ActivityKind::Other) && a.before == cur;
                if fast_forward {
                    b.push_range(&cur, &a.after, Some(a.timestamp), a.actor.as_ref());
                } else {
                    b.push_one(
                        &a.after,
                        VersionKind::ForcePush,
                        Some(a.timestamp),
                        a.actor.as_ref(),
                    );
                }
            }
            false
        }
        None => {
            let initial = history
                .force_pushes
                .first()
                .and_then(|f| f.before.as_deref())
                .or(history.initial_guess.as_deref())
                .unwrap_or(&history.head);
            b.push_one(initial, VersionKind::Initial, None, None);
            for f in &history.force_pushes {
                let cur = b.cur().unwrap_or(ZERO_SHA).to_owned();
                // Regular pushes between the last version and this force
                // push each add their commits.
                if let Some(before) = &f.before {
                    b.push_range(&cur, before, None, None);
                }
                if b.cur() != Some(f.after.as_str()) {
                    b.push_one(
                        &f.after,
                        VersionKind::ForcePush,
                        Some(f.timestamp),
                        f.actor.as_ref(),
                    );
                }
            }
            true
        }
    };

    // Pushes the activity log hasn't caught up with yet.
    let cur = b.cur().unwrap_or(ZERO_SHA).to_owned();
    b.push_range(&cur, &history.head, None, None);

    Computed {
        versions: b.versions,
        approximate,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    /// A commit graph where every commit has at most one parent.
    #[derive(Default)]
    struct Graph {
        parent: HashMap<String, String>,
        missing: HashSet<String>,
    }

    impl Graph {
        fn chain(&mut self, commits: &[&str]) {
            for w in commits.windows(2) {
                self.parent.insert(w[1].to_owned(), w[0].to_owned());
            }
        }
    }

    impl CommitGraph for Graph {
        fn first_parent_range(&self, from: &str, to: &str) -> Option<Vec<String>> {
            if self.missing.contains(from) || self.missing.contains(to) {
                return None;
            }
            let mut out = Vec::new();
            let mut cur = to.to_owned();
            while cur != from {
                out.push(cur.clone());
                cur = self.parent.get(&cur)?.clone();
            }
            out.reverse();
            Some(out)
        }

        fn has(&self, sha: &str) -> bool {
            !self.missing.contains(sha)
        }
    }

    fn act(timestamp: i64, kind: ActivityKind, before: &str, after: &str) -> Activity {
        Activity {
            timestamp,
            kind,
            before: before.to_owned(),
            after: after.to_owned(),
            actor: None,
        }
    }

    fn person(login: &str) -> Person {
        Person {
            login: Some(login.to_owned()),
            ..Default::default()
        }
    }

    fn act_by(
        timestamp: i64,
        kind: ActivityKind,
        before: &str,
        after: &str,
        login: &str,
    ) -> Activity {
        Activity {
            actor: Some(person(login)),
            ..act(timestamp, kind, before, after)
        }
    }

    fn pushers(c: &Computed) -> Vec<Option<&str>> {
        c.versions
            .iter()
            .map(|v| v.pushed_by.as_ref().and_then(|p| p.login.as_deref()))
            .collect()
    }

    fn shas(c: &Computed) -> Vec<&str> {
        c.versions.iter().map(|v| v.sha.as_str()).collect()
    }

    fn kinds(c: &Computed) -> Vec<VersionKind> {
        c.versions.iter().map(|v| v.kind).collect()
    }

    /// The history from the design: C1 and C2 pushed and the PR opened; C3
    /// pushed; rebased to C4 and force-pushed; C5 pushed; squashed into C6
    /// and force-pushed; C7 and C8 pushed together.
    fn example_graph() -> Graph {
        let mut g = Graph::default();
        g.chain(&["M1", "C1", "C2", "C3"]);
        g.chain(&["M2", "C1'", "C2'", "C3'", "C4", "C5"]);
        g.chain(&["M2", "C6", "C7", "C8"]);
        g
    }

    #[test]
    fn example_from_the_activity_log() {
        let g = example_graph();
        let history = History {
            created_at: 100,
            head: "C8".into(),
            activities: vec![
                act(90, ActivityKind::BranchCreation, ZERO_SHA, "C2"),
                act(110, ActivityKind::Push, "C2", "C3"),
                act(120, ActivityKind::ForcePush, "C3", "C4"),
                act(130, ActivityKind::Push, "C4", "C5"),
                act(140, ActivityKind::ForcePush, "C5", "C6"),
                act(150, ActivityKind::Push, "C6", "C8"),
            ],
            ..Default::default()
        };
        let c = compute(&history, &g);
        assert_eq!(shas(&c), ["C2", "C3", "C4", "C5", "C6", "C7", "C8"]);
        use VersionKind::*;
        assert_eq!(
            kinds(&c),
            [Initial, Push, ForcePush, Push, ForcePush, Push, Push]
        );
        assert!(!c.approximate);
        assert_eq!(c.versions[6].pushed_at, Some(150));
    }

    #[test]
    fn example_from_force_push_events() {
        let g = example_graph();
        let history = History {
            created_at: 100,
            head: "C8".into(),
            force_pushes: vec![
                ForcePush {
                    timestamp: 120,
                    before: Some("C3".into()),
                    after: "C4".into(),
                    actor: Some(person("alice")),
                },
                ForcePush {
                    timestamp: 140,
                    before: Some("C5".into()),
                    after: "C6".into(),
                    actor: Some(person("bob")),
                },
            ],
            initial_guess: Some("C2".into()),
            ..Default::default()
        };
        let c = compute(&history, &g);
        // Without the activity log the first force push's "before" is the
        // best guess at the initial head, so C3 is folded into version 1.
        assert_eq!(shas(&c), ["C3", "C4", "C5", "C6", "C7", "C8"]);
        assert!(c.approximate);
        // Only force pushes say who pushed.
        assert_eq!(
            pushers(&c),
            [None, Some("alice"), None, Some("bob"), None, None]
        );
    }

    #[test]
    fn every_commit_of_a_push_has_its_pusher() {
        let g = example_graph();
        let history = History {
            created_at: 100,
            head: "C8".into(),
            activities: vec![
                act_by(90, ActivityKind::BranchCreation, ZERO_SHA, "C2", "alice"),
                act_by(110, ActivityKind::Push, "C2", "C3", "bob"),
                act_by(120, ActivityKind::ForcePush, "C3", "C4", "carol"),
                act_by(130, ActivityKind::Push, "C4", "C5", "alice"),
                act_by(140, ActivityKind::ForcePush, "C5", "C6", "bob"),
                act_by(150, ActivityKind::Push, "C6", "C7", "carol"),
            ],
            ..Default::default()
        };
        let c = compute(&history, &g);
        assert_eq!(shas(&c), ["C2", "C3", "C4", "C5", "C6", "C7", "C8"]);
        // C8 isn't in the activity log yet, so its pusher is unknown.
        assert_eq!(
            pushers(&c),
            [
                Some("alice"),
                Some("bob"),
                Some("carol"),
                Some("alice"),
                Some("bob"),
                Some("carol"),
                None
            ]
        );

        let history = History {
            activities: vec![
                act_by(90, ActivityKind::BranchCreation, ZERO_SHA, "C6", "alice"),
                act_by(150, ActivityKind::Push, "C6", "C8", "bob"),
            ],
            ..history
        };
        assert_eq!(
            pushers(&compute(&history, &g)),
            [Some("alice"), Some("bob"), Some("bob")]
        );
    }

    #[test]
    fn fallback_uses_the_guess_without_force_pushes() {
        let mut g = Graph::default();
        g.chain(&["M", "A", "B", "C"]);
        let history = History {
            created_at: 100,
            head: "C".into(),
            initial_guess: Some("A".into()),
            ..Default::default()
        };
        let c = compute(&history, &g);
        assert_eq!(shas(&c), ["A", "B", "C"]);
        assert!(c.approximate);
    }

    #[test]
    fn fallback_without_a_guess_is_the_head() {
        let mut g = Graph::default();
        g.chain(&["M", "A", "B"]);
        let history = History {
            created_at: 100,
            head: "B".into(),
            ..Default::default()
        };
        assert_eq!(shas(&compute(&history, &g)), ["B"]);
    }

    #[test]
    fn a_push_that_is_not_a_fast_forward_counts_as_a_force_push() {
        let mut g = Graph::default();
        g.chain(&["M", "A", "B"]);
        g.chain(&["M", "X", "Y"]);
        let history = History {
            created_at: 100,
            head: "Y".into(),
            activities: vec![
                act(90, ActivityKind::Push, ZERO_SHA, "B"),
                // Recorded as a push, but its before isn't the last version.
                act(110, ActivityKind::Push, "A", "Y"),
            ],
            ..Default::default()
        };
        let c = compute(&history, &g);
        assert_eq!(shas(&c), ["B", "Y"]);
        assert_eq!(c.versions[1].kind, VersionKind::ForcePush);
    }

    #[test]
    fn pushes_after_the_log_are_split_per_commit() {
        let mut g = Graph::default();
        g.chain(&["M", "A", "B", "C"]);
        let history = History {
            created_at: 100,
            head: "C".into(),
            activities: vec![act(90, ActivityKind::BranchCreation, ZERO_SHA, "A")],
            ..Default::default()
        };
        assert_eq!(shas(&compute(&history, &g)), ["A", "B", "C"]);
    }

    #[test]
    fn a_missing_range_becomes_one_version() {
        let mut g = Graph::default();
        g.chain(&["M", "A", "B", "C"]);
        g.missing.insert("A".into());
        let history = History {
            created_at: 100,
            head: "C".into(),
            activities: vec![
                act(90, ActivityKind::BranchCreation, ZERO_SHA, "A"),
                act(110, ActivityKind::Push, "A", "C"),
            ],
            ..Default::default()
        };
        let c = compute(&history, &g);
        assert_eq!(shas(&c), ["A", "C"]);
        assert!(c.versions[0].missing);
        assert!(!c.versions[1].missing);
    }

    #[test]
    fn deleting_and_restoring_the_branch() {
        let mut g = Graph::default();
        g.chain(&["M", "A", "B"]);
        let history = History {
            created_at: 100,
            head: "B".into(),
            activities: vec![
                act(90, ActivityKind::BranchCreation, ZERO_SHA, "A"),
                act(110, ActivityKind::BranchDeletion, "A", ZERO_SHA),
                act(120, ActivityKind::BranchCreation, ZERO_SHA, "A"),
                act(130, ActivityKind::Push, "A", "B"),
            ],
            ..Default::default()
        };
        assert_eq!(shas(&compute(&history, &g)), ["A", "B"]);
    }

    #[test]
    fn pushes_before_the_pr_was_opened_are_not_versions() {
        let mut g = Graph::default();
        g.chain(&["M", "A", "B", "C"]);
        let history = History {
            created_at: 100,
            head: "C".into(),
            activities: vec![
                act(80, ActivityKind::BranchCreation, ZERO_SHA, "A"),
                act(90, ActivityKind::Push, "A", "B"),
                act(110, ActivityKind::Push, "B", "C"),
            ],
            ..Default::default()
        };
        assert_eq!(shas(&compute(&history, &g)), ["B", "C"]);
    }

    #[test]
    fn computing_twice_gives_the_same_versions() {
        let g = example_graph();
        let history = History {
            created_at: 100,
            head: "C8".into(),
            activities: vec![
                act(90, ActivityKind::BranchCreation, ZERO_SHA, "C2"),
                act(150, ActivityKind::ForcePush, "C2", "C8"),
            ],
            ..Default::default()
        };
        assert_eq!(compute(&history, &g), compute(&history, &g));
    }
}
