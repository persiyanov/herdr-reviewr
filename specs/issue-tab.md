---
Status: Current
Created: 2026-08-07
Last edited: 2026-08-07
---

# Issue tab

A read-only GitHub Issues list in reviewr's frame: filter chips in the header, issues in the navigator, the selected body in the read pane.

## Overview

```
 1 Changes  2 All files  3 PR  4 Issue    #42 Fix the pane     [open] [all] [any]
╭─ #42 · @alice ─────────────────────────╮╭─ Issues · open ──────────────╮
│ labels: bug, ui                        ││ ● #10 Epic              1/2 2d│
│                                        ││   ● #12 child a            1d│
│ Details here…                          ││   ● #13 child b            5h│
╰────────────────────────────────────────╯╰──────────────────────────────╯
 12 open issues   i open · a all · L any · o open ↗ · …                    ?
```

v1 is **GitHub only** via `gh issue list`. No comment write, no edit, no close — those need write permissions and stay out of scope.

## Behavior

### Header and footer

- Tab label is `4 Issue` (rebindable as `tab-issue`).
- Right-anchored filter chips (state, assignee, priority), each clickable:
  - State `[open]` (default) / `[closed]` / `[all]` — `i` (`issue-filter`)
  - Assignee `[all]` (default) / `[mine]` — `a` (`issue-assignee`); `mine` is `gh --assignee @me`
  - Priority `[any]` (default) / `[p0]` / `[p1]` / `[p2]` — `L` (`issue-priority`); matches that label name case-insensitively via `gh --label`
- Selected issue title sits left of the chips when room allows.
- Footer row 1: primary `i {state}`, then `a {assignee}` and `L {priority}` as actions; `o open ↗`
  when a row is selected. go/move bands match the PR tab (line/page only).

### Navigator and read pane

- Navigator title `Issues · {state}` plus ` · mine` / ` · pN` when those filters are active; rows `#n title  age` with open/closed glyph
  (`●` green = open, `○` dim = closed). A title too wide for the row keeps its **head**
  and trails with `…` (not a path-style head elision).
- **Parent / sub-issues.** Fetched via `parent` and `subIssuesSummary` on `gh issue list`. The list
  is reordered so each child sits under its parent when both are in the current result (one
  nesting level, two-space indent). A child whose parent is missing from the list (e.g. open
  filter, closed parent) stays top-level. Parents with sub-issues show `done/total` before the age.
- `j`/`k` or a click selects an issue; the read pane leads with the **full title** in markdown H1
  style (bold mauve, wrapped — the navigator may have truncated it), then a dim parent/sub
  summary when relevant, then labels (if any), then the body as markdown.
- Empty body shows a dim italic placeholder. Empty list shows `No {query} issues.`
- `o` opens the selected issue URL in the browser; `r` refetches (bypasses the cache).
- Authoring keys (`s`, `c`, `v`, `d`, `e`) are inert. The comments-list key is free for other tabs; priority uses `L` so it does not steal `l`.

### Filter and refresh

- Default query: **open**, all assignees, any priority.
- Filters compose into one `gh issue list` call (`--state`, optional `--assignee @me`, optional `--label pN`).
- **Cache.** Successful list results are keyed by the full query (and repository identity). Within a **1 minute** TTL:
  - Re-entering the Issue tab with the same query paints the cache and does **not** call `gh`.
  - Cycling a filter paints a cached result for the new query immediately when present and fresh; only a miss or expiry starts a fetch.
- Fetch triggers: first need (no fresh cache), filter change without a fresh cache entry, `r` (always), and a slow fallback timer (**1 minute**) while the tab is active and the current query's entry is stale or missing.
- A failed refetch keeps the last good list for the active query and shows a retry notice (same freeze rule as the PR tab).

### Degradation

- Missing `gh`, unauthenticated host, missing forge remote, unsupported host, and non-GitHub forges each render their own empty-state remedy.
- GitLab / Azure DevOps remotes say Issues are GitHub-only for now.

## Non-goals

- Writing comments, editing, closing, or assigning issues.
- Multi-forge issue providers.
- Linking issues to the current branch PR.
- Arbitrary label pickers or multi-label OR queries beyond the p0/p1/p2 cycle.
- Resolving the signed-in login for display (assignee filter uses `@me`).
- Expand/collapse of sub-issue trees, multi-level nesting, or extra fetches for missing parents.

## Related specs

- [pr-tab](./pr-tab.md) — layout and interaction template
- [forge-host](./forge-host.md) — repository identity
- [input](./input.md) — keybindings
- [tui](./tui.md) — frame chrome
