//! The terminal-free review state and its transitions.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::diff::{DiffCache, DiffLine, FileDiff, Notice, RenderedKind, ReviewedEdit, Row, View};
use crate::export::{Agent, ExportTarget, format_all};
use crate::file_list::{self, Entry, RowKind};
use crate::forge;
use crate::git;
use crate::herdr::{self, AgentChoice, SendTarget};
use crate::highlight::Highlighter;
use crate::logln;
use crate::marks::{MarkMap, Unit, diff_lines};
use crate::model::{
    ChangeKind, ChangedFile, Comment, CommentStore, CommitPick, FileIdentity, Rev, ReviewContext,
    Scope, Side,
};
use crate::rendered::{
    Built, Content, OldMap, RenderedIndex, RenderedInput, RenderedView, RowId, unit_of,
};
use crate::roles::Palette;
use crate::theme;
use crate::world::{Changeset, PickStatus, PickVerdict};

/// Navigator shares and bounds, as percentages of the body's split axis.
const DEFAULT_SIDE_PCT: u16 = 32;
const DEFAULT_STACK_PCT: u16 = 25;
const MIN_NAVIGATOR_PCT: u16 = 15;
const MAX_SIDE_PCT: u16 = 60;
const MAX_STACK_PCT: u16 = 50;
/// Pause after the last filter edit before probing an empty list as a rev
const BASE_PROBE_DELAY: Duration = Duration::from_millis(150);
/// The search screen's results share and its drag bounds.
const DEFAULT_SEARCH_PCT: u16 = 50;
const MIN_SEARCH_PCT: u16 = 10;
const MAX_SEARCH_PCT: u16 = 90;
/// The rendered markdown width before the first frame notes the real one.
const DEFAULT_RENDER_WIDTH: usize = 80;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DividerDrag {
    #[default]
    Idle,
    Active {
        position: crate::config::NavigatorPosition,
    },
    Cancelled,
}

/// Which pane has the keyboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Files,
    Diff,
}

/// A changed file's session-only review state in the active comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileReviewState {
    Unreviewed,
    Reviewed,
    ReviewedButChanged,
}

#[derive(Debug)]
struct ReviewMark {
    identity: FileIdentity,
    edits: Vec<ReviewedEdit>,
    changed: bool,
}

/// What the file cursor points at, by path, to restore after a rebuild.
enum Anchor {
    File(String),
    Dir(String),
}

/// The top-level tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Changes,
    AllFiles,
    Pr,
}

/// An ambient refresh rides a fetch in flight, a forced one supersedes it; `Ord` keeps the stronger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefreshKind {
    Ambient,
    Forced,
}

impl Tab {
    /// Whether this tab uses the file tree, diff, and per-tab stash.
    pub(crate) fn is_file_tab(self) -> bool {
        matches!(self, Tab::Changes | Tab::AllFiles)
    }
}

/// The inactive file tab's place state, swapped in on a switch.
#[derive(Debug, Default)]
struct TabStash {
    entries: Vec<Entry>,
    file_rows: Vec<file_list::Row>,
    file_cursor: usize,
    file_scroll: usize,
    toggled_dirs: HashSet<String>,
    diff: FileDiff,
    visible: Vec<Row>,
    expanded_folds: HashSet<u32>,
    diff_path: Option<String>,
    diff_cursor: usize,
    diff_scroll: usize,
    h_scroll: usize,
    select_anchor: Option<usize>,
    comment_target: Option<u64>,
    rendered: RenderedView,
    /// Whether this tab ever loaded; a first visit loads before the frame.
    visited: bool,
}

/// A file crossing waiting for its hunk step to repeat, holding the file it resolved.
#[derive(Clone, Debug)]
struct ArmedCross {
    forward: bool,
    path: String,
}

/// The open base picker; its rows freeze at open.
#[derive(Clone, Debug)]
pub struct BasePicker {
    /// Every branch, PR target and default first, plus a current non-branch pick.
    pub rows: Vec<BaseChoice>,
    /// The highlighted row, an index into the visible view.
    pub cursor: usize,
    /// The typed filter, matching anywhere in the name.
    pub query: String,
    /// The caret in `query`, a char index.
    pub caret: usize,
    /// Empty-list commit probe.
    pub probe: BaseProbe,
}

/// Empty-list commit probe while the base picker is open.
#[derive(Clone, Debug, Default)]
pub enum BaseProbe {
    #[default]
    Idle,
    Pending(Instant),
    Hit(BaseChoice),
    Miss,
}

/// One base picker row: a branch with its trail facts, or a revision with its oid.
#[derive(Clone, Debug)]
pub enum BaseChoice {
    Branch { name: String, pr_base: bool, is_default: bool, current: bool, tip_secs: u64 },
    Rev { name: String, oid: String },
}

impl BaseChoice {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Branch { name, .. } | Self::Rev { name, .. } => name,
        }
    }

    #[must_use]
    pub fn pr_base(&self) -> bool {
        matches!(self, Self::Branch { pr_base: true, .. })
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Branch { is_default: true, .. })
    }

    #[must_use]
    pub fn current(&self) -> bool {
        matches!(self, Self::Branch { current: true, .. })
    }

    /// The tip's commit time, `0` on a revision row or a probe hit.
    #[must_use]
    pub fn tip_secs(&self) -> u64 {
        match self {
            Self::Branch { tip_secs, .. } => *tip_secs,
            Self::Rev { .. } => 0,
        }
    }

    #[must_use]
    pub fn oid(&self) -> Option<&str> {
        match self {
            Self::Rev { oid, .. } => Some(oid),
            Self::Branch { .. } => None,
        }
    }
}

impl BasePicker {
    /// Rows the query fuzzily matches, best first, ties in frozen order.
    pub fn filtered(&self) -> Vec<usize> {
        if self.query.is_empty() {
            return (0..self.rows.len()).collect();
        }
        let names: Vec<&str> = self.rows.iter().map(BaseChoice::name).collect();
        // The default sort is score descending, then input order: the tie rule above.
        let config = neo_frizbee::Config::default();
        let matches = neo_frizbee::Matcher::new(self.query.as_str(), &config).match_list(&names);
        matches.into_iter().map(|m| m.index as usize).collect()
    }

    /// Whether a row spells the query, any case, so no revision probe runs.
    #[must_use]
    pub fn query_is_listed(&self) -> bool {
        self.rows.iter().any(|r| r.name().eq_ignore_ascii_case(&self.query))
    }

    /// The matches, then any probe hit appended, so the highlight never moves.
    pub fn visible(&self) -> Vec<&BaseChoice> {
        let mut rows: Vec<&BaseChoice> =
            self.filtered().into_iter().map(|i| &self.rows[i]).collect();
        if let BaseProbe::Hit(probe) = &self.probe
            && !rows.iter().any(|r| r.name() == probe.name())
        {
            rows.push(probe);
        }
        rows
    }
}

/// The open commit picker; a refresh re-lists its rows, reconciled by sha.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitPicker {
    /// The universe, newest first.
    pub rows: Vec<git::CommitRow>,
    /// An unlisted current pick, painted as one row above the list.
    pub pick_row: Option<CommitPick>,
    /// The highlighted row, an index into the visible view (`pick_row` first when present).
    pub cursor: usize,
    /// The anchor row, an index into the visible view, never the pick row.
    pub anchor: Option<usize>,
    /// The picker's title, naming the universe.
    pub title: String,
    /// The empty-universe message.
    pub empty: String,
    /// The `HEAD` the rows were listed under: a refresh re-lists only once it moves.
    pub head: Option<String>,
}

impl CommitPicker {
    /// The number of visible rows: the pick row, when present, plus the list.
    pub fn len(&self) -> usize {
        self.rows.len() + usize::from(self.pick_row.is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The visible index of the list row with `sha`, if listed.
    pub fn index_of(&self, sha: &str) -> Option<usize> {
        let offset = usize::from(self.pick_row.is_some());
        self.rows.iter().position(|r| r.sha == sha).map(|i| i + offset)
    }

    /// Whether both ends of `pick` are listed in order.
    pub fn wholly_lists(&self, pick: &CommitPick) -> bool {
        matches!((self.index_of(&pick.newest), self.index_of(&pick.oldest)), (Some(n), Some(o)) if o >= n)
    }

    /// The list row at visible index `i`, or `None` on the pick row.
    pub fn list_row(&self, i: usize) -> Option<&git::CommitRow> {
        let offset = usize::from(self.pick_row.is_some());
        if i < offset { None } else { self.rows.get(i - offset) }
    }

    /// Whether visible index `i` is the pick row.
    pub fn is_pick_row(&self, i: usize) -> bool {
        self.pick_row.is_some() && i == 0
    }

    /// The `(top, bottom)` run from anchor to highlight; the pick row is a run of itself.
    pub fn run(&self) -> (usize, usize) {
        match self.anchor {
            Some(a) if !self.is_pick_row(self.cursor) => (a.min(self.cursor), a.max(self.cursor)),
            _ => (self.cursor, self.cursor),
        }
    }

    /// How many rows the run spans, for the footer's `enter pick N`.
    pub fn run_len(&self) -> usize {
        if self.is_empty() {
            return 0;
        }
        let (top, bottom) = self.run();
        bottom - top + 1
    }

    /// Whether row `i` is in the anchored run.
    pub fn in_run(&self, i: usize) -> bool {
        if self.anchor.is_none() || self.is_pick_row(self.cursor) {
            return false;
        }
        let (top, bottom) = self.run();
        (top..=bottom).contains(&i)
    }

    /// The pick `enter` makes: the pick row itself, else the run's ends.
    pub fn picked(&self) -> Option<CommitPick> {
        if self.is_empty() {
            return None;
        }
        if self.is_pick_row(self.cursor) {
            return self.pick_row.clone();
        }
        let (top, bottom) = self.run();
        let newest = self.list_row(top)?;
        let oldest = self.list_row(bottom)?;
        Some(CommitPick { oldest: oldest.sha.clone(), newest: newest.sha.clone() })
    }
}

/// The interaction mode the UI is in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    /// Writing a comment; `editing` is the store index when editing an existing one.
    Composing {
        editing: Option<usize>,
    },
    /// Browsing the comments-list overlay.
    List,
    /// Choosing which agent a `Send` goes to.
    Picker,
    /// Choosing the `branch` scope's base.
    BasePick,
    /// Choosing the `commits` scope's pick.
    CommitPick,
    /// The search screen, replacing the body.
    Search,
    /// The read pane's foot band: find in the file, or a line to jump to.
    Find,
}

impl Mode {
    /// Whether this mode freezes the open diff and captures the mouse; `Search` and `Find` don't.
    pub fn is_modal(&self) -> bool {
        matches!(
            self,
            Mode::Composing { .. } | Mode::List | Mode::Picker | Mode::BasePick | Mode::CommitPick
        )
    }
}

/// The search screen's mode: which result set the list shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchMode {
    /// The engine's path matches, one row per file.
    Files,
    /// The engine's content matches, grouped by file.
    Code,
}

/// Where the search overlay stands with the engine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SearchPhase {
    /// The engine's first scan is still running: the overlay shows `indexing…`.
    Indexing,
    /// Results are painted; stale ones stay up while a newer query is in flight.
    Ready,
    /// The engine failed; the message shows inside the overlay.
    Error(String),
}

/// The picked result's file rendered as the read pane's File view, hit-centered
#[derive(Debug)]
pub struct SearchPreview {
    pub path: String,
    pub diff: crate::diff::FileDiff,
    /// A `Code` pick's line and matched byte spans.
    pub hit: Option<(u64, Vec<(u32, u32)>)>,
    /// Top visible row, centered on the hit once per build.
    pub scroll: std::cell::Cell<usize>,
    /// Cleared by the renderer after it centers the hit for this build.
    pub center: std::cell::Cell<bool>,
}

/// The search screen's state, dropped whole on close.
#[derive(Debug)]
pub struct SearchOverlay {
    pub query: String,
    /// The caret into `query`: a char index, edited by the shared caret ops.
    pub caret: usize,
    pub search_mode: SearchMode,
    /// The picked row, indexed into the active mode's result set.
    pub pick: usize,
    /// Top visible result row, kept by the renderer so the pick stays in view.
    pub scroll: std::cell::Cell<usize>,
    pub results: crate::search::SearchResults,
    pub phase: SearchPhase,
    /// The picked result's preview, rebuilt once input settles.
    pub preview: Option<SearchPreview>,
}

impl SearchOverlay {
    fn new() -> Self {
        Self {
            query: String::new(),
            caret: 0,
            search_mode: SearchMode::Files,
            pick: 0,
            scroll: std::cell::Cell::new(0),
            results: crate::search::SearchResults::default(),
            phase: SearchPhase::Indexing,
            preview: None,
        }
    }

    /// How many rows the pick can land on in the active mode.
    pub fn picks(&self) -> usize {
        match self.search_mode {
            SearchMode::Files => self.results.files.len(),
            SearchMode::Code => self.results.code.len(),
        }
    }

    /// The picked result in the active mode.
    pub fn picked(&self) -> Option<PickedResult<'_>> {
        match self.search_mode {
            SearchMode::Files => self.results.files.get(self.pick).map(PickedResult::File),
            SearchMode::Code => self.results.code.get(self.pick).map(PickedResult::Code),
        }
    }
}

/// One picked search result, borrowed from the overlay's results.
#[derive(Debug)]
pub enum PickedResult<'a> {
    File(&'a crate::search::FileHit),
    Code(&'a crate::search::CodeHit),
}

/// The foot band's query, find's or the line field's; matches and count derive from it each frame.
#[derive(Clone, Debug, Default)]
pub struct Find {
    /// What the band asks for: text to find, or a line to jump to.
    pub kind: BandKind,
    pub query: String,
    /// The caret into `query`: a char index, edited by the shared caret ops.
    pub caret: usize,
}

/// What the band at the read pane's foot asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BandKind {
    /// `ctrl+f`: text, stepped through match by match.
    #[default]
    Text,
    /// `:`: a line number, jumped to on Enter, in the file it opened over — another file or tab
    /// closes it.
    Line,
}

/// A found match in file order: how the cursor moves onto it.
enum FindHit {
    /// A visible row, at this `visible` index.
    Visible(usize),
    /// A row inside a collapsed fold, by its context line.
    Folded { new_no: u32 },
}

/// The char ranges of every non-overlapping `query` in `text`.
pub fn find_match_ranges(text: &str, query: &str, case_sensitive: bool) -> Vec<(u32, u32)> {
    if query.is_empty() {
        return Vec::new();
    }
    let q: Vec<char> = query.chars().collect();
    let eq = |a: char, b: char| if case_sensitive { a == b } else { a.eq_ignore_ascii_case(&b) };
    let chars: Vec<char> = text.chars().collect();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i + q.len() <= chars.len() {
        if (0..q.len()).all(|j| eq(chars[i + j], q[j])) {
            ranges.push((i as u32, (i + q.len()) as u32));
            i += q.len();
        } else {
            i += 1;
        }
    }
    ranges
}

/// Whether `query` is case-sensitive under smart-case: any uppercase character makes it so.
pub fn find_case_sensitive(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
}

/// A footer action; the renderer gives it a key and label.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FooterAction {
    Comment,
    ToggleReviewed,
    Select,
    ClearSelection,
    EditComment,
    EditFile,
    DeleteComment,
    JumpComment,
    ExpandFold,
    /// Take the armed crossing by repeating its hunk step.
    CrossFile {
        forward: bool,
    },
    /// The `move` band's cursor-movement pairs, each rendered as its two keys.
    MoveLine,
    MoveHunk,
    MoveFile,
    MovePage,
    ExpandDir,
    CollapseDir,
    /// Open the search screen — offered in every context, on every tab.
    Search,
    /// Open the in-file find band — offered wherever the read pane has content
    Find,
    /// The search screen's bar; the flip names its destination mode.
    FlipSearchMode,
    PickResult,
    OpenResult,
    CloseSearch,
    /// The find band's own bar: step between matches, and close.
    FindStep,
    CloseFind,
    /// Open the line field — offered wherever find is.
    GotoLine,
    /// The line field's own bar: jump to the typed line.
    LineGo,
    /// Switch focus between the file list and the diff; the label names the destination pane.
    TogglePane,
    /// Flip markdown between rendered and source, named by destination.
    Rendered,
    NavigatorPosition,
    /// Hide or show the navigator; hidden, it joins row 1.
    NavigatorHide,
    Wrap,
    Scope,
    Send,
    List,
    Copy,
    /// The quit question's answer that quits, naming the comments still pending.
    QuitDiscard,
    Save,
    Newline,
    Cancel,
    CloseList,
    /// The agent picker's bar.
    PickAgent,
    MovePickerRow,
    ClosePicker,
    /// Open the base picker.
    BasePick,
    /// The base picker's bar.
    PickBaseRow,
    MoveBaseRow,
    /// Open the commit picker.
    CommitPick,
    /// The commit picker's bar.
    PickCommitRun,
    MoveCommitRow,
    CommitAnchor,
    CloseCommitPicker,
    /// The scopes other than the one showing.
    ScopeOther,
    OpenPr,
    Refresh,
    Tabs,
    Quit,
}

/// Where a footer action sits: row 1, or a `?` band.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Band {
    Primary,
    Send,
    Do,
    Go,
    Move,
}

/// The file `edit` opens: a repository-relative path and the 1-based line to open it at
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditTarget {
    pub path: String,
    pub line: u32,
}

/// One mouse-down of a multi-click chain, and the count it reached.
#[derive(Clone, Copy, Debug)]
struct LastClick {
    at: std::time::Instant,
    col: u16,
    row: u16,
    target: usize,
    count: u8,
    surface: crate::selection::Surface,
}

/// The full state of the review session.
// The bools are independent toggles, not a hidden state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct App {
    pub repo: PathBuf,
    pub base: Option<String>,
    /// The `branch` scope's base, from the latest landed snapshot.
    pub branch_base: git::BaseStatus,
    /// The `commits` scope's pick, in memory, replaced but never cleared.
    pub commit_pick: Option<CommitPick>,
    /// The pick's verdict and subject from the latest `commits` build.
    pub pick_status: Option<PickStatus>,
    /// The commit picker's rows, highlight, and anchor while `Mode::CommitPick` is open
    pub commit_picker: Option<CommitPicker>,
    /// Bumped by each pick here, so a build of the old pick never lands.
    base_epoch: u64,
    pub scope: Scope,
    /// The active tab; it drives both panes and selects the per-tab state in play.
    pub tab: Tab,
    /// The file tab in the live fields, which stays put while `PR` is showing.
    active_file_tab: Tab,
    pub focus: Focus,
    /// The navigator's source: changed files, or the whole worktree.
    pub entries: Vec<Entry>,
    /// The flattened tree over `entries`, which `file_cursor` indexes.
    pub file_rows: Vec<file_list::Row>,
    pub file_cursor: usize,
    /// Top visible row of the file list.
    pub file_scroll: usize,
    /// Set by navigation, never the wheel, to reveal `file_cursor` next frame.
    pub reveal_files: bool,
    /// Set by navigation, never the wheel, to reveal `diff_cursor` next frame.
    pub reveal_diff: bool,
    /// Set by `goto_line`: center a cursor that landed off screen, not nudge it to the edge.
    pub reveal_center: bool,
    /// The file crossing a hunk step armed at the file's last hunk.
    armed_cross: Option<ArmedCross>,
    /// Whether this compose opened from the comments list, to return there.
    resume_list: bool,
    /// Directories toggled away from the tab's default, by path.
    toggled_dirs: HashSet<String>,
    /// The inactive tab's saved state, swapped in on a tab switch.
    stash: TabStash,
    /// The active scope's changed files and the ends they were diffed between, on every tab.
    changeset: Changeset,
    /// The semantic namespace that produced `changeset`.
    review_context: Option<ReviewContext>,
    /// Human-authored, session-only decisions by semantic comparison and path.
    reviewed: HashMap<ReviewContext, HashMap<String, ReviewMark>>,
    pub diff: FileDiff,
    /// Changed rows in the open diff whose whole base-anchored edit was reviewed unchanged.
    reviewed_diff_lines: HashSet<DiffLine>,
    /// Rendered Markdown units owned only by unchanged reviewed diff lines.
    reviewed_rendered_units: HashSet<Unit>,
    /// `diff.rows` with folds applied: what the cursor and hit tests index.
    pub visible: Vec<Row>,
    /// Fold anchors (first-hidden-line numbers) currently expanded; survives a refresh.
    expanded_folds: HashSet<u32>,
    /// The open diff's file, frozen with it while composing.
    pub diff_path: Option<String>,
    pub diff_cursor: usize,
    /// Top visible diff line, moved only to keep the cursor in view.
    pub diff_scroll: usize,
    /// Horizontal scroll, in columns, applied to the diff when wrap is off.
    pub h_scroll: usize,
    /// Whether long diff lines wrap (default) or are scrolled horizontally.
    pub wrap: bool,
    /// The open file's rendered markdown view, stashed per tab.
    rendered: RenderedView,
    /// The pane-wide markdown choice, flipped only by `m`.
    markdown_rendered: bool,
    /// The rendered rows' wrap width, `0` until the first frame.
    rendered_width: usize,
    /// The link regions painted this frame.
    painted_links: std::cell::RefCell<Vec<PaintedLink>>,
    /// The read pane's display lines as painted, which every hit test indexes.
    painted_slots: std::cell::RefCell<Vec<crate::ui::Slot>>,
    /// The whole body's heading anchors as `(slug, line)`.
    painted_anchors: std::cell::RefCell<Vec<(String, usize)>>,
    /// `<details>` summary hit boxes painted this frame.
    painted_details: std::cell::RefCell<Vec<PaintedDetails>>,
    /// Open `<details>` on the selected PR description or thread.
    pr_expanded_details: HashSet<String>,
    /// The PR read pane's maximum useful scroll.
    pr_read_max_scroll: std::cell::Cell<usize>,
    /// The global navigator placement and the separate shares remembered for each split axis.
    pub navigator_position: crate::config::NavigatorPosition,
    pub navigator_side_pct: u16,
    pub navigator_stack_pct: u16,
    /// Whether the navigator is hidden, on every tab.
    pub navigator_hidden: bool,
    /// The search screen's own results share.
    pub search_pct: u16,
    divider_drag: DividerDrag,
    pub select_anchor: Option<usize>,
    /// The picked comment among several on the cursor's row; only the reviewer's input clears it.
    comment_target: Option<u64>,
    /// The one live mouse gesture — born at mouse-down, ended on release or an interrupting event.
    pub gesture: crate::selection::Gesture,
    /// The last copy's span and text, highlighted until the next input or a text change.
    settled_sel: Option<(crate::selection::TextDrag, String)>,
    /// The pointer's last reported cell, for the gutter `+`.
    pub hover: Option<(u16, u16)>,
    /// The multi-click chain: the last mouse-down, while the window holds.
    last_click: Option<LastClick>,
    /// Whether a gesture held a reload, owed at its end.
    view_reload_held: bool,
    pub store: CommentStore,
    pub list_cursor: usize,
    /// The agent picker's rows, frozen at open.
    pub picker_rows: Vec<AgentChoice>,
    pub picker_cursor: usize,
    /// The mode the picker opened over, restored on close.
    pub picker_over: Mode,
    /// The agent of the last successful send, which arms the picker.
    pub last_sent_pane: Option<String>,
    /// The base picker's rows, filter, and highlight while `Mode::BasePick` is open
    pub base_picker: Option<BasePicker>,
    pub mode: Mode,
    pub input: String,
    /// The comment editor's caret: a char index into `input` (`0..=chars().count()`).
    pub caret: usize,
    pub status: String,
    /// Whether the `?` list is open, on every tab.
    pub keys_expanded: bool,
    pub should_quit: bool,
    /// Quit pressed with unsent comments; the next input answers (#119). Beside [`Mode`], not one.
    pub confirming_quit: bool,
    /// The read-only `PR` tab's view of the pull request.
    pub pr: forge::PrView,
    /// The resolved target's forge, which picks the display noun.
    pub pr_forge: crate::git::Forge,
    /// Persistent same-input fetch remedy shown without replacing the visible snapshot.
    pr_notice: Option<String>,
    /// A same-input refresh that crossed the loading-indicator delay.
    pr_refreshing: bool,
    /// The PR navigator's cursor over its rows (checks then comments).
    pub(crate) pr_cursor: usize,
    /// Top visible line of the PR read pane, reset when the selected comment changes.
    pub(crate) pr_read_scroll: usize,
    /// Top visible row of the PR navigator, independent of its selection.
    pr_nav_scroll: std::cell::Cell<usize>,
    /// The PR navigator's maximum useful scroll, noted by the renderer each frame.
    pr_nav_max_scroll: std::cell::Cell<usize>,
    /// A cursor move requests the smallest navigator scroll that reveals the selection.
    reveal_pr_nav: std::cell::Cell<bool>,
    /// The PR refresh to dispatch after the frame paints.
    pub pr_pending: Option<RefreshKind>,
    /// The world refresh to dispatch after the frame paints.
    pub world_request: Option<crate::world::WorldRequest>,
    /// The search overlay's state while `mode == Mode::Search`, `None` otherwise.
    pub search: Option<SearchOverlay>,
    /// A query to dispatch after the frame paints.
    pub search_dirty: bool,
    /// A picked path awaiting its frecency record; the event loop hands it to the worker.
    pub search_track: Option<String>,
    /// A file an `edit` press named, for the event loop to open.
    pub editor_request: Option<EditTarget>,
    /// The band's state — find or the line field — while `mode == Mode::Find`, `None` otherwise
    pub find: Option<Find>,
    /// Whether the refresh glyph paints this frame.
    pub refresh_indicator: bool,
    /// Set by `r`, so the glyph lights at once.
    pub refresh_commanded: bool,
    /// Whether the active file tab ever loaded.
    tab_visited: bool,
    highlighter: Highlighter,
    /// The active palette every renderer paints from.
    palette: Palette,
    /// The active theme's name, so re-resolving to the same theme is a no-op.
    theme_name: &'static str,
    /// The `--theme` override name (highest precedence); `None` lets the config file decide.
    cli_theme_name: Option<String>,
    /// The plugin is either ready with one validated snapshot or wholly blocked on its error.
    config: PluginConfigState,
    /// The last theme name requested, so re-resolving the same name skips work and logging.
    requested_theme_name: Option<String>,
    cache: DiffCache,
    /// The markdown render memo, filled by the renderer from `&App`.
    markdown_cache: std::cell::RefCell<crate::markdown::RenderCache>,
    snippet_cache: std::cell::RefCell<crate::snippet::SnippetRowCache>,
    /// What herdr said about this pane and its worktree's turns.
    pub herdr: HerdrView,
}

/// What herdr's connection said about this pane, carried whole across a config recovery.
#[derive(Debug)]
pub struct HerdrView {
    /// This pane's ids, which a move to another tab or workspace changes.
    pub ids: crate::herdr::PaneIds,
    /// Whether this pane is on screen: herdr's focus says, and input reaching it proves it.
    on_screen: bool,
    /// herdr is older than turn tracking needs, so `last-turn` never fills.
    too_old: bool,
    /// What `last-turn` diffs against, mirrored for the next build and the empty state.
    last_turn: crate::turn::LastTurn,
    /// Whether any agent is in this worktree; `None` until the connection has looked.
    agents_present: Option<bool>,
}

impl HerdrView {
    fn new(last_turn: crate::turn::LastTurn) -> Self {
        let ids = crate::herdr::PaneIds::from_env();
        Self { ids, on_screen: true, too_old: false, last_turn, agents_present: None }
    }
}

/// One painted link region: `x_start..x_end` on screen row `y`, in absolute cells.
#[derive(Clone, Debug)]
struct PaintedLink {
    x_start: u16,
    x_end: u16,
    y: u16,
    url: std::sync::Arc<str>,
}

#[derive(Clone, Debug)]
struct PaintedDetails {
    x_start: u16,
    x_end: u16,
    y: u16,
    /// The disclosure's [`crate::markdown::DetailsHit::key`].
    key: std::sync::Arc<str>,
}

#[derive(Debug)]
enum PluginConfigState {
    Ready(crate::config::PluginConfig),
    Blocked { error: String },
}

/// A synchronous scope/base/pick rebuild prepared entirely against prospective selection
/// state. Nothing in `App` changes until one of these complete builds exists.
enum RebaseBuild {
    World(crate::world::WorldSnapshot),
    Changes(crate::world::ScopeBuild),
}

impl App {
    pub fn new(repo: PathBuf, scope: Scope, base: Option<String>) -> Self {
        Self::build(repo, scope, base, true)
    }

    /// Construct the error-only reviewr pane without reading repository state.
    pub(crate) fn blocked(repo: PathBuf, scope: Scope, base: Option<String>) -> Self {
        Self::build(repo, scope, base, false)
    }

