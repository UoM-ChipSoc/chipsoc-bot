//! `pull`: copy changes made in Discord's UI back into the config, as typed
//! [`Op`]s made together (one check, all or nothing).

use chipsoc_config::Config;
use chipsoc_config::ops::{ChannelField, Op, RoleField, ServerField};
use chipsoc_config::schema::Snapshot;
use chipsoc_model::{Action, Cause, Change, Field, ObjectKind, OrderScope, Plan, Snowflake};

use crate::Pulled;

/// Keys made from projects.toml: their files are projects.toml, not these.
fn generated(key: &str) -> bool {
    key.starts_with("proj-")
}

/// The ops that copy `plan`'s Discord-side edits and orders into the config,
/// with a note for each thing copied or left for a person.
pub(crate) fn ops(cfg: &Config, live: &Snapshot, plan: &Plan, permissions: bool) -> (Vec<Op>, Vec<Pulled>) {
    let mut ops = Vec::new();
    let mut notes = Vec::new();
    for a in &plan.actions {
        match a {
            Action::Edit { kind, key, changes, cause, .. } if *cause != Cause::Discord => {
                let why = match cause {
                    Cause::Config => "the config changed it; apply to send it to Discord",
                    Cause::Both => "changed in both Discord and the config; pick one by hand",
                    _ => "not sure who changed it; check by hand",
                };
                for c in changes {
                    notes.push(Pulled::ByHand(format!("{kind} {key}: {} ({why})", c.field)));
                }
            }
            Action::Edit { kind, key, id, changes, .. } => {
                for c in changes {
                    match edit_op(cfg, live, *kind, key, *id, c, permissions) {
                        Found::Op(op) => {
                            ops.push(*op);
                            notes.push(Pulled::Adopted(format!("{kind} {key}: {}", c.field)));
                        }
                        Found::ByHand => notes.push(Pulled::ByHand(format!("{kind} {key}: {} differs", c.field))),
                        Found::Changed => {
                            notes.push(Pulled::ByHand(format!("{kind} {key}: {} changed in Discord", c.field)));
                        }
                        Found::Skip => {}
                    }
                }
            }
            Action::Reorder { scope, from, .. } => {
                let keys: Vec<String> = from.iter().filter(|k| !generated(k)).cloned().collect();
                let (op, what) = match scope {
                    OrderScope::Roles => (Op::OrderRoles { keys }, "roles".to_owned()),
                    OrderScope::Categories => (Op::OrderCategories { keys }, "categories".to_owned()),
                    OrderScope::Channels(cat) if !cat.as_deref().is_some_and(generated) => (
                        Op::OrderChannels { category: cat.clone(), keys },
                        format!("channels in {}", cat.as_deref().unwrap_or("(no category)")),
                    ),
                    OrderScope::Channels(_) => continue,
                };
                ops.push(op);
                notes.push(Pulled::Adopted(format!("the order of {what}")));
            }
            _ => {}
        }
    }
    (ops, notes)
}

enum Found {
    Op(Box<Op>),
    /// Can't be copied: someone has to edit it.
    ByHand,
    /// A channel field that can't be copied.
    Changed,
    /// Not something pull looks at.
    Skip,
}

/// The op that copies one changed field, if it can be.
fn edit_op(
    cfg: &Config,
    live: &Snapshot,
    kind: ObjectKind,
    key: &str,
    id: Snowflake,
    c: &Change,
    perms: bool,
) -> Found {
    let want = &cfg.desired;
    let k = key.to_owned();
    match kind {
        ObjectKind::Role if c.field != Field::Permissions || perms => {
            let Some(r) = live.roles.roles.iter().find(|r| live.lock.role(&r.key) == Some(id)) else {
                return Found::Skip;
            };
            let set = match c.field {
                Field::Name => RoleField::Name(r.name.clone()),
                Field::Color => RoleField::Color(r.color.clone()),
                Field::Hoist => RoleField::Hoist(r.hoist),
                Field::Mentionable => RoleField::Mentionable(r.mentionable),
                Field::Permissions => RoleField::Permissions(r.permissions.clone()),
                _ => return Found::Skip,
            };
            // Project roles come from projects.toml, not roles.toml.
            if generated(key) || want.role(key).is_none() {
                return Found::ByHand;
            }
            Found::Op(Box::new(Op::SetRole { key: k, set }))
        }
        ObjectKind::Category if c.field == Field::Name => {
            let Some(cf) = live.categories.iter().find(|x| live.lock.category(&x.category.key) == Some(id)) else {
                return Found::Skip;
            };
            if generated(key) || want.category(key).is_none() {
                return Found::ByHand;
            }
            Found::Op(Box::new(Op::RenameCategory { key: k, name: cf.category.name.clone() }))
        }
        ObjectKind::Channel => {
            let Some(ch) = live.channels().map(|(c, _)| c).find(|x| live.lock.channel(&x.key) == Some(id)) else {
                return Found::Skip;
            };
            let set = match c.field {
                Field::Name => ChannelField::Name(ch.name.clone()),
                Field::Topic => ChannelField::Topic(ch.topic.clone()),
                Field::Slowmode => ChannelField::Slowmode(ch.slowmode),
                Field::Nsfw => ChannelField::Nsfw(ch.nsfw),
                Field::UserLimit => ChannelField::UserLimit(ch.user_limit),
                Field::Tags => ChannelField::Tags(ch.tags.clone()),
                _ => return Found::Changed,
            };
            if generated(key) || want.channel(key).is_none() {
                return Found::ByHand;
            }
            Found::Op(Box::new(Op::SetChannel { key: k, set }))
        }
        ObjectKind::Server => {
            let g = &live.server.guild;
            let set = match c.field {
                Field::Name => ServerField::Name(g.name.clone()),
                Field::Description => ServerField::Description(g.description.clone()),
                Field::SystemChannel => ServerField::SystemChannel(g.system_channel.clone()),
                Field::RulesChannel => ServerField::RulesChannel(g.rules_channel.clone()),
                Field::UpdatesChannel => ServerField::UpdatesChannel(g.updates_channel.clone()),
                Field::AfkChannel => ServerField::AfkChannel(g.afk_channel.clone()),
                Field::VerificationLevel => ServerField::VerificationLevel(g.verification_level),
                Field::ContentFilter => ServerField::ExplicitContentFilter(g.explicit_content_filter),
                Field::Notifications => ServerField::DefaultMessageNotifications(g.default_message_notifications),
                _ => return Found::Skip,
            };
            Found::Op(Box::new(Op::SetServer { set }))
        }
        _ => Found::ByHand,
    }
}
