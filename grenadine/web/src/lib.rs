//! The grenadine web UI: a landing page with the inboxes of PRs, a page for
//! one PR with its version picker and diff, and the Claude Code sessions
//! started from the UI.

mod agents;
mod api;
mod auto_reviews;
mod composer;
mod debug;
mod diffview;
mod drafts;
mod events;
mod files;
mod global_settings;
mod highlight;
mod image_drafts;
mod images;
mod inboxes;
mod markdown;
mod pr;
mod pr_agents;
mod review;
mod settings;
mod sidebar;
mod tabs;
mod terminal;
mod theme;
mod topbar;

use grenadine_core::api::{GENERAL, PrKey, ServerEvent, SyncStatus};
use leptos::prelude::*;
use wasm_bindgen::prelude::*;

/// Bumped when the server says something changed, so views refetch.
#[derive(Clone, Copy)]
pub struct Updates {
    pub inboxes: RwSignal<u64>,
    /// The PR that changed last, with a counter so that the same PR changing
    /// twice still notifies. `None` after a reconnect, when any PR may have.
    pub pr: RwSignal<(Option<PrKey>, u64)>,
    /// The poller's status, shown in the top bar.
    pub sync: RwSignal<SyncStatus>,
    /// Bumped when an agent session was created, deleted or changed status.
    pub agents: RwSignal<u64>,
    /// Bumped when the cleanup items or the scans for them changed.
    pub cleanup: RwSignal<u64>,
    /// Bumped when a session's terminal opened or closed.
    pub terminals: RwSignal<u64>,
    /// Bumped when a repository's settings changed.
    pub repo_settings: RwSignal<u64>,
    /// Bumped when a review of the viewer's PRs started or ended.
    pub auto_reviews: RwSignal<u64>,
}

/// What the URL's hash shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Inboxes,
    /// `#/owner/name/number`.
    Pr(PrKey),
    /// `#/agents`, `#/agents/new/owner/name` to start a session in a
    /// repository, `#/agents/new/general` to start one in none, or
    /// `#/agents/new/owner/name/number` to start one on a PR.
    /// Each opens a new draft.
    Agents {
        repo: Option<String>,
        pr: Option<u64>,
    },
    /// `#/agents/draft/<id>`: a new session's form.
    Draft(String),
    /// `#/agents/<id>`.
    Agent(String),
    /// `#/agents/cleanup`: what deleted sessions left behind.
    Cleanup,
    /// `#/auto-reviews`: the reviews of the viewer's PRs.
    AutoReviews,
    /// `#/auto-reviews/<id>`: the session doing one of them.
    AutoReview(String),
    /// `#/debug`: what the server spends on GitHub.
    Debug,
}

fn pr_key(owner: &str, name: &str, number: &str) -> Option<PrKey> {
    Some(PrKey {
        repo: format!("{owner}/{name}"),
        number: number.parse().ok()?,
    })
}

fn route_from_hash(hash: &str) -> Route {
    let path = hash.trim_start_matches('#').trim_start_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    let route = match parts[..] {
        ["agents"] => Some(Route::Agents {
            repo: None,
            pr: None,
        }),
        ["agents", "new", GENERAL] => Some(Route::Agents {
            repo: Some(GENERAL.to_owned()),
            pr: None,
        }),
        ["agents", "new", owner, name] if !owner.is_empty() && !name.is_empty() => {
            Some(Route::Agents {
                repo: Some(format!("{owner}/{name}")),
                pr: None,
            })
        }
        ["agents", "new", owner, name, number] => {
            pr_key(owner, name, number).map(|k| Route::Agents {
                repo: Some(k.repo),
                pr: Some(k.number),
            })
        }
        ["agents", "draft", id] if !id.is_empty() => Some(Route::Draft(id.to_owned())),
        ["agents", "cleanup"] => Some(Route::Cleanup),
        ["agents", id] if !id.is_empty() => Some(Route::Agent(id.to_owned())),
        ["auto-reviews"] => Some(Route::AutoReviews),
        ["auto-reviews", id] if !id.is_empty() => Some(Route::AutoReview(id.to_owned())),
        ["debug"] => Some(Route::Debug),
        [owner, name, number] => pr_key(owner, name, number).map(Route::Pr),
        _ => None,
    };
    route.unwrap_or(Route::Inboxes)
}

