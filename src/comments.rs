//! Persistent per-repo comment store: `<git-dir>/reviewr/comments/<id>.json`.
//!
//! One file per [`StoredComment`], written by hand as `serde_json::Value` (the crate never
//! derives `Serialize`/`Deserialize`) so an unknown field written by a newer version survives
//! a rewrite from an older one (`set_status`). Reads never delete a file they can't parse —
//! corruption is surfaced once via [`crate::log`] and left for a human to inspect. This is the
//! foundation the CLI subcommands (agent side) and the TUI sync loop (reviewer side) share:
//! both read and write through this same on-disk format, so neither can silently diverge from
//! the other's view of a comment's lifecycle.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::logln;
use crate::model::{Comment, Side};

/// Who left a comment — the CLI (an agent) or the TUI (a human reviewer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author {
    User,
    Agent,
}

impl Author {
    fn as_str(self) -> &'static str {
        match self {
            Author::User => "user",
            Author::Agent => "agent",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Author::User),
            "agent" => Some(Author::Agent),
            _ => None,
        }
    }
}

/// A comment's place in its lifecycle: still awaiting action, or resolved by either side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Open,
    Resolved,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Resolved => "resolved",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Status::Open),
            "resolved" => Some(Status::Resolved),
            _ => None,
        }
    }
}

/// One persisted comment: lifecycle metadata plus the review-model comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredComment {
    pub id: String,
    pub author: Author,
    pub status: Status,
    pub created_at: String,
    /// `diff_anchored` is not serialized — every stored comment is diff-anchored by
    /// construction (`specs/review-model.md`), so [`Store::load`] sets it `true` rather than
    /// spending a JSON field on a constant.
    pub comment: Comment,
}

/// A store operation failed: the message is already formatted for a status line or log line.
#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

/// A directory of one-file-per-comment JSON, keyed by [`StoredComment::id`].
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Resolve `<git-dir>/reviewr/comments` from `repo` via `git rev-parse --git-dir`
    /// (relative output is joined onto `repo`). Does not create the directory — the first
    /// [`Store::add`] or [`Store::put`] does that.
    pub fn open(repo: &Path) -> Result<Self, StoreError> {
        let raw = git_dir_line(repo).ok_or_else(|| {
            StoreError(format!("git rev-parse --git-dir failed in {}", repo.display()))
        })?;
        let git_dir = PathBuf::from(raw);
        let git_dir = if git_dir.is_absolute() { git_dir } else { repo.join(git_dir) };
        Ok(Self { dir: git_dir.join("reviewr").join("comments") })
    }

    /// For tests and the TUI (which already knows the git dir): point at an explicit dir.
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// All parseable comments, sorted by id (= creation order). Corrupt files are skipped
    /// with a `crate::log` line, never deleted. Missing dir → empty vec.
    pub fn load(&self) -> Vec<StoredComment> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                continue;
            }
            let parsed = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|v| from_value(&v));
            match parsed {
                Some(mut sc) => {
                    sc.comment.diff_anchored = true;
                    out.push(sc);
                }
                None => logln!("comments: skipping corrupt file {}", path.display()),
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Persist a new comment (creates the dir on first write). Returns the stored form.
    pub fn add(&self, author: Author, comment: &Comment) -> Result<StoredComment, StoreError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| StoreError(format!("creating {}: {e}", self.dir.display())))?;
        let mut sc = StoredComment {
            id: new_id(),
            author,
            status: Status::Open,
            created_at: now_iso(),
            comment: comment.clone(),
        };
        match self.write_new(&sc) {
            Ok(()) => Ok(sc),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Two adds landed on the same id (same millisecond, same nanos-derived hex,
                // vanishingly unlikely but not impossible) — one fresh id, one retry.
                sc.id = new_id();
                self.write_new(&sc)
                    .map_err(|e| StoreError(format!("writing comment {}: {e}", sc.id)))?;
                Ok(sc)
            }
            Err(e) => Err(StoreError(format!("writing comment {}: {e}", sc.id))),
        }
    }

    /// Write `sc` to `<id>.json.tmp` via a create-new open — an existing tmp file surfaces as
    /// an id collision to the caller — then rename onto `<id>.json`.
    fn write_new(&self, sc: &StoredComment) -> std::io::Result<()> {
        let tmp = self.dir.join(format!("{}.json.tmp", sc.id));
        {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(to_value(sc).to_string().as_bytes())?;
            file.flush()?;
        }
        std::fs::rename(&tmp, self.dir.join(format!("{}.json", sc.id)))
    }

    /// Persist an already-formed `StoredComment` under its own id (TUI sync path).
    pub fn put(&self, sc: &StoredComment) -> Result<(), StoreError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| StoreError(format!("creating {}: {e}", self.dir.display())))?;
        let path = self.dir.join(format!("{}.json", sc.id));
        write_atomic(&path, &to_value(sc))
            .map_err(|e| StoreError(format!("writing {}: {e}", path.display())))
    }

    /// Flip status, preserving unknown fields (read as `Value`, set `"status"`, tmp+rename).
    /// `Ok(false)` when the id has no file.
    pub fn set_status(&self, id: &str, status: Status) -> Result<bool, StoreError> {
        let path = self.dir.join(format!("{id}.json"));
        let Ok(text) = std::fs::read_to_string(&path) else { return Ok(false) };
        let mut v: Value = serde_json::from_str(&text)
            .map_err(|e| StoreError(format!("parsing {}: {e}", path.display())))?;
        v["status"] = Value::String(status.as_str().to_string());
        write_atomic(&path, &v)
            .map_err(|e| StoreError(format!("writing {}: {e}", path.display())))?;
        Ok(true)
    }

    /// `Ok(false)` when the id has no file.
    pub fn remove(&self, id: &str) -> Result<bool, StoreError> {
        let path = self.dir.join(format!("{id}.json"));
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StoreError(format!("removing {}: {e}", path.display()))),
        }
    }

    /// Cheap change signature: hash of (entry name, mtime) pairs; `None`/0 for missing dir.
    /// The event loop compares signatures across ticks to detect external edits.
    pub fn signature(&self) -> u64 {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return 0 };
        let mut items: Vec<(String, u128)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            items.push((name, mtime));
        }
        items.sort();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis (matches `git::worktree_key`)
        let mut feed = |bytes: &[u8]| {
            for &b in bytes {
                hash ^= u64::from(b);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for (name, mtime) in items {
            feed(name.as_bytes());
            feed(&mtime.to_le_bytes());
        }
        hash
    }
}

