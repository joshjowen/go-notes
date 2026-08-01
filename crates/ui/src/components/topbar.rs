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

use crate::api;
use crate::components::tree::create_note_in;
use crate::state::{use_app, LeftPanel, MainView, Palette, RightPanel};

#[component]
pub fn TopBar() -> impl IntoView {
    let state = use_app();

    let in_graph = move || state.main_view.get() == MainView::Graph;

    view! {
        <header class="gn-topbar">
            <div class="gn-topbar-group">
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
                    class="gn-tool-button"
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
                <button
                    class="gn-tool-button"
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
                    class="gn-tool-button"
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

                <div class="gn-user-menu">
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

fn sign_out(state: crate::state::AppState) {
    spawn_local(async move {
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
