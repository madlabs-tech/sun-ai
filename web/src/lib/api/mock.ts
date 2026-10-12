// In-memory fake of the /api/v1 kernel API (Fake transport for the client).
// Sample data mirrors the Madlabs orchestrator UI kit. State resets on reload.
import type { OutboxItem,
  Workspace,
  Project,
  ApiError,
  CatalogEntry,
  DryRun,
  Correction,
  ByoKeyConfig,
  CreateRuntime,
  CreateTask,
  GateResult,
  Host,
  KnowledgeFlag,
  McpGrant,
  McpServer,
  McpServerAdded,
  McpTool,
  ModInstall,
  PermissionMode,
  Pin,
  PinTarget,
  Design,
  Preview,
  PreviewSide,
  Repo,
  QualityMetrics,
  Run,
  RunDetail,
  TaskRun,
  RunEvent,
  RunStatus,
  Runtime,
  SkillPr,
  Step,
  StepStatus,
  StepType,
  TaskActivity,
  TaskDetail,
  TaskPatch,
  TaskQuality,
  TaskQualityView,
  TaskStatus,
  Workflow,
  WorkflowIssue,
  WorkflowTemplate,
} from './types';
import { BYO_KEY_ADAPTERS, MAIN_WORKSPACE, TASK_MOVES } from './types';
import { LineCounter, isMap, isSeq, parse, parseDocument, stringify } from 'yaml';
import { ADMIN, MEMBER, mock_me, VIEWER } from './auth';
import { eval_route } from './mock_evals';
import { market_route } from './mock_market';
import { account_route, sun_cloud } from './mock_account';
import { knowledge_route, link_source } from './mock_knowledge';
import { memory_route } from './mock_memory';
import { is_vcs_route, vcs_route } from './mock_vcs';
import { pull_request_route } from './mock_pull_requests';
import { agents_route, find_agent } from './mock_agents';
import { budget_route, has_ecosystem_secret, on_limit_route, settings_route } from './mock_settings';

type StepDef = [key: string, name: string, type: StepType];

const WORKFLOWS: Record<string, Workflow & { steps: StepDef[] }> = {
  bugfix: { id: 'wf_bugfix', workspace_id: MAIN_WORKSPACE, name: 'Bug fix → verified PR', version: 3, steps: [['plan', 'Plan', 'agent'], ['implement', 'Implement', 'agent'], ['verify', 'Gates', 'gate'], ['pr', 'Open PR', 'action'], ['review', 'Review', 'approval']] },
  feature: { id: 'wf_feature', workspace_id: MAIN_WORKSPACE, name: 'Mobile feature, checked on a device', version: 2, steps: [['plan', 'Plan', 'agent'], ['implement', 'Implement', 'agent'], ['device', 'Verify on device', 'extension'], ['verify', 'Gates', 'gate'], ['pr', 'Linked PRs', 'action'], ['review', 'Review', 'approval']] },
  uirefactor: { id: 'wf_uirefactor', workspace_id: MAIN_WORKSPACE, name: 'UI refactor with visual review', version: 1, steps: [['implement', 'Implement', 'agent'], ['canvas', 'Design canvas', 'gate'], ['verify', 'Gates', 'gate'], ['pr', 'Open PR', 'action'], ['review', 'Review', 'approval']] },
  campaign: { id: 'wf_campaign', workspace_id: MAIN_WORKSPACE, name: 'Campaign: brief → drafts → approval', version: 4, steps: [['brief', 'Read brief', 'agent'], ['drafts', 'Channel drafts', 'agent'], ['brand', 'Brand gates', 'gate'], ['approval', 'Approval', 'approval'], ['schedule', 'Schedule posts', 'action']] },
  prd: { id: 'wf_prd', workspace_id: MAIN_WORKSPACE, name: 'Interview notes → PRD → tasks', version: 2, steps: [['notes', 'Read notes', 'agent'], ['draft', 'Draft PRD', 'agent'], ['sections', 'Section gates', 'gate'], ['approval', 'Approval', 'approval'], ['tasks', 'Create tasks', 'action']] },
  launch: { id: 'wf_launch', workspace_id: MAIN_WORKSPACE, name: 'Launch post with a review check', version: 1, steps: [['drafts', 'Channel drafts', 'agent'], ['brand', 'Brand gates', 'gate'], ['review_kind', 'Which review?', 'decide'], ['approval', 'Approval', 'approval'], ['schedule', 'Schedule posts', 'action']] },
  weekly: { id: 'wf_weekly', workspace_id: MAIN_WORKSPACE, name: 'Weekly summary of work and cost', version: 1, steps: [['gather', 'Gather runs', 'agent'], ['summarize', 'Summarize', 'agent'], ['post', 'Post to Slack', 'notify']] },
};

const CHAIN = ['claude-code', 'codex', 'opencode', 'credits'];

const today = (hm: string) => `2026-10-07T${hm}:00Z`;

interface Seed {
  n: number;
  title: string;
  wf: keyof typeof WORKFLOWS;
  repos: string[];
  /** The run's state; the task's status follows it (`follow_run`). */
  status: RunStatus;
  step: number;
  step_status?: StepStatus;
  runtime: string;
  cost: number;
  switches?: number;
  end_reason?: Run['end_reason'];
  gates?: [string, GateResult['status']][];
  artifact?: string;
  /** `[hh:mm, kind, summary, detail?, more payload?]` */
  events: [string, string, string, string?, Record<string, unknown>?][];
}

const SEEDS: Seed[] = [
  { n: 151, title: 'Weekly summary for the founders', wf: 'weekly', repos: [], status: 'queued', step: 0, runtime: 'credits', cost: 0, events: [['09:00', 'created', 'Created by schedule · every Monday 09:00']] },
  { n: 150, title: 'Refactor checkout button states', wf: 'uirefactor', repos: ['web'], status: 'queued', step: 0, runtime: 'claude-code', cost: 0, events: [['10:11', 'created', 'Created from Jira MAD-88 by Dimas']] },
  { n: 142, title: 'Fix login timeout on mobile', wf: 'bugfix', repos: ['api', 'mobile'], status: 'running', step: 1, runtime: 'claude-code', cost: 0.84, events: [
    ['10:02', 'created', 'Created from Slack #bugs by Rina'],
    ['10:02', 'dispatched', 'Dispatched to Claude Code · worktrees sun/SUN-142 in api, mobile'],
    ['10:02', 'session_assigned', 'Session on Claude Code', undefined, { context_window: 200000, rotate_at_percent: 80 }],
    ['10:03', 'plan', 'Plan ready · 4 to-dos', '1. Find session timeout config\n2. Raise refresh window to 30 min\n3. Retry refresh on resume (mobile)\n4. Add regression test'],
    ['10:05', 'tool_call', 'grep "session_timeout" api/', 'api/auth/session.go:41\napi/config/defaults.go:12'],
    ['10:06', 'usage', 'Context at 164k tokens', undefined, { context_used: 164000 }],
    ['10:06', 'rotation', 'Context 82% full: continuing in a fresh `claude-code` session', undefined, { percent: 82 }],
    ['10:07', 'session_assigned', 'Session on Claude Code', undefined, { context_window: 200000, rotate_at_percent: 80 }],
    ['10:09', 'file_change', 'Edited api/auth/session.go · +18 −4'],
    ['10:09', 'usage', 'Context at 31k tokens', undefined, { context_used: 31000 }]] },
  { n: 147, title: 'October campaign: channel drafts', wf: 'campaign', repos: [], status: 'running', step: 1, runtime: 'codex', cost: 0.31, switches: 1, artifact: 'x-thread.md', events: [
    ['09:40', 'created', 'Created by Sari · brief attached'],
    ['09:41', 'dispatched', 'Dispatched to Claude Code · task folder'],
    ['09:43', 'runtime_switch', 'Claude Code hit its quota · resets 14:00. Moved to Codex with a hand-off pack.'],
    ['09:44', 'artifact', 'Draft saved · x-thread.md v1']] },
  { n: 139, title: 'Sync pricing page with new plans', wf: 'bugfix', repos: ['web'], status: 'running', step: 1, step_status: 'stalled', runtime: 'opencode', cost: 1.12, events: [
    ['09:12', 'created', 'Created by Dimas'],
    ['09:13', 'dispatched', 'Dispatched to OpenCode'],
    ['09:31', 'command', 'npm run build', '✓ built in 41s'],
    ['09:49', 'watchdog', 'Watchdog: no output, tool call or commit for 18 min · step marked stalled']] },
  { n: 145, title: 'Offline mode for wallet balances', wf: 'feature', repos: ['api', 'mobile'], status: 'verifying', step: 2, step_status: 'running', runtime: 'claude-code', cost: 2.4, gates: [['Diff not empty', 'passed'], ['Tests', 'passed'], ['Typecheck', 'passed'], ['Lint', 'running'], ['CI green', 'pending']], events: [
    ['08:30', 'created', 'Created by Hery'],
    ['08:31', 'session_assigned', 'Session on Claude Code', undefined, { context_window: 200000, rotate_at_percent: 80 }],
    ['09:10', 'note', 'Learning: wallet balances need a cache TTL', undefined, { note_id: '01JARN0TE00000000000000001', kind: 'learning', body: 'Wallet balances need a cache TTL; stale balances confuse people offline.' }],
    ['09:40', 'device_result', 'iPhone 15 (iOS 17.0): flow failed', undefined, {
      device: 'iPhone 15 (iOS 17.0)', passed: false,
      output: 'Running flow smoke.yaml\n✅ Launch app\n✅ Tap "Wallet"\n❌ Assert "Balance" is visible\nElement not found: Text matching regex: Balance',
      screenshots: [{ name: 'wallet.png', blob: 'a'.repeat(64) }, { name: 'sun-final.png', blob: 'b'.repeat(64) }] }],
    ['09:41', 'outbox_delivered', 'Screenshots attached to PR #481', undefined, { target: 'github.attach_to_pr' }],
    ['09:57', 'usage', 'Context at 138k tokens', undefined, { context_used: 138000 }],
    ['09:58', 'verify', 'Agent said done · engine is verifying']] },
  { n: 140, title: 'PRD: referral program', wf: 'prd', repos: [], status: 'waiting_approval', step: 3, step_status: 'waiting_approval', runtime: 'claude-code', cost: 0.66, artifact: 'prd-referrals.md', gates: [['Required sections', 'passed'], ['Length ≤ 2,000 words', 'passed'], ['Links resolve', 'passed']], events: [
    ['08:10', 'created', 'Created from interview notes (3 docs)'],
    ['08:26', 'artifact', 'Artifact saved · prd-referrals.md v2'],
    ['08:27', 'gate_result', 'Section gates passed · 3/3'],
    ['08:27', 'approval', 'Waiting for Product lead approval']] },
  { n: 138, title: 'Launch post for X and LinkedIn', wf: 'launch', repos: [], status: 'waiting_approval', step: 2, step_status: 'waiting_approval', runtime: 'codex', cost: 0.22, artifact: 'launch-posts.md', gates: [['Brand glossary', 'passed'], ['Length limits', 'passed'], ['Links resolve', 'passed']], events: [
    ['09:05', 'created', 'Created by Sari'],
    ['09:20', 'gate_result', 'Brand gates passed · 3/3'],
    ['09:20', 'decision', 'Decision flag needs a person: confidence 0.41 is below the threshold 0.80: a human decides', undefined, {
      step_key: 'review_kind', decides: 'flag', acted: false, answer: { choice: 'brand' }, confidence: 0.41, model: 'english',
      question: { type: 'choice', instructions: 'Which review does this post need?', criteria: { legal: 'It makes claims about money, returns or partners', brand: 'Tone and glossary only', none: 'Ready to schedule' } },
    }]] },
  { n: 134, title: 'Retry failed webhook deliveries', wf: 'bugfix', repos: ['api'], status: 'failed', step: 2, step_status: 'failed', runtime: 'claude-code', cost: 1.31, gates: [['Diff not empty', 'passed'], ['Tests', 'failed']], events: [
    ['08:02', 'plan', 'Plan ready · 3 to-dos'],
    ['08:31', 'gate_result', 'Tests failed · 2 of 214 · sent back twice, then failed'],
  ] },
  { n: 132, title: 'Changelog for v2.4', wf: 'bugfix', repos: ['web'], status: 'running', step: 3, runtime: 'codex', cost: 0.47, events: [
    ['08:40', 'gate_result', 'All gates passed'],
    ['08:52', 'outbox_unknown', 'No answer from github.open_pr in 30 s · not sent again until you decide'],
  ] },
  { n: 136, title: 'Bump wallet SDK to v2.3', wf: 'bugfix', repos: ['mobile'], status: 'done', step: 5, runtime: 'codex', cost: 0.58, end_reason: 'gates_passed', events: [['07:40', 'done', 'PR #477 merged · all gates passed']] },
  { n: 133, title: 'Landing page copy refresh', wf: 'weekly', repos: ['web'], status: 'done', step: 3, runtime: 'claude-code', cost: 0.4, end_reason: 'unverified', events: [
    ['07:10', 'done', 'Done (unverified) · no gate configured'],
    ['07:10', 'gate_suggested', 'Suggested gate: test with `npm test`', undefined, { gate: 'test', command: 'npm test' }],
  ] },
];

