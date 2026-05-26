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
        Quest, ResetCmd, Room, Shop, Trigger, World, Zone,
    },
};

// ---------------------------------------------------------------------------
// Shop file parser
// ---------------------------------------------------------------------------

/// Parse a .shp file. The format (per CircleMUD v3.0) is a sequence of
/// shop records separated by `#<vnum>~` headers and terminated by `$~`.
/// Each record has:
///     <obj_vnum>       (list, terminated by -1)        — items sold
///     <profit_buy>     (float, e.g. 1.15)
///     <profit_sell>    (float, e.g. 0.15)
///     <obj_type>       (list, terminated by -1)        — types bought
///     7×~-strings       (messages, ignored here)
///     <temper>          (int, ignored)
///     <bitvector>       (int, ignored)
///     <keeper_vnum>     (mob vnum)
///     <with_who>        (int, ignored)
///     <room_vnum>      (list, terminated by -1)        — shop rooms
///     <open1> <close1> <open2> <close2>                (ignored)
fn parse_shop_file(path: &PathBuf, world: &mut World) -> Result<()> {
    let mut s = Stream::from_file(path)?;
    // First line is the "CircleMUD v3.0 Shop File~" header — skip it.
    s.skip_blanks();
    let first = match s.peek() {
        Some(l) => l.trim().to_string(),
        None => return Ok(()),
    };
    if first.contains('~') && !first.starts_with('#') {
        let _ = s.next_line();
        s.skip_blanks();
    }

    loop {
        s.skip_blanks();
        let header = match s.next_line() {
            Some(h) => h.trim().to_string(),
            None => return Ok(()),
        };
        if header == "$" || header == "$~" { return Ok(()); }
        let vnum: i32 = header.trim_start_matches('#').trim_end_matches('~').trim()
            .parse().with_context(|| format!("bad shop header: {header:?}"))?;

        // Items sold (vnums, terminated by -1)
        let sells = read_int_list(&mut s)?;

        // profit_buy, profit_sell
        let profit_buy:  f32 = s.next_line()
            .ok_or_else(|| anyhow!("shop {vnum}: missing profit_buy"))?
            .trim().parse().unwrap_or(1.0);
        let profit_sell: f32 = s.next_line()
            .ok_or_else(|| anyhow!("shop {vnum}: missing profit_sell"))?
            .trim().parse().unwrap_or(1.0);

        // Item types bought (terminated by -1)
        let buys_types = read_int_list(&mut s)?;

        // 7 ~-terminated messages — read and discard.
        for _ in 0..7 {
            let _ = s.read_tilde_string()?;
        }

        // temper, bitvector, keeper, with_who
        let _temper:    i32 = s.next_line().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
        let _bitvector: i32 = s.next_line().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
        let keeper:     i32 = s.next_line().and_then(|l| l.trim().parse().ok()).unwrap_or(-1);
        let _with_who:  i32 = s.next_line().and_then(|l| l.trim().parse().ok()).unwrap_or(0);

        // Rooms (terminated by -1)
        let rooms = read_int_list(&mut s)?;

        // open1, close1, open2, close2 — read and discard.
        for _ in 0..4 {
            let _ = s.next_line();
        }

        world.shops.push(Shop {
            vnum,
            keeper_vnum: keeper,
            rooms,
            sells,
            buys_types,
            profit_buy,
            profit_sell,
        });
    }
}

// ---------------------------------------------------------------------------
// Trigger file parser
// ---------------------------------------------------------------------------

/// Parse a `.trg` file containing DG trigger scripts.
/// Format (per record):
///   #<vnum>
///   <name>~
///   <attach_type> <type_letter> <narg>
///   <arg>~                        (may be just `~`)
///   <commands...>
///   ~
fn parse_trigger_file(path: &PathBuf, world: &mut World) -> Result<()> {
    let mut s = Stream::from_file(path)?;
    loop {
        s.skip_blanks();
        let header = match s.next_line() {
            Some(h) => h.trim().to_string(),
            None    => return Ok(()),
        };
        if header == "$" || header == "$~" { return Ok(()); }
        let vnum: i32 = header.trim_start_matches('#').trim()
            .parse().with_context(|| format!("bad trigger header: {header:?}"))?;

        let name = s.read_tilde_string()?;

        // Numeric/type line: "<attach_type> <type_letter> <narg>"
        let line = s.next_line()
            .ok_or_else(|| anyhow!("trigger {vnum}: missing type line"))?;
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() < 3 {
            // Drain commands until ~
            while let Some(l) = s.next_line() {
                if l.trim() == "~" { break; }
            }
            tracing::warn!(vnum, "trigger has malformed type line, skipping");
            continue;
        }
        let attach_type: i32 = toks[0].parse().unwrap_or(-1);
        let trigger_type: char = toks[1].chars().next().unwrap_or('?');
        let narg: i32 = toks[2].parse().unwrap_or(0);

        let arg = s.read_tilde_string()?;

        // Commands until a lone `~` line.
        let mut commands = Vec::new();
        while let Some(l) = s.next_line() {
            if l.trim() == "~" { break; }
            commands.push(l);
        }

        world.triggers.insert(vnum, Trigger {
            vnum,
            name:        name.trim().to_string(),
            attach_type,
            trigger_type,
            narg,
            arg:         arg.trim().to_string(),
            commands,
        });
    }
}

// ---------------------------------------------------------------------------
// Quest file parser
// ---------------------------------------------------------------------------

