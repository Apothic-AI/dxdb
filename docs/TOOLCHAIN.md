# Toolchain policy

The compiler for this workspace is the one installed on the shared Fly build
server. There is exactly one: **1.95.0**. Every command in the `Makefile` passes
`-d 1.95.0`, and that flag is the pin that counts.

## Why the pin lives in flags, not in a config file

`cargo-remote-3000` reads its toolchain only from `-d/--rustup-default`, whose
default value is the string `stable`, and every remote invocation runs
`rustup default <that>` before building. Two consequences:

1. A build with no `-d` silently installs and switches the box to stable.
2. A `toolchain = "1.95.0"` key in `.cargo-remote-3000.toml` parses without
   error and pins nothing, because the config parser ignores unknown keys. It
   would read like a guarantee while changing nothing, so we do not have one.

`rust-toolchain.toml` exists for a developer running bare `cargo` locally. It is
not read by the remote path.

## Changing the pin requires coordination

`rustup default <toolchain>` mutates shared state: the box's default toolchain
for every project on it. Moving it is a coordination decision, not a
per-project preference.

- Default to `1.95.0`. Our own code must stay 1.95-compatible regardless.
- To move: message the other agents/projects sharing the box, get agreement,
  then record the change here with its date and the change's owner.
- Note that `-d <other>` also *downloads* that toolchain to `/data/rustup`, so
  a one-off experiment adds disk to a shared, persistent volume.

## Change log

| Date | Toolchain | By | Why |
| --- | --- | --- | --- |
| 2026-10-25 | 1.95.0 (initial pin) | plan author | the only toolchain installed on the shared box; all shipped-default WASM features verified against it |

## Local machine note

Local `rustc` on the development machine may differ (it is 1.99.0 at time of
writing). That difference is irrelevant to CI and to what ships: nothing in this
repo compiles locally. If a local `cargo` check and a remote `make check`
disagree, the remote result is the truth.
