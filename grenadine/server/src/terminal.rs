//! Login shells in agent sessions' worktrees, which a session's page shows
//! as terminal tabs. A shell keeps running while no page shows it, and a
//! page that connects gets its recent output first. Claude reads them as
//! text through the session's MCP server.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{CStr, OsStr};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use fancy_regex::Regex;
use grenadine_core::api::{AgentSession, ServerEvent, TerminalInfo, TerminalSize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::sync::State;

/// How much of a terminal's output a page that connects gets.
const SCROLLBACK: usize = 1 << 20;

/// How many chunks of output a page may fall behind by. One that falls
/// further is disconnected and reconnects.
const BACKLOG: usize = 1024;

/// How long a closed terminal's shell gets to exit after SIGHUP before it
/// is killed.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

/// How often a wait for a terminal checks whether it is done.
const WAIT_POLL: Duration = Duration::from_millis(200);

pub struct Terminals {
    /// The shell they run.
    shell: PathBuf,
    /// The open terminals by session ID.
    sessions: Mutex<HashMap<String, Session>>,
}

impl Default for Terminals {
    fn default() -> Self {
        Terminals::new(login_shell())
    }
}

#[derive(Default)]
struct Session {
    /// The last terminal ID handed out.
    last: u64,
    open: BTreeMap<u64, Arc<Terminal>>,
}

pub struct Terminal {
    writer: tokio::sync::Mutex<pty_process::OwnedWritePty>,
    /// The pty's controlling side, to see which process group is in the
    /// foreground.
    pty: OwnedFd,
    /// The shell's PID, which is also its process group's ID.
    pid: u32,
    output: Mutex<Output>,
    /// Cancelled to close the terminal.
    hangup: CancellationToken,
    /// Cancelled once the shell exited.
    exited: CancellationToken,
}

struct Output {
    /// The last `SCROLLBACK` bytes of output.
    recent: VecDeque<u8>,
    /// How many bytes of output there have been in all.
    total: u64,
    /// The size the shell was last told it has.
    size: TerminalSize,
    /// Sends the output to the connected pages; `None` once the shell
    /// exited.
    tx: Option<broadcast::Sender<Bytes>>,
}

impl Terminals {
    pub fn new(shell: PathBuf) -> Self {
        Terminals {
            shell,
            sessions: Mutex::default(),
        }
    }

    /// The session's open terminals, oldest first.
    pub fn list(&self, session: &str) -> Vec<TerminalInfo> {
        self.sessions
            .lock()
            .unwrap()
            .get(session)
            .map(|s| s.open.keys().map(|&id| TerminalInfo { id }).collect())
            .unwrap_or_default()
    }

    pub fn get(&self, session: &str, id: u64) -> Option<Arc<Terminal>> {
        self.sessions
            .lock()
            .unwrap()
            .get(session)?
            .open
            .get(&id)
            .cloned()
    }

    /// Hangs up on the terminal. Returns false when there is no such
    /// terminal.
    pub fn close(&self, session: &str, id: u64) -> bool {
        let terminal = self.get(session, id);
        if let Some(t) = &terminal {
            t.hangup.cancel();
        }
        terminal.is_some()
    }

    /// Hangs up on all of the session's terminals and waits for their
    /// shells to exit.
    pub async fn close_all(&self, session: &str) {
        let terminals: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .get(session)
            .map(|s| s.open.values().cloned().collect())
            .unwrap_or_default();
        for t in &terminals {
            t.hangup.cancel();
        }
        for t in terminals {
            t.exited.cancelled().await;
        }
    }

    fn remove(&self, session: &str, id: u64) {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(s) = sessions.get_mut(session) {
            s.open.remove(&id);
            // The counter stays, so that IDs aren't reused while the
            // session lasts and a page can't connect to the wrong shell.
        }
    }

    /// Forgets the deleted session, hanging up on a terminal opened while
    /// it was being deleted.
    pub fn forget(&self, session: &str) {
        if let Some(s) = self.sessions.lock().unwrap().remove(session) {
            for t in s.open.values() {
                t.hangup.cancel();
            }
        }
    }
}

/// Why a wait for a terminal ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitEnd {
    /// The output since the wait began matched.
    Matched,
    /// A command finished and the shell is back at its prompt.
    Finished,
    /// The shell is at its prompt and its output has stopped.
    Idle,
    /// The shell exited.
    Exited,
    TimedOut,
}

impl WaitEnd {
    pub fn describe(self) -> &'static str {
        match self {
            WaitEnd::Matched => "The output matched.",
            WaitEnd::Finished => "A command finished and the shell is back at its prompt.",
            WaitEnd::Idle => "The shell is at its prompt and its output stopped.",
            WaitEnd::Exited => "The shell exited.",
            WaitEnd::TimedOut => "The wait timed out.",
        }
    }
}

