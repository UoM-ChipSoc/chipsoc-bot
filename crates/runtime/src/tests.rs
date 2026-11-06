//! Handlers end to end, on the fake Discord: interactions in, replies, posts,
//! roles and config commits out.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use chipsoc_config::Config;
use chipsoc_discord::fake::FakeDiscord;
use serde_json::{Value, json};
use twilight_model::application::interaction::Interaction;

use crate::Ctx;
use crate::review::Form;
use crate::ui::{Value as Answer, Values};

const ADA: u64 = 7;
const REVIEWER: u64 = 9;
const BEN: u64 = 8;

/// Run git in `dir`. Under load another git (the bot's, or one git started
/// itself) may still hold a lock for a moment: that's tried again.
fn git(dir: &Path, args: &[&str]) -> String {
    let run = || {
        let quiet = ["-c", "gc.auto=0", "-c", "maintenance.auto=false"];
        Command::new("git").arg("-C").arg(dir).args(quiet).args(args).env("GIT_TERMINAL_PROMPT", "0").output().unwrap()
    };
    let mut out = run();
    for _ in 0..20 {
        let err = String::from_utf8_lossy(&out.stderr);
        if out.status.success() || !(err.contains(".lock") || err.contains("Unable to create")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        out = run();
    }
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn write(dir: &Path, file: &str, body: &str) {
    let p = dir.join(file);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A config with an open position whose holders members.toml lists, applied
/// to the fake, in a checkout of a bare origin (so the bot commits and pushes).
async fn server() -> (tempfile::TempDir, Arc<FakeDiscord>, Ctx) {
    server_with("", "").await
}

/// [`server`], with more in bot.toml (`bot`, before the position) and in the position (`position`).
async fn server_with(bot: &str, position: &str) -> (tempfile::TempDir, Arc<FakeDiscord>, Ctx) {
    let root = tempfile::tempdir().unwrap();
    let (origin, dir) = (root.path().join("origin.git"), root.path().join("config"));
    git(root.path(), &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
    git(root.path(), &["clone", "-q", origin.to_str().unwrap(), dir.to_str().unwrap()]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    write(&dir, "server.toml", "[guild]\nname = \"Test\"\n");
    write(
        &dir,
        "bot.toml",
        &format!(
            "name = \"Test\"\nadmin_role = \"committee\"\n[log]\npeople = \"log\"\n[review]\nchannel = \"review\"\n{bot}\n\
             [[position]]\nrole = \"secretary\"\n{position}"
        ),
    );
    write(
        &dir,
        "roles.toml",
        "[[role]]\nkey = \"bot\"\nname = \"Bot\"\nmanaged = true\n\n[[role]]\nkey = \"committee\"\nname = \"Committee\"\n\n\
         [[role]]\nkey = \"secretary\"\nname = \"Secretary\"\n\n\
         [[role]]\nkey = \"everyone\"\nname = \"@everyone\"\npermissions = [\"VIEW_CHANNEL\"]\n",
    );
    write(
        &dir,
        "categories/10-main.toml",
        "[category]\nkey = \"main\"\nname = \"Main\"\n\n[[channel]]\nkey = \"review\"\nname = \"review\"\n\n\
         [[channel]]\nkey = \"log\"\nname = \"log\"\n\n[[channel]]\nkey = \"events\"\nname = \"events\"\n\n\
         [[channel]]\nkey = \"ideas\"\ntype = \"forum\"\nname = \"ideas\"\n",
    );
    write(&dir, "members.toml", "secretary = []\n");
    let fake = Arc::new(FakeDiscord::new());
    for user in [ADA, BEN, REVIEWER] {
        fake.join(user);
    }
    let mut e = chipsoc_engine::Engine::load(fake.clone(), &dir).unwrap();
    let plan = e.plan(chipsoc_engine::Options::default()).await.unwrap();
    e.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "config"]);
    git(&dir, &["push", "-q", "-u", "origin", "HEAD"]);
    let ctx = Ctx::on_fake(Config::load(&dir).unwrap(), fake.clone(), true).await;
    (root, fake, ctx)
}

/// An interaction from `user` in the server, with `token`; `admin`: they have
/// Administrator. `message` is the one its button is under, if any.
fn interaction(user: u64, token: &str, kind: u8, admin: bool, message: Option<Value>) -> Interaction {
    let mut i = json!({
        "id": "500",
        "application_id": "1",
        "type": kind,
        "token": token,
        "guild_id": "1000",
        "attachment_size_limit": 8_388_608,
        "authorizing_integration_owners": {},
        "entitlements": [],
        "member": {
            "user": { "id": user.to_string(), "username": format!("user{user}"), "discriminator": "0", "avatar": null },
            "roles": [],
            "joined_at": "2024-01-01T00:00:00.000000+00:00",
            "deaf": false,
            "mute": false,
            "flags": 0,
            "permissions": if admin { "8" } else { "0" },
        },
    });
    i["data"] = match kind {
        5 => json!({ "custom_id": "form", "components": [] }),
        _ => json!({ "custom_id": "button", "component_type": 2 }),
    };
    if let Some(m) = message {
        i["message"] = m;
    }
    serde_json::from_value(i).unwrap()
}

/// The review card in the review channel, as an interaction's message.
fn card(fake: &FakeDiscord, review: &str) -> Value {
    let st = fake.state.lock().unwrap();
    let mut m = st.messages.iter().find(|m| m["channel_id"] == review).expect("a review card").clone();
    m["type"] = json!(0);
    m["timestamp"] = json!("2024-01-01T00:00:00.000000+00:00");
    m["author"] = json!({ "id": "42", "username": "bot", "discriminator": "0", "avatar": null, "bot": true });
    for k in ["attachments", "embeds", "mentions", "mention_roles"] {
        m[k] = json!([]);
    }
    for k in ["pinned", "tts", "mention_everyone"] {
        m[k] = json!(false);
    }
    m
}

/// Volunteering for a position, then an admin's Accept on its review card:
/// the card is posted, the applicant thanked, members.toml committed and
/// pushed with them in it, the role given and the applicant told.
#[tokio::test]
async fn a_volunteer_is_accepted_end_to_end() {
    let (root, fake, ctx) = server().await;
    let form = interaction(ADA, "tok-form", 5, false, None);
    crate::review::submit(&ctx, &form, Form::Volunteer("secretary".into()), Values::default()).await;
    let replies = fake.replies_to("tok-form");
    assert!(
        replies.iter().any(|r| r["edit"]["content"].as_str().is_some_and(|t| t.starts_with("Thanks!"))),
        "{replies:?}"
    );

    let review = ctx.cfg().desired.lock.channel("review").unwrap().to_string();
    let posted = card(&fake, &review);
    let text = posted["content"].as_str().unwrap();
    assert!(text.contains("<@7>") && text.contains("Secretary"), "{text}");
    let id = ctx.db().pending_applications().await.unwrap()[0].id;

    let press = interaction(REVIEWER, "tok-review", 3, true, Some(posted));
    crate::apps::review(&ctx, &press, true, id, None).await;

    let role = ctx.cfg().desired.lock.role("secretary").unwrap().to_string();
    assert!(fake.roles_of(ADA).contains(&role), "the role is given");
    let members = std::fs::read_to_string(ctx.cfg().dir.join("members.toml")).unwrap();
    assert!(members.contains("\"7\""), "{members}");
    let pushed = git(&root.path().join("origin.git"), &["log", "-1", "--format=%s"]);
    assert!(pushed.contains("secretary"), "committed and pushed: {pushed}");
    assert!(fake.dms_to(ADA).iter().any(|d| d.contains("accepted")), "{:?}", fake.dms_to(ADA));
    let edits: Vec<String> =
        fake.replies_to("tok-review").iter().filter_map(|r| r["edit"]["content"].as_str().map(str::to_owned)).collect();
    assert!(edits.last().is_some_and(|t| t.contains("<@9>")), "the card says who accepted: {edits:?}");
}

/// A button nothing handles (from an older version, say) is answered, not
/// left for Discord to call a failure.
#[tokio::test]
async fn an_unknown_button_is_answered() {
    let (_root, fake, ctx) = server().await;
    crate::commands::interaction(&ctx, &interaction(ADA, "tok-old", 3, false, None)).await;
    let replies = fake.replies_to("tok-old");
    let said = replies[0]["reply"]["data"]["content"].as_str().unwrap_or_default();
    assert!(said.contains("doesn't know that one"), "{replies:?}");
}

/// A message the bot posted, as an interaction's message.
fn posted(fake: &FakeDiscord, id: &str) -> Value {
    let st = fake.state.lock().unwrap();
    let mut m = st.messages.iter().find(|m| m["id"] == id).expect("the post").clone();
    m["type"] = json!(0);
    m["timestamp"] = json!("2024-01-01T00:00:00.000000+00:00");
    m["author"] = json!({ "id": "42", "username": "bot", "discriminator": "0", "avatar": null, "bot": true });
    for k in ["attachments", "embeds", "mentions", "mention_roles"] {
        m[k] = json!([]);
    }
    for k in ["pinned", "tts", "mention_everyone"] {
        m[k] = json!(false);
    }
    m
}

/// What the bot last said in answer to `token` (a reply, or an edit of it).
fn said(fake: &FakeDiscord, token: &str) -> Value {
    let replies = fake.replies_to(token);
    let last = replies.last().expect("an answer").clone();
    if last.get("edit").is_some() { last["edit"].clone() } else { last["reply"]["data"].clone() }
}

fn buttons(message: &Value) -> Vec<String> {
    let rows = message["components"].as_array().cloned().unwrap_or_default();
    rows.iter()
        .flat_map(|r| r["components"].as_array().cloned().unwrap_or_default())
        .filter_map(|b| b["custom_id"].as_str().map(str::to_owned))
        .collect()
}

fn idea(name: &str) -> Values {
    let mut v = Values::default();
    for (k, t) in [("name", name), ("blurb", "A tiny CPU"), ("scope", "Build it"), ("area", "")] {
        v.set(k, Answer::Text(t.into()));
    }
    v.set("lead", Answer::Picked(vec!["lead".into()]));
    v
}

/// Every member form answers at once, before it does anything slow; a
/// refusal they can fix keeps what they typed, and "Edit and resend" opens
/// the form again filled in. The wait reads in words, and is told before
/// they fill the form in again.
#[tokio::test]
async fn a_refused_form_keeps_what_they_typed_for_edit_and_resend() {
    let (_root, fake, ctx) = server_with("[ideas]\nforum = \"ideas\"\n", "").await;
    let first = interaction(ADA, "tok-1", 5, false, None);
    crate::review::submit(&ctx, &first, Form::Idea, idea("Tiny CPU")).await;
    let replies = fake.replies_to("tok-1");
    assert_eq!(replies[0]["reply"]["type"], 5, "deferred first: {replies:?}");
    assert!(said(&fake, "tok-1")["content"].as_str().unwrap().starts_with("Thanks!"));

    let second = interaction(ADA, "tok-2", 5, false, None);
    crate::review::submit(&ctx, &second, Form::Idea, idea("Second idea")).await;
    let refused = said(&fake, "tok-2");
    assert!(refused["content"].as_str().unwrap().contains("please wait about 5 minutes"), "{refused}");
    assert_eq!(buttons(&refused), ["again:idea"]);

    // Pressing it while they still must wait says so again, before the form.
    let press = interaction(ADA, "tok-3", 3, false, None);
    crate::review::resend(&ctx, &press, "idea").await;
    let wait = said(&fake, "tok-3");
    assert!(wait["content"].as_str().unwrap().contains("please wait"), "{wait}");
    assert_eq!(buttons(&wait), ["again:idea"]);

    // Once they may, the form opens with what they typed.
    let ctx = forget_limits(ctx);
    let press = interaction(ADA, "tok-4", 3, false, None);
    crate::review::resend(&ctx, &press, "idea").await;
    let form = fake.replies_to("tok-4")[0]["reply"].clone();
    assert_eq!(form["type"], 9, "a form: {form}");
    let boxes = form["data"]["components"].to_string();
    assert!(boxes.contains("\"value\":\"Second idea\""), "filled in: {boxes}");
    assert!(boxes.contains("\"default\":true"), "the pick too: {boxes}");
}

/// `[ideas] questions` makes the form: the boxes it asks, and their answers on the card.
#[tokio::test]
async fn the_idea_form_asks_what_bot_toml_says() {
    let ideas =
        "[ideas]\nforum = \"ideas\"\nquestions = [\"name\", { key = \"budget\", ask = \"Budget?\" }, \"lead\"]\n";
    let (_root, fake, ctx) = server_with(ideas, "").await;
    let (_, fields) = crate::ideas::form(&ctx);
    let boxes = serde_json::to_string(&fields).unwrap();
    let ids: Vec<&str> = ["\"name\"", "\"budget\"", "\"lead\"", "\"scope\"", "\"blurb\""]
        .into_iter()
        .filter(|id| boxes.contains(&format!("\"custom_id\":{id}")))
        .collect();
    assert_eq!(ids, ["\"name\"", "\"budget\"", "\"lead\""], "{boxes}");

    let mut v = Values::default();
    v.set("name", Answer::Text("Tiny CPU".into()));
    v.set("budget", Answer::Text("£40".into()));
    v.set("lead", Answer::Picked(vec!["help".into()]));
    crate::review::submit(&ctx, &interaction(ADA, "tok-i", 5, false, None), Form::Idea, v).await;
    let review = ctx.cfg().desired.lock.channel("review").unwrap().to_string();
    let posted = card(&fake, &review);
    let text = posted["content"].as_str().unwrap();
    assert!(text.contains("Project idea · Tiny CPU") && text.contains("Budget?") && text.contains("£40"), "{text}");
}

/// The same server with every rate limit forgotten.
fn forget_limits(mut ctx: Ctx) -> Ctx {
    ctx.limits = Default::default();
    ctx
}

/// A member's `/event` is a suggestion: a card, then on Accept the event,
/// its announcement crediting them, and a DM. An admin's is made at once,
/// and a time that can't be read can be fixed and sent again.
#[tokio::test]
async fn members_suggest_events_and_admins_make_them() {
    let (_root, fake, ctx) = server_with("[events]\nchannel = \"events\"\n", "").await;
    let event = |title: &str, start: &str| {
        let mut v = Values::default();
        for (k, t) in
            [("title", title), ("start", start), ("duration", "90"), ("location", "Room 2.15"), ("description", "")]
        {
            v.set(k, Answer::Text(t.into()));
        }
        v
    };
    let form = interaction(ADA, "tok-ev", 5, false, None);
    crate::review::submit(&ctx, &form, Form::Event, event("Social", "2999-01-01 18:00")).await;
    assert!(said(&fake, "tok-ev")["content"].as_str().unwrap().contains("with the admins"));
    let review = ctx.cfg().desired.lock.channel("review").unwrap().to_string();
    let posted = card(&fake, &review);
    assert!(posted["content"].as_str().unwrap().contains("Event suggestion · Social**"), "{posted}");
    assert!(fake.state.lock().unwrap().events.is_empty(), "not made yet");

    let again = interaction(BEN, "tok-dup", 5, false, None);
    crate::review::submit(&ctx, &again, Form::Event, event("social", "2999-01-01 18:00")).await;
    let dup = said(&fake, "tok-dup");
    assert!(dup["content"].as_str().unwrap().contains("already suggested"), "{dup}");
    assert_eq!(buttons(&dup), ["again:event"]);

    let id = ctx.db().pending_applications().await.unwrap()[0].id;
    let press = interaction(REVIEWER, "tok-acc", 3, true, Some(posted));
    crate::apps::review(&ctx, &press, true, id, None).await;
    let made = fake.state.lock().unwrap().events.clone();
    assert_eq!(made.len(), 1);
    let events = ctx.cfg().desired.lock.channel("events").unwrap().to_string();
    let announced = card(&fake, &events);
    assert!(announced["content"].as_str().unwrap().contains("💡 Suggested by <@7>"), "{announced}");
    assert!(fake.dms_to(ADA).iter().any(|d| d.contains("is on the calendar")), "{:?}", fake.dms_to(ADA));

    let admin = interaction(REVIEWER, "tok-adm", 5, true, None);
    crate::review::submit(&ctx, &admin, Form::Event, event("Talk", "tomorrow 25:00")).await;
    let bad = said(&fake, "tok-adm");
    assert!(bad["content"].as_str().unwrap().contains("Couldn't read the start time"), "{bad}");
    assert_eq!(buttons(&bad), ["again:event"]);
    let admin = interaction(REVIEWER, "tok-adm2", 5, true, None);
    crate::review::submit(&ctx, &admin, Form::Event, event("Talk", "2999-02-01 18:00")).await;
    assert!(said(&fake, "tok-adm2")["content"].as_str().unwrap().starts_with("Put **Talk** on the calendar for <t:"));
    assert_eq!(fake.state.lock().unwrap().events.len(), 2, "made at once");
}

/// A position with a window: each application is a line, then once it
/// closes one card lists them all, decided from its buttons.
#[tokio::test]
async fn a_window_s_applicants_wait_for_one_card() {
    let (_root, fake, ctx) = server_with("", "closes = \"2999-01-01\"\n").await;
    for (user, token) in [(ADA, "tok-a"), (BEN, "tok-b")] {
        let form = interaction(user, token, 5, false, None);
        crate::review::submit(&ctx, &form, Form::Volunteer("secretary".into()), Values::default()).await;
        assert!(said(&fake, token)["content"].as_str().unwrap().contains("Applications close on 1 January 2999"));
    }
    let review = ctx.cfg().desired.lock.channel("review").unwrap().to_string();
    let lines: Vec<Value> =
        fake.state.lock().unwrap().messages.iter().filter(|m| m["channel_id"] == review).cloned().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["content"], "📨 <@8> applied for **Secretary** (2 so far)");
    assert!(buttons(&lines[1]).is_empty());

    let mut cfg = (*ctx.cfg()).clone();
    cfg.bot.positions[0].closes = Some("2020-01-01".into());
    ctx.set_cfg(Arc::new(cfg));
    let late = interaction(ADA, "tok-late", 5, false, None);
    crate::review::submit(&ctx, &late, Form::Volunteer("secretary".into()), Values::default()).await;
    assert!(said(&fake, "tok-late")["content"].as_str().unwrap().contains("closed on 1 January 2020"));

    assert_eq!(chipsoc_admin::rounds::post_closed(&ctx).await.unwrap(), 1);
    let posted = card_last(&fake, &review);
    let ids = buttons(&posted);
    assert_eq!(ids.len(), 5, "{ids:?}");
    let first = ctx.db().pending_applications().await.unwrap()[0].id;
    let press = interaction(REVIEWER, "tok-r", 3, true, Some(posted.clone()));
    crate::apps::review(&ctx, &press, true, first, None).await;
    assert_eq!(fake.replies_to("tok-r")[0]["reply"]["type"], 5, "the reviewer is answered on their own");
    assert!(said(&fake, "tok-r")["content"].as_str().unwrap().contains("Accepted by <@9>"));
    let redrawn = card_last(&fake, &review);
    assert!(redrawn["content"].as_str().unwrap().contains("✅ Accepted by <@9>."), "{redrawn}");
    assert_eq!(buttons(&redrawn).len(), 2, "only Ben's");

    let rest = interaction(REVIEWER, "tok-rest", 5, true, None);
    crate::apps::reject_rest(&ctx, &rest, "secretary", "Thanks, we've picked").await;
    assert!(said(&fake, "tok-rest")["content"].as_str().unwrap().contains("Rejected 1 applicant"));
    assert!(fake.dms_to(BEN).iter().any(|d| d.contains("Thanks, we've picked")));
    assert!(buttons(&card_last(&fake, &review)).is_empty());
}

/// The last message in a channel, as an interaction's message.
fn card_last(fake: &FakeDiscord, channel: &str) -> Value {
    let id = {
        let st = fake.state.lock().unwrap();
        st.messages.iter().rev().find(|m| m["channel_id"] == channel).unwrap()["id"].clone()
    };
    let mut m = card(fake, channel);
    let st = fake.state.lock().unwrap();
    let last = st.messages.iter().find(|x| x["id"] == id).unwrap().clone();
    for k in ["id", "content", "components"] {
        m[k] = last[k].clone();
    }
    m
}

/// The bot log's posts with live Apply buttons: (message id, plan id).
fn plan_posts(fake: &FakeDiscord, log: &str) -> Vec<(String, String)> {
    let st = fake.state.lock().unwrap();
    st.messages
        .iter()
        .filter(|m| m["channel_id"] == log)
        .filter_map(|m| {
            let id = m["components"][0]["components"][0]["custom_id"].as_str()?;
            match crate::commands::CustomId::parse(id)? {
                crate::commands::CustomId::Plan { apply: true, id } => Some((m["id"].as_str()?.to_owned(), id)),
                _ => None,
            }
        })
        .collect()
}

/// Change a channel's topic in the files, so there's something to apply.
fn retopic(dir: &Path, topic: &str) {
    write(
        dir,
        "categories/10-main.toml",
        &format!(
            "[category]\nkey = \"main\"\nname = \"Main\"\n\n[[channel]]\nkey = \"review\"\nname = \"review\"\ntopic = \"{topic}\"\n\n\
             [[channel]]\nkey = \"log\"\nname = \"log\"\n"
        ),
    );
}

/// A newer plan closes the older one's buttons; Apply says so at once,
/// then gives the result; other plans shown say they were applied.
#[tokio::test]
async fn only_the_newest_plan_has_live_buttons_and_apply_answers_at_once() {
    let (_root, fake, ctx) = server().await;
    let dir = ctx.cfg().dir.clone();
    let log = ctx.cfg().desired.lock.channel("log").unwrap().to_string();
    retopic(&dir, "one");
    assert!(crate::control::announce(&ctx, "📥 **Config changed**").await);
    let first = plan_posts(&fake, &log);
    assert_eq!(first.len(), 1, "posted with buttons");
    retopic(&dir, "two");
    assert!(crate::control::announce(&ctx, "📥 **Config changed**").await);
    let open = plan_posts(&fake, &log);
    assert_eq!(open.len(), 1, "the older one's buttons went: {open:?}");
    let (newest, plan) = open[0].clone();
    assert_ne!(newest, first[0].0);
    let old = posted(&fake, &first[0].0);
    assert_eq!(
        old["content"],
        format!("↪️ Replaced by a newer plan ↓ https://discord.com/channels/1000/{log}/{newest}"),
        "{old}"
    );

    // An admin's own /server plan reply, shown before the apply.
    let asked = interaction(REVIEWER, "tok-plan", 3, true, None);
    crate::control::server(&ctx, &asked, crate::manage::Basic::Plan).await;
    assert!(fake.replies_to("tok-plan").last().unwrap()["edit"]["components"][0].is_object(), "with buttons");

    let press = interaction(REVIEWER, "tok-apply", 3, true, Some(posted(&fake, &newest)));
    crate::control::plan_button(&ctx, &press, true, &plan).await;
    let replies = fake.replies_to("tok-apply");
    let at_once = &replies[0]["reply"];
    assert_eq!(at_once["type"], 7, "the post itself is updated straight away: {at_once}");
    assert!(at_once["data"]["content"].as_str().unwrap().ends_with("⏳ Applying… (by <@9>)"), "{at_once}");
    assert_eq!(at_once["data"]["components"][0]["components"][0]["disabled"], true, "greyed out meanwhile");
    let done = &replies.last().unwrap()["edit"];
    assert!(done["content"].as_str().unwrap().contains("Applied by <@9>"), "{done}");
    assert_eq!(done["components"], json!([]), "no buttons once used");
    let reply = fake.replies_to("tok-plan").last().unwrap()["edit"].clone();
    assert_eq!(reply["content"], "✅ Applied with a later plan by <@9>.", "{reply}");
    assert_eq!(reply["components"], json!([]));
    assert!(ctx.db().plan_posts().await.unwrap().is_empty(), "nothing left open");
}

#[tokio::test]
async fn dismiss_says_who_and_closes_the_plan() {
    let (_root, fake, ctx) = server().await;
    let log = ctx.cfg().desired.lock.channel("log").unwrap().to_string();
    retopic(&ctx.cfg().dir.clone(), "one");
    assert!(crate::control::announce(&ctx, "📥 **Config changed**").await);
    let (post, plan) = plan_posts(&fake, &log)[0].clone();
    let press = interaction(REVIEWER, "tok-dismiss", 3, true, Some(posted(&fake, &post)));
    crate::control::plan_button(&ctx, &press, false, &plan).await;
    let r = &fake.replies_to("tok-dismiss")[0]["reply"]["data"];
    assert!(r["content"].as_str().unwrap().ends_with("Dismissed by <@9>; nothing applied."), "{r}");
    assert_eq!(r["components"], json!([]));
    assert!(ctx.db().plan_posts().await.unwrap().is_empty());
    assert!(ctx.plans.take(&plan).is_none(), "its plan is gone");
}

/// With one position open, `/volunteer` opens its form: nothing to choose.
#[tokio::test]
async fn one_open_position_skips_the_menu() {
    let (_root, fake, ctx) = server().await;
    let press = interaction(ADA, "tok-vol", 3, false, None);
    crate::apps::volunteer(&ctx, &press, None).await;
    assert_eq!(fake.replies_to("tok-vol")[0]["reply"]["type"], 9, "the form, not a menu");
}

/// A submission whose card can't be posted is said in the bot log, with
/// its number, so it isn't left unseen; the card head carries the number.
#[tokio::test]
async fn a_card_that_cant_be_posted_is_said_in_the_bot_log() {
    let (_root, fake, ctx) = server().await;
    let review = ctx.cfg().desired.lock.channel("review").unwrap().to_string();
    let log = ctx.cfg().desired.lock.channel("log").unwrap().to_string();
    fake.fail(&format!("POST CHANNELS/{review}/MESSAGES"), 500, false);
    let form = interaction(ADA, "tok-form", 5, false, None);
    crate::review::submit(&ctx, &form, Form::Volunteer("secretary".into()), Values::default()).await;
    let said: Vec<String> = {
        let st = fake.state.lock().unwrap();
        st.messages
            .iter()
            .filter(|m| m["channel_id"] == log)
            .filter_map(|m| m["content"].as_str().map(str::to_owned))
            .collect()
    };
    assert!(
        said.iter().any(|t| t.contains("couldn't be posted for review") && t.contains("/server queue list")),
        "{said:?}"
    );

    let form = interaction(REVIEWER, "tok-form2", 5, false, None);
    crate::review::submit(&ctx, &form, Form::Volunteer("secretary".into()), Values::default()).await;
    let card = card(&fake, &review);
    assert!(card["content"].as_str().unwrap().starts_with("**#2 · Position · Secretary**"), "{card}");
}

mod handlers;
mod security;
