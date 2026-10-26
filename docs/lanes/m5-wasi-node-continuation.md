# Lane G (m5-wasi-node) continuation

Written at context reset. `git status` is clean; everything below is committed on
`lane/m5-wasi-node`. **Branch is 2 commits ahead of master** — see §1.

Read `docs/DECISIONS.md` and `docs/STATUS.md` for project state. This file carries only
what is not in the repo, plus the unresolved bits.

---

## 1. Merged, and not yet merged

Merged to master (leader's merge commits, newest first): `2c53932` (M5 gate, real 48-case
suite on five targets), `58d4ec2` (benchmarks + size budgets), `9908787` (filesystem
48/48 + foundationdb accounted), `79c1854` (storage benchmarks), `53ad543` (first M5 gate
run), `6f4c632` (SqliteDriver + redb layout).

**Ahead of master, unmerged — both need merging, neither is broken:**

| Commit | What | Gate state |
| --- | --- | --- |
| `b0cfcf7` | `wasm-size-budgets`: two defects in `results.json` (see §4.7) | no build needed; verified by parsing |
| `a5986a8` | Engine-linked wasm bundles + panic-catching audit (see §3) | `make check/clippy/test/wasm` all 0, 1588 tests, M5 gate 0 |

`a5986a8` touched 11 files, including three in other lanes' crates — all comment-only or
the testkit's own `suite.rs` (see §4.6 for why that one is legitimate).

Not merged and not mine to merge: the Makefile `gate-m5` target (§5.1).

---

## 2. In progress right now

**Nothing.** Working tree clean. The last action was committing `b0cfcf7`, which needed no
compile.

Historical gate state from `a5986a8` (the newest thing I actually built and ran):

| Gate | Exit | Notes |
| --- | --- | --- |
| `make check` | 0 | |
| `make clippy` | **0** | workspace-wide; the `dxdb-opfs` lints I reported earlier are fixed upstream |
| `make test` | 0 | 1588 tests |
| `make wasm` | 0 | |
| `xtask/m5-conformance.sh` | 0 | 48/48 on all five targets |

The build box was being resized (8→4 CPU) when this was written, so **re-run the four
`make` gates before trusting the table** — nothing changed since, but the tree has not
been rebuilt on the resized box.

---

## 3. Exact next task

**Merge `lane/m5-wasi-node` to master.** Both commits are green and self-contained; there
is no unfinished work in the tree. If a fresh session picks this up, that is the whole
task for this lane.

If the leader instead wants more lane work, the two candidates in priority order:

**3a. Measure the OPFS *throughput* row.** It is the last "not measurable on the build
box" cell. Requires the `demo/` browser harness (lane F's territory):
- `docs/BENCHMARKS.md` → "Why indexeddb and opfs are 'not measurable on the build box'".
- The harness must emit the same JSON shape as `crates/dxdb-bench/src/scenario.rs`
  (`BenchReport`), or the row cannot be compared to the others.
- Note `dxdb-opfs` runs against a `FileStore` trait (`crates/storages/dxdb-opfs/src/lib.rs`),
  and the only native backing is in-memory — benchmarking that measures a hash map, not
  OPFS. That is why the row is empty rather than filled with a proxy number.

**3b. `dxdb-sqlite` has no `RxStorage` impl.** It is a driver layer
(`SqliteDriver` + `rusqlite`), so the conformance matrix reports it as
"does not implement RxStorage" and it has no row. `dxdb-filesystem` got its `RxStorage`
impl in `26b8504` (`crates/storages/dxdb-filesystem/src/rx_storage.rs`) and that is the
pattern to copy — it went 0/48 → 48/48 in one commit. Worth doing before the SQLite
storage is claimed anywhere. `docs/STATUS.md` currently says "driver layer only" and is
accurate; do not change that row until the impl exists.

---

## 4. Traps, upstream behaviour, and decisions not written down elsewhere

### 4.1 The wasm size fixtures are only honest because of one function

`crates/dxdb-bench/src/lib.rs` → `dxdb_size_fixture_probe`. A `cdylib` whose storage or
engine code is never *called* gets dead-code-eliminated, so the artifact measures the
binding floor while the row claims to include a storage. During this tranche that trap
bit twice: first two bundles came out byte-identical (759,233 B each) because the probe
was gated on the wrong feature. **If a size number looks implausibly small, check the
probe cfg first.** The gates currently use `cfg(any(feature = "x", feature = "engine-x"))`
patterns; adding a bundle means adding its gate.

### 4.2 `regress` dominates the engine cost, and it is deliberate

Attribution (one dependency at a time, real code path each):

| Fixture | Brotli q11 | Marginal |
| --- | ---: | ---: |
| binding floor (`dxdb-js` + `dxdb-storage`) | 6,735 B | — |
| + `regress` alone | 86,511 B | **+79,776 B** |
| + `dxdb-schema` alone | 37,864 B | +31,129 B |
| + core + reactivity over schema+query | — | **+1,378 B** |

