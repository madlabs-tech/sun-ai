// Typed client for /api/v1. One transport: real fetch, or the in-memory mock
// when the page has ?mock=1 or the build sets VITE_MOCK=1 (UI runs without the kernel).
import type { OnLimit, OutboxItem, Resolution,
  Agent,
  AgentFields,
  Workspace,
  WorkspaceProfileChange,
  Project,
  Account,
  Currency,
  Invoice,
  PaidPlan,
  PayMethod,
  Ticket,
  ApiError,
  CatalogFile,
  CatalogView,
  ContextPreview,
  CreateRuntime,
  CreateTask,
  DecisionAnswer,
  Design,
  DeviceInput,
  DeviceState,
  DryRun,
  DocDetail,
  Embeddings,
  Spend,
  Entitlement,
  EvalRun,
  EvalSet,
  EvalSpec,
  Secret,
  SecretScope,
  Host,
  HostToken,
  KernelUse,
  Correction,
  KnowledgeDoc,
  KnowledgeFlag,
  KnowledgeSource,
  ListingFilter,
  MarketLease,
  MarketListing,
  MarketPayout,
  MarketPayouts,
  MarketPolicy,
  NewLease,
  McpGrant,
  McpServer,
  McpServerAdded,
  McpTool,
  MemoryItem,
  SkillPr,
  MemoryMetrics,
  ModInstall,
  NewSource,
  PermissionMode,
  Pin,
  PinCommit,
  PinTarget,
  Preview,
  PreviewSide,
  QualityMetrics,
  Screen,
  Readiness,
  RunEvent,
  Runtime,
  SearchHit,
  SyncReport,
  TaskActivity,
  TaskContext,
  TaskPullRequests,
  Repo,
  RemoteRepo,
  VcsConnect,
  VcsConnection,
  VcsList,
  WebhookRotation,
  TaskDetail,
  TaskPatch,
  TaskQuality,
  TaskQualityView,
  TaskRun,
  RunDetail,
  TaskStatus,
  TaskSummary,
  UploadDoc,
  UploadFile,
  Usage,
  RuntimeUsage,
  Features,
  Workflow,
  WorkflowCheck,
  WorkflowSource,
  WorkflowTemplate,
} from './types';

const BASE = '/api/v1';

/** Ecosystem secrets (BYO keys, MCP servers): the nil ULID. */
export const ECOSYSTEM_SCOPE: SecretScope = { kind: 'ecosystem', id: '00000000000000000000000000' };

export const use_mock =
  import.meta.env.VITE_MOCK === '1' || new URLSearchParams(location.search).get('mock') === '1';

export class ApiFailure extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

/** A `Blob` body is sent raw with its own type (a logo); anything else as JSON. */
async function http(method: string, path: string, body: unknown): Promise<{ status: number; data: unknown }> {
  const raw = body instanceof Blob;
  const response = await fetch(BASE + path, {
    method,
    headers: body === undefined ? undefined : { 'content-type': raw ? body.type : 'application/json' },
    body: body === undefined ? undefined : raw ? body : JSON.stringify(body),
  });
  return { status: response.status, data: await response.json().catch(() => null) };
}

export async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const { status, data } = use_mock
    ? (await import('./mock')).mock_handle(method, path, body) : await http(method, path, body);
  if (status >= 400) {
    const error = (data as Partial<ApiError> | null)?.error;
    throw new ApiFailure(
      status,
      error?.code ?? 'http_error',
      error?.message ??
        (status === 404 ? 'This kernel can’t do that yet. Update sun.' : `The kernel answered ${status}. Check that sun is running.`),
    );
  }
  return data as T;
}

const id_path = (id: string) => encodeURIComponent(id);

/** `?a=1&b=2` of the set values, or ''. */
const query = (params: Record<string, string | undefined>) => {
  const set = Object.entries(params).filter((p): p is [string, string] => !!p[1]);
  return set.length ? '?' + new URLSearchParams(set) : '';
};

