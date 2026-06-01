/// Player data system: index, per-character records, file I/O, password hashing.
/// Mirrors players.c and the player-file portions of db.c / utils.c.

use std::{
    ffi::{CStr, CString},
    fs,
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};

// Declare crypt(3) explicitly — libc::crypt is not always re-exported.
// build.rs adds -lcrypt on Linux to pull in the implementation.
extern "C" {
    fn crypt(s: *const libc::c_char, salt: *const libc::c_char) -> *mut libc::c_char;
}

// ---------------------------------------------------------------------------
// Constants (mirrors structs.h / utils.h)
// ---------------------------------------------------------------------------

pub const MAX_NAME_LENGTH: usize = 20;
pub const MAX_PWD_LENGTH: usize = 30;
pub const MAX_BAD_PWS: u8 = 3;

/// PLR_DELETED bit index in PLR_FLAGS[0].  Stored as sprintascii letter 'k' (bit 10).
pub const PLR_DELETED_BIT: u32 = 10;
pub const PLR_DELETED: u32 = 1 << PLR_DELETED_BIT;

/// Player classes — the D&D 5e (2024 PHB) class roster.
///
/// The four base archetypes keep their original CircleMUD/TbaMUD discriminants
/// (0=Wizard/ex-MagicUser, 1=Cleric, 2=Rogue/ex-Thief, 3=Fighter/ex-Warrior) so
/// existing player files load unchanged. The eight additional classes are
/// appended (4–11). Each non-base class delegates its mechanics to a base
/// archetype via [`Class::base`] (see the "A Balanced Party" mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i8)]
pub enum Class {
    #[default]
    Undefined = -1,
    // Base archetypes (legacy discriminants preserved).
    Wizard = 0,
    Cleric = 1,
    Rogue = 2,
    Fighter = 3,
    // Additional D&D 5e classes.
    Barbarian = 4,
    Bard = 5,
    Druid = 6,
    Monk = 7,
    Paladin = 8,
    Ranger = 9,
    Sorcerer = 10,
    Warlock = 11,
}

impl Class {
    pub fn from_i8(v: i8) -> Self {
        match v {
            0 => Self::Wizard,
            1 => Self::Cleric,
            2 => Self::Rogue,
            3 => Self::Fighter,
            4 => Self::Barbarian,
            5 => Self::Bard,
            6 => Self::Druid,
            7 => Self::Monk,
            8 => Self::Paladin,
            9 => Self::Ranger,
            10 => Self::Sorcerer,
            11 => Self::Warlock,
            _ => Self::Undefined,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wizard    => "Wizard",
            Self::Cleric    => "Cleric",
            Self::Rogue     => "Rogue",
            Self::Fighter   => "Fighter",
            Self::Barbarian => "Barbarian",
            Self::Bard      => "Bard",
            Self::Druid     => "Druid",
            Self::Monk      => "Monk",
            Self::Paladin   => "Paladin",
            Self::Ranger    => "Ranger",
            Self::Sorcerer  => "Sorcerer",
            Self::Warlock   => "Warlock",
            Self::Undefined => "Undefined",
        }
    }

    /// The base archetype a class delegates its mechanics to (starting skills,
    /// anti-item flags, HP/mana, attacks, guild rooms, titles). The four base
    /// classes and `Undefined` return themselves. Mapping follows the PHB's
    /// "A Balanced Party" box (logical p.36).
    pub fn base(self) -> Class {
        match self {
            // Fighter line
            Self::Barbarian | Self::Monk | Self::Paladin | Self::Ranger => Self::Fighter,
            // Cleric line
            Self::Druid => Self::Cleric,
            // Wizard line
            Self::Bard | Self::Sorcerer | Self::Warlock => Self::Wizard,
            // Bases (and Undefined) map to themselves.
            other => other,
        }
    }

    /// The 12 player-selectable classes, in login-menu order.
    pub fn selectable() -> &'static [Class] {
        &[
            Self::Barbarian,
            Self::Bard,
            Self::Cleric,
            Self::Druid,
            Self::Fighter,
            Self::Monk,
            Self::Paladin,
            Self::Ranger,
            Self::Rogue,
            Self::Sorcerer,
            Self::Warlock,
            Self::Wizard,
        ]
    }

    /// The class's primary ability/abilities (PHB p.33 Class Overview), as
    /// stat keys ("str"/"dex"/"con"/"int"/"wis"/"cha").  Used to bias a fresh
    /// character's rolled scores so class choice matters at creation.
    pub fn primary_abilities(self) -> &'static [&'static str] {
        match self {
            Self::Barbarian => &["str"],
            Self::Bard      => &["cha"],
            Self::Cleric    => &["wis"],
            Self::Druid     => &["wis"],
            Self::Fighter   => &["str"],          // "Strength or Dexterity"
            Self::Monk      => &["dex", "wis"],
            Self::Paladin   => &["str", "cha"],
            Self::Ranger    => &["dex", "wis"],
            Self::Rogue     => &["dex"],
            Self::Sorcerer  => &["cha"],
            Self::Warlock   => &["cha"],
            Self::Wizard    => &["int"],
            Self::Undefined => &[],
        }
    }

    /// Case-insensitive full-name or unambiguous-prefix match over the 12
    /// selectable classes. Single letters that collide (e.g. "b" → Barbarian
    /// vs Bard, "w" → Warlock vs Wizard) return `None`.
    pub fn parse_name(s: &str) -> Option<Class> {
        let s = s.trim().to_ascii_lowercase();
        if s.is_empty() {
            return None;
        }
        // Exact match first.
        if let Some(&c) = Self::selectable()
            .iter()
            .find(|c| c.as_str().eq_ignore_ascii_case(&s))
        {
            return Some(c);
        }
        // Unambiguous prefix match.
        let matches: Vec<Class> = Self::selectable()
            .iter()
            .copied()
            .filter(|c| c.as_str().to_ascii_lowercase().starts_with(&s))
            .collect();
        if matches.len() == 1 {
            Some(matches[0])
        } else {
            None
        }
    }
}

