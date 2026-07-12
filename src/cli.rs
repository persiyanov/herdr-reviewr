//! `herdr-reviewr comment …` and `herdr-reviewr skill-path` — the agent-facing CLI.
//!
//! The TUI (`crate::run`) is the reviewer's half of the shared comment store
//! (`crate::comments`); this module is the agent's half, invoked as a short-lived
//! subprocess rather than a long-running terminal app. Arg parsing is a hand-rolled loop
//! (no dependency) matching the contract in `specs/*.md`: an unknown flag or a flag missing
//! its value is a usage error (exit 2), a store failure (not a git repo, an unknown id) is
//! exit 1 with a single stderr line naming the problem, and success is exit 0 with the
//! command's stdout (an id for `add`, rows or JSON for `list`, nothing otherwise).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::comments::{Author, Status, Store, StoredComment};
use crate::model::{Comment, Side};

const USAGE: &str = "usage: herdr-reviewr comment add --file <path> --start <n> [--end <n>] [--side new|old] [--lines <snippet>] [--author user|agent] --text <text>\n       herdr-reviewr comment list [--json] [--all]\n       herdr-reviewr comment resolve <id>\n       herdr-reviewr comment rm <id>\n       herdr-reviewr skill-path\n       herdr-reviewr skill-install [--target <dir> | --project] [--copy] [--force]\n";

/// Entry point called from `main` with the full process argv (`args[0]` is the program
/// name, `args[1]` the subcommand). Only reached when `main` has already confirmed
/// `args[1]` is `"comment"` or `"skill-path"`. Takes the `Vec` by value (the planned
/// interface): `main` has no further use for argv, so ownership moves here.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Vec<String>) -> ExitCode {
    match args.get(1).map(String::as_str) {
        Some("comment") => comment(&args[2..]),
        Some("skill-path") => skill_path(),
        Some("skill-install") => skill_install(&args[2..]),
        _ => usage_error(),
    }
}

fn usage_error() -> ExitCode {
    eprint!("{USAGE}");
    ExitCode::from(2)
}

fn comment(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("add") => comment_add(&args[1..]),
        Some("list") => comment_list(&args[1..]),
        Some("resolve") => comment_resolve(&args[1..]),
        Some("rm") => comment_rm(&args[1..]),
        _ => usage_error(),
    }
}

/// Open the store for the current directory's repo, printing a single stderr line and
/// returning the exit-1 code on any failure to resolve a git dir.
fn open_store() -> Result<Store, ExitCode> {
    let cwd = std::env::current_dir().map_err(|e| {
        eprintln!("reviewr: cannot read current directory: {e}");
        ExitCode::from(1)
    })?;
    Store::open(&cwd).map_err(|error| {
        eprintln!("reviewr: {error}");
        ExitCode::from(1)
    })
}

fn parse_author(s: &str) -> Option<Author> {
    match s {
        "user" => Some(Author::User),
        "agent" => Some(Author::Agent),
        _ => None,
    }
}

fn parse_side(s: &str) -> Option<Side> {
    match s {
        "new" => Some(Side::New),
        "old" => Some(Side::Old),
        _ => None,
    }
}

fn comment_add(args: &[String]) -> ExitCode {
    let mut file: Option<String> = None;
    let mut start: Option<u32> = None;
    let mut end: Option<u32> = None;
    let mut side = Side::New;
    let mut lines = String::new();
    let mut author = Author::Agent;
    let mut text: Option<String> = None;

    let mut it = args.iter();
    while let Some(flag) = it.next() {
        macro_rules! value {
            () => {
                match it.next() {
                    Some(v) => v,
                    None => return usage_error(),
                }
            };
        }
        match flag.as_str() {
            "--file" => file = Some(value!().clone()),
            "--start" => {
                let Ok(n) = value!().parse() else { return usage_error() };
                start = Some(n);
            }
            "--end" => {
                let Ok(n) = value!().parse() else { return usage_error() };
                end = Some(n);
            }
            "--side" => {
                let Some(s) = parse_side(value!()) else { return usage_error() };
                side = s;
            }
            "--lines" => lines.clone_from(value!()),
            "--author" => {
                let Some(a) = parse_author(value!()) else { return usage_error() };
                author = a;
            }
            "--text" => text = Some(value!().clone()),
            _ => return usage_error(),
        }
    }

    let (Some(file), Some(start), Some(text)) = (file, start, text) else { return usage_error() };
    let end = end.unwrap_or(start);

    if start == 0 {
        eprintln!("reviewr: --start must be >= 1");
        return ExitCode::from(2);
    }
    if end < start {
        eprintln!("reviewr: --end must be >= --start");
        return ExitCode::from(2);
    }

    let store = match open_store() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let comment = Comment { file, side, start, end, lines, text, diff_anchored: true };
    match store.add(author, &comment) {
        Ok(stored) => {
            println!("{}", stored.id);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("reviewr: {error}");
            ExitCode::from(1)
        }
    }
}

