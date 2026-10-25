# dxdb

A Rust port of [RxDB](https://github.com/pubkey/rxdb)'s open-source core, with first-class
WebAssembly targets, plus independently specified replacements for RxDB's paid features.

Planning documents:

- [docs/PLAN.md](docs/PLAN.md): the port (scope, architecture, WASM strategy, milestones,
  verification, build discipline).
- [docs/CLEANROOM.md](docs/CLEANROOM.md): the clean-room program for the paid and closed-source
  features.

Status: planning. No implementation code yet.

## Provenance

`dxdb` is an unaffiliated project. It is not endorsed by or connected to RxDB's maintainer.
Ported code derives from `pubkey/rxdb`, which is Apache-2.0 licensed; the upstream copyright
notice is preserved. Clean-room crates are specified from public documentation only, with no
access to RxDB's premium source.

## Building

Builds run on the shared Fly.io build server via `cargo-remote-3000`, never locally. See
[docs/PLAN.md section 7.4](docs/PLAN.md) before running any `cargo` command in this repo.
