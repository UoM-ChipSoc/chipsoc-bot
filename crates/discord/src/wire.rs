//! Config types ⇄ Discord JSON, in one place.

use chipsoc_config::schema::*;
use chipsoc_config::template::{Button, Rendered};
use chipsoc_model::perms;
use serde_json::{Value, json};

/// Prefix on button custom ids, so the runtime can tell ours apart.
pub const BUTTON_PREFIX: &str = "do:";

pub fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

pub fn id(v: &Value) -> Option<Snowflake> {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64().map(Snowflake))
}

pub fn ids(v: &Value) -> Vec<Snowflake> {
    v.as_array().into_iter().flatten().filter_map(id).collect()
}

/// A member's names (from a guild member object), in the order Discord
/// shows them: nickname, display name, username. Blank ones are left out.
pub fn names(member: &Value) -> impl Iterator<Item = String> + '_ {
    [&member["nick"], &member["user"]["global_name"], &member["user"]["username"]]
        .into_iter()
        .map(s)
        .filter(|n| !n.trim().is_empty())
}

/// The name Discord shows for a member: nickname, else display name, else
/// username (`None` if it has none).
pub fn shown_name(member: &Value) -> Option<String> {
    names(member).next()
}

pub fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned)).collect()
}

pub fn color(hex: &Option<String>) -> u32 {
    hex.as_ref().and_then(|h| u32::from_str_radix(h.trim_start_matches('#'), 16).ok()).unwrap_or(0)
}

/// Overwrites to Discord, resolving role keys through the lock. Unresolvable
/// entries are errors: dropping one could leave a channel more open than configured.
pub fn overwrites_json(ows: &[Overwrite], lock: &Lock) -> Result<Value, String> {
    let mut out = Vec::new();
    for o in ows {
        let (id, kind) = match (&o.role, o.member) {
            (Some(k), _) => (lock.role(k).ok_or(format!("role `{k}` doesn't exist yet"))?, 0),
            (None, Some(m)) => (m, 1),
            _ => return Err("overwrite without a role or member".into()),
        };
        let allow = perms::bits(&o.allow)?;
        let deny = perms::bits(&o.deny)?;
        out.push(json!({ "id": id.to_string(), "type": kind, "allow": allow.to_string(), "deny": deny.to_string() }));
    }
    Ok(Value::Array(out))
}

pub fn overwrites_from(v: &Value, key_of_role: &impl Fn(Snowflake) -> String) -> Vec<Overwrite> {
    let bits = |x: &Value| x.as_str().and_then(|s| s.parse().ok()).or_else(|| x.as_u64()).unwrap_or(0);
    let mut out: Vec<Overwrite> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let target = id(&o["id"])?;
            let (allow, deny) = (perms::names(bits(&o["allow"])), perms::names(bits(&o["deny"])));
            Some(if o["type"].as_u64() == Some(1) || o["type"].as_str() == Some("1") {
                Overwrite { member: Some(target), allow, deny, ..Default::default() }
            } else {
                Overwrite { role: Some(key_of_role(target)), allow, deny, ..Default::default() }
            })
        })
        .filter(|o| !o.allow.is_empty() || !o.deny.is_empty())
        .collect();
    out.sort_by(|a, b| (a.role.as_deref(), a.member).cmp(&(b.role.as_deref(), b.member)));
    out
}

pub fn role_json(r: &Role) -> Result<Value, String> {
    Ok(json!({
        "name": r.name,
        "color": color(&r.color),
        "hoist": r.hoist,
        "mentionable": r.mentionable,
        "permissions": perms::bits(&r.permissions)?.to_string(),
    }))
}

