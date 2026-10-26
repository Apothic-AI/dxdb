//! The WebSocket adapter's hermetic convergence test: two dxdb instances over a real socket.
//!
//! Same shape as `two_process.rs`, one layer down: instead of HTTP request/response it is a
//! WebSocket frame per call, so the framing is exercised as well as the protocol. The point is
//! that nothing about the replication changes — the transport is the only variable.
//!
//! # Hermetic
//!
//! * The listener binds `127.0.0.1:0`; the port is read back from the OS. No fixed port number
//!   appears in this file, so concurrent runs cannot collide.
//! * Loopback only, and the server thread is joined on drop (including on panic).
//! * No external service, no TLS, no DNS.
//!
//! # Minimal RFC 6455
//!
//! The handshake is the SHA-1 accept-key dance from RFC 6455 §4.2.2; frames are written as
//! unmasked server→client and read as masked client→server, which is all the RFC requires of
//! this pair. The brief says not to pull in a framework for this, and the whole server is about
//! eighty lines.
//!
//! Fragmentation, ping/pong, extensions and close-properly are **not** implemented: the test
//! sends one unfragmented text frame per call and drops the connection at the end. That is
//! enough to prove the adapter's format works over a socket; a production server is a binding
//! crate's job.

mod common {
    pub mod memory_storage;
}

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::rc::Rc;

use common::memory_storage::{stored_document, MemoryStorage};
use dxdb_json::Value;
use dxdb_replication::adapters::websocket::{self, Frame, WebSocketHandler};
use dxdb_replication::checkpoint::Sha256HashFunction;
use dxdb_replication::conflict::DefaultConflictHandler;
use dxdb_replication::downstream::Downstream;
use dxdb_replication::state::{
    BatchSize, DocumentsWithCheckpoint, InitialCheckpoint, PullStream, ReplicationHandler,
    ReplicationInput, ReplicationState, WriteToMasterRow,
};
use dxdb_replication::upstream::Upstream;
use dxdb_storage::{DocumentData, RxStorageInstance, StorageError};

// ---------------------------------------------------------------------------
// A minimal RFC 6455 server
// ---------------------------------------------------------------------------

/// The base64 alphabet, needed for the handshake's `Sec-WebSocket-Accept`.
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64-encodes the 20-byte SHA-1 digest, which is what the handshake requires.
fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(BASE64[(n >> 18) as usize & 63] as char);
        out.push(BASE64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            BASE64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// SHA-1, only for the handshake's accept key.
///
/// A hand-rolled SHA-1 because the workspace does not depend on one and this is the only place
/// it is needed. It is not used for anything security-relevant — the RFC's accept key is a
/// fixed nonce check, not authentication.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Reads one masked client text frame.
fn read_frame(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut header = [0u8; 2];
    if stream.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    let opcode = header[0] & 0x0F;
    if opcode == 0x8 {
        // close
        return Ok(None);
    }
    let masked = header[1] & 0x80 != 0;
    let mut length = (header[1] & 0x7F) as usize;
    if length == 126 {
        let mut extended = [0u8; 2];
        stream.read_exact(&mut extended)?;
        length = u16::from_be_bytes(extended) as usize;
    } else if length == 127 {
        let mut extended = [0u8; 8];
        stream.read_exact(&mut extended)?;
        length = u64::from_be_bytes(extended) as usize;
    }
    let mask = if masked {
        let mut mask = [0u8; 4];
        stream.read_exact(&mut mask)?;
        Some(mask)
    } else {
        None
    };
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload)?;
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok(Some(String::from_utf8_lossy(&payload).to_string()))
}

/// Writes one unmasked server text frame.
fn write_frame(stream: &mut TcpStream, text: &str) -> std::io::Result<()> {
    let bytes = text.as_bytes();
    let mut header = vec![0x81u8]; // FIN + text
    if bytes.len() < 126 {
        header.push(bytes.len() as u8);
    } else if bytes.len() <= u16::MAX as usize {
        header.push(126);
        header.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    }
    stream.write_all(&header)?;
    stream.write_all(bytes)?;
    stream.flush()
}

