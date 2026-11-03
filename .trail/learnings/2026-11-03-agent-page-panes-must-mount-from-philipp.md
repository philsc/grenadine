---
title: "Agent page panes must mount from narrow memos, not the whole session, or they rebuild and lose UI state"
author: Philipp
date: 2026-11-03
branch: grenadine/agent/90ac331b
id: l-c373d04a
scope: repo
status: active
src: agent
touches:
  - grenadine/testing/ui_test.py
  - grenadine/web/src/agents.rs
  - grenadine/web/src/diffview.rs
---

## Decided

The Unstaged changes pane's mount closure in the agent view reads a dedicated `Memo<bool>` (is this a general session?) rather than the whole session memo. The session's fields (status, cost, context tokens, unstaged count, PRs) change constantly while an agent works, and any closure that reads the whole session re-runs and recreates its child components from scratch. Before the fix, every update rebuilt the pane, flashed "Loading…", and reopened every file the user had folded.

File folds in that pane are remembered per path, per agent session, in the tab's sessionStorage. A fold holds even when the agent edits the file. That was the user's choice: a file they hid should stay hidden.

## Watch out

WorktreeDiff keys files by a hash of their contents, so a changed file always gets a fresh FileView. Any per-file UI state that should outlive edits has to be kept outside FileView, the way `Folds` is.

In the code: grenadine/web/src/agents.rs (AgentView `general` memo), grenadine/web/src/diffview.rs (`Folds`, key `worktree-folds.<session id>`), test in grenadine/testing/ui_test.py `test_unstaged_changes_pane`.
