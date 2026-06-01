# Combat / Skill Messages in tbamud-rwb

Adapted from the stock TbaMUD "Message Editor" document.

Stock TbaMUD has an in-game OLC editor, `msgedit`, for the combat/skill damage
messages stored in `lib/misc/messages`. The Rust rewrite has a full OLC editor
set (see [building.md](building.md)), but **not** `msgedit` — because the
rewrite doesn't load `lib/misc/messages` at all (combat messages are inline in
the Rust source), there is no message data for such an editor to edit.

It also does **not** load `lib/misc/messages`. The file is still present in
`lib/misc/` (it ships with the data set), but the rewrite does not read it.
Instead, the hit/miss/death lines for attacks, spells, and skills are produced
directly in Rust at the point of resolution:

- **Melee** and the to-hit/miss/damage lines: `src/combat.rs`
- **Per-spell** hit / save / miss / kill flavor: the `cast_*` handlers in
  `src/interpreter.rs`
- **Per-skill** success / fumble lines: the skill handlers in
  `src/interpreter.rs`

For example, each offensive spell handler (`cast_magic_missile`,
`cast_lightning_bolt`, `cast_fireball`, …) builds its own "to the caster" and
"to the room" strings, including the partial-resist variants used when a target
saves. The mob-death broadcast and corpse handling live on the shared kill path
in `src/combat.rs`.

## Changing a combat message

Because the messages are inline, you change them by editing the relevant string
literal in the Rust source and rebuilding:

1. Find the handler (e.g. `cast_fireball` in `src/interpreter.rs`, or
   `resolve_player_attack` / `resolve_mob_attack` in `src/combat.rs`).
2. Edit the `format!(...)` strings. You may use `@x` color codes
   (see [color.md](color.md)).
3. `cargo build`.

If full data-driven combat messages (loading `lib/misc/messages` and selecting a
random entry per skill, as stock does) are added later, this document should be
updated to describe that format; for now the messages are code, not data.
