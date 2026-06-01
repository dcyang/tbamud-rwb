# tbamud-rwb Builder's Manual

Adapted from "The CircleMUD Builder's Manual". The stock manual is largely a
guide to the in-game OLC editors (`redit`/`medit`/`oedit`/`zedit`/`sedit`/
`trigedit`/`qedit`).

The Rust rewrite has **NO OLC**. There are no in-game world editors. You build
the world by editing the plain-text data files under `lib/world/` by hand and
restarting the server (or by validating with `-c`). The file *formats* are
unchanged from stock TbaMUD — the rewrite reads the same
`.wld`/`.mob`/`.obj`/`.zon`/`.shp`/`.qst`/`.trg` files — so existing stock areas
load as-is, and the canonical way to learn a format is to open an existing file
in the same `lib/world/<type>/` subtree and copy its shape exactly.

## Golden rules

- These files are line-oriented and order-sensitive. Preserve formatting.
- Record numbers ("vnums") are global; do not collide with existing ones.
- After editing, run `cargo run -- -c` to load + validate without booting.
  Zone/room/object/mob parse errors abort the boot and name the file;
  trigger/quest/shop errors are warned and that file is skipped.
- Do **not** renumber The Void (room 0) or Limbo (room 1) — they are hardcoded
  load/death/link-dead destinations.

## Directory layout

| Path | Contents |
|------|----------|
| `lib/world/wld/` | rooms (`*.wld`) |
| `lib/world/mob/` | mobiles (`*.mob`) |
| `lib/world/obj/` | objects (`*.obj`) |
| `lib/world/zon/` | zones / resets (`*.zon`) |
| `lib/world/shp/` | shops (`*.shp`) |
| `lib/world/trg/` | DG triggers (`*.trg`) |
| `lib/world/qst/` | quests (`*.qst`) |

Each subtree has an `index` (files loaded at boot) and `index.mini` (the smaller
set loaded with `-m`). The loader reads each subtree's index independently.

## Rooms (`.wld`)

```
#<vnum>
<name>~
<description, possibly multiple lines>
~
<zone#> <flags> <sector> <flags2> <flags3> <flag_count>
D<dir>            (0=N 1=E 2=S 3=W 4=U 5=D)
<exit description>~
<exit keywords>~
<door_flag> <key_vnum> <to_room>
E                 (extra description; keyword~ then text~)
<keywords>~
<text>~
S                 (end of this room)
...
$~                (end of file, after the last room)
```

Sectors and room/exit flag bits match `structs.h`; the rewrite recognizes the
common ones (INSIDE/CITY/FIELD/FOREST/HILLS/MOUNTAIN/WATER\*/UNDERWATER/FLYING
sectors; DARK/DEATH/NOMOB/INDOORS/PEACEFUL/SOUNDPROOF/NOTRACK/NOMAGIC/TUNNEL/
PRIVATE/GODROOM/HOUSE/etc. room flags; ISDOOR/CLOSED/LOCKED/PICKPROOF/HIDDEN exit
flags). An exit whose `to_room` doesn't exist is treated as impassable.

## Mobiles (`.mob`)

CircleMUD "simple" (`S`) and "enhanced" (`E`) mob formats are both read: the
keyword and description block, the action/long descriptions, then the numeric
lines (level, hitroll, AC, HP dice `xdy+z`, damage dice, gold, exp), the flag
lines (`MOB_*` action flags, `AFF_*` affect flags, alignment), and position/sex.
Mob spec procs are assigned by vnum in code (`world.rs::MobSpec::for_vnum`), not
in the data file.

## Objects (`.obj`)

```
#<vnum>
<keywords>~
<short description>~
<long description (on the ground)>~
<action/read description>~
<type> <extra_flags...> <wear_flags...>
<value0> <value1> <value2> <value3>
<weight> <cost> <rent_per_day> <level> <timer>
E ...   (extra descriptions)
A <location> <modifier>   (apply: stat bonuses when worn)
$
```

Item types and the meaning of the four `value` fields follow stock (weapons:
`value1`/`value2` = damage dice; armor: `value0` = AC; containers, drink
containers, lights, wands/staves, etc.). `rent_per_day` feeds the rent system;
`ITEM_NORENT` and `ITEM_KEY` make an item unrentable (see [admin.md](admin.md)).

## Zones / resets (`.zon`)

The zone header gives number, name, builders, bottom/top room vnums, lifespan,
and reset mode. Reset commands (one per line) populate the zone on boot and on
each repop:

| Cmd | Effect |
|-----|--------|
| `M` | load mobile into a room |
| `O` | load object into a room |
| `G` | give object to the last mob |
| `E` | equip last mob (arg3 = wear slot) |
| `P` | put object into another object |
| `D` | set a door's state |
| `R` | remove object from a room |
| `T` | attach a trigger |
| `V` | set a DG variable |

A trailing comment after a tab is informational. To make a mob actually
wield/wear what it's given, the rewrite auto-equips appropriate `G`-loaded items
after the reset, in addition to honoring explicit `E` commands.

## Shops (`.shp`), Quests (`.qst`), Triggers (`.trg`)

These use their stock record formats and are read directly. Triggers are DG
Scripts; see [dg_events.md](dg_events.md) for which trigger types and script
commands the rewrite supports. Shops drive the `list`/`buy`/`sell`/`value`/
`appraise` commands and the repair/receptionist interactions.

## Adding a new zone (vnum N)

1. Create `N.wld`, `N.mob`, `N.obj`, `N.zon`, `N.shp`, `N.trg`, `N.qst` — even if
   some are just a stub containing `$~` (or `$` where that's the terminator).
2. Append `N.<ext>` to the `index` in **each** of the seven `lib/world/<type>/`
   directories (and `index.mini` if you want it in mini-MUD boots).
3. `cargo run -- -c` to validate, then boot.

## In-game help while building

Although there's no OLC, immortal commands help you inspect and prototype:

| Command | Use |
|---------|-----|
| `goto <vnum>` | jump to a room |
| `stat <thing\|vnum>` | inspect a room/mob/object (live or by prototype vnum) |
| `rlist`/`mlist`/`olist`/`zlist` | list rooms/mobs/objects/zones in a vnum range |
| `load mob\|obj <vnum>` | spawn a prototype to test it |
| `purge` | clear mobs/objects from your room |
| `zreset <zone>` | re-run a zone's resets now |
| `dig <dir> <vnum>` | carve a two-way exit (and a stub room) at runtime |
| `oset <vnum> <field> <val>` | tweak an object prototype live |

Runtime changes made with `dig`/`oset` are not written back to the
`.wld`/`.obj` files — they're for testing. Persist changes by editing the data
files.

Game balance and debugging are as in stock: keep levels in 1–34, use realistic
weights/costs, and read [syserr.md](syserr.md) for the common data mistakes.
