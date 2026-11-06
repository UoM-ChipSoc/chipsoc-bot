//! Handlers that once went through twilight's own client, now on the fake:
//! the join flow from a DM, profiles, `/solved`, reminders, command
//! registration, and how DM presses find their server.

use std::collections::HashMap;
use std::time::Duration;

use super::*;
use crate::now;
use twilight_gateway::Event;
use twilight_model::gateway::payload::incoming::{InteractionCreate, MemberAdd, MemberRemove, MemberUpdate};
use twilight_model::id::Id;

/// A press in a DM by `user`: no server of its own, only its id's.
fn dm_press(user: u64, token: &str, custom_id: &str, values: &[&str]) -> Interaction {
    serde_json::from_value(json!({
        "id": "501",
        "application_id": "1",
        "type": 3,
        "token": token,
        "attachment_size_limit": 8_388_608,
        "authorizing_integration_owners": {},
        "entitlements": [],
        "user": { "id": user.to_string(), "username": format!("user{user}"), "discriminator": "0", "avatar": null },
        "data": { "custom_id": custom_id, "component_type": if values.is_empty() { 2 } else { 3 }, "values": values },
    }))
    .unwrap()
}

/// Every button and menu id in what the bot last said to `token`.
fn ids_in(fake: &FakeDiscord, token: &str) -> Vec<String> {
    let said = said(fake, token);
    let rows = said["components"].as_array().cloned().unwrap_or_default();
    rows.iter()
        .flat_map(|r| r["components"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["custom_id"].as_str().map(str::to_owned))
        .collect()
}

/// Rewrite roles.toml with `extra` roles, apply it, and use the config with `join` as join.toml.
async fn with_join(fake: &Arc<FakeDiscord>, ctx: &Ctx, extra: &str, join: &str) {
    let dir = ctx.cfg().dir.clone();
    let roles = std::fs::read_to_string(dir.join("roles.toml")).unwrap();
    write(&dir, "roles.toml", &format!("{roles}\n{extra}"));
    let mut e = chipsoc_engine::Engine::load(fake.clone(), &dir).unwrap();
    let plan = e.plan(chipsoc_engine::Options::default()).await.unwrap();
    e.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
    write(&dir, "join.toml", join);
    ctx.set_cfg(Arc::new(Config::load(&dir).unwrap()));
}

const FLOW: &str = "access_role = \"member\"\n\
    [[step]]\nkey = \"who\"\nquestion = \"Student?\"\n[[step.option]]\nlabel = \"Yes\"\nnext = \"name\"\n\
    [[step]]\nkey = \"name\"\nquestion = \"Your name?\"\n[[step.field]]\nkey = \"name\"\nlabel = \"Name\"\n";

/// From the DM's Get started to Confirm: every button carries the server,
/// the access role is given and the nickname set.
#[tokio::test]
async fn the_join_flow_runs_from_a_dm_to_the_end() {
    let (_root, fake, ctx) = server().await;
    with_join(&fake, &ctx, "[[role]]\nkey = \"member\"\nname = \"Member\"\n", FLOW).await;

    // The button the reminder and notify DMs carry.
    let start = chipsoc_admin::join::start_button(chipsoc_config::Snowflake(1000), "Go");
    let start = start[0]["components"][0]["custom_id"].as_str().unwrap().to_owned();
    crate::commands::interaction(&ctx, &dm_press(ADA, "d1", &start, &[])).await;
    let ids = ids_in(&fake, "d1");
    assert!(!ids.is_empty() && ids.iter().all(|id| id.ends_with("@1000")), "{ids:?}");

    crate::commands::interaction(&ctx, &dm_press(ADA, "d2", "join:pick:who@1000", &["yes"])).await;
    assert!(said(&fake, "d2")["content"].as_str().unwrap().contains("Your name?"), "{}", said(&fake, "d2"));
    crate::commands::interaction(&ctx, &dm_press(ADA, "d3", "join:fill:name@1000", &[])).await;
    let form = fake.replies_to("d3")[0]["reply"].clone();
    assert_eq!((form["type"].as_u64(), form["data"]["custom_id"].as_str()), (Some(9), Some("join:form:name@1000")));
    let typed = HashMap::from([("name".to_owned(), "Ada".to_owned())]);
    crate::join::modal(&ctx, &dm_press(ADA, "d4", "join:form:name@1000", &[]), "form:name", &typed).await;
    assert!(ids_in(&fake, "d4").contains(&"join:ok@1000".to_owned()), "the summary: {}", said(&fake, "d4"));

    crate::commands::interaction(&ctx, &dm_press(ADA, "d5", "join:ok@1000", &[])).await;
    let done = said(&fake, "d5");
    assert!(done["content"].as_str().unwrap().starts_with("✅ All done: you're **Ada** here"), "{done}");
    let member = ctx.cfg().desired.lock.role("member").unwrap().to_string();
    assert!(fake.roles_of(ADA).contains(&member));
    let nick = fake.state.lock().unwrap().members.iter().find(|m| m["user"]["id"] == "7").unwrap()["nick"].clone();
    assert_eq!(nick, "Ada");
}

/// A DM press says which server it's for, and that's checked: someone not
/// in it is refused, and routing finds the server from the id.
#[tokio::test]
async fn dm_presses_find_their_server_and_strangers_are_refused() {
    let (_root, fake, ctx) = server().await;
    crate::commands::interaction(&ctx, &dm_press(77, "s1", "do:profile@1000", &[])).await;
    assert!(said(&fake, "s1")["content"].as_str().unwrap().contains("only for members of that server"));

    let ctx = Arc::new(ctx);
    let guilds = HashMap::from([(Id::new(1000), ctx.clone())]);
    crate::route(&guilds, Event::InteractionCreate(Box::new(InteractionCreate(dm_press(ADA, "s2", "x:y@1000", &[])))));
    crate::route(&guilds, Event::InteractionCreate(Box::new(InteractionCreate(dm_press(ADA, "s3", "x:y@999", &[])))));
    let answered = async {
        while fake.replies_to("s2").is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), answered).await.expect("routed to its server");
    assert!(said(&fake, "s2")["content"].as_str().unwrap().contains("doesn't know that one"));
    assert!(fake.replies_to("s3").is_empty(), "a server the bot doesn't run here: nobody answers");
}

