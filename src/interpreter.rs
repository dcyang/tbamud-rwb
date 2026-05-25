/// Command interpreter — the Rust counterpart to interpreter.c's
/// `command_interpreter()` + `cmd_info[]`.
///
/// All gameplay commands route through `dispatch_command()`. Adding a new
/// command means adding it to `COMMANDS` and writing the matching arm in
/// the `match` block.
///
/// Abbreviation matching mirrors C: walk the table in *priority order* and
/// pick the first command whose canonical name starts with the typed prefix.
/// Single-letter aliases (`l`, `n`, …) come first so they win over longer
/// commands that share the prefix.

use std::sync::Arc;

use tokio::sync::Mutex;

use rand::seq::SliceRandom;

use crate::{
    character::{
        auto_wear_slot, wear_pos_name,
        Character, CharacterList, SharedChars, Target,
        ITEM_WEAR_WIELD, NUM_WEARS, WEAR_WIELD,
    },
    players::PlayerDb,
    world::{Direction, ObjVnum, RoomVnum, World},
};

// ---------------------------------------------------------------------------
// Command table
// ---------------------------------------------------------------------------

/// Canonical command names, in priority order for abbreviation matching.
/// Mirrors the sort order of cmd_info[] in interpreter.c.
const COMMANDS: &[&str] = &[
    // Movement — short aliases first so "n" matches "north" not "news".
    "north", "east", "south", "west", "up", "down",
    // Common short verbs
    "look", "inventory", "kill", "flee",
    "get", "drop", "wield", "wear", "remove",
    "say", "tell", "who",
    "score", "equipment", "save", "help",
    // Single-letter aliases not handled by prefix
    "exits", "quit", "hit",
];

/// Resolve a typed verb to a canonical command name via prefix match.
/// Returns None if no command matches.
fn resolve_command(verb: &str) -> Option<&'static str> {
    if verb.is_empty() { return None; }
    let lower = verb.to_ascii_lowercase();
    COMMANDS.iter().copied().find(|c| c.starts_with(lower.as_str()))
}

// ---------------------------------------------------------------------------
// Command-dispatch result
// ---------------------------------------------------------------------------

/// What the interpreter wants the connection task to do after a command.
pub struct CmdOutput {
    /// Text to send to the actor's socket (already CRLF-formatted; the
    /// caller appends the prompt).
    pub text: String,
    /// True if the player wants to log off.
    pub quit: bool,
}

impl CmdOutput {
    fn text(s: impl Into<String>) -> Self { Self { text: s.into(), quit: false } }
    fn quit(s: impl Into<String>)  -> Self { Self { text: s.into(), quit: true } }
}

// ---------------------------------------------------------------------------
// Dispatch entry point
// ---------------------------------------------------------------------------

pub async fn dispatch_command(
    raw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    let raw = raw.trim();
    if raw.is_empty() {
        return CmdOutput::text(String::new());
    }

    let (verb, rest) = match raw.find(char::is_whitespace) {
        Some(i) => (&raw[..i], raw[i..].trim_start()),
        None    => (raw, ""),
    };

    // Movement is special — accept any prefix of n/e/s/w/u/d as well as
    // longer compass words.
    if let Some(dir) = Direction::parse(verb) {
        return do_move(dir, me, world, chars).await;
    }

    let canon = resolve_command(verb);
    match canon {
        Some("look")      => do_look(rest, me, world, chars).await,
        Some("inventory") => do_inventory(me, world).await,
        Some("get")       => do_get(rest, me, world, chars).await,
        Some("drop")      => do_drop(rest, me, world, chars).await,
        Some("say")       => do_say(rest, me, chars).await,
        Some("tell")      => do_tell(rest, me, chars).await,
        Some("who")       => do_who(me, chars).await,
        Some("score")     => do_score(me),
        Some("kill") | Some("hit") => do_kill(rest, me, world, chars).await,
        Some("flee")      => do_flee(me, world, chars).await,
        Some("wield")     => do_wield(rest, me, world).await,
        Some("wear")      => do_wear(rest, me, world).await,
        Some("remove")    => do_remove(rest, me, world).await,
        Some("equipment") => do_equipment(me, world).await,
        Some("save")      => do_save(me, players).await,
        Some("help")      => CmdOutput::text("\r\nAvailable: look, get, drop, inv, wield, wear, remove, equip, kill, flee, say, tell, who, score, save, quit, n/e/s/w/u/d.\r\n"),
        Some("exits")     => do_exits(me, world).await,
        Some("quit")      => CmdOutput::quit("Goodbye.\r\n"),
        Some("north") | Some("east") | Some("south") |
        Some("west")  | Some("up")   | Some("down")   => {
            // Already handled by Direction::parse above, but just in case
            // someone typed the full word, route here too.
            if let Some(d) = Direction::parse(canon.unwrap()) {
                return do_move(d, me, world, chars).await;
            }
            CmdOutput::text("\r\nHuh?!\r\n")
        }
        _ => CmdOutput::text(format!("\r\nHuh?!? ({raw})\r\n")),
    }
}

