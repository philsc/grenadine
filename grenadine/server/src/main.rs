//! grenadine: a local web server for reviewing GitHub PRs version by version.

mod agent;
mod api;
mod assets;
mod auto_review;
mod claude;
mod cleanup;
mod db;
mod devin;
mod git;
mod github;
mod inbox_search;
mod inboxes;
mod mcp;
mod pr_plan;
mod ratelimit;
mod reflow;
mod stack;
mod summary;
mod sync;
mod system;
mod terminal;
mod unstaged;

#[cfg(test)]
mod test_util {
    /// Writes `contents` to `path` as an executable, from a child
    /// process: a write fd open in the test process leaks into the
    /// children other test threads fork, and exec'ing the file while
    /// it's held open for writing fails with "Text file busy".
    pub(crate) fn write_executable(path: &std::path::Path, contents: &str) {
        use std::io::Write as _;
        let mut writer = std::process::Command::new("sh")
            .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        writer
            .stdin
            .take()
            .unwrap()
            .write_all(contents.as_bytes())
            .unwrap();
        assert!(writer.wait().unwrap().success());
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{Mutex, Notify, broadcast};

/// How long shutdown may take before the process exits anyway.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(about = "Review the versions of your GitHub PRs")]
struct Args {
    /// A local clone of a GitHub repository whose PRs to show, as PATH or
    /// PATH:REMOTE. REMOTE, `origin` by default, must point at the GitHub
    /// repository; it only identifies the repository, fetches go over
    /// HTTPS. Repeat for several repositories.
    #[arg(long = "repo", required = true, value_name = "PATH[:REMOTE]")]
    repos: Vec<String>,

    /// The IP address to serve the UI on, e.g. 0.0.0.0 or :: for all
    /// interfaces.
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    bind: IpAddr,

    /// The port to serve the UI on.
    #[arg(long, default_value_t = 8765)]
    port: u16,

    /// The SQLite database. Defaults to $XDG_DATA_HOME/grenadine/grenadine.db.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Seconds between polls of GitHub.
    #[arg(long, default_value_t = 60)]
    poll_interval: u64,

    /// Seconds between searches of the inboxes that don't sync in
    /// background.
    #[arg(long, default_value_t = 600)]
    off_inbox_interval: u64,

    /// A regex matching the trunk branches. Stacks end at a trunk: a PR
    /// into a trunk has no parent PR, and a PR from a trunk has no PRs on
    /// top of it. The regex isn't anchored implicitly and supports
    /// lookaround, e.g. `^(?!dev/)` for everything but `dev/` branches.
    #[arg(long, default_value = stack::DEFAULT_TRUNK, value_parser = fancy_regex::Regex::new)]
    trunk: fancy_regex::Regex,

    /// The GitHub API to talk to; tests point it at a fake.
    #[arg(long, hide = true, default_value = github::API)]
    github_api: String,

    /// The Claude Code executable that agent sessions run.
    #[arg(long, default_value = "claude")]
    claude: PathBuf,

    /// The Devin executable that agent sessions run.
    #[arg(long, default_value = "devin")]
    devin: PathBuf,

    /// The model a Devin session's summary and its cleanup scan run on.
    #[arg(
        long,
        alias = "devin-summary-model",
        default_value = "claude-haiku-5-5-medium"
    )]
    devin_job_model: String,

    /// The model the checks of Devin sessions' cleanup items run on.
    #[arg(long, default_value = "claude-sonnet-5-5-medium")]
    devin_prune_model: String,

    /// The shell that terminals in agent sessions run, as a login shell.
    /// Defaults to the user's login shell.
    #[arg(long)]
    shell: Option<PathBuf>,

    /// Where agent sessions' git worktrees go. Defaults to a `worktrees`
    /// directory next to the database.
    #[arg(long)]
    worktrees: Option<PathBuf>,
}

fn default_db() -> Result<PathBuf> {
    let data = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => {
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".local/share")
        }
    };
    Ok(data.join("grenadine/grenadine.db"))
}

/// The URL that local processes reach the server at.
fn local_url(addr: SocketAddr) -> String {
    let ip = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    format!("http://{}", SocketAddr::new(ip, addr.port()))
}

fn open_repos(specs: &[String], token: &str) -> Result<BTreeMap<String, Arc<sync::ClonedRepo>>> {
    let mut repos = BTreeMap::new();
    for spec in specs {
        // Only split off a remote when what follows the last colon looks
        // like one, so paths with colons still work.
        let (path, remote) = match spec.rsplit_once(':') {
            Some((p, r)) if !r.is_empty() && !r.contains('/') => (p, r),
            _ => (spec.as_str(), "origin"),
        };
        let repo = git::Repo::open(path.as_ref(), remote)?.with_token(token);
        tracing::info!("{} is {}", repo.path.display(), repo.slug);
        if repos.contains_key(&repo.slug) {
            bail!("{} is configured twice", repo.slug);
        }
        repos.insert(
            repo.slug.clone(),
            Arc::new(sync::ClonedRepo {
                repo,
                git_lock: Mutex::new(()),
            }),
        );
    }
    Ok(repos)
}

/// Listens for SIGINT and SIGTERM; both listeners are created up front so
/// that repeated signals are still caught by `wait`.
struct Signals {
    int: Signal,
    term: Signal,
}

