//! Open a URL in the user's browser — the `PR` tab's only outward action.
//!
//! See `specs/forge-host.md` (external links). Mirrors the clipboard-tool probe in
//! `export.rs`: the first platform opener on `PATH` wins; none present errors clearly.

use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// Platform openers, tried in order: macOS `open`, then the Linux `xdg-open`.
const OPENERS: &[&str] = &["open", "xdg-open"];

/// Open `url` in the default browser via the first available opener. Errors when none is on
/// `PATH` (the caller surfaces it to the status line). The opener hands the URL to the browser
/// and exits at once, so this waits for it — reaping the child rather than leaving a zombie, and
/// returning fast enough for a click handler (mirrors the codebase's synchronous tool calls).
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

/// Gate a markdown link destination before it reaches the OS opener
/// (`specs/markdown.md`): trimmed, case-insensitive `http://`/`https://` with something
/// after the scheme, and no control or bidirectional-override character anywhere — a
/// destination the display would sanitize must never open as different bytes.
pub fn openable_url(url: &str) -> Result<&str, &'static str> {
    let trimmed = url.trim();
    let hostile = trimmed.chars().any(crate::markdown::hostile_char);
    let b = trimmed.as_bytes();
    let schemed = (b.len() > 7 && b[..7].eq_ignore_ascii_case(b"http://"))
        || (b.len() > 8 && b[..8].eq_ignore_ascii_case(b"https://"));
    if !hostile && schemed { Ok(trimmed) } else { Err("unsupported link scheme") }
}

#[cfg(test)]
mod tests {
    use super::openable_url;

    #[test]
    fn the_url_guard_admits_http_and_https_case_insensitively() {
        assert_eq!(openable_url("https://ci.example/1"), Ok("https://ci.example/1"));
        assert_eq!(openable_url("HTTP://ci.example"), Ok("HTTP://ci.example"));
        assert_eq!(openable_url("  https://x.dev  "), Ok("https://x.dev"), "trimmed");
    }

    #[test]
    fn the_url_guard_rejects_other_schemes_and_hostile_bytes() {
        for bad in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https:evil", // scheme without authority
            "https://",   // nothing after the scheme
            "ftp://host",
            "https://a\u{202e}b",   // bidi override
            "https://a\u{1b}[31mb", // control character
            "",
        ] {
            assert!(openable_url(bad).is_err(), "{bad:?} must not open");
        }
    }
}
