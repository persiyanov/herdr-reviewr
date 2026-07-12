# Skill Install Helper + Agent-Onboarding Docs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the reviewr-comments skill durably discoverable by agents: a `skill-install` subcommand that wires it into Claude Code's skills directory, plus README docs for Claude Code, CLAUDE.md, and other harnesses.

**Architecture:** One new subcommand on the existing `src/cli.rs` dispatch (house patterns: hand-rolled args, usage-on-2, one-line stderr errors), reusing the existing skill-path resolution. README gains an "Install the skill" subsection under Working with agents.

**Tech Stack:** Existing crate; no new dependencies.

## Global Constraints

- House rules from prior plans: clippy pedantic `-D warnings` clean; `cargo test --all-features` green; `cargo fmt --all` before commit, discarding CRLF-only rewrites of untouched files; dense contract doc comments; commit trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`; branch `skill-install`.
- Tokens/paths never mangled: the subcommand touches only the target skills directory it is told to.

---

### Task 1: `skill-install` subcommand + README section

**Files:**
- Modify: `src/cli.rs` (new subcommand + usage text), `src/main.rs` (dispatch arm if the current dispatch enumerates subcommands), `README.md` (§Working with agents)
- Test: `tests/comments_cli.rs` (or a sibling integration file if cleaner)

**CLI contract:**

```
herdr-reviewr skill-install [--target <dir>] [--copy] [--force]
```

- Default `<dir>`: `$HOME/.claude/skills/reviewr-comments` (`%USERPROFILE%` on Windows; use `std::env::var_os("HOME").or(USERPROFILE)`). `--target` overrides (also the test seam).
- Resolves the SKILL.md source exactly as `skill-path` does (reuse that function). Source missing → exit 1 with skill-path's error.
- Creates the target dir (parents included). Installs `SKILL.md` into it: **symlink by default** on Unix; on Windows or when symlink creation fails (privileges), **fall back to copy** with a stderr note ("symlink unavailable — copied; re-run after plugin updates"). `--copy` forces copy mode.
- Idempotency: if the target already exists and (a) is a symlink pointing at the resolved source, or (b) `--copy`/fallback and byte-identical → print "already installed at <path>", exit 0. Any other existing file → exit 1 naming the conflict and advising `--force`. `--force` replaces.
- On success print the installed path AND the follow-up hint block (stdout):

  ```
  installed: <path>
  To make agents check comments proactively, add to your CLAUDE.md:
    Reviews happen in the herdr-reviewr sidebar — when starting work or when review
    feedback is mentioned, run `herdr-reviewr comment list` and address open comments.
  ```
- Unknown flag / missing value → usage, exit 2 (existing convention).

**README section** (replace the current lone generic-prompt paragraph with a structured subsection; keep the generic prompt as the "no install" fallback):

1. `### Install the skill (Claude Code)` — the subcommand one-liner, what it does (symlink → stays current across plugin updates), the `--copy`/`--target`/project-level (`--target .claude/skills/reviewr-comments` committed to the repo) variants, and that afterwards "address my review comments" works with no reminder because the skill's description is in every session's skill list.
2. `### Make it proactive (CLAUDE.md)` — the exact one-line snippet from the CLI hint, and why (CLAUDE.md is loaded every session; fixes "the agent doesn't know reviewr exists").
3. `### Other agents/harnesses` — AGENTS.md gets the same pointer line; generic fallback = the existing `skill-path` prompt.

- [ ] **Step 1: Failing integration tests** (drive the real binary with `--target` into a tempdir; follow the existing helper pattern):
  - fresh install creates the file (symlink on Unix — assert `symlink_metadata().file_type().is_symlink()` under `#[cfg(unix)]`; on Windows assert the file exists with the source's content) and stdout contains "installed:" + the CLAUDE.md hint.
  - second run → "already installed", exit 0, file unchanged.
  - pre-existing different file → exit 1 naming conflict; with `--force` → replaced, exit 0.
  - `--copy` → regular file, byte-identical to source.
  - unknown flag → exit 2 + usage.
  - NOTE: `skill-path`'s dev-checkout fallback is cwd-relative; run the binary with cwd = the repo root (as the existing skill_path test does) so the source resolves.
- [ ] **Step 2:** RED. **Step 3:** implement (cli.rs; ~120 lines incl. usage + doc comments). **Step 4:** README section. **Step 5:** full gate. **Step 6:** fmt, commit `feat(cli): skill-install subcommand and agent-onboarding docs`.

---

## Self-Review Notes
- Idempotency/force/copy semantics pinned; Windows fallback specified; test seam via `--target`; README structure fixed. Single task — reviewer gate covers code + docs together.
