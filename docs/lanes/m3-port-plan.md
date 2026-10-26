# M3 port plan: the RxDB replication protocol and attachments

Lane E. Upstream pin `~/Code/rxdb` @ `3b763ee17` (v17.6.0).

This is the plan for porting `src/replication-protocol/*` and the attachment data
model. It records what upstream actually does, what the Rust surface is, and which
observables must be byte-identical versus merely equivalent. It is deliberately
written before `dxdb-storage` exists: the trait names below are the ones this lane
needs, and section 9 lists exactly what is accepted from lane D unchanged versus
what must be negotiated.

Status: **tranche 1 (scoping + skeletons)**. No protocol code is written until
`dxdb-storage` publishes real traits (`docs/WORKTREES.md`: lane D).

---

## 1. What was scoped

Read in full, not sampled.

| Upstream file | Lines | Role |
| --- | --- | --- |
| `src/replication-protocol/index.ts` | 353 | state construction, handler-from-storage adapter, lifecycle, `awaitInSync` helpers |
| `src/replication-protocol/downstream.ts` | 550 | pull: resync loop + change-stream mode, task queue, persistence merge, fork/meta writes, checkpoint |
| `src/replication-protocol/upstream.ts` | 595 | push: initial sync loop, task processing, feed from fork change stream, conflict writes, checkpoint |
| `src/replication-protocol/checkpoint.ts` | 154 | `getLastCheckpointDoc`, `setCheckpoint` (stacking + conflict retry), `getCheckpointKey` |
| `src/replication-protocol/meta-instance.ts` | 195 | meta schema generation, `getAssumedMasterState`, `getMetaWriteRow` |
| `src/replication-protocol/conflicts.ts` | 53 | `resolveConflictError`: how a resolved doc is stamped before writing the fork |
| `src/replication-protocol/helper.ts` | 178 | doc-state ⇄ write-doc conversion, meta attachment stripping, remote JSON attachment shims |
| `src/replication-protocol/default-conflict-handler.ts` | 43 | the default conflict contract |
| **protocol subtotal** | **2,121** | |
| `src/plugins/test-utils/replication-base-test-suite.ts` | 454 | the shared suite every replication plugin runs (gate) |
| `src/plugins/replication/index.ts` | 900 | `RxReplicationState`: start/pause/cancel/remove/await*, retry loop, pull/push wiring |
| `src/plugins/replication/replication-helper.ts` | 122 | `handlePulledDocuments`, `swapDefaultDeletedTodeletedField`, `awaitRetry`, `DEFAULT_MODIFIER` |
| **collection-level subtotal** | **1,022** | |
| **primary total** | **3,597** | |

Types and shared helpers read because the protocol's semantics live in them:

| File | Lines | Used for |
| --- | --- | --- |
| `src/types/replication-protocol.d.ts` | 298 | `RxStorageReplicationMeta`, `RxReplicationWriteToMasterRow`, `DocumentsWithCheckpoint`, `RxReplicationHandler`, `RxStorageInstanceReplicationInput`, `RxStorageInstanceReplicationState` |
| `src/types/conflict-handling.d.ts` | 58 | `RxConflictHandler`, `RxConflictHandlerInput`, `RxReplicationConflict` |
| `src/types/rx-attachment.d.ts` | 46 | `RxAttachment`, `RxAttachmentCreator`, `RxAttachmentCreatorBase64` |
| `src/types/rx-storage.d.ts` (quoted spans) | 347 | `RxDocumentData`, `RxAttachmentData`, `RxAttachmentWriteData`, `BulkWriteRow`, `RxStorageBulkWriteResponse`, `EventBulk`, `RxStorageDefaultCheckpoint` |
| `src/types/rx-storage.interface.d.ts` (quoted spans) | 315 | `RxStorageInstance` method contract |
| `src/types/rx-document.d.ts` (quoted spans) | 160 | `RxDocumentMeta` (`lwt`, `o`, index signature) |
| `src/plugins/attachments/attachments-utils.ts` | 84 | `fillWriteDataForAttachmentsChange`, `ensureSchemaSupportsAttachments` |
| `src/plugins/attachments/index.ts` | 313 | `RxAttachment`, `putAttachment*`, `getAttachment`, `allAttachments`, `preMigrateDocument` |
| | **397** | attachments total |
| `src/rx-storage-helper.ts` (quoted spans) | 1,131 | `stackCheckpoints`, `getChangedDocumentsSince` (+ query fallback), `stripAttachmentsDataFromDocument`, `attachmentWriteDataToNormalData`, `getWrittenDocumentsFromBulkWriteResponse`, `flatCloneDocWithMeta` |
| `src/rx-schema-helper.ts` (quoted spans) | 394 | `getComposedPrimaryKeyOfDocumentData`, `getLengthOfPrimaryKey`, `getPrimaryFieldOfPrimaryKey` |
| `src/plugins/utils/utils-revision.ts` | 66 | `createRevision`, `getHeightOfRevision`, `parseRevision` |
| `src/plugins/utils/utils-time.ts` | 41 | `now()` monotonic clock |
| `src/plugins/utils/utils-document.ts` (quoted spans) | 117 | `getDefaultRevision`, `getDefaultRxDocumentMeta`, `RX_META_LWT_MINIMUM` |
| `src/plugins/utils/utils-hash.ts` | 62 | `nativeSha256` = `defaultHashSha256` |
| `src/plugins/utils/utils-object-deep-equal.ts` | 44 | `deepEqual` (used by the default conflict handler) |
| `src/plugins/test-utils/{humans-collection,schema-objects,test-util}.ts` | 1,240 | fixtures the gate suite depends on |

Numbers in the report are the 2,121 / 454 / 1,022 above.

---

## 2. Terminology, fixed here because it is easy to get backwards

- **fork** — the local storage instance. It is the only place conflicts are
  resolved. Revisions here are a local revision chain, `{height}-{token}`.
- **master** — the remote actor, reached only through `RxReplicationHandler`.
  Never has conflicts; "master is always the truth".
- **meta instance** — a second storage instance, per replication, holding two
  kinds of rows in one collection: per-document assumed-master states
  (`isCheckpoint: '0'`) and per-direction checkpoints (`isCheckpoint: '1'`).
- **downstream = pull** (master → fork), **upstream = push** (fork → master).
  `RxStorageReplicationDirection = 'up' | 'down'`; the checkpoint `itemId` is the
  literal string `'up'` or `'down'`, not `'UP'`/`'DOWN'`. (Upstream's comment
  about `'DOWN'` having 4 chars is only a length bound: `minItemIdLength = 4`.)

---

## 3. The checkpoint stack data model

### 3.1 Types, exactly as upstream

```ts
type RxStorageReplicationDirection = 'up' | 'down';

// src/types/replication-protocol.d.ts:18
type RxStorageReplicationMeta<RxDocType, CheckpointType> = {
    id: string;                  // composed PK: `${itemId}|${isCheckpoint}`
    itemId: string;              // doc primary key, or 'up'/'down' for checkpoints
    isCheckpoint: '0' | '1';     // string on purpose: part of the composed PK
    checkpointData?: CheckpointType;
    docData?: RxDocType | RxDocumentData<RxDocType> | any;
    isResolvedConflict?: string; // the fork _rev this assumed-master came from
};

// src/types/rx-storage.d.ts:310 — only an example; storages may use any shape
type RxStorageDefaultCheckpoint = { id: string; lwt: number };

// src/types/replication-protocol.d.ts:63
type DocumentsWithCheckpoint<RxDocType, CheckpointType> = {
    documents: WithDeletedAndAttachments<RxDocType>[];
    checkpoint: CheckpointType;
};

// src/types/replication-protocol.d.ts:57
type RxReplicationWriteToMasterRow<RxDocType> = {
    assumedMasterState?: WithDeletedAndAttachments<RxDocType>;
    newDocumentState: WithDeletedAndAttachments<RxDocType>;
};

// src/types/replication-protocol.d.ts:69
type RxReplicationPullStreamItem<RxDocType, C> = DocumentsWithCheckpoint<RxDocType, C> | 'RESYNC';
```

`CheckpointType` is **generic and opaque to the protocol**. The protocol only ever
does three things with it:

1. `stackCheckpoints([a, b])` before persisting,
2. `JSON.stringify` it to decide whether a write is needed,
3. hand it back to the caller's `masterChangesSince` / `getChangedDocumentsSince`.

Everything else about a checkpoint belongs to the storage or the transport.

### 3.2 `stackCheckpoints` — the whole of checkpoint merging

