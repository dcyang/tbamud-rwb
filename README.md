# tbamud-rwb

A **Rust rewrite of [TbaMUD](http://tbamud.com)** (The Builder's Academy MUD),
a CircleMUD/DikuMUD derivative. The goal is feature-parity with stock TbaMUD
from the player's point of view: the same login flow, world, combat, spells,
skills, shops, channels, and immortal toolkit — reimplemented in safe,
asynchronous Rust on top of [Tokio](https://tokio.rs).

The compiled binary is named `circle` (matching the upstream CircleMUD
convention), and it reads its world/player data from a `lib/` directory, just
like the original C server.

## Requirements

- **Rust 1.75.0** or newer (edition 2021). The dependencies are pinned to
  versions compatible with this toolchain; newer crate releases require a
  newer compiler.
- A C toolchain with **`libcrypt`** available. `build.rs` links against it for
  DES password hashing (`crypt(3)`), matching the on-disk player-file format.
- A populated `lib/` data tree (world, mobiles, objects, zones, shops,
  triggers, help, player files). This repository ships one.

On Debian/Ubuntu the crypt library comes from `libcrypt-dev`:

```bash
sudo apt-get install build-essential libcrypt-dev
```

## Building

From the repository root:

```bash
cargo build            # debug build  -> target/debug/circle
cargo build --release  # optimized    -> target/release/circle
```

## Launching

The server expects to be run from the repository root so that it can find the
`lib/` data directory. The simplest way to start it on the default port
(**4000**) is:

```bash
cargo run -- -p 4000
```

Or run the compiled binary directly:

```bash
./target/debug/circle -p 4000
```

Once you see `Listening for connections port=4000` in the log, connect with any
telnet client:

```bash
telnet localhost 4000
```

You will be greeted, can create a new character or log into an existing one,
and then drop into the game.

### Command-line options

The flags mirror the upstream `comm.c` arguments. Run `circle -h` for the full
list. These are fully wired today:

| Flag        | Meaning                                                      | Default |
|-------------|--------------------------------------------------------------|---------|
| `-p <port>` | TCP port to listen on                                        | `4000`  |
| `-d <dir>`  | Data directory to read `lib/` data from                      | `lib`   |
| `-r`        | Restrict — disallow new-character creation                   | off     |
| `-m`        | Mini-MUD mode — load the minimal world (`index.mini`)        | off     |
| `-s`        | Suppress special procedures (mob spec_procs)                 | off     |
| `-o <file>` | Write the log to `<file>` instead of stderr                  | stderr  |
| `-c`        | Syntax-check: load the world data, report, and exit          | off     |

The one remaining upstream flag is **accepted on the command line but has no
effect** — it is parsed for CLI compatibility only:

| Flag        | Intended meaning                          | Status                                  |
|-------------|-------------------------------------------|-----------------------------------------|
| `-q`        | Quick boot (skip rent checks)             | no-op — this port has no rent system    |

For example, to boot a second instance on another port with its own data
directory:

```bash
cargo run -- -p 4050 -d /path/to/other/lib
```

A bare trailing number is also accepted as the port (e.g. `circle 4000`),
matching the original C behavior.

## Running unit tests

The project ships a suite of unit tests covering low-level building blocks
(telnet IAC stripping, ASCII flag conversion, DES password round-tripping,
ANSI color-code conversion, mailbox and bulletin-board serialization, and
more). Run them all with:

```bash
cargo test
```

You should see a summary like:

```
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Useful variations:

```bash
cargo test color          # run only tests whose name contains "color"
cargo test -- --nocapture # show println!/log output from tests
cargo test --release      # run the suite against an optimized build
```

The tests do not open a network socket or require a running server, so they
are safe to run in CI or alongside a live instance.

## Repository layout

| Path            | Contents                                                        |
|-----------------|-----------------------------------------------------------------|
| `src/`          | The Rust server (server, descriptor, interpreter, combat, db …) |
| `lib/`          | Runtime data root: world, players, text, etc.                   |
| `Cargo.toml`    | Crate manifest; binary target `circle`                          |
| `build.rs`      | Links `libcrypt` for DES password hashing                       |
| `CLAUDE.md`     | Developer notes and the parity progress log                     |
| `LICENSE.md`    | License (see below)                                             |

## License

tbamud-rwb is a derivative work of TbaMUD, CircleMUD, and DikuMUD, and is
distributed under the terms in [`LICENSE.md`](LICENSE.md). In short: you are
free to use, study, modify, and redistribute it, but — as the predecessor
licenses require — **not for commercial gain**, and you must preserve the
author credits and carry the licenses with any copy you distribute.
