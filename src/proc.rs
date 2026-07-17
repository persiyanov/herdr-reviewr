//! Small helpers for locating external command-line tools.

/// Whether `name` resolves to an executable on `PATH` — a dependency-free `which`.
///
/// On unix a file at `<dir>/<name>` is the executable. On Windows an executable is identified by
/// its extension, so we also probe `<name>` plus each `PATHEXT` suffix (`.EXE`, `.CMD`, …) unless
/// `name` already carries one. Shared by the clipboard probe (`export.rs`) and the URL-opener
/// probe (`browser.rs`).
#[must_use]
pub fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let names = executable_names(name);
    std::env::split_paths(&path).any(|dir| names.iter().any(|n| dir.join(n).is_file()))
}

/// The filenames to probe for `name` in each `PATH` directory: just `name` on unix.
#[cfg(not(windows))]
fn executable_names(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

/// On Windows, `name` plus every `PATHEXT` extension — unless `name` already has an extension,
/// which is then used verbatim (`clip.exe`).
#[cfg(windows)]
fn executable_names(name: &str) -> Vec<String> {
    if std::path::Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    std::iter::once(name.to_string())
        .chain(pathext.split(';').filter(|e| !e.is_empty()).map(|ext| format!("{name}{ext}")))
        .collect()
}
