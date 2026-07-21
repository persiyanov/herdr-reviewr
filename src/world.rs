//! The world snapshot: the derived state one refresh produces, built from git alone.
//!
//! `build` reads nothing from `App`, so the same call runs synchronously (startup, scope
//! switches, first visits) and behind the worker (polls, `r`, return visits)
//! (specs/tui.md Refresh). Reconciling a snapshot into place state stays
//! in `App::reconcile_world`, the one home for the Continuity rules (specs/overview.md).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use anyhow::Result;

use crate::app::Tab;
use crate::file_list::{Annotation, Entry};
use crate::git;
use crate::model::{ChangedFile, Scope};
use crate::turn::{Status, TurnTracker};

/// Everything the build reads. A landed snapshot reconciles only while the view still
/// matches the input that produced it (specs/tui.md).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorldInput {
    pub repo: PathBuf,
    pub tab: Tab,
    pub scope: Scope,
    pub base: Option<String>,
    pub base_branches: Vec<String>,
    /// The `last-turn` baseline tree the changed set diffs against; `None` before a turn.
    pub turn_baseline: Option<String>,
    /// Expanded ignored directories whose children the `All files` tree loads.
    pub toggled_dirs: HashSet<String>,
}

/// The derived state one refresh produces: the scope changeset and the navigator entries.
#[derive(Debug)]
pub struct WorldSnapshot {
    pub changed: HashMap<String, Annotation>,
    pub entries: Vec<Entry>,
}

/// Build the snapshot for `input`. The changeset is computed regardless of tab so the
/// header count and comment staleness stay correct while `All files` lists the whole
/// worktree. In `last-turn` with no baseline yet, the changeset is empty until a turn
/// start is observed (specs/review-model.md).
pub fn build(input: &WorldInput) -> Result<WorldSnapshot> {
    // Outside a git repo, an empty snapshot paints the quiet empty state rather than a
    // failing status line every poll (specs/herdr-host.md).
    if !git::is_repo(&input.repo) {
        return Ok(WorldSnapshot { changed: HashMap::new(), entries: Vec::new() });
    }
    let changed = build_changed(input)?;
    let changed_map = annotate(&changed);
    let entries = match input.tab {
        // The whole worktree (ignored included), with expanded ignored dirs loaded lazily.
        Tab::AllFiles => all_files_entries(input, &changed_map)?,
        // `Changes` (the `PR` tab never builds a snapshot).
        _ => changed.iter().map(Entry::from_changed).collect(),
    };
    Ok(WorldSnapshot { changed: changed_map, entries })
}

/// The active scope's changed files alone — the piece a scope switch rebuilds before its
/// frame, so the header count and list never wear another scope's label (specs/tui.md).
pub fn build_changed(input: &WorldInput) -> Result<Vec<ChangedFile>> {
    if !git::is_repo(&input.repo) {
        return Ok(Vec::new());
    }
    match input.scope {
        Scope::LastTurn => match input.turn_baseline.as_deref() {
            Some(t) => git::changed_against_tree(&input.repo, t),
            None => Ok(Vec::new()),
        },
        _ => git::changed_files(
            &input.repo,
            input.scope,
            input.base.as_deref(),
            &input.base_branches,
        ),
    }
}

/// The changed-files map every consumer keys by path — one construction site, shared by
/// the worker build and the scope switch's synchronous rebuild.
pub fn annotate(changed: &[ChangedFile]) -> HashMap<String, Annotation> {
    changed.iter().map(|f| (f.path.clone(), Annotation::from(f))).collect()
}

/// The persisted turn baseline for `repo`, if any — the one seeding rule, shared by the
/// worker's tracker and the app's first-frame mirror (specs/herdr-host.md).
pub fn seed_baseline(repo: &std::path::Path) -> Option<String> {
    git::read_baseline_ref(repo, &git::worktree_key(repo))
}

/// The `All files` entries: every worktree path (ignored dimmed), with the children of
/// expanded ignored directories loaded lazily (`specs/file-list.md`). Only directories the
/// user has expanded are walked, so the cost tracks what is on screen, not the whole tree.
pub(crate) fn all_files_entries(
    input: &WorldInput,
    changed: &HashMap<String, Annotation>,
) -> Result<Vec<Entry>> {
    let to_entry = |w: git::WorktreeEntry| Entry {
        annotation: changed.get(&w.path).cloned(),
        path: w.path,
        previous_path: None,
        ignored: w.ignored,
        is_dir: w.is_dir,
    };
    let mut entries: Vec<Entry> = git::all_files(&input.repo)?.into_iter().map(&to_entry).collect();
    let mut i = 0;
    while i < entries.len() {
        if entries[i].is_dir && input.toggled_dirs.contains(&entries[i].path) {
            let path = entries[i].path.clone();
            let children = git::list_ignored_dir(&input.repo, &path).into_iter().map(&to_entry);
            entries.extend(children);
        }
        i += 1;
    }
    Ok(entries)
}

