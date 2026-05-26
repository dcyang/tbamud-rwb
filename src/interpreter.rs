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

use std::sync::{Arc, OnceLock};

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

/// Globally-accessible handle to the PlayerDb, populated by `server::run`
/// at boot. Used by script side-effects (`mforce`) that need to dispatch
/// real player commands without threading `players` through every
/// trigger firing path.
pub static PLAYERS_HANDLE: OnceLock<Arc<Mutex<PlayerDb>>> = OnceLock::new();

/// `mforce` work item — broken out of `apply_script_outputs` and posted
/// to a long-lived runner task so the recursion (force_cmd → dispatch →
/// script → force_cmd) crosses an mpsc boundary instead of an async-fn
/// call site. Without this indirection rustc cannot resolve the opaque
/// return-type cycle between `apply_script_outputs` and
/// `dispatch_command`.
pub struct ForceCmdMsg {
    pub player:  String,
    pub command: String,
    pub world:   Arc<Mutex<World>>,
    pub chars:   SharedChars,
}
pub static FORCE_CMD_TX: OnceLock<tokio::sync::mpsc::UnboundedSender<ForceCmdMsg>> = OnceLock::new();

/// Global wizlock level — players below this can't log in (except
/// immortals, who always bypass).  0 means unlocked.  Toggled by the
/// `wizlock` command; read by `login::Password` after a successful
/// auth check.
pub static WIZLOCK_LEVEL: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);

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
    "open", "close", "lock", "unlock", "pick", "search",
    "quaff", "drink", "eat", "recite", "use", "zap", "light", "extinguish",
    "follow", "group", "gtell", "title", "gossip", "chat",
    "auction", "auc", "whisper",
    "brief", "compact", "time", "weather", "bank", "reply", "prompt", "alias",
    "commands",
    "goto", "transfer", "purge", "shutdown", "stat", "force", "set", "wizlock",
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
    me.last_activity = std::time::Instant::now();

    // Expand a per-player alias once.  `alias bs bash`, type `bs goblin`
    // → "bash goblin".  Recursion is prevented by only consulting the
    // map for the original first token.
    let expanded: String;
    let raw: &str = {
        let first_end = raw.find(char::is_whitespace).unwrap_or(raw.len());
        let first    = &raw[..first_end];
        if let Some(rep) = me.aliases.get(first) {
            expanded = format!("{rep}{}", &raw[first_end..]);
            expanded.as_str()
        } else {
            raw
        }
    };

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
        Some("say")       => do_say_with_triggers(rest, me, chars, world).await,
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
        Some("wield")     => do_wield(rest, me, world, chars).await,
        Some("wear")      => do_wear(rest, me, world, chars).await,
        Some("remove")    => do_remove(rest, me, world, chars).await,
        Some("equipment") => do_equipment(me, world).await,
        Some("save")      => do_save(me, players).await,
        Some("help")      => do_help(rest, me, world).await,
        Some("exits")     => do_exits(me, world).await,
        Some("open")      => do_door(rest, me, world, chars, DoorOp::Open).await,
        Some("close")     => do_door(rest, me, world, chars, DoorOp::Close).await,
        Some("lock")      => do_door(rest, me, world, chars, DoorOp::Lock).await,
        Some("unlock")    => do_door(rest, me, world, chars, DoorOp::Unlock).await,
        Some("pick")      => do_pick(rest, me, world, chars).await,
        Some("search")    => do_search(me, world, chars).await,
        Some("quaff")     => do_quaff(rest, me, world, chars).await,
        Some("drink")     => do_drink_container(rest, me, world, chars).await,
        Some("eat")       => do_eat(rest, me, world, chars).await,
        Some("recite")    => do_recite(rest, me, world, chars).await,
        Some("use")       => do_use(rest, me, world, chars).await,
        Some("zap")       => do_zap(rest, me, world, chars).await,
        Some("light")     => do_light(rest, me, world, chars, true).await,
        Some("extinguish")=> do_light(rest, me, world, chars, false).await,
        Some("follow")    => do_follow(rest, me, chars).await,
        Some("group")     => do_group(rest, me, chars).await,
        Some("gtell")     => do_gtell(rest, me, chars).await,
        Some("title")     => do_title(rest, me),
        Some("gossip") | Some("chat") => do_gossip(rest, me, world, chars).await,
        Some("auction") | Some("auc") => do_auction(rest, me, world, chars).await,
        Some("whisper")   => do_whisper(rest, me, chars).await,
        Some("brief")     => do_brief(me),
        Some("compact")   => do_compact(me),
        Some("time")      => do_time(),
        Some("weather")   => do_weather(),
        Some("bank")      => do_bank(rest, me),
        Some("reply")     => do_reply(rest, me, chars).await,
        Some("prompt")    => do_prompt(rest, me),
        Some("alias")     => do_alias(rest, me),
        Some("commands")  => do_commands(),
        Some("goto")      => do_goto(rest, me, world, chars).await,
        Some("transfer")  => do_transfer(rest, me, world, chars).await,
        Some("purge")     => do_purge(me, world, chars).await,
        Some("shutdown")  => do_shutdown(me, chars).await,
        Some("stat")      => do_stat(rest, me, world, chars).await,
        Some("force")     => do_force(rest, me, world, chars).await,
        Some("set")       => do_set(rest, me, chars).await,
        Some("wizlock")   => do_wizlock(rest, me),
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
        _ => {
            // Fallback: dynamic social lookup against loaded socials.
            let social = {
                let w = world.lock().await;
                let lv = verb.to_ascii_lowercase();
                w.socials.iter()
                    .find(|s| s.name.eq_ignore_ascii_case(&lv)
                          || s.name.to_ascii_lowercase().starts_with(&lv))
                    .cloned()
            };
            if let Some(s) = social {
                return do_social(rest, me, chars, &s).await;
            }
            CmdOutput::text(format!("\r\nHuh?!? ({raw})\r\n"))
        }
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
    // Fire any GET triggers attached to the picked-up object.
    fire_obj_get_triggers(iid, &me.name, me.current_room, world, chars).await;
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

    {
        let cl = chars.lock().await;
        cl.broadcast_room(
            me.current_room, Some(me.id),
            &format!("{} drops {}.\r\n", me.name, name),
        );
    }
    // Fire any DROP triggers on the dropped object.
    fire_obj_drop_triggers(iid, &me.name, me.current_room, world, chars).await;

    CmdOutput::text(format!("\r\nYou drop {}.\r\n", name))
}

async fn do_say(
    arg: &str,
    me: &mut Character,
    chars: &SharedChars,
) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nYak yak yak...\r\n");
    }
    me.reveal();
    {
        let cl = chars.lock().await;
        cl.broadcast_room(
            me.current_room, Some(me.id),
            &format!("{} says, '{arg}'\r\n", me.name),
        );
    }
    CmdOutput::text(format!("\r\nYou say, '{arg}'\r\n"))
}

/// Public say wrapper used by the command dispatcher.  Fires any SPEECH
/// triggers in the room (mobs reacting to the player's words).
async fn do_say_with_triggers(
    arg: &str,
    me: &mut Character,
    chars: &SharedChars,
    world: &Arc<Mutex<World>>,
) -> CmdOutput {
    let out = do_say(arg, me, chars).await;
    if !arg.is_empty() {
        fire_mob_triggers(&me.name, me.current_room, 'd', Some(arg), world, chars).await;
        fire_room_speech_triggers(&me.name, me.current_room, arg, world, chars).await;
    }
    out
}

async fn do_tell(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    let (target, msg) = match arg.find(char::is_whitespace) {
        Some(i) => (&arg[..i], arg[i..].trim_start()),
        None    => return CmdOutput::text("\r\nTell whom what?\r\n"),
    };
    if msg.is_empty() {
        return CmdOutput::text("\r\nTell them what?\r\n");
    }
    let target_ph = {
        let cl = chars.lock().await;
        cl.find_by_name(target).filter(|p| p.id != me.id).cloned()
    };
    let Some(p) = target_ph else {
        return CmdOutput::text("\r\nNo one by that name is online.\r\n");
    };
    let _ = p.send.send(format!("{} tells you, '{msg}'\r\n", me.name));
    // Record who tell'd them so `reply` works.
    {
        let mut tc = p.character.lock().await;
        tc.last_tell_from = Some(me.name.clone());
    }
    CmdOutput::text(format!("\r\nYou tell {}, '{msg}'\r\n", p.name))
}

/// `reply <msg>` — send a tell back to the last person who tell'd us.
async fn do_reply(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    let msg = arg.trim();
    if msg.is_empty() {
        return CmdOutput::text("\r\nReply with what?\r\n".to_string());
    }
    let Some(name) = me.last_tell_from.clone() else {
        return CmdOutput::text("\r\nYou have no one to reply to.\r\n".to_string());
    };
    let combined = format!("{name} {msg}");
    do_tell(&combined, me, chars).await
}

fn do_commands() -> CmdOutput {
    let mut names: Vec<&str> = COMMANDS.iter().copied().collect();
    names.sort();
    let mut s = String::from("\r\nAvailable commands:\r\n");
    for chunk in names.chunks(5) {
        s.push_str("  ");
        for name in chunk {
            s.push_str(&format!("{name:<12}"));
        }
        s.push_str("\r\n");
    }
    CmdOutput::text(s)
}

// ---------------------------------------------------------------------------
// Socials — loaded from lib/misc/socials.new at boot into World.socials.
// dispatch_command falls back to a name lookup when the canonical-verb
// match misses, then routes through do_social with the resolved entry.
// ---------------------------------------------------------------------------

async fn do_social(
    arg: &str,
    me: &Character,
    chars: &SharedChars,
    s: &crate::world::Social,
) -> CmdOutput {
    let arg = arg.trim();
    let fill = |s: &str, target: &str| s.replace("$n", &me.name).replace("$N", target);

    if arg.is_empty() {
        if !s.actor_no_arg.is_empty() {
            let cl = chars.lock().await;
            if !s.room_no_arg.is_empty() {
                cl.broadcast_room(me.current_room, Some(me.id),
                    &format!("{}\r\n", fill(&s.room_no_arg, "")));
            }
            return CmdOutput::text(format!("\r\n{}\r\n", fill(&s.actor_no_arg, "")));
        }
        return CmdOutput::text(format!("\r\n{} who?\r\n", s.name));
    }
    let target_ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p|
            p.current_room == me.current_room
            && p.id != me.id
            && p.name.eq_ignore_ascii_case(arg)).cloned();
        h
    };
    let Some(ph) = target_ph else {
        return CmdOutput::text(format!("\r\nNo one named '{arg}' is here.\r\n"));
    };
    let to_actor  = fill(&s.actor_target,  &ph.name);
    let to_victim = fill(&s.victim_target, &ph.name);
    let to_room   = fill(&s.room_target,   &ph.name);
    if !to_victim.is_empty() {
        let _ = ph.send.send(format!("\r\n{to_victim}\r\n"));
    }
    if !to_room.is_empty() {
        let cl = chars.lock().await;
        for peer in cl.iter() {
            if peer.id == me.id || peer.id == ph.id { continue; }
            if peer.current_room != me.current_room { continue; }
            let _ = peer.send.send(format!("\r\n{to_room}\r\n"));
        }
    }
    CmdOutput::text(if to_actor.is_empty() {
        format!("\r\nYou {} at {}.\r\n", s.name, ph.name)
    } else {
        format!("\r\n{to_actor}\r\n")
    })
}

/// `alias` — list, set, or remove personal first-word command aliases.
/// `alias` lists the current set; `alias <name> <cmd>` sets;
/// `alias <name>` removes if present. Reserved verbs from the COMMANDS
/// table can be shadowed (no validation).
fn do_alias(arg: &str, me: &mut Character) -> CmdOutput {
    let parts: Vec<&str> = arg.splitn(2, char::is_whitespace).collect();
    if parts.is_empty() || parts[0].is_empty() {
        if me.aliases.is_empty() {
            return CmdOutput::text("\r\nYou have no aliases set.\r\n".to_string());
        }
        let mut names: Vec<&String> = me.aliases.keys().collect();
        names.sort();
        let mut s = String::from("\r\nYour aliases:\r\n");
        for n in names {
            s.push_str(&format!("  {n:<10}  {}\r\n", me.aliases[n]));
        }
        return CmdOutput::text(s);
    }
    let name = parts[0].to_ascii_lowercase();
    if parts.len() == 1 {
        if me.aliases.remove(&name).is_some() {
            CmdOutput::text(format!("\r\nAlias '{name}' removed.\r\n"))
        } else {
            CmdOutput::text(format!("\r\nYou have no alias called '{name}'.\r\n"))
        }
    } else {
        let exp: String = parts[1].trim()
            .chars().filter(|c| !c.is_control()).take(120).collect();
        if exp.is_empty() {
            me.aliases.remove(&name);
            return CmdOutput::text(format!("\r\nAlias '{name}' removed.\r\n"));
        }
        me.aliases.insert(name.clone(), exp.clone());
        CmdOutput::text(format!("\r\nAlias '{name}' → '{exp}'.\r\n"))
    }
}

/// `prompt <fmt>` — set a custom prompt template.  Empty or `-` clears
/// back to the legacy "> ".  Placeholders: %h/%H HP/maxHP, %m/%M
/// mana/maxMana, %g gold, %x exp, %% literal '%'.  Caps at 80 chars,
/// strips control bytes.
fn do_prompt(arg: &str, me: &mut Character) -> CmdOutput {
    let arg = arg.trim();
    if arg.is_empty() || arg == "-" {
        me.prompt_format.clear();
        return CmdOutput::text("\r\nPrompt reset to default.\r\n".to_string());
    }
    let sanitized: String = arg.chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    me.prompt_format = sanitized;
    CmdOutput::text(format!("\r\nPrompt set to: '{}'\r\n", me.prompt_format))
}

/// Substitute prompt placeholders against the player's current state.
pub fn render_prompt(me: &Character) -> String {
    if me.prompt_format.is_empty() {
        return if me.compact { "> ".to_string() } else { "\r\n> ".to_string() };
    }
    let mut out = String::with_capacity(me.prompt_format.len() + 16);
    let mut iter = me.prompt_format.chars().peekable();
    while let Some(c) = iter.next() {
        if c != '%' { out.push(c); continue; }
        let Some(&n) = iter.peek() else { out.push('%'); break; };
        iter.next();
        match n {
            'h' => out.push_str(&me.hp.to_string()),
            'H' => out.push_str(&me.max_hp.to_string()),
            'm' => out.push_str(&me.mana.to_string()),
            'M' => out.push_str(&me.max_mana.to_string()),
            'g' => out.push_str(&me.gold.to_string()),
            'x' => out.push_str(&me.exp.to_string()),
            '%' => out.push('%'),
            other => { out.push('%'); out.push(other); }
        }
    }
    if me.compact { out } else { format!("\r\n{out}") }
}

/// `follow <player>` — start trailing a leader; subsequent leader
/// movement drags this character along (see `do_move`).  `follow self`
/// or `follow stop` clears the relationship.
async fn do_follow(arg: &str, me: &mut Character, chars: &SharedChars) -> CmdOutput {
    let arg = arg.trim();
    if arg.is_empty() {
        if let Some(lid) = me.following {
            let cl = chars.lock().await;
            let name = cl.iter().find(|p| p.id == lid).map(|p| p.name.clone())
                .unwrap_or_else(|| "someone".to_string());
            return CmdOutput::text(format!("\r\nYou are following {name}.\r\n"));
        }
        return CmdOutput::text("\r\nYou are not following anyone.\r\n".to_string());
    }
    if arg.eq_ignore_ascii_case("self") || arg.eq_ignore_ascii_case("stop")
        || arg.eq_ignore_ascii_case(&me.name)
    {
        if let Some(lid) = me.following.take() {
            let leader = {
                let cl = chars.lock().await;
                let h = cl.iter().find(|p| p.id == lid).cloned();
                h
            };
            if let Some(leader) = leader {
                let _ = leader.send.send(format!("\r\n{} stops following you.\r\n", me.name));
            }
        }
        me.grouped = false;
        return CmdOutput::text("\r\nYou stop following anyone.\r\n".to_string());
    }
    // Resolve target — must be in same room.
    let cl = chars.lock().await;
    let Some(leader) = cl.iter().find(|p|
        p.id != me.id
        && p.current_room == me.current_room
        && p.name.eq_ignore_ascii_case(arg))
    else {
        return CmdOutput::text("\r\nThere is nobody here by that name.\r\n".to_string());
    };
    if me.following == Some(leader.id) {
        return CmdOutput::text(format!("\r\nYou are already following {}.\r\n", leader.name));
    }
    me.following = Some(leader.id);
    me.grouped   = false;          // require explicit `group` to share XP
    let _ = leader.send.send(format!("\r\n{} starts following you.\r\n", me.name));
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} starts following {}.\r\n", me.name, leader.name));
    CmdOutput::text(format!("\r\nYou start following {}.\r\n", leader.name))
}

/// `group`: list members.
/// `group <follower>`: leader toggles a follower into/out of the formal
/// group (eligibility: target must already be following `me`).  Solo
/// players cannot self-group.
async fn do_group(arg: &str, me: &mut Character, chars: &SharedChars) -> CmdOutput {
    let arg = arg.trim();
    if arg.is_empty() {
        // List the group: me + all online followers of me with grouped=true.
        let cl = chars.lock().await;
        let mut text = format!("\r\nYour group:\r\n  {}\r\n", me.name);
        let mut any = false;
        let handles: Vec<_> = cl.iter().cloned().collect();
        drop(cl);
        for ph in &handles {
            if ph.id == me.id { continue; }
            let c = ph.character.lock().await;
            if c.following == Some(me.id) && c.grouped {
                text.push_str(&format!("  {}\r\n", c.name));
                any = true;
            }
        }
        if !any { text.push_str("  (no group members)\r\n"); }
        return CmdOutput::text(text);
    }
    // Resolve target: must be following me.
    let cl = chars.lock().await;
    let Some(target) = cl.iter().find(|p| p.name.eq_ignore_ascii_case(arg)).cloned() else {
        return CmdOutput::text("\r\nNobody by that name is online.\r\n".to_string());
    };
    drop(cl);
    if target.id == me.id {
        return CmdOutput::text("\r\nYou can't group yourself.\r\n".to_string());
    }
    let mut tc = target.character.lock().await;
    if tc.following != Some(me.id) {
        return CmdOutput::text(format!("\r\n{} isn't following you.\r\n", tc.name));
    }
    tc.grouped = !tc.grouped;
    let joined = tc.grouped;
    let msg_them = if joined {
        format!("\r\n{} adds you to the group.\r\n", me.name)
    } else {
        format!("\r\n{} removes you from the group.\r\n", me.name)
    };
    let _ = target.send.send(msg_them);
    me.grouped = true;             // leader is implicitly in their own group
    CmdOutput::text(format!(
        "\r\nYou {} {} {} your group.\r\n",
        if joined { "add" } else { "remove" },
        tc.name,
        if joined { "to" } else { "from" },
    ))
}

/// `gtell <message>`: broadcast to all online characters who share a
/// group with the sender (their leader or any grouped follower of the
/// shared leader).
async fn do_gtell(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    let msg = arg.trim();
    if msg.is_empty() {
        return CmdOutput::text("\r\nGroup-tell what?\r\n".to_string());
    }
    if !me.grouped {
        return CmdOutput::text("\r\nYou're not in any group.\r\n".to_string());
    }
    // Determine the group leader id: if I'm following someone with grouped,
    // they're the leader; otherwise I am.
    let leader_id = me.following.unwrap_or(me.id);
    let formatted = format!("\r\n{} group-tells, '{msg}'\r\n", me.name);
    let cl = chars.lock().await;
    let handles: Vec<_> = cl.iter().cloned().collect();
    drop(cl);
    let mut delivered = 0;
    for ph in &handles {
        if ph.id == me.id { continue; }
        let c = ph.character.lock().await;
        let in_group = (c.id == leader_id && c.grouped)
            || (c.following == Some(leader_id) && c.grouped);
        if in_group {
            let _ = ph.send.send(formatted.clone());
            delivered += 1;
        }
    }
    let _ = delivered;
    CmdOutput::text(format!("\r\nYou group-tell, '{msg}'\r\n"))
}

