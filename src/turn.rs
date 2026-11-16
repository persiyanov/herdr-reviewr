//! Turn tracking: a turn is the worktree's rest→work edge, its baseline promoted once a file changes.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::git;
use crate::herdr::{AgentSample, OddEntry};

/// An agent's status as herdr reports it (`agent_status`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    Idle,
    Working,
    Blocked,
    Done,
    #[default]
    Unknown,
}

impl Status {
    /// A status between turns; `blocked` and `unknown` are mid-turn.
    fn is_resting(self) -> bool {
        matches!(self, Status::Idle | Status::Done)
    }

    /// The status an `agent_status` spelling means; an unknown one is mid-turn, never an edge.
    pub fn from_wire(wire: &str) -> Self {
        match wire {
            "idle" => Status::Idle,
            "working" => Status::Working,
            "blocked" => Status::Blocked,
            "done" => Status::Done,
            _ => Status::Unknown,
        }
    }
}

/// The worktree's work state, folded from every agent in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorktreeState {
    /// Every agent rests, or there is none.
    #[default]
    Resting,
    /// At least one agent works.
    Working,
    /// An agent is `blocked` or `unknown` and none works: an open turn stays open.
    Neither,
}

impl WorktreeState {
    /// Fold the members' statuses; any `working` wins.
    pub fn fold(statuses: impl IntoIterator<Item = Status>) -> Self {
        let mut held = false;
        for status in statuses {
            if status == Status::Working {
                return Self::Working;
            }
            held |= !status.is_resting();
        }
        if held { Self::Neither } else { Self::Resting }
    }
}

/// The lifecycle edges produced by one sample of the worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnTransition {
    pub started: bool,
    pub ended: bool,
}

/// The worktree's turn edges, read from its folded states.
#[derive(Default, Debug)]
pub struct TurnTracker {
    /// Whether the previous sample rested; `false` first, so the first sample starts nothing.
    prev_resting: bool,
    /// Whether the worktree worked since its last rest: an end can come through `Neither`,
    /// where a start never does, so the two edges keep separate memories.
    worked: bool,
}

impl TurnTracker {
    /// Record one sample: a start is `Resting → Working`, an end any return to rest after work.
    pub fn observe(&mut self, state: WorktreeState) -> TurnTransition {
        let transition = TurnTransition {
            started: state == WorktreeState::Working && self.prev_resting,
            ended: self.worked && state == WorktreeState::Resting,
        };
        self.prev_resting = state == WorktreeState::Resting;
        self.worked = match state {
            WorktreeState::Working => true,
            WorktreeState::Resting => false,
            WorktreeState::Neither => self.worked,
        };
        transition
    }

    /// Nobody saw the agents for a while: the next status cannot start a turn.
    pub fn gap(&mut self) {
        self.prev_resting = false;
    }
}

/// What `last-turn` diffs against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LastTurn {
    /// No turn was seen yet.
    #[default]
    Waiting,
    /// The last turn's first write raced its start snapshot: it shows nothing, and says why.
    Raced,
    /// The tree the last turn started from.
    At(String),
}

impl LastTurn {
    /// The tree to diff against, when there is one.
    pub fn tree(&self) -> Option<&str> {
        match self {
            Self::At(tree) => Some(tree),
            Self::Waiting | Self::Raced => None,
        }
    }
}

/// A window of real worktree changes: when their events arrived, and where (`None`: anywhere).
#[derive(Clone, Debug)]
pub struct Wrote {
    pub first: Instant,
    pub last: Instant,
    pub paths: Option<BTreeSet<String>>,
}

impl Wrote {
    /// Fold a later window in: one check covers both.
    pub fn absorb(&mut self, other: Self) {
        self.first = self.first.min(other.first);
        self.last = self.last.max(other.last);
        match (&mut self.paths, other.paths) {
            (Some(mine), Some(theirs)) => mine.extend(theirs),
            (paths, _) => *paths = None,
        }
    }
}

/// What the watcher tells turn tracking.
#[derive(Clone, Debug)]
pub enum TurnNews {
    Wrote(Wrote),
    /// The watcher went down, so no write is reported, or came back.
    WatcherDown(bool),
}

