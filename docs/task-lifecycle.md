# Task lifecycle

How one task runs, from create to done. Back to the [overview](README.md).

A task runs one workflow: a versioned YAML list of steps. Step types are `agent`, `gate`, `action`, `approval`, `wait`, `notify` and extension steps. The flow below follows the common shape: an agent step, a gate, a PR, an approval.

## The flow

```text
  create: UI · sun task create · webhook · cron · mod event
     │
     ▼
  queue + flow control      priority, then FIFO; caps per runtime, host and workspace;
     │                      placed on the least busy host that has the runtime logged in
     ▼
  step state machine        queued → dispatched → running → verifying → succeeded
     │
     ▼
  context pack              ≤ 10% of the model's window, written as files into the session
     │
     ▼
  sun host session          the agent CLI runs in the task's worktree
     │  events (session_id, seq) ──► kernel stores them, acks ──► SSE timeline in the UI
     │
     ├── limit hit ──► failover: checkpoint commit + hand-off pack (no LLM call)
     │                 ──► new session on the next runtime, same worktree
     ├── stall · loop · blown budget ──► watchdog steps in
     │
     ▼  agent says "done"
  gates                     test · lint · diff_not_empty · content gates · …
     │                      fail ──► send back to the agent step (up to `max`)
     ▼
  action: github.open_pr    outbox row in the step's transaction, delivered after commit
     │
     ▼
  approval                  waits for an authorized person, or its timeout
     │
     ▼
  done
```

## The steps

1. **Create.** From the UI, `sun task create`, a webhook (`POST /api/v1/workflows/{id}/trigger`), a cron trigger (UTC) or a mod event. The queue inserts a `queued` run pinned to the workflow's latest version.
2. **Queue and flow control.** One scheduler admits runs by priority, then FIFO, while the caps per runtime, host and workspace allow. It never starts new work on a runtime that is cooling down after a limit. It places each run on the least busy online host with the runtime installed and logged in; every later session of that run stays there. Code: `crates/sun-engine/src/queue.rs`.
3. **Step state machine.** Every step attempt moves through explicit states in `sun_core`. Engine-driven steps (gate, action, approval, wait, notify) skip `dispatched`. Only the engine moves a step to `succeeded`; an agent's "done" only moves it `running → verifying`.
4. **Context pack.** Before each agent session the engine compiles a pack: its budget is the step's budget or 10% of the model's window, whichever is smaller. It is redacted, stored as a blob and written into the session as files (`.sun/context/*`, `.sun/progress.md`). Agents pull more through the `sun` MCP server. Code: `crates/sun-context`.
5. **Host session.** The host starts the agent CLI through its Adapter, in the task's worktree on branch `sun/<task_id>`. Output becomes `SunEvent`s.
6. **Events back.** Each event carries `(session_id, seq)`. The host keeps it on disk until the kernel acks it. The kernel appends it to the event log, and the UI follows the run over SSE.
7. **Watchdog.** It watches the event stream for stalls, loops and blown budgets (wall time, turns, cost). A stall gets its `on_stall` policy: `nudge`, `restart` or `fail`. A loop or a blown budget ends the step, and with it the run.
8. **Limit hit and failover.** A quota message becomes `LimitHit`; the step goes `limit_hit` and the runtime cools down until its reset. The `on_limit` policy decides: `switch`, `wait_until_reset` or `pause_and_notify` (task, then workflow, then workspace setting; `switch` when none is set, a default still under review). On `switch`, the host makes a checkpoint commit `wip(sun): handoff from <runtime>` (not pushed), the engine builds a hand-off pack without an LLM call, and a new session of the next runtime in `runtime.chain` continues in the **same worktree**. The step attempt is the same, so nothing re-runs. Code: `crates/sun-engine/src/failover.rs`.
9. **Gates and approvals decide done.** Gate steps check the finished work. Code gates run on the host; content gates run in the engine; `pr_exists` and `ci_green` ask a mod. A failed gate sends the work back to `on_fail.send_back_to`, up to `max` times, with the failure output. An approval step waits in `waiting_approval` until an authorized person decides, or its `timeout:` acts. Code: `crates/sun-engine/src/gates/`, `crates/sun-engine/src/approval.rs`.
10. **Side effects after commit.** Action and notify steps (open a PR, send a message) write outbox rows in the same transaction as the step change. Before `open_pr`, the engine pushes `sun/<task_id>`. A worker delivers each row through the mod after commit, with an idempotency key. Code: `crates/sun-engine/src/outbox.rs`.
11. **Done.** When every step has succeeded, the run succeeds and the task is `done`. A failed or rejected step, a cancel, a loop or a blown budget ends the run instead.

## Crash and resume

- **One transaction per state change.** Step or run state, the event rows and the outbox rows commit together. No transaction is held across host or mod I/O.
- **The journal.** The event log is append-only. Every host call, timer and outbox wake-up can be re-derived from the database, so on boot the executor resumes every running run where it stopped. A `succeeded` step never runs again.
- **Host events.** A lost connection loses nothing: the host re-sends unacked events, and the kernel dedupes on `(session_id, seq)`.
- **Outbox.** A row goes `pending → unknown` in its own transaction before the mod is called. An `unknown` row is never resent blindly: on start-up the engine asks the mod (`reconcile`), or a person decides.
- **Hand-off.** `handoff_started` is journalled with the move, so a hand-off cut short by a crash is redone on resume.
- `bash scripts/e2e.sh` checks this end to end, including `kill -9` crash resumes.
