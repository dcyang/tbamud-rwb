/// World loader — reads zone and room (.zon, .wld) files from `lib/world/`.
///
/// Mirrors index_boot()/parse_room()/load_zones() in db.c, restricted to the
/// minimum needed for a player to walk around. Mobile / object / shop /
/// trigger / quest parsing is deferred to later checkpoints.

use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
};

use anyhow::{anyhow, bail, Context, Result};

use crate::{
    players::asciiflag_conv,
    world::{Direction, Exit, ExtraDescr, Room, World, Zone},
};

/// Read the world: walk lib/world/zon/index → load zones, then
/// lib/world/wld/index → load rooms.
pub fn load_world(data_dir: &str) -> Result<World> {
    let mut world = World::default();

    let zon_dir = format!("{data_dir}/world/zon");
    let wld_dir = format!("{data_dir}/world/wld");

    // --- Zones -------------------------------------------------------------
    for fname in read_index(&zon_dir)? {
        let path = PathBuf::from(&zon_dir).join(&fname);
        parse_zone_file(&path, &mut world)
            .with_context(|| format!("Parsing zone file {}", path.display()))?;
    }
    tracing::info!(count = world.zones.len(), "Loaded zones");

    // --- Rooms -------------------------------------------------------------
    for fname in read_index(&wld_dir)? {
        let path = PathBuf::from(&wld_dir).join(&fname);
        parse_room_file(&path, &mut world)
            .with_context(|| format!("Parsing room file {}", path.display()))?;
    }
    tracing::info!(count = world.rooms.len(), "Loaded rooms");

    Ok(world)
}

// ---------------------------------------------------------------------------
// Index file reader: returns Vec<String> of filenames listed in `<dir>/index`
// (terminated by a line containing only "$"). Mirrors index_boot() in db.c.
// ---------------------------------------------------------------------------
fn read_index(dir: &str) -> Result<Vec<String>> {
    let path = format!("{dir}/index");
    let f = File::open(&path).with_context(|| format!("opening {path}"))?;
    let mut names = Vec::new();
    for line in BufReader::new(f).lines() {
        let l = line?;
        let t = l.trim();
        if t.is_empty() || t == "$" {
            if t == "$" { break; }
            continue;
        }
        names.push(t.to_string());
    }
    Ok(names)
}

// ---------------------------------------------------------------------------
// Tokenized stream over a flat file: yields trimmed lines, knows how to
// read a `~`-terminated string and a single non-blank "command letter" line.
// ---------------------------------------------------------------------------
struct Stream {
    lines: Vec<String>,
    pos:   usize,
}

impl Stream {
    fn from_file(path: &PathBuf) -> Result<Self> {
        let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut lines = Vec::new();
        for l in BufReader::new(f).lines() {
            lines.push(l?);
        }
        Ok(Self { lines, pos: 0 })
    }

    /// Peek the next non-empty line without consuming it.
    fn peek(&self) -> Option<&str> {
        self.lines.get(self.pos).map(|s| s.as_str())
    }

    /// Consume and return the next line (raw, untrimmed).
    fn next_line(&mut self) -> Option<String> {
        let l = self.lines.get(self.pos).cloned()?;
        self.pos += 1;
        Some(l)
    }

    /// Skip blank lines until we reach a non-blank line, or EOF.
    fn skip_blanks(&mut self) {
        while let Some(l) = self.lines.get(self.pos) {
            if l.trim().is_empty() { self.pos += 1; } else { break; }
        }
    }

    /// Read a `~`-terminated string. CircleMUD fread_string format:
    ///   line1\n
    ///   line2~\n
    /// The `~` may be at the end of a longer line or alone on a line.
    fn read_tilde_string(&mut self) -> Result<String> {
        let mut buf = String::new();
        let mut first = true;
        loop {
            let line = self.next_line()
                .ok_or_else(|| anyhow!("EOF while reading ~-string"))?;
            // Locate ~; everything before it is text, anything after is junk
            if let Some(idx) = line.find('~') {
                if !first { buf.push('\n'); }
                buf.push_str(&line[..idx]);
                return Ok(buf);
            }
            if !first { buf.push('\n'); }
            buf.push_str(&line);
            first = false;
        }
    }
}

