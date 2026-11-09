"""Browser tests for grenadine's web UI.

Each test class starts the real server against a fake GitHub (see
fake_github.py) that it loads with PRs first. A stub `gh` hands the server
the fake's token, and a fake `claude` plays the agent.

pytest runs the tests, spread across Bazel's shards. A shard starts the
server only for the classes it has tests from.
"""

import base64
import http.client
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from collections.abc import Callable
from pathlib import Path
from typing import Any, ClassVar

import pytest_bazel
from playwright.sync_api import Dialog, Locator, expect, sync_playwright

from grenadine.testing import scenarios
from grenadine.testing.fake_github import (
    APPROVED,
    COMMENTED,
    TEAM,
    TEAM_SLUG,
    TOKEN,
    CheckRun,
    FakeGitHub,
    PullRequest,
    Review,
)

# How long the server may take to start listening.
STARTUP_TIMEOUT = 30
# The first poll searches and syncs every PR of the class's scenario,
# which takes a while on a loaded machine.
FIRST_POLL_TIMEOUT = 120
# Playwright's 5s default is too tight for the server's work (agent
# turns, wrap-ups, polls) when many test shards share a machine.
expect.set_options(timeout=15_000)


def devin_scenario(tmp: Path) -> dict:
    """What the fake `devin acp` (//grenadine/testing:fake_devin) plays: a
    turn that asks to run a command with Devin's options, a question when
    the prompt asks for one, and in plan mode a plan from a plan file with
    front matter. Resuming a session replays an answer, which the
    transcript must not show again. A subagent's tool call comes with the
    command, which the transcript doesn't show. The background jobs of a
    Devin session get a summary, no cleanup items and no stale ones. It
    logs what grenadine sends to `devin_acp.log` in `tmp`."""

    def update(u: dict) -> dict:
        return {"update": u}

    def tool_call(kind: str, raw_input: dict, **meta: str) -> dict:
        return update(
            {
                "sessionUpdate": "tool_call",
                "toolCallId": f"{kind}:{{turn}}",
                "rawInput": raw_input,
                "_meta": {"cognition.ai/inferenceToolName": kind, **meta},
            }
        )

    def done(kind: str, text: str) -> dict:
        return update(
            {
                "sessionUpdate": "tool_call_update",
                "toolCallId": f"{kind}:{{turn}}",
                "status": "completed",
                "content": [
                    {"type": "content", "content": {"type": "text", "text": text}}
                ],
            }
        )

    def permission(kind: str, options: list[tuple[str, str, str]]) -> dict:
        return {
            "request": "session/request_permission",
            "params": {
                "sessionId": "devin-test",
                "toolCall": {"toolCallId": f"{kind}:{{turn}}"},
                "options": [
                    {"optionId": id, "name": name, "kind": kind}
                    for id, name, kind in options
                ],
            },
        }

    plan_file = str(tmp / "devin_plan.md")
    question = {
        "question": "Tea or coffee?",
        "header": "Drink",
        "options": [
            {"label": "Tea", "description": "Tea"},
            {"label": "Coffee", "description": "Coffee"},
        ],
    }
    schema = {
        "type": "object",
        "properties": {
            "q0": {
                "title": "Drink",
                "description": "Tea or coffee?",
                "oneOf": [
                    {"const": "Tea", "title": "Tea"},
                    {"const": "Coffee", "title": "Coffee"},
                ],
            }
        },
    }
    multi_question = {
        "question": "Which drinks?",
        "header": "Drinks",
        "multi_select": True,
        "options": [
            {"label": "Tea", "description": "Tea"},
            {"label": "Coffee", "description": "Coffee"},
        ],
    }
    multi_schema = {
        "type": "object",
        "properties": {
            "q0": {
                "type": "array",
                "title": "Drinks",
                "description": "Which drinks?",
                "minItems": 1,
                "items": {
                    "anyOf": [
                        {"const": "Tea", "title": "Tea"},
                        {"const": "Coffee", "title": "Coffee"},
                    ]
                },
            }
        },
    }
    end = {"end": "end_turn"}

    def reply(answer: dict) -> list:
        text = {"type": "text", "text": json.dumps(answer)}
        return [update({"sessionUpdate": "agent_message_chunk", "content": text}), end]

    return {
        "log": str(tmp / "devin_acp.log"),
        "session_id": "devin-test",
        "model": "swe-2",
        "models": [
            {"value": "swe-2", "name": "SWE-2"},
            {"value": "swe-2-high", "name": "SWE-2 High"},
        ],
        "load": [
            {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "Replayed."},
            }
        ],
        "plan_file": {
            "path": plan_file,
            "text": "---\nagent: devin\n---\n# The plan\n\nDo the thing.\n",
        },
        "prompts": [
            {
                "match": "Summarize it as a title",
                "steps": reply({"summary": "A Devin task"}),
            },
            {
                "match": "A coding agent session was just deleted",
                "steps": reply({"items": []}),
            },
            {
                "match": "Find the items that can be deleted",
                "steps": reply({"stale": []}),
            },
            {
                "mode": "plan",
                "steps": [
                    tool_call(
                        "exit_plan_mode",
                        {"plan": "summary"},
                        **{"cognition.ai/planFilePath": plan_file},
                    ),
                    permission(
                        "exit_plan_mode",
                        [
                            (
                                "plan_accept_edits",
                                "Yes, implement and accept edits",
                                "allow_once",
                            ),
                            ("plan_bypass", "Yes, implement and bypass", "allow_once"),
                            ("reject_once", "No", "reject_once"),
                        ],
                    ),
                    done("exit_plan_mode", "ok"),
                    end,
                ],
            },
            {
                "match": "Review pull request",
                "steps": [
                    update(
                        {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": "Reviewed."},
                        }
                    ),
                    update(
                        {
                            "sessionUpdate": "usage_update",
                            "used": 1000,
                            "size": 200000,
                            "_meta": {"cognition.ai/totalAcuCost": 0.05},
                        }
                    ),
                    end,
                ],
            },
            {
                "match": "several",
                "steps": [
                    tool_call("ask_user_question", {"questions": [multi_question]}),
                    {
                        "request": "elicitation/create",
                        "params": {
                            "mode": "form",
                            "sessionId": "devin-test",
                            "requestedSchema": multi_schema,
                            "message": "Which drinks?",
                            "_meta": {
                                "cognition.ai/toolCallId": "ask_user_question:{turn}"
                            },
                        },
                    },
                    done("ask_user_question", "Tea, Coffee"),
                    end,
                ],
            },
            {
                "match": "ponder",
                "steps": [
                    update(
                        {
                            "sessionUpdate": "agent_thought_chunk",
                            "content": {
                                "type": "text",
                                "text": "Pondering the answer",
                            },
                        }
                    ),
                    update(
                        {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": "Pondered."},
                        }
                    ),
                    end,
                ],
            },
            {
                "match": "question",
                "steps": [
                    tool_call("ask_user_question", {"questions": [question]}),
                    {
                        "request": "elicitation/create",
                        "params": {
                            "mode": "form",
                            "sessionId": "devin-test",
                            "requestedSchema": schema,
                            "message": "Tea or coffee?",
                            "_meta": {
                                "cognition.ai/toolCallId": "ask_user_question:{turn}"
                            },
                        },
                    },
                    done("ask_user_question", "Tea"),
                    end,
                ],
            },
            {
                "steps": [
                    update(
                        {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": "On it."},
                        }
                    ),
                    tool_call("exec", {"command": "touch note.txt"}),
                    update(
                        {
                            "sessionUpdate": "tool_call",
                            "toolCallId": "exec:{turn}-sub",
                            "rawInput": {"command": "echo subagent-marker"},
                            "_meta": {
                                "cognition.ai/inferenceToolName": "exec",
                                "cognition.ai/subagent_context": {
                                    "parentAgentId": "sub1"
                                },
                            },
                        }
                    ),
                    permission(
                        "exec",
                        [
                            ("allow_once", "Allow", "allow_once"),
                            ("reject_once", "Reject", "reject_once"),
                        ],
                    ),
                    done("exec", "done"),
                    update(
                        {
                            "sessionUpdate": "usage_update",
                            "used": 1000,
                            "size": 200000,
                            "_meta": {"cognition.ai/totalAcuCost": 0.5},
                        }
                    ),
                    end,
                ],
            },
        ],
    }


# Stands in for `claude -p`: it writes a short turn as stream-json and,
# when the prompt asks it to write, asks grenadine for permission through
# the MCP tool first, like Claude Code does. Permission modes other than
# manual allow the write without asking. It runs the model `--model` names,
# lists its models when asked like the Agent SDK does, summarizes a new
# session's request as "Task: " and its first line, answers the scans of
# deleted sessions with a container they left behind, and finds no stale
# cleanup items. `commit NAME` has it commit NAME.txt as "Add NAME",
# `drop NAME` rebases that commit away, `swap` trades the top two
# commits, `rebase N` rebases the top N commits onto the pushed main's
# tip, and `update prs ENTRY...` calls the update_prs MCP tool, ENTRY
# naming a commit as NAME, or NAME:N to update PR #N. The Open PR
# wrap-up's naming prompt makes it call update_prs too, naming each new
# pull request's branch dev/test/<subject>. `hang up` has it ask to run
# a Bash command and hang up before anyone answers. It says how many
# images came with the prompt. `long transcript N` has it run N Bash
# calls whose results grow from ~1 KB to ~30 KB and start with
# "result i:", so a page can tell them and which call they answer apart.
FAKE_CLAUDE = r"""
import base64
import json
import subprocess
import sys
import time
import urllib.request
import uuid

args = sys.argv[1:]
# Listing models is the only call without an MCP server.
if "--input-format" in args and "--mcp-config" not in args:
    request = json.loads(sys.stdin.readline())
    models = [
        {"value": "default", "displayName": "Default (recommended)", "description": "claude-test"},
        {"value": "sonnet", "displayName": "Sonnet", "description": "Efficient"},
        {"value": "haiku", "displayName": "Haiku", "description": "Fast"},
    ]
    response = {"subtype": "success", "request_id": request["request_id"], "response": {"models": models}}
    print(json.dumps({"type": "control_response", "response": response}), flush=True)
    sys.exit()
if "--json-schema" in args:
    schema = json.loads(args[args.index("--json-schema") + 1])
    request = sys.stdin.read()
    if "summary" in schema["properties"]:
        first = request.split("# Request\n\n", 1)[1].strip().splitlines()[0]
        output = {"summary": "Task: " + first}
    elif "stale" in schema["properties"]:
        output = {"stale": []}
    else:
        item = {"title": "Container e2e", "detail": "Remove it with `docker rm -f e2e`."}
        output = {"items": [item]}
    print(json.dumps({"type": "result", "is_error": False, "structured_output": output}), flush=True)
    sys.exit()
import os
import re
content = json.loads(sys.stdin.read())["message"]["content"]
images = sum(block["type"] == "image" for block in content)
prompt = "".join(block["text"] for block in content if block["type"] == "text")
url = json.loads(args[args.index("--mcp-config") + 1])["mcpServers"]["grenadine"]["url"]
mode = args[args.index("--permission-mode") + 1]
model = args[args.index("--model") + 1] if "--model" in args else "claude-test"


def emit(message):
    message.setdefault("parent_tool_use_id", None)
    print(json.dumps(message), flush=True)


def rpc(method, params, id=1, timeout=None):
    request = urllib.request.Request(
        url,
        data=json.dumps({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.load(response)


def use(name, tool_input):
    tool_use = "toolu_" + uuid.uuid4().hex
    emit({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": tool_use, "name": name, "input": tool_input}]}})
    return tool_use


def approve(name, tool_input, tool_use):
    rpc("initialize", {"protocolVersion": "2025-11-25"}, id=0)
    answer = rpc("tools/call", {"name": "approve", "arguments": {"tool_name": name, "input": tool_input, "tool_use_id": tool_use}})
    return json.loads(answer["result"]["content"][0]["text"])


def result(tool_use, content, is_error=False, patch=None):
    line = {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tool_use, "content": content, "is_error": is_error}]}}
    if patch:
        line["tool_use_result"] = {"structuredPatch": patch}
    emit(line)


emit({"type": "system", "subtype": "init", "session_id": "s", "model": model})
for word in ["Working ", "on ", "it"]:
    emit({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": word}}})
    time.sleep(0.05)
usage = {"input_tokens": 3, "cache_creation_input_tokens": 1000, "cache_read_input_tokens": 40000, "output_tokens": 500}
emit({"type": "assistant", "message": {"content": [{"type": "text", "text": f"You said: **{prompt}**"}], "usage": usage}})
if images:
    emit({"type": "assistant", "message": {"content": [{"type": "text", "text": f"I see {images} images."}]}})
# Grenadine asks it to commit before landing its work.
if prompt.startswith("Wrap up: commit"):
    subprocess.run(["git", "add", "-A"], check=True)
    subprocess.run(["git", "commit", "--quiet", "-m", "Write notes"], check=True)
if "write" in prompt:
    tool_input = {"file_path": "notes.txt", "content": prompt}
    tool_use = use("Write", tool_input)
    decision = {"behavior": "allow"}
    if mode == "manual":
        decision = approve("Write", tool_input, tool_use)
    if decision["behavior"] == "allow":
        with open("notes.txt", "w") as f:
            f.write(prompt)
        # Claude Code reports where in the file the change went.
        patch = [{"oldStart": 1, "oldLines": 0, "newStart": 1, "newLines": 1, "lines": ["+" + prompt]}]
        result(tool_use, "Wrote notes.txt", patch=patch)
    else:
        result(tool_use, decision["message"], is_error=True)
# `hang up` has it ask to run a Bash command and hang up before anyone
# answers.
if prompt == "hang up":
    tool_input = {"command": "true"}
    tool_use = use("Bash", tool_input)
    rpc("initialize", {"protocolVersion": "2025-11-25"}, id=0)
    try:
        rpc("tools/call", {"name": "approve", "arguments": {"tool_name": "Bash", "input": tool_input, "tool_use_id": tool_use}}, timeout=3)
    except TimeoutError:
        pass
    result(tool_use, "Nobody answered.", is_error=True)
if prompt.startswith("commit "):
    name = prompt.removeprefix("commit ")
    with open(name + ".txt", "w") as f:
        f.write(name)
    subprocess.run(["git", "add", "-A"], check=True)
    subprocess.run(["git", "commit", "--quiet", "-m", "Add " + name], check=True)
# `open pr URL` has it open a PR with gh, which prints the PR's URL.
if prompt.startswith("open pr "):
    tool_use = use("Bash", {"command": "gh pr create --fill"})
    result(tool_use, prompt.removeprefix("open pr ") + "\n")


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout.strip()


def sha_of(name):
    # The sha of the commit whose subject is `Add NAME`.
    log = git("log", "--format=%H %s")
    return next(l.split(" ", 1)[0] for l in log.splitlines() if l.split(" ", 1)[1] == "Add " + name)


# `drop NAME` removes the commit "Add NAME" from the worktree's history.
if prompt.startswith("drop "):
    sha = sha_of(prompt.removeprefix("drop "))
    git("rebase", "--onto", sha + "^", sha)
# `swap` trades the top two commits.
if prompt == "swap":
    top = git("log", "-2", "--format=%H").splitlines()
    git("reset", "--hard", "HEAD~2")
    git("cherry-pick", top[0], top[1])
# `rebase N` rebases the top N commits onto the pushed main's tip.
if prompt.startswith("rebase "):
    git("fetch", "--quiet", "origin", "main")
    git("rebase", "--onto", "FETCH_HEAD", "HEAD~" + prompt.removeprefix("rebase "))
def update_prs(arguments):
    tool_use = use("mcp__grenadine__update_prs", arguments)
    rpc("initialize", {"protocolVersion": "2025-11-25"}, id=0)
    # The id becomes the approval's key, so a repeat call needs a new one.
    updated = rpc(
        "tools/call",
        {"name": "update_prs", "arguments": arguments},
        id=tool_use,
    )
    result(tool_use, updated["result"]["content"][0]["text"], is_error=updated["result"].get("isError", False))


# `update prs ENTRY...` calls the update_prs MCP tool, ENTRY naming an
# "Add NAME" commit, or NAME:N to update PR #N.
if prompt.startswith("update prs"):
    prs = []
    for entry in prompt.removeprefix("update prs ").split():
        name, _, pr = entry.partition(":")
        spec = {"commit": sha_of(name)}
        if pr:
            spec["pr"] = int(pr)
        prs.append(spec)
    update_prs({"prs": prs})
# The Open PR wrap-up hands the new pull requests' branch names to the
# agent: each new one is named dev/test/<subject>, and the mapping the
# prompt carries goes to update_prs.
if prompt.startswith("Wrap up: call the grenadine MCP server's update_prs"):
    prs = json.loads(re.search(r"set to `(\[.*?\])`", prompt)[1])
    for spec in prs:
        if "pr" not in spec:
            subject = git("log", "-1", "--format=%s", spec["commit"])
            spec["branch"] = "dev/test/" + re.sub(r"[^a-z0-9]+", "-", subject.lower()).strip("-")
    update_prs({"prs": prs})
# `screenshot` has it read a screenshot, whose result carries the image,
# and then show it to the user.
if prompt == "screenshot":
    png = ("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGA"
           "hKmMIQAAAABJRU5ErkJggg==")
    with open("shot.png", "wb") as f:
        f.write(base64.b64decode(png))
    tool_use = use("Read", {"file_path": "shot.png"})
    image = {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png}}
    result(tool_use, [image])
    arguments = {"path": "shot.png", "caption": "The page"}
    tool_use = use("mcp__grenadine__show_image", arguments)
    rpc("initialize", {"protocolVersion": "2025-11-25"}, id=0)
    shown = rpc("tools/call", {"name": "show_image", "arguments": arguments})
    result(tool_use, shown["result"]["content"][0]["text"])
# `question the plan` asks a question whose text is a plan: a title, then
# Markdown. The answer goes back keyed by the whole question text.
if prompt == "question the plan":
    question = "Implement this plan?\n\n## Steps\n\n- Change **one** thing\n- Test it"
    tool_input = {"questions": [{
        "question": question,
        "header": "Implement",
        "options": [{"label": "Go ahead", "description": ""}, {"label": "Not yet", "description": ""}],
        "multiSelect": False,
    }]}
    tool_use = use("AskUserQuestion", tool_input)
    name = approve("AskUserQuestion", tool_input, tool_use)["updatedInput"]["answers"][question]
    result(tool_use, f'"{question}"="{name}"')
# In plan mode, it asks which file to write, then has its plan approved,
# revising it with the feedback until it is. The file gets the mode the
# approval switches to.
if mode == "plan":
    question = "Which file?"
    tool_input = {"questions": [{
        "question": question,
        "header": "File",
        "options": [{"label": "notes.txt", "description": "The usual"}, {"label": "todo.txt", "description": ""}],
        "multiSelect": False,
    }]}
    tool_use = use("AskUserQuestion", tool_input)
    name = approve("AskUserQuestion", tool_input, tool_use)["updatedInput"]["answers"][question]
    result(tool_use, f'"{question}"="{name}"')
    plan = f"# Plan\n\nWrite **{name}**."
    while True:
        tool_use = use("ExitPlanMode", {"plan": plan})
        decision = approve("ExitPlanMode", {"plan": plan}, tool_use)
        if decision["behavior"] == "allow":
            break
        result(tool_use, decision["message"], is_error=True)
        plan += f"\n\nAlso: {decision['message']}"
    result(tool_use, "User has approved your plan.")
    with open(name, "w") as f:
        f.write(decision["updatedPermissions"][0]["mode"])
# A review of a PR posts a comment on it, as `gh` would, unless the PR
# asks it not to.
review = re.search(r"Review pull request \S+: \"([^\"]*)\" \(([^()\s]+/[^()\s]+)#(\d+)\)", prompt)
if review and "Quiet" not in review[1]:
    request = urllib.request.Request(
        f"{os.environ['FAKE_GITHUB_API']}/repos/{review[2]}/issues/{review[3]}/comments",
        data=json.dumps({"body": "Looks good."}).encode(),
        headers={"content-type": "application/json", "authorization": "Bearer " + os.environ["FAKE_GITHUB_TOKEN"]},
    )
    urllib.request.urlopen(request).close()
# `sleep N` stalls the turn for N seconds, holding the session running.
if prompt.startswith("sleep "):
    time.sleep(float(prompt.removeprefix("sleep ")))
# `long transcript N` runs N Bash calls; the results pad made-up output
# from ~1 KB to ~30 KB and start with "result i:" so which call each
# answers is checkable.
long = re.fullmatch(r"long transcript (\d+)", prompt)
if long:
    count = int(long[1])
    for i in range(count):
        tool_use = use("Bash", {"command": f"step {i}"})
        size = 1024 + (29 * 1024) * i // max(count - 1, 1)
        padding = ("0123456789abcdef" * ((size // 16) + 1))[:size]
        result(tool_use, f"result {i}: {padding}")
emit({"type": "result", "subtype": "success", "is_error": False, "result": "", "total_cost_usd": 0.01, "modelUsage": {"claude-test": {"contextWindow": 200000}}})
"""


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def exactly(text: str) -> re.Pattern[str]:
    return re.compile(f"^{re.escape(text)}$")


def expect_tab_count(count: Locator, n: int) -> None:
    # The count's span isn't rendered at zero.
    if n:
        expect(count).to_have_text(str(n))
    else:
        expect(count).to_have_count(0)


class ServerTestCase(unittest.TestCase):
    """Runs the server and a browser for the class's tests."""

    # The repositories the server is started with; `load` creates them.
    REPOS: ClassVar[list[str]] = [scenarios.REPO]
    # Seconds between the server's polls of GitHub.
    POLL_INTERVAL: ClassVar[int] = 60

    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        """Adds the PRs the tests expect; there are none by default."""
        gh.repo(scenarios.REPO)

    @classmethod
    def setUpClass(cls) -> None:
        tmp = Path(tempfile.mkdtemp(dir=os.environ.get("TEST_TMPDIR")))

        cls.gh = FakeGitHub(tmp / "github")
        cls.load(cls.gh)
        api = cls.gh.start()
        cls.addClassCleanup(cls.gh.stop)

        bin_dir = tmp / "bin"
        bin_dir.mkdir()
        gh = bin_dir / "gh"
        gh.write_text(f"#!/bin/sh\necho {TOKEN}\n")
        gh.chmod(0o755)
        claude = bin_dir / "claude"
        claude.write_text(f"#!{sys.executable}\n{FAKE_CLAUDE}")
        claude.chmod(0o755)
        scenario = tmp / "devin_scenario.json"
        scenario.write_text(json.dumps(devin_scenario(tmp)))
        devin = bin_dir / "devin"
        fake_devin = Path(os.environ["FAKE_DEVIN"]).absolute()
        devin.write_text(f'#!/bin/sh\nexec {fake_devin} --scenario={scenario} "$@"\n')
        devin.chmod(0o755)

        env = dict(
            cls.gh.git_env(),
            PATH=f"{bin_dir}:{os.environ['PATH']}",
            # Where the fake claude posts reviews.
            FAKE_GITHUB_API=api,
            FAKE_GITHUB_TOKEN=TOKEN,
        )
        repo_flags = []
        cls.clones = {}
        for slug in cls.REPOS:
            clone = tmp / "clones" / slug
            cls.clones[slug] = clone
            subprocess.run(["git", "init", "--quiet", clone], env=env, check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    clone,
                    "remote",
                    "add",
                    "origin",
                    f"https://github.com/{slug}",
                ],
                env=env,
                check=True,
            )
            repo_flags.append(f"--repo={clone}")

        port = free_port()
        cls.url = f"http://127.0.0.1:{port}/"
        cls.devin_log = tmp / "devin_acp.log"
        # The database goes on tmpfs where there is one. Each of its
        # commits waits for an fsync, which on a remote executor whose disk
        # is busy with other actions' inputs held the server's startup for
        # 30s. The worktrees stay where the server would put them.
        db_dir = tmp
        if os.access("/dev/shm", os.W_OK):
            db_dir = Path(tempfile.mkdtemp(dir="/dev/shm"))
            cls.addClassCleanup(shutil.rmtree, db_dir, ignore_errors=True)
        cls.server_args = (
            [
                os.environ["GRENADINE_SERVER"],
                *repo_flags,
                f"--port={port}",
                f"--db={db_dir / 'grenadine.db'}",
                f"--worktrees={tmp / 'worktrees'}",
                f"--github-api={api}",
                f"--devin={devin}",
                f"--poll-interval={cls.POLL_INTERVAL}",
                # The fake GitHub shows posted reviews at once, so a review
                # that posts nothing needn't be looked for over 25s.
                "--review-post-checks=0,1",
                # The sandbox's user has no login shell.
                "--shell=/bin/sh",
            ],
            env,
        )
        cls.start_server()
        cls.addClassCleanup(cls.stop_server)
        # Tests assert state that only the first poll fills, so it must
        # have finished before the first one runs. The wait lives here
        # rather than in start_server because the restart test reuses
        # start_server without needing a poll to have finished.
        cls.wait_for_first_poll()

        cls.playwright = sync_playwright().start()
        cls.addClassCleanup(cls.playwright.stop)
        # Chrome's shared libraries and fonts come from @chrome_deps, not the
        # host. fontconfig has no default config there, so it gets one that
        # points at the fonts in the repository.
        chrome_env = dict(os.environ)
        lib_dir = os.path.dirname(os.path.abspath(chrome_env["CHROME_LIB_DIR"]))
        ld_library_path = chrome_env.get("LD_LIBRARY_PATH")
        chrome_env["LD_LIBRARY_PATH"] = (
            lib_dir + os.pathsep + ld_library_path if ld_library_path else lib_dir
        )
        repo = Path(lib_dir).parents[2]
        fonts_cache = tmp / "fonts_cache"
        fonts_conf = tmp / "fonts.conf"
        fonts_conf.write_text(
            '<?xml version="1.0"?>\n<fontconfig>\n'
            f"  <dir>{repo / 'usr' / 'share' / 'fonts'}</dir>\n"
            f"  <cachedir>{fonts_cache}</cachedir>\n"
            "</fontconfig>\n"
        )
        chrome_env["FONTCONFIG_FILE"] = str(fonts_conf)
        cls.browser = cls.playwright.chromium.launch(
            executable_path=os.environ["CHROME"], env=chrome_env
        )
        cls.addClassCleanup(cls.browser.close)

    @classmethod
    def start_server(cls) -> None:
        args, env = cls.server_args
        cls.server = subprocess.Popen(args, env=env)
        cls.wait_for_server()

    @classmethod
    def stop_server(cls) -> None:
        cls.server.terminate()
        cls.server.wait(timeout=15)

    @classmethod
    def wait_for_server(cls) -> None:
        deadline = time.monotonic() + STARTUP_TIMEOUT
        while True:
            if cls.server.poll() is not None:
                raise RuntimeError(f"server exited with {cls.server.returncode}")
            try:
                with urllib.request.urlopen(cls.url):
                    return
            except (urllib.error.URLError, ConnectionError):
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.1)

    @classmethod
    def wait_for_first_poll(cls) -> None:
        """Waits until the server's first GitHub poll has finished."""
        deadline = time.monotonic() + FIRST_POLL_TIMEOUT
        while True:
            with urllib.request.urlopen(f"{cls.url}api/sync") as resp:
                if json.load(resp)["last_finished"] is not None:
                    return
            if time.monotonic() > deadline:
                raise RuntimeError("the server's first poll never finished")
            time.sleep(0.1)

    def setUp(self) -> None:
        # Each page gets its own context, so local storage starts empty.
        self.page = self.browser.new_page()
        self.addCleanup(self.page.close)

    def inbox(self, name: str) -> Locator:
        return self.page.locator("section.inbox").filter(
            has=self.page.locator(".inbox-name", has_text=exactly(name))
        )

    def merge_box(self) -> Locator:
        return self.page.locator(".merge-box")

    def merge(self) -> list[str]:
        """Clicks Merge, accepting the dialog. Returns what it said."""
        messages: list[str] = []

        def accept(dialog: Dialog) -> None:
            messages.append(dialog.message)
            dialog.accept()

        self.page.once("dialog", accept)
        self.merge_box().get_by_role("button", name=re.compile("^Merge")).click()
        return messages

    def repoll(self) -> None:
        """Forces a poll by saving the first inbox unchanged."""
        with urllib.request.urlopen(f"{self.url}api/inboxes") as resp:
            inbox = json.load(resp)[0]["inbox"]
        req = urllib.request.Request(
            f"{self.url}api/inboxes/{inbox['id']}",
            data=json.dumps(inbox).encode(),
            method="PUT",
            # Writes are only taken from the server's own pages.
            headers={
                "Content-Type": "application/json",
                "Origin": self.url.rstrip("/"),
            },
        )
        urllib.request.urlopen(req).close()

    def refresh(self) -> None:
        """Asks the server to search every inbox on its next poll."""
        req = urllib.request.Request(
            f"{self.url}api/inboxes/refresh",
            data=b"",
            method="POST",
            # Writes are only taken from the server's own pages.
            headers={"Origin": self.url.rstrip("/")},
        )
        urllib.request.urlopen(req).close()

    def searched_at(self, name: str) -> int | None:
        """When `name`'s search last ran, from /api/inboxes."""
        with urllib.request.urlopen(f"{self.url}api/inboxes") as resp:
            for i in json.load(resp):
                if i["inbox"]["name"] == name:
                    return i["searched_at"]
        self.fail(f"no inbox {name!r}")

    def devin_saw(self, needle: str) -> None:
        # Waits until the fake devin's client log holds `needle`.
        for _ in range(200):
            if needle in self.devin_log.read_text():
                return
            time.sleep(0.05)
        self.fail(f"devin never saw {needle}: {self.devin_log.read_text()}")


class ChromeTest(ServerTestCase):
    def test_shows_logo_top_left(self) -> None:
        self.page.goto(self.url)
        logo = self.page.locator(".topbar-brand img")
        expect(logo).to_be_visible()
        expect(logo).to_have_attribute("alt", "grenadine")
        self.page.wait_for_function(
            "() => document.querySelector('.topbar-brand img').naturalWidth > 0"
        )

        box = logo.bounding_box()
        viewport = self.page.viewport_size
        assert box is not None and viewport is not None
        self.assertLess(box["x"], viewport["width"] / 4)
        self.assertLess(box["y"], 100)

    def test_theme_picker_switches_themes(self) -> None:
        # Auto follows the OS, so pin the OS to light to tell Auto from Dark.
        self.page.emulate_media(color_scheme="light")
        self.page.goto(self.url)
        html = self.page.locator("html")
        body = self.page.locator("body")
        self.page.get_by_role("button", name="Settings", exact=True).click()
        picker = self.page.get_by_label("Theme")
        expect(picker).to_have_value("auto")
        expect(html).to_have_attribute("data-theme", "light")

        picker.select_option("dark")
        expect(html).to_have_attribute("data-theme", "dark")
        expect(body).to_have_css("background-color", "rgb(13, 17, 23)")

        picker.select_option("light")
        expect(html).to_have_attribute("data-theme", "light")
        expect(body).to_have_css("background-color", "rgb(255, 255, 255)")

        # Auto tracks the OS's scheme as it changes.
        picker.select_option("auto")
        expect(html).to_have_attribute("data-theme", "light")
        self.page.emulate_media(color_scheme="dark")
        expect(html).to_have_attribute("data-theme", "dark")

    def test_theme_survives_reload(self) -> None:
        self.page.emulate_media(color_scheme="light")
        self.page.goto(self.url)
        self.page.get_by_role("button", name="Settings", exact=True).click()
        self.page.get_by_label("Theme").select_option("dark")

        self.page.reload()
        self.page.get_by_role("button", name="Settings", exact=True).click()
        expect(self.page.get_by_label("Theme")).to_have_value("dark")
        expect(self.page.locator("html")).to_have_attribute("data-theme", "dark")

    def test_settings_dialog_opens_and_closes(self) -> None:
        self.page.goto(self.url)
        self.page.get_by_role("button", name="Settings", exact=True).click()
        dialog = self.page.get_by_role("dialog", name="Settings", exact=True)
        expect(dialog).to_be_visible()

        # Escape closes it; a click beside it, on the backdrop, does too.
        self.page.keyboard.press("Escape")
        expect(dialog).to_have_count(0)
        self.page.get_by_role("button", name="Settings", exact=True).click()
        expect(dialog).to_be_visible()
        self.page.mouse.click(4, 4)
        expect(dialog).to_have_count(0)

    def test_inboxes_start_empty(self) -> None:
        self.page.goto(self.url)
        inboxes = self.page.locator("section.inbox")
        expect(inboxes).to_have_count(len(scenarios.default_inboxes()))
        expect(inboxes.locator(".none")).to_have_count(len(scenarios.default_inboxes()))
        expect(inboxes.locator(".count")).to_have_text(
            ["0"] * len(scenarios.default_inboxes())
        )


# The default inboxes that sync in background.
BACKGROUND_INBOXES = {
    name for name, _, _, background in scenarios.default_inboxes() if background
}


