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
    PickLock,
    MagicMissile,
    CureLight,
    Bless,
    BurningHands,
    Sanctuary,
    Harm,
    Sneak,
    Hide,
    Steal,
    WordOfRecall,
    Identify,
    DetectInvis,
    DetectMagic,
    Poison,
    Sleep,
    Blindness,
    CurePoison,
    CureBlind,
    CureCritic,
    Strength,
    Armor,
    Haste,
    Slow,
    Earthquake,
    CharmPerson,
    LocateObject,
    Refresh,
    Summon,
    SenseLife,
    Dodge,
    Parry,
    Rescue,
    LightningBolt,
    Fireball,
    ShockingGrasp,
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
            "picklock" | "pick" => Some(Skill::PickLock),
            "magicmissile" => Some(Skill::MagicMissile),
            "curelight"    => Some(Skill::CureLight),
            "bless"        => Some(Skill::Bless),
            "burninghands" => Some(Skill::BurningHands),
            "sanctuary"    => Some(Skill::Sanctuary),
            "harm"         => Some(Skill::Harm),
            "sneak"        => Some(Skill::Sneak),
            "hide"         => Some(Skill::Hide),
            "steal"        => Some(Skill::Steal),
            "wordofrecall" => Some(Skill::WordOfRecall),
            "identify"     => Some(Skill::Identify),
            "detectinvis" | "detectinvisible" => Some(Skill::DetectInvis),
            "detectmagic"                     => Some(Skill::DetectMagic),
            "poison"                          => Some(Skill::Poison),
            "sleep"                           => Some(Skill::Sleep),
            "blindness" | "blind"             => Some(Skill::Blindness),
            "curepoison"                      => Some(Skill::CurePoison),
            "cureblind" | "cureblindness"     => Some(Skill::CureBlind),
            "curecritic" | "curecritical"     => Some(Skill::CureCritic),
            "strength"                        => Some(Skill::Strength),
            "armor"                           => Some(Skill::Armor),
            "haste"                           => Some(Skill::Haste),
            "slow"                            => Some(Skill::Slow),
            "earthquake"                      => Some(Skill::Earthquake),
            "charmperson" | "charm"           => Some(Skill::CharmPerson),
            "locateobject" | "locate"         => Some(Skill::LocateObject),
            "refresh"                         => Some(Skill::Refresh),
            "summon"                          => Some(Skill::Summon),
            "senselife" | "sense-life"        => Some(Skill::SenseLife),
            "dodge"                           => Some(Skill::Dodge),
            "parry"                           => Some(Skill::Parry),
            "rescue"                          => Some(Skill::Rescue),
            "lightningbolt" | "lightning"     => Some(Skill::LightningBolt),
            "fireball"                        => Some(Skill::Fireball),
            "shockinggrasp" | "shockgrasp" | "shock" => Some(Skill::ShockingGrasp),
            _ => None,
        }
    }

    /// Canonical name (lowercase, may contain spaces for spells).
    pub fn name(self) -> &'static str {
        match self {
            Skill::Kick         => "kick",
            Skill::Bash         => "bash",
            Skill::Backstab     => "backstab",
            Skill::PickLock     => "pick lock",
            Skill::MagicMissile => "magic missile",
            Skill::CureLight    => "cure light",
            Skill::Bless        => "bless",
            Skill::BurningHands => "burning hands",
            Skill::Sanctuary    => "sanctuary",
            Skill::Harm         => "harm",
            Skill::Sneak        => "sneak",
            Skill::Hide         => "hide",
            Skill::Steal        => "steal",
            Skill::WordOfRecall => "word of recall",
            Skill::Identify     => "identify",
            Skill::DetectInvis  => "detect invis",
            Skill::DetectMagic  => "detect magic",
            Skill::Poison       => "poison",
            Skill::Sleep        => "sleep",
            Skill::Blindness    => "blindness",
            Skill::CurePoison   => "cure poison",
            Skill::CureBlind    => "cure blindness",
            Skill::CureCritic   => "cure critic",
            Skill::Strength     => "strength",
            Skill::Armor        => "armor",
            Skill::Haste        => "haste",
            Skill::Slow         => "slow",
            Skill::Earthquake   => "earthquake",
            Skill::CharmPerson  => "charm person",
            Skill::LocateObject => "locate object",
            Skill::Refresh      => "refresh",
            Skill::Summon       => "summon",
            Skill::SenseLife    => "sense life",
            Skill::Dodge        => "dodge",
            Skill::Parry        => "parry",
            Skill::Rescue       => "rescue",
            Skill::LightningBolt => "lightning bolt",
            Skill::Fireball      => "fireball",
            Skill::ShockingGrasp => "shocking grasp",
        }
    }

    pub fn kind(self) -> SkillKind {
        match self {
            Skill::Kick | Skill::Bash | Skill::Backstab | Skill::PickLock
                | Skill::Sneak | Skill::Hide | Skill::Steal
                | Skill::Dodge | Skill::Parry | Skill::Rescue => SkillKind::Physical,
            Skill::MagicMissile | Skill::CureLight
                | Skill::Bless  | Skill::BurningHands
                | Skill::Sanctuary | Skill::Harm
                | Skill::WordOfRecall | Skill::Identify
                | Skill::DetectInvis  | Skill::DetectMagic
                | Skill::Poison       | Skill::Sleep | Skill::Blindness
                | Skill::CurePoison   | Skill::CureBlind | Skill::CureCritic
                | Skill::Strength     | Skill::Armor | Skill::Haste | Skill::Slow
                | Skill::Earthquake   | Skill::CharmPerson | Skill::LocateObject
                | Skill::Refresh      | Skill::Summon       | Skill::SenseLife
                | Skill::LightningBolt | Skill::Fireball | Skill::ShockingGrasp
                                      => SkillKind::Spell,
        }
    }

    /// Mana cost when invoking this skill.  Zero for physical skills.
    pub fn mana_cost(self) -> i32 {
        match self {
            Skill::Kick | Skill::Bash | Skill::Backstab | Skill::PickLock
                | Skill::Sneak | Skill::Hide | Skill::Steal
                | Skill::Dodge | Skill::Parry | Skill::Rescue => 0,
            Skill::MagicMissile => 8,
            Skill::CureLight    => 6,
            Skill::Bless        => 5,
            Skill::BurningHands => 12,
            Skill::Sanctuary    => 10,
            Skill::Harm         => 10,
            Skill::WordOfRecall => 20,
            Skill::Identify     => 15,
            Skill::DetectInvis  => 10,
            Skill::DetectMagic  => 8,
            Skill::Poison       => 12,
            Skill::Sleep        => 15,
            Skill::Blindness    => 8,
            Skill::CurePoison   => 10,
            Skill::CureBlind    => 10,
            Skill::CureCritic   => 14,
            Skill::Strength     => 8,
            Skill::Armor        => 10,
            Skill::Haste        => 15,
            Skill::Slow         => 12,
            Skill::Earthquake   => 18,
            Skill::CharmPerson  => 14,
            Skill::LocateObject => 10,
            Skill::Refresh      => 4,
            Skill::Summon       => 25,
            Skill::SenseLife    => 6,
            Skill::LightningBolt => 20,
            Skill::Fireball      => 30,
            Skill::ShockingGrasp => 8,
        }
    }

    /// Which classes can learn this skill.
    pub fn allowed_classes(self) -> &'static [Class] {
        match self {
            Skill::Kick         => &[Class::Warrior, Class::Thief, Class::Cleric],
            Skill::Bash         => &[Class::Warrior],
            Skill::Backstab     => &[Class::Thief],
            Skill::PickLock     => &[Class::Thief],
            Skill::MagicMissile => &[Class::MagicUser],
            Skill::CureLight    => &[Class::Cleric],
            Skill::Bless        => &[Class::Cleric],
            Skill::BurningHands => &[Class::MagicUser],
            Skill::Sanctuary    => &[Class::Cleric],
            Skill::Harm         => &[Class::Cleric],
            Skill::Sneak        => &[Class::Thief],
            Skill::Hide         => &[Class::Thief],
            Skill::Steal        => &[Class::Thief],
            // Word of recall is Cleric-only here, but in CircleMUD it's
            // shared between Cleric and MagicUser (and trivially castable
            // by all in many forks).  Keep Cleric-only for now.
            Skill::WordOfRecall => &[Class::Cleric, Class::MagicUser],
            Skill::Identify     => &[Class::MagicUser],
            Skill::DetectInvis  => &[Class::MagicUser, Class::Cleric],
            Skill::DetectMagic  => &[Class::MagicUser, Class::Cleric],
            Skill::Poison       => &[Class::MagicUser, Class::Cleric],
            Skill::Sleep        => &[Class::MagicUser],
            Skill::Blindness    => &[Class::MagicUser, Class::Cleric],
            Skill::CurePoison   => &[Class::Cleric],
            Skill::CureBlind    => &[Class::Cleric],
            Skill::CureCritic   => &[Class::Cleric],
            Skill::Strength     => &[Class::MagicUser],
            Skill::Armor        => &[Class::Cleric],
            Skill::Haste        => &[Class::MagicUser],
            Skill::Slow         => &[Class::MagicUser],
            Skill::Earthquake   => &[Class::MagicUser, Class::Cleric],
            Skill::CharmPerson  => &[Class::MagicUser],
            Skill::LocateObject => &[Class::MagicUser, Class::Cleric],
            Skill::Refresh      => &[Class::Cleric],
            Skill::Summon       => &[Class::MagicUser],
            Skill::SenseLife    => &[Class::Cleric, Class::MagicUser],
            Skill::Dodge        => &[Class::Warrior, Class::Thief],
            Skill::Parry        => &[Class::Warrior],
            Skill::Rescue       => &[Class::Warrior, Class::Cleric],
            Skill::LightningBolt => &[Class::MagicUser],
            Skill::Fireball      => &[Class::MagicUser],
            Skill::ShockingGrasp => &[Class::MagicUser],
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
            Skill::PickLock     => "pick-lock",
            Skill::MagicMissile => "magic-missile",
            Skill::CureLight    => "cure-light",
            Skill::Bless        => "bless",
            Skill::BurningHands => "burning-hands",
            Skill::Sanctuary    => "sanctuary",
            Skill::Harm         => "harm",
            Skill::Sneak        => "sneak",
            Skill::Hide         => "hide",
            Skill::Steal        => "steal",
            Skill::WordOfRecall => "word-of-recall",
            Skill::Identify     => "identify",
            Skill::DetectInvis  => "detect-invis",
            Skill::DetectMagic  => "detect-magic",
            Skill::Poison       => "poison",
            Skill::Sleep        => "sleep",
            Skill::Blindness    => "blindness",
            Skill::CurePoison   => "cure-poison",
            Skill::CureBlind    => "cure-blind",
            Skill::CureCritic   => "cure-critic",
            Skill::Strength     => "strength",
            Skill::Armor        => "armor",
            Skill::Haste        => "haste",
            Skill::Slow         => "slow",
            Skill::Earthquake   => "earthquake",
            Skill::CharmPerson  => "charm-person",
            Skill::LocateObject => "locate-object",
            Skill::Refresh      => "refresh",
            Skill::Summon       => "summon",
            Skill::SenseLife    => "sense-life",
            Skill::Dodge        => "dodge",
            Skill::Parry        => "parry",
            Skill::Rescue       => "rescue",
            Skill::LightningBolt => "lightning-bolt",
            Skill::Fireball      => "fireball",
            Skill::ShockingGrasp => "shocking-grasp",
        }
    }

    /// Inverse of save_key.
    pub fn from_save_key(s: &str) -> Option<Skill> {
        Self::parse(s)
    }
}

