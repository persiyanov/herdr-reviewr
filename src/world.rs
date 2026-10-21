//! The world snapshot: what one refresh derives from git alone, built on the caller or the worker.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use anyhow::{Context, Result, bail};

use crate::app::Tab;
use crate::file_list::Entry;
use crate::git;
use crate::model::{ChangeKind, ChangedFile, CommitPick, ReviewContext, Scope};

/// Everything the build reads; a snapshot lands only while the view still matches it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorldInput {
    pub repo: PathBuf,
    pub tab: Tab,
    pub scope: Scope,
    /// The `--base` flag. The pick is read at build time, so another pane's pick lands as content.
    pub base: Option<String>,
    /// Bumped by this pane's pick, so a build of the previous pick never lands.
    pub base_epoch: u64,
    /// The `last-turn` baseline tree the changed set diffs against; `None` before a turn.
    pub turn_baseline: Option<String>,
    /// The `commits` scope's pick, so a build of a replaced pick never lands.
    pub commit_pick: Option<CommitPick>,
    /// Expanded ignored directories whose children the `All files` tree loads.
    pub toggled_dirs: HashSet<String>,
}

/// One refresh's result; the base rides along so the header and its changeset land together.
#[derive(Clone, Debug)]
pub struct WorldSnapshot {
    pub review_context: ReviewContext,
    pub changeset: Changeset,
    pub entries: Vec<Entry>,
    pub branch_base: git::BaseStatus,
    /// The `commits` scope's pick verdict; `None` on every other scope.
    pub pick_status: Option<PickStatus>,
    /// `HEAD` at build time, the commit picker's key; `None` when unborn.
    pub head: Option<String>,
    /// The paths a path-limited build re-read; `None` for a full build, which re-read everything.
    pub touched: Option<BTreeSet<String>>,
}

/// What one build found the commit pick to be.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PickVerdict {
    /// Every commit reachable from `HEAD`.
    Live,
    /// Some commit unreachable from `HEAD`. The run still paints.
    OffBranch,
    /// A needed commit is pruned, named here. The scope is empty.
    Gone(String),
}

/// The pick's verdict and the newest commit's subject, for the header.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PickStatus {
    pub verdict: PickVerdict,
    pub subject: String,
    /// How many commits the run spans, `0` when `gone` or not a run.
    pub count: usize,
}

/// The ends a changeset was diffed between (`new` `None` for the worktree); a file's diff reads these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffEnds {
    pub old: String,
    pub new: Option<String>,
}

/// A scope's changed files by path and the ends they were diffed between, landed together.
#[derive(Clone, Debug, Default)]
pub struct Changeset {
    pub files: BTreeMap<String, ChangedFile>,
    /// `None` when the scope has nothing to diff: no baseline, base, or live pick.
    pub ends: Option<DiffEnds>,
}

/// A build's changeset and the base or pick it diffs against, landed together.
#[derive(Debug, Default)]
pub struct ScopeBuild {
    pub review_context: ReviewContext,
    pub branch_base: git::BaseStatus,
    pub pick_status: Option<PickStatus>,
    pub changeset: Changeset,
}

/// Distinguish a directory outside Git from an established repository that temporarily failed.
fn repository_available(repo: &Path) -> Result<bool> {
    match git::worktree_of(repo) {
        git::Worktree::Root(_) => Ok(true),
        git::Worktree::Unknown => bail!("unable to probe repository at {}", repo.display()),
        git::Worktree::Outside => match std::fs::symlink_metadata(repo.join(".git")) {
            Ok(_) => bail!("unable to probe established repository at {}", repo.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error)
                .with_context(|| format!("checking repository marker at {}", repo.display())),
        },
    }
}

