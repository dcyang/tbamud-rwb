# Sending Messages to Players in tbamud-rwb

Adapted from the stock TbaMUD "act() Function" document. The C server routes
almost all output through one function, `act()`, which parses `$n`/`$N`/`$p`…
control characters and fans a single template out to everyone in a room. The
Rust rewrite does not have a single `act()`; it builds finished strings in Rust
and sends them through two paths. This document maps the stock concepts onto
what the rewrite actually does, so the examples in the C docs translate.

## The two output paths

**1. To the acting player** — a command handler returns a `CmdOutput`
(`src/interpreter.rs`). Its `text` field is the string sent back to the player
who typed the command; its `quit` flag signals logout.

```rust
CmdOutput::text("\r\nYou smile happily.\r\n")
```

**2. To others** — `CharacterList::broadcast_room` (`src/character.rs`) sends a
line to everyone in a room, optionally excluding one player (normally the
actor):

```rust
chars.lock().await.broadcast_room(
    me.current_room,
    Some(me.id),                         // exclude the actor
    &format!("{} smiles happily.\r\n", me.name));
```

Each online player has an mpsc sender; broadcast pushes the line onto every
recipient's channel, and that connection's writer task applies color conversion
(see [color.md](color.md)) before writing to the socket.

So the stock pattern of "TO_CHAR string" + "TO_ROOM string" becomes: return the
TO_CHAR text as the `CmdOutput`, and broadcast the TO_ROOM text with the actor
excluded. A TO_VICT message is sent by looking up the victim's `PlayerHandle`
and pushing directly to its sender (`ph.send.send(...)`).

## Control characters (`$n`, `$N`, …)

The rewrite does **not** interpret `$`-control characters. Instead, the name,
pronoun, or object short-description is substituted directly in Rust with
`format!`, because the code already holds the relevant values:

| Stock | Rewrite equivalent |
|-------|--------------------|
| `$n` (actor) | `me.name` (or the mob's `short_descr`) |
| `$N` (victim) | the victim's name / `short_descr`, looked up explicitly |
| `$p` (object) | the object's `short_description` from its prototype |
| `$m`/`$s`/`$e` (pronoun) | chosen in Rust from the character's sex when needed |
| `$$` (literal `$`) | just write `$` |

Visibility ("someone" / "something") is handled at the point each message is
built or rendered (e.g. hidden/invisible players are filtered out of room
listings in `render_room`), rather than by a `hide_invisible` flag threaded
through one universal function.

## Why the difference

`act()` exists in C largely to avoid building the same sentence several times by
hand and to centralize pronoun/visibility logic. In the rewrite, per-recipient
differences are usually small, room broadcasts are one call, and `format!` plus
explicit lookups are clear and type-checked — so the indirection isn't needed.
When you are porting a snippet from the C source, read its `act(...)` calls as:
"build this sentence with the real names and send the TO_CHAR part back as
`CmdOutput`, the TO_ROOM part via `broadcast_room` (excluding the actor), and
any TO_VICT part to that player's sender."

## Examples from the stock doc, translated

```rust
// act("$n smiles happily.", TRUE, ch, 0, 0, TO_ROOM);
chars.broadcast_room(room, Some(me.id),
    &format!("{} smiles happily.\r\n", me.name));

// act("You kiss $M.", FALSE, ch, 0, vict, TO_CHAR);
CmdOutput::text(format!("\r\nYou kiss {}.\r\n", vict_name));

// act("$n gives you $p.", FALSE, ch, obj1, vict, TO_VICT);
let _ = vict.send.send(format!("{} gives you {}.\r\n", me.name, obj_short));
```

Numerous concrete examples live throughout `src/interpreter.rs` and
`src/combat.rs`.