class InboxTest(ServerTestCase):
    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        cls.cases = scenarios.inbox_cases(gh)

    def row(self, title: str) -> Locator:
        # A PR can be in several inboxes; the first one will do.
        return (
            self.page.locator(".pr-list li")
            .filter(
                has=self.page.locator(
                    ".pr-title", has_text=re.compile(f"{re.escape(title)}$")
                )
            )
            .first
        )

    def expect_titles(self, inbox: Locator, titles: list[str]) -> None:
        """Checks that `inbox` lists exactly `titles`, in any order, each
        without errors. A row shows its synced details or its search
        data, never a stuck "syncing…"."""
        expect(inbox.locator(".count")).to_have_text(str(len(titles)))
        rows = inbox.locator(".pr-list li")
        expect(rows).to_have_count(len(titles))
        for title in titles:
            row = rows.filter(
                has=self.page.locator(
                    ".pr-title", has_text=re.compile(f"{re.escape(title)}$")
                )
            )
            expect(row).to_have_count(1)
            # A PR in a background inbox is synced by the polls; other
            # rows show the search's date, unless a test synced the PR.
            if set(self.cases.expected[title]) & BACKGROUND_INBOXES:
                meta = r"· 1 versions$"
            else:
                meta = r"· (1 versions|\d{4}-\d{2}-\d{2})$"
            expect(row.locator(".pr-meta")).to_have_text(re.compile(meta))

    def test_prs_land_in_their_inboxes(self) -> None:
        self.page.goto(self.url)
        for name, *_ in scenarios.default_inboxes():
            with self.subTest(inbox=name):
                self.expect_titles(self.inbox(name), self.cases.titles_in(name))

    def test_prs_in_no_inbox_are_not_shown(self) -> None:
        self.page.goto(self.url)
        # Wait for the first poll.
        expect(
            self.inbox(scenarios.NEEDS_REVIEW).locator(".pr-list li")
        ).not_to_have_count(0)
        for title, inboxes in self.cases.expected.items():
            if not inboxes:
                expect(self.row(title)).to_have_count(0)

    def test_badges(self) -> None:
        self.page.goto(self.url)
        expect(self.inbox(scenarios.DRAFTS).locator(".pr-title .badge")).to_have_text(
            "draft"
        )
        # The stack badges need the sync's stack data; the stack's PRs
        # are the viewer's, so the auto-review search syncs them.
        expect(
            self.inbox(scenarios.WAITING_FOR_REVIEWERS).locator(".pr-title .badge")
        ).to_have_count(2)
        expect(self.row(scenarios.STACK_BASE).locator(".badge")).to_have_text("1/2")
        expect(self.row(scenarios.STACK_TOP).locator(".badge")).to_have_text("2/2")
        expect(self.row("Wait for reviewers").locator(".badge")).to_have_count(0)

    def test_threads_and_ci(self) -> None:
        self.page.goto(self.url)
        threads = self.row("Wait for the author after my review").locator(
            ".pr-status .threads"
        )
        expect(threads).to_have_text("1/2")
        expect(threads).to_have_attribute("title", "1 of 2 threads resolved")
        expect(threads).not_to_have_class(re.compile(r"\bdone\b"))
        # A PR without threads shows no count.
        expect(
            self.row("Ask me for a review").locator(".pr-status .threads")
        ).to_have_count(0)

        for title, state in [
            ("Ask me for a review", "passing"),
            ("Ask my team for a review", "failing"),
            ("Wait for the author after my review", "running"),
        ]:
            with self.subTest(title=title):
                ci = self.row(title).locator(".pr-status .ci")
                expect(ci).to_have_class(re.compile(rf"\b{state}\b"))
                expect(ci).to_have_attribute("title", f"CI {state}")
        for title, state, label in [
            ("Get approved", "approved", "Approved"),
            ("Get changes requested", "changes", "Changes requested"),
            ("Ask me for a review", "required", "Review required"),
        ]:
            with self.subTest(title=title):
                review = self.row(title).locator(".pr-status .review")
                expect(review).to_have_class(re.compile(rf"\b{state}\b"))
                expect(review).to_have_attribute("title", label)
        # GitHub has no decision for a repository that doesn't require
        # reviews and a PR nobody reviewed.
        expect(
            self.row("Wait for reviewers").locator(".pr-status .review")
        ).to_have_count(0)
        # A PR without checks shows no CI icon.
        expect(self.row("Wait for reviewers").locator(".pr-status .ci")).to_have_count(
            0
        )

    def test_collapsing_an_inbox_survives_reload(self) -> None:
        self.page.goto(self.url)
        inbox = self.inbox(scenarios.APPROVED_INBOX)
        expect(inbox.locator(".pr-list")).to_be_visible()

        inbox.locator(".inbox-toggle").click()
        expect(inbox.locator(".pr-list")).to_be_hidden()
        self.page.reload()
        expect(inbox.locator(".pr-list")).to_be_hidden()
        expect(inbox.locator(".count")).to_have_text("1")

        inbox.locator(".inbox-toggle").click()
        expect(inbox.locator(".pr-list")).to_be_visible()

    def add_inbox(self, name: str, filter: str, counted: bool = False) -> Locator:
        self.addCleanup(self.delete_inbox, name)
        self.page.get_by_role("button", name="+ Add inbox").click()
        form = self.page.locator("form.inbox-form")
        form.get_by_label("Name").fill(name)
        form.get_by_label("GitHub filter").fill(filter)
        if counted:
            form.get_by_label("Count in total").check()
        form.get_by_role("button", name="Save").click()
        return self.inbox(name)

    def delete_inbox(self, name: str) -> None:
        """Deletes an inbox a test added, so that it doesn't show up in the
        other tests."""
        with urllib.request.urlopen(f"{self.url}api/inboxes") as resp:
            ids = [
                i["inbox"]["id"] for i in json.load(resp) if i["inbox"]["name"] == name
            ]
        for id in ids:
            req = urllib.request.Request(
                f"{self.url}api/inboxes/{id}",
                method="DELETE",
                # Writes are only taken from the server's own pages.
                headers={"Origin": self.url.rstrip("/")},
            )
            urllib.request.urlopen(req).close()

    def test_custom_inbox(self) -> None:
        self.page.goto(self.url)
        inbox = self.add_inbox(
            "By the other account", "state:open author:other sort:created-asc"
        )
        others = [
            title
            for title, pr in self.cases.prs.items()
            if pr.author == scenarios.OTHER and pr.state == "OPEN"
        ]
        self.expect_titles(inbox, others)
        # sort:created-asc reaches GitHub and orders the PRs.
        expect(inbox.locator(".pr-title")).to_have_text(
            [re.compile(f"{re.escape(t)}$") for t in others]
        )

        empty = self.add_inbox("Nobody's", "author:nobody")
        expect(empty.locator(".none")).to_have_text("No PRs")
        expect(empty.locator(".count")).to_have_text("0")

    def test_returned_sorts_by_title(self) -> None:
        self.page.goto(self.url)
        expect(self.inbox(scenarios.RETURNED).locator(".pr-title")).to_have_text(
            [
                re.compile("Get a comment-only review$"),
                re.compile("Get changes requested$"),
            ]
        )

    def test_re_request_moves_back_to_waiting(self) -> None:
        pr = self.cases.prs["Get a comment-only review"]
        self.page.goto(self.url)
        self.expect_titles(
            self.inbox(scenarios.RETURNED),
            ["Get a comment-only review", "Get changes requested"],
        )
        # Re-requesting the reviewer's review takes his review out of
        # "active", so the PR goes back to Waiting for reviewers.
        pr.review_requests.add(scenarios.OTHER)
        self.addCleanup(
            lambda: (pr.review_requests.discard(scenarios.OTHER), self.refresh())
        )
        # Waiting for reviewers doesn't sync in background; ask for a
        # search so the moved PR appears.
        self.refresh()
        self.expect_titles(
            self.inbox(scenarios.WAITING_FOR_REVIEWERS),
            [
                "Wait for reviewers",
                scenarios.STACK_BASE,
                scenarios.STACK_TOP,
                "Get a comment-only review",
            ],
        )
        self.expect_titles(self.inbox(scenarios.RETURNED), ["Get changes requested"])
        # Put it back how it was, in the fake and in the database, so
        # later tests see the scenario's state.
        pr.review_requests.discard(scenarios.OTHER)
        self.refresh()
        self.expect_titles(
            self.inbox(scenarios.RETURNED),
            ["Get a comment-only review", "Get changes requested"],
        )

    def test_total_badge_counts_distinct_prs(self) -> None:
        self.page.goto(self.url)
        tab = self.page.locator(".topbar-tabs a").filter(has_text="Inboxes")
        # Needs your review (1), Returned (2) and Approved (1) count in
        # the total.
        expect(tab.locator(".count")).to_have_text("4")
        expect(self.page).to_have_title("(4) grenadine")
        # A counted inbox overlapping them counts its PRs once.
        self.add_inbox("Mine too", "state:open author:@me", counted=True)
        expect(tab.locator(".count")).to_have_text("9")
        expect(self.page).to_have_title("(9) grenadine")

    def test_count_in_total_survives_reorder(self) -> None:
        self.page.goto(self.url)
        inbox = self.add_inbox("Counted", "state:open author:@me", counted=True)
        inbox.locator(".inbox-header").hover()
        inbox.get_by_title("Move up").click()
        expect(inbox.locator(".count")).to_have_text("8")

        with urllib.request.urlopen(f"{self.url}api/inboxes") as resp:
            [saved] = [
                i["inbox"] for i in json.load(resp) if i["inbox"]["name"] == "Counted"
            ]
        self.assertTrue(saved["count_in_total"])

    def test_invalid_gr_filter_shows_the_save_error(self) -> None:
        self.page.goto(self.url)
        self.page.get_by_role("button", name="+ Add inbox").click()
        form = self.page.locator("form.inbox-form")
        form.get_by_label("Name").fill("Broken")
        form.get_by_label("GitHub filter").fill("gr:bogus:x")
        form.get_by_role("button", name="Save").click()
        expect(form.locator(".error")).to_contain_text("unknown grenadine qualifier")

    def test_pr_page(self) -> None:
        pr = self.cases.prs["Get approved"]
        self.page.context.route(
            "https://github.com/**",
            lambda route: route.fulfill(body="<p>github</p>"),
        )
        self.page.goto(self.url)

        # The row's link covers the whole row; the icon after `repo#n`
        # opens the PR on GitHub without navigating the page.
        row = self.row(pr.title)
        expect(row.locator("a.pr-item")).to_have_attribute(
            "href", f"#/{scenarios.REPO}/{pr.number}"
        )
        icon = row.get_by_role("link", name="Open on GitHub")
        expect(icon).to_have_attribute("href", pr.url)
        with self.page.expect_popup() as popup:
            icon.click()
        popup.value.wait_for_load_state()
        expect(self.page).to_have_url(self.url)

        row.locator("a.pr-item").click()
        expect(self.page).to_have_url(re.compile(f"#/{scenarios.REPO}/{pr.number}$"))

        header = self.page.locator(".pr-header")
        expect(header.locator("h1")).to_have_text(f"{pr.title} #{pr.number}")
        # The number is plain text; the icon after it opens the PR on GitHub.
        expect(header.locator(".pr-number")).to_have_text(f"#{pr.number}")
        expect(header.locator("a.pr-number")).to_have_count(0)
        expect(header.get_by_role("link", name="Open on GitHub")).to_have_attribute(
            "href", pr.url
        )
        expect(header.locator(".state")).to_have_text("open")
        expect(header.locator(".badge")).to_have_count(0)

        description = self.page.locator("section.description")
        expect(description.locator("h2")).to_have_text("Why")
        expect(description.locator("li")).to_have_count(2)
        expect(description.locator("li code")).to_have_text("inline code")
        expect(description.locator("pre")).to_contain_text("fn main() {}")

    def sidebar(self) -> Locator:
        return self.page.locator(".pr-sidebar")

    def check_group(self, label: str) -> Locator:
        return self.page.locator(".check-group").filter(
            has=self.page.locator(".check-group-header", has_text=label)
        )

    def test_pr_page_shows_reviews_and_the_conversation(self) -> None:
        pr = self.cases.prs["Get changes requested"]
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")

        sidebar = self.sidebar()
        expect(sidebar.locator(".sidebar-review .review-decision")).to_have_text(
            "Changes requested"
        )
        rows = sidebar.locator(".reviewers > span")
        expect(rows).to_have_count(7)
        expect(sidebar.locator(".reviewer-name")).to_have_text(
            ["Other Person", "third"]
        )
        expect(sidebar.locator(".review-state")).to_have_text(
            ["Changes requested", "Pending"]
        )
        # The header doesn't summarize the review anymore.
        expect(self.page.locator(".pr-header .review-decision")).to_have_count(0)
        expect(self.page.locator(".pr-header .review-summary")).to_have_count(0)

        conversation = self.page.locator("details.conversation")
        expect(conversation.locator("summary")).to_have_text("2 comments")
        comments = conversation.locator(".comment")
        expect(comments.nth(0).locator(".comment-meta")).to_contain_text(
            "Other Person requested changes"
        )
        expect(comments.nth(0).locator(".markdown")).to_have_text(
            "Please split this up."
        )
        expect(comments.nth(1).locator(".comment-meta")).to_contain_text("Me Myself")
        expect(comments.nth(1).locator(".markdown")).to_have_text("Will do, thanks!")

        merge = self.page.locator(".merge-box")
        expect(merge.locator(".checks")).to_have_count(0)
        expect(merge.locator(".merge-status")).to_have_text("Ready to merge.")

    def test_pr_page_shows_checks(self) -> None:
        pr = self.cases.prs["Ask my team for a review"]
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")

        # Failing checks show unfolded, the others folded.
        failing = self.check_group("Failing (1)")
        expect(failing.locator(".check-group-header")).to_have_attribute(
            "aria-expanded", "true"
        )
        expect(failing.locator(".check")).to_have_text(["test"])
        expect(failing.locator(".check .ci")).to_have_class(re.compile(r"\bfailing\b"))
        passed = self.check_group("Passed (1)")
        expect(passed.locator(".check-group-header")).to_have_attribute(
            "aria-expanded", "false"
        )
        expect(passed.locator(".check")).to_have_count(0)
        skipped = self.check_group("Skipped (1)")
        expect(skipped.locator(".check-group-header")).to_have_attribute(
            "aria-expanded", "false"
        )
        # Expanding a group shows its check.
        passed.locator(".check-group-header").click()
        expect(passed.locator(".check")).to_have_text(["lint"])
        expect(passed.locator(".check a")).to_have_attribute(
            "href", f"{pr.url}/checks?name=lint"
        )

        expect(self.page.locator(".merge-status")).to_have_text(
            "Can be merged, but not every check passed."
        )
        expect(self.page.locator(".merge-box .checks")).to_have_count(0)
        # Other tests may have commented on this PR meanwhile.
        n = len(pr.issue_comments)
        expect(self.page.locator("details.conversation summary")).to_have_text(
            "No comments" if n == 0 else f"{n} comment" + ("" if n == 1 else "s")
        )
        # The request went to a team, marked with the team icon.
        team = self.sidebar().locator(".reviewers > span.reviewer-icon")
        expect(team.locator(".team-icon")).to_have_count(1)
        expect(self.sidebar().locator(".reviewer-name")).to_have_text([TEAM])
        expect(self.sidebar().locator(".review-state")).to_have_text(["Pending"])

    def test_pr_page_of_a_blocked_pr(self) -> None:
        pr = self.cases.prs["Ask me for a review"]
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        expect(self.page.locator(".pr-sidebar .review-decision")).to_have_text(
            "Review required"
        )
        status = self.page.locator(".merge-box .merge-status")
        expect(status).to_have_text("Merging is blocked: it needs an approving review.")
        expect(status).to_have_class(re.compile(r"\bbad\b"))

    def test_pr_page_of_a_conflicting_pr(self) -> None:
        pr = self.cases.prs["Leave me out of it"]
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        status = self.page.locator(".merge-box .merge-status")
        expect(status).to_have_text("This branch has conflicts that must be resolved.")
        expect(status).to_have_class(re.compile(r"\bbad\b"))

    def test_pr_page_of_a_merged_pr_has_no_merge_box(self) -> None:
        pr = self.cases.prs["Get merged"]
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        expect(self.page.locator(".pr-header .state")).to_have_text("merged")
        expect(self.page.locator(".merge-box")).to_have_count(0)
        # The sidebar still shows the review and the checks.
        expect(self.sidebar().locator(".review-decision")).to_have_text("Approved")
        expect(self.sidebar().locator(".reviewer-name")).to_have_text(["Other Person"])
        expect(self.check_group("Passed (1)").locator(".check")).to_have_count(0)
        self.check_group("Passed (1)").locator(".check-group-header").click()
        expect(self.check_group("Passed (1)").locator(".check")).to_have_text(["test"])

    def test_pr_page_of_a_queued_pr(self) -> None:
        pr = self.cases.prs["Sit in the merge queue"]
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        header = self.page.locator(".pr-header")
        expect(header.locator(".state")).to_have_text("queued")
        expect(header.locator(".state")).to_have_class(re.compile(r"\bstate-queued\b"))

    def test_pr_sidebar_collapses_and_stays_collapsed(self) -> None:
        pr = self.cases.prs["Get approved"]
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")

        toggle = self.page.locator(".sidebar-toggle")
        expect(toggle).to_have_attribute("aria-expanded", "true")
        toggle.click()
        expect(self.sidebar()).to_have_count(0)
        expect(toggle).to_have_attribute("aria-expanded", "false")

        self.page.reload()
        expect(self.sidebar()).to_have_count(0)
        self.page.locator(".sidebar-toggle").click()
        expect(self.sidebar()).to_have_count(1)

    def test_pr_sidebar_overlays_on_narrow_windows(self) -> None:
        pr = self.cases.prs["Get approved"]
        self.page.set_viewport_size({"width": 1280, "height": 900})
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")

        toggle = self.page.locator(".sidebar-toggle")
        expect(self.sidebar()).to_have_count(0)
        toggle.click()
        expect(self.sidebar()).to_have_count(1)
        expect(self.sidebar()).to_have_class(re.compile(r"\boverlay\b"))

        # Escape closes the overlay.
        self.page.keyboard.press("Escape")
        expect(self.sidebar()).to_have_count(0)

        # So does a click outside it, but not one inside.
        toggle.click()
        expect(self.sidebar()).to_have_count(1)
        self.sidebar().locator(".reviewers").click()
        expect(self.sidebar()).to_have_count(1)
        self.page.locator(".pr-header h1").click()
        expect(self.sidebar()).to_have_count(0)

        # A wide window shows the column again, and forgets the overlay.
        toggle.click()
        self.page.set_viewport_size({"width": 1800, "height": 900})
        expect(self.sidebar()).to_have_count(1)
        expect(self.sidebar()).not_to_have_class(re.compile(r"\boverlay\b"))

    def collaborators(self, users: set[str], team: set[str] | None = None) -> None:
        """Makes `users` assignable on the scenario repo and its owner an
        organization with the team, and undoes it all on cleanup."""
        repo = self.gh.repo(scenarios.REPO)
        saved = (
            dict(self.gh.users),
            set(repo.collaborators),
            {s: set(m) for s, m in repo.teams.items()},
        )
        for u in users:
            self.gh.users.setdefault(u, u)
        repo.collaborators |= users
        if team is not None:
            repo.teams[TEAM_SLUG] = team

        def restore() -> None:
            self.gh.users.clear()
            self.gh.users.update(saved[0])
            repo.collaborators.clear()
            repo.collaborators.update(saved[1])
            repo.teams.clear()
            repo.teams.update(saved[2])

        self.addCleanup(restore)

    def synced(self, pr: PullRequest) -> None:
        """Asks the server to sync and waits until it re-fetched `pr`."""
        seen = f"graphql pr {pr.repo.slug}#{pr.number}"
        n = self.gh.requests.count(seen)
        self.refresh()
        deadline = time.monotonic() + 15
        while self.gh.requests.count(seen) == n:
            if time.monotonic() > deadline:
                self.fail(f"the server never re-synced {seen}")
            time.sleep(0.05)

    def wait_until(self, check: Callable[[], bool], what: str) -> None:
        """Polls `check` until it holds, with the usual deadline."""
        deadline = time.monotonic() + 15
        while not check():
            if time.monotonic() > deadline:
                self.fail(f"timed out waiting for {what}")
            time.sleep(0.05)

    def pr_page(self, title: str, width: int = 1800) -> None:
        """Opens a PR's page at a window width, without a saved sidebar
        width."""
        pr = self.cases.prs[title]
        self.page.set_viewport_size({"width": width, "height": 900})
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        self.page.evaluate(
            "() => localStorage.removeItem('grenadine.pr-sidebar-width')"
        )
        self.page.reload()
        expect(self.sidebar()).to_be_visible()

    def test_picker_requests_users_and_a_team(self) -> None:
        pr = self.cases.prs["Wait for reviewers"]

        def restore() -> None:
            pr.review_requests.discard(scenarios.OTHER)
            pr.team_review_requests.discard(scenarios.OTHER)
            # The server holds the requests the test made until a sync.
            pr.touch()
            self.synced(pr)

        self.addCleanup(restore)
        self.collaborators({scenarios.OTHER}, team={scenarios.OTHER})
        self.pr_page("Wait for reviewers")

        sidebar = self.sidebar()
        sidebar.get_by_role("button", name="Edit reviewers").click()
        popover = self.page.locator(".reviewer-popover")
        other = popover.locator(".picker-row").filter(has_text="Other Person")
        team = popover.locator(".picker-row").filter(has_text=TEAM)
        expect(popover.locator(".picker-row")).to_have_count(2)
        other.click()
        team.click()
        # Closing the picker sends the ticks as one request.
        self.page.locator(".pr-header h1").click()
        expect(sidebar.locator(".reviewer-name")).to_have_text(["Other Person", TEAM])
        self.wait_until(
            lambda: (
                pr.review_requests == {scenarios.OTHER}
                and pr.team_review_requests == {scenarios.OTHER}
            ),
            "the reviewer requests to reach GitHub",
        )

    def test_picker_unticks_a_pending_request(self) -> None:
        pr = self.cases.prs["Get changes requested"]

        def restore() -> None:
            pr.review_requests.add("third")
            pr.touch()
            self.synced(pr)

        self.addCleanup(restore)
        # "third" is requested but not a collaborator, so the picker
        # lists it as an extra ticked row.
        self.collaborators({"third"})
        self.pr_page("Get changes requested")

        sidebar = self.sidebar()
        expect(sidebar.locator(".reviewer-name")).to_have_text(
            ["Other Person", "third"]
        )
        sidebar.get_by_role("button", name="Edit reviewers").click()
        popover = self.page.locator(".reviewer-popover")
        expect(popover.locator(".picker-row")).to_have_count(1)
        popover.locator(".picker-row").click()
        self.page.keyboard.press("Escape")
        expect(sidebar.locator(".reviewer-name")).to_have_text(["Other Person"])
        self.wait_until(
            lambda: pr.review_requests == set(), "the request to be removed"
        )

    def test_rerequest_asks_a_reviewer_again(self) -> None:
        pr = self.cases.prs["Get approved"]

        def restore() -> None:
            pr.review_requests.discard(scenarios.OTHER)
            pr.touch()
            self.synced(pr)

        self.addCleanup(restore)
        self.pr_page("Get approved")

        sidebar = self.sidebar()
        expect(sidebar.locator(".review-state")).to_have_text(["Approved"])
        sidebar.get_by_role("button", name="Re-request review").click()
        expect(sidebar.locator(".review-state")).to_have_text(["Pending"])
        self.wait_until(
            lambda: pr.review_requests == {scenarios.OTHER},
            "the re-request to reach GitHub",
        )

    def test_reviewer_controls_only_when_editable(self) -> None:
        sidebar = self.sidebar()
        # A merged PR keeps the read-only list.
        pr = self.cases.prs["Get merged"]
        self.pr_page("Get merged")
        expect(sidebar.locator(".reviewer-name")).to_have_text(["Other Person"])
        expect(sidebar.get_by_role("button", name="Edit reviewers")).to_have_count(0)
        expect(sidebar.get_by_role("button", name="Re-request review")).to_have_count(0)

        # So does a PR the viewer may not update.
        pr = self.cases.prs["Get approved"]
        pr.viewer_can_update = False
        pr.touch()

        def restore() -> None:
            pr.viewer_can_update = True
            pr.touch()
            self.synced(pr)

        self.addCleanup(restore)
        self.synced(pr)
        self.pr_page("Get approved")
        expect(sidebar.locator(".reviewer-name")).to_have_text(["Other Person"])
        expect(sidebar.get_by_role("button", name="Edit reviewers")).to_have_count(0)
        expect(sidebar.get_by_role("button", name="Re-request review")).to_have_count(0)

    def test_a_failed_reviewer_change_reverts_the_list(self) -> None:
        pr = self.cases.prs["Get changes requested"]
        self.collaborators({"third"})
        self.gh.fail_next_reviewers = "can't reach GitHub"
        self.pr_page("Get changes requested")

        sidebar = self.sidebar()
        sidebar.get_by_role("button", name="Edit reviewers").click()
        popover = self.page.locator(".reviewer-popover")
        popover.locator(".picker-row").click()
        # The row goes optimistically, then the refusal reverts it and
        # shows the error.
        self.page.keyboard.press("Escape")
        expect(sidebar.locator(".reviewer-name")).to_have_text(
            ["Other Person", "third"]
        )
        expect(sidebar.locator(".sidebar-review .error")).to_contain_text(
            "can't reach GitHub"
        )
        self.assertEqual(pr.review_requests, {"third"})

    def test_picker_survives_ticks_and_syncs(self) -> None:
        pr = self.cases.prs["Wait for reviewers"]

        def restore() -> None:
            pr.review_requests.discard(scenarios.OTHER)
            pr.title = "Wait for reviewers"
            pr.touch()
            self.synced(pr)

        self.addCleanup(restore)
        self.collaborators({scenarios.OTHER})
        self.pr_page("Wait for reviewers")

        self.sidebar().get_by_role("button", name="Edit reviewers").click()
        box = self.page.locator(".picker-search")
        box.fill("other")
        popover = self.page.locator(".reviewer-popover")
        popover.locator(".picker-row").wait_for()
        # A tick doesn't rebuild the popover: the search box keeps its
        # text and stays the same element.
        same = "el => el === document.querySelector('.picker-search')"
        self.assertTrue(box.evaluate(same))
        popover.locator(".picker-row").filter(has_text="Other Person").click()
        self.assertTrue(box.evaluate(same))
        expect(box).to_have_value("other")
        box.focus()
        # A sync rebuilding the rest of the sidebar doesn't remount the
        # picker: it's still open, focused and unchanged.
        pr.title = "Wait for reviewers!"
        pr.touch()
        self.synced(pr)
        expect(popover).to_have_count(1)
        self.assertTrue(box.evaluate(same))
        expect(box).to_have_value("other")
        expect(box).to_be_focused()

    def drag_sidebar(self, dx: float) -> None:
        """Drags the sidebar's divider `dx` pixels to the right."""
        divider = self.page.locator(".pr-sidebar .sidebar-divider")
        expect(divider).to_be_visible()
        box = divider.bounding_box()
        x = box["x"] + box["width"] / 2
        y = box["y"] + box["height"] / 2
        self.page.mouse.move(x, y)
        self.page.mouse.down()
        self.page.mouse.move(max(0.0, x + dx), y)
        self.page.mouse.up()

    def assert_sidebar_width(self, expected: float, delta: float = 5) -> None:
        """Waits for the sidebar's width to reach `expected` pixels."""
        expect(self.sidebar()).to_have_css("width", f"{expected}px")
        self.assertAlmostEqual(
            self.sidebar().bounding_box()["width"], expected, delta=delta
        )

    def assert_main_width(self, expected: float, delta: float = 5) -> None:
        """Waits for the PR's content width to reach `expected` pixels."""
        main = self.page.locator(".pr-page .main")
        expect(main).to_have_css("width", f"{expected}px")
        self.assertAlmostEqual(main.bounding_box()["width"], expected, delta=delta)

    def test_pr_sidebar_fills_wide_windows(self) -> None:
        self.pr_page("Get approved", 2400)
        self.assert_sidebar_width(800)
        self.assert_main_width(1600)

        # The divider straddles the sidebar's left edge.
        divider = self.page.locator(".sidebar-divider")
        expect(divider).to_be_visible()
        dbox = divider.bounding_box()
        self.assertAlmostEqual(dbox["width"], 6, delta=1)
        self.assertAlmostEqual(
            dbox["x"], self.sidebar().bounding_box()["x"] - 3, delta=1
        )

    def test_pr_sidebar_divider_stays_put_when_scrolled(self) -> None:
        self.pr_page("Ask my team for a review", 2400)
        self.page.set_viewport_size({"width": 2400, "height": 300})
        # Fill the sidebar enough that its body scrolls.
        for label in ("Failing (1)", "Passed (1)", "Skipped (1)"):
            self.check_group(label).locator(".check-group-header").click()

        body = self.page.locator(".pr-sidebar-body")
        self.assertGreater(body.evaluate("el => el.scrollHeight - el.clientHeight"), 0)
        body.evaluate("el => el.scrollTop = el.scrollHeight")

        divider = self.page.locator(".sidebar-divider")
        expect(divider).to_be_visible()
        dbox = divider.bounding_box()
        sbox = self.sidebar().bounding_box()
        self.assertAlmostEqual(dbox["x"], sbox["x"] - 3, delta=1)
        self.assertLess(dbox["y"], sbox["y"] + sbox["height"])

    def test_pr_sidebar_drags_and_remembers(self) -> None:
        self.pr_page("Get approved", 2400)

        self.drag_sidebar(-200)
        self.assert_sidebar_width(1000)

        # The width survives a reload.
        self.page.reload()
        expect(self.sidebar()).to_be_visible()
        self.assert_sidebar_width(1000)

        # Drags clamp at both ends.
        self.drag_sidebar(2000)
        self.assert_sidebar_width(320)
        self.drag_sidebar(-2000)
        self.assert_sidebar_width(1220)
        self.assert_main_width(1180)

    def test_pr_sidebar_double_click_restores_the_split(self) -> None:
        self.pr_page("Get approved", 2400)
        self.drag_sidebar(-200)
        self.assert_sidebar_width(1000)
        self.page.locator(".sidebar-divider").dblclick()
        self.assert_sidebar_width(800)

    def test_pr_sidebar_clamps_until_the_window_fits_it(self) -> None:
        self.pr_page("Get approved", 2400)
        self.drag_sidebar(-200)
        self.assert_sidebar_width(1000)

        # A narrower window clamps the saved width; widening it back
        # brings it back.
        self.page.set_viewport_size({"width": 1600, "height": 900})
        self.assert_sidebar_width(420)
        self.page.set_viewport_size({"width": 2400, "height": 900})
        self.assert_sidebar_width(1000)

    def test_pr_sidebar_overlay_drags_too(self) -> None:
        pr = self.cases.prs["Get approved"]
        self.page.set_viewport_size({"width": 1280, "height": 900})
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        self.page.evaluate(
            "() => localStorage.removeItem('grenadine.pr-sidebar-width')"
        )
        self.page.locator(".sidebar-toggle").click()
        expect(self.sidebar()).to_be_visible()

        self.drag_sidebar(-200)
        self.assert_sidebar_width(520)

        # A drag that ends outside the sidebar clamps at the page edge
        # and doesn't close the overlay.
        self.drag_sidebar(-2000)
        self.assert_sidebar_width(1232)
        expect(self.sidebar()).to_have_count(1)

        # A later click outside still closes it. The sidebar covers all
        # but the leftmost sliver of the header here.
        self.page.locator(".pr-header h1").click(position={"x": 10, "y": 5})
        expect(self.sidebar()).to_have_count(0)

    def test_pr_fills_the_window_with_the_sidebar_collapsed(self) -> None:
        self.pr_page("Get approved", 2400)
        self.page.locator(".sidebar-toggle").click()
        expect(self.sidebar()).to_have_count(0)
        self.assertAlmostEqual(
            self.page.locator(".pr").bounding_box()["width"], 2400, delta=30
        )

    def test_check_group_folds_survive_a_sync(self) -> None:
        pr = self.cases.prs["Ask my team for a review"]
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")

        passed = self.check_group("Passed (1)")
        passed.locator(".check-group-header").click()
        expect(passed.locator(".check")).to_have_text(["lint"])

        # A comment from outside the page resyncs the PR.
        resp = self.page.request.post(
            f"{self.url}api/pr/{scenarios.REPO}/{pr.number}/comments",
            headers={"Origin": self.url.rstrip("/")},
            data={"body": "Ping"},
        )
        self.assertEqual(resp.status, 204, resp.text())
        expect(self.page.locator("details.conversation summary")).to_have_text(
            "1 comment"
        )
        expect(self.check_group("Passed (1)").locator(".check")).to_have_text(["lint"])

    def test_pr_page_of_a_draft(self) -> None:
        pr = self.cases.prs["Stay a draft"]
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        header = self.page.locator(".pr-header")
        expect(header.locator("h1")).to_have_text(f"{pr.title} #{pr.number}")
        expect(header.locator(".badge")).to_have_text("draft")

    def test_pr_page_shows_the_stack(self) -> None:
        base = self.cases.prs[scenarios.STACK_BASE]
        top = self.cases.prs[scenarios.STACK_TOP]
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{top.number}")

        rows = self.page.locator("section.stack .stack-label")
        expect(rows).to_have_text(
            [
                f"#{top.number} {top.title} open",
                f"#{base.number} {base.title} open",
                "main",
            ]
        )
        # The PR itself is bold; the others link to their page in
        # grenadine, with an icon that opens them on GitHub.
        expect(rows.locator("strong")).to_have_text(f"#{top.number} {top.title}")
        links = rows.nth(1).locator("a")
        expect(links.nth(0)).to_have_attribute(
            "href", f"#/{scenarios.REPO}/{base.number}"
        )
        expect(links.nth(1)).to_have_attribute("href", base.url)
        expect(links.nth(1)).to_have_class("gh-link")

    def test_debug_tab_shows_github_usage(self) -> None:
        self.page.goto(self.url)
        # Wait for the first poll's searches and syncs.
        expect(
            self.inbox(scenarios.NEEDS_REVIEW).locator(".pr-list li")
        ).not_to_have_count(0)
        self.page.get_by_role("link", name="Debug", exact=True).click()
        expect(
            self.page.locator(".debug h1", has_text="GitHub API usage")
        ).to_be_visible()
        expect(self.page.locator(".debug")).to_contain_text("of 5,000 points left")
        expect(
            self.page.locator(".usage-table:not(.usage-reports):not(.usage-sessions)")
        ).to_contain_text("Inbox search")
        expect(self.page.locator(".usage-reports")).to_contain_text(scenarios.REPO)

    def test_debug_tab_shows_system_stats(self) -> None:
        self.page.goto(self.url)
        self.page.get_by_role("link", name="Debug", exact=True).click()
        expect(self.page.locator(".debug h1").first).to_have_text("System")
        expect(self.page.locator(".debug")).to_contain_text(
            re.compile(r"RAM: .* of .* used")
        )
        # Slices need a systemd user manager; without one a note stands
        # in for the table.
        expect(
            self.page.locator(
                ".usage-sessions, .debug p.muted:has-text('systemd user manager')"
            ).first
        ).to_be_visible()

    def test_pauses_near_the_rate_limit(self) -> None:
        self.page.goto(self.url)
        inbox = self.inbox(scenarios.NEEDS_REVIEW)
        expect(inbox.locator(".pr-list li")).not_to_have_count(0)
        status = self.page.locator(".sync-status")
        expect(status).to_have_text(re.compile(r"^Synced"))

        # Below a tenth of the limit, the next poll does nothing at
        # all. The window ends in a few seconds so the pause is
        # short-lived.
        reset = time.time() + 5
        self.gh.set_rate_limit(400, reset_in=5)
        self.addCleanup(self.gh.set_rate_limit, 5000)
        self.repoll()
        expect(status).to_have_text(re.compile(r"^Paused until "))
        # The inboxes keep their last results and no error is shown.
        expect(inbox.locator(".pr-list li")).not_to_have_count(0)
        expect(self.page.locator(".sync-error")).to_have_count(0)

        # The viewer's own actions still go at that level: a PR no
        # inbox covers is synced when its page opens.
        title = next(t for t, i in self.cases.expected.items() if not i)
        pr = self.cases.prs[title]
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        expect(self.page.locator(".pr-header h1")).to_have_text(
            f"{pr.title} #{pr.number}"
        )

        # The server's last-known state still says 400; it only lets
        # searches go once the window's reset has passed, the way
        # GitHub refills the budget.
        self.gh.set_rate_limit(5000)
        time.sleep(max(0, reset + 1 - time.time()))
        self.repoll()
        expect(status).to_have_text(re.compile(r"^Synced"))


