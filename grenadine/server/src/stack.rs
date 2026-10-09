//! Finds a PR's stack by chaining branches: a PR's parent is the PR whose
//! head branch it targets.

use std::collections::HashSet;

use anyhow::Result;
use fancy_regex::Regex;
use grenadine_core::api::{Stack, StackPr};

use crate::github::{RefField, RefPr};

/// How far the walk goes down towards the base branch.
pub const MAX_ANCESTORS: usize = 20;
/// How many PRs on top of the PR the walk collects.
pub const MAX_DESCENDANTS: usize = 50;
/// Which branches are trunks unless configured otherwise: everything but
/// `dev/` branches.
pub const DEFAULT_TRUNK: &str = "^(?!dev/)";

fn stackable(p: &RefPr) -> bool {
    p.pr.state == "OPEN" || p.pr.state == "MERGED"
}

/// Whether `branch` is a trunk, i.e. a long-lived branch that PRs merge
/// into rather than a PR's head. A trunk ends the walk in both directions:
/// any PR from a trunk into another branch (e.g. syncing a dev branch with
/// `main`) would otherwise become the parent of every PR into the trunk. A
/// branch the regex can't decide on, e.g. because it hits the backtrack
/// limit, counts as a trunk so that the walk stays short.
fn is_trunk(trunk: &Regex, branch: &str) -> bool {
    trunk.is_match(branch).unwrap_or(true)
}

/// The PR that a PR targeting the candidates' head branch sits on. Branch
/// names get reused, so an open PR wins over merged ones, and otherwise the
/// most recently merged one does. A head branch in a fork never counts:
/// the PR targets a branch of the base repository, which merely has the
/// same name.
fn pick_parent(candidates: Vec<RefPr>, visited: &HashSet<u64>) -> Option<RefPr> {
    candidates
        .into_iter()
        .filter(|c| stackable(c) && !c.cross_repo && !visited.contains(&c.pr.number))
        .max_by(|a, b| {
            let key = |c: &RefPr| (c.pr.state == "OPEN", c.merged_at.clone(), c.pr.number);
            key(a).cmp(&key(b))
        })
}

