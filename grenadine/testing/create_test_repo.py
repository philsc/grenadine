"""Creates the GitHub repo that grenadine's end-to-end tests run against.

The repo holds one PR per test case. Each case exercises a single grenadine
feature: inbox placement, version detection, hiding rebase-only changes, diff
rendering, markdown rendering, or review comments. The script also writes a
manifest describing what grenadine should show for each PR, so that a browser
test can check the UI against it.

Two GitHub accounts are needed, both logged in to `gh`:

  - The active account ("me") is the one grenadine runs as. It must be an
    admin of the org, with the admin:org scope (and delete_repo for
    --recreate): gh auth refresh -s admin:org,delete_repo
  - --other-user authors the PRs that someone else must have opened, and
    reviews the PRs that I opened. Add it with `gh auth login`, then switch
    back with `gh auth switch --user ME`.

The repo is public because merge queues on org repos need either a public
repo or GitHub Enterprise.
"""

import argparse
import dataclasses
import json
import os
import shutil
import struct
import subprocess
import sys
import time
import zlib
from pathlib import Path
from typing import Any

# The default inboxes, as named in grenadine/server/src/inboxes.rs.
NEEDS_REVIEW = "Needs your review"
NEEDS_TEAM_REVIEW = "Needs your teams' review"
RETURNED = "Returned to you"
APPROVED = "Approved"
WAITING_FOR_REVIEWERS = "Waiting for reviewers"
DRAFTS = "Drafts"
MERGING = "Merging"
DRAFTS_NEEDING_REVIEW = "Drafts needing your review"
WAITING_FOR_AUTHORS = "Waiting for authors"

TEAM_NAME = "grenadine-test-reviewers"
# A required status check that nothing ever reports on merge groups. It keeps
# the "Merging" PR in the merge queue until the queue gives up on it.
HOLD_CONTEXT = "grenadine-hold"
HOLD_TIMEOUT_MINUTES = 360

# grenadine orders versions by the branch activity log's timestamps. Pausing
# after each push keeps them distinct.
PUSH_PAUSE_SECONDS = 2

# Supplies the token from the environment, so it never shows up in a command
# line or in the clone's config.
GIT_CREDENTIAL_HELPER = (
    '!f() { echo username=x-access-token; echo "password=$GRENADINE_GIT_TOKEN"; }; f'
)


class Failure(Exception):
    pass


@dataclasses.dataclass(frozen=True)
class Symlink:
    target: str


@dataclasses.dataclass(frozen=True)
class Executable:
    content: str | bytes


# What a path should hold after a commit. None deletes it.
Content = str | bytes | Symlink | Executable | None


@dataclasses.dataclass
class PullRequest:
    case: str
    number: int
    node_id: str
    url: str
    author: str
    branch: str
    expected_inbox: str | None
    notes: str
    versions: list[dict[str, str]] = dataclasses.field(default_factory=list)
    files: list[dict[str, str]] = dataclasses.field(default_factory=list)

    @property
    def head(self) -> str:
        return self.versions[-1]["sha"]


def log(message: str) -> None:
    print(message, flush=True)


def run(cmd: list[str], *, env: dict[str, str] | None = None, input: str | None = None,
        cwd: Path | None = None, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        cmd,
        input=input,
        capture_output=True,
        text=True,
        cwd=cwd,
        env={**os.environ, **(env or {})},
    )
    if check and result.returncode != 0:
        raise Failure(
            f"{' '.join(cmd[:4])} ... failed ({result.returncode}):\n"
            f"{result.stderr.strip()}\n{result.stdout.strip()}"
        )
    return result


def gh_token(user: str) -> str:
    result = run(["gh", "auth", "token", "--hostname", "github.com", "--user", user], check=False)
    if result.returncode != 0:
        raise Failure(
            f"gh has no token for {user}. Log in with `gh auth login`, then make "
            "the account grenadine runs as active again with `gh auth switch`."
        )
    return result.stdout.strip()


def api(method: str, path: str, token: str, body: Any = None, check: bool = True) -> Any:
    """Calls the GitHub API as the owner of `token`.

    Returns the decoded response, or None if the call failed and `check` is
    false.
    """
    cmd = [
        "gh", "api", "--method", method, path,
        "-H", "Accept: application/vnd.github+json",
        "-H", "X-GitHub-Api-Version: 2022-11-28",
    ]
    if body is not None:
        cmd += ["--input", "-"]
    result = run(
        cmd,
        env={"GH_TOKEN": token},
        input=None if body is None else json.dumps(body),
        check=check,
    )
    if result.returncode != 0:
        return None
    return json.loads(result.stdout) if result.stdout.strip() else None


def graphql(query: str, token: str, **variables: Any) -> Any:
    data = api("POST", "graphql", token, {"query": query, "variables": variables})
    if data.get("errors"):
        raise Failure(f"GraphQL errors: {data['errors']}")
    return data["data"]


def token_scopes(token: str) -> set[str] | None:
    """The token's OAuth scopes, or None for tokens that don't report any."""
    result = run(["gh", "api", "-i", "/user"], env={"GH_TOKEN": token})
    for line in result.stdout.splitlines():
        name, _, value = line.partition(":")
        if name.lower() == "x-oauth-scopes":
            return {s.strip() for s in value.split(",") if s.strip()}
        if not line.strip():
            break
    return None


