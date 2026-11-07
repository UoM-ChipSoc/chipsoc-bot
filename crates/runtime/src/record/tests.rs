//! Recording end to end on the fake Discord and a pretend call: the
//! notices, who may record, the files and metadata, the committee's summary.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chipsoc_config::Config;
use chipsoc_discord::fake::FakeDiscord;
use chipsoc_voice::FRAME;
use chipsoc_voice::capture::{Feed, Input};
use chipsoc_voice::store::{self, Transcript};
use serde_json::{Value, json};
use twilight_model::application::interaction::Interaction;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker};

use super::{Call, Conn, STATUS, Voice};
use crate::Ctx;
use crate::tests::eventually;

const ADA: u64 = 7;
const BEN: u64 = 8;
const CY: u64 = 9;

/// A call that's never really joined: what the bot asked for, and the feed
/// it was given, to play audio into.
#[derive(Default)]
struct Pretend {
    moves: Mutex<Vec<Option<u64>>>,
    feed: Mutex<Option<Feed>>,
    conns: Mutex<Vec<Conn>>,
    left: Arc<Mutex<u32>>,
    /// The server, to see what was posted before the bot started hearing the call.
    fake: Mutex<Option<Arc<FakeDiscord>>>,
    /// Messages posted by the time each connect began.
    posted_before: Mutex<Vec<Vec<String>>>,
    /// Make connecting fail.
    refuse: Mutex<bool>,
}

struct PretendCall(Arc<Mutex<u32>>);

impl Call for PretendCall {
    fn leave(&mut self) {
        *self.0.lock().unwrap() += 1;
    }
}

impl Voice for Pretend {
    fn move_to(&self, _: Id<GuildMarker>, channel: Option<Id<ChannelMarker>>) {
        self.moves.lock().unwrap().push(channel.map(Id::get));
    }
    fn connect(&self, conn: Conn, feed: Feed) -> super::BoxFuture<'static, anyhow::Result<Box<dyn Call>>> {
        if let Some(f) = self.fake.lock().unwrap().as_ref() {
            let st = f.state.lock().unwrap();
            let texts = st.messages.iter().map(|m| m["content"].as_str().unwrap_or("").to_owned()).collect();
            self.posted_before.lock().unwrap().push(texts);
        }
        if *self.refuse.lock().unwrap() {
            return Box::pin(async { anyhow::bail!("the voice server hung up") });
        }
        self.conns.lock().unwrap().push(conn);
        *self.feed.lock().unwrap() = Some(feed);
        let left = self.left.clone();
        Box::pin(async move { Ok(Box::new(PretendCall(left)) as Box<dyn Call>) })
    }
}

