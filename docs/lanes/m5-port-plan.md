# Lane G port plan: WASI, Node, filesystem, SQLite

Status: preparation tranche complete. Design document; no implementation yet.
Owner: lane G (`lane/m5-wasi-node`). Branch base `master` at `12567a9`.
Upstream reference: `~/Code/rxdb` @ `3b763ee17d62bc96cfae556ea0ccf92f9891c8c1` (v17.6.0).

This document is the answer to "what exactly are we building" before any of it is
built. It exists as a separate artifact because the licensing position differs
between the four crates in this lane, and a reviewer needs to see which sentence
came from reading Apache-2.0 source and which one is our own engineering.

## 0. Reading map: what this lane may read, and what it must not

Three different rules apply inside one lane. Getting them confused is the single
easiest way to poison the clean-room program, so they come first.

| Crate | Upstream source available | Rule |
| --- | --- | --- |
| `dxdb-sqlite` | `src/plugins/storage-sqlite/` — Apache-2.0, 1,746 lines | **Port it.** Read it, translate its behaviour. Cite file:line. |
| `dxdb-filesystem` | none. `storage-filesystem-node` and `storage-filesystem-expo` ship in `rxdb-premium` | **Do not read it.** Apache-2.0 source may not be read for this crate either, per CLEANROOM §2 role separation. Design from public docs + our own engineering. |
| `dxdb-wasi` | no upstream counterpart exists in any form | **Ours.** No upstream to port. |
| `dxdb-node` | napi surface has no upstream counterpart; the *behaviour* it exposes is core RxDB's | **Ours**, behind a ported-behaviour boundary (see §5). |

The upstream plugins directory confirms this split by omission. `ls
src/plugins/` contains `storage-sqlite` and `storage-foundationdb`, but no
filesystem storage: the two filesystem storages named in CLEANROOM §4 rows 4 and
5 are premium. `grep -rn "getRxStorageFilesystemNode" docs-src/docs/` shows it
imported from `rxdb-premium/plugins/storage-filesystem-node`, which settles it.

A consequence worth stating plainly: **`dxdb-sqlite` and `dxdb-filesystem` are
not the same kind of artifact.** The SQLite crate is a port of a trial-grade
Apache-2.0 implementation whose own source says it has "no indexes, no
attachments, and a ~500-document cap"
(`sqlite-storage-instance.ts:147`). The filesystem crate has no reference
implementation we are allowed to read. They will therefore diverge in maturity,
and the port plan should not pretend otherwise.

Regression check for a reviewer:

```bash
ls ~/Code/rxdb/src/plugins/ | grep -c filesystem   # expect 0
grep -rn "rxdb-premium/plugins/storage-filesystem" ~/Code/rxdb/docs-src/docs/ | wc -l  # expect > 0
```

## 1. `dxdb-sqlite`

### 1.1 What the Apache-2.0 source actually does

Read in full: `sqlite-types.ts` (151), `sqlite-helpers.ts` (286),
`sqlite-basics-helpers.ts` (657), `sqlite-storage-instance.ts` (588),
`index.ts` (64).

The architecture is worth naming precisely because we are *not* copying it:

- `SQLiteBasics<DB>` is a six-method adapter interface — `open`, `all`, `run`,
  `setPragma`, `close`, plus a `journalMode` policy — declared at
  `sqlite-types.ts:26-73`. Upstream ships **eight** implementations of it
  (`getSQLiteBasicsNode`, `...NodeNative`, `...Capacitor`, `...QuickSQLite`,
  `...ExpoSQLite`, `...ExpoSQLiteAsync`, `...WebSQL`, `...Wasm`, `...Tauri`,
  `sqlite-basics-helpers.ts`). That is the adapter soup CLEANROOM §5.3 refers to.
- The on-disk schema is a single table per `collectionName + '-' + version`, with
  columns `id`, `revision`, `deleted`, `lastWriteTime`, `data` — and `data` holds
  the entire document as a JSON string (`sqlite-storage-instance.ts:389-397`).
  There is no index table and no `CREATE INDEX`. Confirmed by grep: the only
  `CREATE TABLE` in the file is that one.
- Therefore **every query is a full table scan filtered in JavaScript.**
  `query()` reads `SELECT data FROM "table"` (`:322`), then runs
  `getQueryMatcher` and a `createTopKDocumentsCollector` over the decoded rows
  (`:306-341`). Same for `findDocumentsById()`, which scans all rows and filters
  by an `ids.includes()` membership test (`:352-380`).
- `count()` calls `query()` and returns `documents.length` (`:344-350`).
- Transactions are serialised per database connection with a promise chain in a
  `WeakMap<DB, Promise<void>>` (`sqlite-helpers.ts:167-186`), and
  `openSqliteTransaction` retries `BEGIN;` in a loop with a `promiseWait(0)`
  back-off (`:191-228`). The source carries an explicit TODO that it should
  listen for the other commit instead of looping.
- Attachment support is a hard throw (`:405-407`), and `createStorageInstance`
  throws `SQL1` if `params.schema.attachments` is set (`:474-476`).

Useful details we *will* carry over, because they are behaviour rather than
implementation, all from Apache-2.0 code:

- `getDataFromResultRow` handles both the object shape `{id, data}` and the
  positional `[id, revision, deleted, lastWriteTime, data]` shape, preferring
  index 4 and falling back to index 0 (`sqlite-helpers.ts:62-77`). Adapters
  genuinely return both.
- Boolean parameters must be coerced to `0`/`1` before binding, because SQLite
  has no boolean type and several bindings reject `true`/`false`
  (`boolParamsToInt`, `:257-271`; `mapNodeNativeParams`, `-basics-helpers.ts:118-130`).
- `ensureParamsCountIsCorrect` counts `?` placeholders against the parameter
  array in dev mode (`:230-247`). It is a cheap invariant that catches the most
  common prepared-statement bug class.
- `_meta.lwt` is a float (milliseconds) and is stored in an INTEGER column. It
  round-trips because the values are integral in practice; a design that relies
  on sub-millisecond `lwt` would silently truncate. We keep the column as
  `INTEGER` and treat `lwt` as bounded by `Number.MAX_SAFE_INTEGER`, which is the
  same assumption upstream makes.

### 1.2 The `SqliteDriver` adapter trait (our design)

CLEANROOM §5.3 fixes the shape: "our own schema DDL, our own prepared
statements, our own adapter trait (`SqliteDriver`) with implementations for
`rusqlite` and `sqlite-wasm-rs`. Strictly better position than upstream: we own
both sides, so no adapter soup."

The difference from `SQLiteBasics` is not cosmetic. `SQLiteBasics` is
string-SQL-in, string-SQL-out with `any`-typed databases, so every call site
formats SQL and every adapter re-parses it. We own both sides, so the trait can
be typed on the operation and let one implementation own the SQL text.

