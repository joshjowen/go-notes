//! The Rust side of the editor bridge.
//!
//! `window.GoNotesEditor` is defined by `editor-bridge.js`, built from the
//! TypeScript in `editor/`. Reaching it through `js_namespace` rather than as an
//! ES module keeps the interop to a single extern block: plain strings in, plain
//! strings out, with callbacks passed as closures.

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    /// Creates an editor inside `element`. Resolves to a handle id, or -1.
    #[wasm_bindgen(js_namespace = GoNotesEditor, js_name = mount)]
    fn js_mount(element: &web_sys::HtmlElement, options: &JsValue) -> js_sys::Promise;

    #[wasm_bindgen(js_namespace = GoNotesEditor, js_name = getMarkdown)]
    fn js_get_markdown(id: i32) -> String;

    #[wasm_bindgen(js_namespace = GoNotesEditor, js_name = setMarkdown)]
    fn js_set_markdown(id: i32, markdown: &str) -> js_sys::Promise;

    /// Replaces the document in place, preserving the selection — unlike
    /// `setMarkdown`, which rebuilds the editor. Used when text arrives from
    /// outside while someone may still be typing: a merged save, a periodic
    /// background refresh of the open note.
    #[wasm_bindgen(js_namespace = GoNotesEditor, js_name = patchMarkdown)]
    fn js_patch_markdown(id: i32, markdown: &str) -> js_sys::Promise;

    #[wasm_bindgen(js_namespace = GoNotesEditor, js_name = setMode)]
    fn js_set_mode(id: i32, mode: &str) -> js_sys::Promise;

    #[wasm_bindgen(js_namespace = GoNotesEditor, js_name = setKnownTargets)]
    fn js_set_known_targets(id: i32, targets: JsValue);

    #[wasm_bindgen(js_namespace = GoNotesEditor, js_name = insertMarkdown)]
    fn js_insert_markdown(id: i32, snippet: &str) -> js_sys::Promise;

    #[wasm_bindgen(js_namespace = GoNotesEditor, js_name = focus)]
    fn js_focus(id: i32);

    #[wasm_bindgen(js_namespace = GoNotesEditor, js_name = destroy)]
    fn js_destroy(id: i32) -> js_sys::Promise;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorMode {
    Wysiwyg,
    Source,
}

impl EditorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            EditorMode::Wysiwyg => "wysiwyg",
            EditorMode::Source => "source",
        }
    }

    pub fn toggled(self) -> EditorMode {
        match self {
            EditorMode::Wysiwyg => EditorMode::Source,
            EditorMode::Source => EditorMode::Wysiwyg,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EditorMode::Wysiwyg => "Rich text",
            EditorMode::Source => "Markdown",
        }
    }
}

/// The closures handed to JavaScript, kept alive for as long as the editor is.
///
/// Dropping a `Closure` invalidates the function pointer JavaScript holds, so
/// letting these fall out of scope would make the editor call into freed memory
/// the next time the user typed. They are owned by the handle and dropped with it.
pub struct EditorCallbacks {
    _on_change: Closure<dyn FnMut(String)>,
    _on_query: Closure<dyn FnMut(String) -> js_sys::Promise>,
    _on_open_link: Closure<dyn FnMut(String)>,
    _on_upload: Closure<dyn FnMut(web_sys::File) -> js_sys::Promise>,
}

/// A live editor instance.
pub struct EditorHandle {
    id: i32,
    _callbacks: EditorCallbacks,
}

impl EditorHandle {
    /// The bridge's handle id.
    ///
    /// Exposed because the asynchronous operations are free functions taking
    /// this id rather than methods on `&self`. A method returning a future would
    /// borrow the handle for the future's whole lifetime, and the caller holds
    /// the handle inside a `RefCell` — so awaiting it would mean keeping a
    /// `Ref` alive across an await point, which cannot compile and would be a
    /// borrow panic waiting to happen if it did.
    pub fn id(&self) -> i32 {
        self.id
    }

    pub fn markdown(&self) -> String {
        js_get_markdown(self.id)
    }

    /// Tells the editor which link targets exist, so unresolved ones restyle.
    pub fn set_known_targets(&self, targets: &[String]) {
        let array = js_sys::Array::new();
        for target in targets {
            array.push(&JsValue::from_str(target));
        }
        js_set_known_targets(self.id, array.into());
    }

