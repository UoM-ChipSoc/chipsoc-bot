//! The queue and the rest on a fake Discord, an in-memory store and a temp config.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use chipsoc_config::{Config, Snowflake};
use chipsoc_discord::fake::FakeDiscord;
use chipsoc_discord::{Method, Rest};
use chipsoc_engine::{Engine, Options};
use chipsoc_store::{AppKind, Store};
use serde_json::json;

use super::*;

/// The fake, shared between the host and the engines it makes.
type Shared = Arc<FakeDiscord>;

struct TestHost {
    discord: Shared,
    store: Store,
    cfg: Mutex<Arc<Config>>,
    writable: bool,
    _dir: tempfile::TempDir,
}

impl Host for TestHost {
    type Rest = Shared;

    fn rest(&self) -> &Shared {
        &self.discord
    }

    fn db(&self) -> chipsoc_store::Guild<'_> {
        self.store.guild("1000")
    }

    fn cfg(&self) -> Arc<Config> {
        self.cfg.lock().unwrap().clone()
    }

    fn guild(&self) -> Snowflake {
        Snowflake(1000)
    }

    fn saves(&self) -> bool {
        self.writable
    }

    fn unwritable(&self) -> Option<String> {
        (!self.writable).then(|| "not saved here".to_owned())
    }

    async fn record(
        &self,
        ops: &[chipsoc_config::ops::Op],
        user: Snowflake,
        roles: &[String],
    ) -> Result<crate::Recorded> {
        let mut e = Engine::load(self.discord.clone(), &self.cfg().dir)?;
        let text = record_with(&mut e, &chipsoc_engine::By::bot(), ops, user, roles).await;
        *self.cfg.lock().unwrap() = e.cfg.clone();
        text
    }
}

const REVIEWER: u64 = 9;
const ADA: u64 = 7;
const BEN: u64 = 8;

fn write(dir: &Path, file: &str, body: &str) {
    let p = dir.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn write_config(p: &Path) {
    write(p, "server.toml", "[guild]\nname = \"Test\"\n");
    write(
        p,
        "bot.toml",
        "name = \"Test\"\nadmin_role = \"committee\"\n[log]\npeople = \"log\"\n[review]\nchannel = \"review\"\n\
         [feeds]\nchannel = \"feeds\"\n[ideas]\nforum = \"ideas\"\n[resources]\nforum = \"resources\"\n\
         [projects]\nforum = \"projects\"\n\
         [[position]]\nrole = \"treasurer\"\nadmin = true\n[[position]]\nrole = \"secretary\"\n",
    );
    write(
        p,
        "roles.toml",
        "[[role]]\nkey = \"bot\"\nname = \"Bot\"\nmanaged = true\n\n[[role]]\nkey = \"committee\"\nname = \"Committee\"\n\n\
         [[role]]\nkey = \"treasurer\"\nname = \"Treasurer\"\n\n[[role]]\nkey = \"secretary\"\nname = \"Secretary\"\n\n\
         [[role]]\nkey = \"everyone\"\nname = \"@everyone\"\npermissions = [\"VIEW_CHANNEL\"]\n",
    );
    let forum = |k: &str, tags: &str| {
        format!("[[channel]]\nkey = \"{k}\"\ntype = \"forum\"\nname = \"{k}\"\ntags = [{tags}]\n\n")
    };
    write(
        p,
        "categories/10-main.toml",
        &format!(
            "[category]\nkey = \"main\"\nname = \"Main\"\n\n[[channel]]\nkey = \"review\"\nname = \"review\"\n\n\
             [[channel]]\nkey = \"log\"\nname = \"log\"\n\n[[channel]]\nkey = \"feeds\"\nname = \"feeds\"\n\n{}{}{}",
            forum("ideas", "\"cpu\", \"needs-lead\""),
            forum("resources", "\"books\", \"papers\""),
            forum("projects", "\"active\""),
        ),
    );
    write(p, "members.toml", "# who holds what\ntreasurer = []\n");
    write(
        p,
        "projects.toml",
        "[[project]]\nslug = \"golem\"\nname = \"Golem\"\nstatus = \"active\"\nchannels = [\"general\"]\n\
         [project.application]\nopen = true\nlead_question = true\n",
    );
}

/// A host on a server the config has been applied to, with Ada, Ben and the reviewer in it.
async fn host(writable: bool) -> TestHost {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path());
    let discord = Arc::new(FakeDiscord::new());
    for user in [ADA, BEN, REVIEWER] {
        discord.join(user);
    }
    let mut e = Engine::load(discord.clone(), dir.path()).unwrap();
    let plan = e.plan(Options::default()).await.unwrap();
    e.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
    let cfg = Mutex::new(Arc::new(Config::load(dir.path()).unwrap()));
    let store = Store::open("sqlite::memory:").await.unwrap();
    TestHost { discord, store, cfg, writable, _dir: dir }
}

fn reviewer() -> Actor {
    Actor { id: Some(Snowflake(REVIEWER)), name: "rev".into() }
}

impl TestHost {
    fn holds(&self, user: u64, key: &str) -> bool {
        let id = self.cfg().desired.lock.role(key).unwrap().to_string();
        self.discord.roles_of(user).contains(&id)
    }

    async fn apply_for(&self, kind: AppKind, target: &str, user: u64, lead: bool) -> i64 {
        let answers = Answer::to_json(&[Answer::new("Why?", Some(&"fun".to_owned()))]);
        self.db().add_application(kind, target, &user.to_string(), &answers, lead, 5).await.unwrap()
    }

    /// A review card for the application, as the bot posts it.
    async fn card(&self, id: i64) -> String {
        let ch = self.cfg().desired.lock.channel("review").unwrap().to_string();
        let body = json!({ "content": "**Card**", "components": [{ "type": 1 }] });
        let m = self.rest().call(Method::Post, &format!("channels/{ch}/messages"), Some(body)).await.unwrap();
        let msg = m["id"].as_str().unwrap().to_owned();
        self.db().set_card(id, &ch, &msg).await.unwrap();
        format!("channels/{ch}/messages/{msg}")
    }
}

#[tokio::test]
async fn accepting_a_position_records_it_in_members_toml() {
    let h = host(true).await;
    let id = h.apply_for(AppKind::Position, "treasurer", ADA, false).await;
    let queue = pending(&h).await.unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!((queue[0].kind.as_str(), queue[0].what.as_str()), ("position", "Treasurer"));
    assert_eq!(queue[0].answers, [Answer::new("Why?", Some(&"fun".to_owned()))]);
    let card = h.card(id).await;

    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.outcome.contains("Recorded in the config") && d.outcome.contains("Gave them **Treasurer**"), "{d:?}");
    assert!(h.holds(ADA, "treasurer") && h.holds(ADA, "committee"), "admin positions get the admin role too");
    let members = fs::read_to_string(h.cfg().dir.join("members.toml")).unwrap();
    assert!(members.starts_with("# who holds what") && members.contains("\"7\""), "{members}");
    assert!(pending(&h).await.unwrap().is_empty());

    let msg = h.rest().call(Method::Get, &card, None).await.unwrap();
    assert_eq!(msg["content"], format!("**Card**\n✅ **Accepted by <@9>.** {}", d.outcome.markdown()));
    assert_eq!(msg["components"], json!([]), "buttons taken away");
    assert!(h.discord.dms_to(ADA)[0].contains("was accepted"));
    assert_eq!(activity(&h, 5).await.unwrap()[0].action, "accepted");
    assert_eq!(decide(&h, id, false, &reviewer()).await.unwrap_err(), "Someone already accepted this.");
}

#[tokio::test]
async fn managed_roles_are_not_given_behind_the_config_s_back() {
    // members.toml decides who's treasurer (and so on the committee); it can't be saved here,
    // and drift watch would take a role given directly straight back: nothing given, still pending.
    let h = host(false).await;
    let id = h.apply_for(AppKind::Position, "treasurer", ADA, false).await;
    let card = h.card(id).await;
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.pending && !d.accepted && d.outcome.contains("Didn't give **Treasurer**"), "{d:?}");
    assert!(!h.holds(ADA, "treasurer") && !h.holds(ADA, "committee"));
    assert_eq!(pending(&h).await.unwrap().len(), 1, "back in the queue, to try again");
    let msg = h.rest().call(Method::Get, &card, None).await.unwrap();
    assert_eq!(msg["components"], json!([{ "type": 1 }]), "the card keeps its buttons");
    assert!(h.discord.dms_to(ADA).is_empty(), "the applicant isn't told it went through");

    // Drift watch only reporting: the next apply would still take it back, so not given either.
    let h = host(false).await;
    let dir = h.cfg().dir.clone();
    let bot = fs::read_to_string(dir.join("bot.toml")).unwrap();
    write(&dir, "bot.toml", &format!("{bot}[drift]\nmode = \"report\"\n"));
    *h.cfg.lock().unwrap() = Arc::new(Config::load(&dir).unwrap());
    let id = h.apply_for(AppKind::Position, "treasurer", ADA, false).await;
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.pending && !d.accepted && d.outcome.contains("the next apply would take it back"), "{d:?}");
    assert!(!h.holds(ADA, "treasurer") && !h.holds(ADA, "committee"));

    // A role members.toml doesn't list is the config's business only to give.
    let h = host(true).await;
    let id = h.apply_for(AppKind::Position, "secretary", BEN, false).await;
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.outcome.contains("the config doesn't say who holds Secretary"), "{d:?}");
    assert!(h.holds(BEN, "secretary") && !h.holds(BEN, "committee"));
}

