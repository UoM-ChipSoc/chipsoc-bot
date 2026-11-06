//! Regression tests for red-team findings (round 3), on the fake Discord.

use super::*;

fn button_with(user: u64, token: &str, custom_id: &str, values: &[&str]) -> Interaction {
    let mut i = serde_json::to_value(interaction(user, token, 3, false, None)).unwrap();
    i["data"] =
        json!({ "custom_id": custom_id, "component_type": if values.is_empty() { 2 } else { 3 }, "values": values });
    serde_json::from_value(i).unwrap()
}

/// What the people log (here also the bot log) has been sent.
fn logged(fake: &FakeDiscord, ctx: &Ctx) -> String {
    let log = ctx.cfg().desired.lock.channel("log").unwrap().to_string();
    let st = fake.state.lock().unwrap();
    st.messages.iter().filter(|m| m["channel_id"] == log).filter_map(|m| m["content"].as_str()).collect()
}

/// A join flow whose every finisher gets `access`, as a pusher might write it.
fn join_giving(access: &str) -> String {
    format!(
        "access_role = \"{access}\"\n[[step]]\nkey = \"who\"\nquestion = \"Student?\"\n[[step.option]]\nlabel = \"Yes\"\n"
    )
}

/// Through the join flow as ADA, to the end.
async fn finish_join(ctx: &Ctx) {
    crate::join::start(ctx, &button_with(ADA, "t1", "do:join", &[]), None).await;
    crate::join::component(ctx, &button_with(ADA, "t2", "join:pick:who", &["yes"]), "pick:who", &["yes".into()]).await;
    crate::join::component(ctx, &button_with(ADA, "t3", "join:ok", &[]), "ok", &[]).await;
}

/// R3-H1: a pushed join.toml handing out the admin role isn't loaded: the
/// bot runs the same checks as `check` and apply, keeps the old config, and
/// says why in the bot log. Nor does drift watch pick it up.
#[tokio::test]
async fn a_pushed_join_flow_giving_the_admin_role_is_refused() {
    let (_root, fake, ctx) = server().await;
    let dir = ctx.cfg().dir.clone();
    assert!(crate::reload::reload(&ctx, &dir).await, "the first run approves what's there");
    write(&dir, "join.toml", &join_giving("committee"));
    assert!(!crate::reload::reload(&ctx, &dir).await, "refused");
    assert!(ctx.cfg().join.is_none(), "the old config is kept");
    let log = logged(&fake, &ctx);
    assert!(log.contains("The config has 1 problem") && log.contains("is the admin role"), "{log}");
    let pushed = Arc::new(Config::load(&dir).unwrap());
    assert!(!crate::reload::use_config(&ctx, pushed).await, "drift watch's reload refuses it too");
    assert!(ctx.cfg().join.is_none());
    assert_eq!(crate::drift::check(&ctx).await.unwrap(), None, "and drift watch doesn't plan from it");

    finish_join(&ctx).await;
    let committee = ctx.cfg().desired.lock.role("committee").unwrap().to_string();
    assert!(!fake.roles_of(ADA).contains(&committee));
}

