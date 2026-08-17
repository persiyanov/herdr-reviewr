---
Status: Current
Created: 2026-07-17
Last edited: 2026-08-17
---

# PR tab

The tab shows a copy of the pull request in the reviewr frame. The tab only reads. The header shows the identity. The navigator shows the checks and the comments. The read pane shows the selected body.

## Overview

The navigator shows the checks. The navigator selects the description or a comment. The read pane shows that selection. The header shows the identity and the state of the PR. The tab reads the forge of the repository (`forge-host.md`). The tab does not write.

The only action that goes out opens a link in the browser.

The name of the tab is `PR` on each forge. The body text, the chip, the title of the read pane, and the footer use the words of the selected forge (`forge-providers.md`). A repository that has no forge uses the default words. A `finding` that has no code context shows only its body.

```
 1 Changes  2 Files  3 PR    Deep research: GPT-5.5/5.4-mini upgrade…  deep-research  merged #226 ↗
╭─ @codex · manager.py:114-115 ─────────────────────────╮╭─ Checks & comments ──────────╮
│ 114    if primary_result.status == PERM_FAILURE:      ││ description                  │
│ 115 ▌     return primary_result                       ││                              │
│                                                       ││ checks  ✗ 1 failing          │
│ Avoid falling back after target permanent failures.   ││  ✓ build-main-image          │
│ This now attempts a fallback for every non-success…   ││  ✗ tests                     │
│                                                       ││                              │
│                                                       ││ comments · 5                 │
│                                                       ││ @you    comment          5m  │
│                                                       ││▍@codex  manager.py:114-115 2h│
│                                                       ││ @claude review           2h  │
│                                                       ││ @claude manager.py:39    2h  │
│                                                       ││ @claude parse.py:187 outdated│
╰───────────────────────────────────────────────────────╯╰─────────────────────────────╯
 ⚠ conflicts with main · ⇡ 2 unpushed · ✗ 1 failing · 5 comments   o open ↗                            ?
```

## Behavior

### Header and footer

- The header puts a `status #226 ↗` chip on the right. You can click the chip.
- The color of the status is the color of the life cycle: `open` is green, `draft` is yellow, `merged` is mauve, `closed` is red.
- The `draft` status shows only while the PR is open.
- The title of the PR is on the left of the chip. The tab cuts the title if the title is too long.
- The selected head branch (`head_ref`, `forge-host.md`) is between the title and the chip. The branch is dim.
- If the head is in a fork, the branch has the prefix `⑂ `.
- If the bar is too narrow, the tab removes the branch first.
- The footer starts with the merge state, the sync state, the check state, and the comment count. Then the footer shows `o open ↗` and the `?`.
- The merge state and the sync state show only while the PR is open.
- If a list has more pages, the footer adds a `+more ↗` link. The link names the forge (`forge-host.md`).
- The `?` opens the `go` band and a `move` band. The `move` band has down, up, and the page keys.
- The `PR` tab has no hunk step and no file step (`input.md`).
- If there is no pull request, the body shows only `No pull request yet. Ready to ship?`
- If HEAD is detached, the body shows `No pull request found — HEAD is detached.`
- The two messages use the noun of the forge.

### Navigator and read pane

- The title of the navigator is `Checks & comments`.
- The navigator shows a checks section that has only status. That section is above the comment list.
- The cursor moves on the description row and on the comments.
- The comment list has the newest comment first. Each row is `@author anchor age`.
- If the forge set the thread to `outdated` or `resolved`, the row shows that mark.
- If the PR description is not empty, a `description` row stays at the top of the navigator. The row is above the checks.
- If the description becomes empty, the row goes. The cursor goes to a valid row. The read pane starts again.
- The read pane shows the selected item.

| selected item              | the read pane shows                          |
| -------------------------- | -------------------------------------------- |
| a `finding`                | the range caption, the snippet, then the body |
| a `review` or a `comment`  | the text of the comment                      |
| the description row        | the PR description                           |