#[tokio::test]
async fn a_team_role_made_powerful_in_discord_is_not_handed_out() {
    let h = host(true).await;
    let team = h.cfg().desired.lock.role("proj-golem-team").unwrap().to_string();
    for r in h.discord.state.lock().unwrap().roles.iter_mut().filter(|r| r["id"] == team) {
        r["permissions"] = json!("8");
    }
    let id = h.apply_for(AppKind::Project, "golem", BEN, false).await;
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.pending && d.outcome.contains("moderator or admin permissions"), "{d:?}");
    assert!(!h.holds(BEN, "proj-golem-team"));
}

#[tokio::test]
async fn project_applicants_join_the_team_and_can_be_made_leads() {
    let h = host(true).await;
    let id = h.apply_for(AppKind::Project, "golem", BEN, true).await;
    assert_eq!(make_lead(&h, id, &reviewer()).await.unwrap_err(), "That isn't an accepted project application.");
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.lead && d.outcome.contains("Gave them **Golem Team**") && d.outcome.contains("Make lead"), "{d:?}");
    assert!(h.holds(BEN, "proj-golem-team"));

    let text = make_lead(&h, id, &reviewer()).await.unwrap();
    assert!(text.contains("Added to the project's leads"), "{text}");
    assert!(h.holds(BEN, "proj-golem-lead"));
    assert_eq!(h.cfg().projects[0].leads, [Snowflake(BEN)]);
}

#[tokio::test]
async fn make_lead_is_for_those_who_asked_and_new_leads_are_told() {
    let h = host(true).await;
    let id = h.apply_for(AppKind::Project, "golem", ADA, false).await;
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(!d.lead && !d.outcome.contains("Make lead"), "they only asked to join: {d:?}");
    let e = make_lead(&h, id, &reviewer()).await.unwrap_err();
    assert!(e.contains("didn't ask to lead") && e.contains("/server project lead"), "{e}");
    assert!(!h.holds(ADA, "proj-golem-lead"));

    // An admin can still make them one from the project.
    let text = add_project_lead(&h, "golem", Snowflake(ADA), &reviewer()).await.unwrap();
    assert!(h.holds(ADA, "proj-golem-lead") && !text.contains("Couldn't DM"), "{text}");
    assert_eq!(
        h.discord.dms_to(ADA).last().unwrap(),
        "⭐ You're now a lead of **Golem** in Test. Thanks for taking it on!"
    );

    // Asked to lead: Make lead, and they're told too.
    let id = h.apply_for(AppKind::Project, "golem", BEN, true).await;
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.lead && d.outcome.contains("⭐ They'd like to lead it. Press **Make lead**"), "{d:?}");
    make_lead(&h, id, &reviewer()).await.unwrap();
    assert!(h.discord.dms_to(BEN).last().unwrap().starts_with("⭐ You're now a lead of **Golem**"));
}

#[tokio::test]
async fn one_application_waits_at_a_time_and_none_for_what_they_hold() {
    let h = host(true).await;
    assert_eq!(already(&h, AppKind::Position, "secretary", Snowflake(ADA)).await, None);
    let id = h.apply_for(AppKind::Position, "secretary", ADA, false).await;
    let twice = already(&h, AppKind::Position, "secretary", Snowflake(ADA)).await.unwrap();
    assert!(twice.contains("**Secretary**") && twice.contains("still waiting") && twice.contains("<t:5:D>"), "{twice}");
    assert_eq!(already(&h, AppKind::Position, "secretary", Snowflake(BEN)).await, None, "someone else");
    assert_eq!(already(&h, AppKind::Position, "treasurer", Snowflake(ADA)).await, None, "another position");
    assert_eq!(already(&h, AppKind::Project, "golem", Snowflake(ADA)).await, None, "a project");

    decide(&h, id, true, &reviewer()).await.unwrap();
    assert_eq!(
        already(&h, AppKind::Position, "secretary", Snowflake(ADA)).await.as_deref(),
        Some("You already hold **Secretary**."),
        "decided, and now they hold it"
    );
    let id = h.apply_for(AppKind::Project, "golem", BEN, false).await;
    decide(&h, id, true, &reviewer()).await.unwrap();
    let on = already(&h, AppKind::Project, "golem", Snowflake(BEN)).await.unwrap();
    assert_eq!(on, "You're already on the **Golem** team.");

    // Rejected: they may apply again.
    let id = h.apply_for(AppKind::Position, "treasurer", BEN, false).await;
    decide(&h, id, false, &reviewer()).await.unwrap();
    assert_eq!(already(&h, AppKind::Position, "treasurer", Snowflake(BEN)).await, None);
}

#[tokio::test]
async fn rejecting_tells_the_applicant() {
    let h = host(true).await;
    let id = h.apply_for(AppKind::Position, "secretary", ADA, false).await;
    let d = decide(&h, id, false, &reviewer()).await.unwrap();
    assert!(!d.accepted && d.outcome.is_empty() && !h.holds(ADA, "secretary"));
    assert!(h.discord.dms_to(ADA)[0].contains("wasn't taken up"));
    assert_eq!(claim(&h, id, true, &reviewer()).await.unwrap_err(), "Someone already rejected this.");
    assert_eq!(claim(&h, 999, true, &reviewer()).await.unwrap_err(), "That application no longer exists.");
}

#[tokio::test]
async fn feed_suggestions_are_fetched_safely_and_can_be_removed() {
    let h = host(true).await;
    let id = h.apply_for(AppKind::Feed, "http://127.0.0.1/feed", ADA, false).await;
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert_eq!(d.outcome, "⚠️ Couldn't add the feed: that address points inside a private network.");
    assert!(h.discord.dms_to(ADA).is_empty(), "not told while it's waiting again");
    decide(&h, id, false, &reviewer()).await.unwrap();
    assert!(h.discord.dms_to(ADA)[0].starts_with("The feed you suggested, http://127.0.0.1/feed, wasn't added"));

    let f = h.db().add_feed("https://example.org/feed", "feeds", "9", 3).await.unwrap();
    let list = feeds(&h).await.unwrap();
    assert_eq!((list.len(), list[0].channel.as_str(), list[0].added_at), (1, "feeds", 3));
    assert!(remove_feed(&h, f, &reviewer()).await.unwrap());
    assert!(!remove_feed(&h, f, &reviewer()).await.unwrap());
    assert!(feeds(&h).await.unwrap().is_empty());
    assert_eq!(activity(&h, 1).await.unwrap()[0].target.as_deref(), Some("https://example.org/feed"));
}

#[test]
fn older_answers_still_read() {
    let parsed = Answer::parse(r#"{"course":"CS","why":"fun"}"#);
    assert_eq!(parsed, [Answer::new("course", Some(&"CS".into())), Answer::new("why", Some(&"fun".into()))]);
    assert!(Answer::parse("not json").is_empty());
}

#[tokio::test]
async fn resources_are_posted_and_listed() {
    let h = host(true).await;
    let r = NewResource {
        title: "Digital Design".into(),
        url: "https://example.org/book".into(),
        description: "A good book.".into(),
        tags: vec!["Books".into()],
    };
    let post = post_resource(&h, &r, &reviewer()).await.unwrap();
    assert_eq!((post.title.as_str(), post.tags.clone()), ("Digital Design", vec!["books".to_owned()]));
    let list = resources(&h, 10).await.unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].content.contains("https://example.org/book") && list[0].content.contains("Shared by <@9>"));
    assert_eq!(list[0].tags, ["books"]);
    let bad = NewResource { url: "file:///etc/passwd".into(), ..r.clone() };
    assert!(post_resource(&h, &bad, &reviewer()).await.is_err());
    let unknown = NewResource { url: "https://example.org/other".into(), tags: vec!["nope".into()], ..r };
    let e = post_resource(&h, &unknown, &reviewer()).await.unwrap_err();
    assert_eq!(e.to_string(), "There's no tag `nope`. Pick from: books, papers.");
}

fn shared(url: &str) -> NewResource {
    NewResource {
        title: "Book".into(),
        url: url.into(),
        description: "Worth reading.".into(),
        tags: vec!["papers".into()],
    }
}

/// What a refusal says (refusals, not Discord failures).
fn refused(e: anyhow::Error) -> String {
    e.downcast_ref::<Refused>().map(ToString::to_string).unwrap_or_else(|| panic!("not a refusal: {e:#}"))
}

