//! Serves the web UI's files, which the binary embeds at build time.

use std::sync::{Arc, OnceLock};

use axum::extract::State as AxState;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use crate::sync::State;

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

/// index.html with the commit the server runs in a `<meta>`, so that the
/// page knows which version it is whichever server answers it later.
fn with_commit(html: &[u8], sha: &str) -> Vec<u8> {
    let html = String::from_utf8_lossy(html);
    let meta = format!("<meta name=\"grenadine-commit\" content=\"{sha}\" /></head>");
    html.replacen("</head>", &meta, 1).into_bytes()
}

pub async fn serve(AxState(state): AxState<Arc<State>>, uri: Uri) -> Response {
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
    let Some(asset) = asset else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let running = state.update.status().running;
    let bytes = match running {
        Some(commit) if std::ptr::eq(asset, &assets[0]) => {
            with_commit(asset.bytes, &commit.sha).into()
        }
        _ => axum::body::Bytes::from_static(asset.bytes),
    };
    (
        [
            (header::CONTENT_TYPE, asset.content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_is_told_its_commit() {
        let html = with_commit(b"<head><title>t</title></head><body></body>", "abc");
        assert_eq!(
            String::from_utf8(html).unwrap(),
            "<head><title>t</title><meta name=\"grenadine-commit\" content=\"abc\" /></head><body></body>"
        );
    }
}