/// Build the snapshot for `input`; the changeset is built on every tab.
pub fn build(input: &WorldInput) -> Result<WorldSnapshot> {
    // Outside a repo, paint the quiet empty state, not an error every refresh.
    if !repository_available(&input.repo)? {
        return Ok(WorldSnapshot {
            review_context: context_without_git(input),
            changeset: Changeset::default(),
            entries: Vec::new(),
            branch_base: git::BaseStatus::default(),
            pick_status: None,
            head: None,
            touched: None,
        });
    }
    // One read of HEAD serves the snapshot and the uncommitted diff's old end.
    let head = git::head_oid(&input.repo);
    let ScopeBuild { review_context, branch_base, pick_status, changeset } =
        scope_build(input, head.clone())?;
    let entries = match input.tab {
        // The whole worktree (ignored included), with expanded ignored dirs loaded lazily.
        Tab::AllFiles => all_files_entries(input, &changeset.files)?,
        // `Changes` (the `PR` tab never builds a snapshot).
        _ => changeset.files.values().map(Entry::from_changed).collect(),
    };
    Ok(WorldSnapshot {
        review_context,
        changeset,
        entries,
        branch_base,
        pick_status,
        head,
        touched: None,
    })
}

/// The active scope's changeset and, on `branch`, its base.
pub fn build_changed(input: &WorldInput) -> Result<ScopeBuild> {
    if !repository_available(&input.repo)? {
        return Ok(ScopeBuild {
            review_context: context_without_git(input),
            ..ScopeBuild::default()
        });
    }
    scope_build(input, git::head_oid(&input.repo))
}

/// [`build_changed`] against `head`, as the caller read it, inside a repo.
fn scope_build(input: &WorldInput, head: Option<String>) -> Result<ScopeBuild> {
    match input.scope {
        Scope::LastTurn => match input.turn_baseline.as_deref() {
            Some(t) => {
                let now = git::snapshot_worktree(&input.repo)?;
                at_ends(
                    &input.repo,
                    ReviewContext::LastTurn { baseline: Some(t.to_string()) },
                    DiffEnds { old: t.to_string(), new: Some(now) },
                )
            }
            None => Ok(ScopeBuild {
                review_context: ReviewContext::LastTurn { baseline: None },
                ..ScopeBuild::default()
            }),
        },
        Scope::Uncommitted => {
            let base = git::diff_base(head);
            at_ends(&input.repo, ReviewContext::Uncommitted, DiffEnds { old: base, new: None })
        }
        Scope::Branch => {
            // A resolve failure fails the build, keeping the stale frame.
            let resolution = git::resolve_base(&input.repo, input.base.as_deref())?;
            let merge_base = match (head, resolution.status.winner.as_ref()) {
                (Some(_), Some(winner)) => git::merge_base_checked(&input.repo, winner.oid())?,
                _ => None,
            };
            let review_context = ReviewContext::Branch {
                base: resolution.status.winner.as_ref().map(|winner| winner.name().to_string()),
            };
            let build = match merge_base {
                Some(base) => {
                    at_ends(&input.repo, review_context.clone(), DiffEnds { old: base, new: None })?
                }
                None => ScopeBuild { review_context, ..ScopeBuild::default() },
            };
            Ok(ScopeBuild { branch_base: resolution.status, ..build })
        }
        Scope::Commits => {
            // A tag without a pick builds the empty changeset.
            let Some(pick) = &input.commit_pick else {
                return Ok(ScopeBuild {
                    review_context: ReviewContext::Commits { pick: None },
                    ..ScopeBuild::default()
                });
            };
            let mut build = build_pick(&input.repo, pick)?;
            build.review_context = ReviewContext::Commits { pick: Some(pick.clone()) };
            Ok(build)
        }
    }
}

/// The changeset between `ends`, carried beside them: the ends a file's diff reads are its input.
fn at_ends(repo: &Path, review_context: ReviewContext, ends: DiffEnds) -> Result<ScopeBuild> {
    let changed = match &ends.new {
        None => git::changed_from(repo, &ends.old)?,
        Some(new) => git::changed_between(repo, &ends.old, new)?,
    };
    let files = changed.into_iter().map(|f| (f.path.clone(), f)).collect();
    Ok(ScopeBuild {
        review_context,
        changeset: Changeset { files, ends: Some(ends) },
        ..ScopeBuild::default()
    })
}

fn context_without_git(input: &WorldInput) -> ReviewContext {
    match input.scope {
        Scope::Uncommitted => ReviewContext::Uncommitted,
        Scope::Branch => ReviewContext::Branch { base: input.base.clone() },
        Scope::LastTurn => ReviewContext::LastTurn { baseline: input.turn_baseline.clone() },
        Scope::Commits => ReviewContext::Commits { pick: input.commit_pick.clone() },
    }
}

