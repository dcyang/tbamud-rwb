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

use std::collections::HashMap;

use crate::{players::{Class, Sex}, world::RoomVnum};

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

/// A combat skill or spell — CircleMUD doesn't separate them, and we
/// follow that.  Each variant has class restrictions, a mana cost (0 for
/// pure skills), and a hint at whether it is "magical" (uses `cast`) vs.
/// "physical" (its own verb).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Skill {
    Kick,
    Bash,
    Backstab,
    MagicMissile,
    CureLight,
    Bless,
    BurningHands,
    Sanctuary,
    Harm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillKind {
    /// Triggered by its own verb (kick/bash/backstab).
    Physical,
    /// Triggered via `cast '<name>'`.
    Spell,
}

impl Skill {
    /// Parse a player-typed skill or spell name (case-insensitive).
    /// Multi-word spells use lowercase concatenation, e.g. "magic missile"
    /// → "magic-missile" or "magicmissile".
    pub fn parse(s: &str) -> Option<Skill> {
        let s = s.to_ascii_lowercase();
        let normalized = s.replace([' ', '-', '_'], "");
        match normalized.as_str() {
            "kick"         => Some(Skill::Kick),
            "bash"         => Some(Skill::Bash),
            "backstab"     => Some(Skill::Backstab),
            "magicmissile" => Some(Skill::MagicMissile),
            "curelight"    => Some(Skill::CureLight),
            "bless"        => Some(Skill::Bless),
            "burninghands" => Some(Skill::BurningHands),
            "sanctuary"    => Some(Skill::Sanctuary),
            "harm"         => Some(Skill::Harm),
            _ => None,
        }
    }

    /// Canonical name (lowercase, may contain spaces for spells).
    pub fn name(self) -> &'static str {
        match self {
            Skill::Kick         => "kick",
            Skill::Bash         => "bash",
            Skill::Backstab     => "backstab",
            Skill::MagicMissile => "magic missile",
            Skill::CureLight    => "cure light",
            Skill::Bless        => "bless",
            Skill::BurningHands => "burning hands",
            Skill::Sanctuary    => "sanctuary",
            Skill::Harm         => "harm",
        }
    }

    pub fn kind(self) -> SkillKind {
        match self {
            Skill::Kick | Skill::Bash | Skill::Backstab => SkillKind::Physical,
            Skill::MagicMissile | Skill::CureLight
                | Skill::Bless  | Skill::BurningHands
                | Skill::Sanctuary | Skill::Harm        => SkillKind::Spell,
        }
    }

    /// Mana cost when invoking this skill.  Zero for physical skills.
    pub fn mana_cost(self) -> i32 {
        match self {
            Skill::Kick | Skill::Bash | Skill::Backstab => 0,
            Skill::MagicMissile => 8,
            Skill::CureLight    => 6,
            Skill::Bless        => 5,
            Skill::BurningHands => 12,
            Skill::Sanctuary    => 10,
            Skill::Harm         => 10,
        }
    }

    /// Which classes can learn this skill.
    pub fn allowed_classes(self) -> &'static [Class] {
        match self {
            Skill::Kick         => &[Class::Warrior, Class::Thief, Class::Cleric],
            Skill::Bash         => &[Class::Warrior],
            Skill::Backstab     => &[Class::Thief],
            Skill::MagicMissile => &[Class::MagicUser],
            Skill::CureLight    => &[Class::Cleric],
            Skill::Bless        => &[Class::Cleric],
            Skill::BurningHands => &[Class::MagicUser],
            Skill::Sanctuary    => &[Class::Cleric],
            Skill::Harm         => &[Class::Cleric],
        }
    }

    pub fn is_class_allowed(self, class: Class) -> bool {
        self.allowed_classes().contains(&class)
    }

    /// Storage key for serialisation in the player file (spaces collapsed).
    pub fn save_key(self) -> &'static str {
        match self {
            Skill::Kick         => "kick",
            Skill::Bash         => "bash",
            Skill::Backstab     => "backstab",
            Skill::MagicMissile => "magic-missile",
            Skill::CureLight    => "cure-light",
            Skill::Bless        => "bless",
            Skill::BurningHands => "burning-hands",
            Skill::Sanctuary    => "sanctuary",
            Skill::Harm         => "harm",
        }
    }

    /// Inverse of save_key.
    pub fn from_save_key(s: &str) -> Option<Skill> {
        Self::parse(s)
    }
}