/// Parse a single .qst file.  Mirrors parse_quest() in quest.c.  Format:
///   #<vnum>
///   <name>~  <desc>~  <info>~  <done>~  <quit>~
///   <type> <qm_vnum> <flags> <target> <prev> <next> <prereq>
///   <value0..6>     (7 ints)
///   <gold> <exp> <obj_reward>
///   S
fn parse_quest_file(path: &PathBuf, world: &mut World) -> Result<()> {
    let mut s = Stream::from_file(path)?;
    loop {
        s.skip_blanks();
        let header = match s.next_line() {
            Some(h) => h.trim().to_string(),
            None    => return Ok(()),
        };
        if header == "$" || header == "$~" { return Ok(()); }
        let vnum: i32 = header.trim_start_matches('#').trim()
            .parse().with_context(|| format!("bad quest header: {header:?}"))?;

        let name = s.read_tilde_string()?;
        let desc = s.read_tilde_string()?;
        let info = s.read_tilde_string()?;
        let done = s.read_tilde_string()?;
        let quit = s.read_tilde_string()?;

        // Line 1: type qm flags target prev next prereq
        let l1 = s.next_line().ok_or_else(|| anyhow!("quest {vnum}: missing line 1"))?;
        let toks: Vec<&str> = l1.split_whitespace().collect();
        if toks.len() < 7 {
            tracing::warn!(vnum, "quest line 1 has < 7 fields, skipping");
            // Drain until S to keep stream aligned.
            while let Some(line) = s.next_line() {
                if line.trim() == "S" { break; }
            }
            continue;
        }
        let kind:       i32 = toks[0].parse().unwrap_or(-1);
        let qm:         i32 = toks[1].parse().unwrap_or(-1);
        let flags:      u32 = asciiflag_conv(toks[2]);
        let target:     i32 = toks[3].parse().unwrap_or(-1);
        let prev_quest: i32 = toks[4].parse().unwrap_or(-1);
        let next_quest: i32 = toks[5].parse().unwrap_or(-1);
        let prereq:     i32 = toks[6].parse().unwrap_or(-1);

        // Line 2: 7 value ints
        let l2 = s.next_line().ok_or_else(|| anyhow!("quest {vnum}: missing line 2"))?;
        let toks: Vec<&str> = l2.split_whitespace().collect();
        let mut value = [0i32; 7];
        for i in 0..7 {
            value[i] = toks.get(i).and_then(|t| t.parse().ok()).unwrap_or(0);
        }

        // Line 3: rewards
        let l3 = s.next_line().ok_or_else(|| anyhow!("quest {vnum}: missing line 3"))?;
        let toks: Vec<&str> = l3.split_whitespace().collect();
        let gold_reward: i32 = toks.get(0).and_then(|t| t.parse().ok()).unwrap_or(0);
        let exp_reward:  i32 = toks.get(1).and_then(|t| t.parse().ok()).unwrap_or(0);
        let obj_reward:  i32 = toks.get(2).and_then(|t| t.parse().ok()).unwrap_or(-1);

        // Read until S.
        while let Some(line) = s.next_line() {
            if line.trim() == "S" { break; }
        }

        world.quests.insert(vnum, Quest {
            vnum,
            name: name.trim().to_string(),
            desc: desc.trim().to_string(),
            info: info.trim().to_string(),
            done: done.trim().to_string(),
            quit: quit.trim().to_string(),
            kind, flags, qm, target,
            prev_quest, next_quest, prereq,
            value,
            gold_reward, exp_reward, obj_reward,
        });
    }
}

/// Read a list of integers terminated by -1.  Tolerant of formatting
/// quirks: a value like "5lifecutter" (no separator before the keyword
/// annotation) is read as 5.
fn read_int_list(s: &mut Stream) -> Result<Vec<i32>> {
    let mut out = Vec::new();
    loop {
        let line = s.next_line()
            .ok_or_else(|| anyhow!("EOF reading int list"))?;
        let t = line.trim();
        if t.is_empty() { continue; }
        let tok = t.split_ascii_whitespace().next().unwrap_or("");
        // Extract the leading numeric prefix (allow `-`).
        let mut end = 0;
        for (i, c) in tok.char_indices() {
            if (i == 0 && c == '-') || c.is_ascii_digit() {
                end = i + c.len_utf8();
            } else {
                break;
            }
        }
        if end == 0 {
            bail!("expected integer in list, got {tok:?}");
        }
        let n: i32 = tok[..end].parse()
            .with_context(|| format!("bad integer in list: {tok:?}"))?;
        if n == -1 { return Ok(out); }
        out.push(n);
    }
}

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

    // --- Triggers ----------------------------------------------------------
    let trg_dir = format!("{data_dir}/world/trg");
    if std::path::Path::new(&format!("{trg_dir}/index")).exists() {
        for fname in read_index(&trg_dir)? {
            let path = PathBuf::from(&trg_dir).join(&fname);
            if let Err(e) = parse_trigger_file(&path, &mut world) {
                tracing::warn!(path = %path.display(), error = %e, "Trigger parse error, skipping");
            }
        }
        tracing::info!(count = world.triggers.len(), "Loaded triggers");
    }

    // --- Quests ------------------------------------------------------------
    let qst_dir = format!("{data_dir}/world/qst");
    if std::path::Path::new(&format!("{qst_dir}/index")).exists() {
        for fname in read_index(&qst_dir)? {
            let path = PathBuf::from(&qst_dir).join(&fname);
            if let Err(e) = parse_quest_file(&path, &mut world) {
                tracing::warn!(path = %path.display(), error = %e, "Quest parse error, skipping");
            }
        }
        tracing::info!(count = world.quests.len(), "Loaded quests");
    }

    // --- Shops -------------------------------------------------------------
    let shp_dir = format!("{data_dir}/world/shp");
    if std::path::Path::new(&format!("{shp_dir}/index")).exists() {
        for fname in read_index(&shp_dir)? {
            let path = PathBuf::from(&shp_dir).join(&fname);
            if let Err(e) = parse_shop_file(&path, &mut world) {
                tracing::warn!(path = %path.display(), error = %e, "Shop parse error, skipping");
            }
        }
        tracing::info!(count = world.shops.len(), "Loaded shops");
    }

    // --- Socials database --------------------------------------------------
    let socials_path = format!("{data_dir}/misc/socials.new");
    if std::path::Path::new(&socials_path).exists() {
        match parse_socials_new(&socials_path) {
            Ok(socials) => {
                world.socials = socials;
                tracing::info!(count = world.socials.len(), "Loaded socials");
            }
            Err(e) => tracing::warn!(error = %e, "socials.new parse error, skipping"),
        }
    }

    // --- Help database ------------------------------------------------------
    let help_path = format!("{data_dir}/text/help/help.hlp");
    if std::path::Path::new(&help_path).exists() {
        match parse_help_file(&help_path) {
            Ok(entries) => {
                world.help = entries;
                tracing::info!(count = world.help.len(), "Loaded help entries");
            }
            Err(e) => {
                tracing::warn!(error = %e, "help.hlp parse error, skipping");
            }
        }
    }

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

