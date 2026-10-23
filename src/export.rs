//! Comments formatted as `location`, snippet, text, and exported; the caller consumes on `Ok`.

use anyhow::Result;

use crate::forge;
use crate::herdr;
use crate::model::Comment;

/// One comment as its export block: location, snippet, then text.
pub fn format_comment(comment: &Comment) -> String {
    format!("{}\n{}\n{}", comment.location(), comment.lines, normalize_text(&comment.text))
}

/// Format one PR thread with its URL, location, snippet, and attributed comments and replies.
pub fn format_pr_thread(snapshot: &forge::PrSnapshot, comment: &forge::Comment) -> String {
    let mut parts = vec![snapshot.url.clone()];
    if let Some(place) = &comment.place {
        parts.push(pr_thread_location(place));
    }
    if let Some(snippet) = &comment.snippet {
        parts.push(snippet.trim_end_matches('\n').to_string());
    }
    parts.push(format!("Comment by {}:\n{}", comment.author, normalize_text(&comment.body)));
    for reply in &comment.replies {
        parts.push(format!("Reply by {}:\n{}", reply.author, normalize_text(&reply.body)));
    }
    parts.join("\n\n")
}

/// A finding's place as the local comment's own `location()` reads it: range, then side.
fn pr_thread_location(place: &forge::FindingPlace) -> String {
    let range = match place.range {
        Some((a, b)) if a == b => format!("{}:{a}", place.path),
        Some((a, b)) => format!("{}:{a}-{b}", place.path),
        None => place.path.clone(),
    };
    match place.side {
        Some(crate::model::Side::Old) => format!("{range} (removed)"),
        _ => range,
    }
}

/// Comment text without blank lines, which would read as the separator between comments.
fn normalize_text(text: &str) -> String {
    text.replace('\r', "")
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The exported review: the trimmed prefix, then comments by file and start line, a blank line between.
pub fn format_review(prefix: Option<&str>, comments: &[&Comment]) -> String {
    let mut sorted = comments.to_vec();
    sorted.sort_by(|a, b| a.file.cmp(&b.file).then(a.start.cmp(&b.start)));
    let prefix = prefix
        .map(|text| text.replace('\r', "").trim_end().to_owned())
        .filter(|text| !text.is_empty());
    prefix
        .into_iter()
        .chain(sorted.iter().map(|c| format_comment(c)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A destination comments can be exported to. Export succeeds or errors as a whole.
pub trait ExportTarget {
    fn export(&self, text: &str) -> Result<()>;
    fn label(&self) -> &'static str;
    /// Destination-specific confirmation shown after a successful export.
    fn success_message(&self, count: usize) -> String;
    /// The status after a failed export, `copy` naming the copy key; the cause goes to the log.
    fn failure_message(&self, error: &anyhow::Error, copy: &str) -> String;
}

/// The send failure's cause first, followed by a copy hint only where that action exists.
pub fn send_failure(error: &herdr::SendError, agent: Option<&str>, copy: Option<&str>) -> String {
    use herdr::{HerdrError as H, SendError as S};
    let cause = match error {
        S::AtPrompt(name) => return format!("answer {name}'s prompt first"),
        S::Herdr(H::PaneGone) => format!("{} closed", agent.unwrap_or("the agent")),
        S::NoAgent => "no agent in this workspace".to_string(),
        S::TooLarge => "review too large to send".to_string(),
        S::Herdr(H::Unanswered) => "herdr didn't answer".to_string(),
        S::Herdr(H::Refused(_) | H::Unreadable) => "herdr refused the send".to_string(),
    };
    match copy {
        Some(copy) => format!("{cause}, press {copy} to copy"),
        None => cause,
    }
}

/// Explain a typed send failure, falling back to a generic failure for unknown errors.
pub(crate) fn agent_send_failure(
    error: &anyhow::Error,
    agent: Option<&str>,
    copy: Option<&str>,
) -> String {
    error.downcast_ref().map_or_else(
        || match copy {
            Some(copy) => format!("send failed, press {copy} to copy"),
            None => "send failed".to_string(),
        },
        |error| send_failure(error, agent, copy),
    )
}

pub(crate) fn counted_comments(count: usize) -> String {
    let noun = if count == 1 { "comment" } else { "comments" };
    format!("{count} {noun}")
}

/// The system clipboard: the first tool on `PATH`, or the Win32 clipboard on Windows.
#[derive(Debug)]
pub struct Clipboard;

impl ExportTarget for Clipboard {
    fn label(&self) -> &'static str {
        "clipboard"
    }

    fn success_message(&self, count: usize) -> String {
        format!("copied {}", counted_comments(count))
    }

    fn failure_message(&self, error: &anyhow::Error, _copy: &str) -> String {
        match clipboard::remedy(error) {
            Some(remedy) => format!("copy failed: {remedy}"),
            None => "copy failed".to_string(),
        }
    }

    fn export(&self, text: &str) -> Result<()> {
        clipboard::write(text)
    }
}

/// macOS and Linux copy through a clipboard tool.
#[cfg(not(windows))]
mod clipboard {
    use std::io::Write;
    use std::process::Stdio;

    use anyhow::{Context, Result, bail};

    /// Clipboard tools that read stdin, first one on `PATH` wins.
    pub(super) const TOOLS: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];

    /// No clipboard tool on `PATH`, the one copy failure the reviewer can fix.
    #[derive(Debug)]
    pub(super) struct NoTool;

    impl std::fmt::Display for NoTool {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "no clipboard tool found (wl-clipboard, xclip, or xsel)")
        }
    }

    impl std::error::Error for NoTool {}

    pub(super) fn remedy(error: &anyhow::Error) -> Option<&'static str> {
        error.is::<NoTool>().then_some("install wl-clipboard, xclip, or xsel")
    }

    /// Pipe `text` into the first tool on `PATH`, succeeding only when it exits clean.
    pub(super) fn write(text: &str) -> Result<()> {
        let (cmd, args) = select_tool(TOOLS, crate::proc::on_path).ok_or(NoTool)?;
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

    /// The first clipboard tool the `present` predicate accepts, preserving list order.
    pub(super) fn select_tool(
        tools: &'static [(&'static str, &'static [&'static str])],
        present: impl Fn(&str) -> bool,
    ) -> Option<(&'static str, &'static [&'static str])> {
        tools.iter().copied().find(|(cmd, _)| present(cmd))
    }
}

