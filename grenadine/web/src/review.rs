//! A PR's reviews, its conversation and whether it can be merged.

use grenadine_core::api::{
    Check, CiStatus, IssueComment, Person, PrKey, Review, ReviewEvent, ReviewInfo, ReviewState,
    SubmitReview,
};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::composer::{Composer, OpenBoxes, comment_draft_key, draft_signal, sender};
use crate::inboxes::{CiIcon, storage};
use crate::pr::{PersonChip, PrLinks, ReviewTarget, markdown_click};
use crate::{api, markdown};

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
    match info.merge.state.as_str() {
        "CLEAN" | "HAS_HOOKS" => ("ok", "Ready to merge.".into()),
        "UNSTABLE" => ("warn", "Can be merged, but not every check passed.".into()),
        "BEHIND" => ("warn", "This branch is out of date with its base.".into()),
        "DRAFT" => ("warn", "This PR is a draft.".into()),
        "DIRTY" => (
            "bad",
            "This branch has conflicts that must be resolved.".into(),
        ),
        "BLOCKED" => {
            let why = match info.decision.as_deref() {
                Some("REVIEW_REQUIRED") => "it needs an approving review",
                Some("CHANGES_REQUESTED") => "changes were requested",
                _ => "required checks or rules aren't satisfied",
            };
            ("bad", format!("Merging is blocked: {why}."))
        }
        _ => (
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
#[component]
pub fn ReviewSummary(info: ReviewInfo, author: String) -> impl IntoView {
    let decision = info.decision.as_deref().map(|d| match d {
        "APPROVED" => ("review-decision approved", "Approved"),
        "CHANGES_REQUESTED" => ("review-decision changes", "Changes requested"),
        _ => ("review-decision required", "Review required"),
    });
    let reviewers = latest_reviews(&info.reviews, &author)
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
            {decision.map(|(class, text)| view! { <span class=class>{text}</span> })}
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

fn time(ts: &str) -> String {
    ts.replace('T', " ").trim_end_matches('Z').to_owned()
}

#[component]
fn EntryView(
    author: Person,
    what: Option<(&'static str, &'static str)>,
    at: String,
    url: String,
    body: String,
) -> impl IntoView {
    let links = expect_context::<PrLinks>();
    let body = (!body.trim().is_empty()).then(|| {
        view! {
            <div class="markdown" inner_html=markdown::to_html(&body, &links.repo)
                on:click=move |ev| markdown_click(ev, links.invert.get_untracked())></div>
        }
    });
    view! {
        <div class="comment">
            <div class="comment-meta">
                <PersonChip person=author guess=false />
                {what.map(|(class, text)| view! { " " <span class=class>{text}</span> })}
                " "
                <a class="muted" href=url target="_blank" rel="noopener">{time(&at)}</a>
            </div>
            {body}
        </div>
    }
}

/// The comments on the PR itself and the reviews' summaries.
#[component]
pub fn Conversation(info: ReviewInfo) -> impl IntoView {
    let items = entries(&info)
        .into_iter()
        .map(|e| match e {
            Entry::Comment(c) => view! {
                <EntryView author=c.author.clone() what=None at=c.created_at.clone() url=c.url.clone() body=c.body.clone() />
            },
            Entry::Review(r) => view! {
                <EntryView author=r.author.clone() what=Some((state_class(r.state), verb(r.state))) at=r.submitted_at.clone() url=r.url.clone() body=r.body.clone() />
            },
        })
        .collect::<Vec<_>>();
    let count = items.len();
    view! {
        <details class="conversation" open>
            <summary>{match count {
                0 => "No comments".to_owned(),
                1 => "1 comment".to_owned(),
                n => format!("{n} comments"),
            }}</summary>
            {items}
            <NewComment />
        </details>
    }
}

/// A comment on the PR itself, behind a button until it's started.
#[component]
fn NewComment() -> impl IntoView {
    let ReviewTarget { key, .. } = expect_context::<ReviewTarget>();
    let links = expect_context::<PrLinks>();
    let draft = format!("comment.{}#{}", key.repo, key.number);
    let boxes = expect_context::<OpenBoxes>();
    let writing = {
        let draft = draft.clone();
        Signal::derive(move || boxes.is_open(&draft))
    };
    let send = sender(move |body| {
        let key = key.clone();
        async move { api::add_comment(&key, body).await }
    });
    let open = {
        let draft = draft.clone();
        move |_| boxes.set(&draft, true)
    };
    move || {
        if writing.get() {
            view! {
                <div class="new-comment">
                    <Composer
                        draft=draft.clone()
                        repo=links.repo.clone()
                        label="Comment"
                        placeholder="Comment on this PR…"
                        send=send.clone()
                        autofocus=true
                    />
                </div>
            }
            .into_any()
        } else {
            view! {
                <div class="new-comment form-buttons">
                    <button type="button" on:click=open.clone()>"Comment"</button>
                </div>
            }
            .into_any()
        }
    }
}

/// Why a review can't be submitted as it is, if it can't.
pub fn submit_blocker(event: ReviewEvent, body: &str, pending: usize) -> Option<&'static str> {
    let empty = body.trim().is_empty();
    match event {
        ReviewEvent::RequestChanges if empty => Some("Say what needs to change."),
        ReviewEvent::Comment if empty && pending == 0 => {
            Some("Write a summary or add comments to the review.")
        }
        _ => None,
    }
}

/// What the review panel shows, kept above the parts of the page that a
/// refetch rebuilds.
#[derive(Clone, Copy)]
pub struct ReviewChoice {
    event: RwSignal<ReviewEvent>,
    open: RwSignal<bool>,
    preview: RwSignal<bool>,
}

impl ReviewChoice {
    pub fn new() -> ReviewChoice {
        ReviewChoice {
            event: RwSignal::new(ReviewEvent::Comment),
            open: RwSignal::new(false),
            preview: RwSignal::new(false),
        }
    }
}

/// Where the summary of the viewer's review of `key` is kept as a draft.
fn summary_key(key: &PrKey) -> String {
    comment_draft_key(&format!("review.{}#{}", key.repo, key.number))
}

/// Submitting the viewer's review, or discarding it. `pending` counts the
/// comments in their pending review; `own` says the PR is the viewer's,
/// which they can't approve or request changes on.
#[component]
pub fn ReviewPanel(pending: usize, own: bool) -> impl IntoView {
    let ReviewTarget { key, reviewing } = expect_context::<ReviewTarget>();
    let links = expect_context::<PrLinks>();
    let body = draft_signal(comment_draft_key(&format!(
        "review.{}#{}",
        key.repo, key.number
    )));
    let ReviewChoice {
        event,
        open,
        preview,
    } = expect_context::<ReviewChoice>();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let blocker = move || body.with(|b| submit_blocker(event.get(), b, pending));
    let key = StoredValue::new(key);
    let submit = move |_| {
        if blocker().is_some() {
            return;
        }
        let review = SubmitReview {
            event: event.get_untracked(),
            body: body.get_untracked(),
        };
        busy.set(true);
        error.set(None);
        let key = key.get_value();
        spawn_local(async move {
            match api::submit_review(&key, &review).await {
                Ok(()) => {
                    // The panel may have been rebuilt meanwhile; its summary's
                    // draft goes either way.
                    if let Some(s) = storage() {
                        let _ = s.remove_item(&summary_key(&key));
                    }
                    let _ = body.try_set(String::new());
                    event.set(ReviewEvent::Comment);
                    open.set(false);
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
            let _ = busy.try_set(false);
        });
    };
    let discard = move |_| {
        if !window()
            .confirm_with_message("Discard your pending review and its comments?")
            .unwrap_or(false)
        {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            if let Err(e) = api::discard_review(&key.get_value()).await {
                let _ = error.try_set(Some(e));
            }
            let _ = busy.try_set(false);
        });
    };
    let choice = move |value: ReviewEvent, label: &'static str, hint: &'static str| {
        let disabled = own && value != ReviewEvent::Comment;
        view! {
            <label class="review-choice" class:disabled=disabled title=if disabled { "You can't approve or request changes on your own PR" } else { hint }>
                <input type="radio" name="review-event" disabled=disabled
                    prop:checked=move || event.get() == value
                    on:change=move |_| event.set(value) />
                " " {label}
            </label>
        }
    };
    view! {
        // Not a <details>: its toggle events race with the page being
        // rebuilt when the PR is refetched.
        <div class="review-panel" class:open=move || open.get()>
            <button type="button" class="review-button" aria-expanded=move || open.get().to_string()
                on:click=move |_| open.update(|o| *o = !*o)>
                "Review"
                {(pending > 0).then(|| view! { <span class="badge pending-count" title="Pending comments">{pending}</span> })}
            </button>
            <div class="review-popup" class:hidden=move || !open.get()
                on:keydown=move |ev| {
                    if ev.key() == "Escape" {
                        open.set(false);
                    }
                }>
                <div class="composer-tabs" role="tablist">
                    <button type="button" role="tab" class:active=move || !preview.get()
                        on:click=move |_| preview.set(false)>"Write"</button>
                    <button type="button" role="tab" class:active=move || preview.get()
                        on:click=move |_| preview.set(true)>"Preview"</button>
                </div>
                <textarea
                    rows="5"
                    aria-label="Review summary"
                    placeholder="Leave a summary (optional for approvals)…"
                    class:hidden=move || preview.get()
                    prop:value=move || body.get()
                    on:input=move |ev| body.set(event_target_value(&ev))
                ></textarea>
                {move || preview.get().then(|| {
                    let html = body.with(|b| markdown::to_html(b, &links.repo));
                    view! { <div class="markdown composer-preview" inner_html=html></div> }
                })}
                <div class="review-choices">
                    {choice(ReviewEvent::Comment, "Comment", "Feedback without approving")}
                    {choice(ReviewEvent::Approve, "Approve", "Approve merging these changes")}
                    {choice(ReviewEvent::RequestChanges, "Request changes", "Feedback that must be addressed before merging")}
                </div>
                {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                <div class="form-buttons">
                    <button type="button" class="primary" title=move || blocker().unwrap_or_default()
                        disabled=move || busy.get() || blocker().is_some() on:click=submit>
                        {move || if busy.get() { "Submitting…" } else { "Submit review" }}
                    </button>
                    {reviewing.then(|| view! {
                        <button type="button" class="danger" disabled=move || busy.get() on:click=discard>"Discard review"</button>
                    })}
                </div>
            </div>
        </div>
    }
}

/// The checks on the PR's head and whether it can be merged. Only for open
/// PRs.
#[component]
pub fn MergeBox(info: ReviewInfo) -> impl IntoView {
    let (class, text) = merge_status(&info);
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
    use grenadine_core::api::MergeInfo;

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
            pending_review: None,
            merge: MergeInfo {
                conflicting: Some(false),
                state: "CLEAN".into(),
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
        i.merge.state = "BLOCKED".into();
        i.decision = Some("REVIEW_REQUIRED".into());
        assert_eq!(
            merge_status(&i),
            (
                "bad",
                "Merging is blocked: it needs an approving review.".into()
            )
        );
        // Conflicts win over everything else.
        i.merge.conflicting = Some(true);
        assert_eq!(merge_status(&i).0, "bad");
        assert!(merge_status(&i).1.contains("conflicts"));
        i.merge.conflicting = None;
        i.merge.state = "UNKNOWN".into();
        assert_eq!(merge_status(&i).0, "warn");
    }

    #[test]
    fn what_a_review_needs() {
        use ReviewEvent::*;
        assert_eq!(submit_blocker(Approve, "", 0), None);
        assert!(submit_blocker(RequestChanges, " ", 3).is_some());
        assert_eq!(submit_blocker(RequestChanges, "Fix it", 0), None);
        assert!(submit_blocker(Comment, "", 0).is_some());
        assert_eq!(submit_blocker(Comment, "", 1), None);
        assert_eq!(submit_blocker(Comment, "Hm", 0), None);
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
