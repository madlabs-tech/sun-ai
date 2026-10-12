//! Contract-test harness: the [`RuntimeAdapter`](sun_core::RuntimeAdapter)
//! event contract as checks over one session's events. The same checks run
//! against every adapter's recorded fixtures (and its live runs), so all
//! impls honor one contract (AGENTS.md, Liskov).

use sun_core::SunEvent;

use crate::limit;

/// A recorded fixture case: what the session should end with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    /// The task succeeds: ends with `DoneClaimed`.
    Normal,
    /// A tool fails but the agent finishes: a failed `ToolResult`, then `DoneClaimed`.
    ToolError,
    /// The runtime hits a limit: ends with `LimitHit` (with its reset time).
    LimitHit,
    /// The process dies mid-turn: ends with `Error`.
    Crash,
}

impl Case {
    /// Every case.
    pub const ALL: [Self; 4] = [Self::Normal, Self::ToolError, Self::LimitHit, Self::Crash];

    /// The fixture file stem, e.g. `tool-error`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::ToolError => "tool-error",
            Self::LimitHit => "limit-hit",
            Self::Crash => "crash",
        }
    }
}

/// Every contract rule `events` (one single-turn session) breaks for `case`;
/// empty when it holds.
pub fn violations(case: Case, events: &[SunEvent]) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |ok: bool, rule: &str| {
        if !ok {
            out.push(rule.to_owned());
        }
    };

    // Ordering: one `Exited`, last.
    let exits = events
        .iter()
        .filter(|e| matches!(e, SunEvent::Exited { .. }));
    check(exits.count() == 1, "exactly one Exited");
    check(
        matches!(events.last(), Some(SunEvent::Exited { .. })),
        "Exited is the last event",
    );

    // Ordering: a tool result never comes before its call.
    let mut open_calls = 0i64;
    let mut results_follow_calls = true;
    for event in events {
        match event {
            SunEvent::ToolCall { .. } => open_calls += 1,
            SunEvent::ToolResult { .. } => {
                open_calls -= 1;
                results_follow_calls &= open_calls >= 0;
            }
            _ => {}
        }
    }
    check(results_follow_calls, "every ToolResult follows a ToolCall");

    // Exactly one terminal event per turn, followed only by usage and `Exited`.
    let terminals: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| terminal(e))
        .map(|(i, _)| i)
        .collect();
    check(
        terminals.len() == 1,
        "exactly one terminal event (DoneClaimed, LimitHit or Error)",
    );
    if let Some(&last) = terminals.last() {
        check(
            events[last + 1..]
                .iter()
                .all(|e| matches!(e, SunEvent::Usage { .. } | SunEvent::Exited { .. })),
            "only Usage and Exited follow the terminal event",
        );
    }

    // Limits are never reported as errors.
    check(
        !events.iter().any(
            |e| matches!(e, SunEvent::Error { message } if limit::classify(message).is_some()),
        ),
        "limit text is a LimitHit, not an Error",
    );

    let terminal = terminals.first().map(|&i| &events[i]);
    match case {
        Case::Normal => check(
            matches!(terminal, Some(SunEvent::DoneClaimed { .. })),
            "normal: DoneClaimed detected",
        ),
        Case::ToolError => {
            check(
                matches!(terminal, Some(SunEvent::DoneClaimed { .. })),
                "tool-error: DoneClaimed detected",
            );
            check(
                events
                    .iter()
                    .any(|e| matches!(e, SunEvent::ToolResult { ok: false, .. })),
                "tool-error: a failed ToolResult",
            );
        }
        Case::LimitHit => check(
            matches!(
                terminal,
                Some(SunEvent::LimitHit {
                    reset_at: Some(_),
                    ..
                })
            ),
            "limit-hit: LimitHit with a reset time",
        ),
        Case::Crash => check(
            matches!(terminal, Some(SunEvent::Error { .. })),
            "crash: Error before Exited",
        ),
    }
    out
}

fn terminal(event: &SunEvent) -> bool {
    matches!(
        event,
        SunEvent::DoneClaimed { .. } | SunEvent::LimitHit { .. } | SunEvent::Error { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_broken_streams() {
        let done = SunEvent::DoneClaimed {
            summary: "ok".into(),
        };
        let exited = SunEvent::Exited { code: Some(0) };
        assert!(violations(Case::Normal, &[done.clone(), exited.clone()]).is_empty());
        assert_eq!(
            violations(Case::Normal, &[exited.clone(), done.clone()]),
            ["Exited is the last event"]
        );
        let late_text = SunEvent::Text {
            role: sun_core::Role::Assistant,
            text: "more".into(),
        };
        assert_eq!(
            violations(Case::Normal, &[done.clone(), late_text, exited.clone()]),
            ["only Usage and Exited follow the terminal event"]
        );
        let limit_as_error = SunEvent::Error {
            message: "Rate limit exceeded".into(),
        };
        assert_eq!(
            violations(Case::LimitHit, &[limit_as_error, exited]),
            [
                "limit text is a LimitHit, not an Error",
                "limit-hit: LimitHit with a reset time"
            ]
        );
    }
}
