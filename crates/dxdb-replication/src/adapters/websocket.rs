//! A replication transport adapter for WebSocket.
//!
//! Ports the wire format of `src/plugins/replication-websocket/` (638 lines) without the
//! `ws` / `reconnecting-websocket` dependencies: the protocol framing was never RxDB's
//! invention, so this crate supplies the minimal RFC 6455 pieces a test needs and leaves
//! production framing to the platform binding.
//!
//! # The format is the whole adapter
//!
//! Upstream's protocol is a three-method RPC (`websocket-types.ts:37-45`):
//!
//! ```text
//! client -> server   { "id": "...", "collection": "human", "method": "masterChangesSince",
//!                      "params": [checkpoint, batchSize] }
//! server -> client   { "id": "...", "collection": "human", "result": { ... } }
//! ```
//!
//! `method` is one of the three [`ReplicationHandler`] names, and `params` is that method's
//! argument list. There is no provider semantics to invent: a frame is a call, a frame is a
//! response. That is why this adapter is first — it exercises the transport trait without
//! borrowing anyone's checkpoint model.
//!
//! # Design finding: the attachment payloads had to leave the document
//!
//! Upstream's `newDocumentState` embeds attachment payloads inside
//! `_attachments[id].data` as `Blob`s, and the `ws` library serialises a `Blob` in a frame.
//! A Rust document is ordered JSON and cannot hold bytes, so
//! [`WriteToMasterRow::attachment_payloads`](crate::state::WriteToMasterRow::attachment_payloads)
//! carries them beside the document instead.
//!
//! The trait therefore has one field upstream's does not. That is **not** leakage of this
//! adapter into the protocol: the field is on the protocol's own row type, is populated by the
//! protocol's own attachment step, and is ignored by every transport that has no attachments.
//! What it does mean is that a transport must decide how to put bytes on the wire, and this
//! module shows the choice being made explicitly (base64 in the frame, decoded back into the
//! document on receipt) rather than arriving as an implicit side effect of a JS `Blob`.
//!
//! # What is deliberately not here
//!
//! Reconnection, backoff and heartbeats (`reconnecting-websocket.ts`, 266 lines) belong to a
//! binding that owns a clock and a socket lifecycle. This adapter is the format, so it can be
//! tested hermetically and reused by whichever client ends up shipping.

use std::collections::BTreeMap;

use dxdb_json::Value;
use dxdb_storage::{DocumentData, StorageError};

use crate::checkpoint::Checkpoint;
use crate::state::{DocumentsWithCheckpoint, PullStream, ReplicationHandler, WriteToMasterRow};

/// One RPC the client sends.
///
/// Field names and order are upstream's (`websocket-types.ts:37-42`), so a JS peer and this
/// adapter produce the same frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// Correlates the response. Upstream uses `randomToken()`; any unique string works.
    pub id: String,
    /// Which collection the call is against. A server holds one handler per collection.
    pub collection: String,
    /// The handler method name, exactly as upstream spells it.
    pub method: Method,
    /// The method's arguments, positionally.
    pub params: Vec<Value>,
}

/// The three methods, plus upstream's `auth`.
///
/// `auth` exists in the format (`websocket-types.ts:40`) but upstream's client sends it only
/// when an `auth` handler is configured; this port keeps the variant so a server can reject an
/// unauthenticated client rather than treating it as an unknown method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    /// `masterChangesSince(checkpoint, batchSize)`
    MasterChangesSince,
    /// `masterWrite(rows)`
    MasterWrite,
    /// `masterChangeStream$` — not an RPC upstream; see [`Method::as_str`] for why.
    MasterChangeStream,
    /// `auth(token)`
    Auth,
}

impl Method {
    /// The upstream spelling, used on the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            Method::MasterChangesSince => "masterChangesSince",
            Method::MasterWrite => "masterWrite",
            Method::MasterChangeStream => "masterChangeStream$",
            Method::Auth => "auth",
        }
    }

    /// Parses an upstream spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "masterChangesSince" => Some(Method::MasterChangesSince),
            "masterWrite" => Some(Method::MasterWrite),
            "masterChangeStream$" => Some(Method::MasterChangeStream),
            "auth" => Some(Method::Auth),
            _ => None,
        }
    }
}

