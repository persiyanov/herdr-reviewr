//! Small helpers for locating external command-line tools.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Usual host bin dirs a stripped pane PATH may omit (`specs/herdr-host.md` HH-LAUNCHER-BLIND).
const COMMON_BINS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];

fn host_path() -> OsString {
    prepended_path(env::var_os("PATH").as_deref())
}

fn prepended_path(inherited: Option<&OsStr>) -> OsString {
    let mut path = OsString::from(COMMON_BINS.join(":"));
    if let Some(inherited) = inherited
        && !inherited.is_empty()
    {
        path.push(":");
        path.push(inherited);
    }
    path
}

fn resolve_on(path: &OsStr, name: &OsStr) -> Option<PathBuf> {
    let as_path = Path::new(name);
    if as_path.is_absolute() || as_path.parent().is_some_and(|p| !p.as_os_str().is_empty()) {
        return as_path.is_file().then(|| as_path.to_path_buf());
    }
    env::split_paths(path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

/// Resolve `program` on the host PATH and give the child that same PATH
/// (`specs/herdr-host.md` HH-LAUNCHER-BLIND).
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let program = program.as_ref();
    let path = host_path();
    let mut cmd = resolve_on(&path, program).map_or_else(|| Command::new(program), Command::new);
    cmd.env("PATH", path);
    cmd
}

/// Whether `name` resolves to an executable on the host PATH — a dependency-free `which`. Both
/// shipped platforms are unix, so a file in a host-PATH directory is the executable. Shared by
/// the clipboard probe (`export.rs`) and the URL-opener probe (`browser.rs`).
#[must_use]
pub fn on_path(name: &str) -> bool {
    resolve_on(&host_path(), OsStr::new(name)).is_some()
}

#[cfg(test)]
mod tests {
    use super::{COMMON_BINS, prepended_path, resolve_on};
    use std::env;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    #[test]
    fn prepended_path_puts_the_common_bins_in_front_of_the_inherited_path() {
        let got = prepended_path(Some(OsStr::new("/usr/bin:/bin")));
        let parts: Vec<PathBuf> = env::split_paths(&got).collect();
        let mut expected: Vec<PathBuf> = COMMON_BINS.iter().map(PathBuf::from).collect();
        expected.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        assert_eq!(parts, expected);
    }

    #[test]
    fn prepended_path_keeps_the_common_bins_when_nothing_is_inherited() {
        let got = prepended_path(None);
        let parts: Vec<PathBuf> = env::split_paths(&got).collect();
        let expected: Vec<PathBuf> = COMMON_BINS.iter().map(PathBuf::from).collect();
        assert_eq!(parts, expected);
    }

    #[test]
    fn resolve_on_finds_a_bare_name_in_a_path_directory() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("gh");
        std::fs::write(&bin, []).unwrap();
        let path = env::join_paths([dir.path(), PathBuf::from("/usr/bin").as_path()]).unwrap();
        assert_eq!(resolve_on(&path, OsStr::new("gh")).as_deref(), Some(bin.as_path()));
        assert!(resolve_on(&path, OsStr::new("missing")).is_none());
    }
}