```rust
// crate: dxdb-sqlite, module: driver
// Object-safe, sync, no async_trait in the hot path (PLAN.md 3.3).

/// Which backend a driver handle is bound to. Narrows a handful of behaviours
/// (journal-mode policy, transaction-start retry) that differ by host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriverKind { Native, Wasm32Wasi, Wasm32Unknown }

/// A bound parameter. There is deliberately no `bool`: SQLite has no boolean
/// type, and making callers write `Param::Int(0/1)` prevents the
/// "binding rejects true" class of bug by construction rather than by a
/// coercion pass at call time.
#[derive(Clone, Copy, Debug)]
pub enum Param<'a> {
    Null,
    Int(i64),
    Real(f64),
    Text(&'a str),
    Blob(&'a [u8]),
}

/// One decoded result row. Columns are positional and borrowed from the
/// statement's scratch buffer, so a row is only valid inside the callback it
/// was handed to.
pub trait Row<'a> {
    fn is_null(&self, col: usize) -> bool;
    fn get_i64(&self, col: usize) -> Result<i64, SqliteError>;
    fn get_f64(&self, col: usize) -> Result<f64, SqliteError>;
    fn get_text(&self, col: usize) -> Result<&'a str, SqliteError>;
    fn get_blob(&self, col: usize) -> Result<&'a [u8], SqliteError>;
}

pub trait SqliteDriver {
    type Db;
    type Stmt;

    fn kind(&self) -> DriverKind;

    fn open(&mut self, path: &Path) -> Result<Self::Db, SqliteError>;
    fn open_in_memory(&mut self) -> Result<Self::Db, SqliteError>;

    /// `PRAGMA <key> = <value>`. Separate from `execute` because several
    /// SQLite bindings return no rows for pragmas and some throw if asked for
    /// them (upstream's reason for `setPragma` existing at all,
    /// `sqlite-types.ts:48-55`).
    fn set_pragma(&mut self, db: &Self::Db, key: &str, value: &str) -> Result<(), SqliteError>;

    /// Compile once, step many. `prepare` is where we avoid upstream's
    /// re-parse-per-call: the statement cache lives above this layer keyed on a
    /// closed enum of statements, not on SQL text.
    fn prepare(&mut self, db: &Self::Db, sql: &str) -> Result<Self::Stmt, SqliteError>;

    fn execute(&mut self, db: &Self::Db, stmt: &Self::Stmt, params: &[Param<'_>])
        -> Result<u64, SqliteError>;

    fn query<'a>(
        &'a mut self,
        db: &'a Self::Db,
        stmt: &'a Self::Stmt,
        params: &[Param<'_>],
        on_row: &mut dyn FnMut(&dyn Row<'a>) -> Result<(), SqliteError>,
    ) -> Result<(), SqliteError>;

    fn begin(&mut self, db: &Self::Db) -> Result<(), SqliteError>;
    fn commit(&mut self, db: &Self::Db) -> Result<(), SqliteError>;
    fn rollback(&mut self, db: &Self::Db) -> Result<(), SqliteError>;
    fn close(&mut self, db: Self::Db) -> Result<(), SqliteError>;
}
```

Five design decisions, each with the reason it beats upstream's arrangement:

1. **`Param` has no `Bool`.** The `boolParamsToInt` / `mapNodeNativeParams`
   pair exists because the adapter layer lost the type information. Removing
   `bool` from the enum deletes the bug and both coercion functions.
2. **`prepare` is separate and statements are named by a closed enum** in the
   storage layer (`Stmt::InsertDoc`, `Stmt::SelectAll`, `Stmt::DeleteTombstones`,
   `Stmt::SelectById`). A `HashMap<Stmt, PreparedStatement>` per connection means
   the SQL text is compiled once per connection instead of once per call, which
   is the difference between upstream's `SELECT data FROM t` per operation and an
   actual prepared statement.
3. **Rows are callback-delivered, not collected into a `Vec`.** `rusqlite`'s
   `Rows` and `sqlite-wasm-rs`'s cursor both borrow the statement; a
   `Vec<DecodedRow>` forces either a copy per row or a lifetime that does not
   survive the trait object. A `FnMut` sink lets the storage layer match and
   collect only the documents that pass the matcher, so a full scan does not
   allocate a full table's worth of `String`s on the way to discarding them.
4. **No `any`, no `debug_id`, no per-library `WeakMap`.** Upstream keys adapters
   on the *library object* (`BASICS_BY_SQLITE_LIB: WeakMap`,
   `-basics-helpers.ts:14`). Rust generics make that unnecessary.
5. **`DriverKind` exists rather than being inferred**, because exactly one
   behaviour differs: journal mode. Upstream encodes the policy as a
   per-adapter `journalMode` string field with five different values
   (`WAL2`, `WAL`, `''`, …), which is a capability masquerading as a setting.
   We expose `DriverKind::supports_wal()` and let the storage ask.

### 1.3 `rusqlite` implementation (native)

- Crate `rusqlite` 0.41, default features. Feature audit: `rusqlite`'s default
  feature set already includes `ffi-sqlite-wasm-rs`
  (PLAN.md §9), so the same dependency line serves native and wasm32 with no
  fork and no `cfg`-driven feature surgery in our manifest. Verified by probe,
  §7.2.
- Connection: `Connection::open(path)` for a file, `open_in_memory()` for
  `:memory:`. `rusqlite::Connection` is `!Sync`, so a connection is owned by one
  `SqliteStorageInstance` and the trait is `&mut self`.
- Statements: `Connection::prepare_cached` under our enum key; `Statement::raw_bind_parameter`
  / `raw_query` for the `Row` wrapper so the `Param` enum maps without a
  conversion layer.
- Journal mode: `PRAGMA journal_mode = WAL` on open for file databases,
  `PRAGMA synchronous = NORMAL`, `PRAGMA busy_timeout = 5000`. The busy timeout
  is the honest replacement for upstream's `BEGIN`-retry loop
  (`sqlite-helpers.ts:191-228`): SQLite already has a mechanism for "another
  writer holds the lock, wait", and re-implementing it in a `while` loop with a
  microtask back-off burns CPU that `busy_timeout` spends sleeping. Same
  behaviour, kernel-assisted.
- Transactions: `BEGIN IMMEDIATE` rather than `BEGIN`. Upstream's plain `BEGIN`
  acquires no write lock until the first write, which is exactly why two
  concurrent `bulkWrite` calls can both get past `BEGIN` and then deadlock into
  the retry loop. `IMMEDIATE` takes the write lock up front, so contention shows
  up at `begin()` where `busy_timeout` handles it.
- `unsafe`: none. `rusqlite` encapsulates the FFI. The crate keeps
  `#![forbid(unsafe_code)]`.

### 1.4 `sqlite-wasm-rs` implementation (wasm)

- Crate `sqlite-wasm-rs` 0.6.2, pulled in transitively through `rusqlite`'s
  default `ffi-sqlite-wasm-rs` feature. Our manifest should not name
  `sqlite-wasm-rs` directly unless a family/API is needed that `rusqlite` does
  not re-export; naming it directly while also letting `rusqlite` pull it invites
  two versions of the same FFI in one tree.
- **VFS selection is the load-bearing detail.** On wasm32, `rusqlite` resolves to
  the in-memory VFS by default, so an OPFS-backed database silently becomes an
  ephemeral one unless the VFS is installed explicitly. Two supported
  configurations, and only two:
  - `wasm32-wasip1` / `wasm32-wasip2`: the `wasm32-wasi-vfs` feature, so file
    paths resolve against the WASI preopens. This is the configuration the M5
    conformance run uses, because a preopened directory is exactly the capability
    the gate is testing.
  - `wasm32-unknown-unknown`: in-memory VFS, **or** the `sqlite-wasm-vfs` SAH-pool
    which is worker-only (PLAN.md §5.4). Browser OPFS is lane F's territory
    (§1.6); this crate exposes the driver and documents the constraint, it does
    not wire up a worker.
- The driver must refuse to open a file path when the active VFS is in-memory,
  with an error that names the VFS rather than a generic `SQLITE_CANTOPEN`. Silent
  data loss on reload is the failure mode we are designing against here.
- Other wasm caveats that go in the crate docs: no threads, so no parallel
  statements on one connection; `sqlite3_step` blocking a worker is the expected
  cost; the JS-side glue for the SAH-pool is not ours.

### 1.5 Schema DDL (our design, and where it departs from upstream)

The ported *behaviour* is the storage contract. The *schema* is ours. Upstream's
one-table-plus-JSON-blob design is a direct consequence of its trial limits (no
indexes), and copying it would copy the limitation.

```sql
-- one table per collection+version, same naming as upstream so the
-- "collectionName + '-' + schema.version" identity is recognisable
CREATE TABLE IF NOT EXISTS "<collection>_<version>" (
    id            TEXT    NOT NULL PRIMARY KEY,       -- primaryPath value
    revision      TEXT    NOT NULL,
    deleted       INTEGER NOT NULL CHECK (deleted IN (0,1)),
    lwt           INTEGER NOT NULL,                   -- _meta.lwt
    data          TEXT    NOT NULL                    -- ordered JSON, see dxdb-json
) STRICT;

-- The three access paths the storage contract actually needs.
CREATE INDEX IF NOT EXISTS "<collection>_<version>_deleted_lwt"
    ON "<collection>_<version>" (deleted, lwt, id);      -- cleanup() + getChangedDocumentsSince()
CREATE INDEX IF NOT EXISTS "<collection>_<version>_lwt"
    ON "<collection>_<version>" (lwt, id);               -- changes-since without the deleted filter
```

Plus, per declared schema index, one table of precomputed index strings rather
than an expression index over JSON paths:

