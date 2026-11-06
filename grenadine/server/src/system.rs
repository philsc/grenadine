//! The machine's and the sessions' resource use for `/api/debug/system`:
//! /proc, statvfs, and each session's cgroup under `grenadine-agent.slice`.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Instant;

use grenadine_core::api::{
    AgentSession, DiskUsage, LeftoverResources, LoadStats, MemoryStats, PressureStats, ServerStats,
    SessionResources, SystemStats,
};

use crate::sync::State;

/// What the endpoint remembers across requests: where the agent slices'
/// cgroup is and the CPU counters' last reading, which a percent needs
/// two of.
#[derive(Default)]
pub struct Sampler {
    /// `/sys/fs/cgroup/…/grenadine-agent.slice`, once `systemctl` has
    /// answered. `None` while unresolved, so requests keep retrying.
    cgroup: Mutex<Option<PathBuf>>,
    cpu: Mutex<Option<CpuSample>>,
}

/// The usage_usec counters at one request and the sessions they were
/// turned into. A request within `CPU_CACHE` of it gets that answer
/// again rather than a near-infinite percent.
struct CpuSample {
    at: Instant,
    /// usage_usec per slice directory name.
    usage_usec: HashMap<String, u64>,
    rendered: (Vec<SessionResources>, Option<LeftoverResources>),
}

/// How long a sessions/leftover answer stays cached; shorter gaps would
/// inflate tiny CPU deltas into huge percents.
const CPU_CACHE: std::time::Duration = std::time::Duration::from_secs(2);

/// Where `grenadine-agent.slice`'s cgroup directory is; `None` without
/// slices or when systemd doesn't know the slice.
pub async fn agent_cgroup(state: &State) -> Option<PathBuf> {
    if !state.agents.slices() {
        return None;
    }
    if let Some(dir) = state.system.cgroup.lock().unwrap().clone() {
        return Some(dir);
    }
    let out = tokio::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=ControlGroup",
            "--value",
            "grenadine-agent.slice",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    let cgroup = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if cgroup.is_empty() {
        return None;
    }
    let dir = Path::new("/sys/fs/cgroup").join(cgroup.trim_start_matches('/'));
    if !dir.is_dir() {
        return None;
    }
    *state.system.cgroup.lock().unwrap() = Some(dir.clone());
    Some(dir)
}

/// The whole snapshot. Runs on a blocking thread; `cgroup` is what
/// `agent_cgroup` resolved and `sessions` the database's sessions.
pub fn sample(state: &State, sessions: &[AgentSession], cgroup: Option<&Path>) -> SystemStats {
    let memory = std::fs::read_to_string("/proc/meminfo")
        .map(|s| parse_meminfo(&s))
        .unwrap_or_default();
    let (load1, load5, load15) = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| parse_loadavg(&s))
        .unwrap_or_default();
    let load = LoadStats {
        load1,
        load5,
        load15,
        cpus: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(0),
    };
    let pressure = PressureStats {
        cpu: pressure_avg10("/proc/pressure/cpu"),
        memory: pressure_avg10("/proc/pressure/memory"),
        io: pressure_avg10("/proc/pressure/io"),
    };
    let (sessions, leftover) = cgroup
        .map(|dir| session_usage(state, dir, sessions))
        .unwrap_or_default();
    SystemStats {
        now: crate::agent::now(),
        memory,
        load,
        pressure,
        disks: disks(state),
        server: server_stats(state),
        sessions: cgroup.map(|_| sessions),
        leftover,
    }
}

/// A path per interesting filesystem, deduplicated by device so two
/// labels on one filesystem merge into one entry.
fn disks(state: &State) -> Vec<DiskUsage> {
    let candidates = [
        ("Worktrees", state.agents.worktrees().to_owned()),
        ("Database", state.db_dir.clone()),
    ];
    let mut disks: Vec<(u64, DiskUsage)> = Vec::new();
    for (label, path) in candidates {
        let Ok(dev) = std::fs::metadata(&path).map(|m| m.dev()) else {
            continue;
        };
        if let Some((_, d)) = disks.iter_mut().find(|(other, _)| *other == dev) {
            d.label = format!("{}, {}", d.label, label);
            continue;
        }
        if let Some(s) = statvfs(&path) {
            disks.push((
                dev,
                DiskUsage {
                    label: label.to_owned(),
                    path: path.display().to_string(),
                    total_bytes: s.total,
                    free_bytes: s.free,
                },
            ));
        }
    }
    disks.into_iter().map(|(_, d)| d).collect()
}

