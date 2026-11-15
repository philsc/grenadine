//! Keeps Claude Code up to date. Claude Code updates itself only when it's
//! launched interactively, never under `claude -p`, so a host where it only
//! runs through grenadine would stay on one version. Running `claude update`
//! is safe while sessions run: a native install puts the new version next
//! to the old one and moves the `claude` symlink, so sessions that are
//! running keep their binary and only new ones start the new version.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;

/// How long one `claude update` may take, download included.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Runs `claude update` now and then every `interval` until shutdown.
/// Logs each run's result; a failed run waits for the next one.
pub async fn run(claude: PathBuf, interval: Duration, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            result = update(&claude, UPDATE_TIMEOUT) => match result {
                Ok(output) => tracing::info!("claude update: {output}"),
                Err(e) => tracing::warn!("claude update: {e:#}"),
            },
            _ = shutdown.cancelled() => break,
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.cancelled() => break,
        }
    }
}

/// Runs `claude update` once and returns what it printed.
async fn update(claude: &Path, timeout: Duration) -> Result<String> {
    let child = tokio::process::Command::new(claude)
        .arg("update")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("can't run {}", claude.display()))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .with_context(|| format!("timed out after {}s", timeout.as_secs()))??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    if !output.status.success() {
        // `claude update` may print its reason to either stream.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut message = output.status.to_string();
        for text in [stderr.trim(), stdout] {
            if !text.is_empty() {
                message.push('\n');
                message.push_str(text);
            }
        }
        bail!(message);
    }
    Ok(stdout.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::write_executable;

    /// A fake `claude` that appends its arguments to `calls` and runs `body`.
    fn fake(dir: &Path, body: &str) -> (PathBuf, PathBuf) {
        let claude = dir.join("claude");
        let calls = dir.join("calls");
        write_executable(
            &claude,
            &format!("#!/bin/sh\necho \"$@\" >> '{}'\n{body}\n", calls.display()),
        );
        (claude, calls)
    }

    fn calls(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn update_returns_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let (claude, calls_file) = fake(dir.path(), "echo 'Claude Code is up to date (1.0.0)'");
        let output = update(&claude, UPDATE_TIMEOUT).await.unwrap();
        assert_eq!(output, "Claude Code is up to date (1.0.0)");
        assert_eq!(calls(&calls_file), ["update"]);
    }

    #[tokio::test]
    async fn update_reports_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (claude, _) = fake(dir.path(), "echo 'not a native install' >&2; exit 3");
        let error = format!("{:#}", update(&claude, UPDATE_TIMEOUT).await.unwrap_err());
        assert!(error.contains("exit status: 3"), "{error}");
        assert!(error.contains("not a native install"), "{error}");
    }

    #[tokio::test]
    async fn update_reports_a_failure_printed_to_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let (claude, _) = fake(dir.path(), "echo 'network error'; exit 1");
        let error = format!("{:#}", update(&claude, UPDATE_TIMEOUT).await.unwrap_err());
        assert_eq!(error, "exit status: 1\nnetwork error");
    }

    #[tokio::test]
    async fn update_reports_both_outputs_of_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (claude, _) = fake(
            dir.path(),
            "echo 'Checking for updates'; echo 'not a native install' >&2; exit 1",
        );
        let error = format!("{:#}", update(&claude, UPDATE_TIMEOUT).await.unwrap_err());
        assert_eq!(
            error,
            "exit status: 1\nnot a native install\nChecking for updates"
        );
    }

    #[tokio::test]
    async fn update_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let (claude, _) = fake(dir.path(), "exec sleep 60");
        let error = format!(
            "{:#}",
            update(&claude, Duration::from_millis(100))
                .await
                .unwrap_err()
        );
        assert!(error.contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn update_without_claude_fails() {
        let dir = tempfile::tempdir().unwrap();
        let error = update(&dir.path().join("missing"), UPDATE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("can't run"), "{error:#}");
    }

    #[tokio::test]
    async fn run_updates_at_once_and_then_every_interval() {
        let dir = tempfile::tempdir().unwrap();
        // A failing run doesn't stop the later ones.
        let (claude, calls_file) = fake(dir.path(), "exit 1");
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run(claude, Duration::from_millis(50), shutdown.clone()));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while calls(&calls_file).len() < 3 {
            assert!(tokio::time::Instant::now() < deadline, "too few updates");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("stops on shutdown")
            .unwrap();
        assert!(calls(&calls_file).iter().all(|c| c == "update"));
    }
}
