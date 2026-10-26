# Lane I (m7-cleanroom) — continuation prompt

Paste this back to a fresh session. It is dense on purpose: everything here was verified
against the tree, not recalled.

---

## 0. Orientation in one paragraph

You are lane I of the dxdb port, working in `/home/bitnom/Code/dxdb-worktrees/m7-cleanroom`
on branch `lane/m7-cleanroom`. You own `crates/cleanroom/` and `specs/`. Your job is the
**clean-room** track: reimplement RxDB's paid features from public documentation, without
ever reading `rxdb-premium`. Read `docs/lanes/m7-cleanroom.md` (your brief) and
`docs/CLEANROOM.md` (the governing document) if you need re-orientation, but everything
operational is below.

**The rule you must not break:** never read `rxdb-premium` in any form — not source,
bundles, `.d.ts`, source maps, installer, payloads. You may read the Apache-2.0 upstream at
`~/Code/rxdb`, public docs, and generic prior art. If you are about to inspect a premium
path, stop and say so.

---

## 1. Completed and merged to master

Master was at `a9bb7e6` when this was written. Merged clean-room work:

| Lane merge | Contents |
| --- | --- |
| `e30f64a` | B0 process artifacts: `specs/_TEMPLATE.md`, `specs/README.md` (the port-vs-clean-room crate naming split), `specs/READING-LIST.md`, `CODEOWNERS`, `.github/PULL_REQUEST_TEMPLATE.md`, `docs/CLEANROOM-GAPS.md` |
| `a835c14` | `specs/logger.md` + `crates/cleanroom/dxdb-logger` (see INV-7 note in §4) |
| `6ca4bbc` | `specs/encryption.md` + `crates/cleanroom/dxdb-encryption` (15 invariants, measured KDF) |
| `4abf943` | `specs/sharding.md` + `crates/cleanroom/dxdb-sharding` (INV-1 differential vs `dxdb-memory`), `specs/worker.md` |

Specs written and **not yet implemented**: `specs/meta-cache.md` (B4),
`specs/worker.md` (B3).

### Crate inventory, verified

```
crates/cleanroom/dxdb-logger      9 invariant + 14 acceptance tests        MERGED
crates/cleanroom/dxdb-encryption  15 invariant + 9 acceptance tests         MERGED
crates/cleanroom/dxdb-sharding    10 invariant + 6 acceptance tests         MERGED + 1 commit ahead
```

`dxdb-sharding`'s ten invariants (INV-1 through INV-10, no gaps): **all ten proven by
named test.** Note the numbering has no INV-6-shaped hole: INV-6 is the change stream and
INV-10 is attachments, both added in `ca930b8`. Commit `ca930b8` on your branch
(not yet in master) added `inv_06` and `inv_10` and corrected a false claim in
`specs/sharding.md` — see §4.

---

## 2. In progress right now

**Nothing is uncommitted. The tree is clean and green.** The last unmerged commit is
`ca930b8` on `lane/m7-cleanroom`, which is **pushed** and awaiting the leader's merge.

Gate status on that commit, verified: `make check` 0, `make clippy` 0, `make test` 0,
`make wasm` 0. Workspace total at last run: 876+ tests across the merged crates.

If you are resuming and `git log --oneline master..HEAD` is empty, `ca930b8` has been
merged and you can start §3.

---

## 3. The exact next task

### Task A — `dxdb-worker` implementation (B3)

`specs/worker.md` is written and reviewed-shaped but **not implemented**. Implement it.
Files you will create:

```
crates/cleanroom/dxdb-worker/Cargo.toml
crates/cleanroom/dxdb-worker/src/lib.rs        # the RxStorage wrapper
crates/cleanroom/dxdb-worker/src/protocol.rs   # RequestKind enum, Hello/HelloReply, versioning
crates/cleanroom/dxdb-worker/src/transport.rs  # the WorkerTransport trait + fake
crates/cleanroom/dxdb-worker/src/supervision.rs# liveness state machine, heartbeat, deadlines
crates/cleanroom/dxdb-worker/tests/invariants.rs
crates/cleanroom/dxdb-worker/tests/acceptance.rs
```

Read `specs/worker.md` sections 3.1–3.7 and 6 before writing code; section 6 names all
eleven invariant tests and their exact behaviours.

**The model decision is already made and is not up for re-litigation** (G-048): a worker is
not a thread. Browser `Worker`, `SharedWorker` and Node `worker_threads` are separate
isolates with separate memory communicating by messages, so **nothing here needs atomics**,
`wasm32-wasip1-threads` or nightly. It lands on stable `wasm32-unknown-unknown` and
`make wasm` gates it.

