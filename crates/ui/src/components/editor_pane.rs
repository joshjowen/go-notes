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

use crate::editor::{self, EditorConfig, EditorHandle};
use crate::save;
use crate::state::{use_app, AppState};
use crate::vault;

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
    // The text the pending timeout above would send, so switching tabs can
    // flush it immediately instead of just dropping the timer.
    let pending_text: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    // The path the editor currently holds, read inside callbacks that fire long
    // after the render that created them.
    let loaded_path: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let dragging_file = RwSignal::new(false);

    // Load whichever note is active, creating the editor on first use.
    Effect::new({
        let handle = handle.clone();
        let pending_save = pending_save.clone();
        let pending_text = pending_text.clone();
        let loaded_path = loaded_path.clone();

        move |_| {
            let Some(path) = state.active_path() else {
                return;
            };
            if *loaded_path.borrow() == path {
                return;
            }

            let Some(element) = host.get() else { return };

            // Switching away from a note must not leave its last edit stranded
            // behind a timer that will never fire against it again — send it
            // now rather than dropping it.
            pending_save.borrow_mut().take();
            if let Some(markdown) = pending_text.borrow_mut().take() {
                let old_path = loaded_path.borrow().clone();
                if !old_path.is_empty() {
                    save::flush(state, old_path, markdown);
                }
            }

            let handle = handle.clone();
            let pending_save = pending_save.clone();
            let pending_text = pending_text.clone();
            let loaded_path = loaded_path.clone();
            let element: web_sys::HtmlElement = element.unchecked_into();

            spawn_local(async move {
                let note = match vault::read_note(state, path.clone()).await {
                    Ok(note) => note,
                    Err(err) => {
                        state.error(err.user_message());
                        // Close the tab rather than leaving it focused on a note
                        // that could not be read. The editor still holds the
                        // previous document, and a tab that looks like `B.md`
                        // over the text of `A.md` would send anything typed into
                        // it to `A.md`. Far likelier offline, where opening a
                        // note this device has never seen simply fails.
                        state.close_tab_with_path(&path);
                        return;
                    }
                };

                *loaded_path.borrow_mut() = path.clone();
                save::opened(state, &path, note.meta.content_hash.clone(), note.markdown.clone());
                state.mark_dirty(&path, false);
                state.backlinks.set(note.backlinks.clone());
                state.suggested_links.set(note.suggested.clone());
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
                            let pending_text = pending_text.clone();
                            let loaded_path = loaded_path.clone();
                            move |markdown: String| {
                                let path = loaded_path.borrow().clone();
                                if path.is_empty() {
                                    return;
                                }
                                state.mark_dirty(&path, true);
                                state.active_markdown.set(markdown.clone());
                                *pending_text.borrow_mut() = Some(markdown.clone());

                                // Replacing the timeout is the debounce: the
                                // previous one is dropped and therefore cancelled.
                                let scheduled = Timeout::new(AUTOSAVE_DELAY_MS, {
                                    let path = path.clone();
                                    let pending_text = pending_text.clone();
                                    move || {
                                        pending_text.borrow_mut().take();
                                        save::request(state, path.clone(), markdown.clone());
                                    }
                                });
                                *pending_save.borrow_mut() = Some(scheduled);
                            }
                        },
                        on_wikilink_query: move |query: String| {
                            editor::promise_from_future(async move {
                                let items = vault::quickswitch(state, query).await;
                                serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
                            })
                        },
                        on_open_link: move |target: String| open_link(state, target),
                        on_upload: move |file: web_sys::File| {
                            editor::promise_from_future(async move {
                                match vault::upload_attachment(state, file).await {
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

    // Re-reading the open note has to push text into the editor, which only this
    // component can do. Asked for after a sync brings a newer version down, and
    // when a conflict is resolved in favour of the server's copy.
    Effect::new({
        let handle = handle.clone();
        let loaded_path = loaded_path.clone();
        move |previous: Option<u32>| {
            let requested = state.reload_requested.get();
            // The first run is the mount, not a request.
            if previous.is_none() {
                return requested;
            }

            let id = handle.borrow().as_ref().map(|editor| editor.id());
            let path = loaded_path.borrow().clone();
            if path.is_empty() {
                return requested;
            }
            spawn_local(async move {
                if let Ok(note) = vault::read_note(state, path.clone()).await {
                    save::opened(state, &path, note.meta.content_hash.clone(), note.markdown.clone());
                    state.mark_dirty(&path, false);
                    state.active_markdown.set(note.markdown.clone());
                    if let Some(id) = id {
                        // Not `set_markdown`: this can fire from a background
                        // refresh while the note is still open, and rebuilding
                        // the editor would drop the cursor for a change that,
                        // most of the time, touched nothing the person can see.
                        editor::patch_markdown(id, &note.markdown).await;
                    }
                }
            });
            requested
        }
    });

    // Ctrl+S, routed through a signal because the shell handles the keystroke
    // but only this component holds the editor.
    Effect::new({
        let handle = handle.clone();
        let pending_text = pending_text.clone();
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
                pending_text.borrow_mut().take();
                save::request(state, path, markdown);
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
                            match vault::upload_attachment(state, file).await {
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

fn has_files(ev: &web_sys::DragEvent) -> bool {
    ev.data_transfer()
        .map(|dt| dt.types().includes(&wasm_bindgen::JsValue::from_str("Files"), 0))
        .unwrap_or(false)
}

/// Follows a `[[wikilink]]`, offering to create the note when it does not exist.
fn open_link(state: AppState, target: String) {
    spawn_local(async move {
        let candidates = vault::quickswitch(state, target.clone()).await;

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

        let markdown = format!("# {target}\n\n");
        match vault::create_note(state, path, markdown.clone()).await {
            Ok(written) => {
                state.refresh_all();
                state.open_tab(written.path.clone(), written.title.clone());
                // Gives the new tab a real `If-Match` token straight away,
                // rather than leaving it empty until the load effect's own
                // read of the note comes back.
                save::opened(state, &written.path, written.content_hash, markdown);
                state.notify("Created a new note for that link.");
            }
            Err(err) => state.error(err.user_message()),
        }
    });
}
