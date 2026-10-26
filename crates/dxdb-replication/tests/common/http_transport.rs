//! A minimal HTTP transport for a real `ReplicationHandler`.
//!
//! This is the interop evidence `PLAN.md` section 10 asks for: dxdb -> HTTP -> dxdb,
//! running the actual protocol over the actual wire format, with no shortcut between the
//! two halves.
//!
//! # Why a hand-rolled server
//!
//! The lane brief is explicit: "If the two-process test needs an HTTP server
//! implementation, write the minimal one the test needs (a dozen lines over `std::net` on
//! an ephemeral port is fine) rather than pulling in a framework — the server crate is
//! B5's job." So:
//!
//! * `std::net::TcpListener` on `127.0.0.1:0`, so the port is ephemeral and the test never
//!   depends on a fixed number;
//! * HTTP/1.1 with `Connection: close`, one request per connection, which is all the test
//!   needs and removes keep-alive, chunking and pipelining from the surface;
//! * JSON bodies with the protocol's own encoding (`dxdb_json::to_js_json`), because the
//!   whole point is that the wire bytes are the protocol's, not a debug format.
//!
//! # The wire format is the contract
//!
//! Three routes, each a direct transcription of one `RxReplicationHandler` method:
//!
//! | route | request | response |
//! | --- | --- | --- |
//! | `POST /changes-since` | `{checkpoint, batchSize}` | `{documents, checkpoint}` |
//! | `POST /write` | `{rows: [{assumedMasterState, newDocumentState}]}` | `{conflicts: [doc]}` |
//! | `POST /stream` | `{}` | `{items: [...]}` (the change stream, polled) |
//!
//! `master_write` returning **only conflicts** is the contract that is easiest to get
//! wrong and hardest to notice: a server that returns everything works in the happy path
//! and makes the client re-resolve conflicts forever. The two-process test asserts on the
//! response shape, not only on convergence.
//!
//! # Threading
//!
//! One OS thread per connection, because the test drives two sides in one process and needs
//! the server to make progress while the client is between requests. That is a test-harness
//! convenience; a real server crate owns its own concurrency story.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use dxdb_json::Value;
use dxdb_storage::{Checkpoint, DocumentData};

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// The master side of the wire: documents plus the change log a pull reads.
///
/// Deliberately dumb. It holds documents, answers `changes-since` in checkpoint order, and
/// applies writes with the four `masterWrite` rules. It is *not* a storage: the two-process
/// test is about the protocol's wire behaviour, and putting a real storage behind it would
/// mean testing the storage twice.
#[derive(Default)]
pub struct MasterState {
    documents: Vec<DocumentData>,
    /// Monotonic last-write time, so `changes-since` has a total order.
    clock: f64,
    sequence: u64,
    /// Every request the server saw, for assertions about the wire.
    pub request_log: Vec<RequestRecord>,
}

/// One request the server handled, in wire form.
#[derive(Clone, Debug)]
pub struct RequestRecord {
    /// The path, e.g. `/write`.
    pub path: String,
    /// The parsed request body.
    pub body: Value,
    /// The parsed response body.
    pub response: Value,
}

impl MasterState {
    /// Applies a write and returns the conflicts, per upstream's four rules.
    fn write(&mut self, rows: &[Value]) -> Result<Vec<DocumentData>, String> {
        let mut conflicts = Vec::new();
        for row in rows {
            let new_state = row
                .get("newDocumentState")
                .cloned()
                .ok_or_else(|| "a write row needs a newDocumentState".to_string())?;
            let new_state = DocumentData::from_value(new_state)
                .map_err(|error| format!("newDocumentState is not an object: {error}"))?;
            let id = new_state
                .get("passportId")
                .and_then(Value::as_str)
                .ok_or_else(|| "document has no passportId".to_string())?
                .to_string();

            let existing = self
                .documents
                .iter()
                .find(|document| document.get("passportId").and_then(Value::as_str) == Some(&id))
                .cloned();

            let assumed = row
                .get("assumedMasterState")
                .filter(|value| !value.is_null())
                .and_then(|value| DocumentData::from_value(value.clone()).ok());

            match (&existing, &assumed) {
                // No master state: an insert.
                (None, _) => self.store(new_state),
                // Master has it but the client assumed nothing: a conflict.
                (Some(master), None) => conflicts.push(master.clone()),
                (Some(master), Some(assumed)) => {
                    // The client may only write on top of what the master actually holds.
                    if !same_content(master, assumed) {
                        conflicts.push(master.clone());
                    } else {
                        self.store(new_state);
                    }
                }
            }
        }
        Ok(conflicts)
    }

