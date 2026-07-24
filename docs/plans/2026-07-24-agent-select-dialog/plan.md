# Agent select dialog on ambiguous Send — Delivery Plan

**Specs:** ../../../specs/ — the living reference this plan delivers
**Base:** `origin/main` (ahead of the 0.20.1 release: has `Search`/`Find` modes and the
`footer_bands`/`Band` footer). Fork: `shumkov/herdr-reviewr` (remote `fork`).

## Problem

`Send` (`s`) resolves its target with `pick` in `src/herdr.rs`: the sole agent in the
sidebar's tab, else the sole agent in its workspace. Zero **or several** candidates
refuse the send (`HH-SOLE-OR-REFUSE`) with *"several agents here — copy to the clipboard
instead."* The reviewer is then forced onto `Copy`, which writes the OS clipboard on the
machine the binary runs on. Over `mosh` (Warp → mosh → herdr on a remote Mac), that is
the *remote* clipboard; a local paste pulls the *local* clipboard, so the comments never
reach the agent (no OSC 52; "No clipboard over SSH" is an explicit non-goal in
`herdr-host.md`). The fix is to keep the reviewer on **Send** — which writes straight
into the agent pane over the herdr socket and never touches a clipboard — even with
several agents in scope.

## Goal

When `Send` resolves to **several** candidate agents, open a modal **agent picker**
listing those agents; the reviewer chooses one; every comment is sent to that agent's
pane (write input, then focus), consumed on success — exactly as a one-agent send today.
Zero candidates still refuses with the clipboard hint; exactly one still sends directly.

## Definition of Done

- `Send` with **one** resolved agent behaves exactly as today (direct send, no picker).
- `Send` with **several** candidates opens the picker instead of refusing.
- `Send` with **zero** candidates reports *"no agent here — copy to the clipboard
  instead"* and opens no picker. (Status wording note: today this renders with an
  `agent failed:` prefix because the zero case flows through `export`; the new path sets
  the status directly and drops the prefix — a deliberate micro-change, noted in the spec.)
- `Send` with **no comments** still reports *"no comments to send"* and never opens the
  picker (the empty-store check precedes agent resolution).
- In the picker: the highlight moves through the `down`/`up` **bindings** (default `j`/`k`
  and the fixed `↓`/`↑`), `Enter` sends every comment to the highlighted agent and closes,
  `Esc` cancels and keeps every comment. No new rebindable action is added, so the keymap
  and `CFG-KEY-UNIQUE` are untouched — the picker *acts through the existing bindings*,
  exactly as the comments list does (`input.md`).
- The picker never lists the reviewer's own pane (`HH-NOT-SELF`) or a non-agent pane
  (`HH-AGENT-PANES`); it lists **exactly** the ambiguous candidate set.
- Each row identifies its agent at the **pane** level — agent ≠ tab (a tab can hold several
  agents, or an agent plus a shell), and confirmed live: agent `name` is `∅` on nearly all
  agents, pane `label` is `∅` on every agent pane (only plugin panes set it), and `cwd` is
  identical within a worktree. The row is composed and **omits every empty part**:
  `{name or kind} · {tab-label, shown only when it differs from the kind} · {status} — {title}`
  (title = `terminal_title_stripped`, the live "what it's working on"). The pane id is hidden;
  `pane_id` is retained **internally** as the send target. **Exception:** rows that would
  render byte-identical each show their short pane id (`ambiguous_rows`), so several
  look-alike agents in one tab are never a blind choice. The tab label needs a second
  `herdr tab list --workspace <ws>` call joined on `tab_id` (cheap, synchronous between
  frames like the agent list).
- A send whose chosen agent has since vanished fails gracefully: comments stay, the error
  shows, **the picker closes** (the reviewer re-presses `Send` for a fresh list).
- **Turn tracking is unchanged.** `last-turn` still resolves only the *sole* agent; an
  ambiguous or absent agent still pauses tracking. The picker never changes which agent
  turn tracking follows (`resolved_agent_status`: `One → Some`, `Many`/`None → None`).