The four-way supervision split in §3.4 is the part with real failure modes and the part a
test can catch. Do not collapse it into one "worker failed" case. The mid-write rule is
G-049: **never retry a write after a worker death**, and use the distinct error kind
`WorkerDiedDuringWrite { context }`.

Test seam: implement a `WorkerTransport` trait (send / receive / observe-termination) and a
**fake transport**, so every supervision case runs in-process on stable with no browser. A
test needing a real browser worker cannot deterministically simulate a mid-write death.

### Task B — `dxdb-meta-cache` implementation (B4), after A

`specs/meta-cache.md` is written. Files:

```
crates/cleanroom/dxdb-meta-cache/Cargo.toml
crates/cleanroom/dxdb-meta-cache/src/lib.rs       # wrapper over RxStorageInstance
crates/cleanroom/dxdb-meta-cache/src/generation.rs# the generation document + instance_id
crates/cleanroom/dxdb-meta-cache/src/snapshot.rs  # localStorage-shaped store trait + fake
crates/cleanroom/dxdb-meta-cache/tests/invariants.rs
crates/cleanroom/dxdb-meta-cache/tests/acceptance.rs
```

Ten invariants in §4 of that spec; INV-1 (stale snapshot always detected) is the load-bearing
one. **Cross-lane dependency**: the generation bump must live in one function inside
`dxdb-core`'s internal-store writer, which is not your crate. See §5.

---

## 4. Things learned that are not in the repo (or are, but you would not find them)

### Build and tooling

- **Never run `cargo fmt --all`.** It reformats every crate in the workspace including other
  lanes'. Use `cargo fmt -p <your-crate>`. This has already cost the leader time cleaning up
  after a lane.
- **Never wrap a `make` target in your own `flock`.** The Makefile serialises through
  `xtask/with-build-lock.sh`, which is reentrant via an inherited `DXDB_BUILD_LOCKED` marker.
  An outer `flock` plus the Makefile's inner one deadlocked the fleet for 75 minutes. Run
  `make check`, `make test`, etc. directly. If you need several gates as one unit:
  `( for t in check clippy test wasm; do make $t; done ) > /tmp/gates.log 2>&1`.
- **`cargo remote-3000` eats `-p`.** It parses its own flags after the cargo subcommand and
  reads `-p` as `--ssh-port`, so `cargo remote-3000 test -p foo` fails before cargo runs. Put
  `--` before cargo flags: `cargo remote-3000 test -- -p foo`. Note `make gate-storage` and
  `make gate-query-correctness` in the Makefile *do* use the `--` form correctly and both
  pass (verified: `gate-storage` 50 tests green).
- **Build box outages are frequent and not your fault.** Symptoms and what they mean:
  - `connection refused` on port 2222 → the ephemeral rootfs restarted and sshd did not come
    up. Fix is the leader's `make remote-repair`. Tell them, do not work around it.
  - `cargo: command not found` → `/root/.cargo` symlink vanished with the rootfs. Same fix.
  - `No route to host` / rsync 255 → the WireGuard route broke. `make` now auto-falls back to
    `flyctl proxy`; just re-run.
  - Never build locally. Never skip a gate.
- A `make` run can die at rsync with 255 mid-sequence and look like a code failure. Read
  whether the error mentions rsync before debugging your code.
- `cargo test` on this workspace takes 2–10 minutes. Use `run_in_background` and attach;
  do not poll in a loop.

### Upstream behaviours learned the hard way

- **`dxdb-memory` only stores an attachment payload if the document has a stub for it.** The
  categoriser derives the digest from the stub and only emits an `attachment_add` for ids
  present in `_attachments`. Supplying a payload with no stub silently stores nothing. Cost
  me one wrong test. Use `DocumentWriteData::insert_attachment(id, AttachmentWriteData {
  stub, data })`, not `with_payloads` plus a hand-built map.
- **Numeric index fields need `minimum`, `maximum` and `multipleOf` in the schema**, or the
  schema filler rejects them with `CI2`. Writing `{"rank": {"type": "number"}}` in a test
  schema fails at fill time.
- **`dxdb_schema::fill_with_default_settings` is mandatory before `StorageSchema::from_value`**
  for any schema with indexes: it injects the meta properties and rewrites every index with
  the `_deleted` prefix and primary-key suffix. `from_value` takes indexes as-is and
  documents that an unfilled schema produces indexes the conformance suite rejects. It also
  appends the primary key to each index, so a bare `[id]` sort needs `"id"` declared.
- **The query planner refuses an index the schema did not declare** (`UnknownIndex`) rather
  than falling back to a scan. A test corpus with several sort shapes needs every one of
  those indexes declared.
- **`StorageSchema::from_value` accepts an index as a string or an array of strings.**
  Directional compound syntax (`[["a","asc"],["b","desc"]]`) is `dxdb-schema`'s job, not the
  storage layer's.
