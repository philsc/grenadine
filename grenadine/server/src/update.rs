//! Updates the server to the latest `origin/main`: checks it out in the
//! clone that `bazel run` started the server from, builds it while the
//! server goes on serving, waits until no session is busy and no merge
//! runs, and has `main` exec the new build in place of this process.
//!
//! The new build is copied next to the database, and the copy is what
//! runs: the wait can take hours, and other builds in the clone replace or
//! remove Bazel's output meanwhile. The copy's path says what it was built
//! from.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use grenadine_core::api::{CommitInfo, ServerEvent, UpdateCheck, UpdatePhase, UpdateStatus};
use tokio::sync::Notify;

use crate::agent;
use crate::git::Repo;
use crate::sync::State;

const BRANCH: &str = "main";
const REMOTE_REF: &str = "refs/remotes/origin/main";
const TARGET: &str = "//grenadine/server";

/// The argument that has the server binary act as `install`.
pub const INSTALL_ARG: &str = "install-update";

/// The launch script's name in the directory of the copies.
const SCRIPT: &str = "launch.sh";

/// Stands for no `-c` in a copy's path.
const NO_MODE: &str = "default";

/// How often a waiting update looks whether the sessions and merges are
/// done.
const WAIT_POLL: Duration = Duration::from_secs(1);

/// What the running server was built from, and how to build it again.
pub struct Build {
    repo: Repo,
    /// The commit the server was built from: the one its copy was made
    /// of, or the one the clone was at when `bazel run` started it.
    running: CommitInfo,
    /// The `-c` to build with, if the running binary's path tells.
    mode: Option<String>,
    bazel: PathBuf,
    /// Where the new build is copied. An update refuses to use it when it
    /// holds anything that updates don't create.
    dir: PathBuf,
}

impl Build {
    /// `None` when the server wasn't started through `bazel run`.
    pub fn detect(token: &str, dir: PathBuf) -> Option<Build> {
        let workspace = PathBuf::from(std::env::var_os("BUILD_WORKSPACE_DIRECTORY")?);
        let exe = std::env::current_exe().unwrap_or_default();
        let build = Repo::open(&workspace, "origin").and_then(|repo| {
            let repo = repo.with_token(token);
            let (rev, mode) = match installed_from(&exe, &dir) {
                Some((sha, mode)) => (sha, mode),
                None => ("HEAD".to_owned(), compilation_mode(&exe)),
            };
            Ok(Build {
                running: commit(&repo, &rev)?,
                repo,
                mode,
                bazel: "bazel".into(),
                dir,
            })
        });
        build
            .inspect_err(|e| {
                tracing::warn!("can't update from {}: {e:#}", workspace.display());
            })
            .ok()
    }

    /// Fetches the branch and returns its tip.
    fn fetch(&self) -> Result<String> {
        self.repo.fetch_branch(BRANCH, REMOTE_REF)
    }

