use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use chipsoc_admin::Host;
use chipsoc_config::{Lock, Snowflake};
use chipsoc_discord::fake::FakeDiscord;
use proto::Envelope;
use serde_json::Value;

fn member(id: &str, roles: &[&str]) -> Member {
    Member { id: id.into(), roles: roles.iter().map(|r| r.to_string()).collect() }
}

fn part(batch: &str, as_of: u64, part: u64, parts: u64, seq: u64, items: Vec<Member>) -> Part<Member> {
    Part { batch: batch.into(), as_of, part, parts, seq, msg: seq, kid: "k".into(), items }
}

#[test]
fn mass_revocation_is_refused() {
    assert!(!too_many_revokes(3, 3), "small changes always go through");
    assert!(!too_many_revokes(5, 10), "half is allowed");
    assert!(too_many_revokes(6, 10));
    assert!(too_many_revokes(40, 40), "an empty sync can't wipe everyone");
}

#[test]
fn many_small_revocations_add_up() {
    // Ten members granted; one taken per delta: the sixth within a day is held.
    let held: Vec<bool> = (0..7).map(|recent| must_hold_revokes(1, recent, 10 - recent)).collect();
    assert_eq!(held, [false, false, false, false, false, true, true]);
    assert!(!must_hold_revokes(0, 9, 1), "an update taking nothing is never held");
}

#[test]
fn mass_grants_are_held() {
    assert!(!too_many_grants(5, 0), "up to five always go through");
    assert!(too_many_grants(6, 0), "a new partner vouching for many at once");
    assert!(!too_many_grants(10, 20), "half of the established grants is allowed");
    assert!(too_many_grants(11, 20));
}

#[test]
fn many_small_grants_add_up() {
    // One member per envelope: the sixth within a day is held.
    let held: Vec<bool> = (0..7).map(|recent| must_hold(1, recent, 0)).collect();
    assert_eq!(held, [false, false, false, false, false, true, true]);
    assert!(!must_hold(0, 50, 0), "an update granting nothing is never held");
    assert!(!must_hold(1, 5, 40), "small next to what's established");
}

#[test]
fn only_mapped_roles_are_wanted() {
    let targets = vec![
        RoleMap { theirs: "committee".into(), ours: "partner-committee".into() },
        RoleMap { theirs: "member".into(), ours: "partner".into() },
    ];
    let want = wanted(&targets, &member("1", &["committee", "chair"]));
    assert_eq!(want, BTreeSet::from(["partner-committee".to_owned()]));
    assert!(wanted(&targets, &member("1", &[])).is_empty());
}

#[test]
fn full_sync_completes_once_every_part_is_in() {
    let mut b = HashMap::new();
    assert_eq!(add_part(&mut b, "p", part("b1", 0, 1, 2, 20, vec![member("2", &["x"])])), (Added::Waiting, vec![]));
    let again = add_part(&mut b, "p", part("b1", 0, 1, 2, 20, vec![member("2", &["x"])]));
    assert_eq!(again, (Added::Waiting, vec![]), "a repeated part");
    let (done, _) = add_part(&mut b, "p", part("b1", 0, 0, 2, 10, vec![member("1", &["x"])]));
    let Added::Complete { items, seq, msgs, .. } = done else { panic!("{done:?}") };
    assert_eq!(items, vec![member("1", &["x"]), member("2", &["x"])], "in part order");
    assert_eq!(seq, 10, "an older sender's sync counts from its first envelope");
    assert_eq!(msgs, vec![20, 10]);
    assert!(b.is_empty());

    assert_eq!(add_part(&mut b, "p", part("old", 0, 0, 2, 1, vec![member("9", &[])])).0, Added::Waiting);
    let (done, replaced) = add_part(&mut b, "p", part("new", 0, 0, 1, 2, vec![]));
    assert!(matches!(done, Added::Complete { items, .. } if items.is_empty()), "a new batch starts over");
    assert_eq!(replaced, vec![1], "the old batch's messages are done with");
}

#[test]
fn a_snapshot_counts_from_when_it_was_taken() {
    let mut b = HashMap::new();
    // Parts of the snapshot taken at seq 100 arrive at seqs 150 and 300.
    add_part(&mut b, "p", part("s1", 100, 0, 2, 150, vec![member("1", &["x"])]));
    let (done, _) = add_part(&mut b, "p", part("s1", 100, 1, 2, 300, vec![]));
    assert!(matches!(done, Added::Complete { seq: 100, .. }), "{done:?}");

    // A newer snapshot is being received; a late part of an older one is ignored.
    add_part(&mut b, "p", part("s3", 500, 0, 2, 510, vec![]));
    assert_eq!(add_part(&mut b, "p", part("s2", 400, 1, 2, 520, vec![])), (Added::Stale, vec![]));
    assert_eq!(b["p"].id, "s3", "the newer one is kept");
}

#[test]
fn partner_notes_read_as_one_short_message() {
    let log: Vec<Logged> = (1..=10)
        .map(|user| Logged::Gave { role: "Partner".into(), user, back: user == 10 })
        .chain([
            Logged::Took { role: "Partner committee".into(), user: 3 },
            Logged::NotGiven { user: 4, why: chipsoc_admin::Withheld::PowerfulNow { role: "Mod".into() } },
        ])
        .collect();
    assert_eq!(
        sync_note("SHaRC", &log),
        "🔗 **Role sync from SHaRC:** 10 given, 1 taken back.
• Gave **Partner** to <@1>, <@2>, <@3>, <@4>, <@5>, <@6>, <@7>, <@8> +2 more
• Took **Partner committee** from <@3>
• Not given: <@4>: Didn't give **Mod**: in Discord it has moderator or admin permissions now. Check the role, then try again."
    );
    assert_eq!(
        held_note("SHaRC", 40, 2, 3, 45, false),
        "⚠️ **SHaRC's role sync is held for an admin.**
• It would give partner roles to 40 more member(s) and take them from 2 (3 updates, 45 member(s) listed).
• That's more than usual in a day, so it waits in case it's a mistake on their side.
Check with SHaRC, then press **Allow** to apply just these, or **Refuse** to drop them (SHaRC isn't told). Anything held later asks again."
    );
    assert!(held_note("SHaRC", 1, 0, 1, 1, true).starts_with("⏳ Still waiting: **SHaRC's"));
}

#[test]
fn partner_text_is_shown_plain() {
    let evil = "[Free nitro](https://phish.example) <@&123> @everyone\n# Big *bold* `code`";
    let shown = plain(evil, 200);
    assert!(!shown.contains('\n'));
    let chars: Vec<char> = shown.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if "[]()<>@#*`".contains(*c) {
            assert!(i > 0 && chars[i - 1] == '\\', "unescaped {c} in {shown}");
        }
    }
    assert_eq!(plain("Plain words", 50), "Plain words");
    assert_eq!(plain("evil\u{202E}txt.exe\u{200B}", 50), "eviltxt.exe", "no bidi or zero-width tricks");
    assert_eq!(safe_link("https://github.com/a/b").as_deref(), Some("<https://github.com/a/b>"));
    assert!(safe_link("https://github.com/a/my_repo-2?tab=x#y").is_some());
    assert_eq!(safe_link("https://x> [click](https://phish)"), None);
    assert_eq!(safe_link("javascript:alert(1)"), None);
    assert_eq!(safe_link("http://github.com/a"), None, "plain https only");
    assert_eq!(safe_link("https://github.com@evil.example/x"), None, "no user name before the host");
    assert_eq!(safe_link("https://gіthub.com/a"), None, "no look-alike letters in the host");
    assert_eq!(safe_link("https://github.com/a\u{202E}b"), None);
    assert_eq!(safe_link("https://github.com/`x`"), None);
}

#[test]
fn long_answers_go_in_several_messages() {
    let lines: Vec<String> = (0..100).map(|i| format!("• project {i} {}", "x".repeat(80))).collect();
    let out = pages(&lines, 3);
    assert_eq!(out.len(), 3);
    assert!(out.iter().all(|p| p.chars().count() <= ui::MAX_CONTENT));
    assert!(out[2].ends_with("more."), "says what didn't fit");
    let few = pages(&lines[..3], 3);
    assert_eq!(few, vec![lines[..3].join("\n")]);
    assert!(pages(&[], 3).is_empty());
}

#[test]
fn snowflakes_from_time() {
    assert_eq!(snowflake_at(1_420_070_400), 0);
    assert_eq!(snowflake_at(1_420_070_401) >> 22, 1000);
    assert_eq!(posted_at(snowflake_at(1_700_000_000) + 12345), 1_700_000_000);
}

#[test]
fn budgets_reset_hourly() {
    let mut b = Budget::default();
    let spent: Vec<Spent> = (0..4).map(|_| b.spend(2, 10_000)).collect();
    assert_eq!(spent, [Spent::Ok, Spent::Ok, Spent::JustOver, Spent::Over]);
    assert_eq!(b.spend(2, 10_000 + 3600), Spent::Ok, "a new hour");
}

async fn store_with_grants() -> Store {
    let s = Store::open("sqlite::memory:").await.unwrap();
    let f = s.fed("1000");
    f.grant("p", "11", "partner", "old", 1).await.unwrap();
    f.grant("p", "12", "partner", "new", 1).await.unwrap();
    f.grant("p", "13", "partner", "old", 1).await.unwrap();
    f.grant("q", "13", "partner", "q", 1).await.unwrap();
    f.set_catalog("p", "{}", 1, 1).await.unwrap();
    s
}

fn desired() -> Snapshot {
    let mut lock = Lock::default();
    lock.roles.insert("partner".into(), Snowflake(77));
    Snapshot { lock, ..Snapshot::default() }
}

#[tokio::test]
async fn a_removed_key_loses_what_it_signed() {
    let s = store_with_grants().await;
    let fake = FakeDiscord::new();
    for u in [11, 12, 13] {
        fake.join(u);
        fake.call(Method::Put, &format!("guilds/1000/members/{u}/roles/77"), None).await.unwrap();
    }
    let r = revoke_grants(&fake, &s, "1000", &desired(), "p", Which::Kids(&["old".to_owned()])).await.unwrap();
    assert_eq!(r, Revoked { removed: 1, kept: 1, failed: 0, members: 2 });
    assert!(fake.roles_of(11).is_empty());
    assert_eq!(fake.roles_of(12), ["77"], "signed by the key still listed");
    assert_eq!(fake.roles_of(13), ["77"], "another partner also vouches for them");
    let left: Vec<String> = s.fed("1000").peer_grants("p").await.unwrap().into_iter().map(|g| g.user_id).collect();
    assert_eq!(left, ["12"]);
    assert_eq!(s.fed("1000").catalogs().await.unwrap().len(), 1, "a partner still configured keeps its catalogue");
}