    fn build(repo: PathBuf, scope: Scope, base: Option<String>, load_turn: bool) -> Self {
        // Mirror the persisted baseline; the herdr connection owns the tracker.
        let baseline = load_turn.then(|| crate::git::read_baseline_ref(&repo)).flatten();
        let last_turn = baseline.map_or(crate::turn::LastTurn::Waiting, crate::turn::LastTurn::At);
        let theme = theme::resolve(None);
        Self {
            repo,
            base,
            branch_base: git::BaseStatus::default(),
            commit_pick: None,
            pick_status: None,
            commit_picker: None,
            base_epoch: 0,
            scope,
            tab: Tab::Changes,
            active_file_tab: Tab::Changes,
            focus: Focus::Files,
            entries: Vec::new(),
            file_rows: Vec::new(),
            file_cursor: 0,
            file_scroll: 0,
            reveal_files: false,
            reveal_diff: false,
            reveal_center: false,
            armed_cross: None,
            resume_list: false,
            toggled_dirs: HashSet::new(),
            stash: TabStash::default(),
            changeset: Changeset::default(),
            review_context: None,
            reviewed: HashMap::new(),
            diff: FileDiff::empty(),
            reviewed_diff_lines: HashSet::new(),
            reviewed_rendered_units: HashSet::new(),
            visible: Vec::new(),
            expanded_folds: HashSet::new(),
            diff_path: None,
            diff_cursor: 0,
            diff_scroll: 0,
            h_scroll: 0,
            wrap: true,
            rendered: RenderedView::default(),
            markdown_rendered: false,
            rendered_width: 0,
            painted_links: std::cell::RefCell::new(Vec::new()),
            painted_slots: std::cell::RefCell::new(Vec::new()),
            painted_anchors: std::cell::RefCell::new(Vec::new()),
            painted_details: std::cell::RefCell::new(Vec::new()),
            pr_expanded_details: HashSet::new(),
            pr_read_max_scroll: std::cell::Cell::new(usize::MAX),
            navigator_position: crate::config::NavigatorPosition::Right,
            navigator_side_pct: DEFAULT_SIDE_PCT,
            navigator_stack_pct: DEFAULT_STACK_PCT,
            navigator_hidden: false,
            search_pct: DEFAULT_SEARCH_PCT,
            divider_drag: DividerDrag::Idle,
            gesture: crate::selection::Gesture::None,
            settled_sel: None,
            hover: None,
            last_click: None,
            view_reload_held: false,
            select_anchor: None,
            comment_target: None,
            store: CommentStore::new(),
            list_cursor: 0,
            picker_rows: Vec::new(),
            picker_cursor: 0,
            picker_over: Mode::Normal,
            last_sent_pane: None,
            base_picker: None,
            mode: Mode::Normal,
            input: String::new(),
            caret: 0,
            status: String::new(),
            keys_expanded: false,
            should_quit: false,
            confirming_quit: false,
            pr: forge::PrView::Pending,
            pr_forge: crate::git::Forge::GitHub,
            pr_notice: None,
            pr_refreshing: false,
            pr_cursor: 0,
            pr_read_scroll: 0,
            pr_nav_scroll: std::cell::Cell::new(0),
            pr_nav_max_scroll: std::cell::Cell::new(usize::MAX),
            reveal_pr_nav: std::cell::Cell::new(true),
            pr_pending: None,
            world_request: None,
            search: None,
            search_dirty: false,
            search_track: None,
            editor_request: None,
            find: None,
            refresh_indicator: false,
            refresh_commanded: false,
            tab_visited: false,
            highlighter: Highlighter::new(theme.syntax),
            palette: theme.palette,
            theme_name: theme.name,
            cli_theme_name: None,
            config: PluginConfigState::Ready(crate::config::PluginConfig::default()),
            requested_theme_name: None,
            cache: DiffCache::new(),
            markdown_cache: std::cell::RefCell::new(crate::markdown::RenderCache::default()),
            snippet_cache: std::cell::RefCell::new(crate::snippet::SnippetRowCache::default()),
            herdr: HerdrView::new(last_turn),
        }
    }

    /// Apply theme `name` when it changes, rebuilding the highlighter and caches.
    fn set_theme(&mut self, name: Option<&str>) {
        // An unchanged name skips re-deriving and re-logging.
        if self.requested_theme_name.as_deref() == name {
            return;
        }
        self.requested_theme_name = name.map(str::to_owned);
        let theme = theme::resolve(name);
        if theme.name != self.theme_name {
            self.theme_name = theme.name;
            self.palette = theme.palette;
            self.highlighter = Highlighter::new(theme.syntax);
            self.cache = DiffCache::new();
            self.markdown_cache.borrow_mut().clear();
            self.snippet_cache.borrow_mut().clear();
        }
    }

    /// Record the `--theme` override name (highest precedence) and apply the resolved theme now.
    pub fn set_cli_theme(&mut self, name: Option<String>) {
        self.cli_theme_name = name;
        self.refresh_theme();
    }

    /// Apply one complete validated plugin configuration snapshot.
    pub fn set_plugin_config(&mut self, config: crate::config::PluginConfig) {
        let previous_position =
            self.plugin_config().map(crate::config::PluginConfig::navigator_position);
        let next_position = config.navigator_position();
        self.config = PluginConfigState::Ready(config);
        if previous_position != Some(next_position) {
            self.cancel_divider_drag();
            self.navigator_position = next_position;
        }
        self.refresh_theme();
    }

    /// The validated plugin configuration snapshot normal work currently uses.
    pub fn plugin_config(&self) -> Option<&crate::config::PluginConfig> {
        match &self.config {
            PluginConfigState::Ready(config) => Some(config),
            PluginConfigState::Blocked { .. } => None,
        }
    }

    /// Block the reviewr pane on one whole-file configuration failure.
    pub fn set_config_error(&mut self, error: String) {
        self.cancel_divider_drag();
        // The blocked pane never holds a live gesture (CFG-BLOCKED-INERT).
        self.cancel_gesture();
        // The picker closes first, onto its mode, so the closers below tear that mode down.
        self.close_picker();
        self.close_search();
        self.close_find();
        self.config = PluginConfigState::Blocked { error };
        self.pr_pending = None;
    }

    /// The active keymap: the snapshot's, or the defaults while blocked.
    pub fn keymap(&self) -> &crate::keymap::Keymap {
        match &self.config {
            PluginConfigState::Ready(config) => config.keymap(),
            PluginConfigState::Blocked { .. } => crate::keymap::default_keymap(),
        }
    }

    /// The error-only state rendered while plugin configuration is invalid.
    pub fn config_error(&self) -> Option<&str> {
        match &self.config {
            PluginConfigState::Ready(_) => None,
            PluginConfigState::Blocked { error, .. } => Some(error),
        }
    }

    /// Carry the review into a recovered app: comments, and a draft with its frozen diff.
    pub(crate) fn carry_authored_state_from(&mut self, old: &mut Self) {
        self.store = std::mem::take(&mut old.store);
        self.list_cursor = old.list_cursor;
        // The footer expansion is one global toggle, carried regardless of the recovered mode
        self.keys_expanded = old.keys_expanded;
        // Session memory, like the comments.
        self.last_sent_pane = old.last_sent_pane.take();
        // The commit pick is session memory like the comments: replaced, never cleared
        self.commit_pick = old.commit_pick.take();
        // Carry review decisions. Reconcile after deciding whether recovery keeps the freshly
        // loaded frame or the old modal's frozen one.
        self.reviewed = std::mem::take(&mut old.reviewed);
        self.navigator_side_pct = old.navigator_side_pct;
        self.navigator_stack_pct = old.navigator_stack_pct;
        self.navigator_hidden = old.navigator_hidden;
        // A hidden navigator keeps focus on the read pane.
        if self.navigator_hidden {
            self.focus = Focus::Diff;
        }
        self.search_pct = old.search_pct;
        // What herdr reported is reported once; a recovered app keeps it.
        self.herdr =
            std::mem::replace(&mut old.herdr, HerdrView::new(crate::turn::LastTurn::default()));
        // A pending refresh survives, or a carried stale frame waits for the next refresh.
        self.world_request = old.world_request.take();
        let old_mode = old.mode.clone();
        match old_mode {
            // `set_config_error` already closed these.
            Mode::Normal | Mode::Search | Mode::Find | Mode::Picker => {}
            Mode::List | Mode::Composing { .. } | Mode::BasePick | Mode::CommitPick => {
                self.scope = old.scope;
                self.tab = old.tab;
                self.active_file_tab = old.active_file_tab;
                self.focus = old.focus;
                self.entries = std::mem::take(&mut old.entries);
                self.file_rows = std::mem::take(&mut old.file_rows);
                self.file_cursor = old.file_cursor;
                self.file_scroll = old.file_scroll;
                self.reveal_files = old.reveal_files;
                self.reveal_diff = old.reveal_diff;
                self.changeset = std::mem::take(&mut old.changeset);
                self.review_context = old.review_context.take();
                // The header describes the carried list, so it carries too.
                self.branch_base = std::mem::take(&mut old.branch_base);
                self.pick_status = old.pick_status.take();
                self.diff = std::mem::take(&mut old.diff);
                self.visible = std::mem::take(&mut old.visible);
                self.expanded_folds = std::mem::take(&mut old.expanded_folds);
                self.diff_path = old.diff_path.take();
                self.diff_cursor = old.diff_cursor;
                self.diff_scroll = old.diff_scroll;
                self.h_scroll = old.h_scroll;
                self.select_anchor = old.select_anchor;
                self.comment_target = old.comment_target;
                self.resume_list = old.resume_list;
                self.toggled_dirs = std::mem::take(&mut old.toggled_dirs);
                self.stash = std::mem::take(&mut old.stash);
                self.wrap = old.wrap;
                self.markdown_rendered = old.markdown_rendered;
                self.rendered = std::mem::take(&mut old.rendered);
                self.rendered_width = old.rendered_width;
                self.pr_expanded_details = std::mem::take(&mut old.pr_expanded_details);
                self.mode = old.mode.clone();
                self.input = std::mem::take(&mut old.input);
                self.caret = old.caret;
                // The base picker survives recovery whole — rows, filter, and highlight
                self.base_picker = old.base_picker.take();
                // So does the commit picker, with its highlight and anchor.
                self.commit_picker = old.commit_picker.take();
            }
        }
        if let Some(context) = self.review_context.clone() {
            let changed = self.changeset.files.clone();
            self.reconcile_reviewed(&context, &changed);
        }
        self.sync_reviewed_rows();
    }

    fn config_snapshot(&self) -> &crate::config::PluginConfig {
        match &self.config {
            PluginConfigState::Ready(config) => config,
            PluginConfigState::Blocked { .. } => {
                unreachable!("normal work is gated while plugin configuration is invalid")
            }
        }
    }

    fn ensure_config_ready(&self) -> Result<()> {
        match &self.config {
            PluginConfigState::Ready(_) => Ok(()),
            PluginConfigState::Blocked { error } => {
                Err(anyhow::anyhow!("plugin configuration is invalid: {error}"))
            }
        }
    }

    /// Re-resolve the active theme from the CLI override or current validated snapshot.
    fn refresh_theme(&mut self) {
        let name = self
            .cli_theme_name
            .clone()
            .unwrap_or_else(|| self.config_snapshot().theme().to_owned());
        self.set_theme(Some(&name));
    }

    /// The active palette every renderer paints from.
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn composing(&self) -> bool {
        matches!(self.mode, Mode::Composing { .. })
    }

    /// The entry under the cursor, when it is on a file row.
    pub fn current_entry(&self) -> Option<&Entry> {
        self.file_under_cursor_index().map(|i| &self.entries[i])
    }

    /// Whether directories start expanded: in `Changes` only.
    fn default_expanded(&self) -> bool {
        self.tab == Tab::Changes
    }

    /// The `entries` index of the file row under the cursor, or `None` on a directory row.
    fn file_under_cursor_index(&self) -> Option<usize> {
        self.file_rows.get(self.file_cursor).and_then(file_list::Row::file_index)
    }

    /// The visible-row index of the file at `path`, for restoring selection across a refresh.
    fn file_row_of_path(&self, path: &str) -> Option<usize> {
        self.file_rows
            .iter()
            .position(|r| r.file_index().is_some_and(|i| self.entries[i].path == path))
    }

    /// The first file row, so a diff shows at once.
    fn first_file_row(&self) -> Option<usize> {
        self.file_rows.iter().position(|r| r.file_index().is_some())
    }

    /// Rebuild the flattened tree from `entries` and the toggled-directory set.
    fn rebuild_file_rows(&mut self) {
        self.file_rows =
            file_list::build(&self.entries, &self.toggled_dirs, self.default_expanded());
    }

    /// What the cursor points at, by path.
    fn cursor_anchor(&self) -> Option<Anchor> {
        self.file_rows.get(self.file_cursor).map(|r| match &r.kind {
            RowKind::File { index, .. } => Anchor::File(self.entries[*index].path.clone()),
            RowKind::Dir { path, .. } => Anchor::Dir(path.clone()),
        })
    }

    /// The visible-row index matching `anchor`, for restoring the cursor after a rebuild.
    fn row_of_anchor(&self, anchor: &Anchor) -> Option<usize> {
        self.file_rows.iter().position(|r| match (anchor, &r.kind) {
            (Anchor::File(p), RowKind::File { index, .. }) => &self.entries[*index].path == p,
            (Anchor::Dir(p), RowKind::Dir { path, .. }) => path == p,
            _ => false,
        })
    }

    /// The file the pane shows: the one under the cursor, else the open one, so a directory never blanks it.
    fn shown_entry(&self) -> Option<Entry> {
        if let Some(e) = self.current_entry() {
            return Some(e.clone());
        }
        let open = self.diff_path.as_deref()?;
        self.entries.iter().find(|e| e.path == open).cloned()
    }

    /// Build and reconcile synchronously; never touches comments or the draft.
    pub fn reload(&mut self) -> Result<()> {
        self.ensure_config_ready()?;
        // The `PR` tab has no file tree; a file tab reloads on return.
        if !self.tab.is_file_tab() {
            return Ok(());
        }
        // Outside a git repo, show an empty state rather than failing.
        if !git::is_repo(&self.repo) {
            self.entries.clear();
            self.changeset = Changeset::default();
            self.file_rows.clear();
            self.file_cursor = 0;
            self.file_scroll = 0;
            if !self.composing() {
                self.clear_open_view();
            }
            return Ok(());
        }
        let snapshot = crate::world::build(&self.world_input())?;
        self.reconcile_world(snapshot);
        Ok(())
    }

    /// The input the next world build reads, and the tag its snapshot must match.
    pub fn world_input(&self) -> crate::world::WorldInput {
        crate::world::WorldInput {
            repo: self.repo.clone(),
            tab: self.tab,
            scope: self.scope,
            base: self.base.clone(),
            base_epoch: self.base_epoch,
            turn_baseline: self.herdr.last_turn.tree().map(str::to_string),
            commit_pick: self.commit_pick.clone(),
            // `Changes` never reads the toggled set, so a toggle there invalidates nothing.
            toggled_dirs: if self.tab == Tab::AllFiles {
                self.toggled_dirs.clone()
            } else {
                HashSet::new()
            },
        }
    }

    /// Adopt a build's base, which only the `branch` scope owns.
    fn adopt_branch_base(&mut self, base: git::BaseStatus) {
        if self.scope == Scope::Branch {
            self.branch_base = base;
        }
    }

    /// Adopt a build's pick verdict, which only the `commits` scope owns.
    fn adopt_pick_status(&mut self, status: Option<PickStatus>) {
        if self.scope == Scope::Commits {
            self.pick_status = status;
        }
    }

    /// Land the comparison, its namespace, and its header metadata together.
    fn adopt_changeset(
        &mut self,
        review_context: ReviewContext,
        changeset: Changeset,
        branch_base: git::BaseStatus,
        pick_status: Option<PickStatus>,
    ) {
        self.reconcile_reviewed(&review_context, &changeset.files);
        self.review_context = Some(review_context);
        self.changeset = changeset;
        self.adopt_branch_base(branch_base);
        self.adopt_pick_status(pick_status);
    }

    /// Keep stale reviews visible, but forget paths that left this comparison.
    fn reconcile_reviewed(
        &mut self,
        context: &ReviewContext,
        changed: &std::collections::BTreeMap<String, ChangedFile>,
    ) {
        let Some(reviewed) = self.reviewed.get_mut(context) else { return };
        reviewed.retain(|path, mark| {
            let Some(annotation) = changed.get(path) else { return false };
            mark.changed |= !annotation.identity.same_file_comparison(&mark.identity);
            true
        });
        if reviewed.is_empty() {
            self.reviewed.remove(context);
        }
    }

    /// Reconcile a snapshot into the view: identity, then fallback, then clamp (Continuity).
    pub fn reconcile_world(&mut self, snapshot: crate::world::WorldSnapshot) {
        // The cursor keeps its target, else the open file, else the first file.
        let anchor = self.cursor_anchor();
        let open = self.diff_path.clone();
        // A path-limited build that missed the open file and its rename source leaves it as read.
        let open_untouched = snapshot.touched.as_ref().is_some_and(|touched| {
            open.as_ref().is_none_or(|p| match self.changeset.files.get(p) {
                Some(file) => !file.touched_by(touched),
                None => !crate::git::covered(touched, p),
            })
        });
        self.adopt_changeset(
            snapshot.review_context,
            snapshot.changeset,
            snapshot.branch_base,
            snapshot.pick_status,
        );
        self.entries = snapshot.entries;
        self.rebuild_file_rows();
        self.file_cursor = anchor
            .and_then(|a| self.row_of_anchor(&a))
            .or_else(|| open.as_deref().and_then(|p| self.file_row_of_path(p)))
            .or_else(|| self.first_file_row())
            .unwrap_or(0)
            .min(self.file_rows.len().saturating_sub(1));
        // A modal or a view-anchored drag freezes the diff, owing it a reload; the list still updates.
        if self.view_frozen() {
            self.view_reload_held = true;
        } else {
            let shown = self.shown_entry();
            let unchanged = open_untouched && shown.as_ref().map(|e| &e.path) == open.as_ref();
            if unchanged
                && let Some(landed) = shown.and_then(|entry| entry.annotation.map(|f| f.identity))
                && self
                    .diff
                    .identity
                    .as_ref()
                    .is_some_and(|loaded| loaded.same_file_comparison(&landed))
            {
                self.diff.identity = Some(landed);
            }
            if !unchanged {
                self.reload_open_view();
            }
        }
        // The preview refreshes; the results stay as their query found them.
        self.refresh_search_preview();
        // Find closes if its file changed identity or lost its searchable rows.
        if self.mode == Mode::Find && (open != self.diff_path || !self.find_available()) {
            self.close_find();
        }
        // A navigator highlight survives only while its text is unchanged.
        if let Some((d, text)) = &self.settled_sel
            && d.surface == crate::selection::Surface::Files
        {
            let (a, b) = d.ordered();
            if crate::selection::files_text(&self.file_rows, &self.entries, a.row, b.row) != *text {
                self.settled_sel = None;
            }
        }
        self.refresh_commit_picker(snapshot.head.as_deref());
        self.tab_visited = true;
    }

    /// The text of the read-pane row the multi-click chain targets.
    fn clicked_target_text(&self) -> Option<String> {
        let click = self.last_click?;
        if click.surface != crate::selection::Surface::Read {
            return None;
        }
        Some(self.visible.get(click.target)?.text())
    }

    /// Reload the open view; only a different file resets it and drops an armed crossing.
    fn reload_open_view(&mut self) {
        if self.shown_entry().map(|e| e.path) != self.diff_path {
            self.reset_diff_view();
            self.armed_cross = None;
        }
        self.load_read();
    }

    /// Show no file: no diff, rows, render, or marks.
    fn clear_open_view(&mut self) {
        self.diff = FileDiff::empty();
        self.reviewed_diff_lines.clear();
        self.reviewed_rendered_units.clear();
        self.diff_path = None;
        self.rendered.clear();
        self.visible.clear();
        self.drop_read_marks();
        self.reset_diff_view();
    }

    /// Load the read pane for the shown file.
    fn load_read(&mut self) {
        let Some(entry) = self.shown_entry() else {
            self.clear_open_view();
            return;
        };
        self.open_path_in_tab(entry.path);
    }

    /// Open `path`: its diff in `Changes`, its content in `All files`.
    fn open_path_in_tab(&mut self, path: String) {
        match self.tab {
            Tab::AllFiles => self.set_file_view(&path),
            // `Changes` (the `PR` tab never opens a file in the read pane).
            _ => self.set_diff(path),
        }
    }

    /// `path`'s rename or copy source in the landed changeset.
    fn rename_source(&self, path: &str) -> Option<String> {
        self.changeset.files.get(path).and_then(|a| a.previous_path.clone())
    }

    /// Build the diff for `path`, visible in the tree or not.
    fn set_diff(&mut self, path: String) {
        // Folds key by line number, so a different file starts with all collapsed.
        if self.diff_path.as_deref() != Some(path.as_str()) {
            self.expanded_folds.clear();
            self.open_fresh();
        }
        self.diff_path = Some(path.clone());
        let previous_path = self.rename_source(&path);
        let annotation = self.changeset.files.get(&path).cloned();
        let (old, new) = match self.content_sides(&path) {
            Ok((old, new)) => {
                let identity = annotation
                    .as_ref()
                    .and_then(|file| self.loaded_identity(&path, &file.identity, &new).ok());
                self.diff = match identity {
                    Some(identity) => self.cache.get_identified(
                        path,
                        previous_path,
                        &old,
                        &new,
                        identity,
                        &self.highlighter,
                    ),
                    None => self.cache.get(path, previous_path, &old, &new, &self.highlighter),
                };
                (old, new)
            }
            Err(notice) => {
                // The worker already certified the exact Git object behind this notice. Avoid
                // re-reading a potentially huge binary on the UI thread merely to paint it.
                let identity = match notice {
                    Notice::Binary | Notice::TooLarge => annotation
                        .as_ref()
                        .filter(|file| file.identity.live_side_certified())
                        .map(|file| file.identity.clone()),
                    Notice::Unreadable => None,
                };
                self.diff = match identity {
                    Some(identity) => {
                        FileDiff::notice_identified(path, previous_path, notice, identity)
                    }
                    None => FileDiff::notice(path, previous_path, notice, View::Diff),
                };
                (String::new(), String::new())
            }
        };
        // The new side is the render input, the old side the marks' base; a notice holds none.
        let renders = self.markdown_file() && self.diff.notice.is_none();
        self.rendered.content = if renders { self.content(new, Some(old)) } else { None };
        self.rebuild_visible();
        self.settle_read();
    }

    /// Reproduce a landed identity from the live side actually loaded for the read pane.
    fn loaded_identity(
        &self,
        path: &str,
        identity: &FileIdentity,
        loaded: &str,
    ) -> Result<FileIdentity> {
        if !identity.uses_live_worktree() {
            return Ok(identity.clone());
        }
        if identity.new_side_absent() && !identity.is_landed_deletion() {
            anyhow::bail!("the landed worktree side was not certified");
        }
        if identity.new_is_gitlink() {
            if identity.is_dirty_gitlink() {
                anyhow::bail!("a dirty submodule comparison cannot be certified exactly");
            }
            let (_, fingerprint) = git::worktree_gitlink_content_identity(&self.repo, path)?;
            return Ok(identity.with_loaded_worktree_fingerprint("160000", &fingerprint));
        }
        if loaded.contains('\u{fffd}') {
            anyhow::bail!("a lossy text comparison cannot be certified exactly");
        }
        let mode = git::worktree_mode(&self.repo, path, true)?;
        if identity.is_landed_deletion() && mode == "000000" {
            return Ok(identity.clone());
        }
        let fingerprint = git::blob_oid(&self.repo, loaded)?;
        Ok(identity.with_loaded_worktree_fingerprint(&mode, &fingerprint))
    }

    /// The `All files` File view for `path`: its worktree content, no folds.
    fn set_file_view(&mut self, path: &str) {
        // A different file opens rendered when markdown; a refresh keeps the choice.
        if self.diff_path.as_deref() != Some(path) {
            self.open_fresh();
        }
        self.diff_path = Some(path.to_string());
        self.expanded_folds.clear(); // the File view has no folds
        let (diff, content) = self.file_view(path);
        // A notice never renders, so its content is not held.
        let renders = self.markdown_file() && diff.notice.is_none();
        self.rendered.content = if renders { self.content(content, None) } else { None };
        self.diff = diff;
        self.rebuild_visible();
        self.settle_read();
    }

    /// The File view for `path`: the too-large notice unread, else the highlighted worktree content.
    fn file_view(&mut self, path: &str) -> (FileDiff, String) {
        match worktree_content(&self.repo, path) {
            Ok(content) => {
                let diff = self.cache.get_file(path.to_string(), &content, &self.highlighter);
                (diff, content)
            }
            Err(notice) => {
                (FileDiff::notice(path.to_string(), None, notice, View::File), String::new())
            }
        }
    }

    /// Clamp cursor, scroll and selection to `visible`; reveal only a forced move.
    fn settle_read(&mut self) {
        if self.visible.is_empty() {
            self.reset_diff_view();
            return;
        }
        let last = self.visible.len() - 1;
        let clamped = self.diff_cursor.min(last);
        if clamped != self.diff_cursor {
            self.reveal_diff = true;
        }
        self.diff_cursor = clamped;
        self.diff_scroll = self.diff_scroll.min(last);
        self.select_anchor = self.select_anchor.map(|a| a.min(last));
    }

    /// Reset per-file view state, so nothing carries from one file to the next.
    fn open_fresh(&mut self) {
        // Find follows the reviewer across files; the line field was typed for the one left.
        if self.line_open() {
            self.close_find();
        }
        self.rendered.details.clear();
        self.rendered.built = None;
        self.rendered.drop_rows();
        self.visible.clear();
        self.drop_read_marks();
    }

    /// Build `visible`: rendered rows, else source (an empty render shows source too).
    /// The one rebuild site, so it carries the place, the find band, and the marks.
    fn rebuild_visible(&mut self) {
        let clicked_before = self.clicked_target_text();
        let was_rendered = self.rendered.on_screen();
        // A line map, only when the text moved under the rendered rows.
        let text = self.rendered.text().unwrap_or_default();
        let edit = was_rendered
            .then_some(self.rendered.built.as_ref())
            .flatten()
            .filter(|b| b.input.text != text)
            .map(|b| LineMap::new(&b.input.text, text));
        let line_at = |i: usize| -> Option<u32> {
            if was_rendered {
                // A rendered row's current line: its unit's first line, through the edit.
                let src = self.rendered.index.unit_at(i)?.src;
                Some(edit.as_ref().map_or(src, |m| m.line(src)))
            } else {
                source_line_at(&self.visible, i)
            }
        };
        let place = (!self.visible.is_empty()).then(|| {
            let anchor = self.select_anchor.map(line_at);
            (was_rendered, line_at(self.diff_cursor), line_at(self.diff_scroll), anchor)
        });
        let rendered = self.wants_rendered() && self.rebuild_rendered(edit.as_ref());
        if !rendered {
            if !self.wants_rendered() {
                self.rendered.built = None;
            }
            // Source rows on screen: no render, marks, or index stand behind them.
            self.rendered.drop_rows();
            self.visible = self
                .diff
                .rows
                .iter()
                .flat_map(|row| match row {
                    Row::Fold { lines }
                        if row.fold_anchor().is_some_and(|a| self.expanded_folds.contains(&a)) =>
                    {
                        lines.clone()
                    }
                    _ => vec![row.clone()],
                })
                .collect();
        }
        // Across row kinds, the place crosses by source line.
        if let Some((from_rendered, cursor, scroll, anchor)) = place
            && from_rendered != rendered
        {
            let to = |line: Option<u32>| -> Option<usize> {
                let line = line?;
                if rendered {
                    self.rendered.index.row_at_line(line)
                } else {
                    Some(line_row(&self.visible, line, Row::new_no))
                }
            };
            if let Some(i) = to(cursor) {
                self.diff_cursor = i;
            }
            if let Some(i) = to(scroll) {
                self.diff_scroll = i;
            }
            if let Some(Some(i)) = anchor.map(to) {
                self.select_anchor = Some(i);
            }
        }
        if self.mode == Mode::Find && !self.find_available() {
            self.close_find();
        }
        self.sync_reviewed_rows();
        self.revalidate_read_marks(clicked_before.as_deref());
    }

    /// Keep the read pane's marks only where their text is unchanged (Continuity).
    fn revalidate_read_marks(&mut self, clicked_before: Option<&str>) {
        if self.clicked_target_text().as_deref() != clicked_before {
            self.last_click = None;
        }
        if let Some((d, copied)) = &self.settled_sel
            && d.surface == crate::selection::Surface::Read
        {
            let (a, b) = d.ordered();
            if crate::selection::read_text(&self.visible, a, b) != *copied {
                self.settled_sel = None;
            }
        }
    }

    /// Drop the read pane's marks; navigator marks stay.
    fn drop_read_marks(&mut self) {
        use crate::selection::Surface;
        if self.last_click.is_some_and(|c| c.surface == Surface::Read) {
            self.last_click = None;
        }
        if self
            .settled_sel
            .as_ref()
            .is_some_and(|(d, _)| matches!(d.surface, Surface::Read | Surface::Card { .. }))
        {
            self.settled_sel = None;
        }
    }

    /// The open `<details>` keys, from the reviewer's choices and what each holds.
    /// Keyed by summary, so a same-summary disclosure inserted above takes over a choice.
    fn derived_details(&self, disclosures: &[crate::markdown::Disclosure]) -> Vec<String> {
        if disclosures.is_empty() {
            return Vec::new();
        }
        let lines: Vec<&Row> = diff_lines(&self.diff.rows).collect();
        let mut spots = if self.diff.view == View::Diff {
            crate::marks::change_spots(&lines)
        } else {
            Vec::new()
        };
        if let Some(file) = self.diff_path.as_deref() {
            for c in self.store.iter().filter(|c| c.file == file && self.comment_in_view(c)) {
                match c.side {
                    Side::New => spots.push((c.start, c.end)),
                    Side::Old => {
                        spots.extend(crate::marks::old_range_spots(&lines, c.start, c.end));
                    }
                }
            }
        }
        crate::marks::open_details(disclosures, &self.rendered.details, &spots)
    }

