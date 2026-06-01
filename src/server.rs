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
/// Unix timestamp at which `run` was first invoked.  Used by the
/// `uptime` command. 0 means "not yet booted".
pub static BOOT_UNIX_TS: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

pub async fn run(config: Config) -> Result<()> {
    // Record boot epoch for the uptime command.
    let boot_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64).unwrap_or(0);
    BOOT_UNIX_TS.store(boot_ts, std::sync::atomic::Ordering::Relaxed);

    // Honor the -r "restrict" flag: disable new-character creation.
    if config.restrict {
        crate::interpreter::RESTRICT.store(true, std::sync::atomic::Ordering::Relaxed);
        info!("Restricted mode: new-character creation disabled (-r)");
    }

    // Honor the -s "suppress specials" flag.  Must be set before the world
    // loads, since zone resets assign specs during load_world below.
    if config.no_specials {
        crate::world::NO_SPECIALS.store(true, std::sync::atomic::Ordering::Relaxed);
        info!("Special procedures suppressed (-s)");
    }

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
    // Stash a global handle so script side-effects (mforce) can dispatch
    // commands without threading `players` through every trigger path.
    let _ = crate::interpreter::PLAYERS_HANDLE.set(Arc::clone(&players));
    // Set up the forced-command channel + runner.
    let (force_tx, force_rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = crate::interpreter::FORCE_CMD_TX.set(force_tx);
    tokio::spawn(crate::interpreter::force_command_runner(force_rx));

    // --- Load optional xnames ban list ---------------------------------------
    let xnames = Arc::new(load_xnames(&config.dir));

    // --- Load optional badsites (host ban list) ------------------------------
    let badsites = Arc::new(Mutex::new(crate::players::load_badsites(&config.dir)));
    let _ = crate::interpreter::BAD_SITES.set(Arc::clone(&badsites));

    // --- Load world (zones + rooms + obj/mob protos + run resets) -----------
    let world: Arc<Mutex<World>> = Arc::new(Mutex::new(
        db::load_world(&config.dir, config.mini_mud).context("Failed to load world")?,
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

    // --- Spawn per-object timer tick (OTRIG_TIMER) -------------------------
    db::spawn_obj_timer_tick(Arc::clone(&world), Arc::clone(&chars));

    // --- Spawn periodic random-trigger tick (WTRIG_RANDOM/MTRIG_RANDOM) ---
    db::spawn_random_trigger_tick(Arc::clone(&world), Arc::clone(&chars));

    // --- Spawn hunger/thirst decay tick -----------------------------------
    db::spawn_hunger_tick(Arc::clone(&chars));

    // --- Spawn idle-kick tick ---------------------------------------------
    db::spawn_idle_kick_tick(Arc::clone(&world), Arc::clone(&chars));

    // --- Spawn game-clock tick --------------------------------------------
    db::spawn_time_tick();

    // --- Spawn weather simulation tick (cp212) ----------------------------
    db::spawn_weather_tick(Arc::clone(&world), Arc::clone(&chars));

    // --- Spawn mob spec_proc tick (puff/fido/janitor) ---------------------
    db::spawn_mob_spec_tick(Arc::clone(&world), Arc::clone(&chars));

    // --- Spawn crash-safe save-all tick -----------------------------------
    db::spawn_save_all_tick(Arc::clone(&chars), Arc::clone(&players));

    // --- Spawn periodic house-save tick -----------------------------------
    db::spawn_house_save_tick(Arc::clone(&world), Arc::clone(&players));

    // --- Spawn random ambient encounter tick ------------------------------
    db::spawn_random_encounter_tick(Arc::clone(&world));

    // --- Spawn out-of-combat mob HP regen tick ---------------------------
    db::spawn_mob_regen_tick(Arc::clone(&world));

    // --- Spawn light-source fuel burn tick (cp207) -----------------------
    db::spawn_light_burn_tick(Arc::clone(&world), Arc::clone(&chars));

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