/// The pick's changeset, verdict and ends in one pass; a `gone` pick has neither.
fn build_pick(repo: &Path, pick: &CommitPick) -> Result<ScopeBuild> {
    let gone = |sha: &str| {
        let status = PickStatus {
            verdict: PickVerdict::Gone(sha.to_string()),
            subject: String::new(),
            count: 0,
        };
        ScopeBuild { pick_status: Some(status), ..ScopeBuild::default() }
    };
    if !git::commit_exists(repo, &pick.newest) {
        return Ok(gone(&pick.newest));
    }
    let Some(old) = git::parent_or_empty(repo, &pick.oldest) else {
        return Ok(gone(&pick.oldest));
    };
    if old != git::EMPTY_TREE && !git::commit_exists(repo, &old) {
        return Ok(gone(&old));
    }
    let subject = git::commit_subject(repo, &pick.newest).unwrap_or_default();
    let count = git::run_length_from(repo, &old, &pick.oldest, &pick.newest).unwrap_or(0);
    let at = at_ends(
        repo,
        ReviewContext::Commits { pick: Some(pick.clone()) },
        DiffEnds { old, new: Some(pick.newest.clone()) },
    )?;
    // The oldest is an ancestor of the newest, so one reachability check covers the run.
    let verdict = if git::is_reachable(repo, &pick.newest) {
        PickVerdict::Live
    } else {
        PickVerdict::OffBranch
    };
    Ok(ScopeBuild { pick_status: Some(PickStatus { verdict, subject, count }), ..at })
}

