//! Theme definitions, DOM application, and persistence.
//!
//! A theme is a flat set of colours mapped onto the `--gn-*` custom properties
//! declared in `styles.css`. Several are built in; one more, "Custom", is
//! whatever the user has picked in the theme editor and lives in
//! `localStorage` rather than being compiled in. Applying a theme means
//! setting those properties as inline styles on `<html>` — inline style beats
//! any selector in the stylesheet, so it overrides the `:root` and
//! `:root[data-theme="light"]` defaults without needing per-theme CSS.
//!
//! `data-theme` itself is kept for the handful of things a colour picker
//! should not have to reproduce per theme — the hover/selected/code overlays
//! (mixed from white or black depending on `dark`), the shadow, and
//! `color-scheme` for native form controls and scrollbars.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;

use crate::state::AppState;

const THEME_ID_KEY: &str = "go-notes-theme-id";
const CUSTOM_COLORS_KEY: &str = "go-notes-custom-colors";
const CUSTOM_CSS_KEY: &str = "go-notes-custom-css";
/// Pre-dates the theme editor; a plain "light"/"dark" choice with no room for
/// any other theme. Read once, on first load, so upgrading does not reset
/// anyone's preference.
const LEGACY_THEME_KEY: &str = "go-notes-theme";
const CUSTOM_CSS_ELEMENT_ID: &str = "gn-custom-css";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeId {
    DefaultDark,
    DefaultLight,
    KanagawaBones,
    RosePine,
    Everforest,
    Nightfox,
    Custom,
}

impl ThemeId {
    pub const BUILT_IN: [ThemeId; 6] = [
        ThemeId::DefaultDark,
        ThemeId::DefaultLight,
        ThemeId::KanagawaBones,
        ThemeId::RosePine,
        ThemeId::Everforest,
        ThemeId::Nightfox,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ThemeId::DefaultDark => "default-dark",
            ThemeId::DefaultLight => "default-light",
            ThemeId::KanagawaBones => "kanagawabones",
            ThemeId::RosePine => "rose-pine",
            ThemeId::Everforest => "everforest",
            ThemeId::Nightfox => "nightfox",
            ThemeId::Custom => "custom",
        }
    }

    pub fn from_str(value: &str) -> Option<ThemeId> {
        match value {
            "default-dark" => Some(ThemeId::DefaultDark),
            "default-light" => Some(ThemeId::DefaultLight),
            "kanagawabones" => Some(ThemeId::KanagawaBones),
            "rose-pine" => Some(ThemeId::RosePine),
            "everforest" => Some(ThemeId::Everforest),
            "nightfox" => Some(ThemeId::Nightfox),
            "custom" => Some(ThemeId::Custom),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ThemeId::DefaultDark => "Default Dark",
            ThemeId::DefaultLight => "Default Light",
            ThemeId::KanagawaBones => "Kanagawa Bones",
            // Each of these ships several variants upstream; the name says which
            // one this is, so picking a different variant later can be a new
            // entry rather than a silent change under someone's feet.
            ThemeId::RosePine => "Rosé Pine",
            ThemeId::Everforest => "Everforest Dark",
            ThemeId::Nightfox => "Nightfox",
            ThemeId::Custom => "Custom",
        }
    }

    /// The colours a built-in theme starts from. `Custom` has none of its
    /// own — it is whatever is stored in `AppState::custom_colors`.
    pub fn colors(self) -> Option<ThemeColors> {
        Some(match self {
            ThemeId::DefaultDark => ThemeColors::default_dark(),
            ThemeId::DefaultLight => ThemeColors::default_light(),
            ThemeId::KanagawaBones => ThemeColors::kanagawabones(),
            ThemeId::RosePine => ThemeColors::rose_pine(),
            ThemeId::Everforest => ThemeColors::everforest(),
            ThemeId::Nightfox => ThemeColors::nightfox(),
            ThemeId::Custom => return None,
        })
    }
}

