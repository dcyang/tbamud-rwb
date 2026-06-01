# tbamud-rwb Coder's Manual

Adapted from "The CircleMUD Coder's Manual". The stock manual assumes solid C
knowledge and walks through adding commands, spells, skills, socials, and classes
in C. This file does the same for the Rust rewrite. It assumes you can read Rust;
it is not a Rust tutorial.

## Source layout

See [files.md](files.md) for the full module list. The pieces you touch most:

| Module | Role |
|--------|------|
| `src/interpreter.rs` | command table, dispatch, and command/spell handlers. |
| `src/combat.rs` | the combat tick and damage/death resolution. |
| `src/character.rs` | `Character`/`PlayerHandle`/`CharacterList`; the `Skill` enum. |
| `src/world.rs` | Room/Mob/Obj types, `World`, constants, `MobSpec`. |
| `src/db.rs` | world loading + the background ticks. |
| `src/players.rs` | player files + object persistence. |

Shared state is `Arc<Mutex<World>>` and `Arc<Mutex<CharacterList>>`. Command
handlers are `async fn`s that take `&mut Character` (the actor) plus whatever
shared handles they need, and return a `CmdOutput { text, quit }`.

## Build/verify loop (do this after every change)

```sh
cargo build 2>&1 | grep -E "^error"   # hard errors only
cargo test                             # unit suite (testing.md)
cargo run -- -p 4xxx                   # smoke-boot on a 4000-range port
```

## Adding a command

1. Register the verb in the `COMMANDS` table (`interpreter.rs`) so abbreviation
   matching finds it. Order matters for prefix matching (shorter/earlier wins).

2. Add a dispatch arm in the big `match canon { ... }`:

   ```rust
   Some("grin") => do_grin(me, chars).await,
   ```

3. Write the handler:

   ```rust
   async fn do_grin(me: &Character, chars: &SharedChars) -> CmdOutput {
       chars.lock().await.broadcast_room(me.current_room, Some(me.id),
           &format!("{} grins evilly.\r\n", me.name));
       CmdOutput::text("\r\nYou grin evilly.\r\n".to_string())
   }
   ```

   Return the actor's line as the `CmdOutput`; broadcast the room's line with the
   actor excluded (see [act.md](act.md)). Set `CmdOutput::quit(...)` to log the
   player off through the standard save+disconnect path.

If the command is a pure emote with no logic, prefer adding it to
`lib/misc/socials.new` instead of writing code (see [socials.md](socials.md)).

## Adding a spell or skill

Skills and spells share the `Skill` enum in `character.rs`. Registering one is
the error-prone part: it must appear in **seven** places, or the build fails (the
match arms are exhaustive) or the skill silently won't work:

1. the `Skill` enum variant,
2. `parse()` — string → `Skill` (the name players type),
3. `name()` — `Skill` → display name,
4. `kind()` — Physical vs Spell grouping,
5. `mana_cost()`,
6. `allowed_classes()`,
7. `save_key()` — the token used in the player file,

…and add it to the `ALL_SKILLS` array (for `skills`/`practice`/seeding).

> **Tip (learned the hard way):** do these arms with individual edits anchored on
> an adjacent existing arm, then `cargo build` immediately. Bulk regex inserts
> tend to half-apply and produce duplicate / unreachable match arms.

Then wire behavior:

- **Spell:** add a dispatch arm in `do_cast` mapping the `Skill` to a `cast_*`
  handler, and write `async fn cast_foo(...)`. Damage spells follow the
  `cast_magic_missile` / `cast_lightning_bolt` template (resolve target, roll
  to-hit, `save_vs_spell` for half, then the shared kill path: DEATH triggers,
  corpse, XP, level-up, quest hooks). Buffs go through `cast_buff`, debuffs
  through `cast_debuff`.
- **Skill:** add a dispatch arm calling `do_skill(...)` (for the kick/bash
  family) or a dedicated handler. Gate on `is_class_allowed` and a practiced
  level; use `learn_attempt` to grant learn-on-use bumps.

## Adding combat / spell messages

Messages are inline string literals in the relevant handler (see
[msgedit.md](msgedit.md)); edit them there. Use `@x` color codes (see
[color.md](color.md)).

## Adding periodic behavior

Write a `db::spawn_<name>_tick(...)` that loops on a `tokio::time::interval`,
locks the shared state, does its work, and is spawned from `server::run`. This is
the rewrite's equivalent of a recurring event (see [dg_events.md](dg_events.md)).

## Adding a class

Classes live as the `Class` enum in `players.rs`, with class-dependent logic in
`character.rs` (HP/mana per level, titles, allowed skills via
`Skill::allowed_classes`) and the login class-selection menu. Adding one means
extending the enum + each `match` over it (the compiler will list them all).

## Conventions

- Match the surrounding code's style, naming, and comment density.
- Hold locks briefly; never hold a `World`/`CharacterList` lock across an
  `.await` on another lock you also need — snapshot what you need, drop the
  guard, then act. Several helpers use the pattern
  `let h = cl.iter().find(...).cloned(); h` to drop the registry guard before
  locking a `Character` (avoids borrow-lifetime errors and deadlocks).
- `thread_rng()` is `!Send`; don't hold an RNG across an `.await`. Re-acquire it
  per use.
- Prefer returning errors / sensible defaults over panicking in request paths.
- Run `cargo clippy` and `cargo fmt` before sending changes.

When porting a snippet from the stock C source, read its `act()` calls per
[act.md](act.md), its `GET_x(ch)` macros as `Character` fields, and its
`SPECIAL`/event hooks per [dg_events.md](dg_events.md).