- Continuity (O6): a background world poll while the picker is open never moves the
  highlight or the frozen diff underneath — the picker is a centered-popup modal, handled
  like the comments list at the diff-freeze and mouse-capture sites.
- Specs updated with the code: `herdr-host.md` and `input.md` only (see Specs Touched for
  why `overview.md`/`tui.md`/`config.md`/`review-model.md` need no edit). `CHANGELOG.md`
  and `README.md` note the new behavior.

## Design

### Resolution: One / Many / None (pure) — `src/herdr.rs`

- Extend the `AgentPane` serde struct with `name: Option<String>` and
  `terminal_title_stripped: Option<String>`. The struct has no `deny_unknown_fields` and
  already ignores many live fields, so adding two typed `Option`s is safe; missing → `None`.
- Replace `Refusal` + the borrow-returning `pick_agent` with a three-way `pick`:

  ```rust
  enum Pick<'a> { One(&'a AgentPane), Many(Vec<&'a AgentPane>), None }
  ```

  **Preserve today's boundary exactly** (the `in_tab.is_empty()` guard, `herdr.rs`
  current ~113): a sole tab agent → `One` (`HH-TAB-WINS`); else decide on the **widest
  non-empty** candidate set — workspace candidates when non-empty, else the tab
  candidates: one → `One`, several → `Many(those)`, and `None` **only when both the tab
  and workspace candidate sets are empty**. This keeps the existing test
  `two_tab_agents_refuse_as_several_even_without_a_workspace_id` (two tab agents, no
  workspace id) classifying as ambiguous → picker, not `None`. `Many` therefore carries
  exactly the ambiguous set — the picker lists precisely those.
- `resolve_send_target() -> Result<SendTarget>` = `agent_list()` then `pick`, mapped to an
  **owned, transient** result `send_to_agent` destructures immediately (the app stores the
  chosen `agent_choices`, never the `SendTarget`; `Pick` stays borrowed so the turn-tracking
  poll path never allocates throwaway `AgentChoice`s). Only the `Many` branch does the extra
  work: a single `tab list --workspace <ws>` call builds a `tab_id → label` map used to fill
  each choice's `tab_label` (the `One` and `None` branches skip it):

  ```rust
  pub enum SendTarget { One(String), Many(Vec<AgentChoice>), None }

  #[derive(Clone, Debug, PartialEq, Eq)]   // Debug required: App is #[derive(Debug)]
  pub struct AgentChoice {
      pub pane_id: String,           // internal send target — never displayed
      pub kind: String,              // the `agent` field: "claude"/"codex"
      pub name: Option<String>,      // agent session name — usually None
      pub tab_label: Option<String>, // from the tab-list join; shown only when != kind
      pub status: Status,
      pub title: String,             // terminal_title_stripped ("" if absent)
  }
  ```
  Row build (in `ui.rs`): lead with `name` if `Some`, else `kind`; append `tab_label` when
  it is `Some` and `!= kind`; then `status`; then `title` if non-empty — each separated by
  ` · ` / ` — `, empties skipped.
- `resolved_agent_status()` (turn tracking; sole caller `world.rs`): map `Pick::One →
  Some(status)`, `Many`/`None → None` — provably equivalent to today's
  `pick_agent(...).ok().map(...)`. **No behavior change.**
- Remove `resolve_agent_pane` (its only production caller is the old `Agent` target).

### Export target: address a specific pane — `src/export.rs`

- Replace the internally-resolving `Agent` unit struct with a pane-addressed target:

  ```rust
  pub struct Agent { pub pane: String }   // export = herdr::send_text(&pane, text) then focus
  ```

  `label`/`success_message` unchanged (they ignore the pane). Resolution leaves the
  target; the caller resolves, then builds `Agent { pane }`. `Clipboard` untouched. The
  existing `Agent.success_message(..)` unit test is updated to construct `Agent { pane: .. }`.

### App: the picker modal — `src/app.rs`

- New **unit** variant `Mode::SelectAgent` (so the existing `== Mode::X` comparisons keep
  compiling; the data-in-variant fallback is dropped). Backing state as sibling fields to
  `list_cursor`: `agent_choices: Vec<AgentChoice>`, `agent_cursor: usize`.
