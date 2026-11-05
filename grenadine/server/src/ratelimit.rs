//! Keeps grenadine inside GitHub's rate limits, and records what it
//! spends.
//!
//! Requests run under a priority from a task-local: the viewer's own
//! actions always go, inbox searches pause below a tenth of the limit,
//! background syncs below a fifth. GitHub answers each response from
//! one of a few independent counters per resource, each with its own
//! `x-ratelimit-reset` window, so the states are kept per counter —
//! matched by the reported reset — and every request lands in an
//! hour-long log that the Debug tab reads.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;

use grenadine_core::api::{
    ApiResource, GitHubUsage, MinuteUsage, RateLimitInfo, RateLimitReport, RequestKindUsage,
    RequestPriority,
};

tokio::task_local! {
    static PRIORITY: RequestPriority;
}

/// Runs `f` with `priority` as the tier its GitHub requests are admitted
/// and logged under. `tokio::spawn` doesn't inherit the task-local, so
/// spawned tasks must scope themselves.
pub async fn with_priority<F: Future>(priority: RequestPriority, f: F) -> F::Output {
    PRIORITY.scope(priority, f).await
}

/// The priority of the current task; `User` outside `with_priority`.
pub fn current() -> RequestPriority {
    PRIORITY.try_with(|p| *p).unwrap_or(RequestPriority::User)
}

/// A request was held back or refused because the resource's limit is
/// nearly or fully spent.
#[derive(Debug)]
pub struct RateLimited {
    pub resource: ApiResource,
    /// Unix seconds when the limit's window resets.
    pub until: i64,
}

impl fmt::Display for RateLimited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let resource = match self.resource {
            ApiResource::GraphQl => "GraphQL",
            ApiResource::Rest => "REST",
        };
        let until = chrono::DateTime::from_timestamp(self.until, 0)
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_else(|| self.until.to_string());
        write!(
            f,
            "paused to stay within GitHub's {resource} rate limit until {until}"
        )
    }
}

impl std::error::Error for RateLimited {}

/// Whether `e` or one of its causes is a `RateLimited`.
pub fn is_rate_limited(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<RateLimited>())
}

/// A resource's limit as one response's headers reported it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimitState {
    pub limit: u32,
    pub remaining: u32,
    pub used: u32,
    /// Unix seconds when the window resets.
    pub reset: i64,
    /// Unix seconds when this state was observed.
    pub seen: i64,
}

/// One request the log records.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Unix seconds when it ran.
    pub at: i64,
    /// What the request was for, e.g. "search" or "pr".
    pub kind: &'static str,
    pub resource: ApiResource,
    pub priority: RequestPriority,
    /// The GraphQL points it cost; `None` where they can't be known.
    pub points: Option<u32>,
    /// Held back without being sent.
    pub deferred: bool,
    /// The reset the response's headers reported, `None` for deferred
    /// requests and responses without headers.
    pub reset: Option<i64>,
}

/// What a `GitHub` has seen of its limits and spent, per counter.
/// In memory only; it starts over on a restart.
#[derive(Default)]
pub struct Usage {
    /// GitHub answers each response from one of a few independent
    /// counters per resource, each with its own window; the counter a
    /// response came from is told by its `reset`.
    counters: Vec<(ApiResource, LimitState)>,
    log: VecDeque<Entry>,
    /// The last responses' reports, oldest first.
    reports: VecDeque<RateLimitReport>,
}

/// How long the log and the `ours` counts look back.
const WINDOW: i64 = 3600;

/// How many reports are kept for the Debug tab.
const REPORTS: usize = 100;

/// GitHub's reported reset can wobble by a second between two responses
/// from the same counter.
const RESET_SLACK: i64 = 2;

impl Usage {
    /// Records `state` for `resource`, from a `kind` response's headers.
    /// `repos` are the `owner/name` repos the request named. The state
    /// joins the live counter whose reset matches within `RESET_SLACK`,
    /// keeping the highest `used` seen, or starts a new counter.
    pub fn saw(
        &mut self,
        resource: ApiResource,
        kind: &'static str,
        repos: Vec<String>,
        state: LimitState,
    ) {
        self.counters.retain(|(_, s)| state.seen < s.reset);
        match self
            .counters
            .iter_mut()
            .find(|(r, s)| *r == resource && (s.reset - state.reset).abs() <= RESET_SLACK)
        {
            Some((_, s)) if state.used >= s.used => *s = state,
            Some(_) => {}
            None => self.counters.push((resource, state)),
        }
        self.reports.push_back(RateLimitReport {
            at: state.seen,
            resource,
            kind: kind.to_owned(),
            repos,
            limit: state.limit,
            remaining: state.remaining,
            used: state.used,
            reset: state.reset,
        });
        while self.reports.len() > REPORTS {
            self.reports.pop_front();
        }
    }

