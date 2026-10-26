//! The CouchDB replication adapter.
//!
//! Ports `src/plugins/replication-couchdb/` (475 lines: `index.ts` 341,
//! `couchdb-helper.ts` 100, `couchdb-types.ts` 34).
//!
//! # The checkpoint is `{ sequence }`, and that shapes everything else
//!
//! This is the first adapter whose checkpoint is **not** a document position. CouchDB's
//! `_changes` feed is a sequence log: `last_seq` is an opaque token CouchDB assigns, not an id
//! plus a timestamp. That matters in three ways:
//!
//! 1. **It is still a JSON object**, so D7's opaque-checkpoint rule holds without change. The
//!    protocol stacks and compares it as bytes; it never reads `sequence`.
//! 2. **`since=0` is the beginning.** Upstream sends `since: 0` when there is no checkpoint
//!    (`index.ts:114`) rather than omitting the parameter, so a fresh replication and a
//!    replication whose checkpoint `sequence` happens to be `0` are the same request.
//! 3. **A checkpoint can move without documents.** `_changes` returns the current `last_seq`
//!    even with an empty `results`, and upstream *does* persist it
//!    (`index.ts:131-133`). That is the opposite of the protocol's own rule for its internal
//!    checkpoint ("do not advance on an empty batch"), and it is correct here: the sequence log
//!    has advanced, and not recording it means re-reading the same tail forever. The two rules
//!    coexist because the protocol persists *its* checkpoint from the batch it applied, while
//!    this adapter hands the protocol a `{sequence}` value that already moved.
//!
//! # Push: read-then-write, twice
//!
//! CouchDB has no compare-and-set on a batch. Upstream's `masterWrite` therefore:
//!
//! 1. `_all_docs?keys=[...]&include_docs=true` to learn the current `_rev` of every row
//!    (`index.ts:160-171`);
//! 2. treats a row as a conflict when `assumedMasterState` is absent or not equal to the real
//!    state (`index.ts:181-193`) — the same four rules every adapter applies, done by hand;
//! 3. `_bulk_docs` with the `_rev` it just read (`index.ts:200-219`);
//! 4. re-reads any document `_bulk_docs` reports as `error: "conflict"`, because the rev moved
//!    in between (`index.ts:226-247`).
//!
//! Step 4 is the interesting one: it is a genuine race CouchDB exposes and the protocol does
//! not. A port that skipped it would drop a conflict whenever two writers interleaved between
//! steps 1 and 3, and the document would silently diverge.
//!
//! # `_id` and the primary path are swapped at the boundary
//!
//! CouchDB's id field is `_id`; RxDB's is whatever the schema says. Upstream swaps them on the
//! way in and out (`couchdb-helper.ts:22-71`) and strips `_rev` from the document it hands the
//! protocol, because the revision belongs to CouchDB's own tree. [`couch_doc_to_rx_doc`] and
//! [`rx_doc_to_couch_doc`] are those functions.
//!
//! # What is not here
//!
//! Auth (`getFetchWithCouchDBAuthorization`, `couchdb-helper.ts:90`), the live `_changes`
//! long-poll feed with `feed=longpoll`/`heartbeat`, and connection retry. Auth is a header on a
//! request the caller supplies; the live feed is a subscription the caller drives (D2/D3).

use std::collections::BTreeMap;

use dxdb_json::Value;
use dxdb_storage::{DocumentData, StorageError};

use crate::adapters::TransportError;
use crate::checkpoint::Checkpoint;
use crate::state::{DocumentsWithCheckpoint, PullStream, ReplicationHandler, WriteToMasterRow};

