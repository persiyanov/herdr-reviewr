# Strict configuration and GitHub Enterprise — Plan

Delivers [the strict configuration spec](../../../specs/config.md) and [the forge-host spec](../../../specs/forge-host.md) for issue #11, including the affected sidebar, host, review-model, and theme contracts.

## Goal

Make plugin configuration one typed, fail-loud boundary shared by the sidebar, actions, events, and review process, then use its immutable snapshots to support one configured GitHub Enterprise host without stale or misdirected pull-request state. Ship this as one release-sized vertical capability because host resolution depends on the configuration contract and both paths meet in the review-fetch lifecycle.

## Definition of Done

- [x] One resolver implements the defaulting and whole-file validation contract for all six settings in `specs/config.md`, including C1–C8; every plugin entry point consumes the same normalized result.
- [x] An observed invalid configuration blocks all normal plugin work with the specified surface and exit behavior, performs no requested action, and recovers as a fresh start after an atomic valid replacement.
- [x] GitHub.com and the configured Enterprise host satisfy the URL, trusted SSH-alias, rewrite, canonical API-host, and distinct remote-state rules in `specs/forge-host.md`.
- [x] Pull-request work is keyed by a complete fetch input; changed or superseded inputs cannot publish stale results, while same-input failures preserve the visible snapshot and show the remedy.
- [x] Existing valid theme, base-branch, placement, direction, auto-open, GitHub.com, and local-review behavior remains covered.
- [x] User documentation and the Unreleased changelog describe the final behavior, all affected tests and quality gates pass, and the delivered specs are promoted from Draft to Current.

## Out of Scope

- Multiple configured Enterprise hosts or arbitrary per-repository host mappings.
- Discovering arbitrary SSH aliases from SSH configuration; only the documented trusted alias forms are recognized.
- Custom GitHub host ports or a GitHub Enterprise schema compatibility layer.
- Tightening unrelated command-line argument fallback behavior.
- Publishing a release or changing the package version.

## Execution Plan

1. **Create the shared configuration boundary.** Add a typed plugin configuration and structured error in `src/config.rs`, with explicit defaults, unknown-key rejection, and value validation. Expose a small internal machine-readable resolver through the installed binary so shell entry points use the Rust parser rather than reimplement TOML semantics. Reuse the theme catalog as the authority for valid theme names. Lock this boundary down with the C1–C8 matrix, per-key invalid cases, unreadable-file cases, and normalized-output tests.

2. **Put validation before every shell-side effect.** Refactor `herdr/sidebar.sh` so sidebar, action, and event modes resolve configuration before inspecting or mutating pane/workspace state. Consume only normalized settings after validation. Add script-level coverage for exit status, diagnostics, and absence of pane/action side effects, including invalid `open`, `close`, `toggle`, and auto-open events followed by recovery with a corrected file.

3. **Make invalid configuration a first-class runtime state.** Have the event loop own the latest valid configuration snapshot or configuration error and pass snapshots into app reloads instead of allowing theme, Git, or forge code to reread independently. Add the error-only render path, suppress normal review work while blocked, discard completions tied to invalidated snapshots, and restart cleanly when the file becomes valid. Cover valid → invalid → valid transitions and invalidation during in-flight work.

4. **Introduce canonical repository identity and fetch-input coordination.** Extend Git remote parsing for the exact Enterprise host and trusted SSH-alias rules, using the rewritten primary fetch URL and preserving missing, hostless, malformed, and unsupported outcomes. Model the complete PR fetch input as an equality-comparable value, attach it to background work and completions, clear/refetch on input changes, and apply results only when the current worktree and configuration still derive the same input. Pass base-branch configuration into Git discovery instead of rereading it.

5. **Target the canonical GitHub API host and close the delivery loop.** Build `gh api graphql` arguments with an explicit canonical hostname and no ambient `GH_HOST` dependency. Test GitHub.com, Enterprise, alias rejection/acceptance, rewrite behavior, hostile completion orderings, and same-input failure preservation. Update README and CHANGELOG, run the merge gates, perform an xhigh full-branch review, resolve actionable findings, and promote the specs to Current only when the branch is merge-ready.

## Likely Files

| Area | Files |
| --- | --- |
| Configuration model and resolver | `Cargo.toml`, `src/config.rs`, `src/theme.rs`, `src/main.rs`, `src/lib.rs` |
| Blocking runtime/UI state | `src/lib.rs`, `src/app.rs`, `src/ui.rs` |
| Repository identity and PR lifecycle | `src/git.rs`, `src/forge.rs`, `src/app.rs`, `src/lib.rs` |
| Shell entry points | `herdr/sidebar.sh`, script-focused tests or fixtures |
| Contract and user documentation | `README.md`, `CHANGELOG.md`, `specs/config.md`, `specs/forge-host.md`, affected referenced specs |

This list is directional: keep responsibilities with their existing owners and add a focused module only if configuration resolution or fetch coordination would otherwise leave `src/lib.rs` carrying policy details.

## Verification

| Contract surface | Evidence |
| --- | --- |
| Configuration resolution | Unit table for C1–C8, every key's default/valid/invalid behavior, unknown keys, malformed TOML, read failures, and diagnostic path/cause; resolver-output integration test |
| Fail-loud entry points | Script tests and a temporary-config smoke showing invalid sidebar/action/event behavior, no side effects, exit status `1` where specified, and recovery after atomic replacement |
| Runtime convergence | App/event-loop tests for valid → invalid → valid, error-only rendering, no blocked review work, discarded superseded completions, and fresh recovery |
| Host classification | Parser tests for GitHub.com, exact Enterprise host, accepted SSH aliases, rejected HTTPS aliases, rewrites, and separate missing/hostless/malformed/unsupported states |
| Fetch lifecycle | Coordinator tests for branch, HEAD, candidate, base-setting, host, and worktree changes; out-of-order completions; same-input error preservation; changed-input error rejection |
| API targeting | Argument-construction test proving the canonical hostname is passed explicitly; optional live Enterprise smoke when credentials and a host are available |
| Regression and quality gates | `cargo fmt --check`; `cargo test --all-targets`; `cargo clippy --all-targets --all-features -- -D warnings`; `bash -n herdr/sidebar.sh`; `git diff --check`; xhigh full-branch review |

## Replan Triggers

- If the installed binary cannot expose configuration resolution before shell-side workspace logic without creating a startup cycle, move the action/event entry points into Rust rather than duplicating validation in shell.
- If deriving the current fetch input on refresh makes the draw loop observably unresponsive, move that derivation behind background coordination while retaining snapshot equality and stale-result rejection.
- If the available `gh` version cannot target a hostname explicitly for GraphQL, stop and revise the API-execution boundary before implementing an ambient-environment workaround.
- A live Enterprise smoke is conditional on available credentials and a reachable host; lack of either does not weaken the required parser, argument, and lifecycle tests.