// ---------------------------------------------------------------------------
// Individual commands
// ---------------------------------------------------------------------------

async fn do_look(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text(render_room(me.current_room, Some(me.id), world, chars).await);
    }
    // look <keyword>: search obj in inventory, then obj in room, then extras
    let w = world.lock().await;
    let key = arg.to_ascii_lowercase();

    // Inventory
    for &iid in &me.inventory {
        if let Some(obj) = find_obj_by_id(&w, iid) {
            if obj_keyword_matches(&w, obj.vnum, &key) {
                if let Some(p) = w.obj_protos.get(&obj.vnum) {
                    let body = if p.action_description.is_empty() {
                        &p.short_description
                    } else {
                        &p.action_description
                    };
                    return CmdOutput::text(format!("\r\n{}\r\n", body));
                }
            }
        }
    }

    // Room objects
    if let Some(r) = w.rooms.get(&me.current_room) {
        for &iid in &r.objects {
            if let Some(obj) = find_obj_by_id(&w, iid) {
                if obj_keyword_matches(&w, obj.vnum, &key) {
                    if let Some(p) = w.obj_protos.get(&obj.vnum) {
                        let body = if p.action_description.is_empty() {
                            &p.short_description
                        } else {
                            &p.action_description
                        };
                        return CmdOutput::text(format!("\r\n{}\r\n", body));
                    }
                }
            }
        }
        // Room extras
        for e in &r.extras {
            if e.keyword.split_whitespace().any(|w| w.eq_ignore_ascii_case(&key)) {
                return CmdOutput::text(format!("\r\n{}\r\n", e.description));
            }
        }
        // Mobs in room
        for &mid in &r.mobs {
            if let Some(m) = w.mob_instances.iter().find(|m| m.id == mid) {
                if let Some(mp) = w.mob_protos.get(&m.vnum) {
                    if mp.name.split_whitespace().any(|w| w.eq_ignore_ascii_case(&key)) {
                        let body = if mp.description.is_empty() {
                            format!("You see nothing special about {}.", mp.short_descr)
                        } else {
                            mp.description.clone()
                        };
                        return CmdOutput::text(format!("\r\n{}\r\n", body));
                    }
                }
            }
        }
    }
    drop(w);

    // Other players in room
    let cl = chars.lock().await;
    if let Some(other) = cl.iter().find(|p| {
        p.current_room == me.current_room && p.id != me.id
            && p.name.to_ascii_lowercase() == key
    }) {
        return CmdOutput::text(format!("\r\nYou see {}, a player.\r\n", other.name));
    }

    CmdOutput::text("\r\nYou do not see that here.\r\n".to_string())
}

async fn do_inventory(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    if me.inventory.is_empty() {
        return CmdOutput::text("\r\nYou are not carrying anything.\r\n");
    }
    let w = world.lock().await;
    let mut s = String::from("\r\nYou are carrying:\r\n");
    for &iid in &me.inventory {
        if let Some(obj) = find_obj_by_id(&w, iid) {
            if let Some(p) = w.obj_protos.get(&obj.vnum) {
                s.push_str(" ");
                s.push_str(&p.short_description);
                s.push_str("\r\n");
            }
        }
    }
    CmdOutput::text(s)
}

