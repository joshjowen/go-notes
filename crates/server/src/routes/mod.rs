//! Router assembly.

pub mod auth;
pub mod files;
pub mod graph;
pub mod notes;
pub mod search;
pub mod theme;
pub mod tree;

use axum::routing::{delete, get, post, put};
use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::state::AppState;
use crate::web;

pub fn build(state: AppState) -> Router {
    let upload_limit = state.config.uploads.max_bytes;

    // Uploads get their own sub-router because the body limit for a file must be
    // far larger than the limit that should apply to a JSON request.
    let uploads = Router::new()
        .route("/attachments", post(files::upload))
        .layer(RequestBodyLimitLayer::new(upload_limit));

    let api = Router::new()
        // --- identity ---------------------------------------------------
        .route("/auth/info", get(auth::info))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/oidc/login", get(auth::oidc_login))
        .route("/auth/oidc/callback", get(auth::oidc_callback))
        .route("/me", get(auth::me))
        // --- theme ---------------------------------------------------------
        .route("/theme", get(theme::get_theme))
        .route("/theme", put(theme::set_theme))
        // --- tree and folders -------------------------------------------
        .route("/tree", get(tree::tree))
        .route("/folders", post(tree::create_folder))
        .route("/folders/move", post(tree::move_folder))
        .route("/folders/state", post(tree::set_folder_state))
        .route("/folders/{*path}", delete(tree::delete_folder))
        // --- notes -------------------------------------------------------
        // `/notes/move` is declared before the wildcard so it is matched as a
        // literal path rather than as a note named "move".
        .route("/notes", post(notes::create))
        .route("/notes/move", post(notes::move_note))
        .route("/notes/{*path}", get(notes::read))
        .route("/notes/{*path}", put(notes::save))
        .route("/notes/{*path}", delete(notes::delete))
        // --- search ------------------------------------------------------
        .route("/search", get(search::search))
        .route("/quickswitch", get(search::quickswitch))
        .route("/tags", get(search::tags))
        .route("/tagged", get(search::notes_with_tag))
        // --- links and graph ---------------------------------------------
        .route("/graph", get(graph::graph))
        .route("/backlinks/{*path}", get(notes::backlinks))
        // --- attachments --------------------------------------------------
        .route("/files/{*path}", get(files::serve))
        .merge(uploads)
        // A mistyped API path returns a JSON 404 rather than the HTML shell.
        .fallback(web::api_not_found)
        // JSON bodies are bounded separately from uploads. A note is text; 16 MB
        // is far more than anyone writes and far less than an unbounded read.
        .layer(RequestBodyLimitLayer::new(16 * 1024 * 1024));

    Router::new()
        .route("/healthz", get(web::healthz))
        .nest("/api", api)
        // Anything else is a frontend route, served by the embedded SPA.
        .fallback(web::serve_frontend)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            web::origin_guard,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            web::security_headers,
        ))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
