//! The Debug tab: the machine's and the agent sessions' resource use,
//! what grenadine spent on GitHub over the last hour, and where GitHub's
//! rate limits stand.

use std::time::Duration;

use grenadine_core::api::{
    ApiResource, DiskUsage, GitHubUsage, MinuteUsage, RateLimitInfo, RateLimitReport,
    RequestKindUsage, RequestPriority, SessionResources, SystemStats,
};
use leptos::prelude::*;

use crate::{
    Updates,
    agents::status_label,
    api,
    topbar::{hm, hms},
};

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

/// `n` bytes in binary units, e.g. "512 MiB"; GiB and up get one
/// decimal, e.g. "11.3 GiB".
fn bytes(n: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    const TIB: u64 = 1 << 40;
    match n {
        n if n >= TIB => format!("{:.1} TiB", n as f64 / TIB as f64),
        n if n >= GIB => format!("{:.1} GiB", n as f64 / GIB as f64),
        n if n >= MIB => format!("{} MiB", n / MIB),
        n if n >= KIB => format!("{} KiB", n / KIB),
        n => format!("{n} B"),
    }
}

/// How long ago `started` was, e.g. "up 3 h 12 min".
fn uptime(now: i64, started: i64) -> String {
    let mins = (now - started).max(0) / 60;
    match mins {
        m if m >= 24 * 60 => format!("up {} d {} h", m / (24 * 60), m % (24 * 60) / 60),
        m if m >= 60 => format!("up {} h {} min", m / 60, m % 60),
        m => format!("up {m} min"),
    }
}

/// A label with a usage-meter bar of `part` out of `total`.
#[component]
fn Metered(label: String, part: u64, total: u64) -> impl IntoView {
    let width = part
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100);
    view! {
        <p>{label}</p>
        <div class="usage-meter">
            <div class="usage-meter-ours" style:width=format!("{width}%")></div>
        </div>
    }
}

/// The machine's and the sessions' resource use.
#[component]
fn SystemView(stats: SystemStats) -> impl IntoView {
    let mem = &stats.memory;
    let mem_used = mem.total_bytes.saturating_sub(mem.available_bytes);
    let swap_used = mem.swap_total_bytes.saturating_sub(mem.swap_free_bytes);
    let pressure: Vec<String> = [
        ("CPU", stats.pressure.cpu),
        ("memory", stats.pressure.memory),
        ("I/O", stats.pressure.io),
    ]
    .into_iter()
    .filter_map(|(name, v)| v.map(|v| format!("{name} {v:.1}%")))
    .collect();
    let server = &stats.server;
    let now = stats.now;
    view! {
        <Metered
            label=format!("RAM: {} of {} used · {} cache",
                bytes(mem_used), bytes(mem.total_bytes), bytes(mem.cached_bytes))
            part=mem_used total=mem.total_bytes
        />
        {(mem.swap_total_bytes > 0).then(|| view! {
            <Metered
                label=format!("Swap: {} of {} used", bytes(swap_used), bytes(mem.swap_total_bytes))
                part=swap_used total=mem.swap_total_bytes
            />
        })}
        {stats.disks.into_iter().map(|d: DiskUsage| {
            let used = d.total_bytes.saturating_sub(d.free_bytes);
            view! {
                <Metered
                    label=format!("{}: {} free of {}", d.label, bytes(d.free_bytes), bytes(d.total_bytes))
                    part=used total=d.total_bytes
                />
            }
        }).collect_view()}
        <p>{format!("Load: {:.2} / {:.2} / {:.2} on {} cores",
            stats.load.load1, stats.load.load5, stats.load.load15, stats.load.cpus)}</p>
        {(!pressure.is_empty()).then(|| view! {
            <p>{format!("Pressure (stalled, last 10 s): {}", pressure.join(" · "))}</p>
        })}
        <p>{format!("Server: RSS {} · {} threads · {} open files · {}",
            bytes(server.rss_bytes), server.threads, server.open_fds,
            uptime(now, server.started_at))}</p>
        {match stats.sessions {
            None => view! {
                <p class="muted">"Per-session stats need a systemd user manager."</p>
            }.into_any(),
            Some(sessions) => view! { <SessionsView sessions leftover=stats.leftover /> }.into_any(),
        }}
    }
}

