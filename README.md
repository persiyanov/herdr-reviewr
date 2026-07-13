# herdr-reviewr

A code-review sidebar for [herdr](https://herdr.dev). Your agent writes the code. You read its
diff in a pane beside the chat, comment on the lines, and send the notes back. You never leave
the terminal.

![demo](assets/demo.gif)

What you get, in one persistent pane pointed at a git worktree:

- **Diff review** — the agent's changed files, syntax-highlighted, scoped to *uncommitted*,
  *branch*, or *last turn*.
- **Line comments** — select a range and write a note. It stays visible as a card under the code
  instead of hiding behind a marker.
- **Send** — one keystroke drops every comment into the agent's input as
  `path:start-end — comment`. You add context and hit enter.
- **File viewer** — the whole worktree, not just the diff, with any file's current content
  rendered in the pane.
- **PR view** — the branch's open pull request, read-only, without switching windows. The
  description and every comment render as styled markdown.
- **Markdown preview** — one key flips a `.md` file between source and a rendered view, with
  headings, lists, tables, links, and code blocks highlighted like the diff. The toggle keeps
  your reading position.
- **Themes** — 18 named palettes in dark and light, one config line away. Catppuccin, Dracula,
  Nord, Gruvbox, Tokyo Night, Rosé Pine, Solarized, and more.

It **never edits your worktree** and sends nothing on its own. Its only write to git is a private
`last-turn` baseline ref under `refs/reviewr/`. The **PR** tab reads GitHub but never posts there.

## Requirements

- **herdr ≥ 0.7.0** (the plugin system).
- **git** on `PATH`.
- A **truecolor (24-bit)** terminal with Unicode box-drawing support. Pick a theme that matches
  its light or dark background (see [Theme](#theme)).
- **macOS or Linux.**
- **`gh`** (the GitHub CLI), authenticated. Optional, only the **PR** tab needs it. Everything
  else works without it.

## Install

From the herdr marketplace. You get a prebuilt binary, no Rust toolchain:

```bash
herdr plugin install persiyanov/herdr-reviewr
```

The sidebar **auto-opens for a newly created worktree**, so installing the plugin is enough. Set
`auto_open = false` to keep it hidden until you ask (see [Configuration](#configuration)). To
toggle it on demand, bind a key to the **reviewr: toggle sidebar** action in your herdr config.
Keybindings live in user config, not in the plugin manifest:

```toml
[[keys.command]]
key = "cmd+r"
type = "plugin_action"
command = "persiyanov.reviewr.toggle"   # <plugin_id>.<action_id> — note the id, not the name
```

`cmd+…` chords reach herdr. macOS swallows `alt+…`. With no key bound, run the action once with
`herdr plugin action invoke toggle --plugin persiyanov.reviewr`.

Beside `toggle` there are two explicit actions, made for scripts and layout plugins. `open` opens
the sidebar and does nothing when one is already open. `close` closes it and does nothing when none
is. Bind or invoke them the same way, as `persiyanov.reviewr.open` and `persiyanov.reviewr.close`.
See [Auto-open and layout plugins](#auto-open-and-layout-plugins) for the layout recipe.

## Quick start

The core loop takes five keys. Open the sidebar next to your agent and:

1. **Pick a file.** The agent's changed files are in the right pane. `j` / `k` moves the cursor.
   The diff opens on the left as you go.
2. **Focus the diff.** Press `Tab` to move from the file list into the diff.
3. **Select the lines.** Press `v`, then `j` / `k` to extend the selection (or click-drag).
4. **Comment.** Press `c`, type your note, `Enter` to save. It stays on screen as a card under
   the line.
5. **Send.** When you're done, press `s`. Every comment lands in the agent's input as
   `path:start-end — comment`. You add context and send.

The footer always shows the keys that work right now, so you can learn it by using it. The tables
below are the full reference.

## Controls

The single-key shortcuts below are defaults. Any of them can be rebound per action, including to
several keys at once ([Keybindings](#keybindings)).

**Getting around**

| Key | Action |
| --- | --- |
| `1` `2` `3` | Switch tab — Changes / All files / PR |
| `u` `b` `t` | Switch scope — uncommitted / branch / last turn |
| `j` `k` · `↑` `↓` | Move the cursor in the focused pane |
| `f` `F` | Jump to the next / previous file (from either pane) |
| `PageUp` `PageDown` | Move a page · `Ctrl+U` `Ctrl+D` move a half-page |
| `PageUp` `PageDown` | Move a page |
| `Ctrl+U` `Ctrl+D` | Move a half-page |
| `Tab` | Switch focus between the file list and the diff |
| `→` `←` | Expand or collapse a directory or fold, or scroll the diff sideways |
| `w` | Toggle line wrap |
| `m` | Toggle the markdown preview of a `.md` file (All files) |
| `]` `[` | Widen / narrow the file list |
| `r` | Refresh now |
| `q` | Quit |

**Reviewing** (in the diff)

| Key | Action |
| --- | --- |
| `v` | Start a line selection, then `j` / `k` to extend (or click-drag) |
| `c` | Comment on the selection — or on the current line |
| `e` `d` | Edit / delete the comment under the cursor |
| `n` `N` | Jump to the next / previous comment |
| `l` | List every comment |
| `s` | Send all comments to the agent |
| `y` | Copy all comments to the clipboard |
| `esc` | Clear the selection |

**In the comment box**

| Key | Action |
| --- | --- |
| `Enter` | Save the comment |
| `Esc` | Cancel |
| `Shift+Enter` · `Alt+Enter` · `Ctrl+J` | Insert a newline |

Plus the usual caret moves: arrows, `Home` / `End`, `Ctrl+A` / `Ctrl+E`, word-jump with
`Alt+b` / `Alt+f`, and `Ctrl+W` / `Ctrl+U` / `Ctrl+K` to delete by word or to the line edge.

**PR tab** (read-only)

| Key | Action |
| --- | --- |
| `j` `k` | Move through the description and comments |
| `PageUp` `PageDown` | Scroll the read pane |
| `o` | Open the PR in your browser |
| `r` | Refresh |

herdr is mouse-native, so clicking a file, dragging to select lines, clicking a tab or the `Send`
button, and the scroll wheel all work too. A link in rendered markdown opens in your browser on
click (`http`/`https` only), and an anchor link (`#section`) jumps to its heading.

## The three tabs

- **Changes** — the changed files for the active scope, with `+/-` stats. Pick a file to read its
  syntax-highlighted diff. This is where you review and comment.
- **All files** — the whole worktree tree, not only what changed. The diff pane renders any
  file's current content. Git-ignored paths show too, dimmed. A directory ignored as a whole
  (`target/`, `node_modules/`) is one collapsed row that loads its contents only when you expand
  it. You can comment here as well. On a `.md` file, `m` flips between the source and a rendered
  markdown preview. The preview is read-only, so commenting stays in the source view.
- **PR** — a read-only mirror of the branch's open pull request, read from GitHub via `gh`. It
  shows the PR's state (draft, open, merged, or closed, plus mergeability and unpushed-commit
  sync), its checks with a pass/fail rollup, and its comments. The PR description sits at the top
  of the list. Comments cover reviews, inline findings, and plain comments, newest first, with
  `resolved` and `outdated` markers. The description and every comment body render as styled
  markdown, code blocks highlighted with your theme. `o` opens the PR in the browser. The tab
  only reads GitHub. It never posts, resolves, re-runs, or merges.

## Diff scopes

- **uncommitted** — the working tree vs `HEAD` (staged, unstaged, and untracked).
- **branch** — the working tree vs the merge-base with the base branch. The default base is
  `origin/main`, then `origin/master`, `main`, `master`, set via `base_branches` or `--base`.
  This scope is **uncommitted** plus the branch's committed work.
- **last turn** — only what the agent changed since its most recent turn started (see
  [Limitations](#limitations)).

Every scope respects `.gitignore`, so build output never clutters **Changes**. To review a file,
track it in git. An ignored-but-intentional file (a plan, a sample env) belongs in the repo.
There it shows as a change and ages out once committed. **All files** can still browse any
ignored path, dimmed, even untracked ones.

## Configuration

CLI flags on the pane command:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--poll <ms>` | `2000` | worktree poll interval (min `200`) |
| `--base <ref>` | auto | base branch for `branch` scope, overrides `base_branches` |
| `--theme <name>` | `catppuccin` | UI + syntax theme (see below) |
| `--wrap <on\|off>` | `on` | soft-wrap long diff lines (`w` toggles at runtime) |

Everything else is set in reviewr's own config file:

```text
~/.config/herdr/plugins/config/persiyanov.reviewr/config.toml
```

Create the file if it does not exist yet. herdr hands this directory to the plugin as
`$HERDR_PLUGIN_CONFIG_DIR`, and the path above is where it lives on disk. Note that this is
reviewr's file, not herdr's. Settings added to herdr's own `~/.config/herdr/config.toml` never
reach reviewr.

The file accepts these seven keys:

```toml
theme = "tokyo-night"
base_branches = ["origin/develop", "origin/main", "main", "master"]
toggle_placement = "overlay"
toggle_direction = "down"
auto_open = false
github_host = "github.example.com"

[keybindings]
comment = ["c", "ㅊ"]
select  = ["v", "ㅍ"]
```

A missing file or omitted key uses its default. Any unknown key, wrong type, or invalid value
makes the whole file invalid. reviewr never applies the valid-looking parts. The sidebar then
shows only the config error, and actions or events exit non-zero without touching the workspace.
Fix the file and the running sidebar recovers on its next refresh. Replace the file atomically
if your editor or config manager might expose a partial save.

### Theme

One theme colors the whole UI, chrome and syntax together. Set it in reviewr's config file.
reviewr re-reads the file on refresh, so editing it and refreshing re-themes without a relaunch:

```toml
# ~/.config/herdr/plugins/config/persiyanov.reviewr/config.toml
theme = "tokyo-night"
```

`--theme` overrides the config file (handy for a dev run). Pick a name that matches your
terminal's light or dark background. The pane keeps the terminal's background, so a light theme
on a dark terminal reads poorly, and so does the reverse. Available:

- **Dark:** `catppuccin`, `catppuccin-frappe`, `catppuccin-macchiato`, `dracula`, `nord`,
  `gruvbox`, `one-dark`, `solarized`, `monokai`, `tokyo-night`, `rose-pine`.
- **Light:** `catppuccin-latte`, `gruvbox-light`, `one-light`, `solarized-light`, `github-light`,
  `tokyo-night-day`, `rose-pine-dawn`.

Names match herdr's where both ship a palette. An unknown config name is an error. The standalone
`--theme` development flag retains its older fallback to `catppuccin`.

### Base branch

The **branch** scope diffs against the merge-base with a base branch. reviewr tries an ordered
list of candidates and uses the first that exists in your repo, so one setting works across repos
with different trunks. The default is `origin/main`, then `origin/master`, `main`, `master`.

To review against a different base, a `develop` trunk say, set `base_branches` in the same
config file. reviewr re-reads it on refresh, so editing it and pressing `r` re-bases without a
relaunch:

```toml
# ~/.config/herdr/plugins/config/persiyanov.reviewr/config.toml
base_branches = ["origin/develop", "origin/main", "main", "master"]
```

reviewr picks the first entry that exists in the repo. A `--base <ref>` flag still wins when it
names an existing ref. A missing file or omitted key uses the default list. A malformed value
blocks the plugin like any other invalid config.

### Keybindings

Every single-key shortcut is rebindable per action. Set `[keybindings]` in the same config file.
Each entry maps an action name to an array of keys. That array replaces the action's default
keys, and actions you don't mention keep theirs. The footer and header hints show the first key
in the array. reviewr re-reads the file on refresh, so a keymap edit applies without a relaunch.

```toml
# ~/.config/herdr/plugins/config/persiyanov.reviewr/config.toml
[keybindings]
comment = ["c", "ㅊ"]
select  = ["v", "ㅍ"]
```

Several keys per action is the point when a CJK input source is active. The OS sends the
composed character to the terminal, so the plain ASCII shortcut never arrives. Bind the
character your layout produces on the same physical key, and the shortcut works without
switching the input source to English.

The action names and their defaults:

| Action | Default |
| --- | --- |
| `down` / `up` | `j` / `k` |
| `scope-uncommitted` / `scope-branch` / `scope-last-turn` | `u` / `b` / `t` |
| `tab-changes` / `tab-all-files` / `tab-pr` | `1` / `2` / `3` |
| `wrap` | `w` |
| `preview` | `m` |
| `list-wider` / `list-narrower` | `]` / `[` |
| `select` | `v` |
| `comment` | `c` |
| `edit` / `delete` | `e` / `d` |
| `next-comment` / `prev-comment` | `n` / `N` |
| `comments` | `l` |
| `send` | `s`, `S` |
| `copy` | `y`, `Y` |
| `open-pr` | `o` |
| `refresh` | `r` |
| `quit` | `q` |

A key is one character, and any printable character works. The arrows, `Tab`, `Esc`, `Enter`,
and the page keys are fixed and always work. Keys still type normally in the comment box. Two
actions can never share a key. A collision makes the whole file invalid, and the error names
both actions, so a typo can't silently shadow another shortcut.

### GitHub hosts

GitHub.com works without configuration. To read pull requests from one GitHub Enterprise host,
set its bare hostname:

```toml
github_host = "github.example.com"
```

reviewr matches either that exact origin host or a trusted SSH alias beginning
`github.example.com-`, such as `git@github.example.com-work:owner/repo.git`. The alias convention
applies only to scp-style and `ssh://` origins. HTTPS hosts must match exactly. GitHub.com and
its SSH aliases continue to work when Enterprise is configured.

Host identity comes from origin's fetch URL after Git's `url.*.insteadOf` rewrite. A separate
push URL does not change it. API calls name the canonical host on every request, so `GH_HOST`
cannot redirect a fetch. Authenticate the Enterprise host with
`gh auth login --hostname github.example.com`.

### Sidebar placement

By default the toggle opens reviewr as a split to the right of your agent. You can change how it
opens by setting `toggle_placement` in the same config file. reviewr re-reads the file on every
toggle, so a change takes effect the next time you press the key.

```toml
# ~/.config/herdr/plugins/config/persiyanov.reviewr/config.toml
toggle_placement = "overlay"   # split | overlay | zoomed | tab   (default: split)
toggle_direction = "down"      # right | down — split only        (default: right)
```

- **`split`** sits next to your agent and leaves the keyboard with it. Set `toggle_direction` to
  put reviewr on the right (the default) or below.
- **`overlay`** covers the whole tab with reviewr and hands it the keyboard. Toggle again to drop
  back to your agent.
- **`zoomed`** fills the tab the same way as overlay and hands reviewr the keyboard.
- **`tab`** opens reviewr in its own tab and hands it the keyboard.

When you create a new worktree, reviewr auto-opens only for `split` and `tab`. With `overlay` or
`zoomed` it stays out of the way until you press the toggle yourself. An unrecognized value makes
the config invalid. You can also turn the auto-open off entirely. The next section shows how.

### Auto-open and layout plugins

reviewr auto-opens for every new worktree by default. To make it wait for the toggle key instead,
set `auto_open = false` in the same config file:

```toml
# ~/.config/herdr/plugins/config/persiyanov.reviewr/config.toml
auto_open = false   # default: true
```

Do this when another plugin arranges your new worktrees, for example
[herdr-plus](https://github.com/cloudmanic/herdr-plus) worktree layouts. Both plugins react to the
same worktree event and race each other, and either can lose. The race can skip the layout
entirely, or drop reviewr as a split in the middle of it. With `auto_open = false` reviewr leaves
fresh workspaces alone. The layout builds undisturbed, and the toggle key opens reviewr on top of
it in whatever placement you configured.

A layout can also open reviewr itself, once its panes are in place:

```
herdr plugin action invoke open --plugin persiyanov.reviewr
```

`open` ignores `auto_open`, because an explicit call is you asking. It opens with your configured
placement and does nothing when a sidebar is already open, so a layout can run it on every pass.
Two things to know. The action opens reviewr in the **focused** workspace, so invoke it while the
new workspace has focus. And it opens reviewr as its **own new pane**. A layout pane whose command
is the invoke will exit once the invoke returns. Run the invoke as a one-shot command from your
layout hook, not as a pane that should stay.

## Limitations

This is a focused, young tool. The known constraints:

**Terminal & theme**
- **Truecolor required** — colors are 24-bit RGB with no 256/8-color fallback. Basic terminals
  render wrong colors.
- **Theme must match the terminal** — the pane keeps the terminal's background, so a light theme
  on a dark terminal reads poorly, and so does the reverse. There is no auto light/dark detection
  yet. You set the theme to match by hand.
- **Add / remove are red / green** — no secondary cue for colorblind users yet.
- **Box-drawing glyphs required** — the UI draws with Unicode box characters. No Nerd Font
  needed.

**Platform**
- **macOS and Linux only** — no Windows.
- **Clipboard export** uses `pbcopy` on macOS, or `wl-copy` / `xclip` / `xsel` on Linux. With
  none installed it says so, and **Send** still works. OSC 52 and Windows are on the roadmap.

**herdr coupling**
- **Send needs a findable agent pane** — the agent in your tab, or the sole agent in the
  workspace. Otherwise Send does nothing and keeps your comments. Browsing and diffing need no
  herdr.
- **last turn relies on polling** (2 s default) — a turn that starts and finishes inside one poll
  never gets its own snapshot. The scope then shows everything since the last *observed* turn
  start. That is never lines the agent didn't write, but possibly more than one turn.

**PR tab (GitHub)**
- **GitHub-only and read-only** — needs an authenticated `gh` and a GitHub remote. Without either
  it shows one line telling you what to fix, and Changes and All files keep working.
- **Mirrors only the branch's *open* PR** — a merged or closed PR shows as history. Each comment
  surface caps at one page (100 rows), with a `+more on GitHub ↗` marker when there is more.

**Review model**
- **Comments are in-memory and single-session** — closing the pane loses any you haven't sent or
  copied out.
- **Sending is all-or-nothing** — Send (or copy-to-clipboard) delivers the whole set and clears
  it. There is no per-comment send and no duplicate delivery. A failure leaves everything in
  place.
- **No line-number rebasing** — a comment stays locatable by its diff snippet, not its line
  number. reviewr flags a stale comment instead of dropping it.
- **One sidebar per worktree** — two on the same worktree race the baseline ref, and the last
  writer wins.

**Budgets**
- Files over **2 MB** or **50,000 lines** show a "too large" notice. **Binary** files get no
  diff.

## Building from source

For contributors. `herdr plugin link` skips the download build step, so place a locally built
binary where the pane command looks for it, at `$HERDR_PLUGIN_ROOT/bin/herdr-reviewr`:

```bash
git clone https://github.com/persiyanov/herdr-reviewr
cd herdr-reviewr
just install   # build release → bin/herdr-reviewr, ad-hoc re-signed on macOS
herdr plugin link .
```

`just install` replaces the binary with a fresh file and ad-hoc re-signs it. On Apple Silicon
that matters. Overwriting a code-signed binary in place invalidates its signature, and macOS then
SIGKILLs it at launch. So a plain `cp target/release/herdr-reviewr bin/` makes the pane open and
close instantly.

**The dev loop** after the first link:

1. Edit the code.
2. Run `just install` to rebuild and re-sign the binary under `bin/`.
3. Relaunch the sidebar by toggling it off and back on with your keybind. The open pane keeps
   running the *old* process until then, so a rebuild alone changes nothing on screen.

This loop works only while the plugin is **linked**, not installed from the marketplace. Check
with `herdr plugin list`. A `github:…` source means the pane runs a *downloaded* binary under
`~/.config/herdr/plugins/github/`, and local rebuilds never appear there no matter how often you
run `just install`. Switch a GitHub install to a dev link:

```bash
herdr plugin uninstall persiyanov.reviewr   # config is keyed by id and survives
herdr plugin link .
```

## Roadmap

Structured (JSON) export, in-diff search, a side-by-side split view, mark-file-reviewed,
modifier and named-key notation for keybindings, OSC light/dark theme autodetect, more themes
(`kanagawa`, `vesper`, `everforest`, `ayu`, a dark `github`), a `terminal`-following palette,
and OSC 52 clipboard.

## Design

The living design lives in [`specs/`](specs/), one concept per doc, always current.

## License

[MIT](LICENSE). Syntax highlighting comes from [syntect](https://github.com/trishume/syntect) and
[two-face](https://github.com/CosmicHorrorDev/two-face). Most themes' syntax colors come from
two-face's bundled set.

Bundled `.tmTheme` syntax files in `assets/`, each under its own license:

- [Catppuccin Mocha](https://github.com/catppuccin/bat) — MIT.
- [Tokyo Night](https://github.com/folke/tokyonight.nvim) (`tokyo-night`, `tokyo-night-day`) — Apache-2.0.
- [Rosé Pine](https://github.com/rose-pine/tm-theme) (`rose-pine`, `rose-pine-dawn`) — MIT.
