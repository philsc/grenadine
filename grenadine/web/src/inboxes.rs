//! The landing page: the list of inboxes, each with the PRs its filter
//! matches. Inboxes can be added, edited, reordered and deleted in place.

use std::collections::HashMap;

use grenadine_core::api::{
    CiStatus, Inbox, InboxEdit, InboxWithPrs, PrKey, ReviewDecision, ThreadCounts,
};
use grenadine_core::stack::{Link, group_inbox};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::{Updates, api, hash_for};

/// Rows shown when an inbox section first renders, and added per click of
/// its "Show more" button.
const INITIAL_ROWS: usize = 15;
const MORE_ROWS: usize = 10;

#[component]
pub fn Inboxes(
    /// The PR viewed most recently, highlighted in its inbox.
    last: RwSignal<Option<PrKey>>,
) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let inboxes = LocalResource::new(move || {
        updates.inboxes.track();
        api::inboxes()
    });
    let adding = RwSignal::new(false);
    // Kept here rather than in InboxSection so a refetch that rebuilds the
    // sections doesn't reset them; a page reload still does.
    let shown = RwSignal::new(HashMap::<i64, usize>::new());

    view! {
        <div class="inboxes">
            {move || match inboxes.get() {
                None => view! { <p class="muted pad">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <p class="error pad">{e}</p> }.into_any(),
                Some(Ok(list)) => {
                    let all: Vec<Inbox> = list.iter().map(|i| i.inbox.clone()).collect();
                    list.into_iter()
                        .enumerate()
                        .map(|(i, inbox)| {
                            let prev = i.checked_sub(1).map(|p| all[p].clone());
                            let next = all.get(i + 1).cloned();
                            view! { <InboxSection inbox=inbox last=last prev=prev next=next shown=shown /> }
                        })
                        .collect_view()
                        .into_any()
                }
            }}
            {move || if adding.get() {
                view! {
                    <InboxForm
                        initial=InboxEdit { name: String::new(), filter: "state:open archived:false draft:false ".into(), position: None, count_in_total: false }
                        on_done=Callback::new(move |_| adding.set(false))
                        id=None
                    />
                }.into_any()
            } else {
                view! {
                    <button class="add-inbox" on:click=move |_| adding.set(true)>"+ Add inbox"</button>
                }.into_any()
            }}
        </div>
    }
}

/// Swaps the positions of two inboxes by rewriting both.
async fn swap(a: Inbox, b: Inbox) -> api::Result<()> {
    let (pa, pb) = if a.position == b.position {
        (a.position + 1, a.position)
    } else {
        (b.position, a.position)
    };
    api::update_inbox(
        a.id,
        &InboxEdit {
            name: a.name,
            filter: a.filter,
            position: Some(pa),
            count_in_total: a.count_in_total,
        },
    )
    .await?;
    api::update_inbox(
        b.id,
        &InboxEdit {
            name: b.name,
            filter: b.filter,
            position: Some(pb),
            count_in_total: b.count_in_total,
        },
    )
    .await
}

