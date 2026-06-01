# tbamud-rwb Unit Testing

_Adapted from the stock TbaMUD `testing.md`._

Stock TbaMUD ships a C unit-test suite built on the Unity framework, driven by
`./configure` + a `tests/Makefile`, with a Python script to convert Unity output
to JUnit XML.  The Rust rewrite has none of that machinery: it uses Cargo's
built-in test harness, so there is no Unity, no `configure`, no JUnit
conversion, and no separate `tests/` tree to wire up.

## Running the tests

From the repository root:

```sh
cargo test
```

You should see a summary like:

```
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Useful variations:

```sh
cargo test color           # run only tests whose name contains "color"
cargo test -- --nocapture  # show println!/log output from tests
cargo test --release       # run against an optimized build
```

The tests open no sockets and need no running server or `lib/` data, so they
are safe to run in CI or alongside a live instance.

## Where tests live

Rust unit tests live inline in the module they exercise, inside a
`#[cfg(test)] mod tests { ... }` block at the bottom of the file, and run with
`#[test]`.  Current coverage includes low-level building blocks such as:

- telnet IAC stripping (`src/telnet.rs`)
- ASCII flag conversion and the DES password round-trip (`src/players.rs`)
- `@x` color conversion / stripping (`src/color.rs`)
- mailbox escape round-trip (`src/mail.rs`)
- bulletin-board serialization round-trip (`src/boards.rs`)

The total is reported by `cargo test` (currently 15).

## Writing a new test

Add a test next to the code it covers:

```rust
// at the bottom of src/foo.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_the_thing() {
        assert_eq!(2 + 2, 4);
    }
}
```

`cargo test` discovers it automatically -- there is nothing to register in a
makefile and no stubs to maintain (the test compiles against the real module).
Use the standard `assert!`, `assert_eq!`, and `assert_ne!` macros.

Most current tests are pure functions over owned data.  Anything that needs the
async runtime, shared `World`/`CharacterList` state, or a socket is exercised by
hand against a running server (see the scripted-telnet examples in the project
history) rather than in the unit suite; keep unit tests fast and dependency-free.

## CI

There is no committed CI workflow yet.  If one is added, the single command it
needs is `cargo test` (optionally `cargo build --release` first); Cargo's exit
code already reflects pass/fail, so no XML conversion step is required.
