//! Drive the app with keys against a fake Discord and a temp config, and
//! check what's on screen.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chipsoc_client::remote::{Client, Login};
use chipsoc_client::{Backend, Local};
use chipsoc_discord::fake::FakeDiscord;
use chipsoc_web::testing;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use tokio::sync::mpsc;

use crate::Fake;
use crate::app::{App, Command, Tab};
use crate::changes::STALE;
use crate::edits::Target;
use crate::ui;
use crate::worker::{Msg, Worker};

fn write(dir: &Path, file: &str, body: &str) {
    let p = dir.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    write(p, "server.toml", "[guild]\nname = \"Test\"\n");
    write(
        p,
        "bot.toml",
        "name = \"Test\"\nadmin_role = \"admin\"\narchive = \"archive\"\n[projects]\nforum = \"projects\"\nareas = [\"cpu\", \"gpu\"]\n",
    );
    write(
        p,
        "roles.toml",
        "# roles, top first\n[[role]]\nkey = \"bot\"\nname = \"Bot\"\nmanaged = true\n\n[[role]]\nkey = \"admin\"\nname = \"Admin\" # the committee\ncolor = \"#e8944a\"\n\n[[role]]\nkey = \"treasurer\"\nname = \"Treasurer\"\n\n[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\npermissions = [\"VIEW_CHANNEL\", \"SEND_MESSAGES\"]\n",
    );
    write(
        p,
        "categories/10-main.toml",
        "[category]\nkey = \"main\"\nname = \"Main\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\ntopic = \"Chat\"\n\n[[channel]]\nkey = \"projects\"\ntype = \"forum\"\nname = \"projects\"\ntags = [\"active\", \"past\", \"proposed\", \"cpu\", \"gpu\"]\n",
    );
    write(
        p,
        "categories/20-staff.toml",
        "[category]\nkey = \"staff\"\nname = \"Staff\"\n[[category.overwrites]]\nrole = \"everyone\"\ndeny = [\"VIEW_CHANNEL\"]\n[[category.overwrites]]\nrole = \"admin\"\nallow = [\"VIEW_CHANNEL\"]\n\n[[channel]]\nkey = \"review\"\nname = \"review\"\n",
    );
    write(p, "categories/90-archive.toml", "[category]\nkey = \"archive\"\nname = \"Archive\"\n");
    write(
        p,
        "projects.toml",
        "# keep me\n[[project]]\nslug = \"golem\"\nname = \"Golem\"\nblurb = \"a CPU\"\nkind = \"cpu\"\nstatus = \"active\"\nchannels = [\"general\", \"voice\"]\n[project.application]\nopen = true\n",
    );
    d
}

struct Harness {
    dir: tempfile::TempDir,
    app: App,
    worker: Worker<Fake>,
    rx: mpsc::UnboundedReceiver<Msg>,
    edits: Vec<(PathBuf, usize)>,
    texts: Vec<(String, Target)>,
}

/// The bot's database beside the config, hidden so config edits leave it alone.
fn db_url(dir: &Path) -> String {
    format!("sqlite:{}?mode=rwc", dir.join(".bot.db").display())
}

impl Harness {
    async fn new() -> Self {
        Self::on(fixture()).await
    }

