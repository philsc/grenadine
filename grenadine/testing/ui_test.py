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

from playwright.sync_api import Locator, expect, sync_playwright

from grenadine.testing import scenarios
from grenadine.testing.fake_github import TOKEN, FakeGitHub

# How long the server may take to start listening.
STARTUP_TIMEOUT = 30

# Stands in for `claude -p`: it writes a short turn as stream-json and,
# when the prompt asks it to write, asks grenadine for permission through
# the MCP tool first, like Claude Code does.
FAKE_CLAUDE = r'''
import json
import sys
import time
import urllib.request

args = sys.argv[1:]
prompt = sys.stdin.read()
url = json.loads(args[args.index("--mcp-config") + 1])["mcpServers"]["grenadine"]["url"]


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


emit({"type": "system", "subtype": "init", "session_id": "s"})
for word in ["Working ", "on ", "it"]:
    emit({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": word}}})
    time.sleep(0.05)
emit({"type": "assistant", "message": {"content": [{"type": "text", "text": f"You said: **{prompt}**"}]}})
if "write" in prompt:
    tool_input = {"file_path": "notes.txt", "content": prompt}
    emit({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "toolu_1", "name": "Write", "input": tool_input}]}})
    rpc("initialize", {"protocolVersion": "2025-11-25"}, id=0)
    answer = rpc("tools/call", {"name": "approve", "arguments": {"tool_name": "Write", "input": tool_input, "tool_use_id": "toolu_1"}})
    decision = json.loads(answer["result"]["content"][0]["text"])
    if decision["behavior"] == "allow":
        with open("notes.txt", "w") as f:
            f.write(prompt)
        result = {"type": "tool_result", "tool_use_id": "toolu_1", "content": "Wrote notes.txt"}
    else:
        result = {"type": "tool_result", "tool_use_id": "toolu_1", "content": decision["message"], "is_error": True}
    emit({"type": "user", "message": {"content": [result]}})
emit({"type": "result", "subtype": "success", "is_error": False, "result": "", "total_cost_usd": 0.01})
'''


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def exactly(text: str) -> re.Pattern[str]:
    return re.compile(f"^{re.escape(text)}$")


class ServerTestCase(unittest.TestCase):
    """Runs the server and a browser for the class's tests."""

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
        clone = tmp / "clone"
        subprocess.run(["git", "init", "--quiet", clone], env=env, check=True)
        subprocess.run(
            ["git", "-C", clone, "remote", "add", "origin", f"https://github.com/{scenarios.REPO}"],
            env=env,
            check=True,
        )

        port = free_port()
        cls.url = f"http://127.0.0.1:{port}/"
        cls.server = subprocess.Popen(
            [
                os.environ["GRENADINE_SERVER"],
                f"--repo={clone}",
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
        cls.browser = cls.playwright.chromium.launch(executable_path=os.environ["CHROME"])
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
        expect(inboxes.locator(".count")).to_have_text(["0"] * len(scenarios.default_inboxes()))


class InboxTest(ServerTestCase):
    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        cls.cases = scenarios.inbox_cases(gh)

    def inbox(self, name: str) -> Locator:
        return self.page.locator("section.inbox").filter(
            has=self.page.locator(".inbox-name", has_text=exactly(name)))

    def row(self, title: str) -> Locator:
        return self.page.locator(".pr-list li").filter(
            has=self.page.locator(".pr-title", has_text=re.compile(f"{re.escape(title)}$")))

    def expect_titles(self, inbox: Locator, titles: list[str]) -> None:
        """Checks that `inbox` lists exactly `titles`, in any order, each
        synced without errors."""
        expect(inbox.locator(".count")).to_have_text(str(len(titles)))
        rows = inbox.locator(".pr-list li")
        expect(rows).to_have_count(len(titles))
        for title in titles:
            row = rows.filter(has=self.page.locator(".pr-title", has_text=re.compile(f"{re.escape(title)}$")))
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
        expect(self.inbox(scenarios.NEEDS_REVIEW).locator(".pr-list li")).not_to_have_count(0)
        for title, inbox in self.cases.expected.items():
            if inbox is None:
                expect(self.row(title)).to_have_count(0)

    def test_badges(self) -> None:
        self.page.goto(self.url)
        expect(self.inbox(scenarios.DRAFTS).locator(".pr-title .badge")).to_have_text("draft")
        expect(self.inbox(scenarios.WAITING_FOR_REVIEWERS).locator(".pr-title .badge")).to_have_count(2)
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
            ids = [i["inbox"]["id"] for i in json.load(resp) if i["inbox"]["name"] == name]
        for id in ids:
            req = urllib.request.Request(f"{self.url}api/inboxes/{id}", method="DELETE")
            urllib.request.urlopen(req).close()

    def test_custom_inbox(self) -> None:
        self.page.goto(self.url)
        inbox = self.add_inbox("By the other account", "state:open author:other sort:created-asc")
        others = [
            title for title, pr in self.cases.prs.items()
            if pr.author == scenarios.OTHER and pr.state == "OPEN"
        ]
        self.expect_titles(inbox, others)
        # sort:created-asc reaches GitHub and orders the PRs.
        expect(inbox.locator(".pr-title")).to_have_text(
            [re.compile(f"{re.escape(t)}$") for t in others])

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
        expect(rows).to_have_text([
            f"#{top.number} {top.title} open",
            f"#{base.number} {base.title} open",
            "main",
        ])
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
        expect(self.page.locator(".pr-header h1")).to_have_text(f"{pr.title} #{pr.number}")

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
        expect(picker.locator("summary")).to_have_text(f"{name} → v{head} ({self.shas[head - 1][:8]})")

    def file(self, path: str) -> Locator:
        return self.page.locator("section.file").filter(
            has=self.page.locator(".file-header .path", has_text=exactly(path)))

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
            ["opened", "push", "push", "push", "push", "force push"])
        expect(rows.locator(".pushed-by")).to_have_text([""] + ["Me Myself"] * len(shas))
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
        expect(file.locator("table.diff.split td.code.del")).to_have_text("    return 6")
        expect(file.locator("table.diff.split td.code.add")).to_have_text("    return 6 * 2")

        # The unified view shows the same change.
        self.page.get_by_label("Side by side").uncheck()
        expect(file.locator("table.diff.unified tr.line.del .code")).to_have_text("    return 6")
        expect(file.locator("table.diff.unified tr.line.add .code")).to_have_text("    return 6 * 2")

        # Three pushed commits became three versions, so v2 → v5 adds three
        # steps.
        self.choose(2, 5)
        expect(file.locator(".stats")).to_have_text("+12 −0")

    def test_rebase_only_changes_are_hidden(self) -> None:
        self.open(scenarios.REBASE)
        self.choose(1, 2)
        expect(self.page.locator(".diff-summary")).to_contain_text(
            "1 files changed · 2 files changed only by the rebase are hidden")
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
            "1 files changed · 2 files changed only by the rebase are hidden")
        file = self.file("rebase_and_edit/shared.py")
        expect(file.locator(".upstream-badge")).to_have_count(0)
        # The PR's own edit is a plain change; the upstream one is marked.
        expect(file.locator("tr.line:not(.upstream) td.code.add")).to_have_text(
            [scenarios.EDITED_SETTING])
        expect(file.locator("tr.line.upstream td.code.add")).to_contain_text(["changed upstream"])

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
        expect(self.file("kinds/old_name.txt → kinds/new_name.txt").locator(".stats")).to_have_text("+0 −0")


