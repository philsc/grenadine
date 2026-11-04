"""A fake GitHub for running grenadine's server in tests.

It serves the GraphQL and REST endpoints that grenadine/server/src/github.rs
calls, and keeps each repository's git data in a local bare repo. Point the
server at it with `--github-api=URL` and run the server's git with
`git_env()`, which redirects https://github.com/ to the bare repos.

Build a scenario by pushing branches and opening PRs:

    gh = FakeGitHub(tmp)
    repo = gh.repo("test/test")
    repo.commit("Seed", {"README.md": "hi"})
    repo.push("main")
    repo.checkout("feature")
    repo.commit("Add a thing", {"thing.txt": "thing"})
    repo.push("feature")
    pr = repo.open_pr("Add a thing", head="feature")
    pr.reviews.append(Review("other", APPROVED))

Inbox searches are answered by `matches`, which understands the qualifiers
grenadine's default inboxes use and copies the GitHub behaviors they depend
on, warts included: a PR whose only reviews are comments counts as
`review:none`.
"""

import dataclasses
import datetime
import json
import os
import re
import subprocess
import threading
import time
import urllib.parse
from collections.abc import Callable
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

from graphql import build_schema, graphql_sync

TOKEN = "dummy-token"
# Who `@me` is: the user TOKEN belongs to.
VIEWER = "me"
ZERO_SHA = "0" * 40
# When the first thing in a scenario happens. Every push, PR and comment
# advances the clock, so their times are distinct and ordered.
EPOCH = 1_767_225_600  # 2026-01-01T00:00:00Z

APPROVED = "APPROVED"
CHANGES_REQUESTED = "CHANGES_REQUESTED"
COMMENTED = "COMMENTED"
REVIEW_REQUIRED = "REVIEW_REQUIRED"
# The name of the team whose members `team_review_requests` holds.
TEAM = "Reviewers"
DISMISSED = "DISMISSED"
PENDING = "PENDING"

OPEN = "OPEN"
CLOSED = "CLOSED"
MERGED = "MERGED"

SCHEMA = build_schema(Path(__file__).with_name("github_schema.graphql").read_text())


def iso(t: int) -> str:
    return datetime.datetime.fromtimestamp(t, datetime.UTC).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )


@dataclasses.dataclass
class Push:
    before: str
    after: str
    at: int
    actor: str
    force: bool


@dataclasses.dataclass
class Review:
    author: str
    state: str
    body: str = ""
    # When it was submitted; 0 means at EPOCH.
    submitted_at: int = 0
    # The PR head the review was submitted against.
    commit: str | None = None


@dataclasses.dataclass
class IssueComment:
    """A comment on the PR itself rather than on its code."""

    author: str
    body: str
    created_at: int


@dataclasses.dataclass
class CheckRun:
    name: str
    # A CheckStatusState; only COMPLETED runs have a conclusion.
    status: str = "COMPLETED"
    conclusion: str | None = "SUCCESS"
    # When it started; None for one that hasn't.
    started_at: int | None = None
    # The workflow the run belongs to; None for a run outside Actions.
    workflow: str | None = None


@dataclasses.dataclass
class Comment:
    id: int
    author: str
    body: str
    path: str
    commit: str
    line: int
    created_at: int
    in_reply_to: int | None = None
    side: str = "RIGHT"
    resolved: bool = False
    resolved_by: str | None = None
    # Whether the viewer may resolve the thread this comment starts, or
    # unresolve it.
    can_resolve: bool = True
    # On the whole file rather than on `line`.
    on_file: bool = False


@dataclasses.dataclass
class PendingReview:
    """The viewer's review before it's submitted. Its comments aren't in
    the PR's comments until then."""

    id: str
    comments: list[Comment] = dataclasses.field(default_factory=list)


@dataclasses.dataclass
class PullRequest:
    repo: "FakeRepo"
    number: int
    title: str
    body: str
    author: str
    base: str
    head: str
    created_at: int
    updated_at: int
    state: str = OPEN
    draft: bool = False
    merged_at: int | None = None
    # Conflicts with its base.
    conflicting: bool = False
    # In a merge queue.
    queued: bool = False
    # Users asked to review.
    review_requests: set[str] = dataclasses.field(default_factory=set)
    # Members of the teams asked to review; a request goes to TEAM.
    team_review_requests: set[str] = dataclasses.field(default_factory=set)
    # Branch protection requires an approving review.
    review_required: bool = False
    reviews: list[Review] = dataclasses.field(default_factory=list)
    comments: list[Comment] = dataclasses.field(default_factory=list)
    # The check runs on the head, including ones later runs superseded.
    check_runs: list[CheckRun] = dataclasses.field(default_factory=list)
    issue_comments: list[IssueComment] = dataclasses.field(default_factory=list)
    # GitHub's MergeStateStatus; None works it out from the PR.
    merge_state: str | None = None
    # The base branch requires conversations to be resolved: "protection"
    # for classic branch protection, "ruleset" for a ruleset.
    thread_resolution: str | None = None
    # The viewer may merge it bypassing the branch's rules.
    can_merge_as_admin: bool = False
    pending: PendingReview | None = None
    # How it was merged through the API: the method, the commit's title and
    # body.
    merged_with: tuple[str, str | None, str | None] | None = None

    @property
    def url(self) -> str:
        return f"https://github.com/{self.repo.slug}/pull/{self.number}"

    @property
    def head_oid(self) -> str:
        return self.repo.tip(self.head)

    def touch(self) -> None:
        self.updated_at = self.repo.gh.tick()

    def submit_review(self, author: str, state: str, body: str = "") -> None:
        """Adds a review by `author`. Like GitHub, submitting a review
        removes the author's pending review request."""
        self.reviews.append(Review(author, state, body=body, commit=self.head_oid))
        self.review_requests.discard(author)
        self.touch()

    def review_decision(self) -> str | None:
        """APPROVED or CHANGES_REQUESTED from each reviewer's latest
        verdict, else REVIEW_REQUIRED when a review is required, else None.
        Comment-only reviews carry no verdict."""
        latest: dict[str, str] = {}
        for r in self.reviews:
            if r.state != COMMENTED:
                latest[r.author] = r.state
        if CHANGES_REQUESTED in latest.values():
            return CHANGES_REQUESTED
        if APPROVED in latest.values():
            return APPROVED
        return REVIEW_REQUIRED if self.review_required else None

    def checks(self) -> str | None:
        """The StatusState of the checks on the head; None when it has
        none. Like GitHub, it counts the runs that later runs superseded."""
        if not self.check_runs:
            return None
        if any(
            r.conclusion
            in (
                "FAILURE",
                "TIMED_OUT",
                "CANCELLED",
                "ACTION_REQUIRED",
                "STARTUP_FAILURE",
            )
            for r in self.check_runs
        ):
            return "FAILURE"
        if any(r.status != "COMPLETED" for r in self.check_runs):
            return "PENDING"
        return "SUCCESS"

    def merge_state_status(self) -> str:
        if self.merge_state is not None:
            return self.merge_state
        if self.conflicting:
            return "DIRTY"
        if self.draft:
            return "DRAFT"
        if self.thread_resolution and any(
            c.in_reply_to is None and not c.resolved for c in self.comments
        ):
            return "BLOCKED"
        if self.checks() == "FAILURE":
            return "UNSTABLE"
        return "CLEAN"