/// The server's answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    /// The request's id.
    pub id: String,
    /// The collection, echoed.
    pub collection: String,
    /// The method's return value, or `null` when there is none.
    pub result: Value,
}

/// A parsed frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    /// A client call.
    Request(Request),
    /// A server answer.
    Response(Response),
}

impl Frame {
    /// Serialises a frame with JavaScript number formatting, so a checkpoint inside a frame
    /// keeps its bytes (the whole protocol depends on that).
    pub fn to_json(&self) -> Value {
        match self {
            Frame::Request(request) => {
                let mut map = dxdb_json::Map::new();
                map.insert("id".to_string(), Value::String(request.id.clone()));
                map.insert(
                    "collection".to_string(),
                    Value::String(request.collection.clone()),
                );
                map.insert(
                    "method".to_string(),
                    Value::String(request.method.as_str().to_string()),
                );
                map.insert("params".to_string(), Value::Array(request.params.clone()));
                Value::Object(map)
            }
            Frame::Response(response) => {
                let mut map = dxdb_json::Map::new();
                map.insert("id".to_string(), Value::String(response.id.clone()));
                map.insert(
                    "collection".to_string(),
                    Value::String(response.collection.clone()),
                );
                map.insert("result".to_string(), response.result.clone());
                Value::Object(map)
            }
        }
    }

    /// Encodes a frame for the wire.
    pub fn encode(&self) -> String {
        dxdb_json::to_js_json(&self.to_json())
    }

    /// Parses a frame.
    ///
    /// A `result` key means a response; a `method` key means a request. Upstream distinguishes
    /// them the same way, and the two never collide because a response has no `method`.
    pub fn parse(text: &str) -> Result<Self, FrameError> {
        let value =
            dxdb_json::parse(text).map_err(|error| FrameError::Malformed(error.to_string()))?;
        let map = value
            .as_object()
            .ok_or_else(|| FrameError::Malformed("a frame is a JSON object".to_string()))?;
        let id = map
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| FrameError::Malformed("a frame needs an id".to_string()))?
            .to_string();
        let collection = map
            .get("collection")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        if let Some(method) = map.get("method").and_then(Value::as_str) {
            let method = Method::parse(method)
                .ok_or_else(|| FrameError::UnknownMethod(method.to_string()))?;
            let params = map
                .get("params")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            return Ok(Frame::Request(Request {
                id,
                collection,
                method,
                params,
            }));
        }

        Ok(Frame::Response(Response {
            id,
            collection,
            result: map.get("result").cloned().unwrap_or(Value::Null),
        }))
    }
}

/// Why a frame could not be handled.
///
/// A frame *parse* problem, not a transport problem: the bytes arrived but did not say what
/// they should. Transport failures use [`crate::adapters::TransportError`], so a caller can tell
/// "the socket died" from "the peer sent nonsense".
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// The frame was not valid JSON, or not the expected shape.
    #[error("malformed frame: {0}")]
    Malformed(String),
    /// The `method` string is not one the protocol serves.
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    /// The server answered with an error.
    #[error("server error: {0}")]
    Server(String),
}

// ---------------------------------------------------------------------------
// Encoding the three calls
// ---------------------------------------------------------------------------

/// Encodes `masterChangesSince(checkpoint, batchSize)`.
pub fn encode_changes_since(
    id: impl Into<String>,
    collection: &str,
    checkpoint: Option<&Checkpoint>,
    batch_size: usize,
) -> Frame {
    Frame::Request(Request {
        id: id.into(),
        collection: collection.to_string(),
        method: Method::MasterChangesSince,
        params: vec![
            checkpoint
                .map(|checkpoint| checkpoint.as_value().clone())
                .unwrap_or(Value::Null),
            Value::from(batch_size as u64),
        ],
    })
}

/// Encodes `masterWrite(rows)`.
///
/// Rows carry their attachment payloads inside the document, at `_attachments[id].data`, as a
/// **byte array** — exactly where a JS peer keeps its `Blob` and the only place upstream's
/// server looks. A transport that wanted base64 would substitute it here and decode on the
/// other side; the protocol above is indifferent either way, which is what keeps it from
/// leaking this adapter's choice.
pub fn encode_master_write(
    id: impl Into<String>,
    collection: &str,
    rows: &[WriteToMasterRow],
) -> Frame {
    let rows: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut map = dxdb_json::Map::new();
            map.insert(
                "assumedMasterState".to_string(),
                row.assumed_master_state
                    .clone()
                    .map(DocumentData::into_value)
                    .unwrap_or(Value::Null),
            );
            map.insert(
                "newDocumentState".to_string(),
                document_with_payloads(&row.new_document_state, &row.attachment_payloads),
            );
            Value::Object(map)
        })
        .collect();

    Frame::Request(Request {
        id: id.into(),
        collection: collection.to_string(),
        method: Method::MasterWrite,
        params: vec![Value::Array(rows)],
    })
}