    async fn on(dir: tempfile::TempDir) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let fake: Fake = Arc::new(FakeDiscord::new());
        let backend = Backend::Local(Local::with_db(dir.path(), fake.clone(), Some(db_url(dir.path())), "tester"));
        let worker = Worker::with_backend(fake, dir.path(), tx, backend);
        let app = App::new(dir.path().to_owned(), false);
        let mut h = Self { dir, app, worker, rx, edits: Vec::new(), texts: Vec::new() };
        for job in h.app.startup() {
            h.run(Command::Discord(job)).await;
        }
        h
    }

    fn fake(&self) -> Fake {
        self.worker.rest.clone()
    }

    async fn store(&self) -> chipsoc_store::Store {
        chipsoc_store::Store::open(&db_url(self.dir.path())).await.unwrap()
    }

    async fn run(&mut self, cmd: Command) {
        match cmd {
            Command::Discord(job) => {
                self.worker.run(job).await;
                while let Ok(msg) = self.rx.try_recv() {
                    if let Some(Command::Discord(job)) = self.app.on_msg(msg) {
                        self.worker.run(job).await;
                    }
                }
            }
            Command::Edit(path, line) => self.edits.push((path, line)),
            Command::Text(text, target) => self.texts.push((text, target)),
        }
    }

    /// Press a key, and finish any job it starts.
    async fn key(&mut self, code: KeyCode) {
        if let Some(cmd) = self.app.on_key(KeyEvent::from(code)) {
            self.run(cmd).await;
        }
    }

    /// Ctrl and a letter.
    async fn ctrl(&mut self, c: char) {
        if let Some(cmd) = self.app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)) {
            self.run(cmd).await;
        }
    }

    async fn keys(&mut self, s: &str) {
        for c in s.chars() {
            self.key(KeyCode::Char(c)).await;
        }
    }

    /// As if $EDITOR turned the last text asked for into `with`.
    async fn edit_text(&mut self, with: impl FnOnce(&str) -> String) {
        let (text, target) = self.texts.pop().expect("an editor was asked for");
        if let Some(cmd) = self.app.text_edited(target, &text, Ok(with(&text))) {
            self.run(cmd).await;
        }
    }

    /// Move down until `text` is on screen (selected rows start with ▶).
    async fn select(&mut self, text: &str) {
        for _ in 0..30 {
            if self.screen().contains(text) {
                return;
            }
            self.key(KeyCode::Down).await;
        }
        panic!("never selected {text:?}:\n{}", self.screen());
    }

    fn screen(&self) -> String {
        self.screen_at(110, 32)
    }

    fn screen_at(&self, width: u16, height: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(|f| ui::draw(f, &self.app)).unwrap();
        let buf = t.backend().buffer();
        let width = buf.area.width as usize;
        buf.content.chunks(width).map(|row| row.iter().map(|c| c.symbol()).collect::<String>() + "\n").collect()
    }

    fn file(&self, name: &str) -> String {
        fs::read_to_string(self.dir.path().join(name)).unwrap()
    }
}

fn has_key(app: &App, what: &str) -> bool {
    app.bindings().iter().any(|b| b.what == what)
}

fn assert_shows(screen: &str, text: &str) {
    assert!(screen.contains(text), "expected {text:?} on screen:\n{screen}");
}

#[tokio::test]
async fn overview_puts_problems_first_and_reload_rechecks() {
    let d = fixture();
    let staff = d.path().join("categories/20-staff.toml");
    let good = fs::read_to_string(&staff).unwrap();
    fs::write(&staff, good.replace("role = \"admin\"", "role = \"ghost\"")).unwrap();
    let mut h = Harness::on(d).await;
    let s = h.screen();
    assert_shows(&s, "Test");
    assert_shows(&s, "2 problems to fix");
    assert_shows(&s, "unknown role `ghost`");
    assert_shows(&s, "Review queue none waiting");
    assert_shows(&s, "Nothing yet.");

    fs::write(&staff, good).unwrap();
    h.keys("r").await;
    assert_shows(&h.screen(), "No problems found");
}

#[tokio::test]
async fn broken_files_are_explained_not_fatal() {
    let d = fixture();
    write(d.path(), "roles.toml", "[[role]]\nkey = \"a\"\nnmae = \"typo\"\n");
    let mut h = Harness::on(d).await;
    assert_shows(&h.screen(), "roles.toml");
    h.keys("2").await;
    assert_shows(&h.screen(), "Your files have a problem");
}

