//! Knowing whether the server is there.
//!
//! `navigator.onLine` is not the answer on its own. It reports whether the
//! machine has *a* network, not whether it can reach this application: a laptop
//! on a café's captive portal, a VPN that has dropped, or a server that is
//! simply down all read as "online". It is useful as a hint — the browser tells
//! us the instant a cable is plugged back in — but the decision belongs to a
//! request against our own origin.
//!
//! Which is also why there is no third-party reachability check here. A generic
//! "am I online" probe against somebody else's server would be both a privacy
//! leak and useless on the air-gapped networks this application is meant to run
//! on, where nothing outside the local network is reachable by design and
//! everything still works.

use std::cell::Cell;

use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::api::{self, ApiFailure};
use crate::state::{AppState, SyncPhase};

/// Backoff between reachability probes, in milliseconds. Short at first, so a
/// brief drop recovers almost immediately; then longer, so a laptop left in a
/// bag is not waking the radio every three seconds all afternoon.
const BACKOFF_MS: [u32; 5] = [3_000, 5_000, 10_000, 20_000, 30_000];

thread_local! {
    /// Only one probe loop at a time, however many things notice the outage.
    static PROBING: Cell<bool> = const { Cell::new(false) };
}

/// Installs the connectivity listeners.
pub fn watch(state: AppState) {
    let Some(window) = web_sys::window() else {
        return;
    };

    // The browser regaining a network is a reason to check immediately rather
    // than waiting out the current backoff.
    let on_online = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        spawn_local(async move {
            if reachable().await {
                went_online(state);
            }
        });
    });
    let _ = window.add_event_listener_with_callback("online", on_online.as_ref().unchecked_ref());
    on_online.forget();

    // Losing the network is conclusive in the other direction: no network means
    // no server, whatever the last request said.
    let on_offline = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        went_offline(state);
    });
    let _ = window.add_event_listener_with_callback("offline", on_offline.as_ref().unchecked_ref());
    on_offline.forget();
}

/// Records that a request could not reach the server.
///
/// This — not `navigator.onLine` — is what puts the app into local-only mode,
/// because it is the only signal that means the thing we actually need is
/// unavailable.
pub fn report_unreachable(state: AppState) {
    if state.online.get_untracked() {
        state.online.set(false);
        state.notify("Working offline. Changes are being saved on this device.");
    }
    start_probing(state);
}

/// Records that the server answered, whatever it answered with.
pub fn report_reachable(state: AppState) {
    if !state.online.get_untracked() {
        went_online(state);
    }
}

fn went_offline(state: AppState) {
    if state.online.get_untracked() {
        state.online.set(false);
        state.notify("Working offline. Changes are being saved on this device.");
    }
    start_probing(state);
}

fn went_online(state: AppState) {
    state.online.set(true);
    super::sync::start(state);
}

/// Polls until the server answers again.
fn start_probing(state: AppState) {
    if PROBING.with(|probing| probing.replace(true)) {
        return;
    }

    spawn_local(async move {
        let mut attempt = 0usize;
        loop {
            let wait = BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)];
            TimeoutFuture::new(wait).await;

            // Somebody else — a successful save, say — already noticed.
            if state.online.get_untracked() {
                break;
            }
            if reachable().await {
                went_online(state);
                break;
            }
            attempt += 1;
        }
        PROBING.with(|probing| probing.set(false));
    });
}

/// One cheap request against our own API.
///
/// A 401 counts as reachable: the server is there, the session has simply
/// expired, and that is a different problem with a different remedy — which
/// [`super::sync`] reports rather than silently dropping the queued work.
async fn reachable() -> bool {
    match api::me().await {
        Ok(_) => true,
        Err(ApiFailure::Offline(_)) => false,
        Err(_) => true,
    }
}

/// Whether the browser believes it has any network at all. Used only for the
/// first paint, before any request has been made.
pub fn browser_thinks_online() -> bool {
    web_sys::window()
        .map(|window| window.navigator().on_line())
        .unwrap_or(true)
}

/// A short description of the current state, for the status control.
pub fn summary(state: &AppState) -> String {
    let pending = state.pending.get().len();
    match (state.online.get(), state.sync.get(), pending) {
        (_, SyncPhase::Syncing, _) => "Syncing…".to_string(),
        (false, _, 0) => "Local only".to_string(),
        (false, _, 1) => "Local only — 1 change waiting".to_string(),
        (false, _, count) => format!("Local only — {count} changes waiting"),
        (true, SyncPhase::Blocked, _) => "Sync paused".to_string(),
        (true, _, 0) => "Synced".to_string(),
        (true, _, 1) => "1 change waiting".to_string(),
        (true, _, count) => format!("{count} changes waiting"),
    }
}
