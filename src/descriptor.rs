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
                let me = Character {
                    id:           id as u32,
                    name:         pname.clone(),
                    level:        session.level,
                    sex:          session.player.as_ref()
                                    .map(|p| p.sex)
                                    .unwrap_or(crate::players::Sex::Neutral),
                    class:        session.player.as_ref()
                                    .map(|p| p.class)
                                    .unwrap_or(crate::players::Class::Undefined),
                    current_room: start,
                    inventory:    Vec::new(),
                    gold:         0,
                };

                run_game_session(id, host.clone(), stream, me, world, chars).await?;
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
    mut me: Character,
    world: Arc<Mutex<World>>,
    chars: SharedChars,
) -> Result<()> {
    // Split TCP for concurrent read/write
    let (mut read_half, mut write_half) = stream.into_split();

    // Outbound channel: other connections push messages via this, the writer
    // task drains it. Bound is unbounded for now; bursts during say/who are
    // tiny and we'd rather drop bytes on close than spin on send pressure.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // Register in shared player list so others can find us.
    {
        let mut cl = chars.lock().await;
        cl.add(PlayerHandle {
            id:           me.id,
            name:         me.name.clone(),
            level:        me.level,
            current_room: me.current_room,
            send:         tx.clone(),
        });
        // Broadcast arrival to the start room.
        cl.broadcast_room(
            me.current_room, Some(me.id),
            &format!("{} has entered the world.\r\n", me.name),
        );
    }

    // Send the welcome + initial room view via the channel so it goes through
    // the same writer task as everything else.
    let _ = tx.send("\r\nWelcome to tbaMUD!  May your visit here be... Enlightening\r\n".to_string());
    let _ = tx.send(interpreter::render_room(me.current_room, Some(me.id), &world, &chars).await);
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
                info!(id, host = %host, name = %me.name, "EOF in game loop");
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
            debug!(id, name = %me.name, cmd = %line, "command");
            let out = interpreter::dispatch_command(line, &mut me, &world, &chars).await;
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
    let from_room = me.current_room;
    {
        let mut cl = chars.lock().await;
        cl.remove(me.id);
        let verb = if quit { "leaves" } else { "vanishes into thin air" };
        cl.broadcast_room(
            from_room, None,
            &format!("{} {}.\r\n", me.name, verb),
        );
    }

    drop(tx);
    let _ = writer.await;

    info!(id, name = %me.name, "session ended");
    Ok(())
}