#[tokio::test]
async fn a_removed_partner_loses_everything_it_gave() {
    let s = store_with_grants().await;
    let fake = FakeDiscord::new();
    fake.join(11);
    fake.call(Method::Put, "guilds/1000/members/11/roles/77", None).await.unwrap();
    // 12 has left the server; 13's role stays, as q also gave it.
    fake.join(13);
    fake.call(Method::Put, "guilds/1000/members/13/roles/77", None).await.unwrap();
    fake.fail("DELETE guilds/1000/members/11/roles/77", 500, false);
    let r = revoke_grants(&fake, &s, "1000", &desired(), "p", Which::All).await.unwrap();
    assert_eq!((r.failed, r.kept), (1, 1));
    assert_eq!(s.fed("1000").peer_grants("p").await.unwrap().len(), 1, "the failed one is kept to retry");
    assert!(s.fed("1000").catalogs().await.unwrap().is_empty(), "catalogue wiped");

    let r = revoke_grants(&fake, &s, "1000", &desired(), "p", Which::All).await.unwrap();
    assert_eq!(r, Revoked { removed: 1, kept: 0, failed: 0, members: 1 });
    assert!(fake.roles_of(11).is_empty());
    assert_eq!(fake.roles_of(13), ["77"]);
    assert_eq!(s.fed("1000").peers_with_state().await.unwrap(), ["q"]);
}

// --- The receiving side end to end, on a fake Discord and an in-memory store.

/// The fake Discord, shared with the engine that sets it up. Calls
/// starting with `stall` never return, as if the bot stopped right there.
#[derive(Clone)]
struct Fake {
    discord: Arc<FakeDiscord>,
    stall: Arc<Mutex<Option<String>>>,
    /// Told when a call stalls.
    stalled: Arc<tokio::sync::Notify>,
    /// Every call made, reads too.
    seen: Arc<Mutex<Vec<String>>>,
}

impl Rest for Fake {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let call = format!("{method:?} {path}").to_uppercase();
        self.seen.lock().unwrap().push(call.clone());
        let stalled = self.stall.lock().unwrap().as_ref().is_some_and(|s| call.starts_with(&s.to_uppercase()));
        if stalled {
            self.stalled.notify_one();
            std::future::pending::<()>().await;
        }
        self.discord.call(method, path, body).await
    }
}

struct TestEnv {
    rest: Fake,
    store: Store,
    cfg: Mutex<Arc<Config>>,
    state: fedsync::State,
    logs: Mutex<Vec<String>>,
    /// Questions with an Allow button: (text, custom id).
    asked: Mutex<Vec<(String, String)>>,
    /// The partner's keys: the one it signs with, and an older one.
    key: proto::SecretKey,
    old_key: proto::SecretKey,
    next_msg: AtomicU64,
    /// Envelopes this server sent: (partner id, message).
    wire: Mutex<Vec<(String, String)>>,
    _dir: tempfile::TempDir,
}

impl chipsoc_admin::Host for TestEnv {
    type Rest = Fake;

    fn rest(&self) -> &Fake {
        &self.rest
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
        false
    }

    async fn record(
        &self,
        _: &[chipsoc_config::ops::Op],
        _: Snowflake,
        _: &[String],
    ) -> Result<chipsoc_admin::Recorded> {
        bail!("no config edits here")
    }
}

impl Env for TestEnv {
    fn store(&self) -> &Store {
        &self.store
    }

    fn gkey(&self) -> &str {
        "1000"
    }

    fn state(&self) -> &fedsync::State {
        &self.state
    }

    async fn log_bot(&self, text: &str) {
        self.logs.lock().unwrap().push(text.to_owned());
    }

    async fn audit(&self, _: AuditAction, _: &str, _: &str) {}

    async fn ask(&self, text: &str, button: &str) {
        self.asked.lock().unwrap().push((text.to_owned(), button.to_owned()));
    }

    async fn deliver(&self, peer: &Peer, content: &str) -> bool {
        self.wire.lock().unwrap().push((peer.id.clone(), content.to_owned()));
        true
    }
}

const MEMBERS: std::ops::RangeInclusive<u64> = 101..=140;

fn write(dir: &std::path::Path, file: &str, body: &str) {
    std::fs::write(dir.join(file), body).unwrap();
}

/// A server the config has been applied to, partnered with `them`, which
/// vouches for `member` (→ our `partner`) and `committee` (→ `partner-committee`).
async fn env() -> TestEnv {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    let (key, old_key) = (proto::generate().unwrap(), proto::generate().unwrap());
    write(p, "server.toml", "[guild]\nname = \"Test\"\n");
    write(p, "bot.toml", "name = \"Test\"\n");
    write(
        p,
        "roles.toml",
        "[[role]]\nkey = \"bot\"\nname = \"Bot\"\nmanaged = true\n\n[[role]]\nkey = \"partner\"\nname = \"Partner\"\n\n\
         [[role]]\nkey = \"partner-committee\"\nname = \"Partner committee\"\n\n\
         [[role]]\nkey = \"everyone\"\nname = \"@everyone\"\npermissions = [\"VIEW_CHANNEL\"]\n",
    );
    write(
        p,
        "federation.toml",
        &format!(
            "name = \"us\"\n[[peer]]\nname = \"Them\"\nid = \"them\"\npublic_key = \"{}\"\npublic_keys = [\"{}\"]\n\
             [[peer.role_map]]\ntheirs = \"member\"\nours = \"partner\"\n\
             [[peer.role_map]]\ntheirs = \"committee\"\nours = \"partner-committee\"\n",
            key.1, old_key.1
        ),
    );
    let discord = Arc::new(FakeDiscord::new());
    for u in MEMBERS {
        discord.join(u);
    }
    let rest = Fake { discord, stall: Arc::default(), stalled: Arc::default(), seen: Arc::default() };
    let mut e = chipsoc_engine::Engine::load(rest.clone(), p).unwrap();
    let plan = e.plan(chipsoc_engine::Options::default()).await.unwrap();
    e.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
    let cfg = Config::load(p).unwrap();
    let state = fedsync::State::new(&cfg);
    TestEnv {
        rest,
        store: Store::open("sqlite::memory:").await.unwrap(),
        cfg: Mutex::new(Arc::new(cfg)),
        state,
        logs: Mutex::default(),
        asked: Mutex::default(),
        key: proto::signing_key(&key.0).unwrap(),
        old_key: proto::signing_key(&old_key.0).unwrap(),
        next_msg: AtomicU64::new(snowflake_at(now())),
        wire: Mutex::default(),
        _dir: dir,
    }
}

fn now_ms() -> u64 {
    now() as u64 * 1000
}

fn delta(members: &[(u64, &[&str])]) -> Body {
    Body::RolesDelta { members: members.iter().map(|(u, r)| member(&u.to_string(), r)).collect() }
}

fn full(batch: &str, part: u64, parts: u64, as_of: u64, members: &[(u64, &[&str])]) -> Body {
    let members: Vec<Member> = members.iter().map(|(u, r)| member(&u.to_string(), r)).collect();
    Body::Roles { batch: batch.into(), part, parts, total: members.len() as u64, members, as_of }
}

impl TestEnv {
    fn seal_with(&self, key: &proto::SecretKey, seq: u64, ts: i64, body: Body) -> String {
        proto::seal(key, &Envelope::new("them", "us", seq, ts, body).unwrap())
    }

    fn msg_id(&self) -> u64 {
        self.next_msg.fetch_add(1, Ordering::Relaxed)
    }

    /// Post an envelope to the inbox and let the worker handle it.
    async fn post(&self, content: String) -> u64 {
        let msg = self.msg_id();
        arrived(self, msg, &content).await;
        let job = self.state.jobs.pop().await;
        run_job(self, job).await;
        msg
    }

    async fn send(&self, seq: u64, body: Body) -> u64 {
        self.post(self.seal_with(&self.key, seq, now(), body)).await
    }

    fn has(&self, user: u64, key: &str) -> bool {
        let id = self.cfg().desired.lock.role(key).unwrap().to_string();
        self.rest.discord.roles_of(user).contains(&id)
    }

    fn holding(&self, key: &str) -> Vec<u64> {
        MEMBERS.filter(|u| self.has(*u, key)).collect()
    }

    fn edit_cfg(&self, f: impl FnOnce(&mut Config)) {
        let mut c = (*self.cfg()).clone();
        f(&mut c);
        *self.cfg.lock().unwrap() = Arc::new(c);
    }

    fn logs(&self) -> Vec<String> {
        self.logs.lock().unwrap().clone()
    }

    /// Pretend an admin was last asked long enough ago to ask again.
    async fn ask_again_now(&self) {
        self.state.inbox.lock().await.asked.clear();
    }

    /// The batch named by the last question's Allow button.
    fn last_batch(&self) -> String {
        let asked = self.asked.lock().unwrap();
        let id = &asked.last().expect("an admin was asked").1;
        let Some(CustomId::AllowSync { peer, batch }) = CustomId::parse(id) else { panic!("{id}") };
        assert_eq!(peer, "them");
        batch
    }

    /// Members granted `partner` before the guard's window, so they count as established.
    async fn established(&self, users: impl IntoIterator<Item = u64>) {
        let role = self.cfg().desired.lock.role("partner").unwrap();
        for u in users {
            db(self).grant("them", &u.to_string(), "partner", &proto::kid(&self.key.verifying_key()), 1).await.unwrap();
            db(self).claim_seq("them", &u.to_string(), 1).await.unwrap();
            self.rest.discord.call(Method::Put, &format!("guilds/1000/members/{u}/roles/{role}"), None).await.unwrap();
        }
    }
}

#[tokio::test]
async fn a_delta_gives_and_takes_mapped_roles() {
    let e = env().await;
    e.send(now_ms(), delta(&[(101, &["member", "chair"]), (102, &["committee"])])).await;
    assert!(e.has(101, "partner") && e.has(102, "partner-committee") && !e.has(101, "partner-committee"));
    e.send(now_ms() + 1, delta(&[(101, &[])])).await;
    assert!(!e.has(101, "partner"));
    assert!(e.logs().iter().any(|l| l.contains("Took **Partner** from <@101>")), "{:?}", e.logs());
}

