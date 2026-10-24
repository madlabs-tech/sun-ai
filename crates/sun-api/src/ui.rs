//! The web UI (`web/dist`), embedded with rust-embed and served at `/`.
//! The build replaces each text asset over 1 KB with `<file>.gz`, so the
//! binary holds one copy (40 MB budget). It goes out with
//! `Content-Encoding: gzip` when the client accepts it, else decompressed.
//! The UI uses a hash router, so there is no SPA fallback: an unknown path is
//! a 404.
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
use flate2::read::GzDecoder;
use rust_embed::RustEmbed;
use std::io::Read;

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
    serve_from::<Ui>(&method, path, &headers)
}

fn serve_from<E: RustEmbed>(method: &Method, path: &str, headers: &HeaderMap) -> Response {
    let path = if path.is_empty() { "index.html" } else { path };
    if path.split('/').any(|part| part == "..") || (method != Method::GET && method != Method::HEAD)
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (data, gzipped) = if let Some(gz) = E::get(&format!("{path}.gz")) {
        (gz.data, true)
    } else if let Some(file) = E::get(path) {
        (file.data, false)
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // From the requested name, not the embedded `.gz`.
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
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
    if !gzipped {
        return (common, data).into_response();
    }
    if accepts_gzip(headers) {
        return (common, [(CONTENT_ENCODING, "gzip")], data).into_response();
    }
    let mut raw = Vec::new();
    match GzDecoder::new(&data[..]).read_to_end(&mut raw) {
        Ok(_) => (common, raw).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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

    /// A built UI in miniature: raw `index.html`, gzipped-only assets.
    #[derive(RustEmbed)]
    #[folder = "testdata/ui"]
    struct Fixture;

    const JS: &[u8] = b"console.log('sun');\n";
    const CSS: &[u8] = b"body { margin: 0; }\n";

    async fn get(path: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut headers = HeaderMap::new();
        if let Some(accept) = accept {
            headers.insert(ACCEPT_ENCODING, accept.parse().unwrap());
        }
        let response = serve_from::<Fixture>(&Method::GET, path, &headers);
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (parts.status, parts.headers, body.to_vec())
    }

    #[tokio::test]
    async fn gzip_client_gets_the_embedded_gz() {
        let (status, headers, body) = get("assets/app.js", Some("gzip, br")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[CONTENT_ENCODING], "gzip");
        assert_eq!(headers[VARY], "Accept-Encoding");
        let gz = Fixture::get("assets/app.js.gz").unwrap().data;
        assert_eq!(body, gz.as_ref());
    }

    #[tokio::test]
    async fn other_clients_get_identical_raw_bytes() {
        for accept in [None, Some("br"), Some("gzip;q=0")] {
            let (status, headers, body) = get("assets/app.js", accept).await;
            assert_eq!(status, StatusCode::OK);
            assert!(headers.get(CONTENT_ENCODING).is_none(), "{accept:?}");
            assert_eq!(body, JS, "{accept:?}");
        }
        assert_eq!(get("assets/app.css", None).await.2, CSS);
    }

    #[tokio::test]
    async fn content_type_comes_from_the_requested_name() {
        for (path, mime) in [
            ("assets/app.js", "text/javascript"),
            ("assets/app.css", "text/css"),
            ("index.html", "text/html"),
        ] {
            for accept in [None, Some("gzip")] {
                let (_, headers, _) = get(path, accept).await;
                assert_eq!(headers[CONTENT_TYPE], mime, "{path} {accept:?}");
            }
        }
    }

    #[tokio::test]
    async fn root_serves_index_with_headers() {
        let (status, headers, body) = get("", Some("gzip")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get(CONTENT_ENCODING).is_none());
        assert_eq!(headers[CACHE_CONTROL], "no-cache");
        assert_eq!(headers[CONTENT_SECURITY_POLICY], CSP);
        assert_eq!(body, Fixture::get("index.html").unwrap().data.as_ref());
        let (_, headers, _) = get("assets/app.css", None).await;
        assert_eq!(
            headers[CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
    }

    #[tokio::test]
    async fn rejects_traversal_unknown_paths_and_writes() {
        assert_eq!(
            get("assets/../index.html", None).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get("missing.js", Some("gzip")).await.0,
            StatusCode::NOT_FOUND
        );
        let response = serve_from::<Fixture>(&Method::POST, "index.html", &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

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
