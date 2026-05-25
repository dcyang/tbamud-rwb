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

/// Class constants (CLASS_* in structs.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i8)]
pub enum Class {
    #[default]
    Undefined = -1,
    MagicUser = 0,
    Cleric = 1,
    Thief = 2,
    Warrior = 3,
}

impl Class {
    pub fn from_i8(v: i8) -> Self {
        match v {
            0 => Self::MagicUser,
            1 => Self::Cleric,
            2 => Self::Thief,
            3 => Self::Warrior,
            _ => Self::Undefined,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MagicUser => "Magic-user",
            Self::Cleric    => "Cleric",
            Self::Thief     => "Thief",
            Self::Warrior   => "Warrior",
            Self::Undefined => "Undefined",
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
    pub practices:     i32,
    pub room:          i32,
    pub gold:          i64,
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
    /// Vnums of objects this container holds.  Empty for non-containers
    /// and empty containers.  Format on disk: appended as space-separated
    /// integers after the slot field, e.g. "3105 inv 100 200 300".
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
        let contents: Vec<i32> = parts[2..].iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        out.push(SavedObj { vnum, slot, contents });
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
    writeln!(f, "# tbamud-rwb plrobjs v1 — <vnum> <slot> [<content_vnum> ...]")?;
    for e in entries {
        let slot_str: String = match e.slot {
            SavedObjSlot::Inv     => "inv".into(),
            SavedObjSlot::Wear(n) => n.to_string(),
        };
        if e.contents.is_empty() {
            writeln!(f, "{} {slot_str}", e.vnum)?;
        } else {
            let inner: Vec<String> = e.contents.iter().map(|v| v.to_string()).collect();
            writeln!(f, "{} {slot_str} {}", e.vnum, inner.join(" "))?;
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
