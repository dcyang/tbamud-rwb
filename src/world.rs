/// World data: rooms, zones, exits.
/// Mirrors the room_data / zone_data / room_direction_data structs in structs.h
/// and the world[] / zone_table[] globals declared in db.c.

use std::collections::BTreeMap;

pub type RoomVnum = i32;
pub type ZoneVnum = i32;
pub type ObjVnum  = i32;
pub type MobVnum  = i32;

/// NOWHERE sentinel — mirrors the NOWHERE define in structs.h
pub const NOWHERE: RoomVnum = -1;

/// Default mortal start room — mirrors mortal_start_room in config.c
pub const MORTAL_START: RoomVnum = 3001;
/// Default immortal start room
pub const IMMORT_START: RoomVnum = 1204;
/// Default frozen start room
pub const FROZEN_START: RoomVnum = 1202;

/// The Void — hardcoded link-dead / death destination
pub const VOID_ROOM: RoomVnum = 0;

/// Compass directions — mirrors the DIR_* constants in structs.h
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    North = 0,
    East  = 1,
    South = 2,
    West  = 3,
    Up    = 4,
    Down  = 5,
}

impl Direction {
    pub const ALL: [Direction; 6] = [
        Direction::North, Direction::East, Direction::South,
        Direction::West,  Direction::Up,   Direction::Down,
    ];

    pub fn from_index(i: u8) -> Option<Direction> {
        match i {
            0 => Some(Direction::North), 1 => Some(Direction::East),
            2 => Some(Direction::South), 3 => Some(Direction::West),
            4 => Some(Direction::Up),    5 => Some(Direction::Down),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Direction::North => "north", Direction::East => "east",
            Direction::South => "south", Direction::West => "west",
            Direction::Up    => "up",    Direction::Down => "down",
        }
    }

    /// Compass-reverse — n↔s, e↔w, u↔d.  Used by door commands to
    /// mirror state changes on the other side of a door.
    pub fn opposite(self) -> Direction {
        match self {
            Direction::North => Direction::South,
            Direction::South => Direction::North,
            Direction::East  => Direction::West,
            Direction::West  => Direction::East,
            Direction::Up    => Direction::Down,
            Direction::Down  => Direction::Up,
        }
    }

    /// Parse a player-typed direction (e.g. "n", "north", "u")
    pub fn parse(s: &str) -> Option<Direction> {
        match s.to_ascii_lowercase().as_str() {
            "n" | "north" => Some(Direction::North),
            "e" | "east"  => Some(Direction::East),
            "s" | "south" => Some(Direction::South),
            "w" | "west"  => Some(Direction::West),
            "u" | "up"    => Some(Direction::Up),
            "d" | "down"  => Some(Direction::Down),
            _ => None,
        }
    }
}

/// A single exit from a room. Mirrors room_direction_data in structs.h.
#[derive(Debug, Clone, Default)]
pub struct Exit {
    pub description: String,    // general_description in C
    pub keyword:     String,
    pub exit_info:   u32,       // door flags: EX_ISDOOR etc.
    pub key:         i32,       // key vnum or NOTHING (-1)
    pub to_room:     RoomVnum,  // destination vnum (NOWHERE if blocked)
}

/// ROOM_* bit positions inside `Room.room_flags[0]`.  Mirror the
/// same-named macros in structs.h. Only the ones we currently honor are
/// listed.
pub const ROOM_DEATH:      u32 = 1 << 1;
pub const ROOM_NOMOB:      u32 = 1 << 2;
pub const ROOM_DARK:       u32 = 1 << 3;
pub const ROOM_PEACEFUL:   u32 = 1 << 4;
pub const ROOM_SOUNDPROOF: u32 = 1 << 5;
pub const ROOM_NOMAGIC:    u32 = 1 << 7;
pub const ROOM_TUNNEL:     u32 = 1 << 8;
pub const ROOM_PRIVATE:    u32 = 1 << 9;
pub const ROOM_NOTRACK:    u32 = 1 << 6;
pub const ROOM_GODROOM:    u32 = 1 << 10;
/// Marks the room as a persistent house — its `objects` list is saved
/// to `<data_dir>/house/<vnum>.house` on a periodic tick and restored
/// at boot.  Toggled at runtime by the immortal `househere` command.
pub const ROOM_HOUSE:      u32 = 1 << 11;

/// Sector type codes (matches structs.h SECT_* macros).  Used by
/// `Room.sector_type` and `sector_move_cost`.
pub const SECT_INSIDE:        i32 = 0;
pub const SECT_CITY:          i32 = 1;
pub const SECT_FIELD:         i32 = 2;
pub const SECT_FOREST:        i32 = 3;
pub const SECT_HILLS:         i32 = 4;
pub const SECT_MOUNTAIN:      i32 = 5;
pub const SECT_WATER_SWIM:    i32 = 6;
pub const SECT_WATER_NOSWIM:  i32 = 7;
pub const SECT_UNDERWATER:    i32 = 8;
pub const SECT_FLYING:        i32 = 9;

