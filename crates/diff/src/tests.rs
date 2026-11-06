use super::*;

fn chan(key: &str, kind: ChannelKind) -> Channel {
    Channel { key: key.into(), name: key.into(), kind, ..Default::default() }
}

fn snap(channels: &[(&str, ChannelKind)], lock: &[(&str, u64)]) -> Snapshot {
    let mut s = Snapshot::default();
    s.server.guild.name = "T".into();
    s.categories.push(CategoryFile {
        category: Category { key: "cat".into(), name: "Cat".into(), overwrites: vec![] },
        channels: channels.iter().map(|(k, t)| chan(k, *t)).collect(),
    });
    s.lock.categories.insert("cat".into(), Snowflake(1));
    for (k, id) in lock {
        s.lock.channels.insert((*k).into(), Snowflake(*id));
    }
    s
}

fn cfg(desired: Snapshot) -> Config {
    Config { desired, ..Default::default() }
}

fn ops(p: &Plan) -> Vec<String> {
    p.actions.iter().map(|a| a.headline()).collect()
}

#[test]
fn in_sync_is_empty() {
    let s = snap(&[("general", ChannelKind::Text)], &[("general", 10)]);
    assert!(plan(&cfg(s.clone()), &s, Options::default()).is_empty());
}

#[test]
fn rename_is_an_edit_matched_by_id() {
    let mut want = snap(&[("general", ChannelKind::Text)], &[("general", 10)]);
    want.categories[0].channels[0].name = "chat".into();
    let live = snap(&[("general", ChannelKind::Text)], &[("general", 10)]);
    assert_eq!(ops(&plan(&cfg(want), &live, Options::default())), ["~ edit channel general"]);
}

#[test]
fn live_keys_dont_matter_only_ids() {
    let want = snap(&[("general", ChannelKind::Text)], &[("general", 10)]);
    let mut live = snap(&[("chat", ChannelKind::Text)], &[("chat", 10)]);
    live.categories[0].channels[0].name = "general".into();
    assert!(plan(&cfg(want), &live, Options::default()).is_empty());
}

#[test]
fn order_is_compared_as_lists() {
    let want = snap(&[("a", ChannelKind::Text), ("b", ChannelKind::Text)], &[("a", 10), ("b", 11)]);
    let live = snap(&[("b", ChannelKind::Text), ("a", ChannelKind::Text)], &[("a", 10), ("b", 11)]);
    assert_eq!(ops(&plan(&cfg(want), &live, Options::default())), ["↕ reorder channels in cat: b below a"]);
    // Voice is listed after text either way, so this isn't a difference.
    let want = snap(&[("v", ChannelKind::Voice), ("t", ChannelKind::Text)], &[("v", 10), ("t", 11)]);
    let live = snap(&[("t", ChannelKind::Text), ("v", ChannelKind::Voice)], &[("v", 10), ("t", 11)]);
    assert!(plan(&cfg(want), &live, Options::default()).is_empty());
}

#[test]
fn removed_channels_are_archived_unless_delete() {
    let mut want = snap(&[], &[("old", 10)]);
    want.lock.categories.insert("archive".into(), Snowflake(9));
    let live = snap(&[("old", ChannelKind::Text)], &[("old", 10)]);
    let c = cfg(want);
    assert_eq!(ops(&plan(&c, &live, Options::default())), ["→ archive channel old"]);
    let del = Options { delete: true, ..Default::default() };
    assert_eq!(ops(&plan(&c, &live, del)), ["- delete channel old"]);
}

#[test]
fn unmanaged_channels_are_only_reported() {
    let want = snap(&[], &[]);
    let live = snap(&[("stray", ChannelKind::Text)], &[("stray", 10)]);
    let p = plan(&cfg(want.clone()), &live, Options::default());
    assert!(p.is_empty());
    assert!(matches!(p.warnings[0], Warning::Unmanaged { .. }));
    let sweep = Options { sweep: true, ..Default::default() };
    assert_eq!(ops(&plan(&cfg(want), &live, sweep)), ["→ archive channel stray"]);
}

#[test]
fn impossible_type_change_recreates() {
    let want = snap(&[("feeds", ChannelKind::Forum)], &[("feeds", 10)]);
    let live = snap(&[("feeds", ChannelKind::Text)], &[("feeds", 10)]);
    let p = plan(&cfg(want), &live, Options::default());
    assert_eq!(ops(&p)[..2], ["→ archive channel feeds", "+ create channel feeds"]);
    let want = snap(&[("news", ChannelKind::Announcement)], &[("news", 10)]);
    let live = snap(&[("news", ChannelKind::Text)], &[("news", 10)]);
    assert_eq!(ops(&plan(&cfg(want), &live, Options::default())), ["~ edit channel news"]);
}

#[test]
fn overwrites_compare_by_role_id() {
    let ow =
        |role: &str| Overwrite { role: Some(role.into()), deny: vec!["VIEW_CHANNEL".into()], ..Default::default() };
    let mut want = snap(&[("c", ChannelKind::Text)], &[("c", 10)]);
    want.categories[0].channels[0].overwrites = vec![ow("member")];
    want.lock.roles.insert("member".into(), Snowflake(5));
    let mut live = snap(&[("c", ChannelKind::Text)], &[("c", 10)]);
    live.categories[0].channels[0].overwrites = vec![ow("members-2")];
    live.lock.roles.insert("members-2".into(), Snowflake(5));
    assert!(plan(&cfg(want), &live, Options::default()).actions.iter().all(|a| !matches!(a, Action::Edit { .. })));
}