/// Sex constants (SEX_* in structs.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Sex {
    #[default]
    Neutral = 0,
    Male    = 1,
    Female  = 2,
}

impl Sex {
    pub fn from_u8(v: u8) -> Self {
        match v { 1 => Self::Male, 2 => Self::Female, _ => Self::Neutral }
    }
}

// ---------------------------------------------------------------------------
// Player index
// ---------------------------------------------------------------------------

/// One row in `lib/plrfiles/index`.
/// Format on disk: `<id> <Name> <level> <ascii_flags> <last_login_unix>`
#[derive(Debug, Clone)]
pub struct PlayerIndexEntry {
    pub id:         i64,
    pub name:       String,   // capitalised (e.g. "Mahatma")
    pub level:      i32,
    pub flags:      u32,      // PLR_FLAGS[0]
    pub last_login: i64,
}

// ---------------------------------------------------------------------------
// Player record
// ---------------------------------------------------------------------------

/// Minimal player data needed during login and for saving new characters.
/// Full char_data (stats, equipment, …) will be added as the game logic is ported.
#[derive(Debug, Clone, Default)]
pub struct PlayerRecord {
    pub name:          String,
    pub password_hash: String,
    pub level:         i32,
    pub bad_pws:       u32,
    pub sex:           Sex,
    pub class:         Class,
    pub plr_flags:     u32,    // PLR_FLAGS[0]
    pub id:            i64,
    /// Persisted gameplay state. Zero values are treated as "use defaults"
    /// during login so brand-new characters with all-zero records still get
    /// proper init.
    pub hp:            i32,
    pub max_hp:        i32,
    pub mana:          i32,
    pub max_mana:      i32,
    pub movement:      i32,
    pub max_movement:  i32,
    /// Position name (one of "sleeping"/"resting"/"sitting"/"standing").
    /// Empty means "default" — login should use Standing.
    pub position:      String,
    /// Auto-flee HP threshold; 0 disables.
    pub wimpy:         i32,
    /// Persistent color-off preference (strip ANSI codes on output).
    pub color_off:     bool,
    pub autoexit:      bool,
    pub autoloot:      bool,
    pub autoassist:    bool,
    /// Persisted as `AuTl: 0` when the user opted out; we keep
    /// "default = true" implicit so the saved value 0 means off
    /// and absence means on (the default).
    pub autotitle_off: bool,
    /// Stock auto-prefs (persisted as `AuGd`/`AuSp`/`AuSc`/`AuDr`/`AuKy`/
    /// `AuMp` lines, written only when true; absence means off).
    pub autogold:      bool,
    pub autosplit:     bool,
    pub autosac:       bool,
    pub autodoor:      bool,
    pub autokey:       bool,
    pub automap:       bool,
    /// Moral alignment, range -1000..=1000.  Default 0 (neutral).
    pub alignment:     i32,
    /// Clan affiliation; empty for unaffiliated characters.
    pub clan:          String,
    pub pkills:        i32,
    pub pdeaths:       i32,
    pub practices:     i32,
    pub room:          i32,
    pub gold:          i64,
    /// Gold on deposit at the bank.
    pub bank_gold:     i64,
    /// Per-day rent owed since the player last rented (0 = not renting /
    /// free rent).  Accrued cost is deducted on the next login.
    pub rent_per_day:  i32,
    pub exp:           i64,
    pub str_:          i32,
    pub int_:          i32,
    pub wis:           i32,
    pub dex:           i32,
    pub con:           i32,
    pub cha:           i32,
    /// Skill name → practice percent (0..=100).
    pub skills:        std::collections::HashMap<String, u8>,
    /// Currently-active quest vnum (None if no quest in progress).
    pub active_quest:  Option<i32>,
    /// Progress on the active quest (kill counter, etc).
    pub quest_progress: i32,
    /// Vnums of quests already completed.
    pub completed_quests: Vec<i32>,
    /// Hours of food/drink remaining (-1 = never hungry).  Persisted
    /// across login; the runtime tick decays them in real time.
    pub hunger:        i32,
    pub thirst:        i32,
    /// Vanity title (empty for new chars).
    pub title:         String,
    /// Custom prompt format with %h/%H/%m/%M/%g/%x placeholders.
    /// Empty means use the legacy "> " prompt.
    pub prompt_format: String,
    /// Per-character command aliases (first-word expansion).
    pub aliases:       std::collections::HashMap<String, String>,
    /// Personal note pad.
    pub notes:         Vec<String>,
    /// Pose suffix shown in render_room.
    pub pose:          String,
    /// Unix timestamp of the player's last login (best-effort; overwritten
    /// every auto-save).
    pub last_login:    i64,
    /// Name of the deity the character worships (cosmetic, empty = none).
    pub god:           String,
    pub muted:         bool,
    pub frozen:        bool,
}