/// Movement-point cost to leave a room of the given sector type.
/// Tracks tbaMUD's `movement_loss[]` table in constants.c.  `do_move`
/// pays the average of (cost of from-sector) + (cost of to-sector),
/// rounded up.
pub fn sector_move_cost(sector: i32) -> i32 {
    match sector {
        SECT_INSIDE   | SECT_CITY    => 1,
        SECT_FIELD    | SECT_FOREST  => 2,
        SECT_HILLS                   => 3,
        SECT_MOUNTAIN                => 4,
        SECT_WATER_SWIM
        | SECT_WATER_NOSWIM
        | SECT_UNDERWATER            => 4,
        SECT_FLYING                  => 1,
        _                            => 2,
    }
}

/// EX_* bits inside `Exit.exit_info`.  Mirror the same-named macros in
/// structs.h.  Only the ones we currently honor are listed.
pub const EX_ISDOOR:    u32 = 1 << 0;
pub const EX_CLOSED:    u32 = 1 << 1;
pub const EX_LOCKED:    u32 = 1 << 2;
pub const EX_PICKPROOF: u32 = 1 << 3;
pub const EX_HIDDEN:    u32 = 1 << 4;

/// Extra description (E ... ~ ... ~). Mirrors extra_descr_data.
#[derive(Debug, Clone, Default)]
pub struct ExtraDescr {
    pub keyword:     String,
    pub description: String,
}

/// A single room. Mirrors room_data in structs.h (minimal subset).
/// `mobs` and `objects` hold *instance ids* — pointers into World.mob_instances
/// / World.obj_instances. They are populated by reset_zone(), not by the file
/// parser.
#[derive(Debug, Clone, Default)]
pub struct Room {
    pub vnum:        RoomVnum,
    pub zone:        ZoneVnum,
    pub name:        String,
    pub description: String,
    pub sector_type: i32,
    pub room_flags:  [u32; 4],
    pub exits:       [Option<Exit>; 6],
    pub extras:      Vec<ExtraDescr>,
    pub mobs:        Vec<u32>,    // mob instance ids
    pub objects:     Vec<u32>,    // object instance ids
    pub triggers:    Vec<TriggerVnum>, // attached DG triggers
}

/// A single zone reset command. Mirrors `reset_com` in structs.h.
/// `if_flag`: 0 = always execute, 1 = only if previous command succeeded.
/// Operands are interpreted per command kind:
///   M arg1=mob_vnum  arg2=max  arg3=room_vnum
///   O arg1=obj_vnum  arg2=max  arg3=room_vnum (-1 = nowhere)
///   G arg1=obj_vnum  arg2=max  arg3=-1         (give to last mob)
///   E arg1=obj_vnum  arg2=max  arg3=wear_pos
///   P arg1=obj_vnum  arg2=max  arg3=container_vnum
///   D arg1=room_vnum arg2=dir  arg3=state (0=open, 1=closed, 2=locked)
///   R arg1=room_vnum arg2=obj_vnum
#[derive(Debug, Clone, Default)]
pub struct ResetCmd {
    pub command: char,
    pub if_flag: i32,
    pub arg1:    i32,
    pub arg2:    i32,
    pub arg3:    i32,
}

/// A single zone. Mirrors zone_data in structs.h (minimal subset).
#[derive(Debug, Clone, Default)]
pub struct Zone {
    pub number:    ZoneVnum,
    pub builders:  String,
    pub name:      String,
    pub bot:       RoomVnum,
    pub top:       RoomVnum,
    pub lifespan:  i32,
    pub reset_mode: i32,
    pub zone_flags: [u32; 4],
    pub min_level: i32,
    pub max_level: i32,
    pub commands:  Vec<ResetCmd>,
}

// ---------------------------------------------------------------------------
// Object prototypes & instances
// ---------------------------------------------------------------------------

/// Object prototype — mirrors obj_data + obj_flag_data (minimal subset).
/// We split prototypes (read once from disk) from instances (which can be
/// loaded into the world) the same way C does with obj_proto[] vs obj_list.
#[derive(Debug, Clone, Default)]
pub struct ObjProto {
    pub vnum:              ObjVnum,
    pub name:              String,      // keywords (e.g. "wings")
    pub short_description: String,      // "a pair of wings"
    pub description:       String,      // long: shown on the ground
    pub action_description: String,     // action / read text
    pub item_type:         i32,
    pub extra_flags:       [u32; 4],
    pub wear_flags:        [u32; 4],
    pub affect_flags:      [u32; 4],
    pub value:             [i32; 4],
    pub weight:            i32,
    pub cost:              i32,
    pub rent:              i32,
    pub level:             i32,
    pub timer:             i32,
    pub extras:            Vec<ExtraDescr>,
    /// Apply-stat modifiers, parsed from `A` records.  Up to 6 in stock
    /// CircleMUD; we store them as a Vec for simplicity.
    pub affected:          Vec<ObjAffect>,
}