```ts
// src/rx-storage-helper.ts:150
export function stackCheckpoints<C>(checkpoints: (C | undefined)[]): C {
    return Object.assign({}, ...checkpoints.filter(x => !!x));
}
```

Three facts fall out of that one line and they are all wire behaviour:

- **Later wins on key collision.** Merging is a shallow `Object.assign`, left to
  right. A checkpoint from a later batch overwrites earlier keys of the same name.
- **Key order is the order of first insertion.** Keys present only in the first
  object keep their position; overwritten keys keep the *first* position but take
  the later value. So merging `{a,b}` then `{c,b}` yields key order `a,b,c`.
- **`undefined`/`null`/falsy checkpoints are dropped, not merged.** `{...null}`
  contributes nothing, so an initial `undefined` checkpoint is safe.

Consequence for the port: a checkpoint **cannot** be a `BTreeMap` and cannot be
`serde_json::Map` unless the workspace's `serde_json` has `preserve_order`
(it does, `Cargo.toml`), and it must never be re-serialised through a type that
sorts keys. `stack_checkpoints` is `dxdb_json::Value::Object` insertion-order
merge, implemented once, unit-tested against the three facts above, and used by
both directions.

### 3.3 The checkpoint key

```ts
// checkpoint.ts:145
const hash = await input.hashFunction([
    input.identifier,
    input.forkInstance.databaseName,
    input.forkInstance.collectionName
].join('||'));
return 'rx_storage_replication_' + hash;
```

- Join separator is the two characters `||`.
- Default `hashFunction` is `nativeSha256` (`utils-hash.ts:52`): SHA-256 over the
  UTF-8 bytes, lowercase hex, no prefix, no truncation.
- The collection-level layer passes `identifier = 'rxdbreplication' + replicationIdentifier`
  (`plugins/replication/index.ts:72`), so the effective input is
  `rxdbreplication<id>||<databaseName>||<collectionName>`.
- The result is used for **three** distinct purposes:
  1. as the revision token of every meta row (`createRevision(state.checkpointKey, previous)`),
  2. as the meta-instance collection name suffix — no: that one is separate, see 5.1,
  3. as the `_meta` tag key on fork documents (`_meta[state.input.identifier]`).

It is derived asynchronously once, stored as `state.checkpointKey: Promise<string>`,
and awaited at every use site. In Rust it is computed once at construction — the
async-ness of `crypto.subtle` does not exist for us — and stored as `String`.

### 3.4 The asynchrony is not decoration

`state.checkpointDoc` and the write ordering are protected by two *promise
chains*, not by locks:

- `state.checkpointQueue` — every `setCheckpoint` appends to it; every
  `getLastCheckpointDoc` is also appended to it before reading. Serialises
  checkpoint writes and guarantees "last write wins, in call order".
- `state.streamQueue.{up,down}` and `state.persistenceQueue` (module-local in each
  direction file) — serialise batch processing per direction.

The Rust port keeps the same structure (`Option<Arc<Mutex<…>>>` is the wrong
answer here: it would allow interleaving that upstream does not). A
single-threaded ordered task chain per queue, with the executor supplied by the
caller, reproduces upstream's ordering exactly without a lock.

---

## 4. Data flows and their observable return shapes

### 4.1 The transport-facing contract (`RxReplicationHandler`)

This is the trait the caller implements. It is the entire interchange surface of
the protocol, and it must be tiny, because the HTTP transport belongs to the
caller (lane brief: "the HTTP transport belongs to the caller, behind a trait").

```ts
type RxReplicationHandler<RxDocType, MasterCheckpointType> = {
    masterChangeStream$: Observable<RxReplicationPullStreamItem<RxDocType, MasterCheckpointType>>;
    masterChangesSince(
        checkpoint: MasterCheckpointType | undefined,
        batchSize: number
    ): Promise<DocumentsWithCheckpoint<RxDocType, MasterCheckpointType | undefined>>;
    masterWrite(
        rows: RxReplicationWriteToMasterRow<RxDocType>[]
    ): Promise<WithDeleted<RxDocType>[]>;
};
```

Observable semantics that must survive the port:

| Member | Contract |
| --- | --- |
| `masterChangesSince` | Returns **at most `batchSize`** documents sorted by `(_meta.lwt, primaryKey)` ascending, and a checkpoint that resumes *exactly after the last returned document*. Returned checkpoint is `undefined`-tolerant only in the type, not the protocol: the protocol guards with `documents.length === 0` first. |
| `masterWrite` | Returns **only the conflicts** — the real master state of every row the master refused — and an empty array otherwise. Row granularity is the document, not the batch. The protocol splits rows into `pushBatchSize` batches before calling, so `masterWrite` is never handed more than `pushBatchSize` rows. |
| `masterChangeStream$` | Emits either a live `{documents, checkpoint}` bulk or the literal string `'RESYNC'`. `'RESYNC'` is the only signal that the transport lost events, and it is what triggers a full checkpoint-iteration. |

### 4.2 Master-from-storage adapter

`rxStorageInstanceToReplicationHandler` (`index.ts:184`) is the proof the protocol
is transport-agnostic: it builds a handler over another `RxStorageInstance` and is
used by every storage-to-storage plugin.

- `masterChangesSince` → `getChangedDocumentsSince(instance, batchSize, checkpoint)`,
  and **the returned checkpoint is the input checkpoint when no documents came
  back** (`documents.length > 0 ? result.checkpoint : checkpoint`). That is not
  cosmetic: it means a no-op poll cannot advance the checkpoint.
- Each document is converted with `writeDocToDocState(doc, hasAttachments, keepMeta)`,
  i.e. `_meta` and `_rev` are stripped unless `keepMeta`.
- With attachments, every document additionally gets
  `fillWriteDataForAttachmentsChange(primaryPath, instance, doc, undefined)` — passing
  `undefined` as the original means **all** attachment blobs are re-attached,
  because "the master never knows the client state of the document".
- `masterWrite` prefetches master states, then per row:
  - no master state → plain insert (`document` with no `previous`);
  - master state but no `assumedMasterState` → **conflict**, push the master state;
  - `conflictHandler.isEqual(master, assumedMaster)` → write with `previous: masterState`;
  - otherwise → **conflict**, push the master state.
  Then `bulkWrite(..., 'replication-master-write')`; `409`s become conflicts
  (using `err.documentInDb`), any other status is converted to an `RxError` and
  thrown (`rxStorageWriteErrorToRxError`) — non-409 errors are real errors, never
  silently dropped.
- Note `docStateToWriteDoc(..., keepMeta, row.newDocumentState, masterState)`:
  the master generates the next revision, and keeps `previous._meta` fields while
  replacing `lwt`.

### 4.3 Downstream (pull), step by step

Entry: `startReplicationDownstream(state)` (`downstream.ts:39`). It is `async`
because of the `initialCheckpoint` write; the two directions start independently
and never await each other.

1. **Initial checkpoint.** If `input.initialCheckpoint.downstream` is set and no
   checkpoint doc exists yet, `setCheckpoint(state, 'down', initialCheckpoint.downstream)`.
   "No doc exists yet" — an existing checkpoint is *never* overwritten by the option.
2. Start with one `'RESYNC'` task, and subscribe to `masterChangeStream$`.
   Every event is gated behind `state.events.active.up` being false — "while a
   push is running, we have to delay all incoming events from the server to not
   mix up the replication state". That cross-direction gate is load-bearing for
   convergence under contention and must be reproduced.
3. **Task queue.** Tasks get a monotonically increasing `time`. On each drain:
   - drop any task with `time < lastTimeMasterChangesRequested` (it was already
     covered by a pull that started later),
   - a `'RESYNC'` drains alone (everything queued behind it will be re-derived),
   - the remainder are concatenated and processed as one batch.
4. **`downstreamResyncOnce()`** — checkpoint iteration:
   ```
   lastCheckpoint = await getLastCheckpointDoc(state,'down')   (via checkpointQueue)
   loop:
     lastTimeMasterChangesRequested = timer++
     downResult = masterChangesSince(lastCheckpoint, pullBatchSize)
     if downResult.documents.length === 0: break
     lastCheckpoint = stackCheckpoints([lastCheckpoint, downResult.checkpoint])
     promises.push(persistFromMaster(downResult.documents, lastCheckpoint))
     if documents.length < pullBatchSize: break        // "remote has no more documents"
   await all promises
   ```
   Note both exit conditions: empty batch **or** short batch. A full batch always
   continues, even if it was the last one.