impl PlayerRecord {
    pub fn is_deleted(&self) -> bool {
        self.plr_flags & PLR_DELETED != 0
    }
}

// ---------------------------------------------------------------------------
// In-memory player database
// ---------------------------------------------------------------------------

pub struct PlayerDb {
    entries:  Vec<PlayerIndexEntry>,
    next_id:  i64,
    data_dir: String,
}

impl PlayerDb {
    /// The data directory this DB was loaded from (e.g. "lib").
    pub fn data_dir(&self) -> &str { &self.data_dir }

    /// Load the player index from `<data_dir>/plrfiles/index`.
    /// Mirrors build_player_index() in players.c.
    pub fn load(data_dir: &str) -> Result<Self> {
        let index_path = format!("{}/plrfiles/index", data_dir);
        let mut entries  = Vec::new();
        let mut next_id  = 1i64;

        match fs::read_to_string(&index_path) {
            Ok(content) => {
                for line in content.lines() {
                    let line = line.trim();
                    if line.starts_with('~') || line.is_empty() {
                        break;
                    }
                    // "<id> <Name> <level> <ascii_flags> <last>"
                    let parts: Vec<&str> = line.split_ascii_whitespace().collect();
                    if parts.len() < 5 {
                        continue;
                    }
                    let id: i64 = parts[0].parse().unwrap_or(0);
                    let entry = PlayerIndexEntry {
                        id,
                        name:       capitalize(parts[1]),
                        level:      parts[2].parse().unwrap_or(0),
                        flags:      asciiflag_conv(parts[3]),
                        last_login: parts[4].parse().unwrap_or(0),
                    };
                    if id >= next_id {
                        next_id = id + 1;
                    }
                    entries.push(entry);
                }
                tracing::info!(
                    count = entries.len(),
                    "Loaded player index"
                );
            }
            Err(_) => {
                tracing::info!("No player index found — first new character will be implementor");
            }
        }

        Ok(Self { entries, next_id, data_dir: data_dir.to_string() })
    }

    // -------------------------------------------------------------------
    // Index queries
    // -------------------------------------------------------------------

    /// Case-insensitive name lookup.  Returns the index into `entries`.
    pub fn find_by_name(&self, name: &str) -> Option<usize> {
        let lower = name.to_lowercase();
        self.entries.iter().position(|e| e.name.to_lowercase() == lower)
    }

    /// Case-insensitive lookup that returns the canonical capitalized
    /// name (whatever the index file stored).
    pub fn find_name(&self, name: &str) -> Option<String> {
        self.find_by_name(name).map(|i| self.entries[i].name.clone())
    }

