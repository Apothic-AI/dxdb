//! The GraphQL replication adapter.
//!
//! Ports `src/plugins/replication-graphql/index.ts` (255 lines) and `helper.ts` (51 lines).
//! The two schema-derived builders (`graphql-schema-from-rx-schema.ts`, 337 lines, and
//! `query-builder-from-rx-schema.ts`, 195) generate GraphQL types from an RxDB schema; those
//! are *server-authoring* helpers, not replication, and are not ported here — see "What is not
//! here".
//!
//! # The checkpoint is whatever the caller says it is
//!
//! This adapter is the strongest evidence for D7, because it has **no checkpoint shape of its
//! own**. Upstream's `SyncOptionsGraphQL` takes:
//!
//! ```text
//! pull.queryBuilder(checkpoint, batchSize) -> { query, variables }
//! pull.responseModifier(data, kind, checkpoint) -> { documents, checkpoint }
//! ```
//!
//! (`index.ts:124-153`). The checkpoint is a `CheckpointType` type parameter and the adapter's
//! only interaction with it is to hand the previous value to the caller's `queryBuilder` and to
//! take whatever the caller's `responseModifier` returns from `data.checkpoint`. So a GraphQL
//! server may use `{updatedAt}`, `{offset}`, `{cursor}`, or all three, and this adapter does not
//! care. That is exactly the property the protocol needed and could not have if the checkpoint
//! were a document position.
//!
//! **A consequence worth stating:** the adapter cannot validate a checkpoint. If a caller's
//! server changes its checkpoint semantics without changing its replication identifier, the
//! adapter will faithfully resume at a meaningless position. Upstream has the same hazard, and
//! the mitigation is the caller's: bump `replicationIdentifier` when the meaning changes.
//!
//! # Two request shapes, one transport
//!
//! Every call is a `POST` with `{query, variables}` and a JSON reply that may carry `errors`
//! (`helper.ts:22-48`). Both the pull and the push are expressed as:
//!
//! 1. build the request with the caller's `queryBuilder`;
//! 2. POST it;
//! 3. if `errors` is present, fail (`index.ts:129-131`, `:163-165`) — a GraphQL server reports
//!    application failures *inside* a 200, so a status check alone would miss them;
//! 4. walk to the payload with `dataPath` (default `['data', <first key>]`,
//!    `helper.ts:40-48`), then apply the caller's `responseModifier`.
//!
//! Step 3 and step 4's default are both upstream behaviours that are easy to drop and hard to
//! notice: without 3 an error becomes an empty batch, and without 4 a response under a different
//! root field reads as "no documents".
//!
//! # What is not here
//!
//! * **The WebSocket subscription** (`graphql-websocket.ts`, 62 lines, and
//!   `streamQueryBuilder`). Live push via graphql-ws needs a reconnecting socket, which is a
//!   binding concern (D2/D3). The adapter's change stream is empty and a caller polls.
//! * **Schema and query builders.** Generating a GraphQL schema from an RxDB schema, and the
//!   default pull/push query text, are for authoring a *server*. A ported server belongs with
//!   the GraphQL server work, not with the client transport.
//! * **`credentials`** (cookies). A Rust HTTP client has no cookie jar in this port; a caller
//!   that needs cookies supplies them as headers.

use dxdb_json::Value;
use dxdb_storage::DocumentData;
use dxdb_storage::StorageError;

use crate::adapters::TransportError;
use crate::checkpoint::Checkpoint;
use crate::state::{DocumentsWithCheckpoint, PullStream, ReplicationHandler, WriteToMasterRow};

/// A GraphQL request body: `{query, variables}`.
///
/// Matches `RxGraphQLReplicationQueryBuilderResponseObject`, which upstream passes straight to
/// `JSON.stringify` (`helper.ts:33`).
#[derive(Clone, Debug, PartialEq)]
pub struct GraphQlRequest {
    /// The query text.
    pub query: String,
    /// The variables object.
    pub variables: Value,
}

impl GraphQlRequest {
    /// Builds a request.
    pub fn new(query: impl Into<String>, variables: Value) -> Self {
        GraphQlRequest {
            query: query.into(),
            variables,
        }
    }

    /// The JSON body to POST.
    pub fn to_body(&self) -> Value {
        let mut map = dxdb_json::Map::new();
        map.insert("query".to_string(), Value::String(self.query.clone()));
        map.insert("variables".to_string(), self.variables.clone());
        Value::Object(map)
    }
}

