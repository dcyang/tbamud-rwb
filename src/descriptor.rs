/// Per-connection state and I/O handler.
/// Mirrors descriptor_data (structs.h) and the per-descriptor logic in comm.c:
///   new_descriptor(), process_input(), process_output(), close_socket().

use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use bytes::{BufMut, BytesMut};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, Mutex},
};
use tracing::{debug, info, warn};

use crate::{
    character::{Character, PlayerHandle, SharedChars},
    interpreter,
    login::{GameTexts, LoginSession},
    players::PlayerDb,
    telnet,
    world::World,
};

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

/// Connection state — mirrors the CON_* defines in structs.h.
/// Only login-flow states are included here; OLC states will be added later.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ConnState {
    /// Awaiting player name (CON_GET_NAME)
    #[default]
    GetName,
    /// New character, waiting for name confirmation (CON_NAME_CNFRM)
    NameConfirm,
    /// Existing player entering password (CON_PASSWORD)
    Password,
    /// New character, entering password (CON_NEWPASSWD)
    NewPassword,
    /// New character, confirming password (CON_CNFPASSWD)
    ConfirmPassword,
    /// Choosing character sex (CON_QSEX)
    SelectSex,
    /// Choosing character class (CON_QCLASS)
    SelectClass,
    /// Waiting for Return after MOTD (CON_RMOTD)
    ReadMotd,
    /// At the main menu (CON_MENU)
    Menu,
    /// In the game (CON_PLAYING)
    Playing,
    /// Connection closing (CON_CLOSE)
    Close,
}

// ---------------------------------------------------------------------------
// Main per-connection async handler
// ---------------------------------------------------------------------------