def png(width: int, height: int, rgb: tuple[int, int, int]) -> bytes:
    """A solid-colored PNG."""

    def chunk(kind: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data))
        )

    rows = b"".join(b"\x00" + bytes(rgb) * width for _ in range(height))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows))
        + chunk(b"IEND", b"")
    )


def lines(items: list[str]) -> str:
    return "".join(f"{item}\n" for item in items)


class Fixture:
    def __init__(self, args: argparse.Namespace, me: str, me_token: str, other_token: str):
        self.owner: str = args.owner
        self.name: str = args.name
        self.slug = f"{self.owner}/{self.name}"
        self.clone: Path = args.clone_dir
        self.me = me
        self.other: str = args.other_user
        self.tokens = {me: me_token, self.other: other_token}
        self.team_slug = TEAM_NAME
        self.prs: dict[str, PullRequest] = {}
        self.to_enqueue: list[PullRequest] = []

    # GitHub, as one of the two accounts.

    def api(self, method: str, path: str, body: Any = None, *, user: str | None = None,
            check: bool = True) -> Any:
        return api(method, path, self.tokens[user or self.me], body, check)

    def repo_api(self, method: str, path: str, body: Any = None, *, user: str | None = None) -> Any:
        return self.api(method, f"repos/{self.slug}/{path}", body, user=user)

    # Local git.

    def git(self, *args: str) -> str:
        cmd = [
            "git",
            "-c", "credential.helper=",
            "-c", f"credential.helper={GIT_CREDENTIAL_HELPER}",
            "-c", "commit.gpgsign=false",
            *args,
        ]
        return run(cmd, cwd=self.clone, env={"GRENADINE_GIT_TOKEN": self.tokens[self.me]}).stdout.strip()

    def rev(self, ref: str = "HEAD") -> str:
        return self.git("rev-parse", ref)

    def commit(self, message: str, files: dict[str, Content], *, amend: bool = False) -> str:
        for path, content in files.items():
            target = self.clone / path
            if target.is_symlink() or target.exists():
                target.unlink()
            if content is None:
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            if isinstance(content, Symlink):
                target.symlink_to(content.target)
                continue
            data = content.content if isinstance(content, Executable) else content
            if isinstance(data, str):
                target.write_text(data, encoding="utf-8")
            else:
                target.write_bytes(data)
            target.chmod(0o755 if isinstance(content, Executable) else 0o644)
        self.git("add", "-A")
        self.git("commit", "--quiet", *(["--amend"] if amend else []), "-m", message)
        return self.rev()

    def start_branch(self, branch: str) -> None:
        self.git("fetch", "--quiet", "origin", "main")
        self.git("checkout", "--quiet", "-B", branch, "origin/main")

    def push_branch(self, branch: str, *, force: bool = False) -> None:
        self.git("push", "--quiet", *(["--force"] if force else []), "origin", f"HEAD:refs/heads/{branch}")
        time.sleep(PUSH_PAUSE_SECONDS)

    def advance_main(self, message: str, files: dict[str, Content]) -> str:
        """Commits straight to main, as if other PRs had landed."""
        self.start_branch("main")
        sha = self.commit(message, files)
        self.push_branch("main")
        return sha

    # Pull requests.

    def open_pr(self, case: str, title: str, body: str, *, author: str | None = None,
                draft: bool = False, expected_inbox: str | None, notes: str) -> PullRequest:
        """Opens a PR for the current branch, which must already be pushed."""
        author = author or self.me
        branch = self.git("rev-parse", "--abbrev-ref", "HEAD")
        data = self.repo_api(
            "POST", "pulls",
            {"title": title, "head": branch, "base": "main", "body": body, "draft": draft},
            user=author,
        )
        pr = PullRequest(
            case=case,
            number=data["number"],
            node_id=data["node_id"],
            url=data["html_url"],
            author=author,
            branch=branch,
            expected_inbox=expected_inbox,
            notes=notes,
            versions=[{"kind": "initial", "sha": self.rev()}],
        )
        self.record_files(pr)
        self.prs[case] = pr
        log(f"  #{pr.number} {case}")
        return pr

    def push(self, pr: PullRequest, *, force: bool = False) -> None:
        """Pushes the current branch to `pr` and records the new versions."""
        self.push_branch(pr.branch, force=force)
        head = self.rev()
        if force:
            pr.versions.append({"kind": "force_push", "sha": head})
        else:
            # grenadine makes each commit of a fast-forward push its own
            # version, following first parents.
            pushed = self.git("rev-list", "--reverse", "--first-parent", f"{pr.head}..{head}")
            pr.versions += [{"kind": "push", "sha": sha} for sha in pushed.split()]
        self.record_files(pr)

    def record_files(self, pr: PullRequest) -> None:
        """Records the files `pr` changes against its merge base."""
        self.git("fetch", "--quiet", "origin", "main")
        base = self.git("merge-base", "origin/main", "HEAD")
        out = self.git("diff", "--name-status", "-M", "-z", base, "HEAD")
        fields = [f for f in out.split("\0") if f]
        files = []
        while fields:
            status = fields.pop(0)
            if status[0] in "RC":
                old, new = fields.pop(0), fields.pop(0)
                files.append({"status": status[0], "old_path": old, "path": new})
            else:
                files.append({"status": status[0], "path": fields.pop(0)})
        pr.files = files

    def request_review(self, pr: PullRequest, *, users: tuple[str, ...] = (),
                       teams: tuple[str, ...] = ()) -> None:
        self.repo_api(
            "POST", f"pulls/{pr.number}/requested_reviewers",
            {"reviewers": list(users), "team_reviewers": list(teams)},
        )

    def review(self, pr: PullRequest, by: str, event: str, body: str) -> None:
        self.repo_api(
            "POST", f"pulls/{pr.number}/reviews",
            {"commit_id": pr.head, "event": event, "body": body},
            user=by,
        )

    def comment(self, pr: PullRequest, by: str, commit: str, path: str, body: str,
                **position: Any) -> int:
        """Leaves an inline review comment. `position` holds line/side/... fields."""
        data = self.repo_api(
            "POST", f"pulls/{pr.number}/comments",
            {"commit_id": commit, "path": path, "body": body, **position},
            user=by,
        )
        return data["id"]

    def reply(self, pr: PullRequest, by: str, comment_id: int, body: str) -> None:
        self.repo_api("POST", f"pulls/{pr.number}/comments/{comment_id}/replies", {"body": body}, user=by)


