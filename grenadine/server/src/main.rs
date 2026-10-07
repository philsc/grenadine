//! grenadine: a local web server for reviewing GitHub PRs version by version.

mod api;
mod assets;
mod db;
mod git;
mod github;
mod inboxes;
mod stack;
mod sync;

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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
    let github = github::GitHub::new(&token)?;

    let mut signals = Signals::new()?;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let state = Arc::new(sync::State {
        db,
        github,
        repos,
        events: broadcast::channel(256).0,
        sync_status: std::sync::Mutex::default(),
        poke: Notify::new(),
        on_demand: std::sync::Mutex::new(BTreeSet::new()),
        shutdown: shutdown.clone(),
    });
    let sync_task = tokio::spawn(sync::run(
        state.clone(),
        Duration::from_secs(args.poll_interval.max(1)),
    ));

    let addr = SocketAddr::new(args.bind, args.port);
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
