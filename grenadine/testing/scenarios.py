"""PRs to load a FakeGitHub with, and where grenadine should show them."""

import dataclasses
import re
from pathlib import Path

from grenadine.testing.fake_github import (
    APPROVED,
    CHANGES_REQUESTED,
    COMMENTED,
    VIEWER,
    FakeGitHub,
    FakeRepo,
    PullRequest,
    Review,
)

REPO = "test/test"
OTHER = "other"

# The default inboxes, as named in grenadine/server/src/inboxes.rs.
NEEDS_REVIEW = "Needs your review"
NEEDS_TEAM_REVIEW = "Needs your teams' review"
RETURNED = "Returned to you"
APPROVED_INBOX = "Approved"
WAITING_FOR_REVIEWERS = "Waiting for reviewers"
DRAFTS = "Drafts"
MERGING = "Merging"
DRAFTS_NEEDING_REVIEW = "Drafts needing your review"
WAITING_FOR_AUTHORS = "Waiting for authors"

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


def default_inboxes() -> list[tuple[str, str]]:
    """The `(name, filter)` pairs of the server's default inboxes, read from
    its source so that they can't drift."""
    src = (Path(__file__).parents[1] / "server/src/inboxes.rs").read_text()
    return re.findall(r'\(\s*"([^"]*)",\s*"([^"]*)",\s*\)', src)


def server_query(filter: str, repos: list[str]) -> str:
    """The search query the server sends for an inbox, as built by
    `github::search_query`."""
    tokens = filter.split()
    sorts = [t for t in tokens if t.startswith("sort:")]
    terms = [t for t in tokens if not t.startswith("sort:")]
    parts = ["is:pr"]
    if terms:
        parts.append(f"({' '.join(terms)})")
    if repos:
        parts.append(f"({' OR '.join(f'repo:{r}' for r in repos)})")
    return " ".join(parts + sorts)


@dataclasses.dataclass
class Inboxes:
    """The scenario's PRs by title, and the inbox each belongs in (None for
    no inbox)."""

    prs: dict[str, PullRequest]
    expected: dict[str, str | None]

    def titles_in(self, inbox: str) -> list[str]:
        """The titles `inbox` should list, most recently updated first."""
        prs = [self.prs[t] for t, i in self.expected.items() if i == inbox]
        return [pr.title for pr in sorted(prs, key=lambda pr: pr.updated_at, reverse=True)]


def _simple_pr(repo: FakeRepo, case: str, title: str, *, author: str = VIEWER,
               draft: bool = False, body: str = "") -> PullRequest:
    branch = f"case/{case}"
    repo.checkout(branch, "main")
    repo.commit(title, {f"inbox/{case}.md": f"# {title}\n"}, author=author)
    repo.push(branch, actor=author)
    return repo.open_pr(title, head=branch, author=author, draft=draft, body=body or title)


