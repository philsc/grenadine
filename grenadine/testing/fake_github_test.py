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
    APPROVED,
    REVIEW_REQUIRED,
    TEAM,
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
      reviewDecision mergeable mergeStateStatus viewerCanMergeAsAdmin
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
            ... on User { login }
            ... on Team { name }
            ... on Bot { login }
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
            statusCheckRollup {
              state
              contexts(first: 100) {
                nodes {
                  ... on CheckRun { name status conclusion detailsUrl }
                  ... on StatusContext { context state targetUrl }
                }
              }
            }
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
      reviews(first: 100) { nodes { author { login } state } }
      reviewRequests(first: 100) { nodes { requestedReviewer { __typename ... on User { login } } } } } }
  }
  s1: search(type: ISSUE_ADVANCED, query: $q1, first: 100) {
    issueCount pageInfo { hasNextPage endCursor }
    nodes { ... on PullRequest { number title author { login } state isDraft url updatedAt headRefOid
      reviewThreads(first: 100) { totalCount nodes { isResolved } }
      commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      isInMergeQueue reviewDecision repository { nameWithOwner isArchived }
      reviews(first: 100) { nodes { author { login } state } }
      reviewRequests(first: 100) { nodes { requestedReviewer { __typename ... on User { login } } } } } }
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
        self.assertEqual(len(inboxes), 14)
        for name, filter, _ in inboxes:
            with self.subTest(inbox=name):
                found = search_prs(
                    self.gh.prs(), scenarios.server_query(filter, [scenarios.REPO])
                )
                # Local inboxes then keep the hits their filter matches.
                if "gr:" in filter:
                    found = [pr for pr in found if scenarios.matches(pr, filter)]
                    if "gr:sort:title-asc" in filter:
                        found.sort(key=lambda pr: pr.title.lower())
                got = [pr.title for pr in found]
                self.assertEqual(got, self.inboxes.titles_in(name))
        # Every expected inbox exists.
        names = {name for name, _, _ in inboxes}
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
        pr.checks = "FAILURE"
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
        pr.checks = "FAILURE"
        pr.check_runs = [
            CheckRun("lint"),
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
                {"requestedReviewer": {"login": "third"}},
                {"requestedReviewer": {"name": TEAM}},
            ],
        )
        [comment] = data["comments"]["nodes"]
        self.assertEqual(comment["body"], "Thanks!")
        rollup = data["head"]["nodes"][0]["commit"]["statusCheckRollup"]
        self.assertEqual(rollup["state"], "FAILURE")
        self.assertEqual(
            [
                (c["name"], c["status"], c["conclusion"])
                for c in rollup["contexts"]["nodes"]
            ],
            [
                ("lint", "COMPLETED", "SUCCESS"),
                ("test", "COMPLETED", "FAILURE"),
                ("deploy", "IN_PROGRESS", None),
            ],
        )

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
            {"nodes": [{"author": {"login": "other"}, "state": "APPROVED"}]},
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
