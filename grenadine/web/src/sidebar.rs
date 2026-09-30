//! The list of inboxes, each with the PRs its filter matches. Inboxes can be
//! added, edited, reordered and deleted in place.

use grenadine_core::api::{Inbox, InboxEdit, InboxWithPrs, PrKey};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::{Updates, api, hash_for};

#[component]
pub fn Sidebar(selected: RwSignal<Option<PrKey>>) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let inboxes = LocalResource::new(move || {
        updates.inboxes.track();
        api::inboxes()
    });
    let adding = RwSignal::new(false);

    view! {
        <nav class="sidebar">
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
                            view! { <InboxSection inbox=inbox selected=selected prev=prev next=next /> }
                        })
                        .collect_view()
                        .into_any()
                }
            }}
            {move || if adding.get() {
                view! {
                    <InboxForm
                        initial=InboxEdit { name: String::new(), filter: "is:open ".into(), position: None }
                        on_done=Callback::new(move |_| adding.set(false))
                        id=None
                    />
                }.into_any()
            } else {
                view! {
                    <button class="add-inbox" on:click=move |_| adding.set(true)>"+ Add inbox"</button>
                }.into_any()
            }}
        </nav>
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
        },
    )
    .await?;
    api::update_inbox(
        b.id,
        &InboxEdit {
            name: b.name,
            filter: b.filter,
            position: Some(pb),
        },
    )
    .await
}

#[component]
fn InboxSection(
    inbox: InboxWithPrs,
    selected: RwSignal<Option<PrKey>>,
    /// The inboxes above and below, to swap places with.
    prev: Option<Inbox>,
    next: Option<Inbox>,
) -> impl IntoView {
    let collapsed_key = format!("inbox-collapsed-{}", inbox.inbox.id);
    let collapsed = RwSignal::new(load_flag(&collapsed_key));
    let editing = RwSignal::new(false);
    let InboxWithPrs { inbox, prs, error } = inbox;
    let count = prs.len();
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
                        initial=InboxEdit { name: i.name.clone(), filter: i.filter.clone(), position: Some(i.position) }
                        on_done=Callback::new(move |_| editing.set(false))
                        id=Some(i.id)
                    />
                }
            })}
            {error.map(|e| view! { <p class="error inbox-error">{e}</p> })}
            <ul class="pr-list" class:hidden=move || collapsed.get()>
                {if prs.is_empty() {
                    view! { <li class="muted none">"No PRs"</li> }.into_any()
                } else {
                    prs.into_iter().map(|pr| {
                        let key = pr.key.clone();
                        let is_selected = {
                            let key = key.clone();
                            move || selected.with(|s| s.as_ref() == Some(&key))
                        };
                        view! {
                            <li>
                                <a
                                    class="pr-item"
                                    class:selected=is_selected
                                    href=hash_for(&key)
                                >
                                    <span class="pr-title">
                                        {pr.is_draft.then(|| view! { <span class="badge">"draft"</span> })}
                                        {pr.title.clone()}
                                    </span>
                                    <span class="pr-meta muted">
                                        {format!("{}#{} · {} · {} versions", pr.key.repo, pr.key.number, pr.author, pr.version_count)}
                                    </span>
                                </a>
                            </li>
                        }
                    }).collect_view().into_any()
                }}
            </ul>
        </section>
    }
}

#[component]
fn InboxForm(initial: InboxEdit, on_done: Callback<()>, id: Option<i64>) -> impl IntoView {
    let name = RwSignal::new(initial.name);
    let filter = RwSignal::new(initial.filter);
    let error = RwSignal::new(None::<String>);
    let position = initial.position;
    let updates = expect_context::<Updates>();

    let save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let edit = InboxEdit {
            name: name.get_untracked(),
            filter: filter.get_untracked(),
            position,
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
            <p class="hint muted">"is:pr and repo: qualifiers for the configured repositories are added automatically."</p>
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
            <div class="form-buttons">
                <button type="submit" class="primary">"Save"</button>
                <button type="button" on:click=move |_| on_done.run(())>"Cancel"</button>
                {id.map(|_| view! { <button type="button" class="danger" on:click=delete>"Delete"</button> })}
            </div>
        </form>
    }
}

fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

pub fn load_flag(key: &str) -> bool {
    storage()
        .and_then(|s| s.get_item(key).ok().flatten())
        .as_deref()
        == Some("1")
}

pub fn save_flag(key: &str, value: bool) {
    if let Some(s) = storage() {
        let _ = s.set_item(key, if value { "1" } else { "0" });
    }
}
