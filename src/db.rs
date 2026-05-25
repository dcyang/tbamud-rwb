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

use rand::Rng;

use crate::{
    players::asciiflag_conv,
    world::{
        Direction, Exit, ExtraDescr, MobInstance, MobProto, ObjInstance, ObjProto,
        ResetCmd, Room, World, Zone,
    },
};

/// Roll `count` dice of `size` sides. Mirrors dice() in utils.c.
/// Returns 0 if either arg is non-positive.
pub fn dice(count: i32, size: i32) -> i32 {
    if count <= 0 || size <= 0 { return 0; }
    let mut total = 0;
    let mut rng = rand::thread_rng();
    for _ in 0..count {
        total += rng.gen_range(1..=size);
    }
    total
}

/// Read the world: walk lib/world/zon/index → load zones, then
/// lib/world/wld/index → load rooms.
pub fn load_world(data_dir: &str) -> Result<World> {
    let mut world = World::default();

    let zon_dir = format!("{data_dir}/world/zon");
    let wld_dir = format!("{data_dir}/world/wld");
    let obj_dir = format!("{data_dir}/world/obj");
    let mob_dir = format!("{data_dir}/world/mob");

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

    // --- Object prototypes ------------------------------------------------
    for fname in read_index(&obj_dir)? {
        let path = PathBuf::from(&obj_dir).join(&fname);
        parse_object_file(&path, &mut world)
            .with_context(|| format!("Parsing object file {}", path.display()))?;
    }
    tracing::info!(count = world.obj_protos.len(), "Loaded object prototypes");

    // --- Mob prototypes ----------------------------------------------------
    for fname in read_index(&mob_dir)? {
        let path = PathBuf::from(&mob_dir).join(&fname);
        parse_mob_file(&path, &mut world)
            .with_context(|| format!("Parsing mob file {}", path.display()))?;
    }
    tracing::info!(count = world.mob_protos.len(), "Loaded mob prototypes");

    // --- Run zone resets ---------------------------------------------------
    // Mirrors the initial reset_zone() pass that boot_db() performs over all
    // zones. Periodic resets are scheduled by spawn_zone_reset_tick().
    let zone_vnums: Vec<i32> = world.zones.keys().copied().collect();
    for zv in zone_vnums {
        reset_zone(&mut world, zv);
    }
    tracing::info!(
        mobs = world.mob_instances.len(),
        objs = world.obj_instances.len(),
        "Initial zone reset complete",
    );

    Ok(world)
}

// ---------------------------------------------------------------------------
// Periodic zone reset tick
// ---------------------------------------------------------------------------

/// Background task that re-runs reset_zone() for every zone on a fixed
/// interval.  reset_zone respects each command's `max_existing` cap, so
/// rooms whose mobs/objects are already populated stay unchanged — only
/// missing entities (e.g. mobs the players killed) are restocked.
///
/// Mirrors zone_update() in db.c, simplified: in tbaMUD each zone has its
/// own `lifespan` (minutes between resets), but a single shared cadence is
/// fine for now.
pub fn spawn_zone_reset_tick(world: std::sync::Arc<tokio::sync::Mutex<World>>) {
    // Two-minute cadence — short enough for testing without spamming logs.
    const RESET_SECONDS: u64 = 120;

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(RESET_SECONDS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        // First tick fires immediately; skip it so we don't double-reset on boot.
        interval.tick().await;

        loop {
            interval.tick().await;
            let mut w = world.lock().await;
            let zone_vnums: Vec<i32> = w.zones.keys().copied().collect();
            let mobs_before = w.mob_instances.len();
            let objs_before = w.obj_instances.len();
            for zv in zone_vnums {
                reset_zone(&mut w, zv);
            }
            let mobs_after = w.mob_instances.len();
            let objs_after = w.obj_instances.len();
            if mobs_after != mobs_before || objs_after != objs_before {
                tracing::info!(
                    mobs_added = mobs_after.saturating_sub(mobs_before),
                    objs_added = objs_after.saturating_sub(objs_before),
                    total_mobs = mobs_after,
                    total_objs = objs_after,
                    "Periodic zone reset",
                );
            }
        }
    });
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

    // Read reset commands until S or $ (or EOF).
    loop {
        s.skip_blanks();
        let line = match s.next_line() {
            Some(l) => l,
            None => break,
        };
        let t = line.trim();
        if t.is_empty() { continue; }
        let first = t.chars().next().unwrap();
        // Comments
        if first == '*' { continue; }
        if first == 'S' || first == '$' { break; }

        // The first token after the letter is `if_flag`. Parse out the rest.
        // Format: "<cmd> <if_flag> <arg1> <arg2> [<arg3>] ..."
        // Optional tab-separated trailing comment is ignored.
        let rest = &t[1..];
        // Strip trailing comment after a tab (zone files use this convention).
        let rest = rest.split('\t').next().unwrap_or(rest);
        let toks: Vec<&str> = rest.split_whitespace().collect();

        // T and V commands have different shapes; we skip them for now.
        if first == 'T' || first == 'V' {
            continue;
        }
        if toks.len() < 3 {
            // Malformed — skip rather than abort the whole boot
            tracing::warn!(zone = zone.number, line = ?t, "skipping malformed reset");
            continue;
        }
        let if_flag: i32 = toks[0].parse().unwrap_or(0);
        let arg1:    i32 = toks[1].parse().unwrap_or(0);
        let arg2:    i32 = toks[2].parse().unwrap_or(0);
        let arg3:    i32 = toks.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);

        zone.commands.push(ResetCmd {
            command: first,
            if_flag, arg1, arg2, arg3,
        });
    }

    world.zones.insert(zone.number, zone);
    Ok(())
}

