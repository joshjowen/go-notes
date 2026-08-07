//! The top toolbar.
//!
//! Everything here is reachable by keyboard too, and originally *only* by
//! keyboard — the graph was Ctrl+G, the theme lived in the command palette, and
//! neither had a visible control anywhere. That is fine for whoever wrote it and
//! no use at all to anyone else, which rather undercuts the point of an editor
//! meant for people who do not already know the shortcuts.
//!
//! So every button carries its shortcut in the tooltip: the toolbar is how you
//! find the feature, and the tooltip is how you learn the faster way to it.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;

use crate::api;
use crate::components::tree::create_note_in;
use crate::state::{use_app, LeftPanel, MainView, Palette, RightPanel, SyncPhase};

#[component]
pub fn TopBar() -> impl IntoView {
    let state = use_app();

    let in_graph = move || state.main_view.get() == MainView::Graph;

    view! {
        <header class="gn-topbar">
            <div class="gn-topbar-group">
                // The drawer toggle only exists on a narrow screen; on a wide
                // one the sidebar is always there and the stylesheet hides it.
                <button
                    class="gn-tool-button gn-narrow-only"
                    title="Files"
                    aria-label="Files"
                    on:click=move |_| state.drawer_open.update(|open| *open = !*open)
                >
                    <span class="gn-tool-icon">"☰"</span>
                </button>

                <span class="gn-brand">"Go-Notes"</span>

                <button
                    class="gn-tool-button"
                    title="New note (Alt+N)"
                    on:click=move |_| create_note_in(state, "")
                >
                    <span class="gn-tool-icon">"＋"</span>
                    <span class="gn-tool-label">"New"</span>
                </button>

                <button
                    class="gn-tool-button gn-wide-only"
                    title="Search all notes"
                    on:click=move |_| {
                        state.left_panel.set(LeftPanel::Search);
                        state.main_view.set(MainView::Editor);
                    }
                >
                    <span class="gn-tool-icon">"🔍"</span>
                    <span class="gn-tool-label">"Search"</span>
                </button>

                <button
                    class="gn-tool-button"
                    title="Jump to a note by name (Ctrl+P)"
                    on:click=move |_| state.palette.set(Some(Palette::QuickSwitch))
                >
                    <span class="gn-tool-icon">"⌘"</span>
                    <span class="gn-tool-label">"Go to"</span>
                </button>
            </div>

            // The view switch is a segmented control rather than a single
            // toggle, so which of the two you are looking at is visible without
            // having to read the button and infer the opposite.
            <div class="gn-view-switch" role="group" aria-label="View">
                <button
                    class="gn-view-option"
                    class:gn-active=move || !in_graph()
                    title="Edit notes (Ctrl+G switches)"
                    on:click=move |_| state.main_view.set(MainView::Editor)
                >
                    "Editor"
                </button>
                <button
                    class="gn-view-option"
                    class:gn-active=in_graph
                    title="See how your notes link together (Ctrl+G)"
                    on:click=move |_| state.main_view.set(MainView::Graph)
                >
                    "Graph"
                </button>
            </div>

            <div class="gn-topbar-group gn-topbar-right">
                // Shown only while the browser is actually holding an install
                // offer, so it is never a button that explains it cannot do
                // anything.
                <Show when=move || state.installable.get()>
                    <button
                        class="gn-tool-button gn-install-button"
                        title="Install Go-Notes as an app on this device"
                        on:click=move |_| crate::pwa::install(state)
                    >
                        <span class="gn-tool-icon">"⤓"</span>
                        <span class="gn-tool-label">"Install"</span>
                    </button>
                </Show>

                <SyncStatus />

                <button
                    class="gn-tool-button gn-wide-only"
                    title=move || {
                        if state.right_panel.get() == RightPanel::Hidden {
                            "Show the backlinks panel"
                        } else {
                            "Hide the backlinks panel"
                        }
                    }
                    on:click=move |_| {
                        state
                            .right_panel
                            .update(|panel| {
                                *panel = match *panel {
                                    RightPanel::Hidden => RightPanel::Backlinks,
                                    _ => RightPanel::Hidden,
                                }
                            })
                    }
                >
                    <span class="gn-tool-icon">"◧"</span>
                </button>

                <button
                    class="gn-tool-button gn-wide-only"
                    title="Theme"
                    on:click=move |_| state.theme_dialog_open.set(true)
                >
                    <span class="gn-tool-icon">"🎨"</span>
                    <span class="gn-tool-label">"Theme"</span>
                </button>

                <button
                    class="gn-tool-button"
                    title="All commands (Ctrl+Shift+P)"
                    on:click=move |_| state.palette.set(Some(Palette::Commands))
                >
                    <span class="gn-tool-icon">"⋯"</span>
                </button>

                <div class="gn-user-menu gn-wide-only">
                    <span class="gn-username" title=move || {
                        state
                            .me
                            .get()
                            .map(|me| {
                                match me.auth_provider.as_str() {
                                    "oidc" => format!("{} — signed in via your identity provider", me.username),
                                    _ => format!("{} — signed in with a password", me.username),
                                }
                            })
                            .unwrap_or_default()
                    }>
                        {move || state.me.get().map(|me| me.display_name).unwrap_or_default()}
                    </span>
                    <button class="gn-tool-button" title="Sign out" on:click=move |_| sign_out(state)>
                        <span class="gn-tool-icon">"⏻"</span>
                    </button>
                </div>
            </div>
        </header>
    }
}