    /// The input the rendered rows build from right now, with `details` open.
    fn rendered_input(&self, details: Vec<String>) -> RenderedInput {
        let changes = if self.diff.view == View::Diff {
            use std::hash::{Hash, Hasher};
            let mut h = std::hash::DefaultHasher::new();
            for row in diff_lines(&self.diff.rows).filter(|r| is_change(r)) {
                (row.marker(), row.old_no(), row.new_no()).hash(&mut h);
                for s in row.spans() {
                    s.text.hash(&mut h);
                }
            }
            Some(h.finish())
        } else {
            None
        };
        RenderedInput {
            text: self.rendered.text().unwrap_or_default().to_string(),
            details,
            width: if self.rendered_width == 0 {
                DEFAULT_RENDER_WIDTH
            } else {
                self.rendered_width
            },
            theme: self.theme_name,
            changes,
        }
    }

    /// The old side's source map with `open` disclosures opened, cached.
    fn old_map(&mut self, open: &[String]) -> crate::marks::DocMap {
        let old = self.rendered.content.as_ref().and_then(|c| c.old.as_deref()).unwrap_or_default();
        if let Some(cached) = &self.rendered.old_map
            && cached.text == old
            && cached.open == open
        {
            return cached.map.clone();
        }
        let set: HashSet<String> = open.iter().cloned().collect();
        let doc = crate::markdown::render_expanded(
            old,
            DEFAULT_RENDER_WIDTH,
            &self.highlighter,
            &self.palette,
            &set,
        );
        let map = crate::marks::doc_map(&doc);
        let text = old.to_string();
        self.rendered.old_map = Some(OldMap { text, open: open.to_vec(), map: map.clone() });
        map
    }

    /// Build the rendered rows unless their input is unchanged, carrying the place by identity.
    /// `false` when the content renders nothing, a verdict kept for the same content.
    fn rebuild_rendered(&mut self, edit: Option<&LineMap>) -> bool {
        let Some(text) = self.rendered.text().map(str::to_string) else { return false };
        let same_text = self.rendered.built.as_ref().is_some_and(|b| b.input.text == text);
        let details = if same_text && self.rendered.on_screen() {
            self.derived_details(&self.rendered.doc.disclosures)
        } else {
            self.rendered.built.as_ref().map(|b| b.input.details.clone()).unwrap_or_default()
        };
        let mut input = self.rendered_input(details);
        let showing = self.rendered.on_screen();
        if let Some(b) = self.rendered.built.as_ref()
            && b.input == input
            && (b.empty || showing)
        {
            return !b.empty;
        }
        let mut open: HashSet<String> = input.details.iter().cloned().collect();
        let mut doc = self.markdown_render(&text, input.width, &open);
        let derived = self.derived_details(&doc.disclosures);
        if derived != input.details {
            open = derived.iter().cloned().collect();
            input.details = derived;
            doc = self.markdown_render(&text, input.width, &open);
        }
        if doc.lines.is_empty() {
            self.rendered.built = Some(Built { input, empty: true });
            return false;
        }
        // The place, re-expressed in the new content's line numbers.
        let place = showing.then(|| {
            let id = |i: usize| {
                let id = self.visible.get(i).and_then(RowId::of)?;
                Some(edit.map_or(id, |m| id.map(|line| m.line(line))))
            };
            (id(self.diff_cursor), id(self.diff_scroll), self.select_anchor.map(id))
        });
        let mut rows = Vec::with_capacity(doc.lines.len());
        // A line's wrap counts earlier lines of its block on the same source line; gaps get none.
        let mut seen: HashMap<(usize, usize), u32> = HashMap::new();
        for (i, (line, meta)) in doc.lines.iter().zip(&doc.meta).enumerate() {
            let wrap = (!meta.gap).then(|| {
                let n = seen.entry((meta.source_line, meta.lines.0)).or_insert(0);
                *n += 1;
                *n - 1
            });
            let source = (meta.lines.0 as u32, meta.lines.1 as u32);
            rows.push(Row::Rendered {
                src: meta.source_line as u32,
                src_end: meta.source_end as u32,
                text: line.spans.iter().map(|s| s.content.as_ref()).collect(),
                kind: RenderedKind::Block { source, wrap, line: i as u32, bar: None, hides: None },
            });
        }
        let marks = if self.diff.view == View::Diff {
            let new_map = crate::marks::doc_map(&doc);
            let old_map = self.old_map(&input.details);
            let lines: Vec<&Row> = diff_lines(&self.diff.rows).collect();
            crate::marks::derive(&lines, &self.diff.pairs, &new_map, &old_map)
        } else {
            MarkMap::default()
        };
        mark_rendered(&mut rows, &doc, &marks, &input.details);
        self.rendered.index = RenderedIndex::build(&rows);
        self.rendered.marks = marks;
        self.rendered.doc = doc;
        self.rendered.built = Some(Built { input, empty: false });
        self.visible = rows;
        if let Some((cursor, scroll, anchor)) = place {
            let index = &self.rendered.index;
            // A live range's anchor reconciles by the same identity as the cursor.
            if let Some(i) = anchor.flatten().and_then(|id| index.row_of(id)) {
                self.select_anchor = Some(i);
            }
            if let Some(i) = cursor.and_then(|id| index.row_of(id)) {
                self.diff_cursor = i;
            }
            if let Some(i) = scroll.and_then(|id| index.row_of(id)) {
                self.diff_scroll = i;
            }
        }
        true
    }

    /// Rebuild the rendered rows when width or theme moved; a modal or gesture defers it.
    pub fn sync_rendered_width(&mut self, width: usize) {
        if width == 0 || !self.tab.is_file_tab() {
            return;
        }
        self.rendered_width = width;
        // Content and `<details>` rebuild elsewhere.
        if self
            .rendered
            .built
            .as_ref()
            .is_some_and(|b| b.input.width == width && b.input.theme == self.theme_name)
        {
            return;
        }
        if self.rendered.on_screen() && !self.view_frozen() {
            self.rebuild_visible();
            self.settle_read();
        }
    }

    /// Expand the fold under the cursor for good, growing toward the nearer viewport edge.
    pub fn expand_fold(&mut self, heights: &[usize], viewport: usize) {
        let fold_idx = self.diff_cursor;
        let Some(anchor) = self.visible.get(fold_idx).and_then(Row::fold_anchor) else {
            return;
        };
        // Expanding replaces the 1 fold row with N context rows; rows below it shift by N-1.
        let shift = self.visible[fold_idx].hidden().saturating_sub(1);
        // A fold above the viewport counts as top half, which holds the view in place too.
        let above: usize = heights.get(self.diff_scroll..fold_idx).map_or(0, |s| s.iter().sum());
        let top_half = above < viewport / 2;
        self.expanded_folds.insert(anchor);
        self.rebuild_visible();
        if top_half {
            self.diff_scroll += shift; // hold the content below the fold; grow upward
        }
        // bottom half: leave diff_scroll — the content above the fold stays put, grow downward
    }

    /// `path`'s sides from the landed build's record: a notice, or one `git diff` of the scope's ends.
    fn content_sides(&self, path: &str) -> Result<(String, String), crate::diff::Notice> {
        use crate::diff::Notice;
        // A path outside the landed changeset is a stale row: empty until the next reconcile.
        let (Some(annotation), Some(ends)) = (self.changeset.files.get(path), &self.changeset.ends)
        else {
            return Ok((String::new(), String::new()));
        };
        if annotation.binary {
            return Err(Notice::Binary);
        }
        // An untracked file is all new side, read raw.
        if annotation.kind == ChangeKind::Untracked {
            return worktree_content(&self.repo, path).map(|new| (String::new(), new));
        }
        // git diffs a tracked symlink as its target path, so the worktree side is the link's size.
        let new_size = annotation.new_size.unwrap_or_else(|| {
            std::fs::symlink_metadata(self.repo.join(path)).map_or(0, |m| m.len())
        });
        let total = annotation.old_size.saturating_add(new_size);
        if crate::diff::over_byte_budget(usize::try_from(total).unwrap_or(usize::MAX)) {
            return Err(Notice::TooLarge);
        }
        let source = annotation.previous_path.as_deref();
        match git::diff_sides(&self.repo, &ends.old, ends.new.as_deref(), path, source) {
            Ok(git::DiffSides::Text { old, new }) => Ok((old, new)),
            Ok(git::DiffSides::Binary) => Err(Notice::Binary),
            Err(error) => {
                logln!("diff of {path} failed: {error:#}");
                Err(Notice::Unreadable)
            }
        }
    }

    /// The `commits` scope over a pruned pick.
    pub fn commits_gone(&self) -> bool {
        self.scope == Scope::Commits && self.pick_gone()
    }

    /// Whether the last build found the pick pruned, kept across a scope switch.
    pub(crate) fn pick_gone(&self) -> bool {
        matches!(self.pick_status, Some(PickStatus { verdict: PickVerdict::Gone(_), .. }))
    }

    /// The message for a [`Self::commits_gone`] frame, naming the first missing commit.
    pub fn commits_gone_message(&self) -> String {
        match &self.pick_status {
            Some(PickStatus { verdict: PickVerdict::Gone(sha), .. }) => {
                format!("commit {} is gone", git::abbreviate_oid(sha))
            }
            _ => String::new(),
        }
    }

    /// Where the open diff's new side was read.
    fn current_rev(&self) -> Rev {
        match (self.scope, &self.commit_pick) {
            (Scope::Commits, Some(pick)) => Rev::Commit(pick.clone()),
            _ => Rev::Worktree,
        }
    }

    /// `last-turn` with no baseline captured yet.
    pub fn awaiting_turn(&self) -> bool {
        self.scope == Scope::LastTurn && self.herdr.last_turn.tree().is_none()
    }

