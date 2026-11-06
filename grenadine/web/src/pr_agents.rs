//! The agents associated with the PR the page shows, as tabs in a
//! collapsible panel pinned to the bottom of the PR's sidebar, modelled
//! on the terminal panel. A tab is the session's full page, compact; a
//! "+" tab starts one locked to the PR.

use std::rc::Rc;

use grenadine_core::api::{AgentSession, PrKey};
use leptos::ev;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::agents::{AgentView, NewSession, status_class, status_label};
use crate::drafts::Drafts;
use crate::inboxes::{load_flag, save_flag, storage};
use crate::terminal::clamp_height;
use crate::{Updates, api};

/// Where whether the PR's panel is open is kept; each PR has its own.
fn open_key(key: &PrKey) -> String {
    format!("grenadine.pr-agents-open.{}#{}", key.repo, key.number)
}

/// Where the tab last selected on the PR's page is kept.
fn selected_key(key: &PrKey) -> String {
    format!("grenadine.pr-agents-selected.{}#{}", key.repo, key.number)
}

/// The panel's height in pixels, for all PRs.
const HEIGHT_KEY: &str = "grenadine.pr-agents-height";
const DEFAULT_HEIGHT: f64 = 320.0;

/// The tab to show: `current` if it's still a tab, else `stored` if it
/// is, else the first.
fn pick(ids: &[String], current: Option<&str>, stored: Option<&str>) -> Option<String> {
    let has = |id: Option<&str>| id.is_some_and(|i| ids.iter().any(|x| x == i));
    if has(current) {
        current.map(str::to_owned)
    } else if has(stored) {
        stored.map(str::to_owned)
    } else {
        ids.first().cloned()
    }
}

