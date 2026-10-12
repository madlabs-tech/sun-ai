// In-memory plan and secrets for the mock: `entitlement`, `secrets/**`.
// Like the kernel, no answer ever carries a secret value.
import { MAIN_WORKSPACE, type Entitlement, type OnLimit, type Secret, type SecretScope, type Spend } from './types';
import { MockError, clone, not_found } from './mock';

const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });
const NAME = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;

const UNLIMITED = { seats: null, runtimes: null, byo_keys: null, agents: null, hosts: null };
// Main is on Pro from Sun Cloud; every other workspace is Community.
const pro: Entitlement = {
  plan: 'pro',
  paid: true,
  limits: { ...UNLIMITED, seats: 5, byo_keys: 2, hosts: 2 },
  status: 'cloud',
  valid_until: new Date(Date.now() + 5 * 60_000).toISOString(),
};
const community: Entitlement = { plan: 'community', paid: false, limits: UNLIMITED, status: 'community', valid_until: null };
const secrets: (Secret & { value: string })[] = [
  { id: 'sec_1', scope: { kind: 'mod', id: 'github' }, name: 'GITHUB_TOKEN', updated_at: '2026-10-01T09:00:00Z', value: 'ghp_sample' },
];

/** An ecosystem or workspace secret of that name is stored (BYO key readiness, MCP server checks). */
export const has_ecosystem_secret = (name: string) => secrets.some((s) => (s.scope.kind === 'ecosystem' || s.scope.kind === 'workspace') && s.name === name);

// A set-up ecosystem answers `switch`; `?fresh=1` starts like a new install, before onboarding asked.
let on_limit: OnLimit | null = new URLSearchParams(location.search).has('fresh') ? null : 'switch';
const task_on_limit = new Map<string, OnLimit | null>();
const POLICIES: (OnLimit | null)[] = ['switch', 'pause_and_notify', 'wait_until_reset', null];

/** `GET|PUT /settings/on_limit` and `/tasks/{id}/on_limit`. */
export function on_limit_route(method: string, parts: string[], input: Record<string, unknown>): unknown {
  const task = parts[0] === 'tasks' ? parts[1] : null;
  if (method === 'PUT') {
    const value = (input.on_limit ?? null) as OnLimit | null;
    if (!POLICIES.includes(value)) throw invalid(`on_limit: unknown variant \`${value}\``);
    if (task) task_on_limit.set(task, value);
    else on_limit = value;
  }
  return { on_limit: task ? (task_on_limit.get(task) ?? null) : on_limit };
}

// A set-up ecosystem has a $50 cap with $12.40 spent; `?fresh=1` has none.
const month_start = () => {
  const now = new Date();
  return new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), 1)).toISOString();
};
const spend: Spend = { monthly_usd: new URLSearchParams(location.search).has('fresh') ? null : 50, spent_usd: 12.4, since: month_start() };
const task_budget = new Map<string, number | null>();

/** A USD amount from the API: `null` clears; else a number of 0 or more (as `budget::checked`). */
function usd(value: unknown, field: string): number | null {
  if (value === null || value === undefined) return null;
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) throw invalid(`${field}: use an amount in USD of 0 or more`);
  return value;
}

/** `GET|PUT /settings/budget` and `/tasks/{id}/budget`. */
export function budget_route(method: string, parts: string[], input: Record<string, unknown>): unknown {
  const task = parts[0] === 'tasks' ? parts[1] : null;
  if (task) {
    if (method === 'PUT') task_budget.set(task, usd(input.cost_usd, 'cost_usd'));
    return { cost_usd: task_budget.get(task) ?? null };
  }
  if (method === 'PUT') spend.monthly_usd = usd(input.monthly_usd, 'monthly_usd');
  return clone(spend);
}

const view = ({ value: _, ...meta }: Secret & { value: string }): Secret => clone(meta);

export function settings_route(method: string, parts: string[], params: URLSearchParams, input: Record<string, unknown>): unknown {
  const [head, kind, id, name] = parts;
  if (head === 'entitlement' && method === 'GET') return clone((params.get('workspace') ?? MAIN_WORKSPACE) === MAIN_WORKSPACE ? pro : community);
  if (head !== 'secrets') throw not_found(`${method} /${parts.join('/')}`);
  if (method === 'GET') {
    const workspace = params.get('workspace');
    if (workspace) return secrets.filter((s) => s.scope.kind === 'workspace' && s.scope.id === workspace).map(view);
    const [k, i] = [params.get('scope_kind'), params.get('scope_id')];
    return secrets.filter((s) => !k || (s.scope.kind === k && s.scope.id === i)).map(view);
  }
  if (kind !== 'ecosystem' && kind !== 'mod' && kind !== 'workspace') throw invalid('Scope is `ecosystem/<id>` or `mod/<mod name>`.');
  const at = secrets.findIndex((s) => s.scope.kind === kind && s.scope.id === id && s.name === name);
  if (method === 'PUT') {
    const value = String(input.value ?? '');
    if (!NAME.test(name ?? '')) throw invalid('A secret name is 1–64 letters, digits or _, not starting with a digit.');
    // Like the kernel's `Secret::PLAIN`: git app settings that aren't secret may be short.
    const plain = ['GITHUB_APP_ID', 'GITHUB_APP_SLUG', 'GITHUB_APP_API_URL', 'GITLAB_OAUTH_URL', 'GITLAB_OAUTH_CLIENT_ID'].includes(name ?? '');
    if (value.length < (plain ? 1 : 6)) throw invalid('A secret value needs at least 6 characters.');
    const secret = { id: `sec_${Date.now().toString(36)}`, scope: { kind, id } as SecretScope, name, updated_at: new Date().toISOString(), value };
    if (at >= 0) secrets[at] = { ...secret, id: secrets[at].id };
    else secrets.push(secret);
    return view(secrets[at >= 0 ? at : secrets.length - 1]);
  }
  if (method === 'DELETE') {
    if (at < 0) throw not_found(`Secret ${name}`);
    secrets.splice(at, 1);
    return null;
  }
  throw not_found(`${method} /${parts.join('/')}`);
}