#[tokio::test]
async fn a_sync_asks_discord_for_the_roles_once() {
    let e = env().await;
    e.established(MEMBERS.take(30)).await;
    e.rest.seen.lock().unwrap().clear();
    let gaining: Vec<(u64, &[&str])> = (131..=135).map(|u| (u, &["member"][..])).collect();
    e.send(now_ms(), delta(&gaining)).await;
    assert_eq!(e.holding("partner").len(), 35);
    let seen = e.rest.seen.lock().unwrap().clone();
    let reads = seen.iter().filter(|c| *c == "GET GUILDS/1000/ROLES").count();
    assert_eq!(reads, 1, "{seen:?}");
}

#[tokio::test]
async fn a_member_who_rejoins_gets_partner_roles_back() {
    let e = env().await;
    e.send(now_ms(), delta(&[(101, &["member", "committee"])])).await;
    assert!(e.has(101, "partner") && e.has(101, "partner-committee"));
    // They leave and come back with no roles; the partner's word is still on record.
    e.rest.discord.state.lock().unwrap().members.retain(|m| m["user"]["id"] != "101");
    e.rest.discord.join(101);
    // The partner's next update says the same: nothing to change, so it gives nothing.
    e.send(now_ms() + 1, delta(&[(101, &["member", "committee"])])).await;
    assert!(!e.has(101, "partner"), "the bug: grants matched, roles never came back");
    // The join gives back what the partner still vouches for, through role_map entries still there.
    e.edit_cfg(|c| c.federation.peers[0].role_map.retain(|m| m.ours == "partner"));
    rejoined(&e, 101).await;
    assert!(e.has(101, "partner") && !e.has(101, "partner-committee"));
    assert!(e.logs().iter().any(|l| l.contains("back after rejoining")), "{:?}", e.logs());
    // Someone the partner never vouched for gets nothing.
    rejoined(&e, 102).await;
    assert!(!e.has(102, "partner"));
}

#[tokio::test]
async fn a_stolen_key_cannot_freeze_a_member() {
    // Red-team PoC (F-M1): seq = i64::MAX from a stolen key used to block every later update.
    let e = env().await;
    e.send(now_ms(), delta(&[(101, &["member"])])).await;
    assert!(e.has(101, "partner"));
    e.post(e.seal_with(&e.old_key, i64::MAX as u64, now(), delta(&[(101, &[])]))).await;
    assert!(e.has(101, "partner"), "refused: from the future");
    assert!(e.logs().iter().any(|l| l.contains("seq from the future")), "{:?}", e.logs());
    let snapshot = full("evil", 0, 1, u64::MAX / 2, &[]);
    e.post(e.seal_with(&e.old_key, now_ms(), now(), snapshot)).await;
    assert!(e.has(101, "partner"), "a full sync taken in the future is refused too");

    // The most it can do is a seq a few minutes ahead, which the key's removal clears.
    let ahead = now_ms() + 4 * 60 * 1000;
    e.post(e.seal_with(&e.old_key, ahead, now(), delta(&[(101, &[])]))).await;
    assert!(!e.has(101, "partner"));
    e.send(now_ms() + 2, delta(&[(101, &["member"])])).await;
    assert!(!e.has(101, "partner"), "honest updates lose to it for now");
    check_peers(&e).await; // records the keys
    e.edit_cfg(|c| c.federation.peers[0].public_keys.clear());
    check_peers(&e).await;
    e.send(now_ms() + 3, delta(&[(101, &["member"])])).await;
    assert!(e.has(101, "partner"), "removing the stolen key let honest updates apply again");
}

#[tokio::test]
async fn allow_releases_only_what_it_asked_about() {
    // Red-team PoC (F-M2): Allow on "6 members" used to release a 5,000-member sync held after it.
    let e = env().await;
    let six: Vec<(u64, &[&str])> = (101..=106).map(|u| (u, &["member"][..])).collect();
    e.send(now_ms(), delta(&six)).await;
    assert!(e.holding("partner").is_empty(), "held");
    let first = e.last_batch();
    assert!(e.asked.lock().unwrap()[0].0.contains("6 more member(s)"));

    let many: Vec<(u64, &[&str])> = MEMBERS.map(|u| (u, &["member"][..])).collect();
    e.send(now_ms() + 1, full("big", 0, 1, now_ms() + 1, &many)).await;
    assert_eq!(e.asked.lock().unwrap().len(), 1, "not asked again yet");

    release(&e, "them", &first).await;
    assert_eq!(e.holding("partner"), (101..=106).collect::<Vec<_>>(), "just what the question was about");
    release(&e, "them", &first).await;
    assert_eq!(e.holding("partner").len(), 6, "an old button does nothing more");
    release(&e, "them", "made-up").await;
    assert_eq!(e.holding("partner").len(), 6);

    // What was held later gets its own question (after a pause, or on the next check).
    e.ask_again_now().await;
    let cfg = e.cfg();
    ask(&e, &cfg.federation.peers[0]).await;
    let second = e.last_batch();
    assert_ne!(second, first);
    assert!(e.asked.lock().unwrap()[1].0.contains("40 member(s) listed"), "{:?}", e.asked.lock().unwrap());
    release(&e, "them", &second).await;
    assert_eq!(e.holding("partner").len(), 40);
}

