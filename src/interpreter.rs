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
    world::{Direction, ObjVnum, RoomVnum, World, ITEM_ARMOR},
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
    "get", "drop", "put", "give", "wield", "wear", "remove",
    "examine",
    "list", "buy", "sell",
    "kick", "bash", "backstab",
    "sneak", "hide", "steal",
    "cast",
    "skills", "practice", "affects",
    "quest", "where",
    "say", "tell", "who",
    "score", "exp", "equipment", "save", "help",
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
        Some("put")       => do_put(rest, me, world, chars).await,
        Some("say")       => do_say(rest, me, chars).await,
        Some("tell")      => do_tell(rest, me, chars).await,
        Some("who")       => do_who(me, chars).await,
        Some("score")     => do_score(me, world).await,
        Some("exp")       => do_exp(me),
        Some("kill") | Some("hit") => do_kill(rest, me, world, chars).await,
        Some("kick")      => do_skill(rest, me, world, chars, Skill::Kick).await,
        Some("bash")      => do_skill(rest, me, world, chars, Skill::Bash).await,
        Some("backstab")  => do_skill(rest, me, world, chars, Skill::Backstab).await,
        Some("sneak")     => do_sneak(me),
        Some("hide")      => do_hide(me),
        Some("steal")     => do_steal(rest, me, world, chars).await,
        Some("cast")      => do_cast(rest, me, world, chars).await,
        Some("skills")    => do_skills(me),
        Some("practice")  => do_practice(rest, me),
        Some("affects")   => do_affects(me),
        Some("quest")     => do_quest(rest, me, world, chars).await,
        Some("where")     => do_where(me, world, chars).await,
        Some("give")      => do_give(rest, me, world, chars).await,
        Some("examine")   => do_examine(rest, me, world, chars).await,
        Some("list")      => do_list(me, world).await,
        Some("buy")       => do_buy(rest, me, world, chars).await,
        Some("sell")      => do_sell(rest, me, world, chars).await,
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
            if obj_matches_keyword(&w, obj, &key) {
                return CmdOutput::text(format!("\r\n{}", describe_obj(&w, iid)));
            }
        }
    }

    // Room objects
    if let Some(r) = w.rooms.get(&me.current_room) {
        for &iid in &r.objects {
            if let Some(obj) = find_obj_by_id(&w, iid) {
                if obj_matches_keyword(&w, obj, &key) {
                    return CmdOutput::text(format!("\r\n{}", describe_obj(&w, iid)));
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
            let v = obj_view(&w, obj);
            s.push_str(" ");
            s.push_str(&v.short);
            s.push_str("\r\n");
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

    // "get <obj> <container>" — pull from container; otherwise pull from room.
    let parts: Vec<&str> = arg.splitn(3, ' ').collect();
    let from_container = parts.len() >= 2
        && !parts[0].eq_ignore_ascii_case("from")
        && (parts.len() == 2 ||
            (parts.len() >= 3 && parts[1].eq_ignore_ascii_case("from")));
    if from_container {
        let obj_kw = parts[0];
        let cont_kw = if parts.len() == 2 { parts[1] } else { parts[2] };
        return do_get_from_container(obj_kw, cont_kw, me, world, chars).await;
    }

    let key = arg.to_ascii_lowercase();
    let mut w = world.lock().await;

    let (iid, name) = {
        let r = match w.rooms.get(&me.current_room) {
            Some(r) => r,
            None => return CmdOutput::text("\r\nYou are nowhere.\r\n"),
        };
        // Scan room objects for first keyword match. Uses obj_view so
        // corpses (which have no proto) are matchable as "corpse" / mob name.
        let mut found: Option<(u32, String)> = None;
        for &iid in &r.objects {
            if let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) {
                if obj_matches_keyword(&w, obj, &key) {
                    let v = obj_view(&w, obj);
                    found = Some((iid, v.short));
                    break;
                }
            }
        }
        match found {
            Some(f) => f,
            None => return CmdOutput::text(format!("\r\nYou see no {key} here.\r\n")),
        }
    };

    // Capture the object's vnum + weight for quest hook + carry-cap check.
    let (picked_vnum, picked_weight) = w.obj_instances.iter().find(|o| o.id == iid)
        .map(|o| (Some(o.vnum), w.obj_protos.get(&o.vnum).map(|p| p.weight).unwrap_or(0)))
        .unwrap_or((None, 0));

    // Enforce carry weight cap.
    let cap = crate::character::str_carry_cap(me.str_);
    let cur = total_carry_weight(me, &w);
    if cur + picked_weight > cap {
        return CmdOutput::text(format!(
            "\r\n{} is too heavy for you to carry. ({} + {} > {} lb)\r\n",
            name, cur, picked_weight, cap,
        ));
    }

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
    drop(cl);

    let mut msg = format!("\r\nYou get {}.\r\n", name);
    if let Some(vnum) = picked_vnum {
        if let Some(qmsg) = quest_check_pickup(me, vnum, world).await {
            msg.push_str(&qmsg);
        }
    }
    CmdOutput::text(msg)
}

/// Find a container (in inventory or in the current room) by keyword.
/// Returns the container's instance id and a brief identifier for messages.
fn find_container(
    w: &World,
    me: &Character,
    cont_kw: &str,
) -> Option<(u32, String)> {
    let key = cont_kw.to_ascii_lowercase();
    let try_one = |iid: u32| -> Option<(u32, String)> {
        let o = w.obj_instances.iter().find(|o| o.id == iid)?;
        let v = obj_view(w, o);
        if v.item_type == crate::world::ITEM_CONTAINER
            && v.keywords.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
            Some((iid, v.short))
        } else {
            None
        }
    };
    // Inventory containers first.
    for &iid in &me.inventory {
        if let Some(t) = try_one(iid) { return Some(t); }
    }
    // Then room containers.
    if let Some(r) = w.rooms.get(&me.current_room) {
        for &iid in &r.objects {
            if let Some(t) = try_one(iid) { return Some(t); }
        }
    }
    None
}

async fn do_get_from_container(
    obj_kw: &str,
    cont_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let key = obj_kw.to_ascii_lowercase();
    let mut w = world.lock().await;

    let (container_iid, container_name) = match find_container(&w, me, cont_kw) {
        Some(t) => t,
        None => return CmdOutput::text(format!("\r\nYou see no {cont_kw} here.\r\n")),
    };

    // Find a matching item inside.
    let (idx_in_container, child_iid, child_short) = {
        let container = w.obj_instances.iter().find(|o| o.id == container_iid).unwrap();
        let mut found = None;
        for (i, &cid) in container.contents.iter().enumerate() {
            if let Some(child) = w.obj_instances.iter().find(|o| o.id == cid) {
                if let Some(p) = w.obj_protos.get(&child.vnum) {
                    if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                        found = Some((i, cid, p.short_description.clone()));
                        break;
                    }
                }
            }
        }
        match found {
            Some(t) => t,
            None => return CmdOutput::text(format!(
                "\r\nThere is no {obj_kw} in {container_name}.\r\n"
            )),
        }
    };

    // Capture child vnum for quest hook.
    let child_vnum = w.obj_instances.iter().find(|o| o.id == child_iid).map(|o| o.vnum);

    // Remove from container, add to player's inventory.
    if let Some(container) = w.obj_instances.iter_mut().find(|o| o.id == container_iid) {
        container.contents.remove(idx_in_container);
    }
    me.inventory.push(child_iid);
    drop(w);

    let cl = chars.lock().await;
    cl.broadcast_room(
        me.current_room, Some(me.id),
        &format!("{} gets {} from {}.\r\n", me.name, child_short, container_name),
    );
    drop(cl);

    let mut msg = format!("\r\nYou get {} from {}.\r\n", child_short, container_name);
    if let Some(vnum) = child_vnum {
        if let Some(qmsg) = quest_check_pickup(me, vnum, world).await {
            msg.push_str(&qmsg);
        }
    }
    CmdOutput::text(msg)
}

async fn do_put(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    // "put <obj> <container>" or "put <obj> in <container>"
    let parts: Vec<&str> = arg.splitn(3, ' ').collect();
    let (obj_kw, cont_kw) = match parts.as_slice() {
        [_, _, _] if parts[1].eq_ignore_ascii_case("in") => (parts[0], parts[2]),
        [_, _]     => (parts[0], parts[1]),
        _          => return CmdOutput::text("\r\nPut what in what?\r\n"),
    };

    let mut w = world.lock().await;

    let (idx, iid, short) = match find_inv_match(&w, &me.inventory, &obj_kw.to_ascii_lowercase()) {
        Some(t) => t,
        None    => return CmdOutput::text(format!("\r\nYou do not have a {obj_kw}.\r\n")),
    };

    let (container_iid, container_name) = match find_container(&w, me, cont_kw) {
        Some(t) => t,
        None    => return CmdOutput::text(format!("\r\nYou see no {cont_kw} here.\r\n")),
    };

    if container_iid == iid {
        return CmdOutput::text("\r\nYou can't put something inside itself.\r\n");
    }

    me.inventory.remove(idx);
    if let Some(container) = w.obj_instances.iter_mut().find(|o| o.id == container_iid) {
        container.contents.push(iid);
    }
    drop(w);

    let cl = chars.lock().await;
    cl.broadcast_room(
        me.current_room, Some(me.id),
        &format!("{} puts {} in {}.\r\n", me.name, short, container_name),
    );

    CmdOutput::text(format!("\r\nYou put {} in {}.\r\n", short, container_name))
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

async fn do_say(arg: &str, me: &mut Character, chars: &SharedChars) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nYak yak yak...\r\n");
    }
    me.reveal();
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

