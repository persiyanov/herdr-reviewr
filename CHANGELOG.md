# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **Context-aware footer** — the footer is now a live action bar: it shows the actions available
  for what the cursor is on (comment a line, edit/delete the comment under the cursor, expand a
  fold or directory, send), the most likely one highlighted, dropping the least relevant to fit
  one line. `u/b/t scope` stays available everywhere while reviewing, and `s send N` appears once
  a comment is written. Replaces the static key-hint line.

- **Simpler PR merge status** — the footer's merge state now shows only the actionable blockers,
  `conflicts` and `blocked`; GitHub's `behind`, `unstable`, and still-computing states (jargon a
  reviewer can't act on) fold into nothing.
- **PR tab panes named distinctly** — the right navigator is now `Checks & comments` instead of a
  second `PR`, so it no longer repeats the left reader's title.

### Fixed
- **PR empty state renders once** — "no PR for this branch yet…" (and the other PR loading and
  degraded messages) showed in the header, the navigator, and the read pane at the same time; it
  now shows only in the read pane.

## [0.3.0] — 2026-06-27

### Added
- **`PR` tab** — a read-only mirror of the branch's open pull request, read from GitHub via
  `gh`: its identity and state (draft/open/merged/closed, mergeability, unpushed-commit sync),
  its checks with a pass/fail rollup, and its comments (reviews, inline findings, and plain
  comments merged newest-first, with `resolved`/`outdated` markers). Select a comment to read it;
  `o` or a click on the header chip opens the PR in the browser. It fetches when the panel opens
  and refetches on entering the tab, on `r`, on the agent's turn-end, and on a 60s fallback poll;
  a capped list shows a `+more on GitHub` marker. It never writes to GitHub.

## [0.2.1] — 2026-06-27

### Removed
- **`config.toml` and its `keep` list** — reviewr no longer opts git-ignored paths into the
  **Changes** tab. A kept ignored path had no baseline in the commit scopes, so it listed as an
  addition forever — every milestone plan piled up and never cleared. Now **every scope respects
  `.gitignore` without exception**: to review a file, track it. A `keep` entry in an existing
  `config.toml` is now ignored (the file is no longer read).

### Changed
- **Plans are tracked, not ignored** — `docs/plans/` is removed from `.gitignore`, so a plan
  shows in **Changes** while uncommitted and ages out once committed, like any tracked file.
  **All files** still browses every ignored path (dimmed).

## [0.2.0] — 2026-06-26

### Added
- **`config.toml`** — a reviewr config file in herdr's per-plugin config dir, re-read on
  refresh. Its `keep` list (gitignore globs) opts git-ignored paths into the **Changes** tab as
  untracked, so an ignored-but-intentional file (a plan, a sample env) is reviewable while build
  output stays out.
- **All files** now lists git-ignored paths too, dimmed; a wholly-ignored directory
  (`target/`, `node_modules/`) is one collapsed row that loads its contents only on expand.

### Changed
- **`branch` scope** now diffs the worktree against the merge-base with the base branch — a
  superset of `uncommitted` that adds the branch's committed work — instead of the committed-only
  `merge-base...HEAD`. It no longer shows empty when the branch's changes are uncommitted.

## [0.1.1] — 2026-06-26

### Fixed
- Corrected the keybinding example in the herdr API notes: the `plugin_action`
  command is `persiyanov.reviewr.toggle` (the manifest `id`), not `reviewr.toggle`
  (the `name`). The wrong id resolves to a non-existent plugin and herdr reports
  "plugin action not found".

## [0.1.0] — 2026-06-26

First public release as the herdr plugin `persiyanov.reviewr`.

### Added
- **Changes tab** — changed files for the active scope (`uncommitted` / `branch` /
  `last-turn`) with `+/-` stats and syntax-highlighted unified diffs.
- **All files tab** — browse the whole worktree tree and read any file's current
  content in the diff pane.
- **Comment surface** — select a line range, write a comment, and **Add all to chat**
  to send the set to the agent as `path:start-end — comment`; clipboard export via
  `pbcopy` / `wl-copy` / `xclip` / `xsel`.
- **last-turn scope** — snapshots the worktree on each observed agent turn start
  (private `refs/reviewr/` baseline ref) to show only the agent's latest changes.
- Packaged as a herdr plugin: `sidebar` pane, `toggle` action, `worktree.created`
  auto-open. Prebuilt binaries downloaded on `herdr plugin install` via
  `herdr/install.sh` from GitHub Releases (no Rust toolchain required).
- Project scaffold: edition 2024, pinned toolchain, centralized `[lints]`,
  CI (fmt + clippy `-D warnings` + test + build), release workflow, `just` tasks,
  `cargo-deny` config, MIT license.