    /// Create a new index entry for a freshly-created character and return
    /// the assigned player ID.  Mirrors create_entry() in players.c.
    pub fn create_entry(&mut self, name: &str) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(PlayerIndexEntry {
            id,
            name:       capitalize(name),
            level:      0,
            flags:      0,
            last_login: unix_now(),
        });
        id
    }

    /// Update the cached level/flags for an existing entry after save.
    pub fn update_entry(&mut self, rec: &PlayerRecord) {
        if let Some(e) = self.entries.iter_mut()
            .find(|e| e.name.to_lowercase() == rec.name.to_lowercase())
        {
            e.level = rec.level;
            e.flags = rec.plr_flags;
            e.last_login = unix_now();
        }
    }

    /// Persist the in-memory index to disk.
    pub fn save_index(&self) -> Result<()> {
        let path = format!("{}/plrfiles/index", self.data_dir);
        let mut f = fs::File::create(&path)
            .with_context(|| format!("Cannot write player index {path}"))?;
        for e in &self.entries {
            writeln!(f, "{} {} {} {} {}",
                e.id, e.name, e.level,
                sprintascii(e.flags),
                e.last_login)?;
        }
        writeln!(f, "~")?;
        Ok(())
    }

    // -------------------------------------------------------------------
    // Per-player file I/O
    // -------------------------------------------------------------------

    /// Read the ASCII player file for `name`.
    /// Mirrors load_char() in players.c — handles the "Tag: value" format.
    pub fn load_player(&self, name: &str) -> Result<PlayerRecord> {
        let path = self.player_path(name);
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Cannot read player file {path}"))?;

        let mut rec = PlayerRecord::default();

        for raw_line in content.lines() {
            // Skip lines that don't contain ": "
            let Some((raw_tag, val)) = raw_line.split_once(": ") else { continue };
            let tag = raw_tag.trim();
            let val = val.trim();
            match tag {
                "Name" => rec.name          = val.to_string(),
                "Pass" => rec.password_hash = val.to_string(),
                "Levl" => rec.level         = val.parse().unwrap_or(0),
                "Badp" => rec.bad_pws       = val.parse().unwrap_or(0),
                "Sex"  => rec.sex           = Sex::from_u8(val.parse().unwrap_or(0)),
                "Clas" => rec.class         = Class::from_i8(val.parse().unwrap_or(-1)),
                "Id"   => rec.id            = val.parse().unwrap_or(0),
                "Act"  => {
                    // "Act : <ascii_flags0> <ascii_flags1> <ascii_flags2> <ascii_flags3>"
                    let first = val.split_ascii_whitespace().next().unwrap_or("0");
                    rec.plr_flags = asciiflag_conv(first);
                }
                "Hit"  => {
                    // Stored as "<cur>/<max>"
                    let mut parts = val.split('/');
                    if let Some(p) = parts.next() { rec.hp     = p.trim().parse().unwrap_or(0); }
                    if let Some(p) = parts.next() { rec.max_hp = p.trim().parse().unwrap_or(0); }
                }
                "Mana" => {
                    let mut parts = val.split('/');
                    if let Some(p) = parts.next() { rec.mana     = p.trim().parse().unwrap_or(0); }
                    if let Some(p) = parts.next() { rec.max_mana = p.trim().parse().unwrap_or(0); }
                }
                "Move" => {
                    let mut parts = val.split('/');
                    if let Some(p) = parts.next() { rec.movement     = p.trim().parse().unwrap_or(0); }
                    if let Some(p) = parts.next() { rec.max_movement = p.trim().parse().unwrap_or(0); }
                }
                "Pos"  => rec.position = val.trim().to_string(),
                "Wmpy" => rec.wimpy    = val.parse().unwrap_or(0),
                "ClOf" => rec.color_off = val.parse::<i32>().unwrap_or(0) != 0,
                "AuEx" => rec.autoexit  = val.parse::<i32>().unwrap_or(0) != 0,
                "AuLt" => rec.autoloot  = val.parse::<i32>().unwrap_or(0) != 0,
                "AuAs" => rec.autoassist = val.parse::<i32>().unwrap_or(0) != 0,
                "AuTl" => rec.autotitle_off = val.parse::<i32>().unwrap_or(0) != 0,
                "AuGd" => rec.autogold  = val.parse::<i32>().unwrap_or(0) != 0,
                "AuSp" => rec.autosplit = val.parse::<i32>().unwrap_or(0) != 0,
                "AuSc" => rec.autosac   = val.parse::<i32>().unwrap_or(0) != 0,
                "AuDr" => rec.autodoor  = val.parse::<i32>().unwrap_or(0) != 0,
                "AuKy" => rec.autokey   = val.parse::<i32>().unwrap_or(0) != 0,
                "AuMp" => rec.automap   = val.parse::<i32>().unwrap_or(0) != 0,
                "Algn" => rec.alignment = val.parse().unwrap_or(0),
                "Clan" => rec.clan      = val.to_string(),
                "Pkil" => rec.pkills    = val.parse().unwrap_or(0),
                "Pdth" => rec.pdeaths   = val.parse().unwrap_or(0),
                "Prac" => rec.practices = val.parse().unwrap_or(0),
                "Room" => rec.room = val.parse().unwrap_or(0),
                "Gold" => rec.gold = val.parse().unwrap_or(0),
                "Exp"  => rec.exp  = val.parse().unwrap_or(0),
                "Str"  => rec.str_ = val.parse().unwrap_or(0),
                "Int"  => rec.int_ = val.parse().unwrap_or(0),
                "Wis"  => rec.wis  = val.parse().unwrap_or(0),
                "Dex"  => rec.dex  = val.parse().unwrap_or(0),
                "Con"  => rec.con  = val.parse().unwrap_or(0),
                "Cha"  => rec.cha  = val.parse().unwrap_or(0),
                "Skil" => {
                    // "Skil: <name> <percent>"
                    let mut parts = val.split_ascii_whitespace();
                    if let (Some(name), Some(pct)) = (parts.next(), parts.next()) {
                        if let Ok(p) = pct.parse::<u8>() {
                            rec.skills.insert(name.to_string(), p);
                        }
                    }
                }
                "Qst" => {
                    // "Qst: <vnum> <progress>" — active quest
                    let mut parts = val.split_ascii_whitespace();
                    if let (Some(v), Some(p)) = (parts.next(), parts.next()) {
                        if let (Ok(vn), Ok(pr)) = (v.parse::<i32>(), p.parse::<i32>()) {
                            rec.active_quest = Some(vn);
                            rec.quest_progress = pr;
                        }
                    }
                }
                "Qcmp" => {
                    // "Qcmp: <vnum>" — one entry per completed quest
                    if let Ok(v) = val.parse::<i32>() {
                        rec.completed_quests.push(v);
                    }
                }
                "Hung" => rec.hunger = val.parse().unwrap_or(24),
                "Thst" => rec.thirst = val.parse().unwrap_or(24),
                "Titl" => rec.title  = val.to_string(),
                "Bank" => rec.bank_gold = val.parse().unwrap_or(0),
                "RntD" => rec.rent_per_day = val.parse().unwrap_or(0),
                "Prmt" => rec.prompt_format = val.to_string(),
                "Alis" => {
                    // "Alis: <name> <expansion>"
                    let mut parts = val.splitn(2, char::is_whitespace);
                    if let (Some(name), Some(exp)) = (parts.next(), parts.next()) {
                        rec.aliases.insert(name.to_string(), exp.trim().to_string());
                    }
                }
                "Note" => rec.notes.push(val.to_string()),
                "Pose" => rec.pose = val.to_string(),
                "LLog" => rec.last_login = val.parse().unwrap_or(0),
                "God"  => rec.god  = val.to_string(),
                "Mute" => rec.muted  = val.parse::<i32>().unwrap_or(0) != 0,
                "Frzn" => rec.frozen = val.parse::<i32>().unwrap_or(0) != 0,
                _ => {}
            }
        }
        Ok(rec)
    }

    /// Write the ASCII player file for `rec`.
    /// Mirrors save_char() in players.c — produces the "Tag: value" format.
    pub fn save_player(&self, rec: &PlayerRecord) -> Result<()> {
        let path = self.player_path(&rec.name);

        // Ensure the bucket directory exists
        if let Some(parent) = PathBuf::from(&path).parent() {
            fs::create_dir_all(parent)?;
        }

        let mut f = fs::File::create(&path)
            .with_context(|| format!("Cannot create player file {path}"))?;

        let now = unix_now();

        writeln!(f, "Name: {}", rec.name)?;
        writeln!(f, "Pass: {}", rec.password_hash)?;
        if rec.level != 0 {
            writeln!(f, "Levl: {}", rec.level)?;
        }
        writeln!(f, "Id  : {}", rec.id)?;
        writeln!(f, "Brth: {}", now)?;
        writeln!(f, "Plyd: 0")?;
        writeln!(f, "Last: {}", now)?;
        writeln!(f, "Sex : {}", rec.sex as u8)?;
        writeln!(f, "Clas: {}", rec.class as i8)?;
        if rec.bad_pws != 0 {
            writeln!(f, "Badp: {}", rec.bad_pws)?;
        }
        writeln!(f, "Act : {} 0 0 0", sprintascii(rec.plr_flags))?;
        writeln!(f, "Aff : 0 0 0 0")?;
        writeln!(f, "Pref: 0 0 0 0")?;
        if rec.max_hp > 0 {
            writeln!(f, "Hit : {}/{}", rec.hp, rec.max_hp)?;
        }
        if rec.max_mana > 0 {
            writeln!(f, "Mana: {}/{}", rec.mana, rec.max_mana)?;
        }
        if rec.max_movement > 0 {
            writeln!(f, "Move: {}/{}", rec.movement, rec.max_movement)?;
        }
        if !rec.position.is_empty() && rec.position != "standing" {
            writeln!(f, "Pos : {}", rec.position)?;
        }
        if rec.wimpy > 0 {
            writeln!(f, "Wmpy: {}", rec.wimpy)?;
        }
        if rec.color_off {
            writeln!(f, "ClOf: 1")?;
        }
        if rec.autoexit   { writeln!(f, "AuEx: 1")?; }
        if rec.autoloot   { writeln!(f, "AuLt: 1")?; }
        if rec.autoassist { writeln!(f, "AuAs: 1")?; }
        if rec.autotitle_off { writeln!(f, "AuTl: 1")?; }
        if rec.autogold   { writeln!(f, "AuGd: 1")?; }
        if rec.autosplit  { writeln!(f, "AuSp: 1")?; }
        if rec.autosac    { writeln!(f, "AuSc: 1")?; }
        if rec.autodoor   { writeln!(f, "AuDr: 1")?; }
        if rec.autokey    { writeln!(f, "AuKy: 1")?; }
        if rec.automap    { writeln!(f, "AuMp: 1")?; }
        if rec.alignment != 0 { writeln!(f, "Algn: {}", rec.alignment)?; }
        if !rec.clan.is_empty() { writeln!(f, "Clan: {}", rec.clan)?; }
        if rec.pkills  > 0 { writeln!(f, "Pkil: {}", rec.pkills)?; }
        if rec.pdeaths > 0 { writeln!(f, "Pdth: {}", rec.pdeaths)?; }
        if rec.practices != 0 {
            writeln!(f, "Prac: {}", rec.practices)?;
        }
        if rec.room != 0 {
            writeln!(f, "Room: {}", rec.room)?;
        }
        if rec.gold != 0 {
            writeln!(f, "Gold: {}", rec.gold)?;
        }
        if rec.exp != 0 {
            writeln!(f, "Exp : {}", rec.exp)?;
        }
        if rec.str_ != 0 { writeln!(f, "Str : {}", rec.str_)?; }
        if rec.int_ != 0 { writeln!(f, "Int : {}", rec.int_)?; }
        if rec.wis  != 0 { writeln!(f, "Wis : {}", rec.wis)?;  }
        if rec.dex  != 0 { writeln!(f, "Dex : {}", rec.dex)?;  }
        if rec.con  != 0 { writeln!(f, "Con : {}", rec.con)?;  }
        if rec.cha  != 0 { writeln!(f, "Cha : {}", rec.cha)?;  }
        let mut sk_names: Vec<&String> = rec.skills.keys().collect();
        sk_names.sort();
        for name in sk_names {
            writeln!(f, "Skil: {} {}", name, rec.skills[name])?;
        }
        if let Some(qv) = rec.active_quest {
            writeln!(f, "Qst : {} {}", qv, rec.quest_progress)?;
        }
        for qv in &rec.completed_quests {
            writeln!(f, "Qcmp: {qv}")?;
        }
        writeln!(f, "Hung: {}", rec.hunger)?;
        writeln!(f, "Thst: {}", rec.thirst)?;
        if !rec.title.is_empty() { writeln!(f, "Titl: {}", rec.title)?; }
        if rec.bank_gold > 0     { writeln!(f, "Bank: {}", rec.bank_gold)?; }
        if rec.rent_per_day > 0  { writeln!(f, "RntD: {}", rec.rent_per_day)?; }
        if !rec.prompt_format.is_empty() { writeln!(f, "Prmt: {}", rec.prompt_format)?; }
        let mut anames: Vec<&String> = rec.aliases.keys().collect();
        anames.sort();
        for name in anames {
            writeln!(f, "Alis: {name} {}", rec.aliases[name])?;
        }
        for note in &rec.notes {
            writeln!(f, "Note: {note}")?;
        }
        if !rec.pose.is_empty() { writeln!(f, "Pose: {}", rec.pose)?; }
        if rec.last_login > 0   { writeln!(f, "LLog: {}", rec.last_login)?; }
        if !rec.god.is_empty()  { writeln!(f, "God : {}", rec.god)?; }
        if rec.muted            { writeln!(f, "Mute: 1")?; }
        if rec.frozen           { writeln!(f, "Frzn: 1")?; }

        Ok(())
    }

    // -------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------

    fn player_path(&self, name: &str) -> String {
        let lower = name.to_lowercase();
        let bucket = self.bucket(&lower);
        format!("{}/plrfiles/{}/{}.plr", self.data_dir, bucket, lower)
    }

    /// Path to this player's persisted object file (lib/plrobjs/<B>/<name>.objs).
    pub fn objs_path(&self, name: &str) -> String {
        let lower = name.to_lowercase();
        let bucket = self.bucket(&lower);
        format!("{}/plrobjs/{}/{}.objs", self.data_dir, bucket, lower)
    }

    fn bucket(&self, lower: &str) -> &'static str {
        match lower.chars().next().unwrap_or('a') {
            'a'..='e' => "A-E",
            'f'..='j' => "F-J",
            'k'..='o' => "K-O",
            'p'..='t' => "P-T",
            _         => "U-Z",
        }
    }

    /// Boot-time cleanup of stale stored-object files (mirrors stock
    /// `update_obj_file` / `Crash_clean_file`, gated by the `-q` flag).
    /// For each indexed player idle longer than the applicable timeout —
    /// `RENT_FILE_TIMEOUT` real-days if their record shows they rented
    /// (rent_per_day > 0), else `CRASH_FILE_TIMEOUT` — delete their
    /// `.objs` file so abandoned belongings don't persist forever.
    /// Returns the number of files removed.
    pub fn clean_stale_object_files(&self) -> usize {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut removed = 0;
        for e in &self.entries {
            // Unknown age (pre-tracking) — leave it alone.
            if e.last_login <= 0 { continue; }
            let days_idle = (now - e.last_login) / 86_400;
            if days_idle <= crate::config::CRASH_FILE_TIMEOUT { continue; }
            // Past the crash threshold; if they rented, allow the longer
            // rent grace period before deleting.
            let threshold = match self.load_player(&e.name) {
                Ok(rec) if rec.rent_per_day > 0 => crate::config::RENT_FILE_TIMEOUT,
                _ => crate::config::CRASH_FILE_TIMEOUT,
            };
            if days_idle <= threshold { continue; }
            let path = self.objs_path(&e.name);
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    removed += 1;
                    tracing::info!(player = %e.name, days_idle,
                        "Deleting stale object file");
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::warn!(path = %path, error = %err,
                        "Failed to delete stale object file");
                }
            }
        }
        removed
    }
}