/// How the adapter reaches CouchDB.
///
/// One method, because every CouchDB call is an HTTP request the caller must be able to shape:
/// auth headers, a proxy, a test double. The protocol never sees this trait — it is used only
/// inside [`CouchDbHandler`].
///
/// The callback shape keeps `!Send` intact: `ReplicationHandler` is single-threaded (D1), and a
/// boxed `Fn` returning a boxed future is the same shape the rest of the crate uses.
pub trait HttpClient {
    /// Performs a request and returns `(status, body)`.
    ///
    /// The response is not parsed here: CouchDB returns 4xx with a JSON body that carries the
    /// reason, and an adapter that only saw an `Err` would lose it.
    fn request(
        &self,
        method: HttpMethod,
        url: String,
        body: Option<Value>,
    ) -> crate::state::BoxFuture<'_, Result<HttpResponse, TransportError>>;
}

/// The HTTP verbs this adapter uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
}

/// A status and a parsed body.
#[derive(Clone, Debug, PartialEq)]
pub struct HttpResponse {
    /// The HTTP status.
    pub status: u16,
    /// The parsed JSON body. `Value::Null` for an empty body.
    pub body: Value,
}

impl HttpResponse {
    /// True for a 2xx status.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Converts a non-success status into a [`TransportError::Status`].
    pub fn require_success(self) -> Result<Value, TransportError> {
        if self.is_success() {
            Ok(self.body)
        } else {
            Err(TransportError::Status {
                status: self.status,
                body: dxdb_json::to_js_json(&self.body),
            })
        }
    }
}

/// CouchDB's `_changes` response, as far as this adapter reads it.
///
/// `results[].doc` may be absent for a row whose document was deleted and purged, which
/// upstream tolerates by only mapping rows that have a doc (`index.ts:124-127`).
fn decode_changes_response(body: &Value) -> Result<(Vec<DocumentData>, Value), TransportError> {
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            // `RC_COUCHDB_2`: a `_changes` response with no `results` array is not a feed.
            TransportError::Malformed(
                "CouchDB _changes response has no results array (upstream RC_COUCHDB_2)"
                    .to_string(),
            )
        })?;
    let last_seq = body.get("last_seq").cloned().unwrap_or(Value::from(0u64));

    let mut documents = Vec::new();
    for row in results {
        let Some(doc) = row.get("doc").filter(|doc| !doc.is_null()) else {
            continue;
        };
        documents.push(
            DocumentData::from_value(doc.clone())
                .map_err(|error| TransportError::Malformed(error.to_string()))?,
        );
    }
    Ok((documents, last_seq))
}

/// CouchDB's `_all_docs` response: the current `_rev` per requested id.
fn decode_all_docs(body: &Value) -> Result<Vec<(String, Option<DocumentData>)>, TransportError> {
    let rows = body
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| TransportError::Malformed("_all_docs response has no rows".to_string()))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row
            .get("id")
            .or_else(|| row.get("key"))
            .and_then(Value::as_str)
            .ok_or_else(|| TransportError::Malformed("an _all_docs row has no id".to_string()))?
            .to_string();
        let doc = row
            .get("doc")
            .filter(|doc| !doc.is_null())
            .map(|doc| DocumentData::from_value(doc.clone()))
            .transpose()
            .map_err(|error| TransportError::Malformed(error.to_string()))?;
        out.push((id, doc));
    }
    Ok(out)
}

