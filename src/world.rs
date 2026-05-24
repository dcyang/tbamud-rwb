/// World data: rooms, zones, exits.
/// Mirrors the room_data / zone_data / room_direction_data structs in structs.h
/// and the world[] / zone_table[] globals declared in db.c.

use std::collections::BTreeMap;

pub type RoomVnum = i32;
pub type ZoneVnum = i32;

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
}

/// A single zone. Mirrors zone_data in structs.h (minimal subset; reset
/// commands deferred to a later checkpoint).
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
}

/// In-memory world: keyed by vnum so lookups are O(log n) and we sidestep
/// the rnum/vnum two-step that C's `world[]` array required.
#[derive(Debug, Default)]
pub struct World {
    pub rooms: BTreeMap<RoomVnum, Room>,
    pub zones: BTreeMap<ZoneVnum, Zone>,
}

impl World {
    pub fn room(&self, vnum: RoomVnum) -> Option<&Room> {
        self.rooms.get(&vnum)
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
