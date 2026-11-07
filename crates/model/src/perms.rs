//! Discord permission bit <-> flag-name mapping.
//! Source: <https://docs.discord.com/developers/topics/permissions>

pub const ADMINISTRATOR: u32 = 3;
pub const VIEW_CHANNEL: u32 = 10;

/// Permissions that let a member moderate, reconfigure or impersonate: every
/// `MANAGE_*`, Administrator, kick, ban, timeout, mute, deafen and move
/// members, the audit log, insights, mentioning everyone, creating events and
/// emojis or stickers everyone sees, and pinning others' messages.
pub const DANGEROUS: u64 = (1 << 1) // KICK_MEMBERS
    | (1 << 2) // BAN_MEMBERS
    | (1 << 3) // ADMINISTRATOR
    | (1 << 4) // MANAGE_CHANNELS
    | (1 << 5) // MANAGE_GUILD
    | (1 << 7) // VIEW_AUDIT_LOG
    | (1 << 8) // PRIORITY_SPEAKER
    | (1 << 13) // MANAGE_MESSAGES
    | (1 << 17) // MENTION_EVERYONE
    | (1 << 19) // VIEW_GUILD_INSIGHTS
    | (1 << 22) // MUTE_MEMBERS
    | (1 << 23) // DEAFEN_MEMBERS
    | (1 << 24) // MOVE_MEMBERS
    | (1 << 27) // MANAGE_NICKNAMES
    | (1 << 28) // MANAGE_ROLES
    | (1 << 29) // MANAGE_WEBHOOKS
    | (1 << 30) // MANAGE_GUILD_EXPRESSIONS
    | (1 << 33) // MANAGE_EVENTS
    | (1 << 34) // MANAGE_THREADS
    | (1 << 40) // MODERATE_MEMBERS
    | (1 << 41) // VIEW_CREATOR_MONETIZATION_ANALYTICS
    | (1 << 43) // CREATE_GUILD_EXPRESSIONS
    | (1 << 44) // CREATE_EVENTS
    | (1 << 51) // PIN_MESSAGES
    | (1 << 52); // BYPASS_SLOWMODE

/// Whether a permission bitfield includes anything in [`DANGEROUS`], however
/// it's spelt (`BIT_3` is Administrator too).
pub fn dangerous(bits: u64) -> bool {
    bits & DANGEROUS != 0
}

const NAMES: &[(u32, &str)] = &[
    (0, "CREATE_INSTANT_INVITE"),
    (1, "KICK_MEMBERS"),
    (2, "BAN_MEMBERS"),
    (3, "ADMINISTRATOR"),
    (4, "MANAGE_CHANNELS"),
    (5, "MANAGE_GUILD"),
    (6, "ADD_REACTIONS"),
    (7, "VIEW_AUDIT_LOG"),
    (8, "PRIORITY_SPEAKER"),
    (9, "STREAM"),
    (10, "VIEW_CHANNEL"),
    (11, "SEND_MESSAGES"),
    (12, "SEND_TTS_MESSAGES"),
    (13, "MANAGE_MESSAGES"),
    (14, "EMBED_LINKS"),
    (15, "ATTACH_FILES"),
    (16, "READ_MESSAGE_HISTORY"),
    (17, "MENTION_EVERYONE"),
    (18, "USE_EXTERNAL_EMOJIS"),
    (19, "VIEW_GUILD_INSIGHTS"),
    (20, "CONNECT"),
    (21, "SPEAK"),
    (22, "MUTE_MEMBERS"),
    (23, "DEAFEN_MEMBERS"),
    (24, "MOVE_MEMBERS"),
    (25, "USE_VAD"),
    (26, "CHANGE_NICKNAME"),
    (27, "MANAGE_NICKNAMES"),
    (28, "MANAGE_ROLES"),
    (29, "MANAGE_WEBHOOKS"),
    (30, "MANAGE_GUILD_EXPRESSIONS"),
    (31, "USE_APPLICATION_COMMANDS"),
    (32, "REQUEST_TO_SPEAK"),
    (33, "MANAGE_EVENTS"),
    (34, "MANAGE_THREADS"),
    (35, "CREATE_PUBLIC_THREADS"),
    (36, "CREATE_PRIVATE_THREADS"),
    (37, "USE_EXTERNAL_STICKERS"),
    (38, "SEND_MESSAGES_IN_THREADS"),
    (39, "USE_EMBEDDED_ACTIVITIES"),
    (40, "MODERATE_MEMBERS"),
    (41, "VIEW_CREATOR_MONETIZATION_ANALYTICS"),
    (42, "USE_SOUNDBOARD"),
    (43, "CREATE_GUILD_EXPRESSIONS"),
    (44, "CREATE_EVENTS"),
    (45, "USE_EXTERNAL_SOUNDS"),
    (46, "SEND_VOICE_MESSAGES"),
    (48, "SET_VOICE_CHANNEL_STATUS"),
    (49, "SEND_POLLS"),
    (50, "USE_EXTERNAL_APPS"),
    (51, "PIN_MESSAGES"),
    (52, "BYPASS_SLOWMODE"),
];