/// Where the watcher sends [`TurnNews`], straight to turn tracking's thread, so a frame loop held
/// by a terminal editor never delays it.
#[derive(Clone)]
pub struct TurnFeed(std::sync::Arc<dyn Fn(TurnNews) + Send + Sync>);

impl std::fmt::Debug for TurnFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TurnFeed")
    }
}

impl TurnFeed {
    pub fn new(send: impl Fn(TurnNews) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(send))
    }

    pub fn send(&self, news: TurnNews) {
        (self.0)(news);
    }
}

/// How far before the status edge a write still races the start snapshot: herdr reports
/// `working` a little after the agent starts.
const RACE_BEFORE: Duration = Duration::from_secs(1);
/// How long after an instant a write's event may still describe a write made before it: the
/// watcher's gathering plus the platform's delivery.
const RACE_AFTER: Duration = crate::watch::GATHER.saturating_add(Duration::from_millis(150));

/// A started turn awaiting its first real change: its start snapshot, from when a write races
/// it, and when it was snapshotted and ended.
#[derive(Clone, Debug)]
struct TurnWindow {
    candidate: String,
    race_from: Instant,
    snapped: Instant,
    ended: Option<Instant>,
    /// The watcher was down at some point of the turn, so its writes may never have arrived.
    unobserved: bool,
}

/// Turn tracking for one worktree: which agents work in it, its baseline, and the ref that
/// persists it. The herdr connection's thread owns it and feeds it every status change.
#[derive(Debug)]
pub struct TurnHost {
    tracker: TurnTracker,
    last: LastTurn,
    repo: PathBuf,
    /// The reviewed worktree's [`git::canonical`] root, which a member's top level equals.
    root: PathBuf,
    /// Each agent `cwd` with a resolved top level, mapped to whether it is a member.
    resolved: HashMap<String, bool>,
    turn: Option<TurnWindow>,
    /// When the last write arrived, so a turn whose status edge follows its first write sees it.
    last_write: Option<Instant>,
    /// No write evidence comes: a turn is judged at its end by the worktree itself.
    watcher_down: bool,
}

/// One observation's outcome: whether a turn ended, whether agents are present, `last-turn`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnReport {
    pub ended: bool,
    /// `None` when a member didn't resolve, so the reader keeps what it knew.
    pub agents_present: Option<bool>,
    pub last: LastTurn,
}

/// An agent's place in the worktree; `Unknown` holds the observation instead of counting it out.
enum Membership {
    Member,
    NotMember,
    Unknown,
}

/// The agent's cwd when absolute: `git -C` would resolve a relative one against reviewr's own.
fn worktree_cwd(cwd: Option<&str>) -> Option<&str> {
    cwd.filter(|c| Path::new(c).is_absolute())
}

impl TurnHost {
    /// Resume the persisted baseline of `repo`, which must be the git top level.
    pub fn open(repo: PathBuf) -> Self {
        let last = git::read_baseline_ref(&repo).map_or(LastTurn::Waiting, LastTurn::At);
        Self {
            tracker: TurnTracker::default(),
            last,
            root: git::canonical(&repo),
            repo,
            resolved: HashMap::new(),
            turn: None,
            last_write: None,
            watcher_down: false,
        }
    }

    pub fn baseline(&self) -> Option<&str> {
        self.last.tree()
    }

    /// This worktree's agents, else `workspace`'s; `None` while an unread one may be a member.
    pub fn members<'a>(
        &mut self,
        agents: &[(&'a str, &AgentSample)],
        // Entries herdr shaped unexpectedly, which may be agents.
        odd: &[OddEntry],
        workspace: Option<&str>,
    ) -> Option<Vec<&'a str>> {
        let mut members = Vec::new();
        let cwds = agents.iter().map(|(pane, a)| (Some(*pane), a.cwd.as_deref()));
        for (pane, cwd) in cwds.chain(odd.iter().map(|o| (None, o.cwd.as_deref()))) {
            match (self.membership(cwd), pane) {
                (Membership::Member, Some(pane)) => members.push(pane),
                (Membership::NotMember, _) => {}
                (Membership::Member | Membership::Unknown, _) => return None,
            }
        }
        let Some(workspace) = workspace.filter(|_| members.is_empty()) else {
            return Some(members);
        };
        // An odd entry whose workspace is this one or unread may be a member.
        if odd.iter().any(|o| o.workspace.as_deref().is_none_or(|w| w == workspace)) {
            return None;
        }
        Some(
            agents
                .iter()
                .filter(|(_, a)| a.workspace == workspace)
                .map(|(pane, _)| *pane)
                .collect(),
        )
    }

