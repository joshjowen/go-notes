//! A small async wrapper over IndexedDB.
//!
//! IndexedDB rather than `localStorage` for two reasons: notes are not small,
//! and `localStorage` is both synchronous — so writing a note would block
//! rendering — and capped at a few megabytes per origin, which a vault reaches
//! quickly. IndexedDB is asynchronous and has a quota measured in a fraction of
//! the disk.
//!
//! The awkward part of IndexedDB from Rust is that every operation is an
//! `IdbRequest` with `onsuccess`/`onerror` callbacks. Each request here is
//! wrapped in a `Promise` and awaited, with the callbacks owned by the async
//! function so they are dropped *after* the event has been delivered — dropping
//! a closure from inside its own invocation would free the environment
//! JavaScript is still standing in.

use js_sys::Promise;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{IdbDatabase, IdbObjectStore, IdbOpenDbRequest, IdbRequest, IdbTransactionMode};

/// Bumping this runs `onupgradeneeded`, which creates any missing stores.
const DB_VERSION: u32 = 1;
const DB_NAME: &str = "go-notes-offline";

/// Notes cached for reading and editing offline, keyed by vault path.
pub const STORE_NOTES: &str = "notes";
/// Everything else: the file tree, the signed-in identity, the outbox.
pub const STORE_META: &str = "meta";

const STORES: [&str; 2] = [STORE_NOTES, STORE_META];

/// An open database handle.
#[derive(Clone)]
pub struct Db {
    inner: IdbDatabase,
}

impl Db {
    /// Opens the database, creating the object stores on first use.
    pub async fn open() -> Result<Db, JsValue> {
        let factory = web_sys::window()
            .and_then(|window| window.indexed_db().ok().flatten())
            .ok_or_else(|| JsValue::from_str("this browser has no IndexedDB"))?;

        let request: IdbOpenDbRequest = factory.open_with_u32(DB_NAME, DB_VERSION)?;

        // Held until the open request settles: the upgrade runs first, and
        // dropping the closure before then would leave IndexedDB calling into
        // freed memory.
        let upgrade = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
            let Some(target) = event.target() else { return };
            let Ok(request) = target.dyn_into::<IdbRequest>() else {
                return;
            };
            let Ok(result) = request.result() else { return };
            let Ok(db) = result.dyn_into::<IdbDatabase>() else {
                return;
            };
            for store in STORES {
                if !db.object_store_names().contains(store) {
                    let _ = db.create_object_store(store);
                }
            }
        });
        request.set_onupgradeneeded(Some(upgrade.as_ref().unchecked_ref()));

        let result = settle(request.as_ref()).await;
        request.set_onupgradeneeded(None);
        drop(upgrade);

        Ok(Db {
            inner: result?.dyn_into::<IdbDatabase>()?,
        })
    }

    pub async fn get(&self, store: &str, key: &str) -> Result<Option<JsValue>, JsValue> {
        let object_store = self.store(store, IdbTransactionMode::Readonly)?;
        let value = settle(&object_store.get(&JsValue::from_str(key))?).await?;
        Ok(if value.is_undefined() || value.is_null() {
            None
        } else {
            Some(value)
        })
    }

    pub async fn put(&self, store: &str, key: &str, value: &JsValue) -> Result<(), JsValue> {
        let object_store = self.store(store, IdbTransactionMode::Readwrite)?;
        settle(&object_store.put_with_key(value, &JsValue::from_str(key))?).await?;
        Ok(())
    }

    pub async fn delete(&self, store: &str, key: &str) -> Result<(), JsValue> {
        let object_store = self.store(store, IdbTransactionMode::Readwrite)?;
        settle(&object_store.delete(&JsValue::from_str(key))?).await?;
        Ok(())
    }

    pub async fn all(&self, store: &str) -> Result<Vec<JsValue>, JsValue> {
        let object_store = self.store(store, IdbTransactionMode::Readonly)?;
        let values = settle(&object_store.get_all()?).await?;
        Ok(js_sys::Array::from(&values).to_vec())
    }

    pub async fn clear(&self, store: &str) -> Result<(), JsValue> {
        let object_store = self.store(store, IdbTransactionMode::Readwrite)?;
        settle(&object_store.clear()?).await?;
        Ok(())
    }

    fn store(&self, name: &str, mode: IdbTransactionMode) -> Result<IdbObjectStore, JsValue> {
        self.inner
            .transaction_with_str_and_mode(name, mode)?
            .object_store(name)
    }
}

/// Awaits one IndexedDB request.
async fn settle(request: &IdbRequest) -> Result<JsValue, JsValue> {
    let mut on_success: Option<Closure<dyn FnMut(web_sys::Event)>> = None;
    let mut on_error: Option<Closure<dyn FnMut(web_sys::Event)>> = None;

    let promise = Promise::new(&mut |resolve: js_sys::Function, reject: js_sys::Function| {
        let success = {
            let request = request.clone();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
                let value = request.result().unwrap_or(JsValue::UNDEFINED);
                let _ = resolve.call1(&JsValue::NULL, &value);
            })
        };
        let failure = {
            let request = request.clone();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
                let reason = request
                    .error()
                    .ok()
                    .flatten()
                    .map(JsValue::from)
                    .unwrap_or_else(|| JsValue::from_str("the IndexedDB request failed"));
                let _ = reject.call1(&JsValue::NULL, &reason);
            })
        };

        request.set_onsuccess(Some(success.as_ref().unchecked_ref()));
        request.set_onerror(Some(failure.as_ref().unchecked_ref()));
        on_success = Some(success);
        on_error = Some(failure);
    });

    let outcome = JsFuture::from(promise).await;

    // Detached and dropped only now, once the event that resolved the promise
    // has been fully delivered.
    request.set_onsuccess(None);
    request.set_onerror(None);
    drop(on_success);
    drop(on_error);

    outcome
}