    /// The message for an [`Self::awaiting_turn`] frame; "no agent" only once a sample looked.
    pub fn turn_wait_message(&self) -> &'static str {
        if self.herdr.too_old {
            return "last-turn needs a newer herdr";
        }
        if self.herdr.last_turn == crate::turn::LastTurn::Raced {
            return "the last turn started before reviewr could snapshot it";
        }
        match self.herdr.agents_present {
            Some(false) => "no agent works here",
            _ => "waiting for the first turn",
        }
    }

    /// The membership mirror, `None` until a sample observes it.
    pub fn agents_present(&self) -> Option<bool> {
        self.herdr.agents_present
    }

    /// Follow turn tracking: `last-turn` is authoritative, membership `None` (an undetermined
    /// member) holds the last answer. Whether `last-turn` on screen needs rebuilding.
    pub fn sync_turn(&mut self, report: crate::turn::TurnReport) -> bool {
        let moved = self.herdr.last_turn != report.last && self.scope == Scope::LastTurn;
        self.herdr.last_turn = report.last;
        self.herdr.agents_present = report.agents_present.or(self.herdr.agents_present);
        moved
    }

    /// Follow herdr's word on this pane: its ids, when the session still lists it, and its focus.
    /// The visibility it moved to, if it moved.
    pub fn sync_herdr_session(
        &mut self,
        ids: Option<crate::herdr::PaneIds>,
        visible: bool,
    ) -> Option<bool> {
        if let Some(ids) = ids {
            self.herdr.ids = ids;
        }
        self.set_pane_visible(visible)
    }

    /// The visibility it moved to, if it moved. Coming on screen refetches the PR tab and re-runs
    /// an open search, both held while hidden.
    fn set_pane_visible(&mut self, on: bool) -> Option<bool> {
        if on == self.herdr.on_screen {
            return None;
        }
        self.herdr.on_screen = on;
        if on {
            if self.tab == Tab::Pr {
                self.request_pr_refresh(RefreshKind::Ambient);
            }
            self.search_dirty |= self.mode == Mode::Search;
        }
        Some(on)
    }

    /// Queue a refresh the pacer let through: paths or a full rebuild, no reveal.
    pub fn request_paced_refresh(&mut self, refresh: crate::world::Refresh) {
        let paced = || crate::world::WorldRequest { background: true, ..Default::default() };
        self.world_request.get_or_insert_with(paced).refresh.absorb(refresh);
    }

    /// herdr answered with a version too old to track turns: said once, and on last-turn for good.
    pub fn herdr_too_old(&mut self) {
        self.herdr.too_old = true;
        let (major, minor, patch) = crate::herdr_socket::MIN_VERSION;
        self.status =
            format!("herdr is too old for turns and focus: reviewr needs {major}.{minor}.{patch}");
    }

    /// Run the reload a closed modal or an ended gesture owes the open view.
    pub fn catch_up_held_view(&mut self) {
        if self.view_reload_held && !self.view_frozen() {
            self.view_reload_held = false;
            // The held reload's rebuild revalidates a just-settled span against its text.
            self.reload_open_view();
        }
    }

    /// The ignored entries on screen, whose changes the watcher reports: expanded ignored folders
    /// in All files, and an open ignored file.
    pub fn shown_ignored(&self) -> std::collections::BTreeSet<String> {
        let ignored = |path: &str| self.entries.iter().any(|e| e.ignored && e.path == path);
        let mut shown: std::collections::BTreeSet<String> = if self.tab == Tab::AllFiles {
            self.toggled_dirs.iter().filter(|d| ignored(d)).cloned().collect()
        } else {
            std::collections::BTreeSet::new()
        };
        if let Some(open) = self.diff_path.as_deref().filter(|p| ignored(p)) {
            shown.insert(open.to_string());
        }
        shown
    }

    /// The branch scope's local base as a full ref, whose moves the watcher reports too.
    pub fn watched_refs(&self) -> std::collections::BTreeSet<String> {
        let base = self.branch_base.winner.as_ref().filter(|_| self.scope == Scope::Branch);
        let local = base.filter(|b| matches!(b, git::ResolvedBase::Branch { .. }));
        local.map(|b| format!("refs/heads/{}", b.name())).into_iter().collect()
    }

    /// Input reached the pane, so it is on screen until herdr next reports focus. Whether it just
    /// came on screen.
    pub fn note_input(&mut self) -> bool {
        self.set_pane_visible(true).is_some()
    }

    /// Whether this pane is on screen, as far as reviewr knows.
    pub fn pane_visible(&self) -> bool {
        self.herdr.on_screen
    }

    /// Queue a world refresh; `reveal` is for user-initiated switches only.
    pub fn request_world_refresh(&mut self, reveal: bool) {
        let request = self.world_request.get_or_insert(crate::world::WorldRequest::default());
        request.reveal |= reveal;
        request.background = false;
        request.refresh.absorb(crate::world::Refresh::Full);
    }

    /// Snap the diff view back to the top, clearing any pending selection.
    fn reset_diff_view(&mut self) {
        self.diff_cursor = 0;
        self.diff_scroll = 0;
        self.h_scroll = 0;
        self.select_anchor = None;
    }

    /// Scroll sideways by `delta` columns; inert while wrapping, so no offset piles up.
    pub fn scroll_h(&mut self, delta: isize) {
        if self.wrap || self.rendered_active() {
            return;
        }
        self.h_scroll = if delta >= 0 {
            self.h_scroll + delta as usize
        } else {
            self.h_scroll.saturating_sub(delta.unsigned_abs())
        };
    }

    /// Toggle line wrap; reset the horizontal scroll, which only applies with wrap off.
    pub fn toggle_wrap(&mut self) {
        if self.rendered_active() {
            return; // rendered rows come pre-wrapped by the renderer, so the toggle is inert
        }
        self.wrap = !self.wrap;
        self.h_scroll = 0;
    }

    /// Whether the open file is `.md` or `.markdown`, any case.
    #[must_use]
    fn markdown_file(&self) -> bool {
        self.diff_path.as_deref().is_some_and(is_markdown_path)
    }

    /// Whether the read pane shows rendered markdown right now.
    #[must_use]
    pub fn rendered_active(&self) -> bool {
        self.tab.is_file_tab() && self.rendered.on_screen()
    }

    /// The render input for `text`, judging once per text whether it renders nothing.
    fn content(&self, text: String, old: Option<String>) -> Option<Content> {
        if text.is_empty() {
            return None;
        }
        let nothing = match &self.rendered.content {
            Some(c) if c.text == text => c.nothing,
            _ => {
                self.markdown_render(&text, DEFAULT_RENDER_WIDTH, &HashSet::new()).lines.is_empty()
            }
        };
        Some(Content { text, old, nothing })
    }

    /// Whether the open file asks for rendered rows: the pane's choice over markdown content.
    fn wants_rendered(&self) -> bool {
        self.markdown_rendered && self.rendered.content.is_some()
    }

    /// Seed a fresh pane's markdown view; a reread never flips it.
    pub fn seed_from_config(&mut self, config: &crate::config::PluginConfig) {
        self.markdown_rendered = config.markdown_view() == crate::config::MarkdownView::Rendered;
    }

    /// Whether `m` acts: a file tab holding renderable markdown.
    fn toggle_acts(&self) -> bool {
        self.tab.is_file_tab() && self.rendered.content.is_some()
    }

    /// `m`: flip between rendered and source at the same block.
    pub fn toggle_rendered(&mut self) {
        if !self.toggle_acts() {
            return;
        }
        self.clear_selection();
        if self.rendered.renders_nothing() {
            self.status = "nothing here renders".to_string();
        } else {
            self.flip(!self.markdown_rendered);
        }
    }

    /// Flip to `rendered`, the cursor crossing by source line at the same screen height.
    fn flip(&mut self, rendered: bool) {
        let above = self.diff_cursor.saturating_sub(self.diff_scroll);
        self.markdown_rendered = rendered;
        self.rebuild_visible();
        self.diff_scroll = self.diff_cursor.saturating_sub(above);
        // The old rows' marks go with them; a navigator highlight was never on them.
        self.drop_read_marks();
        self.settle_read();
        self.reveal_diff = true;
    }

    /// The styled lines the rendered rows paint, indexed by a `Row::Rendered`'s `line`.
    #[must_use]
    pub(crate) fn rendered_lines(&self) -> &[ratatui::text::Line<'static>] {
        &self.rendered.doc.lines
    }

    /// Whether row `i` leads its rendered block.
    pub(crate) fn is_rendered_lead(&self, i: usize) -> bool {
        self.rendered
            .index
            .unit_at(i)
            .is_some_and(|u| matches!(u.unit, Unit::Block(_)) && u.lead == i)
    }

    /// The links and `<details>` of rendered line `line`, for the paint's hit regions.
    #[must_use]
    pub(crate) fn rendered_meta(&self, line: u32) -> Option<&crate::markdown::LineMeta> {
        self.rendered.doc.meta.get(line as usize)
    }

    /// Note the PR read pane's maximum useful scroll; the renderer calls this each frame.
    pub(crate) fn note_pr_read_max_scroll(&self, max: usize) {
        self.pr_read_max_scroll.set(max);
    }

    /// Record the navigator's painted scroll bound for wheel and page input.
    pub(crate) fn note_pr_nav_max_scroll(&self, max: usize) {
        self.pr_nav_max_scroll.set(max);
    }

    /// The first painted row in the PR navigator.
    #[must_use]
    pub(crate) fn pr_nav_scroll(&self) -> usize {
        self.pr_nav_scroll.get()
    }

    /// Set the bounded first row chosen by the renderer.
    pub(crate) fn set_pr_nav_scroll(&self, scroll: usize) {
        self.pr_nav_scroll.set(scroll);
    }

    /// Consume the request to reveal the selected PR row on this frame.
    pub(crate) fn take_pr_nav_reveal(&self) -> bool {
        self.reveal_pr_nav.replace(false)
    }

    /// Drop the last frame's recorded regions.
    pub(crate) fn clear_painted_frame(&self) {
        self.painted_links.borrow_mut().clear();
        self.painted_anchors.borrow_mut().clear();
        self.painted_details.borrow_mut().clear();
        self.painted_slots.borrow_mut().clear();
    }

    /// Record the read pane's painted layout for the hit tests.
    pub(crate) fn note_painted_slots(&self, slots: Vec<crate::ui::Slot>) {
        *self.painted_slots.borrow_mut() = slots;
    }

    /// The recorded layout on screen; empty without read-pane rows.
    #[must_use]
    pub(crate) fn painted_slots(&self) -> Vec<crate::ui::Slot> {
        self.painted_slots.borrow().clone()
    }

    /// Note one painted link region, in absolute screen cells.
    pub(crate) fn note_painted_link(
        &self,
        x_start: u16,
        x_end: u16,
        y: u16,
        url: std::sync::Arc<str>,
    ) {
        self.painted_links.borrow_mut().push(PaintedLink { x_start, x_end, y, url });
    }

    /// Note one heading anchor of the painted markdown body, by content line index.
    pub(crate) fn note_painted_anchor(&self, slug: String, content_line: usize) {
        self.painted_anchors.borrow_mut().push((slug, content_line));
    }

    /// The destination under `(col, row)` on the painted frame, if a link was there.
    #[must_use]
    pub fn painted_link_at(&self, col: u16, row: u16) -> Option<std::sync::Arc<str>> {
        self.painted_links
            .borrow()
            .iter()
            .find(|l| l.y == row && col >= l.x_start && col < l.x_end)
            .map(|l| l.url.clone())
    }

    /// Follow a link: `#anchor` scrolls to its heading, `http(s)` opens a browser.
    pub fn open_link(&mut self, url: &str) {
        if let Some(fragment) = url.strip_prefix('#') {
            // Normalized like the slugs, so `#Set-Up!` finds its heading.
            self.jump_to_anchor(&crate::markdown::slug_text(fragment));
            return;
        }
        if let Ok(clean) = crate::browser::openable_url(url) {
            let opener = self.plugin_config().and_then(crate::config::PluginConfig::url_opener);
            match crate::browser::open(clean, opener) {
                Ok(()) => self.status = "opened link in browser".to_string(),
                Err(e) => self.status = e.to_string(),
            }
        }
    }

    /// Bring `slug`'s heading to the top, moving the cursor too in a file view.
    fn jump_to_anchor(&mut self, slug: &str) {
        if self.tab == Tab::Pr {
            let target =
                self.painted_anchors.borrow().iter().find(|(s, _)| s == slug).map(|(_, i)| *i);
            if let Some(idx) = target {
                self.pr_read_scroll = idx.min(self.pr_read_max_scroll.get());
            }
        } else if self.rendered_active() {
            let Some(&(_, line)) = self.rendered.doc.anchors.iter().find(|(s, _)| s == slug) else {
                return;
            };
            let at = |r: &Row| {
                matches!(r, Row::Rendered { kind: RenderedKind::Block { line: l, .. }, .. }
                    if *l as usize == line)
            };
            if let Some(row) = self.visible.iter().position(at) {
                self.select_anchor = None;
                self.diff_cursor = row;
                self.diff_scroll = row;
            }
        }
    }

    /// Render `text` at `width` through the memo, with `open` disclosures opened.
    #[must_use]
    pub(crate) fn markdown_render(
        &self,
        text: &str,
        width: usize,
        open: &HashSet<String>,
    ) -> crate::markdown::Rendered {
        self.markdown_cache.borrow_mut().get_expanded(
            text,
            width,
            &self.highlighter,
            &self.palette,
            open,
        )
    }

    /// Render one PR thread body, its disclosures keyed apart from the others'.
    pub(crate) fn pr_body_render(
        &self,
        text: &str,
        width: usize,
        body: usize,
    ) -> crate::markdown::Rendered {
        let ns = format!("{body}/");
        let expanded: HashSet<String> = self
            .pr_expanded_details
            .iter()
            .filter_map(|k| k.strip_prefix(&ns).map(str::to_string))
            .collect();
        let mut rendered = self.markdown_cache.borrow_mut().get_expanded(
            text,
            width,
            &self.highlighter,
            &self.palette,
            &expanded,
        );
        for d in rendered.meta.iter_mut().filter_map(|m| m.details.as_mut()) {
            d.key = std::sync::Arc::from(format!("{ns}{}", d.key));
        }
        rendered
    }

    pub(crate) fn note_painted_details(
        &self,
        x_start: u16,
        x_end: u16,
        y: u16,
        key: std::sync::Arc<str>,
    ) {
        self.painted_details.borrow_mut().push(PaintedDetails { x_start, x_end, y, key });
    }

    #[must_use]
    pub fn painted_details_at(&self, col: u16, row: u16) -> Option<std::sync::Arc<str>> {
        self.painted_details
            .borrow()
            .iter()
            .find(|d| d.y == row && col >= d.x_start && col < d.x_end)
            .map(|d| d.key.clone())
    }

    /// Toggle a `<details>`; in a file tab the choice then overrides the derived state.
    pub fn toggle_details(&mut self, key: &str) {
        if self.tab == Tab::Pr {
            if !self.pr_expanded_details.remove(key) {
                self.pr_expanded_details.insert(key.to_string());
            }
        } else {
            if !self.rendered_active() {
                return; // only a painted disclosure takes the click
            }
            let open = self.details_open(key);
            self.rendered.details.insert(key.to_string(), !open);
        }
        if self.rendered_active() {
            self.rebuild_visible();
            self.settle_read();
        }
    }

    pub fn expand_pr_details(&mut self) {
        // A disclosure's key is its summary and occurrence, the same at any width.
        let width = DEFAULT_RENDER_WIDTH;
        let bodies = self.pr_markdown_bodies();
        let mut keys = HashSet::new();
        for (i, text) in bodies.iter().enumerate() {
            for m in &self.pr_body_render(text, width, i).meta {
                if let Some(d) = &m.details {
                    keys.insert(d.key.to_string());
                }
            }
        }
        self.pr_expanded_details.extend(keys);
    }

    pub fn collapse_pr_details(&mut self) {
        self.pr_expanded_details.clear();
    }

    fn pr_markdown_bodies(&self) -> Vec<String> {
        if self.pr_on_description() {
            return self.pr_snapshot().map(|s| vec![s.body.clone()]).unwrap_or_default();
        }
        let Some(cm) = self.pr_selected_comment() else {
            return Vec::new();
        };
        let mut bodies = vec![cm.body.clone()];
        bodies.extend(cm.replies.iter().map(|r| r.body.clone()));
        bodies
    }

    /// Finding hunk rows for the PR read pane, through the one-slot memo.
    #[must_use]
    pub(crate) fn snippet_rows(
        &self,
        hunk: &str,
        path: &str,
        start: u32,
        end: u32,
        side: crate::model::Side,
    ) -> Vec<crate::diff::Row> {
        self.snippet_cache.borrow_mut().get(hunk, path, start, end, side, &self.highlighter)
    }

    /// The navigator share remembered for the active side or stacked axis.
    #[must_use]
    pub fn navigator_share(&self) -> u16 {
        if self.navigator_position.stacked() {
            self.navigator_stack_pct
        } else {
            self.navigator_side_pct
        }
    }

    /// Move the navigator clockwise, cancelling a divider drag; inert while hidden.
    pub fn cycle_navigator_position(&mut self) {
        if self.navigator_hidden_here() {
            return;
        }
        self.cancel_divider_drag();
        self.navigator_position = self.navigator_position.clockwise();
    }

    /// Whether the active tab can hide its navigator — `PR` never does.
    fn navigator_can_hide(&self) -> bool {
        self.tab != Tab::Pr
    }

    /// Whether the hidden state applies on the active tab.
    #[must_use]
    pub fn navigator_hidden_here(&self) -> bool {
        self.navigator_hidden && self.navigator_can_hide()
    }

    /// Hide the navigator, focusing the read pane, or show it back; inert on `PR`.
    pub fn toggle_navigator_hidden(&mut self) {
        if !self.navigator_can_hide() {
            return;
        }
        self.cancel_divider_drag();
        self.navigator_hidden = !self.navigator_hidden;
        if self.navigator_hidden {
            self.focus = Focus::Diff;
        } else {
            // The reveal deferred while hidden.
            self.reveal_files = true;
        }
    }

    /// Resize the navigator by `delta` points; inert while hidden.
    pub fn resize_navigator(&mut self, delta: i16) {
        if self.navigator_hidden_here() {
            return;
        }
        let next = (self.navigator_share() as i16).saturating_add(delta).max(0) as u16;
        self.set_navigator_share(next);
    }

    /// Capture a divider gesture for the current position; cancelled capture waits for mouse-up.
    pub fn start_divider_drag(&mut self) {
        if self.divider_drag != DividerDrag::Cancelled {
            self.divider_drag = DividerDrag::Active { position: self.navigator_position };
        }
    }

    /// Cancel movement while retaining capture so later drag events cannot become a selection.
    pub fn cancel_divider_drag(&mut self) {
        if matches!(self.divider_drag, DividerDrag::Active { .. }) {
            self.divider_drag = DividerDrag::Cancelled;
        }
    }

    /// Release divider capture on mouse-up.
    pub fn finish_divider_drag(&mut self) {
        self.divider_drag = DividerDrag::Idle;
    }

    /// Whether a divider gesture still owns drag and mouse-up events.
    #[must_use]
    pub fn divider_drag_active(&self) -> bool {
        matches!(self.divider_drag, DividerDrag::Active { .. })
    }

    /// Whether captured drag events are being consumed without resizing.
    #[must_use]
    pub fn divider_drag_cancelled(&self) -> bool {
        self.divider_drag == DividerDrag::Cancelled
    }

    /// Whether the current gesture still owns drag and mouse-up events in either state.
    #[must_use]
    pub fn divider_drag_captured(&self) -> bool {
        self.divider_drag_active() || self.divider_drag_cancelled()
    }

    /// Set the active share from the captured split axis, cancelling if the position changed.
    pub fn drag_divider(&mut self, axis_len: u16, offset: u16) {
        let DividerDrag::Active { position } = self.divider_drag else {
            return;
        };
        if position != self.navigator_position {
            self.divider_drag = DividerDrag::Cancelled;
            return;
        }
        if axis_len == 0 {
            return;
        }
        let offset = offset.min(axis_len);
        let navigator_len = match self.navigator_position {
            crate::config::NavigatorPosition::Left | crate::config::NavigatorPosition::Top => {
                offset
            }
            crate::config::NavigatorPosition::Right | crate::config::NavigatorPosition::Bottom => {
                axis_len.saturating_sub(offset)
            }
        };
        let pct = (u32::from(navigator_len) * 100 / u32::from(axis_len)) as u16;
        self.set_navigator_share(pct);
    }

    /// Set the search results share from a divider drag.
    pub fn drag_search_divider(&mut self, axis_len: u16, offset: u16) {
        if !self.divider_drag_active() || axis_len == 0 {
            return;
        }
        let offset = offset.min(axis_len);
        let pct = (u32::from(offset) * 100 / u32::from(axis_len)) as u16;
        self.search_pct = pct.clamp(MIN_SEARCH_PCT, MAX_SEARCH_PCT);
    }

    /// Clamp and store one share through the active axis's single bounds/ownership contract.
    fn set_navigator_share(&mut self, share: u16) {
        let max = if self.navigator_position.stacked() { MAX_STACK_PCT } else { MAX_SIDE_PCT };
        let clamped = share.clamp(MIN_NAVIGATOR_PCT, max);
        if self.navigator_position.stacked() {
            self.navigator_stack_pct = clamped;
        } else {
            self.navigator_side_pct = clamped;
        }
    }

    // --- Scroll model: keys move the cursor and reveal it, the wheel moves only the viewport.

    /// Nudge the file list so `file_cursor` is on screen.
    pub fn reveal_file_cursor(&mut self, viewport: usize) {
        if self.file_rows.is_empty() {
            self.file_scroll = 0;
            return;
        }
        let cursor = self.file_cursor.min(self.file_rows.len() - 1);
        let heights = vec![1usize; self.file_rows.len()];
        self.file_scroll = keep_in_view(cursor, self.file_scroll, &heights, viewport);
    }

    /// Clamp `file_scroll` within range (no blank tail). Called every frame.
    pub fn bound_file_scroll(&mut self, viewport: usize) {
        self.file_scroll = bound(self.file_scroll, self.file_rows.len(), viewport);
    }

    /// Nudge the diff to show the cursor, or while composing the line the box opens under.
    pub fn reveal_diff_cursor(&mut self, heights: &[usize], viewport: usize) {
        if self.visible.is_empty() {
            self.diff_scroll = 0;
            return;
        }
        let target = if self.composing() { self.compose_row() } else { self.diff_cursor };
        let target = target.min(self.visible.len() - 1);
        self.diff_scroll = keep_in_view(target, self.diff_scroll, heights, viewport);
    }

    /// Center the cursor's row by display heights when it sits off screen.
    pub fn center_diff_cursor(&mut self, heights: &[usize], viewport: usize) {
        if heights.is_empty() || viewport == 0 {
            return;
        }
        let target = self.diff_cursor.min(heights.len() - 1);
        let in_view = self.diff_scroll <= target
            && heights[self.diff_scroll..=target].iter().sum::<usize>() <= viewport;
        if in_view {
            return;
        }
        // Rows above the target fill about half of what the target leaves of the pane.
        let room = viewport.saturating_sub(heights[target]) / 2;
        let mut top = target;
        let mut above = 0;
        while top > 0 && above + heights[top - 1] <= room {
            top -= 1;
            above += heights[top];
        }
        self.diff_scroll = top;
    }

    /// Settle the scroll: a line jump centers (eating a same-event nudge), else nudge, then bound.
    pub fn settle_diff_scroll(&mut self, heights: &[usize], viewport: usize) {
        let nudge = std::mem::take(&mut self.reveal_diff);
        if std::mem::take(&mut self.reveal_center) {
            self.center_diff_cursor(heights, viewport);
        } else if nudge || self.composing() {
            self.reveal_diff_cursor(heights, viewport);
        }
        self.bound_diff_scroll(heights, viewport);
    }

    /// Clamp `diff_scroll` by display heights, so tall wrapped rows stay reachable.
    pub fn bound_diff_scroll(&mut self, heights: &[usize], viewport: usize) {
        if heights.is_empty() {
            self.diff_scroll = 0;
            return;
        }
        let max_top = keep_in_view(heights.len() - 1, self.diff_scroll, heights, viewport);
        self.diff_scroll = self.diff_scroll.min(max_top);
    }

    /// The chip click's next scope, skipping `commits` without a live pick.
    pub fn next_chip_scope(&self) -> Scope {
        let next = self.scope.cycle();
        if next == Scope::Commits && (self.commit_pick.is_none() || self.pick_gone()) {
            return next.cycle();
        }
        next
    }

    /// Switch the scope and reload; inert while composing.
    pub fn set_scope(&mut self, scope: Scope) -> Result<()> {
        self.ensure_config_ready()?;
        // `commits` without a live pick opens the picker instead of switching.
        if scope == Scope::Commits
            && (self.commit_pick.is_none() || self.pick_gone())
            && !self.composing()
        {
            self.open_commit_picker();
            return Ok(());
        }
        if self.scope != scope && !self.composing() {
            let mut input = self.world_input();
            input.scope = scope;
            let build = self.build_rebase(&input)?;
            self.scope = scope;
            self.adopt_rebase(build);
            // An explicit switch reveals the cursor (a refresh does not).
            self.reveal_files = true;
        }
        Ok(())
    }

    /// Prepare a scope/base/pick rebuild without changing visible selection state.
    fn build_rebase(&self, input: &crate::world::WorldInput) -> Result<RebaseBuild> {
        if self.tab == Tab::Changes {
            crate::world::build(input).map(RebaseBuild::World)
        } else {
            crate::world::build_changed(input).map(RebaseBuild::Changes)
        }
    }

    /// Apply a complete prospective rebuild after its selection state becomes current.
    fn adopt_rebase(&mut self, build: RebaseBuild) {
        self.cache = DiffCache::new();
        match build {
            RebaseBuild::World(snapshot) => {
                self.file_cursor = 0;
                self.expanded_folds.clear();
                self.reset_diff_view();
                self.reconcile_world(snapshot);
            }
            RebaseBuild::Changes(build) => {
                self.stash.file_cursor = 0;
                self.stash.expanded_folds.clear();
                self.stash.diff_cursor = 0;
                self.stash.diff_scroll = 0;
                self.stash.h_scroll = 0;
                self.stash.select_anchor = None;
                // `All files` keeps its tree; only the changed set and annotations switch.
                self.adopt_changeset(
                    build.review_context,
                    build.changeset,
                    build.branch_base,
                    build.pick_status,
                );
                for entry in &mut self.entries {
                    entry.annotation = self.changeset.files.get(&entry.path).cloned();
                }
                self.rebuild_file_rows();
                self.request_world_refresh(false);
            }
        }
    }

    /// Queue a PR refresh; the stronger pending kind wins.
    pub fn request_pr_refresh(&mut self, kind: RefreshKind) {
        self.pr_pending = self.pr_pending.max(Some(kind));
    }

    /// Switch to `tab`, restoring its place as left; a refresh lands behind (Continuity).
    pub fn set_tab(&mut self, tab: Tab) -> Result<()> {
        self.ensure_config_ready()?;
        if self.tab == tab || self.composing() {
            return Ok(());
        }
        // The line field was typed for the tab it opened over; the PR tab draws no band at all.
        if self.line_open() || (tab == Tab::Pr && self.mode == Mode::Find) {
            self.close_find();
        }
        self.tab = tab;
        // `PR` leaves the file tabs frozen and refetches behind its last snapshot.
        if tab == Tab::Pr {
            self.request_pr_refresh(RefreshKind::Ambient);
            return Ok(());
        }
        // Bring the tab's state into the live fields if the stash holds it.
        if self.active_file_tab != tab {
            self.swap_active_with_stash();
            self.active_file_tab = tab;
            // Follow a markdown choice flipped while away.
            if self.rendered.on_screen() != self.wants_rendered()
                && !self.rendered.renders_nothing()
            {
                self.rebuild_visible();
                self.settle_read();
            }
        }
        // A return paints its stash and refreshes behind; a first visit loads before the frame.
        if self.tab_visited {
            self.request_world_refresh(true);
        } else {
            self.reload()?;
        }
        self.settle_tab_entry();
        // A find band opened over the other tab's file does not search this one's.
        if self.mode == Mode::Find && !self.find_available() {
            self.close_find();
        }
        self.reveal_files = true; // pull the restored cursor back into view
        Ok(())
    }

    /// Focus the tree when the read pane is empty, so the keys aren't trapped.
    pub(crate) fn settle_tab_entry(&mut self) {
        if self.navigator_hidden_here() {
            // `PR` may have focused its navigator; hidden means read pane.
            self.focus = Focus::Diff;
            return;
        }
        if self.visible.is_empty() {
            self.focus = Focus::Files;
        }
    }

    // ---- PR tab -------------------------------------

    /// Blank a settled highlight on a `PR` surface whose paint is being replaced.
    fn blank_pr_settled(&mut self) {
        if let Some((d, _)) = &self.settled_sel {
            use crate::selection::Surface;
            let on_pr = matches!(d.surface, Surface::PrNav)
                || (matches!(d.surface, Surface::Painted) && self.tab == Tab::Pr);
            if on_pr {
                self.settled_sel = None;
            }
        }
    }

    /// Clear a snapshot whose fetch input no longer matches.
    pub fn clear_pr(&mut self) {
        self.blank_pr_settled();
        self.pr = forge::PrView::Pending;
        self.pr_notice = None;
        self.pr_refreshing = false;
        self.pr_cursor = 0;
        self.pr_read_scroll = 0;
        self.pr_nav_scroll.set(0);
        self.reveal_pr_nav.set(true);
        self.pr_expanded_details.clear();
    }

    /// Apply a fetched snapshot; a transient error keeps the last good one.
    pub fn apply_pr(&mut self, view: forge::PrView) {
        self.pr_refreshing = false;
        let retry = view.retry_remedy(self.keymap().hint(crate::keymap::Action::Refresh));
        let has_snapshot =
            matches!(self.pr, forge::PrView::Pr(_) | forge::PrView::NoPr | forge::PrView::Detached);
        if has_snapshot && let Some(message) = retry {
            self.pr_notice = Some(message);
            return;
        }
        // A hold, or a detach under a painted snapshot, keeps it.
        if matches!(view, forge::PrView::Held)
            || (matches!(view, forge::PrView::Detached) && matches!(self.pr, forge::PrView::Pr(_)))
        {
            self.pr_notice = None;
            return;
        }
        self.pr_notice = None;
        self.blank_pr_settled();
        // Follow the selected row by identity; only a vanished one resets the read pane.
        let on_description = self.pr_on_description();
        let old_number = self.pr_snapshot().map(|s| s.number);
        let selected = self
            .pr_selected_comment()
            .map(|c| (c.author.clone(), c.created_at.clone(), c.anchor.clone()));
        self.pr = view;
        let offset = self.pr_description_offset();
        let restored = if on_description {
            self.pr_has_description()
                .then_some(0)
                .filter(|_| self.pr_snapshot().map(|s| s.number) == old_number)
        } else {
            selected.as_ref().and_then(|(author, created, anchor)| {
                let i = self.pr_snapshot()?.comments.iter().position(|c| {
                    c.author == *author && c.created_at == *created && c.anchor == *anchor
                })?;
                Some(i + offset)
            })
        };
        if let Some(i) = restored {
            self.pr_cursor = i;
        } else {
            // The selection vanished: clamp, and reset the read pane.
            let clamped = self.pr_row_count().saturating_sub(1);
            if self.pr_cursor > clamped || on_description || selected.is_some() {
                self.pr_read_scroll = 0;
            }
            self.pr_cursor = self.pr_cursor.min(clamped);
            self.pr_expanded_details.clear();
        }
    }

    /// Persistent remedy for a failed same-input refresh.
    pub fn pr_notice(&self) -> Option<&str> {
        self.pr_notice.as_deref()
    }

    pub fn set_pr_refreshing(&mut self, refreshing: bool) {
        if refreshing && matches!(self.pr, forge::PrView::Pending) {
            self.pr = forge::PrView::Loading;
            self.pr_refreshing = false;
        } else {
            self.pr_refreshing = refreshing;
        }
    }

    pub fn pr_refreshing(&self) -> bool {
        self.pr_refreshing
    }

    /// The resolved snapshot, or `None` in a loading/degraded view.
    #[must_use]
    pub fn pr_snapshot(&self) -> Option<&forge::PrSnapshot> {
        match &self.pr {
            forge::PrView::Pr(s) => Some(s),
            _ => None,
        }
    }

    /// Whether the snapshot has a description, and so a pinned row for it.
    #[must_use]
    pub fn pr_has_description(&self) -> bool {
        self.pr_snapshot().is_some_and(|s| !s.body.trim().is_empty())
    }

    /// Whether the navigator cursor sits on the pinned `description` row.
    #[must_use]
    pub fn pr_on_description(&self) -> bool {
        self.pr_has_description() && self.pr_cursor == 0
    }

    /// The cursor rows before the comments: the shift between their indices.
    #[must_use]
    pub fn pr_description_offset(&self) -> usize {
        usize::from(self.pr_has_description())
    }

    /// The navigator's cursor stops: description and comments, never checks.
    #[must_use]
    pub fn pr_row_count(&self) -> usize {
        self.pr_snapshot().map_or(0, |s| s.comments.len() + self.pr_description_offset())
    }

    /// The comment under the navigator cursor.
    #[must_use]
    pub fn pr_selected_comment(&self) -> Option<&forge::Comment> {
        if self.pr_on_description() {
            return None;
        }
        let offset = self.pr_description_offset();
        self.pr_snapshot()?.comments.get(self.pr_cursor - offset)
    }

    /// Move the navigator cursor by `delta`, resetting the read pane to the top.
    pub fn pr_move(&mut self, delta: isize) {
        let n = self.pr_row_count();
        if n == 0 {
            return;
        }
        self.pr_select(step(self.pr_cursor, delta, n));
    }

    /// Select navigator row `i`, resetting the read pane to the top.
    pub(crate) fn pr_select(&mut self, i: usize) {
        self.pr_cursor = i;
        self.pr_read_scroll = 0;
        self.reveal_pr_nav.set(true);
        self.pr_expanded_details.clear();
    }

    pub(crate) fn pr_scroll_nav(&mut self, delta: isize) {
        self.reveal_pr_nav.set(false);
        self.pr_nav_scroll.set(clamp_scroll(
            self.pr_nav_scroll.get(),
            delta,
            self.pr_nav_max_scroll.get(),
        ));
    }

    /// Scroll the read pane by `delta`, clamping first so a stale scroll never eats input.
    pub(crate) fn pr_scroll_read(&mut self, delta: isize) {
        self.pr_read_scroll =
            clamp_scroll(self.pr_read_scroll, delta, self.pr_read_max_scroll.get());
    }

    /// Open the pull request through the same http(s) gate a clicked link passes.
    pub fn pr_open(&mut self) {
        let Some(url) = self.pr_snapshot().map(|s| s.url.clone()) else {
            return;
        };
        let opener = self.plugin_config().and_then(crate::config::PluginConfig::url_opener);
        match crate::browser::open(&url, opener) {
            Ok(()) => self.status = format!("opened {} in browser", self.pr_forge.abbr()),
            Err(e) => self.status = e.to_string(),
        }
    }

    /// Swap the live per-tab fields with the stash; a field left out bleeds between tabs.
    fn swap_active_with_stash(&mut self) {
        std::mem::swap(&mut self.entries, &mut self.stash.entries);
        std::mem::swap(&mut self.file_rows, &mut self.stash.file_rows);
        std::mem::swap(&mut self.file_cursor, &mut self.stash.file_cursor);
        std::mem::swap(&mut self.file_scroll, &mut self.stash.file_scroll);
        std::mem::swap(&mut self.toggled_dirs, &mut self.stash.toggled_dirs);
        std::mem::swap(&mut self.diff, &mut self.stash.diff);
        std::mem::swap(&mut self.visible, &mut self.stash.visible);
        std::mem::swap(&mut self.expanded_folds, &mut self.stash.expanded_folds);
        std::mem::swap(&mut self.diff_path, &mut self.stash.diff_path);
        std::mem::swap(&mut self.diff_cursor, &mut self.stash.diff_cursor);
        std::mem::swap(&mut self.diff_scroll, &mut self.stash.diff_scroll);
        std::mem::swap(&mut self.h_scroll, &mut self.stash.h_scroll);
        std::mem::swap(&mut self.select_anchor, &mut self.stash.select_anchor);
        std::mem::swap(&mut self.rendered, &mut self.stash.rendered);
        std::mem::swap(&mut self.comment_target, &mut self.stash.comment_target);
        std::mem::swap(&mut self.tab_visited, &mut self.stash.visited);
    }

    /// Flip focus; a hidden navigator is shown and focused instead.
    pub fn toggle_focus(&mut self) {
        if self.navigator_hidden_here() {
            self.navigator_hidden = false;
            self.focus = Focus::Files;
            self.reveal_files = true;
            return;
        }
        self.focus = match self.focus {
            Focus::Files => Focus::Diff,
            Focus::Diff => Focus::Files,
        };
    }

    /// Move the focused pane's cursor `delta` rows; a file row opens, a directory keeps the diff.
    pub fn move_cursor(&mut self, delta: isize) -> Result<()> {
        self.ensure_config_ready()?;
        match self.focus {
            Focus::Files => {
                if !self.file_rows.is_empty() {
                    self.file_cursor = step(self.file_cursor, delta, self.file_rows.len());
                    self.open_cursor_file();
                    // Reveal even unmoved, to pull the cursor back after a wheel.
                    self.reveal_files = true;
                }
            }
            Focus::Diff => {
                if !self.visible.is_empty() {
                    let mut target = step(self.diff_cursor, delta, self.visible.len());
                    if let Some(a) = self.select_anchor {
                        target = self.fold_clamped(a, target);
                    }
                    self.diff_cursor = target;
                    self.reveal_diff = true;
                }
            }
        }
        Ok(())
    }

    /// Open the file under the cursor if it isn't the one shown.
    fn open_cursor_file(&mut self) {
        if let Some(i) = self.file_under_cursor_index()
            && Some(self.entries[i].path.as_str()) != self.diff_path.as_deref()
        {
            self.reset_diff_view();
            self.load_read();
        }
    }

    /// `next-file`: open the next file, from either pane.
    pub fn next_file(&mut self) {
        self.step_file(true);
    }

    /// `prev-file`: open the previous file; see [`Self::next_file`].
    pub fn prev_file(&mut self) {
        self.step_file(false);
    }

    /// Open the next file row, from the cursor in the list or from the open file in the diff.
    fn step_file(&mut self, forward: bool) {
        if !self.can_traverse() {
            return;
        }
        let from = if self.focus == Focus::Files { self.file_cursor } else { self.open_file_row() };
        let Some(row) = self.file_row_from(from, forward) else { return };
        self.file_cursor = row;
        self.open_cursor_file();
        self.reveal_files = true;
    }

    /// `next-hunk`: jump to the nearest hunk below the cursor.
    pub fn next_hunk(&mut self) {
        self.step_hunk(true);
    }

    /// `prev-hunk`: jump to the nearest hunk above the cursor; see [`Self::next_hunk`].
    pub fn prev_hunk(&mut self) {
        self.step_hunk(false);
    }

    /// Step to the next hunk; past the last, one press arms a file crossing and the next takes it.
    fn step_hunk(&mut self, forward: bool) {
        // Any step drops the arm; only a same-way repeat takes it.
        let armed = self.armed_cross.take().filter(|a| a.forward == forward);
        if !self.can_traverse() || self.tab != Tab::Changes {
            return;
        }
        if let Some(row) = hunk_row(&self.visible, Some(self.diff_cursor), forward) {
            self.diff_cursor = row;
            self.reveal_diff = true;
            return;
        }
        let Some(armed) = armed else {
            // Arm the crossing, if there is a file to cross to.
            if let Some(row) = self.cross_target(forward)
                && let Some(path) = self.path_of_row(row)
            {
                self.armed_cross = Some(ArmedCross { forward, path });
            }
            return;
        };
        // Re-resolve if a refresh dropped the armed file.
        let Some(row) = self.file_row_of_path(&armed.path).or_else(|| self.cross_target(forward))
        else {
            return;
        };
        self.file_cursor = row;
        self.open_cursor_file();
        // Land on a hunk of the rows now on screen.
        self.diff_cursor = hunk_row(&self.visible, None, forward).unwrap_or(0);
        self.reveal_files = true;
        self.reveal_diff = true;
    }

    /// The nearest file row that way with a hunk, so a crossing always lands on a change.
    fn cross_target(&mut self, forward: bool) -> Option<usize> {
        // From the open file: a cursor parked above it would find it again.
        let mut row = self.open_file_row();
        while let Some(next) = self.file_row_from(row, forward) {
            row = next;
            let i = self.file_rows[row].file_index().expect("file_row_from yields file rows");
            let entry = self.entries[i].clone();
            // Skip files numstat counted as lineless, without reading them.
            if entry.annotation.as_ref().is_some_and(|a| a.additions + a.deletions == 0) {
                continue;
            }
            // A notice holds no hunk.
            let Ok((old, new)) = self.content_sides(&entry.path) else { continue };
            let source = self.rename_source(&entry.path);
            let diff = self.cache.get(entry.path, source, &old, &new, &self.highlighter);
            if hunk_row(&diff.rows, None, forward).is_some() {
                return Some(row);
            }
        }
        None
    }

    /// The path of the file at visible row `row`; `None` on a directory row.
    fn path_of_row(&self, row: usize) -> Option<String> {
        let i = self.file_rows.get(row)?.file_index()?;
        Some(self.entries[i].path.clone())
    }

    /// The direction of the crossing the footer is offering, if a hunk step armed one.
    #[must_use]
    pub fn armed_cross(&self) -> Option<bool> {
        self.armed_cross.as_ref().map(|a| a.forward)
    }

    /// Drop an armed crossing. Every input but a repeat of the step that armed it disarms
    pub fn disarm_cross(&mut self) {
        self.armed_cross = None;
    }

    /// Toggle the footer's `?` list, from `Normal` mode only.
    pub fn toggle_keys(&mut self) {
        self.keys_expanded = !self.keys_expanded;
    }

    /// `esc` peels one layer per press: selection, crossing, then footer expansion.
    pub fn escape(&mut self) {
        if self.tab != Tab::Pr {
            if self.select_anchor.is_some() {
                self.clear_selection();
                return;
            }
            if self.armed_cross.is_some() {
                self.armed_cross = None;
                return;
            }
        }
        self.keys_expanded = false;
    }

    /// Whether traversal keys act; a live selection holds the cursor.
    fn can_traverse(&self) -> bool {
        self.plugin_config().is_some() && self.select_anchor.is_none()
    }

    /// The open file's row, else the cursor when that row is hidden.
    fn open_file_row(&self) -> usize {
        self.diff_path
            .as_deref()
            .and_then(|path| self.file_row_of_path(path))
            .unwrap_or(self.file_cursor)
    }

    /// The nearest file row past `row` that way; `None` at the end.
    fn file_row_from(&self, row: usize, forward: bool) -> Option<usize> {
        let is_file = |i: &usize| self.file_rows[*i].file_index().is_some();
        if forward {
            (row + 1..self.file_rows.len()).find(is_file)
        } else {
            (0..row).rev().find(is_file)
        }
    }

    /// Click file row `index`: a file opens, a directory toggles.
    pub fn select_file(&mut self, index: usize) -> Result<()> {
        self.ensure_config_ready()?;
        if index >= self.file_rows.len() {
            return Ok(());
        }
        self.focus = Focus::Files;
        self.file_cursor = index;
        self.reveal_files = true;
        match self.file_rows[index].kind {
            RowKind::File { .. } => self.open_cursor_file(),
            RowKind::Dir { .. } => self.toggle_dir(),
        }
        Ok(())
    }

    /// Toggle the directory under the cursor and rebuild the tree.
    fn toggle_dir(&mut self) {
        let Some(path) = self.dir_under_cursor() else { return };
        // Flip its membership in the toggled set (toggled = flipped from the tab's default).
        if !self.toggled_dirs.remove(&path) {
            self.toggled_dirs.insert(path);
        }
        self.apply_dir_change();
    }

    /// Whether directory `path` is currently expanded under the active tab's resting state.
    fn dir_expanded(&self, path: &str) -> bool {
        self.default_expanded() ^ self.toggled_dirs.contains(path)
    }

    /// Force directory `path` to `want` (expanded or collapsed); returns whether it changed.
    fn set_dir_expanded(&mut self, path: &str, want: bool) -> bool {
        if self.dir_expanded(path) == want {
            return false;
        }
        if !self.toggled_dirs.remove(path) {
            self.toggled_dirs.insert(path.to_string());
        }
        true
    }

    /// Whether the focused file cursor is on a directory, for `←`/`→`.
    pub fn on_folder(&self) -> bool {
        self.focus == Focus::Files
            && self.file_rows.get(self.file_cursor).is_some_and(|r| r.dir_path().is_some())
    }

    /// Whether the focused diff cursor is on a fold, which `→` expands.
    pub fn on_fold(&self) -> bool {
        self.focus == Focus::Diff
            && self.visible.get(self.diff_cursor).and_then(Row::fold_anchor).is_some()
    }

    /// Expand the directory under the cursor (`→`); a no-op if it is a file or already open.
    pub fn expand_dir(&mut self) {
        if self.plugin_config().is_none() {
            return;
        }
        if let Some(path) = self.dir_under_cursor()
            && self.set_dir_expanded(&path, true)
        {
            self.apply_dir_change();
        }
    }

    /// Collapse the directory under the cursor (`←`); a no-op if it is a file or already shut.
    pub fn collapse_dir(&mut self) {
        if self.plugin_config().is_none() {
            return;
        }
        if let Some(path) = self.dir_under_cursor()
            && self.set_dir_expanded(&path, false)
        {
            self.apply_dir_change();
        }
    }

    /// The path of the directory row under the cursor, if any.
    fn dir_under_cursor(&self) -> Option<String> {
        self.file_rows.get(self.file_cursor).and_then(|r| r.dir_path()).map(str::to_string)
    }

    /// Rebuild the tree after a directory's expansion changed, keeping the cursor in range.
    fn apply_dir_change(&mut self) {
        // An expanded ignored directory loads its children first.
        if self.tab == Tab::AllFiles
            && let Ok(entries) =
                crate::world::all_files_entries(&self.world_input(), &self.changeset.files)
        {
            self.entries = entries;
        }
        self.rebuild_file_rows();
        self.file_cursor = self.file_cursor.min(self.file_rows.len().saturating_sub(1));
        self.reveal_files = true; // the row may have moved off-screen; pull it back
    }

    /// Wheel the diff's viewport, never the cursor a comment attaches to.
    pub fn wheel_diff(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        self.diff_scroll = offset_by(self.diff_scroll, delta);
    }

    /// Wheel the file list's viewport, never the selection.
    pub fn wheel_files(&mut self, delta: isize) {
        if self.file_rows.is_empty() {
            return;
        }
        self.file_scroll = offset_by(self.file_scroll, delta);
    }

    /// Extend a mouse drag-selection to the diff line at `index`, anchoring on first drag.
    pub fn drag_select_to(&mut self, index: usize) {
        if index < self.visible.len() {
            self.focus = Focus::Diff;
            let anchor = *self.select_anchor.get_or_insert(self.diff_cursor);
            self.diff_cursor = self.fold_clamped(anchor, index);
            self.reveal_diff = true;
        }
    }

    /// Stop `target` shy of any fold, so a selection never brackets hidden lines.
    fn fold_clamped(&self, anchor: usize, target: usize) -> usize {
        if target > anchor {
            (anchor + 1..=target).find(|&i| !self.visible[i].is_content()).map_or(target, |i| i - 1)
        } else {
            (target..anchor)
                .rev()
                .find(|&i| !self.visible[i].is_content())
                .map_or(target, |i| i + 1)
        }
    }

    /// Whether a mouse text or gutter gesture is live.
    #[must_use]
    pub fn gesture_active(&self) -> bool {
        !matches!(self.gesture, crate::selection::Gesture::None)
    }

    /// The live text drag, if any.
    #[must_use]
    pub fn text_drag(&self) -> Option<crate::selection::TextDrag> {
        match self.gesture {
            crate::selection::Gesture::Text { drag, .. } => Some(drag),
            _ => None,
        }
    }

    /// Whether a gutter comment gesture is live.
    #[must_use]
    pub fn gutter_drag(&self) -> bool {
        matches!(self.gesture, crate::selection::Gesture::Gutter)
    }

    /// The settled selection's span, if one is painted.
    #[must_use]
    pub fn settled_selection(&self) -> Option<crate::selection::TextDrag> {
        self.settled_sel.as_ref().map(|(d, _)| *d)
    }

    /// Keep a copied span highlighted, with its text for revalidation.
    pub(crate) fn settle_selection(&mut self, drag: crate::selection::TextDrag, text: String) {
        self.settled_sel = Some((drag, text));
    }

    /// Clear the settled selection: the user did something else.
    pub(crate) fn clear_settled_selection(&mut self) {
        self.settled_sel = None;
    }

    /// Whether world events leave the open view alone: under a modal or a view-anchored gesture.
    fn view_frozen(&self) -> bool {
        self.mode.is_modal() || self.view_anchored_gesture()
    }

    /// Whether the live gesture anchors to the open view's rows, holding its reload.
    fn view_anchored_gesture(&self) -> bool {
        use crate::selection::Surface;
        matches!(self.gesture, crate::selection::Gesture::Gutter)
            || self.text_drag().is_some_and(|d| match d.surface {
                Surface::Read | Surface::Card { .. } => true,
                // The `PR` read pane holds through the gated PR drains instead.
                Surface::Painted | Surface::Files | Surface::PrNav => false,
            })
    }

    /// Whether a navigator gesture holds the world drain.
    #[must_use]
    pub fn gates_world_drain(&self) -> bool {
        use crate::selection::Surface;
        // Exhaustive, so a new surface must choose its freeze.
        self.text_drag().is_some_and(|d| match d.surface {
            Surface::Files => true,
            Surface::Read | Surface::Card { .. } | Surface::Painted | Surface::PrNav => false,
        })
    }

    /// Whether a `PR` gesture holds the PR drains.
    #[must_use]
    pub fn gates_pr_drain(&self) -> bool {
        use crate::selection::Surface;
        self.text_drag().is_some_and(|d| match d.surface {
            Surface::PrNav => true,
            Surface::Painted => self.tab == Tab::Pr,
            Surface::Read | Surface::Card { .. } | Surface::Files => false,
        })
    }

    /// Forget the pointer before another program takes the terminal and its mouse.
    pub(crate) fn forget_pointer(&mut self) {
        self.cancel_gesture();
        self.finish_divider_drag();
        self.hover = None;
    }

    /// End the live gesture without a copy, resetting the click chain and lifting the freeze.
    pub(crate) fn cancel_gesture(&mut self) {
        if !self.gesture_active() {
            return; // nothing live: the multi-click chain survives unrelated input
        }
        if matches!(self.gesture, crate::selection::Gesture::Gutter) {
            self.select_anchor = None;
        }
        self.gesture = crate::selection::Gesture::None;
        self.last_click = None;
        self.catch_up_held_view();
    }

    /// Count a mouse-down into the click chain, capped at 3; a new `target` row breaks it.
    pub fn note_click(
        &mut self,
        col: u16,
        row: u16,
        target: usize,
        surface: crate::selection::Surface,
    ) -> u8 {
        const WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
        let now = std::time::Instant::now();
        let count = match self.last_click {
            Some(last)
                if (last.col, last.row, last.target, last.surface)
                    == (col, row, target, surface)
                    && now.duration_since(last.at) < WINDOW =>
            {
                (last.count + 1).min(3)
            }
            _ => 1,
        };
        self.last_click = Some(LastClick { at: now, col, row, target, count, surface });
        count
    }

    /// Start a gutter comment gesture on `row`.
    pub fn start_gutter_drag(&mut self, row: usize) {
        if self.composing() || row >= self.visible.len() {
            return; // the gutter is inert while the comment editor is open
        }
        self.focus = Focus::Diff;
        self.diff_cursor = row;
        self.select_anchor = None;
        self.gesture = crate::selection::Gesture::Gutter;
        self.reveal_diff = true;
    }

    /// Finish a gutter gesture by opening the composer.
    pub fn finish_gutter_drag(&mut self) {
        if !matches!(self.gesture, crate::selection::Gesture::Gutter) {
            return;
        }
        self.gesture = crate::selection::Gesture::None;
        // The composer replaces the find band.
        self.close_find();
        self.start_comment();
    }

    /// Copy `text` to `target`, reporting the outcome.
    pub fn copy_selection_text(&mut self, target: &dyn crate::export::ExportTarget, text: &str) {
        if text.is_empty() {
            return;
        }
        match target.export(text) {
            Ok(()) => self.status = crate::selection::copied_status(text),
            Err(e) => {
                crate::logln!("selection copy failed: {e:#}");
                self.status = target.failure_message(&e, &self.copy_key());
            }
        }
    }

    /// Toggle a range-selection anchor at the current diff line.
    pub fn toggle_select(&mut self) {
        if self.focus == Focus::Diff && !self.visible.is_empty() {
            self.select_anchor = match self.select_anchor {
                Some(_) => None,
                None => Some(self.diff_cursor),
            };
            self.reveal_diff = true;
        }
    }

    /// Drop the range-selection anchor (the `esc` clear in the diff); a no-op when none is set.
    pub fn clear_selection(&mut self) {
        if self.select_anchor.is_some() {
            self.select_anchor = None;
            self.reveal_diff = true;
        }
    }

    /// The inclusive `[lo, hi]` diff-line range currently selected.
    pub fn selection_range(&self) -> (usize, usize) {
        match self.select_anchor {
            Some(a) => (a.min(self.diff_cursor), a.max(self.diff_cursor)),
            None => (self.diff_cursor, self.diff_cursor),
        }
    }

    pub fn start_comment(&mut self) {
        if self.focus == Focus::Diff && self.has_anchorable_selection() {
            self.reveal_diff = true; // scroll the anchored line into view before the box opens
            self.input.clear();
            self.caret = 0;
            self.resume_list = false; // a fresh diff comment returns to the diff, not the list
            self.mode = Mode::Composing { editing: None };
        }
    }

    /// `edit`: the comment under the cursor, else the file it names.
    pub fn start_edit(&mut self) {
        if self.comment_claims_edit() {
            self.edit_comment();
            return;
        }
        self.editor_request = self.edit_target();
    }

    /// Whether a visible comment takes `edit` over the file; never mid-selection on the diff.
    fn comment_claims_edit(&self) -> bool {
        let on_the_diff =
            self.tab.is_file_tab() && self.focus == Focus::Diff && self.select_anchor.is_none();
        let claimed =
            if self.mode == Mode::List { self.list_comment_editable() } else { on_the_diff };
        claimed && self.target_comment().is_some()
    }

    /// Whether the list's comment is editable: only in the view it was made on.
    fn list_comment_editable(&self) -> bool {
        self.mode == Mode::List
            && self.store.get(self.list_cursor).is_some_and(|c| self.rev_is_current(c))
    }

    /// Whether `edit` opens a file, as the footer asks.
    fn edit_opens_a_file(&self) -> bool {
        !self.comment_claims_edit() && self.edit_target().is_some()
    }

    /// The file and line `edit` opens, for the press and the footer alike; never touches disk.
    fn edit_target(&self) -> Option<EditTarget> {
        // `PR` names no file of its own.
        if !self.tab.is_file_tab() {
            return None;
        }
        // Every other mode owns the key: the comments list, the pickers, and the text fields.
        if self.mode != Mode::Normal {
            return None;
        }
        let (path, numbered) = if self.focus == Focus::Files {
            // The selected navigator file at its start. A directory row names no file.
            (self.current_entry()?.path.clone(), false)
        } else {
            // A press must not abandon a live selection.
            if self.select_anchor.is_some() {
                return None;
            }
            // The open file; in `commits` the line numbers aren't the worktree's, so no line.
            let commit_diff = self.scope == Scope::Commits && self.diff.view == View::Diff;
            (self.diff_path.clone()?, !commit_diff)
        };
        let line = if !numbered {
            1
        } else if self.rendered_active() {
            // A block's first line, or a marker's line clamped to the file's end.
            let last = self.rendered.text().unwrap_or_default().lines().count().max(1) as u32;
            match self.rendered.index.unit_at(self.diff_cursor).map(|u| u.unit) {
                Some(Unit::Block(src)) => src,
                Some(Unit::Marker(src, _)) => src.clamp(1, last),
                None => 1,
            }
        } else {
            // The nearest worktree line number at or above the cursor, else the start.
            self.visible
                .get(..=self.diff_cursor)
                .and_then(|above| above.iter().rev().find_map(Row::new_no))
                .unwrap_or(1)
        };
        Some(EditTarget { path, line })
    }

    fn edit_comment(&mut self) {
        // Editing from the comments-list overlay returns there on finish (else to the diff).
        let from_list = self.mode == Mode::List;
        let Some(i) = self.target_comment() else { return };
        let Some(c) = self.store.get(i) else { return };
        let (file, text) = (c.file.clone(), c.text.clone());
        let in_view = self.comment_in_view(c);

        // Open the comment's file by path, so even a collapsed one works.
        if self.diff_path.as_deref() != Some(file.as_str())
            && let Some(e) = self.entries.iter().find(|e| e.path == file).cloned()
        {
            self.reset_diff_view();
            self.open_path_in_tab(e.path);
            if let Some(fi) = self.file_row_of_path(&file) {
                self.file_cursor = fi;
            }
        }
        // Only in the comment's own file and view, onto the row its card sits under.
        if in_view
            && self.diff_path.as_deref() == Some(file.as_str())
            && let Some(&(idx, _)) = self.card_rows().iter().find(|&&(_, ci)| ci == i)
        {
            self.diff_cursor = idx;
            self.select_anchor = None;
        }
        self.focus = Focus::Diff;
        self.reveal_diff = true; // scroll the edited line into view before the box opens
        self.caret = text.chars().count(); // edit opens with the caret at the end
        self.input = text;
        self.resume_list = from_list;
        self.mode = Mode::Composing { editing: Some(i) };
        self.target_comment_card(i);
    }

    // --- text editing: one control set, a char-index caret, edits via `Vec<char>` ---------

    /// The mode's editable text and caret, if any.
    fn active_field(&mut self) -> Option<(&mut String, &mut usize)> {
        match self.mode {
            Mode::Composing { .. } => Some((&mut self.input, &mut self.caret)),
            Mode::Search => self.search.as_mut().map(|s| (&mut s.query, &mut s.caret)),
            Mode::Find => self.find.as_mut().map(|f| (&mut f.query, &mut f.caret)),
            Mode::BasePick => self.base_picker.as_mut().map(|b| (&mut b.query, &mut b.caret)),
            Mode::Normal | Mode::List | Mode::Picker | Mode::CommitPick => None,
        }
    }

    /// Run a char-wise edit on the active field; a changed search query re-queries.
    fn edit_input(&mut self, f: impl FnOnce(&mut Vec<char>, &mut usize)) {
        let searching = self.mode == Mode::Search;
        // The highlighted row's name, read before the filter narrows under it.
        let highlighted = self
            .base_picker
            .as_ref()
            .and_then(|bp| bp.visible().get(bp.cursor).map(|c| c.name().to_string()));
        let Some((text, caret_ref)) = self.active_field() else { return };
        let mut v: Vec<char> = text.chars().collect();
        let mut caret = (*caret_ref).min(v.len());
        f(&mut v, &mut caret);
        *caret_ref = caret.min(v.len());
        let edited: String = v.into_iter().collect();
        let changed = *text != edited;
        *text = edited;
        if searching && changed {
            self.search_dirty = true;
        }
        if changed && self.mode == Mode::BasePick {
            self.refilter_base_picker(highlighted);
        }
    }

    /// Re-seat the highlight after a filter edit, and probe a query no row spells exactly.
    fn refilter_base_picker(&mut self, highlighted: Option<String>) {
        let Some(bp) = self.base_picker.as_mut() else { return };
        bp.probe = if !bp.query.is_empty() && !bp.query_is_listed() {
            BaseProbe::Pending(Instant::now() + BASE_PROBE_DELAY)
        } else {
            BaseProbe::Idle
        };
        let cursor = {
            let vis = bp.visible();
            highlighted.and_then(|h| vis.iter().position(|c| c.name() == h)).unwrap_or(0)
        };
        bp.cursor = cursor;
    }

    /// Remaining wait until the empty-list probe, if one is pending.
    #[must_use]
    pub fn base_probe_wait(&self) -> Option<Duration> {
        match self.base_picker.as_ref()?.probe {
            BaseProbe::Pending(at) => Some(at.saturating_duration_since(Instant::now())),
            _ => None,
        }
    }

    /// Run the empty-list commit probe if its pause has elapsed.
    pub fn tick_base_picker_probe(&mut self) {
        let at = match self.base_picker.as_ref().map(|bp| &bp.probe) {
            Some(BaseProbe::Pending(at)) => *at,
            _ => return,
        };
        if Instant::now() < at {
            return;
        }
        self.run_base_probe();
    }

    /// Check the query as a commit now. Tests call this instead of sleeping.
    pub fn run_base_probe(&mut self) {
        let Some(bp) = &self.base_picker else { return };
        if bp.query.is_empty() || bp.query_is_listed() {
            if let Some(bp) = self.base_picker.as_mut() {
                bp.probe = BaseProbe::Idle;
            }
            return;
        }
        let query = bp.query.clone();
        let hit = git::resolve_spelling(&self.repo, &query).map(|resolved| {
            resolved.map(|c| match c {
                git::ResolvedBase::Branch { name, .. } => BaseChoice::Branch {
                    name,
                    pr_base: false,
                    is_default: false,
                    current: false,
                    tip_secs: 0,
                },
                git::ResolvedBase::Rev { spelling, oid } => {
                    BaseChoice::Rev { name: git::complete_sha_prefix(&spelling, &oid), oid }
                }
            })
        });
        let Some(bp) = self.base_picker.as_mut() else { return };
        match hit {
            Ok(Some(choice)) => bp.probe = BaseProbe::Hit(choice),
            Ok(None) => bp.probe = BaseProbe::Miss,
            Err(e) => {
                bp.probe = BaseProbe::Miss;
                self.status = e.0;
            }
        }
    }

    /// Move the caret by `f` over the active field's chars.
    fn move_caret(&mut self, f: impl FnOnce(&[char], usize) -> usize) {
        if let Some((text, caret)) = self.active_field() {
            let v: Vec<char> = text.chars().collect();
            *caret = f(&v, (*caret).min(v.len()));
        }
    }

    /// Insert `ch` at the caret.
    pub fn input_push(&mut self, ch: char) {
        self.edit_input(|v, caret| {
            v.insert(*caret, ch);
            *caret += 1;
        });
    }

    /// Insert a paste as one unit; single-line fields space or drop its newlines, the line field
    /// takes a number or `$` from it.
    pub fn input_paste(&mut self, text: &str) {
        if self.line_open() {
            self.paste_line(text);
            return;
        }
        let mut norm = text.replace("\r\n", "\n").replace('\r', "\n");
        match self.mode {
            Mode::Search | Mode::Find => norm = norm.replace('\n', " "),
            // No branch name holds a newline.
            Mode::BasePick => norm.retain(|c| c != '\n'),
            _ => {}
        }
        let norm: Vec<char> = norm.chars().collect();
        self.edit_input(|v, caret| {
            let n = norm.len();
            v.splice(*caret..*caret, norm);
            *caret += n;
        });
    }

    /// Delete the character before the caret.
    pub fn input_backspace(&mut self) {
        self.edit_input(|v, caret| {
            if *caret > 0 {
                v.remove(*caret - 1);
                *caret -= 1;
            }
        });
    }

    /// Delete the character at the caret (`Delete`).
    pub fn input_delete_forward(&mut self) {
        self.edit_input(|v, caret| {
            if *caret < v.len() {
                v.remove(*caret);
            }
        });
    }

    /// `Ctrl+W`: delete whitespace, then the word, before the caret.
    pub fn input_delete_word(&mut self) {
        self.edit_input(|v, caret| {
            let start = word_start(v, *caret);
            v.drain(start..*caret);
            *caret = start;
        });
    }

    /// Delete from the start of the logical line to the caret (`Ctrl+U`).
    pub fn input_kill_to_start(&mut self) {
        self.edit_input(|v, caret| {
            let start = line_start(v, *caret);
            v.drain(start..*caret);
            *caret = start;
        });
    }

    /// Delete from the caret to the end of the logical line (`Ctrl+K`).
    pub fn input_kill_to_end(&mut self) {
        self.edit_input(|v, caret| {
            let end = line_end(v, *caret);
            v.drain(*caret..end);
        });
    }

    /// Move the caret one character left / right.
    pub fn caret_left(&mut self) {
        self.move_caret(|_, caret| caret.saturating_sub(1));
    }
    pub fn caret_right(&mut self) {
        self.move_caret(|v, caret| (caret + 1).min(v.len()));
    }

    /// Move the caret to the start / end of the logical line (between newlines).
    pub fn caret_home(&mut self) {
        self.move_caret(line_start);
    }
    pub fn caret_end(&mut self) {
        self.move_caret(line_end);
    }

    /// Move the caret one word left / right.
    pub fn caret_word_left(&mut self) {
        self.move_caret(word_start);
    }
    pub fn caret_word_right(&mut self) {
        self.move_caret(word_end);
    }

    pub fn cancel_comment(&mut self) {
        self.leave_compose();
    }

    /// Leave compose mode, back to the list it came from if comments remain.
    fn leave_compose(&mut self) {
        self.input.clear();
        self.caret = 0;
        let resume = std::mem::take(&mut self.resume_list);
        if resume && !self.store.is_empty() {
            self.list_cursor = self.list_cursor.min(self.store.len() - 1);
            self.mode = Mode::List;
        } else {
            self.mode = Mode::Normal;
        }
    }

    /// Save the draft, new or edited, and leave; blank text cancels.
    pub fn submit_comment(&mut self) {
        let Mode::Composing { editing } = self.mode else { return };
        let text = self.input.trim().to_string();
        if text.is_empty() {
            self.cancel_comment();
            return;
        }
        match editing {
            Some(i) => {
                logln!("comment edit [{i}] :: {text}");
                self.store.edit(i, text);
                self.status = "comment updated".to_string();
            }
            None => {
                if let Some(c) = self.build_comment(text) {
                    logln!("comment add {} :: {}", c.location(), c.text);
                    let i = self.store.add(c);
                    self.target_comment_card(i);
                    self.status = "comment added".to_string();
                }
            }
        }
        self.select_anchor = None;
        self.leave_compose();
        self.refresh_rendered();
    }

    /// Re-derive the rendered rows now after a comment change, not on a later refresh.
    fn refresh_rendered(&mut self) {
        if self.rendered.on_screen() {
            self.rebuild_visible();
            self.settle_read();
        }
    }

    /// Whether the selection holds a row a comment can attach to.
    fn has_anchorable_selection(&self) -> bool {
        if self.rendered.on_screen() {
            return self.selection_anchor().is_some();
        }
        let (lo, hi) = self.selection_range();
        self.visible.get(lo..=hi).is_some_and(|s| s.iter().any(Row::is_content))
    }

    /// The selection's `(side, start, end, snippet)`, rendered or not, by one `anchor()` (G1).
    fn selection_anchor(&self) -> Option<(Side, u32, u32, String)> {
        if self.rendered.on_screen() {
            return anchor(&self.rendered_anchor_rows());
        }
        let (lo, hi) = self.selection_range();
        anchor(self.visible.get(lo..=hi)?)
    }

    /// The source rows a rendered selection stands for: its new-side span, deletions, and owned changes.
    /// A comment on them equals the source comment on the same rows.
    fn rendered_anchor_rows(&self) -> Vec<Row> {
        let (lo, hi) = self.selection_range();
        let units: Vec<&crate::rendered::UnitRows> =
            self.rendered.index.units().iter().filter(|u| u.end > lo && u.start <= hi).collect();
        let picked: Vec<Unit> = units.iter().map(|u| u.unit).collect();
        let blocks: Vec<(u32, u32)> = units
            .iter()
            .filter(|u| matches!(u.unit, Unit::Block(_)))
            .map(|u| (u.src, u.src_end))
            .collect();
        let span = blocks.iter().map(|&(s, _)| s).min().zip(blocks.iter().map(|&(_, e)| e).max());
        let marks = &self.rendered.marks;
        let inside = |n: u32| span.is_some_and(|(first, last)| (first..=last).contains(&n));
        diff_lines(&self.diff.rows)
            .enumerate()
            .filter(|&(i, row)| {
                let owned = || picked.iter().any(|&u| marks.anchors(i, u));
                if let Some(n) = row.new_no() {
                    return inside(n) || (is_change(row) && owned());
                }
                // A deletion: inside the span, or owned by a selected unit.
                let at = marks.bounds(i);
                let within = span.is_some_and(|(first, last)| {
                    at.before.is_some_and(|a| a >= first) && at.after.is_some_and(|b| b <= last)
                });
                within || owned()
            })
            .map(|(_, row)| row.clone())
            .collect()
    }

    /// The row the composer splices under, where the card will sit.
    #[must_use]
    pub fn compose_row(&self) -> usize {
        let (_, hi) = self.selection_range();
        if !self.rendered_active() {
            return hi;
        }
        self.rendered.index.unit_at(hi).map_or(hi, |u| u.end - 1)
    }

    fn build_comment(&self, text: String) -> Option<Comment> {
        // The open diff's file, never the list selection.
        let file = self.diff_path.clone()?;
        let (side, start, end, lines) = self.selection_anchor()?;
        // A File view comment is content-anchored: it goes stale only with its file.
        let diff_anchored = self.diff.view == View::Diff;
        // A content comment reads the worktree whatever the scope.
        let rev = if diff_anchored { self.current_rev() } else { Rev::Worktree };
        Some(Comment { file, side, start, end, lines, text, diff_anchored, rev })
    }

    /// The composer's `path:line`, while composing.
    pub fn pending_location(&self) -> Option<String> {
        match self.mode {
            Mode::Composing { editing: Some(i) } => self.store.get(i).map(Comment::location),
            Mode::Composing { editing: None } => {
                let file = self.diff_path.clone()?;
                let (side, start, end, _) = self.selection_anchor()?;
                // Only `location()` is read here, which ignores `diff_anchored`.
                let c = Comment {
                    file,
                    side,
                    start,
                    end,
                    lines: String::new(),
                    text: String::new(),
                    diff_anchored: true,
                    rev: Rev::Worktree,
                };
                Some(c.location())
            }
            Mode::Normal
            | Mode::List
            | Mode::Picker
            | Mode::BasePick
            | Mode::CommitPick
            | Mode::Search
            | Mode::Find => None,
        }
    }

    /// Whether `c` belongs to this view: its kind matches, and a diff comment's `rev` is current.
    fn comment_in_view(&self, c: &Comment) -> bool {
        if c.diff_anchored != (self.diff.view == View::Diff) {
            return false;
        }
        self.rev_is_current(c)
    }

    /// Whether the scope reads the diff `c` was made on.
    fn rev_is_current(&self, c: &Comment) -> bool {
        match &c.rev {
            Rev::Worktree => {
                !c.diff_anchored || self.scope != Scope::Commits || self.commit_pick.is_none()
            }
            Rev::Commit(p) => self.scope == Scope::Commits && self.commit_pick.as_ref() == Some(p),
        }
    }

    /// Each shown comment with the rows it covers: the one comment→row map.
    fn comment_rows(&self) -> Vec<(usize, Vec<usize>)> {
        let Some(file) = self.diff_path.as_deref() else { return Vec::new() };
        let shown: Vec<(usize, &Comment)> = self
            .store
            .iter()
            .enumerate()
            .filter(|(_, c)| c.file == file && self.comment_in_view(c))
            .collect();
        if !self.rendered_active() {
            let rows = |c: &Comment| -> Vec<usize> {
                (0..self.visible.len()).filter(|&i| line_in(c, &self.visible[i])).collect()
            };
            return shown.into_iter().map(|(ci, c)| (ci, rows(c))).collect();
        }
        // The diff's lines, walked once, and only for an old-side comment.
        let lines: Vec<&Row> = if shown.iter().any(|(_, c)| c.side == Side::Old) {
            diff_lines(&self.diff.rows).collect()
        } else {
            Vec::new()
        };
        let units = self.rendered.index.units();
        shown
            .into_iter()
            .map(|(ci, c)| {
                let mut cover = self.rendered_cover(c, &lines);
                cover.sort_unstable();
                cover.dedup();
                // Each covered unit's rows, in row order.
                (ci, cover.into_iter().flat_map(|k| units[k].start..units[k].end).collect())
            })
            .collect()
    }

    /// The rendered units `c` covers, never empty, so no comment hides (G3).
    /// New side: the units its range overlaps; old side: the owner of its last deleted row.
    fn rendered_cover(&self, c: &Comment, lines: &[&Row]) -> Vec<usize> {
        let index = &self.rendered.index;
        match c.side {
            Side::New => index.new_side_cover(c.start, c.end),
            Side::Old => {
                let last = lines
                    .iter()
                    .rposition(|r| r.old_no().is_some_and(|n| c.start <= n && n <= c.end));
                // A restored line sits in its block; a vanished one under the last unit.
                let owner = match last {
                    Some(i) if lines[i].new_no().is_some() => {
                        index.new_side_land(lines[i].new_no())
                    }
                    Some(i) => self
                        .rendered
                        .marks
                        .owner(i)
                        .and_then(|u| index.position(u))
                        .or_else(|| index.new_side_land(None)),
                    None => index.new_side_land(None),
                };
                owner.into_iter().collect()
            }
        }
    }

    /// The card anchors and every commented row, from one walk.
    #[must_use]
    pub fn comment_marks(&self) -> (Vec<(usize, usize)>, HashSet<usize>) {
        let rows = self.comment_rows();
        (cards_of(&rows), rows.into_iter().flat_map(|(_, r)| r).collect())
    }

    /// The card anchors as `(row, store index)`, store-ordered: the one card map.
    pub fn card_rows(&self) -> Vec<(usize, usize)> {
        cards_of(&self.comment_rows())
    }

    /// The comment to act on: the list's highlight, else the one under the cursor.
    fn target_comment(&self) -> Option<usize> {
        if self.mode == Mode::List {
            return (self.list_cursor < self.store.len()).then_some(self.list_cursor);
        }
        self.comment_under_cursor()
    }

    /// A comment covering the cursor's row, the picked one first.
    fn comment_under_cursor(&self) -> Option<usize> {
        let rows = self.comment_rows();
        let covering =
            rows.iter().filter(|(_, r)| r.contains(&self.diff_cursor)).map(|(ci, _)| *ci);
        self.live_target(&rows).or_else(|| covering.into_iter().next())
    }

    /// Pick comment `index`, as any action landing on or creating one does.
    pub fn target_comment_card(&mut self, index: usize) {
        self.comment_target = self.store.id(index);
    }

    /// The picked comment, while it exists and covers the cursor's row.
    fn live_target(&self, rows: &[(usize, Vec<usize>)]) -> Option<usize> {
        let index = self.store.index_of(self.comment_target?)?;
        let covers = rows.iter().any(|(ci, r)| *ci == index && r.contains(&self.diff_cursor));
        covers.then_some(index)
    }

    /// Clear a pick no longer live, after each input; refreshes never call it.
    pub fn settle_pick(&mut self) {
        if self.comment_target.is_some() && self.live_target(&self.comment_rows()).is_none() {
            self.comment_target = None;
        }
    }

    pub fn delete_comment(&mut self) {
        if let Some(i) = self.target_comment() {
            logln!("comment delete [{i}]");
            self.store.take(i);
            self.clamp_list_cursor();
            self.status = "comment deleted".to_string();
            // Don't strand the user in an empty "Comments (0)" overlay, matching `export`.
            if self.store.is_empty() {
                self.close_list();
            }
            self.refresh_rendered();
        }
    }

    /// Step to the next or previous comment's first row and pick it, one comment at a time.
    pub fn jump_comment(&mut self, dir: isize) {
        let rendered = self.rendered_active();
        let rows = self.comment_rows();
        let mut stops: Vec<(usize, usize)> = rows
            .iter()
            .filter_map(|(ci, rows)| {
                let first = *rows.first()?;
                let lead = rendered
                    .then(|| self.rendered.index.unit_at(first).map(|u| u.lead))
                    .flatten()
                    .filter(|lead| rows.contains(lead))
                    .unwrap_or(first);
                Some((lead, *ci))
            })
            .collect();
        if stops.is_empty() {
            return;
        }
        // Stable, so comments starting on one row keep their store order.
        stops.sort_by_key(|&(row, _)| row);
        let n = stops.len();
        let cur = self.diff_cursor;
        let at = self.live_target(&rows).and_then(|t| stops.iter().position(|&(_, ci)| ci == t));
        let k = match at {
            Some(k) if dir >= 0 => (k + 1) % n,
            Some(k) => (k + n - 1) % n,
            None if dir >= 0 => stops.iter().position(|&(r, _)| r > cur).unwrap_or(0),
            None => stops.iter().rposition(|&(r, _)| r < cur).unwrap_or(n - 1),
        };
        let (row, ci) = stops[k];
        self.focus = Focus::Diff;
        self.select_anchor = None; // a comment jump is navigation, not a selection extend
        self.diff_cursor = row;
        self.reveal_diff = true;
        self.target_comment_card(ci);
    }

    // --- Search overlay ------------------------------------------------

    /// The active scope's annotation for `path`.
    pub(crate) fn changed_annotation(&self, path: &str) -> Option<&ChangedFile> {
        self.changeset.files.get(path)
    }

    /// The three-state review status for `path` in the active semantic comparison.
    pub fn file_review_state(&self, path: &str) -> FileReviewState {
        let Some(context) = self.review_context.as_ref() else {
            return FileReviewState::Unreviewed;
        };
        let Some(identity) = self.changeset.files.get(path).map(|a| &a.identity) else {
            return FileReviewState::Unreviewed;
        };
        match self.reviewed.get(context).and_then(|reviewed| reviewed.get(path)) {
            None => FileReviewState::Unreviewed,
            Some(mark) if mark.changed || !mark.identity.same_file_comparison(identity) => {
                FileReviewState::ReviewedButChanged
            }
            Some(_) => FileReviewState::Reviewed,
        }
    }

    /// Whether `path`'s current exact comparison has been reviewed.
    pub fn file_reviewed(&self, path: &str) -> bool {
        self.file_review_state(path) == FileReviewState::Reviewed
    }

    /// The reviewed state of the active Changes target; `None` means unavailable.
    /// Dispatch and footer eligibility share [`Self::review_target`].
    pub fn current_file_reviewed(&self) -> Option<bool> {
        self.review_target().map(|(_, reviewed)| reviewed)
    }

    /// Toggle the active review target. A stale loaded diff stores nothing and requests a
    /// fresh atomic snapshot.
    pub fn toggle_current_file_reviewed(&mut self) {
        if let Some((path, reviewed)) = self.review_target() {
            let path = path.to_string();
            self.set_file_reviewed(&path, !reviewed);
        } else if self.review_display_is_stale() {
            self.request_world_refresh(false);
        }
    }

    /// Resolve the active Changes file and prove its painted comparison matches the landed
    /// world snapshot.
    fn review_target(&self) -> Option<(&str, bool)> {
        if self.tab != Tab::Changes || self.mode != Mode::Normal {
            return None;
        }
        let path = match self.focus {
            Focus::Files => self.current_entry()?.path.as_str(),
            Focus::Diff => self.diff_path.as_deref()?,
        };
        if self.diff_path.as_deref() != Some(path) {
            return None;
        }
        let landed = &self.changeset.files.get(path)?.identity;
        if self.diff.identity.as_ref() != Some(landed) {
            return None;
        }
        Some((path, self.file_reviewed(path)))
    }

    /// Whether a review attempt failed specifically because the displayed diff identity is
    /// absent or stale. Other ineligible surfaces stay inert without scheduling work.
    fn review_display_is_stale(&self) -> bool {
        if self.tab != Tab::Changes || self.mode != Mode::Normal {
            return false;
        }
        let path = match self.focus {
            Focus::Files => match self.current_entry() {
                Some(entry) => entry.path.as_str(),
                None => return false,
            },
            Focus::Diff => match self.diff_path.as_deref() {
                Some(path) => path,
                None => return false,
            },
        };
        if self.diff_path.as_deref() != Some(path) {
            return true;
        }
        self.changeset
            .files
            .get(path)
            .is_some_and(|landed| self.diff.identity.as_ref() != Some(&landed.identity))
    }

    /// Set one landed path's review decision, storing its exact identity.
    /// Returns whether authored state changed.
    pub fn set_file_reviewed(&mut self, path: &str, reviewed: bool) -> bool {
        let Some(context) = self.review_context.clone() else { return false };
        if reviewed {
            let Some(identity) = self.changeset.files.get(path).map(|a| a.identity.clone()) else {
                return false;
            };
            let edits = if self.diff_path.as_deref() == Some(path)
                && self.diff.identity.as_ref() == Some(&identity)
            {
                self.diff.reviewed_edits()
            } else {
                Vec::new()
            };
            let paths = self.reviewed.entry(context).or_default();
            if paths.get(path).is_some_and(|mark| {
                !mark.changed && mark.identity == identity && mark.edits == edits
            }) {
                return false;
            }
            paths.insert(path.to_string(), ReviewMark { identity, edits, changed: false });
            self.sync_reviewed_rows();
            return true;
        }
        let Some(paths) = self.reviewed.get_mut(&context) else { return false };
        let changed = paths.remove(path).is_some();
        if paths.is_empty() {
            self.reviewed.remove(&context);
        }
        if changed {
            self.sync_reviewed_rows();
        }
        changed
    }

    /// Whether the open source or rendered row belongs to a reviewed edit that is still exact.
    pub(crate) fn diff_row_reviewed(&self, row: usize) -> bool {
        let Some(row) = self.visible.get(row) else { return false };
        DiffLine::of(row).is_some_and(|line| self.reviewed_diff_lines.contains(&line))
            || unit_of(row).is_some_and(|unit| self.reviewed_rendered_units.contains(&unit))
    }

    /// Rebuild the derived row set from the painted comparison and its retained review mark.
    fn sync_reviewed_rows(&mut self) {
        let lines = (|| {
            let path = self.diff_path.as_deref()?;
            let context = self.review_context.as_ref()?;
            let mark = self.reviewed.get(context)?.get(path)?;
            let identity = self.diff.identity.as_ref()?;
            mark.identity.same_old_side(identity).then(|| self.diff.reviewed_lines(&mark.edits))
        })()
        .unwrap_or_default();
        self.reviewed_diff_lines = lines;
        self.reviewed_rendered_units.clear();
        if !self.reviewed_diff_lines.is_empty() && self.rendered.on_screen() {
            self.reviewed_rendered_units = self
                .rendered
                .marks
                .reviewed_units(diff_lines(&self.diff.rows), &self.reviewed_diff_lines);
        }
    }

    /// `/`: open the search screen, from any tab, from either pane.
    pub fn open_search(&mut self) {
        // A held navigator-divider drag must not resize the search split.
        self.cancel_divider_drag();
        self.search = Some(SearchOverlay::new());
        self.mode = Mode::Search;
        // The empty query lists frecent files before the first keystroke.
        self.search_dirty = true;
    }

    /// `esc`: drop the screen whole, place untouched.
    pub fn close_search(&mut self) {
        if self.mode == Mode::Search {
            self.mode = Mode::Normal;
        }
        self.search = None;
        self.search_dirty = false;
    }

    /// Whether the foot band opens: a file tab with at least one content row, source or rendered.
    pub fn find_available(&self) -> bool {
        self.tab.is_file_tab() && self.visible.iter().any(Row::is_content)
    }

    /// `ctrl+f`: open find as a fresh gesture, focusing the read pane.
    pub fn open_find(&mut self) {
        if !self.find_available() {
            return;
        }
        self.cancel_divider_drag();
        self.clear_selection();
        self.focus = Focus::Diff;
        self.mode = Mode::Find;
        self.find = Some(Find::default());
        // The band takes the bottom row; keep the cursor above it.
        self.reveal_diff = true;
    }

    /// `:`: open the line field in the band's place, wherever find opens, over the open file.
    /// Nothing moves until Enter ([`App::line_go`]).
    pub fn open_line(&mut self) {
        self.open_find();
        if let Some(f) = self.find.as_mut() {
            f.kind = BandKind::Line;
        }
    }

    /// Whether the band open is the line field.
    pub fn line_open(&self) -> bool {
        self.mode == Mode::Find && self.find.as_ref().is_some_and(|f| f.kind == BandKind::Line)
    }

    /// The line Enter jumps to: the number (`0` first, `$` last, as in vim); `None` when empty.
    pub fn line_target(&self) -> Option<u32> {
        let f = self.find.as_ref().filter(|_| self.line_open())?;
        match f.query.as_str() {
            "$" => Some(u32::MAX),
            digits if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
                Some(digits.parse::<u32>().unwrap_or(u32::MAX))
            }
            _ => None,
        }
    }

    /// Type into the line field: a digit extends the number, `$` (the last line) stands alone,
    /// and a digit after `$` starts a number.
    pub fn line_type(&mut self, c: char) {
        self.edit_input(|v, caret| {
            if c == '$' || v.as_slice() == ['$'] {
                v.clear();
                *caret = 0;
            }
            v.insert(*caret, c);
            *caret += 1;
        });
    }

    /// `enter` in the line field: close it and jump to the typed line. An empty field only
    /// closes.
    pub fn line_go(&mut self) {
        let target = self.line_target();
        self.close_find();
        if let Some(n) = target {
            self.goto_line(n);
        }
    }

    /// Which side's numbers `:N` uses: the new side's, or the old side's in a file with no new
    /// lines (deleted or emptied).
    fn line_side(&self) -> fn(&Row) -> Option<u32> {
        if crate::marks::diff_lines(&self.visible).any(|r| r.new_no().is_some()) {
            Row::new_no
        } else {
            Row::old_no
        }
    }

    /// The open file's line count, as `:N` numbers it: the source's lines rendered, else the
    /// last line on [`App::line_side`].
    pub fn line_count(&self) -> usize {
        if self.rendered_active() {
            return self.rendered.content.as_ref().map_or(0, |c| c.text.lines().count());
        }
        let side = self.line_side();
        crate::marks::diff_lines(&self.visible).filter_map(side).max().unwrap_or(0) as usize
    }

    /// Paste into the line field: a number or `$` replaces it, wrapping punctuation dropped;
    /// anything else, a `path:line` included, leaves it as it was.
    fn paste_line(&mut self, text: &str) {
        // Quotes, brackets and trailing punctuation drop; a sign stays and makes it no line.
        let wrap =
            ['`', '\'', '"', '“', '”', '‘', '’', '(', ')', '[', ']', '<', '>', ':', '.', ',', ';'];
        let word = text.trim().trim_matches(wrap);
        let line = word == "$" || (!word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()));
        if let Some(f) = self.find.as_mut().filter(|_| line) {
            f.query = word.to_string();
            f.caret = f.query.chars().count();
        }
    }

    /// `esc`: close the band, dropping the query. The cursor stays where the last step left it
    pub fn close_find(&mut self) {
        if self.mode == Mode::Find {
            self.mode = Mode::Normal;
        }
        self.find = None;
    }

    /// `:N`: land on line `n`, clamped to the file, opening what hides it, centered off screen.
    pub fn goto_line(&mut self, n: u32) {
        if !self.find_available() {
            return;
        }
        let last = u32::try_from(self.line_count()).unwrap_or(u32::MAX).max(1);
        let n = n.clamp(1, last);
        self.clear_selection();
        self.focus = Focus::Diff;
        if self.rendered_active() {
            self.open_disclosures_holding(n);
            self.diff_cursor = self.rendered.index.row_at_line(n).unwrap_or(0);
        } else {
            self.diff_cursor = self.land_on_line(n, self.line_side());
        }
        self.reveal_center = true;
    }

    /// The row holding line `n` by `side`, its fold opened first: find steps and `:N` share it.
    fn land_on_line(&mut self, n: u32, side: fn(&Row) -> Option<u32>) -> usize {
        let row = line_row(&self.visible, n, side);
        let Some(anchor) = self.visible[row].fold_anchor() else { return row };
        self.expanded_folds.insert(anchor);
        self.rebuild_visible();
        line_row(&self.visible, n, side)
    }

    /// Open every collapsed `<details>` holding source line `n`, so the line renders.
    fn open_disclosures_holding(&mut self, n: u32) {
        let line = n as usize;
        let closed: Vec<String> = self
            .rendered
            .doc
            .disclosures
            .iter()
            .filter(|d| d.body <= line && line <= d.end && !self.details_open(&d.key))
            .map(|d| d.key.clone())
            .collect();
        if closed.is_empty() {
            return;
        }
        for key in closed {
            self.rendered.details.insert(key, true);
        }
        self.rebuild_visible();
    }

    /// Whether the rendered rows were built with the `<details>` keyed `key` open.
    fn details_open(&self, key: &str) -> bool {
        self.rendered.built.as_ref().is_some_and(|b| b.input.details.iter().any(|k| k == key))
    }

    /// Every match in file order, folds included, the count before the cursor, and whether it matches.
    fn find_hits(&self, query: &str) -> (Vec<FindHit>, usize, bool) {
        let cs = find_case_sensitive(query);
        // A row's own text: a rendered row's never carries a marker's or a note's words.
        let is_hit = |row: &Row| !find_match_ranges(&row.text(), query, cs).is_empty();
        let mut hits = Vec::new();
        let mut cursor_rank = 0usize;
        let mut on_match = false;
        for (vis, row) in self.visible.iter().enumerate() {
            let here = vis == self.diff_cursor;
            if here {
                cursor_rank = hits.len();
            }
            if let Row::Fold { lines } = row {
                // Folded lines are context, so each has a new-side number.
                let folded = lines.iter().filter(|l| is_hit(l)).filter_map(Row::new_no);
                hits.extend(folded.map(|new_no| FindHit::Folded { new_no }));
                continue;
            }
            let m = is_hit(row);
            if here {
                on_match = m;
            }
            if m {
                hits.push(FindHit::Visible(vis));
            }
        }
        (hits, cursor_rank, on_match)
    }

    /// Step to the next or previous match, wrapping and expanding a fold that hides it.
    pub fn find_step(&mut self, delta: i32) {
        let Some(query) = self.find_query().map(str::to_string) else { return };
        if query.is_empty() {
            return;
        }
        let (hits, cursor_rank, on_match) = self.find_hits(&query);
        if hits.is_empty() {
            return;
        }
        let len = hits.len();
        // Forward skips the match the cursor sits on.
        let target = if delta > 0 {
            (cursor_rank + usize::from(on_match)) % len
        } else {
            (cursor_rank + len - 1) % len
        };
        match hits[target] {
            FindHit::Visible(v) => self.diff_cursor = v,
            FindHit::Folded { new_no } => self.diff_cursor = self.land_on_line(new_no, Row::new_no),
        }
        self.reveal_diff = true;
    }

    /// Find's query; `None` for the line field, whose digits are no search.
    pub fn find_query(&self) -> Option<&str> {
        self.find.as_ref().filter(|f| f.kind == BandKind::Text).map(|f| f.query.as_str())
    }

    /// The current match's ordinal and the total; `None` for an empty query.
    pub fn find_count(&self) -> Option<(Option<usize>, usize)> {
        let query = self.find_query()?;
        if query.is_empty() {
            return None;
        }
        let (hits, cursor_rank, on_match) = self.find_hits(query);
        Some((on_match.then_some(cursor_rank + 1), hits.len()))
    }

    /// `tab`: flip the mode, keeping the query, the pick on the first result.
    pub fn search_flip(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.search_mode = match s.search_mode {
                SearchMode::Files => SearchMode::Code,
                SearchMode::Code => SearchMode::Files,
            };
            s.pick = 0;
            s.scroll.set(0);
        }
    }

    /// `↓`/`↑`, `ctrl+n`/`p`: move the pick by `delta`, only while `Ready`.
    pub fn search_move(&mut self, delta: isize) {
        // Off `Ready` no rows are painted.
        if let Some(s) = self.search.as_mut()
            && s.phase == SearchPhase::Ready
        {
            s.pick = step(s.pick, delta, s.picks());
        }
    }

    /// Land a search completion, the pick back on the first result.
    pub fn apply_search_completion(&mut self, completion: crate::search::SearchCompletion) {
        use crate::search::SearchOutcome;
        let Some(s) = self.search.as_mut() else { return };
        match completion.outcome {
            SearchOutcome::Ready(results) => {
                s.results = results;
                s.phase = SearchPhase::Ready;
                s.pick = 0;
                s.scroll.set(0);
            }
            SearchOutcome::Indexing => s.phase = SearchPhase::Indexing,
            SearchOutcome::Failed(e) => {
                s.phase = SearchPhase::Error(e);
                // No stale file under the error.
                s.preview = None;
            }
        }
    }

    /// Rebuild the preview if it no longer shows the pick; idempotent.
    pub fn build_search_preview(&mut self) {
        let Some(s) = self.search.as_ref() else { return };
        // The pick's target, or `None` when nothing is pickable (off `Ready`, or empty).
        let picked = (s.phase == SearchPhase::Ready).then(|| s.picked()).flatten();
        // Compared by reference, so an unchanged frame allocates nothing.
        let shows_pick = match (s.preview.as_ref(), picked.as_ref()) {
            (None, None) => true,
            (Some(pv), Some(PickedResult::File(f))) => pv.hit.is_none() && pv.path == f.path,
            (Some(pv), Some(PickedResult::Code(c))) => {
                pv.path == c.path
                    && pv.hit.as_ref().is_some_and(|(l, sp)| *l == c.line && sp == &c.spans)
            }
            _ => false,
        };
        if shows_pick {
            return;
        }
        let target = picked.map(|picked| match picked {
            PickedResult::File(f) => (f.path.clone(), None),
            PickedResult::Code(c) => (c.path.clone(), Some((c.line, c.spans.clone()))),
        });
        let Some((path, hit)) = target else {
            if let Some(s) = self.search.as_mut() {
                s.preview = None;
            }
            return;
        };
        let diff = self.file_view(&path).0;
        if let Some(s) = self.search.as_mut() {
            s.preview = Some(SearchPreview {
                path,
                diff,
                hit,
                scroll: std::cell::Cell::new(0),
                center: std::cell::Cell::new(true),
            });
        }
    }

    /// `PageUp`/`PageDown`: scroll the preview.
    pub fn scroll_search_preview(&mut self, delta: isize) {
        if let Some(p) = self.search.as_ref().and_then(|s| s.preview.as_ref()) {
            p.center.set(false);
            p.scroll.set(p.scroll.get().saturating_add_signed(delta));
        }
    }

    /// Rebuild the previewed file after a refresh, keeping the scroll (Continuity).
    pub fn refresh_search_preview(&mut self) {
        let Some(path) =
            self.search.as_ref().and_then(|s| s.preview.as_ref()).map(|p| p.path.clone())
        else {
            return;
        };
        let diff = self.file_view(&path).0;
        if let Some(pv) = self.search.as_mut().and_then(|s| s.preview.as_mut()) {
            pv.diff = diff;
        }
    }

    /// `enter`: open the pick in `All files`, at its line for a code hit; a vanished path stays.
    pub fn search_open_pick(&mut self) -> Result<()> {
        let Some(s) = self.search.as_ref() else { return Ok(()) };
        // Off `Ready` no rows are painted.
        if s.phase != SearchPhase::Ready {
            return Ok(());
        }
        let (path, line) = match s.picked() {
            Some(PickedResult::File(f)) => (f.path.clone(), None),
            Some(PickedResult::Code(c)) => (c.path.clone(), Some(c.line)),
            None => return Ok(()),
        };
        if !self.repo.join(&path).is_file() {
            return Ok(());
        }
        self.close_search();
        // The origin tab stashes its place.
        if self.tab != Tab::AllFiles {
            self.set_tab(Tab::AllFiles)?;
        }
        // The pick feeds the engine's frecency store, so ranking improves with use.
        self.search_track = Some(path.clone());
        let mut expanded = false;
        let mut dir = path.as_str();
        while let Some((parent, _)) = dir.rsplit_once('/') {
            expanded |= self.set_dir_expanded(parent, true);
            dir = parent;
        }
        if expanded {
            // Search never returns an ignored path, so no worktree walk is needed.
            self.rebuild_file_rows();
        }
        self.reset_diff_view();
        self.set_file_view(&path);
        if let Some(fi) = self.file_row_of_path(&path) {
            self.file_cursor = fi;
            self.reveal_files = true;
        }
        self.focus = Focus::Diff;
        if let Some(line) = line {
            // Rendered, land on the block holding the line.
            let last = self.visible.len().saturating_sub(1);
            self.diff_cursor = if self.rendered_active() {
                let line = u32::try_from(line).unwrap_or(u32::MAX);
                self.rendered.index.row_at_line(line).unwrap_or(0)
            } else {
                (line.saturating_sub(1) as usize).min(last)
            };
        }
        self.reveal_diff = true;
        Ok(())
    }

    pub fn open_list(&mut self) {
        if !self.store.is_empty() {
            self.list_cursor = 0;
            self.mode = Mode::List;
        }
    }

    pub fn close_list(&mut self) {
        if self.mode == Mode::List {
            self.mode = Mode::Normal;
        }
    }

    /// The footer's actions for this context, each with its [`Band`].
    #[must_use]
    pub fn footer_bands(&self) -> Vec<(FooterAction, Band)> {
        use Band::{Do, Go, Move, Primary, Send};
        use FooterAction as A;

        // A modal owns the bar, its exit right after the primary so trims spare it.
        if self.confirming_quit {
            return vec![(A::QuitDiscard, Primary), (A::Cancel, Do), (A::Send, Do), (A::Copy, Do)];
        }
        match self.mode {
            Mode::Composing { .. } => {
                return vec![(A::Save, Primary), (A::Cancel, Do), (A::Newline, Do)];
            }
            Mode::List => {
                let mut out = vec![(A::Send, Primary), (A::CloseList, Do), (A::Copy, Do)];
                // A comment from another diff cannot be edited here.
                if self.list_comment_editable() {
                    out.push((A::EditComment, Do));
                }
                out.push((A::DeleteComment, Do));
                return out;
            }
            Mode::Picker => {
                return vec![(A::PickAgent, Primary), (A::ClosePicker, Do), (A::MovePickerRow, Do)];
            }
            Mode::BasePick => {
                return vec![(A::PickBaseRow, Primary), (A::ClosePicker, Do), (A::MoveBaseRow, Do)];
            }
            Mode::CommitPick => {
                // Nothing listed: only the exit.
                if self.commit_picker.as_ref().is_none_or(CommitPicker::is_empty) {
                    return vec![(A::CloseCommitPicker, Primary)];
                }
                let mut out = vec![
                    (A::PickCommitRun, Primary),
                    (A::CloseCommitPicker, Do),
                    (A::MoveCommitRow, Do),
                ];
                // The pick row takes no anchor, so `v` is not offered on it.
                if self.commit_picker.as_ref().is_some_and(|cp| !cp.is_pick_row(cp.cursor)) {
                    out.push((A::CommitAnchor, Do));
                }
                return out;
            }
            Mode::Search => {
                // Nothing pickable: only the flip and the exit.
                let pickable = self
                    .search
                    .as_ref()
                    .is_some_and(|s| s.phase == SearchPhase::Ready && s.picks() > 0);
                return if pickable {
                    vec![
                        (A::FlipSearchMode, Primary),
                        (A::PickResult, Do),
                        (A::OpenResult, Do),
                        (A::CloseSearch, Do),
                    ]
                } else {
                    vec![(A::FlipSearchMode, Primary), (A::CloseSearch, Do)]
                };
            }
            Mode::Find if self.line_open() => {
                // Enter jumps only to a line; an empty field only closes.
                return if self.line_target().is_some() {
                    vec![(A::LineGo, Primary), (A::CloseFind, Do)]
                } else {
                    vec![(A::CloseFind, Primary)]
                };
            }
            Mode::Find => {
                // Steps only with a match to step to.
                let has_match = self.find_count().is_some_and(|(_, total)| total > 0);
                return if has_match {
                    vec![(A::FindStep, Primary), (A::CloseFind, Do)]
                } else {
                    vec![(A::CloseFind, Primary)]
                };
            }
            Mode::Normal => {}
        }

        // `PR`: `o open` for any resolved PR; `move` holds only the steps the tab has.
        if self.tab == Tab::Pr {
            let mut out = Vec::new();
            if self.pr_snapshot().is_some() {
                out.push((A::OpenPr, Primary));
            }
            out.push((A::Search, Go));
            out.push((A::TogglePane, Go));
            out.push((A::NavigatorPosition, Go));
            out.push((A::Tabs, Go));
            out.push((A::Refresh, Go));
            out.push((A::Quit, Go));
            out.push((A::MoveLine, Move));
            out.push((A::MovePage, Move));
            return out;
        }

        let mut out: Vec<(FooterAction, Band)> = Vec::new();
        // Whether the diff-jump is already the primary, so the `go` band doesn't repeat the toggle.
        let mut pane_is_primary = false;

        if self.file_rows.is_empty()
            && self.scope == Scope::Branch
            && self.branch_base.winner.is_none()
            && self.base_pick_available()
        {
            // No base: the picker leads.
            out.push((A::BasePick, Primary));
            out.push((A::ScopeOther, Do));
            out.push((A::Refresh, Do));
        } else if self.commits_gone() && self.tab == Tab::Changes {
            // A gone pick: the picker leads.
            out.push((A::CommitPick, Primary));
            out.push((A::ScopeOther, Do));
            out.push((A::Refresh, Do));
        } else if self.file_rows.is_empty() {
            // Nothing to review: switch scope or refresh.
            out.push((A::ScopeOther, Primary));
            out.push((A::Refresh, Do));
        } else if self.focus == Focus::Files {
            match self.file_rows.get(self.file_cursor).map(|r| &r.kind) {
                Some(RowKind::Dir { expanded: true, .. }) => out.push((A::CollapseDir, Primary)),
                Some(RowKind::Dir { expanded: false, .. }) => out.push((A::ExpandDir, Primary)),
                _ => {
                    out.push((A::TogglePane, Primary)); // tab into the diff to review
                    pane_is_primary = true;
                }
            }
            // The files pane's calm row 1 has the room for the hide key.
            out.push((A::NavigatorHide, Do));
        } else if self.visible.is_empty() {
            if self.navigator_hidden_here() {
                // The hidden empty read pane: the way back leads row 1.
                out.push((A::NavigatorHide, Primary));
                out.push((A::TogglePane, Do));
            } else {
                // Diff focused but nothing to show (e.g. a binary): only the scope switch helps.
                out.push((A::ScopeOther, Primary));
            }
        } else if self.on_fold() {
            out.push((A::ExpandFold, Primary));
        } else if self.select_anchor.is_some() {
            out.push((A::Comment, Primary));
            out.push((A::ClearSelection, Do));
        } else if self.comment_claims_edit() {
            out.push((A::EditComment, Primary));
            out.push((A::DeleteComment, Do));
            out.push((A::JumpComment, Do));
        } else {
            out.push((A::Comment, Primary));
            out.push((A::Select, Do));
            // The markdown flip, where a rendered view exists.
            if self.toggle_acts() && !self.rendered.renders_nothing() {
                out.push((A::Rendered, Do));
            }
        }

        // `edit` wherever the press opens a file, ahead of the navigator keys.
        if self.edit_opens_a_file() {
            let at = out
                .iter()
                .position(|&(a, band)| band == Do && a == A::NavigatorHide)
                .unwrap_or(out.len());
            out.insert(at, (A::EditFile, Do));
        }

        if self.current_file_reviewed().is_some() {
            let at = out
                .iter()
                .position(|&(a, band)| band == Do && a == A::NavigatorHide)
                .unwrap_or(out.len());
            out.insert(at, (A::ToggleReviewed, Do));
        }

        // An armed crossing leads, so the reviewer sees the next press leaves the file.
        if let Some(forward) = self.armed_cross() {
            out[0].1 = Do;
            out.insert(0, (A::CrossFile { forward }, Primary));
        }

        // `send` closes row 1 once a comment exists.
        if !self.store.is_empty() {
            out.push((A::Send, Send));
        }

        // The `go` band: keys that work anywhere, never repeating row 1.
        if !out.iter().any(|&(a, _)| a == A::Scope || a == A::ScopeOther) {
            out.push((A::Scope, Go));
        }
        // The base picker's key shows only where it works.
        if self.base_pick_available() && !out.iter().any(|&(a, _)| a == A::BasePick) {
            out.push((A::BasePick, Go));
        }
        // The commit picker's key works on every file tab.
        if !out.iter().any(|&(a, _)| a == A::CommitPick) {
            out.push((A::CommitPick, Go));
        }
        out.push((A::Search, Go));
        // In-file find shows wherever the read pane has content to search.
        if self.find_available() {
            out.push((A::Find, Go));
            out.push((A::GotoLine, Go));
        }
        // Rendered rows come pre-wrapped, so `w` acts only on source.
        if !self.rendered_active() {
            out.push((A::Wrap, Go));
        }
        if !self.store.is_empty() {
            out.push((A::List, Go));
            out.push((A::Copy, Go));
        }
        if !out.iter().any(|&(a, _)| a == A::Refresh) {
            out.push((A::Refresh, Go));
        }
        out.push((A::Tabs, Go));
        // `tab` un-hides while hidden, so it stays offered even with an empty changeset
        if !out.iter().any(|&(a, _)| a == A::TogglePane)
            && !pane_is_primary
            && (!self.file_rows.is_empty() || self.navigator_hidden_here())
        {
            out.push((A::TogglePane, Go));
        }
        if !self.navigator_hidden_here() {
            out.push((A::NavigatorPosition, Go));
        }
        if !out.iter().any(|&(a, _)| a == A::NavigatorHide) {
            out.push((A::NavigatorHide, if self.navigator_hidden_here() { Do } else { Go }));
        }
        out.push((A::Quit, Go));

        // The `move` band, with hunk steps only where `step_hunk` reaches and nothing is armed.
        if !self.file_rows.is_empty() {
            out.push((A::MoveLine, Move));
            if self.tab == Tab::Changes && self.armed_cross().is_none() {
                out.push((A::MoveHunk, Move));
            }
            out.push((A::MoveFile, Move));
            out.push((A::MovePage, Move));
        }
        out
    }

    pub fn list_move(&mut self, delta: isize) {
        if self.mode == Mode::List && !self.store.is_empty() {
            self.list_cursor = step(self.list_cursor, delta, self.store.len());
        }
    }
}