/// `git -C <repo> rev-parse --git-dir`'s trimmed stdout, or `None` on any failure. A local
/// 6-line runner rather than reusing `crate::git`'s private helpers, which aren't `pub`.
fn git_dir_line(repo: &Path) -> Option<String> {
    let out =
        Command::new("git").arg("-C").arg(repo).args(["rev-parse", "--git-dir"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// Write `value` to `path` crash-safely: a full write to `<path>.tmp`, flushed, then an atomic
/// rename onto `path` (overwriting any prior content in one filesystem operation).
fn write_atomic(path: &Path, value: &Value) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        file.write_all(value.to_string().as_bytes())?;
        file.flush()?;
    }
    std::fs::rename(&tmp, path)
}

/// `c-<epoch-ms>-<4 lowercase hex>`; hex from the nanos remainder so two adds in the same
/// millisecond differ. Uniqueness is per-process-adequate; `add` retries once with a fresh id
/// on `create_new` collision.
pub fn new_id() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let ms = now.as_millis();
    let hex = now.subsec_nanos() & 0xffff;
    format!("c-{ms}-{hex:04x}")
}

/// `SystemTime` → `"YYYY-MM-DDTHH:MM:SSZ"`, computed by hand (no time-formatting dependency).
pub fn now_iso() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = secs / 86400;
    let rem = secs % 86400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(0));
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Days-since-1970-01-01 to a proleptic Gregorian `(year, month, day)`, Howard Hinnant's
/// widely-used `civil_from_days` algorithm — exact for every day this store will ever see, with
/// no floating point and no calendar dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let mp = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// Whether `id` has the shape [`new_id`] produces: a `c-` prefix over `[a-z0-9-]` only. `load`
/// and `from_value` reject anything else — both `resolve`/`rm` (CLI) and `set_status`/`remove`
/// (store) join the id straight into a filename, so a hostile id like `../../../evil` must never
/// survive parsing far enough to be joined into a path.
fn valid_id(id: &str) -> bool {
    id.starts_with("c-")
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::New => "new",
        Side::Old => "old",
    }
}

fn side_parse(s: &str) -> Option<Side> {
    match s {
        "new" => Some(Side::New),
        "old" => Some(Side::Old),
        _ => None,
    }
}

/// `StoredComment` → hand-built JSON. `diff_anchored` is deliberately not a field — see the
/// doc comment on [`StoredComment::comment`].
fn to_value(sc: &StoredComment) -> Value {
    serde_json::json!({
        "id": sc.id,
        "author": sc.author.as_str(),
        "status": sc.status.as_str(),
        "created_at": sc.created_at,
        "file": sc.comment.file,
        "side": side_str(sc.comment.side),
        "start": sc.comment.start,
        "end": sc.comment.end,
        "lines": sc.comment.lines,
        "text": sc.comment.text,
    })
}

