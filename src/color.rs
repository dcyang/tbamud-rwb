//! tbaMUD `@`-style color code → ANSI escape conversion.
//!
//! World data and online output use the tbaMUD convention: `@C` opens
//! bright cyan, `@n` resets, etc.  See lib/world/wld/0.wld and the
//! color.h file in the original C source for the full set.  We render
//! these to ANSI SGR sequences before writing them to the socket.
//!
//! `@@` is the escape — a literal `@` character.  Unknown `@X` codes
//! pass through unchanged (so accidental `@` sequences in player input
//! show up rather than vanish silently).

const RESET:  &str = "\x1b[0m";

/// Convert all `@X` color codes in `s` to ANSI escape sequences.  Returns
/// a new owned string; for the common no-color case we still allocate
/// once, but the cost is negligible vs. the socket write.
pub fn convert(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut iter = s.chars().peekable();
    while let Some(c) = iter.next() {
        if c != '@' { out.push(c); continue; }
        let Some(&next) = iter.peek() else { out.push('@'); break; };
        iter.next();
        if let Some(ansi) = ansi_for(next) {
            out.push_str(ansi);
        } else if next == '@' {
            out.push('@');                  // `@@` → literal '@'
        } else {
            out.push('@');                  // unknown — pass through.
            out.push(next);
        }
    }
    out
}

/// Strip every `@X` color code from `s` without rendering ANSI.
/// `@@` collapses to a single `@`; unknown codes drop entirely
/// (since the user opted out of color).
pub fn strip(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut iter = s.chars().peekable();
    while let Some(c) = iter.next() {
        if c != '@' { out.push(c); continue; }
        let Some(&next) = iter.peek() else { out.push('@'); break; };
        iter.next();
        if next == '@' { out.push('@'); }
        // Otherwise drop both the '@' and the code letter.
    }
    out
}

#[cfg(test)]
mod tests {
    use super::convert;

    #[test]
    fn passthrough_plain_text() {
        assert_eq!(convert("hello world"), "hello world");
    }

    #[test]
    fn maps_basic_codes_to_ansi() {
        let s = convert("@RBright red@n done.");
        assert!(s.starts_with("\x1b[1;31m"));
        assert!(s.contains("\x1b[0m"));
    }

    #[test]
    fn double_at_is_literal_at() {
        assert_eq!(convert("user@@host"), "user@host");
    }

    #[test]
    fn unknown_code_passes_through() {
        // '@!' is not a color code — should appear verbatim.
        assert_eq!(convert("foo@!bar"), "foo@!bar");
    }
}

fn ansi_for(c: char) -> Option<&'static str> {
    Some(match c {
        'n' => RESET,
        'd' | 'k' => "\x1b[0;30m",         // dark / black
        'r' => "\x1b[0;31m",
        'R' => "\x1b[1;31m",
        'g' => "\x1b[0;32m",
        'G' => "\x1b[1;32m",
        'y' => "\x1b[0;33m",
        'Y' => "\x1b[1;33m",
        'b' => "\x1b[0;34m",
        'B' => "\x1b[1;34m",
        'm' | 'p' => "\x1b[0;35m",
        'M' | 'P' => "\x1b[1;35m",
        'c' => "\x1b[0;36m",
        'C' => "\x1b[1;36m",
        'w' => "\x1b[0;37m",
        'W' => "\x1b[1;37m",
        'K' => "\x1b[1;30m",                // bright black (grey)
        _   => return None,
    })
}
