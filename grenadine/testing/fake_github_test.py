"""Tests for the fake GitHub that the UI tests run against."""

import json
import os
import subprocess
import tempfile
import unittest
import urllib.error
import urllib.request
from pathlib import Path

from grenadine.testing import scenarios
from grenadine.testing.fake_github import (
    TOKEN,
    FakeGitHub,
    SearchError,
    matches,
    search_prs,
)

# Copied from grenadine/server/src/github.rs, to check that the fake accepts
# what the server sends.
PR_QUERY = """
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      title body url state isDraft createdAt updatedAt
      author { login }
      baseRefName headRefName headRefOid
      headRepository { nameWithOwner }
      timelineItems(first: 100, itemTypes: [HEAD_REF_FORCE_PUSHED_EVENT]) {
        nodes {
          ... on HeadRefForcePushedEvent {
            createdAt
            beforeCommit { oid }
            afterCommit { oid }
            actor { login avatarUrl ... on User { name } }
          }
        }
      }
      commits(first: 100) {
        nodes { commit { oid checkSuites(first: 10) { nodes { createdAt } } } }
      }
    }
  }
}
"""

THREADS_QUERY = """
query($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { isResolved comments(first: 1) { nodes { databaseId } } }
      }
    }
  }
}
"""

SEARCH_QUERY = """
query($q0: String!, $q1: String!) {
  s0: search(type: ISSUE_ADVANCED, query: $q0, first: 100) {
    nodes { ... on PullRequest { number title author { login } state isDraft url updatedAt headRefOid repository { nameWithOwner } } }
  }
  s1: search(type: ISSUE_ADVANCED, query: $q1, first: 100) {
    nodes { ... on PullRequest { number title author { login } state isDraft url updatedAt headRefOid repository { nameWithOwner } } }
  }
}
"""


