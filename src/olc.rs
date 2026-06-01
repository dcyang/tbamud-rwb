//! Oasis-style online creation (OLC) — a port of stock TbaMUD's OLC.
//!
//! While a builder is in an editor, every line they type is routed here
//! (via `dispatch_command`) instead of to the normal command interpreter.
//! Each editor is a small menu state machine over a *working copy* of the
//! entity; on Quit the working copy is committed to the live `World` and
//! the zone's data file is rewritten.
//!
//! This module implements the full OLC editor set: `redit` (rooms),
//! `oedit` (objects), `medit` (mobiles), `zedit` (zones + resets),
//! `qedit` (quests), `trigedit` (DG triggers), `sedit` (shops),
//! `aedit` (socials), and `hedit` (help) — all on the same
//! `OlcSession`/`handle_input` backbone, each with a `gen*`-equivalent
//! disk serializer that round-trips through the loaders in `db.rs`.

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::character::Character;
use crate::interpreter::CmdOutput;
use crate::players::PlayerDb;
use crate::world::{Exit, ExtraDescr, Room, RoomVnum, World, ZoneVnum};

const TEXT_END_HINT: &str =
    "[ Enter text; put `@` (or `/s`) on a line by itself to finish. ]\r\n";

fn out(text: impl Into<String>) -> CmdOutput {
    CmdOutput { text: text.into(), quit: false }
}

// ---------------------------------------------------------------------------
// Name tables (stock bit/index order, matching the on-disk data format).
// ---------------------------------------------------------------------------

/// Room flag bit names in stock structs.h order (bit 0 = DARK, …).
pub const ROOM_FLAG_NAMES: &[&str] = &[
    "DARK", "DEATH", "NO_MOB", "INDOORS", "PEACEFUL", "SOUNDPROOF",
    "NO_TRACK", "NO_MAGIC", "TUNNEL", "PRIVATE", "GODROOM", "HOUSE",
    "HOUSE_CRASH", "ATRIUM", "OLC", "*BFS", "WORLDMAP",
];

pub const SECTOR_NAMES: &[&str] = &[
    "Inside", "City", "Field", "Forest", "Hills", "Mountains",
    "Water (Swim)", "Water (No Swim)", "In Flight", "Underwater",
];

const DIR_NAMES: [&str; 6] = ["North", "East", "South", "West", "Up", "Down"];

/// Render a 32-bit flag field as the set of names, or "<None>".
fn sprintbit(flags: u32, names: &[&str]) -> String {
    let mut parts = Vec::new();
    for (i, n) in names.iter().enumerate() {
        if flags & (1 << i) != 0 {
            parts.push(*n);
        }
    }
    if parts.is_empty() { "<None>".to_string() } else { parts.join(" ") }
}

fn sector_name(s: i32) -> &'static str {
    SECTOR_NAMES.get(s as usize).copied().unwrap_or("<Unknown>")
}

// ---------------------------------------------------------------------------
// Session types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct OlcSession {
    pub editor: Editor,
}

#[derive(Debug)]
pub enum Editor {
    Room(Redit),
    Obj(Oedit),
    Mob(Medit),
    Zone(Zedit),
    Quest(Qedit),
    Trig(Tedit),
    Shop(Sedit),
    Social(Aedit),
    Help(Hedit),
}

#[derive(Debug)]
pub struct Redit {
    vnum: RoomVnum,
    zone_number: ZoneVnum,
    room: Room,
    is_new: bool,
    mode: ReditMode,
    /// Accumulator for the multi-line string editor.
    text: Vec<String>,
}

#[derive(Debug, Clone)]
enum ReditMode {
    Main,
    Name,
    Desc,
    Flags,
    Sector,
    ExitMenu(usize),
    ExitRoom(usize),
    ExitDesc(usize),
    ExitKeyword(usize),
    ExitDoorFlag(usize),
    ExitKey(usize),
    ExtraMenu,
    ExtraNewKeyword,
    ExtraKeyword(usize),
    ExtraDesc(usize),
    Script,
    CopyFrom,
    DeleteConfirm,
}

// ---------------------------------------------------------------------------
// Entry: the `redit` command
// ---------------------------------------------------------------------------

/// `redit <vnum>` enters the room editor; `redit save [zone]` writes a zone
/// to disk without editing.  Immortal-gated by the caller.
pub async fn start_redit(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    let mut toks = arg.split_whitespace();
    let first = toks.next().unwrap_or("");

    // redit save [zone-number]
    if first.eq_ignore_ascii_case("save") {
        let znum: Option<ZoneVnum> = toks.next().and_then(|s| s.parse().ok());
        let znum = match znum {
            Some(z) => z,
            None => {
                let w = world.lock().await;
                zone_of_room(&w, me.current_room).map(|z| z.number)
                    .unwrap_or(-1)
            }
        };
        let data_dir = players.lock().await.data_dir().to_string();
        let w = world.lock().await;
        return match save_rooms(&w, &data_dir, znum) {
            Ok(n) => out(format!("\r\nSaved {n} room(s) in zone {znum}.\r\n")),
            Err(e) => out(format!("\r\nSave failed: {e}\r\n")),
        };
    }

    // Otherwise a target vnum (default: current room).
    let vnum: RoomVnum = if first.is_empty() {
        me.current_room
    } else {
        match first.parse() {
            Ok(v) => v,
            Err(_) => return out("\r\nUsage: redit <vnum> | redit save [zone]\r\n"),
        }
    };

    let (room, is_new, zone_number) = {
        let w = world.lock().await;
        match w.rooms.get(&vnum) {
            Some(r) => (r.clone(), false, r.zone),
            None => {
                // New room: must fall inside an existing zone's vnum range.
                match zone_of_vnum(&w, vnum) {
                    Some(z) => {
                        let mut r = Room::default();
                        r.vnum = vnum;
                        r.zone = z.number;
                        r.name = "An unfinished room".to_string();
                        r.description = "You are in an unfinished room.\r\n".to_string();
                        (r, true, z.number)
                    }
                    None => return out(format!(
                        "\r\nVnum {vnum} is not within any existing zone's range.\r\n")),
                }
            }
        }
    };

    let redit = Redit { vnum, zone_number, room, is_new, mode: ReditMode::Main, text: Vec::new() };
    let menu = main_menu(&redit);
    me.olc = Some(OlcSession { editor: Editor::Room(redit) });
    out(menu)
}

// ---------------------------------------------------------------------------
// Input routing
// ---------------------------------------------------------------------------

/// Route one line of input to the active editor.  Returns the next menu /
/// prompt.  Clears `me.olc` when the editor exits.
pub async fn handle_input(
    line: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    // Take the session out so we can mutate freely, then put it back unless
    // the editor decided to exit.
    let mut session = match me.olc.take() {
        Some(s) => s,
        None => return out(String::new()),
    };
    let (text, keep) = match &mut session.editor {
        Editor::Room(r) => redit_input(line, r, world, players).await,
        Editor::Obj(o) => oedit_input(line, o, world, players).await,
        Editor::Mob(m) => medit_input(line, m, world, players).await,
        Editor::Zone(z) => zedit_input(line, z, world, players).await,
        Editor::Quest(q) => qedit_input(line, q, world, players).await,
        Editor::Trig(t) => trigedit_input(line, t, world, players).await,
        Editor::Shop(sh) => sedit_input(line, sh, world, players).await,
        Editor::Social(a) => aedit_input(line, a, world, players).await,
        Editor::Help(h) => hedit_input(line, h, world, players).await,
    };
    if keep {
        me.olc = Some(session);
    }
    out(text)
}

// ---------------------------------------------------------------------------
// Room editor
// ---------------------------------------------------------------------------

/// Handle one input line for the room editor.  Returns (output, keep_open).
async fn redit_input(
    line: &str,
    r: &mut Redit,
    world: &Arc<Mutex<World>>,
    players: &Arc<Mutex<PlayerDb>>,
) -> (String, bool) {
    let mode = r.mode.clone();
    match mode {
        ReditMode::Main => redit_main_choice(line, r, world, players).await,

        ReditMode::Name => {
            r.room.name = strip_ctrl(line, 75);
            r.mode = ReditMode::Main;
            (main_menu(r), true)
        }

        // ---- string editor for the room description --------------------
        ReditMode::Desc => {
            if is_text_end(line) {
                r.room.description = finish_text(r);
                r.mode = ReditMode::Main;
                (main_menu(r), true)
            } else {
                r.text.push(line.to_string());
                (String::new(), true)
            }
        }

        ReditMode::Flags => {
            if line.trim() == "0" {
                r.mode = ReditMode::Main;
                (main_menu(r), true)
            } else if let Ok(n) = line.trim().parse::<usize>() {
                if n >= 1 && n <= ROOM_FLAG_NAMES.len() {
                    r.room.room_flags[0] ^= 1 << (n - 1);
                }
                (flags_menu(r), true)
            } else {
                (flags_menu(r), true)
            }
        }

        ReditMode::Sector => {
            if let Ok(n) = line.trim().parse::<i32>() {
                if n >= 0 && (n as usize) < SECTOR_NAMES.len() {
                    r.room.sector_type = n;
                    r.mode = ReditMode::Main;
                    return (main_menu(r), true);
                }
            }
            (sector_menu(), true)
        }

        ReditMode::ExitMenu(dir) => redit_exit_choice(line, r, dir),

        ReditMode::ExitRoom(dir) => {
            let t = line.trim();
            let to = if t == "-1" || t.is_empty() { crate::world::NOWHERE }
                     else { t.parse().unwrap_or(crate::world::NOWHERE) };
            ensure_exit(r, dir).to_room = to;
            // If the destination is NOWHERE and the exit is otherwise empty,
            // we still keep it (builder may set fields next).
            r.mode = ReditMode::ExitMenu(dir);
            (exit_menu(r, dir), true)
        }

        ReditMode::ExitDesc(dir) => {
            if is_text_end(line) {
                let txt = finish_text(r);
                ensure_exit(r, dir).description = txt;
                r.mode = ReditMode::ExitMenu(dir);
                (exit_menu(r, dir), true)
            } else {
                r.text.push(line.to_string());
                (String::new(), true)
            }
        }

        ReditMode::ExitKeyword(dir) => {
            let kw = strip_ctrl(line, 75);
            ensure_exit(r, dir).keyword = kw;
            r.mode = ReditMode::ExitMenu(dir);
            (exit_menu(r, dir), true)
        }

        ReditMode::ExitDoorFlag(dir) => {
            if let Ok(n) = line.trim().parse::<u32>() {
                use crate::world::{EX_HIDDEN, EX_ISDOOR, EX_PICKPROOF};
                let info = match n {
                    1 => EX_ISDOOR,
                    2 => EX_ISDOOR | EX_PICKPROOF,
                    3 => EX_ISDOOR | EX_HIDDEN,
                    4 => EX_ISDOOR | EX_PICKPROOF | EX_HIDDEN,
                    _ => 0,
                };
                ensure_exit(r, dir).exit_info = info;
                r.mode = ReditMode::ExitMenu(dir);
                return (exit_menu(r, dir), true);
            }
            (door_flag_menu(), true)
        }

        ReditMode::ExitKey(dir) => {
            let key = line.trim().parse::<i32>().unwrap_or(-1);
            ensure_exit(r, dir).key = key;
            r.mode = ReditMode::ExitMenu(dir);
            (exit_menu(r, dir), true)
        }

        ReditMode::ExtraMenu => redit_extra_choice(line, r),

        ReditMode::ExtraNewKeyword => {
            let kw = strip_ctrl(line, 75);
            if kw.is_empty() {
                r.mode = ReditMode::ExtraMenu;
                return (extra_menu(r), true);
            }
            r.room.extras.push(ExtraDescr { keyword: kw, description: String::new() });
            let idx = r.room.extras.len() - 1;
            r.text.clear();
            r.mode = ReditMode::ExtraDesc(idx);
            (format!("Enter the extra description:\r\n{TEXT_END_HINT}"), true)
        }

        ReditMode::ExtraKeyword(idx) => {
            let kw = strip_ctrl(line, 75);
            if let Some(e) = r.room.extras.get_mut(idx) { e.keyword = kw; }
            r.mode = ReditMode::ExtraMenu;
            (extra_menu(r), true)
        }

        ReditMode::ExtraDesc(idx) => {
            if is_text_end(line) {
                let txt = finish_text(r);
                if let Some(e) = r.room.extras.get_mut(idx) { e.description = txt; }
                r.mode = ReditMode::ExtraMenu;
                (extra_menu(r), true)
            } else {
                r.text.push(line.to_string());
                (String::new(), true)
            }
        }

        ReditMode::Script => {
            let t = line.trim();
            if t == "0" || t.is_empty() {
                r.mode = ReditMode::Main;
                return (main_menu(r), true);
            }
            if let Ok(v) = t.parse::<i32>() {
                if let Some(pos) = r.room.triggers.iter().position(|&x| x == v) {
                    r.room.triggers.remove(pos);
                } else {
                    r.room.triggers.push(v);
                }
            }
            (script_menu(r), true)
        }

        ReditMode::CopyFrom => {
            let t = line.trim();
            if let Ok(v) = t.parse::<RoomVnum>() {
                let src = { world.lock().await.rooms.get(&v).cloned() };
                if let Some(src) = src {
                    let (vnum, zone) = (r.room.vnum, r.room.zone);
                    r.room = src;
                    r.room.vnum = vnum;
                    r.room.zone = zone;
                    r.room.mobs.clear();
                    r.room.objects.clear();
                    r.mode = ReditMode::Main;
                    return (format!("\r\nCopied room {v}.\r\n{}", main_menu(r)), true);
                }
                r.mode = ReditMode::Main;
                return (format!("\r\nNo room {v} to copy.\r\n{}", main_menu(r)), true);
            }
            r.mode = ReditMode::Main;
            (main_menu(r), true)
        }

        ReditMode::DeleteConfirm => {
            if line.trim().eq_ignore_ascii_case("y") {
                let data_dir = players.lock().await.data_dir().to_string();
                let znum = r.zone_number;
                let msg = {
                    let mut w = world.lock().await;
                    w.rooms.remove(&r.vnum);
                    // Detach any exits pointing at the deleted room.
                    for room in w.rooms.values_mut() {
                        for ex in room.exits.iter_mut().flatten() {
                            if ex.to_room == r.vnum { ex.to_room = crate::world::NOWHERE; }
                        }
                    }
                    match save_rooms(&w, &data_dir, znum) {
                        Ok(_) => format!("\r\nRoom {} deleted.\r\n", r.vnum),
                        Err(e) => format!("\r\nRoom {} deleted (disk save failed: {e}).\r\n", r.vnum),
                    }
                };
                (msg, false)
            } else {
                r.mode = ReditMode::Main;
                (format!("\r\nDeletion cancelled.\r\n{}", main_menu(r)), true)
            }
        }
    }
}

async fn redit_main_choice(
    line: &str,
    r: &mut Redit,
    world: &Arc<Mutex<World>>,
    players: &Arc<Mutex<PlayerDb>>,
) -> (String, bool) {
    let c = line.trim();
    match c.to_ascii_uppercase().as_str() {
        "1" => { r.mode = ReditMode::Name; ("Enter room name:\r\n".to_string(), true) }
        "2" => {
            r.text.clear();
            r.mode = ReditMode::Desc;
            (format!("Enter room description (replaces current):\r\n{TEXT_END_HINT}"), true)
        }
        "3" => { r.mode = ReditMode::Flags; (flags_menu(r), true) }
        "4" => { r.mode = ReditMode::Sector; (sector_menu(), true) }
        "5" => { r.mode = ReditMode::ExitMenu(0); (exit_menu(r, 0), true) }
        "6" => { r.mode = ReditMode::ExitMenu(1); (exit_menu(r, 1), true) }
        "7" => { r.mode = ReditMode::ExitMenu(2); (exit_menu(r, 2), true) }
        "8" => { r.mode = ReditMode::ExitMenu(3); (exit_menu(r, 3), true) }
        "9" => { r.mode = ReditMode::ExitMenu(4); (exit_menu(r, 4), true) }
        "A" => { r.mode = ReditMode::ExitMenu(5); (exit_menu(r, 5), true) }
        "F" => { r.mode = ReditMode::ExtraMenu; (extra_menu(r), true) }
        "S" => { r.mode = ReditMode::Script; (script_menu(r), true) }
        "W" => { r.mode = ReditMode::CopyFrom; ("Copy from which room vnum?\r\n".to_string(), true) }
        "X" => { r.mode = ReditMode::DeleteConfirm;
                 ("Delete this room?  This cannot be undone. (y/n)\r\n".to_string(), true) }
        "Q" => {
            // Commit to the live world, then persist the zone to disk.
            let data_dir = players.lock().await.data_dir().to_string();
            let znum = r.zone_number;
            let msg = {
                let mut w = world.lock().await;
                w.rooms.insert(r.vnum, r.room.clone());
                match save_rooms(&w, &data_dir, znum) {
                    Ok(n) => format!("\r\nRoom {} saved ({} room(s) written to zone {}).\r\n",
                                     r.vnum, n, znum),
                    Err(e) => format!("\r\nRoom {} saved to memory (disk write failed: {e}).\r\n",
                                      r.vnum),
                }
            };
            (msg, false)
        }
        _ => (main_menu(r), true),
    }
}

