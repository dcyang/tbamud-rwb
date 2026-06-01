/// Entry point for the tbaMUD Rust rewrite.
/// Mirrors main() in comm.c: parse CLI arguments, set up logging, and start the server.

mod boards;
mod character;
mod color;
mod combat;
mod config;
mod db;
mod descriptor;
mod interpreter;
mod login;
mod mail;
mod olc;
mod players;
mod server;
mod telnet;
mod world;

use anyhow::{bail, Result};
use config::Config;
use tracing_subscriber::{fmt, EnvFilter};

/// Parse command-line arguments into a Config.
/// Mirrors the two-pass argument loop in comm.c main() (lines 219–325).
/// Flags: -p <port>, -d <dir>, -o <logfile>, -r, -s, -m, -q, -c, -h
fn parse_args() -> Result<Config> {
    let mut cfg = Config::default();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                eprintln!(
                    "Usage: circle [-p port] [-d dir] [-o logfile] [-r] [-s] [-m] [-q] [-c]\n"
                );
                eprintln!("Options (mirror comm.c flags):");
                eprintln!("  -p <port>    Listen port             (default: {})", config::DFLT_PORT);
                eprintln!("  -d <dir>     Data directory          (default: {})", config::DFLT_DIR);
                eprintln!("  -o <file>    Log output file         (default: stderr)");
                eprintln!("  -r           Restrict -- no new players");
                eprintln!("  -s           Suppress special routines");
                eprintln!("  -m           Mini-MUD mode");
                eprintln!("  -q           Quick boot (skip rent check)");
                eprintln!("  -c           Syntax-check mode only");
                std::process::exit(0);
            }
            "-r" => cfg.restrict = true,
            "-s" => cfg.no_specials = true,
            "-m" => {
                cfg.mini_mud = true;
                cfg.quick_boot = true;
            }
            "-q" => cfg.quick_boot = true,
            "-c" => cfg.syntax_check = true,
            "-p" => {
                i += 1;
                cfg.port = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Expected port number after -p"))?
                    .parse::<u16>()
                    .map_err(|_| anyhow::anyhow!("Invalid port number"))?;
            }
            "-d" => {
                i += 1;
                cfg.dir = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Expected directory after -d"))?
                    .clone();
            }
            "-o" => {
                i += 1;
                cfg.logfile = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("Expected filename after -o"))?
                        .clone(),
                );
            }
            other => {
                // Allow bare port number as final positional arg (C original supports this)
                if let Ok(port) = other.parse::<u16>() {
                    cfg.port = port;
                } else {
                    bail!("Unknown argument: {other}");
                }
            }
        }
        i += 1;
    }
    Ok(cfg)
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = parse_args()?;

    // Initialise structured logging.
    // RUST_LOG env var overrides the default "info" level.
    // Mirrors comm.c's setup_log() / open_logfile().  The -o flag
    // (config.logfile) redirects output to a file instead of stderr.
    let env_filter = EnvFilter::from_default_env().add_directive("info".parse()?);
    match &config.logfile {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| anyhow::anyhow!("Failed to open log file {path}: {e}"))?;
            fmt()
                .with_env_filter(env_filter)
                .with_target(false)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        None => {
            fmt()
                .with_env_filter(env_filter)
                .with_target(false)
                .init();
        }
    }

    tracing::info!(
        "tbaMUD-rwb starting (port={}, dir={})",
        config.port,
        config.dir
    );

    if config.syntax_check {
        // -c mode: parse and load the world data, report the result, and
        // exit without entering the game loop (mirrors `scheck` in comm.c).
        tracing::info!("Syntax-check mode (-c): loading world data...");
        match db::load_world(&config.dir, config.mini_mud) {
            Ok(world) => {
                tracing::info!(
                    zones = world.zones.len(),
                    rooms = world.rooms.len(),
                    objs  = world.obj_protos.len(),
                    mobs  = world.mob_protos.len(),
                    "Syntax check passed -- world data loaded cleanly."
                );
                return Ok(());
            }
            Err(e) => {
                tracing::error!("Syntax check FAILED: {e:#}");
                std::process::exit(1);
            }
        }
    }

    server::run(config).await
}
