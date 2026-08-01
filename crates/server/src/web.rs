//! HTTP concerns that are not specific to any one route: security headers, the
//! cross-origin guard, client-address extraction, and serving the frontend.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

use crate::error::AppError;
use crate::state::AppState;

/// The Leptos frontend, compiled to WebAssembly by Trunk and baked into the
/// binary so the container ships as a single file with no web server in front
/// of it.
///
/// The directory must exist at compile time; the repository keeps a placeholder
/// `index.html` there so a plain `cargo build` works without running Trunk first.
#[derive(Embed)]
#[folder = "../ui/dist/"]
struct Assets;

/// The Content-Security-Policy, built once from the embedded frontend.
///
/// Trunk emits its WebAssembly loader as an *inline* `<script type="module">`,
/// which `script-src 'self'` blocks outright — the page loads and then nothing
/// happens at all. The three ways out are `'unsafe-inline'` (which would also
/// permit any script an attacker managed to inject, defeating the point), a
/// per-request nonce (which means rewriting the HTML on every request), or a
/// hash of the script's exact bytes.
///
/// The hash is the right answer here: the loader is fixed at build time, so the
/// digest is computed once at startup and the policy stays as strict as it can
/// be. Any *other* inline script — including one smuggled into a note — still
/// fails the check, because its hash will not match.
static CSP: std::sync::OnceLock<HeaderValue> = std::sync::OnceLock::new();

fn content_security_policy() -> HeaderValue {
    CSP.get_or_init(|| {
        let hashes = inline_script_hashes()
            .into_iter()
            .map(|hash| format!(" '{hash}'"))
            .collect::<String>();

        // `wasm-unsafe-eval` is required: instantiating a WebAssembly module
        // counts as evaluation under CSP, so without it the frontend cannot
        // start. It is far narrower than `unsafe-eval` — it permits WebAssembly
        // compilation and nothing else, and does not re-enable `eval()`.
        //
        // `style-src` allows inline styles because ProseMirror sets them on
        // elements directly (table column widths, for instance).
        //
        // `img-src` allows `data:` and `blob:` so a pasted image can be shown
        // before its upload has finished.
        let policy = format!(
            "default-src 'self'; \
             script-src 'self' 'wasm-unsafe-eval'{hashes}; \
             style-src 'self' 'unsafe-inline'; \
             img-src 'self' data: blob:; \
             font-src 'self' data:; \
             connect-src 'self'; \
             media-src 'self' blob:; \
             object-src 'none'; \
             base-uri 'none'; \
             form-action 'self'; \
             frame-ancestors 'none'"
        );

        HeaderValue::from_str(&policy)
            .unwrap_or_else(|_| HeaderValue::from_static("default-src 'self'"))
    })
    .clone()
}

/// SHA-256 digests of every inline `<script>` in the embedded index.html,
/// formatted the way CSP expects them.
fn inline_script_hashes() -> Vec<String> {
    use base64::Engine as _;
    use sha2::Digest as _;

    let Some(index) = Assets::get("index.html") else {
        return Vec::new();
    };
    let Ok(html) = std::str::from_utf8(&index.data) else {
        return Vec::new();
    };

    let mut hashes = Vec::new();
    let mut rest = html;

    while let Some(open) = rest.find("<script") {
        let after_tag = &rest[open..];
        let Some(tag_end) = after_tag.find('>') else {
            break;
        };
        let attributes = &after_tag[..tag_end];
        let body_start = open + tag_end + 1;

        let Some(close) = rest[body_start..].find("</script>") else {
            break;
        };
        let body = &rest[body_start..body_start + close];

        // A tag with a `src` loads an external file, which `'self'` already
        // covers; only genuinely inline bodies need a hash.
        if !attributes.contains("src=") && !body.trim().is_empty() {
            let digest = sha2::Sha256::digest(body.as_bytes());
            hashes.push(format!(
                "sha256-{}",
                base64::engine::general_purpose::STANDARD.encode(digest)
            ));
        }

        rest = &rest[body_start + close + "</script>".len()..];
    }

    hashes
}

/// Adds the response headers that constrain what a page can do.
pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    headers.insert(header::CONTENT_SECURITY_POLICY, content_security_policy());
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // `same-origin` rather than `no-referrer` so in-app navigation still works
    // normally, while note paths never leak to an external site.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    // Redundant with `frame-ancestors` above, but still honoured by older
    // browsers that predate CSP level 2.
    headers.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("DENY"),
    );

    response
}

