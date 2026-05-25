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
}

/// ITEM_* item-type constants (mirror structs.h).  Used by parsers and
/// gameplay (containers, weapons, armor).
pub const ITEM_LIGHT:     i32 = 1;
pub const ITEM_WEAPON:    i32 = 5;
pub const ITEM_ARMOR:     i32 = 9;
pub const ITEM_CONTAINER: i32 = 15;

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

    /// Materialize a fresh instance of the given object prototype, parked
    /// in limbo (`NOWHERE`).  Returns the instance id, or None if the vnum
    /// has no prototype.  Used by login to restore persisted inventories.
    pub fn spawn_obj(&mut self, vnum: ObjVnum) -> Option<u32> {
        if !self.obj_protos.contains_key(&vnum) { return None; }
        let id = self.obj_instances.last().map(|o| o.id + 1).unwrap_or(1);
        self.obj_instances.push(ObjInstance {
            id, vnum, in_room: NOWHERE,
            contents: Vec::new(),
        });
        Some(id)
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
