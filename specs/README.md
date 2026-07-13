# Specs

The living design of herdr-reviewr. One concept per doc, always current.

Each doc states end-state truth: what must be true when a change is done, never what is built today or when. The code holds the implementation. The PRs hold the rationale. These docs hold the contract.

## How to read these

- Each doc owns one concept: the model, the UI, a subsystem.
- Every doc keeps the same sections in the same order, so the outline is predictable.
- A doc leads with a concrete example, then rule tables, then traces for the failure-prone paths.
- If a doc and the code disagree, the code is a wrong implementation of the doc's contract.

## Status

The front matter of each spec carries `Status`, `Created`, and `Last edited` (ISO dates). `Last edited` is updated on every edit.

- `Draft` — end-state truth for a change in flight; not yet matched by the code.
- `Current` — the code matches the spec.
- `Superseded` — replaced by another spec; moved to `archive/` with a pointer to its replacement. When an archived and a current spec conflict, the current spec wins.

## Ownership map

Each concern lives in the one doc that owns it. A change is woven into that doc, never a per-feature file.

- `overview.md` — owns the product shape, scope vs roadmap, and top invariants.
- `config.md` — owns plugin config keys, validation, application, and failure behavior.
- `review-model.md` — owns scopes, changed files, comments, lifecycle, and export.
- `diff-view.md` — owns the structured diff viewer: model, syntax highlighting, word emphasis, folds, and views.
- `markdown.md` — owns markdown rendering: the elements, their terminal presentation, and the surfaces that render it.
- `theme.md` — owns the color model: the named palettes, how a palette is filled from anchors, and theme selection.
- `file-list.md` — owns the right-pane file navigator: the changed-files tree, selection, and presentation.
- `tui.md` — owns the terminal UI: layout, interaction, and refresh.
- `herdr-host.md` — owns running as a herdr pane, the export target, and roadmap integration.
- `forge-host.md` — owns reading the pull request from GitHub via `gh`: resolution, state, checks, comments, and failure states.

## The bar

A spec is a communication medium between the agent and the humans on the project. It is never a scratchpad. Human reading speed and understanding come first, in every edit.

- One fact per sentence. Linear sentences, no asides.
- One grammatical template per list or table. Schema-first tables.
- Contract only: mechanism lives in the code, rationale in the PR, provenance in git.
- One home per fact. Cite by number everywhere else.
- Example first, then rules. Headers `###` max. Under ~2,000 words per doc.

The full bar lives in the brainstorming skill's `writing-great-specs.md`.

## Conventions

- Start a new doc from `spec-template.md`.
- New docs are born `Draft`.
- Keep links between specs relative. End each doc with a `Related specs` list.
- When renaming or moving a doc, update this README and every link that points to it.
