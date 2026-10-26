# Lane E (M3 replication) — continuation

Written at a context reset. Read this top to bottom, then `docs/lanes/m3-port-plan.md`
(§13 is the trait review), then `docs/DECISIONS.md` D1–D7. Nothing else is needed to
resume.

Worktree: `/home/bitnom/Code/dxdb-worktrees/m3-replication`, branch `lane/m3-replication`.

---

## 1. Completed and merged to master

Merged (in master as of `60a634b`, the commit this branch is based on):

- **M3 tranche 1** — `docs/lanes/m3-port-plan.md` (~1,100 lines) + `crates/dxdb-replication`
  and `crates/dxdb-attachments` skeletons.
- **Ported conformance suite** — `src/conformance.rs` + `tests/replication_conformance.rs`,
  with the upstream case table (8 cases, each citing its `replication-base-test-suite.ts`
  line) and a test asserting the table, citations and functions stay in step.
- **Protocol layer 1** — `checkpoint.rs` (opaque ordered-JSON checkpoint, `Object.assign`
  stacking, checkpoint key, set/last with the 409 retry), `conflict.rs` (default handler
  exact + `resolve_conflict_error`), `convert.rs` (doc-state ⇄ write-doc, `_meta.o` write
  tag), `meta_instance.rs` (generated meta schema and its id arithmetic).
- **State machine** — `state.rs`: `TaskQueue` (call-order FIFO replacing the promise
  chain), events, stats, pause/resume/cancel, Spawner and Clock wiring.
- **Upstream (push)** — `upstream.rs`, plus `tests/upstream_push.rs` (11 tests).
- **HTTP two-process test** — `tests/two_process.rs` (6 tests) over a hand-rolled
  `std::net` HTTP/1.1 server on an ephemeral port. This closed one of the leader's four
  section-10 red audit items.
- **Vector oracle consumer** — `tests/replication_vectors.rs` against lane C's
  `testdata/vectors/replication-transcripts.json`.

Commit hashes on this branch were squashed by the leader's merges; `git log master` is the
record. Merged commits I made that are in master: `2cdf6c3`, `8207f86`, `8a66e4f`, and the
earlier `c6f7c29` / `bd70c7c`.

---

## 2. In progress right now (uncommitted, all four gates green)

**All four gates pass on the current worktree**: `make check` 0, `make clippy` 0,
`make test` 0, `make wasm` 0.

**106 tests in `dxdb-replication`** (62 lib, 11 upstream_push, 10 replication_conformance,
8 replication_vectors, 6 attachment_cases, 6 two_process, 3 websocket_convergence).
Workspace total 872.

### New files (untracked)

| File | What it is | State |
| --- | --- | --- |
| `src/downstream.rs` | The pull. Ports `downstream.ts` (550 lines): checkpoint iteration, change-stream batching, the 4-way three-state comparison, `_meta.o` tagging, `lift_attachment_payloads` | compiles, exercised by `attachment_cases`, `two_process`, `websocket_convergence`. **No dedicated `tests/downstream.rs` yet.** |
| `src/attachments.rs` | `fill_write_data_for_attachments_change` — ports `attachments-utils.ts:34-84`. Populates `WriteToMasterRow::attachment_payloads` | compiles, 2 unit tests + used by the push |
| `src/websocket.rs` | The WebSocket adapter: frame format `{id, collection, method, params}` → `{id, collection, result}`, `Method`/`Request`/`Response`/`Frame`, `serve()`, `WebSocketHandler` | compiles, 8 unit tests |
| `tests/attachment_cases.rs` | **All five opt-in attachment cases ported**, each citing its upstream line, plus one mechanism test | 6/6 pass |
| `tests/websocket_convergence.rs` | Two dxdb instances converge over a real RFC 6455 socket on `127.0.0.1:0` | 3/3 pass |

### Modified files

- `src/conformance.rs` — all 8 upstream cases now `Backing::Ported`; `PortedDeferred`
  removed entirely (see §4.2). `rust_test` names are `module::function`.
- `src/state.rs` — `WriteToMasterRow` gained `attachment_payloads:
  BTreeMap<String, Vec<u8>>` + `new`/`with_payloads`/`has_payloads`; `ReplicationState`
  gained `identifier_value()` and `identifier_hash()` (see §4.3).
- `src/upstream.rs` — calls the attachment filler before `master_write`.
- `src/lib.rs` — module list.
- `tests/common/memory_storage.rs` — the throwaway `RxStorageInstance` fixture. Its `510`
  rule was corrected (see §4.4).
- `tests/replication_conformance.rs` — case-table self-check updated for the new names.

### Not yet done in this tranche

- **`docs/lanes/m3-port-plan.md` §14 — the per-adapter ordering table.** This is the
  leader's third request and the only deliverable missing. I gathered the evidence (§5
  below has it) but did not write the section.

---

## 3. Exact next task

### 3a. Write `docs/lanes/m3-port-plan.md` §14: the adapter ordering table

