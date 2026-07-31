//! go-notes — the Leptos frontend.
//!
//! Everything here compiles to WebAssembly. The only JavaScript in the project
//! is the Milkdown bridge in `editor/`, reached through `editor.rs`.

mod api;
mod app;
mod components;
mod editor;
mod state;

fn main() {
    // Turns a Rust panic into a readable browser console trace instead of the
    // bare "unreachable executed" that WebAssembly would otherwise produce.
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}
