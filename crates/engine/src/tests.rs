//! Convergence: after `apply`, `plan` must be empty, against a fake Discord.

use std::fs;
use std::path::Path;

use chipsoc_discord::fake::FakeDiscord;
use serde_json::{Value, json};

use super::*;

fn write(dir: &Path, file: &str, body: &str) {
    let p = dir.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A small config touching every kind of object.
fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    write(p, "server.toml", "[guild]\nname = \"Test\"\nsystem_channel = \"general\"\n");
    write(
        p,
        "bot.toml",
        "name = \"Test\"\nadmin_role = \"admin\"\narchive = \"archive\"\n[projects]\nforum = \"projects\"\n",
    );
    write(
        p,
        "roles.toml",
        r##"
[[role]]
key = "bot"
name = "Bot"
managed = true

[[role]]
key = "admin"
name = "Admin"
color = "#e8944a"
hoist = true
permissions = ["MANAGE_MESSAGES"]

[[role]]
key = "member"
name = "Member"

[[role]]
key = "everyone"
name = "@everyone"
permissions = ["VIEW_CHANNEL", "SEND_MESSAGES"]
"##,
    );
    write(
        p,
        "categories/10-main.toml",
        r#"
[category]
key = "main"
name = "Main"

[[channel]]
key = "general"
name = "general"
topic = "Chat"
post = "content/welcome.md"

[[channel]]
key = "projects"
type = "forum"
name = "projects"
tags = ["active", "past", "cpu"]

[[channel]]
key = "lounge"
type = "voice"
name = "Lounge"
user_limit = 5
"#,
    );
    write(
        p,
        "categories/20-staff.toml",
        r#"
[category]
key = "staff"
name = "Staff"
[[category.overwrites]]
role = "everyone"
deny = ["VIEW_CHANNEL"]
[[category.overwrites]]
role = "admin"
allow = ["VIEW_CHANNEL"]

[[channel]]
key = "review"
name = "review"

[[channel]]
key = "inbox"
name = "inbox"
[[channel.overwrites]]
role = "admin"
allow = ["VIEW_CHANNEL"]
deny = ["SEND_MESSAGES"]
"#,
    );
    write(
        p,
        "categories/90-archive.toml",
        "[category]\nkey = \"archive\"\nname = \"Archive\"\n[[category.overwrites]]\nrole = \"everyone\"\ndeny = [\"VIEW_CHANNEL\"]\n",
    );
    write(
        p,
        "content/welcome.md",
        "Hi! Ask in {#general}, or ping {@admin}.\n\n{button:profile|Set up your profile}\n",
    );
    write(
        p,
        "onboarding.toml",
        "enabled = true\n[[prompt]]\ntitle = \"Who are you?\"\n[[prompt.option]]\ntitle = \"A member\"\nemoji = \"👋\"\nrole = \"member\"\n",
    );
    write(p, "screening.toml", "enabled = true\ndescription = \"Hello\"\nlabel = \"I agree\"\nrules = [\"Be kind\"]\n");
    write(
        p,
        "automod.toml",
        "[[rule]]\nkey = \"spam\"\nname = \"Spam\"\ntrigger = \"spam\"\nactions = [\"block\", \"alert\"]\nalert_channel = \"review\"\n",
    );
    write(
        p,
        "projects.toml",
        "[[project]]\nslug = \"golem\"\nname = \"Golem\"\nblurb = \"a CPU\"\nkind = \"cpu\"\nstatus = \"active\"\nchannels = [\"general\", \"voice\"]\n[project.application]\nopen = true\n",
    );
    d
}

fn engine(dir: &Path) -> Engine<FakeDiscord> {
    Engine::load(FakeDiscord::new(), dir).unwrap()
}