#[tokio::test]
async fn shared_resources_are_checked_before_they_reach_the_queue() {
    let h = host(true).await;
    assert!(vet_resource(&h, &shared("https://example.org/a")).await.is_ok());
    let plain = vet_resource(&h, &shared("http://example.org/a")).await.unwrap_err();
    assert_eq!(refused(plain), "Give a link starting https://.");
    let silent = NewResource { description: " ".into(), ..shared("https://example.org/a") };
    assert!(refused(vet_resource(&h, &silent).await.unwrap_err()).contains("what it is"));
    let private = vet_resource(&h, &shared("https://127.0.0.1/a")).await.unwrap_err();
    assert!(refused(private).contains("private network"));
    let tagged = NewResource { tags: vec!["memes".into()], ..shared("https://example.org/a") };
    assert!(refused(vet_resource(&h, &tagged).await.unwrap_err()).contains("Pick from: books, papers"));

    // Already posted (however the address is written): refused, saying where.
    let post = post_resource(&h, &shared("https://example.org/a"), &reviewer()).await.unwrap();
    let again = vet_resource(&h, &shared("https://www.EXAMPLE.org/a/#top")).await.unwrap_err();
    assert_eq!(refused(again), format!("That link has already been shared: {}", post.link));
    let admin = post_resource(&h, &shared("http://example.org/a"), &reviewer()).await.unwrap_err();
    assert!(refused(admin).contains("already been shared"), "http or https, the same page");

    // Waiting for review: refused too.
    let r = shared("https://example.org/b");
    let answers = Answer::to_json(&r.answers());
    h.db().add_application(AppKind::Resource, &r.url, &BEN.to_string(), &answers, false, 5).await.unwrap();
    assert!(refused(vet_resource(&h, &shared("https://example.org/b/")).await.unwrap_err()).contains("waiting"));
}

#[tokio::test]
async fn accepted_resources_are_posted_crediting_whoever_shared_them() {
    let h = host(true).await;
    let r = shared("https://example.org/c");
    let answers = Answer::to_json(&r.answers());
    let id = h.db().add_application(AppKind::Resource, &r.url, &ADA.to_string(), &answers, false, 5).await.unwrap();
    let queue = pending(&h).await.unwrap();
    assert_eq!((queue[0].kind.as_str(), queue[0].what.as_str()), ("resource", "Book"));
    assert!(resources(&h, 10).await.unwrap().is_empty(), "nothing posted before it's accepted");
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.accepted && d.outcome.starts_with("Posted: https://discord.com/channels/"), "{d:?}");
    let list = resources(&h, 10).await.unwrap();
    assert_eq!((list[0].title.as_str(), list[0].tags.clone()), ("Book", vec!["papers".to_owned()]));
    assert!(list[0].content.contains("Worth reading.") && list[0].content.contains("Shared by <@7>"), "{list:?}");
    let dm = &h.discord.dms_to(ADA)[0];
    assert!(dm.contains("**Book**, was posted"), "{dm}");
    assert!(dm.contains(": https://discord.com/channels/"), "{dm}");
}

#[tokio::test]
async fn ideas_wait_for_review_and_rejections_say_why() {
    let h = host(true).await;
    let idea = NewIdea {
        name: "Tiny GPU".into(),
        blurb: "a GPU in a weekend".into(),
        area: "CPU".into(),
        scope: "Shaders, [rasteriser](https://evil.example)\n# Big".into(),
        lead: Lead::Help,
        more: Vec::new(),
    };
    assert_eq!(NewIdea::from_answers(&idea.answers()), idea, "kept in the queue as answers, and back");
    let before = [Answer::new("Who'd lead it?", Some(&Lead::Lead.label().to_owned()))];
    assert_eq!(NewIdea::from_answers(&before).lead, Lead::Lead, "ideas kept under the old question still read");
    let answers = Answer::to_json(&idea.answers());
    let id = h.db().add_application(AppKind::Idea, &idea.name, &ADA.to_string(), &answers, false, 5).await.unwrap();
    assert!(ideas(&h, 10).await.unwrap().is_empty(), "not posted until accepted");
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.accepted && d.outcome.starts_with("Posted: "), "{d:?}");
    let list = ideas(&h, 10).await.unwrap();
    assert_eq!(list[0].title, "Tiny GPU — a GPU in a weekend");
    assert_eq!(list[0].tags, ["cpu"], "its area; no needs-lead tag unless bot.toml names one");
    assert!(list[0].content.starts_with("**Area:** CPU\nProposed by <@7>"), "{}", list[0].content);
    assert!(list[0].content.contains("Looking for someone to lead it."));
    assert!(
        list[0].content.contains("Shaders, \\[rasteriser\\]\\(https://evil.example)\n\\# Big"),
        "their words as written, no masked link or heading: {}",
        list[0].content
    );
    let dm = &h.discord.dms_to(ADA)[0];
    assert!(dm.contains("Your project idea **Tiny GPU** was posted") && dm.contains(": https://"), "{dm}");

    let id = h.db().add_application(AppKind::Idea, "Again", &BEN.to_string(), &answers, false, 6).await.unwrap();
    decide_with(&h, id, false, Some("We have one like it"), &reviewer()).await.unwrap();
    let dm = &h.discord.dms_to(BEN)[0];
    assert_eq!(dm, "Your project idea **Again** wasn't posted in Test this time.\n> We have one like it");
    assert_eq!(ideas(&h, 10).await.unwrap().len(), 1, "nothing posted");
}

/// `[ideas] questions` asks more (or other) questions: the answers are kept
/// under their keys, shown on the card, and in the post, laid out by the bot
/// or by `[ideas] post`.
#[tokio::test]
async fn an_idea_form_of_its_own_is_answered_in_the_post() {
    use chipsoc_config::bot::IdeaQuestion;
    let h = host(true).await;
    let budget = IdeaQuestion { key: "budget".into(), question: chipsoc_config::bot::Question::new("Budget?") };
    let questions: Vec<IdeaQuestion> =
        ["name", "scope", "lead"].iter().filter_map(|k| IdeaQuestion::own(k)).chain([budget]).collect();
    h.edit(|c| c.bot.ideas.as_mut().unwrap().questions = questions.clone());
    let idea = NewIdea {
        name: "Tiny GPU".into(),
        scope: "Shaders".into(),
        lead: Lead::Lead,
        more: vec![Answer::keyed("budget", "Budget?", "£40, [cheap](https://evil.example)")],
        ..Default::default()
    };
    let answers = idea.answers_for(&questions);
    let asked: Vec<&str> = answers.iter().map(|a| a.question.as_str()).collect();
    assert_eq!(asked, ["Name", "What would it do?", "Would you like to lead it?", "Budget?"], "in the form's order");
    assert_eq!(NewIdea::from_answers(&answers), idea, "and back, by key");

    let (kept, ada) = (Answer::to_json(&answers), ADA.to_string());
    let id = h.db().add_application(AppKind::Idea, &idea.name, &ada, &kept, false, 5).await.unwrap();
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.accepted, "{d:?}");
    let post = &ideas(&h, 10).await.unwrap()[0];
    assert_eq!(post.title, "Tiny GPU", "no blurb asked: the name alone");
    let text = &post.content;
    assert!(
        text.contains("**Budget?**\n£40, \\[cheap\\]\\(https://evil.example)"),
        "the added answer, escaped: {text}"
    );
    assert!(text.contains("<@7> would like to lead it."), "{text}");

    // With a template of its own.
    std::fs::create_dir_all(h.cfg().dir.join("content")).unwrap();
    std::fs::write(
        h.cfg().dir.join("content/idea.md"),
        "{by} proposes: {answer:scope}\nBudget: {answer:budget}\n{lead} {x}",
    )
    .unwrap();
    h.edit(|c| c.bot.ideas.as_mut().unwrap().post = Some("content/idea.md".into()));
    let id = h.db().add_application(AppKind::Idea, "Again", &ada, &kept, false, 6).await.unwrap();
    decide(&h, id, true, &reviewer()).await.unwrap();
    let post = &ideas(&h, 10).await.unwrap()[0];
    assert_eq!(
        post.content,
        "<@7> proposes: Shaders\nBudget: £40, \\[cheap\\]\\(https://evil.example)\n<@7> would like to lead it. {x}"
    );
    let unknown = chipsoc_config::template::unknown_idea_placeholders(
        "{by} {answer:budget} {answer:nope} {x} {a b}",
        &["budget"],
    );
    assert_eq!(unknown, ["{answer:nope}", "{x}"]);
}

#[tokio::test]
async fn ideas_are_listed_newest_first_archived_too() {
    let h = host(true).await;
    let forum = h.cfg().desired.lock.channel("ideas").unwrap();
    for name in ["old idea", "new idea"] {
        let body = json!({ "name": name, "message": { "content": format!("{name}!") } });
        h.rest().call(Method::Post, &format!("channels/{forum}/threads"), Some(body)).await.unwrap();
    }
    {
        let mut st = h.discord.state.lock().unwrap();
        let old = st.channels.iter_mut().find(|c| c["name"] == "old idea").unwrap();
        old["thread_metadata"]["archived"] = json!(true);
    }
    let list = ideas(&h, 10).await.unwrap();
    let titles: Vec<(&str, bool)> = list.iter().map(|p| (p.title.as_str(), p.archived)).collect();
    assert_eq!(titles, [("new idea", false), ("old idea", true)]);
    assert_eq!(list[1].content, "old idea!");
    assert!(resources(&h, 10).await.unwrap().is_empty(), "only that forum's posts");
}

