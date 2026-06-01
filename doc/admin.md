# tbamud-rwb Administrator's Guide

Adapted from "The CircleMUD Administrator's Guide". This describes running,
configuring, and maintaining the Rust rewrite. Build/run basics are in
[`../README.md`](../README.md); this file is the operations companion.

## 1. Building and first boot

Prerequisites: a Rust 1.75+ toolchain and a C libcrypt (for DES password
hashing). Then, from the repository root:

```sh
cargo build --release
cargo run --release -- -p 4000
```

On a fresh data directory the first character created becomes the implementor
(level 34) automatically — there is no separate "create the implementor" step.
Connect, make your character, and you have full immortal powers immediately.

To validate world data without starting the game (useful after editing):

```sh
cargo run -- -c
```

## 2. Command-line flags

All flags mirror stock `comm.c` and are documented in [`../README.md`](../README.md). Summary:

| Flag | Meaning |
|------|---------|
| `-p <port>` | listen port (default 4000) |
| `-d <dir>` | data directory (default `lib`) |
| `-r` | restrict: no new-character creation |
| `-m` | mini-MUD: load the minimal world (`index.mini`); implies `-q` |
| `-s` | suppress mob special procedures |
| `-o <file>` | log to a file instead of stderr |
| `-c` | syntax-check world data and exit |
| `-q` | quick boot: skip the timed-out stored-object cleanup |

## 3. Configuration

There is no in-game CEDIT and no rich `lib/etc/config`. Most configuration is
compile-time, in `src/config.rs`:

- `DFLT_PORT`, `DFLT_DIR`, `MAX_PLAYING`
- `FREE_RENT` (in `src/interpreter.rs`, default true), `MIN_RENT_COST`,
  `MAX_OBJ_SAVE`, `CRYO_FACTOR`, `RENT_FACTOR`, `CRASH_FILE_TIMEOUT`,
  `RENT_FILE_TIMEOUT`

To change them, edit the constant and rebuild. A small set of runtime toggles
are controlled in-game by immortals (see below): wizlock, bans, mute/freeze.

## 4. Immortal level and powers

`LVL_IMMORT` is 34; mortals cap at level 30. Immortal-only commands return a
generic "Huh?!" to mortals so their existence stays hidden. Notable admin
commands (all in `src/interpreter.rs`):

- **Movement/inspection:** `goto`, `at`, `transfer`, `stat <thing>`, `status`,
  `olist`/`mlist`/`rlist`/`zlist`
- **World:** `load mob|obj <vnum>`, `purge`, `zreset <zone>`, `dig`,
  `oset` (object proto), `househere`/`house`
- **Players:** `set <player> <field> <value>`, `restore`, `reload`,
  `force <player> <cmd>`, `slay`, `snoop`/`unsnoop`
- **Moderation:** `mute`, `freeze`, `wizlock [level]`, `ban`/`unban`/`bans`,
  `invis [lvl]`/`vis`, `nohassle`, `peace`
- **Communication:** `wiznet`, `echo`, `gecho`

Wizlock (`wizlock <level>`) blocks logins below a level; `-r` blocks
new-character creation entirely. Both are described in
[`../README.md`](../README.md).

## 5. Day-to-day maintenance

**Player data.** Players are plain-text files under `lib/plrfiles/<bucket>/`,
indexed by `lib/plrfiles/index`. Saved objects are under `lib/plrobjs/<bucket>/`.
Both are human-readable; you can inspect or hand-fix them while a player is
offline. `stat <name>` works on offline players too.

**Autosave.** A background tick saves all online mortals every ~5 minutes, and
players save on quit/rent, so a crash loses at most a few minutes.

**Rent / stored-object cleanup.** With `FREE_RENT` on (the default), `rent` just
stores belongings and the player quits; objects persist via the normal save. At
boot the server deletes stored-object files for players idle past the timeout
(10 real-days for quit/crash saves, 30 for rented), unless `-q` is given.

**Site bans.** `ban <host>` / `unban <host>` / `bans` manage `lib/etc/badsites`
live; banned hosts are refused before the greeting.

**Bulletin boards.** Board contents live in `lib/etc/board.*` (plain text).
Boards are tied to object vnums (`boards.rs`); see [building.md](building.md) /
the data set for which rooms carry a board object.

**Player mail.** Stored in `lib/etc/plrmail/<name>.mail`. Offline `tell`s queue
here automatically.

**Feedback queues.** `bug`/`idea`/`typo` append to `lib/misc/{bugs,ideas,typos}`.
Review and clear these periodically.

**Text files.** Everything under `lib/text/` (motd, imotd, news, credits,
policies, handbook, wizlist, immlist, background, greetings) is plain text shown
by the matching command. Edit freely; changes take effect on the next read
(restart to be safe — there is no `reload` of arbitrary text yet).

## 6. Backups and shutdown

Back up the whole `lib/` tree (especially `plrfiles/`, `plrobjs/`, `etc/`,
`house/`) on a schedule. Because everything is plain text, backups diff and
restore cleanly.

`shutdown` (immortal) broadcasts a notice, flushes, and exits the process. There
is no built-in auto-reboot loop or `autorun` script; run the binary under your
process supervisor of choice (systemd, a shell `while` loop, tmux, etc.) if you
want automatic restarts.

## 7. Logs

Operational logging goes through `tracing` to stderr or the `-o` file; control
verbosity with `RUST_LOG` (see [debugging.md](debugging.md)). Structured logs
such as PvP kills are written under `lib/log/`. There is no stock `log/` tree
(rip/levels/olc/…).