#[component]
fn InboxSection(
    inbox: InboxWithPrs,
    last: RwSignal<Option<PrKey>>,
    /// The inboxes above and below, to swap places with.
    prev: Option<Inbox>,
    next: Option<Inbox>,
    /// Rows shown per inbox id.
    shown: RwSignal<HashMap<i64, usize>>,
) -> impl IntoView {
    let collapsed_key = format!("inbox-collapsed-{}", inbox.inbox.id);
    let collapsed = RwSignal::new(load_flag(&collapsed_key, false));
    let editing = RwSignal::new(false);
    let InboxWithPrs {
        inbox,
        prs,
        error,
        truncated,
    } = inbox;
    let count = prs.len();
    let id = inbox.id;
    let rows = group_inbox(&prs);
    let total = rows.len();
    let limit = move || shown.with(|m| m.get(&id).copied().unwrap_or(INITIAL_ROWS));
    let updates = expect_context::<Updates>();

    let reorder = {
        let this = inbox.clone();
        move |other: Option<Inbox>| {
            let Some(other) = other else { return };
            let this = this.clone();
            spawn_local(async move {
                let _ = swap(this, other).await;
                updates.inboxes.update(|n| *n += 1);
            });
        }
    };
    let (up_disabled, down_disabled) = (prev.is_none(), next.is_none());
    let reorder_up = reorder.clone();

    let header_inbox = inbox.clone();
    view! {
        <section class="inbox">
            <div class="inbox-header">
                <button
                    class="inbox-toggle"
                    on:click=move |_| {
                        collapsed.update(|c| *c = !*c);
                        save_flag(&collapsed_key, collapsed.get_untracked());
                    }
                    title=header_inbox.filter.clone()
                >
                    <span class="chevron">{move || if collapsed.get() { "▸" } else { "▾" }}</span>
                    <span class="inbox-name">{header_inbox.name.clone()}</span>
                    <span class="count">{count}</span>
                </button>
                <span class="inbox-actions">
                    <button class="icon" title="Move up" disabled=up_disabled on:click=move |_| reorder_up(prev.clone())>"↑"</button>
                    <button class="icon" title="Move down" disabled=down_disabled on:click=move |_| reorder(next.clone())>"↓"</button>
                    <button class="icon" title="Edit" on:click=move |_| editing.update(|e| *e = !*e)>"✎"</button>
                </span>
            </div>
            {move || editing.get().then(|| {
                let i = inbox.clone();
                view! {
                    <InboxForm
                        initial=InboxEdit { name: i.name.clone(), filter: i.filter.clone(), position: Some(i.position), count_in_total: i.count_in_total }
                        on_done=Callback::new(move |_| editing.set(false))
                        id=Some(i.id)
                    />
                }
            })}
            {error.map(|e| view! { <p class="error inbox-error">{e}</p> })}
            {truncated.map(|t| view! {
                <p class="inbox-truncated">
                    {format!("GitHub found {} PRs; grenadine fetched {}. Tighten the filter to see the rest.", t.found, t.fetched)}
                </p>
            })}
            <ul class="pr-list" class:hidden=move || collapsed.get()>
                {if rows.is_empty() {
                    view! { <li class="muted none">"No PRs"</li> }.into_any()
                } else {
                    let prs = StoredValue::new_local(prs);
                    let rows = StoredValue::new_local(rows);
                    (move || {
                        let cut = limit().min(total);
                        let mut above = None;
                        rows.with_value(|rows| {
                            rows.iter().take(cut).enumerate().map(|(i, row)| {
                                let link_above = std::mem::replace(&mut above, row.link);
                                // A cut that splits a stack dashes the last
                                // visible row's connector to show that more
                                // of the stack is hidden.
                                let link_below = match row.link {
                                    Some(_) if cut < total && i + 1 == cut => Some(Link::Gap),
                                    link => link,
                                };
                                prs.with_value(|prs| {
                                    let pr = &prs[row.index];
                                    let key = pr.key.clone();
                                    let is_selected = {
                                        let key = key.clone();
                                        move || last.with(|s| s.as_ref() == Some(&key))
                                    };
                                    let badge = pr
                                        .stack
                                        .as_ref()
                                        .filter(|s| s.position > 0 && s.length > 1)
                                        .map(|s| view! {
                                            <span class="badge" title="Position in its stack">{format!("{}/{}", s.position, s.length)}</span>
                                        });
                                    view! {
                                        <li
                                            class:stack-above=link_above.is_some()
                                            class:stack-above-gap=link_above == Some(Link::Gap)
                                            class:stack-below=link_below.is_some()
                                            class:stack-below-gap=link_below == Some(Link::Gap)
                                        >
                                            <a
                                                class="pr-item"
                                                class:selected=is_selected
                                                href=hash_for(&key)
                                            >
                                                <span class="pr-text">
                                                    <span class="pr-title">
                                                        {badge}
                                                        {pr.is_draft.then(|| view! { <span class="badge">"draft"</span> })}
                                                        {pr.title.clone()}
                                                    </span>
                                                    <span class="pr-meta muted" title=pr.sync_error.clone()>
                                                        {format!("{}#{} · {} · ", pr.key.repo, pr.key.number, pr.author)}
                                                        {if pr.synced {
                                                            format!("{} versions", pr.version_count).into_any()
                                                        } else if pr.sync_error.is_some() {
                                                            view! { <span class="error">"sync failed"</span> }.into_any()
                                                        } else {
                                                            view! { <span>"syncing…"</span> }.into_any()
                                                        }}
                                                    </span>
                                                </span>
                                                <span class="pr-status">
                                                    {pr.threads.filter(|t| t.total > 0).map(|t| view! { <ThreadCount threads=t /> })}
                                                    {pr.review_decision.map(|d| view! { <ReviewIcon decision=d /> })}
                                                    {pr.ci.map(|ci| view! { <CiIcon ci=ci /> })}
                                                </span>
                                            </a>
                                        </li>
                                    }
                                })
                            }).collect_view()
                        })
                    }).into_any()
                }}
            </ul>
            {move || (total > limit()).then(|| view! {
                <button
                    class="inbox-more"
                    class:hidden=move || collapsed.get()
                    on:click=move |_| shown.update(|m| *m.entry(id).or_insert(INITIAL_ROWS) += MORE_ROWS)
                >
                    {format!("Show {} more", (total - limit()).min(MORE_ROWS))}
                </button>
            })}
        </section>
    }
}

