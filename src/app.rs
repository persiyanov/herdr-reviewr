//! Application state and transitions for the Changes review TUI.
//!
//! See `specs/tui.md` and `specs/review-model.md`. This module is terminal-free:
//! every method is a pure state transition or a read-only git/export call, so the
//! whole interaction model is testable without a backend. `src/main.rs` owns the
//! terminal and maps input events onto these methods.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;

use crate::diff::{DiffCache, FileDiff, Row, View};
use crate::export::{ExportTarget, format_all};
use crate::file_list::{self, Annotation, Entry, RowKind};
use crate::forge;
use crate::git;
use crate::highlight::Highlighter;
use crate::logln;
use crate::model::{Comment, CommentStore, Scope, Side};
use crate::theme::{self, Palette};

/// Navigator shares and bounds, as percentages of the body's split axis.
const DEFAULT_SIDE_PCT: u16 = 32;
const DEFAULT_STACK_PCT: u16 = 25;
const MIN_NAVIGATOR_PCT: u16 = 15;
const MAX_SIDE_PCT: u16 = 60;
const MAX_STACK_PCT: u16 = 50;
/// The search screen's results-pane share: half the body by default, dragged within
/// wide bounds — the geometry's minimum pane sizes clamp the rest (specs/search.md).
const DEFAULT_SEARCH_PCT: u16 = 50;
const MIN_SEARCH_PCT: u16 = 10;
const MAX_SEARCH_PCT: u16 = 90;

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

impl Tab {
    /// Whether this tab uses the file-tree / diff machinery (and so the per-tab stash). The
    /// `PR` tab does not — it holds its own state and never swaps into the diff fields.
    pub(crate) fn is_file_tab(self) -> bool {
        matches!(self, Tab::Changes | Tab::AllFiles)
    }
}

/// The inactive tab's saved navigator and read-pane state, swapped in on a tab switch so
/// each tab keeps its own selection and scroll (specs/tui.md).
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
    preview: bool,
    preview_scroll: usize,
    preview_scrolled: bool,
    preview_text: String,
    /// Whether this tab has ever completed a reload. A never-visited tab has nothing worth
    /// painting, so its first entry loads before the frame instead of deferring.
    visited: bool,
}

/// A file crossing offered by the footer, waiting for the hunk step that armed it to repeat: the
/// direction it crosses in, and the file it resolved to open. Holding the file spares the second
/// press the walk the first one already paid for (specs/input.md).
#[derive(Clone, Debug)]
struct ArmedCross {
    forward: bool,
    path: String,
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
    /// The search screen, replacing the body from any tab (specs/search.md). Its state
    /// lives in [`App::search`].
    Search,
    /// The in-file find band over the read pane (specs/find-in-file.md). Its state lives in
    /// [`App::find`].
    Find,
}

/// The search screen's mode: which result set the list shows (specs/search.md).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchMode {
    /// The engine's path matches, one row per file.
    Files,
    /// The engine's content matches, grouped by file.
    Code,
}

/// Where the search overlay stands with the engine (specs/search.md).
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
/// (specs/search.md Preview).
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
    /// The caret into `query`: a char index, edited by the shared caret ops (`input.md`).
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
    /// longer matches the pick — a sweep never waits on a build (specs/search.md).
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

/// The in-file find band's state while `mode == Mode::Find` (specs/find-in-file.md). The current
/// match is the read-pane cursor when its row matches, so only the query is stored — the matches,
/// count, and highlight all derive from the query against the open file each frame.
#[derive(Clone, Debug, Default)]
pub struct Find {
    pub query: String,
    /// The caret into `query`: a char index, edited by the shared caret ops (`input.md`).
    pub caret: usize,
}

/// A found match in file order: how the cursor moves onto it (specs/find-in-file.md).
enum FindHit {
    /// A visible row, at this `visible` index.
    Visible(usize),
    /// A row hidden in the collapsed fold `anchor`; `new_no` is its context line, unique within
    /// the fold, so it is found again once the fold expands.
    Folded { anchor: u32, new_no: u32 },
}

/// The char-index ranges of every non-overlapping occurrence of `query` in `text`, honoring
/// `case_sensitive` (pass [`find_case_sensitive`]'s result for smart-case). Char indices, so the
/// diff renderer overlays the highlight the same way it does word emphasis (specs/find-in-file.md).
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
/// maps each to its key glyph and label and styles it by [`Band`] (`specs/input.md`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FooterAction {
    Comment,
    Select,
    ClearSelection,
    EditComment,
    DeleteComment,
    JumpComment,
    ExpandFold,
    /// Take the armed crossing: the hunk step that armed it leaves the file when pressed again.
    /// The direction names the destination and picks the key (`] next file`, `[ prev file`).
    CrossFile {
        forward: bool,
    },
    /// The `move` band's cursor-movement pairs, each rendered as its two keys (`specs/input.md`).
    /// `MovePage` names the fixed page keys, which are not rebindable.
    MoveLine,
    MoveHunk,
    MoveFile,
    MovePage,
    ExpandDir,
    CollapseDir,
    /// Open the search screen — offered in every context, on every tab (specs/search.md).
    Search,
    /// Open the in-file find band — offered wherever the read pane has content
    /// (specs/find-in-file.md).
    Find,
    /// The search screen's own bar: flip, pick, open, close (specs/search.md). The flip
    /// label names the destination mode, derived from the current mode at render time.
    FlipSearchMode,
    PickResult,
    OpenResult,
    CloseSearch,
    /// The find band's own bar: step between matches, and close (specs/find-in-file.md).
    FindStep,
    CloseFind,
    /// Switch focus between the file list and the diff; the label names the destination pane.
    TogglePane,
    /// Toggle the markdown preview; the label names the destination view (`m preview`
    /// on source, `m source` in the preview).
    Preview,
    NavigatorPosition,
    Wrap,
    Scope,
    Send,
    List,
    Copy,
    Save,
    Newline,
    Cancel,
    CloseList,
    OpenPr,
    Refresh,
    Tabs,
    Quit,
}

/// Where a footer action sits: on row 1 (`Primary`, `Send`, or a `Do` cursor action), or in one of
/// the `?`-expansion bands (`Do` overflow, `Go`, `Move`). Row 1 keeps the primary, `send`, and the
/// `?`, trimming trailing `Do` actions to fit and spilling them into the `do` band (`specs/input.md`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Band {
    Primary,
    Send,
    Do,
    Go,
    Move,
}

