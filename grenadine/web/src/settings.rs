//! A repository's settings, edited in a dialog over the agents' pages.

use grenadine_core::api::{AgentBackend, AutoReview, PermissionMode, RepoSettings};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::agents::{DEFAULT_MODEL, backends, listed_models, model_options};
use crate::{Updates, api};

#[component]
fn GearIcon() -> impl IntoView {
    view! {
        <svg
            class="gear"
            viewBox="0 0 16 16"
            width="14"
            height="14"
            fill="none"
            stroke="currentColor"
            stroke-width="1.4"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
        >
            <circle cx="8" cy="8" r="2.2" />
            <path d="M8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2M3.4 3.4l1.4 1.4M11.2 11.2l1.4 1.4M3.4 12.6l1.4-1.4M11.2 4.8l1.4-1.4" />
        </svg>
    }
}

/// The gear next to a repository's name, which opens its settings in
/// `open`.
#[component]
pub fn SettingsButton(repo: String, open: RwSignal<Option<String>>) -> impl IntoView {
    let label = format!("Settings for {repo}");
    view! {
        <button
            class="icon repo-settings-button"
            title=label.clone()
            aria-label=label
            on:click=move |_| open.set(Some(repo.clone()))
        >
            <GearIcon />
        </button>
    }
}

/// The settings of the repository in `open`, if any. It lives outside the
/// sidebar's sections, which are drawn again whenever the sessions change.
#[component]
pub fn Settings(open: RwSignal<Option<String>>) -> impl IntoView {
    move || {
        open.get()
            .map(|repo| view! { <SettingsDialog repo=repo on_close=move || open.set(None) /> })
    }
}

/// The repository's settings in a modal dialog: Escape, the close button,
/// Cancel or a click beside it close it without saving.
#[component]
fn SettingsDialog(
    repo: String,
    on_close: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let repo = StoredValue::new(repo);
    let loaded = LocalResource::new(move || {
        updates.repo_settings.track();
        let repo = repo.get_value();
        async move { api::repo_settings(&repo).await }
    });
    // The settings as last loaded, and as edited since.
    let saved = RwSignal::new(None::<RepoSettings>);
    let draft = RwSignal::new(None::<RepoSettings>);
    Effect::new(move || {
        if let Some(Ok(s)) = loaded.get() {
            // An edit in progress isn't thrown away by another page saving.
            if draft.with_untracked(|d| d.is_none() || *d == saved.get_untracked()) {
                draft.set(Some(s.clone()));
            }
            saved.set(Some(s));
        }
    });
    let error = RwSignal::new(None::<String>);
    let saving = RwSignal::new(false);
    let changed = move || draft.with(|d| d.is_some() && *d != saved.get());
    let save = move |_| {
        let Some(settings) = draft.get_untracked() else {
            return;
        };
        saving.set(true);
        error.set(None);
        spawn_local(async move {
            let result = api::set_repo_settings(&repo.get_value(), &settings).await;
            saving.set(false);
            match result {
                Ok(()) => on_close(),
                Err(e) => error.set(Some(e)),
            }
        });
    };

    let dialog = NodeRef::<leptos::html::Dialog>::new();
    Effect::new(move || {
        if let Some(d) = dialog.get()
            && !d.open()
        {
            let _ = d.show_modal();
        }
    });
    let title = format!("{} settings", repo.get_value());
    let label = title.clone();
    view! {
        <dialog
            class="repo-settings"
            node_ref=dialog
            aria-label=label
            on:close=move |_| on_close()
            // The dialog has no padding of its own, so a click on it
            // rather than on its content is on the backdrop.
            on:click=move |ev| {
                if ev.target() == ev.current_target() {
                    on_close();
                }
            }
        >
            <div class="repo-settings-content">
                <header>
                    <h2>{title}</h2>
                    <button class="icon" title="Close" aria-label="Close" on:click=move |_| on_close()>
                        "×"
                    </button>
                </header>
                <div class="repo-settings-body">
                    {move || match (loaded.get(), draft.with(Option::is_some)) {
                        (Some(Err(e)), _) => view! { <p class="error">{e}</p> }.into_any(),
                        (_, true) => view! {
                            <ReviewFields draft=draft />
                            <NewSessionFields draft=draft />
                        }
                        .into_any(),
                        _ => view! { <p class="muted">"Loading…"</p> }.into_any(),
                    }}
                    {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                </div>
                <footer>
                    <button on:click=move |_| on_close()>"Cancel"</button>
                    <button
                        class="primary"
                        disabled=move || !changed() || saving.get()
                        on:click=save
                    >
                        "Save"
                    </button>
                </footer>
            </div>
        </dialog>
    }
}

fn mode_value(mode: AutoReview) -> &'static str {
    match mode {
        AutoReview::Off => "off",
        AutoReview::NonDraft => "non_draft",
        AutoReview::IncludingDrafts => "including_drafts",
    }
}

