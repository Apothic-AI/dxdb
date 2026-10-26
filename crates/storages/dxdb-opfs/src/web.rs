//! The two real OPFS backends, and how the path is chosen.
//!
//! # The choice, stated once
//!
//! ```text
//! is there a WorkerGlobalScope?           -> no  -> AsyncWritable  (main thread; nothing else exists)
//!                                            yes -> does createSyncAccessHandle exist on a handle?
//!                                                     yes -> SyncAccessHandle  (the fast path)
//!                                                     no  -> AsyncWritable     (a worker without it)
//! ```
//!
//! `createSyncAccessHandle` is only exposed to dedicated workers (PLAN.md 5.4; MDN;
//! upstream's OPFS docs). It is therefore *probed*, not assumed: a worker in a browser
//! that has not shipped it gets the async path and says so, rather than trapping.
//!
//! # Why the probe is a property test rather than a version check
//!
//! A version string would be wrong in both directions — a browser can ship the API
//! behind a flag, and a polyfill can provide it. Asking the prototype is the only
//! answer that reflects what will actually work.

use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::backing::{AccessPath, FileStore, OpfsError};

use super::OpfsStorage;

/// Where OPFS code is running, which decides which API is even reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// A `window`: the main thread. `createSyncAccessHandle` is not available here.
    MainThread,
    /// A `DedicatedWorkerGlobalScope`: the fast path may be available.
    DedicatedWorker,
}

/// Which scope this module is executing in.
///
/// `WorkerGlobalScope` is only defined inside a worker, so its presence is the test.
#[must_use]
pub fn current_scope() -> Scope {
    // A single `Reflect` read: `WorkerGlobalScope` is defined only inside a worker, so
    // its presence is the test, and one lookup is cheaper than probing two shapes.
    if js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("WorkerGlobalScope"))
        .map(|value| !value.is_undefined())
        .unwrap_or(false)
    {
        Scope::DedicatedWorker
    } else {
        Scope::MainThread
    }
}

/// Whether `FileSystemFileHandle.prototype.createSyncAccessHandle` exists right here.
///
/// Probed on the prototype rather than by attempting a call, so the answer costs
/// nothing and creates no file.
#[must_use]
pub fn sync_access_handle_available() -> bool {
    let Some(handle) = js_sys::Reflect::get(
        &js_sys::global(),
        &JsValue::from_str("FileSystemFileHandle"),
    )
    .ok()
    .filter(|value| !value.is_undefined()) else {
        return false;
    };
    let Some(prototype) = js_sys::Reflect::get(&handle, &JsValue::from_str("prototype")).ok()
    else {
        return false;
    };
    js_sys::Reflect::get(&prototype, &JsValue::from_str("createSyncAccessHandle"))
        .map(|value| value.is_function())
        .unwrap_or(false)
}

/// The path this environment will use, before any instance is opened.
///
/// Exposed so a caller can decide what to do *before* creating a database — start a
/// worker, warn the user, or accept the slow path deliberately.
#[must_use]
pub fn detect_path() -> AccessPath {
    match current_scope() {
        Scope::DedicatedWorker if sync_access_handle_available() => AccessPath::SyncAccessHandle,
        _ => AccessPath::AsyncWritable,
    }
}

/// A human-readable report of the environment, for a log line or a support request.
#[must_use]
#[wasm_bindgen]
pub fn path_report() -> String {
    format!(
        "scope={:?} createSyncAccessHandle={} chosen={}",
        current_scope(),
        sync_access_handle_available(),
        detect_path().as_str()
    )
}

// ---------------------------------------------------------------------------
// The OPFS root
// ---------------------------------------------------------------------------

