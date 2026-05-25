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
    /// Gold pieces.
    pub gold:         i64,
    pub hp:           i32,
    pub max_hp:       i32,
    /// Current opponent, if any.
    pub fighting:     Option<Target>,
}

impl Character {
    /// Derive starting HP for a brand-new mortal. Immortals (lvl >= 34) get
    /// a much higher pool. Mirrors very loosely what CircleMUD does in
    /// new-character init — exact constants will come with the stat system.
    pub fn init_hp(level: i32) -> i32 {
        // Generous default while combat balance is still placeholder — gives
        // a level-0 mortal enough HP to actually kill something before being
        // pulped by Midgaard's high-level guards.
        if level >= 34 { 1000 } else { 200 + level * 8 }
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