# Setup.


def preflight(args: argparse.Namespace) -> tuple[str, str, str]:
    me = run(["gh", "api", "user", "--jq", ".login"]).stdout.strip()
    if me.lower() == args.other_user.lower():
        raise Failure(
            f"--other-user is the active gh account ({me}). Make the account "
            "grenadine runs as active with `gh auth switch`."
        )
    me_token, other_token = gh_token(me), gh_token(args.other_user)

    needed = {"repo", "admin:org"} | ({"delete_repo"} if args.recreate else set())
    scopes = token_scopes(me_token)
    if scopes is not None and not needed <= scopes:
        missing = ",".join(sorted(needed - scopes))
        raise Failure(f"{me}'s token lacks scopes {missing}. Run: gh auth refresh -s {missing}")

    membership = api("GET", f"orgs/{args.owner}/memberships/{me}", me_token, check=False)
    if not membership or membership.get("role") != "admin":
        raise Failure(f"{me} must be an admin of the {args.owner} org.")
    return me, me_token, other_token


def prepare_clone_dir(fx: Fixture, recreate: bool) -> None:
    clone = fx.clone
    if clone.exists() and any(clone.iterdir()):
        url = run(["git", "config", "--get", "remote.origin.url"], cwd=clone, check=False).stdout.strip()
        ours = url.removesuffix(".git").endswith(fx.slug)
        if not (recreate and ours):
            raise Failure(f"{clone} is not empty. Pass --recreate to replace an earlier clone of {fx.slug}.")
        shutil.rmtree(clone)
    clone.mkdir(parents=True, exist_ok=True)


def create_repo(fx: Fixture, recreate: bool) -> None:
    if fx.api("GET", f"repos/{fx.slug}", check=False) is not None:
        if not recreate:
            raise Failure(f"{fx.slug} already exists. Pass --recreate to delete and rebuild it.")
        log(f"Deleting {fx.slug}")
        fx.api("DELETE", f"repos/{fx.slug}")

    log(f"Creating {fx.slug}")
    body = {
        "name": fx.name,
        "visibility": "public",
        "description": "Test PRs for grenadine. Generated by grenadine/testing/create_test_repo.py.",
        "has_wiki": False,
        "has_projects": False,
    }
    # GitHub can take a moment to free the name of a deleted repo.
    for attempt in range(10):
        if fx.api("POST", f"orgs/{fx.owner}/repos", body, check=attempt == 9) is not None:
            break
        time.sleep(3)

    invitation = fx.api("PUT", f"repos/{fx.slug}/collaborators/{fx.other}", {"permission": "push"})
    if invitation:
        fx.api("PATCH", f"user/repository_invitations/{invitation['id']}", user=fx.other)


def set_up_team(fx: Fixture) -> None:
    org = fx.owner
    if fx.api("GET", f"orgs/{org}/teams/{fx.team_slug}", check=False) is None:
        log(f"Creating team {org}/{fx.team_slug}")
        team = fx.api("POST", f"orgs/{org}/teams", {
            "name": TEAM_NAME,
            "description": "Reviewers for grenadine's test PRs.",
            "privacy": "closed",
        })
        fx.team_slug = team["slug"]
    fx.api("PUT", f"orgs/{org}/teams/{fx.team_slug}/memberships/{fx.me}", {"role": "maintainer"})
    fx.api("PUT", f"orgs/{org}/teams/{fx.team_slug}/repos/{fx.slug}", {"permission": "push"})


def seed_main(fx: Fixture) -> None:
    fx.git("init", "--quiet", "--initial-branch=main")
    fx.git("remote", "add", "origin", f"https://github.com/{fx.slug}.git")
    fx.commit("Add a README", {"README.md": lines([
        "# grenadine-test",
        "",
        "Test PRs for grenadine's end-to-end tests.",
        "",
        "Generated by `grenadine/testing/create_test_repo.py`. Every run deletes",
        "and recreates this repo, so don't change anything here by hand.",
    ])})
    fx.git("push", "--quiet", "--set-upstream", "origin", "main")