/// A live object instance in the world.
#[derive(Debug, Clone)]
pub struct ObjInstance {
    pub id:    u32,
    pub vnum:  ObjVnum,
    pub in_room: RoomVnum,   // NOWHERE if not in a room (carried/in container/equipped)
    /// Instance ids of objects this container holds.  Always empty for
    /// non-container item types.
    pub contents: Vec<u32>,
    /// If Some, this instance is a *corpse* — a synthetic container that
    /// has no prototype.  The string is the mob's short_descr (e.g.
    /// "the gelatinous blob"), used in rendering and keyword matching.
    pub corpse_of: Option<String>,
    /// Seconds remaining until the object decays.  Currently used only by
    /// corpses; regular objects have None.
    pub decay_in: Option<i32>,
    /// DG trigger vnums attached to this object.  Populated by the T zone
    /// reset command with attach_type=1 (OBJ).
    pub triggers: Vec<TriggerVnum>,
    /// Per-instance timer (seconds) for non-corpse objects with a
    /// prototype `timer` > 0 — counted down by the obj-timer tick. When
    /// it reaches zero, OTRIG_TIMER ('f' OBJ trigger) fires before the
    /// object is extracted.
    pub timer: Option<i32>,
    /// For ITEM_LIGHT only: whether the light source is currently lit.
    /// `false` for everything else; toggled by `light`/`extinguish`.
    pub light_lit: bool,
    /// Item durability, 0..=100.  100 = pristine, 0 = broken.  Items
    /// at 0 are extracted by the combat path that landed the final
    /// hit (player or mob).
    pub condition: i32,
    /// CircleMUD spell vnum stored on a player-brewed potion.  `do_quaff`
    /// applies this single spell instead of the proto's value[1..3]
    /// when set.  Cleared on consumption (the obj is extracted anyway).
    pub brewed_spell: Option<i32>,
    /// Per-instance stat affects (cp177) layered on top of the proto's
    /// `affected` list.  Examples: `enchant weapon` pushes +1 hitroll
    /// here.  Persisted alongside condition; capped at a small total
    /// to prevent stacking.
    pub bonus_affects: Vec<ObjAffect>,
    /// For ITEM_LIGHT only: game-hours of fuel remaining while lit.
    /// `0`  = fresh/uninitialised (seeded from proto `value[2]` when lit);
    /// `>0` = burning, hours left;
    /// `-1` = burned out (cannot be relit).
    /// Lights whose proto `value[2] <= 0` are treated as infinite and never
    /// burn down.  Decremented by `db::spawn_light_burn_tick` (cp207).
    pub light_hours: i32,
}

/// Reserved vnum used for corpses (and other synthetic objects that have
/// no prototype). Always checked alongside `corpse_of`.
pub const CORPSE_VNUM: ObjVnum = -1;

/// Seconds before a corpse decays and is removed from the world (5 min).
pub const CORPSE_DECAY_SECS: i32 = 300;
/// Player corpses linger far longer so the owner has a chance to
/// `recover` after a respawn trip.
pub const PC_CORPSE_DECAY_SECS: i32 = 1800;

/// ITEM_* item-type constants (mirror structs.h).  Used by parsers and
/// gameplay (containers, weapons, armor).
/// APPLY_* (object affect locations) — mirror constants.c.  Listed here
/// are the slots `apply_obj_affects()` currently honors; unknown values
/// are tolerated and ignored at apply-time.
pub const APPLY_NONE:    i32 = 0;
pub const APPLY_STR:     i32 = 1;
pub const APPLY_DEX:     i32 = 2;
pub const APPLY_INT:     i32 = 3;
pub const APPLY_WIS:     i32 = 4;
pub const APPLY_CON:     i32 = 5;
pub const APPLY_CHA:     i32 = 6;
pub const APPLY_MANA:    i32 = 12;
pub const APPLY_HIT:     i32 = 13;
pub const APPLY_AC:      i32 = 17;
pub const APPLY_HITROLL: i32 = 18;
pub const APPLY_DAMROLL: i32 = 19;

/// One (location, modifier) entry from an object's `A` record.
#[derive(Debug, Clone, Copy, Default)]
pub struct ObjAffect {
    pub location: i32,
    pub modifier: i32,
}

pub const ITEM_LIGHT:     i32 = 1;
pub const ITEM_SCROLL:    i32 = 2;
pub const ITEM_WAND:      i32 = 3;
pub const ITEM_STAFF:     i32 = 4;
pub const ITEM_WEAPON:    i32 = 5;
pub const ITEM_ARMOR:     i32 = 9;
pub const ITEM_POTION:    i32 = 10;
pub const ITEM_CONTAINER: i32 = 15;
pub const ITEM_DRINKCON:  i32 = 17;
pub const ITEM_FOOD:      i32 = 19;
pub const ITEM_FOUNTAIN:  i32 = 23;
pub const ITEM_BOAT:      i32 = 22;

