//! The area commands, here on a temp config and through a test web server.

use std::fs;
use std::path::Path;

use chipsoc_client::remote::{Client, Login};
use chipsoc_client::{Backend, Local};
use chipsoc_config::ops::{Op, RoleField};
use chipsoc_web::testing;

use crate::manage::*;

fn write(dir: &Path, file: &str, body: &str) {
    let p = dir.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A small config with comments, never applied.
fn config(p: &Path) {
    write(p, "server.toml", "[guild]\nname = \"Test\"\n");
    write(p, "bot.toml", "# the bot\nname = \"Test\"\nadmin_role = \"admin\"\narchive = \"archive\"\n");
    write(
        p,
        "roles.toml",
        "# roles, top first\n[[role]]\nkey = \"admin\"\nname = \"Admin\" # the committee\n\n[[role]]\nkey = \"member\"\n\
         name = \"Member\"\n\n[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\n",
    );
    write(
        p,
        "categories/10-main.toml",
        "# main\n[category]\nkey = \"main\"\nname = \"Main\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\n\n\
         [[channel]]\nkey = \"rules\"\nname = \"rules\"\npost = \"content/rules.md\"\n",
    );
    write(p, "categories/90-archive.toml", "[category]\nkey = \"archive\"\nname = \"Archive\"\n");
    write(p, "content/rules.md", "Be kind.\n");
    write(
        p,
        "projects.toml",
        "# projects\n[[project]]\nslug = \"golem\"\nname = \"Golem\"\nstatus = \"active\"\nchannels = [\"general\"]\n",
    );
}

fn read(p: &Path, file: &str) -> String {
    fs::read_to_string(p.join(file)).unwrap()
}

#[tokio::test]
async fn edits_here_go_through_ops_and_keep_comments() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    config(p);
    let b = Backend::Local(Local::with_db(p, CliRest::NoToken { why: "no token".into() }, None, "me"));
    let c = Ctx { b: &b, json: false, yes: true, dir: p };

    let set = RoleCmd::Set { key: "member".into(), set: role_set(&["--hoist", "on", "--color", "#336699"]) };
    role(&c, set).await.unwrap();
    let roles = read(p, "roles.toml");
    assert!(
        roles.contains("# the committee") && roles.contains("hoist = true") && roles.contains("#336699"),
        "{roles}"
    );

    position(&c, PositionCmd::New { role: "member".into(), label: Some("Helper".into()), admin: false, closed: true })
        .await
        .unwrap();
    position(&c, PositionCmd::Open { role: "member".into() }).await.unwrap();
    let bot = read(p, "bot.toml");
    assert!(
        bot.starts_with("# the bot") && bot.contains("label = \"Helper\"") && !bot.contains("open = false"),
        "{bot}"
    );

    let perms =
        ChannelCmd::Perms { key: "general".into(), perms: perms(&["--deny", "everyone:send messages"]), inherit: None };
    channel(&c, perms).await.unwrap();
    assert!(read(p, "categories/10-main.toml").contains("SEND_MESSAGES"));

    project(&c, ProjectCmd::Status { slug: "golem".into(), status: chipsoc_config::ProjectStatus::Past })
        .await
        .unwrap();
    let projects = read(p, "projects.toml");
    assert!(projects.starts_with("# projects") && projects.contains("status = \"past\""), "{projects}");

    let body = p.join("new-rules.md");
    fs::write(&body, "Be kind.\nSay hi in {#general}.\n").unwrap();
    post(&c, PostCmd::Set { channel: "rules".into(), file: Some(body) }).await.unwrap();
    assert!(read(p, "content/rules.md").contains("Say hi"));

    raw_op(&c, r#"{"op": "rename_category", "key": "main", "name": "Lobby"}"#).await.unwrap();
    assert!(read(p, "categories/10-main.toml").contains("name = \"Lobby\""));

    // A refused edit writes nothing.
    let before = read(p, "roles.toml");
    let err = role(&c, RoleCmd::Remove { key: "admin".into() }).await.unwrap_err();
    assert_eq!(err.to_string(), "nothing was changed");
    assert_eq!(read(p, "roles.toml"), before);
    assert!(raw_op(&c, r#"{"op": "launch"}"#).await.is_err());

    // Work that needs the bot's database says where to find it.
    let err = queue(&c, QueueCmd::List).await.unwrap_err().to_string();
    assert!(err.contains("login --server"), "{err}");
    status(&c).await.unwrap();
}

/// Parse `role set` flags the way clap does.
fn role_set(args: &[&str]) -> RoleSetArgs {
    parse::<RoleSetArgs>(args)
}

fn perms(args: &[&str]) -> PermsArgs {
    parse::<PermsArgs>(args)
}

fn parse<T: clap::Args + clap::FromArgMatches>(args: &[&str]) -> T {
    let cmd = T::augment_args(clap::Command::new("t"));
    let m = cmd.try_get_matches_from(std::iter::once("t").chain(args.iter().copied())).unwrap();
    T::from_arg_matches(&m).unwrap()
}

#[test]
fn the_command_line_parses() {
    use clap::CommandFactory;
    crate::Cli::command().debug_assert();
}

#[test]
fn every_command_and_argument_says_what_it_is() {
    use clap::CommandFactory;
    fn walk(c: &clap::Command, path: &str, missing: &mut Vec<String>) {
        if c.get_about().is_none() {
            missing.push(path.to_owned());
        }
        for a in c.get_arguments().filter(|a| !a.is_hide_set() && !a.is_global_set()) {
            if a.get_help().is_none() && !matches!(a.get_id().as_str(), "help" | "version") {
                missing.push(format!("{path} <{}>", a.get_id()));
            }
        }
        for s in c.get_subcommands().filter(|s| s.get_name() != "help") {
            walk(s, &format!("{path} {}", s.get_name()), missing);
        }
    }
    let mut cli = crate::Cli::command();
    cli.build();
    let mut missing = Vec::new();
    walk(&cli, "chipsoc-bot", &mut missing);
    assert!(missing.is_empty(), "no help text: {missing:#?}");
}

#[tokio::test]
async fn every_area_works_through_the_web_server() {
    let dir = tempfile::tempdir().unwrap();
    let server = testing::spawn(dir.path()).await.unwrap();
    let token = testing::token(&server.url, "code-ada").await.unwrap();
    let client = Client::new(Login { server: server.url.clone(), token, user: "ada".into() }).unwrap();
    let b = Backend::<CliRest>::Remote(client);
    let c = Ctx { b: &b, json: true, yes: true, dir: dir.path() };

    status(&c).await.unwrap();
    drift(&c).await.unwrap();
    role(&c, RoleCmd::List).await.unwrap();
    role(&c, RoleCmd::Set { key: "admin".into(), set: role_set(&["--mentionable", "on"]) }).await.unwrap();
    assert!(read(dir.path(), "roles.toml").contains("mentionable = true"), "edited on the server's copy");
    category(&c, CategoryCmd::List).await.unwrap();
    channel(&c, ChannelCmd::Set { key: "general".into(), set: channel_set(&["--topic", "Say hi"]) }).await.unwrap();
    assert!(read(dir.path(), "categories/10-main.toml").contains("topic = \"Say hi\""));
    channel(&c, ChannelCmd::Archive { key: "projects".into() }).await.unwrap();
    assert!(!read(dir.path(), "categories/10-main.toml").contains("key = \"projects\""));
    project(&c, ProjectCmd::Applications { slug: "golem".into(), state: true }).await.unwrap();
    position(&c, PositionCmd::List).await.unwrap();
    member(&c, MemberCmd::List).await.unwrap();
    let err = member(&c, MemberCmd::Appoint { role: "admin".into(), user: "502".into() }).await.unwrap_err();
    assert!(err.to_string().contains("doesn't say who holds"), "{err}");
    queue(&c, QueueCmd::List).await.unwrap();
    queue(&c, QueueCmd::History { limit: 5 }).await.unwrap();
    assert!(queue(&c, QueueCmd::Reject { id: 9, reason: Some("no".into()) }).await.is_err());
    feed(&c, FeedCmd::List).await.unwrap();
    assert!(feed(&c, FeedCmd::Pause { id: 1 }).await.unwrap_err().to_string().contains("No such feed"));
    assert!(resource(&c, ResourceCmd::List { limit: 5 }).await.unwrap_err().to_string().contains("resources"));
    post(&c, PostCmd::List).await.unwrap();
    onboarding(&c, OnboardingCmd::List).await.unwrap();
    activity(&c, 5).await.unwrap();
    schema(&c).await.unwrap();
    let op = serde_json::to_string(&Op::SetRole { key: "admin".into(), set: RoleField::Hoist(true) }).unwrap();
    raw_op(&c, &op).await.unwrap();
    assert!(read(dir.path(), "roles.toml").contains("hoist = true"));
}

fn channel_set(args: &[&str]) -> ChannelSetArgs {
    parse::<ChannelSetArgs>(args)
}

#[cfg(unix)]
#[test]
fn secrets_are_written_private_and_never_through_a_symlink() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let env = d.path().join(".env");
    std::fs::write(&env, "OLD=1\n").unwrap();
    std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o644)).unwrap();
    crate::write_secret(&env, "KEY=secret\n").unwrap();
    assert_eq!(std::fs::read_to_string(&env).unwrap(), "KEY=secret\n");
    assert_eq!(std::fs::metadata(&env).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1, "no temporary file left");

    let target = d.path().join("elsewhere");
    std::fs::write(&target, "untouched").unwrap();
    let link = d.path().join("link.env");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(crate::write_secret(&link, "KEY=secret\n").unwrap_err().to_string().contains("symlink"));
    assert!(crate::fed_keygen(d.path(), Some(&link), true).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
}

#[test]
fn fed_keygen_writes_the_env_file_it_is_given() {
    let d = tempfile::tempdir().unwrap();
    let server = d.path().join(".env.server");
    std::fs::write(&server, "DISCORD_TOKEN=x\n").unwrap();
    crate::fed_keygen(d.path(), Some(&server), false).unwrap();
    let body = std::fs::read_to_string(&server).unwrap();
    assert!(body.starts_with("DISCORD_TOKEN=x\nFED_SIGNING_KEY="), "{body}");
    assert!(crate::fed_keygen(d.path(), Some(&server), false).is_err(), "an existing key needs --force");
    assert!(!d.path().join(".env").exists(), "no other file is written");
    let cli = <crate::Cli as clap::Parser>::try_parse_from(["bot", "fed", "keygen", "--env", ".env.server"]).unwrap();
    assert!(matches!(cli.cmd, Some(crate::Cmd::Fed(crate::FedCmd::Keygen { env: Some(_), .. }))));
}

#[test]
fn fed_revoke_never_opens_a_new_database() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("bot.db");
    let err = crate::bots_database(false, &db).unwrap_err().to_string();
    assert!(err.contains("DATABASE_URL") && err.contains("deploy.sh revoke"), "{err}");
    assert!(crate::bots_database(true, &db).is_ok(), "DATABASE_URL says where");
    std::fs::write(&db, "").unwrap();
    assert!(crate::bots_database(false, &db).is_ok(), "the default one is here");
}

