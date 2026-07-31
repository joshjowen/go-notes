//! The editing surface: one Milkdown instance, reused across tabs.
//!
//! There is a single editor rather than one per tab. Milkdown instances are
//! expensive to build, and a document swap is cheap — so switching tabs replaces
//! the content rather than the editor. The visible consequence is that undo
//! history does not follow you between tabs, which is the correct behaviour
//! anyway: undo should never reach across two different files.
//!
//! Saving is debounced and guarded. Every write carries the content hash the
//! client last saw, so an edit made elsewhere — another tab, an SSH session, a
//! `git pull` — surfaces as a conflict the user resolves rather than as silent
//! data loss.

use std::cell::RefCell;
use std::rc::Rc;

use gloo_timers::callback::Timeout;
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;

use crate::api::{self, ApiFailure};
use crate::editor::{self, EditorConfig, EditorHandle};
use crate::state::{use_app, AppState, Conflict};

/// How long to wait after the last keystroke before writing to disk.
///
/// Short enough that a save is never more than a moment behind, long enough that
/// ordinary typing produces one write per pause rather than one per character.
const AUTOSAVE_DELAY_MS: u32 = 800;

#[component]
pub fn EditorPane() -> impl IntoView {
    let state = use_app();
    let host: NodeRef<html::Div> = NodeRef::new();

    // Single-threaded WASM, so `Rc<RefCell<_>>` is the right shared-mutable
    // primitive here; none of this ever crosses a thread boundary.
    let handle: Rc<RefCell<Option<EditorHandle>>> = Rc::new(RefCell::new(None));
    let pending_save: Rc<RefCell<Option<Timeout>>> = Rc::new(RefCell::new(None));
    // The path the editor currently holds, read inside callbacks that fire long
    // after the render that created them.
    let loaded_path: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let dragging_file = RwSignal::new(false);

    // Load whichever note is active, creating the editor on first use.
    Effect::new({
        let handle = handle.clone();
        let pending_save = pending_save.clone();
        let loaded_path = loaded_path.clone();

        move |_| {
            let Some(path) = state.active_path() else {
                return;
            };
            if *loaded_path.borrow() == path {
                return;
            }

            let Some(element) = host.get() else { return };

            // Switching away from a note must not leave its save queued against
            // the newly-opened one.
            pending_save.borrow_mut().take();

            let handle = handle.clone();
            let pending_save = pending_save.clone();
            let loaded_path = loaded_path.clone();
            let element: web_sys::HtmlElement = element.unchecked_into();

            spawn_local(async move {
                let note = match api::read_note(path.clone()).await {
                    Ok(note) => note,
                    Err(err) => {
                        state.error(err.user_message());
                        return;
                    }
                };

                *loaded_path.borrow_mut() = path.clone();
                state.set_hash(&path, note.meta.content_hash.clone());
                state.mark_dirty(&path, false);
                state.backlinks.set(note.backlinks.clone());
                state.active_markdown.set(note.markdown.clone());

                // The id is copied out and the borrow released before awaiting,
                // so no `Ref` is ever held across a suspension point.
                let existing_id = {
                    let borrowed = handle.borrow();
                    if let Some(editor) = borrowed.as_ref() {
                        editor.set_known_targets(&state.known_targets());
                    }
                    borrowed.as_ref().map(|editor| editor.id())
                };
                if let Some(id) = existing_id {
                    editor::set_markdown(id, &note.markdown).await;
                    // Put the caret back in the document after a tab switch, so
                    // typing continues to work without an extra click.
                    if let Some(editor) = handle.borrow().as_ref() {
                        editor.focus();
                    }
                    return;
                }

                let created = editor::mount(
                    &element,
                    EditorConfig {
                        markdown: note.markdown.clone(),
                        mode: state.editor_mode.get_untracked(),
                        known_targets: state.known_targets(),
                        on_change: {
                            let pending_save = pending_save.clone();
                            let loaded_path = loaded_path.clone();
                            move |markdown: String| {
                                let path = loaded_path.borrow().clone();
                                if path.is_empty() {
                                    return;
                                }
                                state.mark_dirty(&path, true);
                                state.active_markdown.set(markdown.clone());

                                // Replacing the timeout is the debounce: the
                                // previous one is dropped and therefore cancelled.
                                let scheduled = Timeout::new(AUTOSAVE_DELAY_MS, {
                                    let path = path.clone();
                                    move || save_now(state, path.clone(), markdown.clone())
                                });
                                *pending_save.borrow_mut() = Some(scheduled);
                            }
                        },
                        on_wikilink_query: move |query: String| {
                            editor::promise_from_future(async move {
                                match api::quickswitch(query).await {
                                    Ok(items) => {
                                        serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
                                    }
                                    Err(_) => "[]".to_string(),
                                }
                            })
                        },
                        on_open_link: move |target: String| open_link(state, target),
                        on_upload: move |file: web_sys::File| {
                            editor::promise_from_future(async move {
                                match api::upload_attachment(file).await {
                                    Ok(response) => response.url,
                                    Err(err) => {
                                        state.error(err.user_message());
                                        String::new()
                                    }
                                }
                            })
                        },
                    },
                )
                .await;

                match created {
                    Some(editor) => {
                        editor.focus();
                        *handle.borrow_mut() = Some(editor);
                    }
                    None => state.error(
                        "The editor failed to start. Reload the page, and check the browser console.",
                    ),
                }
            });
        }
    });

    // Keep unresolved-link styling in step with what exists in the vault.
    Effect::new({
        let handle = handle.clone();
        move |_| {
            let targets = state.known_targets();
            if let Some(editor) = handle.borrow().as_ref() {
                editor.set_known_targets(&targets);
            }
        }
    });

    // Apply the rich-text/markdown toggle.
    Effect::new({
        let handle = handle.clone();
        move |_| {
            let mode = state.editor_mode.get();
            let id = handle.borrow().as_ref().map(|editor| editor.id());
            if let Some(id) = id {
                spawn_local(async move { editor::set_mode(id, mode).await });
            }
        }
    });

    // Resolving a conflict by taking the version from disk has to push that text
    // into the editor, which only this component can do.
    Effect::new({
        let handle = handle.clone();
        let loaded_path = loaded_path.clone();
        move |_| {
            let Some(conflict) = state.conflict.get() else {
                return;
            };
            // A `None` marker in `theirs` is how the dialog signals "load theirs".
            if conflict.theirs != TAKE_THEIRS_MARKER {
                return;
            }
            state.conflict.set(None);

            let id = handle.borrow().as_ref().map(|editor| editor.id());
            let path = loaded_path.borrow().clone();
            spawn_local(async move {
                if let Ok(note) = api::read_note(path.clone()).await {
                    state.set_hash(&path, note.meta.content_hash.clone());
                    state.mark_dirty(&path, false);
                    if let Some(id) = id {
                        editor::set_markdown(id, &note.markdown).await;
                    }
                    state.notify("Loaded the version from disk.");
                }
            });
        }
    });

    // Ctrl+S, routed through a signal because the shell handles the keystroke
    // but only this component holds the editor.
    Effect::new({
        let handle = handle.clone();
        let loaded_path = loaded_path.clone();
        move |previous: Option<u32>| {
            let requested = state.save_requested.get();
            // Skip the initial run, which fires on mount rather than on a keypress.
            if previous.is_none() {
                return requested;
            }
            let path = loaded_path.borrow().clone();
            if !path.is_empty() {
                let markdown = handle
                    .borrow()
                    .as_ref()
                    .map(|editor| editor.markdown())
                    .unwrap_or_default();
                save_now(state, path, markdown);
            }
            requested
        }
    });

    view! {
        <div class="gn-editor-toolbar">
            <span class="gn-editor-path">
                {move || state.active_path().unwrap_or_else(|| "No note open".into())}
            </span>
            <button
                class="gn-mode-toggle"
                title="Switch between rich text and raw markdown"
                on:click=move |_| state.editor_mode.update(|mode| *mode = mode.toggled())
            >
                {move || state.editor_mode.get().label()}
            </button>
        </div>

        <div
            class="gn-editor-host"
            class:gn-dragging-file=move || dragging_file.get()
            node_ref=host
            on:dragover=move |ev| {
                // Only react to a file being dragged in from outside the page;
                // an internal text drag must keep ProseMirror's own behaviour.
                if has_files(&ev) {
                    ev.prevent_default();
                    dragging_file.set(true);
                }
            }
            on:dragleave=move |_| dragging_file.set(false)
            on:drop={
                let handle = handle.clone();
                move |ev| {
                    if !has_files(&ev) {
                        return;
                    }
                    ev.prevent_default();
                    dragging_file.set(false);
                    let Some(files) = ev.data_transfer().and_then(|dt| dt.files()) else {
                        return;
                    };
                    let editor_id = handle.borrow().as_ref().map(|editor| editor.id());
                    for index in 0..files.length() {
                        let Some(file) = files.get(index) else { continue };
                        spawn_local(async move {
                            match api::upload_attachment(file).await {
                                Ok(response) => {
                                    let snippet = if response.is_image {
                                        format!("![{}]({})", response.path, response.url)
                                    } else {
                                        format!("[{}]({})", response.path, response.url)
                                    };
                                    if let Some(id) = editor_id {
                                        editor::insert_markdown(id, &snippet).await;
                                    }
                                }
                                Err(err) => state.error(err.user_message()),
                            }
                        });
                    }
                }
            }
        ></div>
    }
}

