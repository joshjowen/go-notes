//! The root component: the login gate, the three-pane shell, and the global
//! keyboard shortcuts.

use gloo_timers::callback::Timeout;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::api::{self, ApiFailure};
use crate::components::editor_pane::EditorPane;
use crate::components::graph::GraphView;
use crate::components::palette::CommandPalette;
use crate::components::panels::{
    extract_headings, BacklinksPane, OutlinePane, SearchPane, TabBar, TagPane,
};
use crate::components::topbar::TopBar;
use crate::components::tree::{create_note_in, FileTree};
use crate::state::{
    use_app, AppState, LeftPanel, MainView, Palette, RightPanel, Theme, ToastKind,
};

#[component]
pub fn App() -> impl IntoView {
    let state = AppState::new();
    provide_context(state);

    // Pick the theme before anything renders, so there is no flash of the wrong
    // one. An explicit choice wins; otherwise follow the operating system, which
    // is what someone running a light desktop expects to see on first visit.
    match local_storage_get("go-notes-theme").as_deref() {
        Some("light") => state.theme.set(Theme::Light),
        Some("dark") => state.theme.set(Theme::Dark),
        _ if prefers_light() => state.theme.set(Theme::Light),
        _ => {}
    }

    Effect::new(move |_| {
        let theme = state.theme.get();
        if let Some(root) = document_element() {
            let _ = root.set_attribute("data-theme", theme.as_str());
        }
        local_storage_set("go-notes-theme", theme.as_str());
    });

    // Find out who we are. A 401 here is the normal unauthenticated case, not an
    // error worth showing.
    let checked = RwSignal::new(false);
    Effect::new(move |_| {
        spawn_local(async move {
            match api::me().await {
                Ok(me) => state.me.set(Some(me)),
                Err(ApiFailure::Unauthenticated) => state.me.set(None),
                Err(err) => state.error(err.user_message()),
            }
            checked.set(true);
        });
    });

    view! {
        <Show
            when=move || checked.get()
            fallback=|| view! { <div class="gn-login"><p class="gn-empty">"Loading…"</p></div> }
        >
            <Show when=move || state.me.get().is_some() fallback=|| view! { <LoginScreen /> }>
                <Shell />
            </Show>
        </Show>
    }
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

#[component]
fn LoginScreen() -> impl IntoView {
    let state = use_app();
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let info = RwSignal::new(None::<go_notes_shared::AuthInfo>);

    Effect::new(move |_| {
        spawn_local(async move {
            if let Ok(found) = api::auth_info().await {
                info.set(Some(found));
            }
        });
    });

    let submit = move || {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);

        spawn_local(async move {
            match api::login(username.get_untracked(), password.get_untracked()).await {
                Ok(me) => {
                    password.set(String::new());
                    state.me.set(Some(me));
                }
                Err(ApiFailure::Unauthenticated) => {
                    // Deliberately does not say which of the two was wrong; the
                    // server does not tell us, precisely so it cannot be used to
                    // discover which usernames exist.
                    error.set(Some("That username and password did not match.".into()));
                }
                Err(err) => error.set(Some(err.user_message())),
            }
            busy.set(false);
        });
    };

    view! {
        <div class="gn-login">
            <div class="gn-login-card">
                <h1>"Go-Notes"</h1>
                <p class="gn-sub">"Your notes, as markdown files you own."</p>

                {move || {
                    error
                        .get()
                        .map(|message| view! { <p class="gn-form-error">{message}</p> })
                }}

                <Show when=move || info.get().is_none_or(|i| i.local_enabled)>
                    <form on:submit=move |ev| {
                        ev.prevent_default();
                        submit();
                    }>
                        <label>
                            <span>"Username"</span>
                            <input
                                class="gn-text-input"
                                type="text"
                                autocomplete="username"
                                autofocus
                                prop:value=move || username.get()
                                on:input=move |ev| username.set(event_target_value(&ev))
                            />
                        </label>
                        <label>
                            <span>"Password"</span>
                            <input
                                class="gn-text-input"
                                type="password"
                                autocomplete="current-password"
                                prop:value=move || password.get()
                                on:input=move |ev| password.set(event_target_value(&ev))
                            />
                        </label>
                        <button class="gn-primary-button" type="submit" disabled=move || busy.get()>
                            {move || if busy.get() { "Signing in…" } else { "Sign in" }}
                        </button>
                    </form>
                </Show>

                {move || {
                    let Some(info) = info.get() else { return ().into_any() };
                    let Some(label) = info.oidc_button else { return ().into_any() };
                    let divider = info
                        .local_enabled
                        .then(|| view! { <div class="gn-divider">"or"</div> });

                    view! {
                        {divider}
                        <a class="gn-secondary-button" href="/api/auth/oidc/login" style="display:block;text-align:center;text-decoration:none">
                            {label}
                        </a>
                    }
                        .into_any()
                }}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

#[component]
fn Shell() -> impl IntoView {
    let state = use_app();
    install_shortcuts(state);

    // Load the tree, and reload whenever something has changed it.
    Effect::new(move |_| {
        let _ = state.tree_epoch.get();
        spawn_local(async move {
            match api::tree().await {
                Ok(tree) => state.tree.set(Some(tree)),
                Err(ApiFailure::Unauthenticated) => state.me.set(None),
                Err(err) => state.error(err.user_message()),
            }
        });
    });

    // Refresh the backlinks pane when the active note changes.
    Effect::new(move |_| {
        let Some(path) = state.active_path() else {
            state.backlinks.set(Vec::new());
            return;
        };
        spawn_local(async move {
            if let Ok(links) = api::backlinks(path).await {
                state.backlinks.set(links);
            }
        });
    });

    // Derived from whatever the editor last reported, so the outline follows
    // along as headings are typed rather than only after a save.
    let headings = Memo::new(move |_| extract_headings(&state.active_markdown.get()));

    // Deliberately a `Memo` over a boolean rather than reading `state.tabs`
    // directly in the view below.
    //
    // Reading the vector there would tie the *existence* of `EditorPane` to
    // every mutation of it — including the dirty flag and the content hash,
    // which the editor itself writes on load. That was an infinite loop: mount
    // the editor, record its hash, the tab list changes, the view re-runs,
    // `EditorPane` is torn down and rebuilt, mount again. A memo only notifies
    // when the boolean actually flips, so the editor is built once.
    let has_tabs = Memo::new(move |_| !state.tabs.get().is_empty());

    view! {
        <div class="gn-app">
            <TopBar />

            <aside class="gn-sidebar">
                <div class="gn-pane-header">
                    <button
                        class="gn-tab-button"
                        class:gn-active=move || state.left_panel.get() == LeftPanel::Files
                        on:click=move |_| state.left_panel.set(LeftPanel::Files)
                    >
                        "Files"
                    </button>
                    <button
                        class="gn-tab-button"
                        class:gn-active=move || state.left_panel.get() == LeftPanel::Search
                        on:click=move |_| state.left_panel.set(LeftPanel::Search)
                    >
                        "Search"
                    </button>
                    <button
                        class="gn-tab-button"
                        class:gn-active=move || state.left_panel.get() == LeftPanel::Tags
                        on:click=move |_| state.left_panel.set(LeftPanel::Tags)
                    >
                        "Tags"
                    </button>
                </div>

                {move || match state.left_panel.get() {
                    LeftPanel::Files => view! { <FileTree /> }.into_any(),
                    LeftPanel::Search => view! { <SearchPane /> }.into_any(),
                    LeftPanel::Tags => view! { <TagPane /> }.into_any(),
                }}
            </aside>

            <main class="gn-main">
                <TabBar />

                {move || match state.main_view.get() {
                    MainView::Graph => view! { <GraphView /> }.into_any(),
                    MainView::Editor => {
                        if !has_tabs.get() {
                            view! {
                                <div class="gn-blank-state">
                                    <h2>"No note open"</h2>
                                    <div class="gn-blank-actions">
                                        <button
                                            class="gn-primary-button"
                                            on:click=move |_| create_note_in(state, "")
                                        >
                                            "New note"
                                        </button>
                                        <button
                                            class="gn-secondary-button"
                                            on:click=move |_| {
                                                state.palette.set(Some(Palette::QuickSwitch))
                                            }
                                        >
                                            "Open a note"
                                        </button>
                                    </div>
                                    <p class="gn-blank-hint">
                                        "Shortcuts: " <kbd>"Alt"</kbd> "+" <kbd>"N"</kbd>
                                        " for a new note, " <kbd>"Ctrl"</kbd> "+" <kbd>"P"</kbd>
                                        " to jump to one."
                                    </p>
                                    <p>
                                        "Everything you write is stored as an ordinary "
                                        <code>".md"</code>
                                        " file on the server, so it stays readable with or without this app."
                                    </p>
                                </div>
                            }
                                .into_any()
                        } else {
                            view! { <EditorPane /> }.into_any()
                        }
                    }
                }}
            </main>

            <Show when=move || state.right_panel.get() != RightPanel::Hidden>
                <aside class="gn-rightbar">
                    <div class="gn-pane-header">
                        <button
                            class="gn-tab-button"
                            class:gn-active=move || state.right_panel.get() == RightPanel::Backlinks
                            on:click=move |_| state.right_panel.set(RightPanel::Backlinks)
                        >
                            "Backlinks"
                        </button>
                        <button
                            class="gn-tab-button"
                            class:gn-active=move || state.right_panel.get() == RightPanel::Outline
                            on:click=move |_| state.right_panel.set(RightPanel::Outline)
                        >
                            "Outline"
                        </button>
                        <button
                            class="gn-icon-button"
                            title="Hide this panel"
                            on:click=move |_| state.right_panel.set(RightPanel::Hidden)
                        >
                            "✕"
                        </button>
                    </div>

                    {move || match state.right_panel.get() {
                        RightPanel::Outline => view! { <OutlinePane headings /> }.into_any(),
                        _ => view! { <BacklinksPane /> }.into_any(),
                    }}
                </aside>
            </Show>

            <CommandPalette />
            <ConflictDialog />
            <ToastHost />
        </div>
    }
}

// ---------------------------------------------------------------------------
// Conflict resolution
// ---------------------------------------------------------------------------

/// Offered when a save loses its `If-Match` check.
///
/// The three choices matter: the whole point of detecting the conflict is to let
/// a person decide, rather than picking a winner on their behalf and losing
/// somebody's writing either way.
#[component]
fn ConflictDialog() -> impl IntoView {
    let state = use_app();

    view! {
        <Show when=move || {
            state
                .conflict
                .get()
                .is_some_and(|conflict| {
                    conflict.theirs != crate::components::editor_pane::TAKE_THEIRS_MARKER
                })
        }>
            <div class="gn-overlay">
                <div class="gn-dialog">
                    <h2>"This note changed on disk"</h2>
                    <p>
                        "Someone — or something — edited this note after you opened it. That could be
                        another browser tab, an edit over SSH, or a "
                        <code>"git pull"</code>
                        ". Nothing has been overwritten yet."
                    </p>
                    <div class="gn-dialog-actions">
                        <button on:click=move |_| {
                            let Some(conflict) = state.conflict.get_untracked() else { return };
                            // Keep mine: save again against the hash the server
                            // just told us about, which will now match.
                            state.set_hash(&conflict.path, conflict.their_hash.clone());
                            state.conflict.set(None);
                            let path = conflict.path.clone();
                            let mine = conflict.mine.clone();
                            let hash = conflict.their_hash.clone();
                            spawn_local(async move {
                                match api::save_note(path.clone(), mine, hash).await {
                                    Ok(response) => {
                                        state.set_hash(&path, response.meta.content_hash.clone());
                                        state.mark_dirty(&path, false);
                                        state.notify("Kept your version.");
                                        state.refresh_all();
                                    }
                                    Err(err) => state.error(err.user_message()),
                                }
                            });
                        }>"Keep my version"</button>

                        <button on:click=move |_| {
                            let Some(conflict) = state.conflict.get_untracked() else { return };
                            // The editor pane watches for this marker and reloads.
                            state
                                .conflict
                                .set(Some(crate::state::Conflict {
                                    theirs: crate::components::editor_pane::TAKE_THEIRS_MARKER
                                        .to_string(),
                                    ..conflict
                                }));
                        }>"Load the version on disk"</button>

                        <button on:click=move |_| {
                            let Some(conflict) = state.conflict.get_untracked() else { return };
                            // Neither side wins: park a copy alongside so the
                            // user can merge them at their leisure.
                            state.conflict.set(None);
                            let stem = go_notes_shared::paths::stem(&conflict.path);
                            let parent = go_notes_shared::paths::parent_of(&conflict.path);
                            let stamp = js_sys::Date::new_0().to_iso_string();
                            let stamp: String = stamp
                                .as_string()
                                .unwrap_or_default()
                                .chars()
                                .take(19)
                                .map(|c| if c == ':' { '-' } else { c })
                                .collect();
                            let copy = go_notes_shared::paths::join(
                                parent,
                                &format!("{stem} (conflicted copy {stamp}).md"),
                            );
                            spawn_local(async move {
                                match api::create_note(copy.clone(), conflict.mine).await {
                                    Ok(response) => {
                                        state.refresh_all();
                                        state
                                            .open_tab(
                                                response.meta.path.clone(),
                                                response.meta.title.clone(),
                                            );
                                        state.notify("Saved your version as a separate note.");
                                    }
                                    Err(err) => state.error(err.user_message()),
                                }
                            });
                        }>"Save mine alongside"</button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

// ---------------------------------------------------------------------------
// Toasts
// ---------------------------------------------------------------------------

#[component]
fn ToastHost() -> impl IntoView {
    let state = use_app();

    // Clear after a delay. Keyed on the sequence number so an identical message
    // shown twice still restarts the timer rather than vanishing early.
    Effect::new(move |previous: Option<u32>| {
        let Some(toast) = state.toast.get() else {
            return previous.unwrap_or(0);
        };
        if previous == Some(toast.seq) {
            return toast.seq;
        }
        let seq = toast.seq;
        Timeout::new(4200, move || {
            if state.toast.get_untracked().is_some_and(|t| t.seq == seq) {
                state.toast.set(None);
            }
        })
        .forget();
        seq
    });

    view! {
        {move || {
            state
                .toast
                .get()
                .map(|toast| {
                    view! {
                        <div
                            class="gn-toast"
                            class:gn-error=matches!(toast.kind, ToastKind::Error)
                            on:click=move |_| state.toast.set(None)
                        >
                            {toast.message.clone()}
                        </div>
                    }
                })
        }}
    }
}

// ---------------------------------------------------------------------------
// Shortcuts
// ---------------------------------------------------------------------------

/// Installs the window-level keyboard shortcuts.
///
/// Registered once on the window rather than per-component, because they have to
/// work regardless of which pane has focus.
fn install_shortcuts(state: AppState) {
    let Some(window) = web_sys::window() else {
        return;
    };

    let handler = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(
        move |ev: web_sys::KeyboardEvent| {
            let modifier = ev.ctrl_key() || ev.meta_key();

            // Escape closes whatever is on top, and needs no modifier.
            if ev.key() == "Escape" {
                if state.palette.get_untracked().is_some() {
                    state.palette.set(None);
                }
                return;
            }

            // New note is Alt+N, not Ctrl+N.
            //
            // The browser reserves Ctrl+N for a new window and a page cannot
            // take it back — `prevent_default` is ignored outright for that one,
            // along with Ctrl+T, Ctrl+W and Ctrl+Shift+N. Binding it would mean
            // advertising a shortcut that opens a Chrome window and creates
            // nothing, which is exactly the bug this replaced. Alt+N is not
            // spoken for, so it actually arrives here.
            if ev.alt_key() && !modifier && ev.key().eq_ignore_ascii_case("n") {
                ev.prevent_default();
                create_note_in(state, "");
                return;
            }

            if !modifier {
                return;
            }

            match ev.key().to_lowercase().as_str() {
                "p" if ev.shift_key() => {
                    ev.prevent_default();
                    state.palette.set(Some(Palette::Commands));
                }
                "p" => {
                    ev.prevent_default();
                    state.palette.set(Some(Palette::QuickSwitch));
                }
                "s" => {
                    // Autosave already handles this, but Ctrl+S is muscle memory
                    // and a browser "save page" dialog would be a nasty surprise.
                    ev.prevent_default();
                    state.request_save();
                }
                "g" => {
                    ev.prevent_default();
                    state.main_view.update(|view| {
                        *view = match *view {
                            MainView::Graph => MainView::Editor,
                            MainView::Editor => MainView::Graph,
                        }
                    });
                }
                "e" => {
                    ev.prevent_default();
                    state.editor_mode.update(|mode| *mode = mode.toggled());
                }
                _ => {}
            }
        },
    );

    let _ = window
        .add_event_listener_with_callback("keydown", handler.as_ref().unchecked_ref());
    // Deliberately leaked: the listener lives for the lifetime of the page, and
    // dropping the closure would leave JavaScript calling into freed memory.
    handler.forget();
}

// ---------------------------------------------------------------------------
// Small DOM helpers
// ---------------------------------------------------------------------------

/// True when the operating system asks for a light colour scheme.
fn prefers_light() -> bool {
    web_sys::window()
        .and_then(|window| window.match_media("(prefers-color-scheme: light)").ok().flatten())
        .is_some_and(|query| query.matches())
}

fn document_element() -> Option<web_sys::Element> {
    web_sys::window()?.document()?.document_element()
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn local_storage_get(key: &str) -> Option<String> {
    local_storage()?.get_item(key).ok().flatten()
}

fn local_storage_set(key: &str, value: &str) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(key, value);
    }
}
