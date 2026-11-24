//! The Processes tab of a session's terminal panel: the tree of what runs
//! in the session, polled while it shows.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use grenadine_core::api::{ProcessInfo, SessionProcesses};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::debug::bytes;

/// How often the processes are listed while they show.
const POLL_EVERY: Duration = Duration::from_secs(2);

/// Snapshots closer together than this, e.g. because the tab was reopened
/// right away, are too coarse to compare: their times are in 10 ms steps.
const MIN_CPU_WINDOW_MS: u64 = 500;

/// Snapshots further apart than this, e.g. because the tab was hidden,
/// don't say how much CPU a process uses now.
const MAX_CPU_WINDOW_MS: u64 = 10_000;

/// What tells a process from all others, even when PIDs are reused: its PID
/// and when it started.
type Key = (u32, u64);

fn key(p: &ProcessInfo) -> Key {
    (p.pid, p.started_ms)
}

/// A row of the tree.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    process: ProcessInfo,
    depth: usize,
    has_children: bool,
    /// How many processes below it are hidden, when it's collapsed.
    hidden: Option<usize>,
}

/// The tree, top to bottom. Processes whose parent isn't listed are roots,
/// which includes daemons such as Bazel's server. Siblings are in the order
/// they started, so that rows keep their places.
fn rows(processes: &[ProcessInfo], collapsed: &HashSet<Key>) -> Vec<Row> {
    let pids: HashSet<u32> = processes.iter().map(|p| p.pid).collect();
    let mut sorted: Vec<&ProcessInfo> = processes.iter().collect();
    sorted.sort_by_key(|p| (p.started_ms, p.pid));
    let mut roots = Vec::new();
    let mut children = HashMap::<u32, Vec<&ProcessInfo>>::new();
    for p in sorted {
        if pids.contains(&p.ppid) {
            children.entry(p.ppid).or_default().push(p);
        } else {
            roots.push(p);
        }
    }
    let mut out = Vec::<Row>::new();
    // What's left to list, with its depth and the row that hides it.
    let mut stack: Vec<(&ProcessInfo, usize, Option<usize>)> =
        roots.into_iter().rev().map(|p| (p, 0, None)).collect();
    while let Some((p, depth, hider)) = stack.pop() {
        let below = children.get(&p.pid).map(Vec::as_slice).unwrap_or_default();
        let below_hider = match hider {
            Some(row) => {
                out[row].hidden = out[row].hidden.map(|n| n + 1);
                hider
            }
            None => {
                let collapse = !below.is_empty() && collapsed.contains(&key(p));
                out.push(Row {
                    process: p.clone(),
                    depth,
                    has_children: !below.is_empty(),
                    hidden: collapse.then_some(0),
                });
                collapse.then_some(out.len() - 1)
            }
        };
        stack.extend(below.iter().rev().map(|&c| (c, depth + 1, below_hider)));
    }
    out
}

/// How much of one core each process used between the snapshots, in
/// percent, for the ones in both. `None` if they're too close together to
/// tell.
fn cpu_percents(before: &SessionProcesses, after: &SessionProcesses) -> Option<HashMap<Key, f64>> {
    // The clock starts over when the server's machine reboots.
    let Some(elapsed) = after.now_ms.checked_sub(before.now_ms) else {
        return Some(HashMap::new());
    };
    if elapsed < MIN_CPU_WINDOW_MS {
        return None;
    }
    if elapsed > MAX_CPU_WINDOW_MS {
        return Some(HashMap::new());
    }
    let used: HashMap<Key, u64> = before
        .processes
        .iter()
        .flatten()
        .map(|p| (key(p), p.cpu_ms))
        .collect();
    let percents = after
        .processes
        .iter()
        .flatten()
        .filter_map(|p| {
            let used = p.cpu_ms.saturating_sub(*used.get(&key(p))?);
            Some((key(p), used as f64 / elapsed as f64 * 100.0))
        })
        .collect();
    Some(percents)
}

/// How long something takes or took, e.g. "0:07", "1:02:03" or "2d 03:04".
fn elapsed(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (days, h) {
        (0, 0) => format!("{m}:{s:02}"),
        (0, _) => format!("{h}:{m:02}:{s:02}"),
        _ => format!("{days}d {h:02}:{m:02}"),
    }
}