/// All known skills — iteration order for `skills` command + persistence.
pub const ALL_SKILLS: &[Skill] = &[
    Skill::Kick, Skill::Bash, Skill::Backstab, Skill::PickLock,
    Skill::Sneak, Skill::Hide, Skill::Steal,
    Skill::MagicMissile, Skill::CureLight,
    Skill::Bless, Skill::BurningHands,
    Skill::Sanctuary, Skill::Harm,
    Skill::WordOfRecall, Skill::Identify,
    Skill::DetectInvis, Skill::DetectMagic,
    Skill::Poison, Skill::Sleep, Skill::Blindness,
    Skill::CurePoison, Skill::CureBlind, Skill::CureCritic,
    Skill::Strength, Skill::Armor, Skill::Haste, Skill::Slow, Skill::Earthquake,
    Skill::CharmPerson, Skill::LocateObject, Skill::Refresh, Skill::Summon,
    Skill::SenseLife,
    Skill::Dodge, Skill::Parry, Skill::Rescue,
    Skill::LightningBolt, Skill::Fireball, Skill::ShockingGrasp,
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
    /// Recurring damage per combat tick (Poison etc).  Zero for buffs.
    pub dot_damage:    i32,
    /// AC bonus while active (positive = better defense, summed into
    /// total_ac alongside armor's value[0]).  Used by Armor spell.
    pub to_ac:         i32,
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

/// Body position.  CircleMUD uses an integer code (POS_DEAD=0 …
/// POS_STANDING=8); we use a tighter enum (no separate dead/incap/stun —
/// we represent those via `hp <= 0` and the combat extract path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Sleeping,
    Resting,
    Sitting,
    Standing,
    Fighting,
}