impl Terminal {
    fn new(writer: pty_process::OwnedWritePty, pty: OwnedFd, pid: u32) -> Terminal {
        Terminal {
            writer: tokio::sync::Mutex::new(writer),
            pty,
            pid,
            output: Mutex::new(Output {
                recent: VecDeque::new(),
                total: 0,
                size: TerminalSize { cols: 80, rows: 24 },
                tx: Some(broadcast::channel(BACKLOG).0),
            }),
            hangup: CancellationToken::new(),
            exited: CancellationToken::new(),
        }
    }

    /// The recent output and a receiver for what comes after it, or `None`
    /// once the shell exited.
    fn subscribe(&self) -> Option<(Vec<u8>, broadcast::Receiver<Bytes>)> {
        let output = self.output.lock().unwrap();
        let rx = output.tx.as_ref()?.subscribe();
        Some((output.recent.iter().copied().collect(), rx))
    }

    /// The last `lines` lines of the recent output as the terminal shows
    /// them, without colors.
    pub fn text(&self, lines: usize) -> String {
        let (recent, size) = {
            let output = self.output.lock().unwrap();
            (
                output.recent.iter().copied().collect::<Vec<_>>(),
                output.size,
            )
        };
        render(&recent, size, lines)
    }

    /// The output after the first `start` bytes of it, as far as it is
    /// still recent, as a terminal shows it on its own.
    fn text_since(&self, start: u64) -> String {
        let (since, size) = {
            let output = self.output.lock().unwrap();
            let new = usize::try_from(output.total - start).unwrap_or(usize::MAX);
            let skip = output.recent.len().saturating_sub(new);
            let since: Vec<u8> = output.recent.range(skip..).copied().collect();
            (since, output.size)
        };
        render(&since, size, usize::MAX)
    }

    /// Whether the shell, rather than a command that it started, has the
    /// terminal.
    fn at_prompt(&self) -> bool {
        // SAFETY: tcgetpgrp has no memory-safety preconditions.
        let group = unsafe { libc::tcgetpgrp(self.pty.as_raw_fd()) };
        group == self.pid as libc::pid_t
    }

    /// Waits for the first of these, and returns which it was:
    /// - the output since the wait began matches `until`;
    /// - a command that ran during the wait finished, and the shell is back
    ///   at its prompt;
    /// - the shell is at its prompt and has been quiet for `idle` since a
    ///   line of output came, which catches commands too quick to see run
    ///   but not a pause while typing one;
    /// - the shell exits;
    /// - `timeout` passes.
    pub async fn wait(
        &self,
        until: Option<&Regex>,
        idle: Option<Duration>,
        timeout: Duration,
    ) -> WaitEnd {
        let (start, mut rx) = {
            let output = self.output.lock().unwrap();
            match &output.tx {
                Some(tx) => (output.total, tx.subscribe()),
                None => return WaitEnd::Exited,
            }
        };
        let deadline = tokio::time::Instant::now() + timeout;
        let mut poll = tokio::time::interval(WAIT_POLL);
        // Output since the last poll.
        let mut fresh = false;
        let mut last_output: Option<Instant> = None;
        let mut new_line = false;
        let mut ran = false;
        loop {
            tokio::select! {
                out = rx.recv() => match out {
                    Ok(data) => {
                        fresh = true;
                        last_output = Some(Instant::now());
                        new_line |= data.contains(&b'\n');
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        fresh = true;
                        last_output = Some(Instant::now());
                        new_line = true;
                    }
                    Err(broadcast::error::RecvError::Closed) => return WaitEnd::Exited,
                },
                _ = poll.tick() => {
                    if fresh
                        && let Some(until) = until
                        && until.is_match(&self.text_since(start)).unwrap_or(false)
                    {
                        return WaitEnd::Matched;
                    }
                    let quiet = !std::mem::take(&mut fresh);
                    if !self.at_prompt() {
                        ran = true;
                    } else if ran {
                        // The prompt has been printed once the output
                        // stopped.
                        if quiet {
                            return WaitEnd::Finished;
                        }
                    } else if let (Some(idle), Some(last)) = (idle, last_output)
                        && new_line
                        && last.elapsed() >= idle
                    {
                        return WaitEnd::Idle;
                    }
                },
                _ = tokio::time::sleep_until(deadline) => return WaitEnd::TimedOut,
            }
        }
    }

