//! Small helpers for locating external command-line tools.

/// Whether `name` resolves to an executable on `PATH` — a dependency-free `which`. On unix a
/// file in a `PATH` directory is the executable; on Windows the on-disk name usually carries
/// a `PATHEXT` extension (`clip` → `clip.exe`), so each extension is tried too. Shared by the
/// clipboard probe (`export.rs`) and the URL-opener probe (`browser.rs`).
#[must_use]
pub fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        if dir.join(name).is_file() {
            return true;
        }
        if cfg!(windows) {
            let pathext =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            return pathext.split(';').filter(|ext| !ext.is_empty()).any(|ext| {
                dir.join(format!("{name}{}", ext.to_lowercase())).is_file()
                    || dir.join(format!("{name}{ext}")).is_file()
            });
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use super::on_path;

    /// `clip.exe` ships in System32 on every Windows; `on_path("clip")` must find it even
    /// though the file on disk is `clip.exe`, not `clip`.
    #[test]
    #[cfg(windows)]
    fn bare_name_resolves_windows_executables() {
        assert!(on_path("clip"));
        assert!(!on_path("definitely-not-a-real-tool-name"));
    }

    #[test]
    #[cfg(unix)]
    fn bare_name_resolves_unix_executables() {
        assert!(on_path("sh"));
        assert!(!on_path("definitely-not-a-real-tool-name"));
    }
}
