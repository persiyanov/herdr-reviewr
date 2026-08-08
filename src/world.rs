//! The world snapshot: the derived state one refresh produces, built from git alone.
//!
//! `build` reads nothing from `App`, so the same call runs synchronously (startup, scope
//! switches, first visits) and behind the worker (polls, `r`, return visits)
//! (specs/tui.md Refresh). Reconciling a snapshot into place state stays
//! in `App::reconcile_world`, the one home for the Continuity rules (specs/overview.md).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use anyhow::Result;

use crate::app::Tab;
use crate::file_list::{Annotation, Entry};
use crate::git;
use crate::herdr::AgentSample;
use crate::model::{ChangedFile, Scope};
use crate::turn::{TurnTracker, WorktreeState};

/// Everything the build reads. A landed snapshot reconciles only while the view still
/// matches the input that produced it (specs/tui.md).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorldInput {
    pub repo: PathBuf,
    pub tab: Tab,
    pub scope: Scope,
    /// The `--base` flag, resolved fresh per build. The pick is read from its ref at build
    /// time, so it is derived output, never input identity — a pick made in another pane
    /// must land here as newer content, not be discarded as a mismatch
    /// (specs/review-model.md).
    pub base: Option<String>,
    /// Bumped by this pane's own pick, so a build that read the previous pick fails the
    /// landing's input-equality gate instead of reverting the picked base. Another pane's
    /// pick leaves it alone — see `base` above.
    pub base_epoch: u64,
    /// The `last-turn` baseline tree the changed set diffs against; `None` before a turn.
    pub turn_baseline: Option<String>,
    /// Expanded ignored directories whose children the `All files` tree loads.
    pub toggled_dirs: HashSet<String>,
}

/// The derived state one refresh produces: the scope changeset, the navigator entries, and
/// the `branch` scope's resolved base. The base rides the snapshot so the header name and
/// the changeset it heads land whole, from one build (specs/tui.md).
#[derive(Debug)]
pub struct WorldSnapshot {
    pub changed: HashMap<String, Annotation>,
    pub entries: Vec<Entry>,
    pub branch_base: git::BaseStatus,
}

/// Build the snapshot for `input`. The changeset is computed regardless of tab so the
/// header count and comment staleness stay correct while `All files` lists the whole
/// worktree. In `last-turn` with no baseline yet, the changeset is empty until a turn
/// start is observed (specs/review-model.md).
pub fn build(input: &WorldInput) -> Result<WorldSnapshot> {
    // Outside a git repo, an empty snapshot paints the quiet empty state rather than a
    // failing status line every poll (specs/herdr-host.md).
    if !git::is_repo(&input.repo) {
        return Ok(WorldSnapshot {
            changed: HashMap::new(),
            entries: Vec::new(),
            branch_base: git::BaseStatus::default(),
        });
    }
    let (branch_base, changed) = build_changed(input)?;
    let changed_map = annotate(&changed);
    let entries = match input.tab {
        // The whole worktree (ignored included), with expanded ignored dirs loaded lazily.
        Tab::AllFiles => all_files_entries(input, &changed_map)?,
        // `Changes` (the `PR` tab never builds a snapshot).
        _ => changed.iter().map(Entry::from_changed).collect(),
    };
    Ok(WorldSnapshot { changed: changed_map, entries, branch_base })
}

