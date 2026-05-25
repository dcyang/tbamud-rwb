/// In-game character state — the Rust equivalent of `char_data` in structs.h
/// (minimal subset). Used for both online players and mobs.
///
/// The split between this and `world::MobInstance` is deliberate: MobInstance
/// is the world-loader's record of "a mob exists in this room", while
/// Character is the in-game state that grows once we start tracking
/// inventory/equipment/hp/etc. For mobs we currently keep MobInstance only
/// (no per-mob inventory yet); for players we use Character.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::{players::{Class, Sex}, world::RoomVnum};

/// Who/what a character is fighting. Mob instance ids are positive; we use
/// the same numeric space for player ids (they're both `u32`) — the
/// disambiguator is `is_player`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub id:        u32,
    pub is_player: bool,
}

/// A live online player's complete state.  Lives behind `Arc<Mutex<>>` in
/// `PlayerHandle.character` so the combat-tick task can mutate HP and
/// fighting state concurrently with the player's own connection.
#[derive(Debug)]
pub struct Character {
    pub id:           u32,
    pub name:         String,
    pub level:        i32,
    pub sex:          Sex,
    pub class:        Class,
    pub current_room: RoomVnum,
    /// Object instance ids carried by this character.
    pub inventory:    Vec<u32>,
    /// Worn/wielded equipment, keyed by WearPos.
    pub equipment:    [Option<u32>; NUM_WEARS],
    /// Gold pieces.
    pub gold:         i64,
    pub hp:           i32,
    pub max_hp:       i32,
    /// Ability scores — rolled at creation (3d6 each), then persisted.
    pub str_:         i32,
    pub int_:         i32,
    pub wis:          i32,
    pub dex:          i32,
    pub con:          i32,
    pub cha:          i32,
    /// Current opponent, if any.
    pub fighting:     Option<Target>,
}

/// STR-based damage modifier. Roughly mirrors the str_app[].todam table in
/// constants.c, scaled down: every 2 points above 10 gives +1, every 2
/// below gives −1.  Clamped to [-5, +8].
pub fn str_damage_bonus(str_score: i32) -> i32 {
    ((str_score - 10) / 2).clamp(-5, 8)
}

// ---------------------------------------------------------------------------
// Wear positions — mirror the WEAR_* defines in structs.h.
// ---------------------------------------------------------------------------

pub const WEAR_LIGHT:    usize = 0;
pub const WEAR_FINGER_R: usize = 1;
pub const WEAR_FINGER_L: usize = 2;
pub const WEAR_NECK_1:   usize = 3;
pub const WEAR_NECK_2:   usize = 4;
pub const WEAR_BODY:     usize = 5;
pub const WEAR_HEAD:     usize = 6;
pub const WEAR_LEGS:     usize = 7;
pub const WEAR_FEET:     usize = 8;
pub const WEAR_HANDS:    usize = 9;
pub const WEAR_ARMS:     usize = 10;
pub const WEAR_SHIELD:   usize = 11;
pub const WEAR_ABOUT:    usize = 12;
pub const WEAR_WAIST:    usize = 13;
pub const WEAR_WRIST_R:  usize = 14;
pub const WEAR_WRIST_L:  usize = 15;
pub const WEAR_WIELD:    usize = 16;
pub const WEAR_HOLD:     usize = 17;
pub const NUM_WEARS:     usize = 18;

/// ITEM_WEAR_* bit flags (from values stored in `ObjProto.wear_flags[0]`).
/// Bit 0 is `ITEM_WEAR_TAKE` (means takeable, no slot).
pub const ITEM_WEAR_TAKE:   u32 = 1 << 0;
pub const ITEM_WEAR_FINGER: u32 = 1 << 1;
pub const ITEM_WEAR_NECK:   u32 = 1 << 2;
pub const ITEM_WEAR_BODY:   u32 = 1 << 3;
pub const ITEM_WEAR_HEAD:   u32 = 1 << 4;
pub const ITEM_WEAR_LEGS:   u32 = 1 << 5;
pub const ITEM_WEAR_FEET:   u32 = 1 << 6;
pub const ITEM_WEAR_HANDS:  u32 = 1 << 7;
pub const ITEM_WEAR_ARMS:   u32 = 1 << 8;
pub const ITEM_WEAR_SHIELD: u32 = 1 << 9;
pub const ITEM_WEAR_ABOUT:  u32 = 1 << 10;
pub const ITEM_WEAR_WAIST:  u32 = 1 << 11;
pub const ITEM_WEAR_WRIST:  u32 = 1 << 12;
pub const ITEM_WEAR_WIELD:  u32 = 1 << 13;
pub const ITEM_WEAR_HOLD:   u32 = 1 << 14;