    /// Advance from the members' statuses as herdr reported them `at`; `None`, an unknown set,
    /// holds the last state.
    pub fn observe_statuses(&mut self, statuses: Option<&[Status]>, at: Instant) -> TurnReport {
        let Some(statuses) = statuses else {
            return self.report(false, None);
        };
        let ended = self.observe(WorktreeState::fold(statuses.iter().copied()), at);
        self.report(ended, Some(!statuses.is_empty()))
    }

    /// The connection to herdr dropped: nobody saw the agents meanwhile.
    pub fn gap(&mut self) {
        self.tracker.gap();
    }

    /// The state as it stands, with no turn ended.
    pub fn status(&self) -> TurnReport {
        self.report(false, None)
    }

    fn report(&self, ended: bool, agents_present: Option<bool>) -> TurnReport {
        TurnReport { ended, agents_present, last: self.last.clone() }
    }

    /// Whether the watcher is down; a turn it was down for is judged at its end.
    pub fn watcher_down(&mut self, down: bool) {
        self.watcher_down = down;
        if let Some(turn) = self.turn.as_mut() {
            turn.unobserved |= down;
        }
    }

    /// The worktree changed: whether `last-turn` moved.
    pub fn on_worktree_change(&mut self, wrote: &Wrote) -> bool {
        self.last_write = Some(wrote.last);
        let Some(turn) = self.turn.clone() else { return false };
        // It may already sit in the start snapshot.
        if wrote.first <= turn.snapped + RACE_AFTER && wrote.last >= turn.race_from {
            self.abandon();
            return true;
        }
        // After the turn and its late writes, a change is someone else's.
        if turn.ended.is_some_and(|ended| wrote.first > ended + RACE_AFTER) {
            self.turn = None;
            return false;
        }
        if wrote.first <= turn.snapped + RACE_AFTER {
            return false;
        }
        // Only a net change promotes: a temp file made and removed, a `touch`, a same-content
        // rewrite leaves the turn pending.
        let narrow = wrote.paths.as_ref().filter(|p| git::fits_command_line(*p));
        let moved = match narrow {
            Some(paths) => {
                let paths: Vec<String> = paths.iter().cloned().collect();
                git::snapshot_worktree_in(&self.repo, &turn.candidate, &paths)
            }
            None => Ok(None),
        };
        let moved = match moved {
            Ok(Some(tree)) => Ok(tree != turn.candidate),
            // Past the cap, or anywhere: the whole worktree answers.
            Ok(None) => git::snapshot_worktree(&self.repo).map(|now| now != turn.candidate),
            Err(e) => Err(e),
        };
        match moved {
            Ok(true) => {
                self.promote(turn.candidate);
                true
            }
            Ok(false) => false,
            Err(e) => {
                // Unanswered: the turn's end judges it instead.
                logln!("turn change check failed: {e}");
                if let Some(turn) = self.turn.as_mut() {
                    turn.unobserved = true;
                }
                false
            }
        }
    }

    fn abandon(&mut self) {
        self.turn = None;
        self.last = LastTurn::Raced;
        if let Err(e) = git::delete_baseline_ref(&self.repo) {
            logln!("turn baseline ref delete failed: {e}");
        }
    }

    fn promote(&mut self, candidate: String) {
        self.turn = None;
        if let Err(e) = git::write_baseline_ref(&self.repo, &candidate) {
            logln!("turn baseline ref write failed: {e}");
        }
        self.last = LastTurn::At(candidate);
    }

    /// The turn rested. If the watcher missed any part of it, the worktree says whether it wrote.
    fn end_turn(&mut self) {
        let Some(turn) = self.turn.as_mut() else { return };
        turn.ended = Some(Instant::now());
        if !turn.unobserved {
            return;
        }
        let candidate = turn.candidate.clone();
        match git::snapshot_worktree(&self.repo) {
            Ok(now) if now != candidate => self.promote(candidate),
            Ok(_) => self.turn = None,
            Err(e) => logln!("turn end snapshot failed: {e}"),
        }
    }

