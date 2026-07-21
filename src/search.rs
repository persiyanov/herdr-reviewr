//! The search worker: the `fff-search` engine behind request/completion channels.
//!
//! The engine owns matching, ranking, and indexing; reviewr passes the query through and
//! renders results in the engine's order (specs/search.md). The worker owns the picker and
//! its background scan, so a query never runs on the frame loop. Completions are
//! generation-tagged and land latest-wins, like the world worker's (specs/tui.md Refresh).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use fff_search::{
    FFFQuery, FilePicker, FilePickerOptions, FileSearchConfig, FrecencyTracker, FuzzySearchOptions,
    GrepSearchOptions, PaginationArgs, SharedFilePicker, SharedFrecency,
};

/// The most results one query fetches per group. The overlay list scrolls, so every
/// fetched result is reachable; anything past the cap shows in the `… N more` count.
const FILE_LIMIT: usize = 50;
const CODE_LIMIT: usize = 200;
/// Cap on one grep's runtime, so a pathological query returns partial results instead of
/// pinning the worker while newer keystrokes queue.
const GREP_BUDGET_MS: u64 = 80;
/// How long a not-yet-warm worker waits for the next keystroke before re-checking whether
/// the scan finished and the pending query can run for real.
const WARMUP_POLL: Duration = Duration::from_millis(50);

/// The engine's cache home. The frecency store lives here, never the worktree
/// (specs/search.md).
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("herdr-reviewr")
}

/// One request to the worker.
#[derive(Debug)]
pub enum SearchJob {
    /// Run `query`; the completion echoes the generation back.
    Query { generation: u64, query: String },
    /// Record a picked result in the engine's frecency store, so ranking improves with
    /// use (specs/search.md).
    Track { path: String },
}

/// A path match, in engine order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHit {
    pub path: String,
    /// Byte spans into `path` the engine matched — the emphasis input.
    pub spans: Vec<(u32, u32)>,
}

/// A content match, in engine order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeHit {
    pub path: String,
    /// 1-based line number.
    pub line: u64,
    pub text: String,
    /// Byte spans into `text` the engine matched — the emphasis input.
    pub spans: Vec<(u32, u32)>,
}

/// One query's results, both groups. `file_total` is the engine's full match count;
/// `code_more` marks a grep the page cap or time budget cut short.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchResults {
    pub files: Vec<FileHit>,
    pub code: Vec<CodeHit>,
    pub file_total: usize,
    pub code_more: bool,
}

/// A finished query's outcome — the three states the overlay's phases mirror, at the wire
/// layer (specs/search.md).
#[derive(Debug)]
pub enum SearchOutcome {
    /// Results for the query; the previously landed set stays painted until this lands.
    Ready(SearchResults),
    /// The engine's first scan is still running — the overlay shows `indexing…` and the
    /// worker re-runs the query when the scan lands.
    Indexing,
    /// The engine failed; its message shows in the results pane.
    Failed(String),
}

/// A finished query: the tag it ran for, and its outcome.
#[derive(Debug)]
pub struct SearchCompletion {
    pub generation: u64,
    pub outcome: SearchOutcome,
}

/// The engine, initialized once on the worker thread.
struct Engine {
    shared: SharedFilePicker,
    frecency: SharedFrecency,
    repo: PathBuf,
}