- `send_to_agent(&mut self)` — the entry point `s` and the header `Send` click call:
  1. `store.is_empty()` → status *"no comments to send"*, return (picker never opens empty).
  2. `resolve_send_target()`:
     - `Ok(One(pane))` → `self.export(&Agent { pane })` (today's direct send).
     - `Ok(Many(choices))` → open the picker inline: `agent_choices = choices`,
       `agent_cursor = 0`, `mode = SelectAgent` (no separate `begin_agent_pick` — one caller).
     - `Ok(None)` → status *"no agent here — copy to the clipboard instead"* (comments kept).
     - `Err(e)` → status *"agent failed: {e}"* (herdr CLI unreachable; comments kept).
- `agent_move(delta)` — highlight movement, guarded on `SelectAgent` && non-empty, via `step`.
- `confirm_agent_pick(&mut self)` — `Enter`: take the highlighted `AgentChoice`;
  `self.export(&Agent { pane })`; then `close_modal()` **unconditionally**. (Do not lean on
  `export`'s store-empty tail: on a failed send the store isn't consumed, so that tail
  would leave the picker open — the DoD requires it to close on failure.)
- **Cancel needs no method:** the picker's `Esc` calls `close_modal()` directly (mirroring
  the comments-list `Esc → close_list`). The flip to `Normal` *is* the entire cancel;
  `agent_choices` is inert outside `SelectAgent` and reset by the next open, so there is
  nothing to clear.
- Rename the shared `close_list()` → `close_modal()`, flipping `List` **or** `SelectAgent`
  back to `Normal` (today `close_list` no-ops unless `mode == List`; widen to both). It
  touches only `mode`, so consume-on-success and comment retention are untouched. Callers:
  the `export` success tail, the comments-list `Esc`/`Comments` handlers, and the picker `Esc`.

This adds **three** App methods — `send_to_agent`, `agent_move`, `confirm_agent_pick` — not five.
- **Modal-behavior sites — only two gain `SelectAgent`** (the picker behaves like the
  comments-list popup, *not* like the body-replacing `Search`/`Find` screens). Add a tiny
  `fn popup_modal(&self) -> bool { matches!(self.mode, Mode::List | Mode::SelectAgent) }`
  and use it at:
  - the reconcile **diff-freeze** guard (`app.rs` current ~950: `!self.composing() &&
    self.mode != Mode::List`) → `!self.composing() && !self.popup_modal()`. Without this a
    landing poll shifts the frozen diff under the picker (O6 violation).
  - the **mouse-capture** guard (`lib.rs` current ~1593: `app.composing() || app.mode ==
    Mode::List`) → `app.composing() || app.popup_modal()`.

  **Leave the List-*semantic* `==`/`!=` guards unchanged** — they compile as-is and already
  exclude `SelectAgent`: `target_comment`, the `start_edit`/`delete_comment` preview guards,
  `list_move`, the `footer_bands` List arm (the picker gets its own arm), and the List
  key-dispatch block. Extending any of these to the picker would be a bug.

  **But three `match` sites are wildcard-less and exhaustive, so adding the `SelectAgent`
  variant forces a new arm — a behavioral no-op, the same compile-forced edit as the two new
  `FooterAction` glyph arms:** `active_field` and `pending_location` add `SelectAgent` to
  their existing `=> None` arm (the picker has no editable field and no pending comment
  location), and `carry_authored_state_from` (config recovery) adds it to the no-op `{}` arm
  (so a recovered `SelectAgent` is *not* preserved and falls to `Normal`, per S1). These are
  "leave the behavior alone" but not "leave the code alone" — the arm must be named.
- **Config recovery: do NOT preserve `SelectAgent`.** The recovery arm preserves only
  `List | Composing`; leaving `SelectAgent` out lets a recovered app fall to `Normal`
  with comments intact (the store is already carried) — the user re-presses `Send`. This
  avoids a recovered `SelectAgent` with an empty `agent_choices` painting a 0-row picker,
  and means `tui.md`'s recovery sentence stays true (no spec edit there).

### Dispatch — `src/lib.rs`

