//! The color theme: Auto follows the OS's preferred scheme, Light and Dark
//! pin it. The effective theme lives on `<html data-theme>` and all themed
//! CSS keys off that attribute.

use leptos::prelude::*;
use wasm_bindgen::prelude::*;

use crate::inboxes::storage;

const KEY: &str = "theme";
const DARK_QUERY: &str = "(prefers-color-scheme: dark)";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    Auto,
    Light,
    Dark,
}

impl Theme {
    pub fn parse(s: &str) -> Theme {
        match s {
            "light" => Theme::Light,
            "dark" => Theme::Dark,
            _ => Theme::Auto,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Auto => "auto",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }
}

fn effective(theme: Theme, prefers_dark: bool) -> &'static str {
    match theme {
        Theme::Light => "light",
        Theme::Dark => "dark",
        Theme::Auto => {
            if prefers_dark {
                "dark"
            } else {
                "light"
            }
        }
    }
}

fn prefers_dark() -> bool {
    window()
        .match_media(DARK_QUERY)
        .ok()
        .flatten()
        .is_some_and(|m| m.matches())
}

fn apply(theme: Theme) {
    if let Some(el) = document().document_element() {
        let _ = el.set_attribute("data-theme", effective(theme, prefers_dark()));
    }
}

/// The selected theme, persisted in localStorage. Changes save it and
/// update `data-theme` on `<html>`; while Auto is selected the attribute
/// also tracks the OS's preferred scheme.
pub fn signal() -> RwSignal<Theme> {
    let theme = RwSignal::new(
        storage()
            .and_then(|s| s.get_item(KEY).ok().flatten())
            .map_or(Theme::Auto, |v| Theme::parse(&v)),
    );
    Effect::new(move |_| {
        let theme = theme.get();
        if let Some(s) = storage() {
            let _ = s.set_item(KEY, theme.as_str());
        }
        apply(theme);
    });
    if let Ok(Some(mql)) = window().match_media(DARK_QUERY) {
        // Lives for the app's lifetime, so the closure can be leaked.
        let on_change = Closure::<dyn FnMut()>::new(move || {
            if theme.get_untracked() == Theme::Auto {
                apply(Theme::Auto);
            }
        });
        let _ = mql.add_event_listener_with_callback("change", on_change.as_ref().unchecked_ref());
        on_change.forget();
    }
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips() {
        for theme in [Theme::Auto, Theme::Light, Theme::Dark] {
            assert_eq!(Theme::parse(theme.as_str()), theme);
        }
    }

    #[test]
    fn parse_unknown_is_auto() {
        assert_eq!(Theme::parse(""), Theme::Auto);
        assert_eq!(Theme::parse("auto"), Theme::Auto);
        assert_eq!(Theme::parse("banana"), Theme::Auto);
        assert_eq!(Theme::parse("Light"), Theme::Auto);
    }

    #[test]
    fn effective_themes() {
        assert_eq!(effective(Theme::Light, true), "light");
        assert_eq!(effective(Theme::Light, false), "light");
        assert_eq!(effective(Theme::Dark, true), "dark");
        assert_eq!(effective(Theme::Dark, false), "dark");
        assert_eq!(effective(Theme::Auto, true), "dark");
        assert_eq!(effective(Theme::Auto, false), "light");
    }
}