// ---------------------------------------------------------------------------
// Persisted object I/O (plrobjs)
// ---------------------------------------------------------------------------

/// Where the saved object lived on the character.  Mirrors the wear-slot
/// number from structs.h; `Inv` is the carried (inventory) list.
#[derive(Debug, Clone, Copy)]
pub enum SavedObjSlot {
    Inv,
    Wear(u8),
}

/// One entry in a saved object file: the prototype vnum, its slot, and
/// (for containers) the vnums it holds.
#[derive(Debug, Clone)]
pub struct SavedObj {
    pub vnum:     i32,
    pub slot:     SavedObjSlot,
    /// Item condition (0..=100); default 100 for older save files that
    /// omit the `c=<n>` marker.
    pub condition: i32,
    /// Player-brewed spell vnum stored on a potion (cp173).  None for
    /// world items.  Serialized as `b=<vnum>` when set.
    pub brewed_spell: Option<i32>,
    /// Per-instance enchantments (cp177): (apply_location, modifier)
    /// pairs serialized as `a=<loc>:<mod>` markers.
    pub bonus_affects: Vec<(i32, i32)>,
    /// Vnums of objects this container holds.  Empty for non-containers
    /// and empty containers.  Format on disk: appended as space-separated
    /// integers after the slot field, e.g. "3105 inv c=85 100 200".
    pub contents: Vec<i32>,
}

