//! Open a URL in the user's browser — the `PR` tab's only outward action.
//!
//! See `specs/forge-host.md` (external links). Mirrors the clipboard-tool probe in
//! `export.rs`: the first platform opener on `PATH` wins; none present errors clearly.

use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// Platform openers, tried in order: macOS `open`, Linux `xdg-open`, Windows `explorer`
/// (`explorer.exe`, which ships in `C:\Windows` and is always on `PATH`).
const OPENERS: &[&str] = &["open", "xdg-open", "explorer"];

/// Open `url` in the default browser via the first available opener. Errors when none is on
/// `PATH` (the caller surfaces it to the status line). The opener hands the URL to the browser
/// and exits at once, so this waits for it — reaping the child rather than leaving a zombie, and
/// returning fast enough for a click handler (mirrors the codebase's synchronous tool calls).
pub fn open(url: &str) -> Result<()> {
    let tool = OPENERS.iter().copied().find(|t| crate::proc::on_path(t)).context(
        "no URL opener found (need `open` on macOS, `xdg-open` on Linux, or `explorer` on Windows)",
    )?;
    let status = Command::new(tool)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("spawning {tool}"))?;
    // `explorer.exe` returns exit code 1 even when it successfully opens the URL (a
    // long-documented quirk), so its exit status carries no information — only a spawn
    // failure above means anything for it. `open` and `xdg-open` exit non-zero on a real
    // failure, so their status is still checked.
    if tool != "explorer" && !status.success() {
        anyhow::bail!("{tool} failed to open the URL");
    }
    Ok(())
}
