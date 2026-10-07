//! Stacked PRs: a PR's place in its stack, the graph the PR page draws, and
//! the grouping of stack-mates in an inbox.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::api::{PrSummary, Stack, StackMember, StackSummary};

impl Stack {
    /// The PRs whose parent is `parent`, by number. PRs whose parent isn't
    /// part of the stack count as children of the base branch (`None`).
    fn children(&self) -> BTreeMap<Option<u64>, Vec<usize>> {
        let numbers: HashSet<u64> = self.prs.iter().map(|p| p.number).collect();
        let mut kids: BTreeMap<Option<u64>, Vec<usize>> = BTreeMap::new();
        for (i, p) in self.prs.iter().enumerate() {
            let parent = p.parent.filter(|n| numbers.contains(n));
            kids.entry(parent).or_default().push(i);
        }
        for list in kids.values_mut() {
            list.sort_by_key(|&i| self.prs[i].number);
        }
        kids
    }

    /// Where `number` sits in the stack. `None` when the stack has no
    /// other PR or doesn't contain `number`.
    pub fn summary(&self, number: u64) -> Option<StackSummary> {
        if self.prs.len() < 2 {
            return None;
        }
        let by_number: HashMap<u64, usize> = self
            .prs
            .iter()
            .enumerate()
            .map(|(i, p)| (p.number, i))
            .collect();
        let me = *by_number.get(&number)?;
        let open = |i: usize| u32::from(self.prs[i].state == "OPEN");

        let mut below = 0;
        let mut seen = HashSet::from([me]);
        let mut at = self.prs[me].parent;
        while let Some(i) = at.and_then(|n| by_number.get(&n).copied()) {
            if !seen.insert(i) {
                break;
            }
            below += open(i);
            at = self.prs[i].parent;
        }

        let kids = self.children();
        // The open PRs on the longest path up from `i`, `i` included.
        fn up(i: usize, kids: &BTreeMap<Option<u64>, Vec<usize>>, s: &Stack, depth: usize) -> u32 {
            let own = u32::from(s.prs[i].state == "OPEN");
            if depth > s.prs.len() {
                return own;
            }
            own + kids
                .get(&Some(s.prs[i].number))
                .into_iter()
                .flatten()
                .map(|&c| up(c, kids, s, depth + 1))
                .max()
                .unwrap_or(0)
        }

        Some(StackSummary {
            position: if open(me) == 1 { below + 1 } else { 0 },
            length: below + up(me, &kids, self, 0),
            members: self
                .prs
                .iter()
                .map(|p| StackMember {
                    number: p.number,
                    parent: p.parent,
                })
                .collect(),
        })
    }

    /// The rows of the stack's graph, top of the stack first, ending with
    /// the base branch. Like `git log --graph`, the first child of a PR
    /// continues its parent's lane and every further child gets a lane to
    /// the right that joins the parent's lane at the parent's row.
    pub fn graph(&self) -> Vec<GraphRow> {
        let kids = self.children();
        let mut rows: Vec<GraphRow> = Vec::new();
        let mut edges: Vec<(usize, usize)> = Vec::new();
        self.place(None, 0, 1, &kids, &mut rows, &mut edges);
        for (child, parent) in edges {
            let lane = rows[child].lane;
            rows[child].down = true;
            for row in &mut rows[child + 1..parent] {
                row.through.push(lane);
            }
            if lane == rows[parent].lane {
                rows[parent].up = true;
            } else {
                rows[parent].joins.push(lane);
            }
        }
        rows
    }

