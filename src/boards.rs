//! Bulletin boards — a port of stock tbaMUD `boards.c`.
//!
//! A board is an object (one of the stock board vnums) sitting in a room.
//! When a player in that room types `look` at the board (or `board`), the
//! message list is shown; `write <header>` posts a message, `read <N>`
//! displays one, and `remove <N>` deletes it (own messages, or immortals).
//!
//! Messages are persisted one board per file under `<data_dir>/etc/`,
//! one message per line as `author \t unix_ts \t header \t escaped-body`
//! (the body escapes newlines as `\n` and backslashes as `\\`).

use std::io::Write;

/// A single board entry.
#[derive(Debug, Clone)]
pub struct BoardMessage {
    pub author: String,
    pub ts:     i64,
    pub header: String,
    pub body:   String,
}

/// Static board descriptor: object vnum + the level gates (read/write/
/// remove) mirroring stock `board_info[]`.  Filename is derived from the
/// vnum.  LVL_IMMORT here is 34 (matches the rest of the codebase).
pub struct BoardDef {
    pub vnum:        i32,
    pub read_lvl:    i32,
    pub write_lvl:   i32,
    pub remove_lvl:  i32,
    pub file:        &'static str,
}

pub const BOARDS: &[BoardDef] = &[
    BoardDef { vnum: 3099, read_lvl: 0,  write_lvl: 0,  remove_lvl: 34, file: "board.mortal" },
    BoardDef { vnum: 3098, read_lvl: 34, write_lvl: 34, remove_lvl: 34, file: "board.immortal" },
    BoardDef { vnum: 3097, read_lvl: 34, write_lvl: 34, remove_lvl: 34, file: "board.freeze" },
    BoardDef { vnum: 3096, read_lvl: 0,  write_lvl: 0,  remove_lvl: 34, file: "board.social" },
    BoardDef { vnum: 1226, read_lvl: 0,  write_lvl: 0,  remove_lvl: 34, file: "board.builder" },
    BoardDef { vnum: 1227, read_lvl: 0,  write_lvl: 0,  remove_lvl: 34, file: "board.staff" },
    BoardDef { vnum: 1228, read_lvl: 0,  write_lvl: 0,  remove_lvl: 34, file: "board.advertising" },
];

/// Look up a board descriptor by object vnum.
pub fn board_for_vnum(vnum: i32) -> Option<&'static BoardDef> {
    BOARDS.iter().find(|b| b.vnum == vnum)
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', " ")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n')  => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => { out.push('\\'); out.push(other); }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn board_path(data_dir: &str, def: &BoardDef) -> String {
    format!("{data_dir}/etc/{}", def.file)
}

/// Load every message stored for a board (oldest first).
pub fn load_board(data_dir: &str, def: &BoardDef) -> Vec<BoardMessage> {
    let path = board_path(data_dir, def);
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() { continue; }
        let mut parts = line.splitn(4, '\t');
        let author = parts.next().unwrap_or("").to_string();
        let ts     = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let header = parts.next().unwrap_or("").to_string();
        let body   = unescape(parts.next().unwrap_or(""));
        out.push(BoardMessage { author, ts, header, body });
    }
    out
}

/// Persist the full message list for a board (overwrites the file).
pub fn save_board(data_dir: &str, def: &BoardDef, msgs: &[BoardMessage]) -> std::io::Result<()> {
    let dir = format!("{data_dir}/etc");
    std::fs::create_dir_all(&dir)?;
    let path = board_path(data_dir, def);
    let mut f = std::fs::File::create(&path)?;
    for m in msgs {
        writeln!(f, "{}\t{}\t{}\t{}",
            escape(&m.author), m.ts, escape(&m.header), escape(&m.body))?;
    }
    Ok(())
}

/// Append a single message to a board's file.
pub fn append_message(data_dir: &str, def: &BoardDef, msg: &BoardMessage) -> std::io::Result<()> {
    let mut msgs = load_board(data_dir, def);
    msgs.push(msg.clone());
    save_board(data_dir, def, &msgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_roundtrip() {
        let s = "line one\nline two\\with backslash";
        assert_eq!(unescape(&escape(s)), s.replace('\t', " "));
    }
}
