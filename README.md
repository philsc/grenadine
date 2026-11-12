# grenadine

Grenadine is a web app for reviewing GitHub pull requests. It lists PRs in inboxes, groups stacked PRs together, and tracks each PR's versions.

Run it with:

```sh
bazel run //grenadine/server -- --repo=PATH[:REMOTE]
```

See [AGENTS.md](AGENTS.md) for building, testing and linting.

- [Inbox filters](#inbox-filters)
- [Default inboxes](#default-inboxes)
- [Auto-reviews](#auto-reviews)

## Inbox filters

Each inbox has a name and a filter. A filter is a GitHub search query, optionally extended with grenadine qualifiers that start with `gr:`. Every poll runs each inbox's filter and lists the matching PRs.

### Query structure

A filter is a list of terms. Terms separated by spaces are ANDed. `OR` combines terms, parentheses group them, and AND binds tighter than OR. A `-` in front of a qualifier negates it.

```text
author:@me ((gr:review:unapproved-human -review:approved) OR gr:changes-requested:unaddressed)
```

`@me` stands for the user whose token the server uses.

Grenadine adds `is:pr` and the server's `--repo` repositories itself, so a filter shouldn't contain `is:pr` or `repo:`.

### GitHub qualifiers

Any qualifier that GitHub's pull request search supports can be used. Grenadine evaluates the following ones itself, as described in [How a filter is evaluated](#how-a-filter-is-evaluated):

| Qualifier                         | Matches a PR that                                                               |
| --------------------------------- | ------------------------------------------------------------------------------- |
| `state:open`, `is:open`           | is open.                                                                        |
| `state:closed`, `is:closed`       | is closed, merged or not.                                                       |
| `is:merged`                       | is merged.                                                                      |
| `is:unmerged`                     | is open or closed without being merged.                                         |
| `draft:true`, `draft:false`       | is, or isn't, a draft.                                                          |
| `archived:true`, `archived:false` | is, or isn't, in an archived repository.                                        |
| `author:USER`                     | was opened by USER.                                                             |
| `user-review-requested:USER`      | has a pending review request for USER directly, not through a team.             |
| `reviewed-by:USER`                | has a review by USER in any state, including dismissed reviews.                 |
| `review:approved`                 | has the review decision APPROVED.                                               |
| `review:changes_requested`        | has the review decision CHANGES_REQUESTED.                                      |
| `review:none`                     | has no approving or change-requesting review. Comment-only reviews don't count. |
| `is:queued`                       | is in a merge queue.                                                            |

`review:approved` and `review:changes_requested` follow GitHub's review decision, which depends on the repository's branch protection. In a repository that doesn't require reviews, no PR matches either of them.

Every other qualifier, such as `team-review-requested-user:`, `label:` or `updated:`, is search-only: only GitHub can evaluate it. See [Restrictions](#restrictions).

### Grenadine qualifiers

| Qualifier                          | Matches a PR that                                                                                                    |
| ---------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `gr:review:active`                 | has an [active review](#reviews).                                                                                    |
| `gr:review:active-human`           | has an active [human review](#reviews).                                                                              |
| `gr:review:unapproved-human`       | has an [unapproved human review](#reviews).                                                                          |
| `gr:changes-requested:unaddressed` | has a reviewer whose [standing verdict](#reviews) is CHANGES_REQUESTED and who hasn't been [re-requested](#reviews). |
| `gr:approvals:>N`                  | has more than N reviewers whose [standing verdict](#reviews) is APPROVED.                                            |
| `gr:approved-by:USER`              | has USER's [standing verdict](#reviews) APPROVED.                                                                    |
| `gr:rerequested:USER`              | has [re-requested](#reviews) USER's review.                                                                          |

`gr:sort:title-asc` and `gr:sort:title-desc` aren't conditions. They sort the inbox by title, A to Z or Z to A. See [Sorting](#sorting).

A `gr:` qualifier that grenadine doesn't know makes the inbox show an error.

### Reviews

The grenadine qualifiers look only at the PR's reviews and pending review requests as they are now. Grenadine doesn't look at the PR's timeline, so it can't tell, for example, whether a request was removed before the reviewer reviewed.

- **Standing verdict**: a reviewer's most recent review that is APPROVED or CHANGES_REQUESTED. A later one replaces an earlier one. A dismissed review counts as no verdict. Comment-only reviews don't change the verdict. Re-requesting a reviewer doesn't clear their verdict either, so an approval stands until it's dismissed or replaced.
- **Re-requested**: the reviewer has submitted a review and is again in the PR's pending review requests. GitHub removes a user's review request when they submit a review, so being requested again means someone asked for another review.
- **Active review**: a review in any state, by anyone other than the PR's author, that isn't dismissed and whose reviewer hasn't been re-requested. Reviews by bots count. The author's own reviews don't count because replying to a review thread creates one.
- **Active human review**: an active review not written by a bot (a GitHub App account). This keeps automated comment-only reviews from moving a PR to Returned to you, while a bot's change request still does through `gr:changes-requested:unaddressed`.
- **Unapproved human review**: an active human review whose reviewer's standing verdict isn't APPROVED. An approving reviewer is done with the PR even if they leave a later comment, so a PR with some but not enough approvals waits for the remaining reviewers instead of coming back.

For example, Bob requests changes on your PR. The PR has an active review and an unaddressed change request. You push a fix and re-request Bob's review. Bob's review is no longer active and his change request is no longer unaddressed, so the PR no longer matches `gr:review:active`. Bob's standing verdict is still CHANGES_REQUESTED until he reviews again.

### How a filter is evaluated

When a filter contains no `gr:` qualifiers, GitHub evaluates all of it and grenadine lists GitHub's results unchanged.

When a filter contains `gr:` qualifiers, grenadine evaluates it in two steps:

1. Grenadine sends GitHub a wider query: the filter with every `gr:` qualifier removed. That search finds a superset of the matching PRs.
2. Grenadine evaluates the whole filter on each PR that the search found. This includes the GitHub qualifiers, because inside an `OR` grenadine can't tell which branch GitHub matched.

### Restrictions

Grenadine can't evaluate search-only qualifiers, so it trusts GitHub's results for them. That is only correct when the search-only qualifier is ANDed with the rest of the filter. A filter that also contains `gr:` qualifiers may therefore use search-only qualifiers only as top-level terms, outside any `OR` or parentheses. Otherwise the inbox shows an error.

```text
draft:false label:needs-review gr:review:active               # fine
(label:needs-review OR gr:review:active)                      # error
```

Filters without `gr:` qualifiers have no restrictions beyond GitHub's own.

### Results

Grenadine fetches up to 100 PRs for a filter without `gr:` qualifiers. For a filter with `gr:` qualifiers, it fetches up to 1000, the most GitHub returns for one search, since the wider query usually finds more PRs than end up in the inbox. When GitHub found more PRs than grenadine fetched, the inbox says so: "GitHub found N PRs; grenadine fetched M. Tighten the filter to see the rest."

Only PRs in the server's `--repo` repositories are listed.

### Sorting

`sort:` qualifiers go to GitHub unchanged and set the order of GitHub's results, for example `sort:updated-desc`. `gr:sort:title-asc` and `gr:sort:title-desc` re-sort the inbox after grenadine has evaluated the filter. GitHub search can't sort by title.

In either case, grenadine then moves stacked PRs next to each other at the position of the highest-ranked PR in the stack.

### Total count

Each inbox has a "count in total" setting. The top bar's Inboxes tab and the browser tab's title show how many distinct PRs the inboxes with that setting list. A PR listed by several of them counts once.

## Default inboxes

A new database starts with these inboxes. The ones marked "total" count in the total. The ones marked "sync" sync in the background and are searched on every poll; an inbox without that mark is searched every 10 minutes by default (`--off-inbox-interval`), right away when it's created or edited, and when you press Refresh. Its header shows when its search last succeeded. Its PRs are listed from its search results and sync when you open them.

| Inbox                    | Total | Sync | Filter                                                                                                                                                              |
| ------------------------ | ----- | ---- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Needs your review        | yes   | yes  | `state:open archived:false draft:false -author:@me -is:queued user-review-requested:@me sort:updated-desc`                                                          |
| Needs your teams' review |       |      | `state:open archived:false draft:false team-review-requested-user:@me -user-review-requested:@me sort:updated-desc`                                                 |
| Returned to you          | yes   | yes  | `state:open archived:false draft:false author:@me -is:queued ((gr:review:unapproved-human -review:approved) OR gr:changes-requested:unaddressed) gr:sort:title-asc` |
| Approved                 | yes   | yes  | `state:open archived:false draft:false author:@me -is:queued review:approved gr:approvals:>0 -gr:changes-requested:unaddressed sort:updated-desc`                   |
| Merging                  |       |      | `state:open archived:false draft:false author:@me is:queued sort:updated-desc`                                                                                      |
| Drafts                   |       |      | `state:open archived:false draft:true author:@me sort:updated-desc`                                                                                                 |
| Drafts from others       |       |      | `state:open archived:false draft:true -author:@me user-review-requested:@me gr:sort:title-asc`                                                                      |
| Waiting for reviewers    |       |      | `state:open archived:false draft:false author:@me -is:queued -gr:review:unapproved-human -gr:changes-requested:unaddressed -review:approved sort:updated-desc`      |
| All your open PRs        |       |      | `state:open archived:false author:@me sort:updated-desc`                                                                                                            |
| Waiting for author       |       |      | `state:open archived:false draft:false reviewed-by:@me -author:@me -user-review-requested:@me -is:queued sort:updated-desc`                                         |
| Your merged PRs          |       |      | `state:closed archived:false author:@me is:merged sort:updated-desc`                                                                                                |
| Merged PRs from others   |       |      | `state:closed archived:false -author:@me is:merged (reviewed-by:@me OR user-review-requested:@me) sort:updated-desc`                                                |
| All open PRs             |       |      | `state:open archived:false -is:queued sort:updated-desc`                                                                                                            |
| Your unmerged PRs        |       |      | `state:closed archived:false author:@me is:unmerged sort:updated-desc`                                                                                              |

Your open PRs that aren't drafts each appear in exactly one of Returned to you, Approved, Merging and Waiting for reviewers:

- Merging has the queued ones. The other three exclude them.
- Of the rest, Approved has the ones GitHub considers approved without an unaddressed change request.
- Returned to you has the ones with an unaddressed change request, or with an unapproved human review but not approved.
- Waiting for reviewers has the ones left: no unaddressed change request, no unapproved human review, not approved.

`bazel run //grenadine/server:validate_inboxes` checks this against live GitHub data.

## Auto-reviews

Grenadine reviews the pull requests you open in its repositories. Each review runs in an unattended agent session that posts its review on the PR. These sessions are listed in the Auto-reviews tab, not the Agents tab.

The gear next to a repository's name in the Agents tab opens the repository's settings:

- **Review my pull requests**: Never, "When opened, once they're not drafts" (the default), or "When opened, drafts too". PRs opened before reviews were turned on for the repository aren't reviewed.
- **Review again after new pushes**: off by default, so each PR is reviewed once.
- **Review skill**: the skill the session runs, e.g. `code-review`. With none, the session gets a plain review prompt.

The same dialog sets what new sessions in the repository start with:

- **Claude/Devin permissions**: the mode the New session form's Permissions starts with for that backend. Without one it starts in "Plan first".
- **Default skill**: the skill a new session's first prompt runs, e.g. `foo` sends `bar` as `/foo bar`. A prompt that starts with its own `/skill` runs that instead.

The prompt names the PR, lists the other PRs in its stack, and asks the session to read the ones below and above it for context. The session is told that nobody answers it, and every tool approval it asks for is denied. It must post a comment review on GitHub before it finishes.

Once grenadine sees the session's review or comment on the PR, it deletes the session. It keeps a session that didn't post, failed or was interrupted, so you can see why. At most three reviews run at once.

### Addressing comments

The Auto-review section in the sidebar of one of your open PRs also has an **Address comments** button. It starts a fixing run: an unattended session that addresses the open comments on that PR and on the PRs stacked on it, pushes the changes to all of them in one push, and answers the comments.

These comments are open:

- An inline thread that isn't resolved and whose last comment isn't yours. A thread that you or an auto-review started is also open until it has a reply.
- A review summary or a comment on the PR itself that was posted after the last fixing run's answer.

The session replies in each thread with what it changed, or why it changed nothing. It resolves the threads posted under your name and leaves the others to whoever started them. It answers summaries and other comments in one comment per PR.

An agent session that manages the PRs keeps managing them while a fixing run works on them, and can push to them. After the run's push, that session's agent is told on its next turn which PRs moved, and to rebase its work onto the new commits before it pushes again.

### Rounds of review and fixes

**Rounds of review and fixes** in the repository's settings has grenadine do both by itself, up to five times. It's off by default. A round goes like this:

1. Each PR of a stack whose changes weren't reviewed yet is reviewed. A PR that was only rebased isn't reviewed again.
2. Once those reviews are done, one fixing run starts at the lowest PR that has open comments posted under your name, which is what an auto-review's are. Other people's comments wait for Address comments.
3. The fixing run's push starts the next round.

The loop ends early when a round's reviews raise nothing, or when its fixing run pushes nothing. After the last round's push there's no further review.

The rounds are counted from the last push that wasn't a fixing run's. With "Review again after new pushes", your next push to the stack starts the loop over; without it, a stack gets one loop. A PR that you add to the stack is reviewed in the round that's going on. One that's stacked on a fixing run's PRs while it works is rebased by that run along with the others.

If someone pushes to one of the PRs while the run works, the run's push is refused, the run ends as superseded, and its changes are dropped. One fixing run works on a stack at a time; another one asked for in the same stack waits. A PR with several PRs stacked directly on it can't get one. Fixing runs count towards the limit of three, use the repository's review backend and model, and are listed in the Auto-reviews tab.
