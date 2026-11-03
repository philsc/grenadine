//! A repository's settings, edited in a dialog over the agents' pages.

use grenadine_core::api::RepoSettings;
use leptos::prelude::*;
use leptos::task::spawn_local;

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
                    {move || match (loaded.get(), draft.get()) {
                        (Some(Err(e)), _) => view! { <p class="error">{e}</p> }.into_any(),
                        (_, Some(_)) => view! { <p class="muted">"No settings yet."</p> }.into_any(),
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
