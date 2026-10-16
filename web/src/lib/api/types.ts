// Wire shapes for the kernel REST API under /api/v1.
// snake_case on the wire and in code. Mirror of the sun-core domain types; when
// sun-core lands, these must match its serde output field for field.

export type Ulid = string;
/** RFC 3339 UTC timestamp */
export type Timestamp = string;

/** A task's status: its Tasks column. The engine moves a task that runs something
 *  after its run; a person moves any task (`PATCH /tasks/{id}`, see `TASK_MOVES`). */
export type TaskStatus =
  | 'backlog'
  | 'todo'
  | 'in_progress'
  | 'verifying'
  | 'needs_you'
  | 'done'
  | 'blocked'
  | 'failed'
  | 'cancelled';

/** Where a person may move a task from each status (the engine's rules): open statuses
 *  move to each other and to Blocked, Failed or Cancelled; Done only from Needs you;
 *  Blocked, Failed and Cancelled reopen to Backlog or Todo; Done is final. Verifying is
 *  the engine's for a task that runs a workflow or an agent. */
export const TASK_MOVES: Record<TaskStatus, TaskStatus[]> = {
  backlog: ['todo', 'in_progress', 'verifying', 'needs_you', 'blocked', 'failed', 'cancelled'],
  todo: ['backlog', 'in_progress', 'verifying', 'needs_you', 'blocked', 'failed', 'cancelled'],
  in_progress: ['backlog', 'todo', 'verifying', 'needs_you', 'blocked', 'failed', 'cancelled'],
  verifying: ['backlog', 'todo', 'in_progress', 'needs_you', 'blocked', 'failed', 'cancelled'],
  needs_you: ['backlog', 'todo', 'in_progress', 'verifying', 'done', 'blocked', 'failed', 'cancelled'],
  blocked: ['backlog', 'todo', 'failed', 'cancelled'],
  failed: ['backlog', 'todo'],
  cancelled: ['backlog', 'todo'],
  done: [],
};

/** The statuses a person may move `t` to: `TASK_MOVES`, less Verifying for a task that runs something. */
export const legal_moves = (t: { status: TaskStatus; workflow: unknown; assignee: unknown }) =>
  TASK_MOVES[t.status].filter((s) => s !== 'verifying' || (!t.workflow && !t.assignee));

/** A run's state as the UI shows it: `running` splits into `verifying` and
 *  `waiting_approval` by its steps; a paused run also reads `waiting_approval`. */
export type RunStatus = 'queued' | 'running' | 'verifying' | 'waiting_approval' | 'done' | 'failed' | 'cancelled';

/** The queue admits by rank: urgent, high, medium, low, then no priority. */
export type Priority = 'urgent' | 'high' | 'medium' | 'low' | 'none';

/** Step attempt state machine. Steps not reached yet report `queued`. */
export type StepStatus =
  | 'queued'
  | 'dispatched'
  | 'running'
  | 'verifying'
  | 'succeeded'
  | 'failed'
  | 'limit_hit'
  | 'handing_off'
  | 'stalled'
  | 'looping'
  | 'budget_exceeded'
  | 'interrupted'
  | 'waiting_approval'
  | 'rejected';

/** `extension` = a mod's step, e.g. `device.verify_on_device`. */
export type StepType = 'agent' | 'gate' | 'action' | 'approval' | 'wait' | 'notify' | 'decide' | 'extension';

/** A human's answer to an escalated `decide` step, validated against its question. */
export type DecisionAnswer = { choice: string } | { score: number } | { noul: number };

/** Why a run ended. `unverified` = no gate and no approval. */
export type EndReason = 'gates_passed' | 'approved' | 'unverified' | 'failed' | 'rejected' | 'cancelled';

export interface Budget {
  wall?: string;
  turns?: number;
  cost_usd?: number;
}

export interface Step {
  step_key: string;
  /** Display name from the workflow definition */
  name: string;
  type: StepType;
  attempt: number;
  status: StepStatus;
  runtime_id: string | null;
  started_at: Timestamp | null;
  ended_at: Timestamp | null;
}

/** A side effect in the outbox (`GET /outbox`). `unknown`: delivery timed out and waits
 *  for a person to say whether it happened. */
export interface OutboxItem {
  id: Ulid;
  /** `null` for a side effect outside any run (a failed trigger's notice) */
  run_id: Ulid | null;
  step_key: string | null;
  /** `mod.action`, e.g. `github.open_pr` */
  target: string;
  idem_key: string;
  status: 'pending' | 'delivered' | 'unknown' | 'failed';
  /** PR title or message text */
  summary: string | null;
}

export type Resolution = 'delivered' | 'resend';

export interface Run {
  id: Ulid;
  task_id: Ulid;
  workflow_version: number;
  status: RunStatus;
  end_reason: EndReason | null;
  budget: Budget;
  cost_usd: number;
  runtime_id: string | null;
  /** Failover order from the workflow */
  runtime_chain: string[];
  /** How many times the run switched runtime; > 0 shows a failover badge */
  runtime_switches: number;
  steps: Step[];
  /** The workspace repos the run works in (resolved from the task text); none: a plain folder */
  repo_ids: string[];
}

export interface WorkflowRef {
  id: Ulid;
  name: string;
  /** The workspace it is on; its tasks are on that workspace too. */
  workspace_id: Ulid;
  version: number;
}

/** GET /api/v1/workspaces — workspace = workspace; each owns its workflows and their tasks. */
export interface Workspace {
  id: Ulid;
  name: string;
  /** URL name, `^[a-z0-9]+(-[a-z0-9]+)*$`, 1–48 chars; unique and never changed. */
  slug: string;
  /** Up to 1000 characters. */
  description: string;
  /** Standing instructions in every run's context pack ("Workspace context"); up to 8000 characters. */
  context: string;
  /** `GET /workspaces/{id}/logo`, versioned by the image; `null`: no logo. */
  logo_url: string | null;
  /** Task keys are `{issue_prefix}-{number}`; `^[A-Z0-9]{1,10}$`. */
  issue_prefix: string;
  /** The last task number given. */
  issue_counter: number;
  /** The workflow new tasks get by default; one of this workspace's. */
  default_workflow_id: Ulid | null;
  created_at: Timestamp;
  /** The member who created it (its first owner); null for Main and older workspaces. */
  created_by: Ulid | null;
  git_settings: GitSettings;
}

/** How the workspace treats its git hosts' pull requests (Settings → Code); all on by default. */
export interface GitSettings {
  github_enabled: boolean;
  /** Show linked pull requests on the task page. */
  pr_sidebar: boolean;
  /** Link a PR to the task whose key is in its title or branch, or after "Closes" in its body. */
  auto_link: boolean;
  /** Every linked PR merged: a workflow task moves to Done, an agent task to Needs you. */
  move_on_merge: boolean;
}

