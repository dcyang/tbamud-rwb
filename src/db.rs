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
pub fn load_world(data_dir: &str, mini: bool) -> Result<World> {
    let mut world = World::default();
    if mini {
        tracing::info!("Mini-MUD mode: loading minimal world (index.mini)");
    }

    let zon_dir = format!("{data_dir}/world/zon");
    let wld_dir = format!("{data_dir}/world/wld");
    let obj_dir = format!("{data_dir}/world/obj");
    let mob_dir = format!("{data_dir}/world/mob");

    // --- Zones -------------------------------------------------------------
    for fname in read_index(&zon_dir, mini)? {
        let path = PathBuf::from(&zon_dir).join(&fname);
        parse_zone_file(&path, &mut world)
            .with_context(|| format!("Parsing zone file {}", path.display()))?;
    }
    tracing::info!(count = world.zones.len(), "Loaded zones");

    // --- Rooms -------------------------------------------------------------
    for fname in read_index(&wld_dir, mini)? {
        let path = PathBuf::from(&wld_dir).join(&fname);
        parse_room_file(&path, &mut world)
            .with_context(|| format!("Parsing room file {}", path.display()))?;
    }
    tracing::info!(count = world.rooms.len(), "Loaded rooms");

    // --- Object prototypes ------------------------------------------------
    for fname in read_index(&obj_dir, mini)? {
        let path = PathBuf::from(&obj_dir).join(&fname);
        parse_object_file(&path, &mut world)
            .with_context(|| format!("Parsing object file {}", path.display()))?;
    }
    tracing::info!(count = world.obj_protos.len(), "Loaded object prototypes");

    // --- Mob prototypes ----------------------------------------------------
    for fname in read_index(&mob_dir, mini)? {
        let path = PathBuf::from(&mob_dir).join(&fname);
        parse_mob_file(&path, &mut world)
            .with_context(|| format!("Parsing mob file {}", path.display()))?;
    }
    tracing::info!(count = world.mob_protos.len(), "Loaded mob prototypes");

    // --- Triggers ----------------------------------------------------------
    let trg_dir = format!("{data_dir}/world/trg");
    if std::path::Path::new(&index_path(&trg_dir, mini)).exists() {
        for fname in read_index(&trg_dir, mini)? {
            let path = PathBuf::from(&trg_dir).join(&fname);
            if let Err(e) = parse_trigger_file(&path, &mut world) {
                tracing::warn!(path = %path.display(), error = %e, "Trigger parse error, skipping");
            }
        }
        tracing::info!(count = world.triggers.len(), "Loaded triggers");
    }

    // --- Quests ------------------------------------------------------------
    let qst_dir = format!("{data_dir}/world/qst");
    if std::path::Path::new(&index_path(&qst_dir, mini)).exists() {
        for fname in read_index(&qst_dir, mini)? {
            let path = PathBuf::from(&qst_dir).join(&fname);
            if let Err(e) = parse_quest_file(&path, &mut world) {
                tracing::warn!(path = %path.display(), error = %e, "Quest parse error, skipping");
            }
        }
        tracing::info!(count = world.quests.len(), "Loaded quests");
    }

    // --- Shops -------------------------------------------------------------
    let shp_dir = format!("{data_dir}/world/shp");
    if std::path::Path::new(&index_path(&shp_dir, mini)).exists() {
        for fname in read_index(&shp_dir, mini)? {
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

    // --- Inject synthetic newbie-kit prototypes ---------------------------
    // Reserved vnums 99001-99004 with the right item_type/values so that
    // descriptor's first-login path can spawn class-appropriate gear via
    // World::spawn_obj without relying on world-data vnums.
    inject_newbie_kit_protos(&mut world);

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

    // --- Restore house contents -------------------------------------------
    // For every room flagged ROOM_HOUSE, load its `.house` file (if any)
    // and spawn the persisted objects into the room.
    let house_rooms: Vec<i32> = world.rooms.iter()
        .filter(|(_, r)| r.room_flags[0] & crate::world::ROOM_HOUSE != 0)
        .map(|(v, _)| *v).collect();
    let mut restored_total = 0usize;
    for rv in &house_rooms {
        restored_total += load_house(data_dir, *rv, &mut world);
        if let Some(owner) = load_house_owner(data_dir, *rv) {
            world.house_owners.insert(*rv, owner);
        }
    }
    if restored_total > 0 || !world.house_owners.is_empty() {
        tracing::info!(
            items = restored_total,
            houses = world.house_owners.len(),
            "Restored house contents",
        );
    }

    Ok(world)
}

/// Read the owner name from `<data_dir>/house/<vnum>.owner`, if any.
pub fn load_house_owner(data_dir: &str, room_vnum: i32) -> Option<String> {
    let path = format!("{data_dir}/house/{room_vnum}.owner");
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Write the owner name to `<data_dir>/house/<vnum>.owner`.  Pass an
/// empty string to clear (file is removed).
pub fn save_house_owner(data_dir: &str, room_vnum: i32, owner: &str) {
    let dir = format!("{data_dir}/house");
    let _ = std::fs::create_dir_all(&dir);
    let path = format!("{dir}/{room_vnum}.owner");
    if owner.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        let _ = std::fs::write(&path, owner);
    }
}

/// Persist the floor objects of a ROOM_HOUSE room to
/// `<data_dir>/house/<vnum>.house`.  One line per top-level object:
/// `<vnum> c=<condition> [content_vnum content_vnum ...]`.
pub fn save_house(data_dir: &str, room_vnum: i32, world: &crate::world::World) {
    use std::io::Write;
    let dir = format!("{data_dir}/house");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, "Failed to create house dir");
        return;
    }
    let path = format!("{dir}/{room_vnum}.house");
    let Some(r) = world.rooms.get(&room_vnum) else { return; };
    let mut lines = String::new();
    lines.push_str("# tbamud-rwb house v1 — <vnum> c=<cond> [content_vnums...]\n");
    for &iid in &r.objects {
        let Some(o) = world.obj_instances.iter().find(|o| o.id == iid) else { continue; };
        // Skip corpses (transient).
        if o.corpse_of.is_some() { continue; }
        let mut line = format!("{} c={}", o.vnum, o.condition);
        if let Some(s) = o.brewed_spell {
            line.push_str(&format!(" b={s}"));
        }
        for a in &o.bonus_affects {
            line.push_str(&format!(" a={}:{}", a.location, a.modifier));
        }
        for &cid in &o.contents {
            if let Some(c) = world.obj_instances.iter().find(|x| x.id == cid) {
                line.push(' ');
                line.push_str(&c.vnum.to_string());
            }
        }
        line.push('\n');
        lines.push_str(&line);
    }
    if let Ok(mut f) = std::fs::File::create(&path) {
        let _ = f.write_all(lines.as_bytes());
    }
}

