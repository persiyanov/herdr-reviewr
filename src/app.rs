//! Application state and transitions for the Changes review TUI.
//!
//! This module is terminal-free:
//! every method is a pure state transition or a read-only git/export call, so the
//! whole interaction model is testable without a backend. `src/main.rs` owns the
//! terminal and maps input events onto these methods.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::diff::{DiffCache, FileDiff, RenderedKind, Row, View};
use crate::export::{Agent, ExportTarget, format_all};
use crate::file_list::{self, Annotation, Entry, RowKind};
use crate::forge;
use crate::git;
use crate::herdr::{self, AgentChoice, SendTarget};
use crate::highlight::Highlighter;
use crate::logln;
use crate::marks::{MarkMap, Unit, diff_lines};
use crate::model::{Comment, CommentStore, CommitPick, Rev, Scope, Side};
use crate::rendered::{Built, Content, OldMap, RenderedIndex, RenderedInput, RenderedView, RowId};
use crate::roles::Palette;
use crate::theme;
use crate::world::{PickStatus, PickVerdict};

/// Navigator shares and bounds, as percentages of the body's split axis.
const DEFAULT_SIDE_PCT: u16 = 32;
const DEFAULT_STACK_PCT: u16 = 25;
const MIN_NAVIGATOR_PCT: u16 = 15;
const MAX_SIDE_PCT: u16 = 60;
const MAX_STACK_PCT: u16 = 50;
/// Pause after the last filter edit before probing an empty list as a rev
const BASE_PROBE_DELAY: Duration = Duration::from_millis(150);
/// The search screen's results-pane share: half the body by default, dragged within
/// wide bounds — the geometry's minimum pane sizes clamp the rest.
const DEFAULT_SEARCH_PCT: u16 = 50;
const MIN_SEARCH_PCT: u16 = 10;
const MAX_SEARCH_PCT: u16 = 90;
/// The wrap width rendered markdown builds at before any frame has noted the pane's — the
/// first frame rebuilds at the real width, carrying the place.
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

/// What the file-list cursor points at, by path, so it can be restored to the same target
/// after the tree rebuilds on a poll.
enum Anchor {
    File(String),
    Dir(String),
}

/// Which top-level tab is active: the changes reviewer, the whole-repo browser, or the
/// read-only PR mirror.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Changes,
    AllFiles,
    Pr,
}

/// What a pending PR refresh may do to a fetch already in flight: an ambient trigger —
/// tab entry, a turn end, the fallback timer — rides it, the user's `refresh` key
/// supersedes it. `Ord` so merging pending requests keeps the
/// stronger kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefreshKind {
    Ambient,
    Forced,
}

impl Tab {
    /// Whether this tab uses the file-tree / diff machinery (and so the per-tab stash). The
    /// `PR` tab does not — it holds its own state and never swaps into the diff fields.
    pub(crate) fn is_file_tab(self) -> bool {
        matches!(self, Tab::Changes | Tab::AllFiles)
    }
}

/// The inactive tab's saved navigator and read-pane state, swapped in on a tab switch so
/// each tab keeps its own selection and scroll.
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
    /// Whether this tab has ever completed a reload. A never-visited tab has nothing worth
    /// painting, so its first entry loads before the frame instead of deferring.
    visited: bool,
}

/// A file crossing offered by the footer, waiting for the hunk step that armed it to repeat: the
/// direction it crosses in, and the file it resolved to open. Holding the file spares the second
/// press the walk the first one already paid for.
#[derive(Clone, Debug)]
struct ArmedCross {
    forward: bool,
    path: String,
}

/// The base picker's state while it is open. The rows freeze
/// at open; the filter and highlight are the reviewer's own place state.
#[derive(Clone, Debug)]
pub struct BasePicker {
    /// Pickable rows: every branch (the PR's target first, the default next, the rest by
    /// tip recency) plus a current non-branch spelling so the highlight can open on it.
    pub rows: Vec<BaseChoice>,
    /// The highlighted row, an index into the visible view.
    pub cursor: usize,
    /// The typed filter, matching anywhere in the name.
    pub query: String,
    /// The caret in `query`, a char index — the filter edits with the comment editor's
    /// controls, like every other text field.
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

/// One base picker row. A branch carries the facts its trail paints: the open PR's
/// target (`pr base`), the repo's default (`default`), checked out here (`current`), and
/// its tip's commit time, painted as an age at render like the commit picker's rows so
/// it never goes stale while the picker sits open (`0` when unknown: no age). A typed
/// revision carries its oid instead.
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
    /// Frozen rows the query fuzzily matches, best score first, by the matcher the search
    /// screen uses (`neo_frizbee`, fff's engine). Ties keep the frozen order, so an empty
    /// query is the list as opened.
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

    /// Whether a frozen row spells the query, case aside: then no revision probe runs,
    /// since Enter would pick that row. Case is ignored because the fuzzy filter already
    /// shows `main` for `MAIN`, and a probe of `MAIN` on a case-insensitive filesystem
    /// would resolve and offer a second, wrongly spelled row.
    #[must_use]
    pub fn query_is_listed(&self) -> bool {
        self.rows.iter().any(|r| r.name().eq_ignore_ascii_case(&self.query))
    }

    /// The on-screen rows: the frozen matches, then a live probe hit as one more row. The
    /// hit is appended, never inserted, so it cannot move the highlight.
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

/// The commit picker's state while it is open. The rows
/// refresh under a poll and reconcile by sha; the highlight and the anchor are the reviewer's
/// own place state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitPicker {
    /// The universe, newest first.
    pub rows: Vec<git::CommitRow>,
    /// The current pick when it is not wholly in `rows`: painted as one row above the list,
    /// outside every run. `None` when the pick is listed, or there is no pick.
    pub pick_row: Option<CommitPick>,
    /// The highlighted row, an index into the visible view (`pick_row` first when present).
    pub cursor: usize,
    /// The anchor row, an index into the visible view, never the pick row.
    pub anchor: Option<usize>,
    /// The picker's title, naming the universe.
    pub title: String,
    /// The empty-universe message.
    pub empty: String,
    /// The `HEAD` the rows were listed under: a poll re-lists only once it moves.
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

    /// Whether `pick` is wholly listed: both ends are rows and the oldest sits at or below
    /// the newest. A pick that is not gets the pick row instead.
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

    /// The inclusive visible range from the anchor to the highlight, or the highlight alone
    /// : `(top, bottom)` in visible indices. The pick row sits outside
    /// every run, so the highlight on it is a run of itself whatever the anchor.
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

    /// Whether visible index `i` carries the run bar: the rows from the anchor to the
    /// highlight, only while an anchor is set.
    pub fn in_run(&self, i: usize) -> bool {
        if self.anchor.is_none() || self.is_pick_row(self.cursor) {
            return false;
        }
        let (top, bottom) = self.run();
        (top..=bottom).contains(&i)
    }

    /// The pick `enter` makes: the pick row re-picks itself, else the run's oldest and
    /// newest commits. The rows are newest first, so the bottom of the run is the oldest.
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
    /// Choosing which agent a `Send` goes to. Its rows and highlight
    /// live in [`App::picker_rows`] and [`App::picker_cursor`].
    Picker,
    /// Choosing the `branch` scope's base. Its state lives in
    /// [`App::base_picker`].
    BasePick,
    /// Choosing the `commits` scope's pick. Its state lives
    /// in [`App::commit_picker`].
    CommitPick,
    /// The search screen, replacing the body from any tab. Its state
    /// lives in [`App::search`].
    Search,
    /// The band at the read pane's foot: find in the file, or a line number to jump to. Its
    /// state lives in [`App::find`].
    Find,
}

impl Mode {
    /// Whether this mode is a modal hold: the reviewer is mid-gesture over the body, with keys
    /// and a mouse of its own. A modal freezes the open diff, so the world can never move the
    /// anchor, the scroll, or the selection out from under the gesture (Continuity), and it captures the mouse so no click reaches the view behind
    ///
    /// `Search` replaces the body rather than holding a place in it, and `Find` is a band the
    /// reviewer navigates the live diff with. Neither freezes anything, so neither is modal here.
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
    /// A `Code` pick's hit: the 1-based line and its matched byte spans, banded and
    /// emphasized by the renderer. A `Files` pick previews from the top.
    pub hit: Option<(u64, Vec<(u32, u32)>)>,
    /// Top visible row. The renderer centers the hit here once per build, then
    /// `PageUp`/`PageDown` move it freely.
    pub scroll: std::cell::Cell<usize>,
    /// Cleared by the renderer after it centers the hit for this build.
    pub center: std::cell::Cell<bool>,
}

/// The search screen's state: the query as typed, the mode, the pick, the last landed
/// results, and the settled preview. Dropped whole on close — a query is cheap, unlike a
/// comment draft.
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
    /// The settled preview of the picked result. `None` until the first build, or while
    /// nothing is pickable. The event loop rebuilds it once input settles, whenever it no
    /// longer matches the pick — a sweep never waits on a build.
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

/// The band at the read pane's foot while `mode == Mode::Find`: find in the file, or the line
/// field. Find stores only its query — matches, count and highlight derive from it against the
/// open file each frame.
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
    /// A row hidden in a collapsed fold; `new_no` is its context line, which lands on it once
    /// the fold opens.
    Folded { new_no: u32 },
}

/// The char-index ranges of every non-overlapping occurrence of `query` in `text`, honoring
/// `case_sensitive` (pass [`find_case_sensitive`]'s result for smart-case). Char indices, so the
/// diff renderer overlays the highlight the same way it does word emphasis.
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

/// A footer action — what the bar offers for the current context. Semantic only: the renderer
/// maps each to its key glyph and label and styles it by [`Band`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FooterAction {
    Comment,
    Select,
    ClearSelection,
    EditComment,
    EditFile,
    DeleteComment,
    JumpComment,
    ExpandFold,
    /// Take the armed crossing: the hunk step that armed it leaves the file when pressed again.
    /// The direction names the destination and picks the key (`] next file`, `[ prev file`).
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
    /// The search screen's own bar: flip, pick, open, close. The flip
    /// label names the destination mode, derived from the current mode at render time.
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
    /// Flip a markdown file between rendered and source; the label names the destination
    /// view (`m rendered` on source, `m source` rendered).
    Rendered,
    NavigatorPosition,
    /// Hide the navigator or show it back; the label names the direction (`z hide` / `z show`).
    /// Visible, it waits in the `go` band; hidden, it joins row 1.
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
    /// The agent picker's own bar: send to the highlight, move it, and cancel
    /// The digits are literal here, so the move hint names them.
    PickAgent,
    MovePickerRow,
    ClosePicker,
    /// Open the base picker.
    BasePick,
    /// The base picker's own bar: pick the highlight and move it. Every printable is
    /// filter text there, so the move hint names the arrows alone.
    PickBaseRow,
    MoveBaseRow,
    /// Open the commit picker.
    CommitPick,
    /// The commit picker's own bar: pick the run, move the highlight, set the anchor, and
    /// `esc`.
    PickCommitRun,
    MoveCommitRow,
    CommitAnchor,
    CloseCommitPicker,
    /// The scopes to switch away to, on every row 1 that leads with a scope switch: the
    /// scope already showing would offer a no-op there.
    ScopeOther,
    OpenPr,
    Refresh,
    Tabs,
    Quit,
}

/// Where a footer action sits: on row 1 (`Primary`, `Send`, or a `Do` cursor action), or in one of
/// the `?`-expansion bands (`Do` overflow, `Go`, `Move`). Row 1 keeps the primary, `send`, and the
/// `?`, trimming trailing `Do` actions to fit and spilling them into the `do` band.
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