/// Parse `lib/misc/socials.new` (AEdit format).
///
/// Each social begins with a tilde header:
///   `~<name> <cmd> <hide> <min_position> <target_required> <action>`
///
/// followed by up to 12 message lines (we only read the first 5,
/// which align with our runtime Social slots).  `#` placeholders mean
/// "say nothing for this slot".  Records are separated by blank lines.
fn parse_socials_new(path: &str) -> Result<Vec<crate::world::Social>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading socials file {path}"))?;
    let mut out = Vec::new();
    let mut lines = contents.lines().peekable();

    while let Some(raw) = lines.next() {
        let line = raw.trim();
        if !line.starts_with('~') { continue; }
        // Strip the leading '~' and split header tokens.
        let header = &line[1..];
        let toks: Vec<&str> = header.split_whitespace().collect();
        if toks.is_empty() { continue; }
        let name           = toks[0].to_string();
        let min_position   = toks.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
        let target_required: i32 = toks.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);

        let mut slots: [String; 5] = Default::default();
        let mut slot_i = 0usize;
        // Read until we've captured 5 slots OR the next `~` appears OR EOF.
        // Trailing `#`-only lines beyond 5 are skipped; we don't honor
        // the 12-slot full format yet.
        while slot_i < 5 {
            let Some(&peek) = lines.peek() else { break; };
            if peek.trim_start().starts_with('~') { break; }
            let l = lines.next().unwrap();
            let t = l.trim_end();
            if t.is_empty() { continue; }
            if t.trim_start() == "#" {
                slot_i += 1;                // empty slot
                continue;
            }
            slots[slot_i] = t.to_string();
            slot_i += 1;
        }
        // Drain any remaining slot lines (#s) until we hit the next ~
        // or blank, so we don't accidentally include them in the next
        // entry's slot 0.
        while let Some(&peek) = lines.peek() {
            let pt = peek.trim_start();
            if pt.starts_with('~') || pt.is_empty() { break; }
            // Only consume `#` placeholders; real text would belong to
            // this entry's trailing slot or a malformed file.
            if pt == "#" { lines.next(); } else { break; }
        }
        // Skip the blank separator if any.
        while let Some(&peek) = lines.peek() {
            if peek.trim().is_empty() { lines.next(); } else { break; }
        }

        out.push(crate::world::Social {
            name,
            min_position,
            target_required: target_required > 0,
            actor_no_arg:    std::mem::take(&mut slots[0]),
            room_no_arg:     std::mem::take(&mut slots[1]),
            actor_target:    std::mem::take(&mut slots[2]),
            room_target:     std::mem::take(&mut slots[3]),
            victim_target:   std::mem::take(&mut slots[4]),
        });
    }
    Ok(out)
}

/// Parse a CircleMUD-style help file.  Each entry is:
///
/// ```text
///   KEYWORD1 KEYWORD2 KEYWORD3 ...
///   <body lines...>
///   #<min-level>           ← terminator: standalone "#N" line OR
///   <body line ending " #N">
/// ```
///
/// File terminated by `$~`.  Min-level is the level-required gate.
fn parse_help_file(path: &str) -> Result<Vec<crate::world::HelpEntry>> {
    use crate::world::HelpEntry;
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading help file {path}"))?;
    let mut entries = Vec::new();
    let mut lines = contents.lines().peekable();
    loop {
        // Skip leading blanks.
        while let Some(l) = lines.peek() {
            if l.trim().is_empty() { lines.next(); } else { break; }
        }
        let keyword_line = match lines.next() {
            Some(l) => l.trim(),
            None => break,
        };
        if keyword_line == "$~" || keyword_line == "$" { break; }
        let keywords: Vec<String> = keyword_line
            .split_whitespace()
            .map(|w| w.to_ascii_uppercase())
            .collect();
        // Read body until terminator.
        let mut body = String::new();
        let mut min_level: i32 = 0;
        loop {
            let raw = match lines.next() {
                Some(l) => l,
                None => break,
            };
            let trimmed = raw.trim_end();
            // Standalone "#N" terminator?
            if let Some(n) = trimmed.strip_prefix('#') {
                if n.trim().chars().all(|c| c.is_ascii_digit() || c == '-') {
                    min_level = n.trim().parse().unwrap_or(0);
                    break;
                }
            }
            // Trailing " #N" terminator?
            if let Some(hash_pos) = trimmed.rfind(" #") {
                let tail = &trimmed[hash_pos + 2..];
                if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || c == '-') {
                    min_level = tail.parse().unwrap_or(0);
                    body.push_str(&trimmed[..hash_pos]);
                    body.push_str("\r\n");
                    break;
                }
            }
            body.push_str(raw);
            body.push_str("\r\n");
        }
        if keywords.is_empty() { continue; }
        entries.push(HelpEntry { keywords, min_level, body });
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Periodic zone reset tick
// ---------------------------------------------------------------------------

/// Background task: every 30 seconds, each non-sentinel, non-fighting
/// mob has ~25% chance to wander to a random adjacent room.  Sentinel
/// mobs (MOB_SENTINEL) stay put, as do mobs in combat.  Mobs flagged
/// MOB_STAY_ZONE refuse to cross zone boundaries.
pub fn spawn_wander_tick(
    world: std::sync::Arc<tokio::sync::Mutex<World>>,
    chars: crate::character::SharedChars,
) {
    const TICK_SECONDS: u64 = 30;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(TICK_SECONDS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        interval.tick().await;  // skip the immediate first fire
        loop {
            interval.tick().await;
            wander_pass(&world, &chars).await;
        }
    });
}

