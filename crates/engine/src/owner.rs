//! What only the server owner may do through the bot, and who is asking.
//! Admins can do a lot; these are the things that can't be put back with
//! an apply, or that would let one admin lock the others (or the owner) out.

use std::fmt;

use anyhow::{Context, Result};
use chipsoc_config::recording::Recording;
use chipsoc_config::trust::Sensitive;
use chipsoc_discord::{Method, Rest};
use chipsoc_model::{Action, Field, ObjectKind, OrderScope, Plan, Snowflake};

/// What to say when Discord couldn't say who owns the server, so what only
/// the owner may do was refused.
pub const OWNER_UNKNOWN: &str =
    "Couldn't check who owns the server just now; nothing was changed. Try again in a moment.";

/// The server's owner, as Discord says now. The one place it's asked: `Err`
/// when Discord couldn't say, and whatever only the owner may do is then
/// refused (see [`OWNER_UNKNOWN`]).
pub async fn owner_of<R: Rest>(rest: &R, guild: Snowflake) -> Result<Snowflake> {
    let g = rest.call(Method::Get, &format!("guilds/{guild}"), None).await.context("asking Discord for the server")?;
    chipsoc_discord::wire::id(&g["owner_id"]).context("Discord didn't say who owns the server")
}

/// Who asks the engine for a change: named in commit messages and the audit
/// log, and checked against [`owner_only`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct By {
    /// For commit messages (`… by <name>`).
    pub name: Option<String>,
    /// Their Discord id, when Discord said who they are.
    pub id: Option<Snowflake>,
    /// The bot itself, or someone on the bot's own machine using its token
    /// (`--local`): the token could do all of this anyway.
    pub unlimited: bool,
}

impl By {
    /// The bot, on its own (drift watch, approvals already given).
    pub fn bot() -> Self {
        Self { unlimited: true, ..Self::default() }
    }

    /// Someone on the bot's machine, with its token.
    pub fn local(name: &str) -> Self {
        Self { name: Some(name.to_owned()), id: None, unlimited: true }
    }

    /// Someone Discord said is `id`.
    pub fn person(id: Snowflake, name: &str) -> Self {
        Self { name: Some(name.to_owned()), id: Some(id), unlimited: false }
    }

    /// Someone known only by name (or not at all), such as the web UI's shared code.
    pub fn named(name: Option<&str>) -> Self {
        Self { name: name.map(str::to_owned), id: None, unlimited: false }
    }

    /// Whether they may do what only `owner` may.
    pub fn may(&self, owner: Option<Snowflake>) -> bool {
        self.unlimited || (self.id.is_some() && self.id == owner)
    }

    /// For the audit log: their id, else their name; `None` for the bot.
    pub fn key(&self) -> Option<String> {
        self.id.map(|i| i.to_string()).or_else(|| self.name.clone())
    }
}

/// Something only the server owner can do through the bot. Shown to whoever
/// asked as "Only the server owner can …".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerOnly {
    /// Deleting for real (`--delete`) rather than archiving.
    Delete,
    /// Archiving channels the config never managed (`--sweep`).
    Sweep,
    /// Approving a change to `admin_role`, admin positions, the partner key or partners.
    Trust,
    /// Changing `[drift] mode`.
    DriftMode,
    /// Reverting or rewinding config changes.
    Undo,
    /// Taking permissions from the bot's own role, denying it some in a
    /// channel, or putting roles above it.
    WeakenBot,
    /// Taking a role from the server owner (or from anyone, while Discord
    /// can't say who the owner is).
    TakeFromOwner,
    /// Approving a change to how recordings are transcribed (`[recording]`
    /// `transcribe`, `model`, `whisper`): it decides what runs on the bot's machine.
    Transcription,
}