fn comment_list(args: &[String]) -> ExitCode {
    let mut json = false;
    let mut all = false;
    for flag in args {
        match flag.as_str() {
            "--json" => json = true,
            "--all" => all = true,
            _ => return usage_error(),
        }
    }

    let store = match open_store() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let mut rows: Vec<StoredComment> = store.load();
    if !all {
        rows.retain(|sc| sc.status == Status::Open);
    }

    if json {
        let docs: Vec<_> = rows.iter().map(stored_to_json).collect();
        println!("{}", serde_json::Value::Array(docs));
    } else {
        for sc in &rows {
            let first_line = sc.comment.text.lines().next().unwrap_or("");
            println!(
                "{}  {}  {}  {}:{}-{}  {}",
                sc.id,
                status_str(sc.status),
                author_str(sc.author),
                sc.comment.file,
                sc.comment.start,
                sc.comment.end,
                first_line,
            );
        }
    }
    ExitCode::SUCCESS
}

fn comment_resolve(args: &[String]) -> ExitCode {
    let [id] = args else { return usage_error() };
    let store = match open_store() {
        Ok(s) => s,
        Err(code) => return code,
    };
    match store.set_status(id, Status::Resolved) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("reviewr: no such comment: {id}");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("reviewr: {error}");
            ExitCode::from(1)
        }
    }
}

fn comment_rm(args: &[String]) -> ExitCode {
    let [id] = args else { return usage_error() };
    let store = match open_store() {
        Ok(s) => s,
        Err(code) => return code,
    };
    match store.remove(id) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("reviewr: no such comment: {id}");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("reviewr: {error}");
            ExitCode::from(1)
        }
    }
}

fn status_str(status: Status) -> &'static str {
    match status {
        Status::Open => "open",
        Status::Resolved => "resolved",
    }
}

fn author_str(author: Author) -> &'static str {
    match author {
        Author::User => "user",
        Author::Agent => "agent",
    }
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::New => "new",
        Side::Old => "old",
    }
}

/// `StoredComment` → the same document shape `comments::Store` persists, for `list --json`.
fn stored_to_json(sc: &StoredComment) -> serde_json::Value {
    serde_json::json!({
        "id": sc.id,
        "author": author_str(sc.author),
        "status": status_str(sc.status),
        "created_at": sc.created_at,
        "file": sc.comment.file,
        "side": side_str(sc.comment.side),
        "start": sc.comment.start,
        "end": sc.comment.end,
        "lines": sc.comment.lines,
        "text": sc.comment.text,
    })
}

/// `<plugin-root>/skills/reviewr-comments/SKILL.md`, where `plugin-root` is the running
/// executable's directory's parent (`bin/..`). Falls back to the cwd-relative dev-checkout
/// path when the installed layout isn't found (running `cargo run`/`cargo test` from a
/// checkout rather than the packaged plugin). Returns the message (without the `reviewr: `
/// prefix) naming both candidates when neither exists; shared by `skill-path` and
/// `skill-install` so both report source resolution identically.
fn resolve_skill_source() -> Result<PathBuf, String> {
    const REL: &str = "skills/reviewr-comments/SKILL.md";
    let installed = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|bin| bin.parent().map(Path::to_path_buf))
        .map(|plugin_root| plugin_root.join(REL));
    if let Some(path) = &installed
        && path.exists()
    {
        return Ok(path.clone());
    }

    let dev_checkout = PathBuf::from(REL);
    if dev_checkout.exists() {
        return Ok(dev_checkout);
    }

    let installed_display =
        installed.as_ref().map_or_else(|| REL.to_string(), |p| p.display().to_string());
    Err(format!("SKILL.md not found at {installed_display} or {}", dev_checkout.display()))
}

fn skill_path() -> ExitCode {
    match resolve_skill_source() {
        Ok(path) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("reviewr: {message}");
            ExitCode::from(1)
        }
    }
}