/// Sentinel written into `Conflict::theirs` to ask the editor to reload the file.
pub const TAKE_THEIRS_MARKER: &str = "\u{0}__go_notes_take_theirs__";

fn has_files(ev: &web_sys::DragEvent) -> bool {
    ev.data_transfer()
        .map(|dt| dt.types().includes(&wasm_bindgen::JsValue::from_str("Files"), 0))
        .unwrap_or(false)
}

/// Writes a note, turning a lost update into a visible conflict.
fn save_now(state: AppState, path: String, markdown: String) {
    let expected = state.hash_for(&path);
    spawn_local(async move {
        match api::save_note(path.clone(), markdown.clone(), expected).await {
            Ok(response) => {
                state.set_hash(&path, response.meta.content_hash.clone());
                state.mark_dirty(&path, false);
                // A save can create links, so the graph and the tree may both
                // have changed — a new note title, a link that now resolves.
                state.refresh_all();
            }
            Err(ApiFailure::Conflict(body)) => {
                state.conflict.set(Some(Conflict {
                    path: path.clone(),
                    theirs: body.current_markdown,
                    their_hash: body.current_hash,
                    mine: markdown,
                }));
            }
            Err(err) => state.error(err.user_message()),
        }
    });
}

/// Follows a `[[wikilink]]`, offering to create the note when it does not exist.
fn open_link(state: AppState, target: String) {
    spawn_local(async move {
        let candidates = api::quickswitch(target.clone()).await.unwrap_or_default();

        // Prefer an exact title match over a mere substring hit, so following
        // `[[Budget]]` never lands on "Budget Archive" because it sorted first.
        let exact = candidates
            .iter()
            .find(|item| item.exists && item.title.eq_ignore_ascii_case(&target))
            .or_else(|| candidates.iter().find(|item| item.exists));

        if let Some(item) = exact {
            state.open_tab(item.path.clone(), item.title.clone());
            return;
        }

        // Nothing matched: the link points at a note that has not been written
        // yet, which is a normal state in a linked vault rather than an error.
        let path = if target.ends_with(".md") {
            target.clone()
        } else {
            format!("{target}.md")
        };
        if go_notes_shared::paths::validate_note_path(&path).is_err() {
            state.error(format!("'{target}' is not a valid note name."));
            return;
        }

        match api::create_note(path, format!("# {target}\n\n")).await {
            Ok(response) => {
                state.refresh_all();
                state.open_tab(response.meta.path.clone(), response.meta.title.clone());
                state.notify("Created a new note for that link.");
            }
            Err(err) => state.error(err.user_message()),
        }
    });
}
