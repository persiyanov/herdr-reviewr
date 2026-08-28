//! Turn tracking for the `last-turn` scope.
//!
//! A turn belongs to the worktree, never to one agent
//! (HH-TURN-PER-WORKTREE): [`WorktreeState`] folds every agent in the worktree into one
//! work state, and a turn is that fold's rest→work edge. On a turn start the host captures
//! a candidate worktree snapshot; it promotes the candidate to the live baseline once the
//! turn has changed a file, so a question-only turn keeps the previous turn's diff.

/// The agent status reported by `herdr agent list` (`agent_status`).
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
    /// A resting status the agent waits at between turns — a new `working` after one of
    /// these is a fresh instruction. `blocked` (a permission prompt) and `unknown` (a
    /// transient overlay) are mid-turn, so they are not resting.
    fn is_resting(self) -> bool {
        matches!(self, Status::Idle | Status::Done)
    }

    /// The status one `agent_status` string means — the only place a wire spelling becomes a
    /// status. A spelling reviewr does not know is `Unknown`, which is mid-turn rather than
    /// resting, so a state herdr adds can never fabricate a turn edge. The row shows herdr's
    /// own spelling rather than one of these names, so nothing maps back (`src/herdr.rs`).
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

/// The worktree's work state, folded from the statuses of every agent in it. Tracking
/// watches this fold's edges rather than one agent's (see the module header).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorktreeState {
    /// Every agent in the worktree rests. A worktree holding no agents rests too, so the
    /// first agent to arrive and work starts a turn.
    #[default]
    Resting,
    /// At least one agent works.
    Working,
    /// An agent is `blocked` or `unknown` and none works. A turn starts only from rest, so
    /// this holds an open turn open instead of ending it or starting another.
    Neither,
}

impl WorktreeState {
    /// Fold the member agents' statuses. `working` wins over everything, since one agent
    /// still editing means the worktree is still being worked on.
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

/// The turn baseline lifecycle: the previous status, a candidate snapshot awaiting
/// promotion, and the live baseline tree the `last-turn` diff reads.
#[derive(Default, Debug)]
pub struct TurnTracker {
    /// Whether the previous sample rested. A turn starts only on `Resting → Working`, and the
    /// first sample never starts one, so this begins `false`.
    prev_resting: bool,
    /// Whether a `Working` sample has landed since the last rest. A turn reaches rest through
    /// `Neither` whenever a permission prompt is answered by going idle, so the end edge
    /// cannot be read from the previous sample alone.
    ///
    /// Deliberately not the same reading of the past as `prev_resting`, which `Neither` clears:
    /// the start edge stays conservative and the end edge liberal, because a missed start
    /// only widens the diff while a missed end strands the `PR` tab's refetch
    /// Collapsing the two — leaving `prev_resting` set
    /// on `Neither` and dropping this field — makes `Resting → Neither → Working` start a turn
    /// and anchor its baseline after edits the agent already made, which shows less than the
    /// turn wrote. Failure semantics forbids that.
    worked: bool,
    candidate: Option<String>,
    baseline: Option<String>,
}

impl TurnTracker {
    /// Seed from the persisted baseline ref at startup (`None` until a turn is observed).
    pub fn with_baseline(baseline: Option<String>) -> Self {
        Self { baseline, ..Self::default() }
    }

    /// The live baseline tree the `last-turn` diff reads against.
    pub fn baseline(&self) -> Option<&str> {
        self.baseline.as_deref()
    }

    pub fn has_baseline(&self) -> bool {
        self.baseline.is_some()
    }

    /// The candidate snapshot awaiting promotion, if a turn is in flight.
    pub fn candidate(&self) -> Option<&str> {
        self.candidate.as_deref()
    }

    /// Record one sample of the worktree and return its complete lifecycle transition. A
    /// start is a `Resting` to `Working` edge; the first sample never starts a turn, since
    /// its start was not observed. An end is the return to rest of a worktree that has
    /// worked, however many `Neither` samples sit between the two.
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

    /// Store the worktree snapshot captured at a turn start as the pending candidate,
    /// replacing any earlier unpromoted candidate (a question-only turn's).
    pub fn set_candidate(&mut self, sha: String) {
        self.candidate = Some(sha);
    }