def enable_merge_queue(fx: Fixture) -> None:
    """Requires a merge queue on main. Main takes no more direct pushes after this."""
    log("Enabling the merge queue")
    fx.repo_api("POST", "rulesets", {
        "name": "merge-queue",
        "target": "branch",
        "enforcement": "active",
        "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
        "rules": [
            {"type": "merge_queue", "parameters": {
                "check_response_timeout_minutes": HOLD_TIMEOUT_MINUTES,
                "grouping_strategy": "ALLGREEN",
                "max_entries_to_build": 5,
                "max_entries_to_merge": 5,
                "merge_method": "MERGE",
                "min_entries_to_merge": 1,
                "min_entries_to_merge_wait_minutes": 0,
            }},
            {"type": "required_status_checks", "parameters": {
                "strict_required_status_checks_policy": False,
                "required_status_checks": [{"context": HOLD_CONTEXT}],
            }},
        ],
    })
    for pr in fx.to_enqueue:
        # The PR itself passes the check, so it may enter the queue. Its merge
        # group never gets the check, so it stays there.
        fx.repo_api("POST", f"statuses/{pr.head}", {"state": "success", "context": HOLD_CONTEXT})
        for attempt in range(10):
            try:
                graphql(
                    "mutation($id: ID!) { enqueuePullRequest(input: {pullRequestId: $id}) { clientMutationId } }",
                    fx.tokens[fx.me],
                    id=pr.node_id,
                )
                break
            except Failure:
                # Mergeability is computed in the background.
                if attempt == 9:
                    raise
                time.sleep(3)


# Cases. Each one branches off the current main, so cases may advance main
# without disturbing each other.


def simple_pr(fx: Fixture, case: str, title: str, *, author: str | None = None, draft: bool = False,
              expected_inbox: str | None, notes: str) -> PullRequest:
    fx.start_branch(f"case/{case}")
    fx.commit(title, {f"inbox/{case}.md": lines([f"# {title}", "", notes])})
    fx.push_branch(f"case/{case}")
    return fx.open_pr(case, title, notes, author=author, draft=draft,
                      expected_inbox=expected_inbox, notes=notes)


def inbox_cases(fx: Fixture) -> None:
    me, other = fx.me, fx.other

    pr = simple_pr(fx, "inbox-needs-review", "Ask me for a review", author=other,
                   expected_inbox=NEEDS_REVIEW,
                   notes="Opened by the other account, with a review requested from me.")
    fx.request_review(pr, users=(me,))

    pr = simple_pr(fx, "inbox-needs-team-review", "Ask my team for a review", author=other,
                   expected_inbox=NEEDS_TEAM_REVIEW,
                   notes="Opened by the other account, with a review requested from my team only.")
    fx.request_review(pr, teams=(fx.team_slug,))

    pr = simple_pr(fx, "inbox-returned-commented", "Get a comment-only review",
                   expected_inbox=RETURNED,
                   notes="Mine. The other account left a review that only comments.")
    fx.review(pr, other, "COMMENT", "Some thoughts, but no verdict.")

    pr = simple_pr(fx, "inbox-returned-changes", "Get changes requested",
                   expected_inbox=RETURNED,
                   notes="Mine. The other account requested changes.")
    fx.review(pr, other, "REQUEST_CHANGES", "Please rework this.")

    pr = simple_pr(fx, "inbox-approved", "Get approved",
                   expected_inbox=APPROVED,
                   notes="Mine. The other account approved it.")
    fx.review(pr, other, "APPROVE", "")

    simple_pr(fx, "inbox-waiting-reviewers", "Wait for reviewers",
              expected_inbox=WAITING_FOR_REVIEWERS,
              notes="Mine, with no reviews.")

    simple_pr(fx, "inbox-draft", "Stay a draft", draft=True,
              expected_inbox=DRAFTS,
              notes="Mine, and a draft.")

    pr = simple_pr(fx, "inbox-merging", "Sit in the merge queue",
                   expected_inbox=MERGING,
                   notes="Mine, approved, and in the merge queue. The queue drops it "
                         f"{HOLD_TIMEOUT_MINUTES} minutes after the script runs.")
    fx.review(pr, other, "APPROVE", "")
    fx.to_enqueue.append(pr)

    pr = simple_pr(fx, "inbox-draft-needs-review", "Ask me to look at a draft", author=other, draft=True,
                   expected_inbox=DRAFTS_NEEDING_REVIEW,
                   notes="A draft by the other account, with a review requested from me.")
    fx.request_review(pr, users=(me,))

    pr = simple_pr(fx, "inbox-waiting-authors", "Wait for the author after my review", author=other,
                   expected_inbox=WAITING_FOR_AUTHORS,
                   notes="Opened by the other account. I reviewed it without being asked.")
    fx.review(pr, me, "COMMENT", "Over to you.")

    pr = simple_pr(fx, "inbox-none-closed", "Get closed", expected_inbox=None,
                   notes="Mine, and closed without merging. It belongs in no inbox.")
    fx.repo_api("PATCH", f"pulls/{pr.number}", {"state": "closed"})

    pr = simple_pr(fx, "inbox-none-merged", "Get merged", expected_inbox=None,
                   notes="Mine, and merged. It belongs in no inbox.")
    fx.repo_api("PUT", f"pulls/{pr.number}/merge", {"merge_method": "merge"})

    simple_pr(fx, "inbox-none-uninvolved", "Leave me out of it", author=other, expected_inbox=None,
              notes="Opened by the other account, and I'm not involved. It belongs in no inbox.")