/// R3-H1: and if such a flow were in use anyway, confirming it never gives
/// a role admins decide on: the admin role, a position's, or one with
/// moderator permissions in Discord now.
#[tokio::test]
async fn the_join_flow_never_gives_roles_admins_decide_on() {
    for access in ["committee", "secretary"] {
        let (_root, fake, ctx) = server().await;
        let dir = ctx.cfg().dir.clone();
        write(&dir, "join.toml", &join_giving(access));
        ctx.set_cfg(Arc::new(Config::load(&dir).unwrap()));
        finish_join(&ctx).await;
        let role = ctx.cfg().desired.lock.role(access).unwrap().to_string();
        assert!(!fake.roles_of(ADA).contains(&role), "{access}: {:?}", fake.roles_of(ADA));
        let said = fake.replies_to("t3").iter().map(Value::to_string).collect::<String>();
        assert!(said.contains("a role admins decide on"), "{access}: {said}");
    }

    // A plain role made powerful in Discord since (here by a channel overwrite).
    let (_root, fake, ctx) = server().await;
    let dir = ctx.cfg().dir.clone();
    let roles = std::fs::read_to_string(dir.join("roles.toml")).unwrap();
    write(&dir, "roles.toml", &format!("{roles}\n[[role]]\nkey = \"member\"\nname = \"Member\"\n"));
    let mut e = chipsoc_engine::Engine::load(fake.clone(), &dir).unwrap();
    let plan = e.plan(chipsoc_engine::Options::default()).await.unwrap();
    e.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
    write(&dir, "join.toml", &join_giving("member"));
    ctx.set_cfg(Arc::new(Config::load(&dir).unwrap()));
    let member = ctx.cfg().desired.lock.role("member").unwrap().to_string();
    fake.edit_channel("log", |c| {
        c["permission_overwrites"] = json!([{ "id": member, "type": 0, "allow": "4", "deny": "0" }]);
    });
    finish_join(&ctx).await;
    assert!(!fake.roles_of(ADA).contains(&member), "BAN_MEMBERS by an overwrite");
    assert!(said(&fake, "t3").contains("a role admins decide on"), "{}", said(&fake, "t3"));
}

/// The resources forum with `n` posts, each a different link.
async fn resources_forum(fake: &Arc<FakeDiscord>, ctx: &Ctx, n: usize) {
    let dir = ctx.cfg().dir.clone();
    let bot = std::fs::read_to_string(dir.join("bot.toml")).unwrap();
    write(&dir, "bot.toml", &format!("{bot}[resources]\nforum = \"res\"\n"));
    let cat = std::fs::read_to_string(dir.join("categories/10-main.toml")).unwrap();
    write(
        &dir,
        "categories/10-main.toml",
        &format!("{cat}\n[[channel]]\nkey = \"res\"\nname = \"res\"\ntype = \"forum\"\n"),
    );
    let mut e = chipsoc_engine::Engine::load(fake.clone(), &dir).unwrap();
    let plan = e.plan(chipsoc_engine::Options::default()).await.unwrap();
    e.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
    assert!(crate::reload::reload(ctx, &dir).await);
    let forum = ctx.cfg().desired.lock.channel("res").unwrap();
    use chipsoc_discord::Rest as _;
    for n in 0..n {
        let body = json!({ "name": format!("r{n}"), "applied_tags": [], "message": { "content": format!("https://ex.org/{n}") } });
        fake.call(chipsoc_discord::Method::Post, &format!("channels/{forum}/threads"), Some(body)).await.unwrap();
    }
}

fn form(fields: &[(&str, &str)]) -> Values {
    let mut v = Values::default();
    for (k, t) in fields {
        v.set(k, Answer::Text((*t).into()));
    }
    v
}

fn said(fake: &FakeDiscord, token: &str) -> String {
    fake.replies_to(token).iter().map(Value::to_string).collect()
}

