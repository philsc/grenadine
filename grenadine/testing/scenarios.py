"""PRs to load a FakeGitHub with, and where grenadine should show them."""

import dataclasses
import re
from pathlib import Path

from grenadine.testing.fake_github import (
    APPROVED,
    CHANGES_REQUESTED,
    COMMENTED,
    DISMISSED,
    PENDING,
    VIEWER,
    CheckRun,
    FakeGitHub,
    FakeRepo,
    PullRequest,
    _parse,
    _term,
)

REPO = "test/test"
OTHER = "other"

# The default inboxes, as named in grenadine/server/src/inboxes.rs.
NEEDS_REVIEW = "Needs your review"
NEEDS_TEAM_REVIEW = "Needs your teams' review"
RETURNED = "Returned to you"
APPROVED_INBOX = "Approved"
MERGING = "Merging"
DRAFTS = "Drafts"
DRAFTS_FROM_OTHERS = "Drafts from others"
WAITING_FOR_REVIEWERS = "Waiting for reviewers"
ALL_MY_OPEN = "All my open PRs"
WAITING_FOR_AUTHOR = "Waiting for author"
MERGED_BY_ME = "Merged by me"
MERGED_BY_OTHERS = "Merged by others"
ALL_OPEN = "All open PRs"
UNMERGED_ON_MINE = "Unmerged PRs on mine"

STACK_BASE = "Lay the stack's foundation"
STACK_TOP = "Build on the foundation"

DESCRIPTION = """\
## Why

Markdown in descriptions should render:

- a list item
- another with `inline code`

```rust
fn main() {}
```
"""


def default_inboxes() -> list[tuple[str, str, bool, bool]]:
    """The `(name, filter, count_in_total, sync_in_background)` tuples of
    the server's default inboxes, read from its source so that they can't
    drift."""
    src = (Path(__file__).parents[1] / "server/src/inboxes.rs").read_text()
    return [
        (name, filter, counted == "true", background == "true")
        for name, filter, counted, background in re.findall(
            r'name:\s*"([^"]*)",\s*filter:\s*"([^"]*)",\s*count_in_total:\s*(true|false),'
            r"\s*sync_in_background:\s*(true|false)",
            src,
        )
    ]


def widen(filter: str) -> str:
    """The widened terms grenadine's filter parsing produces: every gr:
    literal replaced by TRUE, folded, plus the `sort:` tokens. Mirrors
    `filter::Filter::search_terms`."""
    tree, sorts = _parse(filter)

    def w(e: tuple) -> tuple:
        match e[0]:
            case "not":
                inner = w(e[1])
                return ("term", "TRUE") if inner == ("term", "TRUE") else ("not", inner)
            case "term":
                return ("term", "TRUE") if e[1].startswith("gr:") else e
            case "and":
                cs = [c for c in (w(x) for x in e[1]) if c != ("term", "TRUE")]
                if not cs:
                    return ("term", "TRUE")
                return cs[0] if len(cs) == 1 else ("and", cs)
            case "or":
                cs = [w(x) for x in e[1]]
                if ("term", "TRUE") in cs:
                    return ("term", "TRUE")
                return cs[0] if len(cs) == 1 else ("or", cs)
        raise AssertionError(e)

    def ser(e: tuple) -> str:
        match e[0]:
            case "term":
                return e[1]
            case "not":
                return "-" + ser(e[1])
            case "and":
                return " ".join(f"({ser(c)})" if c[0] == "or" else ser(c) for c in e[1])
            case "or":
                return " OR ".join(ser(c) for c in e[1])
        raise AssertionError(e)

    terms = ser(w(tree))
    if terms == "TRUE":
        terms = ""
    return " ".join([terms, *(f"sort:{s}" for s in sorts)]).strip()


def server_query(filter: str, repos: list[str]) -> str:
    """The search query the server sends for an inbox: `filter`'s
    widening, then `github::search_query`'s wrapping."""
    tokens = widen(filter).split()
    sorts = [t for t in tokens if t.startswith("sort:")]
    terms = [t for t in tokens if not t.startswith("sort:")]
    parts = ["is:pr"]
    if terms:
        parts.append(f"({' '.join(terms)})")
    if repos:
        parts.append(f"({' OR '.join(f'repo:{r}' for r in repos)})")
    return " ".join(parts + sorts)


