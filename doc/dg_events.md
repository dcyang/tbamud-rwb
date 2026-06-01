# Timed Behavior and DG Scripts in tbamud-rwb

Adapted from the stock TbaMUD "MUD event system" document (Vatiken / Eric
Green), which describes a C event queue (`mud_events.c`, `NEW_EVENT`,
`EVENTFUNC`, per-character event lists) layered on the Death Gate event code.

The Rust rewrite does **not** have that event-queue subsystem. Asynchronous and
timed behavior is expressed with Tokio instead, in two forms: global background
ticks, and per-script waits. This file maps the stock concepts onto what the
rewrite actually does.

## 1. Global background ticks (the "AI / Weather / Combat" events)

Where stock enqueues recurring global events, the rewrite spawns one long-lived
Tokio task per recurring job from `server::run` at boot. Each is a
`tokio::time::interval` loop that locks the shared world/character state, does
its work, and sleeps. Current ticks (see `src/db.rs` and `src/combat.rs`):

| Tick | Interval | Job |
|------|----------|-----|
| combat | 2s | resolve all fights for the round |
| zone reset | periodic | re-pop zones per their lifespan |
| decay | periodic | corpse / timed-object decay |
| obj timer | 60s | `OTRIG_TIMER` objects + extraction |
| random trigger | 30s | `WTRIG_RANDOM` / `MTRIG_RANDOM` rolls |
| hunger/thirst | 60s | food/drink/intoxication decay |
| idle kick | 60s | disconnect mortals idle > 30m |
| game clock | 75s | advance the in-game hour |
| weather | 75s | pressure walk + sky transitions |
| mob spec | 10s | puff/fido/janitor/… spec procs |
| mob regen | 30s | out-of-combat mob HP regen |
| light burn | 75s | lit light sources consume fuel |
| wander | periodic | sentinel-free mobs wander |
| random encounter | 300s | ambient wilderness spawns |
| save-all | 300s | crash-safe periodic player save |
| house save | 300s | persist `ROOM_HOUSE` contents |

To add recurring behavior, write a `db::spawn_<name>_tick(...)` that loops on an
interval and wire it into `server::run` — this is the rewrite's equivalent of
adding a global event.

## 2. Per-script waits (the `wait` command)

DG scripts can pause with `wait <N> sec`. Rather than a queued event, the
interpreter runs a script in chunks: `execute_script_chunk` returns either
`Done` or `Paused { outputs, wait_secs, resume }`, capturing the interpreter
state (program counter, variables, frames). The sync wrapper emits the first
chunk's output immediately, then `tokio::spawn`s a continuation that loops
`sleep(wait_secs)` → resume → apply, until the script finishes. This is the
rewrite's analogue of a delayed `NEW_EVENT` carrying script state.

## 3. DG Script triggers

The rewrite parses `.trg` files and attaches triggers to mobs, objects, and
rooms via the zone `T` reset command. Supported trigger types include:

- **Mob:** GREET (entry), SPEECH, DEATH, FIGHT, RECEIVE, ENTRY, BRIBE, RANDOM
  (periodic), LOAD
- **Object:** GET, TIMER, LOAD, RECEIVE
- **Room:** GREET/ENTRY, SPEECH, LEAVE, RANDOM

Inside a script, the rewrite supports variables, `if/else/end`, `while` loops,
arithmetic `eval`, and side-effecting commands (`mecho`/`memote`, `mload`,
`mpurge`, `mgoto`/`mteleport`, `mdamage`, `mforce`, `mat`, `give`/BRIBE, etc.).

## Differences from stock, in brief

- No `event_id` enum, `mud_event_list[]`, or per-character event list.
- No `EVENTFUNC` callbacks; recurring work is a Tokio interval task, one-shot
  delayed work is a spawned task that sleeps then runs.
- Concurrency safety comes from `Arc<Mutex<...>>` around shared state plus the
  single-threaded-per-await discipline, not from a single-threaded event loop.