#[tokio::test]
async fn channels_tab_shows_details_and_opens_definitions() {
    let mut h = Harness::new().await;
    h.keys("3").await;
    h.key(KeyCode::Down).await;
    let s = h.screen();
    assert_shows(&s, "▶   # general");
    assert_shows(&s, "Chat");
    assert_shows(&s, "Who sees it  everyone");
    h.keys("e").await;
    let (path, line) = h.edits.pop().unwrap();
    assert!(path.ends_with("categories/10-main.toml") && line == 6, "{path:?}:{line}");

    h.select("▶ ▾ Staff").await;
    assert_shows(&h.screen(), "only Admin");
    h.key(KeyCode::Down).await;
    assert_shows(&h.screen(), "only Admin");
}

#[tokio::test]
async fn after_the_editor_closes_files_are_rechecked() {
    let mut h = Harness::new().await;
    h.keys("4e").await;
    let (path, _) = h.edits.pop().unwrap();
    h.app.edited(Ok(()));
    assert_shows(&h.screen(), "Nothing changed.");

    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replace("name = \"Golem\"", "name = \"Golem II\"")).unwrap();
    h.app.edited(Ok(()));
    assert_shows(&h.screen(), "Saved. To send it: 0, then f (plan), then a (apply).");
    assert_shows(&h.screen(), "Golem II");

    h.app.edited(Err(anyhow::anyhow!("couldn't start vi")));
    assert_shows(&h.screen(), "The editor didn't run: couldn't start vi");
}

#[tokio::test]
async fn projects_change_status_and_applications_keeping_comments() {
    let mut h = Harness::new().await;
    h.keys("4").await;
    assert_shows(&h.screen(), "recruiting");
    h.keys("o").await;
    assert!(h.file("projects.toml").contains("open = false"));
    assert_shows(&h.screen(), "Project golem: applications off.");

    h.keys("s").await;
    assert_shows(&h.screen(), "What stage is Golem at?");
    h.key(KeyCode::Right).await;
    h.key(KeyCode::Enter).await;
    assert_shows(&h.screen(), "Make Golem past");
    assert!(h.file("projects.toml").contains("status = \"active\""), "nothing yet: it asks first");
    h.keys("y").await;
    let file = h.file("projects.toml");
    assert!(file.contains("status = \"past\"") && file.starts_with("# keep me"), "{file}");
    assert_shows(&h.screen(), "Project golem: status to \"past\".");
}

#[tokio::test]
async fn new_project_form_types_picks_and_saves() {
    let mut h = Harness::new().await;
    h.keys("4n").await;
    h.ctrl('s').await;
    assert_shows(&h.screen(), "Give the project a name.");

    h.keys("Hydraq").await;
    h.key(KeyCode::Backspace).await;
    assert!(!h.app.quit, "letters go into the form");
    h.key(KeyCode::Tab).await;
    h.keys("A GPU").await;
    h.key(KeyCode::Tab).await;
    assert_shows(&h.screen(), "‹ cpu ›");
    h.key(KeyCode::Right).await;
    // Enter goes on to the next field; on the last it saves.
    h.key(KeyCode::Enter).await;
    assert!(h.app.overlay.is_some(), "not saved before the last field");
    assert_shows(&h.screen(), "add project");
    h.key(KeyCode::Enter).await;

    let file = h.file("projects.toml");
    assert!(file.contains("slug = \"hydra\"") && file.contains("kind = \"gpu\""), "{file}");
    let s = h.screen();
    assert_shows(&s, "Added project Hydra.");
    assert_shows(&s, "Hydra — A GPU");

    h.keys("nHydra").await;
    h.ctrl('s').await;
    let s = h.screen();
    assert_shows(&s, "already exists");
    assert_shows(&s, "New project");
}