    fn push(&self, data: &[u8]) {
        let mut output = self.output.lock().unwrap();
        output.recent.extend(data);
        output.total += data.len() as u64;
        let excess = output.recent.len().saturating_sub(SCROLLBACK);
        output.recent.drain(..excess);
        if let Some(tx) = &output.tx {
            // Failing only means that no page is connected.
            let _ = tx.send(Bytes::copy_from_slice(data));
        }
    }

    pub(crate) async fn write(&self, input: &[u8]) -> std::io::Result<()> {
        self.writer.lock().await.write_all(input).await
    }

    async fn resize(&self, size: TerminalSize) {
        let size = TerminalSize {
            cols: size.cols.max(1),
            rows: size.rows.max(1),
        };
        let writer = self.writer.lock().await;
        if writer
            .resize(pty_process::Size::new(size.rows, size.cols))
            .is_ok()
        {
            self.output.lock().unwrap().size = size;
        }
    }
}

/// The last `lines` lines of `output` as a terminal of `size` shows it,
/// scrollback included, without colors. Wrapped lines are joined.
fn render(output: &[u8], size: TerminalSize, lines: usize) -> String {
    let dim = |n: u16| std::num::NonZeroU16::new(n).unwrap_or(std::num::NonZeroU16::MIN);
    let mut parser = vt100::Parser::new(dim(size.rows), dim(size.cols), usize::MAX);
    parser.process(output);
    let mut formatted = String::new();
    // Writing to a String can't fail.
    let _ = parser.screen().write_contents_formatted_basic(
        &mut formatted,
        vt100::capture::BasicFormattedCaptureRange::Full(&mut Default::default()),
    );
    let plain: String = vt100::capture::basic_formatted_to_plain(&formatted).collect();
    let all: Vec<&str> = plain.lines().map(str::trim_end).collect();
    let end = all.iter().rposition(|l| !l.is_empty()).map_or(0, |i| i + 1);
    all[end.saturating_sub(lines)..end].join("\n")
}