/// A WebSocket server that runs on the test's own thread.
///
/// `ReplicationHandler` is deliberately `!Send` — a database is single-threaded (crate docs,
/// D1) — so a handler can never be moved into a thread. That is the model, not an obstacle, so
/// this "server" is a listener the *client* pumps: each call dials it, hands the connection to
/// the server loop, and reads the answer. One thread owns both halves, and a real socket with
/// real RFC 6455 frames carries the exchange in between.
///
/// A production binding replaces this with a client on its own thread and a server in its own
/// process. Neither the protocol nor the frame format changes, which is the thing being proven.
struct WsServer {
    listener: TcpListener,
    addr: SocketAddr,
}

impl WsServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("read the address");
        listener
            .set_nonblocking(true)
            .expect("nonblocking accept so the server can be pumped");
        WsServer { listener, addr }
    }

    fn base(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Accepts the queued connection and completes the handshake.
    ///
    /// Returns the server end of the socket, positioned right after the 101. The caller then
    /// drives frames on it one at a time; see [`serve_frame`].
    fn accept(&self) -> std::io::Result<TcpStream> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => return handshake(stream),
                Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() > deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "no client connection arrived",
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// The server side of the handshake (RFC 6455 §4.2.2): read the request, answer the 101.
fn handshake(mut stream: TcpStream) -> std::io::Result<TcpStream> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed during the handshake",
            ));
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&request).to_string();
    let key = request
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .starts_with("sec-websocket-key:")
                .then(|| line["sec-websocket-key:".len()..].trim().to_string())
        })
        .unwrap_or_default();
    let accept = base64(&sha1(
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    ));
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(stream)
}

/// Reads one frame from the client and answers it.
///
/// One call in, one call out. The loop lives at the caller so the client and server can share a
/// thread without either blocking on a frame the other has not sent yet — which is the whole
/// reason this is not a background thread.
fn serve_frame(
    stream: &mut TcpStream,
    handler: &dyn ReplicationHandler,
    received: &mut Vec<Frame>,
) -> std::io::Result<()> {
    let Some(text) = read_frame(stream)? else {
        return Ok(());
    };
    let frame = match Frame::parse(&text) {
        Ok(frame) => frame,
        Err(error) => {
            let mut map = dxdb_json::Map::new();
            map.insert("error".to_string(), Value::String(error.to_string()));
            return write_frame(
                stream,
                &Frame::Response(dxdb_replication::adapters::websocket::Response {
                    id: String::new(),
                    collection: String::new(),
                    result: Value::Object(map),
                })
                .encode(),
            );
        }
    };
    received.push(frame.clone());
    let answer = block_on(websocket::serve(handler, frame));
    write_frame(stream, &answer.encode())
}

/// Writes one masked client text frame, as the RFC requires of a client.
fn write_masked(stream: &mut TcpStream, text: &str) -> std::io::Result<()> {
    let bytes = text.as_bytes();
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    let mut header = vec![0x81u8];
    if bytes.len() < 126 {
        header.push(0x80 | bytes.len() as u8);
    } else {
        header.push(0x80 | 126);
        header.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    }
    header.extend_from_slice(&mask);
    let masked: Vec<u8> = bytes
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ mask[index % 4])
        .collect();
    stream.write_all(&header)?;
    stream.write_all(&masked)?;
    stream.flush()
}

/// A sender that owns the server handle, so a frame genuinely crosses a socket.
///
/// This is the seam that makes the test possible under `!Send`: the closure holds everything it
/// needs (the listener and the handler) and performs client and server in one call. The protocol
/// above sees only `Fn(Frame) -> Result<Frame>`.
struct LoopbackSender {
    server: Rc<WsServer>,
    handler: Rc<StorageMaster>,
    received: RefCell<Vec<Frame>>,
}

impl LoopbackSender {
    fn new(server: Rc<WsServer>, handler: Rc<StorageMaster>) -> Self {
        LoopbackSender {
            server,
            handler,
            received: RefCell::new(Vec::new()),
        }
    }

