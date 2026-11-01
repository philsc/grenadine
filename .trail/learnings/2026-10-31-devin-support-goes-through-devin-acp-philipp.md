---
title: "Devin support goes through devin acp, not devin -p, with the backend chosen per session"
author: Philipp
date: 2026-10-31
id: l-acf52673
scope: repo
status: active
src: agent
touches:
  - grenadine/server/src/agent.rs
  - grenadine/server/src/claude.rs
  - grenadine/core/src/api.rs
---

## Decided

- Agent backend (Claude Code or Devin) is chosen per session when it's created and never changes; a backend other than the default (Claude) is offered only if listing its models works (--devin flag, default `devin`, next to --claude). Each backend's listing is kept in a `ModelList`: success for good, failure for five minutes, so a hanging `devin` doesn't hold up requests. `Agents::offers` asks only the requested backend and always takes a prompt that keeps its default model. A draft whose backend isn't offered any more switches to the first one that is.
- Devin runs as one `devin acp` (Agent Client Protocol, JSON-RPC over stdio) process per turn, like `claude -p` per turn: initialize, session/new or session/load, session/prompt, exit. Interrupts use session/cancel.
- Devin assigns its own session ids (e.g. "relic-boron"), so grenadine keeps its UUID for worktree/branch/slice/MCP URL and stores the backend's session id separately.
- Approvals come as session/request_permission and grenadine answers them directly; the MCP `approve` tool stays Claude-only. Both backends wait through `agent::await_approval`, which forgets the request however the wait ends; Devin's waits run on a JoinSet the turn owns. Devin's auto-allowed tools are `mcp::ALLOWED_TOOLS`, the same as Claude's `--allowedTools`. An approval that picks one of Devin's options takes `allow` from the option's `ApprovalKind`. The terminal/show_image MCP server is passed in session/new (HTTP is supported).
- The Devin adapter maps tools onto the names the UI already knows (exec->Bash, edit->Edit, write->Write, read->Read, ask_user_question->AskUserQuestion, exit_plan_mode->ExitPlanMode) and computes patch hunks from oldText/newText. Unknown tools pass through.
- Permission modes are per-backend lists (Devin: accept-edits, smart, ask, plan, bypass), kept as static tables on `AgentBackend` in core (`modes`, `default_mode`, `after_plan`) that the server and the web UI both read; `/api/agents/backends` only serves the models. The approval card shows every option Devin sends, including persistent "always allow" and "switch to bypass".
- Plan feedback: answer reject_once, then queue the feedback as the next prompt in plan mode (a Devin rejection carries no text).
- Cost is stored with its unit (`AgentBackend::cost_unit`): USD for Claude, ACU for Devin (`totalAcuCost`), no conversion. Devin's ACUs are billed as each usage update reports them (`TurnSink::bill`), so interrupted and failed turns pay too.
- `devin acp` messages are typed serde envelopes (Request / Response / Notification, `SessionUpdate` tagged by `sessionUpdate` with an `Other` variant); `_meta` stays a `Value`. One `Rpc::spawn` runs every ACP client. The agent-client-protocol crate wasn't adopted.
- Tests fake Devin with one scenario-driven script, //grenadine/testing:fake_devin, which the server tests and the UI test both run (FAKE_DEVIN env var).
- Each backend is one impl of the server's `Backend` trait (models, turn, structured) plus one arm in `Agents::backend`. The process plumbing any backend needs (systemd slice, setsid, stderr tail, stopping on interrupt or shutdown) is in agent.rs: `backend_command`, `stderr_tail`, `Stopper`, `OutputLines`.
- Background jobs follow the session's backend. Devin jobs: models from flags (summary/scan vs prune), schema in the prompt with no retry, ACU budgets enforced by cancelling once totalAcuCost goes over, `ask` mode plus the existing read-only allowlist for scans. Scans read only grenadine's digest, so the Devin adapter also records subagent tool calls.
- Delivery: (1) backend seam refactor with no behaviour change, (2) Devin turns plus the UI picker, (3) Devin background jobs.

## Tried and ruled out

- `devin -p`: plain text only, no stream-json or JSON schema output, so it can't drive a transcript.
- Pointing Devin cleanup scans at Devin's own storage: sessions live in a private sqlite db, and transcripts/\*.json isn't written for ACP sessions.

## Watch out

- session/load replays the whole conversation as session/update notifications before it returns. Ignore every update until the load response arrives, or each turn duplicates the transcript.
- Devin's accept-edits mode auto-runs commands it judges read-only (echo, ls, cat) without a permission request.
- Unverified: whether "allow for this session" survives a new process plus session/load, and how Devin subagent events show up over ACP.
- Unverified: whether `cognition.ai/totalAcuCost` is a total per `devin acp` process or for the whole session. Billing assumes per process, as it starts from 0 every turn; if it's session-wide, each resumed turn bills the earlier ones again. Check with a real two-turn run.
- Unverified: what `cognition.ai/permissionRule` holds for an MCP tool. The auto-allow matches it, or the tool's inference name, against `mcp::ALLOWED_TOOLS` exactly; if neither is the bare tool name, those tools get asked about.
- ACP loads the user's global Devin MCP servers, rules, skills and plugins.

In the code: grenadine/server/src/agent.rs (Backend, Agents::backend, backend_command, Stopper, TurnSink), claude.rs (turn, parse_line, list_models, structured), mcp.rs (approve), summary.rs, cleanup.rs (ask, digest, transcript_path), core/src/api.rs (AgentBackend, PermissionMode, AgentSession.cost and cost_unit). Probe scripts and recorded ACP traces: /tmp/acp-probe/ (not in the repo).