/// R3-M1: a refused `/share-resource` that read the forum still uses the
/// member's turn, and members' submits share one read of the forum for a few
/// minutes. A slip caught before any reading is free to fix.
#[tokio::test]
async fn refused_resource_submits_are_limited_and_cheap() {
    let (_root, fake, ctx) = server().await;
    resources_forum(&fake, &ctx, 60).await;
    let dup = form(&[("title", "x"), ("url", "https://ex.org/0"), ("about", "dup")]);

    let before = fake.requests();
    crate::review::submit(&ctx, &interaction(ADA, "res0", 5, false, None), Form::Resource, dup.clone()).await;
    assert!(said(&fake, "res0").contains("already been shared"), "{}", said(&fake, "res0"));
    let first = fake.requests() - before;
    assert!(first > 60, "the first read is the forum's posts: {first}");
    for n in 1..20 {
        let tok = format!("res{n}");
        crate::review::submit(&ctx, &interaction(ADA, &tok, 5, false, None), Form::Resource, dup.clone()).await;
        assert!(said(&fake, &tok).contains("please wait"), "{}", said(&fake, &tok));
    }
    let before = fake.requests();
    crate::review::submit(&ctx, &interaction(REVIEWER, "other", 5, false, None), Form::Resource, dup.clone()).await;
    assert!(said(&fake, "other").contains("already been shared"));
    let cached = fake.requests() - before;
    assert!(cached < 10, "someone else's submit reuses the read: {cached}");

    // A typo (not https) refused before reading anything gives the turn back.
    let typo = form(&[("title", "y"), ("url", "http://ex.org/new"), ("about", "new")]);
    crate::review::submit(&ctx, &interaction(ADA + 100, "typo", 5, false, None), Form::Resource, typo.clone()).await;
    assert!(said(&fake, "typo").contains("https://"), "{}", said(&fake, "typo"));
    let fixed = form(&[("title", "y"), ("url", "https://ex.org/new"), ("about", "new")]);
    crate::review::submit(&ctx, &interaction(ADA + 100, "fixed", 5, false, None), Form::Resource, fixed.clone()).await;
    assert!(said(&fake, "fixed").contains("Thanks!"), "{}", said(&fake, "fixed"));
}

/// R3-M1: a suggested feed is fetched only on a turn that counts, so an
/// unreadable one can't be sent again at once; one refused before fetching
/// (not a public web address) can.
#[tokio::test]
async fn refused_feed_suggestions_use_the_turn_once_fetched() {
    let (_root, fake, ctx) = server().await;
    let dir = ctx.cfg().dir.clone();
    let bot = std::fs::read_to_string(dir.join("bot.toml")).unwrap();
    write(&dir, "bot.toml", &format!("{bot}[feeds]\nchannel = \"log\"\n"));
    ctx.set_cfg(Arc::new(Config::load(&dir).unwrap()));
    for (n, url) in ["not a link", "https://10.0.0.1/feed"].iter().enumerate() {
        let tok = format!("cheap{n}");
        crate::review::submit(&ctx, &interaction(ADA, &tok, 5, false, None), Form::Feed, form(&[("url", url)])).await;
        assert!(said(&fake, &tok).contains("Couldn't read that feed"), "{}", said(&fake, &tok));
    }
    // `.invalid` never resolves: fetched (tried), refused, and counted.
    let gone = form(&[("url", "https://feed.invalid/rss")]);
    crate::review::submit(&ctx, &interaction(ADA, "fetched", 5, false, None), Form::Feed, gone.clone()).await;
    assert!(said(&fake, "fetched").contains("Couldn't read that feed"), "{}", said(&fake, "fetched"));
    crate::review::submit(&ctx, &interaction(ADA, "again", 5, false, None), Form::Feed, gone.clone()).await;
    assert!(said(&fake, "again").contains("please wait"), "{}", said(&fake, "again"));
}

/// Objects in `v` that say something (`content`, `embeds`) with no
/// `allowed_mentions` on them or around them.
fn may_ping(v: &Value, covered: bool, path: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(o) => {
            let covered = covered || o.get("allowed_mentions").is_some_and(|a| a["parse"] == json!([]));
            if !covered && (o.get("content").is_some_and(Value::is_string) || o.contains_key("embeds")) {
                out.push(path.to_owned());
            }
            for (k, v) in o {
                may_ping(v, covered, &format!("{path}.{k}"), out);
            }
        }
        Value::Array(a) => {
            for (n, v) in a.iter().enumerate() {
                may_ping(v, covered, &format!("{path}[{n}]"), out);
            }
        }
        _ => {}
    }
}

/// The id of the waiting application of `kind`.
async fn waiting(ctx: &Ctx, kind: chipsoc_store::AppKind) -> i64 {
    let apps = ctx.db().pending_applications().await.unwrap();
    apps.iter().find(|a| a.kind == kind).expect("one waiting").id
}

