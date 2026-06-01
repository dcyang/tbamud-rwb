# tbamud-rwb File Manifest

Adapted from the stock TbaMUD file manifest to describe the layout the Rust
rewrite actually reads and writes. The world-data layout under `lib/world/` is
unchanged from stock (the rewrite loads the same files); the differences are in
the source tree (Rust, not C) and in a few runtime/state files.

## Top-level repository

| Path | Contents |
|------|----------|
| `Cargo.toml` | crate manifest; binary target is named `circle`. |
| `build.rs` | links libcrypt for DES password hashing. |
| `src/` | the Rust source (see below). |
| `lib/` | MUD data root (passed via `-d`, default `lib`). |
| `doc/` | this documentation. |
| `target/` | Cargo build output (binary at `target/debug/circle`); not in git. |
| `README.md` | build & run. `LICENSE.md` |

There is no `bin/`, no `autorun`, no `Makefile`, and no top-level
changelog/syslog file: the binary lives under `target/`, logging goes to stderr
(or to the file given with `-o`), and the change history lives in git.

## `src/` (Rust modules)

| Module | Responsibility |
|--------|----------------|
| `main.rs` | arg parsing, logging setup, calls `server::run()`. |
| `config.rs` | compile-time defaults + the `Config` struct. |
| `server.rs` | bind/listen, load shared state, accept loop, background ticks. |
| `descriptor.rs` | per-connection handler, telnet I/O, login, writer task. |
| `login.rs` | login state machine (name → password → … → menu). |
| `character.rs` | `Character`, `PlayerHandle`, `CharacterList` (online registry). |
| `interpreter.rs` | command dispatch + all command/spell handlers. |
| `combat.rs` | the combat tick and damage/death resolution. |
| `db.rs` | world loading (`.zon`/`.wld`/`.obj`/`.mob`/`.shp`/`.qst`/`.trg`) + ticks. |
| `world.rs` | Room/Zone/Obj/Mob/Exit types and the `World` container. |
| `players.rs` | player index + ASCII player files + object persistence. |
| `mail.rs` | the mudmail spool. |
| `boards.rs` | bulletin-board storage. |
| `color.rs` | `@x` → ANSI conversion / stripping. |
| `telnet.rs` | IAC constants + telnet negotiation helpers. |

## `lib/` (data root)

| Directory | Contents |
|-----------|----------|
| `etc/` | server-maintained state; do not edit while the game is running. |
| `house/` | per-house save files (one per house room vnum). |
| `misc/` | messages, socials, and player-submitted bug/idea/typo queues. |
| `plrfiles/` | ASCII player files, bucketed by first letter. |
| `plrobjs/` | per-player saved objects (inventory + equipment), bucketed. |
| `plrvars/` | reserved for per-player script variables. |
| `text/` | flat text shown to players; reloadable. |
| `world/` | the world database (rooms, mobs, objects, zones, shops, …). |

`lib/etc/` contains:

- `config` — config file (present but minimal; see [admin.md](admin.md) — there
  is no in-game CEDIT, most config lives in `src/config.rs`).
- `last` — last-login bookkeeping.
- `plrmail` — the mudmail spool directory (one `<name>.mail` file per recipient).
- `badsites` — banned host substrings (one per line); edited live via `ban`.
- `board.*` — bulletin-board contents (one file per board vnum), e.g.
  `board.mortal` / `board.immortal` / `board.social` / … (text, not binary as
  in stock).

`lib/misc/` contains:

- `messages` — combat / skill damage messages.
- `socials` — legacy social text.
- `socials.new` — AEdit-format socials; this is the file the rewrite loads.
- `bugs` / `ideas` / `typos` — player reports from the bug/idea/typo commands.
- `xnames` — disallowed player names.

`lib/text/` contains (each shown by the matching command):

- `greetings` — the connect screen.
- `motd` — mortal message-of-the-day; `imotd` — immortal MOTD.
- `news`, `credits`, `info`, `policies`, `handbook`, `background`, `wizlist`,
  `immlist` — shown by the commands of the same name.
- `help/` — the help database (loaded by the help system).

`lib/plrfiles/` and `lib/plrobjs/` are each split into first-letter buckets
(`A-E`, `F-J`, `K-O`, `P-T`, `U-Z`, `ZZZ`). `plrfiles/index` is the player
index, in the form `<id> <Name> <level> <flags> <last_login>` per line,
terminated by `~`. A player's saved objects live at
`lib/plrobjs/<bucket>/<name>.objs`.

`lib/world/` subdirectories (unchanged from stock):

| Dir | Files |
|-----|-------|
| `wld` | rooms (`*.wld`) |
| `mob` | mobiles (`*.mob`) |
| `obj` | objects (`*.obj`) |
| `zon` | zones / resets (`*.zon`) |
| `shp` | shops (`*.shp`) |
| `trg` | DG triggers (`*.trg`) |
| `qst` | quests (`*.qst`) |

Each world subdirectory has an `index` (files loaded at boot) and an
`index.mini` (the smaller set loaded with the `-m` / mini-MUD flag). To add or
remove a zone you must update the `index` in every relevant subdirectory; see
[building.md](building.md).

## Logs

The rewrite does not maintain the stock `log/` tree (badpws, rip, levels, olc,
…). Operational logging goes through the `tracing` crate to stderr, or to the
file named with the `-o` flag. A couple of structured logs are written under
`lib/log/` on demand — notably `pkill.log` (PvP kills). See
[debugging.md](debugging.md) and [syserr.md](syserr.md).
