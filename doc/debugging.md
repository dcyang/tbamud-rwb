# Debugging tbamud-rwb

Adapted from the stock TbaMUD "Art of Debugging" (excerpted from MERC's
`hacker.txt` by Furey). The general philosophy in the original still holds and
is worth reading: learn your tools deeply, alternate between playing with
something and reading about it, and remember that engineering is work. This file
covers the *Rust-specific* tooling that replaces the C/gdb workflow.

## The build/run loop

```sh
cargo build           # compile (debug); errors are precise -- read them top-down
cargo build --release # optimized build
cargo run -- -p 4000  # build + run
cargo test            # unit tests (see testing.md)
```

The compiler is your first debugger. Rust's type checker and borrow checker
catch at compile time most of the bugs that, in C, you would chase with gdb at
run time (use-after-free, data races, null derefs). When the compiler rejects
code, the message usually names the exact fix; don't fight it — read it.

## Logging (the main runtime tool)

The server logs through the `tracing` crate. Control verbosity with the
`RUST_LOG` environment variable:

```sh
RUST_LOG=info  cargo run -- -p 4000     # default
RUST_LOG=debug cargo run -- -p 4000     # verbose
RUST_LOG=warn  cargo run -- -p 4000     # warnings + errors only
RUST_LOG=tbamud_rwb::combat=debug cargo run -- ...   # one module
```

Use `-o <file>` to send the log to a file instead of stderr. Add temporary
`tracing::debug!(?value, "what happened")` lines while investigating; remove
them when done (the unused-variable / dead-code warnings will remind you).

## Panics and backtraces

A Rust panic is the equivalent of a controlled crash: it unwinds with a message
naming the file and line. For a full backtrace:

```sh
RUST_BACKTRACE=1 cargo run -- -p 4000      # backtrace on panic
RUST_BACKTRACE=full cargo run -- ...        # include std frames
```

Always include the panic message and backtrace when reporting a bug. Note the
server spawns one Tokio task per connection plus several background ticks; a
panic in one task is logged but does not necessarily take the whole server down
— watch the log for `task error` / `panicked` lines.

## Source-level debugging

For stepping, use the Rust wrappers around the system debugger:

```sh
rust-lldb target/debug/circle -- -p 4000
rust-gdb  target/debug/circle -- -p 4000
```

These load Rust pretty-printers so `String`, `Vec`, enums, etc. display
sensibly. In practice, logging + the compiler catch most issues, so reach for
the stepping debugger mainly for logic bugs in a specific code path.

## Validating world data

A large class of "bugs" are really bad world data. Validate it without booting
the game:

```sh
cargo run -- -c              # load all world data, report, exit (see syserr.md)
```

Parse failures in zone/room/object/mob files are hard errors that name the file;
trigger/quest/shop problems are logged at WARN and skipped.

## Reproducing player-facing behavior

There is no test harness for the live game loop; reproduce interactively. A
quick scripted telnet session (printf the inputs with small sleeps, piped to
`nc localhost <port>`) against a throwaway `-d` copy of `lib/` is the standard
way to drive a specific command path end to end without disturbing real player
files. The first character created in a fresh data dir becomes an implementor
(level 34), which is handy for `load`, `goto`, `stat`, etc. while testing.

## Useful extra tools

```sh
cargo clippy          # lints beyond the compiler's (style, likely bugs)
cargo fmt             # canonical formatting
cargo build 2>&1 | grep -E "^error"   # isolate hard errors from warnings
```