async fn do_get(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nGet what?\r\n");
    }
    let key = arg.to_ascii_lowercase();
    let mut w = world.lock().await;

    let (iid, name) = {
        let r = match w.rooms.get(&me.current_room) {
            Some(r) => r,
            None => return CmdOutput::text("\r\nYou are nowhere.\r\n"),
        };
        // Scan room objects for first keyword match.
        let mut found: Option<(u32, String)> = None;
        for &iid in &r.objects {
            if let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) {
                if let Some(p) = w.obj_protos.get(&obj.vnum) {
                    if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                        found = Some((iid, p.short_description.clone()));
                        break;
                    }
                }
            }
        }
        match found {
            Some(f) => f,
            None => return CmdOutput::text(format!("\r\nYou see no {key} here.\r\n")),
        }
    };

    // Mutate world: remove from room, add to player's inventory list,
    // update the instance's in_room.
    if let Some(r) = w.rooms.get_mut(&me.current_room) {
        r.objects.retain(|&i| i != iid);
    }
    if let Some(obj) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
        obj.in_room = crate::world::NOWHERE;
    }
    me.inventory.push(iid);
    drop(w);

    // Notify others in the room
    let cl = chars.lock().await;
    cl.broadcast_room(
        me.current_room, Some(me.id),
        &format!("{} picks up {}.\r\n", me.name, name),
    );

    CmdOutput::text(format!("\r\nYou get {}.\r\n", name))
}

async fn do_drop(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nDrop what?\r\n");
    }
    let key = arg.to_ascii_lowercase();
    let mut w = world.lock().await;

    // Find matching inventory item
    let (idx, iid, name) = {
        let mut found = None;
        for (i, &iid) in me.inventory.iter().enumerate() {
            if let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) {
                if let Some(p) = w.obj_protos.get(&obj.vnum) {
                    if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                        found = Some((i, iid, p.short_description.clone()));
                        break;
                    }
                }
            }
        }
        match found {
            Some(f) => f,
            None => return CmdOutput::text(format!("\r\nYou do not have a {key}.\r\n")),
        }
    };

    me.inventory.remove(idx);
    if let Some(obj) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
        obj.in_room = me.current_room;
    }
    if let Some(r) = w.rooms.get_mut(&me.current_room) {
        r.objects.push(iid);
    }
    drop(w);

    let cl = chars.lock().await;
    cl.broadcast_room(
        me.current_room, Some(me.id),
        &format!("{} drops {}.\r\n", me.name, name),
    );

    CmdOutput::text(format!("\r\nYou drop {}.\r\n", name))
}

async fn do_say(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nYak yak yak...\r\n");
    }
    let cl = chars.lock().await;
    cl.broadcast_room(
        me.current_room, Some(me.id),
        &format!("{} says, '{arg}'\r\n", me.name),
    );
    CmdOutput::text(format!("\r\nYou say, '{arg}'\r\n"))
}

async fn do_tell(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    let (target, msg) = match arg.find(char::is_whitespace) {
        Some(i) => (&arg[..i], arg[i..].trim_start()),
        None    => return CmdOutput::text("\r\nTell whom what?\r\n"),
    };
    if msg.is_empty() {
        return CmdOutput::text("\r\nTell them what?\r\n");
    }
    let cl = chars.lock().await;
    match cl.find_by_name(target) {
        Some(p) if p.id != me.id => {
            let _ = p.send.send(format!("{} tells you, '{msg}'\r\n", me.name));
            CmdOutput::text(format!("\r\nYou tell {}, '{msg}'\r\n", p.name))
        }
        _ => CmdOutput::text("\r\nNo one by that name is online.\r\n"),
    }
}

async fn do_who(me: &Character, chars: &SharedChars) -> CmdOutput {
    let cl = chars.lock().await;
    let mut s = String::from("\r\nPlayers online:\r\n");
    let mut count = 0;
    for p in cl.iter() {
        let marker = if p.id == me.id { " (you)" } else { "" };
        s.push_str(&format!("  [{:>2}] {}{}\r\n", p.level, p.name, marker));
        count += 1;
    }
    s.push_str(&format!("\r\n{count} player(s) online.\r\n"));
    CmdOutput::text(s)
}

fn do_score(me: &Character) -> CmdOutput {
    let s = format!(
        "\r\nName:  {}\r\nLevel: {}\r\nHP:    {}/{}\r\nClass: {:?}\r\nSex:   {:?}\r\nGold:  {}\r\nRoom:  {}\r\n",
        me.name, me.level, me.hp, me.max_hp, me.class, me.sex, me.gold, me.current_room,
    );
    CmdOutput::text(s)
}

