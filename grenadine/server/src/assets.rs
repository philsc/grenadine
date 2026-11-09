//! Serves the web UI's files, which the binary embeds at build time.

use std::sync::OnceLock;

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

/// One of the web UI's files.
pub struct Asset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

static ASSETS: OnceLock<&'static [Asset]> = OnceLock::new();

/// Sets the files to serve. The first one is index.html.
pub fn set(assets: &'static [Asset]) {
    assert!(ASSETS.set(assets).is_ok(), "the assets are already set");
}

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path();
    let assets: &[Asset] = ASSETS.get().copied().unwrap_or_default();
    let asset = assets
        .iter()
        .find(|a| a.path == path)
        // The page routes on the client, so every other non-API path gets
        // index.html.
        .or_else(|| {
            (!path.starts_with("/api/") && !path.contains('.'))
                .then(|| assets.first())
                .flatten()
        });
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