- **`dxdb-storage` does not re-export `compare_js`**; import it from `dxdb-jsstr` directly if
  you need code-unit string order. `dxdb-query::get_sort_comparator` builds the document
  comparator you should reuse rather than reimplement — that is what keeps a merged order
  identical to an unmerged one.

### Clean-room method lessons

- **A spec claim about another crate's capability is a claim you must verify.** I wrote in
  `specs/sharding.md` that INV-6/INV-10 were unprovable because `dxdb-memory` was "not rich
  enough". It was false; `dxdb-memory` emits change events on write and stores attachments
  with digest verification. I had not read the file. Both invariants now have real tests and
  the spec carries the correction explicitly. **Do not write a caveat about code you have not
  read.** Either read it or write "unverified" and say why.
- **The differential test is the highest-value artifact in this lane.** For sharding, INV-1
  comparing against `dxdb-memory` found 240 of 1100 configurations failing, all on `count`,
  and all at `shards=1` too — which pointed away from the merge and straight at a wrong count
  rule (`min(limit, matched)` instead of `min(limit, max(0, matched - skip))`). The spec's
  prose had predicted the *class* of bug but not the instance. Write the differential test
  even where the code path looks trivial.
- **Instrument rather than argue.** When INV-7 for the logger failed on master, the question
  was whether the wrapper raced or the invariant over-specified. I printed the observed record
  order under `join` (`query:start, query:end, count:start, count:end`) and the answer was
  immediate: `join` completes the first future before polling the second, so the invariant was
  over-specified. The fix went into the spec, not the test.
- **Commit before you report.** An entire INV-7 fix was lost once because I reported a stage
  green while the fix was uncommitted in my working tree. Commit, then gate the committed tree,
  then report.

### Encryption specifics worth not rediscovering

- Argon2id `m=64 MiB, t=3, p=1` measured **289 ms release / 2.93 s debug** on the build box.
  The debug number means **never ship Argon2 in a debug build**. The test asserts a
  profile-dependent budget and prints samples.
- The key hierarchy is password → Argon2id → master → generation KEK → data key, with each
  generation's data key wrapped under the previous generation's KEK. Retirement only means
  something because of that chain.
- `dxdb-storage`'s `StorageError` and `dxdb-encryption`'s `StorageError` were distinct types
  during implementation; encryption maps its errors into `StorageError::Unsupported` at the
  boundary.
- The change stream carries whole documents, so **any wrapper that decrypts must decrypt the
  change feed too**. That is the read path that does not look like one and it is the easiest
  place to leak ciphertext.

---

## 5. Blocked on / questions for the leader

**Nothing blocking on the worker task.** One question and one cross-lane dependency:

1. **Question (needed before Task B, meta-cache):** `specs/meta-cache.md` §3.4 requires that
   every internal-store mutation bumps a generation counter, and that the bump lives in **one**
   helper inside `dxdb-core`'s internal-store writer. That is lane D's crate, not mine. Does
   lane D expose a single internal-store mutation point I can hook, or does this need a leader
   decision and a change in another lane's crate? Until it is answered, `dxdb-meta-cache`
   cannot guarantee INV-6 without editing code I do not own.

2. **Cross-lane obligation I have recorded and not discharged:** `docs/CLEANROOM-GAPS.md`
   G-047 states that `dxdb-encryption` and `dxdb-sharding` are tested against real
   `dxdb-memory` now (sharding) and a local double (encryption). When the port's storage
   conformance suite is the acceptance oracle for every clean-room storage
   (CLEANROOM.md §6), both crates should be run through `make gate-storage`. Sharding is
   already covered by its differential test; **encryption is not** and still uses
   `tests/fake.rs`. Do not claim the conformance cross-check for encryption until it has been
   re-run against `dxdb-memory` through `gate-storage`.

3. **Not a blocker, just status:** `specs/worker.md` §9 lists three open questions it
   deliberately leaves to implementation — which JS adapter lands first (Node is CI-testable,
   browser needs Playwright), whether the heartbeat values (5 s / 15 s / 30 s) are right, and
   whether `SharedWorker` mode needs anything on the Rust side at all or is purely a JS-side
   concept. Answer them in the spec as you implement, not before.

---

## 6. First five commands for a fresh session

```bash
cd /home/bitnom/Code/dxdb-worktrees/m7-cleanroom
git fetch origin && git log --oneline master..HEAD      # is ca930b8 still unmerged?
git merge master --no-edit                              # always start from current master
make busy && make headroom                              # box state; under ~10% free, stop
make check                                              # confirm the tree is green before editing
```

Then read `specs/worker.md` §3 and §6 and start Task A.
