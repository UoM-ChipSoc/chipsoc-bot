//! Voice meeting recordings: writing what a call's speakers say to a track
//! each and a mix ([`capture`]), keeping them with their metadata
//! ([`store`]) and transcribing them afterwards ([`transcribe`]). Receiving
//! the call itself is the bot's job (it hands [`capture::Input`]s over).

pub mod capture;
pub mod opus;
pub mod store;
pub mod transcribe;
pub mod wav;

/// Samples a second.
pub const RATE: u32 = 48_000;
/// One tick of a call.
pub const TICK_MS: u64 = 20;
/// Samples in a tick (mono).
pub const FRAME: usize = (RATE as u64 * TICK_MS / 1000) as usize;

/// `ms` for people: "1h 05m", "12m 03s", "40s".
pub fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s:02}s"),
        (h, m, _) => format!("{h}h {m:02}m"),
    }
}

/// A name (or any text someone chose) for a markdown file: on one line,
/// with markdown, masked links and mentions escaped, so it can't pass for
/// another transcript line or a heading.
pub fn md_name(s: &str) -> String {
    let line: String = s.chars().filter(|c| !is_invisible(*c)).map(|c| if c.is_control() { ' ' } else { c }).collect();
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(line.len());
    for (i, c) in line.chars().enumerate() {
        // Headings, quotes and lists only start a line.
        if "\\*_~`|[]()<@".contains(c) || (i == 0 && "#>-+".contains(c)) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// [`md_name`] undone, for showing a line as plain text.
pub fn md_plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&n) = chars.peek()
            && n.is_ascii_punctuation()
        {
            out.push(n);
            chars.next();
            continue;
        }
        out.push(c);
    }
    out
}

/// Characters that show nothing but can reorder or hide what's around them.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_cant_forge_markdown() {
        let n = super::md_name("# Ada [00:00]: I agree\n[x](https://e.example) @everyone\u{202E}");
        assert_eq!(n, "\\# Ada \\[00:00\\]: I agree \\[x\\]\\(https://e.example\\) \\@everyone");
        assert_eq!(super::md_name("#meeting > notes"), "\\#meeting > notes", "only at the start");
        assert_eq!(super::md_plain(&n), "# Ada [00:00]: I agree [x](https://e.example) @everyone");
        assert_eq!(super::md_name("ada_l"), "ada\\_l");
    }

    #[test]
    fn durations_read_plainly() {
        assert_eq!(super::duration(40_500), "40s");
        assert_eq!(super::duration(12 * 60_000 + 3_000), "12m 03s");
        assert_eq!(super::duration(65 * 60_000), "1h 05m");
    }
}