fn write(dir: &std::path::Path, file: &str, body: &str) {
    let p = dir.join(file);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

struct Server {
    _dir: tempfile::TempDir,
    root: tempfile::TempDir,
    fake: Arc<FakeDiscord>,
    ctx: Arc<Ctx>,
    call: Arc<Pretend>,
}

impl Server {
    fn id(&self, channel: &str) -> u64 {
        self.ctx.channel(channel).unwrap().get()
    }
    fn role(&self, role: &str) -> String {
        self.ctx.role(role).unwrap().to_string()
    }
    fn messages_in(&self, channel: &str) -> Vec<String> {
        let id = self.id(channel).to_string();
        let st = self.fake.state.lock().unwrap();
        st.messages
            .iter()
            .filter(|m| m["channel_id"] == id.as_str())
            .map(|m| m["content"].as_str().unwrap_or("").to_owned())
            .collect()
    }
    fn replies(&self, token: &str) -> String {
        self.fake.replies_to(token).iter().map(Value::to_string).collect::<Vec<_>>().join("\n")
    }
    fn bodies(&self, call: &str) -> Vec<Value> {
        let st = self.fake.state.lock().unwrap();
        st.bodies.iter().filter(|(c, _)| c == call).map(|(_, b)| b.clone()).collect()
    }
}

async fn server(recording: &str) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    write(d, "server.toml", "[guild]\nname = \"Test\"\n");
    write(
        d,
        "bot.toml",
        &format!("name = \"Test Society\"\nadmin_role = \"committee\"\n[log]\nbot = \"log\"\n{recording}"),
    );
    write(
        d,
        "roles.toml",
        "[[role]]\nkey = \"bot\"\nname = \"Bot\"\nmanaged = true\n\n[[role]]\nkey = \"committee\"\nname = \"Committee\"\n\n\
         [[role]]\nkey = \"secretary\"\nname = \"Secretary\"\n\n\
         [[role]]\nkey = \"everyone\"\nname = \"@everyone\"\npermissions = [\"VIEW_CHANNEL\", \"CONNECT\"]\n",
    );
    write(
        d,
        "categories/10-main.toml",
        "[category]\nkey = \"main\"\nname = \"Main\"\n\n[[channel]]\nkey = \"log\"\nname = \"log\"\n\n\
         [[channel]]\nkey = \"committee-chat\"\nname = \"committee-chat\"\n\n\
         [[channel]]\nkey = \"meeting\"\nname = \"meeting\"\ntype = \"voice\"\n",
    );
    let fake = Arc::new(FakeDiscord::new());
    for u in [ADA, BEN, CY] {
        fake.join(u);
    }
    let mut e = chipsoc_engine::Engine::load(fake.clone(), d).unwrap();
    let plan = e.plan(chipsoc_engine::Options::default()).await.unwrap();
    e.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
    let ctx = Arc::new(Ctx::on_fake(Config::load(d).unwrap(), fake.clone(), false).await);
    let call = Arc::new(Pretend::default());
    *call.fake.lock().unwrap() = Some(fake.clone());
    ctx.rec.set_link(call.clone());
    let root = tempfile::tempdir().unwrap();
    ctx.rec.set_root(root.path());
    Server { _dir: dir, root, fake, ctx, call }
}

const SET_UP: &str = "[recording]\nwho = [\"secretary\"]\nchannel = \"committee-chat\"\ntranscribe = \"none\"\n";

fn member(user: u64, name: &str, roles: &[String], admin: bool) -> Value {
    json!({
        "user": {
            "id": user.to_string(), "username": name.to_lowercase(), "global_name": name,
            "discriminator": "0", "avatar": null,
        },
        "roles": roles,
        "joined_at": "2024-01-01T00:00:00.000000+00:00",
        "deaf": false,
        "mute": false,
        "flags": 0,
        "permissions": if admin { "8" } else { "0" },
    })
}

/// `/record <sub>` from `user` (with `roles`), with `options` for the subcommand.
fn record(user: u64, name: &str, roles: &[String], sub: &str, options: Value, token: &str) -> Interaction {
    serde_json::from_value(json!({
        "id": "500",
        "application_id": "1",
        "type": 2,
        "token": token,
        "guild_id": "1000",
        "attachment_size_limit": 8_388_608,
        "authorizing_integration_owners": {},
        "entitlements": [],
        "member": member(user, name, roles, false),
        "data": { "id": "1", "name": "record", "type": 1, "options": [{ "name": sub, "type": 1, "options": options }] },
    }))
    .unwrap()
}

/// A voice state update: `user` now in `channel` (or none).
fn moved(s: &Server, user: u64, name: &str, channel: Option<u64>) {
    let mut m = member(user, name, &[], false);
    m.as_object_mut().unwrap().remove("permissions");
    let vs: twilight_model::voice::VoiceState = serde_json::from_value(json!({
        "channel_id": channel.map(|c| c.to_string()),
        "guild_id": "1000",
        "deaf": false, "mute": false, "self_deaf": false, "self_mute": false, "self_stream": false, "self_video": false,
        "suppress": false,
        "session_id": format!("session-{user}"),
        "user_id": user.to_string(),
        "request_to_speak_timestamp": null,
        "member": m,
    }))
    .unwrap();
    super::voice_state(&s.ctx, &vs);
}

/// Start recording as Ada (a secretary, in the meeting), answering for
/// Discord's gateway as the bot joins.
async fn start(s: &Server) {
    let i = record(ADA, "Ada", &[s.role("secretary")], "start", json!([]), "start");
    start_with(s, i).await;
}

