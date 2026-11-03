//! External command-line tools: located, named, and run within a bound.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use process_wrap::std::CommandWrap;

/// Host bin dirs a stripped PATH may omit; none on Windows, where `\\usr\\bin` is plantable.
#[cfg(unix)]
const COMMON_BINS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];
#[cfg(not(unix))]
const COMMON_BINS: &[&str] = &[];

/// Homebrew's Linux prefixes, where a `gh` it installed lives: the shared and per-user installs,
/// then the prefix the environment names. Searched last, so a tool on `PATH` keeps its place.
#[cfg(unix)]
fn brew_bins(home: Option<&OsStr>, prefix: Option<&OsStr>) -> Vec<PathBuf> {
    let mut bins = vec![PathBuf::from("/home/linuxbrew/.linuxbrew/bin")];
    if let Some(home) = home {
        bins.push(PathBuf::from(home).join(".linuxbrew").join("bin"));
    }
    if let Some(prefix) = prefix {
        bins.push(PathBuf::from(prefix).join("bin"));
    }
    bins
}

/// Homebrew does not run on Windows, so there are no prefixes to name.
#[cfg(not(unix))]
fn brew_bins(_home: Option<&OsStr>, _prefix: Option<&OsStr>) -> Vec<PathBuf> {
    Vec::new()
}

/// What a tool search shares across the process: the configured dirs and the built host `PATH`.
#[derive(Default)]
struct Search {
    /// `path` from config.toml, searched before Homebrew's prefixes.
    configured: Vec<PathBuf>,
    /// The built host `PATH`; dropped whenever `configured` changes.
    host: Option<Arc<OsString>>,
}

static SEARCH: LazyLock<Mutex<Search>> = LazyLock::new(|| Mutex::new(Search::default()));

fn search() -> MutexGuard<'static, Search> {
    SEARCH.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Search `dirs` for tools after the inherited `PATH`, before Homebrew's prefixes. A config
/// reload replaces them, and the next spawn rebuilds the search path.
pub(crate) fn set_config_dirs(dirs: &[PathBuf]) {
    let mut search = search();
    if search.configured == dirs {
        return;
    }
    search.configured = dirs.to_vec();
    search.host = None;
}

/// The host PATH: the common bins, the inherited PATH, the configured dirs, then Homebrew's.
fn host_path() -> Arc<OsString> {
    let mut search = search();
    if let Some(host) = &search.host {
        return Arc::clone(host);
    }
    let inherited = env::var_os("PATH");
    let built = Arc::new(prepended_path(
        &common_bins(),
        inherited.as_deref(),
        &search.configured,
        &env_brew_bins(),
    ));
    search.host = Some(Arc::clone(&built));
    built
}

fn common_bins() -> Vec<PathBuf> {
    COMMON_BINS.iter().map(PathBuf::from).collect()
}

/// Homebrew's Linux prefixes, as the environment names them.
fn env_brew_bins() -> Vec<PathBuf> {
    brew_bins(env::var_os("HOME").as_deref(), env::var_os("HOMEBREW_PREFIX").as_deref())
}

/// The inherited PATH's absolute entries: an empty or relative one would search the cwd.
fn inherited_dirs(inherited: Option<&OsStr>) -> Vec<PathBuf> {
    inherited.map(|p| env::split_paths(p).filter(|d| d.is_absolute()).collect()).unwrap_or_default()
}

/// Join PATH entries; none holds a separator, so this cannot fail.
fn joined(dirs: impl Iterator<Item = PathBuf>) -> OsString {
    env::join_paths(dirs).expect("entries split from a PATH join back into one")
}

/// The common bins, the inherited PATH, the configured dirs, then Homebrew's prefixes.
fn prepended_path(
    common: &[PathBuf],
    inherited: Option<&OsStr>,
    configured: &[PathBuf],
    brew: &[PathBuf],
) -> OsString {
    unique(
        common
            .iter()
            .chain(inherited_dirs(inherited).iter())
            .chain(configured.iter())
            .chain(brew.iter())
            .cloned(),
    )
}