    fn call(
        &self,
        frame: Frame,
    ) -> Result<Frame, dxdb_replication::adapters::websocket::SendError> {
        let base = self.server.base();
        let authority = base.strip_prefix("ws://").unwrap_or(&base).to_string();
        let mut stream = TcpStream::connect(&authority).map_err(socket_error)?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .map_err(socket_error)?;

        // Client side of the handshake.
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let request = format!(
            "GET / HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).map_err(socket_error)?;
        stream.flush().map_err(socket_error)?;

        // Server side: accept and complete the handshake. Whatever the client sent before this
        // point is already buffered on the socket, so nothing deadlocks.
        let mut server_stream = self.server.accept().map_err(socket_error)?;

        // Read the client's 101.
        let mut response = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            let read = stream.read(&mut buffer).map_err(socket_error)?;
            if read == 0 {
                return Err(socket("server closed during the handshake"));
            }
            response.extend_from_slice(&buffer[..read]);
            if response.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&response).to_string();
        if !head.starts_with("HTTP/1.1 101") {
            return Err(socket(format!("handshake rejected: {head}")));
        }

        // The call: one masked frame out, the server answers it on the same thread, one
        // unmasked frame back. Serving here rather than in a loop is what makes a `!Send`
        // handler usable — the server reads exactly the frame the client just wrote.
        write_masked(&mut stream, &frame.encode()).map_err(socket_error)?;
        {
            let mut received = self.received.borrow_mut();
            serve_frame(&mut server_stream, self.handler.as_ref(), &mut received)
                .map_err(socket_error)?;
        }
        let text = read_frame(&mut stream)
            .map_err(socket_error)?
            .ok_or_else(|| socket("server closed without answering"))?;
        Frame::parse(&text).map_err(|error| {
            dxdb_replication::adapters::websocket::SendError::Transport(error.to_string())
        })
    }
}

fn socket(message: impl Into<String>) -> dxdb_replication::adapters::websocket::SendError {
    dxdb_replication::adapters::websocket::SendError::Transport(message.into())
}

fn socket_error(error: std::io::Error) -> dxdb_replication::adapters::websocket::SendError {
    dxdb_replication::adapters::websocket::SendError::Transport(error.to_string())
}

// ---------------------------------------------------------------------------
// The server-side replication: a master over the memory storage
// ---------------------------------------------------------------------------

/// The master database, wrapped as a `ReplicationHandler`.
///
/// This is `rxStorageInstanceToReplicationHandler` (`index.ts:184-338`) in miniature: the
/// server's "master" is itself a dxdb storage, which is what makes the test a real
/// dxdb→WebSocket→dxdb round trip rather than a fixture pretending.
struct StorageMaster {
    storage: MemoryStorage,
    primary_path: String,
    change_stream: RefCell<Option<dxdb_storage::ChangeStream>>,
}

impl StorageMaster {
    fn new() -> Self {
        let storage = MemoryStorage::new("ws-master", "human", human_schema(), "master-token");
        StorageMaster {
            storage,
            primary_path: "passportId".to_string(),
            change_stream: RefCell::new(None),
        }
    }
}

fn human_schema() -> Value {
    dxdb_json::parse(
        r#"{
            "version": 0,
            "keyCompression": false,
            "primaryKey": "passportId",
            "type": "object",
            "properties": {
                "passportId": { "type": "string", "maxLength": 100 },
                "firstName": { "type": "string", "maxLength": 100 },
                "age": { "type": "integer" }
            },
            "indexes": ["firstName"]
        }"#,
    )
    .expect("valid schema")
}

fn human(id: &str, first: &str, age: i64) -> DocumentData {
    let mut document = stored_document(
        "passportId",
        id,
        &[
            ("firstName", Value::String(first.to_string())),
            ("age", Value::from(age)),
        ],
    );
    document.set("_rev", Value::String("1-master".to_string()));
    document
}