// ---------------------------------------------------------------------------
// Object file parser
// ---------------------------------------------------------------------------
fn parse_object_file(path: &PathBuf, world: &mut World) -> Result<()> {
    let mut s = Stream::from_file(path)?;

    loop {
        s.skip_blanks();
        let header = match s.next_line() {
            Some(h) => h,
            None => return Ok(()),
        };
        let t = header.trim();
        if t == "$" || t == "$~" { return Ok(()); }
        let vnum: i32 = t.strip_prefix('#')
            .ok_or_else(|| anyhow!("obj header missing '#': {t:?}"))?
            .trim()
            .parse()
            .with_context(|| format!("bad obj vnum: {t:?}"))?;

        let name               = s.read_tilde_string()?;
        let short_description  = s.read_tilde_string()?;
        let description        = s.read_tilde_string()?;
        let action_description = s.read_tilde_string()?;

        let line1 = s.next_line()
            .ok_or_else(|| anyhow!("obj {vnum}: missing first numeric line"))?;
        let toks: Vec<&str> = line1.split_whitespace().collect();

        let mut o = ObjProto {
            vnum,
            name:               name.trim().to_string(),
            short_description:  short_description.trim().to_string(),
            description:        description.trim().to_string(),
            action_description: action_description.trim().to_string(),
            ..Default::default()
        };

        // Modern (128-bit) format has 13 tokens: type + 12 flag tokens.
        if toks.len() >= 13 {
            o.item_type        = toks[0].parse().unwrap_or(0);
            o.extra_flags[0]   = asciiflag_conv(toks[1]);
            o.extra_flags[1]   = asciiflag_conv(toks[2]);
            o.extra_flags[2]   = asciiflag_conv(toks[3]);
            o.extra_flags[3]   = asciiflag_conv(toks[4]);
            o.wear_flags[0]    = asciiflag_conv(toks[5]);
            o.wear_flags[1]    = asciiflag_conv(toks[6]);
            o.wear_flags[2]    = asciiflag_conv(toks[7]);
            o.wear_flags[3]    = asciiflag_conv(toks[8]);
            o.affect_flags[0]  = asciiflag_conv(toks[9]);
            o.affect_flags[1]  = asciiflag_conv(toks[10]);
            o.affect_flags[2]  = asciiflag_conv(toks[11]);
            o.affect_flags[3]  = asciiflag_conv(toks[12]);
        } else if toks.len() >= 4 {
            // Legacy 4-tok form: type extra wear affect
            o.item_type      = toks[0].parse().unwrap_or(0);
            o.extra_flags[0] = asciiflag_conv(toks[1]);
            o.wear_flags[0]  = asciiflag_conv(toks[2]);
            o.affect_flags[0]= asciiflag_conv(toks[3]);
        } else {
            bail!("obj {vnum}: bad first numeric line: {line1:?}");
        }

        let line2 = s.next_line()
            .ok_or_else(|| anyhow!("obj {vnum}: missing values line"))?;
        let toks: Vec<&str> = line2.split_whitespace().collect();
        for i in 0..o.value.len().min(toks.len()) {
            o.value[i] = toks[i].parse().unwrap_or(0);
        }

        let line3 = s.next_line()
            .ok_or_else(|| anyhow!("obj {vnum}: missing third line"))?;
        let toks: Vec<&str> = line3.split_whitespace().collect();
        if !toks.is_empty()  { o.weight = toks[0].parse().unwrap_or(0); }
        if toks.len() > 1    { o.cost   = toks[1].parse().unwrap_or(0); }
        if toks.len() > 2    { o.rent   = toks[2].parse().unwrap_or(0); }
        if toks.len() > 3    { o.level  = toks[3].parse().unwrap_or(0); }
        if toks.len() > 4    { o.timer  = toks[4].parse().unwrap_or(0); }

        // Trailing E (extra desc) / A (affect) / T (trigger) records, until
        // we hit '$' (end of file) or '#' (next obj — push it back).
        loop {
            s.skip_blanks();
            let peeked = s.peek().map(|p| p.trim_start().chars().next()).flatten();
            match peeked {
                Some('E') => {
                    let _ = s.next_line(); // consume 'E' line
                    let kw   = s.read_tilde_string()?;
                    let desc = s.read_tilde_string()?;
                    o.extras.push(ExtraDescr {
                        keyword:     kw.trim().to_string(),
                        description: desc.trim().to_string(),
                    });
                }
                Some('A') => {
                    // 'A' then a line "<location> <modifier>" — we don't use
                    // affects yet but must consume two lines to stay in sync.
                    let _ = s.next_line(); // 'A'
                    let _ = s.next_line(); // values
                }
                Some('T') => {
                    let _ = s.next_line(); // DG trigger — skip
                }
                Some('$') | Some('#') | None => break,
                _ => {
                    let bad = s.peek().map(|p| p.to_string()).unwrap_or_default();
                    bail!("obj {vnum}: unexpected line {bad:?}");
                }
            }
        }

        world.obj_protos.insert(o.vnum, o);
    }
}