class FakeGitHubTest(unittest.TestCase):
    def setUp(self) -> None:
        self.gh = FakeGitHub(Path(tempfile.mkdtemp(dir=os.environ.get("TEST_TMPDIR"))))
        self.inboxes = scenarios.inbox_cases(self.gh)
        self.repo = self.gh.repo(scenarios.REPO)

    def query(self, query: str, **variables: object) -> dict:
        return self.gh.graphql({"query": query, "variables": variables})

    def test_default_inboxes_hold_the_expected_prs(self) -> None:
        inboxes = scenarios.default_inboxes()
        self.assertEqual(len(inboxes), 9)
        for name, filter in inboxes:
            with self.subTest(inbox=name):
                found = search_prs(self.gh.prs(), scenarios.server_query(filter, [scenarios.REPO]))
                self.assertEqual([pr.title for pr in found], self.inboxes.titles_in(name))
        # Every expected inbox exists.
        names = {name for name, _ in inboxes}
        self.assertLessEqual(set(self.inboxes.expected.values()) - {None}, names)

    def test_search_syntax(self) -> None:
        pr = self.inboxes.prs["Get changes requested"]
        self.assertTrue(matches(pr, "is:pr (author:@me) (repo:test/test OR repo:a/b)"))
        self.assertFalse(matches(pr, "is:pr -author:@me"))
        self.assertTrue(matches(pr, "author:other OR review:changes_requested"))
        # AND binds tighter than OR.
        self.assertTrue(matches(pr, "author:other review:none OR state:open"))
        self.assertFalse(matches(pr, "author:other (review:none OR state:open)"))
        self.assertTrue(matches(pr, "changes"))
        with self.assertRaises(SearchError):
            matches(pr, "label:bug")
        with self.assertRaises(SearchError):
            matches(pr, "(state:open")

    def test_search_aliases_fail_independently(self) -> None:
        out = self.query(SEARCH_QUERY, q0="is:pr author:other sort:updated-desc", q1="label:bug")
        titles = [n["title"] for n in out["data"]["s0"]["nodes"]]
        self.assertEqual(titles[-1], "Ask me for a review")
        self.assertIsNone(out["data"]["s1"])
        self.assertEqual(out["errors"][0]["path"], ["s1"])

    def test_pr_query_reports_force_pushes(self) -> None:
        pr = self.inboxes.prs["Wait for reviewers"]
        first = pr.head_oid
        self.repo.checkout(pr.head)
        second = self.repo.commit("Amended", {"inbox/amended.md": "x\n"}, amend=True)
        self.repo.push(pr.head)

        out = self.query(PR_QUERY, owner="test", name="test", number=pr.number)
        self.assertNotIn("errors", out)
        data = out["data"]["repository"]["pullRequest"]
        self.assertEqual(data["headRefOid"], second)
        self.assertEqual(data["author"], {"login": "me"})
        self.assertEqual(data["headRepository"], {"nameWithOwner": "test/test"})
        [event] = data["timelineItems"]["nodes"]
        self.assertEqual(event["beforeCommit"]["oid"], first)
        self.assertEqual(event["afterCommit"]["oid"], second)
        self.assertEqual(event["actor"]["name"], "Me Myself")
        self.assertEqual([n["commit"]["oid"] for n in data["commits"]["nodes"]], [second])

    def test_unknown_fields_are_errors(self) -> None:
        out = self.query("query { repository(owner: \"test\", name: \"test\") { nope } }")
        self.assertIn("Cannot query field 'nope'", out["errors"][0]["message"])

    def test_threads_users_and_commit_authors(self) -> None:
        pr = self.inboxes.prs["Get changes requested"]
        root = self.repo.comment(pr, author="other", body="Hmm", path="inbox/x.md", line=1)
        self.repo.comment(pr, author="me", body="Fixed", path="inbox/x.md", line=1, reply_to=root)
        root.resolved = True
        out = self.query(THREADS_QUERY, owner="test", name="test", number=pr.number)
        threads = out["data"]["repository"]["pullRequest"]["reviewThreads"]
        self.assertEqual(threads["nodes"], [
            {"isResolved": True, "comments": {"nodes": [{"databaseId": root.id}]}},
        ])

        out = self.query('query { u0: user(login: "other") { login name avatarUrl } '
                         'u1: user(login: "nobody") { login } }')
        self.assertEqual(out["data"]["u0"], {"login": "other", "name": "Other Person", "avatarUrl": None})
        self.assertIsNone(out["data"]["u1"])

        sha = self.inboxes.prs["Ask me for a review"].head_oid
        out = self.query(
            "query($o0: GitObjectID!) { repository(owner: \"test\", name: \"test\") {"
            " c0: object(oid: $o0) { ... on Commit { author { name user { login } } } } } }",
            o0=sha)
        self.assertEqual(out["data"]["repository"]["c0"],
                         {"author": {"name": "Other Person", "user": {"login": "other"}}})

    def test_rest_endpoints(self) -> None:
        base = self.gh.start()
        self.addCleanup(self.gh.stop)

        def get(path: str, token: str = TOKEN) -> object:
            req = urllib.request.Request(base + path, headers={"Authorization": f"Bearer {token}"})
            with urllib.request.urlopen(req) as resp:
                return json.load(resp)

        with self.assertRaises(urllib.error.HTTPError) as e:
            get("/repos/test/test/activity?ref=refs/heads/main", token="wrong")
        self.assertEqual(e.exception.code, 401)

        pr = self.inboxes.prs[scenarios.STACK_TOP]
        activity = get(f"/repos/test/test/activity?ref=refs/heads/{pr.head}&direction=asc")
        self.assertEqual([a["activity_type"] for a in activity], ["branch_creation"])
        self.assertEqual(activity[0]["after"], pr.head_oid)

        self.repo.comment(pr, author="other", body="Nit", path="stack/top.txt", line=1)
        [comment] = get(f"/repos/test/test/pulls/{pr.number}/comments?per_page=100")
        self.assertEqual(comment["body"], "Nit")
        self.assertEqual(comment["original_commit_id"], pr.head_oid)

    def test_github_urls_resolve_to_the_bare_repo(self) -> None:
        clone = self.gh.root / "clone"
        env = self.gh.git_env()
        subprocess.run(["git", "init", "--quiet", str(clone)], env=env, check=True)
        pr = self.inboxes.prs["Wait for reviewers"]
        old = pr.head_oid
        self.repo.checkout(pr.head)
        self.repo.commit("Amended", {}, amend=True)
        self.repo.push(pr.head)
        # A force-pushed-away commit can still be fetched by SHA.
        subprocess.run(["git", "-C", str(clone), "fetch", "--quiet",
                        "https://github.com/test/test.git", old], env=env, check=True)


if __name__ == "__main__":
    unittest.main()
