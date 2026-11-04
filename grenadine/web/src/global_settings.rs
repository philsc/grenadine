//! The settings that apply everywhere rather than to one repository:
//! the theme and where PR links open. They live in the browser's local
//! storage, so they carry over between sessions on the same machine.

use leptos::prelude::*;

use crate::inboxes::{load_flag, save_flag};
use crate::settings::GearIcon;
use crate::theme::{self, Theme};

/// The global settings, provided once from `App`.
#[derive(Clone, Copy)]
pub(crate) struct GlobalSettings {
    pub(crate) theme: RwSignal<Theme>,
    /// Whether a plain click on a PR link opens it in grenadine.
    pub(crate) pr_links: RwSignal<bool>,
}

/// Loads the persisted settings into signals; writes to them apply at
/// once and are saved.
pub(crate) fn provide() -> GlobalSettings {
    let pr_links = RwSignal::new(load_flag("pr-links-in-app", true));
    Effect::new(move |_| save_flag("pr-links-in-app", pr_links.get()));
    GlobalSettings {
        theme: theme::signal(),
        pr_links,
    }
}

/// The gear at the top bar's right end, which opens the settings dialog.
#[component]
pub fn GlobalSettingsButton(open: RwSignal<bool>) -> impl IntoView {
    view! {
        <button
            class="icon global-settings-button"
            title="Settings"
            aria-label="Settings"
            on:click=move |_| open.set(true)
        >
            <GearIcon />
        </button>
    }
}

/// The global settings while `open`: a modal dialog that Escape, the
/// close button or a click beside it closes. Changes apply instantly.
#[component]
pub fn GlobalSettingsDialog(open: RwSignal<bool>) -> impl IntoView {
    move || open.get().then(|| view! { <Dialog open=open /> })
}

#[component]
fn Dialog(open: RwSignal<bool>) -> impl IntoView {
    let settings = expect_context::<GlobalSettings>();
    let close = move || open.set(false);

    let dialog = NodeRef::<leptos::html::Dialog>::new();
    Effect::new(move || {
        if let Some(d) = dialog.get()
            && !d.open()
        {
            let _ = d.show_modal();
        }
    });

    view! {
        <dialog
            class="settings-dialog"
            node_ref=dialog
            aria-label="Settings"
            on:close=move |_| close()
            // The dialog has no padding of its own, so a click on it
            // rather than on its content is on the backdrop.
            on:click=move |ev| {
                if ev.target() == ev.current_target() {
                    close();
                }
            }
        >
            <div class="settings-dialog-content">
                <header>
                    <h2>"Settings"</h2>
                    <button class="icon" title="Close" aria-label="Close" on:click=move |_| close()>
                        "×"
                    </button>
                </header>
                <div class="settings-dialog-body">
                    <fieldset class="settings-fields">
                        <label>"Theme"
                            <select
                                prop:value=move || settings.theme.get().as_str()
                                on:change=move |ev| {
                                    settings.theme.set(Theme::parse(&event_target_value(&ev)))
                                }
                            >
                                <option value="auto">"Auto"</option>
                                <option value="light">"Light"</option>
                                <option value="dark">"Dark"</option>
                            </select>
                        </label>
                        <label class="checkbox">
                            <input
                                type="checkbox"
                                prop:checked=move || settings.pr_links.get()
                                on:change=move |ev| {
                                    settings.pr_links.set(event_target_checked(&ev))
                                }
                            />
                            "Open PR links in grenadine"
                        </label>
                        <p class="hint muted">
                            "A plain click on a PR link opens it in grenadine and Shift+click "
                            "opens it on GitHub. Turned off, it's the other way round."
                        </p>
                    </fieldset>
                </div>
            </div>
        </dialog>
    }
}
