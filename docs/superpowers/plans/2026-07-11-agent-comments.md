# Bidirectional Agent/User Comments Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A persistent, worktree-scoped comment store both the reviewer (TUI) and the coding agent (CLI subcommands on the same binary) read and write, with resolve lifecycle — the hunk-style loop.

**Architecture:** New `src/comments.rs` owns the store (one JSON file per comment under `<git-dir>/reviewr/comments/`); new `src/cli.rs` exposes `comment add|list|resolve|rm` and `skill-path` subcommands dispatched from `main.rs`; the TUI's in-memory `CommentStore` entries gain lifecycle metadata and sync with the disk store per a new `comment_sync` config key; the event loop re-reads the store when its directory signature changes. A bundled SKILL.md teaches the agent the loop.

**Tech Stack:** Rust 2024, serde_json, std only — **no new crate dependencies** (hand-rolled arg parsing, no clap).

## Global Constraints

- **Spec:** `docs/superpowers/specs/2026-07-11-agent-comments-design.md` — read it first; its Store/CLI/TUI sections are the contract.
- No daemon, no sockets, no new `[dependencies]`. Store writes only under the git dir, never the worktree.
- Comment document shape (spec §Store): `{id, author, status, created_at, file, side, start, end, lines, text}`; `id` = `c-<epoch-ms>-<4 hex>`; `author` ∈ `user|agent`; `status` ∈ `open|resolved`; `created_at` ISO-8601 `…Z`. Unknown fields preserved on rewrite; corrupt files skipped (logged), never deleted.
- Clippy pedantic `-D warnings` clean; `cargo test --all-features` fully green; run `cargo fmt --all` before every commit and commit any content changes to files you touched (this Windows checkout shows CRLF noise — content diffs are real, eol warnings are not; check `git diff --stat` after fmt).
- Dense contract-style doc comments matching the codebase voice.
- Branch: `agent-comments`. Commit messages end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- TUI serialization detail: `model::Comment.diff_anchored` is TUI-internal — it is NOT part of the store document (spec table). Store round-trip sets it per context (see Task 4).

---

### Task 1: The comment store (`src/comments.rs`)

**Files:**
- Create: `src/comments.rs`
- Modify: `src/lib.rs` (add `pub mod comments;`)

**Interfaces (produced — Tasks 2 and 4 consume these exact names):**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author { User, Agent }           // serializes "user"/"agent"
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status { Open, Resolved }        // serializes "open"/"resolved"

/// One persisted comment: lifecycle metadata plus the review-model comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredComment {
    pub id: String,
    pub author: Author,
    pub status: Status,
    pub created_at: String,
    pub comment: crate::model::Comment,   // diff_anchored is NOT serialized; loader sets it true
}

pub struct Store { dir: std::path::PathBuf }

#[derive(Debug)]
pub struct StoreError(pub String);

impl Store {
    /// Resolve `<git-dir>/reviewr/comments` from `repo` via `git rev-parse --git-dir`
    /// (relative output is joined onto `repo`). Does not create the directory.
    pub fn open(repo: &std::path::Path) -> Result<Self, StoreError>;
    /// For tests and the TUI (which already knows the git dir): point at an explicit dir.
    pub fn at(dir: std::path::PathBuf) -> Self;
    /// All parseable comments, sorted by id (= creation order). Corrupt files are
    /// skipped with a crate::log line, never deleted. Missing dir → empty vec.
    pub fn load(&self) -> Vec<StoredComment>;
    /// Persist a new comment (creates the dir on first write). Returns the stored form.
    pub fn add(&self, author: Author, comment: &crate::model::Comment) -> Result<StoredComment, StoreError>;
    /// Persist an already-formed StoredComment under its own id (TUI sync path).
    pub fn put(&self, sc: &StoredComment) -> Result<(), StoreError>;
    /// Flip status, preserving unknown fields (read as Value, set "status", tmp+rename).
    /// Ok(false) when the id has no file.
    pub fn set_status(&self, id: &str, status: Status) -> Result<bool, StoreError>;
    /// Ok(false) when the id has no file.
    pub fn remove(&self, id: &str) -> Result<bool, StoreError>;
    /// Cheap change signature: hash of (entry name, mtime) pairs; None/0 for missing dir.
    /// The event loop compares signatures across ticks to detect external edits.
    pub fn signature(&self) -> u64;
}