/// `navigator.storage.getDirectory()`, from a window or a worker.
async fn opfs_root() -> Result<web_sys::FileSystemDirectoryHandle, OpfsError> {
    let global = js_sys::global();
    // `navigator` exists on both `Window` and `WorkerGlobalScope`, so this works in
    // either scope without duplicating the call.
    let navigator = js_sys::Reflect::get(&global, &JsValue::from_str("navigator"))
        .ok()
        .filter(|value| !value.is_undefined())
        .ok_or_else(|| OpfsError::Unavailable("no navigator in this scope".to_string()))?;
    let storage = js_sys::Reflect::get(&navigator, &JsValue::from_str("storage"))
        .ok()
        .filter(|value| !value.is_undefined())
        .ok_or_else(|| OpfsError::Unavailable("no navigator.storage".to_string()))?;
    let get_directory = js_sys::Reflect::get(&storage, &JsValue::from_str("getDirectory"))
        .ok()
        .filter(|value| value.is_function())
        .ok_or_else(|| {
            OpfsError::Unavailable("navigator.storage.getDirectory is absent (no OPFS)".to_string())
        })?;
    let function: js_sys::Function = get_directory.unchecked_into();
    let promise = function
        .call0(&storage)
        .map_err(|e| classify(e, "getDirectory"))?;
    let value = JsFuture::from(js_sys::Promise::from(promise))
        .await
        .map_err(|e| classify(e, "getDirectory"))?;
    Ok(value.unchecked_into())
}

/// Splits a key into path segments and returns the parent directory, creating each
/// segment. Keys are already escaped by the storage, so a `/` here is a real separator.
async fn ensure_parent(
    root: &web_sys::FileSystemDirectoryHandle,
    key: &str,
) -> Result<(web_sys::FileSystemDirectoryHandle, String), OpfsError> {
    let segments: Vec<&str> = key.split('/').filter(|s| !s.is_empty()).collect();
    let (name, dirs) = segments
        .split_last()
        .ok_or_else(|| OpfsError::Other("empty key".to_string()))?;
    let mut current = root.clone();
    for segment in dirs {
        current = get_directory_handle(&current, segment).await?;
    }
    Ok((current, (*name).to_string()))
}

async fn get_directory_handle(
    parent: &web_sys::FileSystemDirectoryHandle,
    name: &str,
) -> Result<web_sys::FileSystemDirectoryHandle, OpfsError> {
    let options = web_sys::FileSystemGetDirectoryOptions::new();
    options.set_create(true);
    let promise = parent.get_directory_handle_with_options(name, &options);
    let value = JsFuture::from(promise)
        .await
        .map_err(|e| classify(e, "getDirectoryHandle"))?;
    Ok(value.unchecked_into())
}

async fn get_file_handle(
    parent: &web_sys::FileSystemDirectoryHandle,
    name: &str,
) -> Result<web_sys::FileSystemFileHandle, OpfsError> {
    let options = web_sys::FileSystemGetFileOptions::new();
    options.set_create(true);
    let promise = parent.get_file_handle_with_options(name, &options);
    let value = JsFuture::from(promise)
        .await
        .map_err(|e| classify(e, "getFileHandle"))?;
    Ok(value.unchecked_into())
}

fn classify(error: JsValue, context: &str) -> OpfsError {
    let name = js_sys::Reflect::get(&error, &JsValue::from_str("name"))
        .ok()
        .and_then(|value| value.as_string())
        .unwrap_or_default();
    match name.as_str() {
        "QuotaExceededError" => OpfsError::Quota,
        "NotFoundError" => OpfsError::NotFound(context.to_string()),
        "NotAllowedError" | "SecurityError" => OpfsError::Unavailable(format!("{context}: {name}")),
        _ => OpfsError::Other(format!("{context}: {name} {error:?}")),
    }
}

// ---------------------------------------------------------------------------
// The chosen backend
// ---------------------------------------------------------------------------

/// A real OPFS store using whichever API this environment offers.
///
/// One type rather than two, because the *storage* above it must not care; the path is
/// carried as data ([`FileStore::path`]) so it is still observable.
pub struct WebFiles {
    root: web_sys::FileSystemDirectoryHandle,
    path: AccessPath,
}

