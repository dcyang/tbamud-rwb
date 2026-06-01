/// Login state machine: the Rust equivalent of nanny() in interpreter.c.
///
/// Handles all pre-game connection states from name entry through the main menu.
/// Each call to `LoginSession::process()` consumes one line of client input and
/// returns a `LoginOutput` describing what to write back and how to change state.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::{
    descriptor::ConnState,
    players::{
        capitalize, crypt_password, validate_name, verify_password,
        Class, PlayerDb, PlayerRecord, Sex, MAX_BAD_PWS, MAX_PWD_LENGTH,
    },
};

// ---------------------------------------------------------------------------
// Game text strings (loaded at startup, shared read-only)
// ---------------------------------------------------------------------------

/// All static text displayed during login.  Loaded from lib/text/* and
/// hardcoded defaults.  Mirrors the global text pointers in db.c.
#[derive(Clone)]
pub struct GameTexts {
    pub motd:     String, // lib/text/motd
    pub imotd:    String, // lib/text/imotd
    pub menu:     String, // CONFIG_MENU from config.c (hardcoded below)
    pub welc:     String, // CONFIG_WELC_MESSG
    pub start:    String, // CONFIG_START_MESSG
}

impl GameTexts {
    pub fn load(data_dir: &str) -> Self {
        let motd  = load_text(data_dir, "text/motd");
        let imotd = load_text(data_dir, "text/imotd");

        // These match config.c verbatim but with \t color codes stripped for
        // plain telnet; color support will be added when screen.h is ported.
        let menu = concat!(
            "\r\n",
            "Welcome to tbaMUD!\r\n",
            "0) Exit from tbaMUD.\r\n",
            "1) Enter the game.\r\n",
            "2) Enter description.\r\n",
            "3) Read the background story.\r\n",
            "4) Change password.\r\n",
            "5) Delete this character.\r\n",
            "\r\n",
            "   Make your choice: ",
        ).to_string();

        let welc  = "\r\nWelcome to tbaMUD!  May your visit here be... Enlightening\r\n\r\n".to_string();
        let start = concat!(
            "Welcome.  This is your new tbaMUD character!  You can now earn gold,\r\n",
            "gain experience, find weapons and equipment, and much more -- while\r\n",
            "meeting people from around the world!\r\n",
        ).to_string();

        Self { motd, imotd, menu, welc, start }
    }
}

fn load_text(data_dir: &str, name: &str) -> String {
    let path = format!("{}/{}", data_dir, name);
    std::fs::read_to_string(&path).unwrap_or_else(|_| {
        tracing::warn!("Could not read {path}");
        String::new()
    })
}

// ---------------------------------------------------------------------------
// Class selection menu
// ---------------------------------------------------------------------------

const CLASS_MENU: &str = concat!(
    "\r\n",
    "Select a class:\r\n",
    "  [C]leric\r\n",
    "  [T]hief\r\n",
    "  [W]arrior\r\n",
    "  [M]agic-user\r\n",
);

// ---------------------------------------------------------------------------
// Login session state
// ---------------------------------------------------------------------------

/// Per-connection login session.  Lives inside `handle_connection` and is NOT
/// shared between tasks.  Mirrors the per-descriptor login fields of
/// `descriptor_data` in structs.h (bad_pws, character, state, etc.).
#[derive(Debug, Default)]
pub struct LoginSession {
    /// Current connection state (mirrors `d->connected`)
    pub state: ConnState,

    /// Name typed by the user, tentatively validated (awaiting Y/N confirmation)
    pub player_name: Option<String>,

    /// Password hash stored during new-character password entry, used in
    /// CON_CNFPASSWD to verify the second typing.
    pub pending_hash: Option<String>,

    /// The loaded or newly-created player record (available once the player
    /// is found or after name confirmation)
    pub player: Option<PlayerRecord>,

    /// Failed password attempts this session (mirrors `d->bad_pws`)
    pub bad_pws: u8,

    /// Level of the authenticated player (cached for motd selection)
    pub level: i32,
}

// ---------------------------------------------------------------------------
// Return value from process()
// ---------------------------------------------------------------------------

/// What `process()` wants the I/O layer to do after this input.
#[derive(Debug, Default)]
pub struct LoginOutput {
    /// Text to write to the socket (already CRLF-safe)
    pub text: String,
    /// Send IAC WONT ECHO (re-enable client echo, e.g. after password prompt)
    pub echo_on: bool,
    /// Send IAC WILL ECHO (suppress client echo, e.g. before password prompt)
    pub echo_off: bool,
    /// Close the connection
    pub disconnect: bool,
    /// Player has passed the menu and selected "Enter the game"
    pub entered_game: bool,
}