```sql
CREATE TABLE IF NOT EXISTS "<collection>_<version>_idx_<n>" (
    index_string TEXT NOT NULL,     -- dxdb-query's key encoding, byte order == sort order
    id           TEXT NOT NULL,     -- the document
    PRIMARY KEY (index_string, id)
) STRICT, WITHOUT ROWID;
```

Reasons, stated as decisions rather than preferences:

- `STRICT` because the storage layer already knows every column's type. It turns
  a class of silent coercion into an error at write time.
- `WITHOUT ROWID` on the index table because the only access is an ordered range
  scan on `(index_string, id)`, which is exactly the clustered key. A rowid
  table would store the same key twice and then have to follow the rowid.
- A separate index-string table rather than SQLite native indexes on JSON paths
  because the sort order that matters is dxdb's index-string order, which is
  UTF-16-code-unit order with type prefixes and zero-padded numbers
  (PLAN.md §3.2, §4.3; `custom-index.ts`). A native index on `json_extract(data,
  '$.field')` sorts numbers numerically and strings by BINARY collation, and no
  combination of collations makes those agree with our encoding for mixed types.
  Precomputing the encoding we already have to compute for the query planner
  makes the SQL comparison a plain BLOB/TEXT range compare, which is total and
  correct. This is the same conclusion upstream reached for IndexedDB and
  FoundationDB (`getIndexableStringMonad`, `foundationdb-query.ts:55-70`) and did
  not reach for SQLite because the trial version has no indexes.
- The `(deleted, lwt, id)` ordering is chosen so `cleanup(minimumDeletedTime)`
  is a range delete and `getChangedDocumentsSince` is an ascending scan, matching
  the `CLEANUP_INDEX = ['_deleted', '_meta.lwt']` ordering upstream uses for the
  same two operations (`foundationdb-helpers.ts:4`).

### 1.6 What we are explicitly not doing in `dxdb-sqlite`

- **No trial limits.** No 500-document cap, no 500-operation cap, no
  "no indexes" restriction, no premium-flag warning banner. `docs/STATUS.md`
  already records these as intentional divergences; this crate is where the
  divergence becomes real. Upstream's caps live at
  `sqlite-storage-instance.ts:36-40`, `:100-104`, `:186-249`.
- **No browser OPFS wiring.** Lane F owns `dxdb-opfs` and the worker story. This
  crate exposes a driver and a documented constraint.
- **No reading of the premium SQLite storage.** Its on-disk format is unknown to
  us and will stay unknown. A premium database is migrated through export/import,
  per CLEANROOM §3 fact two.

---

## 2. `dxdb-filesystem`

**Design-only artifact.** No upstream source may be read (see §0). Everything in
this section comes from public documentation plus redb's own documented API.

What the public docs establish, and nothing more:

- Documents are persisted as distinct JSON/text files representing the database
  tree, not as one monolithic file (`rx-storage-filesystem-node.md`, FAQ).
- `basePath` is a directory; `inWorker` is a boolean option
  (`rx-storage-filesystem-node.md`, Usage).
- Using the same folder from parallel Node processes is supported *when*
  `multiInstance: true` is set (`rx-storage-filesystem-node.md`, intro).
- It is documented as somewhat faster than SQLite (same page, intro).

That is the whole specification we are working from. Everything below is ours.

### 2.1 Why redb, restated as a constraint

CLEANROOM §5.4: "`redb` earns its place on native (pure Rust, MVCC, crash safe),
with a documented JSON-file export path for portability and for the Expo target
where a real kv engine is unavailable. Multi-process safety comes from the
engine, not from hand-rolled locking."

The last clause is the requirement, and it is what makes the whole design
tractable. `multiInstance: true` means two OS processes may have the same
database folder open and both must (a) see each other's committed writes and
(b) never corrupt the file. Achieving that with plain files requires a lockfile
protocol, an fsync discipline, and a crash-recovery story that we would have to
write and then prove correct. redb provides MVCC with a single-writer,
many-reader model and a crash-safe commit; the OS provides the inter-process
guarantee via file locking. So the design question collapses from "invent a
durable multi-process file format" to "choose keys, and pick a lock and timeout
policy".

What redb does *not* give us: cross-process **change notification**. Two
processes sharing a folder will not learn about each other's writes through the
engine. That is §2.6 and it is a real gap against
`test/unit/cross-instance.test.ts`.

### 2.2 File layout

```
<basePath>/
  dxdb.redb                     # one redb database for the whole folder
  exports/
    <db>.<collection>.<version>.<timestamp>.jsonl   # documented export path
```

One redb file, not one per collection. Reasons:

- A folder is the unit of the `multiInstance` claim. Two collections in one
  folder is one durability domain, and splitting it across files means a write
  that touches two collections cannot be atomic even in principle.
- redb's commit is a file-wide operation anyway; more files means more
  independent commit points and more chances for a torn multi-collection state.
- Backups and the export path become one file to copy, which is the property the
  docs' "store data in plain JSON files like any normal database" claim is
  reaching for.

Counter-consideration, recorded rather than hidden: one file means one write
lock for the whole folder. Two collections with disjoint write traffic serialise
against each other. Acceptable at the scale this storage targets (single-node,
single-digit collections), and revisitable behind a `perCollectionFiles` option
if a benchmark says otherwise.

### 2.3 redb tables

Six tables. All keys are `&[u8]`; redb is byte-string-kv and typed tables are
built by encoding keys ourselves.

| Table | Key | Value | Purpose |
| --- | --- | --- | --- |
| `meta` | `b"schema"` | encoded `{version, collections: {name -> schemaHash}}` | open-time validation |
| `docs` | `collection\0version\0docId` | ordered-JSON document bytes | primary store, the only source of truth |
| `chg` | `i64 lwt BE` ++ `collection\0version\0docId` | `()` | lwt-ordered scan for `getChangedDocumentsSince` |
| `cleanup` | `collection\0version\0` ++ `0x00/0x01` (deleted) ++ `i64 lwt BE` ++ `docId` | `()` | range delete by age |
| `idx:<n>` | `collection\0version\0` ++ index string ++ `0x00` ++ `docId` | `()` | secondary range scans |
| `attach` | `docId\0attachmentId` | attachment bytes | `getAttachmentData` |

Key-encoding rules, and why each is load-bearing:

- **`\0` as the separator, not `|`.** Document ids and index strings may contain
  any UTF-8 including `|`; upstream has to substitute `|` out of IndexedDB keys
  for exactly this reason (`dexie-helper.ts:261-270`,
  `dexieReplaceIfStartsWithPipe`). A byte that cannot appear in a document
  primary key (`maxLength` strings, and our schema validation rejects `\0`) is
  the safe choice.
- **`i64 lwt` big-endian, offset to unsigned before encoding.** redb compares
  lexicographically, so a signed integer must be sign-flipped
  (`(lwt as u64) ^ 0x8000_0000_0000_0000`) or negative timestamps sort after
  positive ones. `lwt` is a JS millisecond timestamp, so in practice it is
  positive; encoding it correctly costs four bytes of thought and removes a
  whole class of "works until 1970" surprise in tests with synthetic clocks.
- **Index strings are used as-is.** They are already ordered by dxdb's encoding
  (PLAN.md §4.3), so a lexicographic range scan on the encoded bytes *is* a
  correct index-ordered range scan. No collation layer.
- **`0x00` terminator before the docId** in `idx:` keys so a shorter index string
  that is a prefix of a longer one cannot bleed into it during a range scan:
  `"abc"` and `"abcd"` both start with `abc`, and the terminator makes
  `abc\0id` sort before `abcd\0id` deterministically.
- **No events table on disk.** Upstream's FoundationDB storage has one
  (`events.` subspace, `rx-storage-instance-foundationdb.ts:404-406`) and never
  writes to it. We do not create a table we do not use.

### 2.4 Iteration order and range semantics

Every read is a redb `Range`, and redb returns keys in lexicographic byte order.
Consequences the storage layer relies on:

1. `docs` prefix scan with `collection\0version\0` gives all documents of one
   collection, id-ordered. `findDocumentsById(ids)` becomes N point lookups, not
   a scan — this is the first place we beat upstream's SQLite
   (`sqlite-storage-instance.ts:352`, which scans the whole table and filters).
