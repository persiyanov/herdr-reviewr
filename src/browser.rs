//! Open a URL in the user's browser — the `PR` tab's only outward action.
//!
//! See `specs/forge-host.md` (external links). Mirrors the clipboard-tool probe in
//! `export.rs`: the first platform opener on `PATH` wins; none present errors clearly.

use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// Platform openers, tried in order: macOS `open`, then the Linux `xdg-open`.
#[cfg(not(windows))]
const OPENERS: &[&str] = &["open", "xdg-open"];

/// Open `url` in the default browser via the first available opener. Errors when none is on
/// `PATH` (the caller surfaces it to the status line). The opener hands the URL to the browser
/// and exits at once, so this waits for it — reaping the child rather than leaving a zombie, and
/// returning fast enough for a click handler (mirrors the codebase's synchronous tool calls).
#[cfg(not(windows))]
pub fn open(url: &str) -> Result<()> {
    let tool = OPENERS
        .iter()
        .copied()
        .find(|t| crate::proc::on_path(t))
        .context("no URL opener found (need `open` or `xdg-open`)")?;
    let status = Command::new(tool)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("spawning {tool}"))?;
    if !status.success() {
        anyhow::bail!("{tool} failed to open the URL");
    }
    Ok(())
}

/// Open `url` in the default browser on Windows. Uses `rundll32 url.dll,FileProtocolHandler`
/// rather than `cmd /c start`: it's always present, hands the URL straight to the default
/// handler, and — unlike `cmd` — never re-parses `&` in a query string as a command separator.
#[cfg(windows)]
pub fn open(url: &str) -> Result<()> {
    let status = Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("spawning rundll32")?;
    if !status.success() {
        anyhow::bail!("rundll32 failed to open the URL");
    }
    Ok(())
}
