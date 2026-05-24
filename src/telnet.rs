/// Telnet protocol constants and helpers.
/// Mirrors the inline telnet handling in comm.c (process_input, echo_off, echo_on)
/// and telnet.h constants.

// Core telnet command bytes
pub const IAC: u8 = 255; // Interpret As Command
pub const WILL: u8 = 251;
pub const WONT: u8 = 252;
pub const DO: u8 = 253;
pub const DONT: u8 = 254;
pub const SB: u8 = 250; // Subnegotiation Begin
pub const SE: u8 = 240; // Subnegotiation End
pub const GA: u8 = 249; // Go Ahead

// Telnet option codes
pub const OPT_ECHO: u8 = 1;
pub const OPT_SUPPRESS_GA: u8 = 3;
pub const OPT_NAWS: u8 = 31; // Negotiate About Window Size
pub const OPT_TTYPE: u8 = 24; // Terminal Type

/// IAC WILL ECHO — server will echo; tells client to stop echoing locally.
/// Used to suppress local echo during password entry (echo_off() in comm.c).
pub fn cmd_echo_off() -> [u8; 3] {
    [IAC, WILL, OPT_ECHO]
}

/// IAC WONT ECHO — server won't echo; tells client to resume local echo.
/// (echo_on() in comm.c)
pub fn cmd_echo_on() -> [u8; 3] {
    [IAC, WONT, OPT_ECHO]
}

/// IAC WILL SUPPRESS-GA — suppress Go-Ahead (standard for interactive MUDs)
pub fn cmd_suppress_ga() -> [u8; 3] {
    [IAC, WILL, OPT_SUPPRESS_GA]
}

/// IAC DO NAWS — ask client to send window dimensions
pub fn cmd_do_naws() -> [u8; 3] {
    [IAC, DO, OPT_NAWS]
}

/// IAC DO TTYPE — ask client to negotiate terminal type
pub fn cmd_do_ttype() -> [u8; 3] {
    [IAC, DO, OPT_TTYPE]
}

/// Strip IAC sequences and subnegotiations from raw socket input, returning
/// only the printable application data bytes. Mirrors the IAC-stripping
/// logic in process_input() in comm.c.
///
/// Handles:
///   IAC <cmd> <opt>   — 3-byte option commands (WILL/WONT/DO/DONT)
///   IAC SB … IAC SE  — subnegotiation blocks (NAWS, TTYPE, etc.)
///   IAC IAC           — escaped literal 0xFF
pub fn strip_telnet(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] != IAC {
            out.push(input[i]);
            i += 1;
            continue;
        }
        // IAC byte found
        i += 1;
        if i >= input.len() {
            break;
        }
        match input[i] {
            // Escaped IAC (literal 0xFF)
            255 => {
                out.push(IAC);
                i += 1;
            }
            // 3-byte option commands: WILL / WONT / DO / DONT
            WILL | WONT | DO | DONT => {
                i += 2; // skip command byte + option byte
            }
            // Subnegotiation: skip until IAC SE
            SB => {
                i += 1;
                while i + 1 < input.len() {
                    if input[i] == IAC && input[i + 1] == SE {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            // Other single-byte commands (GA, NOP, etc.)
            _ => {
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_plain_text() {
        let input = b"hello\r\n";
        assert_eq!(strip_telnet(input), b"hello\r\n");
    }

    #[test]
    fn strip_will_option() {
        // IAC WILL ECHO followed by text
        let input = &[IAC, WILL, OPT_ECHO, b'h', b'i'];
        assert_eq!(strip_telnet(input), b"hi");
    }

    #[test]
    fn strip_subnegotiation() {
        // IAC SB NAWS <w1> <w2> <h1> <h2> IAC SE followed by text
        let input = &[IAC, SB, OPT_NAWS, 0, 80, 0, 24, IAC, SE, b'o', b'k'];
        assert_eq!(strip_telnet(input), b"ok");
    }

    #[test]
    fn escaped_iac_passthrough() {
        let input = &[IAC, IAC, b'x'];
        assert_eq!(strip_telnet(input), &[255, b'x']);
    }
}