- Route all three `Send` entry points through `send_to_agent()`: Normal `K::Send`
  (current ~1489), List-mode `K::Send` (current ~1449), and the header `HeaderHit::Send`
  click (current ~1680). The `use crate::export::Agent` import stays (still builds
  `Agent { pane }`). `Clipboard` sites untouched.
- New `if app.mode == Mode::SelectAgent { … }` block before the generic dispatch,
  mirroring the `Mode::List`/`Mode::Search`/`Mode::Find` blocks:
  - `Enter` → `confirm_agent_pick`
  - `Esc` → `cancel_agent_pick`
  - `K::Down`/fixed `Down` → `agent_move(1)`; `K::Up`/fixed `Up` → `agent_move(-1)`
    (routing through the resolved actions, so `j`/`k` track any rebind — matching the
    comments-list precedent)
  - everything else inert
- Mouse: `popup_modal()` captures the mouse (inert), so the picker is covered by the
  updated `lib.rs` guard above.

### Footer + rendering — `src/app.rs`, `src/ui.rs`

- `FooterAction`: add `ConfirmAgent` (`↵ send`) and `ChooseAgent` (`↑↓ choose`); reuse the
  existing `Cancel` (`esc cancel`). The glyph map (`ui.rs`) has no wildcard arm, so both
  need an arm there or it won't compile. `footer_bands()` gains
  `Mode::SelectAgent => vec![(ConfirmAgent, Primary), (Cancel, Do), (ChooseAgent, Do)]`
  (using `Band`, not the old `Tier`).
- `render_agent_picker(frame, app, area)` mirrors `render_comments_list`: a
  `centered(area, 80, 60)` popup, `Clear`, bordered block titled `Send to agent (N)`, one
  `selectable_row` per choice built per the row rule above — **name-or-kind** (bold accent),
  then `tab_label` when `Some` and `!= kind` (dim), then status (colored: idle/done dim,
  working accent, blocked red), then the dim, truncated `title` after ` — `, every empty part
  skipped. No pane id is drawn. `render` calls it when `mode == SelectAgent`. Popup visuals
  are code-only (the comments-list popup is too — no `tui.md` edit).

### Why a modal picker (alternatives rejected)

- A **config-set default agent** can't track agents that come and go per worktree and
  silently sends to the wrong one.
- **Cycling agents on a key** hides the choice set and identities; a list showing each
  agent's title is legible at a glance.
- The modal reuses the proven `Mode::List` popup pattern, inheriting diff-freeze and
  mouse-capture with minimal new surface.

## Specs Touched

| Spec | What this plan realizes | Gate |
| --- | --- | --- |
| `herdr-host.md` | `### Sending to the agent`: rewrite the lead prose to the three-way outcome (one → send, several → pick, none → refuse). Retire `HH-SOLE-OR-REFUSE` and `HH-REFUSE-SAYS-CLIPBOARD`; introduce `HH-ZERO-REFUSE` (no candidate → status names the clipboard), `HH-ONE-SENDS`, `HH-MANY-PICK` (the picker lists **exactly** the ambiguous candidates, `HH-NOT-SELF`/`HH-AGENT-PANES`-filtered), `HH-PICK-SENDS-CHOSEN` (the send addresses the **highlighted pane**, not a re-resolved target), `HH-PICK-CANCEL-KEEPS` (cancelling **sends nothing**). Reaffirm turn tracking cites the surviving `HH-AGENT-PANES`/`HH-NOT-SELF`/`HH-TAB-WINS`. Note the zero-case status prefix drop. | Draft → Current |
| `input.md` | Add the agent picker as a local mode that **acts through the `down`/`up` bindings**, sends on `enter`, cancels on `esc` (mirror the line-21 comments-list sentence, not a "fixed keys" claim). Extend the enumerated local-mode sentences to include the picker: the own-one-row-footer set (~172), the "inert in the comments list" line (~173), and the navigator-actions-inert-in-local-modes line (~62). | Draft → Current |

Retired invariant codes are not reused (per `specs/CLAUDE.md`: retire, never repurpose).

