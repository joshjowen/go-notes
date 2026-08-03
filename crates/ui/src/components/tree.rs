//! The file tree.
//!
//! Drag-and-drop here is not a display convenience: dropping a note into a
//! folder performs a real `rename(2)` on the server, and the wikilinks pointing
//! at that note are rewritten to follow it. What the sidebar shows is what is on
//! disk.

use go_notes_shared::{paths, TreeNode};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::state::{title_of, use_app, AppState};
use crate::vault;

/// Where a context menu is open, and on what.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuTarget {
    pub path: String,
    pub is_folder: bool,
    pub x: i32,
    pub y: i32,
}

#[component]
pub fn FileTree() -> impl IntoView {
    let state = use_app();
    let menu = RwSignal::new(None::<MenuTarget>);
    // The path currently being dragged, so a folder can highlight as a target.
    let dragging = RwSignal::new(None::<String>);

    view! {
        <div class="gn-tree" on:click=move |_| menu.set(None)>
            <div class="gn-tree-toolbar">
                <button
                    class="gn-icon-button"
                    title="New note"
                    on:click=move |_| create_note_in(state, "")
                >
                    "＋"
                </button>
                <button
                    class="gn-icon-button"
                    title="New folder"
                    on:click=move |_| create_folder_in(state, "")
                >
                    "🗀"
                </button>
            </div>

            <div
                class="gn-tree-body"
                class:gn-drop-root=move || dragging.get().is_some()
                on:dragover=move |ev| {
                    if dragging.get_untracked().is_some() {
                        ev.prevent_default();
                    }
                }
                on:drop=move |ev| {
                    ev.prevent_default();
                    if let Some(from) = dragging.get_untracked() {
                        move_entry(state, from, String::new());
                        dragging.set(None);
                    }
                }
            >
                {move || {
                    match state.tree.get() {
                        None => view! { <p class="gn-empty">"Loading…"</p> }.into_any(),
                        Some(TreeNode::Folder { children, .. }) if children.is_empty() => {
                            view! {
                                <p class="gn-empty">
                                    "No notes yet. Press " <kbd>"Ctrl"</kbd> "+" <kbd>"N"</kbd>
                                    " to write your first one."
                                </p>
                            }
                                .into_any()
                        }
                        Some(TreeNode::Folder { children, .. }) => {
                            view! {
                                <ul class="gn-tree-list">
                                    {children
                                        .into_iter()
                                        .map(|child| {
                                            view! { <TreeEntry node=child depth=0 menu dragging /> }
                                        })
                                        .collect_view()}
                                </ul>
                            }
                                .into_any()
                        }
                        Some(_) => view! { <p class="gn-empty">"Unexpected tree"</p> }.into_any(),
                    }
                }}
            </div>

            <Show when=move || menu.get().is_some()>
                <ContextMenu menu />
            </Show>
        </div>
    }
}