/// Laptops carry no bot token: what reaches Discord or the bot's data needs a sign-in.
#[test]
fn commands_that_reach_discord_need_a_sign_in() {
    use clap::Parser;
    let cmd = |args: &[&str]| crate::Cli::try_parse_from([&["chipsoc-bot"], args].concat()).unwrap().cmd.unwrap();
    let needs = |args: &[&str]| crate::needs(&cmd(args), false);
    for offline in [&["open", "role"][..], &["template", "abc", "d"], &["tui", "--demo"], &["login"], &["fed", "key"]] {
        assert_eq!(needs(offline), crate::Needs::Nothing, "{offline:?}");
    }
    assert_eq!(crate::needs(&cmd(&["check"]), true), crate::Needs::Nothing, "check on files here");
    for online in [
        &["plan"][..],
        &["apply"],
        &["pull"],
        &["check"],
        &["export", "d"],
        &["status"],
        &["queue", "list"],
        &["member", "add", "chair", "1"],
        &["tui"],
    ] {
        assert_eq!(needs(online), crate::Needs::SignIn, "{online:?}");
    }
    for server_only in [&["run"][..], &["web"], &["fed", "revoke", "x"]] {
        assert_eq!(needs(server_only), crate::Needs::Local, "{server_only:?}");
    }

    let dir = tempfile::tempdir().unwrap();
    config(dir.path());
    let said = crate::sign_in_first(dir.path()).to_string();
    assert!(said.starts_with("Sign in first: chipsoc-bot login --server <url>"), "{said}");
    write(dir.path(), "bot.toml", "name = \"Test\"\n[web]\nurl = \"https://dash.example\"\n");
    let said = crate::sign_in_first(dir.path()).to_string();
    assert!(said.starts_with("Sign in first: chipsoc-bot login --server https://dash.example"), "{said}");

    let local = crate::Cli::try_parse_from(["chipsoc-bot", "--local", "plan"]).unwrap();
    assert!(local.local);
}