def versions_pushes(fx: Fixture) -> None:
    case = "versions-pushes"
    path = "versions/steps.py"

    def steps(count: int, doubled: int = 0) -> str:
        body = ['"""One function per pushed version."""']
        for i in range(1, count + 1):
            value = f"{i} * 2" if i == doubled else str(i)
            body += ["", "", f"def step_{i}():", f"    return {value}"]
        return lines(body)

    fx.start_branch(f"case/{case}")
    fx.commit("Add step 1", {path: steps(1)})
    fx.commit("Add step 2", {path: steps(2)})
    fx.push_branch(f"case/{case}")
    pr = fx.open_pr(
        case, "Grow one push at a time",
        "Opened with two commits, then pushed to several times.",
        expected_inbox=WAITING_FOR_REVIEWERS,
        notes="Six versions: the initial head (2 commits); a fast-forward push of 1 "
              "commit; a fast-forward push of 3 commits, which grenadine makes 3 "
              "versions; and an amended commit that was force-pushed.",
    )
    fx.commit("Add step 3", {path: steps(3)})
    fx.push(pr)
    for i in (4, 5, 6):
        fx.commit(f"Add step {i}", {path: steps(i)})
    fx.push(pr)
    fx.commit("Add step 6", {path: steps(6, doubled=6)}, amend=True)
    fx.push(pr, force=True)


def versions_rebase(fx: Fixture, case: str, *, edit_after_rebase: bool) -> None:
    directory = case.replace("-", "_")
    shared, unrelated, upstream_only = (
        f"{directory}/shared.py", f"{directory}/unrelated.txt", f"{directory}/upstream_only.txt"
    )
    base = [f"SETTING_{i:02d} = {i}" for i in range(1, 61)]

    def with_changes(settings: list[str], changes: dict[int, str]) -> str:
        return lines([changes.get(i, line) for i, line in enumerate(settings, 1)])

    pr_lines = {i: f"SETTING_{i:02d} = {i} + 100  # changed by the PR" for i in (5, 6, 7)}
    upstream_lines = {50: "SETTING_50 = 50 - 1  # changed upstream"}

    fx.advance_main(f"Add {directory}", {shared: lines(base), unrelated: "upstream, before\n"})
    fx.start_branch(f"case/{case}")
    fx.commit("Change settings 5 to 7", {shared: with_changes(base, pr_lines)})
    fx.push_branch(f"case/{case}")

    if edit_after_rebase:
        title = "Rebase and edit in one push"
        notes = ("Version 2 rebases onto a main that changed unrelated.txt, added "
                 "upstream_only.txt and changed a distant line of shared.py, and also "
                 "changes the PR's own lines in shared.py. Between versions 1 and 2, "
                 "the two upstream-only files are hidden, and shared.py shows the PR's "
                 "edit next to the upstream change.")
    else:
        title = "Rebase onto a newer main"
        notes = ("Version 2 only rebases onto a main that changed unrelated.txt, added "
                 "upstream_only.txt and changed a distant line of shared.py. Between "
                 "versions 1 and 2, the two upstream-only files are hidden, and "
                 "shared.py has only rebase changes.")
    pr = fx.open_pr(case, title, notes, expected_inbox=WAITING_FOR_REVIEWERS, notes=notes)

    fx.advance_main("Change things upstream", {
        shared: with_changes(base, upstream_lines),
        unrelated: "upstream, after\n",
        upstream_only: "Only main added this file.\n",
    })
    fx.git("checkout", "--quiet", pr.branch)
    fx.git("rebase", "--quiet", "origin/main")
    if edit_after_rebase:
        edited = {**upstream_lines, **pr_lines, 6: "SETTING_06 = 6 + 200  # edited after the rebase"}
        fx.commit("Change settings 5 to 7", {shared: with_changes(base, edited)}, amend=True)
    fx.push(pr, force=True)