impl Position {
    /// Persistence key written to the .plr file.
    pub fn save_key(self) -> &'static str {
        match self {
            Position::Sleeping => "sleeping",
            Position::Resting  => "resting",
            Position::Sitting  => "sitting",
            Position::Standing => "standing",
            Position::Fighting => "standing",  // re-anchor to standing on load
        }
    }

    pub fn parse(s: &str) -> Option<Position> {
        match s.to_ascii_lowercase().as_str() {
            "sleeping" | "sleep" => Some(Position::Sleeping),
            "resting"  | "rest"  => Some(Position::Resting),
            "sitting"  | "sit"   => Some(Position::Sitting),
            "standing" | "stand" => Some(Position::Standing),
            "fighting" | "fight" => Some(Position::Fighting),
            _ => None,
        }
    }

    /// Regen multiplier (HP/mana/movement gained per non-combat tick is
    /// scaled by this).  Sleep is fastest, fighting blocks regen.
    pub fn regen_factor(self) -> i32 {
        match self {
            Position::Sleeping => 4,
            Position::Resting  => 2,
            Position::Sitting  => 1,
            Position::Standing => 1,
            Position::Fighting => 0,
        }
    }

    /// Short verb shown in look ("is standing here.", "is sleeping here.").
    pub fn room_verb(self) -> &'static str {
        match self {
            Position::Sleeping => "is sleeping here",
            Position::Resting  => "is resting here",
            Position::Sitting  => "is sitting here",
            Position::Standing => "is standing here",
            Position::Fighting => "is here, fighting",
        }
    }
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
    /// Gold on deposit at the bank (separate from carried `gold`).
    /// Persisted across sessions.
    pub bank_gold:    i64,
    pub exp:          i64,
    pub hp:           i32,
    pub max_hp:       i32,
    pub mana:         i32,
    pub max_mana:     i32,
    pub movement:     i32,
    pub max_movement: i32,
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
    pub position:     Position,
    /// Learned skill levels — value is "practice percent" (0..=100).  Only
    /// skills the player has invested in appear here.
    pub skills:       HashMap<Skill, u8>,
    /// Active temporary affects (buffs/debuffs).  Not persisted across
    /// sessions.
    pub affects:      Vec<Affect>,
    /// Stealth flags.  `sneaking` suppresses leave/arrive broadcasts on
    /// movement; `hidden` removes the character from the room listing.
    /// Both break on the next overt action (attack/cast/say).
    pub sneaking:     bool,
    pub hidden:       bool,
    /// Active quest the character is currently working on (the quest's vnum).
    pub active_quest: Option<i32>,
    /// Per-active-quest progress counter (kill counter for AQ_MOB_KILL,
    /// 0/1 for one-shots like AQ_OBJ_FIND).
    pub quest_progress: i32,
    /// Vnums of quests this character has already completed.  Used for
    /// prereq checks and to prevent re-collecting one-shot rewards.
    pub completed_quests: Vec<i32>,
    /// Vanity title shown after the name on `who` and `score`.  Empty
    /// for new characters; trimmed and length-capped on set.
    pub title:        String,
    /// Hours of food remaining. Counts down each game-hour tick;
    /// reaching 0 starts deducting HP. `-1` is the "never hungry"
    /// sentinel used by immortals and applied by certain food affects.
    pub hunger:       i32,
    /// Hours of drink remaining.  Same sentinel semantics as hunger.
    pub thirst:       i32,
    /// Accumulated hitroll bonus from worn equipment's APPLY_HITROLL.
    /// Applied by `apply_obj_affects` on wear and rolled back on remove.
    pub bonus_hitroll: i32,
    /// Accumulated damroll bonus from worn equipment's APPLY_DAMROLL.
    pub bonus_damroll: i32,
    /// Accumulated AC bonus from APPLY_AC modifiers on worn equipment.
    /// Added to `total_ac` alongside the armor's value[0].
    pub bonus_ac:      i32,
    /// Character id of the leader this character is currently following,
    /// or `None` if they aren't following anyone. Set by `follow`,
    /// cleared by `follow self` or by the leader logging off.
    pub following:    Option<u32>,
    /// Whether this character is in a formal group with their leader (or,
    /// for a leader, with their followers). `group` toggles individual
    /// followers in/out; ungrouped followers tag along on movement but
    /// don't share XP and don't see `gtell`.
    pub grouped:      bool,
    /// Personal toggle: if true, this character will neither send nor
    /// receive `gossip` channel traffic.  Not persisted across sessions.
    pub gossip_off:   bool,
    /// Personal toggle for the auction channel (same semantics as
    /// gossip_off).  Not persisted.
    pub auction_off:  bool,
    /// Personal toggle for the wiznet (immortal-only) channel.  Not
    /// persisted.  Only meaningful when `level >= LVL_IMMORT`.
    pub wiznet_off:   bool,
    /// When true, suppress the multi-line room description on every
    /// look/move and only show the room name + exits + contents.
    /// Transient (not persisted).
    pub brief:        bool,
    /// When true, the dispatcher emits a single-line prompt ("> ")
    /// rather than the default "\r\n> " between responses.  Transient.
    pub compact:      bool,
    /// Name of the most recent player who `tell`ed this character;
    /// `reply` routes its message back here.  Transient.
    pub last_tell_from: Option<String>,
    /// Custom prompt format string (empty = legacy "> ").  Placeholders:
    /// %h/%H HP/maxHP, %m/%M mana/maxMana, %g gold, %x exp, %% literal.
    pub prompt_format: String,
    /// Per-character command aliases.  First-word expansion only — the
    /// dispatcher swaps the first whitespace token for the expansion
    /// before the verb resolution.  Persisted.
    pub aliases:      HashMap<String, String>,
    /// Personal notes (free-form strings).  Capped at 50 entries, each
    /// 200 chars max.  Persisted as one `Note: ...` line per entry.
    pub notes:        Vec<String>,
    /// Pose — appended to "X is here, ..." in render_room.  Persisted.
    pub pose:         String,
    /// PvP opt-in.  When false, the player can neither be attacked by
    /// nor attack another player.  Transient.
    pub pvp_ok:       bool,
    /// Immortal invisibility level.  0 = visible to everyone; N > 0
    /// hides this character from anyone whose own level is &lt; N.
    /// Transient (cleared on reboot).
    pub invis_level:  i32,
    /// Name of the deity this character worships (empty for none).
    /// Persisted.  Cosmetic only at the moment.
    pub god:          String,
    /// Muted: cannot use chat channels (say/tell/gossip/auction/...).
    /// Set by `mute` immortal command.  Persisted.
    pub muted:        bool,
    /// Frozen: dispatch_command refuses every verb except quit/look/score.
    /// Set by `freeze` immortal command.  Persisted.
    pub frozen:       bool,
    /// AFK status. `None` = present. `Some(msg)` = away with the given
    /// reason; tells to this character auto-reply with `msg`. Transient.
    pub afk_msg:      Option<String>,
    /// Auto-flee HP threshold.  Zero disables.  When HP drops below
    /// this value during combat, the combat tick triggers a flee.
    pub wimpy:        i32,
    /// Personal toggle for the info (newbie) channel.
    pub info_off:     bool,
    /// Personal toggle for the shout (zone) channel.
    pub shout_off:    bool,
    /// When true, color codes are stripped before sending to the client
    /// (mirrors CircleMUD's COLOR_OFF preference).
    pub color_off:    bool,
    /// Compact exit display under each room title (vs the verbose
    /// "Obvious exits:" line).
    pub autoexit:     bool,
    /// Auto-take items from a freshly-killed mob's corpse.
    pub autoloot:     bool,
    /// Auto-attack any mob that is fighting the leader you follow.
    pub autoassist:   bool,
    /// Timestamp of the last command this player dispatched.  Refreshed
    /// at the top of `dispatch_command`.  Used by `spawn_idle_kick_tick`
    /// to disconnect long-idle mortals.  Not persisted.
    pub last_activity: std::time::Instant,
}

