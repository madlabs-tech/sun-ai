//! The web UI (`web/dist`), embedded with rust-embed and served at `/`.
//! The build pre-compresses text assets to `<file>.gz`; those go out with
//! `Content-Encoding: gzip` when the browser accepts it. The UI uses a hash
//! router, so there is no SPA fallback: an unknown path is a 404.
//!
//! In debug builds rust-embed reads `web/dist` from disk at request time; a
//! release build embeds whatever `npm run build` left there (nothing if it is
//! missing, and `/` then answers 404).

use axum::http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_SECURITY_POLICY, CONTENT_TYPE, VARY,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::error::ApiError;

#[derive(RustEmbed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Ui;

/// Same-origin only (C5): the bundle ships its fonts and icons. Inline styles
/// are allowed because components set CSS variables through `style`.
const CSP: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
                   font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'";

/// Fallback for every path no API route matched.
pub(crate) async fn serve(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "This kernel has no such route. It may be older than the UI.",
        )
        .into_response();
    }
    let path = if path.is_empty() { "index.html" } else { path };
    if path.split('/').any(|part| part == "..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let file = match Ui::get(path) {
        Some(file) if method == Method::GET || method == Method::HEAD => file,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mime = file.metadata.mimetype().to_owned();
    // Vite fingerprints everything under assets/; index.html must revalidate.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let common = [
        (CONTENT_TYPE, mime),
        (CACHE_CONTROL, cache.to_owned()),
        (VARY, "Accept-Encoding".to_owned()),
        (X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        (CONTENT_SECURITY_POLICY, CSP.to_owned()),
    ];
    match accepts_gzip(&headers)
        .then(|| Ui::get(&format!("{path}.gz")))
        .flatten()
    {
        Some(gz) => (common, [(CONTENT_ENCODING, "gzip")], gz.data).into_response(),
        None => (common, file.data).into_response(),
    }
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|coding| {
            let mut parts = coding.split(';');
            parts.next().is_some_and(|name| name.trim() == "gzip")
                && parts.all(|p| p.trim().replace(' ', "") != "q=0")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_negotiation() {
        let mut headers = HeaderMap::new();
        assert!(!accepts_gzip(&headers));
        headers.insert(ACCEPT_ENCODING, "br, gzip, deflate".parse().unwrap());
        assert!(accepts_gzip(&headers));
        headers.insert(ACCEPT_ENCODING, "gzip;q=0, br".parse().unwrap());
        assert!(!accepts_gzip(&headers));
        headers.insert(ACCEPT_ENCODING, "gzipx".parse().unwrap());
        assert!(!accepts_gzip(&headers));
    }
}
