//! Lifecycle **state machines** for tasks, runs and steps.
//!
//! Each status enum lists its legal next states in one exhaustive `match`
//! (no wildcard arm), so adding a state forces a decision about its edges.
//! The engine is the only caller that moves states; it always goes through
//! [`StateMachine::transition`].

use crate::{Error, Result};

/// A status enum with a fixed transition table.
pub trait StateMachine: Copy + Eq + 'static {
    /// Name used in errors, e.g. `StepStatus`.
    const NAME: &'static str;

    /// The snake_case name of this state.
    fn as_str(self) -> &'static str;

    /// Every state reachable in one move from `self`.
    fn next_states(self) -> &'static [Self];

    /// True when no transition leaves this state.
    fn is_terminal(self) -> bool {
        self.next_states().is_empty()
    }

    /// Whether `self -> next` is a legal move.
    fn can_transition_to(self, next: Self) -> bool {
        self.next_states().contains(&next)
    }

    /// Returns `next` if the move is legal, else [`Error::InvalidTransition`].
    fn transition(self, next: Self) -> Result<Self> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(Error::InvalidTransition {
                machine: Self::NAME,
                from: self.as_str(),
                to: next.as_str(),
            })
        }
    }
}

macro_rules! impl_state_machine {
    ($name:ident) => {
        impl StateMachine for $name {
            const NAME: &'static str = stringify!($name);

            fn as_str(self) -> &'static str {
                $name::as_str(self)
            }

            fn next_states(self) -> &'static [Self] {
                self.edges()
            }
        }
    };
}

string_enum! {
    /// State of one step attempt (plus `cancelled` and
    /// `queued -> running` for engine-driven steps).
    pub enum StepStatus {
        /// Waiting for a host/runtime slot.
        Queued => "queued",
        /// Sent to a host; the session is starting.
        Dispatched => "dispatched",
        /// The agent (or action) is working.
        Running => "running",
        /// The agent claimed done; gates are running. Only the engine leaves this state.
        Verifying => "verifying",
        /// Gates passed (or approval granted). Never re-executed.
        Succeeded => "succeeded",
        /// Gates failed after the last send-back.
        Failed => "failed",
        /// The runtime hit a usage/rate limit.
        LimitHit => "limit_hit",
        /// A hand-off pack is being built for the next runtime.
        HandingOff => "handing_off",
        /// Watchdog: no progress for N minutes.
        Stalled => "stalled",
        /// Watchdog: the same action or error repeated K times.
        Looping => "looping",
        /// Watchdog: wall time, turns or cost exceeded.
        BudgetExceeded => "budget_exceeded",
        /// The host was lost mid-step.
        Interrupted => "interrupted",
        /// Blocked on a human decision.
        WaitingApproval => "waiting_approval",
        /// The approver rejected.
        Rejected => "rejected",
        /// A user cancelled the run.
        Cancelled => "cancelled",
    }
}

impl StepStatus {
    fn edges(self) -> &'static [Self] {
        use StepStatus::*;
        match self {
            // `queued -> running`: steps without an agent (approval, wait, notify,
            // action, gate) are driven by the engine itself; nothing is
            // dispatched to a host, so they skip `dispatched`.
            Queued => &[Dispatched, Running, Cancelled],
            // `→ failed`: the host stayed unreachable past the retry bound.
            Dispatched => &[Running, Failed, Cancelled],
            // `→ handing_off`: a manual switch or a context rotation.
            Running => &[
                Verifying,
                LimitHit,
                HandingOff,
                Stalled,
                Looping,
                BudgetExceeded,
                Interrupted,
                WaitingApproval,
                Cancelled,
            ],
            // Gate fail: send back to running (the engine enforces `max`), else fail.
            // `→ waiting_approval`: a human reviews an agent step that failed over.
            Verifying => &[Succeeded, Running, Failed, WaitingApproval, Cancelled],
            LimitHit => &[HandingOff, Cancelled],
            HandingOff => &[Dispatched, Cancelled],
            // Stall policy: nudge (back to running) or restart with a hand-off.
            // "Fail and notify" leaves the step in `stalled`.
            Stalled => &[Running, HandingOff, Cancelled],
            Interrupted => &[Dispatched, Cancelled],
            // `→ running`: a human answered an agent's `sun.ask`.
            WaitingApproval => &[Succeeded, Rejected, Running, Cancelled],
            Succeeded | Failed | Looping | BudgetExceeded | Rejected | Cancelled => &[],
        }
    }
}
impl_state_machine!(StepStatus);