class InboxPagingTest(ServerTestCase):
    """Inboxes show 15 rows first and grow by 10 per "Show more" click."""

    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        repo = gh.repo(scenarios.REPO)
        repo.checkout("main")
        repo.commit("Seed the repo", {"README.md": "# test\n"})
        repo.push("main")
        # Oldest first: 21 plain PRs, a two-PR stack, then 14 plain PRs, so
        # that in the newest-first inbox the cut at 15 rows splits the
        # stack.
        for i in range(1, 22):
            scenarios._simple_pr(repo, f"paging-{i:02}", f"Paging {i:02}")
        repo.checkout("dev/paging-base", "main")
        repo.commit("Paging base", {"paging/base.txt": "base\n"})
        repo.push("dev/paging-base")
        repo.open_pr("Paging base", head="dev/paging-base")
        repo.checkout("dev/paging-top")
        repo.commit("Paging top", {"paging/top.txt": "top\n"})
        repo.push("dev/paging-top")
        repo.open_pr("Paging top", head="dev/paging-top", base="dev/paging-base")
        for i in range(22, 36):
            scenarios._simple_pr(repo, f"paging-{i:02}", f"Paging {i:02}")

    def test_show_more_grows_the_list(self) -> None:
        self.page.goto(self.url)
        inbox = self.inbox(scenarios.ALL_MY_OPEN)
        rows = inbox.locator(".pr-list li")
        more = inbox.get_by_role("button", name=re.compile(r"^Show \d+ more$"))
        expect(inbox.locator(".count")).to_have_text("37")
        expect(rows).to_have_count(15)
        # The cut splits the stack, so the last visible row's connector is
        # dashed.
        expect(rows.nth(14)).to_have_class(re.compile(r"\bstack-below-gap\b"))
        first = rows.locator(".pr-title").all_inner_texts()

        # The button collapses with the list.
        inbox.locator(".inbox-toggle").click()
        expect(more).to_be_hidden()
        inbox.locator(".inbox-toggle").click()
        expect(more).to_be_visible()

        inbox.get_by_role("button", name="Show 10 more", exact=True).click()
        expect(rows).to_have_count(25)
        # The list grows downward; the rows already shown don't move.
        self.assertEqual(rows.locator(".pr-title").all_inner_texts()[:15], first)
        expect(rows.nth(14)).not_to_have_class(re.compile(r"\bstack-below-gap\b"))

        inbox.get_by_role("button", name="Show 10 more", exact=True).click()
        expect(rows).to_have_count(35)
        expect(more).to_have_text("Show 2 more")

        more.click()
        expect(rows).to_have_count(37)
        expect(more).to_have_count(0)

    def test_shown_rows_survive_refreshes_but_reset_on_reload(self) -> None:
        self.page.goto(self.url)
        inbox = self.inbox(scenarios.ALL_MY_OPEN)
        rows = inbox.locator(".pr-list li")
        expect(rows).to_have_count(15)

        inbox.get_by_role("button", name="Show 10 more", exact=True).click()
        expect(rows).to_have_count(25)

        # A refetch, which rebuilds the section, keeps the count.
        with self.page.expect_response(re.compile(r"api/inboxes$")):
            self.repoll()
        expect(rows).to_have_count(25)

        self.page.reload()
        expect(rows).to_have_count(15)


class BackgroundSyncTest(ServerTestCase):
    """Inboxes without "sync in background" list their PRs from the
    search's data; the PRs sync when a page watches them."""

    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        cls.cases = scenarios.inbox_cases(gh)

    def inbox(self, name: str) -> Locator:
        return self.page.locator("section.inbox").filter(
            has=self.page.locator(".inbox-name", has_text=exactly(name))
        )

    def pr_requests(self, pr: PullRequest) -> list[str]:
        """The requests that fetched `pr`'s details."""
        return [
            r
            for r in self.gh.requests
            if r == f"graphql pr {scenarios.REPO}#{pr.number}"
            or r.startswith(
                (
                    f"GET /repos/{scenarios.REPO}/pulls/{pr.number}/",
                    f"GET /repos/{scenarios.REPO}/activity?ref=refs/heads/{pr.head}",
                )
            )
        ]

    def wait_for_requests(self, pr: PullRequest, count: int) -> list[str]:
        """Waits until `pr`'s detail requests reach `count` and returns them."""
        deadline = time.monotonic() + 15
        while len(self.pr_requests(pr)) < count and time.monotonic() < deadline:
            time.sleep(0.1)
        return self.pr_requests(pr)

    def set_background(self, name: str, on: bool) -> None:
        """Sets inbox `name`'s "sync in background" flag through the API."""
        with urllib.request.urlopen(f"{self.url}api/inboxes") as resp:
            [inbox] = [
                i["inbox"] for i in json.load(resp) if i["inbox"]["name"] == name
            ]
        inbox["sync_in_background"] = on
        req = urllib.request.Request(
            f"{self.url}api/inboxes/{inbox['id']}",
            data=json.dumps(inbox).encode(),
            method="PUT",
            headers={
                "Content-Type": "application/json",
                "Origin": self.url.rstrip("/"),
            },
        )
        urllib.request.urlopen(req).close()

    def test_a_direct_link_syncs_a_pr_in_no_inbox(self) -> None:
        # "Get closed" is in no inbox; opening its page syncs it.
        pr = self.cases.prs["Get closed"]
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        expect(self.page.locator(".pr-header h1")).to_have_text(
            f"{pr.title} #{pr.number}"
        )
        self.assertNotEqual(self.pr_requests(pr), [])

    def test_off_inbox_rows_show_search_data_only(self) -> None:
        # "Leave me out of it" is only in inboxes that don't sync in
        # background, and isn't the viewer's to review. No other test in
        # the class opens it, so it's never fetched.
        pr = self.cases.prs["Leave me out of it"]
        self.page.goto(self.url)
        row = (
            self.inbox(scenarios.ALL_OPEN)
            .locator(".pr-list li")
            .filter(has_text=pr.title)
        )
        # The row's data is the search's: title, author, the updatedAt
        # date — no "syncing…", no error.
        expect(row.locator(".pr-meta")).to_have_text(
            re.compile(
                rf"{re.escape(scenarios.REPO)}#{pr.number} · {re.escape(pr.author)} "
                rf"· \d{{4}}-\d{{2}}-\d{{2}}$"
            )
        )
        # And its details were never fetched: the polls skip it.
        self.assertEqual(self.pr_requests(pr), [])

    def test_opening_an_off_inbox_pr_syncs_it(self) -> None:
        pr = self.cases.prs["Ask my team for a review"]
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        expect(self.page.locator(".pr-header h1")).to_have_text(
            f"{pr.title} #{pr.number}"
        )
        self.assertNotEqual(self.pr_requests(pr), [])

    def test_reopening_syncs_again(self) -> None:
        pr = self.cases.prs["Wait for the author after my review"]
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        expect(self.page.locator(".pr-header h1")).to_have_text(
            f"{pr.title} #{pr.number}"
        )
        first = len(self.wait_for_requests(pr, 1))
        self.assertGreater(first, 0)

        # Back to the inboxes and in again: a new watch means a new sync.
        self.page.goto(self.url)
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        again = self.wait_for_requests(pr, first + 1)
        self.assertGreater(len(again), first)

    def test_refresh_searches_off_inboxes(self) -> None:
        # "Leave me out of it" is only in inboxes that don't sync in
        # background. A poll doesn't re-search those: a changed hit waits
        # for the off-inbox search the Refresh button requests.
        pr = self.cases.prs["Leave me out of it"]
        self.page.goto(self.url)
        inbox = self.inbox(scenarios.ALL_OPEN)
        # The first poll searched every inbox, and an "off" one says when.
        expect(inbox.locator(".inbox-header")).to_contain_text("searched")
        self.assertEqual(self.pr_requests(pr), [])

        off_before = self.searched_at(scenarios.ALL_OPEN)
        bg_before = self.searched_at(scenarios.NEEDS_REVIEW)
        # Wait out the current second so the next search's stamp moves.
        tick = int(time.time())
        while int(time.time()) == tick:
            time.sleep(0.05)
        pr.title = "Renamed off-inbox PR"
        self.repoll()
        # The repoll re-searched the background inboxes…
        deadline = time.time() + 15
        while time.time() < deadline:
            if (self.searched_at(scenarios.NEEDS_REVIEW) or -1) > (bg_before or -1):
                break
            time.sleep(0.1)
        else:
            self.fail("the repoll never re-searched a background inbox")
        # …but not the "off" one, whose stamp and hit didn't move.
        self.assertEqual(self.searched_at(scenarios.ALL_OPEN), off_before)
        expect(inbox.locator(".pr-title", has_text="Leave me out of it")).to_have_count(
            1
        )

        self.page.get_by_role("button", name="Refresh").click()
        expect(
            inbox.locator(".pr-title", has_text="Renamed off-inbox PR")
        ).to_have_count(1)

    def test_turning_on_background_syncs_the_inbox(self) -> None:
        # "Ask me to look at a draft" is only in inboxes that don't sync
        # in background, and isn't the viewer's to review. No other test
        # in the class opens it.
        pr = self.cases.prs["Ask me to look at a draft"]
        # The flag change is a class-wide one; undo it for later tests.
        self.addCleanup(self.set_background, scenarios.DRAFTS_FROM_OTHERS, False)
        self.page.goto(self.url)
        self.assertEqual(self.pr_requests(pr), [])

        inbox = self.inbox(scenarios.DRAFTS_FROM_OTHERS)
        inbox.locator(".inbox-header").hover()
        inbox.get_by_title("Edit").click()
        form = self.page.locator("form.inbox-form")
        form.get_by_label("Sync in background").check()
        form.get_by_role("button", name="Save").click()
        # The save pokes the poller; its next poll syncs the inbox's PRs.
        self.assertNotEqual(self.wait_for_requests(pr, 1), [])
        expect(inbox.locator(".pr-meta")).to_have_text(re.compile(r"· 1 versions$"))


class ReviewTest(ServerTestCase):
    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        cls.prs = scenarios.review_cases(gh)

    def open(self, title: str) -> None:
        pr = self.prs[title]
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        expect(self.page.locator(".pr-header h1")).to_have_text(
            f"{pr.title} #{pr.number}"
        )

    def thread(self) -> Locator:
        return (
            self.page.locator("section.file")
            .filter(
                has=self.page.locator(
                    ".file-header .path", has_text=exactly(scenarios.REVIEW_PATH)
                )
            )
            .locator(".thread")
            .first
        )

    def file(self) -> Locator:
        return self.page.locator("section.file").filter(
            has=self.page.locator(
                ".file-header .path", has_text=exactly(scenarios.REVIEW_PATH)
            )
        )

    def comment_on_line(self, line: int, body: str) -> None:
        file = self.file()
        file.locator("td.ln.commentable", has_text=exactly(str(line))).click()
        box = file.get_by_role("textbox", name="Add to review")
        expect(box).to_be_focused()
        box.fill(body)
        box.press("Control+Enter")
        expect(box).to_have_count(0)

    def review_panel(self) -> Locator:
        panel = self.page.locator(".review-panel")
        panel.locator(".review-button").click()
        return panel

    def test_review_lines_then_request_changes(self) -> None:
        pr = self.prs[scenarios.LINE_REVIEW]
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.open(scenarios.LINE_REVIEW)
        self.comment_on_line(4, "Typo here.")
        thread = self.file().locator(
            "table .thread", has=self.page.locator(".pending-badge")
        )
        expect(thread).to_contain_text("Typo here.")
        expect(self.page.locator(".review-panel .pending-count")).to_have_text("1")
        assert pr.pending
        [c] = pr.pending.comments
        self.assertEqual((c.line, c.side, c.body), (4, "RIGHT", "Typo here."))
        # Pending comments aren't on GitHub's PR yet.
        self.assertEqual(len(pr.comments), 1)

        # A pending comment can be edited.
        thread.get_by_role("button", name="Edit").click()
        box = thread.get_by_role("textbox", name="Save")
        expect(box).to_have_value("Typo here.")
        box.fill("Typo: teh.")
        box.press("Control+Enter")
        expect(self.file().locator(".thread", has_text="Typo: teh.")).to_have_count(1)

        # And the whole file can be commented on.
        self.file().get_by_role("button", name="Comment on file").click()
        box = self.file().get_by_role("textbox", name="Add to review")
        box.fill("Split this file?")
        box.press("Control+Enter")
        expect(
            self.file().locator(".file-comments .thread", has_text="Split this file?")
        ).to_have_count(1)
        expect(self.page.locator(".review-panel .pending-count")).to_have_text("2")

        panel = self.review_panel()
        panel.get_by_label("Request changes").check()
        submit = panel.get_by_role("button", name="Submit review")
        # Requesting changes needs a summary.
        expect(submit).to_be_disabled()
        panel.get_by_role("textbox", name="Review summary").fill("A few things.")
        submit.click()

        expect(self.page.locator(".pr-sidebar .review-decision")).to_have_text(
            "Changes requested"
        )
        expect(self.page.locator(".pending-badge")).to_have_count(0)
        expect(self.page.locator(".review-panel .pending-count")).to_have_count(0)
        self.assertIsNone(pr.pending)
        self.assertEqual(
            [c.body for c in pr.comments],
            ["Is this right?", "Typo: teh.", "Split this file?"],
        )
        self.assertEqual(pr.reviews[-1].body, "A few things.")

    def test_approve_without_comments(self) -> None:
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.open(scenarios.APPROVE)
        panel = self.review_panel()
        # A comment review needs a summary or comments.
        expect(panel.get_by_role("button", name="Submit review")).to_be_disabled()
        panel.get_by_label("Approve").check()
        panel.get_by_role("button", name="Submit review").click()
        expect(self.page.locator(".pr-sidebar .review-decision")).to_have_text(
            "Approved"
        )
        expect(self.page.locator("details.conversation .comment-meta")).to_contain_text(
            "approved"
        )

    def test_click_outside_closes_the_review_panel(self) -> None:
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.open(scenarios.APPROVE)
        panel = self.review_panel()
        popup = panel.locator(".review-popup")
        box = panel.get_by_role("textbox", name="Review summary")
        box.fill("Looks good.")
        panel.get_by_label("Approve").check()

        # Pressing elsewhere on the page hides the popup.
        self.page.locator(".pr-header h1").click()
        expect(popup).to_be_hidden()

        # The summary's draft and the chosen event survive reopening.
        self.review_panel()
        expect(popup).to_be_visible()
        expect(box).to_have_value("Looks good.")
        expect(panel.get_by_label("Approve")).to_be_checked()

    def test_editor_tabs_dont_close_the_review_panel(self) -> None:
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.open(scenarios.APPROVE)
        panel = self.review_panel()
        popup = panel.locator(".review-popup")
        panel.get_by_role("textbox", name="Review summary").fill("A **note**.")
        panel.get_by_role("tab", name="Preview").click()
        expect(popup).to_be_visible()
        panel.get_by_role("tab", name="Write").click()
        expect(popup).to_be_visible()

    def test_escape_closes_the_review_panel(self) -> None:
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.open(scenarios.APPROVE)
        panel = self.review_panel()
        popup = panel.locator(".review-popup")
        expect(popup).to_be_visible()
        self.page.locator("body").focus()
        self.page.keyboard.press("Escape")
        expect(popup).to_be_hidden()

    def test_review_button_toggles_the_panel(self) -> None:
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.open(scenarios.APPROVE)
        panel = self.review_panel()
        popup = panel.locator(".review-popup")
        expect(popup).to_be_visible()
        panel.locator(".review-button").click()
        expect(popup).to_be_hidden()

    def test_discard_a_review(self) -> None:
        pr = self.prs[scenarios.DISCARD]
        self.open(scenarios.DISCARD)
        self.comment_on_line(5, "Hmm.")
        expect(self.page.locator(".pending-badge")).to_have_count(1)
        self.page.once("dialog", lambda d: d.accept())
        self.review_panel().get_by_role("button", name="Discard review").click()
        expect(self.page.locator(".pending-badge")).to_have_count(0)
        self.assertIsNone(pr.pending)

    def test_reply_in_a_review_and_delete_it(self) -> None:
        pr = self.prs[scenarios.REPLY_IN_REVIEW]
        self.open(scenarios.REPLY_IN_REVIEW)
        thread = self.thread()
        thread.get_by_role("button", name="Reply").click()
        thread.get_by_role("textbox", name="Reply").fill("Will check.")
        thread.get_by_role("button", name="Add to review").click()
        reply = thread.locator(".comment").nth(1)
        expect(reply.locator(".pending-badge")).to_have_text("Pending")
        assert pr.pending
        self.assertEqual(pr.pending.comments[0].in_reply_to, pr.comments[0].id)

        self.page.once("dialog", lambda d: d.accept())
        reply.get_by_role("button", name="Delete").click()
        expect(thread.locator(".comment")).to_have_count(1)
        self.assertEqual(pr.pending.comments, [])

    def test_own_pr_can_only_be_commented_on(self) -> None:
        self.open(scenarios.OWN)
        panel = self.review_panel()
        expect(panel.get_by_label("Approve")).to_be_disabled()
        expect(panel.get_by_label("Request changes")).to_be_disabled()
        expect(panel.get_by_label("Comment")).to_be_enabled()

    def test_comment_on_a_removed_line(self) -> None:
        pr = self.prs[scenarios.LEFT_LINE]
        self.open(scenarios.LEFT_LINE)
        file = self.page.locator("section.file").filter(
            has=self.page.locator(
                ".file-header .path", has_text=exactly(scenarios.OLD_PATH)
            )
        )
        row = file.locator("tr.line", has_text="Old line 3")
        # The left line number: the line is only on the left.
        row.locator("td.ln.commentable").first.click()
        box = file.get_by_role("textbox", name="Add to review")
        box.fill("Why remove this?")
        box.press("Control+Enter")
        expect(box).to_have_count(0)
        assert pr.pending
        [c] = pr.pending.comments
        self.assertEqual((c.path, c.line, c.side), (scenarios.OLD_PATH, 3, "LEFT"))
        # GitHub only says which side a pending comment is on in its thread;
        # had the page lost that, it would show the comment under the new
        # line 3, which is "Old line 4".
        after = row.locator("xpath=following-sibling::tr[1]")
        expect(after).to_have_class(re.compile(r"\bcomment-row\b"))
        expect(after.locator(".pending-badge")).to_have_count(1)
        expect(after).to_contain_text("Why remove this?")

    def test_boxes_survive_a_refetch(self) -> None:
        pr = self.prs[scenarios.KEEP]
        self.open(scenarios.KEEP)
        own = {"Origin": self.url.rstrip("/")}

        def refetch(body: str) -> None:
            """Comments on the PR from outside the page, which refetches it."""
            resp = self.page.request.post(
                f"{self.url}api/pr/{scenarios.REPO}/{pr.number}/comments",
                headers=own,
                data={"body": body},
            )
            self.assertEqual(resp.status, 204, resp.text())

        file = self.file()
        file.locator("td.ln.commentable", has_text=exactly("4")).click()
        box = file.get_by_role("textbox", name="Add to review")
        box.fill("Keep **this**.")
        composer = file.locator(".comment-composer")
        composer.get_by_role("tab", name="Preview").click()
        panel = self.review_panel()
        panel.get_by_label("Approve").check()

        refetch("Ping")
        conversation = self.page.locator("details.conversation summary")
        expect(conversation).to_have_text("1 comment")
        expect(composer.get_by_role("tab", name="Preview")).to_have_attribute(
            "aria-selected", "true"
        )
        expect(composer.locator(".composer-preview strong")).to_have_text("this")
        expect(panel.locator(".review-popup")).to_be_visible()
        expect(panel.get_by_label("Approve")).to_be_checked()
        panel.locator(".review-button").click()

        # A send that a refetch interrupts: the rebuilt box knows it's being
        # sent, and still closes once it's done.
        composer.get_by_role("tab", name="Write").click()
        held, release = self.gh.hold_next_mutation()
        self.addCleanup(release.set)
        box.press("Control+Enter")
        self.assertTrue(held.wait(timeout=10))
        refetch("Pong")
        expect(conversation).to_have_text("2 comments")
        sending = composer.get_by_role("button", name="Sending…")
        expect(sending).to_be_disabled()
        box.press("Control+Enter")
        release.set()
        expect(box).to_have_count(0)
        expect(self.file().locator(".pending-badge")).to_have_count(1)
        assert pr.pending
        self.assertEqual([c.body for c in pr.pending.comments], ["Keep **this**."])
        # The draft is gone with it.
        self.page.reload()
        expect(self.file().locator(".pending-badge")).to_have_count(1)
        expect(self.file().get_by_role("textbox")).to_have_count(0)

    def test_reply_now_during_a_review(self) -> None:
        pr = self.prs[scenarios.REPLY_NOW]
        self.open(scenarios.REPLY_NOW)
        self.comment_on_line(4, "Start a review.")
        expect(self.page.locator(".review-panel .pending-count")).to_have_text("1")
        thread = self.thread()
        thread.get_by_role("button", name="Reply").click()
        # While a review is pending, replies go into it unless asked not to.
        thread.get_by_role("textbox", name="Add to review").fill("Right away.")
        thread.get_by_role("button", name="Reply now").click()
        reply = thread.locator(".comment").nth(1)
        expect(reply).to_contain_text("Right away.")
        expect(reply.locator(".pending-badge")).to_have_count(0)
        self.assertEqual(pr.comments[-1].body, "Right away.")
        assert pr.pending
        self.assertEqual([c.body for c in pr.pending.comments], ["Start a review."])

    def test_submit_comments_without_a_summary(self) -> None:
        pr = self.prs[scenarios.COMMENT_REVIEW]
        self.open(scenarios.COMMENT_REVIEW)
        self.comment_on_line(3, "One thing.")
        panel = self.review_panel()
        expect(panel.get_by_label("Comment")).to_be_checked()
        # Comments are enough for a comment review.
        submit = panel.get_by_role("button", name="Submit review")
        expect(submit).to_be_enabled()
        submit.click()
        expect(self.page.locator(".pending-badge")).to_have_count(0)
        expect(self.file().locator(".thread", has_text="One thing.")).to_have_count(1)
        self.assertIsNone(pr.pending)
        self.assertEqual((pr.reviews[-1].state, pr.reviews[-1].body), (COMMENTED, ""))
        self.assertEqual(pr.comments[-1].body, "One thing.")

    def make_pr(self, branch: str, title: str, files: dict[str, str]) -> PullRequest:
        """Adds a PR on `branch` holding `files`, by the other account."""
        repo = self.prs[scenarios.RESOLVE].repo
        repo.checkout(branch, "main")
        repo.commit(title, files, author=scenarios.OTHER)
        repo.push(branch, actor=scenarios.OTHER)
        return repo.open_pr(title, head=branch, author=scenarios.OTHER)

    def test_main_keeps_its_scroll_through_a_sync(self) -> None:
        pr = self.make_pr(
            "review/scroll",
            "Scroll through a sync",
            {scenarios.REVIEW_PATH: "".join(f"Line {i}\n" for i in range(1, 300))},
        )
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        file = self.file()
        file.evaluate("el => el.scrollIntoView({block: 'center'})")
        main = self.page.locator(".pr-page .main")
        self.assertGreater(main.evaluate("el => el.scrollTop"), 0)
        # The comment the sync adds grows the conversation above the
        # viewport, so the scroll anchoring that keeps this row's place
        # also moves scrollTop. Its place is what mustn't move.
        row = file.locator("tr.line").nth(150)
        y = row.bounding_box()["y"]
        # A remount of the diff would lose the marker.
        file.evaluate("el => el.setAttribute('data-marker', '1')")

        self.resync_pr(pr)
        self.assertAlmostEqual(row.bounding_box()["y"], y, delta=2)
        expect(file).to_have_attribute("data-marker", "1")

    def test_an_inline_thread_does_not_rebuild_the_diff(self) -> None:
        pr = self.make_pr(
            "review/inline",
            "Take an inline comment",
            {scenarios.REVIEW_PATH: "".join(f"Line {i}\n" for i in range(1, 300))},
        )
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        file = self.file()
        file.evaluate("el => el.scrollIntoView({block: 'center'})")
        main = self.page.locator(".pr-page .main")
        self.assertGreater(main.evaluate("el => el.scrollTop"), 0)
        file.evaluate("el => el.setAttribute('data-marker', '1')")

        pr.repo.comment(
            pr,
            author=scenarios.OTHER,
            body="Out of band.",
            path=scenarios.REVIEW_PATH,
            line=6,
        )
        self.resync_pr(pr)
        expect(file.locator(".thread", has_text="Out of band.")).to_be_visible()
        # The thread's row reflows the file above the viewport, but the
        # scroll mustn't reset the way a remount would.
        self.assertGreater(main.evaluate("el => el.scrollTop"), 0)
        expect(file).to_have_attribute("data-marker", "1")

    def test_a_push_offers_the_new_version(self) -> None:
        pr = self.make_pr(
            "review/push-notice",
            "Notice a push",
            {"notice/a.txt": "a\n"},
        )
        v1 = pr.head_oid
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        picker = self.page.locator("details.picker")
        expect(picker.locator("summary")).to_have_text(f"Base → v1 ({v1[:8]})")

        pr.repo.checkout(pr.head)
        v2 = pr.repo.commit("More", {"notice/b.txt": "b\n"})
        pr.repo.push(pr.head)
        self.resync_pr(pr)
        notice = self.page.locator(".new-version-notice")
        expect(notice).to_contain_text("Version 2 was pushed")
        # The diff stays on v1 until asked.
        expect(picker.locator("summary")).to_have_text(f"Base → v1 ({v1[:8]})")

        notice.get_by_role("button", name="Show it").click()
        expect(picker.locator("summary")).to_have_text(f"Base → v2 ({v2[:8]})")
        expect(notice).to_have_count(0)

    def test_a_push_offers_the_new_version_with_a_selection(self) -> None:
        pr = self.make_pr(
            "review/push-picked",
            "Pick, then push",
            {"pick/a.txt": "a\n"},
        )
        v1 = pr.head_oid
        pr.repo.checkout(pr.head)
        pr.repo.commit("Second", {"pick/b.txt": "b\n"})
        v2 = pr.repo.push(pr.head)
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        picker = self.page.locator("details.picker")

        # Picking v1 → v2 by hand doesn't keep a push from being offered.
        picker.locator("summary").click()
        rows = picker.locator("tbody tr")
        rows.nth(2).locator("input[name=head]").check()
        rows.nth(1).locator("input[name=base]").check()
        self.page.keyboard.press("Escape")
        expect(picker.locator("summary")).to_have_text(f"v1 ({v1[:8]}) → v2 ({v2[:8]})")

        pr.repo.checkout(pr.head)
        v3 = pr.repo.commit("Third", {"pick/c.txt": "c\n"})
        pr.repo.push(pr.head)
        self.resync_pr(pr)
        notice = self.page.locator(".new-version-notice")
        expect(notice).to_contain_text("Version 3 was pushed")

        notice.get_by_role("button", name="Show it").click()
        expect(picker.locator("summary")).to_have_text(f"Base → v3 ({v3[:8]})")
        expect(notice).to_have_count(0)

    def resync_pr(self, pr: PullRequest) -> None:
        """Comments on `pr` from outside the page, which resyncs it, and
        waits for the page to show the comment."""
        resp = self.page.request.post(
            f"{self.url}api/pr/{scenarios.REPO}/{pr.number}/comments",
            headers={"Origin": self.url.rstrip("/")},
            data={"body": "Ping"},
        )
        self.assertEqual(resp.status, 204, resp.text())
        expect(self.page.locator("details.conversation summary")).to_have_text(
            f"{len(pr.issue_comments)} comment"
            + ("" if len(pr.issue_comments) == 1 else "s")
        )

    def resync(self, title: str) -> None:
        """Comments on the PR from outside the page, which resyncs it, and
        waits for the page to show the comment."""
        self.resync_pr(self.prs[title])

    def test_squash_and_merge(self) -> None:
        pr = self.prs[scenarios.SQUASH]
        self.open(scenarios.SQUASH)
        box = self.merge_box()
        method = box.get_by_label("Merge method")
        # GitHub's default for the viewer comes first.
        expect(method).to_have_value("MERGE")
        method.select_option(label="Squash and merge")
        box.get_by_role("button", name="Edit the commit message").click()
        title = box.get_by_label("Commit title")
        expect(title).to_have_value(f"{pr.title} (#{pr.number})")
        title.fill("Squash it all")
        expect(box.get_by_label("Delete the branch")).to_be_checked()

        messages = self.merge()
        expect(self.page.locator(".pr-header .state")).to_have_text("merged")
        expect(self.merge_box()).to_have_count(0)
        self.assertEqual(messages, [f"Squash and merge #{pr.number} into main?"])
        self.assertEqual(pr.merged_with, ("SQUASH", "Squash it all", pr.body))
        self.assertIn(pr.head, pr.repo.deleted_branches)

    def test_merge_refuses_a_head_it_did_not_show(self) -> None:
        pr = self.prs[scenarios.STALE_HEAD]
        self.open(scenarios.STALE_HEAD)
        expect(self.merge_box().get_by_role("button", name="Merge")).to_be_enabled()
        # Someone pushes; the page doesn't know yet.
        pr.repo.checkout(pr.head)
        pr.repo.commit(
            "Sneak in", {"review/sneaky.md": "boo\n"}, author=scenarios.OTHER
        )
        pr.repo.push(pr.head, actor=scenarios.OTHER)

        self.merge()
        expect(self.merge_box().locator(".error")).to_contain_text(
            "Head branch was modified"
        )
        self.assertEqual(pr.state, "OPEN")
        # The refusal resyncs the PR, so the page offers the push. Until
        # it's shown, the merge box still expects the old head.
        notice = self.page.locator(".new-version-notice")
        expect(notice).to_contain_text("Version 2 was pushed")
        self.merge()
        expect(self.merge_box().locator(".error")).to_contain_text(
            "Head branch was modified"
        )
        self.assertEqual(pr.state, "OPEN")
        notice.get_by_role("button", name="Show it").click()
        expect(self.page.locator("details.picker summary")).to_have_text(
            f"Base → v2 ({pr.head_oid[:8]})"
        )
        self.merge()
        expect(self.page.locator(".pr-header .state")).to_have_text("merged")
        self.assertEqual(pr.state, "MERGED")

    def test_blocked_prs_cannot_be_merged(self) -> None:
        self.open(scenarios.BLOCKED)
        box = self.merge_box()
        expect(box.locator(".merge-blocked")).to_have_text(
            "The branch's rules block merging."
        )
        expect(box.get_by_role("button", name="Merge")).to_be_disabled()

    def test_unresolved_conversations_block_merging(self) -> None:
        self.open(scenarios.CONVERSATIONS)
        box = self.merge_box()
        expect(box.locator(".merge-status")).to_have_text(
            "Merging is blocked: 1 conversation must be resolved."
        )
        expect(box.locator(".merge-blocked")).to_have_text(
            "Resolve the conversations first."
        )
        expect(box.get_by_role("button", name="Merge")).to_be_disabled()

        self.thread().get_by_role("button", name="Resolve").click()
        expect(box.locator(".merge-status")).to_have_text("Ready to merge.")
        expect(box.get_by_role("button", name="Merge")).to_be_enabled()

    def test_merge_bypassing_the_rules(self) -> None:
        pr = self.prs[scenarios.BYPASS]
        self.open(scenarios.BYPASS)
        box = self.merge_box()
        expect(box.locator(".merge-blocked")).to_have_count(0)
        button = box.get_by_role("button", name="Merge, bypassing rules")
        expect(button).to_be_enabled()
        expect(button).to_have_class("danger")
        messages = self.merge()
        expect(self.page.locator(".pr-header .state")).to_have_text("merged")
        dialog = (
            f"Create a merge commit #{pr.number} into main, bypassing the"
            " branch's rules?\n\nMerging is blocked: it needs an approving review."
        )
        self.assertEqual(messages, [dialog])
        self.assertEqual(pr.state, "MERGED")

    def test_merged_but_the_branch_stays(self) -> None:
        pr = self.prs[scenarios.KEEP_BRANCH]
        self.open(scenarios.KEEP_BRANCH)
        expect(self.merge_box().get_by_label("Delete the branch")).to_be_checked()
        self.merge()
        expect(self.page.locator(".pr-header .state")).to_have_text("merged")
        # The warning outlasts the merge box.
        expect(self.merge_box()).to_have_count(0)
        warning = self.page.locator(".banner.merge-warning")
        expect(warning).to_contain_text("Merged, but the branch wasn't deleted:")
        expect(warning).to_contain_text("Cannot delete protected branch")
        self.assertEqual(pr.state, "MERGED")
        self.assertNotIn(pr.head, pr.repo.deleted_branches)

    def test_no_branch_to_delete_when_github_deletes_it(self) -> None:
        pr = self.prs[scenarios.AUTO_DELETE]
        self.open(scenarios.AUTO_DELETE)
        delete = self.merge_box().get_by_label("Delete the branch")
        expect(delete).to_have_count(1)
        pr.repo.delete_branch_on_merge = True
        self.addCleanup(setattr, pr.repo, "delete_branch_on_merge", False)
        self.resync(scenarios.AUTO_DELETE)
        expect(delete).to_have_count(0)

    def test_merging_a_stacked_pr(self) -> None:
        parent = self.prs[scenarios.STACK_PARENT]
        self.open(scenarios.STACK_CHILD)
        warning = self.merge_box().locator(".banner.warn")
        expect(warning).to_have_text(
            f"This PR targets the branch of #{parent.number}, so merging it puts"
            " its changes there, not into main."
        )
        # Deleting the parent's branch would close the child.
        self.open(scenarios.STACK_PARENT)
        box = self.merge_box()
        expect(box.locator(".banner.warn")).to_have_count(0)
        expect(box.get_by_label("Delete the branch")).not_to_be_checked()

    def test_merge_waits_for_github_to_decide(self) -> None:
        pr = self.prs[scenarios.SETTLE]
        self.open(scenarios.SETTLE)
        button = self.merge_box().get_by_role("button", name="Merge")
        expect(button).to_be_enabled()
        # Something makes GitHub work out the merge state again.
        pr.merge_state = "UNKNOWN"
        self.resync(scenarios.SETTLE)
        expect(self.merge_box().locator(".merge-blocked")).to_have_text(
            "GitHub hasn't worked out yet whether this can be merged."
        )
        expect(button).to_be_disabled()
        # Once it has, nothing about the PR changes, but the server syncs
        # it again a little later anyway.
        pr.merge_state = None
        expect(button).to_be_enabled(timeout=30_000)

    def test_reply_to_a_thread(self) -> None:
        self.open(scenarios.REPLY)
        thread = self.thread()
        thread.get_by_role("button", name="Reply").click()
        box = thread.get_by_role("textbox", name="Reply")
        expect(box).to_be_focused()
        expect(thread.locator(".composer-hint")).to_have_text("Ctrl+Enter sends")
        box.fill("It is, see **the docs**.")

        # The preview renders the markdown.
        thread.get_by_role("tab", name="Preview").click()
        expect(thread.locator(".composer-preview strong")).to_have_text("the docs")
        thread.get_by_role("tab", name="Write").click()

        box.press("Control+Enter")
        expect(thread.locator(".comment")).to_have_count(2)
        expect(thread.locator(".comment").nth(1)).to_contain_text(
            "It is, see the docs."
        )
        expect(
            thread.locator(".comment").nth(1).locator(".comment-meta")
        ).to_contain_text("me")
        reply = self.prs[scenarios.REPLY].comments[-1]
        self.assertEqual(reply.author, "me")
        self.assertEqual(reply.in_reply_to, self.prs[scenarios.REPLY].comments[0].id)

    def test_reply_drafts_survive_a_reload(self) -> None:
        self.open(scenarios.REPLY)
        self.thread().get_by_role("button", name="Reply").click()
        self.thread().get_by_role("textbox", name="Reply").fill("Half a thought")
        self.page.reload()
        expect(self.thread().get_by_role("textbox", name="Reply")).to_have_value(
            "Half a thought"
        )
        # Cancel drops the draft.
        self.thread().get_by_role("button", name="Cancel").click()
        expect(self.thread().get_by_role("button", name="Reply")).to_be_visible()
        self.page.reload()
        expect(self.thread().get_by_role("button", name="Reply")).to_be_visible()

    def test_resolve_and_unresolve_a_thread(self) -> None:
        self.open(scenarios.RESOLVE)
        thread = self.thread()
        thread.get_by_role("button", name="Resolve").click()
        # Resolved threads fold.
        expect(thread.locator(".thread-header .thread-resolved")).to_have_text(
            "Resolved by Me Myself"
        )
        expect(thread.locator(".thread-header")).to_have_attribute(
            "aria-expanded", "false"
        )
        self.assertTrue(self.prs[scenarios.RESOLVE].comments[0].resolved)

        thread.locator(".thread-header").click()
        thread.get_by_role("button", name="Unresolve").click()
        expect(thread.locator(".thread-resolved")).to_have_count(0)
        self.assertFalse(self.prs[scenarios.RESOLVE].comments[0].resolved)

    def test_a_failed_reply_keeps_its_box(self) -> None:
        self.open(scenarios.FAIL)
        thread = self.thread()
        thread.get_by_role("button", name="Reply").click()
        box = thread.get_by_role("textbox", name="Reply")
        box.fill("Not lost")
        self.gh.fail_next_mutation = "Something went wrong"
        box.press("Control+Enter")
        expect(thread.locator(".comment-composer .error")).to_contain_text(
            "Something went wrong"
        )
        expect(box).to_have_value("Not lost")
        expect(thread.locator(".comment")).to_have_count(1)

        # Sending again works, and closes the box.
        box.press("Control+Enter")
        expect(thread.locator(".comment")).to_have_count(2)
        expect(thread.get_by_role("textbox")).to_have_count(0)
        # The draft is gone with it.
        self.page.reload()
        expect(self.thread().get_by_role("button", name="Reply")).to_be_visible()

    def test_a_thread_the_viewer_cannot_resolve(self) -> None:
        self.open(scenarios.LOCKED)
        thread = self.thread()
        expect(thread.get_by_role("button", name="Reply")).to_be_visible()
        expect(thread.get_by_role("button", name="Resolve")).to_have_count(0)

    def test_the_api_refuses_bad_writes(self) -> None:
        pr = self.prs[scenarios.FAIL]
        api = f"{self.url}api/pr/{scenarios.REPO}/{pr.number}"
        thread = f"PRRT_{pr.comments[0].id}"
        own = {"Origin": self.url.rstrip("/")}
        request = self.page.request

        def refused(status: int, text: str, url: str, **kwargs: Any) -> None:
            resp = request.fetch(
                url,
                method=kwargs.pop("method", "POST"),
                headers=kwargs.pop("headers", own),
                **kwargs,
            )
            self.assertEqual(resp.status, status, resp.text())
            self.assertIn(text, resp.text())

        refused(400, "the comment is empty", f"{api}/comments", data={"body": " "})
        refused(
            404,
            "is not a review thread",
            f"{api}/threads/PRRT_nope/replies",
            data={"body": "Hi"},
        )
        refused(
            404,
            "is not a configured repository",
            f"{self.url}api/pr/elsewhere/repo/1/comments",
            data={"body": "Hi"},
        )
        # Another site's page can't resolve threads (CSRF).
        refused(
            403,
            "cross-origin",
            f"{api}/threads/{thread}/resolve",
            headers={"Origin": "http://evil.example"},
        )
        # Threads GitHub would refuse with an opaque error.
        line = {"path": scenarios.REVIEW_PATH, "side": "Right", "body": "Hi"}
        refused(
            400,
            "the path is empty",
            f"{api}/review/comments",
            data={**line, "path": "", "line": 1},
        )
        refused(
            400,
            "numbered from 1",
            f"{api}/review/comments",
            data={**line, "line": 0},
        )
        refused(
            400,
            "Say what needs to change",
            f"{api}/review",
            data={"event": "RequestChanges", "body": " "},
        )
        refused(
            404,
            "is not a pending comment",
            f"{api}/review/comments/PRRC_nope",
            method="PATCH",
            data={"body": "Hi"},
        )
        refused(
            404,
            "is not a pending comment",
            f"{api}/review/comments/PRRC_nope",
            method="DELETE",
        )
        self.assertFalse(pr.comments[0].resolved)
        self.assertEqual(len(pr.issue_comments), 0)
        self.assertIsNone(pr.pending)
        self.assertEqual(pr.reviews, [])

    def test_comment_on_the_pr(self) -> None:
        self.open(scenarios.CONVERSE)
        conversation = self.page.locator("details.conversation")
        expect(conversation.locator("summary")).to_have_text("No comments")
        conversation.get_by_role("button", name="Comment").click()
        conversation.get_by_role("textbox", name="Comment").fill("Looks good to me.")
        conversation.get_by_role("button", name="Comment").click()
        expect(conversation.locator("summary")).to_have_text("1 comment")
        expect(conversation.locator(".comment .markdown")).to_have_text(
            "Looks good to me."
        )
        # The box closes once the comment is sent.
        expect(conversation.get_by_role("textbox")).to_have_count(0)

    def test_settings_dialog_only_has_the_theme(self) -> None:
        self.open(scenarios.CONVERSE)
        expect(self.page.get_by_label("Open PR links in grenadine")).to_have_count(0)

        self.page.get_by_role("button", name="Settings", exact=True).click()
        dialog = self.page.get_by_role("dialog", name="Settings")
        expect(dialog.get_by_label("Theme")).to_have_count(1)
        expect(dialog.get_by_label("Open PR links in grenadine")).to_have_count(0)


