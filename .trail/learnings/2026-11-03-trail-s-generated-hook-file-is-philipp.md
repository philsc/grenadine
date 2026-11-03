---
title: "Trail's generated hook file is excluded from formatting so new checkouts stay clean"
author: Philipp
date: 2026-11-03
branch: grenadine/agent/8505b015
id: l-8de7f1ea
scope: repo
status: active
src: agent
touches:
  - ".prettierignore"
---

## Decided

- Trail's SessionStart hook rewrites its wiring (including the hook script) whenever its wiring stamp is missing or out of date. The stamp lives in trail's git-ignored cache, so every new checkout or grenadine worktree is missing one, and the hook script gets rewritten when the first session starts.
- The committed hook script is trail's output byte for byte, and prettier skips it. When the committed copy was prettier-formatted, every new worktree showed a formatting-only diff in it.

## Watch out

- Don't run the formatter over trail-generated files, and don't remove the ignore entry.
- After upgrading trail, the template changes and the next session shows a real diff in the hook script. Commit it once. Machines on different trail versions will keep rewriting it to their own version's template.

In the code: .prettierignore (entry for .trail/hook.cjs), .trail/hook.cjs, stamp at .trail/cache/.cache/wiring-stamp.json, logic in trail's dist/upkeep.js reconcileWiring().
