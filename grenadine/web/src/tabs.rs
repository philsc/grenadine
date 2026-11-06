//! The top bar's tabs remember the last page shown under each of them,
//! so that going to another tab and back returns to it. The memory is
//! kept in session storage, which makes it survive a reload while
//! staying per browser window.

use grenadine_core::api::SessionKind;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{Route, hash_for, route_from_hash};

/// A tab in the top bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Inboxes,
    Agents,
    AutoReviews,
    Debug,
}

/// The tab `route` belongs to.
pub fn tab_of(route: &Route) -> Tab {
    match route {
        Route::Inboxes | Route::Pr(_) => Tab::Inboxes,
        Route::Agents { .. } | Route::Draft(_) | Route::Agent(_) | Route::Cleanup => Tab::Agents,
        Route::AutoReviews | Route::AutoReview(_) => Tab::AutoReviews,
        Route::Debug => Tab::Debug,
    }
}

/// The page the tab opens on when it's clicked while active.
pub fn root(tab: Tab) -> &'static str {
    match tab {
        Tab::Inboxes => "#/",
        Tab::Agents => "#/agents",
        Tab::AutoReviews => "#/auto-reviews",
        Tab::Debug => "#/debug",
    }
}

/// The hash of a route worth remembering; roots remember nothing.
fn hash_of(route: &Route) -> Option<String> {
    match route {
        Route::Pr(key) => Some(hash_for(key)),
        Route::Draft(id) => Some(crate::drafts::hash(id)),
        Route::Agent(id) => Some(crate::agents::session_hash(id, SessionKind::Interactive)),
        Route::Cleanup => Some("#/agents/cleanup".to_owned()),
        Route::AutoReview(id) => Some(crate::auto_reviews::hash(id)),
        Route::Inboxes | Route::Agents { .. } | Route::AutoReviews | Route::Debug => None,
    }
}

/// What `tab` links to: its root while it's the active tab, the page it
/// remembers otherwise. `exists` says whether a remembered page's target
/// still is; pages that can't go stale don't ask.
pub fn href(
    tab: Tab,
    current: &Route,
    remembered: Option<&Route>,
    exists: impl Fn(&Route) -> bool,
) -> String {
    if tab_of(current) == tab {
        return root(tab).to_owned();
    }
    match remembered {
        Some(route)
            if match route {
                Route::Draft(_) | Route::Agent(_) | Route::AutoReview(_) => exists(route),
                _ => true,
            } =>
        {
            hash_of(route).unwrap_or_else(|| root(tab).to_owned())
        }
        _ => root(tab).to_owned(),
    }
}

const KEY: &str = "grenadine.tabs";

/// The stored memory: one hash per tab.
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    inboxes: Option<String>,
    agents: Option<String>,
    auto_reviews: Option<String>,
}

/// The last page each of the remembering tabs showed.
#[derive(Clone, Copy)]
pub struct Memory {
    inboxes: RwSignal<Option<Route>>,
    agents: RwSignal<Option<Route>>,
    auto_reviews: RwSignal<Option<Route>>,
}

/// A stored hash as a route, if it's a page of `tab`'s worth keeping.
fn parse(tab: Tab, hash: &Option<String>) -> Option<Route> {
    let route = route_from_hash(hash.as_deref()?);
    (tab_of(&route) == tab && hash_of(&route).is_some()).then_some(route)
}

impl Memory {
    /// The memory left from before the reload, if any.
    pub fn load() -> Memory {
        let saved: Saved = window()
            .session_storage()
            .ok()
            .flatten()
            .and_then(|s| s.get_item(KEY).ok().flatten())
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        Memory {
            inboxes: RwSignal::new(parse(Tab::Inboxes, &saved.inboxes)),
            agents: RwSignal::new(parse(Tab::Agents, &saved.agents)),
            auto_reviews: RwSignal::new(parse(Tab::AutoReviews, &saved.auto_reviews)),
        }
    }

    /// What `tab` remembers, if anything.
    pub fn get(self, tab: Tab) -> Option<Route> {
        match tab {
            Tab::Inboxes => self.inboxes,
            Tab::Agents => self.agents,
            Tab::AutoReviews => self.auto_reviews,
            Tab::Debug => return None,
        }
        .get()
    }

