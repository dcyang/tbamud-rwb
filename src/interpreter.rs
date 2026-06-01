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

/// When true, new-character creation is disabled (the `-r` "restrict"
/// boot flag, mirroring `circle_restrict` in comm.c).  Set once at boot
/// in `server::run`; read by `login::NameConfirm` before creating a
/// brand-new mortal.
pub static RESTRICT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether renting is free (stock `free_rent`, default YES).  When true,
/// the receptionist just stores belongings and tells the player to quit;
/// when false the per-day cost machinery (offer breakdown, affordability,
/// login-time accrual) activates.  Runtime atomic so the paid path stays
/// live code; defaults to the stock-shipped value.
pub static FREE_RENT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Shared mutable site-ban list, populated from `lib/etc/badsites` at
/// boot and mutated at runtime by the `ban`/`unban` commands.
pub static BAD_SITES: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();

/// Recent channel chatter, keyed by channel name ("gossip" / "info" /
/// "shout" / "auction").  Each ring is capped at 20 entries.  Used
/// by `do_chans`.
pub static CHANNEL_HISTORY: OnceLock<Arc<Mutex<std::collections::HashMap<&'static str, std::collections::VecDeque<(String, String)>>>>> = OnceLock::new();

/// Push a message into the named channel's history (creating the
/// ring if needed).  Safe to call concurrently.
pub async fn record_channel(channel: &'static str, sender: &str, msg: &str) {
    let cell = CHANNEL_HISTORY.get_or_init(|| {
        Arc::new(Mutex::new(std::collections::HashMap::new()))
    });
    let mut g = cell.lock().await;
    let ring = g.entry(channel).or_insert_with(std::collections::VecDeque::new);
    if ring.len() >= 20 { ring.pop_front(); }
    ring.push_back((sender.to_string(), msg.to_string()));
}

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
    "get", "drop", "junk", "donate", "sacrifice", "sac",
    "put", "give", "wield", "wear", "remove",
    "examine",
    "list", "buy", "sell", "appraise", "value",
    "kick", "bash", "backstab", "whirlwind", "peek", "rescue", "disarm", "consider", "con",
    "sleep", "rest", "sit", "stand", "wake", "bandage",
    "wimpy",
    "info", "newbie", "shout", "color",
    "autoexit", "autoexits", "autoloot", "autoassist", "autotitle",
    "autogold", "autosplit", "autosac", "autodoor", "autokey", "automap",
    "levels", "areas", "history",
    "clan", "ctell", "clans", "map", "whois",
    "sneak", "hide", "steal",
    "cast",
    "skills", "practice", "affects",
    "quest", "where",
    "say", "tell", "who",
    "score", "exp", "equipment", "save", "help",
    "open", "close", "lock", "unlock", "pick", "search",
    "quaff", "drink", "sip", "eat", "taste", "fill", "pour", "empty", "recite", "use", "zap", "light", "extinguish",
    "follow", "group", "gtell", "split", "report", "title", "gossip", "chat",
    "grats", "gratz", "holler", "version", "visible",
    "notell", "nosummon", "nograts", "nogossip", "noauction",
    "auction", "auc", "whisper", "ask", "unfollow", "cls", "norepeat", "hindex",
    "display", "qsay", "enter", "leave", "page", "happyhour", "receive",
    "brief", "compact", "toggle", "time", "weather", "bank", "reply", "prompt", "alias",
    "balance", "deposit", "withdraw", "offer", "rent", "gsay", "take", "hold", "grab", "diagnose", "whoami",
    "news", "credits", "motd", "imotd", "policy", "policies", "handbook", "background",
    "wizlist", "immlist",
    "commands", "scan", "track", "mail", "spells", "recall",
    "board", "boards", "write", "read",
    "emote", "socials", "note", "notes", "pose", "uptime", "peace", "order", "pvp",
    "finger", "assist", "worship", "afk",
    "bug", "idea", "typo",
    "goto", "transfer", "purge", "shutdown", "stat", "force", "set", "oset", "dig", "redit", "oedit", "medit", "zedit", "qedit", "trigedit", "sedit", "aedit", "hedit", "wizlock",
    "at", "househere", "house",
    "zreset", "olist", "mlist", "rlist", "zlist",
    "invis", "vis", "nohassle", "mute", "freeze",
    "ban", "unban", "bans",
    "wiznet",
    "load", "restore", "echo", "gecho", "slay", "snoop", "unsnoop",
    "status", "reload", "spec_assign",
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

    // OLC editing mode: while an editor is active, every line (including
    // blank lines, for the text editor) is routed to the editor rather
    // than the command interpreter.
    if me.olc.is_some() {
        return crate::olc::handle_input(raw, me, world, players).await;
    }

    // History recording deferred to after speech/history-bang expansion
    // so the buffer shows the effective command, not the bang form.

    // Frozen players can do almost nothing.  Allow quit/look/score so
    // they can sign off cleanly and read their state.  Everything else
    // bails with a notice.
    if me.frozen {
        let first = raw.split_whitespace().next().unwrap_or("");
        let allowed = matches!(first, "quit" | "look" | "l" | "score" | "sc");
        if !allowed {
            return CmdOutput::text(
                "\r\nYou are frozen and cannot act.\r\n".to_string(),
            );
        }
    }

    // Speech-prefix shortcuts (single-char leader).  `: foo` → emote;
    // `' foo` → say; `; foo` → gossip.  Translated before alias
    // expansion so users can still alias the underlying verbs.
    let prefixed: String;
    let raw: &str = if let Some(first) = raw.chars().next() {
        let verb = match first {
            ':' => Some("emote"),
            '\'' => Some("say"),
            ';' => Some("gossip"),
            _   => None,
        };
        if let Some(v) = verb {
            let rest = raw[first.len_utf8()..].trim_start();
            prefixed = if rest.is_empty() { v.to_string() }
                       else { format!("{v} {rest}") };
            prefixed.as_str()
        } else {
            raw
        }
    } else {
        raw
    };

    // Shell-style history expansion: `!!` → last entry, `!N` → entry N
    // (1-based, matches `do_history` numbering), `!<prefix>` → most
    // recent entry starting with <prefix>.  An unresolved bang returns
    // an "event not found" notice without dispatching.  Bang expansion
    // runs once — the result isn't fed back through itself.
    let bang_expanded: String;
    let mut bang_notice: Option<String> = None;
    let raw: &str = if let Some(stripped) = raw.strip_prefix('!') {
        if stripped.is_empty() {
            bang_notice = Some("\r\nUsage: !! | !N | !<prefix>\r\n".to_string());
            raw
        } else if stripped == "!" {
            match me.history.back().cloned() {
                Some(s) => { bang_expanded = s; bang_expanded.as_str() }
                None    => {
                    bang_notice = Some("\r\nNo history yet.\r\n".to_string());
                    raw
                }
            }
        } else if let Ok(n) = stripped.parse::<usize>() {
            if n == 0 || n > me.history.len() {
                bang_notice = Some(format!(
                    "\r\n!{n}: event not found (history has {} entries).\r\n",
                    me.history.len(),
                ));
                raw
            } else {
                bang_expanded = me.history[n - 1].clone();
                bang_expanded.as_str()
            }
        } else {
            // Prefix match — rev-scan.
            match me.history.iter().rev()
                .find(|h| h.starts_with(stripped)).cloned()
            {
                Some(s) => { bang_expanded = s; bang_expanded.as_str() }
                None    => {
                    bang_notice = Some(format!(
                        "\r\n!{stripped}: event not found.\r\n",
                    ));
                    raw
                }
            }
        }
    } else {
        raw
    };
    if let Some(n) = bang_notice {
        return CmdOutput::text(n);
    }

    // Record the (post speech/bang) command in the rolling history.
    if me.history.len() >= 20 { me.history.pop_front(); }
    me.history.push_back(raw.to_string());

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
        Some("junk")      => do_junk(rest, me, world, chars).await,
        Some("donate")    => do_donate(rest, me, world, chars).await,
        Some("sacrifice") | Some("sac") => do_sacrifice(rest, me, world, chars).await,
        Some("put")       => do_put(rest, me, world, chars).await,
        Some("say")       => do_say_with_triggers(rest, me, chars, world).await,
        Some("tell")      => do_tell(rest, me, chars, players).await,
        Some("who")       => do_who(rest, me, chars).await,
        Some("score")     => do_score(me, world).await,
        Some("exp")       => do_exp(me),
        Some("levels")    => do_levels(rest, me),
        Some("areas")     => do_areas(me, world).await,
        Some("kill") | Some("hit") => do_kill(rest, me, world, chars).await,
        Some("kick")      => do_skill(rest, me, world, chars, Skill::Kick).await,
        Some("bash")      => do_skill(rest, me, world, chars, Skill::Bash).await,
        Some("backstab")  => do_skill(rest, me, world, chars, Skill::Backstab).await,
        Some("whirlwind") => do_whirlwind(me, world, chars).await,
        Some("peek")      => do_peek(rest, me, world, chars).await,
        Some("rescue")    => do_rescue(rest, me, world, chars).await,
        Some("disarm")    => do_disarm(rest, me, world, chars).await,
        Some("consider") | Some("con") => do_consider(rest, me, world).await,
        Some("sleep")     => do_position(me, chars, crate::character::Position::Sleeping).await,
        Some("rest")      => do_position(me, chars, crate::character::Position::Resting).await,
        Some("sit")       => do_position(me, chars, crate::character::Position::Sitting).await,
        Some("stand")     => do_position(me, chars, crate::character::Position::Standing).await,
        Some("wake")      => do_wake(me, chars).await,
        Some("bandage")   => do_bandage(me),
        Some("wimpy")     => do_wimpy(rest, me),
        Some("info") | Some("newbie") => do_info(rest, me, chars).await,
        Some("shout")     => do_shout(rest, me, world, chars).await,
        Some("color")     => do_color(rest, me),
        Some("autoexit") | Some("autoexits") => do_toggle_auto(me, AutoFlag::Exit),
        Some("autoloot")   => do_toggle_auto(me, AutoFlag::Loot),
        Some("autoassist") => do_toggle_auto(me, AutoFlag::Assist),
        Some("autotitle")  => do_toggle_auto(me, AutoFlag::Title),
        Some("autogold")   => do_toggle_auto(me, AutoFlag::Gold),
        Some("autosplit")  => do_toggle_auto(me, AutoFlag::Split),
        Some("autosac")    => do_toggle_auto(me, AutoFlag::Sac),
        Some("autodoor")   => do_toggle_auto(me, AutoFlag::Door),
        Some("autokey")    => do_toggle_auto(me, AutoFlag::Key),
        Some("automap")    => do_toggle_auto(me, AutoFlag::Map),
        Some("history")    => do_history(me),
        Some("clan")       => do_clan(rest, me, chars).await,
        Some("clans")      => do_clans(me, chars).await,
        Some("ctell")      => do_ctell(rest, me, chars).await,
        Some("map")        => do_map(me, world).await,
        Some("sneak")     => do_sneak(me),
        Some("hide")      => do_hide(me),
        Some("steal")     => do_steal(rest, me, world, chars).await,
        Some("cast")      => do_cast(rest, me, world, chars).await,
        Some("skills")    => do_skills(me),
        Some("practice") | Some("prac") => do_practice(rest, me),
        Some("affects")   => do_affects(me),
        Some("quest")     => do_quest(rest, me, world, chars).await,
        Some("where")     => do_where(me, world, chars).await,
        Some("give")      => do_give(rest, me, world, chars).await,
        Some("examine")   => do_examine(rest, me, world, chars).await,
        Some("list")      => do_list(me, world).await,
        Some("buy")       => do_buy(rest, me, world, chars).await,
        Some("sell")      => do_sell(rest, me, world, chars).await,
        Some("appraise") | Some("value") => do_appraise(rest, me, world).await,
        Some("flee")      => do_flee(rest, me, world, chars).await,
        Some("wield")     => do_wield(rest, me, world, chars).await,
        Some("wear")      => do_wear(rest, me, world, chars).await,
        Some("remove")    => {
            // Board context: a numeric arg with a board in the room
            // removes a board message (mirrors stock); otherwise unequip.
            if rest.trim().parse::<usize>().is_ok()
                && find_board_in_room(me, world).await.is_some() {
                do_board_remove(rest, me, world).await
            } else {
                do_remove(rest, me, world, chars).await
            }
        }
        Some("board") | Some("boards") => do_board_show(me, world).await,
        Some("write")     => do_board_write(rest, me, world).await,
        Some("read")      => do_board_read(rest, me, world).await,
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
        Some("drink") | Some("sip")    => do_drink_container(rest, me, world, chars).await,
        Some("eat") | Some("taste")    => do_eat(rest, me, world, chars).await,
        Some("fill") | Some("pour")    => do_fill(rest, me, world, chars).await,
        Some("empty")     => do_empty(rest, me, world, chars).await,
        Some("recite")    => do_recite(rest, me, world, chars).await,
        Some("use")       => do_use(rest, me, world, chars).await,
        Some("zap")       => do_zap(rest, me, world, chars).await,
        Some("light")     => do_light(rest, me, world, chars, true).await,
        Some("extinguish")=> do_light(rest, me, world, chars, false).await,
        Some("follow")    => do_follow(rest, me, chars).await,
        Some("group")     => do_group(rest, me, chars).await,
        Some("gtell")     => do_gtell(rest, me, chars).await,
        Some("split")     => do_split(rest, me, chars).await,
        Some("report")    => do_report(me, chars).await,
        Some("title")     => do_title(rest, me),
        Some("gossip") | Some("chat") => do_gossip(rest, me, world, chars).await,
        Some("grats") | Some("gratz") => do_grats(rest, me, world, chars).await,
        Some("holler") => do_holler(rest, me, chars).await,
        Some("version") => do_version(),
        Some("visible") => do_visible(me),
        Some("notell")  => { me.notell = !me.notell; CmdOutput::text(format!("\r\nYou will {} receive tells.\r\n", if me.notell { "no longer" } else { "now" })) }
        Some("nosummon") => { me.nosummon = !me.nosummon; CmdOutput::text(format!("\r\nYou are {} protected from summon.\r\n", if me.nosummon { "now" } else { "no longer" })) }
        Some("nograts")  => { me.grats_off = !me.grats_off; CmdOutput::text(format!("\r\nGrats channel: {}.\r\n", if me.grats_off { "off" } else { "on" })) }
        Some("nogossip") => { me.gossip_off = !me.gossip_off; CmdOutput::text(format!("\r\nGossip channel: {}.\r\n", if me.gossip_off { "off" } else { "on" })) }
        Some("noauction") => { me.auction_off = !me.auction_off; CmdOutput::text(format!("\r\nAuction channel: {}.\r\n", if me.auction_off { "off" } else { "on" })) }
        Some("auction") | Some("auc") => do_auction(rest, me, world, chars).await,
        Some("whisper")   => do_whisper(rest, me, chars).await,
        Some("ask")       => do_spec_comm(rest, me, chars, true).await,
        Some("unfollow")  => do_follow("stop", me, chars).await,
        Some("cls")       => do_cls(),
        Some("norepeat")  => do_norepeat(me),
        Some("hindex")    => do_hindex(rest, me, world).await,
        Some("display")   => do_display(rest, me),
        Some("qsay")      => if rest.trim().is_empty() { CmdOutput::text("\r\nQuest-say what?\r\n".to_string()) }
                             else { self_echo(me, format!("\r\nYou quest-say, '{}'\r\n", rest.trim())) },
        Some("enter")     => CmdOutput::text("\r\nThere is no portal here to enter.\r\n".to_string()),
        Some("leave")     => CmdOutput::text("\r\nYou see no exit to leave through here.\r\n".to_string()),
        Some("page")      => CmdOutput::text("\r\nOutput is not paged on this MUD; everything is sent at once.\r\n".to_string()),
        Some("happyhour") => CmdOutput::text("\r\nThere is no happy hour at the moment.\r\n".to_string()),
        Some("receive")   => CmdOutput::text("\r\nSorry, but you cannot do that here!\r\n".to_string()),
        Some("brief")     => do_brief(me),
        Some("toggle")    => do_toggle(rest, me),
        Some("compact")   => do_compact(me),
        Some("time")      => do_time(),
        Some("weather")   => do_weather(),
        Some("bank")      => do_bank(rest, me),
        Some("offer")     => do_offer(me, world).await,
        Some("rent")      => do_rent(me, world).await,
        Some("balance")   => do_bank("balance", me),
        Some("deposit")   => do_bank(&format!("deposit {rest}"), me),
        Some("withdraw")  => do_bank(&format!("withdraw {rest}"), me),
        Some("gsay")      => do_gtell(rest, me, chars).await,
        Some("take")      => do_get(rest, me, world, chars).await,
        Some("hold") | Some("grab") => do_wear(rest, me, world, chars).await,
        Some("diagnose")  => do_diagnose(rest, me, world, chars).await,
        Some("whoami")    => CmdOutput::text(format!("\r\nYou are {}{}.\r\n",
                                me.name,
                                if me.title.is_empty() { String::new() } else { format!(" {}", me.title) })),
        Some("news")      => do_text_file("news").await,
        Some("credits")   => do_text_file("credits").await,
        Some("motd")      => do_text_file("motd").await,
        Some("imotd")     => do_text_file("imotd").await,
        Some("policy") | Some("policies") => do_text_file("policies").await,
        Some("handbook")  => do_text_file("handbook").await,
        Some("background") => do_text_file("background").await,
        Some("wizlist")   => do_text_file("wizlist").await,
        Some("immlist")   => do_text_file("immlist").await,
        Some("reply")     => do_reply(rest, me, chars, players).await,
        Some("prompt")    => do_prompt(rest, me),
        Some("alias")     => do_alias(rest, me),
        Some("commands")  => do_commands(),
        Some("scan")      => do_scan(rest, me, world, chars).await,
        Some("track")     => do_track(rest, me, world, chars).await,
        Some("mail")      => do_mail(rest, me, chars, players).await,
        Some("spells")    => do_spells(me),
        Some("recall")    => do_cast("'word of recall'", me, world, chars).await,
        Some("emote")     => do_emote(rest, me, chars).await,
        Some("socials")   => do_socials_list(world).await,
        Some("note")      => do_note(rest, me),
        Some("notes")     => do_notes(me),
        Some("pose")      => do_pose(rest, me),
        Some("uptime")    => do_uptime(),
        Some("bug")       => do_submit("bug", rest, me).await,
        Some("idea")      => do_submit("idea", rest, me).await,
        Some("typo")      => do_submit("typo", rest, me).await,
        Some("peace")     => do_peace(me, world, chars).await,
        Some("order")     => do_order(rest, me, world, chars).await,
        Some("pvp")       => do_pvp(me),
        Some("finger") | Some("whois") => do_finger(rest, chars, players).await,
        Some("assist")    => do_assist(rest, me, world, chars).await,
        Some("worship")   => do_worship(rest, me),
        Some("afk")       => do_afk(rest, me),
        Some("goto")      => do_goto(rest, me, world, chars).await,
        Some("transfer")  => do_transfer(rest, me, world, chars).await,
        Some("purge")     => do_purge(me, world, chars).await,
        Some("shutdown")  => do_shutdown(me, chars).await,
        Some("stat")      => do_stat(rest, me, world, chars, players).await,
        Some("force")     => do_force(rest, me, world, chars).await,
        Some("at")        => do_at(rest, me, world, chars, players).await,
        Some("househere") => do_househere(me, world).await,
        Some("house")     => do_house(rest, me, world, players).await,
        Some("set")       => do_set(rest, me, chars).await,
        Some("oset")      => do_oset(rest, me, world).await,
        Some("dig")       => do_dig(rest, me, world).await,
        Some("redit")     => if me.level >= LVL_IMMORT {
                                 crate::olc::start_redit(rest, me, world, players).await
                             } else { immort_huh() },
        Some("oedit")     => if me.level >= LVL_IMMORT {
                                 crate::olc::start_oedit(rest, me, world, players).await
                             } else { immort_huh() },
        Some("medit")     => if me.level >= LVL_IMMORT {
                                 crate::olc::start_medit(rest, me, world, players).await
                             } else { immort_huh() },
        Some("zedit")     => if me.level >= LVL_IMMORT {
                                 crate::olc::start_zedit(rest, me, world, players).await
                             } else { immort_huh() },
        Some("qedit")     => if me.level >= LVL_IMMORT { crate::olc::start_qedit(rest, me, world, players).await } else { immort_huh() },
        Some("trigedit")  => if me.level >= LVL_IMMORT { crate::olc::start_trigedit(rest, me, world, players).await } else { immort_huh() },
        Some("sedit")     => if me.level >= LVL_IMMORT { crate::olc::start_sedit(rest, me, world, players).await } else { immort_huh() },
        Some("aedit")     => if me.level >= LVL_IMMORT { crate::olc::start_aedit(rest, me, world, players).await } else { immort_huh() },
        Some("hedit")     => if me.level >= LVL_IMMORT { crate::olc::start_hedit(rest, me, world, players).await } else { immort_huh() },
        Some("wizlock")   => do_wizlock(rest, me),
        Some("zreset")    => do_zreset(rest, me, world).await,
        Some("olist")     => do_olist(rest, me, world).await,
        Some("mlist")     => do_mlist(rest, me, world).await,
        Some("rlist")     => do_rlist(rest, me, world).await,
        Some("zlist")     => do_zlist(me, world).await,
        Some("invis")     => do_invis(rest, me),
        Some("vis")       => do_vis(me),
        Some("nohassle")  => do_nohassle(me),
        Some("mute")      => do_mute(rest, me, chars).await,
        Some("freeze")    => do_freeze(rest, me, chars).await,
        Some("ban")       => do_ban(rest, me, players).await,
        Some("unban")     => do_unban(rest, me, players).await,
        Some("bans")      => do_bans(me).await,
        Some("wiznet")    => do_wiznet(rest, me, chars).await,
        Some("load")      => do_load(rest, me, world, chars).await,
        Some("restore")   => do_restore(rest, me, chars).await,
        Some("echo")      => do_echo(rest, me, chars).await,
        Some("gecho")     => do_gecho(rest, me, chars).await,
        Some("slay")      => do_slay(rest, me, world, chars).await,
        Some("snoop")     => do_snoop(rest, me, chars).await,
        Some("unsnoop")   => do_unsnoop(me, chars).await,
        Some("status")    => do_status(me, world, chars).await,
        Some("reload")    => do_reload(rest, me, chars, players).await,
        Some("spec_assign") | Some("specassign") => do_spec_assign(rest, me, world).await,
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
    // look <direction>: peek into the adjacent room.
    if let Some(dir) = Direction::parse(arg) {
        let (target, closed, hidden_only) = {
            let w = world.lock().await;
            match w.rooms.get(&me.current_room)
                .and_then(|r| r.exits[dir as usize].as_ref())
            {
                Some(e) if e.to_room != crate::world::NOWHERE
                    && w.rooms.contains_key(&e.to_room) =>
                {
                    let closed = (e.exit_info & crate::world::EX_CLOSED) != 0;
                    let hidden = (e.exit_info & crate::world::EX_HIDDEN) != 0;
                    (Some(e.to_room), closed, hidden && me.level < LVL_IMMORT)
                }
                _ => (None, false, false),
            }
        };
        let Some(t) = target else {
            return CmdOutput::text("\r\nYou see nothing in that direction.\r\n".to_string());
        };
        if hidden_only {
            return CmdOutput::text("\r\nYou see nothing in that direction.\r\n".to_string());
        }
        if closed {
            return CmdOutput::text("\r\nThat way is closed.\r\n".to_string());
        }
        return CmdOutput::text(render_room(t, Some(me.id), world, chars).await);
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
                        let mut body = if mp.description.is_empty() {
                            format!("You see nothing special about {}.", mp.short_descr)
                        } else {
                            mp.description.clone()
                        };
                        // Equipment block.
                        let mut wore_any = false;
                        for (slot, iid_opt) in m.equipment.iter().enumerate() {
                            let Some(iid) = iid_opt else { continue; };
                            let proto = w.obj_instances.iter().find(|o| o.id == *iid)
                                .and_then(|o| w.obj_protos.get(&o.vnum));
                            let Some(p) = proto else { continue; };
                            if !wore_any {
                                body.push_str(&format!("\r\n\r\n{} is using:", mp.short_descr));
                                wore_any = true;
                            }
                            let pos = crate::character::wear_pos_name(slot);
                            body.push_str(&format!("\r\n  <{pos:>10}> {}", p.short_description));
                        }
                        return CmdOutput::text(format!("\r\n{body}\r\n"));
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
        let title = other.character.lock().await.title.clone();
        let header = if title.is_empty() {
            format!("You see {}, a player.", other.name)
        } else {
            format!("You see {} {}.", other.name, title)
        };
        return CmdOutput::text(format!("\r\n{header}\r\n"));
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
    if !house_can_touch(me, world).await {
        return CmdOutput::text(
            "\r\nA ward of ownership stays your hand.\r\n".to_string()
        );
    }

    // `get all` / `get all.<key>` — mass pickup from the room floor.
    if arg.eq_ignore_ascii_case("all") || arg.to_ascii_lowercase().starts_with("all.") {
        let kw = arg.split_once('.').map(|(_, k)| k.to_ascii_lowercase());
        return do_get_all(me, world, chars, kw).await;
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

    // Coin pile: collect the gold instead of carrying the object (cp223).
    let coins = w.obj_instances.iter().find(|o| o.id == iid)
        .filter(|o| o.vnum == crate::db::GOLD_PILE_VNUM)
        .map(|o| o.gold_amount);
    if let Some(amount) = coins {
        me.gold += amount;
        w.extract_obj(iid);
        drop(w);
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} picks up {}.\r\n", me.name, name));
        drop(cl);
        return CmdOutput::text(format!(
            "\r\nYou pick up {name}.\r\nYou now have {} gold.\r\n", me.gold
        ));
    }

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

/// True if the container instance is currently closed (cp215).  Reads the
/// CONT_CLOSED bit from the prototype's `value[1]`.
fn container_closed(w: &World, iid: u32) -> bool {
    w.obj_instances.iter().find(|o| o.id == iid)
        .and_then(|o| w.obj_protos.get(&o.vnum))
        .map(|p| p.value[1] & crate::world::CONT_CLOSED != 0)
        .unwrap_or(false)
}

async fn do_get_from_container(
    obj_kw: &str,
    cont_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    // `get all <container>` / `get all.<kw> <container>` — mass pickup
    // from the container.  Routed before the single-item logic.
    if obj_kw.eq_ignore_ascii_case("all") || obj_kw.to_ascii_lowercase().starts_with("all.") {
        let kw = obj_kw.split_once('.').map(|(_, k)| k.to_ascii_lowercase());
        return do_get_all_from(me, world, chars, kw, cont_kw).await;
    }
    let key = obj_kw.to_ascii_lowercase();
    let mut w = world.lock().await;

    let (container_iid, container_name) = match find_container(&w, me, cont_kw) {
        Some(t) => t,
        None => return CmdOutput::text(format!("\r\nYou see no {cont_kw} here.\r\n")),
    };
    if container_closed(&w, container_iid) {
        return CmdOutput::text(format!("\r\n{container_name} is closed.\r\n"));
    }

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
    // `put all <container>` / `put all.<kw> <container>`
    if obj_kw.eq_ignore_ascii_case("all") || obj_kw.to_ascii_lowercase().starts_with("all.") {
        let kw = obj_kw.split_once('.').map(|(_, k)| k.to_ascii_lowercase());
        return do_put_all(me, world, chars, kw, cont_kw).await;
    }

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
    if container_closed(&w, container_iid) {
        return CmdOutput::text(format!("\r\n{container_name} is closed.\r\n"));
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
    if !house_can_touch(me, world).await {
        return CmdOutput::text(
            "\r\nThe house's ward refuses to accept your offering.\r\n".to_string()
        );
    }
    if arg.eq_ignore_ascii_case("all") || arg.to_ascii_lowercase().starts_with("all.") {
        let kw = arg.split_once('.').map(|(_, k)| k.to_ascii_lowercase());
        return do_drop_all(me, world, chars, kw).await;
    }
    // `drop <N> coins|gold|money` — drop a pile of coins on the floor (cp223).
    {
        let toks: Vec<&str> = arg.split_whitespace().collect();
        if toks.len() == 2 {
            if let Ok(n) = toks[0].parse::<i64>() {
                let what = toks[1].to_ascii_lowercase();
                if what == "coins" || what == "coin" || what == "gold" || what == "money" {
                    return do_drop_gold(n, me, world, chars).await;
                }
            }
        }
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

/// Returns true if `me` is allowed to drop/pick items in the current
/// room.  Non-house rooms always allow.  ROOM_HOUSE rooms only allow
/// the owner (case-insensitive name match) and immortals.
async fn house_can_touch(me: &Character, world: &Arc<Mutex<World>>) -> bool {
    if me.level >= LVL_IMMORT { return true; }
    let w = world.lock().await;
    let Some(r) = w.rooms.get(&me.current_room) else { return true; };
    if r.room_flags[0] & crate::world::ROOM_HOUSE == 0 { return true; }
    match w.house_owners.get(&me.current_room) {
        Some(o) => o.eq_ignore_ascii_case(&me.name),
        None    => true, // unowned house — anyone can use
    }
}

/// `get all` / `get all.<kw>` — pick up every matching object on the
/// floor.  Respects carry-weight cap; stops early when full.
async fn do_get_all(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    keyword: Option<String>,
) -> CmdOutput {
    let mut w = world.lock().await;
    let cap = crate::character::str_carry_cap(me.str_);
    let candidates: Vec<u32> = w.rooms.get(&me.current_room)
        .map(|r| r.objects.clone()).unwrap_or_default();
    if candidates.is_empty() {
        return CmdOutput::text("\r\nThere's nothing on the ground.\r\n".to_string());
    }
    let mut taken: Vec<String> = Vec::new();
    let mut stopped_for_weight = false;
    for iid in candidates {
        // Match keyword filter if any.
        let (matches, short, weight) = {
            let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let m = match &keyword {
                Some(k) => obj_matches_keyword(&w, obj, k),
                None    => true,
            };
            let v = obj_view(&w, obj);
            let wt = w.obj_protos.get(&obj.vnum).map(|p| p.weight).unwrap_or(0);
            (m, v.short, wt)
        };
        if !matches { continue; }
        // Carry-cap check.
        if total_carry_weight(me, &w) + weight > cap {
            stopped_for_weight = true;
            break;
        }
        // Move to inventory.
        if let Some(r) = w.rooms.get_mut(&me.current_room) {
            r.objects.retain(|&i| i != iid);
        }
        if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
            o.in_room = crate::world::NOWHERE;
        }
        me.inventory.push(iid);
        taken.push(short);
    }
    drop(w);
    if taken.is_empty() {
        return CmdOutput::text("\r\nThere's nothing matching to pick up.\r\n".to_string());
    }
    let mut s = format!("\r\nYou pick up {} item(s):\r\n", taken.len());
    for n in &taken {
        s.push_str(&format!("  {n}\r\n"));
    }
    if stopped_for_weight {
        s.push_str("You couldn't carry any more.\r\n");
    }
    chars.lock().await.broadcast_room(me.current_room, Some(me.id),
        &format!("{} picks up several items.\r\n", me.name));
    CmdOutput::text(s)
}

/// `drop all` / `drop all.<kw>` — drop every matching inventory item.
/// `get all <container>` / `get all.<kw> <container>` — pull every
/// matching child out of a container.  Respects carry-weight cap.
async fn do_get_all_from(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    keyword: Option<String>,
    cont_kw: &str,
) -> CmdOutput {
    let mut w = world.lock().await;
    let (container_iid, container_name) = match find_container(&w, me, cont_kw) {
        Some(t) => t,
        None => return CmdOutput::text(format!("\r\nYou see no {cont_kw} here.\r\n")),
    };
    if container_closed(&w, container_iid) {
        return CmdOutput::text(format!("\r\n{container_name} is closed.\r\n"));
    }
    let candidates: Vec<u32> = w.obj_instances.iter()
        .find(|o| o.id == container_iid)
        .map(|o| o.contents.clone()).unwrap_or_default();
    if candidates.is_empty() {
        return CmdOutput::text(format!("\r\n{container_name} is empty.\r\n"));
    }
    let cap = crate::character::str_carry_cap(me.str_);
    let mut taken: Vec<String> = Vec::new();
    let mut stopped = false;
    for iid in candidates {
        let (matches, short, weight) = {
            let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let m = match &keyword {
                Some(k) => obj_matches_keyword(&w, obj, k),
                None    => true,
            };
            let v = obj_view(&w, obj);
            let wt = w.obj_protos.get(&obj.vnum).map(|p| p.weight).unwrap_or(0);
            (m, v.short, wt)
        };
        if !matches { continue; }
        if total_carry_weight(me, &w) + weight > cap {
            stopped = true;
            break;
        }
        if let Some(c) = w.obj_instances.iter_mut().find(|o| o.id == container_iid) {
            c.contents.retain(|&i| i != iid);
        }
        me.inventory.push(iid);
        taken.push(short);
    }
    drop(w);
    if taken.is_empty() {
        return CmdOutput::text(format!("\r\nNothing matching in {container_name}.\r\n"));
    }
    let mut s = format!("\r\nYou take {} item(s) from {container_name}:\r\n", taken.len());
    for n in &taken { s.push_str(&format!("  {n}\r\n")); }
    if stopped { s.push_str("You couldn't carry any more.\r\n"); }
    chars.lock().await.broadcast_room(me.current_room, Some(me.id),
        &format!("{} takes several items from {container_name}.\r\n", me.name));
    CmdOutput::text(s)
}

async fn do_drop_all(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    keyword: Option<String>,
) -> CmdOutput {
    let mut w = world.lock().await;
    let candidates = me.inventory.clone();
    if candidates.is_empty() {
        return CmdOutput::text("\r\nYou have nothing to drop.\r\n".to_string());
    }
    let mut dropped: Vec<String> = Vec::new();
    for iid in candidates {
        let (matches, short) = {
            let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let m = match &keyword {
                Some(k) => obj_matches_keyword(&w, obj, k),
                None    => true,
            };
            (m, obj_view(&w, obj).short)
        };
        if !matches { continue; }
        me.inventory.retain(|&i| i != iid);
        if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
            o.in_room = me.current_room;
        }
        if let Some(r) = w.rooms.get_mut(&me.current_room) {
            r.objects.push(iid);
        }
        dropped.push(short);
    }
    drop(w);
    if dropped.is_empty() {
        return CmdOutput::text("\r\nNothing matching to drop.\r\n".to_string());
    }
    let mut s = format!("\r\nYou drop {} item(s):\r\n", dropped.len());
    for n in &dropped {
        s.push_str(&format!("  {n}\r\n"));
    }
    chars.lock().await.broadcast_room(me.current_room, Some(me.id),
        &format!("{} drops several items.\r\n", me.name));
    CmdOutput::text(s)
}

/// `drop <N> coins` — drop a pile of gold on the floor as a synthetic
/// GOLD_PILE object (cp223), collectible by anyone via `get coins`.
async fn do_drop_gold(
    n: i64,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if n <= 0 {
        return CmdOutput::text("\r\nDrop how many coins?\r\n".to_string());
    }
    if n > me.gold {
        return CmdOutput::text("\r\nYou don't have that much gold.\r\n".to_string());
    }
    me.gold -= n;
    let room = me.current_room;
    {
        let mut w = world.lock().await;
        if let Some(iid) = w.spawn_obj(crate::db::GOLD_PILE_VNUM) {
            if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
                o.gold_amount = n;
                o.in_room = room;
            }
            if let Some(r) = w.rooms.get_mut(&room) {
                r.objects.push(iid);
            }
        }
    }
    chars.lock().await.broadcast_room(room, Some(me.id),
        &format!("{} drops some gold coins.\r\n", me.name));
    CmdOutput::text(format!("\r\nYou drop {n} gold coins.\r\n"))
}

/// `junk <item>` — permanently destroy an inventory item.  Supports
/// `junk all` / `junk all.<kw>` for bulk cleanup.  (cp204)
async fn do_junk(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nJunk what?\r\n".to_string());
    }
    let key = arg.to_ascii_lowercase();
    let all = key == "all" || key.starts_with("all.");
    let kw_filter: Option<String> = if all {
        key.split_once('.').map(|(_, k)| k.to_string())
    } else {
        Some(key.clone())
    };

    let mut w = world.lock().await;
    let candidates = me.inventory.clone();
    let mut junked: Vec<String> = Vec::new();
    for iid in candidates {
        let (matches, short) = {
            let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let m = match &kw_filter {
                Some(k) => obj_matches_keyword(&w, obj, k),
                None    => true,
            };
            (m, obj_view(&w, obj).short)
        };
        if !matches { continue; }
        me.inventory.retain(|&i| i != iid);
        w.extract_obj(iid);
        junked.push(short);
        if !all { break; }   // single-item form stops at the first match
    }
    drop(w);

    if junked.is_empty() {
        return CmdOutput::text(format!("\r\nYou have nothing like '{arg}' to junk.\r\n"));
    }
    {
        let cl = chars.lock().await;
        let bmsg = if junked.len() == 1 {
            format!("{} destroys {}.\r\n", me.name, junked[0])
        } else {
            format!("{} destroys several items.\r\n", me.name)
        };
        cl.broadcast_room(me.current_room, Some(me.id), &bmsg);
    }
    if junked.len() == 1 {
        CmdOutput::text(format!("\r\nYou destroy {}. It is gone forever.\r\n", junked[0]))
    } else {
        let mut s = format!("\r\nYou destroy {} item(s):\r\n", junked.len());
        for n in &junked { s.push_str(&format!("  {n}\r\n")); }
        CmdOutput::text(s)
    }
}

/// `sacrifice <item>` (cp224): offer a corpse or item to your deity and
/// receive a small token of gold in return.  Searches the room floor
/// first (where corpses lie), then inventory.  Distinct from `junk`
/// (which destroys for nothing) — sacrifice grants a deity-flavored
/// reward and ties into `worship` (cp99).
async fn do_sacrifice(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let key = arg.trim().to_ascii_lowercase();
    if key.is_empty() {
        return CmdOutput::text("\r\nSacrifice what?\r\n".to_string());
    }
    // Resolve the target: room floor first, then inventory.  Capture its
    // short + proto cost (for the reward), and whether it was on the floor.
    let (iid, short, cost, on_floor) = {
        let w = world.lock().await;
        let mut hit: Option<(u32, String, i64, bool)> = None;
        // Room floor.
        if let Some(r) = w.rooms.get(&me.current_room) {
            for &iid in &r.objects {
                if let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) {
                    if obj_matches_keyword(&w, obj, &key) {
                        let cost = w.obj_protos.get(&obj.vnum).map(|p| p.cost as i64).unwrap_or(0);
                        hit = Some((iid, obj_view(&w, obj).short, cost, true));
                        break;
                    }
                }
            }
        }
        // Inventory fallback.
        if hit.is_none() {
            for &iid in &me.inventory {
                if let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) {
                    if obj_matches_keyword(&w, obj, &key) {
                        let cost = w.obj_protos.get(&obj.vnum).map(|p| p.cost as i64).unwrap_or(0);
                        hit = Some((iid, obj_view(&w, obj).short, cost, false));
                        break;
                    }
                }
            }
        }
        match hit {
            Some(h) => h,
            None => return CmdOutput::text(format!("\r\nYou see no {key} to sacrifice here.\r\n")),
        }
    };
    // Reward: a token fraction of the item's worth, minimum 1.
    let reward = (cost / 100).max(1);
    if !on_floor {
        me.inventory.retain(|&i| i != iid);
    }
    {
        let mut w = world.lock().await;
        w.extract_obj(iid);
    }
    me.gold += reward;
    let deity = if me.god.is_empty() { "the gods".to_string() } else { me.god.clone() };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} sacrifices {short} to {deity}.\r\n", me.name));
    }
    CmdOutput::text(format!(
        "\r\nYou offer {short} to {deity}, and receive {reward} gold for your devotion.\r\n"
    ))
}

/// `donate <item>` — send an inventory item to the donation room (the
/// mortal start room's floor) where any player can pick it up.  (cp204)
async fn do_donate(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if arg.is_empty() {
        return CmdOutput::text("\r\nDonate what?\r\n".to_string());
    }
    let key = arg.to_ascii_lowercase();
    let mut w = world.lock().await;
    let donation_room = w.start_room(false);
    if donation_room == me.current_room {
        return CmdOutput::text(
            "\r\nYou're already standing in the donation room — just drop it.\r\n".to_string()
        );
    }
    // Find first matching inventory item.
    let (iid, short) = {
        let mut found = None;
        for &iid in &me.inventory {
            if let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) {
                if obj_matches_keyword(&w, obj, &key) {
                    found = Some((iid, obj_view(&w, obj).short));
                    break;
                }
            }
        }
        match found {
            Some(f) => f,
            None => return CmdOutput::text(format!("\r\nYou do not have a {key}.\r\n")),
        }
    };
    me.inventory.retain(|&i| i != iid);
    if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
        o.in_room = donation_room;
    }
    if let Some(r) = w.rooms.get_mut(&donation_room) {
        r.objects.push(iid);
    }
    drop(w);

    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} donates {} to the gods.\r\n", me.name, short));
        cl.broadcast_room(donation_room, None,
            &format!("{} suddenly appears, sent by a generous soul.\r\n", short));
    }
    CmdOutput::text(format!("\r\nYou donate {} to the needy.\r\n", short))
}

/// `put all <container>` / `put all.<kw> <container>` — stuff every
/// matching inventory item into the named container.  Refuses if the
/// container can't be found; silently skips items the container can't
/// hold (over-cap check is intentionally lax — matches `do_put`).
async fn do_put_all(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    keyword: Option<String>,
    cont_kw: &str,
) -> CmdOutput {
    let mut w = world.lock().await;
    let (container_iid, container_name) = match find_container(&w, me, cont_kw) {
        Some(t) => t,
        None    => return CmdOutput::text(format!("\r\nYou see no {cont_kw} here.\r\n")),
    };
    if container_closed(&w, container_iid) {
        return CmdOutput::text(format!("\r\n{container_name} is closed.\r\n"));
    }
    let candidates = me.inventory.clone();
    let mut moved: Vec<String> = Vec::new();
    for iid in candidates {
        if iid == container_iid { continue; }
        let (matches, short) = {
            let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let m = match &keyword {
                Some(k) => obj_matches_keyword(&w, obj, k),
                None    => true,
            };
            (m, obj_view(&w, obj).short)
        };
        if !matches { continue; }
        me.inventory.retain(|&i| i != iid);
        if let Some(c) = w.obj_instances.iter_mut().find(|o| o.id == container_iid) {
            c.contents.push(iid);
        }
        moved.push(short);
    }
    drop(w);
    if moved.is_empty() {
        return CmdOutput::text(format!("\r\nNothing matching to put in {container_name}.\r\n"));
    }
    let mut s = format!("\r\nYou put {} item(s) in {container_name}:\r\n", moved.len());
    for n in &moved { s.push_str(&format!("  {n}\r\n")); }
    chars.lock().await.broadcast_room(me.current_room, Some(me.id),
        &format!("{} puts several items in {container_name}.\r\n", me.name));
    CmdOutput::text(s)
}

/// `give all <player>` / `give all.<kw> <player>` — hand every
/// matching inventory item to the named player in the room.
async fn do_give_all(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    keyword: Option<String>,
    target_kw: &str,
) -> CmdOutput {
    // Resolve target as an online player in the same room.
    let target = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p|
            p.id != me.id
            && p.current_room == me.current_room
            && p.name.eq_ignore_ascii_case(target_kw)).cloned();
        h
    };
    let Some(ph) = target else {
        return CmdOutput::text(format!(
            "\r\nNo one named '{target_kw}' is here.\r\n"
        ));
    };
    let mut w = world.lock().await;
    let candidates = me.inventory.clone();
    let mut handed: Vec<String> = Vec::new();
    for iid in candidates {
        let (matches, short) = {
            let Some(obj) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let m = match &keyword {
                Some(k) => obj_matches_keyword(&w, obj, k),
                None    => true,
            };
            (m, obj_view(&w, obj).short)
        };
        if !matches { continue; }
        me.inventory.retain(|&i| i != iid);
        ph.character.lock().await.inventory.push(iid);
        handed.push(short);
    }
    drop(w);
    if handed.is_empty() {
        return CmdOutput::text("\r\nNothing matching to give.\r\n".to_string());
    }
    let _ = ph.send.send(format!(
        "\r\n{} gives you {} item(s).\r\n", me.name, handed.len(),
    ));
    let mut s = format!("\r\nYou give {} item(s) to {}:\r\n", handed.len(), ph.name);
    for n in &handed { s.push_str(&format!("  {n}\r\n")); }
    chars.lock().await.broadcast_room(me.current_room, Some(me.id),
        &format!("{} hands several items to {}.\r\n", me.name, ph.name));
    CmdOutput::text(s)
}

async fn do_say(
    arg: &str,
    me: &mut Character,
    chars: &SharedChars,
) -> CmdOutput {
    if me.muted { return muted_msg(); }
    if arg.is_empty() {
        return CmdOutput::text("\r\nYak yak yak...\r\n");
    }
    me.reveal();
    let spoken = garble_drunk(arg, me.drunk);
    {
        let cl = chars.lock().await;
        cl.broadcast_room(
            me.current_room, Some(me.id),
            &format!("{} says, '{spoken}'\r\n", me.name),
        );
    }
    self_echo(me, format!("\r\nYou say, '{spoken}'\r\n"))
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

async fn do_tell(
    arg: &str,
    me: &Character,
    chars: &SharedChars,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    if me.muted { return muted_msg(); }
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
        // Offline fallback: if the player exists on disk, queue a mail
        // entry.  Otherwise refuse outright.
        let (data_dir, canonical) = {
            let pl = players.lock().await;
            (pl.data_dir().to_string(), pl.find_name(target))
        };
        let Some(canonical) = canonical else {
            return CmdOutput::text(
                "\r\nNo player by that name exists.\r\n".to_string()
            );
        };
        let body = format!("[offline tell] {msg}");
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64).unwrap_or(0);
        let entry = crate::mail::MailMessage {
            from: me.name.clone(), unix_ts: ts, body,
        };
        if let Err(e) = crate::mail::append_mail(&data_dir, &canonical, &entry) {
            return CmdOutput::text(format!(
                "\r\n{} is offline and the mail queue is broken: {e}\r\n", canonical,
            ));
        }
        return CmdOutput::text(format!(
            "\r\n{canonical} is offline — your tell has been queued as mail.\r\n"
        ));
    };
    // Respect the recipient's notell preference (immortals bypass).
    if me.level < LVL_IMMORT {
        let blocked = { p.character.lock().await.notell };
        if blocked {
            return CmdOutput::text(format!(
                "\r\n{} is not receiving tells right now.\r\n", p.name));
        }
    }
    let _ = p.send.send(format!("{} tells you, '{msg}'\r\n", me.name));
    // Record who tell'd them so `reply` works; also grab the AFK msg
    // for an auto-reply (if any).
    let afk_reply = {
        let mut tc = p.character.lock().await;
        tc.last_tell_from = Some(me.name.clone());
        if tc.tell_history.len() >= 20 { tc.tell_history.pop_front(); }
        tc.tell_history.push_back((me.name.clone(), msg.to_string()));
        tc.afk_msg.clone()
    };
    let mut out = format!("\r\nYou tell {}, '{msg}'\r\n", p.name);
    if let Some(reason) = afk_reply {
        out.push_str(&format!("[AFK] {} is away: {reason}\r\n", p.name));
    }
    CmdOutput::text(out)
}

/// `reply <msg>` — send a tell back to the last person who tell'd us.
async fn do_reply(
    arg: &str,
    me: &Character,
    chars: &SharedChars,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    let msg = arg.trim();
    if msg.is_empty() {
        return CmdOutput::text("\r\nReply with what?\r\n".to_string());
    }
    let Some(name) = me.last_tell_from.clone() else {
        return CmdOutput::text("\r\nYou have no one to reply to.\r\n".to_string());
    };
    let combined = format!("{name} {msg}");
    do_tell(&combined, me, chars, players).await
}

/// `mail` subcommands: `send <to> <text>`, `list`, `read <N>`,
/// `delete <N>`. Mail is stored per-recipient under
/// `<data_dir>/plrmail/<name>.mail`; the recipient sees a "You have
/// new mail." notification immediately if they're online.
async fn do_mail(
    arg: &str,
    me: &Character,
    chars: &SharedChars,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    let data_dir = players.lock().await.data_dir().to_string();
    let parts: Vec<&str> = arg.splitn(3, char::is_whitespace).collect();
    let sub = parts.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();

    match sub.as_str() {
        "" | "list" | "check" => {
            let msgs = crate::mail::load_mailbox(&data_dir, &me.name);
            if msgs.is_empty() {
                return CmdOutput::text("\r\nYour mailbox is empty.\r\n".to_string());
            }
            let mut s = format!("\r\nYou have {} message(s):\r\n", msgs.len());
            for (i, m) in msgs.iter().enumerate() {
                let preview: String = m.body.chars().take(50).collect();
                s.push_str(&format!("  [{}] from {:<14}  {}\r\n", i + 1, m.from, preview));
            }
            s.push_str("Use `mail read <N>` to read a message.\r\n");
            CmdOutput::text(s)
        }
        "read" => {
            let Some(n) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) else {
                return CmdOutput::text("\r\nUsage: mail read <N>\r\n".to_string());
            };
            let msgs = crate::mail::load_mailbox(&data_dir, &me.name);
            let Some(m) = (n.checked_sub(1)).and_then(|i| msgs.get(i)) else {
                return CmdOutput::text(format!("\r\nYou have no message #{n}.\r\n"));
            };
            CmdOutput::text(format!(
                "\r\nFrom: {}\r\nTime: {}\r\n----------------------------------------\r\n{}\r\n",
                m.from, m.unix_ts, m.body,
            ))
        }
        "delete" | "del" => {
            let Some(n) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) else {
                return CmdOutput::text("\r\nUsage: mail delete <N>\r\n".to_string());
            };
            let mut msgs = crate::mail::load_mailbox(&data_dir, &me.name);
            let Some(i) = n.checked_sub(1) else {
                return CmdOutput::text("\r\nBad index.\r\n".to_string());
            };
            if i >= msgs.len() {
                return CmdOutput::text(format!("\r\nYou have no message #{n}.\r\n"));
            }
            msgs.remove(i);
            if let Err(e) = crate::mail::save_mailbox(&data_dir, &me.name, &msgs) {
                return CmdOutput::text(format!("\r\nSave failed: {e}\r\n"));
            }
            CmdOutput::text(format!("\r\nDeleted message #{n}.\r\n"))
        }
        "send" => {
            // mail send <to> <text>
            let Some(to)   = parts.get(1) else {
                return CmdOutput::text("\r\nUsage: mail send <to> <text>\r\n".to_string());
            };
            let Some(text) = parts.get(2) else {
                return CmdOutput::text("\r\nUsage: mail send <to> <text>\r\n".to_string());
            };
            let body = text.trim();
            if body.is_empty() {
                return CmdOutput::text("\r\nNo body — message not sent.\r\n".to_string());
            }
            // Verify the recipient exists in the player index.
            let recipient_name = {
                let db = players.lock().await;
                db.find_name(to)
            };
            let Some(recipient_name) = recipient_name else {
                return CmdOutput::text(format!("\r\nNo player named '{to}' on this MUD.\r\n"));
            };
            let unix_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64).unwrap_or(0);
            let msg = crate::mail::MailMessage {
                from:    me.name.clone(),
                unix_ts,
                body:    body.to_string(),
            };
            if let Err(e) = crate::mail::append_mail(&data_dir, &recipient_name, &msg) {
                return CmdOutput::text(format!("\r\nSend failed: {e}\r\n"));
            }
            // Notify online recipient immediately.
            let cl = chars.lock().await;
            if let Some(p) = cl.iter().find(|p| p.name.eq_ignore_ascii_case(&recipient_name)) {
                let _ = p.send.send(format!("\r\nYou have new mail from {}.\r\n", me.name));
            }
            CmdOutput::text(format!("\r\nMail sent to {recipient_name}.\r\n"))
        }
        _ => CmdOutput::text(
            "\r\nUsage: mail send <to> <text> | list | read <N> | delete <N>\r\n".to_string(),
        ),
    }
}

/// `track <player>` — find the first direction toward an online player
/// via BFS over rooms.  Bounded at depth 50 to keep the pass cheap on
/// the 12700-room dataset.  Closed doors, hidden exits (mortals), and
/// `ROOM_NOTRACK` rooms break the search.
async fn do_track(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nTrack whom?\r\n".to_string());
    }
    let target_room = {
        let cl = chars.lock().await;
        let r = cl.iter().find(|p| p.name.eq_ignore_ascii_case(arg) && p.id != me.id)
            .map(|p| p.current_room);
        r
    };
    let Some(target_room) = target_room else {
        return CmdOutput::text("\r\nNobody by that name is online.\r\n".to_string());
    };
    if target_room == me.current_room {
        return CmdOutput::text(format!("\r\n{arg} is right here.\r\n"));
    }

    let immortal = me.level >= LVL_IMMORT;
    let first_hop = {
        use std::collections::{VecDeque, HashMap};
        let w = world.lock().await;
        // BFS: each visited room remembers its first-hop direction.
        let mut visited: HashMap<crate::world::RoomVnum, Direction> = HashMap::new();
        let mut queue: VecDeque<(crate::world::RoomVnum, i32, Direction)> = VecDeque::new();
        let Some(start) = w.rooms.get(&me.current_room) else {
            return CmdOutput::text("\r\nYou are nowhere.\r\n".to_string());
        };
        for d in Direction::ALL {
            if let Some(e) = &start.exits[d as usize] {
                if e.to_room == crate::world::NOWHERE { continue; }
                if (e.exit_info & crate::world::EX_CLOSED) != 0 { continue; }
                if !immortal && (e.exit_info & crate::world::EX_HIDDEN) != 0 { continue; }
                if w.rooms.get(&e.to_room)
                    .map(|r| r.room_flags[0] & crate::world::ROOM_NOTRACK != 0)
                    .unwrap_or(true) { continue; }
                visited.insert(e.to_room, d);
                queue.push_back((e.to_room, 1, d));
            }
        }
        let mut found: Option<Direction> = None;
        while let Some((rv, depth, first)) = queue.pop_front() {
            if rv == target_room { found = Some(first); break; }
            if depth >= 50 { continue; }
            let Some(r) = w.rooms.get(&rv) else { continue; };
            for d in Direction::ALL {
                if let Some(e) = &r.exits[d as usize] {
                    if e.to_room == crate::world::NOWHERE { continue; }
                    if (e.exit_info & crate::world::EX_CLOSED) != 0 { continue; }
                    if !immortal && (e.exit_info & crate::world::EX_HIDDEN) != 0 { continue; }
                    if w.rooms.get(&e.to_room)
                        .map(|r| r.room_flags[0] & crate::world::ROOM_NOTRACK != 0)
                        .unwrap_or(true) { continue; }
                    if visited.contains_key(&e.to_room) { continue; }
                    visited.insert(e.to_room, first);
                    queue.push_back((e.to_room, depth + 1, first));
                }
            }
        }
        found
    };
    match first_hop {
        Some(d) => CmdOutput::text(format!("\r\nYou sense {arg} is {} from here.\r\n", d.name())),
        None    => CmdOutput::text(format!("\r\nYou cannot sense {arg}.\r\n")),
    }
}

/// `hunt <mob>` (cp222): like `track`, but seeks the nearest room holding
/// a mob whose keywords match — reports the first-hop direction.  Reuses
/// the cp72 BFS shape (closed doors / hidden exits for mortals / NOTRACK
/// rooms all block) but tests each visited room for the quarry instead of
/// a fixed target room.

/// `scan [direction]` — peek into adjacent rooms.  No arg scans every
/// open, non-hidden direction; a direction arg drills into just that
/// one. Closed doors block (caller has to `open` first). Mortals can't
/// see through EX_HIDDEN exits.
async fn do_scan(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    // Parse args: optional direction word, optional distance integer (1-3).
    let mut only_dir: Option<Direction> = None;
    let mut max_hops: i32 = 1;
    for tok in arg.split_whitespace() {
        if let Ok(n) = tok.parse::<i32>() {
            max_hops = n.clamp(1, 3);
            continue;
        }
        if let Some(d) = Direction::parse(tok) {
            only_dir = Some(d);
            continue;
        }
        return CmdOutput::text(format!("\r\nUnknown direction '{tok}'.\r\n"));
    }
    let immortal = me.level >= LVL_IMMORT;

    // Snapshot rooms 1..max_hops in each (filtered) direction.
    let scans: Vec<(Direction, i32, crate::world::RoomVnum, String, Vec<String>)> = {
        let w = world.lock().await;
        let mut out = Vec::new();
        for d in Direction::ALL {
            if let Some(this_d) = only_dir { if this_d != d { continue; } }
            // Walk up to `max_hops` rooms in this direction.
            let mut cur = me.current_room;
            for hop in 1..=max_hops {
                let r = match w.rooms.get(&cur) { Some(r) => r, None => break };
                let Some(e) = &r.exits[d as usize] else { break };
                if e.to_room == crate::world::NOWHERE { break; }
                if (e.exit_info & crate::world::EX_CLOSED) != 0 { break; }
                if !immortal && (e.exit_info & crate::world::EX_HIDDEN) != 0 { break; }
                let Some(target) = w.rooms.get(&e.to_room) else { break };
                let mut mobs: Vec<String> = Vec::new();
                for &mid in &target.mobs {
                    if let Some(m) = w.mob_instances.iter().find(|m| m.id == mid) {
                        if let Some(p) = w.mob_protos.get(&m.vnum) {
                            mobs.push(p.short_descr.clone());
                        }
                    }
                }
                out.push((d, hop, e.to_room, target.name.clone(), mobs));
                cur = e.to_room;
            }
        }
        out
    };

    // Also gather players in those rooms (briefly).
    let cl_snap: Vec<(u32, crate::world::RoomVnum, String)> = {
        let cl = chars.lock().await;
        cl.iter().map(|p| (p.id, p.current_room, p.name.clone())).collect()
    };

    if scans.is_empty() {
        return CmdOutput::text("\r\nNo visible exits to scan.\r\n".to_string());
    }
    let mut s = String::from("\r\n");
    for (d, hop, rv, name, mobs) in &scans {
        let prefix = "  ".repeat(*hop as usize);
        s.push_str(&format!("{prefix}{} (+{hop}) → [{rv}] {name}\r\n", d.name()));
        let players: Vec<&str> = cl_snap.iter()
            .filter(|(id, room, _)| *id != me.id && *room == *rv)
            .map(|(_, _, n)| n.as_str())
            .collect();
        if mobs.is_empty() && players.is_empty() {
            s.push_str(&format!("{prefix}    (quiet)\r\n"));
        } else {
            for n in &players {
                s.push_str(&format!("{prefix}    {n} is here.\r\n"));
            }
            for m in mobs {
                s.push_str(&format!("{prefix}    {m}\r\n"));
            }
        }
    }
    CmdOutput::text(s)
}

/// `notes` lists current notes numbered 1..N.  Empty notepad shows
/// "You have no notes yet."
/// `pose <text>` — set a vanity emote shown in render_room next to your
/// "X is here" line.  `pose` or `pose -` clears.
/// `order <mob> kill <target>` — direct your charmed mob to attack
/// another mob in the same room.  Caller must be the mob's charmer
/// (set by cast_charm_person) and the mob must still hold the
/// CharmPerson affect.
async fn do_order(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let parts: Vec<&str> = arg.splitn(3, char::is_whitespace).collect();
    if parts.len() < 3 || !parts[1].eq_ignore_ascii_case("kill") {
        return CmdOutput::text("\r\nUsage: order <mob> kill <target>\r\n".to_string());
    }
    let mob_kw    = parts[0].to_ascii_lowercase();
    let target_kw = parts[2].to_ascii_lowercase();

    let result = {
        let mut w = world.lock().await;
        // Find the charmed mob in the caller's room.
        let mob_id: Option<u32> = w.rooms.get(&me.current_room).and_then(|r| {
            r.mobs.iter().find_map(|&mid| {
                let m = w.mob_instances.iter().find(|m| m.id == mid)?;
                if m.charmer != Some(me.id) { return None; }
                if !m.affects.iter().any(|a| a.skill == crate::character::Skill::CharmPerson) {
                    return None;
                }
                let p = w.mob_protos.get(&m.vnum)?;
                if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&mob_kw)) {
                    Some(mid)
                } else { None }
            })
        });
        let Some(mob_id) = mob_id else {
            return CmdOutput::text(format!("\r\nYou have no '{mob_kw}' under your command here.\r\n"));
        };
        // Find target mob in the same room (must not be the charmed mob itself).
        let target_id: Option<u32> = w.rooms.get(&me.current_room).and_then(|r| {
            r.mobs.iter().find_map(|&tid| {
                if tid == mob_id { return None; }
                let t = w.mob_instances.iter().find(|m| m.id == tid)?;
                let p = w.mob_protos.get(&t.vnum)?;
                if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&target_kw)) {
                    Some(tid)
                } else { None }
            })
        });
        let Some(target_id) = target_id else {
            return CmdOutput::text(format!("\r\nYou see no '{target_kw}' to attack here.\r\n"));
        };
        // Engage.
        let (m_name, t_name) = {
            let m  = w.mob_instances.iter().find(|m| m.id == mob_id).unwrap();
            let t  = w.mob_instances.iter().find(|m| m.id == target_id).unwrap();
            let mn = w.mob_protos.get(&m.vnum).map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "your servant".to_string());
            let tn = w.mob_protos.get(&t.vnum).map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "the target".to_string());
            (mn, tn)
        };
        let mob = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        mob.fighting = Some(crate::character::Target { id: target_id, is_player: false });
        let target = w.mob_instances.iter_mut().find(|m| m.id == target_id).unwrap();
        if target.fighting.is_none() {
            target.fighting = Some(crate::character::Target { id: mob_id, is_player: false });
        }
        (m_name, t_name)
    };
    let (m_name, t_name) = result;
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{m_name} springs to attack {t_name}!\r\n"));
    CmdOutput::text(format!(
        "\r\nYou order {m_name} to attack {t_name}.\r\n"
    ))
}

/// `finger <name>` — public profile for any player.  Online players
/// show their current title + an "(online)" tag; offline players show
/// their last-login timestamp.  Unknown names get a "no record" reply.
/// `assist <player>` — engage whatever mob the named player is fighting.
/// Both must be in the same room; the leader must be currently fighting
/// a non-player target.  Refuses if the assister is already in combat.
/// `worship <name>` — set the deity displayed on score.  `worship -`
/// or empty clears.  60-char cap, control-stripped.
/// `afk [msg]` — toggle away-from-keyboard state with an optional
/// reason.  Empty arg toggles; `afk -` clears.  When AFK, others
/// tell'ing you get a one-shot auto-reply containing the message.
fn do_afk(arg: &str, me: &mut Character) -> CmdOutput {
    let arg = arg.trim();
    if arg == "-" {
        me.afk_msg = None;
        return CmdOutput::text("\r\nYou are no longer AFK.\r\n".to_string());
    }
    if arg.is_empty() {
        if me.afk_msg.is_some() {
            me.afk_msg = None;
            return CmdOutput::text("\r\nYou are no longer AFK.\r\n".to_string());
        }
        me.afk_msg = Some("AFK".to_string());
        return CmdOutput::text("\r\nYou are now AFK.\r\n".to_string());
    }
    let sanitized: String = arg.chars().filter(|c| !c.is_control()).take(80).collect();
    me.afk_msg = Some(sanitized.clone());
    CmdOutput::text(format!("\r\nYou are now AFK: {sanitized}\r\n"))
}

/// `bind` — set the caller's personal bind point to the current room.
/// Death-respawn and Word-of-Recall both honor this destination.
/// Refuses on rooms that are TUNNEL/PRIVATE/GODROOM/DEATH/HOUSE — those
/// would either lock the player out on respawn (size cap) or be
/// downright lethal (DEATH).  Refuses the immortal void.

/// `achievements` — list every entry of the catalog with a ✓/✗ marker
/// next to the description.  Also runs an opportunistic check so any
/// newly-earned ones get awarded on the spot.

/// Run the achievement predicate sweep and return a CmdOutput-ready
/// announcement banner (empty string if nothing was earned).  Caller
/// usually appends this to whatever text they're returning so the
/// player sees the award immediately.

/// `unbind` — clear the personal bind point.  Recall + respawn fall
/// back to the canonical start room.

fn do_worship(arg: &str, me: &mut Character) -> CmdOutput {
    let arg = arg.trim();
    if arg.is_empty() || arg == "-" {
        me.god.clear();
        return CmdOutput::text("\r\nYou no longer follow any god.\r\n".to_string());
    }
    let sanitized: String = arg.chars().filter(|c| !c.is_control()).take(60).collect();
    me.god = sanitized;
    CmdOutput::text(format!("\r\nYou now worship {}.\r\n", me.god))
}

async fn do_assist(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.fighting.is_some() {
        return CmdOutput::text("\r\nYou're already in a fight.\r\n".to_string());
    }
    let arg = arg.trim();
    // No-arg (or "all"): pick the first same-room ally currently
    // engaging a mob.  Lets a healer drop in without having to type the
    // tank's name every pull.
    let ph = if arg.is_empty() || arg.eq_ignore_ascii_case("all") {
        let handles: Vec<crate::character::PlayerHandle> = {
            let cl = chars.lock().await;
            cl.iter().filter(|p|
                p.id != me.id
                && p.current_room == me.current_room
            ).cloned().collect()
        };
        let mut found = None;
        for ph in handles {
            let engaging = {
                let c = ph.character.lock().await;
                c.fighting.map(|t| !t.is_player).unwrap_or(false)
            };
            if engaging { found = Some(ph); break; }
        }
        match found {
            Some(p) => p,
            None    => return CmdOutput::text(
                "\r\nNobody here is in a fight to join.\r\n".to_string()
            ),
        }
    } else {
        // Find named target player in same room.
        let ph_opt = {
            let cl = chars.lock().await;
            let h = cl.iter().find(|p|
                p.id != me.id
                && p.current_room == me.current_room
                && p.name.eq_ignore_ascii_case(arg)).cloned();
            h
        };
        match ph_opt {
            Some(p) => p,
            None    => return CmdOutput::text(
                "\r\nNo one here by that name.\r\n".to_string()
            ),
        }
    };
    let target_id = {
        let c = ph.character.lock().await;
        c.fighting.filter(|t| !t.is_player).map(|t| t.id)
    };
    let Some(mob_id) = target_id else {
        return CmdOutput::text(format!("\r\n{} isn't fighting anyone.\r\n", ph.name));
    };
    // Engage.
    let mob_name = {
        let mut w = world.lock().await;
        let m = match w.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => m,
            None    => return CmdOutput::text("\r\nThe target has vanished.\r\n".to_string()),
        };
        let name = w.mob_protos.get(&m.vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".to_string());
        let mm = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if mm.fighting.is_none() {
            mm.fighting = Some(Target { id: me.id, is_player: true });
        }
        name
    };
    me.fighting = Some(Target { id: mob_id, is_player: false });
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} joins the fight against {mob_name}!\r\n", me.name));
    CmdOutput::text(format!("\r\nYou join the fight against {mob_name}!\r\n"))
}

async fn do_finger(
    arg: &str,
    chars: &SharedChars,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nFinger whom?\r\n".to_string());
    }
    // Online first.
    let online = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.name.eq_ignore_ascii_case(arg)).cloned();
        h
    };
    if let Some(ph) = online {
        let (title, class) = {
            let c = ph.character.lock().await;
            (c.title.clone(), c.class)
        };
        return CmdOutput::text(format!(
            "\r\nName:    {}{}\r\nClass:   {:?}\r\nLevel:   {}\r\nStatus:  (online)\r\n",
            ph.name,
            if title.is_empty() { String::new() } else { format!(" {title}") },
            class, ph.level,
        ));
    }
    // Offline.
    let rec = {
        let db = players.lock().await;
        db.find_name(arg).and_then(|n| db.load_player(&n).ok())
    };
    let Some(r) = rec else {
        return CmdOutput::text(format!("\r\nNo record of '{arg}' on this MUD.\r\n"));
    };
    let last = if r.last_login > 0 {
        format!("unix {}", r.last_login)
    } else {
        "unknown".to_string()
    };
    CmdOutput::text(format!(
        "\r\nName:    {}{}\r\nClass:   {:?}\r\nLevel:   {}\r\nStatus:  (offline; last login {last})\r\n",
        r.name,
        if r.title.is_empty() { String::new() } else { format!(" {}", r.title) },
        r.class, r.level,
    ))
}

fn do_pvp(me: &mut Character) -> CmdOutput {
    me.pvp_ok = !me.pvp_ok;
    CmdOutput::text(format!(
        "\r\nPvP: {}. {}\r\n",
        if me.pvp_ok { "ENABLED" } else { "disabled" },
        if me.pvp_ok { "You can now attack and be attacked by other consenting players." }
        else         { "You are now safe from other players." },
    ))
}

fn do_uptime() -> CmdOutput {
    use std::sync::atomic::Ordering;
    let boot = crate::server::BOOT_UNIX_TS.load(Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64).unwrap_or(boot);
    let elapsed = (now - boot).max(0);
    let days  = elapsed / 86400;
    let hours = (elapsed % 86400) / 3600;
    let mins  = (elapsed % 3600) / 60;
    let secs  = elapsed % 60;
    CmdOutput::text(format!(
        "\r\nUp {days}d {hours}h {mins}m {secs}s.\r\nBooted at unix {boot}.\r\n"
    ))
}

/// `peace` (immortal): clear all combat state in the caller's current
/// room.  Affects every mob's `fighting` field and every online
/// player's `fighting`.  Broadcasts a peaceful-calm line.
async fn do_peace(
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let room = me.current_room;
    // Clear mob fighting in this room.
    {
        let mut w = world.lock().await;
        for m in w.mob_instances.iter_mut() {
            if m.in_room == room {
                m.fighting = None;
            }
        }
    }
    // Clear player fighting for everyone in this room.
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().filter(|p| p.current_room == room).cloned().collect()
    };
    for ph in &handles {
        let mut c = ph.character.lock().await;
        c.fighting = None;
    }
    let cl = chars.lock().await;
    cl.broadcast_room(room, None,
        "A peaceful calm descends. All combat ceases.\r\n");
    CmdOutput::text(format!(
        "\r\nYou impose peace; {} combatant(s) calmed.\r\n", handles.len()
    ))
}

fn do_pose(arg: &str, me: &mut Character) -> CmdOutput {
    let arg = arg.trim();
    if arg.is_empty() || arg == "-" {
        me.pose.clear();
        return CmdOutput::text("\r\nPose cleared.\r\n".to_string());
    }
    me.pose = arg.chars().filter(|c| !c.is_control()).take(80).collect();
    CmdOutput::text(format!("\r\nPose set: '{}'.\r\n", me.pose))
}

fn do_notes(me: &Character) -> CmdOutput {
    if me.notes.is_empty() {
        return CmdOutput::text("\r\nYou have no notes yet.\r\n".to_string());
    }
    let mut s = format!("\r\nYour notes ({}):\r\n", me.notes.len());
    for (i, n) in me.notes.iter().enumerate() {
        s.push_str(&format!("  [{}] {n}\r\n", i + 1));
    }
    CmdOutput::text(s)
}

/// `note <text>` appends a note (200-char cap, 50-note cap).
/// `note del &lt;N&gt;` removes the 1-based N-th note.
fn do_note(arg: &str, me: &mut Character) -> CmdOutput {
    const MAX_NOTES: usize  = 50;
    const MAX_LEN:   usize  = 200;
    let arg = arg.trim();
    if arg.is_empty() {
        return do_notes(me);
    }
    let parts: Vec<&str> = arg.splitn(2, char::is_whitespace).collect();
    if parts[0].eq_ignore_ascii_case("del") || parts[0].eq_ignore_ascii_case("delete") {
        let Some(n) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) else {
            return CmdOutput::text("\r\nUsage: note del <N>\r\n".to_string());
        };
        let Some(i) = n.checked_sub(1) else {
            return CmdOutput::text("\r\nBad index.\r\n".to_string());
        };
        if i >= me.notes.len() {
            return CmdOutput::text(format!("\r\nNo note #{n}.\r\n"));
        }
        me.notes.remove(i);
        return CmdOutput::text(format!("\r\nDeleted note #{n}.\r\n"));
    }
    if me.notes.len() >= MAX_NOTES {
        return CmdOutput::text(format!(
            "\r\nYou already have {MAX_NOTES} notes — delete some first.\r\n"
        ));
    }
    let body: String = arg.chars().filter(|c| !c.is_control()).take(MAX_LEN).collect();
    me.notes.push(body);
    CmdOutput::text(format!("\r\nNote #{} saved.\r\n", me.notes.len()))
}

async fn do_emote(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    if me.muted { return muted_msg(); }
    let text = arg.trim();
    if text.is_empty() {
        return CmdOutput::text("\r\nEmote what?\r\n".to_string());
    }
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} {text}\r\n", me.name));
    CmdOutput::text(format!("\r\nYou {text}\r\n"))
}

async fn do_socials_list(world: &Arc<Mutex<World>>) -> CmdOutput {
    let mut names: Vec<String> = {
        let w = world.lock().await;
        w.socials.iter().map(|s| s.name.clone()).collect()
    };
    names.sort();
    let mut s = format!("\r\nLoaded socials ({}):\r\n", names.len());
    for chunk in names.chunks(6) {
        s.push_str("  ");
        for n in chunk {
            s.push_str(&format!("{n:<12}"));
        }
        s.push_str("\r\n");
    }
    CmdOutput::text(s)
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
    // No prompt while in an OLC editor — the menu carries its own prompt.
    if me.olc.is_some() {
        return String::new();
    }
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
            'v' => out.push_str(&me.movement.to_string()),
            'V' => out.push_str(&me.max_movement.to_string()),
            'g' => out.push_str(&me.gold.to_string()),
            'x' => out.push_str(&me.exp.to_string()),
            'r' => out.push_str(&me.current_room.to_string()),
            'a' => out.push_str(&me.alignment.to_string()),
            'c' => out.push_str(me.class.as_str()),
            'L' => out.push_str(&me.level.to_string()),
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
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((s, r)) => (s, r.trim()),
        None         => (arg, ""),
    };
    // `group disband` — leader clears the group for every follower.
    if sub.eq_ignore_ascii_case("disband") {
        // Snapshot all online players currently following me + grouped.
        let followers: Vec<crate::character::PlayerHandle> = {
            let cl = chars.lock().await;
            cl.iter().cloned().collect()
        };
        let mut count = 0u32;
        for ph in followers {
            if ph.id == me.id { continue; }
            let mut c = ph.character.lock().await;
            if c.following == Some(me.id) && c.grouped {
                c.grouped   = false;
                c.following = None;
                drop(c);
                let _ = ph.send.send(format!(
                    "\r\n{} has disbanded the group.\r\n", me.name,
                ));
                count += 1;
            }
        }
        me.grouped = false;
        return CmdOutput::text(format!(
            "\r\nYou disband the group ({count} follower(s) released).\r\n"
        ));
    }
    // `group invite <player>` — must be same-room target.
    if sub.eq_ignore_ascii_case("invite") {
        if rest.is_empty() {
            return CmdOutput::text("\r\nUsage: group invite <player>\r\n".to_string());
        }
        let target = {
            let cl = chars.lock().await;
            let h = cl.iter()
                .find(|p| p.id != me.id
                    && p.current_room == me.current_room
                    && p.name.eq_ignore_ascii_case(rest))
                .cloned();
            h
        };
        let Some(ph) = target else {
            return CmdOutput::text(format!(
                "\r\nNo player named '{rest}' is here.\r\n"
            ));
        };
        ph.character.lock().await.group_invite_from = Some(me.id);
        let _ = ph.send.send(format!(
            "\r\n{} invites you to join their group.  Type `group accept` to join.\r\n",
            me.name,
        ));
        return CmdOutput::text(format!(
            "\r\nYou invite {} to your group.\r\n", ph.name,
        ));
    }
    // `group accept` — consume pending invite, follow + group.
    if sub.eq_ignore_ascii_case("accept") {
        let Some(lid) = me.group_invite_from.take() else {
            return CmdOutput::text("\r\nYou have no pending group invite.\r\n".to_string());
        };
        let leader = {
            let cl = chars.lock().await;
            let h = cl.iter().find(|p| p.id == lid).cloned();
            h
        };
        let Some(lph) = leader else {
            return CmdOutput::text("\r\nYour inviter has gone offline.\r\n".to_string());
        };
        // Refuse if not in the same room any more.
        if lph.current_room != me.current_room {
            return CmdOutput::text(format!(
                "\r\n{} isn't here any more.\r\n", lph.name
            ));
        }
        me.following = Some(lph.id);
        me.grouped   = true;
        // Mark the leader's own `grouped` flag so gtell + group-XP work.
        lph.character.lock().await.grouped = true;
        let _ = lph.send.send(format!(
            "\r\n{} accepts your group invitation and falls in behind you.\r\n",
            me.name,
        ));
        return CmdOutput::text(format!(
            "\r\nYou accept {}'s group invitation.\r\n", lph.name,
        ));
    }
    // `group decline` — clear pending invite.
    if sub.eq_ignore_ascii_case("decline") {
        if me.group_invite_from.take().is_none() {
            return CmdOutput::text("\r\nYou have no pending group invite.\r\n".to_string());
        }
        return CmdOutput::text("\r\nGroup invitation declined.\r\n".to_string());
    }
    // `group status` — HP/mana/movement table for every member.  Anyone
    // in the group can run it (leader OR follower).  Same-room not
    // required — a healer in a back room can still watch the front line.
    if sub.eq_ignore_ascii_case("status") {
        if !me.grouped && me.following.is_none() {
            return CmdOutput::text(
                "\r\nYou're not in any group.\r\n".to_string()
            );
        }
        let leader_id = me.following.unwrap_or(me.id);
        let handles: Vec<crate::character::PlayerHandle> = {
            let cl = chars.lock().await;
            cl.iter().cloned().collect()
        };
        let mut rows: Vec<String> = Vec::new();
        for ph in &handles {
            let is_leader   = ph.id == leader_id;
            let is_follower = {
                let c = ph.character.lock().await;
                c.following == Some(leader_id) && c.grouped
            };
            if !(is_leader || is_follower) { continue; }
            let c = ph.character.lock().await;
            let pos = c.position.room_verb();
            rows.push(format!(
                "  {:<14} lvl {:>2}  hp {:>4}/{:<4}  mn {:>4}/{:<4}  mv {:>4}/{:<4}  ({})\r\n",
                c.name, c.level,
                c.hp, c.max_hp,
                c.mana, c.max_mana,
                c.movement, c.max_movement,
                pos.trim_start_matches("is "),
            ));
        }
        let mut text = String::from("\r\nGroup status:\r\n");
        if rows.is_empty() {
            text.push_str("  (no online group members)\r\n");
        } else {
            text.extend(rows);
        }
        return CmdOutput::text(text);
    }
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
    if me.muted { return muted_msg(); }
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

/// `report`: broadcast your HP/mana/movement to every online group member
/// (same leader derivation as `gtell`).  Completes the group toolkit
/// alongside `gtell` (cp34), `group status` (cp195), and `split` (cp201).
async fn do_report(me: &Character, chars: &SharedChars) -> CmdOutput {
    let line = format!(
        "{} reports: {}/{}H {}/{}M {}/{}V.",
        me.name, me.hp, me.max_hp, me.mana, me.max_mana, me.movement, me.max_movement,
    );
    if me.grouped || me.following.is_some() {
        let leader_id = me.following.unwrap_or(me.id);
        let formatted = format!("\r\n{line}\r\n");
        let handles: Vec<crate::character::PlayerHandle> = {
            let cl = chars.lock().await;
            cl.iter().cloned().collect()
        };
        for ph in &handles {
            if ph.id == me.id { continue; }
            let c = ph.character.lock().await;
            let in_group = (c.id == leader_id && c.grouped)
                || (c.following == Some(leader_id) && c.grouped);
            if in_group {
                let _ = ph.send.send(formatted.clone());
            }
        }
    }
    CmdOutput::text(format!("\r\nYou report: {}/{}H {}/{}M {}/{}V.\r\n",
        me.hp, me.max_hp, me.mana, me.max_mana, me.movement, me.max_movement))
}

/// `split <amount>`: divide gold evenly among the group members present in
/// the splitter's room (including the splitter). Any indivisible remainder
/// stays with the splitter. Mirrors classic Diku `do_split`.
async fn do_split(arg: &str, me: &mut Character, chars: &SharedChars) -> CmdOutput {
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nSplit how much gold?\r\n".to_string());
    }
    let Ok(amount) = arg.parse::<i64>() else {
        return CmdOutput::text("\r\nThat's not a valid amount.\r\n".to_string());
    };
    if amount <= 0 {
        return CmdOutput::text("\r\nSplit some positive amount of gold.\r\n".to_string());
    }
    if amount > me.gold {
        return CmdOutput::text("\r\nYou don't have that much gold.\r\n".to_string());
    }

    let leader_id = me.following.unwrap_or(me.id);
    let here = me.current_room;
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    // Collect the other group members present in my room.
    let mut recipients: Vec<crate::character::PlayerHandle> = Vec::new();
    for ph in &handles {
        if ph.id == me.id { continue; }
        let c = ph.character.lock().await;
        let in_group = (c.id == leader_id && c.grouped)
            || (c.following == Some(leader_id) && c.grouped);
        if in_group && c.current_room == here {
            recipients.push(ph.clone());
        }
    }
    if recipients.is_empty() {
        return CmdOutput::text(
            "\r\nWith whom do you wish to share your gold?\r\n".to_string()
        );
    }

    let num = recipients.len() as i64 + 1; // +1 for the splitter
    let share = amount / num;
    if share == 0 {
        return CmdOutput::text(
            "\r\nThere isn't enough gold to go around.\r\n".to_string()
        );
    }
    let paid_out = share * recipients.len() as i64;
    // Splitter keeps their own share plus any remainder.
    me.gold -= paid_out;

    for ph in &recipients {
        let mut c = ph.character.lock().await;
        c.gold += share;
        let _ = ph.send.send(format!(
            "\r\n{} splits {} coins; you receive {} of them.\r\n",
            me.name, amount, share,
        ));
    }
    CmdOutput::text(format!(
        "\r\nYou split {} coins among {} members — {} each.\r\n",
        amount, num, share,
    ))
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

/// `bug` / `idea` / `typo` (cp239): append a player report to the
/// matching `lib/misc/<kind>` file (bugs / ideas / typos), stamped with
/// the player's name, room, and a unix timestamp.  Mirrors the classic
/// CircleMUD feedback commands.
async fn do_submit(kind: &str, text: &str, me: &Character) -> CmdOutput {
    let text = text.trim();
    if text.is_empty() {
        return CmdOutput::text(format!("\r\nUsage: {kind} <text>\r\n"));
    }
    let Some(players_arc) = PLAYERS_HANDLE.get() else {
        return CmdOutput::text("\r\nReports are unavailable right now.\r\n".to_string());
    };
    // File name: bug→bugs, idea→ideas, typo→typos.
    let file = match kind { "bug" => "bugs", "idea" => "ideas", _ => "typos" };
    let data_dir = players_arc.lock().await.data_dir().to_string();
    let dir = format!("{data_dir}/misc");
    let _ = std::fs::create_dir_all(&dir);
    let path = format!("{dir}/{file}");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64).unwrap_or(0);
    let line = format!("[{ts}] {} (room {}): {text}\n", me.name, me.current_room);
    use std::io::Write;
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => { let _ = f.write_all(line.as_bytes()); }
        Err(e)    => {
            tracing::warn!(error = %e, "Failed to append {file}");
            return CmdOutput::text("\r\nYour report could not be saved.\r\n".to_string());
        }
    }
    CmdOutput::text(format!("\r\nThank you — your {kind} has been logged.\r\n"))
}

/// A short ambient sky line for outdoor room descriptions (cp237),
/// derived from the live weather sim (cp212) + day/night band (cp111).
/// Returns None on a clear daytime sky (nothing worth noting).
fn weather_line() -> Option<&'static str> {
    use std::sync::atomic::Ordering;
    let sky = crate::db::WEATHER_SKY.load(Ordering::Relaxed);
    let night = matches!(crate::db::sun_state(),
        crate::db::SunState::Dark | crate::db::SunState::Set);
    Some(match sky {
        crate::db::SKY_RAINING   => "  Rain falls steadily from the grey sky.",
        crate::db::SKY_LIGHTNING => "  A storm rages overhead, lightning splitting the sky.",
        crate::db::SKY_CLOUDY    => "  Clouds hang low overhead.",
        _ => return if night { Some("  Stars glitter in the clear night sky.") } else { None },
    })
}

fn do_weather() -> CmdOutput {
    use std::sync::atomic::Ordering;
    let h = crate::db::GAME_HOUR.load(Ordering::Relaxed);
    let sky = crate::db::WEATHER_SKY.load(Ordering::Relaxed);
    let change = crate::db::WEATHER_CHANGE.load(Ordering::Relaxed);
    // Live sky state, driven by the weather simulation (cp212).
    let desc = match sky {
        crate::db::SKY_CLOUDLESS => "The sky is cloudless and bright.",
        crate::db::SKY_CLOUDY    => "Grey clouds blanket the sky.",
        crate::db::SKY_RAINING   => "Cold rain falls in sheets.",
        _                        => "A fierce storm rages overhead, lightning crackling in the distance.",
    };
    let trend = if change > 0 { "The pressure is rising — the weather is clearing." }
                else if change < 0 { "The pressure is dropping — the weather is worsening." }
                else { "The pressure is steady." };
    let lit = if (6..20).contains(&h) { "It is daytime." } else { "It is night." };
    CmdOutput::text(format!("\r\n{lit}\r\n{desc}\r\n{trend}\r\n"))
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
    if me.muted { return muted_msg(); }
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
    record_channel("auction", &me.name, msg).await;
    CmdOutput::text(format!("\r\n@YYou auction, '{msg}'@n\r\n"))
}

/// `whisper <player> <msg>` — private same-room speech.  The named
/// player and the sender see the full text; everyone else in the room
/// sees "Name whispers something to Target." without content.
async fn do_whisper(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    do_spec_comm(arg, me, chars, false).await
}

/// Same-room private speech: `whisper` (ask=false) or `ask` (ask=true).
/// Mirrors stock do_spec_comm (SCMD_WHISPER / SCMD_ASK).
async fn do_spec_comm(arg: &str, me: &Character, chars: &SharedChars, ask: bool) -> CmdOutput {
    if me.muted { return muted_msg(); }
    let (verb, prompt) = if ask { ("ask", "Ask whom what?") } else { ("whisper to", "Whisper to whom what?") };
    let (target, msg) = match arg.find(char::is_whitespace) {
        Some(i) => (arg[..i].trim(), arg[i..].trim()),
        None    => return CmdOutput::text(format!("\r\n{prompt}\r\n")),
    };
    if target.is_empty() || msg.is_empty() {
        return CmdOutput::text(format!("\r\n{prompt}\r\n"));
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
    if ask {
        let _ = ph.send.send(format!("\r\n{} asks you, '{msg}'\r\n", me.name));
    } else {
        let _ = ph.send.send(format!("\r\n{} whispers to you, '{msg}'\r\n", me.name));
    }
    // Room peers (except me and the recipient) see a redacted line.
    let cl = chars.lock().await;
    let line = if ask {
        format!("\r\n{} asks {} a question.\r\n", me.name, ph.name)
    } else {
        format!("\r\n{} whispers something to {}.\r\n", me.name, ph.name)
    };
    for peer in cl.iter() {
        if peer.id == me.id || peer.id == ph.id { continue; }
        if peer.current_room != me.current_room { continue; }
        let _ = peer.send.send(line.clone());
    }
    self_echo(me, format!("\r\nYou {verb} {}, '{msg}'\r\n", ph.name))
}

/// Self-echo for communication: blanked when the player has `norepeat` set
/// (mirrors stock PRF_NOREPEAT).
fn self_echo(me: &Character, s: String) -> CmdOutput {
    if me.norepeat { CmdOutput::text(String::new()) } else { CmdOutput::text(s) }
}

/// `cls` — clear the screen (ANSI home + erase-display).
fn do_cls() -> CmdOutput {
    CmdOutput::text("\x1b[H\x1b[J".to_string())
}

/// `norepeat` — toggle suppression of your own communication echo.
fn do_norepeat(me: &mut Character) -> CmdOutput {
    me.norepeat = !me.norepeat;
    CmdOutput::text(format!(
        "\r\nYou will {} your own communication.\r\n",
        if me.norepeat { "no longer see" } else { "now see" }))
}

/// `hindex <prefix>` — list help topics whose keyword starts with the prefix.
async fn do_hindex(arg: &str, me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let needle = arg.trim().to_ascii_uppercase();
    if needle.is_empty() {
        return CmdOutput::text("\r\nUsage: hindex <prefix>\r\n".to_string());
    }
    let w = world.lock().await;
    let mut hits: Vec<&str> = Vec::new();
    for e in &w.help {
        if e.min_level > me.level { continue; }
        for k in &e.keywords {
            if k.starts_with(&needle) { hits.push(k.as_str()); }
        }
    }
    hits.sort_unstable();
    hits.dedup();
    if hits.is_empty() {
        return CmdOutput::text(format!("\r\nNo help topics start with '{}'.\r\n", arg.trim()));
    }
    let mut s = format!("\r\nHelp topics starting with '{}':\r\n", arg.trim());
    for (i, k) in hits.iter().enumerate() {
        s.push_str(&format!("{:<20}", k));
        if (i + 1) % 4 == 0 { s.push_str("\r\n"); }
    }
    if hits.len() % 4 != 0 { s.push_str("\r\n"); }
    s.push_str(&format!("({} topic(s))\r\n", hits.len()));
    CmdOutput::text(s)
}

/// `display <all|none|prompt elements>` — set the prompt to a quick preset.
/// Mirrors stock `do_display` (choose which of HP/Mana/Move appear).
fn do_display(arg: &str, me: &mut Character) -> CmdOutput {
    let a = arg.trim().to_ascii_lowercase();
    if a.is_empty() {
        return CmdOutput::text(
            "\r\nUsage: display <all | none | a string of h m v>\r\n\
             e.g. `display hmv` shows hit/mana/move; `display none` shows just '> '.\r\n".to_string());
    }
    if a == "none" {
        me.prompt_format = "> ".to_string();
        return CmdOutput::text("\r\nPrompt set to minimal.\r\n".to_string());
    }
    if a == "all" {
        me.prompt_format = "%h/%HH %m/%MM %v/%VV> ".to_string();
        return CmdOutput::text("\r\nPrompt now shows HP, mana and movement.\r\n".to_string());
    }
    let mut p = String::new();
    if a.contains('h') { p.push_str("%h/%HH "); }
    if a.contains('m') { p.push_str("%m/%MM "); }
    if a.contains('v') { p.push_str("%v/%VV "); }
    p.push_str("> ");
    me.prompt_format = p;
    CmdOutput::text("\r\nPrompt updated.\r\n".to_string())
}

fn muted_msg() -> CmdOutput {
    CmdOutput::text("\r\nYou are muted and cannot speak.\r\n".to_string())
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
    if me.muted { return muted_msg(); }
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
    let spoken = garble_drunk(msg, me.drunk);
    let formatted = format!("\r\n@c{} gossips: '{spoken}'@n\r\n", me.name);
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
    record_channel("gossip", &me.name, msg).await;
    self_echo(me, format!("\r\n@cYou gossip, '{spoken}'@n\r\n"))
}

/// `grats` — the congratulations channel.  Mirrors `do_gossip` but uses
/// the grats toggle and a green envelope.
async fn do_grats(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.muted { return muted_msg(); }
    let msg = arg.trim();
    if msg.is_empty() {
        me.grats_off = !me.grats_off;
        return CmdOutput::text(format!(
            "\r\nGrats channel: {}.\r\n",
            if me.grats_off { "off" } else { "on" },
        ));
    }
    if me.grats_off {
        return CmdOutput::text("\r\nYou have the grats channel turned off.\r\n".to_string());
    }
    {
        let w = world.lock().await;
        if w.rooms.get(&me.current_room)
            .map(|r| r.room_flags[0] & crate::world::ROOM_SOUNDPROOF != 0)
            .unwrap_or(false)
        {
            return CmdOutput::text(
                "\r\nThe walls dampen your voice.\r\n".to_string());
        }
    }
    let formatted = format!("\r\n@g{} congrats: '{msg}'@n\r\n", me.name);
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    for ph in &handles {
        if ph.id == me.id { continue; }
        if ph.character.lock().await.grats_off { continue; }
        let _ = ph.send.send(formatted.clone());
    }
    record_channel("grats", &me.name, msg).await;
    CmdOutput::text(format!("\r\n@gYou congrats, '{msg}'@n\r\n"))
}

/// `holler` — a global shout that costs movement points.  Mirrors stock
/// `do_gen_comm` SCMD_HOLLER (reaches everyone, ignores soundproof for
/// the listener side; costs the holler-er movement).
async fn do_holler(
    arg: &str,
    me: &mut Character,
    chars: &SharedChars,
) -> CmdOutput {
    if me.muted { return muted_msg(); }
    let msg = arg.trim();
    if msg.is_empty() {
        return CmdOutput::text("\r\nYell what?\r\n".to_string());
    }
    const HOLLER_COST: i32 = 20;
    if me.level < LVL_IMMORT {
        if me.movement < HOLLER_COST {
            return CmdOutput::text("\r\nYou're too exhausted to holler.\r\n".to_string());
        }
        me.movement -= HOLLER_COST;
    }
    let formatted = format!("\r\n@Y{} hollers, '{msg}'@n\r\n", me.name);
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    for ph in &handles {
        if ph.id == me.id { continue; }
        let _ = ph.send.send(formatted.clone());
    }
    CmdOutput::text(format!("\r\n@YYou holler, '{msg}'@n\r\n"))
}

/// `version` — display the MUD's identity/version banner.
fn do_version() -> CmdOutput {
    CmdOutput::text(
        "\r\nTbaMUD (Rust rewrite, 'rwb') — a CircleMUD/DikuMUD derivative.\r\n"
            .to_string())
}

/// `visible` — drop magical invisibility and any sneak/hide so others can
/// see you again.  Mirrors stock `do_visible` for mortals.
fn do_visible(me: &mut Character) -> CmdOutput {
    me.hidden = false;
    me.sneaking = false;
    let had_invis = me.affects.iter()
        .any(|a| a.skill == crate::character::Skill::Invisibility);
    me.affects.retain(|a| a.skill != crate::character::Skill::Invisibility);
    if had_invis {
        CmdOutput::text("\r\nYou shimmer back into view.\r\n".to_string())
    } else {
        CmdOutput::text("\r\nYou step out of the shadows.\r\n".to_string())
    }
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

/// `at <room-vnum> <command>` — immortal-only.  Teleports to the named
/// room and then forces the command as oneself via FORCE_CMD_TX so the
/// dispatch runs at the new location.  Doesn't auto-return; immortals
/// `goto` back when done.
async fn do_at(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    _players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let mut parts = arg.splitn(2, char::is_whitespace);
    let vnum_s = parts.next().unwrap_or("");
    let command = parts.next().unwrap_or("").trim();
    let Ok(vnum) = vnum_s.parse::<i32>() else {
        return CmdOutput::text("\r\nUsage: at <room-vnum> <command>\r\n".to_string());
    };
    if command.is_empty() {
        return CmdOutput::text("\r\nWhat command should run there?\r\n".to_string());
    }
    {
        let w = world.lock().await;
        if !w.rooms.contains_key(&vnum) {
            return CmdOutput::text(format!("\r\nNo such room: {vnum}.\r\n"));
        }
    }
    // Teleport to target room (mirrors do_goto's room update).
    me.current_room = vnum;
    {
        let mut cl = chars.lock().await;
        cl.update_room(me.id, vnum);
    }
    // Post a forced command to self.  The runner picks it up after our
    // current dispatch returns, by which point we're already at the target.
    if let Some(tx) = FORCE_CMD_TX.get() {
        let _ = tx.send(ForceCmdMsg {
            player:  me.name.clone(),
            command: command.to_string(),
            world:   Arc::clone(world),
            chars:   Arc::clone(chars),
        });
    }
    CmdOutput::text(format!("\r\nYou flicker to [{vnum}] and queue '{command}'.\r\n"))
}

/// `house [name|-]` — no-arg shows the current room's house status
/// + owner.  Immortals only: `house <name>` assigns the owner;
/// `house -` clears it.
async fn do_house(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    let arg = arg.trim();
    let is_house = {
        let w = world.lock().await;
        w.rooms.get(&me.current_room)
            .map(|r| r.room_flags[0] & crate::world::ROOM_HOUSE != 0)
            .unwrap_or(false)
    };
    let data_dir = players.lock().await.data_dir().to_string();
    if arg.is_empty() {
        if !is_house {
            return CmdOutput::text("\r\nThis room is not a house.\r\n".to_string());
        }
        let owner = {
            let w = world.lock().await;
            w.house_owners.get(&me.current_room).cloned()
        };
        return CmdOutput::text(match owner {
            Some(n) => format!("\r\nThis house belongs to {n}.\r\n"),
            None    => "\r\nThis house has no recorded owner.\r\n".to_string(),
        });
    }
    if me.level < LVL_IMMORT {
        return immort_huh();
    }
    if !is_house {
        return CmdOutput::text(
            "\r\nThis room isn't flagged as a house yet — use `househere` first.\r\n".to_string()
        );
    }
    if arg == "-" {
        crate::db::save_house_owner(&data_dir, me.current_room, "");
        world.lock().await.house_owners.remove(&me.current_room);
        return CmdOutput::text("\r\nHouse ownership cleared.\r\n".to_string());
    }
    let owner = arg.chars().filter(|c| !c.is_control()).take(30).collect::<String>();
    crate::db::save_house_owner(&data_dir, me.current_room, &owner);
    world.lock().await.house_owners.insert(me.current_room, owner.clone());
    CmdOutput::text(format!("\r\nHouse owner set to {owner}.\r\n"))
}

/// `househere` (LVL_IMMORT+) — toggles `ROOM_HOUSE` on the current
/// room.  When set, the room's floor contents persist via
/// `db::spawn_house_save_tick`.
async fn do_househere(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let mut w = world.lock().await;
    let Some(r) = w.rooms.get_mut(&me.current_room) else {
        return CmdOutput::text("\r\nYou are nowhere.\r\n".to_string());
    };
    let now_house = (r.room_flags[0] & crate::world::ROOM_HOUSE) == 0;
    if now_house {
        r.room_flags[0] |= crate::world::ROOM_HOUSE;
    } else {
        r.room_flags[0] &= !crate::world::ROOM_HOUSE;
    }
    CmdOutput::text(format!(
        "\r\nROOM_HOUSE is now {} for room [{}].\r\n",
        if now_house { "ON" } else { "OFF" }, me.current_room,
    ))
}

/// `set <player> <field> <value>` — immortal-only. Supports a handful
/// of common fields: level / hp / maxhp / mana / maxmana / gold / exp /
/// room. Room change updates the registry and broadcasts a "vanishes"
/// / "appears" pair so other players see the move.
/// `dig <direction> <vnum>` (cp236): carve a two-way exit from the room
/// the immortal is in to the room `vnum`, creating that room (blank, same
/// zone) if it doesn't exist yet.  The reverse exit (opposite direction)
/// is linked back automatically.  A live building tool atop `rset`/`oset`.
async fn do_dig(arg: &str, me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let parts: Vec<&str> = arg.split_whitespace().collect();
    if parts.len() < 2 {
        return CmdOutput::text("\r\nUsage: dig <direction> <room-vnum>\r\n".to_string());
    }
    let Some(dir) = Direction::parse(parts[0]) else {
        return CmdOutput::text(format!("\r\n'{}' isn't a direction.\r\n", parts[0]));
    };
    let Ok(dest) = parts[1].parse::<crate::world::RoomVnum>() else {
        return CmdOutput::text("\r\nThat's not a valid room vnum.\r\n".to_string());
    };
    let here = me.current_room;
    if dest == here {
        return CmdOutput::text("\r\nYou can't dig a room to itself.\r\n".to_string());
    }
    let mut w = world.lock().await;
    let zone = w.rooms.get(&here).map(|r| r.zone).unwrap_or(0);
    let created = if !w.rooms.contains_key(&dest) {
        let mut room = crate::world::Room::default();
        room.vnum = dest;
        room.zone = zone;
        room.name = "An Unfinished Room".to_string();
        room.description = "This room has not been described yet.\r\n".to_string();
        w.rooms.insert(dest, room);
        true
    } else { false };
    // Link here -> dest in `dir`.
    if let Some(r) = w.rooms.get_mut(&here) {
        let mut e = crate::world::Exit::default();
        e.to_room = dest;
        r.exits[dir as usize] = Some(e);
    }
    // Link dest -> here in the opposite direction.
    if let Some(r) = w.rooms.get_mut(&dest) {
        let mut e = crate::world::Exit::default();
        e.to_room = here;
        r.exits[dir.opposite() as usize] = Some(e);
    }
    CmdOutput::text(format!(
        "\r\nYou dig {} to room {dest}{}.\r\n",
        dir.name(),
        if created { " (new room created)" } else { "" },
    ))
}

/// `rset <field> <value>` (cp235): edit the room the immortal is standing
/// in (builder tool, room parallel to `oset`/`mset`).  Fields: `sector
/// <n>` sets the sector type, `name <text>` renames the room, `flags <n>`
/// overwrites `room_flags[0]` (raw bitmask).  Operates on `me.current_room`
/// — no vnum argument needed.

/// `mset <vnum> <field> <value>` (cp234): edit a mob prototype's numeric
/// stats at runtime (immortal builder tool, mob parallel to `oset`).
/// Fields: level, hitroll, damroll, ac, gold, exp, alignment, hpdice,
/// hpsize, hpadd, damdice, damsize.  Affects the prototype (future
/// spawns); already-spawned instances keep their rolled HP.

/// `oset <vnum> <field> <value>` (cp233): edit an object prototype's
/// numeric stats at runtime (immortal builder tool).  Fields: cost,
/// weight, level, timer, value0..value3.  Affects the prototype, so it
/// changes all current and future instances (consistent with our
/// shared-proto model for charges/sips/etc).
async fn do_oset(arg: &str, me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let parts: Vec<&str> = arg.split_whitespace().collect();
    if parts.len() < 3 {
        return CmdOutput::text(
            "\r\nUsage: oset <vnum> <field> <value>\r\n  Fields: cost weight level timer value0 value1 value2 value3\r\n".to_string()
        );
    }
    let Ok(vnum) = parts[0].parse::<crate::world::ObjVnum>() else {
        return CmdOutput::text("\r\nThat's not a valid object vnum.\r\n".to_string());
    };
    let field = parts[1].to_ascii_lowercase();
    let Ok(v) = parts[2].parse::<i32>() else {
        return CmdOutput::text(format!("\r\nBad value for '{field}'.\r\n"));
    };
    let mut w = world.lock().await;
    let Some(p) = w.obj_protos.get_mut(&vnum) else {
        return CmdOutput::text(format!("\r\nNo object prototype with vnum {vnum}.\r\n"));
    };
    match field.as_str() {
        "cost"   => p.cost = v,
        "weight" => p.weight = v,
        "level"  => p.level = v,
        "timer"  => p.timer = v,
        "value0" => p.value[0] = v,
        "value1" => p.value[1] = v,
        "value2" => p.value[2] = v,
        "value3" => p.value[3] = v,
        _ => return CmdOutput::text(format!("\r\nUnknown field '{field}'.\r\n")),
    }
    let short = p.short_description.clone();
    CmdOutput::text(format!("\r\nSet {short} (vnum {vnum}) {field} = {v}.\r\n"))
}

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
/// `mute <player>` (immortal): toggle the target's muted state.  Muted
/// players have their channel commands refused with "You're muted."
async fn do_mute(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nMute whom?\r\n".to_string());
    }
    let ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.name.eq_ignore_ascii_case(arg)).cloned();
        h
    };
    let Some(ph) = ph else {
        return CmdOutput::text("\r\nNobody by that name is online.\r\n".to_string());
    };
    let now_muted = {
        let mut c = ph.character.lock().await;
        c.muted = !c.muted;
        c.muted
    };
    let _ = ph.send.send(format!(
        "\r\n{} has {}muted you.\r\n",
        me.name, if now_muted { "" } else { "un-" },
    ));
    CmdOutput::text(format!(
        "\r\nYou {} {}.\r\n",
        if now_muted { "mute" } else { "unmute" }, ph.name,
    ))
}

/// `freeze <player>` (immortal): toggle the target's frozen state.
/// Frozen players have nearly all commands refused.
async fn do_freeze(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text("\r\nFreeze whom?\r\n".to_string());
    }
    let ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.name.eq_ignore_ascii_case(arg)).cloned();
        h
    };
    let Some(ph) = ph else {
        return CmdOutput::text("\r\nNobody by that name is online.\r\n".to_string());
    };
    let now_frozen = {
        let mut c = ph.character.lock().await;
        c.frozen = !c.frozen;
        c.frozen
    };
    let _ = ph.send.send(format!(
        "\r\n{} has {}frozen you.\r\n",
        me.name, if now_frozen { "" } else { "un-" },
    ));
    CmdOutput::text(format!(
        "\r\nYou {} {}.\r\n",
        if now_frozen { "freeze" } else { "unfreeze" }, ph.name,
    ))
}

fn do_invis(arg: &str, me: &mut Character) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let target = arg.trim().parse::<i32>().unwrap_or(LVL_IMMORT);
    me.invis_level = target.clamp(0, 34);
    if me.invis_level == 0 {
        CmdOutput::text("\r\nYou are now fully visible.\r\n".to_string())
    } else {
        CmdOutput::text(format!("\r\nYou fade to invis level {}.\r\n", me.invis_level))
    }
}

fn do_vis(me: &mut Character) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    me.invis_level = 0;
    CmdOutput::text("\r\nYou are now fully visible.\r\n".to_string())
}

/// `nohassle`: toggle immunity to aggressive / memory-grudge mobs (cp202).
/// Immortal-only.  Defaults on for gods at login; this lets them turn it off
/// to test aggression behaviour without dropping to a mortal character.
fn do_nohassle(me: &mut Character) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    me.nohassle = !me.nohassle;
    CmdOutput::text(format!(
        "\r\nNohassle is now {}. Aggressive mobs {} you.\r\n",
        if me.nohassle { "ON" } else { "OFF" },
        if me.nohassle { "ignore" } else { "can now attack" },
    ))
}

async fn do_zreset(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let Ok(zv) = arg.trim().parse::<i32>() else {
        return CmdOutput::text("\r\nUsage: zreset <zone-vnum>\r\n".to_string());
    };
    let (mobs_before, objs_before, mobs_after, objs_after, ok) = {
        let mut w = world.lock().await;
        if !w.zones.contains_key(&zv) {
            return CmdOutput::text(format!("\r\nNo zone with vnum {zv}.\r\n"));
        }
        let mb = w.mob_instances.len();
        let ob = w.obj_instances.len();
        crate::db::reset_zone(&mut w, zv);
        let ma = w.mob_instances.len();
        let oa = w.obj_instances.len();
        (mb, ob, ma, oa, true)
    };
    let _ = ok;
    CmdOutput::text(format!(
        "\r\nZone {zv} reset.\r\n  Mobs:    {mobs_before} → {mobs_after}\r\n  Objects: {objs_before} → {objs_after}\r\n",
    ))
}

/// Shared parser for vnum range args: `""`, `"N"`, `"N-M"`.
fn parse_vnum_range(arg: &str) -> Option<(i32, i32)> {
    let s = arg.trim();
    if s.is_empty() { return Some((0, 60)); }
    if let Some((a, b)) = s.split_once('-') {
        let lo: i32 = a.parse().ok()?;
        let hi: i32 = b.parse().ok()?;
        if hi < lo { None } else { Some((lo, hi)) }
    } else {
        let n: i32 = s.parse().ok()?;
        Some((n, n + 30))
    }
}

async fn do_mlist(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let Some((lo, hi)) = parse_vnum_range(arg) else {
        return CmdOutput::text("\r\nBad range.\r\n".to_string());
    };
    let w = world.lock().await;
    let mut s = format!("\r\nMob prototypes [{lo}..{hi}]:\r\n");
    let mut shown = 0;
    for (vnum, p) in w.mob_protos.range(lo..=hi) {
        if shown >= 100 { s.push_str("  ... (truncated)\r\n"); break; }
        s.push_str(&format!(
            "  [{:>5}] {:<40} level={}\r\n",
            vnum, p.short_descr.chars().take(40).collect::<String>(), p.level,
        ));
        shown += 1;
    }
    if shown == 0 { s.push_str("  (no mobs in range)\r\n"); }
    CmdOutput::text(s)
}

async fn do_rlist(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let Some((lo, hi)) = parse_vnum_range(arg) else {
        return CmdOutput::text("\r\nBad range.\r\n".to_string());
    };
    let w = world.lock().await;
    let mut s = format!("\r\nRooms [{lo}..{hi}]:\r\n");
    let mut shown = 0;
    for (vnum, r) in w.rooms.range(lo..=hi) {
        if shown >= 100 { s.push_str("  ... (truncated)\r\n"); break; }
        s.push_str(&format!(
            "  [{:>5}] zone={:<3} {}\r\n",
            vnum, r.zone, r.name.chars().take(50).collect::<String>(),
        ));
        shown += 1;
    }
    if shown == 0 { s.push_str("  (no rooms in range)\r\n"); }
    CmdOutput::text(s)
}

async fn do_zlist(
    me: &Character,
    world: &Arc<Mutex<World>>,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let w = world.lock().await;
    let mut s = format!("\r\nZones ({}):\r\n", w.zones.len());
    let mut shown = 0;
    for z in w.zones.values() {
        if shown >= 200 { s.push_str("  ... (truncated)\r\n"); break; }
        s.push_str(&format!(
            "  [{:>3}] {:<30} rooms {}..{}\r\n",
            z.number, z.name.chars().take(30).collect::<String>(),
            z.bot, z.top,
        ));
        shown += 1;
    }
    CmdOutput::text(s)
}

async fn do_olist(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let Some((lo, hi)) = parse_vnum_range(arg) else {
        return CmdOutput::text("\r\nBad range.\r\n".to_string());
    };
    let w = world.lock().await;
    let mut s = format!("\r\nObject prototypes [{lo}..{hi}]:\r\n");
    let mut shown = 0;
    for (vnum, p) in w.obj_protos.range(lo..=hi) {
        if shown >= 100 { s.push_str("  ... (truncated)\r\n"); break; }
        s.push_str(&format!(
            "  [{:>5}] {:<35} type={:<8} level={}\r\n",
            vnum,
            p.short_description.chars().take(35).collect::<String>(),
            item_type_name(p.item_type),
            p.level,
        ));
        shown += 1;
    }
    if shown == 0 { s.push_str("  (no objects in range)\r\n"); }
    CmdOutput::text(s)
}

/// `load mob <vnum>` / `load obj <vnum>` — immortal builder command
/// to spawn a mob into the caller's current room (mob path) or to
/// spawn an object into the caller's inventory (obj path).
async fn do_load(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let mut parts = arg.split_whitespace();
    let kind = parts.next().unwrap_or("");
    let vnum_s = parts.next().unwrap_or("");
    let Ok(vnum) = vnum_s.parse::<i32>() else {
        return CmdOutput::text(
            "\r\nUsage: load <mob|obj> <vnum>\r\n".to_string()
        );
    };
    match kind {
        "mob" | "m" => {
            let (id_opt, short) = {
                let mut w = world.lock().await;
                let id = w.spawn_mob(vnum, me.current_room);
                let s = w.mob_protos.get(&vnum)
                    .map(|p| p.short_descr.clone())
                    .unwrap_or_else(|| "something".to_string());
                (id, s)
            };
            if id_opt.is_none() {
                return CmdOutput::text(format!(
                    "\r\nNo mob prototype with vnum {vnum}.\r\n"
                ));
            }
            chars.lock().await.broadcast_room(
                me.current_room, Some(me.id),
                &format!("{} appears in a puff of smoke.\r\n", short),
            );
            CmdOutput::text(format!("\r\nYou create {short}.\r\n"))
        }
        "obj" | "o" => {
            let (id_opt, short) = {
                let mut w = world.lock().await;
                let id = w.spawn_obj(vnum);
                let s = w.obj_protos.get(&vnum)
                    .map(|p| p.short_description.clone())
                    .unwrap_or_else(|| "something".to_string());
                (id, s)
            };
            let Some(_iid) = id_opt else {
                return CmdOutput::text(format!(
                    "\r\nNo object prototype with vnum {vnum}.\r\n"
                ));
            };
            // We can't reach the caller's Character.inventory from here
            // (we only have &Character).  Drop the obj at the caller's
            // feet — they can `get` it.  Set in_room.
            {
                let mut w = world.lock().await;
                if let Some(o) = w.obj_instances.last_mut() {
                    o.in_room = me.current_room;
                }
            }
            chars.lock().await.broadcast_room(
                me.current_room, Some(me.id),
                &format!("{} appears in a puff of smoke.\r\n", short),
            );
            CmdOutput::text(format!(
                "\r\nYou create {short} (dropped at your feet).\r\n"
            ))
        }
        _ => CmdOutput::text(
            "\r\nUsage: load <mob|obj> <vnum>\r\n".to_string()
        ),
    }
}

/// `restore <player>` — heal target to full HP and mana.
async fn do_restore(
    arg: &str,
    me: &Character,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let name = arg.trim();
    if name.is_empty() {
        return CmdOutput::text(
            "\r\nUsage: restore <player>\r\n".to_string()
        );
    }
    let target = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|h| h.name.eq_ignore_ascii_case(name)).cloned();
        h
    };
    let Some(ph) = target else {
        return CmdOutput::text(format!(
            "\r\nNo player named '{name}' is online.\r\n"
        ));
    };
    {
        let mut c = ph.character.lock().await;
        c.hp = c.max_hp;
        c.mana = c.max_mana;
    }
    let _ = ph.send.send(format!(
        "\r\n{} has restored you.\r\n", me.name
    ));
    CmdOutput::text(format!(
        "\r\nYou restore {} to full health.\r\n", ph.name
    ))
}

/// `echo <text>` — broadcast a plain message to every player in the
/// caller's current room (including the caller).  No name prefix.
async fn do_echo(
    arg: &str,
    me: &Character,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let msg = arg.trim();
    if msg.is_empty() {
        return CmdOutput::text("\r\nUsage: echo <text>\r\n".to_string());
    }
    chars.lock().await.broadcast_room(
        me.current_room, None,
        &format!("{msg}\r\n"),
    );
    CmdOutput::text(String::new())
}

/// `gecho <text>` — broadcast to every online player on the MUD.
async fn do_gecho(
    arg: &str,
    me: &Character,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let msg = arg.trim();
    if msg.is_empty() {
        return CmdOutput::text("\r\nUsage: gecho <text>\r\n".to_string());
    }
    let line = format!("\r\n{msg}\r\n");
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    for ph in &handles {
        let _ = ph.send.send(line.clone());
    }
    CmdOutput::text(String::new())
}

/// `slay <target>` — immortal one-shot kill.  Targets a mob in the
/// caller's room by keyword.  PvP slay is intentionally not supported
/// (use `force <player> quit` for that effect instead).
async fn do_slay(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let key = arg.trim().to_ascii_lowercase();
    if key.is_empty() {
        return CmdOutput::text("\r\nSlay whom?\r\n".to_string());
    }
    let (mob_id, mob_short) = {
        let w = world.lock().await;
        let r = match w.rooms.get(&me.current_room) {
            Some(r) => r,
            None => return CmdOutput::text("\r\nYou are nowhere.\r\n".to_string()),
        };
        let hit = r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let proto = w.mob_protos.get(&m.vnum)?;
            if proto.name.split_whitespace().any(|n| n.eq_ignore_ascii_case(&key)) {
                Some((mid, proto.short_descr.clone()))
            } else { None }
        });
        match hit {
            Some(h) => h,
            None => return CmdOutput::text(
                "\r\nNo such creature here.\r\n".to_string()
            ),
        }
    };
    chars.lock().await.broadcast_room(
        me.current_room, Some(me.id),
        &format!(
            "A bolt of holy fury smites {mob_short} dead by {}'s will!\r\n",
            me.name,
        ),
    );
    // Route through the standard kill path so corpse/triggers/XP fire.
    crate::combat::kill_mob_immediate(
        mob_id, me.current_room, &mob_short, &me.name, world, chars,
    ).await;
    CmdOutput::text(format!("\r\nYou slay {mob_short}.\r\n"))
}

/// `snoop <player>` — silently tee an online player's output to your
/// own client.  Lines are prefixed `%name%` on the snooper's side.
/// Refuses to snoop another immortal of equal or higher level.  Each
/// snoop is one-way; do_snoop overwrites any previous target.
async fn do_snoop(
    arg: &str,
    me: &mut Character,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let target_name = arg.trim();
    if target_name.is_empty() {
        return CmdOutput::text(
            "\r\nUsage: snoop <player>  (use `unsnoop` to stop)\r\n".to_string()
        );
    }
    if target_name.eq_ignore_ascii_case(&me.name) {
        return CmdOutput::text("\r\nThat's a strange thing to ask.\r\n".to_string());
    }
    let target = {
        let cl = chars.lock().await;
        let h = cl.iter()
            .find(|p| p.name.eq_ignore_ascii_case(target_name))
            .cloned();
        h
    };
    let Some(tph) = target else {
        return CmdOutput::text(format!(
            "\r\nNo player named '{target_name}' is online.\r\n"
        ));
    };
    // Refuse snooping someone at or above your immortal level (anti-
    // peer-spying among the gods).
    {
        let c = tph.character.lock().await;
        if c.level >= me.level {
            return CmdOutput::text(
                "\r\nYou can't snoop someone at or above your own level.\r\n".to_string()
            );
        }
    }
    // Detach any previous target.
    if let Some(prev_tid) = me.snooping {
        let pph = {
            let cl = chars.lock().await;
            let h = cl.iter().find(|p| p.id == prev_tid).cloned();
            h
        };
        if let Some(pph) = pph {
            pph.character.lock().await.snooped_by.retain(|&i| i != me.id);
        }
    }
    me.snooping = Some(tph.id);
    {
        let mut c = tph.character.lock().await;
        if !c.snooped_by.contains(&me.id) {
            c.snooped_by.push(me.id);
        }
    }
    CmdOutput::text(format!(
        "\r\nYou are now snooping {}.\r\n", tph.name,
    ))
}

/// `unsnoop` — stop snooping whoever you're snooping.
async fn do_unsnoop(
    me: &mut Character,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let Some(tid) = me.snooping.take() else {
        return CmdOutput::text("\r\nYou aren't snooping anyone.\r\n".to_string());
    };
    let target_name = {
        let cl = chars.lock().await;
        let target = cl.iter().find(|p| p.id == tid).cloned();
        if let Some(tph) = target {
            tph.character.lock().await.snooped_by.retain(|&i| i != me.id);
            tph.name.clone()
        } else { String::from("them") }
    };
    CmdOutput::text(format!(
        "\r\nYou stop snooping {target_name}.\r\n"
    ))
}

/// `status` — immortal-only global overview: online players, mob/obj
/// counts, zone count, game-clock state, uptime.
async fn do_status(
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use std::sync::atomic::Ordering;
    if me.level < LVL_IMMORT { return immort_huh(); }
    let (mobs, objs, rooms, zones) = {
        let w = world.lock().await;
        (w.mob_instances.len(), w.obj_instances.len(),
         w.rooms.len(), w.zones.len())
    };
    let online = chars.lock().await.iter().count();
    let h = crate::db::GAME_HOUR.load(Ordering::Relaxed);
    let d = crate::db::GAME_DAY.load(Ordering::Relaxed);
    let mo = crate::db::GAME_MONTH.load(Ordering::Relaxed);
    let y = crate::db::GAME_YEAR.load(Ordering::Relaxed);
    let boot = crate::server::BOOT_UNIX_TS.load(Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64).unwrap_or(0);
    let uptime_secs = (now - boot).max(0);
    CmdOutput::text(format!(
        "\r\n=== Server status ===\r\n\
         \x20  online players: {online}\r\n\
         \x20  zones:          {zones}\r\n\
         \x20  rooms:          {rooms}\r\n\
         \x20  live mobs:      {mobs}\r\n\
         \x20  live objects:   {objs}\r\n\
         \x20  game-clock:     {h:02}:00 day {d} month {mo} year {y}\r\n\
         \x20  uptime:         {}h {}m {}s\r\n",
        uptime_secs / 3600,
        (uptime_secs / 60) % 60,
        uptime_secs % 60,
    ))
}

/// `pkilllog` (LVL_IMMORT+) — dump the last 20 entries of
/// `<data_dir>/log/pkill.log`.

/// `top [xp|level|pkills]` — leaderboard over online players.
/// Defaults to `xp` ranking.  Ties broken alphabetically by name.

/// `reload <player>` — re-read the named online player's PlayerRecord
/// from disk and overwrite their in-memory stats (gold/exp/level/
/// hp/mana/etc).  Equipment + inventory are NOT touched (still live
/// in the world model).  Used to revert in-session mutations.
async fn do_reload(
    arg: &str,
    me: &Character,
    chars: &SharedChars,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let name = arg.trim();
    if name.is_empty() {
        return CmdOutput::text(
            "\r\nUsage: reload <player>\r\n".to_string()
        );
    }
    let target = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.name.eq_ignore_ascii_case(name)).cloned();
        h
    };
    let Some(ph) = target else {
        return CmdOutput::text(format!(
            "\r\nNo player named '{name}' is online.\r\n"
        ));
    };
    let rec = {
        let pl = players.lock().await;
        pl.load_player(&ph.name)
    };
    let rec = match rec {
        Ok(r) => r,
        Err(e) => return CmdOutput::text(format!(
            "\r\nFailed to load {}: {e}\r\n", ph.name
        )),
    };
    {
        let mut c = ph.character.lock().await;
        c.level    = rec.level;
        c.gold     = rec.gold;
        c.exp      = rec.exp;
        c.hp       = rec.hp;     c.max_hp = rec.max_hp;
        c.mana     = rec.mana;   c.max_mana = rec.max_mana;
        c.movement = rec.movement; c.max_movement = rec.max_movement;
        c.alignment = rec.alignment;
        c.title    = rec.title.clone();
        c.god      = rec.god.clone();
        c.practices = rec.practices;
        c.bank_gold = rec.bank_gold;
        c.rent_per_day = rec.rent_per_day;
        c.hunger   = rec.hunger;
        c.thirst   = rec.thirst;
    }
    let _ = ph.send.send(format!(
        "\r\n{} has reverted your stats from disk.\r\n", me.name,
    ));
    CmdOutput::text(format!(
        "\r\nReloaded {}'s stats from disk.\r\n", ph.name,
    ))
}

/// `spec_assign <mob-vnum> <name|none>` (LVL_IMMORT+) — dynamically
/// override the `spec` field on every live mob of the given vnum.
/// Useful for hot-testing without recompiling the static
/// `MobSpec::for_vnum` table.
async fn do_spec_assign(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let mut parts = arg.split_whitespace();
    let vnum_s = parts.next().unwrap_or("");
    let name   = parts.next().unwrap_or("").to_ascii_lowercase();
    let Ok(vnum) = vnum_s.parse::<i32>() else {
        return CmdOutput::text(
            "\r\nUsage: spec_assign <mob-vnum> <puff|fido|janitor|cityguard|snake|magicuser|healer|postmaster|petshop|none>\r\n".to_string()
        );
    };
    let spec: Option<crate::world::MobSpec> = match name.as_str() {
        "puff"      => Some(crate::world::MobSpec::Puff),
        "fido"      => Some(crate::world::MobSpec::Fido),
        "janitor"   => Some(crate::world::MobSpec::Janitor),
        "cityguard" => Some(crate::world::MobSpec::Cityguard),
        "snake"     => Some(crate::world::MobSpec::Snake),
        "magicuser" | "mu" => Some(crate::world::MobSpec::MagicUser),
        "healer"    => Some(crate::world::MobSpec::Healer),
        "postmaster" | "post" => Some(crate::world::MobSpec::Postmaster),
        "petshop"   | "pet"   => Some(crate::world::MobSpec::PetShop),
        "thief"     => Some(crate::world::MobSpec::Thief),
        "none" | "off" | "clear" => None,
        ""          => return CmdOutput::text(
            "\r\nUsage: spec_assign <mob-vnum> <puff|fido|janitor|cityguard|snake|magicuser|healer|postmaster|petshop|thief|none>\r\n".to_string()
        ),
        _ => return CmdOutput::text(format!("\r\nUnknown spec '{name}'.\r\n")),
    };
    let touched = {
        let mut w = world.lock().await;
        let mut n = 0;
        for m in w.mob_instances.iter_mut().filter(|m| m.vnum == vnum) {
            m.spec = spec;
            n += 1;
        }
        n
    };
    if touched == 0 {
        return CmdOutput::text(format!("\r\nNo live mobs with vnum {vnum}.\r\n"));
    }
    let label = match spec {
        Some(s) => format!("{:?}", s),
        None    => "cleared".to_string(),
    };
    CmdOutput::text(format!(
        "\r\nAssigned spec={label} to {touched} live mob(s) of vnum {vnum}.\r\n"
    ))
}

/// Broadcast a wiznet line to every online immortal whose `wiznet_off`
/// is false.  `msg` should be the bare body — this helper wraps it in
/// the magenta `[wiznet] ...` envelope and CRLF.  Used by `do_wiznet`
/// and the login/logout hooks.
pub async fn broadcast_wiznet(msg: &str, chars: &SharedChars) {
    let line = format!("\r\n@m[wiznet] {msg}@n\r\n");
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    for ph in &handles {
        let (lvl, off) = {
            let c = ph.character.lock().await;
            (c.level, c.wiznet_off)
        };
        if lvl < LVL_IMMORT || off { continue; }
        let _ = ph.send.send(line.clone());
    }
}

/// `wiznet [msg]` — immortal-only broadcast channel.  Empty arg
/// toggles the sender's personal `wiznet_off` state.  No SOUNDPROOF
/// gate (this is a privileged channel that ignores room flags).
async fn do_wiznet(
    arg: &str,
    me: &mut Character,
    chars: &SharedChars,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let msg = arg.trim();
    if msg.is_empty() {
        me.wiznet_off = !me.wiznet_off;
        return CmdOutput::text(format!(
            "\r\nWiznet channel: {}.\r\n",
            if me.wiznet_off { "off" } else { "on" },
        ));
    }
    if me.wiznet_off {
        return CmdOutput::text(
            "\r\nYou have the wiznet channel turned off.\r\n".to_string()
        );
    }
    let body = format!("{}: {msg}", me.name);
    broadcast_wiznet(&body, chars).await;
    CmdOutput::text(format!("\r\n@m[wiznet] You: {msg}@n\r\n"))
}

async fn do_ban(
    arg: &str,
    me: &Character,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let site = arg.trim().to_lowercase();
    if site.is_empty() {
        return CmdOutput::text("\r\nUsage: ban <site>\r\n".to_string());
    }
    let Some(bs) = BAD_SITES.get() else {
        return CmdOutput::text("\r\nBan list unavailable.\r\n".to_string());
    };
    let snapshot = {
        let mut g = bs.lock().await;
        if g.iter().any(|s| s == &site) {
            return CmdOutput::text(format!(
                "\r\n'{site}' is already banned.\r\n"
            ));
        }
        g.push(site.clone());
        g.clone()
    };
    let data_dir = { players.lock().await.data_dir().to_string() };
    if let Err(e) = crate::players::save_badsites(&data_dir, &snapshot) {
        return CmdOutput::text(format!(
            "\r\nBanned '{site}' in memory but failed to persist: {e}\r\n"
        ));
    }
    tracing::info!(banner = %me.name, site = %site, "Site banned");
    CmdOutput::text(format!("\r\n'{site}' is now banned.\r\n"))
}

async fn do_unban(
    arg: &str,
    me: &Character,
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let site = arg.trim().to_lowercase();
    if site.is_empty() {
        return CmdOutput::text("\r\nUsage: unban <site>\r\n".to_string());
    }
    let Some(bs) = BAD_SITES.get() else {
        return CmdOutput::text("\r\nBan list unavailable.\r\n".to_string());
    };
    let snapshot_opt = {
        let mut g = bs.lock().await;
        let before = g.len();
        g.retain(|s| s != &site);
        if g.len() == before { None } else { Some(g.clone()) }
    };
    let Some(snapshot) = snapshot_opt else {
        return CmdOutput::text(format!(
            "\r\n'{site}' is not in the ban list.\r\n"
        ));
    };
    let data_dir = { players.lock().await.data_dir().to_string() };
    if let Err(e) = crate::players::save_badsites(&data_dir, &snapshot) {
        return CmdOutput::text(format!(
            "\r\nUnbanned '{site}' in memory but failed to persist: {e}\r\n"
        ));
    }
    tracing::info!(banner = %me.name, site = %site, "Site unbanned");
    CmdOutput::text(format!("\r\n'{site}' has been unbanned.\r\n"))
}

async fn do_bans(me: &Character) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let Some(bs) = BAD_SITES.get() else {
        return CmdOutput::text("\r\nBan list unavailable.\r\n".to_string());
    };
    let entries: Vec<String> = { bs.lock().await.clone() };
    if entries.is_empty() {
        return CmdOutput::text("\r\nNo sites are banned.\r\n".to_string());
    }
    let mut s = String::from("\r\nBanned sites:\r\n");
    for (i, e) in entries.iter().enumerate() {
        s.push_str(&format!("  {:>3}. {}\r\n", i + 1, e));
    }
    CmdOutput::text(s)
}

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
    players: &Arc<Mutex<PlayerDb>>,
) -> CmdOutput {
    if me.level < LVL_IMMORT { return immort_huh(); }
    let arg = arg.trim();

    // `stat zone <vnum>` — summary of a zone's mobs/rooms/objects.
    if let Some(rest) = arg.strip_prefix("zone ").or_else(|| arg.strip_prefix("zone\t")) {
        let Ok(zv) = rest.trim().parse::<i32>() else {
            return CmdOutput::text("\r\nUsage: stat zone <vnum>\r\n".to_string());
        };
        let w = world.lock().await;
        let Some(z) = w.zones.get(&zv) else {
            return CmdOutput::text(format!("\r\nNo zone with vnum {zv}.\r\n"));
        };
        let rooms_in_zone: Vec<i32> = w.rooms.iter()
            .filter(|(_, r)| r.zone == zv).map(|(v, _)| *v).collect();
        let mob_count = w.mob_instances.iter()
            .filter(|m| rooms_in_zone.contains(&m.in_room)).count();
        let obj_count = w.obj_instances.iter()
            .filter(|o| rooms_in_zone.contains(&o.in_room)).count();
        return CmdOutput::text(format!(
            "\r\nZone [{}] {}\r\n  Bottom:   {}\r\n  Top:      {}\r\n  Lifespan: {} minutes\r\n  Reset mode: {}\r\n  Rooms in zone: {}\r\n  Live mobs:     {}\r\n  Live objects:  {}\r\n",
            zv, z.name, z.bot, z.top, z.lifespan, z.reset_mode,
            rooms_in_zone.len(), mob_count, obj_count,
        ));
    }

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

    // Numeric vnum lookup against prototypes: room → obj → mob.
    if let Ok(vn) = arg.parse::<i32>() {
        let w = world.lock().await;
        if let Some(r) = w.rooms.get(&vn) {
            let mut s = format!(
                "\r\nRoom proto [{}] (zone {})\r\n  Name:     {}\r\n  Flags:    0x{:x}\r\n  Sector:   {}\r\n",
                r.vnum, r.zone, r.name, r.room_flags[0], r.sector_type,
            );
            let mut exits = Vec::new();
            for d in crate::world::Direction::ALL {
                if let Some(e) = &r.exits[d as usize] {
                    if e.to_room == crate::world::NOWHERE { continue; }
                    exits.push(format!("{}→{}", d.name(), e.to_room));
                }
            }
            s.push_str(&format!("  Exits:    {}\r\n",
                if exits.is_empty() { "(none)".to_string() } else { exits.join(", ") }));
            return CmdOutput::text(s);
        }
        if let Some(p) = w.obj_protos.get(&vn) {
            return CmdOutput::text(format!(
                "\r\nObject proto [{}]\r\n  Short:    {}\r\n  Type:     {}\r\n  Keys:     {}\r\n  Value:    {} {} {} {}\r\n  Wear:     0x{:x}\r\n  Extra:    0x{:x}\r\n  Weight:   {}\r\n  Cost:     {}\r\n  Level:    {}\r\n  Timer:    {}\r\n  Affects:  {:?}\r\n",
                p.vnum, p.short_description, item_type_name(p.item_type),
                p.name, p.value[0], p.value[1], p.value[2], p.value[3],
                p.wear_flags[0], p.extra_flags[0],
                p.weight, p.cost, p.level, p.timer, p.affected,
            ));
        }
        if let Some(p) = w.mob_protos.get(&vn) {
            return CmdOutput::text(format!(
                "\r\nMob proto [{}]\r\n  Short:    {}\r\n  Keys:     {}\r\n  Level:    {}\r\n  HP dice:  {}d{}+{}\r\n  Dam dice: {}d{}+{}\r\n  AC:       {}\r\n  Hitroll:  {}\r\n  Gold:     {}\r\n  Exp:      {}\r\n  Flags:    0x{:x}\r\n  Affs:     0x{:x}\r\n",
                p.vnum, p.short_descr, p.name, p.level,
                p.hp_dice, p.hp_size, p.hp_add,
                p.dam_dice, p.dam_size, p.damroll,
                p.ac, p.hitroll, p.gold, p.exp,
                p.mob_flags[0], p.aff_flags[0],
            ));
        }
    }

    // Last resort: try the player index for an offline character.
    let offline_rec = {
        let db = players.lock().await;
        if let Some(canon) = db.find_name(arg) {
            db.load_player(&canon).ok()
        } else {
            None
        }
    };
    if let Some(r) = offline_rec {
        let s = format!(
            "\r\n[Offline] Player {}  (level {})\r\n  Class:    {:?}\r\n  HP:       {}/{}\r\n  Mana:     {}/{}\r\n  Exp:      {}\r\n  Gold:     {} (bank {})\r\n  Room:     {}\r\n  Hunger/Thirst: {}/{}\r\n  Active quest: {:?}  (progress {})\r\n  Completed: {}\r\n  Title:    {}\r\n",
            r.name, r.level, r.class, r.hp, r.max_hp, r.mana, r.max_mana,
            r.exp, r.gold, r.bank_gold, r.room,
            r.hunger, r.thirst, r.active_quest, r.quest_progress,
            r.completed_quests.len(),
            if r.title.is_empty() { "(none)".to_string() } else { r.title.clone() },
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

/// `describe <text>` (cp232): set the physical description others see when
/// they `look` at you.  `describe -` (or empty) clears it.  Control bytes
/// stripped, capped at 240 chars.

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

async fn do_who(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    // Parse filters: bare word = name substring; `-c <class>` = class;
    // `-l <lo>-<hi>` = level range.  Filters compose (all must match).
    let mut name_substr: Option<String> = None;
    let mut class_filter: Option<crate::players::Class> = None;
    let mut level_range: Option<(i32, i32)> = None;
    let mut afk_only: bool = false;
    let mut clan_filter: Option<String> = None;
    let toks: Vec<&str> = arg.split_whitespace().collect();
    let mut i = 0;
    while i < toks.len() {
        match toks[i] {
            "-afk" => { afk_only = true; i += 1; continue; }
            "-clan" => {
                if let Some(v) = toks.get(i + 1) {
                    clan_filter = Some(v.to_string());
                    i += 2; continue;
                }
            }
            "-c" => {
                if let Some(v) = toks.get(i + 1) {
                    class_filter = match v.to_ascii_lowercase().as_str() {
                        "warrior"   => Some(crate::players::Class::Warrior),
                        "thief"     => Some(crate::players::Class::Thief),
                        "cleric"    => Some(crate::players::Class::Cleric),
                        "magicuser" | "magic-user" | "mu" => Some(crate::players::Class::MagicUser),
                        _           => None,
                    };
                    i += 2; continue;
                }
            }
            "-l" => {
                if let Some(v) = toks.get(i + 1) {
                    let (lo, hi) = match v.split_once('-') {
                        Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(34)),
                        None         => {
                            let n = v.parse().unwrap_or(0);
                            (n, n)
                        }
                    };
                    level_range = Some((lo, hi));
                    i += 2; continue;
                }
            }
            other => {
                name_substr = Some(other.to_ascii_lowercase());
            }
        }
        i += 1;
    }

    // Snapshot titles + classes outside the registry lock to avoid
    // serializing on contended Character mutexes.
    let extras: Vec<(u32, String, crate::players::Class, i32, bool, String)> = {
        let cl = chars.lock().await;
        let handles: Vec<_> = cl.iter().cloned().collect();
        drop(cl);
        let mut out = Vec::new();
        for ph in handles {
            let c = ph.character.lock().await;
            out.push((ph.id, c.title.clone(), c.class, c.invis_level, c.afk_msg.is_some(), c.clan.clone()));
        }
        out
    };

    let cl = chars.lock().await;
    let mut s = String::from("\r\nPlayers online:\r\n");
    let mut count = 0;
    for p in cl.iter() {
        let (_, title, class, invis_lvl, afk, clan) = extras.iter().find(|(id, _, _, _, _, _)| *id == p.id)
            .cloned()
            .unwrap_or((p.id, String::new(), crate::players::Class::Undefined, 0, false, String::new()));
        // Hide immortal-invis players from anyone below their threshold.
        if p.id != me.id && invis_lvl > me.level { continue; }
        if let Some(sub) = &name_substr {
            if !p.name.to_ascii_lowercase().contains(sub) { continue; }
        }
        if let Some(cf) = class_filter {
            if class != cf { continue; }
        }
        if let Some((lo, hi)) = level_range {
            if p.level < lo || p.level > hi { continue; }
        }
        if afk_only && !afk { continue; }
        if let Some(cf) = &clan_filter {
            if !clan.eq_ignore_ascii_case(cf) { continue; }
        }
        let marker = if p.id == me.id { " (you)" } else { "" };
        let afk_tag = if afk { " [AFK]" } else { "" };
        let title_str = if title.is_empty() { String::new() } else { format!(" {title}") };
        s.push_str(&format!("  [{:>2} {:>3}] {}{}{}{}\r\n",
            p.level, class.as_str(), p.name, title_str, afk_tag, marker));
        count += 1;
    }
    s.push_str(&format!("\r\n{count} player(s) shown.\r\n"));
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
    let god_line = if me.god.is_empty() { String::new() }
                   else { format!("God:   {}\r\n", me.god) };
    let clan_line = if me.clan.is_empty() { String::new() }
                    else { format!("Clan:  {}\r\n", me.clan) };
    let pvp_line = if me.pkills + me.pdeaths > 0 {
        format!("PvP:   {} kills / {} deaths\r\n", me.pkills, me.pdeaths)
    } else { String::new() };
    let align_band = crate::character::AlignmentBand::of(me.alignment);
    let drunk_line = if me.drunk >= MAX_DRUNK { "Drunk: completely smashed\r\n".to_string() }
                     else if me.drunk >= DRUNK_SLUR_THRESHOLD { format!("Drunk: drunk ({})\r\n", me.drunk) }
                     else if me.drunk > 0 { format!("Drunk: tipsy ({})\r\n", me.drunk) }
                     else { String::new() };
    let s = format!(
        "\r\n{name_line}\r\nLevel: {}\r\nExp:   {exp_str}\r\nHP:    {}/{}\r\nMana:  {}/{}\r\nMove:  {}/{}\r\nClass: {:?}\r\nSex:   {:?}\r\nGold:  {}\r\nRoom:  {}\r\nAC:    {}\r\nPrac:  {}\r\nFood:  {food}\r\nDrink: {drink}\r\n{drunk_line}Align: {} ({})\r\n{god_line}{clan_line}{pvp_line}\
         Str/Int/Wis/Dex/Con/Cha: {}/{}/{}/{}/{}/{}\r\n",
        me.level, me.hp, me.max_hp, me.mana, me.max_mana,
        me.movement, me.max_movement,
        me.class, me.sex, me.gold, me.current_room, ac, me.practices,
        me.alignment, align_band.name(),
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

/// `spells` — like `skills`, but filtered to entries whose `kind()` is
/// SkillKind::Spell.  Shows learned% and mana cost.
fn do_spells(me: &Character) -> CmdOutput {
    use crate::character::{ALL_SKILLS, SkillKind};
    let mut s = String::from("\r\nSpells available to your class:\r\n");
    let mut any = false;
    for &skill in ALL_SKILLS {
        if skill.kind() != SkillKind::Spell { continue; }
        if !skill.is_class_allowed(me.class) { continue; }
        any = true;
        let pct = *me.skills.get(&skill).unwrap_or(&0);
        s.push_str(&format!(
            "  {:<16} {:>3}%  ({} mana)\r\n",
            skill.name(), pct, skill.mana_cost(),
        ));
    }
    if !any {
        s.push_str("  (none — your class casts no spells)\r\n");
    }
    CmdOutput::text(s)
}

/// Map a skill percentage to a flavor label.
fn skill_tier(pct: u8) -> &'static str {
    match pct {
        0          => "untrained",
        1..=25     => "unfamiliar",
        26..=50    => "novice",
        51..=75    => "skilled",
        76..=95    => "expert",
        _          => "master",
    }
}

fn do_skills(me: &Character) -> CmdOutput {
    use crate::character::ALL_SKILLS;
    let mut s = String::from("\r\nSkills available to your class:\r\n");
    let mut any = false;
    for &skill in ALL_SKILLS {
        if !skill.is_class_allowed(me.class) { continue; }
        any = true;
        let pct = *me.skills.get(&skill).unwrap_or(&0);
        s.push_str(&format!(
            "  {:<14} {:>3}%   ({})\r\n",
            skill.name(), pct, skill_tier(pct),
        ));
    }
    if !any {
        s.push_str("  (none — your class has no learnable skills)\r\n");
    }
    CmdOutput::text(s)
}

fn do_practice(arg: &str, me: &mut Character) -> CmdOutput {
    // `practice all` — spend remaining practice points one at a time
    // across every class-allowed skill in round-robin, until either
    // budget is exhausted or no skill is below 90% cap.
    if arg.trim().eq_ignore_ascii_case("all") {
        if !is_guild_room_for(me.current_room, me.class) {
            return CmdOutput::text(format!(
                "\r\nYou must visit a {:?} guild to practice your art.\r\n", me.class,
            ));
        }
        if me.practices <= 0 {
            return CmdOutput::text(
                "\r\nYou have no practice points to spend.\r\n".to_string()
            );
        }
        let mut spent = 0i32;
        loop {
            if me.practices <= 0 { break; }
            let mut any_eligible = false;
            for &skill in crate::character::ALL_SKILLS {
                if me.practices <= 0 { break; }
                if !skill.is_class_allowed(me.class) { continue; }
                let pct = me.skills.entry(skill).or_insert(0);
                if *pct >= 90 { continue; }
                *pct = (*pct + 10).min(90);
                me.practices -= 1;
                spent += 1;
                any_eligible = true;
            }
            if !any_eligible { break; }
        }
        return CmdOutput::text(format!(
            "\r\nYou practice diligently: spent {spent} point(s); {} left.\r\n",
            me.practices,
        ));
    }
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
    let tier = skill_tier(*pct);
    CmdOutput::text(format!(
        "\r\nYou practice {} a bit. ({}%, {tier}, {} practice(s) left)\r\n",
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
        if a.to_ac  != 0 { parts.push(format!("AC {:+}",  a.to_ac));  }
        if a.dmg_reduction != 0 { parts.push(format!("dmg-reduction {}%", a.dmg_reduction)); }
        if a.dot_damage    != 0 { parts.push(format!("dot {}/tick",       a.dot_damage));    }
        let mods = if parts.is_empty() { "—".to_string() } else { parts.join(", ") };
        s.push_str(&format!(
            "  {:<14} {:<35} ({} ticks left)\r\n",
            a.name(), mods, a.duration,
        ));
    }
    // Totals footer for quick triage.
    let t_hit  = me.affect_hit_bonus();
    let t_dam  = me.affect_dam_bonus();
    let t_ac   = me.affect_ac_bonus();
    let t_red  = me.affect_dmg_reduction();
    s.push_str(&format!(
        "Totals: hit {:+}, dam {:+}, AC {:+}, dmg-red {}%\r\n",
        t_hit, t_dam, t_ac, t_red,
    ));
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
    let mut s = format!(
        "\r\nLevel {}: {} experience, {} until next level.\r\n\r\n",
        me.level, me.exp, (next - me.exp).max(0),
    );
    s.push_str("Coming up:\r\n");
    for offset in 0..5 {
        let target = me.level + offset;
        if target >= Character::MAX_MORTAL_LEVEL { break; }
        let needed = Character::exp_for_level(target);
        if needed == i64::MAX { break; }
        s.push_str(&format!(
            "  -> level {:>2}: {} xp\r\n", target + 1, needed,
        ));
    }
    CmdOutput::text(s)
}

/// `levels [<min>-<max> | <range>]` — show the XP required for each level
/// band plus the class title at that level.  Mirrors stock `do_levels`.
fn do_levels(arg: &str, me: &Character) -> CmdOutput {
    let max_mortal = Character::MAX_MORTAL_LEVEL;
    let arg = arg.trim();
    let (mut lo, mut hi) = (1, max_mortal);
    if !arg.is_empty() {
        if let Some((a, b)) = arg.split_once('-') {
            lo = a.trim().parse().unwrap_or(1).max(1);
            hi = b.trim().parse().unwrap_or(max_mortal).min(max_mortal);
        } else if let Ok(range) = arg.parse::<i32>() {
            lo = (me.level - range).max(1);
            hi = (me.level + range).min(max_mortal);
        } else {
            return CmdOutput::text(
                "\r\nUsage: levels [<min>-<max> | <range>]\r\n\
                 Displays exp required for levels.\r\n".to_string());
        }
    }
    if lo > hi { std::mem::swap(&mut lo, &mut hi); }
    let mut s = format!("\r\nExperience table for {}:\r\n", me.class.as_str());
    for lvl in lo..hi {
        let this = Character::exp_for_level(lvl - 1).max(0);
        let next = Character::exp_for_level(lvl);
        let next = if next == i64::MAX { 0 } else { next - 1 };
        let _ = this;
        let start = if lvl <= 1 { 0 } else { Character::exp_for_level(lvl - 1) };
        let start = if start == i64::MAX { 0 } else { start };
        s.push_str(&format!(
            "[{:>2}] {:>9}-{:<9} : {}\r\n",
            lvl, start, next,
            Character::default_title_for(me.class, lvl),
        ));
    }
    CmdOutput::text(s)
}

/// `areas` — list every zone (area) and its builder-declared level range.
/// Mirrors stock `do_areas`.
async fn do_areas(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let w = world.lock().await;
    let mut zones: Vec<&crate::world::Zone> = w.zones.values().collect();
    zones.sort_by_key(|z| z.number);
    let mut s = String::from("\r\nAreas of the world:\r\n");
    let mut shown = 0;
    for z in zones {
        // Skip empty/placeholder zone names.
        if z.name.trim().is_empty() || z.name == "New Zone" { continue; }
        let lev = if z.min_level > 0 || z.max_level > 0 {
            format!(" (lvl {}-{})", z.min_level, z.max_level)
        } else {
            String::new()
        };
        s.push_str(&format!("  {:<40}{}\r\n", z.name, lev));
        shown += 1;
        if shown >= 200 { s.push_str("  ...(truncated)\r\n"); break; }
    }
    s.push_str(&format!("\r\n{shown} area(s).\r\n"));
    let _ = me;
    CmdOutput::text(s)
}

/// Display a flat text file under `lib/text/<name>` to the player.
/// Used by news / credits / motd / imotd / policy / handbook / wizlist /
/// immlist / background — mirrors stock's text-file commands.
async fn do_text_file(name: &str) -> CmdOutput {
    let data_dir = match crate::interpreter::PLAYERS_HANDLE.get() {
        Some(p) => p.lock().await.data_dir().to_string(),
        None    => "lib".to_string(),
    };
    let path = format!("{data_dir}/text/{name}");
    match std::fs::read_to_string(&path) {
        Ok(body) => CmdOutput::text(format!("\r\n{}\r\n", body.trim_end())),
        Err(_)   => CmdOutput::text(format!("\r\nThere is no {name} to display.\r\n")),
    }
}

/// `diagnose [target]` — report the health condition of a mob in the room
/// (or the caller's own if no arg).  Mirrors stock `do_diagnose`.
async fn do_diagnose(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let key = arg.trim().to_ascii_lowercase();
    // Health-percentage → description, mirroring stock's diag table.
    fn diag(hp: i32, max_hp: i32, who: &str) -> String {
        let pct = if max_hp > 0 { hp * 100 / max_hp } else { 0 };
        let cond = match pct {
            p if p >= 100 => "is in excellent condition",
            p if p >= 90  => "has a few scratches",
            p if p >= 75  => "has some small wounds and bruises",
            p if p >= 50  => "has quite a few wounds",
            p if p >= 30  => "has some big nasty wounds and scratches",
            p if p >= 15  => "looks pretty hurt",
            p if p >= 0   => "is in awful condition",
            _             => "is bleeding awfully from big wounds",
        };
        format!("{who} {cond}.\r\n")
    }
    if key.is_empty() {
        return CmdOutput::text(format!("\r\n{}", diag(me.hp, me.max_hp, "You")));
    }
    // Try a player in the room first, then a mob.
    let player_hit = {
        let cl = chars.lock().await;
        let mut hit = None;
        for ph in cl.iter() {
            if ph.id != me.id && ph.current_room == me.current_room
                && ph.name.to_ascii_lowercase() == key {
                let c = ph.character.lock().await;
                hit = Some(diag(c.hp, c.max_hp, &ph.name));
                break;
            }
        }
        hit
    };
    if let Some(line) = player_hit {
        return CmdOutput::text(format!("\r\n{line}"));
    }
    let mob_hit = {
        let w = world.lock().await;
        w.rooms.get(&me.current_room).and_then(|r| r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|n| n.eq_ignore_ascii_case(&key)) {
                Some(diag(m.hp, m.max_hp, &p.short_descr))
            } else { None }
        }))
    };
    match mob_hit {
        Some(line) => CmdOutput::text(format!("\r\n{line}")),
        None => CmdOutput::text(format!("\r\nYou see no {key} here.\r\n")),
    }
}

// ---------------------------------------------------------------------------
// Bulletin boards (port of stock boards.c)
// ---------------------------------------------------------------------------

/// Return the board descriptor for any board object present in the
/// caller's current room (or carried, for immortals).
async fn find_board_in_room(
    me: &Character, world: &Arc<Mutex<World>>,
) -> Option<&'static crate::boards::BoardDef> {
    let w = world.lock().await;
    // Floor objects first.
    if let Some(r) = w.rooms.get(&me.current_room) {
        for &iid in &r.objects {
            if let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) {
                if let Some(def) = crate::boards::board_for_vnum(o.vnum) {
                    return Some(def);
                }
            }
        }
    }
    // Immortals can use a board they carry.
    if me.level >= LVL_IMMORT {
        for &iid in &me.inventory {
            if let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) {
                if let Some(def) = crate::boards::board_for_vnum(o.vnum) {
                    return Some(def);
                }
            }
        }
    }
    None
}

async fn board_data_dir() -> String {
    match crate::interpreter::PLAYERS_HANDLE.get() {
        Some(p) => p.lock().await.data_dir().to_string(),
        None    => "lib".to_string(),
    }
}

/// `board` / `boards` — show the message list on a board in the room.
async fn do_board_show(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let Some(def) = find_board_in_room(me, world).await else {
        return CmdOutput::text("\r\nThere is no board here.\r\n".to_string());
    };
    if me.level < def.read_lvl {
        return CmdOutput::text("\r\nYou aren't able to read this board.\r\n".to_string());
    }
    let data_dir = board_data_dir().await;
    let msgs = crate::boards::load_board(&data_dir, def);
    if msgs.is_empty() {
        return CmdOutput::text("\r\nThis board is empty.\r\n".to_string());
    }
    let mut s = format!("\r\nThis is a bulletin board.  There are {} messages on it.\r\n\
        Use `read <num>` to read a message, `write <header>` to post, \
        `remove <num>` to delete one.\r\n", msgs.len());
    for (i, m) in msgs.iter().enumerate() {
        s.push_str(&format!("{:<2} : {} :: {}\r\n", i + 1, m.author, m.header));
    }
    CmdOutput::text(s)
}

/// `write <header>` — post a message to a board in the room.  Since we
/// have no full-screen editor, the typed text becomes both the header
/// (truncated for the listing) and the message body.
async fn do_board_write(arg: &str, me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let Some(def) = find_board_in_room(me, world).await else {
        return CmdOutput::text("\r\nThere is no board here to write on.\r\n".to_string());
    };
    if me.level < def.write_lvl {
        return CmdOutput::text("\r\nYou aren't able to write on this board.\r\n".to_string());
    }
    let text = arg.trim();
    if text.is_empty() {
        return CmdOutput::text("\r\nWrite what?  Usage: write <message>\r\n".to_string());
    }
    let header: String = text.chars().take(40).collect();
    let msg = crate::boards::BoardMessage {
        author: me.name.clone(),
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64).unwrap_or(0),
        header,
        body: text.to_string(),
    };
    let data_dir = board_data_dir().await;
    match crate::boards::append_message(&data_dir, def, &msg) {
        Ok(()) => CmdOutput::text("\r\nYou post your message to the board.\r\n".to_string()),
        Err(_) => CmdOutput::text("\r\nThe board refuses your message.\r\n".to_string()),
    }
}

/// `read <N>` — display message N from a board in the room.
async fn do_board_read(arg: &str, me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let Some(def) = find_board_in_room(me, world).await else {
        return CmdOutput::text("\r\nThere is nothing here to read.\r\n".to_string());
    };
    if me.level < def.read_lvl {
        return CmdOutput::text("\r\nYou aren't able to read this board.\r\n".to_string());
    }
    let Some(n) = arg.trim().parse::<usize>().ok().filter(|&n| n >= 1) else {
        return CmdOutput::text("\r\nRead which message?  Usage: read <num>\r\n".to_string());
    };
    let data_dir = board_data_dir().await;
    let msgs = crate::boards::load_board(&data_dir, def);
    let Some(m) = msgs.get(n - 1) else {
        return CmdOutput::text(format!("\r\nThere is no message {n} on the board.\r\n"));
    };
    CmdOutput::text(format!(
        "\r\nMessage {n} : posted by {}\r\n{}\r\n\r\n{}\r\n",
        m.author, m.header, m.body))
}

/// `remove <N>` (board context) — delete message N.  Mortals may only
/// remove their own posts; immortals at the board's remove level may
/// remove any.  Mirrors stock `board_remove_msg`.
async fn do_board_remove(arg: &str, me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let Some(def) = find_board_in_room(me, world).await else {
        return CmdOutput::text("\r\nThere is no board here.\r\n".to_string());
    };
    let Some(n) = arg.trim().parse::<usize>().ok().filter(|&n| n >= 1) else {
        return CmdOutput::text("\r\nRemove which message?  Usage: remove <num>\r\n".to_string());
    };
    let data_dir = board_data_dir().await;
    let mut msgs = crate::boards::load_board(&data_dir, def);
    let Some(m) = msgs.get(n - 1) else {
        return CmdOutput::text(format!("\r\nThere is no message {n} on the board.\r\n"));
    };
    let is_owner = m.author.eq_ignore_ascii_case(&me.name);
    if !is_owner && me.level < def.remove_lvl {
        return CmdOutput::text("\r\nYou are not holy enough to remove other people's messages.\r\n".to_string());
    }
    msgs.remove(n - 1);
    match crate::boards::save_board(&data_dir, def, &msgs) {
        Ok(()) => CmdOutput::text(format!("\r\nYou remove message {n} from the board.\r\n")),
        Err(_) => CmdOutput::text("\r\nThe board won't let go of that message.\r\n".to_string()),
    }
}

/// Sum of weights of every object the player is carrying (inventory +
/// equipment).  Container contents count toward the carrier's weight.
// ---------------------------------------------------------------------------
// Rent / inn receptionist (port of stock objsave.c gen_receptionist)
// ---------------------------------------------------------------------------

/// Find a receptionist or cryogenicist mob in the caller's room.  Returns
/// `(mob_short, factor)` — RENT_FACTOR for a receptionist, CRYO_FACTOR for
/// a cryogenicist.
async fn find_receptionist(me: &Character, world: &Arc<Mutex<World>>) -> Option<(String, i32, bool)> {
    let w = world.lock().await;
    let r = w.rooms.get(&me.current_room)?;
    for &mid in &r.mobs {
        let Some(m) = w.mob_instances.iter().find(|m| m.id == mid) else { continue };
        let factor = match m.spec {
            Some(crate::world::MobSpec::Receptionist) => Some((crate::config::RENT_FACTOR, false)),
            Some(crate::world::MobSpec::Cryogenicist) => Some((crate::config::CRYO_FACTOR, true)),
            _ => None,
        };
        if let Some((factor, cryo)) = factor {
            let short = w.mob_protos.get(&m.vnum)
                .map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "the receptionist".into());
            return Some((short, factor, cryo));
        }
    }
    None
}

struct RentItem { short: String, cost: i32 }
struct RentBreak {
    unrentables: Vec<String>,
    items: Vec<RentItem>,
    total: i32,
}

/// Walk the caller's carried + equipped objects (recursing into container
/// contents) and tally rent.  Mirrors Crash_report_rent/unrentables.
fn rent_breakdown(me: &Character, w: &World, factor: i32) -> RentBreak {
    let mut stack: Vec<u32> = me.inventory.clone();
    stack.extend(me.equipment.iter().flatten().copied());
    let mut br = RentBreak {
        unrentables: Vec::new(),
        items: Vec::new(),
        total: crate::config::MIN_RENT_COST * factor,
    };
    while let Some(iid) = stack.pop() {
        let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else { continue };
        stack.extend(o.contents.iter().copied());
        let Some(p) = w.obj_protos.get(&o.vnum) else { continue };
        let unrentable = p.extra_flags[0] & crate::world::ITEM_NORENT != 0
            || p.rent < 0
            || p.item_type == crate::world::ITEM_KEY;
        if unrentable {
            br.unrentables.push(p.short_description.clone());
        } else {
            let c = (p.rent * factor).max(0);
            br.total += c;
            br.items.push(RentItem { short: p.short_description.clone(), cost: c });
        }
    }
    br
}

/// Shared body of `offer` (display=true) and `rent` (display=false).
/// Returns the per-day/total cost when the player CAN rent, else None
/// (after emitting the appropriate refusal lines into `out`).
fn rent_offer(
    me: &Character, br: &RentBreak, short: &str, factor: i32,
    display: bool, out: &mut String,
) -> Option<i32> {
    if !br.unrentables.is_empty() {
        for u in &br.unrentables {
            out.push_str(&format!("{short} tells you, 'You cannot store {u}.'\r\n"));
        }
        return None;
    }
    if br.items.is_empty() {
        out.push_str(&format!(
            "{short} tells you, 'But you are not carrying anything!  Just quit!'\r\n"));
        return None;
    }
    if br.items.len() as i32 > crate::config::MAX_OBJ_SAVE {
        out.push_str(&format!(
            "{short} tells you, 'Sorry, but I cannot store more than {} items.'\r\n",
            crate::config::MAX_OBJ_SAVE));
        return None;
    }
    if display {
        for it in &br.items {
            out.push_str(&format!("{short} tells you, '{:5} coins for {}..'\r\n", it.cost, it.short));
        }
        out.push_str(&format!("{short} tells you, 'Plus, my {} coin fee..'\r\n",
            crate::config::MIN_RENT_COST * factor));
        out.push_str(&format!("{short} tells you, 'For a total of {} coins{}.'\r\n",
            br.total, if factor == crate::config::RENT_FACTOR { " per day" } else { "" }));
    }
    Some(br.total)
}

/// `offer` — ask a receptionist what renting would cost.
async fn do_offer(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let Some((short, factor, _cryo)) = find_receptionist(me, world).await else {
        return CmdOutput::text("\r\nSorry, but you cannot do that here!\r\n".to_string());
    };
    if crate::interpreter::FREE_RENT.load(std::sync::atomic::Ordering::Relaxed) {
        return CmdOutput::text(format!(
            "\r\n{short} tells you, 'Rent is free here.  Just quit, and your objects will be saved!'\r\n"));
    }
    let br = { let w = world.lock().await; rent_breakdown(me, &w, factor) };
    let mut out = String::from("\r\n");
    if let Some(total) = rent_offer(me, &br, &short, factor, true, &mut out) {
        let purse = me.gold + me.bank_gold;
        if total as i64 > purse {
            out.push_str(&format!("{short} tells you, '...which I see you can't afford.'\r\n"));
        } else if factor == crate::config::RENT_FACTOR {
            let days = purse / total as i64;
            out.push_str(&format!(
                "{short} tells you, 'You can rent for {days} day{} with the gold you have on hand and in the bank.'\r\n",
                if days != 1 { "s" } else { "" }));
        }
    }
    CmdOutput::text(out)
}

/// `rent` — store belongings and log off (charging per-day rent unless
/// rent is free, in which case it just tells the player to quit).
async fn do_rent(me: &mut Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let Some((short, factor, cryo)) = find_receptionist(me, world).await else {
        return CmdOutput::text("\r\nSorry, but you cannot do that here!\r\n".to_string());
    };
    if crate::interpreter::FREE_RENT.load(std::sync::atomic::Ordering::Relaxed) {
        return CmdOutput::text(format!(
            "\r\n{short} tells you, 'Rent is free here.  Just quit, and your objects will be saved!'\r\n"));
    }
    let br = { let w = world.lock().await; rent_breakdown(me, &w, factor) };
    let mut out = String::from("\r\n");
    let Some(total) = rent_offer(me, &br, &short, factor, false, &mut out) else {
        return CmdOutput::text(out);
    };
    if cryo {
        out.push_str(&format!("{short} tells you, 'It will cost you {total} gold coins to be frozen.'\r\n"));
    } else {
        out.push_str(&format!("{short} tells you, 'Rent will cost you {total} gold coins per day.'\r\n"));
    }
    if total as i64 > me.gold + me.bank_gold {
        out.push_str(&format!("{short} tells you, '...which I see you can't afford.'\r\n"));
        return CmdOutput::text(out);
    }
    // Record the per-day cost; accrued rent is charged on next login.
    me.rent_per_day = total;
    if cryo {
        out.push_str(&format!(
            "{short} stores your belongings and helps you into your private chamber.\r\n\
             A white mist appears in the room, chilling you to the bone...\r\n\
             You begin to lose consciousness...\r\n"));
    } else {
        out.push_str(&format!(
            "{short} stores your belongings and helps you into your private chamber.\r\n"));
    }
    // Route through the standard quit path (saves objects + disconnects).
    CmdOutput::quit(out)
}

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
    match me.position {
        crate::character::Position::Standing => {}
        crate::character::Position::Sleeping =>
            return CmdOutput::text("\r\nYou can't attack while sleeping!\r\n".to_string()),
        _ =>
            return CmdOutput::text("\r\nYou need to stand up to attack.\r\n".to_string()),
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
    drop(w);

    // PvP: if the target name matches another online player in the same
    // room, route through the player-target path.  Both parties must
    // have `pvp_ok` set.
    let pvp_target: Option<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p|
            p.id != me.id
            && p.current_room == me.current_room
            && p.name.eq_ignore_ascii_case(&key)).cloned();
        h
    };
    if let Some(ph) = pvp_target {
        if !me.pvp_ok {
            return CmdOutput::text(
                "\r\nYou need to enable PvP first (type `pvp`).\r\n".to_string()
            );
        }
        let target_pvp = ph.character.lock().await.pvp_ok;
        if !target_pvp {
            return CmdOutput::text(format!(
                "\r\n{} hasn't consented to PvP.\r\n", ph.name,
            ));
        }
        me.fighting = Some(Target { id: ph.id, is_player: true });
        {
            let mut tc = ph.character.lock().await;
            if tc.fighting.is_none() {
                tc.fighting = Some(Target { id: me.id, is_player: false });
            }
        }
        let _ = ph.send.send(format!("\r\n{} attacks you!\r\n", me.name));
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} attacks {}!\r\n", me.name, ph.name));
        return CmdOutput::text(format!("\r\nYou attack {}!\r\n", ph.name));
    }
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
// Body position commands
// ---------------------------------------------------------------------------

use crate::character::Position;

/// `sleep` / `rest` / `sit` / `stand` — change body position.  No-op
/// when the requested position matches the current one.  Refused
/// while fighting (combat locks you into Fighting/Standing-ish state).
async fn do_position(
    me: &mut Character,
    chars: &SharedChars,
    target: Position,
) -> CmdOutput {
    if me.fighting.is_some() {
        return CmdOutput::text(
            "\r\nYou're too busy fighting to change position!\r\n".to_string()
        );
    }
    if me.position == target {
        let already = match target {
            Position::Sleeping => "You are already sound asleep.",
            Position::Resting  => "You are already resting.",
            Position::Sitting  => "You are already sitting.",
            Position::Standing => "You are already standing.",
            Position::Fighting => "You are already fighting.",
        };
        return CmdOutput::text(format!("\r\n{already}\r\n"));
    }
    me.position = target;
    let (self_line, room_line) = match target {
        Position::Sleeping =>
            ("You go to sleep.", format!("{} lies down and goes to sleep.", me.name)),
        Position::Resting =>
            ("You sit down and rest your tired bones.",
             format!("{} sits down and rests.", me.name)),
        Position::Sitting =>
            ("You sit down.", format!("{} sits down.", me.name)),
        Position::Standing =>
            ("You stand up.", format!("{} stands up.", me.name)),
        Position::Fighting =>
            ("You ready yourself for battle.",
             format!("{} readies for battle.", me.name)),
    };
    chars.lock().await.broadcast_room(
        me.current_room, Some(me.id), &format!("{room_line}\r\n"),
    );
    CmdOutput::text(format!("\r\n{self_line}\r\n"))
}

/// `wimpy [hp]` — set auto-flee HP threshold.  No arg shows current
/// state; `wimpy 0` (or `off`) disables.  Threshold is clamped to half
/// of max_hp so the player can't accidentally configure perpetual flight.
fn do_wimpy(arg: &str, me: &mut Character) -> CmdOutput {
    let arg = arg.trim();
    if arg.is_empty() {
        return CmdOutput::text(if me.wimpy <= 0 {
            "\r\nYour wimpy threshold is OFF.\r\n".to_string()
        } else {
            format!("\r\nYou will flee combat below {} HP.\r\n", me.wimpy)
        });
    }
    if arg.eq_ignore_ascii_case("off") {
        me.wimpy = 0;
        return CmdOutput::text("\r\nWimpy is now OFF.\r\n".to_string());
    }
    let Ok(v) = arg.parse::<i32>() else {
        return CmdOutput::text("\r\nUsage: wimpy <hp> | off\r\n".to_string());
    };
    let v = v.max(0).min((me.max_hp / 2).max(1));
    me.wimpy = v;
    if v == 0 {
        CmdOutput::text("\r\nWimpy is now OFF.\r\n".to_string())
    } else {
        CmdOutput::text(format!("\r\nYou will flee combat below {v} HP.\r\n"))
    }
}

/// `info <msg>` — newbie help channel.  Empty arg toggles the
/// sender's personal `info_off`.  Refused in SOUNDPROOF rooms (same
/// pattern as gossip).  Rendered in green.
async fn do_info(
    arg: &str,
    me: &mut Character,
    chars: &SharedChars,
) -> CmdOutput {
    if me.muted { return muted_msg(); }
    let msg = arg.trim();
    if msg.is_empty() {
        me.info_off = !me.info_off;
        return CmdOutput::text(format!(
            "\r\nInfo channel: {}.\r\n",
            if me.info_off { "off" } else { "on" },
        ));
    }
    if me.info_off {
        return CmdOutput::text(
            "\r\nYou have the info channel turned off.\r\n".to_string()
        );
    }
    let formatted = format!("\r\n@g[info] {} asks: '{msg}'@n\r\n", me.name);
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    for ph in &handles {
        if ph.id == me.id { continue; }
        let off = ph.character.lock().await.info_off;
        if off { continue; }
        let _ = ph.send.send(formatted.clone());
    }
    record_channel("info", &me.name, msg).await;
    CmdOutput::text(format!("\r\n@g[info] You ask: '{msg}'@n\r\n"))
}

/// `shout <msg>` — broadcasts to every player in the sender's current
/// *zone*.  Empty arg toggles `shout_off`.  Refused in SOUNDPROOF.
async fn do_shout(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.muted { return muted_msg(); }
    let msg = arg.trim();
    if msg.is_empty() {
        me.shout_off = !me.shout_off;
        return CmdOutput::text(format!(
            "\r\nShout channel: {}.\r\n",
            if me.shout_off { "off" } else { "on" },
        ));
    }
    if me.shout_off {
        return CmdOutput::text(
            "\r\nYou have the shout channel turned off.\r\n".to_string()
        );
    }
    // Snapshot the sender's zone, and the per-room→zone map under one lock.
    let (my_zone, room_zone): (i32, std::collections::HashMap<i32, i32>) = {
        let w = world.lock().await;
        if w.rooms.get(&me.current_room)
            .map(|r| r.room_flags[0] & crate::world::ROOM_SOUNDPROOF != 0)
            .unwrap_or(false)
        {
            return CmdOutput::text(
                "\r\nThe walls dampen your voice — no one outside can hear you.\r\n".to_string()
            );
        }
        let z = w.rooms.get(&me.current_room).map(|r| r.zone).unwrap_or(-1);
        let m = w.rooms.iter().map(|(v, r)| (*v, r.zone)).collect();
        (z, m)
    };
    let formatted = format!("\r\n@Y{} shouts, '{msg}'@n\r\n", me.name);
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    for ph in &handles {
        if ph.id == me.id { continue; }
        if room_zone.get(&ph.current_room).copied() != Some(my_zone) { continue; }
        let off = ph.character.lock().await.shout_off;
        if off { continue; }
        let _ = ph.send.send(formatted.clone());
    }
    record_channel("shout", &me.name, msg).await;
    CmdOutput::text(format!("\r\n@YYou shout, '{msg}'@n\r\n"))
}

/// `color [on|off]` — toggle ANSI color rendering.  Empty arg shows
/// state.  Persisted as `ClOf: 1` in the player file when off.
fn do_color(arg: &str, me: &mut Character) -> CmdOutput {
    let arg = arg.trim().to_ascii_lowercase();
    match arg.as_str() {
        "" => CmdOutput::text(format!(
            "\r\nColor is currently {}.\r\n",
            if me.color_off { "OFF" } else { "ON" },
        )),
        "off" | "0" | "none" => {
            me.color_off = true;
            CmdOutput::text("\r\nColor is now OFF.\r\n".to_string())
        }
        "on" | "1" | "normal" => {
            me.color_off = false;
            CmdOutput::text("\r\nColor is now ON.\r\n".to_string())
        }
        _ => CmdOutput::text("\r\nUsage: color [on|off]\r\n".to_string()),
    }
}

/// `worth` — itemize the player's net worth: gold carried, gold in
/// the bank, and the appraisal sum (proto.cost) of every carried or
/// equipped object.  Container contents are recursed.

/// `repair <item>` — at a shopkeeper's room, pay gold equal to
/// (100 - condition) * 5 to restore the item to pristine condition.

/// `heal` — at a Healer-spec'd mob's room, restores the player's HP
/// (and mana, half-rate) for gold.  Cost: 1 gold per HP missing.

/// Locate a PetShop-spec'd keeper in the caller's room.  Returns its
/// short_descr if present.
async fn find_pet_keeper(me: &Character, world: &Arc<Mutex<World>>) -> Option<String> {
    let w = world.lock().await;
    let r = w.rooms.get(&me.current_room)?;
    for &mid in &r.mobs {
        let m = w.mob_instances.iter().find(|m| m.id == mid)?;
        if m.spec == Some(crate::world::MobSpec::PetShop) {
            return w.mob_protos.get(&m.vnum).map(|p| p.short_descr.clone());
        }
    }
    None
}

/// `petlist` — at a pet-shop keeper, list buyable mobs in the same
/// room (every non-keeper mob) with the price tag `level*100 + 100`.

/// `petbuy <kw>` — at a pet-shop keeper, buy a charmed copy of a
/// nearby mob.  Pays `level*100 + 100`, spawns a fresh instance in
/// the caller's room with `spec` preserved, sets `charmer = me.id`,
/// applies a long-duration CharmPerson affect, and sets `following`.

/// `petdismiss <kw>` — release a charmed mob (`charmer == me.id`) in
/// the caller's room.  The pet is extracted with a polite farewell.

/// joins the named clan (any string, no validation).  `clan -` leaves.
async fn do_clan(arg: &str, me: &mut Character, chars: &SharedChars) -> CmdOutput {
    let arg = arg.trim();
    // `clan invite <player>` — must be in a clan, target in same room.
    if let Some(rest) = arg.strip_prefix("invite ").or_else(|| arg.strip_prefix("invite\t")) {
        let target_name = rest.trim();
        if target_name.is_empty() {
            return CmdOutput::text("\r\nUsage: clan invite <player>\r\n".to_string());
        }
        if me.clan.is_empty() {
            return CmdOutput::text(
                "\r\nYou aren't in a clan; nothing to invite to.\r\n".to_string()
            );
        }
        let target = {
            let cl = chars.lock().await;
            let h = cl.iter()
                .find(|p| p.id != me.id
                    && p.current_room == me.current_room
                    && p.name.eq_ignore_ascii_case(target_name))
                .cloned();
            h
        };
        let Some(ph) = target else {
            return CmdOutput::text(format!(
                "\r\nNo player named '{target_name}' is here.\r\n"
            ));
        };
        ph.character.lock().await.clan_invite_from = Some(me.id);
        let _ = ph.send.send(format!(
            "\r\n{} invites you to join clan {}.  Type `clan accept` to join.\r\n",
            me.name, me.clan,
        ));
        return CmdOutput::text(format!(
            "\r\nYou invite {} to join clan {}.\r\n", ph.name, me.clan,
        ));
    }
    // `clan accept` — consume pending invite, inherit inviter's clan.
    if arg.eq_ignore_ascii_case("accept") {
        let Some(inviter_id) = me.clan_invite_from.take() else {
            return CmdOutput::text("\r\nYou have no pending clan invite.\r\n".to_string());
        };
        let inviter = {
            let cl = chars.lock().await;
            let h = cl.iter().find(|p| p.id == inviter_id).cloned();
            h
        };
        let Some(iph) = inviter else {
            return CmdOutput::text("\r\nThe inviter has gone offline.\r\n".to_string());
        };
        let new_clan = iph.character.lock().await.clan.clone();
        if new_clan.is_empty() {
            return CmdOutput::text(
                "\r\nThe inviter has since left their clan.\r\n".to_string()
            );
        }
        me.clan = new_clan.clone();
        let _ = iph.send.send(format!(
            "\r\n{} has joined clan {new_clan}.\r\n", me.name,
        ));
        return CmdOutput::text(format!(
            "\r\nYou join clan {new_clan}.\r\n"
        ));
    }
    // `clan decline` — clear pending invite.
    if arg.eq_ignore_ascii_case("decline") {
        if me.clan_invite_from.take().is_none() {
            return CmdOutput::text("\r\nYou have no pending clan invite.\r\n".to_string());
        }
        return CmdOutput::text("\r\nClan invitation declined.\r\n".to_string());
    }
    if arg.is_empty() {
        if me.clan.is_empty() {
            return CmdOutput::text(
                "\r\nYou are not in any clan.  Use `clan <name>` to join one.\r\n".to_string()
            );
        }
        // List online clanmates.
        let handles: Vec<crate::character::PlayerHandle> = {
            let cl = chars.lock().await;
            cl.iter().cloned().collect()
        };
        let mut s = format!("\r\n=== Clan {} ===\r\n", me.clan);
        let mut n = 0;
        for ph in &handles {
            let c = ph.character.lock().await;
            if c.clan.eq_ignore_ascii_case(&me.clan) {
                s.push_str(&format!("  {}\r\n", ph.name));
                n += 1;
            }
        }
        s.push_str(&format!("\r\n{n} online member(s).\r\n"));
        return CmdOutput::text(s);
    }
    if arg == "-" {
        if me.clan.is_empty() {
            return CmdOutput::text("\r\nYou aren't in a clan.\r\n".to_string());
        }
        let old = std::mem::take(&mut me.clan);
        return CmdOutput::text(format!("\r\nYou leave clan {old}.\r\n"));
    }
    // Strip control bytes and cap at 30 chars.
    let new: String = arg.chars()
        .filter(|c| !c.is_control())
        .take(30)
        .collect();
    if new.is_empty() {
        return CmdOutput::text("\r\nClan name is empty after stripping.\r\n".to_string());
    }
    me.clan = new.clone();
    CmdOutput::text(format!("\r\nYou are now a member of clan {new}.\r\n"))
}

/// `clans` — list all clans with member counts, derived from online
/// players.
async fn do_clans(_me: &Character, chars: &SharedChars) -> CmdOutput {
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for ph in &handles {
        let c = ph.character.lock().await;
        if c.clan.is_empty() { continue; }
        *counts.entry(c.clan.to_ascii_lowercase()).or_insert(0) += 1;
    }
    if counts.is_empty() {
        return CmdOutput::text("\r\nNo clans have online members.\r\n".to_string());
    }
    let mut rows: Vec<(String, u32)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut s = String::from("\r\nClans with online members:\r\n");
    for (name, n) in rows {
        s.push_str(&format!("  {name:<20} {n} member(s)\r\n"));
    }
    CmdOutput::text(s)
}

/// `ctell <msg>` — broadcast to every online player in your clan,
/// case-insensitive match.  Refuses if you aren't in a clan.  Honors
/// the existing mute gate.
async fn do_ctell(arg: &str, me: &Character, chars: &SharedChars) -> CmdOutput {
    if me.muted { return muted_msg(); }
    if me.clan.is_empty() {
        return CmdOutput::text("\r\nYou aren't in any clan.\r\n".to_string());
    }
    let msg = arg.trim();
    if msg.is_empty() {
        return CmdOutput::text("\r\nCtell what?\r\n".to_string());
    }
    let formatted = format!("\r\n@m[{}] {} ctells: '{msg}'@n\r\n", me.clan, me.name);
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    for ph in &handles {
        if ph.id == me.id { continue; }
        let c = ph.character.lock().await;
        if c.clan.eq_ignore_ascii_case(&me.clan) {
            let _ = ph.send.send(formatted.clone());
        }
    }
    CmdOutput::text(format!("\r\n@m[{}] You ctell: '{msg}'@n\r\n", me.clan))
}

/// `hint [N]` — print one of N rotating beginner tips.  No arg picks
/// a random tip; a numeric arg shows that index (1-based).

/// `map` — 5x5 ASCII mini-map centered on the caller's current room.
/// Walks the exits up to 2 hops in each cardinal direction; cells
/// follow the standard "[#]" (room exists) / "[ ]" (unknown) /
/// "[@]" (you are here) convention.  Up/Down indicated below.
async fn do_map(me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    use crate::world::Direction;
    let w = world.lock().await;
    if !w.rooms.contains_key(&me.current_room) {
        return CmdOutput::text("\r\nYou are nowhere.\r\n".to_string());
    }
    fn step(w: &World, room: crate::world::RoomVnum, dir: Direction) -> Option<crate::world::RoomVnum> {
        let r = w.rooms.get(&room)?;
        let e = r.exits[dir as usize].as_ref()?;
        if e.to_room == crate::world::NOWHERE { return None; }
        if e.exit_info & crate::world::EX_CLOSED != 0 { return None; }
        if w.rooms.contains_key(&e.to_room) { Some(e.to_room) } else { None }
    }
    // Build a 5x5 grid (-2..=2, -2..=2).  Walk N|S then E|W from caller.
    let mut grid = [[" . "; 5]; 5];
    for dy in -2..=2i32 {
        for dx in -2..=2i32 {
            let mut room = Some(me.current_room);
            let (vdir, vsteps) = if dy > 0 { (Direction::North, dy) }
                                 else if dy < 0 { (Direction::South, -dy) }
                                 else { (Direction::North, 0) };
            for _ in 0..vsteps {
                room = room.and_then(|r| step(&w, r, vdir));
            }
            let (hdir, hsteps) = if dx > 0 { (Direction::East, dx) }
                                 else if dx < 0 { (Direction::West, -dx) }
                                 else { (Direction::East, 0) };
            for _ in 0..hsteps {
                room = room.and_then(|r| step(&w, r, hdir));
            }
            let y = (2 - dy) as usize;
            let x = (dx + 2) as usize;
            grid[y][x] = if dx == 0 && dy == 0 { "[@]" }
                         else if room.is_some() { "[#]" } else { " . " };
        }
    }
    let up = step(&w, me.current_room, Direction::Up).is_some();
    let dn = step(&w, me.current_room, Direction::Down).is_some();
    let room_name = w.rooms.get(&me.current_room).map(|r| r.name.clone()).unwrap_or_default();
    drop(w);
    let mut s = String::from("\r\n");
    for row in grid.iter() {
        for cell in row.iter() {
            s.push_str(cell);
        }
        s.push_str("\r\n");
    }
    s.push_str(&format!("\r\nHere: {room_name}\r\n"));
    let mut updn = Vec::new();
    if up { updn.push("up"); }
    if dn { updn.push("down"); }
    if !updn.is_empty() {
        s.push_str(&format!("Vertical: {}\r\n", updn.join(", ")));
    }
    CmdOutput::text(s)
}

/// `prefs` — single-screen overview of every persistent toggle.

/// `history` — show the last 20 dispatched commands (most recent
/// last).  Includes the `history` invocation itself, since recording
/// happens before dispatch.
fn do_history(me: &Character) -> CmdOutput {
    if me.history.is_empty() {
        return CmdOutput::text("\r\nNo history yet.\r\n".to_string());
    }
    let mut s = String::from("\r\nCommand history:\r\n");
    for (i, cmd) in me.history.iter().enumerate() {
        s.push_str(&format!("  {:>3}. {}\r\n", i + 1, cmd));
    }
    CmdOutput::text(s)
}

/// `chans [gossip|info|shout|auction]` — print the channel's last
/// 20 lines.  No arg lists which channels have content.

/// `tells` — list the last 20 received tells (most recent last).

enum AutoFlag { Exit, Loot, Assist, Title, Gold, Split, Sac, Door, Key, Map }

/// Flip a single auto-* preference.  Reports the new state.
fn do_toggle_auto(me: &mut Character, which: AutoFlag) -> CmdOutput {
    let (label, now_on) = match which {
        AutoFlag::Exit   => { me.autoexit   = !me.autoexit;   ("Autoexit",   me.autoexit) }
        AutoFlag::Loot   => { me.autoloot   = !me.autoloot;   ("Autoloot",   me.autoloot) }
        AutoFlag::Assist => { me.autoassist = !me.autoassist; ("Autoassist", me.autoassist) }
        AutoFlag::Title  => { me.autotitle  = !me.autotitle;  ("Autotitle",  me.autotitle) }
        AutoFlag::Gold   => { me.autogold   = !me.autogold;   ("Autogold",   me.autogold) }
        AutoFlag::Split  => { me.autosplit  = !me.autosplit;  ("Autosplit",  me.autosplit) }
        AutoFlag::Sac    => { me.autosac    = !me.autosac;    ("Autosac",    me.autosac) }
        AutoFlag::Door   => { me.autodoor   = !me.autodoor;   ("Autodoor",   me.autodoor) }
        AutoFlag::Key    => { me.autokey    = !me.autokey;    ("Autokey",    me.autokey) }
        AutoFlag::Map    => { me.automap    = !me.automap;    ("Automap",    me.automap) }
    };
    CmdOutput::text(format!(
        "\r\n{label} is now {}.\r\n",
        if now_on { "ON" } else { "OFF" },
    ))
}

/// `toggle [field]` — stock CircleMUD command.  With no arg, show a
/// one-screen summary of the player's toggle preferences; with a field
/// name, flip that preference and report the new state.
fn do_toggle(arg: &str, me: &mut Character) -> CmdOutput {
    let on = |b: bool| if b { "ON " } else { "OFF" };
    let key = arg.trim().to_ascii_lowercase();
    if key.is_empty() {
        let s = format!(
            "\r\nToggles for {}:\r\n\
             \x20 Brief:      {}    Compact:   {}\r\n\
             \x20 Autoexit:   {}    Color:     {}\r\n\
             \x20 Autoloot:   {}    Autoassist:{}\r\n\
             \x20 Autotitle:  {}    Autogold:  {}\r\n\
             \x20 Autosplit:  {}    Autosac:   {}\r\n\
             \x20 Autodoor:   {}    Autokey:   {}\r\n\
             \x20 Automap:    {}\r\n\
             \x20 NoGossip:   {}    NoAuction: {}\r\n\
             \x20 NoInfo:     {}    NoShout:   {}\r\n\
             \x20 Wimpy:      {}\r\n\
             Type `toggle <name>` to flip one (brief/compact/autoexit/color/\
             autoloot/autoassist/autotitle/autogold/autosplit/autosac/autodoor/\
             autokey/automap/gossip/auction/info/shout).\r\n",
            me.name,
            on(me.brief), on(me.compact),
            on(me.autoexit), on(!me.color_off),
            on(me.autoloot), on(me.autoassist),
            on(me.autotitle), on(me.autogold),
            on(me.autosplit), on(me.autosac),
            on(me.autodoor), on(me.autokey),
            on(me.automap),
            on(!me.gossip_off), on(!me.auction_off),
            on(!me.info_off), on(!me.shout_off),
            me.wimpy,
        );
        return CmdOutput::text(s);
    }
    let (label, now) = match key.as_str() {
        "brief"      => { me.brief = !me.brief; ("Brief", me.brief) }
        "compact"    => { me.compact = !me.compact; ("Compact", me.compact) }
        "autoexit" | "exits" => { me.autoexit = !me.autoexit; ("Autoexit", me.autoexit) }
        "autoloot"   => { me.autoloot = !me.autoloot; ("Autoloot", me.autoloot) }
        "autoassist" => { me.autoassist = !me.autoassist; ("Autoassist", me.autoassist) }
        "autotitle"  => { me.autotitle = !me.autotitle; ("Autotitle", me.autotitle) }
        "autogold"   => { me.autogold = !me.autogold; ("Autogold", me.autogold) }
        "autosplit"  => { me.autosplit = !me.autosplit; ("Autosplit", me.autosplit) }
        "autosac"    => { me.autosac = !me.autosac; ("Autosac", me.autosac) }
        "autodoor"   => { me.autodoor = !me.autodoor; ("Autodoor", me.autodoor) }
        "autokey"    => { me.autokey = !me.autokey; ("Autokey", me.autokey) }
        "automap"    => { me.automap = !me.automap; ("Automap", me.automap) }
        "color"      => { me.color_off = !me.color_off; ("Color", !me.color_off) }
        "gossip" | "nogossip"   => { me.gossip_off = !me.gossip_off; ("Gossip", !me.gossip_off) }
        "auction" | "noauction" => { me.auction_off = !me.auction_off; ("Auction", !me.auction_off) }
        "info" | "noinfo"       => { me.info_off = !me.info_off; ("Info", !me.info_off) }
        "shout" | "noshout"     => { me.shout_off = !me.shout_off; ("Shout", !me.shout_off) }
        _ => return CmdOutput::text(format!("\r\nUnknown toggle '{key}'. Type `toggle` for the list.\r\n")),
    };
    CmdOutput::text(format!("\r\n{label} is now {}.\r\n", if now { "ON" } else { "OFF" }))
}

/// `bandage` — basic first aid: bind your wounds for a small heal when
/// out of combat.  Mirrors stock SKILL_BANDAGE (simplified, no skill
/// roll).
fn do_bandage(me: &mut Character) -> CmdOutput {
    if me.fighting.is_some() {
        return CmdOutput::text("\r\nYou can't bandage in the middle of a fight!\r\n".to_string());
    }
    if me.hp >= me.max_hp {
        return CmdOutput::text("\r\nYou aren't wounded.\r\n".to_string());
    }
    let heal = crate::db::dice(2, 6) + me.level / 4;
    me.hp = (me.hp + heal).min(me.max_hp);
    CmdOutput::text(format!(
        "\r\nYou bind your wounds, recovering {heal} hit points. ({}/{})\r\n",
        me.hp, me.max_hp,
    ))
}

/// `wake` — go from Sleeping to Resting (the typical "wake from a
/// nap" transition).  No-op otherwise.
async fn do_wake(me: &mut Character, chars: &SharedChars) -> CmdOutput {
    if me.position != Position::Sleeping {
        return CmdOutput::text("\r\nYou are already awake.\r\n".to_string());
    }
    me.position = Position::Resting;
    chars.lock().await.broadcast_room(
        me.current_room, Some(me.id),
        &format!("{} wakes up.\r\n", me.name),
    );
    CmdOutput::text("\r\nYou wake up.\r\n".to_string())
}

// ---------------------------------------------------------------------------
// Combat skills (kick, bash)
// ---------------------------------------------------------------------------

use crate::character::Skill;

/// `rescue <player>` — pull a mob's aggression off an ally and onto
/// yourself.  Requires the ally to be in the same room and engaged
/// with a mob; on success: I become the mob's new target, the ally
/// stops fighting, and I start fighting the mob.  Chance scales with
/// learned%; on a roll fail nothing happens (mana stays free since
/// physical skill).
async fn do_rescue(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use rand::Rng;
    if !Skill::Rescue.is_class_allowed(me.class) {
        return CmdOutput::text(
            "\r\nYou do not know how to rescue anyone.\r\n".to_string()
        );
    }
    let learned = *me.skills.get(&Skill::Rescue).unwrap_or(&0) as i32;
    if learned == 0 {
        return CmdOutput::text(
            "\r\nYou have never practised rescue.\r\n".to_string()
        );
    }
    let name = arg.trim();
    if name.is_empty() {
        return CmdOutput::text("\r\nWhom do you want to rescue?\r\n".to_string());
    }
    // Find the target ally — must be a different player in the same
    // room who is currently fighting a mob.
    let (ally, mob_target) = {
        let cl = chars.lock().await;
        let ph = cl.iter()
            .find(|p| p.id != me.id
                && p.current_room == me.current_room
                && p.name.eq_ignore_ascii_case(name))
            .cloned();
        let Some(ph) = ph else {
            return CmdOutput::text(
                "\r\nThey aren't here.\r\n".to_string()
            );
        };
        let fighting = ph.character.lock().await.fighting;
        (ph, fighting)
    };
    let Some(mob_t) = mob_target.filter(|t| !t.is_player) else {
        return CmdOutput::text(format!(
            "\r\n{} doesn't need rescuing.\r\n", ally.name
        ));
    };
    // Skill roll.  Chance = (40 + learned/2).min(90).
    let chance = (40 + learned / 2).min(90);
    let roll = rand::thread_rng().gen_range(1..=100);
    if roll > chance {
        let _ = ally.send.send(format!(
            "\r\n{} tried to rescue you, but failed.\r\n", me.name
        ));
        return CmdOutput::text(format!(
            "\r\nYou fail to rescue {}.\r\n", ally.name
        ));
    }
    // Swap targeting under one world lock.
    {
        let mut w = world.lock().await;
        if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_t.id) {
            m.fighting = Some(crate::character::Target {
                id: me.id, is_player: true,
            });
        } else {
            return CmdOutput::text(
                "\r\nThe enemy is no longer here.\r\n".to_string()
            );
        }
    }
    // Ally stops fighting; I take over.
    ally.character.lock().await.fighting = None;
    me.fighting = Some(mob_t);
    // Skill bump on success.
    let bump = learn_attempt(me, Skill::Rescue, 5);
    let _ = ally.send.send(format!(
        "\r\n{} comes to your rescue!\r\n", me.name
    ));
    // Room broadcast to others (excluding rescuer and ally).
    {
        let cl = chars.lock().await;
        let line = format!("{} rescues {}!\r\n", me.name, ally.name);
        for ph in cl.iter() {
            if ph.id == me.id || ph.id == ally.id { continue; }
            if ph.current_room != me.current_room { continue; }
            let _ = ph.send.send(line.clone());
        }
    }
    let mut out = CmdOutput::text(format!(
        "\r\nYou successfully rescue {}!\r\n", ally.name
    ));
    if let Some(b) = bump {
        out.text.push_str(&b);
    }
    out
}

/// `disarm <target>` — knock the target's weapon onto the room floor.
/// PvP target: pulls from `equipment[WEAR_WIELD]`.  Mob target: pulls
/// the first `ITEM_WEAPON` from the mob's inventory.  Chance =
/// `(30 + learned/2).min(80)`; on miss broadcasts a fumble.
async fn do_disarm(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use rand::Rng;
    use crate::character::{Skill, WEAR_WIELD};
    use crate::world::ITEM_WEAPON;
    if !Skill::Disarm.is_class_allowed(me.class) {
        return CmdOutput::text(
            "\r\nYou do not know how to disarm anyone.\r\n".to_string()
        );
    }
    let learned = *me.skills.get(&Skill::Disarm).unwrap_or(&0) as i32;
    if learned == 0 {
        return CmdOutput::text(
            "\r\nYou have never practised disarm.\r\n".to_string()
        );
    }
    let key = arg.trim().to_ascii_lowercase();
    if key.is_empty() {
        return CmdOutput::text("\r\nDisarm whom?\r\n".to_string());
    }
    me.reveal();
    let chance = (30 + learned / 2).min(80);
    let succeeds = rand::thread_rng().gen_range(1..=100) <= chance;

    // PvP path first: look up an online player by name in our room.
    let pvp_target = {
        let cl = chars.lock().await;
        let h = cl.iter()
            .find(|p| p.id != me.id
                && p.current_room == me.current_room
                && p.name.eq_ignore_ascii_case(&key))
            .cloned();
        h
    };
    if let Some(ph) = pvp_target {
        if !succeeds {
            let _ = ph.send.send(format!(
                "\r\n{} tries to disarm you, but fails.\r\n", me.name
            ));
            return CmdOutput::text(format!(
                "\r\nYou fail to disarm {}.\r\n", ph.name
            ));
        }
        let dropped = {
            let mut c = ph.character.lock().await;
            c.equipment[WEAR_WIELD].take()
        };
        let Some(iid) = dropped else {
            return CmdOutput::text(format!(
                "\r\n{} isn't wielding anything.\r\n", ph.name
            ));
        };
        // Move iid to room floor.
        let label = {
            let mut w = world.lock().await;
            if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
                o.in_room = me.current_room;
            }
            if let Some(r) = w.rooms.get_mut(&me.current_room) {
                r.objects.push(iid);
            }
            w.obj_instances.iter()
                .find(|o| o.id == iid)
                .and_then(|o| w.obj_protos.get(&o.vnum))
                .map(|p| p.short_description.clone())
                .unwrap_or_else(|| "their weapon".to_string())
        };
        let _ = learn_attempt(me, Skill::Disarm, 5);
        let _ = ph.send.send(format!(
            "\r\n{} disarms you!  {label} falls to the ground.\r\n", me.name
        ));
        let cl = chars.lock().await;
        let line = format!("{} disarms {} — {label} falls to the ground.\r\n",
            me.name, ph.name);
        for hp in cl.iter() {
            if hp.id == me.id || hp.id == ph.id { continue; }
            if hp.current_room != me.current_room { continue; }
            let _ = hp.send.send(line.clone());
        }
        return CmdOutput::text(format!(
            "\r\nYou disarm {}!  {label} clatters to the floor.\r\n", ph.name
        ));
    }

    // Mob path: keyword match in current room.
    let (mob_id, weapon_iid, weapon_label, mob_short, weapon_from_slot) = {
        let w = world.lock().await;
        let r = match w.rooms.get(&me.current_room) {
            Some(r) => r,
            None => return CmdOutput::text("\r\nYou are nowhere.\r\n".to_string()),
        };
        let mob_hit = r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|n| n.eq_ignore_ascii_case(&key)) {
                Some((mid, m.equipment[WEAR_WIELD], m.inventory.clone(), p.short_descr.clone()))
            } else { None }
        });
        let Some((mid, wielded, inv, short)) = mob_hit else {
            return CmdOutput::text("\r\nNo such target here.\r\n".to_string());
        };
        // Prefer the wielded slot, then fall back to first inventory weapon.
        let weapon: Option<(u32, String, bool)> = if let Some(iid) = wielded {
            w.obj_instances.iter().find(|o| o.id == iid)
                .and_then(|o| w.obj_protos.get(&o.vnum))
                .map(|p| (iid, p.short_description.clone(), true))
        } else {
            let mut hit: Option<(u32, String, bool)> = None;
            for iid in inv {
                if let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) {
                    if let Some(p) = w.obj_protos.get(&o.vnum) {
                        if p.item_type == ITEM_WEAPON {
                            hit = Some((iid, p.short_description.clone(), false));
                            break;
                        }
                    }
                }
            }
            hit
        };
        let Some((wid, wlbl, from_slot)) = weapon else {
            return CmdOutput::text(format!(
                "\r\n{short} isn't carrying a weapon.\r\n"
            ));
        };
        (mid, wid, wlbl, short, from_slot)
    };
    if !succeeds {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} tries to disarm {mob_short}, but fails.\r\n", me.name));
        return CmdOutput::text(format!(
            "\r\nYou fail to disarm {mob_short}.\r\n"
        ));
    }
    {
        let mut w = world.lock().await;
        if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
            if weapon_from_slot {
                m.equipment[WEAR_WIELD] = None;
            } else {
                m.inventory.retain(|&i| i != weapon_iid);
            }
        }
        if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == weapon_iid) {
            o.in_room = me.current_room;
        }
        if let Some(r) = w.rooms.get_mut(&me.current_room) {
            r.objects.push(weapon_iid);
        }
    }
    let bump = learn_attempt(me, Skill::Disarm, 5);
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} disarms {mob_short} — {weapon_label} falls to the ground.\r\n", me.name));
    }
    let mut out = CmdOutput::text(format!(
        "\r\nYou disarm {mob_short}!  {weapon_label} clatters to the floor.\r\n"
    ));
    if let Some(b) = bump { out.text.push_str(&b); }
    out
}

/// `recover` — pull every item out of a corpse on the floor labeled
/// "corpse of <me>" (left behind by `player_death`).  Respects the
/// carry-cap.  Convenience wrapper around the cp133 mass-from-container
/// path; players don't have to type `get all from corpse.<name>`.

/// `consider <kw>` (alias `con`) — gauge the difficulty of fighting a
/// mob in the caller's current room.  Returns a single-line verdict
/// based on the level delta + the mob's current HP fraction.
async fn do_consider(arg: &str, me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let key = arg.trim().to_ascii_lowercase();
    if key.is_empty() {
        return CmdOutput::text("\r\nConsider whom?\r\n".to_string());
    }
    let w = world.lock().await;
    let r = match w.rooms.get(&me.current_room) {
        Some(r) => r,
        None => return CmdOutput::text("\r\nYou are nowhere.\r\n".to_string()),
    };
    let hit = r.mobs.iter().find_map(|&mid| {
        let m = w.mob_instances.iter().find(|m| m.id == mid)?;
        let p = w.mob_protos.get(&m.vnum)?;
        if p.name.split_whitespace().any(|n| n.eq_ignore_ascii_case(&key)) {
            Some((p.short_descr.clone(), p.level, m.hp, m.max_hp))
        } else { None }
    });
    let Some((short, mlevel, hp, max_hp)) = hit else {
        return CmdOutput::text("\r\nNo such creature here.\r\n".to_string());
    };
    let delta = mlevel - me.level;
    let verdict = match delta {
        d if d <= -10 => "is a complete walkover for you.",
        d if d <= -5  => "looks like easy prey.",
        d if d <= -2  => "looks weaker than you.",
        d if d <=  1  => "looks like a fair fight.",
        d if d <=  4  => "looks tough but doable.",
        d if d <=  9  => "would be a deadly challenge.",
        _             => "would tear you apart on sight!",
    };
    let wound = if hp < max_hp / 4 {
        " (It looks badly hurt.)"
    } else if hp < max_hp / 2 {
        " (It is wounded.)"
    } else { "" };
    CmdOutput::text(format!(
        "\r\n{short} (lvl {mlevel}) {verdict}{wound}\r\n"
    ))
}

/// `berserk` (Warrior): work yourself into a combat frenzy — a self-applied
/// affect that boosts damage at the cost of defence for a short duration.
/// No mana cost; no target.  (cp203)

/// `taunt <mob>` (Warrior): provoke a mob into attacking you instead of
/// its current target — a proactive tanking pull (vs `rescue`, which is
/// reactive).  Chance = (40 + learned/2).min(90).  (cp205)
/// `peek <target>` (Thief): glance at a same-room player's or mob's
/// carried inventory.  Rolls the Peek skill (chance = learned%); on a
/// fumble the target is none the wiser.  (cp217)
async fn do_peek(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use rand::Rng;
    use crate::character::Skill;
    if !Skill::Peek.is_class_allowed(me.class) {
        return CmdOutput::text("\r\nYou lack the deft touch to peek at others.\r\n".to_string());
    }
    let learned = *me.skills.get(&Skill::Peek).unwrap_or(&0) as i32;
    if learned == 0 {
        return CmdOutput::text("\r\nYou have never practised peek.\r\n".to_string());
    }
    let key = arg.trim().to_ascii_lowercase();
    if key.is_empty() {
        return CmdOutput::text("\r\nPeek at whom?\r\n".to_string());
    }
    me.reveal();

    // Resolve a same-room player first, then a mob by keyword.
    let player_target = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.id != me.id
            && p.current_room == me.current_room
            && p.name.eq_ignore_ascii_case(&key)).cloned();
        h
    };

    // Collect the target's inventory iids + a display name.
    let (target_name, inv): (String, Vec<u32>) = if let Some(ph) = &player_target {
        let c = ph.character.lock().await;
        (c.name.clone(), c.inventory.clone())
    } else {
        let w = world.lock().await;
        let hit = w.rooms.get(&me.current_room).and_then(|r| {
            r.mobs.iter().find_map(|&mid| {
                let m = w.mob_instances.iter().find(|m| m.id == mid)?;
                let p = w.mob_protos.get(&m.vnum)?;
                if p.name.split_whitespace().any(|n| n.eq_ignore_ascii_case(&key)) {
                    Some((p.short_descr.clone(), m.inventory.clone()))
                } else { None }
            })
        });
        match hit {
            Some(t) => t,
            None => return CmdOutput::text("\r\nNo one like that is here.\r\n".to_string()),
        }
    };

    // Skill roll.
    if rand::thread_rng().gen_range(0..100) >= learned {
        return CmdOutput::text(format!(
            "\r\nYou try to peek at {target_name}'s belongings, but can't get a good look.\r\n"
        ));
    }
    // Render the inventory.
    let names: Vec<String> = {
        let w = world.lock().await;
        inv.iter().filter_map(|&iid| {
            w.obj_instances.iter().find(|o| o.id == iid)
                .map(|o| obj_view(&w, o).short)
        }).collect()
    };
    let _ = learn_attempt(me, Skill::Peek, 5);
    if names.is_empty() {
        return CmdOutput::text(format!(
            "\r\nYou sneak a peek — {target_name} is carrying nothing.\r\n"
        ));
    }
    let mut out = format!("\r\nYou sneak a peek at what {target_name} is carrying:\r\n");
    for n in &names { out.push_str(&format!("  {n}\r\n")); }
    CmdOutput::text(out)
}


async fn do_skill(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    skill: Skill,
) -> CmdOutput {
    use rand::Rng;
    use crate::db::dice;

    // Snap pre-reveal hidden state so Backstab can double damage from
    // ambush; the reveal call below would otherwise clear it.
    let was_hidden = me.hidden;
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

    // PvP path for Bash: if the named target is a same-room player
    // (and both sides have pvp_ok), apply knockdown directly.
    if matches!(skill, Skill::Bash) && !arg.is_empty() {
        let key_pvp = arg.to_ascii_lowercase();
        let pvp_target = {
            let cl = chars.lock().await;
            let h = cl.iter()
                .find(|p| p.id != me.id
                    && p.current_room == me.current_room
                    && p.name.to_ascii_lowercase() == key_pvp)
                .cloned();
            h
        };
        if let Some(ph) = pvp_target {
            let (target_pvp_ok, target_name) = {
                let c = ph.character.lock().await;
                (c.pvp_ok, c.name.clone())
            };
            if !me.pvp_ok || !target_pvp_ok {
                return CmdOutput::text(
                    "\r\nBoth of you need `pvp` set on for that.\r\n".to_string()
                );
            }
            use rand::Rng;
            let learned = *me.skills.get(&skill).unwrap_or(&0) as i32;
            let chance = (30 + learned / 2).min(85);
            let landed = rand::thread_rng().gen_range(1..=100) <= chance;
            if !landed {
                let cl = chars.lock().await;
                cl.broadcast_room(me.current_room, Some(me.id),
                    &format!("{} tries to bash {target_name} but stumbles.\r\n", me.name));
                let _ = ph.send.send(format!(
                    "\r\n{} tries to bash you but stumbles.\r\n", me.name,
                ));
                return CmdOutput::text(format!(
                    "\r\nYou fail to bash {target_name}.\r\n"
                ));
            }
            // Knock to Sitting + apply 1-round Stun affect.
            {
                let mut c = ph.character.lock().await;
                c.position = crate::character::Position::Sitting;
                c.apply_affect(crate::character::Affect {
                    skill:         crate::character::Skill::Stun,
                    duration:      1,
                    to_hit:        0,
                    to_dam:        0,
                    dmg_reduction: 0,
                    dot_damage:    0,
                    to_ac:         0,
                });
            }
            let _ = ph.send.send(format!(
                "\r\n{} bashes you to the ground!\r\n", me.name,
            ));
            let cl = chars.lock().await;
            cl.broadcast_room(me.current_room, Some(me.id),
                &format!("{} bashes {target_name} to the ground!\r\n", me.name));
            let _ = learn_attempt(me, Skill::Bash, 5);
            return CmdOutput::text(format!(
                "\r\nYou bash {target_name} flat!\r\n"
            ));
        }
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
            Skill::Backstab => {
                let base = dice(3, 6) + me.level * 2 + str_b;
                // From-hidden ambush doubles damage.
                if was_hidden { base * 2 } else { base }
            }
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
        // Bash on a non-killing hit stuns the mob for one round.
        if hit && !dead && matches!(skill, Skill::Bash) {
            m.apply_affect(crate::character::Affect {
                skill:         crate::character::Skill::Stun,
                duration:      1,
                to_hit:        0,
                to_dam:        0,
                dmg_reduction: 0,
                dot_damage:    0,
                to_ac:          0,
            });
        }
        (mob_name, vnum, dead, mob_room)
    };

    // Broadcast + reply.
    let ambush_tag = if was_hidden && matches!(skill, Skill::Backstab) {
        " from the shadows"
    } else { "" };
    let (to_me, to_room) = if hit {
        (
            format!("\r\nYou {verb}{ambush_tag} {mob_name} for {dmg} damage!\r\n"),
            format!("{} {verb}s{ambush_tag} {mob_name}.\r\n", me.name),
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
                .map(mob_corpse_contents).unwrap_or_default();
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
    // Position gate.  Spellcasters can stand or fight, but not sleep/sit/rest.
    match me.position {
        crate::character::Position::Standing
        | crate::character::Position::Fighting => {}
        crate::character::Position::Sleeping =>
            return CmdOutput::text("\r\nYou dream of casting spells.\r\n".to_string()),
        _ =>
            return CmdOutput::text("\r\nYou need to stand to focus on a spell.\r\n".to_string()),
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
        crate::character::Skill::LightningBolt => cast_lightning_bolt(target, me, world, chars, learned).await,
        crate::character::Skill::Fireball      => cast_fireball(me, world, chars, learned).await,
        crate::character::Skill::ShockingGrasp => cast_shocking_grasp(target, me, world, chars, learned).await,
        crate::character::Skill::Invisibility  => cast_buff(target, me, chars, learned, crate::character::Skill::Invisibility).await,
        crate::character::Skill::Stoneskin     => cast_buff(target, me, chars, learned, crate::character::Skill::Stoneskin).await,
        crate::character::Skill::CureSerious   => cast_heal_spell(target, me, chars, learned, crate::character::Skill::CureSerious).await,
        crate::character::Skill::Heal          => cast_heal_spell(target, me, chars, learned, crate::character::Skill::Heal).await,
        crate::character::Skill::CureLight    => cast_cure_light(target, me, chars, learned).await,
        crate::character::Skill::Bless        => cast_bless(target, me, chars, learned).await,
        crate::character::Skill::BurningHands => cast_burning_hands(me, world, chars, learned).await,
        crate::character::Skill::Sanctuary    => cast_sanctuary(target, me, chars, learned).await,
        crate::character::Skill::Harm         => cast_harm(target, me, world, chars, learned).await,
        crate::character::Skill::WordOfRecall => cast_word_of_recall(me, world, chars).await,
        crate::character::Skill::Identify     => cast_identify(target, me, world).await,
        crate::character::Skill::DetectInvis  => cast_detect_invis(me),
        crate::character::Skill::Infravision  => cast_infravision(me),
        crate::character::Skill::ColorSpray   => cast_color_spray(me, world, chars, learned).await,
        crate::character::Skill::AcidBlast    => cast_acid_blast(target, me, world, chars, learned).await,
        crate::character::Skill::ChillTouch   => cast_chill_touch(target, me, world, chars, learned).await,
        crate::character::Skill::Enchant      => cast_enchant(target, me, world).await,
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
        crate::character::Skill::LocateObject => cast_locate_object(target, me, world, chars).await,
        crate::character::Skill::Refresh      => cast_refresh(target, me, chars).await,
        crate::character::Skill::Summon       => cast_summon(target, me, world, chars).await,
        crate::character::Skill::SenseLife    => cast_sense_life(me),
        crate::character::Skill::DetectAlign  => cast_detect(me, crate::character::Skill::DetectAlign),
        crate::character::Skill::DetectPoison => cast_detect(me, crate::character::Skill::DetectPoison),
        crate::character::Skill::Restoration  => cast_restoration(target, me, chars).await,
        crate::character::Skill::Fly          => cast_buff(target, me, chars, learned, crate::character::Skill::Fly).await,
        crate::character::Skill::CallLightning => cast_call_lightning(target, me, world, chars, learned).await,
        crate::character::Skill::CreateWater  => cast_create_water(me, world).await,
        crate::character::Skill::Curse        => cast_curse(target, me, world, chars, learned).await,
        crate::character::Skill::RemoveCurse  => cast_remove_curse(me),
        crate::character::Skill::DispelMagic  => cast_dispel_magic(target, me, world, chars).await,
        crate::character::Skill::DispelEvil   => cast_dispel_align(target, me, world, chars, learned, true).await,
        crate::character::Skill::DispelGood   => cast_dispel_align(target, me, world, chars, learned, false).await,
        crate::character::Skill::EnergyDrain  => cast_energy_drain(target, me, world, chars, learned).await,
        crate::character::Skill::ProtFromEvil => cast_buff(target, me, chars, learned, crate::character::Skill::ProtFromEvil).await,
        crate::character::Skill::Waterwalk    => cast_buff(target, me, chars, learned, crate::character::Skill::Waterwalk).await,
        crate::character::Skill::CreateFood   => cast_create_food(me, world).await,
        crate::character::Skill::Teleport     => cast_teleport(me, world, chars).await,
        crate::character::Skill::Ventriloquate => cast_ventriloquate(target, me, world, chars).await,
        crate::character::Skill::Darkness     => cast_darkness(me, world, chars).await,
        crate::character::Skill::ControlWeather => cast_control_weather(target, me),
        crate::character::Skill::GroupHeal    => cast_group(me, world, chars, crate::character::Skill::GroupHeal).await,
        crate::character::Skill::GroupArmor   => cast_group(me, world, chars, crate::character::Skill::GroupArmor).await,
        crate::character::Skill::GroupRecall  => cast_group(me, world, chars, crate::character::Skill::GroupRecall).await,
        crate::character::Skill::AnimateDead  => cast_summon_servant(target, me, world, chars, crate::character::Skill::AnimateDead).await,
        crate::character::Skill::Clone        => cast_summon_servant(target, me, world, chars, crate::character::Skill::Clone).await,
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

fn cast_sense_life(me: &mut Character) -> CmdOutput {
    let aff = crate::character::Affect {
        skill:         crate::character::Skill::SenseLife,
        duration:      12,
        to_hit:        0,
        to_dam:        0,
        dmg_reduction: 0,
        dot_damage:    0,
        to_ac:         0,
    };
    me.mana -= crate::character::Skill::SenseLife.mana_cost();
    me.apply_affect(aff);
    CmdOutput::text(
        "\r\nThe air shimmers. You can feel the heartbeats of those around you.\r\n",
    )
}

/// Self-buff detect spells (detect alignment / detect poison) — apply a
/// short affect and report.  Mirrors stock CircleMUD's detect spells.
fn cast_detect(me: &mut Character, skill: crate::character::Skill) -> CmdOutput {
    me.mana -= skill.mana_cost();
    me.apply_affect(crate::character::Affect {
        skill, duration: 12,
        to_hit: 0, to_dam: 0, dmg_reduction: 0, dot_damage: 0, to_ac: 0,
    });
    let msg = match skill {
        crate::character::Skill::DetectAlign  => "Your eyes tingle — you can sense the moral aura of others.",
        crate::character::Skill::DetectPoison => "Your senses sharpen — you can detect poison.",
        _ => "You feel more aware.",
    };
    CmdOutput::text(format!("\r\n{msg}\r\n"))
}

/// `create water` — fill the first ITEM_DRINKCON in the caster's
/// inventory to capacity with water.  Mirrors stock SPELL_CREATE_WATER.
async fn cast_create_water(me: &mut Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    me.mana -= crate::character::Skill::CreateWater.mana_cost();
    let mut w = world.lock().await;
    // Find a drink container in inventory.
    let target = me.inventory.iter().copied().find_map(|iid| {
        let o = w.obj_instances.iter().find(|o| o.id == iid)?;
        let p = w.obj_protos.get(&o.vnum)?;
        if p.item_type == crate::world::ITEM_DRINKCON {
            Some((o.vnum, p.short_description.clone()))
        } else { None }
    });
    let Some((vnum, short)) = target else {
        return CmdOutput::text("\r\nYou have no drink container to fill.\r\n".to_string());
    };
    if let Some(p) = w.obj_protos.get_mut(&vnum) {
        p.value[1] = p.value[0];   // fill to capacity
        p.value[2] = 0;            // liquid type = water
    }
    CmdOutput::text(format!("\r\nWater wells up, filling {short}.\r\n"))
}

/// `create food` — conjure a waybread (obj vnum 10) into the caster's hands.
/// Mirrors stock SPELL_CREATE_FOOD (`mag_creations`, obj vnum 10).
async fn cast_create_food(me: &mut Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    me.mana -= crate::character::Skill::CreateFood.mana_cost();
    let (iid, short) = {
        let mut w = world.lock().await;
        match w.spawn_obj(10) {
            Some(iid) => {
                let short = w.obj_protos.get(&10)
                    .map(|p| p.short_description.clone())
                    .unwrap_or_else(|| "a waybread".into());
                (iid, short)
            }
            None => return CmdOutput::text("\r\nThe spell fizzles — you can't conjure food here.\r\n".to_string()),
        }
    };
    me.inventory.push(iid);
    CmdOutput::text(format!("\r\nYou wave your hands and create {short}.\r\n"))
}

/// `teleport` — random teleport to another valid room.  Mirrors stock
/// SPELL_TELEPORT: pick a random room that is not private/death/godroom.
async fn cast_teleport(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use rand::seq::SliceRandom;
    me.mana -= crate::character::Skill::Teleport.mana_cost();
    let from_room = me.current_room;
    let target = {
        let w = world.lock().await;
        let candidates: Vec<RoomVnum> = w.rooms.iter()
            .filter(|(_, r)| {
                let f = r.room_flags[0];
                f & crate::world::ROOM_PRIVATE == 0
                    && f & crate::world::ROOM_DEATH == 0
                    && f & crate::world::ROOM_GODROOM == 0
            })
            .map(|(&v, _)| v)
            .collect();
        candidates.choose(&mut rand::thread_rng()).copied()
    };
    let Some(target) = target else {
        return CmdOutput::text("\r\nThe magic finds nowhere to send you.\r\n".to_string());
    };
    me.fighting = None;
    me.current_room = target;
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
        cl.update_room(me.id, target);
        cl.broadcast_room(from_room, Some(me.id),
            &format!("{} slowly fades out of existence and is gone.\r\n", me.name));
        cl.broadcast_room(target, Some(me.id),
            &format!("{} slowly fades into existence.\r\n", me.name));
    }
    let view = render_room(target, Some(me.id), world, chars).await;
    CmdOutput::text(format!("\r\nThe world spins, and you find yourself elsewhere.\r\n{view}"))
}

/// `ventriloquate <target> <message>` — throw the caster's voice so the
/// room hears a chosen mob/object speak.  Mirrors stock SPELL_VENTRILOQUATE.
async fn cast_ventriloquate(
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    me.mana -= crate::character::Skill::Ventriloquate.mana_cost();
    let arg = arg.trim();
    let (who, msg) = match arg.find(char::is_whitespace) {
        Some(i) => (&arg[..i], arg[i..].trim()),
        None    => (arg, ""),
    };
    if who.is_empty() || msg.is_empty() {
        return CmdOutput::text("\r\nFormat: cast 'ventriloquate' <target> <message>\r\n".to_string());
    }
    // Resolve the speaker's display name from a same-room mob keyword.
    let speaker = {
        let w = world.lock().await;
        w.rooms.get(&me.current_room).and_then(|r| r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|n| n.eq_ignore_ascii_case(who)) {
                Some(p.short_descr.clone())
            } else { None }
        }))
    };
    let speaker = speaker.unwrap_or_else(|| who.to_string());
    let line = format!("{speaker} says, '{msg}'\r\n");
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &line);
    }
    CmdOutput::text(format!("\r\nYou throw your voice — '{speaker} says, \"{msg}\"'\r\n"))
}

/// `darkness` — blanket the caster's current room in magical darkness by
/// toggling ROOM_DARK.  Mirrors stock SPELL_DARKNESS (`mag_rooms`).
async fn cast_darkness(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    me.mana -= crate::character::Skill::Darkness.mana_cost();
    let now_dark = {
        let mut w = world.lock().await;
        match w.rooms.get_mut(&me.current_room) {
            Some(r) => {
                r.room_flags[0] ^= crate::world::ROOM_DARK;
                r.room_flags[0] & crate::world::ROOM_DARK != 0
            }
            None => return CmdOutput::text("\r\nThe magic finds no room to darken.\r\n".to_string()),
        }
    };
    let (to_room, to_me) = if now_dark {
        ("The light in the room slowly fades into darkness.\r\n",
         "\r\nYou snuff out the light, plunging the room into darkness.\r\n")
    } else {
        ("The magical darkness slowly lifts.\r\n",
         "\r\nYou banish the magical darkness from the room.\r\n")
    };
    chars.lock().await.broadcast_room(me.current_room, Some(me.id), to_room);
    CmdOutput::text(to_me.to_string())
}

/// `control weather <better|worse>` — nudge the global sky toward fair or
/// foul.  Mirrors stock SPELL_CONTROL_WEATHER.
fn cast_control_weather(arg: &str, me: &mut Character) -> CmdOutput {
    use std::sync::atomic::Ordering;
    me.mana -= crate::character::Skill::ControlWeather.mana_cost();
    let arg = arg.trim().to_ascii_lowercase();
    let better = match arg.as_str() {
        "better" | "fair" | "clear" | "good" => true,
        "worse"  | "foul" | "storm" | "bad"  => false,
        _ => return CmdOutput::text(
            "\r\nDo you want the weather to get 'better' or 'worse'?\r\n".to_string()),
    };
    let sky = crate::db::WEATHER_SKY.load(Ordering::Relaxed);
    let new_sky = if better { (sky - 1).max(crate::db::SKY_CLOUDLESS) }
                  else       { (sky + 1).min(crate::db::SKY_LIGHTNING) };
    crate::db::WEATHER_SKY.store(new_sky, Ordering::Relaxed);
    let msg = if better { "You feel a change in the weather — the skies clear." }
              else       { "You feel a change in the weather — the skies darken." };
    CmdOutput::text(format!("\r\n{msg}\r\n"))
}

/// Group spells (`group heal` / `group armor` / `group recall`) — apply the
/// effect to every group member in the caster's room.  Mirrors stock
/// SPELL_GROUP_HEAL / GROUP_ARMOR / GROUP_RECALL.
async fn cast_group(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    skill: crate::character::Skill,
) -> CmdOutput {
    me.mana -= skill.mana_cost();
    let leader_id = me.following.unwrap_or(me.id);
    let room = me.current_room;

    // Collect candidate room members (other than me); group membership
    // (following/grouped) lives on Character, so filter under each lock.
    let candidates: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter()
            .filter(|p| p.id != me.id && p.current_room == room)
            .cloned()
            .collect()
    };
    let mut members: Vec<crate::character::PlayerHandle> = Vec::new();
    for ph in candidates {
        let in_group = {
            let c = ph.character.lock().await;
            c.grouped && (ph.id == leader_id || c.following == Some(leader_id))
        };
        if in_group { members.push(ph); }
    }

    match skill {
        crate::character::Skill::GroupArmor => {
            // +AC affect on self and each member.
            me.apply_affect(crate::character::Affect {
                skill: crate::character::Skill::Armor, duration: 24,
                to_hit: 0, to_dam: 0, dmg_reduction: 0, dot_damage: 0, to_ac: 20,
            });
            for ph in &members {
                let grouped = { ph.character.lock().await.grouped };
                if !grouped { continue; }
                let mut c = ph.character.lock().await;
                c.apply_affect(crate::character::Affect {
                    skill: crate::character::Skill::Armor, duration: 24,
                    to_hit: 0, to_dam: 0, dmg_reduction: 0, dot_damage: 0, to_ac: 20,
                });
                let _ = ph.send.send("\r\nA shimmering layer of force wraps around you.\r\n".to_string());
            }
            chars.lock().await.broadcast_room(room, Some(me.id),
                &format!("{} invokes a protective ward over the group.\r\n", me.name));
            CmdOutput::text("\r\nYou ward your group with shimmering armor.\r\n".to_string())
        }
        crate::character::Skill::GroupHeal => {
            use crate::db::dice;
            let heal = dice(3, 8) + me.level;
            me.hp = (me.hp + heal).min(me.max_hp);
            for ph in &members {
                let mut c = ph.character.lock().await;
                if !c.grouped { continue; }
                c.hp = (c.hp + heal).min(c.max_hp);
                let _ = ph.send.send("\r\nA warm healing light washes over you.\r\n".to_string());
            }
            chars.lock().await.broadcast_room(room, Some(me.id),
                &format!("{} calls down a wave of healing light.\r\n", me.name));
            CmdOutput::text(format!("\r\nHealing light washes over your group ({heal} HP each).\r\n"))
        }
        crate::character::Skill::GroupRecall => {
            let target = { world.lock().await.start_room(false) };
            // Recall every grouped member in the room (and the caster).
            for ph in &members {
                let (grouped, name) = {
                    let c = ph.character.lock().await;
                    (c.grouped, c.name.clone())
                };
                if !grouped { continue; }
                {
                    let mut c = ph.character.lock().await;
                    c.fighting = None;
                    c.current_room = target;
                }
                {
                    let mut w = world.lock().await;
                    for m in w.mob_instances.iter_mut() {
                        if m.fighting.map(|t| t.is_player && t.id == ph.id).unwrap_or(false) {
                            m.fighting = None;
                        }
                    }
                }
                {
                    let mut cl = chars.lock().await;
                    cl.update_room(ph.id, target);
                }
                let view = render_room(target, Some(ph.id), world, chars).await;
                let _ = ph.send.send(format!(
                    "\r\nA holy beacon snatches you back to the temple.\r\n{view}"));
                let _ = name;
            }
            // Recall the caster.
            let from_room = me.current_room;
            me.fighting = None;
            me.current_room = target;
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
                cl.update_room(me.id, target);
                cl.broadcast_room(from_room, Some(me.id),
                    &format!("{} and companions disappear in a flash of holy light!\r\n", me.name));
            }
            let view = render_room(target, Some(me.id), world, chars).await;
            CmdOutput::text(format!(
                "\r\nA holy beacon snatches your group back to the temple.\r\n{view}"))
        }
        _ => CmdOutput::text("\r\nUnsupported group spell.\r\n".to_string()),
    }
}

/// `animate dead` / `clone` — conjure a charmed servant mob into the
/// caster's room.  Mirrors stock `mag_summons`: animate dead raises a
/// zombie (mob vnum 11) from a corpse and pours the corpse's contents
/// into it; clone makes a duplicate (mob vnum 10) bearing the caster's
/// name.  Both become charmed followers of the caster.
async fn cast_summon_servant(
    _target: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    skill: crate::character::Skill,
) -> CmdOutput {
    me.mana -= skill.mana_cost();
    let animate = matches!(skill, crate::character::Skill::AnimateDead);
    let mob_vnum = if animate { 11 } else { 10 };

    // Animate dead requires a corpse on the floor.
    let corpse_id = if animate {
        let w = world.lock().await;
        let cid = w.rooms.get(&me.current_room).and_then(|r| {
            r.objects.iter().copied().find(|&iid| {
                w.obj_instances.iter().find(|o| o.id == iid)
                    .map(|o| o.corpse_of.is_some()).unwrap_or(false)
            })
        });
        if cid.is_none() {
            return CmdOutput::text(
                "\r\nYou need a corpse here to animate the dead.\r\n".to_string());
        }
        cid
    } else {
        None
    };

    // Spawn the servant mob.
    let mob_id = {
        let mut w = world.lock().await;
        match w.spawn_mob(mob_vnum, me.current_room) {
            Some(id) => id,
            None => return CmdOutput::text(
                "\r\nYou don't quite remember how to make that creature.\r\n".to_string()),
        }
    };

    // Charm it to the caster and (for clone) rename it.
    let servant_name = {
        let mut w = world.lock().await;
        if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
            m.charmer = Some(me.id);
            m.apply_affect(crate::character::Affect {
                skill: crate::character::Skill::CharmPerson,
                duration: 48,
                to_hit: 0, to_dam: 0, dmg_reduction: 0, dot_damage: 0, to_ac: 0,
            });
        }
        if animate {
            // Pour the corpse's contents into the zombie, then extract it.
            if let Some(cid) = corpse_id {
                let contents = w.obj_instances.iter_mut()
                    .find(|o| o.id == cid)
                    .map(|o| std::mem::take(&mut o.contents))
                    .unwrap_or_default();
                if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
                    for iid in contents { m.inventory.push(iid); }
                }
                w.extract_obj(cid);
            }
            w.mob_protos.get(&mob_vnum)
                .map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "a zombie".into())
        } else {
            me.name.clone()
        }
    };

    {
        let cl = chars.lock().await;
        let line = if animate {
            format!("{} raises {servant_name} from the dead!\r\n", me.name)
        } else {
            format!("{} conjures a duplicate of {}!\r\n", me.name, me.name)
        };
        cl.broadcast_room(me.current_room, Some(me.id), &line);
    }
    let reply = if animate {
        format!("\r\nYou raise {servant_name} to serve you.\r\n")
    } else {
        "\r\nA shimmering duplicate of yourself steps forth to serve.\r\n".to_string()
    };
    CmdOutput::text(reply)
}

/// `curse <target>` — debuff a mob in the room (saps its melee damage).
/// Mirrors stock SPELL_CURSE (here: a damage penalty via mob affect).
async fn cast_curse(
    target_kw: &str, me: &mut Character, world: &Arc<Mutex<World>>,
    chars: &SharedChars, learned: u8,
) -> CmdOutput {
    me.mana -= crate::character::Skill::Curse.mana_cost();
    let kw = target_kw.trim().to_ascii_lowercase();
    if kw.is_empty() {
        return CmdOutput::text("\r\nCurse whom?\r\n".to_string());
    }
    let (mid, short) = {
        let w = world.lock().await;
        let hit = w.rooms.get(&me.current_room).and_then(|r| r.mobs.iter().find_map(|&mid| {
            let m = w.mob_instances.iter().find(|m| m.id == mid)?;
            let p = w.mob_protos.get(&m.vnum)?;
            if p.name.split_whitespace().any(|n| n.eq_ignore_ascii_case(&kw)) {
                Some((mid, p.short_descr.clone()))
            } else { None }
        }));
        match hit { Some(h) => h, None => return CmdOutput::text("\r\nNo such creature is here.\r\n".to_string()) }
    };
    {
        let mut w = world.lock().await;
        if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mid) {
            m.apply_affect(crate::character::Affect {
                skill: crate::character::Skill::Curse,
                duration: 6 + learned as i32 / 10,
                to_hit: 0, to_dam: -(2 + learned as i32 / 20),
                dmg_reduction: 0, dot_damage: 0, to_ac: 0,
            });
        }
    }
    chars.lock().await.broadcast_room(me.current_room, Some(me.id),
        &format!("{short} is wreathed in a sickly red aura.\r\n"));
    CmdOutput::text(format!("\r\nYou lay a curse upon {short}.\r\n"))
}

/// `remove curse` — strip a Curse affect from the caster (cleanse).
/// Mirrors stock SPELL_REMOVE_CURSE.
fn cast_remove_curse(me: &mut Character) -> CmdOutput {
    me.mana -= crate::character::Skill::RemoveCurse.mana_cost();
    let before = me.affects.len();
    me.affects.retain(|a| a.skill != crate::character::Skill::Curse);
    if me.affects.len() != before {
        CmdOutput::text("\r\nA dark weight lifts from your shoulders.\r\n".to_string())
    } else {
        CmdOutput::text("\r\nYou are not cursed.\r\n".to_string())
    }
}

/// `dispel magic <target>` — strip one beneficial affect from a mob in
/// the room.  Mirrors stock SPELL_DISPEL_MAGIC (simplified).
async fn cast_dispel_magic(
    target_kw: &str, me: &mut Character, world: &Arc<Mutex<World>>, chars: &SharedChars,
) -> CmdOutput {
    me.mana -= crate::character::Skill::DispelMagic.mana_cost();
    let kw = target_kw.trim().to_ascii_lowercase();
    if kw.is_empty() {
        return CmdOutput::text("\r\nDispel magic on whom?\r\n".to_string());
    }
    use crate::character::Skill as S;
    const BENEFICIAL: &[S] = &[S::Sanctuary, S::Bless, S::Strength, S::Armor, S::Haste, S::Invisibility, S::Stoneskin];
    let (short, stripped) = {
        let mut w = world.lock().await;
        let mid = w.rooms.get(&me.current_room).and_then(|r| r.mobs.iter().copied().find(|&mid| {
            w.mob_instances.iter().find(|m| m.id == mid)
                .and_then(|m| w.mob_protos.get(&m.vnum))
                .map(|p| p.name.split_whitespace().any(|n| n.eq_ignore_ascii_case(&kw)))
                .unwrap_or(false)
        }));
        let Some(mid) = mid else {
            return CmdOutput::text("\r\nNo such creature is here.\r\n".to_string());
        };
        let short = w.mob_instances.iter().find(|m| m.id == mid)
            .and_then(|m| w.mob_protos.get(&m.vnum)).map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".to_string());
        let mut stripped = false;
        if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mid) {
            if let Some(pos) = m.affects.iter().position(|a| BENEFICIAL.contains(&a.skill)) {
                m.affects.remove(pos);
                stripped = true;
            }
        }
        (short, stripped)
    };
    if stripped {
        chars.lock().await.broadcast_room(me.current_room, Some(me.id),
            &format!("A wave of dispelling energy washes over {short}.\r\n"));
        CmdOutput::text(format!("\r\nYou strip the magic from {short}.\r\n"))
    } else {
        CmdOutput::text(format!("\r\n{short} has no magic to dispel.\r\n"))
    }
}

/// `dispel evil` / `dispel good` — Cleric smite vs a mob of the opposing
/// alignment.  `evil = true` damages evil-aligned mobs; `false` damages
/// good-aligned.  Mirrors stock SPELL_DISPEL_EVIL/GOOD.
async fn cast_dispel_align(
    target_kw: &str, me: &mut Character, world: &Arc<Mutex<World>>,
    chars: &SharedChars, learned: u8, evil: bool,
) -> CmdOutput {
    use rand::Rng;
    me.mana -= if evil { crate::character::Skill::DispelEvil.mana_cost() }
               else { crate::character::Skill::DispelGood.mana_cost() };
    let kw = target_kw.trim().to_ascii_lowercase();
    let mob_id = if !kw.is_empty() {
        let w = world.lock().await;
        w.rooms.get(&me.current_room).and_then(|r| r.mobs.iter().copied().find(|&mid| {
            w.mob_instances.iter().find(|m| m.id == mid)
                .and_then(|m| w.mob_protos.get(&m.vnum))
                .map(|p| p.name.split_whitespace().any(|n| n.eq_ignore_ascii_case(&kw)))
                .unwrap_or(false)
        }))
    } else {
        me.fighting.filter(|t| !t.is_player).map(|t| t.id)
    };
    let Some(mob_id) = mob_id else {
        return CmdOutput::text("\r\nThere is no such target here.\r\n".to_string());
    };
    let (short, align, mob_room, killed_vnum) = {
        let w = world.lock().await;
        let m = w.mob_instances.iter().find(|m| m.id == mob_id);
        let vnum = m.map(|m| m.vnum).unwrap_or(-1);
        let align = m.and_then(|m| w.mob_protos.get(&m.vnum)).map(|p| p.alignment).unwrap_or(0);
        let short = m.and_then(|m| w.mob_protos.get(&m.vnum)).map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".to_string());
        (short, align, m.map(|m| m.in_room).unwrap_or(crate::world::NOWHERE), vnum)
    };
    let valid = if evil { align < 0 } else { align > 0 };
    if !valid {
        return CmdOutput::text(format!(
            "\r\n{short} is not {} enough for the spell to bite.\r\n",
            if evil { "evil" } else { "good" }));
    }
    let dmg = crate::db::dice(6, 6) + me.level + crate::character::str_damage_bonus(me.wis);
    let dmg = if save_vs_spell(me.level, 0) { (dmg / 2).max(1) } else { dmg };
    let dead = {
        let mut w = world.lock().await;
        if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
            if me.fighting.is_none() {
                me.fighting = Some(Target { id: mob_id, is_player: false });
                m.fighting = Some(Target { id: me.id, is_player: true });
            }
            m.hp -= dmg;
            m.hp <= 0
        } else { false }
    };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("Holy power sears {short} for {dmg} damage!\r\n"));
    }
    if dead {
        let xp = {
            let w = world.lock().await;
            w.mob_instances.iter().find(|m| m.id == mob_id)
                .and_then(|m| w.mob_protos.get(&m.vnum)).map(|p| p.exp as i64).unwrap_or(0)
        };
        crate::combat::kill_mob_immediate(mob_id, mob_room, &short, &me.name, world, chars).await;
        me.fighting = None;
        let mut msg = format!("\r\nHoly power sears {short} for {dmg} damage!\r\nYou have slain {short}!\r\n");
        if xp > 0 {
            me.exp += xp;
            msg.push_str(&format!("You gain {xp} experience.\r\n"));
            if me.check_level_up() > 0 {
                msg.push_str(&format!("\r\n*** You are now level {}. ***\r\n", me.level));
            }
        }
        if let Some(q) = quest_check_kill(me, killed_vnum, world).await {
            msg.push_str(&q);
        }
        return CmdOutput::text(msg);
    }
    CmdOutput::text(format!("\r\nHoly power sears {short} for {dmg} damage!\r\n"))
}

fn cast_infravision(me: &mut Character) -> CmdOutput {
    // Affect that signals render_room to skip the dark-room gate
    // (the viewer can see in the dark).
    let aff = crate::character::Affect {
        skill:         crate::character::Skill::Infravision,
        duration:      24,
        to_hit:        0,
        to_dam:        0,
        dmg_reduction: 0,
        dot_damage:    0,
        to_ac:         0,
    };
    me.mana -= crate::character::Skill::Infravision.mana_cost();
    me.apply_affect(aff);
    CmdOutput::text(
        "\r\nYour eyes glow with a faint red light, piercing the dark.\r\n",
    )
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
    let base_dmg = crate::db::dice(1, 4) + me.level + crate::character::str_damage_bonus(me.int_);
    me.mana -= crate::character::Skill::MagicMissile.mana_cost();

    let (mob_name, killed_vnum, mob_dead, mob_room, saved, dmg) = {
        let mut w = world.lock().await;
        let m = match w.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => m,
            None    => return CmdOutput::text("\r\nYour target has vanished.\r\n"),
        };
        let vnum = m.vnum;
        let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
        let mob_name = w.mob_protos.get(&vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());
        let mob_room = m.in_room;
        if mob_room != me.current_room {
            return CmdOutput::text("\r\nYour target is no longer here.\r\n");
        }
        // Save for half on a hit.
        let saved = hit && save_vs_spell(me.level, target_level);
        let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
        // Engage combat.
        let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
        let dead = if hit { m.hp -= dmg; m.hp <= 0 } else { false };
        (mob_name, vnum, dead, mob_room, saved, dmg)
    };

    let (to_me, to_room) = if hit && saved {
        (
            format!("\r\nA glowing dart of force streaks from your hand and strikes {mob_name} for {dmg} damage (partial resist)!\r\n"),
            format!("A glowing dart of force streaks from {}'s hand and grazes {mob_name}.\r\n", me.name),
        )
    } else if hit {
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
                .map(mob_corpse_contents).unwrap_or_default();
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

/// `lightning bolt` — single-target high-damage MagicUser spell.
/// `dice(6, 6) + level + INT bonus` on a hit (save halves), 25 mana.
/// Reuses the magic-missile target/engage/death flow with thicker
/// flavor text.
async fn cast_lightning_bolt(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;

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

    // Hit chance: 60 base + half of learned %.
    let hit_chance = (60 + learned as i32 / 2).min(95);
    let hit = rand::thread_rng().gen_range(0..100) < hit_chance;
    let base_dmg = crate::db::dice(6, 6) + me.level
        + crate::character::str_damage_bonus(me.int_);
    me.mana -= crate::character::Skill::LightningBolt.mana_cost();

    let (mob_name, killed_vnum, mob_dead, mob_room, saved, dmg) = {
        let mut w = world.lock().await;
        let m = match w.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => m,
            None    => return CmdOutput::text("\r\nYour target has vanished.\r\n"),
        };
        let vnum = m.vnum;
        let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
        let mob_name = w.mob_protos.get(&vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());
        let mob_room = m.in_room;
        if mob_room != me.current_room {
            return CmdOutput::text("\r\nYour target is no longer here.\r\n");
        }
        let saved = hit && save_vs_spell(me.level, target_level);
        let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
        let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
        let dead = if hit { m.hp -= dmg; m.hp <= 0 } else { false };
        (mob_name, vnum, dead, mob_room, saved, dmg)
    };

    let (to_me, to_room) = if hit && saved {
        (
            format!("\r\nA crackling bolt of lightning arcs into {mob_name} for {dmg} damage (partial resist)!\r\n"),
            format!("A crackling bolt of lightning arcs from {} into {mob_name}.\r\n", me.name),
        )
    } else if hit {
        (
            format!("\r\nA crackling bolt of lightning blasts {mob_name} for {dmg} damage!\r\n"),
            format!("A crackling bolt of lightning arcs from {} and blasts {mob_name}!\r\n", me.name),
        )
    } else {
        (
            format!("\r\nYour lightning bolt fizzles around {mob_name}.\r\n"),
            format!("{}'s lightning bolt fizzles around {mob_name}.\r\n", me.name),
        )
    };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    if mob_dead {
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
                .map(mob_corpse_contents).unwrap_or_default();
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

/// `call lightning` — Cleric attack spell that only works outdoors during
/// rain or a thunderstorm (cp213, ties into the cp212 weather system).
/// `dice(7, 8) + level + WIS-bonus`, hit chance `(65 + learned/2).min(95)`,
/// save-for-half.  Mirrors the lightning-bolt template with a weather gate.
async fn cast_call_lightning(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;
    use std::sync::atomic::Ordering;

    // Weather gate: must be outdoors AND raining/storming.
    {
        let w = world.lock().await;
        let outdoor = w.rooms.get(&me.current_room)
            .map(|r| r.sector_type != crate::world::SECT_INSIDE
                  && r.sector_type != crate::world::SECT_CITY)
            .unwrap_or(false);
        if !outdoor {
            return CmdOutput::text(
                "\r\nYou must be outdoors, under the open sky, to call the lightning.\r\n".to_string()
            );
        }
    }
    let sky = crate::db::WEATHER_SKY.load(Ordering::Relaxed);
    if sky != crate::db::SKY_RAINING && sky != crate::db::SKY_LIGHTNING {
        return CmdOutput::text(
            "\r\nThe sky is calm — there is no storm to draw lightning from.\r\n".to_string()
        );
    }

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

    let hit_chance = (65 + learned as i32 / 2).min(95);
    let hit = rand::thread_rng().gen_range(0..100) < hit_chance;
    let base_dmg = crate::db::dice(7, 8) + me.level
        + crate::character::str_damage_bonus(me.wis);
    me.mana -= crate::character::Skill::CallLightning.mana_cost();

    let (mob_name, killed_vnum, mob_dead, mob_room, saved, dmg) = {
        let mut w = world.lock().await;
        let m = match w.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => m,
            None    => return CmdOutput::text("\r\nYour target has vanished.\r\n"),
        };
        let vnum = m.vnum;
        let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
        let mob_name = w.mob_protos.get(&vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());
        let mob_room = m.in_room;
        if mob_room != me.current_room {
            return CmdOutput::text("\r\nYour target is no longer here.\r\n");
        }
        let saved = hit && save_vs_spell(me.level, target_level);
        let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
        let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
        let dead = if hit { m.hp -= dmg; m.hp <= 0 } else { false };
        (mob_name, vnum, dead, mob_room, saved, dmg)
    };

    let (to_me, to_room) = if hit && saved {
        (
            format!("\r\nA bolt of lightning lances down into {mob_name} for {dmg} damage (partial resist)!\r\n"),
            format!("{} calls down a bolt of lightning upon {mob_name}.\r\n", me.name),
        )
    } else if hit {
        (
            format!("\r\nYou call down a searing bolt of lightning — it blasts {mob_name} for {dmg} damage!\r\n"),
            format!("{} calls down a searing bolt of lightning upon {mob_name}!\r\n", me.name),
        )
    } else {
        (
            format!("\r\nYour lightning strikes the ground beside {mob_name}, missing.\r\n"),
            format!("{}'s lightning strikes the ground, missing {mob_name}.\r\n", me.name),
        )
    };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    if mob_dead {
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
                .map(mob_corpse_contents).unwrap_or_default();
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

/// `energy drain` — single-target MagicUser necromantic damage spell.
/// Stock tbaMUD: low-level victims (<= 2) take a flat 100, else `dice(1, 10)
/// + level + INT-bonus`.  Save-for-half.  Mirrors the lightning template.
async fn cast_energy_drain(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;

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

    let hit_chance = (65 + learned as i32 / 2).min(95);
    let hit = rand::thread_rng().gen_range(0..100) < hit_chance;
    me.mana -= crate::character::Skill::EnergyDrain.mana_cost();

    let (mob_name, killed_vnum, mob_dead, mob_room, saved, dmg) = {
        let mut w = world.lock().await;
        let m = match w.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => m,
            None    => return CmdOutput::text("\r\nYour target has vanished.\r\n"),
        };
        let vnum = m.vnum;
        let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
        let mob_name = w.mob_protos.get(&vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());
        let mob_room = m.in_room;
        if mob_room != me.current_room {
            return CmdOutput::text("\r\nYour target is no longer here.\r\n");
        }
        let base_dmg = if target_level <= 2 {
            100
        } else {
            crate::db::dice(1, 10) + me.level
                + crate::character::str_damage_bonus(me.int_)
        };
        let saved = hit && save_vs_spell(me.level, target_level);
        let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
        let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
        let dead = if hit { m.hp -= dmg; m.hp <= 0 } else { false };
        (mob_name, vnum, dead, mob_room, saved, dmg)
    };

    let (to_me, to_room) = if hit && saved {
        (
            format!("\r\nLife-draining energy washes over {mob_name} for {dmg} damage (partial resist)!\r\n"),
            format!("{} drains the life from {mob_name}.\r\n", me.name),
        )
    } else if hit {
        (
            format!("\r\nYou drain the very life-force from {mob_name} — {dmg} damage!\r\n"),
            format!("{} drains the very life-force from {mob_name}!\r\n", me.name),
        )
    } else {
        (
            format!("\r\nYour draining touch fails to find {mob_name}.\r\n"),
            format!("{}'s draining magic fizzles against {mob_name}.\r\n", me.name),
        )
    };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    if mob_dead {
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
                .map(mob_corpse_contents).unwrap_or_default();
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

/// `acid blast` — single-target MagicUser damage spell.  Heavier than
/// magic missile but cheaper than lightning bolt: `dice(5, 6) + level
/// + INT-bonus`, hit chance `(70 + learned/2).min(98)`, save-for-half.
/// Mirrors the lightning-bolt template with acid flavor.
async fn cast_acid_blast(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;

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

    let hit_chance = (70 + learned as i32 / 2).min(98);
    let hit = rand::thread_rng().gen_range(0..100) < hit_chance;
    let base_dmg = crate::db::dice(5, 6) + me.level
        + crate::character::str_damage_bonus(me.int_);
    me.mana -= crate::character::Skill::AcidBlast.mana_cost();

    let (mob_name, killed_vnum, mob_dead, mob_room, saved, dmg) = {
        let mut w = world.lock().await;
        let m = match w.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => m,
            None    => return CmdOutput::text("\r\nYour target has vanished.\r\n"),
        };
        let vnum = m.vnum;
        let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
        let mob_name = w.mob_protos.get(&vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());
        let mob_room = m.in_room;
        if mob_room != me.current_room {
            return CmdOutput::text("\r\nYour target is no longer here.\r\n");
        }
        let saved = hit && save_vs_spell(me.level, target_level);
        let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
        let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
        let dead = if hit { m.hp -= dmg; m.hp <= 0 } else { false };
        (mob_name, vnum, dead, mob_room, saved, dmg)
    };

    let (to_me, to_room) = if hit && saved {
        (
            format!("\r\nA sizzling acid blast splatters {mob_name} for {dmg} damage (partial resist)!\r\n"),
            format!("A sizzling acid blast from {} splatters {mob_name}.\r\n", me.name),
        )
    } else if hit {
        (
            format!("\r\nA sizzling acid blast eats into {mob_name} for {dmg} damage!\r\n"),
            format!("A sizzling acid blast from {} eats into {mob_name}!\r\n", me.name),
        )
    } else {
        (
            format!("\r\nYour acid blast hisses past {mob_name}.\r\n"),
            format!("{}'s acid blast hisses past {mob_name}.\r\n", me.name),
        )
    };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    if mob_dead {
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
                .map(mob_corpse_contents).unwrap_or_default();
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

/// `chill touch` — single-target MU damage spell that also drains
/// the victim's offensive output for a short duration.  `dice(1, 8) +
/// level + INT-bonus`, hit chance `(75 + learned/2).min(99)`, save-for-
/// half; on a non-saved hit, applies `Affect { ChillTouch, duration: 5,
/// to_dam: -2, .. }` to the mob (reduces its melee damage via the
/// existing affect_dam_bonus path).  10 mana.
async fn cast_chill_touch(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;

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

    let hit_chance = (75 + learned as i32 / 2).min(99);
    let hit = rand::thread_rng().gen_range(0..100) < hit_chance;
    let base_dmg = crate::db::dice(1, 8) + me.level
        + crate::character::str_damage_bonus(me.int_);
    me.mana -= crate::character::Skill::ChillTouch.mana_cost();

    let (mob_name, killed_vnum, mob_dead, mob_room, saved, dmg) = {
        let mut w = world.lock().await;
        let m = match w.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => m,
            None    => return CmdOutput::text("\r\nYour target has vanished.\r\n"),
        };
        let vnum = m.vnum;
        let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
        let mob_name = w.mob_protos.get(&vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());
        let mob_room = m.in_room;
        if mob_room != me.current_room {
            return CmdOutput::text("\r\nYour target is no longer here.\r\n");
        }
        let saved = hit && save_vs_spell(me.level, target_level);
        let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
        let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
        let dead = if hit {
            m.hp -= dmg;
            if !saved {
                m.apply_affect(crate::character::Affect {
                    skill:         crate::character::Skill::ChillTouch,
                    duration:      5,
                    to_hit:        0,
                    to_dam:        -2,
                    dmg_reduction: 0,
                    dot_damage:    0,
                    to_ac:         0,
                });
            }
            m.hp <= 0
        } else { false };
        (mob_name, vnum, dead, mob_room, saved, dmg)
    };

    let (to_me, to_room) = if hit && saved {
        (
            format!("\r\nA chilling touch grips {mob_name} for {dmg} damage (partial resist).\r\n"),
            format!("A chilling touch from {} grips {mob_name}.\r\n", me.name),
        )
    } else if hit {
        (
            format!("\r\nA chilling touch from your hand saps {mob_name} for {dmg} damage!\r\n"),
            format!("A chilling touch from {} saps {mob_name}.\r\n", me.name),
        )
    } else {
        (
            format!("\r\nYour chill touch fades against {mob_name}.\r\n"),
            format!("{}'s chill touch fades against {mob_name}.\r\n", me.name),
        )
    };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    if mob_dead {
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
                .map(mob_corpse_contents).unwrap_or_default();
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

/// `fireball` — high-damage AoE MagicUser spell.  Hits every mob in
/// the room with `dice(8, 8) + level + INT-bonus`; per-target save
/// halves.  30 mana.  Same kill plumbing as burning_hands (DEATH
/// triggers, corpse, XP credited solo to caster).
async fn cast_fireball(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;
    use crate::db::dice;

    let mob_ids: Vec<u32> = {
        let w = world.lock().await;
        w.rooms.get(&me.current_room)
            .map(|r| r.mobs.clone())
            .unwrap_or_default()
    };
    if mob_ids.is_empty() {
        return CmdOutput::text("\r\nA fireball arcs harmlessly through empty air.\r\n");
    }
    me.mana -= crate::character::Skill::Fireball.mana_cost();

    let mut to_me = String::from("\r\nA roaring fireball detonates around you!\r\n");
    let to_room = format!("{} hurls a roaring fireball into the room!\r\n", me.name);
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    for mob_id in mob_ids {
        let hit_chance = (75 + learned as i32 / 4).min(98);
        if rand::thread_rng().gen_range(0..100) >= hit_chance { continue; }
        let base_dmg = dice(8, 8) + me.level
            + crate::character::str_damage_bonus(me.int_);

        let (mob_name, mob_dead, mob_room, dmg, saved) = {
            let mut w = world.lock().await;
            let (vnum, in_room) = match w.mob_instances.iter().find(|m| m.id == mob_id) {
                Some(m) => (m.vnum, m.in_room),
                None => continue,
            };
            if in_room != me.current_room { continue; }
            let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
            let mob_name = w.mob_protos.get(&vnum)
                .map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "the creature".into());
            let saved = save_vs_spell(me.level, target_level);
            let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
            let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
            m.hp -= dmg;
            if me.fighting.is_none() {
                me.fighting = Some(Target { id: mob_id, is_player: false });
            }
            if m.fighting.is_none() {
                m.fighting = Some(Target { id: me.id, is_player: true });
            }
            (mob_name, m.hp <= 0, in_room, dmg, saved)
        };
        if saved {
            to_me.push_str(&format!("Flame engulfs {mob_name} for {dmg} damage (partial resist).\r\n"));
        } else {
            to_me.push_str(&format!("Flame engulfs {mob_name} for {dmg} damage!\r\n"));
        }

        if mob_dead {
            fire_mob_death_triggers(mob_id, &me.name, world, chars).await;
            let (xp, vnum) = {
                let w = world.lock().await;
                let m = w.mob_instances.iter().find(|m| m.id == mob_id);
                let v = m.map(|m| m.vnum).unwrap_or(-1);
                let x = m.and_then(|m| w.mob_protos.get(&m.vnum))
                    .map(|p| p.exp as i64).unwrap_or(0);
                (x, v)
            };
            {
                let mut w = world.lock().await;
                let inv: Vec<u32> = w.mob_instances.iter()
                    .find(|m| m.id == mob_id)
                    .map(mob_corpse_contents).unwrap_or_default();
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
            if me.fighting.map(|t| !t.is_player && t.id == mob_id).unwrap_or(false) {
                me.fighting = None;
            }
            {
                let cl = chars.lock().await;
                cl.broadcast_room(
                    mob_room, None,
                    &format!("\r\n{mob_name} is incinerated by the fireball!\r\n"),
                );
            }
            to_me.push_str(&format!("\r\n{mob_name} is reduced to ash.\r\n"));
            if xp > 0 {
                me.exp += xp;
                to_me.push_str(&format!("You gain {xp} experience.\r\n"));
                let gained = me.check_level_up();
                if gained > 0 {
                    to_me.push_str(&format!(
                        "\r\n*** You feel more powerful!  You are now level {}.  Max HP: {} ***\r\n",
                        me.level, me.max_hp,
                    ));
                }
            }
            if let Some(qmsg) = quest_check_kill(me, vnum, world).await {
                to_me.push_str(&qmsg);
            }
        }
    }
    CmdOutput::text(to_me)
}

/// `shocking grasp` — single-target melee-range MagicUser spell.
/// `dice(3, 8) + level + INT-bonus` on hit, save halves.  8 mana.
async fn cast_shocking_grasp(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;

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

    let hit_chance = (75 + learned as i32 / 2).min(99);
    let hit = rand::thread_rng().gen_range(0..100) < hit_chance;
    let base_dmg = crate::db::dice(3, 8) + me.level
        + crate::character::str_damage_bonus(me.int_);
    me.mana -= crate::character::Skill::ShockingGrasp.mana_cost();

    let (mob_name, killed_vnum, mob_dead, mob_room, saved, dmg) = {
        let mut w = world.lock().await;
        let m = match w.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => m,
            None    => return CmdOutput::text("\r\nYour target has vanished.\r\n"),
        };
        let vnum = m.vnum;
        let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
        let mob_name = w.mob_protos.get(&vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());
        let mob_room = m.in_room;
        if mob_room != me.current_room {
            return CmdOutput::text("\r\nYour target is no longer here.\r\n");
        }
        let saved = hit && save_vs_spell(me.level, target_level);
        let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
        let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
        let dead = if hit { m.hp -= dmg; m.hp <= 0 } else { false };
        (mob_name, vnum, dead, mob_room, saved, dmg)
    };

    let (to_me, to_room) = if hit && saved {
        (
            format!("\r\nYou grasp {mob_name} with a shocking jolt for {dmg} damage (partial resist)!\r\n"),
            format!("{} grasps {mob_name} with a shocking jolt.\r\n", me.name),
        )
    } else if hit {
        (
            format!("\r\nYou grasp {mob_name} and electricity surges for {dmg} damage!\r\n"),
            format!("{} grasps {mob_name} — electricity surges!\r\n", me.name),
        )
    } else {
        (
            format!("\r\nYour shocking grasp fizzles against {mob_name}.\r\n"),
            format!("{}'s shocking grasp fizzles against {mob_name}.\r\n", me.name),
        )
    };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    if mob_dead {
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
                .map(mob_corpse_contents).unwrap_or_default();
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
    let base_dmg = dice(3, 8) + me.level + (me.wis - 10).max(0) / 2;
    me.mana -= crate::character::Skill::Harm.mana_cost();

    let (mob_name, killed_vnum, mob_dead, mob_room, saved, dmg) = {
        let mut w = world.lock().await;
        let (vnum, in_room) = match w.mob_instances.iter().find(|m| m.id == mob_id) {
            Some(m) => (m.vnum, m.in_room),
            None    => return CmdOutput::text("\r\nYour target has vanished.\r\n"),
        };
        let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
        let mob_name = w.mob_protos.get(&vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());
        if in_room != me.current_room {
            return CmdOutput::text("\r\nYour target is no longer here.\r\n");
        }
        let saved = hit && save_vs_spell(me.level, target_level);
        let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
        let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
        if me.fighting.is_none() {
            me.fighting = Some(Target { id: mob_id, is_player: false });
            m.fighting = Some(Target { id: me.id, is_player: true });
        }
        let dead = if hit { m.hp -= dmg; m.hp <= 0 } else { false };
        (mob_name, vnum, dead, in_room, saved, dmg)
    };

    let (to_me, to_room) = if hit && saved {
        (
            format!("\r\nYou call down divine wrath upon {mob_name}! ({dmg} damage, partial resist)\r\n"),
            format!("{} calls down divine wrath upon {mob_name}, who endures it.\r\n", me.name),
        )
    } else if hit {
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
                .map(mob_corpse_contents).unwrap_or_default();
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
    // Mob save vs poison.  Higher-level mobs resist; lower-level mobs
    // succumb more readily.
    let target_level = {
        let w = world.lock().await;
        w.mob_instances.iter().find(|m| m.id == mid)
            .and_then(|m| w.mob_protos.get(&m.vnum))
            .map(|p| p.level).unwrap_or(1)
    };
    if save_vs_spell(me.level, target_level) {
        let mob_name = {
            let w = world.lock().await;
            w.mob_instances.iter().find(|m| m.id == mid)
                .and_then(|m| w.mob_protos.get(&m.vnum))
                .map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "the creature".to_string())
        };
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{mob_name} shakes off the spell.\r\n"));
        return CmdOutput::text(format!("\r\n{mob_name} shakes off the spell.\r\n"));
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

/// Roll a saving throw for a mob target against a spellcasting player.
/// Returns true if the mob shrugged it off (caller bails before
/// applying the affect).  Formula: `save% = (50 - (caster - target)*5).clamp(5, 95)` — equal levels → 50% save; +10 caster
/// level pushes save down to 0% (clamped to 5%); +10 target level
/// pushes save up to 100% (clamped to 95%).
fn save_vs_spell(caster_level: i32, target_level: i32) -> bool {
    use rand::Rng;
    let save_pct = (50 - (caster_level - target_level) * 5).clamp(5, 95);
    rand::thread_rng().gen_range(0..100) < save_pct
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
    // Mob save against the debuff.
    let target_level = {
        let w = world.lock().await;
        w.mob_instances.iter().find(|m| m.id == mid)
            .and_then(|m| w.mob_protos.get(&m.vnum))
            .map(|p| p.level).unwrap_or(1)
    };
    if save_vs_spell(me.level, target_level) {
        let mob_name = {
            let w = world.lock().await;
            w.mob_instances.iter().find(|m| m.id == mid)
                .and_then(|m| w.mob_protos.get(&m.vnum))
                .map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "the creature".to_string())
        };
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{mob_name} shakes off the spell.\r\n"));
        return CmdOutput::text(format!("\r\n{mob_name} shakes off the spell.\r\n"));
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
    let to_ac = 0;
    let mob_name = {
        let mut w = world.lock().await;
        let aff = crate::character::Affect {
            skill, duration,
            to_hit: 0, to_dam: 0, dmg_reduction: 0, dot_damage: 0, to_ac,
        };
        let vnum = {
            let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mid) else {
                return CmdOutput::text("\r\nYour target is gone.\r\n");
            };
            m.apply_affect(aff);
            if skill == crate::character::Skill::CharmPerson {
                m.charmer = Some(me.id);
            }
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

/// Shared shape for the mid/heavy Cleric heal ladder (cure serious
/// wounds, heal).  Caller picks the skill; the handler picks dice and
/// flavor per skill.  Mirrors `cast_cure_critic`'s target resolution.
async fn cast_heal_spell(
    target_kw: &str,
    me: &mut Character,
    chars: &SharedChars,
    learned: u8,
    skill:   crate::character::Skill,
) -> CmdOutput {
    use rand::Rng;
    me.mana -= skill.mana_cost();
    let hit_chance = (90 + learned as i32 / 5).min(99);
    if rand::thread_rng().gen_range(0..100) >= hit_chance {
        return CmdOutput::text("\r\nThe healing prayer fizzles.\r\n");
    }
    let (heal, self_room, target_room, self_self, peer_msg) = match skill {
        crate::character::Skill::CureSerious => (
            crate::db::dice(4, 10) + me.level + (me.wis - 10).max(0) / 2,
            "{} chants softly, bathed in warm light.",
            "{} chants softly, bathed in warm light around {tgt}.",
            "Serious wounds knit closed across your body. ({}/{} HP)",
            "{} chants softly — your serious wounds knit closed. ({}/{} HP)",
        ),
        crate::character::Skill::Heal => (
            crate::db::dice(20, 10) + me.level * 2,
            "{} is enveloped by a brilliant golden aura.",
            "{} is enveloped by a brilliant golden aura that flows into {tgt}.",
            "A brilliant golden aura mends every wound. ({}/{} HP)",
            "{} envelops you in a golden aura — every wound mends. ({}/{} HP)",
        ),
        _ => return CmdOutput::text("\r\nUnsupported healing spell.\r\n"),
    };
    // Self?
    if target_kw.is_empty() {
        me.hp = (me.hp + heal).min(me.max_hp);
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &self_room.replace("{}", &me.name));
        return CmdOutput::text(format!(
            "\r\n{}\r\n", self_self.replacen("{}", &me.hp.to_string(), 1)
                                    .replacen("{}", &me.max_hp.to_string(), 1),
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
    let line = peer_msg.replacen("{}", &me.name, 1)
        .replacen("{}", &new_hp.to_string(), 1)
        .replacen("{}", &max.to_string(), 1);
    let _ = ph.send.send(format!("\r\n{}\r\n", line));
    let cl = chars.lock().await;
    let room_line = target_room.replacen("{}", &me.name, 1)
        .replace("{tgt}", &ph.name);
    cl.broadcast_room(me.current_room, Some(me.id), &format!("{}\r\n", room_line));
    CmdOutput::text(format!(
        "\r\nYour healing magic restores {} ({} HP).\r\n", ph.name, heal,
    ))
}

/// `restoration` — Cleric apex utility.  Fully heals HP/mana/movement
/// and strips every "negative" affect (poison, sleep, blind, slow,
/// charm) from a self or named same-room player.  Drains 80 mana.
async fn cast_restoration(
    target_kw: &str,
    me: &mut Character,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::character::Skill;
    me.mana -= Skill::Restoration.mana_cost();
    let bad = [
        Skill::Poison, Skill::Sleep, Skill::Blindness,
        Skill::Slow,   Skill::CharmPerson,
    ];
    // Self path.
    if target_kw.is_empty() {
        me.hp       = me.max_hp;
        me.mana     = me.max_mana;
        me.movement = me.max_movement;
        me.affects.retain(|a| !bad.contains(&a.skill));
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!(
                "{} is wreathed in a blinding pillar of restorative light.\r\n",
                me.name,
            ));
        return CmdOutput::text(
            "\r\nA blinding pillar of light cleanses every wound, weariness, and curse.\r\n"
                .to_string(),
        );
    }
    // Same-room player target.
    let target_handle = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p|
            p.current_room == me.current_room
            && p.name.eq_ignore_ascii_case(target_kw)).cloned();
        h
    };
    let Some(ph) = target_handle else {
        return CmdOutput::text(format!(
            "\r\nNo one named '{target_kw}' is here.\r\n"
        ));
    };
    {
        let mut c = ph.character.lock().await;
        c.hp       = c.max_hp;
        c.mana     = c.max_mana;
        c.movement = c.max_movement;
        c.affects.retain(|a| !bad.contains(&a.skill));
    }
    let _ = ph.send.send(format!(
        "\r\n{} wreathes you in a blinding pillar of restorative light.\r\n",
        me.name,
    ));
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!(
            "{} wreathes {} in a blinding pillar of restorative light.\r\n",
            me.name, ph.name,
        ));
    CmdOutput::text(format!(
        "\r\nYour restorative magic floods {}.\r\n", ph.name,
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
        crate::character::Skill::Invisibility => (
            24 + learned as i32 / 4,
            0, 0,
            "You vanish.\r\n",
            "{} slowly fades from view.\r\n",
        ),
        crate::character::Skill::Stoneskin => (
            4 + learned as i32 / 10,
            0, 30,
            "Your skin hardens to the texture of granite.\r\n",
            "{}'s skin takes on the texture of stone.\r\n",
        ),
        crate::character::Skill::Fly => (
            10 + learned as i32 / 5,
            0, 0,
            "Your feet lift gently from the ground — you can fly!\r\n",
            "{} rises gently off the ground, hovering in the air.\r\n",
        ),
        crate::character::Skill::ProtFromEvil => (
            24,
            0, 8,
            "You feel invulnerable to the forces of evil!\r\n",
            "{} is surrounded by a faint white aura.\r\n",
        ),
        crate::character::Skill::Waterwalk => (
            24,
            0, 0,
            "You feel webbing between your toes.\r\n",
            "{}'s feet take on a webbed appearance.\r\n",
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

/// `cast locate-object <keyword>` — sweep `world.obj_instances` for the
/// keyword and report each match's location.  Caps at 10 hits so a
/// search for "a" doesn't pour a flood of lines into the player's
/// terminal.  Hit% scales with learned (50 + learned/2 clamped 95).
async fn cast_locate_object(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use rand::Rng;
    me.mana -= crate::character::Skill::LocateObject.mana_cost();
    let kw = target_kw.trim();
    if kw.is_empty() {
        return CmdOutput::text("\r\nLocate what?\r\n".to_string());
    }
    let hit_chance = (50 + me.skills.get(&crate::character::Skill::LocateObject)
        .copied().unwrap_or(0) as i32 / 2).min(95);
    if rand::thread_rng().gen_range(0..100) >= hit_chance {
        return CmdOutput::text(format!("\r\nYou sense nothing called '{kw}'.\r\n"));
    }
    let needle = kw.to_ascii_lowercase();
    let lines: Vec<String> = {
        let w = world.lock().await;
        let mut out = Vec::new();
        for o in &w.obj_instances {
            if out.len() >= 10 { break; }
            let Some(p) = w.obj_protos.get(&o.vnum) else { continue; };
            if !p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&needle))
                && !p.short_description.to_ascii_lowercase().contains(&needle)
            { continue; }
            // Where is it?
            if o.in_room != crate::world::NOWHERE {
                let rname = w.rooms.get(&o.in_room).map(|r| r.name.as_str())
                    .unwrap_or("(nowhere)");
                out.push(format!(
                    "  {} — in [{}] {}",
                    p.short_description, o.in_room, rname,
                ));
            } else {
                // Find the mob carrying this iid.
                let carrier = w.mob_instances.iter()
                    .find(|m| m.inventory.contains(&o.id))
                    .and_then(|m| w.mob_protos.get(&m.vnum).map(|p| p.short_descr.clone()));
                if let Some(carrier_name) = carrier {
                    out.push(format!(
                        "  {} — carried by {carrier_name}",
                        p.short_description,
                    ));
                } else {
                    out.push(format!(
                        "  {} — somewhere unseen", p.short_description,
                    ));
                }
            }
        }
        out
    };
    // Also walk the chars registry for inventory/equipment.  We do this
    // after dropping the world lock so the per-Character locks don't
    // serialize on it.
    let mut player_lines: Vec<String> = Vec::new();
    if lines.len() < 10 {
        let handles: Vec<crate::character::PlayerHandle> = {
            let cl = chars.lock().await;
            cl.iter().cloned().collect()
        };
        for ph in handles {
            if player_lines.len() + lines.len() >= 10 { break; }
            let c = ph.character.lock().await;
            let inv = c.inventory.iter().copied()
                .chain(c.equipment.iter().filter_map(|s| *s));
            let w = world.lock().await;
            for iid in inv {
                if let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) {
                    if let Some(p) = w.obj_protos.get(&o.vnum) {
                        if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&needle))
                            || p.short_description.to_ascii_lowercase().contains(&needle)
                        {
                            player_lines.push(format!(
                                "  {} — carried by {}",
                                p.short_description, ph.name,
                            ));
                            if player_lines.len() + lines.len() >= 10 { break; }
                        }
                    }
                }
            }
        }
    }
    if lines.is_empty() && player_lines.is_empty() {
        return CmdOutput::text(format!("\r\nYou sense nothing called '{kw}'.\r\n"));
    }
    let mut s = String::from("\r\nYour mind reaches out and senses:\r\n");
    for l in &lines { s.push_str(l); s.push_str("\r\n"); }
    for l in &player_lines { s.push_str(l); s.push_str("\r\n"); }
    CmdOutput::text(s)
}

/// `cast refresh [target]` — restore `dice(2,8) + level` mana to the
/// caster (no arg) or to a named player in the same room.
async fn cast_refresh(
    target_kw: &str,
    me: &mut Character,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::db::dice;
    me.mana -= crate::character::Skill::Refresh.mana_cost();
    let gain = dice(2, 8) + me.level;
    if target_kw.is_empty() {
        me.mana = (me.mana + gain).min(me.max_mana);
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} pauses, focusing inward.\r\n", me.name));
        return CmdOutput::text(format!(
            "\r\nClarity floods your mind. ({}/{} mana)\r\n",
            me.mana, me.max_mana,
        ));
    }
    let target_ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p|
            p.current_room == me.current_room
            && p.name.eq_ignore_ascii_case(target_kw)).cloned();
        h
    };
    let Some(ph) = target_ph else {
        return CmdOutput::text(format!("\r\nNo one named '{target_kw}' is here.\r\n"));
    };
    let (new_mana, max) = {
        let mut c = ph.character.lock().await;
        c.mana = (c.mana + gain).min(c.max_mana);
        (c.mana, c.max_mana)
    };
    let _ = ph.send.send(format!(
        "\r\n{} focuses, and clarity floods your mind. ({}/{} mana)\r\n",
        me.name, new_mana, max,
    ));
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} pauses, focusing on {}.\r\n", me.name, ph.name));
    CmdOutput::text(format!(
        "\r\nYou restore {} mana to {}.\r\n", gain, ph.name,
    ))
}

/// `cast summon <player>` — yank a named online player into the
/// caster's room.  Refuses if the target is too powerful
/// (target.level > caster.level + 5) with "They resist the call."
async fn cast_summon(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    me.mana -= crate::character::Skill::Summon.mana_cost();
    if target_kw.is_empty() {
        return CmdOutput::text("\r\nSummon whom?\r\n".to_string());
    }
    let ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p|
            p.id != me.id && p.name.eq_ignore_ascii_case(target_kw)).cloned();
        h
    };
    let Some(ph) = ph else {
        return CmdOutput::text("\r\nNobody by that name is online.\r\n".to_string());
    };
    if ph.level > me.level + 5 {
        return CmdOutput::text(format!("\r\n{} resists the call.\r\n", ph.name));
    }
    // Respect the target's nosummon preference.
    {
        let blocked = { ph.character.lock().await.nosummon };
        if blocked {
            let _ = ph.send.send(format!("\r\n{} tried to summon you.\r\n", me.name));
            return CmdOutput::text(format!("\r\n{} is protected from summon.\r\n", ph.name));
        }
    }
    let to_room = me.current_room;
    let from_room = {
        let mut c = ph.character.lock().await;
        let f = c.current_room;
        if f == to_room {
            let _ = ph.send.send(format!("\r\n{} mutters about you.\r\n", me.name));
            return CmdOutput::text(format!("\r\n{} is already here.\r\n", ph.name));
        }
        c.current_room = to_room;
        f
    };
    {
        let mut cl = chars.lock().await;
        cl.update_room(ph.id, to_room);
        cl.broadcast_room(from_room, Some(ph.id),
            &format!("{} disappears in a flash of magic.\r\n", ph.name));
        cl.broadcast_room(to_room, Some(ph.id),
            &format!("{} appears, summoned by {}.\r\n", ph.name, me.name));
    }
    let _ = ph.send.send(format!("\r\n{} summons you!\r\n", me.name));
    let view = render_room(to_room, Some(ph.id), world, chars).await;
    let _ = ph.send.send(view);
    CmdOutput::text(format!("\r\nYou summon {} to your side.\r\n", ph.name))
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
    // Wear positions allowed.
    let mut slots: Vec<&str> = Vec::new();
    use crate::character::*;
    if p.wear_flags[0] & ITEM_WEAR_FINGER != 0 { slots.push("finger"); }
    if p.wear_flags[0] & ITEM_WEAR_NECK   != 0 { slots.push("neck"); }
    if p.wear_flags[0] & ITEM_WEAR_BODY   != 0 { slots.push("body"); }
    if p.wear_flags[0] & ITEM_WEAR_HEAD   != 0 { slots.push("head"); }
    if p.wear_flags[0] & ITEM_WEAR_LEGS   != 0 { slots.push("legs"); }
    if p.wear_flags[0] & ITEM_WEAR_FEET   != 0 { slots.push("feet"); }
    if p.wear_flags[0] & ITEM_WEAR_HANDS  != 0 { slots.push("hands"); }
    if p.wear_flags[0] & ITEM_WEAR_ARMS   != 0 { slots.push("arms"); }
    if p.wear_flags[0] & ITEM_WEAR_SHIELD != 0 { slots.push("shield"); }
    if p.wear_flags[0] & ITEM_WEAR_ABOUT  != 0 { slots.push("about"); }
    if p.wear_flags[0] & ITEM_WEAR_WAIST  != 0 { slots.push("waist"); }
    if p.wear_flags[0] & ITEM_WEAR_WRIST  != 0 { slots.push("wrist"); }
    if p.wear_flags[0] & ITEM_WEAR_WIELD  != 0 { slots.push("wield"); }
    if p.wear_flags[0] & ITEM_WEAR_HOLD   != 0 { slots.push("hold"); }
    if !slots.is_empty() {
        s.push_str(&format!("  wearable:  {}\r\n", slots.join(", ")));
    }
    // Affects (cp36 proto + cp177 per-instance enchantments).
    if !p.affected.is_empty() || !obj.bonus_affects.is_empty() {
        s.push_str("  affects:\r\n");
        for a in &p.affected {
            let name = apply_name(a.location);
            let sign = if a.modifier >= 0 { "+" } else { "" };
            s.push_str(&format!("    {} by {sign}{}\r\n", name, a.modifier));
        }
        for a in &obj.bonus_affects {
            let name = apply_name(a.location);
            let sign = if a.modifier >= 0 { "+" } else { "" };
            s.push_str(&format!("    {} by {sign}{} (enchant)\r\n", name, a.modifier));
        }
    }
    // Anti-class / anti-alignment hints (cp124/125).
    let xf = p.extra_flags[0];
    let mut anti: Vec<&str> = Vec::new();
    use crate::world::*;
    if xf & ITEM_ANTI_GOOD       != 0 { anti.push("good"); }
    if xf & ITEM_ANTI_EVIL       != 0 { anti.push("evil"); }
    if xf & ITEM_ANTI_NEUTRAL    != 0 { anti.push("neutral"); }
    if xf & ITEM_ANTI_WARRIOR    != 0 { anti.push("warrior"); }
    if xf & ITEM_ANTI_CLERIC     != 0 { anti.push("cleric"); }
    if xf & ITEM_ANTI_THIEF      != 0 { anti.push("thief"); }
    if xf & ITEM_ANTI_MAGIC_USER != 0 { anti.push("magic-user"); }
    if !anti.is_empty() {
        s.push_str(&format!("  forbidden: {}\r\n", anti.join(", ")));
    }
    if xf & ITEM_2H_WEAPON != 0 {
        s.push_str("  two-handed: yes\r\n");
    }
    CmdOutput::text(s)
}

async fn cast_word_of_recall(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    // Cooldown gate (immortals bypass — they have goto anyway).
    if me.level < 34 {
        if let Some(until) = me.recall_cooldown_until {
            let now = std::time::Instant::now();
            if now < until {
                let secs_left = (until - now).as_secs();
                me.mana -= crate::character::Skill::WordOfRecall.mana_cost();
                return CmdOutput::text(format!(
                    "The holy magic has not yet recharged — {}s remain.\r\n",
                    secs_left,
                ));
            }
        }
    }
    let from_room = me.current_room;
    let target = {
        let w = world.lock().await;
        w.start_room(me.level >= 34)
    };
    me.mana -= crate::character::Skill::WordOfRecall.mana_cost();
    if me.level < 34 {
        const RECALL_COOLDOWN_SECS: u64 = 300;
        me.recall_cooldown_until = Some(
            std::time::Instant::now()
                + std::time::Duration::from_secs(RECALL_COOLDOWN_SECS),
        );
    }
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

/// `color spray` — AoE MU damage spell.  Hits every mob in the
/// caster's room with `dice(2, 6) + level/2 + INT-bonus`; per-target
/// save halves damage AND skips the blindness rider.  On a non-saved
/// hit, applies `Affect { skill: Blindness, duration: 4, .. }`.
async fn cast_color_spray(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    learned: u8,
) -> CmdOutput {
    use rand::Rng;
    use crate::db::dice;

    let mob_ids: Vec<u32> = {
        let w = world.lock().await;
        w.rooms.get(&me.current_room)
            .map(|r| r.mobs.clone())
            .unwrap_or_default()
    };
    if mob_ids.is_empty() {
        return CmdOutput::text("\r\nA shimmering spray of colors flares against nothing.\r\n");
    }
    me.mana -= crate::character::Skill::ColorSpray.mana_cost();

    let mut to_me = String::from("\r\nA shimmering spray of colors erupts from your hands!\r\n");
    let to_room = format!("{} releases a shimmering spray of colors!\r\n", me.name);
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }

    for mob_id in mob_ids {
        let hit_chance = (70 + learned as i32 / 4).min(95);
        if rand::thread_rng().gen_range(0..100) >= hit_chance { continue; }
        let base_dmg = dice(2, 6) + me.level / 2
            + crate::character::str_damage_bonus(me.int_);

        let (mob_name, dmg, saved) = {
            let mut w = world.lock().await;
            let (vnum, in_room) = match w.mob_instances.iter().find(|m| m.id == mob_id) {
                Some(m) => (m.vnum, m.in_room),
                None => continue,
            };
            if in_room != me.current_room { continue; }
            let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
            let mob_name = w.mob_protos.get(&vnum)
                .map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "the creature".into());
            let saved = save_vs_spell(me.level, target_level);
            let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
            let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
            m.hp -= dmg;
            // Blindness rider on a non-saved hit.
            if !saved && !m.affects.iter().any(|a| a.skill == crate::character::Skill::Blindness) {
                m.apply_affect(crate::character::Affect {
                    skill:         crate::character::Skill::Blindness,
                    duration:      4,
                    to_hit:        0,
                    to_dam:        0,
                    dmg_reduction: 0,
                    dot_damage:    0,
                    to_ac:         0,
                });
            }
            if me.fighting.is_none() {
                me.fighting = Some(Target { id: mob_id, is_player: false });
            }
            if m.fighting.is_none() {
                m.fighting = Some(Target { id: me.id, is_player: true });
            }
            (mob_name, dmg, saved)
        };
        if saved {
            to_me.push_str(&format!("Colors dazzle {mob_name} for {dmg} damage (partial resist).\r\n"));
        } else {
            to_me.push_str(&format!("Colors dazzle {mob_name} for {dmg} damage — it staggers, blinded!\r\n"));
        }
    }
    CmdOutput::text(to_me)
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
        let base_dmg = dice(2, 4) + me.level / 2
            + crate::character::str_damage_bonus(me.int_);

        let (mob_name, mob_dead, mob_room, dmg, saved) = {
            let mut w = world.lock().await;
            let (vnum, in_room) = match w.mob_instances.iter().find(|m| m.id == mob_id) {
                Some(m) => (m.vnum, m.in_room),
                None => continue,
            };
            if in_room != me.current_room { continue; }
            let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
            let mob_name = w.mob_protos.get(&vnum)
                .map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "the creature".into());
            let saved = save_vs_spell(me.level, target_level);
            let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
            let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
            m.hp -= dmg;
            if me.fighting.is_none() {
                me.fighting = Some(Target { id: mob_id, is_player: false });
            }
            if m.fighting.is_none() {
                m.fighting = Some(Target { id: me.id, is_player: true });
            }
            (mob_name, m.hp <= 0, in_room, dmg, saved)
        };
        if saved {
            to_me.push_str(&format!("Flames sear {mob_name} for {dmg} damage (partial resist).\r\n"));
        } else {
            to_me.push_str(&format!("Flames sear {mob_name} for {dmg} damage!\r\n"));
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
            total_xp += xp;
            {
                let mut w = world.lock().await;
                let inv: Vec<u32> = w.mob_instances.iter()
                    .find(|m| m.id == mob_id)
                    .map(mob_corpse_contents).unwrap_or_default();
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
        let base_dmg = dice(2, 8) + me.level + (me.wis - 10).max(0) / 2;
        let (mob_name, mob_dead, mob_room, dmg, saved) = {
            let mut w = world.lock().await;
            let (vnum, in_room) = match w.mob_instances.iter().find(|m| m.id == mob_id) {
                Some(m) => (m.vnum, m.in_room),
                None => continue,
            };
            if in_room != me.current_room { continue; }
            let target_level = w.mob_protos.get(&vnum).map(|p| p.level).unwrap_or(1);
            let mob_name = w.mob_protos.get(&vnum)
                .map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "the creature".into());
            let saved = save_vs_spell(me.level, target_level);
            let dmg = if saved { (base_dmg / 2).max(1) } else { base_dmg };
            let m = w.mob_instances.iter_mut().find(|m| m.id == mob_id).unwrap();
            m.hp -= dmg;
            if me.fighting.is_none() {
                me.fighting = Some(Target { id: mob_id, is_player: false });
            }
            if m.fighting.is_none() {
                m.fighting = Some(Target { id: me.id, is_player: true });
            }
            (mob_name, m.hp <= 0, in_room, dmg, saved)
        };
        if saved {
            to_me.push_str(&format!("The shockwave hits {mob_name} for {dmg} damage (partial resist).\r\n"));
        } else {
            to_me.push_str(&format!("The shockwave hits {mob_name} for {dmg} damage!\r\n"));
        }

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
                    .map(mob_corpse_contents).unwrap_or_default();
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

/// `whirlwind` — Warrior melee skill that strikes every mob in the room.
/// Per-mob hit roll keyed off the learned percentage; damage is the
/// warrior's melee roll (weapon-independent baseline + STR + damroll).
async fn do_whirlwind(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use rand::Rng;
    use crate::db::dice;

    let skill = Skill::Whirlwind;
    if !skill.is_class_allowed(me.class) {
        return CmdOutput::text("\r\nYou do not know how to whirlwind.\r\n".to_string());
    }
    let learned = *me.skills.get(&skill).unwrap_or(&0);
    if learned == 0 {
        return CmdOutput::text(
            "\r\nYou are unfamiliar with the whirlwind attack. Try `practice whirlwind`.\r\n"
                .to_string(),
        );
    }
    me.reveal();

    let mob_ids: Vec<u32> = {
        let w = world.lock().await;
        w.rooms.get(&me.current_room).map(|r| r.mobs.clone()).unwrap_or_default()
    };

    let to_room = format!("{} spins in a deadly whirlwind of steel!\r\n", me.name);
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id), &to_room);
    }
    let mut to_me = String::from("\r\nYou spin in a deadly whirlwind, striking all around you!\r\n");
    if mob_ids.is_empty() {
        to_me.push_str("There is no one here to strike.\r\n");
        return CmdOutput::text(to_me);
    }

    let hit_chance = (40 + learned as i32 / 2).min(85);
    let str_b = crate::character::str_damage_bonus(me.str_);
    let damroll = me.bonus_damroll;

    let mut total_xp = 0i64;
    let mut killed_names: Vec<String> = Vec::new();
    let mut landed_any = false;
    for mob_id in mob_ids {
        let hit = rand::thread_rng().gen_range(0..100) < hit_chance;
        let base_dmg = (dice(1, 8) + me.level / 2 + str_b + damroll).max(1);
        let (mob_name, mob_dead, mob_room, dmg) = {
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
            if me.fighting.is_none() {
                me.fighting = Some(Target { id: mob_id, is_player: false });
            }
            if m.fighting.is_none() {
                m.fighting = Some(Target { id: me.id, is_player: true });
            }
            let dmg = if hit { m.hp -= base_dmg; base_dmg } else { 0 };
            (mob_name, hit && m.hp <= 0, in_room, dmg)
        };
        if hit {
            landed_any = true;
            to_me.push_str(&format!("Your whirlwind strikes {mob_name} for {dmg} damage!\r\n"));
        } else {
            to_me.push_str(&format!("Your whirlwind misses {mob_name}.\r\n"));
        }

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
                    .map(mob_corpse_contents).unwrap_or_default();
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
                    &format!("{mob_name} is cut down by {}'s whirlwind.\r\n", me.name));
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
    if landed_any {
        if let Some(bump) = learn_attempt(me, skill, 5) {
            to_me.push_str(&bump);
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
        let mob_inv = mob.map(mob_corpse_contents).unwrap_or_default();
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
    arg: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    if me.fighting.is_none() {
        return CmdOutput::text("\r\nYou are not fighting anyone.\r\n");
    }
    // If the player typed `flee <dir>`, try that direction first.  Else
    // pick a random valid exit (original behavior).
    let preferred_dir = Direction::parse(arg.trim());
    let target = {
        let w = world.lock().await;
        let r = match w.rooms.get(&me.current_room) {
            Some(r) => r,
            None => return CmdOutput::text("\r\nYou are nowhere.\r\n"),
        };
        // Use the preferred direction if it's a valid open exit.
        if let Some(d) = preferred_dir {
            if let Some(e) = r.exits[d as usize].as_ref() {
                let closed = e.exit_info & crate::world::EX_CLOSED != 0;
                if e.to_room != crate::world::NOWHERE
                    && !closed
                    && w.rooms.contains_key(&e.to_room)
                {
                    Some((d, e.to_room))
                } else { None }
            } else { None }
        } else {
            let candidates: Vec<(Direction, RoomVnum)> = Direction::ALL.iter()
                .filter_map(|d| {
                    r.exits[*d as usize].as_ref().and_then(|e| {
                        if e.to_room != crate::world::NOWHERE
                            && (e.exit_info & crate::world::EX_CLOSED == 0)
                            && w.rooms.contains_key(&e.to_room)
                        {
                            Some((*d, e.to_room))
                        } else { None }
                    })
                })
                .collect();
            candidates.choose(&mut rand::thread_rng()).copied()
        }
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
    let door = find_door_target(&w, me.current_room, arg);
    // No matching door?  Try a container by the same keyword (cp215).
    let container = if door.is_none() {
        find_container(&w, me, arg)
    } else { None };
    drop(w);
    let Some((dir, info, key_vnum, keyword, _to)) = door else {
        if let Some((iid, short)) = container {
            return do_container_door(iid, short, me, world, chars, op).await;
        }
        return CmdOutput::text(format!("\r\nYou see no such door or container here.\r\n"));
    };
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

/// Open/close/lock/unlock applied to an ITEM_CONTAINER (cp215).  Container
/// state lives in the prototype's `value[1]` (CONT_* bits) — shared across
/// instances, consistent with our drink-container/wand handling; `value[2]`
/// is the key vnum.  Gates put/get on the open state are in `do_put` /
/// `do_get_from_container`.
async fn do_container_door(
    iid: u32,
    short: String,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
    op: DoorOp,
) -> CmdOutput {
    use crate::world::{CONT_CLOSEABLE, CONT_CLOSED, CONT_LOCKED};
    let verb = match op {
        DoorOp::Open => "open", DoorOp::Close => "close",
        DoorOp::Lock => "lock", DoorOp::Unlock => "unlock",
    };
    // Snapshot the container's flags + key vnum.
    let (vnum, flags, key_vnum) = {
        let w = world.lock().await;
        let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else {
            return CmdOutput::text("\r\nIt's gone now.\r\n".to_string());
        };
        let Some(p) = w.obj_protos.get(&o.vnum) else {
            return CmdOutput::text("\r\nIt's gone now.\r\n".to_string());
        };
        (o.vnum, p.value[1], p.value[2])
    };
    if (flags & CONT_CLOSEABLE) == 0 {
        return CmdOutput::text(format!("\r\n{short} can't be {verb}ed.\r\n"));
    }
    let closed = (flags & CONT_CLOSED) != 0;
    let locked = (flags & CONT_LOCKED) != 0;

    let (set, clear) = match op {
        DoorOp::Open => {
            if !closed { return CmdOutput::text(format!("\r\n{short} is already open.\r\n")); }
            if locked  { return CmdOutput::text(format!("\r\n{short} is locked.\r\n")); }
            (0, CONT_CLOSED)
        }
        DoorOp::Close => {
            if closed { return CmdOutput::text(format!("\r\n{short} is already closed.\r\n")); }
            (CONT_CLOSED, 0)
        }
        DoorOp::Unlock => {
            if !closed { return CmdOutput::text(format!("\r\n{short} isn't even closed.\r\n")); }
            if !locked { return CmdOutput::text(format!("\r\n{short} is already unlocked.\r\n")); }
            if !player_has_key(me, key_vnum, world).await {
                return CmdOutput::text("\r\nYou don't have the key.\r\n".to_string());
            }
            (0, CONT_LOCKED)
        }
        DoorOp::Lock => {
            if !closed { return CmdOutput::text(format!("\r\nYou'll need to close {short} first.\r\n")); }
            if locked  { return CmdOutput::text(format!("\r\n{short} is already locked.\r\n")); }
            if !player_has_key(me, key_vnum, world).await {
                return CmdOutput::text("\r\nYou don't have the key.\r\n".to_string());
            }
            (CONT_LOCKED, 0)
        }
    };
    {
        let mut w = world.lock().await;
        if let Some(p) = w.obj_protos.get_mut(&vnum) {
            p.value[1] = (p.value[1] & !clear) | set;
        }
    }
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} {verb}s {short}.\r\n", me.name));
    }
    CmdOutput::text(format!("\r\nYou {verb} {short}.\r\n"))
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
    let door = find_door_target(&w, me.current_room, arg);
    // No matching door?  Try picking a container's lock (cp216).
    let container = if door.is_none() { find_container(&w, me, arg) } else { None };
    drop(w);
    let Some((dir, info, _key, keyword, _to)) = door else {
        if let Some((iid, short)) = container {
            return do_pick_container(iid, short, me, world, chars).await;
        }
        return CmdOutput::text("\r\nYou see no such door or container here.\r\n".to_string());
    };
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

/// Pick the lock on an ITEM_CONTAINER (cp216).  Mirrors the door path:
/// requires the container be closed + locked + not pickproof, rolls the
/// PickLock skill, and clears CONT_LOCKED on the prototype's `value[1]`.
async fn do_pick_container(
    iid: u32,
    short: String,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::world::{CONT_CLOSED, CONT_LOCKED, CONT_PICKPROOF};
    use rand::Rng;
    let (vnum, flags) = {
        let w = world.lock().await;
        let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else {
            return CmdOutput::text("\r\nIt's gone now.\r\n".to_string());
        };
        let Some(p) = w.obj_protos.get(&o.vnum) else {
            return CmdOutput::text("\r\nIt's gone now.\r\n".to_string());
        };
        (o.vnum, p.value[1])
    };
    if (flags & CONT_CLOSED) == 0 {
        return CmdOutput::text(format!("\r\n{short} isn't even closed.\r\n"));
    }
    if (flags & CONT_LOCKED) == 0 {
        return CmdOutput::text(format!("\r\n{short} wasn't locked after all.\r\n"));
    }
    if (flags & CONT_PICKPROOF) != 0 {
        return CmdOutput::text(format!("\r\n{short} resists your attempts to pick it.\r\n"));
    }
    let learned = *me.skills.get(&Skill::PickLock).unwrap_or(&0);
    let roll = rand::thread_rng().gen_range(0..100);
    if roll >= learned as i32 {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} fumbles at {short} with a set of picks.\r\n", me.name));
        return CmdOutput::text(format!("\r\nYou fumble at the lock on {short}.\r\n"));
    }
    {
        let mut w = world.lock().await;
        if let Some(p) = w.obj_protos.get_mut(&vnum) {
            p.value[1] &= !CONT_LOCKED;
        }
    }
    {
        let cl = chars.lock().await;
        cl.broadcast_room(me.current_room, Some(me.id),
            &format!("{} picks the lock on {short}.\r\n", me.name));
    }
    let mut out = format!("\r\nThe lock on {short} clicks open.\r\n");
    if let Some(bump) = learn_attempt(me, Skill::PickLock, 8) { out.push_str(&bump); }
    CmdOutput::text(out)
}

/// True if `key_vnum` is non-negative and the player has an instance of
/// that vnum in their inventory.
async fn player_has_key(me: &Character, key_vnum: i32, world: &Arc<Mutex<World>>) -> bool {
    if key_vnum < 0 { return false; }
    let w = world.lock().await;
    // Held directly, OR nested one level inside a carried container — so a
    // bag of keys acts as a keyring (cp238).
    me.inventory.iter().any(|&iid| {
        let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else { return false; };
        if o.vnum == key_vnum { return true; }
        o.contents.iter().any(|&cid| {
            w.obj_instances.iter().find(|c| c.id == cid)
                .map(|c| c.vnum == key_vnum)
                .unwrap_or(false)
        })
    })
}

// ---------------------------------------------------------------------------
// Potions & scrolls
// ---------------------------------------------------------------------------

/// Cast a spell identified by its CircleMUD spell vnum at `target_kw`,
/// returning the spell's text output.  Used by drink/recite (and a
/// natural extension point if we ever add wand/staff later).  Saves and
/// restores `me.mana` so consumable casts don't drain it.
/// Friendly label for a CircleMUD-ish spell vnum (the inverse of
/// `skill_to_potion_vnum`).  Returns None for unknown vnums.
fn spell_vnum_to_label(vnum: i32) -> Option<&'static str> {
    match vnum {
        3  => Some("bless"),
        5  => Some("burning hands"),
        16 => Some("cure light"),
        19 => Some("detect invisibility"),
        20 => Some("detect magic"),
        27 => Some("harm"),
        32 => Some("magic missile"),
        36 => Some("sanctuary"),
        42 => Some("word of recall"),
        52 => Some("identify"),
        _  => None,
    }
}

/// Reverse mapping from `Skill` to the CircleMUD-ish spell vnum used
/// by `apply_item_spell`.  Only spells in the consumable handler set
/// are brewable.
fn skill_to_potion_vnum(skill: crate::character::Skill) -> Option<i32> {
    use crate::character::Skill;
    match skill {
        Skill::Bless        => Some(3),
        Skill::BurningHands => Some(5),
        Skill::CureLight    => Some(16),
        Skill::DetectInvis  => Some(19),
        Skill::DetectMagic  => Some(20),
        Skill::Harm         => Some(27),
        Skill::MagicMissile => Some(32),
        Skill::Sanctuary    => Some(36),
        Skill::WordOfRecall => Some(42),
        Skill::Identify     => Some(52),
        _ => None,
    }
}


/// `cast enchant <weapon>` — adds +1 hitroll and +1 damroll to a
/// weapon in inventory.  Capped at +3 total bonus per stat.  Item
/// must be ITEM_WEAPON and unwielded (so we don't have to retroactively
/// patch `me.bonus_hitroll`/`damroll`).
async fn cast_enchant(
    target_kw: &str,
    me: &mut Character,
    world: &Arc<Mutex<World>>,
) -> CmdOutput {
    use crate::world::{APPLY_HITROLL, APPLY_DAMROLL, APPLY_AC, ITEM_WEAPON, ITEM_ARMOR};
    let kw = target_kw.trim().to_ascii_lowercase();
    if kw.is_empty() {
        return CmdOutput::text("\r\nEnchant what?\r\n".to_string());
    }
    // Find by keyword in inventory only.  Accept WEAPON or ARMOR.
    let (iid, short, item_type) = {
        let w = world.lock().await;
        let mut hit: Option<(u32, String, i32)> = None;
        for &iid in &me.inventory {
            let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let Some(p) = w.obj_protos.get(&o.vnum) else { continue; };
            if p.item_type != ITEM_WEAPON && p.item_type != ITEM_ARMOR { continue; }
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&kw)) {
                hit = Some((iid, p.short_description.clone(), p.item_type));
                break;
            }
        }
        match hit {
            Some(h) => h,
            None => return CmdOutput::text(
                "\r\nYou have no such weapon or armor to enchant (it must be in your inventory).\r\n".to_string(),
            ),
        }
    };
    // Cap check per-type.
    let too_strong = {
        let w = world.lock().await;
        let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else { return CmdOutput::text("\r\nIt slips from your grasp.\r\n".to_string()); };
        let mut hr = 0; let mut dr = 0; let mut ac = 0;
        for a in &o.bonus_affects {
            match a.location {
                APPLY_HITROLL => hr += a.modifier,
                APPLY_DAMROLL => dr += a.modifier,
                APPLY_AC      => ac += a.modifier,
                _ => {}
            }
        }
        if item_type == ITEM_WEAPON { hr >= 3 || dr >= 3 } else { ac >= 3 }
    };
    if too_strong {
        return CmdOutput::text(format!(
            "\r\n{short} already crackles with as much enchantment as it can hold.\r\n"
        ));
    }
    me.mana -= crate::character::Skill::Enchant.mana_cost();
    let (msg, _): (String, ()) = {
        let mut w = world.lock().await;
        if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
            if item_type == ITEM_WEAPON {
                o.bonus_affects.push(crate::world::ObjAffect {
                    location: APPLY_HITROLL, modifier: 1,
                });
                o.bonus_affects.push(crate::world::ObjAffect {
                    location: APPLY_DAMROLL, modifier: 1,
                });
                (format!(
                    "\r\n{short} hums and accepts the enchantment (+1 hitroll, +1 damroll).\r\n"
                ), ())
            } else {
                o.bonus_affects.push(crate::world::ObjAffect {
                    location: APPLY_AC, modifier: 1,
                });
                (format!(
                    "\r\n{short} hums and accepts the enchantment (+1 AC).\r\n"
                ), ())
            }
        } else {
            ("\r\nIt slips from your grasp.\r\n".to_string(), ())
        }
    };
    CmdOutput::text(msg)
}


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

    // Brewed-potion override: a per-instance spell vnum trumps proto values.
    let brewed = {
        let w = world.lock().await;
        w.obj_instances.iter().find(|o| o.id == iid)
            .and_then(|o| o.brewed_spell)
    };

    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} quaffs {}.\r\n", me.name, short));
    drop(cl);

    let mut text = format!("\r\nYou quaff {}.\r\n", short);
    if let Some(spell_vnum) = brewed {
        let s = apply_item_spell(spell_vnum, "", me, world, chars).await;
        text.push_str(&s);
    } else {
        // value[0] = level, value[1..3] = up to three spell vnums to cast on
        // the drinker.
        for slot in 1..4 {
            let s = apply_item_spell(value[slot], "", me, world, chars).await;
            text.push_str(&s);
        }
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
    let (sips_after, capacity, liq_type) = {
        let mut w = world.lock().await;
        let Some(p) = w.obj_protos.get_mut(&vnum) else {
            return CmdOutput::text("\r\nThe container shimmers and is gone.\r\n".to_string());
        };
        if p.value[1] <= 0 {
            return CmdOutput::text(format!("\r\n{} is empty.\r\n", short));
        }
        p.value[1] -= 1;
        (p.value[1], p.value[0], p.value[2])
    };
    let was_thirsty = me.thirst >= 0 && me.thirst <= 3;
    if me.thirst >= 0 {
        me.thirst = (me.thirst + 3).min(MAX_THIRST);
    }
    // Alcoholic liquids raise intoxication (cp208).
    let was_sober = me.drunk < DRUNK_SLUR_THRESHOLD;
    let drunk_gain = liquid_drunk(liq_type);
    if drunk_gain > 0 {
        me.drunk = (me.drunk + drunk_gain).min(MAX_DRUNK);
    }
    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} drinks from {}.\r\n", me.name, short));
    drop(cl);
    let mut text = format!("\r\nYou drink from {}.\r\n", short);
    if was_thirsty && me.thirst > 3 {
        text.push_str("You are no longer thirsty.\r\n");
    }
    if drunk_gain > 0 {
        if was_sober && me.drunk >= DRUNK_SLUR_THRESHOLD {
            text.push_str("Your head begins to swim — you're getting drunk.\r\n");
        } else if me.drunk >= MAX_DRUNK {
            text.push_str("You are completely smashed.\r\n");
        } else {
            text.push_str("You feel a pleasant warmth spread through you.\r\n");
        }
    }
    if sips_after == 0 {
        text.push_str(&format!("{} is now empty.\r\n", short));
    }
    let _ = capacity;
    CmdOutput::text(text)
}

/// Locate a drink container OR fountain by keyword in the player's
/// inventory and the current-room floor.  Returns `(iid, vnum, short,
/// item_type)` for the first match; restricted to types listed in
/// `accept`.
async fn find_liquid_obj(
    me: &Character,
    world: &Arc<Mutex<World>>,
    kw: &str,
    accept: &[i32],
) -> Option<(u32, crate::world::ObjVnum, String, i32)> {
    let kw = kw.to_ascii_lowercase();
    let w = world.lock().await;
    let pool: Vec<u32> = me.inventory.iter().copied()
        .chain(w.rooms.get(&me.current_room).map(|r| r.objects.clone()).unwrap_or_default())
        .collect();
    for iid in pool {
        let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
        let Some(p) = w.obj_protos.get(&o.vnum) else { continue; };
        if !accept.contains(&p.item_type) { continue; }
        if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&kw)) {
            return Some((iid, o.vnum, p.short_description.clone(), p.item_type));
        }
    }
    None
}

/// `fill <container> <source>` — refill a drink container from a
/// fountain (infinite source) or another drink container (drains it).
/// Liquid type (value[2]) is propagated from the source to the target.
async fn do_fill(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::world::{ITEM_DRINKCON, ITEM_FOUNTAIN};
    let mut parts = arg.split_whitespace();
    let Some(con_kw) = parts.next() else {
        return CmdOutput::text("\r\nUsage: fill <container> <source>\r\n".to_string());
    };
    let Some(src_kw) = parts.next() else {
        return CmdOutput::text("\r\nFill from what?\r\n".to_string());
    };
    let Some((_, con_vnum, con_short, _)) =
        find_liquid_obj(me, world, con_kw, &[ITEM_DRINKCON]).await
    else {
        return CmdOutput::text("\r\nYou don't have a container by that name.\r\n".to_string());
    };
    let Some((_, src_vnum, src_short, src_type)) =
        find_liquid_obj(me, world, src_kw, &[ITEM_DRINKCON, ITEM_FOUNTAIN]).await
    else {
        return CmdOutput::text("\r\nThere's no such source here.\r\n".to_string());
    };
    if con_vnum == src_vnum {
        return CmdOutput::text("\r\nYou can't fill it from itself.\r\n".to_string());
    }
    // Transfer.  Fountain → fill to capacity.  Drink container → drain
    // up to its current sips; both prototypes mutate.
    let (capacity, new_sips, liquid_type) = {
        let mut w = world.lock().await;
        let con_cap = w.obj_protos.get(&con_vnum).map(|p| p.value[0]).unwrap_or(0);
        if con_cap == 0 {
            return CmdOutput::text(format!("\r\n{con_short} has no capacity.\r\n"));
        }
        let liquid_type = w.obj_protos.get(&src_vnum).map(|p| p.value[2]).unwrap_or(0);
        let to_pour = if src_type == ITEM_FOUNTAIN {
            con_cap
        } else {
            let src_cur = w.obj_protos.get(&src_vnum).map(|p| p.value[1]).unwrap_or(0);
            src_cur.min(con_cap)
        };
        if to_pour == 0 {
            return CmdOutput::text(format!("\r\n{src_short} is dry.\r\n"));
        }
        if src_type == ITEM_DRINKCON {
            if let Some(sp) = w.obj_protos.get_mut(&src_vnum) {
                sp.value[1] -= to_pour;
            }
        }
        if let Some(cp) = w.obj_protos.get_mut(&con_vnum) {
            cp.value[1] = to_pour;
            cp.value[2] = liquid_type;
        }
        (con_cap, to_pour, liquid_type)
    };
    let _ = (capacity, liquid_type);
    chars.lock().await.broadcast_room(
        me.current_room, Some(me.id),
        &format!("{} fills {con_short} from {src_short}.\r\n", me.name),
    );
    CmdOutput::text(format!(
        "\r\nYou fill {con_short} from {src_short}. ({new_sips} sips)\r\n"
    ))
}

/// `empty <container>` — pour out a drink container onto the ground.
async fn do_empty(
    arg: &str,
    me: &Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    use crate::world::ITEM_DRINKCON;
    let kw = arg.trim();
    if kw.is_empty() {
        return CmdOutput::text("\r\nEmpty what?\r\n".to_string());
    }
    let Some((_, vnum, short, _)) =
        find_liquid_obj(me, world, kw, &[ITEM_DRINKCON]).await
    else {
        return CmdOutput::text("\r\nYou don't have a container by that name.\r\n".to_string());
    };
    {
        let mut w = world.lock().await;
        if let Some(p) = w.obj_protos.get_mut(&vnum) {
            if p.value[1] <= 0 {
                return CmdOutput::text(format!("\r\n{short} is already empty.\r\n"));
            }
            p.value[1] = 0;
        }
    }
    chars.lock().await.broadcast_room(
        me.current_room, Some(me.id),
        &format!("{} empties {short} onto the ground.\r\n", me.name),
    );
    CmdOutput::text(format!("\r\nYou empty {short}.\r\n"))
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
/// Intoxication cap (game-hours of drunkenness). Matches stock CircleMUD.
pub const MAX_DRUNK:  i32 = 24;
/// Speech starts to slur at or above this intoxication level.
pub const DRUNK_SLUR_THRESHOLD: i32 = 6;

/// Drunkenness added per sip of a given liquid type (`value[2]` on an
/// ITEM_DRINKCON).  Indices match CircleMUD's LIQ_* order; the values
/// mirror the `drunk` column of `drink_aff[]` in constants.c.  Non-listed
/// or non-alcoholic liquids contribute 0.
pub fn liquid_drunk(liq_type: i32) -> i32 {
    match liq_type {
        1  => 3,   // LIQ_BEER
        2  => 5,   // LIQ_WINE
        3  => 2,   // LIQ_ALE
        4  => 1,   // LIQ_DARKALE
        5  => 6,   // LIQ_WHISKY
        7  => 10,  // LIQ_FIREBRT (firebreather)
        8  => 3,   // LIQ_LOCALSPC (local specialty)
        _  => 0,   // water, juice, milk, tea, coffee, blood, etc.
    }
}

/// Garble spoken text for a drunk speaker.  The drunker they are, the more
/// letters get doubled and the more `*hic*` interjections appear.  Sober
/// (below the slur threshold) text passes through unchanged.
pub fn garble_drunk(text: &str, drunk: i32) -> String {
    use rand::Rng;
    if drunk < DRUNK_SLUR_THRESHOLD { return text.to_string(); }
    // Slur intensity scales 0..=100 with drunkenness past the threshold.
    let intensity = ((drunk - DRUNK_SLUR_THRESHOLD) * 6 + 10).min(60);
    let mut rng = rand::thread_rng();
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        out.push(ch);
        if ch.is_alphabetic() && rng.gen_range(0..100) < intensity {
            out.push(ch);   // slurred doubling
        }
        if ch == ' ' && rng.gen_range(0..100) < intensity / 4 {
            out.push_str("*hic* ");
        }
    }
    out
}

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

    // Brewed-scroll override (cp176): if `brewed_spell` is set on the
    // instance, apply that spell instead of the proto's value[1..3].
    let brewed = {
        let w = world.lock().await;
        w.obj_instances.iter().find(|o| o.id == iid)
            .and_then(|o| o.brewed_spell)
    };

    let cl = chars.lock().await;
    cl.broadcast_room(me.current_room, Some(me.id),
        &format!("{} recites {}.\r\n", me.name, short));
    drop(cl);

    let mut text = format!("\r\nYou recite {} which dissolves.\r\n", short);
    if let Some(spell_vnum) = brewed {
        let s = apply_item_spell(spell_vnum, target, me, world, chars).await;
        text.push_str(&s);
    } else {
        for slot in 1..4 {
            let s = apply_item_spell(value[slot], target, me, world, chars).await;
            text.push_str(&s);
        }
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
    let (iid, short, fuel_hours) = {
        let w = world.lock().await;
        let pool: Vec<u32> = me.inventory.iter().copied()
            .chain(w.rooms.get(&me.current_room)
                .map(|r| r.objects.clone()).unwrap_or_default())
            .collect();
        let mut found: Option<(u32, String, i32)> = None;
        for iid in pool {
            let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let Some(p) = w.obj_protos.get(&o.vnum) else { continue; };
            if p.item_type != ITEM_LIGHT { continue; }
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&kw)) {
                found = Some((iid, p.short_description.clone(), p.value[2]));
                break;
            }
        }
        match found {
            Some(v) => v,
            None => return CmdOutput::text(format!("\r\nYou see no such light here.\r\n")),
        }
    };

    // Toggle the lit state under a fresh lock.  Refuse no-op transitions and
    // refuse relighting a burned-out source.  Seed the fuel counter from the
    // proto's value[2] when first lighting a fresh light (cp207).
    let already = {
        let mut w = world.lock().await;
        let o = match w.obj_instances.iter_mut().find(|o| o.id == iid) {
            Some(o) => o,
            None    => return CmdOutput::text("\r\nIt's gone now.\r\n".to_string()),
        };
        if on && o.light_hours < 0 {
            return CmdOutput::text(format!(
                "\r\n{} has burned out — there's nothing left to light.\r\n", short
            ));
        }
        if o.light_lit == on {
            true
        } else {
            o.light_lit = on;
            if on && o.light_hours == 0 && fuel_hours > 0 {
                o.light_hours = fuel_hours;
            }
            false
        }
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
    let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else { return Vec::new() };
    let mut out: Vec<crate::world::ObjAffect> = w.obj_protos.get(&o.vnum)
        .map(|p| p.affected.clone())
        .unwrap_or_default();
    out.extend(o.bonus_affects.iter().cloned());
    out
}

/// Return Some(label) if the player is barred from wearing/wielding
/// this object based on its `extra_flags` and their class/alignment.
fn anti_class_block(
    extra_flags: u32,
    class: crate::players::Class,
    alignment: i32,
) -> Option<&'static str> {
    use crate::players::Class;
    use crate::character::AlignmentBand;
    use crate::world::*;
    match class {
        Class::Warrior   if extra_flags & ITEM_ANTI_WARRIOR    != 0 => return Some("warriors"),
        Class::Cleric    if extra_flags & ITEM_ANTI_CLERIC     != 0 => return Some("clerics"),
        Class::Thief     if extra_flags & ITEM_ANTI_THIEF      != 0 => return Some("thieves"),
        Class::MagicUser if extra_flags & ITEM_ANTI_MAGIC_USER != 0 => return Some("magic users"),
        _ => {}
    }
    match AlignmentBand::of(alignment) {
        AlignmentBand::Good    if extra_flags & ITEM_ANTI_GOOD    != 0 => Some("the good-aligned"),
        AlignmentBand::Evil    if extra_flags & ITEM_ANTI_EVIL    != 0 => Some("the evil-aligned"),
        AlignmentBand::Neutral if extra_flags & ITEM_ANTI_NEUTRAL != 0 => Some("the neutral"),
        _ => None,
    }
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

    // Item must have ITEM_WEAR_WIELD bit set + no anti-class block.
    let (wear_flags, extra_flags) = {
        let obj = w.obj_instances.iter().find(|o| o.id == iid);
        let proto = obj.and_then(|o| w.obj_protos.get(&o.vnum));
        (
            proto.map(|p| p.wear_flags[0]).unwrap_or(0),
            proto.map(|p| p.extra_flags[0]).unwrap_or(0),
        )
    };
    drop(w);

    if wear_flags & ITEM_WEAR_WIELD == 0 {
        return CmdOutput::text(format!("\r\nYou cannot wield {short}.\r\n"));
    }
    if let Some(bar) = anti_class_block(extra_flags, me.class, me.alignment) {
        return CmdOutput::text(format!(
            "\r\n{short} pulses with a runic ward against {bar}.\r\n"
        ));
    }
    if me.equipment[WEAR_WIELD].is_some() {
        return CmdOutput::text("\r\nYou are already wielding something.\r\n");
    }
    // Two-handed weapons need both hands free.
    if extra_flags & crate::world::ITEM_2H_WEAPON != 0 {
        if me.equipment[crate::character::WEAR_SHIELD].is_some() {
            return CmdOutput::text(format!(
                "\r\n{short} is two-handed; you'd have to drop your shield first.\r\n"
            ));
        }
        if me.equipment[crate::character::WEAR_HOLD].is_some() {
            return CmdOutput::text(format!(
                "\r\n{short} is two-handed; your off-hand isn't free.\r\n"
            ));
        }
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
    if arg.eq_ignore_ascii_case("all") {
        return do_wear_all(me, world, chars).await;
    }
    let w = world.lock().await;
    let key = arg.to_ascii_lowercase();

    let (idx, iid, short) = match find_inv_match(&w, &me.inventory, &key) {
        Some(t) => t,
        None => return CmdOutput::text(format!("\r\nYou do not have a {key}.\r\n")),
    };

    // Look up the object's wear flags + extra flags (for anti-class).
    let (wear_flags, extra_flags) = {
        let obj = w.obj_instances.iter().find(|o| o.id == iid);
        let proto = obj.and_then(|o| w.obj_protos.get(&o.vnum));
        (
            proto.map(|p| p.wear_flags[0]).unwrap_or(0),
            proto.map(|p| p.extra_flags[0]).unwrap_or(0),
        )
    };
    drop(w);

    let slot = match auto_wear_slot(wear_flags) {
        Some(s) => s,
        None => return CmdOutput::text(format!("\r\nYou cannot wear {short}.\r\n")),
    };
    if let Some(bar) = anti_class_block(extra_flags, me.class, me.alignment) {
        return CmdOutput::text(format!(
            "\r\n{short} pulses with a runic ward against {bar}.\r\n"
        ));
    }

    if me.equipment[slot].is_some() {
        return CmdOutput::text(format!(
            "\r\nYou are already wearing something {}.\r\n",
            wear_pos_name(slot)
        ));
    }
    // If wearing into shield or hold slot, refuse when a two-handed
    // weapon is wielded.
    if (slot == crate::character::WEAR_SHIELD
        || slot == crate::character::WEAR_HOLD)
       && me.equipment[crate::character::WEAR_WIELD].is_some()
    {
        let w2 = world.lock().await;
        let two_handed = me.equipment[crate::character::WEAR_WIELD]
            .and_then(|iid| w2.obj_instances.iter().find(|o| o.id == iid))
            .and_then(|o| w2.obj_protos.get(&o.vnum))
            .map(|p| p.extra_flags[0] & crate::world::ITEM_2H_WEAPON != 0)
            .unwrap_or(false);
        drop(w2);
        if two_handed {
            return CmdOutput::text(format!(
                "\r\nYour wielded weapon needs both hands — drop or remove it first.\r\n"
            ));
        }
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
    if arg.eq_ignore_ascii_case("all") {
        return do_remove_all(me, world, chars).await;
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

/// `wear all` — wear every wearable item in inventory that fits a free
/// slot (cp214).  Skips weapons (those are `wield`ed), anti-class items,
/// already-occupied slots, and off-hand slots blocked by a 2H weapon.
async fn do_wear_all(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let candidates = me.inventory.clone();
    if candidates.is_empty() {
        return CmdOutput::text("\r\nYou have nothing to wear.\r\n".to_string());
    }
    // Whether a two-handed weapon is currently wielded (blocks shield/hold).
    let two_handed_wield = {
        let w = world.lock().await;
        me.equipment[crate::character::WEAR_WIELD]
            .and_then(|iid| w.obj_instances.iter().find(|o| o.id == iid))
            .and_then(|o| w.obj_protos.get(&o.vnum))
            .map(|p| p.extra_flags[0] & crate::world::ITEM_2H_WEAPON != 0)
            .unwrap_or(false)
    };
    let mut worn: Vec<String> = Vec::new();
    for iid in candidates {
        let (wear_flags, extra_flags, short) = {
            let w = world.lock().await;
            let Some(o) = w.obj_instances.iter().find(|o| o.id == iid) else { continue; };
            let Some(p) = w.obj_protos.get(&o.vnum) else { continue; };
            (p.wear_flags[0], p.extra_flags[0], p.short_description.clone())
        };
        let Some(slot) = auto_wear_slot(wear_flags) else { continue; };
        if me.equipment[slot].is_some() { continue; }
        if anti_class_block(extra_flags, me.class, me.alignment).is_some() { continue; }
        if two_handed_wield
            && (slot == crate::character::WEAR_SHIELD
                || slot == crate::character::WEAR_HOLD)
        { continue; }

        me.inventory.retain(|&i| i != iid);
        me.equipment[slot] = Some(iid);
        let affects = snapshot_obj_affects(iid, world).await;
        apply_obj_affects(me, &affects, 1);
        fire_obj_wear_triggers(iid, &me.name, me.current_room, world, chars).await;
        worn.push(short);
    }
    if worn.is_empty() {
        return CmdOutput::text("\r\nYou have nothing you can wear.\r\n".to_string());
    }
    let mut s = format!("\r\nYou wear {} item(s):\r\n", worn.len());
    for n in &worn { s.push_str(&format!("  {n}\r\n")); }
    CmdOutput::text(s)
}

/// `remove all` — take off every worn item back into inventory (cp214).
async fn do_remove_all(
    me: &mut Character,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) -> CmdOutput {
    let mut removed: Vec<String> = Vec::new();
    for slot in 0..NUM_WEARS {
        let Some(iid) = me.equipment[slot] else { continue; };
        let short = {
            let w = world.lock().await;
            w.obj_instances.iter().find(|o| o.id == iid)
                .and_then(|o| w.obj_protos.get(&o.vnum))
                .map(|p| p.short_description.clone())
                .unwrap_or_else(|| "something".to_string())
        };
        me.equipment[slot] = None;
        me.inventory.push(iid);
        let affects = snapshot_obj_affects(iid, world).await;
        apply_obj_affects(me, &affects, -1);
        fire_obj_remove_triggers(iid, &me.name, me.current_room, world, chars).await;
        removed.push(short);
    }
    if removed.is_empty() {
        return CmdOutput::text("\r\nYou aren't wearing anything.\r\n".to_string());
    }
    let mut s = format!("\r\nYou remove {} item(s):\r\n", removed.len());
    for n in &removed { s.push_str(&format!("  {n}\r\n")); }
    CmdOutput::text(s)
}

/// `compare <a> <b>` (cp225): compare two carried/worn items of the same
/// kind.  Weapons compare by average damage (`dice_count * (dice_size+1)/2`),
/// armor by AC value; other types can't be meaningfully compared.  A
/// connecting "to"/"with" word is ignored ("compare sword to axe").

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
    let proto_info: Option<(i32, [i32; 4], Vec<crate::world::ObjAffect>, i32, Option<i32>, Vec<crate::world::ObjAffect>)> = me.inventory.iter()
        .chain(me.equipment.iter().filter_map(|s| s.as_ref()))
        .find_map(|&iid| {
            let o = w.obj_instances.iter().find(|o| o.id == iid)?;
            let p = w.obj_protos.get(&o.vnum)?;
            if p.name.split_whitespace().any(|k| k.eq_ignore_ascii_case(&key)) {
                Some((p.item_type, p.value, p.affected.clone(),
                      o.condition, o.brewed_spell, o.bonus_affects.clone()))
            } else {
                None
            }
        });

    if let Some((ty, vals, affs, _cond, _brewed, bonus)) = proto_info {
        let kind = item_type_name(ty);
        let extra = match ty {
            // ITEM_WEAPON: value[1] dice count, value[2] dice size, value[3] damage type
            5 => format!("This is a {kind} that does {}d{} damage.\r\n", vals[1], vals[2]),
            // ITEM_ARMOR: value[0] is AC
            9 => format!("This is {kind}, providing {} AC.\r\n", vals[0]),
            // ITEM_LIGHT: value[2] is hours remaining
            1 => format!("This is a {kind} with {} hours of light left.\r\n", vals[2]),
            // ITEM_BOAT: lets the carrier cross deep (no-swim) water.
            22 => format!("This is a {kind} — carry it to cross deep water.\r\n"),
            _ => format!("This is a {kind}.\r\n"),
        };
        let mut out = base.text;
        out.push_str(&extra);
        if !affs.is_empty() || !bonus.is_empty() {
            out.push_str("Affects:\r\n");
            for a in &affs {
                let name = apply_name(a.location);
                let sign = if a.modifier >= 0 { "+" } else { "" };
                out.push_str(&format!("  {} by {sign}{}\r\n", name, a.modifier));
            }
            for a in &bonus {
                let name = apply_name(a.location);
                let sign = if a.modifier >= 0 { "+" } else { "" };
                out.push_str(&format!("  {} by {sign}{} (enchant)\r\n", name, a.modifier));
            }
        }
        return CmdOutput::text(out);
    }
    base
}

/// Banded label for an object's `condition` (0..=100).
pub fn condition_label(cond: i32) -> &'static str {
    match cond {
        0          => "broken",
        1..=20     => "poor",
        21..=50    => "worn",
        51..=80    => "good",
        81..=99    => "fine",
        _          => "pristine",
    }
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
    // `give all coins <player>` / `give all gold <player>` / `give all money <player>`
    if obj_kw.eq_ignore_ascii_case("all") {
        for kw in ["coins ", "gold ", "money "] {
            if let Some(rest) = target_kw.to_ascii_lowercase().strip_prefix(kw) {
                // Reconstruct the literal player name from the trailing part of target_kw.
                let split_at = kw.len();
                let actual_target = target_kw[split_at..].trim();
                let _ = rest;
                let amount = me.gold;
                return do_give_gold(amount, actual_target, me, world, chars).await;
            }
        }
    }
    // `give all <player>` / `give all.<kw> <player>`
    if obj_kw.eq_ignore_ascii_case("all") || obj_kw.to_ascii_lowercase().starts_with("all.") {
        let kw = obj_kw.split_once('.').map(|(_, k)| k.to_ascii_lowercase());
        return do_give_all(me, world, chars, kw, target_kw).await;
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
        let price = ((proto.cost as f32) * shop.profit_sell) as i64;
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

/// `appraise <item>` (alias `value`) — preview a shopkeeper's sell
/// price for an inventory item without committing.  Same gating and
/// formula as `do_sell`.
async fn do_appraise(arg: &str, me: &Character, world: &Arc<Mutex<World>>) -> CmdOutput {
    let key = arg.trim().to_ascii_lowercase();
    if key.is_empty() {
        return CmdOutput::text("\r\nAppraise what?\r\n".to_string());
    }
    let w = world.lock().await;
    let Some(shop) = w.shop_in_room(me.current_room) else {
        return CmdOutput::text("\r\nThere is no shop here.\r\n".to_string());
    };
    let Some((_idx, iid, short)) = find_inv_match(&w, &me.inventory, &key) else {
        return CmdOutput::text(format!("\r\nYou do not have a {key}.\r\n"));
    };
    let obj = w.obj_instances.iter().find(|o| o.id == iid).unwrap();
    let proto = w.obj_protos.get(&obj.vnum).unwrap();
    if !shop.buys_types.is_empty() && !shop.buys_types.contains(&proto.item_type) {
        return CmdOutput::text(format!(
            "\r\nThe shopkeeper wouldn't buy {short}.\r\n"
        ));
    }
    let price = ((proto.cost as f32) * shop.profit_sell) as i64;
    CmdOutput::text(format!(
        "\r\nThe shopkeeper would give you {price} gold for {short}.\r\n"
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

/// Persist `me`'s state to disk via the PlayerDb.  Returns Ok(()) on
/// success, or an error chain.  Used by `do_save`, the auto-save on
/// disconnect, and `spawn_save_all_tick`.
pub async fn save_character_to_db(
    me: &Character,
    players: &Arc<Mutex<PlayerDb>>,
) -> anyhow::Result<()> {
    let pl = players.lock().await;
    let mut r = pl.load_player(&me.name)?;
    r.hp        = me.hp;
    r.max_hp    = me.max_hp;
    r.mana      = me.mana;
    r.max_mana  = me.max_mana;
    r.movement     = me.movement;
    r.max_movement = me.max_movement;
    r.position     = me.position.save_key().to_string();
    r.wimpy        = me.wimpy;
    r.color_off    = me.color_off;
    r.autoexit     = me.autoexit;
    r.autoloot     = me.autoloot;
    r.autoassist   = me.autoassist;
    r.autotitle_off = !me.autotitle;
    r.autogold     = me.autogold;
    r.autosplit    = me.autosplit;
    r.autosac      = me.autosac;
    r.autodoor     = me.autodoor;
    r.autokey      = me.autokey;
    r.automap      = me.automap;
    r.alignment    = me.alignment;
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
    r.rent_per_day    = me.rent_per_day;
    r.prompt_format   = me.prompt_format.clone();
    r.aliases         = me.aliases.clone();
    r.last_login      = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64).unwrap_or(r.last_login);
    r.god             = me.god.clone();
    r.muted           = me.muted;
    r.frozen          = me.frozen;
    r.notes           = me.notes.clone();
    r.pose            = me.pose.clone();
    pl.save_player(&r)?;
    Ok(())
}

async fn do_save(me: &Character, players: &Arc<Mutex<PlayerDb>>) -> CmdOutput {
    let pl = players.lock().await;
    let rec = match pl.load_player(&me.name) {
        Ok(mut r) => {
            r.hp        = me.hp;
            r.max_hp    = me.max_hp;
            r.mana      = me.mana;
            r.max_mana  = me.max_mana;
            r.movement     = me.movement;
            r.max_movement = me.max_movement;
            r.position     = me.position.save_key().to_string();
            r.wimpy        = me.wimpy;
            r.color_off    = me.color_off;
            r.autoexit     = me.autoexit;
            r.autoloot     = me.autoloot;
            r.autoassist   = me.autoassist;
            r.autotitle_off = !me.autotitle;
            r.autogold     = me.autogold;
            r.autosplit    = me.autosplit;
            r.autosac      = me.autosac;
            r.autodoor     = me.autodoor;
            r.autokey      = me.autokey;
            r.automap      = me.automap;
            r.alignment    = me.alignment;
            r.clan         = me.clan.clone();
            r.pkills       = me.pkills;
            r.pdeaths      = me.pdeaths;
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
            r.rent_per_day    = me.rent_per_day;
            r.prompt_format   = me.prompt_format.clone();
            r.aliases         = me.aliases.clone();
            r.last_login      = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64).unwrap_or(r.last_login);
            r.god             = me.god.clone();
            r.muted           = me.muted;
            r.frozen          = me.frozen;
            r.notes           = me.notes.clone();
            r.pose            = me.pose.clone();
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
            let obj = w.obj_instances.iter().find(|o| o.id == iid);
            let short = obj
                .and_then(|o| w.obj_protos.get(&o.vnum))
                .map(|p| p.short_description.clone())
                .unwrap_or_else(|| "(something)".into());
            s.push_str(&format!("  <{:^22}>  {short}\r\n", wear_pos_name(slot)));
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
    let from_sector = r.sector_type;
    let (target, closed, locked, door_key, door_kw, target_flags, target_sector) = match &r.exits[dir as usize] {
        Some(e) if e.to_room != crate::world::NOWHERE
            && w.rooms.contains_key(&e.to_room) =>
        {
            let closed = (e.exit_info & crate::world::EX_CLOSED) != 0;
            let locked = (e.exit_info & crate::world::EX_LOCKED) != 0;
            let kw = if e.keyword.is_empty() { "door".to_string() }
                     else { e.keyword.split_whitespace().next().unwrap_or("door").to_string() };
            let dest = w.rooms.get(&e.to_room);
            let flags = dest.map(|r| r.room_flags[0]).unwrap_or(0);
            let sect  = dest.map(|r| r.sector_type).unwrap_or(0);
            (e.to_room, closed, locked, e.key, kw, flags, sect)
        }
        _ => return CmdOutput::text(format!("\r\nAlas, you cannot go that way...\r\n")),
    };
    // Does the player carry (or wear) a boat?  Lets them cross no-swim
    // water without flying (cp210).
    let has_boat = {
        me.inventory.iter().copied()
            .chain(me.equipment.iter().filter_map(|s| *s))
            .any(|iid| {
                w.obj_instances.iter().find(|o| o.id == iid)
                    .and_then(|o| w.obj_protos.get(&o.vnum))
                    .map(|p| p.item_type == crate::world::ITEM_BOAT)
                    .unwrap_or(false)
            })
    };
    drop(w);
    let mut auto_msg = String::new();
    if closed {
        // Autodoor: try to open a closed door in the path before refusing.
        // Autokey: if it's locked, unlock it first when carrying the key.
        if me.autodoor {
            if locked {
                let has_key = door_key > 0 && me.autokey
                    && player_has_key(me, door_key, world).await;
                if !has_key {
                    return CmdOutput::text(format!("\r\nThe {door_kw} is closed and locked.\r\n"));
                }
                {
                    let mut w = world.lock().await;
                    mutate_door(&mut w, me.current_room, dir, 0, crate::world::EX_LOCKED);
                }
                auto_msg.push_str(&format!("You unlock the {door_kw}.\r\n"));
                let cl = chars.lock().await;
                cl.broadcast_room(me.current_room, Some(me.id),
                    &format!("{} unlocks the {door_kw}.\r\n", me.name));
            }
            {
                let mut w = world.lock().await;
                mutate_door(&mut w, me.current_room, dir, 0, crate::world::EX_CLOSED);
            }
            auto_msg.push_str(&format!("You open the {door_kw}.\r\n"));
            let cl = chars.lock().await;
            cl.broadcast_room(me.current_room, Some(me.id),
                &format!("{} opens the {door_kw}.\r\n", me.name));
        } else {
            return CmdOutput::text(format!("\r\nThe {door_kw} is closed.\r\n"));
        }
    }
    // Position gate.  Must be standing to move.
    match me.position {
        crate::character::Position::Standing
        | crate::character::Position::Fighting => {}
        crate::character::Position::Sleeping =>
            return CmdOutput::text("\r\nIn your dreams, or what?\r\n".to_string()),
        _ =>
            return CmdOutput::text("\r\nYou should probably stand up first.\r\n".to_string()),
    }
    // Deep-water / no-swim and underwater sectors are impassable on foot.
    // No-swim water can be crossed by flying OR by carrying a boat (cp210);
    // underwater requires flying (a boat doesn't help below the surface).
    // Immortals bypass entirely (cp209).
    let flying = me.is_flying();
    if me.level < LVL_IMMORT {
        if target_sector == crate::world::SECT_UNDERWATER && !flying {
            return CmdOutput::text(
                "\r\nYou'd drown — you need to be flying to go there.\r\n".to_string()
            );
        }
        if target_sector == crate::world::SECT_WATER_NOSWIM && !flying && !has_boat {
            return CmdOutput::text(
                "\r\nYou need a boat (or to be flying) to go there.\r\n".to_string()
            );
        }
    }
    // Movement-point gate.  Mortals pay (from_sector + to_sector) / 2
    // movement per step (rounded up, min 1); immortals are exempt.  Flying
    // characters glide at a flat cost of 1 regardless of terrain.
    // Refuse with a "too exhausted" message at 0.
    if me.level < LVL_IMMORT {
        let cost = if flying {
            1
        } else {
            let from_cost = crate::world::sector_move_cost(from_sector);
            let to_cost   = crate::world::sector_move_cost(target_sector);
            ((from_cost + to_cost + 1) / 2).max(1)
        };
        if me.movement < cost {
            return CmdOutput::text(
                "\r\nYou are too exhausted.\r\n".to_string()
            );
        }
        me.movement -= cost;
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

    // Drag charmed mobs whose charmer == me.id and that hold an active
    // CharmPerson affect (the affect's presence is the real authority;
    // charmer can go stale after expiry).
    let dragged_mobs: Vec<String> = {
        let mut w = world.lock().await;
        let mids: Vec<u32> = w.mob_instances.iter()
            .filter(|m| m.in_room == from_room
                     && m.charmer == Some(me.id)
                     && m.affects.iter().any(|a|
                         a.skill == crate::character::Skill::CharmPerson))
            .map(|m| m.id).collect();
        let mut names = Vec::new();
        if !mids.is_empty() {
            if let Some(r) = w.rooms.get_mut(&from_room) {
                r.mobs.retain(|id| !mids.contains(id));
            }
            if let Some(r) = w.rooms.get_mut(&target) {
                for id in &mids { r.mobs.push(*id); }
            }
            for mid in mids {
                if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mid) {
                    m.in_room = target;
                }
                let name = w.mob_instances.iter().find(|m| m.id == mid)
                    .and_then(|m| w.mob_protos.get(&m.vnum))
                    .map(|p| p.short_descr.clone())
                    .unwrap_or_else(|| "a creature".to_string());
                names.push(name);
            }
        }
        names
    };
    if !dragged_mobs.is_empty() {
        let cl = chars.lock().await;
        for name in &dragged_mobs {
            cl.broadcast_room(from_room, Some(me.id),
                &format!("{name} follows {}.\r\n", me.name));
            cl.broadcast_room(target, Some(me.id),
                &format!("{name} arrives, following {}.\r\n", me.name));
        }
    }

    // Automap: append the mini-map to the room view on every move.
    if me.automap {
        let map = do_map(me, world).await;
        view.push_str(&map.text);
    }
    if auto_msg.is_empty() {
        CmdOutput::text(view)
    } else {
        CmdOutput::text(format!("\r\n{auto_msg}{view}"))
    }
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
                .map(mob_corpse_contents).unwrap_or_default();
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
    let (viewer_level, viewer_brief, viewer_autoexit): (i32, bool, bool) = match viewer_id {
        Some(id) => {
            let ph_opt = {
                let cl = chars.lock().await;
                let h = cl.iter().find(|p| p.id == id).cloned();
                h
            };
            match ph_opt {
                Some(ph) => {
                    let c = ph.character.lock().await;
                    (ph.level, c.brief, c.autoexit)
                }
                None => (0, false, false),
            }
        }
        None => (0, false, false),
    };

    // Snapshot the viewer's inventory + equipment ids for the dark-room
    // light-source check below.  Skipped for non-player views.
    let (viewer_objs, viewer_infra): (Vec<u32>, bool) = match viewer_id {
        Some(id) => {
            let ph_opt = {
                let cl = chars.lock().await;
                let h = cl.iter().find(|p| p.id == id).cloned();
                h
            };
            match ph_opt {
                Some(ph) => {
                    let c = ph.character.lock().await;
                    let mut v = c.inventory.clone();
                    for slot in c.equipment.iter().flatten() {
                        v.push(*slot);
                    }
                    let infra = c.affects.iter()
                        .any(|a| a.skill == crate::character::Skill::Infravision);
                    (v, infra)
                }
                None => (Vec::new(), false),
            }
        }
        None => (Vec::new(), false),
    };

    let w = world.lock().await;
    let Some(r) = w.rooms.get(&vnum) else {
        return "\r\nYou are nowhere.\r\n".to_string();
    };

    // Dark room handling.  Immortals always see; otherwise need a lit
    // light source on the floor or in the viewer's possession, OR the
    // viewer has Infravision active.
    if viewer_level < LVL_IMMORT && !viewer_infra && crate::db::is_room_dark(r) {
        let room_lit = r.objects.iter().any(|&iid| {
            w.obj_instances.iter()
                .find(|o| o.id == iid)
                .map(|o| o.light_lit)
                .unwrap_or(false)
        });
        let carried_lit = viewer_objs.iter().any(|iid| {
            w.obj_instances.iter()
                .find(|o| o.id == *iid)
                .map(|o| o.light_lit)
                .unwrap_or(false)
        });
        if !room_lit && !carried_lit {
            return "\r\nIt is pitch black...\r\nYou can't see a thing.\r\n".to_string();
        }
    }

    let mut s = String::with_capacity(r.description.len() + 512);
    s.push_str("\r\n");
    s.push_str(&r.name);
    s.push_str("\r\n");
    if !viewer_brief {
        for line in r.description.split('\n') {
            s.push_str(line);
            s.push_str("\r\n");
        }
        // Outdoor rooms get a live weather/sky line (cp237), driven by the
        // cp212 weather sim + cp111 day/night cycle.
        if r.sector_type != crate::world::SECT_INSIDE
            && r.sector_type != crate::world::SECT_CITY
        {
            if let Some(line) = weather_line() {
                s.push_str(line);
                s.push_str("\r\n");
            }
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
    if viewer_autoexit {
        // Compact bracketed form, uppercase one-letter abbreviations.
        let letters: String = exits.iter()
            .map(|n| n.chars().next().unwrap_or('?').to_ascii_uppercase())
            .collect::<Vec<_>>()
            .iter().map(|c| c.to_string()).collect::<Vec<_>>().join(" ");
        if letters.is_empty() {
            s.push_str("[ Exits: None ]\r\n");
        } else {
            s.push_str(&format!("[ Exits: {letters} ]\r\n"));
        }
    } else if exits.is_empty() {
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
                    // Append "(wielding <weapon>)" when present.
                    if let Some(wiid) = m.equipment[crate::character::WEAR_WIELD] {
                        if let Some(wp) = w.obj_instances.iter()
                            .find(|o| o.id == wiid)
                            .and_then(|o| w.obj_protos.get(&o.vnum))
                        {
                            s.push_str(" (wielding ");
                            s.push_str(&wp.short_description);
                            s.push(')');
                        }
                    }
                    // Append "(fighting <target-mob>)" when engaged
                    // against another mob; player-target case is left
                    // implicit (the player's own listing shows it).
                    if let Some(t) = m.fighting.filter(|t| !t.is_player) {
                        if let Some(label) = w.mob_instances.iter().find(|x| x.id == t.id)
                            .and_then(|x| w.mob_protos.get(&x.vnum))
                            .map(|p| p.short_descr.clone())
                        {
                            s.push_str(" (fighting ");
                            s.push_str(&label);
                            s.push(')');
                        }
                    }
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
                c.affects.iter().any(|a|
                    a.skill == crate::character::Skill::DetectInvis
                    || a.skill == crate::character::Skill::SenseLife)
            }
            None => false,
        }
    } else { false };

    for p in cl.iter() {
        if p.current_room != vnum { continue; }
        if Some(p.id) == viewer_id { continue; }
        let (hidden, invisible_aff, pose, invis_lvl, position, fighting) = {
            let c = p.character.lock().await;
            let invisible = c.affects.iter()
                .any(|a| a.skill == crate::character::Skill::Invisibility);
            (c.hidden, invisible, c.pose.clone(), c.invis_level, c.position, c.fighting)
        };
        // Immortal invis hides them from anyone below `invis_level`.
        if invis_lvl > viewer_level { continue; }
        let invisible = hidden || invisible_aff;
        if !see_hidden && invisible { continue; }
        let hidden_tag = if see_hidden && invisible { " (invis)" } else { "" };
        let invis_tag  = if invis_lvl > 0 { format!(" [invis{}]", invis_lvl) } else { String::new() };
        // "(fighting X)" suffix when engaged — needs a fresh world lock.
        let fight_tag: String = if let Some(t) = fighting {
            let w = world.lock().await;
            let name = if t.is_player {
                None  // skip cross-player fight tag here; sender's own line shows it
            } else {
                w.mob_instances.iter().find(|x| x.id == t.id)
                    .and_then(|x| w.mob_protos.get(&x.vnum))
                    .map(|p| p.short_descr.clone())
            };
            match name {
                Some(n) => format!(" (fighting {n})"),
                None => String::new(),
            }
        } else { String::new() };
        if !pose.is_empty() {
            s.push_str(&format!("{} {pose}{fight_tag}{hidden_tag}{invis_tag}\r\n", p.name));
        } else {
            s.push_str(&format!(
                "{} {}.{fight_tag}{hidden_tag}{invis_tag}\r\n",
                p.name, position.room_verb(),
            ));
        }
    }

    s
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_obj_by_id(w: &World, iid: u32) -> Option<&crate::world::ObjInstance> {
    w.obj_instances.iter().find(|o| o.id == iid)
}

/// Concatenate a mob's inventory and equipped object ids into the
/// single Vec that gets handed to `create_corpse`.
pub fn mob_corpse_contents(m: &crate::world::MobInstance) -> Vec<u32> {
    let mut v = m.inventory.clone();
    for s in m.equipment.iter().flatten() {
        v.push(*s);
    }
    v
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
    // Synthetic coin pile (cp223) — render with its actual amount.
    if obj.vnum == crate::db::GOLD_PILE_VNUM {
        let n = obj.gold_amount;
        let (short, long) = if n == 1 {
            ("a single gold coin".to_string(), "A single gold coin is lying here.".to_string())
        } else {
            (format!("a pile of {n} gold coins"),
             format!("A pile of {n} gold coins is lying here."))
        };
        return ObjView {
            short, long,
            item_type: crate::world::ITEM_MONEY,
            keywords: "pile coins gold money".to_string(),
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