    /// Moves the clone to the branch's tip. Returns the commit it was at
    /// and the tip.
    fn check_out(&self) -> Result<(String, String)> {
        // First, since it takes a while, and the checks are to hold when
        // the clone moves.
        let target = self.fetch()?;
        let path = self.repo.path.display();
        if let Ok(branch) = self
            .repo
            .git(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        {
            bail!(
                "{path} has the branch {} checked out; an update only moves a detached HEAD",
                branch.trim()
            );
        }
        if self.modified()? {
            bail!("{path} has uncommitted changes");
        }
        let old = commit(&self.repo, "HEAD")?.sha;
        if !self
            .repo
            .git_ok(&["merge-base", "--is-ancestor", &old, &target])
        {
            bail!("{path} is at a commit that isn't in origin/{BRANCH}");
        }
        self.repo
            .git(&["checkout", "--quiet", "--detach", &target])?;
        Ok((old, target))
    }

    /// Whether tracked files in the clone have uncommitted changes.
    fn modified(&self) -> Result<bool> {
        let out = self
            .repo
            .git(&["status", "--porcelain", "--untracked-files=no"])?;
        Ok(!out.trim().is_empty())
    }

    /// Moves the clone back to `old`, unless someone has changed it since
    /// `check_out` moved it to `target`.
    fn restore(&self, old: &str, target: &str) -> Result<()> {
        let detached = !self.repo.git_ok(&["symbolic-ref", "--quiet", "HEAD"]);
        if old != target && detached && self.is_at(target)? {
            self.repo.git(&["checkout", "--quiet", "--detach", old])?;
        }
        Ok(())
    }

    /// Whether the clone has `target` checked out without changes.
    fn is_at(&self, target: &str) -> Result<bool> {
        Ok(commit(&self.repo, "HEAD")?.sha == target && !self.modified()?)
    }

    /// Where the build of `target` is copied.
    fn installed(&self, target: &str) -> PathBuf {
        let mode = self.mode.as_deref().unwrap_or(NO_MODE);
        self.dir.join(target).join(mode).join("grenadine")
    }

    /// Builds the checked out commit, `target`, and copies the binary.
    /// Returns the copy's path.
    async fn build(&self, target: &str) -> Result<PathBuf> {
        // The running server may be a copy in there; it goes on without
        // its file.
        clear(&self.dir).with_context(|| format!("can't clear {}", self.dir.display()))?;
        let installed = self.installed(target);
        let parent = installed.parent().expect("it's in the directory");
        std::fs::create_dir_all(parent)
            .with_context(|| format!("can't create {}", parent.display()))?;
        let script = self.dir.join(SCRIPT);

        let mut cmd = tokio::process::Command::new(&self.bazel);
        cmd.arg("run")
            .args(["--color=no", "--noshow_progress"])
            .arg(format!("--script_path={}", script.display()));
        if let Some(mode) = &self.mode {
            cmd.args(["-c", mode]);
        }
        cmd.arg(TARGET)
            .current_dir(&self.repo.path)
            .env_remove("BAZELISK_SKIP_WRAPPER");
        run_step(cmd, "the build").await?;

        // The new build copies itself: only the script knows where it is.
        let mut cmd = tokio::process::Command::new(&script);
        cmd.arg(INSTALL_ARG).arg(&installed);
        run_step(cmd, "copying the new build").await?;
        // Whether the copy runs outside of Bazel's output.
        let mut cmd = tokio::process::Command::new(&installed);
        cmd.arg("--help");
        run_step(cmd, "starting the new build").await?;
        Ok(installed)
    }
}

/// Removes what earlier updates left in `dir`. Removes nothing when there's
/// anything else in it: the user may have had a directory of that name.
fn clear(dir: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        entries => entries?.collect::<std::io::Result<Vec<_>>>()?,
    };
    let is_copy = |name: &str| name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit());
    let mut copies = Vec::new();
    let mut scripts = Vec::new();
    for entry in entries {
        // Doesn't follow symlinks.
        let kind = entry.file_type()?;
        let name = entry.file_name();
        match name.to_str() {
            Some(SCRIPT) if kind.is_file() => scripts.push(entry.path()),
            Some(name) if kind.is_dir() && is_copy(name) => copies.push(entry.path()),
            _ => bail!("{} isn't from an update", entry.path().display()),
        }
    }
    for copy in copies {
        std::fs::remove_dir_all(copy)?;
    }
    for script in scripts {
        std::fs::remove_file(script)?;
    }
    Ok(())
}

/// Runs `cmd`; a failure says that `what` failed, and why.
async fn run_step(mut cmd: tokio::process::Command, what: &str) -> Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Ctrl-C on the server shouldn't reach it directly.
        .process_group(0);
    let mut child = cmd.spawn().with_context(|| {
        let program = Path::new(cmd.as_std().get_program());
        format!("can't run {}", program.display())
    })?;
    let mut group = KillGroup(child.id());
    let stderr = agent::stderr_tail(child.stderr.take().expect("stderr is piped"));
    let status = child.wait().await?;
    // Its PID may be another process's now.
    group.0 = None;
    if !status.success() {
        bail!(
            "{what} failed ({status})\n{}",
            stderr.await.unwrap_or_default()
        );
    }
    Ok(())
}

/// Kills the process group on drop: `bazel` may be a wrapper that runs the
/// real one as a child.
struct KillGroup(Option<u32>);