// ---------------------------------------------------------------------------
// Zone file parser
// ---------------------------------------------------------------------------
fn parse_zone_file(path: &PathBuf, world: &mut World) -> Result<()> {
    let mut s = Stream::from_file(path)?;
    s.skip_blanks();

    // Header: #<vnum>
    let header = s.next_line()
        .ok_or_else(|| anyhow!("empty zone file"))?;
    let header_trim = header.trim();
    let vnum: i32 = header_trim.strip_prefix('#')
        .ok_or_else(|| anyhow!("zone header missing '#': {header_trim}"))?
        .trim()
        .parse()
        .with_context(|| format!("bad zone vnum: {header_trim}"))?;

    // builders~ (may span multiple lines but typically one)
    let builders = s.read_tilde_string()?;
    // name~
    let name = s.read_tilde_string()?;

    // Numeric line: try 10 fields (new format) then 4 (legacy)
    let numline = s.next_line()
        .ok_or_else(|| anyhow!("missing zone numeric line"))?;
    let toks: Vec<&str> = numline.split_whitespace().collect();

    let mut zone = Zone {
        number: vnum,
        builders: builders.trim().to_string(),
        name: name.trim().to_string(),
        min_level: -1,
        max_level: -1,
        ..Default::default()
    };

    if toks.len() >= 10 {
        zone.bot        = toks[0].parse().unwrap_or(0);
        zone.top        = toks[1].parse().unwrap_or(0);
        zone.lifespan   = toks[2].parse().unwrap_or(0);
        zone.reset_mode = toks[3].parse().unwrap_or(0);
        zone.zone_flags[0] = asciiflag_conv(toks[4]);
        zone.zone_flags[1] = asciiflag_conv(toks[5]);
        zone.zone_flags[2] = asciiflag_conv(toks[6]);
        zone.zone_flags[3] = asciiflag_conv(toks[7]);
        zone.min_level  = toks[8].parse().unwrap_or(-1);
        zone.max_level  = toks[9].parse().unwrap_or(-1);
    } else if toks.len() >= 4 {
        zone.bot        = toks[0].parse().unwrap_or(0);
        zone.top        = toks[1].parse().unwrap_or(0);
        zone.lifespan   = toks[2].parse().unwrap_or(0);
        zone.reset_mode = toks[3].parse().unwrap_or(0);
    } else {
        bail!("zone {vnum}: numeric line has too few fields: {numline:?}");
    }

    // We ignore reset commands for this checkpoint, just read until S or $.
    world.zones.insert(zone.number, zone);
    Ok(())
}