def _verdict(pr: PullRequest, user: str) -> str | None:
    """`user`'s standing verdict, as README's reviews describe it."""
    for r in reversed(pr.reviews):
        if r.author == user and r.state in (APPROVED, CHANGES_REQUESTED, DISMISSED):
            return None if r.state == DISMISSED else r.state
    return None


def _gr_term(pr: PullRequest, term: str, me: str) -> bool:
    """One gr: qualifier on one PR, as README's table describes it."""
    name, _, arg = term[3:].partition(":")
    user = me if arg == "@me" else arg
    match name:
        case "review" if arg == "active":
            return any(
                r.state not in (DISMISSED, PENDING)
                and r.author != pr.author
                and r.author not in pr.review_requests
                for r in pr.reviews
            )
        case "changes-requested" if arg == "unaddressed":
            return any(
                _verdict(pr, u) == CHANGES_REQUESTED and u not in pr.review_requests
                for u in {r.author for r in pr.reviews}
            )
        case "approvals":
            n = int(arg.removeprefix(">"))
            return (
                sum(_verdict(pr, u) == APPROVED for u in {r.author for r in pr.reviews})
                > n
            )
        case "approved-by":
            return _verdict(pr, user) == APPROVED
        case "rerequested":
            return user in pr.review_requests and any(
                r.author == user and r.state != PENDING for r in pr.reviews
            )
        case "sort":
            return True
    raise AssertionError(f"unknown gr: qualifier {term!r}")


def _is_local(term: str) -> bool:
    """The GitHub qualifiers grenadine evaluates itself; mirrors
    filter.rs's is_local."""
    key, _, value = term.partition(":")
    if not _:
        return False
    match key:
        case "is":
            return value in (
                "pr",
                "open",
                "closed",
                "merged",
                "unmerged",
                "draft",
                "queued",
            )
        case "state":
            return value in ("open", "closed", "merged")
        case "draft" | "archived":
            return value in ("true", "false")
        case "author" | "user-review-requested" | "reviewed-by":
            return True
        case "review":
            return value in ("approved", "changes_requested", "none")
    return False


def matches(pr: PullRequest, filter: str, me: str = VIEWER) -> bool:
    """Whether `pr` matches `filter` under grenadine's local evaluation:
    `gr:` and the local GitHub qualifiers by README's rules, search-only
    terms true."""

    def ev(e: tuple) -> bool:
        match e[0]:
            case "and":
                return all(ev(c) for c in e[1])
            case "or":
                return any(ev(c) for c in e[1])
            case "not":
                return not ev(e[1])
            case "term":
                term = e[1]
                if term.startswith("gr:"):
                    return _gr_term(pr, term, me)
                if not _is_local(term):
                    return True
                return _term(pr, term)
        raise AssertionError(e)

    return ev(_parse(filter)[0])


@dataclasses.dataclass
class Inboxes:
    """The scenario's PRs by title, and the inboxes each belongs in."""

    prs: dict[str, PullRequest]
    expected: dict[str, list[str]]

    def titles_in(self, inbox: str) -> list[str]:
        """The titles `inbox` should list, in its own order."""
        prs = [self.prs[t] for t, i in self.expected.items() if inbox in i]
        filter = next(f for n, f, *_ in default_inboxes() if n == inbox)
        if "gr:sort:title-asc" in filter:
            return [pr.title for pr in sorted(prs, key=lambda pr: pr.title.lower())]
        return [
            pr.title for pr in sorted(prs, key=lambda pr: pr.updated_at, reverse=True)
        ]


def _simple_pr(
    repo: FakeRepo,
    case: str,
    title: str,
    *,
    author: str = VIEWER,
    draft: bool = False,
    body: str = "",
) -> PullRequest:
    branch = f"case/{case}"
    repo.checkout(branch, "main")
    repo.commit(title, {f"inbox/{case}.md": f"# {title}\n"}, author=author)
    repo.push(branch, actor=author)
    return repo.open_pr(
        title, head=branch, author=author, draft=draft, body=body or title
    )


