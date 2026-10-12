# AGENTS.md — rules for every coding agent working on Sun

Read this first, every session. Keep it under 2k tokens.

## Source of truth

- **The user is the source of truth.** The tasks the user gives the manager, written down by the manager in the plan and `TASKS/`, decide what gets built.
- Task plan: `TASKS/README.md` (board) and one card per phase `TASKS/<id>.md`.
- `references/` (the Madlabs design system) and `TASKS/` are local-only (git-ignored), so a worktree doesn't have them: read them from the main checkout (first path in `git worktree list`). `docs/` holds the high-level overview only, and only the manager edits it, when the user asks. Subagents never touch `docs/`. Decisions are recorded in the plan and `TASKS/`.

## Roles — technical manager + subagents

- **The main session is the technical manager. It does not write code.** It:
  - turns the user's request into detailed tasks on `TASKS/README.md`, with dependencies and what can run in parallel (∥);
  - spawns one subagent per task. Whenever the dependencies allow tasks to run in parallel, they do, in separate worktrees. The number at once is capped by memory, not by habit: before spawning another one, check `scripts/mem-check.sh`, and see "Resources" below;
  - reviews every result: reads the diff, checks security and correctness, and makes sure the checks pass;
  - sends fixes back to the same subagent;
  - passes every subagent question and open question to the user, and waits for the answer before acting on it;
  - merges;
  - after a merge, deletes the merged worktree's `target/` to free disk space. Each worktree's own target holds its own crates; dependencies come from the sccache cache.
- **Subagents do the coding.** Each one has its own worktree and branch, and reads only what its task names. It keeps the existing build and tests green, commits on its branch, and reports back in its final message: commits, check results, deviations and open questions. No hand-off files.
- **Test specialist.** When all implementation subagents for a request are done and merged, the manager spawns a test-specialist subagent. It writes the integration and unit test scenarios for the whole change, including edge cases, abuse cases and permission boundaries. It also runs the full workspace suite and the full Playwright run (mock + real). Any bug it finds goes back to an implementation subagent for the fix.

## Commits — Conventional Commits

Format: `<type>(<scope>): <summary>` — imperative, lowercase, no trailing period, ≤ 72 chars.

- Types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `build`, `ci`, `chore`, `revert`.
- Scope = crate or area without the prefix: `core`, `store`, `engine`, `runtime`, `host`, `api`, `cli`, `web`, `mod`.
- Breaking change: `feat(store)!: ...` plus a `BREAKING CHANGE:` footer.
- One logical change per commit. Example: `feat(engine): resume runs from the step journal after crash`.

## Naming

- **Default is snake_case. Never use camelCase.** Applies to variables, functions, fields, parameters and file names in every language (Rust, TypeScript, JavaScript, Svelte).
- JSON / YAML / wire formats / API fields / DB columns: snake_case. Rust's serde default already emits field names as-is — don't add `rename_all = "camelCase"`.
- Types, traits, classes and components: PascalCase. Constants and env vars: UPPER_SNAKE_CASE.
- Third-party APIs that require camelCase: convert at the boundary (one adapter), never leak it inward.
- Names say what a thing is; no abbreviations beyond common ones (`id`, `url`, `ctx`).

## Design principles (mandatory)

**DRY** — one owner per rule. Before writing code, search for an existing type, helper or pattern and reuse it. Shared logic lives in the lowest crate allowed to own it (`sun-core` for domain types). No copy-pasted blocks; extract on the second copy.

**SOLID**
- **S** — one reason to change per module/type. A crate's `lib.rs` doc comment states its single responsibility.
- **O** — extend through traits and new variants (new runtime = new `RuntimeAdapter` impl), not by editing callers.
- **L** — every trait impl honors the trait's documented contract; contract tests run against all impls.
- **I** — small, focused traits (`HostLink`, `ModLink`, `EngineHandle`); no god-traits.
- **D** — higher crates depend on traits in `sun-core`, never on concrete implementations. `sun-cli` wires the real ones. Respect the existing crate dependencies.

**ACID** — every state change is a transaction.
- Step result + event append + outbox row commit in **one** transaction. Never write state outside a transaction.
- Side effects (PRs, messages, webhooks) only through the outbox, after commit, idempotent by key.
- Schema changes are migrations with a backup taken first; never edit an applied migration.
- Concurrency: SQLite has one writer — serialize writes through the store, don't hold a transaction across `.await` on I/O.

