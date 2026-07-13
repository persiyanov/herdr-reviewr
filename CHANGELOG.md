# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.13.0] — 2026-07-12

### Added
- **Markdown rendering.** PR comment bodies and the PR description render as styled markdown —
  headings, emphasis, lists, quotes, links with dim destinations, tables, and fenced code
  highlighted with the same syntax theme as the diff panes. A wide table degrades to its source
  text. Control characters and bidi overrides in bodies render as visible placeholders, never raw.
- **PR description card.** A non-empty PR description pins a `description` row at the top of
  the PR tab's navigator, above the checks. Its body reads in the left pane.
- **Markdown preview in All files.** The `preview` binding (default `m`) toggles a read-only
  rendered preview on `.md`/`.markdown` files, named `· preview` in the pane title. Source stays
  the commentable view. The toggle carries your reading position both ways, and an unscrolled
  round-trip restores the exact cursor and scroll.
- **Clickable links.** A link in rendered markdown — the preview, the PR description, or a
  comment body — opens in the browser on click. An anchor link (`#section`) scrolls to its
  heading instead. Only `http`/`https` destinations open, anything else is inert, and a
  destination carrying control or bidi characters never reaches the OS.

## [0.12.0] — 2026-07-12

### Added
- **Customizable keybindings.** A `[keybindings]` table in reviewr's `config.toml` rebinds every
  single-key shortcut per action, with several keys per action so CJK input sources can alias the
  composed character their layout produces on the same physical key (e.g. `comment = ["c", "ㅊ"]`).
  A key bound to two actions invalidates the whole file with an error naming both actions. Footer
  and header hints follow the active bindings. (#12)

### Changed
- **The comments list no longer closes on `q`.** It closes on `esc` and the `comments` binding
  (default `l`). `q` inside the list is inert.
- **Bindings act uniformly wherever their action fires.** The comments list now answers `S` and
  `Y` for send and copy, matching the main panes.
- **Ctrl chords no longer trigger character shortcuts.** A bound key fires only unmodified.
  `ctrl+u` / `ctrl+d` half-page movement and the comment editor's chords are unchanged.
- **Degraded PR messages name the active refresh key.** "press r" hints follow a rebound
  `refresh` binding.

## [0.11.0] — 2026-07-10

### Added
- **GitHub Enterprise support in the PR tab.** Set one bare `github_host` in reviewr's
  `config.toml`; GitHub.com remains available, exact Enterprise origins and documented SSH aliases
  resolve to their canonical API host, and every `gh api` call pins that host explicitly. Origin
  rewrites, malformed URLs, unsupported hosts, and authentication remedies are surfaced directly.
  (#11)

### Changed
- **Plugin configuration now fails loud as one value.** Unknown keys or invalid values block the
  sidebar, actions, and events instead of silently falling back or partially applying settings.
  The running sidebar shows only the path-aware config error, discards work from the invalidated
  snapshot, and recovers after the file is corrected. Missing files and omitted keys still use
  defaults.
- **PR refreshes reject stale work by complete input.** Host, repository, branch, pinned `HEAD`,
  candidate branches, and base settings are probed off-thread. Superseded results never replace
  the current view; same-input failures preserve it with the exact remedy.

## [0.10.0] — 2026-07-09

### Added
- **`open` and `close` actions for scripts and layout plugins.** `herdr plugin action invoke
  open --plugin persiyanov.reviewr` opens the sidebar and does nothing when one is already
  open. `close` removes it, including a sidebar herdr's plugin registry forgot after a restart.
  `toggle` keeps its key. `open` ignores `auto_open`, so a layout that opts out of auto-open
  can still place reviewr deliberately. See `specs/herdr-host.md` and the README's layout
  recipe. (#9)

### Changed
- **The sidebar is found by its pane label, not a state file.** Toggle, open, and close now
  look for the `reviewr` pane in the live pane list. A duplicate pane from a race is swept by
  the next close, nothing goes stale across crashes or herdr restarts, and no state files are
  written.
- **Actions report their outcome.** A refused action (no workspace context, or opening outside
  a git repo) exits non-zero with one line saying why. A success prints the pane it acted on.
  Both land in `herdr plugin log list`.

## [0.9.0] — 2026-07-09

### Fixed
- **The PR tab now finds your PR even when the local branch name differs from the pushed
  name.** Agent worktrees often push with `git push origin HEAD:<name>` and no `-u`, which left
  the tab stuck on "no PR for this branch yet" while the PR sat open on GitHub. reviewr now
  derives every branch name the worktree's work could be published under — the recorded
  upstream, remote branches that carry the worktree's commits, and the local name — and asks
  GitHub about all of them in one call. GitHub decides which name holds the PR, so a stale
  upstream or a checkpoint push can never hide it. See `specs/forge-host.md`. (#10)
- **A git hiccup no longer reads as "no PR".** A failing git command during the fetch (a lock
  held by `git gc`, a ref pruned mid-read) now freezes the last good view with the retry marker
  instead of blanking the tab or showing a wrong empty state. Git errors are also read with a
  pinned locale, so a non-English git classifies the same way.

### Added
- **The header names the branch that resolved.** The resolved head branch shows dim next to the
  status chip, marked `⑂` when the head lives in a fork, and drops first on a narrow pane. The
  local branch can differ from the PR's branch now, so the header tells you which one you are
  looking at.
- **Empty states that explain themselves.** With no PR the tab names the branch names it
  queried. Several matching open PRs show the count. A detached HEAD gets its own wording.

## [0.8.2] — 2026-07-09

### Fixed
- **A hard kill mid-snapshot no longer wedges the sidebar's refresh for that worktree.** A crash
  during the turn snapshot's `git add` could leave a stale `reviewr-turn-index.lock` in the
  worktree's git dir, and every refresh after that failed with `refresh failed: git ["add", "-A"]
  failed: fatal: Unable to create … File exists` until the lock was deleted by hand. The snapshot
  now clears any leftover temp index and its lock — both private to reviewr — before running and
  on every exit path. See `specs/herdr-host.md`.
- **`herdr plugin install` now delivers the current release again.** v0.8.1 shipped with
  `herdr-plugin.toml` still saying `0.8.0`, and `install.sh` reads the manifest to pick the
  download tag, so installs were silently getting the v0.8.0 binary — without the Send resolver
  fix from #6. Both version files now carry 0.8.2.

## [0.8.1] — 2026-07-08

### Fixed
- **`Send` no longer fails with "no unambiguous agent" when a plugin sidebar or a plain shell
  shares the tab or workspace (#6).** `herdr agent list` returns every pane, but only entries
  carrying an `agent` field are real agents — the resolver now counts those alone, so one agent
  plus any number of non-agent panes resolves cleanly. Turn tracking uses the same resolver, so
  `last-turn` no longer pauses in these layouts. A refused send now also says why — no agent
  here, or several — and points at `y` to copy to the clipboard instead. Thanks @worldnine for
  the diagnosis and reproduction. See `specs/herdr-host.md`.

## [0.8.0] — 2026-07-08

### Added
- **`auto_open` config key** — `auto_open = false` in reviewr's `config.toml` turns off the
  `worktree.created` auto-open, so a layout plugin like herdr-plus can furnish a fresh worktree
  undisturbed and reviewr opens only on the toggle key, in any placement. Defaults to `true`
  (today's behavior); an unknown value falls back to the default. See `specs/herdr-host.md`. (#5)

### Changed
- README now spells out where reviewr's config file lives on disk
  (`~/.config/herdr/plugins/config/persiyanov.reviewr/config.toml`) instead of only naming
  `$HERDR_PLUGIN_CONFIG_DIR`, which users cannot resolve from their shell. (#5)

## [0.7.1] — 2026-07-08

### Fixed
- **The sidebar no longer opens a blank pane when the first `git` scan is slow, failing, or hung
  (#4).** reviewr now initializes the terminal and paints before running any `git`, so a startup
  scan error shows `load failed: …` in the status line and a hung `git` shows a frozen-but-visible
  sidebar — never the blank pane herdr leaves for a process that blocks or exits before it renders.
  See `specs/herdr-host.md`.

## [0.7.0] — 2026-07-08

### Added
- **Configurable base branch** — `base_branches` in reviewr's `config.toml` sets the ordered
  candidate list for the `branch` scope, re-read on refresh. reviewr uses the first entry that
  exists in the repo (default `origin/main` → `origin/master` → `main` → `master`), so one setting
  works across repos with different trunks and the base is reachable inside herdr, where no CLI
  flag is. `--base` still overrides. See `specs/review-model.md`. (#3)

## [0.6.0] — 2026-07-02

### Added
- **Configurable toggle placement** — `toggle_placement` (`split` | `overlay` | `zoomed` | `tab`,
  default `split`) and `toggle_direction` (`right` | `down`, split only, default `right`) in
  reviewr's `config.toml` set how the toggle opens the sidebar. The `worktree.created` auto-open
  stays a `split`/`tab` (the covering placements open only on a manual toggle). An unknown value
  falls back to its default. See `specs/herdr-host.md`. (#2)

## [0.5.0] — 2026-06-29

### Added
- **Selectable themes** — 18 named palettes (Catppuccin Mocha/Latte/Frappé/Macchiato, Dracula,
  Nord, Gruvbox dark/light, One dark/light, Solarized dark/light, GitHub light, Monokai,
  Tokyo Night day/night, Rosé Pine / Dawn), set via `theme = "<name>"` in reviewr's
  `config.toml` (re-read on refresh) or `--theme` for a dev run; default `catppuccin`. One theme
  colors the whole UI — chrome and syntax together — replacing the hardcoded Catppuccin Mocha.
  An unknown name falls back to the default. See `specs/theme.md`.

### Changed
- **`--theme` now selects the whole theme** (chrome + syntax), not just the syntect syntax theme.

## [0.4.0] — 2026-06-28

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