/// Read `<lib>/plrobjs/<bucket>/<name>.objs`.  Returns an empty Vec if the
/// file is missing — that's what a brand-new character looks like.
pub fn load_objs(data_dir: &str, name: &str) -> Vec<SavedObj> {
    let lower = name.to_lowercase();
    let bucket = match lower.chars().next().unwrap_or('a') {
        'a'..='e' => "A-E",
        'f'..='j' => "F-J",
        'k'..='o' => "K-O",
        'p'..='t' => "P-T",
        _         => "U-Z",
    };
    let path = format!("{data_dir}/plrobjs/{bucket}/{lower}.objs");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') { continue; }
        // "<vnum> <slot> [<content_vnum> …]"
        let parts: Vec<&str> = t.split_ascii_whitespace().collect();
        if parts.len() < 2 { continue; }
        let Ok(vnum) = parts[0].parse::<i32>() else { continue };
        let slot = match parts[1] {
            "inv" => SavedObjSlot::Inv,
            s => match s.parse::<u8>() {
                Ok(n) => SavedObjSlot::Wear(n),
                Err(_) => continue,
            },
        };
        // Optional `c=<N>` condition + `b=<N>` brewed-spell markers,
        // anywhere in the trailing tokens.  Everything else is content vnums.
        let mut condition = 100i32;
        let mut brewed_spell: Option<i32> = None;
        let mut bonus_affects: Vec<(i32, i32)> = Vec::new();
        let mut contents: Vec<i32> = Vec::new();
        for tok in &parts[2..] {
            if let Some(rest) = tok.strip_prefix("c=") {
                if let Ok(n) = rest.parse::<i32>() {
                    condition = n.clamp(0, 100);
                    continue;
                }
            }
            if let Some(rest) = tok.strip_prefix("b=") {
                if let Ok(n) = rest.parse::<i32>() {
                    brewed_spell = Some(n);
                    continue;
                }
            }
            if let Some(rest) = tok.strip_prefix("a=") {
                let mut parts = rest.split(':');
                if let (Some(loc), Some(modi)) = (parts.next(), parts.next()) {
                    if let (Ok(l), Ok(m)) = (loc.parse::<i32>(), modi.parse::<i32>()) {
                        bonus_affects.push((l, m));
                        continue;
                    }
                }
            }
            if let Ok(n) = tok.parse::<i32>() {
                contents.push(n);
            }
        }
        out.push(SavedObj { vnum, slot, condition, brewed_spell, bonus_affects, contents });
    }
    out
}