mod join {
    use chipsoc_config::join::Answers;

    use super::*;
    use crate::join::{confirm, notify, notify_targets};

    const FLOW: &str = "access_role = \"access\"\n\
        [[step]]\nkey = \"uni\"\nquestion = \"Where?\"\n\
        [[step.option]]\nlabel = \"Here\"\nroles = [\"ours\"]\nnext = \"year\"\n\
        [[step.option]]\nlabel = \"Working\"\nroles = [\"industry\"]\n[[step.option.ask]]\nkey = \"company\"\nlabel = \"Where?\"\n\
        [[step]]\nkey = \"year\"\nquestion = \"Year?\"\n\
        [[step.option]]\nlabel = \"Year 1\"\nroles = [\"y1\"]\n[[step.option]]\nlabel = \"Graduated\"\nroles = [\"alumni\"]\n";

    async fn join_host() -> TestHost {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        write_config(p);
        let roles = fs::read_to_string(p.join("roles.toml")).unwrap();
        let extra: String = ["access", "ours", "industry", "y1", "alumni", "partner"]
            .iter()
            .map(|k| format!("\n[[role]]\nkey = \"{k}\"\nname = \"{k}\"\n"))
            .collect();
        write(p, "roles.toml", &format!("{roles}{extra}"));
        write(p, "join.toml", FLOW);
        write(
            p,
            "federation.toml",
            "name = \"us\"\ninbox = \"log\"\n[[peer]]\nname = \"S\"\nid = \"s\"\npublic_key = \"k\"\n[[peer.role_map]]\ntheirs = \"c\"\nours = \"partner\"\n",
        );
        let report = chipsoc_config::authoring::check(p).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let discord = Arc::new(FakeDiscord::new());
        for user in [ADA, BEN, REVIEWER] {
            discord.join(user);
        }
        let mut e = Engine::load(discord.clone(), p).unwrap();
        let plan = e.plan(Options::default()).await.unwrap();
        e.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
        let cfg = Mutex::new(Arc::new(Config::load(p).unwrap()));
        let store = Store::open("sqlite::memory:").await.unwrap();
        TestHost { discord, store, cfg, writable: true, _dir: dir }
    }

    fn answers(picks: &[(&str, &str)], fields: &[(&str, &str, &str)]) -> Answers {
        let mut a = Answers::default();
        for (step, opt) in picks {
            a.0.entry(step.to_string()).or_default().options.push(opt.to_string());
        }
        for (step, k, v) in fields {
            a.0.entry(step.to_string()).or_default().fields.insert(k.to_string(), v.to_string());
        }
        a
    }

    fn give(h: &TestHost, user: u64, key: &str) {
        let id = h.cfg().desired.lock.role(key).unwrap().to_string();
        let uid = user.to_string();
        let mut st = h.discord.state.lock().unwrap();
        let m = st.members.iter_mut().find(|m| m["user"]["id"] == uid.as_str()).unwrap();
        m["roles"].as_array_mut().unwrap().push(json!(id));
    }

    #[tokio::test]
    async fn confirming_gives_the_answers_roles_and_takes_old_ones() {
        let h = join_host().await;
        give(&h, ADA, "treasurer");
        give(&h, ADA, "partner");
        let first = confirm(&h, Snowflake(ADA), &answers(&[("uni", "here"), ("year", "year-1")], &[])).await.unwrap();
        assert!(first.problems.is_empty(), "{:?}", first.problems);
        for k in ["ours", "y1", "access", "treasurer", "partner"] {
            assert!(h.holds(ADA, k), "{k}");
        }
        let saved = h.db().join_record(&ADA.to_string()).await.unwrap().unwrap();
        assert!(saved.finished_at.is_some());
        assert_eq!(Answers::parse(&saved.answers).0["year"].options, ["year-1"]);

        // A year on, they've graduated and left for a job: the uni and year roles go.
        let later = answers(&[("uni", "working")], &[("uni", "company", "Arm")]);
        let second = confirm(&h, Snowflake(ADA), &later).await.unwrap();
        assert_eq!(second.outcome.fields.get("company").map(String::as_str), Some("Arm"));
        for (k, held) in [("industry", true), ("ours", false), ("y1", false), ("access", true)] {
            assert_eq!(h.holds(ADA, k), held, "{k}");
        }
        assert!(h.holds(ADA, "treasurer") && h.holds(ADA, "partner"), "roles the flow doesn't give are never taken");
        let saved = Answers::parse(&h.db().join_record(&ADA.to_string()).await.unwrap().unwrap().answers);
        assert!(!saved.0.contains_key("year"), "the old branch's answers are dropped");
    }