Append a section after §13. The leader wants it to sequence M6 honestly, so for **each** of
the ten remaining adapters give: upstream size, what the provider protocol is, what auth it
needs, what its checkpoint shape is, and the specific thing that blocks it.

Measured facts (already verified with `wc -l` and grep against the pin):

| upstream directory | lines |
| --- | --- |
| `replication-google-drive` | 2421 |
| `replication-webrtc` | 2314 |
| `replication-microsoft-onedrive` | 2053 |
| `replication-graphql` | 900 |
| `replication-websocket` | 638 (**done**) |
| `replication-mongodb` | 578 |
| `replication-firestore` | 538 |
| `replication-couchdb` | 475 |
| `replication-supabase` | 435 |
| `replication-nats` | 347 |
| `replication-appwrite` | 338 |

Checkpoint shapes verified in source — this is the strongest evidence for D7:

- **CouchDB** (`couchdb-types.ts:8`): `{ sequence: number }` — one field, no id/lwt pair.
- **MongoDB** (`mongodb-types.ts:17-30`): `{ iterate: 'changestream' | 'docs-by-id',
  changestreamResumeToken, docId? }` — a **partial/stateful** checkpoint with a mode field.
  This is the case that proves the protocol's checkpoint must be opaque and stackable.
- **Firestore** (`firestore-types.ts:18`): `{ id, ... }` with a timestamp stored as an ISO
  **string** (`'2022-11-30T11:18:25.141Z'`) because Firestore's native
  `{seconds, nanoseconds}` cannot be queried directly.
- **GraphQL**: `CheckpointType | undefined` (`index.ts:125`) — entirely caller-defined.

Suggested ordering rationale to state explicitly (I formed this from the source, the leader
wants the reasoning):

1. **websocket** — done. No provider semantics.
2. **couchdb** — the canonical checkpoint-based replicate protocol; validates the
   checkpoint stack against a real implementation, and its `sequence` is trivially stackable.
3. **graphql** — checkpoint is caller-defined, so it validates that the protocol really is
   generic (it is the adapter most likely to expose a leak).
4. **supabase** / **appwrite** — ordinary REST + PostgREST/Appwrite filters; auth is a
   bearer/JWT header; no exotic semantics.
5. **nats** — message-stream transport, checkpoint from the stream sequence.
6. **firestore** — needs the string-timestamp conversion and its query semantics.
7. **mongodb** — the partial/stateful checkpoint; needs the two-phase `iterate` mode.
8. **google-drive** / **microsoft-onedrive** — file-based transports with OAuth, conflict
   detection over files, and the remote-JSON attachment serialisation
   (`serializeDocAttachments` / `_attachments_data`). Largest surface, most auth.
9. **webrtc** — needs a signalling layer (three variants upstream: nostr, p2pcf,
   simple-peer) and a connection lifecycle; last.

Also state, per adapter, the two things that are **not** the adapter's job: reconnection
and backoff belong to a binding (see `src/websocket.rs` docs), and the HTTP/socket server
belongs to the transport crate (B5), not to `dxdb-replication`.

### 3b. Then commit

The current tree is green but uncommitted. Commit it **before** starting §14 — the leader
has twice asked for incremental commits, and this is ~5 files of real work. Suggested
message shape: M3: all five attachment cases ported, the pull, and the WebSocket adapter.

### 3c. Files §14 will touch

- `docs/lanes/m3-port-plan.md` only.

---

## 4. Things I learned that are not written down

### 4.1 The pull and push have different checkpoint feeds, and pairing them is a bug

An `up` checkpoint is a position in the **fork's** change feed
(`getChangedDocumentsSince(state.input.forkInstance, ...)`, `upstream.ts:174`). A `down`
checkpoint is a position in the **master's** feed. They are not interchangeable. I wrote a
test that asserted an `up` checkpoint resumes the master cleanly; it failed because of a
fixture coincidence, not a protocol rule. Do not assert cross-feed equivalence.

### 4.2 All eight conformance cases are now `Ported`; there is no deferred variant

`Backing` has exactly one variant. Adding a case that cannot run now requires **inventing**
a deferred variant and justifying it, which is deliberate. Do not reintroduce
`PortedDeferred` casually.

### 4.3 Two different hashes exist, and using one where the other belongs is silent

- `checkpoint_key` = `"rx_storage_replication_" + hash(identifier || databaseName || collectionName)`
  (`checkpoint.ts:145`). Used for **meta row revisions**.
- `identifier_hash` = `hash(identifier)` alone (`downstream.ts:67`). Used for the
  **`_meta.o` write tag** and the fork revision token on downstream writes.
- `identifier_value` = the raw identifier string. Used as a **`_meta` key**:
  `_meta[identifier] = next revision height` (`downstream.ts:445`).

All three are on `ReplicationState`. Swapping them changes which writes look self-authored
and nothing fails loudly.

### 4.4 The `510` attachment rule is narrower than "payload required"