/// Bits inside `ObjProto.extra_flags[0]`.  Mirrors structs.h
/// ITEM_x_* macros.  Only the ANTI-class checks are wired right now;
/// the rest are placeholders for future use.
pub const ITEM_ANTI_GOOD:       u32 = 1 << 9;
pub const ITEM_ANTI_EVIL:       u32 = 1 << 10;
pub const ITEM_ANTI_NEUTRAL:    u32 = 1 << 11;
pub const ITEM_ANTI_MAGIC_USER: u32 = 1 << 12;
pub const ITEM_ANTI_CLERIC:     u32 = 1 << 13;
pub const ITEM_ANTI_THIEF:      u32 = 1 << 14;
pub const ITEM_ANTI_WARRIOR:    u32 = 1 << 15;
/// Two-handed weapon (CircleMUD ITEM_2H_WEAPON).  Requires both
/// hands — wielder may not also use WEAR_SHIELD or WEAR_HOLD.
pub const ITEM_2H_WEAPON:       u32 = 1 << 16;

/// MOB_* bitflag positions in `MobProto.mob_flags[0]`.  Mirrors the
/// MOB_* defines in structs.h.
pub const MOB_SPEC:       u32 = 1 << 0;
pub const MOB_SENTINEL:   u32 = 1 << 1;
pub const MOB_SCAVENGER:  u32 = 1 << 2;
pub const MOB_ISNPC:      u32 = 1 << 3;
pub const MOB_AWARE:      u32 = 1 << 4;
pub const MOB_AGGRESSIVE: u32 = 1 << 5;
/// Aggressive only against good-aligned victims (`AlignmentBand::Good`).
pub const MOB_AGGR_GOOD:    u32 = 1 << 16;
pub const MOB_AGGR_EVIL:    u32 = 1 << 17;
pub const MOB_AGGR_NEUTRAL: u32 = 1 << 18;
pub const MOB_STAY_ZONE:  u32 = 1 << 6;
pub const MOB_WIMPY:      u32 = 1 << 7;
pub const MOB_MEMORY:     u32 = 1 << 11;
pub const MOB_HELPER:     u32 = 1 << 12;

// ---------------------------------------------------------------------------
// Mob prototypes & instances
// ---------------------------------------------------------------------------

/// Mob prototype — mirrors mob portion of char_data (minimal subset).
#[derive(Debug, Clone, Default)]
pub struct MobProto {
    pub vnum:        MobVnum,
    pub name:        String,    // keywords
    pub short_descr: String,    // "Puff"
    pub long_descr:  String,    // "Puff the Fractal Dragon is here..."
    pub description: String,    // look text
    pub mob_flags:   [u32; 4],
    pub aff_flags:   [u32; 4],
    pub alignment:   i32,
    pub level:       i32,
    pub hitroll:     i32,
    pub ac:          i32,
    /// HP dice: rolls hp_dice d hp_size + hp_add to set max HP.
    pub hp_dice:     i32,
    pub hp_size:     i32,
    pub hp_add:      i32,
    /// Damage dice: dam_dice d dam_size + dam_roll (barehand attack).
    pub dam_dice:    i32,
    pub dam_size:    i32,
    pub damroll:     i32,
    pub mana:        i32,
    pub mv:          i32,
    pub gold:        i32,
    pub exp:         i32,
    pub position:    i32,
    pub default_pos: i32,
    pub sex:         i32,
}

// ---------------------------------------------------------------------------
// DG triggers (minimal MVP — only GREET attached to mobs is interpreted)
// ---------------------------------------------------------------------------

pub type TriggerVnum = i32;

/// `attach_type`: where this trigger can be attached.
pub const TRIG_ATTACH_MOB:  i32 = 0;
pub const TRIG_ATTACH_OBJ:  i32 = 1;
pub const TRIG_ATTACH_ROOM: i32 = 2;

/// One trigger script from a `.trg` file.  Mirrors `trig_data` in
/// dg_scripts.h (minimal subset).  The `commands` field holds the raw
/// script lines, which the interpreter consumes one at a time.
#[derive(Debug, Clone, Default)]
pub struct Trigger {
    pub vnum:         TriggerVnum,
    pub name:         String,
    pub attach_type:  i32,    // 0 = mob, 1 = obj, 2 = room
    pub trigger_type: char,   // 'g' = GREET, 'd' = SPEECH, ... (currently only 'g' fires)
    pub narg:         i32,    // percent chance to fire (100 = always)
    pub arg:          String, // keywords / phrase the trigger matches on
    pub commands:     Vec<String>,
}

// ---------------------------------------------------------------------------
// Quests
// ---------------------------------------------------------------------------

pub type QuestVnum = i32;

/// Quest-type constants — mirror AQ_* in quest.h.
pub const AQ_OBJ_FIND:   i32 = 0;
pub const AQ_ROOM_FIND:  i32 = 1;
pub const AQ_MOB_FIND:   i32 = 2;
pub const AQ_MOB_KILL:   i32 = 3;
pub const AQ_MOB_SAVE:   i32 = 4;
pub const AQ_OBJ_RETURN: i32 = 5;
pub const AQ_ROOM_CLEAR: i32 = 6;

