//! The grenadine binary: the server with the web UI's files embedded.

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use grenadine_server::Exit;
use grenadine_server::assets::{self, Asset};

/// How long a restart waits for blocking work, and then for killed child
/// processes, to end.
const RESTART_GRACE: Duration = Duration::from_secs(2);

const ASSETS: &[Asset] = &[
    Asset {
        path: "/",
        content_type: "text/html; charset=utf-8",
        bytes: include_bytes!(env!("GRENADINE_INDEX_HTML")),
    },
    Asset {
        path: "/style.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!(env!("GRENADINE_STYLE_CSS")),
    },
    Asset {
        path: "/favicon.svg",
        content_type: "image/svg+xml",
        bytes: include_bytes!(env!("GRENADINE_FAVICON_SVG")),
    },
    Asset {
        path: "/highlight_worker.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!(env!("GRENADINE_HIGHLIGHT_WORKER_JS")),
    },
    Asset {
        path: "/grenadine_web.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!(env!("GRENADINE_WEB_JS")),
    },
    Asset {
        path: "/grenadine_web_bg.wasm",
        content_type: "application/wasm",
        bytes: include_bytes!(env!("GRENADINE_WEB_WASM")),
    },
    Asset {
        path: "/xterm.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!(env!("GRENADINE_XTERM_JS")),
    },
    Asset {
        path: "/xterm.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!(env!("GRENADINE_XTERM_CSS")),
    },
    Asset {
        path: "/addon-fit.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!(env!("GRENADINE_XTERM_FIT_JS")),
    },
];

fn main() -> Result<()> {
    assets::set(ASSETS);
    // Not #[tokio::main]: a restart may only exec once the runtime is gone,
    // since dropping its tasks kills the processes they run.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match runtime.block_on(grenadine_server::run())? {
        Exit::Stop => Ok(()),
        Exit::Restart(binary) => {
            // Nothing reads tokio's signal handlers anymore.
            reset_signals();
            runtime.shutdown_timeout(RESTART_GRACE);
            reap_children();
            exec(&binary)
        }
    }
}

/// Has SIGINT and SIGTERM end the process.
fn reset_signals() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the default action is no handler that could be unsafe.
        unsafe { libc::signal(signal, libc::SIG_DFL) };
    }
}

/// Collects the child processes that have ended: the new version keeps
/// this process's PID, and would keep them as zombies.
fn reap_children() {
    let deadline = Instant::now() + RESTART_GRACE;
    while Instant::now() < deadline {
        // SAFETY: waitpid accepts a null status pointer.
        match unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } {
            -1 => return,
            0 => std::thread::sleep(Duration::from_millis(20)),
            _ => {}
        }
    }
}

/// Replaces this process with `binary`, run with the same arguments.
fn exec(binary: &Path) -> Result<()> {
    let error = std::process::Command::new(binary)
        .args(std::env::args_os().skip(1))
        .exec();
    Err(error).with_context(|| format!("can't run {}", binary.display()))
}
