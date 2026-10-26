# dxdb

A Rust port of [RxDB](https://github.com/pubkey/rxdb)'s open-source core, with first-class
WebAssembly targets, plus independently specified replacements for RxDB's paid features.

Status: in progress. Nineteen crates exist and all four CI targets build; the parity matrix
in [docs/STATUS.md](docs/STATUS.md) records per upstream module what is ported, in progress,
or not started.

`make audit` checks PLAN.md section 10's definition of done against real artifacts. It
currently reports the checklist **unmet** and names what remains, which is the point: the
checklist exists so "port complete" cannot be claimed by rounding up.

Verification lives in three places, and none of them is a claim in prose:

- **Oracle vectors** (`testdata/vectors/`, 2,176 cases) are expected output produced by
  running upstream RxDB in Node, pinned to the commit in [NOTICE](NOTICE). `make
  vectors-check` fails on drift.
- **Ported upstream tests** name the upstream file and line they came from, so a
  reviewer can check a test against the original.
- **Conformance suites** from upstream's own `rx-storage-implementations` and
  `rx-storage-query-correctness` define the storage contract every storage is measured
  against.

Documents:

- [docs/PLAN.md](docs/PLAN.md): the port (scope, architecture, WASM strategy, milestones, verification, build discipline).
- [docs/CLEANROOM.md](docs/CLEANROOM.md): the clean-room program for the paid and closed-source features.
- [docs/DECISIONS.md](docs/DECISIONS.md): the cross-lane decisions, with the rejected
  alternative and the reason for each.
- [docs/GATES.md](docs/GATES.md): the acceptance oracles, and the exact commands.
- [docs/BENCHMARKS.md](docs/BENCHMARKS.md): measured numbers, including where dxdb is
  expected to lose to the JavaScript implementation.
- [docs/WORKTREES.md](docs/WORKTREES.md): how parallel work is separated.
- [docs/TOOLCHAIN.md](docs/TOOLCHAIN.md): why the compiler pin lives in flags.

## Provenance

`dxdb` is an unaffiliated project. It is not endorsed by or connected to RxDB's maintainer.
Ported code derives from `pubkey/rxdb`, which is Apache-2.0 licensed; the upstream copyright
notice is preserved. Clean-room crates are specified from public documentation only, with no
access to RxDB's premium source.

## Building

Builds run on the shared Fly.io build server via `cargo-remote-3000`, never locally. See
[docs/PLAN.md section 7.4](docs/PLAN.md) before running any `cargo` command in this repo.