/// Write the saved-objects file for `name`. Pass `entries` in the order
/// you want them serialised (typically inventory first, then equipment by
/// wear position).
pub fn save_objs(data_dir: &str, name: &str, entries: &[SavedObj]) -> Result<()> {
    let lower = name.to_lowercase();
    let bucket = match lower.chars().next().unwrap_or('a') {
        'a'..='e' => "A-E",
        'f'..='j' => "F-J",
        'k'..='o' => "K-O",
        'p'..='t' => "P-T",
        _         => "U-Z",
    };
    let path = format!("{data_dir}/plrobjs/{bucket}/{lower}.objs");
    if let Some(parent) = PathBuf::from(&path).parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::File::create(&path)
        .with_context(|| format!("Cannot write objs file {path}"))?;
    writeln!(f, "# tbamud-rwb plrobjs v1 — <vnum> <slot> [c=N] [<content_vnum> ...]")?;
    for e in entries {
        let slot_str: String = match e.slot {
            SavedObjSlot::Inv     => "inv".into(),
            SavedObjSlot::Wear(n) => n.to_string(),
        };
        let cond_str = if e.condition < 100 {
            format!(" c={}", e.condition)
        } else { String::new() };
        let brew_str = match e.brewed_spell {
            Some(n) => format!(" b={n}"),
            None    => String::new(),
        };
        let bonus_str: String = e.bonus_affects.iter()
            .map(|(l, m)| format!(" a={l}:{m}"))
            .collect();
        if e.contents.is_empty() {
            writeln!(f, "{} {slot_str}{cond_str}{brew_str}{bonus_str}", e.vnum)?;
        } else {
            let inner: Vec<String> = e.contents.iter().map(|v| v.to_string()).collect();
            writeln!(f, "{} {slot_str}{cond_str}{brew_str}{bonus_str} {}", e.vnum, inner.join(" "))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Password hashing
// ---------------------------------------------------------------------------

/// Hash a password with DES crypt(3), compatible with the tbaMUD CRYPT() macro.
///
/// In tbaMUD:
///   `CRYPT(password, player_name)` — for new characters (creates hash)
///   `CRYPT(password, stored_hash)` — for login verification (re-derives hash)
///
/// `crypt(3)` is not thread-safe (returns a global static buffer), so calls
/// are serialised behind a Mutex.
pub fn crypt_password(password: &str, salt: &str) -> String {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap();

    let Ok(pw_c)   = CString::new(password) else { return String::new() };
    let Ok(salt_c) = CString::new(salt)     else { return String::new() };

    // Safety: crypt(3) is a POSIX function; salt is used only as a read-only
    // input.  We copy the result immediately before releasing the lock.
    unsafe {
        let ptr = crypt(pw_c.as_ptr(), salt_c.as_ptr());
        if ptr.is_null() {
            return String::new();
        }
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

/// Verify `password` against `stored_hash`.
/// Mirrors `strncmp(CRYPT(arg, GET_PASSWD(ch)), GET_PASSWD(ch), MAX_PWD_LENGTH)`.
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    if stored_hash.is_empty() || password.is_empty() {
        return false;
    }
    let computed = crypt_password(password, stored_hash);
    !computed.is_empty() && computed == stored_hash
}

// ---------------------------------------------------------------------------
// Name validation
// ---------------------------------------------------------------------------

/// Validate a player name.  Returns an error string on rejection, or `None`
/// if the name is acceptable.  Mirrors `_parse_name()` + `valid_name()` in
/// interpreter.c / ban.c.
///
/// Rules:
///   - Only ASCII alphabetic characters
///   - Length: 2–MAX_NAME_LENGTH
///   - Must contain at least one vowel (prevents "zxcv" style names)
///   - Not in the optional xnames ban list
pub fn validate_name(name: &str, xnames: &[String]) -> Option<&'static str> {
    if name.len() < 2 || name.len() > MAX_NAME_LENGTH {
        return Some("Invalid name, please try another.\r\nName: ");
    }
    if !name.chars().all(|c| c.is_ascii_alphabetic()) {
        return Some("Invalid name, please try another.\r\nName: ");
    }
    let has_vowel = name.chars().any(|c| "aeiouAEIOU".contains(c));
    if !has_vowel {
        return Some("Invalid name, please try another.\r\nName: ");
    }
    let lower = name.to_lowercase();
    for banned in xnames {
        if lower.contains(banned.as_str()) {
            return Some("Invalid name, please try another.\r\nName: ");
        }
    }
    None
}

/// Load the optional `lib/etc/xnames` file (one banned substring per line).
pub fn load_xnames(data_dir: &str) -> Vec<String> {
    let path = format!("{}/etc/xnames", data_dir);
    fs::read_to_string(path)
        .map(|s| s.lines()
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty())
            .collect())
        .unwrap_or_default()
}

/// Load `lib/etc/badsites` (one host substring per line).  Missing
/// file → empty list.
pub fn load_badsites(data_dir: &str) -> Vec<String> {
    let path = format!("{}/etc/badsites", data_dir);
    fs::read_to_string(path)
        .map(|s| s.lines()
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect())
        .unwrap_or_default()
}

/// Save the badsites list, one entry per line.
pub fn save_badsites(data_dir: &str, entries: &[String]) -> Result<()> {
    let dir = format!("{}/etc", data_dir);
    std::fs::create_dir_all(&dir).ok();
    let path = format!("{}/etc/badsites", data_dir);
    let mut s = String::new();
    for e in entries {
        s.push_str(e);
        s.push('\n');
    }
    fs::write(&path, s)
        .with_context(|| format!("writing {path}"))
}

// ---------------------------------------------------------------------------
// tbaMUD ASCII flag encoding (mirrors sprintascii / asciiflag_conv in utils.c)
// ---------------------------------------------------------------------------

/// Decode an ASCII bitvector string into a u32.
/// If the string is all digits, parse as a plain integer.
/// Otherwise, each lowercase letter represents the corresponding bit (a=0, b=1, …).
pub fn asciiflag_conv(s: &str) -> u32 {
    if s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse().unwrap_or(0)
    } else {
        let mut flags = 0u32;
        for b in s.bytes() {
            if b.is_ascii_lowercase() {
                flags |= 1 << (b - b'a');
            }
        }
        flags
    }
}

/// Encode a u32 bitvector as an ASCII flag string.
/// 0 → "0", otherwise a..z for each set bit.
fn sprintascii(flags: u32) -> String {
    if flags == 0 {
        return "0".to_string();
    }
    let mut s = String::new();
    for i in 0..26u32 {
        if flags & (1 << i) != 0 {
            s.push((b'a' + i as u8) as char);
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

pub fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None    => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asciiflag_roundtrip() {
        assert_eq!(asciiflag_conv("0"), 0);
        assert_eq!(asciiflag_conv("a"), 1);
        assert_eq!(asciiflag_conv("k"), 1 << 10);   // PLR_DELETED
        assert_eq!(sprintascii(0), "0");
        assert_eq!(sprintascii(1 << 10), "k");
    }

    #[test]
    fn name_validation() {
        assert!(validate_name("ab", &[]).is_none());         // ok: minimal valid
        assert!(validate_name("Mahatma", &[]).is_none());    // ok: existing char
        assert!(validate_name("a", &[]).is_some());          // too short
        assert!(validate_name("bcdfgh", &[]).is_some());     // no vowels
        assert!(validate_name("bo b", &[]).is_some());       // space
        assert!(validate_name("foo1", &[]).is_some());       // digit
        let xnames = vec!["ass".to_string()];
        assert!(validate_name("assassin", &xnames).is_some()); // banned substring
    }

    #[test]
    fn password_roundtrip() {
        // Hash a known password, then verify against the stored hash.
        // DES crypt: salt = first 2 chars of player name ("Te").
        let hash = crypt_password("secret", "Testplayer");
        assert!(!hash.is_empty(), "crypt(3) must be available");
        // The hash starts with the 2-char salt
        assert!(hash.starts_with("Te"), "DES hash must start with the salt");
        // verify_password uses the stored hash as its own salt, which is the DES convention
        assert!(verify_password("secret", &hash),     "correct password must verify");
        assert!(!verify_password("wrong",  &hash),    "wrong password must fail");
    }
}