/// Load a house file from disk and spawn its objects into the given
/// room.  Idempotent in the sense of "items present at the moment we
/// load" — duplicate boot calls will pile up.
pub fn load_house(data_dir: &str, room_vnum: i32, world: &mut crate::world::World) -> usize {
    let path = format!("{data_dir}/house/{room_vnum}.house");
    let Ok(body) = std::fs::read_to_string(&path) else { return 0; };
    let mut count = 0;
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') { continue; }
        let parts: Vec<&str> = t.split_ascii_whitespace().collect();
        if parts.is_empty() { continue; }
        let Ok(vnum) = parts[0].parse::<i32>() else { continue; };
        let mut condition = 100;
        let mut brewed_spell: Option<i32> = None;
        let mut bonus_affects: Vec<crate::world::ObjAffect> = Vec::new();
        let mut contents: Vec<i32> = Vec::new();
        for tok in &parts[1..] {
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
                let mut ps = rest.split(':');
                if let (Some(loc), Some(modi)) = (ps.next(), ps.next()) {
                    if let (Ok(l), Ok(m)) = (loc.parse::<i32>(), modi.parse::<i32>()) {
                        bonus_affects.push(crate::world::ObjAffect { location: l, modifier: m });
                        continue;
                    }
                }
            }
            if let Ok(n) = tok.parse::<i32>() { contents.push(n); }
        }
        let Some(iid) = world.spawn_obj(vnum) else { continue; };
        if let Some(o) = world.obj_instances.iter_mut().find(|o| o.id == iid) {
            o.in_room = room_vnum;
            o.condition = condition;
            o.brewed_spell = brewed_spell;
            o.bonus_affects = bonus_affects;
        }
        if let Some(r) = world.rooms.get_mut(&room_vnum) {
            r.objects.push(iid);
        }
        for cv in contents {
            if let Some(cid) = world.spawn_obj(cv) {
                if let Some(container) = world.obj_instances.iter_mut().find(|o| o.id == iid) {
                    container.contents.push(cid);
                }
            }
        }
        count += 1;
    }
    count
}

