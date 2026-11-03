# grenadine

Grenadine is a web app for reviewing GitHub pull requests. It lists PRs in inboxes, groups stacked PRs together, and tracks each PR's versions.

Run it with:

```sh
bazel run //grenadine/server -- --repo=PATH[:REMOTE]
```

See [AGENTS.md](AGENTS.md) for building, testing and linting.

- [Inbox filters](#inbox-filters)
- [Default inboxes](#default-inboxes)

## Inbox filters

Each inbox has a name and a filter. A filter is a GitHub search query, optionally extended with grenadine qualifiers that start with `gr:`. Every poll runs each inbox's filter and lists the matching PRs.

### Query structure

A filter is a list of terms. Terms separated by spaces are ANDed. `OR` combines terms, parentheses group them, and AND binds tighter than OR. A `-` in front of a qualifier negates it.

```text
author:@me ((gr:review:active -review:approved) OR gr:changes-requested:unaddressed)
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

A new database starts with these inboxes. The ones marked "total" count in the total.

| Inbox                    | Total | Filter                                                                                                                                                    |
| ------------------------ | ----- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Needs your review        | yes   | `state:open archived:false draft:false -author:@me -is:queued user-review-requested:@me sort:updated-desc`                                                |
| Needs your teams' review |       | `state:open archived:false draft:false team-review-requested-user:@me -user-review-requested:@me sort:updated-desc`                                       |
| Returned to you          | yes   | `state:open archived:false draft:false author:@me -is:queued ((gr:review:active -review:approved) OR gr:changes-requested:unaddressed) gr:sort:title-asc` |
| Approved                 | yes   | `state:open archived:false draft:false author:@me -is:queued review:approved gr:approvals:>0 -gr:changes-requested:unaddressed sort:updated-desc`         |
| Merging                  |       | `state:open archived:false draft:false author:@me is:queued sort:updated-desc`                                                                            |
| Drafts                   |       | `state:open archived:false draft:true author:@me sort:updated-desc`                                                                                       |
| Drafts from others       |       | `state:open archived:false draft:true -author:@me user-review-requested:@me gr:sort:title-asc`                                                            |
| Waiting for reviewers    |       | `state:open archived:false draft:false author:@me -is:queued -gr:review:active -review:approved sort:updated-desc`                                        |
| All my open PRs          |       | `state:open archived:false author:@me sort:updated-desc`                                                                                                  |
| Waiting for author       |       | `state:open archived:false draft:false reviewed-by:@me -author:@me -user-review-requested:@me -is:queued sort:updated-desc`                               |
| Merged by me             |       | `state:closed archived:false author:@me sort:updated-desc`                                                                                                |
| Merged by others         |       | `state:closed archived:false -author:@me (reviewed-by:@me OR user-review-requested:@me) sort:updated-desc`                                                |
| All open PRs             |       | `state:open archived:false -is:queued sort:updated-desc`                                                                                                  |
| Unmerged PRs on mine     |       | `state:closed archived:false author:@me is:unmerged sort:updated-desc`                                                                                    |

Your open PRs that aren't drafts each appear in exactly one of Returned to you, Approved, Merging and Waiting for reviewers:

- Merging has the queued ones. The other three exclude them.
- Of the rest, Approved has the ones GitHub considers approved without an unaddressed change request.
- Returned to you has the ones with an unaddressed change request, or with an active review but not approved.
- Waiting for reviewers has the ones with neither an active review nor approval.

`bazel run //grenadine/server:validate_inboxes` checks this against live GitHub data.