/// One mouse-down of the multi-click chain: when and where it landed, the row its cell mapped
/// to on its surface, and the click count it reached (1 single, 2 double, 3 triple).
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
// The several bools (wrap, reveal_files, reveal_diff, should_quit, and refresh flags) are independent
// toggles, not a state machine in disguise, so the excessive-bools lint does not apply.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct App {
    pub repo: PathBuf,
    pub base: Option<String>,
    /// The `branch` scope's base outcome, carried by the latest landed snapshot — the
    /// header names its winner (or the skip) and the diff builds against the winner's OID
    pub branch_base: git::BaseStatus,
    /// The `commits` scope's pick: two commit ids, in memory, replaced and never cleared
    /// `None` until the first pick.
    pub commit_pick: Option<CommitPick>,
    /// The pick's verdict and subject from the latest landed `commits` build, so the header
    /// marker and the changeset it heads land whole.
    pub pick_status: Option<PickStatus>,
    /// The commit picker's rows, highlight, and anchor while `Mode::CommitPick` is open
    pub commit_picker: Option<CommitPicker>,
    /// Bumped by each pick made in this pane, so an in-flight build that read the old pick
    /// fails the landing's input match instead of reverting the pick (`crate::world::WorldInput`).
    base_epoch: u64,
    pub scope: Scope,
    /// The active tab; it drives both panes and selects the per-tab state in play.
    pub tab: Tab,
    /// Which file tab (`Changes`/`AllFiles`) currently occupies the diff/file fields. Tracked
    /// apart from `tab` so the `PR` tab can be active while a file tab's state stays frozen in
    /// place, with the other file tab in the stash.
    active_file_tab: Tab,
    pub focus: Focus,
    /// The navigator's source for the active tab: changed files in `Changes`, the whole
    /// worktree in `All files`.
    pub entries: Vec<Entry>,
    /// The flattened directory tree over `entries` — the rows the navigator paints. The
    /// `file_cursor` indexes this, not `entries`.
    pub file_rows: Vec<file_list::Row>,
    pub file_cursor: usize,
    /// Top visible row of the file list, kept so `file_cursor` stays on screen when the
    /// changeset is taller than the pane.
    pub file_scroll: usize,
    /// Set by a navigation that moves `file_cursor`; consumed once per frame to scroll the
    /// cursor into view. The wheel never sets it, so wheel-scrolling moves the viewport alone.
    pub reveal_files: bool,
    /// Set by a navigation that moves `diff_cursor`; consumed once per frame to scroll the
    /// cursor into view. The wheel never sets it.
    pub reveal_diff: bool,
    /// Set by a jump to a named line (`goto_line`): consumed with `reveal_diff`, it centers the
    /// cursor when it landed off screen instead of nudging it to the nearest edge.
    pub reveal_center: bool,
    /// The file crossing a hunk step armed when it found no further hunk in the open file. The
    /// next step the same way takes it, and any other input drops it.
    armed_cross: Option<ArmedCross>,
    /// Whether the current compose was opened from the comments-list overlay, so finishing it
    /// returns there rather than dropping to the diff.
    resume_list: bool,
    /// Directory paths toggled away from the tab's resting state — collapsed in `Changes`
    /// (expanded by default), expanded in `All files` (collapsed by default). Keyed by path,
    /// so it survives a poll that rebuilds the tree.
    toggled_dirs: HashSet<String>,
    /// The inactive tab's saved state, swapped in on a tab switch.
    stash: TabStash,
    /// The active scope's changed files, keyed by repo-relative path and recomputed every
    /// reload regardless of tab. Keys back the header count and diff-comment staleness; values
    /// annotate `All files` entries with their marker and stats. Stays correct while `All
    /// files` lists the whole worktree.
    changed: HashMap<String, Annotation>,
    pub diff: FileDiff,
    /// The rows actually shown: `diff.rows` with each fold collapsed to a marker or
    /// expanded to its lines. The cursor, scroll, selection, and hit-testing index this.
    pub visible: Vec<Row>,
    /// Fold anchors (first-hidden-line numbers) currently expanded; survives a poll.
    expanded_folds: HashSet<u32>,
    /// The file the open diff belongs to — the diff title, frozen with the diff
    /// while composing even if `file_cursor` drifts as the file list updates.
    pub diff_path: Option<String>,
    pub diff_cursor: usize,
    /// Top visible diff line. Sticky: only moves to keep the cursor in view, so the
    /// diff does not jump on every cursor step and drag-selection stays stable.
    pub diff_scroll: usize,
    /// Horizontal scroll, in columns, applied to the diff when wrap is off.
    pub h_scroll: usize,
    /// Whether long diff lines wrap (default) or are scrolled horizontally.
    pub wrap: bool,
    /// The open file's rendered markdown view: the content, the render, its marks, and the
    /// index over its rows. Per file tab, stashed whole. `rendered_active()` is the honest
    /// on-screen predicate.
    rendered: RenderedView,
    /// The pane's markdown choice: rendered, or source. One for the pane, like `wrap`: `m`
    /// flips it, and every markdown file opened afterwards — either tab, any file — follows
    /// it. Seeded from `markdown_view` for a fresh pane; a reread never flips it, and content
    /// that renders nothing shows its source without touching it.
    markdown_rendered: bool,
    /// The rendered rows' wrap width — the read pane's code column, noted each frame by
    /// [`Self::sync_rendered_width`]. `0` until the first frame, which builds at a default.
    rendered_width: usize,
    /// The link regions painted this frame — a click resolves against the painted
    /// frame.
    painted_links: std::cell::RefCell<Vec<PaintedLink>>,
    /// The read pane's display-line layout as painted this frame (`ui::read_layout`) — the
    /// recording every read-pane hit test indexes, so no map can disagree with the screen.
    /// A mid-event scroll re-runs the walk (`ui::refresh_read_layout`)
    painted_slots: std::cell::RefCell<Vec<crate::ui::Slot>>,
    /// The painted markdown body's heading anchors as `(slug, content line index)`,
    /// covering the whole body — an anchor click can jump past the viewport.
    painted_anchors: std::cell::RefCell<Vec<(String, usize)>>,
    /// `<details>` summary hit boxes painted this frame.
    painted_details: std::cell::RefCell<Vec<PaintedDetails>>,
    /// Open `<details>` on the selected PR description or thread.
    pr_expanded_details: HashSet<String>,
    /// The PR read pane's maximum useful scroll, noted the same way for
    /// [`Self::pr_scroll_read`].
    pr_read_max_scroll: std::cell::Cell<usize>,
    /// The global navigator placement and the separate shares remembered for each split axis.
    pub navigator_position: crate::config::NavigatorPosition,
    pub navigator_side_pct: u16,
    pub navigator_stack_pct: u16,
    /// The presence toggle over the navigator, one state across all tabs, never a position
    /// A restart shows the navigator; recovery preserves this.
    pub navigator_hidden: bool,
    /// The search screen's results-pane share — search's own session value, separate
    /// from the review layout's shares.
    pub search_pct: u16,
    divider_drag: DividerDrag,
    pub select_anchor: Option<usize>,
    /// The comment the reviewer picked ([`Self::target_comment_card`]) — which of the comments
    /// under the cursor they chose, by its store id — so `e`/`d` reach it where several cover
    /// the cursor's row. Place state of the tab, stashed with its cursor. Live while the store holds it and it covers the cursor's row
    /// ([`Self::live_target`]); the reviewer's input that leaves it not live clears it
    /// ([`Self::settle_pick`]), and a poll never does.
    comment_target: Option<u64>,
    /// The one live mouse gesture — born at mouse-down, ended on release or an interrupting event.
    pub gesture: crate::selection::Gesture,
    /// The settled selection: the last copy's span and its copied text, kept highlighted as
    /// feedback until the next mouse-down, a keypress, or a refresh that changes the text it
    /// spans.
    settled_sel: Option<(crate::selection::TextDrag, String)>,
    /// The pointer's last reported cell, recomputed against each frame for the gutter hover
    /// `+`.
    pub hover: Option<(u16, u16)>,
    /// The multi-click chain: the last mouse-down, while the window holds.
    last_click: Option<LastClick>,
    /// Whether a view-anchored gesture held the open view's reload while snapshots landed
    /// beneath it, so the gesture's end reloads it once.
    view_reload_held: bool,
    pub store: CommentStore,
    pub list_cursor: usize,
    /// The picker's rows, frozen at the moment it opened. A refresh behind it adds, drops,
    /// and reorders nothing.
    pub picker_rows: Vec<AgentChoice>,
    pub picker_cursor: usize,
    /// The mode the picker opened over — `Normal`, the comments list, or the find band —
    /// so closing it restores the view the reviewer sent from.
    pub picker_over: Mode,
    /// The agent this session last sent to, which arms the picker's highlight. Only a
    /// successful send sets it.
    pub last_sent_pane: Option<String>,
    /// The base picker's rows, filter, and highlight while `Mode::BasePick` is open
    pub base_picker: Option<BasePicker>,
    pub mode: Mode,
    pub input: String,
    /// The comment editor's caret: a char index into `input` (`0..=chars().count()`).
    pub caret: usize,
    pub status: String,
    /// Whether the footer's `?` shortcut list is expanded. Global place state, not tab-stashed: one
    /// toggle across every tab, moved only by `?` and `esc`, preserved through a poll and config
    /// recovery (Continuity).
    pub keys_expanded: bool,
    pub should_quit: bool,
    /// The quit key was pressed with unsent comments, and the footer asks before they are
    /// dropped. The reviewer's next key, click, or wheel turn answers it, so it is never left open
    /// behind another action (#119). A field beside [`Mode`] rather than a mode, because the
    /// config-error screen asks it over whatever mode it froze, a draft included.
    pub confirming_quit: bool,
    /// The read-only `PR` tab's view of the pull request.
    pub pr: forge::PrView,
    /// The resolved repository target's forge, from the latest input probe. Display strings
    /// pick their noun and reference form from it; a forge
    /// change always changes the target, which clears the PR before a mismatch could paint.
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
    /// The PR refresh awaiting dispatch, if any; the event loop services it after drawing, so
    /// a `loading` frame shows before the blocking CLI calls run.
    pub pr_pending: Option<RefreshKind>,
    /// The world refresh request awaiting dispatch, if any; the event loop hands it to
    /// the worker after the frame paints.
    pub world_request: Option<crate::world::WorldRequest>,
    /// The search overlay's state while `mode == Mode::Search`, `None` otherwise.
    pub search: Option<SearchOverlay>,
    /// Set by every query edit (and the open); the event loop dispatches the query to the
    /// search worker after the frame paints, tagged latest-wins.
    pub search_dirty: bool,
    /// A picked path awaiting its frecency record; the event loop hands it to the worker.
    pub search_track: Option<String>,
    /// An `edit` press that named a file. The event loop runs the editor, suspending the pane
    /// first only for one that draws there. `None` when idle.
    pub editor_request: Option<EditTarget>,
    /// The band's state — find or the line field — while `mode == Mode::Find`, `None` otherwise
    pub find: Option<Find>,
    /// Whether the tab-strip glyph paints this frame — maintained by the event loop's
    /// appear-delay and minimum-display clocks.
    pub refresh_indicator: bool,
    /// Set by `r`: the next refresh is commanded, so the glyph lights immediately
    /// instead of waiting out the ambient appear delay.
    pub refresh_commanded: bool,
    /// Whether the active file tab has ever completed a reload (stash counterpart:
    /// `TabStash::visited`). Gates the first-visit synchronous load in [`Self::set_tab`].
    tab_visited: bool,
    highlighter: Highlighter,
    /// The active palette every renderer paints from.
    palette: Palette,
    /// The active theme's name, so re-resolving to the same theme is a no-op.
    theme_name: &'static str,
    /// The `--theme` override name (highest precedence); `None` lets config/appearance decide.
    cli_theme_name: Option<String>,
    /// Host appearance sampled on open; `None` uses the dark fallback.
    detected_appearance: Option<crate::appearance::Appearance>,
    /// The plugin is either ready with one validated snapshot or wholly blocked on its error.
    config: PluginConfigState,
    /// The last theme name requested, so re-resolving the same name skips work and logging.
    requested_theme_name: Option<String>,
    cache: DiffCache,
    /// The markdown render memo behind the PR read pane and the file tabs' rendered
    /// rows. Interior-mutable so the renderer can fill it from
    /// `&App`; cleared with the diff cache on a theme switch.
    markdown_cache: std::cell::RefCell<crate::markdown::RenderCache>,
    snippet_cache: std::cell::RefCell<crate::snippet::SnippetRowCache>,
    /// The worker-owned turn baseline, mirrored from completions so the sync `last-turn`
    /// paths (the diff's old side, the scope-switch rebuild) read it without a round-trip.
    turn_baseline: Option<String>,
    /// Whether any agent is in this worktree — the one home for the answer, held here
    /// because this is what paints it. `None` until a sample observes it, so a frame that
    /// has seen nothing waits instead of asserting an emptiness nobody looked for: stale is
    /// allowed, wrong is not (Continuity). Only a sample that observed the
    /// whole worktree moves it — herdr answered and git resolved every member's directory — so
    /// `Some(false)` always means someone looked and found no member.
    agents_present: Option<bool>,
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

impl App {
    pub fn new(repo: PathBuf, scope: Scope, base: Option<String>) -> Self {
        Self::build(repo, scope, base, true)
    }

    /// Construct the error-only reviewr pane without reading repository state.
    pub(crate) fn blocked(repo: PathBuf, scope: Scope, base: Option<String>) -> Self {
        Self::build(repo, scope, base, false)
    }