#[tokio::test]
async fn refuse_drops_just_that_batch_and_it_isnt_asked_about_again() {
    let e = env().await;
    let six: Vec<(u64, &[&str])> = (101..=106).map(|u| (u, &["member"][..])).collect();
    let msg = e.send(now_ms(), delta(&six)).await;
    let first = e.last_batch();
    assert!(e.asked.lock().unwrap()[0].0.contains("**Refuse**"), "the question offers it");
    assert_eq!(refuse(&e, "them", "made-up").await, 0);
    assert_eq!(refuse(&e, "them", &first).await, 1);
    assert_eq!(db(&e).inbox_state(msg).await.unwrap(), Some(InboxState::Done), "dropped for good");
    assert_eq!(refuse(&e, "them", &first).await, 0, "an old button does nothing");
    release(&e, "them", &first).await;
    assert!(e.holding("partner").is_empty(), "nor does Allow after it");

    // No reminder a day later: nothing's waiting.
    e.state.inbox.lock().await.batches.insert(first.clone(), now() - REMIND_EVERY);
    let cfg = e.cfg();
    ask(&e, &cfg.federation.peers[0]).await;
    assert_eq!(e.asked.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn removing_a_key_drops_what_it_has_held() {
    let e = env().await;
    check_peers(&e).await;
    let six: Vec<(u64, &[&str])> = (101..=106).map(|u| (u, &["member"][..])).collect();
    let msg = e.post(e.seal_with(&e.old_key, now_ms(), now(), delta(&six))).await;
    let batch = e.last_batch();
    e.edit_cfg(|c| c.federation.peers[0].public_keys.clear());
    check_peers(&e).await;
    assert_eq!(db(&e).inbox_state(msg).await.unwrap(), Some(InboxState::Done));
    release(&e, "them", &batch).await;
    assert!(e.holding("partner").is_empty(), "an Allow from before the key went does nothing");
}

#[tokio::test]
async fn held_updates_survive_a_restart_and_are_reminded() {
    let e = env().await;
    let six: Vec<(u64, &[&str])> = (101..=106).map(|u| (u, &["member"][..])).collect();
    e.send(now_ms(), delta(&six)).await;
    let batch = e.last_batch();

    // Restart: what was held is read back into the same batch, without asking again.
    start(&e).await;
    replay(&e, false).await;
    assert_eq!(e.asked.lock().unwrap().len(), 1);
    e.state.inbox.lock().await.batches.insert(batch.clone(), now() - REMIND_EVERY);
    let cfg = e.cfg();
    ask(&e, &cfg.federation.peers[0]).await;
    let asked = e.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 2);
    assert!(asked[1].0.starts_with("⏳ Still waiting") && asked[1].1 == asked[0].1, "the same button again");

    // Pruning leaves it alone, and the button from before the restart still works.
    prune(&e).await;
    release(&e, "them", &batch).await;
    assert_eq!(e.holding("partner").len(), 6);
}

#[tokio::test]
async fn a_held_message_still_verifies_days_later() {
    // Judged by when Discord says it was posted, not when it's read back.
    let e = env().await;
    let then = now() - 3 * 24 * 3600;
    let msg = snowflake_at(then) + 7;
    let content = e.seal_with(&e.key, then as u64 * 1000, then, delta(&[(101, &["member"])]));
    db(&e).inbox_add(msg, &content, then).await.unwrap();
    handle(&e, msg, &content, Gate::Allowed, false, 0).await;
    assert!(e.has(101, "partner"), "{:?}", e.logs());
    // Posting the same envelope again now is refused: it's too old for a new message.
    e.post(content).await;
    assert!(e.logs().iter().any(|l| l.contains("too old")), "{:?}", e.logs());
}

#[tokio::test]
async fn removing_a_role_from_role_map_takes_it_back() {
    // F-M3: grants used to stay until the partner next mentioned the member.
    let e = env().await;
    e.send(now_ms(), delta(&[(101, &["member", "committee"]), (102, &["member"])])).await;
    assert!(e.has(101, "partner") && e.has(101, "partner-committee"));
    e.edit_cfg(|c| c.federation.peers[0].role_map.retain(|m| m.ours != "partner"));
    check_peers(&e).await;
    assert!(e.holding("partner").is_empty() && e.has(101, "partner-committee"));
    assert!(db(&e).grants("them", "102").await.unwrap().is_empty());
    let logs = e.logs();
    assert!(logs.iter().any(|l| l.contains("no longer gives **Partner**") && l.contains("2 role(s) removed")));
    check_peers(&e).await;
    assert_eq!(e.logs().len(), logs.len(), "said once");
}

#[tokio::test]
async fn a_role_made_powerful_in_discord_is_taken_back() {
    let e = env().await;
    e.send(now_ms(), delta(&[(101, &["member"])])).await;
    assert!(e.has(101, "partner"));
    let id = e.cfg().desired.lock.role("partner").unwrap().to_string();
    {
        let mut st = e.rest.discord.state.lock().unwrap();
        let role = st.roles.iter_mut().find(|r| r["id"] == id.as_str()).unwrap();
        role["permissions"] = "8".into(); // Administrator, given in Discord's UI
    }
    check_peers(&e).await;
    assert!(!e.has(101, "partner"));
    assert!(e.logs().iter().any(|l| l.contains("moderator or admin permissions")), "{:?}", e.logs());
}

#[tokio::test]
async fn a_flood_cannot_push_out_saved_messages_or_control_jobs() {
    // F-M4: the queue drops the oldest when full, and used to hold the only copy.
    let e = env().await;
    let real = e.seal_with(&e.key, now_ms(), now(), delta(&[(101, &["member"])]));
    arrived(&e, e.msg_id(), &real).await;
    e.state.control.push(|t| t.peers = true);
    for i in 0..1001 {
        arrived(&e, e.msg_id(), &format!("guildfed/2\njunk {i}")).await;
    }
    assert!(e.state.control.next().await.peers, "control jobs aren't in the droppable queue");
    every_few_minutes(&e).await;
    assert!(e.has(101, "partner"), "read back from the store");
}

#[tokio::test]
async fn a_full_sync_cut_short_is_applied_again() {
    // F-L1 and F-L3: the bot stopping mid-sync used to lose the rest of it, and a
    // role given just before a stop was never recorded as the partner's.
    let e = env().await;
    e.established(120..=129).await;
    let role = e.cfg().desired.lock.role("partner").unwrap();
    *e.rest.stall.lock().unwrap() = Some(format!("PUT guilds/1000/members/102/roles/{role}"));
    let members: [(u64, &[&str]); 3] = [(101, &["member"]), (102, &["member"]), (103, &["member"])];
    let members: Vec<_> = members.into_iter().chain((120..=129).map(|u| (u, &["member"][..]))).collect();
    let content = e.seal_with(&e.key, now_ms(), now(), full("s", 0, 1, now_ms(), &members));
    let msg = e.msg_id();
    arrived(&e, msg, &content).await;
    let job = e.state.jobs.pop().await;
    // Stopped mid-sync: at the stalled call, however long getting there takes.
    tokio::select! {
        _ = run_job(&e, job) => panic!("the sync went past the stalled call"),
        _ = e.rest.stalled.notified() => {}
    }
    assert!(e.has(101, "partner") && !e.has(103, "partner"));
    assert_eq!(db(&e).grants("them", "102").await.unwrap(), ["partner"], "recorded before giving the role");

    // Restart.
    *e.rest.stall.lock().unwrap() = None;
    start(&e).await;
    assert!(db(&e).pending_grants().await.unwrap().is_empty(), "the role never got there: forgotten");
    replay(&e, false).await;
    assert_eq!(e.holding("partner"), [101, 102, 103].into_iter().chain(120..=129).collect::<Vec<_>>());
    assert_eq!(db(&e).inbox_state(msg).await.unwrap(), Some(InboxState::Done));
}

#[tokio::test]
async fn a_pending_grant_that_reached_discord_is_kept() {
    let e = env().await;
    let role = e.cfg().desired.lock.role("partner").unwrap();
    db(&e).grant_pending("them", "101", "partner", "k", now()).await.unwrap();
    e.rest.discord.call(Method::Put, &format!("guilds/1000/members/101/roles/{role}"), None).await.unwrap();
    reconcile(&e).await;
    assert!(db(&e).pending_grants().await.unwrap().is_empty());
    assert_eq!(db(&e).grants("them", "101").await.unwrap(), ["partner"], "still the partner's to take back");
}

#[tokio::test]
async fn revocations_spread_over_many_updates_are_held() {
    // F-L2: only members absent from a full sync used to count.
    let e = env().await;
    e.established(101..=110).await;
    for u in 101..=105 {
        e.send(now_ms() + u, delta(&[(u, &[])])).await;
    }
    assert_eq!(e.holding("partner"), (106..=110).collect::<Vec<_>>());
    assert!(e.asked.lock().unwrap().is_empty());
    e.send(now_ms() + 200, delta(&[(106, &[])])).await;
    assert!(e.has(106, "partner"), "the sixth in a day waits for an admin");
    assert!(e.asked.lock().unwrap()[0].0.contains("take them from 1"));
    // A full sync listing everyone with no roles counts the same.
    let none: Vec<(u64, &[&str])> = (107..=110).map(|u| (u, &[][..])).collect();
    e.send(now_ms() + 300, full("s", 0, 1, now_ms() + 300, &none)).await;
    assert_eq!(e.holding("partner"), (106..=110).collect::<Vec<_>>());
}

#[tokio::test]
async fn failed_revocations_are_tried_again() {
    // F-L4: they used to wait for a restart or a config change.
    let e = env().await;
    e.send(now_ms(), delta(&[(101, &["member"])])).await;
    let role = e.cfg().desired.lock.role("partner").unwrap();
    for _ in 0..2 {
        e.rest.discord.fail(&format!("DELETE guilds/1000/members/101/roles/{role}"), 500, false);
    }
    e.edit_cfg(|c| c.federation.peers.clear());
    check_peers(&e).await;
    assert!(e.has(101, "partner"));
    every_few_minutes(&e).await;
    assert!(e.has(101, "partner"));
    let said = e.logs().iter().filter(|l| l.contains("no longer a partner")).count();
    assert_eq!(said, 1, "a failure is said once, not every few minutes");
    every_few_minutes(&e).await;
    assert!(!e.has(101, "partner"));
    assert!(db(&e).peers_with_state().await.unwrap().is_empty());
}

#[tokio::test]
async fn failed_member_updates_are_kept_to_retry() {
    // F-L5: the retry list was only in memory.
    let e = env().await;
    let role = e.cfg().desired.lock.role("partner").unwrap();
    e.rest.discord.fail(&format!("PUT guilds/1000/members/101/roles/{role}"), 500, false);
    e.send(now_ms(), delta(&[(101, &["member"])])).await;
    assert!(!e.has(101, "partner"));
    assert!(db(&e).grants("them", "101").await.unwrap().is_empty(), "a grant whose role failed is forgotten");
    assert_eq!(db(&e).retries().await.unwrap().len(), 1, "kept in the store");
    start(&e).await;
    retry_pending(&e).await;
    assert!(e.has(101, "partner"));
    assert!(db(&e).retries().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_partner_is_capped() {
    // F-L7 and F-L9: envelopes per hour, held updates, catalogue size.
    let e = env().await;
    e.state.inbox.lock().await.rate.insert("them".into(), Budget { from: now(), used: MAX_PER_HOUR });
    e.send(now_ms(), delta(&[(101, &["member"])])).await;
    assert!(!e.has(101, "partner"));
    assert!(e.logs().iter().any(|l| l.contains("ignoring more until the hour's up")));
    e.state.inbox.lock().await.rate.clear();

    let cfg = e.cfg();
    let peer = &cfg.federation.peers[0];
    for i in 0..=MAX_HELD as u64 {
        let u = Update { members: vec![], seq: i, full: false, kid: "k".into(), msgs: vec![i] };
        hold(&e, peer, u, None).await;
    }
    assert_eq!(e.state.inbox.lock().await.held["them"].len(), MAX_HELD);

    let project = |i: usize| proto::Project {
        slug: format!("p{i}"),
        name: "x".repeat(100),
        blurb: "y".repeat(400),
        kind: String::new(),
        status: String::new(),
        github: None,
        open: false,
    };
    let big: Vec<proto::Project> = (0..600).map(project).collect();
    assert_eq!(keep_catalogue(&e, peer, big, now(), 1).await, Outcome::Done);
    assert!(db(&e).catalogs().await.unwrap().is_empty(), "not kept");
    assert_eq!(keep_catalogue(&e, peer, vec![project(1)], now(), 2).await, Outcome::Done);
    assert_eq!(keep_catalogue(&e, peer, vec![project(1)], now(), 3).await, Outcome::Done);
    let shared = e.logs().iter().filter(|l| l.contains("shared their projects")).count();
    assert_eq!(shared, 1, "said only when it changed");
    let parts = Body::Catalog { projects: vec![], batch: "b".into(), part: 0, parts: MAX_CATALOG_PARTS + 1 };
    e.send(now_ms(), parts).await;
    assert!(e.state.inbox.lock().await.catalogs.is_empty(), "too many parts");
}

// --- Shared content, between two simulated servers: "them" shares, "us" takes.

const CHANNELS: &str = "[category]\nkey = \"a\"\nname = \"A\"\n\n\
    [[channel]]\nkey = \"review\"\nname = \"review\"\n\n\
    [[channel]]\nkey = \"news\"\nname = \"news\"\n\n\
    [[channel]]\nkey = \"events\"\nname = \"events\"\n\n\
    [[channel]]\nkey = \"partner-news\"\nname = \"partner-news\"\n\n\
    [[channel]]\nkey = \"resources\"\nname = \"resources\"\ntype = \"forum\"\n\n\
    [[channel]]\nkey = \"help\"\nname = \"help\"\ntype = \"forum\"\n\n\
    [[channel]]\nkey = \"partner-resources\"\nname = \"partner-resources\"\ntype = \"forum\"\n\n\
    [[channel]]\nkey = \"partner-questions\"\nname = \"partner-questions\"\ntype = \"forum\"\n\n\
    [[channel]]\nkey = \"people\"\nname = \"people\"\n\n\
    [[channel]]\nkey = \"committee\"\nname = \"committee\"\n\
    [[channel.overwrites]]\nrole = \"everyone\"\ndeny = [\"VIEW_CHANNEL\"]\n";

/// A server named `name` that signs with `own`, partnered with each of
/// `peers`: (name, id, public key, extra lines for its `[[peer]]`).
async fn server(name: &str, own: &proto::SecretKey, peers: &[(&str, &str, String, &str)]) -> TestEnv {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::create_dir_all(p.join("categories")).unwrap();
    write(p, "server.toml", "[guild]\nname = \"Test\"\n");
    write(
        p,
        "bot.toml",
        "name = \"Test\"\ntimezone = \"Europe/London\"\n[review]\nchannel = \"review\"\n[resources]\nforum = \"resources\"\n\
         [help]\nforum = \"help\"\n[events]\nchannel = \"events\"\n[log]\npeople = \"people\"\n",
    );
    write(
        p,
        "roles.toml",
        "[[role]]\nkey = \"bot\"\nname = \"Bot\"\nmanaged = true\n\n[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\npermissions = [\"VIEW_CHANNEL\"]\n",
    );
    write(&p.join("categories"), "10-a.toml", CHANNELS);
    let mut fed = format!("name = \"{name}\"\ninbox = \"review\"\n");
    for (pname, id, key, extra) in peers {
        fed.push_str(&format!(
            "[[peer]]\nname = \"{pname}\"\nid = \"{id}\"\npublic_key = \"{key}\"\nwebhook_env = \"X\"\n{extra}\n"
        ));
    }
    write(p, "federation.toml", &fed);
    let discord = Arc::new(FakeDiscord::new());
    for u in MEMBERS {
        discord.join(u);
    }
    let rest = Fake { discord, stall: Arc::default(), stalled: Arc::default(), seen: Arc::default() };
    let mut e = chipsoc_engine::Engine::load(rest.clone(), p).unwrap();
    let plan = e.plan(chipsoc_engine::Options::default()).await.unwrap();
    e.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
    let cfg = Config::load(p).unwrap();
    let junk = || proto::signing_key(&proto::generate().unwrap().0).unwrap();
    TestEnv {
        rest,
        store: Store::open("sqlite::memory:").await.unwrap(),
        cfg: Mutex::new(Arc::new(cfg)),
        state: fedsync::State::with_key(Some(own.clone())),
        logs: Mutex::default(),
        asked: Mutex::default(),
        key: junk(),
        old_key: junk(),
        next_msg: AtomicU64::new(snowflake_at(now())),
        wire: Mutex::default(),
        _dir: dir,
    }
}

fn keypair() -> (proto::SecretKey, String) {
    let (secret, public) = proto::generate().unwrap();
    (proto::signing_key(&secret).unwrap(), public)
}

/// "them" (sending `send` to us, and to "other") and "us" (taking what `accept` says from them).
async fn pair(send: &str, accept: &str) -> (TestEnv, TestEnv) {
    let ((them, them_pub), (us, us_pub)) = (keypair(), keypair());
    let send = format!("send = [{send}]");
    let a = server("them", &them, &[("Us", "us", us_pub, &send), ("Other", "other", keypair().1, &send)]).await;
    let b = server("us", &us, &[("Them", "them", them_pub, accept)]).await;
    // Ada, on their server.
    {
        let st = &mut a.rest.discord.state.lock().unwrap();
        if let Some(m) = st.members.iter_mut().find(|m| m["user"]["id"] == "101") {
            m["nick"] = serde_json::json!("Ada");
        }
    }
    (a, b)
}

impl TestEnv {
    /// Send what's queued, and hand partner `to` what it was sent.
    async fn send_to(&self, to: &TestEnv) -> usize {
        crate::fedshare::flush(self).await;
        let sent: Vec<(String, String)> = std::mem::take(&mut *self.wire.lock().unwrap());
        let ours = to.cfg().federation.name.clone();
        let mut n = 0;
        for (peer, content) in sent {
            if peer == ours {
                to.post(content).await;
                n += 1;
            }
        }
        n
    }

    fn channel_id(&self, key: &str) -> String {
        self.cfg().desired.lock.channel(key).unwrap().to_string()
    }

    /// Messages posted in a channel (or forum post), oldest first.
    fn messages_in(&self, key_or_id: &str) -> Vec<Value> {
        let id = self.cfg().desired.lock.channel(key_or_id).map_or(key_or_id.to_owned(), |c| c.to_string());
        let st = self.rest.discord.state.lock().unwrap();
        st.messages.iter().filter(|m| m["channel_id"] == id.as_str()).cloned().collect()
    }

    /// Posts in a forum: (thread, as the fake keeps it).
    fn threads_in(&self, key: &str) -> Vec<Value> {
        let id = self.channel_id(key);
        let st = self.rest.discord.state.lock().unwrap();
        st.channels.iter().filter(|c| c["thread"] == true && c["parent_id"] == id.as_str()).cloned().collect()
    }

    fn admin(&self) -> chipsoc_admin::Actor {
        chipsoc_admin::Actor { id: Some(Snowflake(140)), name: "admin".into() }
    }
}

#[tokio::test]
async fn a_shared_resource_waits_for_review_then_posts_with_attribution() {
    let (a, b) = pair("\"resources\"", "accept = { resources = \"partner-resources\" }").await;
    let r = chipsoc_admin::NewResource {
        title: "Soldering guide".into(),
        url: "https://example.com/solder".into(),
        description: "Great for *beginners* @everyone".into(),
        tags: vec![],
    };
    let post =
        chipsoc_admin::post_resource(&a, &r, &chipsoc_admin::Actor { id: Some(Snowflake(101)), name: "x".into() })
            .await
            .unwrap();
    assert_eq!(a.send_to(&b).await, 1);
    assert!(a.wire.lock().unwrap().is_empty());

    let waiting = chipsoc_admin::pending(&b).await.unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!((waiting[0].kind.as_str(), waiting[0].what.as_str()), ("partner-resource", "Soldering guide"));
    let card = b.messages_in("review").pop().expect("a review card");
    let text = card["content"].as_str().unwrap();
    assert!(text.contains("From Them · resource: Soldering guide") && text.contains("shared by Ada"), "{text}");
    assert!(card["components"][0]["components"][0]["custom_id"].as_str().unwrap().starts_with("rev:accept:"));
    assert!(b.threads_in("partner-resources").is_empty(), "nothing posted before review");

    let d = chipsoc_admin::decide(&b, waiting[0].id, true, &b.admin()).await.unwrap();
    assert!(d.accepted && d.outcome.contains("Posted:"), "{d:?}");
    let threads = b.threads_in("partner-resources");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0]["name"], "Soldering guide");
    let body = threads[0]["message"]["content"].as_str().unwrap().to_owned();
    assert!(body.contains("<https://example.com/solder>"), "{body}");
    assert!(body.contains("\\*beginners\\* \\@everyone"), "escaped: {body}");
    assert!(body.contains(&format!("From **Them** · shared by Ada · <{}>", post.link)), "{body}");
    assert_eq!(threads[0]["message"]["allowed_mentions"]["parse"], serde_json::json!([]));

    // Edited, then archived on their side: the post here follows.
    let mut item =
        chipsoc_admin::shares::shared(&a, ShareKindAlias::Resource, &post.id.to_string()).await.unwrap().unwrap();
    item.text = "Now with pictures".into();
    assert_eq!(chipsoc_admin::shares::update(&a, &item).await.unwrap(), 2, "to Us and Other, who had it");
    assert_eq!(a.send_to(&b).await, 1);
    let tid = threads[0]["id"].as_str().unwrap().to_owned();
    let still = b.messages_in(&tid).pop().unwrap()["content"].as_str().unwrap().to_owned();
    assert!(!still.contains("Now with pictures"), "an edit waits for review: {still}");
    let edit = chipsoc_admin::pending(&b).await.unwrap().pop().expect("the edit, for review");
    assert!(edit.answers.iter().any(|x| x.question == "Text before"), "{edit:?}");
    let d = chipsoc_admin::decide(&b, edit.id, true, &b.admin()).await.unwrap();
    assert!(d.accepted && d.outcome.contains("Updated:"), "{d:?}");
    let now_says = b.messages_in(&tid).pop().unwrap()["content"].as_str().unwrap().to_owned();
    assert!(now_says.contains("Now with pictures"), "{now_says}");
    assert!(chipsoc_admin::archive_resource(&a, post.id, &a.admin()).await.unwrap());
    assert_eq!(a.send_to(&b).await, 1);
    let thread = b.threads_in("partner-resources").pop().unwrap();
    assert_eq!((thread["archived"].clone(), thread["locked"].clone()), (Value::Bool(true), Value::Bool(true)));
    assert!(b.messages_in(&tid).pop().unwrap()["content"].as_str().unwrap().contains("Them took this down"));
    assert_eq!(chipsoc_admin::pending(&b).await.unwrap().len(), 0);
}

use chipsoc_admin::shares::ShareKind as ShareKindAlias;

#[tokio::test]
async fn an_announcement_goes_only_to_the_partners_picked() {
    let (a, b) =
        pair("\"announcements\"", "accept = { announcements = { to = \"partner-news\", review = false } }").await;
    let news = a.channel_id("news");
    let msg = a
        .rest
        .call(
            Method::Post,
            &format!("channels/{news}/messages"),
            Some(serde_json::json!({ "content": "**Hackathon!**\nSign up <@&5> [here](https://x.example)" })),
        )
        .await
        .unwrap();
    let pick = chipsoc_admin::shares::Pick::Announcement {
        channel: news.parse().unwrap(),
        message: chipsoc_discord::wire::id(&msg["id"]).unwrap(),
    };
    let to = chipsoc_admin::shares::share_now(&a, pick, &["Us".into()], &a.admin()).await.unwrap();
    assert_eq!(to, ["Us"]);
    crate::fedshare::flush(&a).await;
    let sent: Vec<String> = a.wire.lock().unwrap().iter().map(|(p, _)| p.clone()).collect();
    assert_eq!(sent, ["us"], "not to Other");
    let content = a.wire.lock().unwrap().pop().unwrap().1;
    b.post(content).await;
    let posted = b.messages_in("partner-news");
    assert_eq!(posted.len(), 1, "review is off for announcements from them");
    let text = posted[0]["content"].as_str().unwrap();
    assert!(text.starts_with("📣 **Hackathon\\!**") || text.starts_with("📣 **Hackathon!**"), "{text}");
    assert!(text.contains("\\<\\@&5\\>") && text.contains("\\[here\\]"), "no mentions or masked links: {text}");
    assert!(text.contains("From **Them** · posted by"), "{text}");
    assert!(b.logs().iter().any(|l| l.contains("Posted an announcement Them shared")));

    // Picking a partner we don't send announcements to is refused.
    let err = chipsoc_admin::shares::share_now(&a, pick, &["nobody".into()], &a.admin()).await.unwrap_err();
    assert!(err.to_string().contains("No partner is called"), "{err}");
}

#[tokio::test]
async fn a_shared_event_becomes_an_event_here_and_is_cancelled_with_theirs() {
    let (a, b) = pair("\"events\"", "accept = { events = { review = false } }").await;
    let start = chrono::DateTime::from_timestamp(now() + 7200, 0).unwrap().to_rfc3339();
    let end = chrono::DateTime::from_timestamp(now() + 10800, 0).unwrap().to_rfc3339();
    let made = a
        .rest
        .call(
            Method::Post,
            "guilds/1000/scheduled-events",
            Some(serde_json::json!({ "name": "Social", "scheduled_start_time": start, "scheduled_end_time": end,
                "entity_type": 3, "entity_metadata": { "location": "Room 2.15" }, "description": "Pizza" })),
        )
        .await
        .unwrap();
    let id = chipsoc_discord::wire::id(&made["id"]).unwrap();
    let to =
        chipsoc_admin::shares::share_now(&a, chipsoc_admin::shares::Pick::Event(id), &[], &a.admin()).await.unwrap();
    assert_eq!(to, ["Us", "Other"], "none picked: everyone we send events to");
    assert_eq!(a.send_to(&b).await, 1);
    let events = b.rest.discord.state.lock().unwrap().events.clone();
    assert_eq!(events.len(), 1);
    assert_eq!((events[0]["name"].as_str(), events[0]["entity_type"].as_u64()), (Some("Social (Them)"), Some(3)));
    assert_eq!(events[0]["entity_metadata"]["location"], "Room 2.15");
    assert_eq!(events[0]["scheduled_start_time"], start);
    assert!(events[0]["description"].as_str().unwrap().contains("From **Them**"));

    assert_eq!(chipsoc_admin::shares::remove(&a, ShareKindAlias::Event, &id.to_string()).await.unwrap(), 2);
    assert_eq!(a.send_to(&b).await, 1);
    assert_eq!(b.rest.discord.state.lock().unwrap().events[0]["status"], 4, "cancelled, not deleted");
}

#[tokio::test]
async fn a_kind_this_version_doesnt_know_is_ignored_quietly() {
    let e = env().await;
    let fields = serde_json::json!({ "from": "them", "to": "us", "seq": now_ms(), "ts": now(), "nonce": "n1",
        "kind": "poll", "body": { "question": "Pizza?" } });
    // Sealed as a newer partner would, over a kind we don't know.
    let msg = e.post(proto::seal_fields(&e.key, fields)).await;
    assert_eq!(db(&e).inbox_state(msg).await.unwrap(), Some(InboxState::Done));
    assert!(e.logs().is_empty(), "not a rejected message: {:?}", e.logs());

    // A share of a kind (or for a channel) we don't take: left out, said once.
    let item = chipsoc_admin::shares::Share::new(ShareKindAlias::Resource, "1", "A link");
    e.send(now_ms(), Body::Share(item.clone())).await;
    e.send(now_ms() + 1, Body::Share(chipsoc_admin::shares::Share { origin: "2".into(), ..item })).await;
    let said: Vec<String> = e.logs().into_iter().filter(|l| l.contains("we don't take their resources")).collect();
    assert_eq!(said.len(), 1, "{:?}", e.logs());
    assert!(chipsoc_admin::pending(&e).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_take_down_while_waiting_for_review_takes_it_off_the_queue() {
    let (a, b) = pair("\"questions\"", "accept = { questions = \"partner-questions\" }").await;
    let item = chipsoc_admin::shares::question(Snowflake(1000), Snowflake(55), "Which iron?", "Help me pick", "Bo");
    chipsoc_admin::shares::send(&a, &item, None, None).await.unwrap();
    assert_eq!(a.send_to(&b).await, 1);
    assert_eq!(a.send_to(&b).await, 0, "sent once");
    let id = chipsoc_admin::pending(&b).await.unwrap()[0].id;
    // An edit while it waits: accepting posts the newer text.
    let mut edited = item.clone();
    edited.text = "Help me pick a soldering iron".into();
    chipsoc_admin::shares::update(&a, &edited).await.unwrap();
    assert_eq!(a.send_to(&b).await, 1);
    let answers = &chipsoc_admin::pending(&b).await.unwrap()[0].answers;
    assert!(answers.iter().any(|x| x.answer == "Help me pick a soldering iron"), "{answers:?}");
    chipsoc_admin::shares::remove(&a, ShareKindAlias::Question, "55").await.unwrap();
    assert_eq!(a.send_to(&b).await, 1);
    assert!(chipsoc_admin::pending(&b).await.unwrap().is_empty(), "withdrawn");
    let card = b.messages_in("review").pop().unwrap();
    assert!(card["content"].as_str().unwrap().contains("Them took this down"), "{card}");
    assert!(chipsoc_admin::decide(&b, id, true, &b.admin()).await.is_err(), "can't be accepted now");
}

#[tokio::test]
async fn an_accepted_question_links_back_and_says_replies_dont_sync() {
    let (a, b) = pair("\"questions\"", "accept = { questions = \"partner-questions\" }").await;
    let item = chipsoc_admin::shares::question(Snowflake(1000), Snowflake(56), "Which iron?", "Help", "Bo");
    chipsoc_admin::shares::send(&a, &item, None, None).await.unwrap();
    a.send_to(&b).await;
    let id = chipsoc_admin::pending(&b).await.unwrap()[0].id;
    chipsoc_admin::decide(&b, id, true, &b.admin()).await.unwrap();
    let t = b.threads_in("partner-questions").pop().unwrap();
    let text = t["message"]["content"].as_str().unwrap();
    assert!(
        text.contains("From **Them** · asked by Bo. Answer there: <https://discord.com/channels/1000/56>"),
        "{text}"
    );
    assert!(text.contains("Replies here don't reach them."), "{text}");
}

// --- Regression tests from the round-4 red-team review (F4-*, #7).

use chipsoc_admin::shares::{self as shares_api, Share};

impl TestEnv {
    /// The Accept button's custom id on a card.
    fn accept_id(card: &Value) -> String {
        card["components"][0]["components"][0]["custom_id"].as_str().unwrap().to_owned()
    }

    /// A message in the review channel by id.
    fn card(&self, id: &str) -> Value {
        self.messages_in("review").into_iter().find(|m| m["id"] == id).expect("the card")
    }

    fn people_log(&self) -> Vec<String> {
        self.messages_in("people").iter().map(|m| m["content"].as_str().unwrap_or_default().to_owned()).collect()
    }

    fn mirror_count(&self) -> usize {
        self.rest.discord.state.lock().unwrap().channels.len()
    }
}

fn resource_item(origin: &str, title: &str, url: &str) -> Share {
    let mut item = Share::new(ShareKindAlias::Resource, origin, title);
    item.url = Some(url.into());
    item.by = "Ada".into();
    item
}

#[tokio::test]
async fn a_partner_update_while_waiting_redraws_the_card_and_voids_its_accept() {
    // F4-M1 / #7: the PoC swapped the item under a stale card, and Accept posted the swap.
    let (a, b) = pair("\"resources\"", "accept = { resources = \"partner-resources\" }").await;
    shares_api::send(&a, &resource_item("1", "Soldering guide", "https://example.com/solder"), None, None)
        .await
        .unwrap();
    a.send_to(&b).await;
    let first = b.messages_in("review").pop().unwrap();
    let old_accept = TestEnv::accept_id(&first);
    let waiting = chipsoc_admin::pending(&b).await.unwrap().pop().unwrap();
    assert_eq!(old_accept, chipsoc_admin::rounds::review_seen(waiting.id, &waiting.version));

    // The partner swaps it for something else while it waits.
    let swapped = resource_item("1", "Discord Nitro for members", "https://nitro.example/claim");
    shares_api::update(&a, &swapped).await.unwrap();
    a.send_to(&b).await;
    let card = b.card(first["id"].as_str().unwrap());
    let text = card["content"].as_str().unwrap();
    assert!(text.contains("Discord Nitro for members") && text.contains("<https://nitro.example/claim>"), "{text}");
    assert!(text.contains("changed this while it waited") && !text.contains("Soldering"), "redrawn: {text}");
    assert_ne!(TestEnv::accept_id(&card), old_accept, "the old Accept is void");

    // Accept from what the old card showed is refused; nothing is posted.
    let old = chipsoc_admin::pending(&b).await.unwrap().pop().unwrap();
    let err = chipsoc_admin::decide_seen(&b, old.id, true, None, Some(&waiting.version), &b.admin()).await.unwrap_err();
    assert!(err.to_string().contains("changed this since — review again"), "{err}");
    assert!(b.threads_in("partner-resources").is_empty());
    assert_eq!(chipsoc_admin::pending(&b).await.unwrap().len(), 1, "still waiting");

    // Accepting what's shown now posts exactly that.
    let d = chipsoc_admin::decide_seen(&b, old.id, true, None, Some(&old.version), &b.admin()).await.unwrap();
    assert!(d.accepted, "{d:?}");
    let posted = b.threads_in("partner-resources").pop().unwrap();
    assert_eq!(posted["name"], "Discord Nitro for members");
}

#[tokio::test]
async fn an_update_between_pressing_accept_and_posting_isnt_posted() {
    // F4-M1: the change lands after Accept is claimed but before it's posted.
    let (a, b) = pair("\"resources\"", "accept = { resources = \"partner-resources\" }").await;
    shares_api::send(&a, &resource_item("1", "Guide", "https://example.com/g"), None, None).await.unwrap();
    a.send_to(&b).await;
    let p = chipsoc_admin::pending(&b).await.unwrap().pop().unwrap();
    let app = chipsoc_admin::claim_seen(&b, p.id, true, Some(&p.version), &b.admin()).await.unwrap();
    shares_api::update(&a, &resource_item("1", "Swapped", "https://evil.example/x")).await.unwrap();
    a.send_to(&b).await;
    let d = chipsoc_admin::settle(&b, &app, true, &b.admin()).await;
    assert!(d.pending && d.outcome.contains("changed this since"), "{d:?}");
    assert!(b.threads_in("partner-resources").is_empty(), "nothing posted");
    let again = chipsoc_admin::pending(&b).await.unwrap().pop().expect("back in the queue");
    assert!(again.answers.iter().any(|x| x.answer == "Swapped"), "showing the new version: {again:?}");
    let card = b.messages_in("review").pop().unwrap();
    assert!(card["content"].as_str().unwrap().contains("Swapped"), "card redrawn: {card}");
    assert_eq!(TestEnv::accept_id(&card), chipsoc_admin::rounds::review_seen(again.id, &again.version));
}

#[tokio::test]
async fn an_accepted_items_later_edits_wait_for_review() {
    // F4-M2: the PoC edited an accepted item into a phishing link, applied with no review.
    let (a, b) = pair("\"resources\"", "accept = { resources = \"partner-resources\" }").await;
    shares_api::send(&a, &resource_item("1", "Guide", "https://example.com/g"), None, None).await.unwrap();
    a.send_to(&b).await;
    let id = chipsoc_admin::pending(&b).await.unwrap()[0].id;
    chipsoc_admin::decide(&b, id, true, &b.admin()).await.unwrap();
    let tid = b.threads_in("partner-resources")[0]["id"].as_str().unwrap().to_owned();
    let post = || b.messages_in(&tid).pop().unwrap()["content"].as_str().unwrap().to_owned();
    let posted = post();

    shares_api::update(&a, &resource_item("1", "Guide", "https://phish.example/login")).await.unwrap();
    a.send_to(&b).await;
    assert_eq!(post(), posted, "the post stays as accepted");
    let edit = chipsoc_admin::pending(&b).await.unwrap().pop().expect("the edit waits for review");
    assert!(edit.answers.iter().any(|x| x.question == "Link before" && x.answer == "https://example.com/g"));
    let card = b.messages_in("review").pop().unwrap();
    let text = card["content"].as_str().unwrap();
    assert!(text.contains("changed resource") && text.contains("**Link** was: <https://example.com/g>"), "{text}");
    assert!(text.contains(&format!("https://discord.com/channels/1000/{tid}")), "says which post: {text}");

    // Rejected: the post is left alone, and their next edit is reviewed again.
    chipsoc_admin::decide(&b, edit.id, false, &b.admin()).await.unwrap();
    assert_eq!(post(), posted);
    shares_api::update(&a, &resource_item("1", "Guide v2", "https://example.com/g2")).await.unwrap();
    a.send_to(&b).await;
    let next = chipsoc_admin::pending(&b).await.unwrap().pop().expect("reviewed again");
    // Taken down while the edit waits: archived, and the edit is off the queue.
    shares_api::remove(&a, ShareKindAlias::Resource, "1").await.unwrap();
    a.send_to(&b).await;
    assert!(chipsoc_admin::pending(&b).await.unwrap().is_empty());
    assert!(chipsoc_admin::decide(&b, next.id, true, &b.admin()).await.is_err());
    assert!(post().contains("Them took this down"));
}

#[tokio::test]
async fn edits_apply_straight_away_when_the_partner_is_trusted_to() {
    let accept = "accept = { resources = { to = \"partner-resources\", edits = \"apply\" } }";
    let (a, b) = pair("\"resources\"", accept).await;
    shares_api::send(&a, &resource_item("1", "Guide", "https://example.com/g"), None, None).await.unwrap();
    a.send_to(&b).await;
    let id = chipsoc_admin::pending(&b).await.unwrap()[0].id;
    chipsoc_admin::decide(&b, id, true, &b.admin()).await.unwrap();
    shares_api::update(&a, &resource_item("1", "Guide", "https://example.com/new")).await.unwrap();
    a.send_to(&b).await;
    assert!(chipsoc_admin::pending(&b).await.unwrap().is_empty());
    let tid = b.threads_in("partner-resources")[0]["id"].as_str().unwrap().to_owned();
    let text = b.messages_in(&tid).pop().unwrap()["content"].as_str().unwrap().to_owned();
    assert!(text.contains("<https://example.com/new>"), "{text}");
}

#[tokio::test]
async fn only_whoever_shared_it_or_an_admin_changes_it_at_partners() {
    // F4-M2, our side: a member's edit of an admin-shared message isn't sent under our name.
    let (a, b) =
        pair("\"announcements\"", "accept = { announcements = { to = \"partner-news\", review = false } }").await;
    let news = a.channel_id("news");
    let msg = a
        .rest
        .call(Method::Post, &format!("channels/{news}/messages"), Some(serde_json::json!({ "content": "Hackathon!" })))
        .await
        .unwrap();
    let message = chipsoc_discord::wire::id(&msg["id"]).unwrap();
    let pick = chipsoc_admin::shares::Pick::Announcement { channel: news.parse().unwrap(), message };
    chipsoc_admin::shares::share_now(&a, pick, &["Us".into()], &a.admin()).await.unwrap();
    assert_eq!(a.send_to(&b).await, 1);
    // Member 101 (not an admin, not who shared it) edits it: nothing goes out.
    crate::fedshare::edited(&a, message.0, Snowflake(101), "Free Nitro: https://phish.example").await;
    assert_eq!(a.send_to(&b).await, 0, "not followed");
    // The admin who shared it edits it: it goes out.
    crate::fedshare::edited(&a, message.0, a.admin().id.unwrap(), "Hackathon! Room 2").await;
    assert_eq!(a.send_to(&b).await, 1);
    let posted = b.messages_in("partner-news").pop().unwrap();
    assert!(posted["content"].as_str().unwrap().contains("Room 2"), "{posted}");
    // Review 4 #17: the same text again (Discord adding a link's preview) isn't sent again.
    crate::fedshare::edited(&a, message.0, a.admin().id.unwrap(), "Hackathon! Room 2").await;
    assert_eq!(a.send_to(&b).await, 0, "nothing changed");
    // Review 4 #22: a new first line is its new title.
    crate::fedshare::edited(&a, message.0, a.admin().id.unwrap(), "**Hackathon moved!**\nRoom 3").await;
    assert_eq!(a.send_to(&b).await, 1);
    let origin = message.to_string();
    let shared = chipsoc_admin::shares::shared(&a, chipsoc_admin::shares::ShareKind::Announcement, &origin).await;
    assert_eq!(shared.unwrap().unwrap().title, "Hackathon moved!");

    // A question: its asker's own edit is followed (the partner reviews it).
    let q = chipsoc_admin::shares::question(Snowflake(1000), Snowflake(77), "Which iron?", "Help", "Bo");
    let (a, b) = pair("\"questions\"", "accept = { questions = \"partner-questions\" }").await;
    chipsoc_admin::shares::send(&a, &q, None, Some(Snowflake(102))).await.unwrap();
    a.send_to(&b).await;
    crate::fedshare::edited(&a, 77, Snowflake(103), "spam").await;
    assert_eq!(a.send_to(&b).await, 0, "someone else's");
    crate::fedshare::edited(&a, 77, Snowflake(102), "Help me pick one").await;
    assert_eq!(a.send_to(&b).await, 1, "the asker's");
}

/// Review 4 #15: a partner's post deleted here isn't updated (or tried
/// again and again) when they edit it; it counts as gone.
#[tokio::test]
async fn a_partners_post_deleted_here_takes_no_more_updates() {
    let (a, b) =
        pair("\"announcements\"", "accept = { announcements = { to = \"partner-news\", review = false } }").await;
    let news = a.channel_id("news");
    let path = format!("channels/{news}/messages");
    let msg = a.rest.call(Method::Post, &path, Some(serde_json::json!({ "content": "Hackathon!" }))).await.unwrap();
    let message = chipsoc_discord::wire::id(&msg["id"]).unwrap();
    let pick = chipsoc_admin::shares::Pick::Announcement { channel: news.parse().unwrap(), message };
    chipsoc_admin::shares::share_now(&a, pick, &["Us".into()], &a.admin()).await.unwrap();
    assert_eq!(a.send_to(&b).await, 1);
    let posted = b.messages_in("partner-news").pop().unwrap();
    let path = format!("channels/{}/messages/{}", b.channel_id("partner-news"), posted["id"].as_str().unwrap());
    b.rest.call(Method::Delete, &path, None).await.unwrap();

    crate::fedshare::edited(&a, message.0, a.admin().id.unwrap(), "Hackathon! Room 2").await;
    assert_eq!(a.send_to(&b).await, 1);
    let m = b.store().fed(b.gkey()).mirror("them", "announcement", &message.to_string()).await.unwrap().unwrap();
    assert_eq!(m.state, chipsoc_store::MirrorState::Removed);
    assert!(b.messages_in("partner-news").is_empty(), "not posted again");
}

/// Review 5 M4: a share too big to send even cut down is given up on at once
/// (and said), not counted against the partner, so what's queued after it goes.
#[tokio::test]
async fn a_share_too_big_to_send_doesnt_hold_up_the_rest() {
    let (a, b) = pair("\"questions\"", "accept = { questions = \"partner-questions\" }").await;
    let huge =
        chipsoc_admin::shares::question(Snowflake(1000), Snowflake(77), "Which iron?", "Help", &"x".repeat(2500));
    chipsoc_admin::shares::send(&a, &huge, None, Some(Snowflake(102))).await.unwrap();
    let fine = chipsoc_admin::shares::question(Snowflake(1000), Snowflake(78), "Which solder?", "Help", "Bo");
    chipsoc_admin::shares::send(&a, &fine, None, Some(Snowflake(102))).await.unwrap();
    assert_eq!(a.send_to(&b).await, 1, "the one that fits went");
    assert!(a.logs().iter().any(|l| l.contains("too big to send even cut down")), "{:?}", a.logs());
    assert!(a.store().fed(a.gkey()).out_pending().await.unwrap().is_empty(), "neither is left waiting");
    assert!(!crate::fedshare::backing_off(&a, "us"), "not the partner's fault");
}

#[tokio::test]
async fn help_forum_spam_cant_fill_the_partners_queue_or_shut_out_announcements() {
    // F4-M3: the PoC's 20 help posts filled the partner's queue and blocked our announcement.
    let accept = "accept = { questions = \"partner-questions\", announcements = \"partner-news\" }";
    let (a, b) = pair("\"questions\", \"announcements\"", accept).await;
    // Outbound: one member's posts share at most a few an hour, everyone's at most a few more.
    for n in 0..10 {
        let q = chipsoc_admin::shares::question(Snowflake(1000), Snowflake(500 + n), "Help", "?", "Spammer");
        chipsoc_admin::shares::send_quietly(&a, &q, Some(Snowflake(101))).await;
    }
    let fed = a.store.fed("1000");
    let since = now() - 10;
    assert_eq!(fed.shared_since("question", None, since).await.unwrap(), shares_api::MAX_PER_MEMBER);
    for n in 0..40u64 {
        let q = chipsoc_admin::shares::question(Snowflake(1000), Snowflake(600 + n), "Help", "?", "Many");
        chipsoc_admin::shares::send_quietly(&a, &q, Some(Snowflake(1 + n))).await;
    }
    assert_eq!(fed.shared_since("question", None, since).await.unwrap(), shares_api::MAX_AUTO_PER_HOUR);

    // Announcements go first, and the questions can't take their places in the queue there.
    let news = a.channel_id("news");
    let msg = a
        .rest
        .call(
            Method::Post,
            &format!("channels/{news}/messages"),
            Some(serde_json::json!({ "content": "AGM on Friday" })),
        )
        .await
        .unwrap();
    let pick = chipsoc_admin::shares::Pick::Announcement {
        channel: news.parse().unwrap(),
        message: chipsoc_discord::wire::id(&msg["id"]).unwrap(),
    };
    chipsoc_admin::shares::share_now(&a, pick, &["Us".into()], &a.admin()).await.unwrap();
    crate::fedshare::flush(&a).await;
    let wire: Vec<(String, String)> = std::mem::take(&mut *a.wire.lock().unwrap());
    let ours: Vec<String> = wire.into_iter().filter(|(p, _)| p == "us").map(|(_, c)| c).collect();
    let first = proto::verify(&ours[0], &peer_keys(&b.cfg().federation.peers), "us", now()).unwrap();
    assert!(matches!(&first.envelope.body, Body::Share(s) if s.what == ShareKindAlias::Announcement), "first");
    for c in ours {
        b.post(c).await;
    }
    let waiting = chipsoc_admin::pending(&b).await.unwrap();
    let questions = waiting.iter().filter(|p| p.kind == "partner-question").count();
    assert_eq!(questions as i64, shares_api::MAX_IN_REVIEW, "questions capped by their own slots");
    assert!(waiting.iter().any(|p| p.kind == "partner-announcement"), "the announcement still got in");
    assert!(b.logs().iter().any(|l| l.contains("questions waiting for review already")), "{:?}", b.logs());
}

#[tokio::test]
async fn a_partner_event_keeps_its_attribution_and_its_location_is_checked() {
    // F4-L1: a 100-character title cut " (Them)" off; the location skipped safe_link.
    let (a, b) = pair("\"events\"", "accept = { events = true }").await;
    let mut e = Share::new(ShareKindAlias::Event, "9", &"A".repeat(100));
    e.start = Some(now() + 7200);
    e.location = "https://github.com@evil.example/rsvp".into();
    e.link = Some("https://discord.com/events/1000/9".into());
    shares_api::send(&a, &e, None, None).await.unwrap();
    a.send_to(&b).await;
    let card = b.messages_in("review").pop().unwrap();
    let text = card["content"].as_str().unwrap();
    assert!(text.contains("**Where** (a link that isn't plain https"), "shown on the card: {text}");
    let id = chipsoc_admin::pending(&b).await.unwrap()[0].id;
    chipsoc_admin::decide(&b, id, true, &b.admin()).await.unwrap();
    let ev = b.rest.discord.state.lock().unwrap().events[0].clone();
    let name = ev["name"].as_str().unwrap();
    assert!(name.ends_with(" (Them)") && name.chars().count() <= 100, "{name}");
    let at = ev["entity_metadata"]["location"].as_str().unwrap();
    assert_eq!(at, "https://discord.com/events/1000/9", "the unsafe link isn't used");

    // A place in words is kept, and shown.
    let mut e2 = Share::new(ShareKindAlias::Event, "10", "Social");
    e2.start = Some(now() + 7200);
    e2.location = "Room 2.15".into();
    shares_api::send(&a, &e2, None, None).await.unwrap();
    a.send_to(&b).await;
    let text = b.messages_in("review").pop().unwrap()["content"].as_str().unwrap().to_owned();
    assert!(text.contains("**Where** Room 2.15"), "{text}");
}

#[tokio::test]
async fn partner_titles_are_escaped_in_the_people_log_and_the_queue() {
    // F4-L2: a title faked log lines and showed masked links to admins.
    let (a, b) = pair("\"resources\"", "accept = { resources = \"partner-resources\" }").await;
    let title = "x**.\n📝 <@1> rejected [verify](https://evil.example)";
    shares_api::send(&a, &resource_item("1", title, "https://example.com/g"), None, None).await.unwrap();
    a.send_to(&b).await;
    let p = chipsoc_admin::pending(&b).await.unwrap().pop().unwrap();
    assert!(!p.what.contains('\n'), "{}", p.what);
    chipsoc_admin::decide(&b, p.id, false, &b.admin()).await.unwrap();
    let log = b.people_log().pop().expect("a people-log line");
    assert!(!log.contains('\n') && log.contains("x\\*\\*.") && log.contains("\\[verify\\]"), "{log}");
    assert!(log.contains("**Them**'s submission"), "{log}");
    assert_eq!(chipsoc_admin::submitted_by(&b.cfg(), &p.user), "**Them**", "not <@partner:them>");
}

#[tokio::test]
async fn failing_items_are_retried_without_spending_the_hour_again() {
    // F4-L3: every retry re-spent the hourly budget; an untitled item failed every time.
    let accept = "accept = { resources = { to = \"partner-resources\", review = false } }";
    let (a, b) = pair("\"resources\"", accept).await;
    let forum = b.channel_id("partner-resources");
    b.rest.discord.fail(&format!("POST channels/{forum}/threads"), 500, false);
    shares_api::send(&a, &resource_item("1", "Guide", "https://example.com/g"), None, None).await.unwrap();
    a.send_to(&b).await;
    assert!(b.threads_in("partner-resources").is_empty(), "failed");
    let key = ("them".to_owned(), "resources");
    assert_eq!(b.state.inbox.lock().await.shared[&key].used, 1);
    // Backing off: not tried again at once.
    replay(&b, true).await;
    assert!(b.threads_in("partner-resources").is_empty(), "waits before trying again");
    b.state.inbox.lock().await.retry_at.clear();
    replay(&b, true).await;
    assert_eq!(b.threads_in("partner-resources").len(), 1, "tried again");
    assert_eq!(b.state.inbox.lock().await.shared[&key].used, 1, "not counted twice");
    assert_eq!(receive::retry_after(1), 300);
    assert_eq!(receive::retry_after(12), 3600);

    // No title: left out at once, without counting.
    let msg = {
        shares_api::send(&a, &resource_item("2", " \u{200B} ", "https://example.com/g"), None, None).await.unwrap();
        crate::fedshare::flush(&a).await;
        let (_, c) = a.wire.lock().unwrap().pop().unwrap();
        a.wire.lock().unwrap().clear();
        b.post(c).await
    };
    assert_eq!(db(&b).inbox_state(msg).await.unwrap(), Some(InboxState::Done));
    assert_eq!(b.state.inbox.lock().await.shared[&key].used, 1);
}

#[tokio::test]
async fn removing_a_partner_archives_what_it_shared_and_adding_it_back_starts_afresh() {
    // F4-L4: a removed partner's posts stayed up, and editable if the same id came back.
    let (a, b) = pair("\"resources\"", "accept = { resources = \"partner-resources\" }").await;
    shares_api::send(&a, &resource_item("1", "Posted", "https://example.com/1"), None, None).await.unwrap();
    shares_api::send(&a, &resource_item("2", "Waiting", "https://example.com/2"), None, None).await.unwrap();
    a.send_to(&b).await;
    let first = chipsoc_admin::pending(&b).await.unwrap()[0].id;
    chipsoc_admin::decide(&b, first, true, &b.admin()).await.unwrap();
    let channels = b.mirror_count();
    let before = b.cfg();
    b.edit_cfg(|c| c.federation.peers.clear());
    check_peers(&b).await;
    let thread = b.threads_in("partner-resources").pop().unwrap();
    assert_eq!((thread["archived"].clone(), thread["locked"].clone()), (Value::Bool(true), Value::Bool(true)));
    assert_eq!(b.mirror_count(), channels, "nothing deleted");
    assert!(chipsoc_admin::pending(&b).await.unwrap().is_empty(), "off the queue");
    assert!(b.logs().iter().any(|l| l.contains("archived the 2 things they'd shared")), "{:?}", b.logs());
    assert_eq!(db(&b).mirror("them", "resource", "1").await.unwrap(), None, "forgotten");
    // Back again: the same item is new, waiting for review, not an edit of the old post.
    b.edit_cfg(|c| *c = (*before).clone());
    shares_api::update(&a, &resource_item("1", "Posted again", "https://example.com/1")).await.unwrap();
    a.send_to(&b).await;
    let waiting = chipsoc_admin::pending(&b).await.unwrap();
    assert_eq!(waiting.len(), 1);
    assert!(!waiting[0].answers.iter().any(|x| x.question == "Changes"), "{waiting:?}");
}

#[tokio::test]
async fn only_what_every_member_can_see_is_shared_and_only_by_someone_who_sees_it() {
    // F4-I1: an admin could share a private channel's messages.
    let (a, _b) = pair("\"announcements\"", "").await;
    a.edit_cfg(|c| {
        c.federation.peers[0].accept.announcements = Some(chipsoc_config::Landing::To("partner-news".into()))
    });
    let say = |key: &str| {
        let ch = a.channel_id(key);
        let rest = a.rest.clone();
        async move {
            let m = rest
                .call(Method::Post, &format!("channels/{ch}/messages"), Some(serde_json::json!({ "content": "Hi" })))
                .await
                .unwrap();
            chipsoc_admin::shares::Pick::Announcement {
                channel: ch.parse().unwrap(),
                message: chipsoc_discord::wire::id(&m["id"]).unwrap(),
            }
        }
    };
    let private = say("committee").await;
    let err = chipsoc_admin::shares::share_now(&a, private, &[], &a.admin()).await.unwrap_err();
    assert!(err.to_string().contains("isn't open to all members"), "{err}");
    let theirs = say("partner-news").await;
    let err = chipsoc_admin::shares::share_now(&a, theirs, &[], &a.admin()).await.unwrap_err();
    assert!(err.to_string().contains("That's where partners' posts land"), "{err}");
    // A channel open to members, but not to the admin sharing it.
    let news = say("news").await;
    let admin = a.admin().id.unwrap();
    let deny = (1u64 << chipsoc_model::perms::VIEW_CHANNEL).to_string();
    a.rest.discord.edit_channel("news", |c| {
        c["permission_overwrites"] =
            serde_json::json!([{ "id": admin.to_string(), "type": 1, "allow": "0", "deny": deny }]);
    });
    let err = chipsoc_admin::shares::share_now(&a, news, &[], &a.admin()).await.unwrap_err();
    assert!(err.to_string().contains("You can't see that channel"), "{err}");
    assert!(shares_api::can_view(&a, Snowflake(101), news_channel(&a)).await.unwrap(), "others still can");
}

fn news_channel(e: &TestEnv) -> Snowflake {
    e.channel_id("news").parse().unwrap()
}