/// The subset of `--gn-*` variables a person can usefully change by hand.
///
/// The rest — hover/selected/code overlays, the shadow — are derived from
/// `dark` in CSS, the same way the original light/dark toggle worked.
/// Exposing all of them as pickers would be a wall of controls for
/// variations nobody asks for; someone who wants that level of control has
/// the custom CSS box.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeColors {
    /// Picks the hover/selected/code overlay direction and the native
    /// `color-scheme`, so a custom theme with a dark background still gets
    /// light-on-dark overlays instead of the light theme's dark-on-light ones.
    pub dark: bool,
    pub bg: String,
    pub bg_secondary: String,
    pub bg_tertiary: String,
    pub bg_float: String,
    pub text: String,
    pub text_muted: String,
    pub text_faint: String,
    pub heading: String,
    pub border: String,
    pub border_strong: String,
    pub accent: String,
    pub accent_hover: String,
    pub unresolved: String,
    pub danger: String,
    pub success: String,
}

impl ThemeColors {
    /// Mirrors the `:root` block in `styles.css`.
    pub fn default_dark() -> ThemeColors {
        ThemeColors {
            dark: true,
            bg: "#1e1e1e".into(),
            bg_secondary: "#262626".into(),
            bg_tertiary: "#2a2a2a".into(),
            bg_float: "#2d2d2d".into(),
            text: "#dcddde".into(),
            text_muted: "#b3b3b3".into(),
            text_faint: "#7a7a7a".into(),
            heading: "#ffffff".into(),
            border: "#3a3a3a".into(),
            border_strong: "#4a4a4a".into(),
            accent: "#7f6df2".into(),
            accent_hover: "#9382f5".into(),
            unresolved: "#d9707a".into(),
            danger: "#e05252".into(),
            success: "#4caf7d".into(),
        }
    }

    /// Mirrors the `:root[data-theme="light"]` block in `styles.css`.
    pub fn default_light() -> ThemeColors {
        ThemeColors {
            dark: false,
            bg: "#ffffff".into(),
            bg_secondary: "#f5f6f8".into(),
            bg_tertiary: "#ebedf0".into(),
            bg_float: "#ffffff".into(),
            text: "#2e3338".into(),
            text_muted: "#5c6570".into(),
            text_faint: "#8e99a4".into(),
            heading: "#16191c".into(),
            border: "#e0e3e7".into(),
            border_strong: "#c8ccd2".into(),
            accent: "#5a48d6".into(),
            accent_hover: "#4a3ab8".into(),
            unresolved: "#c0392b".into(),
            danger: "#e05252".into(),
            success: "#4caf7d".into(),
        }
    }

    /// Kanagawa Bones — the zenbones.nvim variant built on rebelot/kanagawa's
    /// palette (`sumiInk3` background, `oniViolet` accent, etc).
    pub fn kanagawabones() -> ThemeColors {
        ThemeColors {
            dark: true,
            bg: "#1f1f28".into(),
            bg_secondary: "#252530".into(),
            bg_tertiary: "#2a2a37".into(),
            bg_float: "#252530".into(),
            text: "#ddd8bb".into(),
            text_muted: "#a8a48d".into(),
            text_faint: "#727169".into(),
            heading: "#e6e0c2".into(),
            border: "#363646".into(),
            border_strong: "#54546d".into(),
            accent: "#957fb8".into(),
            accent_hover: "#a98fd2".into(),
            unresolved: "#e46a78".into(),
            danger: "#e46a78".into(),
            success: "#98bc6d".into(),
        }
    }