/// The connection and sync indicator, and the detail behind it.
///
/// Always visible rather than only when something is wrong. "Is my writing
/// actually saved somewhere other than this laptop?" is a question worth being
/// able to answer at a glance, and an indicator that only appears in trouble
/// teaches people to distrust its absence.
#[component]
fn SyncStatus() -> impl IntoView {
    let state = use_app();
    let open = RwSignal::new(false);
    // Fixed rather than positioned relative to `.gn-sync`, so the panel is not
    // clipped by `.gn-topbar`'s `overflow: hidden` — that rule exists to stop a
    // crowded toolbar forcing a horizontal scrollbar, and it clips any
    // descendant that tries to paint outside the topbar's own box, dropdown or
    // not. `(top, right)` in viewport pixels, taken from the chip itself rather
    // than the click point so the panel lands in the same place however it was
    // opened (mouse, keyboard, touch).
    let panel_pos = RwSignal::new((0.0_f64, 0.0_f64));

    let label = move || crate::offline::net::summary(&state);
    let attention = move || state.sync.get() == SyncPhase::Blocked || state.local_only();

    view! {
        <div class="gn-sync">
            <button
                class="gn-sync-chip"
                class:gn-sync-offline=move || state.local_only()
                class:gn-sync-attention=attention
                class:gn-sync-busy=move || state.sync.get() == SyncPhase::Syncing
                title="Connection and sync status"
                on:click=move |ev| {
                    if let Some(target) = ev.current_target() {
                        if let Ok(el) = target.dyn_into::<web_sys::HtmlElement>() {
                            let rect = el.get_bounding_client_rect();
                            let viewport_width = web_sys::window()
                                .and_then(|w| w.inner_width().ok())
                                .and_then(|v| v.as_f64())
                                .unwrap_or(rect.right());
                            panel_pos.set((rect.bottom() + 8.0, viewport_width - rect.right()));
                        }
                    }
                    open.update(|shown| *shown = !*shown);
                }
            >
                <span class="gn-sync-dot"></span>
                <span class="gn-sync-label">{label}</span>
            </button>

            <Show when=move || open.get()>
                <div
                    class="gn-sync-panel"
                    style=move || {
                        let (top, right) = panel_pos.get();
                        format!("top: {top}px; right: {right}px;")
                    }
                    on:click=move |ev| ev.stop_propagation()
                >
                    <p class="gn-panel-title">
                        {move || {
                            if state.local_only() {
                                "Working offline"
                            } else {
                                "Connected to the server"
                            }
                        }}
                    </p>

                    {move || {
                        state
                            .sync_message
                            .get()
                            .map(|message| view! { <p class="gn-form-error">{message}</p> })
                    }}

                    <Show when=move || !state.offline_storage.get()>
                        <p class="gn-empty">
                            "This browser is not giving the app any storage, so nothing is being
                             kept for offline use. A private window, or a setting that blocks site
                             data, will do that."
                        </p>
                    </Show>

                    {move || {
                        let queued = state.pending.get();
                        if queued.is_empty() {
                            return view! {
                                <p class="gn-empty">"Everything here has reached the server."</p>
                            }
                                .into_any();
                        }
                        view! {
                            <div class="gn-sync-list">
                                {queued
                                    .into_iter()
                                    .map(|op| {
                                        let id = op.id;
                                        let failed = op.last_error.clone();
                                        view! {
                                            <div class="gn-sync-item">
                                                <span class="gn-sync-item-label">
                                                    {op.op.describe()}
                                                </span>
                                                {failed
                                                    .map(|reason| {
                                                        view! {
                                                            <span class="gn-sync-item-error">
                                                                {reason}
                                                            </span>
                                                            <button
                                                                class="gn-tool-button"
                                                                title="Discard this change"
                                                                on:click=move |_| discard(state, id)
                                                            >
                                                                "Discard"
                                                            </button>
                                                        }
                                                    })}
                                            </div>
                                        }
                                    })
                                    .collect_view()}
                            </div>
                        }
                            .into_any()
                    }}

                    <div class="gn-dialog-actions">
                        <button on:click=move |_| crate::vault::sync_now(state)>"Sync now"</button>
                        <button on:click=move |_| open.set(false)>"Close"</button>
                    </div>
                </div>
            </Show>
        </div>
    }
}

/// Throws away a queued change the server refused.
fn discard(state: crate::state::AppState, id: u64) {
    spawn_local(async move {
        state.pending.set(crate::offline::cache::drop_op(id).await);
        state.notify("Discarded that change.");
    });
}

fn sign_out(state: crate::state::AppState) {
    spawn_local(async move {
        // Wipe what this browser holds before ending the session, so signing out
        // on a shared machine really does leave nothing behind.
        crate::offline::cache::forget_everything().await;
        state.pending.set(Vec::new());

        match api::logout().await {
            Ok(response) => {
                // With an identity provider that supports it, sign out there too
                // — otherwise clicking "sign in" logs straight back in.
                match response.redirect_to {
                    Some(destination) => {
                        if let Some(window) = web_sys::window() {
                            let _ = window.location().set_href(&destination);
                        }
                    }
                    None => {
                        state.me.set(None);
                        state.tabs.set(Vec::new());
                        state.active.set(None);
                    }
                }
            }
            Err(err) => state.error(err.user_message()),
        }
    });
}