/// The Win32 clipboard as Unicode text; `clip.exe` would mangle non-ASCII.
#[cfg(windows)]
mod clipboard {
    use anyhow::{Context, Result};

    /// Nothing to install, so no failure here has a remedy to name.
    pub(super) fn remedy(_error: &anyhow::Error) -> Option<&'static str> {
        None
    }

    /// Line breaks as CRLF, the Windows clipboard's own convention.
    pub(super) fn write(text: &str) -> Result<()> {
        let text = crate::text::crlf_line_breaks(text);
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(text))
            .context("writing the Windows clipboard")
    }
}

/// One chosen agent pane: fill its input in one socket request, then focus it.
#[derive(Clone, Debug)]
pub struct Agent {
    pub pane: String,
    pub name: String,
}

impl ExportTarget for Agent {
    fn label(&self) -> &'static str {
        "agent"
    }

    /// Names the agent: the reviewer's only record of where the review went.
    fn success_message(&self, count: usize) -> String {
        format!("sent {} to {}", counted_comments(count), self.name)
    }

    /// Every send failure is a [`herdr::SendError`]; anything else only says the send failed.
    fn failure_message(&self, error: &anyhow::Error, copy: &str) -> String {
        agent_send_failure(error, Some(&self.name), Some(copy))
    }

    /// An agent at a prompt refuses: the picker's rows can be minutes old.
    fn export(&self, text: &str) -> Result<()> {
        herdr::send_text(&self.pane, text)?;
        // A focus failure must not fail a delivered export.
        let _ = herdr::focus(&self.pane);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Agent, Clipboard, ExportTarget, agent_send_failure, format_comment, format_review,
        send_failure,
    };
    use crate::model::{Comment, Side};

    #[cfg(not(windows))]
    #[test]
    fn clipboard_tool_selection_prefers_list_order_and_can_be_empty() {
        use super::clipboard::{TOOLS, select_tool};
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

    #[test]
    fn export_confirmations_name_the_actual_result_and_pluralize_comments() {
        // The agent line names the pane it addressed.
        let agent = Agent { pane: "w8:p1".into(), name: "release-bot".into() };
        assert_eq!(agent.success_message(1), "sent 1 comment to release-bot");
        assert_eq!(agent.success_message(2), "sent 2 comments to release-bot");
        assert_eq!(Clipboard.success_message(1), "copied 1 comment");
        assert_eq!(Clipboard.success_message(2), "copied 2 comments");
    }

    #[test]
    fn a_failed_send_or_copy_says_what_to_do() {
        use crate::herdr::{HerdrError as H, SendError as S};
        let agent = Agent { pane: "w8:p1".into(), name: "release-bot".into() };
        let rows = [
            (S::Herdr(H::PaneGone), "release-bot closed, press y to copy"),
            (S::AtPrompt("codex".into()), "answer codex's prompt first"),
            (S::NoAgent, "no agent in this workspace, press y to copy"),
            (S::TooLarge, "review too large to send, press y to copy"),
            (S::Herdr(H::Unanswered), "herdr didn't answer, press y to copy"),
            // Any other refusal never claims the agent closed.
            (
                S::Herdr(H::Refused(Some("internal".into()))),
                "herdr refused the send, press y to copy",
            ),
            (S::Herdr(H::Refused(None)), "herdr refused the send, press y to copy"),
            (S::Herdr(H::Unreadable), "herdr refused the send, press y to copy"),
        ];
        for (error, line) in rows {
            let error = anyhow::Error::from(error);
            assert_eq!(agent.failure_message(&error, "y"), line, "{error}");
        }
        // Before any agent is chosen, a gone pane names no one.
        let gone = S::Herdr(H::PaneGone);
        assert_eq!(send_failure(&gone, None, Some("y")), "the agent closed, press y to copy");

        #[cfg(not(windows))]
        {
            let missing = anyhow::Error::from(super::clipboard::NoTool);
            assert_eq!(
                Clipboard.failure_message(&missing, "y"),
                "copy failed: install wl-clipboard, xclip, or xsel"
            );
            assert_eq!(
                Clipboard.failure_message(&anyhow::anyhow!("pbcopy exited non-zero"), "y"),
                "copy failed"
            );
        }
        // Windows has no tool to install, so a failed write never points at one.
        #[cfg(windows)]
        {
            let busy = anyhow::Error::from(arboard::Error::ClipboardOccupied)
                .context("writing the Windows clipboard");
            assert_eq!(Clipboard.failure_message(&busy, "y"), "copy failed");
        }
    }

    /// A send with no copy action to fall back to states the cause alone.
    #[test]
    fn a_send_with_no_copy_action_states_the_cause_alone() {
        use crate::herdr::{HerdrError as H, SendError as S};
        let rows = [
            (S::Herdr(H::PaneGone), "release-bot closed"),
            (S::AtPrompt("codex".into()), "answer codex's prompt first"),
            (S::NoAgent, "no agent in this workspace"),
        ];
        for (error, line) in rows {
            assert_eq!(send_failure(&error, Some("release-bot"), None), line, "{error:?}");
        }
        let error = anyhow::Error::from(S::NoAgent);
        assert_eq!(
            agent_send_failure(&error, Some("release-bot"), None),
            "no agent in this workspace"
        );
        assert_eq!(agent_send_failure(&anyhow::anyhow!("boom"), None, None), "send failed");
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
        let out = format_review(None, &[&b, &a2, &a1]);
        assert_eq!(out, "a.rs:3\n+z\nearlier\n\na.rs:20\n+y\nlater\n\nb.rs:5\n+x\ntwo");
    }

    #[test]
    fn the_prefix_heads_the_review_with_its_blank_lines_but_not_its_trailing_space() {
        let a = comment("a.rs", Side::New, 3, 3, "+z", "rename");
        assert_eq!(
            format_review(Some("/fix {all}\r\n\n  then test \n\n"), &[&a]),
            "/fix {all}\n\n  then test\n\na.rs:3\n+z\nrename"
        );
        assert_eq!(format_review(None, &[&a]), "a.rs:3\n+z\nrename");
        assert_eq!(format_review(Some(" \r\n"), &[&a]), "a.rs:3\n+z\nrename");
    }
}