#[tokio::test]
async fn editing_a_role_writes_its_file_and_a_refused_edit_writes_nothing() {
    let mut h = Harness::new().await;
    h.keys("2").await;
    h.key(KeyCode::Down).await;
    assert_shows(&h.screen(), "▶ ● Admin");
    h.key(KeyCode::Enter).await;
    assert_shows(&h.screen(), "Edit role Admin");
    for _ in 0..5 {
        h.key(KeyCode::Backspace).await;
    }
    h.keys("Committee").await;
    h.key(KeyCode::Down).await;
    h.key(KeyCode::Down).await;
    h.keys(" ").await;
    h.ctrl('s').await;
    let roles = h.file("roles.toml");
    assert!(roles.contains("name = \"Committee\" # the committee") && roles.contains("hoist = true"), "{roles}");
    assert!(roles.starts_with("# roles, top first"));
    // One change, checked once.
    assert_shows(&h.screen(), "2 changes: Role admin: name to \"Committee\"; Role admin: hoist on.");

    let before = h.file("roles.toml");
    h.key(KeyCode::Enter).await;
    h.key(KeyCode::Down).await;
    h.keys("zz").await;
    h.ctrl('s').await;
    let s = h.screen();
    assert_shows(&s, "Edit role Committee");
    assert_shows(&s, "bad colour");
    assert_eq!(h.file("roles.toml"), before, "nothing written");
    h.key(KeyCode::Esc).await;

    h.keys("p").await;
    h.edit_text(|t| t.replace("# One permission", "KICK_MEMBERS\n# One permission")).await;
    assert!(h.file("roles.toml").contains("permissions = [\"KICK_MEMBERS\"]"), "{}", h.file("roles.toml"));

    h.keys("n").await;
    h.keys("Mods").await;
    h.key(KeyCode::Enter).await;
    h.key(KeyCode::Enter).await;
    assert!(h.file("roles.toml").contains("key = \"mods\""), "the key is made from the name");
}

#[tokio::test]
async fn archiving_a_channel_asks_first_and_posts_are_edited_as_text() {
    let mut h = Harness::new().await;
    h.keys("3").await;
    h.select("▶   # general").await;
    h.keys("x").await;
    assert_shows(&h.screen(), "Archive channel?");
    h.keys("n").await;
    assert!(h.file("categories/10-main.toml").contains("key = \"general\""), "no means no");
    h.keys("x").await;
    h.keys("y").await;
    assert!(!h.file("categories/10-main.toml").contains("key = \"general\""));
    assert_shows(&h.screen(), "Removed channel general.");

    h.select("▶   # review").await;
    h.keys("b").await;
    h.edit_text(|_| "Welcome to {#review}.\n".into()).await;
    assert_eq!(h.file("content/review.md"), "Welcome to {#review}.\n");
    assert!(h.file("categories/20-staff.toml").contains("post = \"content/review.md\""));

    h.keys("p").await;
    h.edit_text(|t| t.to_owned() + "admin deny SEND_MESSAGES\n").await;
    assert!(h.file("categories/20-staff.toml").contains("SEND_MESSAGES"), "{}", h.file("categories/20-staff.toml"));
    h.keys("p").await;
    h.edit_text(|t| t.to_owned() + "admin VIEW_CHANNEL\n").await;
    assert_shows(&h.screen(), "say allow or deny");
}

#[tokio::test]
async fn positions_are_added_edited_and_their_questions_written_as_text() {
    let mut h = Harness::new().await;
    h.keys("5").await;
    assert_shows(&h.screen(), "No positions yet");
    h.keys("n").await;
    // The admin role itself can't be a position.
    h.key(KeyCode::Right).await;
    h.ctrl('s').await;
    assert!(h.file("bot.toml").contains("role = \"treasurer\""), "{}", h.file("bot.toml"));
    assert_shows(&h.screen(), "▶ Treasurer  open");
    assert_shows(&h.screen(), "Admin        no");
    h.keys("o").await;
    assert!(h.file("bot.toml").contains("open = false"));
    h.keys("Q").await;
    h.edit_text(|_| "Why? [long] // a line or two\nCourse?\n".into()).await;
    let pos = chipsoc_config::Config::load(h.dir.path()).unwrap().bot.positions.remove(0);
    assert_eq!((pos.questions.len(), pos.questions[0].long), (2, true));
    h.keys("x").await;
    h.keys("y").await;
    assert!(!h.file("bot.toml").contains("[[position]]"));
}

