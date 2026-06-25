//! Turn tracking for the `last-turn` scope.
//!
//! See `specs/herdr-host.md`. A turn starts when the agent enters `working` from a
//! resting status (`idle`/`done`); a `blocked`→`working` or `unknown`→`working` step
//! is a mid-turn resume, not a new turn. On a turn start the host captures a candidate
//! worktree snapshot; it promotes the candidate to the live baseline once the turn has
//! changed a file, so a question-only turn keeps the previous turn's diff.

/// The agent status reported by `herdr agent list` (`agent_status`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl Status {
    /// Parse the `agent_status` string; anything unrecognized is `Unknown`.
    pub fn parse(s: &str) -> Self {
        match s {
            "idle" => Status::Idle,
            "working" => Status::Working,
            "blocked" => Status::Blocked,
            "done" => Status::Done,
            _ => Status::Unknown,
        }
    }

    /// A resting status the agent waits at between turns — a new `working` after one of
    /// these is a fresh instruction. `blocked` (a permission prompt) and `unknown` (a
    /// transient overlay) are mid-turn, so they are not resting.
    fn is_resting(self) -> bool {
        matches!(self, Status::Idle | Status::Done)
    }
}

/// The turn baseline lifecycle: the previous status, a candidate snapshot awaiting
/// promotion, and the live baseline tree the `last-turn` diff reads.
#[derive(Default, Debug)]
pub struct TurnTracker {
    prev: Option<Status>,
    candidate: Option<String>,
    baseline: Option<String>,
}

impl TurnTracker {
    /// Seed from the persisted baseline ref at startup (`None` until a turn is observed).
    pub fn with_baseline(baseline: Option<String>) -> Self {
        Self { prev: None, candidate: None, baseline }
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

    /// Record a status sample; return `true` when a turn just started — the host should
    /// snapshot the worktree now and hand it back via [`set_candidate`]. A start is a
    /// transition into `Working` from a resting status; the first sample never starts a
    /// turn, since its start was not observed.
    pub fn observe(&mut self, status: Status) -> bool {
        let started = status == Status::Working && self.prev.is_some_and(Status::is_resting);
        self.prev = Some(status);
        started
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
    use super::{Status, TurnTracker};

    #[test]
    fn parse_maps_known_states_and_falls_back_to_unknown() {
        assert_eq!(Status::parse("idle"), Status::Idle);
        assert_eq!(Status::parse("working"), Status::Working);
        assert_eq!(Status::parse("blocked"), Status::Blocked);
        assert_eq!(Status::parse("done"), Status::Done);
        assert_eq!(Status::parse("starting"), Status::Unknown);
        assert_eq!(Status::parse(""), Status::Unknown);
    }

    #[test]
    fn a_turn_starts_when_working_follows_a_resting_status() {
        let mut t = TurnTracker::default();
        assert!(!t.observe(Status::Idle), "the first sample never starts a turn");
        assert!(t.observe(Status::Working), "idle → working starts a turn");
    }

    #[test]
    fn done_is_resting_so_working_after_it_starts_a_turn() {
        let mut t = TurnTracker::default();
        t.observe(Status::Done);
        assert!(t.observe(Status::Working), "done → working starts a turn");
    }

    #[test]
    fn blocked_and_unknown_to_working_are_continuations() {
        let mut t = TurnTracker::default();
        t.observe(Status::Idle);
        t.observe(Status::Working); // turn started
        t.observe(Status::Blocked); // permission prompt mid-turn
        assert!(!t.observe(Status::Working), "blocked → working resumes the same turn");
        t.observe(Status::Unknown); // transient overlay
        assert!(!t.observe(Status::Working), "unknown → working resumes the same turn");
    }

    #[test]
    fn a_lone_first_working_sample_never_starts_a_turn() {
        let mut t = TurnTracker::default();
        assert!(!t.observe(Status::Working), "we did not observe this turn's start");
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
        t.observe(Status::Idle);
        t.observe(Status::Working);
        t.set_candidate("turn-a".into());
        t.promote();
        // Turn B is question-only: candidate captured at its start, never promoted.
        t.observe(Status::Idle);
        t.observe(Status::Working);
        t.set_candidate("turn-b".into());
        assert_eq!(t.baseline(), Some("turn-a"), "the unpromoted turn keeps A's baseline");
    }
}