fn do_time() -> CmdOutput {
    use std::sync::atomic::Ordering;
    let h = crate::db::GAME_HOUR.load(Ordering::Relaxed);
    let d = crate::db::GAME_DAY.load(Ordering::Relaxed);
    let m = crate::db::GAME_MONTH.load(Ordering::Relaxed);
    let y = crate::db::GAME_YEAR.load(Ordering::Relaxed);

    // 12-hour clock with am/pm.
    let (hour12, suffix) = match h {
        0     => (12, "am"),
        1..=11=> (h, "am"),
        12    => (12, "pm"),
        _     => (h - 12, "pm"),
    };
    let month_name = crate::db::MONTH_NAMES
        .get(m as usize).copied().unwrap_or("Unknown");
    let period = match h {
        5..=8   => "early morning",
        9..=11  => "morning",
        12      => "noon",
        13..=17 => "afternoon",
        18..=20 => "evening",
        21..=23 => "night",
        _       => "deep night",
    };
    CmdOutput::text(format!(
        "\r\nIt is {hour12}{suffix}, {period}.\r\nIt is the {}{} day of the {month_name}, year {y}.\r\n",
        d + 1,
        match (d + 1) % 10 { 1 if d + 1 != 11 => "st", 2 if d + 1 != 12 => "nd", 3 if d + 1 != 13 => "rd", _ => "th" },
    ))
}

fn do_weather() -> CmdOutput {
    use std::sync::atomic::Ordering;
    let h = crate::db::GAME_HOUR.load(Ordering::Relaxed);
    let m = crate::db::GAME_MONTH.load(Ordering::Relaxed);
    // Pseudo-randomly varied by hour + month — same time of same day
    // always shows the same weather (stable within a tick).
    let seed = (h.wrapping_mul(7) + m.wrapping_mul(31)) & 0xff;
    let desc = match seed % 5 {
        0 => "The sky is cloudless and bright.",
        1 => "Wispy clouds drift through a clear sky.",
        2 => "Grey clouds blanket the sky.",
        3 => "Cold rain falls in sheets.",
        _ => "A fierce storm rages overhead, lightning crackling in the distance.",
    };
    let lit = if (6..20).contains(&h) { "It is daytime." } else { "It is night." };
    CmdOutput::text(format!("\r\n{lit}\r\n{desc}\r\n"))
}

/// `bank [balance | deposit N | withdraw N]` — manage gold on deposit.
/// `balance` (or no-arg) shows both balances.  No banker-mob gating
/// yet; available anywhere.
fn do_bank(arg: &str, me: &mut Character) -> CmdOutput {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let sub = parts.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();

    if sub.is_empty() || sub == "balance" || sub == "bal" {
        return CmdOutput::text(format!(
            "\r\nBank balance: {} gold\r\nCarrying:     {} gold\r\n",
            me.bank_gold, me.gold,
        ));
    }
    let amount = match parts.get(1).and_then(|v| v.parse::<i64>().ok()) {
        Some(n) if n > 0 => n,
        _ => return CmdOutput::text("\r\nUsage: bank [balance | deposit <N> | withdraw <N>]\r\n".to_string()),
    };
    match sub.as_str() {
        "deposit" | "dep" => {
            if amount > me.gold {
                return CmdOutput::text(format!(
                    "\r\nYou only carry {} gold.\r\n", me.gold,
                ));
            }
            me.gold       -= amount;
            me.bank_gold  += amount;
            CmdOutput::text(format!(
                "\r\nYou deposit {amount} gold. (Bank: {} | Carry: {})\r\n",
                me.bank_gold, me.gold,
            ))
        }
        "withdraw" | "with" => {
            if amount > me.bank_gold {
                return CmdOutput::text(format!(
                    "\r\nYou only have {} on deposit.\r\n", me.bank_gold,
                ));
            }
            me.bank_gold  -= amount;
            me.gold       += amount;
            CmdOutput::text(format!(
                "\r\nYou withdraw {amount} gold. (Bank: {} | Carry: {})\r\n",
                me.bank_gold, me.gold,
            ))
        }
        _ => CmdOutput::text("\r\nUsage: bank [balance | deposit <N> | withdraw <N>]\r\n".to_string()),
    }
}

/// `auction <msg>` — yellow global trade channel.  Same shape as gossip
/// but uses `auction_off` for the personal mute.  Refused in SOUNDPROOF
/// rooms.
async fn do_auction(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let msg = arg.trim();
    if msg.is_empty() {
        me.auction_off = !me.auction_off;
        return CmdOutput::text(format!(
            "\r\nAuction channel: {}.\r\n",
            if me.auction_off { "off" } else { "on" },
        ));
    }
    if me.auction_off {
        return CmdOutput::text("\r\nYou have the auction channel turned off.\r\n".to_string());
    }
    {
        let w = world.lock().await;
        if w.rooms.get(&me.current_room)
            .map(|r| r.room_flags[0] & crate::world::ROOM_SOUNDPROOF != 0)
            .unwrap_or(false)
        {
            return CmdOutput::text(
                "\r\nThe walls dampen your voice — no one outside can hear you.\r\n".to_string()
            );
        }
    }
    let formatted = format!("\r\n@Y{} auctions: '{msg}'@n\r\n", me.name);
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    for ph in &handles {
        if ph.id == me.id { continue; }
        let off = ph.character.lock().await.auction_off;
        if off { continue; }
        let _ = ph.send.send(formatted.clone());
    }
    CmdOutput::text(format!("\r\n@YYou auction, '{msg}'@n\r\n"))
}

/// `whisper <player> <msg>` — private same-room speech.  The named
/// player and the sender see the full text; everyone else in the room
/// sees "Name whispers something to Target." without content.
async fn do_whisper(
    arg: &str,
    me: &Character,
    chars: &SharedChars,
) -> CmdOutput {
    let (target, msg) = match arg.find(char::is_whitespace) {
        Some(i) => (arg[..i].trim(), arg[i..].trim()),
        None    => return CmdOutput::text("\r\nWhisper to whom what?\r\n".to_string()),
    };
    if target.is_empty() || msg.is_empty() {
        return CmdOutput::text("\r\nWhisper to whom what?\r\n".to_string());
    }
    let ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p|
            p.current_room == me.current_room
            && p.name.eq_ignore_ascii_case(target)
            && p.id != me.id).cloned();
        h
    };
    let Some(ph) = ph else {
        return CmdOutput::text("\r\nThere is no one by that name here.\r\n".to_string());
    };
    let _ = ph.send.send(format!("\r\n{} whispers to you, '{msg}'\r\n", me.name));
    // Manually broadcast to room peers EXCEPT me and the recipient —
    // they each get their own copy above.
    let cl = chars.lock().await;
    let line = format!("\r\n{} whispers something to {}.\r\n", me.name, ph.name);
    for peer in cl.iter() {
        if peer.id == me.id || peer.id == ph.id { continue; }
        if peer.current_room != me.current_room { continue; }
        let _ = peer.send.send(line.clone());
    }
    CmdOutput::text(format!("\r\nYou whisper to {}, '{msg}'\r\n", ph.name))
}

fn do_brief(me: &mut Character) -> CmdOutput {
    me.brief = !me.brief;
    CmdOutput::text(format!(
        "\r\nBrief mode: {}.\r\n",
        if me.brief { "on" } else { "off" },
    ))
}

fn do_compact(me: &mut Character) -> CmdOutput {
    me.compact = !me.compact;
    CmdOutput::text(format!(
        "\r\nCompact prompt: {}.\r\n",
        if me.compact { "on" } else { "off" },
    ))
}

/// `gossip <msg>` — global chat channel.  Empty arg toggles the
/// sender's personal `gossip_off` state (mutes both incoming and
/// outgoing).  Refused when the sender's room has ROOM_SOUNDPROOF.
/// Receivers with their own `gossip_off` set are skipped.
async fn do_gossip(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let msg = arg.trim();
    if msg.is_empty() {
        me.gossip_off = !me.gossip_off;
        return CmdOutput::text(format!(
            "\r\nGossip channel: {}.\r\n",
            if me.gossip_off { "off" } else { "on" },
        ));
    }
    if me.gossip_off {
        return CmdOutput::text("\r\nYou have the gossip channel turned off.\r\n".to_string());
    }
    // Soundproof room: refuse.
    {
        let w = world.lock().await;
        if w.rooms.get(&me.current_room)
            .map(|r| r.room_flags[0] & crate::world::ROOM_SOUNDPROOF != 0)
            .unwrap_or(false)
        {
            return CmdOutput::text(
                "\r\nThe walls dampen your voice — no one outside can hear you.\r\n".to_string()
            );
        }
    }
    let formatted = format!("\r\n@c{} gossips: '{msg}'@n\r\n", me.name);
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    for ph in &handles {
        if ph.id == me.id { continue; }
        let off = ph.character.lock().await.gossip_off;
        if off { continue; }
        let _ = ph.send.send(formatted.clone());
    }
    CmdOutput::text(format!("\r\n@cYou gossip, '{msg}'@n\r\n"))
}

// ---------------------------------------------------------------------------
// Immortal toolkit (LVL_IMMORT = 34+).  Gated on me.level >= LVL_IMMORT;
// mortals get "Huh?!" so the existence of the verb stays hidden.
// ---------------------------------------------------------------------------

const LVL_IMMORT: i32 = 34;

fn immort_huh() -> CmdOutput {
    CmdOutput::text("\r\nHuh?!\r\n".to_string())
}

/// `goto <room-vnum|player>` — teleport to a room (by vnum) or to a
/// player's current room.  Broadcasts a disappear/appear pair so other
/// players can react.
async fn do_goto(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nGoto where?\r\n".to_string());
    }
    // Numeric → room vnum. Otherwise, look up a player by name.
    let target_room = if let Ok(vnum) = arg.parse::<i32>() {
        // Validate the room exists.
        let w = world.lock().await;
        if !w.rooms.contains_key(&vnum) {
            return CmdOutput::text(format!("\r\nNo room with vnum {vnum}.\r\n"));
        }
        vnum
    } else {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.name.eq_ignore_ascii_case(arg)).cloned();
        match h {
            Some(p) => p.current_room,
            None    => return CmdOutput::text("\r\nNobody by that name is online.\r\n".to_string()),
        }
    };
    let from_room = me.current_room;
    if target_room == from_room {
        return CmdOutput::text("\r\nYou're already there.\r\n".to_string());
    }
    me.current_room = target_room;
    {
        let mut cl = chars.lock().await;
        cl.update_room(me.id, target_room);
        cl.broadcast_room(from_room, Some(me.id),
            &format!("{} vanishes in a puff of smoke.\r\n", me.name));
        cl.broadcast_room(target_room, Some(me.id),
            &format!("{} arrives in a puff of smoke.\r\n", me.name));
    }
    let view = render_room(target_room, Some(me.id), world, chars).await;
    CmdOutput::text(view)
}

/// `transfer <player>` — yank the named player to the caller's room.
/// Refuses on self and on offline targets.
async fn do_transfer(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nTransfer whom?\r\n".to_string());
    }
    let ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.name.eq_ignore_ascii_case(arg)).cloned();
        h
    };
    let Some(ph) = ph else {
        return CmdOutput::text("\r\nNobody by that name is online.\r\n".to_string());
    };
    if ph.id == me.id {
        return CmdOutput::text("\r\nThat doesn't make sense.\r\n".to_string());
    }
    let to_room = me.current_room;
    let from_room = {
        let mut c = ph.character.lock().await;
        let f = c.current_room;
        c.current_room = to_room;
        f
    };
    {
        let mut cl = chars.lock().await;
        cl.update_room(ph.id, to_room);
        cl.broadcast_room(from_room, Some(ph.id),
            &format!("{} is summoned away by an unseen force.\r\n", ph.name));
        cl.broadcast_room(to_room, Some(ph.id),
            &format!("{} arrives, summoned by {}.\r\n", ph.name, me.name));
    }
    let _ = ph.send.send(format!("\r\n{} summons you.\r\n", me.name));
    let view = render_room(to_room, Some(ph.id), world, chars).await;
    let _ = ph.send.send(view);
    CmdOutput::text(format!("\r\nYou transfer {} here.\r\n", ph.name))
}

/// `purge` — extract every mob and floor object in the caller's room.
/// Carried/equipped items are untouched; mob inventories are extracted
/// alongside the mob (matches stock CircleMUD).
async fn do_purge(
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let room = me.current_room;
    let (n_mobs, n_objs) = {
        let mut w = world.lock().await;
        // Snapshot lists so we can drop the room borrow before mutating
        // the parallel vectors.
        let mobs: Vec<u32> = w.rooms.get(&room).map(|r| r.mobs.clone()).unwrap_or_default();
        let objs: Vec<u32> = w.rooms.get(&room).map(|r| r.objects.clone()).unwrap_or_default();
        // Drop in-room references first.
        if let Some(r) = w.rooms.get_mut(&room) {
            r.mobs.clear();
            r.objects.clear();
        }
        // For each mob, also extract its inventory.
        let mut mob_invs: Vec<u32> = Vec::new();
        for &mid in &mobs {
            if let Some(m) = w.mob_instances.iter().find(|m| m.id == mid) {
                mob_invs.extend(m.inventory.iter().copied());
            }
        }
        w.mob_instances.retain(|m| !mobs.contains(&m.id));
        w.obj_instances.retain(|o| !objs.contains(&o.id) && !mob_invs.contains(&o.id));
        (mobs.len(), objs.len())
    };
    let cl = chars.lock().await;
    cl.broadcast_room(room, Some(me.id),
        &format!("{} disintegrates everything in the room with a wave.\r\n", me.name));
    CmdOutput::text(format!(
        "\r\nPurged: {} mobs, {} floor objects.\r\n", n_mobs, n_objs
    ))
}

/// `shutdown` — graceful exit. Broadcasts a notice to every online
/// player, then `std::process::exit(0)`. We don't currently have a
/// "save everyone" hook on shutdown (auto-save handles ongoing state).
async fn do_shutdown(me: &Character, chars: &SharedChars) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let notice = format!("\r\n*** {} is shutting down the world. ***\r\n", me.name);
    {
        let cl = chars.lock().await;
        for ph in cl.iter() {
            let _ = ph.send.send(notice.clone());
        }
    }
    tracing::warn!(by = %me.name, "Shutdown requested by immortal");
    // Give the writer tasks a moment to flush the notice.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    std::process::exit(0);
}

/// `help [topic]` — look up a topic in the loaded help database.
/// Without an argument, falls back to the original built-in summary so
/// brand-new players can still discover commands when help.hlp is
/// missing. Matching is case-insensitive prefix on any keyword; the
/// first matching entry whose `min_level <= me.level` wins.
async fn do_help(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
) -> CmdOutput {
    let fallback = "\r\nAvailable: look, get, drop, inv, wield, wear, remove, \
        equip, kill, flee, say, tell, who, score, save, quit, n/e/s/w/u/d.\r\n";
    let topic = arg.trim();
    if topic.is_empty() {
        return CmdOutput::text(fallback);
    }
    let needle = topic.to_ascii_uppercase();
    let w = world.lock().await;
    if w.help.is_empty() {
        return CmdOutput::text(fallback);
    }
    let matched = w.help.iter().find(|e|
        e.min_level <= me.level
        && e.keywords.iter().any(|k| k.starts_with(&needle))
    );
    match matched {
        Some(e) => CmdOutput::text(format!("\r\n{}\r\n", e.body.trim_end())),
        None    => CmdOutput::text(format!("\r\nThere is no help on '{topic}'.\r\n")),
    }
}

/// `force <player> <command>` — immortal-only. Dispatches the command
/// as the named online player via the existing FORCE_CMD_TX channel
/// (same plumbing as the `mforce` script verb in cp30). Notifies the
/// target before the dispatch so they see the coercion.
async fn do_force(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let (target, command) = match arg.find(char::is_whitespace) {
        Some(i) => (arg[..i].trim(), arg[i..].trim()),
        None    => return CmdOutput::text("\r\nForce whom to do what?\r\n".to_string()),
    };
    if target.is_empty() || command.is_empty() {
        return CmdOutput::text("\r\nForce whom to do what?\r\n".to_string());
    }
    let ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.name.eq_ignore_ascii_case(target)).cloned();
        h
    };
    let Some(ph) = ph else {
        return CmdOutput::text("\r\nNobody by that name is online.\r\n".to_string());
    };
    let _ = ph.send.send(format!("\r\n{} forces you to '{}'.\r\n", me.name, command));
    let Some(tx) = FORCE_CMD_TX.get() else {
        return CmdOutput::text("\r\nForce dispatch channel is unavailable.\r\n".to_string());
    };
    let _ = tx.send(ForceCmdMsg {
        player:  ph.name.clone(),
        command: command.to_string(),
        world:   Arc::clone(world),
        chars:   Arc::clone(chars),
    });
    CmdOutput::text(format!("\r\nYou force {} to '{}'.\r\n", ph.name, command))
}

/// `set <player> <field> <value>` — immortal-only. Supports a handful
/// of common fields: level / hp / maxhp / mana / maxmana / gold / exp /
/// room. Room change updates the registry and broadcasts a "vanishes"
/// / "appears" pair so other players see the move.
async fn do_set(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let parts: Vec<&str> = arg.split_whitespace().collect();
    if parts.len() < 3 {
        return CmdOutput::text("\r\nUsage: set <player> <field> <value>\r\n  Fields: level hp maxhp mana maxmana gold exp room title\r\n".to_string());
    }
    let target = parts[0];
    let field  = parts[1].to_ascii_lowercase();
    let value_str = parts[2..].join(" ");

    let ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.name.eq_ignore_ascii_case(target)).cloned();
        h
    };
    let Some(ph) = ph else {
        return CmdOutput::text("\r\nNobody by that name is online.\r\n".to_string());
    };

    // Integer fields share a parser.
    let parse_i = || value_str.parse::<i64>().ok();
    let parse_i32 = || value_str.parse::<i32>().ok();

    match field.as_str() {
        "title" => {
            let mut c = ph.character.lock().await;
            c.title = value_str.chars().filter(|c| !c.is_control()).take(60).collect();
            CmdOutput::text(format!("\r\nSet {}'s title to '{}'.\r\n", ph.name, c.title))
        }
        "level" => {
            let Some(v) = parse_i32() else { return bad_value(&field); };
            ph.character.lock().await.level = v.clamp(1, 34);
            CmdOutput::text(format!("\r\nSet {}'s level to {v}.\r\n", ph.name))
        }
        "hp" => {
            let Some(v) = parse_i32() else { return bad_value(&field); };
            let mut c = ph.character.lock().await;
            c.hp = v.max(0).min(c.max_hp.max(v));
            CmdOutput::text(format!("\r\nSet {}'s HP to {}.\r\n", ph.name, c.hp))
        }
        "maxhp" => {
            let Some(v) = parse_i32() else { return bad_value(&field); };
            let mut c = ph.character.lock().await;
            c.max_hp = v.max(1);
            c.hp = c.hp.min(c.max_hp);
            CmdOutput::text(format!("\r\nSet {}'s max HP to {}.\r\n", ph.name, c.max_hp))
        }
        "mana" => {
            let Some(v) = parse_i32() else { return bad_value(&field); };
            let mut c = ph.character.lock().await;
            c.mana = v.max(0).min(c.max_mana.max(v));
            CmdOutput::text(format!("\r\nSet {}'s mana to {}.\r\n", ph.name, c.mana))
        }
        "maxmana" => {
            let Some(v) = parse_i32() else { return bad_value(&field); };
            let mut c = ph.character.lock().await;
            c.max_mana = v.max(0);
            c.mana = c.mana.min(c.max_mana);
            CmdOutput::text(format!("\r\nSet {}'s max mana to {}.\r\n", ph.name, c.max_mana))
        }
        "gold" => {
            let Some(v) = parse_i() else { return bad_value(&field); };
            ph.character.lock().await.gold = v.max(0);
            CmdOutput::text(format!("\r\nSet {}'s gold to {v}.\r\n", ph.name))
        }
        "exp" => {
            let Some(v) = parse_i() else { return bad_value(&field); };
            ph.character.lock().await.exp = v.max(0);
            CmdOutput::text(format!("\r\nSet {}'s exp to {v}.\r\n", ph.name))
        }
        "room" => {
            let Some(v) = parse_i32() else { return bad_value(&field); };
            let from = {
                let mut c = ph.character.lock().await;
                let f = c.current_room;
                c.current_room = v;
                f
            };
            let mut cl = chars.lock().await;
            cl.update_room(ph.id, v);
            cl.broadcast_room(from, Some(ph.id),
                &format!("{} vanishes by divine command.\r\n", ph.name));
            cl.broadcast_room(v, Some(ph.id),
                &format!("{} appears by divine command.\r\n", ph.name));
            let _ = ph.send.send(format!(
                "\r\n{} sends you to room {v}.\r\n", me.name,
            ));
            CmdOutput::text(format!("\r\nMoved {} to room {v}.\r\n", ph.name))
        }
        _ => CmdOutput::text(format!("\r\nUnknown field '{field}'.\r\n")),
    }
}

