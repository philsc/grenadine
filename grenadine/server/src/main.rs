//! grenadine: a local web server for reviewing GitHub PRs version by version.

mod api;
mod assets;
mod db;
mod git;
mod github;
mod sync;

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::sync::{Mutex, Notify, broadcast};

#[derive(Parser)]
#[command(about = "Review the versions of your GitHub PRs")]
struct Args {
    /// A local clone of a GitHub repository whose PRs to show, as PATH or
    /// PATH:REMOTE. REMOTE, `origin` by default, must point at the GitHub
    /// repository. Repeat for several repositories.
    #[arg(long = "repo", required = true, value_name = "PATH[:REMOTE]")]
    repos: Vec<String>,

    /// The port to serve the UI on, on 127.0.0.1.
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

fn open_repos(specs: &[String]) -> Result<BTreeMap<String, Arc<Mutex<git::Repo>>>> {
    let mut repos = BTreeMap::new();
    for spec in specs {
        // Only split off a remote when what follows the last colon looks
        // like one, so paths with colons still work.
        let (path, remote) = match spec.rsplit_once(':') {
            Some((p, r)) if !r.is_empty() && !r.contains('/') => (p, r),
            _ => (spec.as_str(), "origin"),
        };
        let repo = git::Repo::open(path.as_ref(), remote)?;
        tracing::info!("{} is {}", repo.path.display(), repo.slug);
        if repos.contains_key(&repo.slug) {
            bail!("{} is configured twice", repo.slug);
        }
        repos.insert(repo.slug.clone(), Arc::new(Mutex::new(repo)));
    }
    Ok(repos)
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

    let repos = open_repos(&args.repos)?;
    let db_path = match args.db {
        Some(p) => p,
        None => default_db()?,
    };
    let db = db::Db::open(&db_path)?;
    tracing::info!("database: {}", db_path.display());
    let github = github::GitHub::new(&github::gh_token()?)?;

    let state = Arc::new(sync::State {
        db,
        github,
        repos,
        events: broadcast::channel(256).0,
        poke: Notify::new(),
    });
    tokio::spawn(sync::run(
        state.clone(),
        Duration::from_secs(args.poll_interval.max(1)),
    ));

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, args.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("can't listen on {addr}"))?;
    tracing::info!("serving on http://{addr}/");
    axum::serve(listener, api::router(state)).await?;
    Ok(())
}