async fn do_kill(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nKill whom?\r\n");
    }
    if me.fighting.is_some() {
        return CmdOutput::text("\r\nYou are already fighting!\r\n");
    }
    let key = arg.to_ascii_lowercase();
    let mut w = world.lock().await;

    // Find a mob in the current room whose proto.name keyword matches.
    let mob_id = {
        let r = match w.rooms.get(&me.current_room) {
            Some(r) => r,
            None => return CmdOutput::text("\r\nYou are nowhere.\r\n"),
        };
        let mut found: Option<u32> = None;
        for &mid in &r.mobs {
            if let Some(m) = w.mob_instances.iter().find(|m| m.id == mid) {
                if let Some(mp) = w.mob_protos.get(&m.vnum) {
                    if mp.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                        found = Some(mid);
                        break;
                    }
                }
            }
        }
        match found {
            Some(id) => id,
            None => return CmdOutput::text(format!("\r\nYou see no {key} here to attack.\r\n")),
        }
    };

    let mob_name = w.mob_instances.iter()
        .find(|m| m.id == mob_id)
        .and_then(|m| w.mob_protos.get(&m.vnum).map(|p| p.short_descr.clone()))
        .unwrap_or_else(|| "the creature".into());

    // Mutual fighting state.
    me.fighting = Some(Target { id: mob_id, is_player: false });
    if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
        if m.fighting.is_none() {
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
    }
    drop(w);

    let cl = chars.lock().await;
    cl.broadcast_room(
        me.current_room, Some(me.id),
        &format!("{} attacks {mob_name}!\r\n", me.name),
    );

    CmdOutput::text(format!("\r\nYou attack {mob_name}!\r\n"))
}

async fn do_flee(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.fighting.is_none() {
        return CmdOutput::text("\r\nYou are not fighting anyone.\r\n");
    }
    // Pick a random valid exit.
    let target = {
        let w = world.lock().await;
        let r = match w.rooms.get(&me.current_room) {
            Some(r) => r,
            None => return CmdOutput::text("\r\nYou are nowhere.\r\n"),
        };
        let candidates: Vec<(Direction, RoomVnum)> = Direction::ALL.iter()
            .filter_map(|d| {
                r.exits[*d as usize].as_ref().and_then(|e| {
                    if e.to_room != crate::world::NOWHERE && w.rooms.contains_key(&e.to_room) {
                        Some((*d, e.to_room))
                    } else { None }
                })
            })
            .collect();
        candidates.choose(&mut rand::thread_rng()).copied()
    };

    let Some((dir, to)) = target else {
        return CmdOutput::text("\r\nPANIC!  You couldn't escape!\r\n");
    };

    let from = me.current_room;
    me.current_room = to;
    me.fighting     = None;
    // Detach the mob's pointer too.
    {
        let mut w = world.lock().await;
        for m in w.mob_instances.iter_mut() {
            if m.fighting.map(|t| t.is_player && t.id == me.id).unwrap_or(false) {
                m.fighting = None;
            }
        }
    }

    {
        let mut cl = chars.lock().await;
        cl.update_room(me.id, to);
        cl.broadcast_room(from, Some(me.id),
            &format!("{} flees {}!\r\n", me.name, dir.name()));
        cl.broadcast_room(to,   Some(me.id),
            &format!("{} arrives in a panicked rush.\r\n", me.name));
    }

    let view = render_room(to, Some(me.id), world, chars).await;
    CmdOutput::text(format!("\r\nYou flee {}!\r\n{view}", dir.name()))
}

async fn do_exits(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let w = world.lock().await;
    let r = match w.rooms.get(&me.current_room) {
        Some(r) => r,
        None    => return CmdOutput::text("\r\nYou are nowhere.\r\n"),
    };
    let mut s = String::from("\r\nObvious exits:\r\n");
    let mut any = false;
    for d in Direction::ALL {
        if let Some(e) = &r.exits[d as usize] {
            if e.to_room == crate::world::NOWHERE { continue; }
            any = true;
            let to_name = w.rooms.get(&e.to_room)
                .map(|r| r.name.as_str())
                .unwrap_or("(nowhere)");
            s.push_str(&format!("  {:<5} - {}\r\n", d.name(), to_name));
        }
    }
    if !any {
        s.push_str(" None.\r\n");
    }
    CmdOutput::text(s)
}

// ---------------------------------------------------------------------------
// Equipment commands
// ---------------------------------------------------------------------------

