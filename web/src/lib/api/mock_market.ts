// In-memory Sun Cloud marketplace for the mock: listings (with filters),
// the leases this ecosystem bought, with escrow moved by Accept and Dispute,
// the seller's own listings and payouts, and the ecosystem policy.
import type { MarketLease, MarketListing, MarketPayout, MarketPolicy, NewLease } from './types';
import { MockError, clone, not_found } from './mock';

const listing = (l: Partial<MarketListing> & Pick<MarketListing, 'id' | 'offering' | 'model' | 'price_points'>): MarketListing => ({
  runtime: 'claude-code',
  unit: 'task',
  concurrency: 2,
  region: 'id-jkt',
  warning: null,
  capabilities: [],
  rating: null,
  rating_count: 0,
  contract_hours: null,
  host_verified: true,
  ...l,
});

const LISTINGS: MarketListing[] = [
  listing({ id: 'lst_01', offering: 'machine', model: 'claude-sonnet-4.5', price_points: 120, capabilities: ['kvm'], rating: 4.7, rating_count: 23 }),
  listing({ id: 'lst_02', offering: 'open_model', runtime: 'opencode', model: 'qwen3-coder-30b', price_points: 40, unit: 'hour', region: 'sg-sin', capabilities: ['gpu'], rating: 4.1, rating_count: 8 }),
  listing({
    id: 'lst_03',
    offering: 'subscription',
    model: 'claude-opus-4.1',
    price_points: 90,
    warning: 'Reselling a consumer subscription may break the provider’s terms and get the seller’s account banned.',
    host_verified: false,
  }),
  listing({ id: 'lst_04', offering: 'machine', runtime: 'codex', model: 'gpt-5-codex', price_points: 2400, unit: 'contract', contract_hours: 72, region: 'id-sub', capabilities: ['ios_simulator'], rating: 4.9, rating_count: 5 }),
];

/** This ecosystem's own capacity (seller side). */
const MINE: MarketListing[] = [
  listing({ id: 'lst_m1', offering: 'machine', runtime: 'codex', model: 'gpt-5-codex', price_points: 60, unit: 'hour', capabilities: ['gpu'], status: 'active', rating: 4.5, rating_count: 2 }),
];

const payouts: MarketPayout[] = [];
let payable = 1840;

let policy: MarketPolicy = { allow: 'all', workflows: [], failover_max_price_points: null };

const leases: MarketLease[] = [
  {
    id: 'lea_01JMOCKDONE',
    listing_id: 'lst_01',
    buyer_ecosystem: 'me',
    seller_ecosystem: 'seller-1',
    task_ref: '01JAT0136'.padEnd(26, '0'), // SUN-136, done
    unit: 'task',
    price_points: 120,
    status: 'closed',
    started_at: new Date(Date.now() - 5 * 3600_000).toISOString(),
    closed_at: new Date(Date.now() - 3600_000).toISOString(),
    close_reason: 'closed_by_buyer',
    escrow: 'held',
    release_at: new Date(Date.now() + 3 * 86400_000).toISOString(),
    dispute: null,
    rating: null,
  },
];

const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });
const conflict = (message: string) => new MockError(409, { error: { code: 'conflict', message } });

function filtered(params: URLSearchParams): MarketListing[] {
  const [region, offering, capability] = ['region', 'offering', 'capability'].map((k) => params.get(k) ?? '');
  const min_rating = Number(params.get('min_rating') ?? 0);
  return LISTINGS.filter(
    (l) =>
      l.region.startsWith(region) &&
      (!offering || l.offering === offering) &&
      (!capability || l.capabilities.includes(capability)) &&
      (l.rating ?? 0) >= min_rating,
  );
}

