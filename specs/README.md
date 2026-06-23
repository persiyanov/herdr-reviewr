# Specs

The living design of herdr-review. One concept per doc, always current.

Each doc states end-state truth: what must be *true* when a change is done, never what is built today or when. The code holds the implementation; these specs hold the meaning, the invariants, and the decisions behind them.

## How to read these

- Each doc owns one concept: the model, the UI, a subsystem.
- Every doc keeps the same sections in the same order, so the outline is predictable.
- A doc leads with a concrete example, then a field table, then behavior.
- If a doc and the code disagree, the code is a wrong implementation of the doc's contract.

## Status

The front matter of each spec carries `Status`, `Created`, and `Last edited` (ISO dates). `Last edited` is updated on every edit.

- `Draft` — end-state truth for a change in flight; not yet matched by the code.
- `Current` — the code matches the spec.
- `Superseded` — replaced by another spec; moved to `archive/` with a pointer to its replacement. When an archived and a current spec conflict, the current spec wins.

## Ownership map

Each concern lives in the one doc that owns it. A change is woven into that doc, never a per-feature file.

- `overview.md` — owns the product shape, scope vs roadmap, and top invariants.
- `review-model.md` — owns scopes, changed files, comments, lifecycle, and export.
- `tui.md` — owns the terminal UI: layout, interaction, and refresh.
- `herdr-host.md` — owns running as a herdr pane, the export target, and roadmap integration.

## The bar

- Altitude — show the design concretely; don't transcribe the schema.
- Headers — `###` max, short noun phrases, parallel across siblings.
- Bullets — one idea each, no nesting past one level, one emphasis at most.
- Failure semantics — for anything persistent or side-effecting, state what happens on the second run and under concurrent runs.

## Conventions

- Start a new doc from `spec-template.md`.
- New docs are born `Draft`.
- Keep links between specs relative. End each doc with a `Related specs` list.
- When renaming or moving a doc, update this README and every link that points to it.