async fn do_where(
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let immortal = me.level >= 34;
    let cl = chars.lock().await;
    let w = world.lock().await;
    let mut s = String::from("\r\nPlayers in the world:\r\n");
    for p in cl.iter() {
        // Skip hidden players unless we're immortal or them.
        if !immortal && p.id != me.id {
            let hidden = p.character.lock().await.hidden;
            if hidden { continue; }
        }
        let room_name = w.rooms.get(&p.current_room)
            .map(|r| r.name.as_str())
            .unwrap_or("(nowhere)");
        let marker = if p.id == me.id { " (you)" } else { "" };
        s.push_str(&format!(
            "  {:<14}  [{:>5}] {}{}\r\n",
            p.name, p.current_room, room_name, marker,
        ));
    }
    CmdOutput::text(s)
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

async fn do_score(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let ac = total_ac(me, world).await;
    let next = Character::exp_for_level(me.level);
    let to_next = (next - me.exp).max(0);
    let exp_str = if next == i64::MAX {
        format!("{} (max level)", me.exp)
    } else {
        format!("{} ({} to next)", me.exp, to_next)
    };
    let s = format!(
        "\r\nName:  {}\r\nLevel: {}\r\nExp:   {exp_str}\r\nHP:    {}/{}\r\nMana:  {}/{}\r\nClass: {:?}\r\nSex:   {:?}\r\nGold:  {}\r\nRoom:  {}\r\nAC:    {}\r\nPrac:  {}\r\n\
         Str/Int/Wis/Dex/Con/Cha: {}/{}/{}/{}/{}/{}\r\n",
        me.name, me.level, me.hp, me.max_hp, me.mana, me.max_mana,
        me.class, me.sex, me.gold, me.current_room, ac, me.practices,
        me.str_, me.int_, me.wis, me.dex, me.con, me.cha,
    );
    CmdOutput::text(s)
}

// ---------------------------------------------------------------------------
// Quest command
// ---------------------------------------------------------------------------

async fn do_quest(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let parts: Vec<&str> = arg.splitn(2, char::is_whitespace).collect();
    let sub = parts.first().copied().unwrap_or("").to_ascii_lowercase();
    let rest = parts.get(1).map(|s| s.trim()).unwrap_or("");
    match sub.as_str() {
        "" | "help" => CmdOutput::text(
            "\r\nQuest commands:\r\n  \
             quest list             - show quests available from a questmaster here\r\n  \
             quest info <vnum>      - details for a quest\r\n  \
             quest join <vnum>      - accept a quest\r\n  \
             quest status           - show your active quest\r\n  \
             quest complete         - turn in a completed quest (at the giver)\r\n  \
             quest abandon          - give up the current quest\r\n",
        ),
        "list"     => do_quest_list(me, world).await,
        "info"     => do_quest_info(rest, world).await,
        "join"     => do_quest_join(rest, me, world, chars).await,
        "status"   => do_quest_status(me, world).await,
        "complete" => do_quest_complete(me, world, chars).await,
        "abandon"  => do_quest_abandon(me, world),
        _ => CmdOutput::text(format!("\r\nUnknown quest subcommand: {sub}\r\n")),
    }
}

async fn do_quest_list(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let w = world.lock().await;
    // Find all mobs in this room — for each, list the quests where qm == that mob.
    let room_mob_vnums: Vec<i32> = w.rooms.get(&me.current_room)
        .map(|r| r.mobs.iter()
            .filter_map(|&mid| w.mob_instances.iter().find(|m| m.id == mid).map(|m| m.vnum))
            .collect())
        .unwrap_or_default();
    if room_mob_vnums.is_empty() {
        return CmdOutput::text("\r\nThere is no questmaster here.\r\n");
    }
    let mut s = String::from("\r\nQuests available here:\r\n");
    let mut found_any = false;
    for q in w.quests.values() {
        if !room_mob_vnums.contains(&q.qm) { continue; }
        // Skip quests the player has already completed AND that aren't repeatable.
        let repeatable = q.flags & 1 != 0;
        if !repeatable && me.completed_quests.contains(&q.vnum) {
            continue;
        }
        found_any = true;
        s.push_str(&format!("  [{:>5}] {}\r\n", q.vnum, q.name));
    }
    if !found_any {
        s.push_str("  (none — try another questmaster)\r\n");
    }
    CmdOutput::text(s)
}

async fn do_quest_info(arg: &str, world: &Arc<Mutex<World>>) -> CmdOutput {
    let Ok(vnum): Result<i32, _> = arg.parse() else {
        return CmdOutput::text("\r\nUse: quest info <vnum>\r\n");
    };
    let w = world.lock().await;
    let Some(q) = w.quests.get(&vnum) else {
        return CmdOutput::text(format!("\r\nNo quest #{vnum}.\r\n"));
    };
    let kind_str = match q.kind {
        crate::world::AQ_OBJ_FIND   => format!("retrieve object #{}", q.target),
        crate::world::AQ_ROOM_FIND  => format!("visit room #{}", q.target),
        crate::world::AQ_MOB_FIND   => format!("locate mob #{}", q.target),
        crate::world::AQ_MOB_KILL   => format!("slay mob #{}", q.target),
        crate::world::AQ_MOB_SAVE   => format!("rescue mob #{}", q.target),
        crate::world::AQ_OBJ_RETURN => format!("return object #{} to mob #{}", q.target, q.value[5]),
        crate::world::AQ_ROOM_CLEAR => format!("clear room #{}", q.target),
        _ => "unknown".to_string(),
    };
    let s = format!(
        "\r\n=== Quest #{} — {} ===\r\n{}\r\nObjective: {}\r\nReward: {} gold, {} exp{}\r\n",
        q.vnum, q.name, q.info, kind_str,
        q.gold_reward, q.exp_reward,
        if q.obj_reward >= 0 { format!(", obj #{}", q.obj_reward) } else { String::new() },
    );
    CmdOutput::text(s)
}

async fn do_quest_join(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let Ok(vnum): Result<i32, _> = arg.parse() else {
        return CmdOutput::text("\r\nUse: quest join <vnum>\r\n");
    };
    if me.active_quest.is_some() {
        return CmdOutput::text(
            "\r\nYou already have an active quest. Use `quest abandon` first.\r\n",
        );
    }
    let q_info: Option<(i32, String, i32)> = {
        let w = world.lock().await;
        let Some(q) = w.quests.get(&vnum) else {
            return CmdOutput::text(format!("\r\nNo quest #{vnum}.\r\n"));
        };
        // Questmaster must be in the room.
        let room_mob_vnums: Vec<i32> = w.rooms.get(&me.current_room)
            .map(|r| r.mobs.iter()
                .filter_map(|&mid| w.mob_instances.iter().find(|m| m.id == mid).map(|m| m.vnum))
                .collect())
            .unwrap_or_default();
        if !room_mob_vnums.contains(&q.qm) {
            return CmdOutput::text(
                "\r\nThe questmaster for that quest is not here.\r\n",
            );
        }
        // Prereq check.
        if q.prereq != -1 && !me.completed_quests.contains(&q.prereq) {
            return CmdOutput::text(format!(
                "\r\nYou must first complete quest #{} before taking this one.\r\n",
                q.prereq,
            ));
        }
        // Repeatable check.
        let repeatable = q.flags & 1 != 0;
        if !repeatable && me.completed_quests.contains(&q.vnum) {
            return CmdOutput::text("\r\nYou have already completed that quest.\r\n");
        }
        Some((q.vnum, q.desc.clone(), q.qm))
    };
    let (vnum, desc, _qm) = q_info.unwrap();
    me.active_quest = Some(vnum);
    me.quest_progress = 0;
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} accepts a quest.\r\n", me.name));
    CmdOutput::text(format!(
        "\r\nYou accept the quest.\r\n{desc}\r\n",
    ))
}

async fn do_quest_status(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let Some(vnum) = me.active_quest else {
        return CmdOutput::text("\r\nYou have no active quest.\r\n");
    };
    let w = world.lock().await;
    let Some(q) = w.quests.get(&vnum) else {
        return CmdOutput::text("\r\nYour quest's data has been lost.\r\n");
    };
    let done = matches!(q.kind,
        crate::world::AQ_MOB_KILL | crate::world::AQ_OBJ_FIND | crate::world::AQ_OBJ_RETURN
    ) && me.quest_progress >= 1;
    let s = format!(
        "\r\n=== Active Quest #{} — {} ===\r\n{}\r\nProgress: {} {}\r\n",
        q.vnum, q.name, q.info,
        me.quest_progress,
        if done { "(COMPLETE — return to the questmaster)" } else { "" },
    );
    CmdOutput::text(s)
}

async fn do_quest_complete(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let Some(vnum) = me.active_quest else {
        return CmdOutput::text("\r\nYou have no active quest.\r\n");
    };
    let (qname, done_msg, qm_vnum, gold, exp, obj_reward, can_turn_in, next_q) = {
        let w = world.lock().await;
        let Some(q) = w.quests.get(&vnum) else {
            return CmdOutput::text("\r\nYour quest's data has been lost.\r\n");
        };
        // Questmaster must be present.
        let room_mob_vnums: Vec<i32> = w.rooms.get(&me.current_room)
            .map(|r| r.mobs.iter()
                .filter_map(|&mid| w.mob_instances.iter().find(|m| m.id == mid).map(|m| m.vnum))
                .collect())
            .unwrap_or_default();
        let qm_here = room_mob_vnums.contains(&q.qm);
        (q.name.clone(), q.done.clone(), q.qm, q.gold_reward, q.exp_reward, q.obj_reward, qm_here, q.next_quest)
    };
    if !can_turn_in {
        return CmdOutput::text(
            "\r\nThe questmaster for this quest is not here.\r\n",
        );
    }
    if me.quest_progress < 1 {
        return CmdOutput::text("\r\nYou haven't completed the objective yet.\r\n");
    }

    // Award rewards.
    me.gold += gold as i64;
    if exp > 0 {
        me.exp += exp as i64;
        let lvls = me.check_level_up();
        if lvls > 0 {
            // Will be displayed via the response.
        }
    }
    // Spawn the obj reward into the player's inventory.
    if obj_reward >= 0 {
        let iid = {
            let mut w = world.lock().await;
            w.spawn_obj(obj_reward)
        };
        if let Some(iid) = iid {
            me.inventory.push(iid);
        }
    }
    me.completed_quests.push(vnum);
    me.active_quest = None;
    me.quest_progress = 0;
    let _ = qm_vnum;

    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} completes a quest!\r\n", me.name));
    }

    // Auto-join the next quest in the chain, if any.  We re-check the
    // questmaster-is-here invariant since the next quest may belong to a
    // different master; if so, we just announce the chain and let the
    // player seek them out.
    let mut chain_msg = String::new();
    if next_q != -1 && next_q != 0 {
        let chain_ok: Option<(String, String, bool)> = {
            let w = world.lock().await;
            w.quests.get(&next_q).map(|nq| {
                let mob_vnums: Vec<i32> = w.rooms.get(&me.current_room)
                    .map(|r| r.mobs.iter()
                        .filter_map(|&mid|
                            w.mob_instances.iter().find(|m| m.id == mid).map(|m| m.vnum))
                        .collect())
                    .unwrap_or_default();
                let here = mob_vnums.contains(&nq.qm);
                (nq.name.clone(), nq.desc.clone(), here)
            })
        };
        if let Some((nname, ndesc, here)) = chain_ok {
            if here {
                me.active_quest = Some(next_q);
                me.quest_progress = 0;
                chain_msg = format!(
                    "\r\n=== Next Quest: {nname} ===\r\n{ndesc}\r\n",
                );
            } else {
                chain_msg = format!(
                    "\r\n(Seek the next questmaster to continue: #{next_q})\r\n",
                );
            }
        }
    }

    CmdOutput::text(format!(
        "\r\n=== Quest Complete: {qname} ===\r\n{done_msg}\r\n\
         Rewards: {gold} gold, {exp} exp{obj_text}\r\n{chain_msg}",
        obj_text = if obj_reward >= 0 { format!(", obj #{obj_reward}") } else { String::new() },
    ))
}