impl WebFiles {
    /// Opens the origin's OPFS root and records which API will be used.
    pub async fn open() -> Result<Self, OpfsError> {
        let root = opfs_root().await?;
        Ok(Self {
            root,
            path: detect_path(),
        })
    }

    /// Writes through the async path (`createWritable`), which is the main-thread
    /// fallback and the only option outside a worker.
    async fn write_async(&self, key: &str, bytes: &[u8]) -> Result<(), OpfsError> {
        let (dir, name) = ensure_parent(&self.root, key).await?;
        let handle = get_file_handle(&dir, &name).await?;
        // `createWritable` truncates by default; `keepExistingData` would make a short
        // write leave a tail from the previous contents, which for a JSON document is a
        // corrupt file rather than a stale one.
        let stream: web_sys::FileSystemWritableFileStream =
            JsFuture::from(handle.create_writable())
                .await
                .map_err(|e| classify(e, "createWritable"))?
                .unchecked_into();
        let write_promise = stream
            .write_with_u8_array(bytes)
            .map_err(|e| classify(e, "write"))?;
        JsFuture::from(write_promise)
            .await
            .map_err(|e| classify(e, "write"))?;
        let close_promise = stream.close();
        JsFuture::from(close_promise)
            .await
            .map_err(|e| classify(e, "close"))?;
        Ok(())
    }

    /// Writes through `createSyncAccessHandle`, the worker-only fast path.
    async fn write_sync(&self, key: &str, bytes: &[u8]) -> Result<(), OpfsError> {
        let (dir, name) = ensure_parent(&self.root, key).await?;
        let handle = get_file_handle(&dir, &name).await?;
        let access = access_handle(&handle).await?;
        // `truncate(0)` first: a shorter write over a longer file would otherwise leave
        // the old tail in place.
        access
            .truncate_with_f64(0.0)
            .map_err(|e| classify(e, "truncate"))?;
        access
            .write_with_u8_array(bytes)
            .map_err(|e| classify(e, "write"))?;
        access.flush().map_err(|e| classify(e, "flush"))?;
        access.close();
        Ok(())
    }

    /// Reads through the async path, using `getFile()` + `arrayBuffer()`.
    async fn read_async(&self, key: &str) -> Result<Option<Vec<u8>>, OpfsError> {
        let (dir, name) = ensure_parent(&self.root, key).await?;
        // `create: false` semantics: a missing file is `NotFoundError`, which for a read
        // means "absent" rather than an error.
        // `create: false` semantics: a missing file is `NotFoundError`, which for a read
        // means "absent", not an error.
        let handle: web_sys::FileSystemFileHandle =
            match JsFuture::from(dir.get_file_handle(&name)).await {
                Ok(value) => value.unchecked_into(),
                Err(error) => match classify(error, "getFileHandle") {
                    OpfsError::NotFound(_) => return Ok(None),
                    other => return Err(other),
                },
            };
        let file_promise = handle.get_file();
        // Duck-typed rather than `web_sys::File`: only `arrayBuffer()` is needed, and
        // pulling in the whole `File`/`Blob` binding for one call is bytes for nothing.
        let file: JsValue = JsFuture::from(file_promise)
            .await
            .map_err(|e| classify(e, "getFile"))?;
        let array_buffer = js_sys::Reflect::get(&file, &JsValue::from_str("arrayBuffer"))
            .ok()
            .filter(|value| value.is_function())
            .ok_or_else(|| OpfsError::Other("file has no arrayBuffer()".to_string()))?;
        let array_buffer: js_sys::Function = array_buffer.unchecked_into();
        let buffer_promise = array_buffer
            .call0(&file)
            .map_err(|e| classify(e, "arrayBuffer"))?;
        let buffer = JsFuture::from(js_sys::Promise::from(buffer_promise))
            .await
            .map_err(|e| classify(e, "arrayBuffer"))?;
        let array = js_sys::Uint8Array::new(&buffer);
        Ok(Some(array.to_vec()))
    }

