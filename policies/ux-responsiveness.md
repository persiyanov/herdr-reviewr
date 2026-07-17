# UX Responsiveness Policy

## Intent

reviewr runs live beside a working agent, so every action must feel instant and any unavoidable delay must be signalled — never a silent stale-then-swap. Any change that touches rendering, loading, async data, or the event loop is evaluated for perceived latency and transition quality, not just correctness.

## Policy

- A user-initiated action paints its result within ~1 frame in the common (fast) case. It must not wait on a fixed timer, a coarse poll cap, or a full poll interval before the result appears.
- Never replace on-screen content with async-loaded content without first signalling that a load is happening. A silent stale-then-swap is a defect.
- Show a loading indicator only after a short delay (~150ms). Work that resolves faster shows no indicator, so an instant operation never flashes `loading…`.
- Never paint an internally inconsistent transitional frame. The header, file list, changed-count, and diff must agree; a label must not describe content that is not on screen yet.
- Refresh in place (a poll or `r`) keeps the current content and the cursor/scroll position — it updates without flicker, blanking, or a cursor jump.
- When new data is not ready, keep the last content and signal the refetch rather than blanking (the `PR` tab's "keep last, signal, never blank" discipline).
- No blocking external call (git, `gh`, the herdr CLI) runs on the event-loop or draw thread. Run it on a worker and deliver the result over a channel, so a slow or hung call never freezes input or rendering.
- Keep even fast interactive work off the keystroke path when it is avoidable: memoize session-fixed values and never rerun a subprocess per keystroke for something already known.

## Exceptions

- A genuinely slow or hung external call (git under a busy agent, `gh`) may show a delayed loading state and, while it is outstanding, wake the loop more often to deliver promptly.
- A large file or a first-visit diff may pay a one-time inline cost when async prefetch is not warranted; note it rather than building speculative machinery.