const fake_id = (prefix: string, n: number) => `01JA${prefix}${String(n).padStart(4, '0')}`.toUpperCase().padEnd(26, '0');

/** Main's repositories (Settings → Code); `mock_vcs` adds and removes. */
export const REPOS: Repo[] = (
  [
    ['api', 'Go backend: REST API, billing, webhooks'],
    ['web', 'Next.js customer web app'],
    ['mobile', 'React Native app for iOS and Android'],
  ] as const
).map(([name, description], i) => ({
  id: fake_id('R', i + 1),
  workspace_id: MAIN_WORKSPACE,
  url: `https://github.com/madlabs/${name}.git`,
  default_branch: 'main',
  description,
}));

/** The seed's repo names → Main's repo ids (the run's `repo_ids`). */
const repo_ids_of = (names: string[]) => names.map((name) => REPOS.find((r) => r.url.endsWith(`/${name}.git`))?.id ?? name);

function make_steps(def: StepDef[], current: number, status: StepStatus, runtime: string): Step[] {
  return def.map(([step_key, name, type], i) => ({
    step_key,
    name,
    type,
    attempt: 1,
    status: i < current ? 'succeeded' : i === current ? status : 'queued',
    runtime_id: i <= current ? runtime : null,
    started_at: null,
    ended_at: null,
  }));
}

let next_seq = 1;
const tasks: TaskDetail[] = [];
const events = new Map<string, RunEvent[]>();
/** Each task's activity log (`GET /tasks/{id}/activity`). */
const activity = new Map<string, TaskActivity[]>();
/** The signed-in person's member id, for what they do here. */
const ME = '01JAME00000000000000000000';

function event(run_id: string, kind: string, summary: string, detail?: string, at = new Date().toISOString()): RunEvent {
  const e: RunEvent = { seq: next_seq++, run_id, step_key: null, kind, payload: detail ? { summary, detail } : { summary }, at };
  events.get(run_id)?.push(e) ?? events.set(run_id, [e]);
  return e;
}

for (const s of SEEDS) {
  const wf = WORKFLOWS[s.wf];
  const id = fake_id('T', s.n);
  const run_id = fake_id('R', s.n);
  const run: Run | null =
    s.status === 'queued'
      ? null
      : {
          id: run_id,
          task_id: id,
          workflow_version: wf.version,
          status: s.status,
          end_reason: s.end_reason ?? null,
          budget: { wall: '2h', turns: 200, cost_usd: 5 },
          cost_usd: s.cost,
          runtime_id: s.runtime,
          runtime_chain: CHAIN,
          runtime_switches: s.switches ?? 0,
          steps: make_steps(wf.steps, s.step, s.step_status ?? 'running', s.runtime),
          repo_ids: repo_ids_of(s.repos),
        };
  const task: TaskDetail = {
    id,
    key: `SUN-${s.n}`,
    workspace_id: wf.workspace_id,
    number: s.n,
    title: s.title,
    body: '',
    priority: 'none',
    status: 'todo',
    workflow: { id: wf.id, name: wf.name, workspace_id: wf.workspace_id, version: wf.version },
    project_id: null,
    assignee: { kind: 'workflow', id: wf.id },
    position: s.n,
    created_by: null,
    run,
    created_at: today(s.events[0][0]),
    updated_at: today(s.events[s.events.length - 1][0]),
    gates: (s.gates ?? []).map(([name, status]) => ({ step_key: 'verify', name, status })),
    artifacts: s.artifact ? [{ id: fake_id('A', s.n), task_id: id, name: s.artifact, version: 2, mime: 'text/markdown' }] : [],
  };
  follow_run(task);
  tasks.push(task);
  log(task, 'created', { status: task.status }, today(s.events[0][0]));
  for (const [hm, kind, summary, detail, more] of s.events) {
    const e = event(run?.id ?? id, kind, summary, detail, today(hm));
    Object.assign(e.payload, more);
    if (typeof more?.step_key === 'string') e.step_key = more.step_key;
  }
}

export const RUNTIMES: Runtime[] = [
  { id: 'claude-code', name: 'Claude Code', adapter: 'claude_code', source: 'byo_runtime', status: 'busy', cooldown_until: null, sessions: 2, max_sessions: 3, permission_mode: 'accept_edits', allow_bypass: true },
  { id: 'codex', name: 'Codex', adapter: 'codex', source: 'byo_runtime', status: 'busy', cooldown_until: null, sessions: 1, max_sessions: 2, permission_mode: null, allow_bypass: true },
  { id: 'opencode', name: 'OpenCode', adapter: 'opencode', source: 'byo_key', status: 'cooling_down', cooldown_until: today('14:00'), sessions: 0, max_sessions: 2, permission_mode: 'plan', allow_bypass: true },
  { id: 'credits', name: 'Sun credits', adapter: 'opencode', source: 'credits', status: 'ready', cooldown_until: null, sessions: 0, max_sessions: 4, permission_mode: null, allow_bypass: true, model: 'qwen3-coder' },
];

const MIB = 1024 * 1024;
const tool = (ready: boolean, devices: string[] = [], detail: string | null = null) => ({ ready, version: ready ? '1.0' : null, devices, detail });
const live = (n: number, step_key: string, rss_mb: number, cpu_percent: number, uptime_s: number) =>
  ({ session_id: fake_id('S', n), run_id: fake_id('R', n), step_key, rss_bytes: rss_mb * MIB, cpu_percent, uptime_s });
const HOSTS: Host[] = [
  { id: fake_id('H', 1), name: 'hery-mbp', status: 'online', capabilities: { os: 'macos', arch: 'arm64', runtimes: ['claude-code', 'codex'], devices: ['iPhone 16 (iOS 18.2)'], exec_envs: [
    { kind: 'bare', ready: true, version: null, detail: null },
    { kind: 'container', ready: true, version: 'Docker version 28.5.1', detail: null },
    { kind: 'vm', ready: false, version: null, detail: '`limactl --version` cannot run: not found; install Lima (`brew install lima`)' },
  ], device_tools: {
    ios_simulator: tool(true, ['iPhone 16 (iOS 18.2)']),
    android_emulator: tool(false, [], 'No Android system image: run `sdkmanager "system-images;android-35;google_apis;arm64-v8a"`'),
    maestro: tool(true),
  } }, sessions: 2, session_stats: [live(142, 'fix', 612, 38.5, 1460), live(147, 'draft', 248, 4.2, 312)],
  disk: [{ task_id: fake_id('T', 142), bytes: 1.4 * 1024 * MIB }, { task_id: fake_id('T', 147), bytes: 3 * MIB }], last_seen_at: today('10:12'), workspace_ids: [MAIN_WORKSPACE], enrolled_by: null },
  { id: fake_id('H', 2), name: 'build-box-01', status: 'online', capabilities: { os: 'linux', arch: 'x86_64', runtimes: ['opencode', 'codex'], devices: ['Pixel_8_API_35'] }, sessions: 1,
    session_stats: [live(139, 'fix', 410, 0.4, 7380)], disk: [{ task_id: fake_id('T', 139), bytes: 820 * MIB }], last_seen_at: today('10:12'), workspace_ids: [MAIN_WORKSPACE], enrolled_by: null },
  { id: fake_id('H', 3), name: 'rina-laptop', status: 'offline', capabilities: { os: 'linux', arch: 'x86_64', runtimes: ['claude-code'], devices: [] }, sessions: 0, last_seen_at: today('06:40'), workspace_ids: [MAIN_WORKSPACE], enrolled_by: null },
];

const MODS: ModInstall[] = [
  { name: 'github', kind: 'plugin', version: '1.2.0', installed: true, permissions: ['network:api.github.com', 'secret:GITHUB_TOKEN'], signed: true },
  {
    name: 'slack', kind: 'plugin', version: '0.9.1', installed: true, permissions: ['network:slack.com', 'secret:SLACK_BOT_TOKEN', 'secret:SLACK_SIGNING_SECRET'], signed: true,
    update: { version: '1.0.0', permissions: ['network:slack.com', 'network:files.slack.com', 'secret:SLACK_BOT_TOKEN', 'secret:SLACK_SIGNING_SECRET'] },
  },
  { name: 'device-farm', kind: 'extension', version: '0.3.0', installed: true, permissions: ['process:xcrun', 'process:adb', 'device:kvm', 'plugin:github'], signed: false },
  { name: 'gitlab', kind: 'plugin', version: '1.0.0', installed: false, permissions: ['network:gitlab.com', 'secret:GITLAB_TOKEN'], signed: false, tier: 'official', repo: 'https://github.com/madlabs-tech/sun-plugin-gitlab', description: 'Opens merge requests, comments and follows labeled issues on GitLab.' },
  { name: 'jira', kind: 'plugin', version: '0.4.0', installed: false, permissions: ['network:atlassian.net', 'secret:JIRA_TOKEN'], signed: false, tier: 'official', repo: 'https://github.com/madlabs-tech/sun-plugin-jira', description: 'Comments on, moves and follows labeled or assigned Jira issues.' },
  { name: 'social', kind: 'plugin', version: '0.2.0', installed: false, permissions: ['network:api.x.com', 'secret:SOCIAL_TOKEN'], signed: false, tier: 'official', repo: 'https://github.com/madlabs-tech/sun-plugin-social', description: 'Posts task results to X.' },
  { name: 'linear', kind: 'plugin', version: '2.1.0', installed: false, permissions: ['network:api.linear.app', 'secret:LINEAR_API_KEY'], signed: false, tier: 'community', repo: 'https://github.com/linear-fans/sun-plugin-linear', description: 'Follows Linear issues and comments on them.' },
  { name: 'figma-sync', kind: 'extension', version: '1.4.0', installed: false, permissions: ['network:api.figma.com', 'secret:FIGMA_TOKEN'], signed: false, tier: 'community', repo: 'https://github.com/figma-sync/sun-ext-figma-sync', description: 'Keeps design tokens in sync with Figma.' },
];
/** Installed extensions that declare plugin `name` (`plugin:<name>`), as `ModInstall::needs_plugin`. */
const mod_dependents = (name: string) => MODS.filter((m) => m.installed && m.permissions.includes(`plugin:${name}`)).map((m) => m.name);

/** `GET /runtimes/{id}/readiness`: OpenCode runs on build-box-01; a BYO key runtime also needs its key secret. */
function mock_readiness(r: Runtime) {
  const key = r.config?.key_secret;
  const missing = key && !has_ecosystem_secret(key);
  return missing
    ? { installed: true, version: null, logged_in: false, detail: `key_secret: \`${key}\` is not set; set it with \`sun secret set ${key}\`` }
    : { installed: true, version: null, logged_in: true, detail: null };
}

const entry = (e: Partial<CatalogEntry> & Pick<CatalogEntry, 'id' | 'kind' | 'description' | 'source' | 'author' | 'license' | 'version'>): CatalogEntry =>
  ({ stacks: [], reviewed: true, installed: null, matches: [], in_workflows: false, hosts: [], secrets: [], ...e });