/** PATCH /api/v1/workspaces/{id} — absent fields stay; `default_workflow_id: null` clears it. */
export interface WorkspaceProfileChange {
  name?: string;
  description?: string;
  context?: string;
  default_workflow_id?: Ulid | null;
  git_settings?: GitSettings;
}

/** GET /api/v1/workspaces/{id}/projects — a named group of one workspace's tasks. */
export interface Project {
  id: Ulid;
  workspace_id: Ulid;
  /** 1–64 characters, unique in the workspace ignoring case. */
  name: string;
  /** Up to 1000 characters. */
  description: string;
  created_at: Timestamp;
}

/** Workspace "Main": every older workflow is on it, and it can't be deleted. */
export const MAIN_WORKSPACE = '00000000000000000000000001';

/** GET /api/v1/tasks — one row per workspace card */
export interface TaskSummary {
  id: Ulid;
  /** `{issue_prefix}-{number}`, e.g. "ENG-12"; paths take it too, in any case */
  key: string;
  workspace_id: Ulid;
  /** Its number on the workspace */
  number: number;
  title: string;
  body: string;
  priority: Priority;
  status: TaskStatus;
  /** `null`: a plain task that never runs */
  workflow: WorkflowRef | null;
  project_id: Ulid | null;
  /** Who works on it: an agent (runs its hidden system workflow) or a workflow; `null` for a plain task */
  assignee: Assignee | null;
  /** Manual order inside its column, ascending */
  position: number;
  /** The member who created it; `null`: the engine */
  created_by: Ulid | null;
  /** Latest run; null until first dispatch */
  run: Run | null;
  created_at: Timestamp;
  updated_at: Timestamp;
}

/** PATCH /api/v1/tasks/{id} — absent fields stay; a status is a move (409 when illegal). */
export interface TaskPatch {
  title?: string;
  body?: string;
  status?: TaskStatus;
  priority?: Priority;
  /** `null` takes it out of its project */
  project_id?: Ulid | null;
  /** An agent (cancels a run going and requeues on an open task), a workflow (only before the first run), or `null`: a plain task */
  assignee?: Assignee | null;
  position?: number;
}

/** Who works on a task. */
export type Assignee = { kind: 'agent' | 'workflow'; id: Ulid };

export type ActivityKind = 'comment' | 'status' | 'priority' | 'assignee' | 'project' | 'title' | 'description' | 'created' | 'run' | 'pull_request';

/** GET /api/v1/tasks/{id}/activity — the append-only log of a task (SSE with `Accept: text/event-stream`). */
export interface TaskActivity {
  id: Ulid;
  task_id: Ulid;
  /** From 1, no gaps */
  seq: number;
  kind: ActivityKind;
  /** The person's member row; `null` for the engine, or the token holder (see `body.by`) */
  actor_member_id: Ulid | null;
  /** `comment`: `{text}`; `status`/`priority`/`title`/`project`: `{from, to}` (a merge-driven move adds
   *  `source: pr_automation, pull_requests: "#12, #15"`); `created`: `{status}`; `run`: `{run_id, trigger}`;
   *  `pull_request`: `{action: linked|unlinked|merged, number, url, repo, source}`.
   *  A person's entry also has `by`, the audit actor (e.g. `user:ana`); without it the engine did it. */
  body: Record<string, unknown>;
  created_at: Timestamp;
}

/** GET /api/v1/tasks/{id}/runs — the task's runs, oldest first. */
export interface TaskRun {
  id: Ulid;
  /** 1 for the first run */
  attempt: number;
  /** `created`, `trigger`, `moved`, `retry`, or `manual` (older runs) */
  trigger: string;
  status: RunStatus;
  cost_usd: number;
  started_at: Timestamp | null;
  ended_at: Timestamp | null;
}

/** GET /api/v1/runs/{id} — any run of a task, in the task detail's shapes. */
export interface RunDetail {
  run: Run;
  gates: GateResult[];
}

export type GateStatus = 'pending' | 'running' | 'passed' | 'failed';

export interface GateResult {
  step_key: string;
  name: string;
  status: GateStatus;
}

export interface Artifact {
  id: Ulid;
  task_id: Ulid;
  name: string;
  version: number;
  mime: string;
}

/** Design canvas: `before` is the base branch, `after` the task's worktree. */
export type PreviewSide = 'before' | 'after';

/** GET|POST /api/v1/tasks/{id}/previews. `url` is same-origin, sandboxed, token in the path. */
export interface Preview {
  side: PreviewSide;
  url: string;
}

/** Design-canvas pin: open → queued (batched) → in_progress (sent) → done. */
export type PinStatus = 'open' | 'queued' | 'in_progress' | 'done';

/** The element a pin points at, as the preview bridge reported it (page CSS pixels). */
export interface PinTarget {
  selector: string;
  tag: string;
  text: string;
  /** Breakpoint it was picked at. */
  width: number;
  rect: { x: number; y: number; width: number; height: number };
  styles: Record<string, string>;
  source: { file: string; line: number; column: number } | null;
  /** The page under the preview base, e.g. `/pricing` ('' = `/`). */
  path?: string;
}

/** A value the user changed on the canvas: one line of a suggested diff. */
export interface PinEdit {
  property: string;
  from: string;
  to: string;
}

/** GET|POST /api/v1/tasks/{id}/pins, PATCH|DELETE …/pins/{pin} */
export interface Pin {
  id: string;
  task_id: string;
  status: PinStatus;
  comment: string;
  target: PinTarget;
  edits: PinEdit[];
  /** Blob hash of the element's crop when pinned; fetch it with `api.crop_url`. */
  before_crop: string | null;
  /** Blob hash of the same box once the pin is done. */
  after_crop: string | null;
  created_at: string;
  updated_at: string;
}

/** POST /api/v1/tasks/{id}/pins/{pin}/commit */
export interface PinCommit {
  sha: string;
  pin: Pin;
}

/** A design file from the task's repo: `DESIGN.md` or a token file. */
export interface DesignFile {
  path: string;
  text: string;
}

/** GET /api/v1/tasks/{id}/design: the `.sun-preview` screens and the repo's design files. */
export interface Design {
  screens: { name: string; path: string }[];
  files: DesignFile[];
}

/** POST /api/v1/tasks/{id}/screens: one attached screenshot. */
export interface Screen {
  name: string;
  blob: string;
  side: PreviewSide;
  path: string;
  width: number;
}

/** GET /api/v1/tasks/{id} */
export interface TaskDetail extends TaskSummary {
  gates: GateResult[];
  artifacts: Artifact[];
}