/// A channel to create (everything) or edit (only `fields`).
pub fn channel_json(c: &Channel, parent: Option<Snowflake>, lock: &Lock, live_tags: &Value) -> Result<Value, String> {
    let mut v = json!({
        "name": c.name,
        "type": c.kind.discord(),
        "parent_id": parent.map(|p| p.to_string()),
        "nsfw": c.nsfw,
        "permission_overwrites": overwrites_json(&c.overwrites, lock)?,
    });
    if !c.kind.is_voice() {
        v["topic"] = json!(c.topic.clone().unwrap_or_default());
        v["rate_limit_per_user"] = json!(c.slowmode);
    }
    if c.kind.is_voice() {
        v["user_limit"] = json!(c.user_limit.unwrap_or(0));
    }
    if c.kind.has_tags() {
        v["available_tags"] = tags_json(&c.tags, live_tags);
        if let Some(sort) = c.sort {
            v["default_sort_order"] = json!(sort.discord());
        }
    }
    Ok(v)
}

/// Forum tags, keeping the ids of tags that already exist so posts keep them.
pub fn tags_json(names: &[String], live: &Value) -> Value {
    let existing: Vec<&Value> = live.as_array().map(|a| a.iter().collect()).unwrap_or_default();
    Value::Array(
        names
            .iter()
            .map(|n| match existing.iter().find(|t| t["name"].as_str() == Some(n.as_str())) {
                Some(t) => json!({ "id": t["id"], "name": n }),
                None => json!({ "name": n }),
            })
            .collect(),
    )
}

pub fn components_json(buttons: &[Button]) -> Value {
    if buttons.is_empty() {
        return json!([]);
    }
    let row: Vec<Value> = buttons
        .iter()
        .take(5)
        .map(
            |b| json!({ "type": 2, "style": 1, "label": b.label, "custom_id": format!("{BUTTON_PREFIX}{}", b.action) }),
        )
        .collect();
    json!([{ "type": 1, "components": row }])
}

/// A message's text and our buttons, as a [`Rendered`].
pub fn rendered_from(msg: &Value) -> Rendered {
    let buttons = msg["components"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|row| row["components"].as_array().into_iter().flatten())
        .filter_map(|b| {
            let action = b["custom_id"].as_str()?.strip_prefix(BUTTON_PREFIX)?;
            Some(Button { action: action.to_owned(), label: s(&b["label"]) })
        })
        .collect();
    Rendered { content: s(&msg["content"]), buttons }
}

pub fn message_json(r: &Rendered) -> Value {
    json!({ "content": r.content, "components": components_json(&r.buttons), "allowed_mentions": { "parse": [] } })
}

// --- settings -------------------------------------------------------------

pub fn onboarding_json(o: &Onboarding, lock: &Lock) -> Value {
    let cid = |k: &String| lock.channel(k).map(|i| i.to_string());
    let prompts: Vec<Value> = o
        .prompts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let options: Vec<Value> = p
                .options
                .iter()
                .map(|opt| {
                    json!({
                        "title": opt.title,
                        "description": opt.description,
                        "emoji_name": opt.emoji,
                        "role_ids": lock.role(&opt.role).map(|r| vec![r.to_string()]).unwrap_or_default(),
                        "channel_ids": opt.channels.iter().filter_map(cid).collect::<Vec<_>>(),
                    })
                })
                .collect();
            json!({
                "id": (i + 1).to_string(), "type": 0, "title": p.title, "single_select": p.single_select,
                "required": p.required, "in_onboarding": true, "options": options,
            })
        })
        .collect();
    json!({
        "enabled": o.enabled,
        "mode": u8::from(o.advanced),
        "default_channel_ids": o.default_channels.iter().filter_map(cid).collect::<Vec<_>>(),
        "prompts": prompts,
    })
}