/** Suggestions the ecosystem dismissed. */
const DISMISSED = new Set<string>();
const CATALOG: CatalogEntry[] = [
  entry({ id: 'tdd', kind: 'skill', description: 'Test-driven development: write the failing test first, then the fix.', source: 'github.com/mattpocock/skills', author: 'Matt Pocock', license: 'MIT', version: '1.2.3', installed: '1.2.2' }),
  entry({ id: 'diagnosing-bugs', kind: 'skill', description: 'A diagnosis loop for hard bugs and performance regressions.', source: 'github.com/mattpocock/skills', author: 'Matt Pocock', license: 'MIT', version: '1.2.3' }),
  entry({ id: 'frontend-design', kind: 'skill', description: 'Build distinctive, production-grade web interfaces.', source: 'github.com/anthropics/claude-code', author: 'Anthropic', license: 'Apache-2.0', version: '1.0.0', stacks: ['frontend'] }),
  entry({ id: 'github', kind: 'mcp', description: 'Read and update GitHub issues, pull requests and Actions runs.', source: 'github.com/github/github-mcp-server', author: 'GitHub', license: 'MIT', version: '0.20.0', tools: ['get_issue', 'create_pull_request'], hosts: ['api.githubcopilot.com'], secrets: ['GITHUB_TOKEN'], installed: '0.20.0' }),
  entry({ id: 'playwright', kind: 'mcp', description: 'Drive a real browser: open pages, click, fill forms, take screenshots.', source: 'github.com/microsoft/playwright-mcp', author: 'Microsoft', license: 'Apache-2.0', version: '0.0.41', stacks: ['frontend'], tools: ['browser_navigate', 'browser_click'], hosts: ['registry.npmjs.org'] }),
  entry({ id: 'context7', kind: 'mcp', description: 'Up-to-date library docs and code examples for the prompt.', source: 'github.com/upstash/context7', author: 'Upstash', license: 'MIT', version: '1.0.20', tools: ['resolve-library-id', 'get-library-docs'], hosts: ['mcp.context7.com'], secrets: ['CONTEXT7_API_KEY'] }),
];

export class MockError extends Error {
  constructor(
    readonly status: number,
    readonly body: ApiError,
  ) {
    super(body.error.message);
  }
}

export const not_found = (what: string) => new MockError(404, { error: { code: 'not_found', message: `${what} not found` } });

/** Custom MCP servers; installed catalog MCP entries are listed next to them. Grants by task id. */
const MCP_SERVERS: McpServer[] = [];
const MCP_GRANTS = new Map<string, McpGrant[]>();
const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });
const mcp_servers = (): McpServer[] => [
  ...CATALOG.filter((e) => e.kind === 'mcp' && e.installed).map((e) => ({
    name: e.id,
    transport: { type: 'url' as const, url: `https://${e.hosts[0] ?? e.id}/mcp`, ...(e.secrets[0] ? { bearer_secret: e.secrets[0] } : {}) },
  })),
  ...MCP_SERVERS,
].sort((a, b) => a.name.localeCompare(b.name));
/** Catalog servers offer their listed tools (the first one read-only); custom ones a docs-style set. */
function mcp_tools(name: string): McpTool[] {
  const e = CATALOG.find((x) => x.id === name && x.kind === 'mcp');
  if (e) return (e.tools ?? []).map((t, i) => ({ name: t, description: '', read_only: i === 0 }));
  return [
    { name: 'search', description: 'Searches the docs.', read_only: true },
    { name: 'read_page', description: 'Reads one page.', read_only: true },
    { name: 'create_page', description: 'Creates a page.', read_only: false },
  ];
}
function mcp_server(name: string): McpServer {
  const s = mcp_servers().find((x) => x.name === name);
  if (!s) throw not_found(`MCP server ${name}`);
  return s;
}
function add_mcp_server(server: McpServer): McpServerAdded {
  if (!/^[a-z][a-z0-9_-]{0,63}$/.test(server.name ?? '')) throw invalid(`invalid input: mcp server: name ${JSON.stringify(server.name)} must match [a-z][a-z0-9_-]{0,63}`);
  if (server.name === 'sun') throw invalid('invalid input: mcp server name `sun` is reserved for Sun’s own tools');
  if (mcp_servers().some((s) => s.name === server.name)) throw invalid(`invalid input: an MCP server named ${server.name} already exists; pick another name or remove it first`);
  const t = server.transport;
  if (t.type === 'url' && !/^https?:\/\/\S+$/.test(t.url)) throw invalid(`invalid input: mcp server ${server.name}: url must be http:// or https://`);
  if (t.type === 'command' && (!t.command || (t.command.includes('/') && !t.command.startsWith('/')))) throw invalid(`invalid input: mcp server ${server.name}: command must be a bare name or an absolute path`);
  const secrets = t.type === 'url' ? (t.bearer_secret ? [t.bearer_secret] : []) : t.secrets;
  const missing = secrets.find((n) => !has_ecosystem_secret(n));
  if (missing) throw invalid(`invalid input: secret ${missing} is not set; add it under Settings → Secrets (or \`sun secret set ${missing}\`) first`);
  MCP_SERVERS.push(clone(server));
  return { server: clone(server), tools: mcp_tools(server.name) };
}
function set_task_mcp(task_id: string, grants: McpGrant[]): McpGrant[] {
  find_task(task_id);
  const names = grants.map((g) => g.server);
  const unknown = names.find((n) => !mcp_servers().some((s) => s.name === n));
  if (unknown) throw invalid(`No MCP server named ${unknown} is registered. Add it first.`);
  if (new Set(names).size !== names.length) throw invalid('invalid input: mcp server granted twice');
  MCP_GRANTS.set(task_id, clone(grants));
  return clone(grants);
}

/** Repo-scope skill installs by `<skill>:<repo_id>`; the PR opens on the next status read. */
const SKILL_PRS = new Map<string, SkillPr>();

function catalog_entry(id: string): CatalogEntry {
  const e = CATALOG.find((x) => x.id === id);
  if (!e) throw not_found('Catalog entry');
  return e;
}

/** Running previews as `<task_id>/<side>`. Their URLs 404 on `vite preview`; Playwright serves them. */
const previews = new Set<string>();
const preview = (t: TaskDetail, side: PreviewSide): Preview => ({ side, url: `/preview/mock-${t.key}-${side}/` });
/** SUN-139's repo lists two screens in `.sun-preview` and has its own design system. */
const PRICING_DESIGN: Design = {
  screens: [{ name: 'Home', path: '/' }, { name: 'Pricing', path: '/pricing' }],
  files: [
    { path: 'DESIGN.md', text: '# Design\n\nUse the tokens in `src/styles/tokens.css`. Buttons are `--brand` on `--surface`.\n' },
    {
      path: 'src/styles/tokens.css',
      text: ':root {\n  --brand: #ff5a1f;\n  --ink: #1a1d24;\n  --surface: #ffffff;\n  --space-2: 8px;\n  --space-4: 16px;\n  --space-6: 24px;\n  --text-sm: 0.875rem;\n  --text-base: 1rem;\n  --radius-md: 8px;\n}\n',
    },
  ],
};
const design_of = (t: TaskDetail): Design =>
  t.key === 'SUN-139' ? clone(PRICING_DESIGN) : { screens: [{ name: '/', path: '/' }], files: [] };

/** SUN-139 changed a migration and the env; SUN-136 merged, then needed a fix. */
const DATA_TOUCHED: TaskQualityView['data_touched'] = [
  { path: 'db/migrations/20261002_plan_prices.sql', rule: 'migration' },
  { path: '.env.staging', rule: 'data_config' },
];
function quality_of(t: TaskDetail): TaskQualityView {
  const merged = t.key === 'SUN-136';
  const quality: TaskQuality | null = merged
    ? {
        task_id: t.id, mod_name: 'github', repo: 'madlabs/sun', pr_number: 412, base: 'main',
        merge_sha: '9f3c2e1a7b', merged_at: '2026-10-01T09:12:00Z', window_days: 14, ci_state: 'success',
        reverted_by: null, checked_at: '2026-10-06T08:00:00Z',
        fix_commits: [{ sha: '41d07be2c9', subject: 'fix: price rounding for IDR', at: '2026-10-03T14:40:00Z' }],
        bug_issues: [
          {
            relation: 'origin', key: 'madlabs/sun#398', title: 'Wallet SDK v2.2 drops IDR decimals',
            url: 'https://github.com/madlabs/sun/issues/398', opened_at: null,
          },
          {
            relation: 'regression', key: 'madlabs/sun#431', title: 'Checkout total off by one rupiah after the SDK bump',
            url: 'https://github.com/madlabs/sun/issues/431', opened_at: '2026-10-02T16:05:00Z',
          },
        ],
      }
    : null;
  return { data_touched: t.key === 'SUN-139' ? clone(DATA_TOUCHED) : [], quality, held_up: quality ? false : null };
}
const METRICS: QualityMetrics = {
  merged: 4, decided: 3, held_up: 2, ci_failed: 0, reverted: 0, with_fixes: 1, with_bugs: 1, held_up_rate: 2 / 3,
};

/** The pricing task's sources: an old PRD and a sales deck that disagree on the price. */
const PRICING_FLAGS: KnowledgeFlag[] = [
  {
    kind: 'stale',
    message: 'Pricing PRD was last changed on 2026-02-11, 238 days ago (the limit is 180 days). Check it is still current.',
    citations: ['Pricing PRD > Plans > Pro · v12 · 2026-02-11 · https://acme.atlassian.net/wiki/spaces/PROD/pages/4211'],
    pointers: ['doc:01JA7PRICINGPRD'],
  },
  {
    kind: 'conflict',
    message: 'Sources disagree on "Plans > Pro": Pricing PRD says 99.000; Sales deck Q4 says 120.000. Confirm which one is current before using it.',
    citations: [
      'Pricing PRD > Plans > Pro · v12 · 2026-02-11 · https://acme.atlassian.net/wiki/spaces/PROD/pages/4211',
      'Sales deck Q4 > Plans > Pro · rev 31 · 2026-09-30 · https://docs.google.com/document/d/1q2w3e',
    ],
    pointers: ['doc:01JA7PRICINGPRD', 'doc:01JA7SALESDECK'],
  },
];

/** The offline-mode task: one failed gate (sent back) and a steer that pushed back. */
/** Human answers that differed from the decision engine's (`POST /tasks/:id/decision`). */
const overrides: Correction[] = [];

export function corrections_of(t: TaskDetail): Correction[] {
  if (t.key !== 'SUN-145') return overrides.filter((c) => c.task_id === t.id);
  const base = { task_id: t.id, run_id: t.run?.id ?? '', workflow_id: t.workflow?.id ?? '', runtime: 'claude-code', classified_by: 'rules' };
  return [
    {
      ...base, id: fake_id('C', 1), step_key: 'verify', kind: 'gate_fail', cause_label: 'test_failure', created_at: '2026-10-07T09:12:00Z',
      body: 'Gate `test` failed: FAIL wallet/offline_test.go:58\n    TestCachedBalance: expected 120.50, got 0 (cache was empty after restart)\nFAIL\tgithub.com/acme/api/wallet\t0.41s',
    },
    {
      ...base, id: fake_id('C', 2), step_key: 'implement', kind: 'steer', cause_label: 'wrong_scope', created_at: '2026-10-07T09:31:00Z',
      body: 'No, don’t touch the sync scheduler. Only change the balance cache, the scheduler is out of scope.',
    },
  ];
}

const pins: Pin[] = [];

/** Text edits map to the pin's source line; computed style values never appear in source as-is. */
function commit_pin(p: Pin) {
  const hand_off = (why: string) => new MockError(422, { error: { code: 'invalid_input', message: `${why}. Hand the edit to the agent instead.` } });
  if (!p.target.source) throw hand_off('This element maps to no source line');
  if (!p.edits.length) throw new MockError(422, { error: { code: 'invalid_input', message: 'Save an edit on the pin first.' } });
  const odd = p.edits.find((e) => e.property !== 'text');
  if (odd) throw hand_off(`\`${odd.from}\` isn’t on ${p.target.source.file}:${p.target.source.line} exactly once`);
  const sha = (0xa11c0de + pins.indexOf(p)).toString(16).padEnd(40, '0');
  Object.assign(p, { status: 'done', updated_at: new Date().toISOString() });
  p.after_crop ??= p.before_crop?.replace('before', 'after') ?? null;
  const t = find_task(p.task_id);
  if (t.run) event(t.run.id, 'commit', `you committed a canvas edit · ${p.target.source.file}:${p.target.source.line} · ${sha.slice(0, 7)}`);
  return { sha, pin: clone(p) };
}