/// Vnums reserved for synthetic newbie-kit objects.  See
/// `inject_newbie_kit_protos`.
/// D&D 5e class starting-equipment kits (PHB Chapter 3 Core Traits tables).
/// Returns the class's "Choose A or B" (Fighter: A/B/C) options, each an
/// `(option_description, &[(obj_vnum, quantity)], bonus_gold)`.  The final
/// option of each class is gold-only (empty item list).  Objects live in
/// `lib/world/obj/{0,4}.obj`.
#[allow(clippy::type_complexity)]
pub fn class_kit(
    class: crate::players::Class,
) -> &'static [(&'static str, &'static [(crate::world::ObjVnum, i32)], i64)] {
    use crate::players::Class;
    match class {
        Class::Barbarian => &[
            ("a greataxe, 4 handaxes, an explorer's pack, and 15 gold",
             &[(458,1),(459,4),(477,1)], 15),
            ("75 gold pieces", &[], 75),
        ],
        Class::Bard => &[
            ("leather armor, 2 daggers, a musical instrument, an entertainer's pack, and 19 gold",
             &[(468,1),(120,2),(455,1),(478,1)], 19),
            ("90 gold pieces", &[], 90),
        ],
        Class::Cleric => &[
            ("a chain shirt, a shield, a mace, a holy symbol, a priest's pack, and 7 gold",
             &[(470,1),(472,1),(460,1),(432,1),(479,1)], 7),
            ("110 gold pieces", &[], 110),
        ],
        Class::Druid => &[
            ("leather armor, a shield, a sickle, a quarterstaff (druidic focus), an explorer's pack, a herbalism kit, and 9 gold",
             &[(468,1),(472,1),(408,1),(405,1),(477,1),(457,1)], 9),
            ("50 gold pieces", &[], 50),
        ],
        Class::Fighter => &[
            ("chain mail, a greatsword, a flail, 8 javelins, a dungeoneer's pack, and 4 gold",
             &[(471,1),(461,1),(462,1),(463,8),(480,1)], 4),
            ("studded leather, a scimitar, a shortsword, a longbow, 20 arrows, a quiver, a dungeoneer's pack, and 11 gold",
             &[(469,1),(465,1),(464,1),(466,1),(421,1),(425,1),(480,1)], 11),
            ("155 gold pieces", &[], 155),
        ],
        Class::Monk => &[
            ("a spear, 5 daggers, artisan's tools, an explorer's pack, and 11 gold",
             &[(404,1),(120,5),(446,1),(477,1)], 11),
            ("50 gold pieces", &[], 50),
        ],
        Class::Paladin => &[
            ("chain mail, a shield, a longsword, 6 javelins, a holy symbol, a priest's pack, and 9 gold",
             &[(471,1),(472,1),(467,1),(463,6),(432,1),(479,1)], 9),
            ("150 gold pieces", &[], 150),
        ],
        Class::Ranger => &[
            ("studded leather, a scimitar, a shortsword, a longbow, 20 arrows, a quiver, a sprig of mistletoe (druidic focus), an explorer's pack, and 7 gold",
             &[(469,1),(465,1),(464,1),(466,1),(421,1),(425,1),(473,1),(477,1)], 7),
            ("150 gold pieces", &[], 150),
        ],
        Class::Rogue => &[
            ("leather armor, 2 daggers, a shortsword, a shortbow, 20 arrows, a quiver, thieves' tools, a burglar's pack, and 8 gold",
             &[(468,1),(120,2),(464,1),(413,1),(421,1),(425,1),(452,1),(481,1)], 8),
            ("100 gold pieces", &[], 100),
        ],
        Class::Sorcerer => &[
            ("a spear, 2 daggers, an arcane focus (crystal), a dungeoneer's pack, and 28 gold",
             &[(404,1),(120,2),(474,1),(480,1)], 28),
            ("50 gold pieces", &[], 50),
        ],
        Class::Warlock => &[
            ("leather armor, a sickle, 2 daggers, an arcane focus (orb), a book of occult lore, a scholar's pack, and 15 gold",
             &[(468,1),(408,1),(120,2),(475,1),(442,1),(482,1)], 15),
            ("100 gold pieces", &[], 100),
        ],
        Class::Wizard => &[
            ("2 daggers, an arcane focus (quarterstaff), a robe, a spellbook, a scholar's pack, and 5 gold",
             &[(120,2),(405,1),(426,1),(476,1),(482,1)], 5),
            ("55 gold pieces", &[], 55),
        ],
        Class::Undefined => &[("nothing of note", &[], 0)],
    }
}

/// D&D 5e background starting-equipment kits (PHB Chapter 4, logical
/// pp.178–185).  Maps a background name to its option-A description, the
/// option-A item list `(obj_vnum, quantity)`, the option-A bonus gold, and the
/// option-B gold.  Objects live in `lib/world/obj/{0,4}.obj`.  Option B is "50
/// gold pieces" for every background except Guard (the PHB gives Guard 50 *copper*).
#[allow(clippy::type_complexity)]
pub fn background_kit(
    name: &str,
) -> Option<(&'static str, &'static [(crate::world::ObjVnum, i32)], i64, i64)> {
    // vnums: dagger=120 (reused); the rest are 4.obj 404-457.
    Some(match name {
        "Acolyte" => (
            "calligrapher's supplies, a book of prayers, a holy symbol, parchment, a robe, and 8 gold",
            &[(445,1),(442,1),(432,1),(431,1),(426,1)], 8, 50),
        "Artisan" => (
            "a set of artisan's tools, 2 pouches, traveler's clothes, and 32 gold",
            &[(446,1),(424,2),(428,1)], 32, 50),
        "Charlatan" => (
            "a forgery kit, a costume, fine clothes, and 15 gold",
            &[(448,1),(430,1),(429,1)], 15, 50),
        "Criminal" => (
            "2 daggers, thieves' tools, a crowbar, 2 pouches, traveler's clothes, and 16 gold",
            &[(120,2),(452,1),(441,1),(424,2),(428,1)], 16, 50),
        "Entertainer" => (
            "a musical instrument, 2 costumes, a mirror, perfume, traveler's clothes, and 11 gold",
            &[(455,1),(430,2),(434,1),(435,1),(428,1)], 11, 50),
        "Farmer" => (
            "a sickle, carpenter's tools, a healer's kit, an iron pot, a shovel, traveler's clothes, and 30 gold",
            &[(408,1),(449,1),(451,1),(444,1),(436,1),(428,1)], 30, 50),
        "Guard" => (
            "a spear, a light crossbow, 20 bolts, a gaming set, a hooded lantern, manacles, a quiver, traveler's clothes, and 12 gold",
            &[(404,1),(412,1),(414,1),(456,1),(423,1),(438,1),(425,1),(428,1)], 12, 0),
        "Guide" => (
            "a shortbow, 20 arrows, cartographer's tools, a bedroll, a quiver, a tent, traveler's clothes, and 3 gold",
            &[(413,1),(421,1),(453,1),(439,1),(425,1),(440,1),(428,1)], 3, 50),
        "Hermit" => (
            "a quarterstaff, an herbalism kit, a bedroll, a book of philosophy, a lamp, 3 flasks of oil, traveler's clothes, and 16 gold",
            &[(405,1),(457,1),(439,1),(442,1),(422,1),(437,3),(428,1)], 16, 50),
        "Merchant" => (
            "navigator's tools, 2 pouches, traveler's clothes, and 22 gold",
            &[(454,1),(424,2),(428,1)], 22, 50),
        "Noble" => (
            "a gaming set, fine clothes, perfume, and 29 gold",
            &[(456,1),(429,1),(435,1)], 29, 50),
        "Sage" => (
            "a quarterstaff, calligrapher's supplies, a book of history, parchment, a robe, and 8 gold",
            &[(405,1),(445,1),(442,1),(431,1),(426,1)], 8, 50),
        "Sailor" => (
            "a dagger, navigator's tools, a coil of rope, traveler's clothes, and 20 gold",
            &[(120,1),(454,1),(443,1),(428,1)], 20, 50),
        "Scribe" => (
            "calligrapher's supplies, fine clothes, a lamp, 3 flasks of oil, parchment, and 23 gold",
            &[(445,1),(429,1),(422,1),(437,3),(431,1)], 23, 50),
        "Soldier" => (
            "a spear, a shortbow, 20 arrows, a gaming set, a healer's kit, a quiver, traveler's clothes, and 14 gold",
            &[(404,1),(413,1),(421,1),(456,1),(451,1),(425,1),(428,1)], 14, 50),
        "Wayfarer" => (
            "2 daggers, thieves' tools, a gaming set, a bedroll, 2 pouches, traveler's clothes, and 16 gold",
            &[(120,2),(452,1),(456,1),(439,1),(424,2),(428,1)], 16, 50),
        _ => return None,
    })
}

