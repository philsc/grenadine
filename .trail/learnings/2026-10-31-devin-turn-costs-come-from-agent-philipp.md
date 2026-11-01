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

- A Devin turn's ACU cost is the larger of `stats.acuCost` from the root's `_cognition.ai/agent_stopped` notification and the root `usage_update` `totalAcuCost`. The root total leaves out subagents (a probe with one subagent showed 0.0071 vs 0.0085 ACU). Both are billed through `TurnSink::bill`, which adds only an increase and never takes cost back, and TurnEnded carries the same total.
- The probes don't show whether `agent_stopped` comes before or after the session/prompt response, so a turn in which a subagent reported anything waits up to 500 ms (`STOPPED_WAIT`) for it after the response. Without subagents the root total is the whole cost and nothing waits.
- A background job's live budget is the sum of the highest `totalAcuCost` each agent (root and every subagent, by `parentAgentId`) has reported, since the root's leaves the subagents out.
- Devin tool results get no patch hunks. Devin's `diff` content has no line numbers, and the UI already renders Edit/Write from the tool input (old_string/new_string, content).

## Watch out

- Devin repeats a tool's full output across tool_call_update messages, so keep the latest non-empty text and never append.
- Subagent tool calls carry `_meta."cognition.ai/subagent_context".parentAgentId` other than "root" and never get tool_call_updates. They're recorded as `ToolUse { parent }`, hidden in the transcript, and don't end the root's answer in progress. `SessionUpdate::subagent` reads the parent.
- Unverified: whether a subagent's `totalAcuCost` is its own running total (the job budget assumes so), and whether subagents send `agent_stopped` of their own. One whose `_meta` has a non-root `subagent_context` is ignored, assuming it's marked like updates are.
- Devin asks permission for HTTP MCP tools with permissionRule `mcp__<server>__<tool>`. Grenadine auto-allows its own `mcp__grenadine__*` tools.
- A `session/cancel` sent before a prompt is running is ignored, so an early interrupt has to skip the prompt.
- Devin has no equivalent of Claude's `--setting-sources user`, so a scan run in a clone loads that repo's Devin rules and hooks. Jobs without a clone run in a fresh tempdir under a parent only this user can use, since a shared one like /tmp could hold another user's rules and hooks.

In the code: grenadine/server/src/devin.rs (Updates::update, TurnCtx::handle, run_turn, JobRun, structured), cleanup.rs (allowed_command, READ_ONLY_COMMANDS).
