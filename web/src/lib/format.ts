// Display rules shared by every screen: task columns, status labels, money, time.
import type { Agent, OnLimit, Priority, RunEvent, Runtime, RuntimeSource, StepStatus, TaskStatus, TaskSummary, WorkflowTemplate } from './api/types';

/** `list` with `item` added (`on`) or removed, once. */
export const toggle = <T,>(list: T[], item: T, on: boolean) => (on ? [...list.filter((x) => x !== item), item] : list.filter((x) => x !== item));

type Tone = 'neutral' | 'primary' | 'success' | 'warning' | 'danger' | 'info';

/** Role names for templates and the role view. */
export const ROLE: Record<WorkflowTemplate['role'], string> = {
  engineering: 'Engineering',
  marketing: 'Marketing',
  product: 'Product',
  leadership: 'Leadership',
  'forward-deployed': 'Forward-deployed',
};

/** What a runtime limit does, in plain words. */
export const ON_LIMIT: { value: OnLimit; label: string; description: string }[] = [
  { value: 'switch', label: 'Switch to the next runtime', description: 'Work keeps going on another runtime with a hand-off pack. Fastest.' },
  { value: 'pause_and_notify', label: 'Pause and tell me', description: 'The task waits, and you pick: switch now or resume after the reset.' },
  { value: 'wait_until_reset', label: 'Wait for the reset', description: 'The same runtime picks up when its limit resets. Slowest, most consistent.' },
];

/** A correction cause's badge: tone and text, so meaning never rests on colour alone. */
export const CAUSE_BADGE = {
  test_failure: ['danger', 'Tests'],
  lint: ['warning', 'Lint'],
  missing_requirement: ['warning', 'Missing requirement'],
  wrong_scope: ['warning', 'Wrong scope'],
  style: ['info', 'Style'],
  other: ['neutral', 'Other'],
} as const;

/** The Tasks columns, one per status; Cancelled is hidden unless asked for. */
export const COLUMNS: { status: TaskStatus; label: string; hint: string; hidden?: boolean }[] = [
  { status: 'backlog', label: 'Backlog', hint: 'Not started' },
  { status: 'todo', label: 'Todo', hint: 'Waiting for a free runtime' },
  { status: 'in_progress', label: 'In progress', hint: 'Agents at work' },
  { status: 'verifying', label: 'Verifying', hint: 'Engine runs the gates' },
  { status: 'needs_you', label: 'Needs you', hint: 'Approve, answer or review' },
  { status: 'done', label: 'Done', hint: 'Reviewed and finished' },
  { status: 'blocked', label: 'Blocked', hint: 'Stopped or paused' },
  { status: 'failed', label: 'Failed', hint: 'Retry or reopen' },
  { status: 'cancelled', label: 'Cancelled', hint: 'Abandoned', hidden: true },
];

export const STATUS_BADGE: Record<TaskStatus, [Tone, string]> = {
  backlog: ['neutral', 'Backlog'],
  todo: ['neutral', 'Todo'],
  in_progress: ['info', 'In progress'],
  verifying: ['primary', 'Verifying'],
  needs_you: ['warning', 'Needs you'],
  done: ['success', 'Done'],
  blocked: ['warning', 'Blocked'],
  failed: ['danger', 'Failed'],
  cancelled: ['neutral', 'Cancelled'],
};

/** A status as an icon, beside its label in lists and menus. */
export const STATUS_ICON: Record<TaskStatus, string> = {
  backlog: 'circle-dashed',
  todo: 'circle',
  in_progress: 'circle-dot',
  verifying: 'scan-search',
  needs_you: 'user-check',
  done: 'circle-check',
  blocked: 'circle-pause',
  failed: 'circle-x',
  cancelled: 'circle-slash',
};

/** Queue order, higher first (`Priority::rank`): urgent, high, medium, low, then no priority. */
export const PRIORITY_RANK: Record<Priority, number> = { urgent: 4, high: 3, medium: 2, low: 1, none: 0 };

/** Priorities in rank order, with their label and icon. */
export const PRIORITIES: { value: Priority; label: string; icon: string }[] = [
  { value: 'urgent', label: 'Urgent', icon: 'circle-alert' },
  { value: 'high', label: 'High', icon: 'signal-high' },
  { value: 'medium', label: 'Medium', icon: 'signal-medium' },
  { value: 'low', label: 'Low', icon: 'signal-low' },
  { value: 'none', label: 'No priority', icon: 'minus' },
];
export const priority_of = (p: Priority) => PRIORITIES.find((x) => x.value === p) ?? PRIORITIES[4];

/** Who works on a task: an agent (named from `agents`; an archived one isn't listed), its workflow, or
 *  nobody (a plain task). `key` groups swimlanes. */
