# TODO

## Comment-only reviews land in "Waiting for reviewers"

A PR of mine whose only reviews are comments (no approval, no changes
requested) should be in "Returned to you". It shows up in "Waiting for
reviewers" instead. In BungeeSC/grenadine-test, #3 (`inbox-returned-commented`)
and #21 (`comments`) are affected.

GitHub search can't express what we need here:

- `review:none` matches PRs whose only reviews are comments, so these PRs
  land in "Waiting for reviewers".
- So `(-review:approved -review:none)` in the "Returned to you" filter only
  ever matches PRs with changes requested.
- `review:commented` and `reviewed-by:*` match nothing.
- `reviewed-by:` and `commenter:` only take a specific username.
- `comments:>0` misses reviews that have only a body and no inline comments.

Possible fixes:

- Fetch each search hit's `latestReviews` (author and state) and filter the
  inbox results in grenadine. Ignore the PR author's own reviews: replying
  to a comment creates a COMMENTED review. `validate_inboxes` would need
  updating too.
- Or accept GitHub's behavior. Simplify the "Returned to you" filter to
  `review:changes_requested` and change the test repo's expectations.

## Devin's ACU totals may be session-wide

Check against a real two-turn `devin acp` run whether
`cognition.ai/totalAcuCost`, and `agent_stopped`'s `acuCost`, which a
turn's cost comes from, are per-process or session-wide totals.
`TurnSink` bills from 0 each turn, so a session-wide total would bill
earlier turns again after `session/load`.