pub const NEWBIE_WEAPON_VNUM:  crate::world::ObjVnum = 99001;
pub const NEWBIE_ARMOR_VNUM:   crate::world::ObjVnum = 99002;
pub const NEWBIE_LIGHT_VNUM:   crate::world::ObjVnum = 99003;
pub const NEWBIE_BREAD_VNUM:   crate::world::ObjVnum = 99004;
/// Synthetic proto used by `cast brew`.  The cast spawns an instance
/// of this vnum and sets `obj.brewed_spell` per-instance.
pub const BREWED_POTION_VNUM:  crate::world::ObjVnum = 99005;
/// Synthetic proto used by `cast scribe`.  Each instance overrides
/// the bound spell via `ObjInstance.brewed_spell`.
pub const SCRIBED_SCROLL_VNUM: crate::world::ObjVnum = 99006;
/// Synthetic proto for a dropped pile of coins (cp223).  Each instance's
/// `ObjInstance.gold_amount` holds the actual amount.
pub const GOLD_PILE_VNUM:      crate::world::ObjVnum = 99007;
/// Insert four hardcoded synthetic prototypes so the first-login path
/// can spawn a class-aware newbie kit without depending on world-file
/// vnums.  Idempotent — guards against double-insertion.
fn inject_newbie_kit_protos(world: &mut crate::world::World) {
    use crate::world::*;
    if world.obj_protos.contains_key(&NEWBIE_WEAPON_VNUM) { return; }
    // 99001: worn club — ITEM_WEAPON, 1d4 bludgeon
    world.obj_protos.insert(NEWBIE_WEAPON_VNUM, ObjProto {
        vnum: NEWBIE_WEAPON_VNUM,
        name: "club worn".to_string(),
        short_description: "a worn club".to_string(),
        description: "A worn wooden club lies here.".to_string(),
        action_description: String::new(),
        item_type: ITEM_WEAPON,
        extra_flags: [0;4],
        wear_flags: [crate::character::ITEM_WEAR_TAKE | crate::character::ITEM_WEAR_WIELD, 0, 0, 0],
        affect_flags: [0;4],
        value: [0, 1, 4, 0],
        weight: 3, cost: 0, rent: 0, level: 0, timer: 0,
        extras: Vec::new(),
        affected: Vec::new(),
    });
    // 99002: tattered tunic — ITEM_ARMOR, AC 2, wear body
    world.obj_protos.insert(NEWBIE_ARMOR_VNUM, ObjProto {
        vnum: NEWBIE_ARMOR_VNUM,
        name: "tunic tattered".to_string(),
        short_description: "a tattered tunic".to_string(),
        description: "A tattered tunic lies here.".to_string(),
        action_description: String::new(),
        item_type: ITEM_ARMOR,
        extra_flags: [0;4],
        wear_flags: [crate::character::ITEM_WEAR_TAKE | crate::character::ITEM_WEAR_BODY, 0, 0, 0],
        affect_flags: [0;4],
        value: [2, 0, 0, 0],
        weight: 4, cost: 0, rent: 0, level: 0, timer: 0,
        extras: Vec::new(),
        affected: Vec::new(),
    });
    // 99003: brass lantern — ITEM_LIGHT, 24-hour
    world.obj_protos.insert(NEWBIE_LIGHT_VNUM, ObjProto {
        vnum: NEWBIE_LIGHT_VNUM,
        name: "lantern brass".to_string(),
        short_description: "a brass lantern".to_string(),
        description: "A small brass lantern is here.".to_string(),
        action_description: String::new(),
        item_type: ITEM_LIGHT,
        extra_flags: [0;4],
        wear_flags: [crate::character::ITEM_WEAR_TAKE, 0, 0, 0],
        affect_flags: [0;4],
        value: [0, 0, 24, 0],
        weight: 2, cost: 0, rent: 0, level: 0, timer: 0,
        extras: Vec::new(),
        affected: Vec::new(),
    });
    // 99004: small loaf of bread — ITEM_FOOD, value[0]=5 filling hours
    world.obj_protos.insert(NEWBIE_BREAD_VNUM, ObjProto {
        vnum: NEWBIE_BREAD_VNUM,
        name: "loaf bread small".to_string(),
        short_description: "a small loaf of bread".to_string(),
        description: "A small loaf of bread is here.".to_string(),
        action_description: String::new(),
        item_type: ITEM_FOOD,
        extra_flags: [0;4],
        wear_flags: [crate::character::ITEM_WEAR_TAKE, 0, 0, 0],
        affect_flags: [0;4],
        value: [5, 0, 0, 0],
        weight: 1, cost: 0, rent: 0, level: 0, timer: 0,
        extras: Vec::new(),
        affected: Vec::new(),
    });
    // 99005: generic brewed potion — each instance overrides which
    // spell it carries via `ObjInstance.brewed_spell`.
    world.obj_protos.insert(BREWED_POTION_VNUM, ObjProto {
        vnum: BREWED_POTION_VNUM,
        name: "potion vial brewed".to_string(),
        short_description: "a brewed potion".to_string(),
        description: "A small glass vial swirls with mystic vapor.".to_string(),
        action_description: String::new(),
        item_type: ITEM_POTION,
        extra_flags: [0;4],
        wear_flags: [crate::character::ITEM_WEAR_TAKE, 0, 0, 0],
        affect_flags: [0;4],
        value: [0, 0, 0, 0],
        weight: 1, cost: 0, rent: 0, level: 0, timer: 0,
        extras: Vec::new(),
        affected: Vec::new(),
    });
    // 99006: generic scribed scroll — each instance overrides its spell.
    world.obj_protos.insert(SCRIBED_SCROLL_VNUM, ObjProto {
        vnum: SCRIBED_SCROLL_VNUM,
        name: "scroll parchment scribed".to_string(),
        short_description: "a scribed scroll".to_string(),
        description: "A rolled parchment lies here, inked with mystic glyphs.".to_string(),
        action_description: String::new(),
        item_type: ITEM_SCROLL,
        extra_flags: [0;4],
        wear_flags: [crate::character::ITEM_WEAR_TAKE, 0, 0, 0],
        affect_flags: [0;4],
        value: [0, 0, 0, 0],
        weight: 1, cost: 0, rent: 0, level: 0, timer: 0,
        extras: Vec::new(),
        affected: Vec::new(),
    });
    // 99007: generic pile of coins — amount lives on the instance's
    // `gold_amount`; the keyword/short are overridden in obj_view (cp223).
    world.obj_protos.insert(GOLD_PILE_VNUM, ObjProto {
        vnum: GOLD_PILE_VNUM,
        name: "pile coins gold money".to_string(),
        short_description: "a pile of gold coins".to_string(),
        description: "A pile of gold coins is lying here.".to_string(),
        action_description: String::new(),
        item_type: ITEM_MONEY,
        extra_flags: [0;4],
        wear_flags: [crate::character::ITEM_WEAR_TAKE, 0, 0, 0],
        affect_flags: [0;4],
        value: [0, 0, 0, 0],
        weight: 0, cost: 0, rent: 0, level: 0, timer: 0,
        extras: Vec::new(),
        affected: Vec::new(),
    });
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

/// Global weather state (cp212).  `WEATHER_PRESSURE` is barometric
/// pressure in mmHg (~960..1040); `WEATHER_SKY` is the current sky:
/// 0 = cloudless, 1 = cloudy, 2 = raining, 3 = lightning.  Mirrors the
/// `weather_info` struct + `another_hour`/`weather_change` in weather.c.
pub static WEATHER_PRESSURE: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(960);
pub static WEATHER_CHANGE:   std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);
pub static WEATHER_SKY:      std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(1);   // start cloudy

