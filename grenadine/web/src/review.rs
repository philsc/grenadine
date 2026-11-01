//! A PR's reviews, its conversation and whether it can be merged.

use grenadine_core::api::{
    Check, CiStatus, IssueComment, MergeState, Review, ReviewDecision, ReviewInfo, ReviewState,
};
use leptos::prelude::*;

use crate::inboxes::CiIcon;
use crate::pr::{CommentFrame, PersonChip};

/// Each reviewer's verdict: their latest approval, change request or
/// dismissal, or their latest comment when they gave no verdict. The PR's
/// author is left out, since replying to a thread makes a comment review.
/// Ordered by when each reviewer first reviewed.
pub fn latest_reviews<'a>(reviews: &'a [Review], author: &str) -> Vec<&'a Review> {
    let mut out: Vec<&Review> = Vec::new();
    for r in reviews {
        if r.author.login.as_deref() == Some(author) {
            continue;
        }
        match out.iter_mut().find(|o| o.author.login == r.author.login) {
            Some(o) => {
                if r.state != ReviewState::Commented || o.state == ReviewState::Commented {
                    *o = r;
                }
            }
            None => out.push(r),
        }
    }
    out
}

/// One line about whether the PR can be merged: a CSS class, `ok`,
/// `warn` or `bad`, and the text.
pub fn merge_status(info: &ReviewInfo) -> (&'static str, String) {
    if info.merge.conflicting == Some(true) {
        return (
            "bad",
            "This branch has conflicts that must be resolved.".into(),
        );
    }
    match info.merge.state {
        MergeState::Clean | MergeState::HasHooks => ("ok", "Ready to merge.".into()),
        MergeState::Unstable => ("warn", "Can be merged, but not every check passed.".into()),
        MergeState::Behind => ("warn", "This branch is out of date with its base.".into()),
        MergeState::Draft => ("warn", "This PR is a draft.".into()),
        MergeState::Dirty => (
            "bad",
            "This branch has conflicts that must be resolved.".into(),
        ),
        MergeState::Blocked => {
            let why = match info.decision {
                Some(ReviewDecision::ReviewRequired) => "it needs an approving review",
                Some(ReviewDecision::ChangesRequested) => "changes were requested",
                _ => "required checks or rules aren't satisfied",
            };
            ("bad", format!("Merging is blocked: {why}."))
        }
        MergeState::Unknown => (
            "warn",
            "GitHub is still working out whether this can be merged.".into(),
        ),
    }
}

/// How many checks failed, are running and passed, e.g. "1 failing, 2
/// passing".
pub fn check_counts(checks: &[Check]) -> String {
    let count = |s: Option<CiStatus>| checks.iter().filter(|c| c.status == s).count();
    [
        (count(Some(CiStatus::Failing)), "failing"),
        (count(Some(CiStatus::Running)), "running"),
        (count(Some(CiStatus::Passing)), "passing"),
        (count(None), "skipped"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect::<Vec<_>>()
    .join(", ")
}

fn verb(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "requested changes",
        ReviewState::Commented => "reviewed",
        ReviewState::Dismissed => "reviewed (dismissed)",
    }
}

fn state_class(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "review-state approved",
        ReviewState::ChangesRequested => "review-state changes",
        ReviewState::Commented | ReviewState::Dismissed => "review-state commented",
    }
}

/// The review decision and each reviewer's verdict, for the PR header.
pub fn review_summary(info: &ReviewInfo, author: &str) -> impl IntoView + use<> {
    // A decision grenadine doesn't know says nothing useful.
    let decision = info
        .decision
        .filter(|d| *d != ReviewDecision::Unknown)
        .map(|d| view! { <span class=format!("review-decision {}", d.class())>{d.label()}</span> });
    let reviewers = latest_reviews(&info.reviews, author)
        .into_iter()
        .map(|r| {
            view! {
                <span class=state_class(r.state) title=verb(r.state)>
                    <PersonChip person=r.author.clone() guess=false />
                </span>
            }
        })
        .collect_view();
    let requested = (!info.requested.is_empty())
        .then(|| view! { <span class="muted">{format!("Waiting on {}", info.requested.join(", "))}</span> });
    view! {
        <p class="review-summary">
            {decision}
            {reviewers}
            {requested}
        </p>
    }
}