fn redit_exit_choice(line: &str, r: &mut Redit, dir: usize) -> (String, bool) {
    match line.trim() {
        "1" => { r.mode = ReditMode::ExitRoom(dir);
                 ("Exit to room vnum (-1 for none):\r\n".to_string(), true) }
        "2" => { r.text.clear(); r.mode = ReditMode::ExitDesc(dir);
                 (format!("Enter exit description:\r\n{TEXT_END_HINT}"), true) }
        "3" => { r.mode = ReditMode::ExitKeyword(dir);
                 ("Enter exit keywords:\r\n".to_string(), true) }
        "4" => { r.mode = ReditMode::ExitDoorFlag(dir); (door_flag_menu(), true) }
        "5" => { r.mode = ReditMode::ExitKey(dir);
                 ("Enter key vnum (-1 for none):\r\n".to_string(), true) }
        "6" => { // purge exit
                 r.room.exits[dir] = None;
                 r.mode = ReditMode::Main;
                 (main_menu(r), true) }
        "0" => { r.mode = ReditMode::Main; (main_menu(r), true) }
        _ => (exit_menu(r, dir), true),
    }
}

fn redit_extra_choice(line: &str, r: &mut Redit) -> (String, bool) {
    let t = line.trim();
    if t == "0" || t.is_empty() {
        r.mode = ReditMode::Main;
        return (main_menu(r), true);
    }
    if t.eq_ignore_ascii_case("n") {
        r.mode = ReditMode::ExtraNewKeyword;
        return ("Enter keyword(s) for the new extra description:\r\n".to_string(), true);
    }
    // "<n>"  -> edit description of extra n; "d <n>" -> delete; "k <n>" -> keyword
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts[0].eq_ignore_ascii_case("d") {
        if let Some(idx) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
            if idx >= 1 && idx <= r.room.extras.len() { r.room.extras.remove(idx - 1); }
        }
        return (extra_menu(r), true);
    }
    if parts[0].eq_ignore_ascii_case("k") {
        if let Some(idx) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
            if idx >= 1 && idx <= r.room.extras.len() {
                r.mode = ReditMode::ExtraKeyword(idx - 1);
                return ("Enter new keyword(s):\r\n".to_string(), true);
            }
        }
        return (extra_menu(r), true);
    }
    if let Ok(idx) = parts[0].parse::<usize>() {
        if idx >= 1 && idx <= r.room.extras.len() {
            r.text.clear();
            r.mode = ReditMode::ExtraDesc(idx - 1);
            return (format!("Enter the extra description (replaces current):\r\n{TEXT_END_HINT}"), true);
        }
    }
    (extra_menu(r), true)
}

// ---------------------------------------------------------------------------
// Menu rendering
// ---------------------------------------------------------------------------

fn main_menu(r: &Redit) -> String {
    let room = &r.room;
    let mut s = String::new();
    s.push_str(&format!("\r\n-- Room number : [@c{}@n]   Zone: [@c{}@n]\r\n",
        r.vnum, r.zone_number));
    s.push_str(&format!("@g1@n) Name        : @y{}@n\r\n", room.name));
    s.push_str(&format!("@g2@n) Description :\r\n{}\r\n",
        room.description.trim_end_matches(['\r', '\n'])));
    s.push_str(&format!("@g3@n) Room flags  : @c{}@n\r\n", sprintbit(room.room_flags[0], ROOM_FLAG_NAMES)));
    s.push_str(&format!("@g4@n) Sector type : @c{}@n\r\n", sector_name(room.sector_type)));
    for (i, label) in [(0usize, "5"), (1, "6"), (2, "7"), (3, "8"), (4, "9"), (5, "A")] {
        let to = room.exits[i].as_ref().map(|e| e.to_room).unwrap_or(crate::world::NOWHERE);
        let shown = if to == crate::world::NOWHERE { -1 } else { to };
        s.push_str(&format!("@g{}@n) Exit {:<6}: @c{}@n\r\n", label, DIR_NAMES[i].to_lowercase(), shown));
    }
    s.push_str("@gF@n) Extra descriptions menu\r\n");
    s.push_str(&format!("@gS@n) Script      : @c{}@n\r\n",
        if room.triggers.is_empty() { "None".to_string() }
        else { room.triggers.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(" ") }));
    s.push_str("@gW@n) Copy room\r\n");
    s.push_str("@gX@n) Delete room\r\n");
    s.push_str("@gQ@n) Quit (save)\r\n");
    s.push_str("Enter choice : ");
    s
}

fn flags_menu(r: &Redit) -> String {
    let mut s = String::from("\r\n");
    for (i, n) in ROOM_FLAG_NAMES.iter().enumerate() {
        s.push_str(&format!("@g{:2}@n) {:<12}", i + 1, n));
        if (i + 1) % 4 == 0 { s.push_str("\r\n"); }
    }
    if ROOM_FLAG_NAMES.len() % 4 != 0 { s.push_str("\r\n"); }
    s.push_str(&format!("Current : @c{}@n\r\n", sprintbit(r.room.room_flags[0], ROOM_FLAG_NAMES)));
    s.push_str("Toggle which flag (0 to quit) : ");
    s
}

fn sector_menu() -> String {
    let mut s = String::from("\r\n");
    for (i, n) in SECTOR_NAMES.iter().enumerate() {
        s.push_str(&format!("@g{:2}@n) {:<16}", i, n));
        if (i + 1) % 3 == 0 { s.push_str("\r\n"); }
    }
    s.push_str("\r\nEnter sector type : ");
    s
}

fn exit_menu(r: &Redit, dir: usize) -> String {
    let e = r.room.exits[dir].as_ref();
    let to = e.map(|e| e.to_room).unwrap_or(crate::world::NOWHERE);
    let shown = if to == crate::world::NOWHERE { -1 } else { to };
    let desc = e.map(|e| e.description.trim_end_matches(['\r','\n']).to_string()).unwrap_or_default();
    let kw = e.map(|e| e.keyword.clone()).unwrap_or_default();
    let info = e.map(|e| e.exit_info).unwrap_or(0);
    let key = e.map(|e| e.key).unwrap_or(-1);
    let mut s = String::new();
    s.push_str(&format!("\r\n-- Exit : @c{}@n\r\n", DIR_NAMES[dir]));
    s.push_str(&format!("@g1@n) Exit to     : @c{}@n\r\n", shown));
    s.push_str(&format!("@g2@n) Description :\r\n{}\r\n", desc));
    s.push_str(&format!("@g3@n) Door name   : @y{}@n\r\n", kw));
    s.push_str(&format!("@g4@n) Door flags  : @c{}@n\r\n", door_flag_label(info)));
    s.push_str(&format!("@g5@n) Key vnum    : @c{}@n\r\n", key));
    s.push_str("@g6@n) Purge exit\r\n");
    s.push_str("@g0@n) Quit to main menu\r\n");
    s.push_str("Enter choice : ");
    s
}

fn door_flag_menu() -> String {
    "\r\n@g0@n) No door\r\n@g1@n) Door\r\n@g2@n) Pickproof door\r\n\
     @g3@n) Hidden door\r\n@g4@n) Hidden pickproof door\r\nEnter door type : "
        .to_string()
}

fn door_flag_label(info: u32) -> &'static str {
    use crate::world::{EX_HIDDEN, EX_ISDOOR, EX_PICKPROOF};
    if info & EX_ISDOOR == 0 { return "No door"; }
    match (info & EX_PICKPROOF != 0, info & EX_HIDDEN != 0) {
        (false, false) => "Door",
        (true, false) => "Pickproof door",
        (false, true) => "Hidden door",
        (true, true) => "Hidden pickproof door",
    }
}

fn extra_menu(r: &Redit) -> String {
    let mut s = String::from("\r\n-- Extra descriptions --\r\n");
    if r.room.extras.is_empty() {
        s.push_str("(none)\r\n");
    } else {
        for (i, e) in r.room.extras.iter().enumerate() {
            s.push_str(&format!("@g{:2}@n) {}\r\n", i + 1, e.keyword));
        }
    }
    s.push_str("@gN@n) New extra description\r\n");
    s.push_str("Edit <num>, `k <num>` keyword, `d <num>` delete, 0 to quit : ");
    s
}

fn script_menu(r: &Redit) -> String {
    let mut s = String::from("\r\n-- Attached triggers --\r\n");
    if r.room.triggers.is_empty() {
        s.push_str("(none)\r\n");
    } else {
        for t in &r.room.triggers { s.push_str(&format!("  {t}\r\n")); }
    }
    s.push_str("Enter a trigger vnum to add/remove (0 to quit) : ");
    s
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_text_end(line: &str) -> bool {
    let t = line.trim();
    t == "@" || t.eq_ignore_ascii_case("/s")
}

/// Join the accumulated text-editor lines into a stored string (CRLF lines).
fn finish_text(r: &mut Redit) -> String {
    let joined = r.text.join("\r\n");
    r.text.clear();
    joined
}

fn strip_ctrl(s: &str, max: usize) -> String {
    s.trim().chars().filter(|c| !c.is_control()).take(max).collect()
}

/// Get (creating if necessary) a mutable reference to an exit.
fn ensure_exit(r: &mut Redit, dir: usize) -> &mut Exit {
    if r.room.exits[dir].is_none() {
        r.room.exits[dir] = Some(Exit { to_room: crate::world::NOWHERE, key: -1, ..Default::default() });
    }
    r.room.exits[dir].as_mut().unwrap()
}

fn zone_of_room<'a>(w: &'a World, vnum: RoomVnum) -> Option<&'a crate::world::Zone> {
    let znum = w.rooms.get(&vnum)?.zone;
    w.zones.get(&znum)
}

fn zone_of_vnum<'a>(w: &'a World, vnum: RoomVnum) -> Option<&'a crate::world::Zone> {
    w.zones.values().find(|z| vnum >= z.bot && vnum <= z.top)
}

// ---------------------------------------------------------------------------
// Disk serialization (genwld-equivalent: write a zone's <num>.wld)
// ---------------------------------------------------------------------------

/// Write every room belonging to zone `zone_number` (by vnum range) to
/// `<data_dir>/world/wld/<zone_number>.wld`.  Returns the count written.
pub fn save_rooms(w: &World, data_dir: &str, zone_number: ZoneVnum) -> std::io::Result<usize> {
    use std::io::Write;
    let zone = w.zones.get(&zone_number)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such zone"))?;
    let dir = format!("{data_dir}/world/wld");
    std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/{zone_number}.wld");
    let tmp = format!("{path}.new");
    let mut f = std::fs::File::create(&tmp)?;

    let mut count = 0;
    for (&vnum, room) in w.rooms.range(zone.bot..=zone.top) {
        count += 1;
        // Header + name + description.
        writeln!(f, "#{vnum}")?;
        writeln!(f, "{}~", room.name)?;
        writeln!(f, "{}~", room.description.trim_end_matches(['\r', '\n']))?;
        // zone flags0 sector flags1 flags2 flags3
        writeln!(f, "{} {} {} {} {} {}",
            room.zone, room.room_flags[0], room.sector_type,
            room.room_flags[1], room.room_flags[2], room.room_flags[3])?;

        // Exits.
        for (dir, ex) in room.exits.iter().enumerate() {
            let Some(ex) = ex else { continue };
            let dflag = door_dflag(ex.exit_info);
            let to = if ex.to_room == crate::world::NOWHERE { -1 } else { ex.to_room };
            writeln!(f, "D{dir}")?;
            writeln!(f, "{}~", ex.description.trim_end_matches(['\r', '\n']))?;
            writeln!(f, "{}~", ex.keyword)?;
            writeln!(f, "{} {} {}", dflag, ex.key, to)?;
        }
        // Extra descriptions.
        for e in &room.extras {
            writeln!(f, "E")?;
            writeln!(f, "{}~", e.keyword)?;
            writeln!(f, "{}~", e.description.trim_end_matches(['\r', '\n']))?;
        }
        writeln!(f, "S")?;
        // DG trigger attachments (T <vnum>) appear after S in the .wld.
        for t in &room.triggers {
            writeln!(f, "T {t}")?;
        }
    }
    writeln!(f, "$~")?;
    f.flush()?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    Ok(count)
}

/// exit_info bits -> the .wld door-flag integer (matches genwld + the loader).
fn door_dflag(info: u32) -> u32 {
    use crate::world::{EX_HIDDEN, EX_ISDOOR, EX_PICKPROOF};
    if info & EX_ISDOOR == 0 { return 0; }
    let mut d = if info & EX_PICKPROOF != 0 { 2 } else { 1 };
    if info & EX_HIDDEN != 0 { d += 2; }
    d
}

// ===========================================================================
// Object editor (oedit) — port of stock oedit.c / genobj.c
// ===========================================================================

use crate::world::{ObjAffect, ObjProto, ObjVnum};

pub const ITEM_TYPE_NAMES: &[&str] = &[
    "UNDEFINED", "LIGHT", "SCROLL", "WAND", "STAFF", "WEAPON", "FURNITURE",
    "FREE", "TREASURE", "ARMOR", "POTION", "WORN", "OTHER", "TRASH", "FREE2",
    "CONTAINER", "NOTE", "LIQ CONTAINER", "KEY", "FOOD", "MONEY", "PEN",
    "BOAT", "FOUNTAIN",
];

pub const EXTRA_BIT_NAMES: &[&str] = &[
    "GLOW", "HUM", "NO_RENT", "NO_DONATE", "NO_INVIS", "INVISIBLE", "MAGIC",
    "NO_DROP", "BLESS", "ANTI_GOOD", "ANTI_EVIL", "ANTI_NEUTRAL", "ANTI_MAGE",
    "ANTI_CLERIC", "ANTI_THIEF", "ANTI_WARRIOR", "NO_SELL", "QUEST_ITEM",
];

pub const WEAR_BIT_NAMES: &[&str] = &[
    "TAKE", "FINGER", "NECK", "BODY", "HEAD", "LEGS", "FEET", "HANDS", "ARMS",
    "SHIELD", "ABOUT", "WAIST", "WRIST", "WIELD", "HOLD",
];

/// AFF_ flag names; index 0 is reserved/unused (matches affected_bits[0]="\0").
pub const AFFECT_BIT_NAMES: &[&str] = &[
    "(reserved)", "BLIND", "INVIS", "DET-ALIGN", "DET-INVIS", "DET-MAGIC",
    "SENSE-LIFE", "WATWALK", "SANCT", "GROUP", "CURSE", "INFRA", "POISON",
    "PROT-EVIL", "PROT-GOOD", "SLEEP", "NO_TRACK", "FLY", "SCUBA", "SNEAK",
    "HIDE", "UNUSED", "CHARM",
];

pub const APPLY_NAMES: &[&str] = &[
    "NONE", "STR", "DEX", "INT", "WIS", "CON", "CHA", "CLASS", "LEVEL", "AGE",
    "CHAR_WEIGHT", "CHAR_HEIGHT", "MAXMANA", "MAXHIT", "MAXMOVE", "GOLD", "EXP",
    "ARMOR", "HITROLL", "DAMROLL", "SAVING_PARA", "SAVING_ROD", "SAVING_PETRI",
    "SAVING_BREATH", "SAVING_SPELL",
];

fn item_type_name(t: i32) -> &'static str {
    ITEM_TYPE_NAMES.get(t as usize).copied().unwrap_or("UNDEFINED")
}
fn apply_name(loc: i32) -> &'static str {
    APPLY_NAMES.get(loc as usize).copied().unwrap_or("?")
}

/// Per-type labels for the four object values.
fn value_labels(item_type: i32) -> [&'static str; 4] {
    match item_type {
        1  => ["", "", "Hours of light (-1 = inf)", ""],            // LIGHT
        2 | 10 => ["Spell level", "Spell 1", "Spell 2", "Spell 3"], // SCROLL/POTION
        3 | 4  => ["Spell level", "Max charges", "Charges left", "Spell"], // WAND/STAFF
        5  => ["", "Num damage dice", "Size of dice", "Weapon attack type"], // WEAPON
        9  => ["Armor class (AC)", "", "", ""],                     // ARMOR
        15 => ["Capacity (lbs)", "Container flags", "Key vnum", "Closed/etc"], // CONTAINER
        17 | 23 => ["Capacity", "Current amount", "Liquid type", "Poisoned (0/1)"], // DRINKCON/FOUNTAIN
        18 => ["", "", "", ""],                                     // KEY
        19 => ["Hours of food", "", "", "Poisoned (0/1)"],          // FOOD
        20 => ["Gold amount", "", "", ""],                          // MONEY
        _  => ["Value 0", "Value 1", "Value 2", "Value 3"],
    }
}

#[derive(Debug)]
pub struct Oedit {
    vnum: ObjVnum,
    zone_number: ZoneVnum,
    obj: ObjProto,
    is_new: bool,
    mode: OeditMode,
    text: Vec<String>,
}

#[derive(Debug, Clone)]
enum OeditMode {
    Main,
    Keywords,
    ShortDesc,
    LongDesc,
    ActionDesc,
    Type,
    ExtraFlags,
    WearFlags,
    AffectFlags,
    Weight,
    Cost,
    Rent,
    Timer,
    Level,
    ValuesMenu,
    ValueEdit(usize),
    AppliesMenu,
    ApplyType(usize),
    ApplyModifier(usize),
    ExtraMenu,
    ExtraNewKeyword,
    ExtraKeyword(usize),
    ExtraDesc(usize),
    CopyFrom,
    DeleteConfirm,
}