async fn do_wield(arg: &str, me: &mut Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nWield what?\r\n");
    }
    let w = world.lock().await;
    let key = arg.to_ascii_lowercase();

    let (idx, iid, short) = match find_inv_match(&w, &me.inventory, &key) {
        Some(t) => t,
        None => return CmdOutput::text(format!("\r\nYou do not have a {key}.\r\n")),
    };

    // Item must have ITEM_WEAR_WIELD bit set.
    let wear_flags = w.obj_protos.iter()
        .find(|(_, p)| w.obj_instances.iter().any(|o| o.id == iid && o.vnum == p.vnum))
        .map(|(_, p)| p.wear_flags[0])
        .unwrap_or(0);
    drop(w);

    if wear_flags & ITEM_WEAR_WIELD == 0 {
        return CmdOutput::text(format!("\r\nYou cannot wield {short}.\r\n"));
    }
    if me.equipment[WEAR_WIELD].is_some() {
        return CmdOutput::text("\r\nYou are already wielding something.\r\n");
    }

    me.inventory.remove(idx);
    me.equipment[WEAR_WIELD] = Some(iid);
    CmdOutput::text(format!("\r\nYou wield {short}.\r\n"))
}

async fn do_wear(arg: &str, me: &mut Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nWear what?\r\n");
    }
    let w = world.lock().await;
    let key = arg.to_ascii_lowercase();

    let (idx, iid, short) = match find_inv_match(&w, &me.inventory, &key) {
        Some(t) => t,
        None => return CmdOutput::text(format!("\r\nYou do not have a {key}.\r\n")),
    };

    // Look up the object's wear flags.
    let wear_flags = {
        let obj = w.obj_instances.iter().find(|o| o.id == iid);
        obj.and_then(|o| w.obj_protos.get(&o.vnum))
            .map(|p| p.wear_flags[0])
            .unwrap_or(0)
    };
    drop(w);

    let slot = match auto_wear_slot(wear_flags) {
        Some(s) => s,
        None => return CmdOutput::text(format!("\r\nYou cannot wear {short}.\r\n")),
    };

    if me.equipment[slot].is_some() {
        return CmdOutput::text(format!(
            "\r\nYou are already wearing something {}.\r\n",
            wear_pos_name(slot)
        ));
    }

    me.inventory.remove(idx);
    me.equipment[slot] = Some(iid);
    CmdOutput::text(format!("\r\nYou wear {short} {}.\r\n", wear_pos_name(slot)))
}

async fn do_remove(arg: &str, me: &mut Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nRemove what?\r\n");
    }
    let w = world.lock().await;
    let key = arg.to_ascii_lowercase();

    // Find a worn item matching the keyword.
    let found = (0..NUM_WEARS).find_map(|i| {
        let iid = me.equipment[i]?;
        let obj = w.obj_instances.iter().find(|o| o.id == iid)?;
        let p   = w.obj_protos.get(&obj.vnum)?;
        if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
            Some((i, iid, p.short_description.clone()))
        } else {
            None
        }
    });
    drop(w);

    let (slot, iid, short) = match found {
        Some(t) => t,
        None => return CmdOutput::text(format!("\r\nYou are not wearing a {key}.\r\n")),
    };

    me.equipment[slot] = None;
    me.inventory.push(iid);
    CmdOutput::text(format!("\r\nYou stop using {short}.\r\n"))
}

async fn do_save(me: &Character, players: &Arc<Mutex<PlayerDb>>) -> CmdOutput {
    let pl = players.lock().await;
    let rec = match pl.load_player(&me.name) {
        Ok(mut r) => {
            r.hp     = me.hp;
            r.max_hp = me.max_hp;
            r.room   = me.current_room;
            r.gold   = me.gold;
            r
        }
        Err(e) => {
            return CmdOutput::text(format!("\r\nSave failed: {e}\r\n"));
        }
    };
    match pl.save_player(&rec) {
        Ok(()) => CmdOutput::text("\r\nSaving Testperson.\r\nYou have been saved.\r\n"
            .replace("Testperson", &me.name)),
        Err(e) => CmdOutput::text(format!("\r\nSave failed: {e}\r\n")),
    }
}

async fn do_equipment(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let any = me.equipment.iter().any(|s| s.is_some());
    if !any {
        return CmdOutput::text("\r\nYou are not using anything.\r\n");
    }
    let w = world.lock().await;
    let mut s = String::from("\r\nYou are using:\r\n");
    for slot in 0..NUM_WEARS {
        if let Some(iid) = me.equipment[slot] {
            let short = w.obj_instances.iter().find(|o| o.id == iid)
                .and_then(|o| w.obj_protos.get(&o.vnum))
                .map(|p| p.short_description.clone())
                .unwrap_or_else(|| "(something)".into());
            s.push_str(&format!("  <{:^22}>  {}\r\n", wear_pos_name(slot), short));
        }
    }
    CmdOutput::text(s)
}