/// One quest entry from a .qst file.  Mirrors `aq_data` in quest.h.
#[derive(Debug, Clone, Default)]
pub struct Quest {
    pub vnum:        QuestVnum,
    pub name:        String,
    pub desc:        String,
    pub info:        String,
    pub done:        String,
    pub quit:        String,
    pub kind:        i32,         // AQ_* type
    pub flags:       u32,
    pub qm:          MobVnum,     // quest-master mob vnum
    pub target:      i32,         // mob vnum / room vnum / obj vnum depending on kind
    pub prev_quest:  QuestVnum,
    pub next_quest:  QuestVnum,
    pub prereq:      QuestVnum,
    pub value:       [i32; 7],
    pub gold_reward: i32,
    pub exp_reward:  i32,
    pub obj_reward:  ObjVnum,
}

// ---------------------------------------------------------------------------
// Shops
// ---------------------------------------------------------------------------

/// A shop — mirrors shop_data in shop.h (minimal subset). One shopkeeper
/// (mob_vnum) sells a list of object vnums and buys back items of given
/// item types.  Price multipliers come from the .shp file.
#[derive(Debug, Clone)]
pub struct Shop {
    pub vnum:           i32,
    pub keeper_vnum:    MobVnum,
    pub rooms:          Vec<RoomVnum>,
    pub sells:          Vec<ObjVnum>,
    pub buys_types:     Vec<i32>,
    pub profit_buy:     f32,    // multiplier when player buys (e.g. 1.15)
    pub profit_sell:    f32,    // multiplier when player sells (e.g. 0.15)
}

/// A live mob instance in the world.
#[derive(Debug, Clone, Default)]
pub struct MobInstance {
    pub id:    u32,
    pub vnum:  MobVnum,
    pub in_room: RoomVnum,
    /// Object instance ids carried/equipped by this mob.
    pub inventory: Vec<u32>,
    pub hp:        i32,
    pub max_hp:    i32,
    /// Opponent — same Target shape as `Character.fighting` to keep the
    /// combat tick uniform. The player id here is a PlayerHandle.id.
    pub fighting:  Option<crate::character::Target>,
    /// Player ids the mob remembers (used by MOB_MEMORY mobs).  Capped
    /// in practice by gameplay since most fights end with one corpse.
    pub remembers: Vec<u32>,
    /// DG trigger vnums attached to this mob (assigned via the T zone
    /// reset command).
    pub triggers:  Vec<TriggerVnum>,
    /// Active timed effects (Poison etc).  Ticked by the combat loop.
    pub affects:   Vec<crate::character::Affect>,
    /// Player id that currently has this mob charmed (paired with an
    /// active `Skill::CharmPerson` affect).  `None` for non-charmed
    /// mobs.  Allowed to be stale after the affect expires — the drag
    /// path re-checks the affect before using this.
    pub charmer:   Option<u32>,
    /// Mob spec_proc — special behavior assigned by vnum at spawn time.
    /// See `MobSpec::for_vnum`.
    pub spec:      Option<MobSpec>,
    /// Equipment slots, mirrors Character.equipment.  Populated by
    /// zone-reset 'E' commands when arg2 is a valid wear position.
    pub equipment: [Option<u32>; crate::character::NUM_WEARS],
    /// Cumulative bonus from worn/wielded items (APPLY_DAMROLL,
    /// APPLY_HITROLL, APPLY_AC).  Updated by `auto_equip_mob`.
    pub bonus_damroll: i32,
    pub bonus_hitroll: i32,
    pub bonus_ac:      i32,
}

/// Mob spec procs.  Hard-coded by vnum at spawn time (mirrors
/// CircleMUD's spec_assign.c table).  Ticked from a dedicated
/// background task in `db::spawn_mob_spec_tick`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobSpec {
    /// Idle utterer (the canonical "Puff" dragon at vnum 1).
    Puff,
    /// Eats corpses lying in its current room.
    Fido,
    /// Picks up small items lying in its current room.
    Janitor,
    /// Joins the fight against any mob attacking a player in its room.
    Cityguard,
    /// Poisonous bite — chance to apply Poison on melee hit.
    Snake,
    /// In combat, casts a tier-appropriate damage spell at higher
    /// cadence than the legacy keyword-matching `should_cast` heuristic.
    MagicUser,
    /// Service mob: players in the room can `heal` for gold.
    Healer,
    /// Service mob: pings same-room players who have mail waiting.
    Postmaster,
    /// Pet-shop keeper.  Other mobs in the same room are buyable; the
    /// `petbuy <kw>` command spawns a charmed copy as the buyer's
    /// follower.
    PetShop,
    /// Cutpurse: periodically lifts a slice of gold from a random
    /// non-immortal player sharing its room, then may slip away.
    Thief,
}

