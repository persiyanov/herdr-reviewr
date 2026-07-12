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

const USAGE: &str = "usage: herdr-reviewr comment add --file <path> --start <n> [--end <n>] [--side new|old] [--lines <snippet>] [--author user|agent] --text <text>\n       herdr-reviewr comment list [--json] [--all]\n       herdr-reviewr comment resolve <id>\n       herdr-reviewr comment rm <id>\n       herdr-reviewr skill-path\n";

/// Entry point called from `main` with the full process argv (`args[0]` is the program
/// name, `args[1]` the subcommand). Only reached when `main` has already confirmed
/// `args[1]` is `"comment"` or `"skill-path"`. Takes the `Vec` by value (the planned
/// interface): `main` has no further use for argv, so ownership moves here.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Vec<String>) -> ExitCode {
    match args.get(1).map(String::as_str) {
        Some("comment") => comment(&args[2..]),
        Some("skill-path") => skill_path(),
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
/// checkout rather than the packaged plugin); exits 1 naming both candidates when neither
/// exists.
fn skill_path() -> ExitCode {
    const REL: &str = "skills/reviewr-comments/SKILL.md";
    let installed = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|bin| bin.parent().map(Path::to_path_buf))
        .map(|plugin_root| plugin_root.join(REL));
    if let Some(path) = &installed
        && path.exists()
    {
        println!("{}", path.display());
        return ExitCode::SUCCESS;
    }

    let dev_checkout = PathBuf::from(REL);
    if dev_checkout.exists() {
        println!("{}", dev_checkout.display());
        return ExitCode::SUCCESS;
    }

    let installed_display =
        installed.as_ref().map_or_else(|| REL.to_string(), |p| p.display().to_string());
    eprintln!("reviewr: SKILL.md not found at {installed_display} or {}", dev_checkout.display());
    ExitCode::from(1)
}