/// How many of the PR's review threads are resolved, e.g. "1/3".
#[component]
fn ThreadCount(threads: ThreadCounts) -> impl IntoView {
    let ThreadCounts { resolved, total } = threads;
    let noun = if total == 1 { "thread" } else { "threads" };
    view! {
        <span
            class="threads"
            class:done=resolved == total
            title=format!("{resolved} of {total} {noun} resolved")
        >
            <svg
                viewBox="0 0 16 16"
                width="12"
                height="12"
                fill="none"
                stroke="currentColor"
                stroke-width="1.4"
                stroke-linecap="round"
                stroke-linejoin="round"
                aria-hidden="true"
            >
                <path d="M2.5 3.5a1 1 0 0 1 1-1h9a1 1 0 0 1 1 1v6.5a1 1 0 0 1-1 1H7l-3 2.5V11h-.5a1 1 0 0 1-1-1z" />
            </svg>
            {format!("{resolved}/{total}")}
        </span>
    }
}

/// GitHub's review decision on the PR: approved, changes requested, or a
/// review still required. Nothing for a decision grenadine doesn't know.
#[component]
fn ReviewIcon(decision: ReviewDecision) -> impl IntoView {
    // Unlike CI's check and the threads' speech bubble: a person with a
    // check, a file with a diff's plus and minus, and an eye.
    let path = match decision {
        ReviewDecision::Approved => {
            "M6 7.5a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5zM1.5 13.5c0-2.5 2-4.5 4.5-4.5 1 0 1.9.3 2.6.8M9.5 12l1.5 1.5 3.5-4"
        }
        ReviewDecision::ChangesRequested => "M3.5 1.5h6l3 3v10h-9zM8 4.5v4M6 6.5h4M6 11.5h4",
        ReviewDecision::ReviewRequired => {
            "M1.5 8s2.5-4.5 6.5-4.5 6.5 4.5 6.5 4.5-2.5 4.5-6.5 4.5S1.5 8 1.5 8zM8 9.5a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3z"
        }
        ReviewDecision::Unknown => return None,
    };
    Some(view! {
        <span class=format!("review {}", decision.class()) title=decision.label()>
            <svg
                viewBox="0 0 16 16"
                width="12"
                height="12"
                fill="none"
                stroke="currentColor"
                stroke-width="1.5"
                stroke-linecap="round"
                stroke-linejoin="round"
                aria-hidden="true"
            >
                <path d=path />
            </svg>
        </span>
    })
}