fn parse_mode(value: &str) -> AutoReview {
    match value {
        "off" => AutoReview::Off,
        "including_drafts" => AutoReview::IncludingDrafts,
        _ => AutoReview::NonDraft,
    }
}

/// The settings of the reviews of the viewer's own PRs, edited in `draft`,
/// which is loaded.
#[component]
fn ReviewFields(draft: RwSignal<Option<RepoSettings>>) -> impl IntoView {
    let get =
        move |f: fn(&RepoSettings) -> String| draft.with(|d| d.as_ref().map(f).unwrap_or_default());
    let edit = move |f: Box<dyn FnOnce(&mut RepoSettings)>| {
        draft.update(|d| {
            if let Some(d) = d {
                f(d);
            }
        })
    };
    let off = move || draft.with(|d| d.as_ref().is_some_and(|d| d.auto_review == AutoReview::Off));
    let backend = Signal::derive(move || {
        draft.with(|d| d.as_ref().map(|d| d.review_backend).unwrap_or_default())
    });
    let models = listed_models(backend);
    // Models that couldn't be listed when the page loaded may be now.
    if models.get_untracked().is_some_and(|m| m.is_empty()) {
        backends().refetch();
    }
    let picked = Memo::new(move |_| {
        let backend = backend.get();
        draft.with(|d| {
            d.as_ref()
                .and_then(|d| d.review_models.get(&backend).cloned())
                .unwrap_or_else(|| DEFAULT_MODEL.into())
        })
    });
    let options = Memo::new(move |_| {
        model_options(
            models.get().unwrap_or_default(),
            backend.get(),
            &picked.get(),
        )
    });
    view! {
        <fieldset class="repo-settings-fields">
            <legend>"Auto-reviews"</legend>
            <p class="hint muted">
                "An unattended agent session reviews each pull request you open, posts its "
                "review on the pull request and is then deleted. The reviews are in the "
                "Auto-reviews tab."
            </p>
            <label>"Review my pull requests"
                <select
                    prop:value=move || get(|d| mode_value(d.auto_review).to_owned())
                    on:change=move |ev| {
                        let mode = parse_mode(&event_target_value(&ev));
                        edit(Box::new(move |d| d.auto_review = mode));
                    }
                >
                    <option value="off">"Never"</option>
                    <option value="non_draft">"When opened, once they're not drafts"</option>
                    <option value="including_drafts">"When opened, drafts too"</option>
                </select>
            </label>
            <label>"Review with"
                <select
                    prop:disabled=off
                    prop:value=move || get(|d| d.review_backend.as_str().to_owned())
                    on:change=move |ev| {
                        let backend = AgentBackend::parse(&event_target_value(&ev))
                            .unwrap_or_default();
                        edit(Box::new(move |d| d.review_backend = backend));
                    }
                >
                    {AgentBackend::ALL.into_iter().map(|b| view! {
                        <option value=b.as_str()>{b.label()}</option>
                    }).collect_view()}
                </select>
            </label>
            <label>"Review model"
                <select
                    prop:disabled=off
                    prop:value=move || picked.get()
                    on:change=move |ev| {
                        let value = event_target_value(&ev);
                        let backend = backend.get_untracked();
                        edit(Box::new(move |d| {
                            if value == DEFAULT_MODEL {
                                d.review_models.remove(&backend);
                            } else {
                                d.review_models.insert(backend, value);
                            }
                        }));
                    }
                >
                    {move || options.get().into_iter().map(|m| {
                        let value = m.value.clone();
                        let selected = move || picked.with(|p| *p == value);
                        view! {
                            <option value=m.value title=m.description selected=selected>{m.display_name}</option>
                        }
                    }).collect_view()}
                </select>
            </label>
            <p class="hint muted">"Reviews with Devin run in its Smart mode."</p>
            <label class="checkbox">
                <input
                    type="checkbox"
                    prop:disabled=off
                    prop:checked=move || draft.with(|d| d.as_ref().is_some_and(|d| d.rereview_on_push))
                    on:change=move |ev| {
                        let on = event_target_checked(&ev);
                        edit(Box::new(move |d| d.rereview_on_push = on));
                    }
                />
                "Review again after new pushes"
            </label>
            <label>"Review skill"
                <input
                    type="text"
                    class="mono"
                    placeholder="None"
                    prop:disabled=off
                    prop:value=move || get(|d| d.review_skill.clone().unwrap_or_default())
                    on:input=move |ev| {
                        let skill = event_target_value(&ev);
                        edit(Box::new(move |d| {
                            d.review_skill = (!skill.trim().is_empty()).then_some(skill)
                        }));
                    }
                />
            </label>
            <p class="hint muted">
                "The skill the review runs, e.g. code-review, given the pull request. Without "
                "one, the agent reviews it with a plain prompt."
            </p>
        </fieldset>
    }
}