#[component]
fn TreeEntry(
    node: TreeNode,
    depth: usize,
    menu: RwSignal<Option<MenuTarget>>,
    dragging: RwSignal<Option<String>>,
) -> impl IntoView {
    let state = use_app();
    let indent = format!("padding-left: {}px", 6 + depth * 13);

    match node {
        TreeNode::Folder {
            name,
            path,
            collapsed,
            children,
        } => {
            let open = RwSignal::new(!collapsed);
            let drop_target = RwSignal::new(false);

            let folder_path = path.clone();
            let toggle_path = path.clone();
            let menu_path = path.clone();
            let menu_button_path = path.clone();
            let drop_path = path.clone();

            view! {
                <li class="gn-tree-folder">
                    <div
                        class="gn-tree-row"
                        class:gn-drop-target=move || drop_target.get()
                        style=indent
                        draggable="true"
                        on:click=move |_| {
                            let next = !open.get_untracked();
                            open.set(next);
                            let path = toggle_path.clone();
                            spawn_local(async move {
                                vault::set_folder_collapsed(state, path, !next).await;
                            });
                        }
                        on:contextmenu=move |ev| {
                            ev.prevent_default();
                            ev.stop_propagation();
                            menu.set(
                                Some(MenuTarget {
                                    path: menu_path.clone(),
                                    is_folder: true,
                                    x: ev.client_x(),
                                    y: ev.client_y(),
                                }),
                            );
                        }
                        on:dragstart={
                            let path = folder_path.clone();
                            move |_| dragging.set(Some(path.clone()))
                        }
                        on:dragend=move |_| {
                            dragging.set(None);
                            drop_target.set(false);
                        }
                        on:dragover={
                            let path = drop_path.clone();
                            move |ev| {
                                let Some(from) = dragging.get_untracked() else { return };
                                // Refuse the drop that would delete the subtree.
                                if from == path || paths::is_within(&path, &from) {
                                    return;
                                }
                                ev.prevent_default();
                                drop_target.set(true);
                            }
                        }
                        on:dragleave=move |_| drop_target.set(false)
                        on:drop={
                            let path = drop_path.clone();
                            move |ev| {
                                ev.prevent_default();
                                ev.stop_propagation();
                                drop_target.set(false);
                                if let Some(from) = dragging.get_untracked() {
                                    move_entry(state, from, path.clone());
                                    dragging.set(None);
                                }
                            }
                        }
                    >
                        <span class="gn-tree-chevron" class:gn-open=move || open.get()>
                            "▸"
                        </span>
                        <span class="gn-tree-name">{name}</span>
                        <button
                            class="gn-row-menu"
                            title="Actions"
                            aria-label="Actions"
                            on:click={
                                let path = menu_button_path.clone();
                                move |ev| {
                                    ev.stop_propagation();
                                    menu.set(
                                        Some(MenuTarget {
                                            path: path.clone(),
                                            is_folder: true,
                                            x: ev.client_x(),
                                            y: ev.client_y(),
                                        }),
                                    );
                                }
                            }
                        >
                            "⋯"
                        </button>
                    </div>

                    <Show when=move || open.get()>
                        <ul class="gn-tree-list">
                            {children
                                .clone()
                                .into_iter()
                                .map(|child| {
                                    view! { <TreeEntry node=child depth=depth + 1 menu dragging /> }
                                })
                                .collect_view()}
                        </ul>
                    </Show>
                </li>
            }
            .into_any()
        }

        TreeNode::Note { path, title, .. } => {
            let open_path = path.clone();
            let open_title = title.clone();
            let menu_path = path.clone();
            let menu_button_path = path.clone();
            let drag_path = path.clone();
            let is_active = {
                let path = path.clone();
                move || state.active_path().as_deref() == Some(path.as_str())
            };

            view! {
                <li>
                    <div
                        class="gn-tree-row gn-tree-note"
                        class:gn-active=is_active
                        style=indent
                        draggable="true"
                        on:click=move |_| {
                            state.open_tab(open_path.clone(), open_title.clone());
                        }
                        on:contextmenu=move |ev| {
                            ev.prevent_default();
                            ev.stop_propagation();
                            menu.set(
                                Some(MenuTarget {
                                    path: menu_path.clone(),
                                    is_folder: false,
                                    x: ev.client_x(),
                                    y: ev.client_y(),
                                }),
                            );
                        }
                        on:dragstart=move |_| dragging.set(Some(drag_path.clone()))
                        on:dragend=move |_| dragging.set(None)
                    >
                        <span class="gn-tree-name">{title}</span>
                        // Long-pressing a row raises a `contextmenu` event on
                        // most touch browsers, but not reliably and never
                        // discoverably. The stylesheet shows this button only
                        // where there is no mouse to right-click with.
                        <button
                            class="gn-row-menu"
                            title="Actions"
                            aria-label="Actions"
                            on:click={
                                let path = menu_button_path.clone();
                                move |ev| {
                                    ev.stop_propagation();
                                    menu.set(
                                        Some(MenuTarget {
                                            path: path.clone(),
                                            is_folder: false,
                                            x: ev.client_x(),
                                            y: ev.client_y(),
                                        }),
                                    );
                                }
                            }
                        >
                            "⋯"
                        </button>
                    </div>
                </li>
            }
            .into_any()
        }
    }
}

