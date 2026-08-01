//! Offline mode: everything that lets the app keep working with no server.
//!
//! Three layers, deliberately separate:
//!
//! * **The app shell** is cached by a service worker (`crates/ui/sw.js`), so
//!   reloading the page with the network down still starts the application
//!   rather than showing the browser's dinosaur.
//! * **The vault** — the notes themselves, the file tree, and who is signed in —
//!   is cached in IndexedDB by [`cache`], and every change made while offline is
//!   recorded in an outbox ([`queue`]).
//! * **Reconnection** is [`net`] noticing the server is answering again and
//!   [`sync`] replaying the outbox, stopping at anything that turns out to
//!   conflict so a person can decide what happens to their writing.
//!
//! Nothing here reaches outside the origin the app was served from. That is a
//! requirement rather than an accident: Go-Notes has to run on an air-gapped
//! network, so there is no font, no CDN, no analytics endpoint and no
//! connectivity check against somebody else's server anywhere in this module.
//! Reachability is decided by asking *our* server, and nothing else.

pub mod cache;
pub mod diff;
pub mod idb;
pub mod index;
pub mod net;
pub mod queue;
pub mod sync;
pub mod tree;

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// A note as this device holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedNote {
    pub path: String,
    pub title: String,
    pub markdown: String,
    /// The hash the server last confirmed for this text. Empty for a note that
    /// has only ever existed here, which is exactly what the server expects
    /// when it is eventually created.
    pub content_hash: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl CachedNote {
    pub fn new(path: String, markdown: String, content_hash: String) -> CachedNote {
        CachedNote {
            title: go_notes_shared::paths::stem(&path).to_string(),
            path,
            markdown,
            content_hash,
            updated_at: chrono::Utc::now(),
        }
    }
}

/// Starts offline support: opens the local store, restores the outbox, installs
/// the connectivity watcher, and registers the service worker.
pub async fn init(state: AppState) {
    state.offline_storage.set(cache::is_available().await);
    state.pending.set(cache::outbox().await);
    net::watch(state);
    register_service_worker();
}

/// Registers the service worker that caches the application shell.
///
/// Browsers only allow service workers on a secure context — HTTPS, or
/// `localhost` for development. On a plain-HTTP deployment registration fails,
/// which is not fatal: notes, edits and the outbox still work offline for as
/// long as the tab lives, and only *reloading* while disconnected is lost. The
/// failure is logged rather than shown, because it is a property of how the
/// server is deployed and not something the person writing a note can act on.
fn register_service_worker() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let container = window.navigator().service_worker();
    let _ = container.register("/sw.js");
}