pub fn hash_for(key: &PrKey) -> String {
    format!("#/{}/{}", key.repo, key.number)
}

/// The PR's page on GitHub.
pub fn github_url(key: &PrKey) -> String {
    format!("https://github.com/{}/pull/{}", key.repo, key.number)
}

/// The icon after a PR's in-app link that opens the same page on GitHub.
/// It's empty: the `.gh-link` CSS draws the icon.
#[component]
pub fn GithubLink(url: String) -> impl IntoView {
    view! {
        <a
            class="gh-link"
            href=url
            target="_blank"
            rel="noopener noreferrer"
            title="Open on GitHub"
            aria-label="Open on GitHub"
        ></a>
    }
}

pub(crate) fn current_hash() -> String {
    window().location().hash().unwrap_or_default()
}

/// The form's links open a new draft each, in place of the link in the
/// history, so that going back skips them.
fn open_draft(route: Route, drafts: drafts::Drafts) -> Route {
    let Route::Agents { repo, pr } = route else {
        return route;
    };
    let id = drafts.create(repo, pr);
    let _ = window().location().replace(&drafts::hash(&id));
    Route::Draft(id)
}

/// Fetches the sync status; the server's lag fallback sends InboxesChanged,
/// so this also covers a dropped SyncStatus event.
///
/// `events` counts the SyncStatus events received. The answer is dropped
/// when one arrived while the fetch was in flight: the answer may predate
/// it, as a poll's InboxesChanged comes while it still searches and its
/// end can overtake the fetch. Dropping it loses nothing, since every
/// later change sends its own event, in order.
fn refetch_sync(updates: Updates, events: StoredValue<u64>) {
    let before = events.get_value();
    leptos::task::spawn_local(async move {
        if let Ok(s) = api::sync_status().await
            && events.get_value() == before
        {
            updates.sync.set(s);
        }
    });
}

/// Subscribes to the server's events for the life of the page. `url`
/// carries the watched PR, so that the server syncs the PR the page
/// shows.
fn listen(updates: Updates, url: Signal<String>) {
    let sync_events = StoredValue::new(0u64);
    let on_message = move |data: String| match serde_json::from_str::<ServerEvent>(&data) {
        Ok(ServerEvent::InboxesChanged) => {
            updates.inboxes.update(|n| *n += 1);
            updates.agents.update(|n| *n += 1);
            refetch_sync(updates, sync_events);
        }
        Ok(ServerEvent::PrChanged(key)) => updates.pr.update(|(k, n)| {
            *k = Some(key);
            *n += 1;
        }),
        Ok(ServerEvent::SyncStatus(s)) => {
            sync_events.update_value(|n| *n += 1);
            updates.sync.set(s);
        }
        Ok(ServerEvent::AgentsChanged) => updates.agents.update(|n| *n += 1),
        Ok(ServerEvent::CleanupChanged) => updates.cleanup.update(|n| *n += 1),
        Ok(ServerEvent::TerminalsChanged(_)) => updates.terminals.update(|n| *n += 1),
        Ok(ServerEvent::RepoSettingsChanged(_)) => updates.repo_settings.update(|n| *n += 1),
        Ok(ServerEvent::AutoReviewsChanged) => updates.auto_reviews.update(|n| *n += 1),
        Err(_) => {}
    };
    // After a reconnect (e.g. the server restarted or the machine slept)
    // refetch everything.
    let on_open = move || {
        updates.inboxes.update(|n| *n += 1);
        updates.pr.update(|(k, n)| {
            *k = None;
            *n += 1;
        });
        updates.agents.update(|n| *n += 1);
        updates.cleanup.update(|n| *n += 1);
        updates.terminals.update(|n| *n += 1);
        updates.repo_settings.update(|n| *n += 1);
        updates.auto_reviews.update(|n| *n += 1);
        refetch_sync(updates, sync_events);
    };
    events::subscribe(url, on_message, on_open);
}