#[component]
fn ContextMenu(menu: RwSignal<Option<MenuTarget>>) -> impl IntoView {
    let state = use_app();

    view! {
        {move || {
            let Some(target) = menu.get() else { return ().into_any() };
            let style = format!("left: {}px; top: {}px", target.x, target.y);
            let is_folder = target.is_folder;
            // One clone per handler: each `on:click` closure needs its own owned
            // copy, since they all outlive this block.
            let for_new_note = target.path.clone();
            let for_new_folder = target.path.clone();
            let for_rename = target.path.clone();
            let for_move = target.path.clone();
            let for_delete = target.path.clone();

            let folder_items = is_folder.then(|| {
                view! {
                    <button on:click=move |_| {
                        create_note_in(state, &for_new_note);
                        menu.set(None);
                    }>"New note here"</button>
                    <button on:click=move |_| {
                        create_folder_in(state, &for_new_folder);
                        menu.set(None);
                    }>"New folder here"</button>
                }
            });

            view! {
                <div class="gn-context-menu" style=style on:click=move |ev| ev.stop_propagation()>
                    {folder_items}

                    <button on:click=move |_| {
                        rename_entry(state, &for_rename, is_folder);
                        menu.set(None);
                    }>"Rename…"</button>

                    <button on:click=move |_| {
                        move_to_folder(state, &for_move, is_folder);
                        menu.set(None);
                    }>"Move…"</button>

                    <button
                        class="gn-danger"
                        on:click=move |_| {
                            delete_entry(state, &for_delete, is_folder);
                            menu.set(None);
                        }
                    >
                        "Delete"
                    </button>
                </div>
            }
                .into_any()
        }}
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

fn prompt(message: &str, default: &str) -> Option<String> {
    let window = web_sys::window()?;
    let answer = window.prompt_with_message_and_default(message, default).ok()??;
    let trimmed = answer.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Like [`prompt`], but an empty answer is an answer rather than a cancellation.
fn prompt_allowing_empty(message: &str, default: &str) -> Option<String> {
    let window = web_sys::window()?;
    let answer = window.prompt_with_message_and_default(message, default).ok()??;
    Some(answer.trim().to_string())
}

pub fn confirm(message: &str) -> bool {
    web_sys::window()
        .and_then(|window| window.confirm_with_message(message).ok())
        .unwrap_or(false)
}

pub fn create_note_in(state: AppState, folder: &str) {
    let Some(name) = prompt("Name for the new note", "Untitled") else {
        return;
    };
    // Validating here rather than only on the server means the user finds out
    // before the round trip, with the same rule the server will apply.
    if let Err(err) = paths::validate_component(&name) {
        state.error(err.to_string());
        return;
    }

    let path = paths::join(folder, &format!("{name}.md"));
    spawn_local(async move {
        let markdown = format!("# {}\n\n", title_of(&path));
        match vault::create_note(state, path.clone(), markdown.clone()).await {
            Ok(written) => {
                state.refresh_all();
                state.open_tab(written.path.clone(), written.title.clone());
                crate::save::opened(state, &written.path, written.content_hash.clone(), markdown);
                if written.queued {
                    state.notify("Created on this device. It will reach the server when the connection is back.");
                }
            }
            Err(err) => state.error(err.user_message()),
        }
    });
}

pub fn create_folder_in(state: AppState, parent: &str) {
    let Some(name) = prompt("Name for the new folder", "New folder") else {
        return;
    };
    if let Err(err) = paths::validate_component(&name) {
        state.error(err.to_string());
        return;
    }

    let path = paths::join(parent, &name);
    spawn_local(async move {
        match vault::create_folder(state, path).await {
            Ok(_) => state.refresh_tree(),
            Err(err) => state.error(err.user_message()),
        }
    });
}

fn rename_entry(state: AppState, path: &str, is_folder: bool) {
    let current = paths::basename(path);
    let suggestion = if is_folder {
        current.to_string()
    } else {
        paths::stem(path).to_string()
    };

    let Some(name) = prompt("New name", &suggestion) else {
        return;
    };
    if let Err(err) = paths::validate_component(&name) {
        state.error(err.to_string());
        return;
    }

    let parent = paths::parent_of(path);
    let target = if is_folder {
        paths::join(parent, &name)
    } else {
        paths::join(parent, &format!("{name}.md"))
    };
    move_entry_inner(state, path.to_string(), target, is_folder);
}

/// Moves an entry by asking for the destination folder.
///
/// Dragging a note into a folder needs a pointer that can hover, which a finger
/// cannot — so on a phone this is the only way to reorganise a vault. It takes
/// a folder path rather than a full path because that is the operation people
/// mean: keep the name, change where it lives.
fn move_to_folder(state: AppState, path: &str, is_folder: bool) {
    let current = paths::parent_of(path);
    // Not `prompt`: an empty answer means the top level here, and that helper
    // cannot tell an empty answer from a cancelled one — which would turn
    // "cancel" into "move this to the root".
    let Some(destination) = prompt_allowing_empty(
        "Move to which folder? Leave empty for the top level.",
        current,
    ) else {
        return;
    };

    let destination = destination.trim_matches('/').to_string();
    if !destination.is_empty() {
        if let Err(err) = paths::validate_folder_path(&destination) {
            state.error(err.to_string());
            return;
        }
    }
    if is_folder && (destination == path || paths::is_within(&destination, path)) {
        state.error("A folder cannot be moved inside itself.");
        return;
    }

    let target = paths::join(&destination, paths::basename(path));
    move_entry_inner(state, path.to_string(), target, is_folder);
}

/// Moves an entry into a folder, keeping its name.
fn move_entry(state: AppState, from: String, into_folder: String) {
    let name = paths::basename(&from).to_string();
    let target = paths::join(&into_folder, &name);
    if target == from {
        return;
    }
    // Guess folder-ness from the extension; only notes end in `.md`.
    let is_folder = !paths::has_md_extension(&from);
    move_entry_inner(state, from, target, is_folder);
}

fn move_entry_inner(state: AppState, from: String, to: String, is_folder: bool) {
    if from == to {
        return;
    }
    spawn_local(async move {
        let result = if is_folder {
            vault::move_folder(state, from.clone(), to.clone()).await
        } else {
            vault::move_note(state, from.clone(), to.clone()).await
        };

        match result {
            Ok((moved_to, links_rewritten)) => {
                state.rename_tab(&from, &moved_to);
                state.refresh_all();
                if links_rewritten > 0 {
                    let plural = if links_rewritten == 1 { "note" } else { "notes" };
                    state.notify(format!("Moved. Updated links in {links_rewritten} {plural}."));
                } else if state.local_only() {
                    // Offline the rename happens here and the links are left
                    // alone; the server rewrites them when the move is replayed,
                    // which is worth saying rather than leaving someone to
                    // wonder why their links did not follow.
                    state.notify("Moved on this device. Links elsewhere are rewritten when this syncs.");
                } else {
                    state.notify("Moved.");
                }
            }
            Err(err) => state.error(err.user_message()),
        }
    });
}

fn delete_entry(state: AppState, path: &str, is_folder: bool) {
    let what = if is_folder { "folder" } else { "note" };
    if !confirm(&format!(
        "Move this {what} to the vault's trash?\n\n{path}\n\nIt stays on disk under .trash/ and can be restored by hand."
    )) {
        return;
    }

    let path = path.to_string();
    spawn_local(async move {
        let result = if is_folder {
            vault::delete_folder(state, path.clone()).await
        } else {
            vault::delete_note(state, path.clone()).await
        };

        match result {
            Ok(queued) => {
                state.close_tabs_under(&path);
                state.refresh_all();
                state.notify(if queued {
                    "Removed here. It goes to the vault's trash when this syncs."
                } else {
                    "Moved to trash."
                });
            }
            Err(err) => state.error(err.user_message()),
        }
    });
}