/// The user's login shell from the password database, else `$SHELL`.
fn login_shell() -> PathBuf {
    passwd_shell()
        .or_else(|| std::env::var_os("SHELL").map(PathBuf::from))
        .filter(|s| !s.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

fn passwd_shell() -> Option<PathBuf> {
    // SAFETY: passwd is plain data that getpwuid_r fills in.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut found = std::ptr::null_mut();
    // SAFETY: the pointers are valid for the call, and buf's length is
    // passed along with it.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() || pwd.pw_shell.is_null() {
        return None;
    }
    // SAFETY: on success pw_shell points to a C string in buf.
    let shell = unsafe { CStr::from_ptr(pwd.pw_shell) };
    Some(PathBuf::from(OsStr::from_bytes(shell.to_bytes())))
}

/// Sends `signal` to the shell's process group; the shell leads it.
fn signal_group(pid: u32, signal: libc::c_int) {
    // SAFETY: kill has no memory-safety preconditions.
    unsafe {
        libc::kill(-(pid as libc::pid_t), signal);
    }
}

/// Starts `shell` as a login shell in `dir` on a new pty, in `slice` if
/// given.
fn spawn(
    shell: &Path,
    dir: &Path,
    slice: Option<String>,
) -> Result<(tokio::process::Child, pty_process::Pty)> {
    let (pty, pts) = pty_process::open().context("can't open a pty")?;
    pty.resize(pty_process::Size::new(24, 80))?;
    // `systemd-run --scope` execs the shell, so the PID stays the shell's.
    let cmd = match slice {
        Some(slice) => pty_process::Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect"])
            .arg(format!("--slice={slice}"))
            .arg("--")
            .arg(shell),
        None => pty_process::Command::new(shell),
    };
    let child = cmd
        .arg("-l")
        .current_dir(dir)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .kill_on_drop(true)
        .spawn(pts)
        .with_context(|| format!("can't run {}", shell.display()))?;
    Ok((child, pty))
}

/// Starts a login shell in the session's worktree. It goes into the
/// session's systemd slice when there is one, so that deleting the session
/// also stops what was started from the shell, e.g. Bazel's server.
pub fn open(state: &Arc<State>, session: &AgentSession) -> Result<TerminalInfo> {
    let (mut child, pty) = spawn(
        &state.agents.terminals.shell,
        Path::new(&session.worktree),
        state.agents.slice_of(&session.id),
    )?;
    let pid = child.id().context("the shell exited right away")?;
    let fd = pty.as_fd().try_clone_to_owned()?;
    let (mut reader, writer) = pty.into_split();
    let terminal = Arc::new(Terminal::new(writer, fd, pid));
    let id = {
        let mut sessions = state.agents.terminals.sessions.lock().unwrap();
        let s = sessions.entry(session.id.clone()).or_default();
        s.last += 1;
        s.open.insert(s.last, terminal.clone());
        s.last
    };
    state.send(ServerEvent::TerminalsChanged(session.id.clone()));

    let state = state.clone();
    let session = session.id.clone();
    tokio::spawn(async move {
        let mut buf = vec![0; 16 * 1024];
        loop {
            tokio::select! {
                // Output that is already there goes out before the exit.
                biased;
                r = reader.read(&mut buf) => match r {
                    Ok(n) if n > 0 => terminal.push(&buf[..n]),
                    // EIO once the shell and whatever else had the
                    // terminal open are gone.
                    _ => break,
                },
                _ = child.wait() => break,
                _ = terminal.hangup.cancelled() => break,
            }
        }
        // The shell still runs if the terminal was closed.
        if matches!(child.try_wait(), Ok(None)) {
            signal_group(pid, libc::SIGHUP);
            if tokio::time::timeout(CLOSE_TIMEOUT, child.wait())
                .await
                .is_err()
            {
                // Not reaped yet, so the PID is still the shell's.
                signal_group(pid, libc::SIGKILL);
                let _ = child.wait().await;
            }
        }
        state.agents.terminals.remove(&session, id);
        // Dropping the sender disconnects the pages.
        terminal.output.lock().unwrap().tx = None;
        terminal.exited.cancel();
        state.send(ServerEvent::TerminalsChanged(session));
    });
    Ok(TerminalInfo { id })
}

/// Connects a page to the terminal until either goes away. A page that
/// falls behind is disconnected; it reconnects and starts over from the
/// recent output.
pub async fn attach(state: Arc<State>, terminal: Arc<Terminal>, mut socket: WebSocket) {
    let Some((recent, mut rx)) = terminal.subscribe() else {
        let _ = socket.send(Message::Close(None)).await;
        return;
    };
    if socket.send(Message::Binary(recent.into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Ok(data) => {
                    if socket.send(Message::Binary(data)).await.is_err() {
                        return;
                    }
                }
                // The shell exited or the page fell behind.
                Err(_) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(input))) => {
                    if terminal.write(&input).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(size) = serde_json::from_str::<TerminalSize>(&text) {
                        terminal.resize(size).await;
                    }
                }
                // axum answers pings itself.
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return,
            },
            _ = state.shutdown.cancelled() => break,
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shell_runs_in_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, pty) = spawn(Path::new("/bin/sh"), dir.path(), None).unwrap();
        let (mut reader, mut writer) = pty.into_split();
        writer
            .write_all(b"echo \"in-$(basename \"$PWD\")-$((6 * 7))\"\nexit\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        let mut buf = [0; 4096];
        while let Ok(n @ 1..) = reader.read(&mut buf).await {
            out.extend_from_slice(&buf[..n]);
        }
        let name = dir.path().file_name().unwrap().to_str().unwrap();
        let out = String::from_utf8_lossy(&out);
        assert!(out.contains(&format!("in-{name}-42")), "{out}");
        assert!(child.wait().await.unwrap().success());
    }

    #[test]
    fn login_shell_is_absolute() {
        assert!(login_shell().is_absolute());
    }

    #[tokio::test]
    async fn scrollback_keeps_the_end() {
        let (pty, _) = pty_process::open().unwrap();
        let fd = pty.as_fd().try_clone_to_owned().unwrap();
        let (_, writer) = pty.into_split();
        let t = Terminal::new(writer, fd, 0);
        t.push(&vec![b'a'; SCROLLBACK]);
        t.push(b"bc");
        let (recent, mut rx) = t.subscribe().unwrap();
        assert_eq!(recent.len(), SCROLLBACK);
        assert!(recent.ends_with(b"aabc"));
        t.push(b"d");
        assert_eq!(&rx.try_recv().unwrap()[..], b"d");
        t.output.lock().unwrap().tx = None;
        assert!(t.subscribe().is_none());
    }

    #[test]
    fn renders_what_the_terminal_shows() {
        let size = TerminalSize { cols: 10, rows: 3 };
        // Progress redrawn over itself, colors, and a line that wraps.
        let output = b"one\r\n\x1b[32mtwo\x1b[0m\r\n50%\r100%\r\n0123456789abc\r\n$ \r\n\r\n";
        assert_eq!(
            render(output, size, usize::MAX),
            "one\ntwo\n100%\n0123456789abc\n$"
        );
        assert_eq!(render(output, size, 2), "0123456789abc\n$");
        assert_eq!(render(b"", size, 5), "");
    }
}