/// The picker's opening row: the last-sent agent if still listed, else the first.
fn armed_row(rows: &[AgentChoice], last_sent: Option<&str>) -> usize {
    last_sent.and_then(|pane| rows.iter().position(|row| row.pane_id == pane)).unwrap_or(0)
}

impl App {
    /// `Send`: one agent sends, several open the picker, none points at the clipboard.
    pub fn send_to_agent(&mut self) {
        if self.store.is_empty() {
            self.status = "no comments yet".to_string();
            return;
        }
        match herdr::send_target(&self.herdr.ids) {
            Ok(SendTarget::One(agent)) => self.export_to_agent(&agent),
            Ok(SendTarget::Many(rows)) => self.open_picker(rows),
            Err(e) => self.status = crate::export::send_failure(&e, None, &self.copy_key()),
        }
    }

    /// The copy key as its hint shows it, the way out of a failed send.
    fn copy_key(&self) -> String {
        self.keymap().hint(crate::keymap::Action::Copy).label()
    }

    /// Open the agent picker over `rows`, armed on the last-sent agent.
    pub fn open_picker(&mut self, rows: Vec<AgentChoice>) {
        // Empty or nested, it would be a modal one `esc` cannot leave.
        if rows.is_empty() || self.mode == Mode::Picker {
            return;
        }
        self.picker_cursor = armed_row(&rows, self.last_sent_pane.as_deref());
        self.picker_rows = rows;
        self.picker_over = self.mode.clone();
        self.mode = Mode::Picker;
    }