/// statvfs's block counts turned into bytes.
fn statvfs(path: &Path) -> Option<StatVfs> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `st` is a valid out-pointer; statvfs has no preconditions.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(StatVfs {
        total: st.f_blocks * st.f_frsize,
        free: st.f_bavail * st.f_frsize,
    })
}

struct StatVfs {
    total: u64,
    free: u64,
}

/// The server process's own /proc/self numbers.
fn server_stats(state: &State) -> ServerStats {
    let (rss_bytes, threads) = std::fs::read_to_string("/proc/self/status")
        .map(|s| parse_status(&s))
        .unwrap_or_default();
    ServerStats {
        rss_bytes,
        threads,
        open_fds: std::fs::read_dir("/proc/self/fd")
            .map(|d| d.count() as u32)
            .unwrap_or(0),
        started_at: state.started_at,
    }
}

fn pressure_avg10(path: &str) -> Option<f64> {
    parse_pressure_avg10(&std::fs::read_to_string(path).ok()?)
}

/// `MemTotal`-style kB lines into `MemoryStats`.
fn parse_meminfo(s: &str) -> MemoryStats {
    fn kb(s: &str, key: &str) -> u64 {
        s.lines()
            .find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse().ok())
            .map(|kb: u64| kb * 1024)
            .unwrap_or(0)
    }
    MemoryStats {
        total_bytes: kb(s, "MemTotal:"),
        available_bytes: kb(s, "MemAvailable:"),
        cached_bytes: kb(s, "Cached:"),
        swap_total_bytes: kb(s, "SwapTotal:"),
        swap_free_bytes: kb(s, "SwapFree:"),
    }
}

/// The first three fields of /proc/loadavg.
fn parse_loadavg(s: &str) -> Option<(f64, f64, f64)> {
    let mut f = s.split_whitespace();
    Some((
        f.next()?.parse().ok()?,
        f.next()?.parse().ok()?,
        f.next()?.parse().ok()?,
    ))
}

/// The `some` line's `avg10` value of a /proc/pressure file.
fn parse_pressure_avg10(s: &str) -> Option<f64> {
    s.lines()
        .find(|l| l.starts_with("some "))?
        .split_whitespace()
        .find_map(|f| f.strip_prefix("avg10=")?.parse().ok())
}

/// VmRSS and Threads out of /proc/self/status.
fn parse_status(s: &str) -> (u64, u32) {
    let mut rss = 0;
    let mut threads = 0;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("VmRSS:") {
            rss = v.trim_end_matches("kB").trim().parse::<u64>().unwrap_or(0) * 1024;
        } else if let Some(v) = line.strip_prefix("Threads:") {
            threads = v.trim().parse().unwrap_or(0);
        }
    }
    (rss, threads)
}

/// One slice's memory.stat's process (`anon` + `kernel`) and cache
/// (`file`) bytes.
fn parse_memory_stat(s: &str) -> (u64, u64) {
    fn field(s: &str, key: &str) -> u64 {
        s.lines()
            .find_map(|l| match l.split_once(' ')? {
                (k, v) if k == key => v.trim().parse().ok(),
                _ => None,
            })
            .unwrap_or(0)
    }
    (field(s, "anon") + field(s, "kernel"), field(s, "file"))
}

/// usage_usec out of a slice's cpu.stat.
fn parse_cpu_stat(s: &str) -> u64 {
    s.lines()
        .find_map(|l| l.strip_prefix("usage_usec")?.trim().parse().ok())
        .unwrap_or(0)
}

/// One `grenadine-agent-<id>` slice's counters.
#[derive(Default)]
struct SliceUsage {
    process_bytes: u64,
    cache_bytes: u64,
    processes: u32,
    usage_usec: u64,
}

/// The `grenadine-agent-*.slice` directories under `dir`, keyed by the
/// session ID without dashes that the directory name carries.
fn scan_slices(dir: &Path) -> HashMap<String, (String, SliceUsage)> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name
            .strip_prefix("grenadine-agent-")
            .and_then(|n| n.strip_suffix(".slice"))
        else {
            continue;
        };
        let path = entry.path();
        let stat = std::fs::read_to_string(path.join("memory.stat")).unwrap_or_default();
        let (process_bytes, cache_bytes) = parse_memory_stat(&stat);
        let processes = std::fs::read_to_string(path.join("pids.current"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let usage_usec = std::fs::read_to_string(path.join("cpu.stat"))
            .map(|s| parse_cpu_stat(&s))
            .unwrap_or(0);
        out.insert(
            id.to_owned(),
            (
                name,
                SliceUsage {
                    process_bytes,
                    cache_bytes,
                    processes,
                    usage_usec,
                },
            ),
        );
    }
    out
}