impl Engine {
    /// Start the picker's background scan, watcher, and content indexing. The frecency
    /// store opens under `cache_dir`, never the worktree (specs/search.md).
    fn start(repo: PathBuf, cache_dir: &Path) -> Result<Self, String> {
        let shared = SharedFilePicker::default();
        let frecency = SharedFrecency::default();
        match std::fs::create_dir_all(cache_dir) {
            Ok(()) => match FrecencyTracker::open(cache_dir.join("frecency")) {
                Ok(tracker) => {
                    if let Err(e) = frecency.init(tracker) {
                        logln!("search frecency init failed: {e}");
                    }
                }
                // Search works without frecency; ranking just stops improving with use.
                Err(e) => logln!("search frecency open failed: {e}"),
            },
            Err(e) => logln!("search cache dir failed: {e}"),
        }
        FilePicker::new_with_shared_state(
            shared.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: repo.to_string_lossy().into_owned(),
                enable_content_indexing: true,
                ..Default::default()
            },
        )
        .map_err(|e| e.to_string())?;
        Ok(Self { shared, frecency, repo })
    }

    fn warm(&self) -> bool {
        self.shared.wait_for_scan(Duration::from_millis(1))
    }

    /// Run one query against the warm index: the path group, then the content group.
    fn run(&self, raw: &str) -> Result<SearchResults, String> {
        let guard = self.shared.read().map_err(|e| e.to_string())?;
        let picker = guard.as_ref().ok_or("search engine not ready")?;
        let query = FFFQuery::parse(raw, FileSearchConfig);

        let found = picker.fuzzy_search(
            &query,
            None,
            FuzzySearchOptions {
                pagination: PaginationArgs { offset: 0, limit: FILE_LIMIT },
                ..Default::default()
            },
        );
        let files: Vec<FileHit> = found
            .items
            .iter()
            .zip(&found.match_byte_offsets)
            .map(|(i, spans)| FileHit {
                path: i.relative_path(picker),
                spans: spans.iter().copied().collect(),
            })
            .collect();
        let file_total = found.total_matched.max(files.len());

        // The empty query paints the frecency-ranked Files group alone: an empty grep is
        // engine-defined noise, not something the spec describes (specs/search.md).
        if raw.trim().is_empty() {
            return Ok(SearchResults { files, code: Vec::new(), file_total, code_more: false });
        }

        let mut grep = picker.grep(
            &query,
            &GrepSearchOptions {
                page_limit: CODE_LIMIT,
                time_budget_ms: GREP_BUDGET_MS,
                ..Default::default()
            },
        );
        // Drop each match line's leading indentation so the row text aligns at the left in
        // the narrow pane; the engine adjusts its match offsets as it trims. The preview
        // keeps the true indentation (specs/search.md).
        for m in &mut grep.matches {
            m.trim_leading_whitespace();
        }
        let code: Vec<CodeHit> = grep
            .matches
            .iter()
            .map(|m| CodeHit {
                path: grep.files[m.file_index].relative_path(picker),
                line: m.line_number,
                text: m.line_content.clone(),
                spans: m.match_byte_offsets.iter().copied().collect(),
            })
            .collect();
        let code_more = grep.next_file_offset != 0;

        Ok(SearchResults { files, code, file_total, code_more })
    }

    /// Record a pick in the frecency store. Failures only log — a pick must always land.
    fn track(&self, path: &str) {
        match self.frecency.read() {
            Ok(guard) => {
                if let Some(tracker) = guard.as_ref()
                    && let Err(e) = tracker.track_access(&self.repo.join(path))
                {
                    logln!("search frecency track failed: {e}");
                }
            }
            Err(e) => logln!("search frecency lock failed: {e}"),
        }
    }
}

/// Run the search worker until the request channel closes. Queued queries coalesce into
/// the newest; a query that arrives before the first scan finishes completes as
/// `indexing…` and re-runs when the scan lands.
pub fn spawn(
    repo: PathBuf,
    cache_dir: PathBuf,
    rx: Receiver<SearchJob>,
    tx: Sender<SearchCompletion>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("search".into())
        .spawn(move || {
            let engine = match Engine::start(repo, &cache_dir) {
                Ok(engine) => engine,
                Err(e) => {
                    // Report on the first query, then exit: without an engine every later
                    // request would fail the same way.
                    if let Ok(SearchJob::Query { generation, .. }) = rx.recv() {
                        let outcome = SearchOutcome::Failed(e);
                        let _ = tx.send(SearchCompletion { generation, outcome });
                    }
                    return;
                }
            };
            // The query awaiting a warm index, re-run when the scan lands.
            let mut pending: Option<(u64, String)> = None;
            loop {
                let request = if pending.is_some() {
                    match rx.recv_timeout(WARMUP_POLL) {
                        Ok(request) => Some(request),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                } else {
                    match rx.recv() {
                        Ok(request) => Some(request),
                        Err(_) => break,
                    }
                };
                let mut job = None;
                if let Some(request) = request {
                    match request {
                        SearchJob::Query { generation, query } => {
                            job = Some((generation, query));
                        }
                        SearchJob::Track { path } => engine.track(&path),
                    }
                }
                // Latest keystroke wins: drain the queue before running anything.
                while let Ok(next) = rx.try_recv() {
                    match next {
                        SearchJob::Query { generation, query } => {
                            job = Some((generation, query));
                        }
                        SearchJob::Track { path } => engine.track(&path),
                    }
                }
                // A fresh job supersedes any query still parked for warm-up, so a stale
                // generation never burns a grep after the scan lands.
                if job.is_some() {
                    pending = None;
                }
                let Some((generation, query)) = job.or_else(|| pending.take()) else {
                    continue;
                };
                if !engine.warm() {
                    pending = Some((generation, query));
                    let outcome = SearchOutcome::Indexing;
                    let _ = tx.send(SearchCompletion { generation, outcome });
                    continue;
                }
                let outcome = match engine.run(&query) {
                    Ok(results) => SearchOutcome::Ready(results),
                    Err(e) => SearchOutcome::Failed(e),
                };
                let completion = SearchCompletion { generation, outcome };
                if tx.send(completion).is_err() {
                    break;
                }
            }
        })
        .expect("spawn search worker")
}
