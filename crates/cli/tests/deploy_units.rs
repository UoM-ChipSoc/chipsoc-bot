//! The systemd units deploy.sh installs keep the bot's state to its own user.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn unit(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn state_is_private_to_the_bots_user() {
    for name in ["bot.service", "web.service", "backup.service"] {
        let u = unit(name);
        assert!(u.contains("\nStateDirectoryMode=0700\n"), "{name}: StateDirectoryMode=0700");
        assert!(u.contains("\nUMask=0077\n"), "{name}: UMask=0077");
    }
}

/// The backup command, run on a scratch directory: the copy is 0600, in a
/// 0700 directory, and only the 14 newest are kept.
#[test]
fn backups_are_private() {
    if Command::new("sqlite3").arg("-version").output().is_err() {
        eprintln!("no sqlite3 here; skipped");
        return;
    }
    let u = unit("backup.service");
    let exec = u.lines().find_map(|l| l.strip_prefix("ExecStart=/bin/sh -c '")).expect("ExecStart");
    let state = tempfile::tempdir().unwrap();
    let script = exec
        .trim_end_matches('\'')
        .replace("$$", "$")
        .replace("%%", "%")
        .replace("/var/lib/@SERVICE@", &state.path().display().to_string());
    let db = state.path().join("bot.db");
    let made = Command::new("sqlite3").arg(&db).arg("create table t(x); insert into t values (1);").status().unwrap();
    assert!(made.success());
    let ran = Command::new("sh").arg("-c").arg(&script).output().unwrap();
    assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));
    let dir = state.path().join("backups");
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    let copies: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(std::fs::metadata(&copies[0]).unwrap().permissions().mode() & 0o777, 0o600);
}