    /// Appends the rows of `node`'s subtree and returns `node`'s row.
    /// `free` is the lowest lane that nothing outside the subtree uses
    /// while the subtree's rows are drawn.
    fn place(
        &self,
        node: Option<usize>,
        lane: usize,
        free: usize,
        kids: &BTreeMap<Option<u64>, Vec<usize>>,
        rows: &mut Vec<GraphRow>,
        edges: &mut Vec<(usize, usize)>,
    ) -> usize {
        let number = node.map(|i| self.prs[i].number);
        let mut child_rows = Vec::new();
        for (k, &c) in kids.get(&number).into_iter().flatten().enumerate() {
            let (l, f) = if k == 0 {
                (lane, free)
            } else {
                (free + k - 1, free + k)
            };
            child_rows.push(self.place(Some(c), l, f, kids, rows, edges));
        }
        rows.push(GraphRow {
            pr: node,
            lane,
            up: false,
            down: false,
            through: Vec::new(),
            joins: Vec::new(),
        });
        let me = rows.len() - 1;
        edges.extend(child_rows.into_iter().map(|c| (c, me)));
        me
    }
}

/// One row of a stack's graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRow {
    /// Index into `Stack::prs`; `None` for the base branch.
    pub pr: Option<usize>,
    /// The lane the row's node sits in. Lanes count from the left.
    pub lane: usize,
    /// A line leaves the node upwards, to a child in the same lane.
    pub up: bool,
    /// A line leaves the node downwards, to its parent.
    pub down: bool,
    /// Lanes whose line passes through this row.
    pub through: Vec<usize>,
    /// Lanes whose line ends at this row by joining the node from the right.
    pub joins: Vec<usize>,
}

/// How a row of an inbox connects to the row below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// The PR below is this PR's parent or sibling.
    Direct,
    /// Stack-mates sit between the two PRs that this inbox doesn't list.
    Gap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxRow {
    /// Index into the inbox's PRs.
    pub index: usize,
    /// `None` when the next row isn't part of the same stack.
    pub link: Option<Link>,
}

