# Using Color in tbamud-rwb

Adapted from the stock TbaMUD "Using Color" document. The C version described
the `screen.h` macros (`CCRED`/`KWHT`/…) and four color "levels" (off / brief /
normal / complete). The Rust rewrite uses a simpler, data-driven scheme that
matches the TbaMUD-style `@` color codes already present in the world data.

## The `@` color codes

Color is written inline in any string the game sends, using `@` followed by a
single letter. This is the same `@x` convention used in the stock TbaMUD world
files (see e.g. `lib/world/wld/0.wld`), not CircleMUD's `&` codes and not raw
ANSI escapes. The rewrite translates these codes to ANSI SGR sequences at the
moment a line is written to a socket (see `src/color.rs::convert`).

| Code     | Meaning |
|----------|---------|
| `@n`     | reset to normal |
| `@r` `@R` | red / bright red |
| `@g` `@G` | green / bright green |
| `@y` `@Y` | yellow / bright yellow |
| `@b` `@B` | blue / bright blue |
| `@m` `@M` | magenta / bright magenta (`@p` / `@P` are accepted as aliases) |
| `@c` `@C` | cyan / bright cyan |
| `@w` `@W` | white / bright white |
| `@d` `@k` | black (dark) |
| `@K`     | bright black (grey) |
| `@@`     | a literal `@` character |

An unknown code (e.g. `@z`) is passed through verbatim, so stray `@`s are
harmless.

## Per-player color

There are no "brief / normal / complete" sub-levels. Color is simply on or off
per player, controlled by:

- `color` — show or toggle your color setting
- `toggle color` — flip color on/off from the toggle screen

The preference is stored as `color_off` on the character and persists across
sessions (saved as `ClOf: 1` in the player file when color is OFF).

## How it works internally

The per-connection writer task (`src/descriptor.rs`) checks the player's
`color_off` flag for every line it sends:

- color **on** → `color::convert(line)` replaces each `@x` with the ANSI escape.
- color **off** → `color::strip(line)` removes every `@x` (and turns `@@` into
  `@`) so a non-ANSI client sees clean text.

Because conversion happens at send time, code that builds messages never has to
know the recipient's color setting (this is the main simplification over the C
`CCxxx(ch, level)` macros, which had to be told the viewer's level up front).
To emit color from Rust, just put the codes in the string:

```rust
CmdOutput::text(format!("\r\n@cYou gossip, '{msg}'@n\r\n"))
```

Always pair a color code with a closing `@n`, so subsequent output isn't left
tinted.

Unit tests for the converter (plain passthrough, code mapping, `@@` escape, and
unknown-code passthrough) live in `src/color.rs` and run under `cargo test`.
