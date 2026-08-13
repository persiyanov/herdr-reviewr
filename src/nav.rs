//! CLI navigation channel: `herdr-reviewr nav` writes a one-shot command file for the
//! running sidebar on the same repo, which applies and removes it on its next wakeup.
//! File-based so the CLI needs no socket into the TUI; keyed by repo path so concurrent
//! sidebars on different repos never cross.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app::Tab;
use crate::model::Scope;

/// A one-shot navigation command. Every field is optional; present ones apply in
/// tab → scope → file order. Fields hold the CLI spellings, parsed on apply, so a
/// stale file from a newer CLI degrades to a status-line error instead of a crash.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

pub fn parse_scope(name: &str) -> Option<Scope> {
    match name {
        "uncommitted" | "u" => Some(Scope::Uncommitted),
        "branch" | "b" => Some(Scope::Branch),
        "last-turn" | "t" => Some(Scope::LastTurn),
        _ => None,
    }
}

pub fn parse_tab(name: &str) -> Option<Tab> {
    match name {
        "changes" | "1" => Some(Tab::Changes),
        "all" | "all-files" | "2" => Some(Tab::AllFiles),
        "pr" | "3" => Some(Tab::Pr),
        _ => None,
    }
}

/// The command file for `repo`: in the system temp dir, named by an FNV-1a hash of the
/// canonical repo path (stable across processes, unlike `DefaultHasher`).
pub fn path_for(repo: &Path) -> PathBuf {
    let canon = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canon.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    std::env::temp_dir().join(format!("herdr-reviewr-nav-{hash:016x}.json"))
}

/// Write `cmd` for the sidebar on `repo`, atomically, so a concurrent `take` never
/// reads a partial file.
pub fn write(repo: &Path, cmd: &NavCommand) -> anyhow::Result<PathBuf> {
    let path = path_for(repo);
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(cmd)?)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Take the pending command for `repo`, if any: read it and remove the file, so a
/// command applies exactly once. An unreadable file is dropped the same way.
pub fn take(repo: &Path) -> Option<NavCommand> {
    let path = path_for(repo);
    let bytes = fs::read(&path).ok()?;
    let _ = fs::remove_file(&path);
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_command_is_taken_exactly_once() {
        let repo = std::env::temp_dir().join("reviewr-nav-test-repo");
        let cmd = NavCommand {
            tab: Some("changes".into()),
            scope: Some("branch".into()),
            file: Some("src/lib.rs".into()),
        };
        write(&repo, &cmd).unwrap();
        assert_eq!(take(&repo), Some(cmd));
        assert_eq!(take(&repo), None);
    }

    #[test]
    fn distinct_repos_get_distinct_command_files() {
        assert_ne!(path_for(Path::new("/a/repo")), path_for(Path::new("/b/repo")));
    }
}