    fn store(&mut self, mut document: DocumentData) {
        self.sequence += 1;
        self.clock += 1.0;
        let id = document
            .get("passportId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let height = self
            .documents
            .iter()
            .find(|existing| existing.get("passportId").and_then(Value::as_str) == Some(&id))
            .and_then(|existing| existing.rev())
            .map(dxdb_storage::get_height_of_revision)
            .unwrap_or(0)
            + 1;
        document.set("_rev", Value::String(format!("{height}-master")));
        document
            .set_lwt(self.clock)
            .expect("the fixture clock is finite");
        document.set("_deleted", Value::Bool(document.is_deleted()));

        match self
            .documents
            .iter_mut()
            .find(|existing| existing.get("passportId").and_then(Value::as_str) == Some(&id))
        {
            Some(slot) => *slot = document,
            None => self.documents.push(document),
        }
    }

    /// The documents changed after `checkpoint`, in `(lwt, primary)` order.
    fn changes_since(
        &self,
        checkpoint: Option<Checkpoint>,
        batch_size: usize,
    ) -> (Vec<DocumentData>, Option<Checkpoint>) {
        let start = checkpoint.unwrap_or_else(Checkpoint::beginning);
        let mut ordered: Vec<&DocumentData> = self.documents.iter().collect();
        ordered.sort_by(|a, b| {
            let la = a.lwt().unwrap_or(0.0);
            let lb = b.lwt().unwrap_or(0.0);
            la.partial_cmp(&lb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let ia = a
                        .get("passportId")
                        .map(dxdb_json::to_js_json)
                        .unwrap_or_default();
                    let ib = b
                        .get("passportId")
                        .map(dxdb_json::to_js_json)
                        .unwrap_or_default();
                    dxdb_jsstr::compare_js(&ia, &ib)
                })
        });
        let documents: Vec<DocumentData> = ordered
            .into_iter()
            .filter(|document| {
                let lwt = document.lwt().unwrap_or(0.0);
                let id = document
                    .get("passportId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if lwt > start.lwt {
                    true
                } else if lwt == start.lwt {
                    dxdb_jsstr::compare_js(id, &start.id) == std::cmp::Ordering::Greater
                } else {
                    false
                }
            })
            .take(batch_size)
            .cloned()
            .collect();
        let checkpoint = documents.last().map(|last| {
            Checkpoint::new(
                last.get("passportId")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                last.lwt().unwrap_or(start.lwt),
            )
        });
        (documents, checkpoint)
    }

    /// Every stored document, for convergence assertions.
    pub fn all_documents(&self) -> Vec<DocumentData> {
        self.documents.clone()
    }
}