/// Map a `wear_flags[0]` bitmask to a preferred slot (the position
/// `do_wear` would assign automatically).  Returns `None` for items that
/// cannot be worn (only TAKE or no wear bits beyond TAKE).
pub fn auto_wear_slot(wear_flags: u32) -> Option<usize> {
    // Check in the same order as CircleMUD's wear_bits[] traversal.
    if wear_flags & ITEM_WEAR_FINGER != 0 { return Some(WEAR_FINGER_R); }
    if wear_flags & ITEM_WEAR_NECK   != 0 { return Some(WEAR_NECK_1); }
    if wear_flags & ITEM_WEAR_BODY   != 0 { return Some(WEAR_BODY); }
    if wear_flags & ITEM_WEAR_HEAD   != 0 { return Some(WEAR_HEAD); }
    if wear_flags & ITEM_WEAR_LEGS   != 0 { return Some(WEAR_LEGS); }
    if wear_flags & ITEM_WEAR_FEET   != 0 { return Some(WEAR_FEET); }
    if wear_flags & ITEM_WEAR_HANDS  != 0 { return Some(WEAR_HANDS); }
    if wear_flags & ITEM_WEAR_ARMS   != 0 { return Some(WEAR_ARMS); }
    if wear_flags & ITEM_WEAR_SHIELD != 0 { return Some(WEAR_SHIELD); }
    if wear_flags & ITEM_WEAR_ABOUT  != 0 { return Some(WEAR_ABOUT); }
    if wear_flags & ITEM_WEAR_WAIST  != 0 { return Some(WEAR_WAIST); }
    if wear_flags & ITEM_WEAR_WRIST  != 0 { return Some(WEAR_WRIST_R); }
    if wear_flags & ITEM_WEAR_HOLD   != 0 { return Some(WEAR_HOLD); }
    // WIELD is intentionally NOT in `wear`; player uses `wield` instead.
    None
}

pub fn wear_pos_name(pos: usize) -> &'static str {
    match pos {
        WEAR_LIGHT    => "as a light",
        WEAR_FINGER_R => "on the right finger",
        WEAR_FINGER_L => "on the left finger",
        WEAR_NECK_1   => "around the neck",
        WEAR_NECK_2   => "around the neck",
        WEAR_BODY     => "on the body",
        WEAR_HEAD     => "on the head",
        WEAR_LEGS     => "on the legs",
        WEAR_FEET     => "on the feet",
        WEAR_HANDS    => "on the hands",
        WEAR_ARMS     => "on the arms",
        WEAR_SHIELD   => "as a shield",
        WEAR_ABOUT    => "about the body",
        WEAR_WAIST    => "about the waist",
        WEAR_WRIST_R  => "around the right wrist",
        WEAR_WRIST_L  => "around the left wrist",
        WEAR_WIELD    => "wielded",
        WEAR_HOLD     => "held",
        _             => "somewhere",
    }
}

impl Character {
    /// Derive starting HP for a brand-new mortal. Immortals (lvl >= 34) get
    /// a much higher pool. Mirrors very loosely what CircleMUD does in
    /// new-character init — exact constants will come with the stat system.
    pub fn init_hp(level: i32) -> i32 {
        // Starting HP for a brand-new mortal.  Generous enough to survive
        // a sparring match in the temple courtyard.  Subsequent sessions
        // restore from the saved value (Hit: cur/max in the player file).
        if level >= 34 { 1000 } else { 50 + level * 10 }
    }

    /// Roll a fresh ability score: 3d6, immortals get a +6 bonus so they
    /// never sit at average mortal stats.
    pub fn roll_ability(immortal: bool) -> i32 {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let mut total = 0;
        for _ in 0..3 { total += rng.gen_range(1..=6); }
        if immortal { total + 6 } else { total }
    }
}

/// Handle in the shared online-player registry. Holds a copy of cheap
/// identifying fields (name, level, current_room — for room broadcasts and
/// `who` without locking the character), an mpsc sender for inbound text,
/// and a shared handle to the full character behind a lock.
#[derive(Debug, Clone)]
pub struct PlayerHandle {
    pub id:           u32,
    pub name:         String,
    pub level:        i32,
    pub current_room: RoomVnum,
    /// Outbound message channel — the connection's writer task receives
    /// strings from this and writes them to the socket.
    pub send:         mpsc::UnboundedSender<String>,
    /// Full character state. Lock briefly for HP/inventory/fighting mutation.
    pub character:    Arc<tokio::sync::Mutex<Character>>,
}

/// Registry of all currently-online players, keyed by player id (the same
/// id used in players.rs PlayerIndexEntry).
#[derive(Debug, Default)]
pub struct CharacterList {
    pub players: Vec<PlayerHandle>,
}

impl CharacterList {
    pub fn add(&mut self, h: PlayerHandle) {
        self.players.push(h);
    }

    pub fn remove(&mut self, id: u32) {
        self.players.retain(|p| p.id != id);
    }

    pub fn find_by_name(&self, name: &str) -> Option<&PlayerHandle> {
        self.players.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn update_room(&mut self, id: u32, room: RoomVnum) {
        if let Some(p) = self.players.iter_mut().find(|p| p.id == id) {
            p.current_room = room;
        }
    }

    /// Broadcast text to every player currently in `room`, except `except_id`.
    pub fn broadcast_room(&self, room: RoomVnum, except_id: Option<u32>, msg: &str) {
        for p in &self.players {
            if p.current_room != room { continue; }
            if Some(p.id) == except_id { continue; }
            // Silently drop on closed channel — the writer task has exited.
            let _ = p.send.send(msg.to_string());
        }
    }

    /// Iterate all players (read-only).
    pub fn iter(&self) -> impl Iterator<Item = &PlayerHandle> {
        self.players.iter()
    }
}

/// Convenience type alias for the shared registry.
pub type SharedChars = Arc<tokio::sync::Mutex<CharacterList>>;
