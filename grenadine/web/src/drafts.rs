//! The new sessions still being written: each form is a draft of its own,
//! kept in local storage so that it survives going to another page and
//! back, or a reload, until its session starts or it's discarded. The
//! draft's images are kept in IndexedDB (see `image_drafts`).

use grenadine_core::api::{AgentBackend, GENERAL, NewAgent, PermissionMode};
use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use crate::inboxes::storage;

const KEY: &str = "grenadine.new-session-drafts";

/// What a new session's form holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Draft {
    pub id: String,
    /// `GENERAL` until a repository is picked.
    pub repo: String,
    /// The PR to start from, as typed.
    pub pr: String,
    pub prompt: String,
    /// Whether images are kept for the draft (in IndexedDB); they make it
    /// worth keeping even without text.
    #[serde(default)]
    pub has_images: bool,
    /// Which agent backend runs the session's turns.
    #[serde(default)]
    pub backend: AgentBackend,
    pub mode: PermissionMode,
    /// Whether the user picked `mode`; until then the form's mode follows
    /// the repository's default for the backend.
    #[serde(default)]
    pub mode_picked: bool,
    pub model: String,
    /// When the form was opened, in milliseconds since the epoch.
    pub created_at: f64,
}

impl Draft {
    pub fn is_blank(&self) -> bool {
        self.prompt.trim().is_empty() && !self.has_images
    }
}

pub fn hash(id: &str) -> String {
    format!("#/agents/draft/{id}")
}

/// What the ids of the drafts that hold an agent's handoff proposal
/// start with.
const HANDOFF: &str = "handoff";

/// The id of the draft that holds the proposal of the handoff request
/// `tool`. Draft ids are alphanumeric.
pub fn handoff_id(tool: &str) -> String {
    let tool: String = tool.chars().filter(char::is_ascii_alphanumeric).collect();
    let tool = tool.strip_prefix(HANDOFF).unwrap_or(&tool);
    format!("{HANDOFF}{tool}")
}

/// Whether the draft `id` holds a handoff proposal, which shows in its
/// request's card rather than as a draft of its own.
fn is_handoff(id: &str) -> bool {
    id.starts_with(HANDOFF)
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
        .filter(|d| {
            !d.repo.is_empty()
                && !is_handoff(&d.id)
                && (!d.is_blank() || Some(d.id.as_str()) == open)
        })
        .collect();
    shown.sort_by(|a, b| b.created_at.total_cmp(&a.created_at));
    shown
        .into_iter()
        .map(|d| (d.repo.clone(), d.id.clone()))
        .collect()
}

/// The newest draft for `repo`'s PR `pr` (as `Draft::pr` spells it), if
/// `list` has one.
fn find(list: &[Draft], repo: &str, pr: &str) -> Option<String> {
    list.iter()
        .filter(|d| d.repo == repo && d.pr == pr && !is_handoff(&d.id))
        .max_by(|a, b| a.created_at.total_cmp(&b.created_at))
        .map(|d| d.id.clone())
}

