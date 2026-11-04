//! Keeps grenadine inside GitHub's rate limits, and records what it
//! spends.
//!
//! Requests run under a priority from a task-local: the viewer's own
//! actions always go, inbox searches pause below a tenth of the limit,
//! background syncs below a fifth. Every response's `x-ratelimit-*`
//! headers update the last known window, and every request lands in an
//! hour-long log that the Debug tab reads.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;

use grenadine_core::api::{
    ApiResource, GitHubUsage, MinuteUsage, RateLimitInfo, RequestKindUsage, RequestPriority,
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

/// A resource's limit as the last response's headers reported it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimitState {
    pub limit: u32,
    pub remaining: u32,
    pub used: u32,
    /// Unix seconds when the window resets.
    pub reset: i64,
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
}

/// What a `GitHub` has seen of its limits and spent. In memory only; it
/// starts over on a restart.
#[derive(Default)]
pub struct Usage {
    states: BTreeMap<ApiResource, LimitState>,
    log: VecDeque<Entry>,
}

/// How long the log and the `ours` counts look back.
const WINDOW: i64 = 3600;

impl Usage {
    /// Records `state` for `resource`, from a response's headers.
    pub fn saw(&mut self, resource: ApiResource, state: LimitState) {
        self.states.insert(resource, state);
    }

    /// The last known state of `resource`'s limit.
    pub fn state(&self, resource: ApiResource) -> Option<LimitState> {
        self.states.get(&resource).copied()
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

    /// Lets `priority`'s request of `kind` go, or refuses it when the
    /// window's remainder is below the tier's reserve. A refused request
    /// is logged as deferred and must not be sent. Once the reset has
    /// passed everything goes again; the next response refreshes the
    /// state.
    pub fn admit(
        &mut self,
        kind: &'static str,
        resource: ApiResource,
        priority: RequestPriority,
        now: i64,
    ) -> Result<(), RateLimited> {
        if let Some(until) = self.denied(resource, priority, now) {
            self.log(Entry {
                at: now,
                kind,
                resource,
                priority,
                points: None,
                deferred: true,
            });
            return Err(RateLimited { resource, until });
        }
        Ok(())
    }

    /// The reset time when `priority`'s request on `resource` would be
    /// refused now, else `None`.
    fn denied(&self, resource: ApiResource, priority: RequestPriority, now: i64) -> Option<i64> {
        let s = self.states.get(&resource)?;
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

    /// The reset time when `priority` is paused on a resource it uses,
    /// the latest if several are, else `None`. Searches only spend
    /// GraphQL points, so a spent REST limit doesn't pause them.
    pub fn paused_until(&self, priority: RequestPriority, now: i64) -> Option<i64> {
        const BOTH: [ApiResource; 2] = [ApiResource::GraphQl, ApiResource::Rest];
        let resources: &[ApiResource] = match priority {
            RequestPriority::Search => &[ApiResource::GraphQl],
            _ => &BOTH,
        };
        resources
            .iter()
            .filter_map(|&r| self.denied(r, priority, now))
            .max()
    }

    /// What the Debug tab shows: the known limits, the last hour grouped
    /// by kind and the last hour minute by minute.
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
        let limits = self
            .states
            .iter()
            .map(|(&resource, s)| {
                let since = s.reset - WINDOW;
                let ours = self
                    .log
                    .iter()
                    .filter(|e| e.resource == resource && !e.deferred && e.at >= since)
                    .map(|e| match resource {
                        ApiResource::GraphQl => e.points.unwrap_or(0),
                        ApiResource::Rest => 1,
                    })
                    .sum();
                RateLimitInfo {
                    resource,
                    limit: s.limit,
                    remaining: s.remaining,
                    used: s.used,
                    reset: s.reset,
                    ours,
                    paused: TIERS
                        .into_iter()
                        .filter(|&p| self.denied(resource, p, now).is_some())
                        .collect(),
                }
            })
            .collect();
        GitHubUsage {
            now,
            limits,
            kinds,
            minutes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(limit: u32, remaining: u32, reset: i64) -> Usage {
        let mut u = Usage::default();
        u.saw(
            ApiResource::GraphQl,
            LimitState {
                limit,
                remaining,
                used: limit - remaining,
                reset,
            },
        );
        u
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
        let mut u = usage(5000, 1001, reset);
        for p in [
            RequestPriority::User,
            RequestPriority::Search,
            RequestPriority::Background,
        ] {
            admit(&mut u, p, now).unwrap();
        }
        let mut u = usage(5000, 1000, reset);
        admit(&mut u, RequestPriority::Background, now).unwrap_err();
        admit(&mut u, RequestPriority::Search, now).unwrap();
        let mut u = usage(5000, 500, reset);
        admit(&mut u, RequestPriority::Search, now).unwrap_err();
        admit(&mut u, RequestPriority::User, now).unwrap();
        let mut u = usage(5000, 0, reset);
        admit(&mut u, RequestPriority::User, now).unwrap_err();

        // Past the reset everything goes again.
        let mut u = usage(5000, 0, reset);
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
        let mut u = usage(5000, 1000, 3700);
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
        let u = usage(5000, 700, 3700);
        assert_eq!(u.paused_until(RequestPriority::Background, 100), Some(3700));
        assert_eq!(u.paused_until(RequestPriority::Search, 100), None);
        assert_eq!(u.paused_until(RequestPriority::Background, 3700), None);

        // A spent REST limit doesn't pause searches: they only spend
        // GraphQL points.
        let mut u = Usage::default();
        u.saw(
            ApiResource::Rest,
            LimitState {
                limit: 60,
                remaining: 0,
                used: 60,
                reset: 3700,
            },
        );
        assert_eq!(u.paused_until(RequestPriority::Search, 100), None);
        assert_eq!(u.paused_until(RequestPriority::Background, 100), Some(3700));
    }

    #[test]
    fn snapshots_group_sort_and_prune() {
        let now = 10_000;
        let mut u = usage(5000, 4500, now + 1800);
        // Older than an hour: gone.
        u.log(Entry {
            at: now - 3600,
            kind: "pr",
            resource: ApiResource::GraphQl,
            priority: RequestPriority::Background,
            points: Some(100),
            deferred: false,
        });
        // Inside both the log's hour and the limit's window (reset-3600).
        let at = now - 1000;
        for (kind, priority, points) in [
            ("pr", RequestPriority::Background, 4),
            ("pr", RequestPriority::Background, 2),
            ("search", RequestPriority::Search, 3),
            ("activity", RequestPriority::Background, 0),
        ] {
            u.log(Entry {
                at,
                kind,
                resource: if kind == "activity" {
                    ApiResource::Rest
                } else {
                    ApiResource::GraphQl
                },
                priority,
                points: (kind != "activity").then_some(points),
                deferred: false,
            });
        }
        u.saw(
            ApiResource::Rest,
            LimitState {
                limit: 60,
                remaining: 55,
                used: 5,
                reset: now + 1800,
            },
        );
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

        // `ours` counts this window only.
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
}