pub async fn start_oedit(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    let mut toks = arg.split_whitespace();
    let first = toks.next().unwrap_or("");

    if first.eq_ignore_ascii_case("save") {
        let znum: Option<ZoneVnum> = toks.next().and_then(|s| s.parse().ok());
        let znum = match znum {
            Some(z) => z,
            None => { let w = world.lock().await;
                      zone_of_room(&w, me.current_room).map(|z| z.number).unwrap_or(-1) }
        };
        let data_dir = players.lock().await.data_dir().to_string();
        let w = world.lock().await;
        return match save_objects(&w, &data_dir, znum) {
            Ok(n) => out(format!("\r\nSaved {n} object(s) in zone {znum}.\r\n")),
            Err(e) => out(format!("\r\nSave failed: {e}\r\n")),
        };
    }

    let vnum: ObjVnum = match first.parse() {
        Ok(v) => v,
        Err(_) => return out("\r\nUsage: oedit <vnum> | oedit save [zone]\r\n"),
    };

    let (obj, is_new, zone_number) = {
        let w = world.lock().await;
        match w.obj_protos.get(&vnum) {
            Some(o) => (o.clone(), false, zone_of_vnum(&w, vnum).map(|z| z.number).unwrap_or(0)),
            None => match zone_of_vnum(&w, vnum) {
                Some(z) => {
                    let mut o = ObjProto::default();
                    o.vnum = vnum;
                    o.name = "unfinished object".to_string();
                    o.short_description = "an unfinished object".to_string();
                    o.description = "An unfinished object is lying here.".to_string();
                    o.item_type = 12; // OTHER
                    (o, true, z.number)
                }
                None => return out(format!(
                    "\r\nVnum {vnum} is not within any existing zone's range.\r\n")),
            },
        }
    };
    let oedit = Oedit { vnum, zone_number, obj, is_new, mode: OeditMode::Main, text: Vec::new() };
    let menu = oedit_menu(&oedit);
    me.olc = Some(OlcSession { editor: Editor::Obj(oedit) });
    out(menu)
}

async fn oedit_input(
    line: &str,
    o: &mut Oedit,
    world: &Arc<Mutex<World>>,
    players: &Arc<Mutex<PlayerDb>>,
) -> (String, bool) {
    let mode = o.mode.clone();
    match mode {
        OeditMode::Main => oedit_main_choice(line, o, world, players).await,

        OeditMode::Keywords => { o.obj.name = strip_ctrl(line, 100); o.mode = OeditMode::Main; (oedit_menu(o), true) }
        OeditMode::ShortDesc => { o.obj.short_description = strip_ctrl(line, 100); o.mode = OeditMode::Main; (oedit_menu(o), true) }
        OeditMode::LongDesc => { o.obj.description = strip_ctrl(line, 200); o.mode = OeditMode::Main; (oedit_menu(o), true) }
        OeditMode::ActionDesc => {
            if is_text_end(line) {
                o.obj.action_description = o.text.join("\r\n"); o.text.clear();
                o.mode = OeditMode::Main; (oedit_menu(o), true)
            } else { o.text.push(line.to_string()); (String::new(), true) }
        }

        OeditMode::Type => {
            if let Ok(n) = line.trim().parse::<i32>() {
                if n >= 0 && (n as usize) < ITEM_TYPE_NAMES.len() {
                    o.obj.item_type = n; o.mode = OeditMode::Main; return (oedit_menu(o), true);
                }
            }
            (type_menu(), true)
        }

        OeditMode::ExtraFlags => toggle_flag(line, o, FlagKind::Extra),
        OeditMode::WearFlags  => toggle_flag(line, o, FlagKind::Wear),
        OeditMode::AffectFlags => toggle_flag(line, o, FlagKind::Affect),

        OeditMode::Weight => { o.obj.weight = line.trim().parse().unwrap_or(o.obj.weight); o.mode = OeditMode::Main; (oedit_menu(o), true) }
        OeditMode::Cost   => { o.obj.cost   = line.trim().parse().unwrap_or(o.obj.cost);   o.mode = OeditMode::Main; (oedit_menu(o), true) }
        OeditMode::Rent   => { o.obj.rent   = line.trim().parse().unwrap_or(o.obj.rent);   o.mode = OeditMode::Main; (oedit_menu(o), true) }
        OeditMode::Timer  => { o.obj.timer  = line.trim().parse().unwrap_or(o.obj.timer);  o.mode = OeditMode::Main; (oedit_menu(o), true) }
        OeditMode::Level  => { o.obj.level  = line.trim().parse().unwrap_or(o.obj.level);  o.mode = OeditMode::Main; (oedit_menu(o), true) }

        OeditMode::ValuesMenu => {
            let t = line.trim();
            if t == "0" || t.is_empty() { o.mode = OeditMode::Main; return (oedit_menu(o), true); }
            if let Ok(n) = t.parse::<usize>() {
                if (1..=4).contains(&n) {
                    o.mode = OeditMode::ValueEdit(n - 1);
                    let labels = value_labels(o.obj.item_type);
                    let lbl = if labels[n-1].is_empty() { "value" } else { labels[n-1] };
                    return (format!("Enter {lbl}:\r\n"), true);
                }
            }
            (values_menu(o), true)
        }
        OeditMode::ValueEdit(i) => {
            o.obj.value[i] = line.trim().parse().unwrap_or(o.obj.value[i]);
            o.mode = OeditMode::ValuesMenu;
            (values_menu(o), true)
        }

        OeditMode::AppliesMenu => oedit_applies_choice(line, o),
        OeditMode::ApplyType(slot) => {
            if let Ok(n) = line.trim().parse::<i32>() {
                if n >= 0 && (n as usize) < APPLY_NAMES.len() {
                    set_apply_location(o, slot, n);
                    o.mode = OeditMode::ApplyModifier(slot);
                    return ("Enter modifier (0 removes this apply):\r\n".to_string(), true);
                }
            }
            (apply_type_menu(), true)
        }
        OeditMode::ApplyModifier(slot) => {
            let m: i32 = line.trim().parse().unwrap_or(0);
            set_apply_modifier(o, slot, m);
            o.mode = OeditMode::AppliesMenu;
            (applies_menu(o), true)
        }

        OeditMode::ExtraMenu => oedit_extra_choice(line, o),
        OeditMode::ExtraNewKeyword => {
            let kw = strip_ctrl(line, 75);
            if kw.is_empty() { o.mode = OeditMode::ExtraMenu; return (extra_menu_obj(o), true); }
            o.obj.extras.push(ExtraDescr { keyword: kw, description: String::new() });
            let idx = o.obj.extras.len() - 1;
            o.text.clear(); o.mode = OeditMode::ExtraDesc(idx);
            (format!("Enter the extra description:\r\n{TEXT_END_HINT}"), true)
        }
        OeditMode::ExtraKeyword(idx) => {
            let kw = strip_ctrl(line, 75);
            if let Some(e) = o.obj.extras.get_mut(idx) { e.keyword = kw; }
            o.mode = OeditMode::ExtraMenu; (extra_menu_obj(o), true)
        }
        OeditMode::ExtraDesc(idx) => {
            if is_text_end(line) {
                let txt = o.text.join("\r\n"); o.text.clear();
                if let Some(e) = o.obj.extras.get_mut(idx) { e.description = txt; }
                o.mode = OeditMode::ExtraMenu; (extra_menu_obj(o), true)
            } else { o.text.push(line.to_string()); (String::new(), true) }
        }

        OeditMode::CopyFrom => {
            if let Ok(v) = line.trim().parse::<ObjVnum>() {
                let src = { world.lock().await.obj_protos.get(&v).cloned() };
                if let Some(mut src) = src {
                    src.vnum = o.obj.vnum;
                    o.obj = src; o.mode = OeditMode::Main;
                    return (format!("\r\nCopied object {v}.\r\n{}", oedit_menu(o)), true);
                }
            }
            o.mode = OeditMode::Main; (format!("\r\nNo such object.\r\n{}", oedit_menu(o)), true)
        }
        OeditMode::DeleteConfirm => {
            if line.trim().eq_ignore_ascii_case("y") {
                let data_dir = players.lock().await.data_dir().to_string();
                let znum = o.zone_number;
                let msg = {
                    let mut w = world.lock().await;
                    w.obj_protos.remove(&o.vnum);
                    match save_objects(&w, &data_dir, znum) {
                        Ok(_) => format!("\r\nObject {} deleted.\r\n", o.vnum),
                        Err(e) => format!("\r\nObject {} deleted (disk save failed: {e}).\r\n", o.vnum),
                    }
                };
                (msg, false)
            } else { o.mode = OeditMode::Main; (format!("\r\nCancelled.\r\n{}", oedit_menu(o)), true) }
        }
    }
}

async fn oedit_main_choice(
    line: &str, o: &mut Oedit,
    world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>,
) -> (String, bool) {
    match line.trim().to_ascii_uppercase().as_str() {
        "1" => { o.mode = OeditMode::Keywords; ("Enter keywords:\r\n".to_string(), true) }
        "2" => { o.mode = OeditMode::ShortDesc; ("Enter short description:\r\n".to_string(), true) }
        "3" => { o.mode = OeditMode::LongDesc; ("Enter long (on-ground) description:\r\n".to_string(), true) }
        "4" => { o.text.clear(); o.mode = OeditMode::ActionDesc;
                 (format!("Enter action description:\r\n{TEXT_END_HINT}"), true) }
        "5" => { o.mode = OeditMode::Type; (type_menu(), true) }
        "6" => { o.mode = OeditMode::ExtraFlags; (flag_toggle_menu(o.obj.extra_flags[0], EXTRA_BIT_NAMES, "Extra"), true) }
        "7" => { o.mode = OeditMode::WearFlags; (flag_toggle_menu(o.obj.wear_flags[0], WEAR_BIT_NAMES, "Wear"), true) }
        "8" => { o.mode = OeditMode::Weight; ("Enter weight:\r\n".to_string(), true) }
        "9" => { o.mode = OeditMode::Cost; ("Enter cost:\r\n".to_string(), true) }
        "A" => { o.mode = OeditMode::Rent; ("Enter cost per day (rent):\r\n".to_string(), true) }
        "B" => { o.mode = OeditMode::Timer; ("Enter timer:\r\n".to_string(), true) }
        "C" => { o.mode = OeditMode::ValuesMenu; (values_menu(o), true) }
        "D" => { o.mode = OeditMode::AppliesMenu; (applies_menu(o), true) }
        "E" => { o.mode = OeditMode::ExtraMenu; (extra_menu_obj(o), true) }
        "M" => { o.mode = OeditMode::Level; ("Enter minimum level:\r\n".to_string(), true) }
        "P" => { o.mode = OeditMode::AffectFlags;
                 (flag_toggle_menu_affect(o.obj.affect_flags[0]), true) }
        "W" => { o.mode = OeditMode::CopyFrom; ("Copy from which object vnum?\r\n".to_string(), true) }
        "X" => { o.mode = OeditMode::DeleteConfirm; ("Delete this object? (y/n)\r\n".to_string(), true) }
        "Q" => {
            let data_dir = players.lock().await.data_dir().to_string();
            let znum = o.zone_number;
            let msg = {
                let mut w = world.lock().await;
                w.obj_protos.insert(o.vnum, o.obj.clone());
                match save_objects(&w, &data_dir, znum) {
                    Ok(n) => format!("\r\nObject {} saved ({} object(s) written to zone {}).\r\n", o.vnum, n, znum),
                    Err(e) => format!("\r\nObject {} saved to memory (disk write failed: {e}).\r\n", o.vnum),
                }
            };
            (msg, false)
        }
        _ => (oedit_menu(o), true),
    }
}

enum FlagKind { Extra, Wear, Affect }

fn toggle_flag(line: &str, o: &mut Oedit, kind: FlagKind) -> (String, bool) {
    let t = line.trim();
    if t == "0" || t.is_empty() {
        o.mode = OeditMode::Main;
        return (oedit_menu(o), true);
    }
    if let Ok(n) = t.parse::<usize>() {
        match kind {
            FlagKind::Extra => if n >= 1 && n <= EXTRA_BIT_NAMES.len() { o.obj.extra_flags[0] ^= 1 << (n - 1); },
            FlagKind::Wear  => if n >= 1 && n <= WEAR_BIT_NAMES.len()  { o.obj.wear_flags[0]  ^= 1 << (n - 1); },
            FlagKind::Affect => if n >= 1 && n < AFFECT_BIT_NAMES.len() { o.obj.affect_flags[0] ^= 1 << n; },
        }
    }
    match kind {
        FlagKind::Extra => (flag_toggle_menu(o.obj.extra_flags[0], EXTRA_BIT_NAMES, "Extra"), true),
        FlagKind::Wear  => (flag_toggle_menu(o.obj.wear_flags[0], WEAR_BIT_NAMES, "Wear"), true),
        FlagKind::Affect => (flag_toggle_menu_affect(o.obj.affect_flags[0]), true),
    }
}

fn oedit_applies_choice(line: &str, o: &mut Oedit) -> (String, bool) {
    let t = line.trim();
    if t == "0" || t.is_empty() { o.mode = OeditMode::Main; return (oedit_menu(o), true); }
    if t.eq_ignore_ascii_case("n") {
        if o.obj.affected.len() >= 6 { return ("Maximum of 6 applies.\r\n".to_string() + &applies_menu(o), true); }
        o.obj.affected.push(ObjAffect { location: 0, modifier: 0 });
        let slot = o.obj.affected.len() - 1;
        o.mode = OeditMode::ApplyType(slot);
        return (apply_type_menu(), true);
    }
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts[0].eq_ignore_ascii_case("d") {
        if let Some(idx) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
            if idx >= 1 && idx <= o.obj.affected.len() { o.obj.affected.remove(idx - 1); }
        }
        return (applies_menu(o), true);
    }
    if let Ok(idx) = parts[0].parse::<usize>() {
        if idx >= 1 && idx <= o.obj.affected.len() {
            o.mode = OeditMode::ApplyType(idx - 1);
            return (apply_type_menu(), true);
        }
    }
    (applies_menu(o), true)
}

fn set_apply_location(o: &mut Oedit, slot: usize, loc: i32) {
    if let Some(a) = o.obj.affected.get_mut(slot) { a.location = loc; }
}
fn set_apply_modifier(o: &mut Oedit, slot: usize, m: i32) {
    if let Some(a) = o.obj.affected.get_mut(slot) {
        a.modifier = m;
        if m == 0 || a.location == 0 { o.obj.affected.remove(slot); }
    }
}

fn oedit_extra_choice(line: &str, o: &mut Oedit) -> (String, bool) {
    let t = line.trim();
    if t == "0" || t.is_empty() { o.mode = OeditMode::Main; return (oedit_menu(o), true); }
    if t.eq_ignore_ascii_case("n") {
        o.mode = OeditMode::ExtraNewKeyword;
        return ("Enter keyword(s) for the new extra description:\r\n".to_string(), true);
    }
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts[0].eq_ignore_ascii_case("d") {
        if let Some(idx) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
            if idx >= 1 && idx <= o.obj.extras.len() { o.obj.extras.remove(idx - 1); }
        }
        return (extra_menu_obj(o), true);
    }
    if parts[0].eq_ignore_ascii_case("k") {
        if let Some(idx) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
            if idx >= 1 && idx <= o.obj.extras.len() {
                o.mode = OeditMode::ExtraKeyword(idx - 1);
                return ("Enter new keyword(s):\r\n".to_string(), true);
            }
        }
        return (extra_menu_obj(o), true);
    }
    if let Ok(idx) = parts[0].parse::<usize>() {
        if idx >= 1 && idx <= o.obj.extras.len() {
            o.text.clear(); o.mode = OeditMode::ExtraDesc(idx - 1);
            return (format!("Enter the extra description (replaces current):\r\n{TEXT_END_HINT}"), true);
        }
    }
    (extra_menu_obj(o), true)
}

// ---- oedit menus ----------------------------------------------------------

fn oedit_menu(o: &Oedit) -> String {
    let ob = &o.obj;
    let mut s = String::new();
    s.push_str(&format!("\r\n-- Item number : [@c{}@n]   Zone: [@c{}@n]\r\n", o.vnum, o.zone_number));
    s.push_str(&format!("@g1@n) Keywords : @y{}@n\r\n", ob.name));
    s.push_str(&format!("@g2@n) S-Desc   : @y{}@n\r\n", ob.short_description));
    s.push_str(&format!("@g3@n) L-Desc   : @y{}@n\r\n", ob.description));
    s.push_str(&format!("@g4@n) A-Desc   : @y{}@n\r\n",
        if ob.action_description.is_empty() { "Not set." } else { ob.action_description.trim_end_matches(['\r','\n']) }));
    s.push_str(&format!("@g5@n) Type        : @c{}@n\r\n", item_type_name(ob.item_type)));
    s.push_str(&format!("@g6@n) Extra flags : @c{}@n\r\n", sprintbit(ob.extra_flags[0], EXTRA_BIT_NAMES)));
    s.push_str(&format!("@g7@n) Wear flags  : @c{}@n\r\n", sprintbit(ob.wear_flags[0], WEAR_BIT_NAMES)));
    s.push_str(&format!("@g8@n) Weight      : @c{}@n\r\n", ob.weight));
    s.push_str(&format!("@g9@n) Cost        : @c{}@n\r\n", ob.cost));
    s.push_str(&format!("@gA@n) Cost/Day    : @c{}@n\r\n", ob.rent));
    s.push_str(&format!("@gB@n) Timer       : @c{}@n\r\n", ob.timer));
    s.push_str(&format!("@gC@n) Values      : @c{} {} {} {}@n\r\n", ob.value[0], ob.value[1], ob.value[2], ob.value[3]));
    s.push_str("@gD@n) Applies menu\r\n");
    s.push_str(&format!("@gE@n) Extra descriptions menu : @c{}@n\r\n",
        if ob.extras.is_empty() { "Not set." } else { "Set." }));
    s.push_str(&format!("@gM@n) Min Level   : @c{}@n\r\n", ob.level));
    s.push_str(&format!("@gP@n) Perm Affects: @c{}@n\r\n", sprintbit_affect(ob.affect_flags[0])));
    s.push_str("@gW@n) Copy object\r\n@gX@n) Delete object\r\n@gQ@n) Quit (save)\r\n");
    s.push_str("Enter choice : ");
    s
}

