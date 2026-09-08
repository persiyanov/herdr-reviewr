//! Small helpers for locating external command-line tools.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Usual host bin dirs a stripped pane PATH may omit. Unix only: Windows has no equivalent
/// gap between a pane's inherited PATH and the tools reviewr needs, so the list is empty and
/// every function below degrades to passing the inherited PATH through unchanged.
#[cfg(unix)]
const COMMON_BINS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];
#[cfg(windows)]
const COMMON_BINS: &[&str] = &[];

#[cfg(unix)]
const PATH_SEP: &str = ":";
#[cfg(windows)]
const PATH_SEP: &str = ";";

fn host_path() -> OsString {
    prepended_path(env::var_os("PATH").as_deref())
}

fn prepended_path(inherited: Option<&OsStr>) -> OsString {
    let mut path = OsString::from(COMMON_BINS.join(PATH_SEP));
    if let Some(inherited) = inherited
        && !inherited.is_empty()
    {
        if !path.is_empty() {
            path.push(PATH_SEP);
        }
        path.push(inherited);
    }
    path
}

fn appended_path(inherited: Option<&OsStr>) -> OsString {
    let common = OsString::from(COMMON_BINS.join(PATH_SEP));
    let Some(inherited) = inherited.filter(|p| !p.is_empty()) else {
        return common;
    };
    if common.is_empty() {
        return inherited.to_os_string();
    }
    let mut path = inherited.to_os_string();
    path.push(PATH_SEP);
    path.push(&common);
    path
}

/// The default `PATHEXT` Windows uses when the environment doesn't set one.
#[cfg(windows)]
const DEFAULT_PATHEXT: &[&str] = &[".COM", ".EXE", ".BAT", ".CMD"];

/// `raw` is the `PATHEXT` env var's value, if any; injected rather than read here so tests can
/// exercise every branch without mutating real process env.
#[cfg(windows)]
fn pathext_list(raw: Option<&str>) -> Vec<String> {
    match raw.filter(|v| !v.is_empty()) {
        Some(v) => v.split(';').filter(|e| !e.is_empty()).map(str::to_string).collect(),
        None => DEFAULT_PATHEXT.iter().map(|s| (*s).to_string()).collect(),
    }
}

#[cfg(windows)]
fn resolve_bare_with_pathext(
    dir: &Path,
    name: &OsStr,
    pathext_raw: Option<&str>,
) -> Option<PathBuf> {
    pathext_list(pathext_raw).into_iter().find_map(|ext| {
        let mut candidate_name = OsString::from(name);
        candidate_name.push(ext);
        let candidate = dir.join(candidate_name);
        candidate.is_file().then_some(candidate)
    })
}