#[tokio::test]
async fn the_queue_rejects_with_a_reason_and_keeps_the_history() {
    let mut h = Harness::new().await;
    h.keys("0fay").await;
    h.fake().join(42);
    let store = h.store().await;
    let guild = h.app.cfg.as_ref().unwrap().guild().unwrap().to_string();
    let answers = r#"[{"question":"Why?","answer":"To help"}]"#;
    let db = store.guild(&guild);
    db.add_application(chipsoc_store::AppKind::Project, "golem", "42", answers, false, 1).await.unwrap();

    h.keys("6").await;
    let s = h.screen();
    assert_shows(&s, "unknown member (id …42) — Golem");
    assert_shows(&s, "To help");
    // j and k move; they never decide anything.
    h.keys("jk").await;
    assert!(h.app.overlay.is_none() && h.fake().dms_to(42).is_empty());
    h.keys("x").await;
    let s = h.screen();
    assert_shows(&s, "Reject unknown member (id …42)'s application to join Golem");
    assert_shows(&s, "Enter  reject");
    h.keys("Full this term").await;
    h.key(KeyCode::Enter).await;
    assert_shows(&h.screen(), "Rejected; they've been told.");
    assert!(h.fake().dms_to(42)[0].contains("Full this term"));
    assert_shows(&h.screen(), "The review queue is off");

    h.keys("h").await;
    let s = h.screen();
    assert_shows(&s, "✗ unknown member (id …42) — Golem");
    assert_shows(&s, "Reason       Full this term");
    assert!(!s.contains("make lead"), "only accepted project applicants");
    h.key(KeyCode::Esc).await;
    assert!(!h.app.show_history, "Esc goes back to the queue");
}

#[tokio::test]
async fn feeds_pause_and_stopping_needs_a_yes() {
    let mut h = Harness::new().await;
    h.keys("0fay").await;
    let store = h.store().await;
    let guild = h.app.cfg.as_ref().unwrap().guild().unwrap().to_string();
    store.guild(&guild).add_feed("https://example.org/feed", "general", "9", 1).await.unwrap();
    h.keys("8").await;
    assert_shows(&h.screen(), "https://example.org/feed");
    h.keys("p").await;
    assert_shows(&h.screen(), "paused");
    assert!(store.guild(&guild).feeds().await.unwrap()[0].paused);
    h.keys("x").await;
    assert_shows(&h.screen(), "Remove feed?");
    h.key(KeyCode::Esc).await;
    assert_eq!(store.guild(&guild).feeds().await.unwrap().len(), 1);
    h.keys("xy").await;
    let s = h.screen();
    assert_shows(&s, "Feeds are off. To turn them on, add [feeds] to bot.toml");
    assert!(!s.contains("follow a feed"), "nothing to follow with feeds off");
    h.keys("n").await;
    assert!(h.app.overlay.is_none());
}

#[tokio::test]
async fn members_are_appointed_named_and_taken_away() {
    let d = fixture();
    write(d.path(), "members.toml", "# who holds what\nadmin = []\n");
    let mut h = Harness::on(d).await;
    h.keys("0fay").await;
    h.fake().join(42);
    h.keys("7").await;
    assert_shows(&h.screen(), "▾ Admin");
    h.keys("n").await;
    h.key(KeyCode::Down).await;
    h.keys("nope").await;
    h.key(KeyCode::Enter).await;
    assert_shows(&h.screen(), "isn't a Discord user id");
    for _ in 0..4 {
        h.key(KeyCode::Backspace).await;
    }
    h.keys("42").await;
    h.key(KeyCode::Enter).await;
    assert_shows(&h.screen(), "Recorded in the config");
    let members = h.file("members.toml");
    assert!(members.starts_with("# who holds what") && members.contains("\"42\""), "{members}");

    h.select("▶     unknown member (id …42)").await;
    h.keys("x").await;
    assert_shows(&h.screen(), "Take Admin away from unknown member (id …42)? They lose it in Discord");
    h.keys("y").await;
    assert_shows(&h.screen(), "Taken out of the config");
    assert!(!h.file("members.toml").contains("\"42\""));
}