    #[tokio::test]
    async fn the_owner_and_higher_ranks_are_not_renamed() {
        use crate::join::{Unnameable, unnameable};
        let h = join_host().await;
        assert_eq!(unnameable(&h, Snowflake(BEN)).await.unwrap(), None, "an ordinary member");
        h.discord.state.lock().unwrap().guild.insert("owner_id".into(), json!(ADA.to_string()));
        assert_eq!(unnameable(&h, Snowflake(ADA)).await.unwrap(), Some(Unnameable::Owner));
        {
            let mut st = h.discord.state.lock().unwrap();
            let top = st.roles.iter().filter_map(|r| r["position"].as_i64()).max().unwrap();
            st.roles
                .push(json!({ "id": "77", "name": "Boss", "position": top + 1, "permissions": "0", "managed": false }));
            let ben = st.members.iter_mut().find(|m| m["user"]["id"] == BEN.to_string().as_str()).unwrap();
            ben["roles"].as_array_mut().unwrap().push(json!("77"));
        }
        assert_eq!(unnameable(&h, Snowflake(BEN)).await.unwrap(), Some(Unnameable::Above));
        assert_eq!(unnameable(&h, Snowflake(REVIEWER)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn what_someone_told_the_bot_is_shown_from_their_path_only() {
        use crate::join::about;
        let h = join_host().await;
        assert!(about(&h, Snowflake(ADA)).await.unwrap().is_empty());
        let later = answers(&[("uni", "working"), ("year", "year-1")], &[("uni", "company", "Arm")]);
        confirm(&h, Snowflake(ADA), &later).await.unwrap();
        let profile = chipsoc_store::Profile {
            user_id: ADA.to_string(),
            name: "Ada".into(),
            surname: "Lovelace".into(),
            company: "Arm".into(),
        };
        h.db().set_profile(&profile, 1).await.unwrap();
        let a = about(&h, Snowflake(ADA)).await.unwrap();
        assert_eq!((a.name.as_str(), a.company.as_str(), a.shown.as_str()), ("Ada", "Arm", "7"));
        let said: Vec<(&str, &str)> = a.answers.iter().map(|s| (s.question.as_str(), s.answer.as_str())).collect();
        assert_eq!(said, [("Where?", "Working, Where?: Arm")], "the year was off their path: not kept or shown");
        assert!(a.finished.is_some());
        assert_eq!(about(&h, Snowflake(4242)).await.unwrap().shown, "", "not in the server");
    }

    #[tokio::test]
    async fn a_review_card_says_who_someone_is_from_their_answers_or_roles() {
        use crate::join::summary;
        let h = join_host().await;
        assert_eq!(summary(&h, Snowflake(ADA)).await, None, "nothing known");
        give(&h, BEN, "ours");
        give(&h, BEN, "treasurer");
        assert_eq!(summary(&h, Snowflake(BEN)).await.as_deref(), Some("ours"), "the flow's roles they hold");
        let later = answers(&[("uni", "working")], &[("uni", "company", "Arm")]);
        confirm(&h, Snowflake(ADA), &later).await.unwrap();
        assert_eq!(summary(&h, Snowflake(ADA)).await.as_deref(), Some("Working (Arm)"));
        confirm(&h, Snowflake(BEN), &answers(&[("uni", "here"), ("year", "year-1")], &[])).await.unwrap();
        assert_eq!(summary(&h, Snowflake(BEN)).await.as_deref(), Some("Here · Year 1"), "their answers, once given");
    }

    #[tokio::test]
    async fn unfinished_answers_change_nothing() {
        let h = join_host().await;
        h.discord.take_calls();
        let e = confirm(&h, Snowflake(BEN), &answers(&[("uni", "here")], &[])).await.unwrap_err();
        assert!(e.contains("aren't answered"), "{e}");
        assert!(h.discord.take_calls().is_empty());
        let e = confirm(&h, Snowflake(BEN), &answers(&[("uni", "working")], &[])).await.unwrap_err();
        assert!(e.contains("aren't answered"), "a required box left empty: {e}");
    }

    #[tokio::test]
    async fn a_role_made_powerful_in_discord_is_not_given() {
        let h = join_host().await;
        let id = h.cfg().desired.lock.role("y1").unwrap().to_string();
        h.discord.state.lock().unwrap().roles.iter_mut().find(|r| r["id"] == id.as_str()).unwrap()["permissions"] =
            json!("8");
        let done = confirm(&h, Snowflake(BEN), &answers(&[("uni", "here"), ("year", "year-1")], &[])).await.unwrap();
        assert!(!h.holds(BEN, "y1") && h.holds(BEN, "ours"));
        assert!(done.problems.iter().any(|p| p.contains("Didn't give")), "{:?}", done.problems);
    }

    #[tokio::test]
    async fn the_notice_goes_once_to_those_who_have_not_joined_in() {
        let h = join_host().await;
        confirm(&h, Snowflake(ADA), &answers(&[("uni", "here"), ("year", "year-1")], &[])).await.unwrap();
        {
            let mut st = h.discord.state.lock().unwrap();
            st.members.push(json!({ "user": { "id": "77", "bot": true }, "roles": [] }));
        }
        let targets = notify_targets(&h).await.unwrap();
        assert_eq!(targets, [Snowflake(BEN), Snowflake(REVIEWER)], "not Ada (finished) or the bot");
        h.discord.take_calls();
        let first = notify(&h, &targets, Duration::ZERO).await;
        assert_eq!(first.sent, 2);
        assert!(h.discord.dms_to(BEN)[0].contains("quick questions"));
        assert!(notify_targets(&h).await.unwrap().is_empty(), "recorded");
        let again = notify(&h, &targets, Duration::ZERO).await;
        assert_eq!(again.sent, 0, "never twice");
        assert_eq!(h.discord.dms_to(BEN).len(), 1);
    }
}

#[tokio::test]
async fn rejections_can_say_why_and_decisions_are_kept() {
    let h = host(true).await;
    let a = h.apply_for(AppKind::Position, "secretary", ADA, false).await;
    let b = h.apply_for(AppKind::Project, "golem", BEN, false).await;
    let card = h.card(a).await;
    let d = decide_with(&h, a, false, Some("  We're full this year.  "), &reviewer()).await.unwrap();
    assert_eq!(d.reason.as_deref(), Some("We're full this year."));
    let dm = &h.discord.dms_to(ADA)[0];
    assert!(dm.contains("wasn't taken up") && dm.contains("> We're full this year.") && !dm.contains("ask an admin"));
    let msg = h.rest().call(Method::Get, &card, None).await.unwrap();
    assert!(
        msg["content"].as_str().unwrap().ends_with("❌ **Rejected by <@9>.** Reason: We're full this year."),
        "{msg}"
    );
    decide_with(&h, b, true, Some(" "), &reviewer()).await.unwrap();

    let past = history(&h, 10).await.unwrap();
    let seen: Vec<(i64, &str, Option<&str>)> =
        past.iter().map(|p| (p.id, p.status.as_str(), p.reason.as_deref())).collect();
    assert_eq!(seen, [(b, "accepted", None), (a, "rejected", Some("We're full this year."))]);
    assert_eq!((past[1].what.as_str(), past[1].reviewed_by.as_deref()), ("Secretary", Some("9")));
    assert!(pending(&h).await.unwrap().is_empty());
}

#[tokio::test]
async fn feeds_pause_and_only_public_ones_are_added() {
    let h = host(true).await;
    let err = add_feed(&h, "http://10.0.0.1/rss", &reviewer()).await.unwrap_err();
    assert!(err.contains("private network"), "{err}");
    assert!(feeds(&h).await.unwrap().is_empty());
    let f = h.db().add_feed("https://example.org/feed", "feeds", "9", 3).await.unwrap();
    assert!(pause_feed(&h, f, true, &reviewer()).await.unwrap());
    assert!(feeds(&h).await.unwrap()[0].paused);
    assert!(pause_feed(&h, f, false, &reviewer()).await.unwrap());
    assert!(!feeds(&h).await.unwrap()[0].paused);
    assert!(!pause_feed(&h, 99, true, &reviewer()).await.unwrap());
    let actions: Vec<String> = activity(&h, 5).await.unwrap().into_iter().map(|e| e.action).collect();
    assert_eq!(actions, ["resume_feed", "pause_feed"]);
}

#[tokio::test]
async fn resources_are_archived_never_deleted_and_only_from_their_forum() {
    let h = host(true).await;
    let r = NewResource { title: "Book".into(), url: "https://example.org/b".into(), ..Default::default() };
    let post = post_resource(&h, &r, &reviewer()).await.unwrap();
    let ideas = h.cfg().desired.lock.channel("ideas").unwrap();
    let body = json!({ "name": "an idea", "message": { "content": "!" } });
    let idea = h.rest().call(Method::Post, &format!("channels/{ideas}/threads"), Some(body)).await.unwrap();
    let idea: Snowflake = idea["id"].as_str().unwrap().parse().unwrap();
    let general = h.cfg().desired.lock.channel("review").unwrap();
    h.discord.take_calls();

    assert!(!archive_resource(&h, idea, &reviewer()).await.unwrap(), "not a resource");
    assert!(!archive_resource(&h, general, &reviewer()).await.unwrap(), "not a post at all");
    assert!(!archive_resource(&h, Snowflake(424242), &reviewer()).await.unwrap(), "gone");
    assert!(archive_resource(&h, post.id, &reviewer()).await.unwrap());
    let calls = h.discord.take_calls();
    let changes: Vec<&String> = calls.iter().filter(|c| !c.starts_with("Get")).collect();
    assert_eq!(changes.len(), 1, "one change, to the resource: {calls:?}");
    assert!(changes[0].starts_with(&format!("Patch channels/{}", post.id)), "{calls:?}");
    let st = h.discord.state.lock().unwrap();
    let thread = st.channels.iter().find(|c| c["id"] == post.id.to_string()).unwrap();
    assert_eq!((&thread["archived"], &thread["locked"]), (&json!(true), &json!(true)));
}

#[tokio::test]
async fn the_roster_names_people_and_appointments_go_through_the_config() {
    let h = host(true).await;
    {
        let mut st = h.discord.state.lock().unwrap();
        let ada = st.members.iter_mut().find(|m| m["user"]["id"] == json!(ADA.to_string())).unwrap();
        ada["nick"] = json!("Ada L.");
    }
    let text = appoint(&h, "treasurer", Snowflake(ADA), &reviewer()).await.unwrap();
    assert!(text.contains("Recorded in the config") && h.holds(ADA, "treasurer") && h.holds(ADA, "committee"));
    let err = appoint(&h, "treasurer", Snowflake(ADA), &reviewer()).await.unwrap_err();
    assert!(err.contains("already holds"), "{err}");
    assert!(appoint(&h, "secretary", Snowflake(BEN), &reviewer()).await.unwrap_err().contains("doesn't say who holds"));

    let r = roster(&h).await.unwrap();
    let t = r.holders.iter().find(|x| x.role == "treasurer").unwrap();
    assert_eq!((t.name.as_str(), t.position.as_deref()), ("Treasurer", Some("Treasurer")));
    assert_eq!(t.people, [Person { id: Snowflake(ADA), name: "Ada L.".into() }]);
    assert_eq!(r.leads[0].project.as_deref(), Some("golem"));

    // The config's only admin keeps the admin role: nobody would be left with it.
    let text = unappoint(&h, "treasurer", Snowflake(ADA), &reviewer()).await.unwrap();
    assert!(text.contains("Taken out of the config. Took **Treasurer** away."), "{text}");
    assert!(text.contains("Couldn't take **Committee** away") && h.holds(ADA, "committee"), "{text}");
    assert!(!h.holds(ADA, "treasurer"));
    assert!(unappoint(&h, "treasurer", Snowflake(ADA), &reviewer()).await.unwrap_err().contains("isn't listed"));
    let actions: Vec<String> = activity(&h, 5).await.unwrap().into_iter().map(|e| e.action).collect();
    assert_eq!(actions, ["unappoint", "appoint"]);
}

#[tokio::test]
async fn people_who_left_can_be_restored_or_forgotten() {
    let h = host(true).await;
    h.db().remember_roles(&ADA.to_string(), &["treasurer".into()], 50).await.unwrap();
    h.db().remember_roles(&BEN.to_string(), &["treasurer".into()], 60).await.unwrap();
    let r = roster(&h).await.unwrap();
    assert_eq!(r.former.len(), 2);
    assert_eq!((r.former[0].person.id, r.former[0].roles[0].name.as_str()), (Snowflake(BEN), "Treasurer"));

    let me = Actor { id: Some(Snowflake(ADA)), name: "ada".into() };
    assert!(restore(&h, Snowflake(ADA), &me).await.unwrap_err().contains("Another admin"));
    let text = restore(&h, Snowflake(ADA), &reviewer()).await.unwrap();
    assert!(text.contains("Back in the config") && h.holds(ADA, "treasurer"), "{text}");
    assert_eq!(restore(&h, Snowflake(ADA), &reviewer()).await.unwrap_err(), "Already dealt with.");
    assert!(forget_former(&h, Snowflake(BEN), &reviewer()).await.unwrap());
    assert!(!forget_former(&h, Snowflake(BEN), &reviewer()).await.unwrap());
    assert!(roster(&h).await.unwrap().former.is_empty());
    assert!(!h.holds(BEN, "treasurer"));
}

#[test]
fn drift_sorts_steps_by_who_changed_them() {
    use chipsoc_model::{Action, Cause, Change, Field, ObjectKind, OrderScope, Plan};
    let edit = |key: &str, cause| Action::Edit {
        kind: ObjectKind::Channel,
        key: key.into(),
        id: Snowflake(1),
        changes: vec![Change { field: Field::Topic, from: "a".into(), to: "b".into() }],
        cause,
    };
    let plan = Plan {
        actions: vec![
            edit("cfg", Cause::Config),
            edit("dsc", Cause::Discord),
            edit("both", Cause::Both),
            Action::Create { kind: ObjectKind::Role, key: "new".into() },
            Action::Reorder { scope: OrderScope::Roles, from: vec![], to: vec![] },
            Action::Skip { kind: ObjectKind::Role, key: "x".into(), reason: "managed".into() },
        ],
        ..Plan::default()
    };
    let d = drift(&plan);
    let steps = |l: &[Line]| l.iter().map(|l| l.step).collect::<Vec<_>>();
    assert_eq!((steps(&d.waiting), steps(&d.discord), steps(&d.person)), (vec![0, 3], vec![1], vec![2, 4]));
    assert!(d.discord[0].text.contains("dsc") && !d.is_clean());
    assert!(drift(&Plan::default()).is_clean());
}

/// Appointing someone to an admin position gives them the admin role and
/// takes it from nobody (a committee member given it by hand keeps it);
/// taking them out of it takes the admin role from them straight away.
#[tokio::test]
async fn admin_positions_add_and_remove_only_their_holder() {
    let h = host(true).await;
    let committee = h.cfg().desired.lock.role("committee").unwrap();
    // The reviewer was made Committee by hand, and isn't in members.toml.
    let path = format!("guilds/1000/members/{REVIEWER}/roles/{committee}");
    h.rest().call(Method::Put, &path, None).await.unwrap();
    appoint(&h, "treasurer", Snowflake(ADA), &reviewer()).await.unwrap();
    let text = appoint(&h, "treasurer", Snowflake(BEN), &reviewer()).await.unwrap();
    assert!(text.contains("Gave them **Committee**"), "{text}");
    assert!(h.holds(ADA, "committee") && h.holds(BEN, "committee"));
    assert!(h.holds(REVIEWER, "committee"), "nobody else loses it");

    let text = unappoint(&h, "treasurer", Snowflake(BEN), &reviewer()).await.unwrap();
    assert!(text.contains("Took **Treasurer** away. Took **Committee** away."), "{text}");
    assert!(!h.holds(BEN, "committee") && h.holds(ADA, "committee") && h.holds(REVIEWER, "committee"));
}

#[tokio::test]
async fn applications_to_projects_that_arent_active_are_refused() {
    let h = host(true).await;
    let id = h.apply_for(AppKind::Project, "golem", BEN, false).await;
    let op = chipsoc_config::ops::Op::SetProject {
        slug: "golem".into(),
        set: chipsoc_config::ops::ProjectField::Status(chipsoc_config::ProjectStatus::Past),
    };
    chipsoc_config::ops::apply_op(&h.cfg().dir, &op).unwrap();
    *h.cfg.lock().unwrap() = Arc::new(Config::load(&h.cfg().dir).unwrap());
    let err = decide(&h, id, true, &reviewer()).await.unwrap_err();
    assert!(err.contains("**Golem** is past now, so it takes no members"), "{err}");
    assert_eq!(pending(&h).await.unwrap().len(), 1, "still waiting, to be rejected");
    assert!(!decide(&h, id, false, &reviewer()).await.unwrap().accepted);
}

#[tokio::test]
async fn nobody_appoints_themselves_to_an_admin_role() {
    let h = host(true).await;
    let ada = Actor { id: Some(Snowflake(ADA)), name: "ada".into() };
    let err = appoint(&h, "treasurer", Snowflake(ADA), &ada).await.unwrap_err();
    assert!(err.contains("Another admin"), "{err}");
    assert!(!h.holds(ADA, "committee"));
    appoint(&h, "treasurer", Snowflake(ADA), &reviewer()).await.unwrap();
    assert!(unappoint(&h, "treasurer", Snowflake(ADA), &ada).await.unwrap_err().contains("Another admin"));
    // Nor accepts their own application for an admin position.
    let id = h.apply_for(AppKind::Position, "treasurer", BEN, false).await;
    let ben = Actor { id: Some(Snowflake(BEN)), name: "ben".into() };
    assert!(decide(&h, id, true, &ben).await.unwrap_err().contains("Another admin"));
    assert!(decide(&h, id, true, &reviewer()).await.unwrap().accepted);
}

/// No admin takes a role from the server owner through the bot; the owner may step down.
#[tokio::test]
async fn nobody_but_the_owner_takes_the_owners_roles() {
    let h = host(true).await;
    h.discord.state.lock().unwrap().guild.insert("owner_id".into(), json!(ADA.to_string()));
    let ada = Actor { id: Some(Snowflake(ADA)), name: "ada".into() };
    appoint(&h, "treasurer", Snowflake(ADA), &reviewer()).await.unwrap();
    let err = unappoint(&h, "treasurer", Snowflake(ADA), &reviewer()).await.unwrap_err();
    assert_eq!(err, "Only the server owner can take a role from the server owner.");
    assert!(h.holds(ADA, "treasurer"));
    unappoint(&h, "treasurer", Snowflake(ADA), &ada).await.unwrap();
    assert!(!h.holds(ADA, "treasurer"));
    // Nor can anyone but the owner revert config changes.
    let err = undo(&h, "abc1234", chipsoc_config::git::Undo::Revert, &reviewer()).await.unwrap_err();
    assert!(err.starts_with("Only the server owner can revert"), "{err}");
}

#[tokio::test]
async fn a_role_given_power_by_an_overwrite_in_discord_is_not_handed_out() {
    let h = host(true).await;
    let team = h.cfg().desired.lock.role("proj-golem-team").unwrap().0;
    {
        let mut st = h.discord.state.lock().unwrap();
        let ch = st.channels.iter_mut().find(|c| c["name"] == "review").unwrap();
        ch["permission_overwrites"] = json!([{ "id": team.to_string(), "type": 0, "allow": "8192", "deny": "0" }]);
    }
    let id = h.apply_for(AppKind::Project, "golem", BEN, false).await;
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.pending && d.outcome.contains("moderator or admin permissions"), "{d:?}");
}

#[tokio::test]
async fn restoring_goes_through_the_config_checks() {
    let h = host(true).await;
    // A role members.toml no longer lists, and one made powerful in Discord since.
    h.db().remember_roles(&ADA.to_string(), &["secretary".into()], 50).await.unwrap();
    let text = restore(&h, Snowflake(ADA), &reviewer()).await.unwrap();
    assert!(text.contains("Not recorded in the config") && h.holds(ADA, "secretary"), "{text}");
    assert!(!text.contains("config: T"), "mid-sentence, lowercase: {text}");
    assert!(!fs::read_to_string(h.cfg().dir.join("members.toml")).unwrap().contains("secretary"), "no new list");

    let treasurer = h.cfg().desired.lock.role("treasurer").unwrap().to_string();
    for r in h.discord.state.lock().unwrap().roles.iter_mut().filter(|r| r["id"] == treasurer) {
        r["permissions"] = json!("8");
    }
    h.db().remember_roles(&BEN.to_string(), &["treasurer".into()], 60).await.unwrap();
    let text = restore(&h, Snowflake(BEN), &reviewer()).await.unwrap();
    assert!(text.contains("the config doesn't give it") && !h.holds(BEN, "treasurer"), "{text}");
    // Given straight away (the config can't be saved): checked too.
    let unwritable = host(false).await;
    let secretary = unwritable.cfg().desired.lock.role("secretary").unwrap().to_string();
    for r in unwritable.discord.state.lock().unwrap().roles.iter_mut().filter(|r| r["id"] == secretary) {
        r["permissions"] = json!("8");
    }
    let (text, ok) = give_unrecorded(&unwritable, Snowflake(BEN), &["secretary".into()], &"test".into()).await;
    assert!(!ok && text.contains("the config doesn't give it") && !unwritable.holds(BEN, "secretary"), "{text}");
}

/// Checking roles' permissions asks Discord once, however many roles.
#[tokio::test]
async fn roles_permissions_are_read_once_per_check() {
    let h = host(true).await;
    let keys: Vec<String> = ["secretary", "treasurer", "committee"].map(String::from).to_vec();
    let before = h.discord.requests();
    assert!(grown_too_powerful(&h, &keys[..1]).await.is_none());
    let one = h.discord.requests() - before;
    let before = h.discord.requests();
    assert!(grown_too_powerful(&h, &keys).await.is_none());
    assert_eq!(h.discord.requests() - before, one, "the same two reads for three roles");
}

#[tokio::test]
async fn only_the_owner_may_appoint_themselves_to_an_admin_position() {
    let h = host(true).await;
    let ada = Actor { id: Some(Snowflake(ADA)), name: "ada".into() };
    let err = appoint(&h, "treasurer", Snowflake(ADA), &ada).await.unwrap_err();
    assert!(err.contains("Another admin"), "{err}");
    h.discord.state.lock().unwrap().guild.insert("owner_id".into(), json!(ADA.to_string()));
    appoint(&h, "treasurer", Snowflake(ADA), &ada).await.unwrap();
    assert!(h.holds(ADA, "committee"));
}

/// A DM refused because they don't take DMs is told apart by Discord's
/// error code, not by its wording.
#[test]
fn closed_dms_are_known_by_discords_code() {
    let refused = |code| anyhow::Error::from(chipsoc_discord::ApiError { status: 403, message: "x".into(), code });
    let closed = |e: &anyhow::Error| chipsoc_discord::ApiError::code_of(e) == Some(chipsoc_discord::CANNOT_DM);
    let e = refused(Some(50007));
    assert_eq!(crate::DmError::from_send(&e, closed(&e)), crate::DmError::Closed);
    assert_eq!(crate::DmError::Closed.to_string(), "their DMs are closed");
    let e = refused(Some(50013));
    assert!(matches!(crate::DmError::from_send(&e, closed(&e)), crate::DmError::Failed(_)));
}

#[tokio::test]
async fn people_are_found_by_username_or_name() {
    let h = host(true).await;
    {
        let mut st = h.discord.state.lock().unwrap();
        for m in st.members.iter_mut() {
            if m["user"]["id"] == json!(ADA.to_string()) {
                m["user"]["username"] = json!("ada");
                m["user"]["global_name"] = json!("Ada Lovelace");
            } else if m["user"]["id"] == json!(BEN.to_string()) {
                m["user"]["username"] = json!("ben");
                m["nick"] = json!("Ada's friend");
            }
        }
    }
    assert_eq!(resolve_user(&h, "@ada").await, Ok(Snowflake(ADA)));
    assert_eq!(resolve_user(&h, "ada lovelace").await, Ok(Snowflake(ADA)), "display name, any case");
    assert_eq!(resolve_user(&h, &format!("<@{BEN}>")).await, Ok(Snowflake(BEN)), "a mention as it is");
    assert_eq!(resolve_user(&h, "Ada").await, Ok(Snowflake(ADA)), "a username wins over a nickname");
    // Only part of a name: offered to pick, never a guess.
    let Err(Unresolved::Several(one)) = resolve_user(&h, "friend").await else { panic!("should ask") };
    assert_eq!(Unresolved::Several(one).to_string(), "Which one? Ada's friend (@ben)");
    let Err(Unresolved::Several(both)) = resolve_user(&h, "a").await else { panic!("should ask") };
    assert!(both.len() >= 2 && both.iter().any(|m| m.label() == "Ada Lovelace (@ada)"));
    assert_eq!(resolve_user(&h, "zed").await, Err(Unresolved::Nobody("zed".into())));
    assert_eq!(resolve_user(&h, " @ ").await, Err(Unresolved::Blank));
    let named = members_of(&h, [Snowflake(ADA), Snowflake(99)]).await;
    assert_eq!(named.len(), 1, "only people in the server");
    assert_eq!(named[&Snowflake(ADA)].name, "Ada Lovelace");
}

#[tokio::test]
async fn the_reviewer_sees_when_the_decision_dm_failed_and_accepted_dms_say_what_they_got() {
    let h = host(true).await;
    let id = h.apply_for(AppKind::Position, "treasurer", ADA, false).await;
    h.discord.fail("POST USERS/@ME/CHANNELS", 403, false);
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.outcome.contains("(Couldn't DM them:"), "said on the card: {d:?}");

    let id = h.apply_for(AppKind::Position, "treasurer", BEN, false).await;
    decide(&h, id, true, &reviewer()).await.unwrap();
    let dm = &h.discord.dms_to(BEN)[0];
    assert_eq!(dm, "🎉 Your application for **Treasurer** in Test was accepted: you now hold **Treasurer**.");
}

impl TestHost {
    /// Change the config in memory.
    fn edit(&self, f: impl FnOnce(&mut Config)) {
        let mut c = (*self.cfg()).clone();
        f(&mut c);
        *self.cfg.lock().unwrap() = Arc::new(c);
    }

