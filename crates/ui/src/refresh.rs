//! Noticing the vault changed on another device.
//!
//! The tree and the graph only refetch when *this* device writes something —
//! see `state::tree_epoch` and `state::graph_epoch` — because that is the one
//! event every write already knows about. Nothing made them refetch when
//! another device is the one doing the writing, and an installed PWA is
//! *resumed* rather than reloaded, sometimes for days, so a note created on a
//! phone could stay invisible in the sidebar indefinitely.
//!
//! This module is the fix: refetch when the tab regains focus or visibility —
//! the nearest thing a resumed PWA gets to a reload — and on a slow timer
//! while it stays visible, as a backstop for a tab left open and forgotten.

use std::cell::Cell;

use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;

use crate::offline::sync;
use crate::state::AppState;

/// How often the backstop timer fires while the tab stays visible.
const TICK_MS: u32 = 60_000;

/// The shortest gap between two refreshes, so a burst of focus/visibility
/// events — alt-tabbing back and forth — is not a burst of requests.
const MIN_GAP_MS: f64 = 5_000.0;

thread_local! {
    /// `js_sys::Date::now()` of the last refresh. `std::time::Instant` panics
    /// on `wasm32-unknown-unknown`, which has no clock of its own.
    static LAST_REFRESH: Cell<f64> = const { Cell::new(0.0) };
}

/// Installs the resume/focus/timer listeners. Called once, from the shell.
pub fn watch(state: AppState) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };

    let on_visible = Closure::<dyn FnMut()>::new(move || {
        if !document_hidden() {
            refresh_now(state);
        }
    });
    let _ = document.add_event_listener_with_callback(
        "visibilitychange",
        on_visible.as_ref().unchecked_ref(),
    );
    on_visible.forget();

    let on_focus = Closure::<dyn FnMut()>::new(move || refresh_now(state));
    let _ = window.add_event_listener_with_callback("focus", on_focus.as_ref().unchecked_ref());
    on_focus.forget();

    spawn_local(async move {
        loop {
            TimeoutFuture::new(TICK_MS).await;
            if !document_hidden() {
                refresh_now(state);
            }
        }
    });
}

/// Refetches the tree and graph, and drains the outbox, unless there is a
/// reason not to bother.
fn refresh_now(state: AppState) {
    if !state.online.get_untracked() {
        return;
    }
    // A conflict dialog owns the note it is about; reloading behind it would
    // change what "theirs" refers to mid-decision.
    if !state.conflicts.get_untracked().is_empty() {
        return;
    }

    let now = js_sys::Date::now();
    let due = LAST_REFRESH.with(|last| now - last.get() >= MIN_GAP_MS);
    if !due {
        return;
    }
    LAST_REFRESH.with(|last| last.set(now));

    state.refresh_all();
    sync::start(state);

    // Only when nothing typed since the last save would be lost: the reload
    // effect already no-ops when the text it fetches matches what is open.
    let dirty = state
        .active_path()
        .is_some_and(|path| state.tabs.get_untracked().iter().any(|tab| tab.path == path && tab.dirty));
    if !dirty && state.active_path().is_some() {
        state.request_reload();
    }
}

fn document_hidden() -> bool {
    web_sys::window()
        .and_then(|window| window.document())
        .is_some_and(|document| document.hidden())
}