class FakeRepo:
    """A GitHub repository: a bare repo that fetches go to, a work tree to
    build commits in, and the repository's PRs."""

    def __init__(self, gh: "FakeGitHub", slug: str):
        self.gh = gh
        self.slug = slug
        self.bare = gh.root / "remotes" / f"{slug}.git"
        self.work = gh.root / "work" / slug
        self.pushes: dict[str, list[Push]] = {}
        self.prs: dict[int, PullRequest] = {}
        # The PullRequestMergeMethods the repository allows, the first one
        # being the viewer's default.
        self.merge_methods = ["MERGE", "SQUASH", "REBASE"]
        self.delete_branch_on_merge = False
        # Branches deleted through the API, and ones that deleting fails
        # for.
        self.deleted_branches: set[str] = set()
        # GitHub's stacks API: False answers 404 for it, and each stack
        # is a number mapped to its PRs, bottom first.
        self.stacks_enabled = True
        self.stacks: dict[int, list[int]] = {}
        self.undeletable_branches: set[str] = set()
        self.bare.parent.mkdir(parents=True, exist_ok=True)
        self.work.parent.mkdir(parents=True, exist_ok=True)
        gh.git("init", "--quiet", "--bare", "--initial-branch=main", str(self.bare))
        # grenadine fetches force-pushed-away commits by SHA.
        gh.git("-C", str(self.bare), "config", "uploadpack.allowAnySHA1InWant", "true")
        gh.git("init", "--quiet", "--initial-branch=main", str(self.work))
        self.git("remote", "add", "origin", str(self.bare))

    def git(self, *args: str, env: dict[str, str] | None = None) -> str:
        return self.gh.git("-C", str(self.work), *args, env=env)

    def tip(self, branch: str) -> str:
        pushes = self.pushes.get(branch)
        return pushes[-1].after if pushes else ""

    def checkout(self, branch: str, start: str | None = None) -> None:
        """Switches the work tree to `branch`. With `start`, (re)creates the
        branch there; otherwise an existing branch is kept as is and a new
        one starts at the current commit."""
        if start is not None:
            self.git("checkout", "--quiet", "-B", branch, start)
        else:
            exists = (
                subprocess.run(
                    [
                        "git",
                        "-C",
                        str(self.work),
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        f"refs/heads/{branch}",
                    ],
                    env=self.gh.git_env(),
                    capture_output=True,
                    check=False,
                ).returncode
                == 0
            )
            self.git("checkout", "--quiet", *([] if exists else ["-b"]), branch)

    def commit(
        self,
        message: str,
        files: dict[str, str | None],
        *,
        author: str = VIEWER,
        amend: bool = False,
    ) -> str:
        """Writes `files` (None deletes one) and commits them as `author`."""
        for path, content in files.items():
            p = self.work / path
            if content is None:
                self.git("rm", "--quiet", path)
                continue
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(content)
            self.git("add", path)
        args = ["commit", "--quiet", "--allow-empty", "-m", message]
        if amend:
            args.append("--amend")
        self.git(*args, env=self._identity(author, author=True))
        return self.git("rev-parse", "HEAD").strip()

    def rebase(self, onto: str, *, by: str = VIEWER) -> str:
        """Rebases the current branch onto `onto`, committed by `by`."""
        self.git("rebase", "--quiet", onto, env=self._identity(by, author=False))
        return self.git("rev-parse", "HEAD").strip()

    def _identity(self, login: str, *, author: bool) -> dict[str, str]:
        """The environment that makes `login` the committer, and the author
        too if `author`, at the next tick."""
        when = f"@{self.gh.tick()} +0000"
        roles = ["COMMITTER", "AUTHOR"] if author else ["COMMITTER"]
        env = {}
        for role in roles:
            env[f"GIT_{role}_NAME"] = self.gh.users.get(login, login)
            env[f"GIT_{role}_EMAIL"] = f"{login}@users.noreply.github.com"
            env[f"GIT_{role}_DATE"] = when
        return env

    def push(self, branch: str, *, actor: str = VIEWER) -> str:
        """Pushes the work tree's `branch`, force-pushing if needed, and
        logs the push in the branch's activity."""
        after = self.git("rev-parse", branch).strip()
        before = self.tip(branch) or ZERO_SHA
        if after == before:
            return after
        force = (
            before != ZERO_SHA
            and subprocess.run(
                [
                    "git",
                    "-C",
                    str(self.work),
                    "merge-base",
                    "--is-ancestor",
                    before,
                    after,
                ],
                env=self.gh.git_env(),
                check=False,
            ).returncode
            != 0
        )
        if force:
            # Keep the old tip so it can still be fetched by SHA.
            self.git("push", "--quiet", "origin", f"{before}:refs/keep/{before}")
        self.git(
            "push", "--quiet", "--force", "origin", f"{branch}:refs/heads/{branch}"
        )
        self.pushes.setdefault(branch, []).append(
            Push(
                before=before, after=after, at=self.gh.tick(), actor=actor, force=force
            )
        )
        for pr in self.prs.values():
            if pr.head == branch:
                pr.touch()
        return after

    def open_pr(
        self,
        title: str,
        *,
        head: str,
        base: str = "main",
        body: str = "",
        author: str = VIEWER,
        draft: bool = False,
    ) -> PullRequest:
        if not self.tip(head):
            raise ValueError(f"push {head} before opening a PR from it")
        number = self.gh.next_number()
        now = self.gh.tick()
        pr = PullRequest(
            repo=self,
            number=number,
            title=title,
            body=body,
            author=author,
            base=base,
            head=head,
            created_at=now,
            updated_at=now,
            draft=draft,
        )
        self.prs[number] = pr
        return pr

    def merge(self, pr: PullRequest) -> None:
        """Marks `pr` merged. The base branch doesn't change."""
        pr.state = MERGED
        pr.merged_at = self.gh.tick()
        pr.updated_at = pr.merged_at

    def close(self, pr: PullRequest) -> None:
        pr.state = CLOSED
        pr.touch()

    def review(
        self, pr: PullRequest, *, author: str, state: str, body: str = ""
    ) -> Review:
        r = Review(
            author=author,
            state=state,
            body=body,
            submitted_at=self.gh.tick(),
            commit=pr.head_oid,
        )
        pr.reviews.append(r)
        pr.review_requests.discard(author)
        pr.touch()
        return r

    def issue_comment(self, pr: PullRequest, *, author: str, body: str) -> IssueComment:
        c = IssueComment(author=author, body=body, created_at=self.gh.tick())
        pr.issue_comments.append(c)
        pr.touch()
        return c

    def comment(
        self,
        pr: PullRequest,
        *,
        author: str,
        body: str,
        path: str,
        line: int,
        commit: str | None = None,
        reply_to: Comment | None = None,
    ) -> Comment:
        c = Comment(
            id=self.gh.next_comment_id(),
            author=author,
            body=body,
            path=path,
            commit=commit or pr.head_oid,
            line=line,
            created_at=self.gh.tick(),
            in_reply_to=reply_to.id if reply_to else None,
        )
        pr.comments.append(c)
        pr.touch()
        return c

    def commits(self, pr: PullRequest) -> list[str]:
        """The PR's commits, oldest first, as GitHub lists them."""
        base = self.tip(pr.base)
        rng = f"{base}..{pr.head_oid}" if base else pr.head_oid
        out = self.gh.git("-C", str(self.bare), "rev-list", "--reverse", rng)
        return out.split()

    def commit_author(self, sha: str) -> tuple[str, str] | None:
        """The (name, login) of a commit's author, or None for an unknown SHA."""
        try:
            out = self.gh.git(
                "-C", str(self.bare), "log", "-1", "--format=%an%x00%ae", sha
            )
        except subprocess.CalledProcessError:
            return None
        name, email = out.strip().split("\0")
        return name, email.removesuffix("@users.noreply.github.com")