    /// Reads through `createSyncAccessHandle`.
    async fn read_sync(&self, key: &str) -> Result<Option<Vec<u8>>, OpfsError> {
        let (dir, name) = ensure_parent(&self.root, key).await?;
        let handle: web_sys::FileSystemFileHandle =
            match JsFuture::from(dir.get_file_handle(&name)).await {
                Ok(value) => value.unchecked_into(),
                Err(_) => return Ok(None),
            };
        let access = access_handle(&handle).await?;
        let size = access.get_size().map_err(|e| classify(e, "getSize"))? as usize;
        if size == 0 {
            access.close();
            return Ok(Some(Vec::new()));
        }
        let mut buffer = vec![0u8; size];
        let options = web_sys::FileSystemReadWriteOptions::new();
        options.set_at(0.0);
        access
            .read_with_u8_array_and_options(&mut buffer, &options)
            .map_err(|e| classify(e, "read"))?;
        access.close();
        Ok(Some(buffer))
    }
}

async fn access_handle(
    handle: &web_sys::FileSystemFileHandle,
) -> Result<web_sys::FileSystemSyncAccessHandle, OpfsError> {
    let value = JsFuture::from(handle.create_sync_access_handle())
        .await
        .map_err(|e| classify(e, "createSyncAccessHandle"))?;
    Ok(value.unchecked_into())
}

impl FileStore for WebFiles {
    fn path(&self) -> AccessPath {
        self.path
    }

    async fn create_dir(&self, dir: &str) -> Result<(), OpfsError> {
        let mut current = self.root.clone();
        for segment in dir.split('/').filter(|s| !s.is_empty()) {
            current = get_directory_handle(&current, segment).await?;
        }
        Ok(())
    }

    async fn read(&self, key: &str) -> Result<Option<Vec<u8>>, OpfsError> {
        match self.path {
            AccessPath::SyncAccessHandle => self.read_sync(key).await,
            _ => self.read_async(key).await,
        }
    }

    async fn write(&self, key: &str, bytes: &[u8]) -> Result<(), OpfsError> {
        match self.path {
            AccessPath::SyncAccessHandle => self.write_sync(key, bytes).await,
            _ => self.write_async(key, bytes).await,
        }
    }

    async fn remove(&self, key: &str) -> Result<(), OpfsError> {
        let (dir, name) = ensure_parent(&self.root, key).await?;
        // A missing file is not an error for `remove`.
        let _ = JsFuture::from(dir.remove_entry(&name)).await;
        Ok(())
    }