/// JSON `Value` → `StoredComment`. `None` on any missing or mistyped field — the caller treats
/// that as a corrupt file (skip, log, never delete). `comment.diff_anchored` always comes back
/// `false` here; [`Store::load`] is the one place that sets it `true`.
fn from_value(v: &Value) -> Option<StoredComment> {
    let id = v.get("id")?.as_str()?.to_string();
    if !valid_id(&id) {
        return None;
    }
    let author = Author::parse(v.get("author")?.as_str()?)?;
    let status = Status::parse(v.get("status")?.as_str()?)?;
    let created_at = v.get("created_at")?.as_str()?.to_string();
    let file = v.get("file")?.as_str()?.to_string();
    let side = side_parse(v.get("side")?.as_str()?)?;
    let start = u32::try_from(v.get("start")?.as_u64()?).ok()?;
    let end = u32::try_from(v.get("end")?.as_u64()?).ok()?;
    let lines = v.get("lines")?.as_str()?.to_string();
    let text = v.get("text")?.as_str()?.to_string();
    Some(StoredComment {
        id,
        author,
        status,
        created_at,
        comment: Comment { file, side, start, end, lines, text, diff_anchored: false },
    })
}

#[cfg(test)]
mod tests {
    use super::{Author, Status, Store, from_value, to_value};

    #[test]
    fn round_trips_a_comment_through_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("comments"));
        let stored = store.add(Author::User, &sample_comment()).unwrap();
        assert!(stored.id.starts_with("c-"));
        let loaded = store.load();
        assert_eq!(loaded, vec![stored]);
        assert!(loaded[0].comment.diff_anchored, "loader marks store comments diff-anchored");
    }

    #[test]
    fn set_status_preserves_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("comments"));
        let stored = store.add(Author::Agent, &sample_comment()).unwrap();
        // Simulate a future writer adding a field.
        let path = dir.path().join("comments").join(format!("{}.json", stored.id));
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v["future_field"] = serde_json::json!(42);
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();

        assert!(store.set_status(&stored.id, Status::Resolved).unwrap());
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["status"], "resolved");
        assert_eq!(raw["future_field"], 42, "unknown fields survive a rewrite");
    }

    #[test]
    fn corrupt_files_are_skipped_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("comments"));
        store.add(Author::User, &sample_comment()).unwrap();
        let bad = dir.path().join("comments").join("c-0-dead.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert_eq!(store.load().len(), 1);
        assert!(bad.exists(), "corrupt file is never deleted");
    }

    #[test]
    fn signature_changes_on_add_and_remove_and_missing_ops_return_false() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("comments"));
        let empty = store.signature();
        let stored = store.add(Author::Agent, &sample_comment()).unwrap();
        assert_ne!(store.signature(), empty);
        assert!(store.remove(&stored.id).unwrap());
        assert!(!store.remove(&stored.id).unwrap());
        assert!(!store.set_status("c-0-beef", Status::Resolved).unwrap());
    }

    #[test]
    fn to_value_and_from_value_round_trip_and_reject_bad_json() {
        let sc = super::StoredComment {
            id: "c-1-aaaa".into(),
            author: Author::User,
            status: Status::Open,
            created_at: super::now_iso(),
            comment: sample_comment(),
        };
        let v = to_value(&sc);
        assert_eq!(v["side"], "new");
        let back = from_value(&v).unwrap();
        assert_eq!(back.id, sc.id);
        assert_eq!(back.comment.file, sc.comment.file);
        assert!(!back.comment.diff_anchored, "from_value never sets diff_anchored; load does");

        assert!(from_value(&serde_json::json!({"id": "c-1-aaaa"})).is_none());
    }

    #[test]
    fn load_skips_a_file_with_a_path_traversal_id_instead_of_deleting_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("comments"));
        std::fs::create_dir_all(dir.path().join("comments")).unwrap();
        let mut v = to_value(&super::StoredComment {
            id: "c-1-aaaa".into(),
            author: Author::User,
            status: Status::Open,
            created_at: super::now_iso(),
            comment: sample_comment(),
        });
        v["id"] = serde_json::json!("../../../evil");
        let bad = dir.path().join("comments").join("evil.json");
        std::fs::write(&bad, v.to_string()).unwrap();

        assert!(store.load().is_empty(), "a store file whose id fails validation is skipped");
        assert!(bad.exists(), "the file is never deleted, only skipped");
    }

    fn sample_comment() -> crate::model::Comment {
        crate::model::Comment {
            file: "src/a.rs".into(),
            side: crate::model::Side::New,
            start: 3,
            end: 4,
            lines: "+let x = 1;".into(),
            text: "why not a const?".into(),
            diff_anchored: true,
        }
    }
}
