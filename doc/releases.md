# tbamud-rwb Release / Version History

Adapted from the stock TbaMUD/CircleMUD release-history document.

## Lineage

tbamud-rwb is a Rust reimplementation of TbaMUD, which descends from CircleMUD,
which descends from DikuMUD:

```
DikuMUD (1990-91)  ->  CircleMUD (1993-2002)  ->  TbaMUD (2006-present)
                                                     -> tbamud-rwb (Rust)
```

The stock release history (CircleMUD 3.0 through TbaMUD's annual releases) is
preserved upstream at `../../tbamud/doc/releases.txt` and on the Builder's
Academy. The rewrite does not re-version against those numbers.

## Versioning of the rewrite

The rewrite does not cut numbered releases. Its history is the git commit log.
Each checkpoint added one feature area (combat, spells, shops, channels, the
rent system, …) and kept the build green with `cargo test` passing.

- For *what changed and when*, read `git log`.
- The reported in-game version string comes from the `version` command
  (`src/interpreter.rs`); the crate version is in [`../Cargo.toml`](../Cargo.toml).

## Goal

The project's stated goal is faithful parity with stock TbaMUD from the player's
point of view — porting the stock player-facing gameplay into Rust, and adding
nothing that isn't in stock TbaMUD.