/// The `All files` entries; an ignored directory is walked only once expanded.
pub(crate) fn all_files_entries(
    input: &WorldInput,
    changed: &BTreeMap<String, ChangedFile>,
) -> Result<Vec<Entry>> {
    let to_entry = |w: git::WorktreeEntry| {
        let annotation = changed.get(&w.path).cloned();
        Entry::from_worktree(w, annotation)
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
    // Sorted by path, which a path-limited rebuild merges into in one pass.
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    Ok(entries)
}

/// What a narrow re-read must also cover so git pairs renames as a full build would: both sides
/// of every rename it touches, and the deletions and additions it could pair with. `None` past the cap.
fn rename_partners(
    last: &BTreeMap<String, ChangedFile>,
    paths: &BTreeSet<String>,
    fresh: &[ChangedFile],
) -> Option<BTreeSet<String>> {
    let found =
        |kind| fresh.iter().any(|f: &ChangedFile| f.kind == kind || f.kind == ChangeKind::Renamed);
    let (added, deleted) = (found(ChangeKind::Added), found(ChangeKind::Deleted));
    let mut partners = BTreeSet::new();
    for f in last.values() {
        let touched = f.touched_by(paths);
        let pairs = match f.kind {
            ChangeKind::Deleted => added,
            ChangeKind::Added => deleted,
            ChangeKind::Renamed => added || deleted,
            _ => false,
        };
        if (touched && f.previous_path.is_some()) || (pairs && !git::covered(paths, &f.path)) {
            partners.insert(f.path.clone());
            partners.extend(f.previous_path.clone());
        }
    }
    partners.retain(|p| !paths.contains(p));
    let all: Vec<String> = paths.iter().chain(&partners).cloned().collect();
    git::fits_command_line(&all).then_some(partners)
}

/// Re-read only `paths` against `last`, a build of the same input, widened to rename partners.
/// `None` when that reaches past the cap: only a full build is right.
fn rebuild_paths(
    input: &WorldInput,
    last: &WorldSnapshot,
    paths: &BTreeSet<String>,
) -> Option<Result<WorldSnapshot>> {
    let (fresh, ends) = match read_paths(input, last, paths)? {
        Ok(read) => read,
        Err(e) => return Some(Err(e)),
    };
    let partners = rename_partners(&last.changeset.files, paths, &fresh)?;
    if partners.is_empty() {
        return Some(merge_paths(input, last, paths, fresh, ends));
    }
    let wide: BTreeSet<String> = paths.union(&partners).cloned().collect();
    let (fresh, ends) = match read_paths(input, last, &wide)? {
        Ok(read) => read,
        Err(e) => return Some(Err(e)),
    };
    Some(merge_paths(input, last, &wide, fresh, ends))
}

/// The changed files under `paths` as `last`'s scope reads them, and the ends they diff between.
/// `None` when last-turn's re-read files pass the cap.
fn read_paths(
    input: &WorldInput,
    last: &WorldSnapshot,
    paths: &BTreeSet<String>,
) -> Option<Result<(Vec<ChangedFile>, Option<DiffEnds>)>> {
    let ends = last.changeset.ends.clone();
    let batch: Vec<String> = paths.iter().cloned().collect();
    let read = match (&ends, input.scope) {
        // The pick reads committed trees: a worktree batch changes nothing in it.
        (None, _) | (_, Scope::Commits) => Ok((Vec::new(), ends)),
        (Some(DiffEnds { old, new: None }), _) => {
            git::changed_from_in(&input.repo, old, &batch).map(|files| (files, ends.clone()))
        }
        // The new end is last's tree with `paths` re-read, which a full snapshot would equal.
        (Some(DiffEnds { old, new: Some(new) }), _) => {
            match git::snapshot_worktree_in(&input.repo, new, &batch) {
                Ok(None) => return None,
                Ok(Some(tree)) => git::changed_between_in(&input.repo, old, &tree, &batch)
                    .map(|files| (files, Some(DiffEnds { old: old.clone(), new: Some(tree) }))),
                Err(e) => Err(e),
            }
        }
    };
    Some(read)
}

/// Merge a path-limited re-read into `last`: what `paths` covers comes from `fresh`.
fn merge_paths(
    input: &WorldInput,
    last: &WorldSnapshot,
    paths: &BTreeSet<String>,
    fresh: Vec<ChangedFile>,
    ends: Option<DiffEnds>,
) -> Result<WorldSnapshot> {
    let files: BTreeMap<String, ChangedFile> = if input.scope == Scope::Commits {
        last.changeset.files.clone()
    } else {
        let endpoint = ends.as_ref().and_then(|ends| ends.new.as_deref());
        let kept = last.changeset.files.values().filter(|f| !f.touched_by(paths)).cloned().map(
            |mut file| {
                if let Some(endpoint) = endpoint {
                    file.identity = file.identity.with_new_endpoint(endpoint);
                }
                file
            },
        );
        kept.chain(fresh).map(|f| (f.path.clone(), f)).collect()
    };
    let entries = match input.tab {
        Tab::AllFiles => {
            let batch: Vec<String> = paths.iter().cloned().collect();
            let annotate = |path: &str| files.get(path).cloned();
            let mut fresh: Vec<Entry> = git::all_files_in(&input.repo, &batch)?
                .into_iter()
                .map(|w| {
                    let annotation = annotate(&w.path);
                    Entry::from_worktree(w, annotation)
                })
                .collect();
            fresh.sort_by(|a, b| a.path.cmp(&b.path));
            // Both sides are sorted: one pass keeps the order, the fresh entry winning a tie.
            let kept = last.entries.iter().filter(|e| !git::covered(paths, &e.path));
            let mut entries = Vec::with_capacity(last.entries.len() + fresh.len());
            let mut fresh = fresh.into_iter().peekable();
            for e in kept {
                while fresh.peek().is_some_and(|f| f.path <= e.path) {
                    entries.extend(fresh.next());
                }
                if entries.last().is_none_or(|l: &Entry| l.path != e.path) {
                    entries.push(Entry { annotation: annotate(&e.path), ..e.clone() });
                }
            }
            entries.extend(fresh);
            entries
        }
        _ => files.values().map(Entry::from_changed).collect(),
    };
    Ok(WorldSnapshot {
        review_context: last.review_context.clone(),
        changeset: Changeset { files, ends },
        entries,
        branch_base: last.branch_base.clone(),
        pick_status: last.pick_status.clone(),
        head: last.head.clone(),
        touched: Some(paths.clone()),
    })
}

/// What a refresh re-reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// Everything.
    Full,
    /// Some worktree paths; with `index`, a full build too if what the index stages changed.
    Paths { paths: BTreeSet<String>, index: bool },
}