/// [`start`], with `i` as the `/record start` Ada runs.
async fn start_with(s: &Server, i: Interaction) {
    moved(s, ADA, "Ada", Some(s.id("meeting")));
    let ctx = s.ctx.clone();
    let task = tokio::spawn(async move { crate::commands::interaction(&ctx, &i).await });
    // The bot asks to join (unless it was refused); Discord says where to connect.
    eventually("the bot to ask to join", || !s.call.moves.lock().unwrap().is_empty() || task.is_finished()).await;
    moved(s, s.ctx.me.get(), "Bot", Some(s.id("meeting")));
    super::voice_server(&s.ctx, "voice.example:443", "secret");
    task.await.unwrap();
}

fn feed(s: &Server) -> Feed {
    s.call.feed.lock().unwrap().clone().expect("connected")
}

#[tokio::test]
async fn a_meeting_is_recorded_everyone_is_told_and_the_committee_gets_it() {
    let s = server(SET_UP).await;
    let meeting = s.id("meeting");
    start(&s).await;
    assert!(s.replies("start").contains("Recording"), "{}", s.replies("start"));
    assert_eq!(*s.call.moves.lock().unwrap(), vec![Some(meeting)]);
    let conn = s.call.conns.lock().unwrap()[0].clone();
    let got = (conn.session.as_str(), conn.endpoint.as_str(), conn.token.as_str());
    assert_eq!(got, ("session-42", "voice.example:443", "secret"));
    // The call's status, and a notice in its chat.
    let status = s.bodies(&format!("Put channels/{meeting}/voice-status"));
    assert_eq!(status, vec![json!({ "status": STATUS })]);
    let notice = s.messages_in("meeting").join("\n");
    assert!(notice.contains("This call is being recorded and transcribed by the bot for Test Society"), "{notice}");
    assert!(notice.contains("If you don't want to be recorded, leave the call."), "{notice}");

    // Ada talks; Ben joins and is told by DM.
    let f = feed(&s);
    f.send(Input::Speaker { ssrc: 11, user: ADA });
    for _ in 0..100 {
        f.send(Input::Tick(vec![(11, vec![2000; FRAME])]));
    }
    moved(&s, BEN, "Ben", Some(meeting));
    moved(&s, BEN, "Ben", Some(meeting)); // unchanged: not told twice
    eventually("Ben to be told by DM", || !s.fake.dms_to(BEN).is_empty()).await;
    let dms = s.fake.dms_to(BEN);
    assert_eq!(dms.len(), 1, "{dms:?}");
    assert!(dms[0].contains("being recorded") && dms[0].contains("(#meeting)"), "{}", dms[0]);
    assert!(s.fake.dms_to(ADA).is_empty(), "who was there was told in the call");
    f.send(Input::Speaker { ssrc: 12, user: BEN });
    for _ in 0..100 {
        f.send(Input::Tick(vec![(12, vec![-1500; FRAME])]));
    }

    // The bot heard nothing before the notice was up.
    let before = s.call.posted_before.lock().unwrap()[0].join("\n");
    assert!(before.contains("This call is being recorded"), "{before}");

    // Someone without the role can't stop it; nor can another secretary; Ada can.
    let i = record(CY, "Cy", &[], "stop", json!([]), "cy");
    crate::commands::interaction(&s.ctx, &i).await;
    assert!(s.replies("cy").contains("Only admins and Secretary"), "{}", s.replies("cy"));
    let i = record(BEN, "Ben", &[s.role("secretary")], "stop", json!([]), "ben");
    crate::commands::interaction(&s.ctx, &i).await;
    assert!(s.replies("ben").contains(&format!("<@{ADA}> started this recording")), "{}", s.replies("ben"));
    assert!(s.ctx.rec.recording().is_some(), "still recording");
    let i = record(ADA, "Ada", &[s.role("secretary")], "stop", json!([]), "stop");
    crate::commands::interaction(&s.ctx, &i).await;
    assert!(s.replies("stop").contains("Stopped recording"), "{}", s.replies("stop"));
    assert_eq!(s.call.moves.lock().unwrap().last(), Some(&None), "left the call");
    assert_eq!(*s.call.left.lock().unwrap(), 1);
    let status = s.bodies(&format!("Put channels/{meeting}/voice-status"));
    assert_eq!(status.last(), Some(&json!({ "status": "" })));
    assert!(s.messages_in("meeting").iter().any(|m| m.contains("Recording stopped")));

    // The files and what's known about them.
    let all = store::list(s.root.path(), 1000);
    assert_eq!(all.len(), 1);
    let m = &all[0];
    assert!(m.id.ends_with("-meeting"), "{}", m.id);
    assert_eq!(m.channel_name, "#meeting");
    assert_eq!(m.names(), ["Ada", "Ben"]);
    assert_eq!(m.started_by.name, "Ada");
    assert_eq!(m.ended_because.as_deref(), Some("stopped by Ada"));
    assert_eq!(m.files(), ["7.ogg", "8.ogg", store::MIX]);
    assert_eq!(m.length_ms, 4000);
    let dir = s.root.path().join("1000").join(&m.id);
    for f in m.files() {
        assert!(dir.join(f).metadata().unwrap().len() > 100, "{f}");
    }

    // The committee's summary.
    let summary = s.messages_in("committee-chat").join("\n");
    assert!(summary.contains("Recording of #meeting ended** after 4s: stopped by Ada."), "{summary}");
    assert!(summary.contains("People: Ada, Ben."), "{summary}");
    assert!(summary.contains(&format!("Recordings page (`{}`)", m.id)), "{summary}");
    assert!(summary.contains("No transcript: transcripts are turned off"), "{summary}");
    let audit = s.ctx.db().recent_audit(10).await.unwrap();
    let actions: Vec<&str> = audit.iter().map(|a| a.action.as_str()).collect();
    assert!(actions.contains(&"recording_started") && actions.contains(&"recording_stopped"), "{actions:?}");

    // The transcript job marks it skipped, keeping the audio.
    let rec = s.ctx.cfg().bot.recording.clone().unwrap();
    super::make_transcript(&s.ctx, &rec, m).await;
    let m = store::list(s.root.path(), 1000).remove(0);
    assert!(matches!(m.transcript, Transcript::Skipped { .. }), "{:?}", m.transcript);
    assert!(dir.join(store::MIX).exists());
}