/** The repos the task's latest run works in; none for a plain task, a queued one or an agent's folder. */
export const run_repos = (t: TaskSummary) => t.run?.repo_ids ?? [];

/** `GET /workspaces/:id/repos` — a repository of the workspace. Agent tasks list them in their brief;
 *  a workflow run picks the ones the task text names. */
export interface Repo {
  id: Ulid;
  workspace_id: Ulid;
  url: string;
  default_branch: string;
  /** What agents should know it for, e.g. "Go backend + Next.js frontend". */
  description: string;
}

export type VcsProvider = 'github' | 'gitlab';

/** `GET /workspaces/:id/vcs`: how a workspace reaches a git host. Never carries a token or secret. */
export interface VcsConnection {
  id: Ulid;
  workspace_id: Ulid;
  provider: VcsProvider;
  /** `https://github.com`, `https://gitlab.com` or a self-hosted GitLab. */
  instance_url: string;
  /** The org or user it is connected to. */
  account_login: string;
  /** GitHub only. */
  installation_id: number | null;
  /** Audit actor, e.g. `user:ann`. */
  connected_by: string;
  created_at: string;
}

/** `GET /workspaces/:id/vcs`: the connections, which git apps the operator configured (the kernel says;
 *  never a value), and whether the signed-in person may connect and disconnect. */
export interface VcsList {
  connections: VcsConnection[];
  apps: { github: boolean; gitlab_oauth: boolean };
  can_manage: boolean;
}

/** `GET /workspaces/:id/vcs/{github,gitlab}/connect`: the host's page to open in a new tab; `url` is
 *  null and `configured` false when the operator hasn't set the app's secrets. */
export interface VcsConnect {
  url: string | null;
  configured: boolean;
}

/** `GET /workspaces/:id/vcs/:connection_id/repos`: a repo the connection reaches, for the picker. */
export interface RemoteRepo {
  /** owner/name (group/sub/name on GitLab) */
  name: string;
  /** HTTPS clone URL: what `add_repo` takes. */
  url: string;
  web_url: string;
  default_branch: string;
  description: string;
  private: boolean;
}

/** `POST /vcs/connections/:id/rotate-webhook`. */
export interface WebhookRotation {
  /** Hooks updated with the new secret. */
  updated: number;
}

export type PrState = 'open' | 'closed' | 'merged' | 'draft';

/** How a PR got onto the task: a person (`manual`), its title (`title`), its branch (`branch`), or a
 *  closing keyword in its body (`auto`). */
export type LinkSource = 'manual' | 'title' | 'branch' | 'auto';

/** A pull request (or GitLab MR) mirrored from a webhook, as `GET /tasks/{id}/pull_requests` lists it.
 *  The snapshot fields (`mergeable`…`checks_pending`) come from the host's API and are current only
 *  while `snapshot_available`; every rule in `lib/pull_request.ts` treats an absent one as unknown. */
export interface PullRequest {
  id: Ulid;
  provider: VcsProvider;
  repo_owner: string;
  repo_name: string;
  number: number;
  title: string;
  state: PrState;
  html_url: string;
  branch: string | null;
  author_login: string | null;
  author_avatar_url: string | null;
  merged_at: Timestamp | null;
  closed_at: Timestamp | null;
  pr_created_at: Timestamp;
  pr_updated_at: Timestamp;
  additions: number;
  deletions: number;
  changed_files: number;
  /** GitHub's `mergeable`: `mergeable`, `conflicting` or `unknown`. */
  mergeable: string | null;
  /** GitHub's `mergeStateStatus`: `clean`, `dirty`, `blocked`, `behind`, `unstable`, `has_hooks`, `draft`, `unknown`. */
  merge_state_status: string | null;
  /** The checks' rollup: `success`, `failure`, `error`, `pending`, `expected`; null without checks. */
  checks_rollup_state: string | null;
  checks_total: number;
  checks_passed: number;
  checks_failed: number;
  checks_pending: number;
  snapshot_available: boolean;
  link_source: LinkSource;
}

/** What merging the linked PRs does to this task, decided by the kernel:
 *  `none` (no PR), `terminal` (done or cancelled), `workspace_disabled`, `task_disabled`, `at_target`,
 *  `waiting` (some open or draft), `not_merged` (some closed without merging), `all_merged`. */
export interface PrAutoComplete {
  state: 'none' | 'terminal' | 'workspace_disabled' | 'task_disabled' | 'at_target' | 'waiting' | 'not_merged' | 'all_merged';
  /** The PRs the state is about: still open for `waiting`, closed for `not_merged`, all for `all_merged`. */
  pull_request_ids: Ulid[];
  task_disabled: boolean;
  workspace_enabled: boolean;
  /** Where a merge moves the task: `done` for a workflow task, `needs_you` for an agent's. */
  target_status: TaskStatus;
}

/** `GET|POST /tasks/{id}/pull_requests`, `DELETE …/{pr_id}`, `PUT /tasks/{id}/pr_auto_complete`: all answer this. */
export interface TaskPullRequests {
  pull_requests: PullRequest[];
  auto_complete: PrAutoComplete;
}

/** One row of the append-only event log (the ledger). GET /api/v1/runs/{id}/events?after=seq */
export interface RunEvent {
  seq: number;
  run_id: Ulid;
  step_key: string | null;
  /** e.g. created, dispatched, plan, tool_call, command, file_change, gate_result,
   *  runtime_switch, intervention, approval, artifact, watchdog, done */
  kind: string;
  payload: {
    /** One plain-language line, always visible */
    summary: string;
    /** Tool output, plans, diffs — collapsed by default */
    detail?: string;
    /** The rest of the stored payload, by kind (e.g. a `note`'s `note_id` and `kind`). */
    [key: string]: unknown;
  };
  at: Timestamp;
}

export type RuntimeSource = 'byo_runtime' | 'byo_key' | 'credits';
/** What a runtime is doing, derived with precedence offline > unsupported > needs_login > limited > busy > idle.
 *  Public API vocabulary (TRD §6/§7, a one-way door): don't add, rename or alias a value. */
export type RuntimeState = 'offline' | 'unsupported' | 'needs_login' | 'limited' | 'busy' | 'idle';
/** `outdated` and `auth_unknown` are badges on idle/busy, not states. */
export type RuntimeAvailability = 'ready' | 'needs_login' | 'limited' | 'outdated' | 'unsupported' | 'auth_unknown';

/** GET /api/v1/agents: an ecosystem-wide agent. A task assigned to it runs a hidden one-step workflow
 *  with its instructions, runtime chain and skills on the task's workspace. */
