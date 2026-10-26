# Build pause: unverified merges

Another project's orchestrator requested a maintenance window on the shared build box
(`open-surrealdb-ds-builder`, machine `87477e0cd01748`) to resize it from 8 CPU to 4 CPU. A
resize restarts the machine and aborts in-flight compiles, so all nine lanes were stopped and
no remote build was allowed.

Merges made during the pause are listed here because **they carry no compiler evidence**. A
merge that has not been built is not a merge that is known to be green, whatever the lane
reported before the pause.

## Unverified at time of pause

| Merge | What it contains | What it needs |
| --- | --- | --- |
| `34ed3ac` lane/m5-wasi-node | fix for two defects in the `wasm-size-budgets.sh` results artifact; lane G continuation brief | the script change is unexercised until someone next runs the budget measurement |
| `4330b30` lane/m2-testkit | `operator_coverage.rs` (437 lines): enumerates Mango operators from upstream's type definition and asserts per-operator coverage with sort, limit, and skip | `make test -p dxdb-testkit` |

## The rule this records

When builds are unavailable, documentation and queue-draining merges can continue. Anything
whose correctness depends on a compiler cannot be called verified, and the honest move is to
record it as outstanding rather than to let a merge imply it passed.

On RESUME, the first action is `make check` + `make test` + `make clippy` on master. If those
are green, delete this file's table. If not, the failure belongs to whichever row it touches.
