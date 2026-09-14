//! Formatting comments and exporting them to the agent or clipboard.
//!
//! A comment becomes a block of `location`, the
//! diff snippet, then the text. Export is consume-on-success: the caller removes
//! a comment only after `export` returns `Ok`.

use std::io::Write;
use std::process::Stdio;

use anyhow::{Context, Result, bail};

use crate::forge;
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

/// One PR comment as its export block. A finding leads with its anchor and stored hunk, like a
/// written comment does — the quoted lines may be the only copy of the PR head the agent
/// shares, so they ride along whenever the forge saved them; the body is quoted under its
/// author, since it is forwarded review feedback, not this reviewer's own words.
pub fn format_pr_comment(c: &forge::Comment) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(place) = &c.place {
        lines.push(place.anchor());
        if let Some(hunk) = &c.snippet {
            lines.extend(hunk.lines().map(str::to_string));
        }
    }
    let kind = match c.kind {
        forge::CommentKind::Finding => None,
        forge::CommentKind::Review => Some("review"),
        forge::CommentKind::Comment => Some("comment"),
    };
    let who = match kind {
        Some(kind) => format!("@{} ({kind})", c.author),
        None => format!("@{}", c.author),
    };
    let body = c.body.trim_end();
    lines.push(if body.is_empty() { who } else { format!("{who}: {body}") });
    lines.join("\n")
}

/// Many PR comments in the snapshot's own order (newest first — the order the list shows),
/// one blank line between blocks.
pub fn format_pr_all(comments: &[forge::Comment]) -> String {
    comments.iter().map(format_pr_comment).collect::<Vec<_>>().join("\n\n")
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
/// installed (Wayland `wl-copy`, or X11 `xclip`/`xsel`). OSC 52 and Windows are roadmap.
const CLIPBOARD_TOOLS: &[(&str, &[&str])] = &[
    ("pbcopy", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

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
        let (cmd, args) = select_tool(CLIPBOARD_TOOLS, crate::proc::on_path).context(
            "no clipboard tool found (install wl-clipboard, xclip, or xsel) — \
             use Send instead",
        )?;
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

/// The first clipboard tool the `present` predicate accepts, preserving list order.
fn select_tool(
    tools: &'static [(&'static str, &'static [&'static str])],
    present: impl Fn(&str) -> bool,
) -> Option<(&'static str, &'static [&'static str])> {
    tools.iter().copied().find(|(cmd, _)| present(cmd))
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
    use super::{
        Agent, CLIPBOARD_TOOLS, Clipboard, ExportTarget, format_all, format_comment, format_pr_all,
        format_pr_comment, select_tool,
    };
    use crate::forge;
    use crate::model::{Comment, Side};

    #[test]
    fn clipboard_tool_selection_prefers_list_order_and_can_be_empty() {
        // None present -> no tool (the caller surfaces the "install one" error).
        assert!(select_tool(CLIPBOARD_TOOLS, |_| false).is_none());
        // Only an X11 tool present -> it's chosen, with its selection args.
        assert_eq!(
            select_tool(CLIPBOARD_TOOLS, |c| c == "xclip"),
            Some(("xclip", &["-selection", "clipboard"][..]))
        );
        // When several are present, earlier in the list wins (pbcopy over xclip).
        assert_eq!(
            select_tool(CLIPBOARD_TOOLS, |c| c == "pbcopy" || c == "xclip").map(|(cmd, _)| cmd),
            Some("pbcopy")
        );
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

    fn pr_comment(
        kind: forge::CommentKind,
        author: &str,
        anchor: &str,
        place: Option<forge::FindingPlace>,
        body: &str,
        snippet: Option<&str>,
    ) -> forge::Comment {
        forge::Comment {
            kind,
            author: author.into(),
            author_is_bot: false,
            anchor: anchor.into(),
            place,
            body: body.into(),
            snippet: snippet.map(str::to_string),
            created_at: "2026-06-27T10:00:00Z".into(),
            is_resolved: false,
            is_outdated: false,
            reply_count: 0,
        }
    }

    #[test]
    fn a_pr_finding_block_is_anchor_hunk_quoted_body() {
        let c = pr_comment(
            forge::CommentKind::Finding,
            "alice",
            "a.rs:2",
            Some(forge::FindingPlace { path: "a.rs".into(), range: Some((2, 2)), side: None }),
            "the beta line needs a test",
            Some("@@ -1 +1,2 @@\n-alpha\n+beta"),
        );
        assert_eq!(
            format_pr_comment(&c),
            "a.rs:2\n@@ -1 +1,2 @@\n-alpha\n+beta\n@alice: the beta line needs a test"
        );
    }

    #[test]
    fn a_pr_finding_without_a_stored_hunk_skips_the_quote() {
        let c = pr_comment(
            forge::CommentKind::Finding,
            "alice",
            "a.rs:2",
            Some(forge::FindingPlace { path: "a.rs".into(), range: Some((2, 2)), side: None }),
            "still right?",
            None,
        );
        assert_eq!(format_pr_comment(&c), "a.rs:2\n@alice: still right?");
    }

    #[test]
    fn a_pr_prose_block_names_author_and_kind_and_a_review_without_a_body_is_bare() {
        let review =
            pr_comment(forge::CommentKind::Review, "ann", "review", None, "approved", None);
        assert_eq!(format_pr_comment(&review), "@ann (review): approved");
        let comment = pr_comment(
            forge::CommentKind::Comment,
            "bob",
            "comment",
            None,
            "looks good overall",
            None,
        );
        assert_eq!(format_pr_comment(&comment), "@bob (comment): looks good overall");
        let bare = pr_comment(forge::CommentKind::Review, "ann", "review", None, "  ", None);
        assert_eq!(format_pr_comment(&bare), "@ann (review)");
    }

    #[test]
    fn pr_all_keeps_the_snapshots_order_with_blank_separators() {
        let newer = pr_comment(
            forge::CommentKind::Finding,
            "alice",
            "a.rs:2",
            Some(forge::FindingPlace { path: "a.rs".into(), range: Some((2, 2)), side: None }),
            "first",
            None,
        );
        let older = pr_comment(forge::CommentKind::Review, "ann", "review", None, "second", None);
        assert_eq!(
            format_pr_all(&[newer.clone(), older.clone()]),
            format!("{}\n\n{}", format_pr_comment(&newer), format_pr_comment(&older))
        );
    }
}