fn bad_value(field: &str) -> CmdOutput {
    CmdOutput::text(format!("\r\nBad value for '{field}'.\r\n"))
}

/// `wizlock [level]` — show or set the global login threshold.
/// 0 unlocks; any positive value blocks logins below it (immortals
/// always bypass).  Immortal-only.
fn do_wizlock(arg: &str, me: &Character) -> CmdOutput {
    use std::sync::atomic::Ordering;
    if me.level < LVL_IMMORT { return immort_huh(); }
    let arg = arg.trim();
    if arg.is_empty() {
        let cur = WIZLOCK_LEVEL.load(Ordering::Relaxed);
        return CmdOutput::text(if cur <= 0 {
            "\r\nWizlock is currently OFF.\r\n".to_string()
        } else {
            format!("\r\nWizlock is set to level {cur}.\r\n")
        });
    }
    let Ok(v) = arg.parse::<i32>() else {
        return CmdOutput::text("\r\nUsage: wizlock [<level>]   (0 disables)\r\n".to_string());
    };
    let v = v.clamp(0, 34);
    WIZLOCK_LEVEL.store(v, Ordering::Relaxed);
    CmdOutput::text(if v == 0 {
        "\r\nWizlock disabled.\r\n".to_string()
    } else {
        format!("\r\nWizlock set to level {v}.\r\n")
    })
}

/// `stat [name]` — inspect a player/mob/obj/room.  With no arg or
/// "room", dump the caller's current room. Otherwise auto-detect by
/// the same priority C uses: player name → mob keyword in room →
/// object keyword in inventory/equip/room.
async fn do_stat(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let arg = arg.trim();

    // No arg or "room" → describe the current room.
    if arg.is_empty() || arg.eq_ignore_ascii_case("room") {
        let w = world.lock().await;
        let r = match w.rooms.get(&me.current_room) {
            Some(r) => r,
            None    => return CmdOutput::text("\r\nYou're nowhere statable.\r\n".to_string()),
        };
        let mut s = format!(
            "\r\nRoom [{}] (zone {})\r\n  Name:     {}\r\n  Flags:    0x{:x}\r\n  Sector:   {}\r\n  Mobs:     {}\r\n  Objects:  {}\r\n",
            r.vnum, r.zone, r.name, r.room_flags[0], r.sector_type, r.mobs.len(), r.objects.len(),
        );
        s.push_str("  Exits:    ");
        let mut any = false;
        for d in crate::world::Direction::ALL {
            if let Some(e) = &r.exits[d as usize] {
                if e.to_room == crate::world::NOWHERE { continue; }
                if any { s.push_str(", "); }
                s.push_str(&format!("{}→{}", d.name(), e.to_room));
                any = true;
            }
        }
        if !any { s.push_str("(none)"); }
        s.push_str("\r\n");
        if !r.triggers.is_empty() {
            s.push_str(&format!("  Triggers: {:?}\r\n", r.triggers));
        }
        return CmdOutput::text(s);
    }

    // Try player.
    let player_handle = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.name.eq_ignore_ascii_case(arg)).cloned();
        h
    };
    if let Some(ph) = player_handle {
        let c = ph.character.lock().await;
        let s = format!(
            "\r\nPlayer [{}] {}\r\n  Class:    {:?}\r\n  Level:    {}\r\n  HP:       {}/{}\r\n  Mana:     {}/{}\r\n  Exp:      {}\r\n  Gold:     {}\r\n  Room:     {}\r\n  Str/Dex/Int/Wis/Con/Cha: {}/{}/{}/{}/{}/{}\r\n  Bonus hr/dr/ac: {}/{}/{}\r\n  Following:{}  Grouped:{}\r\n  Following:{:?}  Skills:{}\r\n",
            ph.id, c.name, c.class, c.level, c.hp, c.max_hp, c.mana, c.max_mana,
            c.exp, c.gold, c.current_room,
            c.str_, c.dex, c.int_, c.wis, c.con, c.cha,
            c.bonus_hitroll, c.bonus_damroll, c.bonus_ac,
            c.following.is_some(), c.grouped,
            c.following, c.skills.len(),
        );
        return CmdOutput::text(s);
    }

    // Try mob in current room by keyword.
    let key = arg.to_ascii_lowercase();
    let w = world.lock().await;
    let mob_info = w.rooms.get(&me.current_room).and_then(|r| {
        r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                Some((m, p))
            } else { None }
        })
    });
    if let Some((m, p)) = mob_info {
        let s = format!(
            "\r\nMob [vnum {}] iid {}  \"{}\"\r\n  Level:    {}\r\n  HP:       {}/{}\r\n  AC:       {}\r\n  Hitroll:  {}\r\n  Damage:   {}d{}+{}\r\n  Exp:      {}\r\n  Gold:     {}\r\n  Flags:    0x{:x}\r\n  Affs:     0x{:x}\r\n  In room:  {}\r\n  Fighting: {:?}\r\n  Inventory:{} items   Triggers:{:?}\r\n",
            m.vnum, m.id, p.short_descr, p.level, m.hp, m.max_hp, p.ac, p.hitroll,
            p.dam_dice, p.dam_size, p.damroll,
            p.exp, p.gold, p.mob_flags[0], p.aff_flags[0], m.in_room,
            m.fighting, m.inventory.len(), m.triggers,
        );
        return CmdOutput::text(s);
    }

    // Try object (inventory → equipment → current room floor).
    let obj_info = {
        let pool: Vec<u32> = me.inventory.iter().copied()
            .chain(me.equipment.iter().filter_map(|s| s.as_ref()).copied())
            .chain(w.rooms.get(&me.current_room).map(|r| r.objects.clone()).unwrap_or_default())
            .collect();
        pool.into_iter().find_map(|iid| {
            let o = w.obj_instances.iter().find(|o| o.id == iid)?;
            let p = w.obj_protos.get(&o.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                Some((o.clone(), p.clone()))
            } else { None }
        })
    };
    if let Some((o, p)) = obj_info {
        let s = format!(
            "\r\nObject [vnum {}] iid {}  \"{}\"\r\n  Type:     {}\r\n  Value:    {} {} {} {}\r\n  Weight:   {}\r\n  Cost:     {}\r\n  Level:    {}\r\n  Extra:    0x{:x}\r\n  Wear:     0x{:x}\r\n  Affect:   0x{:x}\r\n  Timer:    {:?}  Decay:{:?}  Lit:{}\r\n  In room:  {}\r\n  Affects:  {:?}\r\n  Triggers: {:?}\r\n",
            o.vnum, o.id, p.short_description, item_type_name(p.item_type),
            p.value[0], p.value[1], p.value[2], p.value[3],
            p.weight, p.cost, p.level,
            p.extra_flags[0], p.wear_flags[0], p.affect_flags[0],
            o.timer, o.decay_in, o.light_lit,
            o.in_room, p.affected, o.triggers,
        );
        return CmdOutput::text(s);
    }

    CmdOutput::text("\r\nNo one and nothing matches that here.\r\n".to_string())
}

/// `title <text>` — set the vanity title shown after your name on
/// `who` and `score`.  Empty arg or "-" clears.  Cap at 60 chars,
/// strip control bytes to keep the listing tidy.
fn do_title(arg: &str, me: &mut Character) -> CmdOutput {
    let arg = arg.trim();
    if arg.is_empty() || arg == "-" {
        me.title.clear();
        return CmdOutput::text("\r\nTitle cleared.\r\n".to_string());
    }
    let sanitized: String = arg.chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect();
    if sanitized.is_empty() {
        return CmdOutput::text("\r\nTitle was empty after stripping control bytes.\r\n".to_string());
    }
    me.title = sanitized;
    CmdOutput::text(format!("\r\nTitle set: {}\r\n", me.title))
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
    // Snapshot titles outside the registry lock so we don't serialize on
    // contended Character mutexes.
    let titles: Vec<(u32, String)> = {
        let cl = chars.lock().await;
        let handles: Vec<_> = cl.iter().cloned().collect();
        drop(cl);
        let mut out = Vec::new();
        for ph in handles {
            let t = ph.character.lock().await.title.clone();
            out.push((ph.id, t));
        }
        out
    };
    let cl = chars.lock().await;
    let mut s = String::from("\r\nPlayers online:\r\n");
    let mut count = 0;
    for p in cl.iter() {
        let marker = if p.id == me.id { " (you)" } else { "" };
        let title = titles.iter().find(|(id, _)| *id == p.id).map(|(_, t)| t.as_str()).unwrap_or("");
        let title_str = if title.is_empty() { String::new() } else { format!(" {title}") };
        s.push_str(&format!("  [{:>2}] {}{}{}\r\n", p.level, p.name, title_str, marker));
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
    let food = if me.hunger < 0 { "satisfied".to_string() }
               else if me.hunger == 0 { "starving".to_string() }
               else { format!("{}/{} hours", me.hunger, MAX_HUNGER) };
    let drink = if me.thirst < 0 { "satisfied".to_string() }
                else if me.thirst == 0 { "parched".to_string() }
                else { format!("{}/{} hours", me.thirst, MAX_THIRST) };
    let name_line = if me.title.is_empty() {
        format!("Name:  {}", me.name)
    } else {
        format!("Name:  {} {}", me.name, me.title)
    };
    let s = format!(
        "\r\n{name_line}\r\nLevel: {}\r\nExp:   {exp_str}\r\nHP:    {}/{}\r\nMana:  {}/{}\r\nClass: {:?}\r\nSex:   {:?}\r\nGold:  {}\r\nRoom:  {}\r\nAC:    {}\r\nPrac:  {}\r\nFood:  {food}\r\nDrink: {drink}\r\n\
         Str/Int/Wis/Dex/Con/Cha: {}/{}/{}/{}/{}/{}\r\n",
        me.level, me.hp, me.max_hp, me.mana, me.max_mana,
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
            fire_obj_load_triggers(iid, &me.name, me.current_room, world, chars).await;
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
/// mark the objective complete and return a player-facing message.  If
/// they have an AQ_ROOM_CLEAR quest targeting `kill_room`, completes
/// when no mobs remain in that room after this kill.
pub async fn quest_check_kill(
    me: &mut Character,
    killed_vnum: i32,
    world: &Arc<Mutex<World>>,
) -> Option<String> {
    let qv = me.active_quest?;
    let w = world.lock().await;
    let q = w.quests.get(&qv)?;
    if me.quest_progress >= 1 { return None; }

    if q.kind == crate::world::AQ_MOB_KILL && q.target == killed_vnum {
        me.quest_progress = 1;
        let mob_name = w.mob_protos.get(&killed_vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the target".to_string());
        return Some(format!(
            "\r\n*** Quest objective complete: you have slain {mob_name}! Return to the questmaster. ***\r\n",
        ));
    }
    if q.kind == crate::world::AQ_ROOM_CLEAR {
        // Player must be IN the target room and no mobs may remain there
        // after this kill (the killed mob is already extracted by the
        // time we're called).
        let target_room = q.target;
        if me.current_room != target_room { return None; }
        let mobs_remaining = w.rooms.get(&target_room)
            .map(|r| r.mobs.len()).unwrap_or(0);
        if mobs_remaining == 0 {
            me.quest_progress = 1;
            let room_name = w.rooms.get(&target_room)
                .map(|r| r.name.clone())
                .unwrap_or_else(|| "the area".to_string());
            return Some(format!(
                "\r\n*** Quest objective complete: you have cleared {room_name}! Return to the questmaster. ***\r\n",
            ));
        }
    }
    None
}

/// AQ_MOB_SAVE: after the player kills any mob, completes when the
/// target rescue-mob is still alive in the player's current room AND no
/// other non-charmed NPCs remain in that room.  Mirrors tbaMUD's
/// quest.c:400 — the target survives because all attackers were
/// dispatched.
pub async fn quest_check_save(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
) -> Option<String> {
    let qv = me.active_quest?;
    let w = world.lock().await;
    let q = w.quests.get(&qv)?;
    if q.kind != crate::world::AQ_MOB_SAVE { return None; }
    if me.quest_progress >= 1 { return None; }
    let target_vnum = q.target;
    let r = w.rooms.get(&me.current_room)?;
    // The target mob must be present in the room.  We treat any mob
    // instance with the target vnum as alive — extracted mobs aren't in
    // r.mobs anymore.
    let target_present = r.mobs.iter()
        .filter_map(|&id| w.mob_instances.iter().find(|m| m.id == id))
        .any(|m| m.vnum == target_vnum);
    if !target_present { return None; }
    // No other mobs (i.e., the target's attackers) may remain.
    let intruder = r.mobs.iter()
        .filter_map(|&id| w.mob_instances.iter().find(|m| m.id == id))
        .any(|m| m.vnum != target_vnum);
    if intruder { return None; }
    me.quest_progress = 1;
    let mob_name = w.mob_protos.get(&target_vnum)
        .map(|p| p.short_descr.clone())
        .unwrap_or_else(|| "the target".to_string());
    Some(format!(
        "\r\n*** Quest objective complete: {mob_name} is safe! Return to the questmaster. ***\r\n",
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
    total + me.bonus_ac + me.affect_ac_bonus()
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

    // ROOM_PEACEFUL: refuse combat here.
    if w.rooms.get(&me.current_room)
        .map(|r| r.room_flags[0] & crate::world::ROOM_PEACEFUL != 0)
        .unwrap_or(false)
    {
        return CmdOutput::text(
            "\r\nA flash of white light fills the room, dispelling your violent aggression!\r\n"
        );
    }

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
        // Fire DEATH triggers before extraction.
        fire_mob_death_triggers(mob_id, &me.name, world, chars).await;
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
        if let Some(qmsg) = quest_check_save(me, world).await {
            msg.push_str(&qmsg);
        }
        if let Some(bump) = learn_attempt(me, skill, 5) { msg.push_str(&bump); }
        return CmdOutput::text(msg);
    }

    let mut out = to_me;
    if hit {
        if let Some(bump) = learn_attempt(me, skill, 5) { out.push_str(&bump); }
    }
    CmdOutput::text(out)
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
    // ROOM_NOMAGIC: refuse before any class/learned check so the spell
    // name isn't even resolved.
    {
        let w = world.lock().await;
        if w.rooms.get(&me.current_room)
            .map(|r| r.room_flags[0] & crate::world::ROOM_NOMAGIC != 0)
            .unwrap_or(false)
        {
            return CmdOutput::text("\r\nYour magic fizzles out and dies.\r\n");
        }
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

    // Dispatch.  After the inner handler returns we roll a learn-bump
    // for the spell (~4% per cast) whether it landed or fizzled —
    // both consumed mana, both count as practice.  Mirrors the
    // physical-skill bump path in `do_skill` (cp54).
    let mut out = match spell {
        crate::character::Skill::MagicMissile => cast_magic_missile(target, me, world, chars, learned).await,
        crate::character::Skill::CureLight    => cast_cure_light(target, me, chars, learned).await,
        crate::character::Skill::Bless        => cast_bless(target, me, chars, learned).await,
        crate::character::Skill::BurningHands => cast_burning_hands(me, world, chars, learned).await,
        crate::character::Skill::Sanctuary    => cast_sanctuary(target, me, chars, learned).await,
        crate::character::Skill::Harm         => cast_harm(target, me, world, chars, learned).await,
        crate::character::Skill::WordOfRecall => cast_word_of_recall(me, world, chars).await,
        crate::character::Skill::Identify     => cast_identify(target, me, world).await,
        crate::character::Skill::DetectInvis  => cast_detect_invis(me),
        crate::character::Skill::DetectMagic  => cast_detect_magic(me, world).await,
        crate::character::Skill::Poison       => cast_poison(target, me, world, chars, learned).await,
        crate::character::Skill::Sleep        => cast_debuff(target, me, world, chars, learned, crate::character::Skill::Sleep).await,
        crate::character::Skill::Blindness    => cast_debuff(target, me, world, chars, learned, crate::character::Skill::Blindness).await,
        crate::character::Skill::CurePoison   => cast_cure_affect(target, me, world, chars, crate::character::Skill::Poison).await,
        crate::character::Skill::CureBlind    => cast_cure_affect(target, me, world, chars, crate::character::Skill::Blindness).await,
        crate::character::Skill::CureCritic   => cast_cure_critic(target, me, chars, learned).await,
        crate::character::Skill::Strength     => cast_buff(target, me, chars, learned, crate::character::Skill::Strength).await,
        crate::character::Skill::Armor        => cast_buff(target, me, chars, learned, crate::character::Skill::Armor).await,
        crate::character::Skill::Haste        => cast_buff(target, me, chars, learned, crate::character::Skill::Haste).await,
        crate::character::Skill::Slow         => cast_debuff(target, me, world, chars, learned, crate::character::Skill::Slow).await,
        crate::character::Skill::Earthquake   => cast_earthquake(me, world, chars, learned).await,
        crate::character::Skill::CharmPerson  => cast_debuff(target, me, world, chars, learned, crate::character::Skill::CharmPerson).await,
        _ => CmdOutput::text("\r\nUnknown spell.\r\n"),
    };
    if let Some(bump) = learn_attempt(me, spell, 4) {
        out.text.push_str(&bump);
    }
    out
}

async fn cast_detect_magic(me: &mut Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    // One-shot reveal: list magical items in inventory + current room.
    // An item is "magical" if any affect_flags bit is set or the
    // ITEM_MAGIC extra flag (bit 5 of extra_flags[0]) is set.
    const ITEM_MAGIC: u32 = 1 << 5;
    me.mana -= crate::character::Skill::DetectMagic.mana_cost();

    let w = world.lock().await;
    let is_magical = |obj: &crate::world::ObjInstance| -> bool {
        if obj.corpse_of.is_some() { return false; }
        let Some(p) = w.obj_protos.get(&obj.vnum) else { return false; };
        p.extra_flags[0] & ITEM_MAGIC != 0
            || p.affect_flags[0] != 0
            || p.affect_flags[1] != 0
            || p.affect_flags[2] != 0
            || p.affect_flags[3] != 0
    };

    let mut s = String::from("\r\nYou close your eyes and seek auras of magic...\r\n");
    let mut any = false;

    // Inventory pass.
    let inv_hits: Vec<String> = me.inventory.iter()
        .filter_map(|iid| w.obj_instances.iter().find(|o| o.id == *iid))
        .filter(|o| is_magical(o))
        .filter_map(|o| w.obj_protos.get(&o.vnum).map(|p| p.short_description.clone()))
        .collect();
    if !inv_hits.is_empty() {
        any = true;
        s.push_str("  In your inventory:\r\n");
        for n in &inv_hits { s.push_str(&format!("    {n}\r\n")); }
    }

    // Equipment pass.
    let eq_hits: Vec<String> = me.equipment.iter()
        .filter_map(|s| *s)
        .filter_map(|iid| w.obj_instances.iter().find(|o| o.id == iid))
        .filter(|o| is_magical(o))
        .filter_map(|o| w.obj_protos.get(&o.vnum).map(|p| p.short_description.clone()))
        .collect();
    if !eq_hits.is_empty() {
        any = true;
        s.push_str("  Worn / wielded:\r\n");
        for n in &eq_hits { s.push_str(&format!("    {n}\r\n")); }
    }

    // Room pass.
    if let Some(r) = w.rooms.get(&me.current_room) {
        let room_hits: Vec<String> = r.objects.iter()
            .filter_map(|iid| w.obj_instances.iter().find(|o| o.id == *iid))
            .filter(|o| is_magical(o))
            .filter_map(|o| w.obj_protos.get(&o.vnum).map(|p| p.short_description.clone()))
            .collect();
        if !room_hits.is_empty() {
            any = true;
            s.push_str("  Here in this room:\r\n");
            for n in &room_hits { s.push_str(&format!("    {n}\r\n")); }
        }
    }

    if !any {
        s.push_str("  ...you sense no magic nearby.\r\n");
    }
    CmdOutput::text(s)
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
        dot_damage:    0,
        to_ac:         0,
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
        // Fire DEATH triggers before extraction.
        fire_mob_death_triggers(mob_id, &me.name, world, chars).await;
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
        if let Some(qmsg) = quest_check_save(me, world).await {
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
        dot_damage:    0,
        to_ac:         0,
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
        dot_damage:    0,
        to_ac:         0,
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
        // Fire DEATH triggers before extraction.
        fire_mob_death_triggers(mob_id, &me.name, world, chars).await;
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
        if let Some(qmsg) = quest_check_save(me, world).await {
            msg.push_str(&qmsg);
        }
        return CmdOutput::text(msg);
    }
    CmdOutput::text(to_me)
}

/// `cast poison <target>` — apply a Poison affect to a mob in the
/// caster's room.  Duration scales mildly with learned%; damage is a
/// flat 3/tick.  Refuses on missing target or no matching mob.
async fn cast_poison(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;
    if target_kw.is_empty() {
        return CmdOutput::text("\r\nCast poison on whom?\r\n");
    }
    me.mana -= crate::character::Skill::Poison.mana_cost();

    let kw = target_kw.to_ascii_lowercase();
    let target_mid: Option<u32> = {
        let w = world.lock().await;
        w.rooms.get(&me.current_room).and_then(|r| r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&kw)) {
                Some(mid)
            } else { None }
        }))
    };
    let Some(mid) = target_mid else {
        return CmdOutput::text(format!("\r\nYou see no '{target_kw}' here.\r\n"));
    };

    // Hit chance scales with learned%.  Misses still consume mana.
    let hit_chance = (70 + learned as i32 / 5).min(95);
    if rand::thread_rng().gen_range(0..100) >= hit_chance {
        return CmdOutput::text("\r\nThe poison fails to take.\r\n");
    }

    // Apply the affect + grab mob name for the broadcast.
    let mob_name = {
        let mut w = world.lock().await;
        let aff = crate::character::Affect {
            skill:         crate::character::Skill::Poison,
            duration:      5 + (learned as i32 / 20),  // ~10–15s at 100%
            to_hit:        0,
            to_dam:        0,
            dmg_reduction: 0,
            dot_damage:    3,
            to_ac:         0,
        };
        let vnum = {
            let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mid) else {
                return CmdOutput::text("\r\nYour target is gone.\r\n");
            };
            m.apply_affect(aff);
            m.vnum
        };
        w.mob_protos.get(&vnum).map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".to_string())
    };
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} looks ill.\r\n", mob_name));
    CmdOutput::text(format!("\r\n{mob_name} looks ill.\r\n"))
}

