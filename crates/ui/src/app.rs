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
use crate::components::theme_editor::ThemeEditor;
use crate::components::topbar::TopBar;
use crate::components::tree::{create_note_in, FileTree};
use crate::offline::diff::{change_counts, diff_lines, DiffKind};
use crate::offline::{self, sync};
use crate::pwa;
use crate::state::{use_app, AppState, LeftPanel, MainView, Palette, RightPanel, ToastKind};
use crate::theme;
use crate::vault;

#[component]
pub fn App() -> impl IntoView {
    let state = AppState::new();
    provide_context(state);

    // Pick the theme before anything renders, so there is no flash of the wrong
    // one. An explicit choice wins; otherwise follow the operating system, which
    // is what someone running a light desktop expects to see on first visit.
    let saved = theme::load_saved();
    state.theme_id.set(saved.theme_id);
    state.custom_colors.set(saved.custom_colors);
    state.custom_css.set(saved.custom_css);

    Effect::new(move |_| {
        let colors = theme::active_colors(&state);
        theme::apply(&colors);

        let id = state.theme_id.get();
        theme::save_theme_id(id);
        if id == theme::ThemeId::Custom {
            theme::save_custom_colors(&colors);
        }
    });

    Effect::new(move |_| {
        let css = state.custom_css.get();
        theme::apply_custom_css(&css);
        theme::save_custom_css(&css);
    });

    // Find out who we are. A 401 here is the normal unauthenticated case, not an
    // error worth showing.
    //
    // The server being unreachable is a third case, and the one that decides
    // whether offline mode is any use at all: if this device has been signed in
    // before, we know who the user is and what their vault looked like, so the
    // application opens on their notes instead of on a login screen it cannot
    // service anyway.
    let checked = RwSignal::new(false);
    Effect::new(move |_| {
        spawn_local(async move {
            offline::init(state).await;
            pwa::watch(state);

            match api::me().await {
                Ok(me) => {
                    state.online.set(true);
                    offline::cache::remember_identity(&me).await;
                    state.me.set(Some(me));
                    // Anything queued from a previous session goes now.
                    sync::start(state);
                }
                Err(ApiFailure::Unauthenticated) => {
                    state.online.set(true);
                    state.me.set(None);
                }
                Err(err) if err.is_offline() => {
                    offline::net::report_unreachable(state);
                    state.me.set(offline::cache::identity().await);
                }
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
                    state.online.set(true);
                    // Wipes anything cached for a different account before
                    // recording this one, so a shared machine never shows one
                    // person the notes another left behind.
                    offline::cache::remember_identity(&me).await;
                    state.me.set(Some(me));
                    // A session that expired while offline leaves work queued;
                    // signing back in is exactly when it should go.
                    sync::start(state);
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

                // Signing in needs the server; there is no local password to
                // check against. Saying so is better than a failed login that
                // looks like a wrong password.
                <Show when=move || state.local_only()>
                    <p class="gn-form-error">
                        "The server cannot be reached from here. Signing in needs it, so this
                         will work as soon as the connection is back. Notes cached on a device
                         that is already signed in stay available in the meantime."
                    </p>
                </Show>

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

    // The manifest advertises a "New note" shortcut, which a phone's launcher
    // offers on a long press. It arrives as a query string on a cold start,
    // which is the only signal a shortcut ever gets.
    Effect::new(move |ran: Option<bool>| {
        if ran.is_some() {
            return true;
        }
        if launched_for_a_new_note() {
            create_note_in(state, "");
        }
        true
    });

    // Load the tree, and reload whenever something has changed it. Falls back to
    // the copy this device holds when the server cannot be reached.
    Effect::new(move |_| {
        let _ = state.tree_epoch.get();
        spawn_local(async move {
            match vault::tree(state).await {
                Ok(tree) => state.tree.set(Some(tree)),
                Err(ApiFailure::Unauthenticated) => state.me.set(None),
                Err(err) if err.is_offline() => state.error(
                    "No local copy of the file list yet, and the server cannot be reached.",
                ),
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
            state.backlinks.set(vault::backlinks(state, path).await);
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
        <div
            class="gn-app"
            class:gn-drawer-open=move || state.drawer_open.get()
            class:gn-right-open=move || state.right_panel.get() != RightPanel::Hidden
        >
            <TopBar />
            <OfflineBanner />

            // Only ever visible on a narrow screen, where the sidebar is a
            // drawer over the note rather than a column beside it. Tapping
            // anywhere off the drawer is how everyone expects to close one.
            <div
                class="gn-scrim"
                on:click=move |_| {
                    state.drawer_open.set(false);
                    // On a narrow screen the backlinks panel is over the note
                    // too, and the scrim is the only thing in front of both.
                    if crate::state::is_narrow() {
                        state.right_panel.set(RightPanel::Hidden);
                    }
                }
            ></div>

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

                    // The drawer covers the toolbar it was opened from, so it
                    // carries its own way out rather than relying on the user
                    // guessing that the dimmed note behind it is tappable.
                    <button
                        class="gn-icon-button gn-narrow-only"
                        title="Close"
                        aria-label="Close"
                        on:click=move |_| state.drawer_open.set(false)
                    >
                        "✕"
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
            <ThemeEditor />
            <ConflictDialog />
            <ToastHost />
        </div>
    }
}

// ---------------------------------------------------------------------------
// Conflict resolution
// ---------------------------------------------------------------------------

/// Offered when two versions of the same note disagree — because a save lost
/// its `If-Match` check, or because a change made offline could not be replayed
/// as it stood.
///
/// The three choices matter: the whole point of detecting the conflict is to let
/// a person decide, rather than picking a winner on their behalf and losing
/// somebody's writing either way. The diff is what makes that a decision rather
/// than a guess — without it, "keep mine" and "use theirs" are two unlabelled
/// buttons over an unknown amount of somebody's work.
#[component]
fn ConflictDialog() -> impl IntoView {
    let state = use_app();
    let current = Memo::new(move |_| state.conflicts.get().first().cloned());

    view! {
        {move || {
            let Some(conflict) = current.get() else { return ().into_any() };
            let waiting = state.conflicts.get().len();
            let offline_origin = matches!(conflict.origin, crate::state::ConflictOrigin::Sync { .. });

            let diff = diff_lines(&conflict.mine, &conflict.theirs);
            let (mine_lines, their_lines) = change_counts(&diff);
            let truncated = diff.len().saturating_sub(DIFF_ROWS);

            let for_mine = conflict.clone();
            let for_theirs = conflict.clone();
            let for_both = conflict.clone();

            view! {
                <div class="gn-overlay">
                    <div class="gn-dialog gn-conflict-dialog">
                        <h2>
                            {if offline_origin {
                                "This note also changed on the server"
                            } else {
                                "This note changed on disk"
                            }}
                        </h2>
                        <p class="gn-conflict-path">{conflict.path.clone()}</p>
                        <p>
                            {if offline_origin {
                                "You edited this note while offline, and it was edited on the server \
                                 too. Nothing has been overwritten — the rest of your queued changes \
                                 are waiting behind this decision."
                            } else {
                                "Someone — or something — edited this note after you opened it: \
                                 another browser tab, an edit over SSH, or a git pull. Nothing has \
                                 been overwritten yet."
                            }}
                        </p>

                        <p class="gn-conflict-summary">
                            {format!(
                                "{mine_lines} line{} only in your version, {their_lines} line{} only on the server.",
                                if mine_lines == 1 { "" } else { "s" },
                                if their_lines == 1 { "" } else { "s" },
                            )}
                        </p>

                        <div class="gn-diff">
                            {diff
                                .into_iter()
                                .take(DIFF_ROWS)
                                .map(|line| {
                                    let (class, marker) = match line.kind {
                                        DiffKind::Same => ("gn-diff-same", " "),
                                        DiffKind::Mine => ("gn-diff-mine", "−"),
                                        DiffKind::Theirs => ("gn-diff-theirs", "+"),
                                    };
                                    view! {
                                        <div class=class>
                                            <span class="gn-diff-marker">{marker}</span>
                                            <span class="gn-diff-text">{line.text}</span>
                                        </div>
                                    }
                                })
                                .collect_view()}
                            {(truncated > 0)
                                .then(|| {
                                    view! {
                                        <div class="gn-diff-more">
                                            {format!("… {truncated} more lines")}
                                        </div>
                                    }
                                })}
                        </div>
                        <p class="gn-diff-legend">
                            <span class="gn-diff-mine">"− yours"</span>
                            <span class="gn-diff-theirs">"+ on the server"</span>
                        </p>

                        <div class="gn-dialog-actions">
                            <button on:click=move |_| sync::keep_mine(state, for_mine.clone())>
                                "Keep my version"
                            </button>
                            <button on:click=move |_| sync::take_theirs(state, for_theirs.clone())>
                                "Use the server's version"
                            </button>
                            <button on:click=move |_| sync::keep_both(state, for_both.clone())>
                                "Keep both"
                            </button>
                        </div>

                        {(waiting > 1)
                            .then(|| {
                                view! {
                                    <p class="gn-dialog-note">
                                        {format!("{} more to review after this one.", waiting - 1)}
                                    </p>
                                }
                            })}
                    </div>
                </div>
            }
                .into_any()
        }}
    }
}

/// How much of a diff to render before summarising the rest. Enough to see a
/// normal edit whole; short of the point where the dialog becomes a document
/// viewer nobody scrolls.
const DIFF_ROWS: usize = 200;

/// True when the app was opened through the manifest's "New note" shortcut.
fn launched_for_a_new_note() -> bool {
    web_sys::window()
        .and_then(|window| window.location().search().ok())
        .is_some_and(|search| search.contains("new=1"))
}

// ---------------------------------------------------------------------------
// Offline
// ---------------------------------------------------------------------------

/// The bar across the top when the server cannot be reached.
///
/// A toast would not do: toasts are for things that just happened, and being
/// offline is a state that lasts. Someone who leaves a tab open on a train needs
/// to be able to glance at the window and know that what they are typing is
/// going into this device and nowhere else yet.
#[component]
fn OfflineBanner() -> impl IntoView {
    let state = use_app();

    view! {
        <Show when=move || state.local_only() || state.sync_message.get().is_some()>
            <div class="gn-offline-banner" role="status">
                <span class="gn-offline-dot"></span>
                {move || match state.sync_message.get() {
                    Some(message) => view! { <span>{message}</span> }.into_any(),
                    None => {
                        let waiting = state.pending.get().len();
                        let where_it_goes = if state.offline_storage.get() {
                            " Saving to this device; it syncs when the server is back."
                        } else {
                            " This browser is giving the app no storage, so what you write is only \
                             held while this tab stays open."
                        };
                        let queued = match waiting {
                            0 => String::new(),
                            1 => " 1 change is waiting to sync.".to_string(),
                            count => format!(" {count} changes are waiting to sync."),
                        };

                        view! {
                            <span>
                                <strong>"Local only."</strong>
                                {where_it_goes}
                                {queued}
                            </span>
                        }
                            .into_any()
                    }
                }}
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