export function assignee_of(t: TaskSummary, agents: Agent[]): { kind: 'agent' | 'workflow' | 'none'; key: string; label: string; icon: string } {
  if (t.assignee?.kind === 'agent') {
    const id = t.assignee.id;
    return { kind: 'agent', key: `agent:${id}`, label: agents.find((a) => a.id === id)?.name ?? 'Archived agent', icon: 'bot' };
  }
  if (t.workflow) return { kind: 'workflow', key: `workflow:${t.workflow.id}`, label: t.workflow.name, icon: 'workflow' };
  return { kind: 'none', key: 'none', label: 'No assignee', icon: 'minus' };
}

/** Watchdog states that need a human. */
export const STUCK: Partial<Record<StepStatus, string>> = {
  stalled: 'Stalled',
  looping: 'Looping',
  budget_exceeded: 'Over budget',
  interrupted: 'Host lost',
};

export const current_step = (t: TaskSummary) =>
  t.run?.steps.find((s) => s.status !== 'succeeded' && s.status !== 'queued') ?? null;

export const stuck_label = (t: TaskSummary) => {
  const step = current_step(t);
  return step ? (STUCK[step.status] ?? null) : null;
};

/** Tasks waiting on a human approval step. A paused run also reads `waiting_approval`
 *  but has no step waiting, so it is not one of these. */
export const needs_approval = (t: TaskSummary) =>
  t.status === 'needs_you' && !!t.run?.steps.some((s) => s.status === 'waiting_approval');

/** A paused run (Stop, a limit, a pause): shows Resume instead of Approve. */
export const is_paused = (t: TaskSummary) =>
  t.run?.status === 'waiting_approval' && !t.run.steps.some((s) => s.status === 'waiting_approval');

export const SOURCE_LABEL: Record<RuntimeSource, string> = {
  byo_runtime: 'Your subscription',
  byo_key: 'Your API key',
  credits: 'Sun credits',
};

/** Product names of the built-in CLIs; any other tool shows its id. */
const TOOL_LABEL: Record<string, string> = { 'claude-code': 'Claude Code', codex: 'Codex', opencode: 'OpenCode' };
export const tool_label = (tool: string) => TOOL_LABEL[tool] ?? tool;

/** What a chain entry (`claude-code`, `ollama-qwen`) is called: a host CLI by its product name, a key-backed runtime by its own. */
export const runtime_name = (runtimes: Runtime[], name: string | null) => {
  if (name === null) return '—';
  const r = runtimes.find((x) => x.name === name);
  return r?.host ? tool_label(r.tool) : (r?.name ?? tool_label(name));
};

/** The distinct chain names of a runtime list (one per tool, however many hosts have it). */
export const chain_names = (runtimes: Runtime[]) => [...new Set(runtimes.map((r) => r.name))];

export const money = (usd: number) => `$${usd.toFixed(2)}`;

/** A price in the currency's minor units (whole rupiah, US cents): Rp 199.000, $12.00. */
export function price(currency: string, minor: number): string {
  const idr = currency.toLowerCase() === 'idr';
  const format = new Intl.NumberFormat(idr ? 'id-ID' : 'en-US', { style: 'currency', currency: currency.toUpperCase(), maximumFractionDigits: idr ? 0 : 2 });
  return format.format(idr ? minor : minor / 100);
}

const compact = new Intl.NumberFormat('en', { notation: 'compact', maximumFractionDigits: 1 });
export const tokens = (n: number) => compact.format(n);