/// The full state of the review session.
// The several bools (wrap, reveal_files, reveal_diff, should_quit, and refresh flags) are independent
// toggles, not a state machine in disguise, so the excessive-bools lint does not apply.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct App {
    pub repo: PathBuf,
    pub base: Option<String>,
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
    /// The file crossing a hunk step armed when it found no further hunk in the open file. The
    /// next step the same way takes it, and any other input drops it (specs/input.md).
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
    /// Whether the markdown preview is open for the active file tab's file. Both file tabs
    /// render it; the flag is per file tab and resets on a file change (specs/diff-view.md).
    /// Only the armed toggle — `preview_active()` is the honest on-screen predicate.
    preview: bool,
    /// Top visible rendered line of the markdown preview, clamped to the rendered length.
    pub preview_scroll: usize,
    /// The open markdown file's current content — the preview's render input, refreshed by
    /// `set_diff` and `set_file_view` so no frame rebuilds it. Empty whenever the current
    /// content does not render as a preview: a non-markdown file, a notice, or an empty new
    /// side (a deleted or empty file). One half of the `previewable()` signal.
    preview_text: String,
    /// The preview's maximum useful scroll (rendered lines minus the viewport), noted
    /// by the renderer each frame so [`Self::preview_scroll_by`] can clamp. `usize::MAX`
    /// until the first paint.
    preview_max_scroll: std::cell::Cell<usize>,
    /// Whether a scroll input moved the preview since entry — the exact-restore
    /// predicate; a refresh clamp never sets it (specs/diff-view.md).
    preview_scrolled: bool,
    /// The diff pane's inner width, noted each paint, so the toggle's position mapping
    /// renders at the width the pane will paint with.
    pane_width: std::cell::Cell<usize>,
    /// The link regions painted this frame — a click resolves against the painted
    /// frame (specs/markdown.md).
    painted_links: std::cell::RefCell<Vec<PaintedLink>>,
    /// The painted markdown body's heading anchors as `(slug, content line index)`,
    /// covering the whole body — an anchor click can jump past the viewport.
    painted_anchors: std::cell::RefCell<Vec<(String, usize)>>,
    /// The PR read pane's maximum useful scroll, noted the same way for
    /// [`Self::pr_scroll_read`].
    pr_read_max_scroll: std::cell::Cell<usize>,
    /// The global navigator placement and the separate shares remembered for each split axis.
    pub navigator_position: crate::config::NavigatorPosition,
    pub navigator_side_pct: u16,
    pub navigator_stack_pct: u16,
    /// The search screen's results-pane share — search's own session value, separate
    /// from the review layout's shares (specs/search.md).
    pub search_pct: u16,
    divider_drag: DividerDrag,
    pub select_anchor: Option<usize>,
    pub store: CommentStore,
    pub list_cursor: usize,
    pub mode: Mode,
    pub input: String,
    /// The comment editor's caret: a char index into `input` (`0..=chars().count()`).
    pub caret: usize,
    pub status: String,
    /// Whether the footer's `?` shortcut list is expanded. Global place state, not tab-stashed: one
    /// toggle across every tab, moved only by `?` and `esc`, preserved through a poll and config
    /// recovery (`specs/input.md`, `overview.md` Continuity).
    pub keys_expanded: bool,
    pub should_quit: bool,
    /// The read-only `PR` tab's view of the pull request (`specs/forge-host.md`).
    pub pr: forge::PrView,
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
    /// Set when the PR view needs a (re)fetch; the event loop services it after drawing, so a
    /// `loading` frame shows before the blocking `gh` calls run.
    pub pr_pending: bool,
    /// The world refresh request awaiting dispatch, if any; the event loop hands it to
    /// the worker after the frame paints (specs/tui.md).
    pub world_request: Option<crate::world::WorldRequest>,
    /// The search overlay's state while `mode == Mode::Search`, `None` otherwise.
    pub search: Option<SearchOverlay>,
    /// Set by every query edit (and the open); the event loop dispatches the query to the
    /// search worker after the frame paints, tagged latest-wins (specs/search.md).
    pub search_dirty: bool,
    /// A picked path awaiting its frecency record; the event loop hands it to the worker.
    pub search_track: Option<String>,
    /// The in-file find band's state while `mode == Mode::Find`, `None` otherwise
    /// (specs/find-in-file.md).
    pub find: Option<Find>,
    /// Whether the tab-strip glyph paints this frame — maintained by the event loop's
    /// appear-delay and minimum-display clocks (specs/tui.md).
    pub refresh_indicator: bool,
    /// Set by `r`: the next refresh is commanded, so the glyph lights immediately
    /// instead of waiting out the ambient appear delay (specs/tui.md).
    pub refresh_commanded: bool,
    /// Whether the active file tab has ever completed a reload (stash counterpart:
    /// `TabStash::visited`). Gates the first-visit synchronous load in [`Self::set_tab`].
    tab_visited: bool,
    highlighter: Highlighter,
    /// The active palette every renderer paints from (`specs/theme.md`).
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
    /// The one-slot markdown render memo behind the PR read pane and the file tabs'
    /// preview (`specs/markdown.md`). Interior-mutable so the renderer can fill it from
    /// `&App`; cleared with the diff cache on a theme switch.
    markdown_cache: std::cell::RefCell<crate::markdown::RenderCache>,
    /// The worker-owned turn baseline, mirrored from completions so the sync `last-turn`
    /// paths (the diff's old side, the scope-switch rebuild) read it without a round-trip.
    turn_baseline: Option<String>,
}

/// One painted link region: `x_start..x_end` on screen row `y`, in absolute cells.
#[derive(Clone, Debug)]
struct PaintedLink {
    x_start: u16,
    x_end: u16,
    y: u16,
    url: std::sync::Arc<str>,
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

    /// Construct the error-only sidebar without reading repository state.
    pub(crate) fn blocked(repo: PathBuf, scope: Scope, base: Option<String>) -> Self {
        Self::build(repo, scope, base, false)
    }

    fn build(repo: PathBuf, scope: Scope, base: Option<String>, load_turn: bool) -> Self {
        // Mirror any persisted turn baseline for this worktree, so `last-turn` keeps its
        // anchor across a sidebar restart. The worker's `TurnHost` owns the tracker; this
        // mirror follows its completions (specs/herdr-host.md).
        let turn_baseline = if load_turn { crate::world::seed_baseline(&repo) } else { None };
        let theme = theme::resolve(None);
        Self {
            repo,
            base,
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
            preview: false,
            preview_scroll: 0,
            preview_text: String::new(),
            preview_max_scroll: std::cell::Cell::new(usize::MAX),
            preview_scrolled: false,
            pane_width: std::cell::Cell::new(0),
            painted_links: std::cell::RefCell::new(Vec::new()),
            painted_anchors: std::cell::RefCell::new(Vec::new()),
            pr_read_max_scroll: std::cell::Cell::new(usize::MAX),
            navigator_position: crate::config::NavigatorPosition::Right,
            navigator_side_pct: DEFAULT_SIDE_PCT,
            navigator_stack_pct: DEFAULT_STACK_PCT,
            search_pct: DEFAULT_SEARCH_PCT,
            divider_drag: DividerDrag::Idle,
            select_anchor: None,
            store: CommentStore::new(),
            list_cursor: 0,
            mode: Mode::Normal,
            input: String::new(),
            caret: 0,
            status: String::new(),
            keys_expanded: false,
            should_quit: false,
            pr: forge::PrView::Pending,
            pr_notice: None,
            pr_refreshing: false,
            pr_cursor: 0,
            pr_read_scroll: 0,
            pr_nav_scroll: std::cell::Cell::new(0),
            pr_nav_max_scroll: std::cell::Cell::new(usize::MAX),
            reveal_pr_nav: std::cell::Cell::new(true),
            pr_pending: false,
            world_request: None,
            search: None,
            search_dirty: false,
            search_track: None,
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
            turn_baseline,
        }
    }

    /// Resolve `name` (a CLI or config value; `None` = default) and apply it when it changes:
    /// rebuild the highlighter and drop cached diffs so they re-render. Unknown or
    /// not-yet-supported names fall back to the default (`specs/theme.md`).
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