// ---------------------------------------------------------------------------
// Room file parser
// ---------------------------------------------------------------------------
fn parse_room_file(path: &PathBuf, world: &mut World) -> Result<()> {
    let mut s = Stream::from_file(path)?;

    loop {
        s.skip_blanks();
        // Expect either #<vnum> or $ (end of file marker)
        let header = match s.next_line() {
            Some(h) => h,
            None => return Ok(()),
        };
        let header_trim = header.trim();
        if header_trim == "$" || header_trim == "$~" {
            return Ok(());
        }
        let vnum: i32 = header_trim.strip_prefix('#')
            .ok_or_else(|| anyhow!("room header missing '#': {header_trim:?}"))?
            .trim()
            .parse()
            .with_context(|| format!("bad room vnum: {header_trim:?}"))?;

        let name = s.read_tilde_string()?;
        let description = s.read_tilde_string()?;

        // Flag/sector line: "<zone> <flags> <sector> <flags2> <flags3> <flag_count>"
        // or legacy 3-field: "<zone> <flags> <sector>"
        let flagline = s.next_line()
            .ok_or_else(|| anyhow!("room {vnum}: missing flag/sector line"))?;
        let toks: Vec<&str> = flagline.split_whitespace().collect();

        let mut room = Room {
            vnum,
            name: name.trim().to_string(),
            description: description.trim().to_string(),
            ..Default::default()
        };

        if toks.len() >= 6 {
            room.zone = toks[0].parse().unwrap_or(0);
            room.room_flags[0] = asciiflag_conv(toks[1]);
            room.sector_type   = toks[2].parse().unwrap_or(0);
            room.room_flags[1] = asciiflag_conv(toks[3]);
            room.room_flags[2] = asciiflag_conv(toks[4]);
            room.room_flags[3] = asciiflag_conv(toks[5]);
        } else if toks.len() >= 3 {
            room.zone = toks[0].parse().unwrap_or(0);
            room.room_flags[0] = asciiflag_conv(toks[1]);
            room.sector_type   = toks[2].parse().unwrap_or(0);
        } else {
            bail!("room {vnum}: bad flag line: {flagline:?}");
        }

        // Now read directions, extras, triggers, until 'S'
        loop {
            s.skip_blanks();
            let line = s.next_line()
                .ok_or_else(|| anyhow!("room {vnum}: unexpected EOF before S"))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let c = trimmed.chars().next().unwrap();
            match c {
                'D' => {
                    // D<n>
                    let dir_n: u8 = trimmed[1..].trim().parse()
                        .with_context(|| format!("room {vnum}: bad direction: {trimmed:?}"))?;
                    let dir = Direction::from_index(dir_n)
                        .ok_or_else(|| anyhow!("room {vnum}: bad dir index {dir_n}"))?;

                    let desc = s.read_tilde_string()?;
                    let kw   = s.read_tilde_string()?;
                    let info = s.next_line()
                        .ok_or_else(|| anyhow!("room {vnum} dir {dir_n}: missing info"))?;
                    let toks: Vec<&str> = info.split_whitespace().collect();
                    if toks.len() < 3 {
                        bail!("room {vnum} dir {dir_n}: bad info: {info:?}");
                    }
                    let door_type: i32 = toks[0].parse().unwrap_or(0);
                    let key:       i32 = toks[1].parse().unwrap_or(-1);
                    let to_vnum:   i32 = toks[2].parse().unwrap_or(-1);

                    // Mirror setup_dir() in db.c
                    const EX_ISDOOR:    u32 = 1 << 0;
                    const EX_PICKPROOF: u32 = 1 << 1;
                    const EX_HIDDEN:    u32 = 1 << 4;
                    let exit_info = match door_type {
                        1 => EX_ISDOOR,
                        2 => EX_ISDOOR | EX_PICKPROOF,
                        3 => EX_ISDOOR | EX_HIDDEN,
                        4 => EX_ISDOOR | EX_PICKPROOF | EX_HIDDEN,
                        _ => 0,
                    };
                    let to_room = if to_vnum == -1 || to_vnum == 0 || to_vnum == 65535 {
                        crate::world::NOWHERE
                    } else {
                        to_vnum
                    };

                    room.exits[dir as usize] = Some(Exit {
                        description: desc.trim().to_string(),
                        keyword:     kw.trim().to_string(),
                        exit_info,
                        key:         if key == 65535 { -1 } else { key },
                        to_room,
                    });
                }
                'E' => {
                    let kw   = s.read_tilde_string()?;
                    let desc = s.read_tilde_string()?;
                    room.extras.push(ExtraDescr {
                        keyword:     kw.trim().to_string(),
                        description: desc.trim().to_string(),
                    });
                }
                'T' => {
                    // Skip DG trigger references for now
                    // T <vnum> already consumed via next_line
                    continue;
                }
                'S' => {
                    // DG triggers may appear AFTER 'S' (before the next room
                    // header) — mirrors the loop in parse_room()/db.c that
                    // reads while(letter=='T'). We just consume them.
                    loop {
                        s.skip_blanks();
                        match s.peek() {
                            Some(p) if p.trim_start().starts_with('T') => {
                                let _ = s.next_line();
                            }
                            _ => break,
                        }
                    }
                    world.rooms.insert(room.vnum, room);
                    break;
                }
                _ => {
                    bail!("room {vnum}: unexpected directive {trimmed:?}");
                }
            }
        }
    }
}