/// The settings the New session form starts with, edited in `draft`,
/// which is loaded.
#[component]
fn NewSessionFields(draft: RwSignal<Option<RepoSettings>>) -> impl IntoView {
    let get =
        move |f: fn(&RepoSettings) -> String| draft.with(|d| d.as_ref().map(f).unwrap_or_default());
    let edit = move |f: Box<dyn FnOnce(&mut RepoSettings)>| {
        draft.update(|d| {
            if let Some(d) = d {
                f(d);
            }
        })
    };
    view! {
        <fieldset class="repo-settings-fields">
            <legend>"New sessions"</legend>
            {AgentBackend::ALL.into_iter().map(|b| {
                let label = format!("{} permissions", b.agent_name());
                view! {
                    <label>{label}
                        <select
                            prop:value=move || draft.with(|d| {
                                d.as_ref()
                                    .and_then(|d| d.session_modes.get(&b))
                                    .map(|m| m.as_str().to_owned())
                                    .unwrap_or_else(|| PermissionMode::plan().as_str().to_owned())
                            })
                            on:change=move |ev| {
                                let mode = PermissionMode::new(event_target_value(&ev));
                                edit(Box::new(move |d| {
                                    d.session_modes.insert(b, mode);
                                }));
                            }
                        >
                            {b.modes().iter().map(|&(value, label)| view! {
                                <option value=value>{label}</option>
                            }).collect_view()}
                        </select>
                    </label>
                }
            }).collect_view()}
            <label>"Default skill"
                <input
                    type="text"
                    class="mono"
                    placeholder="None"
                    prop:value=move || get(|d| d.session_skill.clone().unwrap_or_default())
                    on:input=move |ev| {
                        let skill = event_target_value(&ev);
                        edit(Box::new(move |d| {
                            d.session_skill = (!skill.trim().is_empty()).then_some(skill)
                        }));
                    }
                />
            </label>
            <p class="hint muted">
                "The skill a new session's prompt runs, e.g. foo makes \"bar\" start as "
                "\"/foo bar\". A prompt that starts with its own /skill runs that instead."
            </p>
        </fieldset>
    }
}
