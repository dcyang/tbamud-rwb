/// Entry point for the tbaMUD Rust rewrite.
/// Mirrors main() in comm.c: parse CLI arguments, set up logging, and start the server.

mod config;
mod db;
mod descriptor;
mod login;
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

    // Initialise structured logging to stderr.
    // RUST_LOG env var overrides the default "info" level.
    // Mirrors comm.c's setup_log() / open_logfile().
    // TODO: redirect to config.logfile when set.
    fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .with_target(false)
        .init();

    tracing::info!(
        "tbaMUD-rwb starting (port={}, dir={})",
        config.port,
        config.dir
    );

    if config.syntax_check {
        // -c mode: boot world data, report errors, exit.
        // Deferred until db.rs is ported.
        tracing::info!("Syntax-check mode requested (not yet implemented — exiting)");
        return Ok(());
    }

    server::run(config).await
}