def inbox_cases(gh: FakeGitHub) -> Inboxes:
    """PRs covering the default inboxes, PRs that belong in none of the
    counted ones, and a two-PR stack."""
    repo = gh.repo(REPO)
    repo.checkout("main")
    repo.commit("Seed the repo", {"README.md": "# test\n"})
    repo.push("main")

    prs: dict[str, PullRequest] = {}
    expected: dict[str, list[str]] = {}

    def add(pr: PullRequest, inboxes: list[str]) -> PullRequest:
        prs[pr.title] = pr
        expected[pr.title] = inboxes
        return pr

    pr = add(
        _simple_pr(repo, "needs-review", "Ask me for a review", author=OTHER),
        [NEEDS_REVIEW, ALL_OPEN],
    )
    pr.review_requests.add(VIEWER)
    # A cancelled run that a later one superseded doesn't count.
    pr.check_runs = [
        CheckRun("test", conclusion="CANCELLED", started_at=1, workflow="Test"),
        CheckRun("test", started_at=2, workflow="Test"),
    ]
    pr.review_required = True
    pr.merge_state = "BLOCKED"

    pr = add(
        _simple_pr(repo, "needs-team-review", "Ask my team for a review", author=OTHER),
        [NEEDS_TEAM_REVIEW, ALL_OPEN],
    )
    pr.team_review_requests.add(VIEWER)
    pr.check_runs = [
        CheckRun("lint"),
        CheckRun("test", conclusion="FAILURE"),
        CheckRun("docs", conclusion="SKIPPED"),
    ]

    # A comment-only review is an active review.
    pr = add(
        _simple_pr(repo, "returned-commented", "Get a comment-only review"),
        [RETURNED, ALL_MY_OPEN, ALL_OPEN],
    )
    pr.submit_review(OTHER, COMMENTED)

    pr = add(
        _simple_pr(repo, "returned-changes", "Get changes requested"),
        [RETURNED, ALL_MY_OPEN, ALL_OPEN],
    )
    pr.submit_review(OTHER, CHANGES_REQUESTED, body="Please split this up.")
    repo.issue_comment(pr, author=VIEWER, body="Will do, thanks!")
    pr.review_requests.add("third")

    pr = add(
        _simple_pr(repo, "approved", "Get approved", body=DESCRIPTION),
        [APPROVED_INBOX, ALL_MY_OPEN, ALL_OPEN],
    )
    pr.submit_review(OTHER, APPROVED)

    add(
        _simple_pr(repo, "waiting-reviewers", "Wait for reviewers"),
        [WAITING_FOR_REVIEWERS, ALL_MY_OPEN, ALL_OPEN],
    )
    add(
        _simple_pr(repo, "draft", "Stay a draft", draft=True),
        [DRAFTS, ALL_MY_OPEN, ALL_OPEN],
    )

    pr = add(
        _simple_pr(repo, "merging", "Sit in the merge queue"),
        [MERGING, ALL_MY_OPEN],
    )
    pr.submit_review(OTHER, APPROVED)
    pr.queued = True

    pr = add(
        _simple_pr(
            repo,
            "draft-needs-review",
            "Ask me to look at a draft",
            author=OTHER,
            draft=True,
        ),
        [DRAFTS_FROM_OTHERS, ALL_OPEN],
    )
    pr.review_requests.add(VIEWER)

    pr = add(
        _simple_pr(
            repo, "waiting-authors", "Wait for the author after my review", author=OTHER
        ),
        [WAITING_FOR_AUTHOR, ALL_OPEN],
    )
    pr.submit_review(VIEWER, COMMENTED)
    pr.check_runs = [CheckRun("test", status="IN_PROGRESS", conclusion=None)]
    path = "inbox/waiting-authors.md"
    root = repo.comment(pr, author=VIEWER, body="Why?", path=path, line=1)
    repo.comment(pr, author=OTHER, body="Because.", path=path, line=1, reply_to=root)
    root.resolved = True
    repo.comment(pr, author=VIEWER, body="And this?", path=path, line=1)

    pr = add(_simple_pr(repo, "closed", "Get closed", author=OTHER), [])
    repo.close(pr)
    # state:closed covers unmerged PRs too, so "Merged by me" has it.
    pr = add(
        _simple_pr(repo, "closed-mine", "Retract my PR"),
        [MERGED_BY_ME, UNMERGED_ON_MINE],
    )
    repo.close(pr)
    pr = add(_simple_pr(repo, "merged", "Get merged"), [MERGED_BY_ME])
    pr.submit_review(OTHER, APPROVED)
    pr.check_runs = [CheckRun("test")]
    repo.merge(pr)
    pr = add(
        _simple_pr(repo, "merged-others", "Land the other's PR", author=OTHER),
        [MERGED_BY_OTHERS],
    )
    pr.submit_review(VIEWER, APPROVED)
    repo.merge(pr)
    pr = add(
        _simple_pr(repo, "uninvolved", "Leave me out of it", author=OTHER),
        [ALL_OPEN],
    )
    pr.conflicting = True

    # Stacks only form on non-trunk branches, which are dev/ ones by default.
    repo.checkout("dev/stack-base", "main")
    repo.commit(STACK_BASE, {"stack/base.txt": "base\n"})
    repo.push("dev/stack-base")
    add(
        repo.open_pr(STACK_BASE, head="dev/stack-base"),
        [WAITING_FOR_REVIEWERS, ALL_MY_OPEN, ALL_OPEN],
    )
    repo.checkout("dev/stack-top")
    repo.commit(STACK_TOP, {"stack/top.txt": "top\n"})
    repo.push("dev/stack-top")
    add(
        repo.open_pr(STACK_TOP, head="dev/stack-top", base="dev/stack-base"),
        [WAITING_FOR_REVIEWERS, ALL_MY_OPEN, ALL_OPEN],
    )

    return Inboxes(prs, expected)


