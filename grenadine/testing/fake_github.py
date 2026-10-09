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
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable

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

OPEN = "OPEN"
CLOSED = "CLOSED"
MERGED = "MERGED"

SCHEMA = build_schema(Path(__file__).with_name("github_schema.graphql").read_text())


def iso(t: int) -> str:
    return datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


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
    # In a merge queue.
    queued: bool = False
    # Users asked to review.
    review_requests: set[str] = dataclasses.field(default_factory=set)
    # Members of the teams asked to review.
    team_review_requests: set[str] = dataclasses.field(default_factory=set)
    reviews: list[Review] = dataclasses.field(default_factory=list)
    comments: list[Comment] = dataclasses.field(default_factory=list)

    @property
    def url(self) -> str:
        return f"https://github.com/{self.repo.slug}/pull/{self.number}"

    @property
    def head_oid(self) -> str:
        return self.repo.tip(self.head)

    def touch(self) -> None:
        self.updated_at = self.repo.gh.tick()

    def review_decision(self) -> str | None:
        """APPROVED, CHANGES_REQUESTED or None, from each reviewer's latest
        verdict. Comment-only reviews carry no verdict."""
        latest: dict[str, str] = {}
        for r in self.reviews:
            if r.state != COMMENTED:
                latest[r.author] = r.state
        if CHANGES_REQUESTED in latest.values():
            return CHANGES_REQUESTED
        if APPROVED in latest.values():
            return APPROVED
        return None


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
            exists = subprocess.run(
                ["git", "-C", str(self.work), "rev-parse", "--verify", "--quiet", f"refs/heads/{branch}"],
                env=self.gh.git_env(), capture_output=True,
            ).returncode == 0
            self.git("checkout", "--quiet", *([] if exists else ["-b"]), branch)

    def commit(self, message: str, files: dict[str, str | None], *, author: str = VIEWER,
               amend: bool = False) -> str:
        """Writes `files` (None deletes one) and commits them as `author`."""
        for path, content in files.items():
            p = self.work / path
            if content is None:
                self.git("rm", "--quiet", path)
                continue
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(content)
            self.git("add", path)
        when = f"@{self.gh.tick()} +0000"
        env = {
            "GIT_AUTHOR_NAME": self.gh.users.get(author, author),
            "GIT_AUTHOR_EMAIL": f"{author}@users.noreply.github.com",
            "GIT_AUTHOR_DATE": when,
            "GIT_COMMITTER_NAME": self.gh.users.get(author, author),
            "GIT_COMMITTER_EMAIL": f"{author}@users.noreply.github.com",
            "GIT_COMMITTER_DATE": when,
        }
        args = ["commit", "--quiet", "--allow-empty", "-m", message]
        if amend:
            args.append("--amend")
        self.git(*args, env=env)
        return self.git("rev-parse", "HEAD").strip()

    def push(self, branch: str, *, actor: str = VIEWER) -> str:
        """Pushes the work tree's `branch`, force-pushing if needed, and
        logs the push in the branch's activity."""
        after = self.git("rev-parse", branch).strip()
        before = self.tip(branch) or ZERO_SHA
        if after == before:
            return after
        force = before != ZERO_SHA and subprocess.run(
            ["git", "-C", str(self.work), "merge-base", "--is-ancestor", before, after],
            env=self.gh.git_env(),
        ).returncode != 0
        if force:
            # Keep the old tip so it can still be fetched by SHA.
            self.git("push", "--quiet", "origin", f"{before}:refs/keep/{before}")
        self.git("push", "--quiet", "--force", "origin", f"{branch}:refs/heads/{branch}")
        self.pushes.setdefault(branch, []).append(
            Push(before=before, after=after, at=self.gh.tick(), actor=actor, force=force))
        for pr in self.prs.values():
            if pr.head == branch:
                pr.touch()
        return after

    def open_pr(self, title: str, *, head: str, base: str = "main", body: str = "",
                author: str = VIEWER, draft: bool = False) -> PullRequest:
        if not self.tip(head):
            raise ValueError(f"push {head} before opening a PR from it")
        number = self.gh.next_number()
        now = self.gh.tick()
        pr = PullRequest(repo=self, number=number, title=title, body=body, author=author,
                         base=base, head=head, created_at=now, updated_at=now, draft=draft)
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

    def comment(self, pr: PullRequest, *, author: str, body: str, path: str, line: int,
                commit: str | None = None, reply_to: Comment | None = None) -> Comment:
        c = Comment(id=self.gh.next_comment_id(), author=author, body=body, path=path,
                    commit=commit or pr.head_oid, line=line, created_at=self.gh.tick(),
                    in_reply_to=reply_to.id if reply_to else None)
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
            out = self.gh.git("-C", str(self.bare), "log", "-1", "--format=%an%x00%ae", sha)
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
        self.gitconfig = root / "gitconfig"
        self.gitconfig.write_text(
            f'[url "file://{root / "remotes"}/"]\n'
            "\tinsteadOf = https://github.com/\n"
            "[user]\n\tname = Test\n\temail = test@example.com\n"
        )
        self._server: ThreadingHTTPServer | None = None

    def git_env(self) -> dict[str, str]:
        """The environment for git, the server's included: no system or user
        config, and fetches from github.com go to the bare repos."""
        return dict(os.environ, GIT_CONFIG_GLOBAL=str(self.gitconfig), GIT_CONFIG_NOSYSTEM="1")

    def git(self, *args: str, env: dict[str, str] | None = None) -> str:
        return subprocess.run(["git", *args], env=dict(self.git_env(), **(env or {})),
                              check=True, capture_output=True, text=True).stdout

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
            return _reply(req, 401, {"message": "Bad credentials"})
        body = req.rfile.read(int(req.headers.get("Content-Length") or 0))
        with self.lock:
            try:
                status, payload = self._route(method, url.path, urllib.parse.parse_qs(url.query), body)
            except Exception as e:  # noqa: BLE001 - a fake should report, not hang up.
                status, payload = 500, {"message": f"fake GitHub: {e!r}"}
        _reply(req, status, payload)

    def _route(self, method: str, path: str, query: dict[str, list[str]],
               body: bytes) -> tuple[int, Any]:
        if method == "POST" and path == "/graphql":
            return 200, self.graphql(json.loads(body))
        if method == "GET" and (m := re.fullmatch(r"/repos/([^/]+/[^/]+)/activity", path)):
            return self._activity(m[1], query)
        if method == "GET" and (m := re.fullmatch(r"/repos/([^/]+/[^/]+)/pulls/(\d+)/comments", path)):
            return self._review_comments(m[1], int(m[2]))
        return 404, {"message": "Not Found"}

    def _activity(self, slug: str, query: dict[str, list[str]]) -> tuple[int, Any]:
        repo = self.repos.get(slug)
        ref = query.get("ref", [""])[0]
        if repo is None or not ref.startswith("refs/heads/"):
            return 404, {"message": "Not Found"}
        out = []
        for p in repo.pushes.get(ref.removeprefix("refs/heads/"), []):
            kind = "branch_creation" if p.before == ZERO_SHA else "force_push" if p.force else "push"
            out.append({
                "before": p.before,
                "after": p.after,
                "ref": ref,
                "timestamp": iso(p.at),
                "activity_type": kind,
                "actor": {"login": p.actor, "avatar_url": None},
            })
        if query.get("direction", ["desc"])[0] != "asc":
            out.reverse()
        return 200, out

    def _review_comments(self, slug: str, number: int) -> tuple[int, Any]:
        repo = self.repos.get(slug)
        pr = repo and repo.prs.get(number)
        if pr is None:
            return 404, {"message": "Not Found"}
        return 200, [{
            "id": c.id,
            "in_reply_to_id": c.in_reply_to,
            "user": {"login": c.author, "avatar_url": None},
            "body": c.body,
            "path": c.path,
            "commit_id": c.commit,
            "original_commit_id": c.commit,
            "original_line": c.line,
            "original_start_line": None,
            "line": c.line,
            "start_line": None,
            "side": c.side,
            "subject_type": "line",
            "created_at": iso(c.created_at),
            "html_url": f"{pr.url}#discussion_r{c.id}",
        } for c in pr.comments]

    # GraphQL.

    def graphql(self, request: dict[str, Any]) -> dict[str, Any]:
        result = graphql_sync(SCHEMA, request["query"], root_value=self._root(),
                              variable_values=request.get("variables"))
        out: dict[str, Any] = {"data": result.data}
        if result.errors:
            out["errors"] = [e.formatted for e in result.errors]
        return out

    def _root(self) -> dict[str, Any]:
        def search(info: Any, type: str, query: str, first: int | None = None, **_: Any) -> Any:
            hits = search_prs(self.prs(), query)[:first]
            return {"issueCount": len(hits), "nodes": [self._pr_node(pr) for pr in hits]}

        def repository(info: Any, owner: str, name: str) -> Any:
            repo = self.repos.get(f"{owner}/{name}")
            if repo is None:
                raise LookupError(f"Could not resolve to a Repository with the name '{owner}/{name}'.")
            return self._repo_node(repo)

        def user(info: Any, login: str) -> Any:
            if login not in self.users:
                raise LookupError(f"Could not resolve to a User with the login of '{login}'.")
            return self._user_node(login)

        return {"search": search, "repository": repository, "user": user}

    def _user_node(self, login: str) -> dict[str, Any]:
        return {"__typename": "User", "login": login, "name": self.users.get(login), "avatarUrl": None}

    def _repo_node(self, repo: FakeRepo) -> dict[str, Any]:
        def pull_request(info: Any, number: int) -> Any:
            pr = repo.prs.get(number)
            if pr is None:
                raise LookupError(f"Could not resolve to a PullRequest with the number of {number}.")
            return self._pr_node(pr)

        def pull_requests(info: Any, headRefName: str | None = None, baseRefName: str | None = None,
                          states: list[str] | None = None, first: int | None = None, **_: Any) -> Any:
            found = [
                pr for pr in repo.prs.values()
                if (headRefName is None or pr.head == headRefName)
                and (baseRefName is None or pr.base == baseRefName)
                and (states is None or pr.state in states)
            ]
            return {"nodes": [self._pr_node(pr) for pr in found[:first]]}

        def object_(info: Any, oid: str | None = None, **_: Any) -> Any:
            return self._commit_node(repo, oid) if oid and repo.commit_author(oid) else None

        return {
            "__typename": "Repository",
            "nameWithOwner": repo.slug,
            "pullRequest": pull_request,
            "pullRequests": pull_requests,
            "object": object_,
        }

    def _commit_node(self, repo: FakeRepo, sha: str) -> dict[str, Any]:
        def author(info: Any) -> Any:
            found = repo.commit_author(sha)
            if found is None:
                return None
            name, login = found
            return {"name": name, "email": f"{login}@users.noreply.github.com",
                    "user": self._user_node(login) if login in self.users else None}

        return {
            "__typename": "Commit",
            "oid": sha,
            "author": author,
            "checkSuites": lambda info, **_: {"nodes": []},
        }

    def _pr_node(self, pr: PullRequest) -> dict[str, Any]:
        repo = pr.repo

        def timeline(info: Any, itemTypes: list[str] | None = None, first: int | None = None,
                     **_: Any) -> Any:
            nodes: list[dict[str, Any]] = []
            if itemTypes is None or "HEAD_REF_FORCE_PUSHED_EVENT" in itemTypes:
                nodes += [{
                    "__typename": "HeadRefForcePushedEvent",
                    "createdAt": iso(p.at),
                    "actor": self._user_node(p.actor),
                    "beforeCommit": self._commit_node(repo, p.before),
                    "afterCommit": self._commit_node(repo, p.after),
                } for p in repo.pushes.get(pr.head, []) if p.force and p.at >= pr.created_at]
            return {"nodes": nodes[:first]}

        def commits(info: Any, first: int | None = None, **_: Any) -> Any:
            return {"nodes": [{"commit": self._commit_node(repo, sha)}
                              for sha in repo.commits(pr)[:first]]}

        def review_threads(info: Any, first: int | None = None, **_: Any) -> Any:
            roots = [c for c in pr.comments if c.in_reply_to is None]
            return {
                "pageInfo": {"hasNextPage": False, "endCursor": None},
                "nodes": [{
                    "isResolved": c.resolved,
                    "comments": lambda info, c=c, **_: {"nodes": [{"databaseId": c.id}]},
                } for c in roots[:first]],
            }

        return {
            "__typename": "PullRequest",
            "number": pr.number,
            "title": pr.title,
            "body": pr.body,
            "url": pr.url,
            "state": pr.state,
            "isDraft": pr.draft,
            "createdAt": iso(pr.created_at),
            "updatedAt": iso(pr.updated_at),
            "mergedAt": iso(pr.merged_at) if pr.merged_at else None,
            "author": self._user_node(pr.author),
            "repository": lambda info: self._repo_node(repo),
            "baseRefName": pr.base,
            "headRefName": pr.head,
            "headRefOid": pr.head_oid,
            "headRepository": lambda info: self._repo_node(repo),
            "isCrossRepository": False,
            "timelineItems": timeline,
            "commits": commits,
            "reviewThreads": review_threads,
        }


def _reply(req: BaseHTTPRequestHandler, status: int, payload: Any) -> None:
    data = json.dumps(payload).encode()
    req.send_response(status)
    req.send_header("Content-Type", "application/json")
    req.send_header("Content-Length", str(len(data)))
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
            return any(r.author == _me(value) for r in pr.reviews)
        case "review", "none":
            # GitHub ignores comment-only reviews here.
            return all(r.state == COMMENTED for r in pr.reviews)
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
