//! The grenadine binary: the server with the web UI's files embedded.

use anyhow::Result;
use grenadine_server::assets::{self, Asset};

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

#[tokio::main]
async fn main() -> Result<()> {
    assets::set(ASSETS);
    grenadine_server::run().await
}
