//! Plans which of the session's commits go into which pull requests when
//! `update_prs` lands them: one PR per commit, either an open one of the
//! session's or a new one. Without a mapping the commits keep the open
//! PRs in order; the `prs` argument maps each commit to a PR by SHA, so
//! commits can be added, dropped and reordered anywhere.

use anyhow::{Result, bail};
use grenadine_core::api::LandedPr;
use serde::Deserialize;

use crate::agent::stack_branch;
use crate::github::GhStack;

/// One entry of `update_prs`'s `prs` argument: the PR `commit` updates,
/// or a new one when `pr` is omitted.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PrSpec {
    /// A full or abbreviated SHA of one of the session's commits.
    pub commit: String,
    #[serde(default)]
    pub pr: Option<u64>,
    /// The branch a new pull request's commit is pushed to; only with
    /// `pr` omitted.
    #[serde(default)]
    pub branch: Option<String>,
}

/// One commit of the stack and the PR it goes into: an open one of the
/// session's, or a new one (`pr: None`) on `branch`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    pub sha: String,
    pub message: String,
    pub branch: String,
    pub pr: Option<LandedPr>,
}

/// Matches `commits` (sha, message; oldest first) to the session's `open`
/// PRs (bottom first). `taken` is the branch of every PR the session ever
/// landed, open or not: a new PR must not take one, since the branch may
/// still serve another PR. `closed` is the landed PRs the user closed.
/// Returns the plan, bottom first, and the open PRs it drops.
pub fn plan_prs(
    session_branch: &str,
    commits: &[(String, String)],
    open: &[LandedPr],
    taken: &[String],
    closed: &[u64],
    specs: Option<&[PrSpec]>,
) -> Result<(Vec<Planned>, Vec<LandedPr>)> {
    let pr_of = |specs: Option<&[PrSpec]>, i: usize| -> Result<Option<LandedPr>> {
        match specs {
            None => Ok(open.get(i).cloned()),
            Some(specs) => match specs[i].pr {
                None => Ok(None),
                Some(number) => match open.iter().find(|pr| pr.number == number) {
                    Some(pr) if specs[..i].iter().all(|s| s.pr != Some(number)) => {
                        Ok(Some(pr.clone()))
                    }
                    Some(_) => {
                        bail!("#{number} is already the pull request of another commit")
                    }
                    None => bail!("#{number} isn't one of this session's open pull requests"),
                },
            },
        }
    };
    let specs = match specs {
        None => {
            // Without a mapping a closed PR's commit can't be told from
            // the ones after it.
            if !closed.is_empty() {
                let numbers = closed
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "the session's pull requests {numbers} were closed, so its commits can't \
                     be mapped by position; call `update_prs` again with `prs` to choose \
                     which commit goes into which pull request"
                );
            }
            if commits.len() < open.len() {
                bail!(
                    "the session has {} commits but opened {} pull requests, one per commit; \
                     keep one commit per pull request, and amend a pull request's commit to \
                     change it rather than squashing commits together, or pass `prs` to drop \
                     or reorder pull requests",
                    commits.len(),
                    open.len()
                );
            }
            None
        }
        Some(specs) => {
            let expected = commits
                .iter()
                .map(|(sha, message)| {
                    format!(
                        "- {} {}",
                        &sha[..sha.len().min(12)],
                        message.lines().next().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            if specs.len() != commits.len() {
                bail!(
                    "`prs` must have one entry per commit, bottom first; the commits are:\n{expected}"
                );
            }
            for (i, spec) in specs.iter().enumerate() {
                let hex =
                    spec.commit.len() >= 7 && spec.commit.chars().all(|c| c.is_ascii_hexdigit());
                if !hex || !commits[i].0.starts_with(&spec.commit) {
                    bail!(
                        "`{}` doesn't name commit {} of the stack, bottom first; the commits \
                         are:\n{expected}",
                        spec.commit,
                        i + 1
                    );
                }
                if let (Some(number), Some(_)) = (spec.pr, &spec.branch) {
                    bail!(
                        "a pull request's branch can't change; drop `branch` from the entry \
                         for #{number}"
                    );
                }
            }
            Some(specs)
        }
    };
    let mut dropped: Vec<bool> = vec![true; open.len()];
    let mut planned = Vec::with_capacity(commits.len());
    let mut branches: Vec<String> = Vec::new();
    for (i, (sha, message)) in commits.iter().enumerate() {
        let pr = pr_of(specs, i)?;
        let branch = match &pr {
            Some(pr) => {
                dropped[open.iter().position(|o| o.number == pr.number).unwrap()] = false;
                pr.branch.clone()
            }
            None => match specs.and_then(|s| s[i].branch.clone()) {
                // A branch the agent picked: it must be new, like a
                // generated one.
                Some(branch) => {
                    if branches.contains(&branch) {
                        bail!("two pull requests can't share the branch `{branch}`");
                    }
                    // Any branch a PR of the session ever used may still
                    // be that PR's head.
                    if taken.contains(&branch) {
                        bail!(
                            "a pull request of this session already uses the branch `{branch}`; \
                             pick another one"
                        );
                    }
                    branch
                }
                // A fresh branch: any branch a PR of the session ever used
                // may still be that PR's head.
                None => (0..)
                    .map(|k| stack_branch(session_branch, k))
                    .find(|b| !taken.contains(b) && !branches.contains(b))
                    .unwrap(),
            },
        };
        branches.push(branch.clone());
        planned.push(Planned {
            sha: sha.clone(),
            message: message.clone(),
            branch,
            pr,
        });
    }
    let dropped = open
        .iter()
        .zip(dropped)
        .filter(|(_, dropped)| *dropped)
        .map(|(pr, _)| pr.clone())
        .collect();
    Ok((planned, dropped))
}

/// What to do with the GitHub stack a session's PRs make after landing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StackOp {
    /// Nothing changes.
    Keep,
    /// The PRs get a new stack, bottom first.
    Create(Vec<u64>),
    /// `prs` go on top of `stack`.
    Add { stack: u64, prs: Vec<u64> },
    /// `stack` (whose open PRs are `old`, bottom first) comes apart and
    /// `new` becomes a stack if it has 2 or more PRs.
    Rebuild {
        stack: u64,
        old: Vec<u64>,
        new: Vec<u64>,
    },
}

/// Plans the GitHub stack work after landing. `ours` is the session's
/// PRs after this landing, bottom first; `session_prs` every PR number
/// the session has had; `source` the PR the session started from;
/// `ours_stack`/`source_stack` the stack a session or source PR is in;
/// `retargeted` the kept PRs whose base moves, which GitHub may refuse
/// for a stacked PR, so theirs comes apart first.
pub fn plan_stack(
    ours: &[u64],
    session_prs: &[u64],
    source: Option<u64>,
    ours_stack: Option<&GhStack>,
    source_stack: Option<&GhStack>,
    retargeted: &[u64],
) -> StackOp {
    // The stack the session's PRs join, G, and what it should end up
    // holding, F, bottom first. PRs that aren't the session's are left
    // alone, so foreign members stay in place.
    let (g, f): (Option<&GhStack>, Vec<u64>) = match ours_stack {
        Some(g) => (
            Some(g),
            [
                g.open
                    .iter()
                    .filter(|n| !session_prs.contains(n))
                    .copied()
                    .collect(),
                ours.to_vec(),
            ]
            .concat(),
        ),
        None => match source {
            Some(s) => match source_stack {
                Some(ss) if ss.open.last() == Some(&s) => (
                    Some(ss),
                    [
                        ss.open
                            .iter()
                            .filter(|n| !session_prs.contains(n))
                            .copied()
                            .collect(),
                        ours.to_vec(),
                    ]
                    .concat(),
                ),
                // The source sits in the middle of a stack; only the
                // session's own PRs can be stacked on its branch.
                Some(_) => (None, ours.to_vec()),
                None => (None, [vec![s], ours.to_vec()].concat()),
            },
            None => (None, ours.to_vec()),
        },
    };
    let c: &[u64] = g.map(|g| g.open.as_slice()).unwrap_or(&[]);
    // GitHub may refuse to move a stacked PR's base, so a stack with one
    // comes apart and is rebuilt.
    if !c.is_empty() && retargeted.iter().any(|n| c.contains(n)) {
        return StackOp::Rebuild {
            stack: g.unwrap().number,
            old: c.to_vec(),
            new: f,
        };
    }
    if c.is_empty() {
        return match f.len() >= 2 {
            true => StackOp::Create(f),
            false => StackOp::Keep,
        };
    }
    if f.starts_with(c) {
        return match f[c.len()..].is_empty() {
            true => StackOp::Keep,
            false => StackOp::Add {
                stack: g.unwrap().number,
                prs: f[c.len()..].to_vec(),
            },
        };
    }
    StackOp::Rebuild {
        stack: g.unwrap().number,
        old: c.to_vec(),
        new: f,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` commits named c1..cn with hex SHAs, oldest first.
    fn commits(n: usize) -> Vec<(String, String)> {
        (1..=n)
            .map(|i| (format!("{i:040x}"), format!("Add c{i}\n\nBody of c{i}.")))
            .collect()
    }

    fn pr(number: u64, branch: &str) -> LandedPr {
        LandedPr {
            number,
            url: format!("https://github.com/o/n/pull/{number}"),
            branch: branch.into(),
        }
    }

    fn spec(sha: &str, pr: Option<u64>) -> PrSpec {
        PrSpec {
            commit: sha.into(),
            pr,
            branch: None,
        }
    }

    fn spec_branch(sha: &str, branch: &str) -> PrSpec {
        PrSpec {
            branch: Some(branch.into()),
            ..spec(sha, None)
        }
    }

    /// The landed branches `open` implies.
    fn taken(open: &[LandedPr]) -> Vec<String> {
        open.iter().map(|pr| pr.branch.clone()).collect()
    }

    fn plan(
        result: Result<(Vec<Planned>, Vec<LandedPr>)>,
    ) -> (Vec<(String, Option<u64>)>, Vec<u64>) {
        let (planned, dropped) = result.unwrap();
        (
            planned
                .into_iter()
                .map(|p| (p.branch, p.pr.map(|pr| pr.number)))
                .collect(),
            dropped.into_iter().map(|pr| pr.number).collect(),
        )
    }

    /// Without `prs`, commits keep the open PRs in order and new commits
    /// get new PRs on the next stack branches.
    #[test]
    fn positional_append() {
        let commits = commits(3);
        let open = [pr(24, "b"), pr(25, "b-2")];
        assert_eq!(
            plan(plan_prs("b", &commits, &open, &taken(&open), &[], None)),
            (
                vec![
                    ("b".into(), Some(24)),
                    ("b-2".into(), Some(25)),
                    ("b-3".into(), None),
                ],
                vec![]
            )
        );
    }

    /// Fewer commits than open PRs is an error that points at `prs`.
    #[test]
    fn positional_too_few() {
        let commits = commits(1);
        let open = [pr(24, "b"), pr(25, "b-2")];
        let e = plan_prs("b", &commits, &open, &taken(&open), &[], None).unwrap_err();
        let text = format!("{e:#}");
        assert!(
            text.contains("1 commits but opened 2 pull requests"),
            "{text}"
        );
        assert!(text.contains("pass `prs`"), "{text}");
    }

    /// A mapping reorders the commits' PRs; each keeps its branch.
    #[test]
    fn reorder() {
        let commits = commits(2);
        let open = [pr(24, "b"), pr(25, "b-2")];
        let specs = [spec(&commits[1].0, Some(25)), spec(&commits[0].0, Some(24))];
        // The SHAs are out of order, so this fails before reordering: swap
        // the commits instead.
        assert!(plan_prs("b", &commits, &open, &taken(&open), &[], Some(&specs)).is_err());
        let reversed: Vec<_> = commits.iter().rev().cloned().collect();
        assert_eq!(
            plan(plan_prs(
                "b",
                &reversed,
                &open,
                &taken(&open),
                &[],
                Some(&specs)
            )),
            (
                vec![("b-2".into(), Some(25)), ("b".into(), Some(24))],
                vec![]
            )
        );
    }

    /// A PR no commit names is dropped; the ones kept stay on their
    /// branches.
    #[test]
    fn drop_middle_and_top() {
        let commits = commits(3);
        let open = [pr(24, "b"), pr(25, "b-2"), pr(26, "b-3")];
        let specs = [
            spec(&commits[0].0, Some(24)),
            spec(&commits[1].0, None),
            spec(&commits[2].0, Some(26)),
        ];
        assert_eq!(
            plan(plan_prs(
                "b",
                &commits,
                &open,
                &taken(&open),
                &[],
                Some(&specs)
            )),
            (
                vec![
                    ("b".into(), Some(24)),
                    ("b-4".into(), None),
                    ("b-3".into(), Some(26)),
                ],
                vec![25]
            )
        );

        let specs = [
            spec(&commits[0].0, Some(24)),
            spec(&commits[1].0, Some(25)),
            spec(&commits[2].0, None),
        ];
        assert_eq!(
            plan(plan_prs(
                "b",
                &commits,
                &open,
                &taken(&open),
                &[],
                Some(&specs)
            )),
            (
                vec![
                    ("b".into(), Some(24)),
                    ("b-2".into(), Some(25)),
                    ("b-4".into(), None),
                ],
                vec![26]
            )
        );
    }

    /// A branch a PR the session landed ever used stays taken, even by a
    /// merged or closed PR, so new PRs get a fresh name.
    #[test]
    fn new_prs_avoid_old_branches() {
        let commits = commits(2);
        // #24 merged on "b", #25 closed on "b-2", #26 is the only open
        // one, on "b-3".
        let open = [pr(26, "b-3")];
        let taken = vec!["b".to_owned(), "b-2".to_owned(), "b-3".to_owned()];
        let specs = [spec(&commits[0].0, Some(26)), spec(&commits[1].0, None)];
        assert_eq!(
            plan(plan_prs("b", &commits, &open, &taken, &[], Some(&specs))),
            (vec![("b-3".into(), Some(26)), ("b-4".into(), None)], vec![])
        );
    }

    /// Without a mapping, a closed PR leaves the commits' positions
    /// ambiguous, so the call has to name them.
    #[test]
    fn positional_with_a_closed_pr() {
        let commits = commits(3);
        let open = [pr(24, "b"), pr(26, "b-3")];
        let e = plan_prs("b", &commits, &open, &taken(&open), &[25], None).unwrap_err();
        let text = format!("{e:#}");
        assert!(text.contains("#25"), "{text}");
        assert!(text.contains("`prs`"), "{text}");
    }

    /// A new commit between kept PRs gets a branch name none of them has.
    #[test]
    fn insert_new_in_middle() {
        let commits = commits(3);
        let open = [pr(24, "b"), pr(26, "b-3")];
        let specs = [
            spec(&commits[0].0, Some(24)),
            spec(&commits[1].0, None),
            spec(&commits[2].0, Some(26)),
        ];
        assert_eq!(
            plan(plan_prs(
                "b",
                &commits,
                &open,
                &taken(&open),
                &[],
                Some(&specs)
            )),
            (
                vec![
                    ("b".into(), Some(24)),
                    ("b-2".into(), None),
                    ("b-3".into(), Some(26)),
                ],
                vec![]
            )
        );
    }

    /// Mapping errors name the expected commits; a wrong PR or a
    /// repeated one names it.
    #[test]
    fn mapping_errors() {
        let commits = commits(2);
        let open = [pr(24, "b"), pr(25, "b-2")];

        let e = plan_prs(
            "b",
            &commits,
            &open,
            &taken(&open),
            &[],
            Some(&[spec(&commits[0].0, None)]),
        )
        .unwrap_err();
        let text = format!("{e:#}");
        assert!(text.contains("one entry per commit"), "{text}");
        assert!(text.contains("Add c1"), "{text}");

        // Wrong order: the second commit's SHA in the first entry.
        let specs = [spec(&commits[1].0, Some(24)), spec(&commits[0].0, Some(25))];
        let e = plan_prs("b", &commits, &open, &taken(&open), &[], Some(&specs)).unwrap_err();
        assert!(format!("{e:#}").contains("doesn't name commit 1"), "{e:#}");

        // Not a hex SHA at all.
        let specs = [spec("xyz", None), spec(&commits[1].0, None)];
        assert!(plan_prs("b", &commits, &open, &taken(&open), &[], Some(&specs)).is_err());

        let specs = [spec(&commits[0].0, Some(99)), spec(&commits[1].0, None)];
        let e = plan_prs("b", &commits, &open, &taken(&open), &[], Some(&specs)).unwrap_err();
        assert!(
            format!("{e:#}").contains("#99 isn't one of this session's open pull requests"),
            "{e:#}"
        );

        let specs = [spec(&commits[0].0, Some(24)), spec(&commits[1].0, Some(24))];
        let e = plan_prs("b", &commits, &open, &taken(&open), &[], Some(&specs)).unwrap_err();
        assert!(format!("{e:#}").contains("#24"), "{e:#}");
    }

    /// A new PR's `branch` names the branch its commit is pushed to;
    /// without it the stack's next branch name is generated.
    #[test]
    fn custom_branches() {
        let commits = commits(3);
        let open = [pr(24, "b")];
        let specs = [
            spec(&commits[0].0, Some(24)),
            spec_branch(&commits[1].0, "dev/me/topic"),
            spec(&commits[2].0, None),
        ];
        assert_eq!(
            plan(plan_prs(
                "b",
                &commits,
                &open,
                &taken(&open),
                &[],
                Some(&specs)
            )),
            (
                vec![
                    ("b".into(), Some(24)),
                    ("dev/me/topic".into(), None),
                    ("b-2".into(), None),
                ],
                vec![]
            )
        );
    }

    /// `branch` only goes with a new PR, since an open one's branch is
    /// already set.
    #[test]
    fn branch_with_a_pr_errors() {
        let commits = commits(2);
        let open = [pr(24, "b"), pr(25, "b-2")];
        let specs = [
            PrSpec {
                branch: Some("dev/me/topic".into()),
                ..spec(&commits[0].0, Some(24))
            },
            spec(&commits[1].0, Some(25)),
        ];
        let e = plan_prs("b", &commits, &open, &taken(&open), &[], Some(&specs)).unwrap_err();
        let text = format!("{e:#}");
        assert!(text.contains("branch can't change"), "{text}");
        assert!(text.contains("#24"), "{text}");
    }

    /// Two new PRs can't get the same branch, whether two entries name
    /// it or one names a branch the plan already generated.
    #[test]
    fn duplicate_branches_error() {
        let commits = commits(2);
        let specs = [
            spec_branch(&commits[0].0, "dev/me/topic"),
            spec_branch(&commits[1].0, "dev/me/topic"),
        ];
        let e = plan_prs("b", &commits, &[], &[], &[], Some(&specs)).unwrap_err();
        let text = format!("{e:#}");
        assert!(
            text.contains("can't share the branch `dev/me/topic`"),
            "{text}"
        );

        let specs = [spec(&commits[0].0, None), spec_branch(&commits[1].0, "b")];
        let e = plan_prs("b", &commits, &[], &[], &[], Some(&specs)).unwrap_err();
        assert!(
            format!("{e:#}").contains("can't share the branch `b`"),
            "{e:#}"
        );
    }

    /// A branch a PR of the session already used can't name a new one.
    #[test]
    fn taken_branch_errors() {
        let commits = commits(1);
        let open = [pr(24, "b")];
        let specs = [spec_branch(&commits[0].0, "b")];
        let e = plan_prs("b", &commits, &open, &taken(&open), &[], Some(&specs)).unwrap_err();
        let text = format!("{e:#}");
        assert!(text.contains("already uses the branch `b`"), "{text}");
    }

    /// An abbreviated SHA names its commit too.
    #[test]
    fn abbreviated_shas() {
        let commits = commits(2);
        let specs = [
            spec(&commits[0].0[..7], None),
            spec(&commits[1].0[..12], Some(25)),
        ];
        let open = [pr(25, "b-2")];
        assert_eq!(
            plan(plan_prs(
                "b",
                &commits,
                &open,
                &taken(&open),
                &[],
                Some(&specs)
            )),
            (vec![("b".into(), None), ("b-2".into(), Some(25))], vec![])
        );
    }

    fn stack(number: u64, open: &[u64]) -> GhStack {
        GhStack {
            number,
            open: open.to_vec(),
        }
    }

    /// One PR never makes a stack; two make one.
    #[test]
    fn stack_created_at_two_prs() {
        assert_eq!(plan_stack(&[7], &[7], None, None, None, &[]), StackOp::Keep);
        assert_eq!(
            plan_stack(&[7, 8], &[7, 8], None, None, None, &[]),
            StackOp::Create(vec![7, 8])
        );
    }

    /// Unchanged PRs keep their stack; new ones append to it.
    #[test]
    fn stack_appends() {
        let g = stack(3, &[7, 8]);
        assert_eq!(
            plan_stack(&[7, 8], &[7, 8], None, Some(&g), None, &[]),
            StackOp::Keep
        );
        assert_eq!(
            plan_stack(&[7, 8, 9], &[7, 8, 9], None, Some(&g), None, &[]),
            StackOp::Add {
                stack: 3,
                prs: vec![9]
            }
        );
    }

    /// Dropping a middle PR or reordering rebuilds the stack.
    #[test]
    fn stack_rebuilds() {
        let g = stack(3, &[7, 8, 9]);
        assert_eq!(
            plan_stack(&[7, 9], &[7, 8, 9], None, Some(&g), None, &[]),
            StackOp::Rebuild {
                stack: 3,
                old: vec![7, 8, 9],
                new: vec![7, 9]
            }
        );
        assert_eq!(
            plan_stack(&[8, 7, 9], &[7, 8, 9], None, Some(&g), None, &[]),
            StackOp::Rebuild {
                stack: 3,
                old: vec![7, 8, 9],
                new: vec![8, 7, 9]
            }
        );
        assert_eq!(
            plan_stack(&[9], &[7, 8, 9], None, Some(&g), None, &[]),
            StackOp::Rebuild {
                stack: 3,
                old: vec![7, 8, 9],
                new: vec![9]
            }
        );
    }

    /// A session starting from the top of a foreign stack joins it; its
    /// members the session doesn't own stay.
    #[test]
    fn source_stack() {
        let ss = stack(3, &[4, 5]);
        // The source #5 tops [4, 5]: our PRs join it.
        assert_eq!(
            plan_stack(&[7], &[7], Some(5), None, Some(&ss), &[]),
            StackOp::Add {
                stack: 3,
                prs: vec![7]
            }
        );
        // #5 in the middle of [4, 5, 6]: ours get their own stack.
        let mid = stack(3, &[4, 5, 6]);
        assert_eq!(
            plan_stack(&[7, 8], &[7, 8], Some(5), None, Some(&mid), &[]),
            StackOp::Create(vec![7, 8])
        );
        assert_eq!(
            plan_stack(&[7], &[7], Some(5), None, Some(&mid), &[]),
            StackOp::Keep
        );
        // #5 in no stack: [5, ours].
        assert_eq!(
            plan_stack(&[7], &[7], Some(5), None, None, &[]),
            StackOp::Create(vec![5, 7])
        );
    }

    /// A rebuild keeps the foreign PRs and drops the session's; our own
    /// stack wins over the source's.
    #[test]
    fn stack_shared_rebuild() {
        let g = stack(3, &[4, 5, 7, 8]);
        // #8 was the session's and is dropped; #4 and #5 are foreign.
        assert_eq!(
            plan_stack(&[7], &[7, 8], None, Some(&g), None, &[]),
            StackOp::Rebuild {
                stack: 3,
                old: vec![4, 5, 7, 8],
                new: vec![4, 5, 7]
            }
        );
        let ss = stack(9, &[5]);
        assert_eq!(
            plan_stack(&[7], &[7, 8], Some(5), Some(&g), Some(&ss), &[]),
            StackOp::Rebuild {
                stack: 3,
                old: vec![4, 5, 7, 8],
                new: vec![4, 5, 7]
            }
        );
    }
}