class FakeGitHub:
    def __init__(self, root: Path):
        self.root = root
        self.root.mkdir(parents=True, exist_ok=True)
        self.repos: dict[str, FakeRepo] = {}
        # Logins and display names.
        self.users: dict[str, str] = {VIEWER: "Me Myself", "other": "Other Person"}
        self.clock = EPOCH
        self._number = 0
        self._comment_id = 1000
        self.lock = threading.Lock()
        # Every request served, as "METHOD path", to help debug failures.
        self.requests: list[str] = []
        # When set, the next mutation fails with this message and changes
        # nothing.
        self.fail_next_mutation: str | None = None
        # What the x-ratelimit-* headers report: `used` is
        # limit - remaining and `reset` only moves when
        # `set_rate_limit` moves it.
        self.rate_limit = {
            "limit": 5000,
            "remaining": 5000,
            "reset": int(time.time()) + 3600,
        }
        # See `hold_next_mutation`.
        self._hold: tuple[threading.Event, threading.Event] | None = None
        self.gitconfig = root / "gitconfig"
        self.gitconfig.write_text(
            f'[url "file://{root / "remotes"}/"]\n'
            "\tinsteadOf = https://github.com/\n"
            "[user]\n\tname = Test\n\temail = test@example.com\n"
        )
        self._server: ThreadingHTTPServer | None = None

    def __getstate__(self) -> dict[str, Any]:
        assert self._server is None and self._hold is None
        return {k: v for k, v in self.__dict__.items() if k != "lock"}

    def __setstate__(self, state: dict[str, Any]) -> None:
        # Locks can't be copied, so a copy starts with a fresh one.
        self.__dict__.update(state)
        self.lock = threading.Lock()

    def hold_next_mutation(self) -> tuple[threading.Event, threading.Event]:
        """Makes the next mutation wait before it's made, e.g. to change the
        PR while a change from the page is in flight. Returns two events:
        the first is set once the mutation waits, and setting the second
        lets it go on."""
        self._hold = threading.Event(), threading.Event()
        return self._hold

    def set_rate_limit(self, remaining: int, reset_in: int | None = None) -> None:
        """Sets the budget the headers report; 0 answers every request
        with a rate-limit error. `reset_in` moves the window's end to
        now + reset_in seconds; raising the budget without it starts a
        fresh window, the way GitHub refills when the old one ends."""
        with self.lock:
            if reset_in is not None:
                self.rate_limit["reset"] = int(time.time()) + reset_in
            elif remaining > self.rate_limit["remaining"]:
                self.rate_limit["reset"] = int(time.time()) + 3600
            self.rate_limit["remaining"] = remaining

    def git_env(self) -> dict[str, str]:
        """The environment for git, the server's included: no system or user
        config, and fetches from github.com go to the bare repos."""
        return dict(
            os.environ, GIT_CONFIG_GLOBAL=str(self.gitconfig), GIT_CONFIG_NOSYSTEM="1"
        )

    def git(self, *args: str, env: dict[str, str] | None = None) -> str:
        return subprocess.run(
            ["git", *args],
            env=dict(self.git_env(), **(env or {})),
            check=True,
            capture_output=True,
            text=True,
        ).stdout

    def tick(self) -> int:
        self.clock += 60
        return self.clock

    def next_number(self) -> int:
        self._number += 1
        return self._number

    def next_comment_id(self) -> int:
        self._comment_id += 1
        return self._comment_id

    def repo(self, slug: str) -> FakeRepo:
        if slug not in self.repos:
            self.repos[slug] = FakeRepo(self, slug)
        return self.repos[slug]

    def prs(self) -> list[PullRequest]:
        return [pr for r in self.repos.values() for pr in r.prs.values()]

    # Serving.

    def start(self) -> str:
        """Starts serving on a free port and returns the API's base URL."""
        gh = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                gh._handle(self, "GET")

            def do_POST(self) -> None:
                gh._handle(self, "POST")

            def log_message(self, format: str, *args: Any) -> None:
                pass

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._server.daemon_threads = True
        threading.Thread(target=self._server.serve_forever, daemon=True).start()
        return f"http://127.0.0.1:{self._server.server_address[1]}"

    def stop(self) -> None:
        if self._server:
            self._server.shutdown()
            self._server.server_close()

    def _handle(self, req: BaseHTTPRequestHandler, method: str) -> None:
        url = urllib.parse.urlsplit(req.path)
        self.requests.append(f"{method} {url.path}")
        if req.headers.get("Authorization") != f"Bearer {TOKEN}":
            return _reply(req, 401, {"message": "Bad credentials"}, self.rate_limit)
        body = req.rfile.read(int(req.headers.get("Content-Length") or 0))
        if self._hold and _is_mutation(method, url.path, body):
            (held, release), self._hold = self._hold, None
            held.set()
            # Outside the lock, so that other requests go on meanwhile.
            release.wait(timeout=60)
        with self.lock:
            try:
                status, payload = self._route(
                    method,
                    url.path,
                    urllib.parse.parse_qs(url.query),
                    body,
                    req.headers.get("X-GitHub-Api-Version"),
                )
            except Exception as e:  # noqa: BLE001 - a fake should report, not hang up.
                status, payload = 500, {"message": f"fake GitHub: {e!r}"}
        _reply(req, status, payload, self.rate_limit)

    def _route(
        self,
        method: str,
        path: str,
        query: dict[str, list[str]],
        body: bytes,
        api_version: str | None,
    ) -> tuple[int, Any]:
        if self.rate_limit["remaining"] == 0:
            msg = "API rate limit already exceeded for user ID 1."
            if method == "POST" and path == "/graphql":
                return 200, {"errors": [{"type": "RATE_LIMITED", "message": msg}]}
            return 403, {"message": msg}
        if method == "POST" and path == "/graphql":
            return 200, self.graphql(json.loads(body))
        if method == "GET" and (
            m := re.fullmatch(r"/repos/([^/]+/[^/]+)/activity", path)
        ):
            return self._activity(m[1], query)
        if method == "GET" and (
            m := re.fullmatch(r"/repos/([^/]+/[^/]+)/pulls/(\d+)/comments", path)
        ):
            return self._review_comments(m[1], int(m[2]))
        if method == "POST" and (
            m := re.fullmatch(r"/repos/([^/]+/[^/]+)/pulls", path)
        ):
            return self._create_pr(m[1], json.loads(body))
        if method == "POST" and (
            m := re.fullmatch(r"/repos/([^/]+/[^/]+)/issues/(\d+)/comments", path)
        ):
            return self._create_issue_comment(m[1], int(m[2]), json.loads(body))
        if (m := re.fullmatch(r"/repos/([^/]+/[^/]+)/stacks", path)) is not None:
            if method == "GET":
                return self._get_stacks(m[1], query, api_version)
            if method == "POST":
                return self._create_stack(m[1], json.loads(body), api_version)
        if method == "POST" and (
            m := re.fullmatch(r"/repos/([^/]+/[^/]+)/stacks/(\d+)/(add|unstack)", path)
        ):
            req = json.loads(body) if body else {}
            return self._change_stack(m[1], int(m[2]), m[3], req, api_version)
        return 404, {"message": "Not Found"}

    def _create_issue_comment(
        self, slug: str, number: int, req: dict[str, Any]
    ) -> tuple[int, Any]:
        repo = self.repos.get(slug)
        pr = repo.prs.get(number) if repo else None
        if pr is None:
            return 404, {"message": "Not Found"}
        c = repo.issue_comment(pr, author=VIEWER, body=req["body"])
        return 201, {"body": c.body, "user": {"login": VIEWER}}

    def _create_pr(self, slug: str, req: dict[str, Any]) -> tuple[int, Any]:
        repo = self.repos.get(slug)
        if repo is None:
            return 404, {"message": "Not Found"}
        head = req["head"]
        try:
            after = self.git(
                "-C", str(repo.bare), "rev-parse", "--verify", f"refs/heads/{head}"
            ).strip()
        except subprocess.CalledProcessError:
            return 422, {
                "message": "Validation Failed",
                "errors": [{"message": f"No commits on {head}"}],
            }
        # The branch was pushed straight to the bare repo; log it like a push.
        before = repo.tip(head) or ZERO_SHA
        if after != before:
            repo.pushes.setdefault(head, []).append(
                Push(
                    before=before,
                    after=after,
                    at=self.tick(),
                    actor=VIEWER,
                    force=False,
                )
            )
        pr = repo.open_pr(
            req["title"], head=head, base=req["base"], body=req.get("body", "")
        )
        return 201, {"number": pr.number, "html_url": pr.url}

    def _activity(self, slug: str, query: dict[str, list[str]]) -> tuple[int, Any]:
        repo = self.repos.get(slug)
        ref = query.get("ref", [""])[0]
        if repo is None or not ref.startswith("refs/heads/"):
            return 404, {"message": "Not Found"}
        out = []
        for p in repo.pushes.get(ref.removeprefix("refs/heads/"), []):
            kind = (
                "branch_creation"
                if p.before == ZERO_SHA
                else "force_push"
                if p.force
                else "push"
            )
            out.append(
                {
                    "before": p.before,
                    "after": p.after,
                    "ref": ref,
                    "timestamp": iso(p.at),
                    "activity_type": kind,
                    "actor": {"login": p.actor, "avatar_url": None},
                }
            )
        if query.get("direction", ["desc"])[0] != "asc":
            out.reverse()
        return 200, out

    def _review_comments(self, slug: str, number: int) -> tuple[int, Any]:
        repo = self.repos.get(slug)
        pr = repo and repo.prs.get(number)
        if pr is None:
            return 404, {"message": "Not Found"}
        return 200, [
            {
                "id": c.id,
                "in_reply_to_id": c.in_reply_to,
                "user": {"login": c.author, "avatar_url": None},
                "body": c.body,
                "path": c.path,
                "commit_id": c.commit,
                "original_commit_id": c.commit,
                "original_line": None if c.on_file else c.line,
                "original_start_line": None,
                "line": None if c.on_file else c.line,
                "start_line": None,
                "side": c.side,
                "subject_type": "file" if c.on_file else "line",
                "created_at": iso(c.created_at),
                "html_url": f"{pr.url}#discussion_r{c.id}",
            }
            for c in pr.comments
        ]

    # Stacks.

    def _stacks_repo(self, slug: str, api_version: str | None) -> FakeRepo | None:
        repo = self.repos.get(slug)
        if repo is None or not repo.stacks_enabled or api_version != "2026-03-10":
            return None
        return repo

    def _stack_json(self, repo: FakeRepo, number: int) -> dict[str, Any]:
        prs = [repo.prs[n] for n in repo.stacks[number]]
        return {
            "number": number,
            "open": any(p.state == OPEN and p.merged_at is None for p in prs),
            "base": {"ref": prs[0].base},
            "pull_requests": [
                {
                    "number": p.number,
                    "state": "open" if p.state == OPEN else "closed",
                    "draft": p.draft,
                    "merged_at": iso(p.merged_at) if p.merged_at else None,
                    "head": {"ref": p.head, "sha": p.head_oid},
                }
                for p in prs
            ],
        }

    def _get_stacks(
        self, slug: str, query: dict[str, list[str]], api_version: str | None
    ) -> tuple[int, Any]:
        repo = self._stacks_repo(slug, api_version)
        if repo is None:
            return 404, {"message": "Not Found"}
        n = int(query.get("pull_request", ["0"])[0])
        return 200, [
            self._stack_json(repo, number)
            for number, prs in repo.stacks.items()
            if n in prs
        ]

    def _stack_prs(
        self, repo: FakeRepo, numbers: list[int]
    ) -> tuple[list[PullRequest] | None, tuple[int, Any] | None]:
        """The resolved PRs of a create/add request, or its error."""
        prs = [repo.prs.get(n) for n in numbers]
        if not prs or any(p is None or p.state != OPEN for p in prs):
            return None, (422, {"message": "every PR must exist and be open"})
        in_a_stack = {n for stack in repo.stacks.values() for n in stack}
        if any(n in in_a_stack for n in numbers):
            return None, (422, {"message": "a PR is already in a stack"})
        return prs, None  # type: ignore[return-value]

    def _create_stack(
        self, slug: str, req: dict[str, Any], api_version: str | None
    ) -> tuple[int, Any]:
        repo = self._stacks_repo(slug, api_version)
        if repo is None:
            return 404, {"message": "Not Found"}
        numbers = req["pull_requests"]
        if len(numbers) < 2:
            return 422, {"message": "a stack needs at least two pull requests"}
        prs, err = self._stack_prs(repo, numbers)
        if err:
            return err
        for above, below in zip(prs[1:], prs):
            if above.base != below.head:
                return 422, {
                    "message": f"#{above.number}'s base is not #{below.number}'s head"
                }
        number = self.next_number()
        repo.stacks[number] = list(numbers)
        return 201, self._stack_json(repo, number)

    def _change_stack(
        self,
        slug: str,
        number: int,
        action: str,
        req: dict[str, Any],
        api_version: str | None,
    ) -> tuple[int, Any]:
        repo = self._stacks_repo(slug, api_version)
        if repo is None or number not in repo.stacks:
            return 404, {"message": "Not Found"}
        if action == "add":
            numbers = req["pull_requests"]
            prs, err = self._stack_prs(repo, numbers)
            if err:
                return err
            top = repo.prs[repo.stacks[number][-1]]
            if prs[0].base != top.head:
                return 409, {
                    "message": f"#{prs[0].number}'s base is not the stack's top"
                }
            repo.stacks[number] += numbers
            return 200, self._stack_json(repo, number)
        # unstack: the unmerged PRs leave; the merged ones keep the stack.
        remaining = [n for n in repo.stacks[number] if repo.prs[n].state == MERGED]
        if len(remaining) == len(repo.stacks[number]):
            return 422, {"message": "nothing could be unstacked"}
        if not remaining:
            del repo.stacks[number]
            return 204, None
        repo.stacks[number] = remaining
        return 200, self._stack_json(repo, number)

    def _in_open_stack(self, pr: PullRequest) -> bool:
        """Whether `pr` is in a stack with unmerged PRs left, where
        GitHub is assumed to forbid changing its base or closing it."""
        return any(
            pr.number in stack and any(pr.repo.prs[n].state != MERGED for n in stack)
            for stack in pr.repo.stacks.values()
        )

    # GraphQL.

    def graphql(self, request: dict[str, Any]) -> dict[str, Any]:
        result = graphql_sync(
            SCHEMA,
            request["query"],
            root_value=self._root(),
            variable_values=request.get("variables"),
        )
        out: dict[str, Any] = {"data": result.data}
        if result.errors:
            out["errors"] = [e.formatted for e in result.errors]
        return out

    def _root(self) -> dict[str, Any]:
        def search(
            info: Any,
            type: str,
            query: str,
            first: int | None = None,
            after: str | None = None,
            **_: Any,
        ) -> Any:
            # The cursor is how many hits came before this page.
            start = int(after) if after else 0
            found = search_prs(self.prs(), query)
            hits = found[start : start + (first or 100)]
            end = start + len(hits)
            return {
                "issueCount": len(found),
                "pageInfo": {
                    "hasNextPage": end < len(found),
                    "endCursor": str(end),
                },
                "nodes": [self._pr_node(pr) for pr in hits],
            }

        def repository(info: Any, owner: str, name: str) -> Any:
            repo = self.repos.get(f"{owner}/{name}")
            if repo is None:
                raise LookupError(
                    f"Could not resolve to a Repository with the name '{owner}/{name}'."
                )
            return self._repo_node(repo)

        def rate_limit(info: Any) -> Any:
            return {
                "cost": 1,
                "limit": self.rate_limit["limit"],
                "nodeCount": 0,
                "remaining": self.rate_limit["remaining"],
                "resetAt": iso(self.rate_limit["reset"]),
                "used": self.rate_limit["limit"] - self.rate_limit["remaining"],
            }

        def user(info: Any, login: str) -> Any:
            if login not in self.users:
                raise LookupError(
                    f"Could not resolve to a User with the login of '{login}'."
                )
            return self._user_node(login)

        def mutation(f: Callable[..., Any]) -> Callable[..., Any]:
            def run(info: Any, input: dict[str, Any]) -> Any:
                error, self.fail_next_mutation = self.fail_next_mutation, None
                if error is not None:
                    raise PermissionError(error)
                return f(info, input)

            return run

        def add_comment(info: Any, input: dict[str, Any]) -> Any:
            pr = self._pr_by_id(input["subjectId"])
            pr.repo.issue_comment(pr, author=VIEWER, body=input["body"])
            return {"clientMutationId": input.get("clientMutationId")}

        def add_reply(info: Any, input: dict[str, Any]) -> Any:
            pr, root = self._thread(input["pullRequestReviewThreadId"])
            reply = Comment(
                id=self.next_comment_id(),
                author=VIEWER,
                body=input["body"],
                path=root.path,
                commit=root.commit,
                line=root.line,
                created_at=self.tick(),
                in_reply_to=root.id,
                side=root.side,
                on_file=root.on_file,
            )
            if input.get("pullRequestReviewId"):
                self._pending(input["pullRequestReviewId"])[1].comments.append(reply)
            else:
                pr.comments.append(reply)
            pr.touch()
            return {"clientMutationId": input.get("clientMutationId")}

        def new_thread(pr: PullRequest, input: dict[str, Any]) -> Comment:
            """The first comment of a new thread on `pr`, from the input of
            `addPullRequestReviewThread` or a `DraftPullRequestReviewThread`."""
            on_file = input.get("subjectType") == "FILE"
            if not on_file and not input.get("line"):
                raise ValueError("line is required unless subjectType is FILE")
            return Comment(
                id=self.next_comment_id(),
                author=VIEWER,
                body=input["body"],
                path=input["path"],
                commit=pr.head_oid,
                line=0 if on_file else input["line"],
                created_at=self.tick(),
                side=input.get("side") or "RIGHT",
                on_file=on_file,
            )

        def add_review(info: Any, input: dict[str, Any]) -> Any:
            pr = self._pr_by_id(input["pullRequestId"])
            if pr.pending:
                raise ValueError(
                    "User can only have one pending review per pull request"
                )
            # Made first, so that a bad thread starts no review.
            threads = [new_thread(pr, t) for t in input.get("threads") or []]
            if input.get("event"):
                self._submit(pr, input["event"], input.get("body") or "", threads)
                return {"clientMutationId": None, "pullRequestReview": None}
            pr.pending = PendingReview(
                id=f"PRR_{self.next_comment_id()}", comments=threads
            )
            pr.touch()
            return {
                "clientMutationId": None,
                "pullRequestReview": self._pending_node(pr),
            }

        def add_thread(info: Any, input: dict[str, Any]) -> Any:
            pr, review = self._pending(input["pullRequestReviewId"])
            review.comments.append(new_thread(pr, input))
            pr.touch()
            return {"clientMutationId": None}

        def pending_comment(
            node_id: str,
        ) -> tuple[PullRequest, PendingReview, Comment]:
            for pr in self.prs():
                for c in pr.pending.comments if pr.pending else []:
                    if f"PRRC_{c.id}" == node_id:
                        assert pr.pending
                        return pr, pr.pending, c
            raise LookupError(
                f"Could not resolve to a node with the global id of '{node_id}'"
            )

        def update_comment(info: Any, input: dict[str, Any]) -> Any:
            pr, _, c = pending_comment(input["pullRequestReviewCommentId"])
            c.body = input["body"]
            pr.touch()
            return {"clientMutationId": None}

        def delete_comment(info: Any, input: dict[str, Any]) -> Any:
            pr, review, c = pending_comment(input["id"])
            review.comments.remove(c)
            pr.touch()
            return {"clientMutationId": None}

        def merge_pr(info: Any, input: dict[str, Any]) -> Any:
            pr = self._pr_by_id(input["pullRequestId"])
            if pr.state != OPEN:
                raise ValueError("Pull Request is not open")
            expected = input.get("expectedHeadOid")
            if expected and expected != pr.head_oid:
                raise ValueError(
                    "Head branch was modified. Review and try the merge again."
                )
            if pr.draft:
                raise ValueError("Pull Request is still a draft")
            state = pr.merge_state_status()
            bypass = ("BLOCKED", "BEHIND") if pr.can_merge_as_admin else ()
            if state not in ("CLEAN", "HAS_HOOKS", "UNSTABLE", *bypass):
                raise ValueError("Pull Request is not mergeable")
            method = input.get("mergeMethod") or "MERGE"
            if method not in pr.repo.merge_methods:
                raise ValueError(f"{method} merges are not allowed on this repository")
            pr.merged_with = (
                method,
                input.get("commitHeadline"),
                input.get("commitBody"),
            )
            pr.repo.merge(pr)
            return {"clientMutationId": None}

        def update_pr(info: Any, input: dict[str, Any]) -> Any:
            pr = self._pr_by_id(input["pullRequestId"])
            if pr.state != OPEN:
                raise ValueError("Pull request is closed")
            if self._in_open_stack(pr) and input.get("baseRefName"):
                raise ValueError("Pull request is in a stack")
            base = input.get("baseRefName")
            if base is not None:
                try:
                    self.git(
                        "-C",
                        str(pr.repo.bare),
                        "rev-parse",
                        "--verify",
                        f"refs/heads/{base}",
                    )
                except subprocess.CalledProcessError:
                    raise ValueError(
                        f"Could not resolve to a Ref with the name of '{base}'."
                    ) from None
                pr.base = base
                pr.touch()
            return {"clientMutationId": None}

        def close_pr(info: Any, input: dict[str, Any]) -> Any:
            pr = self._pr_by_id(input["pullRequestId"])
            if pr.state != OPEN:
                raise ValueError("Pull request is closed")
            if self._in_open_stack(pr):
                raise ValueError("Pull request is in a stack")
            pr.repo.close(pr)
            return {"clientMutationId": None}

        def delete_ref(info: Any, input: dict[str, Any]) -> Any:
            for repo in self.repos.values():
                for branch in repo.pushes:
                    if _ref_id(repo, branch) == input["refId"]:
                        if branch in repo.undeletable_branches:
                            raise PermissionError(
                                f"Cannot delete protected branch {branch}"
                            )
                        repo.deleted_branches.add(branch)
                        return {"clientMutationId": None}
            raise LookupError(
                f"Could not resolve to a node with the global id of '{input['refId']}'"
            )

        def submit_review(info: Any, input: dict[str, Any]) -> Any:
            pr, review = self._pending(input["pullRequestReviewId"])
            self._submit(pr, input["event"], input.get("body") or "", review.comments)
            return {"clientMutationId": None}

        def delete_review(info: Any, input: dict[str, Any]) -> Any:
            pr, _ = self._pending(input["pullRequestReviewId"])
            pr.pending = None
            pr.touch()
            return {"clientMutationId": None}

        def resolve(resolved: bool) -> Callable[..., Any]:
            def mutation(info: Any, input: dict[str, Any]) -> Any:
                pr, root = self._thread(input["threadId"])
                if not root.can_resolve:
                    raise PermissionError("Resource not accessible by integration")
                root.resolved = resolved
                root.resolved_by = VIEWER if resolved else None
                pr.touch()
                return {"clientMutationId": input.get("clientMutationId")}

            return mutation

        return {
            "search": search,
            "repository": repository,
            "rateLimit": rate_limit,
            "user": user,
            "viewer": lambda info: self._user_node(VIEWER),
            "addComment": mutation(add_comment),
            "addPullRequestReviewThreadReply": mutation(add_reply),
            "resolveReviewThread": mutation(resolve(True)),
            "unresolveReviewThread": mutation(resolve(False)),
            "addPullRequestReview": mutation(add_review),
            "addPullRequestReviewThread": mutation(add_thread),
            "updatePullRequestReviewComment": mutation(update_comment),
            "deletePullRequestReviewComment": mutation(delete_comment),
            "submitPullRequestReview": mutation(submit_review),
            "deletePullRequestReview": mutation(delete_review),
            "mergePullRequest": mutation(merge_pr),
            "updatePullRequest": mutation(update_pr),
            "closePullRequest": mutation(close_pr),
            "deleteRef": mutation(delete_ref),
        }

    def _pending(self, node_id: str) -> tuple[PullRequest, PendingReview]:
        for pr in self.prs():
            if pr.pending and pr.pending.id == node_id:
                return pr, pr.pending
        raise LookupError(
            f"Could not resolve to a node with the global id of '{node_id}'"
        )

    def _submit(
        self, pr: PullRequest, event: str, body: str, comments: list[Comment]
    ) -> None:
        """Submits the viewer's review of `pr` with `comments`, and ends its
        pending review."""
        state = {
            "COMMENT": COMMENTED,
            "APPROVE": APPROVED,
            "REQUEST_CHANGES": CHANGES_REQUESTED,
        }[event]
        if state != APPROVED and not body and not comments:
            raise ValueError("Review body is required")
        if state != COMMENTED and pr.author == VIEWER:
            raise ValueError(
                "Can not approve or request changes on your own pull request"
            )
        pr.comments.extend(comments)
        pr.pending = None
        pr.repo.review(pr, author=VIEWER, state=state, body=body)

    def _pending_node(self, pr: PullRequest) -> dict[str, Any]:
        """The viewer's pending review of `pr`, as GraphQL shows it."""
        assert pr.pending
        review = pr.pending

        def comment_node(c: Comment) -> dict[str, Any]:
            return {
                "id": f"PRRC_{c.id}",
                "databaseId": c.id,
                "body": c.body,
                "path": c.path,
                "line": None if c.on_file else c.line,
                "startLine": None,
                "originalLine": None if c.on_file else c.line,
                "originalStartLine": None,
                "subjectType": "FILE" if c.on_file else "LINE",
                "createdAt": iso(c.created_at),
                "url": f"{pr.url}#discussion_r{c.id}",
                "replyTo": {"databaseId": c.in_reply_to} if c.in_reply_to else None,
                "originalCommit": {"oid": c.commit},
            }

        return {
            "id": review.id,
            "viewerDidAuthor": True,
            "state": "PENDING",
            "body": "",
            "submittedAt": None,
            "url": f"{pr.url}#pullrequestreview-pending",
            "author": self._user_node(VIEWER),
            "comments": lambda info, **_: {
                "nodes": [comment_node(c) for c in review.comments],
                "totalCount": len(review.comments),
            },
        }

    def _pr_by_id(self, node_id: str) -> PullRequest:
        for pr in self.prs():
            if _pr_id(pr) == node_id:
                return pr
        raise LookupError(
            f"Could not resolve to a node with the global id of '{node_id}'"
        )

    def _thread(self, node_id: str) -> tuple[PullRequest, Comment]:
        """The PR and the root comment of the review thread `node_id`."""
        for pr in self.prs():
            for c in pr.comments:
                if c.in_reply_to is None and f"PRRT_{c.id}" == node_id:
                    return pr, c
        raise LookupError(
            f"Could not resolve to a node with the global id of '{node_id}'"
        )

    def _user_node(self, login: str) -> dict[str, Any]:
        return {
            "__typename": "User",
            "login": login,
            "name": self.users.get(login),
            "avatarUrl": None,
        }

    def _repo_node(self, repo: FakeRepo) -> dict[str, Any]:
        def pull_request(info: Any, number: int) -> Any:
            pr = repo.prs.get(number)
            if pr is None:
                raise LookupError(
                    f"Could not resolve to a PullRequest with the number of {number}."
                )
            return self._pr_node(pr)

        def pull_requests(
            info: Any,
            headRefName: str | None = None,
            baseRefName: str | None = None,
            states: list[str] | None = None,
            first: int | None = None,
            **_: Any,
        ) -> Any:
            found = [
                pr
                for pr in repo.prs.values()
                if (headRefName is None or pr.head == headRefName)
                and (baseRefName is None or pr.base == baseRefName)
                and (states is None or pr.state in states)
            ]
            return {"nodes": [self._pr_node(pr) for pr in found[:first]]}

        def object_(info: Any, oid: str | None = None, **_: Any) -> Any:
            return (
                self._commit_node(repo, oid)
                if oid and repo.commit_author(oid)
                else None
            )

        return {
            "__typename": "Repository",
            "nameWithOwner": repo.slug,
            "isArchived": False,
            "pullRequest": pull_request,
            "pullRequests": pull_requests,
            "object": object_,
            "mergeCommitAllowed": "MERGE" in repo.merge_methods,
            "squashMergeAllowed": "SQUASH" in repo.merge_methods,
            "rebaseMergeAllowed": "REBASE" in repo.merge_methods,
            "viewerDefaultMergeMethod": repo.merge_methods[0],
            "deleteBranchOnMerge": repo.delete_branch_on_merge,
        }

    def _commit_node(
        self, repo: FakeRepo, sha: str, pr: PullRequest | None = None
    ) -> dict[str, Any]:
        """A commit; with `pr`, the PR's head and its checks."""

        def author(info: Any) -> Any:
            found = repo.commit_author(sha)
            if found is None:
                return None
            name, login = found
            return {
                "name": name,
                "email": f"{login}@users.noreply.github.com",
                "user": self._user_node(login) if login in self.users else None,
            }

        return {
            "__typename": "Commit",
            "oid": sha,
            "author": author,
            "checkSuites": lambda info, **_: {"nodes": []},
            "statusCheckRollup": (
                {
                    "state": pr.checks(),
                    "contexts": lambda info, **_: {
                        "nodes": [
                            {
                                "__typename": "CheckRun",
                                "name": r.name,
                                "status": r.status,
                                "conclusion": r.conclusion,
                                "detailsUrl": f"{pr.url}/checks?name={r.name}",
                                "startedAt": (
                                    iso(r.started_at)
                                    if r.started_at is not None
                                    else None
                                ),
                                "checkSuite": {
                                    "workflowRun": r.workflow
                                    and {"workflow": {"name": r.workflow}}
                                },
                            }
                            for r in pr.check_runs
                        ]
                    },
                }
                if pr and pr.check_runs
                else None
            ),
        }

    def _pr_node(self, pr: PullRequest) -> dict[str, Any]:
        repo = pr.repo

        def timeline(
            info: Any,
            itemTypes: list[str] | None = None,
            first: int | None = None,
            **_: Any,
        ) -> Any:
            nodes: list[dict[str, Any]] = []
            if itemTypes is None or "HEAD_REF_FORCE_PUSHED_EVENT" in itemTypes:
                nodes += [
                    {
                        "__typename": "HeadRefForcePushedEvent",
                        "createdAt": iso(p.at),
                        "actor": self._user_node(p.actor),
                        "beforeCommit": self._commit_node(repo, p.before),
                        "afterCommit": self._commit_node(repo, p.after),
                    }
                    for p in repo.pushes.get(pr.head, [])
                    if p.force and p.at >= pr.created_at
                ]
            return {"nodes": nodes[:first]}

        def commits(
            info: Any, first: int | None = None, last: int | None = None, **_: Any
        ) -> Any:
            shas = repo.commits(pr)[:first]
            if last is not None:
                shas = shas[-last:] if last else []
            head = pr.head_oid
            return {
                "nodes": [
                    {
                        "commit": self._commit_node(
                            repo, sha, pr if sha == head else None
                        )
                    }
                    for sha in shas
                ]
            }

        def reviews(
            info: Any,
            first: int | None = None,
            last: int | None = None,
            states: list[str] | None = None,
            **_: Any,
        ) -> Any:
            nodes = [
                {
                    "id": f"PRR_{pr.number}_{i}",
                    "viewerDidAuthor": r.author == VIEWER,
                    "comments": lambda info, **_: {"nodes": [], "totalCount": 0},
                    "state": r.state,
                    "body": r.body,
                    "submittedAt": iso(r.submitted_at or EPOCH),
                    "url": f"{pr.url}#pullrequestreview-{i}",
                    "commit": (self._commit_node(repo, r.commit) if r.commit else None),
                    "author": self._user_node(r.author),
                }
                for i, r in enumerate(pr.reviews)
                if states is None or r.state in states
            ]
            # The viewer's pending review is the latest.
            if pr.pending and (states is None or "PENDING" in states):
                nodes.append(self._pending_node(pr))
            nodes = nodes[:first]
            if last is not None:
                nodes = nodes[-last:] if last else []
            return {"nodes": nodes}

        def comments(
            info: Any, first: int | None = None, last: int | None = None, **_: Any
        ) -> Any:
            found = pr.issue_comments[:first]
            if last is not None:
                found = found[-last:] if last else []
            return {
                "nodes": [
                    {
                        "body": c.body,
                        "createdAt": iso(c.created_at),
                        "url": f"{pr.url}#issuecomment-{i}",
                        "author": self._user_node(c.author),
                    }
                    for i, c in enumerate(found)
                ]
            }

        def review_threads(info: Any, first: int | None = None, **_: Any) -> Any:
            # The viewer sees the threads of their pending review too.
            pending = pr.pending.comments if pr.pending else []
            roots = [c for c in pr.comments + pending if c.in_reply_to is None]
            return {
                "totalCount": len(roots),
                "pageInfo": {"hasNextPage": False, "endCursor": None},
                "nodes": [
                    {
                        "id": f"PRRT_{c.id}",
                        "diffSide": c.side,
                        "isResolved": c.resolved,
                        "viewerCanResolve": c.can_resolve and not c.resolved,
                        "viewerCanUnresolve": c.can_resolve and c.resolved,
                        "resolvedBy": (
                            self._user_node(c.resolved_by)
                            if c.resolved and c.resolved_by
                            else None
                        ),
                        "comments": lambda info, c=c, **_: {
                            "nodes": [{"databaseId": c.id}]
                        },
                    }
                    for c in roots[:first]
                ],
            }

        return {
            "__typename": "PullRequest",
            "id": _pr_id(pr),
            "number": pr.number,
            "title": pr.title,
            "body": pr.body,
            "url": pr.url,
            "state": pr.state,
            "mergeable": "CONFLICTING" if pr.conflicting else "MERGEABLE",
            "isDraft": pr.draft,
            "createdAt": iso(pr.created_at),
            "updatedAt": iso(pr.updated_at),
            "mergedAt": iso(pr.merged_at) if pr.merged_at else None,
            "author": self._user_node(pr.author),
            "repository": lambda info: self._repo_node(repo),
            "baseRefName": pr.base,
            "baseRef": {
                "id": _ref_id(repo, pr.base),
                "name": pr.base,
                "refUpdateRule": {
                    "requiresConversationResolution": pr.thread_resolution
                    == "protection"
                },
                "rules": {
                    "nodes": [
                        {
                            "parameters": {
                                "__typename": "PullRequestParameters",
                                "requiredReviewThreadResolution": (
                                    pr.thread_resolution == "ruleset"
                                ),
                            },
                        }
                    ]
                },
            },
            "headRefName": pr.head,
            "headRefOid": pr.head_oid,
            "headRef": (
                None
                if pr.head in repo.deleted_branches
                else {"id": _ref_id(repo, pr.head), "name": pr.head}
            ),
            "headRepository": lambda info: self._repo_node(repo),
            "isCrossRepository": False,
            "timelineItems": timeline,
            "commits": commits,
            "reviewThreads": review_threads,
            "isInMergeQueue": pr.queued,
            "reviewDecision": pr.review_decision(),
            "mergeStateStatus": pr.merge_state_status(),
            "viewerCanMergeAsAdmin": pr.can_merge_as_admin,
            "reviews": reviews,
            "reviewRequests": lambda info, **_: {
                "nodes": [
                    {"requestedReviewer": self._user_node(login)}
                    for login in sorted(pr.review_requests)
                ]
                + (
                    [{"requestedReviewer": {"__typename": "Team", "name": TEAM}}]
                    if pr.team_review_requests
                    else []
                )
            },
            "comments": comments,
        }


