# herdr-reviewr

A native terminal **code-review sidebar** for [herdr](https://herdr.dev) — review a coding
agent's changes in a right-hand pane and send line comments back to the chat, without leaving
the terminal.

![demo](assets/demo.gif)

## What it does

A persistent split pane beside your agent, pointed at one git worktree:

- **Changes tab** — the changed files for the active scope, with `+/-` stats; pick a file to
  read its syntax-highlighted diff.
- **All files tab** — browse the whole worktree tree, not just what changed; the same diff pane
  renders any file's current content. Git-ignored paths show too, dimmed — a wholly-ignored
  directory (`target/`, `node_modules/`) is one collapsed row that loads its contents only when
  you expand it.
- **Comments** — select a line range in a diff, write a comment, repeat. The comment list is a
  surface of its own.

…and the core loop:

> select a line range → write a comment → repeat → **Add all to chat** → each comment lands in
> the agent's input as `path:start-end — comment`, ready for you to add context and send.

It **never edits your worktree** and sends nothing on its own. Its only git write is a private
`last-turn` baseline ref under `refs/reviewr/`.

### Diff scopes

- **uncommitted** — working tree vs `HEAD` (staged, unstaged, and untracked).
- **branch** — the working tree vs the merge-base with the base branch (`origin/main` →
  `origin/master` → `main` → `master`, or `--base`); a superset of **uncommitted** that adds the
  branch's committed work.
- **last turn** — only what the agent changed since its most recent turn started (see
  [Limitations](#limitations)).

## Requirements

- **herdr ≥ 0.7.0** (the plugin system).
- **git** on `PATH`.
- A **truecolor (24-bit), dark** terminal with Unicode box-drawing support.
- macOS or Linux.

## Install

From the herdr marketplace (downloads a prebuilt binary — no Rust toolchain needed):

```bash
herdr plugin install persiyanov/herdr-reviewr
```

Then open the sidebar with the **reviewr: toggle sidebar** action (bind it in your herdr config),
or let it auto-open on `worktree.created`.

### From source (for contributors)

`herdr plugin link` skips the download build step, so place a locally built binary where the
pane command looks for it — `$HERDR_PLUGIN_ROOT/bin/herdr-reviewr`:

```bash
git clone https://github.com/persiyanov/herdr-reviewr
cd herdr-reviewr
just install   # build release → bin/herdr-reviewr, ad-hoc re-signed on macOS
herdr plugin link .
```

`just install` (re)places the binary with a fresh file and ad-hoc re-signs it. On Apple Silicon
that matters: overwriting a code-signed binary in place invalidates its signature, and macOS then
SIGKILLs it at launch — so a plain `cp target/release/herdr-reviewr bin/` makes the sidebar pane
open and close instantly.

**Iterating on changes.** The dev loop after the first link is:

1. Edit the code.
2. `just install` — rebuilds the binary and re-signs it under `bin/`.
3. **Relaunch the sidebar** — toggle it off and back on with your keybind. The open pane keeps
   running the *old* process until it is relaunched, so a rebuild alone changes nothing on screen.

This only works while the plugin is **linked**, not installed from the marketplace. Check with
`herdr plugin list`: a `github:…` source means the pane runs a *downloaded* binary under
`~/.config/herdr/plugins/github/`, so your local rebuilds never appear no matter how often you
`just install`. Switch a GitHub install to a dev link with:

```bash
herdr plugin uninstall persiyanov.reviewr   # config is keyed by id and survives
herdr plugin link .
```

## Configuration

CLI flags on the pane command:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--poll <ms>` | `2000` | worktree poll interval (min `200`) |
| `--base <ref>` | auto | base branch for `branch` scope |
| `--theme <name>` | Catppuccin Mocha | **syntax** theme (structural UI colors are fixed) |
| `--wrap` | off | soft-wrap long diff lines |

Every scope respects `.gitignore`, so build output never clutters **Changes**. To review a
file, track it in git — an ignored-but-intentional file (a plan, a sample env) belongs in the
repo, where it shows as a change and ages out once committed. **All files** can still browse any
ignored path (dimmed), even untracked ones.

## Limitations

This is a focused v0.1. Known constraints, honestly:

**Terminal & theme**
- **Truecolor required** — colors are 24-bit RGB with no 256/8-color fallback; basic terminals
  render wrong.
- **Dark terminal assumed** — the structural UI colors are **hardcoded to Catppuccin Mocha**.
  `--theme` only swaps the *syntax* theme (and silently falls back on an unknown name); there is
  no light theme and no configurable UI palette.
- **Add/remove are distinguished by red/green** — no secondary cue for colorblind users yet.
- Unicode box-drawing glyphs are required (no Nerd Font needed).

**Platform**
- **macOS and Linux only** (no Windows).
- **Clipboard export** uses `pbcopy` (macOS) or `wl-copy`/`xclip`/`xsel` (Linux); if none is
  installed it errors clearly — use **Add all to chat** instead. (OSC 52 and Windows are roadmap.)

**herdr coupling**
- **Add all to chat** needs a resolvable agent pane (the agent in your tab, or the sole agent in
  the workspace); otherwise it no-ops and keeps your comments. Browsing and diffing need no herdr.
- **last turn is poll-based** (2 s default): a turn that starts and finishes inside one poll is
  never snapshotted on its own, so the scope shows everything since the last *observed* turn start
  — never lines the agent didn't write, but possibly more than one turn.

**Review model**
- **Comments are in-memory and single-session** — closing the pane loses any not yet exported.
- **Bulk export only**, consume-on-success: a send delivers the whole set and clears it (no
  duplicates, no per-comment send); a failed send leaves everything in place.
- **No line-number rebasing** — a comment's diff snippet, not its line number, keeps it locatable;
  stale comments are flagged, never silently dropped.
- **One sidebar per worktree** (two on the same worktree race the baseline ref, last-writer-wins).

**Budgets**
- Files over **2 MB** or **50,000 lines** show a "too large" notice; **binary** files aren't
  diffed.

## Roadmap

A Checks/CI tab (`gh` PR + CI status), a config file, customizable keybindings, structured
(JSON) export, in-diff search, a side-by-side split view, mark-file-reviewed, a selectable/light
UI theme, and OSC 52 clipboard.

## Design

The living design lives in [`specs/`](specs/) — one concept per doc, always current.

## License

[MIT](LICENSE). Bundled syntax theme: [Catppuccin Mocha](https://github.com/catppuccin/bat)
(`assets/Catppuccin Mocha.tmTheme`, MIT). Syntax highlighting via
[syntect](https://github.com/trishume/syntect) and [two-face](https://github.com/CosmicHorrorDev/two-face).