/// Shared shape for non-DoT debuff spells (Sleep, Blindness): roll vs
/// learned, apply an Affect with `dot_damage: 0` to the named mob.
/// Duration and broadcast wording are picked per skill.
async fn cast_debuff(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
    skill:   crate::character::Skill,
) -> CmdOutput {
    use rand::Rng;
    if target_kw.is_empty() {
        return CmdOutput::text(format!("\r\nCast {} on whom?\r\n", skill.name()));
    }
    me.mana -= skill.mana_cost();
    let kw = target_kw.to_ascii_lowercase();
    let target_mid: Option<u32> = {
        let w = world.lock().await;
        w.rooms.get(&me.current_room).and_then(|r| r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&kw)) {
                Some(mid)
            } else { None }
        }))
    };
    let Some(mid) = target_mid else {
        return CmdOutput::text(format!("\r\nYou see no '{target_kw}' here.\r\n"));
    };
    let hit_chance = (65 + learned as i32 / 5).min(95);
    if rand::thread_rng().gen_range(0..100) >= hit_chance {
        return CmdOutput::text(format!("\r\nThe {} spell fails to take.\r\n", skill.name()));
    }
    let (duration, broadcast_room, broadcast_self) = match skill {
        crate::character::Skill::Sleep => (
            8 + learned as i32 / 10,
            "{} stumbles, then collapses asleep.\r\n",
            "{} falls into a deep slumber.\r\n",
        ),
        crate::character::Skill::Blindness => (
            6 + learned as i32 / 10,
            "{} gropes around blindly.\r\n",
            "{} cannot see anything.\r\n",
        ),
        crate::character::Skill::Slow => (
            6 + learned as i32 / 10,
            "{} slows down noticeably.\r\n",
            "{} starts moving in slow motion.\r\n",
        ),
        crate::character::Skill::CharmPerson => (
            10 + learned as i32 / 10,
            "{} looks at you with adoring eyes.\r\n",
            "{} now follows your every word.\r\n",
        ),
        _ => return CmdOutput::text("\r\nUnsupported debuff.\r\n"),
    };
    let mob_name = {
        let mut w = world.lock().await;
        let aff = crate::character::Affect {
            skill, duration,
            to_hit: 0, to_dam: 0, dmg_reduction: 0, dot_damage: 0, to_ac: 0,
        };
        let vnum = {
            let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mid) else {
                return CmdOutput::text("\r\nYour target is gone.\r\n");
            };
            m.apply_affect(aff);
            m.vnum
        };
        w.mob_protos.get(&vnum).map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".to_string())
    };
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &broadcast_room.replace("{}", &mob_name));
    CmdOutput::text(format!("\r\n{}", broadcast_self.replace("{}", &mob_name)))
}

/// Strip a single affect (Poison/Blindness/...) from a target.  With no
/// target keyword, cures the caster.  Otherwise looks up another
/// player in the room first, then a mob.  Mana drains on every cast,
/// even when no matching affect was found.
async fn cast_cure_affect(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    affect_kind: crate::character::Skill,
) -> CmdOutput {
    let cure_skill = match affect_kind {
        crate::character::Skill::Poison    => crate::character::Skill::CurePoison,
        crate::character::Skill::Blindness => crate::character::Skill::CureBlind,
        _ => return CmdOutput::text("\r\nUnknown cure target.\r\n"),
    };
    me.mana -= cure_skill.mana_cost();

    // No-arg → self.
    if target_kw.is_empty() {
        let before = me.affects.len();
        me.affects.retain(|a| a.skill != affect_kind);
        let removed = before != me.affects.len();
        return CmdOutput::text(if removed {
            format!("\r\nA warm light banishes the {} from you.\r\n", affect_kind.name())
        } else {
            format!("\r\nYou are not {}.\r\n", affect_kind.name())
        });
    }

    // Try another player in the same room.
    let target_handle = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p|
            p.current_room == me.current_room
            && p.name.eq_ignore_ascii_case(target_kw)).cloned();
        h
    };
    if let Some(ph) = target_handle {
        let removed = {
            let mut c = ph.character.lock().await;
            let before = c.affects.len();
            c.affects.retain(|a| a.skill != affect_kind);
            before != c.affects.len()
        };
        let msg = if removed {
            format!("\r\n{} cures your {}.\r\n", me.name, affect_kind.name())
        } else {
            format!("\r\n{} prays for your relief — but you weren't {}.\r\n",
                me.name, affect_kind.name())
        };
        let _ = ph.send.send(msg);
        return CmdOutput::text(if removed {
            format!("\r\nThe {} fades from {}.\r\n", affect_kind.name(), ph.name)
        } else {
            format!("\r\n{} is not {}.\r\n", ph.name, affect_kind.name())
        });
    }

    // Otherwise a mob in the room.
    let kw = target_kw.to_ascii_lowercase();
    let (mob_name, removed) = {
        let mut w = world.lock().await;
        let target_mid: Option<u32> = w.rooms.get(&me.current_room).and_then(|r| {
            r.mobs.iter().find_map(|&mid| {
                let m = w.mob_instances.iter().find(|m| m.id == mid)?;
                let p = w.mob_protos.get(&m.vnum)?;
                if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&kw)) {
                    Some(mid)
                } else { None }
            })
        });
        let Some(mid) = target_mid else {
            return CmdOutput::text(format!("\r\nYou see no '{target_kw}' here.\r\n"));
        };
        let (name, removed) = {
            let m = w.mob_instances.iter_mut().find(|m| m.id == mid).unwrap();
            let before = m.affects.len();
            m.affects.retain(|a| a.skill != affect_kind);
            let removed = before != m.affects.len();
            (m.vnum, removed)
        };
        let name = w.mob_protos.get(&name).map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".to_string());
        (name, removed)
    };
    if removed {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} looks better.\r\n", mob_name));
        CmdOutput::text(format!("\r\nThe {} fades from {mob_name}.\r\n", affect_kind.name()))
    } else {
        CmdOutput::text(format!("\r\n{mob_name} is not {}.\r\n", affect_kind.name()))
    }
}

/// Heavier heal — 3d8 + level + wis/2.  Targets self if no arg, else a
/// named player in the caller's room (mirrors cast_cure_light).  Spell
/// does not affect mobs.
async fn cast_cure_critic(
    target_kw: &str,
    me: &mut Character,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;
    me.mana -= crate::character::Skill::CureCritic.mana_cost();
    let hit_chance = (90 + learned as i32 / 5).min(99);
    if rand::thread_rng().gen_range(0..100) >= hit_chance {
        return CmdOutput::text("\r\nThe healing prayer fizzles.\r\n");
    }
    let heal = crate::db::dice(3, 8) + me.level + (me.wis - 10).max(0) / 2;

    // Self?
    if target_kw.is_empty() {
        me.hp = (me.hp + heal).min(me.max_hp);
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} bathes themself in restorative light.\r\n", me.name));
        return CmdOutput::text(format!(
            "\r\nWaves of warmth course through you, mending wounds. ({}/{} HP)\r\n",
            me.hp, me.max_hp,
        ));
    }
    // Another player in room.
    let target_handle = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p|
            p.current_room == me.current_room
            && p.name.eq_ignore_ascii_case(target_kw)).cloned();
        h
    };
    let Some(ph) = target_handle else {
        return CmdOutput::text(format!("\r\nNo one named '{target_kw}' is here.\r\n"));
    };
    let (new_hp, max) = {
        let mut c = ph.character.lock().await;
        c.hp = (c.hp + heal).min(c.max_hp);
        (c.hp, c.max_hp)
    };
    let _ = ph.send.send(format!(
        "\r\n{} bathes you in restorative light. ({}/{} HP)\r\n",
        me.name, new_hp, max,
    ));
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} bathes {} in restorative light.\r\n", me.name, ph.name));
    CmdOutput::text(format!(
        "\r\nYour healing magic restores {} ({} HP).\r\n", ph.name, heal,
    ))
}

/// Shared shape for self/target-player buff spells (Strength, Armor).
/// Picks per-skill duration + (to_dam, to_ac) modifiers and refresh-
/// stacks via Character::apply_affect.
async fn cast_buff(
    target_kw: &str,
    me: &mut Character,
    chars: &SharedChars,
    learned: u8,
    skill:   crate::character::Skill,
) -> CmdOutput {
    me.mana -= skill.mana_cost();
    let (duration, to_dam, to_ac, self_msg, room_msg) = match skill {
        crate::character::Skill::Strength => (
            6 + learned as i32 / 10,
            1 + learned as i32 / 30, 0,
            "Your muscles surge with newfound strength.\r\n",
            "{} looks stronger.\r\n",
        ),
        crate::character::Skill::Armor => (
            8 + learned as i32 / 10,
            0, 20,
            "A shimmering layer of force wraps around you.\r\n",
            "A shimmering layer of force wraps around {}.\r\n",
        ),
        crate::character::Skill::Haste => (
            5 + learned as i32 / 15,
            0, 0,
            "Time seems to slow around you as you move with sudden speed.\r\n",
            "{} moves with sudden speed.\r\n",
        ),
        _ => return CmdOutput::text("\r\nUnsupported buff.\r\n"),
    };

    // Resolve target — self if no arg, else named player in room.
    let target_self = target_kw.is_empty()
        || target_kw.eq_ignore_ascii_case(&me.name);
    if target_self {
        me.apply_affect(crate::character::Affect {
            skill, duration,
            to_hit: 0, to_dam, dmg_reduction: 0, dot_damage: 0, to_ac,
        });
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &room_msg.replace("{}", &me.name));
        return CmdOutput::text(format!("\r\n{}", self_msg));
    }

    let ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p|
            p.current_room == me.current_room
            && p.name.eq_ignore_ascii_case(target_kw)).cloned();
        h
    };
    let Some(ph) = ph else {
        return CmdOutput::text(format!("\r\nNo one named '{target_kw}' is here.\r\n"));
    };
    {
        let mut c = ph.character.lock().await;
        c.apply_affect(crate::character::Affect {
            skill, duration,
            to_hit: 0, to_dam, dmg_reduction: 0, dot_damage: 0, to_ac,
        });
    }
    let _ = ph.send.send(format!("\r\n{} casts {} on you.\r\n", me.name, skill.name()));
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &room_msg.replace("{}", &ph.name));
    CmdOutput::text(format!("\r\nYou cast {} on {}.\r\n", skill.name(), ph.name))
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
            // Fire DEATH triggers before extraction.
            fire_mob_death_triggers(mob_id, &me.name, world, chars).await;
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

/// `earthquake` — AoE that auto-hits every mob in the caster's room.
/// Damage is `dice(2, 8) + level + (wis-10)/2`.  Slain mobs follow the
/// normal kill path (corpse + DEATH triggers + XP).  Mana drains
/// regardless of how many targets are present.
async fn cast_earthquake(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use crate::db::dice;
    me.mana -= crate::character::Skill::Earthquake.mana_cost();
    let _ = learned;            // No to-hit roll — earthquake always hits.

    let mob_ids: Vec<u32> = {
        let w = world.lock().await;
        w.rooms.get(&me.current_room).map(|r| r.mobs.clone()).unwrap_or_default()
    };

    let to_room = format!("{} invokes an earthquake!\r\n", me.name);
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }
    let mut to_me = String::from("\r\nThe ground heaves and cracks as you invoke an earthquake!\r\n");
    if mob_ids.is_empty() {
        to_me.push_str("Dust settles — nothing here to shake.\r\n");
        return CmdOutput::text(to_me);
    }

    let mut total_xp = 0i64;
    let mut killed_names: Vec<String> = Vec::new();
    for mob_id in mob_ids {
        let dmg = dice(2, 8) + me.level + (me.wis - 10).max(0) / 2;
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
        to_me.push_str(&format!("The shockwave hits {mob_name} for {dmg} damage!\r\n"));

        if mob_dead {
            fire_mob_death_triggers(mob_id, &me.name, world, chars).await;
            let xp = {
                let w = world.lock().await;
                w.mob_instances.iter().find(|m| m.id == mob_id)
                    .and_then(|m| w.mob_protos.get(&m.vnum))
                    .map(|p| p.exp as i64).unwrap_or(0)
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
                cl.broadcast_room(mob_room, None,
                    &format!("{mob_name} is crushed by falling rubble.\r\n"));
            }
            killed_names.push(mob_name);
        }
    }
    let still_have_target = {
        let w = world.lock().await;
        me.fighting.map(|t| !t.is_player
            && w.mob_instances.iter().any(|m| m.id == t.id)).unwrap_or(false)
    };
    if !still_have_target { me.fighting = None; }
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
    let mut out = if me.sneaking {
        "\r\nYou are now sneaking quietly.\r\n".to_string()
    } else {
        "\r\nYou stop sneaking.\r\n".to_string()
    };
    if me.sneaking {
        if let Some(bump) = learn_attempt(me, Skill::Sneak, 3) { out.push_str(&bump); }
    }
    CmdOutput::text(out)
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
        let mut out = "\r\nYou attempt to hide yourself.\r\n".to_string();
        if let Some(bump) = learn_attempt(me, Skill::Hide, 5) { out.push_str(&bump); }
        CmdOutput::text(out)
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
        let mut out = format!("\r\nYou lift {take} gold from {mob_name}.\r\n");
        if let Some(bump) = learn_attempt(me, Skill::Steal, 5) { out.push_str(&bump); }
        return CmdOutput::text(out);
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
    let mut out = format!("\r\nYou deftly lift {short} from {mob_name}.\r\n");
    if let Some(bump) = learn_attempt(me, Skill::Steal, 5) { out.push_str(&bump); }
    CmdOutput::text(out)
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

// ---------------------------------------------------------------------------
// Door commands (open/close/lock/unlock)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum DoorOp { Open, Close, Lock, Unlock }

/// Resolve a player-supplied target ("north", "n", "door") to the exit
/// it refers to, returning (direction, exit info, key vnum, keyword,
/// destination room). Tries direction parsing first, then matches the
/// arg as a keyword against any door-bearing exit.
fn find_door_target(world: &World, room: RoomVnum, target: &str)
    -> Option<(Direction, u32, i32, String, RoomVnum)>
{
    let r = world.rooms.get(&room)?;
    if let Some(dir) = Direction::parse(target) {
        if let Some(ex) = r.exits[dir as usize].as_ref() {
            return Some((dir, ex.exit_info, ex.key, ex.keyword.clone(), ex.to_room));
        }
    }
    let tlow = target.to_ascii_lowercase();
    for (i, ex) in r.exits.iter().enumerate() {
        if let Some(ex) = ex {
            if (ex.exit_info & crate::world::EX_ISDOOR) == 0 { continue; }
            if ex.keyword.split_whitespace().any(|k| k.eq_ignore_ascii_case(&tlow)) {
                let dir = Direction::from_index(i as u8)?;
                return Some((dir, ex.exit_info, ex.key, ex.keyword.clone(), ex.to_room));
            }
        }
    }
    None
}

/// Toggle EX_* flag bits on both sides of a door under a single world
/// lock.  `set_mask` are bits to set; `clear_mask` are bits to clear.
fn mutate_door(world: &mut World, room: RoomVnum, dir: Direction, set_mask: u32, clear_mask: u32) {
    let to_room = match world.rooms.get_mut(&room)
        .and_then(|r| r.exits[dir as usize].as_mut())
    {
        Some(ex) => {
            ex.exit_info &= !clear_mask;
            ex.exit_info |= set_mask;
            ex.to_room
        }
        None => return,
    };
    if to_room == crate::world::NOWHERE { return; }
    let rev = dir.opposite();
    if let Some(ex) = world.rooms.get_mut(&to_room)
        .and_then(|r| r.exits[rev as usize].as_mut())
    {
        ex.exit_info &= !clear_mask;
        ex.exit_info |= set_mask;
    }
}

async fn do_door(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    op: DoorOp,
) -> CmdOutput {
    use crate::world::{EX_ISDOOR, EX_CLOSED, EX_LOCKED};
    let verb = match op {
        DoorOp::Open   => "open",
        DoorOp::Close  => "close",
        DoorOp::Lock   => "lock",
        DoorOp::Unlock => "unlock",
    };
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text(format!("\r\n{} what?\r\n", capitalize_first(verb)));
    }

    let w = world.lock().await;
    let Some((dir, info, key_vnum, keyword, _to)) =
        find_door_target(&w, me.current_room, arg)
    else {
        return CmdOutput::text(format!("\r\nYou see no such door here.\r\n"));
    };
    drop(w);
    if (info & EX_ISDOOR) == 0 {
        return CmdOutput::text(format!("\r\nThat's not a door.\r\n"));
    }
    let kw_short = if keyword.is_empty() { "door".to_string() }
                   else { keyword.split_whitespace().next().unwrap_or("door").to_string() };

    // Op-specific preconditions.
    let (set_mask, clear_mask, broadcast) = match op {
        DoorOp::Open => {
            if (info & EX_CLOSED) == 0 {
                return CmdOutput::text(format!("\r\nIt's already open.\r\n"));
            }
            if (info & EX_LOCKED) != 0 {
                return CmdOutput::text(format!("\r\nIt seems to be locked.\r\n"));
            }
            (0, EX_CLOSED, format!("{} opens the {kw_short}.\r\n", me.name))
        }
        DoorOp::Close => {
            if (info & EX_CLOSED) != 0 {
                return CmdOutput::text(format!("\r\nIt's already closed.\r\n"));
            }
            (EX_CLOSED, 0, format!("{} closes the {kw_short}.\r\n", me.name))
        }
        DoorOp::Unlock => {
            if (info & EX_CLOSED) == 0 {
                return CmdOutput::text(format!("\r\nIt's not even closed.\r\n"));
            }
            if (info & EX_LOCKED) == 0 {
                return CmdOutput::text(format!("\r\nIt's already unlocked.\r\n"));
            }
            if !player_has_key(me, key_vnum, world).await {
                return CmdOutput::text(format!("\r\nYou don't have the key.\r\n"));
            }
            (0, EX_LOCKED, format!("{} unlocks the {kw_short}.\r\n", me.name))
        }
        DoorOp::Lock => {
            if (info & EX_CLOSED) == 0 {
                return CmdOutput::text(format!("\r\nYou'll need to close it first.\r\n"));
            }
            if (info & EX_LOCKED) != 0 {
                return CmdOutput::text(format!("\r\nIt's already locked.\r\n"));
            }
            if !player_has_key(me, key_vnum, world).await {
                return CmdOutput::text(format!("\r\nYou don't have the key.\r\n"));
            }
            (EX_LOCKED, 0, format!("{} locks the {kw_short}.\r\n", me.name))
        }
    };

    {
        let mut w = world.lock().await;
        mutate_door(&mut w, me.current_room, dir, set_mask, clear_mask);
    }
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id), &broadcast);
    CmdOutput::text(format!("\r\nYou {verb} the {kw_short}.\r\n"))
}