fn type_menu() -> String {
    let mut s = String::from("\r\n");
    for (i, n) in ITEM_TYPE_NAMES.iter().enumerate() {
        s.push_str(&format!("@g{:2}@n) {:<16}", i, n));
        if (i + 1) % 3 == 0 { s.push_str("\r\n"); }
    }
    s.push_str("\r\nEnter item type : ");
    s
}

fn flag_toggle_menu(flags: u32, names: &[&str], label: &str) -> String {
    let mut s = String::from("\r\n");
    for (i, n) in names.iter().enumerate() {
        s.push_str(&format!("@g{:2}@n) {:<14}", i + 1, n));
        if (i + 1) % 3 == 0 { s.push_str("\r\n"); }
    }
    if names.len() % 3 != 0 { s.push_str("\r\n"); }
    s.push_str(&format!("{label} flags : @c{}@n\r\nToggle which (0 to quit) : ", sprintbit(flags, names)));
    s
}

fn flag_toggle_menu_affect(flags: u32) -> String {
    let mut s = String::from("\r\n");
    for i in 1..AFFECT_BIT_NAMES.len() {
        s.push_str(&format!("@g{:2}@n) {:<12}", i, AFFECT_BIT_NAMES[i]));
        if i % 3 == 0 { s.push_str("\r\n"); }
    }
    s.push_str(&format!("\r\nPerm affects : @c{}@n\r\nToggle which (0 to quit) : ", sprintbit_affect(flags)));
    s
}

fn sprintbit_affect(flags: u32) -> String {
    let mut parts = Vec::new();
    for i in 1..AFFECT_BIT_NAMES.len() {
        if flags & (1 << i) != 0 { parts.push(AFFECT_BIT_NAMES[i]); }
    }
    if parts.is_empty() { "<None>".to_string() } else { parts.join(" ") }
}

fn values_menu(o: &Oedit) -> String {
    let labels = value_labels(o.obj.item_type);
    let mut s = format!("\r\n-- Values for a {} --\r\n", item_type_name(o.obj.item_type));
    for i in 0..4 {
        let lbl = if labels[i].is_empty() { "(unused)" } else { labels[i] };
        s.push_str(&format!("@g{}@n) {:<28}: @c{}@n\r\n", i + 1, lbl, o.obj.value[i]));
    }
    s.push_str("Edit which value (0 to quit) : ");
    s
}

fn applies_menu(o: &Oedit) -> String {
    let mut s = String::from("\r\n-- Applies --\r\n");
    if o.obj.affected.is_empty() {
        s.push_str("(none)\r\n");
    } else {
        for (i, a) in o.obj.affected.iter().enumerate() {
            s.push_str(&format!("@g{:2}@n) {:<14} : {}\r\n", i + 1, apply_name(a.location), a.modifier));
        }
    }
    s.push_str("@gN@n) New apply   Edit <num>, `d <num>` delete, 0 to quit : ");
    s
}

fn apply_type_menu() -> String {
    let mut s = String::from("\r\n");
    for (i, n) in APPLY_NAMES.iter().enumerate() {
        s.push_str(&format!("@g{:2}@n) {:<14}", i, n));
        if (i + 1) % 3 == 0 { s.push_str("\r\n"); }
    }
    s.push_str("\r\nEnter apply type : ");
    s
}

fn extra_menu_obj(o: &Oedit) -> String {
    let mut s = String::from("\r\n-- Extra descriptions --\r\n");
    if o.obj.extras.is_empty() {
        s.push_str("(none)\r\n");
    } else {
        for (i, e) in o.obj.extras.iter().enumerate() {
            s.push_str(&format!("@g{:2}@n) {}\r\n", i + 1, e.keyword));
        }
    }
    s.push_str("@gN@n) New extra description\r\n");
    s.push_str("Edit <num>, `k <num>` keyword, `d <num>` delete, 0 to quit : ");
    s
}

// ---- genobj-equivalent disk save ------------------------------------------

/// Write every object proto in zone `zone_number`'s vnum range to
/// `<data_dir>/world/obj/<zone_number>.obj`.  Returns the count written.
pub fn save_objects(w: &World, data_dir: &str, zone_number: ZoneVnum) -> std::io::Result<usize> {
    use std::io::Write;
    let zone = w.zones.get(&zone_number)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such zone"))?;
    let dir = format!("{data_dir}/world/obj");
    std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/{zone_number}.obj");
    let tmp = format!("{path}.new");
    let mut f = std::fs::File::create(&tmp)?;
    let mut count = 0;
    for (&vnum, ob) in w.obj_protos.range(zone.bot..=zone.top) {
        count += 1;
        writeln!(f, "#{vnum}")?;
        writeln!(f, "{}~", if ob.name.is_empty() { "undefined" } else { &ob.name })?;
        writeln!(f, "{}~", if ob.short_description.is_empty() { "undefined" } else { &ob.short_description })?;
        writeln!(f, "{}~", if ob.description.is_empty() { "undefined" } else { &ob.description })?;
        writeln!(f, "{}~", ob.action_description.trim_end_matches(['\r', '\n']))?;
        // type + 4 extra + 4 wear + 4 perm-affect (decimal; loader reads digits)
        writeln!(f, "{} {} {} {} {} {} {} {} {} {} {} {} {}",
            ob.item_type,
            ob.extra_flags[0], ob.extra_flags[1], ob.extra_flags[2], ob.extra_flags[3],
            ob.wear_flags[0], ob.wear_flags[1], ob.wear_flags[2], ob.wear_flags[3],
            ob.affect_flags[0], ob.affect_flags[1], ob.affect_flags[2], ob.affect_flags[3])?;
        writeln!(f, "{} {} {} {}", ob.value[0], ob.value[1], ob.value[2], ob.value[3])?;
        writeln!(f, "{} {} {} {} {}", ob.weight, ob.cost, ob.rent, ob.level, ob.timer)?;
        for e in &ob.extras {
            writeln!(f, "E")?;
            writeln!(f, "{}~", e.keyword)?;
            writeln!(f, "{}~", e.description.trim_end_matches(['\r', '\n']))?;
        }
        for a in &ob.affected {
            if a.modifier != 0 && a.location != 0 {
                writeln!(f, "A")?;
                writeln!(f, "{} {}", a.location, a.modifier)?;
            }
        }
    }
    writeln!(f, "$~")?;
    f.flush()?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    Ok(count)
}

// ===========================================================================
// Mobile editor (medit) — port of stock medit.c / genmob.c
// ===========================================================================

use crate::world::{MobProto, MobVnum};

/// MOB action-flag names in stock action_bits order (bit 0 = SPEC, …).
/// Used by the editor so saved files match the on-disk bit encoding,
/// independent of rwb's gameplay `MOB_*` constants (which differ for the
/// per-alignment aggro bits).
pub const MOB_FLAG_NAMES: &[&str] = &[
    "SPEC", "SENTINEL", "SCAVENGER", "ISNPC", "AWARE", "AGGR", "STAY-ZONE",
    "WIMPY", "AGGR_EVIL", "AGGR_GOOD", "AGGR_NEUTRAL", "MEMORY", "HELPER",
    "NO_CHARM", "NO_SUMMN", "NO_SLEEP", "NO_BASH", "NO_BLIND", "NO_KILL",
];

pub const POSITION_NAMES: &[&str] = &[
    "Dead", "Mortally wounded", "Incapacitated", "Stunned", "Sleeping",
    "Resting", "Sitting", "Fighting", "Standing",
];

pub const GENDER_NAMES: &[&str] = &["neutral", "male", "female"];

fn position_name(p: i32) -> &'static str {
    POSITION_NAMES.get(p as usize).copied().unwrap_or("?")
}
fn gender_name(g: i32) -> &'static str {
    GENDER_NAMES.get(g as usize).copied().unwrap_or("?")
}

#[derive(Debug)]
pub struct Medit {
    vnum: MobVnum,
    zone_number: ZoneVnum,
    mob: MobProto,
    is_new: bool,
    mode: MeditMode,
    text: Vec<String>,
}

#[derive(Debug, Clone)]
enum MeditMode {
    Main,
    Keywords,
    ShortDesc,
    LongDesc,
    DetailDesc,
    Position,
    DefaultPos,
    Sex,
    NpcFlags,
    AffFlags,
    StatsMenu,
    StatLevel,
    StatAlign,
    StatHitroll,
    StatAc,
    StatHpDice,
    StatHpSize,
    StatHpAdd,
    StatDamDice,
    StatDamSize,
    StatDamRoll,
    StatGold,
    StatExp,
    StatMana,
    StatMove,
    CopyFrom,
    DeleteConfirm,
}

pub async fn start_medit(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    let mut toks = arg.split_whitespace();
    let first = toks.next().unwrap_or("");

    if first.eq_ignore_ascii_case("save") {
        let znum: Option<ZoneVnum> = toks.next().and_then(|s| s.parse().ok());
        let znum = match znum {
            Some(z) => z,
            None => { let w = world.lock().await;
                      zone_of_room(&w, me.current_room).map(|z| z.number).unwrap_or(-1) }
        };
        let data_dir = players.lock().await.data_dir().to_string();
        let w = world.lock().await;
        return match save_mobs(&w, &data_dir, znum) {
            Ok(n) => out(format!("\r\nSaved {n} mobile(s) in zone {znum}.\r\n")),
            Err(e) => out(format!("\r\nSave failed: {e}\r\n")),
        };
    }

    let vnum: MobVnum = match first.parse() {
        Ok(v) => v,
        Err(_) => return out("\r\nUsage: medit <vnum> | medit save [zone]\r\n"),
    };

    let (mob, is_new, zone_number) = {
        let w = world.lock().await;
        match w.mob_protos.get(&vnum) {
            Some(m) => (m.clone(), false, zone_of_vnum(&w, vnum).map(|z| z.number).unwrap_or(0)),
            None => match zone_of_vnum(&w, vnum) {
                Some(z) => {
                    let mut m = MobProto::default();
                    m.vnum = vnum;
                    m.name = "mob unfinished".to_string();
                    m.short_descr = "an unfinished mob".to_string();
                    m.long_descr = "An unfinished mob stands here.".to_string();
                    m.description = String::new();
                    m.level = 1;
                    m.hp_dice = 1; m.hp_size = 1; m.hp_add = 0;
                    m.dam_dice = 1; m.dam_size = 1; m.damroll = 0;
                    m.position = 8; m.default_pos = 8; // Standing
                    m.mob_flags[0] = 1 << 3; // ISNPC
                    (m, true, z.number)
                }
                None => return out(format!(
                    "\r\nVnum {vnum} is not within any existing zone's range.\r\n")),
            },
        }
    };
    let medit = Medit { vnum, zone_number, mob, is_new, mode: MeditMode::Main, text: Vec::new() };
    let menu = medit_menu(&medit);
    me.olc = Some(OlcSession { editor: Editor::Mob(medit) });
    out(menu)
}

async fn medit_input(
    line: &str,
    m: &mut Medit,
    world: &Arc<Mutex<World>>,
    players: &Arc<Mutex<PlayerDb>>,
) -> (String, bool) {
    let mode = m.mode.clone();
    macro_rules! back { () => {{ m.mode = MeditMode::Main; (medit_menu(m), true) }} }
    macro_rules! stats { () => {{ m.mode = MeditMode::StatsMenu; (stats_menu(m), true) }} }
    match mode {
        MeditMode::Main => medit_main_choice(line, m, world, players).await,

        MeditMode::Keywords  => { m.mob.name = strip_ctrl(line, 100); back!() }
        MeditMode::ShortDesc => { m.mob.short_descr = strip_ctrl(line, 100); back!() }
        MeditMode::LongDesc  => { m.mob.long_descr = strip_ctrl(line, 200); back!() }
        MeditMode::DetailDesc => {
            if is_text_end(line) { m.mob.description = m.text.join("\r\n"); m.text.clear(); back!() }
            else { m.text.push(line.to_string()); (String::new(), true) }
        }

        MeditMode::Position => {
            if let Ok(n) = line.trim().parse::<i32>() {
                if n >= 0 && (n as usize) < POSITION_NAMES.len() { m.mob.position = n; return back!(); }
            }
            (position_menu("Position"), true)
        }
        MeditMode::DefaultPos => {
            if let Ok(n) = line.trim().parse::<i32>() {
                if n >= 0 && (n as usize) < POSITION_NAMES.len() { m.mob.default_pos = n; return back!(); }
            }
            (position_menu("Default position"), true)
        }
        MeditMode::Sex => {
            if let Ok(n) = line.trim().parse::<i32>() {
                if n >= 0 && (n as usize) < GENDER_NAMES.len() { m.mob.sex = n; return back!(); }
            }
            (sex_menu(), true)
        }

        MeditMode::NpcFlags => {
            let t = line.trim();
            if t == "0" || t.is_empty() { return back!(); }
            if let Ok(n) = t.parse::<usize>() {
                if n >= 1 && n <= MOB_FLAG_NAMES.len() { m.mob.mob_flags[0] ^= 1 << (n - 1); }
            }
            (flag_toggle_menu(m.mob.mob_flags[0], MOB_FLAG_NAMES, "NPC"), true)
        }
        MeditMode::AffFlags => {
            let t = line.trim();
            if t == "0" || t.is_empty() { return back!(); }
            if let Ok(n) = t.parse::<usize>() {
                if n >= 1 && n < AFFECT_BIT_NAMES.len() { m.mob.aff_flags[0] ^= 1 << n; }
            }
            (flag_toggle_menu_affect(m.mob.aff_flags[0]), true)
        }

        MeditMode::StatsMenu => medit_stats_choice(line, m),
        MeditMode::StatLevel   => { m.mob.level    = line.trim().parse().unwrap_or(m.mob.level); stats!() }
        MeditMode::StatAlign   => { m.mob.alignment= line.trim().parse::<i32>().unwrap_or(m.mob.alignment).clamp(-1000,1000); stats!() }
        MeditMode::StatHitroll => { m.mob.hitroll  = line.trim().parse().unwrap_or(m.mob.hitroll); stats!() }
        MeditMode::StatAc      => { m.mob.ac       = line.trim().parse().unwrap_or(m.mob.ac); stats!() }
        MeditMode::StatHpDice  => { m.mob.hp_dice  = line.trim().parse().unwrap_or(m.mob.hp_dice); stats!() }
        MeditMode::StatHpSize  => { m.mob.hp_size  = line.trim().parse().unwrap_or(m.mob.hp_size); stats!() }
        MeditMode::StatHpAdd   => { m.mob.hp_add   = line.trim().parse().unwrap_or(m.mob.hp_add); stats!() }
        MeditMode::StatDamDice => { m.mob.dam_dice = line.trim().parse().unwrap_or(m.mob.dam_dice); stats!() }
        MeditMode::StatDamSize => { m.mob.dam_size = line.trim().parse().unwrap_or(m.mob.dam_size); stats!() }
        MeditMode::StatDamRoll => { m.mob.damroll  = line.trim().parse().unwrap_or(m.mob.damroll); stats!() }
        MeditMode::StatGold    => { m.mob.gold     = line.trim().parse().unwrap_or(m.mob.gold); stats!() }
        MeditMode::StatExp     => { m.mob.exp      = line.trim().parse().unwrap_or(m.mob.exp); stats!() }
        MeditMode::StatMana    => { m.mob.mana     = line.trim().parse().unwrap_or(m.mob.mana); stats!() }
        MeditMode::StatMove    => { m.mob.mv       = line.trim().parse().unwrap_or(m.mob.mv); stats!() }

        MeditMode::CopyFrom => {
            if let Ok(v) = line.trim().parse::<MobVnum>() {
                let src = { world.lock().await.mob_protos.get(&v).cloned() };
                if let Some(mut src) = src {
                    src.vnum = m.mob.vnum; m.mob = src; m.mode = MeditMode::Main;
                    return (format!("\r\nCopied mob {v}.\r\n{}", medit_menu(m)), true);
                }
            }
            m.mode = MeditMode::Main; (format!("\r\nNo such mob.\r\n{}", medit_menu(m)), true)
        }
        MeditMode::DeleteConfirm => {
            if line.trim().eq_ignore_ascii_case("y") {
                let data_dir = players.lock().await.data_dir().to_string();
                let znum = m.zone_number;
                let msg = {
                    let mut w = world.lock().await;
                    w.mob_protos.remove(&m.vnum);
                    match save_mobs(&w, &data_dir, znum) {
                        Ok(_) => format!("\r\nMobile {} deleted.\r\n", m.vnum),
                        Err(e) => format!("\r\nMobile {} deleted (disk save failed: {e}).\r\n", m.vnum),
                    }
                };
                (msg, false)
            } else { m.mode = MeditMode::Main; (format!("\r\nCancelled.\r\n{}", medit_menu(m)), true) }
        }
    }
}

