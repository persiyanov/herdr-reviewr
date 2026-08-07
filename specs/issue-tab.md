---
Status: Current
Created: 2026-08-07
Last edited: 2026-08-07
---

# Issue tab

A read-only GitHub Issues list in reviewr's frame: filter chip in the header, issues in the navigator, the selected body in the read pane.

## Overview

```
 1 Changes  2 All files  3 PR  4 Issue    #42 Fix the pane                [open]
╭─ #42 · @alice ─────────────────────────╮╭─ Issues · open ──────────────╮
│ labels: bug, ui                        ││ ● #42 Fix the pane        2d │
│                                        ││ ● #41 Docs polish         5d │
│ Details here…                          ││                              │
╰────────────────────────────────────────╯╰──────────────────────────────╯
 12 open issues   i open · o open ↗ · …                                    ?
```

v1 is **GitHub only** via `gh issue list`. No comment write, no edit, no close — those need write permissions and stay out of scope.

## Behavior

### Header and footer

- Tab label is `4 Issue` (rebindable as `tab-issue`).
- Right-anchored filter chip: `[open]` (default), `[closed]`, or `[all]`. Click or `i` (`issue-filter`) cycles open → closed → all → open.
- Selected issue title sits left of the chip when room allows.
- Footer leads with the filter action; `o open ↗` when a row is selected; go/move bands match the PR tab (line/page only).

### Navigator and read pane

- Navigator title `Issues · {filter}`; rows `#n title  age` with open/closed glyph
  (`●` green = open, `○` dim = closed). A title too wide for the row keeps its **head**
  and trails with `…` (not a path-style head elision).
- `j`/`k` or a click selects an issue; the read pane shows labels (if any) then the body as markdown.
- Empty body shows a dim italic placeholder. Empty list shows `No {filter} issues.`
- `o` opens the selected issue URL in the browser; `r` refetches.
- Authoring keys (`s`, `c`, `v`, `d`, `e`) are inert.

### Filter and refresh

- Default filter is **open** (all open issues in the repository).
- Fetch on tab entry, on filter change, on `r`, and on a slow fallback timer while the tab is active.
- A failed refetch keeps the last good list and shows a retry notice (same freeze rule as the PR tab).

### Degradation

- Missing `gh`, unauthenticated host, missing forge remote, unsupported host, and non-GitHub forges each render their own empty-state remedy.
- GitLab / Azure DevOps remotes say Issues are GitHub-only for now.

## Non-goals

- Writing comments, editing, closing, or assigning issues.
- Multi-forge issue providers.
- Linking issues to the current branch PR.

## Related specs

- [pr-tab](./pr-tab.md) — layout and interaction template
- [forge-host](./forge-host.md) — repository identity
- [input](./input.md) — keybindings
- [tui](./tui.md) — frame chrome