#[tokio::test]
async fn resources_are_shared_and_archived_after_a_yes() {
    let d = fixture();
    let bot = fs::read_to_string(d.path().join("bot.toml")).unwrap();
    write(d.path(), "bot.toml", &format!("{bot}[resources]\nforum = \"projects\"\n"));
    let mut h = Harness::on(d).await;
    h.keys("9").await;
    assert_shows(&h.screen(), "apply the config first");
    h.keys("0fay9").await;
    assert_shows(&h.screen(), "Golem — a CPU");
    h.keys("n").await;
    h.keys("Verilog guide").await;
    h.key(KeyCode::Down).await;
    h.keys("https://example.org/v").await;
    h.ctrl('s').await;
    let s = h.screen();
    assert_shows(&s, "Shared Verilog guide.");
    h.select("Verilog guide  ").await;
    assert_shows(&h.screen(), "https://example.org/v");
    h.keys("x").await;
    assert_shows(&h.screen(), "closed and locked, not deleted");
    h.keys("y").await;
    assert_shows(&h.screen(), "Archived: the post is closed and locked");
    assert!(h.fake().take_calls().iter().any(|c| c.starts_with("Patch channels/")));
}

#[tokio::test]
async fn plan_confirm_apply_with_progress_then_nothing_left() {
    let mut h = Harness::new().await;
    h.keys("0").await;
    let s = h.screen();
    assert_shows(&s, "Press f to plan");
    assert!(!has_key(&h.app, "apply"), "no apply key before a plan");

    assert!(matches!(h.app.on_key(KeyEvent::from(KeyCode::Char('f'))), Some(Command::Discord(_))));
    assert_shows(&h.screen(), "Planning: checking Discord");
    assert!(!has_key(&h.app, "plan") && !has_key(&h.app, "quit"), "no second plan or quitting while one runs");
    // Ctrl+C mid-job asks first.
    h.ctrl('c').await;
    assert!(!h.app.quit);
    assert_shows(&h.screen(), "Quit now?");
    h.keys("n").await;
    h.app.busy = None;

    h.keys("f").await;
    let s = h.screen();
    assert_shows(&s, "changes ready");
    assert_shows(&s, "+ create role admin");

    h.keys("a").await;
    assert_shows(&h.screen(), "Apply to Discord?");
    h.keys("y").await;
    let s = h.screen();
    assert_shows(&s, "✓ + create role admin");
    assert_shows(&s, "0 failed");

    h.keys("f").await;
    assert_shows(&h.screen(), "Discord matches the config");
    h.keys("d").await;
    assert_shows(&h.screen(), "✓ Discord matches the config.");
}

#[tokio::test]
async fn drift_sorts_discord_edits_from_config_edits() {
    let mut h = Harness::new().await;
    h.keys("0fay").await;
    h.fake().edit_channel("general", |c| c["topic"] = json!("from the UI"));
    h.keys("d").await;
    let s = h.screen();
    assert_shows(&s, "Changed in Discord:");
    assert_shows(&s, "channel general");
}

#[tokio::test]
async fn signed_in_it_works_through_the_web_server() {
    let config = tempfile::tempdir().unwrap();
    let server = testing::spawn(config.path()).await.unwrap();
    let token = testing::token(&server.url, "code-ada").await.unwrap();
    let mut h = Harness::new().await;
    let login = Login { server: server.url.clone(), token, user: "ada".into() };
    h.worker = h.worker.clone().through(Client::new(login).unwrap()).unwrap();
    h.app = App::with(h.dir.path().to_owned(), false, true);
    h.app.connected = Some(format!("connected to {} as ada", server.url));
    for job in h.app.startup() {
        h.run(Command::Discord(job)).await;
    }
    assert_shows(&h.screen(), "connected to http://127.0.0.1:");

    // The tabs show the server's copy, and edits go there.
    h.keys("2").await;
    h.select("▶ ● Admin").await;
    h.key(KeyCode::Enter).await;
    h.key(KeyCode::Down).await;
    h.key(KeyCode::Down).await;
    h.key(KeyCode::Right).await;
    h.ctrl('s').await;
    assert_shows(&h.screen(), "Saved on the server");
    assert!(fs::read_to_string(config.path().join("roles.toml")).unwrap().contains("hoist = true"));
    assert!(!h.file("roles.toml").contains("hoist"), "not the local files");

    h.keys("0f").await;
    // The server's plan: only its pretend Discord has a Mods role.
    assert_shows(&h.screen(), "role Mods isn't in config");
    h.keys("ay").await;
    assert_shows(&h.screen(), "0 failed");
    assert!(!server.discord.take_calls().is_empty(), "applied in Discord");
    assert!(h.fake().take_calls().is_empty(), "not by the local engine");

    h.keys("f").await;
    assert_shows(&h.screen(), "Discord matches the config");

    h.keys("6").await;
    assert_shows(&h.screen(), "The review queue is off");
}