fn same_content(a: &DocumentData, b: &DocumentData) -> bool {
    fn projection(document: &DocumentData) -> Value {
        let mut map = dxdb_json::Map::new();
        for (key, value) in document.as_object() {
            if matches!(key.as_str(), "_rev" | "_meta") {
                continue;
            }
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }
    projection(a) == projection(b)
}

/// A running HTTP server over a [`MasterState`].
pub struct TestServer {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    state: Arc<Mutex<MasterState>>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    /// Binds `127.0.0.1:0` and starts serving.
    ///
    /// The port is ephemeral by construction: `:0` lets the OS choose, and the bound
    /// address is read back. No test ever names a port number, so two tests running at once
    /// cannot collide and a busy developer machine cannot break a suite.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral loopback port");
        let addr = listener.local_addr().expect("read the bound address");
        let shutdown = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(MasterState::default()));

        listener
            .set_nonblocking(true)
            .expect("the listener must be non-blocking so shutdown can be observed");

        let thread_state = state.clone();
        let thread_shutdown = shutdown.clone();
        let handle = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // One thread per connection: the client is between requests
                        // while the test drives the other side, so the server must not
                        // block the accept loop.
                        let connection_state = thread_state.clone();
                        thread::spawn(move || {
                            let _ = serve_connection(stream, connection_state);
                        });
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        TestServer {
            addr,
            shutdown,
            state,
            handle: Some(handle),
        }
    }

    /// The address the server is listening on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The base URL for a client.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The master's documents.
    pub fn documents(&self) -> Vec<DocumentData> {
        self.state.lock().expect("the master lock").all_documents()
    }

    /// Every request the server handled, for wire-level assertions.
    pub fn requests(&self) -> Vec<RequestRecord> {
        self.state
            .lock()
            .expect("the master lock")
            .request_log
            .clone()
    }

    /// Stops the server and waits for its thread.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve_connection(mut stream: TcpStream, state: Arc<Mutex<MasterState>>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    // Request line + headers.
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = trimmed.strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    let request_json: Value = if body.is_empty() {
        Value::Object(dxdb_json::Map::new())
    } else {
        dxdb_json::parse(std::str::from_utf8(&body).unwrap_or("{}"))
            .unwrap_or(Value::Object(dxdb_json::Map::new()))
    };

    let response = route(&method, &path, &request_json, &state);
    let response_body = dxdb_json::to_js_json(&response);

    // Record the exchange for the wire assertions.
    if let Ok(mut master) = state.lock() {
        master.request_log.push(RequestRecord {
            path: path.clone(),
            body: request_json,
            response: response.clone(),
        });
    }

    let head = format!(
        "{version} 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response_body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(response_body.as_bytes())?;
    stream.flush()
}

fn route(method: &str, path: &str, request: &Value, state: &Arc<Mutex<MasterState>>) -> Value {
    if method != "POST" {
        return json_error("only POST is served");
    }
    let mut master = match state.lock() {
        Ok(master) => master,
        Err(poisoned) => poisoned.into_inner(),
    };
    match path {
        "/changes-since" => {
            let checkpoint = request
                .get("checkpoint")
                .filter(|value| !value.is_null())
                .and_then(Checkpoint::from_json);
            let batch_size = request
                .get("batchSize")
                .and_then(Value::as_u64)
                .unwrap_or(100) as usize;
            let (documents, checkpoint) = master.changes_since(checkpoint, batch_size);
            object(vec![
                (
                    "documents",
                    Value::Array(
                        documents
                            .into_iter()
                            .map(DocumentData::into_value)
                            .collect(),
                    ),
                ),
                (
                    "checkpoint",
                    checkpoint
                        .map(|checkpoint| checkpoint.to_json())
                        .unwrap_or(Value::Null),
                ),
            ])
        }
        "/write" => {
            let rows = request
                .get("rows")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            match master.write(&rows) {
                Ok(conflicts) => object(vec![(
                    "conflicts",
                    Value::Array(
                        conflicts
                            .into_iter()
                            .map(DocumentData::into_value)
                            .collect(),
                    ),
                )]),
                Err(error) => json_error(&error),
            }
        }
        "/stream" => {
            // The test drives change delivery by polling; an empty list means "nothing
            // yet". A real transport would hold the request open or use SSE.
            object(vec![("items", Value::Array(Vec::new()))])
        }
        _ => json_error("unknown route"),
    }
}