impl fmt::Display for OwnerOnly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Delete => {
                "Only the server owner can delete things for real (`--delete`). Without it, removed things are \
                 archived, which anyone can undo"
            }
            Self::Sweep => "Only the server owner can archive channels the config never managed (`--sweep`)",
            Self::Trust => {
                "Only the server owner can approve changes to who's an admin (`admin_role`, admin positions) or to \
                 partner sync's key and partners"
            }
            Self::DriftMode => "Only the server owner can change `[drift] mode`",
            Self::Undo => "Only the server owner can revert or rewind config changes",
            Self::WeakenBot => {
                "Only the server owner can take permissions away from the bot's own role (in a channel too) or put \
                 roles above it"
            }
            Self::TakeFromOwner => "Only the server owner can take a role from the server owner",
            Self::Transcription => {
                "Only the server owner can approve changes to how recordings are transcribed (`[recording]` \
                 `transcribe`, `model`, `whisper`), as they decide what runs on the bot's machine"
            }
        })
    }
}

impl std::error::Error for OwnerOnly {}

/// What someone asks for, as [`owner_only`] sees it.
#[derive(Debug, Clone, Copy)]
pub enum Ask<'a> {
    /// Applying a plan ([`Plan::owner`] and [`Plan::bot_role`] say whose server and which role is the bot's).
    Apply(&'a Plan),
    /// Approving the files' sensitive settings `new` over the approved `old`.
    Approve { old: &'a Sensitive, new: &'a Sensitive },
    /// Reverting or rewinding config commits.
    Undo,
    /// Taking roles away from `user` (members.toml, project leads).
    TakeRoles { user: Snowflake },
}

/// The one list of what only the server owner (`owner`, as Discord says)
/// may do through the bot, in every front end (Discord, the web UI and its
/// API, and so the CLI and TUI):
///
/// - apply a plan that deletes for real (`--delete`) or sweeps unmanaged
///   channels (`--sweep`);
/// - apply a plan that takes permissions from the bot's own role, denies
///   its role permissions in a channel, or moves roles above it;
/// - approve a pushed change to `admin_role`, which positions are admin
///   positions, or partner sync's `key_env` and partners;
/// - approve a change to `[drift] mode`, or to how recordings are
///   transcribed (`[recording]` `transcribe`, `model`, `whisper`);
/// - revert or rewind config commits;
/// - take a role from the server owner (apply, approve, or `member remove`):
///   nobody else ever can.
///
/// `owner: None` means Discord couldn't say who the owner is: taking a role
/// from anyone is then the owner's (which nobody can show they are), so
/// this fails closed. Everything else admins may do. `None`: anyone allowed
/// to ask may.
pub fn owner_only(ask: &Ask, owner: Option<Snowflake>) -> Option<OwnerOnly> {
    match ask {
        Ask::Apply(plan) => {
            let owner = plan.owner.or(owner);
            // Who a holders change takes the role from: the owner? (Unknown: anyone could be them.)
            let takes = |from: &str, to: &str| {
                let mut gone =
                    from.split_whitespace().filter(|m| m.starts_with("<@") && !to.split_whitespace().any(|t| t == *m));
                match owner {
                    Some(o) => gone.any(|m| m == format!("<@{o}>")),
                    None => gone.next().is_some(),
                }
            };
            if plan.delete || plan.actions.iter().any(|a| matches!(a, Action::Delete { .. })) {
                return Some(OwnerOnly::Delete);
            }
            if plan.sweep {
                return Some(OwnerOnly::Sweep);
            }
            for a in &plan.actions {
                if let Action::Reorder { scope: OrderScope::Roles, from, to } = a
                    && let Some(bot) = &plan.bot_role_key
                    && raises_above(bot, from, to)
                {
                    return Some(OwnerOnly::WeakenBot);
                }
                let Action::Edit { kind, id, changes, .. } = a else { continue };
                for c in changes {
                    // Permissions read "-TAKEN" → "+GIVEN"; holders list mentions.
                    let weakens = *kind == ObjectKind::Role
                        && Some(*id) == plan.bot_role
                        && c.field == Field::Permissions
                        && c.from.trim_start_matches('-').trim() != "";
                    let denies = c.field == Field::Overwrites
                        && plan.bot_role_key.as_deref().is_some_and(|bot| denies_more(bot, &c.from, &c.to));
                    if weakens || denies {
                        return Some(OwnerOnly::WeakenBot);
                    }
                    if *kind == ObjectKind::Holders && c.field == Field::Holders && takes(&c.from, &c.to) {
                        return Some(OwnerOnly::TakeFromOwner);
                    }
                }
            }
            None
        }
        Ask::Approve { old, new } => {
            let loses = |o: &Snowflake, k: &String| !new.holders.get(k).is_some_and(|n| n.contains(o));
            let losing = old.holders.iter().any(|(k, ids)| match owner {
                Some(o) => ids.contains(&o) && loses(&o, k),
                None => ids.iter().any(|u| loses(u, k)),
            });
            if losing {
                return Some(OwnerOnly::TakeFromOwner);
            }
            if old.admin_role != new.admin_role
                || old.admin_positions != new.admin_positions
                || old.key_env != new.key_env
                || old.peers != new.peers
            {
                return Some(OwnerOnly::Trust);
            }
            if Recording::transcription(old.recording.as_ref()) != Recording::transcription(new.recording.as_ref()) {
                return Some(OwnerOnly::Transcription);
            }
            (old.drift != new.drift).then_some(OwnerOnly::DriftMode)
        }
        Ask::Undo => Some(OwnerOnly::Undo),
        Ask::TakeRoles { user } => owner.is_none_or(|o| o == *user).then_some(OwnerOnly::TakeFromOwner),
    }
}