/// The active scope's changed files and, on the `branch` scope, the base they diff against —
/// the piece a scope switch rebuilds before its frame, so the header count and list never
/// wear another scope's label (specs/tui.md).
pub fn build_changed(input: &WorldInput) -> Result<(git::BaseStatus, Vec<ChangedFile>)> {
    let none = git::BaseStatus::default;
    if !git::is_repo(&input.repo) {
        return Ok((none(), Vec::new()));
    }
    match input.scope {
        Scope::LastTurn => match input.turn_baseline.as_deref() {
            Some(t) => Ok((none(), git::changed_against_tree(&input.repo, t)?)),
            None => Ok((none(), Vec::new())),
        },
        Scope::Uncommitted => Ok((none(), git::changed_files(&input.repo, input.scope, None)?)),
        Scope::Branch => {
            // A resolve failure fails the build whole, so the landing keeps the stale
            // frame and reports — degrading to an empty snapshot would blank a populated
            // view over a transient error (specs/overview.md Continuity). A chain where
            // nothing resolves is not a failure: it returns the legible no-base state.
            let resolution = git::resolve_base(&input.repo, input.base.as_deref())
                .map_err(|e| anyhow::anyhow!("{}", e.0))?;
            let base_oid = resolution.status.winner.as_ref().map(|w| w.oid.clone());
            let changed = git::changed_files(&input.repo, input.scope, base_oid.as_deref())?;
            Ok((resolution.status, changed))
        }
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
/// edge (specs/herdr-host.md). The baseline ref stays reviewr's only git write.
#[derive(Debug)]
pub struct TurnHost {
    tracker: TurnTracker,
    repo: PathBuf,
    turn_key: String,
    /// Each agent `cwd` resolved to whether it is a member of the reviewed worktree. Only a
    /// resolved git top level is recorded, since a worktree root does not move, so a member is
    /// placed once and never re-queried. A cwd git reports outside every worktree is not cached:
    /// re-checking it is cheap, and a directory can become a worktree later. A cwd git could not
    /// run for is not cached either, and holds the poll rather than counting the agent out, so a
    /// transient failure never poisons a member for the session (specs/herdr-host.md).
    resolved: HashMap<String, bool>,
}

/// One sample's outcome, sent back with the completion: whether it ended a turn (the `PR`
/// tab's refetch signal), and what this sample saw of the worktree's membership (the
/// `last-turn` empty state). The baseline itself rides the completion's input.
#[derive(Clone, Debug)]
pub struct TurnReport {
    pub ended: bool,
    /// `None` when the sample could not observe the whole worktree, so the reader keeps whatever
    /// it already knew: either the enumeration failed, or a member's directory would not resolve
    /// this poll. Membership is held on the one consumer that paints it, never mirrored here
    /// (specs/herdr-host.md).
    pub agents_present: Option<bool>,
}

/// An agent's relationship to the reviewed worktree, as [`TurnHost::membership`] resolves it.
/// `Unknown` is not `NotMember`: it means git could not resolve the cwd this poll, so the fold
/// holds on it rather than counting the agent out (specs/herdr-host.md).
enum Membership {
    Member,
    NotMember,
    Unknown,
}

/// The absolute cwd an agent names, or `None` for a blank or relative one. `git -C` resolves a
/// relative directory against reviewr's own cwd, which is normally the reviewed worktree, so a
/// relative cwd would be wrongly admitted as a member (specs/herdr-host.md).
fn worktree_cwd(cwd: Option<&str>) -> Option<&str> {
    cwd.filter(|c| Path::new(c).is_absolute())
}

/// Fold the members' statuses into the worktree's work state and whether any member is present,
/// or `None` if a member's membership was undetermined — the caller then holds the sample.
/// Pure over the `member` resolver so the fold-and-hold rule is unit-testable without git.
fn classify(
    samples: &[AgentSample],
    mut member: impl FnMut(&AgentSample) -> Membership,
) -> Option<(bool, WorktreeState)> {
    let mut members = Vec::new();
    for sample in samples {
        match member(sample) {
            Membership::Member => members.push(sample.status),
            Membership::NotMember => {}
            Membership::Unknown => return None,
        }
    }
    Some((!members.is_empty(), WorktreeState::fold(members)))
}

impl TurnHost {
    /// Resume any persisted turn baseline for this worktree, so `last-turn` keeps its
    /// anchor across a reviewr pane restart (specs/herdr-host.md).
    /// `repo` must already be the git top level, as [`crate::world::seed_baseline`] and
    /// membership both compare against it and `App` derives the same baseline-ref key from
    /// its own copy — normalizing here instead would key the two apart. `run` resolves it
    /// once for both (`src/lib.rs`).
    pub fn open(repo: PathBuf) -> Self {
        let tracker = TurnTracker::with_baseline(seed_baseline(&repo));
        let turn_key = git::worktree_key(&repo);
        Self { tracker, repo, turn_key, resolved: HashMap::new() }
    }

    pub fn baseline(&self) -> Option<&str> {
        self.tracker.baseline()
    }

    /// Sample the agents over the herdr CLI and advance the baseline. A missing herdr is
    /// normal, so a failed enumeration only logs and changes nothing.
    pub fn sample(&mut self) -> TurnReport {
        self.observe_agents(crate::herdr::agent_samples().ok().as_deref())
    }

    /// Advance the baseline from one enumeration — the core [`Self::sample`] wraps, and the
    /// seam tests drive without herdr. `None` is a failed enumeration, which holds the
    /// previous membership rather than reporting an empty worktree (specs/herdr-host.md).
    pub fn observe_agents(&mut self, samples: Option<&[AgentSample]>) -> TurnReport {
        let Some(samples) = samples else {
            return TurnReport { ended: false, agents_present: None };
        };
        // A member whose membership git could not determine leaves the sample incomplete, so
        // hold it exactly as a failed enumeration rather than reading an unresolved member as
        // an empty worktree (specs/herdr-host.md).
        let Some((present, state)) = classify(samples, |s| self.membership(s.cwd.as_deref()))
        else {
            return TurnReport { ended: false, agents_present: None };
        };
        let ended = self.observe(state);
        TurnReport { ended, agents_present: Some(present) }
    }

    /// An agent's relationship to the reviewed worktree. The git top level is authoritative, so
    /// a subdirectory is a member and a second worktree of the same repository is not
    /// (specs/herdr-host.md).
    fn membership(&mut self, cwd: Option<&str>) -> Membership {
        let Some(cwd) = worktree_cwd(cwd) else {
            return Membership::NotMember;
        };
        if let Some(&member) = self.resolved.get(cwd) {
            return if member { Membership::Member } else { Membership::NotMember };
        }
        match git::worktree_of(Path::new(cwd)) {
            // A resolved root is stable, so record whether it is a member and never shell out
            // for this cwd again. git canonicalizes it, so the worktree root itself matches too.
            git::Worktree::Root(top) => {
                let member = top == self.repo;
                self.resolved.insert(cwd.to_string(), member);
                if member { Membership::Member } else { Membership::NotMember }
            }
            // git ran and found no worktree. A determination, but not a stable one, so it is
            // re-checked next poll rather than cached.
            git::Worktree::Outside => Membership::NotMember,
            // git could not run, so nothing is known this poll. Hold rather than count the agent
            // out, exactly as a failed enumeration does (specs/herdr-host.md).
            git::Worktree::Unknown => Membership::Unknown,
        }
    }

    /// Advance the baseline from one folded worktree state, returning whether a turn ended.
    /// On a turn start it snapshots the worktree as the candidate; while a candidate is
    /// pending it promotes once the worktree diverges from it, persisting the new baseline.
    /// Git errors only log, so a transient git failure never crashes the poll.
    fn observe(&mut self, state: WorktreeState) -> bool {
        let transition = self.tracker.observe(state);
        if transition.started {
            match git::snapshot_worktree(&self.repo) {
                // The candidate is this worktree as of a moment ago, so it cannot have
                // diverged from it yet. The next poll runs the check, which is what makes
                // this an early return rather than a second snapshot of the same tree.
                Ok(sha) => {
                    self.tracker.set_candidate(sha);
                    return transition.ended;
                }
                Err(e) => logln!("turn snapshot failed: {e}"),
            }
        }
        // Promote the pending candidate once the turn has changed a file. Compare full
        // snapshots so a new untracked file counts as a change (specs/herdr-host.md).
        let Some(candidate) = self.tracker.candidate().map(str::to_string) else {
            return transition.ended;
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
        transition.ended
    }
}

/// One queued refresh's attributes, accumulated on `App` until the loop dispatches it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorldRequest {
    /// Sample the agents in the worktree — set by the poll alone (specs/tui.md).
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
    /// Poll-driven requests sample the agents in the worktree; tab entry and `r` do not,
    /// so the herdr CLI call count tracks the poll alone (specs/tui.md).
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

#[cfg(test)]
mod tests {
    use super::{Membership, classify, worktree_cwd};
    use crate::herdr::AgentSample;
    use crate::turn::{Status, WorktreeState};

    fn working_at(cwd: &str) -> AgentSample {
        AgentSample { cwd: Some(cwd.into()), status: Status::Working }
    }

    #[test]
    fn only_an_absolute_cwd_can_name_a_worktree() {
        // A blank or relative cwd would resolve against reviewr's own cwd (the reviewed
        // worktree), so membership must reject it before any git call (specs/herdr-host.md).
        assert_eq!(worktree_cwd(Some("/abs/path")), Some("/abs/path"));
        assert_eq!(worktree_cwd(Some("relative/path")), None);
        assert_eq!(worktree_cwd(Some("")), None);
        assert_eq!(worktree_cwd(None), None);
    }

    #[test]
    fn membership_decides_the_fold_and_undetermined_holds() {
        // One working agent, resolved three ways. `Unknown` holds the sample (the caller reads
        // this `None` exactly as a failed enumeration, never as an empty worktree); a determined
        // verdict folds normally (specs/herdr-host.md).
        let samples = [working_at("/w")];
        assert_eq!(classify(&samples, |_| Membership::Unknown), None);
        assert_eq!(
            classify(&samples, |_| Membership::Member),
            Some((true, WorktreeState::Working))
        );
        assert_eq!(
            classify(&samples, |_| Membership::NotMember),
            Some((false, WorktreeState::Resting))
        );
    }

    #[test]
    fn one_undetermined_member_holds_even_beside_a_resolved_one() {
        // A resolved working member does not rescue a sample that also holds an unknown one: an
        // incomplete view of the worktree is held whole, not folded from the part that resolved.
        let samples = [working_at("/a"), working_at("/b")];
        let held = classify(&samples, |s| match s.cwd.as_deref() {
            Some("/b") => Membership::Unknown,
            _ => Membership::Member,
        });
        assert_eq!(held, None);
    }

    #[test]
    fn a_non_members_status_never_reaches_the_fold() {
        // A member resting and a non-member (a sibling worktree) working. Only the member's
        // status folds, so the worktree reads Resting, never the sibling's Working.
        let samples = [
            AgentSample { cwd: Some("/mine".into()), status: Status::Idle },
            AgentSample { cwd: Some("/sibling".into()), status: Status::Working },
        ];
        let folded = classify(&samples, |s| match s.cwd.as_deref() {
            Some("/sibling") => Membership::NotMember,
            _ => Membership::Member,
        });
        assert_eq!(folded, Some((true, WorktreeState::Resting)));
    }
}
