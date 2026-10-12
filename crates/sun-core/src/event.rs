//! [`SunEvent`]: the normalized agent event stream.

use serde::{Deserialize, Serialize};

use crate::Timestamp;

/// One normalized event from an agent session. Every runtime adapter maps its
/// CLI's output to these; the engine and watchdog never see CLI-specific data.
///
/// JSON form is internally tagged: `{"kind": "text", "role": "assistant", ...}`.
/// Text fields must be passed through [`crate::redact`] before they are stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SunEvent {
    /// A message from the agent, the user or the system prompt.
    Text {
        /// Who wrote it.
        role: Role,
        /// The text.
        text: String,
    },
    /// The agent called a tool.
    ToolCall {
        /// Tool name, e.g. `Bash`.
        name: String,
        /// Stable hash of the arguments, used by loop detection.
        args_hash: u64,
        /// Short human-readable summary.
        summary: String,
    },
    /// A tool call finished.
    ToolResult {
        /// Whether the tool succeeded.
        ok: bool,
        /// Short human-readable summary.
        summary: String,
    },
    /// The agent changed a file in its worktree.
    FileChanged {
        /// Path relative to the worktree root.
        path: String,
    },
    /// The agent's plan / todo list changed (TodoWrite, plan updates).
    Plan {
        /// The full current list.
        todos: Vec<Todo>,
    },
    /// Token / cost usage since the previous `Usage` event.
    Usage {
        /// Input tokens.
        input: u64,
        /// Output tokens.
        output: u64,
        /// Cost in USD, when the CLI reports it.
        cost_usd: Option<f64>,
        /// Tokens currently in the context window, when known.
        context_used: Option<u32>,
    },
    /// The runtime hit a limit.
    LimitHit {
        /// Which kind of limit.
        limit: LimitKind,
        /// When the limit resets, if the CLI said so.
        reset_at: Option<Timestamp>,
        /// The original message (redacted), for audit and classifier tuning.
        raw: String,
    },
    /// The agent says it is done. Moves the step to `verifying`, never to `succeeded`.
    DoneClaimed {
        /// The agent's own summary of its work.
        summary: String,
    },
    /// The runtime reported an error that is not a limit.
    Error {
        /// The error text (redacted).
        message: String,
    },
    /// The CLI process exited.
    Exited {
        /// Exit code, if the process exited normally.
        code: Option<i32>,
    },
}

string_enum! {
    /// Author of a [`SunEvent::Text`].
    pub enum Role {
        /// The human or Sun speaking as the user.
        User => "user",
        /// The agent.
        Assistant => "assistant",
        /// System / envelope text.
        System => "system",
    }
}

string_enum! {
    /// Kind of limit in [`SunEvent::LimitHit`].
    pub enum LimitKind {
        /// Short-term request/token rate limit.
        RateLimit => "rate_limit",
        /// Subscription usage quota (e.g. 5-hour or weekly window).
        Quota => "quota",
        /// Out of paid credits or API balance.
        Credits => "credits",
    }
}

/// One item of an agent's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    /// What to do.
    pub text: String,
    /// Progress.
    pub status: TodoStatus,
}

string_enum! {
    /// Progress of a [`Todo`].
    pub enum TodoStatus {
        /// Not started.
        Pending => "pending",
        /// Being worked on.
        InProgress => "in_progress",
        /// Finished.
        Done => "done",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_is_tagged_snake_case() {
        let event = SunEvent::LimitHit {
            limit: LimitKind::Quota,
            reset_at: None,
            raw: "usage limit reached".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "kind": "limit_hit",
                "limit": "quota",
                "reset_at": null,
                "raw": "usage limit reached",
            })
        );
        assert_eq!(serde_json::from_value::<SunEvent>(json).unwrap(), event);
    }
}
