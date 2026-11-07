//! Lists, permissions, questions and permission overwrites as plain text: as
//! typed after a CLI flag, or edited in a text editor from the CLI or TUI.
//! One layout for each, read the same way by every front end. Lines starting
//! with `#` are notes and left out.

use crate::bot::Question;
use crate::schema::Overwrite;

/// "a, b ,c" (or one per line) as ["a", "b", "c"]; empty items dropped.
pub fn list(s: &str) -> Vec<String> {
    s.split([',', '\n']).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).collect()
}

/// Lines that aren't blank or `#` notes.
fn content(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// Permission names, separated by commas, spaces or new lines, each as
/// Discord writes it (`MANAGE_MESSAGES`) or in words (`manage messages`,
/// `manage-messages`). Words make the longest name Discord knows; words that
/// make none are kept together as one name, for `check` to point out.
pub fn parse_permissions(text: &str) -> Vec<String> {
    let known = chipsoc_model::perms::all();
    let mut out = Vec::new();
    for item in content(text).flat_map(|l| l.split(',')) {
        let words: Vec<String> = item.split_whitespace().map(|w| w.to_uppercase().replace('-', "_")).collect();
        // The end of the longest known name starting at word `i`.
        let name_at = |i: usize| (i + 1..=words.len()).rev().find(|&j| known.contains(&words[i..j].join("_").as_str()));
        let mut i = 0;
        while i < words.len() {
            let end = name_at(i)
                .unwrap_or_else(|| (i + 1..words.len()).find(|&k| name_at(k).is_some()).unwrap_or(words.len()));
            out.push(words[i..end].join("_"));
            i = end;
        }
    }
    out
}

/// Permission names to edit, one per line, with every name listed as notes.
pub fn permissions_text(have: &[String], all: &[String]) -> String {
    let mut out = "# One permission per line. Delete a line to take it away.\n".to_owned();
    for p in have {
        out.push_str(p);
        out.push('\n');
    }
    out.push_str("#\n# All permissions:\n");
    for chunk in all.chunks(4) {
        out.push_str(&format!("#   {}\n", chunk.join(" ")));
    }
    out
}

const QUESTIONS_HELP: &str = "# One question per line. Add [long] for a big box, and [optional] if it may be\n\
# left blank. A hint shown in the box goes after \" // \". Discord shows 5 boxes\n\
# at most (the intro, if there is one, takes one).\n";

/// Questions to edit: `Why this position? [long] [optional] // a line or two`.
pub fn questions_text(qs: &[Question]) -> String {
    let mut out = QUESTIONS_HELP.to_owned();
    for q in qs {
        out.push_str(&q.ask);
        if q.long {
            out.push_str(" [long]");
        }
        if q.optional {
            out.push_str(" [optional]");
        }
        if let Some(h) = &q.hint {
            out.push_str(&format!(" // {h}"));
        }
        out.push('\n');
    }
    out
}

/// What [`questions_text`] writes. The older layout is read too: `* ` first
/// for a big box, `(optional)` last, and a hint after ` -- `.
pub fn parse_questions(text: &str) -> Result<Vec<Question>, String> {
    let qs: Vec<Question> = content(text)
        .map(|line| {
            let (rest, hint) = match line.split_once(" // ").or_else(|| line.split_once(" -- ")) {
                Some((r, h)) => (r, Some(h.trim().to_owned()).filter(|h| !h.is_empty())),
                None => (line, None),
            };
            let (rest, star) = match rest.strip_prefix("* ") {
                Some(r) => (r, true),
                None => (rest, false),
            };
            let (rest, paren) = match rest.trim_end().strip_suffix("(optional)") {
                Some(r) => (r, true),
                None => (rest, false),
            };
            let long = star || rest.contains("[long]");
            let optional = paren || rest.contains("[optional]");
            let ask = rest.replace("[long]", "").replace("[optional]", "").trim().to_owned();
            Question { ask, long, optional, hint }
        })
        .collect();
    if qs.is_empty() {
        return Err("Give at least one question.".into());
    }
    Ok(qs)
}

/// Overwrites to edit, one line per role: `<role> allow PERM … deny PERM …`.
pub fn overwrites_text(ows: &[Overwrite], roles: &[String]) -> String {
    let mut out = "# Who can see and do what here, one line per role:\n\
                   #   <role key> allow PERMISSION … deny PERMISSION …\n\
                   # e.g.  everyone deny VIEW_CHANNEL\n\
                   #       committee allow VIEW_CHANNEL SEND_MESSAGES\n"
        .to_owned();
    out.push_str(&format!("# Roles: {}\n", roles.join(", ")));
    for o in ows {
        let mut line = match (&o.role, o.member) {
            (Some(r), _) => r.clone(),
            (None, Some(m)) => format!("member:{m}"),
            (None, None) => continue,
        };
        if !o.allow.is_empty() {
            line.push_str(&format!(" allow {}", o.allow.join(" ")));
        }
        if !o.deny.is_empty() {
            line.push_str(&format!(" deny {}", o.deny.join(" ")));
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// What [`overwrites_text`] writes; the permissions as [`parse_permissions`] reads them.
pub fn parse_overwrites(text: &str) -> Result<Vec<Overwrite>, String> {
    let mut out = Vec::new();
    for line in content(text) {
        let mut words = line.split_whitespace();
        let who = words.next().unwrap_or_default();
        let mut o = Overwrite::default();
        match who.strip_prefix("member:") {
            Some(id) => o.member = Some(id.parse().map_err(|_| format!("`{who}`: a member is member:<their id>"))?),
            None => o.role = Some(who.trim_start_matches('@').to_owned()),
        }
        // The words after each `allow` or `deny`, read as permissions at the end.
        let (mut allow, mut deny) = (String::new(), String::new());
        let mut into: Option<&mut String> = None;
        for w in words {
            match w.trim_end_matches(',').to_lowercase().as_str() {
                "allow" => into = Some(&mut allow),
                "deny" => into = Some(&mut deny),
                _ => match &mut into {
                    Some(s) => {
                        s.push(' ');
                        s.push_str(w);
                    }
                    None => return Err(format!("`{line}`: say allow or deny before {w}")),
                },
            }
        }
        o.allow = parse_permissions(&allow);
        o.deny = parse_permissions(&deny);
        out.push(o);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_split_on_commas_and_lines() {
        assert_eq!(list("a, b\nc,,"), ["a", "b", "c"]);
        assert!(list(" , ").is_empty());
    }

    #[test]
    fn permissions_read_in_words_or_as_discord_writes_them() {
        let perms = parse_permissions("# x\nsend_messages\nVIEW_CHANNEL, kick members\nmanage-threads connect speak");
        assert_eq!(perms, ["SEND_MESSAGES", "VIEW_CHANNEL", "KICK_MEMBERS", "MANAGE_THREADS", "CONNECT", "SPEAK"]);
        assert_eq!(parse_permissions("send messages in threads"), ["SEND_MESSAGES_IN_THREADS"], "the longest name");
        assert_eq!(
            parse_permissions("manage mesages VIEW_CHANNEL"),
            ["MANAGE_MESAGES", "VIEW_CHANNEL"],
            "kept to check"
        );
        assert_eq!(parse_permissions("BIT_47 speak"), ["BIT_47", "SPEAK"]);
        let all = vec!["A".to_owned(), "B".to_owned()];
        assert_eq!(parse_permissions(&permissions_text(&perms, &all)), perms, "the list of all is a note");
    }

    #[test]
    fn questions_round_trip_through_text() {
        let qs = vec![
            Question::new("Course?"),
            Question { long: true, optional: true, hint: Some("a line or two".into()), ..Question::new("Why?") },
        ];
        assert_eq!(parse_questions(&questions_text(&qs)).unwrap(), qs);
        assert!(parse_questions("# nothing\n").is_err());
        assert_eq!(parse_questions("# note\n\n  Hours a week?  \n").unwrap(), [Question::new("Hours a week?")]);
        assert_eq!(parse_questions("Course?\n* Why? (optional) -- a line or two\n").unwrap(), qs, "the older layout");
    }

    #[test]
    fn overwrites_round_trip_through_text() {
        let ows = vec![
            Overwrite { role: Some("everyone".into()), deny: vec!["VIEW_CHANNEL".into()], ..Default::default() },
            Overwrite { member: Some(crate::Snowflake(7)), allow: vec!["SEND_MESSAGES".into()], ..Default::default() },
        ];
        let text = overwrites_text(&ows, &["everyone".into()]);
        assert_eq!(parse_overwrites(&text).unwrap(), ows);
        assert!(parse_overwrites("admin VIEW_CHANNEL").unwrap_err().contains("allow or deny"));
        let words = parse_overwrites("@mods allow view channel, manage messages deny send messages").unwrap();
        assert_eq!(
            (words[0].role.as_deref(), &words[0].allow[..], &words[0].deny[..]),
            (
                Some("mods"),
                &["VIEW_CHANNEL".to_owned(), "MANAGE_MESSAGES".to_owned()][..],
                &["SEND_MESSAGES".to_owned()][..]
            )
        );
    }
}