/// How the adapter reaches the GraphQL server.
///
/// One method: every call is a POST of a [`GraphQlRequest`] body. The caller owns auth headers,
/// TLS and retries. `!Send`, like the protocol.
pub trait GraphQlClient {
    /// POSTs a request body and returns the parsed JSON reply.
    ///
    /// A non-2xx status is a [`TransportError::Status`]; a 200 whose body carries `errors` is
    /// *not* a transport error and is handled by the adapter, because GraphQL reports
    /// application failures inside a successful response.
    fn post(&self, body: Value) -> crate::state::BoxFuture<'_, Result<Value, TransportError>>;
}

/// Builds the pull request for a checkpoint and a batch size.
///
/// Upstream's signature is `pull.queryBuilder(lastPulledCheckpoint, batchSize)`
/// (`index.ts:126`).
pub trait PullQueryBuilder {
    /// Builds the request. `checkpoint` is the previous protocol checkpoint's JSON, or `None`
    /// on a first pull.
    fn build(&self, checkpoint: Option<&Value>, batch_size: usize) -> GraphQlRequest;
}

/// Builds the push request for a set of rows.
///
/// Upstream's signature is `push.queryBuilder(rows)` (`index.ts:160`).
pub trait PushQueryBuilder {
    /// Builds the request.
    fn build(&self, rows: &[WriteToMasterRow]) -> GraphQlRequest;
}

/// A caller-supplied adjustment of a response payload.
///
/// Upstream's `responseModifier(data, kind, checkpoint)` (`index.ts:134-141`, `:166-170`) and
/// the `kind` is how one function serves both directions. Kept as a trait so a caller can
/// reshape a server's field layout without this adapter growing a schema mode.
pub trait ResponseModifier {
    /// Which direction the payload came from.
    fn build(
        &self,
        direction: ResponseDirection,
        payload: Value,
        checkpoint: Option<&Value>,
    ) -> Value;
}

/// Which call a [`ResponseModifier`] is adjusting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseDirection {
    /// A pull.
    Pull,
    /// A push.
    Push,
}

/// The default `dataPath`: `['data', <the first key under data>]`.
///
/// Ports `getDataFromResult`'s default (`helper.ts:44-45`). The default matters: a GraphQL
/// response is `{"data": {"<rootField>": ...}}` and the root field name is the server author's
/// choice, so upstream reaches in by taking whatever the first key is rather than requiring a
/// configured path.
pub fn default_data_path(response: &Value) -> Option<Value> {
    let data = response.get("data")?.as_object()?;
    let first_key = data.keys().next()?;
    data.get(first_key).cloned()
}

/// Walks an explicit `dataPath`.
///
/// A path element that is a number indexes an array, matching `getProperty`
/// (`helper.ts:47`), so `["data", "feed", 0]` works.
pub fn walk_data_path(response: &Value, path: &[Value]) -> Option<Value> {
    let mut current = response.clone();
    for part in path {
        current = match part {
            Value::String(key) => current.get(key.as_str())?.clone(),
            Value::Number(number) => {
                let index = usize::try_from(number.as_u64()?).ok()?;
                current.as_array()?.get(index)?.clone()
            }
            _ => return None,
        };
    }
    Some(current)
}

/// The adapter.
pub struct GraphQlHandler<Pull, Push, Client>
where
    Pull: PullQueryBuilder,
    Push: PushQueryBuilder,
    Client: GraphQlClient,
{
    client: Client,
    /// `None` for a push-only or pull-only replication.
    ///
    /// Upstream allows either direction to be absent (`index.ts:120-122`, `:155-157`), and a
    /// missing direction returns an empty result rather than an error — which is what the
    /// collection layer expects when a caller configures one half.
    pull: Option<Pull>,
    push: Option<Push>,
    /// An explicit path into the response, when the default is wrong.
    data_path: Option<Vec<Value>>,
    /// A caller-supplied payload adjuster.
    response_modifier: Option<Box<dyn ResponseModifier>>,
}

