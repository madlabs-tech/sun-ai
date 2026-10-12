// In-memory Sun account for the mock: a Sun Cloud ecosystem on Individual, with
// checkout invoices (never paid here: payment happens on the provider's page)
// and support tickets.
import type { Account, Invoice, Ticket } from './types';
import { MockError, clone, not_found } from './mock';

const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });
/** `?self_hosted=1`: an ecosystem without Sun Cloud (no SUN_CLOUD_URL / SUN_CLOUD_TOKEN), so no account and no credits. */
const SELF_HOSTED = new URLSearchParams(location.search).has('self_hosted');
/** The kernel is connected to Sun Cloud: credits runtimes can be added. */
export const sun_cloud = () => !SELF_HOSTED && account.cloud;
const hours_ago = (h: number) => new Date(Date.now() - h * 3600_000).toISOString();

const account: Account = {
  cloud: true,
  plan: 'individual',
  subscription: { plan: 'individual', seats: 1, currency: 'idr', method: 'qris', status: 'active', period_end: '2026-12-31T00:00:00Z' },
  balance: 2_450_000,
  catalog: {
    plans: [
      { plan: 'individual', currency: 'idr', period_months: 3, base: 199_000, per_seat: 0 },
      { plan: 'individual', currency: 'usd', period_months: 1, base: 1_200, per_seat: 0 },
      { plan: 'business', currency: 'idr', period_months: 3, base: 990_000, per_seat: 149_000 },
      { plan: 'business', currency: 'usd', period_months: 1, base: 6_000, per_seat: 900 },
    ],
    packs: [
      { points: 500_000, currency: 'idr', amount: 99_000 },
      { points: 500_000, currency: 'usd', amount: 600 },
    ],
  },
  premium_models: false,
  points: { per_usd: 1000, max_margin_percent: 20 },
  support: { target_hours: 48 },
};

const invoices: Invoice[] = [
  {
    id: 'inv_01JMOCKPAID',
    kind: 'subscription',
    currency: 'idr',
    amount: 199_000,
    method: 'qris',
    checkout_url: null,
    period_months: 3,
    points: 3_000_000,
    status: 'paid',
    created_at: hours_ago(24 * 30),
    paid_at: hours_ago(24 * 30 - 1),
  },
];

const tickets: Ticket[] = [
  {
    id: 'tic_01JMOCK1',
    plan: 'individual',
    subject: 'Codex runs stop after an hour',
    status: 'answered',
    created_at: hours_ago(30),
    respond_by: hours_ago(-18),
    messages: [
      { author: 'customer', body: 'Long Codex steps end with a gateway error after about an hour.', created_at: hours_ago(30) },
      { author: 'sun', body: 'Session keys last 12 hours, so it is not the key. We raised the upstream timeout; please retry.', created_at: hours_ago(26) },
    ],
  },
];

let next = 1;
function invoice(kind: Invoice['kind'], currency: Invoice['currency'], amount: number, method: Invoice['method'], points: number, months: number): Invoice {
  const id = `inv_01JMOCK${(next++).toString().padStart(4, '0')}`;
  const created: Invoice = {
    id,
    kind,
    currency,
    amount,
    method,
    checkout_url: `https://checkout.example/${id}`,
    period_months: months,
    points,
    status: 'open',
    created_at: new Date().toISOString(),
    paid_at: null,
  };
  invoices.unshift(created);
  return clone(created);
}

function payment(currency: unknown, method: unknown) {
  if (currency === 'usd' && method !== 'card') throw invalid(`\`${method}\` isn't offered in USD: IDR takes qris, bank_transfer or card; USD takes card.`);
  if (currency !== 'idr' && currency !== 'usd') throw invalid('`currency` must be `idr` or `usd`.');
}

export function account_route(method: string, parts: string[], input: Record<string, unknown>): unknown {
  const what = parts[1] ?? '';
  if (SELF_HOSTED) throw new MockError(503, { error: { code: 'unavailable', message: 'This ecosystem isn’t connected to Sun.' } });
  if (method === 'GET' && !what) return clone(account);
  if (method === 'GET' && what === 'invoices') return clone(invoices);
  if (method === 'GET' && what === 'support') return clone(tickets);
  if (method === 'POST' && what === 'subscription') {
    payment(input.currency, input.method);
    const seats = Number(input.seats);
    const price = account.catalog.plans.find((p) => p.plan === input.plan && p.currency === input.currency);
    if (!price) throw not_found(`price for ${input.plan} in ${input.currency}`);
    if (!Number.isInteger(seats) || seats < 1 || (input.plan === 'individual' && seats !== 1))
      throw invalid('Individual has exactly 1 seat; Business 1–10000.');
    const points = (input.plan === 'business' ? 2_000_000 : 1_000_000) * price.period_months;
    return invoice('subscription', price.currency, price.base + price.per_seat * seats, input.method as Invoice['method'], points, price.period_months);
  }
  if (method === 'POST' && what === 'top-ups') {
    payment(input.currency, input.method);
    const pack = account.catalog.packs.find((p) => p.points === input.points && p.currency === input.currency);
    if (!pack) throw not_found(`pack of ${input.points} points in ${input.currency}`);
    return invoice('top_up', pack.currency, pack.amount, input.method as Invoice['method'], pack.points, 0);
  }
  if (method === 'POST' && what === 'support') {
    const subject = String(input.subject ?? '').trim();
    const body = String(input.body ?? '').trim();
    if (!subject || subject.length > 200) throw invalid('The subject must be 1–200 characters on one line.');
    if (!body || body.length > 5000) throw invalid('The message must be 1–5000 characters.');
    const now = new Date();
    const ticket: Ticket = {
      id: `tic_01JMOCK${(next++).toString().padStart(4, '0')}`,
      plan: account.plan,
      subject,
      status: 'open',
      created_at: now.toISOString(),
      respond_by: new Date(now.getTime() + (account.support.target_hours ?? 0) * 3600_000).toISOString(),
      messages: [{ author: 'customer', body, created_at: now.toISOString() }],
    };
    tickets.unshift(ticket);
    return clone(ticket);
  }
  throw not_found(`${method} /${parts.join('/')}`);
}