impl MobSpec {
    /// Hard-coded vnum → spec table.  Matches the stock CircleMUD
    /// spec_assign for these canonical vnums.
    pub fn for_vnum(vnum: MobVnum) -> Option<MobSpec> {
        match vnum {
            1  => Some(MobSpec::Puff),       // The dragon Puff
            11 => Some(MobSpec::Fido),
            12 => Some(MobSpec::Janitor),
            13 => Some(MobSpec::Snake),
            18 => Some(MobSpec::Cityguard),
            15 => Some(MobSpec::Healer),
            16 => Some(MobSpec::Postmaster),
            17 => Some(MobSpec::PetShop),
            14 => Some(MobSpec::Thief),
            _  => None,
        }
    }
}

impl MobInstance {
    /// Refresh or push a new affect on this mob (same-skill stacks
    /// refresh, otherwise push). Mirrors Character::apply_affect.
    pub fn apply_affect(&mut self, a: crate::character::Affect) {
        if let Some(existing) = self.affects.iter_mut().find(|x| x.skill == a.skill) {
            *existing = a;
        } else {
            self.affects.push(a);
        }
    }

    /// Decrement all affect durations by one tick, applying any
    /// `dot_damage` to `self.hp`.  Returns the list of skills that just
    /// expired (caller broadcasts the "looks better" line).
    pub fn tick_affects(&mut self) -> Vec<crate::character::Skill> {
        let mut expired = Vec::new();
        let mut total_dot = 0;
        for a in self.affects.iter_mut() {
            total_dot += a.dot_damage;
            a.duration -= 1;
        }
        if total_dot > 0 { self.hp -= total_dot; }
        self.affects.retain(|a| {
            if a.duration <= 0 { expired.push(a.skill); false } else { true }
        });
        expired
    }
}

/// In-memory world: keyed by vnum so lookups are O(log n) and we sidestep
/// the rnum/vnum two-step that C's `world[]` array required. Instances live
/// in dense Vecs keyed by instance id.
#[derive(Debug, Default)]
pub struct World {
    pub rooms:      BTreeMap<RoomVnum, Room>,
    pub zones:      BTreeMap<ZoneVnum, Zone>,
    pub obj_protos: BTreeMap<ObjVnum, ObjProto>,
    pub mob_protos: BTreeMap<MobVnum, MobProto>,
    pub obj_instances: Vec<ObjInstance>,
    pub mob_instances: Vec<MobInstance>,
    pub shops:         Vec<Shop>,
    pub quests:        BTreeMap<QuestVnum, Quest>,
    pub triggers:      BTreeMap<TriggerVnum, Trigger>,
    pub help:          Vec<HelpEntry>,
    pub socials:       Vec<Social>,
    /// In-memory cache of `<data_dir>/house/<vnum>.owner` files, keyed
    /// by room vnum.  Populated at boot; updated by `do_house` writes.
    pub house_owners:  std::collections::HashMap<RoomVnum, String>,
}

/// One entry from the help database (lib/text/help/help.hlp).  Keywords
/// are stored upper-cased for case-insensitive prefix lookup.
#[derive(Debug, Clone, Default)]
pub struct HelpEntry {
    pub keywords:  Vec<String>,
    pub min_level: i32,
    pub body:      String,
}

/// One social emote loaded from `lib/misc/socials.new`.  The five
/// `String` slots align with the runtime interpreter format:
///   0 — actor with no target
///   1 — room peers with no target
///   2 — actor with a target ($N → target name)
///   3 — room peers with a target ($n → actor, $N → target)
///   4 — target sees the emote done to them ($n → actor)
/// Empty strings mean "say nothing" (the socials file uses `#`).
#[derive(Debug, Clone, Default)]
pub struct Social {
    pub name:          String,
    pub min_position:  i32,
    pub target_required: bool,
    pub actor_no_arg:  String,
    pub room_no_arg:   String,
    pub actor_target:  String,
    pub room_target:   String,
    pub victim_target: String,
}

impl World {
    pub fn room(&self, vnum: RoomVnum) -> Option<&Room> {
        self.rooms.get(&vnum)
    }

    /// Count live instances of a given object vnum currently in the world
    /// (not yet extracted). Mirrors obj_index[].number used by reset_zone.
    pub fn count_obj(&self, vnum: ObjVnum) -> i32 {
        self.obj_instances.iter().filter(|o| o.vnum == vnum).count() as i32
    }

    /// Count live instances of a given mob vnum currently in the world.
    pub fn count_mob(&self, vnum: MobVnum) -> i32 {
        self.mob_instances.iter().filter(|m| m.vnum == vnum).count() as i32
    }

    /// Find a shop whose keeper is in `room` (or that lists `room` in its
    /// `rooms` list).  Returns the first match.
    pub fn shop_in_room(&self, room: RoomVnum) -> Option<&Shop> {
        // Direct room match first.
        if let Some(s) = self.shops.iter().find(|s| s.rooms.contains(&room)) {
            return Some(s);
        }
        // Else: keeper standing in this room?
        for m in &self.mob_instances {
            if m.in_room == room {
                if let Some(s) = self.shops.iter().find(|s| s.keeper_vnum == m.vnum) {
                    return Some(s);
                }
            }
        }
        None
    }

