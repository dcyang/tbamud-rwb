# Socials in tbamud-rwb

Adapted from the stock TbaMUD socials document.

Socials are canned emotes (smile, wave, comfort, …) with no effect on gameplay.
The rewrite loads them at boot and exposes them as commands; a verb that isn't a
built-in command falls through to a socials lookup, so any social name Just
Works.

## What the rewrite loads

The rewrite reads `lib/misc/socials.new` — the newer "AEdit" format — **not**
the legacy 14-line `lib/misc/socials` file. About 491 socials load from the
stock data set (`db.rs::parse_socials_new` → `World.socials`). There is no
in-game `aedit`/`astat` editor (the rewrite has no OLC); edit `socials.new` by
hand and restart, or boot with the data already in place.

## `socials.new` record format

Each record is a header line beginning with `~`, followed by up to five message
lines, then trailing `#` placeholders / a blank line before the next record. The
file ends at a line containing only `$`.

```
Header:  ~<name> <cmd> <hide> <min_position> <target_required> <action#>
Line 1:  actor, no argument        (what the actor sees)
Line 2:  room peers, no argument
Line 3:  actor, with a target      ($N -> target)
Line 4:  room peers, with a target ($n -> actor, $N -> target)
Line 5:  the target sees           ($n -> actor)
```

A `#` in any message slot means "say nothing" for that case. These five slots
map directly to the rewrite's `Social` struct (`world.rs`):

```
actor_no_arg, room_no_arg, actor_target, room_target, victim_target
+ name, min_position, target_required
```

`hide` (0/1) and `min_position` are parsed and stored; `min_position` uses the
same scale as stock (Dead 0 … Resting 5 … Standing 8).

## Substitution codes

The rewrite substitutes exactly two codes when it sends a social
(`interpreter.rs::do_social`):

- `$n` — the actor's name
- `$N` — the target's name

> **Limitation:** the pronoun and object codes from stock socials (`$e`/`$E`,
> `$m`/`$M`, `$s`/`$S` pronouns; `$t` body part; `$p` object) are **not** yet
> substituted — they pass through literally. In practice most socials read fine
> because the common templates use `$n`/`$N`; lines that lean on pronouns will
> show the raw code until this is extended. If you author new socials, prefer
> `$n`/`$N`.

## Behavior

- `social` (no argument) → actor sees slot 1, room sees slot 2.
- `social <target>` → actor sees slot 3, room peers (excluding actor and target)
  see slot 4, the target sees slot 5.

If a slot is empty the rewrite falls back sensibly (e.g. a missing
actor-with-target line yields "You \<social\> at \<target\>."). A social typed
with a target who isn't present returns "No one named 'X' is here."

Related commands: `socials` lists every loaded social; `emote <text>` is the
free-form version.