/// `search` — peek for hidden exits in the current room.  Always
/// succeeds for now (a future tweak could roll vs class/perception).
/// Lists each hidden direction + door keyword and broadcasts a "X
/// searches the area." line so other players can see what you're up
/// to.  No-op if the room has no hidden exits.
async fn do_search(
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::world::EX_HIDDEN;
    let w = world.lock().await;
    let Some(r) = w.rooms.get(&me.current_room) else {
        return CmdOutput::text("\r\nYou are nowhere.\r\n".to_string());
    };
    let mut found = Vec::new();
    for d in Direction::ALL {
        if let Some(e) = &r.exits[d as usize] {
            if e.to_room == crate::world::NOWHERE { continue; }
            if (e.exit_info & EX_HIDDEN) == 0 { continue; }
            let kw = if e.keyword.is_empty() { "passage".to_string() }
                     else { e.keyword.split_whitespace().next().unwrap_or("passage").to_string() };
            found.push((d, kw));
        }
    }
    drop(w);
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} searches the area.\r\n", me.name));
    drop(cl);
    if found.is_empty() {
        return CmdOutput::text("\r\nYou find nothing of interest.\r\n".to_string());
    }
    let mut s = String::from("\r\nYou find:\r\n");
    for (d, kw) in found {
        s.push_str(&format!("  A hidden {kw} to the {}.\r\n", d.name()));
    }
    CmdOutput::text(s)
}

/// Roll a learn% improvement on a skill after a successful use.  Caps
/// at 100. Returns a player-facing line on a bump (which the caller is
/// free to append to its output), or None on a no-op.  `chance_pct` is
/// the per-use probability of gaining one point.
fn learn_attempt(me: &mut Character, skill: Skill, chance_pct: i32) -> Option<String> {
    use rand::Rng;
    let cur = *me.skills.get(&skill).unwrap_or(&0);
    if cur >= 100 { return None; }
    if rand::thread_rng().gen_range(0..100) >= chance_pct { return None; }
    let next = (cur + 1).min(100);
    me.skills.insert(skill, next);
    Some(format!("You feel more skilled at {}.\r\n", skill.name()))
}

async fn do_pick(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::world::{EX_ISDOOR, EX_CLOSED, EX_LOCKED, EX_PICKPROOF};
    use rand::Rng;

    me.reveal();
    let skill = Skill::PickLock;
    if !skill.is_class_allowed(me.class) {
        return CmdOutput::text("\r\nYou know nothing about picking locks.\r\n".to_string());
    }
    let learned = *me.skills.get(&skill).unwrap_or(&0);
    if learned == 0 {
        return CmdOutput::text("\r\nYou'd need to practice 'pick lock' first.\r\n".to_string());
    }
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nPick what?\r\n".to_string());
    }

    let w = world.lock().await;
    let Some((dir, info, _key, keyword, _to)) = find_door_target(&w, me.current_room, arg) else {
        return CmdOutput::text("\r\nYou see no such door here.\r\n".to_string());
    };
    drop(w);
    if (info & EX_ISDOOR) == 0 {
        return CmdOutput::text("\r\nThat's not a door.\r\n".to_string());
    }
    if (info & EX_CLOSED) == 0 {
        return CmdOutput::text("\r\nIt's not even closed.\r\n".to_string());
    }
    if (info & EX_LOCKED) == 0 {
        return CmdOutput::text("\r\nOh, it wasn't locked after all.\r\n".to_string());
    }
    if (info & EX_PICKPROOF) != 0 {
        return CmdOutput::text("\r\nIt resists your attempts to pick it.\r\n".to_string());
    }
    let kw_short = if keyword.is_empty() { "door".to_string() }
                   else { keyword.split_whitespace().next().unwrap_or("door").to_string() };

    // Roll: chance scales linearly with learned (0..100).
    let roll = rand::thread_rng().gen_range(0..100);
    if roll >= learned as i32 {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} fumbles at the {kw_short} with a set of picks.\r\n", me.name));
        return CmdOutput::text(format!("\r\nYou fumble at the {kw_short}.\r\n"));
    }

    {
        let mut w = world.lock().await;
        mutate_door(&mut w, me.current_room, dir, 0, EX_LOCKED);
    }
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} picks the lock on the {kw_short}.\r\n", me.name));
    }
    let mut out = "\r\nThe lock clicks open.\r\n".to_string();
    if let Some(bump) = learn_attempt(me, Skill::PickLock, 8) { out.push_str(&bump); }
    CmdOutput::text(out)
}

/// True if `key_vnum` is non-negative and the player has an instance of
/// that vnum in their inventory.
async fn player_has_key(me: &Character, key_vnum: i32, world: &Arc<Mutex<World>>) -> bool {
    if key_vnum < 0 { return false; }
    let w = world.lock().await;
    me.inventory.iter().any(|&iid| {
        w.obj_instances.iter().find(|o| o.id == iid)
            .map(|o| o.vnum == key_vnum)
            .unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// Potions & scrolls
// ---------------------------------------------------------------------------

/// Cast a spell identified by its CircleMUD spell vnum at `target_kw`,
/// returning the spell's text output.  Used by drink/recite (and a
/// natural extension point if we ever add wand/staff later).  Saves and
/// restores `me.mana` so consumable casts don't drain it.
async fn apply_item_spell(
    spell_vnum: i32,
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> String {
    let saved_mana = me.mana;
    // Pretend the potion/scroll is fully learned.
    let learned: u8 = 100;
    let out = match spell_vnum {
        3  => cast_bless(target_kw, me, chars, learned).await,
        5  => cast_burning_hands(me, world, chars, learned).await,
        16 => cast_cure_light(target_kw, me, chars, learned).await,
        19 => cast_detect_invis(me),
        20 => cast_detect_magic(me, world).await,
        27 => cast_harm(target_kw, me, world, chars, learned).await,
        32 => cast_magic_missile(target_kw, me, world, chars, learned).await,
        36 => cast_sanctuary(target_kw, me, chars, learned).await,
        42 => cast_word_of_recall(me, world, chars).await,
        52 => cast_identify(target_kw, me, world).await,
        // 0 or -1 in value[1..3] means "no spell in this slot" — skip
        // silently (matches stock potion data).
        n if n <= 0 => return String::new(),
        _  => return "\r\nThe magic fizzles harmlessly.\r\n".to_string(),
    };
    me.mana = saved_mana;
    out.text
}

/// Locate a consumable in inventory matching `keyword` whose item_type
/// is `expected_type`.  Returns (instance_id, short_descr, [v0,v1,v2,v3]).
async fn find_consumable(
    me: &Character,
    world: &Arc<Mutex<World>>,
    keyword: &str,
    expected_type: i32,
) -> Option<(u32, String, [i32; 4])> {
    let kw = keyword.to_ascii_lowercase();
    let w = world.lock().await;
    for &iid in &me.inventory {
        let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
        let Some(p) = w.obj_protos.get(&o.vnum) else { continue; };
        if p.item_type != expected_type { continue; }
        if !p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&kw)) { continue; }
        return Some((iid, p.short_description.clone(), p.value));
    }
    None
}

/// `quaff <potion-kw>` — drink a potion (ITEM_POTION). CircleMUD splits
/// potion vs. drink-container; drink is for ITEM_DRINKCON below.
async fn do_quaff(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::world::ITEM_POTION;
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nQuaff what?\r\n".to_string());
    }
    let Some((iid, short, value)) = find_consumable(me, world, arg, ITEM_POTION).await else {
        return CmdOutput::text("\r\nYou have nothing like that to quaff.\r\n".to_string());
    };

    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} quaffs {}.\r\n", me.name, short));
    drop(cl);

    let mut text = format!("\r\nYou quaff {}.\r\n", short);
    // value[0] = level, value[1..3] = up to three spell vnums to cast on
    // the drinker.
    for slot in 1..4 {
        let s = apply_item_spell(value[slot], "", me, world, chars).await;
        text.push_str(&s);
    }
    me.inventory.retain(|&i| i != iid);
    {
        let mut w = world.lock().await;
        w.obj_instances.retain(|o| o.id != iid);
    }
    CmdOutput::text(text)
}

/// `drink <kw>` — sip from a drink container (ITEM_DRINKCON). value[1]
/// is current sips; decrement by one per command. value[0] is capacity
/// (informational). The container itself stays in inventory.
async fn do_drink_container(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::world::ITEM_DRINKCON;
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nDrink what?\r\n".to_string());
    }
    let kw = arg.to_ascii_lowercase();
    // Search inventory and the room's floor.
    let found_vnum_short: Option<(crate::world::ObjVnum, String)> = {
        let w = world.lock().await;
        let pool: Vec<u32> = me.inventory.iter().copied()
            .chain(w.rooms.get(&me.current_room).map(|r| r.objects.clone()).unwrap_or_default())
            .collect();
        let mut out: Option<(crate::world::ObjVnum, String)> = None;
        for iid in pool {
            let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let Some(p) = w.obj_protos.get(&o.vnum) else { continue; };
            if p.item_type != ITEM_DRINKCON { continue; }
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&kw)) {
                out = Some((o.vnum, p.short_description.clone()));
                break;
            }
        }
        out
    };
    let Some((vnum, short)) = found_vnum_short else {
        return CmdOutput::text("\r\nYou see no such drink container.\r\n".to_string());
    };
    // Drain one sip from the prototype (matches stock CircleMUD shared
    // state — sips drained by one player affect every instance, but
    // that's consistent with our wand/staff handling).
    let (sips_after, capacity) = {
        let mut w = world.lock().await;
        let Some(p) = w.obj_protos.get_mut(&vnum) else {
            return CmdOutput::text("\r\nThe container shimmers and is gone.\r\n".to_string());
        };
        if p.value[1] <= 0 {
            return CmdOutput::text(format!("\r\n{} is empty.\r\n", short));
        }
        p.value[1] -= 1;
        (p.value[1], p.value[0])
    };
    let was_thirsty = me.thirst >= 0 && me.thirst <= 3;
    if me.thirst >= 0 {
        me.thirst = (me.thirst + 3).min(MAX_THIRST);
    }
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} drinks from {}.\r\n", me.name, short));
    drop(cl);
    let mut text = format!("\r\nYou drink from {}.\r\n", short);
    if was_thirsty && me.thirst > 3 {
        text.push_str("You are no longer thirsty.\r\n");
    }
    if sips_after == 0 {
        text.push_str(&format!("{} is now empty.\r\n", short));
    }
    let _ = capacity;
    CmdOutput::text(text)
}

/// `eat <kw>` — consume an ITEM_FOOD in inventory.  Flavor text + room
/// broadcast; the object is extracted on use.  value[0] is the filling
/// in hours, saved for future hunger tracking (no decay tick yet).
async fn do_eat(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::world::ITEM_FOOD;
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nEat what?\r\n".to_string());
    }
    let Some((iid, short, value)) = find_consumable(me, world, arg, ITEM_FOOD).await else {
        return CmdOutput::text("\r\nYou have no such food.\r\n".to_string());
    };
    let was_hungry = me.hunger >= 0 && me.hunger <= 3;
    if me.hunger >= 0 {
        me.hunger = (me.hunger + value[0].max(1)).min(MAX_HUNGER);
    }
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} eats {}.\r\n", me.name, short));
    drop(cl);
    me.inventory.retain(|&i| i != iid);
    {
        let mut w = world.lock().await;
        w.obj_instances.retain(|o| o.id != iid);
    }
    let mut text = format!("\r\nYou eat {}.\r\n", short);
    if was_hungry && me.hunger > 3 {
        text.push_str("You are no longer hungry.\r\n");
    }
    CmdOutput::text(text)
}

/// Hunger/thirst caps — game-hours of fullness. Roughly matches stock
/// CircleMUD constants.c.
pub const MAX_HUNGER: i32 = 24;
pub const MAX_THIRST: i32 = 24;

async fn do_recite(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::world::ITEM_SCROLL;
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nRecite what?\r\n".to_string());
    }
    // First token is the scroll keyword; rest (if any) is the target.
    let (scroll_kw, target) = match arg.find(char::is_whitespace) {
        Some(i) => (&arg[..i], arg[i..].trim()),
        None    => (arg, ""),
    };
    let Some((iid, short, value)) = find_consumable(me, world, scroll_kw, ITEM_SCROLL).await else {
        return CmdOutput::text("\r\nYou have no such scroll.\r\n".to_string());
    };

    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} recites {}.\r\n", me.name, short));
    drop(cl);

    let mut text = format!("\r\nYou recite {} which dissolves.\r\n", short);
    for slot in 1..4 {
        let s = apply_item_spell(value[slot], target, me, world, chars).await;
        text.push_str(&s);
    }
    me.inventory.retain(|&i| i != iid);
    {
        let mut w = world.lock().await;
        w.obj_instances.retain(|o| o.id != iid);
    }
    CmdOutput::text(text)
}

/// Wand: cast value[3] spell on a single target (or self), decrement
/// value[2] charges, extract only when charges hit zero.  The proto's
/// `value` is read-only across calls, so the per-instance charge count
/// would normally need its own field — but since wands deplete during
/// gameplay and aren't carried across reboot, we mutate `proto.value[2]`
/// directly. This matches stock CircleMUD which also shares the proto
/// state across all instances of a wand vnum.
async fn do_use(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::world::ITEM_WAND;
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nUse what?\r\n".to_string());
    }
    let (wand_kw, target) = match arg.find(char::is_whitespace) {
        Some(i) => (&arg[..i], arg[i..].trim()),
        None    => (arg, ""),
    };
    let Some((iid, short, value)) = find_consumable(me, world, wand_kw, ITEM_WAND).await else {
        return CmdOutput::text("\r\nYou have no such wand.\r\n".to_string());
    };
    if value[2] <= 0 {
        return CmdOutput::text(format!("\r\n{} seems to be drained.\r\n", short));
    }
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} points {} at {}.\r\n",
            me.name, short,
            if target.is_empty() { "themself" } else { target }));
    drop(cl);

    let spell_vnum = value[3];
    let mut text = format!("\r\nYou point {} at {}.\r\n", short,
        if target.is_empty() { "yourself" } else { target });
    let s = apply_item_spell(spell_vnum, target, me, world, chars).await;
    text.push_str(&s);

    // Decrement charges in the prototype. When this hits 0 the wand is
    // drained but not destroyed — matches CircleMUD parity (the empty
    // wand can be sold/identified before being thrown away).
    let drained = {
        let mut w = world.lock().await;
        if let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) {
            let vnum = o.vnum;
            if let Some(p) = w.obj_protos.get_mut(&vnum) {
                p.value[2] -= 1;
                p.value[2] <= 0
            } else { false }
        } else { false }
    };
    if drained {
        text.push_str("\r\nYou hear a faint crackle — the wand goes inert.\r\n");
    }
    CmdOutput::text(text)
}

/// Staff: cast value[3] spell on each mob in the player's current room
/// (area effect). Same charge decrement as wand.
async fn do_zap(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::world::ITEM_STAFF;
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nZap with what?\r\n".to_string());
    }
    let Some((iid, short, value)) = find_consumable(me, world, arg, ITEM_STAFF).await else {
        return CmdOutput::text("\r\nYou have no such staff.\r\n".to_string());
    };
    if value[2] <= 0 {
        return CmdOutput::text(format!("\r\n{} seems to be drained.\r\n", short));
    }

    // Snapshot mob keyword list so apply_item_spell can dispatch each
    // cast independently without holding the world lock.
    let targets: Vec<String> = {
        let w = world.lock().await;
        w.rooms.get(&me.current_room)
            .map(|r| r.mobs.iter()
                .filter_map(|&mid| w.mob_instances.iter().find(|m| m.id == mid))
                .filter_map(|m| w.mob_protos.get(&m.vnum))
                .filter_map(|p| p.name.split_whitespace().next().map(|s| s.to_string()))
                .collect())
            .unwrap_or_default()
    };

    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} taps {} on the ground.\r\n", me.name, short));
    drop(cl);

    let spell_vnum = value[3];
    let mut text = format!("\r\nYou tap {} on the ground.\r\n", short);
    if targets.is_empty() {
        let s = apply_item_spell(spell_vnum, "", me, world, chars).await;
        text.push_str(&s);
    } else {
        for kw in &targets {
            let s = apply_item_spell(spell_vnum, kw, me, world, chars).await;
            text.push_str(&s);
        }
    }

    let drained = {
        let mut w = world.lock().await;
        if let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) {
            let vnum = o.vnum;
            if let Some(p) = w.obj_protos.get_mut(&vnum) {
                p.value[2] -= 1;
                p.value[2] <= 0
            } else { false }
        } else { false }
    };
    if drained {
        text.push_str("\r\nYou hear a faint crackle — the staff goes inert.\r\n");
    }
    CmdOutput::text(text)
}

/// `light` / `extinguish`: toggle the `light_lit` state on an ITEM_LIGHT
/// in inventory or in the current room. Broadcasts the change so other
/// players can see it.
async fn do_light(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    on: bool,
) -> CmdOutput {
    use crate::world::ITEM_LIGHT;
    let verb = if on { "light" } else { "extinguish" };
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text(format!("\r\n{} what?\r\n", capitalize_first(verb)));
    }
    let kw = arg.to_ascii_lowercase();

    // Search inventory first, then the current room's floor.
    let (iid, short) = {
        let w = world.lock().await;
        let pool: Vec<u32> = me.inventory.iter().copied()
            .chain(w.rooms.get(&me.current_room)
                .map(|r| r.objects.clone()).unwrap_or_default())
            .collect();
        let mut found: Option<(u32, String)> = None;
        for iid in pool {
            let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let Some(p) = w.obj_protos.get(&o.vnum) else { continue; };
            if p.item_type != ITEM_LIGHT { continue; }
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&kw)) {
                found = Some((iid, p.short_description.clone()));
                break;
            }
        }
        match found {
            Some(v) => v,
            None => return CmdOutput::text(format!("\r\nYou see no such light here.\r\n")),
        }
    };

    // Toggle the lit state under a fresh lock.  Refuse no-op transitions.
    let already = {
        let mut w = world.lock().await;
        let o = match w.obj_instances.iter_mut().find(|o| o.id == iid) {
            Some(o) => o,
            None    => return CmdOutput::text("\r\nIt's gone now.\r\n".to_string()),
        };
        if o.light_lit == on { true } else { o.light_lit = on; false }
    };
    if already {
        return CmdOutput::text(format!(
            "\r\n{} is already {}.\r\n",
            short, if on { "lit" } else { "out" },
        ));
    }
    let cl = chars.lock().await;
    let broadcast = if on {
        format!("{} lights {}.\r\n", me.name, short)
    } else {
        format!("{} extinguishes {}.\r\n", me.name, short)
    };
    cl.broadcast_room(me.current_room, Some(me.id), &broadcast);
    CmdOutput::text(format!(
        "\r\nYou {} {}.\r\n",
        verb, short,
    ))
}

fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None    => String::new(),
    }
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
            // EX_HIDDEN exits don't show up in obvious exits — players
            // have to `search` to find them.  Immortals see everything.
            if (e.exit_info & crate::world::EX_HIDDEN) != 0 && me.level < LVL_IMMORT {
                continue;
            }
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

/// Apply (or unapply) an object's `A`-record modifiers to a Character.
/// `direction` is +1 on wear/wield, -1 on remove. Stats clamp to >=0 at
/// the end so removing a STR+1 ring while debuffed doesn't underflow
/// into nonsense — but the bonus_* caches are unclamped (negative is
/// fine, it just means the cumulative bonus is below baseline).
fn apply_obj_affects(me: &mut Character, affects: &[crate::world::ObjAffect], direction: i32) {
    use crate::world::*;
    for a in affects {
        let delta = a.modifier * direction;
        match a.location {
            APPLY_STR => me.str_ = (me.str_ + delta).max(0),
            APPLY_DEX => me.dex  = (me.dex  + delta).max(0),
            APPLY_INT => me.int_ = (me.int_ + delta).max(0),
            APPLY_WIS => me.wis  = (me.wis  + delta).max(0),
            APPLY_CON => me.con  = (me.con  + delta).max(0),
            APPLY_CHA => me.cha  = (me.cha  + delta).max(0),
            APPLY_HIT => {
                me.max_hp = (me.max_hp + delta).max(1);
                me.hp = me.hp.min(me.max_hp);
            }
            APPLY_MANA => {
                me.max_mana = (me.max_mana + delta).max(0);
                me.mana = me.mana.min(me.max_mana);
            }
            APPLY_HITROLL => me.bonus_hitroll += delta,
            APPLY_DAMROLL => me.bonus_damroll += delta,
            APPLY_AC      => me.bonus_ac      += delta,
            _ => {} // unsupported APPLY_* — ignore silently
        }
    }
}

