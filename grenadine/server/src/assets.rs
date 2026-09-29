//! The web UI's files, embedded into the binary at build time.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

struct Asset {
    path: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
}

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
        path: "/grenadine_web.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!(env!("GRENADINE_WEB_JS")),
    },
    Asset {
        path: "/grenadine_web_bg.wasm",
        content_type: "application/wasm",
        bytes: include_bytes!(env!("GRENADINE_WEB_WASM")),
    },
];

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path();
    let asset = ASSETS
        .iter()
        .find(|a| a.path == path)
        // The page routes on the client, so every other non-API path gets
        // index.html.
        .or_else(|| (!path.starts_with("/api/") && !path.contains('.')).then_some(&ASSETS[0]));
    match asset {
        Some(a) => (
            [
                (header::CONTENT_TYPE, a.content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            a.bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