impl<Pull, Push, Client> GraphQlHandler<Pull, Push, Client>
where
    Pull: PullQueryBuilder,
    Push: PushQueryBuilder,
    Client: GraphQlClient,
{
    /// Builds the adapter.
    pub fn new(client: Client, pull: Option<Pull>, push: Option<Push>) -> Self {
        GraphQlHandler {
            client,
            pull,
            push,
            data_path: None,
            response_modifier: None,
        }
    }

    /// Sets an explicit path into each response, replacing the default.
    pub fn with_data_path(mut self, path: Vec<Value>) -> Self {
        self.data_path = Some(path);
        self
    }

    /// Sets a response modifier.
    pub fn with_response_modifier(mut self, modifier: Box<dyn ResponseModifier>) -> Self {
        self.response_modifier = Some(modifier);
        self
    }

    /// Extracts the payload from a response, applying the path and the modifier.
    fn extract(
        &self,
        response: &Value,
        direction: ResponseDirection,
        checkpoint: Option<&Value>,
    ) -> Result<Value, TransportError> {
        let payload = match &self.data_path {
            Some(path) => walk_data_path(response, path),
            None => default_data_path(response),
        }
        .ok_or_else(|| {
            TransportError::Malformed(format!(
                "the GraphQL response had no payload at {}{}: {}",
                if self.data_path.is_some() {
                    "the configured dataPath"
                } else {
                    "the default dataPath"
                },
                match &self.data_path {
                    Some(path) =>
                        format!(" ({})", dxdb_json::to_js_json(&Value::Array(path.clone()))),
                    None => String::new(),
                },
                dxdb_json::to_js_json(response)
            ))
        })?;

        Ok(match &self.response_modifier {
            Some(modifier) => modifier.build(direction, payload, checkpoint),
            None => payload,
        })
    }

    /// True when a GraphQL reply carries `errors`, which is an application failure inside a 200.
    ///
    /// Ports the check at `index.ts:129-131`. A GraphQL server returns `200 OK` with
    /// `{"errors": [...]}` for a bad query, an authorization failure, or a resolver panic, so a
    /// status check alone would treat all of them as an empty batch.
    fn graphql_errors(response: &Value) -> Option<Vec<String>> {
        let errors = response.get("errors")?.as_array()?;
        if errors.is_empty() {
            return None;
        }
        Some(
            errors
                .iter()
                .map(|error| {
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| dxdb_json::to_js_json(error))
                })
                .collect(),
        )
    }
}