function find_pin(task_id: string, pin_id: string): Pin {
  const t = find_task(task_id);
  const p = pins.find((x) => x.id === pin_id && x.task_id === t.id);
  if (!p) throw not_found('Pin');
  return p;
}

/** Side effects; R132's PR timed out and waits for a person. */
const outbox: OutboxItem[] = [
  { id: fake_id('O', 132), run_id: fake_id('R', 132), step_key: 'pr', target: 'github.open_pr', idem_key: `${fake_id('R', 132)}:pr:github.open_pr`, status: 'unknown', summary: 'Changelog for v2.4' },
];

/** The step finished: the next queued one starts (an approval waits on a person). */
function step_done(t: TaskDetail, step_key: string) {
  const steps = t.run?.steps ?? [];
  const i = steps.findIndex((s) => s.step_key === step_key);
  if (i < 0) return;
  steps[i].status = 'succeeded';
  const next = steps[i + 1];
  if (!next) return set_status(t, 'done', 'gates_passed');
  next.status = next.type === 'approval' ? 'waiting_approval' : 'running';
  set_status(t, next.type === 'approval' ? 'waiting_approval' : 'running');
}

const RETRYABLE = ['failed', 'rejected', 'cancelled', 'stalled', 'looping', 'budget_exceeded'];

/** `POST /runs/{id}/steps/{key}/retry`: a new attempt, finished steps kept. */
function retry_step(parts: string[]): TaskDetail {
  const t = tasks.find((x) => x.run?.id === parts[1]);
  const step = t?.run?.steps.find((s) => s.step_key === parts[3]);
  if (!t || !step) throw not_found('Step');
  if (!RETRYABLE.includes(step.status)) throw new MockError(422, { error: { code: 'invalid_input', message: `step \`${step.step_key}\` is ${step.status}; only a step that failed can be retried` } });
  step.attempt += 1;
  step.status = 'running';
  set_status(t, 'running');
  if (t.gates) for (const g of t.gates) if (g.status === 'failed') g.status = 'running';
  const kept = t.run?.steps.filter((s) => s.status === 'succeeded').length ?? 0;
  event(parts[1], 'intervention', `Retrying ${step.name} (attempt ${step.attempt}) · ${kept} finished steps kept`);
  return clone(t);
}

export function find_task(id: string): TaskDetail {
  const t = tasks.find((x) => x.id === id || x.key === id);
  if (!t) throw not_found('Task');
  return t;
}

/** Moves the task's run, and the task after it. */
function set_status(t: TaskDetail, status: RunStatus, end_reason: Run['end_reason'] = null) {
  t.updated_at = new Date().toISOString();
  if (t.run) {
    t.run.status = status;
    t.run.end_reason = end_reason;
  }
  follow_run(t);
}

/** The engine → status mapping (sun-engine `tasks`): a run a person paused is Blocked, one
 *  paused for them to decide (a limit) Needs you; a run that succeeded is Done after an approval step, else Needs you. A task a
 *  person parked in Backlog or Blocked stays there while its run pauses or is cancelled. */
function follow_run(t: TaskDetail) {
  const run = t.run;
  if (!run) return;
  const waiting = run.steps.some((s) => s.status === 'waiting_approval');
  const next = ({
    queued: 'todo',
    running: 'in_progress',
    verifying: 'verifying',
    // Paused: by a person (Stop, Pause) it is Blocked; a limit's pause waits for them to decide.
    waiting_approval: waiting || run.steps.some((s) => s.status === 'limit_hit') ? 'needs_you' : 'blocked',
    done: run.steps.some((s) => s.type === 'approval') ? 'done' : 'needs_you',
    failed: 'failed',
    cancelled: 'cancelled',
  } as const satisfies Record<RunStatus, TaskStatus>)[run.status];
  const parked = (t.status === 'backlog' || t.status === 'blocked') && (next === 'blocked' || next === 'cancelled');
  if (t.status === next || parked || t.status === 'done') return;
  log(t, 'status', { from: t.status, to: next });
  t.status = next;
}

const conflict = (message: string) => new MockError(409, { error: { code: 'conflict', message } });

const LABEL: Record<TaskStatus, string> = {
  backlog: 'Backlog', todo: 'Todo', in_progress: 'In progress', verifying: 'Verifying', needs_you: 'Needs you',
  done: 'Done', blocked: 'Blocked', failed: 'Failed', cancelled: 'Cancelled',
};

/** A person's move (`PATCH /tasks/{id} {status}`), by `TASK_MOVES` with the kernel's 409 sentences. */
function move_task(t: TaskDetail, to: TaskStatus) {
  if (t.status === to) return;
  if (t.status === 'done') throw conflict("This task is done; a done task doesn't move.");
  if (to === 'verifying' && (t.workflow || t.assignee)) throw conflict('Only the engine moves a task that runs a workflow or an agent to Verifying.');
  if (!TASK_MOVES[t.status].includes(to))
    throw conflict(to === 'done' ? 'A task moves to Done only from Needs you.' : `A task can't move from ${LABEL[t.status]} to ${LABEL[to]}.`);
  if (to === 'done' && t.run && !['done', 'failed', 'cancelled'].includes(t.run.status)) throw conflict('Approve or cancel the run first.');
  log(t, 'status', { from: t.status, to }, undefined, true);
  t.status = to;
  t.updated_at = new Date().toISOString();
  const run = t.run && !['done', 'failed', 'cancelled'].includes(t.run.status) ? t.run : null;
  if ((to === 'backlog' || to === 'blocked') && run?.status === 'running') {
    run.status = 'waiting_approval';
    event(run.id, 'intervention', 'Paused by you');
  } else if ((to === 'todo' || to === 'in_progress') && run && is_paused_run(run)) set_status(t, 'running');
  else if ((to === 'cancelled' || to === 'failed') && run) {
    run.status = 'cancelled';
    run.end_reason = 'cancelled';
  }
}

const is_paused_run = (run: Run) => run.status === 'waiting_approval' && !run.steps.some((s) => s.status === 'waiting_approval');

export function log(t: TaskDetail, kind: TaskActivity['kind'], body: Record<string, unknown>, at = new Date().toISOString(), by_you = false) {
  const list = activity.get(t.id) ?? [];
  activity.set(t.id, list);
  const entry: TaskActivity = {
    id: fake_id('Y', list.length + 1) + t.id.slice(-4), task_id: t.id, seq: list.length + 1, kind,
    actor_member_id: by_you ? ME : null, body: by_you ? { ...body, by: 'user:me' } : body, created_at: at,
  };
  list.push(entry);
  return entry;
}

/** Each task's runs once one started here (`GET /tasks/{id}/runs`); a seeded task has only its seeded run. */
const run_history = new Map<string, TaskRun[]>();
/** Runs a newer one replaced (`GET /runs/{id}`). */
const past_runs = new Map<string, RunDetail>();

// SUN-136 ran twice: its first run failed on Tests, the retry merged.
{
  const t = tasks.find((x) => x.number === 136)!;
  const run = t.run!;
  const wf = WORKFLOWS.bugfix;
  const first: Run = { ...structuredClone(run), id: fake_id('R', 1136), status: 'failed', end_reason: 'failed', cost_usd: 0.31, steps: make_steps(wf.steps, 2, 'failed', 'codex') };
  past_runs.set(first.id, { run: first, gates: [{ step_key: 'verify', name: 'Diff not empty', status: 'passed' }, { step_key: 'verify', name: 'Tests', status: 'failed' }] });
  event(first.id, 'gate_result', 'Tests failed · 1 of 88', undefined, today('07:05'));
  run_history.set(t.id, [
    { id: first.id, attempt: 1, trigger: 'created', status: 'failed', cost_usd: 0.31, started_at: today('07:00'), ended_at: today('07:05') },
    { id: run.id, attempt: 2, trigger: 'retry', status: run.status, cost_usd: run.cost_usd, started_at: today('07:20'), ended_at: today('07:40') },
  ]);
}

function runs_of(t: TaskDetail): TaskRun[] {
  const run = t.run;
  if (!run) return [];
  const started = run.steps.find((x) => x.started_at)?.started_at ?? null;
  const history = run_history.get(t.id) ?? [{ id: run.id, attempt: 1, trigger: 'created', status: run.status, cost_usd: run.cost_usd, started_at: started, ended_at: run.steps.at(-1)?.ended_at ?? null }];
  return history.map((r) => (r.id === run.id ? { ...r, status: run.status, cost_usd: run.cost_usd } : r));
}

/** The engine runs an agent's task: a new run of its one-step system workflow (one going is cancelled). */
function start_agent_run(t: TaskDetail, trigger: string) {
  const agent = t.assignee?.kind === 'agent' ? find_agent(t.assignee.id) : undefined;
  if (!agent) return;
  const now = new Date().toISOString();
  const history = runs_of(t).map((r) => (r.ended_at ? r : { ...r, status: 'cancelled' as const, ended_at: now }));
  if (t.run && !['done', 'failed', 'cancelled'].includes(t.run.status)) [t.run.status, t.run.end_reason] = ['cancelled', 'cancelled'];
  if (t.run) past_runs.set(t.run.id, { run: t.run, gates: t.gates });
  const runtime = agent.runtime_chain[0] ?? 'claude-code';
  const run: Run = {
    id: fake_id('R', 900 + next_seq++), task_id: t.id, workflow_version: 1, status: 'running', end_reason: null,
    budget: { cost_usd: 5 }, cost_usd: 0, runtime_id: runtime, runtime_chain: agent.runtime_chain, runtime_switches: 0, repo_ids: [],
    steps: make_steps([['agent', agent.name, 'agent']], 0, 'running', runtime).map((x) => ({ ...x, started_at: now })),
  };
  t.run = run;
  run_history.set(t.id, [...history, { id: run.id, attempt: history.length + 1, trigger, status: 'running', cost_usd: 0, started_at: now, ended_at: null }]);
  log(t, 'run', { run_id: run.id, trigger });
  event(run.id, 'dispatched', `Dispatched to ${RUNTIMES.find((r) => r.id === runtime)?.name ?? runtime} for ${agent.name}`);
  follow_run(t);
}

// Workflow files for the builder. Seed workflows get a minimal valid file; templates are the real ones.
const STEP_FIELDS: Record<StepType, Record<string, unknown>> = {
  agent: { prompt: 'Do the work described in the task.' },
  gate: { gates: ['test', 'lint'] },
  action: { use: 'github.open_pr' },
  approval: { who: ['role:admin'] },
  wait: { for: '1h' },
  notify: { to: ['slack:#team'] },
  decide: { decides: 'flag', question: { type: 'noul', instructions: 'Does this need a second look?', criteria: 'It needs review' }, threshold: 0.8, who: ['role:admin'] },
  extension: { with: { platform: 'ios', app: 'build/App.app', flow: '.maestro/smoke.yaml' } },
};
const sources = new Map<string, string>(
  Object.entries(WORKFLOWS).map(([name, w]) => [
    w.id,
    stringify({ name, on: ['manual'], steps: w.steps.map(([id, , type]) => ({ id, type, ...STEP_FIELDS[type] })) }),
  ]),
);
const TEMPLATE_FILES = import.meta.glob<string>('../../../../templates/workflows/*.yaml', { query: '?raw', import: 'default', eager: true });
const TEMPLATE_META: [string, WorkflowTemplate['role'], string, string[]][] = [
  ['bug-fix', 'engineering', 'Bug fix → verified PR', ['github']],
  ['marketing-campaign', 'marketing', 'Campaign: brief → drafts → approval', ['social']],
  ['feature-across-repos', 'engineering', 'Feature across repos', ['github']],
  ['ui-refactor', 'engineering', 'UI refactor with the design canvas', ['github']],
  ['prd', 'product', 'Interview notes → PRD → tasks', ['jira']],
  ['client-deployment', 'forward-deployed', 'Client deployment', ['github', 'slack']],
  ['weekly-summary', 'leadership', 'Weekly summary: work, cost, blockers', ['gdrive', 'slack']],
];
const TEMPLATES: WorkflowTemplate[] = TEMPLATE_META.map(([id, role, title, mods]) => ({
  id, role, title, mods,
  description: 'A ready workflow for this role.',
  source_yaml: TEMPLATE_FILES[`../../../../templates/workflows/${id}.yaml`] ?? '',
}));