#[tokio::test]
async fn pull_copies_discord_edits_into_files() {
    let mut h = Harness::new().await;
    h.keys("0fay").await;
    h.fake().edit_channel("general", |c| c["topic"] = json!("from the UI"));
    h.keys("p").await;
    assert_shows(&h.screen(), "Pull from Discord?");
    h.keys("y").await;
    assert_shows(&h.screen(), "✓ channel general");
    assert!(h.file("categories/10-main.toml").contains("from the UI"));
}

#[tokio::test]
async fn editing_files_makes_a_fetched_plan_stale() {
    let mut h = Harness::new().await;
    h.keys("0f4o0").await;
    assert_shows(&h.screen(), STALE);
    assert!(!has_key(&h.app, "apply"));
}

#[tokio::test]
async fn footer_lists_exactly_the_keys_that_work() {
    let mut h = Harness::new().await;
    for tab in Tab::ALL {
        h.keys(&tab.key().to_string()).await;
        for _ in 0..3 {
            let s = h.screen();
            for b in h.app.bindings() {
                assert_shows(&s, b.what);
            }
            h.key(KeyCode::Down).await;
        }
    }
    h.keys("4n").await;
    let s = h.screen();
    for b in h.app.bindings() {
        assert_shows(&s, b.what);
    }
    h.key(KeyCode::Esc).await;
    h.keys("1a").await;
    assert!(h.app.overlay.is_none(), "a does nothing on Overview");
    h.keys("q").await;
    assert!(h.app.quit);
}

#[tokio::test]
async fn discord_errors_show_in_plain_words() {
    let mut h = Harness::new().await;
    write(h.dir.path(), "lock.toml", "guild = \"999\"\n");
    h.keys("0f").await;
    assert_shows(&h.screen(), "✗ Discord said 404");
}

#[tokio::test]
async fn help_explains_plan_then_apply() {
    let mut h = Harness::new().await;
    h.keys("?").await;
    let s = h.screen();
    assert_shows(&s, "How it works");
    assert_shows(&s, "0, then f (plan)");
    h.keys("q").await;
    assert!(h.app.overlay.is_none() && !h.app.quit, "q closes the help, not the app");
}

#[tokio::test]
async fn reordering_takes_shift_and_moving_takes_j_and_k() {
    let mut h = Harness::new().await;
    h.keys("2j").await;
    assert_shows(&h.screen(), "▶ ● Admin");
    h.keys("k").await;
    assert_shows(&h.screen(), "▶ ● Bot");
    h.keys("jj").await;
    assert_shows(&h.screen(), "▶ ● Treasurer");
    let shift_up = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);
    if let Some(cmd) = h.app.on_key(shift_up) {
        h.run(cmd).await;
    }
    let roles = h.file("roles.toml");
    assert!(roles.find("Treasurer").unwrap() < roles.find("\"Admin\"").unwrap(), "{roles}");
    assert_shows(&h.screen(), "▶ ● Treasurer");
    h.keys("d").await;
    assert_eq!(h.file("roles.toml"), roles, "d no longer moves anything");
}