GROW = "Grow one push at a time"
REBASE = "Rebase onto a newer main"
REBASE_AND_EDIT = "Rebase and edit in one push"
FILE_KINDS = "Change every kind of file"
BIG_DIFF = "Add more files than render at once"
# Enough rows that the later files render after the first ones paint.
BIG_FILES = [f"big/module_{i}.py" for i in range(6)]


def big_module(i: int) -> str:
    return "".join(f"def f{i}_{n}():\n    return {n}\n" for n in range(150))


STEPS_PY = "versions/steps.py"


def steps(count: int, doubled: int = 0) -> str:
    """A Python file with one function per pushed version; `doubled`'s
    function returns twice its number."""
    body = ['"""One function per pushed version."""']
    for i in range(1, count + 1):
        value = f"{i} * 2" if i == doubled else str(i)
        body += ["", "", f"def step_{i}():", f"    return {value}"]
    return "\n".join(body) + "\n"


SETTINGS = [f"SETTING_{i:02d} = {i}" for i in range(1, 61)]
PR_SETTINGS = {
    i: f"SETTING_{i:02d} = {i} + 100  # changed by the PR" for i in (5, 6, 7)
}
UPSTREAM_SETTINGS = {50: "SETTING_50 = 50 - 1  # changed upstream"}
EDITED_SETTING = "SETTING_06 = 6 + 200  # edited after the rebase"

UPSTREAM_FILE = "Edit a file that upstream added"
UPSTREAM_ADDED = "upstream_file/added.py"
UPSTREAM_ADDED_LINES = [f"added line {i}" for i in range(1, 21)]
UPSTREAM_ADDED_EDITS = [
    "added line 5, edited",
    "added line 10, edited",
    "added line 21, new",
]
CONFLICT = "Conflict with upstream"
CONFLICT_PATH = "conflict/shared.py"
PR_SETTING_10 = "SETTING_10 = 10 + 100  # changed by the PR"
UPSTREAM_SETTING_10 = "SETTING_10 = 10 - 1  # changed upstream"
RESOLVED_SETTING_10 = "SETTING_10 = 10 * 2  # resolved"


def settings(changes: dict[int, str]) -> str:
    return "\n".join(changes.get(i, line) for i, line in enumerate(SETTINGS, 1)) + "\n"


@dataclasses.dataclass
class Versions:
    """The scenario's PRs by title, and the SHA of each version, oldest
    first."""

    prs: dict[str, PullRequest]
    shas: dict[str, list[str]]