async fn apply_all(e: &mut Engine<FakeDiscord>) -> Plan {
    let plan = e.plan(Options::default()).await.unwrap();
    let report = e.apply(&plan, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap();
    let failed: Vec<String> = report.failed().map(|o| format!("{} → {:?}", o.action, o.result)).collect();
    assert!(failed.is_empty(), "failed steps:\n{}", failed.join("\n"));
    plan
}

fn lines(p: &Plan) -> String {
    p.actions.iter().filter(|a| a.mutates()).map(ToString::to_string).collect::<Vec<_>>().join("\n")
}

async fn assert_converged(e: &Engine<FakeDiscord>) {
    let again = e.plan(Options::default()).await.unwrap();
    assert!(again.is_empty(), "not converged:\n{}", lines(&again));
}

#[tokio::test]
async fn apply_then_plan_is_empty() {
    let d = fixture();
    let mut e = engine(d.path());
    let first = apply_all(&mut e).await;
    assert!(first.changes() > 10, "{}", lines(&first));
    assert_converged(&e).await;

    // Project channels land right after the forum's category, in order.
    let live = e.live().await.unwrap();
    let cats: Vec<&str> = live.categories.iter().map(|c| c.category.key.as_str()).collect();
    assert_eq!(cats, ["main", "proj-golem", "staff", "archive"]);
    // The inbox kept the category's privacy plus its own rule.
    let inbox = live.channel("inbox").unwrap();
    assert!(inbox.overwrites.iter().any(|o| o.role.as_deref() == Some("everyone") && o.deny == ["VIEW_CHANNEL"]));
}

#[tokio::test]
async fn ui_changes_are_detected_and_undone() {
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    e.rest.edit_channel("general", |c| c["topic"] = json!("changed in the UI"));
    e.rest.edit_channel("lounge", |c| c["position"] = json!(-5));
    e.rest.state.lock().unwrap().onboarding["prompts"] = json!([]);
    let plan = e.plan(Options::default()).await.unwrap();
    let text = lines(&plan);
    assert!(text.contains("~ edit channel general") && text.contains("topic"), "{text}");
    assert!(text.contains("~ edit onboarding"), "{text}");
    e.rest.take_calls();
    apply_all(&mut e).await;
    let calls = e.rest.take_calls();
    assert!(calls.iter().all(|c| !c.contains("POST")), "nothing recreated: {calls:?}");
    assert_converged(&e).await;
}

#[tokio::test]
async fn edits_send_only_changed_fields_and_keep_tag_ids() {
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    let tag_id = |e: &Engine<FakeDiscord>| {
        let st = e.rest.state.lock().unwrap();
        let forum = st.channels.iter().find(|c| c["name"] == "projects").unwrap();
        forum["available_tags"][0]["id"].clone()
    };
    let before = tag_id(&e);
    let cat = d.path().join("categories/10-main.toml");
    let text = fs::read_to_string(&cat)
        .unwrap()
        .replace("tags = [\"active\", \"past\", \"cpu\"]", "tags = [\"active\", \"past\", \"cpu\", \"gpu\"]");
    fs::write(&cat, text).unwrap();
    e.reload().unwrap();
    apply_all(&mut e).await;
    assert_eq!(tag_id(&e), before, "existing tags keep their ids");
    assert_converged(&e).await;
}

#[tokio::test]
async fn removed_channels_are_archived_not_deleted() {
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    let staff = d.path().join("categories/20-staff.toml");
    let text = fs::read_to_string(&staff).unwrap();
    let cut = text.find("[[channel]]\nkey = \"inbox\"").unwrap();
    fs::write(&staff, &text[..cut]).unwrap();
    e.reload().unwrap();
    let plan = apply_all(&mut e).await;
    assert!(lines(&plan).contains("→ archive channel inbox"), "{}", lines(&plan));
    let live = e.live().await.unwrap();
    assert!(live.category("archive").unwrap().channels.iter().any(|c| c.name == "inbox"), "moved, history kept");
    assert_converged(&e).await;
}

#[tokio::test]
async fn stale_plans_are_refused() {
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    e.rest.edit_channel("general", |c| c["topic"] = json!("one"));
    let plan = e.plan(Options::default()).await.unwrap();
    e.rest.edit_channel("general", |c| c["topic"] = json!("two"));
    let err = e.apply(&plan, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap_err();
    assert!(err.to_string().contains("plan again"));
    // Typed, so drift watch knows to plan again instead of giving up.
    assert_eq!(err.downcast_ref::<crate::Refused>(), Some(&crate::Refused::Stale));

    // A config change pulled meanwhile (as a push during a drift undo) is the same.
    let plan = e.plan(Options::default()).await.unwrap();
    set_topic(d.path(), "Pushed while undoing");
    let err = e.apply(&plan, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap_err();
    assert_eq!(err.downcast_ref::<crate::Refused>(), Some(&crate::Refused::Stale), "{err}");

    // Another apply running: refused as busy, nothing done.
    let plan = e.plan(Options::default()).await.unwrap();
    let _held = crate::Lease::take(d.path()).unwrap();
    let err = e.apply(&plan, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap_err();
    assert_eq!(err.downcast_ref::<crate::Refused>(), Some(&crate::Refused::Busy), "{err}");
}

#[tokio::test]
async fn a_plan_cannot_be_turned_into_a_delete() {
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    let staff = d.path().join("categories/20-staff.toml");
    let text = fs::read_to_string(&staff).unwrap();
    let cut = text.find("[[channel]]\nkey = \"inbox\"").unwrap();
    fs::write(&staff, &text[..cut]).unwrap();
    e.reload().unwrap();
    let archive = e.plan(Options::default()).await.unwrap();
    let deleting = e.plan(Options { delete: true, ..Default::default() }).await.unwrap();
    assert_ne!(archive.fingerprint, deleting.fingerprint, "what was asked for is part of the plan");
    let forged = Plan { delete: true, actions: deleting.actions.clone(), ..archive };
    let err = e.apply(&forged, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap_err();
    assert!(err.to_string().contains("plan again"), "{err}");
    assert!(e.live().await.unwrap().channels().any(|(c, _)| c.name == "inbox"), "nothing deleted");
}

#[tokio::test]
async fn export_round_trips() {
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    let out = tempfile::tempdir().unwrap();
    let dir = out.path().join("exported");
    export_to(&e.rest, e.rest.guild_id, &dir).await.unwrap();
    let mirror = Engine::load(e.rest, &dir).unwrap();
    assert_converged(&mirror).await;
}

#[tokio::test]
async fn adopt_writes_ui_changes_back() {
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    e.rest.edit_channel("general", |c| c["topic"] = json!("from the UI"));
    let notes = e.adopt(false).await.unwrap();
    assert!(notes.iter().any(|n| matches!(n, Pulled::Adopted(s) if s.contains("channel general"))), "{notes:?}");
    assert_eq!(e.cfg.desired.channel("general").unwrap().topic.as_deref(), Some("from the UI"));
    assert_converged(&e).await;
}

/// The real config in this repo converges on a fresh server.
#[tokio::test]
async fn real_config_converges() {
    // Set BOT_CONFIG to also check a real config (e.g. ../chipsoc-config).
    let Ok(real) = std::env::var("BOT_CONFIG") else { return };
    let real = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(real);
    if !real.join("server.toml").exists() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    copy_dir(&real, d.path());
    let _ = fs::remove_file(d.path().join(LOCK));
    let mut e = engine(d.path());
    // The people members.toml and project leads name, in the pretend server too.
    let people: std::collections::BTreeSet<Snowflake> = e.cfg.desired.holders.values().flatten().copied().collect();
    for user in people {
        e.rest.join(user.0);
    }
    apply_all(&mut e).await;
    // Holders of roles the first run made are given on the next.
    apply_all(&mut e).await;
    assert_converged(&e).await;
}

const LOCK: &str = chipsoc_config::LOCK_FILE;

fn copy_dir(from: &Path, to: &Path) {
    chipsoc_config::ops::copy_dir(from, to).unwrap();
}

fn edit_cause(p: &Plan, key: &str) -> Option<chipsoc_model::Cause> {
    p.actions.iter().find_map(|a| match a {
        Action::Edit { key: k, cause, .. } if k == key => Some(*cause),
        _ => None,
    })
}

fn set_topic(dir: &Path, topic: &str) {
    let f = dir.join("categories/10-main.toml");
    let text = fs::read_to_string(&f).unwrap();
    let start = text.find("topic = \"").unwrap();
    let end = start + text[start..].find('\n').unwrap();
    fs::write(&f, format!("{}topic = \"{topic}\"{}", &text[..start], &text[end..])).unwrap();
}

#[tokio::test]
async fn edits_say_who_changed_what() {
    use chipsoc_model::Cause;
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;

    e.rest.edit_channel("general", |c| c["topic"] = json!("ui"));
    assert_eq!(edit_cause(&e.plan(Options::default()).await.unwrap(), "general"), Some(Cause::Discord));

    e.rest.edit_channel("general", |c| c["topic"] = json!("Chat"));
    set_topic(d.path(), "New topic");
    e.reload().unwrap();
    assert_eq!(edit_cause(&e.plan(Options::default()).await.unwrap(), "general"), Some(Cause::Config));

    e.rest.edit_channel("general", |c| c["topic"] = json!("ui"));
    let p = e.plan(Options::default()).await.unwrap();
    assert_eq!(edit_cause(&p, "general"), Some(Cause::Both));
    // Pull never overwrites config that hasn't been applied.
    let notes = e.adopt(false).await.unwrap();
    assert!(notes.iter().all(|n| matches!(n, Pulled::ByHand(_))), "{notes:?}");
    assert_eq!(e.cfg.desired.channel("general").unwrap().topic.as_deref(), Some("New topic"));
}

/// A role's permissions changed in Discord are copied back only when asked
/// (`pull --permissions`); otherwise they're left for a person.
#[tokio::test]
async fn pull_copies_role_permissions_only_when_asked() {
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    let id = e.cfg.desired.lock.role("member").unwrap().to_string();
    {
        let mut st = e.rest.state.lock().unwrap();
        let role = st.roles.iter_mut().find(|r| r["id"] == id.as_str()).unwrap();
        role["permissions"] = json!((1u64 << 6).to_string()); // ADD_REACTIONS, given in Discord's UI
    }
    let notes = e.adopt(false).await.unwrap();
    assert!(notes.iter().any(|n| matches!(n, Pulled::ByHand(s) if s.contains("role member"))), "{notes:?}");
    assert!(e.cfg.desired.role("member").unwrap().permissions.is_empty(), "left alone");
    let notes = e.adopt(true).await.unwrap();
    assert!(notes.iter().any(|n| matches!(n, Pulled::Adopted(s) if s.contains("role member"))), "{notes:?}");
    assert_eq!(e.cfg.desired.role("member").unwrap().permissions, ["ADD_REACTIONS"]);
    assert_converged(&e).await;
}

#[tokio::test]
async fn a_failed_category_doesnt_misplace_its_channels() {
    let d = fixture();
    let mut e = engine(d.path());
    e.rest.fail("POST guilds/1000/channels", 500, false); // the first create is the Main category
    let plan = e.plan(Options::default()).await.unwrap();
    let report = e.apply(&plan, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap();
    assert!(report.failed().count() >= 2, "the category and its channels fail");
    let top_level: Vec<String> = {
        let st = e.rest.state.lock().unwrap();
        st.channels
            .iter()
            .filter(|c| c["parent_id"].is_null() && c["type"] != 4)
            .map(|c| c["name"].to_string())
            .collect()
    };
    assert!(top_level.is_empty(), "nothing created outside its category: {top_level:?}");
    apply_all(&mut e).await;
    assert_converged(&e).await;
}

#[tokio::test]
async fn a_create_that_reached_discord_isnt_made_twice() {
    let d = fixture();
    let mut e = engine(d.path());
    e.rest.fail("POST guilds/1000/roles", 500, true); // Discord made it, but we never heard back
    let plan = e.plan(Options::default()).await.unwrap();
    let _ = e.apply(&plan, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap();
    apply_all(&mut e).await;
    let admins = e.rest.state.lock().unwrap().roles.iter().filter(|r| r["name"] == "Admin").count();
    assert_eq!(admins, 1);
    assert_converged(&e).await;
}

#[tokio::test]
async fn tampered_or_outdated_plans_are_refused() {
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    e.rest.edit_channel("general", |c| c["topic"] = json!("ui"));
    let mut plan = e.plan(Options::default()).await.unwrap();
    let general = e.cfg.desired.lock.channel("general").unwrap();
    plan.actions.push(Action::Delete { kind: chipsoc_model::ObjectKind::Channel, key: "general".into(), id: general });
    let err = e.apply(&plan, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap_err();
    assert!(err.to_string().contains("doesn't call for"), "{err}");

    let plan = e.plan(Options::default()).await.unwrap();
    set_topic(d.path(), "Edited after planning");
    let err = e.apply(&plan, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap_err();
    assert!(err.to_string().contains("plan again"), "{err}");
}

#[tokio::test]
async fn configs_with_mistakes_dont_plan() {
    let d = fixture();
    write(d.path(), "bot.toml", "name = \"Test\"\nadmin_role = \"nobody\"\n");
    let e = engine(d.path());
    let err = e.plan(Options::default()).await.unwrap_err();
    assert!(err.downcast_ref::<Problems>().is_some(), "{err}");
}

#[tokio::test]
async fn config_decides_who_holds_managed_roles() {
    let d = fixture();
    let p = d.path();
    write(
        p,
        "bot.toml",
        "name = \"Test\"\nadmin_role = \"admin\"\narchive = \"archive\"\n[projects]\nforum = \"projects\"\n[[position]]\nrole = \"chair\"\nadmin = true\n",
    );
    let roles = fs::read_to_string(p.join("roles.toml")).unwrap().replace(
        "[[role]]\nkey = \"admin\"",
        "[[role]]\nkey = \"chair\"\nname = \"Chair\"\n\n[[role]]\nkey = \"admin\"",
    );
    write(p, "roles.toml", &roles);
    write(p, "members.toml", "chair = [\"7\"]\n");
    let mut e = engine(p);
    for u in [7, 8, 9] {
        e.rest.join(u);
    }
    apply_all(&mut e).await; // creates the roles
    apply_all(&mut e).await; // then gives them
    let id = |e: &Engine<FakeDiscord>, k: &str| e.cfg.desired.lock.role(k).unwrap().to_string();
    let (chair, admin) = (id(&e, "chair"), id(&e, "admin"));
    assert!(e.rest.roles_of(7).contains(&chair) && e.rest.roles_of(7).contains(&admin), "chair is also admin");
    assert_converged(&e).await;

    // Someone hands out Chair in Discord: the plan takes it back.
    let path = format!("guilds/1000/members/8/roles/{chair}");
    e.rest.call(chipsoc_discord::Method::Put, &path, None).await.unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(lines(&plan).contains("holders of chair (changed in Discord)"), "{}", lines(&plan));
    apply_all(&mut e).await;
    assert!(!e.rest.roles_of(8).contains(&chair));

    // A project finishes: its team keep their role as a record, renamed.
    let team = id(&e, "proj-golem-team");
    e.rest.call(chipsoc_discord::Method::Put, &format!("guilds/1000/members/9/roles/{team}"), None).await.unwrap();
    let projects = fs::read_to_string(p.join("projects.toml")).unwrap();
    write(p, "projects.toml", &projects.replace("status = \"active\"", "status = \"past\""));
    e.reload().unwrap();
    apply_all(&mut e).await;
    assert!(e.rest.roles_of(9).contains(&team));
    let name = e.rest.state.lock().unwrap().roles.iter().find(|r| r["id"] == team.as_str()).unwrap()["name"].clone();
    assert_eq!(name, "Golem · Contributor");
    assert_converged(&e).await;
}

#[tokio::test]
async fn applying_part_of_a_plan_leaves_the_rest_pending() {
    use chipsoc_model::Cause;
    let d = fixture();
    let p = d.path();
    let mut e = engine(p);
    apply_all(&mut e).await;
    set_topic(p, "New topic");
    e.rest.edit_channel("Lounge", |c| c["user_limit"] = json!(9));
    let is_lounge = |a: &Action| matches!(a, Action::Edit { key, .. } if key == "lounge");
    let report = e.apply_only(is_lounge, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap();
    assert_eq!(report.outcomes.len(), 1);
    let plan = e.plan(Options::default()).await.unwrap();
    assert_eq!(edit_cause(&plan, "lounge"), None, "undone");
    assert_eq!(edit_cause(&plan, "general"), Some(Cause::Config), "still waiting, not mistaken for a Discord edit");
    let none = e.apply_only(is_lounge, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap();
    assert!(none.outcomes.is_empty());
}

#[tokio::test]
async fn a_new_holder_is_given_the_role_alone() {
    let d = fixture();
    let p = d.path();
    write(p, "members.toml", "# people\nadmin = [\"7\"]\n");
    let mut e = engine(p);
    for u in [7, 8] {
        e.rest.join(u);
    }
    apply_all(&mut e).await;
    apply_all(&mut e).await;
    set_topic(p, "Not yet");
    assert!(chipsoc_config::authoring::add_holder(p, "admin", 8).unwrap());
    let holders = |a: &Action| matches!(a, Action::Edit { kind: ObjectKind::Holders, key, .. } if key == "admin");
    let report = e.apply_only(holders, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap();
    assert_eq!(report.failed().count(), 0);
    let admin = e.cfg.desired.lock.role("admin").unwrap().to_string();
    assert!(e.rest.roles_of(8).contains(&admin));
    let plan = e.plan(Options::default()).await.unwrap();
    assert_eq!(lines(&plan).lines().next(), Some("~ edit channel general"), "{}", lines(&plan));
}

fn posts_made(e: &Engine<FakeDiscord>) -> Vec<String> {
    let calls = e.rest.take_calls();
    calls.into_iter().filter(|c| c.starts_with("Post") && !c.contains("@me")).collect()
}

/// Losing lock.toml must not re-post every post and thread, and must not let
/// a sweep archive what the config describes.
#[tokio::test]
async fn a_lost_lockfile_is_matched_back_not_reposted() {
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    apply_all(&mut e).await; // a second save, so lock.toml.bak is the full one
    assert!(d.path().join(chipsoc_config::LOCK_BACKUP).exists());
    e.rest.take_calls();

    fs::remove_file(d.path().join(LOCK)).unwrap();
    e.reload().unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    let warned = plan.warnings.iter().any(|w| w.to_string().contains("cp lock.toml.bak lock.toml"));
    assert!(warned, "{:?}", plan.warnings);
    let err = e.plan(Options { sweep: true, ..Default::default() }).await.unwrap_err().to_string();
    assert!(err.contains("refusing to sweep") && err.contains("lock.toml.bak"), "{err}");

    apply_all(&mut e).await;
    assert_eq!(posts_made(&e), Vec::<String>::new(), "nothing made again");
    assert_converged(&e).await;
    assert!(e.cfg.desired.lock.posts.contains_key("general") && e.cfg.desired.lock.project_posts.contains_key("golem"));
}

#[tokio::test]
async fn several_unclaimed_namesakes_are_refused_not_added_to() {
    let d = fixture();
    let mut e = engine(d.path());
    for id in ["1500", "1501"] {
        let role =
            json!({ "id": id, "name": "Member", "position": 1, "permissions": "0", "color": 0, "managed": false });
        e.rest.state.lock().unwrap().roles.push(role);
    }
    let plan = e.plan(Options::default()).await.unwrap();
    let report = e.apply(&plan, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap();
    let failed: Vec<String> = report.failed().filter_map(|o| o.result.clone().err()).collect();
    assert!(failed.iter().any(|f| f.contains("2 things named `Member`")), "{failed:?}");
    let members = e.rest.state.lock().unwrap().roles.iter().filter(|r| r["name"] == "Member").count();
    assert_eq!(members, 2, "no third one");
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// An origin holding the fixture, and two clones of it.
fn shared_config() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let origin = root.path().join("origin.git");
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    git(root.path(), &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
    for c in [&a, &b] {
        git(root.path(), &["clone", "-q", origin.to_str().unwrap(), c.to_str().unwrap()]);
        git(c, &["config", "user.email", "t@t"]);
        git(c, &["config", "user.name", "t"]);
    }
    let f = fixture();
    copy_dir(f.path(), &a);
    git(&a, &["add", "."]);
    git(&a, &["commit", "-qm", "config"]);
    git(&a, &["push", "-q", "-u", "origin", "HEAD"]);
    git(&b, &["pull", "-q"]);
    (root, a, b)
}

/// A laptop behind its remote would plan someone else's applied change as
/// made in Discord, and undo it.
#[tokio::test]
async fn a_stale_checkout_is_refused_until_pulled() {
    let (_root, a, b) = shared_config();
    let mut ea = engine(&a);
    apply_all(&mut ea).await;
    let applied = ea.cfg.desired.lock.applied.commit.clone();
    assert!(applied.is_some(), "the applied commit is recorded");
    set_topic(&a, "new topic");
    git(&a, &["add", "."]);
    git(&a, &["commit", "-qm", "apply"]);
    git(&a, &["push", "-q"]);

    let eb = Engine::load(FakeDiscord::new(), &b).unwrap();
    let err = eb.plan(Options::default()).await.unwrap_err().to_string();
    assert!(err.contains("1 commit(s) behind origin/main") && err.contains("pull first"), "{err}");
    git(&b, &["pull", "-q"]);
    let eb = Engine::load(FakeDiscord::new(), &b).unwrap();
    eb.plan(Options::default()).await.unwrap();

    // lock.toml naming a commit this copy hasn't got: refused too.
    let lock_path = b.join(LOCK);
    let body = fs::read_to_string(&lock_path).unwrap();
    fs::write(&lock_path, body.replace(applied.as_deref().unwrap(), "0123456789abcdef0123456789abcdef01234567"))
        .unwrap();
    let eb = Engine::load(FakeDiscord::new(), &b).unwrap();
    let err = eb.plan(Options::default()).await.unwrap_err().to_string();
    assert!(err.contains("this copy doesn't have it"), "{err}");
}

#[tokio::test]
async fn offline_checkouts_still_plan_with_a_note() {
    let (root, _a, b) = shared_config();
    git(&b, &["remote", "set-url", "origin", root.path().join("gone.git").to_str().unwrap()]);
    let eb = Engine::load(FakeDiscord::new(), &b).unwrap();
    let plan = eb.plan(Options::default()).await.unwrap();
    assert!(plan.warnings.iter().any(|w| w.to_string().contains("couldn't check the config's remote")));
    assert!(plan.changes() > 0);
}

/// The join gate is permissions: it applies, converges, and Discord's
/// onboarding questions go.
#[tokio::test]
async fn the_join_gate_applies_and_converges() {
    let d = fixture();
    let p = d.path();
    let mut e = engine(p);
    apply_all(&mut e).await;
    let roles = fs::read_to_string(p.join("roles.toml")).unwrap();
    write(p, "roles.toml", &format!("{roles}\n[[role]]\nkey = \"access\"\nname = \"Access\"\n"));
    write(p, "content/welcome.md", "Hi!\n\n{button:join|Get started}\n");
    write(p, "onboarding.toml", "enabled = false\n");
    write(
        p,
        "join.toml",
        "access_role = \"access\"\ngate = true\nopen_channels = [\"general\"]\n\
         [[step]]\nkey = \"who\"\nquestion = \"Who are you?\"\n[[step.option]]\nlabel = \"A member\"\nroles = [\"member\"]\n",
    );
    let report = chipsoc_config::authoring::check(p).unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let mut e = engine(p);
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(plan.warnings.iter().any(|w| w.to_string().contains("join gate: @everyone will only see #general")));
    apply_all(&mut e).await;
    assert_converged(&e).await;
    let live = e.live().await.unwrap();
    assert_eq!(chipsoc_config::public_channels(&live), ["general"]);
    assert!(live.role("access").unwrap().permissions.contains(&"VIEW_CHANNEL".to_owned()));
    let onboarding = e.rest.state.lock().unwrap().onboarding.clone();
    assert_eq!((onboarding["enabled"].clone(), onboarding["prompts"].clone()), (json!(false), json!([])));
    let again = e.plan(Options::default()).await.unwrap();
    assert!(!again.warnings.iter().any(|w| w.to_string().contains("join gate")), "said once, when it goes on");
    // Who holds the flow's roles is up to members' answers, not the config: never drift.
    e.rest.join(5);
    for key in ["member", "access"] {
        let id = e.cfg.desired.lock.role(key).unwrap().to_string();
        let mut st = e.rest.state.lock().unwrap();
        st.members.iter_mut().find(|m| m["user"]["id"] == "5").unwrap()["roles"]
            .as_array_mut()
            .unwrap()
            .push(json!(id));
    }
    assert_converged(&e).await;
}

/// A config with an admin position, applied, with 7 (chair), 8 and 9 in the server.
async fn with_positions() -> (tempfile::TempDir, Engine<FakeDiscord>) {
    let d = fixture();
    let p = d.path();
    write(
        p,
        "bot.toml",
        "name = \"Test\"\nadmin_role = \"admin\"\narchive = \"archive\"\n[projects]\nforum = \"projects\"\n[[position]]\nrole = \"chair\"\nadmin = true\n",
    );
    let roles = fs::read_to_string(p.join("roles.toml")).unwrap().replace(
        "[[role]]\nkey = \"admin\"",
        "[[role]]\nkey = \"chair\"\nname = \"Chair\"\n\n[[role]]\nkey = \"admin\"",
    );
    write(p, "roles.toml", &roles);
    write(p, "members.toml", "chair = [\"7\"]\n");
    let mut e = engine(p);
    for u in [7, 8, 9] {
        e.rest.join(u);
    }
    apply_all(&mut e).await;
    apply_all(&mut e).await;
    (d, e)
}

/// Appointing someone gives them the position and the admin role, and no one
/// else loses anything: not even an admin given the role by hand.
#[tokio::test]
async fn applying_one_persons_holders_touches_no_one_else() {
    let (d, mut e) = with_positions().await;
    let id = |e: &Engine<FakeDiscord>, k: &str| e.cfg.desired.lock.role(k).unwrap().to_string();
    let (chair, admin) = (id(&e, "chair"), id(&e, "admin"));
    // 9 was made an admin by hand; the config doesn't list them.
    e.rest.call(Method::Put, &format!("guilds/1000/members/9/roles/{admin}"), None).await.unwrap();
    e.rest.take_calls();

    let roles = ["chair".to_owned(), "admin".to_owned()];
    let (_, saved) = e
        .edit_with(|dir| Ok((authoring::add_holder(dir, "chair", 8)?, Some("Give chair to 8".into()))), &By::bot())
        .await
        .unwrap();
    assert!(!saved.unwrap(), "not a synced checkout: nothing committed");
    let held = e.apply_holders(Snowflake(8), &roles).await.unwrap();
    assert_eq!(held.len(), 2, "{held:?}");
    assert!(held.iter().all(|h| h.gave && h.result.is_ok()), "{held:?}");
    assert!(e.rest.roles_of(8).contains(&chair) && e.rest.roles_of(8).contains(&admin));
    assert!(e.rest.roles_of(9).contains(&admin), "the hand-made admin keeps it");
    let calls = e.rest.take_calls();
    assert_eq!(
        calls,
        [format!("Put guilds/1000/members/8/roles/{chair}"), format!("Put guilds/1000/members/8/roles/{admin}")]
    );

    // The chair's holders are in step and recorded; a full plan still warns about 9.
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(!lines(&plan).contains("holders of chair"), "{}", lines(&plan));
    let warned = plan.warnings.iter().map(ToString::to_string).find(|w| w.contains("takes the admin role"));
    assert!(warned.is_some_and(|w| w.contains("<@9>") && w.contains("add them there")), "{:?}", plan.warnings);

    // Taking 8 out of the position takes the admin role from them now, and from nobody else.
    authoring::remove_holder(d.path(), "chair", 8).unwrap();
    let held = e.apply_holders(Snowflake(8), &roles).await.unwrap();
    assert!(held.iter().all(|h| !h.gave && h.result.is_ok()), "{held:?}");
    assert!(!e.rest.roles_of(8).contains(&admin) && !e.rest.roles_of(8).contains(&chair));
    assert!(e.rest.roles_of(7).contains(&admin) && e.rest.roles_of(9).contains(&admin));
}

/// The last admin in the config isn't left without the role by a holders apply.
#[tokio::test]
async fn the_last_configured_admin_keeps_the_role() {
    let (d, mut e) = with_positions().await;
    let admin = e.cfg.desired.lock.role("admin").unwrap().to_string();
    write(d.path(), "members.toml", "chair = []\n");
    let held = e.apply_holders(Snowflake(7), &["chair".into(), "admin".into()]).await.unwrap();
    let refused = held.iter().find(|h| h.role == "admin").unwrap();
    assert!(refused.result.as_ref().is_err_and(|e| e.contains("nobody")), "{held:?}");
    assert!(e.rest.roles_of(7).contains(&admin));
}

/// An edit is checked, written, committed saying who, and reloaded, all under the write lock.
#[tokio::test]
async fn edits_are_checked_committed_and_reloaded() {
    let (_root, a, _b) = shared_config();
    let mut e = engine(&a).autosave(true);
    let op = Op::SetChannel { key: "general".into(), set: ops::ChannelField::Topic(Some("Edited".into())) };
    let change = e.edit(&op, &By::named(Some("Ada"))).await.unwrap();
    assert_eq!(change.files, ["categories/10-main.toml"]);
    assert_eq!(e.cfg.desired.channel("general").unwrap().topic.as_deref(), Some("Edited"), "reloaded");
    let log = std::process::Command::new("git").arg("-C").arg(&a).args(["log", "-1", "--format=%s"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "Channel general: topic to \"Edited\" by Ada");
    assert!(git::uncommitted(&a).is_empty(), "committed");

    // One that would break the config is refused and changes nothing.
    let bad = Op::SetChannel {
        key: "general".into(),
        set: ops::ChannelField::Overwrites(vec![Overwrite {
            role: Some("nobody".into()),
            allow: vec!["VIEW_CHANNEL".into()],
            ..Default::default()
        }]),
    };
    let err = e.edit(&bad, &By::bot()).await.unwrap_err();
    assert!(err.downcast_ref::<Problems>().is_some(), "{err:#}");
    assert!(git::uncommitted(&a).is_empty());
    // So is a key that isn't a key.
    let role = Role { key: "Bad Key".into(), name: "Bad".into(), ..Default::default() };
    let err = e.edit(&Op::AddRole { role }, &By::bot()).await.unwrap_err();
    assert!(format!("{err:#}").contains("lower-case letters, digits and dashes"), "{err:#}");
}

/// Pull copies Discord's changes through typed ops: a role, the server, the order.
#[tokio::test]
async fn pull_copies_through_ops_in_one_change() {
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    let member = e.cfg.desired.lock.role("member").unwrap().to_string();
    {
        let mut st = e.rest.state.lock().unwrap();
        let r = st.roles.iter_mut().find(|r| r["id"] == member.as_str()).unwrap();
        r["hoist"] = json!(true);
    }
    e.rest.call(Method::Patch, "guilds/1000", Some(json!({ "name": "Renamed" }))).await.unwrap();
    let notes = e.adopt(false).await.unwrap();
    let adopted: Vec<String> = notes
        .iter()
        .filter_map(|n| match n {
            Pulled::Adopted(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(adopted.iter().any(|s| s == "role member: hoist"), "{notes:?}");
    assert!(adopted.iter().any(|s| s == "server server: name"), "{notes:?}");
    assert!(e.cfg.desired.role("member").unwrap().hoist);
    assert_eq!(e.cfg.desired.server.guild.name, "Renamed");
    assert_converged(&e).await;
}

/// A [`Baseline`] in memory, standing in for the bot's database. With
/// `broken` set, saving the lock fails.
#[derive(Default)]
struct Kept(std::sync::Mutex<chipsoc_config::trust::Trusted>, std::sync::atomic::AtomicBool);

impl Baseline for Kept {
    fn load(&self) -> BoxFuture<'_, Result<chipsoc_config::trust::Trusted>> {
        let t = self.0.lock().unwrap().clone();
        Box::pin(async move { Ok(t) })
    }
    fn save_lock<'a>(&'a self, lock: &'a Lock) -> BoxFuture<'a, Result<()>> {
        if self.1.load(std::sync::atomic::Ordering::Relaxed) {
            return Box::pin(async { anyhow::bail!("database is locked") });
        }
        self.0.lock().unwrap().lock = Some(lock.clone());
        Box::pin(async { Ok(()) })
    }
}

/// Rewrite lock.toml as a pusher would.
fn edit_lock(dir: &Path, f: impl FnOnce(&mut Lock)) {
    let mut lock: Lock = toml::from_str(&fs::read_to_string(dir.join("lock.toml")).unwrap()).unwrap();
    f(&mut lock);
    fs::write(dir.join("lock.toml"), toml::to_string(&lock).unwrap()).unwrap();
}

#[tokio::test]
async fn a_forged_applied_hash_cant_make_a_pushed_change_look_like_discord_drift() {
    use chipsoc_model::Cause;
    let d = fixture();
    let kept = Arc::new(Kept::default());
    let mut e = Engine::load(FakeDiscord::new(), d.path()).unwrap().trusting(kept.clone()).await.unwrap();
    apply_all(&mut e).await;
    assert!(kept.0.lock().unwrap().lock.as_ref().is_some_and(|l| l.applied.hashes.contains_key("role:member")));

    // A pusher gives a role Administrator and forges what lock.toml says was applied.
    let roles = fs::read_to_string(d.path().join("roles.toml")).unwrap();
    let roles = roles.replace("permissions = [\"MANAGE_MESSAGES\"]", "permissions = [\"ADMINISTRATOR\"]");
    fs::write(d.path().join("roles.toml"), roles).unwrap();
    let trust = e.trust.take();
    e.reload().unwrap();
    let forged = e.plan(Options::default()).await.unwrap().hashes["role:admin"].clone();
    edit_lock(d.path(), |l| drop(l.applied.hashes.insert("role:admin".into(), forged.clone())));
    // Without the bot's own record, it would look like a change made in Discord (which drift watch undoes).
    e.reload().unwrap();
    assert_eq!(edit_cause(&e.plan(Options::default()).await.unwrap(), "admin"), Some(Cause::Discord));
    e.trust = trust;
    e.refresh().await.unwrap();
    let p = e.plan(Options::default()).await.unwrap();
    assert_eq!(edit_cause(&p, "admin"), Some(Cause::Config), "{}", lines(&p));
}

#[tokio::test]
async fn a_pushed_lock_cant_repoint_a_managed_id() {
    let d = fixture();
    let kept = Arc::new(Kept::default());
    let mut e = Engine::load(FakeDiscord::new(), d.path()).unwrap().trusting(kept).await.unwrap();
    apply_all(&mut e).await;
    let (member, admin) = (e.cfg.desired.lock.roles["member"], e.cfg.desired.lock.roles["admin"]);
    edit_lock(d.path(), |l| {
        l.roles.insert("member".into(), admin);
    });
    e.refresh().await.unwrap();
    assert_eq!(e.cfg.desired.lock.roles["member"], member, "the bot's id is kept");
    let p = e.plan(Options::default()).await.unwrap();
    assert!(p.warnings.iter().any(|w| w.to_string().contains("roles.member")), "{:?}", p.warnings);
    assert!(p.is_empty() || !lines(&p).contains("member"), "{}", lines(&p));
}

#[tokio::test]
async fn what_the_token_cant_read_is_left_out_and_said() {
    // A read-only bot planning in CI: no Manage Server, and no access to a channel.
    let d = fixture();
    let mut e = engine(d.path());
    apply_all(&mut e).await;
    let general = e.cfg.desired.lock.channel("general").unwrap();
    e.rest.fail("GET guilds/1000/auto-moderation/rules", 403, false);
    e.rest.fail(&format!("GET channels/{general}/messages"), 403, false);
    // Discord documents no permission for these; if it refuses anyway, they're left out too.
    e.rest.fail("GET guilds/1000/onboarding", 403, false);
    e.rest.fail("GET guilds/1000/member-verification", 403, false);
    let p = e.plan(Options::default()).await.unwrap();
    assert!(p.is_empty(), "nothing is planned for what can't be read:\n{}", lines(&p));
    let notes: Vec<String> = p.warnings.iter().map(ToString::to_string).collect();
    let note = notes.iter().find(|n| n.contains("can't read")).expect("a note");
    assert!(note.contains("AutoMod rules (reading them needs Manage Server)") && note.contains("#general"), "{note}");
    assert!(note.contains("onboarding") && note.contains("rules screen"), "{note}");
    assert_converged(&e).await;
}

#[tokio::test]
async fn renaming_a_project_keeps_its_roles_channels_and_post() {
    let d = fixture();
    let kept = Arc::new(Kept::default());
    let mut e = Engine::load(FakeDiscord::new(), d.path()).unwrap().trusting(kept.clone()).await.unwrap();
    apply_all(&mut e).await;
    let (team, cat) =
        (e.cfg.desired.lock.roles["proj-golem-team"].to_string(), e.cfg.desired.lock.category("proj-golem").unwrap());
    let post = e.cfg.desired.lock.project_posts["golem"].clone();
    let (roles, channels) = {
        let st = e.rest.state.lock().unwrap();
        (st.roles.len(), st.channels.len())
    };

    e.edit(&Op::RenameProject { from: "golem".into(), to: "robot".into() }, &By::bot()).await.unwrap();
    // The bot's own lock follows the files', so nothing is held back or made anew.
    assert!(e.vetted.lock.is_clean(), "{:?}", e.vetted.lock);
    let trusted = kept.0.lock().unwrap().lock.clone().unwrap();
    assert_eq!(trusted.roles["proj-robot-team"].to_string(), team);
    assert!(trusted.applied.hashes.contains_key("project-post:robot"));
    let p = e.plan(Options::default()).await.unwrap();
    assert!(!lines(&p).contains("create"), "{}", lines(&p));
    assert!(lines(&p).contains("robot-general"), "the text channel's name follows the slug:\n{}", lines(&p));
    apply_all(&mut e).await;
    let now = {
        let st = e.rest.state.lock().unwrap();
        (st.roles.len(), st.channels.len())
    };
    assert_eq!(now, (roles, channels), "the same objects");
    assert_eq!(e.cfg.desired.lock.category("proj-robot"), Some(cat));
    assert_eq!(e.cfg.desired.lock.project_posts["robot"], post);
    assert_converged(&e).await;
}

/// A rename whose follow-up fails still says the edit was made (it was), with what didn't follow.
#[tokio::test]
async fn a_rename_is_reported_made_even_when_the_trusted_lock_cant_follow() {
    let d = fixture();
    let kept = Arc::new(Kept::default());
    let mut e = Engine::load(FakeDiscord::new(), d.path()).unwrap().trusting(kept.clone()).await.unwrap();
    apply_all(&mut e).await;
    kept.1.store(true, std::sync::atomic::Ordering::Relaxed);
    let change = e.edit(&Op::RenameProject { from: "golem".into(), to: "robot".into() }, &By::bot()).await.unwrap();
    assert!(!change.files.is_empty());
    assert!(change.warnings.iter().any(|w| w.contains("database is locked")), "{:?}", change.warnings);
    assert!(e.cfg.projects.iter().any(|p| p.slug == "robot"), "the edit stands and is loaded");
}

/// examples/minimal (pointed to by the README) checks clean, applies, and then matches.
#[tokio::test]
async fn the_minimal_example_checks_and_applies() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/minimal");
    let report = chipsoc_config::authoring::check(&example).unwrap();
    assert_eq!(report.errors, Vec::<String>::new());
    let d = tempfile::tempdir().unwrap();
    copy_dir(&example, d.path());
    let mut e = engine(d.path());
    e.rest.join(111111111111111111);
    let first = apply_all(&mut e).await;
    assert!(lines(&first).contains("create channel welcome"), "{}", lines(&first));
    // Holders of a role made in the same apply follow in the next.
    apply_all(&mut e).await;
    assert_converged(&e).await;
    let rules = e.cfg.desired.posts["rules"].clone();
    assert!(rules.contains("- **Volunteer**: Help plan events"), "{rules}");
}

/// A post on a forum is a pinned "How to post here" thread, edited in place
/// and found again if lock.toml loses it.
#[tokio::test]
async fn a_forum_post_is_a_pinned_thread() {
    let d = fixture();
    let p = d.path();
    let cat = fs::read_to_string(p.join("categories/10-main.toml")).unwrap();
    let cat = cat.replace(
        "tags = [\"active\", \"past\", \"cpu\"]\n",
        "tags = [\"active\", \"past\", \"cpu\"]\npost = \"content/how.md\"\n",
    );
    write(p, "categories/10-main.toml", &cat);
    write(p, "content/how.md", "Projects only.\n\n{button:idea|Propose an idea}\n");
    let mut e = engine(p);
    apply_all(&mut e).await;
    assert_converged(&e).await;
    let thread = e.cfg.desired.lock.posts["projects"].message;
    let forum = e.cfg.desired.lock.channel("projects").unwrap().to_string();
    {
        let st = e.rest.state.lock().unwrap();
        let t = st.channels.iter().find(|c| c["id"] == thread.to_string()).unwrap();
        assert_eq!((t["name"].as_str(), t["parent_id"].as_str()), (Some("How to post here"), Some(forum.as_str())));
        assert_eq!(t["flags"], json!(2), "pinned");
        let m = st.messages.iter().find(|m| m["id"] == thread.to_string()).unwrap();
        assert_eq!(m["content"], "Projects only.");
        assert_eq!(m["components"][0]["components"][0]["label"], "Propose an idea");
    }

    write(p, "content/how.md", "Projects only, please.\n\n{button:idea|Propose an idea}\n");
    e.reload().unwrap();
    e.rest.take_calls();
    apply_all(&mut e).await;
    let calls = e.rest.take_calls();
    assert_eq!(calls, [format!("Patch channels/{thread}/messages/{thread}")], "edited in place");
    assert_converged(&e).await;

    fs::remove_file(p.join(LOCK)).unwrap();
    e.reload().unwrap();
    apply_all(&mut e).await;
    assert_eq!(posts_made(&e), Vec::<String>::new(), "found again, not posted twice");
    assert_eq!(e.cfg.desired.lock.posts["projects"].message, thread);
    assert_converged(&e).await;
}

/// Roles chair, treasurer, member under the bot's, applied.
async fn ranked() -> (tempfile::TempDir, Engine<FakeDiscord>) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    write(p, "server.toml", "[guild]\nname = \"Test\"\n");
    write(p, "bot.toml", "name = \"Test\"\n");
    let role = |k: &str, n: &str| format!("[[role]]\nkey = \"{k}\"\nname = \"{n}\"\n\n");
    let roles = format!(
        "[[role]]\nkey = \"bot\"\nname = \"Bot\"\nmanaged = true\n\n{}{}{}{}",
        role("chair", "Chair"),
        role("treasurer", "Treasurer"),
        role("member", "Member"),
        role("everyone", "@everyone")
    );
    write(p, "roles.toml", &roles);
    write(p, "categories/10-main.toml", "[category]\nkey = \"main\"\nname = \"Main\"\n");
    let mut e = engine(p);
    apply_all(&mut e).await;
    assert_converged(&e).await;
    (d, e)
}

fn position(e: &Engine<FakeDiscord>, name: &str) -> i64 {
    let st = e.rest.state.lock().unwrap();
    st.roles.iter().find(|r| r["name"] == name).and_then(|r| r["position"].as_i64()).unwrap()
}

#[tokio::test]
async fn roles_above_the_bot_are_named_and_the_rest_still_sorted() {
    let (_d, mut e) = ranked().await;
    // Someone drags Chair above the bot, and swaps the two below it.
    e.rest.set_role_position("Chair", 50);
    let (t, m) = (position(&e, "Treasurer"), position(&e, "Member"));
    e.rest.set_role_position("Treasurer", m);
    e.rest.set_role_position("Member", t);
    let plan = e.plan(Options::default()).await.unwrap();
    let warning = plan.warnings.iter().find_map(|w| match w {
        Warning::BotRoleTooLow { above, .. } => Some(above.clone()),
        _ => None,
    });
    assert_eq!(warning, Some(vec!["Chair".to_owned()]), "{:?}", plan.warnings);
    let text = plan.warnings[0].to_string();
    assert!(text.contains("below Chair") && text.contains("Server Settings → Roles"), "{text}");
    assert!(lines(&plan).contains("↕ reorder roles"), "{}", lines(&plan));
    apply_all(&mut e).await;
    assert!(position(&e, "Treasurer") > position(&e, "Member"), "the roles below the bot are in config order");
    assert_eq!(position(&e, "Chair"), 50, "left where it is");
    assert_converged(&e).await;
}

#[tokio::test]
async fn a_reorder_naming_a_role_above_the_bot_moves_the_rest_and_says_which() {
    let (_d, e) = ranked().await;
    e.rest.set_role_position("Chair", 50);
    let (t, m) = (position(&e, "Treasurer"), position(&e, "Member"));
    e.rest.set_role_position("Treasurer", m);
    e.rest.set_role_position("Member", t);
    let to: Vec<String> = ["chair", "treasurer", "member"].map(String::from).into();
    let plan = Plan {
        actions: vec![Action::Reorder { scope: chipsoc_model::OrderScope::Roles, from: vec![], to }],
        ..Plan::default()
    };
    struct Nothing;
    impl chipsoc_discord::exec::Sink for Nothing {
        async fn saved(&mut self, _: &chipsoc_config::Lock) -> Result<()> {
            Ok(())
        }
    }
    let mut lock = e.cfg.desired.lock.clone();
    let r = chipsoc_discord::exec::execute(&e.rest, &e.cfg, &plan, None, &mut lock, &mut Nothing).await;
    let err = r.outcomes[0].result.clone().unwrap_err();
    assert!(err.contains("not Chair") && err.contains("drag the bot's role"), "{err}");
    assert!(position(&e, "Treasurer") > position(&e, "Member"), "the others moved");
}

/// Only the server owner reverts or rewinds; either is a new commit, and its
/// plan undoes the change in Discord (archiving, never deleting).
#[tokio::test]
async fn the_owner_can_revert_or_rewind_what_an_admin_did() {
    let (_root, a, b) = shared_config();
    let mut e = engine(&a).autosave(true);
    e.rest.state.lock().unwrap().guild.insert("owner_id".into(), json!("1"));
    let (owner, ada) = (By::person(Snowflake(1), "olive"), By::person(Snowflake(2), "ada"));
    apply_all(&mut e).await;
    let before = e.cfg.desired.channel("general").unwrap().topic.clone();

    let op = Op::SetChannel { key: "general".into(), set: ops::ChannelField::Topic(Some("Ada was here".into())) };
    e.edit(&op, &ada).await.unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    e.apply_by(&plan, &ada, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap();
    let theirs = git::log(&a, 5).into_iter().find(|c| c.subject.starts_with("Channel general")).unwrap();
    assert_eq!((theirs.who.as_str(), theirs.who_id), ("ada", Some(Snowflake(2))), "signed with ada's id");

    let refused = e.undo(&theirs.short, git::Undo::Revert, &ada).await.unwrap_err();
    assert_eq!(refused.downcast_ref::<OwnerOnly>(), Some(&OwnerOnly::Undo));
    let made = e.undo(&theirs.short, git::Undo::Revert, &owner).await.unwrap().unwrap().subject;
    assert!(made.starts_with("Revert \"Channel general") && made.ends_with("by olive"), "{made}");
    assert_eq!(e.cfg.desired.channel("general").unwrap().topic, before, "reloaded as it was");
    git(&b, &["pull", "-q"]);
    assert_eq!(engine(&b).cfg.desired.channel("general").unwrap().topic, before, "and pushed");
    let plan = e.plan(Options::default()).await.unwrap();
    let topic = plan.actions.iter().any(|a| {
        matches!(a, Action::Edit { key, changes, .. } if key == "general"
            && changes.iter().any(|c| c.field == chipsoc_model::Field::Topic && c.from == "Ada was here"))
    });
    assert!(topic && plan.changes() == 1, "{}", lines(&plan));
    e.apply_by(&plan, &ada, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap();
    assert!(e.plan(Options::default()).await.unwrap().is_empty(), "Discord is back as it was");

    // A channel ada added: rewinding to before it plans to archive it, never to delete it.
    let start = git::log(&a, 1)[0].short.clone();
    let channel = Channel { key: "ada-chat".into(), name: "ada-chat".into(), ..Default::default() };
    e.edit(&Op::AddChannel { category: Some("main".into()), channel }, &ada).await.unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    e.apply_by(&plan, &ada, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap();
    e.undo(&start, git::Undo::Rewind, &owner).await.unwrap().unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(
        plan.actions.iter().any(|a| matches!(a, Action::Archive { key, .. } if key == "ada-chat")),
        "{}",
        lines(&plan)
    );
    assert!(!plan.actions.iter().any(|a| matches!(a, Action::Delete { .. })));
}

/// Deleting for real and sweeping are the server owner's to apply.
#[tokio::test]
async fn only_the_owner_applies_deletes_and_sweeps() {
    let d = fixture();
    let mut e = engine(d.path());
    e.rest.state.lock().unwrap().guild.insert("owner_id".into(), json!("1"));
    apply_all(&mut e).await;
    let (owner, ada) = (By::person(Snowflake(1), "olive"), By::person(Snowflake(2), "ada"));
    for opts in [Options { delete: true, ..Default::default() }, Options { sweep: true, ..Default::default() }] {
        let plan = e.plan(opts).await.unwrap();
        assert_eq!(plan.owner, Some(Snowflake(1)));
        let err = e.apply_by(&plan, &ada, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap_err();
        let why = err.downcast_ref::<OwnerOnly>().copied();
        assert!(matches!(why, Some(OwnerOnly::Delete | OwnerOnly::Sweep)), "{err:#}");
        assert!(err.to_string().starts_with("Only the server owner can"), "{err}");
        e.apply_by(&plan, &owner, &mut |_: &Action, _: &Result<(), String>| {}).await.unwrap();
    }
}

/// The bot's messages in channel `ch`, oldest first: (id, text, has buttons).
fn messages_in(e: &Engine<FakeDiscord>, ch: Snowflake) -> Vec<(String, String, bool)> {
    let st = e.rest.state.lock().unwrap();
    st.messages
        .iter()
        .filter(|m| m["channel_id"] == ch.to_string())
        .map(|m| {
            let buttons = m["components"].as_array().is_some_and(|c| !c.is_empty());
            (m["id"].as_str().unwrap().to_owned(), m["content"].as_str().unwrap_or_default().to_owned(), buttons)
        })
        .collect()
}

/// A post of `n` parts, the `edited` one (from 1) worded differently, with a button.
fn parts_post(n: usize, edited: usize) -> String {
    let parts: Vec<String> =
        (1..=n).map(|i| if i == edited { format!("Part {i}, edited.") } else { format!("Part {i}.") }).collect();
    format!("{}\n\n{{button:profile|Set up your profile}}\n", parts.join("\n<!-- part -->\n"))
}

/// A long post is several messages: edited part by part, grown and shrunk in
/// place, found again when lock.toml is lost, and put back when one is edited
/// or deleted in Discord.
#[tokio::test]
async fn a_long_post_is_several_messages_kept_in_step() {
    let d = fixture();
    let p = d.path();
    write(p, "content/welcome.md", &parts_post(3, 0));
    let mut e = engine(p);
    apply_all(&mut e).await;
    assert_converged(&e).await;
    let ch = e.cfg.desired.lock.channel("general").unwrap();
    let texts = |e: &Engine<FakeDiscord>| -> Vec<(String, bool)> {
        messages_in(e, ch).into_iter().map(|(_, t, b)| (t, b)).collect()
    };
    let owned = |v: &[(&str, bool)]| -> Vec<(String, bool)> { v.iter().map(|(t, b)| ((*t).to_owned(), *b)).collect() };
    assert_eq!(texts(&e), owned(&[("Part 1.", false), ("Part 2.", false), ("Part 3.", true)]), "buttons on the last");
    let post = e.cfg.desired.lock.posts["general"].clone();
    assert_eq!(post.parts.len(), 2);

    // Editing part 2 edits only its message.
    write(p, "content/welcome.md", &parts_post(3, 2));
    e.reload().unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(lines(&plan).contains("part 2 changed: \"Part 2.\" → \"Part 2, edited.\""), "{}", lines(&plan));
    e.rest.take_calls();
    apply_all(&mut e).await;
    assert_eq!(e.rest.take_calls(), [format!("Patch channels/{ch}/messages/{}", post.parts[0])]);
    assert_converged(&e).await;

    // A fourth part: the third loses its buttons, the fourth is posted after it.
    write(p, "content/welcome.md", &parts_post(4, 2));
    e.reload().unwrap();
    e.rest.take_calls();
    apply_all(&mut e).await;
    let calls = e.rest.take_calls();
    assert_eq!(
        calls,
        [format!("Patch channels/{ch}/messages/{}", post.parts[1]), format!("Post channels/{ch}/messages")]
    );
    assert_eq!(texts(&e).last(), Some(&("Part 4.".to_owned(), true)));
    assert_eq!(e.cfg.desired.lock.posts["general"].parts.len(), 3);
    assert_converged(&e).await;

    // Deleted in Discord: it and those after it are posted again, in order.
    let second = e.cfg.desired.lock.posts["general"].parts[0];
    e.rest.state.lock().unwrap().messages.retain(|m| m["id"] != second.to_string());
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(lines(&plan).contains("part 2 changed: (nothing)"), "{}", lines(&plan));
    apply_all(&mut e).await;
    let want = [("Part 1.", false), ("Part 2, edited.", false), ("Part 3.", false), ("Part 4.", true)];
    assert_eq!(texts(&e), owned(&want));
    assert_converged(&e).await;

    // Edited in Discord: drift, put back.
    let third = e.cfg.desired.lock.posts["general"].parts[1];
    {
        let mut st = e.rest.state.lock().unwrap();
        let m = st.messages.iter_mut().find(|m| m["id"] == third.to_string()).unwrap();
        m["content"] = json!("Changed by hand.");
    }
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(lines(&plan).contains("part 3 changed"), "{}", lines(&plan));
    apply_all(&mut e).await;
    assert_converged(&e).await;

    // Down to two: the extra messages are deleted, the buttons move up.
    write(p, "content/welcome.md", &parts_post(2, 2));
    e.reload().unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(lines(&plan).contains("4 parts → 2 parts"), "{}", lines(&plan));
    e.rest.take_calls();
    apply_all(&mut e).await;
    let calls = e.rest.take_calls();
    assert_eq!(calls.iter().filter(|c| c.starts_with("Delete")).count(), 2, "{calls:?}");
    assert_eq!(texts(&e), owned(&[("Part 1.", false), ("Part 2, edited.", true)]));
    assert_converged(&e).await;

    // A member's reply in between doesn't stop a third part being added after it.
    let member = json!({ "id": "9000000", "channel_id": ch.to_string(), "author": { "id": "7" }, "content": "hi" });
    e.rest.state.lock().unwrap().messages.push(member);
    write(p, "content/welcome.md", &parts_post(3, 2));
    e.reload().unwrap();
    apply_all(&mut e).await;
    assert_converged(&e).await;

    // lock.toml lost: every part is found again, nothing posted twice.
    let before = e.cfg.desired.lock.posts["general"].clone();
    fs::remove_file(p.join(LOCK)).unwrap();
    let _ = fs::remove_file(p.join(chipsoc_config::LOCK_BACKUP));
    e.reload().unwrap();
    e.rest.take_calls();
    apply_all(&mut e).await;
    assert_eq!(posts_made(&e), Vec::<String>::new(), "found again, not posted twice");
    assert_eq!(e.cfg.desired.lock.posts["general"], before);
    assert_converged(&e).await;
}

/// A forum's "How to post here" thread and a project's brief: later parts
/// follow in the thread.
#[tokio::test]
async fn long_forum_posts_and_briefs_continue_in_their_thread() {
    let d = fixture();
    let p = d.path();
    let cat = fs::read_to_string(p.join("categories/10-main.toml")).unwrap();
    let cat = cat.replace(
        "tags = [\"active\", \"past\", \"cpu\"]\n",
        "tags = [\"active\", \"past\", \"cpu\"]\npost = \"content/how.md\"\n",
    );
    write(p, "categories/10-main.toml", &cat);
    write(p, "content/how.md", &parts_post(2, 0));
    let projects = fs::read_to_string(p.join("projects.toml")).unwrap();
    write(p, "projects.toml", &projects.replace("kind = \"cpu\"\n", "kind = \"cpu\"\nbrief = \"content/golem.md\"\n"));
    write(p, "content/golem.md", "About it.\n<!-- part -->\nThe plan.\n<!-- part -->\nThe team.\n");
    let mut e = engine(p);
    apply_all(&mut e).await;
    assert_converged(&e).await;

    let how = e.cfg.desired.lock.posts["projects"].clone();
    let texts: Vec<String> = messages_in(&e, how.message).into_iter().map(|(_, t, _)| t).collect();
    assert_eq!(texts, ["Part 1.", "Part 2."]);
    let golem = e.cfg.desired.lock.project_posts["golem"].clone();
    let msgs = messages_in(&e, golem.thread);
    assert_eq!(msgs.len(), 3);
    assert!(msgs[0].1.starts_with("# Golem") && msgs[0].1.ends_with("About it."), "{:?}", msgs[0]);
    assert_eq!((msgs[1].1.as_str(), msgs[1].2), ("The plan.", false));
    assert_eq!((msgs[2].1.as_str(), msgs[2].2), ("The team.", true), "the apply button under the last");
    assert_eq!(golem.parts.len(), 2);

    // Edit the brief's part 2: that message changes (the thread and its first are always sent).
    write(p, "content/golem.md", "About it.\n<!-- part -->\nThe new plan.\n<!-- part -->\nThe team.\n");
    e.reload().unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(lines(&plan).contains("part 2 changed"), "{}", lines(&plan));
    e.rest.take_calls();
    apply_all(&mut e).await;
    let calls = e.rest.take_calls();
    assert!(calls.contains(&format!("Patch channels/{}/messages/{}", golem.thread, golem.parts[0])), "{calls:?}");
    assert!(!calls.iter().any(|c| c.contains(&golem.parts[1].to_string()) || c.starts_with("Post")), "{calls:?}");
    assert_converged(&e).await;

    // Shrink the brief, grow the forum's post.
    write(p, "content/golem.md", "About it, all of it.\n");
    write(p, "content/how.md", &parts_post(3, 0));
    e.reload().unwrap();
    apply_all(&mut e).await;
    assert_converged(&e).await;
    assert_eq!(messages_in(&e, golem.thread).len(), 1);
    assert!(e.cfg.desired.lock.project_posts["golem"].parts.is_empty());
    let texts: Vec<String> = messages_in(&e, how.message).into_iter().map(|(_, t, _)| t).collect();
    assert_eq!(texts, ["Part 1.", "Part 2.", "Part 3."]);

    // Grow the brief again, then lose lock.toml: both found again, part by part.
    write(p, "content/golem.md", "About it.\n<!-- part -->\nThe plan.\n");
    e.reload().unwrap();
    apply_all(&mut e).await;
    let (how, golem) =
        (e.cfg.desired.lock.posts["projects"].clone(), e.cfg.desired.lock.project_posts["golem"].clone());
    assert_eq!((how.parts.len(), golem.parts.len()), (2, 1));
    fs::remove_file(p.join(LOCK)).unwrap();
    let _ = fs::remove_file(p.join(chipsoc_config::LOCK_BACKUP));
    e.reload().unwrap();
    e.rest.take_calls();
    apply_all(&mut e).await;
    assert_eq!(posts_made(&e), Vec::<String>::new(), "found again, not posted twice");
    assert_eq!(e.cfg.desired.lock.posts["projects"], how);
    assert_eq!(e.cfg.desired.lock.project_posts["golem"], golem);
    assert_converged(&e).await;
}

/// Parts that lock.toml says are the post's, but someone else wrote, are
/// never deleted: a lock.toml from the repo can't make the bot delete members' messages.
#[tokio::test]
async fn only_the_bots_own_old_parts_are_deleted() {
    let d = fixture();
    let p = d.path();
    write(p, "content/welcome.md", &parts_post(2, 0));
    let mut e = engine(p);
    apply_all(&mut e).await;
    let ch = e.cfg.desired.lock.channel("general").unwrap();
    let member = json!({ "id": "9000000", "channel_id": ch.to_string(), "author": { "id": "7" }, "content": "mine" });
    e.rest.state.lock().unwrap().messages.push(member);
    let mut lock = e.cfg.desired.lock.clone();
    lock.posts.get_mut("general").unwrap().parts.push(Snowflake(9000000));
    e.cfg.save_lock(&lock).unwrap();
    e.reload().unwrap();
    write(p, "content/welcome.md", "Just one part now.\n");
    e.reload().unwrap();
    apply_all(&mut e).await;
    assert!(messages_in(&e, ch).iter().any(|(id, t, _)| id == "9000000" && t == "mine"), "kept");
    assert!(e.cfg.desired.lock.posts["general"].parts.is_empty(), "and no longer counted as the post's");
    assert_converged(&e).await;
}

/// The fixture's projects forum, with `extra` after its tags (more fields, `[[channel.thread]]`s).
fn forum_with(p: &Path, extra: &str) {
    let cat = fs::read_to_string(p.join("categories/10-main.toml")).unwrap();
    let start = cat.find("[[channel]]\nkey = \"projects\"").unwrap();
    let end = start + cat[start..].find("tags = [\"active\", \"past\", \"cpu\"]\n").unwrap();
    let end = end + "tags = [\"active\", \"past\", \"cpu\"]\n".len();
    let lounge = cat[end..].find("[[channel]]").map_or(cat.len(), |i| end + i);
    let forum = format!("{}{extra}\n", &cat[start..end]);
    write(p, "categories/10-main.toml", &format!("{}{forum}{}", &cat[..start], &cat[lounge..]));
}

fn thread_of(e: &Engine<FakeDiscord>, id: Snowflake) -> Value {
    let st = e.rest.state.lock().unwrap();
    st.channels.iter().find(|c| c["id"] == id.to_string()).unwrap().clone()
}

/// Tag names a thread has, as its forum names them.
fn tags_of(e: &Engine<FakeDiscord>, thread: &Value) -> Vec<String> {
    let forum = e.cfg.desired.lock.channel("projects").unwrap();
    let f = thread_of(e, forum);
    let applied: Vec<&Value> = thread["applied_tags"].as_array().unwrap().iter().collect();
    let mut names: Vec<String> = f["available_tags"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| applied.contains(&&t["id"]))
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

const TOOLCHAIN: &str = "[[channel.thread]]\nkey = \"toolchain\"\ntitle = \"Toolchain: HDL, FPGA and ASIC flows\"\n\
     post = \"content/toolchain.md\"\ntags = [\"cpu\"]\n";

/// A `[[channel.thread]]` is a forum post the bot starts (not pinned) and
/// keeps in step: text part by part, title and tags in place; found again
/// when lock.toml is lost; archived, never deleted, once taken out. It sits
/// alongside the forum's own pinned post.
#[tokio::test]
async fn config_forum_threads_are_kept_in_step_and_archived_when_removed() {
    let d = fixture();
    let p = d.path();
    forum_with(p, &format!("post = \"content/how.md\"\n\n{TOOLCHAIN}"));
    write(p, "content/how.md", "Projects only.\n");
    write(p, "content/toolchain.md", "Verilator, Yosys.\n<!-- part -->\nOpenROAD for {#general}.\n");
    let mut e = engine(p);
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(lines(&plan).contains("+ create forum post projects/toolchain"), "{}", lines(&plan));
    apply_all(&mut e).await;
    assert_converged(&e).await;

    let lock = e.cfg.desired.lock.clone();
    let t = lock.posts["projects/toolchain"].clone();
    let thread = thread_of(&e, t.message);
    assert_eq!(thread["name"], "Toolchain: HDL, FPGA and ASIC flows");
    assert_eq!(thread["parent_id"], lock.channel("projects").unwrap().to_string());
    assert!(thread["flags"].is_null(), "not pinned: {thread}");
    assert_eq!(tags_of(&e, &thread), ["cpu"]);
    let general = lock.channel("general").unwrap();
    let texts: Vec<String> = messages_in(&e, t.message).into_iter().map(|(_, t, _)| t).collect();
    assert_eq!(texts, ["Verilator, Yosys.".to_owned(), format!("OpenROAD for <#{general}>.")]);
    assert_eq!(thread_of(&e, lock.posts["projects"].message)["flags"], json!(2), "the forum's own post stays pinned");

    // Part 2, the title and the tags change: edited in place, part 1's message left alone.
    write(p, "content/toolchain.md", "Verilator, Yosys.\n<!-- part -->\nOpenROAD and KLayout.\n");
    let renamed = TOOLCHAIN.replace("ASIC flows", "ASIC tools").replace("[\"cpu\"]", "[\"cpu\", \"active\"]");
    forum_with(p, &format!("post = \"content/how.md\"\n\n{renamed}"));
    e.reload().unwrap();
    e.rest.take_calls();
    apply_all(&mut e).await;
    let calls = e.rest.take_calls();
    assert!(!calls.iter().any(|c| c.starts_with("Post")), "nothing posted anew: {calls:?}");
    assert!(calls.contains(&format!("Patch channels/{}/messages/{}", t.message, t.parts[0])), "{calls:?}");
    let thread = thread_of(&e, t.message);
    assert_eq!(thread["name"], "Toolchain: HDL, FPGA and ASIC tools");
    assert_eq!(tags_of(&e, &thread), ["active", "cpu"]);
    assert_converged(&e).await;
    assert_eq!(e.cfg.desired.lock.posts["projects/toolchain"], t, "the same messages");

    // lock.toml lost: found again, not posted twice.
    fs::remove_file(p.join(LOCK)).unwrap();
    let _ = fs::remove_file(p.join(chipsoc_config::LOCK_BACKUP));
    e.reload().unwrap();
    e.rest.take_calls();
    apply_all(&mut e).await;
    assert_eq!(posts_made(&e), Vec::<String>::new(), "found again, not posted twice");
    assert_eq!(e.cfg.desired.lock.posts["projects/toolchain"], t);
    assert_converged(&e).await;

    // Taken out of the config: archived and locked, its messages kept.
    forum_with(p, "post = \"content/how.md\"\n");
    e.reload().unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(lines(&plan).contains("archive forum post projects/toolchain"), "{}", lines(&plan));
    e.rest.take_calls();
    apply_all(&mut e).await;
    let calls = e.rest.take_calls();
    assert!(!calls.iter().any(|c| c.starts_with("Delete")), "nothing deleted: {calls:?}");
    let thread = thread_of(&e, t.message);
    assert_eq!((&thread["thread_metadata"]["archived"], &thread["locked"]), (&json!(true), &json!(true)));
    assert_eq!(messages_in(&e, t.message).len(), 2);
    assert!(!e.cfg.desired.lock.posts.contains_key("projects/toolchain"));
    assert_converged(&e).await;
}

/// `[projects] pinned` pins that project's post; moving it unpins the old
/// one first (Discord pins one post per forum), and the plan says so.
#[tokio::test]
async fn the_pinned_project_moves_its_pin() {
    let d = fixture();
    let p = d.path();
    let projects = fs::read_to_string(p.join("projects.toml")).unwrap();
    write(
        p,
        "projects.toml",
        &format!(
            "{projects}\n[[project]]\nslug = \"robot\"\nname = \"Robot\"\nblurb = \"arms\"\nstatus = \"active\"\ntags = [\"cpu\"]\n"
        ),
    );
    let bot = fs::read_to_string(p.join("bot.toml")).unwrap();
    write(p, "bot.toml", &format!("{bot}pinned = \"golem\"\n"));
    let mut e = engine(p);
    apply_all(&mut e).await;
    assert_converged(&e).await;
    let (golem, robot) =
        (e.cfg.desired.lock.project_posts["golem"].thread, e.cfg.desired.lock.project_posts["robot"].thread);
    assert_eq!(thread_of(&e, golem)["flags"], json!(2));
    assert!(thread_of(&e, robot)["flags"].as_u64().unwrap_or(0) & 2 == 0);
    assert_eq!(tags_of(&e, &thread_of(&e, robot)), ["active", "cpu"], "its own tags, and its status");

    write(p, "bot.toml", &format!("{bot}pinned = \"robot\"\n"));
    e.reload().unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    let said = lines(&plan);
    assert!(said.contains("pinned: pinned → not pinned") && said.contains("pinned: not pinned → pinned"), "{said}");
    let (unpin, pin) = (said.find("project post golem").unwrap(), said.find("project post robot").unwrap());
    assert!(unpin < pin, "unpinned first: {said}");
    apply_all(&mut e).await;
    assert_eq!(thread_of(&e, golem)["flags"], json!(0));
    assert_eq!(thread_of(&e, robot)["flags"], json!(2));
    assert_converged(&e).await;

    // Pinned in Discord by hand: drift, put back.
    e.rest.state.lock().unwrap().channels.iter_mut().find(|c| c["id"] == robot.to_string()).unwrap()["flags"] =
        json!(0);
    let plan = e.plan(Options::default()).await.unwrap();
    assert!(lines(&plan).contains("not pinned → pinned"), "{}", lines(&plan));
}

/// A forum's `sort` is sent and kept; unset, Discord's is left alone (and
/// what was applied before it could be set still matches).
#[tokio::test]
async fn a_forums_sort_order_is_kept_only_when_set() {
    let d = fixture();
    let p = d.path();
    let mut e = engine(p);
    apply_all(&mut e).await;
    let forum = e.cfg.desired.lock.channel("projects").unwrap();
    e.rest.state.lock().unwrap().channels.iter_mut().find(|c| c["id"] == forum.to_string()).unwrap()["default_sort_order"] =
        json!(1);
    assert_converged(&e).await;

    forum_with(p, "sort = \"activity\"\n");
    e.reload().unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    assert_eq!(lines(&plan), "~ edit channel projects\n    sort: creation → activity", "{}", lines(&plan));
    match &plan.actions[0] {
        Action::Edit { cause, .. } => assert_ne!(*cause, chipsoc_model::Cause::Discord, "a config change"),
        a => panic!("{a}"),
    }
    e.rest.take_calls();
    apply_all(&mut e).await;
    assert_eq!(e.rest.take_calls(), [format!("Patch channels/{forum}")], "only the forum");
    assert_eq!(thread_of(&e, forum)["default_sort_order"], json!(0));
    assert_converged(&e).await;
}

/// The bot's own apply of a long brief never leaves lock.toml with ids its
/// trusted copy hasn't: a reload in the middle (the bot's watcher, the web
/// UI) found the new parts there and refused them as a pusher's repointing.
#[tokio::test]
async fn mid_apply_lock_toml_never_runs_ahead_of_the_trusted_lock() {
    let d = fixture();
    let p = d.path().to_owned();
    let kept = Arc::new(Kept::default());
    let mut e = Engine::load(FakeDiscord::new(), &p).unwrap().trusting(kept.clone()).await.unwrap();
    apply_all(&mut e).await;
    let projects = fs::read_to_string(p.join("projects.toml")).unwrap();
    write(&p, "projects.toml", &projects.replace("kind = \"cpu\"\n", "kind = \"cpu\"\nbrief = \"content/golem.md\"\n"));
    write(&p, "content/golem.md", "About it.\n<!-- part -->\nThe plan.\n<!-- part -->\nThe team.\n");
    e.refresh().await.unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    let mut checked = 0;
    let (dir, k) = (p.clone(), kept.clone());
    let mut each = |a: &Action, _: &Result<(), String>| {
        let file: Lock = toml::from_str(&fs::read_to_string(dir.join(LOCK)).unwrap()).unwrap();
        let trusted = k.0.lock().unwrap().lock.clone().unwrap();
        let r = chipsoc_config::trust::review(&file, &trusted);
        assert!(r.refused.is_empty(), "after {a}: {r:?}");
        assert_eq!(file.project_posts, trusted.project_posts, "after {a}");
        checked += 1;
    };
    e.apply(&plan, &mut each).await.unwrap();
    assert!(checked > 0);
    let golem = e.cfg.desired.lock.project_posts["golem"].clone();
    assert_eq!(golem.parts.len(), 2);
    assert_eq!(kept.0.lock().unwrap().lock.as_ref().unwrap().project_posts["golem"], golem);
    assert!(e.vetted.lock.is_clean(), "{:?}", e.vetted.lock);

    // Before: parts on disk, not yet in the trusted copy. Now they don't count.
    kept.0.lock().unwrap().lock.as_mut().unwrap().project_posts.get_mut("golem").unwrap().parts.clear();
    e.refresh().await.unwrap();
    assert!(e.vetted.lock.is_clean(), "{:?}", e.vetted.lock);
    // A pushed lock moving the post to another thread is still refused.
    edit_lock(&p, |l| l.project_posts.get_mut("golem").unwrap().thread = Snowflake(4242));
    e.refresh().await.unwrap();
    assert_eq!(e.vetted.lock.refused, ["project_posts.golem"]);
    assert_eq!(e.cfg.desired.lock.project_posts["golem"].thread, golem.thread);
}

/// Renaming a project's forum post leaves Discord's "changed the post
/// title" notice in it; the bot deletes its own, now and (for older ones)
/// in a sweep, never anyone else's.
#[tokio::test]
async fn the_bots_thread_rename_notices_are_deleted() {
    let d = fixture();
    let p = d.path();
    let mut e = engine(p);
    apply_all(&mut e).await;
    let thread = e.cfg.desired.lock.project_posts["golem"].thread;
    let notices = |e: &Engine<FakeDiscord>| -> Vec<String> {
        let st = e.rest.state.lock().unwrap();
        let mine = st.messages.iter().filter(|m| m["channel_id"] == thread.to_string() && m["type"] == 4);
        mine.map(|m| m["author"]["id"].as_str().unwrap_or_default().to_owned()).collect()
    };
    let projects = fs::read_to_string(p.join("projects.toml")).unwrap();
    write(p, "projects.toml", &projects.replace("name = \"Golem\"", "name = \"Golem II\""));
    e.reload().unwrap();
    e.rest.take_calls();
    apply_all(&mut e).await;
    let calls = e.rest.take_calls();
    assert!(calls.contains(&format!("Patch channels/{thread}")), "{calls:?}");
    assert!(calls.iter().any(|c| c.starts_with(&format!("Delete channels/{thread}/messages/"))), "{calls:?}");
    assert!(notices(&e).is_empty());
    assert_converged(&e).await;

    // Only its tags change: the title doesn't, so nothing to delete.
    let past = projects.replace("name = \"Golem\"", "name = \"Golem II\"").replace("\"active\"", "\"past\"");
    write(p, "projects.toml", &past);
    e.reload().unwrap();
    e.rest.take_calls();
    apply_all(&mut e).await;
    let calls = e.rest.take_calls();
    assert!(calls.contains(&format!("Patch channels/{thread}")), "{calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with(&format!("Delete channels/{thread}/"))), "{calls:?}");

    // Notices from before: the bot's go, someone else's stay.
    {
        let mut st = e.rest.state.lock().unwrap();
        for (n, who) in [(900001, "42"), (900002, "7")] {
            let m = json!({ "id": n.to_string(), "channel_id": thread.to_string(), "type": 4, "content": "x", "author": { "id": who } });
            st.messages.push(m);
        }
    }
    let gone = chipsoc_discord::exec::clear_all_rename_notices(&e.rest, &e.cfg.desired.lock).await.unwrap();
    assert_eq!(gone, 1);
    assert_eq!(notices(&e), ["7"]);
}
