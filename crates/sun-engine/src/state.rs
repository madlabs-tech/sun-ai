//! Pure run decisions over the step **state machine**: no I/O, no store.
//! The executor reads rows, asks these functions what to do, then writes.

use std::collections::HashMap;

use sun_core::model::Step;
use sun_core::{RunStatus, StepKey, StepStatus, SunEvent};
use sun_workflow::{Step as StepDef, Workflow};

/// The latest attempt of each step key (rows are one per attempt).
pub(crate) fn latest(steps: Vec<Step>) -> HashMap<StepKey, Step> {
    let mut out: HashMap<StepKey, Step> = HashMap::new();
    for step in steps {
        if out
            .get(&step.step_key)
            .is_none_or(|seen| seen.attempt < step.attempt)
        {
            out.insert(step.step_key.clone(), step);
        }
    }
    out
}

/// Steps with no attempt yet whose `needs` have all succeeded, in file order.
pub(crate) fn ready<'a>(
    workflow: &'a Workflow,
    latest: &HashMap<StepKey, Step>,
) -> Vec<&'a StepDef> {
    let succeeded = |key: &StepKey| {
        latest
            .get(key)
            .is_some_and(|s| s.status == StepStatus::Succeeded)
    };
    workflow
        .steps
        .iter()
        .filter(|def| !latest.contains_key(&def.id) && def.needs.iter().all(succeeded))
        .collect()
}

/// Where a run stands after its steps moved.
#[derive(Debug, PartialEq)]
pub(crate) enum RunOutcome {
    /// Steps are still pending or in flight.
    Ongoing,
    /// Every step succeeded.
    Succeeded,
    /// A step ended badly; the run ends with this status and reason.
    Ended(RunStatus, String),
}

/// How the run ends given its steps' latest attempts.
pub(crate) fn run_outcome(workflow: &Workflow, latest: &HashMap<StepKey, Step>) -> RunOutcome {
    for def in &workflow.steps {
        let Some(step) = latest.get(&def.id) else {
            continue;
        };
        let end = match step.status {
            StepStatus::Failed | StepStatus::Rejected => RunStatus::Failed,
            StepStatus::Cancelled => RunStatus::Cancelled,
            StepStatus::Looping => RunStatus::Looping,
            // Only the watchdog's "fail" stall policy leaves a step here.
            StepStatus::Stalled => RunStatus::Stalled,
            StepStatus::BudgetExceeded => RunStatus::BudgetExceeded,
            _ => continue,
        };
        return RunOutcome::Ended(end, format!("step `{}` {}", def.id, step.status));
    }
    let all_done = workflow.steps.iter().all(|def| {
        latest
            .get(&def.id)
            .is_some_and(|s| s.status == StepStatus::Succeeded)
    });
    if all_done {
        RunOutcome::Succeeded
    } else {
        RunOutcome::Ongoing
    }
}

/// The move an agent event causes on a `running` step, if any.
///
/// `done` only ever means `verifying`. An exit without a done claim
/// is treated as a lost session (`interrupted`); retry policy decides later.
pub(crate) fn after_agent_event(status: StepStatus, event: &SunEvent) -> Option<StepStatus> {
    if status != StepStatus::Running {
        return None;
    }
    match event {
        SunEvent::DoneClaimed { .. } => Some(StepStatus::Verifying),
        SunEvent::LimitHit { .. } => Some(StepStatus::LimitHit),
        SunEvent::Exited { .. } => Some(StepStatus::Interrupted),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use sun_core::{RunId, StepId};

    use super::*;

    const DAG: &str = "
name: dag
steps:
  - id: a
    type: wait
    for: 1s
  - id: b
    type: wait
    for: 1s
  - id: c
    type: wait
    for: 1s
    needs: [a]
  - id: d
    type: wait
    for: 1s
    needs: [b, c]
";

    fn step(key: &str, attempt: u32, status: StepStatus) -> Step {
        Step {
            id: StepId::generate(),
            run_id: RunId::generate(),
            step_key: StepKey::new(key).unwrap(),
            attempt,
            status,
            runtime_id: None,
            session_id: None,
            started_at: None,
            ended_at: None,
        }
    }

    fn keys(defs: Vec<&StepDef>) -> Vec<&str> {
        defs.iter().map(|d| d.id.as_str()).collect()
    }

    #[test]
    fn ready_follows_needs() {
        let wf = sun_workflow::parse(DAG).unwrap();
        // `b` implicitly needs `a` (no `needs:` = previous step).
        assert_eq!(keys(ready(&wf, &latest(vec![]))), ["a"]);
        let rows = vec![step("a", 1, StepStatus::Succeeded)];
        assert_eq!(keys(ready(&wf, &latest(rows))), ["b", "c"]);
        let rows = vec![
            step("a", 1, StepStatus::Succeeded),
            step("b", 1, StepStatus::Succeeded),
            step("c", 1, StepStatus::Running),
        ];
        assert!(ready(&wf, &latest(rows)).is_empty());
    }

    #[test]
    fn latest_attempt_wins() {
        let rows = vec![
            step("a", 2, StepStatus::Succeeded),
            step("a", 1, StepStatus::Failed),
        ];
        assert_eq!(latest(rows)[&StepKey::new("a").unwrap()].attempt, 2);
    }

    #[test]
    fn run_outcome_cases() {
        let wf = sun_workflow::parse(DAG).unwrap();
        let ok = |k| step(k, 1, StepStatus::Succeeded);
        assert_eq!(
            run_outcome(&wf, &latest(vec![ok("a")])),
            RunOutcome::Ongoing
        );
        assert_eq!(
            run_outcome(&wf, &latest(vec![ok("a"), ok("b"), ok("c"), ok("d")])),
            RunOutcome::Succeeded
        );
        assert_eq!(
            run_outcome(
                &wf,
                &latest(vec![ok("a"), step("b", 1, StepStatus::Rejected)])
            ),
            RunOutcome::Ended(RunStatus::Failed, "step `b` rejected".into())
        );
    }

    #[test]
    fn agent_done_only_reaches_verifying() {
        let done = SunEvent::DoneClaimed {
            summary: "ok".into(),
        };
        assert_eq!(
            after_agent_event(StepStatus::Running, &done),
            Some(StepStatus::Verifying)
        );
        assert_eq!(after_agent_event(StepStatus::Succeeded, &done), None);
        let exit = SunEvent::Exited { code: Some(1) };
        assert_eq!(
            after_agent_event(StepStatus::Running, &exit),
            Some(StepStatus::Interrupted)
        );
    }
}