/// Whether a role reorder from `from` to `to` (keys, top first) puts a role
/// that's below `bot` now above it.
fn raises_above(bot: &str, from: &[String], to: &[String]) -> bool {
    let above = |list: &[String]| -> Vec<String> { list.iter().take_while(|k| *k != bot).cloned().collect() };
    if !from.iter().any(|k| k == bot) || !to.iter().any(|k| k == bot) {
        return false;
    }
    let (was, will) = (above(from), above(to));
    will.iter().any(|k| from.contains(k) && !was.contains(k))
}

/// Whether channel overwrites going from `from` to `to` (as plans write
/// them: `key +ALLOWED -DENIED; …`) deny the role `bot` something more, or
/// stop allowing it something.
fn denies_more(bot: &str, from: &str, to: &str) -> bool {
    let of = |text: &str, sign: char| -> Vec<String> {
        text.split("; ")
            .filter_map(|o| {
                let mut words = o.split_whitespace();
                (words.next() == Some(bot)).then(|| words.filter(|w| w.starts_with(sign)).map(str::to_owned))
            })
            .flatten()
            .collect()
    };
    let (denied, allowed) = (of(from, '-'), of(to, '+'));
    of(to, '-').iter().any(|d| !denied.contains(d)) || of(from, '+').iter().any(|a| !allowed.contains(a))
}