/// All known skills — iteration order for `skills` command + persistence.
pub const ALL_SKILLS: &[Skill] = &[
    Skill::Kick, Skill::Bash, Skill::Backstab,
    Skill::MagicMissile, Skill::CureLight,
    Skill::Bless, Skill::BurningHands,
    Skill::Sanctuary, Skill::Harm,
];

// ---------------------------------------------------------------------------
// Affects (temporary buffs/debuffs)
// ---------------------------------------------------------------------------

/// A timed effect on a character.  Stacks of the same spell refresh rather
/// than accumulate.  Tick count is in combat-tick units (2s each).
#[derive(Debug, Clone)]
pub struct Affect {
    pub skill:         Skill,
    pub duration:      i32,
    pub to_hit:        i32,
    pub to_dam:        i32,
    /// Percent damage reduction on incoming attacks (0..=75).
    pub dmg_reduction: i32,
}

impl Affect {
    pub fn name(&self) -> &'static str { self.skill.name() }
}

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
    pub exp:          i64,
    pub hp:           i32,
    pub max_hp:       i32,
    pub mana:         i32,
    pub max_mana:     i32,
    /// Unspent practice points. Gained on level-up, spent in `practice`.
    pub practices:    i32,
    /// Ability scores — rolled at creation (3d6 each), then persisted.
    pub str_:         i32,
    pub int_:         i32,
    pub wis:          i32,
    pub dex:          i32,
    pub con:          i32,
    pub cha:          i32,
    /// Current opponent, if any.
    pub fighting:     Option<Target>,
    /// Learned skill levels — value is "practice percent" (0..=100).  Only
    /// skills the player has invested in appear here.
    pub skills:       HashMap<Skill, u8>,
    /// Active temporary affects (buffs/debuffs).  Not persisted across
    /// sessions.
    pub affects:      Vec<Affect>,
}

/// STR-based damage modifier — mirrors str_app[].todam in constants.c
/// (the second column of the strength table).
pub fn str_damage_bonus(str_score: i32) -> i32 {
    // Index 0..=25 in CircleMUD's table; higher strengths (18/01..18/00)
    // collapse to the str=18..25 entries here.
    static TODAM: &[i32] = &[
        // 0  1  2  3  4  5  6  7  8  9
          -4,-4,-2,-2,-1,-1, 0, 0, 0, 0,
        // 10 11 12 13 14 15 16 17 18 19
           0, 0, 0, 0, 0, 0, 1, 1, 2, 3,
        // 20 21 22 23 24 25
           3, 4, 5, 6, 6, 7,
    ];
    let i = str_score.clamp(0, (TODAM.len() - 1) as i32) as usize;
    TODAM[i]
}