export const api = {
  /** The workspaces the signed-in person sees. */
  list_workspaces: () => request<Workspace[]>('GET', '/workspaces'),
  /** `create_workspace`: a workspace `group_ids` see (none: admins only); without `manage_groups`, only your own groups. */
  create_workspace: (name: string, group_ids: string[], slug?: string) => request<Workspace>('POST', '/workspaces', { name, group_ids, slug }),
  rename_workspace: (id: string, name: string) => request<Workspace>('PATCH', `/workspaces/${id_path(id)}`, { name }),
  /** `edit_workspace`: name, description, context and default workflow. */
  update_workspace: (id: string, change: WorkspaceProfileChange) => request<Workspace>('PATCH', `/workspaces/${id_path(id)}`, change),
  /** `edit_workspace`: the issue prefix (trimmed, uppercased, 1–10 of A-Z 0-9). */
  set_issue_prefix: (id: string, issue_prefix: string) => request<Workspace>('POST', `/workspaces/${id_path(id)}/prefix`, { issue_prefix }),
  /** `edit_workspace`: a PNG, JPEG or WebP image of at most 1 MB. */
  set_workspace_logo: (id: string, image: Blob) => request<Workspace>('PUT', `/workspaces/${id_path(id)}/logo`, image),
  remove_workspace_logo: (id: string) => request<Workspace>('DELETE', `/workspaces/${id_path(id)}/logo`),
  /** `workspace`'s projects, by name. */
  list_projects: (workspace: string) => request<Project[]>('GET', `/workspaces/${id_path(workspace)}/projects`),
  /** `run`: a project on `workspace`; names are unique there ignoring case. */
  create_project: (workspace: string, name: string, description = '') =>
    request<Project>('POST', `/workspaces/${id_path(workspace)}/projects`, { name, description }),
  update_project: (id: string, change: { name?: string; description?: string }) => request<Project>('PATCH', `/projects/${id_path(id)}`, change),
  delete_project: (id: string) => request<null>('DELETE', `/projects/${id_path(id)}`),
  /** `edit_workspace`: exactly these groups see the workspace; without `manage_groups`, change only your own groups. */
  set_workspace_groups: (id: string, group_ids: string[]) => request<Workspace>('PUT', `/workspaces/${id_path(id)}/groups`, { group_ids }),
  delete_workspace: (id: string) => request<null>('DELETE', `/workspaces/${id_path(id)}`),
  /** Tasks of `workspace` (every workspace when absent). */
  list_tasks: (workspace?: string, status?: TaskStatus) => request<TaskSummary[]>('GET', '/tasks' + query({ workspace, status })),
  get_task: (id: string) => request<TaskDetail>('GET', `/tasks/${id_path(id)}`),
  create_task: (input: CreateTask) => request<TaskSummary>('POST', '/tasks', input),
  /** Fields and moves; an illegal move is 409 with a plain sentence. */
  update_task: (id: string, patch: TaskPatch) => request<TaskSummary>('PATCH', `/tasks/${id_path(id)}`, patch),
  /** A comment of 1–10 000 characters, as the next activity entry. */
  comment_task: (id: string, body: string) => request<TaskActivity>('POST', `/tasks/${id_path(id)}/comments`, { body }),
  task_activity: (id: string, after = 0) => request<TaskActivity[]>('GET', `/tasks/${id_path(id)}/activity?after=${after}`),
  task_runs: (id: string) => request<TaskRun[]>('GET', `/tasks/${id_path(id)}/runs`),
  /** Any run of a task (not only the latest), with its steps and gates. */
  get_run: (run_id: string) => request<RunDetail>('GET', `/runs/${id_path(run_id)}`),
  /** Stop: pauses the running agents; the task goes Blocked. 409 when nothing runs. */
  interrupt_task: (id: string) => request<TaskSummary>('POST', `/tasks/${id_path(id)}/interrupt`, {}),
  /** The workspace's repositories (Settings → Code). */
  list_repos: (workspace: string) => request<Repo[]>('GET', `/workspaces/${id_path(workspace)}/repos`),
  /** `edit_workspace`: 422 "This repository is already added." for a URL already on the workspace. */
  add_repo: (workspace: string, input: { url: string; description?: string; default_branch?: string }) =>
    request<Repo>('POST', `/workspaces/${id_path(workspace)}/repos`, input),
  update_repo: (id: string, change: { description?: string; default_branch?: string }) =>
    request<Repo>('PATCH', `/repos/${id_path(id)}`, change),
  /** `edit_workspace`: deletes it (and its GitLab hook). */
  remove_repo: (workspace: string, repo_id: string) =>
    request<null>('DELETE', `/workspaces/${id_path(workspace)}/repos/${id_path(repo_id)}`),
  /** The workspace's git connections and which git apps the operator configured; never a token. */
  list_vcs: (workspace: string) => request<VcsList>('GET', `/workspaces/${id_path(workspace)}/vcs`),
  /** The GitHub App install page to open in a new tab (state-signed, 10 min). GitHub comes back to
   *  `#/settings/code?workspace=<id>&github_connected=1` (plus `return_to`), or `github_error=<reason>`. */
  connect_github: (workspace: string, return_to?: 'repositories') =>
    request<VcsConnect>('GET', `/workspaces/${id_path(workspace)}/vcs/github/connect` + query({ return_to })),
  /** Like `connect_github`, through GitLab's OAuth app: back with `gitlab_connected=1` or `gitlab_error=<reason>`. */
  connect_gitlab: (workspace: string, return_to?: 'repositories') =>
    request<VcsConnect>('GET', `/workspaces/${id_path(workspace)}/vcs/gitlab/connect` + query({ return_to })),
  /** Keeps the mirrored pull requests and their links. */
  disconnect_vcs: (id: string) => request<null>('DELETE', `/vcs/connections/${id_path(id)}`),
  /** The task's linked pull requests and what merging them does (task view). */
  task_pull_requests: (task_id: string) => request<TaskPullRequests>('GET', `/tasks/${id_path(task_id)}/pull_requests`),
  /** `edit_task`: by URL (404 until the PR's repository is connected and the PR has been mirrored) or by id (undo a removal). */
  link_pull_request: (task_id: string, pr: { url: string } | { pull_request_id: string }) =>
    request<TaskPullRequests>('POST', `/tasks/${id_path(task_id)}/pull_requests`, pr),
  /** `edit_task`: the kernel remembers the removal, so webhooks don't link it again. */
  unlink_pull_request: (task_id: string, pr_id: string) => request<TaskPullRequests>('DELETE', `/tasks/${id_path(task_id)}/pull_requests/${id_path(pr_id)}`),
  /** `edit_task`: `disabled` keeps this task's status when its PRs merge. */
  set_pr_auto_complete: (task_id: string, disabled: boolean) =>
    request<TaskPullRequests>('PUT', `/tasks/${id_path(task_id)}/pr_auto_complete`, { disabled }),
  /** GitLab: a new webhook secret, pushed to every hook Sun made. */
  rotate_vcs_webhook: (id: string) => request<WebhookRotation>('POST', `/vcs/connections/${id_path(id)}/rotate-webhook`, {}),
  /** The picker: repos the connection reaches, 50 a page from 1. */
  list_remote_repos: (workspace: string, connection_id: string, q = '', page = 1) =>
    request<RemoteRepo[]>(
      'GET',
      `/workspaces/${id_path(workspace)}/vcs/${id_path(connection_id)}/repos` + query({ q: q || undefined, page: String(page) }),
    ),
  approve_task: (id: string) => request<TaskSummary>('POST', `/tasks/${id_path(id)}/approve`, {}),
  reject_task: (id: string, reason: string) =>
    request<TaskSummary>('POST', `/tasks/${id_path(id)}/reject`, { reason }),
  /** Answer an escalated `decide` step yourself; a different answer is recorded as a correction. */
  answer_decision: (id: string, answer: DecisionAnswer) =>
    request<TaskSummary>('POST', `/tasks/${id_path(id)}/decision`, { answer }),
  cancel_task: (id: string) => request<TaskSummary>('POST', `/tasks/${id_path(id)}/cancel`, {}),
  retry_run: (run_id: string) => request<TaskSummary>('POST', `/runs/${id_path(run_id)}/retry`, {}),
  /** Retry one failed step: same worktree, finished steps kept. */
  retry_step: (run_id: string, step_key: string) =>
    request<TaskSummary>('POST', `/runs/${id_path(run_id)}/steps/${id_path(step_key)}/retry`, {}),
  /** Side effects by state; `unknown` ones wait for a person. */
  list_outbox: (status: OutboxItem['status']) => request<OutboxItem[]>('GET', `/outbox?status=${status}`),
  resolve_outbox: (id: string, resolution: Resolution) =>
    request<TaskSummary>('POST', `/outbox/${id_path(id)}/resolve`, { resolution }),
  /** Moves a running or limit-hit agent step to `to` (a runtime of the chain) with a hand-off pack. */
  switch_runtime: (run_id: string, step_key: string, to: string) =>
    request<TaskSummary>('POST', `/runs/${id_path(run_id)}/steps/${id_path(step_key)}/switch`, { to }),
  /** Steer now, or `queue` it for when the current step ends. */
  send_message: (run_id: string, body: string, queue = false) =>
    request<RunEvent>('POST', `/runs/${id_path(run_id)}/messages`, queue ? { body, queue } : { body }),
  pause_task: (id: string) => request<TaskSummary>('POST', `/tasks/${id_path(id)}/pause`, {}),
  resume_task: (id: string) => request<TaskSummary>('POST', `/tasks/${id_path(id)}/resume`, {}),
  list_events: (run_id: string, after = 0) =>
    request<RunEvent[]>('GET', `/runs/${id_path(run_id)}/events?after=${after}`),
  /** Stale or conflicting sources among the task's knowledge. */
  knowledge_flags: (id: string) => request<KnowledgeFlag[]>('GET', `/tasks/${id_path(id)}/knowledge_flags`),
  /** Gate fails, rejects, reverts and pushed-back steers on the task, oldest first. */
  corrections: (id: string) => request<Correction[]>('GET', `/tasks/${id_path(id)}/corrections`),
  /** Data the task touched (detected) and how its merged PR held up. */
  task_quality: (id: string) => request<TaskQualityView>('GET', `/tasks/${id_path(id)}/quality`),
  check_quality: (id: string) => request<TaskQuality>('POST', `/tasks/${id_path(id)}/quality/check`, {}),
  quality_metrics: () => request<QualityMetrics>('GET', '/quality/metrics'),
  /** Approve (`active`), reject or retire (`retired`); nothing is active without this. */
  review_memory: (id: string, status: 'active' | 'retired') =>
    request<MemoryItem>('POST', `/memory/${id_path(id)}/review`, { status }),
  /** Proposes a correction as memory; `created: false` when starred before. */
  star_correction: (id: string) => request<{ created: boolean }>('POST', `/corrections/${id_path(id)}/star`, {}),
  /** Eval replay of past tasks against a candidate change, newest first. */
  evals: () => request<EvalRun[]>('GET', '/evals'),
  eval_run: (id: string) => request<EvalRun>('GET', `/evals/${id_path(id)}`),
  /** Starts replaying in the background; the eval comes back `running`. */
  start_eval: (spec: EvalSpec) => request<EvalRun>('POST', '/evals', spec),
  eval_sets: () => request<EvalSet[]>('GET', '/eval_sets'),
  /** Runtime capacity on Sun Cloud's marketplace; 503 off Sun Cloud. */
  market_listings: (filter: ListingFilter = {}) => {
    const query = new URLSearchParams(
      Object.entries(filter)
        .filter(([, v]) => v !== undefined && v !== '')
        .map(([k, v]) => [k, String(v)]),
    ).toString();
    return request<MarketListing[]>('GET', `/marketplace/listings${query ? `?${query}` : ''}`);
  },
  market_leases: () => request<MarketLease[]>('GET', '/marketplace/leases'),
  /** Seller side: this ecosystem's own listings, paused ones too. */
  my_listings: () => request<MarketListing[]>('GET', '/marketplace/listings/mine'),
  set_listing_status: (id: string, status: 'active' | 'paused') =>
    request<MarketListing>('POST', `/marketplace/listings/${id_path(id)}/status`, { status }),
  market_payouts: () => request<MarketPayouts>('GET', '/marketplace/payouts'),
  request_payout: (points: number, currency: 'usd' | 'idr', destination: string) =>
    request<MarketPayout>('POST', '/marketplace/payouts', { points, currency, destination }),
  rate_lease: (id: string, stars: number, comment?: string) =>
    request<MarketLease>('POST', `/marketplace/leases/${id_path(id)}/rating`, { stars, comment }),
  market_policy: () => request<MarketPolicy>('GET', '/marketplace/policy'),
  set_market_policy: (policy: MarketPolicy) => request<MarketPolicy>('PUT', '/marketplace/policy', policy),
  /** Leases capacity for a task that hasn't started; it then runs only on the seller's host. */
  lease: (lease: NewLease) => request<MarketLease>('POST', '/marketplace/leases', lease),
  accept_lease: (id: string) => request<MarketLease>('POST', `/marketplace/leases/${id_path(id)}/accept`, {}),
  dispute_lease: (id: string, reason: string) =>
    request<MarketLease>('POST', `/marketplace/leases/${id_path(id)}/dispute`, { reason }),
  save_eval_set: (name: string, task_ids: string[]) => request<EvalSet>('POST', '/eval_sets', { name, task_ids }),
  /** Proposes a `learning` note as memory; `created: false` when starred before. */
  star_note: (id: string) => request<{ created: boolean }>('POST', `/notes/${id_path(id)}/star`, {}),
  list_previews: (id: string) => request<Preview[]>('GET', `/tasks/${id_path(id)}/previews`),
  /** Starts (or reuses) the dev server; resolves once it listens, which can take minutes. */
  start_preview: (id: string, side: PreviewSide) =>
    request<Preview>('POST', `/tasks/${id_path(id)}/previews`, { side }),
  stop_previews: (id: string) => request<null>('DELETE', `/tasks/${id_path(id)}/previews`),
  /** The `.sun-preview` screens and the repo design files, from the task branch. */
  get_design: (id: string) => request<Design>('GET', `/tasks/${id_path(id)}/design`),
  list_pins: (id: string) => request<Pin[]>('GET', `/tasks/${id_path(id)}/pins`),
  create_pin: (id: string, target: PinTarget) => request<Pin>('POST', `/tasks/${id_path(id)}/pins`, { target }),
  update_pin: (id: string, pin_id: string, change: Partial<Pick<Pin, 'status' | 'comment' | 'edits'>>) =>
    request<Pin>('PATCH', `/tasks/${id_path(id)}/pins/${id_path(pin_id)}`, change),
  delete_pin: (id: string, pin_id: string) => request<null>('DELETE', `/tasks/${id_path(id)}/pins/${id_path(pin_id)}`),
  /** "Change it myself": commits the pin's saved edit to the task branch under your name. */
  commit_pin: (id: string, pin_id: string) => request<PinCommit>('POST', `/tasks/${id_path(id)}/pins/${id_path(pin_id)}/commit`),
  /** Review evidence: before/after screenshots of the agreed screens, on the task and its PR. */
  attach_screens: (id: string, input: { paths?: string[]; widths: number[] }) =>
    request<Screen[]>('POST', `/tasks/${id_path(id)}/screens`, input),
  usage: (run_id: string) => request<Usage>('GET', `/usage?run_id=${id_path(run_id)}`),
  usage_by_runtime: () => request<RuntimeUsage[]>('GET', '/usage?by=runtime'),
  get_features: () => request<Features>('GET', '/features'),
  create_host_token: () => request<HostToken>('POST', '/hosts/tokens', {}),
  /** Workflows of `workspace` (every workspace when absent). */
  list_workflows: (workspace?: string) => request<Workflow[]>('GET', '/workflows' + query({ workspace })),
  get_workflow: (id: string) => request<WorkflowSource>('GET', `/workflows/${id_path(id)}`),
  validate_workflow: (source_yaml: string) => request<WorkflowCheck>('POST', '/workflows/validate', { source_yaml }),
  /** What would run, in which order, what it needs and may cost; saves nothing. */
  dry_run: (source_yaml: string) => request<DryRun>('POST', '/workflows/dry-run', { source_yaml }),
  /** Version 1 of a new workflow on `workspace`, or the next version of `id`, which stays on its workspace. */
  save_workflow: (id: string | null, source_yaml: string, workspace?: string) =>
    request<Workflow>('POST', id ? `/workflows/${id_path(id)}/versions` : '/workflows', id ? { source_yaml } : { source_yaml, workspace_id: workspace }),
  list_templates: () => request<WorkflowTemplate[]>('GET', '/templates'),
  install_template: (id: string, workspace?: string) => request<Workflow>('POST', `/templates/${id_path(id)}/install` + query({ workspace }), {}),
  /** `manage_runtimes`: a new version of its hidden workflows; running tasks keep theirs. */
  update_agent: (id: string, fields: AgentFields) => request<Agent>('PATCH', `/agents/${id_path(id)}`, fields),
  /** `manage_runtimes`: archives it; its tasks keep it, new tasks can't pick it. */
  archive_agent: (id: string) => request<null>('DELETE', `/agents/${id_path(id)}`),
  /** Admin: the permission mode every new session of runtime `id` gets (audited). */
  set_runtime_policy: (id: string, permission_mode: PermissionMode) =>
    request<{ id: string; permission_mode: PermissionMode }>('PUT', `/runtimes/${id_path(id)}/policy`, { permission_mode }),
  list_hosts: () => request<Host[]>('GET', '/hosts'),
  kernel_use: () => request<KernelUse>('GET', '/kernel'),
  /** Admin: stops a live session; its run pauses and records `reason`. */
  kill_session: (session_id: string, reason: string) =>
    request<{ run_id: string | null }>('POST', `/sessions/${id_path(session_id)}/kill`, { reason }),
  list_mods: () => request<ModInstall[]>('GET', '/mods'),
  /** Admin: installs a mod of the mod registry from its GitHub release and loads it. `permissions` =
   *  exactly what the user was shown and approved (422 when the package asks for something else). */
  install_mod: (name: string, permissions: string[]) =>
    request<ModInstall>('POST', `/mods/${id_path(name)}/install`, { permissions }),
  /** Admin: updates an installed mod to its listed `update`, with exactly that version's permissions. */
  update_mod: (name: string, permissions: string[]) =>
    request<ModInstall>('PUT', `/mods/${id_path(name)}`, { permissions }),
  /** Admin: stops and uninstalls it; data and secrets go too only with `purge`. */
  remove_mod: (name: string, purge: boolean) =>
    request<{ removed: string; needed_by: string[] }>('DELETE', `/mods/${id_path(name)}${purge ? '?purge=true' : ''}`),
  /** Ranked for the stack of the checkout at `repo` (an absolute path on the kernel's machine);
   *  with `repo_id`, also the skills installed into that registered repo by PR. */
  list_catalog: (repo = '', repo_id = '') => {
    const query = new URLSearchParams(Object.entries({ repo, repo_id }).filter(([, v]) => v)).toString();
    return request<CatalogView>('GET', '/catalog' + (query ? `?${query}` : ''));
  },
  /** Dismissed for the whole ecosystem, so it stays hidden in every browser. */
  dismiss_catalog: (id: string) => request<null>('PUT', `/catalog/${id_path(id)}/dismissed`),
  restore_catalog: (id: string) => request<null>('DELETE', `/catalog/${id_path(id)}/dismissed`),
  catalog_files: (id: string) => request<CatalogFile[]>('GET', `/catalog/${id_path(id)}/files`),
  /** `runtimes`: the adapter ids that get a skill; omitted = every runtime */
  install_catalog: (id: string, runtimes?: string[]) =>
    request<null>('POST', `/catalog/${id_path(id)}/install`, runtimes ? { runtimes } : {}),
  /** Commits a skill into `repo_id` on `sun/skill-<id>` and asks the git host for a PR. Idempotent. */
  install_skill_pr: (id: string, repo_id: string, runtimes?: string[]) =>
    request<SkillPr>('POST', `/catalog/${id_path(id)}/install`, runtimes ? { repo_id, runtimes } : { repo_id }),
  /** 404 when the skill was never installed into that repo. */
  skill_pr: (id: string, repo_id: string) => request<SkillPr>('GET', `/catalog/${id_path(id)}/repos/${id_path(repo_id)}`),
  uninstall_catalog: (id: string) => request<null>('DELETE', `/catalog/${id_path(id)}`),
  /** Sources of `workspace` (every source when absent). */
  list_sources: (workspace?: string) => request<KnowledgeSource[]>('GET', '/knowledge/sources' + query({ workspace })),
  add_source: (input: NewSource) => request<KnowledgeSource>('POST', '/knowledge/sources', input),
  /** Off `workspace` (default Main); once no workspace has it, its docs, chunks and vectors go with it. */
  delete_source: (id: string, workspace?: string) => request<null>('DELETE', `/knowledge/sources/${id_path(id)}` + query({ workspace })),
  /** Admin: puts an existing source on `workspace` too (the same index). */
  import_source: (workspace: string, id: string) => request<null>('POST', `/workspaces/${id_path(workspace)}/knowledge_sources/${id_path(id)}`, {}),
  list_docs: (source_id: string) => request<KnowledgeDoc[]>('GET', `/knowledge/sources/${id_path(source_id)}/docs`),
  /** Text as `docs`, .xlsx/.docx/.pdf as base64 `files`. */
  upload_docs: (source_id: string, body: { docs?: UploadDoc[]; files?: UploadFile[] }) =>
    request<SyncReport>('POST', `/knowledge/sources/${id_path(source_id)}/docs`, body),
  /** Runs a connector source's sync now (422 for upload sources). */
  reindex_source: (id: string) => request<SyncReport>('POST', `/knowledge/sources/${id_path(id)}/reindex`, {}),
  get_doc: (id: string) => request<DocDetail>('GET', `/knowledge/docs/${id_path(id)}`),
  /** Only `workspace`'s sources (default Main), narrowed to `sources`. */
  search: (q: string, workspace?: string, sources: string[] = []) =>
    request<SearchHit[]>('GET', '/knowledge/search' + query({ q, workspace, sources: sources.join(',') || undefined })),
  /** The ecosystem's limit policy; `null` until someone picks one (asked once at onboarding). */
  get_on_limit: () => request<{ on_limit: OnLimit | null }>('GET', '/settings/on_limit'),
  /** Needs `manage_runtimes`. */
  set_on_limit: (on_limit: OnLimit | null) => request<{ on_limit: OnLimit | null }>('PUT', '/settings/on_limit', { on_limit }),
  /** The task's own limit policy; `null` follows the workflow, then the ecosystem. */
  task_on_limit: (task_id: string) => request<{ on_limit: OnLimit | null }>('GET', `/tasks/${id_path(task_id)}/on_limit`),
  set_task_on_limit: (task_id: string, on_limit: OnLimit | null) =>
    request<{ on_limit: OnLimit | null }>('PUT', `/tasks/${id_path(task_id)}/on_limit`, { on_limit }),
  /** The ecosystem's monthly spend cap and this month's spend. */
  get_spend: () => request<Spend>('GET', '/settings/budget'),
  /** Needs `manage_runtimes`; `null` removes the cap. */
  set_spend_cap: (monthly_usd: number | null) => request<Spend>('PUT', '/settings/budget', { monthly_usd }),
  /** The task's own cost budget per run; `null` leaves the workflow's. */
  task_budget: (task_id: string) => request<{ cost_usd: number | null }>('GET', `/tasks/${id_path(task_id)}/budget`),
  set_task_budget: (task_id: string, cost_usd: number | null) =>
    request<{ cost_usd: number | null }>('PUT', `/tasks/${id_path(task_id)}/budget`, { cost_usd }),
  get_embeddings: () => request<Embeddings>('GET', '/settings/embeddings'),
  /** Admin. Omit `api_key` to keep the saved key. */
  set_embeddings: (input: Omit<Embeddings, 'has_api_key'> & { api_key?: string }) =>
    request<Embeddings>('PUT', '/settings/embeddings', input),
  /** No workspace: Main. */
  get_entitlement: (workspace?: string) =>
    request<Entitlement>('GET', workspace ? `/entitlement?workspace=${encodeURIComponent(workspace)}` : '/entitlement'),
  /** The account at Sun (plan, credits, support); 503 when this ecosystem isn't connected to Sun. */
  get_account: () => request<Account>('GET', '/account'),
  /** Admin. The invoice's `checkout_url` is Sun's hosted payment page; the plan applies once paid. */
  subscribe: (order: { plan: PaidPlan; seats: number; currency: Currency; method: PayMethod }) =>
    request<Invoice>('POST', '/account/subscription', order),
  /** Admin. One catalog credit pack. */
  top_up: (order: { points: number; currency: Currency; method: PayMethod }) => request<Invoice>('POST', '/account/top-ups', order),
  list_invoices: () => request<Invoice[]>('GET', '/account/invoices'),
  list_tickets: () => request<Ticket[]>('GET', '/account/support'),
  open_ticket: (subject: string, body: string) => request<Ticket>('POST', '/account/support', { subject, body }),
  /** Write-only: the value goes in, never comes back. */
  set_secret: (scope: SecretScope, name: string, value: string) =>
    request<Secret>('PUT', `/secrets/${scope.kind}/${id_path(scope.id)}/${id_path(name)}`, { value }),
  delete_secret: (scope: SecretScope, name: string) =>
    request<null>('DELETE', `/secrets/${scope.kind}/${id_path(scope.id)}/${id_path(name)}`),
  get_context: (task_id: string) => request<TaskContext>('GET', `/tasks/${id_path(task_id)}/context`),
  set_context: (task_id: string, context: TaskContext) =>
    request<TaskContext>('PUT', `/tasks/${id_path(task_id)}/context`, context),
  /** The pack an agent step would get with this selection. Stores nothing. */
  preview_context: (task_id: string, context: TaskContext) =>
    request<ContextPreview>('POST', `/tasks/${id_path(task_id)}/context/preview`, context),
  list_mcp_servers: () => request<McpServer[]>('GET', '/mcp_servers'),
  /** Admin. The kernel connects once to list its tools; 422 in plain words when it can't. */
  add_mcp_server: (server: McpServer) => request<McpServerAdded>('POST', '/mcp_servers', server),
  /** Connects to the server now. */
  mcp_tools: (name: string) => request<McpTool[]>('GET', `/mcp_servers/${id_path(name)}/tools`),
  /** Admin. */
  remove_mcp_server: (name: string) => request<null>('DELETE', `/mcp_servers/${id_path(name)}`),
  task_mcp: (task_id: string) => request<McpGrant[]>('GET', `/tasks/${id_path(task_id)}/mcp`),
  set_task_mcp: (task_id: string, grants: McpGrant[]) =>
    request<McpGrant[]>('PUT', `/tasks/${id_path(task_id)}/mcp`, grants),

  // --- Per workspace (M4): agents, runtimes, secrets and memory. `workspace`
  // is required for anyone but a deployment owner, who may leave it out for
  // every workspace's (a new agent or runtime then goes to Main).
  /** The workspace's active agents, by name. */
  list_agents: (workspace?: string) => request<Agent[]>('GET', '/agents' + query({ workspace })),
  /** `manage_runtimes` in the workspace: a name and a runtime at least. */
  create_agent: (fields: AgentFields, workspace?: string) =>
    request<Agent>('POST', '/agents' + query({ workspace }), fields),
  list_runtimes: (workspace?: string) => request<Runtime[]>('GET', '/runtimes' + query({ workspace })),
  /** `manage_runtimes` in the workspace: adds a BYO key runtime (its key is the workspace's secret, set through `set_secret`), or with Sun Cloud a credits one. */
  create_runtime: (input: CreateRuntime, workspace?: string) =>
    request<Runtime>('POST', '/runtimes' + query({ workspace }), input),
  runtime_readiness: (id: string, workspace?: string) =>
    request<Readiness>('GET', `/runtimes/${id_path(id)}/readiness` + query({ workspace })),
  /** A workspace scope lists that workspace's (`manage_secrets` there); without a scope, the deployment's (owners). Names and times, never values. */
  list_secrets: (scope?: SecretScope) =>
    request<Secret[]>(
      'GET',
      scope?.kind === 'workspace'
        ? '/secrets' + query({ workspace: scope.id })
        : scope ? `/secrets?scope_kind=${scope.kind}&scope_id=${id_path(scope.id)}` : '/secrets',
    ),
  /** The workspace's long-term memory and its proposals, newest first. */
  memory: (workspace?: string) => request<MemoryItem[]>('GET', '/memory' + query({ workspace })),
  memory_metrics: (workspace?: string) =>
    request<MemoryMetrics>('GET', '/memory/metrics' + query({ workspace })),
  // --- end per workspace (M4)
};