impl Drop for KillGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            crate::terminal::signal_group(pid, libc::SIGKILL);
        }
    }
}

/// Copies this binary to the path in the second argument and returns the
/// exit status.
pub fn install() -> i32 {
    let copied = std::env::args_os()
        .nth(2)
        .context("no path to copy to")
        .and_then(|to| Ok(std::fs::copy(std::env::current_exe()?, to)?));
    match copied {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("can't copy the build: {e:#}");
            1
        }
    }
}

/// The commit and the `-c` that `exe` was built with, if it's a copy that
/// an update made in `dir`.
fn installed_from(exe: &Path, dir: &Path) -> Option<(String, Option<String>)> {
    let mode_dir = exe.parent()?;
    let sha_dir = mode_dir.parent()?;
    if sha_dir.parent()? != dir.canonicalize().ok()? {
        return None;
    }
    let mode = mode_dir.file_name()?.to_str()?;
    Some((
        sha_dir.file_name()?.to_str()?.to_owned(),
        (mode != NO_MODE).then(|| mode.to_owned()),
    ))
}

fn commit(repo: &Repo, rev: &str) -> Result<CommitInfo> {
    log(repo, &["-1", rev])?
        .pop()
        .ok_or_else(|| anyhow!("no commit {rev}"))
}

fn log(repo: &Repo, args: &[&str]) -> Result<Vec<CommitInfo>> {
    let mut all = vec!["log", "--no-show-signature", "--format=%H%x00%ct%x00%s"];
    all.extend(args);
    repo.git(&all)?
        .lines()
        .map(|line| {
            let mut parts = line.splitn(3, '\0');
            let mut next = || parts.next().context("unexpected git log output");
            Ok(CommitInfo {
                sha: next()?.to_owned(),
                date: next()?.parse()?,
                title: next()?.to_owned(),
            })
        })
        .collect()
}

/// The compilation mode in a Bazel output path such as
/// `…/bazel-out/k8-opt/bin/…`.
fn compilation_mode(exe: &Path) -> Option<String> {
    let mut parts = exe.iter().map(|p| p.to_string_lossy());
    parts.find(|p| p == "bazel-out")?;
    let config = parts.next()?;
    ["fastbuild", "dbg", "opt"]
        .into_iter()
        .find(|m| config.split('-').any(|p| p == *m))
        .map(str::to_owned)
}

enum Decision {
    Force,
    Cancel,
}

#[derive(Default)]
struct Inner {
    phase: UpdatePhase,
    /// What the user decided about the update being built or waiting.
    decision: Option<Decision>,
    /// The binary to restart into, once the restart is due.
    restart_into: Option<PathBuf>,
}

pub struct Updater {
    build: Option<Build>,
    inner: Mutex<Inner>,
    decided: Notify,
    restart: Notify,
    /// Held while git works on the clone.
    git: Arc<tokio::sync::Mutex<()>>,
}

impl Updater {
    pub fn new(build: Option<Build>) -> Updater {
        Updater {
            build,
            inner: Mutex::default(),
            decided: Notify::new(),
            restart: Notify::new(),
            git: Arc::default(),
        }
    }

    pub fn status(&self) -> UpdateStatus {
        UpdateStatus {
            running: self.build.as_ref().map(|b| b.running.clone()),
            phase: self.inner.lock().unwrap().phase.clone(),
        }
    }

    /// Waits until an update is ready to restart, and returns the binary
    /// to exec.
    pub async fn restart_requested(&self) -> PathBuf {
        self.restart.notified().await;
        let inner = self.inner.lock().unwrap();
        inner.restart_into.clone().expect("the restart is due")
    }

    /// Takes the decision about the update; false if it can't be followed
    /// now. A build can only be cancelled.
    fn decide(&self, decision: Decision) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match (&inner.phase, &decision) {
            (UpdatePhase::Waiting { .. }, _) => {}
            (UpdatePhase::Building, Decision::Cancel) => {}
            _ => return false,
        }
        inner.decision = Some(decision);
        drop(inner);
        self.decided.notify_one();
        true
    }

    /// Restarts without waiting for the sessions and merges.
    pub fn force(&self) -> bool {
        self.decide(Decision::Force)
    }
}

