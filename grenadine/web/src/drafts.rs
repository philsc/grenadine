//! The new sessions still being written: each form is a draft of its own,
//! kept in local storage so that it survives going to another page and
//! back, or a reload, until its session starts or it's discarded.

use grenadine_core::api::PermissionMode;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use crate::inboxes::storage;

const KEY: &str = "grenadine.new-session-drafts";

/// What a new session's form holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Draft {
    pub id: String,
    /// Empty until the repositories are known and the form picks one.
    pub repo: String,
    /// The PR to start from, as typed.
    pub pr: String,
    pub prompt: String,
    pub mode: PermissionMode,
    pub model: String,
    /// When the form was opened, in milliseconds since the epoch.
    pub created_at: f64,
}

impl Draft {
    pub fn is_blank(&self) -> bool {
        self.prompt.trim().is_empty()
    }
}

pub fn hash(id: &str) -> String {
    format!("#/agents/draft/{id}")
}

/// The drafts worth keeping when the page loads: the blank ones were left
/// behind by forms that were never written in, except the one that's open.
fn prune(list: Vec<Draft>, open: Option<&str>) -> Vec<Draft> {
    list.into_iter()
        .filter(|d| !d.is_blank() || Some(d.id.as_str()) == open)
        .collect()
}

/// The drafts the sidebar lists, as `(repo, id)`, newest first: the ones
/// written in and the open one, once they have a repository.
pub fn listed(list: &[Draft], open: Option<&str>) -> Vec<(String, String)> {
    let mut shown: Vec<&Draft> = list
        .iter()
        .filter(|d| !d.repo.is_empty() && (!d.is_blank() || Some(d.id.as_str()) == open))
        .collect();
    shown.sort_by(|a, b| b.created_at.total_cmp(&a.created_at));
    shown
        .into_iter()
        .map(|d| (d.repo.clone(), d.id.clone()))
        .collect()
}

/// All the drafts, saved whenever one changes.
#[derive(Clone, Copy)]
pub struct Drafts(RwSignal<Vec<Draft>>);

impl Drafts {
    /// Loads the drafts left from before, with `open` the one whose form is
    /// shown.
    pub fn load(open: Option<&str>) -> Drafts {
        let list = storage()
            .and_then(|s| s.get_item(KEY).ok().flatten())
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        let drafts = RwSignal::new(prune(list, open));
        Effect::new(move |_| {
            let Some(s) = storage() else { return };
            drafts.with(|list| {
                if let Ok(v) = serde_json::to_string(list) {
                    let _ = s.set_item(KEY, &v);
                }
            });
        });
        Drafts(drafts)
    }

    /// Starts a draft in `repo` and on `pr`, if given, and returns its id.
    pub fn create(self, repo: Option<String>, pr: Option<u64>) -> String {
        let now = js_sys::Date::now();
        let id = format!(
            "{:x}{:08x}",
            now as u64,
            (js_sys::Math::random() * f64::from(u32::MAX)) as u32
        );
        self.0.update(|list| {
            list.push(Draft {
                id: id.clone(),
                repo: repo.unwrap_or_default(),
                pr: pr.map(|n| n.to_string()).unwrap_or_default(),
                prompt: String::new(),
                // New sessions start out planning, whatever the others go
                // on with.
                mode: PermissionMode::Plan,
                model: crate::agents::last_model(),
                created_at: now,
            })
        });
        id
    }

    /// The draft `id`, or a blank one under that id if there's none, e.g.
    /// when going back to a draft whose session started.
    pub fn open(self, id: &str) -> Draft {
        if let Some(d) = self
            .0
            .with_untracked(|l| l.iter().find(|d| d.id == id).cloned())
        {
            return d;
        }
        let d = Draft {
            id: id.to_owned(),
            repo: String::new(),
            pr: String::new(),
            prompt: String::new(),
            mode: PermissionMode::Plan,
            model: crate::agents::last_model(),
            created_at: js_sys::Date::now(),
        };
        self.0.update(|list| list.push(d.clone()));
        d
    }

    /// What `f` makes of the draft `id`, if there is one.
    pub fn with<T>(self, id: &str, f: impl FnOnce(&Draft) -> T) -> Option<T> {
        self.0.with(|l| l.iter().find(|d| d.id == id).map(f))
    }

    pub fn update(self, id: &str, f: impl FnOnce(&mut Draft)) {
        self.0.update(|l| {
            if let Some(d) = l.iter_mut().find(|d| d.id == id) {
                f(d);
            }
        });
    }

    pub fn remove(self, id: &str) {
        self.0.update(|l| l.retain(|d| d.id != id));
    }

    /// Drops the draft `id` if nothing was written in it.
    pub fn remove_if_blank(self, id: &str) {
        if self.with(id, Draft::is_blank).unwrap_or(false) {
            self.remove(id);
        }
    }

    /// See `listed`.
    pub fn listed(self, open: Option<&str>) -> Vec<(String, String)> {
        self.0.with(|l| listed(l, open))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(id: &str, repo: &str, prompt: &str, created_at: f64) -> Draft {
        Draft {
            id: id.into(),
            repo: repo.into(),
            pr: String::new(),
            prompt: prompt.into(),
            mode: PermissionMode::Plan,
            model: "default".into(),
            created_at,
        }
    }

    #[test]
    fn round_trips() {
        let d = draft("a", "o/n", "do it", 1.0);
        let v = serde_json::to_string(std::slice::from_ref(&d)).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Draft>>(&v).unwrap(), [d]);
    }

    #[test]
    fn prunes_blank_drafts_but_the_open_one() {
        let list = vec![
            draft("a", "o/n", "do it", 1.0),
            draft("b", "o/n", "  \n", 2.0),
            draft("c", "o/n", "", 3.0),
        ];
        let ids: Vec<String> = prune(list, Some("c")).into_iter().map(|d| d.id).collect();
        assert_eq!(ids, ["a", "c"]);
    }

    #[test]
    fn lists_newest_first() {
        let list = vec![
            draft("a", "o/n", "one", 1.0),
            draft("b", "o/m", "two", 3.0),
            draft("blank", "o/n", "", 4.0),
            draft("open", "o/n", "", 2.0),
            draft("norepo", "", "three", 5.0),
        ];
        assert_eq!(
            listed(&list, Some("open")),
            [
                ("o/m".to_owned(), "b".to_owned()),
                ("o/n".to_owned(), "open".to_owned()),
                ("o/n".to_owned(), "a".to_owned()),
            ]
        );
    }
}