def _ref_id(repo: FakeRepo, branch: str) -> str:
    return f"REF_{repo.slug}:{branch}"


def _pr_id(pr: PullRequest) -> str:
    return f"PR_{pr.repo.slug}/{pr.number}"


def _is_mutation(method: str, path: str, body: bytes) -> bool:
    """Whether a request is a GraphQL mutation."""
    if method != "POST" or path != "/graphql":
        return False
    query = json.loads(body).get("query", "")
    return query.lstrip().startswith("mutation")


def _reply(
    req: BaseHTTPRequestHandler, status: int, payload: Any, rate_limit: dict[str, int]
) -> None:
    data = json.dumps(payload).encode()
    req.send_response(status)
    req.send_header("Content-Type", "application/json")
    req.send_header("Content-Length", str(len(data)))
    req.send_header("x-ratelimit-limit", str(rate_limit["limit"]))
    req.send_header("x-ratelimit-remaining", str(rate_limit["remaining"]))
    req.send_header(
        "x-ratelimit-used", str(rate_limit["limit"] - rate_limit["remaining"])
    )
    req.send_header("x-ratelimit-reset", str(rate_limit["reset"]))
    req.send_header("x-ratelimit-resource", "graphql")
    req.end_headers()
    req.wfile.write(data)


# Search.