    /// Logs a request that was sent.
    pub fn log(&mut self, entry: Entry) {
        self.prune(entry.at);
        self.log.push_back(entry);
    }

    /// Drops the entries older than an hour before `now`.
    fn prune(&mut self, now: i64) {
        while self.log.front().is_some_and(|e| e.at <= now - WINDOW) {
            self.log.pop_front();
        }
    }

    /// Lets `priority`'s request of `kind` go, or refuses it when a
    /// live counter's remainder is below the tier's reserve: the
    /// most-spent counter decides. A refused request is logged as
    /// deferred and must not be sent. Once the reset has passed
    /// everything goes again; the next response refreshes the
    /// counter.
    pub fn admit(
        &mut self,
        kind: &'static str,
        resource: ApiResource,
        priority: RequestPriority,
        now: i64,
    ) -> Result<(), RateLimited> {
        let until = self
            .counters
            .iter()
            .filter(|(r, _)| *r == resource)
            .filter_map(|(_, s)| Self::denied_state(s, priority, now))
            .max();
        if let Some(until) = until {
            self.log(Entry {
                at: now,
                kind,
                resource,
                priority,
                points: None,
                deferred: true,
                reset: None,
            });
            return Err(RateLimited { resource, until });
        }
        Ok(())
    }

    /// The reset time when `s` refuses `priority` now, else `None`.
    fn denied_state(s: &LimitState, priority: RequestPriority, now: i64) -> Option<i64> {
        if now >= s.reset {
            return None;
        }
        let reserve = match priority {
            RequestPriority::User => 0,
            RequestPriority::Search => s.limit / 10,
            RequestPriority::Background => s.limit / 5,
        };
        (s.remaining <= reserve).then_some(s.reset)
    }

    /// The latest reset among the live counters that refuse `priority`,
    /// else `None`. Searches are GraphQL only, so a spent REST counter
    /// doesn't pause them.
    pub fn paused_until(&self, priority: RequestPriority, now: i64) -> Option<i64> {
        self.counters
            .iter()
            .filter(|(r, _)| priority != RequestPriority::Search || *r == ApiResource::GraphQl)
            .filter_map(|(_, s)| Self::denied_state(s, priority, now))
            .max()
    }