fn set_phase(state: &State, phase: UpdatePhase) {
    let mut inner = state.update.inner.lock().unwrap();
    if inner.phase == phase {
        return;
    }
    inner.phase = phase;
    drop(inner);
    state.send(ServerEvent::UpdateChanged);
}

/// Stops building or waiting to restart, or dismisses a failure.
pub fn cancel(state: &State) -> bool {
    let failed = matches!(
        state.update.inner.lock().unwrap().phase,
        UpdatePhase::Failed { .. }
    );
    if failed {
        set_phase(state, UpdatePhase::Idle);
        return true;
    }
    state.update.decide(Decision::Cancel)
}

/// The git directory that the clone's worktrees share.
fn git_dir(repo: &Repo) -> Result<PathBuf> {
    let dir = repo.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    Ok(Path::new(dir.trim_end_matches('\n')).canonicalize()?)
}

/// Runs blocking git work on the clone. The task holds the lock itself, so
/// a page closing mid-request doesn't let other work in early. It also
/// holds the locks of the configured clones that share the clone's git
/// directory, so that sync, landings and new worktrees stay out.
async fn with_clone<T: Send + 'static>(
    state: &Arc<State>,
    f: impl FnOnce(&Build) -> Result<T> + Send + 'static,
) -> Result<T> {
    let guard = state.update.git.clone().lock_owned().await;
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let build = state
            .update
            .build
            .as_ref()
            .context("the server wasn't started with bazel run")?;
        let dir = git_dir(&build.repo)?;
        let _shared: Vec<_> = state
            .repos
            .values()
            .filter(|c| git_dir(&c.repo).is_ok_and(|d| d == dir))
            .map(|c| c.git_lock.blocking_lock())
            .collect();
        f(build)
    })
    .await?
}

/// Fetches the branch and lists its commits that aren't running.
pub async fn check(state: &Arc<State>) -> Result<UpdateCheck> {
    with_clone(state, |build| {
        let target = build.fetch()?;
        let range = format!("{}..{target}", build.running.sha);
        Ok(UpdateCheck {
            behind: log(&build.repo, &[&range])?,
        })
    })
    .await
}

/// Starts an update; false if there's one already or none can be done.
pub fn start(state: &Arc<State>) -> bool {
    if state.update.build.is_none() {
        return false;
    }
    {
        let mut inner = state.update.inner.lock().unwrap();
        if !matches!(inner.phase, UpdatePhase::Idle | UpdatePhase::Failed { .. }) {
            return false;
        }
        inner.phase = UpdatePhase::CheckingOut;
        inner.decision = None;
    }
    state.send(ServerEvent::UpdateChanged);
    let state = state.clone();
    tokio::spawn(async move {
        let result = tokio::select! {
            r = prepare(&state) => r,
            _ = state.shutdown.cancelled() => return,
        };
        match result {
            Ok(Some((target, binary))) => wait(&state, target, binary).await,
            Ok(None) => set_phase(&state, UpdatePhase::Idle),
            Err(e) => {
                tracing::warn!("can't update: {e:#}");
                let error = format!("{e:#}");
                set_phase(&state, UpdatePhase::Failed { error });
            }
        }
    });
    true
}

/// Checks out the branch's tip and builds it. Returns it and the binary, or
/// `None` if the user cancelled the build.
async fn prepare(state: &Arc<State>) -> Result<Option<(String, PathBuf)>> {
    let (old, target) = with_clone(state, Build::check_out).await?;
    tracing::info!("building {target}");
    set_phase(state, UpdatePhase::Building);
    let build = state.update.build.as_ref().context("no build")?;
    let built = async {
        let binary = build.build(&target).await?;
        // The build is only the target's if nobody changed the clone
        // meanwhile. Later changes don't matter to the copy.
        let built = target.clone();
        with_clone(state, move |b| {
            if !b.is_at(&built)? {
                bail!("{} was changed during the build", b.repo.path.display());
            }
            Ok(())
        })
        .await?;
        Ok(binary)
    };
    // Dropping the build stops Bazel.
    let built = tokio::select! {
        r = built => r.map(Some),
        _ = build_cancelled(state) => Ok(None),
    };
    if let Ok(Some(binary)) = built {
        return Ok(Some((target, binary)));
    }
    let restored = with_clone(state, move |b| b.restore(&old, &target)).await;
    if let Err(e) = restored {
        tracing::warn!("can't move the clone back: {e:#}");
    }
    built.map(|_| None)
}