function seller_route(method: string, what: string, id: string | undefined, input: Record<string, unknown>): unknown {
  if (what === 'payouts') {
    if (method === 'GET') return clone({ payable, payouts });
    const points = Number(input.points);
    const destination = String(input.destination ?? '').trim();
    if (!(points > 0) || points > payable) throw invalid(`Pay out 1–${payable} points.`);
    if (!destination) throw invalid('Say where the money goes.');
    const currency = input.currency === 'idr' ? 'idr' : 'usd';
    const payout: MarketPayout = { id: `pay_${payouts.length + 1}`, points, currency, amount: currency === 'usd' ? points / 10 : points * 16, destination, created_at: new Date().toISOString(), sent_at: null };
    payable -= points;
    payouts.unshift(payout);
    return clone(payout);
  }
  if (method === 'GET' && id === 'mine') return clone(MINE);
  const mine = MINE.find((l) => l.id === id);
  if (!mine) throw not_found('Listing');
  const status = input.status;
  if (status !== 'active' && status !== 'paused') throw invalid('`status` is `active` or `paused`.');
  mine.status = status;
  return clone(mine);
}

export function market_route(method: string, parts: string[], input: Record<string, unknown>, params: URLSearchParams): unknown {
  const [, what, id, action] = parts;
  if (what === 'policy') {
    if (method === 'GET') return clone(policy);
    if (!['all', 'none', 'workflows'].includes(String(input.allow))) throw invalid('`allow` is `all`, `none` or `workflows`.');
    policy = clone(input) as unknown as MarketPolicy;
    return clone(policy);
  }
  if (what === 'payouts' || (what === 'listings' && id)) return seller_route(method, what, id, input);
  if (method === 'GET' && what === 'listings') return clone(filtered(params));
  if (method === 'GET' && what === 'leases') return clone(leases);
  if (method === 'POST' && what === 'leases' && !id) {
    const want = input as unknown as NewLease;
    if (!want.accept_disclosure) throw invalid('Confirm that the seller’s machine can see this task’s code and data.');
    if (!(want.max_price_points > 0)) throw invalid('`max_price_points` must be above 0.');
    if (policy.allow === 'none') throw new MockError(403, { error: { code: 'forbidden', message: 'The ecosystem policy keeps this workflow off marketplace runtimes.' } });
    if (leases.some((l) => l.task_ref === want.task_id)) throw conflict('This task is already leased.');
    const match = LISTINGS.filter((l) => l.runtime === want.runtime && l.unit === want.unit && l.price_points <= want.max_price_points).sort((a, b) => a.price_points - b.price_points)[0];
    if (!match) throw not_found('A listing at that price');
    const lease: MarketLease = {
      id: `lea_01JMOCK${leases.length}`,
      listing_id: match.id,
      buyer_ecosystem: 'me',
      seller_ecosystem: 'seller-1',
      task_ref: want.task_id,
      unit: match.unit,
      price_points: match.price_points,
      status: 'open',
      started_at: new Date().toISOString(),
      closed_at: null,
      close_reason: null,
      escrow: 'held',
      release_at: null,
      dispute: null,
      rating: null,
    };
    leases.unshift(lease);
    return clone(lease);
  }
  const lease = leases.find((l) => l.id === id);
  if (method !== 'POST' || what !== 'leases' || !lease) throw not_found('Lease');
  if (action === 'rating') {
    const stars = Number(input.stars);
    if (lease.status !== 'closed') throw conflict('Rate a rental once it has ended.');
    if (lease.rating) throw conflict('You rated this rental already.');
    if (!Number.isInteger(stars) || stars < 1 || stars > 5) throw invalid('`stars` is 1–5.');
    lease.rating = stars;
    return clone(lease);
  }
  if (lease.escrow !== 'held') throw conflict(`lease ${lease.id} is already settled`);
  const close = (reason: string) => {
    if (lease.status === 'open') Object.assign(lease, { status: 'closed', closed_at: new Date().toISOString(), close_reason: reason });
  };
  if (action === 'accept') {
    close('accepted');
    lease.escrow = 'released';
  } else if (action === 'dispute') {
    const reason = String(input.reason ?? '').trim();
    if (!reason) throw invalid('Say what went wrong in 1–2000 characters.');
    close('disputed');
    Object.assign(lease, { escrow: 'disputed', dispute: reason });
  } else throw not_found('Lease action');
  return clone(lease);
}