/// If the player has an active AQ_MOB_KILL quest targeting `killed_vnum`,
/// mark the objective complete and return a player-facing message.
/// Returns `None` if no quest progress occurred.
pub async fn quest_check_kill(
    me: &mut Character,
    killed_vnum: i32,
    world: &Arc<Mutex<World>>,
) -> Option<String> {
    let qv = me.active_quest?;
    let w = world.lock().await;
    let q = w.quests.get(&qv)?;
    if q.kind != crate::world::AQ_MOB_KILL { return None; }
    if q.target != killed_vnum { return None; }
    if me.quest_progress >= 1 { return None; }
    me.quest_progress = 1;
    let mob_name = w.mob_protos.get(&killed_vnum)
        .map(|p| p.short_descr.clone())
        .unwrap_or_else(|| "the target".to_string());
    Some(format!(
        "\r\n*** Quest objective complete: you have slain {mob_name}! Return to the questmaster. ***\r\n",
    ))
}

/// AQ_OBJ_FIND: completes when the player picks up an object matching the
/// target vnum.  Returns a player-facing message if progress was made.
pub async fn quest_check_pickup(
    me: &mut Character,
    obj_vnum: i32,
    world: &Arc<Mutex<World>>,
) -> Option<String> {
    let qv = me.active_quest?;
    let w = world.lock().await;
    let q = w.quests.get(&qv)?;
    if q.kind != crate::world::AQ_OBJ_FIND { return None; }
    if q.target != obj_vnum { return None; }
    if me.quest_progress >= 1 { return None; }
    me.quest_progress = 1;
    let short = w.obj_protos.get(&obj_vnum)
        .map(|p| p.short_description.clone())
        .unwrap_or_else(|| "the item".to_string());
    Some(format!(
        "\r\n*** Quest objective complete: you have found {short}! Return to the questmaster. ***\r\n",
    ))
}

/// AQ_ROOM_FIND: completes when the player enters a room matching the
/// target room vnum.
pub async fn quest_check_room(
    me: &mut Character,
    room_vnum: i32,
    world: &Arc<Mutex<World>>,
) -> Option<String> {
    let qv = me.active_quest?;
    let w = world.lock().await;
    let q = w.quests.get(&qv)?;
    if q.kind != crate::world::AQ_ROOM_FIND { return None; }
    if q.target != room_vnum { return None; }
    if me.quest_progress >= 1 { return None; }
    me.quest_progress = 1;
    let room_name = w.rooms.get(&room_vnum)
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "the destination".to_string());
    Some(format!(
        "\r\n*** Quest objective complete: you have reached {room_name}! Return to the questmaster. ***\r\n",
    ))
}

/// AQ_OBJ_RETURN: completes when the player gives the target object to
/// the target recipient mob (quest.target = obj vnum, quest.value[5] =
/// recipient mob vnum).
pub async fn quest_check_give(
    me: &mut Character,
    given_obj_vnum: i32,
    given_to_mob_vnum: i32,
    world: &Arc<Mutex<World>>,
) -> Option<String> {
    let qv = me.active_quest?;
    let w = world.lock().await;
    let q = w.quests.get(&qv)?;
    if q.kind != crate::world::AQ_OBJ_RETURN { return None; }
    if q.target != given_obj_vnum { return None; }
    if q.value[5] != given_to_mob_vnum { return None; }
    if me.quest_progress >= 1 { return None; }
    me.quest_progress = 1;
    Some(
        "\r\n*** Quest objective complete: you have delivered the item! Return to the questmaster. ***\r\n".to_string()
    )
}

fn do_quest_abandon(me: &mut Character, _world: &Arc<Mutex<World>>) -> CmdOutput {
    if me.active_quest.is_none() {
        return CmdOutput::text("\r\nYou have no quest to abandon.\r\n");
    }
    me.active_quest = None;
    me.quest_progress = 0;
    CmdOutput::text("\r\nYou abandon your quest.\r\n")
}

fn do_skills(me: &Character) -> CmdOutput {
    use crate::character::ALL_SKILLS;
    let mut s = String::from("\r\nSkills available to your class:\r\n");
    let mut any = false;
    for &skill in ALL_SKILLS {
        if !skill.is_class_allowed(me.class) { continue; }
        any = true;
        let pct = *me.skills.get(&skill).unwrap_or(&0);
        s.push_str(&format!("  {:<10} {:>3}%\r\n", skill.name(), pct));
    }
    if !any {
        s.push_str("  (none — your class has no learnable skills)\r\n");
    }
    CmdOutput::text(s)
}

fn do_practice(arg: &str, me: &mut Character) -> CmdOutput {
    if arg.is_empty() {
        // Show skills + remaining practices budget.
        let mut out = do_skills(me).text;
        out.push_str(&format!("\r\nYou have {} practice point(s).\r\n", me.practices));
        return CmdOutput::text(out);
    }
    // Guild-room restriction — must be in your class's guild to practice.
    if !is_guild_room_for(me.current_room, me.class) {
        return CmdOutput::text(format!(
            "\r\nYou must visit a {:?} guild to practice your art.\r\n", me.class,
        ));
    }
    let Some(skill) = crate::character::Skill::parse(arg) else {
        return CmdOutput::text(format!("\r\nThere is no skill called '{arg}'.\r\n"));
    };
    if !skill.is_class_allowed(me.class) {
        return CmdOutput::text(format!(
            "\r\n{} is not a {:?} skill.\r\n", uppercase_first(skill.name()), me.class,
        ));
    }
    if me.practices <= 0 {
        return CmdOutput::text(
            "\r\nYou have no practice points left. Level up to gain more.\r\n".to_string()
        );
    }
    let pct = me.skills.entry(skill).or_insert(0);
    if *pct >= 90 {
        return CmdOutput::text(format!(
            "\r\nYou know everything you can about {} ({}%).\r\n", skill.name(), pct,
        ));
    }
    *pct = (*pct + 10).min(90);
    me.practices -= 1;
    CmdOutput::text(format!(
        "\r\nYou practice {} a bit. ({}%, {} practice(s) left)\r\n",
        skill.name(), pct, me.practices,
    ))
}

/// Which rooms count as guild halls for each class.  Vnums come from
/// Midgaard's stock zone (`lib/world/wld/30.wld`).  Multiple rooms per
/// class accommodate the entry hall + practice room layout used in zone 30.
fn is_guild_room_for(room: crate::world::RoomVnum, class: crate::players::Class) -> bool {
    use crate::players::Class;
    match class {
        // Cleric guild & practice rooms (Temple area)
        Class::Cleric    => matches!(room, 3001 | 3004 | 3017),
        // Mage guild
        Class::MagicUser => matches!(room, 3018 | 3027),
        // Warrior guild
        Class::Warrior   => matches!(room, 3022 | 3023),
        // Thief guild — Midgaard's dark alley
        Class::Thief     => matches!(room, 3038 | 3041),
        Class::Undefined => true,  // tutorial / pre-class state
    }
}

fn do_affects(me: &Character) -> CmdOutput {
    if me.affects.is_empty() {
        return CmdOutput::text("\r\nYou are not affected by any spells or enchantments.\r\n");
    }
    let mut s = String::from("\r\nActive effects:\r\n");
    for a in &me.affects {
        let mut parts: Vec<String> = Vec::new();
        if a.to_hit != 0 { parts.push(format!("hit {:+}", a.to_hit)); }
        if a.to_dam != 0 { parts.push(format!("dam {:+}", a.to_dam)); }
        if a.dmg_reduction != 0 { parts.push(format!("dmg-reduction {}%", a.dmg_reduction)); }
        let mods = if parts.is_empty() { "—".to_string() } else { parts.join(", ") };
        s.push_str(&format!(
            "  {:<14} {:<25} ({} ticks left)\r\n",
            a.name(), mods, a.duration,
        ));
    }
    CmdOutput::text(s)
}

fn uppercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None    => String::new(),
    }
}

fn do_exp(me: &Character) -> CmdOutput {
    let next = Character::exp_for_level(me.level);
    if next == i64::MAX {
        return CmdOutput::text(format!(
            "\r\nYou have {} experience (max mortal level reached).\r\n", me.exp,
        ));
    }
    CmdOutput::text(format!(
        "\r\nLevel {}: {} experience, {} until next level.\r\n",
        me.level, me.exp, (next - me.exp).max(0),
    ))
}

/// Sum of weights of every object the player is carrying (inventory +
/// equipment).  Container contents count toward the carrier's weight.
pub fn total_carry_weight(me: &Character, w: &World) -> i32 {
    let mut sum = 0;
    let mut stack: Vec<u32> = Vec::new();
    stack.extend(me.inventory.iter().copied());
    stack.extend(me.equipment.iter().filter_map(|s| *s));
    while let Some(iid) = stack.pop() {
        if let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) {
            if let Some(p) = w.obj_protos.get(&o.vnum) {
                sum += p.weight;
            }
            // Descend into container contents.
            stack.extend(o.contents.iter().copied());
        }
    }
    sum
}