/// Handle a single accepted TCP connection end-to-end.
///
/// Flow:
///   1. Send telnet negotiation hints + greeting
///   2. Run the login state machine (login::LoginSession) until the player
///      enters the game or disconnects
///   3. TODO: hand off to game command loop once CON_PLAYING is reached
pub async fn handle_connection(
    id: usize,
    mut stream: TcpStream,
    peer: SocketAddr,
    greeting: Arc<String>,
    players: Arc<Mutex<PlayerDb>>,
    texts: Arc<GameTexts>,
    xnames: Arc<Vec<String>>,
    world: Arc<Mutex<World>>,
    chars: SharedChars,
) -> Result<()> {
    let host = peer.ip().to_string();
    info!(id, host = %host, "Connection accepted");

    // --- Site ban check ------------------------------------------------------
    if let Some(bs) = crate::interpreter::BAD_SITES.get() {
        let banned = {
            let g = bs.lock().await;
            let lowered = host.to_lowercase();
            g.iter().any(|s| lowered.contains(s))
        };
        if banned {
            let _ = stream.write_all(b"Sorry, this site is banned.\r\n").await;
            info!(id, host = %host, "Refused banned site");
            return Ok(());
        }
    }

    // --- Telnet negotiation handshake ----------------------------------------
    // Send WILL SUPPRESS-GA, DO NAWS, DO TTYPE.
    // Mirrors init_descriptor() → ProtocolNegotiate() in comm.c.
    let mut init_buf = Vec::with_capacity(9);
    init_buf.extend_from_slice(&telnet::cmd_suppress_ga());
    init_buf.extend_from_slice(&telnet::cmd_do_naws());
    init_buf.extend_from_slice(&telnet::cmd_do_ttype());
    stream.write_all(&init_buf).await?;

    // --- Send greeting -------------------------------------------------------
    // Mirrors the GREETINGS send in new_descriptor() (comm.c:1542).
    let greeting_crlf = crate::color::convert(&greeting.replace('\n', "\r\n"));
    stream.write_all(greeting_crlf.as_bytes()).await?;

    // --- Login state machine --------------------------------------------------
    let mut session = LoginSession::new();
    let mut raw_buf = BytesMut::with_capacity(4096);
    let mut read_tmp = [0u8; 1024];

    loop {
        // Check if state machine requested a close
        if session.state == ConnState::Close {
            break;
        }

        // Read available bytes (may contain telnet IAC sequences)
        let n = match stream.read(&mut read_tmp).await {
            Ok(0) => {
                info!(id, host = %host, "EOF — client disconnected");
                break;
            }
            Ok(n) => n,
            Err(e) => {
                warn!(id, host = %host, error = %e, "Read error");
                break;
            }
        };

        raw_buf.put_slice(&read_tmp[..n]);
        debug!(id, bytes = n, "Received raw bytes");

        // Strip IAC sequences
        let clean = telnet::strip_telnet(&raw_buf);
        raw_buf.clear();

        // Collect complete lines (CR/LF terminated).
        // Treat \r\n and \n\r as a single delimiter (mirrors the CR/LF handling
        // in process_input() in comm.c which uses ISNEWL() on individual bytes
        // but only enqueues one command per CR/LF or bare CR/LF pair).
        let mut lines: Vec<String> = Vec::new();
        let mut i = 0;
        let mut line_start = 0;
        while i < clean.len() {
            let b = clean[i];
            if b == b'\r' || b == b'\n' {
                let chunk = &clean[line_start..i];
                let text = String::from_utf8_lossy(chunk).into_owned();
                lines.push(text);
                // Consume a paired \r\n or \n\r as one delimiter
                if i + 1 < clean.len()
                    && (clean[i + 1] == b'\r' || clean[i + 1] == b'\n')
                    && clean[i + 1] != b
                {
                    i += 1;
                }
                line_start = i + 1;
            }
            i += 1;
        }
        // Put any incomplete (unterminated) line back for the next read
        if line_start < clean.len() {
            raw_buf.put_slice(&clean[line_start..]);
        }

        // Process each complete line through the login state machine
        for line in lines {
            debug!(id, state = ?session.state, input = %line, "Processing input");

            let output = session.process(&line, &players, &xnames, &texts).await;

            // Send echo_on before text if requested (re-enables client echo)
            if output.echo_on {
                stream.write_all(&telnet::cmd_echo_on()).await?;
            }

            // Write response text (color-rendered).
            if !output.text.is_empty() {
                let rendered = crate::color::convert(&output.text);
                stream.write_all(rendered.as_bytes()).await?;
            }

            // Send echo_off after text if requested (suppresses client echo for next input)
            if output.echo_off {
                stream.write_all(&telnet::cmd_echo_off()).await?;
            }

            if output.disconnect {
                info!(id, host = %host, "Disconnecting by login state machine");
                return Ok(());
            }

            if output.entered_game {
                info!(id, host = %host, "Player entered game");
                let immortal = session.level >= 34;
                let start = world.lock().await.start_room(immortal);

                let pname = session.player.as_ref()
                    .map(|p| p.name.clone())
                    .or_else(|| session.player_name.clone())
                    .unwrap_or_else(|| "Someone".to_string());

                // Build the character. id reuses the connection id for now.
                // If the player file has saved HP/Room/Gold, use those;
                // otherwise initialise defaults.  Same for ability scores —
                // any zero scores get freshly rolled (3d6) on this login.
                let p_ref     = session.player.as_ref();
                // Class-aware default HP: warriors get more, magic-users less.
                let cls       = p_ref.map(|p| p.class).unwrap_or(crate::players::Class::Undefined);
                let con_score = p_ref.map(|p| p.con).filter(|v| *v > 0).unwrap_or(12);
                let default_hp = Character::init_hp_for_class(cls, con_score, session.level.max(1));
                let max_hp    = p_ref.map(|p| p.max_hp).filter(|h| *h > 0).unwrap_or(default_hp);
                let hp        = p_ref.map(|p| p.hp).filter(|h| *h > 0).unwrap_or(max_hp);
                // Mana: arcane scales with INT, divine with WIS, others use INT.
                let casting_stat = if cls == crate::players::Class::Cleric {
                    p_ref.map(|p| p.wis).filter(|v| *v > 0).unwrap_or(12)
                } else {
                    p_ref.map(|p| p.int_).filter(|v| *v > 0).unwrap_or(12)
                };
                let default_mana = Character::init_mana_for_class(cls, casting_stat, session.level.max(1));
                let max_mana   = p_ref.map(|p| p.max_mana).filter(|m| *m > 0).unwrap_or(default_mana);
                let mana       = p_ref.map(|p| p.mana).filter(|m| *m > 0).unwrap_or(max_mana);
                // Movement points: flat 100 default for mortals.  Immortals
                // get a big pool so they never feel travel-limited.
                let default_max_mv = if session.level >= 34 { 9999 } else { 100 };
                let max_movement = p_ref.map(|p| p.max_movement).filter(|v| *v > 0).unwrap_or(default_max_mv);
                let movement     = p_ref.map(|p| p.movement).filter(|v| *v > 0).unwrap_or(max_movement);
                let practices  = p_ref.map(|p| p.practices).unwrap_or(0);
                let room      = p_ref.map(|p| p.room).filter(|r| *r != 0).unwrap_or(start);
                let gold      = p_ref.map(|p| p.gold).unwrap_or(0);
                let immortal  = session.level >= 34;
                let ab        = |v: i32| if v > 0 { v } else { Character::roll_ability(immortal) };
                let mut me = Character {
                    id:           id as u32,
                    name:         pname.clone(),
                    level:        session.level,
                    sex:          p_ref.map(|p| p.sex).unwrap_or(crate::players::Sex::Neutral),
                    class:        p_ref.map(|p| p.class).unwrap_or(crate::players::Class::Undefined),
                    current_room: room,
                    inventory:    Vec::new(),
                    equipment:    Default::default(),
                    gold,
                    bank_gold:    p_ref.map(|p| p.bank_gold).unwrap_or(0),
                    exp:          p_ref.map(|p| p.exp).unwrap_or(0),
                    hp,
                    max_hp,
                    mana,
                    max_mana,
                    movement,
                    max_movement,
                    position:     p_ref.and_then(|p| crate::character::Position::parse(&p.position))
                                    .unwrap_or(crate::character::Position::Standing),
                    wimpy:        p_ref.map(|p| p.wimpy).unwrap_or(0),
                    info_off:     false,
                    shout_off:    false,
                    color_off:    p_ref.map(|p| p.color_off).unwrap_or(false),
                    autoexit:     p_ref.map(|p| p.autoexit).unwrap_or(false),
                    autoloot:     p_ref.map(|p| p.autoloot).unwrap_or(false),
                    autoassist:   p_ref.map(|p| p.autoassist).unwrap_or(false),
                    autotitle:    !p_ref.map(|p| p.autotitle_off).unwrap_or(false),
                    history:      std::collections::VecDeque::with_capacity(20),
                    tell_history: std::collections::VecDeque::with_capacity(20),
                    alignment:    p_ref.map(|p| p.alignment).unwrap_or(0),
                    clan:         p_ref.map(|p| p.clan.clone()).unwrap_or_default(),
                    pkills:       p_ref.map(|p| p.pkills).unwrap_or(0),
                    pdeaths:      p_ref.map(|p| p.pdeaths).unwrap_or(0),
                    snooped_by:   Vec::new(),
                    snooping:     None,
                    group_invite_from: None,
                    clan_invite_from:  None,
                    practices,
                    str_:         ab(p_ref.map(|p| p.str_).unwrap_or(0)),
                    int_:         ab(p_ref.map(|p| p.int_).unwrap_or(0)),
                    wis:          ab(p_ref.map(|p| p.wis ).unwrap_or(0)),
                    dex:          ab(p_ref.map(|p| p.dex ).unwrap_or(0)),
                    con:          ab(p_ref.map(|p| p.con ).unwrap_or(0)),
                    cha:          ab(p_ref.map(|p| p.cha ).unwrap_or(0)),
                    fighting:     None,
                    skills:       {
                        // Translate saved skill names → Skill enum values.
                        let mut m = std::collections::HashMap::new();
                        if let Some(p) = p_ref {
                            for (k, v) in &p.skills {
                                if let Some(skill) = crate::character::Skill::from_save_key(k) {
                                    m.insert(skill, *v);
                                }
                            }
                        }
                        m
                    },
                    affects:      Vec::new(),
                    sneaking:     false,
                    hidden:       false,
                    active_quest:    p_ref.and_then(|p| p.active_quest),
                    quest_progress:  p_ref.map(|p| p.quest_progress).unwrap_or(0),
                    completed_quests: p_ref.map(|p| p.completed_quests.clone()).unwrap_or_default(),
                    hunger:           if session.level >= 34 { -1 } else { p_ref.map(|p| p.hunger).unwrap_or(24) },
                    thirst:           if session.level >= 34 { -1 } else { p_ref.map(|p| p.thirst).unwrap_or(24) },
                    title:            {
                        let saved = p_ref.map(|p| p.title.clone()).unwrap_or_default();
                        if saved.is_empty() {
                            // Seed a default title from the (class, level).
                            let cls = p_ref.map(|p| p.class).unwrap_or(crate::players::Class::Undefined);
                            Character::default_title_for(cls, session.level.max(1)).to_string()
                        } else { saved }
                    },
                    bonus_hitroll:    0,
                    bonus_damroll:    0,
                    bonus_ac:         0,
                    following:        None,
                    grouped:          false,
                    gossip_off:       false,
                    auction_off:      false,
                    wiznet_off:       false,
                    brief:            false,
                    compact:          false,
                    last_tell_from:   None,
                    prompt_format:    p_ref.map(|p| p.prompt_format.clone()).unwrap_or_default(),
                    aliases:          p_ref.map(|p| p.aliases.clone()).unwrap_or_default(),
                    notes:            p_ref.map(|p| p.notes.clone()).unwrap_or_default(),
                    pose:             p_ref.map(|p| p.pose.clone()).unwrap_or_default(),
                    pvp_ok:           false,
                    invis_level:      0,
                    god:              p_ref.map(|p| p.god.clone()).unwrap_or_default(),
                    muted:            p_ref.map(|p| p.muted).unwrap_or(false),
                    frozen:           p_ref.map(|p| p.frozen).unwrap_or(false),
                    afk_msg:          None,
                    last_activity:    std::time::Instant::now(),
                    recall_cooldown_until: None,
                };

                // Settle any pending level-ups (e.g. character was offline
                // when their XP crossed thresholds).  This always runs on
                // login; it's a no-op for characters with consistent state.
                let _ = me.check_level_up();

                // Restore persisted inventory + equipment by spawning fresh
                // ObjInstances of the saved vnums.  For containers, also
                // spawn each content vnum and link it into the container.
                let data_dir = players.lock().await.data_dir().to_string();
                {
                    let entries = crate::players::load_objs(&data_dir, &pname);
                    if !entries.is_empty() {
                        let mut w = world.lock().await;
                        for e in entries {
                            if let Some(iid) = w.spawn_obj(e.vnum) {
                                // Restore persisted condition + brewed-spell + bonuses.
                                if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
                                    o.condition = e.condition;
                                    o.brewed_spell = e.brewed_spell;
                                    o.bonus_affects = e.bonus_affects.iter()
                                        .map(|(l, m)| crate::world::ObjAffect {
                                            location: *l, modifier: *m,
                                        })
                                        .collect();
                                }
                                // Spawn any container contents and link them.
                                for &cvnum in &e.contents {
                                    if let Some(cid) = w.spawn_obj(cvnum) {
                                        if let Some(container) = w.obj_instances
                                            .iter_mut().find(|o| o.id == iid)
                                        {
                                            container.contents.push(cid);
                                        }
                                    }
                                }
                                match e.slot {
                                    crate::players::SavedObjSlot::Inv => {
                                        me.inventory.push(iid);
                                    }
                                    crate::players::SavedObjSlot::Wear(n) => {
                                        let n = n as usize;
                                        if n < crate::character::NUM_WEARS {
                                            me.equipment[n] = Some(iid);
                                        } else {
                                            me.inventory.push(iid);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // First-login newbie kit.  Brand-new chars get a
                // synthetic class-appropriate kit if their persisted
                // last_login is still 0 AND nothing was restored above.
                {
                    let fresh = p_ref.map(|p| p.last_login).unwrap_or(0) == 0;
                    // Seed class-allowed skills/spells to a baseline
                    // percentage so newbies aren't completely useless.
                    if fresh && me.skills.is_empty() {
                        for &sk in crate::character::ALL_SKILLS {
                            if sk.is_class_allowed(me.class) {
                                let pct = match sk.kind() {
                                    crate::character::SkillKind::Physical => 30,
                                    crate::character::SkillKind::Spell    => 25,
                                };
                                me.skills.insert(sk, pct);
                            }
                        }
                    }
                    let nothing = me.inventory.is_empty()
                        && me.equipment.iter().all(|s| s.is_none());
                    if fresh && nothing {
                        let mut w = world.lock().await;
                        let kit: &[crate::world::ObjVnum] = match me.class {
                            crate::players::Class::Warrior =>
                                &[crate::db::NEWBIE_WEAPON_VNUM,
                                  crate::db::NEWBIE_ARMOR_VNUM,
                                  crate::db::NEWBIE_LIGHT_VNUM,
                                  crate::db::NEWBIE_BREAD_VNUM],
                            crate::players::Class::Cleric |
                            crate::players::Class::MagicUser =>
                                &[crate::db::NEWBIE_ARMOR_VNUM,
                                  crate::db::NEWBIE_LIGHT_VNUM,
                                  crate::db::NEWBIE_BREAD_VNUM],
                            crate::players::Class::Thief =>
                                &[crate::db::NEWBIE_WEAPON_VNUM,
                                  crate::db::NEWBIE_LIGHT_VNUM,
                                  crate::db::NEWBIE_BREAD_VNUM],
                            _ =>
                                &[crate::db::NEWBIE_LIGHT_VNUM,
                                  crate::db::NEWBIE_BREAD_VNUM],
                        };
                        for &vnum in kit {
                            if let Some(iid) = w.spawn_obj(vnum) {
                                me.inventory.push(iid);
                            }
                        }
                    }
                }

                run_game_session(id, host.clone(), stream, me, world, chars, Arc::clone(&players), data_dir).await?;
                return Ok(());
            }
        }
    }

    info!(id, "Connection closed");
    Ok(())
}

// ---------------------------------------------------------------------------
// In-game session: split socket, spawn writer task, register player, run
// the command-interpreter loop.
// ---------------------------------------------------------------------------

/// Run a player's in-game session from the moment they enter the world
/// until they quit or disconnect.
async fn run_game_session(
    id: usize,
    host: String,
    stream: TcpStream,
    me: Character,
    world: Arc<Mutex<World>>,
    chars: SharedChars,
    players: Arc<Mutex<PlayerDb>>,
    data_dir: String,
) -> Result<()> {
    // Split TCP for concurrent read/write
    let (mut read_half, mut write_half) = stream.into_split();

    // Outbound channel: other connections push messages via this, the writer
    // task drains it. Bound is unbounded for now; bursts during say/who are
    // tiny and we'd rather drop bytes on close than spin on send pressure.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // Stash the character behind an Arc<Mutex<>> so the combat tick task
    // can mutate HP/fighting concurrently with this connection.
    let my_id   = me.id;
    let my_name = me.name.clone();
    let my_room = me.current_room;
    let character = Arc::new(Mutex::new(me));

    // Register in shared player list so others can find us.
    {
        let mut cl = chars.lock().await;
        cl.add(PlayerHandle {
            id:           my_id,
            name:         my_name.clone(),
            level:        character.lock().await.level,
            current_room: my_room,
            send:         tx.clone(),
            character:    Arc::clone(&character),
        });
        // Broadcast arrival to the start room.
        cl.broadcast_room(
            my_room, Some(my_id),
            &format!("{} has entered the world.\r\n", my_name),
        );
    }

    // Wiznet broadcast (drops the cl lock first — broadcast_wiznet
    // re-acquires chars internally).
    crate::interpreter::broadcast_wiznet(
        &format!("{my_name} has connected."),
        &chars,
    ).await;

    // Send the welcome + initial room view via the channel so it goes through
    // the same writer task as everything else.
    let _ = tx.send("\r\nWelcome to tbaMUD!  May your visit here be... Enlightening\r\n".to_string());
    // Notify if they have mail.
    {
        let data_dir = players.lock().await.data_dir().to_string();
        let msgs = crate::mail::load_mailbox(&data_dir, &my_name);
        if !msgs.is_empty() {
            let _ = tx.send(format!(
                "\r\nYou have {} message(s) in your mailbox.  Type `mail list` to read.\r\n",
                msgs.len(),
            ));
        }
    }
    let _ = tx.send(interpreter::render_room(my_room, Some(my_id), &world, &chars).await);
    let _ = tx.send("\r\n> ".to_string());

    // Writer task: drains the channel to the socket. Exits when the channel
    // closes (all senders dropped) or the socket errors.
    let writer_ch = Arc::clone(&character);
    let writer_chars = Arc::clone(&chars);
    let writer_name = my_name.clone();
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            // Snapshot color preference + snooper id list under one lock.
            let (color_off, snoopers): (bool, Vec<u32>) = {
                let c = writer_ch.lock().await;
                (c.color_off, c.snooped_by.clone())
            };
            // Fan out to snoopers BEFORE we strip color (so the
            // snooper sees raw @-codes if they have color enabled).
            if !snoopers.is_empty() {
                let snoop_line = format!("%{writer_name}% {msg}");
                let handles: Vec<crate::character::PlayerHandle> = {
                    let cl = writer_chars.lock().await;
                    cl.iter().cloned().collect()
                };
                for sid in &snoopers {
                    if let Some(ph) = handles.iter().find(|p| p.id == *sid) {
                        let _ = ph.send.send(snoop_line.clone());
                    }
                }
            }
            let rendered = if color_off {
                crate::color::strip(&msg)
            } else {
                crate::color::convert(&msg)
            };
            if write_half.write_all(rendered.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // Reader/dispatcher loop
    let mut raw_buf = BytesMut::with_capacity(4096);
    let mut read_tmp = [0u8; 1024];
    let mut quit = false;

    'outer: loop {
        let n = match read_half.read(&mut read_tmp).await {
            Ok(0) => {
                info!(id, host = %host, name = %my_name, "EOF in game loop");
                break;
            }
            Ok(n) => n,
            Err(e) => {
                warn!(id, host = %host, error = %e, "Read error in game loop");
                break;
            }
        };

        raw_buf.put_slice(&read_tmp[..n]);
        let clean = telnet::strip_telnet(&raw_buf);
        raw_buf.clear();

        // Split on CR/LF, treating paired \r\n / \n\r as one delimiter.
        let mut lines: Vec<String> = Vec::new();
        let mut i = 0;
        let mut line_start = 0;
        while i < clean.len() {
            let b = clean[i];
            if b == b'\r' || b == b'\n' {
                let chunk = &clean[line_start..i];
                lines.push(String::from_utf8_lossy(chunk).into_owned());
                if i + 1 < clean.len()
                    && (clean[i + 1] == b'\r' || clean[i + 1] == b'\n')
                    && clean[i + 1] != b
                {
                    i += 1;
                }
                line_start = i + 1;
            }
            i += 1;
        }
        if line_start < clean.len() {
            raw_buf.put_slice(&clean[line_start..]);
        }

        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                let _ = tx.send("> ".to_string());
                continue;
            }
            debug!(id, name = %my_name, cmd = %line, "command");
            let out = {
                let mut me = character.lock().await;
                interpreter::dispatch_command(line, &mut me, &world, &chars, &players).await
            };
            if !out.text.is_empty() {
                let _ = tx.send(out.text);
            }
            if out.quit {
                quit = true;
                break 'outer;
            }
            let prompt = {
                let c = character.lock().await;
                interpreter::render_prompt(&c)
            };
            let _ = tx.send(prompt);
        }
    }

    // Tear down: remove from registry, broadcast departure, drop sender so
    // the writer task exits naturally.
    let from_room = character.lock().await.current_room;

    // Auto-save on quit or disconnect (best-effort).
    {
        let me = character.lock().await;
        let players_guard = players.lock().await;
        if let Ok(mut rec) = players_guard.load_player(&my_name) {
            rec.hp        = me.hp;
            rec.max_hp    = me.max_hp;
            rec.mana      = me.mana;
            rec.max_mana  = me.max_mana;
            rec.movement     = me.movement;
            rec.max_movement = me.max_movement;
            rec.position     = me.position.save_key().to_string();
            rec.wimpy        = me.wimpy;
            rec.color_off    = me.color_off;
            rec.autoexit     = me.autoexit;
            rec.autoloot     = me.autoloot;
            rec.autoassist   = me.autoassist;
            rec.autotitle_off = !me.autotitle;
            rec.alignment    = me.alignment;
            rec.clan         = me.clan.clone();
            rec.pkills       = me.pkills;
            rec.pdeaths      = me.pdeaths;
            rec.practices = me.practices;
            rec.room      = me.current_room;
            rec.gold      = me.gold;
            rec.exp       = me.exp;
            rec.level     = me.level;
            rec.str_      = me.str_;
            rec.int_   = me.int_;
            rec.wis    = me.wis;
            rec.dex    = me.dex;
            rec.con    = me.con;
            rec.cha    = me.cha;
            rec.skills.clear();
            for (skill, pct) in &me.skills {
                rec.skills.insert(skill.save_key().to_string(), *pct);
            }
            rec.active_quest    = me.active_quest;
            rec.quest_progress  = me.quest_progress;
            rec.completed_quests = me.completed_quests.clone();
            rec.hunger          = me.hunger;
            rec.thirst          = me.thirst;
            rec.title           = me.title.clone();
            rec.bank_gold       = me.bank_gold;
            rec.prompt_format   = me.prompt_format.clone();
            rec.aliases         = me.aliases.clone();
            rec.last_login      = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64).unwrap_or(rec.last_login);
            rec.god             = me.god.clone();
            rec.muted           = me.muted;
            rec.frozen          = me.frozen;
            rec.notes           = me.notes.clone();
            rec.pose            = me.pose.clone();
            if let Err(e) = players_guard.save_player(&rec) {
                warn!(name = %my_name, error = %e, "auto-save failed at session end");
            }
        }
    }

    // Persist + extract inventory & equipment. We walk both lists; for
    // each instance, capture its vnum + (for containers) the vnums it
    // holds, then drop those ObjInstances so the world reset can refill.
    {
        let me = character.lock().await;
        let mut entries: Vec<crate::players::SavedObj> = Vec::new();
        let mut to_remove: Vec<u32> = Vec::new();

        let collect = |w: &World, iid: u32, slot: crate::players::SavedObjSlot|
            -> Option<(crate::players::SavedObj, Vec<u32>)>
        {
            let o = w.obj_instances.iter().find(|o| o.id == iid)?;
            // Pull the content vnums from instance.contents.
            let mut content_vnums = Vec::new();
            let mut to_remove_local = vec![iid];
            for &cid in &o.contents {
                if let Some(c) = w.obj_instances.iter().find(|o| o.id == cid) {
                    content_vnums.push(c.vnum);
                    to_remove_local.push(cid);
                }
            }
            let bonus_affects: Vec<(i32, i32)> = o.bonus_affects.iter()
                .map(|a| (a.location, a.modifier)).collect();
            Some((
                crate::players::SavedObj {
                    vnum: o.vnum,
                    slot,
                    condition: o.condition,
                    brewed_spell: o.brewed_spell,
                    bonus_affects,
                    contents: content_vnums,
                },
                to_remove_local,
            ))
        };

        let w = world.lock().await;
        for &iid in &me.inventory {
            if let Some((e, rms)) = collect(&w, iid, crate::players::SavedObjSlot::Inv) {
                entries.push(e);
                to_remove.extend(rms);
            }
        }
        for (slot_idx, slot) in me.equipment.iter().enumerate() {
            if let Some(iid) = slot {
                if let Some((e, rms)) = collect(
                    &w, *iid, crate::players::SavedObjSlot::Wear(slot_idx as u8),
                ) {
                    entries.push(e);
                    to_remove.extend(rms);
                }
            }
        }
        drop(w);

        if let Err(e) = crate::players::save_objs(&data_dir, &my_name, &entries) {
            warn!(name = %my_name, error = %e, "objs save failed");
        }

        if !to_remove.is_empty() {
            let mut w = world.lock().await;
            w.obj_instances.retain(|o| !to_remove.contains(&o.id));
        }
    }

    // Snoop cleanup before deregistration: detach any snoopers, and if
    // we were snooping someone, remove our id from their snooped_by.
    {
        let (snooping, snoopers) = {
            let c = character.lock().await;
            (c.snooping, c.snooped_by.clone())
        };
        if let Some(tid) = snooping {
            let tph = {
                let cl = chars.lock().await;
                let h = cl.iter().find(|p| p.id == tid).cloned();
                h
            };
            if let Some(tph) = tph {
                tph.character.lock().await.snooped_by.retain(|&i| i != my_id);
            }
        }
        if !snoopers.is_empty() {
            let cl = chars.lock().await;
            let handles: Vec<crate::character::PlayerHandle> = cl.iter().cloned().collect();
            drop(cl);
            for sid in snoopers {
                if let Some(sph) = handles.iter().find(|p| p.id == sid) {
                    sph.character.lock().await.snooping = None;
                    let _ = sph.send.send("\r\nYour snoop target has gone offline.\r\n".to_string());
                }
            }
        }
    }

    {
        let mut cl = chars.lock().await;
        cl.remove(my_id);
        let verb = if quit { "leaves" } else { "vanishes into thin air" };
        cl.broadcast_room(
            from_room, None,
            &format!("{} {}.\r\n", my_name, verb),
        );
    }

    crate::interpreter::broadcast_wiznet(
        &format!(
            "{my_name} has {}.",
            if quit { "quit" } else { "disconnected" },
        ),
        &chars,
    ).await;

    drop(tx);
    let _ = writer.await;

    info!(id, name = %my_name, "session ended");
    Ok(())
}