    /// Rosé Pine, the `main` (dark) variant — `base` through `overlay` for the
    /// surfaces, `iris` for the accent.
    ///
    /// The palette has no neutral brighter than `text`, so headings take `rose`
    /// rather than a lighter grey. That is how the upstream editor themes
    /// colour markdown headings too, and it keeps the theme recognisable.
    pub fn rose_pine() -> ThemeColors {
        ThemeColors {
            dark: true,
            bg: "#191724".into(), // base
            bg_secondary: "#1f1d2e".into(), // surface
            bg_tertiary: "#26233a".into(), // overlay
            bg_float: "#1f1d2e".into(), // surface
            text: "#e0def4".into(), // text
            text_muted: "#908caa".into(), // subtle
            text_faint: "#6e6a86".into(), // muted
            heading: "#ebbcba".into(), // rose
            border: "#403d52".into(), // highlight med
            border_strong: "#524f67".into(), // highlight high
            accent: "#c4a7e7".into(), // iris
            accent_hover: "#d5bef0".into(), // iris, lifted
            unresolved: "#eb6f92".into(), // love
            danger: "#eb6f92".into(), // love
            success: "#9ccfd8".into(), // foam — the palette has no green
        }
    }

    /// Everforest, the dark/medium variant — `bg0`–`bg5` for the surfaces,
    /// `green` for the accent and `yellow` for headings.
    pub fn everforest() -> ThemeColors {
        ThemeColors {
            dark: true,
            bg: "#2d353b".into(), // bg0
            bg_secondary: "#343f44".into(), // bg1
            bg_tertiary: "#3d484d".into(), // bg2
            bg_float: "#343f44".into(), // bg1
            text: "#d3c6aa".into(), // fg
            text_muted: "#9da9a0".into(), // grey2
            text_faint: "#7a8478".into(), // grey0
            heading: "#dbbc7f".into(), // yellow
            border: "#475258".into(), // bg3
            border_strong: "#56635f".into(), // bg5
            accent: "#a7c080".into(), // green
            accent_hover: "#bcd398".into(), // green, lifted
            unresolved: "#e67e80".into(), // red
            danger: "#e67e80".into(), // red
            success: "#83c092".into(), // aqua
        }
    }

    /// Nightfox, the namesake variant of nightfox.nvim — `bg1`–`bg3` for the
    /// surfaces, `blue` for the accent. This palette does carry a neutral
    /// brighter than the body text, so headings use it rather than an accent.
    pub fn nightfox() -> ThemeColors {
        ThemeColors {
            dark: true,
            bg: "#192330".into(), // bg1
            bg_secondary: "#212e3f".into(), // bg2
            bg_tertiary: "#29394f".into(), // bg3
            bg_float: "#212e3f".into(), // bg2
            text: "#cdcecf".into(), // fg1
            text_muted: "#aeafb0".into(), // fg2
            text_faint: "#738091".into(), // comment
            heading: "#dfdfe0".into(), // white
            border: "#2b3b51".into(), // sel0
            border_strong: "#3c5372".into(), // sel1
            accent: "#719cd6".into(), // blue
            accent_hover: "#8db4e3".into(), // blue, lifted
            unresolved: "#c94f6d".into(), // red
            danger: "#c94f6d".into(), // red
            success: "#81b29a".into(), // green
        }
    }
}

/// The colours actually in effect: a built-in's own colours, or the stored
/// custom set when `Custom` is selected.
pub fn active_colors(state: &AppState) -> ThemeColors {
    match state.theme_id.get() {
        ThemeId::Custom => state.custom_colors.get(),
        id => id.colors().expect("only Custom lacks fixed colours"),
    }
}

/// Sets `data-theme` and every `--gn-*` custom property as an inline style on
/// `<html>`.
pub fn apply(colors: &ThemeColors) {
    let Some(root) = document_element() else {
        return;
    };
    let _ = root.set_attribute("data-theme", if colors.dark { "dark" } else { "light" });

    let Some(html_element) = root.dyn_ref::<web_sys::HtmlElement>() else {
        return;
    };
    let style = html_element.style();
    let set = |name: &str, value: &str| {
        let _ = style.set_property(name, value);
    };
    set("--gn-bg", &colors.bg);
    set("--gn-bg-secondary", &colors.bg_secondary);
    set("--gn-bg-tertiary", &colors.bg_tertiary);
    set("--gn-bg-float", &colors.bg_float);
    set("--gn-text", &colors.text);
    set("--gn-text-muted", &colors.text_muted);
    set("--gn-text-faint", &colors.text_faint);
    set("--gn-heading", &colors.heading);
    set("--gn-border", &colors.border);
    set("--gn-border-strong", &colors.border_strong);
    set("--gn-accent", &colors.accent);
    set("--gn-accent-hover", &colors.accent_hover);
    set("--gn-unresolved", &colors.unresolved);
    set("--gn-danger", &colors.danger);
    set("--gn-success", &colors.success);
}

