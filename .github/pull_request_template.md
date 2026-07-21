## What

<!-- One or two sentences: the behavior change or fix, from the user's side. -->

## Checklist

- [ ] `just ci` is green
- [ ] The governing spec under `specs/` says the new behavior (or the change touches no behavior)
- [ ] `CHANGELOG.md` has a bullet under `## [Unreleased]` (or the change is invisible to users)
- [ ] Perf-relevant paths (reload, render, git, highlight): before/after numbers from `scripts/bench_tui.py` are in the description