/// The sessions' slices' counters and what unknown slices add up to.
#[component]
fn SessionsView(
    sessions: Vec<SessionResources>,
    leftover: Option<grenadine_core::api::LeftoverResources>,
) -> impl IntoView {
    let mut process_total = 0;
    let mut cache_total = 0;
    let mut processes_total = 0;
    let mut cpu_total = 0.0;
    let mut any_cpu = false;
    for s in &sessions {
        process_total += s.process_bytes;
        cache_total += s.cache_bytes;
        processes_total += s.processes;
        if let Some(c) = s.cpu_percent {
            cpu_total += c;
            any_cpu = true;
        }
    }
    let leftover_row = leftover.map(|l| {
        process_total += l.process_bytes;
        cache_total += l.cache_bytes;
        processes_total += l.processes;
        view! {
            <tr>
                <td>{format!("Deleted sessions ({} slices)", l.slices)}</td>
                <td></td>
                <td class="num">{bytes(l.process_bytes)}</td>
                <td class="num">{bytes(l.cache_bytes)}</td>
                <td class="num">{l.processes}</td>
                <td class="num">"—"</td>
            </tr>
        }
    });
    view! {
        <table class="usage-table usage-sessions">
            <thead>
                <tr>
                    <th>"Session"</th>
                    <th>"Status"</th>
                    <th class="num">"Process memory"</th>
                    <th class="num">"Cache"</th>
                    <th class="num">"Processes"</th>
                    <th class="num">"CPU"</th>
                </tr>
            </thead>
            <tbody>
                {sessions.into_iter().map(|s| view! {
                    <tr>
                        <td><a href=format!("#/agents/{}", s.id) title=s.title.clone()>{
                            let label = s.summary.clone().unwrap_or_else(|| s.title.clone());
                            if label.is_empty() { s.id.clone() } else { label }
                        }</a></td>
                        <td>{status_label(s.status)}</td>
                        <td class="num">{bytes(s.process_bytes)}</td>
                        <td class="num">{bytes(s.cache_bytes)}</td>
                        <td class="num">{s.processes}</td>
                        <td class="num">{s.cpu_percent.map(|c| format!("{c:.0}%")).unwrap_or_else(|| "—".into())}</td>
                    </tr>
                }).collect_view()}
                {leftover_row}
                <tr class="usage-total">
                    <td>"Total"</td>
                    <td></td>
                    <td class="num">{bytes(process_total)}</td>
                    <td class="num">{bytes(cache_total)}</td>
                    <td class="num">{processes_total}</td>
                    <td class="num">{if any_cpu { format!("{cpu_total:.0}%") } else { "—".into() }}</td>
                </tr>
            </tbody>
        </table>
    }
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
                {format!("Answered {} of the last {} {name} responses.",
                    info.answered, info.responses)}
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

/// The last responses' rate-limit headers and the repos the requests
/// named.
#[component]
fn ReportsView(reports: Vec<RateLimitReport>) -> impl IntoView {
    view! {
        <h2>"Recent rate-limit reports"</h2>
        <p class="muted">"The last 100 responses, newest first."</p>
        <table class="usage-table usage-reports">
            <thead>
                <tr>
                    <th>"Time"</th>
                    <th>"API"</th>
                    <th>"Request"</th>
                    <th>"Repos"</th>
                    <th class="num">"Used"</th>
                    <th class="num">"Remaining"</th>
                    <th class="num">"Limit"</th>
                    <th>"Resets"</th>
                </tr>
            </thead>
            <tbody>
                {reports.into_iter().map(|r| view! {
                    <tr>
                        <td>{hms(r.at)}</td>
                        <td>{resource_name(r.resource)}</td>
                        <td>{kind_label(&r.kind).to_owned()}</td>
                        <td>{match r.repos.is_empty() {
                            true => "—".to_owned(),
                            false => r.repos.join(", "),
                        }}</td>
                        <td class="num">{thousands(r.used)}</td>
                        <td class="num">{thousands(r.remaining)}</td>
                        <td class="num">{thousands(r.limit)}</td>
                        <td>{hm(r.reset)}</td>
                    </tr>
                }).collect_view()}
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
    let system = LocalResource::new(move || {
        updates.sync.track();
        tick.track();
        api::system_stats()
    });
    view! {
        <div class="debug">
            <h1>"System"</h1>
            {move || match system.get() {
                None => view! { <p class="muted">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                Some(Ok(s)) => view! { <SystemView stats=s /> }.into_any(),
            }}
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
        {(!usage.reports.is_empty()).then(|| view! { <ReportsView reports=usage.reports /> })}
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

    #[test]
    fn bytes_binary_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2048), "2 KiB");
        assert_eq!(bytes(512 << 20), "512 MiB");
        assert_eq!(bytes(11 * (1 << 30) + 322_122_547), "11.3 GiB");
        assert_eq!(bytes(3 << 40), "3.0 TiB");
    }
}
