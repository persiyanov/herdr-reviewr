---
name: reviewr-comments
description: Read, act on, and leave line-anchored review comments shared with the herdr-reviewr sidebar. Use when the user asks you to address their review comments, or to review code and leave comments in reviewr.
---

# reviewr comments

The reviewr sidebar and you share one comment store per worktree. Comments are anchored
to `file:start[-end]` with a verbatim diff snippet. The binary is
`$HERDR_PLUGIN_ROOT/bin/herdr-reviewr`; if that variable is unset, `herdr-reviewr` on
PATH or the path printed by whoever gave you this skill. Run every command from the
repo you are working in.

## Read the user's comments

    herdr-reviewr comment list            # open comments, human-readable, ids first
    herdr-reviewr comment list --json     # full documents

Trust the `lines` snippet over the line number — the code may have moved since the
comment was written. Find the snippet in the file, then act.

## The loop

1. `comment list` — see what's open.
2. Address each comment in code.
3. `herdr-reviewr comment resolve <id>` — mark it done. Do not resolve what you did
   not address; say so instead.
4. Leave your own notes where you changed or noticed something:

       herdr-reviewr comment add --file src/api.rs --start 25 \
         --lines '+  await KV.put(key, String(n + 1))' \
         --text "KV increments can lose concurrent updates"

   `--author agent` is the default; keep it. Notes render as cards in the user's
   sidebar within a second — no notification step is needed.

## Rules

- Never `comment rm` a user's comment; `resolve` is yours, `rm` is theirs.
- One comment per finding, at the tightest line range that shows it.
- Keep `--text` to a sentence or two; the diff is visible next to the card.