class QueueTest(ServerTestCase):
    """The merge box on a repository whose base branch requires a merge
    queue."""

    REPOS: ClassVar[list[str]] = [scenarios.REPO, scenarios.QUEUE_REPO]

    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        gh.repo(scenarios.REPO)
        cls.prs = scenarios.queue_cases(gh)

    def open(self, title: str) -> None:
        pr = self.prs[title]
        self.page.goto(f"{self.url}#/{scenarios.QUEUE_REPO}/{pr.number}")
        expect(self.page.locator(".pr-header h1")).to_have_text(
            f"{pr.title} #{pr.number}"
        )

    def status(self) -> Locator:
        return self.merge_box().locator(".merge-status")

    def click(self, name: str) -> list[str]:
        """Clicks the merge box's button `name`, accepting the dialog it
        opens. Returns what the dialog said."""
        messages: list[str] = []

        def accept(dialog: Dialog) -> None:
            messages.append(dialog.message)
            dialog.accept()

        self.page.once("dialog", accept)
        self.merge_box().get_by_role("button", name=name).click()
        return messages

    def test_joining_the_queue(self) -> None:
        pr = self.prs[scenarios.QUEUE_READY]
        self.open(scenarios.QUEUE_READY)
        box = self.merge_box()
        expect(self.status()).to_have_text("Ready to join the merge queue.")
        expect(box.locator(".queue-method")).to_have_text(
            "The merge queue squashes and merges this PR."
        )
        # A queue branch offers no plain merge.
        expect(box.get_by_label("Merge method")).to_have_count(0)
        expect(box.get_by_role("button", name="Merge", exact=True)).to_have_count(0)
        expect(box.get_by_label("Delete the branch")).to_have_count(0)
        expect(box.get_by_role("button", name="Edit the commit message")).to_have_count(
            0
        )

        messages = self.click("Add to merge queue")
        self.assertEqual(messages, [f"Add #{pr.number} to the merge queue for main?"])
        expect(self.status()).to_have_text("In the merge queue: position 1 (queued).")
        self.assertTrue(pr.queued)
        expect(box.get_by_role("button", name="Remove from queue")).to_be_enabled()

    def test_leaving_the_queue(self) -> None:
        pr = self.prs[scenarios.QUEUED]
        self.open(scenarios.QUEUED)
        expect(self.status()).to_have_text("In the merge queue: position 1 (queued).")
        messages = self.click("Remove from queue")
        self.assertEqual(messages, [f"Remove #{pr.number} from the merge queue?"])
        expect(self.status()).to_have_text("Ready to join the merge queue.")
        self.assertFalse(pr.queued)
        expect(
            self.merge_box().get_by_role("button", name="Add to merge queue")
        ).to_be_enabled()

    def test_merging_when_ready(self) -> None:
        pr = self.prs[scenarios.QUEUE_WAITING]
        self.open(scenarios.QUEUE_WAITING)
        blocked = "Merging is blocked: required checks or rules aren't satisfied."
        expect(self.status()).to_have_text(blocked)
        expect(
            self.merge_box().get_by_role("button", name="Merge", exact=True)
        ).to_have_count(0)

        messages = self.click("Merge when ready")
        self.assertEqual(
            messages, [f"Merge #{pr.number} when its requirements are met?"]
        )
        expect(self.status()).to_have_text("Will join the merge queue when ready.")
        self.assertIsNotNone(pr.auto_merge_at)

        messages = self.click("Cancel")
        self.assertEqual(
            messages, [f"Stop #{pr.number} joining the merge queue when ready?"]
        )
        expect(self.status()).to_have_text(blocked)
        self.assertIsNone(pr.auto_merge_at)

    def test_blocked_without_auto_merge(self) -> None:
        self.open(scenarios.QUEUE_NO_AUTO)
        box = self.merge_box()
        expect(box.locator(".merge-blocked")).to_have_text(
            "The branch's rules block merging."
        )
        expect(box.get_by_role("button", name="Add to merge queue")).to_have_count(0)
        expect(box.get_by_role("button", name="Merge when ready")).to_have_count(0)
        expect(box.get_by_role("button", name="Merge", exact=True)).to_have_count(0)

    def test_add_to_front_of_queue(self) -> None:
        pr = self.prs[scenarios.QUEUE_ADMIN]
        self.open(scenarios.QUEUE_ADMIN)
        box = self.merge_box()
        expect(box.get_by_role("button", name="Add to front of queue")).to_be_enabled()
        expect(
            box.get_by_role("button", name="Merge now, bypassing the queue…")
        ).to_be_enabled()

        messages = self.click("Add to front of queue")
        self.assertEqual(
            messages,
            [f"Add #{pr.number} to the front of the merge queue for main?"],
        )
        expect(self.status()).to_contain_text("In the merge queue")
        self.assertTrue(pr.queued)
        self.assertTrue(pr.queue_jump)
        # Leave the queue again so later tests see fixed positions.
        self.click("Remove from queue")
        expect(self.status()).to_have_text("Ready to join the merge queue.")
        self.assertFalse(pr.queued)

    def test_merge_now_bypassing_the_queue(self) -> None:
        pr = self.prs[scenarios.QUEUE_BYPASS]
        self.open(scenarios.QUEUE_BYPASS)
        box = self.merge_box()
        box.get_by_role("button", name="Merge now, bypassing the queue…").click()
        method = box.get_by_label("Merge method")
        expect(method).to_have_value("MERGE")
        expect(box.get_by_label("Delete the branch")).to_be_checked()

        messages: list[str] = []

        def accept(dialog: Dialog) -> None:
            messages.append(dialog.message)
            dialog.accept()

        self.page.once("dialog", accept)
        box.get_by_role("button", name="Merge, bypassing rules").click()
        expect(self.page.locator(".pr-header .state")).to_have_text("merged")
        self.assertEqual(pr.state, "MERGED")
        self.assertEqual(len(messages), 1)
        self.assertIn("bypassing the branch's rules", messages[0])


class StackMergeTest(ServerTestCase):
    """Merging a PR in a GitHub stack through the merge-async API."""

    REPOS: ClassVar[list[str]] = [scenarios.GHSTACK_REPO, scenarios.QUEUE_REPO]

    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        cls.prs = scenarios.stack_merge_cases(gh)

    def open(self, title: str, repo: str = scenarios.GHSTACK_REPO) -> None:
        pr = self.prs[title]
        self.page.goto(f"{self.url}#/{repo}/{pr.number}")
        expect(self.page.locator(".pr-header h1")).to_have_text(
            f"{pr.title} #{pr.number}"
        )

    def test_merging_the_stack_top_merges_below_first(self) -> None:
        low = self.prs[scenarios.GH_LOW]
        top = self.prs[scenarios.GH_TOP]
        self.open(scenarios.GH_TOP)
        box = self.merge_box()
        banner = box.locator(".banner.warn")
        expect(banner).to_contain_text(f"Merging #{top.number} also merges")
        expect(banner).to_contain_text(f"#{low.number}")
        expect(banner).to_contain_text("below it")
        expect(banner).to_contain_text("Each lands on main separately, in stack order")
        rows = box.locator(".merge-stack li")
        expect(rows).to_have_count(1)
        expect(rows.nth(0)).to_contain_text(f"#{low.number} {low.title}")
        expect(rows.nth(0)).to_contain_text("Ready")
        # A stack merge offers no commit message to edit.
        box.get_by_label("Merge method").select_option(label="Squash and merge")
        expect(box.get_by_role("button", name="Edit the commit message")).to_have_count(
            0
        )
        expect(
            box.get_by_label(f"Delete the branches ({top.head}, {low.head})")
        ).to_be_checked()

        messages = self.merge()
        self.assertEqual(
            messages,
            [f"Squash and merge #{low.number}, then #{top.number} into main?"],
        )
        expect(self.page.locator(".pr-header .state")).to_have_text("merged")
        self.assertEqual(top.state, "MERGED")
        self.assertEqual(low.state, "MERGED")
        # Both head branches went with it.
        self.assertIn(top.head, top.repo.deleted_branches)
        self.assertIn(low.head, top.repo.deleted_branches)
        # The stack's commits each land on main, in order.
        self.assertEqual(top.merged_with, ("SQUASH", None, None))
        self.assertEqual(low.merged_with, ("SQUASH", None, None))
        # The top PR's base branch is gone, and it still syncs from the
        # commit GitHub remembers for it.
        self.page.reload()
        expect(self.page.locator(".pr-header .state")).to_have_text("merged")
        expect(self.page.locator(".banner.error")).to_have_count(0)

    def test_merging_the_stack_bottom_leaves_the_top(self) -> None:
        low = self.prs[scenarios.GH_LOW]
        top = self.prs[scenarios.GH_TOP]
        self.open(scenarios.GH_LOW)
        box = self.merge_box()
        expect(box.locator(".banner.warn")).to_have_count(0)
        expect(box.locator(".merge-stack li")).to_have_count(0)
        self.merge()
        expect(self.page.locator(".pr-header .state")).to_have_text("merged")
        self.assertEqual(low.state, "MERGED")
        self.assertEqual(top.state, "OPEN")

    def test_a_draft_below_blocks_merging(self) -> None:
        low = self.prs[scenarios.GH_DRAFT_LOW]
        self.open(scenarios.GH_DRAFT)
        box = self.merge_box()
        rows = box.locator(".merge-stack li")
        expect(rows.nth(0)).to_contain_text("Draft")
        expect(box.locator(".merge-blocked")).to_have_text(f"#{low.number} is a draft.")
        expect(box.get_by_role("button", name=re.compile("^Merge"))).to_be_disabled()

    def test_a_blocked_below_merges_by_bypass(self) -> None:
        low = self.prs[scenarios.GH_BLOCKED_LOW]
        top = self.prs[scenarios.GH_BLOCKED]
        self.open(scenarios.GH_BLOCKED)
        box = self.merge_box()
        rows = box.locator(".merge-stack li")
        expect(rows.nth(0)).to_contain_text("Blocked by rules")
        button = box.get_by_role("button", name="Merge 2 PRs, bypassing rules")
        expect(button).to_be_enabled()
        expect(button).to_have_class("danger")
        messages = self.merge()
        self.assertIn("bypassing the branch's rules", messages[0])
        expect(self.page.locator(".pr-header .state")).to_have_text("merged")
        self.assertEqual(top.state, "MERGED")
        self.assertEqual(low.state, "MERGED")

    def test_a_merge_failing_at_run_time_shows_the_error(self) -> None:
        low = self.prs[scenarios.GH_FAIL_LOW]
        top = self.prs[scenarios.GH_FAIL]
        self.open(scenarios.GH_FAIL)
        expect(
            self.merge_box().get_by_role("button", name="Merge 2 PRs")
        ).to_be_enabled()
        # The below PR turns unmergeable after the page read it.
        low.merge_state = "BLOCKED"
        self.merge()
        expect(self.merge_box().locator(".error")).to_contain_text(
            "Pull Request is not mergeable"
        )
        self.assertEqual(low.state, "OPEN")
        self.assertEqual(top.state, "OPEN")

    def test_a_stacked_pr_queues_its_stack(self) -> None:
        low = self.prs[scenarios.GHQ_LOW]
        top = self.prs[scenarios.GHQ_TOP]
        self.open(scenarios.GHQ_TOP, repo=scenarios.QUEUE_REPO)
        box = self.merge_box()
        rows = box.locator(".merge-stack li")
        expect(rows.nth(0)).to_contain_text(f"#{low.number} {low.title}")
        # A stack has no jump and no merge-when-ready.
        expect(box.get_by_role("button", name="Add to front of queue")).to_have_count(0)
        expect(box.get_by_role("button", name="Merge when ready")).to_have_count(0)

        messages: list[str] = []

        def accept(dialog: Dialog) -> None:
            messages.append(dialog.message)
            dialog.accept()

        self.page.once("dialog", accept)
        box.get_by_role("button", name="Add 2 PRs to merge queue").click()
        expect(box.locator(".merge-status")).to_contain_text("In the merge queue")
        self.assertTrue(low.queued)
        self.assertTrue(top.queued)


class VersionsTest(ServerTestCase):
    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        cls.cases = scenarios.versions_cases(gh)
        grow = cls.cases.prs[scenarios.GROW]
        shas = cls.cases.shas[scenarios.GROW]
        # The viewer last reviewed v3; someone else's later approval of the
        # head doesn't count.
        grow.reviews.append(
            Review(scenarios.VIEWER, COMMENTED, commit=shas[2], submitted_at=gh.tick())
        )
        grow.reviews.append(
            Review(scenarios.OTHER, APPROVED, commit=shas[-1], submitted_at=gh.tick())
        )
        # A thread made on v1 shows on the left of diffs starting at v1.
        repo = gh.repo(scenarios.REPO)
        repo.comment(
            grow,
            author=scenarios.OTHER,
            body="Left over from v1.",
            path=scenarios.STEPS_PY,
            line=9,
            commit=shas[0],
        )
        # A thread on the head stays on the right.
        repo.comment(
            grow,
            author=scenarios.OTHER,
            body="Doubling, eh?",
            path=scenarios.STEPS_PY,
            line=20,
            commit=shas[-1],
        )
        # A multi-line thread on the head labels and marks its range.
        repo.comment(
            grow,
            author=scenarios.OTHER,
            body="These two steps look off.",
            path=scenarios.STEPS_PY,
            line=21,
            start_line=16,
            commit=shas[-1],
        )
        # Threads on v1 of a rebased PR: one on a line the merge keeps,
        # one on a line upstream replaced, and one on v2.
        rebase_edit = cls.cases.prs[scenarios.REBASE_AND_EDIT]
        r_shas = cls.cases.shas[scenarios.REBASE_AND_EDIT]
        for line, body in (
            (5, "Kept through the merge."),
            (50, "Dropped by the merge."),
        ):
            repo.comment(
                rebase_edit,
                author=scenarios.OTHER,
                body=body,
                path="rebase_and_edit/shared.py",
                line=line,
                commit=r_shas[0],
            )
        repo.comment(
            rebase_edit,
            author=scenarios.OTHER,
            body="Made on v2.",
            path="rebase_and_edit/shared.py",
            line=7,
            commit=r_shas[1],
        )

    def open(self, title: str) -> None:
        pr = self.cases.prs[title]
        self.shas = self.cases.shas[title]
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        expect(self.page.locator(".pr-header h1")).to_have_text(
            f"{pr.title} #{pr.number}"
        )

    def choose(self, base: int, head: int) -> None:
        """Picks versions to diff; 0 is the base."""
        picker = self.page.locator("details.picker")
        picker.locator("summary").click()
        rows = picker.locator("tbody tr")
        # Head first: a base at or past the head would move the head.
        rows.nth(head).locator("input[name=head]").check()
        rows.nth(base).locator("input[name=base]").check()
        self.page.keyboard.press("Escape")
        name = "Base" if base == 0 else f"v{base} ({self.shas[base - 1][:8]})"
        expect(picker.locator("summary")).to_have_text(
            f"{name} → v{head} ({self.shas[head - 1][:8]})"
        )

    def file(self, path: str) -> Locator:
        return self.page.locator("section.file").filter(
            has=self.page.locator(".file-header .path", has_text=exactly(path))
        )

    def test_versions_follow_the_pushes(self) -> None:
        self.open(scenarios.GROW)
        shas = self.cases.shas[scenarios.GROW]
        picker = self.page.locator("details.picker")
        expect(picker.locator("summary")).to_have_text(f"Base → v6 ({shas[-1][:8]})")

        picker.locator("summary").click()
        rows = picker.locator("tbody tr")
        expect(rows).to_have_count(len(shas) + 1)
        expect(rows.locator(".version code")).to_have_text([s[:8] for s in shas])
        expect(rows.locator(".vkind")).to_have_text(
            ["opened", "push", "push", "push", "push", "force push"]
        )
        expect(rows.locator(".pushed-by")).to_have_text(
            [""] + ["Me Myself"] * len(shas)
        )
        # Base can't be the latest version, and Head can't be the base.
        expect(rows.nth(len(shas)).locator("input[name=base]")).to_be_disabled()
        expect(rows.nth(0).locator("input[name=head]")).to_be_disabled()

    def test_marks_the_last_reviewed_version(self) -> None:
        self.open(scenarios.GROW)
        picker = self.page.locator("details.picker")
        picker.locator("summary").click()
        rows = picker.locator("tbody tr")
        expect(picker.locator("tr.reviewed")).to_have_count(1)
        expect(rows.nth(3)).to_have_class(re.compile(r"(?:^| )reviewed(?: |$)"))
        expect(rows.nth(3).locator(".vreviewed")).to_have_text("last reviewed")
        expect(picker.locator(".vreviewed")).to_have_count(1)

    def test_lines_are_commentable_only_against_the_base(self) -> None:
        self.open(scenarios.GROW)
        lines = self.file(scenarios.STEPS_PY).locator("td.ln.commentable")
        expect(lines.first).to_be_visible()
        # GitHub anchors comments to the diff from the base, which can
        # differ from one between versions.
        self.choose(5, 6)
        expect(self.file(scenarios.STEPS_PY).locator("table")).to_be_visible()
        expect(lines).to_have_count(0)
        expect(self.page.get_by_role("button", name="Comment on file")).to_have_count(0)

    def test_threads_fold(self) -> None:
        self.open(scenarios.GROW)
        threads = self.file(scenarios.STEPS_PY).locator(".thread")
        expect(threads).to_have_count(5)
        open_thread, resolved = threads.nth(0), threads.nth(1)

        # Open threads start unfolded, resolved ones folded.
        expect(open_thread.locator(".thread-header")).to_have_attribute(
            "aria-expanded", "true"
        )
        expect(open_thread.locator(".comment")).to_have_count(2)
        expect(open_thread.locator(".thread-resolved")).to_have_count(0)
        header = resolved.locator(".thread-header")
        expect(header).to_have_attribute("aria-expanded", "false")
        expect(resolved.locator(".comment")).to_have_count(0)
        expect(header.locator(".thread-snippet")).to_have_text("Rename this.")
        expect(header.locator(".thread-resolved")).to_have_text(
            "Resolved by Other Person"
        )

        # The header folds and unfolds.
        header.click()
        expect(resolved.locator(".comment")).to_have_count(1)
        expect(header.locator(".thread-snippet")).to_have_count(0)
        open_thread.locator(".thread-header").click()
        expect(open_thread.locator(".comment")).to_have_count(0)

        # Rebuilding the diff keeps what the user folded.
        self.choose(5, 6)
        threads = self.file(scenarios.STEPS_PY).locator(".thread")
        expect(threads.nth(0).locator(".comment")).to_have_count(0)
        expect(threads.nth(1).locator(".comment")).to_have_count(1)

    def test_diff_against_the_base(self) -> None:
        self.open(scenarios.GROW)
        expect(self.page.locator(".diff-summary")).to_have_text("1 files changed")
        file = self.file(scenarios.STEPS_PY)
        expect(file.locator(".status")).to_have_text("added")
        added = scenarios.steps(6, doubled=6).count("\n")
        expect(file.locator(".stats")).to_have_text(f"+{added} −0")

    def test_diff_between_versions(self) -> None:
        self.open(scenarios.GROW)
        self.choose(5, 6)
        file = self.file(scenarios.STEPS_PY)
        expect(file.locator(".status")).to_have_text("modified")
        expect(file.locator(".stats")).to_have_text("+1 −1")
        expect(file.locator("table.diff.split td.code.del")).to_have_text(
            "    return 6"
        )
        expect(file.locator("table.diff.split td.code.add")).to_have_text(
            "    return 6 * 2"
        )

        # The unified view shows the same change.
        self.page.get_by_label("Side by side").uncheck()
        expect(file.locator("table.diff.unified tr.line.del .code")).to_have_text(
            "    return 6"
        )
        expect(file.locator("table.diff.unified tr.line.add .code")).to_have_text(
            "    return 6 * 2"
        )

        # Three pushed commits became three versions, so v2 → v5 adds three
        # steps.
        self.choose(2, 5)
        expect(file.locator(".stats")).to_have_text("+12 −0")

    def row_above(self, file: Locator, thread_text: str) -> Locator:
        """The diff row a thread's comment row sits under."""
        return file.locator("tr.comment-row", has_text=thread_text).locator(
            "xpath=preceding-sibling::tr[1]"
        )

    def test_threads_on_the_diffs_left_version(self) -> None:
        self.open(scenarios.GROW)
        self.choose(1, 6)
        file = self.file(scenarios.STEPS_PY)
        # The thread made on v1 sits under its line on the left; the one
        # made on the head stays on the right.
        expect(
            self.row_above(file, "Left over from v1.").locator("td.ln").nth(0)
        ).to_have_text("9")
        expect(
            self.row_above(file, "Doubling, eh?").locator("td.ln").nth(1)
        ).to_have_text("20")

        # The unified view places them the same way.
        self.page.get_by_label("Side by side").uncheck()
        expect(
            self.row_above(file, "Left over from v1.").locator("td.ln").nth(0)
        ).to_have_text("9")
        expect(
            self.row_above(file, "Doubling, eh?").locator("td.ln").nth(1)
        ).to_have_text("20")

    def test_threads_on_the_left_version_rebased(self) -> None:
        self.open(scenarios.REBASE_AND_EDIT)
        self.choose(1, 2)
        file = self.file("rebase_and_edit/shared.py")
        expect(
            self.row_above(file, "Kept through the merge.").locator("td.ln").nth(0)
        ).to_have_text("5")
        expect(
            self.row_above(file, "Dropped by the merge.").locator("td.ln").nth(0)
        ).to_have_text("50")

        # In the rebased view the left side is the merged text: the thread
        # on the line upstream replaced moves to the list of lines the
        # diff doesn't show, while the v2 thread stays on the right.
        self.page.get_by_label("Rebase older version").check()
        expect(
            self.row_above(file, "Kept through the merge.").locator("td.ln").nth(0)
        ).to_have_text("5")
        expect(
            self.row_above(file, "Made on v2.").locator("td.ln").nth(1)
        ).to_have_text("7")
        outside = file.locator(
            ".outside-comments .outside", has_text="Dropped by the merge."
        )
        expect(outside).to_contain_text("old line 50")

    def test_a_multi_line_thread_shows_its_range(self) -> None:
        self.open(scenarios.GROW)
        file = self.file(scenarios.STEPS_PY)
        thread = file.locator(
            "tr.comment-row .thread", has_text="These two steps look off."
        )
        expect(thread.locator(".thread-lines")).to_have_text("Lines +16 to +21")
        marked = file.locator("tr.commented-range")
        expect(marked).to_have_count(6)
        self.assertEqual(
            [r.locator("td.ln").nth(1).inner_text() for r in marked.all()],
            ["16", "17", "18", "19", "20", "21"],
        )
        # Single-line threads have no label.
        expect(
            file.locator(".thread", has_text="Doubling, eh?").locator(".thread-lines")
        ).to_have_count(0)
        # The unified view marks the same rows.
        self.page.get_by_label("Side by side").uncheck()
        expect(file.locator("tr.commented-range")).to_have_count(6)

    def test_expand_context_buttons(self) -> None:
        # v1 → v2 of shared.py changes two far-apart lines, so its diff
        # has a short gap at the top, a long one in the middle, and a
        # short one at the end.
        self.open(scenarios.REBASE_AND_EDIT)
        self.choose(1, 2)
        file = self.file("rebase_and_edit/shared.py")
        skips = file.locator("tr.skip")
        expect(skips).to_have_count(3)
        expect(skips.nth(1)).to_contain_text("37 unchanged lines")
        # The short gaps get one "expand all"; the long one gets both
        # directions.
        expect(file.get_by_role("button", name="Expand all")).to_have_count(2)
        up = file.get_by_role("button", name="Expand up")
        down = file.get_by_role("button", name="Expand down")
        expect(up).to_have_count(1)
        expect(down).to_have_count(1)

        # Each click reveals 20 more lines; under 20 left the row offers
        # only "expand all".
        shown = file.locator("tr.line").count()
        up.click()
        expect(file.locator("tr.line")).to_have_count(shown + 20)
        expect(skips.nth(1)).to_contain_text("17 unchanged lines")
        expect(up).to_have_count(0)
        expect(down).to_have_count(0)
        skips.nth(1).get_by_role("button", name="Expand all").click()
        # The gap is fully shown: its row is gone and the hunks joined.
        expect(skips).to_have_count(2)
        # Both sides of the split view show the revealed line.
        expect(
            file.locator("td.code", has_text=exactly("SETTING_30 = 30"))
        ).to_have_count(2)

        # Toggling the view keeps the expansion.
        self.page.get_by_label("Side by side").uncheck()
        expect(skips).to_have_count(2)
        expect(
            file.locator("tr.line td.code", has_text=exactly("SETTING_30 = 30"))
        ).to_have_count(1)

    def test_expand_at_the_file_start(self) -> None:
        # v5 → v6 of steps.py changes one line near the end: a gap of 21
        # lines at the top, which only "expand up" can reach.
        self.open(scenarios.GROW)
        self.choose(5, 6)
        file = self.file(scenarios.STEPS_PY)
        skips = file.locator("tr.skip")
        expect(skips).to_have_count(1)
        expect(skips).to_contain_text("21 unchanged lines")
        up = file.get_by_role("button", name="Expand up")
        expect(up).to_have_count(1)
        expect(file.get_by_role("button", name="Expand down")).to_have_count(0)
        expect(file.get_by_role("button", name="Expand all")).to_have_count(0)
        up.click()
        expect(skips).to_contain_text("1 unchanged lines")
        file.get_by_role("button", name="Expand all").click()
        expect(skips).to_have_count(0)
        expect(
            file.locator(
                "td.code",
                has_text=exactly('"""One function per pushed version."""'),
            )
        ).to_have_count(2)

    def test_expanding_shows_a_hidden_thread(self) -> None:
        self.open(scenarios.REBASE)
        file = self.file("rebase/shared.py")
        outside = file.locator(".outside-comments")
        expect(outside).to_contain_text("Is 30 right?")
        skips = file.locator("tr.skip")
        down = file.get_by_role("button", name="Expand down")
        down.click()
        expect(outside).to_have_count(0)
        expect(
            file.locator("tr.comment-row .thread", has_text="Is 30 right?")
        ).to_have_count(1)
        # The gap after the change hides 50 lines; two more clicks of 20
        # and an "expand all" show it all.
        expect(skips.nth(1)).to_contain_text("30 unchanged lines")
        down.click()
        expect(skips.nth(1)).to_contain_text("10 unchanged lines")
        skips.nth(1).get_by_role("button", name="Expand all").click()
        expect(skips).to_have_count(1)

        # A revealed line takes a comment like any context line.
        file.locator('td.ln.commentable[data-side="R"]', has_text=exactly("30")).click()
        box = file.get_by_role("textbox", name="Add to review")
        box.fill("Thanks, it is.")
        box.press("Control+Enter")
        expect(box).to_have_count(0)
        pr = self.cases.prs[scenarios.REBASE]
        assert pr.pending
        [c] = pr.pending.comments
        self.assertEqual((c.line, c.side), (30, "RIGHT"))

    def test_rebase_only_changes_are_hidden(self) -> None:
        self.open(scenarios.REBASE)
        self.choose(1, 2)
        # Every change between the versions came in with the rebase, so in
        # both modes nothing is shown.
        expect(self.page.locator("p.empty")).to_have_text(
            "No changes. 3 files changed only by the rebase are hidden."
        )
        self.page.get_by_label("Rebase older version").check()
        expect(self.page.locator("p.empty")).to_have_text(
            "No changes. 3 files changed only by the rebase are hidden."
        )

    def test_rebase_with_an_edit_shows_the_edit(self) -> None:
        self.open(scenarios.REBASE_AND_EDIT)
        self.choose(1, 2)
        expect(self.page.locator(".diff-summary")).to_contain_text(
            "1 files changed · 2 files changed only by the rebase are hidden"
        )
        file = self.file("rebase_and_edit/shared.py")
        # The PR's own edit is a plain change; the upstream one is marked.
        expect(file.locator("tr.line:not(.upstream) td.code.add")).to_have_text(
            [scenarios.EDITED_SETTING]
        )
        expect(file.locator("tr.line.upstream td.code.add")).to_contain_text(
            ["changed upstream"]
        )

        # The rebased view diffs the older version replayed onto the newer
        # base, which leaves just the PR's edit.
        self.page.get_by_label("Rebase older version").check()
        expect(file.locator(".status")).to_have_text("modified")
        expect(file.locator(".stats")).to_have_text("+1 −1")
        expect(file.locator("tr.line.upstream")).to_have_count(0)
        expect(file.locator("td.code.add")).to_have_text([scenarios.EDITED_SETTING])
        expect(file.locator("td.code.del")).to_contain_text(["changed by the PR"])

    def test_edit_of_a_file_upstream_added(self) -> None:
        self.open(scenarios.UPSTREAM_FILE)
        self.choose(1, 2)
        expect(self.page.locator(".diff-summary")).to_contain_text("1 files changed")
        file = self.file(scenarios.UPSTREAM_ADDED)
        # The file is new between the versions: the upstream lines are
        # shaded, the PR's three lines are not.
        expect(file.locator(".status")).to_have_text("added")
        expect(file.locator("tr.line.upstream td.code.add")).to_have_count(18)
        expect(file.locator("tr.line:not(.upstream) td.code.add")).to_have_text(
            scenarios.UPSTREAM_ADDED_EDITS
        )

        # The rebased view shows the file as modified with only the PR's
        # changes.
        self.page.get_by_label("Rebase older version").check()
        expect(file.locator(".status")).to_have_text("modified")
        expect(file.locator("tr.line.upstream")).to_have_count(0)
        expect(file.locator("tr.line.mixed")).to_have_count(0)
        expect(file.locator("td.code.add")).to_have_text(scenarios.UPSTREAM_ADDED_EDITS)
        expect(file.locator("td.code.del")).to_have_text(
            ["added line 5", "added line 10"]
        )

    def test_conflict_with_upstream_shows_mixed(self) -> None:
        self.open(scenarios.CONFLICT)
        self.choose(1, 2)
        expect(self.page.locator(".diff-summary")).to_contain_text(
            "changes mixed with the rebase"
        )
        file = self.file(scenarios.CONFLICT_PATH)
        expect(file.locator("tr.line.mixed")).to_have_count(1)
        expect(
            file.locator("td.code.del:not(.mixed), td.code.add:not(.mixed)")
        ).to_have_count(0)
        expect(file.locator("td.code.del")).to_have_text([scenarios.PR_SETTING_10])
        expect(file.locator("td.code.add")).to_have_text(
            [scenarios.RESOLVED_SETTING_10]
        )

        self.page.get_by_label("Rebase older version").check()
        expect(file.locator("tr.line.mixed")).to_have_count(1)
        expect(
            file.locator("td.code.del:not(.mixed), td.code.add:not(.mixed)")
        ).to_have_count(0)

    def test_rebase_toggle_keeps_folds(self) -> None:
        self.open(scenarios.REBASE_AND_EDIT)
        self.choose(1, 2)
        file = self.file("rebase_and_edit/shared.py")
        file.locator(".file-header").click()
        expect(file.locator("table.diff")).to_have_count(0)
        # Flipping the mode re-renders the diff but doesn't remount the
        # files, so the fold survives.
        self.page.get_by_label("Rebase older version").check()
        expect(file.locator("table.diff")).to_have_count(0)
        self.page.get_by_label("Rebase older version").uncheck()
        expect(file.locator("table.diff")).to_have_count(0)

    def test_rebase_older_version_toggle(self) -> None:
        self.open(scenarios.REBASE)
        toggle = self.page.get_by_label("Rebase older version")
        label = self.page.locator("label.toggle", has_text="Rebase older version")
        # Diffing the base against the head leaves nothing to rebase.
        expect(toggle).to_be_disabled()
        expect(label).to_have_attribute(
            "title", "Both sides have the same base, so there's nothing to rebase."
        )
        self.choose(1, 2)
        expect(toggle).to_be_enabled()
        expect(label).to_have_attribute(
            "title",
            "Show the older version's changes replayed onto the newer version's base.",
        )
        # The choice survives a reload.
        toggle.check()
        expect(toggle).to_be_checked()
        self.page.reload()
        self.choose(1, 2)
        expect(toggle).to_be_checked()

    def test_rebased_version_against_the_base(self) -> None:
        # Against its own merge-base, the rebased version shows only the
        # PR's changes.
        self.open(scenarios.REBASE)
        expect(self.page.locator(".diff-summary")).to_have_text("1 files changed")
        file = self.file("rebase/shared.py")
        expect(file.locator(".stats")).to_have_text("+3 −3")
        expect(file.locator("tr.line.upstream")).to_have_count(0)

    def test_file_kinds(self) -> None:
        self.open(scenarios.FILE_KINDS)
        expect(self.page.locator(".diff-summary")).to_have_text("4 files changed")
        for path, status in [
            ("kinds/added.txt", "added"),
            ("kinds/deleted.txt", "deleted"),
            ("kinds/modified.txt", "modified"),
            ("kinds/old_name.txt → kinds/new_name.txt", "renamed"),
        ]:
            with self.subTest(path=path):
                expect(self.file(path).locator(".status")).to_have_text(status)
        expect(self.file("kinds/modified.txt").locator(".stats")).to_have_text("+1 −1")
        expect(
            self.file("kinds/old_name.txt → kinds/new_name.txt").locator(".stats")
        ).to_have_text("+0 −0")

    def test_big_diff_renders_every_file_highlighted(self) -> None:
        self.open(scenarios.BIG_DIFF)
        expect(self.page.locator(".diff-summary")).to_have_text(
            f"{len(scenarios.BIG_FILES)} files changed"
        )
        # The later files render after the first ones, plain at first and
        # highlighted once the background highlighter gets to them.
        expect(self.page.locator(".diff-placeholder")).to_have_count(0)
        for path in scenarios.BIG_FILES:
            with self.subTest(path=path):
                file = self.file(path)
                expect(file.locator("tr.line td.code.add")).to_have_count(300)
                expect(file.locator("td.code .sy-storage").first).to_have_text("def")

    def toolbar_bottom(self) -> float:
        box = self.page.locator(".toolbar").bounding_box()
        return box["y"] + box["height"]

    def test_file_headers_stick_below_the_toolbar(self) -> None:
        self.open(scenarios.BIG_DIFF)
        file = self.file(scenarios.BIG_FILES[0])
        # Scrolling mid-file only sticks the header once the rows are laid
        # out at their real height.
        expect(file.locator("tr.line td.code.add")).to_have_count(300)
        # Mid-file the header sticks to the toolbar's bottom edge.
        file.evaluate("el => el.scrollIntoView({block: 'center'})")
        header = file.locator(".file-header")
        expect(header).to_be_visible()
        self.assertAlmostEqual(
            header.bounding_box()["y"], self.toolbar_bottom(), delta=1
        )

    def test_toolbar_shows_the_title_only_while_pinned(self) -> None:
        pr = self.cases.prs[scenarios.BIG_DIFF]
        self.open(scenarios.BIG_DIFF)
        title = self.page.locator(".toolbar-title")
        # The real h1 covers the top of the page, so no title row.
        expect(title).to_have_count(0)

        file = self.file(scenarios.BIG_FILES[0])
        # Scrolling mid-file only sticks the header once the rows are laid
        # out at their real height.
        expect(file.locator("tr.line td.code.add")).to_have_count(300)
        file.evaluate("el => el.scrollIntoView({block: 'center'})")
        expect(title).to_be_visible()
        expect(title.locator(".state")).to_have_text(
            self.page.locator(".pr-header .state").inner_text()
        )
        expect(title).to_contain_text(pr.title)
        expect(title.locator(".pr-number")).to_have_text(f"#{pr.number}")
        # The row grew the toolbar; the stuck file header follows it.
        header = file.locator(".file-header")
        self.assertAlmostEqual(
            header.bounding_box()["y"], self.toolbar_bottom(), delta=1
        )

        main = self.page.locator(".pr-page .main")
        main.evaluate("el => el.scrollTop = 0")
        expect(title).to_have_count(0)

    def test_pinning_the_toolbar_does_not_jump_the_diff(self) -> None:
        self.open(scenarios.BIG_DIFF)
        main = self.page.locator(".pr-page .main")
        # The scrollTop at which the marker reaches the scroller's top and
        # the toolbar pins.
        pin_top = main.evaluate(
            "el => el.scrollTop"
            " + document.querySelector('.toolbar-marker').getBoundingClientRect().top"
            " - el.getBoundingClientRect().top"
        )
        # Stop just short of pinning; a diff line sits just below the
        # toolbar in normal flow.
        gap = 40
        main.evaluate("(el, y) => el.scrollTop = y", pin_top - gap)
        line = self.file(scenarios.BIG_FILES[0]).locator("tr.line").nth(2)
        y_before = line.bounding_box()["y"]
        # Pinning adds the title row to the in-flow toolbar; scroll
        # anchoring should keep the diff under the cursor steady, so the
        # line moves up by exactly the scrolled amount.
        past = 30
        main.evaluate("(el, y) => el.scrollTop = y", pin_top + past)
        expect(self.page.locator(".toolbar-title")).to_have_count(1)
        self.assertAlmostEqual(line.bounding_box()["y"], y_before - gap - past, delta=6)

    def test_file_index_scrolls_files_below_the_toolbar(self) -> None:
        self.open(scenarios.BIG_DIFF)
        path = scenarios.BIG_FILES[4]
        self.page.locator(".file-index a", has_text=path).click()
        header = self.file(path).locator(".file-header")
        self.assertAlmostEqual(
            header.bounding_box()["y"], self.toolbar_bottom(), delta=1
        )

    def test_collapsing_a_stuck_file_keeps_its_header_in_view(self) -> None:
        self.open(scenarios.BIG_DIFF)
        file = self.file(scenarios.BIG_FILES[2])
        # Scrolling mid-file only sticks the header once the rows are laid
        # out at their real height.
        expect(file.locator("tr.line td.code.add")).to_have_count(300)
        file.evaluate("el => el.scrollIntoView({block: 'center'})")
        header = file.locator(".file-header")
        header.click()
        expect(header).to_be_visible()
        # Collapsing scrolls the file back below the toolbar.
        self.page.wait_for_function(
            "(file) => Math.abs("
            "file.querySelector('.file-header').getBoundingClientRect().top"
            " - document.querySelector('.toolbar').getBoundingClientRect().bottom"
            ") <= 1",
            arg=file.element_handle(),
        )
        self.assertAlmostEqual(
            header.bounding_box()["y"], self.toolbar_bottom(), delta=1
        )

    def test_collapsing_a_stuck_file_lets_the_user_scroll(self) -> None:
        self.open(scenarios.BIG_DIFF)
        file = self.file(scenarios.BIG_FILES[2])
        expect(file.locator("tr.line td.code.add")).to_have_count(300)
        file.evaluate("el => el.scrollIntoView({block: 'center'})")
        file.locator(".file-header").click()
        self.page.wait_for_function(
            "(file) => Math.abs("
            "file.querySelector('.file-header').getBoundingClientRect().top"
            " - document.querySelector('.toolbar').getBoundingClientRect().bottom"
            ") <= 1",
            arg=file.element_handle(),
        )
        # The collapse's scroll-back must not fight a user who scrolls.
        main_scroll_top = "el => el.closest('.main').scrollTop"
        top_before = file.evaluate(main_scroll_top)
        self.page.mouse.wheel(0, 2000)
        self.page.wait_for_timeout(500)
        top = file.evaluate(main_scroll_top)
        self.page.wait_for_timeout(500)
        self.assertEqual(file.evaluate(main_scroll_top), top)
        self.assertGreater(top - top_before, 500)


