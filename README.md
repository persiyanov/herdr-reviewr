# herdr-reviewr

A code-review sidebar for [herdr](https://herdr.dev). Your agent writes the code; you read its
diff in a pane beside the chat, leave comments on the lines, and send the notes back — without
leaving the terminal.

![demo](assets/demo.gif)

What you get, in one persistent pane pointed at a git worktree:

- **A diff to review** — the agent's changed files, syntax-highlighted, scoped to *uncommitted*,
  *branch*, or *last turn*.
- **Line comments that stay put** — select a range, write a note; it renders as a card under the
  code instead of hiding behind a marker.
- **One keystroke back to the agent** — **Send** drops every comment into the agent's input as
  `path:start-end — comment`, ready for you to add context and hit enter.
- **More when you need it** — browse the whole worktree, not just the diff, and read the branch's
  open pull request without switching windows.

It **never edits your worktree** and sends nothing on its own. Its only write to git is a private
`last-turn` baseline ref under `refs/reviewr/`. The **PR** tab reads GitHub but never posts there.

## Requirements

- **herdr ≥ 0.7.0** (the plugin system).
- **git** on `PATH`.
- A **truecolor (24-bit), dark** terminal with Unicode box-drawing support.
- **macOS or Linux.**
- **`gh`** (the GitHub CLI), authenticated — *optional*, only for the **PR** tab. Everything else
  works without it.

## Install

From the herdr marketplace — a prebuilt binary, no Rust toolchain:

```bash
herdr plugin install persiyanov/herdr-reviewr
```

The sidebar **auto-opens for a newly created worktree** — installing the plugin is enough. To
toggle it on demand, bind a key to the **reviewr: toggle sidebar** action in your herdr config
(keybindings live in user config, not the plugin manifest):

```toml
[[keys.command]]
key = "cmd+r"
type = "plugin_action"
command = "persiyanov.reviewr.toggle"   # <plugin_id>.<action_id> — note the id, not the name
```

`cmd+…` chords reach herdr; macOS swallows `alt+…`. With no key bound, run it once with
`herdr plugin action invoke toggle --plugin persiyanov.reviewr`.

## Quick start

The core loop takes five keys. Open the sidebar next to your agent and:

1. **Pick a file.** The agent's changed files are in the right pane. `j` / `k` moves the cursor;
   the diff opens on the left as you go.
2. **Focus the diff.** Press `Tab` to move from the file list into the diff.
3. **Select the lines.** Press `v`, then `j` / `k` to extend the selection (or click-drag).
4. **Comment.** Press `c`, type your note, `Enter` to save. It stays on screen as a card under
   the line.
5. **Send.** When you're done, press `s`. Every comment lands in the agent's input as
   `path:start-end — comment` — you add context and send.

The footer always shows the keys that work right now, so you can learn it by using it. The tables
below are the full reference.

## Controls

**Getting around**

| Key | Action |
| --- | --- |
| `1` `2` `3` | Switch tab — Changes / All files / PR |
| `u` `b` `t` | Switch scope — uncommitted / branch / last turn |
| `j` `k` · `↑` `↓` | Move the cursor in the focused pane |
| `PageUp` `PageDown` | Move a page · `Ctrl+U` `Ctrl+D` move a half-page |
| `Tab` | Switch focus between the file list and the diff |
| `→` `←` | Expand / collapse a directory or expand a fold; otherwise scroll the diff sideways |
| `w` | Toggle line wrap |
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
| `Enter` | Save · `Esc` cancel |
| `Shift+Enter` · `Alt+Enter` · `Ctrl+J` | Insert a newline |

Plus the usual caret moves: arrows, `Home` / `End`, `Ctrl+A` / `Ctrl+E`, word-jump with
`Alt+b` / `Alt+f`, and `Ctrl+W` / `Ctrl+U` / `Ctrl+K` to delete by word or to the line edge.

**PR tab** (read-only)

| Key | Action |
| --- | --- |
| `j` `k` | Move through checks and comments |
| `PageUp` `PageDown` | Scroll the selected comment |
| `o` | Open the PR in your browser |
| `r` | Refresh |

herdr is mouse-native, so clicking a file, dragging to select lines, clicking a tab or the `Send`
button, and the scroll wheel all work too.

## The three tabs

- **Changes** — the changed files for the active scope, with `+/-` stats; pick a file to read its
  syntax-highlighted diff. This is where you review and comment.
- **All files** — browse the whole worktree tree, not only what changed; the diff pane renders any
  file's current content. Git-ignored paths show too, dimmed — a wholly-ignored directory
  (`target/`, `node_modules/`) is one collapsed row that loads its contents only when you expand
  it. You can comment here as well.
- **PR** — a read-only mirror of the branch's open pull request, read from GitHub via `gh`: its
  state (draft / open / merged / closed, mergeability, unpushed-commit sync), its checks with a
  pass/fail rollup, and its comments (reviews, inline findings, plain comments, newest first, with
  `resolved` / `outdated` markers). `o` opens it in the browser. It only reads GitHub — never
  posts, resolves, re-runs, or merges.

## Diff scopes

- **uncommitted** — the working tree vs `HEAD` (staged, unstaged, and untracked).
- **branch** — the working tree vs the merge-base with the base branch (`origin/main` →
  `origin/master` → `main` → `master`, or `--base`); a superset of **uncommitted** that adds the
  branch's committed work.
- **last turn** — only what the agent changed since its most recent turn started (see
  [Limitations](#limitations)).

Every scope respects `.gitignore`, so build output never clutters **Changes**. To review a file,
track it in git — an ignored-but-intentional file (a plan, a sample env) belongs in the repo,
where it shows as a change and ages out once committed. **All files** can still browse any ignored
path, dimmed, even untracked ones.

## Configuration

CLI flags on the pane command:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--poll <ms>` | `2000` | worktree poll interval (min `200`) |
| `--base <ref>` | auto | base branch for `branch` scope |
| `--theme <name>` | Catppuccin Mocha | **syntax** theme (structural UI colors are fixed) |
| `--wrap <on\|off>` | `on` | soft-wrap long diff lines (`w` toggles at runtime) |

## Limitations

This is a focused, young tool. The known constraints, honestly:

**Terminal & theme**
- **Truecolor required** — colors are 24-bit RGB with no 256/8-color fallback; basic terminals
  render wrong.
- **Dark terminal assumed** — the structural UI colors are **hardcoded to Catppuccin Mocha**.
  `--theme` swaps only the *syntax* theme (and falls back silently on an unknown name); there is no
  light theme and no configurable UI palette.
- **Add / remove are red / green** — no secondary cue for colorblind users yet.
- Unicode box-drawing glyphs are required (no Nerd Font needed).

**Platform**
- **macOS and Linux only** — no Windows.
- **Clipboard export** uses `pbcopy` (macOS) or `wl-copy` / `xclip` / `xsel` (Linux); if none is
  installed it says so and you use **Send** instead. (OSC 52 and Windows are roadmap.)

**herdr coupling**
- **Send** needs a resolvable agent pane — the agent in your tab, or the sole agent in the
  workspace; otherwise it no-ops and keeps your comments. Browsing and diffing need no herdr.
- **last turn is poll-based** (2 s default): a turn that starts and finishes inside one poll is
  never snapshotted on its own, so the scope shows everything since the last *observed* turn start
  — never lines the agent didn't write, but possibly more than one turn.

**PR tab (GitHub)**
- **GitHub-only and read-only** — needs an authenticated `gh` and a GitHub remote; without either
  it shows one remediation line and the rest of the app (Changes, All files) is unaffected.
- **Mirrors only the branch's *open* PR** — a merged or closed PR shows as history; comment
  surfaces are capped at one page (100 rows each), with a `+more on GitHub ↗` marker when there's
  more.

**Review model**
- **Comments are in-memory and single-session** — closing the pane loses any you haven't sent or
  copied out.
- **Bulk only, consume-on-success** — Send (or copy-to-clipboard) delivers the whole set and clears
  it: no duplicates, no per-comment send. A failure leaves everything in place.
- **No line-number rebasing** — a comment's diff snippet, not its line number, keeps it locatable;
  stale comments are flagged, never silently dropped.
- **One sidebar per worktree** — two on the same worktree race the baseline ref, last writer wins.

**Budgets**
- Files over **2 MB** or **50,000 lines** show a "too large" notice; **binary** files aren't
  diffed.

## Building from source

For contributors. `herdr plugin link` skips the download build step, so place a locally built
binary where the pane command looks for it — `$HERDR_PLUGIN_ROOT/bin/herdr-reviewr`:

```bash
git clone https://github.com/persiyanov/herdr-reviewr
cd herdr-reviewr
just install   # build release → bin/herdr-reviewr, ad-hoc re-signed on macOS
herdr plugin link .
```

`just install` replaces the binary with a fresh file and ad-hoc re-signs it. On Apple Silicon that
matters: overwriting a code-signed binary in place invalidates its signature, and macOS then
SIGKILLs it at launch — so a plain `cp target/release/herdr-reviewr bin/` makes the pane open and
close instantly.

**The dev loop** after the first link:

1. Edit the code.
2. `just install` — rebuilds and re-signs the binary under `bin/`.
3. **Relaunch the sidebar** — toggle it off and back on with your keybind. The open pane keeps
   running the *old* process until you relaunch it, so a rebuild alone changes nothing on screen.

This works only while the plugin is **linked**, not installed from the marketplace. Check with
`herdr plugin list`: a `github:…` source means the pane runs a *downloaded* binary under
`~/.config/herdr/plugins/github/`, so local rebuilds never appear no matter how often you
`just install`. Switch a GitHub install to a dev link:

```bash
herdr plugin uninstall persiyanov.reviewr   # config is keyed by id and survives
herdr plugin link .
```

## Roadmap

A config file, customizable keybindings, structured (JSON) export, in-diff search, a side-by-side
split view, mark-file-reviewed, a selectable / light UI theme, and OSC 52 clipboard.

## Design

The living design lives in [`specs/`](specs/) — one concept per doc, always current.

## License

[MIT](LICENSE). Bundled syntax theme: [Catppuccin Mocha](https://github.com/catppuccin/bat)
(`assets/Catppuccin Mocha.tmTheme`, MIT). Syntax highlighting via
[syntect](https://github.com/trishume/syntect) and [two-face](https://github.com/CosmicHorrorDev/two-face).