export interface Agent {
  id: Ulid;
  /** The workspace it belongs to; its tasks are that workspace's. */
  workspace_id: Ulid;
  name: string;
  description: string;
  instructions: string;
  /** Runtime ids in failover order */
  runtime_chain: string[];
  /** Catalog skill ids */
  skill_ids: string[];
  created_at: Timestamp;
  archived_at: Timestamp | null;
  /** Some runtime of the chain is logged in on an online host */
  ready: boolean;
  /** Why not, per runtime; `null` when ready */
  ready_reason: string | null;
  /** Its tasks that aren't Done or Cancelled */
  task_count: number;
}

/** POST /agents (name and runtime_chain required), PATCH /agents/{id} (any). `manage_runtimes`. */
export interface AgentFields {
  name?: string;
  description?: string;
  instructions?: string;
  runtime_chain?: string[];
  skill_ids?: string[];
}

/** GET /api/v1/runtimes: one per (host, tool), then the workspace's key-backed runtimes (`host: null`). */
export interface Runtime {
  /** `<host id>:<tool>` for a host's CLI; the name for a key-backed one */
  id: string;
  /** What chains name: the tool id for a host's CLI */
  name: string;
  tool: string;
  /** Same as `tool` (kept for older screens) */
  adapter: string;
  family: string | null;
  version: string | null;
  /** The host that reported the CLI; `null` for a key-backed runtime (any online host with its CLI) */
  host: { id: Ulid; name: string } | null;
  presence: 'online' | 'offline';
  availability: RuntimeAvailability;
  /** Live sessions; `max` is `null` until hosts report their cap. `null` for a key-backed runtime. */
  load: { active: number; max: number | null } | null;
  state: RuntimeState;
  /** What to do about the state, e.g. "Run `claude auth login` on vps-sg-1" */
  detail: string | null;
  cooldown_until: Timestamp | null;
  probed_at: Timestamp | null;
  last_seen_at: Timestamp | null;
  /** The workspace whose chains it serves; `null` for a host's CLI (every workspace's). */
  workspace_id: Ulid | null;
  source: RuntimeSource;
  /** Sent with every new session of this tool, on every host; `null` = the host's sun.yaml default */
  permission_mode: PermissionMode | null;
  /** The operator allows `bypass_permissions` (sun.yaml `runtimes.allow_bypass`) */
  allow_bypass: boolean;
  /** A stored `byo_key` runtime's source; `null` for the others */
  config: ByoKeyConfig | null;
  /** A `credits` runtime's gateway model (OpenCode only); `null` for the others */
  model: string | null;
}

/** Claude Code's permission modes; every adapter maps them. */
export const PERMISSION_MODES = ['default', 'accept_edits', 'plan', 'bypass_permissions'] as const;
export type PermissionMode = (typeof PERMISSION_MODES)[number];

/** GET /api/v1/hosts */
export interface Host {
  id: Ulid;
  name: string;
  status: 'online' | 'offline';
  capabilities: {
    os: string;
    arch: string;
    runtimes: string[];
    /** Bootable simulators/emulators of the ready device tools */
    devices: string[];
    device_tools?: Record<'ios_simulator' | 'android_emulator' | 'maestro', DeviceTool>;
    exec_envs?: ExecEnvCapability[];
  };
  sessions: number;
  /** Live sessions, largest RSS first */
  session_stats?: SessionStats[];
  /** Bytes per task folder on the host */
  disk?: { task_id: Ulid; bytes: number }[];
  last_seen_at: Timestamp;
  /** The workspaces it is attached to (those the caller sees); their runs may go to it. */
  workspace_ids: Ulid[];
  /** Who minted the token it joined with; `null` from before, or the kernel's own host. */
  enrolled_by: Ulid | null;
}

/** One device tool a host detected; `detail` says what's missing and the fix. */
export interface DeviceTool {
  ready: boolean;
  version: string | null;
  devices: string[];
  detail: string | null;
}

/**
 * A live agent session's resources. `run_id`/`step_key` are null after a kernel restart.
 * `cpu_percent` is the lifetime average (CPU seconds over seconds alive), as `ps %cpu`.
 */
export interface SessionStats {
  session_id: Ulid;
  run_id: Ulid | null;
  step_key: string | null;
  rss_bytes: number;
  cpu_percent: number;
  uptime_s: number;
}

/** GET /api/v1/kernel: the kernel process alone; `cpu_percent` is its lifetime average. */
export interface KernelUse {
  pid: number;
  rss_bytes: number | null;
  cpu_percent: number | null;
  uptime_s: number;
}

export type ModKind = 'plugin' | 'extension' | 'mcp';

/** GET /api/v1/mods: installed mods, then the mod registry's (`catalog/mods.json`), not installed yet */
export interface ModInstall {
  name: string;
  kind: ModKind;
  version: string;
  installed: boolean;
  /** `network:host`, `secret:NAME`, …: granted when installed, asked for when available */
  permissions: string[];
  /** Verified against a trusted key at install; unsigned = a private mod */
  signed: boolean;
  /** Available ones only: `official` (Madlabs) or `community` (not reviewed by Madlabs) */
  tier?: 'official' | 'community';
  /** Available ones only: its GitHub repo, where the release assets live */
  repo?: string;
  /** Available ones only: what it does, one sentence */
  description?: string;
  /** Installed only: the installed extensions that declare this plugin in `permissions.plugins` */
  needed_by?: string[];
  /** Installed only: a different installable version (PUT /mods/{name}), with what it asks for */
  update?: { version: string; permissions: string[] } | null;
}

/** One entry of GET /api/v1/catalog: a skill or MCP server. Templates are GET /templates. */
export interface CatalogEntry {
  /** Also the skill folder or MCP server name */
  id: string;
  kind: 'skill' | 'mcp';
  description: string;
  source: string;
  author: string;
  license: string;
  /** Catalog (pinned) version */
  version: string;
  stacks: string[];
  /** MCP: tool names */
  tools?: string[];
  /** On the curated list */
  reviewed: boolean;
  /** Installed version; older than `version` means an update is on offer */
  installed: string | null;
  /** The repo's stacks this entry fits */
  matches: string[];
  /** A saved workflow names it (agent step `skills:` or an `mcp:` grant); ranks first */
  in_workflows: boolean;
  /** MCP: network hosts it reaches */
  hosts: string[];
  /** MCP: secret names it needs */
  secrets: string[];
  /** Skill: the runtime adapter ids that get it; null = every runtime */
  runtimes?: string[] | null;
}

/** GET /api/v1/catalog?repo=&repo_id= */
export interface CatalogView {
  /** Stacks detected in the repo folder */
  stacks: string[];
  entries: CatalogEntry[];
  /** Suggestions the ecosystem dismissed (PUT/DELETE /catalog/:id/dismissed) */
  dismissed: string[];
  /** With `repo_id`: the skills installed into that repo by PR */
  repo_installs?: SkillPr[];
}