/// Snapshot an object's affected list under a brief world lock — used
/// by the wear/wield/remove paths to avoid holding the lock across
/// `apply_obj_affects` (which mutates `me`).
async fn snapshot_obj_affects(iid: u32, world: &Arc<Mutex<World>>) -> Vec<crate::world::ObjAffect> {
    let w = world.lock().await;
    w.obj_instances.iter().find(|o| o.id == iid)
        .and_then(|o| w.obj_protos.get(&o.vnum))
        .map(|p| p.affected.clone())
        .unwrap_or_default()
}

async fn do_wield(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
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
    let affects = snapshot_obj_affects(iid, world).await;
    apply_obj_affects(me, &affects, 1);
    fire_obj_wear_triggers(iid, &me.name, me.current_room, world, chars).await;
    CmdOutput::text(format!("\r\nYou wield {short}.\r\n"))
}

async fn do_wear(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
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
    let affects = snapshot_obj_affects(iid, world).await;
    apply_obj_affects(me, &affects, 1);
    fire_obj_wear_triggers(iid, &me.name, me.current_room, world, chars).await;
    CmdOutput::text(format!("\r\nYou wear {short} {}.\r\n", wear_pos_name(slot)))
}

async fn do_remove(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
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
    let affects = snapshot_obj_affects(iid, world).await;
    apply_obj_affects(me, &affects, -1);
    fire_obj_remove_triggers(iid, &me.name, me.current_room, world, chars).await;
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
    let proto_info: Option<(i32, [i32; 4], Vec<crate::world::ObjAffect>)> = me.inventory.iter()
        .chain(me.equipment.iter().filter_map(|s| s.as_ref()))
        .find_map(|&iid| {
            let o = w.obj_instances.iter().find(|o| o.id == iid)?;
            let p = w.obj_protos.get(&o.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                Some((p.item_type, p.value, p.affected.clone()))
            } else {
                None
            }
        });

    if let Some((ty, vals, affs)) = proto_info {
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
        if !affs.is_empty() {
            out.push_str("Affects:\r\n");
            for a in &affs {
                let name = apply_name(a.location);
                let sign = if a.modifier >= 0 { "+" } else { "" };
                out.push_str(&format!("  {} by {sign}{}\r\n", name, a.modifier));
            }
        }
        return CmdOutput::text(out);
    }
    base
}

/// Human-readable label for an APPLY_* location.  Returns "?" for
/// values outside the supported set (apply_obj_affects ignores those at
/// apply-time too).
fn apply_name(loc: i32) -> &'static str {
    use crate::world::*;
    match loc {
        APPLY_STR     => "STR",
        APPLY_DEX     => "DEX",
        APPLY_INT     => "INT",
        APPLY_WIS     => "WIS",
        APPLY_CON     => "CON",
        APPLY_CHA     => "CHA",
        APPLY_HIT     => "max HP",
        APPLY_MANA    => "max mana",
        APPLY_AC      => "AC",
        APPLY_HITROLL => "hitroll",
        APPLY_DAMROLL => "damroll",
        _             => "?",
    }
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
    // "give <N> [coins|gold|money] <target>"
    if let Ok(amount) = obj_kw.parse::<i64>() {
        // Strip optional "coins"/"gold"/"money" word.
        let actual_target = if let Some(rest) = target_kw
            .strip_prefix("coins ")
            .or_else(|| target_kw.strip_prefix("gold "))
            .or_else(|| target_kw.strip_prefix("money "))
        {
            rest.trim()
        } else { target_kw };
        return do_give_gold(amount, actual_target, me, world, chars).await;
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
        // Capture obj vnum + keywords for the quest + receive trigger hooks.
        let (obj_vnum, obj_keywords) = w.obj_instances.iter()
            .find(|o| o.id == iid)
            .map(|o| (Some(o.vnum), w.obj_protos.get(&o.vnum)
                .map(|p| p.name.clone()).unwrap_or_default()))
            .unwrap_or((None, String::new()));
        me.inventory.remove(idx);
        if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mid) {
            m.inventory.push(iid);
        }
        drop(w);
        {
            let cl = chars.lock().await;
            cl.broadcast_room(
                me.current_room, Some(me.id),
                &format!("{} gives {} to {}.\r\n", me.name, short, mname),
            );
        }

        let mut msg = format!("\r\nYou give {} to {}.\r\n", short, mname);
        if let Some(ov) = obj_vnum {
            if let Some(qmsg) = quest_check_give(me, ov, mob_vnum, world).await {
                msg.push_str(&qmsg);
            }
        }
        // Fire RECEIVE triggers on the receiving mob.
        fire_mob_receive_triggers(mid, &me.name, &obj_keywords, world, chars).await;
        // Fire GIVE triggers on the given object itself.
        fire_obj_give_triggers(iid, &me.name, me.current_room, world, chars).await;
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

    {
        let cl = chars.lock().await;
        cl.broadcast_room(
            me.current_room, Some(me.id),
            &format!("{} buys {} from {}.\r\n", me.name, short, keeper_name),
        );
    }
    // Fire LOAD triggers on the freshly-spawned shop item.
    fire_obj_load_triggers(iid, &me.name, me.current_room, world, chars).await;

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

/// Hand `amount` gold to a target named `target_kw`.  Target may be a
/// player in the room or a mob in the room.  Mob recipients fire BRIBE
/// triggers.  Insufficient funds aborts.
async fn do_give_gold(
    amount: i64,
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if amount <= 0 {
        return CmdOutput::text("\r\nGive how much gold?\r\n");
    }
    if me.gold < amount {
        return CmdOutput::text(format!(
            "\r\nYou don't have {amount} gold to give. (You have {}.)\r\n",
            me.gold,
        ));
    }
    let tlow = target_kw.to_ascii_lowercase();

    // Player target first.
    let target_handle = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p|
            p.current_room == me.current_room && p.name.to_ascii_lowercase() == tlow
        ).cloned();
        h
    };
    if let Some(ph) = target_handle {
        me.gold -= amount;
        {
            let mut c = ph.character.lock().await;
            c.gold += amount;
        }
        let _ = ph.send.send(format!(
            "\r\n{} gives you {amount} gold.\r\n", me.name,
        ));
        let cl = chars.lock().await;
        cl.broadcast_room(
            me.current_room, Some(me.id),
            &format!("{} gives some gold to {}.\r\n", me.name, ph.name),
        );
        return CmdOutput::text(format!(
            "\r\nYou give {amount} gold to {}. (Now {} left.)\r\n",
            ph.name, me.gold,
        ));
    }

    // Mob target.
    let mob_match: Option<(u32, String)> = {
        let w = world.lock().await;
        let r = w.rooms.get(&me.current_room);
        r.and_then(|r| r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&tlow)) {
                Some((mid, p.short_descr.clone()))
            } else { None }
        }))
    };
    if let Some((mid, mname)) = mob_match {
        me.gold -= amount;
        {
            let cl = chars.lock().await;
            cl.broadcast_room(
                me.current_room, Some(me.id),
                &format!("{} gives some gold to {}.\r\n", me.name, mname),
            );
        }
        // Fire BRIBE triggers on the receiver.
        fire_mob_bribe_triggers(mid, &me.name, amount, world, chars).await;
        return CmdOutput::text(format!(
            "\r\nYou give {amount} gold to {mname}. (Now {} left.)\r\n",
            me.gold,
        ));
    }

    CmdOutput::text(format!("\r\nNo one called '{target_kw}' is here.\r\n"))
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
            r.hunger          = me.hunger;
            r.thirst          = me.thirst;
            r.title           = me.title.clone();
            r.bank_gold       = me.bank_gold;
            r.prompt_format   = me.prompt_format.clone();
            r.aliases         = me.aliases.clone();
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

/// ROOM_DEATH handler: the player has stepped into a death trap.  Their
/// inventory is dropped into the trap (visible to anyone who later
/// passes through), their HP is reset to a single point, and they
/// respawn at the mortal start room.  Broadcasts a death notice to
/// everyone online so other players can react.
async fn death_trap(
    me: &mut Character,
    death_room: crate::world::RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let from_room = me.current_room;
    // Move the inventory contents into the death room (so the corpse
    // pile is recoverable). Equipped gear stays on the body for
    // simplicity — same as our normal player_death() behavior in combat.
    let inv: Vec<u32> = std::mem::take(&mut me.inventory);
    {
        let mut w = world.lock().await;
        if let Some(r) = w.rooms.get_mut(&death_room) {
            for &iid in &inv {
                r.objects.push(iid);
            }
        }
        for &iid in &inv {
            if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
                o.in_room = death_room;
            }
        }
    }
    // Respawn at mortal start, restore HP/mana to a sliver.
    let start = {
        let w = world.lock().await;
        w.start_room(false)
    };
    me.current_room = start;
    me.hp   = 1;
    me.mana = me.max_mana;
    me.fighting = None;
    {
        let mut cl = chars.lock().await;
        cl.update_room(me.id, start);
        cl.broadcast_room(from_room, Some(me.id),
            &format!("{} leaves to the {}.\r\n", me.name, "void"));
        cl.broadcast_room(start, Some(me.id),
            &format!("{} appears in a flash of light, looking shaken.\r\n", me.name));
        for ph in cl.iter() {
            if ph.id == me.id { continue; }
            let _ = ph.send.send(format!(
                "\r\n*** {} has been killed by a deathtrap. ***\r\n", me.name,
            ));
        }
    }
    let view = render_room(start, Some(me.id), world, chars).await;
    CmdOutput::text(format!(
        "\r\nYou step forward — and the world goes black.\r\n\
         You wake in a familiar place, drained but alive.\r\n{view}",
    ))
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
    let (target, closed, door_kw, target_flags) = match &r.exits[dir as usize] {
        Some(e) if e.to_room != crate::world::NOWHERE
            && w.rooms.contains_key(&e.to_room) =>
        {
            let closed = (e.exit_info & crate::world::EX_CLOSED) != 0;
            let kw = if e.keyword.is_empty() { "door".to_string() }
                     else { e.keyword.split_whitespace().next().unwrap_or("door").to_string() };
            let flags = w.rooms.get(&e.to_room).map(|r| r.room_flags[0]).unwrap_or(0);
            (e.to_room, closed, kw, flags)
        }
        _ => return CmdOutput::text(format!("\r\nAlas, you cannot go that way...\r\n")),
    };
    drop(w);
    if closed {
        return CmdOutput::text(format!("\r\nThe {door_kw} is closed.\r\n"));
    }
    // ROOM_GODROOM: mortals can't enter.
    if (target_flags & crate::world::ROOM_GODROOM) != 0 && me.level < LVL_IMMORT {
        return CmdOutput::text("\r\nYou aren't godly enough to enter that room.\r\n".to_string());
    }
    // ROOM_DEATH: instant death for mortals.  Drops inventory in the
    // death room, respawns at the mortal start. Immortals just enter.
    if (target_flags & crate::world::ROOM_DEATH) != 0 && me.level < LVL_IMMORT {
        return death_trap(me, target, world, chars).await;
    }
    // ROOM_TUNNEL / ROOM_PRIVATE: cap occupancy by player count.
    // Immortals bypass both.
    if me.level < LVL_IMMORT &&
        (target_flags & (crate::world::ROOM_TUNNEL | crate::world::ROOM_PRIVATE)) != 0
    {
        let occupants = {
            let cl = chars.lock().await;
            cl.iter().filter(|p| p.current_room == target).count()
        };
        if (target_flags & crate::world::ROOM_TUNNEL) != 0 && occupants >= 1 {
            return CmdOutput::text("\r\nThere isn't enough room for you to enter.\r\n".to_string());
        }
        if (target_flags & crate::world::ROOM_PRIVATE) != 0 && occupants >= 2 {
            return CmdOutput::text("\r\nThat room is private — there's no room for a third.\r\n".to_string());
        }
    }

    let from_room = me.current_room;
    // Fire LEAVE triggers on the source room *before* the player is
    // gone — the script can still see them in the room via %actor.*%.
    fire_room_leave_triggers(&me.name, from_room, world, chars).await;
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

    // Fire greet triggers on mobs in the destination room.
    fire_greet_triggers(me, target, world, chars).await;

    // Show the new room — and append any quest-room hit.
    let mut view = render_room(target, Some(me.id), world, chars).await;
    if let Some(qmsg) = quest_check_room(me, target, world).await {
        view.push_str(&qmsg);
    }

    // Drag any followers who were with us in from_room and aren't busy.
    // Each follower's Character is behind its own mutex; lock them
    // one at a time after we've released ours.  Fighting followers
    // stay behind.
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    for ph in handles {
        if ph.id == me.id { continue; }
        let should_drag = {
            let c = ph.character.lock().await;
            c.following == Some(me.id)
                && c.current_room == from_room
                && c.fighting.is_none()
        };
        if !should_drag { continue; }
        {
            let mut c = ph.character.lock().await;
            c.current_room = target;
        }
        {
            let mut cl = chars.lock().await;
            cl.update_room(ph.id, target);
            cl.broadcast_room(from_room, Some(ph.id),
                &format!("{} follows {}.\r\n", ph.name, me.name));
            cl.broadcast_room(target, Some(ph.id),
                &format!("{} has arrived.\r\n", ph.name));
        }
        let _ = ph.send.send(format!("\r\nYou follow {}.\r\n", me.name));
        let follower_view = render_room(target, Some(ph.id), world, chars).await;
        let _ = ph.send.send(follower_view);
    }

    CmdOutput::text(view)
}

/// One output line from an executed trigger script.  Different DG
/// command verbs map to different presentation styles.
enum ScriptOut {
    /// "mob_name says, '...'" broadcast in `room`.
    Say { mob_name: String, text: String, room: RoomVnum },
    /// "mob_name <text>" — used by both `memote` and `mecho` (mecho is raw
    /// room broadcast, treated identically here for simplicity).
    /// `room` defaults to ctx.self_room but `mat` may override it.
    Echo { text: String, room: RoomVnum },
    /// Spawn an object of this vnum into the mob's room.
    Load { vnum: i32, room: RoomVnum },
    /// Move the self mob to the given room (`mgoto`).
    MobGoto { mob_id: u32, mob_name: String, to: RoomVnum },
    /// Teleport a named player to the given room (`mteleport`).
    PlayerTeleport { name: String, to: RoomVnum },
    /// Extract the self mob silently (`mpurge`).  Inventory is destroyed.
    Purge { mob_id: u32, mob_name: String, room: RoomVnum },
    /// Inflict raw damage on a target by name (`mdamage`).  The target is
    /// either a player (matched against PlayerHandle.name) or a mob in
    /// the script's `self_room` (matched against mob_proto.name keywords).
    Damage { target: String, amount: i32, mob_name: String, room: RoomVnum },
    /// Force a named player to execute a command (`mforce`).  Dispatched
    /// via the global PlayerDb handle established by `server::run`.
    ForceCommand { player: String, command: String },
}

/// Per-script-execution context carrying mutable variables and the
/// host-environment values (actor name, self/mob name, current room).
struct ScriptCtx<'a> {
    actor_name:    &'a str,
    actor_hp:      i32,
    actor_level:   i32,
    actor_gold:    i64,
    actor_class:   String,
    mob_name:      &'a str,
    /// Instance id of the "self" mob when this script is attached to a
    /// mob.  None for room/obj scripts; commands like `mgoto`/`mpurge`
    /// no-op when this is unset.
    self_mob_id:   Option<u32>,
    self_hp:       i32,
    self_max_hp:   i32,
    self_level:    i32,
    self_fighting: bool,
    self_room:     RoomVnum,
    room_people:   i32,
    /// Optional direction the actor came from (e.g. "south") — set by
    /// the caller for greet triggers when known.  Empty for others.
    direction:     String,
    vars:          std::collections::HashMap<String, String>,
}

/// Owned snapshot of the dynamic state of an executing script.  Used to
/// suspend at a `wait` and resume after the sleep elapses.
#[derive(Clone)]
struct ResumeState {
    pc:     usize,
    vars:   std::collections::HashMap<String, String>,
    frames: Vec<Frame>,
}

/// Frame variant used by both if/else and while loops.  Moved out of
/// `execute_script` so it can be stored in `ResumeState`.
#[derive(Clone)]
enum Frame {
    If    { skip: bool, in_else: bool },
    While { skip: bool, start_pc: usize, cond: String, iters: i32 },
}

/// Return value of a single script chunk.  `Done` means the script ran
/// to completion in this chunk.  `Paused` means we hit `wait N sec` —
/// caller should flush outputs, sleep `wait_secs`, then call again with
/// `Some(resume)`.
enum ScriptResult {
    Done(Vec<ScriptOut>),
    Paused {
        outputs:   Vec<ScriptOut>,
        wait_secs: u64,
        resume:    ResumeState,
    },
}

/// Bundle of trigger inputs to keep the `execute_script` signature sane
/// as more variables enter the picture.  Numeric fields default to 0
/// when not available to the caller.
#[derive(Default, Clone)]
pub struct ScriptInputs {
    pub actor_hp:      i32,
    pub actor_level:   i32,
    pub actor_gold:    i64,
    pub actor_class:   String,
    pub self_mob_id:   Option<u32>,
    pub self_hp:       i32,
    pub self_max_hp:   i32,
    pub self_level:    i32,
    pub self_fighting: bool,
    pub room_people:   i32,
    pub direction:     String,
}

/// Execute one trigger script.  Returns a list of pending side-effects
/// to apply under the chars lock.  Supports:
///   - `set <var> <expr>` for variable assignment
///   - `if <cond>` / `end` (single-level, no nesting)
///   - `%var%` substitution (built-in + user-set)
///   - `say` / `mecho` / `memote` / `mload [obj] <vnum>`
/// Nested if, while/loops, eval expressions are still skipped silently.
/// Run a trigger script, returning the outputs that should be applied
/// immediately. If the script hits a `wait`, the remainder is spawned as
/// a background tokio task that sleeps and resumes through subsequent
/// chunks. Callers don't need to be aware of suspension.
fn execute_script(
    t: &crate::world::Trigger,
    actor_name: &str,
    mob_name: &str,
    self_room: RoomVnum,
    inputs: &ScriptInputs,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> Vec<ScriptOut> {
    match execute_script_chunk(t, actor_name, mob_name, self_room, inputs, None) {
        ScriptResult::Done(out) => out,
        ScriptResult::Paused { outputs, wait_secs, resume } => {
            // Clone everything the resume task needs to live for the
            // duration of its sleeps.
            let trig   = t.clone();
            let actor  = actor_name.to_string();
            let mob    = mob_name.to_string();
            let inputs = inputs.clone();
            let world  = Arc::clone(world);
            let chars  = Arc::clone(chars);
            tokio::spawn(async move {
                let mut state = resume;
                let mut secs  = wait_secs;
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                    let res = execute_script_chunk(
                        &trig, &actor, &mob, self_room, &inputs, Some(state),
                    );
                    match res {
                        ScriptResult::Done(out) => {
                            apply_script_outputs(out, self_room, &world, &chars).await;
                            return;
                        }
                        ScriptResult::Paused { outputs, wait_secs, resume: ns } => {
                            apply_script_outputs(outputs, self_room, &world, &chars).await;
                            state = ns;
                            secs  = wait_secs;
                        }
                    }
                }
            });
            outputs
        }
    }
}