/// Synchronous helper: scavenger mobs in rooms with ground objects pick
/// one up (50% chance each tick).  Mob skips its own corpse and any
/// objects already in its inventory.
fn scavenge_pass(w: &mut World) {
    use rand::{Rng, seq::SliceRandom};
    let mut rng = rand::thread_rng();
    // Snapshot ids first to avoid borrow conflicts during mutation.
    let scavengers: Vec<(u32, crate::world::RoomVnum)> = w.mob_instances.iter()
        .filter_map(|m| {
            let p = w.mob_protos.get(&m.vnum)?;
            if p.mob_flags[0] & crate::world::MOB_SCAVENGER == 0 { return None; }
            if m.fighting.is_some() { return None; }
            Some((m.id, m.in_room))
        })
        .collect();
    for (mob_id, room_vnum) in scavengers {
        if rng.gen_range(0..100) >= 50 { continue; }
        // Get the room's ground items.
        let ground: Vec<u32> = w.rooms.get(&room_vnum)
            .map(|r| r.objects.clone())
            .unwrap_or_default();
        if ground.is_empty() { continue; }
        // Pick a random one (skip corpses — too gross even for scavengers).
        let candidates: Vec<u32> = ground.into_iter()
            .filter(|iid| w.obj_instances.iter()
                .find(|o| o.id == *iid)
                .map(|o| o.corpse_of.is_none()).unwrap_or(false))
            .collect();
        let Some(&iid) = candidates.choose(&mut rng) else { continue; };
        // Move from room to mob's inventory.
        if let Some(r) = w.rooms.get_mut(&room_vnum) {
            r.objects.retain(|&i| i != iid);
        }
        if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
            o.in_room = crate::world::NOWHERE;
        }
        if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
            m.inventory.push(iid);
        }
    }
}

/// Synchronous helper: pick wander targets for all eligible mobs.  Keeps
/// the !Send `rand::thread_rng` out of the async future.
fn compute_wander_moves(
    w: &World,
) -> Vec<(u32, String, crate::world::RoomVnum, crate::world::RoomVnum, crate::world::Direction)> {
    use rand::{Rng, seq::SliceRandom};
    let mut rng = rand::thread_rng();
    let mut v = Vec::new();
    for m in &w.mob_instances {
        if m.fighting.is_some() { continue; }
        let Some(proto) = w.mob_protos.get(&m.vnum) else { continue; };
        let flags = proto.mob_flags[0];
        if flags & crate::world::MOB_SENTINEL != 0 { continue; }
        if rng.gen_range(0..100) >= 25 { continue; }
        let Some(room) = w.rooms.get(&m.in_room) else { continue; };
        let stay_zone = flags & crate::world::MOB_STAY_ZONE != 0;
        let mob_zone = room.zone;
        let mut candidates: Vec<(crate::world::Direction, crate::world::RoomVnum)> = Vec::new();
        for d in crate::world::Direction::ALL {
            if let Some(e) = &room.exits[d as usize] {
                if e.to_room == crate::world::NOWHERE { continue; }
                let Some(target) = w.rooms.get(&e.to_room) else { continue; };
                if stay_zone && target.zone != mob_zone { continue; }
                if target.room_flags[0] & crate::world::ROOM_NOMOB != 0 { continue; }
                candidates.push((d, e.to_room));
            }
        }
        if let Some(&(dir, to)) = candidates.choose(&mut rng) {
            v.push((m.id, proto.short_descr.clone(), m.in_room, to, dir));
        }
    }
    v
}

async fn wander_pass(
    world: &std::sync::Arc<tokio::sync::Mutex<World>>,
    chars: &crate::character::SharedChars,
) {
    // Phase 1: snapshot candidate moves while holding the world lock.
    // The RNG is created and dropped entirely within this synchronous
    // block so the resulting future stays Send (rand::thread_rng() is !Send).
    let moves: Vec<(u32, String, crate::world::RoomVnum, crate::world::RoomVnum, crate::world::Direction)>;
    {
        let w = world.lock().await;
        moves = compute_wander_moves(&w);
    }

    // Scavenger pass: any mob with MOB_SCAVENGER, in a room with loot, has
    // a 50% chance to grab one ground item.  Independent of wandering so
    // sentinel-scavengers still hoard.
    {
        let mut w = world.lock().await;
        scavenge_pass(&mut w);
    }

    if moves.is_empty() { return; }

    // Phase 2: apply moves under a fresh lock.
    let mut entered: Vec<u32> = Vec::new();
    {
        let cl = chars.lock().await;
        let mut w = world.lock().await;
        for (mob_id, mob_name, from, to, dir) in moves {
            // Skip if mob has since started fighting or its room changed.
            let Some(m) = w.mob_instances.iter().find(|m| m.id == mob_id) else { continue; };
            if m.fighting.is_some() || m.in_room != from { continue; }
            // Move the mob between room mob-lists.
            if let Some(r) = w.rooms.get_mut(&from) {
                r.mobs.retain(|&id| id != mob_id);
            }
            if let Some(r) = w.rooms.get_mut(&to) {
                r.mobs.push(mob_id);
            }
            if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
                m.in_room = to;
            }
            // Broadcast to both rooms.
            cl.broadcast_room(from, None, &format!("{mob_name} leaves {}.\r\n", dir.name()));
            cl.broadcast_room(to,   None, &format!("{mob_name} arrives.\r\n"));
            entered.push(mob_id);
        }
    }
    // Phase 3: fire ENTRY triggers for mobs that just moved.
    for mob_id in entered {
        crate::interpreter::fire_mob_entry_triggers(mob_id, world, chars).await;
    }
}

/// Background task that decays timed objects (currently just corpses).
/// Ticks every 60 seconds; per tick subtracts 60 from each timed object's
/// `decay_in`, dumping contents into the room when timers hit zero.
pub fn spawn_decay_tick(world: std::sync::Arc<tokio::sync::Mutex<World>>) {
    const TICK_SECONDS: u64 = 60;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(TICK_SECONDS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        interval.tick().await;  // skip the immediate first fire
        loop {
            interval.tick().await;
            let mut w = world.lock().await;
            let removed = w.decay_tick(TICK_SECONDS as i32);
            if removed > 0 {
                tracing::debug!(removed, "Object decay tick");
            }
        }
    });
}

/// Global game-clock state.  35 days/month, 17 months/year matches the
/// stock CircleMUD calendar.  Hours advance every TICK_GAME_HOUR_SECS
/// of real time (one game hour ≈ 75 real seconds).
pub static GAME_HOUR:  std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
pub static GAME_DAY:   std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
pub static GAME_MONTH: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
pub static GAME_YEAR:  std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

pub const HOURS_PER_DAY:    i32 = 24;
pub const DAYS_PER_MONTH:   i32 = 35;
pub const MONTHS_PER_YEAR:  i32 = 17;

/// Stock CircleMUD sun states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SunState { Dark, Rise, Light, Set }