    /// Notes `route` as its tab's page; a tab's root clears its memory.
    /// Debug keeps none.
    pub fn record(self, route: &Route) {
        let signal = match tab_of(route) {
            Tab::Inboxes => self.inboxes,
            Tab::Agents => self.agents,
            Tab::AutoReviews => self.auto_reviews,
            Tab::Debug => return,
        };
        match route {
            Route::Inboxes | Route::Agents { .. } | Route::AutoReviews => signal.set(None),
            _ => signal.set(Some(route.clone())),
        }
        let Some(storage) = window().session_storage().ok().flatten() else {
            return;
        };
        let saved = Saved {
            inboxes: self.inboxes.get_untracked().as_ref().and_then(hash_of),
            agents: self.agents.get_untracked().as_ref().and_then(hash_of),
            auto_reviews: self.auto_reviews.get_untracked().as_ref().and_then(hash_of),
        };
        if let Ok(json) = serde_json::to_string(&saved) {
            let _ = storage.set_item(KEY, &json);
        }
    }
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::PrKey;

    use super::*;

    fn pr(number: u64) -> Route {
        Route::Pr(PrKey {
            repo: "o/n".into(),
            number,
        })
    }

    #[test]
    fn tabs_of_routes() {
        assert_eq!(tab_of(&Route::Inboxes), Tab::Inboxes);
        assert_eq!(tab_of(&pr(1)), Tab::Inboxes);
        assert_eq!(
            tab_of(&Route::Agents {
                repo: None,
                pr: None
            }),
            Tab::Agents
        );
        assert_eq!(tab_of(&Route::Draft("d".into())), Tab::Agents);
        assert_eq!(tab_of(&Route::Agent("a".into())), Tab::Agents);
        assert_eq!(tab_of(&Route::Cleanup), Tab::Agents);
        assert_eq!(tab_of(&Route::AutoReviews), Tab::AutoReviews);
        assert_eq!(tab_of(&Route::AutoReview("r".into())), Tab::AutoReviews);
        assert_eq!(tab_of(&Route::Debug), Tab::Debug);
    }

    #[test]
    fn roots() {
        assert_eq!(root(Tab::Inboxes), "#/");
        assert_eq!(root(Tab::Agents), "#/agents");
        assert_eq!(root(Tab::AutoReviews), "#/auto-reviews");
        assert_eq!(root(Tab::Debug), "#/debug");
    }

    #[test]
    fn hrefs() {
        let on_pr = pr(1);
        let on_agent = Route::Agent("a".into());
        let yes = |_: &Route| true;
        let no = |_: &Route| false;

        // Another tab's remembered page is its link.
        assert_eq!(href(Tab::Inboxes, &on_agent, Some(&pr(7)), no), "#/o/n/7");
        assert_eq!(
            href(Tab::Agents, &on_pr, Some(&Route::Agent("s".into())), yes),
            "#/agents/s"
        );
        assert_eq!(
            href(
                Tab::AutoReviews,
                &on_pr,
                Some(&Route::AutoReview("r".into())),
                yes
            ),
            "#/auto-reviews/r"
        );

        // The active tab goes to its root, memory or not.
        assert_eq!(href(Tab::Inboxes, &pr(7), Some(&pr(7)), yes), "#/");
        assert_eq!(
            href(Tab::Agents, &on_agent, Some(&on_agent.clone()), yes),
            "#/agents"
        );
        assert_eq!(
            href(
                Tab::AutoReviews,
                &Route::AutoReview("r".into()),
                Some(&Route::AutoReview("r".into())),
                yes
            ),
            "#/auto-reviews"
        );

        // Nothing remembered, or what is is gone: the root.
        assert_eq!(href(Tab::Agents, &on_pr, None, yes), "#/agents");
        assert_eq!(
            href(Tab::Agents, &on_pr, Some(&Route::Agent("gone".into())), no),
            "#/agents"
        );
        assert_eq!(
            href(Tab::Agents, &on_pr, Some(&Route::Draft("gone".into())), no),
            "#/agents"
        );
        assert_eq!(
            href(
                Tab::AutoReviews,
                &on_pr,
                Some(&Route::AutoReview("gone".into())),
                no
            ),
            "#/auto-reviews"
        );

        // PRs and cleanup can't go stale.
        assert_eq!(href(Tab::Inboxes, &on_agent, Some(&pr(7)), no), "#/o/n/7");
        assert_eq!(
            href(Tab::Agents, &on_pr, Some(&Route::Cleanup), no),
            "#/agents/cleanup"
        );
    }
}
