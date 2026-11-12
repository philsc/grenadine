//! The Threads tab of a PR's sidebar: the whole conversation of each of
//! the PR's review threads, filtered by whether the thread is open,
//! resolved or pending, with a link that shows it in the diff.

use std::sync::Arc;

use grenadine_core::api::{PrDetail, PrKey, ReviewComment};
use leptos::prelude::*;

use crate::inboxes::{load_flag, save_flag};
use crate::pr::{
    CommentBody, DraftScope, PersonChip, PrLinks, ReviewTarget, Thread, ThreadActions, ThreadJump,
    ThreadWhere, range_label, threads,
};
use crate::sidebar::Sidebar;

/// What the names of the tab's reply and edit boxes start with, so that
/// they're apart from the diff's boxes for the same threads.
pub const DRAFT_SCOPE: &str = "sidebar.";

/// What the filter tells threads apart by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Submitted and not resolved.
    Open,
    Resolved,
    /// Started in the viewer's pending review: only the viewer sees it
    /// until the review is submitted.
    Pending,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Open, Kind::Resolved, Kind::Pending];

    fn label(self) -> &'static str {
        match self {
            Kind::Open => "Open",
            Kind::Resolved => "Resolved",
            Kind::Pending => "Pending",
        }
    }

    /// The localStorage key of whether the filter shows the kind.
    fn flag(self) -> &'static str {
        match self {
            Kind::Open => "pr-threads-open",
            Kind::Resolved => "pr-threads-resolved",
            Kind::Pending => "pr-threads-pending",
        }
    }

    /// Whether the filter shows the kind until the user says otherwise.
    fn default_shown(self) -> bool {
        self != Kind::Resolved
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// A thread's kind, which its root comment decides: a pending reply to an
/// open thread leaves it open.
pub fn kind(t: &Thread) -> Kind {
    if t.root.pending {
        Kind::Pending
    } else if t.root.resolved {
        Kind::Resolved
    } else {
        Kind::Open
    }
}

/// The number of threads of each kind, indexed by `Kind::index`.
pub fn counts(comments: &[ReviewComment]) -> [usize; 3] {
    let mut out = [0; 3];
    for t in threads(comments) {
        out[kind(&t).index()] += 1;
    }
    out
}

/// The threads of the kinds in `shown`, grouped by file in path order.
/// In a file, threads on the whole file come first, then the others by
/// line: on the PR's head where GitHub has one, else where they were
/// made.
pub fn listed(comments: &[ReviewComment], shown: &[Kind]) -> Vec<(String, Vec<Thread>)> {
    let mut list: Vec<Thread> = threads(comments)
        .into_iter()
        .filter(|t| shown.contains(&kind(t)))
        .collect();
    list.sort_by_key(|t| {
        let c = &t.root;
        (
            c.path.clone(),
            !c.on_file,
            c.line.or(c.original_line).unwrap_or(0),
            c.id,
        )
    });
    let mut out: Vec<(String, Vec<Thread>)> = Vec::new();
    for t in list {
        match out.last_mut() {
            Some((path, group)) if *path == t.root.path => group.push(t),
            _ => out.push((t.root.path.clone(), vec![t])),
        }
    }
    out
}

/// Where in its file a thread was made: "File", "Line 12", or a range
/// like "Lines +4 to +7".
pub fn where_label(c: &ReviewComment) -> String {
    if c.on_file {
        return "File".to_owned();
    }
    let side = c.start_side.unwrap_or(c.side);
    match (c.original_start_line, c.original_line) {
        (Some(s), Some(e)) if (side, s) != (c.side, e) => range_label((side, s), (c.side, e)),
        (_, Some(l)) => format!("Line {l}"),
        _ => "Lines unknown".to_owned(),
    }
}

/// The tab's filter and its threads. `detail` keeps the last PR loaded
/// while a refetch runs.
#[component]
pub fn ThreadsTab(
    key: PrKey,
    detail: Memo<Option<Arc<PrDetail>>>,
    sidebar: Sidebar,
) -> impl IntoView {
    provide_context(DraftScope(DRAFT_SCOPE));
    provide_context(PrLinks {
        repo: key.repo.clone(),
    });
    provide_context(ReviewTarget {
        key,
        reviewing: Signal::derive(move || {
            detail.with(|d| {
                d.as_ref().is_some_and(|d| {
                    d.review
                        .as_ref()
                        .is_some_and(|r| r.pending_review.is_some())
                })
            })
        }),
    });
    let shown = Kind::ALL.map(|k| RwSignal::new(load_flag(k.flag(), k.default_shown())));
    let counts = Memo::new(move |_| {
        detail.with(|d| d.as_ref().map(|d| counts(&d.comments)).unwrap_or_default())
    });
    let chips = Kind::ALL
        .into_iter()
        .map(|k| {
            let on = shown[k.index()];
            view! {
                <button
                    type="button"
                    class="thread-chip"
                    class:on=move || on.get()
                    aria-pressed=move || on.get().to_string()
                    on:click=move |_| {
                        let v = !on.get_untracked();
                        on.set(v);
                        save_flag(k.flag(), v);
                    }
                >
                    {k.label()}
                    " "
                    <span class="thread-chip-count">{move || counts.get()[k.index()]}</span>
                </button>
            }
        })
        .collect_view();
    let list = move || {
        let Some(d) = detail.get() else {
            return ().into_any();
        };
        let kinds: Vec<Kind> = Kind::ALL
            .into_iter()
            .filter(|k| shown[k.index()].get())
            .collect();
        let groups = listed(&d.comments, &kinds);
        if groups.is_empty() {
            let text = if d.comments.is_empty() {
                "This PR has no review threads."
            } else if kinds == [Kind::Open] {
                "No open threads."
            } else {
                "No threads match the filter."
            };
            return view! { <p class="muted threads-empty">{text}</p> }.into_any();
        }
        groups
            .into_iter()
            .map(|(path, threads)| {
                let items = threads
                    .into_iter()
                    .map(|t| {
                        let version = d
                            .versions
                            .iter()
                            .find(|v| v.sha == t.root.original_commit)
                            .map(|v| v.number);
                        view! { <ListedThread thread=t version=version sidebar=sidebar /> }
                    })
                    .collect_view();
                view! {
                    <section class="thread-file">
                        <h3 class="thread-file-path"><code>{path}</code></h3>
                        {items}
                    </section>
                }
            })
            .collect_view()
            .into_any()
    };
    view! {
        <div class="thread-filter" role="group" aria-label="Show threads">{chips}</div>
        {list}
    }
}

/// One thread with all its comments, under where it was made. Its place
/// shows it in the diff.
#[component]
fn ListedThread(thread: Thread, version: Option<u32>, sidebar: Sidebar) -> impl IntoView {
    let jump = expect_context::<ThreadJump>().0;
    let kind = kind(&thread);
    let root = thread.root.clone();
    let id = root.id;
    let outdated = root.line.is_none() && !root.on_file && !root.pending;
    let status = (kind == Kind::Resolved).then(|| match root.resolved_by.clone() {
        Some(person) => view! {
            <span class="thread-resolved">"Resolved by " <PersonChip person=person guess=false /></span>
        }
        .into_any(),
        None => view! { <span class="thread-resolved">"Resolved"</span> }.into_any(),
    });
    view! {
        <article class="listed-thread" class:resolved=kind == Kind::Resolved data-thread=id.to_string()>
            <header class="listed-thread-where">
                <ThreadWhere version=version>
                    <button
                        type="button"
                        class="link thread-jump"
                        title="Show it in the diff"
                        on:click=move |_| {
                            sidebar.hide_overlay();
                            jump.set(Some(id));
                        }
                    >
                        {where_label(&root)}
                    </button>
                </ThreadWhere>
                {outdated.then(|| view! { " " <span class="badge outdated-badge">"Outdated"</span> })}
                {status}
            </header>
            <CommentBody comment=thread.root.clone() />
            {thread.replies.iter().map(|r| view! { <CommentBody comment=r.clone() /> }).collect_view()}
            <ThreadActions root=thread.root />
        </article>
    }
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::Side;

    use super::*;

    fn comment(id: u64, path: &str, line: Option<u32>) -> ReviewComment {
        ReviewComment {
            id,
            in_reply_to: None,
            author: "a".into(),
            body: "b".into(),
            path: path.into(),
            original_commit: "c".into(),
            original_line: line,
            original_start_line: None,
            line,
            start_line: None,
            start_side: None,
            side: Side::Right,
            on_file: false,
            created_at: String::new(),
            url: String::new(),
            resolved: false,
            resolved_by: None,
            thread_id: None,
            can_resolve: false,
            pending: false,
            node_id: None,
        }
    }

    fn reply(id: u64, to: u64) -> ReviewComment {
        ReviewComment {
            in_reply_to: Some(to),
            ..comment(id, "f.rs", Some(1))
        }
    }

    fn ids(groups: &[(String, Vec<Thread>)]) -> Vec<(&str, Vec<u64>)> {
        groups
            .iter()
            .map(|(p, ts)| (p.as_str(), ts.iter().map(|t| t.root.id).collect()))
            .collect()
    }

    #[test]
    fn the_root_decides_the_kind() {
        let resolved = ReviewComment {
            resolved: true,
            ..comment(1, "f.rs", Some(1))
        };
        let pending = ReviewComment {
            pending: true,
            ..comment(2, "f.rs", Some(2))
        };
        let open = comment(3, "f.rs", Some(3));
        // A pending reply leaves its thread open.
        let pending_reply = ReviewComment {
            pending: true,
            ..reply(4, 3)
        };
        let comments = [resolved, pending, open, pending_reply];
        let kinds: Vec<Kind> = threads(&comments).iter().map(kind).collect();
        assert_eq!(kinds, [Kind::Resolved, Kind::Pending, Kind::Open]);
        assert_eq!(counts(&comments), [1, 1, 1]);
    }

    #[test]
    fn listed_filters_by_kind() {
        let comments = [
            comment(1, "f.rs", Some(1)),
            ReviewComment {
                resolved: true,
                ..comment(2, "f.rs", Some(2))
            },
            ReviewComment {
                pending: true,
                ..comment(3, "f.rs", Some(3))
            },
        ];
        assert_eq!(
            ids(&listed(&comments, &[Kind::Open, Kind::Pending])),
            [("f.rs", vec![1, 3])]
        );
        assert_eq!(
            ids(&listed(&comments, &[Kind::Resolved])),
            [("f.rs", vec![2])]
        );
        assert_eq!(
            ids(&listed(&comments, &Kind::ALL)),
            [("f.rs", vec![1, 2, 3])]
        );
        assert!(listed(&comments, &[]).is_empty());
    }

    #[test]
    fn listed_groups_by_file_then_line() {
        let comments = [
            comment(1, "b.rs", Some(30)),
            comment(2, "a.rs", Some(20)),
            comment(3, "b.rs", Some(4)),
            ReviewComment {
                on_file: true,
                ..comment(4, "b.rs", None)
            },
            // Outdated: no line on the head, so where it was made.
            ReviewComment {
                line: None,
                ..comment(5, "b.rs", Some(10))
            },
            reply(6, 2),
        ];
        let groups = listed(&comments, &[Kind::Open]);
        assert_eq!(
            ids(&groups),
            [("a.rs", vec![2]), ("b.rs", vec![4, 3, 5, 1])]
        );
        // Replies stay in their thread.
        assert_eq!(groups[0].1[0].replies.len(), 1);
    }

    #[test]
    fn where_label_names_the_lines() {
        assert_eq!(where_label(&comment(1, "f.rs", Some(12))), "Line 12");
        let range = ReviewComment {
            original_start_line: Some(4),
            ..comment(2, "f.rs", Some(7))
        };
        assert_eq!(where_label(&range), "Lines +4 to +7");
        let file = ReviewComment {
            on_file: true,
            ..comment(3, "f.rs", None)
        };
        assert_eq!(where_label(&file), "File");
    }
}
