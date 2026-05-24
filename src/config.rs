/// Default listening port (mirrors DFLT_PORT in ../tbamud/src/config.c)
pub const DFLT_PORT: u16 = 4000;

/// Default data directory (mirrors DFLT_DIR in config.c)
pub const DFLT_DIR: &str = "lib";

/// Maximum simultaneous player connections (mirrors CONFIG_MAX_PLAYING)
pub const MAX_PLAYING: usize = 300;

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