2. `chg` ascending scan from `(lastLwt, lastId)` yields `getChangedDocumentsSince`
   in exactly the order the contract wants, with `limit` applied by stopping the
   iterator. Zero allocation per skipped row beyond the key decode.
3. `cleanup` range from `collection\0version\0\x01` (deleted = 1) ++ `0..=cutoff`
   is a contiguous range, so cleanup is a bounded batch delete plus a `cleanup()`
   return value of `false` when more work remains — the contract in PLAN.md §4.2.
4. `idx:<n>` scan from the planner's `startKeys` to `endKeys` (already encoded to
   index strings by `dxdb-query`) yields ids in index order, and each is a point
   lookup into `docs`. Descending sort is *not* supported by redb ranges, which
   matches upstream: a `desc` in the sort forces a re-sort above the storage
   layer (`PLAN.md §4.3`, `query-planner.ts:48`).

The one place a naive prefix scan is wrong: a collection name that is a prefix of
another (`todos` and `todos_archive`). The `\0` separator fixes it — `todos\0` is
not a prefix of `todos_archive\0`. This is the kind of detail that produces a
"works in every test we wrote" storage and then corrupts production, so it gets
an explicit test.

### 2.5 Writes: per-document atomicity, one transaction per `bulkWrite`

The storage contract says `bulk_write` is **per-document atomic, no
cross-document transaction, partial success with a per-row error list**
(PLAN.md §4.2). Two readings are possible and they are not equivalent:

- (a) one redb transaction spanning the whole `bulkWrite`, committing all
  successful rows together;
- (b) one redb transaction per document.

We take **(a)**, with the response's error list still per-row, because:

- the contract's "no cross-document transaction" is about *semantics above the
  storage* — a caller must not assume all-or-nothing across rows — not an
  instruction to make each row a separate commit. Making each row its own commit
  multiplies fsyncs by the batch size.
- conflicts are detected *before* writing (see below), so a batch never has to
  be partially applied because of a mid-batch conflict.
- if the single commit fails, the whole batch is rolled back and every row in it
  is reported as an error. That is a legal outcome under per-document atomicity
  and is strictly safer than a half-applied batch.

The write path:

1. `categorizeBulkWriteRows`-equivalent runs first, against the documents read
   at the start of the transaction, producing insert/update/conflict
   partitions and the error list. This is the one pure function from
   `rx-storage-helper.ts:205` we port verbatim (it belongs to lane D's
   `dxdb-storage`; this crate consumes it).
2. For each insert: write `docs`, `chg`, and one entry per declared index in
   `idx:<n>`, plus attachment entries.
3. For each update: read the previous document, delete every stale `idx:<n>`
   entry whose index string changed, write the new ones, replace `docs`, and
   replace the `chg` entry (delete old lwt key, insert new). Updating `chg` in
   place would be a bug: `lwt` changes, so the key changes.
4. Commit once. Emit one `EventBulk` with `checkpoint = {id: last.id, lwt:
   last.lwt}` after the commit returns, never inside the transaction — a
   subscriber must not be able to read a document that is not yet visible
   outside the transaction (the reason upstream does the same,
   `rx-storage-instance-foundationdb.ts:196-205`).

`unsafe`: none. redb is pure Rust. `#![forbid(unsafe_code)]`.

### 2.6 `multiInstance` safety: what the engine gives us and what it does not

Three separate claims hide inside "multiInstance safe". They need separate
answers, because only the first is free.

**Claim 1 — no corruption, no lost writes, mutual exclusion.** Provided by redb
plus the OS file lock. Two processes, one file, single writer at a time, MVCC for
readers, crash-safe commit. We configure and verify it; we do not implement it.
Concretely: open with redb's default (file-locking) backend, set a write timeout,
and treat `DatabaseAlreadyOpen` as a startup error with a message that says
another process holds the folder, not as a retry.

**Claim 2 — a second process sees the first process's committed writes.**
Provided, but only on a fresh read transaction. redb's MVCC is snapshot-based:
a long-lived read transaction in process B will keep returning B's snapshot
forever. So the design rule is: **read transactions are short and per-operation,
never cached on the instance.** One `bulkWrite` opens one write transaction; one
`query` opens one read transaction and drops it. The moment an instance caches a
`ReadTransaction` for "performance", multi-instance visibility silently breaks.
This is a design rule with a test, not a comment.

**Claim 3 — cross-process change events.** **Not provided. This is the real gap.**
`cross-instance.test.ts` requires that a document written by `db1` produces a
change event on `db2`'s live query when both point at the same storage. Upstream
gets this from `broadcast-channel` inside `addRxStorageMultiInstanceSupport`
(`src/rx-storage-multiinstance.ts:102-188`), which is a JS-only mechanism:
`BroadcastChannel`, `localStorage` events, or a socket. None of those exist for
two independent OS processes on a redb file.

Options, with the honest trade-off:

| Approach | Works across processes | Cost |
| --- | --- | --- |
| Port `addRxStorageMultiInstanceSupport` shape (in-process bus) | no — in-process only | trivial; covers same-process multi-instance only |
| Poll the `chg` table on an interval per open instance | yes | latency = interval; one read tx per poll per instance |
| Add an OS-level notify channel (inotify / kqueue) | yes | OS-specific, one more failure mode, not portable to Windows |
| A sidecar lockfile + `chg` tail | yes | the hand-rolled locking CLEANROOM §5.4 says not to do |
| Declare `multiInstance: false` for the filesystem storage | n/a | contradicts the brief: "`multiInstance` safety is part of the ported conformance suite. Make it pass." |

Decision for the first implementation: **in-process bus for same-process
instances, plus `chg`-table tailing on a configurable interval (default 250 ms)
for cross-process.** Rationale:

- Claims 1 and 2 are what "does not corrupt your data" means, and they are
  fully covered. Claim 3 is a *latency* property, and a 250 ms poll gives a
  correct event stream with bounded latency rather than an absent one.
- Tailing `chg` is cheap and naturally resumable: each instance keeps its
  last-seen `(lwt, id)` cursor, so a poll is a range scan from the cursor with an
  empty result in the steady state. No extra bookkeeping table.
- It composes with the same code path as `getChangedDocumentsSince`, so the
  polling implementation is not throwaway work.
- The interval is a named setting with the default documented, and
  `multiInstance: false` disables the tailer entirely (no cost for the common
  single-process case).

This is recorded in the crate docs as a **known, documented behavioural delta**:
cross-process change *delivery* is bounded-latency rather than immediate. The
conformance suite's assertions are about eventual event delivery with `waitUntil`
semantics, which a 250 ms tail satisfies. If a test turns out to require
sub-millisecond cross-process delivery, that test is testing the bus, not the
storage, and the delta gets raised rather than papered over.

Also ours to decide, and decided:

- **Schema/version coexistence.** The `docs` key includes `version`, so two
  schema versions of one collection coexist in one file. `meta` records the
  schema hashes; opening with a mismatched hash for a version already present is
  an error, not a silent re-use (a silent mismatch is how a storage quietly
  corrupts an index).
- **`remove()`** deletes the collection+version's ranges in all tables in one
  transaction, including the index tables and the attachment entries, then closes.
- **Attachment digests** are stored alongside the blob in `attach` so
  `getAttachmentData(doc, att, digest)` can verify; a digest mismatch is an
  error rather than a returned blob.

### 2.7 The export path (required, not optional)

CLEANROOM §5.4 requires a "documented JSON-file export path for portability and
for the Expo target where a real kv engine is unavailable". So:

- `export(collection, out: impl Write) -> Result<ExportStats>` writes JSONL: one
  document per line, ordered by `(lwt, id)` for reproducibility, documents
  serialised with `dxdb-json` (ordered keys, JS number formatting) so a diff
  between two exports is meaningful.
- `import(reader) -> Result<ImportStats>` accepts the same format and is
  idempotent per document by `(id, revision)`.
- The same format is the RN/Expo target's file format (§2.8), which is why it is
  a first-class path and not a debug utility.
- Test: export → wipe → import → the storage conformance suite passes on the
  result. That is the portability claim made assertable.

### 2.8 wasm: OPFS is lane F's, and where that boundary sits

PLAN.md §5.4 says filesystem storage on wasm is "yes (`preopens`)" for WASI and
"no" for browser/worker. So:

