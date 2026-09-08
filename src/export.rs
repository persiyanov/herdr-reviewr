//! Formatting comments and exporting them to the agent or clipboard.
//!
//! A comment becomes a block of `location`, the
//! diff snippet, then the text. Export is consume-on-success: the caller removes
//! a comment only after `export` returns `Ok`.

use std::io::Write;
use std::process::Stdio;

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
    /// Destination-specific confirmation shown after a successful export.
    fn success_message(&self, count: usize) -> String;
    /// Destination-specific line shown after a failed one. It is the whole status, so it is one
    /// short sentence a reviewer can read, never the underlying error. The cause goes to the log.
    fn failure_message(&self) -> String;
}

fn counted_comments(count: usize) -> String {
    let noun = if count == 1 { "comment" } else { "comments" };
    format!("{count} {noun}")
}

/// A clipboard tool and the args that make it read stdin into the system clipboard. Tried in
/// order — the first one present on `PATH` wins. macOS ships `pbcopy`; Linux needs one of these
/// installed (Wayland `wl-copy`, or X11 `xclip`/`xsel`); Windows uses `clip`.
#[cfg(windows)]
const CLIPBOARD_TOOLS: &[(&str, &[&str])] = &[("clip", &[])];

#[cfg(unix)]
const CLIPBOARD_TOOLS: &[(&str, &[&str])] = &[
    ("pbcopy", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

/// Platform-specific error message when no clipboard tool is found.
#[cfg(windows)]
const CLIPBOARD_NOT_FOUND_ERROR: &str =
    "no clipboard tool found — `clip` should be built into Windows; use Send instead";

#[cfg(unix)]
const CLIPBOARD_NOT_FOUND_ERROR: &str =
    "no clipboard tool found (install wl-clipboard, xclip, or xsel) — use Send instead";

/// The system clipboard, via the first available platform clipboard tool.
#[derive(Debug)]
pub struct Clipboard;

impl ExportTarget for Clipboard {
    fn label(&self) -> &'static str {
        "clipboard"
    }

    fn success_message(&self, count: usize) -> String {
        format!("copied {}", counted_comments(count))
    }

    fn failure_message(&self) -> String {
        "clipboard failed".to_string()
    }

    fn export(&self, text: &str) -> Result<()> {
        let (cmd, args) = crate::proc::select_first_present(CLIPBOARD_TOOLS, crate::proc::on_path)
            .context(CLIPBOARD_NOT_FOUND_ERROR)?;
        let mut child = crate::proc::command(cmd)
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

/// One chosen agent pane: fill its input via `herdr pane send-text`, then focus it.
///
/// The pane is decided before the export runs, by the sole-agent path or by the picker, and
/// nothing re-resolves it here. A pane that closed in between fails the send and keeps every
/// comment.
#[derive(Clone, Debug)]
pub struct Agent {
    pub pane: String,
    pub name: String,
}

impl ExportTarget for Agent {
    fn label(&self) -> &'static str {
        "agent"
    }

    /// Names the agent it addressed. The send is irreversible and consumes the whole set, so
    /// this line is the reviewer's only record of where the review went.
    fn success_message(&self, count: usize) -> String {
        format!("added {} to {}", counted_comments(count), self.name)
    }

    /// The pane was resolved before the send and closed in between, which is the only way this
    /// happens in practice. herdr's own wording is a JSON envelope around a pane id, so the
    /// reviewer gets this instead and the payload goes to the log.
    fn failure_message(&self) -> String {
        "agent not found".to_string()
    }

    fn export(&self, text: &str) -> Result<()> {
        herdr::send_text(&self.pane, text)?;
        // Focus is a convenience once the text is delivered; a focus failure must NOT fail the
        // export, or the comments stay unconsumed and the next Send duplicates the whole review.
        let _ = herdr::focus(&self.pane);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::CLIPBOARD_NOT_FOUND_ERROR;
    use super::{Agent, CLIPBOARD_TOOLS, Clipboard, ExportTarget, format_all, format_comment};
    use crate::model::{Comment, Side};
    use crate::proc::select_first_present;

    #[test]
    #[cfg(unix)]
    fn clipboard_tool_selection_prefers_list_order_and_can_be_empty() {
        // None present -> no tool (the caller surfaces the "install one" error).
        assert!(select_first_present(CLIPBOARD_TOOLS, |_| false).is_none());
        // Only an X11 tool present -> it's chosen, with its selection args.
        assert_eq!(
            select_first_present(CLIPBOARD_TOOLS, |c| c == "xclip"),
            Some(("xclip", &["-selection", "clipboard"][..]))
        );
        // When several are present, earlier in the list wins (pbcopy over xclip).
        assert_eq!(
            select_first_present(CLIPBOARD_TOOLS, |c| c == "pbcopy" || c == "xclip")
                .map(|(cmd, _)| cmd),
            Some("pbcopy")
        );
    }

    #[test]
    #[cfg(windows)]
    fn clipboard_tool_selection_prefers_list_order_and_can_be_empty() {
        // None present -> no tool (the caller surfaces the "install one" error).
        assert!(select_first_present(CLIPBOARD_TOOLS, |_| false).is_none());
        // clip is present -> it's chosen
        assert_eq!(select_first_present(CLIPBOARD_TOOLS, |c| c == "clip"), Some(("clip", &[][..])));
    }

    #[test]
    fn export_confirmations_name_the_actual_result_and_pluralize_comments() {
        // The agent line names the pane it addressed, so a mis-send is visible the moment it
        // lands.
        let agent = Agent { pane: "w8:p1".into(), name: "release-bot".into() };
        assert_eq!(agent.success_message(1), "added 1 comment to release-bot");
        assert_eq!(agent.success_message(2), "added 2 comments to release-bot");
        assert_eq!(Clipboard.success_message(1), "copied 1 comment");
        assert_eq!(Clipboard.success_message(2), "copied 2 comments");
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
            rev: crate::model::Rev::Worktree,
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

    #[test]
    fn export_windows_clip_selected_as_tool() {
        // On Windows, verify that `clip` is selected when present.
        let windows_tools: &[(&str, &[&str])] = &[("clip", &[])];
        assert_eq!(select_first_present(windows_tools, |c| c == "clip"), Some(("clip", &[][..])));
        // Also verify that if clip is absent, select_first_present returns None.
        assert!(select_first_present(windows_tools, |_| false).is_none());
    }

    #[test]
    #[cfg(windows)]
    fn export_windows_no_tool_error_is_windows_specific() {
        // Verify that the Windows error message doesn't mention Linux-specific tools.
        assert!(!CLIPBOARD_NOT_FOUND_ERROR.contains("wl-clipboard"));
        assert!(!CLIPBOARD_NOT_FOUND_ERROR.contains("xclip"));
        assert!(!CLIPBOARD_NOT_FOUND_ERROR.contains("xsel"));
    }
}
