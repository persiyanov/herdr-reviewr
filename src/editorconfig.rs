//! The `EditorConfig` properties that affect source display.
//!
//! See `specs/diff-view.md`. Resolution happens when a file model is built, never while a
//! frame paints, so walking parent directories cannot turn rendering into filesystem work.

use std::path::Path;

use ec4rs::property::TabWidth;

/// The historical tab stop when no usable `EditorConfig` property applies.
pub const DEFAULT_TAB_WIDTH: usize = 4;

/// Resolve the tab stop for `path`, including `EditorConfig`'s `indent_size` fallback.
/// An unreadable, malformed, missing, or zero-width setting degrades to the historical default.
#[must_use]
pub fn tab_width(repo: &Path, path: &str) -> usize {
    let mut properties = ec4rs::properties_of(repo.join(path)).unwrap_or_default();
    properties.use_fallbacks();
    match properties.get::<TabWidth>() {
        Ok(TabWidth::Value(width)) if width > 0 => width,
        _ => DEFAULT_TAB_WIDTH,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{DEFAULT_TAB_WIDTH, tab_width};

    #[test]
    fn defaults_to_four_without_a_matching_setting() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".editorconfig"), "root = true\n[*.md]\ntab_width = 2\n")
            .unwrap();
        assert_eq!(tab_width(dir.path(), "src/main.rs"), DEFAULT_TAB_WIDTH);
    }

    #[test]
    fn closer_files_and_later_sections_win() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(".editorconfig"),
            "root = true\n[*]\ntab_width = 8\n[*.rs]\ntab_width = 6\n",
        )
        .unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/.editorconfig"), "[*.rs]\ntab_width = 2\n").unwrap();
        assert_eq!(tab_width(dir.path(), "src/main.rs"), 2);
        assert_eq!(tab_width(dir.path(), "README.md"), 8);
    }

    #[test]
    fn numeric_indent_size_is_the_standard_fallback() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".editorconfig"), "root = true\n[*]\nindent_size = 3\n").unwrap();
        assert_eq!(tab_width(dir.path(), "src/main.rs"), 3);
    }

    #[test]
    fn explicit_tab_width_wins_over_indent_size() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(".editorconfig"),
            "root = true\n[*.{rs,py}]\nindent_size = 2\ntab_width = 7\n",
        )
        .unwrap();
        assert_eq!(tab_width(dir.path(), "nested/main.rs"), 7);
    }

    #[test]
    fn unset_removes_an_inherited_width() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(".editorconfig"),
            "root = true\n[*]\ntab_width = 8\n[generated/**]\ntab_width = unset\n",
        )
        .unwrap();
        assert_eq!(tab_width(dir.path(), "generated/main.rs"), DEFAULT_TAB_WIDTH);
    }

    #[test]
    fn invalid_or_malformed_settings_keep_the_default() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".editorconfig"), "root = true\n[*]\ntab_width = 0\n").unwrap();
        assert_eq!(tab_width(dir.path(), "main.rs"), DEFAULT_TAB_WIDTH);

        fs::write(dir.path().join(".editorconfig"), "root = true\n[broken\n").unwrap();
        assert_eq!(tab_width(dir.path(), "main.rs"), DEFAULT_TAB_WIDTH);
    }
}