/// The reviewer's own PATH, the common bins, the configured dirs, then Homebrew's prefixes.
fn appended_path(
    common: &[PathBuf],
    inherited: Option<&OsStr>,
    configured: &[PathBuf],
    brew: &[PathBuf],
) -> OsString {
    unique(
        inherited_dirs(inherited)
            .iter()
            .chain(common.iter())
            .chain(configured.iter())
            .chain(brew.iter())
            .cloned(),
    )
}

/// `dirs` in order, each directory once: a repeat only makes the search wider, never different.
fn unique(dirs: impl Iterator<Item = PathBuf>) -> OsString {
    let mut seen: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if !seen.contains(&dir) {
            seen.push(dir);
        }
    }
    joined(seen.into_iter())
}

/// Resolve `name` as a shell would: PATHEXT on Windows, the cwd only for a path.
fn resolve_on(path: &OsStr, name: &OsStr) -> Option<PathBuf> {
    let cwd = env::current_dir().unwrap_or_default();
    which::which_in(name, Some(path), cwd).ok()
}

/// Resolve one of reviewr's own tools: the common host bins first, then the inherited PATH,
/// then the configured dirs and Homebrew's Linux prefixes, so a stripped `PATH` still finds `gh`.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let program = program.as_ref();
    // The idle check counts these: an idle pane starts no process.
    logln!("spawn {}", program.to_string_lossy());
    let path = host_path();
    let mut cmd = resolve_on(&path, program).map_or_else(|| Command::new(program), Command::new);
    cmd.env("PATH", &*path);
    // No optional locks for any git the forge CLIs run either: a read never rewrites the index.
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

/// Resolve the reviewer's own program on their PATH first, the bins last; never in the cwd.
pub(crate) fn user_command(program: impl AsRef<OsStr>) -> Option<Command> {
    let program = program.as_ref();
    let configured = search().configured.clone();
    let path = appended_path(
        &common_bins(),
        env::var_os("PATH").as_deref(),
        &configured,
        &env_brew_bins(),
    );
    let mut cmd = Command::new(resolve_on(&path, program)?);
    cmd.env("PATH", path);
    Some(cmd)
}

/// A program's name: the base name after `/` or `\\`, a Windows extension dropped.
pub(crate) fn program_name(path: &str) -> &str {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match base.rsplit_once('.') {
        Some((stem, ext)) if ["exe", "cmd", "bat"].iter().any(|e| ext.eq_ignore_ascii_case(e)) => {
            stem
        }
        _ => base,
    }
}

/// Whether `name` resolves to an executable on the host PATH.
#[must_use]
pub fn on_path(name: &str) -> bool {
    resolve_on(&host_path(), OsStr::new(name)).is_some()
}

/// How one bounded run of a tool failed.
#[derive(Debug)]
pub(crate) enum RunError {
    /// The program is not there to run.
    NotFound,
    /// It ran and exited non-zero; `stderr` carries its diagnostic.
    Failed { stderr: String },
    /// Spawning or waiting failed at the OS level.
    Io(String),
    /// The caller's stop came first: a cancel, or a deadline.
    Stopped,
}

/// Run `cmd` in its own process tree to its stdout; once `stop` says so, the whole tree ends.
pub(crate) fn run_tree(cmd: Command, stop: impl Fn() -> bool) -> Result<String, RunError> {
    let mut cmd = CommandWrap::from(cmd);
    // No stdin: the terminal belongs to the pane.
    cmd.command_mut().stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    cmd.wrap(process_wrap::std::ProcessGroup::leader());
    #[cfg(windows)]
    cmd.wrap(process_wrap::std::JobObject);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(RunError::NotFound),
        Err(error) => return Err(RunError::Io(error.to_string())),
    };
    // Drained while polling, so a large answer cannot block the child.
    let stdout = read_all(child.stdout().take());
    let stderr = read_all(child.stderr().take());
    // Done once the tool exits and its pipes close; a lingering descendant is ended once.
    let mut ended = false;
    let status = loop {
        if stop() {
            // The readers are left to finish as the ended tree closes their pipes.
            let _ = child.start_kill();
            // The tool alone, and only unreaped: a job's own wait blocks on news a poll drained.
            if !ended {
                let _ = child.inner_mut().wait();
            }
            return Err(RunError::Stopped);
        }
        match child.try_wait() {
            Ok(Some(status)) if stdout.is_finished() && stderr.is_finished() => break status,
            Ok(Some(_)) if !ended => {
                ended = true;
                let _ = child.start_kill();
            }
            Ok(_) => thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = child.start_kill();
                let _ = child.inner_mut().wait();
                return Err(RunError::Io(error.to_string()));
            }
        }
    };
    let (stdout, stderr) = (stdout.join().unwrap_or_default(), stderr.join().unwrap_or_default());
    if status.success() {
        return Ok(String::from_utf8_lossy(&stdout).into_owned());
    }
    Err(RunError::Failed { stderr: String::from_utf8_lossy(&stderr).into_owned() })
}