/// Total AC = sum of worn ITEM_ARMOR value[0] + DEX defensive bonus.
/// Higher is better.
pub async fn total_ac(me: &Character, world: &Arc<Mutex<World>>) -> i32 {
    let w = world.lock().await;
    let mut total = crate::character::dex_ac_bonus(me.dex);
    for slot in me.equipment.iter() {
        if let Some(iid) = slot {
            if let Some(obj) = w.obj_instances.iter().find(|o| o.id == *iid) {
                if let Some(p) = w.obj_protos.get(&obj.vnum) {
                    if p.item_type == ITEM_ARMOR {
                        total += p.value[0];
                    }
                }
            }
        }
    }
    total
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
    me.reveal();
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

// ---------------------------------------------------------------------------
// Combat skills (kick, bash)
// ---------------------------------------------------------------------------

use crate::character::Skill;

async fn do_skill(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    skill: Skill,
) -> CmdOutput {
    use rand::Rng;
    use crate::db::dice;

    me.reveal();
    // Class restriction.
    if !skill.is_class_allowed(me.class) {
        return CmdOutput::text(format!(
            "\r\nYou do not know how to {}.\r\n", skill.name(),
        ));
    }
    // Must have practised the skill at all.
    let learned = *me.skills.get(&skill).unwrap_or(&0);
    if learned == 0 {
        return CmdOutput::text(format!(
            "\r\nYou are unfamiliar with the art of {}. Try `practice {}`.\r\n",
            skill.name(), skill.name(),
        ));
    }

    // Choose target: either the explicit arg, or our current fighting target.
    let target_mob_id: Option<u32> = if !arg.is_empty() {
        let key = arg.to_ascii_lowercase();
        let w = world.lock().await;
        let r = w.rooms.get(&me.current_room);
        r.and_then(|r| r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                Some(mid)
            } else { None }
        }))
    } else {
        me.fighting.filter(|t| !t.is_player).map(|t| t.id)
    };

    let Some(mob_id) = target_mob_id else {
        return CmdOutput::text(format!("\r\n{} whom?\r\n",
            uppercase_first(skill.name())));
    };

    // Per-skill prerequisites.
    if let Skill::Bash = skill {
        if me.equipment[crate::character::WEAR_SHIELD].is_none() {
            return CmdOutput::text("\r\nYou need a shield to bash effectively.\r\n");
        }
    }
    if let Skill::Backstab = skill {
        // Backstab needs a piercing weapon AND target not yet fighting.
        if me.equipment[crate::character::WEAR_WIELD].is_none() {
            return CmdOutput::text("\r\nYou need to wield a weapon to backstab.\r\n");
        }
        if me.fighting.is_some() {
            return CmdOutput::text("\r\nYou can't backstab someone while in combat.\r\n");
        }
    }

    // Roll to-hit (modified by skill %) and damage.
    let (hit, dmg) = {
        let mut rng = rand::thread_rng();
        let str_b = crate::character::str_damage_bonus(me.str_);
        // Hit chance baseline + skill bonus.
        let base_hit = match skill {
            Skill::Kick     => 60,
            Skill::Bash     => 30,
            Skill::Backstab => 40,
            _ => return CmdOutput::text("\r\nThat isn't a physical skill.\r\n"),
        };
        let hit_chance = (base_hit + learned as i32 / 2).min(95);
        let hit = rng.gen_range(0..100) < hit_chance;
        let dmg = match skill {
            Skill::Kick     => dice(1, 6) + me.level / 2 + str_b,
            Skill::Bash     => dice(2, 4) + me.level + str_b,
            Skill::Backstab => dice(3, 6) + me.level * 2 + str_b,
            _ => 0,
        };
        (hit, dmg.max(1))
    };

    let verb = skill.name();

    // Apply.
    let (mob_name, killed_vnum, mob_dead, mob_room) = {
        let mut w = world.lock().await;
        let Some(m) = w.mob_instances.iter().find(|m| m.id == mob_id) else {
            return CmdOutput::text("\r\nYour target is gone.\r\n");
        };
        let vnum = m.vnum;
        let mob_name = w.mob_protos.get(&vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());
        let mob_room = m.in_room;
        if mob_room != me.current_room {
            return CmdOutput::text("\r\nYour target is no longer here.\r\n");
        }

        // Engage combat regardless of hit/miss — committing to the attack
        // pulls the mob into the fight.
        let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
        let dead = if hit {
            m.hp -= dmg;
            m.hp <= 0
        } else {
            false
        };
        (mob_name, vnum, dead, mob_room)
    };

    // Broadcast + reply.
    let (to_me, to_room) = if hit {
        (
            format!("\r\nYou {verb} {mob_name} for {dmg} damage!\r\n"),
            format!("{} {verb}s {mob_name}.\r\n", me.name),
        )
    } else {
        (
            format!("\r\nYou {verb} at {mob_name}, but miss!\r\n"),
            format!("{} {verb}s at {mob_name}, but misses.\r\n", me.name),
        )
    };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    if mob_dead {
        // Look up XP first, then extract mob and spawn corpse.
        let xp = {
            let w = world.lock().await;
            w.mob_instances.iter().find(|m| m.id == mob_id)
                .and_then(|m| w.mob_protos.get(&m.vnum))
                .map(|p| p.exp as i64)
                .unwrap_or(0)
        };
        {
            let mut w = world.lock().await;
            let inv: Vec<u32> = w.mob_instances.iter()
                .find(|m| m.id == mob_id)
                .map(|m| m.inventory.clone()).unwrap_or_default();
            // Clear any other mob fighting state targeting this mob.
            for other in w.mob_instances.iter_mut() {
                if other.fighting.map(|t| !t.is_player && t.id == mob_id).unwrap_or(false) {
                    other.fighting = None;
                }
            }
            if let Some(r) = w.rooms.get_mut(&mob_room) {
                r.mobs.retain(|&id| id != mob_id);
            }
            w.mob_instances.retain(|m| m.id != mob_id);
            w.create_corpse(&mob_name, inv, mob_room);
        }
        me.fighting = None;
        {
            let cl = chars.lock().await;
            cl.broadcast_room(
                mob_room, None,
                &format!("\r\n{} has slain {mob_name}!\r\n", me.name),
            );
            cl.broadcast_room(
                mob_room, None,
                &format!("{mob_name} collapses to the floor, dead.\r\n"),
            );
        }
        // Award XP and check level-up locally (we hold the live `me`).
        let mut msg = format!("{to_me}\r\nYou have slain {mob_name}!\r\n");
        if xp > 0 {
            me.exp += xp;
            msg.push_str(&format!("You gain {xp} experience.\r\n"));
            let gained = me.check_level_up();
            if gained > 0 {
                msg.push_str(&format!(
                    "\r\n*** You feel more powerful!  You are now level {}.  Max HP: {} ***\r\n",
                    me.level, me.max_hp,
                ));
            }
        }
        if let Some(qmsg) = quest_check_kill(me, killed_vnum, world).await {
            msg.push_str(&qmsg);
        }
        return CmdOutput::text(msg);
    }

    CmdOutput::text(to_me)
}

// ---------------------------------------------------------------------------
// Spell casting
// ---------------------------------------------------------------------------

async fn do_cast(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nCast which spell? Try `cast magic-missile fido` or `cast cure-light`.\r\n");
    }
    me.reveal();

    // Accept either `cast '<spell name>' target` or `cast <hyphenated-spell> target`.
    let (spell_str, target) = if let Some(stripped) = arg.strip_prefix('\'') {
        match stripped.find('\'') {
            Some(end) => (&stripped[..end], stripped[end+1..].trim_start()),
            None      => return CmdOutput::text("\r\nUnclosed spell name (missing ').\r\n"),
        }
    } else {
        match arg.find(char::is_whitespace) {
            Some(i) => (&arg[..i], arg[i..].trim_start()),
            None    => (arg, ""),
        }
    };

    let Some(spell) = crate::character::Skill::parse(spell_str) else {
        return CmdOutput::text(format!("\r\nThere is no spell '{spell_str}'.\r\n"));
    };
    if spell.kind() != crate::character::SkillKind::Spell {
        return CmdOutput::text(format!(
            "\r\n{} is a skill, not a spell. Use `{}` directly.\r\n",
            uppercase_first(spell.name()), spell.save_key(),
        ));
    }
    if !spell.is_class_allowed(me.class) {
        return CmdOutput::text(format!(
            "\r\nYou cannot cast {}.\r\n", spell.name(),
        ));
    }
    let learned = *me.skills.get(&spell).unwrap_or(&0);
    if learned == 0 {
        return CmdOutput::text(format!(
            "\r\nYou haven't learned the spell '{}'. Try `practice {}`.\r\n",
            spell.name(), spell.save_key(),
        ));
    }
    let cost = spell.mana_cost();
    if me.mana < cost {
        return CmdOutput::text(format!(
            "\r\nYou lack the mana to cast {} (need {}, have {}).\r\n",
            spell.name(), cost, me.mana,
        ));
    }

    // Dispatch.
    match spell {
        crate::character::Skill::MagicMissile => cast_magic_missile(target, me, world, chars, learned).await,
        crate::character::Skill::CureLight    => cast_cure_light(target, me, chars, learned).await,
        crate::character::Skill::Bless        => cast_bless(target, me, chars, learned).await,
        crate::character::Skill::BurningHands => cast_burning_hands(me, world, chars, learned).await,
        crate::character::Skill::Sanctuary    => cast_sanctuary(target, me, chars, learned).await,
        crate::character::Skill::Harm         => cast_harm(target, me, world, chars, learned).await,
        crate::character::Skill::WordOfRecall => cast_word_of_recall(me, world, chars).await,
        crate::character::Skill::Identify     => cast_identify(target, me, world).await,
        crate::character::Skill::DetectInvis  => cast_detect_invis(me),
        _ => CmdOutput::text("\r\nUnknown spell.\r\n"),
    }
}

fn cast_detect_invis(me: &mut Character) -> CmdOutput {
    // Adds a long-duration Affect that signals render_room to skip the
    // hidden-player filter for this viewer.
    let aff = crate::character::Affect {
        skill:         crate::character::Skill::DetectInvis,
        duration:      12,   // ~24s of clear vision
        to_hit:        0,
        to_dam:        0,
        dmg_reduction: 0,
    };
    me.mana -= crate::character::Skill::DetectInvis.mana_cost();
    me.apply_affect(aff);
    CmdOutput::text(
        "\r\nYour eyes tingle. You can sense things that wish to remain unseen.\r\n",
    )
}