/// Rejects state-changing requests that came from another origin.
///
/// This is defence in depth rather than the primary CSRF control. The session
/// cookie is `SameSite=Lax`, which already means a browser will not attach it to
/// a cross-site `POST` — so a forged request arrives unauthenticated and fails
/// anyway. This check catches the residual cases: a browser that does not
/// enforce `SameSite`, or a future change that loosens the cookie.
///
/// A request with *no* `Origin` at all is allowed through. Browsers have sent
/// `Origin` on every mutating request for years, so an absent header means a
/// non-browser client — `curl`, a script, a sync tool — which is not subject to
/// CSRF in the first place, and rejecting it would make the API unusable outside
/// a browser for no security gain.
pub async fn origin_guard(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let is_mutating = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );

    if is_mutating {
        if let Some(origin) = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
        {
            let expected = state.config.server.public_url.trim_end_matches('/');
            if origin != expected {
                tracing::warn!(
                    origin,
                    expected,
                    path = %request.uri().path(),
                    "rejecting cross-origin request"
                );
                return Err(AppError::forbidden(
                    "this request did not come from an allowed origin",
                ));
            }
        }
    }

    Ok(next.run(request).await)
}

/// Best-effort client address, used only for rate-limiting login attempts.
///
/// Behind a reverse proxy the socket address is the proxy, so every user would
/// share one throttle bucket and one attacker could lock out everybody. Reading
/// `X-Forwarded-For` fixes that, at the cost of being spoofable by a client that
/// can reach the app directly — which is why it is gated on configuration, and
/// why nothing but throttling is allowed to depend on this value. Authorisation
/// never does.
pub fn client_address(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> String {
    if state.config.server.trust_proxy_headers {
        if let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
        {
            // Leftmost entry is the original client; the rest are proxies.
            if let Some(first) = forwarded.split(',').next() {
                let trimmed = first.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }

    peer.map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn user_agent(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Whether an asset's name contains a content hash, and may therefore be cached
/// forever.
///
/// Trunk fingerprints what it generates as `name-<16 hex digits>.ext`. It does
/// *not* fingerprint files copied through verbatim, and `editor-bridge.js` and
/// its stylesheet are copied — `index.html` links them under those fixed names.
/// Testing merely for a hyphen therefore caught them too, pinning the editor
/// bundle in every returning browser for a year: an editor fix shipped in a new
/// release would never be seen without a manual cache clear.
fn is_fingerprinted(path: &str) -> bool {
    let Some((stem, _extension)) = path.rsplit_once('.') else {
        return false;
    };
    let Some((_name, hash)) = stem.rsplit_once('-') else {
        return false;
    };
    hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Serves the embedded frontend, falling back to `index.html` for client-side
/// routes so that reloading on `/note/Projects/A.md` works.
pub async fn serve_frontend(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // A real file: cache it forever only if its name carries a content hash, so
    // that shipping a new build actually replaces what the browser holds.
    if !path.is_empty() {
        if let Some(asset) = Assets::get(path) {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let cacheable = is_fingerprinted(path) && !path.ends_with(".html");
            return (
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (
                        header::CACHE_CONTROL,
                        if cacheable {
                            "public, max-age=31536000, immutable".to_string()
                        } else {
                            "no-cache".to_string()
                        },
                    ),
                ],
                asset.data.into_owned(),
            )
                .into_response();
        }
    }

    // Anything else is a frontend route. Note this handler is only mounted as a
    // fallback *after* the `/api` routes, so an unknown API path still 404s as
    // JSON rather than being answered with a page.
    match Assets::get("index.html") {
        Some(index) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            index.data.into_owned(),
        )
            .into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "The frontend was not built into this binary. Run `trunk build` in crates/ui.",
        )
            .into_response(),
    }
}

/// 404 for unmatched `/api` paths, so a mistyped endpoint returns JSON rather
/// than the HTML shell.
pub async fn api_not_found() -> Response {
    AppError::NotFound.into_response()
}

/// Liveness and readiness in one: the process is up and the database answers.
pub async fn healthz(State(state): State<AppState>) -> Response {
    match sqlx::query("SELECT 1").fetch_one(&state.pool).await {
        Ok(_) => (StatusCode::OK, "ok").into_response(),
        Err(err) => {
            tracing::warn!(error = %err, "health check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response()
        }
    }
}

/// Body used when an upload exceeds the configured limit, so the client gets the
/// standard JSON error shape rather than tower's bare status code.
pub fn payload_too_large(limit: usize) -> Response {
    let mb = limit / (1024 * 1024);
    AppError::TooLarge(format!("that file is larger than the {mb} MB upload limit"))
        .into_response()
}

/// Empty body helper for handlers that only signal success.
pub fn no_content() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("building an empty response cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_only_hashed_asset_names_forever() {
        // What Trunk fingerprints.
        assert!(is_fingerprinted("go-notes-ui-ccfb7b424d5a1f48.js"));
        assert!(is_fingerprinted("styles-1d8abe2cdabc1247.css"));

        // What it copies through under a fixed name. These contain a hyphen, so
        // they were previously cached as immutable and a new editor build could
        // not reach a browser that had already loaded one.
        assert!(!is_fingerprinted("editor-bridge.js"));
        assert!(!is_fingerprinted("editor-bridge.css"));

        assert!(!is_fingerprinted("index.html"));
        assert!(!is_fingerprinted("favicon.ico"));
        // A hyphenated tail that is the right length but not hex.
        assert!(!is_fingerprinted("some-notquiteahashzz.js"));
    }
}