/// Injects (or updates) a `<style>` element holding the user's raw CSS, for
/// the handful of things fifteen colour pickers cannot express.
pub fn apply_custom_css(css: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };

    let element = match document.get_element_by_id(CUSTOM_CSS_ELEMENT_ID) {
        Some(existing) => existing,
        None => {
            let Ok(created) = document.create_element("style") else {
                return;
            };
            created.set_id(CUSTOM_CSS_ELEMENT_ID);
            if let Some(head) = document.head() {
                let _ = head.append_child(&created);
            }
            created
        }
    };
    element.set_text_content(Some(css));
}

// ---------------------------------------------------------------------------
// Startup: figure out what was saved last time.
// ---------------------------------------------------------------------------

pub struct SavedTheme {
    pub theme_id: ThemeId,
    pub custom_colors: ThemeColors,
    pub custom_css: String,
}

/// Reads whatever was persisted, falling back to the legacy light/dark key
/// and then to the operating system's preference.
pub fn load_saved() -> SavedTheme {
    let theme_id = local_storage_get(THEME_ID_KEY)
        .as_deref()
        .and_then(ThemeId::from_str)
        .or_else(|| {
            local_storage_get(LEGACY_THEME_KEY).and_then(|value| match value.as_str() {
                "light" => Some(ThemeId::DefaultLight),
                "dark" => Some(ThemeId::DefaultDark),
                _ => None,
            })
        })
        .unwrap_or_else(|| {
            if prefers_light() {
                ThemeId::DefaultLight
            } else {
                ThemeId::DefaultDark
            }
        });

    let custom_colors = local_storage_get(CUSTOM_COLORS_KEY)
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_else(ThemeColors::default_dark);

    let custom_css = local_storage_get(CUSTOM_CSS_KEY).unwrap_or_default();

    SavedTheme {
        theme_id,
        custom_colors,
        custom_css,
    }
}

pub fn save_theme_id(id: ThemeId) {
    local_storage_set(THEME_ID_KEY, id.as_str());
}

pub fn save_custom_colors(colors: &ThemeColors) {
    if let Ok(json) = serde_json::to_string(colors) {
        local_storage_set(CUSTOM_COLORS_KEY, &json);
    }
}

pub fn save_custom_css(css: &str) {
    local_storage_set(CUSTOM_CSS_KEY, css);
}

/// True when the operating system asks for a light colour scheme.
fn prefers_light() -> bool {
    web_sys::window()
        .and_then(|window| {
            window
                .match_media("(prefers-color-scheme: light)")
                .ok()
                .flatten()
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `colors`, `as_str` and `name` are exhaustive `match`es, so the compiler
    /// catches a forgotten arm when a theme is added. `from_str` maps the other
    /// way and is not checked: a missing arm there silently drops someone's
    /// saved theme back to the default on their next visit.
    #[test]
    fn every_built_in_theme_round_trips_through_storage() {
        for id in ThemeId::BUILT_IN {
            assert_eq!(
                ThemeId::from_str(id.as_str()),
                Some(id),
                "{} does not round-trip",
                id.name()
            );
            assert!(id.colors().is_some(), "{} has no colours", id.name());
        }
        assert_eq!(ThemeId::from_str("custom"), Some(ThemeId::Custom));
        assert_eq!(ThemeId::from_str("nonsense"), None);
    }

    /// Two themes sharing a storage key would make one unselectable.
    #[test]
    fn theme_storage_keys_are_distinct() {
        let mut keys: Vec<&str> = ThemeId::BUILT_IN.iter().map(|id| id.as_str()).collect();
        keys.push(ThemeId::Custom.as_str());
        let count = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), count, "duplicate theme id: {keys:?}");
    }
}