## Design patterns (mandatory, named)

Use the established pattern for each job and name it in the type's doc comment. Patterns already in use:

| Concern | Pattern |
|---|---|
| Runtime CLIs (Claude Code, Codex, OpenCode) | **Adapter** behind `RuntimeAdapter` |
| Run lifecycle | **State machine** (explicit states, exhaustive `match`) |
| Side effects after commit | **Transactional outbox** |
| Persistence | **Repository** traits in `sun-store` |
| Events / timelines | **Event sourcing** (append-only log, `seq`) + **Observer** (subscribe/SSE) |
| Failover chains, gates | **Strategy** / **Chain of Responsibility** |
| Mods | **Plugin** over JSON-RPC stdio |
| Wiring | **Dependency injection** in `sun-cli` |

A pattern must solve a present problem. No interface with a single implementation unless it is an existing crate boundary or needed for a test fake.

## UI — Madlabs design system (mandatory)

- Every screen uses `@hryer/madlabs-design-system` (Svelte 5 components from `/svelte`, `styles.css` once in the root layout). Source and rules: `references/madlabs-design-system/` — read `SKILL.md`, `readme.md`, `svelte/README.md`, `components/MARKUP.md` first.
- Sun is the Orchestrator product: app root sets `data-product="orchestrator"` and `data-theme="dark"`. Copy screen patterns from `ui_kits/orchestrator/` (board, task drawer, agents, skills) and `ui_kits/orchestrator-landing/`.
- No hand-rolled buttons, inputs, dialogs, badges or colors when a component or token exists. Missing primitive: build it from tokens (`--space-*`, `--radius-*`, semantic colors) in `web/src/lib/ui/`, never with raw hex.
- Lucide icons via `Icon`; no emoji. Text contrast ≥ 4.5:1. Mobile-first from 360 px.

## Resources — RAM and CPU (mandatory)

- The dev machine has 16 GB RAM shared with the user's own apps and other agents. Always keep **at least 1 GB free as a buffer** (`MEM_BUFFER_GB`, default 1).
- Before every heavy step (cargo build/test, release build, Playwright, VMs, browsers) run `scripts/mem-check.sh <need_gb> --wait` (default need 3 GB). It passes only when available RAM ≥ need + 1 GB and CPU load is below 1.5 × cores; otherwise wait or reduce work — never push past it.
- Build env for every cargo command: `RUSTC_WRAPPER=sccache CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=4 CARGO_INCREMENTAL=0`.
  - sccache shares compiled dependencies across worktrees, so a new worktree doesn't rebuild them from scratch.
  - Incremental stays off, because sccache refuses incremental builds.
  - CI keeps its own settings.
  - New worktree: run `npm run build` in `web/` before the first `sun-api` build. Otherwise the UI is embedded empty and sccache keeps serving that build. If the real UI answers 404 at `/`, rebuild `sun-api` once without `RUSTC_WRAPPER`.
- Playwright `--workers=2`. Never two full `cargo test --workspace` or release builds at once. Stress loops: at most 2 copies.
- Stop every server, browser, emulator or database you started before finishing.

## Code rules

- Rust stable pinned in `rust-toolchain.toml`, edition 2024, Tokio. Errors: `thiserror` in libraries, `anyhow` only in `sun-cli`. No `unwrap()`/`expect()` outside tests.
- Validate all input at trust boundaries (API, host protocol, mod protocol, YAML). Redact secrets with the shared filter.
- Every phase leaves one runnable check (its exit command). Use `cargo test` and Playwright only.
- Integration tests: one binary per crate, `crates/<crate>/tests/it/main.rs`. A new test file is `tests/it/<name>.rs` plus `mod <name>;` in `main.rs`, never a new top-level `tests/*.rs` (each one is another link). Run one file with `cargo test -p <crate> --test it <name>::`.
- Before a commit, run only what the change touches:
  - `cargo fmt --check && cargo clippy --all-targets -- -D warnings`;
  - `cargo test -p <each crate touched>`, plus the crates that depend on it when its public API changed;
  - if `web/` is touched: `npm run check` and the affected Playwright specs.

  The full workspace suite and the full Playwright run (mock + real) happen once per request, in the test-specialist pass.
- Don't add a dependency for something a few lines of std can do; pin versions; confirm the crate is maintained.
