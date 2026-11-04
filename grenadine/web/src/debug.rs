//! The Debug tab: what grenadine spent on GitHub over the last hour,
//! and where GitHub's rate limits stand.

use std::time::Duration;

use grenadine_core::api::{
    ApiResource, GitHubUsage, MinuteUsage, RateLimitInfo, RequestKindUsage, RequestPriority,
};
use leptos::prelude::*;

use crate::{Updates, api, topbar::hm};

/// `n` with thousands separators, e.g. "5,000".
fn thousands(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn resource_name(r: ApiResource) -> &'static str {
    match r {
        ApiResource::GraphQl => "GraphQL",
        ApiResource::Rest => "REST",
    }
}

/// The unit a resource's budget counts in.
fn resource_unit(r: ApiResource) -> &'static str {
    match r {
        ApiResource::GraphQl => "points",
        ApiResource::Rest => "requests",
    }
}

fn priority_label(p: RequestPriority) -> &'static str {
    match p {
        RequestPriority::User => "your actions",
        RequestPriority::Search => "inbox searches",
        RequestPriority::Background => "background syncs",
    }
}

/// Who asked for the request, for the table's "Started by" column.
fn started_by(p: RequestPriority) -> &'static str {
    match p {
        RequestPriority::User => "You",
        RequestPriority::Search => "Inbox search",
        RequestPriority::Background => "Background sync",
    }
}

/// What a request kind is called for the table's "Request" column;
/// unknown kinds show raw.
fn kind_label(kind: &str) -> &str {
    match kind {
        "search" => "Inbox search",
        "pr" => "PR details",
        "review_threads" => "Review threads",
        "prs_by_ref" => "Stack lookup",
        "users" => "User names",
        "commit_authors" => "Commit authors",
        "pr_statuses" => "Agent PR states",
        "pr_node" => "PR lookup for an action",
        "mutation" => "Change on GitHub",
        "activity" => "Push history",
        "review_comments" => "Review comments",
        "create_pr" => "Open PR",
        other => other,
    }
}

/// One resource's limit: what's left, a bar of who spent it, and when
/// the tiers pause.
#[component]
fn LimitView(info: RateLimitInfo, now: i64) -> impl IntoView {
    let unit = resource_unit(info.resource);
    let name = resource_name(info.resource);
    let other = info.used.saturating_sub(info.ours);
    let in_min = ((info.reset - now).max(0) + 59) / 60;
    let paused = (!info.paused.is_empty()).then(|| {
        info.paused
            .iter()
            .map(|&p| priority_label(p))
            .collect::<Vec<_>>()
            .join(", ")
    });
    view! {
        <section class="usage-limit">
            <p>
                {format!("{name}: {} of {} {unit} left · resets {} (in {in_min} min)",
                    thousands(info.remaining), thousands(info.limit), hm(info.reset))}
            </p>
            <div class="usage-meter" title=format!(
                "Grenadine {} · Other clients on this token {}",
                thousands(info.ours), thousands(other),
            )>
                <div
                    class="usage-meter-ours"
                    style:width=format!("{}%", pct(info.ours, info.limit))
                ></div>
                <div
                    class="usage-meter-other"
                    style:width=format!("{}%", pct(other, info.limit))
                ></div>
            </div>
            <p class="muted">
                {format!("Grenadine {} · Other clients on this token {}",
                    thousands(info.ours), thousands(other))}
            </p>
            <p class="muted">
                {format!("Used by: {}", info.kinds.iter().map(|k| kind_label(k)).collect::<Vec<_>>().join(", "))}
            </p>
            <p class="muted">
                {format!("Background syncs pause below {} left; inbox searches below {}.",
                    thousands(info.limit / 5), thousands(info.limit / 10))}
            </p>
            {paused.map(|p| view! { <p class="usage-paused">{format!("Paused now: {p}")}</p> })}
        </section>
    }
}

/// `part` as a whole percent of `total`.
fn pct(part: u32, total: u32) -> u32 {
    part.saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100)
}