/// Code review H3: whatever a flow sends through the `Rest` seam pings
/// nobody unless it says whom: every message body, reply, follow-up, DM and
/// forum post carries `allowed_mentions` with no `parse`.
#[tokio::test]
async fn every_outgoing_body_pings_nobody() {
    let (_root, fake, ctx) = server().await;
    resources_forum(&fake, &ctx, 2).await;
    let hostile = "@everyone <@&1> **hi**";
    let answers = form(&[("q0", hostile)]);
    let vol = Form::Volunteer("secretary".into());
    crate::review::submit(&ctx, &interaction(ADA, "vol", 5, false, None), vol.clone(), answers).await;
    let review = ctx.cfg().desired.lock.channel("review").unwrap().to_string();
    let id = ctx.db().pending_applications().await.unwrap()[0].id;
    crate::apps::review(&ctx, &interaction(REVIEWER, "acc", 3, true, Some(card(&fake, &review))), true, id, None).await;
    let share = form(&[("title", hostile), ("url", "https://ex.org/new"), ("about", hostile)]);
    crate::review::submit(&ctx, &interaction(ADA, "res", 5, false, None), Form::Resource, share.clone()).await;
    write(&ctx.cfg().dir, "join.toml", &join_giving("secretary").replace("access_role = \"secretary\"\n", ""));
    ctx.set_cfg(Arc::new(Config::load(&ctx.cfg().dir).unwrap()));
    finish_join(&ctx).await;
    crate::commands::interaction(&ctx, &interaction(ADA, "old", 3, false, None)).await;

    // A member's event suggestion, accepted.
    let mut cfg = (*ctx.cfg()).clone();
    cfg.bot.events = Some(chipsoc_config::bot::Events { channel: "events".into(), ..Default::default() });
    ctx.set_cfg(Arc::new(cfg));
    let event =
        form(&[("title", hostile), ("start", "2999-01-01 18:00"), ("location", hostile), ("description", hostile)]);
    crate::review::submit(&ctx, &interaction(BEN, "ev", 5, false, None), Form::Event, event).await;
    let id = waiting(&ctx, chipsoc_store::AppKind::Event).await;
    let press = interaction(REVIEWER, "ev-acc", 3, true, Some(super::card_last(&fake, &review)));
    crate::apps::review(&ctx, &press, true, id, None).await;
    assert_eq!(fake.state.lock().unwrap().events.len(), 1, "made");

    // A window: a line per application, then the round's card, decided from it.
    let mut cfg = (*ctx.cfg()).clone();
    cfg.bot.positions[0].closes = Some("2999-01-01".into());
    ctx.set_cfg(Arc::new(cfg));
    crate::review::submit(&ctx, &interaction(BEN, "win", 5, false, None), vol, form(&[("q0", hostile)])).await;
    let mut cfg = (*ctx.cfg()).clone();
    cfg.bot.positions[0].closes = Some("2020-01-01".into());
    ctx.set_cfg(Arc::new(cfg));
    assert_eq!(chipsoc_admin::rounds::post_closed(&ctx).await.unwrap(), 1);
    let id = waiting(&ctx, chipsoc_store::AppKind::Position).await;
    let press = interaction(REVIEWER, "win-acc", 3, true, Some(super::card_last(&fake, &review)));
    crate::apps::review(&ctx, &press, false, id, None).await;

    let st = fake.state.lock().unwrap();
    assert!(st.bodies.len() > 10, "the flows sent things: {}", st.bodies.len());
    let mut bad = Vec::new();
    for (call, body) in &st.bodies {
        may_ping(body, false, call, &mut bad);
    }
    assert!(bad.is_empty(), "these could ping: {bad:?}");
    for m in &st.messages {
        assert_eq!(m["allowed_mentions"], json!({ "parse": [] }), "{m}");
    }
    let says =
        st.replies.iter().filter(|r| r["edit"]["content"].is_string() || r["reply"]["data"]["content"].is_string());
    for r in says {
        let am =
            if r["edit"].is_null() { &r["reply"]["data"]["allowed_mentions"] } else { &r["edit"]["allowed_mentions"] };
        assert_eq!(*am, json!({ "parse": [] }), "{r}");
    }
}