    /// Close the picker onto the view it opened over.
    pub fn close_picker(&mut self) {
        if self.mode == Mode::Picker {
            self.mode = std::mem::replace(&mut self.picker_over, Mode::Normal);
        }
        self.picker_rows.clear();
        self.picker_cursor = 0;
    }

    pub fn picker_move(&mut self, delta: isize) {
        if self.mode == Mode::Picker && !self.picker_rows.is_empty() {
            self.picker_cursor = step(self.picker_cursor, delta, self.picker_rows.len());
        }
    }

    /// Highlight `row`; past the end is inert, never clamped.
    pub fn picker_goto(&mut self, row: usize) {
        if self.mode == Mode::Picker && row < self.picker_rows.len() {
            self.picker_cursor = row;
        }
    }

    /// Send to the highlighted agent and close either way; a failure keeps the comments.
    pub fn picker_pick(&mut self) {
        let Some(agent) = self.picker_rows.get(self.picker_cursor).cloned() else { return };
        self.close_picker();
        self.export_to_agent(&agent);
    }

    /// Whether the base picker opens: a file tab without `--base`.
    #[must_use]
    pub fn base_pick_available(&self) -> bool {
        self.tab.is_file_tab() && self.base.is_none()
    }

    /// Open the base picker on the current base; it opens empty too, so a revision can be typed.
    pub fn open_base_picker(&mut self) {
        if !self.base_pick_available() || self.mode != Mode::Normal {
            return;
        }
        // Re-resolved: `branch_base` lands only under `branch`.
        let resolution = match git::resolve_base(&self.repo, self.base.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                self.status = e.0;
                return;
            }
        };
        let (winner, default) = (resolution.status.winner, resolution.default);
        let listed = git::list_branches(&self.repo).and_then(|rows| {
            let current = git::checked_out_branch(&self.repo)?;
            Ok((rows, current))
        });
        let (branches, current) = match listed {
            Ok(v) => v,
            Err(e) => {
                self.status = e.0;
                return;
            }
        };
        let target = self
            .pr_snapshot()
            .filter(|s| s.state == forge::PrState::Open)
            .map(|s| s.base_ref.clone());
        let mut rows: Vec<BaseChoice> = branches
            .into_iter()
            .map(|b| BaseChoice::Branch {
                pr_base: target.as_deref() == Some(b.name.as_str()),
                is_default: default.as_deref() == Some(b.name.as_str()),
                current: current.as_deref() == Some(b.name.as_str()),
                tip_secs: b.tip_secs,
                name: b.name,
            })
            .collect();
        // A stable sort, so recency still orders the promoted pair and the rest alike.
        rows.sort_by_key(|r| (!r.pr_base(), !r.is_default()));
        if let Some(git::ResolvedBase::Rev { spelling, oid }) = &winner
            && !rows.iter().any(|r| r.name() == spelling)
        {
            rows.insert(0, BaseChoice::Rev { name: spelling.clone(), oid: oid.clone() });
        }
        let current = winner.as_ref().map(git::ResolvedBase::name);
        let cursor = current.and_then(|c| rows.iter().position(|r| r.name() == c)).unwrap_or(0);
        self.base_picker = Some(BasePicker {
            rows,
            cursor,
            query: String::new(),
            caret: 0,
            probe: BaseProbe::Idle,
        });
        self.mode = Mode::BasePick;
    }

    pub fn close_base_picker(&mut self) {
        if self.mode == Mode::BasePick {
            self.mode = Mode::Normal;
        }
        self.base_picker = None;
    }

    /// Move the highlight through the visible view.
    pub fn base_picker_move(&mut self, delta: isize) {
        let Some(bp) = self.base_picker.as_mut() else { return };
        let len = bp.visible().len();
        if len > 0 {
            bp.cursor = step(bp.cursor.min(len - 1), delta, len);
        }
    }

    /// Move the highlight to visible `row`, for a click. A row past the end is inert
    pub fn base_picker_goto(&mut self, row: usize) {
        if let Some(bp) = self.base_picker.as_mut()
            && row < bp.visible().len()
        {
            bp.cursor = row;
        }
    }

    /// Pick the highlight, or a resolving query, and rebuild; the default row clears the pick.
    pub fn base_picker_pick(&mut self) -> Result<()> {
        let Some(bp) = &self.base_picker else { return Ok(()) };
        if bp.visible().is_empty() {
            if bp.query.is_empty() {
                return Ok(());
            }
            self.run_base_probe();
        }
        let choice = {
            let Some(bp) = &self.base_picker else { return Ok(()) };
            bp.visible().get(bp.cursor).map(|c| (*c).clone())
        };
        let Some(choice) = choice else { return Ok(()) };
        // Build against the choice before the private ref or visible state moves. After the
        // ref write, ordinary inputs resolve the same repository-owned pick.
        let mut input = self.world_input();
        input.scope = Scope::Branch;
        input.base = Some(choice.name().to_string());
        input.base_epoch = input.base_epoch.wrapping_add(1);
        let build = self.build_rebase(&input)?;
        let write = git::write_base_pick(&self.repo, choice.name());
        if let Err(e) = write {
            self.status = e.0;
            return Ok(());
        }
        self.close_base_picker();
        // Epoch first, so an in-flight build of the old pick never lands.
        self.base_epoch = self.base_epoch.wrapping_add(1);
        // A pick takes the reviewer to the scope it configures, like the commit picker
        self.scope = Scope::Branch;
        self.adopt_rebase(build);
        self.reveal_files = true;
        Ok(())
    }

    /// Open the commit picker with the current pick highlighted and anchored.
    pub fn open_commit_picker(&mut self) {
        if !self.tab.is_file_tab() || self.mode != Mode::Normal {
            return;
        }
        let mut picker = match self.list_commit_rows() {
            Ok(p) => p,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        match self.commit_pick.clone() {
            Some(p) if picker.wholly_lists(&p) => {
                let n = picker.index_of(&p.newest).expect("wholly listed");
                let o = picker.index_of(&p.oldest).expect("wholly listed");
                picker.cursor = n;
                picker.anchor = (o > n).then_some(o);
            }
            Some(p) => {
                picker.pick_row = Some(p);
                picker.cursor = 0;
            }
            None => {}
        }
        self.commit_picker = Some(picker);
        self.mode = Mode::CommitPick;
    }

    /// The commit picker's rows and title, the last 50 without a merge-base.
    fn list_commit_rows(&self) -> Result<CommitPicker, String> {
        let head = git::head_oid(&self.repo);
        let base = git::resolve_base(&self.repo, self.base.as_deref())
            .map_err(|e| e.0)?
            .status
            .winner
            .and_then(|b| git::merge_base(&self.repo, b.oid()).map(|mb| (b, mb)))
            // On the base branch itself the range is empty: the last 50 is the universe.
            .filter(|(_, mb)| head.as_deref() != Some(mb.as_str()));
        let rows = git::list_commits(&self.repo, base.as_ref().map(|(_, mb)| mb.as_str()))
            .map_err(|e| e.to_string())?;
        // Only an unborn repository lists nothing.
        let title = match &base {
            Some((b, _)) => format!("commits · {} over {}", rows.len(), b.name()),
            None => "commits · last 50".to_string(),
        };
        let empty = "no commits yet".to_string();
        Ok(CommitPicker { rows, pick_row: None, cursor: 0, anchor: None, title, empty, head })
    }

    /// Close the commit picker; the scope stays.
    pub fn close_commit_picker(&mut self) {
        if self.mode == Mode::CommitPick {
            self.mode = Mode::Normal;
        }
        self.commit_picker = None;
    }

    /// `esc` in the picker: clear the anchor, else close.
    pub fn commit_picker_escape(&mut self) {
        match self.commit_picker.as_mut() {
            Some(cp) if cp.anchor.is_some() => cp.anchor = None,
            _ => self.close_commit_picker(),
        }
    }

    /// Move the highlight, clamped at the ends.
    pub fn commit_picker_move(&mut self, delta: isize) {
        let Some(cp) = self.commit_picker.as_mut() else { return };
        cp.cursor = step(cp.cursor, delta, cp.len());
    }

    /// Move the highlight to visible `row`, for a click. A row past the end is inert.
    pub fn commit_picker_goto(&mut self, row: usize) {
        if let Some(cp) = self.commit_picker.as_mut()
            && row < cp.len()
        {
            cp.cursor = row;
        }
    }

    /// `v`: toggle the anchor on the highlight; never on the pick row.
    pub fn commit_picker_anchor(&mut self) {
        let Some(cp) = self.commit_picker.as_mut() else { return };
        if cp.is_empty() || cp.is_pick_row(cp.cursor) {
            return;
        }
        cp.anchor = if cp.anchor == Some(cp.cursor) { None } else { Some(cp.cursor) };
    }

    /// `enter`: pick the run, switch to `commits`, and rebuild.
    pub fn commit_picker_pick(&mut self) -> Result<()> {
        let Some(pick) = self.commit_picker.as_ref().and_then(CommitPicker::picked) else {
            return Ok(());
        };
        let mut input = self.world_input();
        input.scope = Scope::Commits;
        input.commit_pick = Some(pick.clone());
        let build = self.build_rebase(&input)?;
        self.close_commit_picker();
        self.commit_pick = Some(pick);
        // The old status describes the old shas.
        self.pick_status = None;
        // The pick is world input, so a build of the old one never lands.
        self.scope = Scope::Commits;
        self.adopt_rebase(build);
        self.reveal_files = true;
        Ok(())
    }

    /// Once `HEAD` moved, re-list the picker and reconcile by sha (Continuity); else nothing.
    pub fn refresh_commit_picker(&mut self, head: Option<&str>) {
        if self.mode != Mode::CommitPick {
            return;
        }
        let Some(old) = self.commit_picker.as_ref() else { return };
        if old.head.as_deref() == head {
            return;
        }
        let Ok(mut fresh) = self.list_commit_rows() else { return };
        let old = self.commit_picker.take().expect("checked above");
        let pick = self.commit_pick.clone();
        if let Some(p) = pick.clone().filter(|p| !fresh.wholly_lists(p)) {
            fresh.pick_row = Some(p);
        }
        // The pick row follows the pick, onto its newest commit once wholly listed.
        let relocate = |i: usize| -> Option<usize> {
            if old.is_pick_row(i) {
                return match &fresh.pick_row {
                    Some(_) => Some(0),
                    None => pick.as_ref().and_then(|p| fresh.index_of(&p.newest)),
                };
            }
            let sha = &old.list_row(i)?.sha;
            fresh.index_of(sha)
        };
        let last = fresh.len().saturating_sub(1);
        // Identity, then the nearest survivor (above wins a tie), then clamp.
        let place = |i: usize| -> usize {
            if let Some(at) = relocate(i) {
                return at;
            }
            (1..old.len())
                .find_map(|d| i.checked_sub(d).and_then(relocate).or_else(|| relocate(i + d)))
                .unwrap_or(i.min(last))
        };
        let cursor = place(old.cursor);
        let mut anchor = old.anchor.map(place).filter(|&a| !fresh.is_pick_row(a));
        // A dissolved pick row seeds the whole run, the way `G` opens on it.
        if old.is_pick_row(old.cursor) && fresh.pick_row.is_none() {
            anchor = pick.as_ref().and_then(|p| fresh.index_of(&p.oldest)).filter(|&o| o > cursor);
        }
        fresh.cursor = cursor;
        fresh.anchor = anchor;
        self.commit_picker = Some(fresh);
    }

    /// Export to one decided pane; only a delivery records it as `last used`.
    fn export_to_agent(&mut self, agent: &AgentChoice) {
        let target = Agent { pane: agent.pane_id.clone(), name: agent.name.clone() };
        if self.export(&target) {
            self.last_sent_pane = Some(agent.pane_id.clone());
        }
    }

    /// Export every comment to `target`, consuming them only on success, which it returns.
    pub fn export(&mut self, target: &dyn ExportTarget) -> bool {
        if self.store.is_empty() {
            self.status = "no comments yet".to_string();
            return false;
        }
        let refs: Vec<&Comment> = self.store.iter().collect();
        let text = format_all(&refs);
        let n = refs.len();
        logln!("export ({n}) -> {} ::\n{text}", target.label());
        let delivered = match target.export(&text) {
            Ok(()) => {
                self.store.take_all();
                self.status = target.success_message(n);
                logln!("export OK");
                true
            }
            Err(e) => {
                self.status = target.failure_message(&e, &self.copy_key());
                logln!("export ERR: {e:#}");
                false
            }
        };
        self.clamp_list_cursor();
        if self.store.is_empty() {
            self.close_list();
        }
        self.refresh_rendered();
        delivered
    }

    /// Quit, asking first while comments are unsent.
    pub fn request_quit(&mut self) {
        if self.unsent() == 0 {
            self.should_quit = true;
        } else {
            self.confirming_quit = true;
        }
    }

    /// What a quit would drop: the comments, plus a non-empty new draft.
    pub fn unsent(&self) -> usize {
        let draft =
            matches!(self.mode, Mode::Composing { editing: None }) && !self.input.trim().is_empty();
        self.store.len() + usize::from(draft)
    }

    /// Whether the footer is the `Normal` bar with `?` and bands.
    pub fn footer_open_ended(&self) -> bool {
        self.mode == Mode::Normal && !self.confirming_quit
    }

    /// The scope's changed-file count, on every tab.
    pub fn changed_count(&self) -> usize {
        self.changeset.files.len()
    }

    /// The scope's line totals, saturating.
    pub fn changed_totals(&self) -> (u32, u32) {
        self.changeset.files.values().fold((0, 0), |(added, removed), a| {
            (added.saturating_add(a.additions), removed.saturating_add(a.deletions))
        })
    }

    /// Whether a comment's anchor may have moved: its file left the changeset, or the disk.
    pub fn is_stale(&self, c: &Comment) -> bool {
        if c.diff_anchored {
            !self.changeset.files.contains_key(&c.file)
        } else {
            !self.repo.join(&c.file).exists()
        }
    }

    fn clamp_list_cursor(&mut self) {
        if self.list_cursor >= self.store.len() {
            self.list_cursor = self.store.len().saturating_sub(1);
        }
    }
}