string_enum! {
    /// State of a run. Every run ends in an explicit terminal state.
    pub enum RunStatus {
        /// Created, not started.
        Queued => "queued",
        /// Steps are executing.
        Running => "running",
        /// Waiting on a human: approval, limit policy `pause_and_notify`, or a user pause.
        Paused => "paused",
        /// Every step succeeded.
        Succeeded => "succeeded",
        /// A step failed or was rejected.
        Failed => "failed",
        /// A user cancelled.
        Cancelled => "cancelled",
        /// Ended by the stall watchdog.
        Stalled => "stalled",
        /// Ended by the loop watchdog.
        Looping => "looping",
        /// Ended by a budget limit.
        BudgetExceeded => "budget_exceeded",
    }
}

impl RunStatus {
    fn edges(self) -> &'static [Self] {
        use RunStatus::*;
        match self {
            Queued => &[Running, Cancelled],
            Running => &[
                Paused,
                Succeeded,
                Failed,
                Cancelled,
                Stalled,
                Looping,
                BudgetExceeded,
            ],
            Paused => &[Running, Failed, Cancelled],
            // `→ running`: a human retried a step of a run that ended badly;
            // its worktree was kept for exactly that.
            Failed | Stalled | Looping | BudgetExceeded => &[Running],
            Succeeded | Cancelled => &[],
        }
    }

    /// Ended badly, so a human may retry a step (`→ running`).
    pub fn is_retryable(self) -> bool {
        use RunStatus::*;
        matches!(self, Failed | Stalled | Looping | BudgetExceeded)
    }
}

impl StateMachine for RunStatus {
    const NAME: &'static str = "RunStatus";

    fn as_str(self) -> &'static str {
        RunStatus::as_str(self)
    }

    fn next_states(self) -> &'static [Self] {
        self.edges()
    }

    /// Every run ends in one of these. A retryable end still has
    /// the human step-retry edge out, so this is not "no edges" here.
    fn is_terminal(self) -> bool {
        !matches!(
            self,
            RunStatus::Queued | RunStatus::Running | RunStatus::Paused
        )
    }
}

string_enum! {
    /// State of a task (its Tasks column). The engine moves a task that runs
    /// something after its run; a person moves any task within these edges.
    pub enum TaskStatus {
        /// Parked; nothing runs.
        Backlog => "backlog",
        /// Ready; a task that runs something has a queued run.
        Todo => "todo",
        /// An agent step is working.
        InProgress => "in_progress",
        /// The agent claimed done; gates are checking.
        Verifying => "verifying",
        /// Waiting on a person: an approval, a decision, or a review of the result.
        NeedsYou => "needs_you",
        /// Reviewed and finished.
        Done => "done",
        /// Stopped by a person or paused; reopens to Backlog or Todo.
        Blocked => "blocked",
        /// The run ended without success; reopens to Backlog or Todo.
        Failed => "failed",
        /// Abandoned; reopens to Backlog or Todo.
        Cancelled => "cancelled",
    }
}

