//! Plans which of the session's commits go into which pull requests when
//! `update_prs` lands them: one PR per commit, either an open one of the
//! session's or a new one. Without a mapping the commits keep the open
//! PRs in order; the `prs` argument maps each commit to a PR by SHA, so
//! commits can be added, dropped and reordered anywhere.

use anyhow::{Result, bail};
use grenadine_core::api::LandedPr;
use serde::Deserialize;

use crate::agent::stack_branch;

/// One entry of `update_prs`'s `prs` argument: the PR `commit` updates,
/// or a new one when `pr` is omitted.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PrSpec {
    /// A full or abbreviated SHA of one of the session's commits.
    pub commit: String,
    #[serde(default)]
    pub pr: Option<u64>,
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
            // A fresh branch: any branch a PR of the session ever used
            // may still be that PR's head.
            None => (0..)
                .map(|k| stack_branch(session_branch, k))
                .find(|b| !taken.contains(b) && !branches.contains(b))
                .unwrap(),
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
}