/// `c-<epoch-ms>-<4 lowercase hex>`; hex from the nanos remainder so two adds in the
/// same millisecond differ. Uniqueness is per-process-adequate; `add` retries once with
/// a fresh id on create_new collision.
pub fn new_id() -> String;
pub fn now_iso() -> String;               // SystemTime → "YYYY-MM-DDTHH:MM:SSZ"
```

Serialization is by hand via `serde_json::Value` (the codebase does not derive Serialize anywhere — match that): `to_value(sc) -> Value` and `from_value(&Value) -> Option<StoredComment>` as private helpers, both unit-tested. `side` serializes `"new"`/`"old"` per `specs/review-model.md`. Writes go to `<id>.json.tmp` then rename; `add` uses `OpenOptions::new().write(true).create_new(true)`.

- [ ] **Step 1: Write failing unit tests** in `src/comments.rs` `#[cfg(test)]` (tempfile is already a dev-dependency):

```rust
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

    fn sample_comment() -> crate::model::Comment {
        crate::model::Comment {
            file: "src/a.rs".into(), side: crate::model::Side::New,
            start: 3, end: 4, lines: "+let x = 1;".into(),
            text: "why not a const?".into(), diff_anchored: true,
        }
    }
```

- [ ] **Step 2: Run** `cargo test --lib comments` — Expected: FAIL (module missing → compile error; add skeleton with `todo!()` to reach red assertions if you prefer strict RED).
- [ ] **Step 3: Implement** per the interface block above. `Store::open` shells `git rev-parse --git-dir` with `std::process::Command` (pattern: `crate::git`'s helpers — reuse `crate::git::git_line` if visible, else a local 6-line runner).
- [ ] **Step 4: Run** `cargo test --all-features` + `cargo clippy --all-targets --all-features -- -D warnings` — Expected: PASS/clean.
- [ ] **Step 5:** `cargo fmt --all`, commit: `feat(comments): per-file persistent comment store`

---

### Task 2: CLI subcommands and the bundled skill

**Files:**
- Create: `src/cli.rs`, `skills/reviewr-comments/SKILL.md`
- Modify: `src/main.rs` (dispatch), `src/lib.rs` (`pub mod cli;`)
- Test: `tests/comments_cli.rs`

**Interfaces:**
- Consumes: `comments::{Store, Author, Status, StoredComment}` (Task 1).
- Produces: `cli::run(args: Vec<String>) -> std::process::ExitCode` — handles `comment …` and `skill-path`; `main.rs` calls it when `args[1]` is one of those, otherwise existing behavior (`--resolve-plugin-config`, then TUI) unchanged.

**CLI contract (spec §CLI, verbatim):**

```
herdr-reviewr comment add --file <path> --start <n> [--end <n>] [--side new|old]
                          [--lines <snippet>] [--author user|agent] --text <text>
herdr-reviewr comment list [--json] [--all]     # default: open only, human-readable
herdr-reviewr comment resolve <id>
herdr-reviewr comment rm <id>
herdr-reviewr skill-path
```

Defaults: `--end` = `--start`, `--side` = `new`, `--author` = `agent`, `--lines` = empty. Human `list` rows: `<id>  <status>  <author>  <file>:<start>-<end>  <first line of text>`. `--json` prints a JSON array of full documents. Unknown id → exit 1, stderr names the id. Not a git repo → exit 1, one stderr line. `add` prints the new id on stdout. Arg parsing is a hand-rolled loop over `args` (no clap); an unknown flag or missing value prints usage to stderr, exit 2. `skill-path` prints `<plugin-root>/skills/reviewr-comments/SKILL.md` where plugin-root = the executable's dir's parent (`bin/..`); if that file does not exist, try the cwd-relative dev checkout path `skills/reviewr-comments/SKILL.md`; else exit 1 naming both candidates.

SKILL.md content (write exactly this, it is the deliverable):

```markdown
---
name: reviewr-comments
description: Read, act on, and leave line-anchored review comments shared with the herdr-reviewr sidebar. Use when the user asks you to address their review comments, or to review code and leave comments in reviewr.
---

# reviewr comments

The reviewr sidebar and you share one comment store per worktree. Comments are anchored
to `file:start[-end]` with a verbatim diff snippet. The binary is
`$HERDR_PLUGIN_ROOT/bin/herdr-reviewr`; if that variable is unset, `herdr-reviewr` on
PATH or the path printed by whoever gave you this skill. Run every command from the
repo you are working in.

## Read the user's comments

    herdr-reviewr comment list            # open comments, human-readable, ids first
    herdr-reviewr comment list --json     # full documents

Trust the `lines` snippet over the line number — the code may have moved since the
comment was written. Find the snippet in the file, then act.

## The loop

1. `comment list` — see what's open.
2. Address each comment in code.
3. `herdr-reviewr comment resolve <id>` — mark it done. Do not resolve what you did
   not address; say so instead.
4. Leave your own notes where you changed or noticed something:

       herdr-reviewr comment add --file src/api.rs --start 25 \
         --lines '+  await KV.put(key, String(n + 1))' \
         --text "KV increments can lose concurrent updates"

   `--author agent` is the default; keep it. Notes render as cards in the user's
   sidebar within a second — no notification step is needed.

## Rules

- Never `comment rm` a user's comment; `resolve` is yours, `rm` is theirs.
- One comment per finding, at the tightest line range that shows it.
- Keep `--text` to a sentence or two; the diff is visible next to the card.
```

- [ ] **Step 1: Write failing integration tests** in `tests/comments_cli.rs`, following the existing `tests/` pattern of spawning the binary (`env!("CARGO_BIN_EXE_herdr-reviewr")`) in a `tempfile::tempdir()` initialized with `git init`:

```rust
// Helper: run the binary with args in `dir`, return (status, stdout, stderr).
// Tests: add prints an id and list shows the comment (human + --json shapes);
// list defaults to open-only and --all includes resolved; resolve flips status
// (visible in list --all --json); rm deletes; resolve/rm on unknown id exit 1
// naming the id; any comment command outside a git repo exits 1 with one stderr
// line; add with a missing --text exits 2 with usage on stderr.
```

Write each of those as a real `#[test]` with concrete assertions (assert stdout contains the id; parse `--json` output with serde_json and assert fields).

- [ ] **Step 2: Run** `cargo test --test comments_cli` — Expected: FAIL (unrecognized subcommand → binary launches TUI or errors).
- [ ] **Step 3: Implement** `src/cli.rs` + the `main.rs` dispatch + write `skills/reviewr-comments/SKILL.md` verbatim from above.
- [ ] **Step 4: Run** full `cargo test --all-features` + clippy — Expected: green/clean. Also manually: `cargo run -- comment list` in the repo prints nothing and exits 0.
- [ ] **Step 5:** fmt, commit: `feat(cli): comment subcommands and bundled agent skill`

---

### Task 3: `comment_sync` config key

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Produces: `PluginConfig::comment_sync() -> CommentSync` where `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum CommentSync { Immediate, OnSend }`, default `Immediate`. Key `comment_sync`, values `"immediate"` / `"on-send"`; any other value is a config error (the file's fail-loud contract). Add to `PLUGIN_CONFIG_KEYS` and `to_json` (serialize the string form).

- [ ] **Step 1: Failing tests** next to the existing enum-valued key tests (find how `toggle_placement` — an existing enum key — is parsed and tested; mirror it exactly):

```rust
    #[test]
    fn comment_sync_parses_both_values_and_defaults_immediate() {
        let dir = tempdir_with(""); // reuse the file's existing fixture helper
        assert_eq!(plugin_config_in(dir.path()).unwrap().comment_sync(), CommentSync::Immediate);
        let dir = tempdir_with("comment_sync = \"on-send\"\n");
        assert_eq!(plugin_config_in(dir.path()).unwrap().comment_sync(), CommentSync::OnSend);
        let dir = tempdir_with("comment_sync = \"sometimes\"\n");
        assert!(plugin_config_in(dir.path()).is_err());
    }
```

(Adapt helper names to the file's actual fixtures, as Task 3 of the forge plan did.)

- [ ] **Step 2:** `cargo test --lib config` — FAIL. **Step 3:** implement. **Step 4:** full suite + clippy green. **Step 5:** fmt, commit: `feat(config): comment_sync key`

---

### Task 4: App integration — lifecycle metadata, sync modes, external reload

**Files:**
- Modify: `src/model.rs` (CommentStore entries), `src/app.rs` (persistence + reload + resolve), `src/lib.rs` (tick check), `src/export.rs` (send no longer consumes)
- Test: additions to existing test modules + `tests/app_flow.rs`

**Interfaces:**
- Consumes: `comments::{Store, StoredComment, Author, Status, new_id, now_iso}` (Task 1), `config::CommentSync` (Task 3).
- Produces (Task 5 renders these):
  - `model::CommentStore` items become `Vec<comments::StoredComment>`; every entry has id/author/status from creation (`add` wraps a `model::Comment` with `new_id()`/`Author::User`/`Status::Open`). Accessors `iter()/get()/len()` now yield `&StoredComment`; `edit` mutates `comment.text`; `take` stays for delete.
  - `App::comments_disk: Option<comments::Store>` — `Store::at(<git-dir>/reviewr/comments)` built where the App learns the repo (it already resolves the git dir for `refs/reviewr`; reuse that path source). `None` when unavailable → TUI-local fallback + one status-line notice.
  - `App::resolve_selected_comment(&mut self)` — flips status of the selected list entry in memory and (if persisted mode/entry) on disk via `set_status`.
  - `App::hide_resolved: bool` + `App::toggle_hide_resolved(&mut self)`.
  - `App::sync_comments_from_disk(&mut self)` — reload merge: disk wins for ids it has; local entries whose id is absent on disk are kept ONLY if they are unpersisted `on-send` drafts (author User, never written); a locally-known id missing from disk (agent `rm`) drops. Draft-in-compose is untouched (compose state is separate).
  - `App::comments_signature: u64` — last seen `Store::signature()`; `lib.rs` calls `app.check_comment_store()` on each poll tick (piggyback where the existing periodic work runs, near the turn/PR polling) which compares signatures and calls `sync_comments_from_disk` on change.

**Behavior (spec §TUI, encode exactly):**
- `Immediate` mode: `App` persists on comment save (`store.put`), edit (rewrite via `put`), delete (`remove`). `OnSend`: user comments stay memory-only until `s`; send persists them all (put), then exports.
- **Send no longer consumes comments.** `export.rs`'s callers currently `take_all`; change to iterating open, user-authored comments (`status == Open && author == User`) for the export text, leaving entries in place. Existing app-flow tests that assert comments vanish after send must be updated to assert they REMAIN with status Open (this is the one intended behavior change; update `specs/review-model.md` in Task 6 to match).
- Agent comments are never exported by `s` and never editable (`e` on an agent comment is a no-op with a status notice); resolve and `rm`-via-TUI-delete work on any author.

- [ ] **Step 1: Failing tests.** In `src/model.rs`: adapt existing CommentStore tests to StoredComment entries (assertions keep meaning). In `tests/app_flow.rs` (find the existing helpers that build an App over a temp git repo and drive keys):
  - `immediate_mode_persists_comments_to_the_store` — create a comment via the compose flow, assert a `.json` appears under the store dir with `author == "user"`.
  - `on_send_mode_persists_only_on_send` — config `comment_sync = "on-send"`; comment → store dir empty; press `s` (export to a stub target if the harness has one; else call `app.export(...)` with the test target the suite already uses) → file exists; entry still present with status open.
  - `external_agent_comment_appears_after_tick` — write a valid store file directly, call the tick-check hook, assert the comments list contains it with `author == Agent`.
  - `resolve_toggles_status_in_memory_and_on_disk`.
- [ ] **Step 2:** Run the new tests — FAIL.
- [ ] **Step 3:** Implement per interfaces above.
- [ ] **Step 4:** Full suite + clippy — green (including updated legacy send tests).
- [ ] **Step 5:** fmt, commit: `feat(app): persistent comment lifecycle and store sync`

---

### Task 5: TUI rendering and keys

**Files:**
- Modify: `src/ui.rs` (cards, comments-list rows, footer hints), `src/lib.rs` (key handling)
- Test: `tests/render.rs` additions

**Interfaces:**
- Consumes: `StoredComment` accessors, `App::{resolve_selected_comment, toggle_hide_resolved, hide_resolved}` (Task 4).

**Behavior:**
- Diff-pane cards: an agent comment's card carries an ` agent ` label chip in the card's top border line, tinted with the palette's existing `mauve` accent (user cards unchanged). A resolved card renders dim (the palette's existing muted/overlay text color) with a `resolved` marker; when `App::hide_resolved` is set, resolved cards render nothing (height 0 — the row-height math in `comment_cards`/`comment_card_lines` must agree, see ui.rs:168-205).
- Comments-list overlay (`l`): each row gains author + status columns (`@agent`/`@you`, `resolved` marker mirroring the PR tab's row voice at ui.rs:1420ish). New keys IN THE OVERLAY ONLY: `x` = resolve/unresolve selected, `h` = toggle hide-resolved. (`x` and `h` are unbound in the overlay today — verify against the overlay's key arm in lib.rs:844-850 and the footer hint table; update both plus README Controls tables and specs/tui.md's key reference.)
- `e` on an agent comment: status-line notice "agent comments are read-only (x to resolve)".

- [ ] **Step 1: Failing render tests** in `tests/render.rs` (mirror its snapshot-style helpers): agent card shows the chip; resolved card renders dim marker; hide-resolved hides it; overlay rows show author/status columns.
- [ ] **Step 2:** Run — FAIL. **Step 3:** Implement. **Step 4:** Full suite + clippy green. **Step 5:** fmt, commit: `feat(ui): agent comment cards, resolve and hide-resolved controls`

---

### Task 6: Docs and specs

**Files:**
- Modify: `specs/review-model.md` (store, lifecycle, CLI, send-no-longer-consumes), `specs/config.md` (`comment_sync`), `specs/tui.md` (overlay keys, card markers), `specs/overview.md` (write-invariant wording: "writes only under the repo's git dir — the `refs/reviewr/` baseline and the comment store — never the worktree"), `README.md` ("Working with agents" section + Controls tables + config reference)

README "Working with agents" must include the generic prompt, mirroring hunk's:

```
Run `herdr-reviewr skill-path`, load that skill, then review this code and leave
comments in reviewr.
```

and the reverse flow: "implement the comments I left in reviewr", plus the `comment_sync` explanation (immediate = agent can read your comments anytime; on-send = nothing persists until `s`).

- [ ] **Step 1:** Read the code surfaces you document (cli.rs usage strings, actual key bindings, config parse) — docs describe what the code DOES.
- [ ] **Step 2:** Update all five files in the specs' existing voice (contract tables, terse bullets).
- [ ] **Step 3:** Guard: full `cargo test --all-features` still green; `grep -rn "never routes PR feedback" specs/` — update any invariant sentence that now contradicts the comment loop (comments are review feedback routed deliberately; the non-goal that remains is the PR tab never writing to the forge).
- [ ] **Step 4:** fmt (no-op), commit: `docs: comment store contract, agent workflow, config`

---

### Task 7: E2E verification, push, PR

- [ ] **Step 1:** Full local gate: `cargo fmt --all` + `git diff --stat` (commit content changes if any), clippy `-D warnings`, `cargo test --all-features`, `cargo build --release`. Record the test-count delta vs 290.
- [ ] **Step 2:** Manual smoke in this repo: `cargo run -- comment add --file src/main.rs --start 1 --text "smoke"` → id printed; `comment list` shows it; `comment resolve <id>`; `comment list` empty; `comment list --all` shows resolved; `comment rm <id>`. `cargo run -- skill-path` errors helpfully OR prints the dev-checkout path.
- [ ] **Step 3:** Push `agent-comments`, open a draft PR on `dcieslak19973/herdr-reviewr` (base `main`) to run CI; confirm both jobs green.
- [ ] **Step 4:** Report: CI link, test delta, and the release step (bump to 0.13.0 + tag) left for after merge.

---

## Self-Review Notes

- **Spec coverage:** store (T1), CLI + skill-path + SKILL.md (T2), config (T3), sync modes + reload + resolve + send-change (T4), rendering + keys + hide (T5), docs incl. invariant rewording (T6), e2e (T7). Non-goals respected: no threads, no daemon, no anchor rebinding.
- **Type consistency:** `StoredComment`/`Author`/`Status`/`Store::{open,at,load,add,put,set_status,remove,signature}` defined in T1, consumed by name in T2/T4; `CommentSync::{Immediate,OnSend}` T3→T4.
- **Known judgment latitude:** exact key choices (`x`/`h`) must be verified free in the overlay before binding — if taken, pick a free key and update every doc surface; the reviewer checks docs-match-code.
- **Deliberate behavior change:** send no longer consumes comments (cards persist until resolved/deleted). Old tests asserting consumption are updated, and `specs/review-model.md` documents the new lifecycle. This is spec-mandated, not drift.