- Bodies show as markdown (`markdown.md`).
- A finding with a range shows a caption above the snippet. A one-line range shows `Comment on line N`. A wider range shows `Comment on lines A to B`. The navigator does not show this caption.
- The caption puts `-` when the side is the old file. It puts `+` when the side is the new file and the range has insertions. It puts no sign when the new-file range has no change row.
- A finding shows the line range of the comment as Diff-view content rows (`diff-view.md`). Then the finding shows the body.
- The range is the start line and the end line from the forge.
- The range uses the finding's side. It uses new-file numbers when the side is the new file. It uses old-file numbers when the side is the old file. A numbered snippet with no side uses the new file.
- A subject-side row stays when that side's number is in the range or in the three-line margin. A paired change on the other side stays only when it sits in the same replace block as a subject-side row in the range.
- A comment on one line is a range of one line.
- The tab shows the full range.
- The tab shows three lines above the range and three lines below the range. Those lines come from the stored hunk. A hunk that has fewer lines shows the lines that it has.
- If the range is higher than the pane, the pane scrolls.
- The tab does not show a line that is more than three lines from the range.
- If a finding has no snippet, the tab shows the caption and the body.
- If the hunk cannot put the range, the tab shows the caption and the body.
- reviewr does not get a hunk. reviewr does not make a hunk.
- The rows are the unified-diff lines of the snippet that are in the range or in the three-line margin. There are no folds.

This table shows how each snippet line shows:

| snippet line                  | shows as                           |
| ----------------------------- | ---------------------------------- |
| ` ` / `+` / `-` prefix        | context / insertion / deletion row |
| `@@` header                   | nothing                            |
| `\ No newline at end of file` | nothing                            |
| a different line, and no `@@` | a context row with no line number  |

- Each `@@` header sets the line numbers of the rows after that header.
- If a snippet has no header that the tab can read, the tab shows no line numbers. The tab still shows the tints, the highlight, and the word emphasis.
- The language of the syntax is the language of the path in the finding anchor.
- If the path has no known language, the lines are plain. This is the same as the Diff view.
- Each line number in the gutter is the number on the side that the range uses. A number on the comment's side in the range has the comment color (`diff-view.md`). A paired change on the other side does not. A number in the margin does not.
- The rows that show are the comment range and the three-line margin. You cannot put the cursor on these rows. You cannot select these rows. You cannot put a comment on these rows.
- A dim horizontal rule sits between the snippet and the body.
- Long snippet lines wrap as the markdown body wraps. The wrap key does not operate on this tab.
- A snippet that is not correct still shows. The tab does not make the body empty.
- The tab shows a human author more than a bot.
- The `j` key, the `k` key, or a click selects a description or a comment. The navigator shows the selected row. You cannot select a check.
- The wheel on the navigator scrolls the navigator view. The selection does not change.
- The wheel on the read pane scrolls the read pane.
- The `page-up` and `page-down` bindings scroll the pane that has focus (`input.md`).
- Each pane stops when the last line is at the bottom edge.
- The `o` key or the chip opens the PR in the browser.
- If a body is higher than the read pane, a scrollbar shows on the right border of the pane. If the body fits, there is no scrollbar.
- A retry notice for a snapshot that the tab kept stays above the read body. The notice stays in view. The scroll of the reader does not move the notice.
- The keys `s`, `c`, `v`, `d`, and `e` do not operate on this tab.
- A merged PR or a closed PR shows the same copy. The tab only reads.
- If there is no forge CLI that can operate, the tab shows the failure from `forge-host.md`. The failure names the command that lets you continue.

### Refresh

The tab gets a new snapshot when one of these occurs:

| when                                            | the tab        |
| ----------------------------------------------- | -------------- |
| the tab opens                                   | starts a fetch |
| the user enters the tab                         | starts a fetch |
| the user presses `r`                            | starts a fetch |
| a turn ends in the worktree, on any tab         | starts a fetch |
| the tab is active and the fallback timer ends   | starts a fetch |

- The tab does one fetch when a turn ends (`herdr-host.md`, HH-TURN-PER-WORKTREE). This keeps the tab current before the user enters the tab.
- If a new trigger occurs during a fetch, the tab keeps that fetch. The tab shows the result. Then the tab does one more fetch.
- If the user presses `r`, the tab stops the fetch that is in progress. Then the tab starts a new fetch.
- If a fetch does not complete in one minute, the tab stops that fetch. Then the tab starts a new fetch.
- The time of the PR fetch is not the time of the worktree poll (`tui.md`).
- A new fetch keeps the position of the user. The cursor stays on the same comment by identity. The two panes keep their scroll positions.
- If the comment is gone, the tab sets the cursor to a valid row. The read pane starts again.

## Non-goals

- The tab does not jump from the anchor of a PR comment to the code tabs.
- The tab does not change how a comment body shows. The body stays markdown. The reply count stays dim. There is no card frame.
- The tab does not make a hunk again from the worktree.
- The tab does not make a hunk from GitLab or Azure position data.
- The tab does not make a hunk from one more forge call.
- The tab does not show hunk lines that are not in the line range of the comment.

## Related specs

- [forge-host](./forge-host.md)
- [forge-providers](./forge-providers.md)
- [tui](./tui.md)
- [input](./input.md)
- [markdown](./markdown.md)
- [diff-view](./diff-view.md)