/// The events URL for the route: a PR page watches its PR, every other
/// page watches nothing.
fn events_url(route: &Route) -> String {
    match route {
        Route::Pr(key) => format!(
            "/api/events?watch={}",
            js_sys::encode_uri_component(&format!("pr/{}/{}", key.repo, key.number))
        ),
        _ => "/api/events".into(),
    }
}

fn pr_of(route: &Route) -> Option<PrKey> {
    match route {
        Route::Pr(key) => Some(key.clone()),
        _ => None,
    }
}

#[component]
fn App() -> impl IntoView {
    let updates = Updates {
        inboxes: RwSignal::new(0),
        pr: RwSignal::new((None, 0)),
        sync: RwSignal::new(SyncStatus::default()),
        agents: RwSignal::new(0),
        cleanup: RwSignal::new(0),
        terminals: RwSignal::new(0),
        repo_settings: RwSignal::new(0),
        auto_reviews: RwSignal::new(0),
    };
    provide_context(updates);

    let initial = route_from_hash(&current_hash());
    let open = match &initial {
        Route::Draft(id) => Some(id.clone()),
        _ => None,
    };
    let drafts = drafts::Drafts::load(open.as_deref());
    // Drop image drafts whose text draft is gone; the `agent.*` ones stay
    // like their text drafts do.
    let kept = {
        let open = open.clone();
        move |key: &str| {
            key.starts_with("agent.")
                || drafts
                    .listed(open.as_deref())
                    .iter()
                    .any(|(_, id)| key == image_drafts::new_session_key(id))
        }
    };
    files::sweep(kept.clone());
    image_drafts::sweep(kept);
    provide_context(drafts);
    provide_context(global_settings::provide());
    provide_context(LocalResource::new(api::agent_backends));
    provide_context(LocalResource::new(move || {
        updates.agents.track();
        api::agents()
    }));
    // The page each tab last showed, for its link.
    let memory = tabs::Memory::load();
    provide_context(memory);
    let route = RwSignal::new(open_draft(initial, drafts));
    memory.record(&route.get_untracked());
    // A Memo so the connection only reopens when the URL actually
    // changes, e.g. between two PR pages.
    let url = Memo::new(move |_| route.with(events_url));
    listen(updates, url.into());
    // The PR shown most recently, highlighted when going back to the inboxes.
    let last = RwSignal::new(route.with_untracked(pr_of));
    let handle = window_event_listener_untyped("hashchange", move |_| {
        let r = open_draft(route_from_hash(&current_hash()), drafts);
        if let Some(key) = pr_of(&r) {
            last.set(Some(key));
        }
        memory.record(&r);
        route.set(r);
    });
    on_cleanup(move || handle.remove());
    let on_inboxes = Memo::new(move |_| route.with(|r| *r == Route::Inboxes));
    let selected = Memo::new(move |_| route.with(pr_of));
    let agents = Memo::new(move |_| {
        route.with(|r| match r {
            Route::Draft(_) | Route::Agent(_) | Route::Cleanup => Some(r.clone()),
            _ => None,
        })
    });
    // The sidebar stays mounted while going between the agents' pages.
    let on_agents = Memo::new(move |_| agents.with(Option::is_some));
    let reviews = Memo::new(move |_| {
        route.with(|r| match r {
            Route::AutoReviews | Route::AutoReview(_) => Some(r.clone()),
            _ => None,
        })
    });
    let on_reviews = Memo::new(move |_| reviews.with(Option::is_some));
    let on_debug = Memo::new(move |_| route.with(|r| *r == Route::Debug));

    // The inboxes stay mounted while another page is shown so that going
    // back is instant. Hiding them loses their scroll position, so
    // remember it.
    let inboxes_ref = NodeRef::<leptos::html::Main>::new();
    let scroll = StoredValue::new(0);
    Effect::new(move |_| {
        if on_inboxes.get()
            && let Some(main) = inboxes_ref.get_untracked()
        {
            main.set_scroll_top(scroll.get_value());
        }
    });

    view! {
        <div class="page">
            <topbar::Topbar route=route.into() />
            <main
                class="main"
                class:hidden=move || !on_inboxes.get()
                node_ref=inboxes_ref
                on:scroll=move |_| {
                    if let Some(main) = inboxes_ref.get_untracked() {
                        scroll.set_value(main.scroll_top());
                    }
                }
            >
                <inboxes::Inboxes last=last />
            </main>
            {move || selected.get().map(|key| view! {
                <pr::PrView key=key />
            })}
            {move || on_agents.get().then(|| view! {
                <div class="agents-page">
                    <agents::Sidebar route=route.into() />
                    {move || agents.get().map(|r| match r {
                        // It has its own main, next to the worktree's changes.
                        Route::Agent(id) => view! { <agents::AgentView id=id /> }.into_any(),
                        Route::Draft(id) => view! {
                            <main class="main"><agents::NewSession draft=id /></main>
                        }.into_any(),
                        Route::Cleanup => view! { <main class="main"><agents::CleanupView /></main> }.into_any(),
                        _ => ().into_any(),
                    })}
                </div>
            })}
            {move || on_reviews.get().then(|| view! {
                <div class="agents-page">
                    <auto_reviews::Sidebar route=route.into() />
                    {move || reviews.get().map(|r| match r {
                        Route::AutoReview(id) => view! { <agents::AgentView id=id read_only=true /> }.into_any(),
                        _ => view! { <main class="main"><auto_reviews::Overview /></main> }.into_any(),
                    })}
                </div>
            })}
            {move || on_debug.get().then(|| view! {
                <main class="main"><debug::Debug /></main>
            })}
        </div>
    }
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    // The highlight worker's init runs this too; only a page has a
    // window.
    if web_sys::window().is_none() {
        return;
    }
    // The syntax colors for both themes.
    let document = document();
    if let Ok(style) = document.create_element("style") {
        style.set_text_content(Some(&highlight::stylesheet()));
        if let Some(head) = document.head() {
            let _ = head.append_child(&style);
        }
    }
    leptos::mount::mount_to_body(App);
    highlight::warm();
}

