# Rebindable keys A/B (2026-08-17)

`bench_tui.py --fixture`, two interleaved runs per binary on a quiet system. Baseline is
`b0a997d` (v0.31.0) rebuilt in a separate worktree. New is the rebindable-keys branch.

Painted medians, baseline run1 / new run1 / baseline run2 / new run2 (ms):

| scenario                   | base1 | new1  | base2 | new2  |
| -------------------------- | ----- | ----- | ----- | ----- |
| tab_enter_all_files        | 116.2 | 131.6 | 148.8 | 148.1 |
| tab_enter_changes          | 110.1 | 108.3 | 109.0 | 130.3 |
| tab_enter_all_files_then_f | 133.6 | 116.8 | 132.9 | 133.0 |
| file_next_changes          | 43.0  | 34.3  | 42.7  | 39.9  |
| file_next_all_files        | 0.8   | 1.0   | 0.9   | 0.8   |

Deltas sit inside the baseline's own run-to-run swing (116→149 on tab_enter_all_files), in
both directions. Verdict: no regression signal. The change swaps footer hint strings and
relocates key-dispatch match arms; the keymap lookup grows 34→40 entries of linear scan.
