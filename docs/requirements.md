# grenadine: requirements

These requirements describe how grenadine should behave. Each feature has
its own section, and each requirement has a stable ID so that tests and
reviews can refer to it.

- [Stacked PRs](#stacked-prs)
- [Version picker](#version-picker)
- [Reviews and merge state](#reviews-and-merge-state)

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
_Why:_ GitHub, Graphite, spr and git-town all express stacks this way.

**STACK-MEM-2.** A PR whose head branch is in a fork must never be a
parent. It may still have a parent of its own.
_Why:_ a fork's branch only shares its name with a branch in the base
repository. Without this rule, every PR targeting `main` would get any
fork PR opened from a branch called `main` as its parent.

**STACK-MEM-3.** A PR whose head branch is in a fork must have no
descendants.
_Why:_ PRs target branches of the base repository, so nothing can target
a fork's branch.

**STACK-MEM-4.** Only open and merged PRs may be in a stack. Closed PRs
that were never merged must be left out.
_Why:_ a PR closed without merging is usually abandoned.

**STACK-MEM-5.** When several PRs have the head branch a PR targets, the
open one must be the parent. If none is open, the most recently merged
one must be. A PR has at most one parent.
_Why:_ branch names get reused. Example: B targets `feature-x`. #10 from
`feature-x` was merged last month and #12 from the same branch is open now.
#12 is B's parent.

**STACK-MEM-6.** A PR's stack must contain its ancestors, the PR itself
and all of its descendants, including forks. It must not contain other
descendants of the ancestors.
_Why:_ those other branches don't affect the PR. Example: on C2's page
the stack is A, B and C2. C1 and D are left out.

**STACK-MEM-7.** The walk must stop after 20 ancestors and after 50
descendants. A PR that the walk has already visited must not be added
again.
_Why:_ the limits keep the number of GitHub requests bounded. The visited
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
_Why:_ this matches Graphite and `git log`.

**STACK-PAGE-4.** Forks must be drawn as lanes in a graph, like
`git log --graph`. The first child of a PR, the one with the lowest
number, continues its parent's lane. Each further child gets its own lane
to the right, and that lane joins the parent's lane at the parent's row.
A child's whole subtree is listed above the parent, and lanes must never
overlap.
_Example:_ on B's page the rows are D, C1, C2, B, A, `main`. D and C1
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
_Why:_ PR links behave the same everywhere on the page.

**STACK-PAGE-9.** When the walk stopped at the descendant limit, a muted
"… more not shown" row must appear above the top row. When it stopped at
the ancestor limit, that row must replace the base branch's name.

**STACK-PAGE-10.** Merged PRs must be shown in the list even though the
badge doesn't count them (STACK-BADGE-2).
_Example:_ in `main <- A (merged) <- B`, B's page lists B, A and `main`,
but B has no badge.

## Inboxes

**STACK-INBOX-1.** Within one inbox, stack-mates must be listed next to
each other, where the highest-ranked of them is. PRs that aren't stacked
keep their place in the search order.

**STACK-INBOX-2.** Within such a group, PRs must be ordered top of the
stack first, deepest first. PRs at the same depth must keep their inbox
order.
_Example:_ an inbox lists 9, B, 8, C2 and C1 in that order. It shows 9,
C2, C1, B, 8. C2 and C1 are at the same depth, so C2 comes first because
it ranks higher. 9 and 8 aren't stacked.

**STACK-INBOX-3.** Depth must count every ancestor, including ones that
aren't in this inbox.
_Example:_ an inbox lists C2, B and D. D is two levels above B and C2 is
one level above it, so the order is D, C2, B.

**STACK-INBOX-4.** Each pair of neighbors in a group must be joined by a
vertical line left of the titles. The inbox list doesn't draw lanes.
_Why:_ lanes would clutter a list of many PRs. The badge tells siblings
apart.

**STACK-INBOX-5.** The line between two neighbors must be solid when the
lower one is the upper one's parent, or when both have the same parent.
Otherwise it must be dashed, to show that stack-mates between them aren't
in this inbox.
_Example:_ an inbox lists A and C from `main <- A <- B <- C`. C sits above
A and the line between them is dashed.

**STACK-INBOX-6.** Two PRs must be grouped when either one's stack lists
the other.
_Why:_ stacks are only refreshed when a PR syncs, so one PR's stack can be
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
_Example:_ A is 1/4, B is 2/4, C1 is 3/4, D is 4/4 and C2 is 3/3.

**STACK-BADGE-2.** Only open PRs count towards `n` and `m`.
_Example:_ in `main <- A (merged) <- B <- C`, B is 1/2 and C is 2/2.

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
of a stack targets is the _base branch_ (see Stacked PRs).

## Example PR

Most examples below use this PR. It is the one in the tests of
`core/src/versions.rs`.

1. The author pushes C1 and C2 and opens the PR. _v1 = C2._
2. The author pushes C3. _v2 = C3._
3. The author rebases onto a newer `main` and force-pushes C4. _v3 = C4._
4. The author pushes C5. _v4 = C5._
5. The author squashes everything into C6 and force-pushes. _v5 = C6._
6. The author pushes C7 and C8 together. _v6 = C7, v7 = C8._

## Versions

**VPICK-VER-1.** The commit the head branch pointed at when the PR was
opened must be version 1, however many commits the PR had then.
_Why:_ the commits before opening are the author's drafts. The PR as first
proposed is what reviewers saw.

**VPICK-VER-2.** Every force push must add one version: its new head.

**VPICK-VER-3.** Every commit that a fast-forward push adds must be its
own version, following first parents, even when one push adds several
commits.
_Example:_ step 6 adds v6 and v7.
_Why:_ authors who push fixups one by one, or several at once, still get
one diff per fixup.

**VPICK-VER-4.** A push that GitHub records as a regular push but that
doesn't start at the previous version must count as a force push.

**VPICK-VER-5.** Pushes before the PR was opened must not be versions.
Deleting and restoring the head branch must not add versions by itself.

**VPICK-VER-6.** The push history must come from GitHub's activity log of
the head branch. Pushes the log doesn't list yet must still become
versions, one per commit.
_Why:_ the log lags behind the branch by a few minutes.

**VPICK-VER-7.** When the activity log doesn't cover the PR, the versions
must be reconstructed from the PR's force-push events. Version 1 must be
the first force push's old head, or else the last commit whose first
check suite ran before the PR was opened, or else the PR's head. The PR
page must then show a banner saying that the versions were reconstructed
and may not match every push.
_Why:_ the log only goes back to around March 2023, and it is gone with a
deleted fork.

**VPICK-VER-8.** A version whose commit can't be fetched must still be
listed, marked as missing. When the commits between two versions can't be
listed, the newer one must become a single version, as if it had been
force-pushed.

**VPICK-VER-9.** Computing the versions twice from the same history must
give the same versions. When a sync's versions don't just add to the
stored ones, the PR page must show a banner saying how they changed, and
the change must be logged.
_Why:_ version numbers are how people refer to versions. Silently
renumbering them would make old references wrong.

**VPICK-VER-10.** A commit that has been a version must stay in the local
clone, even when it is no longer a version.
_Why:_ otherwise git could garbage-collect it.

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
_Example:_ right after step 6, the activity log doesn't list that push
yet, so v6 and v7 show the authors of C7 and C8. Once the log lists it,
they show the pusher. Without the log, only versions created by force
pushes have a known pusher.
_Why:_ the author usually pushed the commit, but cherry-picks and bots
break that rule, so the guess must look like one.

**VPICK-PUSH-4.** When neither the pusher nor the author is known, the
cell must be empty.

**VPICK-PUSH-5.** Names and avatars must be looked up at most once a
week per user. A commit's author must be looked up at most once per
commit.
_Why:_ to keep the number of GitHub requests bounded. A commit's author
never changes.

**VPICK-PUSH-6.** Failing to look up pushers must not fail the PR's sync.
_Why:_ the versions and the diff matter more than who pushed.

## Picker

**VPICK-ROW-1.** The picker must be a drop-down in the toolbar above the
diff. Closed, it must show the selection as `Base → v4 (1a2b3c4d)`, or
`v2 (…) → v4 (…)` when Base is a version, with each SHA shortened to 8
characters. A PR without versions must show "No versions".

**VPICK-ROW-2.** The list must have one row per version and a merge-base
row. It must start with the latest version and end with the merge-base
row.
_Why:_ the latest version is the one people pick most, and this matches
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
_Why:_ when reviewing, how recent a push is matters more than its exact
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
_Example:_ two threads were started on v2 and one of them is resolved.
v2 shows `1/2`.

**VPICK-ROW-8.** The picker must close when the user clicks anywhere
outside it or presses Escape. Picking a Base or a Head must leave it open.
_Why:_ choosing a selection often takes two clicks.

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
_Why:_ the default diff then isn't the latest code, and the reader must
know.

**VPICK-SEL-4.** Picking a Base at or above Head must move Head to the
latest selectable version. Picking a Head at or below Base must move Base
to the merge-base row.
_Why:_ those are the two most common comparisons, "what changed since I
last looked" and "the whole PR".
_Example:_ the selection is v2 → v4. Picking v5 as Base gives v5 → v7.
Picking v1 as Head gives Base → v1.

**VPICK-SEL-5.** Once the user has picked something, new versions must
not change the selection. If a sync makes the selection invalid, e.g.
because a version became missing, the selection must fall back to the
default (VPICK-SEL-2).
_Why:_ jumping to a new version in the middle of a review would lose the
reader's place.

**VPICK-SEL-6.** The selection must reset to the default when the page is
reloaded or another PR is opened. It is not required to be in the URL or
to be remembered.
_Why:_ nobody has needed to share or resume a selection yet. This can be
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
_Example:_ v2 → v4 crosses the rebase of step 3. Upstream changed 40
files the PR never touched, so the summary says "40 files changed only
by the rebase are hidden".
_Why:_ those files only differ because of the rebase. To see them,
compare against the merge-base row.

**VPICK-DIFF-4.** In the same case, a change block that upstream also
made between the two merge-bases must be marked as brought in by the
rebase, and the diff summary must explain the mark. A file whose change
blocks are all marked must say "only rebase changes" in its header.
Trailing whitespace must not matter when comparing blocks.
_Why:_ this separates the author's changes from upstream's, like Gerrit.

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
_Why:_ left-side comments were made against the PR's base at the time,
not against an older version.

**VPICK-CMT-4.** A thread on a whole file must be shown at the top of
that file when Head is the commit it was made on or the latest version.

**VPICK-CMT-5.** Every other thread must be listed in a collapsed panel
above the diff, titled "N comment threads on other versions". Each entry
must show the file and line, and the version it was made on as a button,
or "on an unknown version" when its commit isn't a version.
_Example:_ with v2 → v4, Head is neither v3 nor the latest version, so a
thread made on v3 is listed as "on v3".

**VPICK-CMT-6.** The version button must make that version Head. Base
must stay when it is older than that version and must otherwise become
the merge-base row.

**VPICK-CMT-7.** A resolved thread must be shown collapsed to one line,
"Resolved · author: first line of the first comment", inline and in the
panel. Clicking it must expand it.
_Why:_ the picker counts resolved threads, so the diff must show which
ones they are, without them taking up space.

# Reviews and merge state

These requirements cover how the PR page shows who reviewed a PR, the
conversation on it, its checks and whether it can be merged.

## Terms

- **Verdict**: an approval, a change request or a dismissed review.
- **Conversation**: the comments on the PR itself and the summaries of
  its reviews, as opposed to review comments on its code.
- **Viewer**: the GitHub user whose token grenadine uses.

## Review summary

**REVIEW-SUM-1.** The PR header must show GitHub's review decision as
"Approved", "Changes requested" or "Review required". When GitHub
reports no decision, none must be shown.

**REVIEW-SUM-2.** Next to the decision, the header must show every
reviewer once, in the order they first reviewed, marked with their latest
verdict. A reviewer without a verdict must be marked as having commented.
_Why:_ a comment after an approval doesn't take the approval back on
GitHub either.

**REVIEW-SUM-3.** The PR's author must not be listed as a reviewer.
_Why:_ replying to a review thread creates a comment review by the author.

**REVIEW-SUM-4.** Users and teams asked to review who haven't yet must be
listed as "Waiting on …".

**REVIEW-SUM-5.** Pending reviews must not count as reviews.
_Why:_ only their author can see them, and they may never be submitted.

## Conversation

**REVIEW-CONV-1.** Below the description, the page must list the
comments on the PR and the reviews, oldest first, in a section titled
"N comments" that starts out unfolded.

**REVIEW-CONV-2.** Each review must show its author, its verdict
("approved", "requested changes", "reviewed") and its summary. Comment
reviews without a summary must be left out.
_Why:_ their comments are already shown in the diff.

**REVIEW-CONV-3.** Each entry must link its time to the entry on GitHub
and render its body like the description.

## Writing comments

**REVIEW-WRITE-1.** Comments are written in a comment box with Write and
Preview tabs. The preview must render the text like the description.
Ctrl+Enter, or Cmd+Enter, must send it; plain Enter adds a line.
_Why:_ comments usually span several lines, unlike agent prompts.

**REVIEW-WRITE-2.** What's written in a comment box must be kept as a
draft in the browser until it's sent, so that reloading the page or
switching PRs doesn't lose it. A box with a draft must start out open.
Cancel must close the box and drop its draft.

**REVIEW-WRITE-3.** A box must stay open with its text and show the error
when sending fails. Once sent, it must close and drop its draft.

**REVIEW-WRITE-4.** Every unfolded review thread must offer Reply, which
opens a comment box under the thread. A reply must be posted to GitHub
right away.

**REVIEW-WRITE-5.** An unfolded thread must offer Resolve, or Unresolve
when it's resolved, when GitHub says the viewer may. Resolving folds the
thread like any other resolved thread (VPICK-CMT-7).

**REVIEW-WRITE-6.** The conversation must end with a Comment button that
opens a comment box for a comment on the PR itself. The conversation must
be shown even when it's empty, titled "No comments".

**REVIEW-WRITE-7.** After grenadine changes a PR on GitHub, it must sync
the PR again so that the page shows the change. A sync that was already
running when the change was made must not count, since it may have read
the PR before the change.

## Pending review

A review is written as GitHub's pending review, so it's also there on
github.com and can be continued or submitted in either place.

**REVIEW-PEND-1.** Clicking a line number in the diff must open a comment
box under that line, and "Comment on file" in a file's header must open
one for the whole file. Sending must add the comment to the viewer's
pending review, starting one if there's none.

**REVIEW-PEND-2.** Lines and files can only be commented on when the PR
is open and Head is the latest version. Lines on the left side can only
be commented on when Base is the merge-base row.
_Why:_ GitHub anchors new comments to the PR's current diff, and left
lines are lines of the merge-base (VPICK-CMT-3).

**REVIEW-PEND-3.** Comments of the pending review must be shown like
other comments, placed by the same rules, with a "Pending" badge, Edit and
Delete. Delete must ask first. Pending threads must not be counted in the
version picker.

**REVIEW-PEND-4.** A reply box must offer both posting the reply right
away and adding it to the pending review. While a review is pending,
adding it to the review must be the main button; otherwise posting it
right away must be.

**REVIEW-PEND-5.** The toolbar must have a Review button showing the
number of pending comments. It must open a panel with a summary box,
Comment, Approve and Request changes, "Submit review" and, while a review
is pending, "Discard review", which must ask first.

**REVIEW-PEND-6.** Submitting must submit the pending review with the
summary and the chosen verdict, or review the PR without comments when
nothing is pending. Request changes needs a summary. Comment needs a
summary or pending comments. Submit must be disabled until these hold.

**REVIEW-PEND-7.** On the viewer's own PR, Approve and Request changes
must be disabled.
_Why:_ GitHub refuses them.

**REVIEW-PEND-8.** Comment boxes, which ones are open and show their
preview, the panel's choice and whether it is open must survive the page
refetching the PR. A comment sent while the page refetches must still
close its box and drop its draft.
_Why:_ every change, the viewer's own included, refetches the PR.

## Merge box

**REVIEW-MERGE-1.** Below the conversation, open PRs must show a merge
box. Merged and closed PRs must not.

**REVIEW-MERGE-2.** The merge box must list the checks on the PR's head
under a summary with their combined state and how many failed, are
running, passed and were skipped, e.g. "Checks: 1 failing, 2 passing".
The list must start out unfolded when the combined state is failing, and
folded otherwise. Each check must link to its details when GitHub has a
link. The list must be hidden when the head has no checks.

**REVIEW-MERGE-3.** The merge box must say whether the PR can be merged,
from GitHub's merge state:

| Merge state                 | Text                                                    |
| --------------------------- | ------------------------------------------------------- |
| conflicts (or DIRTY)        | This branch has conflicts that must be resolved.        |
| CLEAN, HAS_HOOKS            | Ready to merge.                                         |
| UNSTABLE                    | Can be merged, but not every check passed.              |
| BEHIND                      | This branch is out of date with its base.               |
| DRAFT                       | This PR is a draft.                                     |
| BLOCKED                     | Merging is blocked: _reason_.                           |
| anything else, e.g. UNKNOWN | GitHub is still working out whether this can be merged. |

The reason for BLOCKED must be "it needs an approving review" when a
review is required, "changes were requested" when changes were requested,
and "required checks or rules aren't satisfied" otherwise.

**REVIEW-MERGE-4.** Reviews, the conversation, checks and the merge state
must be fetched with each sync of the PR, in the same GraphQL request as
its other metadata.
_Why:_ they change whenever the PR's `updatedAt` does, which is what
triggers a sync.

**REVIEW-MERGE-5.** The merge box must offer the merge methods the
repository allows, labelled "Create a merge commit", "Squash and merge"
and "Rebase and merge". The first one picked must be the one last used in
the repository from this browser, else GitHub's default for the viewer,
else the first allowed one.

**REVIEW-MERGE-6.** Merge must be disabled, with the reason shown, when
the PR is a draft, conflicts with its base, isn't known to be mergeable
yet, or the branch's rules block it or require it to be up to date. When
GitHub says the viewer may bypass the rules, the button must instead read
"Merge, bypassing rules" and look dangerous. Conflicts can't be bypassed.

**REVIEW-MERGE-7.** Merge must ask first, naming the method, the PR and
the branch it goes into.

**REVIEW-MERGE-8.** The merge must only go through when the PR's head is
still the latest version the page shows. Otherwise GitHub's refusal must
be shown and the PR left open.
_Why:_ a push after the viewer looked must not be merged unseen.

**REVIEW-MERGE-9.** Squash merges must offer to edit the commit message,
starting from "_title_ (#_number_)" and the description. Without edits,
GitHub's default message must be used.

**REVIEW-MERGE-10.** Unless the repository deletes merged branches
itself, the merge box must offer to delete the head branch, checked
unless other open PRs target it. A merge whose branch couldn't be deleted
must still count as merged, with a warning.
_Why:_ deleting a branch that other PRs target closes them.

**REVIEW-MERGE-11.** When the PR targets another open PR's branch, the
merge box must warn that merging puts its changes there, not into the
stack's base branch.

## Inboxes

**REVIEW-INBOX-1.** Each PR in an inbox must show GitHub's review decision
as an icon before its CI icon: a green check for approved, red for
changes requested and muted for review required, titled "Approved",
"Changes requested" and "Review required". A PR without a decision shows
no icon.
_Why:_ it tells at a glance which of one's PRs can be merged and which
need work, without opening them.

**REVIEW-INBOX-2.** The decision must come from the inbox search, like the
thread count and the CI status, so that it costs no extra requests.
