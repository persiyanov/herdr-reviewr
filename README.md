# herdr-reviewr

[![CI](https://github.com/persiyanov/herdr-reviewr/actions/workflows/ci.yml/badge.svg)](https://github.com/persiyanov/herdr-reviewr/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/persiyanov/herdr-reviewr)](https://github.com/persiyanov/herdr-reviewr/releases/latest)
[![License](https://img.shields.io/github/license/persiyanov/herdr-reviewr)](LICENSE)

<p align="center">
  <a href="#install">install</a> · <a href="#quick-start">quick start</a> · <a href="#controls">controls</a> · <a href="#diff-scopes">scopes</a> · <a href="#configuration">configuration</a> · <a href="#limitations">limitations</a> · <a href="CHANGELOG.md">changelog</a>
</p>

A code-review pane for [herdr](https://herdr.dev). Your agent writes the code. You read its
diff in a pane beside the chat, comment on the lines, and send the notes back. You never leave
the terminal.

![demo](assets/demo.gif)

One persistent pane, pointed at a git worktree:

- **Diff review** — the agent's changed files, syntax-highlighted.
- **Last-turn diff** — what the worktree's latest turn changed, on its own.
- **Line comments** — comment on a line or a range. Then send it to the agent.
- **Text selection** — drag over any text to copy it, like an editor.
- **File viewer** — any file's current content from the whole worktree.
- **Search** — fuzzy file names and live code grep across the worktree, powered by [fff](https://github.com/dmtrKovalenko/fff).
- **Find in file** — search the open file and step between every match.
- **PR view** — the branch's pull request in the pane, read-only.
- **Markdown preview** — flip a `.md` file between source and rendered view.
- **Themes** — 18 palettes in dark and light.

It never edits your worktree and sends nothing on its own. Its only git writes are private
refs under `refs/reviewr/`. The **PR** tab reads GitHub, GitLab, or Azure DevOps and never
posts.

## Requirements

- **herdr ≥ 0.7.5** (the plugin system).
- **git** on `PATH`.
- A **truecolor** terminal with Unicode box-drawing.
- **macOS or Linux.**
- **`gh`** (GitHub), **`glab`** (GitLab), or **`az`** (Azure DevOps, with the `azure-devops` extension), authenticated. Only the **PR** tab needs one.

## Install

Prebuilt binaries, no Rust toolchain needed:

```bash
herdr plugin install persiyanov/herdr-reviewr
```

Open it in the current workspace:

```bash
herdr plugin action invoke open --plugin persiyanov.reviewr
```

reviewr auto-opens in new worktrees. `auto_open = false` keeps it hidden until you ask
([Configuration](#configuration)).

**To update**, reinstall. Your config is keyed by plugin id and survives:

```bash
herdr plugin uninstall persiyanov.reviewr && herdr plugin install persiyanov/herdr-reviewr
```

**Without herdr**, reviewr runs as a plain terminal app. Grab a
[release binary](https://github.com/persiyanov/herdr-reviewr/releases/latest) and point it at a
repo:

```bash
herdr-reviewr ~/some/repo
```

Everything works except **Send** and the **last turn** scope. Those need herdr around.

## Quick start

Open reviewr next to your agent:

1. **Pick a file.** Changed files are in the navigator. `j` / `k` moves, the diff follows. Or
   `]` walks the changes hunk by hunk, file after file.
2. **Focus the diff.** `Tab` switches panes.
3. **Select lines.** `v`, then `j` / `k` to extend (or click or drag the gutter).
4. **Comment.** `c`, type, `Enter`.
5. **Send.** `s` sends every comment to the agent's input.

The footer shows the next step. Press `?` for every key that works right now.

For a shortcut, bind a key to the toggle in your herdr config (user config, not the plugin manifest):

```toml
[[keys.command]]
key = "cmd+r"
type = "plugin_action"
command = "persiyanov.reviewr.toggle"   # <plugin_id>.<action_id> — note the id, not the name
```

`cmd+…` chords reach herdr. Many macOS terminals swallow `alt+…` themselves.

## Controls

The keys below are defaults. You can rebind every action, even to several keys at once
([Keybindings](#keybindings)).

**Getting around**

| Key | Action |
| --- | --- |
| `1` `2` `3` | Switch tab — Changes / All files / PR |
| `u` `b` `t` | Switch scope — uncommitted / branch / last turn |
| `B` | Pick the branch scope's base |
| `j` `k` · `↑` `↓` | Move cursor |
| `]` `[` | Jump to next / previous hunk |
| `f` `F` | Jump to next / previous file |
| `PageUp` `PageDown` | Move a page |
| `Ctrl+U` `Ctrl+D` | Move a half-page |
| `Tab` | Switch focus |
| `→` `←` | Expand / collapse, or scroll sideways |
| `/` | Search files and code |
| `Ctrl+F` | Find in file |
| `w` | Toggle line wrap |
| `m` | Preview markdown file |
| `p` | Rotate navigator |
| `z` | Hide / show navigator |
| `<` `>` | Grow / shrink navigator |
| `r` | Refresh |
| `?` | Open shortcuts helper |
| `q` | Quit |

**Reviewing** (in the diff)

| Key | Action |
| --- | --- |
| `v` | Select lines |
| `c` | Comment on line or selection |
| `e` `d` | Edit / delete comment |
| `Ctrl+E` | Open the file under the cursor in `$EDITOR` — works from the search screen on the picked result |
| `n` `N` | Jump to next / previous comment |
| `l` | List all comments |
| `s` | Send comments to agent |
| `y` | Copy comments to clipboard |
| `esc` | Clear selection |

**In the comment box**

| Key | Action |
| --- | --- |
| `Enter` | Save comment |
| `Esc` | Cancel |
| `Shift+Enter` · `Alt+Enter` · `Ctrl+J` | Insert newline |

Plus the usual caret moves: arrows, `Home` / `End`, `Ctrl+A` / `Ctrl+E`, `Alt+b` / `Alt+f` word
jumps, and `Ctrl+W` / `Ctrl+U` / `Ctrl+K` deletes.

**PR tab** (read-only)

| Key | Action |
| --- | --- |
| `j` `k` | Move through description and comments |
| `PageUp` `PageDown` | Scroll focused pane |
| `o` | Open PR in browser |
| `r` | Refresh |

The mouse works too. Drag over any text to select and copy it, double-click a word,
triple-click a line. Click or drag the line-number gutter to comment. Click files, tabs, and
links, and scroll with the wheel.

## The three tabs

- **Changes** — the active scope's changed files with `+/-` stats and totals in the header.
- **All files** — any file's current content from the whole worktree, comments too. Ignored
  paths show dimmed.
- **PR** — a read-only mirror of the branch's pull request (GitHub, Azure DevOps) or merge
  request (GitLab): state, checks, description, and comments, rendered as markdown. reviewr
  never writes to the forge.

## Diff scopes

- **uncommitted** — the working tree vs `HEAD` (staged, unstaged, and untracked).
- **branch** — the working tree vs the merge-base with the base branch: **uncommitted** plus
  the branch's commits. The base is your repo's default branch until you pick another with
  `B` ([Base branch](#base-branch)).
- **last turn** — everything that changed in this worktree since its most recent turn started
  ([Limitations](#limitations)).

reviewr starts in **uncommitted**. `default_scope` changes that. Switching with `u`/`b`/`t`
wins for the rest of the session.

Every scope respects `.gitignore`, so build output never clutters **Changes**. To review a file,
track it. **All files** still browses any ignored path.

## Configuration

CLI flags on the pane command:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--poll <ms>` | `2000` | worktree poll interval (min `200`) |
| `--base <ref>` | auto | base for `branch` scope, any rev, overrides the pick |
| `--theme <name>` | `catppuccin` | UI + syntax theme (see below) |
| `--wrap <on\|off>` | `on` | soft-wrap long diff lines (`w` toggles at runtime) |

Everything else lives in reviewr's config file:

```text
~/.config/herdr/plugins/config/persiyanov.reviewr/config.toml
```

Create it if missing. It is reviewr's file. Settings in herdr's `~/.config/herdr/config.toml`
never reach it. reviewr re-reads it on every refresh and toggle, so edits apply without a
relaunch.

The file accepts these keys:

```toml
theme = "tokyo-night"
default_scope = "branch"
navigator_position = "right"
toggle_placement = "overlay"
toggle_direction = "down"
auto_open = false
github_host = "github.example.com"

[keybindings]
comment = ["c", "ㅊ"]
select  = ["v", "ㅍ"]
```

A missing file or omitted key uses its default. An invalid file is rejected whole — the pane
shows the error and recovers on the next refresh after you fix it.

### Theme

One theme colors the whole UI, chrome and syntax together:

```toml
theme = "tokyo-night"
```

`--theme` overrides the file. Match your terminal's light or dark background. Available:

- **Dark:** `catppuccin`, `catppuccin-frappe`, `catppuccin-macchiato`, `dracula`, `nord`,
  `gruvbox`, `one-dark`, `solarized`, `monokai`, `tokyo-night`, `rose-pine`.
- **Light:** `catppuccin-latte`, `gruvbox-light`, `one-light`, `solarized-light`,
  `github-light`, `tokyo-night-day`, `rose-pine-dawn`.

Names match herdr's where both ship a palette. An unknown name is an error. The standalone
`--theme` flag keeps its older fallback to `catppuccin`.

### Navigator position

The navigator starts on the right. Set `navigator_position` to `right`, `bottom`, `left`, or
`top`, or press `p` to cycle clockwise:

```toml
navigator_position = "bottom"
```

`<` grows, `>` shrinks, or drag the divider. `z` hides the navigator altogether and brings it
back.

### Base branch

The **branch** scope diffs against the merge-base with your repo's default branch, the one
`origin/HEAD` names. The header shows the resolved base, `vs main`.

When the trunk is something else, or you review a stacked branch, press `B` (or click the
base name) and pick the branch. The pick is stored in the repo, shared by every reviewr pane
on it, and holds until you pick again. Choosing the default branch clears it.

You can also type any revision — `HEAD~2`, a tag, a SHA prefix. Named spellings re-resolve
like git, a SHA pins its commit, and the header shows what resolved: `vs HEAD~2 (a1b2c3d)`.

`--base <ref>` sets the base for this pane and takes any rev (a branch, a tag, a SHA,
`HEAD~1`). It wins over the pick and disables the picker.

A picked branch that is gone (deleted after a stacked review, or a typo) is skipped, and the
header says so: `vs main · dev missing`. When nothing resolves, the scope stays empty and the
header reads `no base`, with the footer offering `B pick base`.

### Keybindings

`[keybindings]` maps an action name to an array of keys. The array replaces that action's
defaults, actions you don't mention keep theirs, and hints show the first key:

```toml
[keybindings]
comment = ["c", "ㅊ"]
select  = ["v", "ㅍ"]
```

Several keys per action serves CJK input sources — bind the character your layout produces
on the same physical key.

The action names and their defaults:

| Action | Default |
| --- | --- |
| `down` / `up` | `j` / `k` |
| `next-hunk` / `prev-hunk` | `]` / `[` |
| `next-file` / `prev-file` | `f` / `F` |
| `scope-uncommitted` / `scope-branch` / `scope-last-turn` | `u` / `b` / `t` |
| `base-pick` | `B` |
| `tab-changes` / `tab-all-files` / `tab-pr` | `1` / `2` / `3` |
| `wrap` | `w` |
| `preview` | `m` |
| `navigator-position` | `p` |
| `navigator-hide` | `z` |
| `navigator-grow` / `navigator-shrink` | `<` / `>` |
| `select` | `v` |
| `comment` | `c` |
| `edit` / `delete` | `e` / `d` |
| `next-comment` / `prev-comment` | `n` / `N` |
| `comments` | `l` |
| `search` | `/` |
| `find` | `ctrl+f` |
| `keys` | `?` |
| `send` | `s`, `S` |
| `copy` | `y`, `Y` |
| `open-pr` | `o` |
| `refresh` | `r` |
| `quit` | `q` |

A key is one printable character, or a `ctrl+`/`alt+` chord like `ctrl+f`. `Tab`, `Esc`, and
`Enter` are fixed. Keys still type normally in the comment box. Two actions sharing a key
invalidates the file. `list-wider` and `list-narrower` stay accepted as aliases for
`navigator-grow` and `navigator-shrink`.

### Forge repositories and hosts

A remote named exactly `upstream` with a recognized forge fetch URL wins. Otherwise the PR tab
reads `origin`. A standard fork clone works without setup.

GitHub.com, GitLab.com, dev.azure.com, and the `*.visualstudio.com` organization hosts work
without configuration. For one self-hosted instance per forge, set its bare hostname:

```toml
github_host = "github.example.com"
gitlab_host = "git.corp.example"
azure_devops_host = "tfs.corp.example"
```

Matching is exact. reviewr does not infer SSH aliases like `github.com-work` — use a
canonical-host remote or an `insteadOf` rewrite. Authenticate with
`gh auth login --hostname github.example.com`, `glab auth login --hostname git.corp.example`,
or `az login`.

### Pane placement

The toggle opens reviewr as a split to the right of your agent. `toggle_placement` changes the
shape:

```toml
toggle_placement = "overlay"   # split | overlay | zoomed | tab   (default: split)
toggle_direction = "down"      # right | down — split only        (default: right)
```

- **`split`** sits next to your agent. `toggle_direction` puts reviewr on the right (default) or below.
- **`overlay`** covers the tab. Toggle again to drop back.
- **`zoomed`** fills the tab.
- **`tab`** opens its own tab.

Every placement takes the keyboard on toggle. New worktrees auto-open only `split` and `tab`,
and never steal focus.

### Auto-open and layout plugins

reviewr auto-opens in every new worktree. `auto_open = false` makes it wait for the toggle:

```toml
auto_open = false   # default: true
```

Set this when a layout plugin like [herdr-plus](https://github.com/cloudmanic/herdr-plus)
arranges your new worktrees, so the two don't race.

A layout places reviewr like any other program. Give one pane the command:

```toml
command = "herdr-reviewr"
```

That pane is a full reviewr pane. The install links the binary at `~/.local/bin/herdr-reviewr`
and at `~/.local/state/herdr/plugins/persiyanov.reviewr/bin/herdr-reviewr`. Use the long path
if `~/.local/bin` is not on your `PATH`.

A layout hook can also invoke the actions, once its panes are in place:

```bash
herdr plugin action invoke open --plugin persiyanov.reviewr
```

`open` ignores `auto_open`, and both actions are safe to repeat. They target the focused
workspace. Put `herdr-reviewr` itself in a layout pane, never the invoke.

## Limitations

The known constraints:

**Terminal & theme**
- **Truecolor required** — colors are 24-bit RGB with no 256/8-color fallback. Basic terminals
  render wrong colors.
- **Theme must match the terminal** — the pane keeps the terminal's background, and there is no
  auto light/dark detection yet. You match the theme by hand.
- **Add / remove are red / green** — no secondary cue for colorblind users yet.
- **Box-drawing glyphs required**, but no Nerd Font.

**Platform**
- **macOS and Linux only** — no Windows.
- **Clipboard export** uses `pbcopy`, `wl-copy`, `xclip`, or `xsel`. With none installed it
  says so, and **Send** still works.

**herdr coupling**
- **Send needs an agent in the workspace** — one agent takes the comments straight away, and
  several open a picker so you choose. With no agent, Send says so and keeps your comments.
- **last turn relies on polling** (2 s default) — a turn that starts and finishes inside one
  poll is missed, and the scope shows everything since the last *observed* turn start, your
  own edits included.

**PR tab (GitHub, GitLab, and Azure DevOps)**
- **Read-only** — needs the forge's authenticated CLI (`gh`, `glab`, or `az`) and a
  recognized `upstream` or `origin`. Without either it tells you what to fix, and the other
  tabs keep working. Other forges are not supported.
- **One repository, never a cross-repository search** — a readable, recognized `upstream` is
  authoritative, otherwise `origin`. Clones that target different parent repositories stay
  separate.
- **Mirrors the branch's *open* PR or MR** — merged or closed shows as history. Each comment
  surface caps at its newest 100 rows, with a `+more` marker naming the forge when there is
  more.

**Review model**
- **Comments are in-memory and single-session** — closing the pane loses any you haven't sent
  or copied out.
- **Sending is all-or-nothing** — Send (or copy) delivers the whole set and clears it. A
  failure leaves everything in place.
- **No line-number rebasing** — a comment stays locatable by its diff snippet, not its line
  number. reviewr flags a stale comment instead of dropping it.
- **Two panes on one worktree drift a little** — they agree on turn boundaries, but each
  snapshots on its own poll clock, so their last-turn baselines can differ by the edits made
  between the two samples.

**Budgets**
- Files over 2 MB or 50,000 lines show a "too large" notice. Binary files get no diff.

## Building from source

For the dev setup, tests, and benchmarks, see [CONTRIBUTING.md](CONTRIBUTING.md). To run your
own build inside herdr panes, link the checkout. `herdr plugin link` runs the binary you build
at `bin/herdr-reviewr`:

```bash
git clone https://github.com/persiyanov/herdr-reviewr
cd herdr-reviewr
just install   # build release → bin/herdr-reviewr, ad-hoc re-signed on macOS
herdr plugin link .
```

After every `just install`, toggle the reviewr pane off and on. An open pane keeps running the old
process. The loop only works while the plugin is linked: a `github:…` source in
`herdr plugin list` runs a downloaded binary that local rebuilds never touch. Switch with:

```bash
herdr plugin uninstall persiyanov.reviewr   # config is keyed by id and survives
herdr plugin link .
```

## Roadmap

Structured (JSON) export, a side-by-side split view, mark-file-reviewed,
named-key notation for keybindings, OSC light/dark theme autodetect, more themes
(`kanagawa`, `vesper`, `everforest`, `ayu`, a dark `github`), a `terminal`-following palette,
and OSC 52 clipboard.

## Design

The living design is in [`specs/`](specs/), one concept per doc, always current.

## License

[MIT](LICENSE). Syntax highlighting comes from [syntect](https://github.com/trishume/syntect)
and [two-face](https://github.com/CosmicHorrorDev/two-face). Most themes' syntax colors come
from two-face's bundled set.

Bundled `.tmTheme` syntax files in `assets/`, each under its own license:

- [Catppuccin Mocha](https://github.com/catppuccin/bat) — MIT.
- [Tokyo Night](https://github.com/folke/tokyonight.nvim) (`tokyo-night`, `tokyo-night-day`) — Apache-2.0.
- [Rosé Pine](https://github.com/rose-pine/tm-theme) (`rose-pine`, `rose-pine-dawn`) — MIT.
