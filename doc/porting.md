# Portability and Data Compatibility (tbamud-rwb)

Adapted from the stock TbaMUD "Porting to New Platforms" document (originally by
Jeremy Elson). The stock document is about making the C source compile on
non-UNIX platforms by hand-editing `conf.h`. For the Rust rewrite that whole
problem largely disappears, and "porting" instead means two things: running on a
given OS, and reusing stock world/player data.

## Porting to a platform (the build)

There is no `configure`, no `conf.h`, and no per-platform `#ifdef` maze. The
rewrite builds with Cargo and the Rust standard library + Tokio, which abstract
sockets and non-blocking I/O across platforms. To run on a new platform you need
only:

1. A Rust 1.75+ toolchain (`cargo`).
2. A C `libcrypt` providing DES `crypt(3)`, which `build.rs` links for password
   hashing. This is the single platform dependency of note.

```sh
cargo build --release
```

Anywhere those two exist — Linux, the BSDs, macOS, Windows (MSVC or GNU
toolchain), etc. — the same source compiles unchanged. The obsolete stock
platform guides (AMIGA, ARC, OS/2, VMS, Borland, Watcom, old MSVC) do not apply:
those platforms either lack a modern Rust toolchain or are no longer relevant.

The one porting nuance is `crypt(3)`: if a target's libc does not provide it,
either install a libcrypt (e.g. `libxcrypt`) or replace the small
`extern "C" { fn crypt }` shim in `src/players.rs` + the link directive in
`build.rs` with a pure-Rust DES implementation. Everything else is portable Rust.

## Porting data (using stock world/player files)

The more useful sense of "porting" for the rewrite is data compatibility:

- **World data.** The rewrite reads the stock TbaMUD world formats directly
  (`.wld`/`.mob`/`.obj`/`.zon`/`.shp`/`.qst`/`.trg`). You can drop a stock
  `lib/world/` tree in and it will load (subject to the parser supporting the
  records it uses; unknown records are skipped with a warning rather than
  aborting the boot). See [building.md](building.md) for the formats and
  [syserr.md](syserr.md) for what the loader warns about.

- **Player files.** Player records are the same ASCII key/value format, and
  passwords use the same DES `crypt(3)` salt scheme, so a stock player file is
  readable by the rewrite and vice versa for the fields both understand. Fields
  the rewrite adds (e.g. `rent_per_day`) are simply absent in older files and
  default cleanly; fields it does not yet model are ignored on load.

- **Importing older DikuMUD/CircleMUD areas.** As with stock, very old area
  formats may need a one-time hand conversion to the current TbaMUD layout
  before they load. The rewrite does not ship conversion utilities (see
  [utils.md](utils.md)); convert with a small script against the plain-text
  files, or bring the area up to current format by hand using an existing file
  in the same `lib/world/<type>/` subtree as a template.

If you bring in new world files, remember to add them to the `index` (and, for
the mini world, `index.mini`) in each relevant `lib/world/<type>/` directory —
the loader reads those indexes to decide what to boot.