`regress` is ~80 KiB, **62% of the engine+memory bundle's cost over the floor**. PLAN.md 9
chose it for ECMAScript `$regex` semantics, so it stays; but if a budget is ever blown,
that is the first thing to inspect and lazy-loading the `$regex` path is the first remedy.
`dxdb-core` + `dxdb-reactivity` being 1.4 KiB means "the big three engine crates" was the
wrong mental model — the weight is in one third-party regex crate and `jsonschema`.

### 4.3 `panic = "abort"` is release-only, and no harness runs release

`catch_unwind` cannot catch under `panic="abort"`. **Proven, not assumed**: the same
program prints `CAUGHT`/exit 0 under dev and terminates (exit 255) under `--release`.
Six `catch_unwind` sites exist; `cargo test` inherits the dev profile (unwind), so in-repo
callers always get catching behaviour. That is why CI never lied.

The real defect was `dxdb_testkit::suite::run_all` — **shipped library code** whose doc
comment promised "a panic in one case is caught so it does not hide the rest", which
silently becomes "abort on first" under abort. It had **zero callers and zero tests**, so
the damaged contract would have been found by its first user. `a5986a8` fixes it by
exposing `dxdb_testkit::suite::PANIC_STRATEGY` and `catch_unwind_works()`, documenting the
dependency, and adding two tests that assert the constant matches the compiler and that
`cargo test` really gets unwind. Five harness sites carry a pointer note.

**If anyone adds `[profile.test]` or runs tests with `--release`, five harnesses change
meaning at once.** The test in `suite.rs` (`cargo_test_gets_the_catching_behaviour`) is
what catches that.

### 4.4 The M5 `.wasm` runner deliberately does not catch panics

`crates/dxdb-wasi/src/conformance.rs`. Reason: `catch_unwind` cannot catch in the release
artifact, so an apparent catch would silently stop working. Instead each case prints
`RUN <id>` and flushes *before* running, so an abort names the failing case, and
`--filter <id>` reruns it alone. `xtask/m5-conformance.sh` reads the last `RUN` line to
report "last case before failure". Do not "fix" this by adding `catch_unwind`.

### 4.5 Host-path-vs-guest-path is now structurally impossible, not documented

`assert_preopen_usable` in `crates/dxdb-wasi/src/conformance.rs` creates, reads back and
deletes a sentinel file before any case runs, refusing if the dir is absent, not a dir,
read-only, or non-persistent. A caller passing the *host* path (the classic WASI trap —
`ENOENT` from a directory that demonstrably exists) gets a message naming the `--dir`
mapping. `xtask/m5-node-wasi.mjs` takes `<guest-dir>` and `<host-dir>` as **separate
arguments** for the same reason.

### 4.6 Why editing `dxdb-testkit/src/suite.rs` was in scope

Lane D owns that crate. The change is a doc comment plus two public constants and a test
module, made because the defect (a strategy-dependent promise in shipped code) was found
by this lane's audit and leaving it would keep the trap live. No behavioural change to any
case. Flagging it because it crosses a lane boundary by the letter of `docs/WORKTREES.md`
even though it respects the intent.

### 4.7 `results.json` had two bugs that console output hid

`xtask/wasm-size-budgets.sh`:
- Attribution fixtures (zero budget, exist to split a cost) emitted
  `"status": "EXCEEDED"` — two permanent false failures in a CI diff. Now `"attribution"`.
- The file had **invalid JSON** (trailing comma) because rows were appended with a comma
  each. Pre-existing since the file was introduced; only surfaced when something finally
  parsed it. Rows are now collected and joined.

Lesson worth carrying: the console summary looked perfect in both cases. Machine-readable
artifacts need to be parsed, not eyeballed.

### 4.8 `redb` concurrency mode is target-dependent, and WASI loses multi-process

`FilesystemStorage::concurrency_mode()` in `crates/storages/dxdb-filesystem/src/lib.rs`.
`MultiWriter` needs **byte-range** file locks; redb implements `RangeLock` only for
linux/apple/windows, so on WASI the whole-file-lock fallback runs only for
`ExclusiveWriter`. Under `MultiWriter` on wasip1 every open fails with "The storage backend
does not support this operation". Consequence: `multiInstance` across two WASI instances on
one directory is unsupported, and the second open fails loudly with `DatabaseAlreadyOpen`.
`Capabilities::multi_process` reports it per target.

### 4.9 `rusqlite`/sqlite wasm feature truth

- `rusqlite` has **no** `sqlite-wasm-vfs` feature. The SAH pool is a separate crate,
  `sqlite-wasm-vfs` 0.3 (`sahpool`), wasm32-unknown-unknown only.
- `wasm32-wasi-vfs` **is** a real `rusqlite` feature and is required for a persistent
  database on wasip1/wasip2; without it the default VFS is in-memory and a file path
  silently becomes ephemeral. `dxdb_sqlite::Vfs::selected()` reports which one is in force
  and `DriverError::WouldBeEphemeral` refuses a path under a memory VFS.