/// `Err` with what only the owner may do, unless `by` may.
pub fn check(by: &By, ask: &Ask, owner: Option<Snowflake>) -> Result<(), OwnerOnly> {
    let owner = match ask {
        Ask::Apply(p) => p.owner.or(owner),
        _ => owner,
    };
    match owner_only(ask, owner) {
        Some(why) if !by.may(owner) => Err(why),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipsoc_model::Change;

    const OWNER: Snowflake = Snowflake(1);
    const ADMIN: Snowflake = Snowflake(2);
    const BOT_ROLE: Snowflake = Snowflake(90);

    fn edit(kind: ObjectKind, id: Snowflake, field: Field, from: &str, to: &str) -> Action {
        let changes = vec![Change { field, from: from.into(), to: to.into() }];
        Action::Edit { kind, key: "k".into(), id, changes, cause: Default::default() }
    }

    fn plan(actions: Vec<Action>) -> Plan {
        Plan {
            actions,
            owner: Some(OWNER),
            bot_role: Some(BOT_ROLE),
            bot_role_key: Some("bot".into()),
            ..Plan::default()
        }
    }

    #[test]
    fn owner_only_plans() {
        let admin = By::person(ADMIN, "ada");
        let owner = By::person(OWNER, "olive");
        let ok = plan(vec![Action::Create { kind: ObjectKind::Channel, key: "x".into() }]);
        assert_eq!(check(&admin, &Ask::Apply(&ok), None), Ok(()));

        let cases = [
            (Plan { delete: true, ..ok.clone() }, OwnerOnly::Delete),
            (Plan { sweep: true, ..ok.clone() }, OwnerOnly::Sweep),
            (
                plan(vec![edit(ObjectKind::Role, BOT_ROLE, Field::Permissions, "-BAN_MEMBERS", "+")]),
                OwnerOnly::WeakenBot,
            ),
            (
                plan(vec![edit(ObjectKind::Holders, Snowflake(5), Field::Holders, "<@1> <@3>", "<@3>")]),
                OwnerOnly::TakeFromOwner,
            ),
        ];
        for (p, why) in cases {
            assert_eq!(check(&admin, &Ask::Apply(&p), None), Err(why));
            assert_eq!(check(&owner, &Ask::Apply(&p), None), Ok(()), "the owner may");
            assert_eq!(check(&By::bot(), &Ask::Apply(&p), None), Ok(()), "and the bot");
            assert_eq!(check(&By::named(Some("shared token")), &Ask::Apply(&p), None), Err(why), "nobody unnamed");
            assert!(why.to_string().starts_with("Only the server owner can "));
        }
        // Giving the bot more, or someone else's role, is anyone's.
        let more = plan(vec![
            edit(ObjectKind::Role, BOT_ROLE, Field::Permissions, "-", "+MANAGE_ROLES"),
            edit(ObjectKind::Role, Snowflake(7), Field::Permissions, "-BAN_MEMBERS", "+"),
            edit(ObjectKind::Holders, Snowflake(5), Field::Holders, "<@3>", "<@1>"),
            edit(ObjectKind::Channel, Snowflake(6), Field::Overwrites, "bot -SEND_MESSAGES", "bot +VIEW_CHANNEL"),
            edit(ObjectKind::Channel, Snowflake(6), Field::Overwrites, "(none)", "everyone -VIEW_CHANNEL"),
            Action::Reorder { scope: OrderScope::Roles, from: keys("bot a b"), to: keys("bot b a") },
        ]);
        assert_eq!(check(&admin, &Ask::Apply(&more), None), Ok(()));
    }

    fn keys(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn the_bot_is_weakened_in_channels_and_by_order_too() {
        let admin = By::person(ADMIN, "ada");
        let with_key = |actions| Plan { bot_role_key: Some("bot".into()), ..plan(actions) };
        let weakening = [
            // A channel that denies the bot's role, or stops allowing it something.
            edit(ObjectKind::Channel, Snowflake(6), Field::Overwrites, "(none)", "bot -VIEW_CHANNEL"),
            edit(
                ObjectKind::Category,
                Snowflake(6),
                Field::Overwrites,
                "admin +VIEW_CHANNEL; bot +VIEW_CHANNEL",
                "admin +VIEW_CHANNEL",
            ),
            // Roles moved above the bot's.
            Action::Reorder { scope: OrderScope::Roles, from: keys("bot a b"), to: keys("a bot b") },
        ];
        for a in weakening {
            let p = with_key(vec![a.clone()]);
            assert_eq!(check(&admin, &Ask::Apply(&p), None), Err(OwnerOnly::WeakenBot), "{a:?}");
            assert_eq!(check(&By::person(OWNER, "o"), &Ask::Apply(&p), None), Ok(()));
        }
        // A new role can't go above the bot's anyway; channels' order isn't roles'.
        let fine = with_key(vec![
            Action::Reorder { scope: OrderScope::Roles, from: keys("bot a"), to: keys("new bot a") },
            Action::Reorder { scope: OrderScope::Categories, from: keys("bot a"), to: keys("a bot") },
        ]);
        assert_eq!(check(&admin, &Ask::Apply(&fine), None), Ok(()));
    }

    #[test]
    fn an_unknown_owner_is_anyone_for_taking_roles() {
        let admin = By::person(ADMIN, "ada");
        let take = edit(ObjectKind::Holders, Snowflake(5), Field::Holders, "<@3> <@4>", "<@3>");
        let p = Plan { owner: None, ..plan(vec![take]) };
        assert_eq!(check(&admin, &Ask::Apply(&p), None), Err(OwnerOnly::TakeFromOwner));
        let give = edit(ObjectKind::Holders, Snowflake(5), Field::Holders, "<@3>", "<@3> <@4>");
        assert_eq!(check(&admin, &Ask::Apply(&Plan { owner: None, ..plan(vec![give]) }), None), Ok(()));
        assert_eq!(check(&admin, &Ask::TakeRoles { user: Snowflake(3) }, None), Err(OwnerOnly::TakeFromOwner));
        let mut old = Sensitive::default();
        old.holders.insert("admin".into(), vec![Snowflake(3), Snowflake(4)]);
        let mut new = old.clone();
        new.holders.insert("admin".into(), vec![Snowflake(3)]);
        assert_eq!(check(&admin, &Ask::Approve { old: &old, new: &new }, None), Err(OwnerOnly::TakeFromOwner));
        assert_eq!(check(&admin, &Ask::Approve { old: &old, new: &new }, Some(OWNER)), Ok(()), "not the owner's");
        assert_eq!(check(&admin, &Ask::Approve { old: &new, new: &old }, None), Ok(()), "giving is fine");
    }

    #[test]
    fn how_recordings_are_transcribed_is_the_owners() {
        use chipsoc_config::recording::{Engine, Recording};
        let admin = By::person(ADMIN, "ada");
        let on = Recording { channel: "c".into(), ..Default::default() };
        let old = Sensitive { recording: Some(on.clone()), ..Default::default() };
        let mut new = old.clone();
        new.recording.as_mut().unwrap().who = vec!["member".into()];
        assert_eq!(check(&admin, &Ask::Approve { old: &old, new: &new }, Some(OWNER)), Ok(()), "who may record");
        for change in [
            |r: &mut Recording| r.whisper = Some("/usr/local/bin/w".into()),
            |r: &mut Recording| r.model = Some("/m.bin".into()),
            |r: &mut Recording| r.transcribe = Some(Engine::WhisperCpp),
        ] {
            let mut new = old.clone();
            change(new.recording.as_mut().unwrap());
            let ask = Ask::Approve { old: &old, new: &new };
            assert_eq!(check(&admin, &ask, Some(OWNER)), Err(OwnerOnly::Transcription));
            assert_eq!(check(&By::person(OWNER, "o"), &ask, Some(OWNER)), Ok(()));
        }
        // Turning recording on with whisper.cpp is too.
        let off = Sensitive::default();
        let whisper = Sensitive {
            recording: Some(Recording { transcribe: Some(Engine::WhisperCpp), ..on }),
            ..Default::default()
        };
        assert_eq!(
            check(&admin, &Ask::Approve { old: &off, new: &whisper }, Some(OWNER)),
            Err(OwnerOnly::Transcription)
        );
    }

    #[test]
    fn owner_only_approvals_and_the_rest() {
        let admin = By::person(ADMIN, "ada");
        let old = Sensitive { admin_role: Some("admin".into()), ..Default::default() };
        let mut new = old.clone();
        new.holders.insert("admin".into(), vec![Snowflake(9)]);
        assert_eq!(check(&admin, &Ask::Approve { old: &old, new: &new }, Some(OWNER)), Ok(()), "a new admin");
        let mut role = old.clone();
        role.admin_role = Some("member".into());
        assert_eq!(check(&admin, &Ask::Approve { old: &old, new: &role }, Some(OWNER)), Err(OwnerOnly::Trust));
        let mut drift = old.clone();
        drift.drift = chipsoc_config::bot::DriftMode::Report;
        assert_eq!(check(&admin, &Ask::Approve { old: &old, new: &drift }, Some(OWNER)), Err(OwnerOnly::DriftMode));
        let mut held = old.clone();
        held.holders.insert("admin".into(), vec![OWNER]);
        assert_eq!(check(&admin, &Ask::Approve { old: &held, new: &old }, Some(OWNER)), Err(OwnerOnly::TakeFromOwner));
        assert_eq!(check(&By::person(OWNER, "o"), &Ask::Approve { old: &held, new: &role }, Some(OWNER)), Ok(()));

        assert_eq!(check(&admin, &Ask::Undo, Some(OWNER)), Err(OwnerOnly::Undo));
        assert_eq!(check(&admin, &Ask::Undo, None), Err(OwnerOnly::Undo), "an unknown owner is nobody");
        assert_eq!(check(&admin, &Ask::TakeRoles { user: OWNER }, Some(OWNER)), Err(OwnerOnly::TakeFromOwner));
        assert_eq!(check(&admin, &Ask::TakeRoles { user: Snowflake(3) }, Some(OWNER)), Ok(()));
    }
}
