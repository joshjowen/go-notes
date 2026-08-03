//! Installing Go-Notes as an application on the device.
//!
//! Two very different browsers to satisfy. Chromium fires
//! `beforeinstallprompt`, which can be held onto and replayed later from a
//! button of our own — and *must* be, because the event is only useful if
//! `preventDefault` was called on it, and it never fires again. Safari fires
//! nothing at all: on iOS, installing means Share → Add to Home Screen, done by
//! hand, so all the application can usefully do is say so when asked.
//!
//! The result either way is a standalone window with no browser chrome, which
//! matters more here than it looks: `display: standalone` is what stops a phone
//! keyboard from fighting the URL bar for the bottom of the screen.

use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::state::AppState;

thread_local! {
    /// The deferred `beforeinstallprompt` event. Held because the browser only
    /// offers it once, and the moment it arrives is never the moment the user
    /// wants to be asked.
    static DEFERRED: RefCell<Option<JsValue>> = const { RefCell::new(None) };
}

/// Starts listening for the browser's install offer.
pub fn watch(state: AppState) {
    let Some(window) = web_sys::window() else {
        return;
    };

    let on_prompt = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        // Suppresses Chromium's own mini-infobar, which is the price of being
        // allowed to trigger the prompt ourselves later.
        event.prevent_default();
        let value: JsValue = event.into();
        DEFERRED.with(|deferred| *deferred.borrow_mut() = Some(value));
        state.installable.set(true);
    });
    let _ = window
        .add_event_listener_with_callback("beforeinstallprompt", on_prompt.as_ref().unchecked_ref());
    on_prompt.forget();

    let on_installed = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        DEFERRED.with(|deferred| *deferred.borrow_mut() = None);
        state.installable.set(false);
        state.notify("Go-Notes is installed. It opens like any other app now.");
    });
    let _ =
        window.add_event_listener_with_callback("appinstalled", on_installed.as_ref().unchecked_ref());
    on_installed.forget();
}

/// Asks the browser to install, or explains how to do it by hand where there is
/// nothing to ask.
pub fn install(state: AppState) {
    if is_standalone() {
        state.notify("Go-Notes is already installed — this is the installed app.");
        return;
    }

    let deferred = DEFERRED.with(|deferred| deferred.borrow().clone());

    let Some(event) = deferred else {
        state.notify(manual_instructions());
        return;
    };

    // `prompt()` is a method on the event object, which web-sys has no binding
    // for — `BeforeInstallPromptEvent` is not in any standard. Reflect is the
    // honest way to call it rather than pretending the type exists.
    let prompt = js_sys::Reflect::get(&event, &JsValue::from_str("prompt"))
        .ok()
        .and_then(|value| value.dyn_into::<js_sys::Function>().ok());

    match prompt {
        Some(prompt) => {
            let _ = prompt.call0(&event);
            // Spent either way: accepted, and it will not fire again; dismissed,
            // and Chromium refuses to show it again for a while regardless.
            DEFERRED.with(|deferred| *deferred.borrow_mut() = None);
            state.installable.set(false);
        }
        None => state.notify(manual_instructions()),
    }
}

/// What to tell someone whose browser has no install prompt to offer.
fn manual_instructions() -> &'static str {
    if is_ios() {
        "To install: tap Share, then “Add to Home Screen”."
    } else {
        "To install: use your browser's menu — “Install app” or “Add to Home screen”."
    }
}

/// Already running as an installed app, rather than in a browser tab.
pub fn is_standalone() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };

    let by_display_mode = window
        .match_media("(display-mode: standalone)")
        .ok()
        .flatten()
        .is_some_and(|query| query.matches());

    // iOS predates the display-mode query and answers with its own property.
    let by_navigator = js_sys::Reflect::get(&window.navigator(), &JsValue::from_str("standalone"))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    by_display_mode || by_navigator
}

fn is_ios() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let agent = window.navigator().user_agent().unwrap_or_default();
    // An iPad reports itself as a Mac; the touch point count is what separates
    // it from a desktop, and it is the one place a user-agent sniff is still
    // the least-bad option — no feature detects "this is where Add to Home
    // Screen lives".
    agent.contains("iPhone")
        || agent.contains("iPad")
        || agent.contains("iPod")
        || (agent.contains("Macintosh") && window.navigator().max_touch_points() > 1)
}
