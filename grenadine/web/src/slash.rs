//! Suggestions for the command a prompt starts with: typing `/` first in
//! a prompt box lists the skills and commands whose names start with what
//! follows, most used first. Tab completes the highlighted one and cycles
//! through them when pressed again; Shift+Tab cycles back.

use std::collections::HashSet;

use grenadine_core::api::{AgentBackend, SlashCommand, command_char};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::global_settings::GlobalSettings;
use crate::inboxes::storage;

const KEY: &str = "hidden-commands";

/// The commands hidden until the setting is changed: Claude Code's own
/// that do nothing, or nothing useful, in a session Grenadine runs.
pub const DEFAULT_HIDDEN: &str = "clear exit login logout config resume vim terminal-setup ide \
    doctor help status theme memory permissions agents mcp hooks model add-dir";

/// The "Hidden commands" setting as typed, persisted in localStorage.
pub fn hidden_signal() -> RwSignal<String> {
    let hidden = RwSignal::new(
        storage()
            .and_then(|s| s.get_item(KEY).ok().flatten())
            .unwrap_or_else(|| DEFAULT_HIDDEN.to_owned()),
    );
    Effect::new(move |_| {
        if let Some(s) = storage() {
            hidden.with(|h| {
                let _ = s.set_item(KEY, h);
            });
        }
    });
    hidden
}

/// The names in the "Hidden commands" setting: separated by spaces or
/// commas, with or without their `/`.
pub fn parse_hidden(setting: &str) -> HashSet<String> {
    setting
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|n| n.trim_start_matches('/'))
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The command being typed: the first word of `text` without its `/`,
/// when `text` starts with one and the caret, `caret` characters in, is
/// still in that word. It may be empty, right after the `/`.
pub fn typed(text: &str, caret: usize) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    let end = rest.find(|c: char| !command_char(c)).unwrap_or(rest.len());
    if rest[end..]
        .chars()
        .next()
        .is_some_and(|c| !c.is_whitespace())
    {
        // Like `/tmp/x`: not a command.
        return None;
    }
    let word = &rest[..end];
    (1..=1 + word.chars().count())
        .contains(&caret)
        .then_some(word)
}

/// The names of `commands` that start with `prefix`, ignoring case, and
/// aren't hidden: the most used first, then alphabetically.
pub fn matches(commands: &[SlashCommand], prefix: &str, hidden: &HashSet<String>) -> Vec<String> {
    let prefix = prefix.to_lowercase();
    let mut found: Vec<&SlashCommand> = commands
        .iter()
        .filter(|c| c.name.to_lowercase().starts_with(&prefix) && !hidden.contains(&c.name))
        .collect();
    found.sort_by(|a, b| b.uses.cmp(&a.uses).then_with(|| a.name.cmp(&b.name)));
    found.into_iter().map(|c| c.name.clone()).collect()
}

/// `text` with its first word replaced by `/name` and a space, and where
/// the caret goes then, in characters: after the space.
pub fn complete(text: &str, name: &str) -> (String, usize) {
    let rest = text.strip_prefix('/').unwrap_or(text);
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let rest = &rest[end..];
    let head = format!("/{name} ");
    let caret = head.chars().count();
    // A space that was there already stands in for the one added.
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    (head + rest, caret)
}

/// Where a box's suggestions come from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A new session's prompt: the repository's commands for the backend.
    Repo { repo: String, backend: AgentBackend },
    /// A session's follow-up: the session's own.
    Session(String),
}

/// The suggestions for one prompt box; `SlashList` shows them, and the
/// box's handlers call `input`, `keydown`, `caret_moved` and `blur`.
#[derive(Clone, Copy)]
pub struct Suggest {
    textarea: NodeRef<leptos::html::Textarea>,
    text: RwSignal<String>,
    /// The commands last fetched, and for which source.
    commands: RwSignal<Option<(Source, Vec<SlashCommand>)>>,
    /// What `typed` says of the box: the command being typed.
    word: RwSignal<Option<String>>,
    /// The matches Tab cycles through, and which one the box holds.
    cycle: RwSignal<Option<(Vec<String>, usize)>>,
    /// Whether Escape closed the list; until the next input.
    dismissed: RwSignal<bool>,
    hidden: Memo<HashSet<String>>,
}