    fn build(repo: PathBuf, scope: Scope, base: Option<String>, load_turn: bool) -> Self {
        // Mirror any persisted turn baseline for this worktree, so `last-turn` keeps its
        // anchor across a reviewr pane restart. The worker's `TurnHost` owns the tracker; this
        // mirror follows its completions.
        let turn_baseline = if load_turn { crate::world::seed_baseline(&repo) } else { None };
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
            changed: HashMap::new(),
            diff: FileDiff::empty(),
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
            detected_appearance: None,
            config: PluginConfigState::Ready(crate::config::PluginConfig::default()),
            requested_theme_name: None,
            cache: DiffCache::new(),
            markdown_cache: std::cell::RefCell::new(crate::markdown::RenderCache::default()),
            snippet_cache: std::cell::RefCell::new(crate::snippet::SnippetRowCache::default()),
            turn_baseline,
            agents_present: None,
        }
    }

    /// Resolve `name` (a CLI or config value; `None` = default) and apply it when it changes:
    /// rebuild the highlighter and drop cached diffs so they re-render. Unknown or
    /// not-yet-supported names fall back to the default.
    fn set_theme(&mut self, name: Option<&str>) {
        // Re-resolving the same name every poll would redo derivation and re-log an unknown
        // name, so skip when the request is unchanged.
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

    /// Store a fresh appearance observation, returning whether the effective palette changed.
    pub(crate) fn detected_appearance(&self) -> Option<crate::appearance::Appearance> {
        self.detected_appearance
    }

    pub(crate) fn set_detected_appearance(
        &mut self,
        appearance: crate::appearance::Appearance,
    ) -> bool {
        if self.detected_appearance == Some(appearance) {
            return false;
        }
        let previous = self.theme_name;
        self.detected_appearance = Some(appearance);
        if self.plugin_config().is_some() {
            self.refresh_theme();
        }
        self.theme_name != previous
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
        // A config layout or theme change also completes a live gesture's copy — that runs at
        // the observation boundary, where the painted frame is still in reach
        // (`lib.rs::reconcile_plugin_config`).
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
        // The config view replaces the body, ending any gesture over it. The observation
        // boundary completes a visible selection's copy first
        // (`lib.rs::reconcile_plugin_config`); this cancel is the backstop for every other
        // caller, so the blocked pane never holds a live gesture (CFG-BLOCKED-INERT).
        self.cancel_gesture();
        // The search overlay, the find band, and the agent picker close when the config view
        // takes over; recovery restores the tab beneath them. The query is not restored, and
        // neither are the picker's frozen rows, which would be stale by then.
        // The picker closes first, onto the mode it opened over, so the two closers below then
        // tear down that mode's own state instead of leaving it restored but emptied.
        self.close_picker();
        self.close_search();
        self.close_find();
        self.config = PluginConfigState::Blocked { error };
        self.pr_pending = None;
    }

    /// The active keymap: the snapshot's while ready, the defaults while blocked. The blocked
    /// arm only keeps this total — blocked key handling never reaches dispatch; the event
    /// loop's error gate answers the default `quit` key itself (`lib.rs`).
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

    /// Move user-authored review state into a freshly loaded app after config recovery. Saved
    /// comments always survive; an in-progress draft keeps the exact frozen diff it was written
    /// against, matching the ordinary refresh invariant.
    pub(crate) fn carry_authored_state_from(&mut self, old: &mut Self) {
        self.store = std::mem::take(&mut old.store);
        self.list_cursor = old.list_cursor;
        // The footer expansion is one global toggle, carried regardless of the recovered mode
        self.keys_expanded = old.keys_expanded;
        // The `last used` arming is session memory, like the comments themselves — a config
        // error must not forget which agent the session sent to.
        self.last_sent_pane = old.last_sent_pane.take();
        // The commit pick is session memory like the comments: replaced, never cleared
        self.commit_pick = old.commit_pick.take();
        self.navigator_side_pct = old.navigator_side_pct;
        self.navigator_stack_pct = old.navigator_stack_pct;
        self.navigator_hidden = old.navigator_hidden;
        // A hidden navigator keeps focus on the read pane; the fresh app
        // starts on the file list. The `List`/`Composing` arm re-carries the exact focus.
        if self.navigator_hidden {
            self.focus = Focus::Diff;
        }
        self.search_pct = old.search_pct;
        // A tab switch requested its refresh and recovery landed first: the carried fields
        // below may reinstate the stale stashed frame, so the pending request must survive
        // the swap or that frame never refreshes until the next poll.
        self.world_request = old.world_request.take();
        let old_mode = old.mode.clone();
        match old_mode {
            // `set_config_error` closes the search overlay, the find band, and the agent picker
            // before the mode is stored, so none reaches recovery; the search query is not
            // restored and the picker's frozen rows are not either.
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
                self.changed = std::mem::take(&mut old.changed);
                // The header's base label describes the carried list, so it carries too —
                // a fresh app would paint `no base` beside a populated frame
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
        let name = crate::appearance::select_theme(
            self.cli_theme_name.as_deref(),
            self.config_snapshot().theme(),
            self.detected_appearance,
        );
        self.set_theme(Some(&name));
    }

    /// The active palette every renderer paints from.
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn composing(&self) -> bool {
        matches!(self.mode, Mode::Composing { .. })
    }

    /// The entry under the cursor when the cursor is on a file row; `None` on a directory
    /// row (or an empty list).
    pub fn current_entry(&self) -> Option<&Entry> {
        self.file_under_cursor_index().map(|i| &self.entries[i])
    }

    /// A directory's resting state in the active tab: `Changes` opens expanded, `All files`
    /// collapsed.
    fn default_expanded(&self) -> bool {
        self.tab == Tab::Changes
    }

    /// The `entries` index of the file row under the cursor, or `None` on a directory row.
    fn file_under_cursor_index(&self) -> Option<usize> {
        self.file_rows.get(self.file_cursor).and_then(file_list::Row::file_index)
    }

    /// The visible-row index of the file at `path`, for restoring selection across a poll.
    fn file_row_of_path(&self, path: &str) -> Option<usize> {
        self.file_rows
            .iter()
            .position(|r| r.file_index().is_some_and(|i| self.entries[i].path == path))
    }

    /// The visible-row index of the first file row, the initial selection so a diff shows
    /// at once even when the tree opens on a directory.
    fn first_file_row(&self) -> Option<usize> {
        self.file_rows.iter().position(|r| r.file_index().is_some())
    }

    /// Rebuild the flattened tree from `entries` and the toggled-directory set.
    fn rebuild_file_rows(&mut self) {
        self.file_rows =
            file_list::build(&self.entries, &self.toggled_dirs, self.default_expanded());
    }

    /// What the cursor currently points at — a file (by path) or a directory (by path) — so
    /// the cursor can be put back on the same target after the tree rebuilds.
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

    /// The file whose diff the pane shows: the file under the cursor, or — when the cursor
    /// rests on a directory — the already-open file (matched by `diff_path`), so scanning the
    /// tree never blanks the diff. `None` only when nothing is open.
    fn shown_entry(&self) -> Option<Entry> {
        if let Some(e) = self.current_entry() {
            return Some(e.clone());
        }
        let open = self.diff_path.as_deref()?;
        self.entries.iter().find(|e| e.path == open).cloned()
    }

    /// Never touches the comment store or the in-progress input — that is the
    /// "a comment is never lost to a refresh" invariant.
    pub fn reload(&mut self) -> Result<()> {
        self.ensure_config_ready()?;
        // The PR tab holds its own state and renders nothing from the file tree, so a poll on
        // it skips the rebuild; switching back to a file tab reloads it then.
        if !self.tab.is_file_tab() {
            return Ok(());
        }
        // Outside a git repo, show an empty state rather than failing.
        if !git::is_repo(&self.repo) {
            self.entries.clear();
            self.changed.clear();
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

    /// The input the next world build reads — the tag a landed snapshot is checked against
    /// before it may reconcile.
    pub fn world_input(&self) -> crate::world::WorldInput {
        crate::world::WorldInput {
            repo: self.repo.clone(),
            tab: self.tab,
            scope: self.scope,
            base: self.base.clone(),
            base_epoch: self.base_epoch,
            turn_baseline: self.turn_baseline.clone(),
            commit_pick: self.commit_pick.clone(),
            // `Changes` never reads the toggled set, so it stays out of that tab's tag —
            // a directory toggle there must not invalidate an in-flight build.
            toggled_dirs: if self.tab == Tab::AllFiles {
                self.toggled_dirs.clone()
            } else {
                HashSet::new()
            },
        }
    }

    /// Adopt a build's base outcome — the one rule for both writers (the landed snapshot
    /// and the scope switch's synchronous rebuild): only the `branch` scope owns a base,
    /// and the base and the changeset it produced land together, so the header name and
    /// the list it heads never disagree.
    fn adopt_branch_base(&mut self, base: git::BaseStatus) {
        if self.scope == Scope::Branch {
            self.branch_base = base;
        }
    }

    /// Adopt a build's pick verdict, the same way: only the `commits` scope owns one, and it
    /// lands with the changeset it heads.
    fn adopt_pick_status(&mut self, status: Option<PickStatus>) {
        if self.scope == Scope::Commits {
            self.pick_status = status;
        }
    }

    /// Reconcile a built snapshot into the view — the one place a world result touches place
    /// state, by identity first, then fallback, then clamp (Continuity).
    /// A navigator drag never reaches here: it gates the world drain itself, so the snapshot
    /// waits in the completion channel (`lib.rs`).
    pub fn reconcile_world(&mut self, snapshot: crate::world::WorldSnapshot) {
        // A content change under the pointer resets the multi-click chain — the clicked
        // row's own text counts, so a same-length edit under the pointer breaks it too — while
        // a snapshot that changed nothing on screen leaves a double-click in flight alone
        // Keep the cursor on the same row target across the rebuild; fall back to the open
        // file, then the first file. The toggled-directory set survives untouched.
        let anchor = self.cursor_anchor();
        let open = self.diff_path.clone();
        self.changed = snapshot.changed;
        self.entries = snapshot.entries;
        self.adopt_branch_base(snapshot.branch_base);
        self.adopt_pick_status(snapshot.pick_status);
        self.rebuild_file_rows();
        self.file_cursor = anchor
            .and_then(|a| self.row_of_anchor(&a))
            .or_else(|| open.as_deref().and_then(|p| self.file_row_of_path(p)))
            .or_else(|| self.first_file_row())
            .unwrap_or(0)
            .min(self.file_rows.len().saturating_sub(1));
        // A poll preserves the file-list wheel scroll — it does not reveal the cursor.
        // Explicit actions (navigation, a scope switch) request their own reveal.
        // While a modal is open the diff below it is frozen, so a poll can't shift the anchor
        // beneath the writer, reset the scroll and selection under the overlay, or move the
        // reviewer's place while they choose an agent (`Mode::is_modal`, Continuity).
        // A view-anchored drag holds it the same way, catching up when the gesture ends.
        // The file list still updates above.
        if self.view_anchored_gesture() {
            self.view_reload_held = true;
        } else if !self.view_frozen() {
            self.reload_open_view();
        }
        // A landed poll repaints the search preview in place — never the results, which
        // describe the worktree when their query ran.
        self.refresh_search_preview();
        // The find band closes if its file lost its searchable rows or changed identity under the
        // poll — a forced return. The current
        // match otherwise follows the reconciled cursor, so nothing else to do.
        if self.mode == Mode::Find && (open != self.diff_path || !self.find_available()) {
            self.close_find();
        }
        // The read pane's marks were revalidated by its own rebuild (`rebuild_visible`). The
        // navigator's settled highlight follows Continuity the same way: it survives a land
        // that left its text alone and blanks when the text under it changed — stale never
        // wrong. The `PR` surfaces land through the PR paint.
        if let Some((d, text)) = &self.settled_sel
            && d.surface == crate::selection::Surface::Files
        {
            let (a, b) = d.ordered();
            if crate::selection::files_text(&self.file_rows, &self.entries, a.row, b.row) != *text {
                self.settled_sel = None;
            }
        }
        // The open commit picker reads the same world: its list refreshes under the poll and
        // reconciles by sha.
        self.refresh_commit_picker(snapshot.head.as_deref());
        self.tab_visited = true;
    }

    /// The text of the read-pane row the multi-click chain last targeted, for the rebuild's
    /// revalidation. `None` for a chain on another surface, which no rebuild moves.
    fn clicked_target_text(&self) -> Option<String> {
        let click = self.last_click?;
        if click.surface != crate::selection::Surface::Read {
            return None;
        }
        Some(self.visible.get(click.target)?.text())
    }

    /// Reload the open view against the reconciled list: only a different shown file resets
    /// the diff view to the top, and it drops an armed crossing, which was armed at the edge
    /// of a file that is no longer the one on screen.
    fn reload_open_view(&mut self) {
        if self.shown_entry().map(|e| e.path) != self.diff_path {
            self.reset_diff_view();
            self.armed_cross = None;
        }
        self.load_read();
    }

    /// Show no file: an empty diff, no rows — `visible` keeps mirroring `diff`, so no stale row
    /// paints — no rendered view behind them, and no marks on what is gone.
    fn clear_open_view(&mut self) {
        self.diff = FileDiff::empty();
        self.diff_path = None;
        self.rendered.clear();
        self.visible.clear();
        self.drop_read_marks();
        self.reset_diff_view();
    }

    /// Load the read pane for the active tab: the scope diff in `Changes`, the whole-file
    /// content in `All files`. Both flatten into `visible` and settle the cursor/scroll.
    fn load_read(&mut self) {
        let Some(entry) = self.shown_entry() else {
            self.clear_open_view();
            return;
        };
        self.open_path_in_tab(entry.path, entry.previous_path);
    }

    /// Open `path` in the active tab's read pane: the scope diff in `Changes` (rename-aware via
    /// `previous_path`), the whole-file content in `All files`. The one place this dispatch lives,
    /// so opening a file from the tree and from a comment edit can't drift apart.
    fn open_path_in_tab(&mut self, path: String, previous_path: Option<String>) {
        match self.tab {
            Tab::AllFiles => self.set_file_view(&path),
            // `Changes` (the `PR` tab never opens a file in the read pane).
            _ => self.set_diff(path, previous_path),
        }
    }

    /// Build the diff for a specific `path` regardless of whether its row is visible in the
    /// tree — so editing a comment can surface its file even from a collapsed directory.
    fn set_diff(&mut self, path: String, previous_path: Option<String>) {
        // A different file opens with all folds collapsed, and rendered when it is markdown.
        // `expanded_folds` is keyed by line number, so without the clear a fold in the new file
        // whose first hidden line matches an expanded one in the old file would render
        // pre-expanded. A same-file poll or scope switch keeps both the folds and the
        // rendered/source choice.
        if self.diff_path.as_deref() != Some(path.as_str()) {
            self.expanded_folds.clear();
            self.open_fresh();
        }
        self.diff_path = Some(path.clone());
        // Git already reported no text diff for this change — binary content, or a path whose
        // `diff` attribute `.gitattributes` unsets. Take that verdict rather than re-deciding
        // from content, which would paint a `-diff` lockfile as a full text diff, and skip
        // both blob reads while we are at it.
        let (old, new) = if self.changed.get(&path).is_some_and(|a| a.binary) {
            self.diff = FileDiff::binary_notice(path, previous_path);
            (String::new(), String::new())
        } else {
            let (old, new) = self.content_sides(&path, previous_path.as_deref());
            self.diff = self.cache.get(path, previous_path, &old, &new, &self.highlighter);
            (old, new)
        };
        // Hold the new side as the render input, the same current content the File view
        // renders, and the old side the marks read deletions against. A non-markdown file, a
        // notice, or a deleted file (empty new side) holds nothing, so it shows its source and
        // its toggle stays inert.
        let renders = self.markdown_file() && self.diff.state == crate::diff::FileState::Normal;
        self.rendered.content = if renders { self.content(new, Some(old)) } else { None };
        self.rebuild_visible();
        self.settle_read();
    }

    /// Build the File view for `path`: its current worktree content as `Context` rows, no
    /// folds. The `All files` read pane. Content is scope-independent.
    fn set_file_view(&mut self, path: &str) {
        // Opening a different file starts rendered when it is markdown; a same-file refresh
        // keeps the rendered/source choice.
        if self.diff_path.as_deref() != Some(path) {
            self.open_fresh();
        }
        self.diff_path = Some(path.to_string());
        self.expanded_folds.clear(); // the File view has no folds
        let (diff, content) = self.file_view(path);
        // Keep the render input current without a per-frame rebuild. A file the source view
        // degrades to a notice never renders, so its content is not held either.
        let renders = self.markdown_file() && diff.state == crate::diff::FileState::Normal;
        self.rendered.content = if renders { self.content(content, None) } else { None };
        self.diff = diff;
        self.rebuild_visible();
        self.settle_read();
    }

    /// Build the read pane's File view for `path`: an over-budget blob (a model weight, a
    /// vendored bundle) previews as the too-large notice without a read — reading it whole
    /// would spike the UI thread before `build_file`'s budget could discard it — else the
    /// worktree content is highlighted through the shared content-hash cache. Returns the
    /// diff and the content read (empty for the notice), for a caller that also keeps the
    /// raw content. The one build the source view and the search
    /// preview share.
    fn file_view(&mut self, path: &str) -> (FileDiff, String) {
        let oversize = std::fs::metadata(self.repo.join(path))
            .is_ok_and(|m| crate::diff::over_byte_budget(m.len() as usize));
        if oversize {
            (FileDiff::too_large_notice(path.to_string()), String::new())
        } else {
            let content = worktree_content(&self.repo, path);
            let diff = self.cache.get_file(path.to_string(), &content, &self.highlighter);
            (diff, content)
        }
    }

    /// Clamp the cursor, scroll, and selection to the rebuilt `visible`, keeping the reader's
    /// position. A shrunk view that forced the cursor to move reveals it; a poll that left it
    /// in range does not, so a wheel scroll survives.
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

    /// Reset the per-file view state for a newly opened file: every `<details>` collapsed. The
    /// rows of the file being left are dropped first, so no place or mark is carried from one
    /// file to another.
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

    /// Build `visible`: the rendered rows when the open markdown file shows rendered, else
    /// `diff.rows` flattened — an expanded fold becomes its lines, a collapsed fold stays a
    /// single marker row. Content that renders nothing at all (an HTML comment alone) shows
    /// its source: an empty pane would claim there is nothing there. That is a property of
    /// this build, never a write to the reviewer's choice, so content that renders again
    /// shows rendered again.
    ///
    /// The one place rows are rebuilt, so the rules every rebuild owes live here: a change of
    /// row kind carries the cursor and scroll top by source line, the find band lives only
    /// while it can search, and the read pane's marks are revalidated against the new rows.
    fn rebuild_visible(&mut self) {
        let clicked_before = self.clicked_target_text();
        let was_rendered = self.rendered.on_screen();
        // The rendered rows' content edit since they were built: one line map per rebuild,
        // and only when the text moved under them.
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
        // Rows of the other kind: the place crosses by its source line. Same-kind rendered
        // rows reconciled by identity inside the build; same-kind source rows keep indices.
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
        self.revalidate_read_marks(clicked_before.as_deref());
    }

    /// The read pane's marks after its rows changed: the settled highlight survives only where
    /// the text under it is unchanged, and the multi-click chain only where its row's text is
    /// — stale never wrong (Continuity).
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

    /// Drop the read pane's marks wholesale: the rows they sat on belong to a view that is
    /// gone. A navigator mark survives, being on rows the open file never touched.
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

    /// The `<details>` keys open in the rendered view: derived from `disclosures` — the
    /// content's, open or not — the reviewer's own choices, and the changes and comments each
    /// holds ([`crate::marks::open_details`]).
    ///
    /// A choice keys by summary plus occurrence (`Details#1`), never by position, so an edit
    /// elsewhere keeps it. The accepted hole: a disclosure with the same summary inserted above
    /// takes over the choice's key, and renaming a summary drops its choice.
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

    /// The old side's source map with `open` disclosures opened: the cached one when the old
    /// text and the open set are the ones it came from, else a fresh render's.
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

    /// Build the rendered rows from the view's text at the noted width, theme, and open
    /// `<details>`, unless the rows on screen already came from exactly that input. A rebuild
    /// over rendered rows carries the cursor and the scroll top by their `(unit, offset)`
    /// identity, never by row index, mapping the line through the content's own edits when it
    /// changed (Continuity). Returns `false`, with the rows untouched, when the content renders
    /// nothing; that verdict is kept, so a poll of the same content never renders it again.
    ///
    /// The open `<details>` derive from the content's disclosures, which only a render knows:
    /// the build renders with the set it last derived — from the render on screen, when that
    /// rendered this same text — and renders again only when the new render derives another.
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
        // A content line's wrap: how many of its block's content lines start on the same
        // source line before it. A gap carries none: a block gains or loses its gap with the
        // blocks around it, never its own lines' wraps.
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

    /// Rebuild the rendered rows when the frame's geometry or theme moved — the frame hook
    /// calls this with this frame's code column, before measuring row heights. A modal
    /// freezes the open view, so a draft's anchor holds still and the rebuild waits for its
    /// close; a view-anchored gesture holds it the same way.
    pub fn sync_rendered_width(&mut self, width: usize) {
        if width == 0 || !self.tab.is_file_tab() {
            return;
        }
        self.rendered_width = width;
        // Content and `<details>` only move through their own rebuilds, so a frame compares
        // the width and the theme alone.
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

    /// Expand the fold under the cursor, revealing its hidden lines. Expansion is
    /// permanent for the session — an expand is taken as intentional, so there is no
    /// collapse-back.
    /// Expand the fold under the cursor, keeping the viewport visually still. Where the fold
    /// sits decides which way it grows: a fold in the top half of the diff expands upward (the
    /// lines below it hold their screen position); one in the bottom half expands downward (the
    /// lines above hold theirs). `heights`/`viewport` are this frame's pre-expand diff geometry.
    pub fn expand_fold(&mut self, heights: &[usize], viewport: usize) {
        let fold_idx = self.diff_cursor;
        let Some(anchor) = self.visible.get(fold_idx).and_then(Row::fold_anchor) else {
            return;
        };
        // Expanding replaces the 1 fold row with N context rows; rows below it shift by N-1.
        let shift = self.visible[fold_idx].hidden().saturating_sub(1);
        // Display rows between the viewport top and the fold; < half ⇒ top half. When the fold
        // is wheeled above the viewport (fold_idx < diff_scroll), the range is empty → above 0 →
        // top half, which is correct: the inserted rows land above the viewport, so advancing
        // diff_scroll by `shift` holds the visible content in place.
        let above: usize = heights.get(self.diff_scroll..fold_idx).map_or(0, |s| s.iter().sum());
        let top_half = above < viewport / 2;
        self.expanded_folds.insert(anchor);
        self.rebuild_visible();
        if top_half {
            self.diff_scroll += shift; // hold the content below the fold; grow upward
        }
        // bottom half: leave diff_scroll — the content above the fold stays put, grow downward
    }

    /// The old and new content of `file` for the current scope: old from `HEAD` (or the
    /// merge-base on the branch scope), new from the worktree. A rename reads its old side
    /// from `previous_path`, so the diff shows real edits, not a wholesale delete-and-add.
    fn content_sides(&self, path: &str, previous_path: Option<&str>) -> (String, String) {
        let new_path = path;
        let old_path = previous_path.unwrap_or(new_path);
        match self.scope {
            Scope::Uncommitted => {
                let old = git::file_content(&self.repo, "HEAD", old_path);
                let new = worktree_content(&self.repo, new_path);
                (old, new)
            }
            Scope::Branch => {
                let mb = self
                    .branch_base
                    .winner
                    .as_ref()
                    .and_then(|b| git::merge_base(&self.repo, b.oid()));
                let old =
                    mb.map(|m| git::file_content(&self.repo, &m, old_path)).unwrap_or_default();
                (old, worktree_content(&self.repo, new_path))
            }
            Scope::LastTurn => {
                let old = self
                    .turn_baseline
                    .as_deref()
                    .map(|b| git::file_content(&self.repo, b, old_path))
                    .unwrap_or_default();
                (old, worktree_content(&self.repo, new_path))
            }
            // Both sides from the commits: `A^` and `B`.
            Scope::Commits => {
                let Some(pick) = &self.commit_pick else { return (String::new(), String::new()) };
                let old = git::parent_or_empty(&self.repo, &pick.oldest)
                    .map(|a| git::file_content(&self.repo, &a, old_path))
                    .unwrap_or_default();
                (old, git::file_content(&self.repo, &pick.newest, new_path))
            }
        }
    }

    /// Whether the `commits` scope is active over a pruned pick: the empty state both panes
    /// paint as [`Self::commits_gone_message`].
    pub fn commits_gone(&self) -> bool {
        self.scope == Scope::Commits && self.pick_gone()
    }

    /// Whether the last build found the pick pruned, whatever scope is showing now: the
    /// verdict is adopted only in `commits` and kept across a switch away, so `g` from
    /// another scope knows the pick has nothing to show.
    pub(crate) fn pick_gone(&self) -> bool {
        matches!(self.pick_status, Some(PickStatus { verdict: PickVerdict::Gone(_), .. }))
    }

    /// The one message both panes paint for a [`Self::commits_gone`] frame, naming the first
    /// missing commit.
    pub fn commits_gone_message(&self) -> String {
        match &self.pick_status {
            Some(PickStatus { verdict: PickVerdict::Gone(sha), .. }) => {
                format!("commit {} is gone", git::abbreviate_oid(sha))
            }
            _ => String::new(),
        }
    }

    /// Where the open diff's new side was read: the picked run's
    /// newest commit in `commits`, the worktree everywhere else.
    fn current_rev(&self) -> Rev {
        match (self.scope, &self.commit_pick) {
            (Scope::Commits, Some(pick)) => Rev::Commit(pick.clone()),
            _ => Rev::Worktree,
        }
    }

    /// Whether the `last-turn` scope is active but no baseline has been captured yet — the
    /// cold-start state the UI paints as [`Self::turn_wait_message`].
    pub fn awaiting_turn(&self) -> bool {
        self.scope == Scope::LastTurn && self.turn_baseline.is_none()
    }

    /// The one message both panes paint for an [`Self::awaiting_turn`] frame, chosen here
    /// so the file list and the diff view cannot disagree. An empty
    /// worktree will never produce a turn, so saying so beats waiting — but only a sample
    /// that found no member says it, since the pre-poll frame may only wait: stale is
    /// allowed, wrong is not (Continuity).
    pub fn turn_wait_message(&self) -> &'static str {
        match self.agents_present {
            Some(false) => "no agent works here",
            _ => "waiting for the first turn",
        }
    }

    /// The membership mirror itself: `None` until a sample observes it. The UI reads only
    /// [`Self::turn_wait_message`], which paints `None` and `Some(true)` alike; this exposes the
    /// held-versus-empty distinction underneath, which the turn-tracking tests assert directly.
    pub fn agents_present(&self) -> Option<bool> {
        self.agents_present
    }

    /// Follow the worker's baseline. Every completion carries the authoritative value, so
    /// the mirror syncs even when the completion's snapshot is superseded or discarded.
    pub fn sync_turn_baseline(&mut self, baseline: Option<String>) {
        self.turn_baseline = baseline;
    }

    /// Follow what a sample saw. `None` is a sample that could not observe the whole worktree —
    /// herdr was unreachable, or a member's directory would not resolve — and so saw nothing,
    /// which holds the previous answer rather than replacing it. Like
    /// [`Self::sync_turn_baseline`], this lands even from a superseded completion — the
    /// worker is serial, so no completion can carry membership newer than a later one.
    pub fn sync_agents_present(&mut self, present: Option<bool>) {
        self.agents_present = present.or(self.agents_present);
    }

    /// Queue a world refresh for the event loop to dispatch after the frame paints.
    /// `sample` rides the poll's status sample along; `reveal` re-reveals the cursor when
    /// the result lands, for user-initiated switches only.
    pub fn request_world_refresh(&mut self, sample_turn: bool, reveal: bool) {
        let request = self.world_request.get_or_insert(crate::world::WorldRequest::default());
        request.sample_turn |= sample_turn;
        request.reveal |= reveal;
    }

    /// Snap the diff view back to the top, clearing any pending selection.
    fn reset_diff_view(&mut self) {
        self.diff_cursor = 0;
        self.diff_scroll = 0;
        self.h_scroll = 0;
        self.select_anchor = None;
    }

    /// Scroll the diff horizontally by `delta` columns, clamped at the left edge. A no-op
    /// while wrap is on, since the renderer ignores `h_scroll` when wrapping — so the offset
    /// never silently accumulates and then jumps the view when wrap is toggled off.
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

    /// Whether the open file qualifies for the rendered view: a `.md`/`.markdown`
    /// extension, case-insensitive.
    #[must_use]
    fn markdown_file(&self) -> bool {
        self.diff_path.as_deref().is_some_and(is_markdown_path)
    }

    /// Whether the read pane shows a markdown file rendered. A file renamed away from
    /// markdown, degraded to a notice, or rendering nothing shows its source without
    /// disarming the toggle.
    #[must_use]
    pub fn rendered_active(&self) -> bool {
        self.tab.is_file_tab() && self.rendered.on_screen()
    }

    /// The open markdown file's content for its rendered view: `None` for an empty text. Whether
    /// it renders nothing is read once per text — the last verdict when the text is the same,
    /// else a render of it (any width and open set show the same emptiness).
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

    /// Seed a fresh pane from its configuration — startup and config recovery: the markdown
    /// view it opens with. A running pane's reread goes through
    /// [`Self::set_plugin_config`] alone and never flips it.
    pub fn seed_from_config(&mut self, config: &crate::config::PluginConfig) {
        self.markdown_rendered = config.markdown_view() == crate::config::MarkdownView::Rendered;
    }

    /// Whether the `m` toggle acts here: a file tab holding current markdown content, filled
    /// only for a markdown file whose source rows render — a notice, a deleted file, an
    /// emptied changeset, or a rename away from markdown leaves it empty and the toggle
    /// inert.
    fn toggle_acts(&self) -> bool {
        self.tab.is_file_tab() && self.rendered.content.is_some()
    }

    /// `m`: flip the open markdown file between rendered and source, at the same block;
    /// inert anywhere else. A live line selection clears first, its rows are about to go.
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

    /// Flip the open file's view to `rendered`, the cursor crossing by its source line
    /// (`rebuild_visible`): rendered → source lands on the block's first line, source →
    /// rendered on the block holding the cursor's current line. The cursor holds its distance
    /// from the pane's top where the new rows allow; the old highlight goes with the old rows.
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

    /// Whether row `i` is a rendered block's lead row, the one whose gutter carries the
    /// block's source line number.
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

    /// Drop the painted frame's recorded regions — the links, the anchors, and the read
    /// pane's display-line layout; the renderer calls this each frame.
    pub(crate) fn clear_painted_frame(&self) {
        self.painted_links.borrow_mut().clear();
        self.painted_anchors.borrow_mut().clear();
        self.painted_details.borrow_mut().clear();
        self.painted_slots.borrow_mut().clear();
    }

    /// Record the read pane's painted display-line layout — the walk the paint performed
    /// (`ui::read_layout`), which the selection, gutter, and hover hit tests resolve
    /// against.
    pub(crate) fn note_painted_slots(&self, slots: Vec<crate::ui::Slot>) {
        *self.painted_slots.borrow_mut() = slots;
    }

    /// The recorded display-line layout of the frame on screen — empty when the last frame
    /// painted no read-pane rows (a notice, the `PR` tab).
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

    /// Act on a clicked link destination: a `#anchor` scrolls its
    /// own surface to the matching heading, an `http(s)` destination opens in the
    /// browser, and anything else is inert.
    pub fn open_link(&mut self, url: &str) {
        if let Some(fragment) = url.strip_prefix('#') {
            // The fragment runs through the same normalization that made the slugs, so
            // `#Set-Up!` and `#İstanbul` find their headings.
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

    /// Bring `slug`'s heading to the top of its markdown surface: the `PR` read pane scrolls
    /// to it, and the rendered file view moves its cursor there too — a click is the reader's
    /// own input, so the place may move. A missing anchor is inert.
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

    /// Render `text` as markdown wrapped to `width`, through the memo, with the `<details>`
    /// keyed in `open` opened.
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

    /// Render one body of the open PR thread — `body` is its place in
    /// [`Self::pr_markdown_bodies`]'s order. Each body keys its disclosures in its own
    /// namespace, so a reply's `Details#0` never opens with the description's.
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

    /// Open or close the `<details>` with this [`crate::markdown::DetailsHit::key`]. In a
    /// file tab that is the reviewer's own choice, held over the derived state from then on,
    /// and the rendered rows rebuild around it, keeping the place.
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

    /// Move clockwise and cancel any drag captured under the previous geometry. Inert while
    /// the navigator is hidden.
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

    /// Hide the navigator, or show it back in its kept position and share. Hiding moves focus
    /// to the read pane; showing leaves it there. Inert on `PR`.
    pub fn toggle_navigator_hidden(&mut self) {
        if !self.navigator_can_hide() {
            return;
        }
        self.cancel_divider_drag();
        self.navigator_hidden = !self.navigator_hidden;
        if self.navigator_hidden {
            self.focus = Focus::Diff;
        } else {
            // File reveals wait out the hidden state (the files viewport is zero);
            // request one now at the shown size.
            self.reveal_files = true;
        }
    }

    /// Grow or shrink the navigator by `delta` percentage points on the active split axis.
    /// Inert while the navigator is hidden.
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

    /// Set the search screen's results share from a captured drag on its divider. The
    /// review layout's shares are untouched.
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

    // --- Scroll model (shared by both panes) ---------------------------------------
    //
    // Each pane has a cursor (selection) and a scroll offset (viewport top). They are
    // independent: keyboard navigation moves the cursor and requests a reveal; the wheel
    // moves the offset and requests nothing. Every frame the event loop reveals the cursor
    // *only if a move requested it* (so the wheel can leave the cursor off screen) and then
    // bounds the offset (so an over-scroll never shows a blank tail). Both panes run the
    // same `keep_in_view` + `bound`; the file list passes all-height-1 rows.

    /// Scroll the file list so `file_cursor` is on screen — the minimal nudge. Called once
    /// per frame when a navigation requested a reveal, not on a wheel scroll.
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

    /// Scroll the diff so the reveal target's row fits the `viewport`-display-row window —
    /// `heights` is each visible row's display height (wrap + comment cards). Called once
    /// per frame when a navigation requested a reveal, not on a wheel scroll.
    ///
    /// The target is the cursor, except while composing: the box opens under the selection's
    /// last line ([`Self::compose_row`]), so that line is what has to stay in view. A selection built
    /// upward has its cursor at the top, and following the cursor there would leave the box
    /// off the bottom — the selection covers the same rows either way.
    pub fn reveal_diff_cursor(&mut self, heights: &[usize], viewport: usize) {
        if self.visible.is_empty() {
            self.diff_scroll = 0;
            return;
        }
        let target = if self.composing() { self.compose_row() } else { self.diff_cursor };
        let target = target.min(self.visible.len() - 1);
        self.diff_scroll = keep_in_view(target, self.diff_scroll, heights, viewport);
    }

    /// Center the cursor's row in the read pane when it sits off screen; leave the scroll when
    /// it is already in view. Row heights decide both, so a wrapped row counts its full height.
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

    /// Settle the read pane's scroll for this frame: a jump to a named line centers, any other
    /// navigation (or an open composer) nudges the cursor into view, then the offset bounds. A
    /// center consumes a nudge asked in the same event, which would pin the line to the edge.
    pub fn settle_diff_scroll(&mut self, heights: &[usize], viewport: usize) {
        let nudge = std::mem::take(&mut self.reveal_diff);
        if std::mem::take(&mut self.reveal_center) {
            self.center_diff_cursor(heights, viewport);
        } else if nudge || self.composing() {
            self.reveal_diff_cursor(heights, viewport);
        }
        self.bound_diff_scroll(heights, viewport);
    }

    /// Clamp `diff_scroll` within range (no blank tail). Called every frame. Height-aware:
    /// the cap is the offset that shows the LAST row at the bottom — computed from `heights`,
    /// not the row count, so a wrapped diff (tall rows) stays fully reachable. A row-count cap
    /// would stop short of the bottom whenever rows span more than one display line.
    pub fn bound_diff_scroll(&mut self, heights: &[usize], viewport: usize) {
        if heights.is_empty() {
            self.diff_scroll = 0;
            return;
        }
        let max_top = keep_in_view(heights.len() - 1, self.diff_scroll, heights, viewport);
        self.diff_scroll = self.diff_scroll.min(max_top);
    }

    /// The scope the header chip's click moves to: the next in the cycle, skipping `commits`
    /// while no pick exists or it is `gone`, so a mouse is never parked on a picker it keeps
    /// reopening.
    pub fn next_chip_scope(&self) -> Scope {
        let next = self.scope.cycle();
        if next == Scope::Commits && (self.commit_pick.is_none() || self.pick_gone()) {
            return next.cycle();
        }
        next
    }

    /// Switch the changeset scope and reload. A no-op while composing, so a comment
    /// in progress is never stranded against a different diff.
    pub fn set_scope(&mut self, scope: Scope) -> Result<()> {
        self.ensure_config_ready()?;
        // `commits` with no pick, or a `gone` one, has nothing to show: the picker opens
        // instead, without switching, so `esc` leaves this scope active.
        if scope == Scope::Commits
            && (self.commit_pick.is_none() || self.pick_gone())
            && !self.composing()
        {
            self.open_commit_picker();
            return Ok(());
        }
        if self.scope != scope && !self.composing() {
            self.scope = scope;
            self.rebase_changes()?;
            // An explicit switch reveals the cursor (a poll does not).
            self.reveal_files = true;
        }
        Ok(())
    }

    /// Rebuild the views after the changeset's base moved — a scope switch or a base pick.
    /// The change replaces the Changes changeset (and each file's old side), so the Changes
    /// tab snaps to the top: reset its cursor, folds, and diff scroll, and drop cached diffs.
    /// The `All files` listing and File view are base-independent (only the annotations
    /// move), so its own state is held by `reload`. The Changes state is the active one on
    /// `Changes` and the stashed one while `All files` is shown — reset whichever holds it,
    /// so a return to Changes never lands on a stale scroll or a pre-expanded fold.
    fn rebase_changes(&mut self) -> Result<()> {
        self.cache = DiffCache::new();
        if self.tab == Tab::Changes {
            self.file_cursor = 0;
            self.expanded_folds.clear();
            self.reset_diff_view();
            // The changed set rebuilds before the frame, so the list never shows another
            // base's files under the new base's label. In `Changes` the
            // changeset is the whole snapshot, so this is the full (cheap) reload.
            self.reload()?;
        } else {
            self.stash.file_cursor = 0;
            self.stash.expanded_folds.clear();
            self.stash.diff_cursor = 0;
            self.stash.diff_scroll = 0;
            self.stash.h_scroll = 0;
            self.stash.select_anchor = None;
            // `All files` keeps its tree; only the changed set rebuilds before the frame.
            // The tree's annotations refresh behind it via the worker.
            let build = crate::world::build_changed(&self.world_input())?;
            self.adopt_branch_base(build.branch_base);
            self.adopt_pick_status(build.pick_status);
            self.changed = crate::world::annotate(&build.changed);
            // Re-mark the tree in place — the rows are base-independent, only their
            // badges move, so the switch frame never shows the old base's badges
            // under the new base's header (policies/ux-responsiveness.md). The tree
            // itself still refreshes behind the switch.
            for entry in &mut self.entries {
                entry.annotation = self.changed.get(&entry.path).cloned();
            }
            self.rebuild_file_rows();
            self.request_world_refresh(false, false);
        }
        Ok(())
    }

    /// Queue a PR refresh, merging into any request already pending: the stronger kind
    /// wins, so an ambient trigger can never downgrade the user's commanded refresh.
    pub fn request_pr_refresh(&mut self, kind: RefreshKind) {
        self.pr_pending = self.pr_pending.max(Some(kind));
    }

    /// Switch to `tab`, saving the active tab's navigator and read-pane state and restoring the
    /// target's. Each tab keeps its own opened file and scroll, so returning to a tab lands
    /// exactly where you left it. The switch frame paints the restored state as
    /// it was; a world refresh lands behind it — stale until it lands, never wrong
    /// (Continuity). A no-op on the active tab or while composing; focus
    /// stays on the same side.
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
        // Entering the PR tab leaves the file tabs frozen in place and fetches the PR. A
        // `loading` frame draws before the blocking fetch the event loop services, and a
        // re-entry keeps the last snapshot on screen while it refetches.
        if tab == Tab::Pr {
            self.request_pr_refresh(RefreshKind::Ambient);
            return Ok(());
        }
        // Entering a file tab: bring its state into the diff fields if the other file tab holds
        // them (a Changes↔AllFiles switch, or a return from PR onto the stashed tab).
        if self.active_file_tab != tab {
            self.swap_active_with_stash();
            self.active_file_tab = tab;
            // The pane's markdown choice may have flipped while this tab was away: its rows
            // follow it, crossing by source line like any flip.
            if self.rendered.on_screen() != self.wants_rendered()
                && !self.rendered.renders_nothing()
            {
                self.rebuild_visible();
                self.settle_read();
            }
        }
        // A first visit has no stash to paint: refreshing behind would show an empty tree
        // under a live changed-count, a header/body disagreement
        // (policies/ux-responsiveness.md). Load it before the frame instead; every return
        // visit paints its stash instantly and refreshes behind it. The visited marker,
        // not emptiness — a clean repo's `Changes` tab is legitimately empty.
        if self.tab_visited {
            self.request_world_refresh(false, true);
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

    /// An empty read pane — a first visit landing on a collapsed tree, or an open file gone
    /// empty — focuses the tree, so the cursor keys aren't trapped on a pane with nothing to
    /// move. Runs on the switch frame and again when its world refresh lands.
    pub(crate) fn settle_tab_entry(&mut self) {
        if self.navigator_hidden_here() {
            // A `PR` visit may have focused its always-shown navigator; entry restores
            // the hidden-state invariant.
            self.focus = Focus::Diff;
            return;
        }
        if self.visible.is_empty() {
            self.focus = Focus::Files;
        }
    }

    // ---- PR tab -------------------------------------

    /// Blank a settled highlight anchored to a `PR` surface: the tab's paint is being
    /// replaced or emptied, so the span's text is leaving the screen
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

    /// Clear a snapshot whose complete fetch input no longer matches the worktree. A gesture
    /// over the `PR` tab gates the drains this runs from, so it never fires under a live drag
    /// (`lib.rs`).
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

    /// Apply a snapshot fetched off-thread (`forge::fetch` runs on a worker so the UI never
    /// blocks — `lib.rs`). A transient `Error` keeps the last good snapshot frozen with a status
    /// note, so a failed poll never blanks a populated tab; the cursor clamps to the new rows.
    pub fn apply_pr(&mut self, view: forge::PrView) {
        self.pr_refreshing = false;
        let retry = view.retry_remedy(self.keymap().hint(crate::keymap::Action::Refresh));
        let has_snapshot =
            matches!(self.pr, forge::PrView::Pr(_) | forge::PrView::NoPr | forge::PrView::Detached);
        if has_snapshot && let Some(message) = retry {
            self.pr_notice = Some(message);
            return;
        }
        // A held resolution keeps the painted story, and so does a transient detach
        // while a snapshot is on screen.
        if matches!(view, forge::PrView::Held)
            || (matches!(view, forge::PrView::Detached) && matches!(self.pr, forge::PrView::Pr(_)))
        {
            self.pr_notice = None;
            return;
        }
        self.pr_notice = None;
        // The snapshot replaces the painted `PR` surfaces, so a settled highlight on them
        // would point at replaced text — blank it. The early returns above keep the old
        // paint, and keep the highlight with it.
        self.blank_pr_settled();
        // Follow the selected row by identity, not index, so a refresh that inserts a newer
        // comment (the list is newest-first) keeps the cursor on the same one and leaves the read
        // scroll intact — only a vanished or absent selection resets it (mirrors the file tabs'
        // poll-preservation). The pinned description row's identity is itself:
        // it survives while the new snapshot still has a description, and an emptied one
        // vanishes like a deleted comment.
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
            // The selection vanished (or there was none): clamp the cursor into range,
            // and reset the read pane whenever a selected row disappeared — the pane now
            // shows a different row.
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

    /// Whether the snapshot carries a PR description — the pinned `description` row's
    /// existence condition.
    #[must_use]
    pub fn pr_has_description(&self) -> bool {
        self.pr_snapshot().is_some_and(|s| !s.body.trim().is_empty())
    }

    /// Whether the navigator cursor sits on the pinned `description` row.
    #[must_use]
    pub fn pr_on_description(&self) -> bool {
        self.pr_has_description() && self.pr_cursor == 0
    }

    /// How many cursor rows the pinned description occupies before the comments — the
    /// one home for the comment-index ↔ cursor-index shift every consumer applies.
    #[must_use]
    pub fn pr_description_offset(&self) -> usize {
        usize::from(self.pr_has_description())
    }

    /// The navigator's cursor count: the pinned description row (when the PR has one)
    /// plus the comments. Checks are a status display, not a cursor stop — landing on
    /// one shows nothing the row itself doesn't.
    #[must_use]
    pub fn pr_row_count(&self) -> usize {
        self.pr_snapshot().map_or(0, |s| s.comments.len() + self.pr_description_offset())
    }

    /// The comment under the navigator cursor, for the read pane. `None` on the pinned
    /// description row ([`Self::pr_on_description`]) and in a degraded view.
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

    /// Select navigator row `i`, resetting the read pane to the top — the one place the
    /// cursor-move and the read-scroll reset stay paired (a click and `j`/`k` share it).
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

    /// Scroll the read pane by `delta` lines (the wheel and `PageUp`/`PageDown`), stopping
    /// with the last line at the pane's bottom edge. The base clamps first, so a stale
    /// scroll (the pane grew, or the body shrank) never swallows the first upward input.
    pub(crate) fn pr_scroll_read(&mut self, delta: isize) {
        self.pr_read_scroll =
            clamp_scroll(self.pr_read_scroll, delta, self.pr_read_max_scroll.get());
    }

    /// Open the pull request in the browser. A resolved PR always carries a
    /// `url`, so there is nothing to guard against.
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

    /// Exchange the active per-tab fields with the inactive tab's saved snapshot. Every per-tab
    /// field on `App` must be swapped here — a new per-tab field left out silently bleeds one
    /// tab's selection or scroll into the other.
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

    /// While the navigator is hidden, `tab` shows it and focuses it instead of flipping
    /// between panes.
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

    /// Move the cursor in the focused pane by `delta` rows. In the files pane the cursor steps
    /// over the tree's visible rows; landing on a file row opens its diff, while a directory row
    /// keeps the current diff so scanning the tree never blanks the pane. The page/half-page keys
    /// reuse this with a larger `delta`, since paging is just a bigger cursor move in the focus.
    pub fn move_cursor(&mut self, delta: isize) -> Result<()> {
        self.ensure_config_ready()?;
        match self.focus {
            Focus::Files => {
                if !self.file_rows.is_empty() {
                    self.file_cursor = step(self.file_cursor, delta, self.file_rows.len());
                    self.open_cursor_file();
                    // Reveal even when the index clamps unchanged (e.g. `k` at the top), so a
                    // navigation always pulls the cursor back after a wheel scroll.
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

    /// Open the diff for the file under the cursor when it differs from the one shown; a
    /// no-op on a directory row, so the current diff stays put.
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

    /// Move the file cursor to the nearest file row and open it, keeping the focused pane. The
    /// cursor carries the selection with it, so the list always highlights the open file.
    ///
    /// The list steps from its own cursor, which is what the reviewer is moving there. The diff
    /// steps from the open file, so a press always opens a file — the cursor may sit elsewhere,
    /// parked on a directory row (which keeps the open diff).
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

    /// Move the diff cursor to the nearest hunk's first changed row past it. With no hunk left
    /// this way, the first press arms the crossing and the second one takes it, so a held key
    /// stops at each file. Only `Changes` paints change rows — `All files` is all context — so
    /// a step anywhere else has no target.
    fn step_hunk(&mut self, forward: bool) {
        // Any step drops the standing arm. A step the other way is not the repeat it waits for.
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
            // The first press resolves the crossing and arms it, so the footer can offer it and
            // a held key stops at the file boundary. With no file to cross to — the changeset's
            // end — nothing is offered and the press is inert.
            if let Some(row) = self.cross_target(forward)
                && let Some(path) = self.path_of_row(row)
            {
                self.armed_cross = Some(ArmedCross { forward, path });
            }
            return;
        };
        // The armed file is normally still there, since a poll that changes the open diff
        // disarms. A poll that dropped the armed file alone leaves the crossing to re-resolve.
        let Some(row) = self.file_row_of_path(&armed.path).or_else(|| self.cross_target(forward))
        else {
            return;
        };
        self.file_cursor = row;
        self.open_cursor_file();
        // The landing hunk reads off the rows now on screen, so a file reshaped since the arm
        // still lands on a real change.
        self.diff_cursor = hunk_row(&self.visible, None, forward).unwrap_or(0);
        self.reveal_files = true;
        self.reveal_diff = true;
    }

    /// The row of the nearest file a crossing would open: the first one that has a hunk. A file
    /// with no hunk — a binary, a pure rename, an over-budget notice — is crossed over, so a
    /// crossing always lands on a change. `None` when no such file lies that way.
    fn cross_target(&mut self, forward: bool) -> Option<usize> {
        // From the open file, never the file cursor: parked on a directory row above the open
        // file, the cursor would find that same file again and wrap the diff to its first hunk.
        let mut row = self.open_file_row();
        while let Some(next) = self.file_row_from(row, forward) {
            row = next;
            let i = self.file_rows[row].file_index().expect("file_row_from yields file rows");
            let entry = self.entries[i].clone();
            // Cross over the files git already counted as having no lines — a binary, a pure
            // rename — without reading them. The reload's `--numstat` knows (`file_list.rs`), so
            // a keystroke that only passes a file by spends no git on it.
            if entry.annotation.as_ref().is_some_and(|a| a.additions + a.deletions == 0) {
                continue;
            }
            // An over-budget file renders a notice, so it holds no hunk either. Check the size
            // before reading, as `set_file_view` does: pulling a vendored bundle in whole would
            // spike the UI thread for a file the reviewer only crosses over.
            if std::fs::metadata(self.repo.join(&entry.path))
                .is_ok_and(|m| crate::diff::over_byte_budget(m.len() as usize))
            {
                continue;
            }
            let (old, new) = self.content_sides(&entry.path, entry.previous_path.as_deref());
            let diff =
                self.cache.get(entry.path, entry.previous_path, &old, &new, &self.highlighter);
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

    /// Toggle the footer's `?` shortcut list. Called only from `Normal` mode, so a modal's `?` stays
    /// text or inert.
    pub fn toggle_keys(&mut self) {
        self.keys_expanded = !self.keys_expanded;
    }

    /// The `esc` ladder in `Normal` mode: peel exactly one layer per press — a live selection, then
    /// an armed crossing, then the footer expansion. The selection and crossing
    /// are file-tab place state, frozen in place while `PR` is active, so `esc` on `PR` closes only
    /// the expansion and never disturbs the file tab the reviewer will return to (Continuity).
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

    /// Whether the traversal keys act at all: a live selection holds the cursor still, since a
    /// jump would silently drop the selection under it.
    fn can_traverse(&self) -> bool {
        self.plugin_config().is_some() && self.select_anchor.is_none()
    }

    /// The open file's row, the origin of every traversal the diff drives. Falls back to the
    /// cursor when the open file has no visible row, as a file opened from a collapsed
    /// directory does.
    fn open_file_row(&self) -> usize {
        self.diff_path
            .as_deref()
            .and_then(|path| self.file_row_of_path(path))
            .unwrap_or(self.file_cursor)
    }

    /// The visible-row index of the nearest file row past `row`, in `forward`'s direction.
    /// Directory rows are skipped. `None` when no file lies that way, which is how both
    /// traversals clamp at the changeset's ends.
    fn file_row_from(&self, row: usize, forward: bool) -> Option<usize> {
        let is_file = |i: &usize| self.file_rows[*i].file_index().is_some();
        if forward {
            (row + 1..self.file_rows.len()).find(is_file)
        } else {
            (0..row).rev().find(is_file)
        }
    }

    /// Act on the file-list row at `index` (a mouse click): a file opens its diff, a
    /// directory toggles its expansion.
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

    /// Collapse or expand the directory under the cursor, then rebuild the tree. The cursor
    /// stays on the directory row (still present, now toggled).
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

    /// Whether the cursor is on a directory row in the focused file list — the rows `←`/`→`
    /// collapse and expand (elsewhere those keys scroll the diff).
    pub fn on_folder(&self) -> bool {
        self.focus == Focus::Files
            && self.file_rows.get(self.file_cursor).is_some_and(|r| r.dir_path().is_some())
    }

    /// Whether the diff cursor is on a fold row — the row `→` expands (elsewhere `→` scrolls
    /// the diff sideways). Folds are expand-only, so `←` never collapses one.
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
        // In `All files`, expanding an ignored directory loads its children lazily, so the
        // entry set is rebuilt before the rows. Other tabs just re-flatten.
        if self.tab == Tab::AllFiles
            && let Ok(entries) = crate::world::all_files_entries(&self.world_input(), &self.changed)
        {
            self.entries = entries;
        }
        self.rebuild_file_rows();
        self.file_cursor = self.file_cursor.min(self.file_rows.len().saturating_sub(1));
        self.reveal_files = true; // the row may have moved off-screen; pull it back
    }

    /// Wheel-scroll the diff's viewport, leaving `diff_cursor` (the comment anchor) put —
    /// so wheeling to read context never moves what a comment will attach to. The upper
    /// bound is applied each frame by `bound_diff_scroll`.
    pub fn wheel_diff(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        self.diff_scroll = offset_by(self.diff_scroll, delta);
    }

    /// Wheel-scroll the file list's viewport, leaving the selection and the open diff
    /// untouched — so browsing the list never reloads a diff. Bounded each frame.
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

    /// Clamp `target` so the inclusive range from `anchor` to `target` crosses no fold: a
    /// selection treats a fold as a hard boundary, so its line range and snippet always agree
    /// (never bracketing hidden lines the snippet omits). Stops the moving end shy of the fold.
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

    /// Settle a completed copy: the span stays highlighted as feedback, with its copied text
    /// kept so a refresh can tell stale from wrong.
    pub(crate) fn settle_selection(&mut self, drag: crate::selection::TextDrag, text: String) {
        self.settled_sel = Some((drag, text));
    }

    /// Clear the settled selection: the user did something else.
    pub(crate) fn clear_settled_selection(&mut self) {
        self.settled_sel = None;
    }

    /// Whether the live gesture anchors to the open view's rows: a landing snapshot holds
    /// the view's reload while it lives, exactly as composing does.
    /// Whether world events — a poll, a resize — leave the open view alone: under a modal, so
    /// a draft's anchor holds and the place holds while the reviewer chooses, and under a
    /// view-anchored gesture. The reviewer's own input moves it regardless (Continuity).
    fn view_frozen(&self) -> bool {
        self.mode.is_modal() || self.view_anchored_gesture()
    }

    fn view_anchored_gesture(&self) -> bool {
        use crate::selection::Surface;
        matches!(self.gesture, crate::selection::Gesture::Gutter)
            || self.text_drag().is_some_and(|d| match d.surface {
                Surface::Read | Surface::Card { .. } => true,
                // The painted surface is the `PR` read pane, which holds through the gated
                // PR drains instead.
                Surface::Painted | Surface::Files | Surface::PrNav => false,
            })
    }

    /// Whether the live gesture anchors to the file navigator's rows, which a snapshot
    /// rebuilds — the event loop holds the world drain while it lives, so the completion
    /// waits in its channel.
    #[must_use]
    pub fn gates_world_drain(&self) -> bool {
        use crate::selection::Surface;
        // Exhaustive, so a new surface must pick its freeze here rather than silently
        // landing snapshots under a live gesture.
        self.text_drag().is_some_and(|d| match d.surface {
            Surface::Files => true,
            Surface::Read | Surface::Card { .. } | Surface::Painted | Surface::PrNav => false,
        })
    }

    /// Whether the live gesture anchors to the `PR` tab's fetched result — the event loop
    /// holds the PR drains the same way.
    #[must_use]
    pub fn gates_pr_drain(&self) -> bool {
        use crate::selection::Surface;
        self.text_drag().is_some_and(|d| match d.surface {
            Surface::PrNav => true,
            Surface::Painted => self.tab == Tab::Pr,
            Surface::Read | Surface::Card { .. } | Surface::Files => false,
        })
    }

    /// Forget where the pointer was.
    ///
    /// Mouse reporting stops while an external program owns the terminal, so no release and no
    /// motion will arrive to end a drag or move the hover off the cell it was painted on
    pub(crate) fn forget_pointer(&mut self) {
        self.cancel_gesture();
        self.finish_divider_drag();
        self.hover = None;
    }

    /// End the live gesture without a copy: the reflow-input cancel, and the dissolve of a
    /// press that never moved or a lost gutter drag. The multi-click chain resets — only a
    /// gesture's own release continues it — and the freeze lifts.
    pub(crate) fn cancel_gesture(&mut self) {
        if !self.gesture_active() {
            return; // nothing live: the multi-click chain survives unrelated input
        }
        if matches!(self.gesture, crate::selection::Gesture::Gutter) {
            self.select_anchor = None;
        }
        self.gesture = crate::selection::Gesture::None;
        self.last_click = None;
        self.lift_gesture_freeze();
    }

    /// Lift the ended gesture's freeze: the reload a view-anchored gesture held runs now;
    /// the gated completion channels drain on the event loop's next pass
    pub(crate) fn lift_gesture_freeze(&mut self) {
        // Skip (and drop the bit) when the gesture ended into the composer: the composing
        // freeze owns the view from here, and its own exit catches up.
        if std::mem::take(&mut self.view_reload_held) && !self.mode.is_modal() {
            // The held reload's rebuild revalidates a just-settled span against its text.
            self.reload_open_view();
        }
    }

    /// Note a mouse-down for the multi-click chain: the count at this cell within the window
    /// (1 = single, 2 = double, 3 = triple; further clicks within the window stay a triple).
    /// `target` is the row the cell maps to in its surface, so a scroll that slides new
    /// content under the pointer breaks the chain rather than double-clicking what the user
    /// never aimed at.
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

    /// Start a gutter comment gesture on `row`: line cursor there, selection cleared, the
    /// range extended by drag and the composer opened on release.
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

    /// Finish a gutter gesture at its release: open the composer on the selected line or
    /// range.
    pub fn finish_gutter_drag(&mut self) {
        if !matches!(self.gesture, crate::selection::Gesture::Gutter) {
            return;
        }
        self.gesture = crate::selection::Gesture::None;
        // The composer replaces the find band, so a gutter release while finding is a
        // forced return — without it the band's highlight would keep painting with no way
        // left to close it.
        self.close_find();
        self.start_comment();
        self.lift_gesture_freeze();
    }

    /// Copy `text` to `target` (the clipboard at runtime, injected like [`Self::export`]):
    /// `copied N chars` on success, the loud failure status otherwise
    pub fn copy_selection_text(&mut self, target: &dyn crate::export::ExportTarget, text: &str) {
        if text.is_empty() {
            return;
        }
        match target.export(text) {
            Ok(()) => self.status = crate::selection::copied_status(text),
            Err(e) => {
                crate::logln!("selection copy failed: {e:#}");
                self.status = target.failure_message(&e);
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

    /// `edit`: the comment under the cursor, else the file the cursor names
    /// One key, resolved by what is actually there.
    pub fn start_edit(&mut self) {
        if self.comment_claims_edit() {
            self.edit_comment();
            return;
        }
        self.editor_request = self.edit_target();
    }

    /// Whether a comment takes the key here, rather than the file.
    ///
    /// A comment claims it only where its card is on screen. With the navigator focused the
    /// diff cursor is off screen, so the file row under the eye wins. A live line selection is
    /// a gesture in progress on the diff, so nothing on the diff claims the key and the range
    /// survives — the comments list, which owns the screen instead, still claims it
    fn comment_claims_edit(&self) -> bool {
        let on_the_diff =
            self.tab.is_file_tab() && self.focus == Focus::Diff && self.select_anchor.is_none();
        let claimed =
            if self.mode == Mode::List { self.list_comment_editable() } else { on_the_diff };
        claimed && self.target_comment().is_some()
    }

    /// Whether the list's highlighted comment can be edited here: only while the active
    /// scope reads the diff it was made on, so the edit box opens over its card and never
    /// over a same-numbered line of another revision.
    fn list_comment_editable(&self) -> bool {
        self.mode == Mode::List
            && self.store.get(self.list_cursor).is_some_and(|c| self.rev_is_current(c))
    }

    /// Whether `edit` opens a file here. The branch [`Self::start_edit`] takes, asked by the
    /// footer, so the bar and the press cannot disagree.
    fn edit_opens_a_file(&self) -> bool {
        !self.comment_claims_edit() && self.edit_target().is_some()
    }

    /// The file `edit` opens and the line to open it at, or `None` where the key opens nothing.
    ///
    /// Pure and total. The press acts on it and the footer offers the key exactly when it is
    /// `Some`, so the two cannot disagree, and no surface can be reached without passing
    /// through here. Whether the file is still on disk is the press's
    /// question, asked in `run_editor`: the footer asks this one twice a frame.
    fn edit_target(&self) -> Option<EditTarget> {
        // The `PR` tab names no file of its own, and the open diff behind it belongs to a
        // file tab the reviewer left.
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
            // A live line selection is a gesture in progress on the diff, and a press must not
            // abandon the range. The navigator's own row is untouched by it
            if self.select_anchor.is_some() {
                return None;
            }
            // The open file, never the navigator's selection: the two diverge whenever a file
            // was opened by path rather than by row. A rendered row names its block's first
            // source line. In `commits` the numbers belong to the picked commit, not the
            // worktree file the editor opens, so it opens the file at its start.
            let commit_diff = self.scope == Scope::Commits && self.diff.view == View::Diff;
            (self.diff_path.clone()?, !commit_diff)
        };
        let line = if !numbered {
            1
        } else if self.rendered_active() {
            // Rendered, a block opens at its first source line, and a marker row at the line
            // it sits at — past the file's end for a deletion at its tail, so that clamps.
            let last = self.rendered.text().unwrap_or_default().lines().count().max(1) as u32;
            match self.rendered.index.unit_at(self.diff_cursor).map(|u| u.unit) {
                Some(Unit::Block(src)) => src,
                Some(Unit::Marker(src, _)) => src.clamp(1, last),
                None => 1,
            }
        } else {
            // The nearest row at or above the cursor carrying a worktree line number. A
            // deletion and a fold carry none, and a notice diff paints no rows at all, so each
            // falls back to the file's start.
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

        // Bring the comment's file into the diff and land the cursor on its line, so the
        // inline edit box opens over the comment — even when editing from the list, and even
        // when the file's row is hidden inside a collapsed directory (load it by path, not by
        // tree row). Move the list cursor onto its row when one exists.
        if self.diff_path.as_deref() != Some(file.as_str())
            && let Some(e) = self.entries.iter().find(|e| e.path == file).cloned()
        {
            self.reset_diff_view();
            // Open it in the active tab's view — the File view on `All files`, not a diff — so
            // the pane and the comment's anchor kind stay consistent with the tab.
            self.open_path_in_tab(e.path, e.previous_path);
            if let Some(fi) = self.file_row_of_path(&file) {
                self.file_cursor = fi;
            }
        }
        // Only move the cursor when the open diff is actually the comment's file, so a
        // stale comment (file gone from the changeset) never jumps the cursor onto a
        // same-numbered line in a different file, and a comment from another view (a commit
        // comment under a worktree scope) never lands on the same-numbered worktree line
        // Land on the row the card splices under (`card_rows`) — the range's last row, or
        // rendered its block's last row — so the edit box opens in the card's place, in
        // whichever view shows the file.
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

    // --- text editing: a character caret into the active field ----------------------------
    // The comment editor and the search input share one control set. `caret` is a char index in `0..=text.chars().count()`. Edits
    // round-trip through a `Vec<char>` (both fields are short), so every op is
    // character-wise and multi-byte safe.

    /// The mode's editable text and caret: the comment draft while composing, the search
    /// query, the find query, the base picker's filter, nothing otherwise.
    fn active_field(&mut self) -> Option<(&mut String, &mut usize)> {
        match self.mode {
            Mode::Composing { .. } => Some((&mut self.input, &mut self.caret)),
            Mode::Search => self.search.as_mut().map(|s| (&mut s.query, &mut s.caret)),
            Mode::Find => self.find.as_mut().map(|f| (&mut f.query, &mut f.caret)),
            Mode::BasePick => self.base_picker.as_mut().map(|b| (&mut b.query, &mut b.caret)),
            Mode::Normal | Mode::List | Mode::Picker | Mode::CommitPick => None,
        }
    }

    /// Run a character-wise edit on the active field: collect it into a `Vec<char>` with
    /// the caret as an in-range index, hand both to `f`, then reassemble and re-clamp the
    /// caret. Every mutating `input_*` op routes through here, so the guard / collect /
    /// reassemble lives once instead of seven times. A changed search query re-queries
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

    /// Re-seat the base picker's highlight after a filter edit: it follows its own row into
    /// the narrowed view when the row survives, else rests on the first match
    /// (Continuity). A non-empty query no row spells exactly schedules a revision probe,
    /// whatever the fuzzy matches: `v1.2` must reach the tag even beside `v1.2-hotfix`.
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

    /// Move the caret with a function of the current `Vec<char>` view; a no-op without an
    /// active field. The read-only sibling of [`edit_input`](Self::edit_input).
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

    /// Insert pasted `text` at the caret as one unit, normalizing `\r\n`/`\r` to `\n`. The
    /// single-line search and find queries take a newline as a space; the
    /// base picker's filter drops it, so a branch name pasted with the newline it was copied
    /// with still matches its branch. The line field takes a number or `$` from it instead.
    pub fn input_paste(&mut self, text: &str) {
        if self.line_open() {
            self.paste_line(text);
            return;
        }
        let mut norm = text.replace("\r\n", "\n").replace('\r', "\n");
        match self.mode {
            Mode::Search | Mode::Find => norm = norm.replace('\n', " "),
            // No branch name holds a newline, so a name pasted with the one it was copied
            // with filters as the bare name rather than matching nothing.
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

    /// Delete the word before the caret (`Ctrl+W`): the trailing whitespace, then the run of
    /// non-whitespace before it, so one press clears one word.
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

    /// Leave compose mode, returning to the comments-list overlay if the compose was opened
    /// from it (and any comments remain), else to Normal.
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

    /// Save the in-progress comment — editing the existing one or anchoring a new one
    /// to the selection — then leave compose mode. Blank text cancels instead.
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

    /// Re-derive the rendered rows after the reviewer's own comment change: a disclosure a
    /// comment sits in opens or closes now, on this input, never on a later unrelated poll
    /// (Continuity) — the reviewer's own input, so no freeze holds it ([`Self::view_frozen`]).
    /// The place reconciles by identity, as on any rebuild.
    fn refresh_rendered(&mut self) {
        if self.rendered.on_screen() {
            self.rebuild_visible();
            self.settle_read();
        }
    }

    /// Whether the selection has at least one content row a comment can attach to —
    /// a fold marker does not qualify.
    fn has_anchorable_selection(&self) -> bool {
        if self.rendered.on_screen() {
            return self.selection_anchor().is_some();
        }
        let (lo, hi) = self.selection_range();
        self.visible.get(lo..=hi).is_some_and(|s| s.iter().any(Row::is_content))
    }

    /// The `(side, start, end, snippet)` the current selection anchors to. A rendered
    /// selection anchors through the source diff rows it stands for, so the same `anchor()`
    /// makes both views' comments (G1).
    fn selection_anchor(&self) -> Option<(Side, u32, u32, String)> {
        if self.rendered.on_screen() {
            return anchor(&self.rendered_anchor_rows());
        }
        let (lo, hi) = self.selection_range();
        anchor(self.visible.get(lo..=hi)?)
    }

    /// The source diff rows a rendered selection stands for, in diff order: every new-side
    /// line from its first block's start to its last block's end — the blank and unrendered
    /// lines between blocks included, so the agent sees what the comment spans — the
    /// deletions inside that span, and every change its blocks and markers own
    /// ([`MarkMap::anchors`]). A marker row stands for its changed lines alone, so a removed
    /// marker anchors the deleted lines on the old side. A comment anchored on these rows
    /// equals the source comment on the same rows, export included.
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

    /// The row the composer splices under: the selection's last row, and in the rendered
    /// view that row's unit's last row — where the comment's card will sit.
    #[must_use]
    pub fn compose_row(&self) -> usize {
        let (_, hi) = self.selection_range();
        if !self.rendered_active() {
            return hi;
        }
        self.rendered.index.unit_at(hi).map_or(hi, |u| u.end - 1)
    }

    fn build_comment(&self, text: String) -> Option<Comment> {
        // Anchor to the file the open diff belongs to (`diff_path`), not the file-list
        // selection — they diverge if the list shifts under a comment in progress.
        let file = self.diff_path.clone()?;
        let (side, start, end, lines) = self.selection_anchor()?;
        // The File view marks every comment as content-anchored, so it ages by file existence,
        // not changeset membership.
        let diff_anchored = self.diff.view == View::Diff;
        // A content comment reads the worktree whatever the scope.
        let rev = if diff_anchored { self.current_rev() } else { Rev::Worktree };
        Some(Comment { file, side, start, end, lines, text, diff_anchored, rev })
    }

    /// The `path:line` the composer is anchored to (selection for a new comment,
    /// the existing location when editing). `None` when not composing.
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

    /// Whether comment `c` anchors to the pane's current view — a diff comment to the Diff view,
    /// a content comment to the File view. Stops a comment of one kind rendering on, or being
    /// acted on at, an unrelated line in the other tab's view of the same file (the diff's line
    /// numbering and the File view's worktree line numbering differ).
    /// A diff comment also renders only while the scope reads its new side from the comment's
    /// `rev`, so a commit comment never lands on a worktree line.
    fn comment_in_view(&self, c: &Comment) -> bool {
        if c.diff_anchored != (self.diff.view == View::Diff) {
            return false;
        }
        self.rev_is_current(c)
    }

    /// Whether the active scope reads the diff `c` was made on: a worktree comment under any
    /// worktree scope, a commit comment under its own pick. Compared in place, since the
    /// render path asks per row and comment.
    fn rev_is_current(&self, c: &Comment) -> bool {
        match &c.rev {
            Rev::Worktree => {
                !c.diff_anchored || self.scope != Scope::Commits || self.commit_pick.is_none()
            }
            Rev::Commit(p) => self.scope == Scope::Commits && self.commit_pick.as_ref() == Some(p),
        }
    }

    /// Each comment shown on the open file, by store index, with the visible rows it covers:
    /// a source row by its line number on the comment's side, a rendered row by its unit
    /// ([`Self::rendered_cover`]). The one comment→row map the marks, the cards, and the
    /// cursor's comment all read, so the views cannot disagree about where a comment sits.
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

    /// The rendered units (by position in the index) comment `c` covers. A new-side comment:
    /// the blocks and the don't-render markers its range overlaps, else the unit its first
    /// line lands on. An old-side comment: the unit owning its last deleted row
    /// ([`MarkMap::owner`]) — a block, or the marker standing for it — the same owner its
    /// anchor reads, or, once that line is back as context, the unit it lands on. Never empty
    /// over rendered rows, so the rendered view never hides a comment (G3). `lines` is the
    /// diff in order, for an old-side comment.
    fn rendered_cover(&self, c: &Comment, lines: &[&Row]) -> Vec<usize> {
        let index = &self.rendered.index;
        match c.side {
            Side::New => index.new_side_cover(c.start, c.end),
            Side::Old => {
                let last = lines
                    .iter()
                    .rposition(|r| r.old_no().is_some_and(|n| c.start <= n && n <= c.end));
                // A line restored as context sits in its block; a comment whose line left the
                // diff still shows, under the last unit.
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

    /// The card anchors ([`Self::card_rows`]) and every commented row, from one walk, for
    /// the paint that needs both.
    #[must_use]
    pub fn comment_marks(&self) -> (Vec<(usize, usize)>, HashSet<usize>) {
        let rows = self.comment_rows();
        (cards_of(&rows), rows.into_iter().flat_map(|(_, r)| r).collect())
    }

    /// The comment-card anchors as (row, store index) pairs, store-ordered ([`cards_of`]). The
    /// one card map: the layout walk, the row heights, and the hit tests all read it.
    pub fn card_rows(&self) -> Vec<(usize, usize)> {
        cards_of(&self.comment_rows())
    }

    /// The store index to act on: the comment under the diff cursor, or — in the
    /// list overlay — the highlighted row.
    fn target_comment(&self) -> Option<usize> {
        if self.mode == Mode::List {
            return (self.list_cursor < self.store.len()).then_some(self.list_cursor);
        }
        self.comment_under_cursor()
    }

    /// The store index of a comment whose range covers the current diff row, if any.
    /// The picked comment comes first when it still covers that row.
    fn comment_under_cursor(&self) -> Option<usize> {
        let rows = self.comment_rows();
        let covering =
            rows.iter().filter(|(_, r)| r.contains(&self.diff_cursor)).map(|(ci, _)| *ci);
        self.live_target(&rows).or_else(|| covering.into_iter().next())
    }

    /// Pick comment `index`. The one rule: any action that lands on or creates a specific
    /// comment picks it — a step (`n`/`N`), a card click, an edit (from the list too), a new
    /// comment's submit — so the next `e`/`d` acts on that comment wherever several cover the
    /// cursor's row.
    pub fn target_comment_card(&mut self, index: usize) {
        self.comment_target = self.store.id(index);
    }

    /// The picked comment's store index, while the store still holds it and its covered rows
    /// (`rows`, [`Self::comment_rows`]) include the cursor's row.
    fn live_target(&self, rows: &[(usize, Vec<usize>)]) -> Option<usize> {
        let index = self.store.index_of(self.comment_target?)?;
        let covers = rows.iter().any(|(ci, r)| *ci == index && r.contains(&self.diff_cursor));
        covers.then_some(index)
    }

    /// Clear the pick once it is no longer live: after each of the reviewer's inputs, so a
    /// move off the comment's rows and back never revives it. Polls never call it.
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

    /// Move the diff cursor to the next (`dir >= 0`) or previous comment, comment by comment,
    /// landing on the row where each starts — rendered, its first block's lead row, never the
    /// gap above it — and pick it, so a block holding two comments reaches each. With no pick,
    /// it steps from the cursor's row.
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

    /// The active scope's annotation for `path` — the search overlay's file rows wear the
    /// same marker and stats as the file list.
    pub(crate) fn changed_annotation(&self, path: &str) -> Option<&Annotation> {
        self.changed.get(path)
    }

    /// `/`: open the search screen, from any tab, from either pane.
    pub fn open_search(&mut self) {
        // A navigator-divider drag held from the review view must not become a search-split
        // resize: cancel it so its remaining drag events are consumed, not acted on — the
        // search divider only drags a gesture it started itself.
        self.cancel_divider_drag();
        self.search = Some(SearchOverlay::new());
        self.mode = Mode::Search;
        // The empty query runs too: the warm engine answers it with its frecency-ranked
        // files, so the screen is useful before the first keystroke.
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

    /// Whether the band opens, find or the line field: a file tab whose read pane shows content
    /// rows, source or rendered. A notice (binary, too large) and an empty file carry no content
    /// rows, so `any(is_content)` excludes them.
    pub fn find_available(&self) -> bool {
        self.tab.is_file_tab() && self.visible.iter().any(Row::is_content)
    }

    /// `ctrl+f`: open the find band over the read pane, inert with nothing to search. Opening is a
    /// fresh gesture — it cancels a held drag, clears a live selection, and focuses the read pane
    /// so the steps land there.
    pub fn open_find(&mut self) {
        if !self.find_available() {
            return;
        }
        self.cancel_divider_drag();
        self.clear_selection();
        self.focus = Focus::Diff;
        self.mode = Mode::Find;
        self.find = Some(Find::default());
        // The band steals the pane's bottom row, so pull the cursor above it — otherwise a cursor
        // on the old last row hides behind the band until the first step.
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

    /// The line Enter jumps to: the typed number, `0` the first line and `$` the last as in
    /// vim, and past every file's end when it outgrows a `u32`. `None` on an empty field, where
    /// Enter only closes.
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

    /// Paste into the line field: a number, or `$`, replaces the field — quotes, backticks and
    /// punctuation around it drop away. Anything else, a `path:line` included, leaves the field
    /// as it was: whether a path means the open file is search's to resolve, not the field's.
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

    /// `:N`: land the read cursor on line `n` of the open file — the new side's line, or the old
    /// side's in a file with no new-side lines; in rendered markdown the block holding source
    /// line `n`. `0` is the first line and anything past the end the last, so a fold or a
    /// collapsed `<details>` hiding either opens like any other. A jump is navigation, so a
    /// line-range pick drops. Off screen, the line lands centered. Inert with nothing to land on.
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

    /// The source row holding line `n` by `side`'s numbering ([`line_row`]), its fold opened
    /// first when one hides it — the one landing a find step and `:N` share.
    fn land_on_line(&mut self, n: u32, side: fn(&Row) -> Option<u32>) -> usize {
        let row = line_row(&self.visible, n, side);
        let Some(anchor) = self.visible[row].fold_anchor() else { return row };
        self.expanded_folds.insert(anchor);
        self.rebuild_visible();
        line_row(&self.visible, n, side)
    }

    /// Open every collapsed `<details>` whose body holds source line `n`, outermost included, so
    /// the line renders; the rows rebuild around them.
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

    /// Every match of `query` over the open file in file order, the runs hidden inside folds
    /// included, with the cursor's rank among them (matches strictly before it) and whether the
    /// cursor's own row matches. The current match is the cursor's row when it matches, so both
    /// the count and stepping derive from this walk.
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
                // A collapsed fold's lines are hidden, still searched; folds hold only
                // context runs, so a folded line has a new-side number.
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

    /// `enter`/`↓` (`delta > 0`) and `↑` (`delta < 0`): move the cursor to the nearest matching
    /// row below or above it, wrapping. A match in a collapsed fold expands it first, then the
    /// cursor lands on the revealed row. Inert while nothing matches.
    pub fn find_step(&mut self, delta: i32) {
        // A query lives only while the band is open, and every path that leaves the rows
        // unsearchable closes the band — so a query here always has rows to search.
        let Some(query) = self.find_query().map(str::to_string) else { return };
        if query.is_empty() {
            return;
        }
        let (hits, cursor_rank, on_match) = self.find_hits(&query);
        if hits.is_empty() {
            return;
        }
        let len = hits.len();
        // `cursor_rank` counts matches strictly before the cursor, so a forward step adds `1` to
        // skip a match the cursor already sits on; a backward step never lands on it.
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

    /// Find's query while the band is find; `None` for the line field, whose digits are no
    /// search.
    pub fn find_query(&self) -> Option<&str> {
        self.find.as_ref().filter(|f| f.kind == BandKind::Text).map(|f| f.query.as_str())
    }

    /// The find band's count: the current match's 1-based ordinal (`None` off a match) and the
    /// total. `None` while the query is empty — the band shows a blank count then
    pub fn find_count(&self) -> Option<(Option<usize>, usize)> {
        let query = self.find_query()?;
        if query.is_empty() {
            return None;
        }
        let (hits, cursor_rank, on_match) = self.find_hits(query);
        Some((on_match.then_some(cursor_rank + 1), hits.len()))
    }

    /// `tab`: flip the mode, keeping the query. The held results paint at once and the
    /// pick lands on the new mode's first result row.
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
        // Off `Ready` the screen paints a message, not rows, so there is nothing to
        // move onto — the same guard `search_open_pick` and `build_search_preview` uphold.
        if let Some(s) = self.search.as_mut()
            && s.phase == SearchPhase::Ready
        {
            s.pick = step(s.pick, delta, s.picks());
        }
    }

    /// Land one completion. The dispatcher already dropped stale generations; while a
    /// query is in flight the previous results stay painted. A landed set resets the pick
    /// to the first result row.
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
                // Drop the last preview so the pane falls back to its notice — a stale file
                // under a red error reads as a result.
                s.preview = None;
            }
        }
    }

    /// Rebuild the picked result's preview when it no longer matches the pick — idempotent,
    /// so the event loop can call it every settled frame. It runs only with no input pending,
    /// so a pick sweep never waits on it.
    pub fn build_search_preview(&mut self) {
        let Some(s) = self.search.as_ref() else { return };
        // The pick's target, or `None` when nothing is pickable (off `Ready`, or empty).
        let picked = (s.phase == SearchPhase::Ready).then(|| s.picked()).flatten();
        // Skip when the settled preview already shows this pick — the compare is by reference,
        // so the common no-change frame allocates nothing. A pick move or a landed set makes it
        // differ (rebuild); a poll refreshes the diff in place without moving the pick (skip).
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
        // A deleted file reads empty and previews empty; an over-budget file previews as the
        // File view's notice.
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

    /// `PageUp`/`PageDown`: scroll the settled preview. The renderer clamps; the next
    /// pick re-centers.
    pub fn scroll_search_preview(&mut self, delta: isize) {
        if let Some(p) = self.search.as_ref().and_then(|s| s.preview.as_ref()) {
            p.center.set(false);
            p.scroll.set(p.scroll.get().saturating_add_signed(delta));
        }
    }

    /// A landed poll's preview reconcile: rebuild the previewed file in place, keeping the
    /// scroll. The renderer clamps the scroll and bands the hit only while its line still
    /// exists (Continuity).
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

    /// `enter`: open the picked result in `All files` whatever tab the search left — the
    /// file in the read pane, the navigator selection onto it, ancestors expanded; a code
    /// pick lands the cursor on its line, clamped into the file's current length. A
    /// vanished path opens nothing and the screen stays.
    pub fn search_open_pick(&mut self) -> Result<()> {
        let Some(s) = self.search.as_ref() else { return Ok(()) };
        // Off `Ready`, the painted screen shows no rows — held results are stale and
        // invisible, so nothing opens.
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
        // Opening is a deliberate leave: the origin tab stashes its place on the switch,
        // kept for `1`/`2`/`3`.
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
            // Re-flatten the rows only: the picked file is already in `entries` (search
            // never returns an ignored path), so the worktree walk `apply_dir_change`
            // runs for lazy ignored children would block the pick for nothing
            // (policies/ux-responsiveness.md).
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
            // A code hit names a source line: a markdown file that opens rendered lands on
            // the block holding it, any other file on the line itself.
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

    /// The footer's actions for the current context, tagged with their [`Band`] — row 1 (primary,
    /// send, the cursor's `Do` actions) and the `?`-expansion bands (`Go`, `Move`). Pure: a context
    /// → action mapping, unit-tested without a terminal. The renderer packs row 1, spills trimmed
    /// `Do` actions into the `do` band, and wraps the bands below.
    #[must_use]
    pub fn footer_bands(&self) -> Vec<(FooterAction, Band)> {
        use Band::{Do, Go, Move, Primary, Send};
        use FooterAction as A;

        // A modal sub-task owns the whole bar: one row, the primary then its own actions, no `?`
        // and no bands. The escape action comes right after the primary so the exit hint survives a
        // narrow-width trim (trailing `Do` actions drop first).
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
                // An empty universe has nothing to open or move to, so only the exit is
                // offered.
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
                // With nothing pickable — warming, errored, or no matches — only the
                // mode flip and the exit are offered, so the bar never lists a key that
                // would not work.
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
                // The steps show only with a match to step to, so the bar never lists a key that
                // would not work.
                let has_match = self.find_count().is_some_and(|(_, total)| total > 0);
                return if has_match {
                    vec![(A::FindStep, Primary), (A::CloseFind, Do)]
                } else {
                    vec![(A::CloseFind, Primary)]
                };
            }
            Mode::Normal => {}
        }

        // The read-only PR tab: the state summary leads row 1 (rendered separately); `o open` is the
        // act — available for any resolved PR, not only while a comment is selected, since `o`
        // opens the PR URL itself (`pr_open`). The `go` band carries the always-there keys; `move`
        // carries only the steps the tab has — the PR has no hunk or file steps.
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
            // The `branch` scope with no base: the picker is the way forward, and `b` would
            // re-select the scope already showing, so only the other two offer
            out.push((A::BasePick, Primary));
            out.push((A::ScopeOther, Do));
            out.push((A::Refresh, Do));
        } else if self.commits_gone() && self.tab == Tab::Changes {
            // A gone pick: the picker is the way forward, and `g` would reopen it too, so
            // only the other three scopes offer. `All files` keeps its
            // content and its own actions.
            out.push((A::CommitPick, Primary));
            out.push((A::ScopeOther, Do));
            out.push((A::Refresh, Do));
        } else if self.file_rows.is_empty() {
            // Nothing in scope to review: only switching scope or refreshing is useful, and
            // the scope already showing is never offered on row 1.
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
            // On a markdown file, surface the flip: `m source` rendered, `m rendered` on its
            // source. A deleted file, holding no current content, offers nothing, and content
            // that renders nothing has no rendered view to offer.
            if self.toggle_acts() && !self.rendered.renders_nothing() {
                out.push((A::Rendered, Do));
            }
        }

        // `edit` offers the file wherever the press would open one, asked once for every
        // surface so the bar can neither miss a row nor name one twice.
        // It sits ahead of the navigator's own keys: a narrow row trims trailing actions
        // first, and the file is worth more there than the hide key.
        if self.edit_opens_a_file() {
            let at = out
                .iter()
                .position(|&(a, band)| band == Do && a == A::NavigatorHide)
                .unwrap_or(out.len());
            out.insert(at, (A::EditFile, Do));
        }

        // An armed crossing leads row 1: nothing else on screen says the next press leaves the
        // file. The cursor's own action stays, demoted — commenting still works here
        if let Some(forward) = self.armed_cross() {
            out[0].1 = Do;
            out.insert(0, (A::CrossFile { forward }, Primary));
        }

        // `send` closes row 1 once a comment is written, after the cursor's actions and before the
        // `?` (the renderer keeps it when a narrow row trims the actions before it).
        if !self.store.is_empty() {
            out.push((A::Send, Send));
        }

        // The `go` band: the keys that work anywhere. `scope` and `refresh` only when they are not
        // already row-1 actions (the empty / no-diff states above lead with them), and the pane
        // toggle only when it is not the primary — so a band never repeats a row-1 key.
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

        // The `move` band: the cursor-movement pairs, shown only when there is a changeset to
        // traverse. The hunk step follows `step_hunk`'s own reach — the `Changes` diff only — and
        // drops while a crossing is armed, since the armed primary already owns that key.
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

/// The row the picker's highlight opens on: the agent this session sent to last, else the
/// first row. The last-sent agent counts only while it is still a candidate, so a closed
/// pane falls through.
fn armed_row(rows: &[AgentChoice], last_sent: Option<&str>) -> usize {
    last_sent.and_then(|pane| rows.iter().position(|row| row.pane_id == pane)).unwrap_or(0)
}

impl App {
    /// `Send`: one agent goes straight out, several open the picker, none refuses and names
    /// the clipboard. The empty-store refusal is repeated here, ahead
    /// of [`Self::export`]'s own, so `Send` with nothing written shells out to no herdr call
    /// and opens no picker.
    pub fn send_to_agent(&mut self) {
        if self.store.is_empty() {
            self.status = "no comments yet".to_string();
            return;
        }
        match herdr::send_target() {
            Ok(SendTarget::One(agent)) => self.export_to_agent(&agent),
            Ok(SendTarget::Many(rows)) => self.open_picker(rows),
            Err(e) => self.status = self.failure_line(&e, ToString::to_string),
        }
    }

    /// The status for a failed send or copy: a [`herdr::Refusal`] in the app's words, else the
    /// target's own line.
    fn failure_line(
        &self,
        error: &anyhow::Error,
        other: impl Fn(&anyhow::Error) -> String,
    ) -> String {
        match error.downcast_ref::<herdr::Refusal>() {
            Some(refusal) => self.refusal_line(refusal),
            None => other(error),
        }
    }

    /// A refused send's line: the cause, then the way out. It leads with the cause, because a
    /// narrow pane keeps only the start of the line, and the cause already says nothing went.
    /// The comments stay, and the footer's send count shows them.
    fn refusal_line(&self, refusal: &herdr::Refusal) -> String {
        let copy = self.keymap().hint(crate::keymap::Action::Copy).label();
        match refusal {
            herdr::Refusal::AtPrompt(name) => format!("answer {name}'s prompt first"),
            herdr::Refusal::Unanswered => {
                format!("herdr didn't answer, press {copy} to copy")
            }
            herdr::Refusal::NoAgent => {
                format!("no agent in this workspace, press {copy} to copy")
            }
        }
    }

    /// Open the picker over `rows`, arming the highlight on the agent this session sent to
    /// last when it is still a candidate, else the first row.
    pub fn open_picker(&mut self, rows: Vec<AgentChoice>) {
        // A picker with no rows has nothing to choose and no `enter` that acts, and a second open
        // over a live one would capture `Picker` as the mode to restore — either way a modal that
        // swallows every key and that one `esc` cannot leave. The frozen row set also outranks a
        // later one: it is what the reviewer is reading.
        if rows.is_empty() || self.mode == Mode::Picker {
            return;
        }
        self.picker_cursor = armed_row(&rows, self.last_sent_pane.as_deref());
        self.picker_rows = rows;
        self.picker_over = self.mode.clone();
        self.mode = Mode::Picker;
    }

    /// Close the picker back onto the view it opened over, so a reviewer who sent from the
    /// comments list or with the find band open is not dropped into `Normal`.
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

    /// Move the highlight to `row`, for a digit key or a click. A row past the end is inert
    /// rather than clamped, so a mistyped digit never arms a neighbour.
    pub fn picker_goto(&mut self, row: usize) {
        if self.mode == Mode::Picker && row < self.picker_rows.len() {
            self.picker_cursor = row;
        }
    }

    /// Send every comment to the highlighted agent, then close whatever the outcome. A
    /// failure reports and keeps the comments, so the reviewer can reopen a fresh picker
    /// rather than retry against a frozen row.
    pub fn picker_pick(&mut self) {
        let Some(agent) = self.picker_rows.get(self.picker_cursor).cloned() else { return };
        self.close_picker();
        self.export_to_agent(&agent);
    }

    /// Whether the base picker can open here: a file tab and no `--base` flag, whatever the
    /// scope, like the commit picker.
    #[must_use]
    pub fn base_pick_available(&self) -> bool {
        self.tab.is_file_tab() && self.base.is_none()
    }

    /// Open the base picker: one row per branch, the open PR's target first, the default
    /// branch next, the rest by tip recency, each with its trail facts. Picking the
    /// default row is the way back to the default: its name deletes the pick instead of
    /// recording it. A current non-branch pick is inserted as a row. The highlight opens
    /// on the current base, else the first row. Still opens when that list is empty, so a
    /// revision can be typed.
    pub fn open_base_picker(&mut self) {
        if !self.base_pick_available() || self.mode != Mode::Normal {
            return;
        }
        // The base is re-resolved here, not read from `branch_base`: that lands only while
        // `branch` is showing, and the picker opens from every scope. One pass serves both
        // the winner and the default row's mark.
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

    /// Pick the highlighted row: persist its spelling, then rebuild the changeset
    /// against it. The default row is the way back: its spelling deletes the ref instead
    /// (`git::write_base_pick`), so the pane follows the repo's default again. With no
    /// visible row, Enter checks the query immediately and records it if it resolves.
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
        self.close_base_picker();
        let write = git::write_base_pick(&self.repo, choice.name());
        if let Err(e) = write {
            self.status = e.0;
            return Ok(());
        }
        // Epoch first: any build still in flight read the old pick, and the bump makes its
        // landing fail the input match instead of reverting this one
        // (`crate::world::WorldInput`).
        self.base_epoch = self.base_epoch.wrapping_add(1);
        // A pick takes the reviewer to the scope it configures, like the commit picker
        self.scope = Scope::Branch;
        self.rebase_changes()?;
        self.reveal_files = true;
        Ok(())
    }

    /// Open the commit picker over the body: the universe
    /// newest first, the pick as a row above the list when it is not wholly listed, the
    /// highlight on the pick's newest commit else the first row, and the anchor on the
    /// oldest for a run of two or more. Inert off the file tabs and under any other overlay.
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

    /// The picker's rows, title, and empty message for the current universe
    /// The title follows the range actually listed:
    /// a base with no merge-base (unrelated histories, a shallow cut) lists the last 50.
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
        // A base with a merge-base behind `HEAD` always lists something, so the only empty
        // universe is an unborn repository.
        let title = match &base {
            Some((b, _)) => format!("commits · {} over {}", rows.len(), b.name()),
            None => "commits · last 50".to_string(),
        };
        let empty = "no commits yet".to_string();
        Ok(CommitPicker { rows, pick_row: None, cursor: 0, anchor: None, title, empty, head })
    }

    /// Close the picker back to `Normal`. The scope stays where it is: a pick moved it, and a
    /// cancel leaves the scope the picker opened over.
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

    /// `v`: set the anchor on the highlight, or clear it when it sits there. The pick row
    /// takes no anchor.
    pub fn commit_picker_anchor(&mut self) {
        let Some(cp) = self.commit_picker.as_mut() else { return };
        if cp.is_empty() || cp.is_pick_row(cp.cursor) {
            return;
        }
        cp.anchor = if cp.anchor == Some(cp.cursor) { None } else { Some(cp.cursor) };
    }

    /// `enter`: pick the run, switch to `commits`, and rebuild before the frame
    /// An empty universe does nothing.
    pub fn commit_picker_pick(&mut self) -> Result<()> {
        let Some(pick) = self.commit_picker.as_ref().and_then(CommitPicker::picked) else {
            return Ok(());
        };
        self.close_commit_picker();
        self.commit_pick = Some(pick);
        // The old status describes the old shas: until the build lands the header paints
        // only what the new pick fixes.
        self.pick_status = None;
        // A re-pick in the scope and a switch into it rebuild the same way: the pick is part
        // of the world input, so an in-flight build for the old pick fails the landing gate.
        self.scope = Scope::Commits;
        self.rebase_changes()?;
        self.reveal_files = true;
        Ok(())
    }

    /// A poll under the open picker: once `HEAD` moved, re-list the universe and reconcile
    /// the highlight and the anchor by sha (Continuity). The same `HEAD` keeps the rows as they are, so a
    /// quiet poll spawns nothing on the frame thread, and a failed re-list keeps them too,
    /// without touching the status line.
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
        // The pick row's identity is the pick: it relocates to the fresh pick row, or to the
        // pick's newest commit once the poll listed the whole run.
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
        // Identity first, then the nearest surviving neighbour (the one above wins a tie),
        // then clamp.
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

    /// Export to one decided pane. Nothing re-resolves it, so a pane that closed while the
    /// picker was open fails here and keeps every comment. Only a
    /// delivery arms the next picker's highlight, and the pane comes from the row this send
    /// addressed, so `last used` can never name a pane the export did not reach.
    fn export_to_agent(&mut self, agent: &AgentChoice) {
        let target = Agent { pane: agent.pane_id.clone(), name: agent.name.clone() };
        if self.export(&target) {
            self.last_sent_pane = Some(agent.pane_id.clone());
        }
    }

    /// Send/copy every written comment to `target`; consume the whole set only on
    /// success. A failed export leaves all comments in place.
    /// Reports whether the comments were delivered.
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
                self.status = self.failure_line(&e, |e| target.failure_message(e));
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

    /// The quit key: quits at once when no comment is unsent, else asks first, since quitting
    /// drops every unsent comment for good. The question's own `quit-discard` key is the answer
    /// that quits.
    pub fn request_quit(&mut self) {
        if self.unsent() == 0 {
            self.should_quit = true;
        } else {
            self.confirming_quit = true;
        }
    }

    /// What a quit would drop: every written comment, plus a new comment's draft with text in
    /// it. A draft can only be open when the config-error screen took over mid-composing,
    /// since `q` is text inside the composer.
    pub fn unsent(&self) -> usize {
        let draft =
            matches!(self.mode, Mode::Composing { editing: None }) && !self.input.trim().is_empty();
        self.store.len() + usize::from(draft)
    }

    /// Whether the footer is the open-ended `Normal` bar, with its `?` and bands. A modal, or the
    /// quit question, owns the whole bar instead.
    pub fn footer_open_ended(&self) -> bool {
        self.mode == Mode::Normal && !self.confirming_quit
    }

    /// The number of files changed in the active scope — the header count, the same on both
    /// tabs, since `All files` lists the worktree but counts the changeset.
    pub fn changed_count(&self) -> usize {
        self.changed.len()
    }

    /// The scope's aggregate line stats, shown beside the header count.
    /// Saturating, so a pathological changeset pins at the cap instead of wrapping.
    pub fn changed_totals(&self) -> (u32, u32) {
        self.changed.values().fold((0, 0), |(added, removed), a| {
            (added.saturating_add(a.additions), removed.saturating_add(a.deletions))
        })
    }

    /// Whether a comment's anchor may have moved. A diff comment is stale once its file leaves
    /// the changeset; a File-view (content) comment only once its file is gone from the
    /// worktree, since it was never tied to the changeset.
    pub fn is_stale(&self, c: &Comment) -> bool {
        if c.diff_anchored {
            !self.changed.contains_key(&c.file)
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

/// Each comment's card anchor as (row, store index), store-ordered: a card sits under the last
/// visible row its comment covers — rendered, the last row of the last unit it covers — so
/// the renderer can splice it inline (always visible) and the geometry stays anchored to a
/// real row.
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

/// Move `scroll` the minimal amount so the row at `cursor` fits within a `viewport`-tall
/// window, given each row's display `heights`. Scrolls up when the cursor is above the top,
/// advances the top until the cursor's row fits, then pulls back so the bottom isn't left
/// blank — the shared "keep the cursor visible" rule for both panes (the file list passes
/// all-height-1 rows, where this degenerates to plain row arithmetic).
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

/// Clamp a scroll offset so a `viewport`-tall window over `total` rows shows no blank tail
/// (and 0 when the content fits). Called every frame after any reveal.
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

/// Move a scroll offset by `delta` rows, saturating at 0. The upper bound is applied
/// separately by `bound` once the frame's viewport is known.
fn offset_by(scroll: usize, delta: isize) -> usize {
    if delta >= 0 {
        scroll.saturating_add(delta.unsigned_abs())
    } else {
        scroll.saturating_sub(delta.unsigned_abs())
    }
}

/// One scroll step against a per-frame maximum. The base clamps first, so a stale
/// over-max scroll (the pane grew, the content shrank, an entry alignment overshot)
/// still yields to the first upward input; the result stops at the bottom edge.
fn clamp_scroll(base: usize, delta: isize, max: usize) -> usize {
    base.min(max).saturating_add_signed(delta).min(max)
}

/// Settle freshly built rendered rows on their change marks: a marked block's rows from its
/// lead line on wear its bar, a collapsed summary hiding changes carries how many, and each
/// marker gets a row of its own — after the block it hides in, else where its lines sit. Data
/// alone: the paint words and colors it.
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

/// Whether `row` is one of a hunk's changed lines: a source change row, or a rendered line
/// wearing a change mark — a changed block's line or a marker row.
fn is_change(row: &Row) -> bool {
    match row {
        Row::Deletion { .. }
        | Row::Insertion { .. }
        | Row::Rendered { kind: RenderedKind::Marker { .. }, .. } => true,
        Row::Rendered { kind: RenderedKind::Block { bar, .. }, .. } => bar.is_some(),
        Row::Context { .. } | Row::Fold { .. } => false,
    }
}

/// The nearest hunk's first changed row in `forward`'s direction: strictly past `from` inside
/// the open file, or from the far end (`None`) in a file being crossed into. A hunk starts at a
/// change row whose predecessor is not one: source hunks are separated by context lines or a
/// fold, rendered ones by an unmarked row, as a run of marked lines reads as one edit. A
/// marker row is a stop of its own.
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

/// Whether `path` names a markdown file: a `.md`/`.markdown` extension,
/// case-insensitive.
fn is_markdown_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// The working-tree content of `path`, lossily as UTF-8; empty when the file is
/// absent (a deletion) or unreadable.
fn worktree_content(repo: &std::path::Path, path: &str) -> String {
    std::fs::read(repo.join(path))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// The current-content line source row `i` stands for: its own, else — a deletion or a fold
/// names none — a fold's first hidden line, the nearest line below, then the nearest above.
fn source_line_at(rows: &[Row], i: usize) -> Option<u32> {
    let line_of = |r: &Row| match r {
        Row::Fold { lines } => lines.first().and_then(Row::new_no),
        _ => r.new_no(),
    };
    let at = i.min(rows.len());
    rows[at..].iter().find_map(line_of).or_else(|| rows[..at].iter().rev().find_map(line_of))
}

/// The row holding line `line` by `side`'s numbering: the row numbered `line`, else the
/// collapsed fold hiding it, else the first row numbered past it, else the last numbered row —
/// never a trailing row the side doesn't number (a deleted tail on the new side) — else the last
/// row.
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

/// Old → new source line numbers across one edit of a file's content: an unchanged line maps
/// to where it moved, and a line inside a changed region to the nearest surviving line — its
/// counterpart in a rewrite, the line after it in a deletion.
struct LineMap {
    ops: Vec<similar::DiffOp>,
    new_len: usize,
}

impl LineMap {
    fn new(old: &str, new: &str) -> Self {
        let ops = similar::TextDiff::from_lines(old, new).ops().to_vec();
        Self { ops, new_len: new.lines().count() }
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

/// Whether source row `row` lies in `c`'s range on the comment's side. Rendered rows go by
/// block instead ([`App::rendered_cover`]).
fn line_in(c: &Comment, row: &Row) -> bool {
    let no = match c.side {
        Side::New => row.new_no(),
        Side::Old => row.old_no(),
    };
    no.is_some_and(|n| c.start <= n && n <= c.end)
}

/// Compute `(side, start, end, snippet)` for a selection of diff rows.
///
/// New-side numbers win when present (insertion/context rows); a pure deletion
/// anchors to the old side. The snippet keeps each row's `+`/`−`/space marker.
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
    use super::{App, Mode};
    use crate::config::NavigatorPosition;
    use crate::model::{Comment, CommitPick, Scope, Side};
    use crate::world::{PickStatus, PickVerdict};
    use std::path::PathBuf;

    #[test]
    fn detected_appearance_survives_invalid_config_for_recovery() {
        let mut blocked = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        blocked.set_config_error("invalid config".to_owned());
        assert!(!blocked.set_detected_appearance(crate::appearance::Appearance::Light));
        assert_eq!(blocked.detected_appearance(), Some(crate::appearance::Appearance::Light));

        let mut recovered = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.set_plugin_config(crate::config::PluginConfig::default());
        assert!(recovered.set_detected_appearance(blocked.detected_appearance().unwrap()));
        assert_eq!(recovered.theme_name, "catppuccin-latte");
    }

    #[test]
    fn appearance_updates_follow_paired_defaults_but_explicit_themes_win() {
        let mut app = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        app.set_plugin_config(crate::config::PluginConfig::default());
        assert_eq!(app.theme_name, "catppuccin", "unknown appearance uses the dark fallback");
        assert!(app.set_detected_appearance(crate::appearance::Appearance::Light));
        assert_eq!(app.theme_name, "catppuccin-latte");
        assert!(!app.set_detected_appearance(crate::appearance::Appearance::Light));
        assert!(app.set_detected_appearance(crate::appearance::Appearance::Dark));
        assert_eq!(app.theme_name, "catppuccin");

        let config_dir = tempfile::tempdir().unwrap();
        std::fs::write(config_dir.path().join("config.toml"), "theme = \"nord\"\n").unwrap();
        app.set_plugin_config(crate::config::plugin_config_in(config_dir.path()).unwrap());
        assert_eq!(app.theme_name, "nord", "explicit file theme overrides detected appearance");
        assert!(!app.set_detected_appearance(crate::appearance::Appearance::Light));
        app.set_cli_theme(Some("dracula".to_owned()));
        assert_eq!(app.theme_name, "dracula", "CLI override wins over file and appearance");
        app.set_cli_theme(None);
        app.set_plugin_config(crate::config::PluginConfig::default());
        assert_eq!(
            app.theme_name, "catppuccin-latte",
            "removing the file override restores detected appearance"
        );
    }

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

        // A transient fetch error keeps the painted snapshot, and the highlight with it;
        // a landed snapshot replaces the paint and blanks it; an emptied tab blanks it
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
        // A comments list open over a rendered file, then a broken config fixed with
        // another theme: the carried rows were built in the old palette.
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
        // The `last used` arming is session memory: a config error between two sends must
        // not move the next picker's default.
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
        // The header's base rides with the carried frame — recovery never paints
        // `no base` beside a populated list.
        let base = recovered.branch_base.winner.as_ref().expect("the resolved base is carried");
        assert_eq!(base.name(), "main");
    }

    #[test]
    fn config_recovery_carries_the_commit_picker_with_its_highlight_and_anchor() {
        // The commit picker survives recovery with its rows, highlight, and anchor, and the
        // pick with its verdict rides the carried frame.
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

        // In `Normal` the pick still carries: it is session memory, replaced and never
        // cleared.
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
        // A tab switch requested its refresh, then recovery landed first: the carried flags
        // are what make the recovered app dispatch that refresh instead of keeping the
        // stale stashed frame until the next poll.
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.request_world_refresh(true, true);
        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.carry_authored_state_from(&mut old);
        let request = recovered.world_request.expect("the pending refresh survives the swap");
        assert!(request.sample_turn, "the poll's sample flag survives the recovery swap");
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

    /// A read pane showing `src/lib.rs`: an insertion at new line 10, a deletion of old line
    /// 11, an insertion at new line 12. The cursor sits on the deletion, and the navigator has
    /// one file row for the same file.
    fn edit_app() -> App {
        use crate::diff::{Row, Span};
        use crate::file_list::{Row as ListRow, RowKind};
        let mut app = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        app.entries.push(crate::file_list::Entry {
            path: "src/lib.rs".into(),
            previous_path: None,
            annotation: None,
            ignored: false,
            is_dir: false,
        });
        app.entries.push(crate::file_list::Entry {
            path: "src/other.rs".into(),
            previous_path: None,
            annotation: None,
            ignored: false,
            is_dir: false,
        });
        app.file_rows = vec![
            ListRow {
                depth: 1,
                name: "lib.rs".into(),
                kind: RowKind::File { index: 0, annotation: None },
                ignored: false,
            },
            ListRow {
                depth: 1,
                name: "other.rs".into(),
                kind: RowKind::File { index: 1, annotation: None },
                ignored: false,
            },
        ];
        app.diff_path = Some("src/lib.rs".into());
        app.focus = crate::Focus::Diff;
        let bare = Vec::<Span>::new;
        app.visible = vec![
            Row::Insertion { new_no: 10, spans: bare(), emphasis: Vec::new() },
            Row::Deletion { old_no: 11, spans: bare(), emphasis: Vec::new() },
            Row::Insertion { new_no: 12, spans: bare(), emphasis: Vec::new() },
        ];
        app.diff_cursor = 1;
        app
    }

    /// Every state `edit` can be pressed in, and what it names there.
    ///
    /// The table is the enumeration: `edit_target` is one total function of the cursor, so a
    /// state that is not a row here is a state nobody decided.
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
            // The footer offers the key exactly where the press acts, and names it once. A
            // count, not a presence check: two branches each pushing it reads as `e edit file
            // · z hide · e edit file` on one row.
            let bands = app.footer_bands();
            let offered = bands.iter().filter(|&&(a, _)| a == FooterAction::EditFile).count();
            let expected_offers = usize::from(expected.is_some() && !app.comment_claims_edit());
            assert_eq!(offered, expected_offers, "the footer disagrees with the press: {name}");
            // Ahead of the navigator's own keys, since a narrow row trims from the end and the
            // file is worth more there than the hide key.
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
        // The rendered view paints the card under the block, so the comment claims the key
        // exactly as on source, and the edit opens in place without leaving the rendered view.
        // The table above cannot assert this: its footer expectation
        // is derived from `comment_claims_edit`, so only an outcome catches a wrong predicate.
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

        // The freeze is the diff's. The comments list owns its own screen, so the key still
        // opens the highlighted comment there.
        app.open_list();
        app.start_edit();
        assert!(app.composing(), "the list edits its comment under a live range");
    }

    #[test]
    fn the_pr_tab_leaves_the_key_alone_entirely() {
        // The diff is not on screen there, so neither branch has a target: the file branch has
        // no file tab, and the comment behind the tab has no card.
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
}
