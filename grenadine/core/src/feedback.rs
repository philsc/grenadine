//! Which of a PR's comments are still open: the ones a fixing run
//! addresses, and the PR's page counts.
//!
//! GitHub only marks inline threads as resolved, so the rest goes by
//! time. A fixing run answers the reviews' summaries and the conversation
//! in one comment that starts with `MARK`; what was posted before the
//! latest such comment has been dealt with.

use crate::api::{IssueComment, PrDetail, Review, ReviewComment, ReviewState};

/// The line that starts the comment in which a fixing run answers a PR's
/// summaries and conversation. Markdown doesn't render it.
pub const MARK: &str = "<!-- grenadine:addressed -->";

/// An inline thread: its first comment and the replies, oldest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread<'a> {
    pub root: &'a ReviewComment,
    pub replies: Vec<&'a ReviewComment>,
}

/// The open feedback on a PR, each kind oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Feedback<'a> {
    pub threads: Vec<Thread<'a>>,
    /// Reviews with a summary.
    pub summaries: Vec<&'a Review>,
    /// Comments on the PR itself.
    pub comments: Vec<&'a IssueComment>,
}

impl Feedback<'_> {
    pub fn len(&self) -> usize {
        self.threads.len() + self.summaries.len() + self.comments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The open feedback on `detail`'s PR; none before its reviews synced.
///
/// A thread is open while it's unresolved and its last comment isn't the
/// viewer's. One that the viewer started, which includes an auto-review's,
/// is also open until it has a reply. Summaries and conversation comments
/// are open when they came after the latest comment with `MARK`.
pub fn open(detail: &PrDetail) -> Feedback<'_> {
    match &detail.review {
        Some(review) => open_in(
            &review.viewer,
            &detail.comments,
            &review.reviews,
            &review.comments,
        ),
        None => Feedback::default(),
    }
}

/// `open` of a PR with these inline comments, `reviews` and
/// `conversation`, seen by `viewer`.
pub fn open_in<'a>(
    viewer: &str,
    inline: &'a [ReviewComment],
    reviews: &'a [Review],
    conversation: &'a [IssueComment],
) -> Feedback<'a> {
    let mut threads: Vec<Thread> = Vec::new();
    for c in inline.iter().filter(|c| !c.pending) {
        match c.in_reply_to {
            None => threads.push(Thread {
                root: c,
                replies: Vec::new(),
            }),
            Some(root) => {
                if let Some(t) = threads.iter_mut().find(|t| t.root.id == root) {
                    t.replies.push(c);
                }
            }
        }
    }
    threads.retain(|t| {
        let last = t.replies.last().copied().unwrap_or(t.root);
        !t.root.resolved && (last.author != viewer || t.replies.is_empty())
    });
    // GitHub's timestamps are all UTC in one format, so they sort as text.
    let since = conversation
        .iter()
        .filter(|c| c.author.login.as_deref() == Some(viewer) && c.body.contains(MARK))
        .map(|c| c.created_at.as_str())
        .max()
        .unwrap_or_default();
    let summaries = reviews
        .iter()
        .filter(|r| {
            !r.body.trim().is_empty()
                && r.state != ReviewState::Dismissed
                && r.submitted_at.as_str() > since
        })
        .collect();
    let comments = conversation
        .iter()
        .filter(|c| !c.body.contains(MARK) && c.created_at.as_str() > since)
        .collect();
    Feedback {
        threads,
        summaries,
        comments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Person, Side};

    fn person(login: &str) -> Person {
        Person {
            login: Some(login.into()),
            ..Person::default()
        }
    }

    fn inline(id: u64, in_reply_to: Option<u64>, author: &str) -> ReviewComment {
        ReviewComment {
            id,
            in_reply_to,
            author: author.into(),
            body: format!("comment {id}"),
            path: "a.rs".into(),
            original_commit: "c".into(),
            original_line: Some(1),
            original_start_line: None,
            line: Some(1),
            start_line: None,
            start_side: None,
            side: Side::Right,
            on_file: false,
            created_at: "2026-01-01T00:00:00Z".into(),
            url: String::new(),
            resolved: false,
            resolved_by: None,
            thread_id: None,
            can_resolve: true,
            pending: false,
            node_id: None,
        }
    }

    fn summary(author: &str, body: &str, at: &str) -> Review {
        Review {
            author: person(author),
            state: ReviewState::Commented,
            body: body.into(),
            submitted_at: at.into(),
            url: String::new(),
            commit: None,
        }
    }

    fn said(author: &str, body: &str, at: &str) -> IssueComment {
        IssueComment {
            author: person(author),
            body: body.into(),
            created_at: at.into(),
            url: String::new(),
        }
    }

    fn roots(f: &Feedback) -> Vec<u64> {
        f.threads.iter().map(|t| t.root.id).collect()
    }

    #[test]
    fn a_thread_is_open_until_the_viewer_answers_or_its_resolved() {
        let mut resolved = inline(5, None, "ann");
        resolved.resolved = true;
        let mut pending = inline(9, None, "me");
        pending.pending = true;
        let comments = vec![
            // Someone else's, unanswered.
            inline(1, None, "ann"),
            // Someone else's, answered by the viewer.
            inline(2, None, "ann"),
            inline(3, Some(2), "me"),
            // Answered, and then they wrote again.
            inline(4, None, "ann"),
            inline(6, Some(4), "me"),
            inline(7, Some(4), "ann"),
            resolved,
            // The viewer's own, e.g. an auto-review's: open until it
            // has a reply.
            inline(8, None, "me"),
            pending,
            inline(10, None, "me"),
            inline(11, Some(10), "me"),
            inline(12, None, "me"),
            inline(13, Some(12), "ann"),
        ];
        let open = open_in("me", &comments, &[], &[]);
        assert_eq!(roots(&open), [1, 4, 8, 12]);
        assert_eq!(open.threads[1].replies.len(), 2);
        assert_eq!(open.len(), 4);
    }

    #[test]
    fn summaries_and_the_conversation_are_open_after_the_latest_mark() {
        let reviews = vec![
            summary("ann", "Old points.", "2026-01-01T00:00:00Z"),
            // A reply in a thread makes a review without a summary.
            summary("me", " ", "2026-01-03T00:00:00Z"),
            summary("ann", "New points.", "2026-01-03T00:00:00Z"),
        ];
        let conversation = vec![
            said("ann", "Before.", "2026-01-01T00:00:00Z"),
            said("me", &format!("{MARK}\nDone."), "2026-01-02T00:00:00Z"),
            said("ann", "After.", "2026-01-03T00:00:00Z"),
            // Someone else quoting the mark doesn't move the line.
            said("bob", &format!("> {MARK}"), "2026-01-04T00:00:00Z"),
            said("ann", "Later.", "2026-01-05T00:00:00Z"),
        ];
        let open_now = open_in("me", &[], &reviews, &conversation);
        let bodies: Vec<_> = open_now.summaries.iter().map(|r| &r.body).collect();
        assert_eq!(bodies, ["New points."]);
        let bodies: Vec<_> = open_now.comments.iter().map(|c| &c.body).collect();
        assert_eq!(bodies, ["After.", "Later."]);

        // Without a mark, everything with a body is open.
        let unmarked: Vec<_> = conversation
            .into_iter()
            .filter(|c| !c.body.contains(MARK))
            .collect();
        let open_now = open_in("me", &[], &reviews, &unmarked);
        assert_eq!((open_now.summaries.len(), open_now.comments.len()), (2, 3));
    }
}