impl TaskStatus {
    fn edges(self) -> &'static [Self] {
        use TaskStatus::*;
        // Open statuses move to each other and to Blocked, Failed and
        // Cancelled; Done only from Needs you.
        match self {
            Backlog => &[
                Todo, InProgress, Verifying, NeedsYou, Blocked, Failed, Cancelled,
            ],
            Todo => &[
                Backlog, InProgress, Verifying, NeedsYou, Blocked, Failed, Cancelled,
            ],
            InProgress => &[
                Backlog, Todo, Verifying, NeedsYou, Blocked, Failed, Cancelled,
            ],
            Verifying => &[
                Backlog, Todo, InProgress, NeedsYou, Blocked, Failed, Cancelled,
            ],
            NeedsYou => &[
                Backlog, Todo, InProgress, Verifying, Done, Blocked, Failed, Cancelled,
            ],
            // A stopped task can still be given up.
            Blocked => &[Backlog, Todo, Failed, Cancelled],
            Failed | Cancelled => &[Backlog, Todo],
            Done => &[],
        }
    }

    /// Still being worked on or waiting to be: not Done, Blocked, Failed or Cancelled.
    pub fn is_open(self) -> bool {
        use TaskStatus::*;
        matches!(self, Backlog | Todo | InProgress | Verifying | NeedsYou)
    }

    /// The PR-automation edges, outside the table above: once every linked
    /// PR has merged, any open task may go straight to `done` (workflow
    /// tasks) or `needs_you` (agent tasks). Only the store's
    /// `set_task_status_by_merge` moves along these.
    pub fn can_move_by_merge(self, to: TaskStatus) -> bool {
        self.is_open() && self != to && matches!(to, TaskStatus::Done | TaskStatus::NeedsYou)
    }
}
impl_state_machine!(TaskStatus);

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn edges<S: StateMachine>(all: &[S]) -> BTreeSet<(&'static str, &'static str)> {
        all.iter()
            .flat_map(|s| s.next_states().iter().map(|n| (s.as_str(), n.as_str())))
            .collect()
    }

    fn assert_all_reachable<S: StateMachine>(start: S, all: &[S]) {
        let mut seen = vec![start];
        let mut i = 0;
        while i < seen.len() {
            for &n in seen[i].next_states() {
                if !seen.contains(&n) {
                    seen.push(n);
                }
            }
            i += 1;
        }
        for s in all {
            assert!(seen.contains(s), "{} unreachable", s.as_str());
        }
    }

    #[test]
    fn step_edges_mirror_trd_diagram() {
        let mut expected: BTreeSet<_> = [
            ("queued", "dispatched"),
            // engine-driven steps (no agent) skip `dispatched`
            ("queued", "running"),
            ("dispatched", "running"),
            // the host stayed unreachable past the retry bound (1.12c)
            ("dispatched", "failed"),
            ("running", "verifying"),
            ("verifying", "succeeded"),
            ("verifying", "running"),
            ("verifying", "failed"),
            ("running", "limit_hit"),
            ("limit_hit", "handing_off"),
            ("running", "handing_off"),
            ("handing_off", "dispatched"),
            ("running", "stalled"),
            ("running", "looping"),
            ("running", "budget_exceeded"),
            ("running", "interrupted"),
            ("interrupted", "dispatched"),
            ("running", "waiting_approval"),
            ("waiting_approval", "succeeded"),
            ("waiting_approval", "rejected"),
            ("waiting_approval", "running"),
            // Review after a failover
            ("verifying", "waiting_approval"),
            // Stall policy
            ("stalled", "running"),
            ("stalled", "handing_off"),
        ]
        .into_iter()
        .collect();
        // Cancel from every non-terminal state.
        for s in [
            "queued",
            "dispatched",
            "running",
            "verifying",
            "limit_hit",
            "handing_off",
            "stalled",
            "interrupted",
            "waiting_approval",
        ] {
            expected.insert((s, "cancelled"));
        }
        assert_eq!(edges(StepStatus::ALL), expected);
    }

    #[test]
    fn step_terminal_states() {
        use StepStatus::*;
        let terminal: Vec<_> = StepStatus::ALL
            .iter()
            .copied()
            .filter(|s| s.is_terminal())
            .collect();
        assert_eq!(
            terminal,
            [
                Succeeded,
                Failed,
                Looping,
                BudgetExceeded,
                Rejected,
                Cancelled
            ]
        );
    }

    #[test]
    fn agent_done_never_reaches_succeeded_directly() {
        assert!(!StepStatus::Running.can_transition_to(StepStatus::Succeeded));
        assert_eq!(
            StepStatus::Running.transition(StepStatus::Succeeded),
            Err(Error::InvalidTransition {
                machine: "StepStatus",
                from: "running",
                to: "succeeded",
            })
        );
        assert_eq!(
            StepStatus::Running.transition(StepStatus::Verifying),
            Ok(StepStatus::Verifying)
        );
    }

    #[test]
    fn every_state_is_reachable() {
        assert_all_reachable(StepStatus::Queued, StepStatus::ALL);
        assert_all_reachable(RunStatus::Queued, RunStatus::ALL);
        assert_all_reachable(TaskStatus::Backlog, TaskStatus::ALL);
    }

    #[test]
    fn run_terminal_states_match_us_6_4() {
        let terminal: Vec<_> = RunStatus::ALL
            .iter()
            .filter(|s| s.is_terminal())
            .map(|s| s.as_str())
            .collect();
        assert_eq!(
            terminal,
            [
                "succeeded",
                "failed",
                "cancelled",
                "stalled",
                "looping",
                "budget_exceeded"
            ]
        );
    }

    #[test]
    fn only_a_run_that_ended_badly_reopens_for_a_step_retry() {
        let reopen: Vec<_> = RunStatus::ALL
            .iter()
            .filter(|s| s.is_terminal() && s.can_transition_to(RunStatus::Running))
            .copied()
            .collect();
        let retryable: Vec<_> = RunStatus::ALL
            .iter()
            .filter(|s| s.is_retryable())
            .copied()
            .collect();
        assert_eq!(reopen, retryable);
        assert!(!RunStatus::Succeeded.can_transition_to(RunStatus::Running));
        assert!(!RunStatus::Cancelled.can_transition_to(RunStatus::Running));
    }

    /// Every allowed and refused task move, against the plan's rules.
    #[test]
    fn task_moves_follow_the_lifecycle_rules() {
        use TaskStatus::*;
        let open = [Backlog, Todo, InProgress, Verifying, NeedsYou];
        for &from in TaskStatus::ALL {
            for &to in TaskStatus::ALL {
                let allowed = match (from, to) {
                    _ if from == to => false,
                    (Done, _) => false,
                    (_, Done) => from == NeedsYou,
                    (Blocked, Failed | Cancelled) => true,
                    (Blocked | Failed | Cancelled, _) => matches!(to, Backlog | Todo),
                    (_, Blocked | Failed | Cancelled) => open.contains(&from),
                    _ => open.contains(&from) && open.contains(&to),
                };
                assert_eq!(from.can_transition_to(to), allowed, "{from} -> {to}");
            }
        }
        assert!(Done.is_terminal());
        assert_eq!(
            TaskStatus::ALL.iter().filter(|s| s.is_open()).count(),
            open.len()
        );
        // PR automation: open → done | needs_you, and nothing else.
        for &from in TaskStatus::ALL {
            for &to in TaskStatus::ALL {
                let allowed = open.contains(&from) && matches!(to, Done | NeedsYou) && from != to;
                assert_eq!(
                    from.can_move_by_merge(to),
                    allowed,
                    "{from} -> {to} by merge"
                );
            }
        }
        assert!(
            !Backlog.can_transition_to(Done),
            "the table itself is unchanged"
        );
    }

    #[test]
    fn status_text_round_trips_through_str_and_json() {
        for s in StepStatus::ALL {
            assert_eq!(s.as_str().parse::<StepStatus>().unwrap(), *s);
            assert_eq!(
                serde_json::to_string(s).unwrap(),
                format!("\"{}\"", s.as_str())
            );
        }
        assert!("done".parse::<StepStatus>().is_err());
    }
}
