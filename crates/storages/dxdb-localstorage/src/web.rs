//! The browser binding: a [`Kv`] over `window.localStorage`.
//!
//! Thin on purpose. All the storage logic is platform-independent and lives in
//! [`crate`]'s `LocalStorageInstance`; this file only turns `web_sys::Storage` into
//! the four `Kv` methods, so the code the browser runs is the code the host tests
//! exercise.

use std::rc::Rc;

use wasm_bindgen::prelude::*;

use crate::{Kv, KvError, LocalStorageStorage};

/// `window.localStorage`, as a `Kv`.
pub struct WebStorage {
    storage: web_sys::Storage,
}

impl WebStorage {
    /// Reads the browser's `localStorage`.
    ///
    /// Errors rather than panics when unavailable, because "the global exists but
    /// touching it throws" is a real, supported browser state (Safari in a
    /// partitioned or blocked-cookie context) and the demo harness probes it.
    pub fn open() -> Result<Self, KvError> {
        let window = web_sys::window()
            .ok_or_else(|| KvError::Unavailable("no window object".to_string()))?;
        let storage = window
            .local_storage()
            .map_err(|e| KvError::Unavailable(format!("localStorage threw on access: {e:?}")))?
            .ok_or_else(|| KvError::Unavailable("localStorage is null".to_string()))?;
        Ok(Self { storage })
    }
}

/// `localStorage` reports a full quota as a `DOMException` named
/// `QuotaExceededError` (older engines: `NS_ERROR_DOM_QUOTA_REACHED`). Both are
/// recognised so the caller gets a typed error rather than an opaque string.
fn classify(err: JsValue) -> KvError {
    let name = js_sys::Reflect::get(&err, &JsValue::from_str("name"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    if name == "QuotaExceededError" || name == "NS_ERROR_DOM_QUOTA_REACHED" {
        KvError::QuotaExceeded
    } else {
        KvError::Other(format!("{name}: {err:?}"))
    }
}

impl Kv for WebStorage {
    fn get_item(&self, key: &str) -> Result<Option<String>, KvError> {
        self.storage.get_item(key).map_err(classify)
    }

    fn set_item(&self, key: &str, value: &str) -> Result<(), KvError> {
        self.storage.set_item(key, value).map_err(classify)
    }

    fn remove_item(&self, key: &str) -> Result<(), KvError> {
        self.storage.remove_item(key).map_err(classify)
    }

    fn keys(&self) -> Result<Vec<String>, KvError> {
        // `localStorage` has no key iterator, only `length` + `key(i)`. Keys can be
        // added or removed while iterating from another tab, so a `None` mid-walk is
        // expected and means "that slot went away" — skip it rather than failing.
        let length = self.storage.length().map_err(classify)?;
        let mut keys = Vec::with_capacity(length as usize);
        for i in 0..length {
            match self.storage.key(i) {
                Ok(Some(key)) => keys.push(key),
                Ok(None) => continue,
                Err(e) => return Err(classify(e)),
            }
        }
        Ok(keys)
    }
}

/// Opens the storage for JS callers.
///
/// Returns a handle the `dxdb-js` layer wraps; the internal `Rc` is why this is
/// not `Send` and why wasm32's single thread is the right assumption (PLAN.md 3.4).
#[wasm_bindgen]
pub fn open_localstorage() -> Result<LocalStorageHandle, JsValue> {
    let kv = WebStorage::open().map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(LocalStorageHandle {
        storage: LocalStorageStorage::new(Rc::new(kv)),
    })
}

/// The handle JS holds. Opaque for now: the `dxdb-js` database surface arrives in
/// the next tranche, and exporting a half-built one would be worse than exporting
/// none.
#[wasm_bindgen]
pub struct LocalStorageHandle {
    #[allow(dead_code)]
    storage: LocalStorageStorage<WebStorage>,
}

#[wasm_bindgen]
impl LocalStorageHandle {
    /// The storage name, so a caller can assert which backend it got.
    #[wasm_bindgen(getter)]
    pub fn name(&self) -> String {
        crate::STORAGE_NAME.to_string()
    }
}
