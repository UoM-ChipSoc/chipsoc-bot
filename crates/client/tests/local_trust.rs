//! The `--local` backend (CLI and TUI on the bot's machine) trusts what the
//! bot trusts: its own copy of lock.toml in its database, not the repo's.

use std::path::Path;
use std::sync::Arc;

use chipsoc_client::backend::{Backend, Local};
use chipsoc_discord::fake::FakeDiscord;
use chipsoc_engine::Options;

fn write(dir: &Path, file: &str, body: &str) {
    let p = dir.join(file);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

#[tokio::test]
async fn a_pushed_lock_change_is_not_trusted_locally() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("config");
    write(&dir, "server.toml", "[guild]\nname = \"Test\"\n");
    write(&dir, "bot.toml", "name = \"Test\"\nadmin_role = \"committee\"\n");
    write(
        &dir,
        "roles.toml",
        "[[role]]\nkey = \"bot\"\nname = \"Bot\"\nmanaged = true\n\n[[role]]\nkey = \"committee\"\nname = \"Committee\"\n\n\
         [[role]]\nkey = \"everyone\"\nname = \"@everyone\"\npermissions = [\"VIEW_CHANNEL\"]\n",
    );
    write(&dir, "categories/10-main.toml", "[category]\nkey = \"main\"\nname = \"Main\"\n");
    let db = format!("sqlite:{}?mode=rwc", root.path().join("bot.db").display());
    let fake = Arc::new(FakeDiscord::new());
    let local = || Backend::Local(Local::with_db(&dir, fake.clone(), Some(db.clone()), "tester"));

    let b = local();
    let plan = b.plan(Options::default()).await.unwrap();
    b.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
    assert!(local().plan(Options::default()).await.unwrap().plan.is_empty(), "applied");

    // A push points the committee role at another Discord role.
    let lock = std::fs::read_to_string(dir.join("lock.toml")).unwrap();
    let id = chipsoc_config::Config::load(&dir).unwrap().desired.lock.role("committee").unwrap().to_string();
    write(&dir, "lock.toml", &lock.replace(&id, "1000"));

    let plan = local().plan(Options::default()).await.unwrap().plan;
    let lines = chipsoc_engine::plan_lines(&plan).join("\n");
    assert!(plan.actions.is_empty(), "the bot's lock is used, so nothing to change: {lines}");

    // Without the bot's database, only the files are there to go by.
    let files = Backend::Local(Local::with_db(&dir, fake.clone(), None, "tester"));
    assert!(!files.plan(Options::default()).await.unwrap().plan.actions.is_empty());
}