/// A relative timestamp like "5m ago" for a status.
pub(crate) fn ago(now: i64, then: i64) -> String {
    let secs = (now - then).max(0);
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Days since 1970-01-01 for `y`-`m`-`d` (Howard Hinnant's
/// days-from-civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9).rem_euclid(12);
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The inverse of `days_from_civil`: (year, month, day).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `secs` (Unix seconds) as UTC "YYYY-MM-DDTHH:MM:SSZ", for a `<time>`
/// element's `datetime` attribute.
pub(crate) fn iso_datetime(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86400));
    let t = secs.rem_euclid(86400);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        t / 3600,
        t / 60 % 60,
        t % 60
    )
}

/// (year, month, day, hour, minute) in local time under wasm, in UTC in
/// the non-wasm tests.
fn local_fields(secs: i64) -> (i64, i64, i64, i64, i64) {
    #[cfg(target_arch = "wasm32")]
    {
        let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(secs as f64 * 1000.0));
        (
            d.get_full_year() as i64,
            (d.get_month() + 1) as i64,
            d.get_date() as i64,
            d.get_hours() as i64,
            d.get_minutes() as i64,
        )
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (y, m, d) = civil_from_days(secs.div_euclid(86400));
        let t = secs.rem_euclid(86400);
        (y, m, d, t / 3600, t / 60 % 60)
    }
}

/// A compact local "10:34" for `then_secs`, prefixed with the date when
/// it isn't on `now_secs`' local day: "Nov 9, 10:34" in the same year,
/// "2025-11-09 10:34" in a different one.
pub(crate) fn short_local_time(now_secs: i64, then_secs: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (now_y, now_m, now_d, ..) = local_fields(now_secs);
    let (y, m, d, h, min) = local_fields(then_secs);
    if (y, m, d) == (now_y, now_m, now_d) {
        format!("{h:02}:{min:02}")
    } else if y == now_y {
        format!("{} {}, {:02}:{:02}", MONTHS[(m - 1) as usize], d, h, min)
    } else {
        format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}")
    }
}

