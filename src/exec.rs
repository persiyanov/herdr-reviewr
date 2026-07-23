//! Spawn the user's editor for "open in editor" (Files pane, `e`).
//!
//! See `specs/config.md` (`editor`) and `specs/input.md` ("Open in editor"). No shell ever
//! runs: the command is whitespace-split into argv, so a config value is never interpreted as
//! shell syntax.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// The editor command, resolved: the configured value, else `$EDITOR`. `None` when neither is
/// set (`specs/config.md`).
#[must_use]
pub fn resolve_editor(configured: Option<&str>) -> Option<String> {
    configured
        .map(str::to_string)
        .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.trim().is_empty()))
}

/// Split `editor` into a program and its arguments, `path` appended as the final argument.
/// `None` when `editor` has no non-whitespace content.
fn argv(editor: &str, path: &Path) -> Option<(String, Vec<OsString>)> {
    let mut words = editor.split_whitespace();
    let program = words.next()?.to_string();
    let mut args: Vec<OsString> = words.map(OsString::from).collect();
    args.push(path.as_os_str().to_owned());
    Some((program, args))
}

/// Run `editor`'s argv against `path`, inheriting this process's stdio and waiting on it
/// synchronously. The caller (`lib.rs`) leaves the alternate screen and raw mode first and
/// restores both after — this only spawns and waits. A non-zero exit is not treated as a
/// failure: many editors exit non-zero on a cancelled save, and the file may still have
/// changed, so the caller refreshes regardless.
pub fn run_editor(editor: &str, path: &Path) -> Result<()> {
    let (program, args) = argv(editor, path).context("`editor` is empty")?;
    Command::new(&program)
        .args(&args)
        .status()
        .with_context(|| format!("spawning editor {program:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{argv, resolve_editor, run_editor};
    use std::path::Path;

    #[test]
    fn argv_splits_the_command_and_appends_the_path() {
        let (program, args) = argv("code --wait", Path::new("/tmp/a.rs")).unwrap();
        assert_eq!(program, "code");
        assert_eq!(args, vec!["--wait", "/tmp/a.rs"]);
    }

    #[test]
    fn argv_is_none_for_an_empty_command() {
        assert!(argv("   ", Path::new("/tmp/a.rs")).is_none());
    }

    #[test]
    fn configured_value_wins_over_the_environment() {
        assert_eq!(resolve_editor(Some("code --wait")), Some("code --wait".to_string()));
    }

    #[test]
    fn spawns_the_configured_program_against_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "").unwrap();
        assert!(run_editor("true", &file).is_ok());
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "").unwrap();
        let error = run_editor("definitely-not-a-real-editor-binary", &file).unwrap_err();
        assert!(error.to_string().contains("definitely-not-a-real-editor-binary"));
    }

    #[test]
    fn empty_editor_string_is_an_error() {
        let error = run_editor("   ", Path::new("a.txt")).unwrap_err();
        assert!(error.to_string().contains("empty"));
    }
}
