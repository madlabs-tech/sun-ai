// Mirrored pull requests for the mock: Main's PRs (open with checks, draft, merged, closed, failing,
// and one nobody linked yet), their links to tasks, and the kernel's rules: link by URL (404 for a PR
// Sun never received), remove (remembered, so a webhook wouldn't link it again), the per-task
// auto-complete switch, and the decision (multica's `decidePRAutoComplete`) that moves a task once every
// linked PR is merged: a workflow task to Done, an agent's to Needs you.
import type { LinkSource, PrAutoComplete, PullRequest, TaskDetail, TaskPullRequests, TaskStatus } from './types';
import { MockError, clone, find_task, find_workspace, log, not_found } from './mock';

const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });
const today = (hm: string) => `2026-10-07T${hm}:00Z`;
const id_of = (n: number) => `01JAPR${String(n).padStart(4, '0')}`.padEnd(26, '0');

type Row = Omit<PullRequest, 'link_source'>;

const base = (n: number, repo: string, title: string, state: PullRequest['state'], at: string): Row => ({
  id: id_of(n),
  provider: 'github',
  repo_owner: 'madlabs',
  repo_name: repo,
  number: n,
  title,
  state,
  html_url: `https://github.com/madlabs/${repo}/pull/${n}`,
  branch: null,
  author_login: 'ada-bot',
  author_avatar_url: null,
  merged_at: state === 'merged' ? at : null,
  closed_at: state === 'merged' || state === 'closed' ? at : null,
  pr_created_at: today('06:00'),
  pr_updated_at: at,
  additions: 0,
  deletions: 0,
  changed_files: 0,
  mergeable: null,
  merge_state_status: null,
  checks_rollup_state: null,
  checks_total: 0,
  checks_passed: 0,
  checks_failed: 0,
  checks_pending: 0,
  snapshot_available: true,
});

const PULL_REQUESTS: Row[] = [
  // Open, checks still running, branch-named after the task.
  { ...base(470, 'api', 'SUN-142: Fix login timeout on mobile', 'open', today('09:40')), branch: 'sun/SUN-142-fix-login-timeout-on-mobile', additions: 312, deletions: 48, changed_files: 6, mergeable: 'mergeable', merge_state_status: 'blocked', checks_rollup_state: 'pending', checks_total: 5, checks_passed: 4, checks_pending: 1 },
  // A draft, keyed in its title.
  { ...base(471, 'mobile', 'SUN-142 Retry the session refresh on timeout', 'draft', today('09:50')), branch: 'mobile-retry', additions: 40, deletions: 3, changed_files: 2, merge_state_status: 'draft' },
  // Merged, linked by "Closes SUN-136" in its body.
  { ...base(477, 'mobile', 'Bump wallet SDK to v2.3', 'merged', today('07:40')), branch: 'sun/SUN-136-bump-wallet-sdk-to-v2-3', additions: 120, deletions: 80, changed_files: 9, merge_state_status: 'clean', checks_rollup_state: 'success', checks_total: 3, checks_passed: 3 },
  // Closed without merging.
  { ...base(463, 'api', 'Retry failed webhook deliveries', 'closed', today('08:15')), branch: 'sun/SUN-134-retry-failed-webhook-deliveries', additions: 88, deletions: 12, changed_files: 4 },
  // Open, a failing check and a conflict.
  { ...base(466, 'web', 'SUN-139: Sync pricing page with new plans', 'open', today('10:05')), branch: 'sun/SUN-139-sync-pricing-page-with-new-plans', additions: 210, deletions: 95, changed_files: 11, mergeable: 'conflicting', merge_state_status: 'dirty', checks_rollup_state: 'failure', checks_total: 4, checks_passed: 3, checks_failed: 1 },
  // Mirrored, open, no checks, linked to nothing yet: paste its URL on a task.
  { ...base(480, 'web', 'Pricing page: new plans table', 'open', today('10:20')), branch: 'pricing-table', additions: 64, deletions: 20, changed_files: 3, mergeable: 'mergeable', merge_state_status: 'clean' },
];

/** `task id → pr id → how it got there`; the seeds link by key once the tasks exist. */
const links = new Map<string, Map<string, LinkSource>>();
const excluded = new Set<string>();
const disabled = new Set<string>();
let seeded = false;

function seed() {
  if (seeded) return;
  seeded = true;
  const seeds: [key: string, n: number, source: LinkSource][] = [
    ['SUN-142', 470, 'branch'],
    ['SUN-142', 471, 'title'],
    ['SUN-136', 477, 'auto'],
    ['SUN-134', 463, 'manual'],
    ['SUN-139', 466, 'title'],
  ];
  for (const [key, n, source] of seeds) links_of(find_task(key).id).set(id_of(n), source);
}

function links_of(task_id: string): Map<string, LinkSource> {
  const map = links.get(task_id) ?? new Map<string, LinkSource>();
  links.set(task_id, map);
  return map;
}