/// `$HOME/.claude/skills/reviewr-comments` (`%USERPROFILE%` on Windows), the default
/// `skill-install` target when `--target` isn't given. `None` when neither environment
/// variable is set.
fn default_skill_install_target() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| Path::new(&home).join(".claude").join("skills").join("reviewr-comments"))
}

/// The stdout hint printed after a fresh install (or a `--force` replace), reminding the user
/// that a proactive agent needs the reminder in `CLAUDE.md` too — the skill list alone only
/// covers the "user asks for it" path.
fn print_installed(dest: &Path) {
    println!("installed: {}", dest.display());
    println!("To make agents check comments proactively, add to your CLAUDE.md:");
    println!("  Reviews happen in the herdr-reviewr sidebar — when starting work or when review");
    println!(
        "  feedback is mentioned, run `herdr-reviewr comment list` and address open comments."
    );
}

/// True when `a` and `b` refer to the same file, comparing canonicalized paths where possible
/// and falling back to a literal comparison (e.g. a dangling symlink target) otherwise.
fn paths_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Installs the bundled `SKILL.md` (resolved exactly as `skill-path` does) into `dest`,
/// symlinking on Unix and falling back to a copy — with a one-line stderr note — on Windows or
/// when symlink creation fails for any reason (typically privileges).
fn install_skill(source: &Path, dest: &Path, copy: bool) -> ExitCode {
    if !copy {
        #[cfg(unix)]
        {
            if std::os::unix::fs::symlink(source, dest).is_ok() {
                print_installed(dest);
                return ExitCode::SUCCESS;
            }
        }
        eprintln!("reviewr: symlink unavailable — copied; re-run after plugin updates");
    }
    match std::fs::copy(source, dest) {
        Ok(_) => {
            print_installed(dest);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("reviewr: cannot install to {}: {error}", dest.display());
            ExitCode::from(1)
        }
    }
}

/// Installs the bundled skill into a Claude Code (or compatible) skills directory so
/// `address my review comments` works with no per-session reminder. See the module doc and
/// `README.md`'s "Working with agents" section for the full contract.
fn skill_install(args: &[String]) -> ExitCode {
    let mut target: Option<PathBuf> = None;
    let mut project = false;
    let mut copy = false;
    let mut force = false;

    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--target" => {
                let Some(value) = it.next() else { return usage_error() };
                target = Some(PathBuf::from(value));
            }
            "--project" => project = true,
            "--copy" => copy = true,
            "--force" => force = true,
            _ => return usage_error(),
        }
    }

    if project && target.is_some() {
        return usage_error();
    }
    if project {
        let cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(error) => {
                eprintln!("reviewr: cannot read current directory: {error}");
                return ExitCode::from(1);
            }
        };
        target = Some(cwd.join(".agents").join("skills").join("reviewr-comments"));
    }

    let Some(target_dir) = target.or_else(default_skill_install_target) else {
        eprintln!(
            "reviewr: cannot determine home directory (set HOME or USERPROFILE, or pass --target)"
        );
        return ExitCode::from(1);
    };

    let source = match resolve_skill_source() {
        Ok(path) => path,
        Err(message) => {
            eprintln!("reviewr: {message}");
            return ExitCode::from(1);
        }
    };
    let canonical_source = std::fs::canonicalize(&source).unwrap_or(source);

    if let Err(error) = std::fs::create_dir_all(&target_dir) {
        eprintln!("reviewr: cannot create {}: {error}", target_dir.display());
        return ExitCode::from(1);
    }
    let dest = target_dir.join("SKILL.md");

    if let Ok(meta) = std::fs::symlink_metadata(&dest) {
        let already_installed = if meta.file_type().is_symlink() {
            std::fs::read_link(&dest).is_ok_and(|link| {
                let resolved = if link.is_absolute() { link } else { target_dir.join(link) };
                paths_equal(&resolved, &canonical_source)
            })
        } else {
            matches!(
                (std::fs::read(&dest), std::fs::read(&canonical_source)),
                (Ok(existing), Ok(wanted)) if existing == wanted
            )
        };
        if already_installed {
            println!("already installed at {}", dest.display());
            return ExitCode::SUCCESS;
        }
        if !force {
            eprintln!(
                "reviewr: {} already exists and differs from the bundled skill; re-run with --force to replace",
                dest.display()
            );
            return ExitCode::from(1);
        }
        if let Err(error) = std::fs::remove_file(&dest) {
            eprintln!("reviewr: cannot remove existing {}: {error}", dest.display());
            return ExitCode::from(1);
        }
    }

    install_skill(&canonical_source, &dest, copy)
}