/// Map the current game hour to a sun state.  Matches the canonical
/// CircleMUD bands: 0-4 = dark, 5 = rise, 6-20 = light, 21 = set,
/// 22-23 = dark.
pub fn sun_state() -> SunState {
    use std::sync::atomic::Ordering;
    let h = GAME_HOUR.load(Ordering::Relaxed);
    match h {
        5         => SunState::Rise,
        6..=20    => SunState::Light,
        21        => SunState::Set,
        _         => SunState::Dark,
    }
}

/// Decide whether a room is dark from the viewer's perspective.
/// True when either (a) the room has the explicit ROOM_DARK flag, or
/// (b) the room is outdoors (not INSIDE/CITY) AND the sun is down.
pub fn is_room_dark(r: &crate::world::Room) -> bool {
    use crate::world::*;
    if (r.room_flags[0] & ROOM_DARK) != 0 { return true; }
    let outdoors = !matches!(r.sector_type, SECT_INSIDE | SECT_CITY);
    let dark_sun = matches!(sun_state(), SunState::Dark | SunState::Set);
    outdoors && dark_sun
}

/// Names of the 17 game months.  Mirrors `month_name[]` in constants.c
/// (only the first letters of each name are stock — we use full strings).
pub const MONTH_NAMES: [&str; 17] = [
    "Month of Winter",       "Month of the Winter Wolf",
    "Month of the Frost Giant", "Month of the Old Forces",
    "Month of the Grand Struggle", "Month of the Spring",
    "Month of Nature",       "Month of Futility",
    "Month of the Dragon",   "Month of the Sun",
    "Month of the Heat",     "Month of the Battle",
    "Month of the Dark Shades", "Month of the Shadows",
    "Month of the Long Shadows", "Month of the Ancient Darkness",
    "Month of the Great Evil",
];

/// Periodic tick that advances the game clock by one hour every
/// `HOUR_REAL_SECS` real seconds.  Wraps day/month/year overflow.
pub fn spawn_time_tick() {
    const HOUR_REAL_SECS: u64 = 75;
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(HOUR_REAL_SECS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        interval.tick().await;        // skip immediate fire
        loop {
            interval.tick().await;
            let h = GAME_HOUR.load(Ordering::Relaxed) + 1;
            if h >= HOURS_PER_DAY {
                GAME_HOUR.store(0, Ordering::Relaxed);
                let d = GAME_DAY.load(Ordering::Relaxed) + 1;
                if d >= DAYS_PER_MONTH {
                    GAME_DAY.store(0, Ordering::Relaxed);
                    let m = GAME_MONTH.load(Ordering::Relaxed) + 1;
                    if m >= MONTHS_PER_YEAR {
                        GAME_MONTH.store(0, Ordering::Relaxed);
                        GAME_YEAR.fetch_add(1, Ordering::Relaxed);
                    } else {
                        GAME_MONTH.store(m, Ordering::Relaxed);
                    }
                } else {
                    GAME_DAY.store(d, Ordering::Relaxed);
                }
            } else {
                GAME_HOUR.store(h, Ordering::Relaxed);
            }
        }
    });
}

/// Background task that disconnects mortals who have been idle longer
/// than `IDLE_LIMIT_SECS` since their last command.  Immortals are
/// exempt.  Disconnect is performed by posting a "quit" command
/// through the FORCE_CMD_TX channel — that routes through the normal
/// dispatcher loop and triggers the usual auto-save + cleanup path.
pub fn spawn_idle_kick_tick(
    world: std::sync::Arc<tokio::sync::Mutex<World>>,
    chars: crate::character::SharedChars,
) {
    const TICK_SECONDS:     u64 = 60;
    const IDLE_LIMIT_SECS:  u64 = 30 * 60;       // 30 minutes
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(TICK_SECONDS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        interval.tick().await;
        loop {
            interval.tick().await;
            let handles: Vec<crate::character::PlayerHandle> = {
                let cl = chars.lock().await;
                cl.iter().cloned().collect()
            };
            let now = std::time::Instant::now();
            for ph in handles {
                let kick = {
                    let c = ph.character.lock().await;
                    c.level < 34
                        && now.duration_since(c.last_activity).as_secs() > IDLE_LIMIT_SECS
                };
                if !kick { continue; }
                let _ = ph.send.send(
                    "\r\nYou have been idle too long — link severed.\r\n".to_string(),
                );
                if let Some(tx) = crate::interpreter::FORCE_CMD_TX.get() {
                    let _ = tx.send(crate::interpreter::ForceCmdMsg {
                        player:  ph.name.clone(),
                        command: "quit".to_string(),
                        world:   std::sync::Arc::clone(&world),
                        chars:   std::sync::Arc::clone(&chars),
                    });
                }
            }
        }
    });
}

/// Background task that ticks each online mortal's hunger/thirst once
/// per game-hour (~60s of real time).  `-1` is the never-hungry
/// sentinel (immortals).  At 0 the player gets a one-shot warning; at
/// -1 they take 1 HP damage per tick with a "starving/parched" line.
/// Crash-safe save-all tick.  Every 5 minutes, walks online players
/// and persists each one's full state via
/// `interpreter::save_character_to_db`.  Mirrors what auto-save-on-
/// disconnect already does, but doesn't wait for clean logout.
pub fn spawn_save_all_tick(
    chars: crate::character::SharedChars,
    players: std::sync::Arc<tokio::sync::Mutex<crate::players::PlayerDb>>,
) {
    const TICK_SECONDS: u64 = 300;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(TICK_SECONDS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        interval.tick().await;
        loop {
            interval.tick().await;
            let handles: Vec<crate::character::PlayerHandle> = {
                let cl = chars.lock().await;
                cl.iter().cloned().collect()
            };
            let mut saved = 0u32;
            for ph in handles {
                // Skip immortals (their state is more volatile and we
                // already auto-save on disconnect).
                let me = ph.character.lock().await;
                if me.level >= 34 { continue; }
                if let Err(e) = crate::interpreter::save_character_to_db(&me, &players).await {
                    tracing::warn!(name = %me.name, error = %e,
                        "Periodic save failed");
                } else {
                    saved += 1;
                }
            }
            if saved > 0 {
                tracing::info!(saved, "Periodic save-all complete");
            }
        }
    });
}