class SearchError(ValueError):
    pass


_TOKEN = re.compile(r"\(|\)|[^\s()]+")


def _parse(query: str) -> tuple[Any, list[str]]:
    """Parses a search query into an expression tree and its `sort:` keys.

    Trees are ("and", [...]), ("or", [...]), ("not", t) or ("term", text).
    AND binds tighter than OR, as on GitHub.
    """
    tokens = _TOKEN.findall(query)
    sorts = [t.removeprefix("sort:") for t in tokens if t.startswith("sort:")]
    tokens = [t for t in tokens if not t.startswith("sort:")]
    pos = 0

    def peek() -> str | None:
        return tokens[pos] if pos < len(tokens) else None

    def or_expr() -> Any:
        nonlocal pos
        terms = [and_expr()]
        while peek() == "OR":
            pos += 1
            terms.append(and_expr())
        return terms[0] if len(terms) == 1 else ("or", terms)

    def and_expr() -> Any:
        terms = []
        while peek() not in (None, ")", "OR"):
            terms.append(unary())
        if not terms:
            raise SearchError(f"expected a term at {pos} in {query!r}")
        return terms[0] if len(terms) == 1 else ("and", terms)

    def unary() -> Any:
        nonlocal pos
        tok = tokens[pos]
        pos += 1
        if tok == "(":
            inner = or_expr()
            if peek() != ")":
                raise SearchError(f"unbalanced parentheses in {query!r}")
            pos += 1
            return inner
        if tok.startswith("-") and len(tok) > 1:
            return ("not", ("term", tok[1:]))
        return ("term", tok)

    tree = or_expr() if tokens else ("and", [])
    if pos != len(tokens):
        raise SearchError(f"unexpected {tokens[pos]!r} in {query!r}")
    return tree, sorts


