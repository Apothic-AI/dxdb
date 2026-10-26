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

## Recovering the box after a machine restart

The build machine has been reset twice during this project, and both times the
failure looked like a broken build rather than a broken box. Read this before
diagnosing anything.

### What survives a restart and what does not

`/data` is a persistent volume. Everything else is an ephemeral root filesystem
that is rebuilt from the image.

| Path | Survives | Consequence if missing |
| --- | --- | --- |
| `/data/cargo`, `/data/rustup` | yes | the toolchain and all wasm targets are intact |
| `/data/builds` | yes | warm build trees are intact, so nothing needs rebuilding |
| `/data/ssh/*` | yes | host key, authorized_keys, sshd_config are intact |
| `/usr/sbin/sshd` | **no** | port 2222 refuses connections; no way in except `flyctl machine exec` |
| `/root/.cargo`, `/root/.rustup` | **no** | builds fail with `cargo: command not found` |

The two failures in order, because the second masks the first:

1. **Port 2222 refuses connections.** The entrypoint is `sleep infinity` and there
   is no systemd (pid1 is Fly's `init`), so nothing starts our sshd after a
   restart. Fly's platform sshd listens on port 22 and rejects our key.
2. **`cargo: command not found`.** The `/root/.cargo` and `/root/.rustup`
   symlinks live on the ephemeral rootfs. `cargo-remote-3000` sources
   `~/.cargo/env`, so every build fails even though the toolchain is on the volume.

### The repair

```bash
make remote-repair
```

That runs the box's own `ensure-ready` script through `flyctl machine exec -a
<app> --no-container <machine> /data/bin/ensure-ready`, which reinstalls the
toolchain packages, starts sshd against `/data/ssh/sshd_config`, and restores both
symlinks. It is idempotent and it touches nothing under `/data/builds`, so it
cannot destroy another project's compile.

It must be run with `flyctl machine exec`, not `flyctl ssh console`: the latter
goes through the platform sshd it has no key for.

### Verifying the repair

```bash
ssh fly-builder 'readlink /root/.cargo; readlink /root/.rustup'   # both -> /data/*
ssh fly-builder 'export PATH=/data/cargo/bin:$PATH; rustc --version'  # 1.95.0
make busy                                                        # IDLE or BUSY, not an error
make headroom                                                    # confirm free space
make check                                                       # a real build
```

`rustc --version` alone is misleading: `rustup default` prints the pin from
`/data/rustup` even when the `/root/.cargo` symlink is missing and no build can
run. Check the symlinks and then run an actual build.

### Why this is not automated further

Durable self-healing needs an entrypoint change plus a `flyctl machine update`
and restart, and this box is shared with other projects. A restart triggered to
fix our own convenience would interrupt whatever those projects are compiling,
which PLAN.md 7.4 says is a data-loss error and not a routine cost. The repair is
therefore one deliberate command that a human runs when the box is already known
to be down, documented here so it is a two-minute action instead of a two-hour
diagnosis.