/** One artifact version's bytes, as a download (the session cookie authorizes it). */
export const artifact_url = (id: string) => `${BASE}/artifacts/${id_path(id)}`;

/** A pin's crop (an `<img src>`); the hash only busts the cache when a new crop lands. */
export const crop_url = (task_id: string, pin_id: string, side: PreviewSide, hash: string) =>
  `${BASE}/tasks/${id_path(task_id)}/pins/${id_path(pin_id)}/crops/${side}?v=${encodeURIComponent(hash)}`;

/** A device verify screenshot of run `run_id` (an `<img src>`, so the session cookie authorizes it). */
export const screenshot_url = (run_id: string, blob: string) =>
  `${BASE}/runs/${id_path(run_id)}/screenshots/${id_path(blob)}`;

/** A live log at `path` (`?after=seq`): every entry with `seq > after`, then new ones as they land.
 *  Real kernel: SSE, which resumes from the last `seq` on reconnect (Last-Event-ID).
 *  Mock: polls `list`. `on_closed` fires when the stream gives up (e.g. 401). Returns a stop function. */
function watch_log<T extends { seq: number }>(
  path: string,
  list: (after: number) => Promise<T[]>,
  after: number,
  on_entry: (e: T) => void,
  on_closed: () => void,
): () => void {
  if (use_mock) {
    let last = after;
    const tick = async () => {
      for (const e of await list(last)) {
        last = e.seq;
        on_entry(e);
      }
    };
    tick();
    const timer = setInterval(tick, 1000);
    return () => clearInterval(timer);
  }
  const source = new EventSource(`${BASE}${path}?after=${after}`);
  source.onmessage = (m) => on_entry(JSON.parse(m.data) as T);
  source.onerror = () => source.readyState === EventSource.CLOSED && on_closed();
  return () => source.close();
}