impl<Pull, Push, Client> ReplicationHandler for GraphQlHandler<Pull, Push, Client>
where
    Pull: PullQueryBuilder,
    Push: PushQueryBuilder,
    Client: GraphQlClient,
{
    fn master_changes_since(
        &self,
        checkpoint: Option<&Checkpoint>,
        batch_size: usize,
    ) -> crate::state::BoxFuture<'_, Result<DocumentsWithCheckpoint, StorageError>> {
        // A pull-only caller's complement: no pull configured means no documents, not an error
        // (`index.ts:120-122`).
        let Some(builder) = &self.pull else {
            return Box::pin(async {
                Ok(DocumentsWithCheckpoint {
                    documents: Vec::new(),
                    checkpoint: None,
                })
            });
        };

        let previous = checkpoint.map(|checkpoint| checkpoint.as_value().clone());
        let request = builder.build(previous.as_ref(), batch_size);

        Box::pin(async move {
            let response = self
                .client
                .post(request.to_body())
                .await
                .map_err(crate::adapters::to_storage_error)?;

            if let Some(errors) = Self::graphql_errors(&response) {
                return Err(StorageError::Unsupported(format!(
                    "the GraphQL server reported errors: {}",
                    errors.join("; ")
                )));
            }

            let payload = self
                .extract(&response, ResponseDirection::Pull, previous.as_ref())
                .map_err(crate::adapters::to_storage_error)?;

            // The payload is `{documents, checkpoint}` -- the shape the caller's
            // `responseModifier` produces (`index.ts:143-152`).
            let documents = payload
                .get("documents")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|value| {
                    DocumentData::from_value(value).map_err(|error| {
                        StorageError::InvalidDocument(format!(
                            "a GraphQL pull document is not an object: {error}"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            // The checkpoint is whatever the server put here. The adapter does not inspect it;
            // that is the whole point (module docs).
            let checkpoint = match payload.get("checkpoint") {
                Some(Value::Null) | None => None,
                Some(value) => Some(
                    Checkpoint::new(value.clone())
                        .map_err(|error| StorageError::Unsupported(error.to_string()))?,
                ),
            };

            Ok(DocumentsWithCheckpoint {
                documents,
                checkpoint,
            })
        })
    }

    fn master_write(
        &self,
        rows: Vec<WriteToMasterRow>,
    ) -> crate::state::BoxFuture<'_, Result<Vec<DocumentData>, StorageError>> {
        // A pull-only caller's complement: no push configured means nothing to send.
        let Some(builder) = &self.push else {
            return Box::pin(async { Ok(Vec::new()) });
        };
        if rows.is_empty() {
            return Box::pin(async { Ok(Vec::new()) });
        }

        let request = builder.build(&rows);

        Box::pin(async move {
            let response = self
                .client
                .post(request.to_body())
                .await
                .map_err(crate::adapters::to_storage_error)?;

            if let Some(errors) = Self::graphql_errors(&response) {
                return Err(StorageError::Unsupported(format!(
                    "the GraphQL server reported errors on push: {}",
                    errors.join("; ")
                )));
            }

            let payload = self
                .extract(&response, ResponseDirection::Push, None)
                .map_err(crate::adapters::to_storage_error)?;

            // The push payload is the conflicts array itself. Upstream returns `data` and
            // expects the server's resolver to have produced `WithDeleted<RxDocType>[]`
            // (`index.ts:172`).
            let conflicts = match payload {
                Value::Array(items) => items,
                // A server whose push resolver returns `{conflicts: [...]}` is also accepted,
                // because that is the shape every other adapter in this crate uses and a
                // caller should not have to reshape it in a modifier.
                Value::Object(_) => payload
                    .get("conflicts")
                    .and_then(Value::as_array)
                    .cloned()
                    .ok_or_else(|| {
                        StorageError::Unsupported(
                            "a GraphQL push payload must be an array of conflicts, or an object \
                             with a `conflicts` array"
                                .to_string(),
                        )
                    })?,
                other => {
                    return Err(StorageError::Unsupported(format!(
                        "a GraphQL push payload must be an array of conflicts, got {}",
                        dxdb_json::to_js_json(&other)
                    )))
                }
            };

            conflicts
                .into_iter()
                .map(|value| {
                    DocumentData::from_value(value).map_err(|error| {
                        StorageError::InvalidDocument(format!(
                            "a GraphQL conflict is not an object: {error}"
                        ))
                    })
                })
                .collect()
        })
    }

    fn master_change_stream(&self) -> PullStream {
        // The graphql-ws subscription needs a reconnecting client; a caller that wants live
        // behaviour polls `master_changes_since`.
        Box::pin(futures::stream::empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_data_path_takes_the_first_root_field() {
        // `helper.ts:44-45`: `['data', Object.keys(result.data)[0]]`.
        let response = dxdb_json::parse(
            r#"{"data":{"feedHuman":{"documents":[{"id":"a"}],"checkpoint":{"updatedAt":5}}}}"#,
        )
        .expect("json");
        let payload = default_data_path(&response).expect("a payload");
        assert!(payload.get("documents").is_some());
        assert_eq!(
            payload
                .get("checkpoint")
                .and_then(|c| c.get("updatedAt"))
                .and_then(Value::as_u64),
            Some(5)
        );
    }

    #[test]
    fn a_configured_path_can_index_an_array() {
        // `getProperty` supports numeric path elements (`helper.ts:47`).
        let response = dxdb_json::parse(r#"{"data":{"feed":[{"documents":[]}]}}"#).expect("json");
        let path = vec![
            Value::String("data".to_string()),
            Value::String("feed".to_string()),
            Value::from(0u64),
            Value::String("documents".to_string()),
        ];
        let payload = walk_data_path(&response, &path).expect("a payload");
        assert_eq!(payload, Value::Array(Vec::new()));
    }

    #[test]
    fn graphql_errors_are_detected_inside_a_200() {
        let response = dxdb_json::parse(r#"{"errors":[{"message":"not authorised"}],"data":null}"#)
            .expect("json");
        let errors = GraphQlHandler::<NoPull, NoPush, NoClient>::graphql_errors(&response)
            .expect("errors must be reported");
        assert_eq!(errors, vec!["not authorised".to_string()]);
    }

    #[test]
    fn an_empty_errors_array_is_not_an_error() {
        let response = dxdb_json::parse(r#"{"errors":[],"data":{}}"#).expect("json");
        assert!(
            GraphQlHandler::<NoPull, NoPush, NoClient>::graphql_errors(&response).is_none(),
            "an empty errors array is success, not a failure"
        );
    }

    #[test]
    fn a_request_body_is_query_plus_variables() {
        let request = GraphQlRequest::new("query { a }", {
            let mut map = dxdb_json::Map::new();
            map.insert("id".to_string(), Value::String("x".to_string()));
            Value::Object(map)
        });
        let body = request.to_body();
        assert_eq!(
            body.get("query").and_then(Value::as_str),
            Some("query { a }")
        );
        assert_eq!(
            body.get("variables")
                .and_then(|v| v.get("id"))
                .and_then(Value::as_str),
            Some("x")
        );
    }

    // Placeholder types so the static helpers can be named without a client.
    struct NoPull;
    impl PullQueryBuilder for NoPull {
        fn build(&self, _checkpoint: Option<&Value>, _batch_size: usize) -> GraphQlRequest {
            GraphQlRequest::new("", Value::Null)
        }
    }
    struct NoPush;
    impl PushQueryBuilder for NoPush {
        fn build(&self, _rows: &[WriteToMasterRow]) -> GraphQlRequest {
            GraphQlRequest::new("", Value::Null)
        }
    }
    struct NoClient;
    impl GraphQlClient for NoClient {
        fn post(&self, _body: Value) -> crate::state::BoxFuture<'_, Result<Value, TransportError>> {
            Box::pin(async { Err(TransportError::Unreachable("test".to_string())) })
        }
    }
}