/// Whether the checks on the PR's head pass, fail or are still running.
#[component]
pub(crate) fn CiIcon(ci: CiStatus) -> impl IntoView {
    let (class, title, path) = match ci {
        CiStatus::Passing => ("ci passing", "CI passing", "M3.5 8.5l3 3 6-7"),
        CiStatus::Failing => ("ci failing", "CI failing", "M4 4l8 8M12 4l-8 8"),
        CiStatus::Running => ("ci running", "CI running", "M8 4.5V8l2.5 1.5"),
    };
    view! {
        <span class=class title=title>
            <svg
                viewBox="0 0 16 16"
                width="12"
                height="12"
                fill="none"
                stroke="currentColor"
                stroke-width="1.6"
                stroke-linecap="round"
                stroke-linejoin="round"
                aria-hidden="true"
            >
                {(ci == CiStatus::Running).then(|| view! { <circle cx="8" cy="8" r="6" /> })}
                <path d=path />
            </svg>
        </span>
    }
}

#[component]
fn InboxForm(initial: InboxEdit, on_done: Callback<()>, id: Option<i64>) -> impl IntoView {
    let name = RwSignal::new(initial.name);
    let filter = RwSignal::new(initial.filter);
    let count_in_total = RwSignal::new(initial.count_in_total);
    let error = RwSignal::new(None::<String>);
    let position = initial.position;
    let updates = expect_context::<Updates>();

    let save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let edit = InboxEdit {
            name: name.get_untracked(),
            filter: filter.get_untracked(),
            position,
            count_in_total: count_in_total.get_untracked(),
        };
        spawn_local(async move {
            let result = match id {
                Some(id) => api::update_inbox(id, &edit).await,
                None => api::create_inbox(&edit).await,
            };
            match result {
                Ok(()) => {
                    updates.inboxes.update(|n| *n += 1);
                    on_done.run(());
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let delete = move |_| {
        let Some(id) = id else { return };
        let confirmed = window()
            .confirm_with_message(&format!("Delete the inbox \"{}\"?", name.get_untracked()))
            .unwrap_or(false);
        if !confirmed {
            return;
        }
        spawn_local(async move {
            match api::delete_inbox(id).await {
                Ok(()) => {
                    updates.inboxes.update(|n| *n += 1);
                    on_done.run(());
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    view! {
        <form class="inbox-form" on:submit=save>
            <label>"Name"
                <input type="text" required prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
            </label>
            <label>"GitHub filter"
                <input type="text" class="mono" prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) />
            </label>
            <label class="checkbox">"Count in total"
                <input type="checkbox" prop:checked=move || count_in_total.get() on:change=move |ev| count_in_total.set(event_target_checked(&ev)) />
            </label>
            <p class="hint muted">"is:pr and repo: qualifiers for the configured repositories are added automatically. gr: qualifiers, described in the README, are evaluated by grenadine."</p>
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
            <div class="form-buttons">
                <button type="submit" class="primary">"Save"</button>
                <button type="button" on:click=move |_| on_done.run(())>"Cancel"</button>
                {id.map(|_| view! { <button type="button" class="danger" on:click=delete>"Delete"</button> })}
            </div>
        </form>
    }
}

pub(crate) fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

pub fn load_flag(key: &str, default: bool) -> bool {
    storage()
        .and_then(|s| s.get_item(key).ok().flatten())
        .map_or(default, |v| v == "1")
}

pub fn save_flag(key: &str, value: bool) {
    if let Some(s) = storage() {
        let _ = s.set_item(key, if value { "1" } else { "0" });
    }
}
