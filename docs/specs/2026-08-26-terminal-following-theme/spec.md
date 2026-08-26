# Terminal-following theme

Status: Approved
Date: 2026-08-26

## Problem

Every existing theme emits 24-bit RGB colors. Users whose terminal palette is already their source of truth cannot make reviewr follow that palette, and terminals without truecolor render the fixed themes incorrectly.

## Proposal

Add `terminal` as a theme name accepted by config and `--theme`. It uses terminal-default foreground and background for ordinary cells and named ANSI colors for semantic foregrounds. It does not query the terminal palette.

Terminal mode paints no base, deletion, or insertion row backgrounds. Orange maps to yellow.

Terminal syntax uses a project-owned sentinel `.tmTheme`. Every owned sentinel maps to a named ANSI color. Unknown syntax colors become reset. Fixed themes continue to accept only RGB syntax colors, with other syntax colors falling back to the palette text color. Terminal mode accepts only reset and named ANSI syntax colors. RGB and indexed syntax colors become reset.

Interaction state uses modifiers instead of generated fill colors:

- Cursor cells use reverse. Focused cursor cells also use bold.
- Selection uses reverse and underline.
- Search matches use reverse, bold, and underline.
- Inline emphasis uses bold.
- Modal scrims use dim.

The terminal resolves reset and ANSI identities from its active palette. A palette change therefore takes effect when the terminal redraws, without cache invalidation or a reviewr restart.

README and changelog distinguish fixed truecolor themes from terminal mode. The bundled sentinel theme is project-owned and MIT licensed.

## Invariants

False if the named test is red.

| code | Always true | Enforcement |
| ---- | ----------- | ----------- |
| TT-IDENTITY | Ordinary terminal-theme cells retain reset foreground and background, while semantic colors remain named ANSI identities. | `terminal_palette_uses_only_reset_and_named_colors` |
| TT-NO-FILLS | Terminal mode adds no RGB row, selection, emphasis, or search-match fill. | `terminal_production_rows_contain_only_terminal_colors_after_interaction_paint` |
| TT-SYNTAX | Owned syntax sentinels map to ANSI identities, and unknown or unsupported terminal syntax colors reset. | `terminal_syntax_maps_owned_sentinels_and_unknowns` |
| TT-FIXED | Existing fixed themes retain their RGB palette, fills, and syntax behavior. | `terminal_caret_uses_reset_and_reversed_but_fixed_caret_keeps_palette_style` |

## Alternatives

- Sample terminal RGB values. Palette changes would require querying and cache invalidation, and indexed terminals would still lose their color identities.
- Approximate terminal mode with a fixed 16-color RGB palette. The result would not follow the user's configured ANSI palette.
- Keep RGB row fills in terminal mode. Their colors cannot be derived without querying the terminal and can clash with the active palette.

## Out of scope

Automatic light or dark detection. User-defined palettes. Live switching to a different named fixed theme. New UI controls. Additional bundled fixed themes.