async fn cast_magic_missile(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;

    // Target lookup: mob in room, falling back to current fighting target.
    let target_mob_id: Option<u32> = if !target_kw.is_empty() {
        let key = target_kw.to_ascii_lowercase();
        let w = world.lock().await;
        let r = w.rooms.get(&me.current_room);
        r.and_then(|r| r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                Some(mid)
            } else { None }
        }))
    } else {
        me.fighting.filter(|t| !t.is_player).map(|t| t.id)
    };

    let Some(mob_id) = target_mob_id else {
        return CmdOutput::text("\r\nThere is no such target here.\r\n");
    };

    // Hit chance: 70 base + half of learned %. Magic missile rarely misses.
    let hit_chance = (70 + learned as i32 / 2).min(99);
    let hit = rand::thread_rng().gen_range(0..100) < hit_chance;
    let dmg = crate::db::dice(1, 4) + me.level + crate::character::str_damage_bonus(me.int_);
    me.mana -= crate::character::Skill::MagicMissile.mana_cost();

    let (mob_name, killed_vnum, mob_dead, mob_room) = {
        let mut w = world.lock().await;
        let m = match w.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => m,
            None    => return CmdOutput::text("\r\nYour target has vanished.\r\n"),
        };
        let vnum = m.vnum;
        let mob_name = w.mob_protos.get(&vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());
        let mob_room = m.in_room;
        if mob_room != me.current_room {
            return CmdOutput::text("\r\nYour target is no longer here.\r\n");
        }
        // Engage combat.
        let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
        let dead = if hit { m.hp -= dmg; m.hp <= 0 } else { false };
        (mob_name, vnum, dead, mob_room)
    };

    let (to_me, to_room) = if hit {
        (
            format!("\r\nA glowing dart of force streaks from your hand and strikes {mob_name} for {dmg} damage!\r\n"),
            format!("A glowing dart of force streaks from {}'s hand and strikes {mob_name}.\r\n", me.name),
        )
    } else {
        (
            format!("\r\nYour magic missile misses {mob_name}.\r\n"),
            format!("{}'s magic missile streaks past {mob_name}.\r\n", me.name),
        )
    };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    if mob_dead {
        let xp = {
            let w = world.lock().await;
            w.mob_instances.iter().find(|m| m.id == mob_id)
                .and_then(|m| w.mob_protos.get(&m.vnum))
                .map(|p| p.exp as i64)
                .unwrap_or(0)
        };
        {
            let mut w = world.lock().await;
            let inv: Vec<u32> = w.mob_instances.iter()
                .find(|m| m.id == mob_id)
                .map(|m| m.inventory.clone()).unwrap_or_default();
            for other in w.mob_instances.iter_mut() {
                if other.fighting.map(|t| !t.is_player && t.id == mob_id).unwrap_or(false) {
                    other.fighting = None;
                }
            }
            if let Some(r) = w.rooms.get_mut(&mob_room) {
                r.mobs.retain(|&id| id != mob_id);
            }
            w.mob_instances.retain(|m| m.id != mob_id);
            w.create_corpse(&mob_name, inv, mob_room);
        }
        me.fighting = None;
        {
            let cl = chars.lock().await;
            cl.broadcast_room(
                mob_room, None,
                &format!("\r\n{} has slain {mob_name}!\r\n", me.name),
            );
        }
        let mut msg = format!("{to_me}\r\nYou have slain {mob_name}!\r\n");
        if xp > 0 {
            me.exp += xp;
            msg.push_str(&format!("You gain {xp} experience.\r\n"));
            let gained = me.check_level_up();
            if gained > 0 {
                msg.push_str(&format!(
                    "\r\n*** You feel more powerful!  You are now level {}.  Max HP: {} ***\r\n",
                    me.level, me.max_hp,
                ));
            }
        }
        if let Some(qmsg) = quest_check_kill(me, killed_vnum, world).await {
            msg.push_str(&qmsg);
        }
        return CmdOutput::text(msg);
    }

    CmdOutput::text(to_me)
}

async fn cast_cure_light(
    target_kw: &str,
    me: &mut Character,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;

    // Cure light: target self if no arg, or another player in the same
    // room by name.  No PvP healing concerns since combat is mob-only.
    let target_handle: Option<crate::character::PlayerHandle> = if target_kw.is_empty() {
        None
    } else {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| {
            p.current_room == me.current_room
                && p.name.eq_ignore_ascii_case(target_kw)
        }).cloned();
        h
    };

    let heal = crate::db::dice(1, 8) + me.level
        + (me.wis - 10).max(0) / 2;
    let hit_chance = (90 + learned as i32 / 5).min(99);
    let hit = rand::thread_rng().gen_range(0..100) < hit_chance;
    me.mana -= crate::character::Skill::CureLight.mana_cost();

    if !hit {
        return CmdOutput::text("\r\nYou lose your concentration and the spell fizzles.\r\n");
    }

    if let Some(ph) = target_handle {
        // Heal another player.
        let (new_hp, max) = {
            let mut c = ph.character.lock().await;
            c.hp = (c.hp + heal).min(c.max_hp);
            (c.hp, c.max_hp)
        };
        let _ = ph.send.send(format!(
            "\r\n{} weaves a soothing prayer over you. You feel better. ({}/{} HP)\r\n",
            me.name, new_hp, max,
        ));
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} weaves a soothing prayer over {}.\r\n", me.name, ph.name));
        CmdOutput::text(format!(
            "\r\nYou weave a soothing prayer over {} ({} HP restored).\r\n",
            ph.name, heal,
        ))
    } else {
        // Heal self.
        me.hp = (me.hp + heal).min(me.max_hp);
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} weaves a soothing prayer over themself.\r\n", me.name));
        CmdOutput::text(format!(
            "\r\nA warm glow flows through you. ({}/{} HP)\r\n",
            me.hp, me.max_hp,
        ))
    }
}

async fn cast_bless(
    target_kw: &str,
    me: &mut Character,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;
    let target_handle: Option<crate::character::PlayerHandle> = if target_kw.is_empty() {
        None
    } else {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| {
            p.current_room == me.current_room
                && p.name.eq_ignore_ascii_case(target_kw)
        }).cloned();
        h
    };

    me.mana -= crate::character::Skill::Bless.mana_cost();

    // Hit chance scales with skill %.
    let hit_chance = (75 + learned as i32 / 5).min(99);
    if rand::thread_rng().gen_range(0..100) >= hit_chance {
        return CmdOutput::text("\r\nYour blessing falters and fizzles.\r\n");
    }

    // Bless: +1 to-hit, +1 to-dam, lasts 6 combat ticks (~12s).
    let aff = crate::character::Affect {
        skill:         crate::character::Skill::Bless,
        duration:      6 + (learned as i32 / 20),
        to_hit:        1,
        to_dam:        1,
        dmg_reduction: 0,
    };

    if let Some(ph) = target_handle {
        let dur = aff.duration;
        {
            let mut c = ph.character.lock().await;
            c.apply_affect(aff);
        }
        let _ = ph.send.send(format!(
            "\r\n{} invokes a blessing upon you. You feel emboldened. ({} ticks)\r\n",
            me.name, dur,
        ));
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} blesses {}.\r\n", me.name, ph.name));
        CmdOutput::text(format!("\r\nYou invoke a blessing upon {}.\r\n", ph.name))
    } else {
        let dur = aff.duration;
        me.apply_affect(aff);
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} mutters a blessing under their breath.\r\n", me.name));
        CmdOutput::text(format!(
            "\r\nYou feel righteous. (blessed for {} ticks)\r\n", dur,
        ))
    }
}

async fn cast_sanctuary(
    target_kw: &str,
    me: &mut Character,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;
    let target_handle: Option<crate::character::PlayerHandle> = if target_kw.is_empty() {
        None
    } else {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| {
            p.current_room == me.current_room
                && p.name.eq_ignore_ascii_case(target_kw)
        }).cloned();
        h
    };
    me.mana -= crate::character::Skill::Sanctuary.mana_cost();

    let hit_chance = (70 + learned as i32 / 5).min(99);
    if rand::thread_rng().gen_range(0..100) >= hit_chance {
        return CmdOutput::text(
            "\r\nYour prayer goes unanswered; the aura fails to form.\r\n".to_string(),
        );
    }

    // Sanctuary: 50% damage reduction for 8 ticks (~16s).
    let aff = crate::character::Affect {
        skill:         crate::character::Skill::Sanctuary,
        duration:      8 + (learned as i32 / 20),
        to_hit:        0,
        to_dam:        0,
        dmg_reduction: 50,
    };

    if let Some(ph) = target_handle {
        let dur = aff.duration;
        {
            let mut c = ph.character.lock().await;
            c.apply_affect(aff);
        }
        let _ = ph.send.send(format!(
            "\r\n{} surrounds you with a glowing white aura. ({} ticks)\r\n",
            me.name, dur,
        ));
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} surrounds {} with a glowing white aura.\r\n", me.name, ph.name));
        CmdOutput::text(format!("\r\nYou surround {} with a glowing white aura.\r\n", ph.name))
    } else {
        let dur = aff.duration;
        me.apply_affect(aff);
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} is surrounded by a glowing white aura.\r\n", me.name));
        CmdOutput::text(format!(
            "\r\nA glowing white aura surrounds you. (sanctuary for {} ticks)\r\n", dur,
        ))
    }
}

async fn cast_harm(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;
    use crate::db::dice;

    let target_mob_id: Option<u32> = if !target_kw.is_empty() {
        let key = target_kw.to_ascii_lowercase();
        let w = world.lock().await;
        let r = w.rooms.get(&me.current_room);
        r.and_then(|r| r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                Some(mid)
            } else { None }
        }))
    } else {
        me.fighting.filter(|t| !t.is_player).map(|t| t.id)
    };
    let Some(mob_id) = target_mob_id else {
        return CmdOutput::text("\r\nThere is no such target here.\r\n");
    };

    let hit_chance = (65 + learned as i32 / 4).min(95);
    let hit = rand::thread_rng().gen_range(0..100) < hit_chance;
    let dmg = dice(3, 8) + me.level + (me.wis - 10).max(0) / 2;
    me.mana -= crate::character::Skill::Harm.mana_cost();

    let (mob_name, killed_vnum, mob_dead, mob_room) = {
        let mut w = world.lock().await;
        let (vnum, in_room) = match w.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => (m.vnum, m.in_room),
            None    => return CmdOutput::text("\r\nYour target has vanished.\r\n"),
        };
        let mob_name = w.mob_protos.get(&vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());
        if in_room != me.current_room {
            return CmdOutput::text("\r\nYour target is no longer here.\r\n");
        }
        let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
        let dead = if hit { m.hp -= dmg; m.hp <= 0 } else { false };
        (mob_name, vnum, dead, in_room)
    };

    let (to_me, to_room) = if hit {
        (
            format!("\r\nYou call down divine wrath upon {mob_name}! ({dmg} damage)\r\n"),
            format!("{} calls down divine wrath upon {mob_name}.\r\n", me.name),
        )
    } else {
        (
            format!("\r\nYour curse upon {mob_name} fails to take hold.\r\n"),
            format!("{} curses {mob_name}, who shrugs it off.\r\n", me.name),
        )
    };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    if mob_dead {
        let xp = {
            let w = world.lock().await;
            w.mob_instances.iter().find(|m| m.id == mob_id)
                .and_then(|m| w.mob_protos.get(&m.vnum))
                .map(|p| p.exp as i64)
                .unwrap_or(0)
        };
        {
            let mut w = world.lock().await;
            let inv: Vec<u32> = w.mob_instances.iter()
                .find(|m| m.id == mob_id)
                .map(|m| m.inventory.clone()).unwrap_or_default();
            for other in w.mob_instances.iter_mut() {
                if other.fighting.map(|t| !t.is_player && t.id == mob_id).unwrap_or(false) {
                    other.fighting = None;
                }
            }
            if let Some(r) = w.rooms.get_mut(&mob_room) {
                r.mobs.retain(|&id| id != mob_id);
            }
            w.mob_instances.retain(|m| m.id != mob_id);
            w.create_corpse(&mob_name, inv, mob_room);
        }
        me.fighting = None;
        {
            let cl = chars.lock().await;
            cl.broadcast_room(
                mob_room, None,
                &format!("\r\n{} has slain {mob_name}!\r\n", me.name),
            );
        }
        let mut msg = format!("{to_me}\r\nYou have slain {mob_name}!\r\n");
        if xp > 0 {
            me.exp += xp;
            msg.push_str(&format!("You gain {xp} experience.\r\n"));
            let gained = me.check_level_up();
            if gained > 0 {
                msg.push_str(&format!(
                    "\r\n*** You feel more powerful!  You are now level {}.  Max HP: {} ***\r\n",
                    me.level, me.max_hp,
                ));
            }
        }
        if let Some(qmsg) = quest_check_kill(me, killed_vnum, world).await {
            msg.push_str(&qmsg);
        }
        return CmdOutput::text(msg);
    }
    CmdOutput::text(to_me)
}