/** GET /api/v1/catalog/:id/files */
export interface CatalogFile {
  path: string;
  /** The catalog's text; empty for a `removed` file */
  text: string;
  /** Against the installed copy (diff on update); `null` when not installed */
  change: 'added' | 'changed' | 'unchanged' | 'removed' | null;
  /** A `changed` file's line diff, installed → catalog; each line starts with ' ', '-' or '+' */
  diff?: string[];
}

/** A skill installed into a repo by PR: POST /catalog/:id/install {repo_id},
 *  GET /catalog/:id/repos/:repo_id. */
export interface SkillPr {
  repo_id: Ulid;
  skill: string;
  /** The catalog version the PR carries */
  version: string;
  /** `sun/skill-<id>` */
  branch: string;
  /** The done task on the repo the PR also shows on */
  task_id: Ulid;
  /** The `open_pr` delivery; `null` until the branch is pushed */
  pr: MemoryItem['pr'];
}

/** GET /api/v1/workflows */
export type Workflow = WorkflowRef;

/** POST /api/v1/tasks */
export interface CreateTask {
  title: string;
  body?: string;
  /** Required without a workflow (or with `default`); else the workflow's */
  workspace_id?: Ulid;
  /** A workflow, `default` (the workspace's), or absent: a plain task that never runs */
  workflow_id?: Ulid | 'default';
  /** Overrides the workflow's `exec_env:`; omit for the workflow's */
  exec_env?: ExecEnvSpec | null;
  /** Default `none` */
  priority?: Priority;
  /** Default `todo`; only a `todo` task with a workflow starts a run */
  status?: 'backlog' | 'todo';
  project_id?: Ulid;
  /** Instead of `workflow_id`; an agent needs `workspace_id` */
  assignee?: Assignee | null;
}

/** Where agent sessions run: the host must report the kind ready */
export type ExecEnvSpec =
  | { kind: 'bare' }
  | { kind: 'container'; image: string; memory_mb?: number | null; cpu_millis?: number | null; network?: string | null }
  | { kind: 'vm'; template: string; memory_mb?: number | null; cpus?: number | null };
export type ExecEnvKind = ExecEnvSpec['kind'];

/** One exec env kind a host reported in `hello` */
export interface ExecEnvCapability {
  kind: ExecEnvKind;
  ready: boolean;
  version: string | null;
  /** When not ready: what is missing and the fix */
  detail: string | null;
}

/** GET /api/v1/usage?run_id= — summed `usage` events */
/** A stale or conflicting knowledge source on a task. */
export interface KnowledgeFlag {
  kind: 'stale' | 'conflict';
  message: string;
  /** `title > heading · version · date · url`, one per doc involved. */
  citations: string[];
  /** `doc:<id>`, same order as `citations`. */
  pointers: string[];
}

/** GET /api/v1/tasks/{id}/corrections — a human correction of agent work, labelled by cause. */
export interface Correction {
  id: Ulid;
  task_id: Ulid;
  run_id: Ulid;
  workflow_id: Ulid;
  step_key: string | null;
  /** Runtime of the run's latest agent attempt. */
  runtime: string | null;
  kind: 'gate_fail' | 'reject' | 'revert' | 'steer' | 'decision_override' | 'review_comment';
  cause_label: 'test_failure' | 'lint' | 'missing_requirement' | 'wrong_scope' | 'style' | 'other';
  /** `rules`, or the classifier model that relabelled it. */
  classified_by: string;
  body: string;
  created_at: Timestamp;
}

/** A changed file the `data_touched` gate matched, or a database, table or API the agents'
 *  tool calls used (`path` is then its name); best effort, "detected". */
export interface DataTouch {
  path: string;
  rule: 'migration' | 'schema' | 'seed_data' | 'sql' | 'orm_model' | 'data_config' | 'custom' | 'database' | 'table' | 'api';
}

/** How a task's merged PR held up afterwards. */
export interface TaskQuality {
  task_id: Ulid;
  mod_name: string;
  repo: string;
  pr_number: number | null;
  base: string;
  merge_sha: string;
  merged_at: Timestamp;
  window_days: number;
  ci_state: 'success' | 'failure' | 'pending' | 'none' | null;
  reverted_by: string | null;
  fix_commits: { sha: string; subject: string; at: Timestamp }[];
  bug_issues: BugIssue[];
  checked_at: Timestamp | null;
}

/** A bug issue linked to a merged PR: the one that started the task, one its text
 *  links, or a bug reported against the merged code afterwards. */
export interface BugIssue {
  relation: 'origin' | 'referenced' | 'regression';
  /** `owner/repo#12` or a tracker key like `APP-4`. */
  key: string;
  title: string | null;
  url: string | null;
  opened_at: Timestamp | null;
}

/** GET /api/v1/tasks/{id}/quality */
export interface TaskQualityView {
  data_touched: DataTouch[];
  quality: TaskQuality | null;
  held_up: boolean | null;
}

/** GET /api/v1/quality/metrics */
export interface QualityMetrics {
  merged: number;
  decided: number;
  held_up: number;
  ci_failed: number;
  reverted: number;
  with_fixes: number;
  with_bugs: number;
  held_up_rate: number | null;
}

export interface Usage {
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
}

/** GET /api/v1/usage?by=runtime. `quota_window`: subscription runtimes only. */
export interface RuntimeUsage {
  runtime: string;
  source: RuntimeSource;
  usage: Usage;
  quota_window: { hours: number; usage: Usage } | null;
}

/** GET /api/v1/features — built-in features this kernel runs (sun.yaml `features:`). */
export interface Features {
  knowledge: boolean;
  canvas: boolean;
  evals: boolean;
  market: boolean;
  catalog: boolean;
  /** Plans per workspace through Sun Cloud: off without it. */
  billing: boolean;
}

/** POST /api/v1/workspaces/{id}/hosts/tokens — single-use enrolment token for `sun host join` */
export interface HostToken {
  token: string;
  expires_at: Timestamp;
  /** The kernel's public URL: `sun host join --kernel` */
  kernel_url: string;
}

/** GET /api/v1/workflows/{id} — the latest version with its file (the builder loads this) */
export interface WorkflowSource extends WorkflowRef {
  source_yaml: string;
}

/** One problem in a workflow file, 1-based (`line:column: message`) */
export interface WorkflowIssue {
  line: number;
  column: number;
  message: string;
}

/** POST /api/v1/workflows/validate — empty `errors` when the file is valid */
export interface WorkflowCheck {
  errors: WorkflowIssue[];
}

