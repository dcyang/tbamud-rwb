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

/// A live online player's complete state, owned by their connection task.
/// Mob in-world state still lives in `world::MobInstance` for now.
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
}

/// Lightweight handle in the shared online-player registry. Holds just
/// enough to (a) display in `who`, (b) deliver a message via mpsc.
#[derive(Debug, Clone)]
pub struct PlayerHandle {
    pub id:           u32,
    pub name:         String,
    pub level:        i32,
    pub current_room: RoomVnum,
    /// Outbound message channel — the connection's writer task receives
    /// strings from this and writes them to the socket.
    pub send:         mpsc::UnboundedSender<String>,
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