**Deliberately NOT touched (conformance, not edits):**
- `overview.md` — has no mode map (modes live in `src/app.rs`); its "mid-gesture frozen"
  invariant (O6) already generalizes over modals without naming each one. No edit.
- `tui.md` — the comments-list popup is code-only, so the picker popup is too; and because
  the picker is **not** preserved across config recovery, the recovery sentence (~line 87)
  stays true. No edit.
- `config.md` — no `[keybindings]` action added; `CFG-KEY-UNIQUE`/form invariants unaffected.
- `review-model.md` — delegates pane-finding to `herdr-host.md`; consume-on-success stays
  its contract (the new invariants don't restate it). No edit.

## Out of Scope

- **Copy from the picker** (Esc then `y` still copies); the picker stays single-purpose.
- **Mouse click-to-select in the picker** — captured/inert, matching `Mode::List`.
- **Per-comment routing to different agents** — Send stays all-or-nothing to one agent.
- **Config default / auto-pick a primary agent** — rejected above.
- **OSC 52 / remote clipboard** — the picker removes the need; separate concern.
- **Persisting the last-picked agent** — the comment store is in-memory by design (O3).

## Likely Files

- `src/herdr.rs` — `AgentPane` fields; `Pick`; `resolve_send_target` (+ the `Many`-only
  `tab list` join for `tab_label`); `SendTarget`; `AgentChoice`; `resolved_agent_status`
  remap; drop `resolve_agent_pane`; tests.
- `src/export.rs` — `Agent { pane }`; test fix.
- `src/app.rs` — `Mode::SelectAgent` + fields; `send_to_agent`/`agent_move`/
  `confirm_agent_pick`; `close_list`→`close_modal`; `popup_modal`; footer arm;
  diff-freeze guard; the three compile-forced no-op arms; tests.
- `src/lib.rs` — Send call sites → `send_to_agent`; `SelectAgent` key block; mouse-capture
  guard.
- `src/ui.rs` — `render_agent_picker`; `ConfirmAgent`/`ChooseAgent` glyphs.
- `specs/herdr-host.md`, `specs/input.md`; `CHANGELOG.md`, `README.md`.

## Execution Plan

1. [ ] `herdr.rs`: `AgentPane` fields; `Pick` + pure classifier preserving the
       `in_tab.is_empty()` boundary; `resolve_send_target`/`SendTarget`/`AgentChoice`
       (derive `Debug`; `Many` branch joins `tab list` for `tab_label`); remap
       `resolved_agent_status`; drop `resolve_agent_pane`. Unit tests: One/Many/None across
       tab/workspace scope, the two-tab-no-ws case stays ambiguous, self + non-agent
       exclusion, `Many` carries the exact set, the row-compose helper (name/kind/tab/title,
       empties omitted, tab==kind collapsed).
2. [ ] `export.rs`: `Agent { pane }`; fix the `success_message` test.
3. [ ] `app.rs`: `Mode::SelectAgent` + fields; the three methods
       (`send_to_agent`/`agent_move`/`confirm_agent_pick`); `close_list`→`close_modal`;
       `popup_modal`; footer arm; diff-freeze guard; the three compile-forced no-op arms
       (`active_field`, `pending_location`, `carry_authored_state_from`). Unit tests:
       empty-store precedence, `Many` → mode+choices, `None`/`Err` → status (no picker),
       confirm closes on both success and failure, Esc → Normal + comments kept,
       `agent_move` clamps.
4. [ ] `lib.rs`: route Send → `send_to_agent`; `SelectAgent` key block; mouse-capture guard.
5. [ ] `ui.rs`: `render_agent_picker`; footer glyphs.
6. [ ] Specs: `herdr-host.md`, `input.md`. `CHANGELOG.md`, `README.md`.
7. [ ] `just ci` (fmt-check, lint, test, release build). Sanity-run the bench once
       (no reload/render/git/highlight hot-path change expected).
8. [ ] `just qa-install`; drive live in a worktree with several agents.

## Verification

- **Done:** in a worktree with ≥2 agents, write comments, `s` → picker lists the agents
  with titles; `↑/↓` + `Enter` drops the comments into the chosen agent's input and
  focuses it; `Esc` keeps them. One-agent and zero-agent worktrees behave as before.
- **Tight:** row-check the diff against the Likely Files / Execution Plan; nothing beyond
  the picker; confirm no un-listed `Mode::List` site gained `SelectAgent`.
- **Invariants:**

| Ref | Bound to | Signal |
| --- | --- | --- |
| `HH-ONE-SENDS` | one workspace agent | direct send, no picker |
| `HH-MANY-PICK` | ≥2 candidates | picker opens listing exactly them |
| `HH-ZERO-REFUSE` | no candidate | clipboard-hint status, no picker |
| two-tab-no-ws | 2 tab agents, no `HERDR_WORKSPACE_ID` | still ambiguous → picker, not `None` |
| `HH-NOT-SELF`/`HH-AGENT-PANES` | reviewer pane + a plain shell present | neither appears |
| `HH-PICK-SENDS-CHOSEN` | Enter on a row | comments land in that pane, consumed |
| `HH-PICK-CANCEL-KEEPS` | Esc | comments preserved, mode Normal |
| vanished agent | agent closed before Enter | error status, comments kept, picker closes |
| empty-store precedence | `s` with no comments | "no comments to send", no picker |
| turn tracking | several agents present | `last-turn` still paused (no auto-pick) |
| continuity | world poll while picker open | highlight + diff undisturbed |
| config recovery | invalid→valid config while picker open | falls to Normal, comments intact |

## Review findings folded (spec-review gate)

Three independent reviewers ran to completion against current `main` (two initial runs
misfired and were re-spawned); their must-fixes are incorporated above:

- **Feasibility (FEASIBLE-WITH-FIXES):** M1 preserve the `in_tab.is_empty()` None/Many
  boundary (else the two-tab-no-ws test flips and the picker is skipped); M2
  `confirm_agent_pick` closes the modal **unconditionally** (export's tail only closes on
  success); M3 `AgentChoice` must derive `Debug`; S1 do **not** preserve `SelectAgent`
  across config recovery; S2 use a **unit** variant (drop the data-in-variant fallback);
  S3 the row discriminator is `terminal_title_stripped`, not `name` (absent for claude);
  S4 the zero-case status loses its `agent failed:` prefix (noted); S5 fix the
  `export.rs` test. Only **two** modal sites gain the picker (diff-freeze, mouse-capture);
  all other `Mode::List` sites are left alone.
- **Spec-first (SPEC-GAPS):** retire `HH-SOLE-OR-REFUSE` **and** `HH-REFUSE-SAYS-CLIPBOARD`
  (don't reuse codes); the picker "acts through the `down`/`up` bindings" — `j`/`k` are
  **not** fixed, only `↑`/`↓`/`enter`/`esc` are; enumerate the input.md set-sentences
  (~172/173/62); drop `overview.md` and the `tui.md` popup edit; sharpen the new HH-*
  wording so they don't duplicate review-model.md's consume-on-success; update the lead
  prose of "Sending to the agent". `config.md`/`review-model.md` correctly untouched.
- **Simplicity (TRIM-RECOMMENDED):** drop `begin_agent_pick` (inline its 3 assignments)
  and `cancel_agent_pick` (route `Esc` straight to `close_modal`), taking the new method
  count from five to three; `SendTarget` is transient, not held across frames (`Pick` stays
  borrowed for the poll path); `popup_modal` and the `Pick`/`SendTarget` split are justified,
  not overbuilt; all folded fixes are internally consistent. It also caught that
  `active_field`/`pending_location`/`carry_authored_state_from` are exhaustive matches
  needing an explicit (no-op) `SelectAgent` arm — folded above.

## Replan Triggers

- If a future herdr drops `terminal_title_stripped` for some kind, the row falls back to
  `name`-or-kind + pane id, noted in the spec.

## Replan Log

- Row format settled with Ivan during implementation: drop the always-on pane id; key rows on
  name-or-kind + tab label + status + title; show the short pane id only as a collision
  fallback (`ambiguous_rows`) so several look-alike agents in one tab are never a blind choice.