/** Live run timeline (see `watch_log`). */
export const watch_events = (run_id: string, after: number, on_event: (e: RunEvent) => void, on_closed: () => void) =>
  watch_log(`/runs/${id_path(run_id)}/events`, (last) => api.list_events(run_id, last), after, on_event, on_closed);

/** Live task activity: comments, moves and runs (see `watch_log`). */
export const watch_activity = (task_id: string, after: number, on_entry: (e: TaskActivity) => void, on_closed: () => void) =>
  watch_log(`/tasks/${id_path(task_id)}/activity`, (last) => api.task_activity(task_id, last), after, on_entry, on_closed);

/** Live device view of a task: state changes, binary payloads (pictures, or H.264 access units
 *  when the state's `mime` is `video/h264`) and failed inputs. Asks for video when `video` (the browser
 *  decodes H.264) until `no_video()`. Reconnects every few seconds while open (the mock has no device;
 *  tests fake the socket). */
export function watch_device(
  task_id: string,
  video: boolean,
  on_state: (s: DeviceState) => void,
  on_frame: (data: ArrayBuffer, mime: string) => void,
  on_input_failed: (message: string) => void,
): { send: (input: DeviceInput) => void; no_video: () => void; close: () => void } {
  const url = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}${BASE}/tasks/${id_path(task_id)}/device`;
  let socket: WebSocket | null = null;
  let mime = 'image/png';
  let retry: ReturnType<typeof setTimeout> | undefined;
  let closed = false;
  const open = () => {
    on_state({ kind: 'connecting' });
    const ws = new WebSocket(video ? `${url}?video=h264` : url);
    ws.binaryType = 'arraybuffer';
    ws.onmessage = (m) => {
      if (ws !== socket) return;
      if (typeof m.data !== 'string') return on_frame(m.data as ArrayBuffer, mime);
      const message = JSON.parse(m.data) as DeviceState | { kind: 'input_failed'; message: string };
      if (message.kind === 'input_failed') return on_input_failed(message.message);
      if (message.kind === 'live') mime = message.mime;
      on_state(message);
    };
    ws.onclose = () => {
      if (!closed && ws === socket) retry = setTimeout(open, 3000);
    };
    socket = ws;
  };
  open();
  return {
    send: (input) => socket?.readyState === WebSocket.OPEN && socket.send(JSON.stringify(input)),
    no_video: () => {
      if (!video || closed) return;
      video = false;
      clearTimeout(retry);
      socket?.close();
      open();
    },
    close: () => {
      closed = true;
      clearTimeout(retry);
      socket?.close();
    },
  };
}