async fn cast_identify(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
) -> CmdOutput {
    if target_kw.is_empty() {
        return CmdOutput::text("\r\nIdentify what? (Specify an item in your inventory.)\r\n");
    }
    let key = target_kw.to_ascii_lowercase();
    me.mana -= crate::character::Skill::Identify.mana_cost();

    // Find a matching obj in inventory or equipment first, then room.
    let w = world.lock().await;
    let candidate: Option<u32> = me.inventory.iter().copied().find(|iid| {
        if let Some(o) = w.obj_instances.iter().find(|o| o.id == *iid) {
            obj_matches_keyword(&w, o, &key)
        } else { false }
    }).or_else(|| {
        me.equipment.iter().flatten().copied().find(|iid| {
            if let Some(o) = w.obj_instances.iter().find(|o| o.id == *iid) {
                obj_matches_keyword(&w, o, &key)
            } else { false }
        })
    }).or_else(|| {
        let r = w.rooms.get(&me.current_room)?;
        r.objects.iter().copied().find(|iid| {
            if let Some(o) = w.obj_instances.iter().find(|o| o.id == *iid) {
                obj_matches_keyword(&w, o, &key)
            } else { false }
        })
    });

    let Some(iid) = candidate else {
        return CmdOutput::text(format!("\r\nYou see no {key} to identify.\r\n"));
    };
    let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) else {
        return CmdOutput::text("\r\nThe item slips from your mind.\r\n");
    };

    // Corpses have no proto — special-case.
    if let Some(of) = &obj.corpse_of {
        let count = obj.contents.len();
        return CmdOutput::text(format!(
            "\r\nIdentify result:\r\n  the corpse of {of}\r\n  type:      corpse\r\n  contents:  {count} items\r\n",
        ));
    }

    let Some(p) = w.obj_protos.get(&obj.vnum) else {
        return CmdOutput::text("\r\nYou cannot fathom what this is.\r\n");
    };
    let kind_name = item_type_name(p.item_type);
    let mut s = format!(
        "\r\nIdentify result:\r\n  {}\r\n  type:      {}\r\n  weight:    {}\r\n  cost:      {}\r\n",
        p.short_description, kind_name, p.weight, p.cost,
    );
    match p.item_type {
        5 /* ITEM_WEAPON */ => {
            s.push_str(&format!("  damage:    {}d{} ({:+} avg)\r\n",
                p.value[1], p.value[2],
                if p.value[1] > 0 && p.value[2] > 0 {
                    p.value[1] * (p.value[2] + 1) / 2
                } else { 0 },
            ));
        }
        9 /* ITEM_ARMOR */ => {
            s.push_str(&format!("  AC apply:  {}\r\n", p.value[0]));
        }
        1 /* ITEM_LIGHT */ => {
            s.push_str(&format!("  hours:     {}\r\n", p.value[2]));
        }
        15 /* ITEM_CONTAINER */ => {
            s.push_str(&format!("  capacity:  {} lb\r\n", p.value[0]));
            s.push_str(&format!("  contents:  {} item(s)\r\n", obj.contents.len()));
        }
        _ => {}
    }
    if p.level > 0 {
        s.push_str(&format!("  min level: {}\r\n", p.level));
    }
    CmdOutput::text(s)
}

async fn cast_word_of_recall(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let from_room = me.current_room;
    // Mortal start = Temple of Midgaard.  Immortals get the immortal start.
    let target = {
        let w = world.lock().await;
        w.start_room(me.level >= 34)
    };
    me.mana -= crate::character::Skill::WordOfRecall.mana_cost();
    me.fighting = None;
    me.hidden   = false;
    me.sneaking = false;
    let was_room = me.current_room;
    me.current_room = target;
    // Clear any mob targeting this player.
    {
        let mut w = world.lock().await;
        for m in w.mob_instances.iter_mut() {
            if m.fighting.map(|t| t.is_player && t.id == me.id).unwrap_or(false) {
                m.fighting = None;
            }
        }
        let _ = was_room;
    }
    // Update registry and broadcast.
    {
        let mut cl = chars.lock().await;
        cl.update_room(me.id, target);
        cl.broadcast_room(
            from_room, Some(me.id),
            &format!("{} disappears in a flash of holy light!\r\n", me.name),
        );
        cl.broadcast_room(
            target, Some(me.id),
            &format!("{} appears in a flash of holy light!\r\n", me.name),
        );
    }
    let view = render_room(target, Some(me.id), world, chars).await;
    CmdOutput::text(format!(
        "\r\nA holy beacon snatches you back to the temple.\r\n{view}",
    ))
}

async fn cast_burning_hands(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;
    use crate::db::dice;

    // Pull mob list for the room.
    let mob_ids: Vec<u32> = {
        let w = world.lock().await;
        w.rooms.get(&me.current_room)
            .map(|r| r.mobs.clone())
            .unwrap_or_default()
    };
    if mob_ids.is_empty() {
        return CmdOutput::text("\r\nThere is nothing here for your flames to consume.\r\n");
    }
    me.mana -= crate::character::Skill::BurningHands.mana_cost();

    let mut to_me = String::from("\r\nA cone of flame erupts from your hands!\r\n");
    let to_room = format!("{} hurls a cone of flame across the room!\r\n", me.name);
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    let mut total_xp = 0i64;
    let mut killed_names: Vec<String> = Vec::new();
    for mob_id in mob_ids {
        // Per-target hit roll.
        let hit_chance = (65 + learned as i32 / 4).min(95);
        if rand::thread_rng().gen_range(0..100) >= hit_chance { continue; }
        let dmg = dice(2, 4) + me.level / 2
            + crate::character::str_damage_bonus(me.int_);

        let (mob_name, mob_dead, mob_room) = {
            let mut w = world.lock().await;
            let (vnum, in_room) = match w.mob_instances.iter().find(|m| m.id == mob_id) {
                Some(m) => (m.vnum, m.in_room),
                None => continue,
            };
            if in_room != me.current_room { continue; }
            let mob_name = w.mob_protos.get(&vnum)
                .map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "the creature".into());
            let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
            m.hp -= dmg;
            if me.fighting.is_none() {
                me.fighting = Some(Target { id: mob_id, is_player: false });
            }
            if m.fighting.is_none() {
                m.fighting = Some(Target { id: me.id, is_player: true });
            }
            (mob_name, m.hp <= 0, in_room)
        };
        to_me.push_str(&format!("Flames sear {mob_name} for {dmg} damage!\r\n"));

        if mob_dead {
            let xp = {
                let w = world.lock().await;
                w.mob_instances.iter().find(|m| m.id == mob_id)
                    .and_then(|m| w.mob_protos.get(&m.vnum))
                    .map(|p| p.exp as i64)
                    .unwrap_or(0)
            };
            total_xp += xp;
            {
                let mut w = world.lock().await;
                let inv: Vec<u32> = w.mob_instances.iter()
                    .find(|m| m.id == mob_id)
                    .map(|m| m.inventory.clone()).unwrap_or_default();
                for other in w.mob_instances.iter_mut() {
                    if other.fighting.map(|t| !t.is_player && t.id == mob_id).unwrap_or(false) {
                        other.fighting = None;
                    }
                }
                if let Some(r) = w.rooms.get_mut(&mob_room) {
                    r.mobs.retain(|&id| id != mob_id);
                }
                w.mob_instances.retain(|m| m.id != mob_id);
                w.create_corpse(&mob_name, inv, mob_room);
            }
            {
                let cl = chars.lock().await;
                cl.broadcast_room(
                    mob_room, None,
                    &format!("{mob_name} is reduced to ashes.\r\n"),
                );
            }
            killed_names.push(mob_name);
        }
    }

    // If we ended up with no living foes, drop combat.
    if !killed_names.is_empty() {
        let still_have_target = {
            let w = world.lock().await;
            me.fighting.map(|t| !t.is_player
                && w.mob_instances.iter().any(|m| m.id == t.id)).unwrap_or(false)
        };
        if !still_have_target { me.fighting = None; }
    }

    if !killed_names.is_empty() {
        for name in &killed_names {
            to_me.push_str(&format!("You have slain {name}!\r\n"));
        }
        if total_xp > 0 {
            me.exp += total_xp;
            to_me.push_str(&format!("You gain {total_xp} experience.\r\n"));
            let gained = me.check_level_up();
            if gained > 0 {
                to_me.push_str(&format!(
                    "\r\n*** You feel more powerful!  You are now level {}.  Max HP: {} ***\r\n",
                    me.level, me.max_hp,
                ));
            }
        }
    }

    CmdOutput::text(to_me)
}

// ---------------------------------------------------------------------------
// Thief utility skills (sneak / hide / steal)
// ---------------------------------------------------------------------------

fn do_sneak(me: &mut Character) -> CmdOutput {
    if !crate::character::Skill::Sneak.is_class_allowed(me.class) {
        return CmdOutput::text("\r\nYou are too clumsy to sneak about.\r\n");
    }
    let learned = *me.skills.get(&crate::character::Skill::Sneak).unwrap_or(&0);
    if learned == 0 {
        return CmdOutput::text(
            "\r\nYou haven't practised sneaking. Try `practice sneak`.\r\n",
        );
    }
    me.sneaking = !me.sneaking;
    if me.sneaking {
        CmdOutput::text("\r\nYou are now sneaking quietly.\r\n")
    } else {
        CmdOutput::text("\r\nYou stop sneaking.\r\n")
    }
}

