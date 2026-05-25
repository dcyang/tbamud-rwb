/// TCP server: bind, listen, load shared state, and dispatch connections.
/// Mirrors init_game() + the accept portion of game_loop() in comm.c.

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use tokio::{net::TcpListener, sync::Mutex};
use tracing::info;

use crate::{
    character::CharacterList,
    combat,
    config::Config,
    db,
    descriptor,
    login::GameTexts,
    players::{load_xnames, PlayerDb},
    world::World,
};

/// Load shared game state and start the accept loop.
pub async fn run(config: Config) -> Result<()> {
    // --- Load greeting -------------------------------------------------------
    let greetings_path = format!("{}/text/greetings", config.dir);
    let greeting = Arc::new(
        std::fs::read_to_string(&greetings_path)
            .with_context(|| format!("Failed to read greeting from {greetings_path}"))?,
    );

    // --- Load text files (motd, imotd, menu) ---------------------------------
    // Mirrors the file_to_string_alloc() calls in boot_db() in db.c.
    let texts = Arc::new(GameTexts::load(&config.dir));

    // --- Load player index ---------------------------------------------------
    // Mirrors build_player_index() called from boot_db().
    let players = Arc::new(Mutex::new(
        PlayerDb::load(&config.dir)
            .context("Failed to load player index")?,
    ));

    // --- Load optional xnames ban list ---------------------------------------
    let xnames = Arc::new(load_xnames(&config.dir));

    // --- Load world (zones + rooms + obj/mob protos + run resets) -----------
    let world: Arc<Mutex<World>> = Arc::new(Mutex::new(
        db::load_world(&config.dir).context("Failed to load world")?,
    ));

    // --- Shared online-player registry --------------------------------------
    let chars: Arc<Mutex<CharacterList>> =
        Arc::new(Mutex::new(CharacterList::default()));

    // --- Spawn background combat tick ---------------------------------------
    combat::spawn(Arc::clone(&world), Arc::clone(&chars));

    // --- Spawn periodic zone reset tick -------------------------------------
    db::spawn_zone_reset_tick(Arc::clone(&world));

    // --- Spawn corpse/decay tick -------------------------------------------
    db::spawn_decay_tick(Arc::clone(&world));

    // --- Spawn mob wander tick ---------------------------------------------
    db::spawn_wander_tick(Arc::clone(&world), Arc::clone(&chars));

    // --- Bind listening socket -----------------------------------------------
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind to port {}", config.port))?;

    info!(port = config.port, "Listening for connections");

    // --- Accept loop ---------------------------------------------------------
    let mut next_id: usize = 1;
    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .context("Failed to accept connection")?;

        let id = next_id;
        next_id += 1;

        info!(id, %peer, "Accepted new connection");

        let greeting = Arc::clone(&greeting);
        let players  = Arc::clone(&players);
        let texts    = Arc::clone(&texts);
        let xnames   = Arc::clone(&xnames);
        let world    = Arc::clone(&world);
        let chars    = Arc::clone(&chars);

        tokio::spawn(async move {
            if let Err(e) = descriptor::handle_connection(
                id, stream, peer, greeting, players, texts, xnames, world, chars,
            ).await {
                tracing::warn!(id, error = %e, "Connection task error");
            }
        });
    }
}
