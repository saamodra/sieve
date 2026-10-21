//! The embedded frontend (`ui/dist`).

use axum::{
    http::{StatusCode, Uri, header},
    response::{Html, IntoResponse, Response},
};
use rust_embed::RustEmbed;

/// `allow_missing` keeps a clean checkout building before `npm run build`.
#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
struct Dist;

const PLACEHOLDER: &str = "<!doctype html><meta charset=utf-8><title>sieve</title>\
<body style=\"font:14px system-ui;padding:2rem\"><h1>sieve</h1>\
<p>The web UI has not been built. Run <code>npm ci &amp;&amp; npm run build</code> in <code>ui/</code> \
and rebuild.</p><p>The API is available under <code>/api</code>.</p></body>";

pub async fn serve_asset(uri: Uri) -> Response {
    let requested = uri.path().trim_start_matches('/');
    let path = if requested.is_empty() {
        "index.html"
    } else {
        requested
    };
    if let Some(file) = Dist::get(path) {
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        let cache = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        return (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, cache.to_string()),
            ],
            file.data.into_owned(),
        )
            .into_response();
    }
    // Paths with an extension are real asset misses; anything else is an SPA route.
    if requested
        .rsplit('/')
        .next()
        .is_some_and(|s| s.contains('.'))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    match Dist::get("index.html") {
        Some(index) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            index.data.into_owned(),
        )
            .into_response(),
        None => Html(PLACEHOLDER).into_response(),
    }
}