/** What a runtime limit does; `null` = the next scope's policy (task → workflow → ecosystem → `switch`). */
export type OnLimit = 'switch' | 'pause_and_notify' | 'wait_until_reset';

/** GET /api/v1/templates — a role template and the mods it needs */
export interface WorkflowTemplate {
  id: string;
  role: 'engineering' | 'marketing' | 'product' | 'leadership' | 'forward-deployed';
  title: string;
  description: string;
  mods: string[];
  source_yaml: string;
}

/** Where knowledge comes from: uploads, a folder on the kernel's machine, or a connector mod that syncs. */
export type KnowledgeKind = 'upload' | 'repo' | 'gdrive' | 'confluence' | 'jira' | 'slack';

/** The source's one parameter, plus how old a doc may get before it is flagged stale. */
export interface SourceSettings {
  folder?: string;
  space?: string;
  project?: string;
  channel?: string;
  /** `repo`: an absolute folder on the kernel's machine. */
  path?: string;
  max_age_days?: number;
  /** Connectors only: every synced page is also POSTed here (https, or http to localhost). */
  export_webhook?: string;
}

/** What a sync or upload did. `skipped` = `name: reason` per doc not indexed. */
export interface SyncReport {
  indexed: number;
  unchanged: number;
  skipped: string[];
  retired: number;
}

/** GET /api/v1/knowledge/sources */
export interface KnowledgeSource {
  id: string;
  kind: KnowledgeKind;
  name: string;
  settings: SourceSettings;
  cursor: string | null;
  last_sync: { at: Timestamp; report?: SyncReport; error: string | null } | null;
  docs: number;
  chunks: number;
  last_indexed_at: Timestamp | null;
}

/** POST /api/v1/knowledge/sources */
export interface NewSource {
  kind: KnowledgeKind;
  name: string;
  settings: SourceSettings;
  /** The workspace it goes on; default Main. */
  workspace_id?: Ulid;
}

/** GET /api/v1/knowledge/sources/{id}/docs */
export interface KnowledgeDoc {
  id: string;
  source_id: string;
  external_id: string;
  title: string;
  version: string;
  url: string | null;
  updated_at: Timestamp | null;
  fetched_at: Timestamp;
  chunks: number;
}

/** One doc in POST /api/v1/knowledge/sources/{id}/docs */
export interface UploadDoc {
  external_id: string;
  title: string;
  version: string;
  updated_at?: Timestamp;
  content: { markdown: string };
}

/** One binary file (.xlsx, .docx, .pdf, ≤ 8 MB) in POST /api/v1/knowledge/sources/{id}/docs */
export interface UploadFile {
  external_id: string;
  title: string;
  version: string;
  updated_at?: Timestamp;
  content_base64: string;
}

/** GET /api/v1/knowledge/docs/{id} */
export interface DocDetail {
  doc: KnowledgeDoc;
  chunks: { heading_path: string; body: string; token_estimate: number; citation: string }[];
}

/** GET /api/v1/knowledge/search, best first */
export interface SearchHit {
  citation: string;
  text: string;
  /** `doc:<id>` */
  pointer: string;
  doc_id: string;
  flags: KnowledgeFlag[];
}

export type EmbeddingProvider = 'none' | 'openai' | 'ollama';

/** GET|PUT /api/v1/settings/embeddings. The key is write-only: PUT `api_key` sets it, `''` clears it. */
export interface Embeddings {
  provider: EmbeddingProvider;
  base_url: string;
  model: string;
  /** OpenAI only */
  dimensions: number | null;
  has_api_key: boolean;
}

/** GET|PUT /api/v1/tasks/{id}/context. `sources: null` = every source. */
export interface TaskContext {
  sources: string[] | null;
  budget_tokens: number | null;
  /** Pinned constraints: in every pack and envelope, so they survive context rotation and hand-offs. */
  pinned: string[];
  /** `true`: a step whose pack doesn't fit the budget doesn't start; `false`: trim to pointers. */
  block_over_budget: boolean;
}

/** POST /api/v1/tasks/{id}/context/preview. Priority 4 = knowledge docs; `included: false` = a pointer only. */
export interface ContextPreview {
  summary: string;
  budget_tokens: number;
  token_estimate: number;
  items: { priority: number; label: string; pointer: string; tokens: number; included: boolean }[];
  /** Sources with docs among the task's top hits, best first; `selected` = in the selection. */
  suggested_sources: { source_id: string; name: string; score: number; docs: number; selected: boolean }[];
  /** `null` until a runtime reported a cost. */
  cost_estimate_usd: number | null;
  /** Why block mode would stop the run. */
  blocked: string | null;
}

/** GET /api/v1/memory — long-term memory and its proposals. */
export interface MemoryItem {
  id: Ulid;
  /** The workspace it was learned in; only its runs get it. */
  workspace_id: Ulid;
  scope: 'ecosystem' | 'repo' | 'workflow';
  /** Repo or workflow id; null for the ecosystem. */
  scope_id: string | null;
  /** The repo's `owner/name` or the workflow's name. */
  scope_name: string | null;
  /** The instruction, as it goes into context packs and the PR. */
  body: string;
  /** The repo file the PR edits (AGENTS.md, CLAUDE.md, a skill or a workflow prompt); null: the repo's agent instructions file. */
  target: string | null;
  /** Why it was proposed. */
  reason: string;
  source_run: Ulid | null;
  status: 'proposed' | 'active' | 'retired';
  /** `sun` (a repeated correction) or `user:<name>` (a star). */
  author: string;
  reviewed_by: string | null;
  created_at: Timestamp;
  /** The proposal's PR to `target`, once enqueued (none for a repo-less task). */
  pr: { status: 'pending' | 'delivered' | 'unknown' | 'failed'; url: string | null } | null;
}

/** Corrections and how many repeated a cause seen before in the same workflow. */
export interface RepeatRate {
  corrections: number;
  repeats: number;
  rate: number | null;
}

/** GET /api/v1/memory/metrics — the repeat-mistake rate. */
export interface MemoryMetrics {
  overall: RepeatRate;
  /** 8 weeks, oldest first. */
  weeks: (RepeatRate & { week_start: Timestamp })[];
  by_workflow: (RepeatRate & { name: string })[];
  by_runtime: (RepeatRate & { name: string })[];
  active_items: number;
  proposed_items: number;
  /** Do gate steps pass their first check? */
  first_pass: {
    overall: PassRate;
    weeks: (PassRate & { week_start: Timestamp })[];
    by_workflow: (PassRate & { name: string })[];
    /** By the runtime whose work the first check checked (`none`: no agent ran). */
    by_runtime: (PassRate & { name: string })[];
  };
  /** Review rejections (`reject` corrections): `corrections` counts them. */
  review_rejections: {
    overall: RepeatRate;
    weeks: (RepeatRate & { week_start: Timestamp })[];
    by_workflow: (RepeatRate & { name: string })[];
    by_runtime: (RepeatRate & { name: string })[];
  };
}