5. **`downstreamProcessChanges(tasks)`** merges all task documents and stacks all
   task checkpoints, then calls `persistFromMaster` once.
6. **`persistFromMaster(docs, checkpoint)`** does *not* write anything itself. It
   merges the docs into a module-local `nonPersistedFromMaster` map keyed by
   primary, records the newest checkpoint, and appends work to `persistenceQueue`.
   The drain then, in one pass:
   - reads current fork states and assumed-master states for the ids;
   - **waits for upstream** when the current fork state *is* the unresolved
     conflict resolution (`assumedMaster.metaDocument.isResolvedConflict === forkStateFullDoc._rev`):
     "the current fork state represents a resolved conflict that first must be
     sent to the master in the upstream; all conflicts are resolved by the upstream";
   - computes three booleans: `isAssumedMasterEqualToForkState` (from
     `conflictHandler.isEqual`, with a `_rev`-height fallback for masters that
     carry `_rev`), "was this fork state written by the master itself"
     (`_meta.o.hash === identifierHash && _meta.o._rev === height(fork._rev)`), and
     `areStatesExactlyEqual` (`conflictHandler.isEqual(master, fork)`);
   - **skips the write** when there is an unpushed local write and it was not
     written by us — that state must go upstream first;
   - **meta-only patch** when the states are exactly equal (recovery after an
     unclean shutdown) — fork is not rewritten;
   - otherwise builds the new fork state:
     `_meta` cloned from the fork (or `{lwt: now()}` for a new doc),
     `_attachments` from the master only when `hasAttachments`,
     `_rev = getDefaultRevision()` (`''`),
     `_meta[identifier] = height(fork._rev)+1` **when the master state has a `_rev`**,
     `_rev = master._rev` if `keepMeta`,
     `_meta = master._meta` if `keepMeta`,
     and always `_meta.o = {_rev: height(fork._rev)+1, hash: identifierHash}`;
     then `_rev = createRevision(identifierHash, previous)`;
   - writes fork rows with **context `state.downstreamBulkWriteFlag`**, so the
     upstream can recognise and ignore its own downstream writes;
   - writes meta rows with `isCheckpoint:'0'` and `docData = masterState`;
   - emits `events.processed.down` per successfully written row;
   - converts non-409 fork write errors to `RC_PULL` on `events.error` **and throws**;
   - finally calls `setCheckpoint(state,'down',checkpoint)` without awaiting it
     (ordering is preserved by `checkpointQueue`).

### 4.4 Upstream (push), step by step

Entry: `startReplicationUpstream(state)` (`upstream.ts:43`). It queues on
`state.streamQueue.up` as `upstreamInitialSync()` then `processTasks()`.

1. **Initial checkpoint** mirrors downstream: only written when no checkpoint doc
   exists.
2. Subscribe to `forkInstance.changeStream()`. Each event bulk becomes a task,
   **unless paused** (paused tasks are dropped, not buffered). `waitBeforePersist`
   is awaited before processing when configured.
3. `'RESYNC'` from `masterChangeStream$` becomes a task too — the fork lost
   nothing, but the master might have, so the upstream re-runs its initial sync.
4. **`upstreamInitialSync()`** — checkpoint iteration over the *fork*:
   ```
   lastCheckpoint = await getLastCheckpointDoc(state,'up')
   loop:
     initialSyncStartTime = timer++
     if pending > 3: await Promise.race(pending)      // throttle; UI stays responsive
     if canceled: break
     upResult = getChangedDocumentsSince(forkInstance, pushBatchSize, lastCheckpoint)
     if documents empty: break
     lastCheckpoint = stackCheckpoints([lastCheckpoint, upResult.checkpoint])
     pending.add(persistToMaster(documents, lastCheckpoint))
   await all pending
   if any returned true (had conflict writes): upstreamInitialSync() recursively
   else firstSyncDone.up.next(true)
   ```
   The recursive re-run is how "conflicts resolved during initial sync created new
   local writes that must also be pushed" is handled. It is not a retry loop with
   a bound; it terminates because each round pushes and conflict writes monotonically
   advance fork revisions.
5. **`processTasks()`** merges open tasks, drops tasks with
   `time < initialSyncStartTime`, and **drops documents whose event bulk context
   equals `state.downstreamBulkWriteFlag`** (those came from us; the checkpoint
   still advances). A `'RESYNC'` task inside a batch short-circuits to
   `upstreamInitialSync()`.
6. **`persistToMaster(docs, checkpoint)` → `Promise<boolean>` (had conflicts)**:
   merges into `nonPersistedFromMaster`, then on the queue:
   - **even with zero documents, the checkpoint is stored.** "This ensures that
     when many documents have been pulled from the remote (that do not have to be
     pushed again), we continue at the correct position."
   - skip a document when the assumed master is equal to the fork state — either
     `conflictHandler.isEqual(...)` or the `_rev`-height shortcut — **unless**
     `isResolvedConflict === fork._rev`, in which case it must be pushed;
     this is the "resolved on the fork, still needs to reach the master" case;
   - build `{assumedMasterState, newDocumentState}` rows plus meta rows;
   - split rows into `pushBatchSize` batches (`batchArray`), attach blob data per
     row when `hasAttachments`, call `masterWrite` per batch concurrently;
   - **re-check `canceled || paused` after the writes and before marking meta.**
     Without this, a pause during a push would mark unsent docs as pushed and they
     would never be retried on resume. This is an explicit upstream fix and must be
     preserved;
   - emit `events.processed.up` for non-conflicting rows whose
     `newDocumentState !== null` (a push modifier may filter a row to `null`; the
     meta is still written so it is not retried forever);
   - write meta for non-conflicting rows (`isCheckpoint:'0'`);
   - for each conflict: `resolveConflictError(...)` → emit `events.resolvedConflicts`
     with `{input, output}` → `bulkWrite(conflictFork, 'replication-up-write-conflict')`
     → write meta with `isResolvedConflict = resolved._rev` and
     `docData = realMasterState`;
   - non-409 fork write errors → `RC_PUSH` on `events.error` and throw;
   - store the checkpoint, return `hadConflictWrites`.

### 4.5 What the caller can observe

Per-direction counters exist because the ported test suite asserts on replication
*behaviour*, not internals. They are part of the contract:

```ts
stats.down: { addNewTask, downstreamResyncOnce, downstreamProcessChanges,
              masterChangeStreamEmit, persistFromMaster }
stats.up:   { upstreamInitialSync, forkChangeStreamEmit, processTasks, persistToMaster,
              persistToMasterHadConflicts, persistToMasterConflictWrites }
```

and the event surface: `processed.{up,down}`, `resolvedConflicts`, `error`,
`canceled`, `paused`, `active.{up,down}`, plus `firstSyncDone.{up,down}`,
`streamQueue.{up,down}`, `checkpointQueue`.

`awaitRxStorageReplicationIdle` documents the idleness definition precisely:
await `firstSyncDone` first, then loop `Promise.all([streamQueue.up, streamQueue.down])`
until **both promise identities are unchanged across the await**. A boolean "busy"
flag would pass the happy path and fail under the race the loop exists for, so the
Rust port keeps queue-generation identity (`Arc::ptr_eq` on the current queue
future, or a generation counter incremented on every reassignment).

---

## 5. The meta instance persistence contract

### 5.1 Identity

The meta instance is a *real storage instance* with its own schema, not a key-value
store. Its collection name is
`'rx-replication-meta-' + hashFunction([collection.name, replicationIdentifier].join('-'))`
(`plugins/replication/index.ts:51-54`). Note the separator here is `-`, unlike the
`||` in the checkpoint key. Both are observable because both name persisted data.

### 5.2 Schema, field by field

`getRxReplicationMetaInstanceSchema(replicatedSchema, encrypted)` (`meta-instance.ts:26`):

```ts
title: 'RxReplicationProtocolMetaData'            // META_INSTANCE_SCHEMA_TITLE
primaryKey: { key: 'id', fields: ['itemId','isCheckpoint'], separator: '|' }
type: 'object'
version: replicatedSchema.version                 // copied
additionalProperties: false
keyCompression: replicatedSchema.keyCompression   // copied
required: ['id','isCheckpoint','itemId']
encrypted: ['docData']                            // only when encrypted
properties: {
  id:            { type:'string', minLength:1, maxLength:idLength },
  isCheckpoint:  { type:'string', enum:['0','1'], minLength:1, maxLength:1 },
  itemId:        { type:'string', maxLength: max(parentPKMaxLength, 4) },
  checkpointData:{ type:'object', additionalProperties:true },
  docData:       { type:'object', properties: replicatedSchema.properties },
  isResolvedConflict: { type:'string' }
}
```

