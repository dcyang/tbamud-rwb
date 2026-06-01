# Errors and Log Messages in tbamud-rwb

Adapted from the stock TbaMUD SYSERR list.

## How the rewrite reports errors

The rewrite logs through the `tracing` crate (not C `log()`/SYSERR), at levels
INFO / WARN / ERROR, to stderr or to the file named with `-o` (see
[admin.md](admin.md), [debugging.md](debugging.md)). There is no `errors`
syslog file; filter the log stream by level instead (e.g. `RUST_LOG=warn`).

World-data problems are surfaced at boot:

- A malformed zone/room/object/mob file is a **hard error** — `db::load_world`
  returns `Err` and the server refuses to boot, naming the file. Use the `-c`
  flag to do exactly this check (load the data, report, and exit) without
  starting the game.
- A malformed trigger/quest/shop file is logged at **WARN** and that single
  file is skipped, so one bad file doesn't take the world down.

## The stock SYSERR list, mapped to the rewrite

Unlike stock, there is no OLC, so the `oedit-s-desc` / `medit-s-desc` / `zedit`
style SYSERRs do not occur — but the underlying *data* problems they warned
about can still exist in hand-edited world files.

- **1 — Exits to NOWHERE.** A room exit whose destination vnum doesn't exist
  (or is `-1`) leads nowhere. The rewrite simply treats such an exit as
  impassable ("Alas, you cannot go that way…") rather than crashing. Fix it by
  editing the `D<dir>` block in the `.wld` file (or removing it). There is no
  `show error` command yet; grep the `.wld` files, or use the immortal
  `stat`/`goto` commands to inspect rooms.

- **2 — Drink container aliases.** Stock shop code needs the drink type as the
  last keyword so a jug shows as "a jug of \<liquid\>". The rewrite's
  drink-container handling does not impose this, but keeping the liquid type in
  the keyword list is still good practice for findability.

- **3 — Mob both Aggressive and Aggressive-to-alignment.** Harmless in the
  rewrite too: `MOB_AGGRESSIVE` attacks anyone, so the per-alignment aggro bits
  are redundant when it's set. Pick one.

- **4 — Object spell/level out of range.** Keep spell levels within 1–34 (the
  rewrite's mortal range) and use realistic values; the loader tolerates odd
  numbers but they make no sense in play.

- **5 — Absurd weights / levels (huge numbers).** Rust integer parsing won't
  crash on these the way old C did, but use sane values — garbage in, garbage
  out.

- **6 — "UNDEFINED" spell on an object.** Use a real spell vnum, or `-1` for
  none. An object referencing a spell the rewrite doesn't implement simply won't
  cast anything.

- **7 — Drink container contains more than its maximum.** Keep `value[1]`
  (current) ≤ `value[0]` (capacity).

- **8–9, 11 — medit/zedit-equip conflicts, spec_assign to a non-existent mob,
  "Mob using player_specials".** These are C/OLC-internal errors with no
  analogue: the rewrite assigns mob specs by vnum in code
  (`world.rs::MobSpec::for_vnum`), keeps player-only state on the `Character`
  type so a mob can never read it, and equips mobs via the zone `E` reset (a
  busy slot just falls back to inventory).

- **10 — Board has no associated object.** Bulletin boards are keyed to object
  vnums in code (`boards.rs::BOARDS`); a board with no object in the room is
  simply never found, and `read`/`write` reply "Sorry, but you cannot do that
  here!". No file surgery required.

- **12 — NOTE object with a duplicate extra-description keyword.** Still good
  advice for data hygiene; the rewrite renders the action/extra description it
  finds.

## When you hit a real error

Run with `-c` to validate world data, or `RUST_LOG=debug` for verbose tracing.
A panic (Rust's equivalent of a crash) prints a message and, with
`RUST_BACKTRACE=1`, a backtrace — include that when reporting a bug. See
[debugging.md](debugging.md).
