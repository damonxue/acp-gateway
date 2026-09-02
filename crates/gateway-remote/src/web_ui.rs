//! Hosting the web client.
//!
//! The phone UI is a static bundle built from `web/`. Embedding it in the
//! binary (rather than shipping a directory) is what makes
//! `https://gw.example.com/app` work with no extra moving parts: the tunnel
//! already points at this server, and the `.app` bundle contains exactly one
//! file to install.
//!
//! ## Why the feature is optional
//!
//! Embedding requires `web/dist` to exist, which requires Node and a `pnpm
//! build`. Making that a hard requirement would mean nobody can run
//! `cargo test --workspace` without a JavaScript toolchain, so the `web-ui`
//! feature is off by default and the release bundle turns it on. Without it,
//! `/app` serves a short page explaining how to build the client.
//!
//! ## Why `/app` needs no credential
//!
//! The bundle is public code: HTML, CSS and JavaScript with no secrets. Every
//! byte of *data* it later reads still requires a pairing code or a single-use
//! ticket, so serving it openly costs nothing and saves the phone a
//! chicken-and-egg problem (it cannot authenticate before it has the client
//! that does the authenticating).

use axum::extract::Path;
use axum::http::{HeaderName, HeaderValue, header};
use axum::response::{IntoResponse, Response};

/// No caching for the entry point; hashed assets get [`IMMUTABLE`].
const NO_CACHE: &str = "no-cache";

/// Headers applied to everything under `/app`.
///
/// The CSP is deliberately tight: the client loads no third-party code and
/// talks only to its own origin (over HTTP and WebSocket), so anything else
/// being requested means something is wrong.
fn security_headers() -> [(HeaderName, HeaderValue); 3] {
    [
        (
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                 img-src 'self' data: blob:; media-src 'self' blob:; \
                 connect-src 'self' ws: wss:; frame-ancestors 'none'; base-uri 'none'",
            ),
        ),
        (
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ),
        (
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ),
    ]
}

#[cfg(feature = "web-ui")]
mod embedded {
    use rust_embed::Embed;

    /// Long-lived caching is safe for assets: their names contain a hash.
    pub(super) const IMMUTABLE: &str = "public, max-age=31536000, immutable";

    /// The built web client.
    ///
    /// In debug builds `rust-embed` reads from disk, so `pnpm dev` output is
    /// picked up without recompiling the gateway; release builds embed.
    #[derive(Embed)]
    #[folder = "$CARGO_MANIFEST_DIR/../../web/dist"]
    pub(super) struct Assets;
}

/// `GET /app` — the client's entry point.
pub async fn index() -> Response {
    serve("index.html").await
}

/// `GET /app/{*path}` — assets, with an SPA fallback to `index.html`.
pub async fn asset(Path(path): Path<String>) -> Response {
    serve(&path).await
}

#[cfg(feature = "web-ui")]
async fn serve(path: &str) -> Response {
    use axum::http::StatusCode;
    use embedded::{Assets, IMMUTABLE};

    let requested = path.trim_start_matches('/');
    let entry_point = requested.is_empty() || requested == "index.html";
    let (file, cache) = match Assets::get(requested) {
        Some(file) if !entry_point => (Some(file), IMMUTABLE),
        // Unknown paths fall back to the entry point: the client keeps its
        // routing in memory, so a reload of any URL must still boot the app.
        _ => (Assets::get("index.html"), NO_CACHE),
    };

    let Some(file) = file else {
        return (StatusCode::NOT_FOUND, "web client not built").into_response();
    };
    (
        security_headers(),
        [
            (header::CONTENT_TYPE, file.metadata.mimetype().to_owned()),
            (header::CACHE_CONTROL, cache.to_owned()),
        ],
        file.data.to_vec(),
    )
        .into_response()
}

#[cfg(not(feature = "web-ui"))]
async fn serve(_path: &str) -> Response {
    const PAGE: &str = concat!(
        "<!doctype html><meta charset=utf-8><title>Agent Gateway</title>",
        "<style>body{font:15px/1.5 system-ui;background:#0d0f13;color:#e6e9ef;",
        "padding:2rem;max-width:36rem;margin:auto}code{background:#1e232b;",
        "padding:.15rem .4rem;border-radius:4px}</style>",
        "<h1>Web client not built</h1>",
        "<p>This gateway was compiled without the <code>web-ui</code> feature.</p>",
        "<pre><code>cd web &amp;&amp; pnpm install &amp;&amp; pnpm build\n",
        "cargo build --release -p gateway-cli --features web-ui</code></pre>",
        "<p>The API and the WebSocket protocol are unaffected.</p>"
    );
    (
        security_headers(),
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(NO_CACHE)),
        ],
        PAGE,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_entry_point_is_never_cached() {
        let response = index().await;
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            NO_CACHE
        );
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
    }

    #[tokio::test]
    async fn every_response_carries_the_security_headers() {
        let response = asset(Path("assets/index.js".to_owned())).await;
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        assert_eq!(
            response
                .headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .unwrap(),
            "nosniff"
        );
    }
}