with

```
idLength = max(parentPrimaryKeyMaxLength + 2, 6)   // +1 separator, +1 isCheckpoint flag
```

The lengths are not cosmetic: `getComposedPrimaryKeyOfDocumentData` concatenates
`itemId + '|' + isCheckpoint`, and the schema validator runs on every meta write.
`parentPrimaryKeyMaxLength` comes from the *fork* schema's primary field
`maxLength` via `getLengthOfPrimaryKey`. For a **composite** primary key that
function reads `properties[primaryKey.key].maxLength` (`getPrimaryFieldOfPrimaryKey`
returns the composite's `.key`, `rx-schema-helper.ts:126-142`), and
`fillWithDefaultSettings` does not inject a `properties` entry for `.key`, so the
lookup fails on `ensureNotFalsy` unless the schema happens to declare one. The port
reproduces that failure rather than inventing a default length.

### 5.3 Row shapes

Checkpoint row (`setCheckpoint`, `checkpoint.ts:76`):

```json
{ "id": "<direction>|1", "isCheckpoint": "1", "itemId": "<direction>",
  "_deleted": false, "_attachments": {}, "checkpointData": <stacked checkpoint>,
  "_meta": { "lwt": <now()> }, "_rev": "<height>-<checkpointKey>" }
```

Assumed-master row (`getMetaWriteRow`, `meta-instance.ts:147`):

```json
{ "id": "<docId>|0", "isCheckpoint": "0", "itemId": "<docId>",
  "docData": <master document state>, "_attachments": {}, "_deleted": false,
  "_rev": "<height>-<checkpointKey>", "_meta": { "lwt": <now()> },
  "isResolvedConflict": "<fork _rev>"        // only when set
}
```

Rules that matter:

- `isResolvedConflict` must be **omitted**, not `undefined`/`null`, when absent:
  upstream's comment says sending `undefined` fails schema validation.
- `checkpointData` is preserved verbatim through the storage round-trip. It is
  never reordered, never renumbered, never normalised.
- `docData` carries the master state **as the master expressed it** — no `_meta`,
  no `_rev`, no attachment blobs (stripped by
  `stripAttachmentsDataFromMetaWriteRows` when the fork has attachments).
- `_rev` here is a fork-token revision, generated from the *previous meta row*,
  not from `docData`.

### 5.4 The write loop and its 409 recovery

`setCheckpoint` is the only place the protocol deliberately loses a race and
recovers:

1. Skip entirely when `checkpoint` is falsy, when canceled, or when
   `JSON.stringify(previous.checkpointData) === JSON.stringify(checkpoint)`.
   The comparison is **JavaScript `JSON.stringify`**, so it is exactly
   `dxdb_json::to_js_json` on both sides; a `serde_json` float writer would make
   `1.0` vs `1` compare unequal and cause a write upstream would have skipped.
2. Build the row, then loop: stack with the previous checkpoint, set
   `_meta.lwt = now()`, set `_rev = createRevision(checkpointKey, previous)`,
   bail if canceled, `bulkWrite([{previous, document}], 'replication-set-checkpoint')`.
3. On success, `state.lastCheckpointDoc[direction] = successDoc`. Note it stores
   the **written document** (the one returned by
   `getWrittenDocumentsFromBulkWriteResponse`), so the next stacking uses the
   server-assigned revision.
4. On `409`, take `error.documentInDb`, use it as the new `previous`, recompute the
   revision, and retry. Any non-409 error is thrown, out of the queue.

`getLastCheckpointDoc` caches into `state.lastCheckpointDoc[direction]` on every
read, including when nothing was found (`undefined`), and runs on the checkpoint
queue.

### 5.5 What the meta instance is *not*

- It is not where replication state lives. `paused`, `canceled`, `active`,
  queues and task times are in-memory only; a restart begins with `paused=false`.
- It is not the fork. Writing meta must not emit fork change events, which is the
  stated reason the meta instance exists at all ("RxDB will not re-emit query
  results or document state when replication meta data is written").
- It is not optional. `skipStoringPullMeta` only skips the per-document rows for
  pull-only replications; checkpoints are still written.

---

## 6. The conflict-resolution contract

### 6.1 The interface

```ts
type RxConflictHandlerInput<Doc> = {
    assumedMasterState?: WithDeleted<Doc>;
    realMasterState: WithDeleted<Doc>;
    newDocumentState: WithDeleted<Doc>;
};

type RxConflictHandler<Doc> = {
    isEqual: (a: WithDeleted<Doc>, b: WithDeleted<Doc>, context: string) => boolean; // must be non-async
    resolve: (i: RxConflictHandlerInput<Doc>, context: string) => Promise<WithDeleted<Doc>>;
};
```

Both operate on `WithDeleted<Doc>`: `_meta`/`_rev` are **not** passed to the
handler, because they are local storage metadata and are not replicated. A port
that hands the handler `RxDocumentData` changes what a user's handler can see and
breaks the upstream contract.

### 6.2 Where conflicts exist, and where they do not

> "Conflicts are only solved in the upstream, never in the downstream."

The downstream never asks a user handler to decide anything. It collects whatever
the master has and writes it. Conflict decisions happen only when `masterWrite`
returns real master states that differ from `assumedMasterState`.

### 6.3 The default handler, exactly

```ts
// default-conflict-handler.ts
isEqual(a, b) {
    a = addAttachmentsIfNotExists(a);   // if (!d._attachments) { clone; d._attachments = {} }
    b = addAttachmentsIfNotExists(b);
    return deepEqual(stripAttachmentsDataFromDocument(a),
                     stripAttachmentsDataFromDocument(b));
}
resolve(i) { return i.realMasterState; }   // drop the fork, keep the master
```

Reproduce all five behaviours, including the ones that look incidental:

1. **Missing `_attachments` normalises to `{}` on both sides.** A doc with no
   `_attachments` key equals a doc with `_attachments: {}`. Note the helper mutates
   by reassignment *only* when the key is missing, and clones before doing so.
2. **Attachment binary data is stripped before comparing, but attachment
   *metadata* is not.** `stripAttachmentsDataFromDocument` maps each attachment
   through `attachmentWriteDataToNormalData`, keeping `{length, digest, type}` and
   dropping `data`. So two attachments that differ only in blob payload but share
   length/digest/type compare equal — and digests are content hashes, so in
   practice a payload change changes the digest. Short-circuit when there are no
   attachments (upstream returns the input doc, not a clone).
3. **Equality is `deepEqual`, not canonical JSON.** `deepEqual` short-circuits on
   `a === b`, compares constructors, handles arrays by length then element-wise,
   compares `RegExp` by source+flags, uses `valueOf`/`toString` when overridden,
   then compares own keys (count, presence, recursive value). Two consequences:
   - **key order does not matter** for `isEqual`, unlike for checkpoint bytes;
   - `NaN` equals `NaN` (`a !== a && b !== b`).
   In Rust: structural equality over the ordered-JSON value that ignores key order
   for objects and treats `Number` semantically (`-0` and `0` are `===` in JS, so
   they must compare equal; `NaN` equals `NaN` only inside this comparator).
4. **`resolve` is synchronous in effect and total.** It always returns
   `realMasterState` unchanged — same object, not a clone. The protocol then
   stamps a *new* document from it (see 6.4), so returning the master state does
   not leak `_meta`/`_rev` into the fork.
5. **No merge, no heuristics, no timestamps.** The fork loses. Silent, deterministic.

### 6.4 What happens to a resolved document (`resolveConflictError`)

```ts
resolvedDoc = { ...resolved,
    _meta:        { ...forkState._meta },
    _rev:         '',
    _attachments: { ...(resolved._attachments || forkState._attachments) } };
resolvedDoc._meta.lwt = now();
resolvedDoc._rev = createRevision(checkpointKey, forkState);
```

- The **fork's** `_meta` wins (with a fresh `lwt`), because the result is written to
  the fork, not the master.
- `_attachments` prefers the resolved document's own attachments, falling back to
  the fork's.
- The new `_rev` is a normal fork revision: `height(fork._rev)+1` and the
  checkpoint key as token.
- Any other fields of the resolved document survive; `_deleted` is whatever the
  handler returned.

Then, in the upstream:

- `events.resolvedConflicts` emits `{input, output: resolvedDoc}`;
- the resolved doc is written to the fork with `previous: forkState`,
  context `'replication-up-write-conflict'`;
- the meta row for that document gets `docData = realMasterState` and
  `isResolvedConflict = resolvedDoc._rev`;
- if the fork write itself conflicts (409), it is **ignored on purpose**: the
  intervening write will trigger another upstream cycle which resolves again.
  Non-409 errors are `RC_PUSH` and thrown.

That `isResolvedConflict` value is the entire mechanism by which the two directions
agree that "this fork state is a conflict resolution that still has to be pushed".
Both directions compare it against a fork `_rev` (upstream: skip the equality
shortcut; downstream: wait for the upstream before overwriting). Losing it makes
resolved documents get overwritten by the master and the resolution silently lost.

---

## 7. The state machine

### 7.1 Two layers, and the boundary between them

- **Protocol layer** (`replication-protocol/index.ts`): no start/pause API. It
  *exposes* `canceled`, `paused`, `active`, and reads them. Cancelling is
  `cancelRxStorageReplication`.
- **Collection layer** (`plugins/replication/index.ts`): `RxReplicationState`
  owns start/pause/cancel/remove, the retry loop, modifiers, `deletedField`
  swapping, and the public observables.

The port keeps this split. Retry lives in the collection layer because "the
replication protocol itself has no error handling" — a fact worth preserving, since
it means the protocol can be driven by a transport with its own retry policy.

### 7.2 States

| Name | Type | Meaning |
| --- | --- | --- |
| `canceled` | `BehaviorSubject<boolean>` | terminal; a cancelled state can never run again |
| `paused` | `BehaviorSubject<boolean>` | pushes and pulls stop; queue tasks are not buffered |
| `active.down` / `active.up` | `BehaviorSubject<boolean>` | "is this direction doing something right now" |
| `firstSyncDone.{down,up}` | `BehaviorSubject<boolean>` | initial checkpoint iteration finished once |
| `streamQueue.{up,down}`, `checkpointQueue` | promise chain | ordering |
| `firstSyncDone` → public `awaitInitialReplication` | | |

`active` is a *union* at the collection layer: `isActive = down || up`
(`index.ts:459-466`).

### 7.3 Transitions

Only outcomes the caller can observe; internal awaits omitted.

```
                 replicateRxStorageInstance()
                            │
              ┌─────────────┴─────────────┐
              ▼                           ▼
     download RESYNC               upstreamInitialSync → processTasks
              │                           │
   masterChangesSince loop        getChangedDocumentsSince loop
              │                           │
    persistFromMaster(tasks)       persistToMaster batch
              │                           │
     fork.bulkWrite + meta.bulkWrite   masterWrite → conflicts?
              │                           │
    setCheckpoint('down')          resolveConflictError → fork write + meta
              │                           │
   firstSyncDone.down = true      setCheckpoint('up')
                                          │
                                  hadConflicts → upstreamInitialSync again
                                          │ else firstSyncDone.up = true
```

Public operations (`RxReplicationState`):

| Operation | Precondition | Effect |
| --- | --- | --- |
| `start()` | not stopped | queues `_start()` on `startQueue`; a rejection is converted into `error$` and the queue is left usable |
| `start()` on a cancelled state | → rejects with `RC_START_CANCELED` | state is dead |
| `_start()` | — | if already started before (the `wasStarted` flag): **this is resume** — sets `paused=false` and sends a `RESYNC`; otherwise creates the meta instance, connects it to the collection, builds `internalReplicationState`, wires observables, and for `live:false` awaits first-sync + in-sync then cancels |
| `pause()` | queued | sets `paused=true`; does **not** stop the protocol, only gates task intake and re-checks before meta writes |
| `isPaused()` | | `paused && internalReplicationState` |
| `isStopped()` | | `subjects.canceled` — the collection-level flag |
| `isStoppedOrPaused()` | | either |
| `reSync()` | | pushes `'RESYNC'` into `remoteEvents$` (and therefore into both directions' change-streams) |
| `emitEvent(ev)` | | pushes an arbitrary pull-stream item |
| `awaitInitialReplication()` | | awaits `firstSyncDone.down && firstSyncDone.up` |
| `awaitInSync()` | | awaits first-sync, then **twice** awaits `requestIdlePromise` and `awaitRxStorageReplicationInSync` (queue identity check) |
| `awaitDocumentPushed(doc)` | push configured | resolves when this doc state (or a later one) is confirmed on the master |
| `cancel()` / `_cancel(doNotClose)` | | `cancelRxStorageReplication` (sets `canceled`, completes subjects, awaits `checkpointQueue`), closes the meta instance unless `doNotClose`, unsubscribes, completes public subjects, deregisters from the collection registry |
| `remove()` | | cancel without closing, then create-if-missing meta instance, await `checkpointQueue`, `metaInstance.remove()`, remove the connected storage |
| collection `onClose` | | calls `cancel()` |
| non-live start | | runs to in-sync then auto-cancels |

Traps to preserve:

- `start()` must not leave `startQueue` rejected, or every later `start`/`pause`/
  `remove` fails with the first error forever.
- `cancel()` must await `checkpointQueue`, otherwise a checkpoint for data that was
  fetched but never written to the fork can be persisted. The comment says exactly
  this and the guard `!state.events.canceled.getValue()` inside `setCheckpoint` is
  the second half of the same protection.
- A paused replication must not write meta for an unfinished push (section 4.4).
- State transitions are serialised on `startQueue` because "start/pause/cancel/remove
  must never run in parallel to avoid a wide range of bugs".

### 7.4 Options and defaults

| Option | Default | Layer |
| --- | --- | --- |
| `pullBatchSize` / `pushBatchSize` | `100` | collection (`100`), required at protocol level |
| `live` | `true` | collection |
| `retryTime` | `5000` ms | collection |
| `waitForLeadership` | `true` | collection |
| `autoStart` | `true` | collection |
| `toggleOnDocumentVisible` | `true` | collection (browser only) |
| `deletedField` | `'_deleted'` | collection |
| `keepMeta` | `false` | protocol input |
| `skipStoringPullMeta` | `push ? false : true` | collection sets it; pull-only replications default to `true` |
| `initialCheckpoint` | undefined | protocol input, per direction |
| `waitBeforePersist` | undefined | protocol input |

`skipStoringPullMeta` deserves a note: upstream *names* it for pull-only, and the
collection layer sets it true exactly when there is no `push`. Its meaning is
"do not store per-document assumed-master states", and it must not suppress
checkpoint writes.

---

## 8. Byte-identical versus allowed to differ

The framing: **a JS peer must be able to talk to a Rust peer, and a stored value
written by one must be readable and comparable by the other.** Anything on the
wire or in persisted meta is byte-identical; anything internal is free.

### 8.1 Must be byte-identical

| # | Artifact | Why | How |
| --- | --- | --- | --- |
| 1 | Checkpoint JSON | compared with `JSON.stringify`, persisted, and fed back to a possibly-JS peer | `dxdb_json::to_js_json`; ordered map; JS number formatting |
| 2 | `CheckpointType` key **order** after `stackCheckpoints` | determines the bytes in (1) | insertion-order merge; never sort |
| 3 | `stackCheckpoints` merge result | observable in (1) whenever two checkpoints share keys or arrive out of order | shallow left-to-right `Object.assign` semantics |
| 4 | Document states crossing the wire (`masterChangesSince` / `masterWrite` rows) | the entire contract with a JS master | same encoder as (1), ordered key preservation, `_deleted` always present as boolean, `_attachments` stubs `{length,digest,type}` |
| 5 | Meta `id` strings | composed primary key of persisted rows; a JS peer or the same DB reopened must find them | `itemId + '\|' + isCheckpoint`, `separator: '\|'` |
| 6 | Meta schema identity (title, primaryKey composition, `idLength`, `maxLength`s, `required`, `encrypted`) | row validation and schema-hash identity against a JS-written DB | port `getRxReplicationMetaInstanceSchema` arithmetic verbatim |
| 7 | Revision strings | fork revision chain is compared by height and persisted | `str(height) + '-' + token`, token = checkpoint key / identifier hash |
| 8 | Revision **height arithmetic** | `+1` per write, `getHeightOfRevision` parsing, single-digit fast path equivalence | integer parse up to `-`; no content hashing |
| 9 | `_meta` tagging: `_meta[identifier] = height(fork._rev)+1` and `_meta.o = {_rev, hash}` | this is how a replicated write is recognised as "already on the master"; get it wrong and documents are pushed forever or never | exact field names, exact values |
| 10 | `_meta.lwt` **monotonic, two-decimal** shape | it is a JSON number that orders writes, and it is part of what a peer sorts on | same clock discipline (see 8.3) |
| 11 | Default conflict handler equality verdicts | including `_attachments:{}` normalisation, attachment data stripping, NaN==NaN, key-order insensitivity | port the comparator's rules, not `==` on canonical bytes |
| 12 | Resolved-document field contents | the resolution is replicated onward | `_meta` from fork, `lwt` fresh, `_rev` from `createRevision`, `_attachments` preference order |
| 13 | Checkpoint **skip** decision (`JSON.stringify` equality) | decides whether a write happens at all; affects persisted `lwt`/`_rev` history | `to_js_json` comparison |
| 14 | `hashFunction` default | the checkpoint key and meta collection name are persisted names | SHA-256 hex over UTF-8, lowercase, untruncated |
| 15 | `'up'` / `'down'` literals and `'0'`/`'1'` flags | part of persisted `itemId`/`isCheckpoint` | literals, not enums with different spellings |
| 16 | Attachment stub shape | `length`, `digest`, `type` only; blob never in the document, except when a transport serialises it | type + field names |

### 8.2 May differ

| Artifact | Allowed difference | Constraint |
| --- | --- | --- |
| Revision tokens (`databaseInstanceToken`, checkpoint key) | any opaque string, different per run | must be constant for the life of one replication and must not contain `-` (it is the token half of `height-token`) |
| `_meta.lwt` **values** | anything monotonically increasing in ms with 0.01 resolution | ordering must be reproducible, values need not be |
| Task scheduling, batch boundaries within a batch size, coroutine interleaving | free | must not change observable order: per-direction serialisation, checkpoint write order, fork-before-meta-before-checkpoint |
| Concurrency between up and down | free | except the two documented gates: downstream waits for `active.up` before consuming master stream events; downstream waits for `streamQueue.up` when the fork state is a pending conflict resolution |
| Retry loop / backoff | free in shape | must live in the collection layer and must not mark unsent docs as pushed |
| Error types | Rust types, not `RxError` codes | status numbers (409/422/510) and the "throw on non-409" discipline stay |
| Log/message text, statistics naming beyond the listed counters | free | counters in 4.5 stay because tests assert them |
| Memory representation of documents | typed projections vs ordered JSON | the *encoded* form is 8.1 |
| `Blob` versus `Vec<u8>`/`Arc<[u8]>` | free internally | wire form is base64 or a byte channel chosen by the transport; the MIME type and digest are reproduced |
| Order of concurrently-fetched master states, `Object.values`/`Object.keys` iteration over prepared rows | free | must not reorder persisted rows or checkpoint stacking |
| `Object.assign`-clone identity, `flatClone` shallow-copy semantics | free | observable *values* stay identical |

### 8.3 The one genuinely awkward JS dependency: `now()`

```ts
// utils-time.ts
if (dateNow > _lastNowMs) { _lastNowMs = dateNow; _lastNowSub = 1; }
else if (++_lastNowSub === 100) { _lastNowMs++; _lastNowSub = 1; }
return _lastNowMs + _lastNowSub * 0.01;
```

It returns `ms + k*0.01` for `k in 1..=99`, bumping the millisecond when 99 is
exceeded, and it never returns the same value twice. The *values* are inherently
run-dependent, so they are in 8.2. The *shape* is in 8.1: a JSON number with at
most two decimals, strictly increasing within a process. Two float hazards:

- `ms + k*0.01` is not exactly representable for every `k`, and JS is the reference
  implementation of that arithmetic. We must produce the same `Number` (and hence
  the same `to_js_json` digits) as JS for the same inputs — i.e. compute in `f64`
  with the same expression order and format via `dxdb_jsnum`, never via `format!`.
- A Rust clock that only guarantees whole milliseconds would produce ties, which
  upstream's `_meta.lwt` ordering relies on being broken. Keep the sub-counter.

---

## 9. Rust surface

### 9.1 Crate layout (this tranche creates the skeletons)

```
crates/dxdb-replication/
  Cargo.toml
  src/lib.rs
```

Planned modules once traits land (all inside this one crate; no sub-crates, so a
lane-local rename never touches another lane's `Cargo.toml`):

| Module | Ports |
| --- | --- |
| `checkpoint.rs` | `checkpoint.ts` + `stackCheckpoints` |
| `meta_instance.rs` | `meta-instance.ts` (schema, assumed-master read, meta write row) |
| `conflict.rs` | `conflicts.ts` + `default_conflict_handler.rs` |
| `convert.rs` | `helper.ts` (`doc_state_to_write_doc`, `write_doc_to_doc_state`, attachment stripping) |
| `state.rs` | `RxStorageInstanceReplicationState`, queues, stats, events |
| `downstream.rs` | `downstream.ts` |
| `upstream.rs` | `upstream.ts` |
| `protocol.rs` | `index.ts` (construct/cancel/await helpers) |
| `collection.rs` | `plugins/replication/index.ts` lifecycle + retry (behind a thin layer, no RxJS) |
| `attachment.rs` (in `dxdb-attachments`) | attachment model, digest, stub conversion |

### 9.2 What this lane needs from `dxdb-storage` (lane D)

Names below are what the protocol *calls*; the exact spellings are lane D's to
choose. Recorded here so the first review of their traits can be a checklist.

| Need | Shape required by the protocol |
| --- | --- |
| instance `bulk_write(rows, context) -> BulkWriteResponse` | per-row atomic, returns **errors only**, `409` must carry the in-DB document, `context` is echoed onto change events |
| `find_documents_by_id(ids, with_deleted) -> Vec<DocumentData>` | includes tombstones when asked |
| `change_stream() -> Stream<EventBulk>` | each bulk carries `events[].documentData`, `checkpoint`, and the **`context`** of the originating write — the upstream's "ignore my own downstream writes" check depends on that context round-tripping |
| `get_changed_documents_since(limit, checkpoint)` | optional, with the documented query fallback; returned checkpoint must resume exactly after the last returned doc, sorted by `(lwt, pk)` |
| `get_attachment_data(doc_id, att_id, digest) -> Blob` | digest-addressed; a digest mismatch is an error, not a silent read |
| `schema` (primaryPath, `primaryKey.maxLength`, `attachments` flag, `keyCompression`, `version`) | the meta schema arithmetic and `_attachments` gating both read it |
| `close()`, `remove()`, `cleanup()` | lifecycle parity |
| `DocumentData` representation | must be ordered JSON (or expose it losslessly) — 8.1 items 1–5 are impossible otherwise |
| `_meta` access | `lwt`, arbitrary string keys, and the `o` tag; the protocol writes `_meta[identifier]` |

### 9.3 Non-negotiable crate constraints (from the lane brief)

- no `tokio`: `futures` only, executor supplied by the caller
- no `unsafe` (`#![forbid(unsafe_code)]`)
- no dependency on a concrete storage crate; `dxdb-storage` traits only
- builds for `wasm32-unknown-unknown`: therefore no sockets, no `std::time::Instant`
  inside the protocol (the clock is injected), no thread-locals
- the HTTP transport is a caller-side trait implementation, not a dependency

---

## 10. Test plan

### 10.1 The ported suite (the gate)

`replication-base-test-suite.ts` is a `describe`/`it` tree parameterised by six
callbacks:

```ts
startReplication(collection) → replicationState        // live
syncOnce(collection) → Promise<void>                   // one-shot
getAllServerDocs() → Promise<any[]>
cleanUpServer() → Promise<void>
softDeletes?: boolean; isDeleted?(doc); getPrimaryOfServerDoc(doc)
waitTime?: number
syncOnceWithAttachments?(collection)     // optional attachment block
syncOnceWithAttachmentsDisabled?(collection) // optional
```

Cases (each calls `cleanUpServer()` first):

| Block | Cases |
| --- | --- |
| live replication | `push replication to client-server` (insert 2 → initial; insert 1; patch → in sync; delete → soft vs hard count), `two collections` (7 convergence checkpoints incl. parallel inserts, with explicit `reSync()` before each assert) |
| conflict handling | `should keep the master state as default conflict handler` (c1 and c2 diverge; c2 pushes first; after c1 pushes, c1 must show c2's value) |
| attachment replication (opt-in) | insert, update, delete, and `should keep the master attachment state on conflict` |
| attachment disabled (opt-in) | binary data must not replicate, document must |

Port shape in Rust:

- one generic suite function over a `ReplicationTestBackend` trait mirroring the
  config callbacks, so the same body runs against in-memory and filesystem;
- the same case names, so a failure names the same upstream case;
- `ensureReplicationHasNoErrors` becomes a subscriber that panics on `error`;
- `awaitEffectsHaveRun`-style `waitTime` becomes a deterministic "drain queues"
  where possible and a bounded poll otherwise (upstream itself calls `reSync()`
  after `wait()`, which is the deterministic part — reproduce that ordering);
- the suite needs fixtures from `humans-collection.ts` / `schema-objects.ts`
  (`passportId`, `firstName`, `lastName`, `age`) and `awaitCollectionsHaveEqualState`;
  those are lane D's `dxdb-testkit`, so this lane either consumes `dxdb-testkit` or
  keeps a lane-local fixture until it exists.

`JS-only` facilities in this suite and how they are handled:

| Upstream facility | Used for | Handling |
| --- | --- | --- |
| `Blob` | attachment payloads | `dxdb-attachments` byte buffer type; the suite compares bytes + MIME |
| `doc.incrementalPatch` / `incrementalWriteQueue` | conflict-handling case | normal read-modify-write with the current `_rev`; the incremental queue is a collection-layer serialisation, not protocol behaviour |
| `collection.database.close()` | teardown | storage instance `close()` |
| `assert`/`describe`/`it` | structure | `#[test]`/suite harness in `dxdb-testkit` (lane D) |
| `wait()` | eventual consistency | rejected where possible: the suite's own `reSync()` is the deterministic replacement, and the two-process test drives explicit rounds |

### 10.2 Two-process test (gate item 2)

Hermetic: ephemeral port, loopback only, no external network.

```
process A: dxdb + storage + replication (fork)  ──HTTP──▶  process B: dxdb + storage (master)
```

- Bind `127.0.0.1:0`, read the assigned port back, pass it to the child via argv;
  never a fixed port.
- Transport lives in the test, not the crate: a `ReplicationHandler` implementation
  over a tiny request/response protocol carrying ordered JSON.
- Assertions: after N push/pull rounds, both sides have identical document sets
  *including tombstones*, and the checkpoint the client persisted is byte-equal to
  an expected canonical form (`to_js_json`), and re-running a sync from that
  checkpoint produces zero documents.
- Also assert the failure direction: an interrupted push (kill between
  `masterWrite` and meta write) followed by restart converges, and no document is
  reported as pushed before the master has it.

### 10.3 Vectors (gate item 3, lane C)

Consume `testdata/vectors/replication-transcripts.json` when lane C publishes it:
a scripted pull/push sequence in, expected checkpoints and resolved document states
out. Assert our output byte-for-byte via `dxdb_json::to_js_json`. Until it exists,
this lane writes its own transcripts *in the same schema* so the swap is a file
change, not a test rewrite. Any expectation that depends on `now()` must come from
the vector file (an explicit clock), never from the wall clock.

### 10.4 Unit-level obligations

- `stack_checkpoints`: later-wins, first-insertion order, falsy dropped, and a
  three-way merge whose key order differs from any single input.
- `idLength`/`itemId` maxLength arithmetic over a table of primary-key lengths.
- default handler: no-`_attachments` vs `{}`, attachment-data-insensitive but
  digest-sensitive, key-order-insensitive, NaN, `-0`.
- `set_checkpoint`: skip-on-equal (including float formatting), 409 retry using
  `document_in_db`, abort-on-cancel.
- `create_revision`/`height`: 1, 9, 10, 100, and the multi-digit parse.
- `now()`: strictly increasing, two decimals, millisecond bump at 99.

---

## 11. Risks and open questions

1. **The traits do not exist yet.** Every name in 9.2 is a requirement, not a
   guess at lane D's spelling. First action after `dxdb-storage` lands is a
   read-through of the real traits against that table, then a decision on where
   the document type lives. No protocol code before that.
2. **Streams versus futures.** `changeStream()` and `masterChangeStream$` are
   push-based, and the protocol's queue-wake logic (`addNewTask` appending to a
   promise chain; `processTasks` merging whatever is currently open) is written
   against push delivery. `futures::Stream` + a wake channel reproduces it; an
   async `next()` in a loop does **not** (it changes batching and the
   `time < lastTimeMasterChangesRequested` drop rule). This is the biggest
   architectural decision in the port.
3. **Executor-neutral task spawning.** Upstream relies on "fire and forget, then
   await at the end" (`persistFromMaster` in resync, `setCheckpoint` never
   awaited). Without `tokio::spawn`, the caller must supply a spawner. Proposal:
   a `Spawner` capability on the state, defaulted to a no-op that just keeps the
   future in the queue (so sequential execution is the default and bounded
   parallelism is opt-in). Needs agreement with lane D since `dxdb-storage` may
   already define something.
4. **`_meta.lwt` clock injection.** The protocol needs `now()`. Making it a trait
   (or a plain `fn() -> f64` on the state) keeps the crate wasm-clean and makes
   vector tests exact.
5. **`deepEqual`'s `valueOf`/`toString` branches.** They only fire for non-plain
   objects, which cannot appear in decoded JSON. So the Rust comparator can ignore
   them; recorded so a reviewer does not treat it as a dropped case.
6. **`additionalProperties: false` in the meta schema** plus `docData.properties`
   copied from the fork schema means a meta write can fail validation if the fork
   schema is edited without a migration. Upstream has this behaviour; the port
   reproduces it rather than relaxing it.
7. **Attachment serialisation is two different contracts.** The in-protocol one is
   "stub in `_attachments`, blobs fetched by digest". The remote-JSON one
   (`serializeDocAttachments` / `deserializeDocAttachments` /
   `_attachments_data`) is a *transport* shim that must not leak into the protocol
   proper. Planned: `dxdb-attachments` owns the model and digest; the shim lives
   with the transport that needs it.
8. **`rxdb`'s `now()` uses a module-global.** Two replications in one process share
   the clock upstream. Reproduce process-global monotonicity, or make it explicit
   and document the difference — decide when the clock abstraction lands.

---

## 12. Definition of done for this lane

```
make check && make clippy && make test && make wasm     # all exit 0
```

plus:

- the ported suite green against **in-memory and filesystem** endpoints with a
  reported case count;
- the two-process test green with named convergence assertions;
- `testdata/vectors/replication-transcripts.json` consumed and byte-equal;
- a written answer to "which ported cases depend on a JS-only facility, and how
  was it handled" (the table in 10.1 is the draft answer).

---

## 13. Trait-surface review, after `dxdb-storage` landed

Lane D published `crates/dxdb-storage` (traits, document type, revisions,
checkpoints, `categorize_bulk_write_rows` as a pure function, `Spawner`, `Clock`).
This section records the review against the requirements in section 9.2, because a
review done from the source is the only kind worth writing down.

### 13.1 Fit, method by method

| Requirement (9.2) | `dxdb-storage` | Verdict |
| --- | --- | --- |
| `bulk_write` per-row atomic, errors only | `bulk_write(Vec<BulkWriteRow>, String) -> BulkWriteResponse`, `WriteError::Conflict` carries `document_in_db` | matches |
| the context round-trips onto change events | `EventBulk.context` | matches |
| `find_documents_by_id(ids, with_deleted)` | present, `with_deleted: bool` | matches |
| `change_stream()` push delivery | `ChangeStream = Pin<Box<dyn Stream<Item = EventBulk>>>` | matches D2 |
| `get_changed_documents_since` optional with fallback | default body *implements* the upstream fallback, exact rather than approximate | better than required; use the default |
| `get_attachment_data(doc, att, digest)` | returns `Vec<u8>`, digest verified, mismatch is `StorageError::MissingAttachment` | matches |
| schema projection | `StorageSchema { primary_path, version, has_attachments, raw, .. }` | matches; `raw` gives the meta-schema arithmetic its `maxLength` |
| `close` / `remove` / `cleanup -> bool` | present with the upstream contracts documented | matches |
| documents are ordered JSON | `DocumentData(Value)` with `preserve_order`, `to_js_json`, `canonical_bytes` | matches |
| `_meta` open for plugin keys | `DocumentData::meta()`, `set_lwt` leaves other keys | matches |
| execution injected (D3) | `Spawner`, `SequentialSpawner`, `spawns_concurrently()` | matches, and the assertion hook is there |
| time injected (D4) | `Clock`, `MonotonicClock`, `lwt_from_parts` with upstream's expression order | matches |

Async form: the methods are `-> impl Future`, so a call site written against a
concrete `&impl RxStorageInstance` awaits them directly. The dynosaur mirror is
needed only where a `Box<dyn _>` is unavoidable — here, exactly one place:
`underlying_persistent_storage()` returns `Option<&DynRxStorageInstance<'_>>`, so
following the memory-tier indirection is the only dyn call in the crate. No
`dynosaur` dependency is added; it is a transitive type, not something this lane
names.

### 13.2 The one mismatch: `Checkpoint` is typed, the protocol's is opaque

`dxdb-storage` defines

```rust
pub struct Checkpoint { pub id: String, pub lwt: f64 }
```

with `to_json()` → `{"id", "lwt"}` and a strict `from_json`.

That is correct for *the storage's own* change feed, and its `to_json` does satisfy
the round-trip requirement: `lwt` is an `f64` carried in a JSON number, and
`dxdb_json::to_js_json` formats it with JavaScript number semantics. As the
**storage's native checkpoint** it is right, and this lane uses it as exactly that.

It cannot be the *protocol's* checkpoint type, and the reason is upstream's own:

> "Instead of just storing the new checkpoint, we have to stack up the checkpoint
> with the previous one. This is required for plugins like the sharding RxStorage
> where the changeStream events only contain a Partial of the checkpoint."
> — `checkpoint.ts:92-97`

Upstream's `CheckpointType` is a type parameter, `stackCheckpoints` is
`Object.assign` over arbitrary objects, and a checkpoint is only ever passed back
to the caller's own `masterChangesSince`. A sharding checkpoint is not `{id, lwt}`;
a transport may use `{seq}` or `{cursor, region}`. Stacking two `Checkpoint`s would
silently drop every key that is not `id` or `lwt`, which is the data-loss case the
stacking exists to prevent.

**Resolution, without editing lane D's crate:** the protocol's checkpoint is
generic and opaque (`C: JsonCheckpoint`, i.e. any ordered-JSON object), and
`stack_checkpoints` is the `Object.assign` merge over it. `dxdb_storage::Checkpoint`
is one concrete `C` a caller may use — via `Checkpoint::to_json()` / `from_json()`
as its encoding — and that is how the storage-to-storage adapter is written. So
nothing in `dxdb-storage` needs to change; the mismatch is resolved by this lane not
*constraining* the protocol to the storage's convenience type. Recorded here rather
than in `docs/DECISIONS.md` because it does not change a decision: it keeps the
protocol faithful to upstream and leaves the storage type alone.

What the protocol does require of `C`, and asserts in one place (`stack_checkpoints`):

1. `C` encodes to a JSON **object** (not a scalar or an array);
2. its key order survives a round-trip (so `dxdb_json` on both sides);
3. merging is shallow, left-to-right, later-wins, first-insertion key order.

### 13.3 Behaviours the conformance suite needs that are *not* in the traits

Two, both deliberately out of the storage layer:

- **`find()` projection** — upstream's suite calls `collection.find()` and diffs
  `toJSON()`. That is a query plus a projection, i.e. `dxdb-query` + `dxdb-core`.
  Until those exist, the adapter trait in `conformance.rs` asks the endpoint for
  `fork_documents()` instead. When `dxdb-core` lands, an adapter over a real
  collection answers it with a query, and no case body changes.
- **`incrementalPatch` / `incrementalWriteQueue`** — a collection-layer serialisation
  (section 10.1), expressed here as read-modify-write against the current state.

Neither is a gap in `dxdb-storage`; both are above it by design.


---

## 14. Interop with real upstream RxDB — what was proved, and how

`PLAN.md` section 10 asks for "dxdb client <-> upstream RxDB client through the same HTTP
endpoint converges, and checkpoints survive a restart on both sides". That claim needed real
upstream, so this lane provisioned it: `crates/dxdb-replication/tests/interop/vendor.mjs`
fetches the pinned commit and bundles its TypeScript with esbuild, exactly as
`xtask/vectors/vendor.mjs` does. Nothing checked in here was produced by our own code.

### 14.1 A finding first: upstream has no HTTP replication client

The audit item says "through the same HTTP endpoint". Upstream **has no HTTP replication
client to interop with**. Its shipped transports are:

| transport | file | shape |
| --- | --- | --- |
| WebSocket | `plugins/replication-websocket/` | `{id, collection, method, params}` frames |
| GraphQL | `plugins/replication-graphql/` | a user-supplied GraphQL server |
| CouchDB | `plugins/replication-couchdb/` | the CouchDB `_changes` protocol |
| MongoDB, Firestore, Supabase, Appwrite, NATS, Google Drive, OneDrive, WebRTC | their own providers | |

Every `RxReplicationHandler` is caller-supplied. There is no generic "RxDB HTTP endpoint"
whose wire format we could match — the HTTP shape in `tests/common/http_transport.rs` is
**ours**, invented for the two-process test, and no upstream client speaks it.

So interop was proved over **WebSocket**, which is the one transport where upstream ships both
a client and a server, and whose frame format is upstream's own (`websocket-types.ts:37-45`)
rather than one we designed. That is a stronger test than HTTP would have been, and it is the
shape `crates/dxdb-replication/src/websocket.rs` already implements.

**Consequence for the HTTP test:** it stays as a transport-agnostic convergence test and should
not be described as interop. If a generic HTTP endpoint is wanted, it is a new design, not a
port, and `docs/DECISIONS.md` should say so.

### 14.2 Direction 1: upstream client → dxdb endpoint

`client-upstream.mjs`. Genuine `replicateWithWebsocketServer` from rxdb@17.6.0 at the pin,
against an endpoint that speaks our frames and applies our `upstream.rs` write rules.

| assertion | result |
| --- | --- |
| dxdb → upstream (a document seeded on the endpoint appears in upstream) | **pass** |
| upstream → dxdb (a document inserted in upstream reaches the endpoint) | **pass** |
| checkpoint equality | **pass** — upstream persisted `{id: "from-upstream", lwt: 102}` and our endpoint's own checkpoint was byte-identical, including the `lwt`, because upstream adopted the value we assigned |
| restart re-sends nothing | **pass** — `restartResends: []`, 3 frames after restart (a stream request and two pulls, no `masterWrite`) |
| frames understood | 8, zero rejected |

**Honest limitation, stated so it is not overclaimed:** direction 1 is a *wire-format* test, not
a whole-process test. Node cannot dial into `cargo test`, so the dxdb half is re-implemented in
JavaScript from the Rust behaviour, with each rule citing the Rust source it mirrors. It proves
an upstream client's frames are understood and its checkpoints land where ours do. It does
**not** prove our Rust server process talks to an upstream client. Direction 2 is the one that
runs our shape against a genuine upstream server.

### 14.3 Direction 2: dxdb client → upstream server

`client-dxdb.mjs`. Our client frame shapes against genuine `startWebsocketServer`. This is the
reference implementation of the endpoint, so a frame-shape mistake in our code fails here.

| assertion | result |
| --- | --- |
| `masterChangesSince` returns documents and a `{id, lwt}` checkpoint | **pass** |
| `masterWrite` returns conflicts **only** (empty array for an accepted write) | **pass** |
| the pushed document exists in upstream's collection | **pass** |
| `masterChangeStream$` is handled as a stream subscription, not an RPC | **pass** — upstream answers with `id: 'stream'` frames, matching what `websocket.rs` documents |

### 14.4 Two upstream behaviours the interop run surfaced

1. **`ignoreDuplicate: true` requires dev mode.** `createRxDatabase` throws `DB9` outside dev
   mode when `ignoreDuplicate` is set (`rx-database.ts:874-880`).
2. **A schema that omits `_deleted` from `properties` fails at collection creation** with
   `CI1 fieldName: "_deleted"`, because `fillWithDefaultSettings` prepends `_deleted` to every
   index (`rx-schema-helper.ts:271`) and `getIndexMeta` then looks up
   `properties._deleted` (`custom-index.ts:61-63`). Our Rust `human_schema()` also omits it,
   which is harmless today because nothing consumes indexes — but it will bite the moment
   `dxdb-query` indexes a replicated collection.

### 14.5 Reproducing

```bash
cd crates/dxdb-replication/tests/interop
npm install          # pinned deps from upstream's own package.json at the pin
node run.mjs         # both directions; exits non-zero on failure
```

Hermetic: every server binds `127.0.0.1:0`, ports are read back from the OS, nothing leaves
the machine, and upstream is fetched from the pin rather than checked in. `node_modules/` is
gitignored (47 MB). The upstream checkout and the bundle are cached under
`~/.cache/dxdb-vectors/` and shared with the vector generator.

**Not yet wired into `make`**: `Makefile` and `xtask/` are leader-owned. Requested target:

```make
interop:
	@cd crates/dxdb-replication/tests/interop && node run.mjs
```
