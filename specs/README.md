# Specs

The living design of herdr-reviewr. One concept per doc, always current.

Each doc states end-state truth: what must be true when a change is done, never what is built today or when. The code holds the implementation. The PRs hold the rationale. These docs hold the contract.

## How to read these

- Each doc owns one concept: the model, the UI, a subsystem.
- Every doc opens with the smallest useful mental model. Its middle follows the reader's questions.
- Every section earns its place from the concept. Docs do not share sections merely for uniformity.
- Every list and table states its admission rule and contains the complete admitted set.
- If a doc and the code disagree, the code is a wrong implementation of the doc's contract.

For a full-system read, follow the dependency order: overview → review-model → config → diff-view, markdown, theme, file-list → tui → input → search → find-in-file → herdr-host, forge-host → pr-tab.

## Status

The front matter of each spec carries `Status`, `Created`, and `Last edited`, ISO dates. `Last edited` is updated on every edit.

- **Draft** — end-state truth for a change in flight. Not yet matched by the code.
- **Current** — the code matches the spec.
- **Superseded** — replaced. Moved to `archive/` with a pointer to its replacement. The current spec wins any conflict.

## Ownership map

Each concern lives in the one doc that owns it. A change is woven into that doc, never a per-feature file. Each doc registers a short uppercase prefix for its invariant and trace codes.

- `overview.md` (`OV`) — owns the product shape, scope vs roadmap, and top invariants.
- `config.md` (`CFG`) — owns plugin config keys, validation, application, and failure behavior.
- `review-model.md` (`RM`) — owns scopes, changed files, comments, lifecycle, and export.
- `diff-view.md` (`DV`) — owns the structured diff viewer: model, syntax highlighting, word emphasis, folds, and views.
- `markdown.md` (`MD`) — owns markdown rendering: the elements, their terminal presentation, and the surfaces that render it.
- `theme.md` (`TH`) — owns the color model: the named palettes, how a palette is filled from anchors, and theme selection.
- `file-list.md` (`FL`) — owns the file navigator: the changed-files tree, selection, and presentation.
- `tui.md` (`TUI`) — owns the terminal frame: layout, tabs, and refresh.
- `input.md` (`INP`) — owns driving the review: the keymap, changeset traversal, the footer, and the comment editor.
- `search.md` (`SR`) — owns the search overlay: the engine, the query, the results, and where a pick lands.
- `find-in-file.md` (`FIF`) — owns in-file find: the band, literal matching, match-to-match stepping, and its continuity.
- `pr-tab.md` (`PRT`) — owns the read-only PR mirror: its header, navigator, read pane, and refetch.
- `herdr-host.md` (`HH`) — owns running as a herdr pane, the export target, and roadmap integration.
- `forge-host.md` (`FH`) — owns reading the pull request from GitHub via `gh`: resolution, state, checks, comments, and failure states.

## The bar

A spec is a communication medium between the agent and the humans on the project. It is never a scratchpad. Human reading speed and understanding come first, in every edit.

- One fact per sentence. Linear sentences, no asides.
- One grammatical template per list or table. Schema-first tables.
- Structure derives from the domain, never from editorial history or investigation effort.
- Every collection is complete under one stated admission rule.
- Contract only: mechanism lives in the code, rationale in the PR, provenance in git.
- One home per fact. Link to that home everywhere else.
- Only cross-operation constraints on valid state are invariants. Code one (`CFG-WHOLE-FILE`) only when it needs a stable citation.
- Use an example when it teaches faster. Headers `###` max. Under ~2,000 words per doc.

The full bar lives in [`AGENTS.md`](./AGENTS.md).

## Conventions

- Start a new doc from `spec-template.md`.
- New docs are born `Draft`.
- Keep links between specs relative. End each doc with a `Related specs` list.
- When renaming or moving a doc, update this README and every link that points to it.