/// Waits until the user cancels the build.
async fn build_cancelled(state: &State) {
    loop {
        // Before waiting: a notification can be left from an earlier one.
        if state.update.inner.lock().unwrap().decision.is_some() {
            return;
        }
        state.update.decided.notified().await;
    }
}

/// Waits until no session is busy and no merge runs, or the user decides.
async fn wait(state: &Arc<State>, target: String, binary: PathBuf) {
    loop {
        let sessions = agent::busy_sessions(state);
        let merges = crate::api::busy_merges(state);
        let terminals = state.agents.terminals.count();
        {
            let mut inner = state.update.inner.lock().unwrap();
            let next = match inner.decision.take() {
                Some(Decision::Cancel) => UpdatePhase::Idle,
                None if !sessions.is_empty() || !merges.is_empty() => UpdatePhase::Waiting {
                    target: target.clone(),
                    sessions,
                    merges,
                    terminals,
                },
                // While the server still runs.
                _ if !binary.exists() => UpdatePhase::Failed {
                    error: format!("{} is gone", binary.display()),
                },
                _ => UpdatePhase::Restarting,
            };
            if next == UpdatePhase::Restarting {
                inner.restart_into = Some(binary.clone());
            }
            let changed = inner.phase != next;
            inner.phase = next.clone();
            drop(inner);
            if changed {
                state.send(ServerEvent::UpdateChanged);
            }
            match next {
                UpdatePhase::Waiting { .. } => {}
                UpdatePhase::Restarting => {
                    tracing::info!("restarting into {target}");
                    state.update.restart.notify_one();
                    return;
                }
                _ => return,
            }
        }
        tokio::select! {
            _ = state.update.decided.notified() => {}
            _ = tokio::time::sleep(WAIT_POLL) => {}
            _ = state.shutdown.cancelled() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{Fixture, commit as add_commit, fixture, run};

    /// A Bazel whose build copies the launch script as the new build.
    const BAZEL: &str = r#"#!/bin/sh
for arg; do
    case "$arg" in --script_path=*) script="${arg#--script_path=}" ;; esac
done
cat > "$script" <<'EOF'
#!/bin/sh
if [ "$1" = install-update ]; then cp "$0" "$2"; fi
EOF
chmod 755 "$script"
"#;

    fn build(f: &Fixture, bazel: &str) -> Build {
        run(&f.clone.path, &["checkout", "-q", "--detach"]);
        let exe = f._dir.path().join("bazel");
        crate::test_util::write_executable(&exe, bazel);
        Build {
            repo: f.clone.clone(),
            running: commit(&f.clone, "HEAD").unwrap(),
            mode: Some("opt".into()),
            bazel: exe,
            dir: f._dir.path().join("grenadine.db-update"),
        }
    }

    fn state(build: Option<Build>) -> Arc<State> {
        let mut state = crate::sync::test_state();
        Arc::get_mut(&mut state).unwrap().update = Updater::new(build);
        state
    }

    fn head(f: &Fixture) -> String {
        run(&f.clone.path, &["rev-parse", "HEAD"])
    }

    async fn until(state: &State, done: impl Fn(&UpdatePhase) -> bool) -> UpdatePhase {
        let wait = async {
            loop {
                let phase = state.update.status().phase;
                if done(&phase) {
                    return phase;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(30), wait)
            .await
            .unwrap_or_else(|_| panic!("stuck: {:?}", state.update.status()))
    }

    #[test]
    fn modes() {
        let mode = |p: &str| compilation_mode(Path::new(p));
        assert_eq!(mode("/x/bazel-out/k8-opt/bin/s").as_deref(), Some("opt"));
        assert_eq!(
            mode("/x/bazel-out/k8-fastbuild-ST-1a/bin/s").as_deref(),
            Some("fastbuild")
        );
        assert_eq!(
            mode("/x/bazel-out/darwin_arm64-dbg/bin/s").as_deref(),
            Some("dbg")
        );
        assert_eq!(mode("/opt/bazel-out/k8/bin/s"), None);
        assert_eq!(mode("/opt/grenadine"), None);
    }

    #[test]
    fn refuses_to_touch_local_work() {
        let f = fixture();
        let b = build(&f, "");
        let target = add_commit(&f.upstream, "b", "1\n");
        let refused = |why: &str| {
            let before = head(&f);
            let e = format!("{:#}", b.check_out().unwrap_err());
            assert!(e.contains(why), "{e}");
            assert_eq!(head(&f), before);
        };

        run(&f.clone.path, &["checkout", "-q", "main"]);
        refused("has the branch main checked out");
        run(&f.clone.path, &["checkout", "-q", "--detach"]);

        std::fs::write(f.clone.path.join("a"), "mine\n").unwrap();
        refused("uncommitted changes");
        assert_eq!(
            std::fs::read_to_string(f.clone.path.join("a")).unwrap(),
            "mine\n"
        );
        run(&f.clone.path, &["checkout", "-q", "a"]);

        add_commit(&f.clone.path, "c", "1\n");
        refused("isn't in origin/main");
        run(&f.clone.path, &["checkout", "-q", "--detach", "HEAD^"]);

        // Untracked files are fine.
        std::fs::write(f.clone.path.join("untracked"), "").unwrap();
        let (old, new) = b.check_out().unwrap();
        assert_eq!((old, new.clone()), (b.running.sha.clone(), target.clone()));
        assert_eq!(head(&f), target);
    }

    #[test]
    fn restores_only_an_unchanged_clone() {
        let f = fixture();
        let b = build(&f, "");
        add_commit(&f.upstream, "b", "1\n");
        let (old, target) = b.check_out().unwrap();
        std::fs::write(f.clone.path.join("a"), "mine\n").unwrap();
        b.restore(&old, &target).unwrap();
        assert_eq!(head(&f), target);
        run(&f.clone.path, &["checkout", "-q", "a"]);
        b.restore(&old, &target).unwrap();
        assert_eq!(head(&f), old);
    }

    #[tokio::test]
    async fn an_idle_server_restarts_into_the_build() {
        let f = fixture();
        let b = build(&f, &format!("{BAZEL}echo \"$@\" > args\n"));
        let dir = b.dir.clone();
        let first = b.running.sha.clone();
        let state = state(Some(b));
        let target = add_commit(&f.upstream, "b", "1\n");
        let check = check(&state).await.unwrap();
        assert_eq!(check.behind.len(), 1);
        assert_eq!(check.behind[0].sha, target);
        assert_eq!(check.behind[0].title, "b");
        assert_eq!(head(&f), first, "checking doesn't move the clone");

        assert!(start(&state));
        let restart = state.update.restart_requested();
        let got = tokio::time::timeout(Duration::from_secs(30), restart).await;
        let binary = dir.join(&target).join("opt/grenadine");
        assert_eq!(got.unwrap(), binary);
        assert!(binary.exists());
        assert_eq!(state.update.status().phase, UpdatePhase::Restarting);
        assert!(!start(&state), "one at a time");
        assert_eq!(head(&f), target);
        let args = std::fs::read_to_string(f.clone.path.join("args")).unwrap();
        assert_eq!(
            args.trim(),
            format!(
                "run --color=no --noshow_progress --script_path={} -c opt //grenadine/server",
                dir.join("launch.sh").display()
            )
        );
    }

    #[test]
    fn a_copy_tells_what_it_was_built_from() {
        let f = fixture();
        let mut b = build(&f, "");
        std::fs::create_dir(&b.dir).unwrap();
        b.dir = b.dir.canonicalize().unwrap();
        let from = |b: &Build| installed_from(&b.installed("abc"), &b.dir);
        assert_eq!(from(&b), Some(("abc".into(), Some("opt".into()))));
        b.mode = None;
        assert_eq!(from(&b), Some(("abc".into(), None)));
        let other = Path::new("/x/bazel-out/k8-opt/bin/grenadine/server/server");
        assert_eq!(installed_from(other, &b.dir), None);
    }

    #[test]
    fn clears_only_what_updates_create() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("x-update");
        clear(&dir).unwrap();
        let copy = dir.join("0123456789abcdef".repeat(3).split_at(40).0);
        std::fs::create_dir_all(copy.join("opt")).unwrap();
        std::fs::write(copy.join("opt/grenadine"), "").unwrap();
        std::fs::write(dir.join(SCRIPT), "").unwrap();
        for name in ["notes", &"f".repeat(40)] {
            let mine = dir.join(name);
            std::fs::write(&mine, "mine").unwrap();
            let e = format!("{:#}", clear(&dir).unwrap_err());
            assert!(e.contains("isn't from an update"), "{e}");
            assert!(mine.exists() && copy.exists() && dir.join(SCRIPT).exists());
            std::fs::remove_file(mine).unwrap();
        }
        clear(&dir).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn waits_for_merges() {
        let f = fixture();
        let state = state(Some(build(&f, BAZEL)));
        let key = grenadine_core::api::PrKey {
            repo: "o/n".into(),
            number: 1,
        };
        state.starting_merges.lock().unwrap().push(key.clone());
        assert!(start(&state));
        let phase = until(&state, |p| matches!(p, UpdatePhase::Waiting { .. })).await;
        let UpdatePhase::Waiting { merges, .. } = phase else {
            unreachable!()
        };
        assert_eq!(merges, [key]);
        state.starting_merges.lock().unwrap().clear();
        until(&state, |p| *p == UpdatePhase::Restarting).await;
    }

    /// The clone can change and Bazel's output go away during the wait.
    #[tokio::test]
    async fn the_copy_outlives_changes_to_the_clone() {
        let f = fixture();
        let b = build(&f, BAZEL);
        let dir = b.dir.clone();
        let state = state(Some(b));
        let target = add_commit(&f.upstream, "b", "1\n");
        let busy = agent::busy_for_test(&state, "busy");
        assert!(start(&state));
        until(&state, |p| matches!(p, UpdatePhase::Waiting { .. })).await;
        add_commit(&f.clone.path, "c", "1\n");
        std::fs::remove_file(dir.join("launch.sh")).unwrap();
        drop(busy);
        let got = state.update.restart_requested().await;
        assert_eq!(got, dir.join(target).join("opt/grenadine"));
    }

    #[tokio::test]
    async fn fails_when_the_copy_is_gone() {
        let f = fixture();
        let b = build(&f, BAZEL);
        let dir = b.dir.clone();
        let state = state(Some(b));
        let busy = agent::busy_for_test(&state, "busy");
        assert!(start(&state));
        until(&state, |p| matches!(p, UpdatePhase::Waiting { .. })).await;
        std::fs::remove_dir_all(dir).unwrap();
        drop(busy);
        until(&state, |p| matches!(p, UpdatePhase::Failed { .. })).await;
    }

    #[tokio::test]
    async fn refuses_a_build_of_a_changed_clone() {
        let f = fixture();
        let b = build(&f, &format!("{BAZEL}echo mine > a\n"));
        let state = state(Some(b));
        let target = add_commit(&f.upstream, "b", "1\n");
        assert!(start(&state));
        let failed = until(&state, |p| matches!(p, UpdatePhase::Failed { .. })).await;
        let UpdatePhase::Failed { error } = failed else {
            unreachable!()
        };
        assert!(error.contains("changed during the build"), "{error}");
        assert_eq!(head(&f), target, "the changed clone stays");
    }

    /// Landings and syncs of a `--repo` that is the same clone stay out.
    #[tokio::test]
    async fn takes_the_lock_of_a_repo_in_the_same_clone() {
        let f = fixture();
        let b = build(&f, BAZEL);
        let first = b.running.sha.clone();
        let repo = Arc::new(crate::sync::ClonedRepo {
            repo: f.clone.clone(),
            git_lock: tokio::sync::Mutex::new(()),
        });
        let repos = [("owner/name".to_owned(), repo.clone())].into();
        let mut state = crate::sync::test_state_with(repos);
        Arc::get_mut(&mut state).unwrap().update = Updater::new(Some(b));
        add_commit(&f.upstream, "b", "1\n");
        let held = repo.git_lock.lock().await;
        assert!(start(&state));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(state.update.status().phase, UpdatePhase::CheckingOut);
        assert_eq!(head(&f), first);
        drop(held);
        until(&state, |p| *p == UpdatePhase::Restarting).await;
    }

    #[tokio::test]
    async fn a_failed_build_puts_the_clone_back() {
        let f = fixture();
        let b = build(&f, "#!/bin/sh\necho 'ERROR: no good' >&2\nexit 1\n");
        let old = b.running.sha.clone();
        let state = state(Some(b));
        add_commit(&f.upstream, "b", "1\n");
        assert!(start(&state));
        let failed = until(&state, |p| matches!(p, UpdatePhase::Failed { .. })).await;
        let UpdatePhase::Failed { error } = failed else {
            unreachable!()
        };
        assert!(error.contains("ERROR: no good"), "{error}");
        assert_eq!(head(&f), old);
        // It can be dismissed, and tried again.
        assert!(cancel(&state));
        assert_eq!(state.update.status().phase, UpdatePhase::Idle);
        assert!(start(&state));
    }

    #[tokio::test]
    async fn a_hung_build_can_be_cancelled() {
        let f = fixture();
        let b = build(
            &f,
            "#!/bin/sh\nsleep 600 &\necho $! > pid.tmp && mv pid.tmp pid\nwait\n",
        );
        let old = b.running.sha.clone();
        let state = state(Some(b));
        add_commit(&f.upstream, "b", "1\n");
        assert!(start(&state));
        until(&state, |p| *p == UpdatePhase::Building).await;
        let file = f.clone.path.join("pid");
        until(&state, |_| file.exists()).await;
        assert!(!state.update.force(), "there's nothing to restart into");
        assert!(cancel(&state));
        until(&state, |p| *p == UpdatePhase::Idle).await;
        assert_eq!(head(&f), old);
        // What the build started is gone too, or a zombie until whatever
        // adopted it reaps it.
        let pid = std::fs::read_to_string(file).unwrap();
        let stat = format!("/proc/{}/stat", pid.trim());
        until(&state, |_| match std::fs::read_to_string(&stat) {
            Ok(s) => s.contains(") Z "),
            Err(_) => true,
        })
        .await;
        assert!(start(&state));
    }

    #[tokio::test]
    async fn waits_for_busy_sessions() {
        let f = fixture();
        let state = state(Some(build(&f, BAZEL)));
        assert!(!state.update.force(), "nothing to force yet");
        assert!(!cancel(&state));

        let busy = agent::busy_for_test(&state, "busy");
        assert!(start(&state));
        let phase = until(&state, |p| matches!(p, UpdatePhase::Waiting { .. })).await;
        let UpdatePhase::Waiting { sessions, .. } = phase else {
            unreachable!()
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "busy");

        // Cancelled, it doesn't restart, and can be started again.
        assert!(cancel(&state));
        until(&state, |p| *p == UpdatePhase::Idle).await;
        assert!(start(&state));
        until(&state, |p| matches!(p, UpdatePhase::Waiting { .. })).await;
        assert!(state.update.force());
        until(&state, |p| *p == UpdatePhase::Restarting).await;
        drop(busy);
    }

    #[tokio::test]
    async fn restarts_once_the_sessions_are_done() {
        let f = fixture();
        let state = state(Some(build(&f, BAZEL)));
        let busy = agent::busy_for_test(&state, "busy");
        assert!(start(&state));
        until(&state, |p| matches!(p, UpdatePhase::Waiting { .. })).await;
        drop(busy);
        until(&state, |p| *p == UpdatePhase::Restarting).await;
    }

    #[tokio::test]
    async fn unavailable_without_bazel_run() {
        let state = state(None);
        assert!(state.update.status().running.is_none());
        assert!(!start(&state));
        assert!(check(&state).await.is_err());
    }
}