/** Bytes as people read them: 512 KB, 312 MB, 1.2 GB. */
export function bytes(n: number): string {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${i === 0 || n >= 100 ? Math.round(n) : n.toFixed(1)} ${units[i]}`;
}

/** A duration in seconds, largest unit first: 42s, 4m, 3h 5m, 2d 1h. */
export function uptime(s: number): string {
  const [d, h, m] = [Math.floor(s / 86400), Math.floor((s % 86400) / 3600), Math.floor((s % 3600) / 60)];
  if (d) return `${d}d ${h}h`;
  if (h) return `${h}h ${m}m`;
  return m ? `${m}m` : `${s}s`;
}

/** The date part of an RFC 3339 timestamp, as citations show it (2026-09-30). */
export const day = (at: string) => at.slice(0, 10);

/** `owner/name` of a clone URL (`https://host/owner/name.git`, `git@host:owner/name.git`). */
export const repo_name = (url: string) => url.replace(/^[a-z]+:\/\/[^/]+\/|^[^@]+@[^:]+:/, '').replace(/\.git$/, '');

/** Host and path of a clone URL, so the HTTPS and SSH forms of one repository compare equal;
 *  null for anything that isn't a git URL (http(s), ssh, git, or `user@host:path`). */
export function repo_identity(raw: string): string | null {
  const value = raw.trim();
  let host = '';
  let path = '';
  const scp = !value.includes('://') && value.match(/^(?:[^@\s/]+@)?([^:\s/]+):(.+)$/);
  if (scp) [host, path] = [scp[1], scp[2]];
  else {
    try {
      const url = new URL(value);
      if (!['http:', 'https:', 'ssh:', 'git:'].includes(url.protocol)) return null;
      [host, path] = [url.host, url.pathname];
    } catch {
      return null;
    }
  }
  path = path.replace(/^\/+|\/+$/g, '').replace(/\.git$/i, '');
  return host && path ? `${host.toLowerCase()}/${path}` : null;
}

/** The host's icon for a repository row: `github`, `gitlab`, or a plain git folder. */
export const repo_icon = (url: string) => {
  const host = repo_identity(url)?.split('/')[0] ?? '';
  return host.includes('github') ? 'github' : host.includes('gitlab') ? 'gitlab' : 'folder-git-2';
};

/** The slug a workspace named `name` starts from, as the kernel makes it (`Workspace::slug_from_name`). */
export const slug_from_name = (name: string) =>
  name.replace(/[^A-Za-z0-9]+/g, '-').toLowerCase().replace(/^-+/, '').slice(0, 48).replace(/-+$/, '') || 'workspace';

/** Its default issue prefix: the slug's first four letters or digits, uppercased (`Workspace::prefix_from_slug`). */
export const prefix_from_slug = (slug: string) => slug.replace(/[^a-z0-9]/gi, '').slice(0, 4).toUpperCase();

/** A workspace slug the kernel accepts: 1–48 lowercase letters or digits in words joined by single `-`. */
export const SLUG = /^(?=.{1,48}$)[a-z0-9]+(-[a-z0-9]+)*$/;

/** A link the page may open: http(s) only, since connector text is untrusted. */
export const safe_url = (url: string | null | undefined) => (url && /^https?:\/\//.test(url) ? url : null);

/** A citation's trailing url (`title > heading · version · date · url`) split off as a link. */
export function cited(citation: string): { text: string; url: string | null } {
  const at = citation.lastIndexOf(' · ');
  const url = safe_url(citation.slice(at + 3));
  return at > 0 && url ? { text: citation.slice(0, at), url } : { text: citation, url: null };
}

/** A whole-number field (an `<input type="number">` binds a number, or null/'' when empty):
 *  null when empty, NaN when not a positive whole number. */
export function whole(value: unknown): number | null {
  const text = value == null ? '' : String(value).trim();
  if (!text) return null;
  const n = Number(text);
  return Number.isInteger(n) && n > 0 ? n : NaN;
}

/** A dollar amount from a form field: `null` when empty, `NaN` unless a number of 0 or more. */
export function usd(value: unknown): number | null {
  const text = value == null ? '' : String(value).trim();
  if (!text) return null;
  const n = Number(text);
  return Number.isFinite(n) && n >= 0 ? n : NaN;
}

export const clock = (at: string) =>
  new Date(at).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false });

/** Date and time, e.g. "Oct 10, 14:05". */
export const stamp = (at: string) =>
  new Date(at).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false });

const relative = new Intl.RelativeTimeFormat([], { numeric: 'auto' });
const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [['year', 31_536_000], ['month', 2_592_000], ['week', 604_800], ['day', 86_400], ['hour', 3_600], ['minute', 60]];

/** How long ago `at` was, in words: "3 hours ago", "yesterday", "just now". */
export function time_ago(at: string, now = Date.now()): string {
  const seconds = Math.round((new Date(at).getTime() - now) / 1000);
  const unit = UNITS.find(([, size]) => Math.abs(seconds) >= size);
  return unit ? relative.format(Math.trunc(seconds / unit[1]), unit[0]) : 'just now';
}

/** Who did it, from an audit actor (`user:ana`): "You" for `me` (the signed-in user), "Sun" without one (the engine). */
export function actor_name(by: unknown, me: string | undefined): string {
  if (typeof by !== 'string' || !by) return 'Sun';
  const name = by.replace(/^[a-z_]+:/, '');
  return name === me ? 'You' : name;
}

/** How full the latest agent session's context window is, read from the timeline:
 *  `session_assigned` carries the window and threshold, `usage` the tokens in context,
 *  `rotation` counts the fresh sessions Sun started. `null`: no session reported its context yet. */
export interface ContextFill {
  used: number;
  /** Tokens; `null` when the runtime doesn't say (no bar, no rotation). */
  window: number | null;
  /** Whole percent of the window; `null` without a window. */
  percent: number | null;
  rotate_at: number;
  rotations: number;
}

export function context_fill(events: RunEvent[]): ContextFill | null {
  let fill: ContextFill | null = null;
  let window: number | null = null;
  let rotate_at = 80;
  let rotations = 0;
  for (const e of events) {
    const p = e.payload as Record<string, unknown>;
    if (e.kind === 'session_assigned') {
      window = typeof p.context_window === 'number' && p.context_window > 0 ? p.context_window : null;
      rotate_at = typeof p.rotate_at_percent === 'number' ? p.rotate_at_percent : 80;
      fill = null; // a new session starts empty
    } else if (e.kind === 'usage' && typeof p.context_used === 'number') {
      const percent = window ? Math.floor((p.context_used * 100) / window) : null;
      fill = { used: p.context_used, window, percent, rotate_at, rotations };
    } else if (e.kind === 'rotation') {
      rotations += 1;
    }
  }
  return fill && { ...fill, rotations };
}