/// Each card's `(row, store index)`: under the last row its comment covers.
fn cards_of(rows: &[(usize, Vec<usize>)]) -> Vec<(usize, usize)> {
    rows.iter().filter_map(|(ci, r)| r.last().map(|&last| (last, *ci))).collect()
}

/// Step `cur` by `delta` within `0..n`, clamping at both ends.
fn step(cur: usize, delta: isize, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let max = n - 1;
    if delta >= 0 {
        (cur + delta as usize).min(max)
    } else {
        cur.saturating_sub(delta.unsigned_abs())
    }
}

/// The minimal scroll that fits `cursor`'s row in the viewport, by display heights.
fn keep_in_view(cursor: usize, scroll: usize, heights: &[usize], viewport: usize) -> usize {
    if viewport == 0 || heights.is_empty() {
        return 0;
    }
    let cursor = cursor.min(heights.len() - 1);
    let mut top = scroll.min(cursor);
    while top < cursor && heights[top..=cursor].iter().sum::<usize>() > viewport {
        top += 1;
    }
    while top > 0 && heights[top - 1..].iter().sum::<usize>() <= viewport {
        top -= 1;
    }
    top
}

/// Clamp a scroll so the window shows no blank tail.
fn bound(scroll: usize, total: usize, viewport: usize) -> usize {
    scroll.min(total.saturating_sub(viewport))
}

/// The start of the logical line (after the previous `\n`, or 0) containing char `caret`.
fn line_start(v: &[char], caret: usize) -> usize {
    v[..caret].iter().rposition(|&c| c == '\n').map_or(0, |p| p + 1)
}

/// The end of the logical line (the next `\n`, or the end) containing char `caret`.
fn line_end(v: &[char], caret: usize) -> usize {
    v[caret..].iter().position(|&c| c == '\n').map_or(v.len(), |p| caret + p)
}

/// The start of the word before `caret`: skip trailing whitespace, then the word run.
fn word_start(v: &[char], caret: usize) -> usize {
    let mut i = caret;
    while i > 0 && v[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !v[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

/// The end of the word after `caret`: skip leading whitespace, then the word run.
fn word_end(v: &[char], caret: usize) -> usize {
    let mut i = caret;
    while i < v.len() && v[i].is_whitespace() {
        i += 1;
    }
    while i < v.len() && !v[i].is_whitespace() {
        i += 1;
    }
    i
}

/// Move a scroll by `delta`, saturating at 0; `bound` caps it per frame.
fn offset_by(scroll: usize, delta: isize) -> usize {
    if delta >= 0 {
        scroll.saturating_add(delta.unsigned_abs())
    } else {
        scroll.saturating_sub(delta.unsigned_abs())
    }
}

/// One scroll step under `max`, clamping first so a stale scroll yields at once.
fn clamp_scroll(base: usize, delta: isize, max: usize) -> usize {
    base.min(max).saturating_add_signed(delta).min(max)
}

/// Put change marks on fresh rendered rows: bars, hidden counts, and a row per marker.
fn mark_rendered(
    rows: &mut Vec<Row>,
    doc: &crate::markdown::Rendered,
    marks: &MarkMap,
    open: &[String],
) {
    use crate::marks::Place;
    if marks.blocks.is_empty() && marks.markers.is_empty() {
        return;
    }
    let index = RenderedIndex::build(rows);
    let collapsed = |line: u32| {
        doc.meta
            .get(line as usize)
            .and_then(|m| m.details.as_ref())
            .is_some_and(|d| !open.iter().any(|k| *k == *d.key))
    };
    for b in &marks.blocks {
        let Some(u) = index.get(Unit::Block(b.src)) else { continue };
        for (i, row) in rows[u.lead..u.end].iter_mut().enumerate() {
            if let Row::Rendered { kind: RenderedKind::Block { bar, hides, line, .. }, .. } = row {
                *bar = Some(b.bar);
                if i == 0 && collapsed(*line) {
                    *hides = Some(b.lines);
                }
            }
        }
    }
    // Each marker's slot: after its block, else ahead of the first unit at or past it.
    let mut slots: Vec<(usize, &crate::marks::Marker)> = marks
        .markers
        .iter()
        .map(|m| {
            let after = match m.place {
                Place::After(block) => index.get(Unit::Block(block)).map(|u| u.end),
                Place::At | Place::Gone => None,
            };
            let at = after.unwrap_or_else(|| {
                let units = index.units();
                let k = units.partition_point(|u| u.src < m.src);
                units.get(k).map_or(rows.len(), |u| u.start)
            });
            (at, m)
        })
        .collect();
    slots.sort_by_key(|&(at, m)| (at, m.unit()));
    let marker_row = |m: &crate::marks::Marker| Row::Rendered {
        src: m.src,
        src_end: m.src_end,
        text: String::new(),
        kind: RenderedKind::Marker { kind: m.kind, lines: m.lines, gone: m.place == Place::Gone },
    };
    let mut out = Vec::with_capacity(rows.len() + slots.len());
    let mut next = slots.into_iter().peekable();
    for (i, row) in std::mem::take(rows).into_iter().enumerate() {
        while let Some((_, m)) = next.next_if(|&(at, _)| at <= i) {
            out.push(marker_row(m));
        }
        out.push(row);
    }
    out.extend(next.map(|(_, m)| marker_row(m)));
    *rows = out;
}

/// Whether `row` is a changed line, source or rendered.
fn is_change(row: &Row) -> bool {
    match row {
        Row::Deletion { .. }
        | Row::Insertion { .. }
        | Row::Rendered { kind: RenderedKind::Marker { .. }, .. } => true,
        Row::Rendered { kind: RenderedKind::Block { bar, .. }, .. } => bar.is_some(),
        Row::Context { .. } | Row::Fold { .. } => false,
    }
}

/// The nearest hunk start past `from` that way, or from the far end when `None`.
fn hunk_row(rows: &[Row], from: Option<usize>, forward: bool) -> Option<usize> {
    let joins = |a: &Row, b: &Row| match (a, b) {
        (
            Row::Rendered { kind: RenderedKind::Block { bar: Some(_), .. }, .. },
            Row::Rendered { kind: RenderedKind::Block { .. }, .. },
        ) => true,
        (Row::Rendered { .. }, _) | (_, Row::Rendered { .. }) => false,
        _ => is_change(a),
    };
    let starts_hunk =
        |&i: &usize| is_change(&rows[i]) && (i == 0 || !joins(&rows[i - 1], &rows[i]));
    if forward {
        (from.map_or(0, |i| i + 1)..rows.len()).find(starts_hunk)
    } else {
        (0..from.unwrap_or(rows.len()).min(rows.len())).rev().find(starts_hunk)
    }
}

/// Whether `path` is `.md` or `.markdown`, any case.
fn is_markdown_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// `path`'s worktree text, regular files only: over the budget, the notice, never read.
fn worktree_content(repo: &std::path::Path, path: &str) -> Result<String, crate::diff::Notice> {
    use std::io::{ErrorKind, Read};
    let at = repo.join(path);
    match std::fs::symlink_metadata(&at) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return std::fs::read_link(&at)
                .ok()
                .and_then(|target| target.into_os_string().into_string().ok())
                .ok_or(crate::diff::Notice::Unreadable);
        }
        Ok(_) => {}
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(String::new());
        }
        Err(_) => return Err(crate::diff::Notice::Unreadable),
    }
    let meta = match std::fs::metadata(&at) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Ok(String::new()),
        // Gone, or a parent turned into a file (which Windows also reports as not found).
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(String::new());
        }
        Err(_) => return Err(crate::diff::Notice::Unreadable),
    };
    if crate::diff::over_byte_budget(usize::try_from(meta.len()).unwrap_or(usize::MAX)) {
        return Err(crate::diff::Notice::TooLarge);
    }
    // Capped, so a file grown since its stat still stops past the budget.
    let mut bytes = Vec::new();
    let cap = crate::diff::MAX_BYTES as u64 + 1;
    match std::fs::File::open(at).and_then(|f| f.take(cap).read_to_end(&mut bytes)) {
        Ok(_) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(_) => Err(crate::diff::Notice::Unreadable),
    }
}

/// The current line row `i` stands for, else a fold's first, the next below, the next above.
fn source_line_at(rows: &[Row], i: usize) -> Option<u32> {
    let line_of = |r: &Row| match r {
        Row::Fold { lines } => lines.first().and_then(Row::new_no),
        _ => r.new_no(),
    };
    let at = i.min(rows.len());
    rows[at..].iter().find_map(line_of).or_else(|| rows[..at].iter().rev().find_map(line_of))
}

/// The row holding `line` by `side`: its own, the fold hiding it, the next, or the last numbered.
fn line_row(rows: &[Row], line: u32, side: fn(&Row) -> Option<u32>) -> usize {
    // A row's lines, a collapsed fold's hidden ones included.
    let any_line = |r: &Row, pred: &dyn Fn(Option<u32>) -> bool| {
        crate::marks::diff_lines(std::slice::from_ref(r)).any(|l| pred(side(l)))
    };
    let numbered = |r: &Row| any_line(r, &|n| n.is_some());
    let holds = |r: &Row| any_line(r, &|n| n == Some(line));
    rows.iter()
        .position(holds)
        .or_else(|| rows.iter().position(|r| side(r).is_some_and(|n| n > line)))
        .or_else(|| rows.iter().rposition(numbered))
        .unwrap_or(rows.len().saturating_sub(1))
}

/// Old → new line numbers across one edit; a changed line maps to its nearest survivor.
struct LineMap {
    ops: Vec<similar::DiffOp>,
    new_len: usize,
}

impl LineMap {
    fn new(old: &str, new: &str) -> Self {
        let (old, new) = (crate::text::lines(old), crate::text::lines(new));
        let ops = similar::TextDiff::from_slices(&old, &new).ops().to_vec();
        Self { ops, new_len: new.len() }
    }

    /// Map 1-based old line `line` to its 1-based new line.
    fn line(&self, line: u32) -> u32 {
        let i = line.saturating_sub(1) as usize;
        let mapped = self.ops.iter().find_map(|op| {
            let (tag, old, new) = op.as_tag_tuple();
            old.contains(&i).then(|| match tag {
                similar::DiffTag::Equal => new.start + (i - old.start),
                _ => new.start + (i - old.start).min(new.len().saturating_sub(1)),
            })
        });
        let mapped = mapped.unwrap_or(self.new_len).min(self.new_len.saturating_sub(1));
        mapped as u32 + 1
    }
}

/// Whether source row `row` lies in `c`'s range on its side.
fn line_in(c: &Comment, row: &Row) -> bool {
    let no = match c.side {
        Side::New => row.new_no(),
        Side::Old => row.old_no(),
    };
    no.is_some_and(|n| c.start <= n && n <= c.end)
}

/// The `(side, start, end, snippet)` of diff rows: new side unless all deletions.
fn anchor(selected: &[Row]) -> Option<(Side, u32, u32, String)> {
    let mut new: Option<(u32, u32)> = None;
    let mut old: Option<(u32, u32)> = None;
    let mut snippet = String::new();
    for row in selected.iter().filter(|row| row.is_content()) {
        if !snippet.is_empty() {
            snippet.push('\n');
        }
        snippet.push_str(&row.marker_text());
        if let Some(line) = row.new_no() {
            new = Some(new.map_or((line, line), |(min, max)| (min.min(line), max.max(line))));
        }
        if let Some(line) = row.old_no() {
            old = Some(old.map_or((line, line), |(min, max)| (min.min(line), max.max(line))));
        }
    }
    let (side, (start, end)) =
        new.map(|range| (Side::New, range)).or_else(|| old.map(|range| (Side::Old, range)))?;
    Some((side, start, end, snippet))
}

#[cfg(test)]
mod tests {
    use super::{App, FileReviewState, Mode};
    use crate::config::NavigatorPosition;
    use crate::model::{Comment, CommitPick, Scope, Side};
    use crate::test_support::test_repo;
    use crate::world::{PickStatus, PickVerdict};
    use std::path::PathBuf;

    #[test]
    fn a_pr_settled_highlight_survives_a_kept_snapshot_and_blanks_on_a_replace() {
        use crate::selection::{Point, Surface, TextDrag};
        let mut app = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        app.apply_pr(crate::forge::PrView::NoPr);
        let drag = TextDrag {
            surface: Surface::PrNav,
            anchor: Point { row: 0, chr: 0 },
            extent: Point { row: 0, chr: 3 },
        };
        app.settle_selection(drag, "text".into());

        // A kept paint keeps the highlight; a replaced or emptied one blanks it.
        app.apply_pr(crate::forge::PrView::Error(crate::git::Forge::GitHub, "offline".into()));
        assert!(app.settled_selection().is_some(), "a kept paint keeps the highlight");
        app.apply_pr(crate::forge::PrView::NoPr);
        assert!(app.settled_selection().is_none(), "a replaced paint blanks the highlight");
        app.settle_selection(drag, "text".into());
        app.clear_pr();
        assert!(app.settled_selection().is_none(), "an emptied tab blanks the highlight");
    }

    #[test]
    fn the_read_pane_scroll_stops_at_the_bottom_edge() {
        let mut app = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        app.note_pr_read_max_scroll(4);
        app.pr_scroll_read(100);
        assert_eq!(app.pr_read_scroll, 4, "scroll stops with the last line at the pane edge");
        app.pr_scroll_read(-1);
        assert_eq!(app.pr_read_scroll, 3, "no dead zone above the clamp");
        app.note_pr_read_max_scroll(0);
        app.pr_scroll_read(5);
        assert_eq!(app.pr_read_scroll, 0, "content that fits the pane does not scroll");
    }

    #[test]
    fn a_line_map_carries_each_line_through_every_kind_of_edit() {
        use super::LineMap;
        // (old, new, old line, its new line)
        let cases = [
            ("a\nb\nc\n", "x\na\nb\nc\n", 2, 3), // equal, shifted by an insert above
            ("a\nb\nc\n", "a\nB\nc\n", 2, 2),    // replaced: its counterpart
            ("a\nb\nc\nd\n", "a\nd\n", 3, 2),    // deleted: the line after
            ("a\nb\n", "a\nb\n", 9, 2),          // past the end: the last line
            ("a\nb\n", "", 1, 1),                // an emptied file: line 1
            ("a\rb\nc\n", "a\rb\nc\n", 9, 2),    // a bare CR breaks no line
        ];
        for (old, new, line, want) in cases {
            assert_eq!(LineMap::new(old, new).line(line), want, "{old:?} → {new:?} at {line}");
        }
    }

    #[test]
    fn config_recovery_carries_the_rendered_choice_and_open_details() {
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.mode = Mode::List;
        old.markdown_rendered = true; // flipped to rendered, away from the default
        old.rendered.content =
            Some(crate::rendered::Content { text: "# doc".to_string(), old: None, nothing: false });
        old.rendered.details.insert("Details#0".to_string(), true);

        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.carry_authored_state_from(&mut old);

        assert!(recovered.markdown_rendered, "the pane's choice survives a modal's recovery");
        assert_eq!(recovered.rendered.text(), Some("# doc"));
        assert_eq!(
            recovered.rendered.details.get("Details#0"),
            Some(&true),
            "open details survive"
        );
    }

    #[test]
    fn config_recovery_to_another_theme_repaints_rendered_rows() {
        // Recovery into another theme must repaint carried rendered rows.
        let colors = |a: &App| -> Vec<ratatui::style::Color> {
            a.rendered_lines()
                .iter()
                .flat_map(|l| l.spans.iter().filter_map(|s| s.style.fg))
                .collect()
        };
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.markdown_rendered = true;
        old.rendered.content = Some(crate::rendered::Content {
            text: "# Heading\n\nbody\n".into(),
            old: None,
            nothing: false,
        });
        old.rebuild_visible();
        old.mode = Mode::List;
        let before = colors(&old);

        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.set_cli_theme(Some("dracula".to_string()));
        recovered.carry_authored_state_from(&mut old);
        assert_eq!(colors(&recovered), before, "the list freezes the view under it");
        recovered.mode = Mode::Normal;
        recovered.sync_rendered_width(80); // the next frame
        assert!(matches!(recovered.visible.first(), Some(crate::diff::Row::Rendered { .. })));
        assert_ne!(colors(&recovered), before, "the rows repaint in the recovered theme");
    }

    #[test]
    fn config_recovery_carries_the_last_sent_agent() {
        // `last used` survives a config error.
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.last_sent_pane = Some("w8:p2".to_string());
        old.mode = Mode::Picker;

        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.carry_authored_state_from(&mut old);
        assert_eq!(recovered.last_sent_pane.as_deref(), Some("w8:p2"));
        // A picker that was open when the config broke does not come back with it.
        assert_eq!(recovered.mode, Mode::Normal);
    }