/// Swaps CouchDB's `_id` onto the schema's primary path and strips `_rev`.
///
/// Ports `couchDBDocToRxDocData` (`couchdb-helper.ts:22-38`):
///
/// * `_id` becomes the primary value, unless the schema's primary key already *is* `_id` or the
///   document already carries its primary field (the optimisation at
///   `couchdb-helper.ts:48-50`);
/// * `_deleted` is coerced to a boolean, because CouchDB omits it entirely for a live document;
/// * `_rev` is dropped. It is CouchDB's revision, meaningless to the protocol, and keeping it
///   would make `isEqual` see a difference on every pull.
pub fn couch_doc_to_rx_doc(primary_path: &str, couch_doc: &Value) -> DocumentData {
    let mut document =
        DocumentData::from_value(couch_doc.clone()).unwrap_or_else(|_| DocumentData::default());
    let already_has_primary = document.get(primary_path).is_some();
    if primary_path != "_id" && !already_has_primary {
        if let Some(id) = document.get("_id").cloned() {
            document.set(primary_path, id);
        }
    }
    document.as_object_mut().remove("_id");
    document.set(
        "_deleted",
        Value::Bool(
            document
                .get("_deleted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    document.as_object_mut().remove("_rev");
    document
}

/// Swaps the schema's primary value onto `_id`, for a write.
///
/// Ports `couchSwapPrimaryToId` (`couchdb-helper.ts:55-71`). `_id` is CouchDB's field; leaving
/// the primary key in place as well would store it twice and make every subsequent doc compare
/// unequal.
pub fn rx_doc_to_couch_doc(primary_path: &str, document: &DocumentData) -> DocumentData {
    if primary_path == "_id" {
        return document.clone();
    }
    let mut out = document.clone();
    let id = out.get(primary_path).cloned();
    out.as_object_mut().remove(primary_path);
    if let Some(id) = id {
        out.set("_id", id);
    }
    out
}

/// The CouchDB replication adapter.
///
/// Holds the base URL and the client. Every call is one or two HTTP requests; the http client
/// owns auth, proxying and retries.
pub struct CouchDbHandler<C: HttpClient> {
    /// The database URL, **with a trailing slash**: upstream concatenates `'_changes'` onto it
    /// (`index.ts:110`), so `http://host/db/` is correct and `http://host/db` produces
    /// `http://host/db_changes`.
    base_url: String,
    primary_path: String,
    client: C,
    /// A named replication identifier is not needed by CouchDB itself, but keeping it here
    /// documents that two replications against one database share no state on the server.
    _replication_identifier: String,
}

impl<C: HttpClient> CouchDbHandler<C> {
    /// Builds the adapter.
    ///
    /// `base_url` must end with `/`; it is corrected rather than rejected, because a caller
    /// forgetting it is the single most likely mistake and the failure mode (a `404` from
    /// `/_changes`) is confusing.
    pub fn new(base_url: impl Into<String>, primary_path: impl Into<String>, client: C) -> Self {
        let mut base_url = base_url.into();
        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        CouchDbHandler {
            base_url,
            primary_path: primary_path.into(),
            client,
            _replication_identifier: String::new(),
        }
    }

    /// Sets the replication identifier (documentation only; CouchDB keeps no per-replication
    /// state on the server).
    pub fn with_identifier(mut self, identifier: impl Into<String>) -> Self {
        self._replication_identifier = identifier.into();
        self
    }

    /// The base URL, with its trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn changes_url(&self, since: &Value, batch_size: usize) -> String {
        // Upstream's exact query (`index.ts:110-118`). `style=all_docs` and `include_docs=true`
        // are load-bearing: without them the feed carries revisions only, with no document.
        // `since` is always sent, `0` when there is no checkpoint.
        let since = match since {
            Value::Number(number) => dxdb_json::to_js_json(&Value::Number(number.clone())),
            Value::String(text) => text.clone(),
            // A CouchDB sequence can be a string ("1-g1AAAA...") on clustered servers.
            _ => "0".to_string(),
        };
        format!(
            "{}_changes?style=all_docs&feed=normal&include_docs=true&since={}&limit={}&seq_interval={}",
            self.base_url, since, batch_size, batch_size
        )
    }

    fn all_docs_url(&self) -> String {
        format!("{}_all_docs?include_docs=true", self.base_url)
    }

    fn bulk_docs_url(&self) -> String {
        format!("{}_bulk_docs", self.base_url)
    }
}

impl<C: HttpClient> ReplicationHandler for CouchDbHandler<C> {
    fn master_changes_since(
        &self,
        checkpoint: Option<&Checkpoint>,
        batch_size: usize,
    ) -> crate::state::BoxFuture<'_, Result<DocumentsWithCheckpoint, StorageError>> {
        // The checkpoint is `{sequence}`. `0` when absent, matching `since: 0`
        // (`index.ts:114`).
        let since = checkpoint
            .and_then(|checkpoint| checkpoint.as_map().get("sequence").cloned())
            .unwrap_or(Value::from(0u64));
        let url = self.changes_url(&since, batch_size);

        Box::pin(async move {
            let response = self
                .client
                .request(HttpMethod::Get, url, None)
                .await
                .map_err(crate::adapters::to_storage_error)?
                .require_success()
                .map_err(crate::adapters::to_storage_error)?;

            let (documents, last_seq) =
                decode_changes_response(&response).map_err(crate::adapters::to_storage_error)?;

            let documents = documents
                .iter()
                .map(|document| couch_doc_to_rx_doc(&self.primary_path, document.as_value()))
                .collect::<Vec<_>>();

            // The checkpoint moves even when `documents` is empty: `last_seq` is CouchDB's
            // sequence log position, not a document position. See the module docs.
            let mut map = dxdb_json::Map::new();
            map.insert("sequence".to_string(), last_seq);
            let checkpoint = Checkpoint::new(Value::Object(map))
                .map_err(|error| StorageError::Unsupported(error.to_string()))?;

            Ok(DocumentsWithCheckpoint {
                documents,
                checkpoint: Some(checkpoint),
            })
        })
    }

    fn master_write(
        &self,
        rows: Vec<WriteToMasterRow>,
    ) -> crate::state::BoxFuture<'_, Result<Vec<DocumentData>, StorageError>> {
        Box::pin(async move {
            if rows.is_empty() {
                return Ok(Vec::new());
            }

            let primary_path = &self.primary_path;

            // --- 1. learn the current _rev of every row ---------------------------
            let keys: Vec<Value> = rows
                .iter()
                .filter_map(|row| row.new_document_state.get(primary_path).cloned())
                .collect();
            let mut body = dxdb_json::Map::new();
            body.insert("keys".to_string(), Value::Array(keys));
            body.insert("include_docs".to_string(), Value::Bool(true));
            // `deleted: 'ok'` so a tombstone's rev comes back rather than a missing row.
            body.insert("deleted".to_string(), Value::String("ok".to_string()));

            let current = self
                .client
                .request(
                    HttpMethod::Post,
                    self.all_docs_url(),
                    Some(Value::Object(body)),
                )
                .await
                .map_err(crate::adapters::to_storage_error)?
                .require_success()
                .map_err(crate::adapters::to_storage_error)?;
            let current = decode_all_docs(&current).map_err(crate::adapters::to_storage_error)?;

            let mut remote_rev: BTreeMap<String, String> = BTreeMap::new();
            let mut remote_state: BTreeMap<String, DocumentData> = BTreeMap::new();
            for (id, doc) in current {
                if let Some(doc) = doc {
                    if let Some(rev) = doc.get("_rev").and_then(Value::as_str) {
                        remote_rev.insert(id.clone(), rev.to_string());
                    }
                    remote_state.insert(
                        id.clone(),
                        couch_doc_to_rx_doc(primary_path, doc.as_value()),
                    );
                }
            }

            // --- 2. the four conflict rules, by hand ------------------------------
            //
            // The protocol's rules assume the master can compare-and-set for us. CouchDB
            // cannot, so each row is classified here (`index.ts:181-193`).
            let mut conflicts: Vec<DocumentData> = Vec::new();
            let mut accepted: Vec<&WriteToMasterRow> = Vec::new();
            for row in &rows {
                let Some(id) = row
                    .new_document_state
                    .get(primary_path)
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                match remote_state.get(id) {
                    None => accepted.push(row),
                    Some(real) => {
                        let equal = row
                            .assumed_master_state
                            .as_ref()
                            .is_some_and(|assumed| content_equal(real, assumed));
                        if equal {
                            accepted.push(row);
                        } else {
                            conflicts.push(real.clone());
                        }
                    }
                }
            }

            // --- 3. _bulk_docs ----------------------------------------------------
            if !accepted.is_empty() {
                let mut docs = Vec::with_capacity(accepted.len());
                for row in &accepted {
                    let id = row
                        .new_document_state
                        .get(primary_path)
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let mut send = rx_doc_to_couch_doc(primary_path, &row.new_document_state);
                    // Attach the rev we just read, which is what makes this an update rather
                    // than a conflicting insert (`index.ts:203-207`).
                    if let Some(rev) = remote_rev.get(id) {
                        send.set("_rev", Value::String(rev.clone()));
                    }
                    docs.push(send.into_value());
                }
                let mut body = dxdb_json::Map::new();
                body.insert("docs".to_string(), Value::Array(docs));

                let response = self
                    .client
                    .request(
                        HttpMethod::Post,
                        self.bulk_docs_url(),
                        Some(Value::Object(body)),
                    )
                    .await
                    .map_err(crate::adapters::to_storage_error)?
                    .require_success()
                    .map_err(crate::adapters::to_storage_error)?;

                // --- 4. a rev that moved between 1 and 3 comes back as a conflict --
                //
                // `_bulk_docs` answers per row: `{ok}`, or `{error: "conflict"}` when the rev
                // changed under us. Anything else is a real failure, not a race.
                let rows_out = response
                    .as_array()
                    .ok_or_else(|| {
                        StorageError::Unsupported(
                            "CouchDB _bulk_docs did not return an array".to_string(),
                        )
                    })?
                    .clone();
                let mut raced: Vec<String> = Vec::new();
                for entry in &rows_out {
                    let ok = entry.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    let is_conflict =
                        entry.get("error").and_then(Value::as_str) == Some("conflict");
                    if ok {
                        continue;
                    }
                    if is_conflict {
                        if let Some(id) = entry.get("id").and_then(Value::as_str) {
                            raced.push(id.to_string());
                        }
                        continue;
                    }
                    return Err(StorageError::Unsupported(format!(
                        "CouchDB _bulk_docs reported an unexpected failure: {}",
                        dxdb_json::to_js_json(entry)
                    )));
                }

                // Re-read the raced ids so the protocol gets the real master state and can
                // resolve (`index.ts:226-247`).
                if !raced.is_empty() {
                    let mut body = dxdb_json::Map::new();
                    body.insert(
                        "keys".to_string(),
                        Value::Array(raced.iter().cloned().map(Value::String).collect()),
                    );
                    body.insert("include_docs".to_string(), Value::Bool(true));
                    let again = self
                        .client
                        .request(
                            HttpMethod::Post,
                            self.all_docs_url(),
                            Some(Value::Object(body)),
                        )
                        .await
                        .map_err(crate::adapters::to_storage_error)?
                        .require_success()
                        .map_err(crate::adapters::to_storage_error)?;
                    for (_, doc) in
                        decode_all_docs(&again).map_err(crate::adapters::to_storage_error)?
                    {
                        if let Some(doc) = doc {
                            conflicts.push(couch_doc_to_rx_doc(primary_path, doc.as_value()));
                        }
                    }
                }
            }

            Ok(conflicts)
        })
    }

    fn master_change_stream(&self) -> PullStream {
        // The live `_changes` feed is `feed=longpoll` with a `heartbeat`, which needs a
        // long-lived connection and a clock. That is a binding's job (D2/D3); a caller wanting
        // live behaviour polls [`Self::master_changes_since`] on its own schedule.
        Box::pin(futures::stream::empty())
    }
}