    /// An agent's place by git top level: a subdirectory is a member, a sibling worktree is not.
    fn membership(&mut self, cwd: Option<&str>) -> Membership {
        let Some(cwd) = worktree_cwd(cwd) else {
            return Membership::NotMember;
        };
        if let Some(&member) = self.resolved.get(cwd) {
            return if member { Membership::Member } else { Membership::NotMember };
        }
        // A member's top level is this root, so a cwd outside it is none, with no git to ask.
        if !git::canonical(Path::new(cwd)).starts_with(&self.root) {
            return Membership::NotMember;
        }
        match git::worktree_of(Path::new(cwd)) {
            // A resolved root never moves, so it is cached.
            git::Worktree::Root(top) => {
                let member = git::canonical(&top) == self.root;
                self.resolved.insert(cwd.to_string(), member);
                if member { Membership::Member } else { Membership::NotMember }
            }
            // Not cached: a directory can become a worktree later.
            git::Worktree::Outside => Membership::NotMember,
            // git could not run: hold, as an unknown set does.
            git::Worktree::Unknown => Membership::Unknown,
        }
    }

    /// Advance from one worktree state herdr reported `started`: a start snapshots at once, an
    /// end closes the turn. Whether a turn ended.
    fn observe(&mut self, state: WorktreeState, started: Instant) -> bool {
        let transition = self.tracker.observe(state);
        if transition.started {
            self.turn = None;
            match git::snapshot_worktree(&self.repo) {
                Ok(candidate) => {
                    let unobserved = self.watcher_down;
                    let snapped = Instant::now();
                    let race_from = started.checked_sub(RACE_BEFORE).unwrap_or(started);
                    let ended = None;
                    self.turn =
                        Some(TurnWindow { candidate, race_from, snapped, ended, unobserved });
                    // A write from just before the edge up to the snapshot already sits in it.
                    if self.last_write.is_some_and(|w| w >= race_from && w <= snapped) {
                        self.abandon();
                    }
                }
                Err(e) => logln!("turn snapshot failed: {e}"),
            }
        }
        if transition.ended {
            self.end_turn();
        }
        transition.ended
    }
}

#[cfg(test)]
mod tests {
    use super::{OddEntry, Status, TurnTracker, WorktreeState, worktree_cwd};
    use std::time::Instant;

    #[test]
    fn a_turns_race_window_opens_when_herdr_reported_the_start_not_when_it_was_read() {
        let (dir, git) = crate::test_support::test_repo();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        let crate::git::Worktree::Root(root) = crate::git::worktree_of(dir.path()) else {
            panic!("a fresh repository resolves to a worktree root");
        };
        let mut host = super::TurnHost::open(root);
        let reported = Instant::now().checked_sub(std::time::Duration::from_secs(2)).unwrap();
        host.observe_statuses(Some(&[super::Status::Idle]), reported);
        host.observe_statuses(Some(&[super::Status::Idle]), reported);
        // The agent wrote just after reporting working; the thread read the status 2s later.
        let at = reported + std::time::Duration::from_millis(100);
        host.on_worktree_change(&super::Wrote { first: at, last: at, paths: None });
        host.observe_statuses(Some(&[super::Status::Working]), reported);
        assert_eq!(host.status().last, super::LastTurn::Raced, "the write may sit in the snapshot");
    }

    #[test]
    fn an_agent_outside_the_worktree_is_no_member_and_costs_no_git() {
        let (dir, _) = crate::test_support::test_repo();
        let crate::git::Worktree::Root(root) = crate::git::worktree_of(dir.path()) else {
            panic!("a fresh repository resolves to a worktree root");
        };
        let elsewhere = tempfile::tempdir().unwrap();
        let mut host = super::TurnHost::open(root);
        let before = crate::git::GIT_COMMANDS.with(std::cell::Cell::get);
        let agent = seat(elsewhere.path().to_str(), "w");
        assert_eq!(host.members(&[("w:1", &agent)], &[], None), Some(vec![]));
        assert_eq!(crate::git::GIT_COMMANDS.with(std::cell::Cell::get), before, "no git asked");
    }

