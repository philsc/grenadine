"""Browser tests for grenadine's web UI.

Each test class starts the real server against a fake GitHub (see
fake_github.py) that it loads with PRs first. A stub `gh` hands the server
the fake's token, and a fake `claude` plays the agent.
"""

import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from typing import ClassVar

from playwright.sync_api import Locator, expect, sync_playwright

from grenadine.testing import scenarios
from grenadine.testing.fake_github import TOKEN, FakeGitHub

# How long the server may take to start listening.
STARTUP_TIMEOUT = 30

# Stands in for `claude -p`: it writes a short turn as stream-json and,
# when the prompt asks it to write, asks grenadine for permission through
# the MCP tool first, like Claude Code does. Permission modes other than
# manual allow the write without asking. It runs the model `--model` names,
# lists its models when asked like the Agent SDK does, summarizes a new
# session's request as "Task: " and its first line, and answers the scans
# of deleted sessions with a container they left behind.
FAKE_CLAUDE = r"""
import json
import subprocess
import sys
import time
import urllib.request
import uuid

args = sys.argv[1:]
if "--input-format" in args:
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
    else:
        item = {"title": "Container e2e", "detail": "Remove it with `docker rm -f e2e`."}
        output = {"items": [item]}
    print(json.dumps({"type": "result", "is_error": False, "structured_output": output}), flush=True)
    sys.exit()
prompt = sys.stdin.read()
url = json.loads(args[args.index("--mcp-config") + 1])["mcpServers"]["grenadine"]["url"]
mode = args[args.index("--permission-mode") + 1]
model = args[args.index("--model") + 1] if "--model" in args else "claude-test"


def emit(message):
    message.setdefault("parent_tool_use_id", None)
    print(json.dumps(message), flush=True)


def rpc(method, params, id=1):
    request = urllib.request.Request(
        url,
        data=json.dumps({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(request) as response:
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
emit({"type": "result", "subtype": "success", "is_error": False, "result": "", "total_cost_usd": 0.01, "modelUsage": {"claude-test": {"contextWindow": 200000}}})
"""


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def exactly(text: str) -> re.Pattern[str]:
    return re.compile(f"^{re.escape(text)}$")


class ServerTestCase(unittest.TestCase):
    """Runs the server and a browser for the class's tests."""

    # The repositories the server is started with; `load` creates them.
    REPOS: ClassVar[list[str]] = [scenarios.REPO]

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

        env = dict(cls.gh.git_env(), PATH=f"{bin_dir}:{os.environ['PATH']}")
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
        cls.server = subprocess.Popen(
            [
                os.environ["GRENADINE_SERVER"],
                *repo_flags,
                f"--port={port}",
                f"--db={tmp / 'grenadine.db'}",
                f"--github-api={api}",
            ],
            env=env,
        )
        cls.addClassCleanup(cls.stop_server)
        cls.wait_for_server()

        cls.playwright = sync_playwright().start()
        cls.addClassCleanup(cls.playwright.stop)
        cls.browser = cls.playwright.chromium.launch(
            executable_path=os.environ["CHROME"]
        )
        cls.addClassCleanup(cls.browser.close)

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

    def setUp(self) -> None:
        # Each page gets its own context, so local storage starts empty.
        self.page = self.browser.new_page()
        self.addCleanup(self.page.close)


class ChromeTest(ServerTestCase):
    def test_shows_wordmark_top_left(self) -> None:
        self.page.goto(self.url)
        brand = self.page.locator(".topbar-brand")
        expect(brand).to_have_text("grenadine")

        box = brand.bounding_box()
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
        self.page.get_by_label("Theme").select_option("dark")

        self.page.reload()
        expect(self.page.get_by_label("Theme")).to_have_value("dark")
        expect(self.page.locator("html")).to_have_attribute("data-theme", "dark")

    def test_inboxes_start_empty(self) -> None:
        self.page.goto(self.url)
        inboxes = self.page.locator("section.inbox")
        expect(inboxes).to_have_count(len(scenarios.default_inboxes()))
        expect(inboxes.locator(".none")).to_have_count(len(scenarios.default_inboxes()))
        expect(inboxes.locator(".count")).to_have_text(
            ["0"] * len(scenarios.default_inboxes())
        )