    #[test]
    fn config_recovery_carries_reviews_then_reconciles_the_loaded_context() {
        let context = crate::model::ReviewContext::Uncommitted;
        let identity = |new_content| {
            crate::model::FileIdentity::from_git(crate::model::FileIdentityInput {
                old_endpoint: "old",
                new_endpoint: "worktree",
                kind: crate::model::ChangeKind::Modified,
                path: "fixture.rs",
                previous_path: None,
                old_mode: "100644",
                new_mode: "100644",
                old_content: "old-content",
                new_content,
                binary: false,
                live_new_side: true,
            })
        };
        let annotation = |path: &str, identity| crate::model::ChangedFile {
            path: path.to_string(),
            kind: crate::model::ChangeKind::Modified,
            additions: 1,
            deletions: 1,
            previous_path: None,
            binary: false,
            old_size: 1,
            new_size: None,
            identity,
        };
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.review_context = Some(context.clone());
        old.changeset.files.insert("kept.rs".to_string(), annotation("kept.rs", identity("kept")));
        old.changeset
            .files
            .insert("changed.rs".to_string(), annotation("changed.rs", identity("reviewed")));
        old.changeset.files.insert("gone.rs".to_string(), annotation("gone.rs", identity("gone")));
        assert!(old.set_file_reviewed("kept.rs", true));
        assert!(old.set_file_reviewed("changed.rs", true));
        assert!(old.set_file_reviewed("gone.rs", true));

        // Recovery has already loaded a fresh snapshot before authored state transfers. The
        // changed review becomes stale, while the disappeared path is forgotten.
        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.review_context = Some(context);
        recovered
            .changeset
            .files
            .insert("kept.rs".to_string(), annotation("kept.rs", identity("kept")));
        recovered
            .changeset
            .files
            .insert("changed.rs".to_string(), annotation("changed.rs", identity("new")));
        recovered.carry_authored_state_from(&mut old);

        assert!(recovered.file_reviewed("kept.rs"), "an exact review survives recovery");
        assert_eq!(
            recovered.file_review_state("changed.rs"),
            FileReviewState::ReviewedButChanged,
            "a changed identity is retained as stale",
        );
        assert!(!recovered.file_reviewed("gone.rs"), "the recovered loaded context prunes it");
    }

    #[test]
    fn config_recovery_reconciles_reviews_against_the_carried_modal_frame() {
        let context = crate::model::ReviewContext::Uncommitted;
        let identity = |new_content| {
            crate::model::FileIdentity::from_git(crate::model::FileIdentityInput {
                old_endpoint: "old",
                new_endpoint: "worktree",
                kind: crate::model::ChangeKind::Modified,
                path: "fixture.rs",
                previous_path: None,
                old_mode: "100644",
                new_mode: "100644",
                old_content: "old-content",
                new_content,
                binary: false,
                live_new_side: true,
            })
        };
        let annotation = |identity| crate::model::ChangedFile {
            path: "fixture.rs".to_string(),
            kind: crate::model::ChangeKind::Modified,
            additions: 1,
            deletions: 1,
            previous_path: None,
            binary: false,
            old_size: 1,
            new_size: None,
            identity,
        };

        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.mode = Mode::Composing { editing: None };
        old.review_context = Some(context.clone());
        old.changeset.files.insert("fixture.rs".to_string(), annotation(identity("frozen")));
        assert!(old.set_file_reviewed("fixture.rs", true));

        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.review_context = Some(context);
        recovered.changeset.files.insert("fixture.rs".to_string(), annotation(identity("fresh")));
        recovered.carry_authored_state_from(&mut old);

        assert_eq!(recovered.mode, Mode::Composing { editing: None });
        assert!(recovered.file_reviewed("fixture.rs"), "the frozen modal frame stays reviewed");
    }

    #[test]
    fn config_recovery_carries_the_base_picker_whole() {
        // The base picker survives recovery with its rows, filter, and highlight
        let mut old = App::blocked(PathBuf::from("."), Scope::Branch, None);
        old.mode = Mode::BasePick;
        old.base_picker = Some(super::BasePicker {
            rows: vec![super::BaseChoice::Branch {
                name: "dev".to_string(),
                pr_base: false,
                is_default: false,
                current: false,
                tip_secs: 0,
            }],
            cursor: 0,
            query: "d".to_string(),
            caret: 1,
            probe: super::BaseProbe::Idle,
        });
        old.branch_base = crate::git::BaseStatus {
            winner: Some(crate::git::ResolvedBase::Branch {
                name: "main".to_string(),
                oid: "0".repeat(40),
            }),
            skipped: None,
        };

        let mut recovered = App::new(PathBuf::from("."), Scope::Branch, None);
        recovered.carry_authored_state_from(&mut old);
        assert_eq!(recovered.mode, Mode::BasePick);
        let bp = recovered.base_picker.as_ref().expect("the picker state is carried");
        assert_eq!(bp.query, "d");
        assert_eq!(bp.rows[0].name(), "dev");
        // The header's base carries with the frame.
        let base = recovered.branch_base.winner.as_ref().expect("the resolved base is carried");
        assert_eq!(base.name(), "main");
    }

    #[test]
    fn config_recovery_carries_the_commit_picker_with_its_highlight_and_anchor() {
        // The commit picker and the pick's verdict survive recovery.
        let row = |sha: &str| crate::git::CommitRow {
            sha: sha.repeat(40),
            subject: format!("commit {sha}"),
            time: 1,
            author: "Test".to_string(),
            refs: Vec::new(),
            merge: false,
        };
        let mut old = App::blocked(PathBuf::from("."), Scope::Commits, None);
        old.mode = Mode::CommitPick;
        old.commit_picker = Some(super::CommitPicker {
            rows: vec![row("a"), row("b"), row("c")],
            pick_row: None,
            cursor: 0,
            anchor: Some(2),
            title: "commits · last 50".to_string(),
            empty: "no commits yet".to_string(),
            head: None,
        });
        old.commit_pick = Some(CommitPick { oldest: "c".repeat(40), newest: "a".repeat(40) });
        old.pick_status = Some(PickStatus {
            verdict: PickVerdict::Live,
            subject: "commit a".to_string(),
            count: 3,
        });

        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.carry_authored_state_from(&mut old);
        assert_eq!(recovered.mode, Mode::CommitPick);
        assert_eq!(recovered.scope, Scope::Commits);
        let cp = recovered.commit_picker.as_ref().expect("the picker state is carried");
        assert_eq!((cp.cursor, cp.anchor), (0, Some(2)));
        assert_eq!(cp.rows.len(), 3);
        assert_eq!(
            recovered.commit_pick.as_ref().map(|p| p.newest.as_str()),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(recovered.pick_status.as_ref().map(|s| s.count), Some(3));

        // In `Normal` the pick still carries.
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.commit_pick = Some(CommitPick::single("d"));
        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.carry_authored_state_from(&mut old);
        assert_eq!(recovered.commit_pick, Some(CommitPick::single("d")));
    }

    #[test]
    fn config_recovery_carries_the_footer_expansion() {
        // The `?` expansion is one global toggle, carried whatever mode recovery finds.
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.keys_expanded = true;

        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        assert!(!recovered.keys_expanded, "a fresh app opens collapsed");
        recovered.carry_authored_state_from(&mut old);
        assert!(recovered.keys_expanded, "the expansion survives config recovery");
    }

    #[test]
    fn config_recovery_carries_saved_comments_and_the_live_draft() {
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.store.add(Comment {
            file: "src/lib.rs".to_string(),
            side: Side::New,
            start: 1,
            end: 1,
            lines: "+line".to_string(),
            text: "saved".to_string(),
            diff_anchored: true,
            rev: crate::model::Rev::Worktree,
        });
        old.mode = Mode::Composing { editing: None };
        old.resume_list = true;
        old.input = "draft".to_string();
        old.caret = 3;

        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.carry_authored_state_from(&mut old);

        assert_eq!(recovered.store.len(), 1);
        assert_eq!(recovered.input, "draft");
        assert_eq!(recovered.caret, 3);
        assert!(recovered.resume_list);
        assert!(matches!(recovered.mode, Mode::Composing { editing: None }));
    }

    #[test]
    fn config_recovery_keeps_the_comment_list_view_and_navigation() {
        let mut old = App::blocked(PathBuf::from("."), Scope::Branch, None);
        old.mode = Mode::List;
        old.file_cursor = 4;
        old.file_scroll = 2;
        old.diff_cursor = 8;
        old.diff_scroll = 5;
        old.input = "unsent".to_string();

        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.carry_authored_state_from(&mut old);

        assert!(matches!(recovered.mode, Mode::List));
        assert_eq!(recovered.scope, Scope::Branch);
        assert_eq!(recovered.file_cursor, 4);
        assert_eq!(recovered.file_scroll, 2);
        assert_eq!(recovered.diff_cursor, 8);
        assert_eq!(recovered.diff_scroll, 5);
        assert_eq!(recovered.input, "unsent");
    }

    #[test]
    fn config_recovery_carries_a_pending_world_request() {
        // A pending refresh survives recovery.
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.request_world_refresh(true);
        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.carry_authored_state_from(&mut old);
        let request = recovered.world_request.expect("the pending refresh survives the swap");
        assert!(request.reveal, "the switch's reveal flag survives the recovery swap");
    }

    #[test]
    fn config_recovery_keeps_both_shares_and_reapplies_the_configured_position() {
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.navigator_position = NavigatorPosition::Top;
        old.navigator_side_pct = 41;
        old.navigator_stack_pct = 37;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "navigator_position = \"left\"\n").unwrap();
        let config = crate::config::plugin_config_in(dir.path()).unwrap();
        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.set_plugin_config(config);
        recovered.carry_authored_state_from(&mut old);

        assert_eq!(recovered.navigator_position, NavigatorPosition::Left);
        assert_eq!(recovered.navigator_side_pct, 41);
        assert_eq!(recovered.navigator_stack_pct, 37);
    }

    #[test]
    fn config_recovery_keeps_the_hidden_navigator() {
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.navigator_hidden = true;

        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.carry_authored_state_from(&mut old);

        assert!(recovered.navigator_hidden, "the hidden state survives config recovery");
        assert_eq!(recovered.focus, crate::Focus::Diff, "and focus lands on the read pane");
    }

    #[test]
    fn blocked_app_rejects_normal_repository_work_without_panicking() {
        let mut app = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        app.set_config_error("bad config".to_string());

        assert!(app.reload().unwrap_err().to_string().contains("bad config"));
        assert!(app.set_scope(Scope::Branch).is_err());
        assert!(app.set_tab(super::Tab::AllFiles).is_err());
        assert!(app.move_cursor(1).is_err());
        assert!(app.select_file(0).is_err());
    }

    #[test]
    fn pr_bodies_open_their_disclosures_independently() {
        let mut app = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        let one = "first\n\n<details><summary>Details</summary>\n\nbody one\n\n</details>\n";
        let two = "second\n\n<details><summary>Details</summary>\n\nbody two\n\n</details>\n";
        app.tab = super::Tab::Pr;
        let key_of = |app: &App, text: &str, body| {
            app.pr_body_render(text, 80, body)
                .meta
                .iter()
                .find_map(|m| m.details.clone())
                .unwrap()
                .key
        };
        assert_ne!(key_of(&app, one, 0), key_of(&app, two, 1), "same summary, two bodies");
        let key = key_of(&app, one, 0);
        app.toggle_details(&key);
        let opened = |app: &App, text: &str, body| {
            app.pr_body_render(text, 80, body).lines.iter().any(|l| l.to_string().contains("body"))
        };
        assert!(opened(&app, one, 0));
        assert!(!opened(&app, two, 1), "the other body's disclosure stays closed");
        assert!(!opened(&app, one, 1), "identical text in another body is another body");
    }

    /// `src/lib.rs`: +10, −11, +12, the cursor on the deletion.
    fn edit_app() -> App {
        use crate::diff::{Row, Span};
        use crate::file_list::{Row as ListRow, RowKind};
        let mut app = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        app.entries.push(crate::file_list::Entry {
            path: "src/lib.rs".into(),
            annotation: None,
            ignored: false,
            is_dir: false,
        });
        app.entries.push(crate::file_list::Entry {
            path: "src/other.rs".into(),
            annotation: None,
            ignored: false,
            is_dir: false,
        });
        app.file_rows = vec![
            ListRow {
                depth: 1,
                name: "lib.rs".into(),
                kind: RowKind::File { index: 0 },
                ignored: false,
            },
            ListRow {
                depth: 1,
                name: "other.rs".into(),
                kind: RowKind::File { index: 1 },
                ignored: false,
            },
        ];
        app.diff_path = Some("src/lib.rs".into());
        app.focus = crate::Focus::Diff;
        let bare = Vec::<Span>::new;
        app.visible = vec![
            Row::Insertion { new_no: 10, spans: bare(), emphasis: Vec::new(), cr: false },
            Row::Deletion { old_no: 11, spans: bare(), emphasis: Vec::new(), cr: false },
            Row::Insertion { new_no: 12, spans: bare(), emphasis: Vec::new(), cr: false },
        ];
        app.diff_cursor = 1;
        app
    }

    /// Every state `edit` can be pressed in, and what it names; a missing row is undecided.
    #[test]
    fn edit_names_a_file_in_exactly_these_states() {
        use super::{EditTarget, FooterAction, Tab};
        use crate::file_list::RowKind;
        let target = |path: &str, line: u32| Some(EditTarget { path: path.into(), line });

        #[allow(clippy::type_complexity)]
        let cases: Vec<(&str, Box<dyn Fn(&mut App)>, Option<EditTarget>)> = vec![
            (
                "read pane, on an insertion",
                Box::new(|a: &mut App| a.diff_cursor = 2),
                target("src/lib.rs", 12),
            ),
            ("read pane, on a deletion", Box::new(|_: &mut App| {}), target("src/lib.rs", 10)),
            (
                "read pane, nothing numbered above",
                Box::new(|a: &mut App| {
                    a.visible.truncate(1);
                    a.visible[0] = crate::diff::Row::Deletion {
                        old_no: 3,
                        spans: Vec::new(),
                        emphasis: Vec::new(),
                        cr: false,
                    };
                    a.diff_cursor = 0;
                }),
                target("src/lib.rs", 1),
            ),
            (
                "read pane, a notice diff with no rows",
                Box::new(|a: &mut App| {
                    a.visible.clear();
                    a.diff_cursor = 0;
                }),
                target("src/lib.rs", 1),
            ),
            (
                "read pane, rendered markdown",
                Box::new(|a: &mut App| {
                    a.rendered.content = Some(crate::rendered::Content {
                        text: "intro\n\n# heading\n".into(),
                        old: None,
                        nothing: false,
                    });
                    a.markdown_rendered = true;
                    a.rebuild_visible();
                    a.diff_cursor = a.visible.len() - 1;
                }),
                // The rendered cursor names its block's first source line.
                target("src/lib.rs", 3),
            ),
            ("read pane, no file open", Box::new(|a: &mut App| a.diff_path = None), None),
            (
                "navigator, on a file row",
                Box::new(|a: &mut App| a.focus = crate::Focus::Files),
                target("src/lib.rs", 1),
            ),
            (
                "navigator, on a row that is not the open file",
                Box::new(|a: &mut App| {
                    a.focus = crate::Focus::Files;
                    a.file_cursor = 1;
                }),
                target("src/other.rs", 1),
            ),
            (
                "read pane, with the navigator cursor on another file",
                Box::new(|a: &mut App| a.file_cursor = 1),
                target("src/lib.rs", 10),
            ),
            (
                "navigator, on a directory row",
                Box::new(|a: &mut App| {
                    a.focus = crate::Focus::Files;
                    a.file_rows[0].kind =
                        RowKind::Dir { path: "src".into(), expanded: true, has_change: false };
                }),
                None,
            ),
            (
                "a live line selection on the diff",
                Box::new(|a: &mut App| a.select_anchor = Some(0)),
                None,
            ),
            (
                "a live line selection, from the navigator",
                Box::new(|a: &mut App| {
                    a.select_anchor = Some(0);
                    a.focus = crate::Focus::Files;
                }),
                target("src/lib.rs", 1),
            ),
            ("the comments list", Box::new(|a: &mut App| a.mode = Mode::List), None),
            ("the search screen", Box::new(|a: &mut App| a.mode = Mode::Search), None),
            ("the `PR` tab", Box::new(|a: &mut App| a.tab = Tab::Pr), None),
            ("the find band", Box::new(|a: &mut App| a.mode = Mode::Find), None),
            ("the agent picker", Box::new(|a: &mut App| a.mode = Mode::Picker), None),
            ("the base picker", Box::new(|a: &mut App| a.mode = Mode::BasePick), None),
        ];

        for (name, setup, expected) in cases {
            let mut app = edit_app();
            setup(&mut app);
            assert_eq!(app.edit_target(), expected, "{name}");
            // Offered exactly where it acts, exactly once.
            let bands = app.footer_bands();
            let offered = bands.iter().filter(|&&(a, _)| a == FooterAction::EditFile).count();
            let expected_offers = usize::from(expected.is_some() && !app.comment_claims_edit());
            assert_eq!(offered, expected_offers, "the footer disagrees with the press: {name}");
            // Ahead of the hide key, which trims first.
            let at = |want| bands.iter().position(|&(a, _)| a == want);
            if let (Some(edit), Some(hide)) =
                (at(FooterAction::EditFile), at(FooterAction::NavigatorHide))
            {
                assert!(edit < hide, "the hide key trims before the file: {name}");
            }
        }
    }

    #[test]
    fn a_commented_line_keeps_edit_for_its_comment() {
        use super::EditTarget;
        let mut app = edit_app();
        app.store.add(crate::model::Comment {
            file: "src/lib.rs".into(),
            side: Side::New,
            start: 12,
            end: 12,
            lines: "+x".into(),
            text: "note".into(),
            diff_anchored: true,
            rev: crate::model::Rev::Worktree,
        });
        app.diff_cursor = 2;
        app.start_edit();
        assert!(app.composing(), "the comment under the cursor claims the key");
        assert!(app.editor_request.is_none(), "and no file is requested");

        // One row up carries no comment, so the same key names the file instead.
        app.cancel_comment();
        app.diff_cursor = 0;
        app.start_edit();
        assert!(!app.composing(), "no comment covers this row");
        assert_eq!(
            app.editor_request,
            Some(EditTarget { path: "src/lib.rs".into(), line: 10 }),
            "so the same key names the file"
        );
    }

    #[test]
    fn rendered_markdown_over_a_commented_block_edits_the_comment() {
        // Rendered, the comment claims `edit` as on source; only an outcome can prove it.
        let mut app = edit_app();
        app.store.add(crate::model::Comment {
            file: "src/lib.rs".into(),
            side: Side::New,
            start: 1,
            end: 1,
            lines: "+x".into(),
            text: "note".into(),
            diff_anchored: true,
            rev: crate::model::Rev::Worktree,
        });
        app.rendered.content =
            Some(crate::rendered::Content { text: "# heading".into(), old: None, nothing: false });
        app.markdown_rendered = true;
        app.rebuild_visible();
        app.diff_cursor = 0;

        app.start_edit();
        assert!(app.composing(), "the card on screen claims the key");
        assert_eq!(app.editor_request, None, "so the file does not open");
        assert!(app.rendered.on_screen(), "the edit stays rendered");
    }

    #[test]
    fn a_live_selection_freezes_both_branches_of_edit() {
        let mut app = edit_app();
        app.store.add(crate::model::Comment {
            file: "src/lib.rs".into(),
            side: Side::New,
            start: 12,
            end: 12,
            lines: "+x".into(),
            text: "note".into(),
            diff_anchored: true,
            rev: crate::model::Rev::Worktree,
        });
        app.diff_cursor = 2;
        app.toggle_select();
        assert!(app.select_anchor.is_some(), "v starts a selection on a commented line");

        app.start_edit();
        assert!(!app.composing(), "the comment branch must not fire either");
        assert!(app.editor_request.is_none());
        assert!(app.select_anchor.is_some(), "and the range survives the press");

        // The comments list still opens its highlighted comment.
        app.open_list();
        app.start_edit();
        assert!(app.composing(), "the list edits its comment under a live range");
    }

    #[test]
    fn the_pr_tab_leaves_the_key_alone_entirely() {
        // Off screen, neither the file nor the comment is a target.
        let mut app = edit_app();
        app.store.add(crate::model::Comment {
            file: "src/lib.rs".into(),
            side: Side::New,
            start: 12,
            end: 12,
            lines: "+x".into(),
            text: "note".into(),
            diff_anchored: true,
            rev: crate::model::Rev::Worktree,
        });
        app.diff_cursor = 2;
        app.tab = super::Tab::Pr;

        app.start_edit();
        assert!(!app.composing(), "the comment behind the tab does not claim the key");
        assert!(app.editor_request.is_none(), "and no file is named");
    }

    #[test]
    fn edit_from_the_navigator_ignores_a_comment_on_the_hidden_diff_cursor() {
        let mut app = edit_app();
        app.store.add(crate::model::Comment {
            file: "src/lib.rs".into(),
            side: Side::New,
            start: 10,
            end: 10,
            lines: "+x".into(),
            text: "note".into(),
            diff_anchored: true,
            rev: crate::model::Rev::Worktree,
        });
        app.diff_cursor = 0;
        app.focus = crate::Focus::Files;
        app.start_edit();
        assert!(!app.composing(), "the card is off screen, so it does not claim the key");
        assert!(app.editor_request.is_some(), "the file row under the eye wins");
    }

    /// Git commands the current thread has built so far.
    fn git_commands() -> usize {
        crate::git::GIT_COMMANDS.with(std::cell::Cell::get)
    }

    #[test]
    fn an_over_budget_file_opens_as_its_notice_without_reading_in_any_scope() {
        let (dir, git) = test_repo();
        let repo = dir.path();
        // A `diff` attribute forces text, which git's own size threshold would not override.
        std::fs::write(repo.join(".gitattributes"), "big.txt diff\n").unwrap();
        std::fs::write(repo.join("big.txt"), "x\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        let big = "y\n".repeat(crate::diff::MAX_BYTES / 2 + 1);
        std::fs::write(repo.join("big.txt"), &big).unwrap();
        let open = |app: &mut App| {
            app.reload().unwrap();
            let before = git_commands();
            app.set_diff("big.txt".to_string());
            assert_eq!(git_commands() - before, 0, "the oversize file was read through git");
            assert_eq!(app.diff.notice, Some(crate::diff::Notice::TooLarge));
        };

        // The worktree side, in `uncommitted`.
        let mut app = App::new(repo.to_path_buf(), Scope::Uncommitted, None);
        open(&mut app);
        // A committed side, with the worktree shrunk.
        git(&["commit", "-q", "-am", "grow"]);
        std::fs::write(repo.join("big.txt"), "small\n").unwrap();
        open(&mut app);
        // A run of commits: the big side old, then new.
        git(&["commit", "-q", "-am", "shrink"]);
        app.commit_pick = Some(CommitPick::single(&git(&["rev-parse", "HEAD"])));
        app.scope = Scope::Commits;
        open(&mut app);
        app.commit_pick = Some(CommitPick::single(&git(&["rev-parse", "HEAD~1"])));
        open(&mut app);
        // `last-turn`, whose new side is the snapshot the build took.
        let baseline = git(&["rev-parse", "HEAD^{tree}"]);
        std::fs::write(repo.join("big.txt"), &big).unwrap();
        app.sync_turn(crate::turn::TurnReport {
            last: crate::turn::LastTurn::At(baseline),
            ..Default::default()
        });
        app.set_scope(Scope::LastTurn).unwrap();
        open(&mut app);
        // A stale row's path has no diff at the landed ends.
        app.reload().unwrap();
        let before = git_commands();
        app.set_diff("gone.txt".to_string());
        assert_eq!(git_commands() - before, 0, "a path outside the changeset was read");
    }

    /// A failed `git diff` shows a notice, never an empty diff that reads as unchanged.
    #[test]
    fn a_failed_read_of_a_changed_file_is_a_notice() {
        let (dir, git) = test_repo();
        let repo = dir.path();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();

        let mut app = App::new(repo.to_path_buf(), Scope::Uncommitted, None);
        app.reload().unwrap();
        // The landed ends name a commit git no longer has.
        app.changeset.ends = Some(crate::world::DiffEnds { old: "0".repeat(40), new: None });
        app.set_diff("a.txt".to_string());
        assert_eq!(app.diff.notice, Some(crate::diff::Notice::Unreadable));
        assert!(app.diff.rows.is_empty());
        assert!(app.diff.identity.is_none(), "an unread comparison cannot be marked reviewed");
        app.toggle_current_file_reviewed();
        assert!(!app.file_reviewed("a.txt"));
    }

    /// An untracked file the disk refuses to read or stat is the notice, never an empty addition.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_untracked_file_is_a_notice() {
        use std::os::unix::fs::PermissionsExt;
        // The file itself unreadable, then its directory unsearchable.
        for (path, locked, mode) in [("locked.txt", "locked.txt", 0o000), ("d/f.txt", "d", 0o600)] {
            let (dir, git) = test_repo();
            let repo = dir.path();
            std::fs::write(repo.join("seed.txt"), "x\n").unwrap();
            git(&["add", "-A"]);
            git(&["commit", "-q", "-m", "init"]);
            std::fs::create_dir_all(repo.join("d")).unwrap();
            std::fs::write(repo.join(path), "secret\n").unwrap();

            let mut app = App::new(repo.to_path_buf(), Scope::Uncommitted, None);
            app.reload().unwrap();
            let locked = repo.join(locked);
            let restore = std::fs::metadata(&locked).unwrap().permissions();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(mode)).unwrap();
            app.set_diff(path.to_string());
            std::fs::set_permissions(&locked, restore).unwrap();
            assert_eq!(app.diff.notice, Some(crate::diff::Notice::Unreadable), "{path}");
            assert!(app.diff.identity.is_none(), "{path}");
            app.toggle_current_file_reviewed();
            assert!(!app.file_reviewed(path), "{path}");
        }
    }

    /// An over-budget rename's notice keeps its source and its Diff view.
    #[test]
    fn an_over_budget_rename_keeps_its_source_and_its_view() {
        let (dir, git) = test_repo();
        let repo = dir.path();
        std::fs::write(repo.join("big.txt"), "y\n".repeat(crate::diff::MAX_BYTES / 2 + 1)).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        git(&["mv", "big.txt", "moved.txt"]);

        let mut app = App::new(repo.to_path_buf(), Scope::Uncommitted, None);
        app.reload().unwrap();
        app.set_diff("moved.txt".to_string());
        assert_eq!(app.diff.notice, Some(crate::diff::Notice::TooLarge));
        assert_eq!(app.diff.previous_path.as_deref(), Some("big.txt"));
        assert_eq!(app.diff.view, crate::diff::View::Diff);
    }

    /// A tracked link diffs as its target path, never the too-large notice.
    #[cfg(unix)]
    #[test]
    fn a_tracked_link_to_a_large_file_diffs_as_its_target_path() {
        let (dir, git) = test_repo();
        let repo = dir.path();
        let elsewhere = tempfile::tempdir().unwrap();
        let big = elsewhere.path().join("big");
        std::fs::write(&big, "y\n".repeat(crate::diff::MAX_BYTES)).unwrap();
        std::os::unix::fs::symlink("nowhere", repo.join("link")).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::remove_file(repo.join("link")).unwrap();
        std::os::unix::fs::symlink(&big, repo.join("link")).unwrap();

        let mut app = App::new(repo.to_path_buf(), Scope::Uncommitted, None);
        app.reload().unwrap();
        app.set_diff("link".to_string());
        assert_eq!(app.diff.notice, None);
        assert!(app.diff.rows.iter().any(|r| r.marker() == '+'));
    }

    #[test]
    fn a_file_build_spends_one_git_diff_in_every_scope() {
        let (dir, git) = test_repo();
        let repo = dir.path();
        git(&["config", "core.autocrlf", "true"]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("same.txt"), "same\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        let base = git(&["rev-parse", "HEAD"]);
        git(&["update-ref", "refs/remotes/origin/main", &base]);
        git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);
        let baseline = git(&["rev-parse", "HEAD^{tree}"]);
        git(&["mv", "same.txt", "moved.txt"]);
        std::fs::write(repo.join("a.txt"), "one\r\nTWO\r\n").unwrap();
        std::fs::write(repo.join("new.txt"), "fresh\n").unwrap();

        let mut app = App::new(repo.to_path_buf(), Scope::Uncommitted, None);
        let cost = |app: &mut App, path: &str| {
            let before = git_commands();
            app.set_diff(path.to_string());
            git_commands() - before
        };
        app.reload().unwrap();
        let uncommitted =
            (cost(&mut app, "a.txt"), cost(&mut app, "new.txt"), cost(&mut app, "moved.txt"));
        app.set_scope(Scope::Branch).unwrap();
        let branch = cost(&mut app, "a.txt");
        app.sync_turn(crate::turn::TurnReport {
            last: crate::turn::LastTurn::At(baseline),
            ..Default::default()
        });
        app.set_scope(Scope::LastTurn).unwrap();
        let last_turn = cost(&mut app, "a.txt");
        app.commit_pick = Some(CommitPick::single(&base));
        app.scope = Scope::Commits;
        app.reload().unwrap();
        let commits = cost(&mut app, "a.txt");
        // One `git diff` per tracked file, a rename's source included.
        assert_eq!(uncommitted, (1, 0, 1), "a CRLF edit, an untracked file, a pure rename");
        assert_eq!((branch, last_turn, commits), (1, 1, 1));
        let rows: Vec<String> = app
            .diff
            .rows
            .iter()
            .filter(|r| r.marker() != ' ')
            .map(crate::diff::Row::marker_text)
            .collect();
        assert_eq!(rows, ["+one", "+two"], "the root commit adds the file, read in that one diff");
    }
}
