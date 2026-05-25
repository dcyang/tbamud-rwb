/// Per-connection state and I/O handler.
/// Mirrors descriptor_data (structs.h) and the per-descriptor logic in comm.c:
///   new_descriptor(), process_input(), process_output(), close_socket().

use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use bytes::{BufMut, BytesMut};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
};
use tracing::{debug, info, warn};

use crate::{
    login::{GameTexts, LoginSession},
    players::PlayerDb,
    telnet,
    world::{Direction, Room, World},
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
    world: Arc<World>,
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
                // Determine starting room: immortals get the immort start,
                // mortals get the mortal start, with fallbacks in start_room().
                let immortal = session.level >= 34;
                let start = world.start_room(immortal);
                let pname = session.player.as_ref()
                    .map(|p| p.name.clone())
                    .or_else(|| session.player_name.clone())
                    .unwrap_or_else(|| "Someone".to_string());
                handle_game_loop(
                    id, &host, &mut stream,
                    Arc::clone(&world),
                    pname,
                    start,
                ).await?;
                return Ok(());
            }
        }
    }

    info!(id, "Connection closed");
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal in-game command loop
// ---------------------------------------------------------------------------

/// In-game command loop.  Supports `look`, the six movement commands, and
/// `quit`.  Acts as a tiny stand-in for command_interpreter() in interpreter.c
/// until that's fully ported.
async fn handle_game_loop(
    id: usize,
    host: &str,
    stream: &mut TcpStream,
    world: Arc<World>,
    name: String,
    start_room: crate::world::RoomVnum,
) -> Result<()> {
    let mut current = start_room;

    // Show the starting room
    if let Some(r) = world.room(current) {
        stream.write_all(render_room(r, &world).as_bytes()).await?;
    } else {
        stream.write_all(b"\r\nYou are nowhere. The world has not loaded properly.\r\n").await?;
    }
    stream.write_all(b"\r\n> ").await?;

    let mut raw_buf = BytesMut::with_capacity(4096);
    let mut read_tmp = [0u8; 1024];

    loop {
        let n = match stream.read(&mut read_tmp).await {
            Ok(0) => {
                info!(id, host = %host, name = %name, "EOF in game loop");
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

        let mut lines: Vec<String> = Vec::new();
        let mut i = 0;
        let mut line_start = 0;
        while i < clean.len() {
            let b = clean[i];
            if b == b'\r' || b == b'\n' {
                let chunk = &clean[line_start..i];
                let text = String::from_utf8_lossy(chunk).into_owned();
                lines.push(text);
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
            let cmd = line.trim();
            if cmd.is_empty() {
                stream.write_all(b"> ").await?;
                continue;
            }

            let reply = execute_command(cmd, &mut current, &world);
            stream.write_all(reply.as_bytes()).await?;

            if cmd.eq_ignore_ascii_case("quit") {
                return Ok(());
            }
            stream.write_all(b"\r\n> ").await?;
        }
    }
    Ok(())
}

/// Process a single command line.  Returns the text to write back (no
/// trailing prompt).
fn execute_command(cmd: &str, current: &mut crate::world::RoomVnum, world: &World) -> String {
    let lower = cmd.to_ascii_lowercase();

    // Movement
    if let Some(dir) = Direction::parse(&lower) {
        return do_move(dir, current, world);
    }

    match lower.as_str() {
        "l" | "look" => match world.room(*current) {
            Some(r) => render_room(r, world),
            None => "\r\nYou are nowhere.".to_string(),
        },
        "quit" => "Goodbye.\r\n".to_string(),
        _ => format!("\r\nHuh?!? ({cmd})"),
    }
}

/// Attempt to move in the given direction from the current room.
fn do_move(
    dir: Direction,
    current: &mut crate::world::RoomVnum,
    world: &World,
) -> String {
    let Some(room) = world.room(*current) else {
        return "\r\nYou are nowhere; you cannot move.".to_string();
    };

    match &room.exits[dir as usize] {
        Some(exit) if exit.to_room != crate::world::NOWHERE
            && world.rooms.contains_key(&exit.to_room) =>
        {
            *current = exit.to_room;
            match world.room(*current) {
                Some(r) => render_room(r, world),
                None    => "\r\nYou stumble into the void.".to_string(),
            }
        }
        _ => format!("\r\nAlas, you cannot go that way..."),
    }
}

/// Render a room's name, description, obvious exits, objects on the ground,
/// and mobs present.  Mirrors look_at_room() in act.informative.c (minimal
/// subset — no light/sneak/invisibility, no colour).
fn render_room(r: &Room, world: &World) -> String {
    let mut s = String::with_capacity(r.description.len() + 512);
    s.push_str("\r\n");
    s.push_str(&r.name);
    s.push_str("\r\n");
    for line in r.description.split('\n') {
        s.push_str(line);
        s.push_str("\r\n");
    }

    // Obvious exits
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

    // Objects on the ground — use prototype's long description (the "lies
    // here" line). Mirror the "list_obj_to_char(LIST_NORMAL)" branch.
    for &iid in &r.objects {
        if let Some(inst) = world.obj_instances.iter().find(|o| o.id == iid) {
            if let Some(proto) = world.obj_protos.get(&inst.vnum) {
                if !proto.description.is_empty() {
                    s.push_str(&proto.description);
                    s.push_str("\r\n");
                }
            }
        }
    }

    // Mobs in room — use prototype long_descr (the "is here" line).
    for &iid in &r.mobs {
        if let Some(inst) = world.mob_instances.iter().find(|m| m.id == iid) {
            if let Some(proto) = world.mob_protos.get(&inst.vnum) {
                if !proto.long_descr.is_empty() {
                    s.push_str(proto.long_descr.trim_end());
                    s.push_str("\r\n");
                }
            }
        }
    }
    s
}
