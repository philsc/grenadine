"""Tests for the fake GitHub that the UI tests run against."""

import copy
import json
import os
import shutil
import subprocess
import tempfile
import unittest
import urllib.error
import urllib.request
from pathlib import Path

from grenadine.testing import scenarios
from grenadine.testing.fake_github import (
    APPROVED,
    REVIEW_REQUIRED,
    TEAM,
    TEAM_SLUG,
    TOKEN,
    CheckRun,
    FakeGitHub,
    PullRequest,
    Review,
    SearchError,
    matches,
    search_prs,
)

# Copied from grenadine/server/src/github.rs, to check that the fake accepts
# what the server sends.
PR_QUERY = """
query($owner: String!, $name: String!, $number: Int!) {
  rateLimit { cost }
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      title body url state isDraft createdAt updatedAt
      author { login }
      baseRefName headRefName headRefOid
      baseRef {
        refUpdateRule { requiresConversationResolution }
        rules(first: 100) {
          nodes {
            parameters {
              ... on PullRequestParameters { requiredReviewThreadResolution }
            }
          }
        }
      }
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
      reviewDecision mergeable mergeStateStatus viewerCanMergeAsAdmin
      viewerCanUpdate
      reviews(last: 100) {
        nodes { state body submittedAt url author { ...Who } }
      }
      pending: reviews(states: [PENDING], first: 1) {
        nodes {
          id viewerDidAuthor
          comments(first: 100) {
            nodes {
              id databaseId body path line startLine originalLine originalStartLine
              subjectType createdAt url
              replyTo { databaseId }
              originalCommit { oid }
            }
          }
        }
      }
      reviewRequests(first: 20) {
        nodes {
          requestedReviewer {
            __typename
            ... on User { login name avatarUrl }
            # Team's name isn't nullable while User's is; the same field
            # can't be both, so the team's is aliased.
            ... on Team { teamName: name slug }
            ... on Bot { login avatarUrl }
            ... on Mannequin { login }
          }
        }
      }
      comments(last: 100) {
        nodes { body createdAt url author { ...Who } }
      }
      head: commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup { state ...Checks }
          }
        }
      }
    }
    mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed
    viewerDefaultMergeMethod deleteBranchOnMerge
  }
  viewer { login }
}

fragment Who on Actor { login avatarUrl ... on User { name } }

fragment Checks on StatusCheckRollup {
  contexts(first: 100) {
    nodes {
      ... on CheckRun {
        name status conclusion detailsUrl startedAt
        checkSuite { workflowRun { workflow { name } } }
      }
      ... on StatusContext { context state targetUrl }
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
        nodes {
          id diffSide
          isResolved viewerCanResolve viewerCanUnresolve
          resolvedBy { login name avatarUrl }
          comments(first: 1) { nodes { databaseId } }
        }
      }
    }
  }
}
"""

SEARCH_QUERY = """
query($q0: String!, $q1: String!) {
  viewer { login }
  s0: search(type: ISSUE_ADVANCED, query: $q0, first: 100) {
    issueCount pageInfo { hasNextPage endCursor }
    nodes { ... on PullRequest { number title author { login } state isDraft url updatedAt headRefOid
      reviewThreads(first: 100) { totalCount nodes { isResolved } }
      commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      isInMergeQueue reviewDecision repository { nameWithOwner isArchived }
      reviews(first: 100) { nodes { author { __typename login } state } }
      reviewRequests(first: 100) { nodes { requestedReviewer { __typename ... on User { login } } } } } }
  }
  s1: search(type: ISSUE_ADVANCED, query: $q1, first: 100) {
    issueCount pageInfo { hasNextPage endCursor }
    nodes { ... on PullRequest { number title author { login } state isDraft url updatedAt headRefOid
      reviewThreads(first: 100) { totalCount nodes { isResolved } }
      commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      isInMergeQueue reviewDecision repository { nameWithOwner isArchived }
      reviews(first: 100) { nodes { author { __typename login } state } }
      reviewRequests(first: 100) { nodes { requestedReviewer { __typename ... on User { login } } } } } }
  }
}
"""


class FakeGitHubTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        tmp = Path(tempfile.mkdtemp(dir=os.environ.get("TEST_TMPDIR")))
        cls.root = tmp / "github"
        cls.template = tmp / "template"
        gh = FakeGitHub(cls.root)
        inboxes = scenarios.inbox_cases(gh)
        # The repos move aside to become the template; each test restores a
        # copy at `root` so the absolute paths baked into them stay valid.
        shutil.move(cls.root, cls.template)
        cls.template_state = (gh, inboxes)

    def setUp(self) -> None:
        shutil.rmtree(self.root, ignore_errors=True)
        shutil.copytree(self.template, self.root, symlinks=True)
        # Copied together so `inboxes.prs` point at the copy's PRs.
        self.gh, self.inboxes = copy.deepcopy(self.template_state)
        self.repo = self.gh.repo(scenarios.REPO)

    def query(self, query: str, **variables: object) -> dict:
        return self.gh.graphql({"query": query, "variables": variables})

    def test_default_inboxes_hold_the_expected_prs(self) -> None:
        inboxes = scenarios.default_inboxes()
        self.assertEqual(len(inboxes), 14)
        for name, filter, *_ in inboxes:
            with self.subTest(inbox=name):
                found = search_prs(
                    self.gh.prs(), scenarios.server_query(filter, [scenarios.REPO])
                )
                # Local inboxes then keep the hits their filter matches.
                if "gr:" in filter:
                    found = [
                        pr
                        for pr in found
                        if scenarios.matches(pr, filter, bots=self.gh.bots)
                    ]
                    if "gr:sort:title-asc" in filter:
                        found.sort(key=lambda pr: pr.title.lower())
                got = [pr.title for pr in found]
                self.assertEqual(got, self.inboxes.titles_in(name))
        # Every expected inbox exists.
        names = {name for name, *_ in inboxes}
        self.assertLessEqual(
            {i for inboxes in self.inboxes.expected.values() for i in inboxes},
            names,
        )

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
        out = self.query(
            SEARCH_QUERY, q0="is:pr author:other sort:updated-desc", q1="label:bug"
        )
        titles = [n["title"] for n in out["data"]["s0"]["nodes"]]
        self.assertEqual(titles[-1], "Ask me for a review")
        self.assertIsNone(out["data"]["s1"])
        self.assertEqual(out["errors"][0]["path"], ["s1"])

    def test_search_reports_threads_and_checks(self) -> None:
        pr = self.inboxes.prs["Get changes requested"]
        root = self.repo.comment(
            pr, author="other", body="Hmm", path="inbox/x.md", line=1
        )
        self.repo.comment(
            pr, author="me", body="Fixed", path="inbox/x.md", line=1, reply_to=root
        )
        self.repo.comment(pr, author="other", body="And?", path="inbox/x.md", line=1)
        root.resolved = True
        # Like GitHub's, the state counts the run that a later one
        # superseded.
        pr.check_runs = [
            CheckRun("test", conclusion="CANCELLED", started_at=1, workflow="Test"),
            CheckRun("test", started_at=2, workflow="Test"),
        ]
        out = self.query(
            SEARCH_QUERY, q0=f"is:pr {pr.title}", q1="is:pr Wait for reviewers"
        )
        node = next(n for n in out["data"]["s0"]["nodes"] if n["number"] == pr.number)
        self.assertEqual(
            node["reviewThreads"],
            {
                "totalCount": 2,
                "nodes": [{"isResolved": True}, {"isResolved": False}],
            },
        )
        self.assertEqual(
            node["commits"],
            {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]},
        )
        [other] = out["data"]["s1"]["nodes"]
        self.assertEqual(other["reviewThreads"], {"totalCount": 0, "nodes": []})
        self.assertEqual(
            other["commits"], {"nodes": [{"commit": {"statusCheckRollup": None}}]}
        )

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
        self.assertEqual(
            [n["commit"]["oid"] for n in data["commits"]["nodes"]], [second]
        )

    def test_pr_query_reports_reviews_and_merge_state(self) -> None:
        pr = self.inboxes.prs["Wait for reviewers"]
        self.repo.review(pr, author="other", state=APPROVED, body="LGTM")
        self.repo.issue_comment(pr, author="other", body="Thanks!")
        pr.review_requests.add("third")
        pr.team_review_requests.add("me")
        pr.check_runs = [
            CheckRun("lint", started_at=1, workflow="Lint"),
            CheckRun("test", conclusion="FAILURE"),
            CheckRun("deploy", status="IN_PROGRESS", conclusion=None),
        ]
        self.repo.merge_methods = ["SQUASH", "MERGE"]

        out = self.query(PR_QUERY, owner="test", name="test", number=pr.number)
        self.assertNotIn("errors", out)
        self.assertEqual(out["data"]["viewer"], {"login": "me"})
        repo = out["data"]["repository"]
        self.assertEqual(
            {k: v for k, v in repo.items() if k != "pullRequest"},
            {
                "mergeCommitAllowed": True,
                "squashMergeAllowed": True,
                "rebaseMergeAllowed": False,
                "viewerDefaultMergeMethod": "SQUASH",
                "deleteBranchOnMerge": False,
            },
        )
        data = repo["pullRequest"]
        self.assertEqual(data["reviewDecision"], "APPROVED")
        self.assertEqual(data["mergeable"], "MERGEABLE")
        self.assertEqual(data["mergeStateStatus"], "UNSTABLE")
        [review] = data["reviews"]["nodes"]
        self.assertEqual(review["state"], "APPROVED")
        self.assertEqual(review["body"], "LGTM")
        self.assertEqual(review["author"]["name"], "Other Person")
        self.assertEqual(
            data["reviewRequests"]["nodes"],
            [
                {
                    "requestedReviewer": {
                        "__typename": "User",
                        "login": "third",
                        "name": None,
                        "avatarUrl": None,
                    }
                },
                {
                    "requestedReviewer": {
                        "__typename": "Team",
                        "teamName": TEAM,
                        "slug": TEAM_SLUG,
                    }
                },
            ],
        )
        [comment] = data["comments"]["nodes"]
        self.assertEqual(comment["body"], "Thanks!")
        rollup = data["head"]["nodes"][0]["commit"]["statusCheckRollup"]
        self.assertEqual(rollup["state"], "FAILURE")
        self.assertEqual(
            [
                (
                    c["name"],
                    c["status"],
                    c["conclusion"],
                    c["startedAt"],
                    c["checkSuite"],
                )
                for c in rollup["contexts"]["nodes"]
            ],
            [
                (
                    "lint",
                    "COMPLETED",
                    "SUCCESS",
                    "1970-01-01T00:00:01Z",
                    {"workflowRun": {"workflow": {"name": "Lint"}}},
                ),
                ("test", "COMPLETED", "FAILURE", None, {"workflowRun": None}),
                ("deploy", "IN_PROGRESS", None, None, {"workflowRun": None}),
            ],
        )

    def test_pr_query_reports_the_conversation_resolution_rule(self) -> None:
        pr = self.inboxes.prs["Wait for reviewers"]
        base_ref = lambda: self.query(
            PR_QUERY, owner="test", name="test", number=pr.number
        )["data"]["repository"]["pullRequest"]["baseRef"]

        def requires() -> tuple[bool, bool]:
            ref = base_ref()
            return (
                ref["refUpdateRule"]["requiresConversationResolution"],
                ref["rules"]["nodes"][0]["parameters"][
                    "requiredReviewThreadResolution"
                ],
            )

        self.assertEqual(requires(), (False, False))
        pr.thread_resolution = "protection"
        self.assertEqual(requires(), (True, False))
        pr.thread_resolution = "ruleset"
        self.assertEqual(requires(), (False, True))

        # The rule makes an unresolved thread block merging.
        self.repo.comment(pr, author="other", body="Is this right?", path="p", line=1)
        self.assertEqual(pr.merge_state_status(), "BLOCKED")
        pr.comments[0].resolved = True
        self.assertEqual(pr.merge_state_status(), "CLEAN")

    def test_pr_query_reports_required_reviews(self) -> None:
        pr = self.inboxes.prs["Wait for reviewers"]
        pr.review_required = True
        pr.merge_state = "BLOCKED"
        out = self.query(PR_QUERY, owner="test", name="test", number=pr.number)
        data = out["data"]["repository"]["pullRequest"]
        self.assertEqual(data["reviewDecision"], REVIEW_REQUIRED)
        self.assertEqual(data["mergeStateStatus"], "BLOCKED")
        # An approval settles it.
        self.repo.review(pr, author="other", state=APPROVED)
        out = self.query(PR_QUERY, owner="test", name="test", number=pr.number)
        data = out["data"]["repository"]["pullRequest"]
        self.assertEqual(data["reviewDecision"], APPROVED)

    def test_mutations(self) -> None:
        pr = self.inboxes.prs["Get changes requested"]
        root = self.repo.comment(
            pr, author="other", body="Hmm", path="inbox/x.md", line=1
        )
        pr_id = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {pr.number}) {{ id }} }} }}"
        )["data"]["repository"]["pullRequest"]["id"]

        out = self.query(
            'mutation($s: ID!) { addComment(input: {subjectId: $s, body: "Done"})'
            " { clientMutationId } }",
            s=pr_id,
        )
        self.assertNotIn("errors", out)
        self.assertEqual(pr.issue_comments[-1].body, "Done")
        self.assertEqual(pr.issue_comments[-1].author, "me")

        thread = f"PRRT_{root.id}"
        out = self.query(
            "mutation($t: ID!) { addPullRequestReviewThreadReply("
            'input: {pullRequestReviewThreadId: $t, body: "Fixed"}) { clientMutationId } }',
            t=thread,
        )
        self.assertNotIn("errors", out)
        reply = pr.comments[-1]
        self.assertEqual(
            (reply.body, reply.in_reply_to, reply.path), ("Fixed", root.id, root.path)
        )

        self.query(
            "mutation($t: ID!) { resolveReviewThread(input: {threadId: $t}) { clientMutationId } }",
            t=thread,
        )
        self.assertEqual((root.resolved, root.resolved_by), (True, "me"))
        self.query(
            "mutation($t: ID!) { unresolveReviewThread(input: {threadId: $t}) { clientMutationId } }",
            t=thread,
        )
        self.assertEqual((root.resolved, root.resolved_by), (False, None))

        out = self.query(
            'mutation { resolveReviewThread(input: {threadId: "nope"}) { clientMutationId } }'
        )
        self.assertIn("Could not resolve", out["errors"][0]["message"])

        # The next mutation can be made to fail, and only that one.
        self.gh.fail_next_mutation = "Something went wrong"
        resolve = (
            "mutation($t: ID!) { resolveReviewThread(input: {threadId: $t})"
            " { clientMutationId } }"
        )
        out = self.query(resolve, t=thread)
        self.assertEqual(out["errors"][0]["message"], "Something went wrong")
        self.assertFalse(root.resolved)
        self.assertNotIn("errors", self.query(resolve, t=thread))
        self.assertTrue(root.resolved)

    def test_threads_the_viewer_cannot_resolve(self) -> None:
        pr = self.inboxes.prs["Get changes requested"]
        root = self.repo.comment(
            pr, author="other", body="Hmm", path="inbox/x.md", line=1
        )
        root.can_resolve = False
        threads = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {pr.number}) {{"
            " reviewThreads(first: 10) { nodes { id viewerCanResolve } } } } }"
        )["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
        self.assertIn({"id": f"PRRT_{root.id}", "viewerCanResolve": False}, threads)
        out = self.query(
            "mutation($t: ID!) { resolveReviewThread(input: {threadId: $t})"
            " { clientMutationId } }",
            t=f"PRRT_{root.id}",
        )
        self.assertIn("not accessible", out["errors"][0]["message"])
        self.assertFalse(root.resolved)

    def test_pending_reviews(self) -> None:
        pr = self.inboxes.prs["Ask me for a review"]
        root = self.repo.comment(
            pr, author="other", body="Hmm", path="inbox/x.md", line=1
        )
        pr_id = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {pr.number}) {{ id }} }} }}"
        )["data"]["repository"]["pullRequest"]["id"]

        out = self.query(
            "mutation($p: ID!) { addPullRequestReview(input: {pullRequestId: $p})"
            " { pullRequestReview { id } } }",
            p=pr_id,
        )
        review = out["data"]["addPullRequestReview"]["pullRequestReview"]["id"]
        out = self.query(
            "mutation($p: ID!) { addPullRequestReview(input: {pullRequestId: $p})"
            " { clientMutationId } }",
            p=pr_id,
        )
        self.assertIn("one pending review", out["errors"][0]["message"])

        self.query(
            "mutation($r: ID!) { addPullRequestReviewThread(input: {pullRequestReviewId: $r,"
            ' path: "inbox/x.md", body: "Nit", line: 1, side: RIGHT}) { clientMutationId } }',
            r=review,
        )
        self.query(
            "mutation($r: ID!, $t: ID!) { addPullRequestReviewThreadReply(input: {"
            ' pullRequestReviewThreadId: $t, pullRequestReviewId: $r, body: "Agreed"})'
            " { clientMutationId } }",
            r=review,
            t=f"PRRT_{root.id}",
        )
        # Pending comments stay out of the PR's comments.
        self.assertEqual(len(pr.comments), 1)
        out = self.query(PR_QUERY, owner="test", name="test", number=pr.number)
        self.assertNotIn("errors", out)
        [pending] = out["data"]["repository"]["pullRequest"]["pending"]["nodes"]
        self.assertEqual(pending["id"], review)
        nit, agreed = pending["comments"]["nodes"]
        self.assertEqual((nit["body"], nit["line"]), ("Nit", 1))
        self.assertEqual(agreed["replyTo"], {"databaseId": root.id})

        self.query(
            "mutation($c: ID!) { updatePullRequestReviewComment(input: {"
            ' pullRequestReviewCommentId: $c, body: "Nit: typo"}) { clientMutationId } }',
            c=nit["id"],
        )
        self.query(
            "mutation($c: ID!) { deletePullRequestReviewComment(input: {id: $c})"
            " { clientMutationId } }",
            c=agreed["id"],
        )
        out = self.query(
            "mutation($r: ID!) { submitPullRequestReview(input: {pullRequestReviewId: $r,"
            ' event: APPROVE, body: "LGTM"}) { clientMutationId } }',
            r=review,
        )
        self.assertNotIn("errors", out)
        self.assertIsNone(pr.pending)
        self.assertEqual([c.body for c in pr.comments], ["Hmm", "Nit: typo"])
        self.assertEqual(pr.review_decision(), APPROVED)
        self.assertEqual(pr.reviews[-1].body, "LGTM")

    def test_a_review_can_start_with_a_thread(self) -> None:
        pr = self.inboxes.prs["Stay a draft"]
        pr_id = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {pr.number}) {{ id }} }} }}"
        )["data"]["repository"]["pullRequest"]["id"]
        start = (
            "mutation($p: ID!, $t: [DraftPullRequestReviewThread]) {"
            " addPullRequestReview(input: {pullRequestId: $p, threads: $t})"
            " { pullRequestReview { id } } }"
        )
        # A bad thread starts no review.
        out = self.query(start, p=pr_id, t=[{"path": "inbox/x.md", "body": "?"}])
        self.assertIn("line is required", out["errors"][0]["message"])
        self.assertIsNone(pr.pending)

        thread = {"path": "inbox/x.md", "body": "Old", "line": 1, "side": "LEFT"}
        out = self.query(start, p=pr_id, t=[thread])
        self.assertNotIn("errors", out)
        assert pr.pending
        [c] = pr.pending.comments
        self.assertEqual((c.body, c.line, c.side), ("Old", 1, "LEFT"))
        self.assertNotIn(c, pr.comments)

        # The viewer sees their pending threads, with their side.
        threads = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {pr.number}) {{"
            " reviewThreads(first: 10) { nodes { id diffSide } }"
            " reviews(states: [PENDING], first: 1)"
            " { nodes { viewerDidAuthor comments { totalCount } } } } } }"
        )["data"]["repository"]["pullRequest"]
        self.assertIn(
            {"id": f"PRRT_{c.id}", "diffSide": "LEFT"},
            threads["reviewThreads"]["nodes"],
        )
        self.assertEqual(
            threads["reviews"]["nodes"],
            [{"viewerDidAuthor": True, "comments": {"totalCount": 1}}],
        )

    def test_merging(self) -> None:
        pr = self.inboxes.prs["Wait for reviewers"]
        pr_id = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {pr.number}) {{ id headRef {{ id }} }} }} }}"
        )["data"]["repository"]["pullRequest"]
        merge = (
            "mutation($p: ID!, $h: GitObjectID!) { mergePullRequest(input: {"
            " pullRequestId: $p, mergeMethod: SQUASH, expectedHeadOid: $h,"
            ' commitHeadline: "Title"}) { clientMutationId } }'
        )
        out = self.query(merge, p=pr_id["id"], h="0" * 40)
        self.assertIn("Head branch was modified", out["errors"][0]["message"])
        self.assertEqual(pr.state, "OPEN")

        out = self.query(merge, p=pr_id["id"], h=pr.head_oid)
        self.assertNotIn("errors", out)
        self.assertEqual(pr.state, "MERGED")
        self.assertEqual(pr.merged_with, ("SQUASH", "Title", None))

        out = self.query(
            "mutation($r: ID!) { deleteRef(input: {refId: $r}) { clientMutationId } }",
            r=pr_id["headRef"]["id"],
        )
        self.assertNotIn("errors", out)
        self.assertIn(pr.head, self.repo.deleted_branches)

    def test_a_deleted_base_keeps_its_last_commit(self) -> None:
        low = self.inboxes.prs["Wait for reviewers"]
        self.repo.checkout("on-top", low.head)
        self.repo.commit("On top", {"inbox/on-top.md": "x\n"})
        self.repo.push("on-top")
        top = self.repo.open_pr("On top", head="on-top", base=low.head)
        query = (
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {top.number}) {{"
            " baseRefOid baseBranch: baseRef { id } } } }"
        )
        data = self.query(query)["data"]["repository"]["pullRequest"]
        self.assertEqual(data["baseRefOid"], low.head_oid)
        self.assertIsNotNone(data["baseBranch"])

        self.repo.merge(top)
        self.repo.merge(low)
        out = self.query(
            "mutation($r: ID!) { deleteRef(input: {refId: $r}) { clientMutationId } }",
            r=data["baseBranch"]["id"],
        )
        self.assertNotIn("errors", out)
        data = self.query(query)["data"]["repository"]["pullRequest"]
        self.assertEqual(data, {"baseRefOid": low.head_oid, "baseBranch": None})
        # The branch is gone from fetches too.
        refs = self.gh.git("-C", str(self.repo.bare), "for-each-ref", "refs/heads/")
        self.assertNotIn(f"refs/heads/{low.head}", refs)

    def test_merging_needs_a_mergeable_pr(self) -> None:
        def merge(pr: PullRequest) -> dict:
            pr_id = self.query(
                'query { repository(owner: "test", name: "test") {'
                f" pullRequest(number: {pr.number}) {{ id }} }} }}"
            )["data"]["repository"]["pullRequest"]["id"]
            return self.query(
                "mutation($p: ID!) { mergePullRequest(input: {pullRequestId: $p})"
                " { clientMutationId } }",
                p=pr_id,
            )

        draft = self.inboxes.prs["Stay a draft"]
        self.assertIn("still a draft", merge(draft)["errors"][0]["message"])
        draft.can_merge_as_admin = True
        self.assertIn("still a draft", merge(draft)["errors"][0]["message"])

        # Blocked and out-of-date PRs only merge for those who may bypass
        # the rules.
        pr = self.inboxes.prs["Wait for reviewers"]
        for state in ("BLOCKED", "BEHIND"):
            pr.merge_state = state
            pr.can_merge_as_admin = False
            self.assertIn("not mergeable", merge(pr)["errors"][0]["message"])
        pr.can_merge_as_admin = True
        self.assertNotIn("errors", merge(pr))
        self.assertEqual(pr.state, "MERGED")

    def test_merge_queue(self) -> None:
        pr = self.inboxes.prs["Wait for reviewers"]
        repo = self.repo
        repo.merge_queue = "SQUASH"

        fields = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {pr.number}) {{ id isMergeQueueEnabled"
            " mergeQueue { configuration { mergeMethod } } mergeQueueEntry"
            " { position state jump } autoMergeRequest { enabledAt }"
            " viewerCanEnableAutoMerge viewerCanDisableAutoMerge } } }"
        )["data"]["repository"]["pullRequest"]
        self.assertTrue(fields["isMergeQueueEnabled"])
        self.assertEqual(
            fields["mergeQueue"], {"configuration": {"mergeMethod": "SQUASH"}}
        )
        self.assertIsNone(fields["mergeQueueEntry"])
        self.assertIsNone(fields["autoMergeRequest"])
        self.assertTrue(fields["viewerCanEnableAutoMerge"])
        self.assertTrue(fields["viewerCanDisableAutoMerge"])

        # A direct merge is refused on a queue branch, but not for admins.
        merge = (
            "mutation($p: ID!) { mergePullRequest(input: {pullRequestId: $p})"
            " { clientMutationId } }"
        )
        out = self.query(merge, p=fields["id"])
        self.assertIn(
            "Changes must be made through the merge queue",
            out["errors"][0]["message"],
        )
        self.assertEqual(pr.state, "OPEN")
        pr.can_merge_as_admin = True
        out = self.query(merge, p=fields["id"])
        self.assertNotIn("errors", out)
        self.assertEqual(pr.state, "MERGED")

    def test_enqueueing_and_dequeueing(self) -> None:
        pr = self.inboxes.prs["Wait for reviewers"]
        self.repo.merge_queue = "SQUASH"

        def pr_fields() -> dict:
            return self.query(
                'query { repository(owner: "test", name: "test") {'
                f" pullRequest(number: {pr.number}) {{ id isInMergeQueue"
                " mergeQueueEntry { position state jump } } } }"
            )["data"]["repository"]["pullRequest"]

        enqueue = (
            "mutation($p: ID!, $h: GitObjectID!, $j: Boolean) {"
            " enqueuePullRequest(input: {pullRequestId: $p,"
            " expectedHeadOid: $h, jump: $j}) { clientMutationId } }"
        )
        out = self.query(enqueue, p=pr_fields()["id"], h="0" * 40)
        self.assertIn("Head branch was modified", out["errors"][0]["message"])
        self.assertFalse(pr.queued)

        out = self.query(enqueue, p=pr_fields()["id"], h=pr.head_oid, j=False)
        self.assertNotIn("errors", out)
        self.assertTrue(pr.queued)
        entry = pr_fields()["mergeQueueEntry"]
        self.assertEqual(entry, {"position": 1, "state": "QUEUED", "jump": False})
        out = self.query(enqueue, p=pr_fields()["id"], h=pr.head_oid)
        self.assertIn("already in the merge queue", out["errors"][0]["message"])

        out = self.query(
            "mutation($p: ID!) { dequeuePullRequest(input: {id: $p})"
            " { clientMutationId } }",
            p=pr_fields()["id"],
        )
        self.assertNotIn("errors", out)
        self.assertFalse(pr.queued)
        self.assertIsNone(pr_fields()["mergeQueueEntry"])
        out = self.query(
            "mutation($p: ID!) { dequeuePullRequest(input: {id: $p})"
            " { clientMutationId } }",
            p=pr_fields()["id"],
        )
        self.assertIn("not in the merge queue", out["errors"][0]["message"])

    def test_auto_merge(self) -> None:
        pr = self.inboxes.prs["Wait for reviewers"]
        self.repo.merge_queue = "SQUASH"
        pr.merge_state = "BLOCKED"
        pr_id = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {pr.number}) {{ id"
            " autoMergeRequest { enabledAt enabledBy { login } } } } }"
        )["data"]["repository"]["pullRequest"]["id"]

        out = self.query(
            "mutation($p: ID!, $h: GitObjectID!) { enablePullRequestAutoMerge"
            "(input: {pullRequestId: $p, expectedHeadOid: $h})"
            " { clientMutationId } }",
            p=pr_id,
            h=pr.head_oid,
        )
        self.assertNotIn("errors", out)
        request = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {pr.number}) {{"
            " autoMergeRequest { enabledAt enabledBy { login } } } } }"
        )["data"]["repository"]["pullRequest"]["autoMergeRequest"]
        self.assertIsNotNone(request["enabledAt"])
        self.assertEqual(request["enabledBy"]["login"], "me")

        out = self.query(
            "mutation($p: ID!) { disablePullRequestAutoMerge"
            "(input: {pullRequestId: $p}) { clientMutationId } }",
            p=pr_id,
        )
        self.assertNotIn("errors", out)
        self.assertIsNone(pr.auto_merge_at)

        pr.can_auto_merge = False
        out = self.query(
            "mutation($p: ID!) { enablePullRequestAutoMerge"
            "(input: {pullRequestId: $p}) { clientMutationId } }",
            p=pr_id,
        )
        self.assertIn("not allowed", out["errors"][0]["message"])

    def test_retargeting_and_closing_a_pr(self) -> None:
        pr = self.inboxes.prs["Wait for reviewers"]
        pr_id = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {pr.number}) {{ id baseRefName headRefOid }} }} }}"
        )["data"]["repository"]["pullRequest"]
        self.assertEqual(pr_id["baseRefName"], pr.base)
        self.assertEqual(pr_id["headRefOid"], pr.head_oid)

        out = self.query(
            "mutation($p: ID!, $b: String!) { updatePullRequest(input: {"
            " pullRequestId: $p, baseRefName: $b}) { clientMutationId } }",
            p=pr_id["id"],
            b="nope",
        )
        self.assertIn("Could not resolve to a Ref", out["errors"][0]["message"])
        out = self.query(
            "mutation($p: ID!, $b: String!) { updatePullRequest(input: {"
            " pullRequestId: $p, baseRefName: $b}) { clientMutationId } }",
            p=pr_id["id"],
            b="main",
        )
        self.assertNotIn("errors", out)
        self.assertEqual(pr.base, "main")

        # The title and body are set through the same mutation.
        out = self.query(
            "mutation($p: ID!, $t: String!, $b: String!) {"
            " updatePullRequest(input: {"
            " pullRequestId: $p, title: $t, body: $b}) { clientMutationId } }",
            p=pr_id["id"],
            t="Renamed",
            b="New body",
        )
        self.assertNotIn("errors", out)
        self.assertEqual(pr.title, "Renamed")
        self.assertEqual(pr.body, "New body")

        out = self.query(
            "mutation($p: ID!) { closePullRequest(input: {pullRequestId: $p})"
            " { clientMutationId } }",
            p=pr_id["id"],
        )
        self.assertNotIn("errors", out)
        self.assertEqual(pr.state, "CLOSED")

        # A closed PR's title and body stay editable.
        out = self.query(
            "mutation($p: ID!, $t: String!) { updatePullRequest(input: {"
            " pullRequestId: $p, title: $t}) { clientMutationId } }",
            p=pr_id["id"],
            t="Renamed again",
        )
        self.assertNotIn("errors", out)
        self.assertEqual(pr.title, "Renamed again")

        # A closed PR can't be retargeted or closed again.
        for mutation in (
            (
                "mutation($p: ID!) { updatePullRequest(input: {"
                ' pullRequestId: $p, baseRefName: "main"}) { clientMutationId } }'
            ),
            (
                "mutation($p: ID!) { closePullRequest(input: {pullRequestId: $p})"
                " { clientMutationId } }"
            ),
        ):
            out = self.query(mutation, p=pr_id["id"])
            self.assertIn("closed", out["errors"][0]["message"])

    def stacks_rest(
        self, method: str, path: str, body=None, version="2026-03-10"
    ) -> tuple[int, object]:
        import urllib.request

        req = urllib.request.Request(
            self.api + path,
            method=method,
            data=json.dumps(body).encode() if body is not None else None,
            headers={
                "authorization": f"Bearer {TOKEN}",
                "content-type": "application/json",
                "x-github-api-version": version,
            },
        )
        try:
            with urllib.request.urlopen(req) as resp:
                return resp.status, json.load(resp)
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read() or b"{}")

    def test_stacks(self) -> None:
        api = getattr(self, "api", None)
        if api is None:
            self.api = self.gh.start()
            self.addCleanup(self.gh.stop)
        repo = self.repo
        a, b = (
            self.inboxes.prs["Wait for reviewers"],
            self.inboxes.prs["Get changes requested"],
        )
        # Chain b onto a so the stack's bases line up.
        b.base = a.head
        c = self.inboxes.prs["Get merged"]

        # The wrong API version and a disabled repo see no endpoint.
        status, _ = self.stacks_rest(
            "GET",
            f"/repos/test/test/stacks?pull_request={a.number}",
            version="2022-11-28",
        )
        self.assertEqual(status, 404)
        repo.stacks_enabled = False
        status, _ = self.stacks_rest(
            "GET", f"/repos/test/test/stacks?pull_request={a.number}"
        )
        self.assertEqual(status, 404)
        repo.stacks_enabled = True

        # One PR doesn't make a stack; bases must chain.
        status, _ = self.stacks_rest(
            "POST", "/repos/test/test/stacks", {"pull_requests": [a.number]}
        )
        self.assertEqual(status, 422)
        status, _ = self.stacks_rest(
            "POST", "/repos/test/test/stacks", {"pull_requests": [a.number, c.number]}
        )
        self.assertEqual(status, 422)
        status, out = self.stacks_rest(
            "POST", "/repos/test/test/stacks", {"pull_requests": [a.number, b.number]}
        )
        self.assertEqual(status, 201)
        number = out["number"]
        self.assertEqual(repo.stacks[number], [a.number, b.number])

        status, out = self.stacks_rest(
            "GET", f"/repos/test/test/stacks?pull_request={b.number}"
        )
        self.assertEqual([s["number"] for s in out], [number])
        self.assertEqual(
            [p["number"] for p in out[0]["pull_requests"]], [a.number, b.number]
        )

        # A stacked PR can't be retargeted or closed through GraphQL.
        pr_id = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {b.number}) {{ id }} }} }}"
        )["data"]["repository"]["pullRequest"]["id"]
        out = self.query(
            "mutation($p: ID!) { updatePullRequest(input: {"
            ' pullRequestId: $p, baseRefName: "main"}) { clientMutationId } }',
            p=pr_id,
        )
        self.assertIn("in a stack", out["errors"][0]["message"])
        out = self.query(
            "mutation($p: ID!) { closePullRequest(input: {pullRequestId: $p})"
            " { clientMutationId } }",
            p=pr_id,
        )
        self.assertIn("in a stack", out["errors"][0]["message"])

        # Add needs the new PR to chain onto the top.
        other = self.inboxes.prs["Ask me for a review"]
        other.base = "nope"
        status, _ = self.stacks_rest(
            "POST",
            f"/repos/test/test/stacks/{number}/add",
            {"pull_requests": [other.number]},
        )
        self.assertEqual(status, 409)
        other.base = b.head
        status, out = self.stacks_rest(
            "POST",
            f"/repos/test/test/stacks/{number}/add",
            {"pull_requests": [other.number]},
        )
        self.assertEqual(status, 200)
        self.assertEqual(repo.stacks[number], [a.number, b.number, other.number])

        # Merged PRs keep the stack alive through an unstack.
        repo.merge(a)
        status, out = self.stacks_rest(
            "POST", f"/repos/test/test/stacks/{number}/unstack", {}
        )
        self.assertEqual(status, 200)
        self.assertEqual(repo.stacks[number], [a.number])
        status, out = self.stacks_rest(
            "GET", f"/repos/test/test/stacks?pull_request={a.number}"
        )
        merged = out[0]["pull_requests"][0]
        self.assertEqual(
            (merged["state"], merged["merged_at"] is None), ("closed", False)
        )
        status, _ = self.stacks_rest(
            "POST", f"/repos/test/test/stacks/{number}/unstack", {}
        )
        self.assertEqual(status, 422)
        repo.merge(repo.prs[a.number])
        del repo.stacks[number]

    def test_deleting_a_branch_can_fail(self) -> None:
        pr = self.inboxes.prs["Wait for reviewers"]
        self.repo.undeletable_branches.add(pr.head)
        ref = self.query(
            'query { repository(owner: "test", name: "test") {'
            f" pullRequest(number: {pr.number}) {{ headRef {{ id }} }} }} }}"
        )["data"]["repository"]["pullRequest"]["headRef"]["id"]
        out = self.query(
            "mutation($r: ID!) { deleteRef(input: {refId: $r}) { clientMutationId } }",
            r=ref,
        )
        self.assertIn("Cannot delete", out["errors"][0]["message"])
        self.assertNotIn(pr.head, self.repo.deleted_branches)

    def test_search_reports_facts_and_pages(self) -> None:
        out = self.query(SEARCH_QUERY, q0="is:pr", q1="is:pr sort:created-asc")
        self.assertEqual(out["data"]["viewer"], {"login": "me"})
        s0 = out["data"]["s0"]
        # issueCount is the total, not the page's size.
        total = len(self.gh.prs())
        self.assertEqual(s0["issueCount"], total)
        self.assertFalse(s0["pageInfo"]["hasNextPage"])

        pr = self.inboxes.prs["Ask me for a review"]
        node = next(n for n in s0["nodes"] if n["number"] == pr.number)
        self.assertFalse(node["isInMergeQueue"])
        self.assertEqual(node["reviewDecision"], "REVIEW_REQUIRED")
        self.assertEqual(
            node["reviewRequests"],
            {"nodes": [{"requestedReviewer": {"__typename": "User", "login": "me"}}]},
        )
        self.assertEqual(node["repository"]["isArchived"], False)

        queued = self.inboxes.prs["Sit in the merge queue"]
        node = next(n for n in s0["nodes"] if n["number"] == queued.number)
        self.assertTrue(node["isInMergeQueue"])
        self.assertEqual(node["reviewDecision"], "APPROVED")
        self.assertEqual(
            node["reviews"],
            {
                "nodes": [
                    {
                        "author": {"__typename": "User", "login": "other"},
                        "state": "APPROVED",
                    }
                ]
            },
        )

        team = self.inboxes.prs["Ask my team for a review"]
        node = next(n for n in s0["nodes"] if n["number"] == team.number)
        self.assertEqual(
            node["reviewRequests"]["nodes"][0]["requestedReviewer"]["__typename"],
            "Team",
        )

        # A small page size paginates with cursors.
        out = self.query(
            "query($q: String!, $a: String) {"
            " s: search(type: ISSUE_ADVANCED, query: $q, first: 3, after: $a) {"
            "   issueCount pageInfo { hasNextPage endCursor } nodes { ... on PullRequest { number } } } }",
            q="is:pr sort:created-asc",
        )
        page = out["data"]["s"]
        seen = [n["number"] for n in page["nodes"]]
        while page["pageInfo"]["hasNextPage"]:
            out = self.query(
                "query($q: String!, $a: String) {"
                " s: search(type: ISSUE_ADVANCED, query: $q, first: 3, after: $a) {"
                "   issueCount pageInfo { hasNextPage endCursor } nodes { ... on PullRequest { number } } } }",
                q="is:pr sort:created-asc",
                a=page["pageInfo"]["endCursor"],
            )
            page = out["data"]["s"]
            seen += [n["number"] for n in page["nodes"]]
        self.assertEqual(len(seen), page["issueCount"])
        self.assertEqual(len(seen), len(set(seen)))

    def test_gr_qualifiers_never_reach_the_fake(self) -> None:
        pr = self.inboxes.prs["Get changes requested"]
        with self.assertRaises(SearchError):
            matches(pr, "gr:review:active")
        with self.assertRaises(SearchError):
            matches(pr, "-gr:review:active")

    def test_dismissed_and_unmerged(self) -> None:
        pr = self.inboxes.prs["Get changes requested"]
        self.assertTrue(matches(pr, "review:changes_requested"))
        pr.reviews.append(Review("other", "DISMISSED"))
        self.assertFalse(matches(pr, "review:changes_requested"))
        # The change request still happened, so the PR isn't review:none.
        self.assertFalse(matches(pr, "review:none"))
        self.assertTrue(matches(pr, "is:unmerged"))
        self.assertFalse(matches(self.inboxes.prs["Get merged"], "is:unmerged"))

    def test_unknown_fields_are_errors(self) -> None:
        out = self.query('query { repository(owner: "test", name: "test") { nope } }')
        self.assertIn("Cannot query field 'nope'", out["errors"][0]["message"])

    def test_threads_users_and_commit_authors(self) -> None:
        pr = self.inboxes.prs["Get changes requested"]
        root = self.repo.comment(
            pr, author="other", body="Hmm", path="inbox/x.md", line=1
        )
        self.repo.comment(
            pr, author="me", body="Fixed", path="inbox/x.md", line=1, reply_to=root
        )
        root.resolved = True
        root.resolved_by = "other"
        out = self.query(THREADS_QUERY, owner="test", name="test", number=pr.number)
        threads = out["data"]["repository"]["pullRequest"]["reviewThreads"]
        self.assertEqual(
            threads["nodes"],
            [
                {
                    "id": f"PRRT_{root.id}",
                    "diffSide": "RIGHT",
                    "isResolved": True,
                    "viewerCanResolve": False,
                    "viewerCanUnresolve": True,
                    "resolvedBy": {
                        "login": "other",
                        "name": "Other Person",
                        "avatarUrl": None,
                    },
                    "comments": {"nodes": [{"databaseId": root.id}]},
                },
            ],
        )

        out = self.query(
            'query { u0: user(login: "other") { login name avatarUrl } '
            'u1: user(login: "nobody") { login } }'
        )
        self.assertEqual(
            out["data"]["u0"],
            {"login": "other", "name": "Other Person", "avatarUrl": None},
        )
        self.assertIsNone(out["data"]["u1"])

        sha = self.inboxes.prs["Ask me for a review"].head_oid
        out = self.query(
            'query($o0: GitObjectID!) { repository(owner: "test", name: "test") {'
            " c0: object(oid: $o0) { ... on Commit { author { name user { login } } } } } }",
            o0=sha,
        )
        self.assertEqual(
            out["data"]["repository"]["c0"],
            {"author": {"name": "Other Person", "user": {"login": "other"}}},
        )

    def test_rest_endpoints(self) -> None:
        base = self.gh.start()
        self.addCleanup(self.gh.stop)

        def get(path: str, token: str = TOKEN) -> object:
            req = urllib.request.Request(
                base + path, headers={"Authorization": f"Bearer {token}"}
            )
            with urllib.request.urlopen(req) as resp:
                return json.load(resp)

        with self.assertRaises(urllib.error.HTTPError) as e:
            get("/repos/test/test/activity?ref=refs/heads/main", token="wrong")
        self.assertEqual(e.exception.code, 401)

        pr = self.inboxes.prs[scenarios.STACK_TOP]
        activity = get(
            f"/repos/test/test/activity?ref=refs/heads/{pr.head}&direction=asc"
        )
        self.assertEqual([a["activity_type"] for a in activity], ["branch_creation"])
        self.assertEqual(activity[0]["after"], pr.head_oid)

        self.repo.comment(pr, author="other", body="Nit", path="stack/top.txt", line=1)
        [comment] = get(f"/repos/test/test/pulls/{pr.number}/comments?per_page=100")
        self.assertEqual(comment["body"], "Nit")
        self.assertEqual(comment["original_commit_id"], pr.head_oid)

        def post(path: str, body: object) -> object:
            req = urllib.request.Request(
                base + path,
                data=json.dumps(body).encode(),
                headers={
                    "Authorization": f"Bearer {TOKEN}",
                    "Content-Type": "application/json",
                },
            )
            with urllib.request.urlopen(req) as resp:
                return json.load(resp)

        # A PR from a branch that was pushed straight to the bare repo, the
        # way the server's wrap-up pushes.
        new = {"head": "agent", "base": "main", "title": "Agent work", "body": "Did it"}
        with self.assertRaises(urllib.error.HTTPError) as e:
            post("/repos/test/test/pulls", new)
        self.assertEqual(e.exception.code, 422)
        self.repo.checkout("agent", "main")
        sha = self.repo.commit("Agent work", {"agent.txt": "1\n"})
        self.repo.git("push", "--quiet", "origin", "agent:refs/heads/agent")
        created = post("/repos/test/test/pulls", new)
        opened = self.repo.prs[created["number"]]
        self.assertEqual(
            (opened.title, opened.base, opened.head_oid), ("Agent work", "main", sha)
        )
        self.assertEqual(created["html_url"], opened.url)

    def test_reviewer_requests(self) -> None:
        base = self.gh.start()
        self.addCleanup(self.gh.stop)
        pr = self.inboxes.prs["Wait for reviewers"]
        self.repo.teams[TEAM_SLUG] = {"other"}
        url = f"{base}/repos/test/test/pulls/{pr.number}/requested_reviewers"

        def call(method: str, body: object) -> object:
            req = urllib.request.Request(
                url,
                data=json.dumps(body).encode(),
                method=method,
                headers={
                    "Authorization": f"Bearer {TOKEN}",
                    "Content-Type": "application/json",
                },
            )
            with urllib.request.urlopen(req) as resp:
                return json.load(resp)

        call("POST", {"reviewers": ["other", "me"], "team_reviewers": [TEAM_SLUG]})
        self.assertEqual(pr.review_requests, {"other", "me"})
        self.assertEqual(pr.team_review_requests, {"other"})
        call("DELETE", {"reviewers": ["other"], "team_reviewers": [TEAM_SLUG]})
        self.assertEqual(pr.review_requests, {"me"})
        self.assertEqual(pr.team_review_requests, set())

        def fail(method: str, body: object) -> int:
            req = urllib.request.Request(
                url,
                data=json.dumps(body).encode(),
                method=method,
                headers={
                    "Authorization": f"Bearer {TOKEN}",
                    "Content-Type": "application/json",
                },
            )
            with self.assertRaises(urllib.error.HTTPError) as e:
                urllib.request.urlopen(req)
            return e.exception.code

        self.assertEqual(
            fail("POST", {"reviewers": ["nobody"], "team_reviewers": []}), 422
        )
        self.assertEqual(fail("POST", {"reviewers": [], "team_reviewers": ["x"]}), 422)
        self.gh.fail_next_reviewers = "nope"
        self.assertEqual(fail("POST", {"reviewers": [], "team_reviewers": []}), 422)
        self.assertEqual(pr.review_requests, {"me"})

    def test_reviewer_candidate_fields(self) -> None:
        pr = self.inboxes.prs["Wait for reviewers"]
        self.repo.collaborators = {"me", "other", "third"}
        self.repo.suggested_reviewers = {"other"}
        self.repo.teams[TEAM_SLUG] = {"other"}
        data = self.query(
            """
            query($owner: String!, $name: String!, $number: Int!, $q: String!) {
              repository(owner: $owner, name: $name) {
                pullRequest(number: $number) {
                  author { login }
                  viewerCanUpdate
                  suggestedReviewers { nodes { reviewer { login } } }
                }
                assignableUsers(query: $q, first: 20) { nodes { login } }
              }
              organization(login: $owner) {
                teams(query: $q, first: 20) { nodes { name slug } }
              }
            }
            """,
            owner="test",
            name="test",
            number=pr.number,
            q="",
        )["data"]
        repo = data["repository"]
        self.assertTrue(repo["pullRequest"]["viewerCanUpdate"])
        self.assertEqual(
            repo["pullRequest"]["suggestedReviewers"]["nodes"],
            [{"reviewer": {"login": "other"}}],
        )
        self.assertEqual(
            repo["assignableUsers"]["nodes"],
            [{"login": "me"}, {"login": "other"}, {"login": "third"}],
        )
        self.assertEqual(
            data["organization"]["teams"]["nodes"],
            [{"name": TEAM, "slug": TEAM_SLUG}],
        )
        pr.viewer_can_update = False
        self.assertFalse(
            self.query(
                """
                query($owner: String!, $name: String!, $number: Int!) {
                  repository(owner: $owner, name: $name) {
                    pullRequest(number: $number) { viewerCanUpdate }
                  }
                }
                """,
                owner="test",
                name="test",
                number=pr.number,
            )["data"]["repository"]["pullRequest"]["viewerCanUpdate"]
        )
        # An owner with no teams is no organization, like a user-owned repo.
        self.repo.teams.clear()
        out = self.query('query { organization(login: "test") { login } }')
        self.assertEqual(len(out["errors"]), 1)
        self.assertIn("Organization", out["errors"][0]["message"])

    def test_rate_limit(self) -> None:
        out = self.query("query { rateLimit { cost limit remaining } }")
        self.assertEqual(
            out["data"]["rateLimit"],
            {"cost": 1, "limit": 5000, "remaining": 5000},
        )

        base = self.gh.start()
        self.addCleanup(self.gh.stop)

        def post_graphql() -> urllib.request.addinfourl:
            req = urllib.request.Request(
                base + "/graphql",
                data=json.dumps({"query": "query { viewer { login } }"}).encode(),
                headers={
                    "Authorization": f"Bearer {TOKEN}",
                    "Content-Type": "application/json",
                },
            )
            return urllib.request.urlopen(req)

        resp = post_graphql()
        self.assertEqual(resp.headers["x-ratelimit-limit"], "5000")
        self.assertEqual(resp.headers["x-ratelimit-remaining"], "5000")

        # An empty budget answers GraphQL with a RATE_LIMITED error and
        # REST with a 403, like GitHub does.
        self.gh.set_rate_limit(0)
        out = json.load(post_graphql())
        self.assertEqual(out["errors"][0]["type"], "RATE_LIMITED")
        req = urllib.request.Request(
            base + "/repos/test/test/activity?ref=refs/heads/main",
            headers={"Authorization": f"Bearer {TOKEN}"},
        )
        with self.assertRaises(urllib.error.HTTPError) as e:
            urllib.request.urlopen(req)
        self.assertEqual(e.exception.code, 403)

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
        subprocess.run(
            [
                "git",
                "-C",
                str(clone),
                "fetch",
                "--quiet",
                "https://github.com/test/test.git",
                old,
            ],
            env=env,
            check=True,
        )


if __name__ == "__main__":
    unittest.main()