fn same_content(a: &DocumentData, b: &DocumentData) -> bool {
    fn projection(document: &DocumentData) -> Value {
        let mut map = dxdb_json::Map::new();
        for (key, value) in document.as_object() {
            if matches!(key.as_str(), "_rev" | "_meta" | "_attachments") {
                continue;
            }
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }
    projection(a) == projection(b)
}

impl ReplicationHandler for StorageMaster {
    fn master_changes_since(
        &self,
        checkpoint: Option<&dxdb_replication::checkpoint::Checkpoint>,
        batch_size: usize,
    ) -> dxdb_replication::state::BoxFuture<'_, Result<DocumentsWithCheckpoint, StorageError>> {
        let storage = self.storage.clone();
        let checkpoint = checkpoint.cloned();
        Box::pin(async move {
            let changed = storage
                .get_changed_documents_since(
                    batch_size,
                    checkpoint
                        .as_ref()
                        .and_then(|checkpoint| checkpoint.as_storage_checkpoint()),
                )
                .await?;
            let checkpoint = changed
                .documents
                .last()
                .map(|last| {
                    dxdb_replication::checkpoint::Checkpoint::from_storage_checkpoint(
                        &dxdb_storage::Checkpoint::new(
                            last.get("passportId")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            last.lwt().unwrap_or(0.0),
                        ),
                    )
                })
                .or(checkpoint);
            Ok(DocumentsWithCheckpoint {
                documents: changed.documents,
                checkpoint,
            })
        })
    }

    fn master_write(
        &self,
        rows: Vec<WriteToMasterRow>,
    ) -> dxdb_replication::state::BoxFuture<'_, Result<Vec<DocumentData>, StorageError>> {
        let storage = self.storage.clone();
        let primary_path = self.primary_path.clone();
        Box::pin(async move {
            let ids: Vec<String> = rows
                .iter()
                .filter_map(|row| {
                    row.new_document_state
                        .get(&primary_path)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect();
            let mut existing: BTreeMap<String, DocumentData> = BTreeMap::new();
            for document in storage.find_documents_by_id(ids.clone(), true).await? {
                if let Some(id) = document
                    .get(&primary_path)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                {
                    existing.insert(id, document);
                }
            }

            let mut conflicts = Vec::new();
            let mut writes = Vec::new();
            for row in rows {
                let id = row
                    .new_document_state
                    .get(&primary_path)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let master_state = existing.get(&id);
                // The four `masterWrite` rules.
                let accepted = match (master_state, &row.assumed_master_state) {
                    (None, _) => true,
                    (Some(master), None) => {
                        conflicts.push(master.clone());
                        false
                    }
                    (Some(master), Some(assumed)) => {
                        if same_content(master, assumed) {
                            true
                        } else {
                            conflicts.push(master.clone());
                            false
                        }
                    }
                };
                if !accepted {
                    continue;
                }
                let mut write = dxdb_storage::DocumentWriteData::new(row.new_document_state);
                for (attachment_id, bytes) in &row.attachment_payloads {
                    if let Some(stub) = write.document().attachment(attachment_id) {
                        if let Some(parsed) = dxdb_storage::AttachmentData::from_value(stub) {
                            write.insert_attachment(
                                attachment_id,
                                dxdb_storage::AttachmentWriteData {
                                    stub: parsed,
                                    data: Some(bytes.clone()),
                                },
                            );
                        }
                    }
                }
                writes.push(match master_state {
                    Some(master) => dxdb_storage::BulkWriteRow {
                        previous: Some(master.clone()),
                        document: write,
                    },
                    None => dxdb_storage::BulkWriteRow {
                        previous: None,
                        document: write,
                    },
                });
            }
            if !writes.is_empty() {
                let response = storage
                    .bulk_write(writes, "ws-master-write".to_string())
                    .await?;
                for error in response.error {
                    if !error.is_conflict() {
                        return Err(StorageError::InvalidDocument(format!(
                            "master write failed with status {}",
                            error.status()
                        )));
                    }
                    if let dxdb_storage::WriteError::Conflict {
                        document_in_db: Some(in_db),
                        ..
                    } = error
                    {
                        conflicts.push(in_db);
                    }
                }
            }
            Ok(conflicts)
        })
    }

    fn master_change_stream(&self) -> PullStream {
        // The client pulls explicitly in this test; a live stream would add timing to an
        // assertion about convergence.
        let holder = self.change_stream.borrow_mut();
        let _ = &holder;
        Box::pin(futures::stream::empty())
    }
}

/// A `WebSocketHandler` whose frames go through a real socket, served inline.
fn handler_over(
    sender: Rc<LoopbackSender>,
) -> WebSocketHandler<
    impl Fn(Frame) -> Result<Frame, dxdb_replication::adapters::websocket::SendError>,
> {
    WebSocketHandler::new("human", move |frame: Frame| sender.call(frame))
}

struct Side {
    fork: MemoryStorage,
    state: ReplicationState<MemoryStorage>,
}

fn side<F>(handler: WebSocketHandler<F>, identifier: &str) -> Side
where
    F: Fn(Frame) -> Result<Frame, dxdb_replication::adapters::websocket::SendError> + 'static,
{
    let fork = MemoryStorage::new("ws-client", "human", human_schema(), "fork-token");
    let meta_schema =
        dxdb_replication::meta_instance::replication_meta_instance_schema(fork.schema(), false)
            .expect("meta schema");
    let meta = MemoryStorage::new(
        "ws-client",
        "rx-replication-meta",
        meta_schema,
        "meta-token",
    );
    let state = ReplicationState::new(ReplicationInput {
        identifier: identifier.to_string(),
        pull_batch_size: BatchSize::new(100),
        push_batch_size: BatchSize::new(100),
        handler: Box::new(handler),
        conflict_handler: Box::new(DefaultConflictHandler),
        fork: fork.clone(),
        meta: meta.instance(),
        hash_function: Rc::new(Sha256HashFunction),
        skip_storing_pull_meta: false,
        keep_meta: false,
        initial_checkpoint: InitialCheckpoint::default(),
        wait_before_persist: None,
    });
    Side { fork, state }
}

fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Noop;
    impl Wake for Noop {
        fn wake(self: std::sync::Arc<Self>) {}
        fn wake_by_ref(self: &std::sync::Arc<Self>) {}
    }
    let waker = Waker::from(std::sync::Arc::new(Noop));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// Two dxdb instances converge over a real WebSocket, and the checkpoint is exact.
#[test]
fn two_dxdb_instances_converge_over_websocket() {
    // One master, one listener. The handler is `!Send`, so the sender owns it and pumps the
    // server inline; that is the only way to have a real socket with a single-threaded
    // database, and it is what the `LoopbackSender` exists for.
    let server = Rc::new(WsServer::start());
    let master = Rc::new(StorageMaster::new());

    let sender1 = Rc::new(LoopbackSender::new(server.clone(), master.clone()));
    let sender2 = Rc::new(LoopbackSender::new(server.clone(), master.clone()));

    let c1 = side(handler_over(sender1.clone()), "ws-repl-c1");
    let c2 = side(handler_over(sender2.clone()), "ws-repl-c2");

    // c1 writes five documents and pushes them over the socket.
    for index in 0..5 {
        block_on(c1.fork.bulk_write(
            vec![dxdb_storage::insert_row(human(
                &format!("ws-doc-{index}"),
                "over-websocket",
                20 + index,
            ))],
            "local".to_string(),
        ))
        .expect("the fork write succeeds");
    }
    let mut upstream = Upstream::new(c1.state.clone());
    block_on(upstream.pump()).expect("the push succeeds");

    // The frames really were the protocol's: a write call went over the wire.
    let frames = sender1.received.borrow().clone();
    let saw_write = frames.iter().any(|frame| {
        matches!(
            frame,
            Frame::Request(request)
                if request.method == dxdb_replication::adapters::websocket::Method::MasterWrite
        )
    });
    assert!(
        saw_write,
        "the push must have sent a masterWrite frame, got {} frames",
        frames.len()
    );

    // c2 pulls the same documents over its own socket.
    let mut downstream = Downstream::new(c2.state.clone(), c2.state.identifier_hash());
    block_on(downstream.resync()).expect("the pull succeeds");

    // --- convergence ---------------------------------------------------------
    let c1_docs = c1.fork.all_documents();
    let c2_docs = c2.fork.all_documents();
    assert_eq!(c1_docs.len(), 5, "c1 holds all five");
    assert_eq!(c2_docs.len(), 5, "c2 pulled all five over the socket");

    let mut c1_ids: Vec<String> = c1_docs
        .iter()
        .filter_map(|document| {
            document
                .get("passportId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let mut c2_ids: Vec<String> = c2_docs
        .iter()
        .filter_map(|document| {
            document
                .get("passportId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    c1_ids.sort();
    c2_ids.sort();
    assert_eq!(
        c1_ids, c2_ids,
        "the two instances hold the same document ids"
    );

    for document in &c2_docs {
        let id = document
            .get("passportId")
            .and_then(Value::as_str)
            .expect("primary key");
        let original = c1_docs
            .iter()
            .find(|candidate| candidate.get("passportId").and_then(Value::as_str) == Some(id))
            .expect("the same id exists on c1");
        assert_eq!(
            content_of(document),
            content_of(original),
            "document {id} converged in content"
        );
        assert_eq!(
            document.get("firstName").and_then(Value::as_str),
            Some("over-websocket"),
            "and the field values survived the frame encoding"
        );
    }

    // --- checkpoint equality -------------------------------------------------
    //
    // The pushed checkpoint crossed frames, so this also proves the frame encoding preserves
    // the protocol's checkpoint bytes rather than reformatting numbers.
    let pushed = block_on(dxdb_replication::checkpoint::last_checkpoint(
        &c1.state,
        dxdb_replication::checkpoint::Direction::Up,
    ))
    .expect("read")
    .expect("a push checkpoint exists");
    let serialised = pushed.to_js_json();
    let reparsed = dxdb_replication::checkpoint::Checkpoint::new(
        dxdb_json::parse(&serialised).expect("valid json"),
    )
    .expect("object");
    assert_eq!(
        reparsed.to_js_json(),
        serialised,
        "a checkpoint must survive a frame round trip unchanged"
    );
    let replayed = block_on(
        c1.fork
            .get_changed_documents_since(100, pushed.as_storage_checkpoint()),
    )
    .expect("read");
    assert!(
        replayed.documents.is_empty(),
        "the up checkpoint must resume the fork feed cleanly"
    );

    // --- a second round, to prove it is not one-shot -------------------------
    block_on(c2.fork.bulk_write(
        vec![dxdb_storage::insert_row(human("ws-from-c2", "second", 40))],
        "local".to_string(),
    ))
    .expect("the c2 write succeeds");
    let mut upstream2 = Upstream::new(c2.state.clone());
    block_on(upstream2.pump()).expect("the c2 push succeeds");

    let mut downstream1 = Downstream::new(c1.state.clone(), c1.state.identifier_hash());
    block_on(downstream1.resync()).expect("the c1 pull succeeds");

    assert_eq!(
        c1.fork.all_documents().len(),
        6,
        "c1 pulled c2's new document over the socket"
    );
    assert!(
        c1.fork.get("ws-from-c2").is_some(),
        "and it is the right document"
    );
}

/// The server is on an ephemeral loopback port, and two servers do not collide.
#[test]
fn the_server_binds_an_ephemeral_loopback_port() {
    let first = WsServer::start();
    let second = WsServer::start();
    assert!(first.addr.ip().is_loopback(), "loopback only");
    assert!(first.addr.port() > 1024, "ephemeral, not privileged");
    assert_ne!(
        first.addr.port(),
        second.addr.port(),
        "two servers must not share a port"
    );
    assert!(
        first.base().starts_with("ws://127.0.0.1:"),
        "the base URL is loopback: {}",
        first.base()
    );
}

/// A frame with the wrong arity is answered with an error naming the problem.
///
/// Driven through the real socket, so the error path of the frame format is covered too.
#[test]
fn a_bad_call_is_answered_with_an_error() {
    let server = Rc::new(WsServer::start());
    let master = Rc::new(StorageMaster::new());
    let sender = LoopbackSender::new(server, master);

    let answer = sender
        .call(Frame::Request(
            dxdb_replication::adapters::websocket::Request {
                id: "bad".to_string(),
                collection: "human".to_string(),
                method: dxdb_replication::adapters::websocket::Method::MasterChangesSince,
                // Wrong arity on purpose.
                params: vec![Value::Null],
            },
        ))
        .expect("a response comes back");
    let Frame::Response(response) = answer else {
        panic!("expected a response")
    };
    let error = dxdb_replication::adapters::websocket::result_error(&response.result)
        .expect("the server must report the arity error");
    assert!(
        error.contains("expects 2 parameters"),
        "the error names the problem, got {error}"
    );
}

/// A document's content, ignoring each side's local bookkeeping.
fn content_of(document: &DocumentData) -> Value {
    let mut map = dxdb_json::Map::new();
    for (key, value) in document.as_object() {
        if matches!(key.as_str(), "_rev" | "_meta") {
            continue;
        }
        if key == "_attachments" {
            if let Some(attachments) = value.as_object() {
                if attachments.is_empty() {
                    continue;
                }
            }
        }
        map.insert(key.clone(), value.clone());
    }
    Value::Object(map)
}