    async fn keys(&self) -> Result<Vec<String>, OpfsError> {
        // OPFS has no recursive listing and no recursive `keys()`, so this walks the
        // tree by hand. The iteration protocol is driven through `Reflect` rather than
        // typed bindings because `FileSystemDirectoryHandle.entries()` returns an async
        // iterable whose `js_sys` shape is a moving target; reading `next()`/`done`/
        // `value` off it is stable across the versions we test.
        let mut out = Vec::new();
        let mut stack = vec![(String::new(), self.root.clone())];
        let mut guard = 0usize;
        while let Some((prefix, dir)) = stack.pop() {
            let entries = dir.entries();
            // `for await` is not reachable from stable wasm-bindgen, so the async
            // iterator is drained through its own `next()` method.
            let iterator: JsValue = entries.clone().into();
            let next = js_sys::Reflect::get(&iterator, &JsValue::from_str("next"))
                .ok()
                .and_then(|value| value.dyn_into::<js_sys::Function>().ok());
            let next = match next {
                Some(next) => next,
                None => continue,
            };
            loop {
                guard += 1;
                if guard > 20_000 {
                    return Err(OpfsError::Other(
                        "directory walk exceeded 20k entries".to_string(),
                    ));
                }
                let step = match next.call0(&iterator) {
                    Ok(step) => step,
                    Err(_) => break,
                };
                let done = js_sys::Reflect::get(&step, &JsValue::from_str("done"))
                    .ok()
                    .and_then(|value| value.as_bool())
                    .unwrap_or(true);
                if done {
                    break;
                }
                let pair = js_sys::Reflect::get(&step, &JsValue::from_str("value"))
                    .unwrap_or(JsValue::NULL);
                let name = js_sys::Reflect::get(&pair, &JsValue::from_str("0"))
                    .ok()
                    .and_then(|value| value.as_string())
                    .unwrap_or_default();
                let handle =
                    js_sys::Reflect::get(&pair, &JsValue::from_str("1")).unwrap_or(JsValue::NULL);
                let kind = js_sys::Reflect::get(&handle, &JsValue::from_str("kind"))
                    .ok()
                    .and_then(|value| value.as_string())
                    .unwrap_or_default();
                let path = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if kind == "directory" {
                    stack.push((path, handle.unchecked_into()));
                } else {
                    out.push(path);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    async fn remove_dir(&self, dir: &str) -> Result<(), OpfsError> {
        // Remove the leaf only: `removeEntry({recursive: true})` on the parent would be
        // one call, but the parent of a root path is the OPFS root, and deleting by name
        // there is clearer than reasoning about recursion options.
        let segments: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return Ok(());
        }
        let mut current = self.root.clone();
        for segment in &segments[..segments.len() - 1] {
            match get_directory_handle(&current, segment).await {
                Ok(next) => current = next,
                Err(OpfsError::NotFound(_)) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        let options = web_sys::FileSystemRemoveOptions::new();
        options.set_recursive(true);
        let _ = current.remove_entry_with_options(segments[segments.len() - 1], &options);
        Ok(())
    }
}

/// Opens the storage for JS callers, reporting which path was chosen.
///
/// The handle holds the `Rc`, so it is not `Send` — wasm32 is single-threaded by
/// default (PLAN.md 3.4), which is the assumption the whole crate makes.
#[wasm_bindgen]
pub async fn open_opfs() -> Result<OpfsHandle, JsValue> {
    let files = WebFiles::open()
        .await
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    let path = files.path();
    Ok(OpfsHandle {
        storage: OpfsStorage::new(Rc::new(files)),
        path,
    })
}

/// The handle JS holds.
#[wasm_bindgen]
pub struct OpfsHandle {
    storage: OpfsStorage<WebFiles>,
    path: AccessPath,
}

impl OpfsHandle {
    /// Borrow the storage, for the conformance runner.
    #[must_use]
    pub fn storage(&self) -> &OpfsStorage<WebFiles> {
        &self.storage
    }
}

#[wasm_bindgen]
impl OpfsHandle {
    /// The storage name (`opfs`).
    #[wasm_bindgen(getter)]
    pub fn name(&self) -> String {
        crate::STORAGE_NAME.to_string()
    }

    /// Which OPFS API this handle is using, as a stable identifier.
    ///
    /// `opfs-sync-access-handle` inside a dedicated worker,
    /// `opfs-async-writable` on the main thread. Callers assert on this rather than
    /// hoping: an accidental fallback is the failure mode this exists to prevent.
    #[wasm_bindgen(getter)]
    pub fn path(&self) -> String {
        match self.path {
            AccessPath::SyncAccessHandle => "opfs-sync-access-handle".to_string(),
            AccessPath::AsyncWritable => "opfs-async-writable".to_string(),
            AccessPath::InMemory => "in-memory".to_string(),
        }
    }

    /// The backing handle, for the conformance runner.
    #[wasm_bindgen(getter)]
    pub fn storage_json(&self) -> String {
        format!(
            "{{\"name\":\"{}\",\"path\":\"{}\"}}",
            crate::STORAGE_NAME,
            self.path()
        )
    }
}