/// Chunked execution: runs the script from `state` (or from scratch when
/// state is None) until completion or until a `wait N sec` is reached.
/// `Paused` carries an opaque `ResumeState` to feed back in.
fn execute_script_chunk(
    t: &crate::world::Trigger,
    actor_name: &str,
    mob_name: &str,
    self_room: RoomVnum,
    inputs: &ScriptInputs,
    state: Option<ResumeState>,
) -> ScriptResult {
    use rand::Rng;
    // Probability gate only applies on the FIRST chunk (state is None).
    if state.is_none() && t.narg < 100 && rand::thread_rng().gen_range(0..100) >= t.narg {
        return ScriptResult::Done(Vec::new());
    }
    let (mut pc, mut vars, mut stack) = match state {
        Some(s) => (s.pc, s.vars, s.frames),
        None    => (0, std::collections::HashMap::new(), Vec::new()),
    };
    let mut ctx = ScriptCtx {
        actor_name,
        actor_hp:      inputs.actor_hp,
        actor_level:   inputs.actor_level,
        actor_gold:    inputs.actor_gold,
        actor_class:   inputs.actor_class.clone(),
        mob_name,
        self_mob_id:   inputs.self_mob_id,
        self_hp:       inputs.self_hp,
        self_max_hp:   inputs.self_max_hp,
        self_level:    inputs.self_level,
        self_fighting: inputs.self_fighting,
        self_room,
        room_people:   inputs.room_people,
        direction:     inputs.direction.clone(),
        vars,
    };
    let mut out = Vec::new();
    let frame_skip = |f: &Frame| match f {
        Frame::If { skip, .. } => *skip,
        Frame::While { skip, .. } => *skip,
    };
    // Safety net: scripts that loop forever shouldn't lock the server.
    let mut total_iters: i32 = 0;
    const MAX_TOTAL_ITERS: i32 = 2000;

    while pc < t.commands.len() {
        let raw = &t.commands[pc];
        let line = raw.trim();
        pc += 1;
        if line.is_empty() || line.starts_with('*') { continue; }
        total_iters += 1;
        if total_iters > MAX_TOTAL_ITERS { break; }

        // Block control: handled regardless of skip state.
        if line == "end" {
            // If the closing frame is a While whose cond is still true,
            // iterate by jumping back to start_pc+1.  Otherwise pop.
            if let Some(Frame::While { skip, start_pc, cond, iters }) = stack.last() {
                if !*skip && *iters < 100 && eval_condition(cond, &ctx) {
                    let sp = *start_pc;
                    if let Some(Frame::While { iters, .. }) = stack.last_mut() {
                        *iters += 1;
                    }
                    pc = sp + 1;
                    continue;
                }
            }
            stack.pop();
            continue;
        }
        if line == "else" {
            // Only flip the innermost If frame; ignore on While frames.
            if let Some(Frame::If { skip, in_else }) = stack.last_mut() {
                if !*in_else {
                    *in_else = true;
                    *skip = !*skip;
                }
            }
            continue;
        }
        if let Some(cond) = line.strip_prefix("if ") {
            let outer_skipping = stack.iter().any(frame_skip);
            let frame_skip_val = if outer_skipping { true } else { !eval_condition(cond, &ctx) };
            stack.push(Frame::If { skip: frame_skip_val, in_else: false });
            continue;
        }
        if let Some(cond) = line.strip_prefix("while ") {
            let outer_skipping = stack.iter().any(frame_skip);
            let cond_text = cond.to_string();
            let frame_skip_val = if outer_skipping {
                true
            } else {
                !eval_condition(&cond_text, &ctx)
            };
            stack.push(Frame::While {
                skip: frame_skip_val,
                start_pc: pc - 1,   // index of the `while` line itself
                cond: cond_text,
                iters: 0,
            });
            continue;
        }
        if stack.iter().any(frame_skip) { continue; }

        // `wait <N> sec` — suspend the script for N seconds.  Encode
        // the remaining state into a ResumeState; caller awaits the
        // sleep then re-invokes execute_script_chunk with `Some(state)`.
        if let Some(rest) = line.strip_prefix("wait ") {
            let secs = parse_wait_seconds(&substitute(&ctx, rest));
            let vars_taken = std::mem::take(&mut ctx.vars);
            return ScriptResult::Paused {
                outputs:   out,
                wait_secs: secs,
                resume:    ResumeState {
                    pc,
                    vars:   vars_taken,
                    frames: stack,
                },
            };
        }

        // set <var> <expr>
        if let Some(rest) = line.strip_prefix("set ") {
            let mut parts = rest.splitn(2, char::is_whitespace);
            if let (Some(var), Some(val)) = (parts.next(), parts.next()) {
                let expanded = substitute(&ctx, val);
                ctx.vars.insert(var.to_string(), expanded);
            }
            continue;
        }

        // eval <var> <expr> — evaluate a binary arithmetic expression
        // and store the integer result; falls back to substituted text
        // if either operand isn't numeric.
        if let Some(rest) = line.strip_prefix("eval ") {
            let mut parts = rest.splitn(2, char::is_whitespace);
            if let (Some(var), Some(expr)) = (parts.next(), parts.next()) {
                let result = eval_expr(&ctx, expr);
                ctx.vars.insert(var.to_string(), result);
            }
            continue;
        }
        // `mat <room> <cmd>` — retarget a single inner command at a
        // different room.  Only supports the simple-command verbs (no
        // nested if/while/wait).
        if let Some(rest) = line.strip_prefix("mat ") {
            let mut parts = rest.splitn(2, char::is_whitespace);
            if let (Some(room_str), Some(inner)) = (parts.next(), parts.next()) {
                if let Ok(new_room) = substitute(&ctx, room_str.trim()).parse::<i32>() {
                    let saved = ctx.self_room;
                    ctx.self_room = new_room;
                    exec_simple_command(&mut ctx, inner.trim(), &mut out);
                    ctx.self_room = saved;
                }
            }
            continue;
        }

        exec_simple_command(&mut ctx, line, &mut out);
    }
    ScriptResult::Done(out)
}

/// Match `line` against the simple-command verbs (say/memote/mecho/mload/
/// mgoto/mteleport/mdamage/mpurge/mforce) and push the corresponding
/// `ScriptOut`. Returns true if the line was a known verb (even if no
/// output was produced because of bad arguments).  Used both inline and
/// as the body of `mat <room> <cmd>` so the latter doesn't need to
/// re-implement command parsing.
fn exec_simple_command(ctx: &mut ScriptCtx, line: &str, out: &mut Vec<ScriptOut>) -> bool {
    if let Some(rest) = line.strip_prefix("say ") {
        out.push(ScriptOut::Say {
            mob_name: ctx.mob_name.to_string(),
            text:     substitute(ctx, rest),
            room:     ctx.self_room,
        });
        return true;
    }
    if let Some(rest) = line.strip_prefix("memote ") {
        let body = substitute(ctx, rest);
        out.push(ScriptOut::Echo {
            text: format!("{} {body}\r\n", ctx.mob_name),
            room: ctx.self_room,
        });
        return true;
    }
    if let Some(rest) = line.strip_prefix("mecho ") {
        out.push(ScriptOut::Echo {
            text: format!("{}\r\n", substitute(ctx, rest)),
            room: ctx.self_room,
        });
        return true;
    }
    if let Some(rest) = line.strip_prefix("mload obj ") {
        if let Ok(vnum) = substitute(ctx, rest.trim()).parse::<i32>() {
            out.push(ScriptOut::Load { vnum, room: ctx.self_room });
        }
        return true;
    }
    if let Some(rest) = line.strip_prefix("mload ") {
        if let Ok(vnum) = substitute(ctx, rest.trim()).parse::<i32>() {
            out.push(ScriptOut::Load { vnum, room: ctx.self_room });
        }
        return true;
    }
    if let Some(rest) = line.strip_prefix("mgoto ") {
        if let (Some(mid), Ok(to)) = (ctx.self_mob_id,
            substitute(ctx, rest.trim()).parse::<i32>())
        {
            out.push(ScriptOut::MobGoto {
                mob_id: mid, mob_name: ctx.mob_name.to_string(), to,
            });
        }
        return true;
    }
    if let Some(rest) = line.strip_prefix("mteleport ") {
        let mut parts = rest.splitn(2, char::is_whitespace);
        if let (Some(name), Some(room_str)) = (parts.next(), parts.next()) {
            let n = substitute(ctx, name.trim());
            if let Ok(to) = substitute(ctx, room_str.trim()).parse::<i32>() {
                out.push(ScriptOut::PlayerTeleport { name: n, to });
            }
        }
        return true;
    }
    if let Some(rest) = line.strip_prefix("mdamage ") {
        let mut parts = rest.splitn(2, char::is_whitespace);
        if let (Some(target), Some(amt_str)) = (parts.next(), parts.next()) {
            let t = substitute(ctx, target.trim());
            if let Ok(a) = substitute(ctx, amt_str.trim()).parse::<i32>() {
                out.push(ScriptOut::Damage {
                    target: t,
                    amount: a,
                    mob_name: ctx.mob_name.to_string(),
                    room:   ctx.self_room,
                });
            }
        }
        return true;
    }
    if line == "mpurge" || line.starts_with("mpurge ") {
        if let Some(mid) = ctx.self_mob_id {
            out.push(ScriptOut::Purge {
                mob_id:   mid,
                mob_name: ctx.mob_name.to_string(),
                room:     ctx.self_room,
            });
        }
        return true;
    }
    if let Some(rest) = line.strip_prefix("mforce ") {
        let mut parts = rest.splitn(2, char::is_whitespace);
        if let (Some(name), Some(cmd)) = (parts.next(), parts.next()) {
            let n = substitute(ctx, name.trim());
            let c = substitute(ctx, cmd.trim());
            if !n.is_empty() && !c.is_empty() {
                out.push(ScriptOut::ForceCommand { player: n, command: c });
            }
        }
        return true;
    }
    false
}

/// Parse the number-of-seconds operand from a `wait` line.  Accepts
/// `wait 5`, `wait 5 sec`, `wait 5 seconds`, and `wait 5s`.  Falls back
/// to 1 second on parse failure (matches CircleMUD's default).
fn parse_wait_seconds(s: &str) -> u64 {
    let s = s.trim();
    // Strip trailing unit suffix if present.
    let s = s.strip_suffix(" seconds").or_else(|| s.strip_suffix(" sec"))
        .or_else(|| s.strip_suffix("s")).unwrap_or(s);
    s.trim().parse::<u64>().unwrap_or(1)
}

/// Substitute %var% tokens in `s` against the context's built-ins and
/// user-set variables.  Unknown vars expand to the empty string.
fn substitute(ctx: &ScriptCtx, s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut iter = s.chars().peekable();
    while let Some(c) = iter.next() {
        if c != '%' { out.push(c); continue; }
        let mut var = String::new();
        while let Some(&nc) = iter.peek() {
            iter.next();
            if nc == '%' { break; }
            var.push(nc);
        }
        if var.is_empty() {
            // `%%` → literal `%`
            out.push('%');
            continue;
        }
        out.push_str(&resolve_var(ctx, &var));
    }
    out
}

fn resolve_var(ctx: &ScriptCtx, name: &str) -> String {
    use rand::Rng;
    match name {
        "actor.name"     => ctx.actor_name.to_string(),
        "actor.is_pc"    => "1".to_string(),
        "actor.hp"       => ctx.actor_hp.to_string(),
        "actor.level"    => ctx.actor_level.to_string(),
        "actor.gold"     => ctx.actor_gold.to_string(),
        "actor.class"    => ctx.actor_class.clone(),
        "self.name"      => ctx.mob_name.to_string(),
        "self.hp"        => ctx.self_hp.to_string(),
        "self.maxhp"     => ctx.self_max_hp.to_string(),
        "self.level"     => ctx.self_level.to_string(),
        "self.fighting"  => if ctx.self_fighting { "1".into() } else { "0".into() },
        "self.room.vnum" => ctx.self_room.to_string(),
        "room.people"    => ctx.room_people.to_string(),
        "direction"      => ctx.direction.clone(),
        "random.dir"     => {
            use rand::seq::SliceRandom;
            let dirs = ["north","east","south","west","up","down"];
            dirs.choose(&mut rand::thread_rng()).copied().unwrap_or("north").to_string()
        }
        // %random.N% — uniform 1..=N integer roll.
        other if other.starts_with("random.") => {
            let n_str = &other["random.".len()..];
            if let Ok(n) = n_str.parse::<i32>() {
                if n >= 1 {
                    return rand::thread_rng().gen_range(1..=n).to_string();
                }
            }
            String::new()
        }
        // User-set vars or unknown.
        other => ctx.vars.get(other).cloned().unwrap_or_default(),
    }
}

/// Evaluate `<a> <op> <b>` integer arithmetic.  Operators: +, -, *, /, %.
/// Falls back to the substituted text if either operand isn't an integer.
/// Division by zero yields "0".
fn eval_expr(ctx: &ScriptCtx, expr: &str) -> String {
    let sub = substitute(ctx, expr);
    let tokens: Vec<&str> = sub.split_whitespace().collect();
    if tokens.len() != 3 {
        return sub;
    }
    let (Ok(a), Ok(b)) = (tokens[0].parse::<i64>(), tokens[2].parse::<i64>()) else {
        return sub;
    };
    let v = match tokens[1] {
        "+" => a + b,
        "-" => a - b,
        "*" => a * b,
        "/" => if b == 0 { 0 } else { a / b },
        "%" => if b == 0 { 0 } else { a % b },
        _   => return sub,
    };
    v.to_string()
}

/// Evaluate a condition. Supports a single comparison or two terms joined
/// with `&&` / `||`.  Comparison operators: ==, !=.  A bare value
/// (no operator) is truthy unless empty or "0".
fn eval_condition(cond: &str, ctx: &ScriptCtx) -> bool {
    let cond = cond.trim();
    if let Some((l, r)) = cond.split_once(" && ") {
        return eval_condition(l, ctx) && eval_condition(r, ctx);
    }
    if let Some((l, r)) = cond.split_once(" || ") {
        return eval_condition(l, ctx) || eval_condition(r, ctx);
    }
    if let Some((l, r)) = cond.split_once(" == ") {
        return substitute(ctx, l.trim()) == substitute(ctx, r.trim());
    }
    if let Some((l, r)) = cond.split_once(" != ") {
        return substitute(ctx, l.trim()) != substitute(ctx, r.trim());
    }
    // Bare truthiness.
    let v = substitute(ctx, cond);
    !v.is_empty() && v != "0" && v != "false"
}

/// Apply a list of script outputs: broadcasts speech/echoes to the room,
/// and spawns any loaded objects into their target rooms.
async fn apply_script_outputs(
    outputs: Vec<ScriptOut>,
    _room: RoomVnum,    // each ScriptOut now carries its own target room
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    if outputs.is_empty() { return; }
    // Bin outputs by side-effect category so we can apply chars-only ops
    // separately from world mutations.
    let mut load_queue: Vec<(i32, RoomVnum)> = Vec::new();
    let mut mob_gotos: Vec<(u32, String, RoomVnum)> = Vec::new();
    let mut purges:    Vec<(u32, String, RoomVnum)> = Vec::new();
    let mut teleports: Vec<(String, RoomVnum)>      = Vec::new();
    let mut damages:   Vec<(String, i32, String, RoomVnum)> = Vec::new();
    let mut forces:    Vec<(String, String)>        = Vec::new();
    {
        let cl = chars.lock().await;
        for out in outputs {
            match out {
                ScriptOut::Say { mob_name, text, room: r } => {
                    cl.broadcast_room(r, None, &format!("{mob_name} says, '{text}'\r\n"));
                }
                ScriptOut::Echo { text, room: r } => {
                    cl.broadcast_room(r, None, &text);
                }
                ScriptOut::Load { vnum, room } => {
                    load_queue.push((vnum, room));
                }
                ScriptOut::MobGoto { mob_id, mob_name, to } => {
                    mob_gotos.push((mob_id, mob_name, to));
                }
                ScriptOut::PlayerTeleport { name, to } => {
                    teleports.push((name, to));
                }
                ScriptOut::Purge { mob_id, mob_name, room } => {
                    purges.push((mob_id, mob_name, room));
                }
                ScriptOut::Damage { target, amount, mob_name, room } => {
                    damages.push((target, amount, mob_name, room));
                }
                ScriptOut::ForceCommand { player, command } => {
                    forces.push((player, command));
                }
            }
        }
    }
    // Apply world mutations under a single lock.
    let mut loaded_iids: Vec<(u32, RoomVnum)> = Vec::new();
    if !load_queue.is_empty() || !mob_gotos.is_empty() || !purges.is_empty() {
        let mut w = world.lock().await;
        for (vnum, rv) in load_queue {
            if let Some(iid) = w.spawn_obj(vnum) {
                if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
                    o.in_room = rv;
                }
                if let Some(r) = w.rooms.get_mut(&rv) {
                    r.objects.push(iid);
                }
                loaded_iids.push((iid, rv));
            }
        }
        for (mob_id, _mob_name, to) in &mob_gotos {
            let from = w.mob_instances.iter()
                .find(|m| m.id == *mob_id).map(|m| m.in_room);
            if let Some(from) = from {
                if from != *to {
                    if let Some(r) = w.rooms.get_mut(&from) { r.mobs.retain(|&id| id != *mob_id); }
                    if let Some(r) = w.rooms.get_mut(to)    { r.mobs.push(*mob_id); }
                    if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == *mob_id) {
                        m.in_room = *to;
                    }
                }
            }
        }
        for (mob_id, _mob_name, room) in &purges {
            if let Some(r) = w.rooms.get_mut(room) {
                r.mobs.retain(|&id| id != *mob_id);
            }
            // Extract any objects the mob was holding too.
            let inv: Vec<u32> = w.mob_instances.iter()
                .find(|m| m.id == *mob_id)
                .map(|m| m.inventory.clone()).unwrap_or_default();
            w.mob_instances.retain(|m| m.id != *mob_id);
            w.obj_instances.retain(|o| !inv.contains(&o.id));
        }
    }
    // Apply damages.  Player target: lookup PlayerHandle, decrement HP,
    // notify via mpsc.  Mob target: find by keyword in target room.
    for (target, amount, mob_name, room) in damages {
        let tlow = target.to_ascii_lowercase();
        // Player path.
        let ph = {
            let cl = chars.lock().await;
            let h = cl.iter()
                .find(|p| p.name.to_ascii_lowercase() == tlow)
                .cloned();
            h
        };
        if let Some(ph) = ph {
            let (cur, max) = {
                let mut c = ph.character.lock().await;
                c.hp -= amount;
                (c.hp, c.max_hp)
            };
            let _ = ph.send.send(format!(
                "\r\n{mob_name} hits you with raw force for {amount} damage! ({cur}/{max} HP)\r\n",
            ));
            continue;
        }
        // Mob path: keyword match in `room`.
        let mut w = world.lock().await;
        let room_mobs: Vec<u32> = w.rooms.get(&room)
            .map(|r| r.mobs.clone()).unwrap_or_default();
        for mid in room_mobs {
            let proto_match = w.mob_instances.iter().find(|m| m.id == mid)
                .and_then(|m| w.mob_protos.get(&m.vnum))
                .map(|p| p.name.split_whitespace()
                    .any(|k| k.eq_ignore_ascii_case(&tlow)))
                .unwrap_or(false);
            if !proto_match { continue; }
            if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mid) {
                m.hp -= amount;
            }
            break;
        }
    }

    // Announce mob movements + apply teleports under the chars lock.
    if !mob_gotos.is_empty() || !purges.is_empty() || !teleports.is_empty() {
        let cl = chars.lock().await;
        for (_, mob_name, to) in &mob_gotos {
            cl.broadcast_room(*to, None, &format!("{mob_name} appears in a puff of smoke.\r\n"));
        }
        for (_, mob_name, room) in &purges {
            cl.broadcast_room(*room, None, &format!("{mob_name} dissolves into nothingness.\r\n"));
        }
        // Player teleports.
        let handles: Vec<crate::character::PlayerHandle> = cl.iter().cloned().collect();
        drop(cl);
        for (name, to) in teleports {
            let Some(ph) = handles.iter().find(|p| p.name.eq_ignore_ascii_case(&name)).cloned() else { continue; };
            // Update character + registry, broadcast departure/arrival.
            let from_room = {
                let mut c = ph.character.lock().await;
                let f = c.current_room;
                c.current_room = to;
                f
            };
            {
                let mut cl = chars.lock().await;
                cl.update_room(ph.id, to);
                cl.broadcast_room(from_room, Some(ph.id),
                    &format!("{} vanishes in a flash.\r\n", ph.name));
                cl.broadcast_room(to, Some(ph.id),
                    &format!("{} appears in a flash.\r\n", ph.name));
            }
            let _ = ph.send.send(format!("\r\nThe world swirls — you find yourself elsewhere.\r\n"));
        }
    }
    // NOTE: LOAD triggers are deliberately NOT fired for mload-spawned
    // objects to avoid recursive async (apply -> fire_obj_load ->
    // fire_obj_triggers -> apply).  Callers that spawn objects via
    // do_buy / do_quest_complete fire LOAD triggers themselves.
    let _ = loaded_iids;

    // `mforce` — post to the global runner so the recursion (script ->
    // force -> dispatch -> script) crosses an mpsc boundary instead of
    // a direct async-fn call (which would form an opaque-type cycle).
    if !forces.is_empty() {
        if let Some(tx) = FORCE_CMD_TX.get() {
            for (player, command) in forces {
                let _ = tx.send(ForceCmdMsg {
                    player,
                    command,
                    world: Arc::clone(world),
                    chars: Arc::clone(chars),
                });
            }
        }
    }
}