/// What a session's tab is called.
fn label(s: &AgentSession) -> String {
    s.summary
        .clone()
        .or_else(|| Some(s.title.clone()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "New session".into())
}

/// The PR's sessions as tabs in a panel that can be collapsed to its tab
/// bar and dragged taller.
#[component]
pub fn PrAgentPanel(key: PrKey) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let drafts = expect_context::<Drafts>();
    let open_key = open_key(&key);
    let open = RwSignal::new(load_flag(&open_key, false));
    Effect::new(move |_| save_flag(&open_key, open.get()));
    let stored_height = storage()
        .and_then(|s| s.get_item(HEIGHT_KEY).ok().flatten())
        .and_then(|h| h.parse::<f64>().ok());
    let height = RwSignal::new(stored_height.unwrap_or(DEFAULT_HEIGHT));
    let panel = NodeRef::<leptos::html::Section>::new();
    // Without a dragged height the panel takes a share of the sidebar.
    panel.on_load(move |_| {
        if stored_height.is_none()
            && let Some(a) = document().query_selector(".pr-sidebar").ok().flatten()
        {
            let available = f64::from(a.client_height());
            height.set(clamp_height(available * 0.55, available));
        }
    });
    Effect::new(move |_| {
        if let Some(s) = storage() {
            let _ = s.set_item(HEIGHT_KEY, &height.get().to_string());
        }
    });
    let listed = {
        let key = key.clone();
        LocalResource::new(move || {
            updates.agents.track();
            let key = key.clone();
            async move { api::pr_agents(&key).await }
        })
    };
    let sessions = RwSignal::new(Vec::<AgentSession>::new());
    // The new-session draft the "+" tab shows, if it's open.
    let draft = RwSignal::new(None::<String>);
    let selected = RwSignal::new(None::<String>);
    let selected_key = selected_key(&key);
    let stored = storage().and_then(|s| s.get_item(&selected_key).ok().flatten());
    Effect::new(move |_| {
        if let (Some(t), Some(s)) = (selected.get(), storage()) {
            let _ = s.set_item(&selected_key, &t);
        }
    });
    let error = RwSignal::new(None::<String>);

    // The strip scrolls when the tabs overflow it; the tab just
    // selected may be out of view.
    Effect::new(move |_| {
        if selected.get().is_none() {
            return;
        }
        sessions.get();
        let Some(tab) = document()
            .query_selector(".pr-agents-panel .pr-agents-tab.selected")
            .ok()
            .flatten()
        else {
            return;
        };
        let Some(strip) = tab.parent_element() else {
            return;
        };
        let Ok(tab) = tab.dyn_into::<web_sys::HtmlElement>() else {
            return;
        };
        let (left, right) = (tab.offset_left(), tab.offset_left() + tab.offset_width());
        let scrolled = strip.scroll_left();
        if left < scrolled {
            strip.set_scroll_left(left);
        } else if right > scrolled + strip.client_width() {
            strip.set_scroll_left(right - strip.client_width());
        }
    });

    // A draft already being written for this PR gets a tab; it isn't
    // selected for it.
    if let Some(id) = drafts.find(&key.repo, key.number)
        && drafts.with(&id, |d| !d.is_blank()).unwrap_or(false)
    {
        draft.set(Some(id));
    }

    // Opens the draft's tab, making or reusing the PR's draft.
    let open_draft = Rc::new({
        let key = key.clone();
        move || {
            let id = drafts
                .find(&key.repo, key.number)
                .unwrap_or_else(|| drafts.create(Some(key.repo.clone()), Some(key.number)));
            draft.set(Some(id.clone()));
            selected.set(Some(id));
            open.set(true);
        }
    });

    let open_draft_fx = open_draft.clone();
    Effect::new(move |_| match listed.get() {
        Some(Ok(list)) => {
            // A session that started from the draft's tab takes its
            // place.
            if let Some(d) = draft.get_untracked()
                && let Some(id) = list
                    .iter()
                    .find(|s| s.draft.as_deref() == Some(d.as_str()))
                    .map(|s| s.id.clone())
            {
                draft.set(None);
                selected.set(Some(id));
            }
            let mut ids: Vec<String> = list.iter().map(|s| s.id.clone()).collect();
            let empty = list.is_empty();
            sessions.set(list);
            if let Some(d) = draft.get_untracked() {
                ids.push(d);
            }
            selected.set(pick(
                &ids,
                selected.get_untracked().as_deref(),
                stored.as_deref(),
            ));
            // With nothing to talk to, expanding the panel shows the
            // form rather than nothing.
            if empty && open.get_untracked() && draft.get_untracked().is_none() {
                open_draft_fx();
            }
        }
        Some(Err(e)) => error.set(Some(e)),
        None => {}
    });

    let toggle = {
        let open_draft = open_draft.clone();
        move |_| {
            open.update(|o| *o = !*o);
            if open.get_untracked()
                && sessions.with_untracked(Vec::is_empty)
                && draft.get_untracked().is_none()
            {
                open_draft();
            }
        }
    };

    let drag = StoredValue::new_local(None::<(WindowListenerHandle, WindowListenerHandle)>);
    let start_drag = move |e: web_sys::MouseEvent| {
        e.prevent_default();
        let (y, h) = (e.client_y() as f64, height.get_untracked());
        // The sidebar's height bounds the panel's.
        let available = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|t| t.closest(".pr-sidebar").ok().flatten())
            .map(|a| f64::from(a.client_height()))
            .unwrap_or(1000.0);
        let on_move = window_event_listener(ev::mousemove, move |e| {
            height.set(clamp_height(h + y - e.client_y() as f64, available));
        });
        let on_up = window_event_listener(ev::mouseup, move |_| {
            drag.update_value(|d| {
                if let Some((a, b)) = d.take() {
                    a.remove();
                    b.remove();
                }
            });
        });
        drag.set_value(Some((on_move, on_up)));
    };

    view! {
        <section class="pr-agents-panel" class:open=open node_ref=panel>
            {move || open.get().then(|| view! {
                <div class="pr-agents-resize" title="Drag to resize" on:mousedown=start_drag></div>
            })}
            <div class="pr-agents-bar">
                <button
                    class="pr-agents-toggle"
                    aria-expanded=move || open.get().to_string()
                    title="Agents on this PR"
                    on:click=toggle
                >
                    <span class="pr-agents-chevron" aria-hidden="true">{move || if open.get() { "▾" } else { "▸" }}</span>
                    "Agents"
                </button>
                <div class="pr-agents-tabs" role="tablist">
                    <For
                        each=move || sessions.get()
                        key=|s| s.id.clone()
                        children=move |s| {
                            let id = s.id.clone();
                            // The session the tab last listed, refetched
                            // for its status and label.
                            let current = Signal::derive(move || {
                                sessions
                                    .with(|ss| ss.iter().find(|x| x.id == s.id).cloned())
                                    .unwrap_or_else(|| s.clone())
                            });
                            let is_selected = move |id: &String| {
                                selected.get().as_deref() == Some(id.as_str())
                            };
                            let click_id = id.clone();
                            view! {
                                <span
                                    class="pr-agents-tab"
                                    class:selected={
                                        let id = id.clone();
                                        move || is_selected(&id)
                                    }
                                >
                                    <button
                                        class="pr-agents-tab-name"
                                        role="tab"
                                        aria-selected={
                                            let id = id.clone();
                                            move || is_selected(&id).to_string()
                                        }
                                        title=move || {
                                            let s = current.get();
                                            format!("{} — {}", label(&s), status_label(s.status))
                                        }
                                        on:click=move |_| {
                                            selected.set(Some(click_id.clone()));
                                            open.set(true);
                                        }
                                    >
                                        {move || {
                                            let s = current.get();
                                            let text = label(&s);
                                            let title = text.clone();
                                            view! {
                                                <span class=status_class(s.status) aria-hidden="true"></span>
                                                <span class="pr-agents-tab-label" title=title>{text}</span>
                                            }
                                        }}
                                    </button>
                                </span>
                            }
                        }
                    />
                    {move || draft.get().map(|d| {
                        let (sel_id, aria_id, click_id) = (d.clone(), d.clone(), d);
                        view! {
                            <span
                                class="pr-agents-tab"
                                class:selected=move || selected.get().as_deref() == Some(sel_id.as_str())
                            >
                                <button
                                    class="pr-agents-tab-name draft"
                                    role="tab"
                                    aria-selected=move || (selected.get().as_deref() == Some(aria_id.as_str())).to_string()
                                    title="New session"
                                    on:click=move |_| {
                                        selected.set(Some(click_id.clone()));
                                        open.set(true);
                                    }
                                >
                                    <span class="pr-agents-tab-label">"New session"</span>
                                </button>
                            </span>
                        }
                    })}
                </div>
                <button
                    class="pr-agents-new"
                    aria-label="New session"
                    title="New session on this PR"
                    on:click=move |_| {
                        if let Some(d) = draft.get_untracked() {
                            selected.set(Some(d));
                            open.set(true);
                        } else {
                            open_draft();
                        }
                    }
                >"+"</button>
                {move || error.get().map(|e| view! { <span class="error">{e}</span> })}
            </div>
            <div
                class="pr-agents-body"
                hidden=move || !open.get()
                style:height=move || format!("{}px", height.get())
            >
                {move || {
                    let sel = selected.get();
                    if let Some(d) = draft.get()
                        && sel.as_deref() == Some(d.as_str())
                    {
                        view! { <NewSession draft=d pr=key.clone() /> }.into_any()
                    } else {
                        sel.map(|id| view! { <AgentView id=id compact=true /> }.into_any())
                            .unwrap_or_else(|| ().into_any())
                    }
                }}
            </div>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_current_then_stored_then_first_tab() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ids = ids(&["a", "b"]);
        assert_eq!(pick(&ids, Some("b"), Some("a")), Some("b".into()));
        assert_eq!(pick(&ids, Some("x"), Some("a")), Some("a".into()));
        assert_eq!(pick(&ids, Some("x"), Some("y")), Some("a".into()));
        assert_eq!(pick(&ids, None, Some("b")), Some("b".into()));
        assert_eq!(pick(&ids, None, None), Some("a".into()));
        assert_eq!(pick(&[], Some("a"), Some("b")), None);
    }
}