    async fn apply_in_window(&self, user: u64, why: &str) -> i64 {
        let answers = Answer::to_json(&[Answer::new("Why this position?", Some(&why.to_owned()))]);
        let round = chipsoc_store::Round::Waiting;
        self.db()
            .add_application_in(AppKind::Position, "secretary", &user.to_string(), &answers, false, 5, round)
            .await
            .unwrap()
    }

    fn messages_in(&self, key: &str) -> Vec<serde_json::Value> {
        let ch = self.cfg().desired.lock.channel(key).unwrap().to_string();
        self.discord.state.lock().unwrap().messages.iter().filter(|m| m["channel_id"] == ch).cloned().collect()
    }
}

/// A window whose card can't be posted waits for next time; the others are posted meanwhile.
#[tokio::test]
async fn one_window_that_fails_to_post_holds_up_no_other() {
    let h = host(true).await;
    h.edit(|c| {
        for p in &mut c.bot.positions {
            p.closes = Some("2020-01-01".into());
        }
    });
    for role in ["treasurer", "secretary"] {
        let answers = Answer::to_json(&[Answer::new("Why?", Some(&"why not".to_owned()))]);
        let round = chipsoc_store::Round::Waiting;
        h.db().add_application_in(AppKind::Position, role, &ADA.to_string(), &answers, false, 5, round).await.unwrap();
    }
    h.discord.fail("POST CHANNELS", 500, false);
    assert_eq!(rounds::post_closed(&h).await.unwrap(), 1, "the other one is posted");
    assert_eq!(rounds::post_closed(&h).await.unwrap(), 1, "the failed one, next time");
    assert_eq!(rounds::post_closed(&h).await.unwrap(), 0);
}

/// A window: applications wait quietly, then one card lists them all; each
/// decision redraws it, and "Reject the rest" rejects whoever is left.
#[tokio::test]
async fn a_window_s_applicants_are_listed_on_one_card_once_it_closes() {
    let h = host(true).await;
    h.edit(|c| c.bot.positions[1].closes = Some("2999-01-01".into()));
    assert!(rounds::in_window(&h.cfg(), "secretary", now()));
    let ada = h.apply_in_window(ADA, "I keep minutes").await;
    rounds::noted(&h, "secretary", Snowflake(ADA)).await;
    let ben = h.apply_in_window(BEN, "[click](https://evil.example)").await;
    rounds::noted(&h, "secretary", Snowflake(BEN)).await;
    let notes = h.messages_in("review");
    assert_eq!(notes[1]["content"], "📨 <@8> applied for **Secretary** (2 so far)");
    assert!(notes.iter().all(|m| m.get("components").is_none()), "no buttons while it's open");
    assert_eq!(rounds::post_closed(&h).await.unwrap(), 0, "still open");

    h.edit(|c| c.bot.positions[1].closes = Some("2020-01-01".into()));
    assert_eq!(rounds::post_closed(&h).await.unwrap(), 1);
    assert_eq!(rounds::post_closed(&h).await.unwrap(), 0, "posted once");
    let card = h.messages_in("review").pop().unwrap();
    let text = card["content"].as_str().unwrap();
    assert!(text.starts_with("**Position · Secretary: 2 applicants**"), "{text}");
    assert!(text.contains("**1.** <@7>") && text.contains("I keep minutes"), "{text}");
    assert!(text.contains("\\[click\\]\\(https://evil.example)"), "their words escaped: {text}");
    let ids: Vec<&str> = card["components"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|r| r["components"].as_array().unwrap())
        .map(|b| b["custom_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            rounds::review_button(true, ada),
            rounds::review_button(false, ada),
            rounds::review_button(true, ben),
            rounds::review_button(false, ben),
            rounds::rest_button("secretary"),
        ]
    );

    let d = decide(&h, ada, true, &reviewer()).await.unwrap();
    assert!(d.accepted && h.holds(ADA, "secretary"));
    let path = format!("channels/{}/messages/{}", card["channel_id"].as_str().unwrap(), card["id"].as_str().unwrap());
    let card = h.rest().call(Method::Get, &path, None).await.unwrap();
    let text = card["content"].as_str().unwrap();
    assert!(text.contains("✅ Accepted by <@9>."), "{text}");
    assert!(text.starts_with("**Position · Secretary: 2 applicants**"), "the head is kept: {text}");
    let left: Vec<&str> = card["components"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|r| r["components"].as_array().unwrap())
        .map(|b| b["label"].as_str().unwrap())
        .collect();
    assert_eq!(left, ["Accept 2", "Reject 2"], "buttons only for whoever is waiting");

    assert_eq!(rounds::reject_rest(&h, "secretary", Some("We picked someone"), &reviewer()).await, Ok(1));
    assert!(h.discord.dms_to(BEN).iter().any(|d| d.contains("wasn't taken up") && d.contains("We picked someone")));
    let card = h.rest().call(Method::Get, &path, None).await.unwrap();
    assert!(card["content"].as_str().unwrap().contains("❌ Rejected by <@9>. Reason: We picked someone"));
    assert_eq!(card["components"], json!([]));
    assert!(pending(&h).await.unwrap().is_empty());
}

/// A window with more applicants than fit on one card goes on several.
#[tokio::test]
async fn a_big_round_is_split_over_cards_that_fit_discord() {
    let h = host(true).await;
    h.edit(|c| c.bot.positions[1].closes = Some("2020-01-01".into()));
    for n in 0..10 {
        h.apply_in_window(100 + n, &"long answer ".repeat(80)).await;
    }
    assert_eq!(rounds::post_closed(&h).await.unwrap(), 2);
    for card in h.messages_in("review") {
        let text = card["content"].as_str().unwrap();
        assert!(text.chars().count() < fmt::MAX_CONTENT, "{}", text.chars().count());
        assert!(card["components"].as_array().unwrap().len() <= 5);
        assert!(text.contains("(part "), "{text}");
    }
}

/// An event a member suggested is made when it's accepted, crediting them.
#[tokio::test]
async fn an_accepted_event_suggestion_goes_on_the_calendar() {
    let h = host(true).await;
    h.edit(|c| c.bot.events = Some(chipsoc_config::bot::Events { channel: "feeds".into(), ..Default::default() }));
    let tz = events::tz(&h.cfg()).unwrap();
    let e = events::NewEvent {
        title: "Soldering *night*".into(),
        start: now() + 3 * 24 * 3600,
        minutes: 90,
        location: "Room 2.15".into(),
        description: "Bring a board".into(),
    };
    assert_eq!(events::already_suggested(&h, &e).await, None);
    let answers = Answer::to_json(&e.answers(tz));
    let id = h.db().add_application(AppKind::Event, &e.title, &ADA.to_string(), &answers, false, 5).await.unwrap();
    assert!(events::already_suggested(&h, &e).await.unwrap().contains("already suggested"));
    assert_eq!(pending(&h).await.unwrap()[0].what, "Soldering *night*");

    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.accepted && d.outcome.contains(" on the calendar for "), "{d:?}");
    let made = h.discord.state.lock().unwrap().events.clone();
    assert_eq!((made[0]["name"].as_str(), made[0]["entity_type"].as_u64()), (Some("Soldering *night*"), Some(3)));
    let post = &h.messages_in("feeds")[0];
    let text = post["content"].as_str().unwrap();
    assert!(text.contains("Soldering \\*night\\*") && text.contains("💡 Suggested by <@7>"), "{text}");
    assert!(h.discord.dms_to(ADA)[0].contains("is on the calendar in Test: https://discord.com/events/1000/"));
    assert_eq!(h.db().due_reminders(now() + 4 * 24 * 3600).await.unwrap().len(), 2, "a day and an hour before");
}

