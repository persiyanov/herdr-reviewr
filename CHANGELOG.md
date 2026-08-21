# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Edit the file under the cursor in your editor.** `Ctrl+E` opens the file you're
  reviewing in `$VISUAL`/`$EDITOR` — at the line under the cursor in the diff, at the
  selected file in the tree, or at a highlighted comment's anchor from the comments list.
  The chord also works from the search screen on the picked result, where every printable
  key types into the query. reviewr suspends while the editor runs and refreshes the diff
  when you return, so your edits show up immediately. Rebindable as `edit-file`.

## [0.34.0] — 2026-08-20

### Added
- **Mouse text selection.** Drag over any text — a diff line, a filename, PR comment text, the
  markdown preview — to select it character by character. Releasing copies the source text to
  the clipboard, and the selection stays highlighted until your next action. A drag released
  past the pane border still copies what was highlighted. A double-click copies the word
  under the pointer, and a triple-click copies the whole line. In the file tree, a drag
  copies the spanned rows' full paths, directories included, and a double-click copies
  one row's.
  ([#62](https://github.com/persiyanov/herdr-reviewr/issues/62))
- **Gutter commenting.** Hovering a line shows a `[+]` over its line number. Click the gutter
  to comment that line, or drag along it to comment a range — the composer opens on release.

### Changed
- **Dragging over diff text no longer selects a line range.** Range selection by mouse moved
  to the gutter. The keyboard `v` selection is unchanged.

## [0.33.0] — 2026-08-19

### Added
- **Typed base revisions.** The pick-base menu (`B`) takes any git revision, not only named
  branches: `HEAD~1`, a tag, a unique SHA prefix
  ([#75](https://github.com/persiyanov/herdr-reviewr/issues/75)). Named spellings re-resolve
  like git, so a later commit still diffs one back; the header reads `vs HEAD~1 (a1b2c3d)`.
  Type a SHA to freeze that commit; the header shows the abbrev once.

### Changed
- A unique SHA prefix shorter than seven characters completes to the abbreviated object id.
  A pasted 40-hex stays a pin.

## [0.32.1] — 2026-08-18

### Fixed
- **Brew-installed host tools resolve from any launch.** herdr starts plugin panes with a
  PATH that omits Homebrew, so the PR tab reported `gh` as missing. Reviewr now looks up
  and spawns host tools against the usual bin dirs plus the inherited PATH, so
  `gh`/`glab`/`az` resolve however the pane was opened.

## [0.32.0] — 2026-08-17

### Added
- **Every key rebindable.** The arrows and page keys are now ordinary defaults of six new
  actions in `[keybindings]`: `expand`, `collapse`, `page-up`, `page-down`, `half-up`, and
  `half-down`. Named keys spell as `left`, `right`, `up`, `down`, `pageup`, and `pagedown`
  in the config, so a vim-style `expand = ["l"]` with `collapse = ["h"]` works. Only `tab`,
  `esc`, and `enter` stay fixed. Thanks to @dferland1 for driving this in #68.

### Changed
- **A rebind now replaces the arrow defaults too.** A config that already rebinds `down` or
  `up` frees the `↓`/`↑` arrows on upgrade. Add `"down"` / `"up"` to those key lists to keep
  them.
- **A held modifier makes an arrow or page key its own key.** `ctrl+↓` no longer acts as
  plain `↓`. Bind `ctrl+down` (or any `ctrl+`/`alt+` named key) explicitly to use it.

## [0.31.0] — 2026-08-15

### Added
- **PR finding quotes.** A GitHub review comment with a stored hunk paints the comment's
  line range as Diff-view rows: syntax highlight, add/delete tints, line numbers, wrap, and
  word emphasis. The window is the range plus three stored lines above and below. The
  navigator shows `path:start-end` when the ends differ, and the read pane captions the
  range (`Comment on lines +1618 to +1622`). Peach marks the comment subject only.

### Changed
- **Finding ranges keep the forge side.** GitHub `diffSide`, GitLab `line_range.type`, and
  Azure left/right fields pick old vs new. A minus caption is an old-side comment; a plus
  caption is a new-side insertion. GitLab and Azure findings still have no snippet.

## [0.30.4] — 2026-08-13

### Changed
- **A split open takes the keyboard.** Toggle and open focus reviewr in every placement,
  including split. A new worktree still never steals focus
  ([#61](https://github.com/persiyanov/herdr-reviewr/issues/61)).

## [0.30.3] — 2026-08-13

### Fixed
- **Input IME anchoring.** The terminal cursor follows the comment, search, find, and base-picker
  insertion points using display-cell widths, keeping CJK IME candidate windows at the input
  position and avoiding a trailing caret ghost after Backspace deletes a wide character. Thanks
  [@tomotochi](https://github.com/tomotochi) ([#55](https://github.com/persiyanov/herdr-reviewr/pull/55)).
- **Comment box caret room.** A comment box too short for its text scrolls to keep the caret row
  visible, and a comment ending on an exactly-full row grows the box by the empty row the caret
  waits on.

## [0.30.2] — 2026-08-12

### Fixed
- **Opening beside a worktree agent.** Opening reviewr next to a pane running `claude -w <worktree>` now reviews the worktree's branch, not the main checkout's. The open follows where the pane's program actually is and falls back to the pane's starting directory when that place is not a git repo. Thanks [@KyongSik-Yoon](https://github.com/KyongSik-Yoon) ([#59](https://github.com/persiyanov/herdr-reviewr/pull/59)).

## [0.30.1] — 2026-08-08

### Fixed
- **Selecting upward.** A range selected from the bottom up now comments on every line in it, not just the line it started from ([#50](https://github.com/persiyanov/herdr-reviewr/issues/50)).

## [0.30.0] — 2026-08-08

### Added
- **Base picker.** `B`, or a click on the base name, picks the branch the `branch` scope diffs against, remembered per repository.
- **The header names the base.** `vs dev` while it resolves, `vs main · dev missing` when a pick stops resolving, `no base` when nothing does.

### Changed
- **One resolution chain.** `--base`, then your pick, then `origin/HEAD`, and no guessing anywhere in it.
- **`base_branches` is retired.** A config still carrying the key fails to load. Drop it and press `B` instead.
- **The `All files` tab reads `Files`,** the header stats moved to the right, and the header `Send` button is gone.

## [0.29.0] — 2026-08-01

### Changed
- **Table cell wrapping.** An over-wide table now shrinks its widest columns and wraps their
  cells instead of falling back to raw source. Tied columns shrink together. Each column keeps
  at least 8 cells, and only a table too wide at every floor still renders as its source text.

## [0.28.0] — 2026-07-31

### Added
- **Hide the navigator.** `z` hides the files navigator so the diff takes the whole body, and
  shows it again in its kept position and share. While hidden, `tab` brings it back focused,
  the footer offers `z show`, and the `PR` tab keeps its navigator. Rebind via `navigator-hide`.

## [0.27.1] — 2026-07-31

### Fixed
- **The stable launch paths now survive the install.** The installer's build step runs in a
  staging checkout that herdr renames afterwards, so the `~/.local/bin/herdr-reviewr` and
  `~/.local/state/herdr/plugins/persiyanov.reviewr/bin/herdr-reviewr` links pointed at a
  directory that no longer existed. Every toggle, open, close, or auto-open now re-points
  both links at the live plugin root, and the installer aims them at the runtime root when
  herdr provides one.

## [0.27.0] — 2026-07-31

### Added
- **Any pane running the binary is a full reviewr pane.** A layout plugin or a hand-typed
  command launches reviewr with `command = "herdr-reviewr"` and gets the same pane the
  toggle opens: the binary asks herdr for your plugin config when `HERDR_PLUGIN_CONFIG_DIR`
  is not set, and the toggle, open, and close actions recognize every reviewr pane by its
  foreground process instead of a label. The installer links the binary at the stable paths
  `~/.local/state/herdr/plugins/persiyanov.reviewr/bin/herdr-reviewr` and
  `~/.local/bin/herdr-reviewr`, so layouts have a fixed command to name. (#20)

### Changed
- **The sidebar is now the pane.** The action titles read "reviewr: toggle/open/close pane",
  and the docs follow. The keybindings and action ids are unchanged.

## [0.26.2] — 2026-07-29

### Fixed
- **Send arrives intact when the agent input is in vim normal mode.** The batch went to the
  agent as raw bytes, so a vim-style input resting in normal mode ran its leading characters
  as commands: `bit/…` arrived as `t/…`, and a batch starting with `dd` could edit whatever
  was already typed. The send now travels as one bracketed paste, which the input inserts
  literally in any mode. A paste terminator inside the batch is removed so it cannot end the
  frame early. The clipboard export is unchanged. (#41)

## [0.26.1] — 2026-07-28

### Fixed
- **A `tab`-placement sidebar is now labeled in the tab bar.** herdr gives a fresh tab a bare
  number, so the sidebar showed up as a stray like `4` and got closed as clutter. It now names
  the tab `reviewr`. The rename is best-effort, so an open that already succeeded never fails
  because the rename did.

## [0.26.0] — 2026-07-28

### Changed
- **Last turn works with any number of agents, in any sidebar placement.** A turn now belongs to
  the worktree rather than to one agent reviewr had to guess at. Work starts when any agent in the
  worktree starts and ends when they all stop, so two agents on one worktree read as one turn
  instead of stalling the scope. Before, anything reviewr could not resolve to exactly one agent
  left `last turn` waiting forever, which is what happened with a second agent around or with the
  sidebar in its own tab. The `PR` tab's per-turn refresh was stuck the same way and comes back
  with it.
- **Last turn says why it is empty.** It reads `no agent works here` when nothing is running in the
  worktree, and `waiting for the first turn` when an agent is there but has not started yet. The
  old single message claimed a turn was coming even when none could.
- **reviewr now needs herdr 0.7.5.** Worktree turns read each agent's working directory from
  `herdr agent list`, which older versions are not known to report.

### Fixed
- **The `PR` tab refreshes after a turn you answered a prompt in.** A turn that went from working
  to a permission prompt and then straight to idle never registered as having ended, so the tab
  skipped its per-turn refetch for it.

## [0.25.1] — 2026-07-27

### Changed
- **Selected rows keep their dim parts readable.** A row's secondary text used to all but vanish
  under the selection fill. It now brightens with the fill, in every list that has one: the file
  list's indent, a search hit's line number, and a picker row's state and tab.

### Fixed
- **A failed send says something you can read.** When `herdr` refused a send, the status filled
  with the command reviewr had run, which carries your whole review as one argument. A 40-column
  footer answered with a fragment of your own comments. It now says `agent not found`. herdr's own
  wording is a JSON envelope around a pane id, so that goes to the log and never to you.
- **A chord never sends by accident.** `Alt+Enter` and `Shift+Enter` mean "newline, not submit" in
  the comment editor, and they used to send the whole review from the agent picker. Only the
  unmodified `enter` sends now. Modified digits no longer move the highlight either.
- **The footer never spends its width twice.** On a pane too narrow to show a long status, the
  footer used to drop the cursor's actions to make room for it and then drop the status too,
  leaving the row with neither. A status that cannot be shown now costs the row nothing.
- **The picker keeps its keys on the PR tab.** A picker opened there would have handed `q` and the
  digits to the tab behind it.

## [0.25.0] — 2026-07-26

### Added
- **Send picks the agent when there are several.** `Send` used to refuse in a workspace with more
  than one agent, leaving the clipboard as the only route, which reaches nothing when you review
  over SSH. It now opens a picker listing every agent in the workspace. Move with the arrows or
  `j`/`k`, jump with `1`–`9`, `enter` sends, `esc` keeps every comment. A click highlights a row,
  and a click on the highlighted row sends. The highlight opens on the agent you sent to last,
  marked `last used`, else the agent the sidebar was opened beside. A successful send names the
  agent it went to.
- **Modals own the screen.** While the picker or the comments list is open, everything behind it
  dims toward the theme background. The footer stays bright with the modal's own keys.

### Changed
- One agent still sends straight through, with no picker. Turn tracking is unchanged.

### Fixed
- **The status survives a narrow sidebar.** A message longer than the room left on the footer's
  first row used to vanish outright, so a 40-column pane answered `s` with nothing at all. It named
  neither the agent it reached nor the reason it refused. The status now truncates to fit, and the
  cursor's actions step aside for it, since `?` already lists them.
- **A picker row names any state herdr reports.** An agent status reviewr had never heard of read
  `unknown` on the row. The row now shows herdr's own spelling for it, and herdr's own label.

## [0.24.1] — 2026-07-23

### Changed
- Linux release binaries are now statically linked via musl (matching herdr itself), removing the
  glibc ≥ 2.39 requirement that prevented installation on Amazon Linux 2023, Debian 12,
  Ubuntu 22.04, and other distributions.

## [0.24.0] — 2026-07-23

### Changed
- **The PR tab resolves by branch name.** The tab shows the newest PR opened from the current
  branch, the same answer `gh pr view` gives, on GitHub, GitLab, and Azure DevOps. A merged PR
  stays visible until the branch's next PR replaces it. A new branch that reuses a deleted
  branch's name starts empty. Work pushed from main as `HEAD:<side-branch>` shows the side
  branch's PR. On a fork, a PR into upstream outranks the fork's own. The ambiguous
  several-PRs state is gone, and so is the commit-identity machinery behind it.

## [0.23.0] — 2026-07-23

### Added
- **GitLab and Azure DevOps in the PR tab.** The read-only PR tab now mirrors merge requests on
  GitLab and pull requests on Azure DevOps, not only GitHub. GitLab works on gitlab.com and one
  self-hosted instance set with `gitlab_host`, through the `glab` CLI. Azure DevOps works on
  dev.azure.com, the `*.visualstudio.com` organization hosts, and one self-hosted server set with
  `azure_devops_host`, through `az` with the azure-devops extension. Each forge fills the same
  snapshot and shows its own vocabulary — merge request `!42` on GitLab, pull request `#12` on
  Azure DevOps (#29, #30).

## [0.22.1] — 2026-07-22

### Fixed
- **Send works on herdr 0.7.5.** herdr 0.7.5 removed `agent send`, so pressing send failed and
  the comments stayed put (#28). Comments now go through `pane send-text` — the same
  literal-text, no-Enter write — which works on every supported herdr from 0.7.0 up.

## [0.22.0] — 2026-07-21

### Added
- **Find in file.** `Ctrl+F` searches the open file. Every match lights up, and `enter` and the
  arrows step the cursor between them, expanding a fold to reveal a hidden match. The query is a
  literal, smart-case substring. `esc` closes the band and leaves you on the match.
- **Modifier chords in keybindings.** `[keybindings]` now binds an action to a `ctrl+`/`alt+`
  chord, not only a bare character. `find` defaults to `ctrl+f` and rebinds like any other action.

### Changed
- **The footer expands on demand.** By default it shows one row — the next step, the cursor's
  actions, and `send` — closing with a `?` at the right. Press `?` to open every shortcut that works
  here, grouped into `do`, `go`, and `move` bands. Press `?` or `esc` to close it. The always-on
  cluster of muted keys is gone. `keys` binds the toggle, default `?`.
- **Footer hints spell out named keys.** `shift+enter` and `tab` replace the `⇧⏎` and `⇥` glyphs, so
  a hint reads the same on screen as in the config.

## [0.21.0] — 2026-07-20

### Added
- **Search.** `/` from any tab opens a search screen over the whole worktree. Fuzzy file names
  and literal code grep share one list, in the engine's order. Pick a result to land on its
  file and line. Matching, ranking, and indexing come from
  [fff](https://github.com/dmtrKovalenko/fff). Ranking improves as you pick, and the frecency
  store lives in the cache directory, never the worktree.

## [0.20.1] — 2026-07-18

### Changed
- **Input never waits on a refresh.** Every background rebuild — the changed set, the file tree,
  the agent-status sample, the turn snapshot — now runs on a worker thread. A keypress paints
  immediately even while the sidebar refreshes, and a poll tick can no longer swallow a keystroke
  mid-scroll. Results land only while they still describe what you are looking at, so the view is
  at worst briefly stale, never wrong.
- **A refresh indicator in the tab strip.** Pressing `r` lights a one-cell `⟳` beside the tabs
  immediately, held long enough to read. Background refreshes show it only when they run long — a
  cold scan, a slow fetch, a hung git. It replaces the `PR` pane's `· refreshing…` title note, and
  its reserved cell means the header never shifts.
- **Scope switches repaint consistently.** Switching scope in `All files` updates the header count
  and every row's change badge in the same frame, with the tree itself refreshing right behind.

## [0.19.0] — 2026-07-18

### Changed
- **Tab switches are instant.** Entering `Changes` or `All files` paints the tab exactly as you
  left it in one frame and refreshes right behind it, on any repo size. A first-ever visit loads
  before its frame, so the header never describes a tab that shows nothing.
- **`All files` is fast in huge repos.** The ignored-tree listing no longer walks inside ignored
  directories. Entering the tab dropped from over a second to well under 200ms on a 10k-file repo
  with gigabytes of ignored trees, and every background refresh sheds the same cost.
- **The `PR` tab resolves by published commits, not branch names.** The worktree's published
  work nominates its pull request by exact commit identity, so renames, deletions, and same-named
  fork branches cannot misdirect the tab.
- **The `PR` tab keeps its snapshot while it refreshes.** New commits no longer blank the tab to
  `loading`. It clears only when the repository itself changes. A turn-end refetch now fires from
  any tab, so opening `PR` after the agent finishes finds fresh data already on its way.

## [0.18.1] — 2026-07-16

### Changed
- **Copy and onboarding are clearer.** Export confirmations now distinguish adding comments to the
  agent input from copying them, PR failures pair the problem with a concrete recovery step, and
  config errors explain that a corrected file reloads automatically. The README now shows how to
  open reviewr immediately after installation, gives the last-turn diff its own feature callout,
  and demonstrates the full comment-to-agent handoff.
- **The demo shows reviewr itself.** The README recording now runs the installed plugin full-screen
  with its real terminal palette instead of simulating an adjacent agent pane.

## [0.18.0] — 2026-07-15

### Changed
- **Fork pull requests resolve automatically.** A readable, supported `upstream` remote now selects
  the base repository. An absent or unsupported `upstream` falls back to `origin`; a Git read failure
  stays visible and never falls through. SSH host aliases are no longer inferred: GitHub.com and
  configured Enterprise hosts must match exactly. Literal `github.com-*` Enterprise hostnames remain
  valid when configured exactly. A Git failure before the target resolves replaces any snapshot
  whose repository can no longer be proven. The ordinary empty state now says `No pull request yet.
  Ready to ship?`. (#18; thanks @ubuntudroid for the report and original fix.)
- **Rust 1.97 is now the minimum toolchain.** Local builds, Clippy, CI, and release builds use the
  same pinned compiler version.

## [0.17.0] — 2026-07-14

### Added
- **Four-way navigator placement.** The navigator can sit on the right, bottom, left, or top of
  every tab. Press `p` to cycle clockwise, or set `navigator_position` in plugin config. Side and
  stacked layouts remember separate sizes, with `<` / `>` and divider dragging available on both
  axes. (#16)
- **Independent PR navigator scrolling.** The checks and comments viewport scrolls without moving
  its selection. `Tab` changes pane focus, and page keys scroll the focused PR pane.

### Changed
- **Navigator resize actions have position-neutral names.** Config uses `navigator-grow` and
  `navigator-shrink`; `list-wider` and `list-narrower` remain accepted aliases.
- **Breaking: `p` is a new default key.** A custom binding that already uses `p` now collides with
  `navigator-position` and must be moved before the config becomes valid again.

## [0.16.1] — 2026-07-13

### Fixed
- **The diff cursor is visible from the file list.** The diff pane hid its cursor row whenever the
  file list held focus, so a hunk step driven from the list moved a cursor you could not see. Both
  panes now always mark their cursor row, filling it brightly when the pane has focus and a step
  softer when it does not — the file list already behaved this way.

## [0.16.0] — 2026-07-13

### Added
- **Changeset traversal.** `]` and `[` jump to the next and previous hunk, so the whole changeset
  reads hunk by hunk without a detour through the file list. At a file's last hunk the key stops:
  the footer offers `] next file`, and pressing it again crosses, so a held key never flies past a
  file. A file with no hunk — a binary, a pure rename — is crossed over. `f` and `F` jump to the
  next and previous file outright, from either pane. All four are rebindable, like the rest of the
  keymap.

### Changed
- **Pane divider keys.** The divider moves with `<` and `>`, each key pointing the way it goes, so
  `<` widens the file list and `>` narrows it. The old `]` and `[` now step hunks.
- **Breaking: `]`, `[`, `f`, `F`, `<`, and `>` are new default keys.** A `[keybindings]` config
  that binds any of them to another action now collides with a default. A collision makes the
  whole config invalid, so the sidebar shows only the config error until you move the key. The
  error names both actions involved.

## [0.15.0] — 2026-07-13

### Added
- **Aggregate change stats in the header.** The header now shows the active scope's line totals
  next to the changed-file count (`9 changed  +42 −18`), colored like the per-file stats. A zero
  side drops, and an empty changeset shows the bare count.
- **Configurable startup scope.** A new `default_scope` config key (`"uncommitted"`, `"branch"`,
  or `"last-turn"`) names the scope the sidebar starts in. It seeds only a fresh sidebar:
  switching with `u`/`b`/`t` wins for the session, and a config reread never switches the
  active scope.

## [0.14.0] — 2026-07-13

### Changed
- **Markdown preview in the Changes tab.** The `preview` binding (default `m`) now toggles the
  rendered preview from a markdown file's diff, not only in All files. It renders the file's
  current content, so a deleted file's toggle is inert. Returning to the diff leaves the cursor,
  scroll, and folds exactly where they were. The preview choice is kept per tab.

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