impl Signals {
    fn new() -> Result<Signals> {
        Ok(Signals {
            int: signal(SignalKind::interrupt())?,
            term: signal(SignalKind::terminate())?,
        })
    }

    async fn wait(&mut self) {
        tokio::select! {
            _ = self.int.recv() => {}
            _ = self.term.recv() => {}
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Claude Code runs the server's binary as the cleanup jobs' hook.
    if std::env::args().nth(1).as_deref() == Some(cleanup::BASH_HOOK_ARG) {
        std::process::exit(cleanup::bash_hook());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    // Use the process-wide crypto provider bundled with rustls (ring).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let token = github::gh_token()?;
    let repos = open_repos(&args.repos, &token)?;
    let db_path = match args.db {
        Some(p) => p,
        None => default_db()?,
    };
    let db = db::Db::open(&db_path)?;
    tracing::info!("database: {}", db_path.display());
    let github = github::GitHub::new(&token, &args.github_api)?;
    let addr = SocketAddr::new(args.bind, args.port);
    let worktrees = match args.worktrees {
        Some(p) => p,
        None => db_path.with_file_name("worktrees"),
    };
    let slices = agent::systemd_available();
    if !slices {
        tracing::warn!(
            "no systemd user manager: deleting a session only stops the processes still in its worktree"
        );
    }
    // Before serving, since a session being created has a worktree but no
    // row yet. The output bases of what it removes go next.
    if let Err(e) = agent::remove_orphaned_worktrees(&db, &repos, &worktrees) {
        tracing::warn!("can't remove orphaned worktrees: {e:#}");
    }
    // A thread rather than spawn_blocking, which shutting down would wait for.
    std::thread::spawn({
        let worktrees = worktrees.clone();
        move || agent::remove_orphaned_output_bases(&worktrees)
    });
    let mut agents = agent::Agents::new(
        &db,
        args.claude,
        devin::DevinConfig {
            exe: args.devin,
            job_model: args.devin_job_model,
            prune_model: args.devin_prune_model,
        },
        worktrees,
        local_url(addr),
        slices,
        true,
        true,
    )?;
    if let Some(shell) = args.shell {
        agents.terminals = terminal::Terminals::new(shell);
    }
    if let Err(e) = agent::remove_orphaned_images(&db, agents.images()) {
        tracing::warn!("can't remove orphaned images: {e:#}");
    }

    if let Err(e) = auto_review::start_watching(&db, repos.keys(), agent::now()) {
        tracing::warn!("can't set up reviewing PRs: {e:#}");
    }

    let mut signals = Signals::new()?;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let state = Arc::new(sync::State {
        db,
        github,
        agents,
        repos,
        trunk: args.trunk,
        events: broadcast::channel(256).0,
        sync_status: std::sync::Mutex::default(),
        poke: Notify::new(),
        on_demand: std::sync::Mutex::new(BTreeSet::new()),
        watched: std::sync::Mutex::new(BTreeMap::new()),
        off_inbox_interval: Duration::from_secs(args.off_inbox_interval),
        inbox_searches: std::sync::Mutex::new(sync::InboxSearches::default()),
        following_up: std::sync::Mutex::new(BTreeSet::new()),
        review_locks: std::sync::Mutex::default(),
        db_dir: db_path.parent().unwrap_or(db_path.as_path()).to_owned(),
        started_at: agent::now(),
        system: system::Sampler::default(),
        shutdown: shutdown.clone(),
    });
    let sync_task = tokio::spawn(sync::run(
        state.clone(),
        Duration::from_secs(args.poll_interval.max(1)),
    ));
    // Shutting down doesn't wait for it; dropping it kills its claude.
    tokio::spawn(cleanup::prune_loop(state.clone()));
    // Counts the sessions' unstaged changes off the request path.
    tokio::spawn(unstaged::run(state.clone()));
    // Listing the backends' models can take a while, so it starts now
    // rather than with the first page or prompt.
    tokio::spawn({
        let state = state.clone();
        async move {
            state.agents.backends().await;
        }
    });

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("can't listen on {addr}"))?;
    tracing::info!("serving on http://{addr}/");
    let serve_shutdown = shutdown.clone();
    let mut server_task = tokio::spawn(async move {
        axum::serve(listener, api::router(state))
            .with_graceful_shutdown(serve_shutdown.cancelled_owned())
            .await
    });

    tokio::select! {
        _ = signals.wait() => {}
        r = &mut server_task => return r.context("server task panicked")?.context("can't serve"),
    }
    tracing::info!("shutting down");
    shutdown.cancel();
    tokio::select! {
        r = async {
            server_task.await.context("server task panicked")??;
            sync_task.await.context("sync task panicked")
        } => r?,
        // Dropping the runtime would wait forever on spawn_blocking git
        // work, so timeouts and repeated signals exit the process directly.
        _ = tokio::time::sleep(SHUTDOWN_TIMEOUT) => {
            tracing::warn!("shutdown timed out");
            std::process::exit(1);
        }
        _ = signals.wait() => {
            tracing::warn!("forced shutdown");
            std::process::exit(1);
        }
    }
    Ok(())
}