    pub fn focus(&self) {
        js_focus(self.id);
    }
}

impl Drop for EditorHandle {
    fn drop(&mut self) {
        // Fire-and-forget: the promise only completes after Milkdown has torn
        // down, and there is nothing useful to do with the result here.
        let _ = js_destroy(self.id);
    }
}

/// Replaces the editor's document.
pub async fn set_markdown(id: i32, markdown: &str) {
    let _ = wasm_bindgen_futures::JsFuture::from(js_set_markdown(id, markdown)).await;
}

/// Replaces the editor's document without losing the caret, for text that
/// arrives while the note may still be open and being typed into.
pub async fn patch_markdown(id: i32, markdown: &str) {
    let _ = wasm_bindgen_futures::JsFuture::from(js_patch_markdown(id, markdown)).await;
}

/// Switches between rich text and raw markdown.
pub async fn set_mode(id: i32, mode: EditorMode) {
    let _ = wasm_bindgen_futures::JsFuture::from(js_set_mode(id, mode.as_str())).await;
}

/// Inserts markdown at the cursor, used by the attachment drop handler.
pub async fn insert_markdown(id: i32, snippet: &str) {
    let _ = wasm_bindgen_futures::JsFuture::from(js_insert_markdown(id, snippet)).await;
}

/// What the host provides when creating an editor.
pub struct EditorConfig<C, Q, O, U>
where
    C: FnMut(String) + 'static,
    Q: FnMut(String) -> js_sys::Promise + 'static,
    O: FnMut(String) + 'static,
    U: FnMut(web_sys::File) -> js_sys::Promise + 'static,
{
    pub markdown: String,
    pub mode: EditorMode,
    pub known_targets: Vec<String>,
    pub on_change: C,
    pub on_wikilink_query: Q,
    pub on_open_link: O,
    pub on_upload: U,
}

/// Creates an editor and returns a handle that owns its callbacks.
pub async fn mount<C, Q, O, U>(
    element: &web_sys::HtmlElement,
    config: EditorConfig<C, Q, O, U>,
) -> Option<EditorHandle>
where
    C: FnMut(String) + 'static,
    Q: FnMut(String) -> js_sys::Promise + 'static,
    O: FnMut(String) + 'static,
    U: FnMut(web_sys::File) -> js_sys::Promise + 'static,
{
    let on_change = Closure::new(config.on_change);
    let on_query = Closure::new(config.on_wikilink_query);
    let on_open_link = Closure::new(config.on_open_link);
    let on_upload = Closure::new(config.on_upload);

    let options = js_sys::Object::new();
    let set = |key: &str, value: &JsValue| {
        let _ = js_sys::Reflect::set(&options, &JsValue::from_str(key), value);
    };

    set("markdown", &JsValue::from_str(&config.markdown));
    set("mode", &JsValue::from_str(config.mode.as_str()));
    set("onChange", on_change.as_ref());
    set("onWikilinkQuery", on_query.as_ref());
    set("onOpenLink", on_open_link.as_ref());
    set("onUploadFile", on_upload.as_ref());

    let targets = js_sys::Array::new();
    for target in &config.known_targets {
        targets.push(&JsValue::from_str(target));
    }
    set("knownTargets", &targets);

    let result = wasm_bindgen_futures::JsFuture::from(js_mount(element, &options)).await;

    let id = match result {
        Ok(value) => value.as_f64().unwrap_or(-1.0) as i32,
        Err(err) => {
            web_sys::console::error_2(&JsValue::from_str("editor failed to mount"), &err);
            return None;
        }
    };
    if id < 0 {
        return None;
    }

    Some(EditorHandle {
        id,
        _callbacks: EditorCallbacks {
            _on_change: on_change,
            _on_query: on_query,
            _on_open_link: on_open_link,
            _on_upload: on_upload,
        },
    })
}

/// Wraps a Rust future as a JavaScript promise resolving to a string.
///
/// Used for the two callbacks the editor awaits — autocomplete lookups and file
/// uploads — so their implementations can be ordinary async Rust.
pub fn promise_from_future<F>(future: F) -> js_sys::Promise
where
    F: std::future::Future<Output = String> + 'static,
{
    wasm_bindgen_futures::future_to_promise(async move { Ok(JsValue::from_str(&future.await)) })
}