    /// What the Debug tab shows: the live counters, the last hour
    /// grouped by kind and the last hour minute by minute.
    pub fn snapshot(&mut self, now: i64) -> GitHubUsage {
        self.prune(now);
        let mut groups: BTreeMap<(ApiResource, &'static str, RequestPriority), RequestKindUsage> =
            BTreeMap::new();
        for e in &self.log {
            let g = groups
                .entry((e.resource, e.kind, e.priority))
                .or_insert_with(|| RequestKindUsage {
                    kind: e.kind.to_owned(),
                    resource: e.resource,
                    priority: e.priority,
                    requests: 0,
                    points: None,
                    deferred: 0,
                });
            g.requests += 1;
            g.deferred += e.deferred as u32;
            if let Some(points) = e.points {
                *g.points.get_or_insert(0) += points;
            }
        }
        let mut kinds: Vec<RequestKindUsage> = groups.into_values().collect();
        kinds.sort_by(|a, b| {
            b.points
                .unwrap_or(0)
                .cmp(&a.points.unwrap_or(0))
                .then(b.requests.cmp(&a.requests))
        });

        let current = now - now % 60;
        let mut minutes = (0..60)
            .map(|i| MinuteUsage {
                start: current - (59 - i) * 60,
                graphql_points: 0,
                rest_requests: 0,
            })
            .collect::<Vec<_>>();
        for e in self.log.iter().filter(|e| !e.deferred) {
            let i = 59i64 - (current - (e.at - e.at % 60)) / 60;
            let Some(m) = usize::try_from(i).ok().and_then(|i| minutes.get_mut(i)) else {
                continue;
            };
            match e.resource {
                ApiResource::GraphQl => m.graphql_points += e.points.unwrap_or(0),
                ApiResource::Rest => m.rest_requests += 1,
            }
        }

        // The tiers from least to most patient.
        const TIERS: [RequestPriority; 3] = [
            RequestPriority::Background,
            RequestPriority::Search,
            RequestPriority::User,
        ];
        let mut limits = Vec::new();
        for &(resource, s) in &self.counters {
            if now >= s.reset {
                continue;
            }
            let since = s.reset - WINDOW;
            let ours = self
                .log
                .iter()
                .filter(|e| {
                    e.resource == resource
                        && !e.deferred
                        && e.at >= since
                        && e.reset.is_some_and(|r| (r - s.reset).abs() <= RESET_SLACK)
                })
                .map(|e| match resource {
                    ApiResource::GraphQl => e.points.unwrap_or(0),
                    ApiResource::Rest => 1,
                })
                .sum();
            let responses = self
                .reports
                .iter()
                .filter(|r| r.resource == resource)
                .count() as u32;
            let answered = self
                .reports
                .iter()
                .filter(|r| r.resource == resource && (r.reset - s.reset).abs() <= RESET_SLACK)
                .count() as u32;
            limits.push(RateLimitInfo {
                resource,
                limit: s.limit,
                remaining: s.remaining,
                used: s.used,
                reset: s.reset,
                answered,
                responses,
                ours,
                paused: TIERS
                    .into_iter()
                    .filter(|&p| Self::denied_state(&s, p, now).is_some())
                    .collect(),
            });
        }
        limits.sort_by_key(|l| (l.resource, l.remaining));
        GitHubUsage {
            now,
            limits,
            kinds,
            minutes,
            reports: self.reports.iter().rev().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(limit: u32, remaining: u32, reset: i64) -> LimitState {
        LimitState {
            limit,
            remaining,
            used: limit - remaining,
            reset,
            seen: 0,
        }
    }

    /// A `Usage` that saw one response of `kind` on GraphQL.
    fn usage(kind: &'static str, limit: u32, remaining: u32, reset: i64) -> Usage {
        let mut u = Usage::default();
        u.saw(
            ApiResource::GraphQl,
            kind,
            vec![],
            state(limit, remaining, reset),
        );
        u
    }

    /// Logs a request that ran at `at` on a counter resetting at `reset`.
    fn sent(
        u: &mut Usage,
        resource: ApiResource,
        kind: &'static str,
        points: u32,
        at: i64,
        reset: i64,
    ) {
        u.log(Entry {
            at,
            kind,
            resource,
            priority: RequestPriority::Background,
            points: (resource == ApiResource::GraphQl).then_some(points),
            deferred: false,
            reset: Some(reset),
        });
    }

    fn admit(u: &mut Usage, priority: RequestPriority, now: i64) -> Result<(), RateLimited> {
        u.admit("pr", ApiResource::GraphQl, priority, now)
    }

    #[test]
    fn tasks_default_to_user_priority() {
        assert_eq!(current(), RequestPriority::User);
    }

    #[tokio::test]
    async fn with_priority_scopes_the_tier() {
        assert_eq!(
            with_priority(RequestPriority::Background, async { current() }).await,
            RequestPriority::Background
        );
        assert_eq!(current(), RequestPriority::User);
    }

    #[test]
    fn tiers_pause_at_their_reserves() {
        let now = 100;
        let reset = 3700;
        // Background pauses below a fifth, searches below a tenth, the
        // user's actions only at zero.
        let mut u = usage("pr", 5000, 1001, reset);
        for p in [
            RequestPriority::User,
            RequestPriority::Search,
            RequestPriority::Background,
        ] {
            admit(&mut u, p, now).unwrap();
        }
        let mut u = usage("pr", 5000, 1000, reset);
        admit(&mut u, RequestPriority::Background, now).unwrap_err();
        admit(&mut u, RequestPriority::Search, now).unwrap();
        let mut u = usage("pr", 5000, 500, reset);
        admit(&mut u, RequestPriority::Search, now).unwrap_err();
        admit(&mut u, RequestPriority::User, now).unwrap();
        let mut u = usage("pr", 5000, 0, reset);
        admit(&mut u, RequestPriority::User, now).unwrap_err();

        // Past the reset everything goes again.
        let mut u = usage("pr", 5000, 0, reset);
        for p in [
            RequestPriority::User,
            RequestPriority::Search,
            RequestPriority::Background,
        ] {
            admit(&mut u, p, reset).unwrap();
        }
    }

    #[test]
    fn refused_requests_are_logged_deferred() {
        let mut u = usage("pr", 5000, 1000, 3700);
        admit(&mut u, RequestPriority::Background, 100).unwrap_err();
        let snap = u.snapshot(100);
        let entry = &snap.kinds[0];
        assert_eq!(entry.requests, 1);
        assert_eq!(entry.deferred, 1);
        // A refusal spends nothing.
        assert_eq!(snap.minutes[59].graphql_points, 0);
    }

    #[test]
    fn paused_until_reports_the_pause() {
        let now = 100;
        let u = &mut usage("pr", 5000, 400, 3700);
        assert_eq!(u.paused_until(RequestPriority::Background, now), Some(3700));
        assert_eq!(u.paused_until(RequestPriority::Search, now), Some(3700));
        assert_eq!(u.paused_until(RequestPriority::User, now), None);

        // A spent REST counter pauses background syncs but not searches:
        // searches are GraphQL only.
        u.saw(ApiResource::Rest, "activity", vec![], state(60, 0, 4000));
        assert_eq!(u.paused_until(RequestPriority::Search, now), Some(3700));
        assert_eq!(u.paused_until(RequestPriority::Background, now), Some(4000));

        // Nothing is paused past the resets.
        assert_eq!(u.paused_until(RequestPriority::Background, 4000), None);
        assert_eq!(u.paused_until(RequestPriority::Search, 3700), None);
    }

    #[test]
    fn one_counter_keeps_the_highest_used() {
        let now = 10_000;
        let reset = now + 1800;
        let mut u = Usage::default();
        for (used, reset, seen) in [(1000, reset, 1), (990, reset, 2), (1010, reset + 1, 3)] {
            // A reset that wobbles by a second is the same counter.
            let mut s = state(5000, 5000 - used, reset);
            s.seen = now - seen;
            u.saw(ApiResource::GraphQl, "pr", vec![], s);
        }
        let snap = u.snapshot(now);
        assert_eq!(snap.limits.len(), 1);
        assert_eq!(snap.limits[0].used, 1010);
        assert_eq!(snap.limits[0].answered, 3);
        assert_eq!(snap.limits[0].responses, 3);
    }

    #[test]
    fn the_most_spent_counter_decides() {
        let now = 100;
        let reset_a = 3700;
        let mut u = Usage::default();
        // Counter A is nearly out; counter B, its reset a little later,
        // has plenty left.
        u.saw(
            ApiResource::GraphQl,
            "pr",
            vec![],
            state(5000, 400, reset_a),
        );
        u.saw(
            ApiResource::GraphQl,
            "review_threads",
            vec![],
            state(5000, 3600, reset_a + 34),
        );
        for kind in ["pr", "search"] {
            for priority in [RequestPriority::Search, RequestPriority::Background] {
                assert_eq!(
                    u.admit(kind, ApiResource::GraphQl, priority, now)
                        .unwrap_err()
                        .until,
                    reset_a
                );
            }
            u.admit(kind, ApiResource::GraphQl, RequestPriority::User, now)
                .unwrap();
        }

        // A healthy report from B doesn't lift the pause.
        u.saw(
            ApiResource::GraphQl,
            "pr",
            vec![],
            state(5000, 4000, reset_a + 34),
        );
        admit(&mut u, RequestPriority::Background, now).unwrap_err();

        // Past A's reset it has expired and everything goes again.
        admit(&mut u, RequestPriority::Search, reset_a).unwrap();
        admit(&mut u, RequestPriority::Background, reset_a).unwrap();
    }

    #[test]
    fn snapshots_group_sort_and_prune() {
        let now = 10_000;
        let reset = now + 1800;
        let mut u = usage("pr", 5000, 4500, reset);
        // Older than an hour: gone.
        u.log(Entry {
            at: now - 3600,
            kind: "pr",
            resource: ApiResource::GraphQl,
            priority: RequestPriority::Background,
            points: Some(100),
            deferred: false,
            reset: Some(reset),
        });
        // Inside both the log's hour and the counter's window
        // (reset-3600).
        let at = now - 1000;
        for (kind, priority, points) in [
            ("pr", RequestPriority::Background, 4),
            ("pr", RequestPriority::Background, 2),
            ("search", RequestPriority::Search, 3),
            ("activity", RequestPriority::Background, 0),
        ] {
            let resource = if kind == "activity" {
                ApiResource::Rest
            } else {
                ApiResource::GraphQl
            };
            u.log(Entry {
                at,
                kind,
                resource,
                priority,
                points: (kind != "activity").then_some(points),
                deferred: false,
                reset: Some(reset),
            });
        }
        u.saw(ApiResource::Rest, "activity", vec![], state(60, 55, reset));
        let snap = u.snapshot(now);
        assert_eq!(snap.kinds.len(), 3);
        // Sorted by points desc: pr (6) then search (3) then activity.
        assert_eq!(snap.kinds[0].kind, "pr");
        assert_eq!(snap.kinds[0].requests, 2);
        assert_eq!(snap.kinds[0].points, Some(6));
        assert_eq!(snap.kinds[1].kind, "search");
        assert_eq!(snap.kinds[2].kind, "activity");
        assert_eq!(snap.kinds[2].points, None);

        assert_eq!(snap.minutes.len(), 60);
        assert_eq!(snap.minutes[59].start, now - now % 60);
        let minute = (at - at % 60) as usize;
        let i = 59 - ((now - now % 60) as usize - minute) / 60;
        assert_eq!(snap.minutes[i].graphql_points, 9);
        assert_eq!(snap.minutes[i].rest_requests, 1);

        // `ours` counts only this window and this counter's entries.
        let gql = snap
            .limits
            .iter()
            .find(|l| l.resource == ApiResource::GraphQl)
            .unwrap();
        assert_eq!(gql.ours, 9);
        let rest = snap
            .limits
            .iter()
            .find(|l| l.resource == ApiResource::Rest)
            .unwrap();
        assert_eq!(rest.ours, 1);
    }

    #[test]
    fn snapshots_have_one_limit_per_counter() {
        let now = 10_000;
        let reset_a = now + 1800;
        let reset_b = reset_a + 34;
        let mut u = Usage::default();
        u.saw(
            ApiResource::GraphQl,
            "pr",
            vec![],
            state(5000, 3600, reset_b),
        );
        u.saw(
            ApiResource::GraphQl,
            "pr",
            vec![],
            state(5000, 400, reset_a),
        );
        u.saw(
            ApiResource::Rest,
            "activity",
            vec![],
            state(60, 55, now + 2000),
        );
        // The requests' resets say which counter answered them; a
        // request without headers counts for none.
        sent(&mut u, ApiResource::GraphQl, "pr", 10, now - 100, reset_a);
        sent(&mut u, ApiResource::GraphQl, "pr", 20, now - 100, reset_b);
        u.log(Entry {
            at: now - 100,
            kind: "pr",
            resource: ApiResource::GraphQl,
            priority: RequestPriority::Background,
            points: Some(5),
            deferred: false,
            reset: None,
        });
        sent(
            &mut u,
            ApiResource::Rest,
            "activity",
            0,
            now - 100,
            now + 2000,
        );
        let snap = u.snapshot(now);
        assert_eq!(snap.limits.len(), 3);
        // The most-spent GraphQL counter comes first.
        assert_eq!(snap.limits[0].reset, reset_a);
        assert_eq!(snap.limits[0].used, 4600);
        assert_eq!(snap.limits[0].ours, 10);
        assert_eq!(snap.limits[0].answered, 1);
        assert_eq!(snap.limits[0].responses, 2);
        assert_eq!(snap.limits[1].reset, reset_b);
        assert_eq!(snap.limits[1].used, 1400);
        assert_eq!(snap.limits[1].ours, 20);
        assert_eq!(snap.limits[1].answered, 1);
        let rest = &snap.limits[2];
        assert_eq!(rest.resource, ApiResource::Rest);
        assert_eq!(rest.ours, 1);
        assert_eq!(rest.answered, 1);
        assert_eq!(rest.responses, 1);
    }

    #[test]
    fn snapshots_drop_expired_windows() {
        let now = 10_000;
        let mut u = usage("pr", 5000, 0, now - 1);
        let snap = u.snapshot(now);
        assert!(snap.limits.is_empty());
    }

    #[test]
    fn reports_come_out_newest_first_capped_at_100() {
        let now = 10_000;
        let mut u = Usage::default();
        for i in 0..101 {
            let mut s = state(5000, 4000, now + 1800);
            s.seen = now - 100 + i;
            u.saw(ApiResource::GraphQl, "pr", vec!["o/n".to_owned()], s);
        }
        let snap = u.snapshot(now);
        assert_eq!(snap.reports.len(), 100);
        // The oldest report was dropped; the rest come out newest first.
        assert_eq!(snap.reports[0].at, now - 100 + 100);
        assert_eq!(snap.reports[99].at, now - 100 + 1);
        assert_eq!(snap.reports[0].kind, "pr");
        assert_eq!(snap.reports[0].repos, ["o/n"]);
        assert_eq!(snap.reports[0].limit, 5000);
    }
}