def versions_cases(gh: FakeGitHub) -> Versions:
    """PRs whose versions and diffs the tests check, as in
    create_test_repo.py."""
    repo = gh.repo(REPO)
    repo.checkout("main")
    repo.commit("Seed the repo", {"README.md": "# test\n"})
    repo.push("main")
    prs: dict[str, PullRequest] = {}
    shas: dict[str, list[str]] = {}

    # Opened with two commits, then a push of one commit, a push of three,
    # and a force push of an amended commit: six versions.
    repo.checkout("case/grow", "main")
    repo.commit("Add step 1", {STEPS_PY: steps(1)})
    v = [repo.commit("Add step 2", {STEPS_PY: steps(2)})]
    repo.push("case/grow")
    pr = prs[GROW] = repo.open_pr(GROW, head="case/grow")
    v.append(repo.commit("Add step 3", {STEPS_PY: steps(3)}))
    repo.push(pr.head)
    for i in (4, 5, 6):
        v.append(repo.commit(f"Add step {i}", {STEPS_PY: steps(i)}))
    repo.push(pr.head)
    v.append(repo.commit("Add step 6", {STEPS_PY: steps(6, doubled=6)}, amend=True))
    repo.push(pr.head)
    shas[GROW] = v
    # An open thread with a reply, and a thread the other account resolved.
    root = repo.comment(
        pr, author=OTHER, body="Why a docstring?", path=STEPS_PY, line=1
    )
    repo.comment(
        pr, author=VIEWER, body="To explain.", path=STEPS_PY, line=1, reply_to=root
    )
    done = repo.comment(
        pr, author=OTHER, body="Rename this.\n\nIt's vague.", path=STEPS_PY, line=4
    )
    done.resolved = True
    done.resolved_by = OTHER

    for title, edit in ((REBASE, False), (REBASE_AND_EDIT, True)):
        d = "rebase_and_edit" if edit else "rebase"
        shared, unrelated, upstream_only = (
            f"{d}/shared.py",
            f"{d}/unrelated.txt",
            f"{d}/upstream_only.txt",
        )
        repo.checkout("main")
        repo.commit(f"Add {d}", {shared: settings({}), unrelated: "upstream, before\n"})
        repo.push("main")
        branch = f"case/{d}"
        repo.checkout(branch, "main")
        v = [repo.commit("Change settings 5 to 7", {shared: settings(PR_SETTINGS)})]
        repo.push(branch)
        pr = prs[title] = repo.open_pr(title, head=branch)
        if not edit:
            # A thread on a line the diff's context doesn't show.
            repo.comment(pr, author=OTHER, body="Is 30 right?", path=shared, line=30)

        repo.checkout("main")
        repo.commit(
            "Change things upstream",
            {
                shared: settings(UPSTREAM_SETTINGS),
                unrelated: "upstream, after\n",
                upstream_only: "Only main added this file.\n",
            },
        )
        repo.push("main")
        repo.checkout(branch)
        repo.rebase("main")
        if edit:
            edited = {**UPSTREAM_SETTINGS, **PR_SETTINGS, 6: EDITED_SETTING}
            repo.commit(
                "Change settings 5 to 7", {shared: settings(edited)}, amend=True
            )
        v.append(repo.push(branch))
        shas[title] = v

    # The PR never touches a file that upstream adds between the versions;
    # the rebased version edits two of its lines and adds one.
    d = "upstream_file"
    shared = f"{d}/shared.py"
    repo.checkout("main")
    repo.commit(f"Add {d}", {shared: settings({})})
    repo.push("main")
    branch = f"case/{d}"
    repo.checkout(branch, "main")
    v = [repo.commit("Change settings 5 to 7", {shared: settings(PR_SETTINGS)})]
    repo.push(branch)
    prs[UPSTREAM_FILE] = repo.open_pr(UPSTREAM_FILE, head=branch)
    repo.checkout("main")
    repo.commit(
        "Add a file upstream", {UPSTREAM_ADDED: "\n".join(UPSTREAM_ADDED_LINES) + "\n"}
    )
    repo.push("main")
    repo.checkout(branch)
    repo.rebase("main")
    edited = list(UPSTREAM_ADDED_LINES)
    edited[4] = UPSTREAM_ADDED_EDITS[0]
    edited[9] = UPSTREAM_ADDED_EDITS[1]
    edited.append(UPSTREAM_ADDED_EDITS[2])
    repo.commit(
        "Edit the file upstream added", {UPSTREAM_ADDED: "\n".join(edited) + "\n"}
    )
    v.append(repo.push(branch))
    shas[UPSTREAM_FILE] = v

    # The PR and upstream change the same line; the rebased version is a
    # commit on the newer main resolving it the PR's way.
    repo.checkout("main")
    repo.commit("Add conflict", {CONFLICT_PATH: settings({})})
    repo.push("main")
    branch = "case/conflict"
    repo.checkout(branch, "main")
    v = [
        repo.commit("Change setting 10", {CONFLICT_PATH: settings({10: PR_SETTING_10})})
    ]
    repo.push(branch)
    prs[CONFLICT] = repo.open_pr(CONFLICT, head=branch)
    repo.checkout("main")
    repo.commit(
        "Change setting 10 upstream",
        {CONFLICT_PATH: settings({10: UPSTREAM_SETTING_10})},
    )
    repo.push("main")
    # A rebase would conflict, so version 2 is committed on the new main.
    repo.checkout(branch, "main")
    repo.commit(
        "Change setting 10", {CONFLICT_PATH: settings({10: RESOLVED_SETTING_10})}
    )
    v.append(repo.push(branch))
    shas[CONFLICT] = v

    repo.checkout("main")
    repo.commit(
        "Add files to change",
        {
            "kinds/modified.txt": "one\ntwo\nthree\n",
            "kinds/deleted.txt": "going away\n",
            "kinds/old_name.txt": "a file that only moves\n" * 5,
        },
    )
    repo.push("main")
    repo.checkout("case/file-kinds", "main")
    v = [
        repo.commit(
            FILE_KINDS,
            {
                "kinds/added.txt": "brand new\n",
                "kinds/deleted.txt": None,
                "kinds/modified.txt": "one\n2\nthree\n",
                "kinds/old_name.txt": None,
                "kinds/new_name.txt": "a file that only moves\n" * 5,
            },
        )
    ]
    repo.push("case/file-kinds")
    prs[FILE_KINDS] = repo.open_pr(FILE_KINDS, head="case/file-kinds")
    shas[FILE_KINDS] = v

    repo.checkout("case/big-diff", "main")
    v = [
        repo.commit(BIG_DIFF, {path: big_module(i) for i, path in enumerate(BIG_FILES)})
    ]
    repo.push("case/big-diff")
    prs[BIG_DIFF] = repo.open_pr(BIG_DIFF, head="case/big-diff")
    shas[BIG_DIFF] = v

    return Versions(prs, shas)