#[tokio::test]
async fn an_event_that_would_have_started_is_not_made() {
    let h = host(true).await;
    h.edit(|c| c.bot.events = Some(chipsoc_config::bot::Events { channel: "feeds".into(), ..Default::default() }));
    let answers = Answer::to_json(&[
        Answer::new("Title", Some(&"Old".to_owned())),
        Answer::new("Starts", Some(&"2020-01-01 18:00".to_owned())),
    ]);
    let id = h.db().add_application(AppKind::Event, "Old", &ADA.to_string(), &answers, false, 5).await.unwrap();
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.pending && d.outcome.contains("would have started already"), "{d:?}");
    assert!(h.discord.state.lock().unwrap().events.is_empty());
}

/// An accepted position whose role isn't theirs yet isn't told "you now hold".
#[tokio::test]
async fn an_accept_that_gives_no_role_yet_says_why_and_a_grown_role_stays_queued() {
    let h = host(true).await;
    let id = h.apply_for(AppKind::Position, "treasurer", BEN, false).await;
    h.discord.fail(&format!("PUT guilds/1000/members/{BEN}/roles"), 403, false);
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.accepted && !d.pending, "recorded in the config: {d:?}");
    let dm = &h.discord.dms_to(BEN)[0];
    assert_eq!(
        dm,
        "🎉 Your application for **Treasurer** in Test was approved. The role follows once an admin has given it to you."
    );

    // Too powerful now: nothing given, nothing recorded, still in the queue.
    let h = host(true).await;
    let treasurer = h.cfg().desired.lock.role("treasurer").unwrap().to_string();
    for r in h.discord.state.lock().unwrap().roles.iter_mut().filter(|r| r["id"] == treasurer) {
        r["permissions"] = json!("8");
    }
    let id = h.apply_for(AppKind::Position, "treasurer", ADA, false).await;
    let d = decide(&h, id, true, &reviewer()).await.unwrap();
    assert!(d.pending && !d.accepted && d.outcome.contains("moderator or admin permissions"), "{d:?}");
    assert!(!d.outcome.contains("Not recorded"), "{d:?}");
    assert!(!h.holds(ADA, "treasurer") && h.discord.dms_to(ADA).is_empty());
    assert!(!fs::read_to_string(h.cfg().dir.join("members.toml")).unwrap().contains(&ADA.to_string()));

    // Held for an Approve, or not applied: said so.
    let held = Recorded { held_back: true, ..Default::default() };
    let roles = vec!["treasurer".to_owned()];
    assert_eq!(Waits::of(&held, &roles), Some(Waits::Approval));
    assert_eq!(Waits::of(&Recorded { unapplied: true, ..Default::default() }, &roles), Some(Waits::Apply));
    let given = Recorded { holds: roles.iter().cloned().collect(), held_back: true, ..Default::default() };
    assert_eq!(Waits::of(&given, &roles), None, "held back, but not this");
    assert!(Waits::Approval.for_them().contains("another admin has approved"));
}