    /// Run one decay tick: decrement `decay_in` on every object that has
    /// it set; when an object reaches 0, dump its contents into its room
    /// (corpse contents fall to the floor) and remove it from the world.
    /// Returns the number of objects extracted (for logging).
    pub fn decay_tick(&mut self, seconds: i32) -> usize {
        let mut to_remove: Vec<u32> = Vec::new();
        // Phase 1: decrement timers, identify expired.
        for o in self.obj_instances.iter_mut() {
            if let Some(t) = o.decay_in {
                let next = t - seconds;
                if next <= 0 {
                    to_remove.push(o.id);
                } else {
                    o.decay_in = Some(next);
                }
            }
        }
        // Phase 2: for each expired corpse, move contents to its room.
        for id in &to_remove {
            let (room, contents) = match self.obj_instances.iter().find(|o| o.id == *id) {
                Some(o) => (o.in_room, o.contents.clone()),
                None => continue,
            };
            if room != NOWHERE {
                if let Some(r) = self.rooms.get_mut(&room) {
                    r.objects.retain(|&i| i != *id);
                    for &cid in &contents {
                        r.objects.push(cid);
                    }
                }
                for &cid in &contents {
                    if let Some(child) = self.obj_instances.iter_mut().find(|o| o.id == cid) {
                        child.in_room = room;
                    }
                }
            }
        }
        // Phase 3: drop the expired instances themselves.
        if !to_remove.is_empty() {
            self.obj_instances.retain(|o| !to_remove.contains(&o.id));
        }
        to_remove.len()
    }

    /// Create a synthetic corpse object containing the given vector of
    /// child instance ids.  Places the corpse in `room` and returns the
    /// new instance id.  `mob_short` becomes the corpse's identifying
    /// string (e.g. "the green gelatinous blob").
    pub fn create_corpse(
        &mut self,
        mob_short: &str,
        contents: Vec<u32>,
        room: RoomVnum,
    ) -> u32 {
        self.create_corpse_with_decay(mob_short, contents, room, CORPSE_DECAY_SECS)
    }

    /// PC-corpse variant: same fields but a much longer decay so the
    /// owner has time to retrieve their stuff.
    pub fn create_pc_corpse(
        &mut self,
        label: &str,
        contents: Vec<u32>,
        room: RoomVnum,
    ) -> u32 {
        self.create_corpse_with_decay(label, contents, room, PC_CORPSE_DECAY_SECS)
    }

    fn create_corpse_with_decay(
        &mut self,
        label: &str,
        contents: Vec<u32>,
        room: RoomVnum,
        decay_secs: i32,
    ) -> u32 {
        let id = self.obj_instances.last().map(|o| o.id + 1).unwrap_or(1);
        for &cid in &contents {
            if let Some(o) = self.obj_instances.iter_mut().find(|o| o.id == cid) {
                o.in_room = NOWHERE;
            }
        }
        self.obj_instances.push(ObjInstance {
            id,
            vnum: CORPSE_VNUM,
            in_room: room,
            contents,
            triggers: Vec::new(),
            corpse_of: Some(label.to_string()),
            decay_in: Some(decay_secs),
            timer: None,
            light_lit: false,
            light_hours: 0,
            condition: 100,
            brewed_spell: None,
            bonus_affects: Vec::new(),
        });
        if let Some(r) = self.rooms.get_mut(&room) {
            r.objects.push(id);
        }
        id
    }

    /// Materialize a fresh instance of the given object prototype, parked
    /// in limbo (`NOWHERE`).  Returns the instance id, or None if the vnum
    /// has no prototype.  Used by login to restore persisted inventories.
    /// Spawn a mob instance from its prototype into the given room.  HP
    /// is rolled from the proto's dice.  Returns the new instance id, or
    /// None when the prototype is unknown or the room doesn't exist.
    pub fn spawn_mob(&mut self, vnum: MobVnum, room: RoomVnum) -> Option<u32> {
        let (hp_dice, hp_size, hp_add) = {
            let p = self.mob_protos.get(&vnum)?;
            (p.hp_dice, p.hp_size, p.hp_add)
        };
        if !self.rooms.contains_key(&room) { return None; }
        // Roll HP using the same dice helper as db::reset_zone.
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let mut hp: i32 = hp_add;
        if hp_dice > 0 && hp_size > 0 {
            for _ in 0..hp_dice {
                hp += rng.gen_range(1..=hp_size);
            }
        }
        let hp = hp.max(1);
        let id = self.mob_instances.last().map(|m| m.id + 1).unwrap_or(1);
        self.rooms.get_mut(&room)?.mobs.push(id);
        self.mob_instances.push(MobInstance {
            id, vnum, in_room: room,
            inventory: Vec::new(),
            hp, max_hp: hp,
            fighting: None,
            remembers: Vec::new(),
            triggers: Vec::new(),
            affects: Vec::new(),
            charmer: None,
            spec: MobSpec::for_vnum(vnum),
            equipment: Default::default(),
            bonus_damroll: 0, bonus_hitroll: 0, bonus_ac: 0,
        });
        Some(id)
    }