    /// An idle agent at `cwd` in workspace `ws`.
    fn seat(cwd: Option<&str>, ws: &str) -> crate::herdr::AgentSample {
        crate::herdr::AgentSample {
            cwd: cwd.map(str::to_string),
            workspace: ws.to_string(),
            status: super::Status::Idle,
        }
    }

    #[test]
    fn only_an_absolute_cwd_can_name_a_worktree() {
        // A blank or relative cwd is rejected before any git call.
        let abs = if cfg!(windows) { r"C:\abs\path" } else { "/abs/path" };
        assert_eq!(worktree_cwd(Some(abs)), Some(abs));
        assert_eq!(worktree_cwd(Some("relative/path")), None);
        assert_eq!(worktree_cwd(Some("")), None);
        assert_eq!(worktree_cwd(None), None);
    }

    /// An agent whose cwd spells the root in another case is a member.
    #[cfg(windows)]
    #[test]
    fn an_agent_at_the_root_in_another_case_is_a_member() {
        let (dir, _) = crate::test_support::test_repo();
        let crate::git::Worktree::Root(root) = crate::git::worktree_of(dir.path()) else {
            panic!("a fresh repository resolves to a worktree root");
        };
        let mut host = super::TurnHost::open(root.clone());
        let recased = root.to_string_lossy().to_ascii_uppercase();
        assert_ne!(recased, root.to_string_lossy(), "the spelling really differs");
        assert!(matches!(host.membership(Some(&recased)), super::Membership::Member));
    }

    #[test]
    fn members_are_the_panes_in_this_worktree_and_an_outsider_is_not() {
        let (dir, _) = crate::test_support::test_repo();
        let crate::git::Worktree::Root(root) = crate::git::worktree_of(dir.path()) else {
            panic!("a fresh repository resolves to a worktree root");
        };
        let outside = tempfile::tempdir().unwrap();
        let mut host = super::TurnHost::open(root.clone());
        let (mine, theirs, nowhere) =
            (seat(root.to_str(), "w"), seat(outside.path().to_str(), "w"), seat(None, "w"));
        let agents = [("w:1", &mine), ("w:2", &theirs), ("w:3", &nowhere)];
        assert_eq!(host.members(&agents, &[], Some("w")), Some(vec!["w:1"]));
    }

    #[test]
    fn with_no_agent_here_the_workspaces_agents_are_members() {
        let (dir, git) = crate::test_support::test_repo();
        let sibling = tempfile::tempdir().unwrap();
        let linked = sibling.path().join("linked");
        git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        git(&["worktree", "add", "-q", "-b", "linked", &linked.to_string_lossy()]);
        let mut host = super::TurnHost::open(linked.canonicalize().unwrap());
        let main = dir.path().to_str();
        // Where the workspace's agent works makes no difference: herdr reports where it launched.
        let (here, there, nowhere) = (seat(main, "w"), seat(main, "elsewhere"), seat(None, "w"));
        let agents = [("w:1", &here), ("w:2", &there), ("w:3", &nowhere)];
        let before = crate::git::GIT_COMMANDS.with(std::cell::Cell::get);
        assert_eq!(host.members(&agents, &[], Some("w")), Some(vec!["w:1", "w:3"]));
        assert_eq!(crate::git::GIT_COMMANDS.with(std::cell::Cell::get), before, "no git to ask");
        assert_eq!(host.members(&agents, &[], None), Some(vec![]), "no workspace, no fallback");

        // An entry herdr shaped unexpectedly may be this workspace's agent: hold.
        let odd = |workspace: Option<&str>| OddEntry {
            cwd: main.map(str::to_string),
            workspace: workspace.map(str::to_string),
        };
        assert_eq!(host.members(&agents, &[odd(None)], Some("w")), None);
        assert_eq!(host.members(&agents, &[odd(Some("w"))], Some("w")), None);
        let elsewhere = host.members(&agents, &[odd(Some("elsewhere"))], Some("w"));
        assert_eq!(elsewhere, Some(vec!["w:1", "w:3"]), "another workspace's entry is never one");

        // An agent here outranks the fallback.
        let here = seat(linked.to_str(), "elsewhere");
        assert_eq!(host.members(&[agents[0], ("w:6", &here)], &[], Some("w")), Some(vec!["w:6"]));
    }