impl LoginOutput {
    fn send(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Default::default() }
    }
    fn send_echo_off(text: impl Into<String>) -> Self {
        Self { text: text.into(), echo_off: true, ..Default::default() }
    }
    fn send_echo_on(text: impl Into<String>) -> Self {
        Self { text: text.into(), echo_on: true, ..Default::default() }
    }
    fn disconnect(text: impl Into<String>) -> Self {
        Self { text: text.into(), disconnect: true, ..Default::default() }
    }
    fn enter_game(text: impl Into<String>) -> Self {
        Self { text: text.into(), entered_game: true, ..Default::default() }
    }
}

// ---------------------------------------------------------------------------
// Main state machine
// ---------------------------------------------------------------------------

impl LoginSession {
    pub fn new() -> Self {
        Self { state: ConnState::GetName, ..Default::default() }
    }

    /// Process one line of input from the client.
    /// Mirrors `nanny()` in interpreter.c.
    pub async fn process(
        &mut self,
        raw_input: &str,
        players_arc: &Arc<Mutex<PlayerDb>>,
        xnames: &[String],
        texts: &GameTexts,
    ) -> LoginOutput {
        // skip_spaces equivalent: strip leading/trailing whitespace
        let input = raw_input.trim();

        match self.state {
            // ---------------------------------------------------------------
            // CON_GET_NAME — waiting for player to type their name
            // ---------------------------------------------------------------
            ConnState::GetName => {
                if input.is_empty() {
                    return LoginOutput::disconnect("");
                }

                let tmp_name = input.to_string();

                // Name must pass validation rules
                if let Some(err) = validate_name(&tmp_name, xnames) {
                    return LoginOutput::send(err);
                }

                // Look up in player index
                let mut players = players_arc.lock().await;
                let found_idx = players.find_by_name(&tmp_name);

                if let Some(idx) = found_idx {
                    // Existing player — load their record
                    match players.load_player(&tmp_name) {
                        Err(e) => {
                            tracing::error!("Failed to load player {tmp_name}: {e}");
                            return LoginOutput::disconnect(
                                "Error loading character. Please try again later.\r\n"
                            );
                        }
                        Ok(rec) => {
                            if rec.is_deleted() {
                                // Treat deleted player like new (name is free)
                                drop(players);
                                self.player_name = Some(capitalize(&tmp_name));
                                self.player = None;
                                self.state  = ConnState::NameConfirm;
                                return LoginOutput::send(format!(
                                    "Did I get that right, {} (Y/N)? ",
                                    capitalize(&tmp_name)
                                ));
                            }

                            tracing::info!(name = %rec.name, "Existing player found");
                            let _ = idx; // suppress warning
                            self.player = Some(rec);
                            self.state  = ConnState::Password;
                            return LoginOutput::send_echo_off("Password: ");
                        }
                    }
                } else {
                    // New player
                    drop(players);
                    self.player_name = Some(capitalize(&tmp_name));
                    self.state = ConnState::NameConfirm;
                    return LoginOutput::send(format!(
                        "Did I get that right, {} (Y/N)? ",
                        capitalize(&tmp_name)
                    ));
                }
            }

            // ---------------------------------------------------------------
            // CON_NAME_CNFRM — new name confirmed?
            // ---------------------------------------------------------------
            ConnState::NameConfirm => {
                match input.chars().next().map(|c| c.to_ascii_uppercase()) {
                    Some('Y') => {
                        // The -r "restrict" boot flag disables new-character
                        // creation entirely.
                        if crate::interpreter::RESTRICT
                            .load(std::sync::atomic::Ordering::Relaxed)
                        {
                            return LoginOutput {
                                text:       "\r\nSorry, new characters are not allowed right now.\r\n".to_string(),
                                echo_on:    false,
                                disconnect: true,
                                ..Default::default()
                            };
                        }
                        // Wizlock blocks new-character creation entirely
                        // (a brand-new mortal is level 1).
                        let wl = crate::interpreter::WIZLOCK_LEVEL
                            .load(std::sync::atomic::Ordering::Relaxed);
                        if wl > 0 {
                            return LoginOutput {
                                text:       format!(
                                    "\r\nNew character creation is disabled (wizlock {wl}).\r\n",
                                ),
                                echo_on:    false,
                                disconnect: true,
                                ..Default::default()
                            };
                        }
                        let name = self.player_name.clone().unwrap_or_default();
                        self.state = ConnState::NewPassword;
                        return LoginOutput::send_echo_off(format!(
                            "New character.\r\nGive me a password for {name}: "
                        ));
                    }
                    Some('N') => {
                        self.player_name = None;
                        self.state = ConnState::GetName;
                        return LoginOutput::send("Okay, what IS it, then? ");
                    }
                    _ => {
                        return LoginOutput::send("Please type Yes or No: ");
                    }
                }
            }

            // ---------------------------------------------------------------
            // CON_PASSWORD — existing player entering their password
            // ---------------------------------------------------------------
            ConnState::Password => {
                // echo_on first (mirrors the C code: echo_on(d) before checking)
                // The I/O layer will handle the IAC WONT ECHO; we tell it to
                // re-enable echo via LoginOutput::echo_on.

                if input.is_empty() {
                    return LoginOutput {
                        echo_on: true,
                        disconnect: true,
                        text: "\r\n".to_string(),
                        ..Default::default()
                    };
                }

                let rec = self.player.as_ref().expect("player must be set in Password state");
                let stored = rec.password_hash.clone();
                let name   = rec.name.clone();

                if !verify_password(input, &stored) {
                    self.bad_pws += 1;
                    tracing::info!(name = %name, attempt = self.bad_pws, "Bad password");

                    if self.bad_pws >= MAX_BAD_PWS {
                        return LoginOutput {
                            text:       "\r\nWrong password... disconnecting.\r\n".into(),
                            echo_on:    true,
                            disconnect: true,
                            ..Default::default()
                        };
                    }

                    return LoginOutput {
                        text:    "\r\nWrong password.\r\nPassword: ".into(),
                        echo_on: true,   // echo_on after the CRLF...
                        echo_off: true,  // ...then echo_off for the next prompt
                        ..Default::default()
                    };
                }

                // Correct password
                tracing::info!(name = %name, "Player authenticated");
                self.level = rec.level;

                // Wizlock gate: refuse if their level is below the
                // global threshold and they aren't already an immortal.
                let wl = crate::interpreter::WIZLOCK_LEVEL
                    .load(std::sync::atomic::Ordering::Relaxed);
                if wl > 0 && rec.level < wl.min(34) && rec.level < 34 {
                    tracing::info!(name = %name, wizlock = wl, "Wizlock kick");
                    return LoginOutput {
                        text:       format!(
                            "\r\nThe game is locked to mortals below level {wl}. Try again later.\r\n",
                        ),
                        echo_on:    true,
                        disconnect: true,
                        ..Default::default()
                    };
                }

                let motd_text = if self.level >= 34 /* LVL_IMMORT */ {
                    texts.imotd.clone()
                } else {
                    texts.motd.clone()
                };

                self.state = ConnState::ReadMotd;
                LoginOutput {
                    text:    format!("\r\n{}\r\n*** PRESS RETURN: ", motd_text),
                    echo_on: true,
                    ..Default::default()
                }
            }

            // ---------------------------------------------------------------
            // CON_NEWPASSWD — new character choosing password
            // ---------------------------------------------------------------
            ConnState::NewPassword => {
                let name = self.player_name.clone().unwrap_or_default();

                // Validate: 3–30 chars, not same as name
                if input.len() < 3
                    || input.len() > MAX_PWD_LENGTH
                    || input.to_lowercase() == name.to_lowercase()
                {
                    return LoginOutput {
                        text:    "\r\nIllegal password.\r\nPassword: ".into(),
                        echo_off: true,
                        ..Default::default()
                    };
                }

                // Hash using player name as salt (mirrors CRYPT(arg, GET_PC_NAME(d->character)))
                let hash = crypt_password(input, &name);
                self.pending_hash = Some(hash);
                self.state = ConnState::ConfirmPassword;
                LoginOutput::send_echo_off("\r\nPlease retype password: ")
            }

            // ---------------------------------------------------------------
            // CON_CNFPASSWD — confirm the new password
            // ---------------------------------------------------------------
            ConnState::ConfirmPassword => {
                let pending = self.pending_hash.clone().unwrap_or_default();

                // Verify by hashing input with the stored hash as salt
                if !verify_password(input, &pending) {
                    self.pending_hash = None;
                    self.state = ConnState::NewPassword;
                    return LoginOutput {
                        text:    "\r\nPasswords don't match... start over.\r\nPassword: ".into(),
                        echo_on: true,
                        echo_off: true,
                        ..Default::default()
                    };
                }

                // Passwords match — move on to sex selection for new characters
                let name = self.player_name.clone().unwrap_or_default();
                self.player = Some(PlayerRecord {
                    name:          name.clone(),
                    password_hash: pending,
                    ..Default::default()
                });
                self.state = ConnState::SelectSex;
                LoginOutput {
                    text:   "\r\nWhat is your sex (M/F)? ".into(),
                    echo_on: true,
                    ..Default::default()
                }
            }

            // ---------------------------------------------------------------
            // CON_QSEX — new character sex selection
            // ---------------------------------------------------------------
            ConnState::SelectSex => {
                match input.chars().next().map(|c| c.to_ascii_uppercase()) {
                    Some('M') => {
                        if let Some(p) = &mut self.player {
                            p.sex = Sex::Male;
                        }
                    }
                    Some('F') => {
                        if let Some(p) = &mut self.player {
                            p.sex = Sex::Female;
                        }
                    }
                    _ => {
                        return LoginOutput::send(
                            "That is not a sex..\r\nWhat IS your sex? "
                        );
                    }
                }
                self.state = ConnState::SelectClass;
                LoginOutput::send(format!("{CLASS_MENU}\r\nClass: "))
            }

            // ---------------------------------------------------------------
            // CON_QCLASS — new character class selection
            // ---------------------------------------------------------------
            ConnState::SelectClass => {
                let class = match input.chars().next().map(|c| c.to_ascii_lowercase()) {
                    Some('m') => Class::MagicUser,
                    Some('c') => Class::Cleric,
                    Some('t') => Class::Thief,
                    Some('w') => Class::Warrior,
                    _ => {
                        return LoginOutput::send("\r\nThat's not a class.\r\nClass: ");
                    }
                };

                if let Some(p) = &mut self.player {
                    p.class = class;
                }

                // Create the player in the DB and save
                let name = self.player_name.clone().unwrap_or_default();
                {
                    let mut players = players_arc.lock().await;
                    let id = players.create_entry(&name);
                    if let Some(p) = &mut self.player {
                        p.id    = id;
                        p.level = 0;
                        // First-ever character becomes implementor (mirrors tbaMUD)
                        if id == 1 {
                            p.level = 34; // LVL_IMPL
                        }
                    }
                    if let Some(p) = &self.player {
                        if let Err(e) = players.save_player(p) {
                            tracing::error!("Failed to save new player {name}: {e}");
                        }
                        players.update_entry(p);
                        if let Err(e) = players.save_index() {
                            tracing::error!("Failed to save player index: {e}");
                        }
                    }
                }

                self.level = self.player.as_ref().map(|p| p.level).unwrap_or(0);
                tracing::info!(name = %name, class = ?class, "New player created");

                self.state = ConnState::ReadMotd;
                LoginOutput::send(format!("{}\r\n*** PRESS RETURN: ", texts.motd))
            }

            // ---------------------------------------------------------------
            // CON_RMOTD — waiting for Return after motd
            // ---------------------------------------------------------------
            ConnState::ReadMotd => {
                self.state = ConnState::Menu;
                LoginOutput::send(&texts.menu)
            }

            // ---------------------------------------------------------------
            // CON_MENU — main menu
            // ---------------------------------------------------------------
            ConnState::Menu => {
                match input.chars().next() {
                    Some('0') => {
                        return LoginOutput::disconnect("Goodbye.\r\n");
                    }

                    Some('1') => {
                        // Enter the game
                        let welc = texts.welc.clone();
                        let start = if self.level == 0 {
                            texts.start.clone()
                        } else {
                            String::new()
                        };
                        self.state = ConnState::Playing;
                        return LoginOutput::enter_game(format!("{welc}{start}"));
                    }

                    Some('2') => {
                        // Description editor — not yet implemented
                        return LoginOutput::send(
                            "Description editing not yet implemented.\r\n"
                        ).then_menu(&texts.menu);
                    }

                    Some('3') => {
                        // Background
                        let bg = load_text(
                            self.player.as_ref()
                                .map(|_| "lib")
                                .unwrap_or("lib"),
                            "text/background"
                        );
                        self.state = ConnState::ReadMotd; // reuse ReadMotd to wait for Return
                        return LoginOutput::send(format!("{bg}\r\n*** PRESS RETURN: "));
                    }

                    Some('4') => {
                        // Change password — not yet implemented
                        return LoginOutput::send(
                            "Password change not yet implemented.\r\n"
                        ).then_menu(&texts.menu);
                    }

                    Some('5') => {
                        // Delete character — not yet implemented
                        return LoginOutput::send(
                            "Character deletion not yet implemented.\r\n"
                        ).then_menu(&texts.menu);
                    }

                    _ => {
                        return LoginOutput::send(
                            format!("\r\nThat's not a menu choice!\r\n{}", texts.menu)
                        );
                    }
                }
            }

            // ---------------------------------------------------------------
            // Playing / Close — handled by the I/O layer, not nanny
            // ---------------------------------------------------------------
            ConnState::Playing | ConnState::Close => {
                LoginOutput::default()
            }
        }
    }
}

// Small helper to append the menu after an informational message
impl LoginOutput {
    fn then_menu(mut self, menu: &str) -> Self {
        self.text.push_str(menu);
        self
    }
}