#[tokio::test]
async fn who_may_record_and_where() {
    let s = server("").await;
    let i = record(ADA, "Ada", &[], "start", json!([]), "off");
    crate::commands::interaction(&s.ctx, &i).await;
    assert!(s.replies("off").contains("Recording isn't set up here"), "{}", s.replies("off"));

    let s = server(SET_UP).await;
    let i = record(CY, "Cy", &[], "start", json!([]), "cy");
    crate::commands::interaction(&s.ctx, &i).await;
    assert!(s.replies("cy").contains("Only admins and Secretary can"), "{}", s.replies("cy"));
    // Allowed, but not in a call and none named.
    let i = record(ADA, "Ada", &[s.role("secretary")], "start", json!([]), "nowhere");
    crate::commands::interaction(&s.ctx, &i).await;
    assert!(s.replies("nowhere").contains("Join the call first"), "{}", s.replies("nowhere"));
    // A text channel isn't a call.
    let log = s.id("log").to_string();
    let options = json!([{ "name": "channel", "type": 7, "value": log }]);
    let i = record(ADA, "Ada", &[s.role("secretary")], "start", options, "text");
    crate::commands::interaction(&s.ctx, &i).await;
    assert!(s.replies("text").contains("isn't a voice or stage channel"), "{}", s.replies("text"));
    assert!(s.call.moves.lock().unwrap().is_empty(), "never joined");
    // Stopping with nothing running says so.
    let i = record(ADA, "Ada", &[s.role("committee")], "stop", json!([]), "none");
    crate::commands::interaction(&s.ctx, &i).await;
    assert!(s.replies("none").contains("Nothing is being recorded"), "{}", s.replies("none"));
}