fn object(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = dxdb_json::Map::new();
    for (key, value) in pairs {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

fn json_error(message: &str) -> Value {
    object(vec![("error", Value::String(message.to_string()))])
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// A `ReplicationHandler` that talks to a [`TestServer`] over HTTP.
///
/// This is the whole point of the exercise: the protocol above it calls three methods, and
/// this turns each into one request. Nothing in the protocol changes.
pub struct HttpHandler {
    base_url: String,
    /// Counts, so the test can assert the protocol actually made requests.
    ///
    /// Shared through an `Rc` because the protocol holds its own handle (the adapter may
    /// clone it, and a transport clone must observe the same traffic). Per-instance `Cell`s
    /// would make a caller read the count of a handle nobody used, which is a silent
    /// zero rather than a wrong number.
    counters: std::rc::Rc<Counters>,
}

/// Traffic counters, shared by every clone of a handler.
#[derive(Default)]
pub struct Counters {
    changes_since_calls: std::cell::Cell<u64>,
    write_calls: std::cell::Cell<u64>,
    responses: std::cell::RefCell<Vec<Value>>,
}

impl HttpHandler {
    /// Points a handler at a running server.
    pub fn new(base_url: impl Into<String>) -> Self {
        HttpHandler {
            base_url: base_url.into(),
            counters: std::rc::Rc::new(Counters::default()),
        }
    }

    /// A second handle onto the same counters and the same URL.
    pub fn clone_handle(&self) -> Self {
        HttpHandler {
            base_url: self.base_url.clone(),
            counters: self.counters.clone(),
        }
    }

    /// How many `changes-since` requests were made.
    pub fn changes_since_calls(&self) -> u64 {
        self.counters.changes_since_calls.get()
    }

    /// How many `write` requests were made.
    pub fn write_calls(&self) -> u64 {
        self.counters.write_calls.get()
    }

    /// The parsed response bodies, in order.
    pub fn responses(&self) -> Vec<Value> {
        self.counters.responses.borrow().clone()
    }

    /// Sends one POST and returns the parsed response body.
    pub fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        // Count before sending, so a request that fails to parse still shows as attempted.
        match path {
            "/changes-since" => self
                .counters
                .changes_since_calls
                .set(self.counters.changes_since_calls.get() + 1),
            "/write" => self
                .counters
                .write_calls
                .set(self.counters.write_calls.get() + 1),
            _ => {}
        }
        let url = format!("{}{}", self.base_url, path);
        let (status, response_body) = http_post(&url, &dxdb_json::to_js_json(&body))?;
        if status != 200 {
            return Err(format!("{path} returned HTTP {status}: {response_body}"));
        }
        let parsed = dxdb_json::parse(&response_body)
            .map_err(|error| format!("{path} returned invalid JSON: {error}"))?;
        // A body-level error is an error even on 200: the test server reports an unknown
        // route that way, and a caller that ignored it would treat a routing bug as an
        // empty successful response.
        if let Some(message) = parsed.get("error").and_then(Value::as_str) {
            return Err(format!("{path}: {message}"));
        }
        self.counters.responses.borrow_mut().push(parsed.clone());
        Ok(parsed)
    }
}

/// A single HTTP/1.1 POST over a fresh connection.
///
/// `std::net` only: no TLS, no keep-alive, no chunking. The URL is parsed by hand because
/// the only forms this ever sees are `http://host:port/path`.
fn http_post(url: &str, body: &str) -> Result<(u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// is supported, got {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let mut stream = TcpStream::connect(authority)
        .map_err(|error| format!("cannot connect to {authority}: {error}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .map_err(|error| format!("cannot set a read timeout: {error}"))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("cannot send the request: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("cannot flush the request: {error}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| format!("cannot read the response: {error}"))?;
    let text = String::from_utf8_lossy(&response).to_string();

    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            format!(
                "malformed status line in response: {}",
                &text[..text.len().min(80)]
            )
        })?;
    let body_start = text
        .find("\r\n\r\n")
        .map(|index| index + 4)
        .ok_or_else(|| "response has no header terminator".to_string())?;
    Ok((status, text[body_start..].to_string()))
}

/// Builds a URL for a server.
pub fn url_for(addr: SocketAddr, path: &str) -> String {
    format!("http://{addr}{path}")
}

/// A map from document id to document, for convergence assertions.
pub fn by_primary(documents: &[DocumentData]) -> HashMap<String, DocumentData> {
    documents
        .iter()
        .filter_map(|document| {
            document
                .get("passportId")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), document.clone()))
        })
        .collect()
}
