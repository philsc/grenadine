---
title: "Inbox searches fetch review fields only when a filter needs them, in small batches"
author: Philipp
date: 2026-11-01
branch: main
id: l-3d5fb015
scope: repo
status: active
src: agent
touches:
  - README.md
  - grenadine/core/src/api.rs
  - grenadine/core/src/lib.rs
  - grenadine/server/BUILD.bazel
  - grenadine/server/src/api.rs
  - grenadine/server/src/bin/validate_inboxes.rs
  - grenadine/server/src/db.rs
  - grenadine/server/src/github.rs
  - grenadine/server/src/inboxes.rs
  - grenadine/server/src/main.rs
---

## Decided

- Grenadine evaluates `gr:` qualifiers itself from the review data on each search hit (reviews, reviewRequests, reviewDecision, isInMergeQueue), fetched inline in the search query. Re-requests and closed requests are inferred from the PR's current state (a reviewer with a review who is back in reviewRequests), not from timeline events, to keep it to one query per poll.
- The review fields (`SEARCH_FACT_FIELDS` in grenadine/server/src/github.rs) are only requested for inboxes whose filter has `gr:` terms. With them on every search, GitHub returned 502s for wide searches such as "Merged by others" and "All open PRs" even on their own.
- Searches go `SEARCH_BATCH` (5) per GraphQL request in grenadine/server/src/inbox_search.rs. A failed batch is split in half and retried; a single search gets one retry before its inbox shows the error. All searches in one request also produced 502s.

## Watch out

- GitHub's `sort:updated-desc` on ISSUE_ADVANCED search sometimes returns results out of order (seen on shasta and brt). The order checks in grenadine/server/src/bin/validate_inboxes.rs fail on that; it isn't a grenadine bug.