pub fn onboarding_from(v: &Value, lock: &Lock) -> Option<Onboarding> {
    v["prompts"].as_array()?;
    let ckey = |x: &Value| id(x).and_then(|i| lock.channel_key(i)).map(str::to_owned);
    let mut default_channels: Vec<String> =
        v["default_channel_ids"].as_array().into_iter().flatten().filter_map(ckey).collect();
    default_channels.sort();
    let prompts = v["prompts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| OnbPrompt {
            title: s(&p["title"]),
            single_select: p["single_select"].as_bool().unwrap_or(true),
            required: p["required"].as_bool().unwrap_or(false),
            options: p["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|o| OnbOption {
                    title: s(&o["title"]),
                    description: o["description"].as_str().filter(|d| !d.is_empty()).map(str::to_owned),
                    emoji: o["emoji"]["name"].as_str().or(o["emoji_name"].as_str()).map(str::to_owned),
                    role: ids(&o["role_ids"]).first().and_then(|r| lock.role_key(*r)).unwrap_or_default().to_owned(),
                    channels: o["channel_ids"].as_array().into_iter().flatten().filter_map(ckey).collect(),
                })
                .collect(),
        })
        .collect();
    Some(Onboarding {
        enabled: v["enabled"].as_bool().unwrap_or(false),
        advanced: v["mode"].as_u64() == Some(1),
        default_channels,
        prompts,
    })
}

pub fn screening_json(s: &Screening) -> Value {
    json!({
        "enabled": s.enabled,
        "description": s.description,
        "form_fields": [{ "field_type": "TERMS", "label": s.label, "required": true, "values": s.rules }],
    })
}

pub fn screening_from(v: &Value, enabled: bool) -> Option<Screening> {
    let terms = v["form_fields"].as_array()?.iter().find(|f| f["field_type"] == "TERMS")?;
    Some(Screening {
        enabled,
        description: s(&v["description"]),
        label: s(&terms["label"]),
        rules: strings(&terms["values"]),
    })
}

pub fn automod_json(r: &AutoModRule, lock: &Lock) -> Result<Value, String> {
    let metadata = match r.trigger {
        Trigger::Keyword => json!({ "keyword_filter": r.keywords, "regex_patterns": r.regex, "allow_list": r.allow }),
        Trigger::KeywordPreset => {
            json!({ "presets": r.presets.iter().map(|p| p.discord()).collect::<Vec<_>>(), "allow_list": r.allow })
        }
        Trigger::MentionSpam => json!({
            "mention_total_limit": r.mention_limit.unwrap_or(10),
            "mention_raid_protection_enabled": r.raid_protection,
        }),
        Trigger::Spam => json!({}),
    };
    let mut actions = Vec::new();
    for a in &r.actions {
        actions.push(match a {
            ModAction::Block => match &r.block_message {
                Some(m) => json!({ "type": 1, "metadata": { "custom_message": m } }),
                None => json!({ "type": 1 }),
            },
            ModAction::Alert => {
                let k = r.alert_channel.as_deref().ok_or("alert needs alert_channel")?;
                let ch = lock.channel(k).ok_or(format!("channel `{k}` doesn't exist yet"))?;
                json!({ "type": 2, "metadata": { "channel_id": ch.to_string() } })
            }
            ModAction::Timeout => {
                json!({ "type": 3, "metadata": { "duration_seconds": r.timeout_seconds.unwrap_or(60) } })
            }
        });
    }
    Ok(json!({
        "name": r.name,
        "event_type": 1,
        "trigger_type": r.trigger.discord(),
        "trigger_metadata": metadata,
        "actions": actions,
        "enabled": r.enabled,
        "exempt_roles": r.exempt_roles.iter().filter_map(|k| lock.role(k)).map(|i| i.to_string()).collect::<Vec<_>>(),
        "exempt_channels": r.exempt_channels.iter().filter_map(|k| lock.channel(k)).map(|i| i.to_string()).collect::<Vec<_>>(),
    }))
}

