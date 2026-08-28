//! Small helpers for locating external command-line tools.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Usual host bin dirs a stripped pane PATH may omit.
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

fn appended_path(inherited: Option<&OsStr>) -> OsString {
    let Some(inherited) = inherited.filter(|p| !p.is_empty()) else {
        return OsString::from(COMMON_BINS.join(":"));
    };
    let mut path = inherited.to_os_string();
    path.push(":");
    path.push(COMMON_BINS.join(":"));
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

/// Resolve `program` on the host PATH — the common host bins first, the inherited PATH after —
/// and give the child that same PATH.
///
/// For the tools reviewr runs for itself. [`user_command`] is the other way round, for the
/// reviewer's own.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let program = program.as_ref();
    let path = host_path();
    let mut cmd = resolve_on(&path, program).map_or_else(|| Command::new(program), Command::new);
    cmd.env("PATH", path);
    cmd
}

/// Resolve `program` the way the reviewer's own shell would: their `PATH` first, the common
/// host bins only as a fallback. The child is given that same PATH. `None` when the name
/// resolves to nothing, so a caller can say so before it acts.
///
/// The opposite order from [`command`], and deliberately. `git` and the forge CLIs are the
/// host's tools, so a stripped pane PATH must not hide them. The editor is the reviewer's own,
/// so a version-managed shim on their `PATH` has to win over a stale copy in a common bin, and
/// so must every tool the editor goes on to launch — its language servers, its formatters, its
/// runtime.
pub(crate) fn user_command(program: impl AsRef<OsStr>) -> Option<Command> {
    let program = program.as_ref();
    let path = appended_path(env::var_os("PATH").as_deref());
    let mut cmd = Command::new(resolve_on(&path, program)?);
    cmd.env("PATH", path);
    Some(cmd)
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
    use super::{COMMON_BINS, appended_path, prepended_path, resolve_on};
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
    fn appended_path_leaves_the_reviewers_own_entries_in_front() {
        // The editor's own tools have to resolve the way its shell would resolve them, so a
        // version-managed shim wins and the common bins only backstop a stripped PATH.
        let got = appended_path(Some(OsStr::new("/me/.mise/shims:/usr/bin")));
        let parts: Vec<PathBuf> = env::split_paths(&got).collect();
        let mut expected = vec![PathBuf::from("/me/.mise/shims"), PathBuf::from("/usr/bin")];
        expected.extend(COMMON_BINS.iter().map(PathBuf::from));
        assert_eq!(parts, expected);

        let bare: Vec<PathBuf> = env::split_paths(&appended_path(None)).collect();
        assert_eq!(bare, COMMON_BINS.iter().map(PathBuf::from).collect::<Vec<_>>());

        // A set-but-empty PATH is the same as none. Joined instead, its empty entry would put
        // the reviewed repository's own working directory ahead of every real bin dir.
        assert_eq!(appended_path(Some(OsStr::new(""))), appended_path(None));
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