impl Default for Refresh {
    fn default() -> Self {
        Self::Paths { paths: BTreeSet::new(), index: false }
    }
}

impl Refresh {
    /// Some worktree paths, past the command-line cap a full build.
    pub fn paths(paths: impl IntoIterator<Item = String>) -> Self {
        Self::Paths { paths: paths.into_iter().collect(), index: false }.settled()
    }

    /// Whether it re-reads nothing.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Paths { paths, index: false } if paths.is_empty())
    }

    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    /// The paths it re-reads; `None` for a full build.
    pub fn named_paths(&self) -> Option<&BTreeSet<String>> {
        match self {
            Self::Paths { paths, .. } => Some(paths),
            Self::Full => None,
        }
    }

    /// What one watcher batch asks for: a rescan, or any git change but an index rewrite, moves
    /// what every path diffs against, so only a full build is right.
    pub fn from_batch(batch: &crate::watch::Batch) -> Self {
        use crate::watch::GitChange;
        if batch.rescan || batch.git.iter().any(|g| *g != GitChange::Index) {
            return Self::Full;
        }
        let index = batch.git.contains(&GitChange::Index);
        Self::Paths { paths: batch.worktree.clone(), index }.settled()
    }

    /// Fold a later request in: anything full stays full.
    pub fn absorb(&mut self, other: Refresh) {
        let merged = match (std::mem::take(self), other) {
            (Self::Paths { mut paths, index }, Self::Paths { paths: more, index: also }) => {
                paths.extend(more);
                Self::Paths { paths, index: index || also }
            }
            _ => Self::Full,
        };
        *self = merged.settled();
    }

    /// Paths past the command-line cap make it full.
    fn settled(self) -> Self {
        match self {
            Self::Paths { paths, .. } if !git::fits_command_line(&paths) => Self::Full,
            other => other,
        }
    }
}

/// One queued refresh's attributes, accumulated on `App` until the loop dispatches it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorldRequest {
    /// Re-reveal the cursor when the result lands — user-initiated switches only.
    pub reveal: bool,
    pub refresh: Refresh,
    /// Only the pacer asked for it: it spends the budget. The reviewer's work never does.
    pub background: bool,
}

/// One refresh request; the completion echoes its tag.
#[derive(Debug)]
pub struct WorldJob {
    pub generation: u64,
    pub input: WorldInput,
    /// Whether the result re-reveals the cursor: a user's switch does, a background one never.
    pub reveal: bool,
    pub refresh: Refresh,
}

/// A finished job; no snapshot on the `PR` tab.
#[derive(Debug)]
pub struct WorldCompletion {
    pub generation: u64,
    pub input: WorldInput,
    pub reveal: bool,
    pub snapshot: Option<Result<WorldSnapshot>>,
    /// What the job re-read, so a failure can retry exactly that.
    pub refresh: Refresh,
    /// How long the build took: the pacing budget's measure, which bounds its git's CPU.
    pub took: std::time::Duration,
}

/// Run the world worker for `repo`; queued requests coalesce into the newest, keeping a reveal.
pub fn spawn(
    repo: PathBuf,
    rx: Receiver<WorldJob>,
    tx: crate::wake::Sender<WorldCompletion>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("world".into())
        .spawn(move || {
            git::sweep_dead_copies(&repo);
            let mut worker = Worker::default();
            while let Ok(mut job) = rx.recv() {
                while let Ok(next) = rx.try_recv() {
                    let mut refresh = job.refresh;
                    refresh.absorb(next.refresh);
                    job = WorldJob { reveal: job.reveal || next.reveal, refresh, ..next };
                }
                let started = std::time::Instant::now();
                let snapshot = if job.input.tab.is_file_tab() {
                    worker.refresh(&job.input, job.refresh.clone())
                } else {
                    None
                };
                let done = WorldCompletion {
                    generation: job.generation,
                    input: job.input,
                    reveal: job.reveal,
                    snapshot,
                    refresh: job.refresh,
                    took: started.elapsed(),
                };
                if tx.send(done).is_err() {
                    break;
                }
            }
        })
        .expect("spawn world worker")
}

