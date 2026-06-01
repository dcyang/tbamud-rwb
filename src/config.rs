/// Default listening port (mirrors DFLT_PORT in ../tbamud/src/config.c)
pub const DFLT_PORT: u16 = 4000;

/// Default data directory (mirrors DFLT_DIR in config.c)
pub const DFLT_DIR: &str = "lib";

/// Maximum simultaneous player connections (mirrors CONFIG_MAX_PLAYING)
pub const MAX_PLAYING: usize = 300;

// --- Rent system (mirrors the CONFIG_* rent knobs in config.c) ----------
// `free_rent` itself lives as a runtime atomic in `interpreter::FREE_RENT`
// (default true, matching stock's `free_rent = YES`) so the paid-rent path
// stays live code rather than being const-folded away.
/// Receptionist's flat surcharge on top of per-item rent (per day).
pub const MIN_RENT_COST: i32 = 100;
/// Maximum number of items a player may store when renting.
pub const MAX_OBJ_SAVE: i32 = 30;
/// Cost multiplier for cryo-renting vs. ordinary per-day rent.
pub const CRYO_FACTOR: i32 = 4;
/// Ordinary per-day rent multiplier.
pub const RENT_FACTOR: i32 = 1;
/// Real-days a "crash/quit" stored-object file is kept before the boot
/// cleanup deletes it (mirrors stock `crash_file_timeout`).
pub const CRASH_FILE_TIMEOUT: i64 = 10;
/// Real-days a "rented" stored-object file is kept (mirrors stock
/// `rent_file_timeout`).  A player whose saved record has rent_per_day > 0
/// is treated as rented and gets this longer grace period.
pub const RENT_FILE_TIMEOUT: i64 = 30;

/// Runtime configuration, constructed from CLI args.
/// Mirrors the global CONFIG_* variables set by load_config() + arg parsing in comm.c.
#[derive(Debug, Clone)]
pub struct Config {
    /// TCP port to listen on
    pub port: u16,
    /// Data directory path (lib/)
    pub dir: String,
    /// Path for log output (None = stderr)
    pub logfile: Option<String>,
    /// -r: Restrict game — no new players
    pub restrict: bool,
    /// -s: Suppress assignment of special routines
    pub no_specials: bool,
    /// -m: Mini-MUD mode (minimal boot + no rent check)
    pub mini_mud: bool,
    /// -q: Quick boot — skip rent object-limit scan
    pub quick_boot: bool,
    /// -c: Syntax-check mode only (boot world then exit)
    pub syntax_check: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DFLT_PORT,
            dir: DFLT_DIR.to_string(),
            logfile: None,
            restrict: false,
            no_specials: false,
            mini_mud: false,
            quick_boot: false,
            syntax_check: false,
        }
    }
}