REVIEW_PATH = "review/notes.md"
REVIEW_LINES = [f"Line {i}" for i in range(1, 11)]
REPLY = "Reply to a thread"
RESOLVE = "Resolve a thread"
CONVERSE = "Comment on the PR"
FAIL = "Fail to send a reply"
LOCKED = "Have a thread I can't resolve"
LINE_REVIEW = "Review some lines"
APPROVE = "Approve without comments"
DISCARD = "Discard a review"
REPLY_IN_REVIEW = "Reply in a review"
OWN = "Review my own PR"
LEFT_LINE = "Comment on a removed line"
KEEP = "Keep boxes through refetches"
REPLY_NOW = "Reply now during a review"
COMMENT_REVIEW = "Submit comments without a summary"
SQUASH = "Squash and merge me"
STALE_HEAD = "Move on before merging"
BLOCKED = "Wait for the rules"
BYPASS = "Merge past the rules"
CONVERSATIONS = "Resolve the conversations"
KEEP_BRANCH = "Keep my branch"
AUTO_DELETE = "Leave the branch to GitHub"
SETTLE = "Wait for GitHub to decide"
STACK_PARENT = "Carry another PR"
STACK_CHILD = "Sit on another PR"
REVIEW_CASES = [
    REPLY,
    RESOLVE,
    CONVERSE,
    FAIL,
    LOCKED,
    LINE_REVIEW,
    APPROVE,
    DISCARD,
    REPLY_IN_REVIEW,
    OWN,
    LEFT_LINE,
    KEEP,
    REPLY_NOW,
    COMMENT_REVIEW,
    SQUASH,
    STALE_HEAD,
    BLOCKED,
    BYPASS,
    CONVERSATIONS,
    KEEP_BRANCH,
    AUTO_DELETE,
    SETTLE,
    STACK_PARENT,
    STACK_CHILD,
]
# A file on main, which LEFT_LINE removes the third line of.
OLD_PATH = "review/old.md"
OLD_LINES = [f"Old line {i}" for i in range(1, 6)]