/// `delta` usage microseconds over `elapsed` real microseconds, in
/// percent of one core.
fn cpu_percent(delta_usec: u64, elapsed_usec: u64) -> f64 {
    if elapsed_usec == 0 {
        return 0.0;
    }
    delta_usec as f64 / elapsed_usec as f64 * 100.0
}

/// The sessions' slices' counters, sorted by process memory then cache,
/// plus what slices no session owns add up to. `cpu` maps each slice's
/// directory name to its percent of a core.
fn divide(
    slices: HashMap<String, (String, SliceUsage)>,
    sessions: &[AgentSession],
    cpu: &HashMap<String, Option<f64>>,
) -> (Vec<SessionResources>, Option<LeftoverResources>) {
    let mut leftover = LeftoverResources::default();
    let mut used = Vec::with_capacity(sessions.len());
    let mut slices: HashMap<String, (String, SliceUsage)> = slices;
    for session in sessions {
        let id = session.id.replace('-', "");
        let (name, usage) = match slices.remove(&id) {
            Some((name, usage)) => (Some(name), usage),
            None => (None, SliceUsage::default()),
        };
        used.push(SessionResources {
            id: session.id.clone(),
            title: session.title.clone(),
            summary: session.summary.clone(),
            status: session.status,
            process_bytes: usage.process_bytes,
            cache_bytes: usage.cache_bytes,
            processes: usage.processes,
            cpu_percent: name.and_then(|n| cpu.get(&n).copied().flatten()),
        });
    }
    for (_, usage) in slices.values() {
        leftover.slices += 1;
        leftover.process_bytes += usage.process_bytes;
        leftover.cache_bytes += usage.cache_bytes;
        leftover.processes += usage.processes;
    }
    used.sort_by(|a, b| {
        b.process_bytes
            .cmp(&a.process_bytes)
            .then(b.cache_bytes.cmp(&a.cache_bytes))
    });
    (used, (leftover.slices > 0).then_some(leftover))
}