#[test]
fn bot_below_managed_roles_warns_and_skips() {
    let role = |k: &str, managed| Role { key: k.into(), name: k.into(), managed, ..Default::default() };
    let mut want = snap(&[], &[]);
    want.roles.roles = vec![role("bot", true), role("chair", false)];
    want.lock.roles = [("bot".to_owned(), Snowflake(2)), ("chair".to_owned(), Snowflake(3))].into();
    let mut live = want.clone();
    live.roles.roles = vec![role("chair", false), role("bot", true)];
    let mut chair = live.roles.roles[0].clone();
    chair.name = "Boss".into();
    live.roles.roles[0] = chair;
    let p = plan(&cfg(want.clone()), &live, Options { bot_role: Some(Snowflake(2)), ..Default::default() });
    let above = p.warnings.iter().find_map(|w| match w {
        Warning::BotRoleTooLow { above, .. } => Some(above.clone()),
        _ => None,
    });
    assert_eq!(above, Some(vec!["Boss".to_owned()]), "the warning names the roles above the bot");
    assert!(
        ops(&p).contains(&". skip role chair (above the bot's role, so Discord won't let it edit this)".to_owned())
    );

    // A role above the bot out of order isn't a reorder the bot would try (and fail) every time.
    want.roles.roles.push(role("member", false));
    want.lock.roles.insert("member".into(), Snowflake(5));
    let mut live = want.clone();
    live.roles.roles = vec![role("member", false), role("bot", true), role("chair", false)];
    let p = plan(&cfg(want), &live, Options { bot_role: Some(Snowflake(2)), ..Default::default() });
    assert!(!ops(&p).iter().any(|o| o.contains("reorder")), "{:?}", ops(&p));
}

#[test]
fn another_bots_role_above_is_no_reason_to_warn() {
    // A read-only bot planning in CI, its role just below the main bot's.
    let role = |k: &str, managed| Role { key: k.into(), name: k.into(), managed, ..Default::default() };
    let mut want = snap(&[], &[]);
    want.roles.roles = vec![role("bot", true), role("reader", true), role("chair", false)];
    want.lock.roles =
        [("bot".to_owned(), Snowflake(2)), ("reader".to_owned(), Snowflake(4)), ("chair".to_owned(), Snowflake(3))]
            .into();
    let mut live = want.clone();
    live.roles.roles[2].name = "Boss".into();
    let p = plan(&cfg(want), &live, Options { bot_role: Some(Snowflake(4)), ..Default::default() });
    assert!(!p.warnings.iter().any(|w| matches!(w, Warning::BotRoleTooLow { .. })), "{:?}", p.warnings);
    assert_eq!(ops(&p), ["~ edit role chair"]);
}

#[test]
fn the_admin_role_is_never_taken_from_everyone_and_losing_it_is_called_out() {
    let base = || {
        let mut s = snap(&[], &[]);
        s.roles.roles.push(Role { key: "admin".into(), name: "Admin".into(), ..Default::default() });
        s.lock.roles.insert("admin".into(), Snowflake(50));
        s
    };
    let mut live = base();
    live.holders.insert("admin".into(), [Snowflake(7), Snowflake(8)].into());
    live.members = [Snowflake(7), Snowflake(8)].into();
    let mut c = cfg(base());
    c.bot.admin_role = Some("admin".into());

    c.desired.holders.insert("admin".into(), Default::default());
    let p = plan(&c, &live, Options::default());
    assert!(ops(&p).iter().all(|a| !a.contains("holders")), "{:?}", ops(&p));
    assert!(p.warnings.iter().any(|w| w.to_string().contains("gives it to nobody")), "{:?}", p.warnings);

    c.desired.holders.insert("admin".into(), [Snowflake(7)].into());
    let p = plan(&c, &live, Options::default());
    assert_eq!(ops(&p), ["~ edit holders of admin"]);
    assert!(
        p.warnings.iter().any(|w| w.to_string().contains("takes the admin role (admin) from <@8>")),
        "{:?}",
        p.warnings
    );
}

/// An unknown permission name (which `check` refuses) counts as every
/// permission, as in the power checks: never as none, so the plan doesn't
/// call the channel in sync, and apply refuses to send it.
#[test]
fn unknown_permission_names_are_never_nothing() {
    let mut want = snap(&[("general", ChannelKind::Text)], &[("general", 10)]);
    want.lock.roles.insert("mods".into(), Snowflake(5));
    want.categories[0].channels[0].overwrites =
        vec![Overwrite { role: Some("mods".into()), allow: vec!["VIEW_CHANEL".into()], ..Default::default() }];
    let live = snap(&[("general", ChannelKind::Text)], &[("general", 10)]);
    assert_eq!(ops(&plan(&cfg(want), &live, Options::default())), ["~ edit channel general"]);
}