/// After onboarding, a DM with the profile button; the form sets a nickname
/// told apart from someone else's.
#[tokio::test]
async fn profiles_are_offered_by_dm_and_set_the_nickname() {
    let (_root, fake, ctx) = server_with("[profile]\n", "").await;
    let update: MemberUpdate = serde_json::from_value(json!({
        "guild_id": "1000", "roles": [], "joined_at": "2024-01-01T00:00:00.000000+00:00", "pending": false,
        "flags": 2, "deaf": false, "mute": false, "nick": null, "avatar": null, "premium_since": null,
        "communication_disabled_until": null,
        "user": { "id": ADA.to_string(), "username": "ada", "discriminator": "0", "avatar": null },
    }))
    .unwrap();
    crate::profile::on_member_update(&ctx, &update).await;
    assert_eq!(fake.dms_to(ADA).len(), 1, "one DM");
    let button = {
        let st = fake.state.lock().unwrap();
        st.messages.last().unwrap()["components"][0]["components"][0]["custom_id"].clone()
    };
    assert_eq!(button, "do:profile@1000");
    crate::profile::on_member_update(&ctx, &update).await;
    assert_eq!(fake.dms_to(ADA).len(), 1, "only the first time");

    fake.state.lock().unwrap().members.iter_mut().find(|m| m["user"]["id"] == "8").unwrap()["nick"] = json!("Ada");
    let typed = HashMap::from([("name".to_owned(), "Ada".to_owned()), ("surname".to_owned(), "Lovelace".to_owned())]);
    crate::profile::submit(&ctx, &interaction(ADA, "p1", 5, false, None), &typed).await;
    assert!(said(&fake, "p1")["content"].as_str().unwrap().starts_with("Done: you're **Ada L.** here"));
    let nick = fake.state.lock().unwrap().members.iter().find(|m| m["user"]["id"] == "7").unwrap()["nick"].clone();
    assert_eq!(nick, "Ada L.");
}

/// `/solved` in a help post: tagged, said in the post (pinging nobody), then closed.
#[tokio::test]
async fn solved_tags_and_closes_the_post() {
    let (_root, fake, ctx) = server_with("[help]\nforum = \"ideas\"\n", "").await;
    let forum = ctx.cfg().desired.lock.channel("ideas").unwrap().to_string();
    fake.edit_channel("ideas", |c| c["available_tags"] = json!([{ "id": "77", "name": "Solved" }]));
    use chipsoc_discord::Rest as _;
    let body = json!({ "name": "Help", "applied_tags": [], "message": { "content": "How?" } });
    let thread =
        fake.call(chipsoc_discord::Method::Post, &format!("channels/{forum}/threads"), Some(body)).await.unwrap();
    let tid = thread["id"].as_str().unwrap().to_owned();
    let mut i = serde_json::to_value(interaction(ADA, "h1", 3, false, None)).unwrap();
    i["type"] = json!(2);
    i["data"] = json!({ "id": "1", "name": "solved", "type": 1 });
    i["channel"] =
        json!({ "id": tid, "type": 11, "parent_id": forum, "owner_id": ADA.to_string(), "applied_tags": [] });
    crate::help::solved(&ctx, &serde_json::from_value(i).unwrap()).await;

    let reply = fake.replies_to("h1")[0]["reply"].clone();
    assert!(reply["data"]["content"].as_str().unwrap().starts_with("✅ Marked solved by <@7>"), "{reply}");
    assert_eq!(reply["data"]["allowed_mentions"], json!({ "parse": [] }));
    let st = fake.state.lock().unwrap();
    let t = st.channels.iter().find(|c| c["id"] == tid.as_str()).unwrap();
    assert_eq!((t["applied_tags"].clone(), t["archived"].clone()), (json!(["77"]), json!(true)));
}