class InboxTest(ServerTestCase):
    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        cls.cases = scenarios.inbox_cases(gh)

    def inbox(self, name: str) -> Locator:
        return self.page.locator("section.inbox").filter(
            has=self.page.locator(".inbox-name", has_text=exactly(name))
        )

    def row(self, title: str) -> Locator:
        return self.page.locator(".pr-list li").filter(
            has=self.page.locator(
                ".pr-title", has_text=re.compile(f"{re.escape(title)}$")
            )
        )

    def expect_titles(self, inbox: Locator, titles: list[str]) -> None:
        """Checks that `inbox` lists exactly `titles`, in any order, each
        synced without errors."""
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
            expect(row.locator(".pr-meta")).to_have_text(re.compile(r"· 1 versions$"))

    def test_prs_land_in_their_inboxes(self) -> None:
        self.page.goto(self.url)
        for name, _ in scenarios.default_inboxes():
            with self.subTest(inbox=name):
                self.expect_titles(self.inbox(name), self.cases.titles_in(name))

    def test_prs_in_no_inbox_are_not_shown(self) -> None:
        self.page.goto(self.url)
        # Wait for the first poll.
        expect(
            self.inbox(scenarios.NEEDS_REVIEW).locator(".pr-list li")
        ).not_to_have_count(0)
        for title, inbox in self.cases.expected.items():
            if inbox is None:
                expect(self.row(title)).to_have_count(0)

    def test_badges(self) -> None:
        self.page.goto(self.url)
        expect(self.inbox(scenarios.DRAFTS).locator(".pr-title .badge")).to_have_text(
            "draft"
        )
        expect(
            self.inbox(scenarios.WAITING_FOR_REVIEWERS).locator(".pr-title .badge")
        ).to_have_count(2)
        expect(self.row(scenarios.STACK_BASE).locator(".badge")).to_have_text("1/2")
        expect(self.row(scenarios.STACK_TOP).locator(".badge")).to_have_text("2/2")
        expect(self.row("Wait for reviewers").locator(".badge")).to_have_count(0)

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

    def add_inbox(self, name: str, filter: str) -> Locator:
        self.addCleanup(self.delete_inbox, name)
        self.page.get_by_role("button", name="+ Add inbox").click()
        form = self.page.locator("form.inbox-form")
        form.get_by_label("Name").fill(name)
        form.get_by_label("GitHub filter").fill(filter)
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
            req = urllib.request.Request(f"{self.url}api/inboxes/{id}", method="DELETE")
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

    def test_pr_page(self) -> None:
        pr = self.cases.prs["Get approved"]
        self.page.goto(self.url)
        self.row(pr.title).locator("a").click()
        expect(self.page).to_have_url(re.compile(f"#/{scenarios.REPO}/{pr.number}$"))

        header = self.page.locator(".pr-header")
        expect(header.locator("h1")).to_have_text(f"{pr.title} #{pr.number}")
        expect(header.locator(".pr-number")).to_have_attribute("href", pr.url)
        expect(header.locator(".state")).to_have_text("open")
        expect(header.locator(".badge")).to_have_count(0)

        description = self.page.locator("section.description")
        expect(description.locator("h2")).to_have_text("Why")
        expect(description.locator("li")).to_have_count(2)
        expect(description.locator("li code")).to_have_text("inline code")
        expect(description.locator("pre")).to_contain_text("fn main() {}")

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
        # The PR itself is bold; the others link to GitHub.
        expect(rows.locator("strong")).to_have_text(f"#{top.number} {top.title}")
        expect(rows.locator("a")).to_have_attribute("href", base.url)


class VersionsTest(ServerTestCase):
    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        cls.cases = scenarios.versions_cases(gh)

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

    def test_rebase_only_changes_are_hidden(self) -> None:
        self.open(scenarios.REBASE)
        self.choose(1, 2)
        expect(self.page.locator(".diff-summary")).to_contain_text(
            "1 files changed · 2 files changed only by the rebase are hidden"
        )
        expect(self.page.locator(".diff-summary .legend")).to_be_visible()
        expect(self.page.locator(".file-index li")).to_have_text(["rebase/shared.py"])

        # The one file left has only rebase changes, so it starts collapsed.
        file = self.file("rebase/shared.py")
        expect(file.locator(".upstream-badge")).to_have_text("only rebase changes")
        expect(file.locator("table.diff")).to_have_count(0)
        file.locator(".file-header").click()
        rows = file.locator("tr.line.upstream")
        expect(rows).not_to_have_count(0)
        expect(file.locator("tr.line:not(.upstream) td.code.add")).to_have_count(0)
        expect(rows.locator("td.code.add")).to_contain_text(["changed upstream"])

    def test_rebase_with_an_edit_shows_the_edit(self) -> None:
        self.open(scenarios.REBASE_AND_EDIT)
        self.choose(1, 2)
        expect(self.page.locator(".diff-summary")).to_contain_text(
            "1 files changed · 2 files changed only by the rebase are hidden"
        )
        file = self.file("rebase_and_edit/shared.py")
        expect(file.locator(".upstream-badge")).to_have_count(0)
        # The PR's own edit is a plain change; the upstream one is marked.
        expect(file.locator("tr.line:not(.upstream) td.code.add")).to_have_text(
            [scenarios.EDITED_SETTING]
        )
        expect(file.locator("tr.line.upstream td.code.add")).to_contain_text(
            ["changed upstream"]
        )

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