    /// Scan a freshly-spawned mob's inventory and auto-equip the first
    /// weapon (into `WEAR_WIELD`) plus the first armor for each slot
    /// the wear flags permit.  Items already worn / wielded stay put.
    /// Returns the count of items moved into slots.
    pub fn auto_equip_mob(&mut self, mob_id: u32) -> u32 {
        use crate::character::{auto_wear_slot, NUM_WEARS, WEAR_WIELD, ITEM_WEAR_WIELD};
        let inv = match self.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => m.inventory.clone(),
            None => return 0,
        };
        let mut moved = 0u32;
        for iid in inv {
            let Some(o) = self.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let Some(p) = self.obj_protos.get(&o.vnum) else { continue; };
            let wear_flags = p.wear_flags[0];
            // Skip non-wearable items (no wear-flag bits beyond TAKE).
            // Try WIELD first, then any other slot via auto_wear_slot.
            let slot: Option<usize> = if wear_flags & ITEM_WEAR_WIELD != 0 {
                Some(WEAR_WIELD)
            } else { auto_wear_slot(wear_flags) };
            let Some(slot) = slot else { continue; };
            if slot >= NUM_WEARS { continue; }
            // Snapshot the proto's affect list before mutating mob (so
            // both borrows don't overlap).
            let affs: Vec<crate::world::ObjAffect> = self.obj_protos.get(&o.vnum)
                .map(|p| p.affected.clone()).unwrap_or_default();
            let m = self.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
            if m.equipment[slot].is_some() { continue; }
            m.equipment[slot] = Some(iid);
            m.inventory.retain(|&i| i != iid);
            for a in affs {
                use crate::world::*;
                match a.location {
                    APPLY_HITROLL => m.bonus_hitroll += a.modifier,
                    APPLY_DAMROLL => m.bonus_damroll += a.modifier,
                    APPLY_AC      => m.bonus_ac      += a.modifier,
                    _ => {}
                }
            }
            moved += 1;
        }
        moved
    }

    pub fn spawn_obj(&mut self, vnum: ObjVnum) -> Option<u32> {
        let proto_timer = self.obj_protos.get(&vnum)?.timer;
        let id = self.obj_instances.last().map(|o| o.id + 1).unwrap_or(1);
        // proto.timer is in MUD-hours; convert to seconds (~75s per
        // mud-hour). Only values >0 enable an active timer.
        let timer = if proto_timer > 0 {
            Some(proto_timer.saturating_mul(75))
        } else {
            None
        };
        self.obj_instances.push(ObjInstance {
            id, vnum, in_room: NOWHERE,
            contents: Vec::new(),
            corpse_of: None,
            decay_in: None,
            triggers: Vec::new(),
            timer,
            light_lit: false,
            light_hours: 0,
            condition: 100,
            brewed_spell: None,
            bonus_affects: Vec::new(),
        });
        Some(id)
    }

    /// Decrement instance `timer` fields on a tick. Returns the list of
    /// (instance_id, room, vnum) for objects whose timer hit zero — the
    /// caller is responsible for firing OTRIG_TIMER and extracting.
    pub fn obj_timer_tick(&mut self, seconds: i32) -> Vec<(u32, RoomVnum, ObjVnum)> {
        let mut expired = Vec::new();
        for o in self.obj_instances.iter_mut() {
            if let Some(t) = o.timer {
                let next = t - seconds;
                if next <= 0 {
                    o.timer = Some(0);
                    expired.push((o.id, o.in_room, o.vnum));
                } else {
                    o.timer = Some(next);
                }
            }
        }
        expired
    }

    /// Extract a single object instance from the world. Container
    /// contents drop to the object's room (mirrors corpse decay). Used
    /// after OTRIG_TIMER has fired.
    pub fn extract_obj(&mut self, id: u32) {
        let (room, contents) = match self.obj_instances.iter().find(|o| o.id == id) {
            Some(o) => (o.in_room, o.contents.clone()),
            None => return,
        };
        if room != NOWHERE {
            if let Some(r) = self.rooms.get_mut(&room) {
                r.objects.retain(|&i| i != id);
                for &cid in &contents { r.objects.push(cid); }
            }
            for &cid in &contents {
                if let Some(child) = self.obj_instances.iter_mut().find(|o| o.id == cid) {
                    child.in_room = room;
                }
            }
        }
        self.obj_instances.retain(|o| o.id != id);
    }

    /// Pick a start room: prefer the configured mortal start, fall back to
    /// the void if the world is incomplete.
    pub fn start_room(&self, immortal: bool) -> RoomVnum {
        if immortal && self.rooms.contains_key(&IMMORT_START) {
            return IMMORT_START;
        }
        if self.rooms.contains_key(&MORTAL_START) {
            return MORTAL_START;
        }
        if self.rooms.contains_key(&VOID_ROOM) {
            return VOID_ROOM;
        }
        // Last resort: first room in the world
        *self.rooms.keys().next().unwrap_or(&VOID_ROOM)
    }
}