/** A rough stand-in for sun-workflow: YAML syntax, a name, unique step ids, agent prompts. */
function check(source: string): WorkflowIssue[] {
  const counter = new LineCounter();
  const doc = parseDocument(source, { lineCounter: counter });
  if (doc.errors.length) return doc.errors.map((e) => ({ line: e.linePos?.[0].line ?? 1, column: e.linePos?.[0].col ?? 1, message: e.message }));
  const at = (node: unknown) => {
    const range = (node as { range?: number[] } | null)?.range;
    const pos = counter.linePos(range?.[0] ?? 0);
    return { line: pos.line, column: pos.col };
  };
  const issues: WorkflowIssue[] = [];
  if (!doc.get('name')) issues.push({ line: 1, column: 1, message: 'missing field `name`' });
  const steps = doc.get('steps', true);
  const seen = new Set<string>();
  for (const step of isSeq(steps) ? steps.items : []) {
    if (!isMap(step)) continue;
    const id = String(step.get('id') ?? '');
    if (seen.has(id)) issues.push({ ...at(step.get('id', true)), message: `duplicate step id \`${id}\`` });
    seen.add(id);
    if (step.get('type') === 'agent' && !step.has('prompt')) issues.push({ ...at(step.get('id', true)), message: '`agent` steps need a `prompt`' });
  }
  return issues;
}

/** Like the kernel's dry run: waves from `needs`, mods from dotted types and notify schemes,
 *  $0.62 per agent step as the "past sessions" average. */
function dry_run(source: string): DryRun {
  const issues = check(source);
  if (issues.length) throw new MockError(422, { error: { code: 'invalid_input', message: issues.map((i) => `${i.line}:${i.column}: ${i.message}`).join('; ') } });
  const wf = parse(source) as { steps?: Record<string, unknown>[]; runtime?: { chain?: string[] }; budget?: { cost_usd?: number } };
  const steps = wf.steps ?? [];
  const wave: Record<string, number> = {};
  const mods = new Set<string>();
  const kinds = ['agent', 'gate', 'approval', 'wait', 'notify', 'action'];
  const out = steps.map((s, i) => {
    const needs = (s.needs as string[] | undefined) ?? (i ? [String(steps[i - 1].id)] : []);
    const id = String(s.id);
    wave[id] = 1 + Math.max(0, ...needs.map((n) => wave[n] ?? 0));
    const type = String(s.type);
    const uses = type === 'action' ? [String(s.use)] : type === 'notify' ? (s.to as string[]) ?? [] : type === 'gate' ? ((s.gates as unknown[]) ?? []).map((g) => (typeof g === 'string' ? g : Object.keys(g as object)[0])) : (s.who as string[]) ?? (kinds.includes(type) ? [] : [type]);
    for (const u of uses) if (u.includes('.') || (type === 'notify' && u.includes(':'))) mods.add(u.split(/[.:]/)[0]);
    return { id, kind: (kinds.includes(type) ? type : 'extension') as StepType, wave: wave[id], uses };
  });
  const agent_steps = out.filter((s) => s.kind === 'agent').length;
  return {
    steps: out,
    runtimes: wf.runtime?.chain ?? [],
    mods: [...mods].sort().map((name) => ({ name, installed: MODS.some((m) => m.name === name && m.installed) })),
    agent_steps,
    estimated_cost_usd: agent_steps ? 0.62 * agent_steps : null,
    budget_usd: wf.budget?.cost_usd ?? null,
    // Like the kernel's sandbox: every step passes; device checks need a real device.
    simulation: simulate(out),
  };
}

function simulate(steps: DryRun['steps']): DryRun['simulation'] {
  const failed = steps.findIndex((s) => s.uses.some((u) => u.startsWith('device.')));
  const NOTES: Partial<Record<StepType, string>> = {
    agent: 'Scripted fake runtime; no model ran',
    gate: 'Commands not run; reported as passing',
    approval: 'Approved automatically; a person decides in a real run',
  };
  return {
    status: failed < 0 ? 'succeeded' : 'failed',
    end_reason: failed < 0 ? null : `step \`${steps[failed].id}\` failed: a dry run has no device`,
    steps: steps.map((s, i) => ({
      id: s.id,
      status: failed < 0 || i < failed ? 'succeeded' : i === failed ? 'failed' : 'not_reached',
      attempts: failed >= 0 && i > failed ? 0 : 1,
      note: NOTES[s.kind] ?? null,
    })),
    actions: steps.slice(0, failed < 0 ? undefined : failed).filter((s) => s.kind === 'action').flatMap((s) => s.uses),
  };
}

function save_source(id: string | null, source: string, workspace = MAIN_WORKSPACE) {
  const issues = check(source);
  if (issues.length) throw new MockError(422, { error: { code: 'invalid_input', message: issues.map((i) => `${i.line}:${i.column}: ${i.message}`).join('; ') } });
  const name = String(parse(source).name);
  let w = id ? Object.values(WORKFLOWS).find((x) => x.id === id) : undefined;
  if (id && !w) throw not_found('Workflow');
  if (w) Object.assign(w, { version: w.version + 1, name }); // a changed `name:` renames it, like the kernel
  else {
    if (!WORKSPACES.some((b) => b.id === workspace)) throw not_found('Workspace');
    w = { id: fake_id('W', Object.keys(WORKFLOWS).length + 1), name, workspace_id: workspace, version: 1, steps: [] };
    WORKFLOWS[w.id] = w;
  }
  sources.set(w.id, source);
  return { id: w.id, name: w.name, workspace_id: w.workspace_id, version: w.version };
}

export const clone = <T>(v: T): T => structuredClone(v);

// Workspaces (workspace = workspace): Main holds the seed workflows.
const new_workspace = (id: string, name: string, slug: string, created_at: string, group_ids: string[]): Workspace => ({
  id, name, slug, description: '', context: '', logo_url: null,
  issue_prefix: slug.replace(/-/g, '').slice(0, 4).toUpperCase(), issue_counter: 0, default_workflow_id: null, created_at, group_ids,
  git_settings: { github_enabled: true, pr_sidebar: true, auto_link: true, move_on_merge: true },
});
const WORKSPACES: Workspace[] = [
  { ...new_workspace(MAIN_WORKSPACE, 'Main', 'main', '2026-10-01T09:00:00Z', [VIEWER, MEMBER, ADMIN]), issue_prefix: 'SUN', issue_counter: 151 },
];

/** Like the kernel: a given slug must be valid and free; the default comes from the name, made unique with `-2`, …. */
function workspace_slug(raw: unknown, name: string): string {
  if (raw !== undefined && raw !== null) {
    const slug = String(raw).trim();
    if (!/^[a-z0-9]+(-[a-z0-9]+)*$/.test(slug) || slug.length > 48) throw invalid('A slug is 1 to 48 lowercase letters or digits, in words joined by single `-`.');
    if (WORKSPACES.some((w) => w.slug === slug)) throw invalid(`A workspace with the slug \`${slug}\` already exists.`);
    return slug;
  }
  const base = name.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+/, '').slice(0, 48).replace(/-+$/, '') || 'workspace';
  let slug = base;
  for (let n = 2; WORKSPACES.some((w) => w.slug === slug); n++) slug = `${base.slice(0, 48 - `-${n}`.length).replace(/-+$/, '')}-${n}`;
  return slug;
}

const PROJECTS: Project[] = [];

function find_project(id: string): Project {
  const project = PROJECTS.find((p) => p.id === id);
  if (!project) throw not_found('Project');
  return project;
}

/** Like the kernel: trimmed, 1–64 characters, unique in the workspace ignoring case. */
function project_name(raw: unknown, self: Project): string {
  const name = String(raw ?? '').trim();
  if (!name || [...name].length > 64) throw invalid('Give the project a name of 1 to 64 characters.');
  if (PROJECTS.some((p) => p.id !== self.id && p.workspace_id === self.workspace_id && p.name.toLowerCase() === name.toLowerCase())) {
    throw invalid(`This workspace already has a project named \`${name}\`.`);
  }
  return name;
}

/** Like the kernel: trimmed, at most `max` characters. */
function workspace_text(raw: unknown, max: number, what: string): string {
  const text = String(raw ?? '').trim();
  if ([...text].length > max) throw invalid(`Keep the ${what} to ${max} characters.`);
  return text;
}

/** Like the kernel: a workspace is seen by its groups, and by owners and `access_all_workspaces`. */
const sees = (workspace: Workspace) => {
  const me = mock_me();
  return me.permissions.includes('access_all_workspaces') || workspace.group_ids.some((g) => me.groups.includes(g));
};

/** Without `manage_groups`, only your own groups' access changes (422 otherwise). */
function only_own_groups(changed: string[]) {
  const me = mock_me();
  if (!me.permissions.includes('manage_groups') && changed.some((g) => !me.groups.includes(g))) {
    throw invalid('You can give or take workspace access only for your own groups.');
  }
}

export function find_workspace(id: string): Workspace {
  const workspace = WORKSPACES.find((b) => b.id === id);
  if (!workspace) throw not_found('Workspace');
  return workspace;
}

/** Like the kernel: trimmed, 1–64 characters, unique ignoring case. */
function workspace_name(raw: unknown, self?: string): string {
  const name = String(raw ?? '').trim();
  if (!name || [...name].length > 64) throw invalid('Give the workspace a name of 1 to 64 characters.');
  if (WORKSPACES.some((b) => b.id !== self && b.name.toLowerCase() === name.toLowerCase())) throw invalid(`A workspace named \`${name}\` already exists.`);
  return name;
}

/** Answers like the kernel would: status + JSON body. */
export function mock_handle(method: string, full_path: string, body: unknown): { status: number; data: unknown } {
  try {
    return { status: 200, data: route(method, full_path, body) };
  } catch (e) {
    if (e instanceof MockError) return { status: e.status, data: e.body };
    throw e;
  }
}

