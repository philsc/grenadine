# grenadine: requirements

These requirements describe how grenadine should behave. Each feature has
its own section, and each requirement has a stable ID so that tests and
reviews can refer to it.

- [Stacked PRs](#stacked-prs)
- [Version picker](#version-picker)

# Stacked PRs

These requirements cover which PRs make up a PR's stack and how grenadine
shows stacks on the PR page and in the inboxes.

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

# Version picker

These requirements cover what a PR's versions are, how the version picker
shows them, how picking two versions decides the diff, and where review
comments go for that diff.

## Terms

- **Version**: a commit the PR's head branch pointed at, numbered from 1
  in push order. VPICK-VER-1 to VPICK-VER-8 define which commits are
  versions.
- **Latest version**: the version with the highest number.
- **Selectable version**: a version whose commit isn't missing
  (VPICK-VER-8).
- **Base** and **Head**: the two sides of the diff. Base is the older side,
  Head the newer one. The picker's columns have these names.
- **Merge-base**: the merge-base of a version's commit and the tip of the
  PR's target branch.
- **Merge-base row**: the picker's row for version 0, labelled "Base". As
  Base it stands for Head's merge-base, so its commit depends on Head.
- **Selection**: the pair of Base and Head.

Base and Head refer to the sides of a diff only. The branch the bottom PR
of a stack targets is the *base branch* (see Stacked PRs).

## Example PR

Most examples below use this PR. It is the one in the tests of
`core/src/versions.rs`.

1. The author pushes C1 and C2 and opens the PR. *v1 = C2.*
2. The author pushes C3. *v2 = C3.*
3. The author rebases onto a newer `main` and force-pushes C4. *v3 = C4.*
4. The author pushes C5. *v4 = C5.*
5. The author squashes everything into C6 and force-pushes. *v5 = C6.*
6. The author pushes C7 and C8 together. *v6 = C7, v7 = C8.*

## Versions

**VPICK-VER-1.** The commit the head branch pointed at when the PR was
opened must be version 1, however many commits the PR had then.
*Why:* the commits before opening are the author's drafts. The PR as first
proposed is what reviewers saw.

**VPICK-VER-2.** Every force push must add one version: its new head.

**VPICK-VER-3.** Every commit that a fast-forward push adds must be its
own version, following first parents, even when one push adds several
commits.
*Example:* step 6 adds v6 and v7.
*Why:* authors who push fixups one by one, or several at once, still get
one diff per fixup.

**VPICK-VER-4.** A push that GitHub records as a regular push but that
doesn't start at the previous version must count as a force push.

**VPICK-VER-5.** Pushes before the PR was opened must not be versions.
Deleting and restoring the head branch must not add versions by itself.

**VPICK-VER-6.** The push history must come from GitHub's activity log of
the head branch. Pushes the log doesn't list yet must still become
versions, one per commit.
*Why:* the log lags behind the branch by a few minutes.

**VPICK-VER-7.** When the activity log doesn't cover the PR, the versions
must be reconstructed from the PR's force-push events. Version 1 must be
the first force push's old head, or else the last commit whose first
check suite ran before the PR was opened, or else the PR's head. The PR
page must then show a banner saying that the versions were reconstructed
and may not match every push.
*Why:* the log only goes back to around March 2023, and it is gone with a
deleted fork.

**VPICK-VER-8.** A version whose commit can't be fetched must still be
listed, marked as missing. When the commits between two versions can't be
listed, the newer one must become a single version, as if it had been
force-pushed.

**VPICK-VER-9.** Computing the versions twice from the same history must
give the same versions. When a sync's versions don't just add to the
stored ones, the PR page must show a banner saying how they changed, and
the change must be logged.
*Why:* version numbers are how people refer to versions. Silently
renumbering them would make old references wrong.

**VPICK-VER-10.** A commit that has been a version must stay in the local
clone, even when it is no longer a version.
*Why:* otherwise git could garbage-collect it.

## Pushers

**VPICK-PUSH-1.** Each version must show who pushed it: the actor of the
push or force push that created it. Every commit of one push has the same
pusher.

**VPICK-PUSH-2.** A pusher must show their GitHub avatar and full name,
or their login when they have no name. The tooltip must show the name and
the login.

**VPICK-PUSH-3.** When the push history doesn't tell who pushed a
version, the version must show its commit's author instead. That author
must be shown dimmed, and the tooltip must say that the author is shown
because the pusher is unknown. An author without a GitHub account must be
shown with the name from the commit and a generic avatar.
*Example:* right after step 6, the activity log doesn't list that push
yet, so v6 and v7 show the authors of C7 and C8. Once the log lists it,
they show the pusher. Without the log, only versions created by force
pushes have a known pusher.
*Why:* the author usually pushed the commit, but cherry-picks and bots
break that rule, so the guess must look like one.

**VPICK-PUSH-4.** When neither the pusher nor the author is known, the
cell must be empty.

**VPICK-PUSH-5.** Names and avatars must be looked up at most once a
week per user. A commit's author must be looked up at most once per
commit.
*Why:* to keep the number of GitHub requests bounded. A commit's author
never changes.

**VPICK-PUSH-6.** Failing to look up pushers must not fail the PR's sync.
*Why:* the versions and the diff matter more than who pushed.

## Picker

**VPICK-ROW-1.** The picker must be a drop-down in the toolbar above the
diff. Closed, it must show the selection as `Base → v4 (1a2b3c4d)`, or
`v2 (…) → v4 (…)` when Base is a version, with each SHA shortened to 8
characters. A PR without versions must show "No versions".

**VPICK-ROW-2.** The list must have one row per version and a merge-base
row. It must start with the latest version and end with the merge-base
row.
*Why:* the latest version is the one people pick most, and this matches
the stack list and `git log`.

**VPICK-ROW-3.** The list must have the columns Base, Head, Version,
Pushed by and Threads, with these headers.

**VPICK-ROW-4.** The Version cell must show the version's number, its SHA
shortened to 8 characters with the full SHA as tooltip, how it was created
("opened", "push" or "force push"), when it was pushed, and "missing" for
a missing version. A missing version's row must be dimmed.

**VPICK-ROW-5.** The push time must be shown relative to now, e.g. "3
hours ago", with the exact time in the browser's time zone as tooltip. A
version whose push time isn't known must show no time.
*Why:* when reviewing, how recent a push is matters more than its exact
time, and a time without a time zone is easily misread.

**VPICK-ROW-6.** The Pushed by cell must show the pusher (VPICK-PUSH-1 to
VPICK-PUSH-4). The merge-base row's cell must be empty.

**VPICK-ROW-7.** The Threads cell must show `resolved/total`, the number
of resolved review threads and of all review threads whose first comment
was made on that version's commit. The tooltip must spell the numbers
out. When every thread is resolved, the count must be shown in the
"added" color. The cell must be empty for versions without threads and
for the merge-base row. Threads made on commits that aren't versions must
not be counted anywhere.
*Example:* two threads were started on v2 and one of them is resolved.
v2 shows `1/2`.

**VPICK-ROW-8.** The picker must close when the user clicks anywhere
outside it or presses Escape. Picking a Base or a Head must leave it open.
*Why:* choosing a selection often takes two clicks.

## Selection

**VPICK-SEL-1.** Each row must have a Base radio button and a Head radio
button. Base must be older than Head. The merge-base row can only be Base.
The latest selectable version can only be Head. Missing versions can't be
picked.

**VPICK-SEL-2.** Until the user picks something, the selection must be
the merge-base row as Base and the latest selectable version as Head. It
must follow new versions as they arrive.

**VPICK-SEL-3.** When the latest version is missing, the PR page must
show a banner that names the missing versions newer than the default
Head. When every version is missing, the page must say so instead of
showing a diff.
*Why:* the default diff then isn't the latest code, and the reader must
know.

**VPICK-SEL-4.** Picking a Base at or above Head must move Head to the
latest selectable version. Picking a Head at or below Base must move Base
to the merge-base row.
*Why:* those are the two most common comparisons, "what changed since I
last looked" and "the whole PR".
*Example:* the selection is v2 → v4. Picking v5 as Base gives v5 → v7.
Picking v1 as Head gives Base → v1.

**VPICK-SEL-5.** Once the user has picked something, new versions must
not change the selection. If a sync makes the selection invalid, e.g.
because a version became missing, the selection must fall back to the
default (VPICK-SEL-2).
*Why:* jumping to a new version in the middle of a review would lose the
reader's place.

**VPICK-SEL-6.** The selection must reset to the default when the page is
reloaded or another PR is opened. It is not required to be in the URL or
to be remembered.
*Why:* nobody has needed to share or resume a selection yet. This can be
revisited.

## Diff

**VPICK-DIFF-1.** With the merge-base row as Base, the diff must go from
Head's merge-base to Head. When Head's merge-base is unknown, the page
must say so instead of showing a diff.

**VPICK-DIFF-2.** With a version as Base, the diff must go from Base's
commit to Head's commit.

**VPICK-DIFF-3.** When Base and Head have different merge-bases, files
that the PR changed in neither version must be hidden. The diff summary
must say how many were hidden. They can't be shown.
*Example:* v2 → v4 crosses the rebase of step 3. Upstream changed 40
files the PR never touched, so the summary says "40 files changed only
by the rebase are hidden".
*Why:* those files only differ because of the rebase. To see them,
compare against the merge-base row.

**VPICK-DIFF-4.** In the same case, a change block that upstream also
made between the two merge-bases must be marked as brought in by the
rebase, and the diff summary must explain the mark. A file whose change
blocks are all marked must say "only rebase changes" in its header.
Trailing whitespace must not matter when comparing blocks.
*Why:* this separates the author's changes from upstream's, like Gerrit.

**VPICK-DIFF-5.** A PR without versions must show "This PR has no
versions yet." instead of a diff.

## Comments

**VPICK-CMT-1.** A review thread must be shown inline at the line it was
made on when Head is the commit it was made on.

**VPICK-CMT-2.** Otherwise, when Head is the latest version, a thread
must be shown inline at the line GitHub mapped it to on the PR's head. An
outdated thread, which GitHub couldn't map, isn't shown inline.

**VPICK-CMT-3.** A thread on the left side of a diff must only be shown
inline when Base is the merge-base row.
*Why:* left-side comments were made against the PR's base at the time,
not against an older version.

**VPICK-CMT-4.** A thread on a whole file must be shown at the top of
that file when Head is the commit it was made on or the latest version.

**VPICK-CMT-5.** Every other thread must be listed in a collapsed panel
above the diff, titled "N comment threads on other versions". Each entry
must show the file and line, and the version it was made on as a button,
or "on an unknown version" when its commit isn't a version.
*Example:* with v2 → v4, Head is neither v3 nor the latest version, so a
thread made on v3 is listed as "on v3".

**VPICK-CMT-6.** The version button must make that version Head. Base
must stay when it is older than that version and must otherwise become
the merge-base row.

**VPICK-CMT-7.** A resolved thread must be shown collapsed to one line,
"Resolved · author: first line of the first comment", inline and in the
panel. Clicking it must expand it.
*Why:* the picker counts resolved threads, so the diff must show which
ones they are, without them taking up space.