def _me(login: str) -> str:
    return VIEWER if login == "@me" else login


def _term(pr: PullRequest, term: str) -> bool:
    if ":" not in term:
        return term.lower() in f"{pr.title}\n{pr.body}".lower()
    key, value = term.split(":", 1)
    match key, value:
        case "is", "pr":
            return True
        case "is", "issue":
            return False
        case "is", "open":
            return pr.state == OPEN
        case "is", "closed":
            return pr.state != OPEN
        case "is", "merged":
            return pr.state == MERGED
        case "is", "unmerged":
            return pr.state != MERGED
        case "is", "draft":
            return pr.draft
        case "is", "queued":
            return pr.queued
        case "state", ("open" | "closed" | "merged"):
            return _term(pr, f"is:{value}")
        case "draft", ("true" | "false"):
            return pr.draft == (value == "true")
        case "archived", ("true" | "false"):
            return value == "false"
        case "repo", _:
            return pr.repo.slug == value
        case "author", _:
            return pr.author == _me(value)
        case "user-review-requested", _:
            return _me(value) in pr.review_requests
        case "team-review-requested-user", _:
            return _me(value) in pr.team_review_requests
        case "reviewed-by", _:
            return any(
                r.author == _me(value) and r.state != PENDING for r in pr.reviews
            )
        case "review", "none":
            # Only verdicts count; comments and dismissals don't.
            return not any(r.state in (APPROVED, CHANGES_REQUESTED) for r in pr.reviews)
        case "gr", _:
            # gr: qualifiers are grenadine's own; one reaching the fake
            # means the widening didn't drop it.
            raise SearchError(f"a gr: qualifier reached GitHub: {term!r}")
        case "review", "approved":
            return pr.review_decision() == APPROVED
        case "review", "changes_requested":
            return pr.review_decision() == CHANGES_REQUESTED
    raise SearchError(f"the fake GitHub doesn't support the qualifier {term!r}")