#[tokio::test]
async fn long_details_scroll_and_long_notices_wrap() {
    let mut h = Harness::new().await;
    h.keys("0fay").await;
    h.fake().join(42);
    let store = h.store().await;
    let guild = h.app.cfg.as_ref().unwrap().guild().unwrap().to_string();
    let answers: Vec<String> =
        (1..=30).map(|i| format!(r#"{{"question":"Question {i}?","answer":"Answer {i}"}}"#)).collect();
    let answers = format!("[{}]", answers.join(","));
    store
        .guild(&guild)
        .add_application(chipsoc_store::AppKind::Project, "golem", "42", &answers, false, 1)
        .await
        .unwrap();
    h.keys("6").await;
    // The footer offers scrolling once the pane is drawn too long to fit.
    h.screen();
    let s = h.screen();
    assert_shows(&s, "Question 1?");
    assert!(!s.contains("Answer 30"), "{s}");
    assert_shows(&s, "scroll details");
    for _ in 0..20 {
        h.key(KeyCode::PageDown).await;
    }
    let s = h.screen();
    assert_shows(&s, "Answer 30");
    assert!(!s.contains("Question 1?"), "{s}");
    h.key(KeyCode::Down).await;
    assert_eq!(h.app.detail_scroll, 0, "a new selection starts at the top");

    h.app.info(
        "Saved. To send it: 0, then f (plan), then a (apply). Note: this is a long note that runs past the edge.",
    );
    assert_shows(&h.screen_at(80, 24), "runs past the edge.");
}

#[tokio::test]
async fn a_narrow_screen_keeps_the_demo_badge_and_the_current_tab() {
    let mut h = Harness::new().await;
    h.app.demo = true;
    h.keys("6").await;
    let s = h.screen_at(80, 24);
    let top = s.lines().next().unwrap();
    assert!(top.starts_with(" DEMO "), "{top}");
    assert!(top.contains("6 Queue") && top.contains(" 0 ") && !top.contains("Roles"), "{top}");
    assert!(h.screen_at(80, 24).contains("q  quit"), "every key fits in the footer");
}

#[tokio::test]
async fn removing_a_lead_on_projects_asks_first() {
    let d = fixture();
    let p = fs::read_to_string(d.path().join("projects.toml")).unwrap();
    write(
        d.path(),
        "projects.toml",
        &p.replace("[project.application]", "lead_role = true\nleads = [\"42\"]\n[project.application]"),
    );
    let mut h = Harness::on(d).await;
    h.keys("0fay").await;
    h.fake().join(42);
    h.keys("4L").await;
    assert_shows(&h.screen(), "Enter  remove lead…");
    h.key(KeyCode::Enter).await;
    assert_shows(&h.screen(), "They lose the Lead role straight away.");
    h.keys("n").await;
    assert!(h.file("projects.toml").contains("\"42\""), "no means no");
}

#[tokio::test]
async fn a_tab_opened_mid_job_loads_when_the_job_ends() {
    let mut h = Harness::new().await;
    let Some(Command::Discord(job)) = h.app.on_key(KeyEvent::from(KeyCode::Char('r'))) else {
        // Overview reloads the activity log: a job.
        panic!("r starts a job");
    };
    h.keys("7").await;
    assert_shows(&h.screen(), "Loading…");
    h.run(Command::Discord(job)).await;
    assert!(!h.screen().contains("Loading…"), "{}", h.screen());
}

#[tokio::test]
async fn form_fields_take_a_cursor_paste_and_the_status_picker_takes_arrows() {
    let mut h = Harness::new().await;
    h.keys("4").await;
    h.key(KeyCode::Enter).await;
    h.key(KeyCode::Home).await;
    h.keys("The ").await;
    h.app.on_paste("pasted\n");
    assert!(h.app.overlay.is_some(), "a pasted newline doesn't submit");
    h.ctrl('s').await;
    assert!(h.file("projects.toml").contains("name = \"The pastedGolem\""), "{}", h.file("projects.toml"));

    h.keys("s").await;
    h.key(KeyCode::Down).await;
    h.key(KeyCode::Enter).await;
    assert_shows(&h.screen(), "Make The pastedGolem past");
}