/// Orders an inbox's PRs so that stack-mates sit together, top of the
/// stack first, where the highest-ranked of them is. Two synced PRs are
/// stack-mates when either one's stack lists the other, so that one stale
/// stack doesn't split them.
pub fn group_inbox(prs: &[PrSummary]) -> Vec<InboxRow> {
    let index: HashMap<(&str, u64), usize> = prs
        .iter()
        .enumerate()
        .filter(|(_, p)| p.synced)
        .map(|(i, p)| ((p.key.repo.as_str(), p.key.number), i))
        .collect();
    let mut group: Vec<usize> = (0..prs.len()).collect();
    fn root(group: &mut [usize], mut i: usize) -> usize {
        while group[i] != i {
            group[i] = group[group[i]];
            i = group[i];
        }
        i
    }
    // The parent of every PR that some stack mentions. A PR's own stack
    // has the final say about its own parent.
    let mut parent: HashMap<(&str, u64), Option<u64>> = HashMap::new();
    for (i, p) in prs.iter().enumerate() {
        let Some(stack) = p.stack.as_ref().filter(|_| p.synced) else {
            continue;
        };
        let repo = p.key.repo.as_str();
        for m in &stack.members {
            if m.number == p.key.number {
                parent.insert((repo, m.number), m.parent);
            } else {
                parent.entry((repo, m.number)).or_insert(m.parent);
            }
            if let Some(&j) = index.get(&(repo, m.number)) {
                let (a, b) = (root(&mut group, i), root(&mut group, j));
                group[a.max(b)] = a.min(b);
            }
        }
    }
    let parent_of = |i: usize| {
        parent
            .get(&(prs[i].key.repo.as_str(), prs[i].key.number))
            .copied()
            .flatten()
    };
    let depth = |i: usize| {
        let repo = prs[i].key.repo.as_str();
        let mut seen = HashSet::from([prs[i].key.number]);
        let mut at = parent_of(i);
        let mut d = 0;
        while let Some(n) = at.filter(|&n| seen.insert(n)) {
            d += 1;
            at = parent.get(&(repo, n)).copied().flatten();
        }
        d
    };

    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..prs.len() {
        members.entry(root(&mut group, i)).or_default().push(i);
    }
    let mut out = Vec::with_capacity(prs.len());
    for mut list in members.into_values() {
        list.sort_by_key(|&i| (std::cmp::Reverse(depth(i)), i));
        for (k, &i) in list.iter().enumerate() {
            let link = list.get(k + 1).map(|&next| {
                let (mine, theirs) = (parent_of(i), parent_of(next));
                if mine == Some(prs[next].key.number) || (mine.is_some() && mine == theirs) {
                    Link::Direct
                } else {
                    Link::Gap
                }
            });
            out.push(InboxRow { index: i, link });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{PrKey, StackPr};

    fn pr(number: u64, parent: Option<u64>, state: &str) -> StackPr {
        StackPr {
            number,
            title: format!("pr {number}"),
            state: state.into(),
            is_draft: false,
            url: String::new(),
            updated_at: String::new(),
            head_oid: String::new(),
            parent,
        }
    }

    fn stack(prs: Vec<StackPr>) -> Stack {
        Stack {
            base_ref: "main".into(),
            prs,
            more_ancestors: false,
            more_descendants: false,
        }
    }

    /// main <- A(1) <- B(2) <- {C1(3) <- D(5), C2(4)}
    fn forked() -> Stack {
        stack(vec![
            pr(1, None, "OPEN"),
            pr(2, Some(1), "OPEN"),
            pr(3, Some(2), "OPEN"),
            pr(4, Some(2), "OPEN"),
            pr(5, Some(3), "OPEN"),
        ])
    }

    fn badge(s: &Stack, n: u64) -> (u32, u32) {
        let s = s.summary(n).unwrap();
        (s.position, s.length)
    }

    #[test]
    fn badges_use_the_longest_path() {
        let s = forked();
        assert_eq!(badge(&s, 1), (1, 4));
        assert_eq!(badge(&s, 2), (2, 4));
        assert_eq!(badge(&s, 3), (3, 4));
        assert_eq!(badge(&s, 5), (4, 4));
        assert_eq!(badge(&s, 4), (3, 3));
    }

    #[test]
    fn badges_count_only_open_prs() {
        let s = stack(vec![
            pr(1, None, "MERGED"),
            pr(2, Some(1), "OPEN"),
            pr(3, Some(2), "OPEN"),
        ]);
        assert_eq!(badge(&s, 2), (1, 2));
        assert_eq!(badge(&s, 3), (2, 2));
        assert_eq!(badge(&s, 1).0, 0);
    }

    #[test]
    fn a_lone_pr_has_no_summary() {
        assert_eq!(stack(vec![pr(1, None, "OPEN")]).summary(1), None);
        assert_eq!(forked().summary(9), None);
    }

    fn row(pr: Option<usize>, lane: usize, up: bool, down: bool) -> GraphRow {
        GraphRow {
            pr,
            lane,
            up,
            down,
            through: Vec::new(),
            joins: Vec::new(),
        }
    }

    #[test]
    fn linear_graph() {
        let s = stack(vec![pr(1, None, "OPEN"), pr(2, Some(1), "OPEN")]);
        assert_eq!(
            s.graph(),
            [
                row(Some(1), 0, false, true),
                row(Some(0), 0, true, true),
                row(None, 0, true, false),
            ]
        );
    }

    #[test]
    fn forks_get_their_own_lanes() {
        let rows = forked().graph();
        let order: Vec<Option<usize>> = rows.iter().map(|r| r.pr).collect();
        // D, C1, C2, B, A, main.
        assert_eq!(order, [Some(4), Some(2), Some(3), Some(1), Some(0), None]);
        assert_eq!(rows[2].lane, 1);
        assert_eq!(rows[2].through, [0]);
        assert!(rows[2].down && !rows[2].up);
        assert_eq!(rows[3].joins, [1]);
        assert!(rows[3].up);
        assert!(rows[0].down && !rows[0].up);
    }

    #[test]
    fn nested_forks_dont_share_lanes() {
        // main <- 1 <- {2 <- {4, 5}, 3}
        let s = stack(vec![
            pr(1, None, "OPEN"),
            pr(2, Some(1), "OPEN"),
            pr(3, Some(1), "OPEN"),
            pr(4, Some(2), "OPEN"),
            pr(5, Some(2), "OPEN"),
        ]);
        let rows = s.graph();
        let lanes: Vec<(Option<u64>, usize)> = rows
            .iter()
            .map(|r| (r.pr.map(|i| s.prs[i].number), r.lane))
            .collect();
        assert_eq!(
            lanes,
            [
                (Some(4), 0),
                (Some(5), 1),
                (Some(2), 0),
                (Some(3), 1),
                (Some(1), 0),
                (None, 0)
            ]
        );
        // 5's lane ends at 2 before 3 reuses it.
        assert_eq!(rows[2].joins, [1]);
        assert_eq!(rows[3].through, [0]);
        assert_eq!(rows[4].joins, [1]);
    }

    fn summary(number: u64, stack: Option<&Stack>) -> PrSummary {
        PrSummary {
            key: PrKey {
                repo: "o/r".into(),
                number,
            },
            title: String::new(),
            author: String::new(),
            state: "OPEN".into(),
            is_draft: false,
            updated_at: String::new(),
            url: String::new(),
            version_count: 1,
            synced: true,
            sync_error: None,
            stack: stack.and_then(|s| s.summary(number)),
        }
    }

    fn layout(prs: &[PrSummary]) -> Vec<(u64, Option<Link>)> {
        group_inbox(prs)
            .into_iter()
            .map(|r| (prs[r.index].key.number, r.link))
            .collect()
    }

    #[test]
    fn stack_mates_cluster_at_the_first_one() {
        let s = forked();
        let prs = [
            summary(9, None),
            summary(2, Some(&s)),
            summary(8, None),
            summary(4, Some(&s)),
            summary(3, Some(&s)),
        ];
        assert_eq!(
            layout(&prs),
            [
                (9, None),
                (4, Some(Link::Direct)),
                (3, Some(Link::Direct)),
                (2, None),
                (8, None),
            ]
        );
    }

    #[test]
    fn missing_middle_is_a_gap() {
        let s = stack(vec![
            pr(1, None, "OPEN"),
            pr(2, Some(1), "OPEN"),
            pr(3, Some(2), "OPEN"),
        ]);
        let prs = [summary(1, Some(&s)), summary(3, Some(&s))];
        assert_eq!(layout(&prs), [(3, Some(Link::Gap)), (1, None)]);
    }

    #[test]
    fn one_stale_stack_doesnt_split_a_cluster() {
        let fresh = stack(vec![pr(1, None, "OPEN"), pr(2, Some(1), "OPEN")]);
        let prs = [summary(1, None), summary(2, Some(&fresh))];
        assert_eq!(layout(&prs), [(2, Some(Link::Direct)), (1, None)]);
    }

    #[test]
    fn depth_counts_prs_outside_the_inbox() {
        // D(5) sits above C2(4) even though only C2's parent is listed.
        let s = forked();
        let prs = [summary(4, Some(&s)), summary(2, Some(&s)), summary(5, Some(&s))];
        assert_eq!(
            layout(&prs),
            [(5, Some(Link::Gap)), (4, Some(Link::Direct)), (2, None)]
        );
    }

    #[test]
    fn unsynced_prs_stay_apart() {
        let s = stack(vec![pr(1, None, "OPEN"), pr(2, Some(1), "OPEN")]);
        let mut unsynced = summary(1, None);
        unsynced.synced = false;
        let prs = [unsynced, summary(2, Some(&s))];
        assert_eq!(layout(&prs), [(1, None), (2, None)]);
    }
}