#[tokio::test]
async fn closed_dms_get_the_notice_in_the_call_and_a_missing_status_goes_in_the_notice() {
    let s = server(SET_UP).await;
    let meeting = s.id("meeting");
    s.fake.fail(&format!("PUT channels/{meeting}/voice-status"), 403, false);
    start(&s).await;
    let notice = s.messages_in("meeting").join("\n");
    assert!(notice.contains(&format!("**{STATUS} now.**")), "{notice}");
    s.fake.fail("POST users/@me/channels", 403, false);
    moved(&s, BEN, "Ben", Some(meeting));
    let ping = |st: &chipsoc_discord::fake::State| {
        st.messages.iter().find(|m| m["content"].as_str().is_some_and(|c| c.starts_with(&format!("<@{BEN}>")))).cloned()
    };
    eventually("Ben to be told in the call", || ping(&s.fake.state.lock().unwrap()).is_some()).await;
    {
        let st = s.fake.state.lock().unwrap();
        let ping = st
            .messages
            .iter()
            .find(|m| m["content"].as_str().is_some_and(|c| c.starts_with(&format!("<@{BEN}>"))))
            .expect("told in the call");
        assert_eq!(ping["allowed_mentions"]["users"], json!([BEN.to_string()]), "pings only them");
    }
    // The bot is pulled out of the call: it stops, saying why.
    moved(&s, s.ctx.me.get(), "Bot", None);
    let ended = || store::list(s.root.path(), 1000).first().is_some_and(|m| m.ended_because.is_some());
    eventually("the recording to stop", || s.ctx.rec.recording().is_none() && ended()).await;
    let m = store::list(s.root.path(), 1000).remove(0);
    assert_eq!(m.ended_because.as_deref(), Some("the bot was disconnected from the call"));
    assert!(s.ctx.rec.recording().is_none());
}

#[test]
fn limits_stop_a_recording() {
    let min = |m: u64| Duration::from_secs(m * 60);
    let max = min(180);
    let files = |full: bool| if full { super::Files::Full } else { super::Files::Fine };
    let r = |humans, empty, ran, full, free| super::reason(humans, empty, ran, max, files(full), free, (4.0, None));
    assert_eq!(r(2, None, min(10), false, Some(u64::MAX)), None);
    assert_eq!(r(0, Some(min(1)), min(10), false, None), None, "empty only a minute");
    assert_eq!(r(0, Some(min(2)), min(10), false, None).unwrap(), "the call was empty for 2 minutes");
    assert!(r(1, None, min(180), false, None).unwrap().contains("3-hour limit"));
    assert!(r(1, None, min(1), true, None).unwrap().contains("the 4 GB limit"));
    let broken = super::reason(1, None, min(1), max, super::Files::Broken, None, (4.0, None)).unwrap();
    assert_eq!(broken, "writing the files failed (see the bot's log)", "not the size limit");
    assert_eq!(r(1, None, min(1), false, Some(1000)).unwrap(), "the disk is nearly full");
    let all = super::reason(1, None, min(1), max, files(true), None, (4.0, Some(20.0))).unwrap();
    assert_eq!(all, "the recordings here reached their 20 GB limit in all");
}

#[tokio::test]
async fn a_recording_the_bot_was_making_when_it_stopped_is_closed_and_reported() {
    let s = server(SET_UP).await;
    let (id, dir) = store::create(s.root.path(), 1000, 1_000_000, "meeting").unwrap();
    let by = store::Person { id: ADA, name: "Ada".into() };
    let mut m = store::Meta::new(id.clone(), 1000, s.id("meeting"), "#meeting", 1_000_000, by);
    m.length_ms = 60_000;
    let span = store::Span { from_ms: 0, to_ms: None };
    m.participants.push(store::Participant { id: ADA, name: "Ada".into(), spans: vec![span] });
    store::save(&dir, &m).unwrap();
    super::recover(&s.ctx).await;
    let m = store::find(s.root.path(), 1000, &id).unwrap().1;
    assert!(m.ended_at.is_some());
    assert_eq!(m.ended_because.as_deref(), Some("the bot stopped while recording"));
    assert_eq!(m.participants[0].spans[0].to_ms, Some(60_000));
    assert_eq!(m.transcript, Transcript::Waiting);
    let summary = s.messages_in("committee-chat").join("\n");
    assert!(summary.contains("the bot stopped while recording") && summary.contains("after 1m 00s"), "{summary}");
}

