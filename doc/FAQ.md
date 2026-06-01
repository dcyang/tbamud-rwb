# tbamud-rwb Frequently Asked Questions

Adapted from the stock TbaMUD/CircleMUD FAQ. Most of the stock FAQ is about
compiling C on assorted platforms/compilers (cc syntax errors, crypt link
errors, makefiles, bcopy/strdup, autorun, Winsock, etc.). Almost none of that
applies to the Rust rewrite, which builds with one command. This FAQ keeps the
still-relevant questions and adds rewrite-specific ones.

**Contents**

1. General
2. Building
3. Running
4. Gameplay / features
5. Building the world / coding

## 1. General

### 1.1 What is tbamud-rwb?

A from-scratch reimplementation of TbaMUD in Rust, aiming for parity with stock
TbaMUD from the player's point of view.

### 1.2 How does it relate to TbaMUD / CircleMUD / DikuMUD?

It is a derivative of all three (and bound by their licenses; see
[`../LICENSE.md`](../LICENSE.md)). It reads the same world data and reproduces
the same gameplay, rewritten in Rust on Tokio.

### 1.3 I've never coded in Rust. Can I still run it?

Yes — running it needs only `cargo` and a C libcrypt; you don't need to write any
Rust. To modify it, read [coding.md](coding.md).

## 2. Building

### 2.1 How do I build it?

`cargo build --release` from the repo root. That's the whole story — no
`./configure`, no Makefile, no per-platform README. See
[`../README.md`](../README.md).

### 2.2 I get a link error about `crypt`.

This is the one platform dependency. The server links the C `libcrypt` for DES
password hashing (`build.rs`). Install your platform's libcrypt (e.g.
`libcrypt-dev` / `libxcrypt`) and rebuild. See [porting.md](porting.md) if your
libc lacks `crypt(3)` entirely.

### 2.3 Which platforms are supported?

Anything with a Rust 1.75+ toolchain and libcrypt: Linux, the BSDs, macOS,
Windows. The obsolete stock platform guides (AMIGA/ARC/OS2/VMS/Borland/Watcom/old
MSVC) do not apply.

### 2.4 How do I grep / search the source?

Use `rg` (ripgrep) or `grep -rn` over `src/`. On Windows, ripgrep or the editor's
search both work.

### 2.5 Build is full of warnings — is that a problem?

Warnings (unused imports, dead code) are harmless and not errors. Isolate real
errors with `cargo build 2>&1 | grep -E "^error"`. `cargo clippy` reports
style/likely-bug lints.

## 3. Running

### 3.1 How do I start the MUD and connect?

`cargo run -- -p 4000`, then `telnet localhost 4000`. Wait for "Listening for
connections" in the log.

### 3.2 It loads the whole world slowly — can I boot a small world for testing?

Yes: `-m` (mini-MUD) loads `index.mini` instead of the full index.

### 3.3 How do I become an immortal/implementor?

The first character created in a fresh data directory is made level 34
(implementor) automatically. Use a throwaway `-d` copy of `lib/` if you want a
clean implementor for testing.

### 3.4 How do I stop new players from being created? / lock the game?

`-r` at the command line disables new-character creation. In-game,
`wizlock <level>` blocks logins below a level.

### 3.5 Where do the logs go?

To stderr by default, or to a file with `-o <file>`. Control verbosity with the
`RUST_LOG` env var (see [debugging.md](debugging.md)).

### 3.6 A connection error / panic appeared in the log — what do I do?

The server runs one task per connection plus background ticks; a panic in one is
logged without necessarily killing the server. Re-run with `RUST_BACKTRACE=1` and
include the message + backtrace when reporting.

## 4. Gameplay / features

### 4.1 Is it feature-complete vs stock TbaMUD?

The player-facing command/spell/skill set, shops, channels, quests, DG triggers
(a subset), the rent system, and boards are implemented.

### 4.2 Is there OLC (online creation)?

Yes — the full editor set is implemented for immortals: `redit`, `oedit`,
`medit`, `zedit`, `qedit`, `trigedit`, `sedit`, `aedit`, `hedit`. Each is a
menu-driven editor that commits to the live world and rewrites the data file on
quit (see [building.md](building.md)). You can still hand-edit the data files
and restart if you prefer. (`cedit`/`prefedit`/`msgedit` are not ported — config
is compile-time + command-driven, prefs use `toggle`/`set`, and combat messages
are in code.)

### 4.3 Colors show as garbage / don't show.

Color uses `@x` codes converted to ANSI at send time ([color.md](color.md)).
Toggle it per-player with `color` or `toggle color`; clients without ANSI should
set it off.

### 4.4 Does it support MSDP/GMCP/MXP/MCCP/256-color?

No. Only basic telnet negotiation (echo on/off for passwords, suppress-GA,
NAWS/TTYPE requests) is implemented. See [ProtocolSystem.md](ProtocolSystem.md).

### 4.5 Rent took my stuff / players can't find an innkeeper.

Rent is free by default (you just quit and your objects are saved). In the stock
data set no zone actually spawns a receptionist, so `offer`/`rent` reply "you
cannot do that here" — identical to stock on the same data. See
[admin.md](admin.md).

### 4.6 Can I use stock TbaMUD areas and player files?

Yes — the world formats and the ASCII player/password format are compatible. See
[porting.md](porting.md).

## 5. Building the world / coding

### 5.1 How do I add a room/mob/object/zone?

Hand-edit the `lib/world/` files and update the index files; validate with `-c`.
See [building.md](building.md).

### 5.2 How do I add a command or spell in the code?

See [coding.md](coding.md) (commands: `COMMANDS` table + dispatch + handler;
spells/skills: the seven-place `Skill` registration + a cast handler).

### 5.3 Where do I report bugs or ask for help?

This repository is the surface for the rewrite. The original TbaMUD community
is at The Builder's Academy, telnet tbamud.com 9091.