def review_cases(gh: FakeGitHub) -> dict[str, PullRequest]:
    """One PR per review action, each with an unresolved thread on line 2,
    so that the tests don't see each other's changes. All are by the other
    account but OWN, which is the viewer's. All target main but
    STACK_CHILD, which is stacked on STACK_PARENT."""
    repo = gh.repo(REPO)
    repo.checkout("main")
    repo.commit(
        "Seed the repo",
        {"README.md": "# test\n", OLD_PATH: "\n".join(OLD_LINES) + "\n"},
    )
    repo.push("main")
    prs = {}
    for i, title in enumerate(REVIEW_CASES):
        # Only dev/ branches stack.
        stacked = title in (STACK_PARENT, STACK_CHILD)
        branch = f"dev/review/{i}" if stacked else f"review/{i}"
        base = prs[STACK_PARENT].head if title == STACK_CHILD else "main"
        repo.checkout(branch, base)
        lines = [f"{line} of {title}" for line in REVIEW_LINES]
        author = VIEWER if title == OWN else OTHER
        files = {REVIEW_PATH: "\n".join(lines) + "\n"}
        if title == LEFT_LINE:
            kept = OLD_LINES[:2] + OLD_LINES[3:]
            files[OLD_PATH] = "\n".join(kept) + "\n"
        repo.commit(title, files, author=author)
        repo.push(branch, actor=author)
        pr = repo.open_pr(title, head=branch, base=base, author=author)
        root = repo.comment(
            pr, author=OTHER, body="Is this right?", path=REVIEW_PATH, line=2
        )
        root.can_resolve = title != LOCKED
        if title in (BLOCKED, BYPASS):
            pr.merge_state = "BLOCKED"
        if title == CONVERSATIONS:
            pr.thread_resolution = "ruleset"
        if title == BYPASS:
            pr.review_required = True
            pr.can_merge_as_admin = True
        if title == KEEP_BRANCH:
            repo.undeletable_branches.add(branch)
        prs[title] = pr
    return prs


QUEUE_REPO = "test/queue"
QUEUE_READY = "Join the queue"
QUEUE_WAITING = "Join it when ready"
QUEUE_NO_AUTO = "Wait out the rules"
QUEUE_ADMIN = "Jump the queue"
QUEUE_BYPASS = "Merge past the queue"
QUEUED = "Wait in the queue"
QUEUE_CASES = [
    QUEUE_READY,
    QUEUE_WAITING,
    QUEUE_NO_AUTO,
    QUEUE_ADMIN,
    QUEUE_BYPASS,
    QUEUED,
]


def queue_cases(gh: FakeGitHub) -> dict[str, PullRequest]:
    """PRs on a repository whose main branch requires a squash merge
    queue: a ready one, a blocked one that may auto-merge, a blocked one
    that may not, two an admin may act on, and one already queued."""
    repo = gh.repo(QUEUE_REPO)
    repo.merge_queue = "SQUASH"
    repo.checkout("main")
    repo.commit("Seed the repo", {"README.md": "# queue\n"})
    repo.push("main")
    prs = {}
    for i, title in enumerate(QUEUE_CASES):
        branch = f"queue/{i}"
        repo.checkout(branch, "main")
        repo.commit(title, {f"queue/{i}.md": f"{title}\n"}, author=OTHER)
        repo.push(branch, actor=OTHER)
        pr = repo.open_pr(title, head=branch, base="main", author=OTHER)
        if title == QUEUE_WAITING:
            pr.merge_state = "BLOCKED"
        if title == QUEUE_NO_AUTO:
            pr.merge_state = "BLOCKED"
            pr.can_auto_merge = False
        if title in (QUEUE_ADMIN, QUEUE_BYPASS):
            pr.can_merge_as_admin = True
        if title == QUEUE_BYPASS:
            pr.merge_state = "BLOCKED"
        if title == QUEUED:
            pr.queued = True
        prs[title] = pr
    return prs