async fn medit_main_choice(
    line: &str, m: &mut Medit,
    world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>,
) -> (String, bool) {
    match line.trim().to_ascii_uppercase().as_str() {
        "1" => { m.mode = MeditMode::Sex; (sex_menu(), true) }
        "2" => { m.mode = MeditMode::Keywords; ("Enter keywords:\r\n".to_string(), true) }
        "3" => { m.mode = MeditMode::ShortDesc; ("Enter short description:\r\n".to_string(), true) }
        "4" => { m.mode = MeditMode::LongDesc; ("Enter long description (room line):\r\n".to_string(), true) }
        "5" => { m.text.clear(); m.mode = MeditMode::DetailDesc;
                 (format!("Enter detailed (look) description:\r\n{TEXT_END_HINT}"), true) }
        "6" => { m.mode = MeditMode::Position; (position_menu("Position"), true) }
        "7" => { m.mode = MeditMode::DefaultPos; (position_menu("Default position"), true) }
        "9" => { m.mode = MeditMode::StatsMenu; (stats_menu(m), true) }
        "A" => { m.mode = MeditMode::NpcFlags; (flag_toggle_menu(m.mob.mob_flags[0], MOB_FLAG_NAMES, "NPC"), true) }
        "B" => { m.mode = MeditMode::AffFlags; (flag_toggle_menu_affect(m.mob.aff_flags[0]), true) }
        "W" => { m.mode = MeditMode::CopyFrom; ("Copy from which mob vnum?\r\n".to_string(), true) }
        "X" => { m.mode = MeditMode::DeleteConfirm; ("Delete this mob? (y/n)\r\n".to_string(), true) }
        "Q" => {
            let data_dir = players.lock().await.data_dir().to_string();
            let znum = m.zone_number;
            let msg = {
                let mut w = world.lock().await;
                w.mob_protos.insert(m.vnum, m.mob.clone());
                match save_mobs(&w, &data_dir, znum) {
                    Ok(n) => format!("\r\nMobile {} saved ({} mob(s) written to zone {}).\r\n", m.vnum, n, znum),
                    Err(e) => format!("\r\nMobile {} saved to memory (disk write failed: {e}).\r\n", m.vnum),
                }
            };
            (msg, false)
        }
        _ => (medit_menu(m), true),
    }
}

fn medit_stats_choice(line: &str, m: &mut Medit) -> (String, bool) {
    match line.trim().to_ascii_uppercase().as_str() {
        "1" => { m.mode = MeditMode::StatLevel;   ("Enter level:\r\n".to_string(), true) }
        "2" => { m.mode = MeditMode::StatAlign;   ("Enter alignment (-1000..1000):\r\n".to_string(), true) }
        "3" => { m.mode = MeditMode::StatHitroll; ("Enter hitroll:\r\n".to_string(), true) }
        "4" => { m.mode = MeditMode::StatAc;      ("Enter armor class (e.g. -90..100):\r\n".to_string(), true) }
        "5" => { m.mode = MeditMode::StatHpDice;  ("Enter number of HP dice:\r\n".to_string(), true) }
        "6" => { m.mode = MeditMode::StatHpSize;  ("Enter size of HP dice:\r\n".to_string(), true) }
        "7" => { m.mode = MeditMode::StatHpAdd;   ("Enter HP add (bonus):\r\n".to_string(), true) }
        "8" => { m.mode = MeditMode::StatDamDice; ("Enter number of damage dice:\r\n".to_string(), true) }
        "9" => { m.mode = MeditMode::StatDamSize; ("Enter size of damage dice:\r\n".to_string(), true) }
        "A" => { m.mode = MeditMode::StatDamRoll; ("Enter damroll (+damage):\r\n".to_string(), true) }
        "B" => { m.mode = MeditMode::StatGold;    ("Enter gold:\r\n".to_string(), true) }
        "C" => { m.mode = MeditMode::StatExp;     ("Enter experience:\r\n".to_string(), true) }
        "D" => { m.mode = MeditMode::StatMana;    ("Enter mana:\r\n".to_string(), true) }
        "E" => { m.mode = MeditMode::StatMove;    ("Enter movement:\r\n".to_string(), true) }
        "0" => { m.mode = MeditMode::Main; (medit_menu(m), true) }
        _ => (stats_menu(m), true),
    }
}

// ---- medit menus ----------------------------------------------------------

fn medit_menu(m: &Medit) -> String {
    let mb = &m.mob;
    let mut s = String::new();
    s.push_str(&format!("\r\n-- Mob Number : [@c{}@n]   Zone: [@c{}@n]\r\n", m.vnum, m.zone_number));
    s.push_str(&format!("@g1@n) Sex     : @y{}@n\r\n", gender_name(mb.sex)));
    s.push_str(&format!("@g2@n) Keywords: @y{}@n\r\n", mb.name));
    s.push_str(&format!("@g3@n) S-Desc  : @y{}@n\r\n", mb.short_descr));
    s.push_str(&format!("@g4@n) L-Desc  : @y{}@n\r\n", mb.long_descr.trim_end_matches(['\r','\n'])));
    s.push_str(&format!("@g5@n) D-Desc  : @y{}@n\r\n",
        if mb.description.is_empty() { "Not set." } else { mb.description.trim_end_matches(['\r','\n']) }));
    s.push_str(&format!("@g6@n) Position: @y{}@n\r\n", position_name(mb.position)));
    s.push_str(&format!("@g7@n) Default : @y{}@n\r\n", position_name(mb.default_pos)));
    s.push_str("@g9@n) Stats menu...\r\n");
    s.push_str(&format!("@gA@n) NPC Flags : @c{}@n\r\n", sprintbit(mb.mob_flags[0], MOB_FLAG_NAMES)));
    s.push_str(&format!("@gB@n) AFF Flags : @c{}@n\r\n", sprintbit_affect(mb.aff_flags[0])));
    s.push_str("@gW@n) Copy mob\r\n@gX@n) Delete mob\r\n@gQ@n) Quit (save)\r\n");
    s.push_str("Enter choice : ");
    s
}

fn stats_menu(m: &Medit) -> String {
    let mb = &m.mob;
    let mut s = String::from("\r\n-- Mob stats --\r\n");
    s.push_str(&format!("@g1@n) Level     : @c{}@n\r\n", mb.level));
    s.push_str(&format!("@g2@n) Alignment : @c{}@n\r\n", mb.alignment));
    s.push_str(&format!("@g3@n) Hitroll   : @c{}@n\r\n", mb.hitroll));
    s.push_str(&format!("@g4@n) Armor     : @c{}@n\r\n", mb.ac));
    s.push_str(&format!("@g5@n) HP dice   : @c{}@n\r\n", mb.hp_dice));
    s.push_str(&format!("@g6@n) HP size   : @c{}@n\r\n", mb.hp_size));
    s.push_str(&format!("@g7@n) HP add    : @c{}@n\r\n", mb.hp_add));
    s.push_str(&format!("@g8@n) Dam dice  : @c{}@n\r\n", mb.dam_dice));
    s.push_str(&format!("@g9@n) Dam size  : @c{}@n\r\n", mb.dam_size));
    s.push_str(&format!("@gA@n) Damroll   : @c{}@n\r\n", mb.damroll));
    s.push_str(&format!("@gB@n) Gold      : @c{}@n\r\n", mb.gold));
    s.push_str(&format!("@gC@n) Exp       : @c{}@n\r\n", mb.exp));
    s.push_str(&format!("@gD@n) Mana      : @c{}@n\r\n", mb.mana));
    s.push_str(&format!("@gE@n) Move      : @c{}@n\r\n", mb.mv));
    s.push_str("@g0@n) Back\r\nEnter choice : ");
    s
}

fn position_menu(label: &str) -> String {
    let mut s = String::from("\r\n");
    for (i, n) in POSITION_NAMES.iter().enumerate() {
        s.push_str(&format!("@g{}@n) {:<18}", i, n));
        if (i + 1) % 3 == 0 { s.push_str("\r\n"); }
    }
    s.push_str(&format!("\r\nEnter {label} : "));
    s
}

fn sex_menu() -> String {
    "\r\n@g0@n) neutral\r\n@g1@n) male\r\n@g2@n) female\r\nEnter sex : ".to_string()
}

// ---- genmob-equivalent disk save ------------------------------------------

/// Write every mob proto in zone `zone_number`'s vnum range to
/// `<data_dir>/world/mob/<zone_number>.mob` in the simple (`S`) format.
/// Returns the count written.
pub fn save_mobs(w: &World, data_dir: &str, zone_number: ZoneVnum) -> std::io::Result<usize> {
    use std::io::Write;
    let zone = w.zones.get(&zone_number)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such zone"))?;
    let dir = format!("{data_dir}/world/mob");
    std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/{zone_number}.mob");
    let tmp = format!("{path}.new");
    let mut f = std::fs::File::create(&tmp)?;
    let mut count = 0;
    for (&vnum, mb) in w.mob_protos.range(zone.bot..=zone.top) {
        count += 1;
        writeln!(f, "#{vnum}")?;
        writeln!(f, "{}~", if mb.name.is_empty() { "mob" } else { &mb.name })?;
        writeln!(f, "{}~", if mb.short_descr.is_empty() { "a mob" } else { &mb.short_descr })?;
        writeln!(f, "{}~", mb.long_descr.trim_end_matches(['\r', '\n']))?;
        writeln!(f, "{}~", mb.description.trim_end_matches(['\r', '\n']))?;
        // mob_flags0..3 aff_flags0..3 alignment S   (decimal flags)
        writeln!(f, "{} {} {} {} {} {} {} {} {} S",
            mb.mob_flags[0], mb.mob_flags[1], mb.mob_flags[2], mb.mob_flags[3],
            mb.aff_flags[0], mb.aff_flags[1], mb.aff_flags[2], mb.aff_flags[3],
            mb.alignment)?;
        // level thac0 ac  hp_d d hp_s + hp_a   dam_d d dam_s + dam_a
        let thac0 = 20 - mb.hitroll;
        let ac_field = mb.ac / 10;
        writeln!(f, "{} {} {} {}d{}+{} {}d{}+{}",
            mb.level, thac0, ac_field,
            mb.hp_dice, mb.hp_size, mb.hp_add,
            mb.dam_dice, mb.dam_size, mb.damroll)?;
        writeln!(f, "{} {}", mb.gold, mb.exp)?;
        writeln!(f, "{} {} {}", mb.position, mb.default_pos, mb.sex)?;
    }
    writeln!(f, "$~")?;
    f.flush()?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    Ok(count)
}

// ===========================================================================
// Zone editor (zedit) — port of stock zedit.c / genzon.c
// ===========================================================================

use crate::world::ResetCmd;

pub const ZONE_FLAG_NAMES: &[&str] = &[
    "CLOSED", "NO_IMMORT", "QUEST", "GRID", "NOBUILD", "!ASTRAL", "WORLDMAP",
];

#[derive(Debug)]
pub struct Zedit {
    number: ZoneVnum,
    bot: RoomVnum,
    top: RoomVnum,
    name: String,
    builders: String,
    lifespan: i32,
    reset_mode: i32,
    zone_flags: [u32; 4],
    min_level: i32,
    max_level: i32,
    commands: Vec<ResetCmd>,
    mode: ZeditMode,
    /// Index being edited (None = appending a new command).
    edit_idx: Option<usize>,
    work: ResetCmd,
}

#[derive(Debug, Clone)]
enum ZeditMode {
    Main,
    Name,
    Builders,
    Lifespan,
    ResetMode,
    Bottom,
    Top,
    MinLevel,
    MaxLevel,
    Flags,
    CmdList,
    CmdNewType,
    CmdIf,
    CmdArg1,
    CmdArg2,
    CmdArg3,
    DeleteConfirm,
}

pub async fn start_zedit(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    let mut toks = arg.split_whitespace();
    let first = toks.next().unwrap_or("");

    if first.eq_ignore_ascii_case("save") {
        let znum: Option<ZoneVnum> = toks.next().and_then(|s| s.parse().ok());
        let znum = match znum {
            Some(z) => z,
            None => { let w = world.lock().await;
                      zone_of_room(&w, me.current_room).map(|z| z.number).unwrap_or(-1) }
        };
        let data_dir = players.lock().await.data_dir().to_string();
        let w = world.lock().await;
        return match save_zone(&w, &data_dir, znum) {
            Ok(n) => out(format!("\r\nSaved zone {znum} ({n} reset commands).\r\n")),
            Err(e) => out(format!("\r\nSave failed: {e}\r\n")),
        };
    }

    // Argument is a zone NUMBER (not a room vnum), defaulting to current zone.
    let znum: ZoneVnum = if first.is_empty() {
        let w = world.lock().await;
        match zone_of_room(&w, me.current_room) { Some(z) => z.number, None => -1 }
    } else {
        match first.parse() { Ok(v) => v, Err(_) => return out("\r\nUsage: zedit <zone#> | zedit save [zone]\r\n") }
    };

    let z = {
        let w = world.lock().await;
        match w.zones.get(&znum) {
            Some(z) => Zedit {
                number: z.number, bot: z.bot, top: z.top,
                name: z.name.clone(), builders: z.builders.clone(),
                lifespan: z.lifespan, reset_mode: z.reset_mode,
                zone_flags: z.zone_flags, min_level: z.min_level, max_level: z.max_level,
                commands: z.commands.clone(), mode: ZeditMode::Main,
                edit_idx: None, work: ResetCmd { command: 'M', if_flag: 0, arg1: 0, arg2: 0, arg3: 0 },
            },
            None => return out(format!("\r\nNo such zone: {znum}\r\n")),
        }
    };
    let menu = zedit_menu(&z, world).await;
    me.olc = Some(OlcSession { editor: Editor::Zone(z) });
    out(menu)
}

async fn zedit_input(
    line: &str,
    z: &mut Zedit,
    world: &Arc<Mutex<World>>,
    players: &Arc<Mutex<PlayerDb>>,
) -> (String, bool) {
    let mode = z.mode.clone();
    macro_rules! back { () => {{ z.mode = ZeditMode::Main; (zedit_menu(z, world).await, true) }} }
    macro_rules! list { () => {{ z.mode = ZeditMode::CmdList; (cmd_list_menu(z, world).await, true) }} }
    match mode {
        ZeditMode::Main => zedit_main_choice(line, z, world, players).await,

        ZeditMode::Name      => { z.name = strip_ctrl(line, 80); back!() }
        ZeditMode::Builders  => { z.builders = strip_ctrl(line, 80); back!() }
        ZeditMode::Lifespan  => { z.lifespan = line.trim().parse().unwrap_or(z.lifespan); back!() }
        ZeditMode::Bottom    => { z.bot = line.trim().parse().unwrap_or(z.bot); back!() }
        ZeditMode::Top       => { z.top = line.trim().parse().unwrap_or(z.top); back!() }
        ZeditMode::MinLevel  => { z.min_level = line.trim().parse().unwrap_or(z.min_level); back!() }
        ZeditMode::MaxLevel  => { z.max_level = line.trim().parse().unwrap_or(z.max_level); back!() }
        ZeditMode::ResetMode => {
            if let Ok(n) = line.trim().parse::<i32>() { if (0..=2).contains(&n) { z.reset_mode = n; return back!(); } }
            ("\r\n0) Never reset\r\n1) Reset when empty\r\n2) Normal reset\r\nEnter reset mode : ".to_string(), true)
        }
        ZeditMode::Flags => {
            let t = line.trim();
            if t == "0" || t.is_empty() { return back!(); }
            if let Ok(n) = t.parse::<usize>() { if n >= 1 && n <= ZONE_FLAG_NAMES.len() { z.zone_flags[0] ^= 1 << (n - 1); } }
            (flag_toggle_menu(z.zone_flags[0], ZONE_FLAG_NAMES, "Zone"), true)
        }

        ZeditMode::CmdList => zedit_cmd_list_choice(line, z, world).await,

        ZeditMode::CmdNewType => {
            let c = line.trim().chars().next().map(|c| c.to_ascii_uppercase()).unwrap_or(' ');
            if "MOGEPDRTV".contains(c) {
                z.work = ResetCmd { command: c, if_flag: 0, arg1: 0, arg2: 0, arg3: -1 };
                z.mode = ZeditMode::CmdIf;
                ("If-flag (0 = independent, 1 = only if previous succeeded) : ".to_string(), true)
            } else if c == '0' || line.trim().is_empty() {
                list!()
            } else {
                (new_cmd_menu(), true)
            }
        }
        ZeditMode::CmdIf => {
            z.work.if_flag = line.trim().parse().unwrap_or(0);
            z.mode = ZeditMode::CmdArg1;
            (format!("{} : ", arg_label(z.work.command, 1)), true)
        }
        ZeditMode::CmdArg1 => {
            z.work.arg1 = line.trim().parse().unwrap_or(0);
            z.mode = ZeditMode::CmdArg2;
            (format!("{} : ", arg_label(z.work.command, 2)), true)
        }
        ZeditMode::CmdArg2 => {
            z.work.arg2 = line.trim().parse().unwrap_or(0);
            // Commands G and R conventionally have no 3rd arg (-1).
            if matches!(z.work.command, 'G' | 'R') {
                z.work.arg3 = -1;
                commit_work(z);
                return list!();
            }
            z.mode = ZeditMode::CmdArg3;
            (format!("{} : ", arg_label(z.work.command, 3)), true)
        }
        ZeditMode::CmdArg3 => {
            z.work.arg3 = line.trim().parse().unwrap_or(-1);
            commit_work(z);
            list!()
        }

        ZeditMode::DeleteConfirm => {
            // (unused for zones; kept for symmetry) -> back to main
            back!()
        }
    }
}

fn commit_work(z: &mut Zedit) {
    match z.edit_idx {
        Some(i) if i < z.commands.len() => z.commands[i] = z.work.clone(),
        _ => z.commands.push(z.work.clone()),
    }
    z.edit_idx = None;
}