/// Every known permission flag name, in bit order.
pub fn all() -> Vec<&'static str> {
    NAMES.iter().map(|(_, n)| *n).collect()
}

/// Decode a permission bitfield into flag names (sorted by bit). Unknown bits
/// become `BIT_n` so nothing is silently dropped on export.
pub fn names(bits: u64) -> Vec<String> {
    (0..64)
        .filter(|b| bits & (1u64 << b) != 0)
        .map(|b| match NAMES.iter().find(|(n, _)| *n == b) {
            Some((_, name)) => (*name).to_owned(),
            None => format!("BIT_{b}"),
        })
        .collect()
}

/// [`bits`] where there's no error to return (plans, power checks). One rule
/// everywhere: an unknown name counts as every permission, so it fails
/// closed: too powerful to hand out, and a change the plan shows but apply
/// refuses (it uses [`bits`]). `check` reports the name as an error, and no
/// config with errors is applied or used by the bot.
pub fn bits_or_all(names: &[String]) -> u64 {
    bits(names).unwrap_or(u64::MAX)
}

/// A permission name [`bits`] doesn't know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unknown(pub String);

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown permission {:?}", self.0)
    }
}

impl std::error::Error for Unknown {}

/// Encode flag names back into a bitfield (for `apply`). Errors on an unknown name.
pub fn bits(names: &[String]) -> Result<u64, Unknown> {
    let mut acc = 0u64;
    for n in names {
        let bit = match n.strip_prefix("BIT_") {
            Some(rest) => rest.parse().ok().filter(|b| *b < 64),
            None => NAMES.iter().find(|(_, name)| *name == n).map(|(bit, _)| *bit),
        };
        acc |= 1u64 << bit.ok_or_else(|| Unknown(n.clone()))?;
    }
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trips() {
        let b = (1 << 10) | (1 << 28) | (1 << 3);
        let n = names(b);
        assert!(n.contains(&"VIEW_CHANNEL".to_string()));
        assert!(n.contains(&"ADMINISTRATOR".to_string()));
        assert_eq!(bits(&n).unwrap(), b);
    }
    #[test]
    fn unknown_bit_preserved() {
        assert_eq!(bits(&names(1 << 47)).unwrap(), 1 << 47);
        assert!(bits(&["BIT_64".into()]).is_err() && bits(&["BIT_x".into()]).is_err());
    }

    #[test]
    fn dangerous_by_bit_not_by_name() {
        let named = |n: &[&str]| bits(&n.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        assert!(dangerous(named(&["BIT_3"])), "Administrator spelt as a bit");
        for n in ["MUTE_MEMBERS", "MOVE_MEMBERS", "VIEW_AUDIT_LOG", "MANAGE_ROLES", "MODERATE_MEMBERS"] {
            assert!(dangerous(named(&[n])), "{n}");
        }
        assert!(!dangerous(named(&["VIEW_CHANNEL", "SEND_MESSAGES", "CONNECT", "SPEAK"])));
    }
}
