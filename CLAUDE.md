@AGENTS.md

## Skills (mandatory for every Claude session and sub-agent)

- Start every session by invoking the `ponytail:ponytail` skill (full mode): simplest complete solution, reuse before writing, no speculative abstractions.
- Also invoke `ui-ux-pro-max:ui-ux-pro-max` at session start, and use it for every UI, UX, copy or layout decision. It works inside the Madlabs design system (see AGENTS.md "UI"), never instead of it.
- Every sub-agent runs in a fresh session (its own worktree) and follows AGENTS.md and this file.