def _eval(pr: PullRequest, tree: Any) -> bool:
    match tree:
        case ("and", terms):
            return all(_eval(pr, t) for t in terms)
        case ("or", terms):
            return any(_eval(pr, t) for t in terms)
        case ("not", t):
            return not _eval(pr, t)
        case ("term", text):
            return _term(pr, text)
    raise AssertionError(tree)


_SORT_KEYS: dict[str, Callable[[PullRequest], Any]] = {
    "updated": lambda pr: pr.updated_at,
    "created": lambda pr: pr.created_at,
    "comments": lambda pr: len(pr.comments),
}


def matches(pr: PullRequest, query: str) -> bool:
    return _eval(pr, _parse(query)[0])


def search_prs(prs: list[PullRequest], query: str) -> list[PullRequest]:
    """The PRs that `query` finds, in its `sort:` order (most recently
    updated first by default)."""
    tree, sorts = _parse(query)
    found = [pr for pr in prs if _eval(pr, tree)]
    # Apply the sorts last to first so that the first one wins.
    for sort in reversed(sorts or ["updated-desc"]):
        field, _, direction = sort.partition("-")
        if field not in _SORT_KEYS:
            raise SearchError(f"the fake GitHub doesn't support sort:{sort}")
        found.sort(key=_SORT_KEYS[field], reverse=direction != "asc")
    return found