pub fn automod_from(r: &Value, key: String, lock: &Lock) -> Option<AutoModRule> {
    let trigger = Trigger::from_discord(r["trigger_type"].as_u64()? as u8)?;
    let md = &r["trigger_metadata"];
    let mut rule = AutoModRule {
        key,
        name: s(&r["name"]),
        trigger,
        enabled: r["enabled"].as_bool().unwrap_or(true),
        keywords: strings(&md["keyword_filter"]),
        regex: strings(&md["regex_patterns"]),
        allow: strings(&md["allow_list"]),
        presets: md["presets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Preset::from_discord(p.as_u64()? as u8))
            .collect(),
        mention_limit: md["mention_total_limit"].as_u64().map(|n| n as u8),
        raid_protection: md["mention_raid_protection_enabled"].as_bool().unwrap_or(false),
        exempt_roles: ids(&r["exempt_roles"]).into_iter().filter_map(|i| lock.role_key(i).map(str::to_owned)).collect(),
        exempt_channels: ids(&r["exempt_channels"])
            .into_iter()
            .filter_map(|i| lock.channel_key(i).map(str::to_owned))
            .collect(),
        ..Default::default()
    };
    for a in r["actions"].as_array().into_iter().flatten() {
        match a["type"].as_u64() {
            Some(1) => {
                rule.actions.push(ModAction::Block);
                rule.block_message =
                    a["metadata"]["custom_message"].as_str().filter(|m| !m.is_empty()).map(str::to_owned);
            }
            Some(2) => {
                rule.actions.push(ModAction::Alert);
                rule.alert_channel =
                    id(&a["metadata"]["channel_id"]).and_then(|i| lock.channel_key(i)).map(str::to_owned);
            }
            Some(3) => {
                rule.actions.push(ModAction::Timeout);
                rule.timeout_seconds = a["metadata"]["duration_seconds"].as_u64().map(|n| n as u32);
            }
            _ => {}
        }
    }
    Some(rule)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_members_shown_name_is_nick_then_display_name_then_username() {
        let m =
            |nick: Value, global: Value| json!({ "nick": nick, "user": { "username": "ada", "global_name": global } });
        assert_eq!(shown_name(&m(json!("Ada L."), json!("Ada"))).as_deref(), Some("Ada L."));
        assert_eq!(shown_name(&m(Value::Null, json!("Ada"))).as_deref(), Some("Ada"));
        assert_eq!(shown_name(&m(json!(" "), Value::Null)).as_deref(), Some("ada"), "blank names are skipped");
        assert_eq!(shown_name(&json!({})), None);
    }

    /// A lockfile that knows a few roles and channels, as `lock.toml` writes it.
    fn lock() -> Lock {
        serde_json::from_value(json!({
            "guild": "613425648685547541",
            "roles": { "member": "982014491980083211", "mods": "323456789123456789", "alumni": "423456789123456789" },
            "channels": {
                "general": "962007075288916001",
                "rules": "962007075288916002",
                "mod-log": "123456789123456789",
                "memes": "523456789123456789",
            },
        }))
        .unwrap()
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Role ids to keys as export does: unknown roles keep their id.
    fn role_key(lock: &Lock) -> impl Fn(Snowflake) -> String + '_ {
        |id| lock.role_key(id).map_or_else(|| id.to_string(), str::to_owned)
    }

    fn ow(role: Option<&str>, member: Option<u64>, allow: &[&str], deny: &[&str]) -> Overwrite {
        Overwrite { role: role.map(str::to_owned), member: member.map(Snowflake), allow: strs(allow), deny: strs(deny) }
    }

    #[test]
    fn overwrites_round_trip() {
        let lock = lock();
        // In the order export gives: members first, then roles by key.
        let ows = vec![
            ow(None, Some(80351110224678912), &["MANAGE_MESSAGES"], &["SEND_MESSAGES"]),
            ow(Some("everyone"), None, &[], &["VIEW_CHANNEL"]),
            ow(Some("member"), None, &["VIEW_CHANNEL", "SEND_MESSAGES"], &[]),
        ];
        let v = overwrites_json(&ows, &lock).unwrap();
        assert_eq!(overwrites_from(&v, &role_key(&lock)), ows);
        assert!(overwrites_json(&[ow(Some("ghost"), None, &["VIEW_CHANNEL"], &[])], &lock).is_err(), "unknown role");
    }

    #[test]
    fn overwrites_from_a_discord_channel() {
        let lock = lock();
        let channel = json!({
            "id": "962007075288916001", "type": 0, "guild_id": "613425648685547541", "name": "general",
            "permission_overwrites": [
                { "id": "613425648685547541", "type": 0, "allow": "0", "deny": "1024" },
                { "id": "982014491980083211", "type": 0, "allow": "3072", "deny": "0" },
                { "id": "80351110224678912", "type": 1, "allow": "8192", "deny": "2048" },
                { "id": "777", "type": 0, "allow": "64", "deny": "0" },
                { "id": "778", "type": 0, "allow": "0", "deny": "0" },
            ],
        });
        let got = overwrites_from(&channel["permission_overwrites"], &role_key(&lock));
        assert_eq!(
            got,
            [
                ow(None, Some(80351110224678912), &["MANAGE_MESSAGES"], &["SEND_MESSAGES"]),
                ow(Some("777"), None, &["ADD_REACTIONS"], &[]),
                ow(Some("everyone"), None, &[], &["VIEW_CHANNEL"]),
                ow(Some("member"), None, &["VIEW_CHANNEL", "SEND_MESSAGES"], &[]),
            ],
            "an overwrite that does nothing is dropped; an unknown role keeps its id"
        );
        assert_eq!(overwrites_from(&overwrites_json(&got[2..], &lock).unwrap(), &role_key(&lock)), got[2..]);
    }

    #[test]
    fn messages_round_trip() {
        let r = Rendered {
            content: "Welcome! Start below.".into(),
            buttons: vec![
                Button { action: "join".into(), label: "Start".into() },
                Button { action: "apply:golem".into(), label: "Apply to join".into() },
            ],
        };
        assert_eq!(rendered_from(&message_json(&r)), r);
        let plain = Rendered { content: "No buttons.".into(), buttons: vec![] };
        assert_eq!(rendered_from(&message_json(&plain)), plain);
    }

    #[test]
    fn a_discord_message_keeps_only_our_buttons() {
        let msg = json!({
            "id": "1100000000000000001", "channel_id": "962007075288916001", "type": 0,
            "author": { "id": "42", "username": "bot", "bot": true },
            "content": "Volunteer with us",
            "components": [{ "type": 1, "components": [
                { "type": 2, "style": 1, "label": "Volunteer", "custom_id": "do:volunteer" },
                { "type": 2, "style": 5, "label": "Docs", "url": "https://example.org" },
                { "type": 2, "style": 2, "label": "Someone else's", "custom_id": "poll:1" },
            ]}],
        });
        let r = rendered_from(&msg);
        assert_eq!(r.content, "Volunteer with us");
        assert_eq!(r.buttons, [Button { action: "volunteer".into(), label: "Volunteer".into() }]);
        assert_eq!(rendered_from(&message_json(&r)), r);
    }

    #[test]
    fn onboarding_round_trips() {
        let lock = lock();
        let o = Onboarding {
            enabled: true,
            advanced: true,
            default_channels: strs(&["general", "rules"]),
            prompts: vec![
                OnbPrompt {
                    title: "What brings you here?".into(),
                    single_select: false,
                    required: true,
                    options: vec![
                        OnbOption {
                            title: "Studying".into(),
                            description: Some("Current students".into()),
                            emoji: Some("🎓".into()),
                            role: "member".into(),
                            channels: strs(&["general"]),
                        },
                        OnbOption { title: "Just looking".into(), channels: strs(&["memes"]), ..Default::default() },
                    ],
                },
                OnbPrompt {
                    title: "Graduated?".into(),
                    single_select: true,
                    required: false,
                    options: vec![OnbOption { title: "Yes".into(), role: "alumni".into(), ..Default::default() }],
                },
            ],
        };
        assert_eq!(onboarding_from(&onboarding_json(&o, &lock), &lock), Some(o));
    }

    #[test]
    fn onboarding_from_discord() {
        let lock = lock();
        // GET /guilds/{id}/onboarding, as Discord documents it.
        let v = json!({
            "guild_id": "613425648685547541",
            "prompts": [{
                "id": "1067461047608422473",
                "title": "What do you want to do in this community?",
                "options": [
                    {
                        "id": "1067461047608422476", "title": "Chat with Friends", "description": "",
                        "emoji": { "id": null, "name": "💬", "animated": false },
                        "role_ids": [], "channel_ids": ["962007075288916001"],
                    },
                    {
                        "id": "1070004843541954678", "title": "Get Gud", "description": "We have excellent teachers!",
                        "emoji": { "id": null, "name": "😀", "animated": false },
                        "role_ids": ["982014491980083211"], "channel_ids": [],
                    },
                ],
                "single_select": false, "required": false, "in_onboarding": true, "type": 0,
            }],
            "default_channel_ids": ["962007075288916002", "962007075288916001"],
            "enabled": true,
            "mode": 0,
        });
        let o = onboarding_from(&v, &lock).unwrap();
        let want = Onboarding {
            enabled: true,
            advanced: false,
            default_channels: strs(&["general", "rules"]),
            prompts: vec![OnbPrompt {
                title: "What do you want to do in this community?".into(),
                single_select: false,
                required: false,
                options: vec![
                    OnbOption {
                        title: "Chat with Friends".into(),
                        emoji: Some("💬".into()),
                        channels: strs(&["general"]),
                        ..Default::default()
                    },
                    OnbOption {
                        title: "Get Gud".into(),
                        description: Some("We have excellent teachers!".into()),
                        emoji: Some("😀".into()),
                        role: "member".into(),
                        ..Default::default()
                    },
                ],
            }],
        };
        assert_eq!(o, want);
        assert_eq!(onboarding_from(&onboarding_json(&o, &lock), &lock), Some(o));
        assert_eq!(onboarding_from(&json!({ "message": "Unknown Guild" }), &lock), None, "not onboarding");
    }

    #[test]
    fn screening_round_trips() {
        let s = Screening {
            enabled: true,
            description: "Welcome! A few rules first.".into(),
            label: "I have read and agree to the rules".into(),
            rules: strs(&["Be kind", "No spam"]),
        };
        assert_eq!(screening_from(&screening_json(&s), s.enabled), Some(s));
    }

    #[test]
    fn screening_from_discord() {
        // GET /guilds/{id}/member-verification (undocumented; this is what it returns).
        let v = json!({
            "version": "2024-03-01T12:00:00.000000+00:00",
            "description": "Welcome to the server.",
            "form_fields": [
                { "field_type": "PARAGRAPH", "label": "Why are you here?", "required": false, "values": null },
                {
                    "field_type": "TERMS", "label": "Read and agree to the server rules", "description": null,
                    "automations": null, "required": true, "values": ["Be kind", "No spam"],
                },
            ],
        });
        let s = screening_from(&v, false).unwrap();
        assert_eq!(
            s,
            Screening {
                enabled: false,
                description: "Welcome to the server.".into(),
                label: "Read and agree to the server rules".into(),
                rules: strs(&["Be kind", "No spam"]),
            }
        );
        assert_eq!(screening_from(&screening_json(&s), s.enabled), Some(s));
        assert_eq!(screening_from(&json!({ "description": "", "form_fields": [] }), true), None, "no rules");
    }

    fn rule(key: &str, trigger: Trigger) -> AutoModRule {
        AutoModRule { key: key.into(), name: key.into(), trigger, enabled: true, ..Default::default() }
    }

    #[test]
    fn automod_round_trips_for_each_trigger() {
        let lock = lock();
        let rules = [
            AutoModRule {
                enabled: false,
                keywords: strs(&["cat*", "*dog"]),
                regex: strs(&["(b|c)at"]),
                allow: strs(&["catalogue"]),
                actions: vec![ModAction::Block, ModAction::Alert, ModAction::Timeout],
                alert_channel: Some("mod-log".into()),
                timeout_seconds: Some(300),
                block_message: Some("Not here, please.".into()),
                exempt_roles: strs(&["mods"]),
                exempt_channels: strs(&["memes"]),
                ..rule("words", Trigger::Keyword)
            },
            AutoModRule {
                presets: vec![Preset::Profanity, Preset::Slurs],
                allow: strs(&["scunthorpe"]),
                actions: vec![ModAction::Block],
                ..rule("presets", Trigger::KeywordPreset)
            },
            AutoModRule {
                mention_limit: Some(5),
                raid_protection: true,
                actions: vec![ModAction::Block, ModAction::Alert],
                alert_channel: Some("mod-log".into()),
                ..rule("mentions", Trigger::MentionSpam)
            },
            AutoModRule { actions: vec![ModAction::Block], ..rule("spam", Trigger::Spam) },
        ];
        for r in rules {
            let v = automod_json(&r, &lock).unwrap();
            assert_eq!(automod_from(&v, r.key.clone(), &lock).as_ref(), Some(&r), "{v}");
        }
    }

    #[test]
    fn automod_from_discord() {
        let lock = lock();
        // An auto moderation rule object, as Discord documents it.
        let v = json!({
            "id": "969707018069872670",
            "guild_id": "613425648685547541",
            "name": "Keyword Filter 1",
            "creator_id": "423457898095789043",
            "trigger_type": 1,
            "event_type": 1,
            "actions": [
                { "type": 1, "metadata": { "custom_message": "Please keep financial discussions limited to the #finance channel" } },
                { "type": 2, "metadata": { "channel_id": "123456789123456789" } },
                { "type": 3, "metadata": { "duration_seconds": 60 } },
            ],
            "trigger_metadata": {
                "keyword_filter": ["cat*", "*dog", "*ana*", "i like c++"],
                "regex_patterns": ["(b|c)at", "^(?:[0-9]{1,3}\\.){3}[0-9]{1,3}$"],
            },
            "enabled": true,
            "exempt_roles": ["323456789123456789", "423456789123456789"],
            "exempt_channels": ["523456789123456789"],
        });
        let r = automod_from(&v, "keyword-filter-1".into(), &lock).unwrap();
        let want = AutoModRule {
            name: "Keyword Filter 1".into(),
            keywords: strs(&["cat*", "*dog", "*ana*", "i like c++"]),
            regex: strs(&["(b|c)at", "^(?:[0-9]{1,3}\\.){3}[0-9]{1,3}$"]),
            actions: vec![ModAction::Block, ModAction::Alert, ModAction::Timeout],
            alert_channel: Some("mod-log".into()),
            timeout_seconds: Some(60),
            block_message: Some("Please keep financial discussions limited to the #finance channel".into()),
            exempt_roles: strs(&["mods", "alumni"]),
            exempt_channels: strs(&["memes"]),
            ..rule("keyword-filter-1", Trigger::Keyword)
        };
        assert_eq!(r, want);
        assert_eq!(automod_from(&automod_json(&r, &lock).unwrap(), r.key.clone(), &lock), Some(r));

        let mentions = json!({
            "id": "969707018069872671", "guild_id": "613425648685547541", "name": "Mention Spam",
            "trigger_type": 5, "event_type": 1, "enabled": true,
            "trigger_metadata": { "mention_total_limit": 8, "mention_raid_protection_enabled": true },
            "actions": [{ "type": 1, "metadata": {} }],
            "exempt_roles": [], "exempt_channels": [],
        });
        let r = automod_from(&mentions, "mention-spam".into(), &lock).unwrap();
        assert_eq!((r.mention_limit, r.raid_protection, r.block_message.as_deref()), (Some(8), true, None));
        assert_eq!(automod_from(&automod_json(&r, &lock).unwrap(), r.key.clone(), &lock), Some(r));

        let profile = json!({ "trigger_type": 6, "trigger_metadata": {}, "actions": [] });
        assert_eq!(automod_from(&profile, "names".into(), &lock), None, "a trigger the config can't express");
    }
}