/// The last hour's GraphQL points, a flex bar per minute.
#[component]
fn MinutesView(minutes: Vec<MinuteUsage>) -> impl IntoView {
    let max = minutes.iter().map(|m| m.graphql_points).max().unwrap_or(0);
    view! {
        <h2>"Last 60 minutes"</h2>
        <p class="muted">{format!("GraphQL points per minute; peak {}.", thousands(max))}</p>
        <div class="usage-chart">
            {minutes.into_iter().map(|m| {
                let height = (m.graphql_points * 100).checked_div(max).unwrap_or(0).max(2);
                view! {
                    <div class="usage-bar" style:height=format!("{height}%")
                        title=format!("{} — {} points", hm(m.start), thousands(m.graphql_points))
                    ></div>
                }
            }).collect_view()}
        </div>
    }
}

/// The last hour's requests, grouped by what they were for.
#[component]
fn KindsView(kinds: Vec<RequestKindUsage>) -> impl IntoView {
    let totals = kinds.iter().fold((0u32, 0u32, 0u32), |(r, p, d), k| {
        (r + k.requests, p + k.points.unwrap_or(0), d + k.deferred)
    });
    let row = |k: RequestKindUsage| {
        let per_request = k.points.map(|p| {
            let sent = k.requests - k.deferred;
            match sent {
                0 => "—".to_owned(),
                _ => format!("{:.1}", p as f64 / sent as f64),
            }
        });
        view! {
            <tr>
                <td>{kind_label(&k.kind).to_owned()}</td>
                <td>{resource_name(k.resource)}</td>
                <td>{started_by(k.priority)}</td>
                <td class="num">{k.requests}</td>
                <td class="num">{k.points.map(thousands).unwrap_or_else(|| "—".into())}</td>
                <td class="num">{per_request.unwrap_or_else(|| "—".into())}</td>
                <td class="num">{k.deferred}</td>
            </tr>
        }
    };
    view! {
        <h2>"Requests in the last hour"</h2>
        <table class="usage-table">
            <thead>
                <tr>
                    <th>"Request"</th>
                    <th>"API"</th>
                    <th>"Started by"</th>
                    <th class="num">"Requests"</th>
                    <th class="num">"Points"</th>
                    <th class="num">"Points/request"</th>
                    <th class="num">"Deferred"</th>
                </tr>
            </thead>
            <tbody>
                {kinds.into_iter().map(row).collect_view()}
                <tr class="usage-total">
                    <td>"Total"</td>
                    <td></td>
                    <td></td>
                    <td class="num">{totals.0}</td>
                    <td class="num">{thousands(totals.1)}</td>
                    <td class="num"></td>
                    <td class="num">{totals.2}</td>
                </tr>
            </tbody>
        </table>
    }
}

/// The tab's page.
#[component]
pub fn Debug() -> impl IntoView {
    let updates = expect_context::<Updates>();
    let tick = RwSignal::new(0u64);
    let handle =
        set_interval_with_handle(move || tick.update(|t| *t += 1), Duration::from_secs(10));
    on_cleanup(move || {
        if let Ok(h) = handle {
            h.clear();
        }
    });
    let usage = LocalResource::new(move || {
        updates.sync.track();
        tick.track();
        api::github_usage()
    });
    view! {
        <div class="debug">
            <h1>"GitHub API usage"</h1>
            {move || match usage.get() {
                None => view! { <p class="muted">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                Some(Ok(u)) => view! { <UsageView usage=u /> }.into_any(),
            }}
        </div>
    }
}

#[component]
fn UsageView(usage: GitHubUsage) -> impl IntoView {
    let now = usage.now;
    view! {
        {usage.limits.into_iter().map(|l| view! { <LimitView info=l now=now /> }).collect_view()}
        {if usage.kinds.is_empty() {
            view! { <p class="muted">"No GitHub requests yet."</p> }.into_any()
        } else {
            view! {
                <MinutesView minutes=usage.minutes />
                <KindsView kinds=usage.kinds />
            }.into_any()
        }}
        <p class="muted">"Counts start over when the server restarts."</p>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(12), "12");
        assert_eq!(thousands(1234), "1,234");
        assert_eq!(thousands(5000), "5,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