/// One entry of the conversation.
enum Entry<'a> {
    Comment(&'a IssueComment),
    Review(&'a Review),
}

impl Entry<'_> {
    fn at(&self) -> &str {
        match self {
            Entry::Comment(c) => &c.created_at,
            Entry::Review(r) => &r.submitted_at,
        }
    }
}

/// The comments on the PR and the reviews, oldest first. Comment-only
/// reviews without a summary are left out: their comments are in the
/// diff.
fn entries(info: &ReviewInfo) -> Vec<Entry<'_>> {
    let mut out: Vec<Entry> = info.comments.iter().map(Entry::Comment).collect();
    out.extend(
        info.reviews
            .iter()
            .filter(|r| r.state != ReviewState::Commented || !r.body.trim().is_empty())
            .map(Entry::Review),
    );
    // RFC 3339 times in UTC sort as strings.
    out.sort_by(|a, b| a.at().cmp(b.at()));
    out
}

/// The comments on the PR itself and the reviews' summaries.
pub fn conversation(info: &ReviewInfo) -> AnyView {
    let items = entries(info)
        .into_iter()
        .map(|e| match e {
            Entry::Comment(c) => {
                let author = c.author.clone();
                view! {
                    <CommentFrame at=c.created_at.clone() url=c.url.clone() body=c.body.clone()>
                        <PersonChip person=author guess=false />
                    </CommentFrame>
                }
                .into_any()
            }
            Entry::Review(r) => {
                let (author, class, what) = (r.author.clone(), state_class(r.state), verb(r.state));
                view! {
                    <CommentFrame at=r.submitted_at.clone() url=r.url.clone() body=r.body.clone()>
                        <PersonChip person=author guess=false />
                        " " <span class=class>{what}</span>
                    </CommentFrame>
                }
                .into_any()
            }
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        return ().into_any();
    }
    let count = items.len();
    view! {
        <details class="conversation" open>
            <summary>{if count == 1 { "1 comment".to_owned() } else { format!("{count} comments") }}</summary>
            {items}
        </details>
    }
    .into_any()
}

/// The checks on the PR's head and whether it can be merged. Only for open
/// PRs.
pub fn merge_box(info: &ReviewInfo) -> impl IntoView + use<> {
    let (class, text) = merge_status(info);
    let checks = (!info.checks.is_empty()).then(|| {
        let rows = info
            .checks
            .iter()
            .map(|c| {
                let icon = c.status.map(|ci| view! { <CiIcon ci=ci /> });
                let name = match &c.url {
                    Some(url) => view! { <a href=url.clone() target="_blank" rel="noopener">{c.name.clone()}</a> }.into_any(),
                    None => view! { <span>{c.name.clone()}</span> }.into_any(),
                };
                view! { <li class="check">{icon}{name}</li> }
            })
            .collect_view();
        // Failing checks are what needs attention, so they show unfolded.
        let open = info.ci == Some(CiStatus::Failing);
        view! {
            <details class="checks" open=open>
                <summary>
                    {info.ci.map(|ci| view! { <CiIcon ci=ci /> })}
                    {format!(" Checks: {}", check_counts(&info.checks))}
                </summary>
                <ul>{rows}</ul>
            </details>
        }
    });
    view! {
        <section class="merge-box">
            {checks}
            <p class=format!("merge-status {class}")>{text}</p>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{MergeInfo, Person};

    use super::*;

    fn review(login: &str, state: ReviewState, at: &str) -> Review {
        Review {
            author: Person {
                login: Some(login.into()),
                name: None,
                avatar_url: None,
            },
            state,
            body: String::new(),
            submitted_at: at.into(),
            url: String::new(),
        }
    }

    fn info() -> ReviewInfo {
        ReviewInfo {
            viewer: "me".into(),
            decision: None,
            reviews: vec![],
            requested: vec![],
            comments: vec![],
            checks: vec![],
            ci: None,
            merge: MergeInfo {
                conflicting: Some(false),
                state: MergeState::Clean,
                methods: vec![],
                default_method: None,
                delete_branch_on_merge: false,
                can_merge_as_admin: false,
            },
        }
    }

    #[test]
    fn a_comment_does_not_override_a_verdict() {
        use ReviewState::*;
        let reviews = [
            review("a", Approved, "1"),
            review("b", Commented, "2"),
            review("a", Commented, "3"),
            review("author", Commented, "4"),
            review("b", ChangesRequested, "5"),
            review("c", Commented, "6"),
            review("c", Commented, "7"),
        ];
        let latest: Vec<_> = latest_reviews(&reviews, "author")
            .into_iter()
            .map(|r| {
                (
                    r.author.login.clone().unwrap(),
                    r.state,
                    r.submitted_at.clone(),
                )
            })
            .collect();
        assert_eq!(
            latest,
            [
                ("a".into(), Approved, "1".into()),
                ("b".into(), ChangesRequested, "5".into()),
                ("c".into(), Commented, "7".into()),
            ]
        );
    }

    #[test]
    fn merge_statuses() {
        let mut i = info();
        assert_eq!(merge_status(&i).0, "ok");
        i.merge.state = MergeState::Blocked;
        i.decision = Some(ReviewDecision::ReviewRequired);
        assert_eq!(
            merge_status(&i),
            (
                "bad",
                "Merging is blocked: it needs an approving review.".into()
            )
        );
        i.decision = Some(ReviewDecision::ChangesRequested);
        assert_eq!(
            merge_status(&i).1,
            "Merging is blocked: changes were requested."
        );
        i.merge.state = MergeState::Dirty;
        assert_eq!(merge_status(&i).0, "bad");
        assert!(merge_status(&i).1.contains("conflicts"));
        i.merge.state = MergeState::Blocked;
        // Conflicts win over everything else.
        i.merge.conflicting = Some(true);
        assert_eq!(merge_status(&i).0, "bad");
        assert!(merge_status(&i).1.contains("conflicts"));
        i.merge.conflicting = None;
        i.merge.state = MergeState::Unknown;
        assert_eq!(merge_status(&i).0, "warn");
    }

    #[test]
    fn counts_checks() {
        let check = |status| Check {
            name: String::new(),
            status,
            url: None,
        };
        assert_eq!(
            check_counts(&[
                check(Some(CiStatus::Passing)),
                check(Some(CiStatus::Failing)),
                check(Some(CiStatus::Passing)),
                check(None),
            ]),
            "1 failing, 2 passing, 1 skipped"
        );
    }

    #[test]
    fn conversation_skips_bare_comment_reviews() {
        let mut i = info();
        let mut summary = review("a", ReviewState::Commented, "2026-01-01T00:03:00Z");
        summary.body = "Overall fine".into();
        i.reviews = vec![
            review("a", ReviewState::Commented, "2026-01-01T00:01:00Z"),
            review("b", ReviewState::Approved, "2026-01-01T00:04:00Z"),
            summary,
        ];
        i.comments = vec![IssueComment {
            author: Person::default(),
            body: "hi".into(),
            created_at: "2026-01-01T00:02:00Z".into(),
            url: String::new(),
        }];
        let at: Vec<_> = entries(&i).iter().map(|e| e.at().to_owned()).collect();
        assert_eq!(
            at,
            [
                "2026-01-01T00:02:00Z",
                "2026-01-01T00:03:00Z",
                "2026-01-01T00:04:00Z"
            ]
        );
    }
}