/// All the drafts, saved whenever one changes.
#[derive(Clone, Copy)]
pub struct Drafts {
    list: RwSignal<Vec<Draft>>,
    /// The drafts whose sessions are being started. In memory only: a
    /// reload while a start is in flight doesn't keep it.
    starting: RwSignal<std::collections::HashSet<String>>,
    /// The error each draft's last start left, if any; in memory only,
    /// like `starting`.
    start_errors: RwSignal<std::collections::HashMap<String, String>>,
}

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
        Drafts {
            list: drafts,
            starting: RwSignal::default(),
            start_errors: RwSignal::default(),
        }
    }

    /// Starts a draft in `repo` and on `pr`, if given, and returns its id.
    pub fn create(self, repo: Option<String>, pr: Option<u64>) -> String {
        let now = js_sys::Date::now();
        let id = format!(
            "{:x}{:08x}",
            now as u64,
            (js_sys::Math::random() * f64::from(u32::MAX)) as u32
        );
        let backend = crate::agents::last_backend();
        self.list.update(|list| {
            list.push(Draft {
                id: id.clone(),
                repo: repo.unwrap_or_else(|| GENERAL.to_owned()),
                pr: pr.map(|n| n.to_string()).unwrap_or_default(),
                prompt: String::new(),
                has_images: false,
                backend,
                // New sessions start out planning, whatever the others go
                // on with.
                mode: PermissionMode::plan(),
                mode_picked: false,
                model: crate::agents::last_model(backend),
                created_at: now,
            })
        });
        id
    }

    /// The draft `id`, or a blank one under that id if there's none, e.g.
    /// when going back to a draft whose session started.
    pub fn open(self, id: &str) -> Draft {
        if let Some(d) = self
            .list
            .with_untracked(|l| l.iter().find(|d| d.id == id).cloned())
        {
            return d;
        }
        let backend = crate::agents::last_backend();
        let d = Draft {
            id: id.to_owned(),
            repo: GENERAL.to_owned(),
            pr: String::new(),
            prompt: String::new(),
            has_images: false,
            backend,
            mode: PermissionMode::plan(),
            mode_picked: false,
            model: crate::agents::last_model(backend),
            created_at: js_sys::Date::now(),
        };
        self.list.update(|list| list.push(d.clone()));
        d
    }

    /// Makes the draft `id` hold `req`, an agent's proposal, unless there
    /// is one already, e.g. with the user's edits from before a reload.
    pub fn seed(self, id: &str, req: &NewAgent) {
        if self.contains(id) {
            return;
        }
        let d = Draft {
            id: id.to_owned(),
            repo: req.repo.clone(),
            pr: req.pr.map(|n| n.to_string()).unwrap_or_default(),
            prompt: req.prompt.clone(),
            has_images: false,
            backend: req.backend,
            mode: req.permission_mode.clone(),
            // The agent picked it, so it doesn't follow the repository's
            // default.
            mode_picked: true,
            model: req
                .model
                .clone()
                .unwrap_or_else(|| crate::agents::DEFAULT_MODEL.to_owned()),
            created_at: js_sys::Date::now(),
        };
        self.list.update(|list| list.push(d));
    }

    /// Whether there is a draft `id`, untracked: callers that react to
    /// the list instead read it through `with` or `listed`.
    pub fn contains(self, id: &str) -> bool {
        self.list.with_untracked(|l| l.iter().any(|d| d.id == id))
    }

    /// The newest draft for `repo`'s PR `pr`, if there's one.
    pub fn find(self, repo: &str, pr: u64) -> Option<String> {
        self.list.with_untracked(|l| find(l, repo, &pr.to_string()))
    }

    /// What `f` makes of the draft `id`, if there is one.
    pub fn with<T>(self, id: &str, f: impl FnOnce(&Draft) -> T) -> Option<T> {
        self.list.with(|l| l.iter().find(|d| d.id == id).map(f))
    }

    pub fn update(self, id: &str, f: impl FnOnce(&mut Draft)) {
        self.list.update(|l| {
            if let Some(d) = l.iter_mut().find(|d| d.id == id) {
                f(d);
            }
        });
    }

    /// Whether a session is being started from the draft `id`.
    pub fn is_starting(self, id: &str) -> bool {
        self.starting.with(|s| s.contains(id))
    }

    pub fn set_starting(self, id: &str, on: bool) {
        self.starting.update(|s| {
            if on {
                s.insert(id.to_owned());
            } else {
                s.remove(id);
            }
        });
    }

    /// The error the draft's last start left, if any.
    pub fn start_error(self, id: &str) -> Option<String> {
        self.start_errors.with(|e| e.get(id).cloned())
    }

    pub fn set_start_error(self, id: &str, error: Option<String>) {
        self.start_errors.update(|e| match error {
            Some(error) => {
                e.insert(id.to_owned(), error);
            }
            None => {
                e.remove(id);
            }
        });
    }

    pub fn remove(self, id: &str) {
        self.list.update(|l| l.retain(|d| d.id != id));
        self.set_starting(id, false);
        self.set_start_error(id, None);
        crate::image_drafts::forget(&crate::image_drafts::new_session_key(id));
    }

    /// Drops the draft `id` if nothing was written in it.
    pub fn remove_if_blank(self, id: &str) {
        if self.with(id, Draft::is_blank).unwrap_or(false) {
            self.remove(id);
        }
    }

    /// See `listed`.
    pub fn listed(self, open: Option<&str>) -> Vec<(String, String)> {
        self.list.with(|l| listed(l, open))
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
            has_images: false,
            backend: AgentBackend::Claude,
            mode: PermissionMode::plan(),
            mode_picked: false,
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
    fn images_make_a_draft_not_blank() {
        let mut d = draft("a", "o/n", "", 1.0);
        d.has_images = true;
        assert!(!d.is_blank());
        // A draft with only images survives the load-time prune.
        let ids: Vec<String> = prune(vec![d], None).into_iter().map(|d| d.id).collect();
        assert_eq!(ids, ["a"]);
    }

    #[test]
    fn finds_the_newest_draft_for_the_pr() {
        let mut older = draft("a", "o/n", "one", 1.0);
        older.pr = "7".into();
        let mut newer = draft("b", "o/n", "two", 2.0);
        newer.pr = "7".into();
        let mut other_pr = draft("c", "o/n", "three", 3.0);
        other_pr.pr = "8".into();
        let mut other_repo = draft("d", "o/m", "four", 4.0);
        other_repo.pr = "7".into();
        let list = vec![older, newer, other_pr, other_repo];
        assert_eq!(find(&list, "o/n", "7"), Some("b".into()));
        assert_eq!(find(&list, "o/n", "8"), Some("c".into()));
        assert_eq!(find(&list, "o/m", "7"), Some("d".into()));
        assert_eq!(find(&list, "o/n", "9"), None);
    }

    #[test]
    fn handoff_ids_are_alphanumeric() {
        assert_eq!(handoff_id("handoff-0123abcd"), "handoff0123abcd");
        assert!(is_handoff(&handoff_id("handoff-0123abcd")));
        assert!(!is_handoff("18f0a1b2c3d4e5f6"));
    }

    #[test]
    fn leaves_handoff_proposals_out() {
        let mut proposal = draft(&handoff_id("handoff-01"), "o/n", "one", 2.0);
        proposal.pr = "7".into();
        let list = vec![draft("a", "o/n", "two", 1.0), proposal];
        assert_eq!(listed(&list, None), [("o/n".to_owned(), "a".to_owned())]);
        assert_eq!(find(&list, "o/n", "7"), None);
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