/// What the worker keeps between jobs: its last build, and what the index staged when it ran.
#[derive(Default)]
struct Worker {
    last: Option<(WorldInput, WorldSnapshot)>,
    staged: Option<u64>,
}

impl Worker {
    /// Run one refresh: only its paths when the last build is of the same input, else in full.
    /// `None` when nothing needs re-reading.
    fn refresh(
        &mut self,
        input: &WorldInput,
        mut refresh: Refresh,
    ) -> Option<Result<WorldSnapshot>> {
        // A pick reads committed trees: on `Changes`, a worktree or index change moves nothing in it.
        let pick_only = input.scope == Scope::Commits && input.tab == Tab::Changes;
        if refresh.is_empty() || (pick_only && !refresh.is_full()) {
            return None;
        }
        // Read before any build, so a stage landing during it reads as new next time.
        let mut fingerprint = None;
        if let Refresh::Paths { index: true, .. } = refresh {
            let now = git::staged_fingerprint(&input.repo).ok();
            if now.is_none() || now != self.staged {
                refresh = Refresh::Full;
                fingerprint = Some(now);
            }
        }
        // An expanded ignored directory lists from disk, so a batch inside one rebuilds it whole.
        let in_expanded = |p: &String| {
            input.toggled_dirs.iter().any(|d| git::under_or_eq(p, d) || git::under_or_eq(d, p))
        };
        let last = self.last.as_ref().filter(|(of, _)| of == input).map(|(_, built)| built);
        if let Some(last) = last
            && let Refresh::Paths { paths, .. } = &refresh
            && !paths.iter().any(in_expanded)
        {
            if paths.is_empty() {
                return None;
            }
            match rebuild_paths(input, last, paths) {
                Some(Ok(built)) => {
                    self.last = Some((input.clone(), built.clone()));
                    return Some(Ok(built));
                }
                // A failed narrow read is retried at once as a full build.
                Some(Err(e)) => logln!("path-limited refresh failed, rebuilding in full: {e:#}"),
                None => {}
            }
        }
        let fingerprint = fingerprint.unwrap_or_else(|| git::staged_fingerprint(&input.repo).ok());
        let built = build(input);
        match &built {
            Ok(built) => {
                self.staged = fingerprint;
                self.last = Some((input.clone(), built.clone()));
            }
            // A failed build saw nothing: the next one reads everything again.
            Err(_) => self.last = None,
        }
        Some(built)
    }
}

#[cfg(test)]
mod tests {
    use super::Refresh;
    use crate::watch::{Batch, GitChange};

    fn batch(git: &[GitChange], worktree: &[&str], rescan: bool) -> Batch {
        Batch {
            worktree: worktree.iter().map(|p| (*p).to_string()).collect(),
            git: git.iter().copied().collect(),
            rescan,
            ..Batch::default()
        }
    }

    #[test]
    fn a_batch_asks_for_exactly_what_it_moved() {
        let paths = |list: &[&str]| list.iter().map(|p| (*p).to_string()).collect();
        let rows = [
            ("a rescan", batch(&[], &["a"], true), Refresh::Full),
            ("HEAD", batch(&[GitChange::Head], &["a"], false), Refresh::Full),
            ("a ref", batch(&[GitChange::Refs], &[], false), Refresh::Full),
            ("git config", batch(&[GitChange::Config], &[], false), Refresh::Full),
            ("an ignore rule", batch(&[GitChange::IgnoreRules], &[], false), Refresh::Full),
            ("an attribute rule", batch(&[GitChange::Attributes], &[], false), Refresh::Full),
            ("another pane's pick", batch(&[GitChange::ReviewrRefs], &[], false), Refresh::Full),
            (
                "an index rewrite with paths",
                batch(&[GitChange::Index], &["a"], false),
                Refresh::Paths { paths: paths(&["a"]), index: true },
            ),
            (
                "worktree paths alone",
                batch(&[], &["a", "b/c"], false),
                Refresh::Paths { paths: paths(&["a", "b/c"]), index: false },
            ),
            ("the config alone", Batch { config: true, ..Batch::default() }, Refresh::default()),
        ];
        for (name, batch, expected) in rows {
            assert_eq!(Refresh::from_batch(&batch), expected, "{name}");
        }
    }
}
