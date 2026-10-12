// In-memory Sun Cloud billing for the mock: each workspace's subscription (`subscriptions/**`) and the
// deployment owners' support tickets (`support`). Main is on Pro with 5 seats; `?plan=free` puts every
// workspace on Free (no subscription), `?prices=none` has the cloud return no prices, and `?billing=down`
// makes the cloud unreachable (503 `billing_unavailable`, never Free).
import type { PlanPrices, SeatPreview, SubscriptionSummary, Ticket } from './types';
import { MAIN_WORKSPACE } from './types';
import { need } from './auth';
import { MockError, clone, find_workspace, not_found } from './mock';

const params = new URLSearchParams(location.search);
export const mock_free = params.get('plan') === 'free';
const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });
const UNLIMITED = { seats: null, runtimes: null, byo_keys: null, agents: null, hosts: null };
const hours_ago = (h: number) => new Date(Date.now() - h * 3600_000).toISOString();

const PRICES: PlanPrices = {
  currency: 'usd',
  plans: [
    { plan: 'pro', interval: 'month', amount_minor: 0, per_seat_minor: 900 },
    { plan: 'pro', interval: 'year', amount_minor: 0, per_seat_minor: 9_000 },
    { plan: 'business', interval: 'month', amount_minor: 0, per_seat_minor: 1_900 },
    { plan: 'business', interval: 'year', amount_minor: 0, per_seat_minor: 19_000 },
  ],
};

const free = (): SubscriptionSummary => ({
  entitlement: { plan: 'free', status: 'none', seats: null, limits: UNLIMITED, version: 1 },
  seat_capacity: { purchased: 0, used: 3, reserved: 0, available: null, overcommitted: false, version: 1 },
  available_actions: { checkout: true, portal: false, purchase_seats: false },
  currency: 'usd',
});
const pro = (): SubscriptionSummary => ({
  entitlement: { plan: 'pro', status: 'active', seats: 5, limits: { ...UNLIMITED, seats: 5, byo_keys: 2, hosts: 2 }, version: 3 },
  seat_capacity: { purchased: 5, used: 3, reserved: 0, available: 2, overcommitted: false, version: 7 },
  available_actions: { checkout: true, portal: true, purchase_seats: true },
  currency: 'usd',
});
const summaries = new Map<string, SubscriptionSummary>();
const summary_of = (workspace: string) => {
  if (!summaries.has(workspace)) summaries.set(workspace, workspace === MAIN_WORKSPACE && !mock_free ? pro() : free());
  return summaries.get(workspace)!;
};

const tickets: Ticket[] = [
  {
    id: 'tic_01JMOCK1',
    plan: 'pro',
    subject: 'Codex runs stop after an hour',
    status: 'answered',
    created_at: hours_ago(30),
    respond_by: hours_ago(-18),
    messages: [
      { author: 'customer', body: 'Long Codex steps end with a gateway error after about an hour.', created_at: hours_ago(30) },
      { author: 'sun', body: 'We raised the upstream timeout; please retry.', created_at: hours_ago(26) },
    ],
  },
];

let next = 1;
const fresh_id = (prefix: string) => `${prefix}_01JMOCK${(next++).toString().padStart(4, '0')}`;
const key = (input: Record<string, unknown>) => {
  const k = String(input.idempotency_key ?? '');
  if (!k || k.length > 200) throw invalid('`idempotency_key` must be 1–200 visible ASCII characters.');
};
const seats = (n: unknown, field: string) => {
  if (!Number.isInteger(n) || (n as number) < 1 || (n as number) > 10_000) throw invalid(`\`${field}\` must be 1–10000.`);
  return n as number;
};
/** The pro-rated price of `n` more seats on the workspace's plan this month. */
const seat_price = (s: SubscriptionSummary, n: number) =>
  n * (PRICES.plans.find((p) => p.plan === s.entitlement.plan && p.interval === 'month')?.per_seat_minor ?? 0);

/** Where the hosted checkout sends the browser back: the app's `#/billing/return`. */
const return_url = (status: string, workspace: string) =>
  `${location.origin}${location.pathname}?${params}#/billing/return?status=${status}&workspace=${workspace}`;

export function billing_route(method: string, parts: string[], input: Record<string, unknown>): unknown {
  if (params.get('billing') === 'down')
    throw new MockError(503, { error: { code: 'billing_unavailable', message: 'Billing is unavailable right now: Sun Cloud didn’t answer. Try again in a few minutes.' } });
  if (parts[0] === 'support') {
    if (method === 'GET') return clone(tickets);
    const subject = String(input.subject ?? '').trim();
    const body = String(input.body ?? '').trim();
    if (!subject || subject.length > 200) throw invalid('The subject must be 1–200 characters on one line.');
    if (!body || body.length > 5000) throw invalid('The message must be 1–5000 characters.');
    const now = new Date();
    const ticket: Ticket = {
      id: fresh_id('tic'),
      plan: 'pro',
      subject,
      status: 'open',
      created_at: now.toISOString(),
      respond_by: new Date(now.getTime() + 48 * 3600_000).toISOString(),
      messages: [{ author: 'customer', body, created_at: now.toISOString() }],
    };
    tickets.unshift(ticket);
    return clone(ticket);
  }
  // `subscriptions/checkout-sessions` names its workspace in the body; the rest in the path.
  const workspace = parts[1] === 'checkout-sessions' ? String(input.workspace_id ?? '') : parts[1];
  find_workspace(workspace);
  const what = parts.slice(parts[1] === 'checkout-sessions' ? 1 : 2).join('/');
  const s = summary_of(workspace);
  if (method === 'GET' && what === 'summary') return clone(s);
  if (method === 'GET' && what === 'prices') return params.get('prices') === 'none' ? { currency: 'usd', plans: [] } : clone(PRICES);
  if (method !== 'POST') throw not_found(`${method} /${parts.join('/')}`);
  need('edit_workspace', workspace, 'change its plan or seats');
  if (what === 'checkout-sessions') {
    if (input.plan !== 'pro' && input.plan !== 'business') throw invalid('`plan` must be `pro` or `business`.');
    if (input.interval !== 'month' && input.interval !== 'year') throw invalid('`interval` must be `month` or `year`.');
    seats(input.seats, 'seats');
    key(input);
    return { checkout_url: return_url('success', workspace), session_id: fresh_id('cs') };
  }
  if (what === 'portal-sessions') return { url: `https://billing.example/portal/${workspace}` };
  if (what === 'seats/purchase-preview') {
    const n = seats(input.additional_seats, 'additional_seats');
    const preview: SeatPreview = {
      amount_minor: seat_price(s, n),
      currency: s.currency,
      expected_current_seats: s.seat_capacity.purchased,
      expected_purchase_version: s.seat_capacity.version,
    };
    return preview;
  }
  if (what === 'seats/purchases') {
    const n = seats(input.additional_seats, 'additional_seats');
    key(input);
    if (input.expected_current_seats !== s.seat_capacity.purchased || input.expected_purchase_version !== s.seat_capacity.version)
      throw new MockError(409, { error: { code: 'idempotency_conflict', message: 'The seats changed since the preview. Preview again.' } });
    s.seat_capacity.purchased += n;
    s.seat_capacity.available = (s.seat_capacity.available ?? 0) + n;
    s.seat_capacity.version += 1;
    s.entitlement.seats = s.seat_capacity.purchased;
    return clone(s.seat_capacity);
  }
  if (what === 'seats/reconcile') return clone(s.seat_capacity);
  throw not_found(`${method} /${parts.join('/')}`);
}
