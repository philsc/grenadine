# Stacked PRs: requirements

These requirements cover which PRs make up a PR's stack and how grenadine
shows stacks on the PR page and in the inboxes. Each requirement has a
stable ID so that tests and reviews can refer to it.

## Terms

- **Parent**: the PR whose head branch a PR targets. A PR without a parent
  targets a plain branch, such as `main`.
- **Base branch**: the branch the bottom PR of a stack targets.
- **Ancestors**: a PR's parent, the parent's parent, and so on down to the
  base branch.
- **Descendants**: every PR whose ancestors include the PR.
- **Stack**: a PR's ancestors, the PR itself and all of its descendants.
- **Stack-mates**: the other PRs in a PR's stack.

## Example stack

Most examples below use this stack. All of its PRs are open.

```
main <- #1 A <- #2 B <- #3 C1 <- #5 D
                     <- #4 C2
```

B targets A's head branch. C1 and C2 both target B's head branch. D
targets C1's head branch.

## Membership

**STACK-MEM-1.** A PR's parent must be the PR whose head branch has the
same name as the branch the PR targets, in the same repository.
*Why:* GitHub, Graphite, spr and git-town all express stacks this way.

**STACK-MEM-2.** A PR whose head branch is in a fork must never be a
parent. It may still have a parent of its own.
*Why:* a fork's branch only shares its name with a branch in the base
repository. Without this rule, every PR targeting `main` would get any
fork PR opened from a branch called `main` as its parent.

**STACK-MEM-3.** A PR whose head branch is in a fork must have no
descendants.
*Why:* PRs target branches of the base repository, so nothing can target
a fork's branch.

**STACK-MEM-4.** Only open and merged PRs may be in a stack. Closed PRs
that were never merged must be left out.
*Why:* a PR closed without merging is usually abandoned.

**STACK-MEM-5.** When several PRs have the head branch a PR targets, the
open one must be the parent. If none is open, the most recently merged
one must be. A PR has at most one parent.
*Why:* branch names get reused. Example: B targets `feature-x`. #10 from
`feature-x` was merged last month and #12 from the same branch is open now.
#12 is B's parent.

**STACK-MEM-6.** A PR's stack must contain its ancestors, the PR itself
and all of its descendants, including forks. It must not contain other
descendants of the ancestors.
*Why:* those other branches don't affect the PR. Example: on C2's page
the stack is A, B and C2. C1 and D are left out.

**STACK-MEM-7.** The walk must stop after 20 ancestors and after 50
descendants. A PR that the walk has already visited must not be added
again.
*Why:* the limits keep the number of GitHub requests bounded. The visited
check ends the walk when retargeting has made two PRs target each other's
branches.

**STACK-MEM-8.** A PR is stacked only when its stack contains at least
one other PR.

## PR page

**STACK-PAGE-1.** The stack list must appear below the PR header (the
title, the "wants to merge" line and its banners) and above the
description.

**STACK-PAGE-2.** The stack list must be hidden when the PR isn't stacked
(STACK-MEM-8) or when its stack hasn't been fetched yet.

**STACK-PAGE-3.** The list must start with the top of the stack and end
with a row for the base branch, which shows the branch's name.
*Why:* this matches Graphite and `git log`.

**STACK-PAGE-4.** Forks must be drawn as lanes in a graph, like
`git log --graph`. The first child of a PR, the one with the lowest
number, continues its parent's lane. Each further child gets its own lane
to the right, and that lane joins the parent's lane at the parent's row.
A child's whole subtree is listed above the parent, and lanes must never
overlap.
*Example:* on B's page the rows are D, C1, C2, B, A, `main`. D and C1
share B's lane. C2 has a second lane that joins B's lane at B's row.

**STACK-PAGE-5.** The graph must be drawn with CSS, not box-drawing
characters.

**STACK-PAGE-6.** Each PR row must show `#number`, the title, a state
badge (open or merged) and a draft badge for drafts.

**STACK-PAGE-7.** The current PR must be shown in bold, must not be a
link, and must have its graph dot highlighted.

**STACK-PAGE-8.** Every other PR must link to its PR. A click must behave
like a PR link in the description: it follows the "Open PR links in
grenadine" toggle, and Shift reverses it.
*Why:* PR links behave the same everywhere on the page.

**STACK-PAGE-9.** When the walk stopped at the descendant limit, a muted
"… more not shown" row must appear above the top row. When it stopped at
the ancestor limit, that row must replace the base branch's name.

**STACK-PAGE-10.** Merged PRs must be shown in the list even though the
badge doesn't count them (STACK-BADGE-2).
*Example:* in `main <- A (merged) <- B`, B's page lists B, A and `main`,
but B has no badge.

## Inboxes

**STACK-INBOX-1.** Within one inbox, stack-mates must be listed next to
each other, where the highest-ranked of them is. PRs that aren't stacked
keep their place in the search order.

**STACK-INBOX-2.** Within such a group, PRs must be ordered top of the
stack first, deepest first. PRs at the same depth must keep their inbox
order.
*Example:* an inbox lists 9, B, 8, C2 and C1 in that order. It shows 9,
C2, C1, B, 8. C2 and C1 are at the same depth, so C2 comes first because
it ranks higher. 9 and 8 aren't stacked.

**STACK-INBOX-3.** Depth must count every ancestor, including ones that
aren't in this inbox.
*Example:* an inbox lists C2, B and D. D is two levels above B and C2 is
one level above it, so the order is D, C2, B.

**STACK-INBOX-4.** Each pair of neighbors in a group must be joined by a
vertical line left of the titles. The sidebar doesn't draw lanes.
*Why:* the sidebar is narrow. The badge tells siblings apart.

**STACK-INBOX-5.** The line between two neighbors must be solid when the
lower one is the upper one's parent, or when both have the same parent.
Otherwise it must be dashed, to show that stack-mates between them aren't
in this inbox.
*Example:* an inbox lists A and C from `main <- A <- B <- C`. C sits above
A and the line between them is dashed.

**STACK-INBOX-6.** Two PRs must be grouped when either one's stack lists
the other.
*Why:* stacks are only refreshed when a PR syncs, so one PR's stack can be
stale. Example: A synced before C was opened on top of it, so A's stack
doesn't list C, but C's stack lists A. They are still grouped.

**STACK-INBOX-7.** When ordering a group, a PR's own stack decides who
its parent is. For a PR whose own stack doesn't say, a stack-mate's stack
decides.

**STACK-INBOX-8.** PRs that haven't synced must not be grouped.

**STACK-INBOX-9.** Only PRs of the same repository may be grouped.

## Position badge

**STACK-BADGE-1.** A stacked PR in an inbox must show an `n/m` badge.
`n` is the PR's position counted up from the base branch. `m` is the
length of the longest path from the base branch through the PR to the top
of the stack.
*Example:* A is 1/4, B is 2/4, C1 is 3/4, D is 4/4 and C2 is 3/3.

**STACK-BADGE-2.** Only open PRs count towards `n` and `m`.
*Example:* in `main <- A (merged) <- B <- C`, B is 1/2 and C is 2/2.

**STACK-BADGE-3.** The badge must be hidden when the PR isn't open or
when `m` is less than 2.

**STACK-BADGE-4.** The badge must come from the PR's own stack. Unlike
grouping (STACK-INBOX-6), it doesn't combine stacks, so it can be stale
until the PR syncs again.
