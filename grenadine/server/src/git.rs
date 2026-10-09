//! Operations on a local clone, done by running `git`.
//!
//! Everything here blocks; async callers go through `spawn_blocking`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use grenadine_core::api::{Blob, ChangeStatus, FileChange};
use grenadine_core::versions::CommitGraph;

/// Refs that grenadine writes into the clone live under this prefix.
pub const REF_PREFIX: &str = "refs/grenadine/pr";

/// Blobs larger than this are sent to the page as binary, without contents.
const MAX_TEXT_BLOB: u64 = 4 << 20;

/// A `git fetch` credential header, redacted in `Debug` output so a
/// `Repo` can be logged without leaking the token.
#[derive(Clone)]
struct AuthHeader(String);

impl std::fmt::Debug for AuthHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthHeader(<redacted>)")
    }
}

/// A local clone of a GitHub repository.
#[derive(Clone, Debug)]
pub struct Repo {
    pub path: PathBuf,
    /// `owner/name` on GitHub.
    pub slug: String,
    /// The `http.https://github.com/.extraHeader` value sent on fetches.
    auth: Option<AuthHeader>,
}

/// Parses `owner/name` out of a GitHub remote URL.
fn github_slug(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

impl Repo {
    /// Opens the clone at `path`. `remote` must point at github.com; it
    /// only identifies the repository, fetches go over HTTPS.
    pub fn open(path: &Path, remote: &str) -> Result<Repo> {
        let mut repo = Repo {
            path: path
                .canonicalize()
                .with_context(|| format!("no such directory: {}", path.display()))?,
            slug: String::new(),
            auth: None,
        };
        // Read the configured URL rather than `git remote get-url`, which
        // applies `url.*.insteadOf` rewrites.
        let url = repo
            .git(&["config", "--get", &format!("remote.{remote}.url")])
            .with_context(|| format!("{} has no remote named {remote}", path.display()))?;
        repo.slug = github_slug(url.trim())
            .ok_or_else(|| anyhow!("remote {remote} ({}) is not a github.com URL", url.trim()))?;
        Ok(repo)
    }

    /// Authenticates HTTPS fetches to github.com with a GitHub token.
    pub fn with_token(mut self, token: &str) -> Repo {
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("x-access-token:{token}"));
        self.auth = Some(AuthHeader(format!("Authorization: Basic {basic}")));
        self
    }

    /// The HTTPS URL fetches go to, regardless of the remote's own URL.
    fn fetch_url(&self) -> String {
        format!("https://github.com/{}.git", self.slug)
    }

    fn command(&self) -> Command {
        let mut c = Command::new("git");
        c.arg("-C").arg(&self.path);
        // Never prompt for credentials; a fetch that needs them should fail.
        c.env("GIT_TERMINAL_PROMPT", "0");
        if let Some(auth) = &self.auth {
            // Pass the credential through the environment so the token
            // never appears in argv, a URL, or a file.
            c.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraHeader")
                .env("GIT_CONFIG_VALUE_0", &auth.0);
        }
        // A new session has no controlling terminal, so ssh or askpass
        // helpers can't open /dev/tty to prompt and stop the process with
        // SIGTTIN/SIGTTOU. It also keeps git out of the terminal's
        // foreground process group, so Ctrl-C lets an in-flight sync
        // finish during shutdown.
        // SAFETY: setsid is async-signal-safe and the closure touches no
        // memory outside the call.
        unsafe {
            c.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        c
    }

    /// Runs git and returns its stdout.
    fn git(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .context("failed to run git")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8(out.stdout)?)
    }

    /// Runs git and reports only whether it succeeded.
    fn git_ok(&self, args: &[&str]) -> bool {
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    pub fn has_commit(&self, sha: &str) -> bool {
        self.git_ok(&["cat-file", "-e", &format!("{sha}^{{commit}}")])
    }

    /// Fetches commits by SHA over HTTPS from the GitHub repository,
    /// skipping ones the clone already has. Returns the SHAs that could
    /// not be fetched.
    pub fn fetch_commits(&self, shas: &[String]) -> Vec<String> {
        let mut wanted: Vec<&str> = shas
            .iter()
            .map(String::as_str)
            .filter(|s| !self.has_commit(s))
            .collect();
        wanted.sort();
        wanted.dedup();
        if wanted.is_empty() {
            return Vec::new();
        }
        let url = self.fetch_url();
        let fetch = |shas: &[&str]| {
            let mut args = vec![
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                &url,
            ];
            args.extend_from_slice(shas);
            self.git(&args)
        };
        if fetch(&wanted).is_ok() {
            return Vec::new();
        }
        // One unavailable commit fails the whole fetch; find out which.
        wanted
            .into_iter()
            .filter(|sha| {
                let result = fetch(&[sha]);
                if let Err(e) = &result {
                    tracing::warn!("{}: can't fetch {sha}: {e:#}", self.slug);
                }
                result.is_err()
            })
            .map(str::to_owned)
            .collect()
    }

    /// Fetches the tip of a branch over HTTPS from the GitHub repository
    /// into `local_ref` and returns it.
    pub fn fetch_branch(&self, branch: &str, local_ref: &str) -> Result<String> {
        let refspec = format!("+refs/heads/{branch}:{local_ref}");
        self.git(&[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            &self.fetch_url(),
            &refspec,
        ])?;
        Ok(self.git(&["rev-parse", local_ref])?.trim().to_owned())
    }

    pub fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        self.git(&["merge-base", a, b])
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    /// The refs under `prefix` with the commits they point at.
    pub fn refs(&self, prefix: &str) -> Result<BTreeMap<String, String>> {
        let out = self.git(&["for-each-ref", "--format=%(refname) %(objectname)", prefix])?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(r, o)| (r.to_owned(), o.to_owned()))
            .collect())
    }

    /// Points `set` refs at their commits and deletes the `delete` refs, all
    /// in one transaction.
    pub fn update_refs(&self, set: &[(String, String)], delete: &[String]) -> Result<()> {
        let mut input = String::from("start\n");
        for (r, sha) in set {
            input += &format!("update {r} {sha}\n");
        }
        for r in delete {
            input += &format!("delete {r}\n");
        }
        input += "commit\n";
        let mut child = self
            .command()
            .args(["update-ref", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "git update-ref failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// The files that differ between the trees of two commits. Only trees
    /// are compared here; the page diffs the contents.
    pub fn changed_files(&self, from: &str, to: &str) -> Result<Vec<FileChange>> {
        let out = self.git(&[
            "diff-tree",
            "-r",
            "-z",
            "-M",
            "--raw",
            "--no-commit-id",
            from,
            to,
        ])?;
        parse_raw_diff(&out)
    }

    /// The contents of blobs.
    pub fn blobs(&self, ids: &[String]) -> Result<BTreeMap<String, Blob>> {
        let mut child = self
            .command()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let input: String = ids.iter().map(|id| format!("{id}\n")).collect();
        let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut blobs = BTreeMap::new();
        for id in ids {
            let mut header = String::new();
            stdout.read_line(&mut header)?;
            let fields: Vec<&str> = header.split_whitespace().collect();
            if fields.get(1) == Some(&"missing") {
                continue;
            }
            let [_, kind, size] = fields[..] else {
                bail!("unexpected cat-file output: {header}");
            };
            let size: u64 = size.parse()?;
            let mut data = vec![0; size as usize + 1];
            stdout.read_exact(&mut data)?;
            data.pop();
            if kind != "blob" {
                continue;
            }
            let text = if size <= MAX_TEXT_BLOB && !data.contains(&0) {
                String::from_utf8(data).ok()
            } else {
                None
            };
            blobs.insert(id.clone(), Blob { text, size });
        }
        writer.join().unwrap()?;
        child.wait()?;
        Ok(blobs)
    }
}

impl CommitGraph for Repo {
    fn first_parent_range(&self, from: &str, to: &str) -> Option<Vec<String>> {
        if !self.git_ok(&["merge-base", "--is-ancestor", from, to]) {
            return None;
        }
        let out = self
            .git(&[
                "rev-list",
                "--reverse",
                "--first-parent",
                &format!("{from}..{to}"),
            ])
            .ok()?;
        Some(out.lines().map(str::to_owned).collect())
    }

    fn has(&self, sha: &str) -> bool {
        self.has_commit(sha)
    }
}

const NULL_OID: &str = "0000000000000000000000000000000000000000";

fn parse_raw_diff(out: &str) -> Result<Vec<FileChange>> {
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let mut files = Vec::new();
    while let Some(meta) = fields.next() {
        // :old_mode new_mode old_oid new_oid status
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_oid, new_oid, status] = parts[..] else {
            bail!("unexpected diff-tree output: {meta}");
        };
        let mut path = || {
            fields
                .next()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("truncated diff-tree output"))
        };
        let (status, old_path, new_path) = match status.chars().next() {
            Some('A') => (ChangeStatus::Added, None, Some(path()?)),
            Some('D') => (ChangeStatus::Deleted, Some(path()?), None),
            Some('R') => (ChangeStatus::Renamed, Some(path()?), Some(path()?)),
            Some('C') => (ChangeStatus::Copied, Some(path()?), Some(path()?)),
            Some('T') => {
                let p = path()?;
                (ChangeStatus::TypeChanged, Some(p.clone()), Some(p))
            }
            _ => {
                let p = path()?;
                (ChangeStatus::Modified, Some(p.clone()), Some(p))
            }
        };
        // Submodules (mode 160000) have commits, not blobs.
        let blob =
            |mode: &str, oid: &str| (oid != NULL_OID && mode != "160000").then(|| oid.to_owned());
        files.push(FileChange {
            status,
            old_blob: old_path.as_ref().and_then(|_| blob(old_mode, old_oid)),
            new_blob: new_path.as_ref().and_then(|_| blob(new_mode, new_oid)),
            old_path,
            new_path,
        });
    }
    Ok(files)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(
            github_slug("git@github.com:a/b.git").as_deref(),
            Some("a/b")
        );
        assert_eq!(
            github_slug("https://github.com/a/b").as_deref(),
            Some("a/b")
        );
        assert_eq!(
            github_slug("https://github.com/a/b.git/").as_deref(),
            Some("a/b")
        );
        assert_eq!(
            github_slug("ssh://git@github.com/a/b.git").as_deref(),
            Some("a/b")
        );
        assert_eq!(github_slug("https://gitlab.com/a/b"), None);
    }

    pub(crate) struct Fixture {
        pub(crate) _dir: tempfile::TempDir,
        pub(crate) upstream: PathBuf,
        pub(crate) clone: Repo,
    }

    pub(crate) fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    pub(crate) fn commit(dir: &Path, file: &str, contents: &str) -> String {
        std::fs::write(dir.join(file), contents).unwrap();
        run(dir, &["add", file]);
        run(dir, &["commit", "-q", "-m", file]);
        run(dir, &["rev-parse", "HEAD"])
    }

    /// An "upstream" repository and a clone of it whose remote is renamed to
    /// look like GitHub, with HTTPS fetches redirected to the upstream
    /// directory.
    pub(crate) fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("upstream");
        let clone = dir.path().join("clone");
        std::fs::create_dir(&upstream).unwrap();
        run(&upstream, &["init", "-q", "-b", "main"]);
        // GitHub lets clients fetch any commit by SHA.
        run(
            &upstream,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        commit(&upstream, "a", "1\n");
        run(
            dir.path(),
            &["clone", "-q", upstream.to_str().unwrap(), "clone"],
        );
        run(
            &clone,
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:owner/name.git",
            ],
        );
        run(
            &clone,
            &[
                "config",
                &format!("url.{}.insteadOf", upstream.display()),
                "https://github.com/owner/name.git",
            ],
        );
        let clone = Repo::open(&clone, "origin").unwrap();
        Fixture {
            _dir: dir,
            upstream,
            clone,
        }
    }

    #[test]
    fn open_reads_the_slug() {
        let f = fixture();
        assert_eq!(f.clone.slug, "owner/name");
        assert!(Repo::open(&f.clone.path, "nope").is_err());
    }

    #[test]
    fn debug_does_not_leak_the_token() {
        let f = fixture();
        let repo = f.clone.with_token("hunter2");
        let debug = format!("{repo:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn token_goes_to_git_config_env() {
        let f = fixture();
        let repo = f.clone.with_token("secret");
        let header = repo
            .git(&["config", "--get", "http.https://github.com/.extraheader"])
            .unwrap();
        assert_eq!(
            header.trim(),
            "Authorization: Basic eC1hY2Nlc3MtdG9rZW46c2VjcmV0"
        );
    }

    #[test]
    fn fetches_unreachable_commits_by_sha() {
        let f = fixture();
        let clone = f.clone.with_token("secret");
        run(&f.upstream, &["checkout", "-q", "-b", "pr"]);
        let c1 = commit(&f.upstream, "b", "1\n");
        let c2 = commit(&f.upstream, "b", "2\n");
        // Rewrite the branch so that c2 is unreachable upstream.
        run(&f.upstream, &["reset", "-q", "--hard", &c1]);
        let c3 = commit(&f.upstream, "b", "3\n");

        let bogus = "1234567890123456789012345678901234567890".to_owned();
        let failed = clone.fetch_commits(&[c2.clone(), c3.clone(), bogus.clone()]);
        assert_eq!(failed, [bogus]);
        assert!(clone.has_commit(&c2));
        assert!(clone.has_commit(&c3));

        assert_eq!(clone.first_parent_range(&c1, &c3), Some(vec![c3.clone()]));
        assert_eq!(clone.first_parent_range(&c2, &c3), None);
    }

    #[test]
    fn refs_and_merge_bases() {
        let f = fixture();
        let main = run(&f.upstream, &["rev-parse", "HEAD"]);
        run(&f.upstream, &["checkout", "-q", "-b", "pr"]);
        let v1 = commit(&f.upstream, "b", "1\n");
        run(&f.upstream, &["checkout", "-q", "main"]);
        commit(&f.upstream, "c", "1\n");

        let target = format!("{REF_PREFIX}/1/target");
        f.clone.fetch_branch("main", &target).unwrap();
        assert!(f.clone.fetch_commits(std::slice::from_ref(&v1)).is_empty());
        assert_eq!(f.clone.merge_base(&v1, &target), Some(main));

        let v1_ref = format!("{REF_PREFIX}/1/v1");
        f.clone
            .update_refs(&[(v1_ref.clone(), v1.clone())], &[])
            .unwrap();
        let refs = f.clone.refs(REF_PREFIX).unwrap();
        assert_eq!(refs.get(&v1_ref), Some(&v1));
        f.clone.update_refs(&[], std::slice::from_ref(&v1_ref)).unwrap();
        assert!(!f.clone.refs(REF_PREFIX).unwrap().contains_key(&v1_ref));
    }

    #[test]
    fn changed_files_and_blobs() {
        let f = fixture();
        let base = run(&f.upstream, &["rev-parse", "HEAD"]);
        commit(&f.upstream, "b", "hello\n");
        std::fs::write(f.upstream.join("bin"), b"\0\x01").unwrap();
        run(&f.upstream, &["add", "bin"]);
        run(&f.upstream, &["rm", "-q", "a"]);
        run(&f.upstream, &["commit", "-q", "-m", "x"]);
        let head = run(&f.upstream, &["rev-parse", "HEAD"]);
        f.clone.fetch_commits(std::slice::from_ref(&head));

        let files = f.clone.changed_files(&base, &head).unwrap();
        let summary: Vec<_> = files
            .iter()
            .map(|c| (c.status, c.path().to_owned()))
            .collect();
        assert_eq!(
            summary,
            [
                (ChangeStatus::Deleted, "a".to_owned()),
                (ChangeStatus::Added, "b".to_owned()),
                (ChangeStatus::Added, "bin".to_owned()),
            ]
        );
        let ids: Vec<String> = files
            .iter()
            .flat_map(|c| c.old_blob.iter().chain(&c.new_blob))
            .cloned()
            .collect();
        let blobs = f.clone.blobs(&ids).unwrap();
        assert_eq!(
            blobs[files[0].old_blob.as_ref().unwrap()].text.as_deref(),
            Some("1\n")
        );
        assert_eq!(
            blobs[files[1].new_blob.as_ref().unwrap()].text.as_deref(),
            Some("hello\n")
        );
        assert_eq!(blobs[files[2].new_blob.as_ref().unwrap()].text, None);
    }
}