#[tokio::test]
async fn an_idea_is_closed_with_a_tag_and_a_reason_never_deleted() {
    let h = host(true).await;
    h.discord.edit_channel("ideas", |f| {
        f["available_tags"].as_array_mut().unwrap().push(json!({ "id": "777", "name": "Closed" }));
    });
    let set = |closed: &str| {
        let mut c = (*h.cfg()).clone();
        let i = c.bot.ideas.as_mut().unwrap();
        (i.needs_lead_tag, i.closed_tag) = (Some("needs-lead".into()), Some(closed.into()));
        *h.cfg.lock().unwrap() = Arc::new(c);
    };
    set("closed");
    let ideas = h.cfg().desired.lock.channel("ideas").unwrap();
    let info = h.rest().call(Method::Get, &format!("channels/{ideas}"), None).await.unwrap();
    let tag_id = |name: &str| {
        info["available_tags"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap()["id"].clone()
    };
    let (cpu, needs) = (tag_id("cpu"), tag_id("needs-lead"));
    let body = json!({ "name": "an idea", "applied_tags": [cpu, needs], "message": { "content": "!" } });
    let idea = h.rest().call(Method::Post, &format!("channels/{ideas}/threads"), Some(body)).await.unwrap();
    let idea: Snowflake = idea["id"].as_str().unwrap().parse().unwrap();
    let resource = h.cfg().desired.lock.channel("resources").unwrap();
    let body = json!({ "name": "a book", "message": { "content": "!" } });
    let other = h.rest().call(Method::Post, &format!("channels/{resource}/threads"), Some(body)).await.unwrap();
    let other: Snowflake = other["id"].as_str().unwrap().parse().unwrap();

    assert!(!close_idea(&h, other, None, &reviewer()).await.unwrap(), "not an idea");
    assert!(!close_idea(&h, Snowflake(424242), None, &reviewer()).await.unwrap(), "gone");
    set("done");
    let err = close_idea(&h, idea, None, &reviewer()).await.unwrap_err().to_string();
    assert!(err.contains("no tag `done`"), "{err}");
    set("closed");
    assert!(close_idea(&h, idea, Some(" Merged into Golem. "), &reviewer()).await.unwrap());
    {
        let st = h.discord.state.lock().unwrap();
        let thread = st.channels.iter().find(|c| c["id"] == idea.to_string()).unwrap();
        assert_eq!(thread["applied_tags"], json!([cpu, "777"]), "closed, no longer needing a lead");
        assert_eq!((&thread["archived"], &thread["locked"]), (&json!(true), &json!(true)));
        let mine = st.messages.iter().filter(|m| m["channel_id"] == idea.to_string());
        let said: Vec<&str> = mine.filter_map(|m| m["content"].as_str()).collect();
        assert_eq!(said, ["!", "**Closed:** Merged into Golem."]);
    }
    let actions: Vec<String> = activity(&h, 1).await.unwrap().into_iter().map(|e| e.action).collect();
    assert_eq!(actions, ["close_idea"]);
}
