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
    let greeting_crlf = greeting.replace('\n', "\r\n");
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

            // Write response text
            if !output.text.is_empty() {
                stream.write_all(output.text.as_bytes()).await?;
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
                    exp:          p_ref.map(|p| p.exp).unwrap_or(0),
                    hp,
                    max_hp,
                    mana,
                    max_mana,
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

    // Send the welcome + initial room view via the channel so it goes through
    // the same writer task as everything else.
    let _ = tx.send("\r\nWelcome to tbaMUD!  May your visit here be... Enlightening\r\n".to_string());
    let _ = tx.send(interpreter::render_room(my_room, Some(my_id), &world, &chars).await);
    let _ = tx.send("\r\n> ".to_string());

    // Writer task: drains the channel to the socket. Exits when the channel
    // closes (all senders dropped) or the socket errors.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_half.write_all(msg.as_bytes()).await.is_err() {
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
            let _ = tx.send("\r\n> ".to_string());
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
            Some((
                crate::players::SavedObj { vnum: o.vnum, slot, contents: content_vnums },
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

    {
        let mut cl = chars.lock().await;
        cl.remove(my_id);
        let verb = if quit { "leaves" } else { "vanishes into thin air" };
        cl.broadcast_room(
            from_room, None,
            &format!("{} {}.\r\n", my_name, verb),
        );
    }

    drop(tx);
    let _ = writer.await;

    info!(id, name = %my_name, "session ended");
    Ok(())
}