    /// Block the sidebar on one whole-file configuration failure.
    pub fn set_config_error(&mut self, error: String) {
        self.cancel_divider_drag();
        // The search overlay and the find band close when the config view takes over; recovery
        // restores the tab beneath them, and the query is not restored (specs/search.md,
        // specs/find-in-file.md).
        self.close_search();
        self.close_find();
        self.config = PluginConfigState::Blocked { error };
        self.pr_pending = false;
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
        // (`specs/input.md`).
        self.keys_expanded = old.keys_expanded;
        self.navigator_side_pct = old.navigator_side_pct;
        self.navigator_stack_pct = old.navigator_stack_pct;
        self.search_pct = old.search_pct;
        // A tab switch requested its refresh and recovery landed first: the carried fields
        // below may reinstate the stale stashed frame, so the pending request must survive
        // the swap or that frame never refreshes until the next poll.
        self.world_request = old.world_request.take();
        let old_mode = old.mode.clone();
        match old_mode {
            // `set_config_error` closes the search overlay and the find band before the mode is
            // stored, so neither reaches recovery; their query is not restored (specs/search.md,
            // specs/find-in-file.md).
            Mode::Normal | Mode::Search | Mode::Find => {}
            Mode::List | Mode::Composing { .. } => {
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
                self.diff = std::mem::take(&mut old.diff);
                self.visible = std::mem::take(&mut old.visible);
                self.expanded_folds = std::mem::take(&mut old.expanded_folds);
                self.diff_path = old.diff_path.take();
                self.diff_cursor = old.diff_cursor;
                self.diff_scroll = old.diff_scroll;
                self.h_scroll = old.h_scroll;
                self.select_anchor = old.select_anchor;
                self.resume_list = old.resume_list;
                self.toggled_dirs = std::mem::take(&mut old.toggled_dirs);
                self.stash = std::mem::take(&mut old.stash);
                self.wrap = old.wrap;
                self.preview = old.preview;
                self.preview_scroll = old.preview_scroll;
                self.preview_scrolled = old.preview_scrolled;
                self.preview_text = std::mem::take(&mut old.preview_text);
                self.mode = old.mode.clone();
                self.input = std::mem::take(&mut old.input);
                self.caret = old.caret;
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
        let name = self
            .cli_theme_name
            .clone()
            .unwrap_or_else(|| self.config_snapshot().theme().to_owned());
        self.set_theme(Some(&name));
    }

    /// The active palette every renderer paints from (`specs/theme.md`).
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
    /// collapsed (specs/file-list.md).
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
    /// "a comment is never lost to a refresh" invariant (`specs/overview.md`).
    pub fn reload(&mut self) -> Result<()> {
        self.ensure_config_ready()?;
        // The PR tab holds its own state and renders nothing from the file tree, so a poll on
        // it skips the rebuild; switching back to a file tab reloads it then (specs/tui.md).
        if !self.tab.is_file_tab() {
            return Ok(());
        }
        // Outside a git repo, show an empty state rather than failing (herdr-host.md).
        if !git::is_repo(&self.repo) {
            self.entries.clear();
            self.changed.clear();
            self.file_rows.clear();
            self.file_cursor = 0;
            self.file_scroll = 0;
            if !self.composing() {
                self.diff = FileDiff::empty();
                self.diff_path = None;
                self.visible.clear(); // keep `visible` mirroring `diff` so no stale rows paint
                self.reset_diff_view();
            }
            return Ok(());
        }
        let snapshot = crate::world::build(&self.world_input())?;
        self.reconcile_world(snapshot);
        Ok(())
    }

    /// The input the next world build reads — the tag a landed snapshot is checked against
    /// before it may reconcile (specs/tui.md).
    pub fn world_input(&self) -> crate::world::WorldInput {
        crate::world::WorldInput {
            repo: self.repo.clone(),
            tab: self.tab,
            scope: self.scope,
            base: self.base.clone(),
            base_branches: self.config_snapshot().base_branches().to_vec(),
            turn_baseline: self.turn_baseline.clone(),
            // `Changes` never reads the toggled set, so it stays out of that tab's tag —
            // a directory toggle there must not invalidate an in-flight build.
            toggled_dirs: if self.tab == Tab::AllFiles {
                self.toggled_dirs.clone()
            } else {
                HashSet::new()
            },
        }
    }

    /// Reconcile a built snapshot into the view — the one place a world result touches place
    /// state, by identity first, then fallback, then clamp (`specs/overview.md` Continuity).
    pub fn reconcile_world(&mut self, snapshot: crate::world::WorldSnapshot) {
        // Keep the cursor on the same row target across the rebuild; fall back to the open
        // file, then the first file. The toggled-directory set survives untouched.
        let anchor = self.cursor_anchor();
        let open = self.diff_path.clone();
        self.changed = snapshot.changed;
        self.entries = snapshot.entries;
        self.rebuild_file_rows();
        self.file_cursor = anchor
            .and_then(|a| self.row_of_anchor(&a))
            .or_else(|| open.as_deref().and_then(|p| self.file_row_of_path(p)))
            .or_else(|| self.first_file_row())
            .unwrap_or(0)
            .min(self.file_rows.len().saturating_sub(1));
        // A poll preserves the file-list wheel scroll — it does not reveal the cursor.
        // Explicit actions (navigation, a scope switch) request their own reveal.
        // While a modal is open — composing a comment, or the comments-list overlay — the
        // open diff is frozen, so a poll can't shift the anchor beneath the writer or reset
        // the scroll/selection under the overlay. The file list still updates above.
        if !self.composing() && self.mode != Mode::List {
            // A poll keeps the reader on the same file; only a different shown file resets
            // the diff view to the top. It also drops an armed crossing, which was armed at the
            // edge of a file that is no longer the one on screen (specs/input.md).
            if self.shown_entry().map(|e| e.path) != self.diff_path {
                self.reset_diff_view();
                self.armed_cross = None;
            }
            self.load_read();
        }
        // A landed poll repaints the search preview in place — never the results, which
        // describe the worktree when their query ran (specs/search.md).
        self.refresh_search_preview();
        // The find band closes if its file lost its searchable rows or changed identity under the
        // poll — a forced return, like the markdown preview (specs/find-in-file.md). The current
        // match otherwise follows the reconciled cursor, so nothing else to do.
        if self.mode == Mode::Find && (open != self.diff_path || !self.find_available()) {
            self.close_find();
        }
        self.tab_visited = true;
    }

    /// Load the read pane for the active tab: the scope diff in `Changes`, the whole-file
    /// content in `All files`. Both flatten into `visible` and settle the cursor/scroll.
    fn load_read(&mut self) {
        let Some(entry) = self.shown_entry() else {
            self.diff = FileDiff::empty();
            self.diff_path = None;
            self.visible.clear();
            self.reset_diff_view();
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
        // A different file opens with all folds collapsed and in source. `expanded_folds` is
        // keyed by line number, so without the clear a fold in the new file whose first hidden
        // line matches an expanded one in the old file would render pre-expanded. A same-file
        // poll or scope switch keeps both the folds and the preview choice (specs/diff-view.md).
        if self.diff_path.as_deref() != Some(path.as_str()) {
            self.expanded_folds.clear();
            self.preview = false;
            self.preview_scroll = 0;
            self.preview_max_scroll.set(usize::MAX);
        }
        self.diff_path = Some(path.clone());
        let (old, new) = self.content_sides(&path, previous_path.as_deref());
        self.diff = self.cache.get(path, previous_path, &old, &new, &self.highlighter);
        // Hold the new side as the preview's render input, the same current content the File
        // view previews. A non-markdown file, a notice, or a deleted file (empty new side)
        // holds nothing, so its toggle stays inert (specs/diff-view.md).
        if self.markdown_file() && self.diff.state == crate::diff::FileState::Normal {
            self.preview_text = new;
        } else {
            self.preview_text.clear();
        }
        self.rebuild_visible();
        self.settle_read();
    }

    /// Build the File view for `path`: its current worktree content as `Context` rows, no
    /// folds. The `All files` read pane (specs/diff-view.md). Content is scope-independent.
    fn set_file_view(&mut self, path: &str) {
        // Opening a different file starts in source; a same-file refresh keeps the
        // preview choice and its scroll (specs/diff-view.md).
        if self.diff_path.as_deref() != Some(path) {
            self.preview = false;
            self.preview_scroll = 0;
            self.preview_max_scroll.set(usize::MAX);
        }
        self.diff_path = Some(path.to_string());
        self.expanded_folds.clear(); // the File view has no folds
        let (diff, content) = self.file_view(path);
        // Keep the preview's render input current without a per-frame rebuild. A file the
        // source view degrades to a notice never previews (specs/diff-view.md), so its
        // content is not held either.
        if self.markdown_file() && diff.state == crate::diff::FileState::Normal {
            self.preview_text = content;
        } else {
            self.preview_text.clear();
        }
        self.diff = diff;
        self.rebuild_visible();
        self.settle_read();
    }

    /// Build the read pane's File view for `path`: an over-budget blob (a model weight, a
    /// vendored bundle) previews as the too-large notice without a read — reading it whole
    /// would spike the UI thread before `build_file`'s budget could discard it — else the
    /// worktree content is highlighted through the shared content-hash cache. Returns the
    /// diff and the content read (empty for the notice), for a caller that also keeps the
    /// raw content (specs/diff-view.md). The one build the source view and the search
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

    /// Flatten `diff.rows` into `visible`: an expanded fold becomes its lines, a
    /// collapsed fold stays a single marker row.
    fn rebuild_visible(&mut self) {
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
                let mb = git::merge_base(
                    &self.repo,
                    self.base.as_deref(),
                    self.config_snapshot().base_branches(),
                );
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
        }
    }

    /// Whether the `last-turn` scope is active but no baseline has been captured yet — the
    /// cold-start (or no-herdr) state the UI paints as `waiting for the agent's next turn`.
    pub fn awaiting_turn(&self) -> bool {
        self.scope == Scope::LastTurn && self.turn_baseline.is_none()
    }

    /// Follow the worker's baseline. Every completion carries the authoritative value, so
    /// the mirror syncs even when the completion's snapshot is superseded or discarded.
    pub fn sync_turn_baseline(&mut self, baseline: Option<String>) {
        self.turn_baseline = baseline;
    }

    /// Queue a world refresh for the event loop to dispatch after the frame paints.
    /// `sample` rides the poll's status sample along; `reveal` re-reveals the cursor when
    /// the result lands, for user-initiated switches only (specs/tui.md).
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
        if self.wrap || self.preview_active() {
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
        if self.preview_active() {
            return; // the wrap toggle is inert in the preview (specs/diff-view.md)
        }
        self.wrap = !self.wrap;
        self.h_scroll = 0;
    }

    /// Whether the open file qualifies for the markdown preview: a `.md`/`.markdown`
    /// extension, case-insensitive (specs/diff-view.md).
    #[must_use]
    fn markdown_file(&self) -> bool {
        self.diff_path.as_deref().is_some_and(is_markdown_path)
    }

    /// Whether the `m` toggle would open a preview here: a file tab holding current markdown
    /// content over a rendered pane. `preview_text` is filled only for a markdown file whose
    /// source rows render, so a notice, a deleted file (empty new side), or a rename away from
    /// markdown empties it and makes the toggle inert. The `visible` guard is not redundant:
    /// an emptied changeset clears `visible` through `load_read` without routing through
    /// `set_diff`, so a stale `preview_text` must not preview over a pane with no rows. The
    /// footer offers `m preview` exactly when this holds.
    #[must_use]
    fn previewable(&self) -> bool {
        self.tab.is_file_tab() && !self.preview_text.is_empty() && !self.visible.is_empty()
    }

    /// Whether the markdown preview is on screen: previewable and the toggle armed. A file
    /// renamed away from markdown or degraded mid-preview empties `preview_text` and drops
    /// back to source without disarming the toggle.
    #[must_use]
    pub fn preview_active(&self) -> bool {
        self.previewable() && self.preview
    }

    /// Toggle source ↔ preview on a markdown file in a file tab; inert anywhere else.
    /// Entering clears a live selection and opens at the cursor's block; returning in the
    /// File view maps the top visible block back to a source cursor (specs/diff-view.md).
    pub fn toggle_preview(&mut self) {
        // A file whose source view shows a notice, or a deleted file with no current
        // content, is not previewable, so the title can never claim a preview over a
        // notice (specs/diff-view.md).
        if !self.previewable() {
            return;
        }
        if self.preview {
            self.return_from_preview();
        } else {
            self.clear_selection();
            self.preview = true;
            self.preview_scrolled = false;
            self.align_preview_to_cursor();
        }
    }

    /// Scroll the preview to the block holding the cursor's current-content line, or the
    /// nearest block above it. Meta source lines are non-decreasing, so both lookups bisect.
    fn align_preview_to_cursor(&mut self) {
        self.preview_scroll = 0;
        let width = self.pane_width.get();
        if width == 0 || self.preview_text.is_empty() {
            return;
        }
        // The preview renders the current content, so a row's new-side line is its render
        // source line. A row without one — a deletion, a fold — aligns by the nearest row
        // above with one; none above leaves the preview at its top (specs/diff-view.md). A
        // File-view row is a context row numbered by its position, so this reduces to it.
        let Some(target) = self.visible[..=self.diff_cursor]
            .iter()
            .rev()
            .find_map(Row::new_no)
            .map(|n| n as usize)
        else {
            return;
        };
        let rendered = self.markdown_render(&self.preview_text, width);
        let after = rendered.meta.partition_point(|m| m.source_line <= target);
        let Some(last) = after.checked_sub(1) else {
            return;
        };
        let block_line = rendered.meta[last].source_line;
        self.preview_scroll = rendered.meta.partition_point(|m| m.source_line < block_line);
    }

    /// Leave the preview. In the Diff view the cursor, scroll, and folds stay exactly as
    /// they were left. In the File view a scrolled preview maps its top visible block back
    /// to a source cursor; an unscrolled one leaves the source position exactly as it was
    /// (specs/diff-view.md).
    fn return_from_preview(&mut self) {
        let scrolled = self.preview_scrolled;
        self.preview = false;
        if self.tab != Tab::AllFiles {
            return;
        }
        let width = self.pane_width.get();
        if !scrolled || width == 0 || self.preview_text.is_empty() {
            return;
        }
        let rendered = self.markdown_render(&self.preview_text, width);
        if rendered.meta.is_empty() || self.visible.is_empty() {
            return;
        }
        // Clamp to what the frame painted: a stale scroll past the max would map to a
        // block below the one the reader actually saw at the top of the pane.
        let top =
            self.preview_scroll.min(self.preview_max_scroll.get()).min(rendered.meta.len() - 1);
        let row = rendered.meta[top].source_line.saturating_sub(1);
        self.diff_cursor = row.min(self.visible.len() - 1);
        self.reveal_diff = true;
    }

    /// Scroll the preview by `delta` rendered lines, stopping with the last line at the
    /// pane's bottom edge — content that fits the pane does not scroll, and over-scroll
    /// never builds a dead zone the reader must unwind.
    pub fn preview_scroll_by(&mut self, delta: isize) {
        self.preview_scrolled = true;
        self.preview_scroll =
            clamp_scroll(self.preview_scroll, delta, self.preview_max_scroll.get());
    }

    /// The open markdown file's current content — the preview's render input.
    #[must_use]
    pub(crate) fn preview_text(&self) -> &str {
        &self.preview_text
    }

    /// Note the preview's maximum useful scroll; the renderer calls this each preview frame.
    pub fn note_preview_max_scroll(&self, max: usize) {
        self.preview_max_scroll.set(max);
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

    /// Note the diff pane's inner width; the renderer calls this each paint, and the
    /// toggle's position mapping renders at this width.
    pub fn note_diff_width(&self, width: usize) {
        self.pane_width.set(width);
    }

    /// Drop the painted link and anchor regions; the renderer calls this each frame.
    pub(crate) fn clear_painted_links(&self) {
        self.painted_links.borrow_mut().clear();
        self.painted_anchors.borrow_mut().clear();
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

    /// Act on a clicked link destination (`specs/markdown.md`): a `#anchor` scrolls its
    /// own surface to the matching heading, an `http(s)` destination opens in the
    /// browser, and anything else is inert.
    pub fn open_link(&mut self, url: &str) {
        if let Some(fragment) = url.strip_prefix('#') {
            // The fragment runs through the same normalization that made the slugs, so
            // `#Set-Up!` and `#İstanbul` find their headings (`specs/markdown.md`).
            self.jump_to_anchor(&crate::markdown::slug_text(fragment));
            return;
        }
        if let Ok(clean) = crate::browser::openable_url(url) {
            match crate::browser::open(clean) {
                Ok(()) => self.status = "opened link in browser".to_string(),
                Err(e) => self.status = e.to_string(),
            }
        }
    }

    /// Scroll the painted markdown surface to `slug`'s heading; a missing anchor is inert.
    fn jump_to_anchor(&mut self, slug: &str) {
        let target = self.painted_anchors.borrow().iter().find(|(s, _)| s == slug).map(|(_, i)| *i);
        let Some(idx) = target else {
            return;
        };
        if self.tab == Tab::Pr {
            self.pr_read_scroll = idx.min(self.pr_read_max_scroll.get());
        } else if self.preview_active() {
            self.preview_scrolled = true;
            self.preview_scroll = idx.min(self.preview_max_scroll.get());
        }
    }

    /// Render `text` as markdown wrapped to `width`, through the one-slot memo
    /// (`specs/markdown.md`).
    #[must_use]
    pub(crate) fn markdown_render(&self, text: &str, width: usize) -> crate::markdown::Rendered {
        self.markdown_cache.borrow_mut().get(text, width, &self.highlighter, &self.palette)
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

    /// Move clockwise and cancel any drag captured under the previous geometry.
    pub fn cycle_navigator_position(&mut self) {
        self.cancel_divider_drag();
        self.navigator_position = self.navigator_position.clockwise();
    }

    /// Grow or shrink the navigator by `delta` percentage points on the active split axis.
    pub fn resize_navigator(&mut self, delta: i16) {
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
    /// review layout's shares are untouched (specs/search.md).
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

    /// Scroll the diff so `diff_cursor`'s row fits the `viewport`-display-row window —
    /// `heights` is each visible row's display height (wrap + comment cards). Called once
    /// per frame when a navigation requested a reveal, not on a wheel scroll.
    pub fn reveal_diff_cursor(&mut self, heights: &[usize], viewport: usize) {
        if self.visible.is_empty() {
            self.diff_scroll = 0;
            return;
        }
        let cursor = self.diff_cursor.min(self.visible.len() - 1);
        self.diff_scroll = keep_in_view(cursor, self.diff_scroll, heights, viewport);
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

    /// Switch the changeset scope and reload. A no-op while composing, so a comment
    /// in progress is never stranded against a different diff.
    pub fn set_scope(&mut self, scope: Scope) -> Result<()> {
        self.ensure_config_ready()?;
        if self.scope != scope && !self.composing() {
            self.scope = scope;
            // A scope switch changes the Changes changeset (and each file's old side), so the
            // Changes tab snaps to the top of the new scope: reset its cursor, folds, and diff
            // scroll, and drop cached diffs. The `All files` listing and File view are
            // scope-independent (only the annotations move), so its own state is held by `reload`.
            // The Changes state is the active one on `Changes` and the stashed one while `All
            // files` is shown — reset whichever holds it, so a return to Changes never lands on a
            // stale scroll or a pre-expanded fold.
            self.cache = DiffCache::new();
            if self.tab == Tab::Changes {
                self.file_cursor = 0;
                self.expanded_folds.clear();
                self.reset_diff_view();
                // The changed set rebuilds before the frame, so the list never shows another
                // scope's files under the new scope's label (specs/tui.md). In `Changes` the
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
                // The tree's annotations refresh behind it via the worker (specs/tui.md).
                let changed = crate::world::build_changed(&self.world_input())?;
                self.changed = crate::world::annotate(&changed);
                // Re-mark the tree in place — the rows are scope-independent, only their
                // badges move, so the switch frame never shows the old scope's badges
                // under the new scope's header (policies/ux-responsiveness.md). The tree
                // itself still refreshes behind the switch.
                for entry in &mut self.entries {
                    entry.annotation = self.changed.get(&entry.path).cloned();
                }
                self.rebuild_file_rows();
                self.request_world_refresh(false, false);
            }
            // An explicit switch reveals the cursor (a poll does not).
            self.reveal_files = true;
        }
        Ok(())
    }

    /// Switch to `tab`, saving the active tab's navigator and read-pane state and restoring the
    /// target's. Each tab keeps its own opened file and scroll, so returning to a tab lands
    /// exactly where you left it (specs/tui.md). The switch frame paints the restored state as
    /// it was; a world refresh lands behind it — stale until it lands, never wrong
    /// (specs/overview.md Continuity). A no-op on the active tab or while composing; focus
    /// stays on the same side.
    pub fn set_tab(&mut self, tab: Tab) -> Result<()> {
        self.ensure_config_ready()?;
        if self.tab == tab || self.composing() {
            return Ok(());
        }
        self.tab = tab;
        // Entering the PR tab leaves the file tabs frozen in place and fetches the PR. A
        // `loading` frame draws before the blocking fetch the event loop services, and a
        // re-entry keeps the last snapshot on screen while it refetches.
        if tab == Tab::Pr {
            self.pr_pending = true;
            return Ok(());
        }
        // Entering a file tab: bring its state into the diff fields if the other file tab holds
        // them (a Changes↔AllFiles switch, or a return from PR onto the stashed tab).
        if self.active_file_tab != tab {
            self.swap_active_with_stash();
            self.active_file_tab = tab;
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
        self.reveal_files = true; // pull the restored cursor back into view
        Ok(())
    }

    /// An empty read pane — a first visit landing on a collapsed tree, or an open file gone
    /// empty — focuses the tree, so the cursor keys aren't trapped on a pane with nothing to
    /// move (specs/tui.md). Runs on the switch frame and again when its world refresh lands.
    pub(crate) fn settle_tab_entry(&mut self) {
        if self.visible.is_empty() {
            self.focus = Focus::Files;
        }
    }

    // ---- PR tab (specs/forge-host.md, specs/pr-tab.md) -------------------------------------

    /// Clear a snapshot whose complete fetch input no longer matches the worktree.
    pub fn clear_pr(&mut self) {
        self.pr = forge::PrView::Pending;
        self.pr_notice = None;
        self.pr_refreshing = false;
        self.pr_cursor = 0;
        self.pr_read_scroll = 0;
        self.pr_nav_scroll.set(0);
        self.reveal_pr_nav.set(true);
    }

    /// Apply a snapshot fetched off-thread (`forge::fetch` runs on a worker so the UI never
    /// blocks — `lib.rs`). A transient `Error` keeps the last good snapshot frozen with a status
    /// note, so a failed poll never blanks a populated tab; the cursor clamps to the new rows.
    pub fn apply_pr(&mut self, view: forge::PrView) {
        self.pr_refreshing = false;
        let retry = view.retry_remedy(self.keymap().hint(crate::keymap::Action::Refresh));
        let has_snapshot = matches!(
            self.pr,
            forge::PrView::Pr(_)
                | forge::PrView::NoPr
                | forge::PrView::Detached
                | forge::PrView::Ambiguous(_)
        );
        if has_snapshot && let Some(message) = retry {
            self.pr_notice = Some(message);
            return;
        }
        self.pr_notice = None;
        // Follow the selected row by identity, not index, so a refresh that inserts a newer
        // comment (the list is newest-first) keeps the cursor on the same one and leaves the read
        // scroll intact — only a vanished or absent selection resets it (mirrors the file tabs'
        // poll-preservation, specs/pr-tab.md). The pinned description row's identity is itself:
        // it survives while the new snapshot still has a description, and an emptied one
        // vanishes like a deleted comment.
        let on_description = self.pr_on_description();
        let selected = self
            .pr_selected_comment()
            .map(|c| (c.author.clone(), c.created_at.clone(), c.anchor.clone()));
        self.pr = view;
        let offset = self.pr_description_offset();
        let restored = if on_description {
            self.pr_has_description().then_some(0)
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
            // shows a different row (specs/pr-tab.md).
            let clamped = self.pr_row_count().saturating_sub(1);
            if self.pr_cursor > clamped || on_description || selected.is_some() {
                self.pr_read_scroll = 0;
            }
            self.pr_cursor = self.pr_cursor.min(clamped);
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
    /// existence condition (specs/pr-tab.md).
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

    /// Open the pull request in the browser (`specs/pr-tab.md`). A resolved PR always carries a
    /// `url`, so there is nothing to guard against.
    pub fn pr_open(&mut self) {
        let Some(url) = self.pr_snapshot().map(|s| s.url.clone()) else {
            return;
        };
        match crate::browser::open(&url) {
            Ok(()) => self.status = "opened PR in browser".to_string(),
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
        std::mem::swap(&mut self.preview, &mut self.stash.preview);
        std::mem::swap(&mut self.preview_scroll, &mut self.stash.preview_scroll);
        std::mem::swap(&mut self.preview_text, &mut self.stash.preview_text);
        std::mem::swap(&mut self.preview_scrolled, &mut self.stash.preview_scrolled);
        std::mem::swap(&mut self.tab_visited, &mut self.stash.visited);
    }

    pub fn toggle_focus(&mut self) {
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
                // The preview has no cursor: vertical movement scrolls it, and the source
                // view's cursor waits untouched for the toggle back (specs/diff-view.md).
                if self.preview_active() {
                    self.preview_scroll_by(delta);
                } else if !self.visible.is_empty() {
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

    /// `next-file`: open the next file, from either pane (`specs/input.md`).
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

    /// `next-hunk`: jump to the nearest hunk below the cursor (`specs/input.md`).
    pub fn next_hunk(&mut self) {
        self.step_hunk(true);
    }

    /// `prev-hunk`: jump to the nearest hunk above the cursor; see [`Self::next_hunk`].
    pub fn prev_hunk(&mut self) {
        self.step_hunk(false);
    }

    /// Move the diff cursor to the nearest hunk's first changed row past it. With no hunk left
    /// this way, the first press arms the crossing and the second one takes it, so a held key
    /// stops at each file. Only `Changes` paints change rows — `All files` is all context, and
    /// the preview has no cursor — so a step anywhere else has no target (`specs/input.md`).
    fn step_hunk(&mut self, forward: bool) {
        // Any step drops the standing arm. A step the other way is not the repeat it waits for.
        let armed = self.armed_cross.take().filter(|a| a.forward == forward);
        if !self.can_traverse() || self.tab != Tab::Changes || self.preview_active() {
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
    /// (`specs/input.md`).
    pub fn disarm_cross(&mut self) {
        self.armed_cross = None;
    }

    /// Toggle the footer's `?` shortcut list. Called only from `Normal` mode, so a modal's `?` stays
    /// text or inert (`specs/input.md`).
    pub fn toggle_keys(&mut self) {
        self.keys_expanded = !self.keys_expanded;
    }

    /// The `esc` ladder in `Normal` mode: peel exactly one layer per press — a live selection, then
    /// an armed crossing, then the footer expansion (`specs/input.md`). The selection and crossing
    /// are file-tab place state, frozen in place while `PR` is active, so `esc` on `PR` closes only
    /// the expansion and never disturbs the file tab the reviewer will return to (`overview.md`
    /// Continuity).
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
    /// jump would silently drop the selection under it (`specs/input.md`).
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
        // entry set is rebuilt before the rows (file-list.md). Other tabs just re-flatten.
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
        if self.preview_active() {
            self.preview_scroll_by(delta);
            return;
        }
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

    /// Toggle a range-selection anchor at the current diff line.
    pub fn toggle_select(&mut self) {
        if self.preview_active() {
            return; // the preview is read-only (specs/diff-view.md)
        }
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
        if self.preview_active() {
            return; // the preview is read-only (specs/diff-view.md)
        }
        if self.focus == Focus::Diff && self.has_anchorable_selection() {
            // Anchor the cursor at the selection's last line so the scroll keeps it (and
            // the box drawn beneath it) in view.
            self.diff_cursor = self.selection_range().1;
            self.reveal_diff = true; // scroll the anchored line into view before the box opens
            self.input.clear();
            self.caret = 0;
            self.resume_list = false; // a fresh diff comment returns to the diff, not the list
            self.mode = Mode::Composing { editing: None };
        }
    }

    pub fn start_edit(&mut self) {
        // Cards don't show in the preview, so `e` on the invisible source cursor is inert;
        // an edit reached through the comments-list overlay drops back to source, where
        // the composer and its anchor are visible (specs/diff-view.md).
        if self.preview_active() && self.mode != Mode::List {
            return;
        }
        // Editing from the comments-list overlay returns there on finish (else to the diff).
        let from_list = self.mode == Mode::List;
        let Some(i) = self.target_comment() else { return };
        let Some(c) = self.store.get(i) else { return };
        self.preview = false;
        let (file, side, start, end, text) =
            (c.file.clone(), c.side, c.start, c.end, c.text.clone());

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
        // same-numbered line in a different file.
        if self.diff_path.as_deref() == Some(file.as_str())
            && let Some(idx) = self.visible.iter().position(|row| {
                let no = match side {
                    Side::New => row.new_no(),
                    Side::Old => row.old_no(),
                };
                no.is_some_and(|n| start <= n && n <= end)
            })
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
    }

    // --- text editing: a character caret into the active field ----------------------------
    // The comment editor and the search input share one control set (specs/input.md,
    // specs/search.md). `caret` is a char index in `0..=text.chars().count()`. Edits
    // round-trip through a `Vec<char>` (both fields are short), so every op is
    // character-wise and multi-byte safe.

    /// The mode's editable text and caret: the comment draft while composing, the search
    /// query while the overlay is open, nothing otherwise.
    fn active_field(&mut self) -> Option<(&mut String, &mut usize)> {
        match self.mode {
            Mode::Composing { .. } => Some((&mut self.input, &mut self.caret)),
            Mode::Search => self.search.as_mut().map(|s| (&mut s.query, &mut s.caret)),
            Mode::Find => self.find.as_mut().map(|f| (&mut f.query, &mut f.caret)),
            Mode::Normal | Mode::List => None,
        }
    }

    /// Run a character-wise edit on the active field: collect it into a `Vec<char>` with
    /// the caret as an in-range index, hand both to `f`, then reassemble and re-clamp the
    /// caret. Every mutating `input_*` op routes through here, so the guard / collect /
    /// reassemble lives once instead of seven times. A changed search query re-queries
    /// (specs/search.md).
    fn edit_input(&mut self, f: impl FnOnce(&mut Vec<char>, &mut usize)) {
        let searching = self.mode == Mode::Search;
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

    /// Insert pasted `text` at the caret as one unit, normalizing `\r\n`/`\r` to `\n`.
    /// The single-line search input takes newlines as spaces (specs/search.md).
    pub fn input_paste(&mut self, text: &str) {
        let mut norm = text.replace("\r\n", "\n").replace('\r', "\n");
        if matches!(self.mode, Mode::Search | Mode::Find) {
            norm = norm.replace('\n', " ");
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
                    self.store.add(c);
                    self.status = "comment added".to_string();
                }
            }
        }
        self.select_anchor = None;
        self.leave_compose();
    }

    /// Whether the selection has at least one content row a comment can attach to —
    /// a fold marker does not qualify.
    fn has_anchorable_selection(&self) -> bool {
        let (lo, hi) = self.selection_range();
        self.visible.get(lo..=hi).is_some_and(|s| s.iter().any(Row::is_content))
    }

    /// The `(side, start, end, snippet)` the current selection anchors to.
    fn selection_anchor(&self) -> Option<(Side, u32, u32, String)> {
        let (lo, hi) = self.selection_range();
        anchor(self.visible.get(lo..=hi)?)
    }

    fn build_comment(&self, text: String) -> Option<Comment> {
        // Anchor to the file the open diff belongs to (`diff_path`), not the file-list
        // selection — they diverge if the list shifts under a comment in progress.
        let file = self.diff_path.clone()?;
        let (side, start, end, lines) = self.selection_anchor()?;
        // The File view marks every comment as content-anchored, so it ages by file existence,
        // not changeset membership (specs/review-model.md).
        let diff_anchored = self.diff.view == View::Diff;
        Some(Comment { file, side, start, end, lines, text, diff_anchored })
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
                };
                Some(c.location())
            }
            Mode::Normal | Mode::List | Mode::Search | Mode::Find => None,
        }
    }

    /// Whether comment `c` anchors to the pane's current view — a diff comment to the Diff view,
    /// a content comment to the File view. Stops a comment of one kind rendering on, or being
    /// acted on at, an unrelated line in the other tab's view of the same file (the diff's line
    /// numbering and the File view's worktree line numbering differ; specs/review-model.md).
    fn comment_in_view(&self, c: &Comment) -> bool {
        c.diff_anchored == (self.diff.view == View::Diff)
    }

    /// Row indices on the open diff's file that a comment anchors to.
    pub fn commented_lines(&self) -> HashSet<usize> {
        let Some(file) = self.diff_path.clone() else {
            return HashSet::new();
        };
        self.visible
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                self.store
                    .iter()
                    .any(|c| c.file == file && self.comment_in_view(c) && line_in(c, row))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// For each visible diff row, the store indices of comments whose card renders after it.
    /// A comment's card sits under the last visible row its line range covers, so the renderer
    /// can splice it inline (always visible) and the geometry stays anchored to a real row.
    pub fn comment_cards(&self) -> Vec<Vec<usize>> {
        let mut cards = vec![Vec::new(); self.visible.len()];
        let Some(file) = self.diff_path.as_deref() else { return cards };
        for (ci, c) in self.store.iter().enumerate() {
            if c.file == file
                && self.comment_in_view(c)
                && let Some(last) = self.visible.iter().rposition(|row| line_in(c, row))
            {
                cards[last].push(ci);
            }
        }
        cards
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
    fn comment_under_cursor(&self) -> Option<usize> {
        let file = self.diff_path.as_deref()?;
        let row = self.visible.get(self.diff_cursor)?;
        self.store.iter().position(|c| c.file == file && self.comment_in_view(c) && line_in(c, row))
    }

    pub fn delete_comment(&mut self) {
        // Cards don't show in the preview: `d` only acts through the comments-list overlay.
        if self.preview_active() && self.mode != Mode::List {
            return;
        }
        if let Some(i) = self.target_comment() {
            logln!("comment delete [{i}]");
            self.store.take(i);
            self.clamp_list_cursor();
            self.status = "comment deleted".to_string();
            // Don't strand the user in an empty "Comments (0)" overlay, matching `export`.
            if self.store.is_empty() {
                self.close_list();
            }
        }
    }

    /// Move the diff cursor to the next (`dir >= 0`) or previous commented line.
    pub fn jump_comment(&mut self, dir: isize) {
        if self.preview_active() {
            return; // no cursor and no cards in the preview (specs/diff-view.md)
        }
        let mut idxs: Vec<usize> = self.commented_lines().into_iter().collect();
        if idxs.is_empty() {
            return;
        }
        idxs.sort_unstable();
        self.focus = Focus::Diff;
        let cur = self.diff_cursor;
        let target = if dir >= 0 {
            idxs.iter().copied().find(|&i| i > cur).or_else(|| idxs.first().copied())
        } else {
            idxs.iter().rev().copied().find(|&i| i < cur).or_else(|| idxs.last().copied())
        };
        if let Some(t) = target {
            self.select_anchor = None; // a comment jump is navigation, not a selection extend
            self.diff_cursor = t;
            self.reveal_diff = true;
        }
    }

    // --- Search overlay (specs/search.md) ------------------------------------------------

    /// The active scope's annotation for `path` — the search overlay's file rows wear the
    /// same marker and stats as the file list (specs/search.md).
    pub(crate) fn changed_annotation(&self, path: &str) -> Option<&Annotation> {
        self.changed.get(path)
    }

    /// `/`: open the search screen, from any tab, from either pane (specs/search.md).
    pub fn open_search(&mut self) {
        // A navigator-divider drag held from the review view must not become a search-split
        // resize: cancel it so its remaining drag events are consumed, not acted on — the
        // search divider only drags a gesture it started itself (specs/input.md).
        self.cancel_divider_drag();
        self.search = Some(SearchOverlay::new());
        self.mode = Mode::Search;
        // The empty query runs too: the warm engine answers it with its frecency-ranked
        // files, so the screen is useful before the first keystroke.
        self.search_dirty = true;
    }

    /// `esc`: drop the screen whole, place untouched (specs/search.md).
    pub fn close_search(&mut self) {
        if self.mode == Mode::Search {
            self.mode = Mode::Normal;
        }
        self.search = None;
        self.search_dirty = false;
    }

    /// Whether the find band opens: the read pane shows searchable content rows — a file tab, not
    /// the markdown preview, at least one content row. A notice (binary, too large) and an empty
    /// file carry no content rows, so `any(is_content)` excludes them (specs/find-in-file.md).
    pub fn find_available(&self) -> bool {
        self.tab.is_file_tab() && !self.preview && self.visible.iter().any(Row::is_content)
    }

    /// `ctrl+f`: open the find band over the read pane, inert with nothing to search. Opening is a
    /// fresh gesture — it cancels a held drag, clears a live selection, and focuses the read pane
    /// so the steps land there (specs/find-in-file.md, specs/diff-view.md).
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
        // on the old last row hides behind the band until the first step (specs/find-in-file.md).
        self.reveal_diff = true;
    }

    /// `esc`: close the band, dropping the query. The cursor stays where the last step left it
    /// (specs/find-in-file.md).
    pub fn close_find(&mut self) {
        if self.mode == Mode::Find {
            self.mode = Mode::Normal;
        }
        self.find = None;
    }

    /// Every match of `query` over the open file in file order, the runs hidden inside folds
    /// included, with the cursor's rank among them (matches strictly before it) and whether the
    /// cursor's own row matches. The current match is the cursor's row when it matches, so both
    /// the count and stepping derive from this walk (specs/find-in-file.md).
    fn find_hits(&self, query: &str) -> (Vec<FindHit>, usize, bool) {
        let cs = find_case_sensitive(query);
        let is_hit = |row: &Row| !find_match_ranges(&row.text(), query, cs).is_empty();
        let mut hits = Vec::new();
        let mut vis = 0usize;
        let mut cursor_rank = 0usize;
        let mut on_match = false;
        for row in &self.diff.rows {
            let expanded = row.fold_anchor().is_some_and(|a| self.expanded_folds.contains(&a));
            match row {
                Row::Fold { lines } if !expanded => {
                    // The collapsed marker sits at `vis`; its lines are hidden, still searched.
                    if vis == self.diff_cursor {
                        cursor_rank = hits.len();
                        on_match = false;
                    }
                    let anchor = row.fold_anchor().expect("a fold has a first hidden line");
                    for line in lines {
                        if is_hit(line) {
                            // Folds hold only context runs, so a folded line has a new-side number.
                            let new_no = line.new_no().expect("a folded row is context");
                            hits.push(FindHit::Folded { anchor, new_no });
                        }
                    }
                    vis += 1;
                }
                Row::Fold { lines } => {
                    // Expanded: its lines are visible rows, inline at `vis`.
                    for line in lines {
                        let m = is_hit(line);
                        if vis == self.diff_cursor {
                            cursor_rank = hits.len();
                            on_match = m;
                        }
                        if m {
                            hits.push(FindHit::Visible(vis));
                        }
                        vis += 1;
                    }
                }
                content => {
                    let m = is_hit(content);
                    if vis == self.diff_cursor {
                        cursor_rank = hits.len();
                        on_match = m;
                    }
                    if m {
                        hits.push(FindHit::Visible(vis));
                    }
                    vis += 1;
                }
            }
        }
        (hits, cursor_rank, on_match)
    }

    /// `enter`/`↓` (`delta > 0`) and `↑` (`delta < 0`): move the cursor to the nearest matching
    /// row below or above it, wrapping. A match in a collapsed fold expands it first, then the
    /// cursor lands on the revealed row. Inert while nothing matches (specs/find-in-file.md).
    pub fn find_step(&mut self, delta: i32) {
        let Some(query) = self.find.as_ref().map(|f| f.query.clone()) else { return };
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
            FindHit::Folded { anchor, new_no } => {
                self.expanded_folds.insert(anchor);
                self.rebuild_visible();
                // Expanding the fold reveals the context row that held this new-side line number.
                self.diff_cursor = self
                    .visible
                    .iter()
                    .position(|r| r.new_no() == Some(new_no))
                    .expect("the expanded fold reveals the row for this new_no");
            }
        }
        self.reveal_diff = true;
    }

    /// The find band's count: the current match's 1-based ordinal (`None` off a match) and the
    /// total. `None` while the query is empty — the band shows a blank count then
    /// (specs/find-in-file.md).
    pub fn find_count(&self) -> Option<(Option<usize>, usize)> {
        let query = &self.find.as_ref()?.query;
        if query.is_empty() {
            return None;
        }
        let (hits, cursor_rank, on_match) = self.find_hits(query);
        Some((on_match.then_some(cursor_rank + 1), hits.len()))
    }

    /// `tab`: flip the mode, keeping the query. The held results paint at once and the
    /// pick lands on the new mode's first result row (specs/search.md).
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

    /// `↓`/`↑`, `ctrl+n`/`p`: move the pick by `delta`, only while `Ready` (specs/search.md).
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
    /// to the first result row (specs/search.md).
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
                // under a red error reads as a result (specs/search.md).
                s.preview = None;
            }
        }
    }

    /// Rebuild the picked result's preview when it no longer matches the pick — idempotent,
    /// so the event loop can call it every settled frame. It runs only with no input pending,
    /// so a pick sweep never waits on it (specs/search.md Preview).
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
        // File view's notice (specs/search.md).
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
    /// pick re-centers (specs/search.md).
    pub fn scroll_search_preview(&mut self, delta: isize) {
        if let Some(p) = self.search.as_ref().and_then(|s| s.preview.as_ref()) {
            p.center.set(false);
            p.scroll.set(p.scroll.get().saturating_add_signed(delta));
        }
    }

    /// A landed poll's preview reconcile: rebuild the previewed file in place, keeping the
    /// scroll. The renderer clamps the scroll and bands the hit only while its line still
    /// exists (specs/search.md, `overview.md` Continuity).
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
    /// vanished path opens nothing and the screen stays (specs/search.md).
    pub fn search_open_pick(&mut self) -> Result<()> {
        let Some(s) = self.search.as_ref() else { return Ok(()) };
        // Off `Ready`, the painted screen shows no rows — held results are stale and
        // invisible, so nothing opens (specs/search.md).
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
        // kept for `1`/`2`/`3` (specs/search.md).
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
        // A same-file pick must land on the hit line in source, not behind an open
        // markdown preview (`set_file_view` keeps the choice for a same-path open).
        self.preview = false;
        self.set_file_view(&path);
        if let Some(fi) = self.file_row_of_path(&path) {
            self.file_cursor = fi;
            self.reveal_files = true;
        }
        self.focus = Focus::Diff;
        if let Some(line) = line {
            let last = self.visible.len().saturating_sub(1);
            self.diff_cursor = (line.saturating_sub(1) as usize).min(last);
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
    /// `Do` actions into the `do` band, and wraps the bands below (`specs/input.md`).
    #[must_use]
    pub fn footer_bands(&self) -> Vec<(FooterAction, Band)> {
        use Band::{Do, Go, Move, Primary, Send};
        use FooterAction as A;

        // A modal sub-task owns the whole bar: one row, the primary then its own actions, no `?`
        // and no bands. The escape action comes right after the primary so the exit hint survives a
        // narrow-width trim (trailing `Do` actions drop first).
        match self.mode {
            Mode::Composing { .. } => {
                return vec![(A::Save, Primary), (A::Cancel, Do), (A::Newline, Do)];
            }
            Mode::List => {
                return vec![
                    (A::Send, Primary),
                    (A::CloseList, Do),
                    (A::Copy, Do),
                    (A::EditComment, Do),
                    (A::DeleteComment, Do),
                ];
            }
            Mode::Search => {
                // With nothing pickable — warming, errored, or no matches — only the
                // mode flip and the exit are offered, so the bar never lists a key that
                // would not work (specs/search.md).
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
            Mode::Find => {
                // The steps show only with a match to step to, so the bar never lists a key that
                // would not work (specs/find-in-file.md).
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
        // carries only the steps the tab has — the PR has no hunk or file steps (`specs/pr-tab.md`).
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

        if self.preview_active() && self.focus == Focus::Diff {
            // The read-only preview: the way back to the commentable source leads, and
            // no comment key is offered (specs/input.md); the shared tail below adds the
            // scope, send, and band actions. With the file list focused, the tree's own
            // actions apply instead.
            out.push((A::Preview, Primary));
        } else if self.file_rows.is_empty() {
            // Nothing in scope to review: only switching scope or refreshing is useful.
            out.push((A::Scope, Primary));
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
        } else if self.visible.is_empty() {
            // Diff focused but nothing to show (e.g. a binary): only the scope switch helps.
            out.push((A::Scope, Primary));
        } else if self.on_fold() {
            out.push((A::ExpandFold, Primary));
        } else if self.select_anchor.is_some() {
            out.push((A::Comment, Primary));
            out.push((A::ClearSelection, Do));
        } else if self.comment_under_cursor().is_some() {
            out.push((A::EditComment, Primary));
            out.push((A::DeleteComment, Do));
            out.push((A::JumpComment, Do));
        } else {
            out.push((A::Comment, Primary));
            out.push((A::Select, Do));
            // On a markdown file's source line that previews, surface the way in —
            // otherwise the rendered view is undiscoverable (specs/input.md). A deleted
            // file, holding no current content, offers nothing.
            if self.previewable() {
                out.push((A::Preview, Do));
            }
        }

        // An armed crossing leads row 1: nothing else on screen says the next press leaves the
        // file. The cursor's own action stays, demoted — commenting still works here
        // (specs/input.md).
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
        if !out.iter().any(|&(a, _)| a == A::Scope) {
            out.push((A::Scope, Go));
        }
        out.push((A::Search, Go));
        // In-file find shows wherever the read pane has content to search (specs/find-in-file.md).
        if self.find_available() {
            out.push((A::Find, Go));
        }
        out.push((A::Wrap, Go));
        if !self.store.is_empty() {
            out.push((A::List, Go));
            out.push((A::Copy, Go));
        }
        if !out.iter().any(|&(a, _)| a == A::Refresh) {
            out.push((A::Refresh, Go));
        }
        out.push((A::Tabs, Go));
        if !pane_is_primary && !self.file_rows.is_empty() {
            out.push((A::TogglePane, Go));
        }
        out.push((A::NavigatorPosition, Go));
        out.push((A::Quit, Go));

        // The `move` band: the cursor-movement pairs, shown only when there is a changeset to
        // traverse. The hunk step follows `step_hunk`'s own reach — the `Changes` diff only, never a
        // preview — and drops while a crossing is armed, since the armed primary already owns that
        // key (`specs/input.md`).
        if !self.file_rows.is_empty() {
            out.push((A::MoveLine, Move));
            if self.tab == Tab::Changes && !self.preview_active() && self.armed_cross().is_none() {
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

    /// Send/copy every written comment to `target`; consume the whole set only on
    /// success. A failed export leaves all comments in place (`specs/review-model.md`).
    pub fn export(&mut self, target: &dyn ExportTarget) {
        if self.store.is_empty() {
            self.status = "no comments to send".to_string();
            return;
        }
        let refs: Vec<&Comment> = self.store.iter().collect();
        let text = format_all(&refs);
        let n = refs.len();
        logln!("export ({n}) -> {} ::\n{text}", target.label());
        match target.export(&text) {
            Ok(()) => {
                self.store.take_all();
                self.status = target.success_message(n);
                logln!("export OK");
            }
            Err(e) => {
                self.status = format!("{} failed: {e}", target.label());
                logln!("export ERR: {e}");
            }
        }
        self.clamp_list_cursor();
        if self.store.is_empty() {
            self.close_list();
        }
    }

    /// The number of files changed in the active scope — the header count, the same on both
    /// tabs (specs/tui.md), since `All files` lists the worktree but counts the changeset.
    pub fn changed_count(&self) -> usize {
        self.changed.len()
    }

    /// The scope's aggregate line stats, shown beside the header count (specs/tui.md).
    /// Saturating, so a pathological changeset pins at the cap instead of wrapping.
    pub fn changed_totals(&self) -> (u32, u32) {
        self.changed.values().fold((0, 0), |(added, removed), a| {
            (added.saturating_add(a.additions), removed.saturating_add(a.deletions))
        })
    }

    /// Whether a comment's anchor may have moved. A diff comment is stale once its file leaves
    /// the changeset; a File-view (content) comment only once its file is gone from the
    /// worktree, since it was never tied to the changeset (specs/review-model.md).
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

/// Whether `row` is one of a hunk's changed lines.
fn is_change(row: &Row) -> bool {
    matches!(row, Row::Deletion { .. } | Row::Insertion { .. })
}

/// The nearest hunk's first changed row in `forward`'s direction: strictly past `from` inside
/// the open file, or from the far end (`None`) in a file being crossed into. A hunk starts at a
/// change row whose predecessor is not one, since context lines or a fold always separate two
/// hunks (specs/diff-view.md).
fn hunk_row(rows: &[Row], from: Option<usize>, forward: bool) -> Option<usize> {
    let starts_hunk = |&i: &usize| is_change(&rows[i]) && (i == 0 || !is_change(&rows[i - 1]));
    if forward {
        (from.map_or(0, |i| i + 1)..rows.len()).find(starts_hunk)
    } else {
        (0..from.unwrap_or(rows.len()).min(rows.len())).rev().find(starts_hunk)
    }
}

/// Whether `path` names a markdown file: a `.md`/`.markdown` extension,
/// case-insensitive (specs/diff-view.md).
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
    use crate::model::{Comment, Scope, Side};
    use std::path::PathBuf;

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
    fn config_recovery_carries_an_open_preview() {
        let mut old = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        old.mode = Mode::List;
        old.preview = true;
        old.preview_scroll = 7;
        old.preview_text = "# doc".to_string();

        let mut recovered = App::new(PathBuf::from("."), Scope::Uncommitted, None);
        recovered.carry_authored_state_from(&mut old);

        assert!(recovered.preview, "the preview choice survives config recovery");
        assert_eq!(recovered.preview_scroll, 7);
        assert_eq!(recovered.preview_text(), "# doc");
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
    fn blocked_app_rejects_normal_repository_work_without_panicking() {
        let mut app = App::blocked(PathBuf::from("."), Scope::Uncommitted, None);
        app.set_config_error("bad config".to_string());

        assert!(app.reload().unwrap_err().to_string().contains("bad config"));
        assert!(app.set_scope(Scope::Branch).is_err());
        assert!(app.set_tab(super::Tab::AllFiles).is_err());
        assert!(app.move_cursor(1).is_err());
        assert!(app.select_file(0).is_err());
    }
}