    #[test]
    fn from_wire_reads_herdrs_four_spellings_and_folds_the_rest_to_unknown() {
        // herdr's spellings, pinned literally.
        assert_eq!(Status::from_wire("idle"), Status::Idle);
        assert_eq!(Status::from_wire("working"), Status::Working);
        assert_eq!(Status::from_wire("blocked"), Status::Blocked);
        assert_eq!(Status::from_wire("done"), Status::Done);
        assert_eq!(Status::from_wire("unknown"), Status::Unknown);
        // A new herdr state is unknown, never resting, so it starts no turn.
        assert_eq!(Status::from_wire("compacting"), Status::Unknown);
        assert!(!Status::from_wire("compacting").is_resting());
    }

    #[test]
    fn an_empty_worktree_rests_so_its_first_working_agent_starts_a_turn() {
        // No agents rests, so a fresh pane tracks the next turn it sees.
        assert_eq!(WorktreeState::fold([]), WorktreeState::Resting);
        let mut t = TurnTracker::default();
        t.observe(WorktreeState::fold([]));
        assert!(t.observe(WorktreeState::fold([Status::Working])).started);
    }

    #[test]
    fn one_working_agent_makes_the_whole_worktree_work() {
        // `working` wins over every peer.
        assert_eq!(WorktreeState::fold([Status::Idle, Status::Working]), WorktreeState::Working);
        assert_eq!(WorktreeState::fold([Status::Blocked, Status::Working]), WorktreeState::Working);
        assert_eq!(WorktreeState::fold([Status::Idle, Status::Done]), WorktreeState::Resting);
    }

    #[test]
    fn a_held_agent_with_no_worker_leaves_the_worktree_neither() {
        // `blocked` and `unknown` never rest.
        assert_eq!(WorktreeState::fold([Status::Blocked, Status::Idle]), WorktreeState::Neither);
        assert_eq!(WorktreeState::fold([Status::Unknown]), WorktreeState::Neither);
    }

    #[test]
    fn a_turn_starts_when_the_worktree_works_after_resting() {
        let mut t = TurnTracker::default();
        assert!(!t.observe(WorktreeState::Resting).started, "the first sample never starts a turn");
        assert!(t.observe(WorktreeState::Working).started, "resting → working starts a turn");
    }

    #[test]
    fn a_held_worktree_returning_to_work_is_a_continuation() {
        let mut t = TurnTracker::default();
        t.observe(WorktreeState::Resting);
        t.observe(WorktreeState::Working); // turn started
        t.observe(WorktreeState::Neither); // permission prompt mid-turn
        assert!(
            !t.observe(WorktreeState::Working).started,
            "neither → working resumes the same turn"
        );
    }

    #[test]
    fn a_turn_ends_only_on_a_working_to_resting_edge() {
        let mut t = TurnTracker::default();
        assert!(!t.observe(WorktreeState::Resting).ended, "no prior work, so no turn to end");
        assert!(!t.observe(WorktreeState::Working).ended, "resting → working starts, never ends");
        assert!(
            !t.observe(WorktreeState::Neither).ended,
            "working → neither is a mid-turn pause, not an end"
        );
        t.observe(WorktreeState::Working);
        assert!(t.observe(WorktreeState::Resting).ended, "working → resting ends the turn");
    }

    #[test]
    fn a_turn_held_by_a_prompt_still_ends_when_the_worktree_rests() {
        // Working → prompt → idle: the end comes through `Neither`.
        let mut t = TurnTracker::default();
        t.observe(WorktreeState::Resting);
        t.observe(WorktreeState::Working);
        assert!(!t.observe(WorktreeState::Neither).ended, "the prompt holds the turn open");
        assert!(t.observe(WorktreeState::Resting).ended, "resting after it ends the turn");
        assert!(
            !t.observe(WorktreeState::Resting).ended,
            "a turn ends once, not on every resting sample after it"
        );
    }

    #[test]
    fn a_lone_first_working_sample_never_starts_a_turn() {
        let mut t = TurnTracker::default();
        assert!(!t.observe(WorktreeState::Working).started, "we did not observe this turn's start");
    }
}