/// Turn tracking, owned by the worker: the sample, the snapshot capture, and the baseline
/// promotion happen on one thread, so the snapshot always rides the sample that observed the
/// edge (specs/herdr-host.md). The baseline ref stays the sidebar's only git write.
#[derive(Debug)]
pub struct TurnHost {
    tracker: TurnTracker,
    repo: PathBuf,
    turn_key: String,
}

/// One sample's outcome, sent back with the completion: whether it ended a turn (the `PR`
/// tab's refetch signal). The baseline itself rides the completion's input.
#[derive(Clone, Debug)]
pub struct TurnReport {
    pub ended: bool,
}

impl TurnHost {
    /// Resume any persisted turn baseline for this worktree, so `last-turn` keeps its
    /// anchor across a sidebar restart (specs/herdr-host.md).
    pub fn open(repo: PathBuf) -> Self {
        let tracker = TurnTracker::with_baseline(seed_baseline(&repo));
        let turn_key = git::worktree_key(&repo);
        Self { tracker, repo, turn_key }
    }

    pub fn baseline(&self) -> Option<&str> {
        self.tracker.baseline()
    }

    /// Sample the agent's status over the herdr CLI and advance the baseline. Absence or
    /// ambiguity pauses tracking; a missing herdr is normal, so failures only log.
    pub fn sample(&mut self) -> TurnReport {
        self.observe(crate::herdr::resolved_agent_status().ok().flatten())
    }

    /// Advance the baseline from one status sample — the core [`Self::sample`] wraps, and
    /// the seam tests drive without herdr. On a turn start (a resting→`working` edge) it
    /// snapshots the worktree as the candidate; while a candidate is pending it promotes
    /// once the worktree diverges from it, persisting the new baseline. Git errors only
    /// log, so a transient git failure never crashes the poll.
    pub fn observe(&mut self, status: Option<Status>) -> TurnReport {
        let Some(status) = status else { return TurnReport { ended: false } };
        let transition = self.tracker.observe(status);
        if transition.started {
            match git::snapshot_worktree(&self.repo) {
                Ok(sha) => self.tracker.set_candidate(sha),
                Err(e) => logln!("turn snapshot failed: {e}"),
            }
        }
        // Promote the pending candidate once the turn has changed a file. Compare full
        // snapshots so a new untracked file counts as a change (specs/herdr-host.md).
        let Some(candidate) = self.tracker.candidate().map(str::to_string) else {
            return TurnReport { ended: transition.ended };
        };
        match git::snapshot_worktree(&self.repo) {
            Ok(now) if now != candidate => {
                self.tracker.promote();
                if let Err(e) = git::write_baseline_ref(&self.repo, &self.turn_key, &candidate) {
                    logln!("turn baseline ref write failed: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => logln!("turn divergence check failed: {e}"),
        }
        TurnReport { ended: transition.ended }
    }
}

/// One queued refresh's attributes, accumulated on `App` until the loop dispatches it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorldRequest {
    /// Sample the agent's status — set by the poll alone (specs/tui.md).
    pub sample_turn: bool,
    /// Re-reveal the cursor when the result lands — user-initiated switches only.
    pub reveal: bool,
}

/// One refresh request. The worker builds against `input`, refreshing its `turn_baseline`
/// from the sample first, and echoes the tag back with the completion.
#[derive(Debug)]
pub struct WorldJob {
    pub generation: u64,
    pub input: WorldInput,
    /// Poll-driven requests sample the agent's status; tab entry and `r` do not, so the
    /// herdr CLI call count tracks the poll alone (specs/tui.md).
    pub sample_turn: bool,
    /// A user-initiated switch re-reveals the cursor when its result lands; a poll never
    /// does (specs/tui.md).
    pub reveal: bool,
}

/// A finished job: the tag it was built for, the sample's outcome (`None` when the job
/// didn't sample — a tab entry or `r`, not a poll), and the snapshot — `None` when the
/// input's tab builds no file tree (the `PR` tab).
#[derive(Debug)]
pub struct WorldCompletion {
    pub generation: u64,
    pub input: WorldInput,
    pub reveal: bool,
    pub turn: Option<TurnReport>,
    pub snapshot: Option<Result<WorldSnapshot>>,
}

/// Run the world worker until the request channel closes. The latest request wins: queued
/// requests coalesce into the newest, keeping any superseded job's sample and reveal flags
/// so a poll's status sample is never skipped.
pub fn spawn(
    mut host: TurnHost,
    rx: Receiver<WorldJob>,
    tx: Sender<WorldCompletion>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("world".into())
        .spawn(move || {
            while let Ok(mut job) = rx.recv() {
                while let Ok(next) = rx.try_recv() {
                    job = WorldJob {
                        sample_turn: job.sample_turn || next.sample_turn,
                        reveal: job.reveal || next.reveal,
                        ..next
                    };
                }
                let turn = job.sample_turn.then(|| host.sample());
                job.input.turn_baseline = host.baseline().map(str::to_string);
                let snapshot = job.input.tab.is_file_tab().then(|| build(&job.input));
                let completion = WorldCompletion {
                    generation: job.generation,
                    input: job.input,
                    reveal: job.reveal,
                    turn,
                    snapshot,
                };
                if tx.send(completion).is_err() {
                    break;
                }
            }
        })
        .expect("spawn world worker")
}