/// Long-lived consumer of `FORCE_CMD_TX`. Spawned once by `server::run`.
/// Drains forced-command messages and dispatches each via
/// `dispatch_command` against the named player.
pub async fn force_command_runner(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ForceCmdMsg>,
) {
    while let Some(msg) = rx.recv().await {
        let Some(players_arc) = PLAYERS_HANDLE.get().cloned() else { continue; };
        let ForceCmdMsg { player, command, world, chars } = msg;
        let ph_opt: Option<crate::character::PlayerHandle> = {
            let cl = chars.lock().await;
            let h = cl.iter().find(|p| p.name.eq_ignore_ascii_case(&player)).cloned();
            h
        };
        let Some(ph) = ph_opt else { continue; };
        let _ = ph.send.send(format!("\r\n{}\r\n", command));
        let result = {
            let mut c = ph.character.lock().await;
            dispatch_command(&command, &mut c, &world, &chars, &players_arc).await
        };
        if !result.text.is_empty() {
            let _ = ph.send.send(result.text);
        }
    }
}

/// Fire all triggers of the given type attached to mobs in `room`.
/// `keyword_filter`, when Some, restricts to triggers whose `arg`
/// contains the keyword (used by SPEECH triggers).
async fn fire_mob_triggers(
    actor_name: &str,
    room: RoomVnum,
    trigger_type: char,
    keyword_filter: Option<&str>,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let outputs: Vec<ScriptOut> = {
        let w = world.lock().await;
        let Some(r) = w.rooms.get(&room) else { return; };
        let mut acc: Vec<ScriptOut> = Vec::new();
        for &mid in &r.mobs {
            let Some(m) = w.mob_instances.iter().find(|m| m.id == mid) else { continue; };
            let Some(proto) = w.mob_protos.get(&m.vnum) else { continue; };
            let mob_name = proto.short_descr.clone();
            for &tvnum in &m.triggers {
                let Some(t) = w.triggers.get(&tvnum) else { continue; };
                if t.trigger_type != trigger_type { continue; }
                if let Some(kw) = keyword_filter {
                    // SPEECH triggers: arg is the keyword(s) to match in
                    // the actor's speech.  CircleMUD's matching is loose:
                    // any keyword from arg appearing as a word in the text.
                    let arg_low = t.arg.to_ascii_lowercase();
                    let text_low = kw.to_ascii_lowercase();
                    let any_match = arg_low.split_whitespace()
                        .any(|w| text_low.split_whitespace().any(|t| t == w));
                    if !any_match { continue; }
                }
                let inputs = ScriptInputs {
                    self_mob_id: Some(m.id),
                    self_hp: m.hp, self_max_hp: m.max_hp,
                    self_level: proto.level,
                    self_fighting: m.fighting.is_some(),
                    room_people: 0,
                    ..Default::default()
                };
                acc.extend(execute_script(t, actor_name, &mob_name, room, &inputs, world, chars));
            }
        }
        acc
    };
    apply_script_outputs(outputs, room, world, chars).await;
}

/// Fire all triggers of the given type attached directly to a room.
async fn fire_room_triggers(
    actor_name: &str,
    room: RoomVnum,
    trigger_type: char,
    keyword_filter: Option<&str>,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let outputs: Vec<ScriptOut> = {
        let w = world.lock().await;
        let Some(r) = w.rooms.get(&room) else { return; };
        let room_name = r.name.clone();
        let mut acc: Vec<ScriptOut> = Vec::new();
        for &tvnum in &r.triggers {
            let Some(t) = w.triggers.get(&tvnum) else { continue; };
            if t.trigger_type != trigger_type { continue; }
            if let Some(kw) = keyword_filter {
                let arg_low  = t.arg.to_ascii_lowercase();
                let text_low = kw.to_ascii_lowercase();
                let any_match = arg_low.split_whitespace()
                    .any(|w| text_low.split_whitespace().any(|t| t == w));
                if !any_match { continue; }
            }
            acc.extend(execute_script(t, actor_name, &room_name, room, &ScriptInputs::default(), world, chars));
        }
        acc
    };
    apply_script_outputs(outputs, room, world, chars).await;
}

/// Public wrapper for room SPEECH triggers ('d' on attach=ROOM). Fired
/// from `do_say` with the spoken text as the keyword filter, mirroring
/// the mob-SPEECH ('d' on MOB) semantics.
pub async fn fire_room_speech_triggers(
    actor_name: &str,
    room: RoomVnum,
    text: &str,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    fire_room_triggers(actor_name, room, 'd', Some(text), world, chars).await;
}

/// Public wrapper for room LEAVE triggers ('q' on attach=ROOM). Fired
/// from `do_move` against the room a player is exiting, before the
/// world state is updated.
pub async fn fire_room_leave_triggers(
    actor_name: &str,
    room: RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    fire_room_triggers(actor_name, room, 'q', None, world, chars).await;
}

/// Fire one of the object-trigger types (GET/DROP/WEAR/REMOVE/GIVE) on
/// the object identified by `obj_iid`.  `room` is where output gets
/// broadcast — typically the actor's current room.
async fn fire_obj_triggers(
    obj_iid: u32,
    actor_name: &str,
    room: RoomVnum,
    trigger_type: char,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let outputs: Vec<ScriptOut> = {
        let w = world.lock().await;
        let Some(o) = w.obj_instances.iter().find(|o| o.id == obj_iid) else {
            return;
        };
        let obj_name = w.obj_protos.get(&o.vnum)
            .map(|p| p.short_description.clone())
            .unwrap_or_else(|| "an object".to_string());
        let mut acc = Vec::new();
        for &tvnum in &o.triggers {
            let Some(t) = w.triggers.get(&tvnum) else { continue; };
            if t.attach_type != crate::world::TRIG_ATTACH_OBJ { continue; }
            if t.trigger_type != trigger_type { continue; }
            acc.extend(execute_script(t, actor_name, &obj_name, room, &ScriptInputs::default(), world, chars));
        }
        acc
    };
    apply_script_outputs(outputs, room, world, chars).await;
}

/// GET trigger ('g' on objects).
pub async fn fire_obj_get_triggers(
    obj_iid: u32,
    actor_name: &str,
    room: RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    fire_obj_triggers(obj_iid, actor_name, room, 'g', world, chars).await;
}

/// DROP trigger ('h' on objects).
pub async fn fire_obj_drop_triggers(
    obj_iid: u32,
    actor_name: &str,
    room: RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    fire_obj_triggers(obj_iid, actor_name, room, 'h', world, chars).await;
}

/// WEAR trigger ('j' on objects).  Fired by both `wear` and `wield`.
pub async fn fire_obj_wear_triggers(
    obj_iid: u32,
    actor_name: &str,
    room: RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    fire_obj_triggers(obj_iid, actor_name, room, 'j', world, chars).await;
}

/// REMOVE trigger ('l' on objects).
pub async fn fire_obj_remove_triggers(
    obj_iid: u32,
    actor_name: &str,
    room: RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    fire_obj_triggers(obj_iid, actor_name, room, 'l', world, chars).await;
}

/// GIVE trigger ('i' on objects) — fires when the object is handed to
/// a mob.
pub async fn fire_obj_give_triggers(
    obj_iid: u32,
    actor_name: &str,
    room: RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    fire_obj_triggers(obj_iid, actor_name, room, 'i', world, chars).await;
}

/// TIMER trigger ('f' on objects) — fires when an object's per-instance
/// timer counts down to zero, immediately before the object is
/// extracted by `spawn_obj_timer_tick`. The object name is used as the
/// actor identity for the script (no player actor in this context).
pub async fn fire_obj_timer_triggers(
    obj_iid: u32,
    room: RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    // Pull the object's short_description for use as the actor name —
    // the OTRIG_TIMER context has no triggering player.
    let actor_name = {
        let w = world.lock().await;
        w.obj_instances.iter()
            .find(|o| o.id == obj_iid)
            .and_then(|o| w.obj_protos.get(&o.vnum))
            .map(|p| p.short_description.clone())
            .unwrap_or_else(|| "an object".to_string())
    };
    fire_obj_triggers(obj_iid, &actor_name, room, 'f', world, chars).await;
}

/// LOAD trigger ('m' on objects) — fires when the object is freshly
/// spawned at runtime (mload, quest reward, shop buy). Not fired for
/// objects restored from a player's saved inventory.
pub async fn fire_obj_load_triggers(
    obj_iid: u32,
    actor_name: &str,
    room: RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    fire_obj_triggers(obj_iid, actor_name, room, 'm', world, chars).await;
}

/// Run FIGHT (type 'k') triggers each combat round for a mob currently
/// engaged with a player.  Provides %actor.name%/%actor.hp% to the
/// script so dynamic combat dialogue is possible.
pub async fn fire_mob_fight_triggers(
    mob_id: u32,
    actor_name: &str,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let (outputs, room) = {
        let w = world.lock().await;
        let Some(m) = w.mob_instances.iter().find(|m| m.id == mob_id) else { return; };
        let Some(proto) = w.mob_protos.get(&m.vnum) else { return; };
        let mob_name = proto.short_descr.clone();
        let mob_room = m.in_room;
        let inputs = ScriptInputs {
            self_mob_id: Some(m.id),
            self_hp: m.hp, self_max_hp: m.max_hp,
            self_level: proto.level,
            ..Default::default()
        };
        let mut acc = Vec::new();
        for &tvnum in &m.triggers {
            let Some(t) = w.triggers.get(&tvnum) else { continue; };
            if t.trigger_type != 'k' { continue; }
            acc.extend(execute_script(t, actor_name, &mob_name, mob_room, &inputs, world, chars));
        }
        (acc, mob_room)
    };
    apply_script_outputs(outputs, room, world, chars).await;
}

/// Run ENTRY (type 'i') triggers when a specific mob has just entered
/// a room.  The mob is the actor in this case.  Called from
/// wander/flee paths in combat.rs / db.rs.
pub async fn fire_mob_entry_triggers(
    mob_id: u32,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let (outputs, room) = {
        let w = world.lock().await;
        let Some(m) = w.mob_instances.iter().find(|m| m.id == mob_id) else {
            return;
        };
        let Some(proto) = w.mob_protos.get(&m.vnum) else { return; };
        let mob_name = proto.short_descr.clone();
        let mob_room = m.in_room;
        let mut acc = Vec::new();
        for &tvnum in &m.triggers {
            let Some(t) = w.triggers.get(&tvnum) else { continue; };
            if t.trigger_type != 'i' { continue; }
            acc.extend(execute_script(t, &mob_name, &mob_name, mob_room, &ScriptInputs::default(), world, chars));
        }
        (acc, mob_room)
    };
    apply_script_outputs(outputs, room, world, chars).await;
}

/// Roll the mob's RANDOM ('b') triggers once. Each matching trigger
/// rolls `narg`% independently — those that pass run.  Caller is the
/// random-trigger tick (`db::spawn_random_trigger_tick`).  No-op if the
/// mob isn't in a room (NOWHERE) since broadcasts won't reach anyone.
pub async fn fire_mob_random_tick(
    mob_id: u32,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    use rand::Rng;
    let (outputs, room) = {
        let w = world.lock().await;
        let Some(m) = w.mob_instances.iter().find(|m| m.id == mob_id) else { return; };
        if m.in_room == crate::world::NOWHERE { return; }
        let Some(proto) = w.mob_protos.get(&m.vnum) else { return; };
        let mob_name = proto.short_descr.clone();
        let mob_room = m.in_room;
        let inputs = ScriptInputs {
            self_mob_id: Some(m.id),
            self_hp: m.hp, self_max_hp: m.max_hp,
            self_level: proto.level,
            ..Default::default()
        };
        let mut acc = Vec::new();
        for &tvnum in &m.triggers {
            let Some(t) = w.triggers.get(&tvnum) else { continue; };
            if t.trigger_type != 'b' { continue; }
            // narg is "% chance to fire per tick" (1..100).
            let chance = t.narg.clamp(0, 100);
            if chance <= 0 { continue; }
            // Re-acquire thread_rng per check — its handle is !Send so it
            // can't live across the .lock().await above.
            if rand::thread_rng().gen_range(0..100) >= chance { continue; }
            acc.extend(execute_script(t, &mob_name, &mob_name, mob_room, &inputs, world, chars));
        }
        (acc, mob_room)
    };
    apply_script_outputs(outputs, room, world, chars).await;
}

/// Roll a room's RANDOM ('b') triggers (attach=ROOM) once. Mirrors
/// `fire_mob_random_tick` but for room-attached scripts.
pub async fn fire_room_random_tick(
    room: RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    use rand::Rng;
    let outputs: Vec<ScriptOut> = {
        let w = world.lock().await;
        let Some(r) = w.rooms.get(&room) else { return; };
        let room_name = r.name.clone();
        let mut acc: Vec<ScriptOut> = Vec::new();
        for &tvnum in &r.triggers {
            let Some(t) = w.triggers.get(&tvnum) else { continue; };
            if t.trigger_type != 'b' { continue; }
            let chance = t.narg.clamp(0, 100);
            if chance <= 0 { continue; }
            if rand::thread_rng().gen_range(0..100) >= chance { continue; }
            acc.extend(execute_script(t, &room_name, &room_name, room, &ScriptInputs::default(), world, chars));
        }
        acc
    };
    apply_script_outputs(outputs, room, world, chars).await;
}

/// Run BRIBE (type 'l') triggers when a mob receives gold from a player.
/// `gold_amount` is passed in via `%actor.gold%` (overrides default).
pub async fn fire_mob_bribe_triggers(
    mob_id: u32,
    actor_name: &str,
    gold_amount: i64,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let (outputs, room) = {
        let w = world.lock().await;
        let Some(m) = w.mob_instances.iter().find(|m| m.id == mob_id) else {
            return;
        };
        let Some(proto) = w.mob_protos.get(&m.vnum) else { return; };
        let mob_name = proto.short_descr.clone();
        let mob_room = m.in_room;
        let inputs = ScriptInputs {
            self_mob_id: Some(m.id),
            self_hp: m.hp, self_max_hp: m.max_hp,
            self_level: proto.level,
            actor_gold: gold_amount,
            self_fighting: m.fighting.is_some(),
            ..Default::default()
        };
        let mut acc = Vec::new();
        for &tvnum in &m.triggers {
            let Some(t) = w.triggers.get(&tvnum) else { continue; };
            if t.trigger_type != 'l' { continue; }
            // CircleMUD's BRIBE narg is the minimum gold threshold to fire.
            if (gold_amount as i32) < t.narg { continue; }
            acc.extend(execute_script(t, actor_name, &mob_name, mob_room, &inputs, world, chars));
        }
        (acc, mob_room)
    };
    apply_script_outputs(outputs, room, world, chars).await;
}

/// Run RECEIVE (type 'j') triggers when a mob receives an object from
/// a player.  `obj_keywords` is the just-received object's keyword
/// string, supplied as the filter (same model as SPEECH triggers).
pub async fn fire_mob_receive_triggers(
    mob_id: u32,
    actor_name: &str,
    obj_keywords: &str,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let (outputs, room) = {
        let w = world.lock().await;
        let Some(m) = w.mob_instances.iter().find(|m| m.id == mob_id) else {
            return;
        };
        let Some(proto) = w.mob_protos.get(&m.vnum) else { return; };
        let mob_name = proto.short_descr.clone();
        let mob_room = m.in_room;
        let mut acc = Vec::new();
        for &tvnum in &m.triggers {
            let Some(t) = w.triggers.get(&tvnum) else { continue; };
            if t.trigger_type != 'j' { continue; }
            // arg keyword match against the obj's keywords (any-of).
            if !t.arg.is_empty() {
                let arg_low  = t.arg.to_ascii_lowercase();
                let obj_low  = obj_keywords.to_ascii_lowercase();
                let any_match = arg_low.split_whitespace()
                    .any(|w| obj_low.split_whitespace().any(|o| o == w));
                if !any_match { continue; }
            }
            acc.extend(execute_script(t, actor_name, &mob_name, mob_room, &ScriptInputs::default(), world, chars));
        }
        (acc, mob_room)
    };
    apply_script_outputs(outputs, room, world, chars).await;
}

/// Run DEATH (type 'f') triggers for a specific mob *before* it is
/// extracted from the world.  Used so dying-mob scripts (last words,
/// loot drops via `mload`) execute against the still-live instance.
pub async fn fire_mob_death_triggers(
    mob_id: u32,
    killer_name: &str,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let outputs: Vec<ScriptOut> = {
        let w = world.lock().await;
        let Some(m) = w.mob_instances.iter().find(|m| m.id == mob_id) else {
            return;
        };
        let Some(proto) = w.mob_protos.get(&m.vnum) else { return; };
        let mob_name = proto.short_descr.clone();
        let mob_room = m.in_room;
        let mut acc = Vec::new();
        for &tvnum in &m.triggers {
            let Some(t) = w.triggers.get(&tvnum) else { continue; };
            if t.trigger_type != 'f' { continue; }
            acc.extend(execute_script(t, killer_name, &mob_name, mob_room, &ScriptInputs::default(), world, chars));
        }
        acc
    };
    if outputs.is_empty() { return; }
    // Take the mob's room for delivery before extraction.
    let mob_room = {
        let w = world.lock().await;
        w.mob_instances.iter().find(|m| m.id == mob_id).map(|m| m.in_room).unwrap_or(crate::world::NOWHERE)
    };
    apply_script_outputs(outputs, mob_room, world, chars).await;
}

/// Convenience: greet triggers from both mob and room sources, plus the
/// quest-room hook.
async fn fire_greet_triggers(
    me: &Character,
    room: RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    fire_mob_triggers(&me.name, room, 'g', None, world, chars).await;
    fire_room_triggers(&me.name, room, 'g', None, world, chars).await;
}

/// Minimal trigger-language variable substitution: replaces `%actor.name%`
/// with the player's name; strips other `%foo%` tokens to keep output
/// readable until a real interpreter lands.
fn substitute_vars(s: &str, actor_name: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut iter = s.chars().peekable();
    while let Some(c) = iter.next() {
        if c != '%' { out.push(c); continue; }
        // Read until the next %.
        let mut var = String::new();
        while let Some(&nc) = iter.peek() {
            iter.next();
            if nc == '%' { break; }
            var.push(nc);
        }
        match var.as_str() {
            "actor.name" => out.push_str(actor_name),
            "" => out.push('%'),  // literal %% → %
            _ => { /* drop unknown vars */ }
        }
    }
    out
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
    // Snapshot viewer flags first: level (for EX_HIDDEN), brief (skips
    // the description block).  Locks the registry briefly and the
    // viewer's Character once each — no contention since dispatch is
    // serial per player.
    let (viewer_level, viewer_brief): (i32, bool) = match viewer_id {
        Some(id) => {
            let ph_opt = {
                let cl = chars.lock().await;
                let h = cl.iter().find(|p| p.id == id).cloned();
                h
            };
            match ph_opt {
                Some(ph) => {
                    let c = ph.character.lock().await;
                    (ph.level, c.brief)
                }
                None => (0, false),
            }
        }
        None => (0, false),
    };

    let w = world.lock().await;
    let Some(r) = w.rooms.get(&vnum) else {
        return "\r\nYou are nowhere.\r\n".to_string();
    };

    let mut s = String::with_capacity(r.description.len() + 512);
    s.push_str("\r\n");
    s.push_str(&r.name);
    s.push_str("\r\n");
    if !viewer_brief {
        for line in r.description.split('\n') {
            s.push_str(line);
            s.push_str("\r\n");
        }
    }

    // Exits — EX_HIDDEN ones are suppressed for mortals (viewer_level
    // was already snapshotted above).
    let exits: Vec<&str> = Direction::ALL.iter()
        .filter(|d| r.exits[**d as usize].as_ref()
            .map(|e| e.to_room != crate::world::NOWHERE
                && (viewer_level >= LVL_IMMORT
                    || e.exit_info & crate::world::EX_HIDDEN == 0))
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

#[cfg(test)]
mod tests {
    use super::parse_wait_seconds;

    #[test]
    fn wait_seconds_parses_common_forms() {
        assert_eq!(parse_wait_seconds("5"),         5);
        assert_eq!(parse_wait_seconds("5 sec"),     5);
        assert_eq!(parse_wait_seconds("5 seconds"), 5);
        assert_eq!(parse_wait_seconds("5s"),        5);
        assert_eq!(parse_wait_seconds("  10  sec"), 10);
    }

    #[test]
    fn wait_seconds_fallback_on_garbage() {
        // unparseable input → safe default (don't hang forever).
        assert!(parse_wait_seconds("forever") >= 1);
        assert!(parse_wait_seconds("") >= 1);
    }
}