// ---------------------------------------------------------------------------
// Mob file parser
// ---------------------------------------------------------------------------
fn parse_mob_file(path: &PathBuf, world: &mut World) -> Result<()> {
    let mut s = Stream::from_file(path)?;

    loop {
        s.skip_blanks();
        let header = match s.next_line() {
            Some(h) => h,
            None => return Ok(()),
        };
        let t = header.trim();
        if t == "$" || t == "$~" { return Ok(()); }
        let vnum: i32 = t.strip_prefix('#')
            .ok_or_else(|| anyhow!("mob header missing '#': {t:?}"))?
            .trim()
            .parse()
            .with_context(|| format!("bad mob vnum: {t:?}"))?;

        let name        = s.read_tilde_string()?;
        let short_descr = s.read_tilde_string()?;
        let long_descr  = s.read_tilde_string()?;
        let description = s.read_tilde_string()?;

        // Flag line: "<f1..f8> <align> <S|E>" (10 tokens — modern 128-bit).
        // Legacy 4-tok form: "<mobflags> <affflags> <align> <S|E>".
        let flagline = s.next_line()
            .ok_or_else(|| anyhow!("mob {vnum}: missing flag line"))?;
        let toks: Vec<&str> = flagline.split_whitespace().collect();

        let mut m = MobProto {
            vnum,
            name:        name.trim().to_string(),
            short_descr: short_descr.trim().to_string(),
            long_descr:  long_descr.trim().to_string(),
            description: description.trim().to_string(),
            ..Default::default()
        };

        let mob_kind: char;
        if toks.len() >= 10 {
            m.mob_flags[0] = asciiflag_conv(toks[0]);
            m.mob_flags[1] = asciiflag_conv(toks[1]);
            m.mob_flags[2] = asciiflag_conv(toks[2]);
            m.mob_flags[3] = asciiflag_conv(toks[3]);
            m.aff_flags[0] = asciiflag_conv(toks[4]);
            m.aff_flags[1] = asciiflag_conv(toks[5]);
            m.aff_flags[2] = asciiflag_conv(toks[6]);
            m.aff_flags[3] = asciiflag_conv(toks[7]);
            m.alignment    = toks[8].parse().unwrap_or(0);
            mob_kind = toks[9].chars().next().unwrap_or('S').to_ascii_uppercase();
        } else if toks.len() >= 4 {
            m.mob_flags[0] = asciiflag_conv(toks[0]);
            m.aff_flags[0] = asciiflag_conv(toks[1]);
            m.alignment    = toks[2].parse().unwrap_or(0);
            mob_kind = toks[3].chars().next().unwrap_or('S').to_ascii_uppercase();
        } else {
            bail!("mob {vnum}: bad flag line: {flagline:?}");
        }

        // Simple-mob stat block (both S and E start with this).
        // Line A: "<level> <thac0> <ac> <hp_d>d<hp_s>+<hp_a> <dam_d>d<dam_s>+<dam_a>"
        let l1 = s.next_line()
            .ok_or_else(|| anyhow!("mob {vnum}: missing stat line 1"))?;
        let (lvl, thac0, ac, hp_d, hp_s, hp_a, dd, ds, da) = parse_mob_stats(&l1)
            .with_context(|| format!("mob {vnum}: parsing stat line 1"))?;
        m.level    = lvl;
        m.hitroll  = 20 - thac0;
        m.ac       = 10 * ac;
        m.hp_dice  = hp_d;
        m.hp_size  = hp_s;
        m.hp_add   = hp_a;
        m.dam_dice = dd;
        m.dam_size = ds;
        m.damroll  = da;
        m.mana     = 10;
        m.mv       = 50;

        let l2 = s.next_line()
            .ok_or_else(|| anyhow!("mob {vnum}: missing gold/exp line"))?;
        let toks: Vec<&str> = l2.split_whitespace().collect();
        if toks.len() >= 2 {
            m.gold = toks[0].parse().unwrap_or(0);
            m.exp  = toks[1].parse().unwrap_or(0);
        }

        let l3 = s.next_line()
            .ok_or_else(|| anyhow!("mob {vnum}: missing pos line"))?;
        let toks: Vec<&str> = l3.split_whitespace().collect();
        if toks.len() >= 3 {
            m.position    = toks[0].parse().unwrap_or(0);
            m.default_pos = toks[1].parse().unwrap_or(0);
            m.sex         = toks[2].parse().unwrap_or(0);
        }

        // Enhanced section: skip ESpec keyword:value lines until lone 'E'.
        if mob_kind == 'E' {
            loop {
                let line = s.next_line()
                    .ok_or_else(|| anyhow!("mob {vnum}: EOF in espec section"))?;
                if line.trim() == "E" { break; }
                if line.starts_with('#') {
                    bail!("mob {vnum}: unterminated espec section");
                }
                // ignore espec keyword:value content for now
            }
        }

        // Trailing DG trigger references (T <vnum>) — skip
        loop {
            s.skip_blanks();
            match s.peek() {
                Some(p) if p.trim_start().starts_with('T') => { let _ = s.next_line(); }
                _ => break,
            }
        }

        world.mob_protos.insert(m.vnum, m);
    }
}

