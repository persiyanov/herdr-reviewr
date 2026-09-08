//! Open a URL in the user's browser — the `PR` tab's only outward action.
//!
//! Mirrors the clipboard-tool probe in
//! `export.rs`: the first platform opener on `PATH` wins; none present errors clearly.

use std::process::Stdio;

use anyhow::{Context, Result};

/// Platform openers, tried in order. Each entry is (program, `additional_args`).
/// Unix: macOS `open`, then Linux `xdg-open`.
/// Windows: `rundll32 url.dll,FileProtocolHandler <url>`.
#[cfg(unix)]
const OPENERS: &[(&str, &[&str])] = &[("open", &[]), ("xdg-open", &[])];

#[cfg(windows)]
const OPENERS: &[(&str, &[&str])] = &[("rundll32", &["url.dll,FileProtocolHandler"])];

/// The process for `opener` to open `url` — pure argv construction, no `PATH` probing, so a
/// test can inspect the exact argv (`Command::get_program`/`get_args`) a regression would
/// otherwise only be caught by an actual browser launch.
fn command_for(
    opener: (&'static str, &'static [&'static str]),
    url: &str,
) -> std::process::Command {
    let (cmd, args) = opener;
    let mut command = crate::proc::command(cmd);
    for arg in args {
        command.arg(arg);
    }
    command.arg(url);
    command
}

/// The process `open` would spawn for `url`, without spawning it — selects the platform opener
/// against the real `PATH`, then delegates to [`command_for`].
fn build_open_command(url: &str) -> Result<std::process::Command> {
    let opener = crate::proc::select_first_present(OPENERS, crate::proc::on_path).context(
        #[cfg(unix)]
        "no URL opener found (need `open` or `xdg-open`)",
        #[cfg(windows)]
        "no URL opener found (rundll32 required)",
    )?;
    Ok(command_for(opener, url))
}

/// Open `url` in the default browser via the first available opener. Errors when none is on
/// `PATH` (the caller surfaces it to the status line). The opener hands the URL to the browser
/// and exits at once, so this waits for it — reaping the child rather than leaving a zombie, and
/// returning fast enough for a click handler (mirrors the codebase's synchronous tool calls).
pub fn open(url: &str) -> Result<()> {
    let mut command = build_open_command(url)?;
    let cmd = command.get_program().to_string_lossy().into_owned();
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("spawning {cmd}"))?;
    if !status.success() {
        anyhow::bail!("{cmd} failed to open the URL");
    }
    Ok(())
}

/// Gate a markdown link destination before it reaches the OS opener
/// : trimmed, case-insensitive `http://`/`https://` with something
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
    use super::{OPENERS, command_for, openable_url};

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

    #[test]
    #[cfg(windows)]
    fn windows_opener_command_is_rundll32_with_the_file_protocol_handler_arg() {
        let command = command_for(OPENERS[0], "https://example.com");
        let program = command.get_program().to_string_lossy().to_lowercase();
        assert!(program.contains("rundll32"), "{program}");
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[0], "url.dll,FileProtocolHandler", "{args:?}");
    }

    #[test]
    fn an_ampersand_url_reaches_the_opener_as_one_unmangled_argument() {
        // Real `Command` introspection (`get_args`), not a restatement of the openers table:
        // if `.arg(url)` were ever replaced with shell-string concatenation, this would fail —
        // the URL would arrive split across several args, or embedded in a larger one.
        let url =
            "https://github.com/example/repo/compare/a...b?expand=1&tab=logs&check_suite_id=123";
        for opener in OPENERS {
            let command = command_for(*opener, url);
            let args: Vec<_> = command.get_args().collect();
            assert_eq!(
                args.last().map(|a| a.to_string_lossy()),
                Some(std::borrow::Cow::Borrowed(url)),
                "{opener:?}: {args:?}"
            );
            // The url is the last arg; anything before it is the opener's own fixed args,
            // never part of the url itself.
            assert_eq!(args.len(), opener.1.len() + 1, "{opener:?}: {args:?}");
        }
    }
}