def diff_file_kinds(fx: Fixture) -> None:
    case = "diff-file-kinds"
    d = "kinds"
    renamed = lines([f"This line {i} moves along with its file." for i in range(1, 21)])
    renamed_edited = [f"def helper_{i}():\n    return {i}\n" for i in range(1, 11)]
    script = "#!/bin/sh\necho 'I become executable.'\n"
    big_line = "This text file is over grenadine's 4 MiB limit, so it diffs as binary.\n"
    big = big_line * (5 * 1024 * 1024 // len(big_line) + 1)

    fx.advance_main(f"Add {d}", {
        f"{d}/deleted.txt": "The PR deletes this file.\n",
        f"{d}/renamed.txt": renamed,
        f"{d}/renamed_edited.py": "\n\n".join(renamed_edited),
        f"{d}/script.sh": script,
        f"{d}/link_me.txt": "The PR turns this file into a symlink.\n",
        f"{d}/data.bin": bytes(range(256)) * 4,
    })
    fx.start_branch(f"case/{case}")
    edited = list(renamed_edited)
    edited[4] = "def helper_5():\n    return 5 * 5  # edited while moving\n"
    fx.commit("Change every kind of file", {
        f"{d}/added.md": "# Added\n\nThe PR adds this file.\n",
        f"{d}/deleted.txt": None,
        f"{d}/renamed.txt": None,
        f"{d}/moved/renamed.txt": renamed,
        f"{d}/renamed_edited.py": None,
        f"{d}/moved/renamed_edited.py": "\n\n".join(edited),
        f"{d}/script.sh": Executable(script),
        f"{d}/link_me.txt": Symlink("added.md"),
        f"{d}/data.bin": bytes(range(256)) * 8,
        f"{d}/logo.png": png(16, 16, (0xc0, 0x1f, 0x3c)),
        f"{d}/big.txt": big,
    })
    fx.push_branch(f"case/{case}")
    notes = ("Adds added.md, logo.png (binary) and big.txt (binary because it's over "
             "4 MiB). Deletes deleted.txt. Renames renamed.txt without changes and "
             "renamed_edited.py with an edit. Makes script.sh executable, turns "
             "link_me.txt into a symlink (a type change), and grows data.bin, a binary "
             "file, from 1024 to 2048 bytes.")
    fx.open_pr(case, "Change every kind of file", notes, expected_inbox=WAITING_FOR_REVIEWERS, notes=notes)


def diff_inline(fx: Fixture) -> None:
    case = "diff-inline"
    path = "inline/settings.py"
    long_before = "LONG = '" + "a" * 600 + "b" + "a" * 600 + "'"
    long_after = "LONG = '" + "a" * 600 + "c" + "a" * 600 + "'"
    before = lines([
        '"""Edits for intra-line highlighting."""',
        "",
        "TIMEOUT_SECONDS = 30",
        "RETRIES = 3",
        'GREETING = "Grüße, Welt 👋"',
        'LOG_FORMAT = "%(asctime)s %(levelname)s %(message)s"',
        'COLORS = ["red", "green", "blue"]',
        "WHITESPACE = 1   ",
        long_before,
        "",
        "",
        "def fetch_user(user_id, retries=RETRIES):",
        '    return {"id": user_id, "retries": retries}',
        "",
        "",
        "def replaced():",
        '    return "this line has nothing in common with its replacement"',
        "",
        "",
        "if True:",
        "  indented = 2",
    ])
    after = lines([
        '"""Edits for intra-line highlighting."""',
        "",
        "TIMEOUT_SECONDS = 45",
        "RETRIES = 3",
        'GREETING = "Grüße, Wörld 👋🌍"',
        'LOG_FORMAT = "%(asctime)s %(name)s %(levelname)s %(message)s"',
        'COLORS = ["red", "green", "blue", "purple"]',
        "WHITESPACE = 1",
        long_after,
        "",
        "",
        "def fetch_user(user_id, retries=RETRIES, backoff=1.5):",
        '    return {"id": user_id, "retries": retries}',
        "",
        "",
        "def replaced():",
        "    raise NotImplementedError(42)",
        "",
        "",
        "if True:",
        "    indented = 2",
    ])
    fx.advance_main("Add inline settings", {path: before})
    fx.start_branch(f"case/{case}")
    fx.commit("Edit lines a little at a time", {path: after})
    fx.push_branch(f"case/{case}")
    notes = ("Similar edits (a number, an inserted word, a list item, a new argument) "
             "should highlight just the changed characters. So should non-ASCII "
             "text, trailing whitespace and indentation. The two 1200-character "
             "LONG lines differ in one character, but grenadine never pairs lines "
             "over 1000 characters. The body of replaced() has nothing in common "
             "with its replacement, so the two lines shouldn't be paired.")
    fx.open_pr(case, "Edit lines a little at a time", notes, expected_inbox=WAITING_FOR_REVIEWERS, notes=notes)


# path: (contents on main, [(old, new) replacements made by the PR])
SYNTAX_FILES: dict[str, tuple[str, list[tuple[str, str]]]] = {
    "syntax/lib.rs": (
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        [("a + b", "a.saturating_add(b)")],
    ),
    "syntax/tool.py": (
        "def greet(name: str) -> str:\n    return f\"hello {name}\"\n",
        [("hello", "hi")],
    ),
    "syntax/app.ts": (
        "export function total(xs: number[]): number {\n  return xs.reduce((a, b) => a + b, 0);\n}\n",
        [("0);", "0) ?? 0;")],
    ),
    "syntax/view.tsx": (
        "export const View = () => <div className=\"view\">hello</div>;\n",
        [("hello", "<b>hello</b>")],
    ),
    "syntax/main.go": (
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hello\")\n}\n",
        [("\"hello\"", "\"hello, world\"")],
    ),
    "syntax/Cargo.toml": (
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        [("0.1.0", "0.2.0")],
    ),
    "syntax/README.md": (
        "# Demo\n\nSome *emphasis* and `code`.\n",
        [("*emphasis*", "**strong emphasis**")],
    ),
    "syntax/BUILD.bazel": (
        "load(\"//:defs.bzl\", \"demo\")\n\ndemo(\n    name = \"demo\",\n)\n",
        [("name = \"demo\",", "name = \"demo\",\n    visibility = [\"//visibility:public\"],")],
    ),
    "syntax/defs.bzl": (
        "def demo(name, **kwargs):\n    native.filegroup(name = name, **kwargs)\n",
        [("native.filegroup", "native.alias")],
    ),
    "syntax/data.json": (
        "{\n  \"enabled\": true,\n  \"count\": 1\n}\n",
        [("1", "2")],
    ),
    "syntax/style.css": (
        ".view {\n  color: red;\n}\n",
        [("red", "rebeccapurple")],
    ),
    "syntax/query.sql": (
        "SELECT id, name FROM users WHERE active;\n",
        [("WHERE active", "WHERE active ORDER BY name")],
    ),
    "syntax/Makefile": (
        "all:\n\techo building\n",
        [("building", "building everything")],
    ),
    "syntax/notes.unknownext": (
        "Plain text in a file whose extension nothing recognizes.\n",
        [("Plain", "Unhighlighted")],
    ),
    "syntax/huge.py": (
        "".join(f"VALUE_{i:05d} = {i}\n" for i in range(1, 20_501)),
        [("VALUE_10000 = 10000\n", "VALUE_10000 = 10000 * 2\n")],
    ),
}


def diff_syntax(fx: Fixture) -> None:
    case = "diff-syntax"
    fx.advance_main("Add files in many languages", {p: before for p, (before, _) in SYNTAX_FILES.items()})
    fx.start_branch(f"case/{case}")
    edited = {}
    for path, (before, replacements) in SYNTAX_FILES.items():
        for old, new in replacements:
            before = before.replace(old, new, 1)
        edited[path] = before
    fx.commit("Edit a file in every language", edited)
    fx.push_branch(f"case/{case}")
    notes = ("One small edit per language. BUILD.bazel and .bzl files highlight as "
             "Python, and .ts/.tsx as JavaScript. notes.unknownext has no "
             "highlighting. huge.py is over 20,000 lines, so it isn't highlighted.")
    fx.open_pr(case, "Edit a file in every language", notes, expected_inbox=WAITING_FOR_REVIEWERS, notes=notes)


def diff_hunks(fx: Fixture) -> None:
    case = "diff-hunks"
    path = "hunks/long.txt"
    before = [f"Line {i:03d} of a long file." for i in range(1, 301)]
    after = list(before)
    after[0] = "Line 001 of a long file, edited."
    after[99] = "Line 100 of a long file, edited."
    after[103] = "Line 104 of a long file, edited."
    del after[199]
    after.insert(249, "A line inserted before line 250.")
    after[-1] = "Line 300 of a long file, edited."
    fx.advance_main("Add a long file", {path: lines(before)})
    fx.start_branch(f"case/{case}")
    fx.commit("Edit a long file in far-apart places", {path: lines(after)})
    fx.push_branch(f"case/{case}")
    notes = ("Edits lines 1, 100, 104 and 300, deletes line 200, and inserts a line "
             "before line 250. The edits at 100 and 104 share a hunk. The unchanged "
             "stretches between hunks are skipped.")
    fx.open_pr(case, "Edit a long file in far-apart places", notes,
               expected_inbox=WAITING_FOR_REVIEWERS, notes=notes)


def comments(fx: Fixture) -> None:
    case = "comments"
    code, notes_md = "comments/values.py", "comments/notes.md"
    base = [f"VALUE_{i:02d} = {i}" for i in range(1, 41)]
    total = ['def total():', '    """Sum the first few values."""',
             "    values = [VALUE_01, VALUE_02, VALUE_03]", "    return sum(values)", ""]

    def version(values_line: str) -> str:
        changed = [f"VALUE_{i:02d} = {i} * 10  # scaled" if i in (10, 11, 12) else line
                   for i, line in enumerate(base, 1)]
        function = list(total)
        function[2] = values_line
        # Lines 26-30 of the new file hold total().
        return lines(changed[:25] + function + changed[25:])

    fx.advance_main("Add values", {code: lines(base), notes_md: "# Notes\n\nNothing yet.\n"})
    fx.start_branch(f"case/{case}")
    v1 = fx.commit("Scale some values and add total()", {
        code: version(total[2]),
        notes_md: "# Notes\n\nValues 10 to 12 are scaled.\n",
    })
    fx.push_branch(f"case/{case}")
    notes = ("The other account comments on version 1. One comment is on line 11, "
             "which version 2 keeps. One is on line 28, which version 2 rewrites, so "
             "that one goes outdated. On version 2 they then leave a single-line "
             "comment, a multi-line comment (lines 26-29), a comment on a removed "
             "line (left side, old line 10), a file-level comment on notes.md, and a "
             "comment with markdown. I reply to the single-line comment. There's also "
             "a general PR comment, which grenadine doesn't show yet.")
    pr = fx.open_pr(case, "Collect review comments", notes, expected_inbox=RETURNED, notes=notes)

    other = fx.other
    fx.comment(pr, other, v1, code, "Still applies to version 2.", line=11, side="RIGHT")
    fx.comment(pr, other, v1, code, "This list goes away in version 2.", line=28, side="RIGHT")

    v2 = fx.commit("Scale some values and add total()", {
        code: version('    values = [globals()[f"VALUE_{i:02d}"] for i in range(1, 41)]'),
        notes_md: "# Notes\n\nValues 10 to 12 are scaled.\n",
    }, amend=True)
    fx.push(pr, force=True)

    single = fx.comment(pr, other, v2, code, "Why scale by 10?", line=12, side="RIGHT")
    fx.reply(pr, fx.me, single, "To match the units used upstream.")
    fx.comment(pr, other, v2, code, "This whole function could be a one-liner.",
               start_line=26, start_side="RIGHT", line=29, side="RIGHT")
    fx.comment(pr, other, v2, code, "The old value was fine.", line=10, side="LEFT")
    fx.comment(pr, other, v2, notes_md, "These notes need more detail.", subject_type="file")
    fx.comment(pr, other, v2, code, lines([
        "Some **markdown** in a comment:",
        "",
        "- a list item",
        "- `inline code`",
        "",
        "```python",
        "print(sum(range(1, 41)))",
        "```",
    ]), line=29, side="RIGHT")
    fx.repo_api("POST", f"issues/{pr.number}/comments",
                {"body": "A general comment on the PR, not attached to any line."}, user=other)


def markdown_description(fx: Fixture) -> None:
    case = "markdown-description"
    image = "markdown/pixel.png"
    main_sha = fx.advance_main("Add an image for the markdown case", {image: png(8, 8, (0x2f, 0x81, 0xf7))})
    fx.start_branch(f"case/{case}")
    fx.commit("Add markdown notes", {"markdown/notes.md": "# Notes\n\nThe PR description is what matters here.\n"})
    fx.push_branch(f"case/{case}")

    versions = fx.prs["versions-pushes"]
    comments_pr = fx.prs["comments"]
    body = lines([
        "This description uses every markdown feature grenadine renders.",
        "",
        "## Text",
        "",
        "*Emphasis*, **strong**, ~~strikethrough~~ and `inline code`.",
        "A soft line break follows,",
        "and should render as a line break.",
        "",
        "> A blockquote.",
        "",
        "## Tables",
        "",
        "| Feature | Supported | Notes |",
        "| :------ | :-------: | ----: |",
        "| Tables  | yes       | 1     |",
        "| Tasks   | yes       | 2     |",
        "",
        "## Task list",
        "",
        "- [x] A finished task",
        "- [ ] An open task",
        "",
        "## Code",
        "",
        "```rust",
        "fn main() {",
        '    println!("highlighted");',
        "}",
        "```",
        "",
        "```python",
        "print('highlighted too')",
        "```",
        "",
        "```",
        "no language at all",
        "```",
        "",
        "## Links",
        "",
        f"- Mentions: @{fx.other} and @{fx.owner}/{fx.team_slug}",
        f"- Same-repo PR: #{versions.number}",
        f"- Cross-repo PR: {fx.slug}#{versions.number}",
        f"- Full PR URL, which should open in grenadine: {comments_pr.url}",
        f"- Commit: {main_sha}",
        "- Bare URL: https://example.com/path?query=1",
        "- Email: someone@example.com",
        "- [An explicit link](https://github.com)",
        "",
        "## Emoji",
        "",
        ":tada: :+1: :rocket:",
        "",
        "## Image",
        "",
        f"![A blue square](https://raw.githubusercontent.com/{fx.slug}/main/{image})",
        "",
        "## Footnotes",
        "",
        "A claim that needs a source.[^source]",
        "",
        "[^source]: The source.",
        "",
        "## Unsafe HTML",
        "",
        "<details><summary>Collapsed</summary>Hidden text.</details>",
        "",
        "<script>alert('this should be sanitized away')</script>",
        "",
        '<a href="javascript:alert(1)">A javascript: link that should be neutralized</a>',
    ])
    fx.open_pr(case, "Describe a PR with every markdown feature", body,
               expected_inbox=WAITING_FOR_REVIEWERS,
               notes="The PR description exercises the markdown renderer: tables, task lists, "
                     "footnotes, strikethrough, code blocks with syntax highlighting, an image, "
                     "soft breaks, autolinked mentions, issue and PR references, commits, "
                     "emoji shortcodes, a PR link that grenadine should open itself, and "
                     "HTML that must be sanitized.")


def write_manifest(fx: Fixture, path: Path) -> None:
    manifest = {
        "repo": fx.slug,
        "clone_dir": str(fx.clone),
        "me": fx.me,
        "other_user": fx.other,
        "team": f"{fx.owner}/{fx.team_slug}",
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "merge_queue_expires_minutes": HOLD_TIMEOUT_MINUTES,
        "inboxes": [NEEDS_REVIEW, NEEDS_TEAM_REVIEW, RETURNED, APPROVED, WAITING_FOR_REVIEWERS,
                    DRAFTS, MERGING, DRAFTS_NEEDING_REVIEW, WAITING_FOR_AUTHORS],
        "cases": [dataclasses.asdict(pr) for pr in fx.prs.values()],
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def resolve(path: str) -> Path:
    """Resolves `path` against the directory `bazel run` was invoked from."""
    return (Path(os.environ.get("BUILD_WORKING_DIRECTORY", ".")) / path).resolve()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--owner", default="BungeeSC", help="The org that owns the repo.")
    parser.add_argument("--name", default="grenadine-test", help="The repo's name.")
    parser.add_argument("--other-user", required=True,
                        help="A second gh account that opens and reviews PRs.")
    parser.add_argument("--clone-dir", required=True, type=resolve,
                        help="Where to leave a clone of the repo, for grenadine's --repo.")
    parser.add_argument("--manifest", type=resolve,
                        help="Where to write the manifest. Defaults to NAME-manifest.json next to --clone-dir.")
    parser.add_argument("--recreate", action="store_true",
                        help="Delete the repo and the clone if they already exist.")
    args = parser.parse_args()
    manifest = args.manifest or args.clone_dir.parent / f"{args.name}-manifest.json"

    try:
        me, me_token, other_token = preflight(args)
        fx = Fixture(args, me, me_token, other_token)
        prepare_clone_dir(fx, args.recreate)
        create_repo(fx, args.recreate)
        set_up_team(fx)
        seed_main(fx)

        log("Opening PRs")
        inbox_cases(fx)
        versions_pushes(fx)
        versions_rebase(fx, "versions-rebase", edit_after_rebase=False)
        versions_rebase(fx, "versions-rebase-and-edit", edit_after_rebase=True)
        diff_file_kinds(fx)
        diff_inline(fx)
        diff_syntax(fx)
        diff_hunks(fx)
        comments(fx)
        markdown_description(fx)

        # Last, because the ruleset blocks pushes to main.
        enable_merge_queue(fx)
        write_manifest(fx, manifest)
    except Failure as e:
        print(f"error: {e}", file=sys.stderr)
        return 1

    log(f"\nCreated {len(fx.prs)} PRs in https://github.com/{fx.slug}")
    log(f"Manifest: {manifest}")
    log(f"The merging PR leaves the merge queue in {HOLD_TIMEOUT_MINUTES} minutes.")
    log(f"Run grenadine with: bazel run //grenadine/server -- --repo={fx.clone}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