/// Parse a mob simple-stat line "L T A HdH+A DdD+A" into a 9-tuple.
fn parse_mob_stats(line: &str) -> Result<(i32,i32,i32,i32,i32,i32,i32,i32,i32)> {
    // Replace 'd' and '+' with spaces so we can split uniformly.
    let normalized: String = line.chars()
        .map(|c| if c == 'd' || c == '+' { ' ' } else { c })
        .collect();
    let toks: Vec<&str> = normalized.split_whitespace().collect();
    if toks.len() < 9 {
        bail!("expected 9 numeric fields, got {}: {line:?}", toks.len());
    }
    Ok((
        toks[0].parse().unwrap_or(0),
        toks[1].parse().unwrap_or(0),
        toks[2].parse().unwrap_or(0),
        toks[3].parse().unwrap_or(0),
        toks[4].parse().unwrap_or(0),
        toks[5].parse().unwrap_or(0),
        toks[6].parse().unwrap_or(0),
        toks[7].parse().unwrap_or(0),
        toks[8].parse().unwrap_or(0),
    ))
}

// ---------------------------------------------------------------------------
// Zone resets — execute one zone's reset commands
// ---------------------------------------------------------------------------

/// Initial zone reset: spawns mobs and objects according to the zone's
/// reset command list. Mirrors reset_zone() in db.c, restricted to the
/// commands we currently implement (M, O, G, P, R, D). E (equip) is
/// downgraded to G (give) since we don't have equipment slots yet. T and V
/// are dropped at parse time.
fn reset_zone(world: &mut World, zone_vnum: i32) {
    // Clone the command list so we can mutate world.* during execution.
    let cmds = match world.zones.get(&zone_vnum) {
        Some(z) => z.commands.clone(),
        None => return,
    };

    let mut last_cmd_ok = true;
    let mut next_mob_id: u32 = world.mob_instances.last().map(|m| m.id + 1).unwrap_or(1);
    let mut next_obj_id: u32 = world.obj_instances.last().map(|o| o.id + 1).unwrap_or(1);
    // Track the most-recently-loaded mob instance id for 'G' (give to mob).
    let mut last_mob_id: Option<u32> = None;

    for cmd in &cmds {
        if cmd.if_flag != 0 && !last_cmd_ok {
            continue;
        }

        match cmd.command {
            'M' => {
                // arg1=mob_vnum arg2=max arg3=room_vnum
                if !world.mob_protos.contains_key(&cmd.arg1) {
                    last_cmd_ok = false;
                    continue;
                }
                if world.count_mob(cmd.arg1) >= cmd.arg2 {
                    last_cmd_ok = false;
                    continue;
                }
                if let Some(room) = world.rooms.get_mut(&cmd.arg3) {
                    // Roll HP from "<d>d<s>+<a>" dice on each spawn (mirrors
                    // dice() in db.c that initializes mob_proto[i] hit total).
                    let hp = world.mob_protos.get(&cmd.arg1)
                        .map(|p| dice(p.hp_dice, p.hp_size) + p.hp_add)
                        .unwrap_or(10)
                        .max(1);
                    let id = next_mob_id; next_mob_id += 1;
                    room.mobs.push(id);
                    world.mob_instances.push(MobInstance {
                        id, vnum: cmd.arg1, in_room: cmd.arg3,
                        inventory: Vec::new(),
                        hp, max_hp: hp,
                        fighting: None,
                    });
                    last_mob_id = Some(id);
                    last_cmd_ok = true;
                } else {
                    last_cmd_ok = false;
                }
            }
            'O' => {
                // arg1=obj_vnum arg2=max arg3=room_vnum
                if !world.obj_protos.contains_key(&cmd.arg1) {
                    last_cmd_ok = false;
                    continue;
                }
                if world.count_obj(cmd.arg1) >= cmd.arg2 {
                    last_cmd_ok = false;
                    continue;
                }
                if cmd.arg3 == -1 {
                    // Limbo / nowhere — load but don't place.
                    let id = next_obj_id; next_obj_id += 1;
                    world.obj_instances.push(ObjInstance {
                        id, vnum: cmd.arg1, in_room: crate::world::NOWHERE,
                    });
                    last_cmd_ok = true;
                } else if let Some(room) = world.rooms.get_mut(&cmd.arg3) {
                    let id = next_obj_id; next_obj_id += 1;
                    room.objects.push(id);
                    world.obj_instances.push(ObjInstance {
                        id, vnum: cmd.arg1, in_room: cmd.arg3,
                    });
                    last_cmd_ok = true;
                } else {
                    last_cmd_ok = false;
                }
            }
            'G' | 'E' => {
                // G = give-to-mob (inventory); E = equip-to-mob (wear slot).
                // We don't model wear slots yet, so E falls back to G —
                // the item goes into the mob's inventory either way and is
                // not visible on the ground.
                let Some(mob_id) = last_mob_id else {
                    last_cmd_ok = false;
                    continue;
                };
                if !world.obj_protos.contains_key(&cmd.arg1) {
                    last_cmd_ok = false;
                    continue;
                }
                if world.count_obj(cmd.arg1) >= cmd.arg2 {
                    last_cmd_ok = false;
                    continue;
                }
                let id = next_obj_id; next_obj_id += 1;
                world.obj_instances.push(ObjInstance {
                    id, vnum: cmd.arg1, in_room: crate::world::NOWHERE,
                });
                if let Some(m) = world.mob_instances.iter_mut().find(|m| m.id == mob_id) {
                    m.inventory.push(id);
                }
                last_cmd_ok = true;
            }
            'P' => {
                // Put obj-in-obj — we don't model containers yet; just spawn
                // the inner object in limbo so the count_obj cap still works.
                if !world.obj_protos.contains_key(&cmd.arg1) {
                    last_cmd_ok = false;
                    continue;
                }
                if world.count_obj(cmd.arg1) >= cmd.arg2 {
                    last_cmd_ok = false;
                    continue;
                }
                let id = next_obj_id; next_obj_id += 1;
                world.obj_instances.push(ObjInstance {
                    id, vnum: cmd.arg1, in_room: crate::world::NOWHERE,
                });
                last_cmd_ok = true;
            }
            'R' => {
                // Remove obj from room: arg1=room_vnum arg2=obj_vnum
                if let Some(r) = world.rooms.get_mut(&cmd.arg1) {
                    if let Some(pos) = r.objects.iter().position(|&iid| {
                        world.obj_instances.iter()
                            .any(|o| o.id == iid && o.vnum == cmd.arg2)
                    }) {
                        let iid = r.objects.remove(pos);
                        world.obj_instances.retain(|o| o.id != iid);
                    }
                }
                last_cmd_ok = true;
            }
            'D' => {
                // Door state — skipped until door-flag handling lands; counts
                // as success so subsequent if-conditioned commands proceed.
                last_cmd_ok = true;
            }
            _ => { last_cmd_ok = false; }
        }
    }
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