- `dxdb-filesystem` on `wasm32-wasip1`/`wasip2` uses the WASI preopened
  directory as `basePath`. redb needs an `std::fs::File` with `read`/`write`/
  `set_len`/`sync_all`; on WASI those exist, and `sync_all` maps to a host call.
  This is the configuration the M5 conformance gate exercises.
- On `wasm32-unknown-unknown` there is no filesystem at all. The crate must still
  **build** there (PLAN.md §4.1 CI rule) with a stub backend whose `open` returns
  `Unsupported`, so the workspace stays green and a user gets a clear error
  instead of a link failure.
- Browser OPFS is a different storage (`dxdb-opfs`, lane F) with a different
  concurrency model (sync access handles, worker-only). We do not implement a
  redb-on-OPFS adapter. If lane F wants redb there later, they own the decision.

### 2.9 Open question this lane cannot settle alone

`crates/storages/dxdb-memory/` is lane D's and is scaffolded but not implemented.
`dxdb-filesystem` cannot compile its real implementation until `dxdb-storage`
publishes the traits and `dxdb-memory` publishes the reference behaviour it is
checked against. That is why this tranche is design + skeletons; see §8 for the
exact interface we need.

---

## 3. `dxdb-wasi`: the WIT world

### 3.1 What the M5 gate actually has to prove

From PLAN.md §6/M5 and the lane brief: "the conformance suite runs under Wasmtime
from a `.wasm` component with a preopened directory". Two capabilities are being
proven at once:

1. the component model works — our code is exported through `wit-bindgen`, not
   through a snapshot-preview1 C ABI;
2. the storage works against a real, capability-scoped filesystem — the
   directory is passed in, not assumed.

Both were de-risked by probe before writing this section; see §7.

### 3.2 Constraint discovered by probe: no side-module wiring in Wasmtime's CLI

`wasmtime run --help` on Wasmtime 50.0.0 has no side-module option, and
`wasmtime run a.wasm b.wasm` silently ignores the second file. So there is **no
supported way to hand the CLI a component plus a separate core module that
satisfies it**. That rules out "one component, one core-module import per
storage" if the gate is to stay a single `wasmtime run` command.

Consequences for the world design:

- The complete storage implementation lives **inside** the component. The WASI
  target's storage is filesystem-on-preopens (§2.8), which needs no host
  callback beyond WASI itself. No host imports of our own are required for the
  gate.
- Anything that genuinely needs host-provided storage (a host that wants to hand
  us memory-backed storage, for example) goes behind an **optional import
  interface** that the component may declare but must not require. Optionality in
  the component model is per-import, not per-run, so in practice this means two
  build configurations, not one clever one.
- The napi and Node-WASI paths (§4, §5) are where "the host supplies the storage"
  gets exercised. The component-model path is deliberately self-contained: it is
  the *portability* proof, not the *embedding* proof.

### 3.3 The world

```wit
// crates/dxdb-wasi/wit/dxdb.wit
package dxdb:wasi@0.1.0;

interface types {
    /// A document is opaque ordered JSON plus its identity. The component does
    /// not interpret fields; it stores and returns bytes, so a host in any
    /// language can use it without a shared type registry.
    record document {
        id: string,
        rev: string,
        deleted: bool,
        lwt: u64,
        json: list<u8>,
    }

    record write-row {
        doc: document,
        previous: option<document>,
    }

    variant write-error {
        conflict,
        /// Same document state already present (upstream's `same-state-fork`).
        same-state,
        /// The host refused the write for a reason we do not model.
        host(string),
    }

    record write-result {
        written: list<string>,
        /// Parallel to the attempted rows: `none` means the row succeeded.
        errors: list<option<write-error>>,
    }

    record query-request {
        /// Ordered JSON of the prepared query, as produced by dxdb-query.
        prepared: list<u8>,
    }

    record checkpoint {
        id: string,
        lwt: u64,
    }

    enum journal-mode { wal, wal2, delete, truncate, persist, memory, off, untouched }
    enum driver { filesystem, sqlite, memory }
}

interface storage {
    use types.{document, write-row, write-result, query-request, checkpoint, journal-mode, driver};

    /// The preopened directory the component was given. An empty list means
    /// "no preopen", which is an error for every persistent driver: a storage
    /// that silently becomes ephemeral is a data-loss bug, so this is a
    /// startup failure with a message naming the missing capability.
    preopens: func() -> list<string>;

    open: func(db: string, collection: string, schema-version: u32,
               driver: driver, dir: string) -> result<_, string>;
    close: func() -> result<_, string>;

    bulk-write: func(rows: list<write-row>) -> result<write-result, string>;
    find-by-id: func(ids: list<string>, with-deleted: bool) -> result<list<document>, string>;
    query: func(req: query-request) -> result<list<document>, string>;
    count: func(req: query-request) -> result<u64, string>;

    /// `false` means "more tombstones remain, call again" (PLAN.md 4.2).
    cleanup: func(minimum-deleted-ms: u64) -> result<bool, string>;
    changed-since: func(limit: u32, from: option<checkpoint>) -> result<list<document>, string>;

    /// Explicit, not ambient: the gate needs to assert the file exists on the
    /// host side after a write.
    flush: func() -> result<_, string>;
}

/// The CLI surface the conformance runner drives. Kept separate from `storage`
/// so a host that embeds the component does not have to implement file I/O.
interface conformance {
    use types.{driver};

    /// Runs the ported suite against `dir` and returns machine-readable results.
    /// Returns the number of failed cases; a non-zero count is the gate failing.
    run: func(suite: string, dir: string, driver: driver, filter: option<string>)
        -> result<report, string>;

    record report {
        passed: u32,
        failed: u32,
        /// One entry per failure: `case-id: message`.
        failures: list<string>,
    }
}

world dxdb-wasi {
    /// The only capability the gate assumes. `wasi:filesystem` comes with it.
    import wasi:cli/environment@0.2.6;
    import wasi:cli/exit@0.2.6;
    import wasi:filesystem/preopens@0.2.6;

    export storage;
    export conformance;
}
```

Design notes a reviewer should push back on if they disagree:

- **Documents cross the boundary as `list<u8>` ordered JSON, not as component
  records.** A typed record would force every host to implement a WIT-to-JSON
  mapping that has to agree with our field ordering, and field ordering is
  semantically load-bearing for hashing (PLAN.md §3.1). Bytes keep one
  implementation of the encoding, in `dxdb-json`.
- **`preopens()` is exported so a caller can see the capability** rather than
  discovering its absence by getting `ENOENT` three calls later. A component can
  enumerate preopens through WASI; the export makes it a first-class part of the
  API and gives the conformance runner something to assert.
- **`write-result.errors` is parallel to the input rows** rather than a list of
  `(id, error)` pairs. Parallel arrays are harder to misuse than a map whose
  keys are ids that may be duplicated in a batch (they can be: the same document
  written twice in one call is legal input).
- **No change-stream export.** A component export that yields a stream would need
  either the 0.3 async model (nightly, not shipped) or a poll-plus-token protocol
  we would be inventing on the spot. `changed-since` is the same information as a
  resource handle, expressed as a function, and it is what the conformance suite
  actually needs. A real streaming surface belongs to the 0.3 work.
- **`flush()` is explicit.** WASI writes are buffered by the host; the gate wants
  to assert the bytes landed. Hiding that behind `close()` makes the failure
  harder to attribute.

### 3.4 What a preopened directory means for the conformance run

This is the part most likely to be misunderstood, so it is spelled out.

**The guest sees only what was mapped.** Probe result (§7.3), on Wasmtime 50.0.0:

```
$ wasmtime run --dir /tmp/preopen-demo::/data --env PROBE_DIR=/data p2-core.wasm
dxdb wasi probe ok: 42
read_dir(/data) ok, 2 entries                        # the preopen

$ ... --env PROBE_DIR=/  p2-core.wasm               # same preopen still mapped
read_dir(/) failed: No such file or directory        # no root
$ ... --env PROBE_DIR=/tmp p2-core.wasm
read_dir(/tmp) failed: No such file or directory     # host /tmp is NOT visible
$ wasmtime run --env PROBE_DIR=/data p2-core.wasm    # no --dir at all
read_dir(/data) failed: No such file or directory
```

Three operational consequences:

