# Verification queue

Work that is committed on a lane branch and awaiting a build. The shared build box was paused for
another project's resize, so nothing here has compiler evidence yet.

Order matters: merge bottom-up (fewest dependents first) and run the workspace gates after each, so a
failure is attributable to one lane.

| Lane | Tip | What needs verifying |
| --- | --- | --- |
| `lane/m7-cleanroom` | `45b4d21` | dxdb-worker's RxStorageInstance wrapper, codec, and transport seam (5,440 lines with tests) |
| `lane/m1-schema-query` | `a596ee0` | sort-ties test + corpus; the lane's own brief calls it unverified |
| `lane/m2-testkit` | `a07868b` | every operator selectable: the operator x clause coverage extension |
| `lane/m2-core` | `88f55df` | two retired risks in the uncovered-attachment area |
| `lane/m5-wasi-node` | `59b1e43` | full-engine memory link in the size measurement |
| `lane/m6-plugins` | `c56a2bb` | continuation brief + the plugin work behind it |
| `lane/m4-browser` | `b01c4ed` | continuation brief; in-browser suite run still outstanding |

## The check, once the box is clear

```
make remote-repair      # only if the restart dropped sshd on 2222 or the ~/.cargo symlinks
make busy               # must not print an error
make headroom           # confirm free space before a cold dependency graph
make check              # then merge a lane, then re-run
make test
make clippy
make wasm
make audit
```

A lane whose merge turns master red is that lane's problem, not the next merge's — which is why the
gates run after every merge rather than once at the end.
