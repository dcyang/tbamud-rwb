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
    DetectAlign,
    DetectPoison,
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
    Invisibility,
    Stoneskin,
    Disarm,
    CureSerious,
    Heal,
    Infravision,
    /// Flag affect: mob skips its next attack swing.  Applied by a
    /// successful Bash hit.  No class entry (internal only).
    Stun,
    ColorSpray,
    AcidBlast,
    ChillTouch,
    Enchant,
    /// High-tier Cleric spell — full HP + mana + movement restore AND
    /// strip every negative affect (poison/sleep/blind/slow/charm).
    Restoration,
    /// Buff spell — grants flight: cross deep-water/no-swim sectors and
    /// move at a flat low movement cost.
    Fly,
    /// Cleric attack spell — calls down a lightning bolt; only works
    /// outdoors during rain or a thunderstorm.
    CallLightning,
    /// Utility spell — fill a held drink container with water.
    CreateWater,
    /// Debuff spell — curses a mob (saps its damage).
    Curse,
    /// Cure spell — strips a Curse affect from self or a player.
    RemoveCurse,
    /// Offensive utility — strips a beneficial affect from a mob.
    DispelMagic,
    /// Cleric attack spells — smite a mob of the opposing alignment.
    DispelEvil,
    DispelGood,
    /// MagicUser attack spell — drains life energy from a mob.
    EnergyDrain,
    /// Warrior physical skill — strike every mob in the room.
    Whirlwind,
    /// Cleric/MagicUser self-buff — ward against evil creatures.
    ProtFromEvil,
    /// Cleric self-buff — walk across water unhindered.
    Waterwalk,
    /// Cleric utility — conjure a mushroom into the caster's hands.
    CreateFood,
    /// MagicUser utility — random teleport to another room.
    Teleport,
    /// MagicUser prank — throw the caster's voice to the room.
    Ventriloquate,
    /// Cleric utility — blanket the room in magical darkness.
    Darkness,
    /// Cleric utility — change the weather toward fair or foul.
    ControlWeather,
    /// Group spells — apply to every group member in the caster's room.
    GroupHeal,
    GroupArmor,
    GroupRecall,
    /// Necromancy — raise a zombie from a corpse as a charmed servant.
    AnimateDead,
    /// MagicUser — conjure a charmed duplicate of the caster.
    Clone,
    /// Thief physical skill — glance at a target's carried inventory.
    Peek,
    /// Warrior/Thief physical skill — befriend a weaker creature, turning
    /// it into a charmed pet (non-magical path to pets + mounts).
    Tame,

    // ---- D&D 5e class signature abilities (one per class, exact-class
    // gated — see `is_signature` / `is_class_allowed`). ----
    /// Barbarian — primal fury self-buff (+damage, damage resistance).
    Rage,
    /// Bard — inspire an ally (grants +hit/+dam for a while).
    BardicInspiration,
    /// Cleric — Channel Divinity: searing radiance that routs evil foes.
    TurnUndead,
    /// Druid — Wild Shape: beast form (tougher hide + fiercer blows).
    WildShape,
    /// Fighter — Second Wind: catch your breath and recover HP.
    SecondWind,
    /// Monk — Flurry of Blows: a burst of extra unarmed strikes.
    FlurryOfBlows,
    /// Paladin — Lay on Hands: channel divine power to heal + cure poison.
    LayOnHands,
    /// Ranger — Hunter's Mark: mark your quarry for extra damage.
    HuntersMark,
    /// Rogue — Sneak Attack: bonus damage when hidden or flanking (passive).
    SneakAttack,
    /// Sorcerer — Innate Sorcery: self-buff that empowers your spells.
    InnateSorcery,
    /// Warlock — Eldritch Blast: signature beams of crackling force.
    EldritchBlast,
    /// Wizard — Arcane Recovery: meditate to recover spent mana.
    ArcaneRecovery,
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
            "detectalignment" | "detectalign" => Some(Skill::DetectAlign),
            "detectpoison"                    => Some(Skill::DetectPoison),
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
            "invisibility" | "invis"          => Some(Skill::Invisibility),
            "stoneskin"                       => Some(Skill::Stoneskin),
            "disarm"                          => Some(Skill::Disarm),
            "cureserious" | "cureseriouswounds" => Some(Skill::CureSerious),
            "heal"                            => Some(Skill::Heal),
            "infravision" | "infra"           => Some(Skill::Infravision),
            "stun"                            => Some(Skill::Stun),
            "fly" | "flight"                  => Some(Skill::Fly),
            "rage"                            => Some(Skill::Rage),
            "bardicinspiration" | "inspire"   => Some(Skill::BardicInspiration),
            "turnundead" | "channeldivinity" | "turn" => Some(Skill::TurnUndead),
            "wildshape" | "shift"             => Some(Skill::WildShape),
            "secondwind"                      => Some(Skill::SecondWind),
            "flurryofblows" | "flurry"        => Some(Skill::FlurryOfBlows),
            "layonhands" | "layhands"         => Some(Skill::LayOnHands),
            "huntersmark" | "huntermark" | "mark" => Some(Skill::HuntersMark),
            "sneakattack"                     => Some(Skill::SneakAttack),
            "innatesorcery" | "innate"        => Some(Skill::InnateSorcery),
            "eldritchblast" | "eldritch" | "blast" => Some(Skill::EldritchBlast),
            "arcanerecovery" | "meditate" | "recover" => Some(Skill::ArcaneRecovery),
            "calllightning" | "calllightening" => Some(Skill::CallLightning),
            "createwater" | "createspring" => Some(Skill::CreateWater),
            "curse"                           => Some(Skill::Curse),
            "removecurse"                     => Some(Skill::RemoveCurse),
            "dispelmagic"                     => Some(Skill::DispelMagic),
            "dispelevil"                      => Some(Skill::DispelEvil),
            "dispelgood"                      => Some(Skill::DispelGood),
            "energydrain"                     => Some(Skill::EnergyDrain),
            "whirlwind"                       => Some(Skill::Whirlwind),
            "protfromevil" | "protectionfromevil" => Some(Skill::ProtFromEvil),
            "waterwalk"                       => Some(Skill::Waterwalk),
            "createfood"                      => Some(Skill::CreateFood),
            "teleport"                        => Some(Skill::Teleport),
            "ventriloquate"                   => Some(Skill::Ventriloquate),
            "darkness"                        => Some(Skill::Darkness),
            "controlweather"                  => Some(Skill::ControlWeather),
            "groupheal"                       => Some(Skill::GroupHeal),
            "grouparmor"                      => Some(Skill::GroupArmor),
            "grouprecall"                     => Some(Skill::GroupRecall),
            "animatedead"                     => Some(Skill::AnimateDead),
            "clone"                           => Some(Skill::Clone),
            "peek"                            => Some(Skill::Peek),
            "tame"                            => Some(Skill::Tame),
            "colorspray" | "color"            => Some(Skill::ColorSpray),
            "acidblast" | "acid"              => Some(Skill::AcidBlast),
            "chilltouch" | "chill"            => Some(Skill::ChillTouch),
            "enchant" | "enchantweapon"       => Some(Skill::Enchant),
            "restoration" | "restore"         => Some(Skill::Restoration),
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
            Skill::DetectAlign  => "detect alignment",
            Skill::DetectPoison => "detect poison",
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
            Skill::Invisibility  => "invisibility",
            Skill::Stoneskin     => "stoneskin",
            Skill::Disarm        => "disarm",
            Skill::CureSerious   => "cure serious",
            Skill::Heal          => "heal",
            Skill::Infravision   => "infravision",
            Skill::Stun          => "stun",
            Skill::ColorSpray    => "color spray",
            Skill::AcidBlast     => "acid blast",
            Skill::ChillTouch    => "chill touch",
            Skill::Enchant       => "enchant weapon",
            Skill::Restoration   => "restoration",
            Skill::Fly           => "fly",
            Skill::CallLightning => "call lightning",
            Skill::CreateWater   => "create water",
            Skill::Curse         => "curse",
            Skill::RemoveCurse   => "remove curse",
            Skill::DispelMagic   => "dispel magic",
            Skill::DispelEvil    => "dispel evil",
            Skill::DispelGood    => "dispel good",
            Skill::EnergyDrain   => "energy drain",
            Skill::Whirlwind     => "whirlwind",
            Skill::ProtFromEvil  => "protection from evil",
            Skill::Waterwalk     => "waterwalk",
            Skill::CreateFood    => "create food",
            Skill::Teleport      => "teleport",
            Skill::Ventriloquate => "ventriloquate",
            Skill::Darkness      => "darkness",
            Skill::ControlWeather => "control weather",
            Skill::GroupHeal     => "group heal",
            Skill::GroupArmor    => "group armor",
            Skill::GroupRecall   => "group recall",
            Skill::AnimateDead   => "animate dead",
            Skill::Clone         => "clone",
            Skill::Peek          => "peek",
            Skill::Tame          => "tame",
            Skill::Rage              => "rage",
            Skill::BardicInspiration => "bardic inspiration",
            Skill::TurnUndead        => "turn undead",
            Skill::WildShape         => "wild shape",
            Skill::SecondWind        => "second wind",
            Skill::FlurryOfBlows     => "flurry of blows",
            Skill::LayOnHands        => "lay on hands",
            Skill::HuntersMark       => "hunter's mark",
            Skill::SneakAttack       => "sneak attack",
            Skill::InnateSorcery     => "innate sorcery",
            Skill::EldritchBlast     => "eldritch blast",
            Skill::ArcaneRecovery    => "arcane recovery",
        }
    }

    pub fn kind(self) -> SkillKind {
        match self {
            Skill::Kick | Skill::Bash | Skill::Backstab | Skill::PickLock
                | Skill::Sneak | Skill::Hide | Skill::Steal
                | Skill::Dodge | Skill::Parry | Skill::Rescue | Skill::Disarm
                | Skill::Peek | Skill::Tame | Skill::Whirlwind
                | Skill::Rage | Skill::WildShape | Skill::SecondWind
                | Skill::FlurryOfBlows | Skill::LayOnHands | Skill::SneakAttack
                | Skill::ArcaneRecovery
                | Skill::Stun => SkillKind::Physical,
            Skill::MagicMissile | Skill::CureLight
                | Skill::Bless  | Skill::BurningHands
                | Skill::Sanctuary | Skill::Harm
                | Skill::WordOfRecall | Skill::Identify
                | Skill::DetectInvis  | Skill::DetectMagic
                | Skill::DetectAlign  | Skill::DetectPoison
                | Skill::Poison       | Skill::Sleep | Skill::Blindness
                | Skill::CurePoison   | Skill::CureBlind | Skill::CureCritic
                | Skill::Strength     | Skill::Armor | Skill::Haste | Skill::Slow
                | Skill::Earthquake   | Skill::CharmPerson | Skill::LocateObject
                | Skill::Refresh      | Skill::Summon       | Skill::SenseLife
                | Skill::LightningBolt | Skill::Fireball | Skill::ShockingGrasp
                | Skill::Invisibility  | Skill::Stoneskin
                | Skill::CureSerious   | Skill::Heal
                | Skill::Infravision   | Skill::ColorSpray
                | Skill::AcidBlast     | Skill::ChillTouch
                | Skill::Enchant       | Skill::Restoration
                | Skill::Fly
                | Skill::CallLightning
                | Skill::CreateWater
                | Skill::Curse | Skill::RemoveCurse | Skill::DispelMagic
                | Skill::DispelEvil | Skill::DispelGood
                | Skill::EnergyDrain
                | Skill::ProtFromEvil | Skill::Waterwalk | Skill::CreateFood
                | Skill::Teleport | Skill::Ventriloquate | Skill::Darkness
                | Skill::ControlWeather
                | Skill::GroupHeal | Skill::GroupArmor | Skill::GroupRecall
                | Skill::AnimateDead | Skill::Clone
                | Skill::BardicInspiration | Skill::TurnUndead
                | Skill::HuntersMark | Skill::InnateSorcery
                | Skill::EldritchBlast
                                      => SkillKind::Spell,
        }
    }

    /// Mana cost when invoking this skill.  Zero for physical skills.
    pub fn mana_cost(self) -> i32 {
        match self {
            Skill::Kick | Skill::Bash | Skill::Backstab | Skill::PickLock
                | Skill::Sneak | Skill::Hide | Skill::Steal
                | Skill::Dodge | Skill::Parry | Skill::Rescue | Skill::Disarm
                | Skill::Peek | Skill::Tame | Skill::Whirlwind
                | Skill::Rage | Skill::WildShape | Skill::SecondWind
                | Skill::FlurryOfBlows | Skill::LayOnHands | Skill::SneakAttack
                | Skill::ArcaneRecovery
                | Skill::Stun => 0,
            Skill::EldritchBlast     => 5,
            Skill::HuntersMark       => 8,
            Skill::BardicInspiration => 10,
            Skill::TurnUndead        => 15,
            Skill::InnateSorcery     => 20,
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
            Skill::DetectAlign  => 6,
            Skill::DetectPoison => 6,
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
            Skill::Invisibility  => 12,
            Skill::Stoneskin     => 30,
            Skill::CureSerious   => 16,
            Skill::Heal          => 35,
            Skill::Infravision   => 6,
            Skill::ColorSpray    => 18,
            Skill::AcidBlast     => 25,
            Skill::ChillTouch    => 10,
            Skill::Enchant       => 60,
            Skill::Restoration   => 80,
            Skill::Fly           => 10,
            Skill::CallLightning => 15,
            Skill::CreateWater   => 5,
            Skill::Curse         => 12,
            Skill::RemoveCurse   => 12,
            Skill::DispelMagic   => 15,
            Skill::DispelEvil    => 15,
            Skill::DispelGood    => 15,
            Skill::EnergyDrain   => 35,
            Skill::ProtFromEvil  => 10,
            Skill::Waterwalk     => 10,
            Skill::CreateFood    => 5,
            Skill::Teleport      => 25,
            Skill::Ventriloquate => 5,
            Skill::Darkness      => 8,
            Skill::ControlWeather => 25,
            Skill::GroupHeal     => 40,
            Skill::GroupArmor    => 20,
            Skill::GroupRecall   => 25,
            Skill::AnimateDead   => 35,
            Skill::Clone         => 80,
        }
    }

    /// Which classes can learn this skill.
    pub fn allowed_classes(self) -> &'static [Class] {
        match self {
            Skill::Kick         => &[Class::Fighter, Class::Rogue, Class::Cleric],
            Skill::Bash         => &[Class::Fighter],
            Skill::Backstab     => &[Class::Rogue],
            Skill::PickLock     => &[Class::Rogue],
            Skill::MagicMissile => &[Class::Wizard],
            Skill::CureLight    => &[Class::Cleric],
            Skill::Bless        => &[Class::Cleric],
            Skill::BurningHands => &[Class::Wizard],
            Skill::Sanctuary    => &[Class::Cleric],
            Skill::Harm         => &[Class::Cleric],
            Skill::Sneak        => &[Class::Rogue],
            Skill::Hide         => &[Class::Rogue],
            Skill::Steal        => &[Class::Rogue],
            // Word of recall is Cleric-only here, but in CircleMUD it's
            // shared between Cleric and MagicUser (and trivially castable
            // by all in many forks).  Keep Cleric-only for now.
            Skill::WordOfRecall => &[Class::Cleric, Class::Wizard],
            Skill::Identify     => &[Class::Wizard],
            Skill::DetectInvis  => &[Class::Wizard, Class::Cleric],
            Skill::DetectMagic  => &[Class::Wizard, Class::Cleric],
            Skill::DetectAlign  => &[Class::Cleric],
            Skill::DetectPoison => &[Class::Wizard, Class::Cleric],
            Skill::Poison       => &[Class::Wizard, Class::Cleric],
            Skill::Sleep        => &[Class::Wizard],
            Skill::Blindness    => &[Class::Wizard, Class::Cleric],
            Skill::CurePoison   => &[Class::Cleric],
            Skill::CureBlind    => &[Class::Cleric],
            Skill::CureCritic   => &[Class::Cleric],
            Skill::Strength     => &[Class::Wizard],
            Skill::Armor        => &[Class::Cleric],
            Skill::Haste        => &[Class::Wizard],
            Skill::Slow         => &[Class::Wizard],
            Skill::Earthquake   => &[Class::Wizard, Class::Cleric],
            Skill::CharmPerson  => &[Class::Wizard],
            Skill::LocateObject => &[Class::Wizard, Class::Cleric],
            Skill::Refresh      => &[Class::Cleric],
            Skill::Summon       => &[Class::Wizard],
            Skill::SenseLife    => &[Class::Cleric, Class::Wizard],
            Skill::Dodge        => &[Class::Fighter, Class::Rogue],
            Skill::Parry        => &[Class::Fighter],
            Skill::Rescue       => &[Class::Fighter, Class::Cleric],
            Skill::LightningBolt => &[Class::Wizard],
            Skill::Fireball      => &[Class::Wizard],
            Skill::ShockingGrasp => &[Class::Wizard],
            Skill::Invisibility  => &[Class::Wizard],
            Skill::Stoneskin     => &[Class::Wizard],
            Skill::Disarm        => &[Class::Fighter, Class::Rogue],
            Skill::CureSerious   => &[Class::Cleric],
            Skill::Heal          => &[Class::Cleric],
            Skill::Infravision   => &[Class::Wizard, Class::Cleric],
            Skill::Stun          => &[],
            Skill::ColorSpray    => &[Class::Wizard],
            Skill::AcidBlast     => &[Class::Wizard],
            Skill::ChillTouch    => &[Class::Wizard],
            Skill::Enchant       => &[Class::Wizard],
            Skill::Restoration   => &[Class::Cleric],
            Skill::Fly           => &[Class::Wizard, Class::Cleric],
            Skill::CallLightning => &[Class::Cleric],
            Skill::CreateWater   => &[Class::Cleric, Class::Wizard],
            Skill::Curse         => &[Class::Cleric, Class::Wizard],
            Skill::RemoveCurse   => &[Class::Cleric],
            Skill::DispelMagic   => &[Class::Wizard, Class::Cleric],
            Skill::DispelEvil    => &[Class::Cleric],
            Skill::DispelGood    => &[Class::Cleric],
            Skill::EnergyDrain   => &[Class::Wizard],
            Skill::Whirlwind     => &[Class::Fighter],
            Skill::ProtFromEvil  => &[Class::Cleric],
            Skill::Waterwalk     => &[Class::Cleric],
            Skill::CreateFood    => &[Class::Cleric],
            Skill::Teleport      => &[Class::Wizard],
            Skill::Ventriloquate => &[Class::Wizard],
            Skill::Darkness      => &[Class::Wizard],
            Skill::ControlWeather => &[Class::Cleric],
            Skill::GroupHeal     => &[Class::Cleric],
            Skill::GroupArmor    => &[Class::Cleric],
            Skill::GroupRecall   => &[Class::Cleric],
            Skill::AnimateDead   => &[Class::Cleric, Class::Wizard],
            Skill::Clone         => &[Class::Wizard],
            Skill::Peek          => &[Class::Rogue],
            Skill::Tame          => &[Class::Fighter, Class::Rogue],
            // Signature abilities — exact leaf class only (see is_signature).
            Skill::Rage              => &[Class::Barbarian],
            Skill::BardicInspiration => &[Class::Bard],
            Skill::TurnUndead        => &[Class::Cleric],
            Skill::WildShape         => &[Class::Druid],
            Skill::SecondWind        => &[Class::Fighter],
            Skill::FlurryOfBlows     => &[Class::Monk],
            Skill::LayOnHands        => &[Class::Paladin],
            Skill::HuntersMark       => &[Class::Ranger],
            Skill::SneakAttack       => &[Class::Rogue],
            Skill::InnateSorcery     => &[Class::Sorcerer],
            Skill::EldritchBlast     => &[Class::Warlock],
            Skill::ArcaneRecovery    => &[Class::Wizard],
        }
    }

    /// True for the 12 D&D class signature abilities. These are gated to an
    /// EXACT leaf class (not inherited via the base archetype), so e.g. Rage
    /// is Barbarian-only and never granted to other Fighter-line classes.
    pub fn is_signature(self) -> bool {
        matches!(self,
            Skill::Rage | Skill::BardicInspiration | Skill::TurnUndead
            | Skill::WildShape | Skill::SecondWind | Skill::FlurryOfBlows
            | Skill::LayOnHands | Skill::HuntersMark | Skill::SneakAttack
            | Skill::InnateSorcery | Skill::EldritchBlast | Skill::ArcaneRecovery)
    }

    pub fn is_class_allowed(self, class: Class) -> bool {
        if self.is_signature() {
            // Signature abilities are exact-class only — never inherited via
            // the base archetype (Rage is Barbarian-only, etc.).
            self.allowed_classes().contains(&class)
        } else {
            // Derived classes (Barbarian, Bard, …) inherit their base
            // archetype's skill access; the table is keyed on the 4 bases.
            self.allowed_classes().contains(&class.base())
        }
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
            Skill::DetectAlign  => "detect-alignment",
            Skill::DetectPoison => "detect-poison",
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
            Skill::Invisibility  => "invisibility",
            Skill::Stoneskin     => "stoneskin",
            Skill::Disarm        => "disarm",
            Skill::CureSerious   => "cure-serious",
            Skill::Heal          => "heal",
            Skill::Infravision   => "infravision",
            Skill::Stun          => "stun",
            Skill::ColorSpray    => "color-spray",
            Skill::AcidBlast     => "acid-blast",
            Skill::ChillTouch    => "chill-touch",
            Skill::Enchant       => "enchant-weapon",
            Skill::Restoration   => "restoration",
            Skill::Fly           => "fly",
            Skill::CallLightning => "call-lightning",
            Skill::CreateWater   => "create-water",
            Skill::Curse         => "curse",
            Skill::RemoveCurse   => "remove-curse",
            Skill::DispelMagic   => "dispel-magic",
            Skill::DispelEvil    => "dispel-evil",
            Skill::DispelGood    => "dispel-good",
            Skill::EnergyDrain   => "energy-drain",
            Skill::Whirlwind     => "whirlwind",
            Skill::ProtFromEvil  => "prot-from-evil",
            Skill::Waterwalk     => "waterwalk",
            Skill::CreateFood    => "create-food",
            Skill::Teleport      => "teleport",
            Skill::Ventriloquate => "ventriloquate",
            Skill::Darkness      => "darkness",
            Skill::ControlWeather => "control-weather",
            Skill::GroupHeal     => "group-heal",
            Skill::GroupArmor    => "group-armor",
            Skill::GroupRecall   => "group-recall",
            Skill::AnimateDead   => "animate-dead",
            Skill::Clone         => "clone",
            Skill::Peek          => "peek",
            Skill::Tame          => "tame",
            Skill::Rage              => "rage",
            Skill::BardicInspiration => "bardic-inspiration",
            Skill::TurnUndead        => "turn-undead",
            Skill::WildShape         => "wild-shape",
            Skill::SecondWind        => "second-wind",
            Skill::FlurryOfBlows     => "flurry-of-blows",
            Skill::LayOnHands        => "lay-on-hands",
            Skill::HuntersMark       => "hunters-mark",
            Skill::SneakAttack       => "sneak-attack",
            Skill::InnateSorcery     => "innate-sorcery",
            Skill::EldritchBlast     => "eldritch-blast",
            Skill::ArcaneRecovery    => "arcane-recovery",
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
    Skill::DetectInvis, Skill::DetectMagic, Skill::DetectAlign, Skill::DetectPoison,
    Skill::Poison, Skill::Sleep, Skill::Blindness,
    Skill::CurePoison, Skill::CureBlind, Skill::CureCritic,
    Skill::Strength, Skill::Armor, Skill::Haste, Skill::Slow, Skill::Earthquake,
    Skill::CharmPerson, Skill::LocateObject, Skill::Refresh, Skill::Summon,
    Skill::SenseLife,
    Skill::Dodge, Skill::Parry, Skill::Rescue,
    Skill::LightningBolt, Skill::Fireball, Skill::ShockingGrasp,
    Skill::Invisibility, Skill::Stoneskin, Skill::Disarm,
    Skill::CureSerious, Skill::Heal, Skill::Infravision,
    Skill::ColorSpray, Skill::AcidBlast, Skill::ChillTouch,
    Skill::Enchant,
    Skill::Restoration,
    Skill::Fly,
    Skill::CallLightning,
    Skill::CreateWater,
    Skill::Curse, Skill::RemoveCurse, Skill::DispelMagic,
    Skill::DispelEvil, Skill::DispelGood,
    Skill::EnergyDrain, Skill::Whirlwind,
    Skill::ProtFromEvil, Skill::Waterwalk, Skill::CreateFood,
    Skill::Teleport, Skill::Ventriloquate, Skill::Darkness, Skill::ControlWeather,
    Skill::GroupHeal, Skill::GroupArmor, Skill::GroupRecall,
    Skill::AnimateDead, Skill::Clone,
    Skill::Peek,
    Skill::Tame,
    // D&D 5e class signatures.
    Skill::Rage, Skill::BardicInspiration, Skill::TurnUndead, Skill::WildShape,
    Skill::SecondWind, Skill::FlurryOfBlows, Skill::LayOnHands, Skill::HuntersMark,
    Skill::SneakAttack, Skill::InnateSorcery, Skill::EldritchBlast, Skill::ArcaneRecovery,
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
    /// Per-day rent recorded when this character last `rent`ed (0 = not
    /// renting).  Accrued cost is charged on the next login.  Persisted.
    pub rent_per_day: i32,
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
    /// Self-set physical description shown when another player `look`s at
    /// this character.  Empty by default; set via `describe` (cp232).
    /// Persisted, control-stripped + length-capped.
    /// Hours of food remaining. Counts down each game-hour tick;
    /// reaching 0 starts deducting HP. `-1` is the "never hungry"
    /// sentinel used by immortals and applied by certain food affects.
    pub hunger:       i32,
    /// Hours of drink remaining.  Same sentinel semantics as hunger.
    pub thirst:       i32,
    /// Intoxication level (0 = sober).  Raised by drinking alcoholic
    /// liquids, decremented one per game-hour tick.  When high enough it
    /// garbles speech.  Transient (resets sober on reboot).
    pub drunk:        i32,
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
    /// No-hassle mode: aggressive and memory-grudge mobs ignore this
    /// character.  Defaults on for immortals at login.  Transient.
    pub nohassle:     bool,
    /// Name of the deity this character worships (empty for none).
    /// Persisted.  Cosmetic only at the moment.
    pub god:          String,
    /// D&D 5e background chosen at creation (Step 2).  Persisted; cosmetic for
    /// now (no ability/feat/skill mechanics attached yet).
    pub background:   String,
    /// D&D 5e species chosen at creation (PHB pp.186–197).  Persisted.  Drives
    /// darkvision, Dwarven Toughness HP, Gnomish Cunning save advantage, and
    /// Halfling Luck; the rest of each species' traits are flavour for now.
    pub species:      crate::players::Species,
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
    /// Personal toggle for the grats (congratulations) channel.
    pub grats_off:    bool,
    /// `norepeat` — suppress the self-echo of your own communication
    /// (you won't see "You say, '...'" etc.).  Transient PRF toggle.
    pub norepeat:     bool,
    /// `notell` — refuse incoming tells.  `nosummon` — refuse being the
    /// target of the summon spell.  Both transient PRF toggles (stock).
    pub notell:       bool,
    pub nosummon:     bool,
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
    /// When true (default), the level-up path auto-updates `title`.
    /// Setting it false freezes the title at whatever the user typed,
    /// even on level transitions.
    pub autotitle:    bool,
    /// Stock auto-prefs (transient): auto-collect gold from corpses,
    /// auto-split looted gold with the group, auto-sacrifice the corpse
    /// after a kill, auto-open closed doors when moving, auto-unlock
    /// locked doors with a held key, and auto-show the mini-map on move.
    pub autogold:     bool,
    pub autosplit:    bool,
    pub autosac:      bool,
    pub autodoor:     bool,
    pub autokey:      bool,
    pub automap:      bool,
    /// Last N dispatched commands (transient).  Recorded at the top of
    /// `dispatch_command`; viewed via `history`.
    pub history:      std::collections::VecDeque<String>,
    /// Active OLC editing session (None = not editing).  While set, all
    /// input is routed to the editor instead of the command interpreter.
    pub olc:          Option<crate::olc::OlcSession>,
    /// Last N received tells (transient).  Recorded at the receiving
    /// end of `do_tell`; viewed via `tells`.
    pub tell_history: std::collections::VecDeque<(String, String)>,
    /// Moral alignment: -1000 (pure evil) → +1000 (pure good).  0 = neutral.
    /// Persisted; thresholds are >350 good, <-350 evil, else neutral.
    pub alignment:    i32,
    /// Clan name (empty = unaffiliated).  Persisted; case is preserved
    /// but membership comparison is case-insensitive.
    pub clan:         String,
    /// Career PvP kill count (this character has killed N players).
    pub pkills:       i32,
    /// Career PvP death count.
    pub pdeaths:      i32,
    /// Player ids currently snooping this character — every line their
    /// writer task drains is also cloned (prefixed) to each snooper's
    /// mpsc.  Transient; cleared on logout.  Multiple snoopers are
    /// allowed.
    pub snooped_by:   Vec<u32>,
    /// Target this character is snooping, if any.  Used by
    /// `do_unsnoop` to clean up the reverse pointer.
    pub snooping:     Option<u32>,
    /// Pending `group invite` source — populated when another player
    /// invites us, consumed by `group accept`.  Transient.
    pub group_invite_from: Option<u32>,
    /// Pending `clan invite` — Some(inviter_id) when someone wants
    /// us in their clan; cleared by accept/decline.  Transient.
    pub clan_invite_from:  Option<u32>,
    /// Timestamp of the last command this player dispatched.  Refreshed
    /// at the top of `dispatch_command`.  Used by `spawn_idle_kick_tick`
    /// to disconnect long-idle mortals.  Not persisted.
    pub last_activity: std::time::Instant,

    /// Earliest Instant at which this player may successfully cast Word
    /// of Recall again.  Prevents recall spam-tele.  Not persisted —
    /// reset on every fresh login (anti-grief is "you logged in fresh,
    /// take a breath").  None = no active cooldown.
    pub recall_cooldown_until: Option<std::time::Instant>,

    /// Ranger Hunter's Mark: the mob instance id currently marked.  While
    /// set, the ranger deals bonus damage to that mob (cp: D&D signatures).
    /// Transient — cleared on a fresh login.
    pub hunters_mark: Option<u32>,
    /// Per-ability cooldown gates for the martial class signatures (Rage,
    /// Second Wind, Flurry of Blows, Lay on Hands, Wild Shape, Arcane
    /// Recovery).  Maps a Skill to the Instant at which it may be used
    /// again.  Transient — reset on a fresh login.
    pub ability_cooldowns: HashMap<Skill, std::time::Instant>,
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

    /// Seconds remaining on an ability's cooldown (0 = ready).
    pub fn ability_cooldown_remaining(&self, skill: Skill) -> u64 {
        match self.ability_cooldowns.get(&skill) {
            Some(&until) => {
                let now = std::time::Instant::now();
                if until > now { (until - now).as_secs() + 1 } else { 0 }
            }
            None => 0,
        }
    }

    /// Start (or refresh) an ability's cooldown for `secs` seconds.
    pub fn set_ability_cooldown(&mut self, skill: Skill, secs: u64) {
        self.ability_cooldowns.insert(
            skill,
            std::time::Instant::now() + std::time::Duration::from_secs(secs),
        );
    }

    /// Bonus damage added to this caster's offensive spells while the
    /// Sorcerer's Innate Sorcery self-buff is active (0 otherwise).
    pub fn spell_power_bonus(&self) -> i32 {
        if self.affects.iter().any(|a| a.skill == Skill::InnateSorcery) {
            (self.level / 2).max(1)
        } else {
            0
        }
    }

    /// True while a Druid's Wild Shape beast form is active.
    pub fn is_wild_shaped(&self) -> bool {
        self.affects.iter().any(|a| a.skill == Skill::WildShape)
    }

    /// This character's current score in the given ability.
    pub fn ability_score(&self, a: Ability) -> i32 {
        match a {
            Ability::Str => self.str_,
            Ability::Dex => self.dex,
            Ability::Con => self.con,
            Ability::Int => self.int_,
            Ability::Wis => self.wis,
            Ability::Cha => self.cha,
        }
    }

    /// D&D proficiency bonus: +2 at level 1, +1 every 4 levels. The PHB only
    /// defines this through level 20 (+6); we continue the same pattern across
    /// the 30-level mortal range (+7 at 21–24, +8 at 25–28, +9 at 29–30).
    pub fn proficiency_bonus(&self) -> i32 {
        (2 + (self.level - 1) / 4).max(2)
    }

    /// Whether this character's class is proficient in `a` saving throws.
    pub fn is_save_proficient(&self, a: Ability) -> bool {
        self.class.save_proficiencies()[a.index()]
    }

    /// Saving-throw bonus for `a`: ability modifier + proficiency bonus
    /// (the latter only if the class is proficient in that save).
    pub fn saving_throw(&self, a: Ability) -> i32 {
        ability_modifier(self.ability_score(a))
            + if self.is_save_proficient(a) { self.proficiency_bonus() } else { 0 }
    }

    /// Whether this character is proficient in the given D&D skill.  Skill
    /// proficiencies currently come from the chosen background (PHB pp.178–185).
    pub fn is_skill_proficient(&self, sk: Skill5e) -> bool {
        crate::players::background_proficiencies(&self.background)
            .map(|(skills, _)| skills.iter().any(|s| s.eq_ignore_ascii_case(sk.name())))
            .unwrap_or(false)
    }

    /// Ability-check bonus for a skill: the governing ability's modifier +
    /// proficiency bonus (when proficient).  This is where background skill
    /// proficiencies plug into the check system.
    pub fn skill_check_bonus(&self, sk: Skill5e) -> i32 {
        ability_modifier(self.ability_score(sk.ability()))
            + if self.is_skill_proficient(sk) { self.proficiency_bonus() } else { 0 }
    }

    /// Whether this character is proficient with the named tool (currently from
    /// the chosen background; e.g. `"Thieves' Tools"`).  Matches the category
    /// prefix so "Gaming Set (one kind)" matches `"Gaming Set"`.
    pub fn is_tool_proficient(&self, tool: &str) -> bool {
        crate::players::background_proficiencies(&self.background)
            .map(|(_, t)| t.eq_ignore_ascii_case(tool)
                || t.to_ascii_lowercase().starts_with(&tool.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    /// Roll a single d20, applying Halfling Luck (reroll a natural 1 once,
    /// keeping the new roll).
    pub fn roll_d20(&self) -> i32 {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let r = rng.gen_range(1..=20);
        if r == 1 && self.species.has_luck() { rng.gen_range(1..=20) } else { r }
    }

    /// A d20 test: `roll + bonus`, with optional advantage (roll twice, keep
    /// the higher).  Each die honours Halfling Luck.
    pub fn roll_d20_test(&self, bonus: i32, advantage: bool) -> i32 {
        let a = self.roll_d20();
        let roll = if advantage { a.max(self.roll_d20()) } else { a };
        roll + bonus
    }

    /// Roll a saving throw vs `dc`: d20 + saving-throw bonus >= dc.  Gnomish
    /// Cunning grants advantage on INT/WIS/CHA saves; Halfling Luck rerolls 1s.
    pub fn roll_saving_throw(&self, a: Ability, dc: i32) -> bool {
        let advantage = self.species.mental_save_advantage()
            && matches!(a, Ability::Int | Ability::Wis | Ability::Cha);
        self.roll_d20_test(self.saving_throw(a), advantage) >= dc
    }

    /// Whether this character can see in the dark — from a darkvision species
    /// trait (PHB) or an active Infravision affect.
    pub fn has_darkvision(&self) -> bool {
        self.species.darkvision() > 0
            || self.affects.iter().any(|a| a.skill == Skill::Infravision)
    }
}

/// The six D&D ability scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ability { Str, Dex, Con, Int, Wis, Cha }

impl Ability {
    /// Index into the STR,DEX,CON,INT,WIS,CHA-ordered arrays
    /// (e.g. `Class::save_proficiencies`).
    pub fn index(self) -> usize {
        match self {
            Ability::Str => 0, Ability::Dex => 1, Ability::Con => 2,
            Ability::Int => 3, Ability::Wis => 4, Ability::Cha => 5,
        }
    }
    pub fn abbr(self) -> &'static str {
        match self {
            Ability::Str => "Str", Ability::Dex => "Dex", Ability::Con => "Con",
            Ability::Int => "Int", Ability::Wis => "Wis", Ability::Cha => "Cha",
        }
    }
    /// All six, in STR,DEX,CON,INT,WIS,CHA order.
    pub const ALL: [Ability; 6] =
        [Ability::Str, Ability::Dex, Ability::Con, Ability::Int, Ability::Wis, Ability::Cha];
}

/// D&D ability modifier: `floor((score - 10) / 2)` (e.g. 10→0, 14→+2, 7→-2).
pub fn ability_modifier(score: i32) -> i32 {
    (score - 10).div_euclid(2)
}

/// The 18 D&D 5e skills (PHB Chapter 1), each governed by an ability score.
/// An ability check with a skill is `d20 + ability modifier + proficiency
/// bonus (if proficient)`.  Background grants supply the proficiencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skill5e {
    Athletics,
    Acrobatics, SleightOfHand, Stealth,
    Arcana, History, Investigation, Nature, Religion,
    AnimalHandling, Insight, Medicine, Perception, Survival,
    Deception, Intimidation, Performance, Persuasion,
}

impl Skill5e {
    pub const ALL: [Skill5e; 18] = [
        Skill5e::Athletics,
        Skill5e::Acrobatics, Skill5e::SleightOfHand, Skill5e::Stealth,
        Skill5e::Arcana, Skill5e::History, Skill5e::Investigation,
        Skill5e::Nature, Skill5e::Religion,
        Skill5e::AnimalHandling, Skill5e::Insight, Skill5e::Medicine,
        Skill5e::Perception, Skill5e::Survival,
        Skill5e::Deception, Skill5e::Intimidation, Skill5e::Performance,
        Skill5e::Persuasion,
    ];

    /// Display name (matches the strings in `players::background_proficiencies`).
    pub fn name(self) -> &'static str {
        match self {
            Skill5e::Athletics       => "Athletics",
            Skill5e::Acrobatics      => "Acrobatics",
            Skill5e::SleightOfHand   => "Sleight of Hand",
            Skill5e::Stealth         => "Stealth",
            Skill5e::Arcana          => "Arcana",
            Skill5e::History         => "History",
            Skill5e::Investigation   => "Investigation",
            Skill5e::Nature          => "Nature",
            Skill5e::Religion        => "Religion",
            Skill5e::AnimalHandling  => "Animal Handling",
            Skill5e::Insight         => "Insight",
            Skill5e::Medicine        => "Medicine",
            Skill5e::Perception      => "Perception",
            Skill5e::Survival        => "Survival",
            Skill5e::Deception       => "Deception",
            Skill5e::Intimidation    => "Intimidation",
            Skill5e::Performance     => "Performance",
            Skill5e::Persuasion      => "Persuasion",
        }
    }

    /// The ability that governs this skill.
    pub fn ability(self) -> Ability {
        match self {
            Skill5e::Athletics => Ability::Str,
            Skill5e::Acrobatics | Skill5e::SleightOfHand | Skill5e::Stealth => Ability::Dex,
            Skill5e::Arcana | Skill5e::History | Skill5e::Investigation
                | Skill5e::Nature | Skill5e::Religion => Ability::Int,
            Skill5e::AnimalHandling | Skill5e::Insight | Skill5e::Medicine
                | Skill5e::Perception | Skill5e::Survival => Ability::Wis,
            Skill5e::Deception | Skill5e::Intimidation | Skill5e::Performance
                | Skill5e::Persuasion => Ability::Cha,
        }
    }

    /// Parse a player-typed skill name (case/space/dash-insensitive).
    pub fn parse(s: &str) -> Option<Skill5e> {
        let n = s.trim().to_ascii_lowercase().replace([' ', '-', '_'], "");
        Skill5e::ALL.iter().copied()
            .find(|sk| sk.name().to_ascii_lowercase().replace([' ', '-'], "") == n)
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

/// Banded alignment label.  Mirrors the CircleMUD thresholds in
/// limits.c so anti-item gates can compare cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignmentBand { Good, Neutral, Evil }

impl AlignmentBand {
    pub fn of(alignment: i32) -> AlignmentBand {
        if alignment >  350 { AlignmentBand::Good }
        else if alignment < -350 { AlignmentBand::Evil }
        else { AlignmentBand::Neutral }
    }
    pub fn name(self) -> &'static str {
        match self {
            AlignmentBand::Good    => "good",
            AlignmentBand::Neutral => "neutral",
            AlignmentBand::Evil    => "evil",
        }
    }
}

impl Character {
    /// Derive starting HP for a brand-new mortal. Immortals (lvl >= 34) get
    /// a much higher pool. Mirrors very loosely what CircleMUD does in
    /// new-character init — exact constants will come with the stat system.
    /// Class-specific HP gain per level. Mirrors the CircleMUD ranges in
    /// constants.c::Class_apply_table[].hit_dice.
    pub fn hp_per_level(class: Class) -> i32 {
        // Tracks D&D 5e hit dice (d12 → 13, d10 → 11-12, d8 → 8-9, d6 → 6),
        // normalised so the four legacy base classes keep their old values
        // (Fighter 12, Cleric 9, Rogue 8, Wizard 6).
        match class {
            Class::Barbarian => 13, // d12
            Class::Fighter   => 12, // d10
            Class::Paladin   => 12, // d10
            Class::Ranger    => 11, // d10
            Class::Cleric    =>  9, // d8
            Class::Druid     =>  9, // d8
            Class::Monk      =>  9, // d8
            Class::Bard      =>  9, // d8
            Class::Rogue     =>  8, // d8 (legacy value)
            Class::Warlock   =>  8, // d8
            Class::Sorcerer  =>  6, // d6
            Class::Wizard    =>  6, // d6
            Class::Undefined =>  8,
        }
    }

    /// Class-specific mana gain per level.  Full casters scale fastest;
    /// half-casters (Paladin/Ranger) get a little (Ranger needs it for
    /// Hunter's Mark); pure martials barely any.
    pub fn mana_per_level(class: Class) -> i32 {
        match class {
            Class::Wizard | Class::Sorcerer => 10,
            Class::Bard                     =>  9,
            Class::Cleric | Class::Druid    =>  8,
            Class::Warlock                  =>  8,
            Class::Ranger                   =>  4,
            Class::Paladin                  =>  3,
            Class::Fighter | Class::Barbarian | Class::Monk | Class::Rogue => 2,
            Class::Undefined                =>  4,
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

    /// True while a Fly affect is active.  Lets the bearer cross deep-water
    /// / no-swim sectors and pay a flat low movement cost (cp209).
    pub fn is_flying(&self) -> bool {
        self.affects.iter().any(|a| a.skill == Skill::Fly)
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
        // Each of the 12 D&D classes has its own banded titles.
        match class {
            Class::Fighter => match level {
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
            Class::Rogue => match level {
                ..=4   => "the Pickpocket",
                5..=9  => "the Rogue",
                10..=14 => "the Burglar",
                15..=19 => "the Cutpurse",
                20..=24 => "the Shadow",
                25..=29 => "the Assassin",
                30..=33 => "the Master Thief",
                _       => "the Immortal Thief",
            },
            Class::Wizard => match level {
                ..=4   => "the Apprentice of Magic",
                5..=9  => "the Spell Student",
                10..=14 => "the Scholar of Magic",
                15..=19 => "the Mage",
                20..=24 => "the Sorcerer",
                25..=29 => "the Conjurer",
                30..=33 => "the Arch-Mage",
                _       => "the Immortal Mage",
            },
            Class::Barbarian => match level {
                ..=4   => "the Brawler",
                5..=9  => "the Berserker",
                10..=14 => "the Reaver",
                15..=19 => "the Marauder",
                20..=24 => "the Ravager",
                25..=29 => "the Warlord",
                30..=33 => "the Chieftain",
                _       => "the Immortal Barbarian",
            },
            Class::Bard => match level {
                ..=4   => "the Busker",
                5..=9  => "the Songsmith",
                10..=14 => "the Minstrel",
                15..=19 => "the Lyrist",
                20..=24 => "the Troubadour",
                25..=29 => "the Loremaster",
                30..=33 => "the Virtuoso",
                _       => "the Immortal Bard",
            },
            Class::Druid => match level {
                ..=4   => "the Initiate",
                5..=9  => "the Wanderer",
                10..=14 => "the Shaper",
                15..=19 => "the Keeper of the Grove",
                20..=24 => "the Warden of the Wild",
                25..=29 => "the Elder",
                30..=33 => "the Archdruid",
                _       => "the Immortal Druid",
            },
            Class::Monk => match level {
                ..=4   => "the Novice",
                5..=9  => "the Disciple",
                10..=14 => "the Adept",
                15..=19 => "the Master",
                20..=24 => "the Grandmaster",
                25..=29 => "the Ascendant",
                30..=33 => "the Enlightened",
                _       => "the Immortal Monk",
            },
            Class::Paladin => match level {
                ..=4   => "the Squire",
                5..=9  => "the Cavalier",
                10..=14 => "the Crusader",
                15..=19 => "the Templar",
                20..=24 => "the Justicar",
                25..=29 => "the Paragon",
                30..=33 => "the Holy Avenger",
                _       => "the Immortal Paladin",
            },
            Class::Ranger => match level {
                ..=4   => "the Tracker",
                5..=9  => "the Strider",
                10..=14 => "the Pathfinder",
                15..=19 => "the Hunter",
                20..=24 => "the Warden",
                25..=29 => "the Beastmaster",
                30..=33 => "the Ranger Lord",
                _       => "the Immortal Ranger",
            },
            Class::Sorcerer => match level {
                ..=4   => "the Awakened",
                5..=9  => "the Channeler",
                10..=14 => "the Adept of Power",
                15..=19 => "the Evoker",
                20..=24 => "the Thaumaturge",
                25..=29 => "the Archsorcerer",
                30..=33 => "the Dragonblood",
                _       => "the Immortal Sorcerer",
            },
            Class::Warlock => match level {
                ..=4   => "the Pactbound",
                5..=9  => "the Cultist",
                10..=14 => "the Invoker",
                15..=19 => "the Occultist",
                20..=24 => "the Channeler of the Pact",
                25..=29 => "the Maledictor",
                30..=33 => "the Patron's Chosen",
                _       => "the Immortal Warlock",
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
            // Class-specific HP gain + CON bonus (+ Dwarven Toughness), heal full.
            let con_bonus = (self.con - 10).max(0) / 2;
            self.max_hp   += Self::hp_per_level(self.class) + con_bonus
                + self.species.hp_bonus_per_level();
            self.hp = self.max_hp;
            // Mana gain: scales with INT for arcane, WIS for divine.
            let casting_stat = match self.class.base() {
                Class::Cleric => self.wis,
                _             => self.int_, // Wizard line + martial
            };
            let stat_bonus = (casting_stat - 10).max(0) / 2;
            self.max_mana += Self::mana_per_level(self.class) + stat_bonus;
            self.mana = self.max_mana;
            // Practice points.
            self.practices += Self::PRACTICES_PER_LEVEL;
            // Bump every class-allowed skill the character has learned
            // by +5% (capped at 100).  Doesn't grant new skills they
            // haven't started practising.
            for (_, pct) in self.skills.iter_mut() {
                *pct = (*pct).saturating_add(5).min(100);
            }
            gained += 1;
        }
        // Re-apply the auto-title at the new level (only when the user
        // was on an auto-title; custom titles are preserved).
        if gained > 0 && title_is_auto && self.autotitle {
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