fn do_hide(me: &mut Character) -> CmdOutput {
    use rand::Rng;
    if !crate::character::Skill::Hide.is_class_allowed(me.class) {
        return CmdOutput::text("\r\nYou have no idea how to hide.\r\n");
    }
    let learned = *me.skills.get(&crate::character::Skill::Hide).unwrap_or(&0);
    if learned == 0 {
        return CmdOutput::text(
            "\r\nYou haven't practised hiding. Try `practice hide`.\r\n",
        );
    }
    let chance = (40 + learned as i32).min(95);
    let success = rand::thread_rng().gen_range(0..100) < chance;
    if success {
        me.hidden = true;
        CmdOutput::text("\r\nYou attempt to hide yourself.\r\n")
    } else {
        // Failure tries to look secretive but ultimately fails — same
        // message either way: the player can't easily tell.
        me.hidden = false;
        CmdOutput::text("\r\nYou attempt to hide yourself.\r\n")
    }
}

async fn do_steal(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use rand::Rng;
    if !crate::character::Skill::Steal.is_class_allowed(me.class) {
        return CmdOutput::text("\r\nYou couldn't pickpocket if your life depended on it.\r\n");
    }
    let learned = *me.skills.get(&crate::character::Skill::Steal).unwrap_or(&0);
    if learned == 0 {
        return CmdOutput::text(
            "\r\nYou haven't practised stealing. Try `practice steal`.\r\n",
        );
    }
    // "steal <item|coins> <target>"
    let parts: Vec<&str> = arg.splitn(2, char::is_whitespace).collect();
    if parts.len() < 2 {
        return CmdOutput::text("\r\nSteal what from whom?\r\n");
    }
    let what = parts[0].to_ascii_lowercase();
    let target_kw = parts[1].trim().to_ascii_lowercase();

    // Find a mob in the room with the target keyword.
    let mob_id = {
        let w = world.lock().await;
        let r = w.rooms.get(&me.current_room);
        r.and_then(|r| r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&target_kw)) {
                Some(mid)
            } else { None }
        }))
    };
    let Some(mob_id) = mob_id else {
        return CmdOutput::text(format!("\r\nYou see no {target_kw} here.\r\n"));
    };

    // Hide breaks on stealing; sneak survives.
    me.hidden = false;

    let success = rand::thread_rng().gen_range(0..100) < (30 + learned as i32 / 2).min(85);

    // Mob info needed regardless of success.
    let mob_name = {
        let w = world.lock().await;
        w.mob_instances.iter().find(|m| m.id == mob_id)
            .and_then(|m| w.mob_protos.get(&m.vnum))
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into())
    };

    if !success {
        // Detection — mob aggros.
        {
            let mut w = world.lock().await;
            if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
                if m.fighting.is_none() {
                    m.fighting = Some(Target { id: me.id, is_player: true });
                }
            }
        }
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
        }
        let cl = chars.lock().await;
        cl.broadcast_room(
            me.current_room, Some(me.id),
            &format!("{mob_name} catches {} trying to steal from them!\r\n", me.name),
        );
        return CmdOutput::text(format!(
            "\r\nOops. {mob_name} catches you and bristles in anger!\r\n",
        ));
    }

    // Success — take coins or a named item.
    if what == "coins" || what == "gold" || what == "money" {
        // We don't model mob gold currently; treat as a small windfall
        // proportional to mob level.
        let level = {
            let w = world.lock().await;
            w.mob_instances.iter().find(|m| m.id == mob_id)
                .and_then(|m| w.mob_protos.get(&m.vnum))
                .map(|p| p.gold.max(1))
                .unwrap_or(1)
        };
        let take = (level / 4).max(1) as i64;
        me.gold += take;
        return CmdOutput::text(format!(
            "\r\nYou lift {take} gold from {mob_name}.\r\n",
        ));
    }

    // Otherwise: try to steal a named item from mob inventory.
    let stolen = {
        let mut w = world.lock().await;
        let mob = w.mob_instances.iter().find(|m| m.id == mob_id);
        let mob_inv = mob.map(|m| m.inventory.clone()).unwrap_or_default();
        let mut found: Option<(u32, String)> = None;
        for &iid in &mob_inv {
            if let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) {
                if let Some(p) = w.obj_protos.get(&o.vnum) {
                    if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&what)) {
                        found = Some((iid, p.short_description.clone()));
                        break;
                    }
                }
            }
        }
        if let Some((iid, _)) = found.as_ref() {
            // Remove from mob, the caller pushes onto player inventory.
            if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
                m.inventory.retain(|&i| i != *iid);
            }
        }
        found
    };

    let Some((iid, short)) = stolen else {
        return CmdOutput::text(format!(
            "\r\n{mob_name} has no {what} for you to steal.\r\n",
        ));
    };
    me.inventory.push(iid);
    CmdOutput::text(format!(
        "\r\nYou deftly lift {short} from {mob_name}.\r\n",
    ))
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

async fn do_examine(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    // examine = look <arg> plus any item-type-specific details.  We
    // delegate to do_look and append type info if the keyword matched an
    // object.  For Checkpoint 8 the extra detail is just the item-type
    // banner.
    if arg.is_empty() {
        return CmdOutput::text("\r\nExamine what?\r\n");
    }
    let base = do_look(arg, me, world, chars).await;

    // Quick item-type sniffing: find a matching object and report its type.
    let key = arg.to_ascii_lowercase();
    let w = world.lock().await;
    let proto_info: Option<(i32, [i32; 4])> = me.inventory.iter()
        .chain(me.equipment.iter().filter_map(|s| s.as_ref()))
        .find_map(|&iid| {
            let o = w.obj_instances.iter().find(|o| o.id == iid)?;
            let p = w.obj_protos.get(&o.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                Some((p.item_type, p.value))
            } else {
                None
            }
        });

    if let Some((ty, vals)) = proto_info {
        let kind = item_type_name(ty);
        let extra = match ty {
            // ITEM_WEAPON: value[1] dice count, value[2] dice size, value[3] damage type
            5 => format!("This is a {kind} that does {}d{} damage.\r\n", vals[1], vals[2]),
            // ITEM_ARMOR: value[0] is AC
            9 => format!("This is {kind}, providing {} AC.\r\n", vals[0]),
            // ITEM_LIGHT: value[2] is hours remaining
            1 => format!("This is a {kind} with {} hours of light left.\r\n", vals[2]),
            _ => format!("This is a {kind}.\r\n"),
        };
        let mut out = base.text;
        out.push_str(&extra);
        return CmdOutput::text(out);
    }
    base
}

async fn do_give(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let (obj_kw, target_kw) = match arg.find(char::is_whitespace) {
        Some(i) => (&arg[..i], arg[i..].trim_start()),
        None    => return CmdOutput::text("\r\nGive what to whom?\r\n"),
    };
    if target_kw.is_empty() {
        return CmdOutput::text("\r\nGive it to whom?\r\n");
    }
    let key = obj_kw.to_ascii_lowercase();

    // Find item in inventory
    let (idx, iid, short) = {
        let w = world.lock().await;
        match find_inv_match(&w, &me.inventory, &key) {
            Some(t) => t,
            None    => return CmdOutput::text(format!("\r\nYou do not have a {key}.\r\n")),
        }
    };

    // Target may be another player in the same room.
    let tlow = target_kw.to_ascii_lowercase();
    let target_player = {
        let cl = chars.lock().await;
        let found = cl.iter()
            .find(|p| p.current_room == me.current_room
                  && p.id != me.id
                  && p.name.to_ascii_lowercase() == tlow)
            .cloned();
        found
    };

    if let Some(ph) = target_player {
        // Transfer: remove from us, push to their inventory, notify.
        me.inventory.remove(idx);
        {
            let mut tc = ph.character.lock().await;
            tc.inventory.push(iid);
        }
        let _ = ph.send.send(format!("\r\n{} gives you {}.\r\n", me.name, short));
        let cl = chars.lock().await;
        cl.broadcast_room(
            me.current_room, Some(me.id),
            &format!("{} gives {} to {}.\r\n", me.name, short, ph.name),
        );
        // Don't echo to receiver again
        return CmdOutput::text(format!("\r\nYou give {} to {}.\r\n", short, ph.name));
    }

    // Or a mob in the same room — find by keyword.
    let mut w = world.lock().await;
    let room_mobs: Vec<u32> = w.rooms.get(&me.current_room)
        .map(|r| r.mobs.clone())
        .unwrap_or_default();
    let mob_match: Option<(u32, i32, String)> = room_mobs.iter().find_map(|&mid| {
        let m = w.mob_instances.iter().find(|m| m.id == mid)?;
        let p = w.mob_protos.get(&m.vnum)?;
        if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&tlow)) {
            Some((mid, m.vnum, p.short_descr.clone()))
        } else {
            None
        }
    });

    if let Some((mid, mob_vnum, mname)) = mob_match {
        // Capture obj vnum for the quest hook.
        let obj_vnum = w.obj_instances.iter().find(|o| o.id == iid).map(|o| o.vnum);
        me.inventory.remove(idx);
        if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mid) {
            m.inventory.push(iid);
        }
        drop(w);
        let cl = chars.lock().await;
        cl.broadcast_room(
            me.current_room, Some(me.id),
            &format!("{} gives {} to {}.\r\n", me.name, short, mname),
        );
        drop(cl);

        let mut msg = format!("\r\nYou give {} to {}.\r\n", short, mname);
        if let Some(ov) = obj_vnum {
            if let Some(qmsg) = quest_check_give(me, ov, mob_vnum, world).await {
                msg.push_str(&qmsg);
            }
        }
        return CmdOutput::text(msg);
    }

    CmdOutput::text(format!("\r\nNo one called '{target_kw}' is here.\r\n"))
}

// ---------------------------------------------------------------------------
// Shop commands
// ---------------------------------------------------------------------------

async fn do_list(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let w = world.lock().await;
    let Some(shop) = w.shop_in_room(me.current_room) else {
        return CmdOutput::text("\r\nThere is no shop here.\r\n");
    };
    if shop.sells.is_empty() {
        return CmdOutput::text("\r\nThe shopkeeper has nothing for sale.\r\n");
    }
    let mut s = String::from("\r\n##  Available    Item                                           Price\r\n");
    s.push_str(  "--  ---------    ----                                          ------\r\n");
    for (i, &vnum) in shop.sells.iter().enumerate() {
        let Some(p) = w.obj_protos.get(&vnum) else { continue };
        let price = (p.cost as f32 * shop.profit_buy) as i64;
        s.push_str(&format!(
            "{:>2}.  unlimited    {:<45} {:>6}\r\n",
            i + 1, p.short_description, price,
        ));
    }
    CmdOutput::text(s)
}