# A new session's form; each is a draft of its own.
DRAFT_URL = r"#/agents/draft/[0-9a-f]+$"

# Pastes a PNG screenshot into the text box it's evaluated on.
PASTE_IMAGE = """async (box) => {
    const canvas = document.createElement("canvas");
    canvas.width = 2000;
    canvas.height = 1000;
    canvas.getContext("2d").fillRect(0, 0, 10, 10);
    const blob = await new Promise((resolve) => canvas.toBlob(resolve, "image/png"));
    const data = new DataTransfer();
    data.items.add(new File([blob], "screenshot.png", {type: "image/png"}));
    box.dispatchEvent(new ClipboardEvent("paste", {clipboardData: data, bubbles: true, cancelable: true}));
}"""


class AgentTest(ServerTestCase):
    OTHER_REPO = "test/other"
    REPOS: ClassVar[list[str]] = [scenarios.REPO, OTHER_REPO]
    # Sessions' PRs merge and close between polls.
    POLL_INTERVAL: ClassVar[int] = 1

    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        for slug in cls.REPOS:
            repo = gh.repo(slug)
            repo.commit("Seed", {"README": "hello\n"})
            repo.push("main")
        # The PR the agents panel is tested on.
        repo = gh.repo(scenarios.REPO)
        repo.checkout("case/panel", "main")
        repo.commit("Panel", {"panel.md": "# Panel\n"})
        repo.push("case/panel")
        cls.panel_pr = repo.open_pr("Panel", head="case/panel")

    def setUp(self) -> None:
        super().setUp()
        # Tests share the class's server, and which tests share one
        # changes with the shard layout, so each test leaves it as it
        # found it: sessions started by the test are deleted and
        # repository settings restored.
        with urllib.request.urlopen(f"{self.url}api/agents") as resp:
            sessions = {a["id"] for a in json.load(resp)}
        settings = {}
        for slug in self.REPOS:
            with urllib.request.urlopen(f"{self.url}api/repos/{slug}/settings") as resp:
                settings[slug] = resp.read()

        def restore() -> None:
            origin = {"Origin": self.url.rstrip("/")}
            with urllib.request.urlopen(f"{self.url}api/agents") as resp:
                for a in json.load(resp):
                    if a["id"] in sessions:
                        continue
                    req = urllib.request.Request(
                        f"{self.url}api/agents/{a['id']}",
                        method="DELETE",
                        headers=origin,
                    )
                    try:
                        urllib.request.urlopen(req).close()
                    except urllib.error.HTTPError as e:
                        if e.code != 404:
                            raise
            for slug, saved in settings.items():
                with urllib.request.urlopen(
                    f"{self.url}api/repos/{slug}/settings"
                ) as resp:
                    if resp.read() == saved:
                        continue
                req = urllib.request.Request(
                    f"{self.url}api/repos/{slug}/settings",
                    data=saved,
                    method="PUT",
                    headers={**origin, "Content-Type": "application/json"},
                )
                urllib.request.urlopen(req).close()

        self.addCleanup(restore)

    def section(self, repo: str) -> Locator:
        return self.page.locator(".agent-repo").filter(
            has=self.page.locator(".agent-repo-name span", has_text=exactly(repo))
        )

    def test_sidebar_has_a_section_per_repo(self) -> None:
        # The open form's draft is listed in its repository's section.
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        # Sessions in no repository come last.
        expect(self.page.locator(".agent-repo-name span")).to_have_text(
            [*sorted([scenarios.REPO, self.OTHER_REPO]), "General"]
        )

        # A repository without sessions says so and links to the form.
        other = self.section(self.OTHER_REPO)
        expect(other.locator(".agent-none")).to_contain_text(
            "No agents in this repository yet."
        )
        other.get_by_role("link", name="Start one").click()
        expect(self.page).to_have_url(re.compile(DRAFT_URL))
        expect(self.page.get_by_label("Repository")).to_have_value(self.OTHER_REPO)

    def test_repo_settings_dialog(self) -> None:
        self.page.goto(self.url + "#/agents")
        # Sessions in no repository have no settings.
        expect(
            self.section("General").get_by_role("button", name=re.compile("Settings"))
        ).to_have_count(0)
        gear = self.section(self.OTHER_REPO).get_by_role(
            "button", name=f"Settings for {self.OTHER_REPO}"
        )
        dialog = self.page.get_by_role("dialog", name=f"{self.OTHER_REPO} settings")

        gear.click()
        expect(dialog).to_be_visible()
        expect(dialog.get_by_role("group", name="Auto-reviews")).to_be_visible()
        expect(dialog.get_by_role("group", name="New sessions")).to_be_visible()
        # It stays open while polls redraw the sidebar.
        self.page.wait_for_timeout(2500)
        expect(dialog).to_be_visible()
        # Nothing changed, so there's nothing to save.
        expect(dialog.get_by_role("button", name="Save")).to_be_disabled()
        self.page.keyboard.press("Escape")
        expect(dialog).to_have_count(0)

        gear.click()
        dialog.get_by_role("button", name="Close").click()
        expect(dialog).to_have_count(0)

        gear.click()
        dialog.get_by_role("button", name="Cancel").click()
        expect(dialog).to_have_count(0)

    def test_repo_session_defaults(self) -> None:
        # The repository's settings pick the New session form's defaults.
        self.page.goto(self.url + "#/agents")
        gear = self.section(self.OTHER_REPO).get_by_role(
            "button", name=f"Settings for {self.OTHER_REPO}"
        )
        dialog = self.page.get_by_role("dialog", name=f"{self.OTHER_REPO} settings")
        gear.click()
        dialog.get_by_label("Claude permissions").select_option(label="Accept edits")
        dialog.get_by_label("Devin permissions").select_option(label="Smart")
        dialog.get_by_label("Default skill").fill("/foo")
        dialog.get_by_role("button", name="Save").click()
        expect(dialog).to_have_count(0)

        # Reopened, they persisted.
        gear.click()
        expect(dialog.get_by_label("Default skill")).to_have_value("foo")
        expect(dialog.get_by_label("Claude permissions")).to_have_value("acceptEdits")
        expect(dialog.get_by_label("Devin permissions")).to_have_value("smart")
        self.page.keyboard.press("Escape")
        expect(dialog).to_have_count(0)

        # The form starts with them and follows repository and backend
        # switches until a mode is picked by hand. The dialog's "Claude
        # permissions"/"Devin permissions" labels contain "Permissions",
        # so the form's is matched exactly.
        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        expect(self.page.locator("dialog")).to_have_count(0)
        permissions = self.page.get_by_label("Permissions", exact=True)
        expect(permissions).to_have_value("acceptEdits")
        expect(self.page.locator(".agent-form")).to_contain_text("/foo")
        self.page.get_by_label("Backend").select_option(label="Devin")
        expect(permissions).to_have_value("smart")
        self.page.get_by_label("Repository").select_option(label="General")
        expect(permissions).to_have_value("plan")
        self.page.get_by_label("Repository").select_option(self.OTHER_REPO)
        expect(permissions).to_have_value("smart")
        permissions.select_option(label="Ask")
        self.page.get_by_label("Repository").select_option(label="General")
        self.page.get_by_label("Repository").select_option(self.OTHER_REPO)
        expect(permissions).to_have_value("ask")

        # A prompt runs the repository's skill, which Devin sees.
        permissions.select_option(label="Code")
        self.page.get_by_label("Prompt").fill("bar")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        transcript = self.page.locator(".transcript")
        expect(transcript.locator(".turn-prompt").first).to_have_text("/foo bar")
        self.devin_saw("/foo bar")
        transcript.locator(".approval").get_by_role("button", name="Allow").click()
        expect(transcript.locator(".turn-end")).to_have_count(1)

        # A prompt with a skill of its own doesn't.
        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions", exact=True).select_option(label="Code")
        self.page.get_by_label("Prompt").fill("/baz bar")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        expect(transcript.locator(".turn-prompt").first).to_have_text("/baz bar")
        self.devin_saw("/baz bar")
        transcript.locator(".approval").get_by_role("button", name="Allow").click()
        expect(transcript.locator(".turn-end")).to_have_count(1)

        sessions = self.section(self.OTHER_REPO).locator(".agent-entry")
        expect(sessions).to_have_count(2)
        for left in [1, 0]:
            self.page.once("dialog", lambda dialog: dialog.accept())
            sessions.first.get_by_role("button", name="Delete session").click()
            expect(sessions).to_have_count(left)

        # The settings go back to their defaults, "Plan first" and no
        # skill, for the other tests.
        gear.click()
        dialog.get_by_label("Claude permissions").select_option(label="Plan first")
        dialog.get_by_label("Devin permissions").select_option(label="Plan first")
        dialog.get_by_label("Default skill").fill("")
        dialog.get_by_role("button", name="Save").click()
        expect(dialog).to_have_count(0)

    def start_devin_session(self, permissions: str, prompt: str) -> Locator:
        self.page.goto(self.url + "#/agents")
        self.page.get_by_label("Repository").select_option(label="General")
        self.page.get_by_label("Backend").select_option(label="Devin")
        self.page.get_by_label("Permissions").select_option(label=permissions)
        self.page.get_by_label("Prompt").fill(prompt)
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        return self.page.locator(".transcript")

    def delete_session(self) -> None:
        # The summary can relabel the session before the delete lands, so
        # the wait is for its link to be gone, not for its title.
        id = self.page.url.split("/")[-1]
        self.page.once("dialog", lambda dialog: dialog.accept())
        self.page.get_by_role("button", name="Delete", exact=True).click()
        expect(self.page.locator(f'.agent-item[href="#/agents/{id}"]')).to_have_count(0)

    def test_backends_keep_their_own_models(self) -> None:
        self.page.goto(self.url + "#/agents")
        backend = self.page.get_by_label("Backend")
        model = self.page.get_by_label("Model")
        model.select_option("sonnet")
        backend.select_option(label="Devin")
        # Devin doesn't offer Claude's pick, and resetting it doesn't
        # count as a pick for Devin.
        expect(model).to_have_value("default")
        backend.select_option(label="Claude Code")
        expect(model).to_have_value("sonnet")
        backend.select_option(label="Devin")
        model.select_option("swe-2-high")
        backend.select_option(label="Claude Code")
        expect(model).to_have_value("sonnet")
        backend.select_option(label="Devin")
        expect(model).to_have_value("swe-2-high")

    def test_a_draft_leaves_a_backend_no_longer_offered(self) -> None:
        # The last session was Devin's, but Devin can't list its models
        # any more, so the server offers Claude Code alone.
        self.page.add_init_script(
            "localStorage.setItem('grenadine.agent-backend', 'devin')"
        )
        self.page.route(
            "**/api/agents/backends",
            lambda route: route.fulfill(json=[{"backend": "claude", "models": []}]),
        )
        self.page.goto(self.url + "#/agents")
        expect(self.page.get_by_label("Prompt")).to_have_attribute(
            "placeholder", re.compile(r"^What should Claude do\?")
        )
        # With nothing else to pick, there's no picker.
        expect(self.page.get_by_label("Backend")).to_have_count(0)
        # The draft keeps its pick, so once Devin is back, so is the draft's
        # backend.
        self.page.get_by_label("Prompt").fill("later")
        self.page.unroute("**/api/agents/backends")
        self.page.add_init_script(
            "localStorage.setItem('grenadine.agent-backend', 'claude')"
        )
        self.page.reload()
        expect(self.page.get_by_label("Prompt")).to_have_value("later")
        expect(self.page.get_by_label("Backend")).to_have_value("devin")

    def test_a_fallback_backend_isnt_saved(self) -> None:
        # The stored backend is Devin's, but the server offers Claude
        # Code alone, so the form goes with it. Starting that session
        # doesn't make the fallback the stored backend.
        self.page.add_init_script(
            "localStorage.setItem('grenadine.agent-backend', 'devin')"
        )
        self.page.route(
            "**/api/agents/backends",
            lambda route: route.fulfill(json=[{"backend": "claude", "models": []}]),
        )
        self.page.goto(self.url + "#/agents")
        self.page.get_by_label("Prompt").fill("hi")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        backend = self.page.evaluate(
            "() => localStorage.getItem('grenadine.agent-backend')"
        )
        self.assertEqual(backend, "devin")
        self.delete_session()

        # Picking a backend by hand does store it.
        self.page.unroute("**/api/agents/backends")
        self.page.goto(self.url + "#/agents")
        self.page.get_by_label("Backend").select_option(label="Devin")
        backend = self.page.evaluate(
            "() => localStorage.getItem('grenadine.agent-backend')"
        )
        self.assertEqual(backend, "devin")
        self.page.get_by_label("Backend").select_option(label="Claude Code")
        backend = self.page.evaluate(
            "() => localStorage.getItem('grenadine.agent-backend')"
        )
        self.assertEqual(backend, "claude")

    def test_a_backend_listing_error_is_shown(self) -> None:
        # A backend whose models can't be listed says why, with the whole
        # error in the hint's title.
        self.page.route(
            "**/api/agents/backends",
            lambda route: route.fulfill(
                json=[
                    {
                        "backend": "claude",
                        "models": [],
                        "error": "claude exited 1\nout of credit",
                    },
                    {"backend": "devin", "models": []},
                ]
            ),
        )
        self.page.goto(self.url + "#/agents")
        hint = self.page.locator(
            ".agent-form .hint", has_text="couldn't list its models"
        )
        expect(hint).to_have_text("Claude couldn't list its models: claude exited 1")
        expect(hint).to_have_attribute("title", "claude exited 1\nout of credit")

    def test_no_backend_is_an_error(self) -> None:
        # With no backend installed on the server, the form says so and
        # a session can't be started.
        self.page.route(
            "**/api/agents/backends",
            lambda route: route.fulfill(json=[]),
        )
        self.page.goto(self.url + "#/agents")
        expect(self.page.locator(".agent-form .error")).to_have_text(
            "No agent backend is installed on the server."
        )
        self.page.get_by_label("Prompt").fill("hi")
        expect(self.page.get_by_role("button", name="Start")).to_be_disabled()

    def test_devin_session(self) -> None:
        transcript = self.start_devin_session("Code", "write a note")
        expect(transcript.locator(".turn-text")).to_contain_text("On it.")
        approval = transcript.locator(".approval")
        expect(approval).to_contain_text("Devin wants to use Bash")
        # The call shows its command highlighted, not the raw JSON.
        tool_input = approval.locator(".tool-input")
        expect(tool_input).to_contain_text("touch note.txt")
        expect(tool_input.locator('span[class*="sy-"]').first).to_be_visible()
        expect(tool_input).not_to_contain_text('"command":')
        # The toggle swaps it for the raw JSON and back.
        toggle = approval.get_by_role("button", name="JSON")
        toggle.click()
        expect(tool_input).to_contain_text('"command":')
        toggle.click()
        expect(tool_input).not_to_contain_text('"command":')
        # Devin's options render as a button each.
        approval.get_by_role("button", name="Allow").click()
        # The answered card is gone; its mark is on the call's row.
        expect(approval).to_have_count(0)
        tool = transcript.locator("details.tool")
        expect(tool.locator(".approval-mark")).to_have_attribute(
            "aria-label", "Allowed"
        )
        expect(tool.locator(".tool-status")).to_have_attribute(
            "aria-label", "Succeeded"
        )
        expect(transcript.locator(".turn-end")).to_have_text(["Done · 0.500 ACU"])
        expect(self.page.locator(".composer .agent-model")).to_have_text("swe-2")
        # The subagent's exec is recorded but its tool call doesn't render.
        expect(transcript).to_contain_text("touch note.txt")
        expect(transcript).not_to_contain_text("subagent-marker")
        # Devin wrote the summary that labels the session.
        sessions = self.section("General").locator(".agent-entry")
        expect(sessions.filter(has_text="A Devin task")).to_have_count(1)
        self.delete_session()

    def test_devin_thinking_toggle(self) -> None:
        transcript = self.start_devin_session("Code", "ponder it")
        first_id = self.page.url.split("/")[-1]
        expect(transcript.locator(".turn-text")).to_contain_text("Pondered.")
        # Thinking stays hidden until the session's toggle says so.
        expect(transcript.locator(".turn-thinking")).to_have_count(0)
        expect(transcript).not_to_contain_text("Pondering the answer")
        self.page.get_by_label("Show thinking").check()
        expect(transcript.locator(".turn-thinking")).to_contain_text(
            "Pondering the answer"
        )
        # The pick is the session's own: it survives a reload, and a
        # different session still starts with thinking hidden.
        self.page.reload()
        transcript = self.page.locator(".transcript")
        expect(transcript.locator(".turn-thinking")).to_contain_text(
            "Pondering the answer"
        )
        expect(self.page.get_by_label("Show thinking")).to_be_checked()
        transcript = self.start_devin_session("Code", "ponder again")
        expect(transcript.locator(".turn-text")).to_contain_text("Pondered.")
        expect(transcript.locator(".turn-thinking")).to_have_count(0)
        self.delete_session()
        self.delete_session_via_api(first_id)

    def test_composer_wraps_long_model_names(self) -> None:
        # Long model names make the composer's button row wrap onto a
        # second line rather than overflow, hiding the context meter.
        def add_long_model(route) -> None:
            response = route.fetch()
            data = response.json()
            for entry in data:
                if entry["backend"] == "devin":
                    entry["models"].append(
                        {
                            "value": "fusion-very-long-model-id-for-testing-wrapping",
                            "displayName": "Fusion (An Extremely Long Model Display Name Medium + Another Long Sidekick Name Medium)",
                            "description": "",
                        }
                    )
            route.fulfill(response=response, json=data)

        self.page.route("**/api/agents/backends", add_long_model)
        self.page.set_viewport_size({"width": 800, "height": 900})
        transcript = self.start_devin_session("Code", "write a note")
        transcript.locator(".approval").get_by_role("button", name="Allow").click()
        expect(transcript.locator(".turn-end")).to_have_count(1)

        # The panes never scroll horizontally.
        overflow = self.page.locator(".agent-panes > .main").evaluate(
            "e => e.scrollWidth - e.clientWidth"
        )
        self.assertEqual(overflow, 0)

        meter = self.page.get_by_role("meter", name="Context")
        expect(meter).to_be_in_viewport()
        meter_box = meter.bounding_box()
        buttons_box = self.page.locator(".composer .form-buttons").bounding_box()
        assert meter_box and buttons_box
        # The status sits at the row's right edge even after wrapping.
        self.assertAlmostEqual(
            meter_box["x"] + meter_box["width"],
            buttons_box["x"] + buttons_box["width"],
            delta=1,
        )
        # The row wrapped: the meter is on a line below Send.
        send_box = self.page.get_by_role("button", name="Send").bounding_box()
        assert send_box
        self.assertGreater(meter_box["y"], send_box["y"] + send_box["height"])
        self.delete_session()

    def test_agent_footer_flush_with_bottom_when_scrolled_down(self) -> None:
        # The sticky footer ends the transcript; scrolled to the bottom
        # it sits flush with the pane's edge, with no padding below it.
        self.page.set_viewport_size({"width": 1000, "height": 400})
        self.page.goto(self.url + "#/agents")
        self.page.get_by_label("Repository").select_option(scenarios.REPO)
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("long transcript 10")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        expect(self.page.locator(".transcript .turn-end")).to_have_count(
            1, timeout=60_000
        )

        main = self.page.locator(".agent-panes > .main")
        self.assertGreater(
            main.evaluate("e => e.scrollHeight"),
            main.evaluate("e => e.clientHeight"),
        )
        main.evaluate("e => e.scrollTop = e.scrollHeight")
        footer_box = self.page.locator(".agent-footer").bounding_box()
        main_box = main.bounding_box()
        assert footer_box and main_box
        self.assertAlmostEqual(
            footer_box["y"] + footer_box["height"],
            main_box["y"] + main_box["height"],
            delta=1,
        )
        self.delete_session()

    def test_transcript_keeps_bottom_when_terminal_opens(self) -> None:
        # The terminal panel takes its room from the transcript's pane,
        # which keeps its distance from the bottom rather than the top.
        self.page.set_viewport_size({"width": 1000, "height": 800})
        self.page.goto(self.url + "#/agents")
        self.page.get_by_label("Repository").select_option(scenarios.REPO)
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("long transcript 30")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        expect(self.page.locator(".transcript .turn-end")).to_have_count(
            1, timeout=60_000
        )
        main = self.page.locator(".agent-panes > .main")
        toggle = self.page.get_by_role("button", name="Terminal", exact=True)

        def below() -> int:
            return main.evaluate("e => e.scrollHeight - e.scrollTop - e.clientHeight")

        def scroll_to(distance: int) -> None:
            main.evaluate(
                "e => e.scrollTop = e.scrollHeight - e.clientHeight - " + str(distance)
            )
            # The page hears of the scroll on the next frame.
            self.page.evaluate(
                "() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))"
            )
            self.assertAlmostEqual(below(), distance, delta=1)

        def toggle_to(expanded: str) -> None:
            height = main.evaluate("e => e.clientHeight")
            toggle.click()
            expect(toggle).to_have_attribute("aria-expanded", expanded)
            expect(self.page.locator(".terminal-body")).to_be_visible(
                visible=expanded == "true"
            )
            self.page.wait_for_function(
                "([e, h]) => e.clientHeight !== h", arg=[main.element_handle(), height]
            )
            # The pane restores its distance from the bottom in a resize
            # observer, which runs after the height changed.
            self.page.evaluate(
                "() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))"
            )

        # Scrolled to the bottom, opening the first terminal keeps the
        # bottom of the transcript in view.
        scroll_to(0)
        toggle_to("true")
        expect(self.page.get_by_role("tab")).to_have_text(["Terminal 1"])
        self.assertAlmostEqual(below(), 0, delta=1)

        # Scrolled up, the pane keeps its distance from the bottom as the
        # panel collapses and opens again.
        scroll_to(200)
        toggle_to("false")
        self.assertAlmostEqual(below(), 200, delta=1)
        toggle_to("true")
        self.assertAlmostEqual(below(), 200, delta=1)
        self.delete_session()

    def test_devin_question(self) -> None:
        transcript = self.start_devin_session("Code", "ask me a question")
        question = transcript.locator(".approval")
        expect(question).to_contain_text("Devin asks")
        question.get_by_text("Tea", exact=True).click()
        question.get_by_role("button", name="Answer").click()
        # The answer goes back to Devin as the elicitation's q0.
        self.devin_saw('"action":"accept"')
        self.devin_saw('"q0":"Tea"')
        expect(transcript.locator(".turn-end")).to_have_count(1)
        self.delete_session()

    def test_devin_question_answers_on_enter(self) -> None:
        transcript = self.start_devin_session("Code", "ask me a question by enter")
        question = transcript.locator(".approval")
        expect(question).to_contain_text("Devin asks")
        # Enter in the empty text field submits nothing: the answer is
        # incomplete and the submit button stays disabled.
        other = question.get_by_label("Something else")
        other.focus()
        self.page.keyboard.press("Enter")
        expect(question).to_contain_text("Devin asks")
        other.fill("Water")
        self.page.keyboard.press("Enter")
        # The answer goes back to Devin as the elicitation's q0.
        self.devin_saw('"q0":"Water"')
        expect(question).to_contain_text("Answered")
        expect(question.locator(".question-answer")).to_have_text("Water")
        expect(transcript.locator(".turn-end")).to_have_count(1)
        self.delete_session()

    def test_devin_multi_select(self) -> None:
        transcript = self.start_devin_session("Code", "ask me several drinks")
        question = transcript.locator(".approval")
        expect(question).to_contain_text("Devin asks")
        checkboxes = question.get_by_role("checkbox")
        expect(checkboxes).to_have_count(2)
        checkboxes.nth(0).check()
        checkboxes.nth(1).check()
        question.get_by_role("button", name="Answer").click()
        # A multi-select answer goes back to Devin as a JSON array.
        self.devin_saw('"action":"accept"')
        self.devin_saw('"q0":["Tea","Coffee"]')
        expect(question).to_contain_text("Answered")
        expect(question.locator(".question-answer")).to_have_text("Tea, Coffee")
        expect(transcript.locator(".turn-end")).to_have_count(1)
        self.delete_session()

    def test_devin_plan(self) -> None:
        transcript = self.start_devin_session("Plan first", "make a plan")
        plan = transcript.locator(".approval.plan")
        # The card shows the plan file's text, without its front matter.
        expect(plan).to_contain_text("Devin's plan is waiting for your approval")
        expect(plan.locator(".plan-text")).to_contain_text("Do the thing.")
        expect(plan.locator(".plan-text")).not_to_contain_text("agent: devin")
        # Devin's after-plan modes are Code, Smart and Bypass permissions.
        expect(plan.get_by_label("After the plan")).to_have_value("smart")
        # Sent back with feedback, the plan's denial goes to Devin as
        # reject_once and the feedback runs as the next prompt.
        plan.get_by_label("Feedback").fill("more detail")
        plan.get_by_role("button", name="Keep planning").click()
        self.devin_saw('"optionId":"reject_once"')
        expect(transcript.locator(".turn-prompt").last).to_have_text("more detail")
        self.devin_saw("more detail")

        # The next turn plans again; approving it picks the option that
        # names the mode the turn goes on in.
        revised = transcript.locator(".approval.plan").last
        expect(revised).to_contain_text("Devin's plan is waiting for your approval")
        revised.get_by_role("button", name="Approve plan").click()
        self.devin_saw('"optionId":"plan_accept_edits"')
        # Devin offers no plan_smart, so a set_mode to smart follows the
        # approval's plan_accept_edits.
        self.devin_saw('"method":"session/set_mode"')
        self.devin_saw('"modeId":"smart"')
        expect(revised).to_contain_text("Plan approved")
        expect(transcript.locator(".turn-end")).to_have_count(2)
        # The second turn resumed the session, whose replayed answer the
        # transcript already has.
        self.devin_saw('"method":"session/load"')
        expect(transcript).not_to_contain_text("Replayed.")
        self.delete_session()

    def test_general_session(self) -> None:
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.page.goto(self.url + "#/agents")
        repo = self.page.get_by_label("Repository")
        expect(repo.locator("option")).to_have_text(
            [*sorted([scenarios.REPO, self.OTHER_REPO]), "General"]
        )
        # New sessions start in no repository unless one is picked.
        expect(repo).to_have_value("general")
        # There are no PRs to start from.
        expect(self.page.get_by_label("Start from PR")).to_be_disabled()
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("write a note")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        expect(self.page.locator(".transcript .turn-end")).to_have_count(1)
        expect(self.page.locator(".agent-header")).to_contain_text("in no repository")

        # It works in a plain directory of its own, with nothing to land.
        directory = Path(self.page.locator(".agent-worktree").inner_text())
        self.assertTrue((directory / "notes.txt").exists())
        self.assertFalse((directory / ".git").exists())
        expect(self.page.get_by_role("button", name="Open PR")).to_have_count(0)
        expect(
            self.page.get_by_role("button", name="Merge into checkout")
        ).to_have_count(0)
        expect(self.page.locator(".changes-pane")).to_have_count(0)
        # Deleting it opens a new form, whose draft is listed there too.
        sessions = (
            self.section("General")
            .locator(".agent-entry")
            .filter(has_not=self.page.locator(".agent-draft"))
        )
        expect(sessions).to_have_count(1)

        # Deleting it takes the directory with it.
        self.page.once("dialog", lambda dialog: dialog.accept())
        self.page.get_by_role("button", name="Delete", exact=True).click()
        expect(sessions).to_have_count(0)
        self.assertFalse(directory.exists())

    def test_agent_session(self) -> None:
        self.page.goto(self.url)
        self.page.get_by_role("link", name="Agents").click()
        self.page.get_by_label("Repository").select_option(scenarios.REPO)
        # New sessions start out planning unless another mode is picked.
        permissions = self.page.get_by_label("Permissions")
        expect(permissions).to_have_value("plan")
        permissions.select_option(label="Ask for everything")
        # The models are the ones Claude Code lists; its default comes first.
        model = self.page.get_by_label("Model")
        expect(model).to_have_value("default")
        expect(model.locator("option")).to_have_text(
            ["Default (recommended)", "Sonnet", "Haiku"]
        )
        self.page.get_by_label("Prompt").fill("please write a note")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))

        transcript = self.page.locator(".transcript")
        expect(transcript.locator(".turn-prompt")).to_have_text(["please write a note"])
        expect(transcript.locator(".turn-text strong")).to_have_text(
            "please write a note"
        )
        approval = transcript.locator(".approval")
        expect(approval).to_contain_text("Claude wants to use Write notes.txt")
        expect(approval.locator(".edit-diff tr.add .code")).to_have_text(
            "please write a note"
        )
        approval.get_by_role("button", name="Allow").click()
        # The answered card is gone; its mark is on the call's row.
        expect(approval).to_have_count(0)
        expect(transcript.locator(".turn-end")).to_have_text(["Done · $0.01"])

        # The context meter sits at the right of the composer's buttons,
        # after the model, with Send on the left.
        meter = self.page.get_by_role("meter", name="Context")
        expect(meter.locator(".context-text")).to_have_text("41.5k / 200k")
        expect(meter.locator(".context-fill")).to_have_class(re.compile(r"\bgreen\b"))
        buttons = self.page.locator(".composer .form-buttons").bounding_box()
        send = self.page.get_by_role("button", name="Send").bounding_box()
        box = meter.bounding_box()
        assert buttons and send and box
        self.assertAlmostEqual(
            box["x"] + box["width"], buttons["x"] + buttons["width"], delta=1
        )
        active = self.page.locator(".composer .agent-model")
        expect(active).to_have_text("claude-test")
        active_box = active.bounding_box()
        assert active_box
        self.assertLess(send["x"] + send["width"], active_box["x"])
        self.assertLess(active_box["x"] + active_box["width"], box["x"])
        tool = transcript.locator(".tool")
        # The row carries the allowed mark and the succeeded dot.
        expect(tool.locator(".approval-mark")).to_have_attribute(
            "aria-label", "Allowed"
        )
        expect(tool.locator(".tool-status")).to_have_attribute(
            "aria-label", "Succeeded"
        )
        tool.locator("summary").click()
        # The write shows as a diff, without the result that only repeats it.
        added = tool.locator(".edit-diff tr.add")
        expect(added.locator(".ln")).to_have_text(["", "1"])
        expect(added.locator(".code")).to_have_text("please write a note")
        expect(tool.locator(".tool-result")).to_have_count(0)

        # The agent worked in the session's worktree, which starts at main.
        worktree = Path(self.page.locator(".agent-worktree").inner_text())
        self.assertEqual((worktree / "notes.txt").read_text(), "please write a note")
        self.assertEqual((worktree / "README").read_text(), "hello\n")

        follow_up = self.page.get_by_label("Follow-up")
        # A follow-up can switch models, which the composer then shows.
        model.select_option(label="Sonnet")
        # Shift+Enter starts a new line and Enter sends.
        follow_up.fill("thanks")
        follow_up.press("Shift+Enter")
        follow_up.press_sequentially("so much")
        expect(follow_up).to_have_value("thanks\nso much")
        expect(transcript.locator(".turn-prompt")).to_have_count(1)
        follow_up.press("Enter")
        expect(transcript.locator(".turn-prompt")).to_have_text(
            ["please write a note", "thanks\nso much"]
        )
        expect(transcript.locator(".turn-end")).to_have_count(2)
        expect(follow_up).to_have_value("")
        expect(active).to_have_text("sonnet")

        # The composer starts with the permissions picked last. In Auto
        # mode, the write goes ahead without asking.
        expect(permissions).to_have_value("manual")
        permissions.select_option(label="Auto")
        follow_up.fill("write more")
        follow_up.press("Enter")
        expect(transcript.locator(".turn-end")).to_have_count(3)
        expect(transcript.locator(".approval")).to_have_count(0)
        self.assertEqual((worktree / "notes.txt").read_text(), "write more")

        # The transcript is stored, so a reload shows it all again.
        self.page.reload()
        expect(transcript.locator(".turn-prompt")).to_have_text(
            ["please write a note", "thanks\nso much", "write more"]
        )
        expect(permissions).to_have_value("auto")
        expect(model).to_have_value("sonnet")
        expect(transcript.locator(".approval")).to_have_count(0)
        expect(transcript.locator(".approval-mark")).to_have_attribute(
            "aria-label", "Allowed"
        )
        expect(self.page.locator(".agent-header")).to_contain_text("idle")
        expect(self.page.get_by_role("meter", name="Context")).to_contain_text(
            "41.5k / 200k"
        )
        expect(active).to_have_text("sonnet")

        # The sidebar lists the session under its repository and highlights it.
        item = self.section(scenarios.REPO).locator(".agent-item")
        expect(item).to_contain_text(["please write a note"])
        expect(item).to_have_class(re.compile(r"\bselected\b"))
        # It's labelled with Haiku's summary of the first prompt, with the
        # status under it and a button that deletes it on the right.
        entry = self.section(scenarios.REPO).locator(".agent-entry")
        title = entry.locator(".agent-title")
        expect(title).to_have_text("Task: please write a note")
        expect(title).to_have_attribute("title", "please write a note")
        status = entry.locator(".agent-status")
        expect(status).to_have_text("idle")
        # The branch is on the session's page, not in the sidebar.
        branch = self.page.locator(".agent-header p.muted code").last
        expect(branch).not_to_have_text("")
        expect(entry.locator(".agent-meta")).not_to_contain_text(branch.inner_text())
        trash = entry.get_by_role("button", name="Delete session")
        title_box = title.bounding_box()
        status_box = status.bounding_box()
        trash_box = trash.bounding_box()
        entry_box = entry.bounding_box()
        assert title_box and status_box and trash_box and entry_box
        self.assertGreaterEqual(status_box["y"], title_box["y"] + title_box["height"])
        self.assertAlmostEqual(status_box["x"], title_box["x"], delta=1)
        self.assertGreaterEqual(trash_box["y"], title_box["y"] + title_box["height"])
        self.assertAlmostEqual(
            trash_box["x"] + trash_box["width"],
            entry_box["x"] + entry_box["width"],
            delta=16,
        )
        self.page.get_by_role("link", name="Agents").click()
        expect(item).not_to_have_class(re.compile(r"\bselected\b"))
        # New sessions start with the model picked last.
        expect(model).to_have_value("sonnet")
        item.click()
        expect(transcript.locator(".turn-prompt")).to_have_text(
            ["please write a note", "thanks\nso much", "write more"]
        )

        # Opening a session that doesn't fit shows the end of its transcript.
        self.page.set_viewport_size({"width": 1000, "height": 320})
        self.page.get_by_role("link", name="Agents").click()
        item.click()
        expect(transcript.locator(".turn-end").last).to_be_in_viewport()
        expect(transcript.locator(".turn-prompt").first).not_to_be_in_viewport()

    def test_denied_approval_marks_the_row(self) -> None:
        # A denied request leaves no card behind: its mark and the
        # failure's dot are on the call's row.
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Ask for everything")
        self.page.get_by_label("Prompt").fill("please write a note")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))

        transcript = self.page.locator(".transcript")
        approval = transcript.locator(".approval")
        expect(approval).to_contain_text("Claude wants to use Write notes.txt")
        # While the request waits, the call's row spins, the spinner
        # first in the summary.
        tool = transcript.locator("details.tool")
        spinner = tool.locator("summary .spinner")
        expect(spinner).to_be_visible()
        spinner_box = spinner.bounding_box()
        name_box = tool.locator(".tool-name").bounding_box()
        assert spinner_box and name_box
        self.assertLess(spinner_box["x"], name_box["x"])

        approval.get_by_role("button", name="Deny").click()
        expect(approval).to_have_count(0)
        expect(tool.locator(".approval-mark")).to_have_attribute("aria-label", "Denied")
        expect(tool.locator(".tool-status")).to_have_attribute("aria-label", "Failed")
        expect(tool.locator("summary")).not_to_contain_text("failed")
        # The dot takes the spinner's space, so the name doesn't move.
        finished_name_box = tool.locator(".tool-name").bounding_box()
        assert finished_name_box
        self.assertAlmostEqual(finished_name_box["x"], name_box["x"], delta=0.5)
        self.delete_session()

    def test_a_request_nobody_waits_for_expires(self) -> None:
        # Once Claude Code hangs up on a request, the request can't be
        # answered anymore, so its row says it expired and nothing offers
        # to answer it.
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Ask for everything")
        self.page.get_by_label("Prompt").fill("hang up")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))

        transcript = self.page.locator(".transcript")
        approval = transcript.locator(".approval")
        expect(approval).to_contain_text("Claude wants to use Bash")
        tool = transcript.locator("details.tool")
        expect(tool.locator(".approval-mark")).to_have_attribute(
            "aria-label", "Expired"
        )
        expect(approval).to_have_count(0)
        expect(transcript.locator(".turn-end")).to_have_count(1)
        self.delete_session()

    def open_pr(self, title: str) -> Any:
        repo = self.gh.repo(scenarios.REPO)
        branch = re.sub(r"\W+", "-", title).lower()
        repo.checkout(branch, "main")
        repo.commit(title, {f"{branch}.txt": title})
        repo.push(branch)
        return repo.open_pr(title, head=branch)

    def delete_session_via_api(self, id: str) -> None:
        req = urllib.request.Request(
            f"{self.url}api/agents/{id}",
            method="DELETE",
            headers={"Origin": self.url.rstrip("/")},
        )
        urllib.request.urlopen(req).close()

    def test_tabs_remember_their_last_page(self) -> None:
        pr_a = self.open_pr("Tab memory A")
        pr_b = self.open_pr("Tab memory B")
        url_a = f"#/{scenarios.REPO}/{pr_a.number}$"
        url_b = f"#/{scenarios.REPO}/{pr_b.number}$"

        def tab(name: str) -> Locator:
            return self.page.locator(".topbar-tabs a").filter(has_text=name)

        # On PR A, Agents has nothing remembered and opens a draft.
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr_a.number}")
        expect(self.page.locator(".pr-header h1")).to_have_text(
            f"{pr_a.title} #{pr_a.number}"
        )
        tab("Agents").click()
        expect(self.page).to_have_url(re.compile(DRAFT_URL))

        # Starting a session makes it the tab's page.
        self.page.get_by_label("Repository").select_option(scenarios.REPO)
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("tab memory")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        session_url = self.page.url
        session_id = session_url.split("/")[-1]

        # Inboxes goes back to PR A; visiting PR B moves the memory.
        tab("Inboxes").click()
        expect(self.page).to_have_url(re.compile(url_a))
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr_b.number}")
        tab("Agents").click()
        expect(self.page).to_have_url(session_url)
        tab("Inboxes").click()
        expect(self.page).to_have_url(re.compile(url_b))

        # The active tab goes to its root instead.
        tab("Inboxes").click()
        expect(self.page).to_have_url(re.compile(r"#/$"))
        tab("Agents").click()
        expect(self.page).to_have_url(session_url)
        tab("Agents").click()
        expect(self.page).to_have_url(re.compile(DRAFT_URL))

        # Back on the session through its sidebar link, a reload keeps
        # the memory: Agents still goes to it.
        self.page.locator(f'a.agent-item[href="#/agents/{session_id}"]').click()
        expect(self.page).to_have_url(session_url)
        # The hashchange for the click lands before the next navigation.
        self.page.wait_for_function(
            f"sessionStorage.getItem('grenadine.tabs').includes('{session_id}')"
        )
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr_b.number}")
        self.page.reload()
        tab("Agents").click()
        expect(self.page).to_have_url(session_url)

        # With the session gone, Agents opens a new draft.
        tab("Inboxes").click()
        self.delete_session_via_api(session_id)
        expect(tab("Agents")).to_have_attribute("href", "#/agents")
        tab("Agents").click()
        expect(self.page).to_have_url(re.compile(DRAFT_URL))

    def test_long_transcript(self) -> None:
        # A session whose transcript has hundreds of tool calls renders
        # them both as they stream in and replayed on a reload. Each tool
        # call opens to its own result, and the view follows the output.
        count = 300
        self.page.goto(self.url + "#/agents")
        self.page.get_by_label("Repository").select_option(scenarios.REPO)
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill(f"long transcript {count}")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))

        transcript = self.page.locator(".transcript")
        tools = transcript.locator(".tool")
        main = self.page.locator(".agent-page .main")

        def at_bottom() -> None:
            deadline = time.monotonic() + 15
            while True:
                top, height, total = main.evaluate(
                    "(e) => [e.scrollTop, e.clientHeight, e.scrollHeight]"
                )
                if abs(top + height - total) <= 1:
                    return
                self.assertLess(time.monotonic(), deadline, "transcript not at bottom")
                time.sleep(0.1)

        def check() -> None:
            expect(tools).to_have_count(count)
            at_bottom()
            for i in [0, count - 1]:
                tool = tools.nth(i)
                tool.locator("summary").click()
                expect(tool.locator(".tool-result")).to_contain_text(f"result {i}:")
                # A Bash call shows its command highlighted, not raw JSON.
                tool_input = tool.locator(".tool-input")
                expect(tool_input).to_contain_text(f"step {i}")
                expect(tool_input.locator('span[class*="sy-"]').first).to_be_visible()
                tool.locator("summary").click()

        # As it streams in.
        expect(transcript.locator(".turn-end")).to_have_text(
            ["Done · $0.01"], timeout=60_000
        )
        check()
        # Replayed from the stored transcript after a reload.
        self.page.reload()
        check()
        self.delete_session()

    def test_collapsed_tools_compact(self) -> None:
        # Collapsed tool calls carry no visible border and pack tight;
        # opening one gives it back its border.
        self.page.goto(self.url + "#/agents")
        self.page.get_by_label("Repository").select_option(scenarios.REPO)
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("long transcript 3")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page.locator(".transcript .turn-end")).to_have_count(
            1, timeout=60_000
        )

        tools = self.page.locator(".transcript .tool")
        expect(tools).to_have_count(3)
        first = tools.first
        self.assertEqual(
            first.evaluate("e => getComputedStyle(e).borderTopColor"),
            "rgba(0, 0, 0, 0)",
        )
        first.locator("summary").click()
        self.assertNotEqual(
            first.evaluate("e => getComputedStyle(e).borderTopColor"),
            "rgba(0, 0, 0, 0)",
        )
        first.locator("summary").click()

        first_box = tools.nth(0).bounding_box()
        second_box = tools.nth(1).bounding_box()
        assert first_box and second_box
        self.assertLessEqual(
            second_box["y"] - (first_box["y"] + first_box["height"]), 3
        )
        self.delete_session()

    def test_activity_bar(self) -> None:
        # The activity bar waits with the agent for the approval and goes
        # once the turn ends.
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Ask for everything")
        self.page.get_by_label("Prompt").fill("please write a note")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))

        transcript = self.page.locator(".transcript")
        approval = transcript.locator(".approval")
        expect(approval).to_contain_text("Claude wants to use Write notes.txt")
        activity = self.page.locator(".activity")
        expect(activity).to_contain_text("Waiting for your approval")
        approval.get_by_role("button", name="Allow").click()
        expect(transcript.locator(".turn-end")).to_have_count(1)
        expect(activity).to_have_count(0)

    def test_images(self) -> None:
        self.page.goto(self.url + "#/agents")
        self.page.get_by_label("Repository").select_option(scenarios.REPO)
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("screenshot")
        self.page.get_by_role("button", name="Start").click()
        transcript = self.page.locator(".transcript")
        expect(transcript.locator(".turn-end")).to_have_count(1)

        # The screenshot Read returned shows below its collapsed tool call,
        # and the one Claude showed has its caption.
        images = transcript.locator(".turn-image")
        expect(images).to_have_count(2)
        expect(images.locator("figcaption")).to_have_text(["The page"])
        expect(images.last.locator("img")).to_have_attribute("alt", "The page")
        for img in images.locator("img").all():
            img.scroll_into_view_if_needed()
            expect(img).to_have_js_property("naturalWidth", 1)
        expect(transcript.locator(".tool[open]")).to_have_count(0)

        # They're stored, so a reload shows them again.
        self.page.reload()
        expect(images).to_have_count(2)
        expect(images.first.locator("img")).to_have_js_property("naturalWidth", 1)

    def test_sidebar_delete(self) -> None:
        sessions = self.section(scenarios.REPO).locator(".agent-entry")
        for prompt in ["say one", "say two"]:
            self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
            self.page.get_by_label("Permissions").select_option(label="Auto")
            self.page.get_by_label("Prompt").fill(prompt)
            self.page.get_by_role("button", name="Start").click()
            expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
            expect(self.page.locator(".transcript .turn-end")).to_have_count(1)
        two = self.page.url
        one = sessions.filter(has_text="say one")
        trash = one.get_by_role("button", name="Delete session")

        # The trash button asks first, like the session page's Delete.
        self.page.once("dialog", lambda dialog: dialog.dismiss())
        trash.click()
        expect(one).to_have_count(1)

        # Deleting another session leaves the open one on the page.
        self.page.once("dialog", lambda dialog: dialog.accept())
        trash.click()
        expect(one).to_have_count(0)
        self.assertEqual(self.page.url, two)
        expect(self.page.locator(".transcript .turn-prompt")).to_have_text(["say two"])

        # Deleting the open session goes back to the form.
        self.page.once("dialog", lambda dialog: dialog.accept())
        sessions.filter(has_text="say two").get_by_role(
            "button", name="Delete session"
        ).click()
        expect(self.page).to_have_url(re.compile(DRAFT_URL))
        expect(sessions.filter(has_text="say two")).to_have_count(0)

    def test_follow_up_draft_survives_switching_sessions(self) -> None:
        sessions = self.section(scenarios.REPO).locator(".agent-item")
        transcript = self.page.locator(".transcript")
        for prompt in ["say left", "say right"]:
            self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
            self.page.get_by_label("Permissions").select_option(label="Auto")
            self.page.get_by_label("Prompt").fill(prompt)
            self.page.get_by_role("button", name="Start").click()
            expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
            expect(transcript.locator(".turn-end")).to_have_count(1)
        follow_up = self.page.get_by_label("Follow-up")
        follow_up.fill("half a thought")

        # Each session keeps its own draft.
        sessions.filter(has_text="say left").click()
        expect(transcript.locator(".turn-prompt")).to_have_text(["say left"])
        expect(follow_up).to_have_value("")
        sessions.filter(has_text="say right").click()
        expect(transcript.locator(".turn-prompt")).to_have_text(["say right"])
        expect(follow_up).to_have_value("half a thought")

        # The draft survives a reload too, until it's sent.
        self.page.reload()
        expect(follow_up).to_have_value("half a thought")
        follow_up.press("Enter")
        expect(transcript.locator(".turn-prompt")).to_have_text(
            ["say right", "half a thought"]
        )
        expect(follow_up).to_have_value("")
        self.page.reload()
        expect(transcript.locator(".turn-prompt")).to_have_count(2)
        expect(follow_up).to_have_value("")

    def test_pasted_images_go_with_the_prompt(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("say hi")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        transcript = self.page.locator(".transcript")
        expect(transcript.locator(".turn-end")).to_have_count(1)

        # A screenshot pasted into the text box shows as a thumbnail.
        follow_up = self.page.get_by_label("Follow-up")
        follow_up.fill("look at this")
        follow_up.evaluate(PASTE_IMAGE)
        thumbnails = self.page.locator(".prompt-image img")
        expect(thumbnails).to_have_count(1)
        follow_up.evaluate(PASTE_IMAGE)
        expect(thumbnails).to_have_count(2)
        self.page.get_by_role("button", name="Remove the image").first.click()
        expect(thumbnails).to_have_count(1)
        # The pasted text stays as it was.
        expect(follow_up).to_have_value("look at this")

        # Claude gets the image with the prompt, scaled down, and the
        # transcript shows it.
        follow_up.press("Enter")
        expect(transcript.locator(".turn-end")).to_have_count(2)
        expect(transcript).to_contain_text("I see 1 images.")
        expect(thumbnails).to_have_count(0)
        sent = transcript.locator(".turn-images img")
        expect(sent).to_have_count(1)
        expect(sent).to_have_js_property("naturalWidth", 1568)
        self.page.reload()
        expect(sent).to_have_js_property("naturalWidth", 1568)

    def test_new_session_images_survive_leaving_the_form(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        prompt = self.page.get_by_label("Prompt")
        prompt.fill("say hi")
        prompt.evaluate(PASTE_IMAGE)
        thumbnails = self.page.locator(".prompt-image img")
        expect(thumbnails).to_have_count(1)

        # Going away and back to the draft keeps the image.
        draft = (
            self.section(scenarios.REPO)
            .locator(".agent-entry")
            .filter(has=self.page.locator(".agent-draft"))
        )
        expect(draft).to_have_count(1)
        self.page.get_by_role("link", name="Agents").click()
        expect(thumbnails).to_have_count(0)
        draft.click()
        expect(prompt).to_have_value("say hi")
        expect(thumbnails).to_have_count(1)

        # And a reload.
        self.page.reload()
        expect(thumbnails).to_have_count(1)

        # The image goes with the started session's prompt.
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        transcript = self.page.locator(".transcript")
        expect(transcript.locator(".turn-end")).to_have_count(1)
        expect(transcript.locator(".turn-images img")).to_have_count(1)

        # A fresh form has no images.
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        expect(thumbnails).to_have_count(0)

    def test_follow_up_images_survive_switching_sessions(self) -> None:
        sessions = self.section(scenarios.REPO).locator(".agent-item")
        transcript = self.page.locator(".transcript")
        for prompt in ["say left", "say right"]:
            self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
            self.page.get_by_label("Permissions").select_option(label="Auto")
            self.page.get_by_label("Prompt").fill(prompt)
            self.page.get_by_role("button", name="Start").click()
            expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
            expect(transcript.locator(".turn-end")).to_have_count(1)
        follow_up = self.page.get_by_label("Follow-up")
        thumbnails = self.page.locator(".prompt-image img")

        # An image pasted into "say right"'s composer is its own.
        follow_up.evaluate(PASTE_IMAGE)
        expect(thumbnails).to_have_count(1)
        sessions.filter(has_text="say left").click()
        expect(transcript.locator(".turn-prompt")).to_have_text(["say left"])
        expect(thumbnails).to_have_count(0)
        sessions.filter(has_text="say right").click()
        expect(transcript.locator(".turn-prompt")).to_have_text(["say right"])
        expect(thumbnails).to_have_count(1)

        # It survives a reload too, until it's sent.
        self.page.reload()
        expect(thumbnails).to_have_count(1)
        follow_up.press("Enter")
        expect(transcript).to_contain_text("I see 1 images.")
        expect(thumbnails).to_have_count(0)
        self.page.reload()
        expect(thumbnails).to_have_count(0)

    def test_new_session_drafts_survive_switching(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("say started")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page.locator(".transcript .turn-end")).to_have_count(1)

        new = self.page.get_by_role("link", name="+ New session")
        prompt = self.page.get_by_label("Prompt")
        repo = self.page.get_by_label("Repository")
        permissions = self.page.get_by_label("Permissions")
        is_draft = self.page.locator(".agent-draft")
        entries = self.section(scenarios.REPO).locator(".agent-entry")
        drafts = entries.filter(has=is_draft)
        other_drafts = (
            self.section(self.OTHER_REPO).locator(".agent-entry").filter(has=is_draft)
        )

        # Each new form is a draft of its own, listed first in its
        # repository's section while it's open.
        new.click()
        expect(self.page).to_have_url(re.compile(DRAFT_URL))
        expect(prompt).to_be_focused()
        repo.select_option(scenarios.REPO)
        expect(drafts).to_have_count(1)
        expect(drafts.locator(".agent-title")).to_have_text("New session")
        expect(drafts.locator(".agent-status")).to_have_text("Draft")
        prompt.fill("first draft\nwith more")
        expect(drafts.locator(".agent-title")).to_have_text("first draft")
        first = self.page.url
        new.click()
        expect(prompt).to_have_value("")
        expect(prompt).to_be_focused()
        repo.select_option(scenarios.REPO)
        prompt.fill("second draft")
        permissions.select_option(label="Accept edits")
        second = self.page.url
        self.assertNotEqual(first, second)
        expect(drafts.locator(".agent-title")).to_have_text(
            ["second draft", "first draft"]
        )
        # Above the sessions, and set apart from them.
        titles = entries.locator(".agent-title")
        expect(titles.nth(0)).to_have_text("second draft")
        expect(titles.nth(1)).to_have_text("first draft")
        expect(entries.nth(2).locator(".agent-draft")).to_have_count(0)
        expect(entries.nth(2).locator(".agent-item")).to_have_count(1)

        # Picking another repository moves the draft to its section.
        repo.select_option(self.OTHER_REPO)
        expect(drafts.locator(".agent-title")).to_have_text(["first draft"])
        expect(other_drafts.locator(".agent-title")).to_have_text(["second draft"])
        expect(other_drafts.locator(".agent-draft")).to_have_class(
            re.compile(r"\bselected\b")
        )

        # Going to a session and back keeps both drafts as they were.
        started = self.section(scenarios.REPO).locator(".agent-item")
        started.filter(has_text="say started").click()
        expect(prompt).to_have_count(0)
        drafts.click()
        self.assertEqual(self.page.url, first)
        expect(prompt).to_have_value("first draft\nwith more")
        expect(prompt).to_be_focused()
        end = len("first draft\nwith more")
        self.assertEqual(
            prompt.evaluate("e => [e.selectionStart, e.selectionEnd, e.value.length]"),
            [end, end, end],
        )
        expect(repo).to_have_value(scenarios.REPO)
        expect(permissions).to_have_value("plan")
        other_drafts.click()
        self.assertEqual(self.page.url, second)
        expect(prompt).to_have_value("second draft")
        expect(repo).to_have_value(self.OTHER_REPO)
        expect(permissions).to_have_value("acceptEdits")

        # They survive a reload too.
        self.page.reload()
        expect(prompt).to_have_value("second draft")
        expect(drafts).to_have_count(1)
        expect(other_drafts).to_have_count(1)

        # Starting one turns its draft into a session. Its form has to be
        # showing before typing, or the text goes into the one before.
        drafts.click()
        expect(prompt).to_have_value("first draft\nwith more")
        permissions.select_option(label="Auto")
        prompt.fill("say from a draft")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        expect(drafts).to_have_count(0)
        expect(started.filter(has_text="say from a draft")).to_have_count(1)
        expect(other_drafts).to_have_count(1)

        # Discarding the open draft goes to a new one, which may be listed
        # in either section.
        other_drafts.click()
        expect(prompt).to_have_value("second draft")
        other_drafts.get_by_role("button", name="Discard draft").click()
        expect(self.page).to_have_url(re.compile(DRAFT_URL))
        self.assertNotEqual(self.page.url, second)
        expect(is_draft.filter(has_text="second draft")).to_have_count(0)

        # A form that's left without writing anything isn't kept.
        expect(is_draft.locator(".agent-title")).to_have_text(["New session"])
        started.filter(has_text="say started").click()
        expect(is_draft).to_have_count(0)
        self.page.reload()
        expect(is_draft).to_have_count(0)

    def hold_agent_posts(self) -> list:
        """Stashes the routes of POSTs to /api/agents, which start sessions,
        so the test can release them with `route.continue_()`."""
        held = []

        def hold(route) -> None:
            if route.request.method == "POST":
                held.append(route)
            else:
                route.continue_()

        self.page.route("**/api/agents", hold)
        return held

    def hold_agent_answers(self) -> list:
        """Stashes the server's answers to POSTs to /api/agents, so the
        session exists before the page learns of it; the test releases
        each answer with `route.fulfill(response=response)`."""
        held = []

        def hold(route) -> None:
            if route.request.method == "POST":
                held.append((route, route.fetch()))
            else:
                route.continue_()

        self.page.route("**/api/agents", hold)
        return held

    def wait_held(self, held: list) -> None:
        for _ in range(100):
            if held:
                return
            self.page.wait_for_timeout(50)
        self.fail("no session start reached the server")

    def test_starting_a_draft_leaves_you_where_you_went(self) -> None:
        # A session to go to while the draft's start is in flight.
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("say first")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        first_url = self.page.url

        held = self.hold_agent_posts()
        self.page.get_by_role("link", name="+ New session").click()
        self.page.get_by_label("Repository").select_option(scenarios.REPO)
        self.page.get_by_label("Prompt").fill("say second")
        self.page.get_by_role("button", name="Start").click()
        # Going to another session while the start is held stays there
        # once it answers.
        self.section(scenarios.REPO).locator(
            ".agent-item", has_text="say first"
        ).click()
        self.wait_held(held)
        # The row can appear before the start's answer reaches the page,
        # so wait for the answer itself.
        with self.page.expect_response(
            lambda r: r.request.method == "POST" and r.url.endswith("/api/agents")
        ) as answer:
            held[0].continue_()
        answer.value.finished()
        entries = self.section(scenarios.REPO).locator(".agent-entry")
        expect(entries.filter(has_text="say second")).to_have_count(1)
        self.assertEqual(self.page.url, first_url)
        expect(self.page.locator(".agent-draft")).to_have_count(0)

    def test_returning_to_a_starting_draft(self) -> None:
        held = self.hold_agent_posts()
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("say held")
        self.page.get_by_role("button", name="Start").click()

        # Coming back to the draft while its start is held finds it
        # starting: no second Start, no discard.
        self.page.get_by_role("link", name="+ New session").click()
        draft = self.page.locator(".agent-draft", has_text="say held")
        draft.click()
        submit = self.page.locator("button.primary[type=submit]")
        expect(submit).to_be_disabled()
        expect(submit).not_to_have_text("Start")
        expect(draft.locator(".agent-status")).to_have_text("Starting…")
        entry = self.page.locator(
            ".agent-entry",
            has=self.page.locator(".agent-draft", has_text="say held"),
        )
        expect(entry.get_by_role("button", name="Discard draft")).to_be_hidden()

        # Releasing the start takes the page, still on the draft, to the
        # session; only one comes of it.
        self.wait_held(held)
        held[0].continue_()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        entries = self.section(scenarios.REPO).locator(".agent-entry")
        expect(entries.filter(has_text="say held")).to_have_count(1)
        expect(draft).to_have_count(0)

    def test_a_session_opened_before_its_start_answers_keeps_its_mode(self) -> None:
        held = self.hold_agent_answers()
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Accept edits")
        self.page.get_by_label("Prompt").fill("say held")
        self.page.get_by_role("button", name="Start").click()

        # The sidebar can open the session before its start answers.
        permissions = self.page.get_by_label("Permissions")
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        expect(permissions).to_have_value("acceptEdits")
        self.wait_held(held)
        route, response = held[0]
        route.fulfill(response=response)
        expect(permissions).to_have_value("acceptEdits")
        self.page.reload()
        expect(permissions).to_have_value("acceptEdits")

    def test_a_draft_whose_session_started_is_cleaned_up(self) -> None:
        # The server can start a draft's session without the page seeing
        # the answer, e.g. a tab closed mid-request; the draft left
        # behind becomes the session's entry on the next load.
        resp = self.page.request.post(
            f"{self.url}api/agents",
            headers={"Origin": self.url.rstrip("/")},
            data={
                "backend": "claude",
                "repo": scenarios.REPO,
                "prompt": "say orphaned",
                "permission_mode": "auto",
                "draft": "orphandraft1",
            },
        )
        self.assertEqual(resp.status, 200, resp.text())
        session_id = resp.json()["id"]
        drafts = json.dumps(
            [
                {
                    "id": "orphandraft1",
                    "repo": scenarios.REPO,
                    "pr": "",
                    "prompt": "say orphaned",
                    "mode": "plan",
                    "model": "default",
                    "createdAt": 1,
                }
            ]
        )
        self.page.add_init_script(
            "localStorage.setItem("
            f"'grenadine.new-session-drafts', {json.dumps(drafts)})"
        )
        self.page.goto(self.url + "#/agents/draft/orphandraft1")
        expect(self.page).to_have_url(re.compile(f"#/agents/{session_id}$"))
        expect(
            self.page.locator(".agent-draft", has_text="say orphaned")
        ).to_have_count(0)
        expect(
            self.section(scenarios.REPO)
            .locator(".agent-entry")
            .filter(has_text="say orphaned")
        ).to_have_count(1)

    def test_unstaged_changes_pane(self) -> None:
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("write a note")
        self.page.get_by_role("button", name="Start").click()
        transcript = self.page.locator(".transcript")
        expect(transcript.locator(".turn-end")).to_have_count(1)

        # The note the agent wrote isn't tracked yet, so it shows as added.
        pane = self.page.locator(".changes-pane")
        # The session opened no PRs, so there's no associated section.
        expect(pane.locator(".associated-prs")).to_have_count(0)
        files = pane.locator("section.file")
        expect(files.locator(".path")).to_have_text(["notes.txt"])
        expect(files.locator(".status")).to_have_text(["added"])
        expect(pane.locator("table.diff.unified tr.add .code")).to_have_text(
            ["write a note"]
        )
        # The sidebar counts the files too.
        entry = (
            self.section(self.OTHER_REPO)
            .locator(".agent-entry")
            .filter(has=self.page.locator(".agent-item.selected"))
        )
        count = entry.locator(".agent-unstaged")
        expect(count).to_have_text("1")
        expect(count).to_have_attribute("title", "1 file with unstaged changes")

        # Fold notes.txt. Tagging the pane proves below that the next
        # turn's session updates don't rebuild it.
        pane.evaluate("el => el.dataset.probe = '1'")
        files.locator(".file-header").click()
        expect(files.locator("table")).to_have_count(0)

        # Edits made since show up with the agent's next events.
        worktree = Path(self.page.locator(".agent-worktree").inner_text())
        (worktree / "README").write_text("hello\nworld\n")
        follow_up = self.page.get_by_label("Follow-up")
        follow_up.fill("write more")
        follow_up.press("Enter")
        expect(transcript.locator(".turn-end")).to_have_count(2)
        expect(files.locator(".path")).to_have_text(["README", "notes.txt"])
        expect(files.locator(".status")).to_have_text(["modified", "added"])
        expect(count).to_have_text("2")
        expect(count).to_have_attribute("title", "2 files with unstaged changes")
        # The pane kept its element, and notes.txt its fold even though
        # its contents changed; README starts unfolded.
        expect(pane).to_have_attribute("data-probe", "1")
        expect(files.first.locator("tr.add .code")).to_have_text(["world"])
        expect(files.nth(1).locator("table")).to_have_count(0)
        files.nth(1).locator(".file-header").click()
        expect(files.nth(1).locator("tr.add .code")).to_have_text(["write more"])
        files.nth(1).locator(".file-header").click()
        expect(files.nth(1).locator("table")).to_have_count(0)

        # Folds are kept in the tab's session storage, so they survive a
        # reload.
        self.page.reload()
        expect(files.locator(".path")).to_have_text(["README", "notes.txt"])
        expect(files.first.locator("tr.add .code")).to_have_text(["world"])
        expect(files.nth(1).locator("table")).to_have_count(0)

        # The transcript keeps most of the width.
        main = self.page.locator(".agent-panes > .main").bounding_box()
        side = pane.bounding_box()
        assert main and side
        self.assertGreaterEqual(main["width"], 720)
        self.assertGreaterEqual(main["width"], side["width"])

        # Without room for both, only the transcript stays.
        self.page.set_viewport_size({"width": 1200, "height": 900})
        expect(pane).to_have_count(0)
        expect(transcript).to_be_visible()
        self.page.set_viewport_size({"width": 1800, "height": 900})
        expect(files.locator(".path")).to_have_text(["README", "notes.txt"])

    def test_terminals(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("write a note")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page.locator(".transcript .turn-end")).to_have_count(1)
        session = Path(self.page.locator(".agent-worktree").inner_text()).name
        tabs = self.page.get_by_role("tab")
        screen = self.page.locator(".terminal-host:not([hidden]) .xterm-rows")

        def wait_for_shell(tab: Locator) -> None:
            # Keys typed while a tab parses its replay never reach the
            # shell, so wait until it's done and the prompt shows.
            expect(tab).to_have_attribute("aria-busy", "false")
            # Remote executors run the tests as root, whose prompt is #.
            expect(screen).to_contain_text(re.compile(r"[$#]"))

        # Opening the panel starts a shell in the worktree. The output
        # differs from the command line it echoes.
        self.page.get_by_role("button", name="Terminal", exact=True).click()
        expect(tabs).to_have_text(["Terminal 1"])
        wait_for_shell(tabs.nth(0))
        self.page.keyboard.type('echo in-$(basename "$PWD")\n')
        expect(screen).to_contain_text(f"in-{session}")

        # Another tab gets its own shell, and the first keeps its output.
        self.page.get_by_role("button", name="New terminal").click()
        expect(tabs).to_have_text(["Terminal 1", "Terminal 2"])
        expect(tabs.nth(1)).to_have_attribute("aria-selected", "true")
        wait_for_shell(tabs.nth(1))
        self.page.keyboard.type("echo second-$((6 * 7))\n")
        expect(screen).to_contain_text("second-42")
        tabs.nth(0).click()
        expect(screen).to_contain_text(f"in-{session}")
        expect(screen).not_to_contain_text("second-42")

        # The shells outlive the page, and their output comes back in the
        # tab that was selected.
        self.page.reload()
        expect(tabs).to_have_text(["Terminal 1", "Terminal 2"])
        expect(tabs.nth(0)).to_have_attribute("aria-selected", "true")
        expect(screen).to_contain_text(f"in-{session}")

        # Each session keeps its own panel: another session without
        # terminals has it collapsed, and coming back shows the tab that
        # was selected.
        toggle = self.page.get_by_role("button", name="Terminal", exact=True)
        first = self.page.url
        tabs.nth(1).click()

        # Escape sequences that ask the terminal about itself, which a
        # program may leave in the pty's output, get answers from xterm;
        # the interrupt flushes the live ones out of the shell's input
        # and the clear takes their echoes off the screen. On a reconnect
        # the same queries come back in the replay and must not reach the
        # shell again.
        self.page.keyboard.type("printf '\\033[c\\033]11;?\\033\\\\'\n")
        self.page.wait_for_timeout(500)
        self.page.keyboard.press("Control+C")
        self.page.keyboard.type("clear\n")
        self.page.keyboard.type("echo back-$((6 * 7))\n")
        expect(screen).to_contain_text("back-42")

        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("write another note")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page.locator(".transcript .turn-end")).to_have_count(1)
        expect(toggle).to_have_attribute("aria-expanded", "false")
        expect(tabs).to_have_count(0)
        self.page.goto(first)
        expect(toggle).to_have_attribute("aria-expanded", "true")
        expect(tabs.nth(1)).to_have_attribute("aria-selected", "true")
        # The marker sits at the replay's end, so the replay is parsed
        # once it shows.
        expect(screen).to_contain_text("back-42")

        # The tab reports the replay's progress, and the answers xterm
        # gives its queries while parsing don't reach the shell.
        expect(tabs.nth(1)).to_have_attribute("aria-busy", "false")
        self.page.keyboard.type("echo clean-$((6 * 7))\n")
        expect(screen).to_contain_text("clean-42")
        expect(screen).not_to_contain_text("rgb:")
        expect(screen).not_to_contain_text("1;2c")

        # Closing a tab hangs up on its shell; exiting a shell closes its tab,
        # and the panel collapses without any.
        self.page.get_by_role("button", name="Close terminal 2").click()
        expect(tabs).to_have_text(["Terminal 1"])
        expect(screen).to_contain_text(f"in-{session}")
        self.page.keyboard.type("exit\n")
        expect(tabs).to_have_count(0)
        expect(toggle).to_have_attribute("aria-expanded", "false")

        # Other sites' pages can't connect to a shell.
        self.page.get_by_role("button", name="New terminal").click()
        expect(tabs).to_have_text(["Terminal 3"])
        session_id = self.page.url.rsplit("/", 1)[1]
        port = int(self.url.rsplit(":", 1)[1].strip("/"))

        def handshake(origin: str) -> int:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            try:
                conn.request(
                    "GET",
                    f"/api/agents/{session_id}/terminals/3/ws",
                    headers={
                        "Connection": "Upgrade",
                        "Upgrade": "websocket",
                        "Sec-WebSocket-Version": "13",
                        "Sec-WebSocket-Key": base64.b64encode(os.urandom(16)).decode(),
                        "Origin": origin,
                    },
                )
                return conn.getresponse().status
            finally:
                conn.close()

        self.assertEqual(handshake("http://evil.example"), 403)
        self.assertEqual(handshake(f"http://127.0.0.1:{port}"), 101)

    def pr_page(self) -> Locator:
        """The PR page's agents panel, in a window wide enough for the sidebar."""
        self.page.set_viewport_size({"width": 1800, "height": 900})
        self.page.goto(f"{self.url}#/{scenarios.REPO}/{self.panel_pr.number}")
        toggle = self.page.locator(".pr-agents-toggle")
        expect(toggle).to_be_visible()
        return self.page.locator(".pr-agents-panel")

    def start_pr_session(self, prompt: str) -> None:
        """Starts a session from the PR on the Agents tab."""
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        # The unlocked form shows its run hints plainly.
        expect(self.page.locator(".agent-form")).to_contain_text(
            "its own git worktree", use_inner_text=True
        )
        self.page.get_by_label("Start from PR").fill(str(self.panel_pr.number))
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill(prompt)
        self.page.get_by_role("button", name="Start").click()
        expect(self.page.locator(".transcript .turn-end")).to_have_count(1)

    def test_pr_agents_panel_starts_a_session(self) -> None:
        panel = self.pr_page()
        toggle = panel.locator(".pr-agents-toggle")
        expect(toggle).to_have_attribute("aria-expanded", "false")

        # With no sessions, expanding shows the new session form locked
        # to the PR.
        toggle.click()
        expect(toggle).to_have_attribute("aria-expanded", "true")
        form = panel.locator(".agent-form")
        expect(form).to_contain_text(
            f"Starts from #{self.panel_pr.number} in {scenarios.REPO}"
        )
        expect(panel.get_by_label("Repository")).to_have_count(0)
        expect(panel.get_by_label("Start from PR")).to_have_count(0)
        expect(panel.get_by_label("Prompt")).to_be_focused()

        # The run hints sit behind a summary naming the permission mode.
        hints = panel.locator(".session-hints")
        summary = hints.locator("summary")
        expect(summary).to_contain_text("How this session runs")
        expect(form).not_to_contain_text("its own git worktree", use_inner_text=True)
        summary.click()
        expect(form).to_contain_text("its own git worktree", use_inner_text=True)

        panel.get_by_label("Permissions", exact=True).select_option(label="Auto")
        expect(summary).to_contain_text("Auto")
        panel.get_by_label("Prompt").fill("say one")
        panel.get_by_role("button", name="Start").click()
        # The page stays on the PR, and the draft's tab becomes the
        # session's, selected.
        expect(self.page).to_have_url(
            re.compile(f"#/{scenarios.REPO}/{self.panel_pr.number}$")
        )
        tabs = panel.get_by_role("tab")
        expect(tabs).to_have_count(1)
        expect(tabs.first).to_have_attribute("aria-selected", "true")
        transcript = panel.locator(".transcript")
        expect(transcript.locator(".turn-prompt")).to_have_text(["say one"])
        expect(transcript.locator(".turn-end")).to_have_count(1)

        # The session answers follow-ups from the composer.
        follow_up = panel.get_by_label("Follow-up")
        follow_up.fill("say more")
        follow_up.press("Enter")
        expect(transcript.locator(".turn-prompt")).to_have_text(["say one", "say more"])

    def test_pr_agents_panel_lists_sessions_started_from_the_pr(self) -> None:
        self.start_pr_session("say one")
        self.start_pr_session("say two")

        panel = self.pr_page()
        panel.locator(".pr-agents-toggle").click()
        tabs = panel.get_by_role("tab")
        expect(tabs).to_have_count(2)
        # Oldest first, and the oldest is the one opened.
        expect(tabs.first).to_contain_text("say one")
        expect(tabs.first).to_have_attribute("aria-selected", "true")
        transcript = panel.locator(".transcript")
        expect(transcript.locator(".turn-prompt")).to_have_text(["say one"])
        # The other tab shows its own transcript.
        tabs.nth(1).click()
        expect(transcript.locator(".turn-prompt")).to_have_text(["say two"])

    def test_pr_agents_panel_survives_reload_and_sync(self) -> None:
        self.start_pr_session("say one")

        panel = self.pr_page()
        toggle = panel.locator(".pr-agents-toggle")
        toggle.click()
        transcript = panel.locator(".transcript")
        expect(transcript.locator(".turn-end")).to_have_count(1)
        follow_up = panel.get_by_label("Follow-up")
        follow_up.fill("half a thought")
        follow_up.focus()
        # The node, tagged so a remount that replaced it shows.
        follow_up.evaluate("el => { el.__kept = true; }")

        # A comment from outside the page resyncs the PR, which rebuilds
        # the review sidebar but not the panel under it. The resync also
        # refetches the panel's sessions.
        with self.page.expect_response(
            lambda r: r.url.endswith(
                f"/api/pr/{scenarios.REPO}/{self.panel_pr.number}/agents"
            )
        ):
            resp = self.page.request.post(
                f"{self.url}api/pr/{scenarios.REPO}/{self.panel_pr.number}/comments",
                headers={"Origin": self.url.rstrip("/")},
                data={"body": "Ping"},
            )
        self.assertEqual(resp.status, 204, resp.text())
        expect(self.page.locator("details.conversation summary")).to_have_text(
            "1 comment"
        )
        expect(toggle).to_have_attribute("aria-expanded", "true")
        expect(follow_up).to_have_value("half a thought")
        expect(transcript.locator(".turn-prompt")).to_have_text(["say one"])
        # The refetched panel keeps the box as the same node, still
        # focused mid-typing.
        expect(follow_up).to_be_focused()
        self.assertTrue(follow_up.evaluate("el => el.__kept === true"))

        # The expansion and the selected tab survive a reload.
        self.page.reload()
        expect(toggle).to_have_attribute("aria-expanded", "true")
        tabs = panel.get_by_role("tab")
        expect(tabs).to_have_count(1)
        expect(tabs.first).to_have_attribute("aria-selected", "true")
        expect(transcript.locator(".turn-prompt")).to_have_text(["say one"])
        expect(follow_up).to_have_value("half a thought")

    def test_pr_agents_panel_terminals(self) -> None:
        self.start_pr_session("say one")
        session = Path(self.page.locator(".agent-worktree").inner_text()).name

        panel = self.pr_page()
        panel.locator(".pr-agents-toggle").click()
        expect(panel.locator(".transcript .turn-end")).to_have_count(1)
        terminals = panel.locator(".terminal-panel")
        toggle = terminals.get_by_role("button", name="Terminal", exact=True)
        tabs = terminals.get_by_role("tab")
        screen = terminals.locator(".terminal-host:not([hidden]) .xterm-rows")

        # The session's terminal panel starts collapsed under its chat;
        # opening it starts a shell in the session's worktree.
        expect(toggle).to_have_attribute("aria-expanded", "false")
        toggle.click()
        expect(tabs).to_have_text(["Terminal 1"])
        # Keys typed while the tab parses its replay never reach the shell,
        # so wait until it's done and the prompt shows.
        expect(tabs.nth(0)).to_have_attribute("aria-busy", "false")
        expect(screen).to_contain_text(re.compile(r"[$#]"))
        self.page.keyboard.type('echo in-$(basename "$PWD")\n')
        expect(screen).to_contain_text(f"in-{session}")

        # The terminal shares the panel with the chat, which keeps room.
        body = panel.locator(".pr-agents-body").bounding_box()
        shell = terminals.locator(".terminal-body").bounding_box()
        assert body is not None and shell is not None
        self.assertLess(shell["height"], body["height"] * 0.9)
        expect(panel.get_by_label("Follow-up")).to_be_visible()

        # The shell outlives the page, and the panel comes back open.
        self.page.reload()
        expect(toggle).to_have_attribute("aria-expanded", "true")
        expect(tabs).to_have_text(["Terminal 1"])
        expect(screen).to_contain_text(f"in-{session}")

        # It's the same shell the session's own page shows.
        self.page.get_by_role("link", name="Open in Agents tab").click()
        page_tabs = self.page.locator(".terminal-panel").get_by_role("tab")
        expect(page_tabs).to_have_text(["Terminal 1"])
        expect(
            self.page.locator(".terminal-host:not([hidden]) .xterm-rows")
        ).to_contain_text(f"in-{session}")

    def test_wrap_up(self) -> None:
        # The server's clone has main checked out, which the work lands on.
        clone = self.clones[self.OTHER_REPO]
        env = self.gh.git_env()

        def git(*args: str) -> str:
            return subprocess.run(
                ["git", "-C", clone, *args],
                env=env,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()

        git("fetch", "--quiet", "origin", "main")
        git("checkout", "--quiet", "-B", "main", "FETCH_HEAD")

        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        prompt = self.page.get_by_label("Prompt")
        prompt.fill("write a note")
        # Enter starts the session.
        prompt.press("Enter")
        transcript = self.page.locator(".transcript")
        expect(transcript.locator(".turn-end")).to_have_count(1)

        # The note isn't committed, so Claude is asked to commit it first.
        header = self.page.locator(".agent-header")
        actions = self.page.locator(".agent-actions")
        actions.get_by_role("button", name="Merge into checkout").click()
        expect(transcript.locator(".turn-prompt").nth(1)).to_contain_text(
            "Wrap up: commit"
        )
        expect(transcript.locator(".wrapped-up")).to_contain_text(
            "Merged into the checkout at"
        )
        sha = git("rev-parse", "HEAD")
        expect(transcript.locator(".wrapped-up code")).to_have_text(sha)
        expect(header).to_contain_text(f"merged {sha[:8]}")
        self.assertEqual(git("branch", "--show-current"), "main")
        self.assertEqual((clone / "notes.txt").read_text(), "write a note")
        self.assertEqual(git("log", "-1", "--format=%s"), "Write notes")

        # Then it proposes more work in a PR.
        follow_up = self.page.get_by_label("Follow-up")
        follow_up.fill("write more")
        follow_up.press("Enter")
        expect(transcript.locator(".turn-end")).to_have_count(3)
        actions.get_by_role("button", name="Open PR").click()
        opened = transcript.locator(".wrapped-up").nth(1)
        expect(opened).to_contain_text(re.compile(r"Opened PR #\d+"))
        pr = next(
            pr
            for pr in self.gh.repo(self.OTHER_REPO).prs.values()
            if pr.title == "Write notes"
        )
        expect(opened.get_by_role("link", name="Open on GitHub")).to_have_attribute(
            "href", pr.url
        )
        expect(header.get_by_role("link", name=f"PR #{pr.number}")).to_be_visible()
        # The sidebar counts the PR, which isn't merged yet.
        entry = (
            self.section(self.OTHER_REPO)
            .locator(".agent-entry")
            .filter(has=self.page.locator(".agent-item.selected"))
        )
        expect(entry.locator(".agent-prs")).to_have_text("0/1")
        self.assertEqual(pr.base, "main")
        self.assertEqual(pr.head, "dev/test/write-notes")
        # The checkout stays where the first wrap-up left it.
        self.assertEqual(git("rev-parse", "HEAD"), sha)

    def test_wrap_up_stack(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        prompt = self.page.get_by_label("Prompt")
        prompt.fill("commit stack-a")
        prompt.press("Enter")
        transcript = self.page.locator(".transcript")
        done = transcript.locator(".turn-end.muted")
        expect(done).to_have_count(1)
        follow_up = self.page.get_by_label("Follow-up")
        follow_up.fill("commit stack-b")
        follow_up.press("Enter")
        expect(done).to_have_count(2)

        # Each commit gets its own PR, into the branch of the one below.
        actions = self.page.locator(".agent-actions")
        actions.get_by_role("button", name="Open PR").click()
        opened = transcript.locator(".wrapped-up")
        links = opened.get_by_role("link", name=re.compile(r"^PR #"))
        expect(links).to_have_count(2)
        repo = self.gh.repo(self.OTHER_REPO)

        def pr(title: str):
            return max(
                (pr for pr in repo.prs.values() if pr.title == title),
                key=lambda pr: pr.number,
            )

        a, b = pr("Add stack-a"), pr("Add stack-b")
        self.assertEqual(a.base, "main")
        self.assertEqual(a.head, "dev/test/add-stack-a")
        self.assertEqual((b.base, b.head), (a.head, "dev/test/add-stack-b"))
        expect(links.nth(0)).to_have_attribute(
            "href", f"#/{self.OTHER_REPO}/{a.number}"
        )
        expect(links.nth(1)).to_have_attribute(
            "href", f"#/{self.OTHER_REPO}/{b.number}"
        )
        icons = opened.get_by_role("link", name="Open on GitHub")
        expect(icons.nth(0)).to_have_attribute("href", a.url)
        expect(icons.nth(1)).to_have_attribute("href", b.url)
        header = self.page.locator(".agent-header")
        badge = header.get_by_role("link", name=f"PRs #{a.number}–#{b.number}")
        expect(badge).to_have_attribute("href", f"#/{self.OTHER_REPO}/{b.number}")
        entry = (
            self.section(self.OTHER_REPO)
            .locator(".agent-entry")
            .filter(has=self.page.locator(".agent-item.selected"))
        )
        expect(entry.locator(".agent-prs")).to_have_text("0/2")
        # They make a GitHub stack, bottom first.
        stacks = {n: s for n, s in repo.stacks.items() if a.number in s}
        ((number, stack),) = stacks.items()
        self.assertEqual(stack, [a.number, b.number])

        # More work goes on top of the stack; the PRs below stay.
        follow_up.fill("commit stack-c")
        follow_up.press("Enter")
        expect(done).to_have_count(4)
        actions.get_by_role("button", name="Open PR").click()
        expect(
            opened.nth(1).get_by_role("link", name=re.compile(r"^PR #"))
        ).to_have_count(3)
        c = pr("Add stack-c")
        self.assertEqual((c.base, c.head), (b.head, "dev/test/add-stack-c"))
        self.assertEqual(
            sum(pr.head.startswith("dev/test/") for pr in repo.prs.values()), 3
        )
        expect(entry.locator(".agent-prs")).to_have_text("0/3")
        # The stack grows its PR on top.
        stack = next(s for s in repo.stacks.values() if a.number in s)
        self.assertEqual(stack, [a.number, b.number, c.number])
        self.assertIn(f"POST /repos/{repo.slug}/stacks/{number}/add", self.gh.requests)

    def test_update_prs_drops_and_reorders(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        prompt = self.page.get_by_label("Prompt")
        prompt.fill("commit stack-a")
        prompt.press("Enter")
        transcript = self.page.locator(".transcript")
        done = transcript.locator(".turn-end.muted")
        follow_up = self.page.get_by_label("Follow-up")
        expect(done).to_have_count(1)
        for i, name in enumerate(["stack-b", "stack-c"]):
            follow_up.fill(f"commit {name}")
            follow_up.press("Enter")
            expect(done).to_have_count(i + 2)

        # Each commit gets its own PR, into the branch of the one below.
        actions = self.page.locator(".agent-actions")
        actions.get_by_role("button", name="Open PR").click()
        opened = transcript.locator(".wrapped-up")
        expect(opened.get_by_role("link", name=re.compile(r"^PR #"))).to_have_count(3)
        repo = self.gh.repo(self.OTHER_REPO)

        def pr(title: str):
            return max(
                (pr for pr in repo.prs.values() if pr.title == title),
                key=lambda pr: pr.number,
            )

        a, b, c = pr("Add stack-a"), pr("Add stack-b"), pr("Add stack-c")
        self.assertEqual((b.base, b.head), (a.head, "dev/test/add-stack-b"))
        self.assertEqual((c.base, c.head), (b.head, "dev/test/add-stack-c"))

        def commits(pr) -> list[str]:
            """The subjects of `pr`'s commits, from the pushed refs."""
            out = self.gh.git(
                "-C",
                str(repo.bare),
                "rev-list",
                "--format=%s",
                "--no-commit-header",
                f"refs/heads/{pr.base}..refs/heads/{pr.head}",
            )
            return out.splitlines()

        # Dropping the middle commit closes its PR, deletes its branch
        # and retargets the PR above it.
        follow_up.fill("drop stack-b")
        follow_up.press("Enter")
        expect(done).to_have_count(5)
        follow_up.fill(f"update prs stack-a:{a.number} stack-c:{c.number}")
        follow_up.press("Enter")
        expect(done).to_have_count(6)
        expect(transcript).to_contain_text(f"Closed #{b.number}")
        self.assertEqual(b.state, "CLOSED")
        self.assertTrue(
            any(
                "dropped this pull request" in comment.body
                for comment in b.issue_comments
            )
        )
        gone = subprocess.run(
            [
                "git",
                "-C",
                str(repo.bare),
                "rev-parse",
                "--verify",
                f"refs/heads/{b.head}",
            ],
            env=self.gh.git_env(),
            check=False,
            capture_output=True,
        )
        self.assertNotEqual(gone.returncode, 0)
        self.assertEqual(c.base, a.head)
        self.assertEqual(commits(a), ["Add stack-a"])
        self.assertEqual(commits(c), ["Add stack-c"])
        stack = next(s for s in repo.stacks.values() if a.number in s)
        self.assertEqual(stack, [a.number, c.number])
        expect(transcript).to_contain_text("GitHub stack")

        # Reordering the commits retargets their PRs.
        follow_up.fill("swap")
        follow_up.press("Enter")
        expect(done).to_have_count(7)
        follow_up.fill(f"update prs stack-c:{c.number} stack-a:{a.number}")
        follow_up.press("Enter")
        expect(done).to_have_count(8)
        self.assertEqual((c.base, a.base), ("main", c.head))
        self.assertEqual(commits(c), ["Add stack-c"])
        self.assertEqual(commits(a), ["Add stack-a"])
        stack = next(s for s in repo.stacks.values() if c.number in s)
        self.assertEqual(stack, [c.number, a.number])

    def test_update_prs_with_stacks_off(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        prompt = self.page.get_by_label("Prompt")
        prompt.fill("commit stack-a")
        prompt.press("Enter")
        transcript = self.page.locator(".transcript")
        done = transcript.locator(".turn-end.muted")
        follow_up = self.page.get_by_label("Follow-up")
        expect(done).to_have_count(1)
        repo = self.gh.repo(self.OTHER_REPO)
        repo.stacks_enabled = False
        follow_up.fill("commit stack-b")
        follow_up.press("Enter")
        expect(done).to_have_count(2)

        # Without stacked PRs turned on, wrap-up still opens the PRs,
        # chained by their bases.
        self.page.locator(".agent-actions").get_by_role(
            "button", name="Open PR"
        ).click()
        opened = transcript.locator(".wrapped-up")
        expect(opened.get_by_role("link", name=re.compile(r"^PR #"))).to_have_count(2)
        a, b = (
            max(
                (pr for pr in repo.prs.values() if pr.title == title),
                key=lambda pr: pr.number,
            )
            for title in ["Add stack-a", "Add stack-b"]
        )
        self.assertEqual((a.base, b.base), ("main", a.head))
        self.assertFalse(any(a.number in s for s in repo.stacks.values()))

        follow_up.fill("commit stack-c")
        follow_up.press("Enter")
        expect(done).to_have_count(4)
        follow_up.fill(f"update prs stack-a:{a.number} stack-b:{b.number} stack-c")
        follow_up.press("Enter")
        expect(done).to_have_count(5)
        expect(transcript).to_contain_text(
            "doesn't have GitHub's stacked pull requests"
        )
        self.assertFalse(any(b.number in s for s in repo.stacks.values()))

    def test_update_prs_merged_prs_are_frozen(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        prompt = self.page.get_by_label("Prompt")
        prompt.fill("commit stack-a")
        prompt.press("Enter")
        transcript = self.page.locator(".transcript")
        done = transcript.locator(".turn-end.muted")
        follow_up = self.page.get_by_label("Follow-up")
        expect(done).to_have_count(1)
        follow_up.fill("commit stack-b")
        follow_up.press("Enter")
        expect(done).to_have_count(2)

        self.page.locator(".agent-actions").get_by_role(
            "button", name="Open PR"
        ).click()
        opened = transcript.locator(".wrapped-up")
        expect(opened.get_by_role("link", name=re.compile(r"^PR #"))).to_have_count(2)
        repo = self.gh.repo(self.OTHER_REPO)

        def pr(title: str):
            return max(
                (pr for pr in repo.prs.values() if pr.title == title),
                key=lambda pr: pr.number,
            )

        a, b = pr("Add stack-a"), pr("Add stack-b")

        # A merges while something else lands on main; the agent is asked
        # to rebase the remaining work onto the trunk's tip.
        repo.merge(a)
        repo.checkout("main")
        repo.commit("Other work", {"other.txt": "other"})
        repo.push("main")
        follow_up.fill(f"update prs stack-b:{b.number}")
        follow_up.press("Enter")
        expect(done).to_have_count(4)
        expect(transcript).to_contain_text(f"#{a.number} merged into main")
        expect(transcript).to_contain_text("git rebase --onto")
        self.assertEqual(b.state, "OPEN")
        self.assertEqual(b.base, a.head)

        follow_up.fill("rebase 1")
        follow_up.press("Enter")
        expect(done).to_have_count(5)
        follow_up.fill(f"update prs stack-b:{b.number}")
        follow_up.press("Enter")
        expect(done).to_have_count(6)
        # B is retargeted at the trunk; the merged A isn't touched.
        self.assertEqual(b.base, "main")
        out = self.gh.git(
            "-C",
            str(repo.bare),
            "rev-list",
            "--format=%s",
            "--no-commit-header",
            f"refs/heads/{b.base}..refs/heads/{b.head}",
        )
        self.assertEqual(out.splitlines(), ["Add stack-b"])
        self.assertEqual(a.state, "MERGED")
        self.assertEqual(a.issue_comments, [])

    def _foreign_pr(self, repo, name: str, base: str):
        """An open PR `name`, made and pushed by someone else on `base`,
        waited on until the server has synced it."""
        repo.checkout(name, base)
        repo.commit(name, {name + ".txt": name})
        repo.push(name)
        pr = repo.open_pr(f"Add {name}", head=name, base=base)
        for _ in range(40):
            try:
                out = json.loads(
                    urllib.request.urlopen(
                        self.url + f"api/pr/{self.OTHER_REPO}/{pr.number}"
                    ).read()
                )
            except urllib.error.HTTPError:
                out = {}
            if out.get("versions"):
                break
            time.sleep(0.25)
        # Leave the work tree on the default branch again.
        repo.checkout("main")
        return pr

    def _session_from_pr(self, number: int):
        """Starts a session from PR `number`; returns its page locators."""
        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Start from PR").fill(str(number))
        prompt = self.page.get_by_label("Prompt")
        prompt.fill("commit stack-a")
        prompt.press("Enter")
        transcript = self.page.locator(".transcript")
        done = transcript.locator(".turn-end.muted")
        expect(done).to_have_count(1)
        return transcript, done, self.page.get_by_label("Follow-up")

    def _allow(self, transcript) -> None:
        # Answered approvals are gone from the transcript; the pending
        # one takes the click.
        approval = transcript.locator(".approval.pending")
        expect(approval).to_contain_text("update_prs")
        approval.get_by_role("button", name="Allow").click()

    def test_update_prs_joins_a_foreign_stack(self) -> None:
        repo = self.gh.repo(self.OTHER_REPO)
        x = self._foreign_pr(repo, "x", "main")
        s = self._foreign_pr(repo, "s", x.head)
        stack_number = repo.gh.next_number()
        repo.stacks[stack_number] = [x.number, s.number]
        transcript, done, follow_up = self._session_from_pr(s.number)

        # The session's PR joins the foreign stack on top.
        follow_up.fill("update prs stack-a")
        follow_up.press("Enter")
        self._allow(transcript)
        expect(done).to_have_count(2)
        a = max(
            (pr for pr in repo.prs.values() if pr.title == "Add stack-a"),
            key=lambda pr: pr.number,
        )
        self.assertEqual(repo.stacks[stack_number], [x.number, s.number, a.number])

        # More work appends the same way.
        follow_up.fill("commit stack-b")
        follow_up.press("Enter")
        expect(done).to_have_count(3)
        follow_up.fill(f"update prs stack-a:{a.number} stack-b")
        follow_up.press("Enter")
        self._allow(transcript)
        expect(done).to_have_count(4)
        b = max(
            (pr for pr in repo.prs.values() if pr.title == "Add stack-b"),
            key=lambda pr: pr.number,
        )
        self.assertEqual(
            repo.stacks[stack_number], [x.number, s.number, a.number, b.number]
        )

        # Dropping one of ours rebuilds the stack; the foreign PRs are
        # untouched.
        follow_up.fill("drop stack-a")
        follow_up.press("Enter")
        expect(done).to_have_count(5)
        follow_up.fill(f"update prs stack-b:{b.number}")
        follow_up.press("Enter")
        self._allow(transcript)
        expect(done).to_have_count(6)
        stack = next(s for s in repo.stacks.values() if b.number in s)
        self.assertEqual(stack, [x.number, s.number, b.number])
        self.assertEqual((x.base, x.head, x.state), ("main", "x", "OPEN"))
        self.assertEqual((s.base, s.head, s.state), ("x", "s", "OPEN"))
        self.assertEqual(a.state, "CLOSED")

    def test_update_prs_source_pr_in_no_stack(self) -> None:
        repo = self.gh.repo(self.OTHER_REPO)
        s = self._foreign_pr(repo, "s", "main")
        transcript, done, follow_up = self._session_from_pr(s.number)

        follow_up.fill("commit stack-b")
        follow_up.press("Enter")
        expect(done).to_have_count(2)
        follow_up.fill("update prs stack-a stack-b")
        follow_up.press("Enter")
        self._allow(transcript)
        expect(done).to_have_count(3)
        a, b = (
            max(
                (pr for pr in repo.prs.values() if pr.title == title),
                key=lambda pr: pr.number,
            )
            for title in ["Add stack-a", "Add stack-b"]
        )
        self.assertEqual((a.base, b.base), (s.head, a.head))
        stack = next(s for s in repo.stacks.values() if a.number in s)
        self.assertEqual(stack, [s.number, a.number, b.number])

    def test_update_prs_source_pr_in_middle_of_a_stack(self) -> None:
        repo = self.gh.repo(self.OTHER_REPO)
        x = self._foreign_pr(repo, "x", "main")
        s = self._foreign_pr(repo, "s", x.head)
        y = self._foreign_pr(repo, "y", s.head)
        foreign = repo.gh.next_number()
        repo.stacks[foreign] = [x.number, s.number, y.number]
        transcript, done, follow_up = self._session_from_pr(s.number)

        # S can't take new PRs on top inside [X, S, Y]; ours get a stack
        # of their own.
        follow_up.fill("commit stack-b")
        follow_up.press("Enter")
        expect(done).to_have_count(2)
        follow_up.fill("update prs stack-a stack-b")
        follow_up.press("Enter")
        self._allow(transcript)
        expect(done).to_have_count(3)
        a, b = (
            max(
                (pr for pr in repo.prs.values() if pr.title == title),
                key=lambda pr: pr.number,
            )
            for title in ["Add stack-a", "Add stack-b"]
        )
        self.assertEqual(repo.stacks[foreign], [x.number, s.number, y.number])
        ours = next(s for s in repo.stacks.values() if a.number in s)
        self.assertEqual(ours, [a.number, b.number])

    def test_sidebar_shows_merged_prs(self) -> None:
        repo = self.gh.repo(self.OTHER_REPO)
        prs = []
        for name in ["count-a", "count-b"]:
            repo.checkout(name, "main")
            repo.commit(name, {f"{name}.txt": name})
            repo.push(name)
            prs.append(repo.open_pr(name, head=name))
        a, b = prs

        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill(f"open pr {a.url}")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page.locator(".transcript .turn-end")).to_have_count(1)
        entry = (
            self.section(self.OTHER_REPO)
            .locator(".agent-entry")
            .filter(has_text=f"open pr {a.url}")
        )
        count = entry.locator(".agent-prs")
        expect(count).to_have_text("0/1")
        expect(count).to_have_attribute("title", "0 of 1 PR merged")
        # The session changed no files.
        expect(entry.locator(".agent-unstaged")).to_have_count(0)

        follow_up = self.page.get_by_label("Follow-up")
        follow_up.fill(f"open pr {b.url}")
        follow_up.press("Enter")
        expect(count).to_have_text("0/2")

        # The count turns red while a PR conflicts with its base.
        conflict = re.compile(r"\bconflict\b")
        a.conflicting = True
        expect(count).to_have_class(conflict, timeout=15_000)
        expect(count).to_have_attribute("title", "0 of 2 PRs merged, 1 with conflicts")
        a.conflicting = False
        expect(count).not_to_have_class(conflict, timeout=15_000)
        expect(count).to_have_attribute("title", "0 of 2 PRs merged")

        # Failing checks turn it red too.
        a.check_runs.append(CheckRun("build", conclusion="FAILURE"))
        expect(count).to_have_class(conflict, timeout=15_000)
        expect(count).to_have_attribute(
            "title", "0 of 2 PRs merged, 1 with failing checks"
        )
        a.check_runs[0].conclusion = "SUCCESS"
        expect(count).not_to_have_class(conflict, timeout=15_000)
        expect(count).to_have_attribute("title", "0 of 2 PRs merged")

        # The next poll sees the merge.
        repo.merge(a)
        expect(count).to_have_text("1/2", timeout=15_000)
        expect(count).not_to_have_class(re.compile(r"\bdone\b"))
        # A PR closed without merging counts in neither number.
        repo.close(b)
        expect(count).to_have_text("1/1", timeout=15_000)
        expect(count).to_have_class(re.compile(r"\bdone\b"))

    def test_associated_prs(self) -> None:
        self.page.set_viewport_size({"width": 1800, "height": 900})
        repo = self.gh.repo(self.OTHER_REPO)
        prs = []
        for name in ["assoc-a", "assoc-b"]:
            repo.checkout(name, "main")
            repo.commit(name, {f"{name}.txt": name})
            repo.push(name)
            prs.append(repo.open_pr(f"title-{name[-1]}", head=name))
        a, b = prs

        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill(f"open pr {a.url}")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page.locator(".transcript .turn-end")).to_have_count(1)

        section = self.page.locator(".associated-prs")
        items = section.locator("li")
        icons = section.locator(".pr-state")
        # The next poll fills in the title.
        expect(items).to_have_text([f"#{a.number} title-a"], timeout=15_000)

        follow_up = self.page.get_by_label("Follow-up")
        follow_up.fill(f"open pr {b.url}")
        follow_up.press("Enter")
        expect(items).to_have_text(
            [f"#{a.number} title-a", f"#{b.number} title-b"], timeout=15_000
        )
        for i, pr in enumerate(prs):
            # Each PR links to its page in grenadine, with an icon that
            # opens it on GitHub.
            item = items.nth(i)
            expect(item.locator("a").first).to_have_attribute(
                "href", f"#/{self.OTHER_REPO}/{pr.number}"
            )
            expect(item.get_by_role("link", name="Open on GitHub")).to_have_attribute(
                "href", f"https://github.com/{self.OTHER_REPO}/pull/{pr.number}"
            )

        # Neither head has checks.
        expect(icons.nth(0)).to_have_class(re.compile(r"\bopen\b"), timeout=15_000)
        expect(icons.nth(0)).to_have_attribute("title", "No checks")
        a.check_runs.append(CheckRun("build", status="IN_PROGRESS", conclusion=None))
        expect(icons.nth(0)).to_have_class(re.compile(r"\brunning\b"), timeout=15_000)
        a.check_runs[0].status = "COMPLETED"
        a.check_runs[0].conclusion = "SUCCESS"
        expect(icons.nth(0)).to_have_class(re.compile(r"\bpassing\b"), timeout=15_000)
        a.check_runs[0].conclusion = "FAILURE"
        expect(icons.nth(0)).to_have_class(re.compile(r"\bfailing\b"), timeout=15_000)
        # A conflict outranks failing checks.
        a.conflicting = True
        expect(icons.nth(0)).to_have_class(re.compile(r"\bconflict\b"), timeout=15_000)
        expect(icons.nth(0)).to_have_attribute("title", "Conflicts with its base")
        repo.merge(a)
        expect(icons.nth(0)).to_have_class(re.compile(r"\bmerged\b"), timeout=15_000)
        repo.close(b)
        expect(icons.nth(1)).to_have_class(re.compile(r"\bclosed\b"), timeout=15_000)

        # A plain click opens a PR link in grenadine; the icon opens
        # GitHub in a new tab.
        session = self.page.url
        # GitHub itself is stubbed so that a click that slips through hits
        # nothing real.
        self.page.context.route(
            "https://github.com/**",
            lambda route: route.fulfill(body="<p>github</p>"),
        )
        items.nth(0).locator("a").first.click()
        expect(self.page).to_have_url(re.compile(f"#/{self.OTHER_REPO}/{a.number}$"))
        self.page.goto(session)
        expect(items).to_have_count(2)
        with self.page.expect_popup() as popup:
            items.nth(0).get_by_role("link", name="Open on GitHub").click()
        popup.value.wait_for_load_state()
        expect(self.page).to_have_url(session)

        # The session goes too, so later tests' counts aren't off.
        entry = self.section(self.OTHER_REPO).locator(".agent-entry")
        self.page.once("dialog", lambda dialog: dialog.accept())
        entry.get_by_role("button", name="Delete session").click()
        expect(entry).to_have_count(0)

    def test_associated_prs_include_the_start_pr(self) -> None:
        # The associated PRs are on the changes pane, which only a wide
        # window shows.
        self.page.set_viewport_size({"width": 1800, "height": 900})
        repo = self.gh.repo(self.OTHER_REPO)
        pr = self._foreign_pr(repo, "start", "main")
        self._session_from_pr(pr.number)

        items = self.page.locator(".associated-prs li")
        # The number is there right away; a poll fills in the title.
        expect(items).to_contain_text(f"#{pr.number}")
        expect(items).to_have_text([f"#{pr.number} {pr.title}"], timeout=15_000)

        # The session goes too, so later tests' counts aren't off.
        entry = self.section(self.OTHER_REPO).locator(".agent-entry")
        self.page.once("dialog", lambda dialog: dialog.accept())
        entry.get_by_role("button", name="Delete session").click()
        expect(entry).to_have_count(0)

    def test_transcript_pr_links_open_in_grenadine(self) -> None:
        self.page.context.route(
            "https://github.com/**",
            lambda route: route.fulfill(body="<p>github</p>"),
        )
        self.page.goto(self.url + f"#/agents/new/{self.OTHER_REPO}")
        url = f"https://github.com/{self.OTHER_REPO}/pull/1"
        self.page.get_by_label("Prompt").fill(f"look at {url}")
        self.page.get_by_role("button", name="Start").click()

        # The echoed prompt's PR URL links to the PR in grenadine, with an
        # icon that opens it on GitHub.
        link = self.page.locator(".transcript .turn-text a").filter(has_text=url)
        expect(link).to_have_attribute("href", f"#/{self.OTHER_REPO}/1")
        icon = self.page.locator(".transcript .turn-text").get_by_role(
            "link", name="Open on GitHub"
        )
        expect(icon).to_have_attribute("href", url)

        # A plain click opens the PR in grenadine; the icon opens GitHub
        # in a new tab.
        session = self.page.url
        link.click()
        expect(self.page).to_have_url(re.compile(f"#/{self.OTHER_REPO}/1$"))
        self.page.goto(session)
        with self.page.expect_popup() as popup:
            icon.click()
        popup.value.wait_for_load_state()
        expect(self.page).to_have_url(session)

        # The session goes too, so later tests' counts aren't off.
        entry = self.section(self.OTHER_REPO).locator(".agent-entry")
        self.page.once("dialog", lambda dialog: dialog.accept())
        entry.get_by_role("button", name="Delete session").click()
        expect(entry).to_have_count(0)

    def test_plan_mode(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        expect(self.page.get_by_label("Permissions")).to_have_value("plan")
        self.page.get_by_label("Prompt").fill("make a plan")
        self.page.get_by_role("button", name="Start").click()
        transcript = self.page.locator(".transcript")

        # Claude asks a question first; it takes an answer to every question.
        question = transcript.locator(".approval.questions")
        expect(question).to_contain_text("Claude asks")
        expect(question.locator("legend")).to_have_text("File Which file?")
        answer = question.get_by_role("button", name="Answer")
        expect(answer).to_be_disabled()
        question.get_by_label("todo.txt").check()
        answer.click()
        expect(question).to_contain_text("Answered")
        expect(question.locator(".question-answer")).to_have_text("todo.txt")

        # A plan sent back comes again with the feedback in it.
        plans = transcript.locator(".approval.plan")
        expect(plans.locator(".plan-text strong")).to_have_text("todo.txt")
        # The feedback box leaves its focus ring room above the buttons.
        # Measured in one go so that both rects come from the same
        # layout; the card can still move between two round trips.
        gap = plans.evaluate(
            """(plans) => {
                const box = plans.querySelector('textarea.plan-feedback')
                    .getBoundingClientRect();
                const buttons = plans.querySelector('.form-buttons')
                    .getBoundingClientRect();
                return buttons.y - (box.y + box.height);
            }"""
        )
        self.assertGreaterEqual(gap, 6)
        plans.get_by_label("Feedback").fill("Add a title")
        plans.get_by_role("button", name="Keep planning").click()
        expect(plans).to_have_count(2)
        expect(plans.first).to_contain_text("Plan sent back")
        # The feedback stays in the transcript, between the two plans.
        feedback = transcript.locator(".approval.plan ~ .turn-prompt")
        expect(feedback).to_have_text("Add a title")
        expect(transcript.locator(".turn-prompt ~ .approval.plan")).to_have_count(2)
        expect(
            transcript.locator(".approval.plan ~ .turn-prompt ~ .approval.plan")
        ).to_have_count(1)
        revised = plans.nth(1)
        expect(revised.locator(".plan-text")).to_contain_text("Also: Add a title")

        # Approving it switches the turn and the follow-ups to the mode picked.
        after = revised.get_by_label("After the plan")
        expect(after).to_have_value("auto")
        after.select_option(label="Accept edits")
        revised.get_by_role("button", name="Approve plan").click()
        expect(revised).to_contain_text("Plan approved")
        expect(transcript.locator(".turn-end")).to_have_text(["Done · $0.01"])
        worktree = Path(self.page.locator(".agent-worktree").inner_text())
        self.assertEqual((worktree / "todo.txt").read_text(), "acceptEdits")
        expect(self.page.get_by_label("Permissions")).to_have_value("acceptEdits")

    def test_question_with_a_markdown_body(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Ask for everything")
        self.page.get_by_label("Prompt").fill("question the plan")
        self.page.get_by_role("button", name="Start").click()
        transcript = self.page.locator(".transcript")

        # The legend holds the badge and the title; the rest of the
        # question renders as Markdown under it.
        question = transcript.locator(".approval.questions")
        expect(question).to_contain_text("Claude asks")
        expect(question.locator("legend")).to_have_text(
            "Implement Implement this plan?"
        )
        body = question.locator(".question-body")
        expect(body.locator("h2")).to_have_text("Steps")
        expect(body.locator("strong")).to_have_text("one")

        question.get_by_label("Go ahead").check()
        question.get_by_role("button", name="Answer").click()
        expect(question).to_contain_text("Answered")
        # Answered, the card keeps the title and the Markdown body.
        expect(question.locator("p.muted")).to_have_text("Implement this plan?")
        expect(body.locator("h2")).to_have_text("Steps")
        expect(body.locator("strong")).to_have_text("one")
        expect(question.locator(".question-answer")).to_have_text("Go ahead")
        # The answer went back keyed by the full question: the fake
        # backend reads `answers[question]` and finishes the turn.
        expect(transcript.locator(".turn-end")).to_have_text(["Done · $0.01"])
        # Reloaded, the stored events still hold the answer.
        self.page.reload()
        expect(transcript.locator(".question-answer")).to_have_text("Go ahead")

    def test_approved_mode_survives_leaving_the_session(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Prompt").fill("make a plan")
        self.page.get_by_role("button", name="Start").click()
        transcript = self.page.locator(".transcript")
        question = transcript.locator(".approval.questions")
        question.get_by_label("todo.txt").check()
        question.get_by_role("button", name="Answer").click()
        plan = transcript.locator(".approval.plan")
        plan.get_by_label("After the plan").select_option(label="Accept edits")
        plan.get_by_role("button", name="Approve plan").click()
        expect(transcript.locator(".turn-end")).to_have_text(["Done · $0.01"])
        permissions = self.page.get_by_label("Permissions")
        expect(permissions).to_have_value("acceptEdits")
        session = self.page.url

        # The form for a new session starts out planning, but that doesn't
        # change what the session's follow-ups go with.
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        expect(permissions).to_have_value("plan")
        self.page.goto(session)
        expect(permissions).to_have_value("acceptEdits")
        self.page.reload()
        expect(permissions).to_have_value("acceptEdits")

    def test_plan_stranded_by_a_restart(self) -> None:
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Prompt").fill("make a plan")
        self.page.get_by_role("button", name="Start").click()
        transcript = self.page.locator(".transcript")
        question = transcript.locator(".approval.questions")
        question.get_by_label("todo.txt").check()
        question.get_by_role("button", name="Answer").click()
        plan = transcript.locator(".approval.plan")
        expect(plan).to_contain_text("Claude's plan is waiting for your approval")

        # The restart ends the turn that asked, but the plan can still be
        # answered, with a new turn.
        self.stop_server()
        self.start_server()
        self.page.reload()
        expect(plan).to_contain_text("The turn ended before the plan was answered")
        plan.get_by_label("After the plan").select_option(label="Accept edits")
        # What's in the box goes along with the approval.
        plan.get_by_label("Feedback").fill("Use tabs.")
        plan.get_by_role("button", name="Approve plan").click()
        expect(transcript.locator(".turn-prompt").last).to_have_text(
            "I approve the plan. Carry it out, keeping the following in mind:\n\nUse tabs."
        )
        expect(transcript.locator(".turn-end")).to_have_text(["Done · $0.01"])
        expect(self.page.get_by_label("Permissions")).to_have_value("acceptEdits")
        # Once answered, the old plan offers nothing more.
        expect(plan).to_contain_text("Unanswered plan")
        expect(plan.get_by_role("button", name="Approve plan")).to_have_count(0)

    def test_tab_counts_waiting_sessions(self) -> None:
        tab = self.page.locator(".topbar-tabs a").filter(has_text="Agents")
        count = tab.locator(".count")
        # Sessions left waiting by earlier tests may already count.
        with urllib.request.urlopen(f"{self.url}api/agents") as resp:
            base = sum(
                1
                for s in json.load(resp)
                if s["kind"] == "interactive"
                and s["status"]
                in {"idle", "awaiting_approval", "interrupted", "failed"}
            )
        self.page.goto(self.url + "#/agents")
        expect_tab_count(count, base)
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("sleep 5")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))

        # A session still working doesn't count.
        entry = self.section("General").locator(".agent-entry")
        status = entry.locator(".agent-status")
        expect(status).to_have_text("working…")
        expect_tab_count(count, base)
        # Idle once its turn ends, it does — without joining the title's
        # inbox total.
        expect(status).to_have_text("idle", timeout=30_000)
        expect(count).to_have_text(str(base + 1))
        expect(self.page).to_have_title("grenadine")

        # Deleting it clears the count.
        self.delete_session()
        expect_tab_count(count, base)


class AgentMergeTest(ServerTestCase):
    """A session's PR list follows merges through grenadine."""

    # The session must learn of the merge between polls, so the poll
    # interval stays at its default.
    POLL_INTERVAL: ClassVar[int] = 60

    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        repo = gh.repo(scenarios.REPO)
        repo.commit("Seed", {"README": "hello\n"})
        repo.push("main")

    def test_merge_updates_the_session_between_polls(self) -> None:
        # The associated PRs are on the changes pane, which only a wide
        # window shows.
        self.page.set_viewport_size({"width": 1800, "height": 900})
        repo = self.gh.repo(scenarios.REPO)
        repo.checkout("assoc", "main")
        repo.commit("assoc", {"assoc.txt": "assoc"})
        repo.push("assoc")
        pr = repo.open_pr("title-assoc", head="assoc")

        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill(f"open pr {pr.url}")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page.locator(".transcript .turn-end")).to_have_count(1)

        section = self.page.locator(".associated-prs")
        # A poll fills in the title; the number is there right away.
        expect(section.locator("li")).to_contain_text(f"#{pr.number}")
        icon = section.locator(".pr-state")
        expect(icon).to_have_class(re.compile(r"\bopen\b"))
        session = self.page.url

        self.page.goto(f"{self.url}#/{scenarios.REPO}/{pr.number}")
        expect(self.page.locator(".pr-header h1")).to_have_text(
            f"{pr.title} #{pr.number}"
        )
        self.merge()
        expect(self.page.locator(".pr-header .state")).to_have_text("merged")

        # The merge resyncs the PR, which reaches the session's own copy
        # of its state without waiting for the next poll.
        self.page.goto(session)
        expect(icon).to_have_class(re.compile(r"\bmerged\b"), timeout=10_000)
        expect(self.page.locator(".agent-prs")).to_have_text("1/1", timeout=10_000)

        session_id = session.split("/")[-1]
        self.page.once("dialog", lambda dialog: dialog.accept())
        self.page.get_by_role("button", name="Delete", exact=True).click()
        expect(
            self.page.locator(f'.agent-item[href="#/agents/{session_id}"]')
        ).to_have_count(0)


class AutoReviewTest(ServerTestCase):
    QUIET_REPO = "test/quiet"
    OFF_REPO = "test/off"
    DEVIN_REPO = "test/devin"
    REPOS: ClassVar[list[str]] = [scenarios.REPO, QUIET_REPO, OFF_REPO, DEVIN_REPO]
    POLL_INTERVAL: ClassVar[int] = 1

    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        # The PRs the tests open come after the server starts reviewing.
        gh.clock = int(time.time()) + 3600
        for slug in cls.REPOS:
            repo = gh.repo(slug)
            repo.commit("Seed", {"README": "hello\n"})
            repo.push("main")

    def open_pr(self, slug: str, title: str) -> Any:
        repo = self.gh.repo(slug)
        branch = re.sub(r"\W+", "-", title).lower()
        repo.checkout(branch, "main")
        repo.commit(title, {f"{branch}.txt": title})
        repo.push(branch)
        return repo.open_pr(title, head=branch)

    def review_entry(self, title: str) -> Locator:
        return self.page.locator(".agent-entry").filter(has_text=title)

    def settings(self, slug: str) -> Locator:
        self.page.goto(self.url + "#/agents")
        self.page.get_by_role("button", name=f"Settings for {slug}").click()
        return self.page.get_by_role("dialog", name=f"{slug} settings")

    def test_a_posted_review_deletes_its_session(self) -> None:
        pr = self.open_pr(scenarios.REPO, "Add the feature")
        self.page.goto(self.url + "#/auto-reviews")
        entry = self.review_entry("Add the feature")
        expect(entry.locator(".agent-status")).to_have_text("posted", timeout=30_000)
        self.assertEqual([c.body for c in pr.issue_comments], ["Looks good."])
        # Its session is gone, and the entry links to the PR instead,
        # with an icon that opens it on GitHub.
        expect(entry.locator("a.agent-item")).to_have_attribute(
            "href", f"#/{scenarios.REPO}/{pr.number}"
        )
        expect(entry.get_by_role("link", name="Open on GitHub")).to_have_attribute(
            "href", pr.url
        )
        self.page.get_by_role("link", name="Agents").click()
        expect(self.page.locator(".agent-sidebar")).not_to_contain_text("Review #")

    def test_a_review_that_posts_nothing_is_kept(self) -> None:
        dialog = self.settings(self.QUIET_REPO)
        expect(dialog.get_by_label("Review my pull requests")).to_have_value(
            "non_draft"
        )
        expect(dialog.get_by_label("Review with")).to_have_value("claude")
        expect(dialog.get_by_label("Review model")).to_have_value("default")
        dialog.get_by_label("Review model").select_option(label="Sonnet")
        dialog.get_by_label("Review skill").fill("/code-review")
        dialog.get_by_label("Review again after new pushes").check()
        dialog.get_by_role("button", name="Save").click()
        expect(dialog).to_have_count(0)
        # The server keeps the name without its slash.
        dialog = self.settings(self.QUIET_REPO)
        expect(dialog.get_by_label("Review skill")).to_have_value("code-review")
        expect(dialog.get_by_label("Review again after new pushes")).to_be_checked()
        expect(dialog.get_by_label("Review model")).to_have_value("sonnet")
        # Switching the backend shows that backend's models and default;
        # Escape leaves the saved settings alone.
        dialog.get_by_label("Review with").select_option(label="Devin")
        model = dialog.get_by_label("Review model")
        expect(model.locator("option")).to_have_text(["Default", "SWE-2", "SWE-2 High"])
        expect(model).to_have_value("default")
        self.page.keyboard.press("Escape")

        pr = self.open_pr(self.QUIET_REPO, "Quiet change")
        self.page.get_by_role("link", name="Auto-reviews").click()
        entry = self.review_entry("Quiet change")
        # The server looks for the review on GitHub a few times.
        expect(entry.locator(".agent-status")).to_have_text(
            "didn't post", timeout=60_000
        )
        self.assertEqual(pr.issue_comments, [])
        # It's not among the Agents tab's sessions.
        self.page.get_by_role("link", name="Agents").click()
        expect(self.page.locator(".agent-sidebar")).not_to_contain_text("Review #")
        self.page.get_by_role("link", name="Auto-reviews").click()

        # Its page shows the prompt, with the skill, and takes none.
        entry.locator("a.agent-item").click()
        expect(self.page).to_have_url(re.compile(r"#/auto-reviews/[0-9a-f-]{36}$"))
        transcript = self.page.locator(".transcript")
        expect(transcript).to_contain_text(f"/code-review Review pull request {pr.url}")
        expect(transcript).to_contain_text("Quiet change")
        expect(self.page.get_by_label("Follow-up")).to_have_count(0)
        expect(self.page.locator(".agent-header h1")).to_have_text(
            f"Review #{pr.number}: Quiet change"
        )

        # A push gets another review.
        repo = self.gh.repo(self.QUIET_REPO)
        repo.commit("More", {"more.txt": "more"})
        repo.push(pr.head)
        expect(entry).to_have_count(2, timeout=30_000)

        self.page.once("dialog", lambda dialog: dialog.accept())
        self.page.get_by_role("button", name="Delete", exact=True).click()
        expect(self.page).to_have_url(re.compile(r"#/auto-reviews$"))

    def test_a_devin_review(self) -> None:
        dialog = self.settings(self.DEVIN_REPO)
        dialog.get_by_label("Review with").select_option(label="Devin")
        dialog.get_by_label("Review model").select_option(label="SWE-2 High")
        dialog.get_by_role("button", name="Save").click()
        expect(dialog).to_have_count(0)
        # Reopened, the picks persisted.
        dialog = self.settings(self.DEVIN_REPO)
        expect(dialog.get_by_label("Review with")).to_have_value("devin")
        expect(dialog.get_by_label("Review model")).to_have_value("swe-2-high")
        self.page.keyboard.press("Escape")

        pr = self.open_pr(self.DEVIN_REPO, "Devin change")
        self.page.get_by_role("link", name="Auto-reviews").click()
        entry = self.review_entry("Devin change")
        # The fake devin can't post to GitHub, so nothing is posted.
        expect(entry.locator(".agent-status")).to_have_text(
            "didn't post", timeout=60_000
        )
        self.assertEqual(pr.issue_comments, [])
        self.devin_saw("Review pull request")
        # The review ran on the picked model.
        self.devin_saw('"method":"session/set_config_option"')
        self.devin_saw('"value":"swe-2-high"')

        # Its page shows the review's prompt and its cost in ACUs.
        entry.locator("a.agent-item").click()
        transcript = self.page.locator(".transcript")
        expect(transcript).to_contain_text(f"Review pull request {pr.url}")
        expect(transcript.locator(".turn-end")).to_have_text(["Done · 0.050 ACU"])

    def test_reviews_can_be_turned_off(self) -> None:
        dialog = self.settings(self.OFF_REPO)
        dialog.get_by_label("Review my pull requests").select_option(label="Never")
        expect(dialog.get_by_label("Review with")).to_be_disabled()
        expect(dialog.get_by_label("Review model")).to_be_disabled()
        expect(dialog.get_by_label("Review skill")).to_be_disabled()
        dialog.get_by_role("button", name="Save").click()
        expect(dialog).to_have_count(0)
        self.open_pr(self.OFF_REPO, "Unreviewed change")
        # A review that was due starts within a poll or two.
        time.sleep(4)
        self.page.goto(self.url + "#/auto-reviews")
        expect(self.page.locator(".agent-sidebar")).not_to_contain_text(
            "Unreviewed change"
        )

    def test_tab_counts_kept_reviews(self) -> None:
        tab = self.page.locator(".topbar-tabs a").filter(has_text="Auto-reviews")
        count = tab.locator(".count")
        # Reviews left over by earlier tests may already count.
        with urllib.request.urlopen(f"{self.url}api/auto-reviews") as resp:
            base = sum(
                1
                for r in json.load(resp)
                if r["state"] in {"unposted", "failed"} and r["session"] is not None
            )
        self.page.goto(self.url + "#/auto-reviews")
        expect_tab_count(count, base)

        # A review whose session posts and is deleted doesn't count.
        self.open_pr(scenarios.REPO, "Counted posted")
        posted = self.review_entry("Counted posted")
        expect(posted.locator(".agent-status")).to_have_text("posted", timeout=60_000)
        expect_tab_count(count, base)

        # One that ends without posting keeps its session, and counts.
        self.open_pr(self.QUIET_REPO, "Quiet recount")
        unposted = self.review_entry("Quiet recount")
        expect(unposted.locator(".agent-status")).to_have_text(
            "didn't post", timeout=90_000
        )
        expect(count).to_have_text(str(base + 1))

        # Deleting the session clears it.
        unposted.locator("a.agent-item").click()
        self.page.once("dialog", lambda dialog: dialog.accept())
        self.page.get_by_role("button", name="Delete", exact=True).click()
        expect(self.page).to_have_url(re.compile(r"#/auto-reviews$"))
        expect_tab_count(count, base)


class CleanupTest(ServerTestCase):
    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        repo = gh.repo(scenarios.REPO)
        repo.commit("Seed", {"README": "hello\n"})
        repo.push("main")

    def leave_behind(self) -> None:
        """Starts a session and deletes it, which leaves one item behind."""
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("run the e2e tests")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page.locator(".transcript .turn-end")).to_have_count(1)
        self.page.once("dialog", lambda dialog: dialog.accept())
        self.page.locator(".agent-actions").get_by_role("button", name="Delete").click()
        expect(self.page).to_have_url(re.compile(DRAFT_URL))

    def test_deleted_sessions_leave_cleanup_items(self) -> None:
        # The section is there before anything is left behind.
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        section = self.page.locator(".agent-cleanup .agent-item")
        expect(section).to_contain_text("Nothing to clean up")

        self.leave_behind()

        # Scanning the deleted session finds what it left behind.
        expect(section).to_contain_text("1 item to clean up")
        section.click()
        expect(self.page).to_have_url(re.compile(r"#/agents/cleanup$"))
        expect(section).to_have_class(re.compile(r"\bselected\b"))
        item = self.page.locator(".cleanup-item")
        expect(item.locator(".cleanup-title")).to_have_text(["Container e2e"])
        expect(item.locator(".cleanup-meta")).to_contain_text(
            f"From “run the e2e tests” in {scenarios.REPO}"
        )
        expect(item.locator(".cleanup-detail code")).to_have_text("docker rm -f e2e")

        # Picked items go to a new session that plans how to clean them up.
        hand_off = self.page.get_by_role("button", name="Pick items to clean up")
        expect(hand_off).to_be_disabled()
        self.page.get_by_label("Container e2e").check()
        # The session runs in no repository, whichever one the item is from.
        expect(self.page.get_by_label("Repository")).to_have_value("general")
        self.page.get_by_role("button", name="Clean up 1 item in a new session").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
        expect(self.page.locator(".agent-header")).to_contain_text("in no repository")
        prompt = self.page.locator(".transcript .turn-prompt")
        expect(prompt).to_contain_text(
            "Clean up what deleted agent sessions left behind"
        )
        expect(prompt).to_contain_text("Container e2e")
        expect(self.page.get_by_label("Permissions")).to_have_value("plan")
        session = self.page.url

        # The item stays, linking to that session, until it's deleted.
        section.click()
        handed_off = item.get_by_role("link", name="handed off")
        expect(handed_off).to_have_attribute("href", "#" + session.split("#", 1)[1])
        item.get_by_role("button", name="Delete").click()
        expect(item).to_have_count(0)
        expect(self.page.locator(".cleanup")).to_contain_text("Nothing to clean up.")
        expect(section).to_contain_text("Nothing to clean up")

    def test_select_all_picks_every_item(self) -> None:
        self.leave_behind()
        self.leave_behind()
        section = self.page.locator(".agent-cleanup .agent-item")
        expect(section).to_contain_text("2 items to clean up")
        section.click()
        items = self.page.locator(".cleanup-item input[type=checkbox]")
        expect(items).to_have_count(2)

        select_all = self.page.get_by_label("Select all")
        select_all.check()
        expect(items.nth(0)).to_be_checked()
        expect(items.nth(1)).to_be_checked()
        expect(
            self.page.get_by_role("button", name="Clean up 2 items in a new session")
        ).to_be_enabled()

        # Picking only some leaves it indeterminate, and checking it then
        # picks the rest.
        items.nth(0).uncheck()
        expect(select_all).not_to_be_checked()
        expect(select_all).to_have_js_property("indeterminate", True)
        expect(
            self.page.get_by_role("button", name="Clean up 1 item in a new session")
        ).to_be_enabled()
        select_all.check()
        expect(items.nth(0)).to_be_checked()
        expect(select_all).to_have_js_property("indeterminate", False)

        select_all.uncheck()
        expect(items.nth(0)).not_to_be_checked()
        expect(items.nth(1)).not_to_be_checked()
        expect(
            self.page.get_by_role("button", name="Pick items to clean up")
        ).to_be_disabled()

        # It goes away with the last item.
        for n in (1, 0):
            self.page.locator(".cleanup-item").first.get_by_role(
                "button", name="Delete"
            ).click()
            expect(items).to_have_count(n)
        expect(select_all).to_have_count(0)


if __name__ == "__main__":
    sys.exit(pytest_bazel.main([__file__, *sys.argv[1:]]))
