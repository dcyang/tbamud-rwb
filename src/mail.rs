//! File-backed player mail.
//!
//! Each recipient has a single file at
//!   `<data_dir>/plrmail/<name>.mail`
//! containing one message per line:
//!   `<from>\t<unix_ts>\t<body>`
//!
//! Newlines in the body are escaped to the literal two-character
//! sequence `\n` on save and unescaped on load.  A literal `\` becomes
//! `\\` — this avoids needing a more complex framing format while still
//! preserving multi-line bodies if we ever add a composer for them.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct MailMessage {
    pub from:     String,
    pub unix_ts:  i64,
    pub body:     String,
}

fn mailbox_path(data_dir: &str, name: &str) -> PathBuf {
    PathBuf::from(format!("{}/plrmail/{}.mail", data_dir, name.to_ascii_lowercase()))
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.peek() {
                Some('n')  => { it.next(); out.push('\n'); }
                Some('\\') => { it.next(); out.push('\\'); }
                _          => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Load all messages for `recipient`.  Missing file returns an empty
/// vec — that's the normal "no mail" state, not an error.
pub fn load_mailbox(data_dir: &str, recipient: &str) -> Vec<MailMessage> {
    let path = mailbox_path(data_dir, recipient);
    let Ok(contents) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in contents.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(from), Some(ts), Some(body)) = (parts.next(), parts.next(), parts.next()) else { continue; };
        let unix_ts: i64 = ts.parse().unwrap_or(0);
        out.push(MailMessage {
            from:    from.to_string(),
            unix_ts,
            body:    unescape(body),
        });
    }
    out
}

/// Overwrite the mailbox with the given messages.
pub fn save_mailbox(
    data_dir: &str,
    recipient: &str,
    msgs: &[MailMessage],
) -> std::io::Result<()> {
    let dir = format!("{}/plrmail", data_dir);
    fs::create_dir_all(&dir)?;
    let path = mailbox_path(data_dir, recipient);
    let mut f = fs::File::create(&path)?;
    for m in msgs {
        writeln!(f, "{}\t{}\t{}", m.from, m.unix_ts, escape(&m.body))?;
    }
    Ok(())
}

/// Append a single message to the recipient's mailbox.
pub fn append_mail(
    data_dir: &str,
    recipient: &str,
    msg: &MailMessage,
) -> std::io::Result<()> {
    let dir = format!("{}/plrmail", data_dir);
    fs::create_dir_all(&dir)?;
    let path = mailbox_path(data_dir, recipient);
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}\t{}\t{}", msg.from, msg.unix_ts, escape(&msg.body))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_roundtrip() {
        let inputs = ["plain", "with\nnewlines", "tab\there", "back\\slash", ""];
        for s in inputs {
            assert_eq!(unescape(&escape(s)), s);
        }
    }
}