impl Character {
    /// Reveal: drop both stealth flags.  Called from any overt action.
    /// Returns true if we *were* hidden (caller uses this to broadcast
    /// "X steps out of the shadows").
    pub fn reveal(&mut self) -> bool {
        let was_hidden = self.hidden;
        self.sneaking = false;
        self.hidden   = false;
        was_hidden
    }
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

/// STR-based carry weight cap.  Mirrors str_app[].carry_w in constants.c
/// (the third column).  Clamped lookup at the table edges.
pub fn str_carry_cap(str_score: i32) -> i32 {
    static CARRY: &[i32] = &[
        // 0  1  2  3  4  5  6  7  8  9
           0, 3, 3, 10, 25, 55, 80, 90,100,100,
        //10 11 12 13 14 15 16 17 18 19
          115,115,140,140,170,170,195,220,255,640,
        //20 21 22 23 24 25
          700,810,970,1130,1440,1750,
    ];
    let i = str_score.clamp(0, (CARRY.len() - 1) as i32) as usize;
    CARRY[i]
}

/// DEX-based to-hit bonus — mirrors dex_app[].reaction in constants.c.
/// Positive means the attacker is more likely to land a hit.  Used by
/// `resolve_player_attack`'s hit roll.
pub fn dex_hit_bonus(dex_score: i32) -> i32 {
    static REACTION: &[i32] = &[
        // 0  1  2  3  4  5  6  7  8  9
          -7,-6,-4,-3,-2,-1, 0, 0, 0, 0,
        // 10 11 12 13 14 15 16 17 18 19
           0, 0, 0, 0, 0, 0, 1, 2, 2, 3,
        // 20 21 22 23 24 25
           3, 4, 4, 4, 5, 5,
    ];
    let i = dex_score.clamp(0, (REACTION.len() - 1) as i32) as usize;
    REACTION[i]
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

    /// Sum of AC bonuses from active affects (Armor spell etc).
    pub fn affect_ac_bonus(&self) -> i32 {
        self.affects.iter().map(|a| a.to_ac).sum()
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

    /// Return the canonical class title for the given (class, level).
    /// Tracks the stock CircleMUD `class_titles[]` tables from class.c
    /// in spirit (we collapse the long banded tables to a single
    /// representative title per ~5-level band).
    pub fn default_title_for(class: crate::players::Class, level: i32) -> &'static str {
        use crate::players::Class;
        match class {
            Class::Warrior => match level {
                ..=4   => "the Warrior",
                5..=9  => "the Soldier",
                10..=14 => "the Veteran",
                15..=19 => "the Champion",
                20..=24 => "the Knight",
                25..=29 => "the Hero",
                30..=33 => "the Lord",
                _       => "the Immortal Warrior",
            },
            Class::Cleric => match level {
                ..=4   => "the Believer",
                5..=9  => "the Acolyte",
                10..=14 => "the Priest",
                15..=19 => "the Cardinal",
                20..=24 => "the Bishop",
                25..=29 => "the High Priest",
                30..=33 => "the Patriarch",
                _       => "the Immortal Cleric",
            },
            Class::Thief => match level {
                ..=4   => "the Pickpocket",
                5..=9  => "the Rogue",
                10..=14 => "the Burglar",
                15..=19 => "the Cutpurse",
                20..=24 => "the Shadow",
                25..=29 => "the Assassin",
                30..=33 => "the Master Thief",
                _       => "the Immortal Thief",
            },
            Class::MagicUser => match level {
                ..=4   => "the Apprentice of Magic",
                5..=9  => "the Spell Student",
                10..=14 => "the Scholar of Magic",
                15..=19 => "the Mage",
                20..=24 => "the Sorcerer",
                25..=29 => "the Conjurer",
                30..=33 => "the Arch-Mage",
                _       => "the Immortal Mage",
            },
            Class::Undefined => "the Adventurer",
        }
    }

    /// Apply level-up effects if the character has enough XP. Returns the
    /// number of levels gained (0 if none).
    pub fn check_level_up(&mut self) -> i32 {
        let mut gained = 0;
        // Snapshot the class's current default title so we can detect
        // whether the user is still on the auto-title (and update) vs.
        // a custom title (which we never overwrite).
        let prev_default = Self::default_title_for(self.class, self.level);
        let title_is_auto = self.title.is_empty() || self.title == prev_default;
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
        // Re-apply the auto-title at the new level (only when the user
        // was on an auto-title; custom titles are preserved).
        if gained > 0 && title_is_auto {
            self.title = Self::default_title_for(self.class, self.level).to_string();
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
