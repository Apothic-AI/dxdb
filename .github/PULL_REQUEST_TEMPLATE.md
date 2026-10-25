# Pull request

## What this changes

<!-- One paragraph. Name the crate and the spec file if there is one. -->

## Gate

- [ ] `make check` exits 0
- [ ] `make clippy` exits 0
- [ ] `make test` exits 0
- [ ] `make wasm` exits 0 (`wasm32-unknown-unknown`, required for every crate)
- [ ] the acceptance tests for the spec this PR implements pass, and each one
      names its number (`acceptance_NN_...`)

Paste the commands and exit statuses. "It works locally" is not a gate: nothing
in this repo compiles on a laptop (`docs/TOOLCHAIN.md`).

```
$ make check            # exit
$ make clippy           # exit
$ make test             # exit
$ make wasm             # exit
```

## Provenance — required if this touches `crates/cleanroom/` or `specs/`

The clean-room claim is a process claim (docs/CLEANROOM.md section 2). A diff
that does not answer these is not reviewable, and will be sent back rather than
reviewed.

**1. Did you read `rxdb-premium` in any form to write this?**

That includes its source, its minified bundles, its published `.d.ts`, its
source maps, its encrypted payloads, its installer, its npm tarball, or any
transcription or quotation of any of those.

- [ ] No. I read only the spec, dxdb's own code, public documentation, the
      Apache-2.0 source at `~/Code/rxdb`, and generic prior art.
- [ ] Yes — **stop, do not merge.** Explain below, hand the crate to an
      uncontaminated implementer, and record the incident in
      `specs/READING-LIST.md`.

**2. If this PR implements a clean-room crate, list the sources you actually read.**

<!--
Example:
  specs/logger.md (all of it)
  specs/READING-LIST.md (the dxdb-logger row)
  crates/dxdb-storage/src/lib.rs
  tracing 0.1 span API docs
-->

**3. If you are the specification author for this feature, confirm the split.**

- [ ] I wrote `specs/<feature>.md` and did not write implementation code for it
      in this PR.
- [ ] I am the implementer and did not author this spec.
- [ ] One person did both, and the leader has recorded that fact in
      `docs/CLEANROOM-GAPS.md` with the reason.
- [ ] Not applicable (Apache-2.0 port, not clean-room work).

**4. Does this PR add any of the following?** Each one is a defect, not a
discussion:

- [ ] a constant, magic number, layout, KDF parameter or string format that is
      not derivable from public docs or the Apache-2.0 source
      (`docs/CLEANROOM-GAPS.md` G-001, CLEANROOM.md section 2)
- [ ] a reference to `PREMIUM_FLAG_HASH`, `RXDB_UTILS_GLOBAL`, `hasPremiumFlag`,
      or `NON_PREMIUM_COLLECTION_LIMIT` (G-006)
- [ ] an artificial limit, trial cap, or tier check (G-006)
- [ ] a network call, telemetry ping, or phone-home of any kind (G-006)
- [ ] a claim that this crate is faster than something, without a number from
      `docs/BENCHMARKS.md` (G-003)
- [ ] an RxDB premium product name, tier word, or crown emoji in code, docs, or
      a commit subject (CLEANROOM.md section 8)

If any box above is checked, the PR is not ready.

## Notes

- Commit subjects for clean-room work start with `[cleanroom]` and name the spec
  file: `[cleanroom] specs/logger.md: ...`. Reviewers audit the boundary with
  `git log --grep cleanroom`.
- Never merge `lane/*` into `master` from a lane; the leader merges.
- Do not add `Co-Authored-By` or any attribution trailer.