/// Read `pipe` to its end on a thread of its own.
fn read_all(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

#[cfg(test)]
mod tests {
    use super::{
        COMMON_BINS, RunError, appended_path, brew_bins, prepended_path, program_name, resolve_on,
        run_tree,
    };
    use std::env;
    use std::ffi::{OsStr, OsString};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    /// An absolute directory named `name`.
    fn abs(name: &str) -> PathBuf {
        env::temp_dir().join(name)
    }

    fn path_of(dirs: &[&str]) -> OsString {
        env::join_paths(dirs.iter().map(|d| abs(d))).unwrap()
    }

    fn common() -> Vec<PathBuf> {
        COMMON_BINS.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn every_tool_runs_without_gits_optional_locks() {
        let cmd = super::command("gh");
        let locks = cmd.get_envs().find(|(k, _)| *k == "GIT_OPTIONAL_LOCKS").and_then(|(_, v)| v);
        assert_eq!(
            locks,
            Some(std::ffi::OsStr::new("0")),
            "a forge CLI's git never refreshes the index"
        );
    }

    /// The Homebrew prefix tier: Linux's shared and per-user installs, then the named one.
    #[cfg(unix)]
    #[test]
    fn the_linuxbrew_prefixes_are_searched() {
        let bins = brew_bins(Some(OsStr::new("/home/reviewer")), None);
        assert_eq!(
            bins,
            [
                PathBuf::from("/home/linuxbrew/.linuxbrew/bin"),
                PathBuf::from("/home/reviewer/.linuxbrew/bin"),
            ]
        );

        // The prefix the environment names is searched too.
        let named = brew_bins(None, Some(OsStr::new("/opt/brew")));
        assert!(named.contains(&PathBuf::from("/opt/brew/bin")), "{named:?}");
    }

    /// A Homebrew-installed tool resolves even when the pane's `PATH` never named it.
    #[cfg(unix)]
    #[test]
    fn a_homebrew_install_is_found_on_a_stripped_path() {
        let dir = tempfile::tempdir().unwrap();
        let brew_bin = dir.path().join("bin");
        std::fs::create_dir(&brew_bin).unwrap();
        // A name no real install carries, so the search reaches this dir and no other.
        let tool = brew_bin.join("reviewr-brew-probe");
        std::fs::write(&tool, []).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let inherited = path_of(&["empty-bin"]);
        let path = prepended_path(&common(), Some(&inherited), &[], &[brew_bin]);
        let found = resolve_on(&path, OsStr::new("reviewr-brew-probe")).expect("the tool resolves");
        assert!(same_file(&found, &tool), "{found:?} is not {tool:?}");
    }

    /// A tool the pane's `PATH` already provides is not displaced by a Homebrew copy.
    #[cfg(unix)]
    #[test]
    fn the_inherited_path_keeps_its_place_over_homebrew() {
        let dir = tempfile::tempdir().unwrap();
        let (own, brew) = (dir.path().join("own"), dir.path().join("brew"));
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&brew).unwrap();
        for bin in [&own, &brew] {
            let tool = bin.join("reviewr-brew-probe");
            std::fs::write(&tool, []).unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let inherited = env::join_paths([&own]).unwrap();
        let path = prepended_path(&common(), Some(&inherited), &[], &[brew]);
        let found = resolve_on(&path, OsStr::new("reviewr-brew-probe")).expect("the tool resolves");
        assert!(same_file(&found, &own.join("reviewr-brew-probe")), "{found:?} is Homebrew's");
    }

    /// Production's own search path carries the Linux Homebrew prefix, so `gh` is never hidden.
    #[cfg(unix)]
    #[test]
    fn the_host_search_path_carries_linuxbrew() {
        let bins = super::brew_bins(Some(OsStr::new("/home/reviewer")), None);
        assert!(bins.contains(&PathBuf::from("/home/linuxbrew/.linuxbrew/bin")), "{bins:?}");
        // The original macOS Homebrew dir keeps its place in the common tier.
        assert!(super::common_bins().contains(&PathBuf::from("/opt/homebrew/bin")));
    }

    #[test]
    fn prepended_path_puts_the_common_bins_in_front_of_the_inherited_path() {
        let got =
            prepended_path(&common(), Some(&path_of(&["inherited-a", "inherited-b"])), &[], &[]);
        let parts: Vec<PathBuf> = env::split_paths(&got).collect();
        let mut expected = common();
        expected.extend([abs("inherited-a"), abs("inherited-b")]);
        assert_eq!(parts, expected);
    }

    #[test]
    fn prepended_path_keeps_the_common_bins_when_nothing_is_inherited() {
        let got = prepended_path(&common(), None, &[], &[]);
        let parts: Vec<PathBuf> =
            env::split_paths(&got).filter(|p| !p.as_os_str().is_empty()).collect();
        assert_eq!(parts, common());
    }

    /// Homebrew's prefixes follow everything else, so they only ever widen the search.
    #[test]
    fn homebrews_prefixes_come_last() {
        let brew = [abs("linuxbrew-bin")];
        let got = prepended_path(&common(), Some(&path_of(&["inherited"])), &[], &brew);
        let parts: Vec<PathBuf> = env::split_paths(&got).collect();
        let mut expected = common();
        expected.extend([abs("inherited"), abs("linuxbrew-bin")]);
        assert_eq!(parts, expected);

        // One already reached by an earlier tier is not searched twice.
        let repeated =
            prepended_path(&common(), Some(&path_of(&["inherited"])), &[], &[abs("inherited")]);
        let parts: Vec<PathBuf> = env::split_paths(&repeated).collect();
        assert_eq!(parts.iter().filter(|p| *p == &abs("inherited")).count(), 1);
    }

    /// A configured dir sits between the pane's own `PATH` and Homebrew's guesses.
    #[test]
    fn configured_dirs_come_after_the_inherited_path_and_before_homebrew() {
        let (configured, brew) = ([abs("configured-bin")], [abs("linuxbrew-bin")]);
        let got = prepended_path(&common(), Some(&path_of(&["inherited"])), &configured, &brew);
        let parts: Vec<PathBuf> = env::split_paths(&got).collect();
        let mut expected = common();
        expected.extend([abs("inherited"), abs("configured-bin"), abs("linuxbrew-bin")]);
        assert_eq!(parts, expected);
    }

    /// A configured dir resolves a tool the pane's `PATH` never named.
    #[test]
    fn a_configured_dir_resolves_a_tool_on_a_stripped_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let tool = bin.join("reviewr-configured-probe");
        std::fs::write(&tool, []).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let inherited = path_of(&["empty-bin"]);
        let path = prepended_path(&common(), Some(&inherited), &[bin], &[]);
        let found =
            resolve_on(&path, OsStr::new("reviewr-configured-probe")).expect("the tool resolves");
        assert!(same_file(&found, &tool), "{found:?} is not {tool:?}");
    }

    #[test]
    fn appended_path_leaves_the_reviewers_own_entries_in_front() {
        // The reviewer's shim wins; the common bins only backstop.
        let got = appended_path(&common(), Some(&path_of(&["mise-shims", "system-bin"])), &[], &[]);
        let parts: Vec<PathBuf> = env::split_paths(&got).collect();
        let mut expected = vec![abs("mise-shims"), abs("system-bin")];
        expected.extend(common());
        assert_eq!(parts, expected);

        // An empty entry would put the reviewed repo's cwd first.
        assert_eq!(
            appended_path(&common(), Some(OsStr::new("")), &[], &[]),
            appended_path(&common(), None, &[], &[])
        );
    }

    #[test]
    fn an_empty_or_relative_path_entry_never_searches_the_cwd() {
        let inherited = env::join_paths([abs("x"), PathBuf::new(), PathBuf::from("rel"), abs("y")]);
        let got = appended_path(&common(), Some(&inherited.unwrap()), &[], &[]);
        let mut expected = vec![abs("x"), abs("y")];
        expected.extend(common());
        assert_eq!(env::split_paths(&got).collect::<Vec<_>>(), expected);
    }

    #[test]
    fn a_program_name_drops_the_directory_on_either_separator_and_a_windows_extension() {
        let rows = [
            ("target/debug/herdr-reviewr", "herdr-reviewr"),
            (r"C:\Users\me\plugin\bin\herdr-reviewr.exe", "herdr-reviewr"),
            (r"C:\plugin\bin\herdr-reviewr.EXE", "herdr-reviewr"),
            (r"C:\Program Files\Microsoft VS Code\Code.exe", "Code"),
            (r"C:\Users\me\AppData\Roaming\npm\code.CMD", "code"),
            ("C:/tools/edit.bat", "edit"),
            ("herdr-reviewr", "herdr-reviewr"),
            ("/usr/bin/herdr-reviewr-helper", "herdr-reviewr-helper"),
            ("/usr/bin/notepad++", "notepad++"),
            ("/opt/app.d/run.sh", "run.sh"),
            ("é.exe", "é"),
            ("exe", "exe"),
        ];
        for (path, want) in rows {
            assert_eq!(program_name(path), want, "{path:?}");
        }
    }

    /// An executable named `name` in `dir`, spelled the way the platform spells programs.
    #[cfg(unix)]
    fn program(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join(name);
        std::fs::write(&bin, []).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(windows)]
    fn program(dir: &Path, name: &str) -> PathBuf {
        let bin = dir.join(format!("{name}.exe"));
        std::fs::write(&bin, []).unwrap();
        bin
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap()
    }

    #[test]
    fn resolve_on_finds_a_bare_name_in_a_path_directory() {
        let dir = tempfile::tempdir().unwrap();
        let bin = program(dir.path(), "gh");
        let path = env::join_paths([dir.path()]).unwrap();
        let found = resolve_on(&path, OsStr::new("gh")).expect("gh resolves");
        assert!(same_file(&found, &bin), "{found:?} is not {bin:?}");
        assert!(resolve_on(&path, OsStr::new("missing")).is_none());
    }

    /// A program named by its path resolves as itself, PATH unread.
    #[test]
    fn a_program_named_by_its_path_resolves_as_itself() {
        let dir = tempfile::tempdir().unwrap();
        let bin = program(dir.path(), "editor");
        let elsewhere = env::join_paths([std::path::Path::new("nowhere")]).unwrap();
        let found = resolve_on(&elsewhere, bin.as_os_str()).expect("the path resolves");
        assert!(same_file(&found, &bin), "{found:?} is not {bin:?}");
        assert!(resolve_on(&elsewhere, dir.path().join("missing").as_os_str()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_file_without_the_executable_bit_is_not_a_program() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes"), []).unwrap();
        let path = env::join_paths([dir.path()]).unwrap();
        assert!(resolve_on(&path, OsStr::new("notes")).is_none());
    }

    /// A tool whose grandchild holds its pipes, and that waits on it when `waits`. A second
    /// grandchild writes `survived` three seconds in, unless the tree ends first.
    #[cfg(unix)]
    fn tool_with_grandchild(dir: &Path, waits: bool) -> Command {
        let script = if waits {
            r#"echo answer; sleep 60 & (sleep 3; touch "$2") & touch "$1"; wait"#
        } else {
            r#"echo answer; sleep 60 & touch "$1""#
        };
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script, "tool"]).arg(dir.join("ready")).arg(dir.join("survived"));
        cmd
    }

    /// The same tool as a batch file, the shape `az.cmd` has.
    #[cfg(windows)]
    fn tool_with_grandchild(dir: &Path, waits: bool) -> Command {
        let survivor = dir.join("survivor.cmd");
        std::fs::write(&survivor, "@ping -n 4 127.0.0.1 >nul\r\n@echo x> \"%~1\"\r\n").unwrap();
        let (timed, wait) = if waits {
            (
                format!("start /b \"\" \"{}\" \"%~2\"\r\n", survivor.display()),
                "ping -n 61 127.0.0.1 >nul\r\n",
            )
        } else {
            (String::new(), "")
        };
        let script = format!(
            "@echo off\r\necho answer\r\nstart /b \"\" ping -n 61 127.0.0.1 >nul\r\n{timed}\
             echo ready> \"%~1\"\r\n{wait}"
        );
        let path = dir.join("tool.cmd");
        std::fs::write(&path, script).unwrap();
        let mut cmd = Command::new(path);
        cmd.arg(dir.join("ready")).arg(dir.join("survived"));
        cmd
    }

    /// Run `cmd` on its own thread until `stop`.
    fn spawn_run(
        cmd: Command,
        stop: impl Fn() -> bool + Send + 'static,
    ) -> mpsc::Receiver<Result<String, RunError>> {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || done_tx.send(run_tree(cmd, stop)));
        done_rx
    }

    /// Wait until the tool's grandchild exists.
    fn await_ready(dir: &Path) {
        let started = Instant::now();
        while !dir.join("ready").exists() {
            assert!(started.elapsed() < Duration::from_secs(10), "never started");
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(300));
    }

    /// Whether the timed grandchild outlived its tree, asked past its three seconds.
    fn survived(dir: &Path, started: Instant) -> bool {
        thread::sleep(Duration::from_secs(5).saturating_sub(started.elapsed()));
        dir.join("survived").exists()
    }

    #[test]
    fn a_cancel_ends_the_tools_whole_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let done =
            spawn_run(tool_with_grandchild(dir.path(), true), move || flag.load(Ordering::Acquire));
        await_ready(dir.path());
        cancelled.store(true, Ordering::Release);
        let result = done.recv_timeout(Duration::from_secs(5)).expect("the cancel ends the run");
        assert!(matches!(result, Err(RunError::Stopped)), "{result:?}");
        assert!(!survived(dir.path(), started), "a grandchild outlived the cancel");
    }

    #[test]
    fn the_deadline_ends_a_tool_that_never_exits() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_secs(1);
        let cmd = tool_with_grandchild(dir.path(), true);
        let done = spawn_run(cmd, move || Instant::now() >= deadline);
        let result = done.recv_timeout(Duration::from_secs(6)).expect("the deadline ends the run");
        assert!(matches!(result, Err(RunError::Stopped)), "{result:?}");
        assert!(!survived(dir.path(), started), "a grandchild outlived the deadline");
    }

    #[test]
    fn a_tool_that_exits_is_done_though_a_grandchild_holds_its_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = tool_with_grandchild(dir.path(), false);
        // Never stopped: the tool's own exit has to end the run.
        let done = spawn_run(cmd, || false);
        let result = done.recv_timeout(Duration::from_secs(10)).expect("the tool's exit ends it");
        assert_eq!(result.map(|out| out.trim().to_string()).ok().as_deref(), Some("answer"));
    }

    /// A bare name reaches a batch shim through PATHEXT.
    #[cfg(windows)]
    #[test]
    fn a_batch_shim_resolves_through_pathext() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("az.cmd");
        std::fs::write(&shim, "@echo off\r\n").unwrap();
        let path = env::join_paths([dir.path()]).unwrap();
        let found = resolve_on(&path, OsStr::new("az")).expect("az resolves");
        assert!(same_file(&found, &shim), "{found:?} is not {shim:?}");
    }
}