/// The sessions' part of the snapshot: the slices' current counters,
/// with CPU percents against the previous sample. A sample younger than
/// `CPU_CACHE` is answered from its cache.
fn session_usage(
    state: &State,
    cgroup: &Path,
    sessions: &[AgentSession],
) -> (Vec<SessionResources>, Option<LeftoverResources>) {
    let mut slot = state.system.cpu.lock().unwrap();
    if let Some(prev) = &*slot
        && prev.at.elapsed() < CPU_CACHE
    {
        return prev.rendered.clone();
    }
    let slices = scan_slices(cgroup);
    let at = Instant::now();
    let elapsed_usec = slot
        .as_ref()
        .map(|p| p.at.elapsed().as_micros() as u64)
        .unwrap_or(0);
    let usage_usec: HashMap<String, u64> = slices
        .values()
        .map(|(name, u)| (name.clone(), u.usage_usec))
        .collect();
    let cpu: HashMap<String, Option<f64>> = slices
        .values()
        .map(|(name, u)| {
            let percent = slot.as_ref().and_then(|p| {
                p.usage_usec
                    .get(name)
                    .map(|prev| cpu_percent(u.usage_usec.saturating_sub(*prev), elapsed_usec))
            });
            (name.clone(), percent)
        })
        .collect();
    let rendered = divide(slices, sessions, &cpu);
    *slot = Some(CpuSample {
        at,
        usage_usec,
        rendered: rendered.clone(),
    });
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use grenadine_core::api::AgentStatus;

    #[test]
    fn meminfo() {
        let s = "MemTotal:       32880528 kB\n\
                 MemFree:         1234567 kB\n\
                 MemAvailable:   20000000 kB\n\
                 Cached:         10000000 kB\n\
                 SwapCached:            0 kB\n\
                 SwapTotal:       2097152 kB\n\
                 SwapFree:        1048576 kB\n";
        assert_eq!(
            parse_meminfo(s),
            MemoryStats {
                total_bytes: 32880528 * 1024,
                available_bytes: 20000000 * 1024,
                cached_bytes: 10000000 * 1024,
                swap_total_bytes: 2097152 * 1024,
                swap_free_bytes: 1048576 * 1024,
            }
        );
    }

    #[test]
    fn loadavg() {
        assert_eq!(
            parse_loadavg("1.23 0.98 0.76 2/1234 5678"),
            Some((1.23, 0.98, 0.76))
        );
        assert_eq!(parse_loadavg("garbage"), None);
    }

    #[test]
    fn pressure() {
        let s = "some avg10=1.23 avg60=4.56 avg300=7.89 total=12345\n\
                 full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(parse_pressure_avg10(s), Some(1.23));
        assert_eq!(parse_pressure_avg10(""), None);
    }

    #[test]
    fn status() {
        let s = "Name:\tgrenadine\nVmRSS:\t   12345 kB\nThreads:\t42\n";
        assert_eq!(parse_status(s), (12345 * 1024, 42));
    }

    #[test]
    fn memory_stat() {
        let s = "anon_thp 9\nanon 1000\nfile_mapped 9\nfile 2000\nkernel_stack 9\nkernel 300\n";
        assert_eq!(parse_memory_stat(s), (1300, 2000));
        assert_eq!(parse_memory_stat(""), (0, 0));
    }

    fn write_slice(dir: &Path, name: &str, stat: &str, pids: &str, cpu: &str) {
        let dir = dir.join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("memory.stat"), stat).unwrap();
        std::fs::write(dir.join("pids.current"), pids).unwrap();
        std::fs::write(dir.join("cpu.stat"), cpu).unwrap();
    }

    fn session(id: &str) -> AgentSession {
        AgentSession {
            id: id.to_owned(),
            title: format!("title {id}"),
            status: AgentStatus::Running,
            ..AgentSession::fixture(id, "o/n")
        }
    }

    #[test]
    fn slices_match_sessions_and_sum_leftover() {
        let dir = tempfile::tempdir().unwrap();
        let a = "0123456789abcdef0123456789abcdef";
        let b = "11111111111111111111111111111111";
        write_slice(
            dir.path(),
            &format!("grenadine-agent-{a}.slice"),
            "anon 1000\nfile 2000\nkernel 300\n",
            "5\n",
            "usage_usec 100\n",
        );
        write_slice(
            dir.path(),
            &format!("grenadine-agent-{b}.slice"),
            "anon 5000\nfile 6000\n",
            "3\n",
            "usage_usec 200\n",
        );
        // A slice no session owns, and a file that isn't a slice.
        write_slice(
            dir.path(),
            "grenadine-agent-ffffffffffffffffffffffffffffffff.slice",
            "anon 700\nfile 800\nkernel 0\n",
            "2\n",
            "usage_usec 50\n",
        );
        std::fs::write(dir.path().join("cgroup.controllers"), "").unwrap();

        let mut sessions = [
            session("01234567-89ab-cdef-0123-456789abcdef"),
            session("11111111-1111-1111-1111-111111111111"),
            session("22222222-2222-2222-2222-222222222222"),
        ];
        sessions[0].summary = Some("summary a".into());
        let cpu = HashMap::from([
            (format!("grenadine-agent-{a}.slice"), Some(12.5)),
            (
                "grenadine-agent-ffffffffffffffffffffffffffffffff.slice".to_owned(),
                Some(1.0),
            ),
        ]);
        let (used, leftover) = divide(scan_slices(dir.path()), &sessions, &cpu);

        // Sorted by process memory: b's 5000 before a's 1300. Session
        // 2222… has no slice: zeros and no CPU.
        assert_eq!(used.len(), 3);
        assert_eq!(used[0].id, sessions[1].id);
        assert_eq!(used[0].process_bytes, 5000);
        assert_eq!(used[0].cache_bytes, 6000);
        assert_eq!(used[0].processes, 3);
        assert_eq!(used[0].cpu_percent, None);
        assert_eq!(used[0].summary, None);
        assert_eq!(used[1].id, sessions[0].id);
        assert_eq!(used[1].process_bytes, 1300);
        assert_eq!(used[1].cache_bytes, 2000);
        assert_eq!(used[1].processes, 5);
        assert_eq!(used[1].cpu_percent, Some(12.5));
        assert_eq!(used[1].summary, Some("summary a".to_owned()));
        assert_eq!(used[2].id, sessions[2].id);
        assert_eq!(used[2].process_bytes, 0);
        assert_eq!(used[2].cpu_percent, None);

        let leftover = leftover.unwrap();
        assert_eq!(leftover.slices, 1);
        assert_eq!(leftover.process_bytes, 700);
        assert_eq!(leftover.cache_bytes, 800);
        assert_eq!(leftover.processes, 2);
    }

    #[test]
    fn cpu_percent_of_one_core() {
        assert_eq!(cpu_percent(0, 1_000_000), 0.0);
        assert_eq!(cpu_percent(500_000, 1_000_000), 50.0);
        // Several busy cores can exceed 100%.
        assert_eq!(cpu_percent(3_000_000, 1_000_000), 300.0);
        assert_eq!(cpu_percent(1, 0), 0.0);
    }
}