/// Reminders go out for events still on, and are dropped for cancelled ones.
#[tokio::test]
async fn reminders_skip_cancelled_events() {
    let (_root, fake, ctx) = server().await;
    let events = ctx.cfg().desired.lock.channel("events").unwrap().to_string();
    for (id, status) in [("300", 1), ("301", 4)] {
        fake.state.lock().unwrap().events.push(json!({ "id": id, "status": status }));
    }
    for (event, text) in [("300", "Starting soon"), ("301", "Cancelled one"), ("302", "Deleted one")] {
        ctx.db().add_reminder(&events, now() - 1, text, Some(event)).await.unwrap();
    }
    crate::events::remind(&ctx).await;
    let posted: Vec<String> = {
        let st = fake.state.lock().unwrap();
        st.messages
            .iter()
            .filter(|m| m["channel_id"] == events)
            .filter_map(|m| m["content"].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(posted, ["Starting soon"]);
    assert!(ctx.db().due_reminders(now()).await.unwrap().is_empty(), "none left");
}

/// Slash commands are set for the server, as JSON Discord takes.
#[tokio::test]
async fn commands_are_registered() {
    let (_root, fake, ctx) = server().await;
    crate::commands::register(&ctx).await;
    let names: Vec<String> = {
        let st = fake.state.lock().unwrap();
        st.commands.as_array().unwrap().iter().filter_map(|c| c["name"].as_str().map(str::to_owned)).collect()
    };
    assert!(names.contains(&"volunteer".to_owned()) && names.contains(&"server".to_owned()), "{names:?}");
}

/// The member list kept follows the gateway's member events.
#[tokio::test]
async fn the_member_list_follows_gateway_events() {
    let (_root, fake, mut ctx) = server().await;
    ctx.rest.members = Arc::new(crate::members::Members::new(crate::members::FRESH));
    assert_eq!(ctx.members().await.unwrap().len(), 3);
    fake.join(77);
    let user =
        |id: u64| json!({ "id": id.to_string(), "username": format!("u{id}"), "discriminator": "0", "avatar": null });
    let member = |id: u64| {
        json!({ "guild_id": "1000", "user": user(id), "roles": [], "joined_at": "2024-01-01T00:00:00.000000+00:00",
                "deaf": false, "mute": false, "flags": 0 })
    };
    let added: MemberAdd = serde_json::from_value(member(77)).unwrap();
    crate::seen_member(&ctx, &Event::MemberAdd(Box::new(added)));
    let removed: MemberRemove = serde_json::from_value(json!({ "guild_id": "1000", "user": user(BEN) })).unwrap();
    crate::seen_member(&ctx, &Event::MemberRemove(removed));
    let mut changed = member(ADA);
    changed["nick"] = json!("Ada");
    changed["pending"] = json!(false);
    let changed: MemberUpdate = serde_json::from_value(changed).unwrap();
    crate::seen_member(&ctx, &Event::MemberUpdate(Box::new(changed)));
    let list = ctx.members().await.unwrap();
    let ids: Vec<u64> = list.iter().filter_map(crate::members::user).collect();
    assert_eq!(ids, [ADA, REVIEWER, 77], "without reading Discord again (which still has Ben)");
    assert_eq!(crate::members::shown(&list[0]), "Ada");
    assert_eq!(crate::members::joined(&list[0]), 1_704_067_200);
}

/// After a restart no plan's buttons work, so the old posts lose them.
#[tokio::test]
async fn plan_posts_from_before_a_restart_lose_their_buttons() {
    use chipsoc_discord::Rest as _;
    let (_root, fake, ctx) = server_with("", "").await;
    let log = ctx.bot_log().unwrap().to_string();
    let buttons = json!([{ "type": 1, "components": [
        { "type": 2, "style": 3, "label": "Apply", "custom_id": "plan:apply:abc" },
        { "type": 2, "style": 2, "label": "Dismiss", "custom_id": "plan:dismiss:abc" }
    ] }]);
    let path = format!("channels/{log}/messages");
    let plan =
        fake.call(chipsoc_discord::Method::Post, &path, Some(json!({ "content": "3 changes", "components": buttons })));
    let plan = plan.await.unwrap()["id"].as_str().unwrap().to_owned();
    let other = json!([{ "type": 1, "components": [{ "type": 2, "style": 1, "label": "Approve", "custom_id": "trust:ok:1" }] }]);
    fake.call(chipsoc_discord::Method::Post, &path, Some(json!({ "content": "approve?", "components": other })))
        .await
        .unwrap();

    crate::control::close_stale_posts(&ctx).await;
    let st = fake.state.lock().unwrap();
    let m = |id: &str| st.messages.iter().find(|m| m["id"] == id).unwrap().clone();
    let closed = m(&plan);
    assert_eq!(closed["components"], json!([]));
    assert!(closed["content"].as_str().unwrap().contains("ended when the bot restarted"), "{closed}");
    let kept = st.messages.iter().find(|m| m["content"] == "approve?").unwrap();
    assert_ne!(kept["components"], json!([]), "other buttons stay");
}