/// An RFC 3339 timestamp like "2026-01-02T03:04:05Z" or
/// "2026-01-02T03:04:05.123-08:00" as Unix seconds, or `None` when `ts`
/// isn't one.
pub(crate) fn rfc3339_secs(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year: i64 = ts.get(0..4)?.parse().ok()?;
    let month: i64 = ts.get(5..7)?.parse().ok()?;
    let day: i64 = ts.get(8..10)?.parse().ok()?;
    let hour: i64 = ts.get(11..13)?.parse().ok()?;
    let min: i64 = ts.get(14..16)?.parse().ok()?;
    let sec: i64 = ts.get(17..19)?.parse().ok()?;
    let mut rest = &ts[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(frac.len());
        if digits == 0 {
            return None;
        }
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let b = rest.as_bytes();
            if b.len() != 6 || b[3] != b':' {
                return None;
            }
            let sign = match b[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let h: i64 = rest[1..3].parse().ok()?;
            let m: i64 = rest[4..6].parse().ok()?;
            sign * (h * 3600 + m * 60)
        }
    };
    Some(days_from_civil(year, month, day) * 86400 + hour * 3600 + min * 60 + sec - offset)
}

/// `secs` (Unix seconds) as local "YYYY-MM-DD HH:MM:SS UTC±HH:MM".
pub(crate) fn local_datetime(secs: i64) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(secs as f64 * 1000.0));
        // JavaScript's offset is minutes to add to local time to get
        // UTC, the opposite of the "UTC±HH:MM" sign.
        let offset = -d.get_timezone_offset() as i64;
        let (sign, abs) = if offset < 0 {
            ('-', -offset)
        } else {
            ('+', offset)
        };
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC{}{:02}:{:02}",
            d.get_full_year(),
            d.get_month() + 1,
            d.get_date(),
            d.get_hours(),
            d.get_minutes(),
            d.get_seconds(),
            sign,
            abs / 60,
            abs % 60
        )
    }
    // The tests don't run under wasm; UTC is close enough there.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (y, m, d) = civil_from_days(secs.div_euclid(86400));
        let t = secs.rem_euclid(86400);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC+00:00",
            y,
            m,
            d,
            t / 3600,
            t / 60 % 60,
            t % 60
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_ranges() {
        assert_eq!(ago(100, 100), "just now");
        assert_eq!(ago(100, 200), "just now");
        assert_eq!(ago(119, 60), "just now");
        assert_eq!(ago(120, 60), "1m ago");
        assert_eq!(ago(3659, 60), "59m ago");
        assert_eq!(ago(3660, 60), "1h ago");
        assert_eq!(ago(86400 + 60, 60), "1d ago");
        assert_eq!(ago(86400 * 5 + 60, 60), "5d ago");
    }

    #[test]
    fn iso_datetime_utc() {
        let secs = rfc3339_secs("2026-11-10T18:34:06Z").unwrap();
        assert_eq!(iso_datetime(secs), "2026-11-10T18:34:06Z");
        assert_eq!(iso_datetime(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_datetime(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn short_local_time_utc() {
        let now = rfc3339_secs("2026-11-10T18:34:06Z").unwrap();
        // The same day is a bare time.
        assert_eq!(
            short_local_time(now, rfc3339_secs("2026-11-10T10:34:00Z").unwrap()),
            "10:34"
        );
        // An earlier day in the same year gets a short date.
        assert_eq!(
            short_local_time(now, rfc3339_secs("2026-11-09T10:34:00Z").unwrap()),
            "Nov 9, 10:34"
        );
        // A different year gets the full ISO date.
        assert_eq!(
            short_local_time(now, rfc3339_secs("2025-12-31T10:34:00Z").unwrap()),
            "2025-12-31 10:34"
        );
        // Right across a day boundary the date still shows.
        assert_eq!(
            short_local_time(
                rfc3339_secs("2026-11-10T00:01:00Z").unwrap(),
                rfc3339_secs("2026-11-09T23:59:00Z").unwrap()
            ),
            "Nov 9, 23:59"
        );
        // Right across a year boundary the year shows.
        assert_eq!(
            short_local_time(
                rfc3339_secs("2026-01-01T00:01:00Z").unwrap(),
                rfc3339_secs("2025-12-31T23:59:00Z").unwrap()
            ),
            "2025-12-31 23:59"
        );
    }

    #[test]
    fn rfc3339_secs_parses() {
        let secs = rfc3339_secs("2026-11-10T18:34:06Z").unwrap();
        assert_eq!(rfc3339_secs("2026-11-10T18:34:06+00:00"), Some(secs));
        assert_eq!(rfc3339_secs("2026-11-10T10:34:06-08:00"), Some(secs));
        assert_eq!(rfc3339_secs("2026-11-10T18:34:06.987Z"), Some(secs));
        assert_eq!(rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_secs("1969-12-31T16:00:00-08:00"), Some(0));
        assert_eq!(rfc3339_secs(""), None);
        assert_eq!(rfc3339_secs("garbage"), None);
        assert_eq!(rfc3339_secs("2026-11-10 18:34:06Z"), None);
        assert_eq!(rfc3339_secs("2026-11-10T18:34:06"), None);
        assert_eq!(rfc3339_secs("2026-11-10T18:34:06."), None);
    }

    #[test]
    fn local_datetime_utc() {
        let secs = rfc3339_secs("2026-11-10T18:34:06Z").unwrap();
        assert_eq!(local_datetime(secs), "2026-11-10 18:34:06 UTC+00:00");
        assert_eq!(local_datetime(0), "1970-01-01 00:00:00 UTC+00:00");
        assert_eq!(local_datetime(-1), "1969-12-31 23:59:59 UTC+00:00");
    }

    fn key(repo: &str, number: u64) -> PrKey {
        PrKey {
            repo: repo.into(),
            number,
        }
    }

    #[test]
    fn routes() {
        assert_eq!(route_from_hash(""), Route::Inboxes);
        assert_eq!(route_from_hash("#/"), Route::Inboxes);
        assert_eq!(route_from_hash("#/o/n/7"), Route::Pr(key("o/n", 7)));
        assert_eq!(route_from_hash("#/o/n/x"), Route::Inboxes);
        assert_eq!(
            route_from_hash("#/agents"),
            Route::Agents {
                repo: None,
                pr: None
            }
        );
        assert_eq!(
            route_from_hash("#/agents/new/o/n"),
            Route::Agents {
                repo: Some("o/n".into()),
                pr: None
            }
        );
        assert_eq!(
            route_from_hash("#/agents/new/general"),
            Route::Agents {
                repo: Some(GENERAL.into()),
                pr: None
            }
        );
        assert_eq!(
            route_from_hash("#/agents/new/o/n/7"),
            Route::Agents {
                repo: Some("o/n".into()),
                pr: Some(7)
            }
        );
        assert_eq!(
            route_from_hash("#/agents/draft/abc"),
            Route::Draft("abc".into())
        );
        assert_eq!(route_from_hash("#/agents/abc"), Route::Agent("abc".into()));
        assert_eq!(route_from_hash("#/agents/cleanup"), Route::Cleanup);
        assert_eq!(route_from_hash("#/agents/"), Route::Inboxes);
        assert_eq!(route_from_hash("#/auto-reviews"), Route::AutoReviews);
        assert_eq!(
            route_from_hash("#/auto-reviews/abc"),
            Route::AutoReview("abc".into())
        );
        assert_eq!(route_from_hash("#/auto-reviews/"), Route::Inboxes);
        assert_eq!(route_from_hash("#/debug"), Route::Debug);
        // An owner called "agents" still has PRs.
        assert_eq!(
            route_from_hash("#/agents/n/7"),
            Route::Pr(key("agents/n", 7))
        );
    }

    #[test]
    fn github_urls() {
        assert_eq!(github_url(&key("o/n", 7)), "https://github.com/o/n/pull/7");
    }
}
