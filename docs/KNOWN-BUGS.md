# Known bugs, reproduced but unresolved

Each entry is a failing test that has been marked `#[ignore]` **with the reason inline**, so it
still runs on demand (`--ignored`) and cannot be mistaken for a fix. An ignored test with no
explanation is a deleted test; these carry their own reproduction.

## KB-1: a `limit` returns zero rows where the unlimited query returns seven

**Reproduces:**
```bash
cargo remote-3000 -r fly -d 1.95.0 test -- -p dxdb-testkit --test operator_coverage -- --ignored
```

**Symptom:**
```
operator: $gt   selector: {"score":{"$gt":25}}   clause: limit
full result (7): ["a3","a4","a5","a6","b3","b4","c1"]
this clause (0): []
```
`{"score":{"$type":"number"}}` also matches nothing, which is the same root cause: both queries
return an empty result rather than a wrong one.

**The plan is not the problem.** Printed from inside the failing test:
```
index=["_meta.lwt","id"]
start=[Number(INDEX_MIN), Number(INDEX_MIN)]   end=[String(INDEX_MAX), String(INDEX_MAX)]
selector_satisfied_by_index=false  sort_satisfied_by_index=false  skip=0  limit=Some(2)
```
That is the correct unrestricted plan: the storage must filter and sort. Without `limit` the same
plan returns 7 rows, so the range scan is fine and something in the limit path is not.

**Ruled out, with evidence:**

| Suspect | Why it is not the cause |
| --- | --- |
| `planned_index` picking the wrong index | Fixed: it returned the first user index whenever the sort was only the primary key (zero non-primary sort fields matched zero). Now requires a non-empty prefix match. |
| `dxdb-memory` limit handling | `.skip(skip).take(limit)` is correct (lib.rs:628-638) and its own 21 tests pass. |
| The `INDEX_MAX` sentinel not surviving a JSON round-trip | `PlanKey::String` → `to_json` → `Value::String` is in-memory, and `BoundValue::from_json` compares against `INDEX_MAX` directly (custom_index.rs:459). No serialisation is involved. |
| Encoder correctness | `dxdb-query` passes all 900 index-string oracle cases, and an upper bound of `String(INDEX_MAX)` on a *number* field correctly emits all-9s (custom_index.rs:585). |
| Digit width for `_meta.lwt` | Width derives from `maximum - minimum` of the injected `_meta` schema (1e15), so all-9s sorts above any real value. |

**Next step for whoever picks this up:** instrument `bound_string` in `dxdb-memory` to print the
encoded `lower` and `upper` strings in hex alongside the first stored index-string key, for the
exact query above, and compare lexicographically. If `lower >= upper`, the collapse is in the bound
computation; if `lower < upper` and the range still yields nothing, the fault is in
`lower_bound`/`upper_bound` on the index itself.

**Why this is worth more than the green test it costs:** both failures are "returns nothing", and
`dxdb-memory` is the reference storage every other storage is compared against. If the reference
implementation drops rows under a limit, the conformance suite is measuring against a broken oracle.