impl Suggest {
    pub fn new(
        textarea: NodeRef<leptos::html::Textarea>,
        text: RwSignal<String>,
        source: Signal<Source>,
    ) -> Suggest {
        let setting = expect_context::<GlobalSettings>().hidden_commands;
        let s = Suggest {
            textarea,
            text,
            commands: RwSignal::new(None),
            word: RwSignal::new(None),
            cycle: RwSignal::new(None),
            dismissed: RwSignal::new(false),
            hidden: Memo::new(move |_| setting.with(|h| parse_hidden(h))),
        };
        // Fetched afresh whenever the list opens, so that new skills and
        // counts show, and when the source changes while it's open.
        Effect::new(move |prev: Option<(bool, Source)>| {
            let open = s.word.with(Option::is_some);
            let current = source.get();
            if open && prev.as_ref() != Some(&(true, current.clone())) {
                let wanted = current.clone();
                spawn_local(async move {
                    let fetched = match &wanted {
                        Source::Repo { repo, backend } => api::slash_commands(repo, *backend).await,
                        Source::Session(id) => api::agent_slash_commands(id).await,
                    };
                    if let Ok(list) = fetched
                        && source.try_get_untracked().as_ref() == Some(&wanted)
                    {
                        s.commands.try_set(Some((wanted, list)));
                    }
                });
            }
            (open, current)
        });
        s
    }

    /// The names the list shows, and which is highlighted.
    fn shown(self) -> (Vec<String>, usize) {
        if let Some((names, i)) = self.cycle.get() {
            return (names, i);
        }
        if self.dismissed.get() {
            return (Vec::new(), 0);
        }
        let Some(word) = self.word.get() else {
            return (Vec::new(), 0);
        };
        let names = self.commands.with(|c| match c {
            Some((_, commands)) => self.hidden.with(|h| matches(commands, &word, h)),
            None => Vec::new(),
        });
        (names, 0)
    }

    /// Looks again at where the caret is.
    pub fn caret_moved(self) {
        let Some(t) = self.textarea.get_untracked() else {
            return;
        };
        let text = t.value();
        let caret = t.selection_start().ok().flatten().unwrap_or(0) as usize;
        // The caret counts UTF-16 units; `typed` counts characters.
        let caret = text
            .chars()
            .scan(0, |units, c| {
                *units += c.len_utf16();
                Some(*units)
            })
            .take_while(|u| *u <= caret)
            .count();
        let word = typed(&text, caret).map(str::to_owned);
        if self.word.get_untracked() != word {
            self.word.set(word);
        }
    }

    /// The box's text changed by typing.
    pub fn input(self) {
        self.dismissed.set(false);
        self.cycle.set(None);
        self.caret_moved();
    }

    pub fn blur(self) {
        self.cycle.set(None);
        self.word.set(None);
    }

    /// Handles a key pressed in the box; whether it was used up.
    pub fn keydown(self, ev: &web_sys::KeyboardEvent) -> bool {
        match ev.key().as_str() {
            // Pressed on their own, e.g. on the way to Shift+Tab.
            "Shift" | "Control" | "Alt" | "Meta" | "CapsLock" => false,
            "Tab" if !ev.ctrl_key() && !ev.alt_key() && !ev.meta_key() => {
                let (names, at) = self.shown_untracked();
                if names.is_empty() {
                    return false;
                }
                let n = names.len();
                let at = match (self.cycle.with_untracked(Option::is_some), ev.shift_key()) {
                    (true, false) => (at + 1) % n,
                    (true, true) => (at + n - 1) % n,
                    (false, false) => 0,
                    (false, true) => n - 1,
                };
                ev.prevent_default();
                self.pick(names, at);
                true
            }
            "Escape" if !self.shown_untracked().0.is_empty() => {
                ev.prevent_default();
                ev.stop_propagation();
                self.cycle.set(None);
                self.dismissed.set(true);
                true
            }
            _ => {
                self.cycle.set(None);
                false
            }
        }
    }