async fn zedit_main_choice(
    line: &str, z: &mut Zedit,
    world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>,
) -> (String, bool) {
    match line.trim().to_ascii_uppercase().as_str() {
        "1" => { z.mode = ZeditMode::Builders; ("Enter builders:\r\n".to_string(), true) }
        "Z" => { z.mode = ZeditMode::Name; ("Enter zone name:\r\n".to_string(), true) }
        "L" => { z.mode = ZeditMode::Lifespan; ("Enter lifespan (minutes):\r\n".to_string(), true) }
        "B" => { z.mode = ZeditMode::Bottom; ("Enter bottom-of-zone vnum:\r\n".to_string(), true) }
        "T" => { z.mode = ZeditMode::Top; ("Enter top-of-zone vnum:\r\n".to_string(), true) }
        "R" => { z.mode = ZeditMode::ResetMode;
                 ("\r\n0) Never reset\r\n1) Reset when empty\r\n2) Normal reset\r\nEnter reset mode : ".to_string(), true) }
        "F" => { z.mode = ZeditMode::Flags; (flag_toggle_menu(z.zone_flags[0], ZONE_FLAG_NAMES, "Zone"), true) }
        "N" => { z.mode = ZeditMode::MinLevel; ("Enter minimum recommended level:\r\n".to_string(), true) }
        "X" => { z.mode = ZeditMode::MaxLevel; ("Enter maximum recommended level:\r\n".to_string(), true) }
        "C" => { z.mode = ZeditMode::CmdList; (cmd_list_menu(z, world).await, true) }
        "Q" => {
            let data_dir = players.lock().await.data_dir().to_string();
            let znum = z.number;
            let msg = {
                let mut w = world.lock().await;
                if let Some(zone) = w.zones.get_mut(&znum) {
                    zone.name = z.name.clone();
                    zone.builders = z.builders.clone();
                    zone.lifespan = z.lifespan;
                    zone.reset_mode = z.reset_mode;
                    zone.bot = z.bot;
                    zone.top = z.top;
                    zone.zone_flags = z.zone_flags;
                    zone.min_level = z.min_level;
                    zone.max_level = z.max_level;
                    zone.commands = z.commands.clone();
                }
                match save_zone(&w, &data_dir, znum) {
                    Ok(n) => format!("\r\nZone {} saved ({} reset commands written).\r\n", znum, n),
                    Err(e) => format!("\r\nZone {} saved to memory (disk write failed: {e}).\r\n", znum),
                }
            };
            (msg, false)
        }
        _ => (zedit_menu(z, world).await, true),
    }
}

async fn zedit_cmd_list_choice(line: &str, z: &mut Zedit, world: &Arc<Mutex<World>>) -> (String, bool) {
    let t = line.trim();
    if t == "0" || t.is_empty() { z.mode = ZeditMode::Main; return (zedit_menu(z, world).await, true); }
    if t.eq_ignore_ascii_case("a") {
        z.edit_idx = None;
        z.mode = ZeditMode::CmdNewType;
        return (new_cmd_menu(), true);
    }
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts[0].eq_ignore_ascii_case("d") {
        if let Some(idx) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
            if idx < z.commands.len() { z.commands.remove(idx); }
        }
        return (cmd_list_menu(z, world).await, true);
    }
    if let Ok(idx) = parts[0].parse::<usize>() {
        if idx < z.commands.len() {
            // Re-enter this command's fields, starting from the type.
            z.edit_idx = Some(idx);
            z.work = z.commands[idx].clone();
            z.mode = ZeditMode::CmdNewType;
            return (format!("Editing command {idx} ({}).\r\n{}", z.work.command, new_cmd_menu()), true);
        }
    }
    (cmd_list_menu(z, world).await, true)
}

// ---- zedit menus ----------------------------------------------------------

async fn zedit_menu(z: &Zedit, _world: &Arc<Mutex<World>>) -> String {
    let reset = match z.reset_mode { 0 => "Never reset", 1 => "Reset when empty", _ => "Normal reset" };
    let lev = if z.min_level >= 0 || z.max_level >= 0 {
        format!("{} to {}", z.min_level, z.max_level)
    } else { "<not set>".to_string() };
    let mut s = String::new();
    s.push_str(&format!("\r\n-- Zone number : [@c{}@n]\r\n", z.number));
    s.push_str(&format!("@g1@n) Builders       : @y{}@n\r\n", z.builders));
    s.push_str(&format!("@gZ@n) Zone name      : @y{}@n\r\n", z.name));
    s.push_str(&format!("@gL@n) Lifespan       : @y{}@n minutes\r\n", z.lifespan));
    s.push_str(&format!("@gB@n) Bottom of zone : @y{}@n\r\n", z.bot));
    s.push_str(&format!("@gT@n) Top of zone    : @y{}@n\r\n", z.top));
    s.push_str(&format!("@gR@n) Reset Mode     : @y{}@n\r\n", reset));
    s.push_str(&format!("@gF@n) Zone Flags     : @c{}@n\r\n", sprintbit(z.zone_flags[0], ZONE_FLAG_NAMES)));
    s.push_str(&format!("@gN@n) Min Level      : @y{}@n\r\n", z.min_level));
    s.push_str(&format!("@gX@n) Max Level      : @y{}@n   ({lev})\r\n", z.max_level));
    s.push_str(&format!("@gC@n) Reset command list ({} commands)\r\n", z.commands.len()));
    s.push_str("@gQ@n) Quit (save)\r\n");
    s.push_str("Enter choice : ");
    s
}

async fn cmd_list_menu(z: &Zedit, world: &Arc<Mutex<World>>) -> String {
    let w = world.lock().await;
    let mut s = String::from("\r\n-- Reset commands --\r\n");
    if z.commands.is_empty() {
        s.push_str("(none)\r\n");
    } else {
        for (i, c) in z.commands.iter().enumerate() {
            s.push_str(&format!("@g{:2}@n - {}\r\n", i, reset_label(c, &w)));
        }
    }
    s.push_str("@ga@n) Add command   `<num>` edit, `d <num>` delete, 0 to quit : ");
    s
}

fn reset_label(c: &ResetCmd, w: &World) -> String {
    let ifs = if c.if_flag != 0 { " (then)" } else { "" };
    let mob_short = |v: i32| w.mob_protos.get(&v).map(|m| m.short_descr.as_str()).unwrap_or("?");
    let obj_short = |v: i32| w.obj_protos.get(&v).map(|o| o.short_description.as_str()).unwrap_or("?");
    match c.command {
        'M' => format!("Load mob {} [{}], max {}, room {}{ifs}", mob_short(c.arg1), c.arg1, c.arg2, c.arg3),
        'O' => format!("Load obj {} [{}], max {}, room {}{ifs}", obj_short(c.arg1), c.arg1, c.arg2, c.arg3),
        'G' => format!("Give obj {} [{}], max {}{ifs}", obj_short(c.arg1), c.arg1, c.arg2),
        'E' => format!("Equip obj {} [{}], pos {}, max {}{ifs}", obj_short(c.arg1), c.arg1, c.arg3, c.arg2),
        'P' => format!("Put obj {} [{}] into obj {} [{}], max {}{ifs}", obj_short(c.arg1), c.arg1, obj_short(c.arg3), c.arg3, c.arg2),
        'D' => format!("Door room {} dir {} state {}{ifs}", c.arg1,
                       DIR_NAMES.get(c.arg2 as usize).copied().unwrap_or("?"), c.arg3),
        'R' => format!("Remove obj {} [{}] from room {}{ifs}", obj_short(c.arg2), c.arg2, c.arg1),
        'T' => format!("Attach trigger [{}] (type {}){ifs}", c.arg2, c.arg1),
        'V' => format!("Set variable (ctx {}, {} {}){ifs}", c.arg1, c.arg2, c.arg3),
        other => format!("{other} {} {} {} {}", c.if_flag, c.arg1, c.arg2, c.arg3),
    }
}

fn new_cmd_menu() -> String {
    "\r\nReset command type:\r\n\
     @gM@n) Load a mobile into a room\r\n\
     @gO@n) Load an object into a room\r\n\
     @gG@n) Give an object to the last-loaded mob\r\n\
     @gE@n) Equip the last-loaded mob with an object\r\n\
     @gP@n) Put an object inside another object\r\n\
     @gD@n) Set a door's state\r\n\
     @gR@n) Remove an object from a room\r\n\
     @gT@n) Attach a trigger\r\n\
     @gV@n) Set a DG variable\r\n\
     @g0@n) Cancel\r\nEnter command type : ".to_string()
}

fn arg_label(cmd: char, n: usize) -> &'static str {
    match (cmd, n) {
        ('M', 1) => "Mob vnum",          ('M', 2) => "Max in world",     ('M', 3) => "Room vnum",
        ('O', 1) => "Object vnum",       ('O', 2) => "Max in world",     ('O', 3) => "Room vnum",
        ('G', 1) => "Object vnum",       ('G', 2) => "Max in world",     ('G', 3) => "(unused)",
        ('E', 1) => "Object vnum",       ('E', 2) => "Max in world",     ('E', 3) => "Wear position",
        ('P', 1) => "Object vnum",       ('P', 2) => "Max in world",     ('P', 3) => "Container obj vnum",
        ('D', 1) => "Room vnum",         ('D', 2) => "Direction (0-5)",  ('D', 3) => "State (0 open,1 closed,2 locked)",
        ('R', 1) => "Room vnum",         ('R', 2) => "Object vnum",      ('R', 3) => "(unused)",
        ('T', 1) => "Trigger type (0 mob,1 obj,2 room)", ('T', 2) => "Trigger vnum", ('T', 3) => "(unused)",
        ('V', 1) => "Context",           ('V', 2) => "Var arg",          ('V', 3) => "Var arg",
        _ => "Value",
    }
}

// ---- genzon-equivalent disk save ------------------------------------------

/// Write zone `zone_number`'s header + reset commands to
/// `<data_dir>/world/zon/<zone_number>.zon`.  Returns the command count.
pub fn save_zone(w: &World, data_dir: &str, zone_number: ZoneVnum) -> std::io::Result<usize> {
    use std::io::Write;
    let zone = w.zones.get(&zone_number)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such zone"))?;
    let dir = format!("{data_dir}/world/zon");
    std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/{zone_number}.zon");
    let tmp = format!("{path}.new");
    let mut f = std::fs::File::create(&tmp)?;
    writeln!(f, "#{zone_number}")?;
    writeln!(f, "{}~", if zone.builders.is_empty() { "None." } else { &zone.builders })?;
    writeln!(f, "{}~", if zone.name.is_empty() { "New Zone" } else { &zone.name })?;
    // bot top lifespan reset_mode flags0..3 min max
    writeln!(f, "{} {} {} {} {} {} {} {} {} {}",
        zone.bot, zone.top, zone.lifespan, zone.reset_mode,
        zone.zone_flags[0], zone.zone_flags[1], zone.zone_flags[2], zone.zone_flags[3],
        zone.min_level, zone.max_level)?;
    for c in &zone.commands {
        let comment = reset_label(c, w);
        writeln!(f, "{} {} {} {} {}\t({})",
            c.command, c.if_flag, c.arg1, c.arg2, c.arg3, comment)?;
    }
    writeln!(f, "S")?;
    writeln!(f, "$")?;
    f.flush()?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    Ok(zone.commands.len())
}

// ===========================================================================
// Quest editor (qedit) — port of stock qedit.c / genqst.c
// ===========================================================================

use crate::world::{Quest, QuestVnum};

#[derive(Debug)]
pub struct Qedit { vnum: QuestVnum, zone_number: ZoneVnum, q: Quest, mode: QeditMode, text: Vec<String> }

#[derive(Debug, Clone)]
enum QeditMode {
    Main, Name, Desc, Info, Done, Quit, Kind, Qm, Target, Flags,
    Prev, Next, Prereq, Gold, Exp, ObjReward, ValuesMenu, ValueEdit(usize), DeleteConfirm,
}

pub async fn start_qedit(arg: &str, me: &mut Character, world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>) -> CmdOutput {
    let mut toks = arg.split_whitespace();
    let first = toks.next().unwrap_or("");
    if first.eq_ignore_ascii_case("save") {
        let znum = toks.next().and_then(|s| s.parse().ok())
            .unwrap_or_else(|| -1);
        let znum = if znum >= 0 { znum } else { let w = world.lock().await; zone_of_room(&w, me.current_room).map(|z| z.number).unwrap_or(-1) };
        let dd = players.lock().await.data_dir().to_string();
        let w = world.lock().await;
        return match save_quests(&w, &dd, znum) { Ok(n) => out(format!("\r\nSaved {n} quest(s) in zone {znum}.\r\n")), Err(e) => out(format!("\r\nSave failed: {e}\r\n")) };
    }
    let vnum: QuestVnum = match first.parse() { Ok(v) => v, Err(_) => return out("\r\nUsage: qedit <vnum> | qedit save [zone]\r\n") };
    let (q, zone_number) = {
        let w = world.lock().await;
        match w.quests.get(&vnum) {
            Some(q) => (q.clone(), zone_of_vnum(&w, vnum).map(|z| z.number).unwrap_or(0)),
            None => match zone_of_vnum(&w, vnum) {
                Some(z) => { let mut q = Quest::default(); q.vnum = vnum; q.name = "An unfinished quest".into();
                    q.kind = -1; q.qm = -1; q.target = -1; q.prev_quest = -1; q.next_quest = -1; q.prereq = -1; q.obj_reward = -1;
                    (q, z.number) }
                None => return out(format!("\r\nVnum {vnum} not within any zone.\r\n")),
            }
        }
    };
    let qe = Qedit { vnum, zone_number, q, mode: QeditMode::Main, text: Vec::new() };
    let menu = qedit_menu(&qe);
    me.olc = Some(OlcSession { editor: Editor::Quest(qe) });
    out(menu)
}

async fn qedit_input(line: &str, q: &mut Qedit, world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>) -> (String, bool) {
    let mode = q.mode.clone();
    macro_rules! back { () => {{ q.mode = QeditMode::Main; (qedit_menu(q), true) }} }
    macro_rules! txt { ($field:ident, $prompt:expr) => {{
        if is_text_end(line) { q.q.$field = q.text.join("\r\n"); q.text.clear(); back!() }
        else { q.text.push(line.to_string()); (String::new(), true) }
    }} }
    match mode {
        QeditMode::Main => qedit_main_choice(line, q, world, players).await,
        QeditMode::Name => { q.q.name = strip_ctrl(line, 100); back!() }
        QeditMode::Desc => txt!(desc, ""),
        QeditMode::Info => txt!(info, ""),
        QeditMode::Done => txt!(done, ""),
        QeditMode::Quit => txt!(quit, ""),
        QeditMode::Kind => { q.q.kind = line.trim().parse().unwrap_or(q.q.kind); back!() }
        QeditMode::Qm => { q.q.qm = line.trim().parse().unwrap_or(q.q.qm); back!() }
        QeditMode::Target => { q.q.target = line.trim().parse().unwrap_or(q.q.target); back!() }
        QeditMode::Flags => { q.q.flags = line.trim().parse().unwrap_or(q.q.flags); back!() }
        QeditMode::Prev => { q.q.prev_quest = line.trim().parse().unwrap_or(q.q.prev_quest); back!() }
        QeditMode::Next => { q.q.next_quest = line.trim().parse().unwrap_or(q.q.next_quest); back!() }
        QeditMode::Prereq => { q.q.prereq = line.trim().parse().unwrap_or(q.q.prereq); back!() }
        QeditMode::Gold => { q.q.gold_reward = line.trim().parse().unwrap_or(q.q.gold_reward); back!() }
        QeditMode::Exp => { q.q.exp_reward = line.trim().parse().unwrap_or(q.q.exp_reward); back!() }
        QeditMode::ObjReward => { q.q.obj_reward = line.trim().parse().unwrap_or(q.q.obj_reward); back!() }
        QeditMode::ValuesMenu => {
            let t = line.trim();
            if t == "0" || t.is_empty() { return back!(); }
            if let Ok(n) = t.parse::<usize>() { if (1..=7).contains(&n) { q.mode = QeditMode::ValueEdit(n-1); return (format!("Enter value {}:\r\n", n-1), true); } }
            (qvalues_menu(q), true)
        }
        QeditMode::ValueEdit(i) => { q.q.value[i] = line.trim().parse().unwrap_or(q.q.value[i]); q.mode = QeditMode::ValuesMenu; (qvalues_menu(q), true) }
        QeditMode::DeleteConfirm => {
            if line.trim().eq_ignore_ascii_case("y") {
                let dd = players.lock().await.data_dir().to_string(); let zn = q.zone_number;
                let msg = { let mut w = world.lock().await; w.quests.remove(&q.vnum);
                    match save_quests(&w, &dd, zn) { Ok(_) => format!("\r\nQuest {} deleted.\r\n", q.vnum), Err(e) => format!("\r\nQuest {} deleted (disk fail: {e}).\r\n", q.vnum) } };
                (msg, false)
            } else { q.mode = QeditMode::Main; (format!("\r\nCancelled.\r\n{}", qedit_menu(q)), true) }
        }
    }
}

async fn qedit_main_choice(line: &str, q: &mut Qedit, world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>) -> (String, bool) {
    match line.trim().to_ascii_uppercase().as_str() {
        "1" => { q.mode = QeditMode::Name; ("Enter quest name:\r\n".into(), true) }
        "2" => { q.text.clear(); q.mode = QeditMode::Desc; (format!("Enter description:\r\n{TEXT_END_HINT}"), true) }
        "3" => { q.text.clear(); q.mode = QeditMode::Info; (format!("Enter info (accept) text:\r\n{TEXT_END_HINT}"), true) }
        "4" => { q.text.clear(); q.mode = QeditMode::Done; (format!("Enter completion text:\r\n{TEXT_END_HINT}"), true) }
        "5" => { q.text.clear(); q.mode = QeditMode::Quit; (format!("Enter quit text:\r\n{TEXT_END_HINT}"), true) }
        "6" => { q.mode = QeditMode::Kind; ("Enter quest type (AQ_* number):\r\n".into(), true) }
        "7" => { q.mode = QeditMode::Qm; ("Enter quest-master mob vnum (-1 none):\r\n".into(), true) }
        "8" => { q.mode = QeditMode::Target; ("Enter target vnum (mob/obj/room per type):\r\n".into(), true) }
        "9" => { q.mode = QeditMode::Flags; ("Enter flags (decimal bitmask):\r\n".into(), true) }
        "A" => { q.mode = QeditMode::Prev; ("Enter previous quest vnum (-1 none):\r\n".into(), true) }
        "B" => { q.mode = QeditMode::Next; ("Enter next quest vnum (-1 none):\r\n".into(), true) }
        "C" => { q.mode = QeditMode::Prereq; ("Enter prerequisite object vnum (-1 none):\r\n".into(), true) }
        "D" => { q.mode = QeditMode::ValuesMenu; (qvalues_menu(q), true) }
        "E" => { q.mode = QeditMode::Gold; ("Enter gold reward:\r\n".into(), true) }
        "F" => { q.mode = QeditMode::Exp; ("Enter exp reward:\r\n".into(), true) }
        "G" => { q.mode = QeditMode::ObjReward; ("Enter object reward vnum (-1 none):\r\n".into(), true) }
        "X" => { q.mode = QeditMode::DeleteConfirm; ("Delete this quest? (y/n)\r\n".into(), true) }
        "Q" => {
            let dd = players.lock().await.data_dir().to_string(); let zn = q.zone_number;
            let msg = { let mut w = world.lock().await; w.quests.insert(q.vnum, q.q.clone());
                match save_quests(&w, &dd, zn) { Ok(n) => format!("\r\nQuest {} saved ({} in zone {}).\r\n", q.vnum, n, zn), Err(e) => format!("\r\nQuest {} saved to memory (disk fail: {e}).\r\n", q.vnum) } };
            (msg, false)
        }
        _ => (qedit_menu(q), true),
    }
}