pub fn spawn_hunger_tick(chars: crate::character::SharedChars) {
    const TICK_SECONDS: u64 = 60;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(TICK_SECONDS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        interval.tick().await;       // skip the immediate first fire
        loop {
            interval.tick().await;
            let handles: Vec<crate::character::PlayerHandle> = {
                let cl = chars.lock().await;
                cl.iter().cloned().collect()
            };
            for ph in handles {
                let mut c = ph.character.lock().await;
                let mut hunger_msg: Option<&'static str> = None;
                let mut thirst_msg: Option<&'static str> = None;
                if c.hunger > 0 {
                    c.hunger -= 1;
                    if c.hunger == 0 { hunger_msg = Some("You are hungry.\r\n"); }
                } else if c.hunger == 0 {
                    c.hp = (c.hp - 1).max(0);
                    hunger_msg = Some("You are starving — your strength fades.\r\n");
                }
                if c.thirst > 0 {
                    c.thirst -= 1;
                    if c.thirst == 0 { thirst_msg = Some("You are thirsty.\r\n"); }
                } else if c.thirst == 0 {
                    c.hp = (c.hp - 1).max(0);
                    thirst_msg = Some("You are parched — your strength fades.\r\n");
                }
                drop(c);
                if let Some(m) = hunger_msg { let _ = ph.send.send(m.to_string()); }
                if let Some(m) = thirst_msg { let _ = ph.send.send(m.to_string()); }
            }
        }
    });
}

/// Background task that ticks per-instance object timers (the OTRIG_TIMER
/// mechanism). Each prototype's `timer` field (in MUD-hours) seeds a
/// per-instance countdown when the object spawns; on expiry, the object's
/// 'f' OBJ trigger fires (if any) before the instance is extracted.
pub fn spawn_obj_timer_tick(
    world: std::sync::Arc<tokio::sync::Mutex<World>>,
    chars: crate::character::SharedChars,
) {
    const TICK_SECONDS: u64 = 60;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(TICK_SECONDS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        interval.tick().await;
        loop {
            interval.tick().await;
            let expired = {
                let mut w = world.lock().await;
                w.obj_timer_tick(TICK_SECONDS as i32)
            };
            if expired.is_empty() { continue; }
            for (iid, room, _vnum) in &expired {
                if *room != crate::world::NOWHERE {
                    crate::interpreter::fire_obj_timer_triggers(
                        *iid, *room, &world, &chars,
                    ).await;
                }
            }
            let mut w = world.lock().await;
            for (iid, _, _) in expired {
                w.extract_obj(iid);
            }
        }
    });
}

/// Background task that rolls every mob's and room's RANDOM ('b')
/// triggers on a fixed cadence — roughly the CircleMUD PULSE_MOBILE
/// cadence (~13s), but we use 30s to keep the per-tick cost reasonable
/// against the ~12700 rooms / ~7282 mobs of stock data.  Each fire is
/// gated on `trigger.narg` percent, so most rolls no-op.
pub fn spawn_random_trigger_tick(
    world: std::sync::Arc<tokio::sync::Mutex<World>>,
    chars: crate::character::SharedChars,
) {
    const TICK_SECONDS: u64 = 30;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(TICK_SECONDS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        interval.tick().await;       // skip the immediate first fire
        loop {
            interval.tick().await;
            // Snapshot the ids of entities holding any 'b' trigger so
            // we don't hold the world lock across each fire (each fire
            // re-locks the world via apply_script_outputs).
            let (mob_ids, room_vnums): (Vec<u32>, Vec<i32>) = {
                let w = world.lock().await;
                let mids: Vec<u32> = w.mob_instances.iter()
                    .filter(|m| m.triggers.iter().any(|tv| {
                        w.triggers.get(tv).map(|t| t.trigger_type == 'b').unwrap_or(false)
                    }))
                    .map(|m| m.id).collect();
                let rvs: Vec<i32> = w.rooms.values()
                    .filter(|r| r.triggers.iter().any(|tv| {
                        w.triggers.get(tv).map(|t| t.trigger_type == 'b').unwrap_or(false)
                    }))
                    .map(|r| r.vnum).collect();
                (mids, rvs)
            };
            for mid in mob_ids {
                crate::interpreter::fire_mob_random_tick(mid, &world, &chars).await;
            }
            for rv in room_vnums {
                crate::interpreter::fire_room_random_tick(rv, &world, &chars).await;
            }
        }
    });
}

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

