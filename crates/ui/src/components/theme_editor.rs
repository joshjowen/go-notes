//! The theme picker and colour editor, opened from the top bar or the command
//! palette.
//!
//! Three themes ship built in — Default Dark, Default Light, and Kanagawa
//! Bones — chosen as one-click starting points. Dragging any colour picker
//! copies whichever theme is active into "Custom" and edits that instead, so
//! customizing never means starting from a blank slate. A raw-CSS box at the
//! bottom covers whatever the fifteen pickers cannot express, for anyone who
//! wants it — but it is opt-in, not the only way in.

use leptos::prelude::*;

use crate::state::{use_app, AppState};
use crate::theme::{active_colors, ThemeColors, ThemeId};

#[component]
pub fn ThemeEditor() -> impl IntoView {
    let state = use_app();

    view! {
        <Show when=move || state.theme_dialog_open.get()>
            <div class="gn-overlay" on:mousedown=move |_| state.theme_dialog_open.set(false)>
                <div class="gn-dialog gn-theme-dialog" on:mousedown=move |ev| ev.stop_propagation()>
                    <h2>"Theme"</h2>
                    <p>
                        "Pick a theme to start from. Changing any colour below switches to a
                        custom theme automatically — the built-ins are left untouched."
                    </p>

                    <div class="gn-theme-grid">
                        {ThemeId::BUILT_IN.into_iter().map(|id| theme_card(state, id)).collect_view()}
                        {theme_card_custom(state)}
                    </div>

                    <div class="gn-theme-section-title">"Customize"</div>
                    {move || {
                        let colors = active_colors(&state);
                        view! {
                            <div class="gn-theme-fields">
                                {color_row(state, "Background", &colors, |c| &c.bg, |c, v| c.bg = v)}
                                {color_row(
                                    state,
                                    "Sidebar background",
                                    &colors,
                                    |c| &c.bg_secondary,
                                    |c, v| c.bg_secondary = v,
                                )}
                                {color_row(
                                    state,
                                    "Panel background",
                                    &colors,
                                    |c| &c.bg_tertiary,
                                    |c, v| c.bg_tertiary = v,
                                )}
                                {color_row(
                                    state,
                                    "Popup background",
                                    &colors,
                                    |c| &c.bg_float,
                                    |c, v| c.bg_float = v,
                                )}
                                {color_row(state, "Text", &colors, |c| &c.text, |c, v| c.text = v)}
                                {color_row(
                                    state,
                                    "Muted text",
                                    &colors,
                                    |c| &c.text_muted,
                                    |c, v| c.text_muted = v,
                                )}
                                {color_row(
                                    state,
                                    "Faint text",
                                    &colors,
                                    |c| &c.text_faint,
                                    |c, v| c.text_faint = v,
                                )}
                                {color_row(
                                    state,
                                    "Headings",
                                    &colors,
                                    |c| &c.heading,
                                    |c, v| c.heading = v,
                                )}
                                {color_row(state, "Border", &colors, |c| &c.border, |c, v| c.border = v)}
                                {color_row(
                                    state,
                                    "Strong border",
                                    &colors,
                                    |c| &c.border_strong,
                                    |c, v| c.border_strong = v,
                                )}
                                {color_row(state, "Accent", &colors, |c| &c.accent, |c, v| c.accent = v)}
                                {color_row(
                                    state,
                                    "Accent (hover)",
                                    &colors,
                                    |c| &c.accent_hover,
                                    |c, v| c.accent_hover = v,
                                )}
                                {color_row(
                                    state,
                                    "Unresolved link",
                                    &colors,
                                    |c| &c.unresolved,
                                    |c, v| c.unresolved = v,
                                )}
                                {color_row(state, "Danger", &colors, |c| &c.danger, |c, v| c.danger = v)}
                                {color_row(state, "Success", &colors, |c| &c.success, |c, v| c.success = v)}
                            </div>

                            <label class="gn-theme-dark-toggle">
                                <input
                                    type="checkbox"
                                    prop:checked=colors.dark
                                    on:change=move |ev| {
                                        let mut edited = active_colors(&state);
                                        edited.dark = event_target_checked(&ev);
                                        apply_custom_edit(state, edited);
                                    }
                                />
                                <span>"Dark base (hover and selection contrast follow this)"</span>
                            </label>
                        }
                    }}

                    <details class="gn-theme-advanced">
                        <summary>"Custom CSS"</summary>
                        <p class="gn-theme-advanced-hint">
                            "Applied on top of everything above, for anything the colour pickers
                            do not cover."
                        </p>
                        <textarea
                            class="gn-theme-css-input"
                            spellcheck="false"
                            placeholder=".gn-sidebar { border-radius: 8px; }"
                            prop:value=move || state.custom_css.get()
                            on:input=move |ev| state.custom_css.set(event_target_value(&ev))
                        ></textarea>
                    </details>

                    <div class="gn-dialog-actions">
                        <button on:click=move |_| {
                            state.custom_colors.set(ThemeColors::default_dark());
                            state.theme_id.set(ThemeId::DefaultDark);
                        }>"Reset colours"</button>
                        <button on:click=move |_| state.theme_dialog_open.set(false)>"Done"</button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

fn theme_card(state: AppState, id: ThemeId) -> impl IntoView {
    let colors = id.colors().expect("BUILT_IN never contains Custom");
    let active = move || state.theme_id.get() == id;

    view! {
        <button class="gn-theme-card" class:gn-active=active on:click=move |_| state.theme_id.set(id)>
            {swatch(&colors)}
            <span class="gn-theme-card-name">{id.name()}</span>
        </button>
    }
}

fn theme_card_custom(state: AppState) -> impl IntoView {
    let active = move || state.theme_id.get() == ThemeId::Custom;

    view! {
        <button
            class="gn-theme-card"
            class:gn-active=active
            on:click=move |_| state.theme_id.set(ThemeId::Custom)
        >
            {move || swatch(&state.custom_colors.get())}
            <span class="gn-theme-card-name">"Custom"</span>
        </button>
    }
}

fn swatch(colors: &ThemeColors) -> impl IntoView {
    let background = format!("background:{}; border-color:{};", colors.bg, colors.border);
    let accent_dot = format!("background:{};", colors.accent);
    let text_dot = format!("background:{};", colors.text);
    view! {
        <span class="gn-theme-swatch" style=background>
            <span class="gn-theme-swatch-dot" style=accent_dot></span>
            <span class="gn-theme-swatch-dot" style=text_dot></span>
        </span>
    }
}

/// One labelled colour picker. `get` reads the field's current value out of
/// whatever colours are active; `set` writes an edited value back before the
/// result is applied. Both are plain function pointers (no captures), which
/// keeps each call site to a single line naming the field once.
fn color_row(
    state: AppState,
    label: &'static str,
    colors: &ThemeColors,
    get: fn(&ThemeColors) -> &String,
    set: fn(&mut ThemeColors, String),
) -> impl IntoView {
    let value = get(colors).clone();
    view! {
        <label class="gn-theme-field">
            <input
                type="color"
                prop:value=value
                on:input=move |ev| {
                    let mut edited = active_colors(&state);
                    set(&mut edited, event_target_value(&ev));
                    apply_custom_edit(state, edited);
                }
            />
            <span>{label}</span>
        </label>
    }
}

/// Writes an edited colour set as the custom theme, switching the active
/// theme to `Custom` if something else was selected.
fn apply_custom_edit(state: AppState, colors: ThemeColors) {
    state.custom_colors.set(colors);
    if state.theme_id.get_untracked() != ThemeId::Custom {
        state.theme_id.set(ThemeId::Custom);
    }
}