fn qedit_menu(q: &Qedit) -> String {
    let x = &q.q;
    let mut s = format!("\r\n-- Quest number : [@c{}@n]   Zone: [@c{}@n]\r\n", q.vnum, q.zone_number);
    s.push_str(&format!("@g1@n) Name      : @y{}@n\r\n", x.name));
    s.push_str(&format!("@g2@n) Desc      : @y{}@n\r\n", first_line(&x.desc)));
    s.push_str(&format!("@g3@n) Info      : @y{}@n\r\n", first_line(&x.info)));
    s.push_str(&format!("@g4@n) Complete  : @y{}@n\r\n", first_line(&x.done)));
    s.push_str(&format!("@g5@n) Quit text : @y{}@n\r\n", first_line(&x.quit)));
    s.push_str(&format!("@g6@n) Type      : @c{}@n\r\n", x.kind));
    s.push_str(&format!("@g7@n) Questmaster: @c{}@n\r\n", x.qm));
    s.push_str(&format!("@g8@n) Target    : @c{}@n\r\n", x.target));
    s.push_str(&format!("@g9@n) Flags     : @c{}@n\r\n", x.flags));
    s.push_str(&format!("@gA@n) Prev quest: @c{}@n   @gB@n) Next quest: @c{}@n   @gC@n) Prereq obj: @c{}@n\r\n", x.prev_quest, x.next_quest, x.prereq));
    s.push_str(&format!("@gD@n) Values    : @c{:?}@n\r\n", x.value));
    s.push_str(&format!("@gE@n) Gold rwd  : @c{}@n   @gF@n) Exp rwd: @c{}@n   @gG@n) Obj rwd: @c{}@n\r\n", x.gold_reward, x.exp_reward, x.obj_reward));
    s.push_str("@gX@n) Delete quest\r\n@gQ@n) Quit (save)\r\nEnter choice : ");
    s
}

fn qvalues_menu(q: &Qedit) -> String {
    let mut s = String::from("\r\n-- Quest values --\r\n");
    for i in 0..7 { s.push_str(&format!("@g{}@n) value[{}] : @c{}@n\r\n", i+1, i, q.q.value[i])); }
    s.push_str("Edit which (0 to quit) : ");
    s
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

pub fn save_quests(w: &World, data_dir: &str, zone_number: ZoneVnum) -> std::io::Result<usize> {
    use std::io::Write;
    let zone = w.zones.get(&zone_number).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such zone"))?;
    let dir = format!("{data_dir}/world/qst"); std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/{zone_number}.qst"); let tmp = format!("{path}.new");
    let mut f = std::fs::File::create(&tmp)?;
    let mut count = 0;
    for (&vnum, q) in w.quests.range(zone.bot..=zone.top) {
        count += 1;
        writeln!(f, "#{vnum}")?;
        writeln!(f, "{}~", q.name)?;
        writeln!(f, "{}~", q.desc.trim_end_matches(['\r','\n']))?;
        writeln!(f, "{}~", q.info.trim_end_matches(['\r','\n']))?;
        writeln!(f, "{}~", q.done.trim_end_matches(['\r','\n']))?;
        writeln!(f, "{}~", q.quit.trim_end_matches(['\r','\n']))?;
        writeln!(f, "{} {} {} {} {} {} {}", q.kind, q.qm, q.flags, q.target, q.prev_quest, q.next_quest, q.prereq)?;
        writeln!(f, "{} {} {} {} {} {} {}", q.value[0], q.value[1], q.value[2], q.value[3], q.value[4], q.value[5], q.value[6])?;
        writeln!(f, "{} {} {}", q.gold_reward, q.exp_reward, q.obj_reward)?;
        writeln!(f, "S")?;
    }
    writeln!(f, "$~")?;
    f.flush()?; drop(f); std::fs::rename(&tmp, &path)?;
    Ok(count)
}

// ===========================================================================
// Trigger editor (trigedit) — port of stock trigedit.c / dg_olc
// ===========================================================================

use crate::world::{Trigger, TriggerVnum};

#[derive(Debug)]
pub struct Tedit { vnum: TriggerVnum, zone_number: ZoneVnum, t: Trigger, mode: TeditMode, text: Vec<String> }

#[derive(Debug, Clone)]
enum TeditMode { Main, Name, Attach, Type, Narg, Arg, Script, DeleteConfirm }

pub async fn start_trigedit(arg: &str, me: &mut Character, world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>) -> CmdOutput {
    let mut toks = arg.split_whitespace();
    let first = toks.next().unwrap_or("");
    if first.eq_ignore_ascii_case("save") {
        let znum = toks.next().and_then(|s| s.parse().ok()).unwrap_or(-1);
        let znum = if znum >= 0 { znum } else { let w = world.lock().await; zone_of_room(&w, me.current_room).map(|z| z.number).unwrap_or(-1) };
        let dd = players.lock().await.data_dir().to_string();
        let w = world.lock().await;
        return match save_triggers(&w, &dd, znum) { Ok(n) => out(format!("\r\nSaved {n} trigger(s) in zone {znum}.\r\n")), Err(e) => out(format!("\r\nSave failed: {e}\r\n")) };
    }
    let vnum: TriggerVnum = match first.parse() { Ok(v) => v, Err(_) => return out("\r\nUsage: trigedit <vnum> | trigedit save [zone]\r\n") };
    let (t, zone_number) = {
        let w = world.lock().await;
        match w.triggers.get(&vnum) {
            Some(t) => (t.clone(), zone_of_vnum(&w, vnum).map(|z| z.number).unwrap_or(0)),
            None => match zone_of_vnum(&w, vnum) {
                Some(z) => { let mut t = Trigger::default(); t.vnum = vnum; t.name = "new trigger".into();
                    t.attach_type = 0; t.trigger_type = 'g'; t.narg = 100;
                    (t, z.number) }
                None => return out(format!("\r\nVnum {vnum} not within any zone.\r\n")),
            }
        }
    };
    let te = Tedit { vnum, zone_number, t, mode: TeditMode::Main, text: Vec::new() };
    let menu = trig_menu(&te);
    me.olc = Some(OlcSession { editor: Editor::Trig(te) });
    out(menu)
}

async fn trigedit_input(line: &str, t: &mut Tedit, world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>) -> (String, bool) {
    let mode = t.mode.clone();
    macro_rules! back { () => {{ t.mode = TeditMode::Main; (trig_menu(t), true) }} }
    match mode {
        TeditMode::Main => match line.trim().to_ascii_uppercase().as_str() {
            "1" => { t.mode = TeditMode::Name; ("Enter trigger name:\r\n".into(), true) }
            "2" => { t.mode = TeditMode::Attach; ("Attach to (0 = mob, 1 = object, 2 = room):\r\n".into(), true) }
            "3" => { t.mode = TeditMode::Type; ("Trigger type letter (e.g. g greet, d speech, q command):\r\n".into(), true) }
            "4" => { t.mode = TeditMode::Narg; ("Numeric arg (e.g. percent chance):\r\n".into(), true) }
            "5" => { t.mode = TeditMode::Arg; ("Argument (keyword/phrase to match):\r\n".into(), true) }
            "6" => { t.text.clear(); t.mode = TeditMode::Script; (format!("Enter the trigger script:\r\n{TEXT_END_HINT}"), true) }
            "X" => { t.mode = TeditMode::DeleteConfirm; ("Delete this trigger? (y/n)\r\n".into(), true) }
            "Q" => {
                let dd = players.lock().await.data_dir().to_string(); let zn = t.zone_number;
                let msg = { let mut w = world.lock().await; w.triggers.insert(t.vnum, t.t.clone());
                    match save_triggers(&w, &dd, zn) { Ok(n) => format!("\r\nTrigger {} saved ({} in zone {}).\r\n", t.vnum, n, zn), Err(e) => format!("\r\nTrigger {} saved to memory (disk fail: {e}).\r\n", t.vnum) } };
                return (msg, false);
            }
            _ => (trig_menu(t), true),
        },
        TeditMode::Name => { t.t.name = strip_ctrl(line, 100); back!() }
        TeditMode::Attach => { let n: i32 = line.trim().parse().unwrap_or(0); if (0..=2).contains(&n) { t.t.attach_type = n; } back!() }
        TeditMode::Type => { if let Some(c) = line.trim().chars().next() { t.t.trigger_type = c; } back!() }
        TeditMode::Narg => { t.t.narg = line.trim().parse().unwrap_or(t.t.narg); back!() }
        TeditMode::Arg => { t.t.arg = strip_ctrl(line, 200); back!() }
        TeditMode::Script => {
            if is_text_end(line) { t.t.commands = t.text.clone(); t.text.clear(); back!() }
            else { t.text.push(line.to_string()); (String::new(), true) }
        }
        TeditMode::DeleteConfirm => {
            if line.trim().eq_ignore_ascii_case("y") {
                let dd = players.lock().await.data_dir().to_string(); let zn = t.zone_number;
                let msg = { let mut w = world.lock().await; w.triggers.remove(&t.vnum);
                    match save_triggers(&w, &dd, zn) { Ok(_) => format!("\r\nTrigger {} deleted.\r\n", t.vnum), Err(e) => format!("\r\nTrigger {} deleted (disk fail: {e}).\r\n", t.vnum) } };
                (msg, false)
            } else { t.mode = TeditMode::Main; (format!("\r\nCancelled.\r\n{}", trig_menu(t)), true) }
        }
    }
}

fn trig_menu(t: &Tedit) -> String {
    let x = &t.t;
    let attach = match x.attach_type { 0 => "Mobile", 1 => "Object", 2 => "Room", _ => "?" };
    let mut s = format!("\r\n-- Trigger number : [@c{}@n]   Zone: [@c{}@n]\r\n", t.vnum, t.zone_number);
    s.push_str(&format!("@g1@n) Name        : @y{}@n\r\n", x.name));
    s.push_str(&format!("@g2@n) Attach to   : @c{}@n\r\n", attach));
    s.push_str(&format!("@g3@n) Trigger type: @c{}@n\r\n", x.trigger_type));
    s.push_str(&format!("@g4@n) Numeric arg : @c{}@n\r\n", x.narg));
    s.push_str(&format!("@g5@n) Argument    : @y{}@n\r\n", x.arg));
    s.push_str(&format!("@g6@n) Script ({} lines):\r\n", x.commands.len()));
    for l in x.commands.iter().take(8) { s.push_str(&format!("    {l}\r\n")); }
    if x.commands.len() > 8 { s.push_str("    ...\r\n"); }
    s.push_str("@gX@n) Delete trigger\r\n@gQ@n) Quit (save)\r\nEnter choice : ");
    s
}

pub fn save_triggers(w: &World, data_dir: &str, zone_number: ZoneVnum) -> std::io::Result<usize> {
    use std::io::Write;
    let zone = w.zones.get(&zone_number).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such zone"))?;
    let dir = format!("{data_dir}/world/trg"); std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/{zone_number}.trg"); let tmp = format!("{path}.new");
    let mut f = std::fs::File::create(&tmp)?;
    let mut count = 0;
    for (&vnum, t) in w.triggers.range(zone.bot..=zone.top) {
        count += 1;
        writeln!(f, "#{vnum}")?;
        writeln!(f, "{}~", t.name)?;
        writeln!(f, "{} {} {}", t.attach_type, t.trigger_type, t.narg)?;
        writeln!(f, "{}~", t.arg)?;
        for l in &t.commands { writeln!(f, "{}", l.trim_end_matches(['\r','\n']))?; }
        writeln!(f, "~")?;
    }
    writeln!(f, "$~")?;
    f.flush()?; drop(f); std::fs::rename(&tmp, &path)?;
    Ok(count)
}

// ===========================================================================
// Shop editor (sedit) — port of stock sedit.c / genshp.c
// ===========================================================================

use crate::world::Shop;

#[derive(Debug)]
pub struct Sedit { vnum: i32, zone_number: ZoneVnum, shop: Shop, mode: SeditMode }

#[derive(Debug, Clone)]
enum SeditMode {
    Main, Keeper, ProfitBuy, ProfitSell,
    SellsMenu, SellsAdd, BuysMenu, BuysAdd, RoomsMenu, RoomsAdd, DeleteConfirm,
}

pub async fn start_sedit(arg: &str, me: &mut Character, world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>) -> CmdOutput {
    let mut toks = arg.split_whitespace();
    let first = toks.next().unwrap_or("");
    if first.eq_ignore_ascii_case("save") {
        let znum = toks.next().and_then(|s| s.parse().ok()).unwrap_or(-1);
        let znum = if znum >= 0 { znum } else { let w = world.lock().await; zone_of_room(&w, me.current_room).map(|z| z.number).unwrap_or(-1) };
        let dd = players.lock().await.data_dir().to_string();
        let w = world.lock().await;
        return match save_shops(&w, &dd, znum) { Ok(n) => out(format!("\r\nSaved {n} shop(s) in zone {znum}.\r\n")), Err(e) => out(format!("\r\nSave failed: {e}\r\n")) };
    }
    let vnum: i32 = match first.parse() { Ok(v) => v, Err(_) => return out("\r\nUsage: sedit <vnum> | sedit save [zone]\r\n") };
    let (shop, zone_number) = {
        let w = world.lock().await;
        match w.shops.iter().find(|s| s.vnum == vnum) {
            Some(s) => (s.clone(), zone_of_vnum(&w, vnum).map(|z| z.number).unwrap_or(0)),
            None => match zone_of_vnum(&w, vnum) {
                Some(z) => (Shop { vnum, keeper_vnum: -1, rooms: vec![], sells: vec![], buys_types: vec![], profit_buy: 1.0, profit_sell: 0.1 }, z.number),
                None => return out(format!("\r\nVnum {vnum} not within any zone.\r\n")),
            }
        }
    };
    let se = Sedit { vnum, zone_number, shop, mode: SeditMode::Main };
    let menu = sedit_menu(&se);
    me.olc = Some(OlcSession { editor: Editor::Shop(se) });
    out(menu)
}

async fn sedit_input(line: &str, s: &mut Sedit, world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>) -> (String, bool) {
    let mode = s.mode.clone();
    macro_rules! back { () => {{ s.mode = SeditMode::Main; (sedit_menu(s), true) }} }
    match mode {
        SeditMode::Main => match line.trim().to_ascii_uppercase().as_str() {
            "1" => { s.mode = SeditMode::Keeper; ("Enter keeper mob vnum (-1 none):\r\n".into(), true) }
            "2" => { s.mode = SeditMode::ProfitBuy; ("Enter buy profit (player pays cost * this, e.g. 1.15):\r\n".into(), true) }
            "3" => { s.mode = SeditMode::ProfitSell; ("Enter sell profit (shop pays cost * this, e.g. 0.15):\r\n".into(), true) }
            "4" => { s.mode = SeditMode::SellsMenu; (list_menu("Items sold", &s.shop.sells), true) }
            "5" => { s.mode = SeditMode::BuysMenu; (list_menu("Item types bought", &s.shop.buys_types), true) }
            "6" => { s.mode = SeditMode::RoomsMenu; (list_menu("Shop rooms", &s.shop.rooms), true) }
            "X" => { s.mode = SeditMode::DeleteConfirm; ("Delete this shop? (y/n)\r\n".into(), true) }
            "Q" => {
                let dd = players.lock().await.data_dir().to_string(); let zn = s.zone_number;
                let msg = { let mut w = world.lock().await;
                    if let Some(existing) = w.shops.iter_mut().find(|x| x.vnum == s.vnum) { *existing = s.shop.clone(); }
                    else { w.shops.push(s.shop.clone()); }
                    match save_shops(&w, &dd, zn) { Ok(n) => format!("\r\nShop {} saved ({} in zone {}).\r\n", s.vnum, n, zn), Err(e) => format!("\r\nShop {} saved to memory (disk fail: {e}).\r\n", s.vnum) } };
                return (msg, false);
            }
            _ => (sedit_menu(s), true),
        },
        SeditMode::Keeper => { s.shop.keeper_vnum = line.trim().parse().unwrap_or(s.shop.keeper_vnum); back!() }
        SeditMode::ProfitBuy => { s.shop.profit_buy = line.trim().parse().unwrap_or(s.shop.profit_buy); back!() }
        SeditMode::ProfitSell => { s.shop.profit_sell = line.trim().parse().unwrap_or(s.shop.profit_sell); back!() }
        SeditMode::SellsMenu => list_choice(line, &mut s.shop.sells, &mut s.mode, SeditMode::SellsAdd, SeditMode::Main, "Items sold"),
        SeditMode::SellsAdd => { if let Ok(v) = line.trim().parse::<ObjVnum>() { s.shop.sells.push(v); } s.mode = SeditMode::SellsMenu; (list_menu("Items sold", &s.shop.sells), true) }
        SeditMode::BuysMenu => list_choice(line, &mut s.shop.buys_types, &mut s.mode, SeditMode::BuysAdd, SeditMode::Main, "Item types bought"),
        SeditMode::BuysAdd => { if let Ok(v) = line.trim().parse::<i32>() { s.shop.buys_types.push(v); } s.mode = SeditMode::BuysMenu; (list_menu("Item types bought", &s.shop.buys_types), true) }
        SeditMode::RoomsMenu => list_choice(line, &mut s.shop.rooms, &mut s.mode, SeditMode::RoomsAdd, SeditMode::Main, "Shop rooms"),
        SeditMode::RoomsAdd => { if let Ok(v) = line.trim().parse::<RoomVnum>() { s.shop.rooms.push(v); } s.mode = SeditMode::RoomsMenu; (list_menu("Shop rooms", &s.shop.rooms), true) }
        SeditMode::DeleteConfirm => {
            if line.trim().eq_ignore_ascii_case("y") {
                let dd = players.lock().await.data_dir().to_string(); let zn = s.zone_number;
                let msg = { let mut w = world.lock().await; w.shops.retain(|x| x.vnum != s.vnum);
                    match save_shops(&w, &dd, zn) { Ok(_) => format!("\r\nShop {} deleted.\r\n", s.vnum), Err(e) => format!("\r\nShop {} deleted (disk fail: {e}).\r\n", s.vnum) } };
                (msg, false)
            } else { s.mode = SeditMode::Main; (format!("\r\nCancelled.\r\n{}", sedit_menu(s)), true) }
        }
    }
}

/// Shared list-editor choice handler for sedit's int lists.
fn list_choice(line: &str, list: &mut Vec<i32>, mode: &mut SeditMode, add: SeditMode, back: SeditMode, label: &str) -> (String, bool) {
    let t = line.trim();
    if t == "0" || t.is_empty() { *mode = back; return ("".to_string(), true); } // caller re-renders main
    if t.eq_ignore_ascii_case("a") { *mode = add; return ("Enter vnum/type to add (-1 cancels):\r\n".to_string(), true); }
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts[0].eq_ignore_ascii_case("d") {
        if let Some(idx) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
            if idx < list.len() { list.remove(idx); }
        }
    }
    (list_menu(label, list), true)
}

