//! The quick switcher (Ctrl+P) and the command palette (Ctrl+Shift+P).
//!
//! Both are the same widget with a different source of rows, which is why they
//! share a component: the interaction — type, arrow, Enter — has to feel
//! identical whichever one you opened.

use go_notes_shared::QuickSwitchItem;
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::tree::{create_folder_in, create_note_in};
use crate::state::{use_app, AppState, MainView, Palette, RightPanel};

/// One selectable row.
#[derive(Clone)]
struct Row {
    label: String,
    hint: String,
    action: RowAction,
}

#[derive(Clone)]
enum RowAction {
    OpenNote { path: String, title: String },
    CreateNote { path: String },
    Command(fn(AppState)),
}

#[component]
pub fn CommandPalette() -> impl IntoView {
    let state = use_app();
    let query = RwSignal::new(String::new());
    let selected = RwSignal::new(0usize);
    let notes = RwSignal::new(Vec::<QuickSwitchItem>::new());
    let input: NodeRef<html::Input> = NodeRef::new();

    // Refetch matches as the user types. The server does the ranking, so this
    // stays correct for a vault far larger than would be sensible to hold here.
    Effect::new(move |_| {
        if state.palette.get() != Some(Palette::QuickSwitch) {
            return;
        }
        let text = query.get();
        spawn_local(async move {
            if let Ok(items) = api::quickswitch(text).await {
                notes.set(items);
                selected.set(0);
            }
        });
    });

    // Focus the field as soon as the palette appears.
    Effect::new(move |_| {
        if state.palette.get().is_some() {
            if let Some(element) = input.get() {
                let _ = element.focus();
            }
        }
    });

    // A closure rather than a `Memo`: `Row` holds a function pointer and has no
    // meaningful equality, which `Memo` requires to decide whether to notify.
    let rows = move || match state.palette.get() {
        Some(Palette::QuickSwitch) => notes
            .get()
            .into_iter()
            .map(|item| {
                if item.exists {
                    Row {
                        label: item.title.clone(),
                        hint: item.path.clone(),
                        action: RowAction::OpenNote {
                            path: item.path,
                            title: item.title,
                        },
                    }
                } else {
                    Row {
                        label: format!("Create “{}”", item.title),
                        hint: item.path.clone(),
                        action: RowAction::CreateNote { path: item.path },
                    }
                }
            })
            .collect::<Vec<_>>(),

        Some(Palette::Commands) => {
            let filter = query.get().to_lowercase();
            commands()
                .into_iter()
                .filter(|row| filter.is_empty() || row.label.to_lowercase().contains(&filter))
                .collect()
        }

        None => Vec::new(),
    };

    let run = move |row: Row| {
        state.palette.set(None);
        query.set(String::new());

        match row.action {
            RowAction::OpenNote { path, title } => state.open_tab(path, title),
            RowAction::CreateNote { path } => {
                let title = go_notes_shared::paths::stem(&path).to_string();
                spawn_local(async move {
                    match api::create_note(path, format!("# {title}\n\n")).await {
                        Ok(response) => {
                            state.refresh_all();
                            state.open_tab(response.meta.path.clone(), response.meta.title.clone());
                        }
                        Err(err) => state.error(err.user_message()),
                    }
                });
            }
            RowAction::Command(command) => command(state),
        }
    };

    view! {
        <Show when=move || state.palette.get().is_some()>
            <div
                class="gn-overlay"
                on:mousedown=move |_| state.palette.set(None)
            >
                <div class="gn-palette" on:mousedown=move |ev| ev.stop_propagation()>
                    <input
                        node_ref=input
                        type="text"
                        autocomplete="off"
                        spellcheck="false"
                        placeholder=move || {
                            match state.palette.get() {
                                Some(Palette::Commands) => "Run a command…",
                                _ => "Search notes by name, or type a new name to create one…",
                            }
                        }
                        prop:value=move || query.get()
                        on:input=move |ev| {
                            query.set(event_target_value(&ev));
                            selected.set(0);
                        }
                        on:keydown=move |ev| {
                            let options = rows();
                            match ev.key().as_str() {
                                "Escape" => {
                                    ev.prevent_default();
                                    state.palette.set(None);
                                }
                                "ArrowDown" => {
                                    ev.prevent_default();
                                    if !options.is_empty() {
                                        selected
                                            .update(|index| {
                                                *index = (*index + 1) % options.len()
                                            });
                                    }
                                }
                                "ArrowUp" => {
                                    ev.prevent_default();
                                    if !options.is_empty() {
                                        selected
                                            .update(|index| {
                                                *index = (*index + options.len() - 1)
                                                    % options.len()
                                            });
                                    }
                                }
                                "Enter" => {
                                    ev.prevent_default();
                                    if let Some(row) = options
                                        .get(selected.get_untracked())
                                        .cloned()
                                    {
                                        run(row);
                                    }
                                }
                                _ => {}
                            }
                        }
                    />

                    <div class="gn-palette-list">
                        {move || {
                            let options = rows();
                            if options.is_empty() {
                                return view! {
                                    <p class="gn-empty">"Nothing matches."</p>
                                }
                                    .into_any();
                            }
                            options
                                .into_iter()
                                .enumerate()
                                .map(|(index, row)| {
                                    let label = row.label.clone();
                                    let hint = row.hint.clone();
                                    let for_click = row.clone();
                                    view! {
                                        <button
                                            class="gn-palette-option"
                                            class:gn-selected=move || selected.get() == index
                                            on:mouseenter=move |_| selected.set(index)
                                            on:click=move |_| run(for_click.clone())
                                        >
                                            <span>{label}</span>
                                            <span class="gn-palette-hint">{hint}</span>
                                        </button>
                                    }
                                })
                                .collect_view()
                                .into_any()
                        }}
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// The command list. Kept small and obvious rather than exhaustive — every entry
/// here is something a person would plausibly go looking for by name.
fn commands() -> Vec<Row> {
    fn row(label: &str, hint: &str, action: fn(AppState)) -> Row {
        Row {
            label: label.to_string(),
            hint: hint.to_string(),
            action: RowAction::Command(action),
        }
    }

    vec![
        row("New note", "Alt+N", |state| create_note_in(state, "")),
        row("New folder", "", |state| create_folder_in(state, "")),
        row("Open graph view", "Ctrl+G", |state| {
            state.main_view.set(MainView::Graph)
        }),
        row("Open editor", "", |state| {
            state.main_view.set(MainView::Editor)
        }),
        row("Toggle rich text / markdown", "", |state| {
            state.editor_mode.update(|mode| *mode = mode.toggled())
        }),
        row("Toggle light / dark theme", "", |state| {
            state.theme.update(|theme| *theme = theme.toggled())
        }),
        row("Toggle backlinks panel", "", |state| {
            state
                .right_panel
                .update(|panel| {
                    *panel = match *panel {
                        RightPanel::Hidden => RightPanel::Backlinks,
                        _ => RightPanel::Hidden,
                    }
                })
        }),
        row("Sign out", "", |state| {
            spawn_local(async move {
                match api::logout().await {
                    Ok(response) => {
                        // With Authelia, signing out here should also end the
                        // session at the provider — otherwise clicking "sign in"
                        // silently logs straight back in.
                        let destination = response.redirect_to.unwrap_or_else(|| "/".to_string());
                        if let Some(window) = web_sys::window() {
                            let _ = window.location().set_href(&destination);
                        }
                    }
                    Err(err) => state.error(err.user_message()),
                }
            })
        }),
    ]
}
