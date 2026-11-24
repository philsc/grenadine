//! Updates the server to the latest `origin/main`: checks it out in the
//! clone that `bazel run` started the server from, builds it while the
//! server goes on serving, waits until no session is busy, and has `main`
//! exec the new build in place of this process.

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

/// How often a waiting update looks whether the sessions are done.
const WAIT_POLL: Duration = Duration::from_secs(1);

/// What the running server was built from, and how to build it again.
pub struct Build {
    repo: Repo,
    /// The commit the clone was at when the server started.
    running: CommitInfo,
    /// The `-c` to build with, if the running binary's path tells.
    mode: Option<String>,
    bazel: PathBuf,
    /// Where Bazel writes the script that launches the new build.
    script: PathBuf,
}

impl Build {
    /// `None` when the server wasn't started through `bazel run`.
    pub fn detect(token: &str, script: PathBuf) -> Option<Build> {
        let workspace = PathBuf::from(std::env::var_os("BUILD_WORKSPACE_DIRECTORY")?);
        let exe = std::env::current_exe().unwrap_or_default();
        let build = Repo::open(&workspace, "origin").and_then(|repo| {
            let repo = repo.with_token(token);
            Ok(Build {
                running: commit(&repo, "HEAD")?,
                repo,
                mode: compilation_mode(&exe),
                bazel: "bazel".into(),
                script,
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
        let target = self.fetch()?;
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
        if old != target
            && detached
            && commit(&self.repo, "HEAD")?.sha == target
            && !self.modified()?
        {
            self.repo.git(&["checkout", "--quiet", "--detach", old])?;
        }
        Ok(())
    }

    /// Builds the checked out commit and has Bazel write the launch script.
    async fn build(&self) -> Result<()> {
        let mut cmd = tokio::process::Command::new(&self.bazel);
        cmd.arg("run")
            .args(["--color=no", "--noshow_progress"])
            .arg(format!("--script_path={}", self.script.display()));
        if let Some(mode) = &self.mode {
            cmd.args(["-c", mode]);
        }
        cmd.arg(TARGET)
            .current_dir(&self.repo.path)
            .env_remove("BAZELISK_SKIP_WRAPPER")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // Ctrl-C on the server shouldn't reach it directly.
            .process_group(0);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("can't run {}", self.bazel.display()))?;
        let stderr = agent::stderr_tail(child.stderr.take().expect("stderr is piped"));
        let status = child.wait().await?;
        if !status.success() {
            bail!(
                "the build failed ({status})\n{}",
                stderr.await.unwrap_or_default()
            );
        }
        Ok(())
    }
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
    /// What the user decided about the waiting update.
    decision: Option<Decision>,
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

    /// Waits until an update is ready to restart, and returns the script
    /// to exec.
    pub async fn restart_requested(&self) -> PathBuf {
        self.restart.notified().await;
        let build = self.build.as_ref().expect("only a build restarts");
        build.script.clone()
    }

    /// Takes the waiting update's decision; false if none is waiting.
    fn decide(&self, decision: Decision) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if !matches!(inner.phase, UpdatePhase::Waiting { .. }) {
            return false;
        }
        inner.decision = Some(decision);
        drop(inner);
        self.decided.notify_one();
        true
    }

    /// Restarts without waiting for the sessions.
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

/// Stops waiting to restart, or dismisses a failure.
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

/// Runs blocking git work on the clone. The task holds the lock itself, so
/// a page closing mid-request doesn't let other work in early.
async fn with_clone<T: Send + 'static>(
    state: &Arc<State>,
    f: impl FnOnce(&Build) -> Result<T> + Send + 'static,
) -> Result<T> {
    let guard = state.update.git.clone().lock_owned().await;
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        f(state
            .update
            .build
            .as_ref()
            .context("the server wasn't started with bazel run")?)
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
            Ok(target) => wait(&state, target).await,
            Err(e) => {
                tracing::warn!("can't update: {e:#}");
                let error = format!("{e:#}");
                set_phase(&state, UpdatePhase::Failed { error });
            }
        }
    });
    true
}

/// Checks out the branch's tip, builds it, and returns it.
async fn prepare(state: &Arc<State>) -> Result<String> {
    let (old, target) = with_clone(state, Build::check_out).await?;
    tracing::info!("building {target}");
    set_phase(state, UpdatePhase::Building);
    let build = state.update.build.as_ref().context("no build")?;
    if let Err(e) = build.build().await {
        let restored = with_clone(state, move |b| b.restore(&old, &target)).await;
        if let Err(e) = restored {
            tracing::warn!("can't move the clone back: {e:#}");
        }
        return Err(e);
    }
    Ok(target)
}

/// Waits until no session is busy or the user decides.
async fn wait(state: &Arc<State>, target: String) {
    loop {
        let sessions = agent::busy_sessions(state);
        {
            let mut inner = state.update.inner.lock().unwrap();
            let next = match inner.decision.take() {
                Some(Decision::Cancel) => UpdatePhase::Idle,
                Some(Decision::Force) => UpdatePhase::Restarting,
                None if sessions.is_empty() => UpdatePhase::Restarting,
                None => UpdatePhase::Waiting {
                    target: target.clone(),
                    sessions,
                },
            };
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

    fn build(f: &Fixture, bazel: &str) -> Build {
        run(&f.clone.path, &["checkout", "-q", "--detach"]);
        let exe = f._dir.path().join("bazel");
        crate::test_util::write_executable(&exe, bazel);
        Build {
            repo: f.clone.clone(),
            running: commit(&f.clone, "HEAD").unwrap(),
            mode: Some("opt".into()),
            bazel: exe,
            script: f._dir.path().join("restart.sh"),
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
        let b = build(&f, "#!/bin/sh\necho \"$@\" > args\n");
        let script = b.script.clone();
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
        assert_eq!(got.unwrap(), script);
        assert_eq!(state.update.status().phase, UpdatePhase::Restarting);
        assert!(!start(&state), "one at a time");
        assert_eq!(head(&f), target);
        let args = std::fs::read_to_string(f.clone.path.join("args")).unwrap();
        assert_eq!(
            args.trim(),
            format!(
                "run --color=no --noshow_progress --script_path={} -c opt //grenadine/server",
                script.display()
            )
        );
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
    async fn waits_for_busy_sessions() {
        let f = fixture();
        let state = state(Some(build(&f, "#!/bin/sh\n")));
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
        let state = state(Some(build(&f, "#!/bin/sh\n")));
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