/// Puts a row's attachment payloads into its document as `_attachments[id].data`.
fn document_with_payloads(document: &DocumentData, payloads: &BTreeMap<String, Vec<u8>>) -> Value {
    if payloads.is_empty() {
        return document.clone().into_value();
    }
    let mut out = document.clone();
    let mut attachments = out.attachments().cloned().unwrap_or_default();
    for (attachment_id, bytes) in payloads {
        let Some(stub) = attachments.get(attachment_id) else {
            continue;
        };
        let Some(map) = stub.as_object() else {
            continue;
        };
        let mut stub = map.clone();
        stub.insert(
            "data".to_string(),
            Value::Array(bytes.iter().map(|byte| Value::from(*byte as u64)).collect()),
        );
        attachments.insert(attachment_id.clone(), Value::Object(stub));
    }
    out.set("_attachments", Value::Object(attachments));
    out.into_value()
}

/// Decodes a `masterChangesSince` result.
pub fn decode_changes_since(result: &Value) -> Result<DocumentsWithCheckpoint, FrameError> {
    let documents = result
        .get("documents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|value| {
            DocumentData::from_value(value).map_err(|error| {
                FrameError::Malformed(format!("a document is not an object: {error}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let checkpoint = result
        .get("checkpoint")
        .filter(|value| !value.is_null())
        .map(|value| Checkpoint::new(value.clone()))
        .transpose()
        .map_err(|error| FrameError::Malformed(error.to_string()))?;
    Ok(DocumentsWithCheckpoint {
        documents,
        checkpoint,
    })
}

/// Decodes a `masterWrite` result: the conflicts, and nothing else.
///
/// A server that returned acknowledgements instead would produce a client that re-resolves
/// conflicts forever; [`decode_conflicts`] exists so that mistake has a name.
pub fn decode_master_write(result: &Value) -> Result<Vec<DocumentData>, FrameError> {
    decode_conflicts(result)
}

/// The conflicts a `masterWrite` answered with.
pub fn decode_conflicts(result: &Value) -> Result<Vec<DocumentData>, FrameError> {
    let conflicts = result
        .get("conflicts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    conflicts
        .into_iter()
        .map(|value| {
            DocumentData::from_value(value).map_err(|error| {
                FrameError::Malformed(format!("a conflict is not an object: {error}"))
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// A server over any handler
// ---------------------------------------------------------------------------

/// Serves the three-method RPC away from a [`ReplicationHandler`].
///
/// The whole server side: match the method, check the arity, call, encode. Upstream's
/// `websocket-server.ts` does exactly this after resolving the collection to a handler
/// (`:110-135`).
pub async fn serve(handler: &dyn ReplicationHandler, frame: Frame) -> Frame {
    let Frame::Request(request) = frame else {
        // A response sent to a server is a client bug; answer it rather than panicking.
        return Frame::Response(Response {
            id: String::new(),
            collection: String::new(),
            result: error_result("a server does not accept responses"),
        });
    };

    let result = match request.method {
        Method::MasterChangesSince => {
            if request.params.len() != 2 {
                return Frame::Response(Response {
                    id: request.id,
                    collection: request.collection,
                    result: error_result(&format!(
                        "masterChangesSince expects 2 parameters, got {}",
                        request.params.len()
                    )),
                });
            }
            let checkpoint = match request.params[0] {
                Value::Null => None,
                ref value => match Checkpoint::new(value.clone()) {
                    Ok(checkpoint) => Some(checkpoint),
                    Err(error) => {
                        return Frame::Response(Response {
                            id: request.id,
                            collection: request.collection,
                            result: error_result(&error.to_string()),
                        })
                    }
                },
            };
            let batch_size = request.params[1].as_u64().unwrap_or(100) as usize;
            match handler
                .master_changes_since(checkpoint.as_ref(), batch_size)
                .await
            {
                Ok(pulled) => {
                    let mut map = dxdb_json::Map::new();
                    map.insert(
                        "documents".to_string(),
                        Value::Array(
                            pulled
                                .documents
                                .into_iter()
                                .map(document_for_wire)
                                .collect(),
                        ),
                    );
                    map.insert(
                        "checkpoint".to_string(),
                        pulled
                            .checkpoint
                            .map(|checkpoint| checkpoint.into_value())
                            .unwrap_or(Value::Null),
                    );
                    Value::Object(map)
                }
                Err(error) => error_result(&error.to_string()),
            }
        }
        Method::MasterWrite => {
            if request.params.len() != 1 {
                return Frame::Response(Response {
                    id: request.id,
                    collection: request.collection,
                    result: error_result(&format!(
                        "masterWrite expects 1 parameter, got {}",
                        request.params.len()
                    )),
                });
            }
            let Some(rows) = request.params[0].as_array() else {
                return Frame::Response(Response {
                    id: request.id,
                    collection: request.collection,
                    result: error_result("masterWrite expects an array of rows"),
                });
            };
            let rows: Vec<WriteToMasterRow> = rows.iter().map(decode_write_row).collect();
            match handler.master_write(rows).await {
                Ok(conflicts) => {
                    let mut map = dxdb_json::Map::new();
                    map.insert(
                        "conflicts".to_string(),
                        Value::Array(conflicts.into_iter().map(document_for_wire).collect()),
                    );
                    Value::Object(map)
                }
                Err(error) => error_result(&error.to_string()),
            }
        }
        Method::Auth => {
            // The format carries it; the protocol has no notion of it, so the honest answer is
            // that this server does not authenticate rather than a false `true`.
            error_result("this server does not implement auth")
        }
        Method::MasterChangeStream => error_result(
            "masterChangeStream$ is a stream, not an RPC; a transport delivers stream items as \
             frames rather than calling it",
        ),
    };

    Frame::Response(Response {
        id: request.id,
        collection: request.collection,
        result,
    })
}

/// Reads a document off the wire, lifting attachment payloads back into the write row.
fn decode_write_row(value: &Value) -> WriteToMasterRow {
    let assumed_master_state = value
        .get("assumedMasterState")
        .filter(|value| !value.is_null())
        .and_then(|value| DocumentData::from_value(value.clone()).ok());
    let new_document_state = value
        .get("newDocumentState")
        .and_then(|value| DocumentData::from_value(value.clone()).ok())
        .unwrap_or_else(|| {
            DocumentData::from_value(Value::Object(dxdb_json::Map::new()))
                .expect("an empty object is a document")
        });
    let (new_document_state, attachment_payloads) = lift_payloads(new_document_state);
    WriteToMasterRow::with_payloads(
        assumed_master_state,
        new_document_state,
        attachment_payloads,
    )
}

/// Moves `_attachments[id].data` out of a wire document and into the payload map.
pub fn lift_payloads(mut document: DocumentData) -> (DocumentData, BTreeMap<String, Vec<u8>>) {
    let mut payloads = BTreeMap::new();
    let Some(attachments) = document.attachments().cloned() else {
        return (document, payloads);
    };
    let mut stripped = attachments.clone();
    for (attachment_id, stub) in &attachments {
        let Some(map) = stub.as_object() else {
            continue;
        };
        let Some(Value::Array(bytes)) = map.get("data") else {
            continue;
        };
        payloads.insert(
            attachment_id.clone(),
            bytes
                .iter()
                .filter_map(Value::as_u64)
                .map(|byte| byte as u8)
                .collect(),
        );
        let mut stub = map.clone();
        stub.remove("data");
        stripped.insert(attachment_id.clone(), Value::Object(stub));
    }
    if !payloads.is_empty() {
        document.set("_attachments", Value::Object(stripped));
    }
    (document, payloads)
}

/// A document as it goes on the wire: `_attachments` present, so the other side can tell
/// "no attachments" from "the field was omitted".
fn document_for_wire(document: DocumentData) -> Value {
    let mut document = document;
    if document.get("_attachments").is_none() {
        document.set("_attachments", Value::Object(dxdb_json::Map::new()));
    }
    document.into_value()
}

fn error_result(message: &str) -> Value {
    let mut map = dxdb_json::Map::new();
    map.insert("error".to_string(), Value::String(message.to_string()));
    Value::Object(map)
}

/// True when a `masterChangesSince` or `masterWrite` result is a server-side error.
pub fn result_error(result: &Value) -> Option<&str> {
    result.get("error").and_then(Value::as_str)
}

/// Adapts a caller-supplied frame sender into a [`ReplicationHandler`].
///
/// This is the piece that keeps the adapter's specifics out of the protocol: the protocol sees
/// three methods, and this maps each to one frame over whatever channel the caller supplies.
/// The channel is a function so a test can hand in an in-process pair and a binding can hand in
/// a socket without the protocol knowing which.
pub struct WebSocketHandler<F>
where
    F: Fn(Frame) -> Result<Frame, SendError>,
{
    collection: String,
    send: F,
    next_id: std::cell::Cell<u64>,
    /// The change stream, fed by frames the caller routes here.
    stream: crate::state::PullChannel,
}

impl<F> WebSocketHandler<F>
where
    F: Fn(Frame) -> Result<Frame, SendError>,
{
    /// Builds a handler over a frame channel.
    pub fn new(collection: impl Into<String>, send: F) -> Self {
        WebSocketHandler {
            collection: collection.into(),
            send,
            next_id: std::cell::Cell::new(0),
            stream: crate::state::PullChannel::new(),
        }
    }

    /// The channel stream items should be pushed into when the transport delivers them.
    pub fn stream_channel(&self) -> &crate::state::PullChannel {
        &self.stream
    }

    fn next_request_id(&self) -> String {
        let next = self.next_id.get() + 1;
        self.next_id.set(next);
        // Upstream uses a random token; a counter is equally unique within a connection and
        // makes a frame dump readable.
        format!("r{next}")
    }

    fn call(&self, frame: Frame) -> Result<Value, SendError> {
        let expected_id = match &frame {
            Frame::Request(request) => request.id.clone(),
            Frame::Response(response) => response.id.clone(),
        };
        let response = (self.send)(frame)?;
        match response {
            Frame::Response(response) => {
                if response.id != expected_id {
                    return Err(SendError::Frame(FrameError::Server(format!(
                        "response id {} does not match request id {expected_id}",
                        response.id
                    ))));
                }
                if let Some(error) = result_error(&response.result) {
                    return Err(SendError::Frame(FrameError::Server(error.to_string())));
                }
                Ok(response.result)
            }
            Frame::Request(_) => Err(SendError::Frame(FrameError::Server(
                "expected a response, got a request".to_string(),
            ))),
        }
    }
}

impl<F> ReplicationHandler for WebSocketHandler<F>
where
    F: Fn(Frame) -> Result<Frame, SendError>,
{
    fn master_changes_since(
        &self,
        checkpoint: Option<&Checkpoint>,
        batch_size: usize,
    ) -> crate::state::BoxFuture<'_, Result<DocumentsWithCheckpoint, StorageError>> {
        let id = self.next_request_id();
        let frame = encode_changes_since(id, &self.collection, checkpoint, batch_size);
        Box::pin(async move {
            let result = self.call(frame).map_err(to_storage_error)?;
            decode_changes_since(&result).map_err(|error| to_storage_error(error.into()))
        })
    }

    fn master_write(
        &self,
        rows: Vec<WriteToMasterRow>,
    ) -> crate::state::BoxFuture<'_, Result<Vec<DocumentData>, StorageError>> {
        let id = self.next_request_id();
        let frame = encode_master_write(id, &self.collection, &rows);
        Box::pin(async move {
            let result = self.call(frame).map_err(to_storage_error)?;
            decode_master_write(&result).map_err(|error| to_storage_error(error.into()))
        })
    }

    fn master_change_stream(&self) -> PullStream {
        self.stream.stream()
    }
}

/// What can go wrong sending one frame and reading its answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// The frame could not cross: no connection, a closed socket, a write failure.
    #[error("transport: {0}")]
    Transport(String),
    /// The answer arrived but was not a frame this adapter understands.
    #[error("frame: {0}")]
    Frame(#[from] FrameError),
}

impl From<crate::adapters::TransportError> for SendError {
    fn from(error: crate::adapters::TransportError) -> Self {
        SendError::Transport(error.message())
    }
}

fn to_storage_error(error: SendError) -> StorageError {
    StorageError::Unsupported(format!("websocket: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(json: &str) -> Checkpoint {
        Checkpoint::new(dxdb_json::parse(json).expect("json")).expect("object")
    }

    #[test]
    fn a_changes_since_request_matches_upstreams_frame() {
        let frame = encode_changes_since(
            "abc",
            "human",
            Some(&checkpoint(r#"{"id":"doc-1","lwt":1.5}"#)),
            20,
        );
        assert_eq!(
            frame.encode(),
            r#"{"id":"abc","collection":"human","method":"masterChangesSince","params":[{"id":"doc-1","lwt":1.5},20]}"#,
            "field names, order and the JS number formatting of the checkpoint"
        );
    }

    #[test]
    fn a_frame_round_trips_through_its_wire_form() {
        let frame = encode_changes_since("abc", "human", Some(&checkpoint(r#"{"seq":7}"#)), 5);
        let parsed = Frame::parse(&frame.encode()).expect("parse");
        assert_eq!(parsed, frame);
    }

    #[test]
    fn a_response_is_distinguishable_from_a_request() {
        let response = Frame::Response(Response {
            id: "abc".to_string(),
            collection: "human".to_string(),
            result: Value::Null,
        });
        let parsed = Frame::parse(&response.encode()).expect("parse");
        assert_eq!(parsed, response);
    }

    #[test]
    fn an_unknown_method_is_reported_rather_than_ignored() {
        let error = Frame::parse(r#"{"id":"a","collection":"c","method":"nope","params":[]}"#)
            .expect_err("must fail");
        assert_eq!(error, FrameError::UnknownMethod("nope".to_string()));
    }

    #[test]
    fn a_write_row_carries_its_payload_inside_the_document() {
        let mut document = DocumentData::from_value(
            dxdb_json::parse(
                r#"{"passportId":"a","_attachments":{"f":{"length":2,"digest":"d","type":"text/plain"}}}"#,
            )
            .expect("json"),
        )
        .expect("object");
        document.set("_rev", Value::String("1-t".to_string()));
        let mut payloads = BTreeMap::new();
        payloads.insert("f".to_string(), vec![104u8, 105u8]);
        let row = WriteToMasterRow::with_payloads(None, document, payloads);

        let frame = encode_master_write("id1", "human", &[row]);
        let encoded = frame.encode();
        assert!(
            encoded.contains(r#""data":[104,105]"#),
            "the payload must be at _attachments[id].data, where a JS peer looks: {encoded}"
        );

        // And a receipt lifts it back out, so the storage layer sees payloads beside the
        // document rather than inside its JSON.
        let Frame::Request(request) = frame else {
            panic!("expected a request")
        };
        let rows = request.params[0].as_array().expect("rows");
        let decoded = decode_write_row(&rows[0]);
        assert!(decoded.new_document_state.get("_attachments").is_some());
        assert_eq!(
            decoded
                .new_document_state
                .attachment("f")
                .and_then(|stub| stub.get("data")),
            None,
            "the lift removes data from the stub"
        );
        assert_eq!(
            decoded.attachment_payloads.get("f"),
            Some(&vec![104u8, 105u8])
        );
    }

    #[test]
    fn a_document_without_payloads_is_sent_unchanged() {
        let document = DocumentData::from_value(
            dxdb_json::parse(r#"{"passportId":"a","_attachments":{}}"#).expect("json"),
        )
        .expect("object");
        let row = WriteToMasterRow::new(None, document);
        let frame = encode_master_write("id1", "human", &[row]);
        let encoded = frame.encode();
        assert!(
            !encoded.contains(r#""data""#),
            "no payloads means no data key: {encoded}"
        );
    }

    #[test]
    fn decoding_conflicts_reads_only_the_conflicts_field() {
        // A well-behaved server.
        let result = dxdb_json::parse(r#"{"conflicts":[{"passportId":"a","_deleted":false}]}"#)
            .expect("json");
        let conflicts = decode_conflicts(&result).expect("decode");
        assert_eq!(conflicts.len(), 1);

        // A server that returned acknowledgements instead.
        let wrong = dxdb_json::parse(r#"{"written":[{"passportId":"a"}]}"#).expect("json");
        assert!(
            decode_conflicts(&wrong).expect("decode").is_empty(),
            "masterWrite returning acknowledgements must decode to no conflicts, which is why \
             the adapter asserts on the field name rather than the array length"
        );
    }
}