class AgentTest(ServerTestCase):
    OTHER_REPO = "test/other"
    REPOS: ClassVar[list[str]] = [scenarios.REPO, OTHER_REPO]

    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        for slug in cls.REPOS:
            repo = gh.repo(slug)
            repo.commit("Seed", {"README": "hello\n"})
            repo.push("main")

    def section(self, repo: str) -> Locator:
        return self.page.locator(".agent-repo").filter(
            has=self.page.locator(".agent-repo-name span", has_text=exactly(repo))
        )

    def test_sidebar_has_a_section_per_repo(self) -> None:
        self.page.goto(self.url + "#/agents")
        expect(self.page.locator(".agent-repo-name span")).to_have_text(
            sorted([scenarios.REPO, self.OTHER_REPO])
        )

        # A repository without sessions says so and links to the form.
        other = self.section(self.OTHER_REPO)
        expect(other.locator(".agent-none")).to_contain_text(
            "No agents in this repository yet."
        )
        other.get_by_role("link", name="Start one").click()
        expect(self.page).to_have_url(re.compile(f"#/agents/new/{self.OTHER_REPO}$"))
        expect(self.page.get_by_label("Repository")).to_have_value(self.OTHER_REPO)

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
        expect(approval).to_contain_text("Allowed Write")
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
        expect(transcript.locator(".approval")).to_have_count(1)
        self.assertEqual((worktree / "notes.txt").read_text(), "write more")

        # The transcript is stored, so a reload shows it all again.
        self.page.reload()
        expect(transcript.locator(".turn-prompt")).to_have_text(
            ["please write a note", "thanks\nso much", "write more"]
        )
        expect(permissions).to_have_value("auto")
        expect(model).to_have_value("sonnet")
        expect(transcript.locator(".approval")).to_contain_text("Allowed Write")
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
        self.page.set_viewport_size({"width": 1000, "height": 300})
        self.page.get_by_role("link", name="Agents").click()
        item.click()
        expect(transcript.locator(".turn-end").last).to_be_in_viewport()
        expect(transcript.locator(".turn-prompt").first).not_to_be_in_viewport()

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
        expect(self.page).to_have_url(re.compile(r"#/agents$"))
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
        files = pane.locator("section.file")
        expect(files.locator(".path")).to_have_text(["notes.txt"])
        expect(files.locator(".status")).to_have_text(["added"])
        expect(pane.locator("table.diff.unified tr.add .code")).to_have_text(
            ["write a note"]
        )

        # Edits made since show up with the agent's next events.
        worktree = Path(self.page.locator(".agent-worktree").inner_text())
        (worktree / "README").write_text("hello\nworld\n")
        follow_up = self.page.get_by_label("Follow-up")
        follow_up.fill("write more")
        follow_up.press("Enter")
        expect(transcript.locator(".turn-end")).to_have_count(2)
        expect(files.locator(".path")).to_have_text(["README", "notes.txt"])
        expect(files.locator(".status")).to_have_text(["modified", "added"])
        expect(files.first.locator("tr.add .code")).to_have_text(["world"])
        expect(files.nth(1).locator("tr.add .code")).to_have_text(["write more"])

        # The transcript keeps most of the width.
        main = self.page.locator(".agents-page > .main").bounding_box()
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
        expect(opened.get_by_role("link")).to_have_attribute("href", pr.url)
        expect(header.get_by_role("link", name=f"PR #{pr.number}")).to_be_visible()
        self.assertEqual(pr.base, "main")
        self.assertTrue(pr.head.startswith("grenadine/agent/"))
        # The checkout stays where the first wrap-up left it.
        self.assertEqual(git("rev-parse", "HEAD"), sha)

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

        # A plan sent back comes again with the feedback in it.
        plans = transcript.locator(".approval.plan")
        expect(plans.locator(".plan-text strong")).to_have_text("todo.txt")
        plans.get_by_label("Feedback").fill("Add a title")
        plans.get_by_role("button", name="Keep planning").click()
        expect(plans).to_have_count(2)
        expect(plans.first).to_contain_text("Plan sent back")
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


class CleanupTest(ServerTestCase):
    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        repo = gh.repo(scenarios.REPO)
        repo.commit("Seed", {"README": "hello\n"})
        repo.push("main")

    def test_deleted_sessions_leave_cleanup_items(self) -> None:
        # The section is there before anything is left behind.
        self.page.goto(self.url + f"#/agents/new/{scenarios.REPO}")
        section = self.page.locator(".agent-cleanup .agent-item")
        expect(section).to_contain_text("Nothing to clean up")

        self.page.get_by_label("Permissions").select_option(label="Auto")
        self.page.get_by_label("Prompt").fill("run the e2e tests")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page.locator(".transcript .turn-end")).to_have_count(1)
        self.page.once("dialog", lambda dialog: dialog.accept())
        self.page.locator(".agent-actions").get_by_role("button", name="Delete").click()
        expect(self.page).to_have_url(re.compile(r"#/agents$"))

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
        self.page.get_by_role("button", name="Clean up 1 item in a new session").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))
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


if __name__ == "__main__":
    unittest.main()