/// Walks down from `me` to the base branch and up to every PR stacked on
/// top of it. `head_ref` is `None` when `me`'s head branch lives in a
/// fork, where no PR can target it. `trunk` matches the branches that end
/// the walk. `lookup` returns, for each branch, the PRs whose head (or
/// base) branch it is.
pub async fn walk<F>(
    me: StackPr,
    base_ref: &str,
    head_ref: Option<&str>,
    trunk: &Regex,
    lookup: impl Fn(RefField, Vec<String>) -> F,
) -> Result<Stack>
where
    F: Future<Output = Result<Vec<Vec<RefPr>>>>,
{
    let mut visited = HashSet::from([me.number]);
    let mut prs = vec![me];
    let mut base = base_ref.to_owned();
    let mut more_ancestors = false;
    let mut child = 0;
    while !is_trunk(trunk, &base) {
        let found = lookup(RefField::Head, vec![base.clone()])
            .await?
            .pop()
            .unwrap_or_default();
        let Some(parent) = pick_parent(found, &visited) else {
            break;
        };
        if prs.len() > MAX_ANCESTORS {
            more_ancestors = true;
            break;
        }
        visited.insert(parent.pr.number);
        prs[child].parent = Some(parent.pr.number);
        base = parent.base_ref;
        prs.push(parent.pr);
        child = prs.len() - 1;
    }

    let mut more_descendants = false;
    let mut descendants = 0;
    let mut frontier: Vec<(u64, String)> = head_ref
        .filter(|h| !is_trunk(trunk, h))
        .map(|h| (prs[0].number, h.to_owned()))
        .into_iter()
        .collect();
    'walk: while !frontier.is_empty() {
        let refs = frontier.iter().map(|(_, h)| h.clone()).collect();
        let found = lookup(RefField::Base, refs).await?;
        let mut next = Vec::new();
        for ((parent, _), mut kids) in frontier.iter().zip(found) {
            kids.retain(stackable);
            kids.sort_by_key(|k| k.pr.number);
            for mut kid in kids {
                if !visited.insert(kid.pr.number) {
                    continue;
                }
                if descendants == MAX_DESCENDANTS {
                    more_descendants = true;
                    break 'walk;
                }
                descendants += 1;
                kid.pr.parent = Some(*parent);
                if !kid.cross_repo && !is_trunk(trunk, &kid.head_ref) {
                    next.push((kid.pr.number, kid.head_ref));
                }
                prs.push(kid.pr);
            }
        }
        frontier = next;
    }

    Ok(Stack {
        base_ref: base,
        prs,
        more_ancestors,
        more_descendants,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn me(number: u64) -> StackPr {
        StackPr {
            number,
            title: String::new(),
            state: "OPEN".into(),
            is_draft: false,
            url: String::new(),
            updated_at: String::new(),
            head_oid: String::new(),
            parent: None,
        }
    }

    /// A PR from `head` into `base`.
    fn ref_pr(number: u64, base: &str, head: &str) -> RefPr {
        RefPr {
            pr: me(number),
            base_ref: base.into(),
            head_ref: head.into(),
            cross_repo: false,
            merged_at: None,
        }
    }

    fn merged(mut p: RefPr, at: &str) -> RefPr {
        p.pr.state = "MERGED".into();
        p.merged_at = Some(at.into());
        p
    }

    /// Answers lookups from a fixed list of PRs, like GitHub would.
    struct Fake {
        prs: Vec<RefPr>,
        calls: RefCell<usize>,
    }

    impl Fake {
        fn new(prs: Vec<RefPr>) -> Fake {
            Fake {
                prs,
                calls: RefCell::new(0),
            }
        }

        async fn lookup(&self, field: RefField, refs: Vec<String>) -> Result<Vec<Vec<RefPr>>> {
            *self.calls.borrow_mut() += 1;
            Ok(refs
                .iter()
                .map(|r| {
                    self.prs
                        .iter()
                        .filter(|p| match field {
                            RefField::Head => &p.head_ref == r,
                            RefField::Base => &p.base_ref == r,
                        })
                        .cloned()
                        .collect()
                })
                .collect())
        }
    }

    fn walk_with(fake: &Fake, trunk: &str, number: u64, base: &str, head: Option<&str>) -> Stack {
        let trunk = Regex::new(trunk).unwrap();
        futures::executor::block_on(walk(me(number), base, head, &trunk, |f, r| {
            fake.lookup(f, r)
        }))
        .unwrap()
    }

    fn walk_from(fake: &Fake, number: u64, base: &str, head: Option<&str>) -> Stack {
        walk_with(fake, "^main$", number, base, head)
    }

    fn parents(s: &Stack) -> Vec<(u64, Option<u64>)> {
        s.prs.iter().map(|p| (p.number, p.parent)).collect()
    }

    #[test]
    fn finds_ancestors_and_descendants() {
        // main <- 1 (a) <- 2 (b) <- {3 (c) <- 5 (e), 4 (d)}; 6 (x) is a
        // sibling of 2 and stays out.
        let fake = Fake::new(vec![
            ref_pr(1, "main", "a"),
            ref_pr(2, "a", "b"),
            ref_pr(3, "b", "c"),
            ref_pr(4, "b", "d"),
            ref_pr(5, "c", "e"),
            ref_pr(6, "a", "x"),
        ]);
        let s = walk_from(&fake, 2, "a", Some("b"));
        assert_eq!(s.base_ref, "main");
        assert_eq!(
            parents(&s),
            [
                (2, Some(1)),
                (1, None),
                (3, Some(2)),
                (4, Some(2)),
                (5, Some(3))
            ]
        );
        assert!(!s.more_ancestors && !s.more_descendants);
    }

    #[test]
    fn a_lone_pr_is_its_own_stack() {
        let fake = Fake::new(vec![ref_pr(1, "main", "a")]);
        let s = walk_from(&fake, 1, "main", Some("a"));
        assert_eq!(parents(&s), [(1, None)]);
        assert_eq!(s.base_ref, "main");
    }

    #[test]
    fn fork_heads_are_never_parents() {
        let mut fork = ref_pr(1, "main", "a");
        fork.cross_repo = true;
        let fake = Fake::new(vec![fork]);
        let s = walk_from(&fake, 2, "a", Some("b"));
        assert_eq!(parents(&s), [(2, None)]);
        assert_eq!(s.base_ref, "a");
    }

    #[test]
    fn prs_from_a_trunk_are_never_parents() {
        // 1 merged main into a dev branch. 2 targets main and so sits on
        // nothing.
        let fake = Fake::new(vec![merged(
            ref_pr(1, "dev/update_yaml", "main"),
            "2026-01-01T00:00:00Z",
        )]);
        let s = walk_with(&fake, DEFAULT_TRUNK, 2, "main", Some("dev/feature"));
        assert_eq!(parents(&s), [(2, None)]);
        assert_eq!(s.base_ref, "main");
    }

    #[test]
    fn prs_from_a_trunk_have_no_descendants() {
        let fake = Fake::new(vec![ref_pr(2, "main", "dev/feature")]);
        let s = walk_with(&fake, DEFAULT_TRUNK, 1, "dev/update_yaml", Some("main"));
        assert_eq!(parents(&s), [(1, None)]);
        assert_eq!(s.base_ref, "dev/update_yaml");
    }

    #[test]
    fn descendants_from_a_trunk_are_not_expanded() {
        // 2 merges main into 1's branch. PRs into main aren't on top of 2.
        let fake = Fake::new(vec![ref_pr(2, "dev/a", "main"), ref_pr(3, "main", "dev/b")]);
        let s = walk_with(&fake, DEFAULT_TRUNK, 1, "main", Some("dev/a"));
        assert_eq!(parents(&s), [(1, None), (2, Some(1))]);
    }

    #[test]
    fn stacks_of_dev_branches_end_at_the_trunk() {
        let fake = Fake::new(vec![
            ref_pr(1, "release/1.0", "dev/a"),
            ref_pr(2, "dev/a", "dev/b"),
            ref_pr(3, "dev/b", "dev/c"),
        ]);
        let s = walk_with(&fake, DEFAULT_TRUNK, 2, "dev/a", Some("dev/b"));
        assert_eq!(parents(&s), [(2, Some(1)), (1, None), (3, Some(2))]);
        assert_eq!(s.base_ref, "release/1.0");
    }

    #[test]
    fn fork_prs_can_sit_on_top_but_have_no_children() {
        let mut fork = ref_pr(3, "b", "c");
        fork.cross_repo = true;
        let fake = Fake::new(vec![fork, ref_pr(4, "c", "d")]);
        let s = walk_from(&fake, 2, "main", Some("b"));
        assert_eq!(parents(&s), [(2, None), (3, Some(2))]);
    }

    #[test]
    fn a_fork_pr_has_no_descendants() {
        let fake = Fake::new(vec![ref_pr(3, "b", "c")]);
        let s = walk_from(&fake, 2, "main", None);
        assert_eq!(parents(&s), [(2, None)]);
    }

    #[test]
    fn open_parents_win_over_merged_ones() {
        let fake = Fake::new(vec![
            merged(ref_pr(1, "main", "a"), "2026-01-01T00:00:00Z"),
            ref_pr(5, "main", "a"),
            merged(ref_pr(3, "main", "a"), "2026-02-01T00:00:00Z"),
        ]);
        let s = walk_from(&fake, 9, "a", None);
        assert_eq!(s.prs[0].parent, Some(5));
    }

    #[test]
    fn the_newest_merged_parent_wins() {
        let fake = Fake::new(vec![
            merged(ref_pr(1, "main", "a"), "2026-03-01T00:00:00Z"),
            merged(ref_pr(3, "main", "a"), "2026-02-01T00:00:00Z"),
        ]);
        let s = walk_from(&fake, 9, "a", None);
        assert_eq!(s.prs[0].parent, Some(1));
    }

    #[test]
    fn closed_prs_are_left_out() {
        let mut closed = ref_pr(3, "b", "c");
        closed.pr.state = "CLOSED".into();
        let fake = Fake::new(vec![closed]);
        let s = walk_from(&fake, 2, "main", Some("b"));
        assert_eq!(parents(&s), [(2, None)]);
    }

    #[test]
    fn cycles_end_the_walk() {
        // Retargeting can make two PRs target each other's branches.
        let fake = Fake::new(vec![ref_pr(1, "b", "a"), ref_pr(2, "a", "b")]);
        let s = walk_from(&fake, 2, "a", Some("b"));
        assert_eq!(parents(&s), [(2, Some(1)), (1, None)]);
    }

    #[test]
    fn long_stacks_are_capped() {
        let n = (MAX_ANCESTORS + MAX_DESCENDANTS + 10) as u64;
        let branch = |i: u64| {
            if i == 0 {
                "main".to_owned()
            } else {
                format!("b{i}")
            }
        };
        let fake = Fake::new(
            (1..=n)
                .map(|i| ref_pr(i, &branch(i - 1), &branch(i)))
                .collect(),
        );
        let mid = MAX_ANCESTORS as u64 + 5;
        let s = walk_from(&fake, mid, &branch(mid - 1), Some(&branch(mid)));
        assert!(s.more_ancestors && s.more_descendants);
        assert_eq!(s.prs.len(), 1 + MAX_ANCESTORS + MAX_DESCENDANTS);
    }

    #[test]
    fn children_of_one_level_share_a_lookup() {
        let fake = Fake::new(vec![
            ref_pr(3, "b", "c"),
            ref_pr(4, "b", "d"),
            ref_pr(5, "c", "e"),
            ref_pr(6, "d", "f"),
        ]);
        walk_from(&fake, 2, "main", Some("b"));
        // None for the trunk, then one per level: [b], [c, d], [e, f].
        assert_eq!(*fake.calls.borrow(), 3);
    }
}