#[tokio::test]
async fn nobody_is_recorded_without_the_notice() {
    let s = server(SET_UP).await;
    let meeting = s.id("meeting");
    s.fake.fail(&format!("POST channels/{meeting}/messages"), 403, false);
    start(&s).await;
    assert!(s.replies("start").contains("isn't recording"), "{}", s.replies("start"));
    assert_eq!(*s.call.moves.lock().unwrap(), vec![Some(meeting), None], "joined, then left");
    assert!(s.call.conns.lock().unwrap().is_empty() && s.call.posted_before.lock().unwrap().is_empty(), "never heard");
    assert!(s.ctx.rec.recording().is_none());
    assert!(store::list(s.root.path(), 1000).is_empty(), "nothing kept");
    let status = s.bodies(&format!("Put channels/{meeting}/voice-status"));
    assert_eq!(status.last(), Some(&json!({ "status": "" })), "the status is cleared");
}

#[tokio::test]
async fn nobody_records_a_call_they_cant_join_themselves() {
    let s = server(SET_UP).await;
    let meeting = s.id("meeting");
    let everyone = "1000";
    s.fake.edit_channel("meeting", |c| {
        c["permission_overwrites"] = json!([{ "id": everyone, "type": 0, "allow": "0", "deny": "1024" }]);
    });
    let options = json!([{ "name": "channel", "type": 7, "value": meeting.to_string() }]);
    let i = record(ADA, "Ada", &[s.role("secretary")], "start", options.clone(), "private");
    crate::commands::interaction(&s.ctx, &i).await;
    assert!(s.replies("private").contains("You can't join"), "{}", s.replies("private"));
    assert!(s.call.moves.lock().unwrap().is_empty(), "never joined");
    // Let in by their role: they may.
    let secretary = s.role("secretary");
    s.fake.edit_channel("meeting", |c| {
        c["permission_overwrites"] = json!([
            { "id": everyone, "type": 0, "allow": "0", "deny": "1024" },
            { "id": secretary, "type": 0, "allow": "1049600", "deny": "0" },
        ]);
    });
    start(&s).await;
    assert!(s.replies("start").contains("🔴 Recording"), "{}", s.replies("start"));
    // A member overwrite shutting them out wins over their role.
    let ow = |c: &mut Value| {
        c["permission_overwrites"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "id": ADA.to_string(), "type": 1, "allow": "0", "deny": "1048576" }));
    };
    let guild = json!({ "id": "1000", "owner_id": "5", "roles": [{ "id": "1000", "permissions": "1049600" }] });
    let mut c = json!({ "permission_overwrites": [] });
    ow(&mut c);
    assert_eq!(super::permissions_in(&guild, &c, ADA, &[]) & super::CONNECT, 0);
    assert_eq!(super::permissions_in(&guild, &c, 5, &[]), u64::MAX, "the owner may do anything");
}

#[tokio::test]
async fn a_late_joiner_nobody_can_tell_stops_the_recording() {
    let s = server(SET_UP).await;
    let meeting = s.id("meeting");
    start(&s).await;
    s.fake.fail("POST users/@me/channels", 403, false);
    s.fake.fail(&format!("POST channels/{meeting}/messages"), 403, false);
    moved(&s, BEN, "Ben", Some(meeting));
    // Stopping closes the files after the session ends: wait for both, however slow the machine.
    let ended = || store::list(s.root.path(), 1000).first().is_some_and(|m| m.ended_because.is_some());
    let told = || s.messages_in("log").join("\n").contains("couldn't be told");
    eventually("the recording to stop, saying why", || s.ctx.rec.recording().is_none() && ended() && told()).await;
    let m = store::list(s.root.path(), 1000).remove(0);
    assert_eq!(m.ended_because.as_deref(), Some("someone who joined couldn't be told it's recording"));
    let log = s.messages_in("log").join("\n");
    assert!(log.contains(&format!("<@{BEN}> joined the call and couldn't be told")), "{log}");
}