/// Spawn the mob spec_proc tick — fires every PULSE seconds, walks all
/// mobs holding a `spec`, and dispatches per-spec behavior.  Mirrors
/// CircleMUD's mobile_activity() spec call, just simplified.
pub fn spawn_mob_spec_tick(
    world: std::sync::Arc<tokio::sync::Mutex<World>>,
    chars: crate::character::SharedChars,
) {
    const PULSE_SECONDS: u64 = 10;
    tokio::spawn(async move {
        use rand::seq::SliceRandom;
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(PULSE_SECONDS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        interval.tick().await;
        // Idle utterance pool for Puff.
        const PUFF_PHRASES: &[&str] = &[
            "My god!  It's full of stars!",
            "How'd all those fish get up here?",
            "I'm so very tired.",
            "I wish star trek was real.",
        ];
        loop {
            interval.tick().await;

            // Phase A: snapshot all mobs with a spec — id, vnum, room,
            // spec, short_descr — under one world lock.
            let mut acts: Vec<(u32, crate::world::RoomVnum, crate::world::MobSpec, String)> = Vec::new();
            {
                let w = world.lock().await;
                for m in &w.mob_instances {
                    if let Some(spec) = m.spec {
                        let short = w.mob_protos.get(&m.vnum)
                            .map(|p| p.short_descr.clone())
                            .unwrap_or_default();
                        acts.push((m.id, m.in_room, spec, short));
                    }
                }
            }
            if acts.is_empty() { continue; }

            // Phase B: per-spec dispatch.
            for (mob_id, room, spec, short) in acts {
                match spec {
                    crate::world::MobSpec::Puff => {
                        // 30% chance per tick to utter a random phrase.
                        let say = {
                            use rand::Rng;
                            let mut rng = rand::thread_rng();
                            if rng.gen_range(1..=100) <= 30 {
                                PUFF_PHRASES.choose(&mut rng).copied()
                            } else { None }
                        };
                        if let Some(phrase) = say {
                            chars.lock().await.broadcast_room(
                                room, None,
                                &format!("{short} says, '{phrase}'\r\n"),
                            );
                        }
                    }
                    crate::world::MobSpec::Fido => {
                        // Find a corpse in the room and eat it.
                        let corpse_iid = {
                            let w = world.lock().await;
                            w.rooms.get(&room)
                                .and_then(|r| r.objects.iter()
                                    .find(|&&iid| {
                                        w.obj_instances.iter()
                                            .find(|o| o.id == iid)
                                            .map(|o| o.corpse_of.is_some())
                                            .unwrap_or(false)
                                    })
                                    .copied())
                        };
                        if let Some(iid) = corpse_iid {
                            let label = {
                                let w = world.lock().await;
                                w.obj_instances.iter()
                                    .find(|o| o.id == iid)
                                    .and_then(|o| o.corpse_of.clone())
                                    .unwrap_or_else(|| "a corpse".to_string())
                            };
                            chars.lock().await.broadcast_room(
                                room, None,
                                &format!("{short} savagely devours {label}.\r\n"),
                            );
                            // Extract the corpse and any contents.
                            let mut w = world.lock().await;
                            w.extract_obj(iid);
                        }
                    }
                    crate::world::MobSpec::Cityguard => {
                        // Already fighting? skip.
                        let busy = {
                            let w = world.lock().await;
                            w.mob_instances.iter()
                                .find(|m| m.id == mob_id)
                                .map(|m| m.fighting.is_some())
                                .unwrap_or(true)
                        };
                        if busy { continue; }
                        // Find any other mob in the room currently
                        // fighting a player; engage that mob.
                        let aggressor = {
                            let w = world.lock().await;
                            w.rooms.get(&room).and_then(|r| {
                                r.mobs.iter()
                                    .filter(|&&mid| mid != mob_id)
                                    .find_map(|&mid| {
                                        let m = w.mob_instances.iter().find(|m| m.id == mid)?;
                                        if let Some(t) = m.fighting {
                                            if t.is_player { return Some((mid, m.vnum)); }
                                        }
                                        None
                                    })
                            })
                        };
                        if let Some((tid, tvnum)) = aggressor {
                            let target_name = {
                                let w = world.lock().await;
                                w.mob_protos.get(&tvnum)
                                    .map(|p| p.short_descr.clone())
                                    .unwrap_or_default()
                            };
                            {
                                let mut w = world.lock().await;
                                if let Some(g) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
                                    g.fighting = Some(crate::character::Target {
                                        id: tid, is_player: false,
                                    });
                                }
                            }
                            chars.lock().await.broadcast_room(
                                room, None,
                                &format!(
                                    "{short} draws steel and charges {target_name}!\r\n",
                                ),
                            );
                        }
                    }
                    crate::world::MobSpec::Snake => {
                        // Snake's poison-on-hit lives in combat.rs;
                        // no per-tick behavior here.
                    }
                    crate::world::MobSpec::Janitor => {
                        // Pick up the first non-corpse floor object whose
                        // weight ≤ 5 (CircleMUD's threshold) — but we don't
                        // track weight per-instance, so use the proto.
                        let pickup = {
                            let w = world.lock().await;
                            w.rooms.get(&room).and_then(|r| {
                                r.objects.iter().find(|&&iid| {
                                    let o = match w.obj_instances.iter().find(|x| x.id == iid) {
                                        Some(o) => o, None => return false,
                                    };
                                    if o.corpse_of.is_some() { return false; }
                                    w.obj_protos.get(&o.vnum)
                                        .map(|p| p.weight <= 5)
                                        .unwrap_or(false)
                                }).copied()
                            })
                        };
                        if let Some(iid) = pickup {
                            let label = {
                                let w = world.lock().await;
                                w.obj_instances.iter()
                                    .find(|o| o.id == iid)
                                    .and_then(|o| w.obj_protos.get(&o.vnum)
                                        .map(|p| p.short_description.clone()))
                                    .unwrap_or_else(|| "something".to_string())
                            };
                            {
                                let mut w = world.lock().await;
                                if let Some(r) = w.rooms.get_mut(&room) {
                                    r.objects.retain(|&i| i != iid);
                                }
                                if let Some(o) = w.obj_instances.iter_mut().find(|x| x.id == iid) {
                                    o.in_room = crate::world::NOWHERE;
                                }
                                if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
                                    m.inventory.push(iid);
                                }
                            }
                            chars.lock().await.broadcast_room(
                                room, None,
                                &format!("{short} picks up {label}.\r\n"),
                            );
                        }
                    }
                }
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

        // V commands set variables — we skip them for now.
        if first == 'V' {
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
                    // 'A' then a line "<location> <modifier>" — fill into
                    // ObjProto.affected so wear-time stat bonuses can apply.
                    let _ = s.next_line(); // 'A'
                    if let Some(values_line) = s.next_line() {
                        let toks: Vec<&str> = values_line.split_whitespace().collect();
                        if toks.len() >= 2 {
                            let location: i32 = toks[0].parse().unwrap_or(0);
                            let modifier: i32 = toks[1].parse().unwrap_or(0);
                            if location != crate::world::APPLY_NONE && modifier != 0 {
                                o.affected.push(crate::world::ObjAffect { location, modifier });
                            }
                        }
                    }
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
pub fn reset_zone(world: &mut World, zone_vnum: i32) {
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
    let mut last_obj_id: Option<u32> = None;
    let mut last_room_vnum: Option<crate::world::RoomVnum> = None;

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
                        remembers: Vec::new(),
                        triggers: Vec::new(),
                        affects: Vec::new(),
                        charmer: None,
                        spec: crate::world::MobSpec::for_vnum(cmd.arg1),
                    });
                    last_mob_id = Some(id);
                    last_room_vnum = Some(cmd.arg3);
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
                let proto_timer = world.obj_protos.get(&cmd.arg1).map(|p| p.timer).unwrap_or(0);
                let init_timer = if proto_timer > 0 { Some(proto_timer.saturating_mul(75)) } else { None };
                if cmd.arg3 == -1 {
                    // Limbo / nowhere — load but don't place.
                    let id = next_obj_id; next_obj_id += 1;
                    world.obj_instances.push(ObjInstance {
                        id, vnum: cmd.arg1, in_room: crate::world::NOWHERE,
                        contents: Vec::new(),
                        corpse_of: None,
                        decay_in: None,
                        triggers: Vec::new(),
                        timer: init_timer,
                        light_lit: false,
                    });
                    last_obj_id = Some(id);
                    last_cmd_ok = true;
                } else if let Some(room) = world.rooms.get_mut(&cmd.arg3) {
                    let id = next_obj_id; next_obj_id += 1;
                    room.objects.push(id);
                    world.obj_instances.push(ObjInstance {
                        id, vnum: cmd.arg1, in_room: cmd.arg3,
                        contents: Vec::new(),
                        corpse_of: None,
                        decay_in: None,
                        triggers: Vec::new(),
                        timer: init_timer,
                        light_lit: false,
                    });
                    last_obj_id = Some(id);
                    last_room_vnum = Some(cmd.arg3);
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
                let proto_timer = world.obj_protos.get(&cmd.arg1).map(|p| p.timer).unwrap_or(0);
                let init_timer = if proto_timer > 0 { Some(proto_timer.saturating_mul(75)) } else { None };
                let id = next_obj_id; next_obj_id += 1;
                world.obj_instances.push(ObjInstance {
                    id, vnum: cmd.arg1, in_room: crate::world::NOWHERE,
                    contents: Vec::new(),
                    corpse_of: None,
                    decay_in: None,
                    triggers: Vec::new(),
                    timer: init_timer,
                        light_lit: false,
                });
                if let Some(m) = world.mob_instances.iter_mut().find(|m| m.id == mob_id) {
                    m.inventory.push(id);
                }
                last_cmd_ok = true;
            }
            'P' => {
                // Put obj-in-obj — spawn the inner object and push it into
                // the most-recently-loaded container's `contents`. arg3 is
                // the target container vnum; we find the latest live
                // instance of that vnum (typically the most-recent O cmd).
                if !world.obj_protos.contains_key(&cmd.arg1) {
                    last_cmd_ok = false;
                    continue;
                }
                if world.count_obj(cmd.arg1) >= cmd.arg2 {
                    last_cmd_ok = false;
                    continue;
                }
                // Find the most recently-created instance with the target vnum.
                let target_iid = world.obj_instances.iter().rev()
                    .find(|o| o.vnum == cmd.arg3)
                    .map(|o| o.id);
                let proto_timer = world.obj_protos.get(&cmd.arg1).map(|p| p.timer).unwrap_or(0);
                let init_timer = if proto_timer > 0 { Some(proto_timer.saturating_mul(75)) } else { None };
                let id = next_obj_id; next_obj_id += 1;
                world.obj_instances.push(ObjInstance {
                    id, vnum: cmd.arg1, in_room: crate::world::NOWHERE,
                    contents: Vec::new(),
                    corpse_of: None,
                    decay_in: None,
                    triggers: Vec::new(),
                    timer: init_timer,
                        light_lit: false,
                });
                if let Some(tid) = target_iid {
                    if let Some(t) = world.obj_instances.iter_mut().find(|o| o.id == tid) {
                        t.contents.push(id);
                    }
                }
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
                // D <if> <room_vnum> <direction> <state>
                //   state: 0=open, 1=closed, 2=closed+locked.
                use crate::world::{EX_CLOSED, EX_LOCKED};
                let room_vnum = cmd.arg1;
                let dir       = cmd.arg2 as usize;
                let state     = cmd.arg3;
                if dir >= 6 { last_cmd_ok = false; continue; }
                // Apply to the source room.
                let to_room = if let Some(r) = world.rooms.get_mut(&room_vnum) {
                    if let Some(ex) = r.exits[dir].as_mut() {
                        ex.exit_info &= !(EX_CLOSED | EX_LOCKED);
                        match state {
                            1 => ex.exit_info |= EX_CLOSED,
                            2 => ex.exit_info |= EX_CLOSED | EX_LOCKED,
                            _ => {}
                        }
                        ex.to_room
                    } else { crate::world::NOWHERE }
                } else { crate::world::NOWHERE };
                // Mirror the state on the reverse-side exit so both
                // halves of the door stay consistent.
                let rev = match dir { 0=>2, 1=>3, 2=>0, 3=>1, 4=>5, 5=>4, _=>0 };
                if to_room != crate::world::NOWHERE {
                    if let Some(r) = world.rooms.get_mut(&to_room) {
                        if let Some(ex) = r.exits[rev].as_mut() {
                            ex.exit_info &= !(EX_CLOSED | EX_LOCKED);
                            match state {
                                1 => ex.exit_info |= EX_CLOSED,
                                2 => ex.exit_info |= EX_CLOSED | EX_LOCKED,
                                _ => {}
                            }
                        }
                    }
                }
                last_cmd_ok = true;
            }
            'T' => {
                // Attach trigger arg2 to the last-loaded entity.
                // arg1 = attach_type (0 mob, 1 obj, 2 room), arg2 = trig vnum.
                if !world.triggers.contains_key(&cmd.arg2) {
                    last_cmd_ok = false;
                    continue;
                }
                match cmd.arg1 {
                    0 /* mob */ => {
                        if let Some(mid) = last_mob_id {
                            if let Some(m) = world.mob_instances.iter_mut().find(|m| m.id == mid) {
                                m.triggers.push(cmd.arg2);
                                last_cmd_ok = true;
                            } else { last_cmd_ok = false; }
                        } else { last_cmd_ok = false; }
                    }
                    1 /* obj */ => {
                        if let Some(oid) = last_obj_id {
                            if let Some(o) = world.obj_instances.iter_mut().find(|o| o.id == oid) {
                                o.triggers.push(cmd.arg2);
                                last_cmd_ok = true;
                            } else { last_cmd_ok = false; }
                        } else { last_cmd_ok = false; }
                    }
                    2 /* room */ => {
                        if let Some(rv) = last_room_vnum {
                            if let Some(r) = world.rooms.get_mut(&rv) {
                                r.triggers.push(cmd.arg2);
                                last_cmd_ok = true;
                            } else { last_cmd_ok = false; }
                        } else { last_cmd_ok = false; }
                    }
                    _ => { last_cmd_ok = false; }
                }
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
                    use crate::world::{EX_ISDOOR, EX_PICKPROOF, EX_HIDDEN};
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