function route(method: string, full_path: string, body: unknown): unknown {
  const [path, query = ''] = full_path.split('?');
  const params = new URLSearchParams(query);
  const parts = path.split('/').filter(Boolean).map(decodeURIComponent);
  const input = (body ?? {}) as Record<string, unknown>;
  if (['entitlement', 'secrets'].includes(parts[0])) return settings_route(method, parts, params, input);
  if (parts.at(-1) === 'on_limit') return on_limit_route(method, parts, input);
  if (parts.at(-1) === 'budget') return budget_route(method, parts, input);
  if (parts[0] === 'knowledge' || parts[0] === 'settings' || (parts[0] === 'tasks' && parts[2] === 'context')) return knowledge_route(method, parts, params, input);
  if (parts[0] === 'memory' || (['corrections', 'notes'].includes(parts[0]) && parts[2] === 'star')) return memory_route(method, parts, input);
  if (parts[0] === 'evals' || parts[0] === 'eval_sets') return eval_route(method, parts, input);
  if (parts[0] === 'marketplace') return market_route(method, parts, input, params);
  if (parts[0] === 'account') return account_route(method, parts, input);
  if (is_vcs_route(method, parts)) return vcs_route(method, parts, params, input);
  if (parts[0] === 'tasks' && (parts[2] === 'pull_requests' || parts[2] === 'pr_auto_complete')) return pull_request_route(method, parts, input);
  if (parts[0] === 'agents') return agents_route(method, parts, input);
  const key = `${method} /${parts[0] ?? ''}${parts.length > 1 ? '/:id' : ''}${parts[2] ? '/' + parts[2] : ''}`;

  switch (key) {
    case 'GET /tasks': {
      const [status, workspace, project] = [params.get('status'), params.get('workspace'), params.get('project')];
      return clone(tasks.filter((t) => (!status || t.status === status) && (!workspace || t.workspace_id === workspace) && (!project || t.project_id === project)));
    }
    case 'GET /workspaces':
      return clone(WORKSPACES.filter(sees));
    case 'POST /workspaces': {
      const group_ids = (input.group_ids as string[] | undefined) ?? [];
      only_own_groups(group_ids);
      const name = workspace_name(input.name);
      const workspace = new_workspace(fake_id('B', WORKSPACES.length + 1), name, workspace_slug(input.slug, name), new Date().toISOString(), group_ids);
      const base = workspace.issue_prefix;
      for (let n = 2; WORKSPACES.some((w) => w.issue_prefix === workspace.issue_prefix); n++) workspace.issue_prefix = `${base.slice(0, 10 - String(n).length)}${n}`;
      WORKSPACES.push(workspace);
      return clone(workspace);
    }
    case 'PUT /workspaces/:id/groups': {
      const workspace = find_workspace(parts[1]);
      const next = (input.group_ids as string[] | undefined) ?? [];
      only_own_groups([...workspace.group_ids.filter((g) => !next.includes(g)), ...next.filter((g) => !workspace.group_ids.includes(g))]);
      workspace.group_ids = next;
      return clone(workspace);
    }
    case 'PATCH /workspaces/:id': {
      const workspace = find_workspace(parts[1]);
      const next = { ...workspace };
      if (input.name !== undefined) next.name = workspace_name(input.name, workspace.id);
      if (input.description !== undefined) next.description = workspace_text(input.description, 1000, 'description');
      if (input.context !== undefined) next.context = workspace_text(input.context, 8000, 'agent context');
      if (input.default_workflow_id !== undefined) {
        const id = input.default_workflow_id as string | null;
        if (id !== null && !Object.values(WORKFLOWS).some((w) => w.id === id && w.workspace_id === workspace.id)) throw invalid('Pick a default workflow on this workspace.');
        next.default_workflow_id = id;
      }
      if (input.git_settings !== undefined) {
        const settings = input.git_settings as Record<string, unknown>;
        for (const key of Object.keys(workspace.git_settings)) if (typeof settings[key] !== 'boolean') throw invalid(`git_settings.${key}: use true or false`);
        next.git_settings = settings as unknown as Workspace['git_settings'];
      }
      return clone(Object.assign(workspace, next));
    }
    case 'POST /workspaces/:id/prefix': {
      const workspace = find_workspace(parts[1]);
      const prefix = String(input.issue_prefix ?? '').trim().toUpperCase();
      if (!/^[A-Z0-9]{1,10}$/.test(prefix)) throw invalid('An issue prefix is 1 to 10 letters or digits.');
      if (WORKSPACES.some((w) => w.id !== workspace.id && w.issue_prefix === prefix)) throw invalid(`Another workspace already uses the prefix ${prefix}.`);
      workspace.issue_prefix = prefix;
      // Keys are computed from the prefix: every task's key follows.
      for (const t of tasks) if (t.workspace_id === workspace.id) t.key = `${prefix}-${t.number}`;
      workspace.issue_prefix = prefix;
      return clone(workspace);
    }
    case 'PUT /workspaces/:id/logo': {
      const workspace = find_workspace(parts[1]);
      const image = body as Blob;
      if (image.size > 1024 * 1024) throw invalid('The logo is larger than 1 MB.');
      if (!['image/png', 'image/jpeg', 'image/webp'].includes(image.type)) throw invalid('The logo must be a PNG, JPEG or WebP image.');
      workspace.logo_url = URL.createObjectURL(image);
      return clone(workspace);
    }
    case 'DELETE /workspaces/:id/logo': {
      const workspace = find_workspace(parts[1]);
      workspace.logo_url = null;
      return clone(workspace);
    }
    case 'GET /workspaces/:id/projects': {
      const workspace = find_workspace(parts[1]);
      return clone(PROJECTS.filter((p) => p.workspace_id === workspace.id).sort((a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase())));
    }
    case 'POST /workspaces/:id/projects': {
      const workspace = find_workspace(parts[1]);
      const project: Project = {
        id: fake_id('P', PROJECTS.length + 1), workspace_id: workspace.id, name: '', description: workspace_text(input.description, 1000, 'description'), created_at: new Date().toISOString(),
      };
      project.name = project_name(input.name, project);
      PROJECTS.push(project);
      return clone(project);
    }
    case 'PATCH /projects/:id': {
      const project = find_project(parts[1]);
      const next = { ...project };
      if (input.name !== undefined) next.name = project_name(input.name, project);
      if (input.description !== undefined) next.description = workspace_text(input.description, 1000, 'description');
      return clone(Object.assign(project, next));
    }
    case 'DELETE /projects/:id': {
      const project = find_project(parts[1]);
      PROJECTS.splice(PROJECTS.indexOf(project), 1);
      // The store's `ON DELETE SET NULL`: its tasks stay, outside any project.
      for (const t of tasks) if (t.project_id === project.id) t.project_id = null;
      return null;
    }
    case 'DELETE /workspaces/:id': {
      const workspace = find_workspace(parts[1]);
      if (workspace.id === MAIN_WORKSPACE) throw invalid("Main is the default workspace and can't be deleted.");
      if (Object.values(WORKFLOWS).some((w) => w.workspace_id === workspace.id)) throw invalid('The workspace still has workflows. Move or delete them first.');
      if (tasks.some((t) => t.workspace_id === workspace.id)) throw invalid('The workspace still has tasks.');
      WORKSPACES.splice(WORKSPACES.indexOf(workspace), 1);
      return null;
    }
    case 'GET /tasks/:id':
      return clone(find_task(parts[1]));
    case 'POST /tasks': {
      const create = input as unknown as CreateTask;
      const space = create.workspace_id ? find_workspace(create.workspace_id) : null;
      const assigned = create.assignee ?? null;
      if (assigned && create.workflow_id) throw invalid('Give the task an assignee or a workflow_id, not both.');
      if (assigned?.kind === 'agent' && !find_agent(assigned.id)) throw not_found('agent');
      const picked = assigned?.kind === 'workflow' ? assigned.id : create.workflow_id === 'default' ? space?.default_workflow_id : create.workflow_id;
      if (create.workflow_id === 'default' && !picked) throw invalid(space ? 'This workspace has no default workflow.' : 'Pick a workspace for the task.');
      const wf = picked ? Object.values(WORKFLOWS).find((w) => w.id === picked) : null;
      if (!create.title?.trim() || (picked && !wf)) throw new MockError(422, { error: { code: 'invalid', message: 'Give the task a title and a workflow.' } });
      const workspace = wf ? find_workspace(wf.workspace_id) : space;
      if (!workspace) throw invalid('Pick a workspace for the task.');
      const env = create.exec_env;
      const env_name = env?.kind === 'container' ? env.image : env?.kind === 'vm' ? env.template : 'ok';
      if (!/^[A-Za-z0-9_][A-Za-z0-9._\-/:@]{0,254}$/.test(env_name) || env_name.includes('..'))
        throw new MockError(422, { error: { code: 'invalid_input', message: `invalid input: bad ${env?.kind === 'vm' ? 'vm template' : 'container image'} \`${env_name}\`` } });
      workspace.issue_counter = Math.max(workspace.issue_counter, ...tasks.filter((x) => x.workspace_id === workspace.id).map((x) => x.number)) + 1;
      const n = workspace.issue_counter;
      const t: TaskDetail = {
        id: fake_id('T', n), key: `${workspace.issue_prefix}-${n}`, workspace_id: workspace.id, number: n, title: create.title.trim(), body: create.body ?? '',
        priority: create.priority ?? 'none', status: create.status ?? 'todo',
        workflow: wf ? { id: wf.id, name: wf.name, workspace_id: wf.workspace_id, version: wf.version } : null,
        project_id: create.project_id ?? null, assignee: create.assignee ?? (wf ? { kind: 'workflow', id: wf.id } : null), position: n, created_by: ME, run: null,
        created_at: new Date().toISOString(), updated_at: new Date().toISOString(), gates: [], artifacts: [],
      };
      tasks.push(t);
      log(t, 'created', { status: t.status }, undefined, true);
      event(t.id, 'created', 'Created by you on the workspace');
      if (t.status === 'todo') start_agent_run(t, 'created');
      return clone(t);
    }
    case 'PATCH /tasks/:id': {
      const t = find_task(parts[1]);
      const patch = input as TaskPatch;
      if (patch.title !== undefined && (!patch.title.trim() || patch.title.trim().length > 200)) throw invalid('Give the task a title of 1 to 200 characters.');
      if (patch.status) move_task(t, patch.status);
      if (patch.title !== undefined && patch.title.trim() !== t.title) {
        log(t, 'title', { from: t.title, to: patch.title.trim() }, undefined, true);
        t.title = patch.title.trim();
      }
      if (patch.body !== undefined && patch.body !== t.body) [t.body] = [patch.body, log(t, 'description', {}, undefined, true)];
      if (patch.priority && patch.priority !== t.priority) {
        log(t, 'priority', { from: t.priority, to: patch.priority }, undefined, true);
        t.priority = patch.priority;
      }
      if (patch.project_id !== undefined && patch.project_id !== t.project_id) {
        if (patch.project_id && find_project(patch.project_id).workspace_id !== t.workspace_id) throw invalid('The project is on another workspace than the task.');
        log(t, 'project', { from: t.project_id, to: patch.project_id }, undefined, true);
        t.project_id = patch.project_id;
      }
      if (patch.assignee !== undefined && JSON.stringify(patch.assignee) !== JSON.stringify(t.assignee)) {
        if (patch.assignee?.kind === 'workflow' && t.run) throw conflict('A workflow is picked before the first run.');
        if (patch.assignee?.kind === 'agent' && !find_agent(patch.assignee.id)) throw not_found('agent');
        log(t, 'assignee', { from: t.assignee, to: patch.assignee }, undefined, true);
        t.assignee = patch.assignee;
        const wf = patch.assignee?.kind === 'workflow' ? Object.values(WORKFLOWS).find((w) => w.id === patch.assignee?.id) : null;
        t.workflow = wf ? { id: wf.id, name: wf.name, workspace_id: wf.workspace_id, version: wf.version } : patch.assignee ? t.workflow : null;
        // An agent on an open task: the run going is cancelled and the agent's starts.
        if (patch.assignee?.kind === 'agent' && ['todo', 'in_progress', 'blocked'].includes(t.status)) start_agent_run(t, 'moved');
      }
      if (patch.position !== undefined) t.position = patch.position;
      return clone(t);
    }
    case 'POST /tasks/:id/comments': {
      const t = find_task(parts[1]);
      const text = String(input.body ?? '').trim();
      if (!text || [...text].length > 10_000) throw invalid('Write a comment of 1 to 10000 characters.');
      const entry = log(t, 'comment', { text }, undefined, true);
      // Wake-on-comment: a running agent is steered; a stopped one starts a new run with the comment.
      if (t.assignee?.kind === 'agent' && t.run?.status === 'running') event(t.run.id, 'intervention', `You: ${text}`);
      else if (t.assignee?.kind === 'agent' && t.status !== 'done' && t.status !== 'cancelled') start_agent_run(t, 'comment');
      return clone(entry);
    }
    case 'GET /tasks/:id/activity': {
      const t = find_task(parts[1]);
      const after = Number(params.get('after') ?? 0);
      return clone((activity.get(t.id) ?? []).filter((a) => a.seq > after));
    }
    case 'GET /tasks/:id/runs':
      return clone(runs_of(find_task(parts[1])));
    case 'POST /tasks/:id/interrupt': {
      const t = find_task(parts[1]);
      if (t.run?.status !== 'running') throw conflict('Nothing is running on this task.');
      move_task(t, 'blocked');
      return clone(t);
    }
    case 'POST /tasks/:id/approve': {
      const t = find_task(parts[1]);
      if (!t.run?.steps.some((x) => x.status === 'waiting_approval')) throw new MockError(409, { error: { code: 'conflict', message: 'This task is not waiting for approval.' } });
      t.run?.steps.forEach((s) => (s.status = 'succeeded'));
      set_status(t, 'done', 'approved');
      event(t.run?.id ?? t.id, 'approval', 'Approved by you · done');
      return clone(t);
    }
    case 'POST /tasks/:id/decision': {
      const t = find_task(parts[1]);
      const step = t.run?.steps.find((x) => x.status === 'waiting_approval' && x.type === 'decide');
      if (!t.run || !step) throw new MockError(409, { error: { code: 'conflict', message: 'No decision is waiting on a person.' } });
      const asked = events.get(t.run.id)?.filter((e) => e.kind === 'decision' && e.step_key === step.step_key).at(-1)?.payload;
      const criteria = (asked?.question as { criteria?: Record<string, string> } | undefined)?.criteria ?? {};
      const label = (input.answer as { choice?: string } | undefined)?.choice;
      if (!label || !(label in criteria)) throw new MockError(422, { error: { code: 'invalid_input', message: 'Pick one of the listed answers.' } });
      step.status = 'succeeded';
      const next = t.run.steps.find((x) => x.status === 'queued');
      if (next) next.status = 'waiting_approval';
      const suggested = (asked?.answer as { choice?: string } | undefined)?.choice;
      const e = event(t.run.id, 'decision', `Decided ${String(asked?.decides ?? 'it')}: ${label} · by you`);
      e.step_key = step.step_key;
      if (suggested && suggested !== label) {
        overrides.push({
          id: fake_id('C', 100 + overrides.length), task_id: t.id, run_id: t.run.id, workflow_id: t.workflow?.id ?? '', step_key: step.step_key,
          runtime: null, kind: 'decision_override', cause_label: 'other', classified_by: 'rules', created_at: new Date().toISOString(),
          body: `The decision engine answered {"choice":"${suggested}"} (confidence ${String(asked?.confidence)}); you answered {"choice":"${label}"}.`,
        });
      }
      t.updated_at = new Date().toISOString();
      return clone(t);
    }
    case 'POST /tasks/:id/reject': {
      const t = find_task(parts[1]);
      set_status(t, 'running');
      event(t.run?.id ?? t.id, 'intervention', `Rejected by you · ${String(input.reason || 'no reason given')} · sent back to the agent`);
      return clone(t);
    }
    case 'POST /tasks/:id/cancel': {
      const t = find_task(parts[1]);
      move_task(t, 'cancelled');
      return clone(t);
    }
    case 'GET /runs/:id': {
      const t = tasks.find((x) => x.run?.id === parts[1]);
      const found = t?.run ? { run: t.run, gates: t.gates } : past_runs.get(parts[1]);
      if (!found) throw not_found('Run');
      return clone(found);
    }
    case 'POST /runs/:id/retry': {
      const t = tasks.find((x) => x.run?.id === parts[1]);
      if (!t) throw not_found('Run');
      t.run?.steps.forEach((s) => s.status === 'stalled' && (s.status = 'running'));
      event(parts[1], 'intervention', 'Restarted by you with a hand-off pack');
      return clone(t);
    }
    case 'POST /runs/:id/steps': {
      // /runs/{id}/steps/{key}/switch (retry: see `retry_step`)
      if (parts[4] === 'retry') return retry_step(parts);
      const t = tasks.find((x) => x.run?.id === parts[1]);
      const step = t?.run?.steps.find((s) => s.step_key === parts[3]);
      if (!t?.run || !step || parts[4] !== 'switch') throw not_found('Step');
      const to = String(input.to ?? '');
      if (!t.run.runtime_chain.includes(to)) throw new MockError(422, { error: { code: 'invalid_input', message: `runtime \`${to}\` is not in the chain` } });
      if (step.type !== 'agent' || !['running', 'limit_hit'].includes(step.status))
        throw new MockError(422, { error: { code: 'invalid_input', message: `step \`${step.step_key}\` is ${step.status}; only a running or limit_hit step can switch` } });
      const name = (id: string | null) => RUNTIMES.find((r) => r.id === id)?.name ?? id;
      const from = step.runtime_id;
      [step.runtime_id, step.status, t.run.runtime_id] = [to, 'running', to];
      t.run.runtime_switches += 1;
      if (t.run.status === 'waiting_approval') set_status(t, 'running');
      event(t.run.id, 'handoff', `Moved from ${name(from)} to ${name(to)} with a hand-off pack (switched by hand)`);
      return clone(t);
    }
    case 'POST /runs/:id/messages': {
      const text = String(input.body ?? '').trim();
      // A steer needs a running agent; a queued message only a run that hasn't ended.
      const status = tasks.find((x) => x.run?.id === parts[1])?.run?.status;
      const ended = !status || ['done', 'failed', 'cancelled'].includes(status);
      if (input.queue ? ended : status !== 'running') throw new MockError(422, { error: { code: 'invalid_input', message: input.queue ? 'This run has ended.' : 'No agent is running on this task.' } });
      if (!text) throw new MockError(422, { error: { code: 'invalid', message: 'Write a message first.' } });
      return clone(event(parts[1], 'intervention', input.queue ? `Queued for the next step: ${text}` : `You: ${text}`));
    }
    case 'GET /outbox':
      return clone(outbox.filter((o) => o.status === params.get('status')));
    case 'POST /outbox/:id/resolve': {
      const row = outbox.find((o) => o.id === parts[1]);
      if (!row) throw not_found('Side effect');
      if (row.status !== 'unknown') throw new MockError(409, { error: { code: 'conflict', message: 'This side effect is not waiting for a decision.' } });
      const run_id = row.run_id;
      const t = tasks.find((x) => x.run?.id === run_id);
      if (!t || !run_id) throw not_found('Run');
      const resend = input.resolution === 'resend';
      row.status = 'delivered';
      event(run_id, resend ? 'outbox_resolved' : 'outbox_delivered', resend ? `Sent ${row.target} again · delivered` : `Marked ${row.target} as delivered by you`);
      if (row.step_key) step_done(t, row.step_key);
      return clone(t);
    }
    case 'POST /tasks/:id/pause':
    case 'POST /tasks/:id/resume': {
      const t = find_task(parts[1]);
      const pause = parts[2] === 'pause';
      if (t.run?.status !== (pause ? 'running' : 'waiting_approval')) throw new MockError(409, { error: { code: 'conflict', message: pause ? 'Only a running task can be paused.' : 'This task is not paused.' } });
      set_status(t, pause ? 'waiting_approval' : 'running');
      event(t.run?.id ?? t.id, 'intervention', pause ? 'Paused by you' : 'Resumed by you');
      return clone(t);
    }
    case 'GET /features': {
      // `?features_off=knowledge,canvas` tries the UI with features off (sun.yaml `features:`).
      const off = new URLSearchParams(location.search).get('features_off')?.split(',') ?? [];
      return Object.fromEntries(['knowledge', 'canvas', 'evals', 'market', 'catalog'].map((f) => [f, !off.includes(f)]));
    }
    case 'GET /usage': {
      if (params.get('by') === 'runtime')
        return RUNTIMES.map((r, i) => {
          const usage = { input_tokens: 412_000 * (3 - i), output_tokens: 38_500 * (3 - i), cost_usd: 6.4 * (3 - i) };
          const window = { input_tokens: 61_200 * (3 - i), output_tokens: 5_400 * (3 - i), cost_usd: 0.9 * (3 - i) };
          return { runtime: r.id, source: r.source, usage, quota_window: r.source === 'byo_runtime' ? { hours: 5, usage: window } : null };
        });
      const cost = tasks.find((x) => x.run?.id === params.get('run_id'))?.run?.cost_usd ?? 0;
      return { input_tokens: Math.round(cost * 120_000), output_tokens: Math.round(cost * 9_000), cost_usd: cost };
    }
    // --- Per workspace (M5): a token enrols a host into its workspace; attach and detach.
    case 'POST /workspaces/:id/hosts': // /workspaces/{id}/hosts/tokens
      return { token: 'sun_enrol_' + Math.random().toString(36).slice(2, 14), expires_at: new Date(Date.now() + 3600_000).toISOString() };
    case 'PUT /workspaces/:id/hosts':
    case 'DELETE /workspaces/:id/hosts': {
      const host = HOSTS.find((h) => h.id === parts[3]);
      if (!host) throw not_found('Host');
      host.workspace_ids = host.workspace_ids.filter((w) => w !== parts[1]).concat(method === 'PUT' ? [parts[1]] : []);
      return null;
    }
    // --- end per workspace (M5)
    case 'GET /runs/:id/events': {
      const after = Number(params.get('after') ?? 0);
      return clone((events.get(parts[1]) ?? []).filter((e) => e.seq > after));
    }
    case 'POST /workspaces/:id/knowledge_sources':
      link_source(parts[1], parts[3]);
      return null;
    case 'GET /workflows': {
      const workspace = params.get('workspace');
      return Object.values(WORKFLOWS)
        .filter((w) => !workspace || w.workspace_id === workspace)
        .map(({ id, name, workspace_id, version }) => ({ id, name, workspace_id, version }));
    }
    case 'GET /workflows/:id': {
      const w = Object.values(WORKFLOWS).find((x) => x.id === parts[1]);
      if (!w) throw not_found('Workflow');
      return { id: w.id, name: w.name, version: w.version, source_yaml: sources.get(w.id) ?? '' };
    }
    case 'POST /workflows/:id': // /workflows/validate, /workflows/dry-run
      if (parts[1] === 'dry-run') return dry_run(String(input.source_yaml ?? ''));
      return { errors: check(String(input.source_yaml ?? '')) };
    case 'POST /workflows':
      return save_source(null, String(input.source_yaml ?? ''), (input.workspace_id as string | undefined) ?? MAIN_WORKSPACE);
    case 'POST /workflows/:id/versions':
      return save_source(parts[1], String(input.source_yaml ?? ''));
    case 'GET /templates':
      return clone(TEMPLATES);
    case 'POST /templates/:id/install': {
      const t = TEMPLATES.find((x) => x.id === parts[1]);
      if (!t) throw not_found('Template');
      const workspace = params.get('workspace') ?? MAIN_WORKSPACE;
      if (Object.values(WORKFLOWS).some((w) => w.name === t.id && w.workspace_id === workspace)) throw new MockError(409, { error: { code: 'conflict', message: `This workspace already has a workflow named \`${t.id}\`.` } });
      return save_source(null, t.source_yaml, workspace);
    }
    case 'GET /runtimes':
      return clone(RUNTIMES);
    case 'PUT /runtimes/:id/policy': {
      const r = RUNTIMES.find((x) => x.id === parts[1]);
      if (!r) throw not_found('Runtime');
      const mode = (body as { permission_mode: PermissionMode }).permission_mode;
      if (mode === 'bypass_permissions' && !r.allow_bypass) throw new MockError(422, { error: { code: 'invalid_input', message: 'bypass_permissions needs runtimes.allow_bypass in sun.yaml.' } });
      r.permission_mode = mode;
      return { id: r.id, permission_mode: mode };
    }
    case 'GET /hosts': {
      const workspace = params.get('workspace');
      return clone(HOSTS.filter((h) => !workspace || h.workspace_ids.includes(workspace)));
    }
    case 'GET /kernel':
      return { pid: 4242, rss_bytes: 46 * MIB, cpu_percent: 0.6, uptime_s: 93_600 };
    case 'POST /sessions/:id/kill': {
      const reason = String(input.reason ?? '').trim();
      if (!reason) throw new MockError(422, { error: { code: 'invalid_input', message: 'reason: say why in 1 to 500 characters; the run records it.' } });
      const host = HOSTS.find((h) => h.session_stats?.some((s) => s.session_id === parts[1]));
      const session = host?.session_stats?.find((s) => s.session_id === parts[1]);
      if (!host || !session) throw not_found('Session');
      host.session_stats = host.session_stats?.filter((s) => s !== session);
      host.sessions = host.session_stats?.length ?? 0;
      const t = tasks.find((x) => x.run?.id === session.run_id);
      if (t) {
        set_status(t, 'waiting_approval');
        event(t.run?.id ?? t.id, 'intervention', `Session killed by you: ${reason}`);
      }
      return { run_id: session.run_id };
    }
    case 'GET /mods':
      // Like the kernel: installed ones carry the extensions that need them and any update.
      return clone(MODS.map((m) => (m.installed ? { ...m, needed_by: mod_dependents(m.name), update: m.update ?? null } : m)));
    case 'PUT /mods/:id': {
      const mod = MODS.find((m) => m.name === parts[1] && m.installed);
      if (!mod?.update) throw not_found('Mod update');
      const granted = [...((input as { permissions?: string[] }).permissions ?? [])].sort();
      if (granted.join() !== [...mod.update.permissions].sort().join())
        throw new MockError(422, { error: { code: 'invalid_input', message: `The permissions approved differ from what ${mod.name} ${mod.update.version} asks for. Reload and review them again.` } });
      Object.assign(mod, { version: mod.update.version, permissions: mod.update.permissions, update: null });
      return clone(mod);
    }
    case 'DELETE /mods/:id': {
      const mod = MODS.find((m) => m.name === parts[1] && m.installed);
      if (!mod) throw not_found('Mod');
      const needed_by = mod_dependents(mod.name);
      // A removed mod is listed as available again (the mock lists every mod in the registry).
      Object.assign(mod, { installed: false, signed: false, tier: mod.tier ?? 'official', update: null });
      return { removed: mod.name, needed_by };
    }
    case 'POST /runtimes': {
      // Same checks and field-prefixed messages as `Runtime::validate` in sun-core and the API.
      const { name = '', adapter = 'opencode', source = 'byo_key' } = input as Partial<CreateRuntime>;
      const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });
      if (!/^[a-z0-9_-]{1,64}$/.test(name)) throw invalid('name: 1–64 lowercase letters, digits, - or _, e.g. ollama-qwen');
      if (RUNTIMES.some((r) => r.id === name)) throw new MockError(409, { error: { code: 'conflict', message: `name: a runtime named \`${name}\` already exists.` } });
      if (source === 'credits') {
        if (!sun_cloud()) throw invalid('source: Sun credits need Sun Cloud; set SUN_CLOUD_URL and SUN_CLOUD_TOKEN on the kernel, or use a BYO key.');
        const model = String((input as { model?: string | null }).model ?? '').trim() || null;
        if (adapter === 'opencode' && !model) throw invalid('model: OpenCode on Sun credits needs a model from the price table');
        if (adapter === 'opencode' && model && (model.length > 200 || /\s/.test(model))) throw invalid('model: up to 200 characters, no spaces');
        if (adapter !== 'opencode' && model) throw invalid('model: only OpenCode takes a model; Claude Code and Codex pick their own');
        const r: Runtime = {
          id: name, name, adapter, source, status: 'ready', cooldown_until: null, sessions: 0, max_sessions: 0,
          permission_mode: null, allow_bypass: true, config: null, model,
        };
        RUNTIMES.push(r);
        return clone(r);
      }
      const { config } = input as { config?: ByoKeyConfig };
      const { kind = 'openai_compatible', endpoint = '', model = '', key_secret = null } = config ?? {};
      if (!BYO_KEY_ADAPTERS[kind]?.includes(adapter))
        throw invalid('adapter: opencode takes any key; claude-code takes an anthropic key, codex an openai_compatible one');
      if (!endpoint.trim() && kind === 'openai_compatible')
        throw invalid('endpoint: no endpoint; set an OpenAI-compatible base URL, e.g. http://localhost:11434/v1');
      if (endpoint.trim() && !/^https?:\/\/[^/@\s]+(\/\S*)?$/.test(endpoint.trim())) throw invalid('endpoint: must be an http(s) URL without credentials');
      if (!model.trim() || /\s/.test(model)) throw invalid('model: needs a model id without spaces, e.g. qwen2.5-coder:7b');
      if (key_secret !== null && !/^[A-Za-z_][A-Za-z0-9_]{0,63}$/.test(key_secret)) throw invalid('key_secret: a secret name is 1–64 letters, digits or _, not starting with a digit');
      const r: Runtime = {
        id: name, name, adapter, source: 'byo_key', status: 'offline', cooldown_until: null, sessions: 0, max_sessions: 0,
        permission_mode: null, allow_bypass: true, config: { kind, endpoint, model, key_secret },
      };
      r.status = mock_readiness(r).logged_in ? 'ready' : 'offline';
      RUNTIMES.push(r);
      return clone(r);
    }
    case 'GET /runtimes/:id/readiness': {
      const r = RUNTIMES.find((x) => x.id === parts[1]);
      if (!r) throw not_found('Runtime');
      return mock_readiness(r);
    }
    case 'POST /mods/:id/install': {
      // Same checks as sun-cli's KernelInstaller: listed, consent = exactly what it asks for.
      const mod = MODS.find((m) => m.name === parts[1] && !m.installed);
      if (!mod) throw not_found('Mod');
      const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });
      const granted = [...((input as { permissions?: string[] }).permissions ?? [])].sort();
      if (granted.join() !== [...mod.permissions].sort().join())
        throw invalid(`The permissions approved differ from what ${mod.name} ${mod.version} asks for (${mod.permissions.join(', ') || 'nothing'}). Reload and review them again.`);
      Object.assign(mod, { installed: true });
      return clone(mod);
    }
    case 'GET /catalog': {
      const stacks = params.get('repo') ? ['frontend', 'node', 'svelte'] : [];
      const entries = CATALOG.map((e) => ({ ...e, matches: e.stacks.filter((t) => stacks.includes(t)) }));
      const repo_id = params.get('repo_id');
      return clone({
        stacks,
        entries: entries.sort((a, b) => b.matches.length - a.matches.length),
        dismissed: [...DISMISSED].sort(),
        ...(repo_id ? { repo_installs: [...SKILL_PRS.values()].filter((p) => p.repo_id === repo_id) } : {}),
      });
    }
    case 'PUT /catalog/:id/dismissed':
      DISMISSED.add(parts[1]);
      return null;
    case 'DELETE /catalog/:id/dismissed':
      DISMISSED.delete(parts[1]);
      return null;
    case 'GET /catalog/:id/files': {
      const e = catalog_entry(parts[1]);
      const main = { path: e.kind === 'skill' ? 'SKILL.md' : `${e.id}.${e.kind === 'mcp' ? 'json' : 'yaml'}`, text: `---\nname: ${e.id}\ndescription: ${e.description}\n---\n\n# ${e.id}\n` };
      if (!e.installed) return [{ ...main, change: null }];
      if (e.installed === e.version) return [{ ...main, change: 'unchanged' }];
      // An update: the kernel compares each file with the installed copy.
      return [
        { ...main, change: 'changed', diff: main.text.split('\n').slice(0, -1).flatMap((l) => (l.startsWith('description:') ? [`-description: ${e.id}, older wording`, `+${l}`] : [` ${l}`])) },
        { path: 'tests.md', text: '# Good tests\n\nTest behaviour through public interfaces.\n', change: 'added' },
        { path: 'mocking.md', text: '', change: 'removed' },
        { path: 'LICENSE', text: 'MIT License\n', change: 'unchanged' },
      ];
    }
    case 'POST /catalog/:id/install': {
      const e = catalog_entry(parts[1]);
      const runtimes = (body as { runtimes?: string[] } | null)?.runtimes;
      if (runtimes && !runtimes.length) throw invalid('runtimes: pick at least one runtime');
      if (runtimes && e.kind !== 'skill') throw invalid(`catalog entry \`${e.id}\`: only skills are installed per runtime`);
      const repo_id = (body as { repo_id?: string } | null)?.repo_id;
      if (repo_id) {
        if (e.kind !== 'skill') throw invalid(`catalog entry \`${e.id}\`: only skills are installed into a repo`);
        const repo = REPOS.find((r) => r.id === repo_id);
        if (!repo) throw not_found('Repo');
        const key = `${e.id}:${repo_id}`;
        const had = SKILL_PRS.get(key);
        if (had?.version === e.version && had.pr) return clone(had);
        const pr: SkillPr = { repo_id, skill: e.id, version: e.version, branch: `sun/skill-${e.id}`, task_id: had?.task_id ?? fake_id('SK', SKILL_PRS.size + 1), pr: { status: 'pending', url: null } };
        SKILL_PRS.set(key, pr);
        return clone(pr);
      }
      e.installed = e.version;
      if (e.kind === 'skill') e.runtimes = runtimes ?? null;
      return null;
    }
    case 'GET /catalog/:id/repos': {
      const pr = SKILL_PRS.get(`${parts[1]}:${parts[3]}`);
      if (!pr) throw not_found('Repo install');
      if (pr.pr?.status === 'pending') {
        const slug = REPOS.find((r) => r.id === pr.repo_id)?.url.replace(/^https:\/\/github\.com\/|\.git$/g, '');
        pr.pr = { status: 'delivered', url: `https://github.com/${slug}/pull/42` };
      }
      return clone(pr);
    }
    case 'GET /tasks/:id/quality':
      return quality_of(find_task(parts[1]));
    case 'POST /tasks/:id/quality': {
      const q = quality_of(find_task(parts[1])).quality;
      if (!q) throw new MockError(422, { error: { code: 'invalid_input', message: 'This task has no merged PR yet.' } });
      return q;
    }
    case 'GET /quality/:id':
      return clone(METRICS);
    case 'GET /tasks/:id/knowledge_flags':
      return find_task(parts[1]).key === 'SUN-139' ? clone(PRICING_FLAGS) : [];
    case 'GET /tasks/:id/corrections':
      return clone(corrections_of(find_task(parts[1])));
    case 'GET /tasks/:id/previews': {
      const t = find_task(parts[1]);
      return (['before', 'after'] as const).filter((side) => previews.has(`${t.id}/${side}`)).map((side) => preview(t, side));
    }
    case 'POST /tasks/:id/previews': {
      const t = find_task(parts[1]);
      const side = input.side as PreviewSide;
      if (side !== 'before' && side !== 'after') throw new MockError(422, { error: { code: 'invalid_input', message: 'Pick before or after.' } });
      if (!t.run?.repo_ids.length) throw new MockError(422, { error: { code: 'invalid_input', message: 'the repo declares no preview: add a `.sun-preview` file with the dev server command' } });
      previews.add(`${t.id}/${side}`);
      return preview(t, side);
    }
    case 'GET /tasks/:id/design':
      return design_of(find_task(parts[1]));
    case 'DELETE /tasks/:id/previews': {
      const t = find_task(parts[1]);
      previews.delete(`${t.id}/before`);
      previews.delete(`${t.id}/after`);
      return null;
    }
    case 'GET /tasks/:id/pins': {
      const t = find_task(parts[1]);
      return clone(pins.filter((p) => p.task_id === t.id));
    }
    case 'POST /tasks/:id/pins': {
      const t = find_task(parts[1]);
      const now = new Date().toISOString();
      if (parts[4] === 'commit') return commit_pin(find_pin(parts[1], parts[3]));
      // The host's crop lands right away here; on a kernel it follows in a few seconds.
      const p: Pin = {
        id: fake_id('P', pins.length + 1), task_id: t.id, status: 'open', comment: '', edits: [],
        target: input.target as PinTarget, before_crop: `crop-${pins.length + 1}-before`, after_crop: null,
        created_at: now, updated_at: now,
      };
      pins.push(p);
      if (t.run) event(t.run.id, 'pin', `Pinned ${p.target.tag} on the canvas`);
      return clone(p);
    }
    case 'PATCH /tasks/:id/pins': {
      const p = find_pin(parts[1], parts[3]);
      Object.assign(p, input, { updated_at: new Date().toISOString() });
      if (p.status === 'done') p.after_crop ??= p.before_crop?.replace('before', 'after') ?? null;
      return clone(p);
    }
    case 'POST /tasks/:id/screens': {
      const t = find_task(parts[1]);
      const widths = (input.widths as number[] | undefined) ?? [];
      if (!t.run) throw new MockError(422, { error: { code: 'invalid_input', message: 'The task has no run to attach screenshots to.' } });
      const paths = (input.paths as string[] | undefined)?.length ? (input.paths as string[]) : ['/'];
      const shots = paths.flatMap((path) =>
        widths.flatMap((width) =>
          (['before', 'after'] as const).map((side) => ({ name: `${side}-${width}-${path.slice(1) || 'home'}.png`, blob: `shot-${side}-${width}`, side, path, width })),
        ),
      );
      event(t.run.id, 'canvas_screens', `Canvas screenshots attached · ${shots.length} image(s)`);
      return shots;
    }
    case 'DELETE /tasks/:id/pins': {
      const p = find_pin(parts[1], parts[3]);
      pins.splice(pins.indexOf(p), 1);
      return null;
    }
    case 'GET /mcp_servers':
      return mcp_servers();
    case 'POST /mcp_servers':
      return add_mcp_server(input as unknown as McpServer);
    case 'GET /mcp_servers/:id/tools':
      return mcp_tools(mcp_server(parts[1]).name);
    case 'DELETE /mcp_servers/:id': {
      const name = mcp_server(parts[1]).name;
      const i = MCP_SERVERS.findIndex((s) => s.name === name);
      if (i >= 0) MCP_SERVERS.splice(i, 1);
      else catalog_entry(name).installed = null;
      return null;
    }
    case 'GET /tasks/:id/mcp':
      return clone(MCP_GRANTS.get(find_task(parts[1]).id) ?? []);
    case 'PUT /tasks/:id/mcp':
      return set_task_mcp(parts[1], body as McpGrant[]);
    case 'DELETE /catalog/:id': {
      const e = catalog_entry(parts[1]);
      if (!e.installed) throw not_found(`${e.id} install`);
      e.installed = null;
      return null;
    }
  }
  throw not_found(`${method} ${path}`);
}
