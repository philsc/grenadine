---
title: "Devin turn costs come from agent_stopped, and Devin edits carry no diff hunks"
author: Philipp
date: 2026-10-31
branch: dev/philipp.schrader/devin-jobs
id: l-f2520f8d
scope: repo
status: active
src: agent
touches:
  - grenadine/server/src/devin.rs
---

## Decided

- A Devin turn's ACU cost is `acuCost` from the `_cognition.ai/agent_stopped` notification. The root `usage_update` `totalAcuCost` leaves out subagents (a probe with one subagent showed 0.0071 vs 0.0085 ACU). That total is only the fallback, plus the live budget check for background jobs.
- Devin tool results get no patch hunks. Devin's `diff` content has no line numbers, and the UI already renders Edit/Write from the tool input (old_string/new_string, content).

## Watch out

- Devin repeats a tool's full output across tool_call_update messages, so keep the latest non-empty text and never append.
- Subagent tool calls carry `_meta."cognition.ai/subagent_context".parentAgentId` other than "root" and never get tool_call_updates.
- Devin asks permission for HTTP MCP tools with permissionRule `mcp__<server>__<tool>`. Grenadine auto-allows its own `mcp__grenadine__*` tools.
- A `session/cancel` sent before a prompt is running is ignored, so an early interrupt has to skip the prompt.
- Devin has no equivalent of Claude's `--setting-sources user`, so a scan run in a clone loads that repo's Devin rules and hooks.

In the code: grenadine/server/src/devin.rs (Updates::update, run_turn, structured), cleanup.rs (allowed_command).
