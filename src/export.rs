//! Formatting comments and exporting them to the agent or clipboard.
//!
//! See `specs/review-model.md`. A comment becomes a block of `location`, the
//! diff snippet, then the text. Export is consume-on-success: the caller removes
//! a comment only after `export` returns `Ok`.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::herdr;
use crate::model::Comment;

/// One comment as its export block: location, snippet, then text.
pub fn format_comment(comment: &Comment) -> String {
    format!("{}\n{}\n{}", comment.location(), comment.lines, normalize_text(&comment.text))
}

/// Comment text for export: drop `\r`, trim trailing space per line, and drop blank
/// lines so a multi-line comment can never introduce the blank-line block separator.
fn normalize_text(text: &str) -> String {
    text.replace('\r', "")
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Many comments, sorted by file then start line, one blank line between blocks.
pub fn format_all(comments: &[&Comment]) -> String {
    let mut sorted = comments.to_vec();
    sorted.sort_by(|a, b| a.file.cmp(&b.file).then(a.start.cmp(&b.start)));
    sorted.iter().map(|c| format_comment(c)).collect::<Vec<_>>().join("\n\n")
}

/// A destination comments can be exported to. Export succeeds or errors as a whole.
pub trait ExportTarget {
    fn export(&self, text: &str) -> Result<()>;
    fn label(&self) -> &'static str;
}

/// A clipboard tool and the args that make it read stdin into the system clipboard. Tried in
/// order — the first one present on `PATH` wins. macOS ships `pbcopy`; Linux needs one of these
/// installed (Wayland `wl-copy`, or X11 `xclip`/`xsel`). OSC 52 is roadmap.
#[cfg(not(windows))]
const CLIPBOARD_TOOLS: &[(&str, &[&str])] = &[
    ("pbcopy", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

/// On Windows the built-in `clip` reads stdin into the clipboard; it's always present.
#[cfg(windows)]
const CLIPBOARD_TOOLS: &[(&str, &[&str])] = &[("clip", &[])];

/// Remediation shown when no clipboard tool resolves — platform-specific install hint.
#[cfg(not(windows))]
const NO_CLIPBOARD: &str =
    "no clipboard tool found (install wl-clipboard, xclip, or xsel) — use \"Add all to chat\" instead";
#[cfg(windows)]
const NO_CLIPBOARD: &str = "no clipboard tool found — use \"Add all to chat\" instead";

/// The system clipboard, via the first available platform clipboard tool.
#[derive(Debug)]
pub struct Clipboard;

impl ExportTarget for Clipboard {
    fn label(&self) -> &'static str {
        "clipboard"
    }

    fn export(&self, text: &str) -> Result<()> {
        let (cmd, args) = select_tool(CLIPBOARD_TOOLS, crate::proc::on_path).context(NO_CLIPBOARD)?;
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning {cmd}"))?;
        child
            .stdin
            .as_mut()
            .with_context(|| format!("{cmd} stdin unavailable"))?
            .write_all(text.as_bytes())
            .with_context(|| format!("writing to {cmd}"))?;
        if !child.wait().with_context(|| format!("waiting for {cmd}"))?.success() {
            bail!("{cmd} exited non-zero");
        }
        Ok(())
    }
}

/// The first clipboard tool the `present` predicate accepts, preserving list order.
fn select_tool(
    tools: &'static [(&'static str, &'static [&'static str])],
    present: impl Fn(&str) -> bool,
) -> Option<(&'static str, &'static [&'static str])> {
    tools.iter().copied().find(|(cmd, _)| present(cmd))
}

/// The agent pane: fill its input via `herdr agent send`, then focus it.
#[derive(Debug)]
pub struct Agent;

impl ExportTarget for Agent {
    fn label(&self) -> &'static str {
        "agent"
    }

    fn export(&self, text: &str) -> Result<()> {
        let pane = herdr::resolve_agent_pane()?;
        herdr::send_text(&pane, text)?;
        // Focus is a convenience once the text is delivered; a focus failure must NOT fail the
        // export, or the comments stay unconsumed and the next Send duplicates the whole review.
        let _ = herdr::focus(&pane);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{format_all, format_comment, select_tool};
    use crate::model::{Comment, Side};

    #[test]
    fn clipboard_tool_selection_prefers_list_order_and_can_be_empty() {
        // A fixed fixture, independent of the platform-specific CLIPBOARD_TOOLS, so the selection
        // logic is exercised identically on every OS.
        const TOOLS: &[(&str, &[&str])] = &[
            ("pbcopy", &[]),
            ("xclip", &["-selection", "clipboard"]),
        ];
        // None present -> no tool (the caller surfaces the "install one" error).
        assert!(select_tool(TOOLS, |_| false).is_none());
        // Only an X11 tool present -> it's chosen, with its selection args.
        assert_eq!(
            select_tool(TOOLS, |c| c == "xclip"),
            Some(("xclip", &["-selection", "clipboard"][..]))
        );
        // When several are present, earlier in the list wins (pbcopy over xclip).
        assert_eq!(
            select_tool(TOOLS, |c| c == "pbcopy" || c == "xclip").map(|(cmd, _)| cmd),
            Some("pbcopy")
        );
    }

    fn comment(file: &str, side: Side, start: u32, end: u32, lines: &str, text: &str) -> Comment {
        Comment {
            file: file.into(),
            side,
            start,
            end,
            lines: lines.into(),
            text: text.into(),
            diff_anchored: true,
        }
    }

    #[test]
    fn block_is_location_snippet_text() {
        let c = comment(
            "extruct/core/llm_registry.py",
            Side::New,
            40,
            41,
            "-from .z import w\n+from .x import y",
            "this import path looks wrong",
        );
        assert_eq!(
            format_comment(&c),
            "extruct/core/llm_registry.py:40-41\n-from .z import w\n+from .x import y\nthis import path looks wrong"
        );
    }

    #[test]
    fn removed_side_marks_the_header() {
        let c = comment("a.rs", Side::Old, 38, 38, "-    cleanup()", "still needed");
        assert_eq!(format_comment(&c), "a.rs:38 (removed)\n-    cleanup()\nstill needed");
    }

    #[test]
    fn multiline_text_keeps_breaks_but_drops_blank_lines() {
        let c = comment("a.rs", Side::New, 1, 1, "+x", "first line\n\n  \nsecond line\n");
        assert_eq!(format_comment(&c), "a.rs:1\n+x\nfirst line\nsecond line");
    }

    #[test]
    fn all_sorts_by_file_then_start_with_blank_separator() {
        let b = comment("b.rs", Side::New, 5, 5, "+x", "two");
        let a2 = comment("a.rs", Side::New, 20, 20, "+y", "later");
        let a1 = comment("a.rs", Side::New, 3, 3, "+z", "earlier");
        let out = format_all(&[&b, &a2, &a1]);
        assert_eq!(out, "a.rs:3\n+z\nearlier\n\na.rs:20\n+y\nlater\n\nb.rs:5\n+x\ntwo");
    }
}
