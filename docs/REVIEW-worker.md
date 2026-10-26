# Code review: dxdb-worker (lane/m7-cleanroom, tip `45b4d21`)

Reviewed during the build pause, so this is a reading review: **no compiler evidence**. It records
what the code claims and whether the claims are structured to be checkable. It does not substitute for
the gates.

## Verdict: merge and verify; the design choices are sound and testable

3,993 lines across five modules, no `todo!()` or `unimplemented!()`.

## What is right

**The codec is hand-written over `serde_json::Value`, not a derive.** The stated reason is D1: the
types belong to `dxdb-storage`, and putting a wire format on them with a derive would put a
serialisation contract on the trait layer that D1 deliberately keeps free of one. That is the correct
inference from the decision rather than a stylistic preference, and it means the wire format can change
without touching the trait.

**`inv_01` is a differential test.** One instance directly on `dxdb-memory`, one through the wrapper on
the same collection, every operation compared with `==`: documents and key order, result order, `count`,
the `WriteError` variants *with contents*, the checkpoint, and the error variant for a refusal. That is
the right shape for a transparency invariant — comparing against a real storage rather than against
expectations written next to the code.

**`inv_08` is the important one.** A write that was in flight when the worker died must fail, and must
not be re-sent. The test asserts `writes_received() == 1` after a second attempt and the assertion
message says why: "applying a row twice is worse than reporting an unknown outcome." That is the correct
priority for a storage write, and it is the kind of thing that gets quietly optimised into a retry by
someone adding resilience later. The comment is the defence against that.

**`inv_11` bounds the queues under sustained load**, which is what distinguishes backpressure from a
memory leak dressed up as buffering.

Three defects were found by the wrapper's own tests and recorded: a `RefMut` held across a re-borrow in
`pump`'s deadline sweep (`inv_06`/`inv_07` caught it at exactly those two branches), and a `Stream`
returning `Pending` without registering a waker. A test suite that finds three real bugs in the code
that was written alongside it is doing its job.

## What needs the gates to confirm

- `supervision::drive` is described as "nested-safe" because `block_on` refuses to nest and the host
  must run a storage future from inside a poll. That is a re-entrancy claim that only execution can
  confirm.
- `inv_05` (dead worker fails every call) and `inv_10` (reconnect reports a new instance token) both
  cross a lifecycle boundary; timing-sensitive invariants are the ones most likely to be green in a
  fast test and flaky under load.
- The change stream registers its waker via D2's wake channel rather than poll-and-check. Correct in
  principle; whether every path wakes is what `inv_09` (events delivered once) and `inv_11` (bounded
  queues) will show.

## On merge

Run the workspace gates immediately after, because this branch touches `crates/cleanroom/` only but a
clean-room crate still depends on `dxdb-storage` and `dxdb-json`. If the merge is red, the failure is
attributable to this branch alone — which is why the verification queue orders it first.