/** Gate steps and how many passed their first check. */
export interface PassRate {
  gate_steps: number;
  first_pass: number;
  rate: number | null;
}

/** POST /api/v1/workflows/dry-run: nothing is saved or started. */
export interface DryRun {
  steps: { id: string; kind: StepType; wave: number; uses: string[] }[];
  /** `runtime.chain`; empty = the ecosystem default runtime. */
  runtimes: string[];
  mods: { name: string; installed: boolean }[];
  agent_steps: number;
  /** Agent steps × the average past session cost; null before any usage. */
  estimated_cost_usd: number | null;
  budget_usd: number | null;
  /** The steps actually run in a sandbox with a scripted agent; nothing is sent. */
  simulation: {
    /** `succeeded`, `failed` or `cancelled`. */
    status: string;
    end_reason: string | null;
    /** Every step in file order; `not_reached` when the run ended before it. */
    steps: { id: string; status: string; attempts: number; note: string | null }[];
    /** Outbox actions recorded, not sent (`github.open_pr`). */
    actions: string[];
  };
}

/** GET /api/v1/marketplace/listings: runtime capacity sold on Sun Cloud. */
export interface MarketListing {
  id: string;
  offering: 'machine' | 'open_model' | 'api' | 'subscription';
  runtime: string;
  /** Always disclosed. */
  model: string;
  unit: MarketUnit;
  /** Per hour, per task, or per whole contract. */
  price_points: number;
  concurrency: number;
  region: string;
  /** Set when the listing resells a consumer subscription. */
  warning: string | null;
  /** e.g. `gpu`, `ios_simulator`, `kvm`. */
  capabilities: string[];
  /** Average buyer rating, 1–5; null before the first one. */
  rating: number | null;
  rating_count: number;
  /** Length of a `contract` lease. */
  contract_hours: number | null;
  /** The seller proved it owns the host (its host key signed the listing). */
  host_verified: boolean;
  /** The seller's own listings only (`/marketplace/listings/mine`). */
  status?: 'active' | 'paused';
}

export type MarketUnit = 'hour' | 'task' | 'contract';

/** Query of GET /api/v1/marketplace/listings. */
export interface ListingFilter {
  region?: string;
  offering?: string;
  capability?: string;
  min_rating?: number;
}

/** GET|PUT /api/v1/marketplace/policy: which workflows may use marketplace runtimes. */
export interface MarketPolicy {
  allow: 'all' | 'none' | 'workflows';
  workflows: string[];
  /** Most a `market:` chain entry pays per task; unset = no automatic failover. */
  failover_max_price_points?: number | null;
}

/** GET /api/v1/marketplace/payouts: the seller's earnings. */
export interface MarketPayouts {
  /** Earned points that can be paid out now. */
  payable: number;
  payouts: MarketPayout[];
}

export interface MarketPayout {
  id: string;
  points: number;
  currency: 'usd' | 'idr';
  amount: number;
  destination: string;
  created_at: Timestamp;
  sent_at: Timestamp | null;
}

export type Escrow = 'held' | 'disputed' | 'released' | 'refunded';

/** GET /api/v1/marketplace/leases: one task on one listing, as buyer or seller. */
export interface MarketLease {
  id: string;
  listing_id: string;
  buyer_ecosystem: string;
  seller_ecosystem: string;
  /** The buyer's task id. */
  task_ref: string;
  unit: MarketUnit;
  price_points: number;
  status: 'open' | 'closed';
  started_at: Timestamp;
  closed_at: Timestamp | null;
  close_reason: string | null;
  escrow: Escrow;
  /** A held, closed lease goes to the seller at this time unless disputed. */
  release_at: Timestamp | null;
  dispute: string | null;
  /** The buyer's stars for it, once rated. */
  rating?: number | null;
}

/** POST /api/v1/marketplace/leases. */
export interface NewLease {
  task_id: string;
  runtime: string;
  unit: MarketUnit;
  max_price_points: number;
  model?: string;
  region?: string;
  /** The buyer read that the seller's machine sees the task's code and data. */
  accept_disclosure: boolean;
  /** Ecosystem secrets allowed on the seller's machine, by name; the rest stay home. */
  secrets?: string[];
}

/** A `device_result` event's payload: the verify's outcome, or why it couldn't run. */
export interface DeviceResultPayload {
  summary: string;
  device?: string;
  passed?: boolean;
  output?: string;
  screenshots?: { name: string; blob: string }[];
  error?: string;
}

/** Which past tasks an eval replays. */
export type EvalPick =
  | { by: 'workflow'; workflow_id: string }
  | { by: 'cause'; cause: Correction['cause_label'] }
  | { by: 'set'; set_id: string }
  | { by: 'tasks'; task_ids: string[] };

/** POST /api/v1/evals. No `runtime` replays the recorded transcripts. */
export interface EvalSpec {
  name: string;
  pick: EvalPick;
  candidate?: { workflow?: string | null; memory?: string[] };
  runtime?: string | null;
  limit?: number | null;
}

/** How one run of a task went; `actions` are the outbox calls a replay recorded. */
export interface EvalOutcome {
  run_id: string;
  status: string;
  passed: boolean;
  end_reason: string | null;
  causes: Correction['cause_label'][];
  repeats: number;
  actions: string[];
}

export interface EvalSide {
  passed: number;
  pass_rate: number;
  mistakes: number;
  repeats: number;
}

/** The side-by-side result of an eval. */
export interface EvalReport {
  runtime: string;
  replayed: number;
  tasks: { task_id: string; title: string; original: EvalOutcome; replay: EvalOutcome | null; error: string | null }[];
  original: EvalSide;
  replay: EvalSide;
  pass_delta: number;
  repeat_delta: number;
}

/** GET /api/v1/evals — one replay of past tasks. */
export interface EvalRun {
  id: string;
  name: string;
  spec: EvalSpec;
  status: 'running' | 'done' | 'failed';
  report: EvalReport | null;
  error: string | null;
  created_at: Timestamp;
  finished_at: Timestamp | null;
}

/** GET /api/v1/eval_sets — past tasks saved to replay later. */
export interface EvalSet {
  id: string;
  name: string;
  task_ids: string[];
  created_at: Timestamp;
}

/** Error body for every non-2xx response */
export interface ApiError {
  error: { code: string; message: string };
}