async fn do_buy(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nBuy what?\r\n");
    }
    let key = arg.to_ascii_lowercase();

    let (vnum, short, price, keeper_name) = {
        let w = world.lock().await;
        let Some(shop) = w.shop_in_room(me.current_room) else {
            return CmdOutput::text("\r\nThere is no shop here.\r\n");
        };
        let mut hit: Option<(i32, String, i64)> = None;
        for &vnum in &shop.sells {
            let Some(p) = w.obj_protos.get(&vnum) else { continue };
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                let price = (p.cost as f32 * shop.profit_buy) as i64;
                hit = Some((vnum, p.short_description.clone(), price));
                break;
            }
        }
        let Some((vnum, short, price)) = hit else {
            return CmdOutput::text(format!("\r\nThe shopkeeper has no {key} for sale.\r\n"));
        };
        let keeper_name = w.mob_protos.get(&shop.keeper_vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the shopkeeper".to_string());
        (vnum, short, price, keeper_name)
    };

    if me.gold < price {
        return CmdOutput::text(format!(
            "\r\n{keeper_name} says, 'You can't afford that ({price} gold)!'\r\n"
        ));
    }

    // Spawn a fresh instance, deduct gold, push to inventory.
    let iid = {
        let mut w = world.lock().await;
        w.spawn_obj(vnum)
    };
    let Some(iid) = iid else {
        return CmdOutput::text("\r\nThe shopkeeper fumbles awkwardly.\r\n");
    };
    me.gold -= price;
    me.inventory.push(iid);

    let cl = chars.lock().await;
    cl.broadcast_room(
        me.current_room, Some(me.id),
        &format!("{} buys {} from {}.\r\n", me.name, short, keeper_name),
    );

    CmdOutput::text(format!(
        "\r\n{keeper_name} says, 'Here you are, that'll be {price} gold.'\r\nYou now have {} gold.\r\n",
        me.gold,
    ))
}

async fn do_sell(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nSell what?\r\n");
    }
    let key = arg.to_ascii_lowercase();

    let (idx, iid, short, price, keeper_name) = {
        let w = world.lock().await;
        let Some(shop) = w.shop_in_room(me.current_room) else {
            return CmdOutput::text("\r\nThere is no shop here.\r\n");
        };
        let Some((idx, iid, short)) = find_inv_match(&w, &me.inventory, &key) else {
            return CmdOutput::text(format!("\r\nYou do not have a {key}.\r\n"));
        };
        // Look up the proto for cost and check the shop accepts this item type.
        let obj = w.obj_instances.iter().find(|o| o.id == iid).unwrap();
        let proto = w.obj_protos.get(&obj.vnum).unwrap();
        if !shop.buys_types.is_empty() && !shop.buys_types.contains(&proto.item_type) {
            return CmdOutput::text("\r\nThe shopkeeper doesn't buy that kind of item.\r\n");
        }
        let price = (proto.cost as f32 * shop.profit_sell) as i64;
        let keeper_name = w.mob_protos.get(&shop.keeper_vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the shopkeeper".to_string());
        (idx, iid, short, price, keeper_name)
    };

    // Remove from inventory; extract instance from world (item absorbed by shop).
    me.inventory.remove(idx);
    {
        let mut w = world.lock().await;
        w.obj_instances.retain(|o| o.id != iid);
    }
    me.gold += price;

    let cl = chars.lock().await;
    cl.broadcast_room(
        me.current_room, Some(me.id),
        &format!("{} sells {} to {}.\r\n", me.name, short, keeper_name),
    );

    CmdOutput::text(format!(
        "\r\n{keeper_name} gives you {price} gold for {short}.\r\nYou now have {} gold.\r\n",
        me.gold,
    ))
}

/// Best-effort English name for an ITEM_* type (structs.h).
fn item_type_name(t: i32) -> &'static str {
    match t {
        1 => "light source",
        2 => "scroll",
        3 => "wand",
        4 => "staff",
        5 => "weapon",
        6 => "missile",
        7 => "treasure",
        8 => "armor",
        9 => "armor",   // ITEM_ARMOR is 9 in tbaMUD (not 8 like some Circle forks)
        10 => "potion",
        11 => "worn item",
        12 => "other",
        13 => "trash",
        14 => "trap",
        15 => "container",
        16 => "note",
        17 => "drink container",
        18 => "key",
        19 => "food",
        20 => "money",
        21 => "pen",
        22 => "boat",
        23 => "fountain",
        _ => "object",
    }
}

async fn do_save(me: &Character, players: &Arc<Mutex<PlayerDb>>) -> CmdOutput {
    let pl = players.lock().await;
    let rec = match pl.load_player(&me.name) {
        Ok(mut r) => {
            r.hp        = me.hp;
            r.max_hp    = me.max_hp;
            r.mana      = me.mana;
            r.max_mana  = me.max_mana;
            r.practices = me.practices;
            r.room      = me.current_room;
            r.gold      = me.gold;
            r.exp       = me.exp;
            r.level     = me.level;
            r.str_      = me.str_;
            r.int_   = me.int_;
            r.wis    = me.wis;
            r.dex    = me.dex;
            r.con    = me.con;
            r.cha    = me.cha;
            r.skills.clear();
            for (skill, pct) in &me.skills {
                r.skills.insert(skill.save_key().to_string(), *pct);
            }
            r.active_quest    = me.active_quest;
            r.quest_progress  = me.quest_progress;
            r.completed_quests = me.completed_quests.clone();
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
    // Hide drops on any movement. Sneak persists across movements but
    // suppresses the broadcasts.
    let was_sneaking = me.sneaking;
    me.hidden = false;
    let leave_msg = format!("{} leaves {}.\r\n", me.name, dir.name());
    let arrive_msg = format!("{} has arrived.\r\n", me.name);

    me.current_room = target;
    {
        let mut cl = chars.lock().await;
        cl.update_room(me.id, target);
        if !was_sneaking {
            cl.broadcast_room(from_room, Some(me.id), &leave_msg);
            cl.broadcast_room(target,    Some(me.id), &arrive_msg);
        }
    }

    // Show the new room — and append any quest-room hit.
    let mut view = render_room(target, Some(me.id), world, chars).await;
    if let Some(qmsg) = quest_check_room(me, target, world).await {
        view.push_str(&qmsg);
    }
    CmdOutput::text(view)
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

    // Ground objects (uses obj_view so corpses render properly)
    for &iid in &r.objects {
        if let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) {
            let v = obj_view(&w, obj);
            if !v.long.is_empty() {
                s.push_str(&v.long);
                s.push_str("\r\n");
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

    // Other players in this room (skip hidden players unless we have
    // Detect-Invis active).
    let cl = chars.lock().await;
    let see_hidden = if let Some(vid) = viewer_id {
        match cl.iter().find(|p| p.id == vid) {
            Some(p) => {
                let c = p.character.lock().await;
                c.affects.iter().any(|a| a.skill == crate::character::Skill::DetectInvis)
            }
            None => false,
        }
    } else { false };

    for p in cl.iter() {
        if p.current_room != vnum { continue; }
        if Some(p.id) == viewer_id { continue; }
        if !see_hidden {
            let hidden = p.character.lock().await.hidden;
            if hidden { continue; }
        }
        let hidden_tag = if see_hidden {
            let c = p.character.lock().await;
            if c.hidden { " (hidden)" } else { "" }
        } else { "" };
        s.push_str(&format!("{} is standing here.{hidden_tag}\r\n", p.name));
    }

    s
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_obj_by_id(w: &World, iid: u32) -> Option<&crate::world::ObjInstance> {
    w.obj_instances.iter().find(|o| o.id == iid)
}

/// A view onto an object's display attributes — falls back to the proto
/// but is overridden for synthetic objects like corpses.
struct ObjView {
    short:     String,
    long:      String,           // "X is lying here." form
    item_type: i32,
    keywords:  String,           // space-separated keyword list for matching
}

fn obj_view(w: &World, obj: &crate::world::ObjInstance) -> ObjView {
    if let Some(short) = &obj.corpse_of {
        return ObjView {
            short:     format!("the corpse of {short}"),
            long:      format!("The corpse of {short} is lying here."),
            item_type: crate::world::ITEM_CONTAINER,
            keywords:  format!("corpse {short}"),
        };
    }
    if let Some(p) = w.obj_protos.get(&obj.vnum) {
        ObjView {
            short:     p.short_description.clone(),
            long:      p.description.clone(),
            item_type: p.item_type,
            keywords:  p.name.clone(),
        }
    } else {
        ObjView {
            short: "something".into(), long: "Something is here.".into(),
            item_type: 0, keywords: "thing".into(),
        }
    }
}

fn obj_matches_keyword(w: &World, obj: &crate::world::ObjInstance, key: &str) -> bool {
    let view = obj_view(w, obj);
    view.keywords.split_whitespace().any(|k| k.eq_ignore_ascii_case(key))
}

/// Produce a descriptive blob for one object, with container contents
/// listed inline if any.  Used by look/examine on inventory + room items.
fn describe_obj(w: &World, iid: u32) -> String {
    let Some(obj) = find_obj_by_id(w, iid) else { return String::new(); };
    let view = obj_view(w, obj);

    // Prefer proto's action_description for real objects (e.g. signs); for
    // corpses just use the short.
    let body: String = if obj.corpse_of.is_some() {
        view.short.clone()
    } else {
        let p = w.obj_protos.get(&obj.vnum);
        let ad = p.map(|p| p.action_description.as_str()).unwrap_or("");
        if ad.is_empty() { view.short.clone() } else { ad.to_string() }
    };
    let mut s = format!("{body}\r\n");

    if view.item_type == crate::world::ITEM_CONTAINER {
        if obj.contents.is_empty() {
            s.push_str("It is empty.\r\n");
        } else {
            s.push_str("It contains:\r\n");
            for &cid in &obj.contents {
                if let Some(c) = w.obj_instances.iter().find(|o| o.id == cid) {
                    let cv = obj_view(w, c);
                    s.push_str(&format!("  {}\r\n", cv.short));
                }
            }
        }
    }
    s
}

#[allow(dead_code)]
fn obj_keyword_matches(w: &World, vnum: ObjVnum, key: &str) -> bool {
    w.obj_protos.get(&vnum)
        .map(|p| p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(key)))
        .unwrap_or(false)
}

#[allow(dead_code)]
fn _silence_unused(c: CharacterList) -> CharacterList { c }