class AgentTest(ServerTestCase):
    @classmethod
    def load(cls, gh: FakeGitHub) -> None:
        repo = gh.repo(scenarios.REPO)
        repo.commit("Seed", {"README": "hello\n"})
        repo.push("main")

    def test_agent_session(self) -> None:
        self.page.goto(self.url)
        self.page.get_by_role("link", name="Agents").click()
        self.page.get_by_label("Prompt").fill("please write a note")
        self.page.get_by_role("button", name="Start").click()
        expect(self.page).to_have_url(re.compile(r"#/agents/[0-9a-f-]{36}$"))

        transcript = self.page.locator(".transcript")
        expect(transcript.locator(".turn-prompt")).to_have_text(["please write a note"])
        expect(transcript.locator(".turn-text strong")).to_have_text("please write a note")
        approval = transcript.locator(".approval")
        expect(approval).to_contain_text("Claude wants to use Write notes.txt")
        approval.get_by_role("button", name="Allow").click()
        expect(approval).to_contain_text("Allowed Write")
        expect(transcript.locator(".turn-end")).to_have_text(["Done · $0.01"])
        tool = transcript.locator(".tool")
        tool.locator("summary").click()
        expect(tool.locator(".tool-result")).to_have_text("Wrote notes.txt")

        # The agent worked in the session's worktree, which starts at main.
        worktree = Path(self.page.locator(".agent-worktree").inner_text())
        self.assertEqual((worktree / "notes.txt").read_text(), "please write a note")
        self.assertEqual((worktree / "README").read_text(), "hello\n")

        follow_up = self.page.get_by_label("Follow-up")
        follow_up.fill("thanks")
        follow_up.press("Control+Enter")
        expect(transcript.locator(".turn-prompt")).to_have_text(["please write a note", "thanks"])
        expect(transcript.locator(".turn-end")).to_have_count(2)
        expect(follow_up).to_have_value("")

        # The transcript is stored, so a reload shows it all again.
        self.page.reload()
        expect(transcript.locator(".turn-prompt")).to_have_text(["please write a note", "thanks"])
        expect(transcript.locator(".approval")).to_contain_text("Allowed Write")
        expect(self.page.locator(".agent-header")).to_contain_text("idle")

        self.page.get_by_role("link", name="Agents").click()
        expect(self.page.locator(".agent-item")).to_contain_text(["please write a note"])


if __name__ == "__main__":
    unittest.main()