fn list_menu(label: &str, list: &[i32]) -> String {
    let mut s = format!("\r\n-- {label} --\r\n");
    if list.is_empty() { s.push_str("(none)\r\n"); }
    else { for (i, v) in list.iter().enumerate() { s.push_str(&format!("@g{:2}@n) {}\r\n", i, v)); } }
    s.push_str("@ga@n) Add   `d <num>` delete, 0 to quit : ");
    s
}

fn sedit_menu(s: &Sedit) -> String {
    let sh = &s.shop;
    let mut out_s = format!("\r\n-- Shop number : [@c{}@n]   Zone: [@c{}@n]\r\n", s.vnum, s.zone_number);
    out_s.push_str(&format!("@g1@n) Keeper mob vnum : @c{}@n\r\n", sh.keeper_vnum));
    out_s.push_str(&format!("@g2@n) Buy profit      : @c{:.2}@n\r\n", sh.profit_buy));
    out_s.push_str(&format!("@g3@n) Sell profit     : @c{:.2}@n\r\n", sh.profit_sell));
    out_s.push_str(&format!("@g4@n) Items sold      : @c{}@n items\r\n", sh.sells.len()));
    out_s.push_str(&format!("@g5@n) Item types bought: @c{}@n types\r\n", sh.buys_types.len()));
    out_s.push_str(&format!("@g6@n) Shop rooms      : @c{}@n rooms\r\n", sh.rooms.len()));
    out_s.push_str("@gX@n) Delete shop\r\n@gQ@n) Quit (save)\r\nEnter choice : ");
    out_s
}

const SHOP_MSGS: [&str; 7] = [
    "%s Sorry, I don't stock that item.~",
    "%s You don't seem to have that.~",
    "%s I don't buy such items.~",
    "%s That is too expensive for me!~",
    "%s You can't afford it!~",
    "%s That'll be %d coins, please.~",
    "%s You'll get %d coins for it!~",
];

pub fn save_shops(w: &World, data_dir: &str, zone_number: ZoneVnum) -> std::io::Result<usize> {
    use std::io::Write;
    let zone = w.zones.get(&zone_number).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such zone"))?;
    let dir = format!("{data_dir}/world/shp"); std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/{zone_number}.shp"); let tmp = format!("{path}.new");
    let mut f = std::fs::File::create(&tmp)?;
    writeln!(f, "CircleMUD v3.0 Shop File~")?;
    let mut count = 0;
    let mut shops: Vec<&Shop> = w.shops.iter().filter(|s| s.vnum >= zone.bot && s.vnum <= zone.top).collect();
    shops.sort_by_key(|s| s.vnum);
    for sh in shops {
        count += 1;
        writeln!(f, "#{}~", sh.vnum)?;
        for v in &sh.sells { writeln!(f, "{v}")?; }
        writeln!(f, "-1")?;
        writeln!(f, "{:.2}", sh.profit_buy)?;
        writeln!(f, "{:.2}", sh.profit_sell)?;
        for t in &sh.buys_types { writeln!(f, "{t}")?; }
        writeln!(f, "-1")?;
        for m in SHOP_MSGS { writeln!(f, "{m}")?; }
        writeln!(f, "0")?;            // temper
        writeln!(f, "0")?;            // bitvector
        writeln!(f, "{}", sh.keeper_vnum)?;
        writeln!(f, "0")?;            // with_who (trade with all)
        for r in &sh.rooms { writeln!(f, "{r}")?; }
        writeln!(f, "-1")?;
        writeln!(f, "0\n28\n0\n28")?; // open1 close1 open2 close2 (always open)
    }
    writeln!(f, "$~")?;
    f.flush()?; drop(f); std::fs::rename(&tmp, &path)?;
    Ok(count)
}

// ===========================================================================
// Social editor (aedit) — port of stock aedit.c
// ===========================================================================

pub async fn start_aedit(arg: &str, me: &mut Character, world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>) -> CmdOutput {
    let mut toks = arg.split_whitespace();
    let first = toks.next().unwrap_or("");
    if first.eq_ignore_ascii_case("save") {
        let dd = players.lock().await.data_dir().to_string();
        let w = world.lock().await;
        return match save_socials(&w, &dd) { Ok(n) => out(format!("\r\nSaved {n} socials.\r\n")), Err(e) => out(format!("\r\nSave failed: {e}\r\n")) };
    }
    if first.is_empty() { return out("\r\nUsage: aedit <social-name> | aedit save\r\n"); }
    let name = first.to_ascii_lowercase();
    let social = {
        let w = world.lock().await;
        w.socials.iter().find(|s| s.name.eq_ignore_ascii_case(&name)).cloned()
            .unwrap_or_else(|| crate::world::Social { name: name.clone(), min_position: 5, target_required: false, ..Default::default() })
    };
    let ae = Aedit { social, mode: AeditMode::Main, text: Vec::new() };
    let menu = aedit_menu(&ae);
    me.olc = Some(OlcSession { editor: Editor::Social(ae) });
    out(menu)
}

#[derive(Debug)]
pub struct Aedit { social: crate::world::Social, mode: AeditMode, text: Vec<String> }

#[derive(Debug, Clone)]
enum AeditMode { Main, MinPos, Target, Slot(usize) }

async fn aedit_input(line: &str, a: &mut Aedit, world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>) -> (String, bool) {
    let mode = a.mode.clone();
    macro_rules! back { () => {{ a.mode = AeditMode::Main; (aedit_menu(a), true) }} }
    match mode {
        AeditMode::Main => match line.trim().to_ascii_uppercase().as_str() {
            "1" => { a.mode = AeditMode::MinPos; (position_menu("Minimum position"), true) }
            "2" => { a.social.target_required = !a.social.target_required; back!() }
            "3" => { a.mode = AeditMode::Slot(0); ("Actor, no target (`#` for none):\r\n".into(), true) }
            "4" => { a.mode = AeditMode::Slot(1); ("Room, no target (`#` for none):\r\n".into(), true) }
            "5" => { a.mode = AeditMode::Slot(2); ("Actor, with target (`#` for none):\r\n".into(), true) }
            "6" => { a.mode = AeditMode::Slot(3); ("Room, with target (`#` for none):\r\n".into(), true) }
            "7" => { a.mode = AeditMode::Slot(4); ("Victim sees (`#` for none):\r\n".into(), true) }
            "Q" => {
                let dd = players.lock().await.data_dir().to_string();
                let msg = { let mut w = world.lock().await;
                    if let Some(e) = w.socials.iter_mut().find(|s| s.name.eq_ignore_ascii_case(&a.social.name)) { *e = a.social.clone(); }
                    else { w.socials.push(a.social.clone()); }
                    match save_socials(&w, &dd) { Ok(n) => format!("\r\nSocial '{}' saved ({} socials written).\r\n", a.social.name, n), Err(e) => format!("\r\nSocial saved to memory (disk fail: {e}).\r\n") } };
                return (msg, false);
            }
            _ => (aedit_menu(a), true),
        },
        AeditMode::MinPos => { if let Ok(n) = line.trim().parse::<i32>() { if n >= 0 && (n as usize) < POSITION_NAMES.len() { a.social.min_position = n; } } back!() }
        AeditMode::Target => back!(),
        AeditMode::Slot(i) => {
            let v = if line.trim() == "#" { String::new() } else { line.trim_end().to_string() };
            match i { 0 => a.social.actor_no_arg = v, 1 => a.social.room_no_arg = v, 2 => a.social.actor_target = v, 3 => a.social.room_target = v, _ => a.social.victim_target = v };
            back!()
        }
    }
}

fn aedit_menu(a: &Aedit) -> String {
    let s = &a.social;
    let show = |x: &str| if x.is_empty() { "<none>".to_string() } else { x.to_string() };
    let mut m = format!("\r\n-- Social : [@c{}@n]\r\n", s.name);
    m.push_str(&format!("@g1@n) Min position    : @c{}@n\r\n", position_name(s.min_position)));
    m.push_str(&format!("@g2@n) Target required : @c{}@n\r\n", s.target_required));
    m.push_str(&format!("@g3@n) Actor, no target : @y{}@n\r\n", show(&s.actor_no_arg)));
    m.push_str(&format!("@g4@n) Room, no target  : @y{}@n\r\n", show(&s.room_no_arg)));
    m.push_str(&format!("@g5@n) Actor, w/ target : @y{}@n\r\n", show(&s.actor_target)));
    m.push_str(&format!("@g6@n) Room, w/ target  : @y{}@n\r\n", show(&s.room_target)));
    m.push_str(&format!("@g7@n) Victim sees      : @y{}@n\r\n", show(&s.victim_target)));
    m.push_str("@gQ@n) Quit (save)\r\nEnter choice : ");
    m
}

/// Rewrite the whole socials.new file from World.socials (sorted by name).
pub fn save_socials(w: &World, data_dir: &str) -> std::io::Result<usize> {
    use std::io::Write;
    let dir = format!("{data_dir}/misc"); std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/socials.new"); let tmp = format!("{path}.new");
    let mut f = std::fs::File::create(&tmp)?;
    let mut socials: Vec<&crate::world::Social> = w.socials.iter().collect();
    socials.sort_by(|a, b| a.name.cmp(&b.name));
    let emit = |f: &mut std::fs::File, x: &str| -> std::io::Result<()> {
        if x.is_empty() { writeln!(f, "#") } else { writeln!(f, "{}", x) }
    };
    for s in &socials {
        let tgt = if s.target_required { 1 } else { 0 };
        // ~name cmd hide min_pos target action
        writeln!(f, "~{} {} 0 {} {} 0", s.name, s.name, s.min_position, tgt)?;
        emit(&mut f, &s.actor_no_arg)?;
        emit(&mut f, &s.room_no_arg)?;
        emit(&mut f, &s.actor_target)?;
        emit(&mut f, &s.room_target)?;
        emit(&mut f, &s.victim_target)?;
        // legacy slots 6-13 (body-part / object) not modelled: emit placeholders
        for _ in 0..8 { writeln!(f, "#")?; }
        writeln!(f)?;
    }
    writeln!(f, "$")?;
    f.flush()?; drop(f); std::fs::rename(&tmp, &path)?;
    Ok(socials.len())
}

// ===========================================================================
// Help editor (hedit) — port of stock hedit.c
// ===========================================================================

use crate::world::HelpEntry;

pub async fn start_hedit(arg: &str, me: &mut Character, world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>) -> CmdOutput {
    let mut toks = arg.splitn(2, ' ');
    let first = toks.next().unwrap_or("");
    if first.eq_ignore_ascii_case("save") {
        let dd = players.lock().await.data_dir().to_string();
        let w = world.lock().await;
        return match save_help(&w, &dd) { Ok(n) => out(format!("\r\nSaved {n} help entries.\r\n")), Err(e) => out(format!("\r\nSave failed: {e}\r\n")) };
    }
    let topic = arg.trim();
    if topic.is_empty() { return out("\r\nUsage: hedit <keyword> | hedit save\r\n"); }
    let key = topic.to_ascii_uppercase();
    let (entry, is_new) = {
        let w = world.lock().await;
        match w.help.iter().find(|h| h.keywords.iter().any(|k| k.eq_ignore_ascii_case(&key) || k.eq_ignore_ascii_case(topic))) {
            Some(h) => (h.clone(), false),
            None => (HelpEntry { keywords: vec![key.clone()], min_level: 0, body: String::new() }, true),
        }
    };
    let he = Hedit { orig_key: key, entry, is_new, mode: HeditMode::Main, text: Vec::new() };
    let menu = hedit_menu(&he);
    me.olc = Some(OlcSession { editor: Editor::Help(he) });
    out(menu)
}

#[derive(Debug)]
pub struct Hedit { orig_key: String, entry: HelpEntry, is_new: bool, mode: HeditMode, text: Vec<String> }

#[derive(Debug, Clone)]
enum HeditMode { Main, Keywords, MinLevel, Body }

async fn hedit_input(line: &str, h: &mut Hedit, world: &Arc<Mutex<World>>, players: &Arc<Mutex<PlayerDb>>) -> (String, bool) {
    let mode = h.mode.clone();
    macro_rules! back { () => {{ h.mode = HeditMode::Main; (hedit_menu(h), true) }} }
    match mode {
        HeditMode::Main => match line.trim().to_ascii_uppercase().as_str() {
            "1" => { h.mode = HeditMode::Keywords; ("Enter keywords (space-separated):\r\n".into(), true) }
            "2" => { h.mode = HeditMode::MinLevel; ("Enter minimum level:\r\n".into(), true) }
            "3" => { h.text.clear(); h.mode = HeditMode::Body; (format!("Enter help text:\r\n{TEXT_END_HINT}"), true) }
            "Q" => {
                let dd = players.lock().await.data_dir().to_string();
                let msg = { let mut w = world.lock().await;
                    if let Some(e) = w.help.iter_mut().find(|x| x.keywords.iter().any(|k| k.eq_ignore_ascii_case(&h.orig_key))) { *e = h.entry.clone(); }
                    else { w.help.push(h.entry.clone()); }
                    match save_help(&w, &dd) { Ok(n) => format!("\r\nHelp entry saved ({} entries written).\r\n", n), Err(e) => format!("\r\nHelp saved to memory (disk fail: {e}).\r\n") } };
                return (msg, false);
            }
            _ => (hedit_menu(h), true),
        },
        HeditMode::Keywords => { h.entry.keywords = line.split_whitespace().map(|s| s.to_uppercase()).collect(); if h.entry.keywords.is_empty() { h.entry.keywords.push(h.orig_key.clone()); } back!() }
        HeditMode::MinLevel => { h.entry.min_level = line.trim().parse().unwrap_or(h.entry.min_level); back!() }
        HeditMode::Body => {
            if is_text_end(line) { h.entry.body = h.text.join("\r\n"); h.text.clear(); back!() }
            else { h.text.push(line.to_string()); (String::new(), true) }
        }
    }
}

fn hedit_menu(h: &Hedit) -> String {
    let e = &h.entry;
    let mut s = format!("\r\n-- Help entry{} --\r\n", if h.is_new { " (NEW)" } else { "" });
    s.push_str(&format!("@g1@n) Keywords  : @y{}@n\r\n", e.keywords.join(" ")));
    s.push_str(&format!("@g2@n) Min level : @c{}@n\r\n", e.min_level));
    s.push_str(&format!("@g3@n) Body:\r\n{}\r\n", e.body.trim_end_matches(['\r','\n'])));
    s.push_str("@gQ@n) Quit (save)\r\nEnter choice : ");
    s
}

/// Rewrite the help database from World.help.
pub fn save_help(w: &World, data_dir: &str) -> std::io::Result<usize> {
    use std::io::Write;
    let dir = format!("{data_dir}/text/help"); std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/help.hlp"); let tmp = format!("{path}.new");
    let mut f = std::fs::File::create(&tmp)?;
    for e in &w.help {
        if e.keywords.is_empty() { continue; }
        writeln!(f, "{}", e.keywords.join(" "))?;
        writeln!(f, "{}", e.body.trim_end_matches(['\r', '\n']))?;
        writeln!(f, "#{}", e.min_level)?;
    }
    writeln!(f, "$~")?;
    f.flush()?; drop(f); std::fs::rename(&tmp, &path)?;
    Ok(w.help.len())
}