- SQLite's `bundled` feature must be **target-scoped**
  (`[target.'cfg(not(target_family = "wasm"))'.dependencies.libsqlite3-sys]`), because
  cargo unifies features globally: putting `bundled` on `rusqlite` also applies it to
  wasip1/2, where it tries to compile `sqlite3.c` with clang and dies on the missing WASI
  SDK. The box has `libsqlite3.so.0` but not `libsqlite3-dev`, so native needs `bundled`.

### 4.10 Benchmark comparability rules

`crates/dxdb-bench/src/scenario.rs` is the single definition of the operation mix; both
storage benches and the memory bench consume it. `document_bytes` is 290 B (upstream's
`averageSchemaData()` shape) — at 30 B every implementation looks alike and the comparison
says nothing.

Two traps baked into the generator:
- **The id must come from the document.** An early version keyed rows `b0-d0000` while
  `find_by_id` looked up the `id` field; the mismatch surfaced as a failed assertion
  (0 found, 3000 expected), not a wrong number.
- **Bulk inserts must set `var1 = idx % 2`, `var2 = idx % parallelQueryAmount`** (upstream
  `performance.ts:235`). With random `var2` the selector queries match nothing.

The **memory row is not the same measurement** as filesystem/sqlite: memory drives the real
`RxStorageInstance` trait (categoriser, `Matcher`, event emission), the other two drive a
driver API. Documented in BENCHMARKS.md before any reader can misread it.

### 4.11 The `sqlite` and `filesystem` bench numbers are not durability-comparable

redb in `MultiWriter` **refuses** `Durability::None`, so every filesystem commit fsyncs;
SQLite runs WAL + `synchronous=NORMAL` and does not. Measured: SQLite `insert_serial`
2.91 ms at NORMAL, **45.99 ms at `synchronous=FULL`**, vs redb 25.31 ms. So the headline
"sqlite 8.7× faster" is mostly a durability-policy difference; fsync-per-commit SQLite is
1.8× *slower* than redb. `bench-sqlite --synchronous FULL` reproduces it. Anyone quoting
the raw ratio without this is quoting the wrong thing.

### 4.12 Environment facts

- Builds go through `cargo remote-3000 -d 1.95.0` only. The Makefile computes a route via
  `xtask/remote-route.sh` and falls back to `fly-builder-proxy` when the WireGuard 6PN path
  is down. **Do not wrap make targets in your own `flock`** — the reentrant
  `xtask/with-build-lock.sh` handles it and a wrapping flock deadlocks the fleet.
- `make busy` / `make headroom` reach the box over the *direct* host and fail when only the
  proxy route is up. Check idle state with `ssh fly-builder-proxy 'ps -eo comm= | grep -qxE
  "rustc|cargo" && echo BUSY || echo IDLE'` instead.
- mise's `wasmtime` shim is on PATH but exits nonzero ("installed but not activated").
  `xtask/m5-conformance.sh` executes every candidate before accepting it. The real binary
  is under `~/.local/share/mise/installs/wasmtime/50.0.0/wasmtime-v50.0.0-x86_64-linux/`.
- **Never `cargo fmt --all`** — it reformats other lanes' crates. Use `cargo fmt -p <crate>`.
- Benchmark and size runs must be marked idle honestly. `xtask/bench-storages.sh` and
  `wasm-size-budgets.sh` record `idle_at_start` and the load average. Never hardcode the
  idle flag: an early memory run did, and the number was discarded and re-taken through
  the script.
- Do not benchmark locally. The first bench script ran `cargo run` locally while recording
  the Fly box's metadata — it described an i5-13420H laptop while claiming an AMD EPYC.
  Both scripts rsync, build and run on the box, and fetch only the artifact back.

---

## 5. Blocked / questions for the leader

### 5.1 The `gate-m5` Makefile target — outstanding request

`xtask/m5-conformance.sh` is merged and works, but **there is no make target for it**, so
the M5 gate is still a remembered incantation. I do not edit the Makefile. Wanted:

```make
gate-m5:
	@xtask/m5-conformance.sh
```

plus `gate-m5` added to the `.PHONY` line (currently line 54) and to the `help` echo
(line 57). **Do not wrap it in `$(LOCKED)`** — it invokes `cargo remote-3000` itself,
which takes the build lock per invocation, and the suite run afterwards is local.

### 5.2 The engine-linked budget numbers are unmerged

`a5986a8` holds them: memory+engine **131,775 B** vs 320 KiB (59.8% unused),
sqlite+engine **402,134 B** vs 900 KiB (55.1% unused), full engine+core+reactivity
**133,153 B** (1,378 B over schema+query). **Both PLAN.md 5.3 budgets hold with the engine
linked**, which is the answer to "are the targets real". `docs/BENCHMARKS.md` in that commit
retires the old "the bundles do not contain the engine" caveat with an explicit
"no longer true" marker rather than deleting it.

If the leader would rather the caveat paragraph be deleted outright than
retained-and-marked, that is a judgement call I left in.

### 5.3 Nothing else blocks this lane

M5 is met with the documented panic caveat. The only genuinely unmeasurable row is OPFS
throughput (needs the browser harness, §3a). No questions pending.