pub const SKY_CLOUDLESS: i32 = 0;
pub const SKY_CLOUDY:    i32 = 1;
pub const SKY_RAINING:   i32 = 2;
pub const SKY_LIGHTNING: i32 = 3;

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

/// Map the current barometric pressure to a sky state, with band edges
/// chosen to give some hysteresis against flapping.  Lower pressure =
/// worse weather.  (cp212)
fn sky_for_pressure(pressure: i32) -> i32 {
    if pressure >= 1010 { SKY_CLOUDLESS }
    else if pressure >= 990 { SKY_CLOUDY }
    else if pressure >= 970 { SKY_RAINING }
    else { SKY_LIGHTNING }
}

/// Background weather simulation (cp212).  Every game-hour (75s) it walks
/// the barometric pressure (seasonally biased, like `weather_change` in
/// weather.c), recomputes the sky band, and — on an actual sky change —
/// broadcasts an ambient line to every player standing in an outdoor room
/// (sector other than INSIDE / CITY).
pub fn spawn_weather_tick(
    world: std::sync::Arc<tokio::sync::Mutex<World>>,
    chars: crate::character::SharedChars,
) {
    const HOUR_REAL_SECS: u64 = 75;
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(HOUR_REAL_SECS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        interval.tick().await;
        loop {
            interval.tick().await;
            // --- Pressure random-walk, seasonally biased. ---
            let month = GAME_MONTH.load(Ordering::Relaxed);
            let mut pressure = WEATHER_PRESSURE.load(Ordering::Relaxed);
            let diff = if (9..=16).contains(&month) {
                if pressure > 985 { -2 } else { 2 }
            } else if pressure > 1015 { -2 } else { 2 };
            let mut change = WEATHER_CHANGE.load(Ordering::Relaxed);
            change += dice(1, 4) * diff + dice(2, 6) - dice(2, 6);
            change = change.clamp(-12, 12);
            pressure = (pressure + change).clamp(960, 1040);
            WEATHER_CHANGE.store(change, Ordering::Relaxed);
            WEATHER_PRESSURE.store(pressure, Ordering::Relaxed);

            let old_sky = WEATHER_SKY.load(Ordering::Relaxed);
            let new_sky = sky_for_pressure(pressure);
            if new_sky == old_sky { continue; }
            WEATHER_SKY.store(new_sky, Ordering::Relaxed);

            // Ambient message keyed off the direction of change.
            let msg = match (old_sky, new_sky) {
                (_, SKY_LIGHTNING) => "Lightning flashes and thunder rumbles overhead.\r\n",
                (_, SKY_RAINING) if new_sky > old_sky => "It begins to rain.\r\n",
                (SKY_LIGHTNING, SKY_RAINING) => "The lightning subsides, but the rain continues.\r\n",
                (_, SKY_CLOUDY) if new_sky > old_sky => "The sky clouds over.\r\n",
                (SKY_RAINING, SKY_CLOUDY) => "The rain stops, leaving a grey overcast.\r\n",
                (_, SKY_CLOUDLESS) => "The clouds part and the sky clears.\r\n",
                _ => "The weather shifts.\r\n",
            };

            // Broadcast to players standing outdoors.
            let handles: Vec<crate::character::PlayerHandle> = {
                let cl = chars.lock().await;
                cl.iter().cloned().collect()
            };
            if handles.is_empty() { continue; }
            let w = world.lock().await;
            for ph in &handles {
                let outdoor = w.rooms.get(&ph.current_room)
                    .map(|r| r.sector_type != crate::world::SECT_INSIDE
                          && r.sector_type != crate::world::SECT_CITY)
                    .unwrap_or(false);
                if outdoor {
                    let _ = ph.send.send(format!("\r\n{msg}"));
                }
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

/// Crash-safe house-save tick.  Every 5 minutes, walks every
/// ROOM_HOUSE room and persists its floor contents.
pub fn spawn_house_save_tick(
    world: std::sync::Arc<tokio::sync::Mutex<crate::world::World>>,
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
            let data_dir = players.lock().await.data_dir().to_string();
            let w = world.lock().await;
            let house_rooms: Vec<i32> = w.rooms.iter()
                .filter(|(_, r)| r.room_flags[0] & crate::world::ROOM_HOUSE != 0)
                .map(|(v, _)| *v).collect();
            for rv in &house_rooms {
                save_house(&data_dir, *rv, &w);
            }
            if !house_rooms.is_empty() {
                tracing::info!(houses = house_rooms.len(), "Periodic house save");
            }
        }
    });
}

/// Random ambient encounter spawner.  Every TICK_SECONDS the tick
/// walks the world looking for an outdoor empty room (FIELD/FOREST/
/// HILLS sector, zero mobs, no `ROOM_PEACEFUL`/`ROOM_NOMOB`/`ROOM_HOUSE`
/// flags) and, with 30% chance, spawns a random low-level (≤5)
/// mob_proto into it.  No-op if no eligible (room, proto) pair exists.
pub fn spawn_random_encounter_tick(
    world: std::sync::Arc<tokio::sync::Mutex<crate::world::World>>,
) {
    const TICK_SECONDS: u64 = 300;
    tokio::spawn(async move {
        use rand::Rng;
        use rand::seq::SliceRandom;
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(TICK_SECONDS)
        );
        interval.set_missed_tick_behavior(
            tokio::time::MissedTickBehavior::Skip
        );
        interval.tick().await;
        loop {
            interval.tick().await;
            if rand::thread_rng().gen_range(0..100) >= 30 { continue; }
            let mut w = world.lock().await;
            // Build eligible-room list.
            use crate::world::*;
            let elig_rooms: Vec<RoomVnum> = w.rooms.iter()
                .filter(|(_, r)| {
                    let bad = ROOM_PEACEFUL | ROOM_NOMOB | ROOM_HOUSE | ROOM_GODROOM;
                    if r.room_flags[0] & bad != 0 { return false; }
                    if !r.mobs.is_empty() { return false; }
                    matches!(r.sector_type, SECT_FIELD | SECT_FOREST | SECT_HILLS)
                })
                .map(|(v, _)| *v)
                .collect();
            if elig_rooms.is_empty() { continue; }
            // Eligible mob_proto vnums (level 1-5, has a name).
            let elig_protos: Vec<MobVnum> = w.mob_protos.iter()
                .filter(|(_, p)| p.level > 0 && p.level <= 5 && !p.short_descr.is_empty())
                .map(|(v, _)| *v)
                .collect();
            if elig_protos.is_empty() { continue; }
            let room  = *elig_rooms.choose(&mut rand::thread_rng()).unwrap();
            let proto = *elig_protos.choose(&mut rand::thread_rng()).unwrap();
            if w.spawn_mob(proto, room).is_some() {
                tracing::debug!(
                    room, proto, "Random encounter spawned",
                );
            }
        }
    });
}