/** CI in the mock takes 20 s from the page load: a PR's pending checks pass once that is over, like a
 *  later snapshot from the host. */
const started = Date.now();
const snapshot = (pr: Row): Row =>
  pr.checks_pending > 0 && Date.now() - started >= 20_000
    ? { ...pr, checks_pending: 0, checks_passed: pr.checks_total, checks_rollup_state: 'success', merge_state_status: 'clean' }
    : pr;

const linked = (t: TaskDetail): PullRequest[] =>
  [...links_of(t.id)].flatMap(([pr_id, link_source]) => {
    const pr = PULL_REQUESTS.find((p) => p.id === pr_id);
    return pr ? [{ ...snapshot(pr), link_source }] : [];
  });

/** Where a merge moves the task (decision 6): a workflow task to Done, an agent's to Needs you. */
const target_of = (t: TaskDetail): TaskStatus => (t.assignee?.kind === 'agent' ? 'needs_you' : 'done');

function decide(t: TaskDetail, prs: PullRequest[]): PrAutoComplete {
  const workspace_enabled = find_workspace(t.workspace_id).git_settings.move_on_merge;
  const d: PrAutoComplete = { state: 'none', pull_request_ids: [], task_disabled: disabled.has(t.id), workspace_enabled, target_status: target_of(t) };
  if (!prs.length) return d;
  if (t.status === 'done' || t.status === 'cancelled') return { ...d, state: 'terminal' };
  if (!workspace_enabled) return { ...d, state: 'workspace_disabled' };
  if (d.task_disabled) return { ...d, state: 'task_disabled' };
  if (t.status === d.target_status) return { ...d, state: 'at_target' };
  const open = prs.filter((pr) => pr.state === 'open' || pr.state === 'draft');
  const closed = prs.filter((pr) => pr.state === 'closed');
  const about = open.length ? open : closed.length ? closed : prs;
  return { ...d, state: open.length ? 'waiting' : closed.length ? 'not_merged' : 'all_merged', pull_request_ids: about.map((pr) => pr.id) };
}

/** A PR event touched the task: every linked PR merged moves it (a guarded direct write, logged as `pr_automation`). */
function view(t: TaskDetail): TaskPullRequests {
  const prs = linked(t);
  let auto = decide(t, prs);
  if (auto.state === 'all_merged') {
    log(t, 'status', { from: t.status, to: auto.target_status, source: 'pr_automation', pull_requests: prs.map((pr) => `#${pr.number}`).join(', ') });
    t.status = auto.target_status;
    t.updated_at = new Date().toISOString();
    auto = decide(t, prs);
  }
  return clone({ pull_requests: prs, auto_complete: auto });
}

/** multica's manual link URL rule: the PR page, with any query or fragment dropped. */
const normalize = (raw: string) => raw.trim().match(/^(https?:\/\/[^/?#]+\/.+?\/(?:pull|pulls|merge_requests)\/\d+)(?:[/?#].*)?$/)?.[1];

/** `GET|POST /tasks/:id/pull_requests`, `DELETE …/:pr`, `PUT /tasks/:id/pr_auto_complete`. */
export function pull_request_route(method: string, parts: string[], input: Record<string, unknown>): unknown {
  seed();
  const t = find_task(parts[1]);
  if (parts[2] === 'pr_auto_complete' && method === 'PUT') {
    if (typeof input.disabled !== 'boolean') throw invalid('disabled: use true or false');
    if (input.disabled) disabled.add(t.id);
    else disabled.delete(t.id);
    return view(t);
  }
  if (parts[2] !== 'pull_requests') throw not_found('route');
  if (method === 'GET') return view(t);
  if (method === 'POST') {
    const url = typeof input.url === 'string' ? normalize(input.url) : undefined;
    const pr = typeof input.pull_request_id === 'string' ? PULL_REQUESTS.find((p) => p.id === input.pull_request_id) : PULL_REQUESTS.find((p) => url && p.html_url === url);
    if (typeof input.url === 'string' && !url) throw invalid('Paste the URL of a pull request or merge request.');
    if (!pr) throw not_found('pull request');
    links_of(t.id).set(pr.id, 'manual');
    excluded.delete(`${t.id}:${pr.id}`);
    log(t, 'pull_request', { action: 'linked', number: pr.number, url: pr.html_url, repo: `${pr.repo_owner}/${pr.repo_name}`, source: 'manual' }, undefined, true);
    return view(t);
  }
  if (method === 'DELETE' && parts[3]) {
    const pr = PULL_REQUESTS.find((p) => p.id === parts[3]);
    if (!pr || !links_of(t.id).delete(pr.id)) throw not_found('pull request');
    excluded.add(`${t.id}:${pr.id}`);
    log(t, 'pull_request', { action: 'unlinked', number: pr.number, url: pr.html_url, repo: `${pr.repo_owner}/${pr.repo_name}`, source: 'manual' }, undefined, true);
    return view(t);
  }
  throw not_found('route');
}