#[tokio::test]
async fn a_late_joiner_is_told_where_the_recording_was_started_if_nowhere_else() {
    let s = server(SET_UP).await;
    let meeting = s.id("meeting");
    let chat = s.id("committee-chat");
    let mut i = serde_json::to_value(record(ADA, "Ada", &[s.role("secretary")], "start", json!([]), "start")).unwrap();
    i["channel"] = json!({ "id": chat.to_string(), "type": 0 });
    i["channel_id"] = json!(chat.to_string());
    start_with(&s, serde_json::from_value(i).unwrap()).await;
    s.fake.fail("POST users/@me/channels", 403, false);
    s.fake.fail(&format!("POST channels/{meeting}/messages"), 403, false);
    moved(&s, BEN, "Ben", Some(meeting));
    let said = || s.messages_in("committee-chat").join("\n").contains(&format!("<@{BEN}> "));
    eventually("Ben to be told where it was started", said).await;
    assert!(s.ctx.rec.recording().is_some(), "still recording");
    let told = s.messages_in("committee-chat").join("\n");
    assert!(told.contains(&format!("<@{BEN}> ")) && told.contains("being recorded"), "{told}");
}

#[tokio::test]
async fn recordings_stop_piling_up_at_max_total_gb() {
    let s = server(&format!("{SET_UP}max_total_gb = 0.000001\n")).await;
    let (_, old) = store::create(s.root.path(), 1000, 0, "old").unwrap();
    std::fs::write(old.join("7.ogg"), [0u8; 2000]).unwrap();
    start(&s).await;
    let reply = s.replies("start");
    assert!(reply.contains("recording.max_total_gb") && reply.contains("Delete old recordings"), "{reply}");
    assert!(s.ctx.rec.recording().is_none() && s.call.moves.lock().unwrap().is_empty());
    assert!(s.messages_in("log").join("\n").contains("couldn't start a recording"));
}

#[tokio::test]
async fn a_call_that_cant_be_heard_records_nothing_and_says_so() {
    let s = server(SET_UP).await;
    *s.call.refuse.lock().unwrap() = true;
    start(&s).await;
    assert!(s.replies("start").contains("Couldn't start recording"), "{}", s.replies("start"));
    assert!(s.messages_in("meeting").iter().any(|m| m.contains("didn't start after all")));
    assert!(s.ctx.rec.recording().is_none());
    assert!(store::list(s.root.path(), 1000).is_empty(), "nothing kept");
}

#[test]
fn transcripts_run_only_what_is_installed_outside_the_config() {
    use chipsoc_config::recording::{Engine, Recording};
    let cfg = tempfile::tempdir().unwrap();
    std::fs::write(cfg.path().join("x.sh"), "#!/bin/sh\n").unwrap();
    let with = |whisper: &str, model: &str| Recording {
        transcribe: Some(Engine::WhisperCpp),
        whisper: Some(whisper.into()),
        model: Some(model.into()),
        ..Default::default()
    };
    let inside = cfg.path().join("x.sh").display().to_string();
    let e = super::engine(&with(&inside, "/usr/bin/env"), cfg.path()).unwrap_err();
    assert!(e.contains("recording.whisper") && e.contains("config folder"), "{e}");
    let e = super::engine(&with("/usr/bin/env", &inside), cfg.path()).unwrap_err();
    assert!(e.contains("recording.model"), "{e}");
    let e = super::engine(&with("/no/such/whisper-cli", "/usr/bin/env"), cfg.path()).unwrap_err();
    assert!(e.contains("isn't there"), "{e}");
    let summary = super::summary_text(
        &store::Meta::new(
            "x".into(),
            1,
            2,
            "#a_b",
            0,
            store::Person { id: 7, name: "[Ada](https://e.example)".into() },
        ),
        "link",
        Err(&e),
    );
    assert!(summary.contains("Recording of #a\\_b ended"), "{summary}");
}