    /// Promote the pending candidate to the live baseline once the turn has changed a
    /// file. Returns the new baseline for the host to persist, or `None` if no candidate
    /// was pending.
    pub fn promote(&mut self) -> Option<&str> {
        if self.candidate.is_some() {
            self.baseline = self.candidate.take();
        }
        self.baseline.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::{Status, TurnTracker, WorktreeState};

    #[test]
    fn from_wire_reads_herdrs_four_spellings_and_folds_the_rest_to_unknown() {
        // The spellings are herdr's, so they are pinned literally rather than derived from
        // anything reviewr owns.
        assert_eq!(Status::from_wire("idle"), Status::Idle);
        assert_eq!(Status::from_wire("working"), Status::Working);
        assert_eq!(Status::from_wire("blocked"), Status::Blocked);
        assert_eq!(Status::from_wire("done"), Status::Done);
        assert_eq!(Status::from_wire("unknown"), Status::Unknown);
        // A state herdr adds is unknown to tracking, and unknown is never resting, so the next
        // `working` sample resumes the turn in flight instead of starting a new one.
        assert_eq!(Status::from_wire("compacting"), Status::Unknown);
        assert!(!Status::from_wire("compacting").is_resting());
    }

    #[test]
    fn an_empty_worktree_rests_so_its_first_working_agent_starts_a_turn() {
        // The fold that makes a freshly opened reviewr pane track the next turn it sees, rather
        // than waiting for an agent that was already there.
        assert_eq!(WorktreeState::fold([]), WorktreeState::Resting);
        let mut t = TurnTracker::default();
        t.observe(WorktreeState::fold([]));
        assert!(t.observe(WorktreeState::fold([Status::Working])).started);
    }

    #[test]
    fn one_working_agent_makes_the_whole_worktree_work() {
        // Any agent still editing means the worktree is still being worked on, so `working`
        // wins over every resting or held peer (HH-TURN-PER-WORKTREE).
        assert_eq!(WorktreeState::fold([Status::Idle, Status::Working]), WorktreeState::Working);
        assert_eq!(WorktreeState::fold([Status::Blocked, Status::Working]), WorktreeState::Working);
        assert_eq!(WorktreeState::fold([Status::Idle, Status::Done]), WorktreeState::Resting);
    }

    #[test]
    fn a_held_agent_with_no_worker_leaves_the_worktree_neither() {
        // `blocked` is a permission prompt and `unknown` a transient overlay. Neither rests,
        // so neither lets the next `working` sample start a fresh turn.
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
        // An agent works, hits a permission prompt, and the answer sends it idle. The path is
        // working → neither → resting, so no `working` sample is ever adjacent to the end.
        // Missing this edge strands the `PR` tab's per-turn refetch (`src/lib.rs`).
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

    #[test]
    fn promote_moves_the_candidate_to_the_baseline() {
        let mut t = TurnTracker::with_baseline(None);
        assert!(!t.has_baseline());
        t.set_candidate("tree-a".into());
        assert_eq!(t.candidate(), Some("tree-a"));
        assert_eq!(t.promote(), Some("tree-a"));
        assert_eq!(t.baseline(), Some("tree-a"));
        assert_eq!(t.candidate(), None, "promotion consumes the candidate");
    }

    #[test]
    fn promote_without_a_candidate_keeps_the_baseline() {
        let mut t = TurnTracker::with_baseline(Some("tree-a".into()));
        assert_eq!(t.promote(), Some("tree-a"), "no candidate leaves the baseline intact");
        assert_eq!(t.baseline(), Some("tree-a"));
    }

    #[test]
    fn a_question_only_turn_keeps_the_previous_baseline() {
        // Turn A edits a file: candidate captured then promoted.
        let mut t = TurnTracker::default();
        t.observe(WorktreeState::Resting);
        t.observe(WorktreeState::Working);
        t.set_candidate("turn-a".into());
        t.promote();
        // Turn B is question-only: candidate captured at its start, never promoted.
        t.observe(WorktreeState::Resting);
        t.observe(WorktreeState::Working);
        t.set_candidate("turn-b".into());
        assert_eq!(t.baseline(), Some("turn-a"), "the unpromoted turn keeps A's baseline");
    }
}