/// Content equality, ignoring CouchDB's and RxDB's local bookkeeping.
///
/// Mirrors what the protocol's `isEqual` does, which is what upstream passes as
/// `conflictHandler.isEqual` at `index.ts:186`.
fn content_equal(a: &DocumentData, b: &DocumentData) -> bool {
    fn projection(document: &DocumentData) -> Value {
        let mut map = dxdb_json::Map::new();
        for (key, value) in document.as_object() {
            if matches!(key.as_str(), "_rev" | "_meta" | "_id") {
                continue;
            }
            if key == "_attachments"
                && value
                    .as_object()
                    .is_some_and(|attachments| attachments.is_empty())
            {
                continue;
            }
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }
    projection(a) == projection(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(json: &str) -> DocumentData {
        DocumentData::from_value(dxdb_json::parse(json).expect("json")).expect("object")
    }

    #[test]
    fn _id_becomes_the_primary_path_and_rev_is_stripped() {
        let couch = doc(r#"{"_id":"a","_rev":"1-x","firstName":"A","_deleted":false}"#);
        let rx = couch_doc_to_rx_doc("passportId", couch.as_value());
        assert_eq!(rx.get("passportId").and_then(Value::as_str), Some("a"));
        assert!(rx.get("_id").is_none(), "_id must not survive the swap");
        assert!(rx.get("_rev").is_none(), "_rev is CouchDB's, not ours");
    }

    #[test]
    fn a_document_already_carrying_its_primary_keeps_it() {
        // `couchdb-helper.ts:48-50`: the optimisation shortcut.
        let couch = doc(r#"{"_id":"a","passportId":"a","_deleted":false}"#);
        let rx = couch_doc_to_rx_doc("passportId", couch.as_value());
        assert_eq!(rx.get("passportId").and_then(Value::as_str), Some("a"));
    }

    #[test]
    fn a_missing_deleted_flag_becomes_false() {
        // CouchDB omits `_deleted` for a live document, so a pull would otherwise hand the
        // protocol a document with no flag at all.
        let couch = doc(r#"{"_id":"a"}"#);
        let rx = couch_doc_to_rx_doc("passportId", couch.as_value());
        assert_eq!(rx.get("_deleted").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn a_write_document_gets_id_instead_of_the_primary_path() {
        let rx = doc(r#"{"passportId":"a","firstName":"A","_deleted":false}"#);
        let couch = rx_doc_to_couch_doc("passportId", &rx);
        assert_eq!(couch.get("_id").and_then(Value::as_str), Some("a"));
        assert!(
            couch.get("passportId").is_none(),
            "the primary path must not be sent"
        );
    }

    #[test]
    fn a_changes_response_without_results_is_an_error() {
        let body = dxdb_json::parse(r#"{"last_seq":5}"#).expect("json");
        let error = decode_changes_response(&body).expect_err("must fail");
        assert!(
            error.message().contains("RC_COUCHDB_2"),
            "the error must name the upstream code, got {}",
            error.message()
        );
    }

    #[test]
    fn a_changes_row_without_a_doc_is_skipped() {
        let body = dxdb_json::parse(
            r#"{"results":[{"id":"a"},{"id":"b","doc":{"_id":"b"}}],"last_seq":9}"#,
        )
        .expect("json");
        let (documents, last_seq) = decode_changes_response(&body).expect("decode");
        assert_eq!(documents.len(), 1, "a purged row contributes no document");
        assert_eq!(last_seq, Value::from(9u64));
    }

    #[test]
    fn the_base_url_gets_its_trailing_slash() {
        struct NoClient;
        impl HttpClient for NoClient {
            fn request(
                &self,
                _method: HttpMethod,
                _url: String,
                _body: Option<Value>,
            ) -> crate::state::BoxFuture<'_, Result<HttpResponse, TransportError>> {
                Box::pin(async { Err(TransportError::Unreachable("test".to_string())) })
            }
        }
        let handler = CouchDbHandler::new("http://localhost:5984/db", "passportId", NoClient);
        assert_eq!(handler.base_url(), "http://localhost:5984/db/");
        // And the query is upstream's exact set (`index.ts:110-118`).
        let url = handler.changes_url(&Value::from(0u64), 25);
        assert!(url.contains("style=all_docs"), "{url}");
        assert!(url.contains("feed=normal"), "{url}");
        assert!(url.contains("include_docs=true"), "{url}");
        assert!(url.contains("since=0"), "{url}");
        assert!(url.contains("limit=25"), "{url}");
        assert!(url.contains("seq_interval=25"), "{url}");
    }
}