/// Locate a keyword match within an inventory list.  Returns
/// (vec_index, instance_id, short_description) of the first match.
fn find_inv_match(w: &World, inv: &[u32], key: &str) -> Option<(usize, u32, String)> {
    for (i, &iid) in inv.iter().enumerate() {
        if let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) {
            if let Some(p) = w.obj_protos.get(&obj.vnum) {
                if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(key)) {
                    return Some((i, iid, p.short_description.clone()));
                }
            }
        }
    }
    None
}

async fn do_move(
    dir: Direction,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let w = world.lock().await;
    let r = match w.rooms.get(&me.current_room) {
        Some(r) => r,
        None    => return CmdOutput::text("\r\nYou are nowhere.\r\n"),
    };
    let target = match &r.exits[dir as usize] {
        Some(e) if e.to_room != crate::world::NOWHERE
            && w.rooms.contains_key(&e.to_room) => e.to_room,
        _ => return CmdOutput::text(format!("\r\nAlas, you cannot go that way...\r\n")),
    };
    drop(w);

    let from_room = me.current_room;
    let leave_msg = format!("{} leaves {}.\r\n", me.name, dir.name());
    let arrive_msg = format!("{} has arrived.\r\n", me.name);

    me.current_room = target;
    {
        let mut cl = chars.lock().await;
        cl.update_room(me.id, target);
        cl.broadcast_room(from_room, Some(me.id), &leave_msg);
        cl.broadcast_room(target,    Some(me.id), &arrive_msg);
    }

    // Show the new room
    CmdOutput::text(render_room(target, Some(me.id), world, chars).await)
}

// ---------------------------------------------------------------------------
// Room rendering — lives in interpreter.rs so look/move share the format.
// ---------------------------------------------------------------------------

/// Format a room (name, description, exits, ground objects, mobs, other
/// players) for the player at `viewer_id`.
pub async fn render_room(
    vnum: RoomVnum,
    viewer_id: Option<u32>,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> String {
    let w = world.lock().await;
    let Some(r) = w.rooms.get(&vnum) else {
        return "\r\nYou are nowhere.\r\n".to_string();
    };

    let mut s = String::with_capacity(r.description.len() + 512);
    s.push_str("\r\n");
    s.push_str(&r.name);
    s.push_str("\r\n");
    for line in r.description.split('\n') {
        s.push_str(line);
        s.push_str("\r\n");
    }

    // Exits
    let exits: Vec<&str> = Direction::ALL.iter()
        .filter(|d| r.exits[**d as usize].as_ref()
            .map(|e| e.to_room != crate::world::NOWHERE)
            .unwrap_or(false))
        .map(|d| d.name())
        .collect();
    if exits.is_empty() {
        s.push_str("Obvious exits: none.\r\n");
    } else {
        s.push_str("Obvious exits: ");
        s.push_str(&exits.join(", "));
        s.push_str(".\r\n");
    }

    // Ground objects
    for &iid in &r.objects {
        if let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) {
            if let Some(p) = w.obj_protos.get(&obj.vnum) {
                if !p.description.is_empty() {
                    s.push_str(&p.description);
                    s.push_str("\r\n");
                }
            }
        }
    }

    // Mobs
    for &mid in &r.mobs {
        if let Some(m) = w.mob_instances.iter().find(|m| m.id == mid) {
            if let Some(mp) = w.mob_protos.get(&m.vnum) {
                if !mp.long_descr.is_empty() {
                    s.push_str(mp.long_descr.trim_end());
                    s.push_str("\r\n");
                }
            }
        }
    }
    drop(w);

    // Other players in this room
    let cl = chars.lock().await;
    for p in cl.iter() {
        if p.current_room != vnum { continue; }
        if Some(p.id) == viewer_id { continue; }
        s.push_str(&format!("{} is standing here.\r\n", p.name));
    }

    s
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_obj_by_id(w: &World, iid: u32) -> Option<&crate::world::ObjInstance> {
    w.obj_instances.iter().find(|o| o.id == iid)
}

fn obj_keyword_matches(w: &World, vnum: ObjVnum, key: &str) -> bool {
    w.obj_protos.get(&vnum)
        .map(|p| p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(key)))
        .unwrap_or(false)
}

#[allow(dead_code)]
fn _silence_unused(c: CharacterList) -> CharacterList { c }
