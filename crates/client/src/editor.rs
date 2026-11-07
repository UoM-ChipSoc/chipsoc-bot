//! The person's own text editor, for the CLI and TUI: `$VISUAL` or `$EDITOR`
//! (else nano), which may carry arguments (`emacs -nw`). Editors that open a
//! window and return at once (VS Code, Zed, Sublime Text) are told to wait.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Open `path` at `line` (from 1), and wait for the editor to close.
pub fn open_editor(path: &Path, line: usize) -> Result<()> {
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "nano".into());
    let mut cmd = command(&editor, path, line).context("$EDITOR is empty")?;
    let status = cmd.status().with_context(|| format!("couldn't start {editor}"))?;
    if !status.success() {
        bail!("{editor} exited with {status}");
    }
    Ok(())
}

/// Edit `text` in a scratch file ending `suffix` (`.md`, `.txt`); what it says after.
pub fn edit_text(text: &str, suffix: &str) -> Result<String> {
    let file = tempfile::Builder::new().suffix(suffix).tempfile()?;
    std::fs::write(file.path(), text)?;
    open_editor(file.path(), 1)?;
    Ok(std::fs::read_to_string(file.path())?)
}

/// How to run `editor` on `path` at `line`; none if `editor` is blank.
fn command(editor: &str, path: &Path, line: usize) -> Option<Command> {
    let mut words = editor.split_whitespace();
    let program = words.next()?;
    let args: Vec<&str> = words.collect();
    let mut cmd = Command::new(program);
    cmd.args(&args);
    let at = format!("{}:{line}", path.display());
    let name = Path::new(program).file_stem().and_then(|n| n.to_str()).unwrap_or(program);
    let wait = |cmd: &mut Command, flag: &str| {
        if !args.contains(&flag) {
            cmd.arg(flag);
        }
    };
    match name {
        "code" | "code-insiders" | "codium" | "code-oss" => {
            wait(&mut cmd, "--wait");
            cmd.arg("--goto").arg(at);
        }
        "zed" | "subl" => {
            wait(&mut cmd, "--wait");
            cmd.arg(at);
        }
        _ => {
            cmd.arg(format!("+{line}")).arg(path);
        }
    }
    Some(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(editor: &str) -> Vec<String> {
        let cmd = command(editor, Path::new("/t/a.md"), 3).unwrap();
        let mut out = vec![cmd.get_program().to_string_lossy().into_owned()];
        out.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
        out
    }

    #[test]
    fn editors_open_at_the_line_and_window_ones_wait() {
        assert_eq!(args("nano"), ["nano", "+3", "/t/a.md"]);
        assert_eq!(args("emacs -nw"), ["emacs", "-nw", "+3", "/t/a.md"]);
        assert_eq!(args("/usr/bin/code"), ["/usr/bin/code", "--wait", "--goto", "/t/a.md:3"]);
        assert_eq!(args("code --wait"), ["code", "--wait", "--goto", "/t/a.md:3"], "not told twice");
        assert_eq!(args("zed"), ["zed", "--wait", "/t/a.md:3"]);
        assert!(command("  ", Path::new("a"), 1).is_none());
    }
}