    fn shown_untracked(self) -> (Vec<String>, usize) {
        untrack(move || self.shown())
    }

    /// Puts `names[at]` in the box and keeps cycling through `names`.
    fn pick(self, names: Vec<String>, at: usize) {
        let (text, caret) = complete(&self.text.get_untracked(), &names[at]);
        let units: usize = text.chars().take(caret).map(char::len_utf16).sum();
        if let Some(t) = self.textarea.get_untracked() {
            t.set_value(&text);
            let _ = t.set_selection_range(units as u32, units as u32);
        }
        self.text.set(text);
        self.word.set(None);
        self.cycle.set(Some((names, at)));
    }
}

/// The list of `suggest`'s matches, under its box or, for a box at the
/// bottom of the page, `above` it; nothing without any.
#[component]
pub fn SlashList(suggest: Suggest, #[prop(optional)] above: bool) -> impl IntoView {
    move || {
        let (names, at) = suggest.shown();
        (!names.is_empty()).then(|| {
            let items = names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let names = names.clone();
                    view! {
                        <li
                            role="option"
                            aria-selected=(i == at).to_string()
                            class:selected=i == at
                            // Keeps the box focused, unlike a click.
                            on:mousedown=move |ev| {
                                ev.prevent_default();
                                suggest.pick(names.clone(), i);
                            }
                        >
                            {format!("/{name}")}
                        </li>
                    }
                })
                .collect_view();
            view! {
                <ul class="slash-suggest" class:above=above role="listbox" aria-label="Commands">
                    {items}
                </ul>
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, uses: u64) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            uses,
        }
    }

    #[test]
    fn the_word_being_typed() {
        assert_eq!(typed("/", 1), Some(""));
        assert_eq!(typed("/f", 2), Some("f"));
        assert_eq!(typed("/foo bar", 2), Some("foo"));
        assert_eq!(typed("/foo bar", 4), Some("foo"));
        // Past the space, or before the slash.
        assert_eq!(typed("/foo bar", 5), None);
        assert_eq!(typed("/foo", 0), None);
        assert_eq!(typed("foo", 1), None);
        assert_eq!(typed(" /foo", 2), None);
        assert_eq!(typed("/tmp/x", 3), None);
        assert_eq!(typed("/plug:a-b", 9), Some("plug:a-b"));
        assert_eq!(typed("/foo\nbar", 4), Some("foo"));
    }

    #[test]
    fn matches_most_used_then_alphabetically() {
        let commands = [cmd("foo", 0), cmd("baz", 0), cmd("bar", 0), cmd("bat", 3)];
        let none = HashSet::new();
        assert_eq!(matches(&commands, "f", &none), ["foo"]);
        assert_eq!(matches(&commands, "b", &none), ["bat", "bar", "baz"]);
        assert_eq!(matches(&commands, "B", &none), ["bat", "bar", "baz"]);
        assert_eq!(matches(&commands, "", &none), ["bat", "bar", "baz", "foo"]);
        assert!(matches(&commands, "x", &none).is_empty());
        let hidden = parse_hidden("/bat, baz");
        assert_eq!(matches(&commands, "b", &hidden), ["bar"]);
    }

    #[test]
    fn hidden_names() {
        let hidden = parse_hidden(" /clear,exit\n  model ,, ");
        let mut names: Vec<_> = hidden.into_iter().collect();
        names.sort();
        assert_eq!(names, ["clear", "exit", "model"]);
        assert!(parse_hidden(DEFAULT_HIDDEN).contains("clear"));
        assert!(parse_hidden("").is_empty());
    }

    #[test]
    fn completes_the_first_word() {
        assert_eq!(complete("/b", "bar"), ("/bar ".into(), 5));
        assert_eq!(complete("/bar ", "baz"), ("/baz ".into(), 5));
        assert_eq!(complete("/b fix it", "bar"), ("/bar fix it".into(), 5));
        assert_eq!(complete("/b\nfix it", "bar"), ("/bar \nfix it".into(), 5));
        assert_eq!(complete("/", "foo"), ("/foo ".into(), 5));
        assert_eq!(complete("/b", "plüg"), ("/plüg ".into(), 6));
    }
}
