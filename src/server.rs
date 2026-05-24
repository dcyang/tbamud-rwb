/// TCP server: bind, listen, and dispatch connections.
/// Mirrors init_game() + the accept portion of game_loop() in comm.c.

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tracing::info;

use crate::{config::Config, descriptor};

/// Read greeting text from the data directory and start the accept loop.
///
/// Mirrors (in order):
///   init_game()     — setup before game_loop()
///   game_loop()     — the accept-new-connection branch
///   new_descriptor() — delegated to descriptor::handle_connection()
pub async fn run(config: Config) -> Result<()> {
    // Load the greeting shown to every new connection.
    // In the C codebase, GREETINGS is loaded by boot_db() from GREETINGS_FILE
    // ("text/greetings" relative to lib/).  We read it here before we chdir so
    // we can use the full relative path; later, when db.rs handles boot_db(),
    // this will move there.
    let greetings_path = format!("{}/text/greetings", config.dir);
    let greeting = Arc::new(
        std::fs::read_to_string(&greetings_path)
            .with_context(|| format!("Failed to read greeting from {greetings_path}"))?,
    );

    // Bind the listening socket.
    // Mirrors init_socket() in comm.c: SO_REUSEADDR is set by TcpListener automatically.
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind to port {}", config.port))?;

    info!(port = config.port, "Listening for connections");

    // Accept loop — mirrors the FD_ISSET(mother_desc) branch of game_loop().
    // Each connection is handled in its own Tokio task; there is no shared
    // descriptor linked-list yet (that will arrive with the game state port).
    let mut next_id: usize = 1;
    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .context("Failed to accept connection")?;

        let id = next_id;
        next_id += 1;

        let greeting = Arc::clone(&greeting);

        info!(id, %peer, "Accepted new connection");

        // Spawn an independent task per connection (replaces the descriptor-list
        // iteration in game_loop(); Tokio's scheduler handles concurrency).
        tokio::spawn(async move {
            if let Err(e) = descriptor::handle_connection(id, stream, peer, greeting).await {
                tracing::warn!(id, error = %e, "Connection task error");
            }
        });
    }
}