/** How to reach an MCP server: streamable HTTP, or a stdio command Sun runs sandboxed. Secrets are names. */
export type McpTransport =
  | { type: 'url'; url: string; bearer_secret?: string }
  | { type: 'command'; command: string; args: string[]; network: string[]; secrets: string[] };

/** `GET /mcp_servers` row: a registered MCP server (catalog install or custom). */
export interface McpServer {
  name: string;
  transport: McpTransport;
  /** A custom server's workspace (absent: Main's, or a catalog install). */
  workspace_id?: Ulid;
}

/** One tool an MCP server offers. */
export interface McpTool {
  name: string;
  description: string;
  /** The server marks it `readOnlyHint: true` */
  read_only: boolean;
}

/** `POST /mcp_servers` (admin): the server as saved, with the tools Sun listed when it connected. */
export interface McpServerAdded {
  server: McpServer;
  tools: McpTool[];
}

/** `GET`/`PUT /tasks/:id/mcp`: a server the task's agents may use. No `tools` = its read-only tools only. */
export interface McpGrant {
  server: string;
  tools?: string[];
}

/** The provider API a BYO key speaks. */
export type ByoKeyKind = 'anthropic' | 'google' | 'openai_compatible';

/** Which CLI can run each kind: OpenCode any, Claude Code anthropic, Codex openai_compatible. */
export const BYO_KEY_ADAPTERS: Record<ByoKeyKind, ByoKeyAdapter[]> = {
  openai_compatible: ['opencode', 'codex'],
  anthropic: ['opencode', 'claude-code'],
  google: ['opencode'],
};

export type ByoKeyAdapter = 'opencode' | 'claude-code' | 'codex';

/** A `byo_key` runtime's source: a provider kind and endpoint (empty = the provider's own; required
 *  for `openai_compatible`), a model and the *name* of the ecosystem secret holding the key (`null`
 *  for keyless endpoints such as a local Ollama). */
export interface ByoKeyConfig {
  kind: ByoKeyKind;
  endpoint: string;
  model: string;
  key_secret: string | null;
}

/** `POST /runtimes` (admin). A 422 message starts with the field to fix, e.g. `endpoint: no endpoint; …`.
 *  `credits` needs Sun Cloud on the kernel; OpenCode on credits needs a gateway `model`. */
export type CreateRuntime =
  | { name: string; adapter: ByoKeyAdapter; source: 'byo_key'; config: ByoKeyConfig }
  | { name: string; adapter: ByoKeyAdapter; source: 'credits'; model: string | null };

/** `GET|PUT /settings/budget`: the ecosystem's monthly spend cap and this month's spend. */
export interface Spend {
  /** `null`: no cap */
  monthly_usd: number | null;
  /** Cost runtimes reported since `since` */
  spent_usd: number;
  /** The 1st of this month, 00:00 UTC */
  since: Timestamp;
}

/** `GET /runtimes/{id}/readiness` */
export interface Readiness {
  installed: boolean;
  version: string | null;
  logged_in: boolean;
  /** What to fix when not ready */
  detail: string | null;
}
/** Text messages of the live device socket, GET /api/v1/tasks/{id}/device.
 *  After `live`, binary messages are `mime` images in device pixels, or H.264 Annex B access units for
 *  `video/h264` (asked for with `?video=h264`). `connecting` is client-side. */
export type DeviceState =
  | { kind: 'connecting' | 'idle' }
  | { kind: 'live'; device: string; platform: 'ios' | 'android'; input: boolean; mime: string }
  | { kind: 'unavailable'; message: string };

/** What the browser sends: a tap in device pixels, text, or a key. */
export type DeviceInput =
  | { kind: 'tap'; x: number; y: number }
  | { kind: 'text'; text: string }
  | { kind: 'key'; key: 'home' | 'back' | 'enter' | 'delete' };

/** `GET /entitlement?workspace=`: a workspace's plan and where it comes from. */
export type Plan = 'community' | 'free' | 'pro' | 'business';
/** `cloud`: Sun Cloud's policy, fresh until `valid_until`; `stale`: its last policy while it can't be reached;
 *  `unavailable`: Sun Cloud unreachable, Community until it answers; `community`: self-hosted. */
export type EntitlementStatus = 'cloud' | 'stale' | 'unavailable' | 'community';
export interface Entitlement {
  plan: Plan;
  /** A paid plan (exclusive mods). */
  paid: boolean;
  /** Caps of the plan; null = unlimited. */
  limits: Record<'seats' | 'runtimes' | 'byo_keys' | 'agents' | 'hosts', number | null>;
  status: EntitlementStatus;
  /** Set when `status` is `cloud`. */
  valid_until: string | null;
}

/** `GET /secrets`: metadata only; a value is never sent back. */
export interface SecretScope {
  kind: 'ecosystem' | 'mod' | 'workspace';
  id: string;
}
export interface Secret {
  id: string;
  scope: SecretScope;
  name: string;
  updated_at: string;
}

/** `GET /subscriptions/{workspace}/summary`: the workspace's subscription at Sun Cloud. */
export type PaidPlan = 'pro' | 'business';
export type Interval = 'month' | 'year';
/** The currencies Sun Cloud prices in. */
export type Currency = 'usd' | 'idr';
export interface SubscriptionSummary {
  entitlement: {
    plan: Plan;
    status: 'active' | 'past_due' | 'canceled' | 'none';
    /** Seats the plan includes; null without a subscription. */
    seats: number | null;
    limits: Entitlement['limits'];
    version: number;
  };
  seat_capacity: { purchased: number; used: number; reserved: number; available: number | null; overcommitted: boolean; version: number };
  /** What the cloud lets the workspace do now. */
  available_actions: { checkout: boolean; portal: boolean; purchase_seats: boolean };
  currency: string;
}
/** `GET /subscriptions/{workspace}/prices`; amounts in the currency's minor units (whole rupiah, cents). */
export interface PlanPrices {
  currency: string;
  plans: { plan: PaidPlan; interval: Interval; amount_minor: number; per_seat_minor: number }[];
}
/** A hosted payment page to send the browser to. */
export interface CheckoutSession {
  checkout_url: string;
  session_id: string;
}
/** `POST /subscriptions/{workspace}/seats/purchase-preview`: what the extra seats cost now. */
export interface SeatPreview {
  amount_minor: number;
  currency: string;
  expected_current_seats: number;
  expected_purchase_version: number | string;
}
/** `GET /support`: the signed-in person's questions to Sun. */
export interface Ticket {
  id: string;
  plan: Plan;
  subject: string;
  status: 'open' | 'answered';
  created_at: string;
  /** First-answer target; null without one. */
  respond_by: string | null;
  messages: { author: 'customer' | 'sun'; body: string; created_at: string }[];
}