Upstream (`rx-storage-helper.ts:385-407`) returns `510` only when an attachment is **new
relative to the previous stored document** and carries no data. An attachment the stored
document already had may be written stub-only, because its bytes are already present. This
matters because the pull copies the master's stubs and does not re-send payloads it was not
sent. My fixture originally checked "do we know this digest", which rejected the pull's
legitimate write.

And: the **master** attaches all attachment payloads in `masterChangesSince`
(`index.ts:241-247`) — "the master never knows the client state of the document. Therefore
we always send all attachments data". The pull receives `_attachments[id].data` inline and
`downstream.rs::lift_attachment_payloads` moves it into `DocumentWriteData`.

### 4.5 `ReplicationHandler` is deliberately `!Send`

A database is single-threaded (crate docs, D1). Consequences for any test that wants a
network server:

- The handler **cannot** go into a `thread::spawn` or be shared across threads.
- `tests/two_process.rs` works because its server is a plain HTTP handler with no protocol
  handler inside it.
- `tests/websocket_convergence.rs` needs the *protocol* on both sides, so the client and
  server must share a thread. The `LoopbackSender` dials a real socket, then pumps the
  server **inline** for exactly one frame. That is why its server is a listener the client
  pumps rather than a background thread. Do not "fix" this into a thread.

### 4.6 Two design findings, both reported and both benign

1. **`WriteToMasterRow::attachment_payloads` is a field upstream's row does not have.**
   Upstream embeds `Blob`s inside `_attachments[id].data`; a Rust document is ordered JSON
   and cannot hold bytes. The field is populated by the protocol's own attachment step and
   ignored by transports without attachments, so it is not adapter leakage into the
   protocol — but it does mean a transport must decide how to put bytes on the wire. The
   WebSocket adapter puts them back at `_attachments[id].data` as a byte array, which is
   where a JS peer looks.
2. **The transport trait did not need anything else from the adapter.** It stayed at three
   methods. If a future adapter needs a fourth, that is a real finding worth stopping for.

### 4.7 Test-harness traps that cost real time

- `TcpStream::read` in a handshake loop **can over-read** past `\r\n\r\n`. For the
  one-call-per-connection shapes here it has not bitten, but a keep-alive design must
  buffer.
- `eprintln!` output is **lost** when a test aborts, which makes `--nocapture` useless for
  a hang. Write diagnostics to a file, or (better) assert on counts in the message.
- A test that hangs hits the 10s `set_read_timeout` and then reports a misleading error.
  When a socket test takes ~10s, suspect an ordering deadlock, not a slow machine.
- `cargo remote-3000 ... test -- -p crate --test name` — `--nocapture` must go after a
  second `--`.
- **Never** wrap a `make` target in `flock` (it deadlocks the fleet), and **never** call
  `cargo remote-3000` directly: use `make`, which picks the right route via
  `xtask/remote-route.sh`.
- **Never run `cargo fmt --all`** — it reformats other lanes' crates. Use
  `rustfmt --edition 2021 <my files>` only.

### 4.8 `try_get_height_of_revision`, not `get_height_of_revision`

A fork document that has never been through a master has `_rev: ""`, which has no dash.
`get_height_of_revision` **panics** on that; `try_get_height_of_revision` returns `None`.
Any fixture that compares revisions across sides must use the `try_` form.

### 4.9 Storage fixtures must stamp `_meta.lwt`

A storage owns `lwt`; it must advance a monotonic clock on every write. My first fixture
reused the incoming value, so every document shared one timestamp, and a change feed ordered
by `(lwt, primary)` could report "nothing after the checkpoint" while a larger-primary
document still existed. Data loss wearing a passing test's clothes. The fixture now stamps
`ms + k*0.01` from a fixed epoch.

---

## 5. Blocked on / questions

**Nothing blocking.** No question needs answering to continue.

Two forward-looking notes for the leader (not blockers):

1. `dxdb-query` and `dxdb-core` now exist (`dxdb-core` is ~3,200 lines with
   `Document`/`Collection`/`Database`/hooks/queues). But `dxdb-core` has **no
   `put_attachment` / `get_attachment`** — that is the collection-layer API the five
   attachment case *bodies* call upstream. The ported cases work at the storage level
   instead, which covers the protocol's payload handling but not collection-layer
   argument handling. `docs/lanes/m3-port-plan.md` should say so when the leader sequences
   the attachment plugin.
2. `dxdb-attachments` (my crate) is still a 41-line doc-only stub. The digest and stub
   shapes it was meant to own now live in `dxdb-storage` (`AttachmentData`,
   `AttachmentWriteData`, `sha256_hex`) and in `dxdb-replication::attachments`. Either the
   crate should be filled with the `RxAttachment` handle + `preMigrateDocument` when the
   collection layer lands, or it should be deleted and its scope folded into
   `dxdb-storage` — worth a leader decision, recorded in `docs/DECISIONS.md`.
