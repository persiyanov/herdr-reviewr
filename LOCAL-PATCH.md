# Local patch: per-tab toggle

This checkout is linked into herdr (`herdr plugin link`).

## What changed

`herdr/pane.sh` scopes `toggle` / `open` / `close` to the **current herdr tab**
when `HERDR_TAB_ID` is set and `toggle_placement` is not `tab`.

- Each tab can keep its own reviewr instance.
- Toggle in one tab does not close reviewr in another tab.
- `toggle_placement = "tab"` still uses workspace scope (reviewr is a sibling tab).
- `auto_open` / `worktree.created` stays workspace-scoped.

## Update from upstream

```bash
cd /Users/zhuo/Developer/forks/herdr-reviewer
git fetch origin
git rebase origin/main
# if pane.sh conflicts: keep per-tab logic, take upstream for the rest
# release binary after build:
#   just install   # or cargo build --release && herdr plugin link .
```

Config is separate and survives:

`~/.config/herdr/plugins/config/persiyanov.reviewr/`

## Switch back to official

```bash
herdr plugin uninstall persiyanov.reviewr
herdr plugin install persiyanov/herdr-reviewr
```