def inbox_cases(gh: FakeGitHub) -> Inboxes:
    """One PR for each default inbox, PRs that belong in none, and a
    two-PR stack."""
    repo = gh.repo(REPO)
    repo.checkout("main")
    repo.commit("Seed the repo", {"README.md": "# test\n"})
    repo.push("main")

    prs: dict[str, PullRequest] = {}
    expected: dict[str, str | None] = {}

    def add(pr: PullRequest, inbox: str | None) -> PullRequest:
        prs[pr.title] = pr
        expected[pr.title] = inbox
        return pr

    pr = add(_simple_pr(repo, "needs-review", "Ask me for a review", author=OTHER), NEEDS_REVIEW)
    pr.review_requests.add(VIEWER)

    pr = add(_simple_pr(repo, "needs-team-review", "Ask my team for a review", author=OTHER),
             NEEDS_TEAM_REVIEW)
    pr.team_review_requests.add(VIEWER)

    # GitHub counts a comment-only review as no review; see todo.md.
    pr = add(_simple_pr(repo, "returned-commented", "Get a comment-only review"),
             WAITING_FOR_REVIEWERS)
    pr.reviews.append(Review(OTHER, COMMENTED))

    pr = add(_simple_pr(repo, "returned-changes", "Get changes requested"), RETURNED)
    pr.reviews.append(Review(OTHER, CHANGES_REQUESTED))

    pr = add(_simple_pr(repo, "approved", "Get approved", body=DESCRIPTION), APPROVED_INBOX)
    pr.reviews.append(Review(OTHER, APPROVED))

    add(_simple_pr(repo, "waiting-reviewers", "Wait for reviewers"), WAITING_FOR_REVIEWERS)
    add(_simple_pr(repo, "draft", "Stay a draft", draft=True), DRAFTS)

    pr = add(_simple_pr(repo, "merging", "Sit in the merge queue"), MERGING)
    pr.reviews.append(Review(OTHER, APPROVED))
    pr.queued = True

    pr = add(_simple_pr(repo, "draft-needs-review", "Ask me to look at a draft", author=OTHER,
                        draft=True), DRAFTS_NEEDING_REVIEW)
    pr.review_requests.add(VIEWER)

    pr = add(_simple_pr(repo, "waiting-authors", "Wait for the author after my review",
                        author=OTHER), WAITING_FOR_AUTHORS)
    pr.reviews.append(Review(VIEWER, COMMENTED))

    pr = add(_simple_pr(repo, "closed", "Get closed"), None)
    repo.close(pr)
    pr = add(_simple_pr(repo, "merged", "Get merged"), None)
    repo.merge(pr)
    add(_simple_pr(repo, "uninvolved", "Leave me out of it", author=OTHER), None)

    # Stacks only form on non-trunk branches, which are dev/ ones by default.
    repo.checkout("dev/stack-base", "main")
    repo.commit(STACK_BASE, {"stack/base.txt": "base\n"})
    repo.push("dev/stack-base")
    add(repo.open_pr(STACK_BASE, head="dev/stack-base"), WAITING_FOR_REVIEWERS)
    repo.checkout("dev/stack-top")
    repo.commit(STACK_TOP, {"stack/top.txt": "top\n"})
    repo.push("dev/stack-top")
    add(repo.open_pr(STACK_TOP, head="dev/stack-top", base="dev/stack-base"),
        WAITING_FOR_REVIEWERS)

    return Inboxes(prs, expected)


GROW = "Grow one push at a time"
REBASE = "Rebase onto a newer main"
REBASE_AND_EDIT = "Rebase and edit in one push"
FILE_KINDS = "Change every kind of file"

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
PR_SETTINGS = {i: f"SETTING_{i:02d} = {i} + 100  # changed by the PR" for i in (5, 6, 7)}
UPSTREAM_SETTINGS = {50: "SETTING_50 = 50 - 1  # changed upstream"}
EDITED_SETTING = "SETTING_06 = 6 + 200  # edited after the rebase"


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

    for title, edit in ((REBASE, False), (REBASE_AND_EDIT, True)):
        d = "rebase_and_edit" if edit else "rebase"
        shared, unrelated, upstream_only = f"{d}/shared.py", f"{d}/unrelated.txt", f"{d}/upstream_only.txt"
        repo.checkout("main")
        repo.commit(f"Add {d}", {shared: settings({}), unrelated: "upstream, before\n"})
        repo.push("main")
        branch = f"case/{d}"
        repo.checkout(branch, "main")
        v = [repo.commit("Change settings 5 to 7", {shared: settings(PR_SETTINGS)})]
        repo.push(branch)
        pr = prs[title] = repo.open_pr(title, head=branch)

        repo.checkout("main")
        repo.commit("Change things upstream", {
            shared: settings(UPSTREAM_SETTINGS),
            unrelated: "upstream, after\n",
            upstream_only: "Only main added this file.\n",
        })
        repo.push("main")
        repo.checkout(branch)
        repo.rebase("main")
        if edit:
            edited = {**UPSTREAM_SETTINGS, **PR_SETTINGS, 6: EDITED_SETTING}
            repo.commit("Change settings 5 to 7", {shared: settings(edited)}, amend=True)
        v.append(repo.push(branch))
        shas[title] = v

    repo.checkout("main")
    repo.commit("Add files to change", {
        "kinds/modified.txt": "one\ntwo\nthree\n",
        "kinds/deleted.txt": "going away\n",
        "kinds/old_name.txt": "a file that only moves\n" * 5,
    })
    repo.push("main")
    repo.checkout("case/file-kinds", "main")
    v = [repo.commit(FILE_KINDS, {
        "kinds/added.txt": "brand new\n",
        "kinds/deleted.txt": None,
        "kinds/modified.txt": "one\n2\nthree\n",
        "kinds/old_name.txt": None,
        "kinds/new_name.txt": "a file that only moves\n" * 5,
    })]
    repo.push("case/file-kinds")
    prs[FILE_KINDS] = repo.open_pr(FILE_KINDS, head="case/file-kinds")
    shas[FILE_KINDS] = v

    return Versions(prs, shas)
