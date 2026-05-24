/// Per-connection state and I/O handler.
/// Mirrors descriptor_data (structs.h) and the per-descriptor logic in comm.c:
///   new_descriptor(), process_input(), process_output(), close_socket().

use std::{net::SocketAddr, sync::Arc, time::Instant};

use anyhow::Result;
use bytes::{BufMut, BytesMut};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tracing::{debug, info, warn};

use crate::telnet;

/// Connection state — mirrors the CON_* defines in structs.h.
/// Only the states relevant to this checkpoint are included; others will be
/// added as login/game logic is ported.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    /// Awaiting player name (CON_GET_NAME)
    GetName,
    /// Fully logged in and in the game (CON_PLAYING)
    Playing,
    /// Marked for closure (CON_CLOSE)
    Close,
}

/// Lightweight per-connection metadata.
/// The full descriptor_data struct (structs.h:1074) will grow as more of the
/// game is ported. For checkpoint 1 we track only what we need for I/O.
pub struct Descriptor {
    /// Sequential connection ID (desc_num in descriptor_data)
    pub id: usize,
    /// Client hostname or IP string (host[] in descriptor_data)
    pub host: String,
    /// Current connection state (connected field)
    pub state: ConnState,
    /// Time the connection was opened (login_time)
    pub login_time: Instant,
}

impl Descriptor {
    pub fn new(id: usize, peer: SocketAddr) -> Self {
        Self {
            id,
            host: peer.ip().to_string(),
            state: ConnState::GetName,
            login_time: Instant::now(),
        }
    }
}

/// Handle a single accepted TCP connection end-to-end.
/// Mirrors the flow of new_descriptor() → process_input() → process_output() in comm.c.
///
/// Greeting protocol:
///   1. Send IAC WILL SUPPRESS-GA + IAC DO NAWS (minimal telnet negotiation)
///   2. Send the shared greeting text (lib/text/greetings)
///   3. Read lines from the client, strip telnet sequences, echo them back
///      (placeholder until the login/command interpreter is ported)
///   4. Close on EOF or I/O error
pub async fn handle_connection(
    id: usize,
    mut stream: TcpStream,
    peer: SocketAddr,
    greeting: Arc<String>,
) -> Result<()> {
    let desc = Descriptor::new(id, peer);
    info!(
        id = desc.id,
        host = %desc.host,
        "Connection accepted"
    );

    // --- Telnet negotiation handshake ----------------------------------------
    // Mirrors init_descriptor() + the protocol negotiation block in new_descriptor().
    // We send SUPPRESS-GA (so the client doesn't need a Go-Ahead to display output)
    // and request NAWS so we can learn the terminal dimensions later.
    let mut init_buf = Vec::with_capacity(9);
    init_buf.extend_from_slice(&telnet::cmd_suppress_ga());
    init_buf.extend_from_slice(&telnet::cmd_do_naws());
    init_buf.extend_from_slice(&telnet::cmd_do_ttype());
    stream.write_all(&init_buf).await?;

    // --- Send greeting -------------------------------------------------------
    // Mirrors the GREETINGS send in new_descriptor() (comm.c:1542).
    // The C version uses write_to_output() which queues to the output buffer;
    // here we write directly since we have no shared output queue yet.
    // Convert \n to \r\n for telnet line endings.
    let greeting_crlf = greeting.replace('\n', "\r\n");
    stream.write_all(greeting_crlf.as_bytes()).await?;

    // --- I/O loop ------------------------------------------------------------
    // Mirrors the process_input() / process_output() cycle in game_loop().
    // For this checkpoint we echo stripped input back to the client.
    let mut raw_buf = BytesMut::with_capacity(4096);
    let mut read_tmp = [0u8; 1024];

    loop {
        // Read available bytes (may contain telnet IAC sequences)
        let n = match stream.read(&mut read_tmp).await {
            Ok(0) => {
                info!(id, host = %desc.host, "EOF — client disconnected");
                break;
            }
            Ok(n) => n,
            Err(e) => {
                warn!(id, host = %desc.host, error = %e, "Read error");
                break;
            }
        };

        raw_buf.put_slice(&read_tmp[..n]);
        debug!(id, bytes = n, "Received {} raw bytes", n);

        // Strip IAC sequences (mirrors process_input() IAC handling)
        let clean = telnet::strip_telnet(&raw_buf);
        raw_buf.clear();

        // Collect complete lines (CR/LF delimited)
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut line_start = 0;
        for (i, &b) in clean.iter().enumerate() {
            if b == b'\n' || b == b'\r' {
                let line = clean[line_start..i].to_vec();
                if !line.is_empty() {
                    lines.push(line);
                }
                line_start = i + 1;
            }
        }
        // Any partial line not yet terminated: put back into raw_buf for next read
        if line_start < clean.len() {
            raw_buf.put_slice(&clean[line_start..]);
        }

        // Echo each complete line back (placeholder for command_interpreter)
        for line in lines {
            if let Ok(text) = std::str::from_utf8(&line) {
                debug!(id, cmd = text, "Input line");
                // TODO: route to nanny() or command_interpreter() once ported
                let echo = format!("Echo: {}\r\n", text);
                if let Err(e) = stream.write_all(echo.as_bytes()).await {
                    warn!(id, error = %e, "Write error");
                    return Ok(());
                }
            }
        }
    }

    info!(id, "Connection closed");
    Ok(())
}
