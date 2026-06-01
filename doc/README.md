# tbamud-rwb Documentation (doc/)

tbamud-rwb is a from-scratch reimplementation of TbaMUD (a CircleMUD/DikuMUD
derivative) in the Rust programming language. These docs are adapted from the
stock TbaMUD `doc/` tree to describe how the *Rust* rewrite actually works. Where
stock TbaMUD documents C internals, autoconf, or in-game OLC editors that the
rewrite does not (yet) implement, the corresponding file here says so plainly
instead of describing machinery that isn't present.

The authoritative top-level docs live in the repository root:

- [`../README.md`](../README.md) — build & run instructions (cargo),
  command-line flags, tests.
- [`../LICENSE.md`](../LICENSE.md) — license (derivative of TbaMUD / CircleMUD /
  DikuMUD).

## Building & running

There is no `configure`, Makefile, or per-platform build procedure. The server
builds with Cargo on any platform with a Rust 1.75+ toolchain and a C `libcrypt`
(for DES password hashing):

```sh
cargo build                 # debug   -> target/debug/circle
cargo build --release       # release -> target/release/circle
cargo run -- -p 4000        # run on port 4000 (default)
telnet localhost 4000
```

The compiled binary is named `circle` (CircleMUD convention) and reads its data
from a `lib/` directory, exactly like the C server.

Command-line flags (`-p -d -r -m -s -o -c -q`) are documented in
[`../README.md`](../README.md).

## Documentation index (this directory)

**Content / gameplay**

- [files.md](files.md) — the `lib/` data tree as the rewrite uses it.
- [color.md](color.md) — the `@` color-code system (rwb uses `@x`, not `&` or
  raw ANSI).
- [socials.md](socials.md) — socials and the `socials.new` format.
- [building.md](building.md) — world-data file formats; how to edit the world
  (by hand).
- [act.md](act.md) — how the rewrite sends messages to players (no C `act()`).
- [msgedit.md](msgedit.md) — the combat/skill messages file.
- [dg_events.md](dg_events.md) — DG Scripts: which trigger types the rewrite
  supports.
- [ProtocolSystem.md](ProtocolSystem.md) — telnet/protocol support (basic; no
  MSDP/GMCP/MXP).
- [porting.md](porting.md) — using stock CircleMUD/TbaMUD world data with the
  rewrite.

**Administration / operations**

- [admin.md](admin.md) — running, configuring, and maintaining a rewrite server.
- [syserr.md](syserr.md) — log / SYSERR-style messages and what they mean.
- [debugging.md](debugging.md) — debugging the Rust server (`RUST_LOG`,
  lldb/rust-gdb).
- [testing.md](testing.md) — the `cargo test` unit-test suite.
- [utils.md](utils.md) — the C utility programs and their rewrite status.
- [releases.md](releases.md) — versioning / where the changelog lives.
- [license.txt](license.txt) — pointer to the adapted
  [`../LICENSE.md`](../LICENSE.md).
- [FAQ.md](FAQ.md) — frequently asked questions, adapted for the rewrite.
- [coding.md](coding.md) — Rust coding conventions for contributors.

Obsolete C build guides (`README.UNIX`/`WIN`/`MSVC*`/`BSD`/`CYGWIN`/`OS2`/`VMS`/
`AMIGA`/`ARC`/`BORLAND`/`WATCOM`/`CMAKE`): these described compiling the C server
on specific platforms/toolchains. The rewrite builds uniformly with `cargo`, so
each of those files now contains only a short note redirecting here.

## Getting help

The original TbaMUD community lives at The Builder's Academy (telnet
tbamud.com 9091). For the rewrite specifically, the code, parity log, and issue
surface are this repository.