/// DEX-based AC bonus — mirrors dex_app[].defensive in constants.c.
/// Negative values reduce AC (better defense).  Returned with the same
/// sign convention as armor: more positive = better.  So we negate the
/// CircleMUD column to match.
pub fn dex_ac_bonus(dex_score: i32) -> i32 {
    static DEFENSIVE: &[i32] = &[
        // 0  1  2  3  4  5  6  7  8  9
           5, 5, 5, 4, 3, 2, 1, 1, 0, 0,
        // 10 11 12 13 14 15 16 17 18 19
           0, 0, 0, 0, 0, 0,-1,-1,-2,-3,
        // 20 21 22 23 24 25
          -4,-4,-4,-5,-5,-6,
    ];
    let i = dex_score.clamp(0, (DEFENSIVE.len() - 1) as i32) as usize;
    -DEFENSIVE[i]   // tbamud-rwb AC is "higher = better"
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
    /// Class-specific HP gain per level. Mirrors the CircleMUD ranges in
    /// constants.c::Class_apply_table[].hit_dice.
    pub fn hp_per_level(class: Class) -> i32 {
        match class {
            Class::Warrior   => 12,
            Class::Cleric    => 9,
            Class::Thief     => 8,
            Class::MagicUser => 6,
            Class::Undefined => 8,
        }
    }

    /// Class-specific mana gain per level.  Spellcasters scale faster.
    pub fn mana_per_level(class: Class) -> i32 {
        match class {
            Class::MagicUser => 10,
            Class::Cleric    =>  8,
            Class::Thief     =>  2,
            Class::Warrior   =>  2,
            Class::Undefined =>  4,
        }
    }

    /// Starting mana for a freshly-rolled character.
    pub fn init_mana_for_class(class: Class, int_or_wis: i32, level: i32) -> i32 {
        let base = 10;
        let per_lvl = Self::mana_per_level(class);
        let stat_bonus = (int_or_wis - 10).max(0) / 2;
        base + per_lvl * level.max(1) + stat_bonus * level.max(1)
    }

    /// Practice points granted on each level-up.
    pub const PRACTICES_PER_LEVEL: i32 = 2;

    // ----- Affect helpers ----------------------------------------------------

    /// Sum of `to_hit` bonuses from all active affects.
    pub fn affect_hit_bonus(&self) -> i32 {
        self.affects.iter().map(|a| a.to_hit).sum()
    }

    /// Sum of `to_dam` bonuses from all active affects.
    pub fn affect_dam_bonus(&self) -> i32 {
        self.affects.iter().map(|a| a.to_dam).sum()
    }

    /// Total damage reduction percent (0..=75) from active affects.
    pub fn affect_dmg_reduction(&self) -> i32 {
        let total: i32 = self.affects.iter().map(|a| a.dmg_reduction).sum();
        total.clamp(0, 75)
    }

    /// Replace an existing affect from the same spell (refresh duration)
    /// or push a new one.
    pub fn apply_affect(&mut self, a: Affect) {
        if let Some(existing) = self.affects.iter_mut().find(|x| x.skill == a.skill) {
            *existing = a;
        } else {
            self.affects.push(a);
        }
    }

    /// Decrement all affect durations.  Returns the list of skills whose
    /// effects just expired (for "X fades away" messaging).
    pub fn tick_affects(&mut self) -> Vec<Skill> {
        let mut expired = Vec::new();
        for a in self.affects.iter_mut() {
            a.duration -= 1;
        }
        self.affects.retain(|a| {
            if a.duration <= 0 { expired.push(a.skill); false } else { true }
        });
        expired
    }

    pub fn init_hp(level: i32) -> i32 {
        // Class-independent default; per-class scaling is applied on
        // level-up.  Mortals start at 50 (+ a small ramp per level), but
        // the actual starting HP for a brand-new character is set in
        // descriptor.rs using init_hp_for_class().
        if level >= 34 { 1000 } else { 50 + level * 10 }
    }

    /// Starting HP for a freshly-rolled character.  Combines a flat base
    /// with the per-level gain for the chosen class.
    pub fn init_hp_for_class(class: Class, con: i32, level: i32) -> i32 {
        if level >= 34 { return 1000; }
        let base = 30;
        let per_lvl = Self::hp_per_level(class);
        let con_bonus = (con - 10).max(0) / 2;
        base + per_lvl * level.max(1) + con_bonus * level.max(1)
    }

    /// Max mortal level. Above this you're an immortal (LVL_IMMORT = 34 in
    /// CircleMUD).  Mortal progression stops here.
    pub const MAX_MORTAL_LEVEL: i32 = 30;

    /// XP needed to advance from `cur_level` → `cur_level + 1`.  Simple
    /// linear-ish ramp; CircleMUD uses class-specific tables that we'll
    /// inherit once the class system arrives.
    pub fn exp_for_level(cur_level: i32) -> i64 {
        if cur_level >= Self::MAX_MORTAL_LEVEL {
            i64::MAX  // can't level past the cap
        } else {
            // Triangle-style: 1000, 3000, 6000, 10000, ...
            let n = (cur_level as i64) + 1;
            n * (n + 1) / 2 * 1000
        }
    }

    /// Apply level-up effects if the character has enough XP. Returns the
    /// number of levels gained (0 if none).
    pub fn check_level_up(&mut self) -> i32 {
        let mut gained = 0;
        while self.level < Self::MAX_MORTAL_LEVEL
            && self.exp >= Self::exp_for_level(self.level)
        {
            self.level += 1;
            // Class-specific HP gain + CON bonus, heal to full.
            let con_bonus = (self.con - 10).max(0) / 2;
            self.max_hp   += Self::hp_per_level(self.class)   + con_bonus;
            self.hp = self.max_hp;
            // Mana gain: scales with INT for arcane, WIS for divine.
            let casting_stat = match self.class {
                Class::MagicUser => self.int_,
                Class::Cleric    => self.wis,
                _                => self.int_,
            };
            let stat_bonus = (casting_stat - 10).max(0) / 2;
            self.max_mana += Self::mana_per_level(self.class) + stat_bonus;
            self.mana = self.max_mana;
            // Practice points.
            self.practices += Self::PRACTICES_PER_LEVEL;
            gained += 1;
        }
        gained
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