/// Out-of-combat mob HP regen.  Every TICK_SECONDS, walks every live
/// mob; for any not currently fighting AND below max_hp, restores a
/// chunk (~10% of max_hp, min 1).  Mobs with the Poison affect skip
/// regen — the combat-tick Phase 0 DoT loop handles them.  Keeps
/// partially-bloodied mobs from staying weak forever.
pub fn spawn_mob_regen_tick(
    world: std::sync::Arc<tokio::sync::Mutex<crate::world::World>>,
) {
    const TICK_SECONDS: u64 = 30;
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
            let mut w = world.lock().await;
            let mut healed = 0usize;
            for m in w.mob_instances.iter_mut() {
                if m.fighting.is_some() { continue; }
                if m.hp >= m.max_hp { continue; }
                if m.affects.iter().any(|a|
                    a.skill == crate::character::Skill::Poison
                ) { continue; }
                let gain = (m.max_hp / 10).max(1);
                m.hp = (m.hp + gain).min(m.max_hp);
                healed += 1;
            }
            if healed > 0 {
                tracing::debug!(healed, "Mob regen tick");
            }
        }
    });
}

/// Burn down lit ITEM_LIGHT sources (cp207).  Runs every game-hour (75s);
/// each lit light with `light_hours > 0` loses one hour and, on reaching
/// zero, is extinguished (`light_lit = false`, `light_hours = -1` so it
/// can't be relit).  The holder (or the room, for a floor light) is told.
pub fn spawn_light_burn_tick(
    world: std::sync::Arc<tokio::sync::Mutex<crate::world::World>>,
    chars: crate::character::SharedChars,
) {
    const TICK_SECONDS: u64 = 75;   // one game-hour, matches the clock tick
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
            // Phase A: decrement fuel; collect the ones that just died as
            // (iid, in_room, short_descr).
            let mut burned_out: Vec<(u32, crate::world::RoomVnum, String)> = Vec::new();
            {
                let mut w = world.lock().await;
                // Snapshot short descriptions to avoid a borrow conflict.
                let mut expired_ids: Vec<(u32, crate::world::RoomVnum)> = Vec::new();
                for o in w.obj_instances.iter_mut() {
                    if !o.light_lit || o.light_hours <= 0 { continue; }
                    o.light_hours -= 1;
                    if o.light_hours <= 0 {
                        o.light_hours = -1;
                        o.light_lit = false;
                        expired_ids.push((o.id, o.in_room));
                    }
                }
                for (iid, room) in expired_ids {
                    let short = w.obj_instances.iter()
                        .find(|o| o.id == iid)
                        .and_then(|o| w.obj_protos.get(&o.vnum))
                        .map(|p| p.short_description.clone())
                        .unwrap_or_else(|| "a light".to_string());
                    burned_out.push((iid, room, short));
                }
            }
            if burned_out.is_empty() { continue; }

            // Phase B: notify.  Floor lights broadcast to their room; a
            // carried/equipped light (in_room == NOWHERE) is traced to its
            // holder via the registry.
            for (iid, room, short) in burned_out {
                if room != crate::world::NOWHERE {
                    chars.lock().await.broadcast_room(
                        room, None,
                        &format!("{short} flickers and goes out.\r\n"),
                    );
                    continue;
                }
                // Carried/equipped: find the player holding this iid.
                let handles: Vec<crate::character::PlayerHandle> = {
                    let cl = chars.lock().await;
                    cl.iter().cloned().collect()
                };
                for ph in handles {
                    let holds = {
                        let c = ph.character.lock().await;
                        c.inventory.contains(&iid)
                            || c.equipment.iter().any(|s| *s == Some(iid))
                    };
                    if holds {
                        let _ = ph.send.send(format!(
                            "\r\n{short} flickers and goes out, leaving you in the dark.\r\n"
                        ));
                        break;
                    }
                }
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
                // Sober up one notch per tick (cp208).
                let mut sober_msg: Option<&'static str> = None;
                if c.drunk > 0 {
                    let was_slurring = c.drunk >= crate::interpreter::DRUNK_SLUR_THRESHOLD;
                    c.drunk -= 1;
                    if was_slurring && c.drunk < crate::interpreter::DRUNK_SLUR_THRESHOLD {
                        sober_msg = Some("Your head begins to clear.\r\n");
                    } else if c.drunk == 0 {
                        sober_msg = Some("You feel completely sober again.\r\n");
                    }
                }
                drop(c);
                if let Some(m) = hunger_msg { let _ = ph.send.send(m.to_string()); }
                if let Some(m) = thirst_msg { let _ = ph.send.send(m.to_string()); }
                if let Some(m) = sober_msg  { let _ = ph.send.send(m.to_string()); }
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
                    crate::world::MobSpec::MagicUser => {
                        // MagicUser's combat-cast logic lives in
                        // combat.rs (resolve_mob_attack); no idle tick.
                    }
                    crate::world::MobSpec::Receptionist
                    | crate::world::MobSpec::Cryogenicist => {
                        // The offer/rent commands drive the real behavior.
                        // Idle flavor only: ~1-in-6 chance of a small social,
                        // mirroring gen_receptionist's `!cmd` action table.
                        const RECEP_ACTS: &[&str] = &[
                            "smiles", "sighs", "blushes", "coughs",
                            "twiddles $s thumbs", "yawns",
                        ];
                        let act = {
                            use rand::Rng;
                            let mut rng = rand::thread_rng();
                            if rng.gen_range(0..6) == 0 {
                                RECEP_ACTS.choose(&mut rng).copied()
                            } else { None }
                        };
                        if let Some(a) = act {
                            let a = a.replace("$s", "its");
                            chars.lock().await.broadcast_room(
                                room, None, &format!("{short} {a}.\r\n"));
                        }
                    }
                    crate::world::MobSpec::Healer => {
                        // Healer service is invoked by the `heal`
                        // player command (cp180); no idle tick.
                    }
                    crate::world::MobSpec::PetShop => {
                        // Pet shop is invoked by the `petlist`/`petbuy`
                        // player commands; no idle tick behavior.
                    }
                    crate::world::MobSpec::Postmaster => {
                        // 20% chance per tick to ping anyone in the
                        // room who has mail waiting.
                        let roll = {
                            use rand::Rng;
                            rand::thread_rng().gen_range(0..100)
                        };
                        if roll >= 20 { continue; }
                        // Snapshot players in this room.
                        let players: Vec<(u32, String)> = {
                            let cl = chars.lock().await;
                            cl.iter()
                                .filter(|p| p.current_room == room)
                                .map(|p| (p.id, p.name.clone()))
                                .collect()
                        };
                        if players.is_empty() { continue; }
                        let Some(players_arc) = crate::interpreter::PLAYERS_HANDLE.get() else { continue; };
                        let data_dir = players_arc.lock().await.data_dir().to_string();
                        for (_id, pname) in &players {
                            let n = crate::mail::load_mailbox(&data_dir, pname).len();
                            if n == 0 { continue; }
                            chars.lock().await.broadcast_room(
                                room, None,
                                &format!(
                                    "{short} says, 'You have {n} letter(s), {pname}!'\r\n"
                                ),
                            );
                        }
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
                    crate::world::MobSpec::Thief => {
                        use rand::Rng;
                        use rand::seq::SliceRandom;
                        // Don't pickpocket while in combat.
                        let busy = {
                            let w = world.lock().await;
                            w.mob_instances.iter()
                                .find(|m| m.id == mob_id)
                                .map(|m| m.fighting.is_some())
                                .unwrap_or(true)
                        };
                        if busy { continue; }
                        // 25% chance per tick to attempt a lift.
                        if rand::thread_rng().gen_range(0..100) >= 25 { continue; }
                        // Snapshot mortal players sharing the room.
                        let victims: Vec<crate::character::PlayerHandle> = {
                            let cl = chars.lock().await;
                            cl.iter()
                                .filter(|p| p.current_room == room && p.level < 34)
                                .cloned()
                                .collect()
                        };
                        if victims.is_empty() { continue; }
                        let victim = {
                            let mut rng = rand::thread_rng();
                            victims.choose(&mut rng).cloned()
                        };
                        let Some(victim) = victim else { continue; };
                        // Lift up to ~25% of the victim's purse.
                        let stolen = {
                            let mut c = victim.character.lock().await;
                            if c.gold <= 0 { 0 } else {
                                let amt = (c.gold / 4).max(1).min(c.gold);
                                c.gold -= amt;
                                amt
                            }
                        };
                        if stolen <= 0 { continue; }
                        // The thief pockets the coins (no per-instance gold
                        // store yet — the gold simply leaves the game).
                        let _ = victim.send.send(format!(
                            "\r\n{short} brushes past you — your purse feels lighter! \
                             ({stolen} gold gone)\r\n"
                        ));
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
/// Path to a directory's index file.  In mini-MUD mode (`-m`) we prefer
/// `index.mini` (the minimal/test list) when it exists, falling back to
/// the full `index` otherwise.  Mirrors MINDEX_FILE handling in db.c.
fn index_path(dir: &str, mini: bool) -> String {
    if mini {
        let mini_path = format!("{dir}/index.mini");
        if std::path::Path::new(&mini_path).exists() {
            return mini_path;
        }
    }
    format!("{dir}/index")
}

fn read_index(dir: &str, mini: bool) -> Result<Vec<String>> {
    let path = index_path(dir, mini);
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
                        equipment: Default::default(),
                        bonus_damroll: 0, bonus_hitroll: 0, bonus_ac: 0,
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
            light_hours: 0,
            gold_amount: 0,
                        condition: 100,
                        brewed_spell: None,
                        bonus_affects: Vec::new(),
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
            light_hours: 0,
            gold_amount: 0,
                        condition: 100,
                        brewed_spell: None,
                        bonus_affects: Vec::new(),
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
                // For E, cmd.arg3 is the wear position (WEAR_* index from
                // structs.h).  If valid and the slot is free, put it
                // there; otherwise fall through to inventory.
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
            light_hours: 0,
            gold_amount: 0,
                    condition: 100,
                    brewed_spell: None,
                    bonus_affects: Vec::new(),
                });
                let equipped = if cmd.command == 'E' {
                    let pos = cmd.arg3 as usize;
                    if pos < crate::character::NUM_WEARS {
                        if let Some(m) = world.mob_instances.iter_mut().find(|m| m.id == mob_id) {
                            if m.equipment[pos].is_none() {
                                m.equipment[pos] = Some(id);
                                true
                            } else { false }
                        } else { false }
                    } else { false }
                } else { false };
                if !equipped {
                    if let Some(m) = world.mob_instances.iter_mut().find(|m| m.id == mob_id) {
                        m.inventory.push(id);
                    }
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
            light_hours: 0,
            gold_amount: 0,
                        condition: 100,
                        brewed_spell: None,
                        bonus_affects: Vec::new(),
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

    // Auto-equip pass: any mob in this zone with empty equipment slots
    // and wearable items in its inventory now picks them up.  Iterates
    // a snapshot of ids since `auto_equip_mob` mutates `world`.
    let mob_ids: Vec<u32> = world.mob_instances.iter()
        .filter_map(|m| {
            world.rooms.get(&m.in_room)
                .map(|r| r.zone)
                .filter(|&z| z == zone_vnum)
                .map(|_| m.id)
        })
        .collect();
    for id in mob_ids {
        world.auto_equip_mob(id);
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