fn resolve_on(path: &OsStr, name: &OsStr) -> Option<PathBuf> {
    let as_path = Path::new(name);
    let is_explicit_path =
        as_path.is_absolute() || as_path.parent().is_some_and(|p| !p.as_os_str().is_empty());

    #[cfg(windows)]
    {
        if is_explicit_path {
            return as_path.is_file().then(|| as_path.to_path_buf());
        }
        if as_path.extension().is_some() {
            // A bare name with its own extension is still searched across PATH, just without
            // PATHEXT expansion on top of it.
            return env::split_paths(path).find_map(|dir| {
                let candidate = dir.join(name);
                candidate.is_file().then_some(candidate)
            });
        }
        let pathext = env::var("PATHEXT").ok();
        env::split_paths(path)
            .find_map(|dir| resolve_bare_with_pathext(&dir, name, pathext.as_deref()))
    }

    #[cfg(unix)]
    {
        if is_explicit_path {
            return as_path.is_file().then(|| as_path.to_path_buf());
        }
        env::split_paths(path).find_map(|dir| {
            let candidate = dir.join(name);
            candidate.is_file().then_some(candidate)
        })
    }
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

/// Whether `name` resolves to an executable on the host PATH — a dependency-free `which`.
/// Shared by the clipboard probe (`export.rs`) and the URL-opener probe (`browser.rs`).
#[must_use]
pub fn on_path(name: &str) -> bool {
    resolve_on(&host_path(), OsStr::new(name)).is_some()
}

/// The first `(program, args)` candidate whose `program` the `present` predicate accepts,
/// preserving list order. Shared by the clipboard tool probe (`export.rs`) and the URL opener
/// probe (`browser.rs`) — both try a short platform-specific tool list and use whichever entry
/// is actually installed.
pub(crate) fn select_first_present(
    candidates: &'static [(&'static str, &'static [&'static str])],
    present: impl Fn(&str) -> bool,
) -> Option<(&'static str, &'static [&'static str])> {
    candidates.iter().copied().find(|(cmd, _)| present(cmd))
}

#[cfg(test)]
mod tests {
    use super::resolve_on;
    #[cfg(unix)]
    use std::env;
    #[cfg(unix)]
    use std::ffi::OsStr;
    #[cfg(unix)]
    use std::path::PathBuf;

    #[cfg(unix)]
    mod unix {
        use super::super::{COMMON_BINS, appended_path, prepended_path};
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
    }

    #[cfg(windows)]
    mod windows {
        use super::super::{appended_path, prepended_path};
        use std::env;
        use std::ffi::OsStr;
        use std::path::PathBuf;

        #[test]
        fn proc_windows_no_unix_bin_dirs_prepended() {
            let got = prepended_path(Some(OsStr::new(r"C:\inherited")));
            let parts: Vec<PathBuf> = env::split_paths(&got).collect();
            assert_eq!(parts, vec![PathBuf::from(r"C:\inherited")]);
        }

        #[test]
        fn proc_windows_inherited_path_preserved() {
            let inherited = r"C:\a;C:\b";
            let got = prepended_path(Some(OsStr::new(inherited)));
            assert_eq!(got, OsStr::new(inherited));

            let got = appended_path(Some(OsStr::new(inherited)));
            assert_eq!(got, OsStr::new(inherited));
        }

        #[test]
        fn proc_windows_no_inherited_path_yields_empty() {
            assert_eq!(prepended_path(None), OsStr::new(""));
            assert_eq!(appended_path(None), OsStr::new(""));
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_on_finds_a_bare_name_in_a_path_directory() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("gh");
        std::fs::write(&bin, []).unwrap();
        let path = env::join_paths([dir.path(), PathBuf::from("/usr/bin").as_path()]).unwrap();
        assert_eq!(resolve_on(&path, OsStr::new("gh")).as_deref(), Some(bin.as_path()));
        assert!(resolve_on(&path, OsStr::new("missing")).is_none());
    }

    #[cfg(windows)]
    mod pathext {
        use super::super::resolve_bare_with_pathext;
        use super::resolve_on;
        use std::env;
        use std::ffi::OsStr;

        #[test]
        fn proc_windows_pathext_resolves_bare_name_with_default_list_when_unset() {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("probe.EXE");
            std::fs::write(&bin, []).unwrap();

            let found = resolve_bare_with_pathext(dir.path(), OsStr::new("probe"), None);
            assert_eq!(found.as_deref(), Some(bin.as_path()));
        }

        #[test]
        fn proc_windows_pathext_custom_list_and_order() {
            let dir = tempfile::tempdir().unwrap();
            let bat = dir.path().join("probe.BAT");
            let exe = dir.path().join("probe.EXE");
            std::fs::write(&bat, []).unwrap();
            std::fs::write(&exe, []).unwrap();

            let found =
                resolve_bare_with_pathext(dir.path(), OsStr::new("probe"), Some(".BAT;.EXE"));
            assert_eq!(found.as_deref(), Some(bat.as_path()));

            let found =
                resolve_bare_with_pathext(dir.path(), OsStr::new("probe"), Some(".EXE;.BAT"));
            assert_eq!(found.as_deref(), Some(exe.as_path()));
        }

        #[test]
        fn proc_windows_explicit_extension_bypasses_pathext() {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("probe.exe");
            std::fs::write(&bin, []).unwrap();
            let path = env::join_paths([dir.path()]).unwrap();
            // With an explicit extension, the exact file must exist; no PATHEXT expansion.
            assert_eq!(resolve_on(&path, OsStr::new("probe.exe")).as_deref(), Some(bin.as_path()));
            assert!(resolve_on(&path, OsStr::new("probe.bat")).is_none());
        }

        #[test]
        fn proc_windows_explicit_path_used_as_is() {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("probe.exe");
            std::fs::write(&bin, []).unwrap();
            let explicit = bin.to_str().unwrap();
            assert_eq!(
                resolve_on(OsStr::new(""), OsStr::new(explicit)).as_deref(),
                Some(bin.as_path())
            );
        }
    }
}