1. **The conformance run's database directory must be inside the preopen, and the
   suite must be told the guest-side path, not the host-side path.** The runner
   creates a host temp dir, maps it to `/data`, and passes `/data/...` into the
   component. Passing the host path produces `ENOENT` from a directory that
   demonstrably exists, which is the single most confusing failure available here.
2. **A missing preopen must be a loud, early failure.** Because `read_dir("/")`
   and `read_dir("/tmp")` fail rather than falling back, any storage code that
   assumes an ambient filesystem fails at random later points. `open()` therefore
   checks `preopens()` first (and §3.3's `storage::preopens()` exists for it), and
   returns a named error when the requested `dir` is not under any preopen.
3. **Test isolation is a preopen decision.** Each suite case gets its own
   `--dir <case-tmp>::/data` so a case cannot inherit another case's file. Because
   the guest cannot see anything else, a leaked path is caught immediately instead
   of silently succeeding against a stale file.

Guest-side path arithmetic is worth pinning down: paths inside the component are
absolute-from-preopen (`/data/x.redb`), never host paths, and never `.`-relative.
The `dir` parameter in the world is the guest-side path. The runner owns the
host↔guest mapping and logs it.

### 3.5 `wasm32-wasip3`

Tracked, never shipped. PLAN.md §3.4 and the brief agree: 0.3 is nightly-only,
needs the `async` feature, and its `wasi:clocks@0.3.0` import shape differs. Lane
action: keep a `cfg(target_feature = "…")`-free source layout (that is, do not
write 0.3-only code paths into the shared module) so the 0.3 build is a
`wit-bindgen` version bump plus an async wrapper, not a fork. No CI job, no
dependency, no shipped artifact.

---

## 4. `dxdb-node`: the napi surface

### 4.1 Shape

CLEANROOM has no upstream for this, so the shape is ours; the *behaviour* it
exposes is `dxdb-core`'s. The rule that keeps it honest: **`dxdb-node` adds no
semantics.** Every function below is a translation of a core API, and where a
translation would be lossy it is documented rather than smoothed over.

```rust
// crate: dxdb-node, built with napi-rs (napi 3.15)

/// One handle per open database, held on the JS side. napi-rs `#[napi]` class.
#[napi]
pub struct Database { /* Arc<DatabaseHandle> + a tokio LocalSet-free block-on */ }

#[napi]
impl Database {
  /// `options` is a typed object generated by napi-rs, not `serde_json::Value`.
  #[napi(factory)]
  pub fn open(options: OpenOptions) -> Result<Self>;

  #[napi]
  pub fn collection(&self, name: String, schema: SchemaInput) -> Result<Collection>;

  #[napi]
  pub fn close(&self) -> Result<()>;
}

#[napi]
impl Collection {
  /// Resolves to `{ written: string[], errors: (null | WriteError)[] }`,
  /// mirroring `write-result` in the WIT world so the two Node paths agree.
  #[napi(ts_return_type = "Promise<WriteResult>")]
  pub fn bulk_write(&self, rows: Vec<WriteRowInput>) -> AsyncTask<BulkWriteTask>;

  #[napi(ts_return_type = "Promise<Document[]>")]
  pub fn find(&self, query: QueryInput) -> AsyncTask<FindTask>;

  /// A JS-visible change stream. Deliberately an async iterator rather than an
  /// Observable: dxdb is not RxJS and shipping a hand-rolled Observable shim into
  /// Node would be a second, subtly different API for the same thing. Lane F
  /// owns the Observable bridge for the JS/browser surface.
  #[napi(ts_return_type = "AsyncGenerator<EventBulk>")]
  pub fn changes(&self) -> Result<ChangesStream>;
}
```

Decisions and their reasons:

- **`AsyncTask` / `block_on` over an executor thread, not `tokio` by policy.**
  PLAN.md §9 makes `tokio` native-only by policy and says the platform binding
  supplies the executor. For Node, the executor is napi-rs's `AsyncTask`, which
  runs on libuv's thread pool. That is the correct answer for a synchronous-ish
  storage on redb or SQLite: the work is blocking file I/O, and libuv's pool is
  where blocking file I/O belongs.
- **`#[napi(object)]` inputs, not `serde_json::Value`.** A JS caller passing
  `{ selector: { age: { $gt: 5 } } }` gets a typed conversion with a real error
  message at the boundary. Accepting an opaque `Value` would push every mistake
  into the query planner, where the error says "unknown operator in index string"
  instead of "`$gt` got a string". Types come from napi-rs's `ToNapiValue`, not
  from `tsify-next` (that crate is for the `wasm-bindgen` surface, PLAN.md §9).
- **`changes()` as an `AsyncGenerator`, not a callback.** Backpressure and
  cancellation are then the JS author's own `for await` semantics, and there is no
  listener-leak lifecycle to get wrong.
- **One `Database` handle fans out to `Collection` handles**, matching the core's
  ownership so a closed `Database` invalidates its collections with a clear error
  rather than a panic across the FFI boundary.
- **Panics must not cross the boundary.** `#[napi]` turns a Rust panic into a JS
  throw, but the database state afterwards is unspecified. The rule for this
  crate: every public method returns `Result`, panics are only reachable from
  bugs, and `panic = "abort"` is *not* used for this crate (it would kill the
  process). This interacts with PLAN.md §5.3's release profile: the profile sets
  `panic = "abort"` workspace-wide, so `dxdb-node` needs a profile override for
  the native cdylib, or `catch_unwind` at the boundary. Prefer the profile
  override; a `catch_unwind` on every entry point is dead weight in the common
  case. Flag this to the leader before the first release build: **it is a change
  to a leader-owned decision surface**, and the resolution (per-crate
  `[profile.release.package.dxdb-node] panic = "unwind"`) must be recorded in
  PLAN.md §5.3 if adopted.

### 4.2 `wasm32-wasip1` as the install-free fallback

`dxdb-node` also builds for `wasm32-wasip1` so Node can load a `.wasm` with
`node:wasi` and no native build step. Constraints:

- `napi` is a native ABI; it cannot be compiled for `wasm32-wasip1`. So the WASI
  build of this crate must be a **different artifact with the same JS-facing
  behaviour**, not the same source. Practically: the storage/query/conformance
  entry points move behind a `cfg`, and the crate exposes a small
  `wasi` submodule that `node:wasi` can instantiate.
- Node's `node:wasi` is **synchronous and preview1**. `wasm32-wasip1` output is a
  core module importing `wasi_snapshot_preview1` (verified in §7.1), which is
  exactly what `node:wasi` accepts. `wasm32-wasip2` output is a *component* and
  `node:wasi` does **not** accept components — so the Node-WASI path is
  wasip1-only. That asymmetry is a fact about Node, not a choice.
- The preopen rule from §3.4 applies unchanged: Node's
  `new WASI({ preopens: { '/data': hostDir } })` is the same capability, and the
  same "guest path, not host path" mistake is available.
- `napi` and `wasm-bindgen` must not appear in the same build graph as each other
  for wasm32-wasip1; the crate's feature split enforces that.

### 4.3 How the two Node paths stay honest

The gate is "the conformance suite under Node from both the napi build and the
WASI build". Two paths that drift are worse than one path, so:

- both paths return the same JSON shapes, defined once in `dxdb-node`'s
  `protocol` module (`WriteResult`, `QueryResult`, `ChangeBulk`), serialised with
  `dxdb-json`;
- the conformance runner is **one JS harness** parameterised by a `StorageFactory`
  that is either the napi object or an instantiated WASI module. It does not
  know which one it is holding, which is the property that makes the comparison
  meaningful;
- a shape difference is a test failure in the harness, not a documentation note.

---

## 5. Cross-cutting: lanes, features, and what stays green

- **`wasm32-unknown-unknown` must keep building for all four crates** (PLAN.md
  §4.1). That means: `dxdb-wasi` compiles to a stub on unknown-unknown (no
  `wasi` imports available), `dxdb-node`'s napi path is `cfg`-excluded,
  `dxdb-filesystem` has an `Unsupported` backend (§2.8), and `dxdb-sqlite` uses
  the in-memory VFS (§1.4). `make wasm` is the check.
- **Dependencies live in the crate, never in `[workspace.dependencies]`.** All
  four crates carry their own `rusqlite`/`redb`/`wit-bindgen`/`napi` lines. The
  leader owns the workspace table (WORKTREES.md).
- **No `unsafe` beyond what the bindings need**, each with a comment naming the
  invariant. Expected total: zero in `dxdb-sqlite` and `dxdb-filesystem` (both
  FFI layers are internal to `rusqlite`/`sqlite-wasm-rs` and redb is pure Rust),
  zero in `dxdb-wasi`, and some in `dxdb-node` only if a napi-rs
  `extern "C"`-adjacent shim forces it — the napi-rs macro handles that itself,
  so the expectation is still zero in our code.
- **Disk.** SQLite plus `wasm-bindgen` plus a napi build is the 2-4 GB per tree
  the brief warns about. `make headroom` before any cold build; `make busy` and
  the `flock` are the standing rules.

## 6. Interfaces this lane needs from lane D before implementation starts

Named precisely, because "we are gated on lane D" is not actionable:

1. `dxdb-storage`: the `RxStorage` / `RxStorageInstance` traits (PLAN.md §4.2)
   with an object-safe method set, so `dxdb-sqlite` and `dxdb-filesystem` can
   implement them for two concrete backends each.
2. `dxdb-storage`: `categorizeBulkWriteRows` as a pure function with upstream's
   inputs/outputs (`rx-storage-helper.ts:205`). Both storages call it and neither
   reimplements it.
3. `dxdb-storage`: the change-event types (`EventBulk`,
   `RxStorageChangeEvent`, checkpoint) — these appear in our public signatures.
4. `dxdb-memory`: enough of the reference implementation to check ordering,
   batching, and `cleanup()` semantics against. The ported conformance suite is
   the real oracle, but we need *a* working storage to run it against before ours
   exists.
5. `dxdb-query`: `getStartIndexStringFromLowerBound` /
   `...UpperBound` / `getIndexableStringMonad` and the prepared-query shape. Our
   index tables are meaningless without the exact encoding.

None of these are needed for this tranche: the skeletons are dependency-free on
purpose, so they compile today and gain dependencies when lane D publishes.

## 7. Probe results (toolchain de-risking, run for this tranche)

All builds remote, via the shared Fly box, `-d 1.95.0`, under
`flock /tmp/dxdb-remote-build.lock`. Host `rustc` and its installed toolchains
were not touched; the box's `rustup default` was `1.95.0` before and after.

### 7.1 Target availability (provisioned by the operator, verified here)

```
$ ssh fly-builder 'source ~/.cargo/env; rustup default; \
    rustup target list --installed --toolchain 1.95.0'
1.95.0-x86_64-unknown-linux-gnu (default)
wasm32-unknown-unknown
wasm32-wasip1
wasm32-wasip2
x86_64-unknown-linux-gnu
```

`wasm32-wasip1` and `wasm32-wasip2` are present on 1.95.0 and the default is
unchanged. No `rustup target add` was run by this lane.

### 7.2 Throwaway probe: `/tmp/wasi-probe` (a bin + lib crate)

```bash
$ cd /tmp/wasi-probe
$ flock /tmp/dxdb-remote-build.lock \
    cargo remote-3000 -r fly -d 1.95.0 build --target wasm32-wasip1
   Compiling wasi-probe v0.0.0 (/data/builds/e3334462d4c5371b)
    Finished `dev` profile [unoptimized] target(s) in 0.37s
   → exit 0

$ flock /tmp/dxdb-remote-build.lock \
    cargo remote-3000 -r fly -d 1.95.0 build --target wasm32-wasip2
   Compiling wasi-probe v0.0.0 (/data/builds/e3334462d4c5371b)
    Finished `dev` profile [unoptimized] target(s) in 0.32s
   → exit 0
```

Artifacts, fetched and validated on the host:

| Artifact | Bytes | `wasm-tools validate` | Notes |
| --- | --- | --- | --- |
| `wasm32-wasip1/debug/wasi-probe.wasm` | 104,969 | pass | imports `wasi_snapshot_preview1` — the shape `node:wasi` accepts |
| `wasm32-wasip2/debug/wasi-probe.wasm` | 136,597 | pass | **is a component**; `wasm-tools component wit` shows `import wasi:filesystem/preopens@0.2.6` |
| `wasm32-wasip2/debug/wasi_probe.wasm` | 14,399 | pass | the cdylib, a `world root {}` component |

The wasip2 result is the important one: on 1.95.0 `wasm32-wasip2` emits a real
component (not a core module + adapter), and WASI 0.2.6 filesystem imports come
for free. That is what §3's world is built on.

### 7.3 `wit-bindgen` 0.63 export path: `/tmp/wit-probe`

The riskiest assumption in the lane was that a `wit-bindgen` component can export
*our* interface on 1.95.0, and that Wasmtime runs it with a preopen.

```bash
$ cd /tmp/wit-probe
$ flock /tmp/dxdb-remote-build.lock \
    cargo remote-3000 -r fly -d 1.95.0 build --target wasm32-wasip2
   Compiling wit-bindgen v0.63.0
   Compiling wit-probe v0.0.0 (/data/builds/406d866a85dc19af)
    Finished `dev` profile [unoptimized] target(s) in 15.68s
   → exit 0

$ wasm-tools component wit probe.wasm | grep -A2 'package dxdb:probe'
package dxdb:probe@0.0.1 {
  interface types {
    add: func(a: s32, b: s32) -> s32;
  }
}
```

The export survives into the component's type section. And the run, with the
preopen semantics §3.4 depends on:

```bash
$ wasmtime --version
wasmtime 50.0.0 (ebe5e8df1 2026-10-20)

$ wasmtime run --dir /tmp/preopen-demo::/data --env PROBE_DIR=/data p2-core.wasm
dxdb wasi probe ok: 42
read_dir(/data) ok, 2 entries                       → exit 0

$ wasmtime run --dir /tmp/preopen-demo::/data --env PROBE_DIR=/tmp p2-core.wasm
read_dir(/tmp) failed: No such file or directory    → host /tmp invisible

$ wasmtime run --env PROBE_DIR=/data p2-core.wasm    # no --dir
read_dir(/data) failed: No such file or directory    → no preopen, no access
```

Also established: `wasmtime run --help` has no side-module option, which is why
§3.2 puts the whole storage inside the component.

### 7.4 Dependency probe: `/tmp/deps-probe`

Confirms PLAN.md §9's claim that `rusqlite` 0.41's *default* features already
swap FFI to `sqlite-wasm-rs` on wasm, and that `redb` 4.4.0 needs no native
toolchain. The probe crate depends on `rusqlite = "0.41"` and `redb = "4.4.0"`
with **no feature selection at all**, and calls
`Connection::open_in_memory()` in real code, so this is a link test, not a
manifest-resolution test.

```bash
$ cd /tmp/deps-probe
$ flock /tmp/dxdb-remote-build.lock \
    cargo remote-3000 -r fly -d 1.95.0 build --target <TARGET>
```

| Target | Result | Wall | Note |
| --- | --- | --- | --- |
| `x86_64-unknown-linux-gnu` (`check`) | **exit 0** | 3.30s | `Checking rusqlite v0.41.0` — host path clean |
| `wasm32-unknown-unknown` | **exit 0** | 9.76s | FFI swapped, no C toolchain needed |
| `wasm32-wasip1` | **exit 0** | 4.43s | `Compiling rusqlite v0.41.0` via `sqlite-wasm-rs` |
| `wasm32-wasip2` | **exit 0** | 4.05s | same, component target |

All four cells green, so §1.3 and §1.4's "one dependency line, no feature
surgery" claim holds on the pinned toolchain. `redb` compiled in every cell too,
which is the "pure Rust, no C" claim in PLAN.md §9 holding up.

What this probe did **not** test, and therefore what remains unproven until the
real implementation: that the default wasm VFS is in-memory (so §1.4's refusal
path is needed), that `wasm32-wasi-vfs` is required for preopen-backed files, and
that `redb`'s `sync_all`/`set_len` path behaves on WASI. Those are runtime
properties and need the conformance run, not a link test.

Host tool versions used for validation, recorded so the numbers are reproducible:
`wasm-tools 1.246.2`, `wasmtime 50.0.0`, `node v22.23.3`.

## 8. Skeletons created in this tranche

Four crates, **dependency-free**, so `make check` and `make wasm` are green now
and each crate gains dependencies when lane D publishes §6's interfaces:

| Crate | Today | Gains |
| --- | --- | --- |
| `crates/dxdb-wasi/` | module tree + `wit/` dir + docs | `wit-bindgen`, optional `dxdb-*` |
| `crates/dxdb-node/` | module tree + docs | `napi`, `napi-derive`, `dxdb-*` |
| `crates/storages/dxdb-filesystem/` | module tree + docs | `redb`, `dxdb-storage`, `dxdb-json` |
| `crates/storages/dxdb-sqlite/` | module tree + docs | `rusqlite`, `dxdb-storage`, `dxdb-json`, `dxdb-query` |

Each crate's `lib.rs` states which rule from §0 governs it, so the licensing
boundary travels with the code instead of living only in this document. That
matters because a future contributor reads `lib.rs`, not lane docs.

### 8.1 Two workspace bugs found by this tranche (both need a leader fix)

Neither is caused by this lane's changes, and both leave a "green" gate that is
green for the wrong reason. They are reported here rather than fixed, because
`Cargo.toml` and the `Makefile` are leader-owned (WORKTREES.md).

**Bug 1 — `exclude = ["crates/storages"]` silently disables the
`crates/storages/*` member glob.** `crates/storages` has no `Cargo.toml` (it is a
grouping directory), so it was added to `exclude` to stop `crates/*` from
matching it. Cargo's `exclude` wins over `members` for *any* path underneath an
excluded directory, so `crates/storages/*` matches nothing. Verified against the
current manifest on master, not just this checkout:

```
$ cargo metadata --no-deps --format-version 1 | jq -r '.workspace_members[]'
.../crates/dxdb-core#0.0.0   .../crates/dxdb-jsnum#0.0.0
.../crates/dxdb-json#0.0.0   .../crates/dxdb-jsstr#0.0.0
.../crates/dxdb-node#0.0.0   .../crates/dxdb-storage#0.0.0
.../crates/dxdb-wasi#0.0.0   .../xtask#0.0.0
                                          # no crates/storages/* member
```

Consequences today: `crates/storages/dxdb-memory/` (lane D's parity oracle) is
**not in the workspace**, so `make check`, `make test`, `make clippy` and
`make wasm` never see it. `cargo check -p dxdb-sqlite` answers "did not match
any packages". The two storages crates in this tranche behave the same way,
which is how the bug surfaced: `make test` reported 15 green test binaries and
none of them were the storages.

Minimal reproduction (no repo needed):

```bash
mkdir -p /tmp/ex/crates/storages/foo/src && cd /tmp/ex
echo '[workspace]\nresolver="2"\nmembers=["crates/*","crates/storages/*"]\nexclude=["crates/storages"]' > Cargo.toml
printf '[package]\nname="foo"\nversion="0.0.0"\nedition="2021"\n' > crates/storages/foo/Cargo.toml
echo 'pub fn f(){}' > crates/storages/foo/src/lib.rs
cargo metadata --no-deps --format-version 1   # members: []   <- foo missing
```

Fix, verified in a full copy of this worktree:

```toml
members = ["crates/dxdb-*", "crates/storages/*", "xtask"]
# no `exclude` needed: `crates/dxdb-*` cannot match the `crates/storages`
# grouping directory, which is the only reason `exclude` was there.
```

`crates/dxdb-*` keeps the "a lane adds its own crate without editing this file"
property the comment in `Cargo.toml` is protecting, and it does not match
`crates/storages`, so the reason for the `exclude` disappears. With that line,
all eleven crates are members and `dxdb-memory`, `dxdb-filesystem`,
`dxdb-sqlite` all appear.

**Bug 2 — `-p` in a `make` recipe is eaten by `cargo-remote-3000` as
`--ssh-port`.** `gate-storage` and `gate-query-correctness` both use
`$(CARGO) test -p dxdb-testkit ...`, which fails before cargo ever runs:

```
$ make gate-storage
error: invalid value 'dxdb-testkit' for '--ssh-port <PORT>': invalid digit found in string
```

`cargo-remote-3000`'s own argument parser claims `-p`, so the package selector
never reaches cargo. `--package` is unambiguous and works:

```
$ cargo remote-3000 -r fly -d 1.95.0 test --package dxdb-testkit --test storage_implementations
```

So the same underlying `test` invocation succeeds with `--package` and fails
with `-p`. With Bug 1 also fixed, `cargo remote-3000 ... test --package dxdb-filesystem --package dxdb-sqlite`
runs the storages crates' tests and they pass, which is the check that proves the
skeletons are real source rather than unreferenced files.

Note for whoever fixes it: `make test` and `make check` are unaffected because
they pass no package selector, which is why the bug survived the M0 skeleton
tranche. Any future `make` target that needs a package filter has to use
`--package`.

### 8.2 Gate status: as-committed, and with both fixes applied

Reported separately because they are different claims.

**As committed (current `Cargo.toml`, current `Makefile`):**

| Command | Exit | What it actually covered |
| --- | --- | --- |
| `make check` | **0** | 8 members; **storages crates excluded** |
| `make wasm` | **0** | same 8 members |
| `make test` | **0** | 15 test binaries; **0 from `crates/storages/`** |
| `make clippy` | **0** | same 8 members |

Green, and honest about why: the two storage skeletons are not compiled by any of
these commands until Bug 1 is fixed. This is the vacuous-pass failure mode the
lane is supposed to catch, so it is stated rather than reported as success.

**With Bug 1 + Bug 2 fixed (verified in a full copy of this worktree):**

| Command | Exit | Coverage |
| --- | --- | --- |
| `check --workspace --all-targets` | **0** | 11 members |
| `test --workspace` | **0** | 21 test binaries (was 15) |
| `test --package dxdb-filesystem --package dxdb-sqlite` | **0** | 3 storage tests + doctests |
| `check --workspace --target wasm32-unknown-unknown` | **0** | 11 members, all wasm-clean |
| `clippy --workspace --all-targets -- --deny warnings` | **0** | 11 members, no warnings |

So the four skeletons are real, compiling, tested source; the current manifest
just does not look at two of them. That distinction is the whole point of this
section.

## 9. What is a port and what is our design: the summary table

The one table a reviewer or a lawyer should read.

| Item | Origin |
| --- | --- |
| Bulk-write categorisation (insert/update/conflict/same-state) | **Port** (Apache-2.0, `rx-storage-helper.ts:205`) — implemented in lane D |
| Query/plan/documents contract, `cleanup` tri-state, checkpoint shape | **Port** (Apache-2.0, PLAN.md §4.2-4.3) |
| Index-string encoding | **Port** (Apache-2.0, `custom-index.ts`) — implemented in lane B |
| Ordered-JSON serialisation | **Port/ours**, `dxdb-json` |
| `bool`→`0/1` binding rule, `?`-count invariant, dual result-row shapes | **Port** (Apache-2.0, `sqlite-helpers.ts:62,230,257`) |
| SQLite table-per-collection-version naming, JSON-blob column idea | **Port** (Apache-2.0, `sqlite-storage-instance.ts:389`) — but see below |
| `SqliteDriver` trait shape, `Param`/`Row`/`Stmt` enums | **Ours** (CLEANROOM §5.3) |
| SQLite DDL: `STRICT`, `WITHOUT ROWID`, index-string tables, `(deleted,lwt,id)` index | **Ours** |
| `BEGIN IMMEDIATE` + `busy_timeout` instead of a `BEGIN` retry loop | **Ours** |
| Refusing a file path on an in-memory VFS | **Ours** |
| Removing the trial caps (500 docs / 500 ops / no indexes / premium banner) | **Ours** — deliberate divergence, `docs/STATUS.md` |
| redb file layout, all six tables, key encodings, `\0` separator, lwt sign-flip | **Ours** |
| `multiInstance` via engine MVCC + short read transactions + `chg` tail for cross-process events | **Ours** |
| JSONL export/import path | **Ours** (required by CLEANROOM §5.4, design is ours) |
| The entire WIT world, `storage`/`conformance` interfaces, `list<u8>` document boundary | **Ours** |
| Preopen validation and guest-path discipline | **Ours** |
| napi surface shape, `AsyncGenerator` change stream, shared `protocol` module | **Ours** |
| `storage-filesystem-node` behaviour | **Public documentation only** — no source read, at any time |

Anything not in this table has not been designed yet, which is the honest state
of a preparation tranche.