/// The session's processes; polled while `active` and the page is visible.
#[component]
pub fn ProcessesView(session: String, active: Signal<bool>) -> impl IntoView {
    let snapshot = RwSignal::new(None::<SessionProcesses>);
    let cpu = RwSignal::new(HashMap::<Key, f64>::new());
    // The snapshot the next one's CPU use is worked out from.
    let baseline = StoredValue::new(None::<SessionProcesses>);
    let error = RwSignal::new(None::<String>);
    let collapsed = RwSignal::new(HashSet::<Key>::new());
    let loading = StoredValue::new(false);

    let session = StoredValue::new(session);
    let load = move || {
        if document().hidden() || loading.get_value() {
            return;
        }
        loading.set_value(true);
        spawn_local(async move {
            let Some(id) = session.try_get_value() else {
                return;
            };
            let result = api::processes(&id).await;
            // The page may be gone by now.
            if loading.try_set_value(false).is_some() {
                return;
            }
            match result {
                Ok(new) => {
                    let percents = baseline.with_value(|old| match old {
                        Some(old) => cpu_percents(old, &new),
                        None => Some(HashMap::new()),
                    });
                    if let Some(percents) = percents {
                        cpu.set(percents);
                        baseline.set_value(Some(new.clone()));
                    }
                    if let Some(listed) = &new.processes {
                        let live: HashSet<Key> = listed.iter().map(key).collect();
                        if collapsed.with_untracked(|c| !c.is_subset(&live)) {
                            collapsed.update(|c| c.retain(|k| live.contains(k)));
                        }
                    }
                    snapshot.set(Some(new));
                    error.set(None);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    Effect::new(move |_| {
        if !active.get() {
            return;
        }
        load();
        let interval = set_interval_with_handle(load, POLL_EVERY);
        let on_visible = window_event_listener_untyped("visibilitychange", move |_| load());
        on_cleanup(move || {
            if let Ok(i) = interval {
                i.clear();
            }
            on_visible.remove();
        });
    });

    let tree = Memo::new(move |_| {
        snapshot.with(|s| {
            let processes = s.as_ref().and_then(|s| s.processes.as_deref());
            collapsed.with(|c| rows(processes.unwrap_or_default(), c))
        })
    });
    // The rows by key, so that an update changes rows in place, which keeps
    // the focus and a text selection.
    let by_key = Memo::new(move |_| {
        tree.with(|t| {
            t.iter()
                .map(|r| (key(&r.process), r.clone()))
                .collect::<HashMap<_, _>>()
        })
    });
    let now_ms = move || snapshot.with(|s| s.as_ref().map_or(0, |s| s.now_ms));
    let note = Memo::new(move |_| {
        snapshot.with(|s| match s.as_ref()?.processes.as_deref() {
            None => Some("Listing the processes needs a systemd user manager."),
            Some([]) => Some("No processes are running."),
            Some(_) => None,
        })
    });
    let toggle = move |k: Key| {
        collapsed.update(|c| {
            if !c.remove(&k) {
                c.insert(k);
            }
        });
    };

    view! {
        <div class="processes" hidden=move || !active.get()>
            {move || error.get().map(|e| view! { <div class="error">{e}</div> })}
            {move || note.get().map(|n| view! { <div class="muted">{n}</div> })}
            <table hidden=move || tree.with(Vec::is_empty)>
                <thead>
                    <tr>
                        <th class="num">"PID"</th>
                        <th class="num" title="Of one core">"CPU"</th>
                        <th class="num" title="Resident memory">"Memory"</th>
                        <th class="num" title="Since it started">"Time"</th>
                        <th>"Command"</th>
                    </tr>
                </thead>
                <tbody>
                    <For
                        each=move || tree.with(|t| t.iter().map(|r| key(&r.process)).collect::<Vec<_>>())
                        key=|k| *k
                        children=move |k| {
                            // `None` while the row is being removed.
                            let row = Memo::new(move |_| by_key.with(|m| m.get(&k).cloned()));
                            let get = move || row.get().unwrap_or_else(|| Row {
                                process: ProcessInfo::default(),
                                depth: 0,
                                has_children: false,
                                hidden: None,
                            });
                            let command = Memo::new(move |_| get().process.command);
                            let has_children = Memo::new(move |_| get().has_children);
                            let terminal = Memo::new(move |_| get().process.terminal);
                            let hidden = Memo::new(move |_| get().hidden);
                            let depth = Memo::new(move |_| get().depth);
                            view! {
                                <tr data-pid=k.0>
                                    <td class="num">{k.0}</td>
                                    <td class="num">{move || cpu.with(|c| c.get(&k).map(|c| format!("{c:.0}%")))}</td>
                                    <td class="num">{move || bytes(get().process.rss_bytes)}</td>
                                    <td class="num">{move || elapsed(now_ms().saturating_sub(k.1))}</td>
                                    <td
                                        class="process-command"
                                        title=move || command.get()
                                        style:padding-left=move || format!("{}em", depth.get() as f64 * 1.4)
                                    >
                                        {move || if has_children.get() {
                                            view! {
                                                <button
                                                    class="process-toggle"
                                                    aria-expanded=move || hidden.get().is_none().to_string()
                                                    aria-label=format!("Processes below {}", k.0)
                                                    on:click=move |_| toggle(k)
                                                >{move || if hidden.get().is_none() { "▾" } else { "▸" }}</button>
                                            }.into_any()
                                        } else {
                                            view! { <span class="process-toggle"></span> }.into_any()
                                        }}
                                        {move || hidden.get().map(|n| view! {
                                            <span class="process-hidden">{format!("({n} hidden) ")}</span>
                                        })}
                                        {move || terminal.get().map(|t| view! {
                                            <span class="process-terminal">{format!("Terminal {t}: ")}</span>
                                        })}
                                        {move || command.get()}
                                    </td>
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, ppid: u32, started_ms: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            started_ms,
            ..ProcessInfo::default()
        }
    }

    fn listed(processes: &[ProcessInfo], collapsed: &[Key]) -> Vec<(u32, usize, Option<usize>)> {
        rows(processes, &collapsed.iter().copied().collect())
            .into_iter()
            .map(|r| (r.process.pid, r.depth, r.hidden))
            .collect()
    }

    #[test]
    fn trees() {
        let processes = [
            // A daemon, whose parent is init.
            process(5, 1, 50),
            process(6, 5, 60),
            // The agent, which the server outside the session started.
            process(10, 2, 10),
            // Siblings: the later one has the lower PID.
            process(3, 10, 40),
            process(11, 10, 20),
            process(12, 11, 30),
        ];
        assert_eq!(
            listed(&processes, &[]),
            [
                (10, 0, None),
                (11, 1, None),
                (12, 2, None),
                (3, 1, None),
                (5, 0, None),
                (6, 1, None),
            ]
        );
        // Collapsing counts all levels; a process without children and a
        // reused PID aren't collapsed.
        assert_eq!(
            listed(&processes, &[(10, 10), (11, 20), (6, 60), (5, 49)]),
            [(10, 0, Some(3)), (5, 0, None), (6, 1, None)]
        );
        assert_eq!(listed(&[], &[]), []);
    }

    #[test]
    fn cpu() {
        let snapshot = |now_ms, cpu: &[(u32, u64, u64)]| SessionProcesses {
            now_ms,
            processes: Some(
                cpu.iter()
                    .map(|&(pid, started_ms, cpu_ms)| ProcessInfo {
                        cpu_ms,
                        ..process(pid, 1, started_ms)
                    })
                    .collect(),
            ),
        };
        let before = snapshot(1000, &[(1, 10, 500), (2, 10, 0)]);
        // 2's PID is reused, 3 is new.
        let after = snapshot(3000, &[(1, 10, 4500), (2, 2000, 100), (3, 2500, 7)]);
        assert_eq!(
            cpu_percents(&before, &after),
            Some(HashMap::from([((1, 10), 200.0)]))
        );
        // Too close together to tell.
        assert_eq!(cpu_percents(&before, &before), None);
        let soon = snapshot(1499, &[(1, 10, 510)]);
        assert_eq!(cpu_percents(&before, &soon), None);
        let enough = snapshot(1500, &[(1, 10, 510)]);
        assert_eq!(
            cpu_percents(&before, &enough),
            Some(HashMap::from([((1, 10), 2.0)]))
        );
        // The machine rebooted.
        assert_eq!(cpu_percents(&after, &before), Some(HashMap::new()));
        let late = snapshot(60_000, &[(1, 10, 4500)]);
        assert_eq!(cpu_percents(&before, &late), Some(HashMap::new()));
    }

    #[test]
    fn elapsed_times() {
        assert_eq!(elapsed(7_900), "0:07");
        assert_eq!(elapsed(3_723_000), "1:02:03");
        assert_eq!(elapsed(183_840_000), "2d 03:04");
    }
}
