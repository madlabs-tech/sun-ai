<!-- Plan & billing of the open workspace. The plan badge: which plan it runs on, where it comes from, and its
     limits. With Sun Cloud billing (`features.billing`), its subscription too: status, seats purchased / used /
     available, and for its owners and admins Upgrade (plan + interval + seats, and the currency when the cloud
     prices in more than one, then the hosted checkout),
     Manage billing (the provider's portal), Buy seats (a price preview first) and Cancel plan (confirmed first).
     Members read only. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import { ApiFailure, api } from '../lib/api/client';
  import type { Currency, Entitlement, Interval, PaidPlan, PlanPrices, SeatPreview, SubscriptionSummary } from '../lib/api/types';
  import { app, can, current_workspace, error_text, toast } from '../lib/app_state.svelte';
  import { clock, day, price, whole } from '../lib/format';

  const PLAN = { community: 'Self-hosted', free: 'Free', pro: 'Pro', business: 'Business' };
  const STATUS = {
    active: ['success', 'Active'],
    past_due: ['warning', 'Payment past due'],
    canceled: ['neutral', 'Canceled'],
    none: ['neutral', 'No subscription'],
  } as const;
  const INTERVALS = [
    { value: 'month', label: 'Monthly' },
    { value: 'year', label: 'Yearly' },
  ];
  const CURRENCIES: { value: Currency; label: string }[] = [
    { value: 'usd', label: 'US dollar (USD)' },
    { value: 'idr', label: 'Indonesian rupiah (IDR)' },
  ];

  const workspace = current_workspace();
  let current = $state<Entitlement | null>(null);
  let load_error = $state('');
  api.get_entitlement(workspace || undefined).then((e) => (current = e), (e) => (load_error = error_text(e)));

  /** Only once the kernel said billing is on (Sun Cloud connected). */
  const billing = $derived(workspace !== '' && app.features?.billing === true);
  /** Owners and admins (and deployment owners) pay; the kernel checks the same. */
  const may_pay = $derived(can('edit_workspace'));
  let summary = $state<SubscriptionSummary | null>(null);
  let prices = $state<PlanPrices | null>(null);
  let billing_error = $state('');
  /** The currency picked for prices; none: the subscription's, else the cloud's default. */
  let currency = $state<Currency | undefined>();
  /** The currencies the cloud has prices in; a switch shows when there are two. */
  let currencies = $state<Currency[]>([]);
  const fetch_billing = () => Promise.all([api.subscription_summary(workspace, currency), api.subscription_prices(workspace, currency)]);
  async function load_billing() {
    try {
      [summary, prices] = await fetch_billing();
    } catch (e) {
      billing_error = error_text(e);
      return;
    }
    if (currencies.length || !may_pay) return;
    // Asked for a currency it has no prices in, the cloud answers in its default one.
    const shown = prices!;
    const priced = await Promise.all(
      CURRENCIES.map(({ value }) => (value === shown.currency ? shown : api.subscription_prices(workspace, value).catch(() => null))),
    );
    currencies = CURRENCIES.map((c) => c.value).filter((c, i) => priced[i]?.currency === c && priced[i]!.plans.length > 0);
    currency = shown.currency as Currency;
  }
  async function switch_currency(picked: string) {
    currency = picked as Currency;
    try {
      [summary, prices] = await fetch_billing();
    } catch (e) {
      toast('danger', 'Prices not loaded', error_text(e));
    }
  }
  let started = false;
  $effect(() => {
    if (billing && !started) {
      started = true;
      load_billing();
    }
  });

  /** A fresh nonce per form; with the order it makes the idempotency key, so a double submit never pays twice. */
  const nonce = () => Array.from(crypto.getRandomValues(new Uint8Array(12)), (b) => b.toString(16).padStart(2, '0')).join('');

  const tone = (e: Entitlement) => ({ cloud: 'success', stale: 'warning', unavailable: 'warning', community: 'neutral' } as const)[e.status];

  function detail(e: Entitlement): string {
    switch (e.status) {
      case 'community':
        return 'Community: every engine feature, no limits.';
      case 'cloud':
        return `From Sun Cloud${e.paid ? ', paid' : ''}${e.valid_until ? `; checked again at ${clock(e.valid_until)}` : ''}.`;
      case 'stale':
        return 'Sun Cloud can’t be reached: the last plan it sent still applies.';
      case 'unavailable':
        return 'Sun Cloud can’t be reached: no limits apply until it answers.';
    }
  }

  /** The caps people meet. */
  function limit_text(e: Entitlement): string {
    const l = e.limits;
    return [
      [l.seats, 'seats'],
      [l.hosts, 'hosts'],
      [l.runtimes, 'runtimes'],
      [l.agents, 'agents'],
      [l.byo_keys, 'own API keys'],
    ]
      .filter(([n]) => typeof n === 'number')
      .map(([n, what]) => `${n} ${what}`)
      .join(' · ');
  }

  // Upgrade: the paid plans the cloud prices, other than the current one.
  const offered = $derived(
    [...new Set((prices?.plans ?? []).map((p) => p.plan))].filter((p) => p !== summary?.entitlement.plan),
  );
  const can_upgrade = $derived(may_pay && !!summary?.available_actions.checkout && offered.length > 0);
  let upgrading = $state<{ plan: PaidPlan; interval: Interval; seats: string; nonce: string } | null>(null);
  let seats_error = $state('');
  let busy = $state<'' | 'checkout' | 'portal' | 'preview' | 'buy' | 'cancel'>('');
  let confirming_cancel = $state(false);

  async function cancel_plan() {
    busy = 'cancel';
    try {
      const { period_end } = await api.cancel_plan(workspace);
      toast('success', 'Plan canceled', period_end ? `It stays on until ${day(period_end)}.` : undefined);
      confirming_cancel = false;
      await load_billing();
    } catch (err) {
      toast('danger', 'Plan not canceled', error_text(err));
    }
    busy = '';
  }

  function open_upgrade() {
    const used = summary?.seat_capacity.used ?? 1;
    upgrading = { plan: offered[0], interval: 'month', seats: String(Math.max(used, 1)), nonce: nonce() };
  }

  const order_price = $derived(upgrading && prices?.plans.find((p) => p.plan === upgrading!.plan && p.interval === upgrading!.interval));
  const order_seats = $derived(upgrading ? whole(upgrading.seats) : null);
  const total = $derived(
    order_price && typeof order_seats === 'number' && order_seats >= 1 && prices
      ? `${price(prices.currency, order_price.amount_minor + order_price.per_seat_minor * order_seats)} a ${order_price.interval}`
      : null,
  );

  async function checkout(e: SubmitEvent) {
    e.preventDefault();
    if (!upgrading) return;
    const seats = order_seats;
    seats_error = typeof seats === 'number' && seats >= 1 && seats <= 10_000 ? '' : 'Use a whole number from 1 to 10,000.';
    if (seats_error || typeof seats !== 'number') return;
    busy = 'checkout';
    try {
      const { plan, interval } = upgrading;
      const in_currency = prices?.currency as Currency | undefined;
      const idempotency_key = `${upgrading.nonce}-${plan}-${interval}-${seats}-${in_currency ?? ''}`;
      const session = await api.start_checkout({ workspace_id: workspace, plan, interval, seats, idempotency_key, currency: in_currency });
      location.assign(session.checkout_url);
    } catch (err) {
      toast('danger', 'Checkout not started', error_text(err));
    }
    busy = '';
  }

  async function manage() {
    busy = 'portal';
    try {
      location.assign((await api.billing_portal(workspace)).url);
    } catch (err) {
      toast('danger', 'Billing portal not opened', error_text(err));
    }
    busy = '';
  }

  // Buy seats: preview the price, then buy at exactly that price.
  let buying = $state<{ seats: string; nonce: string } | null>(null);
  let extra_error = $state('');
  let preview = $state<(SeatPreview & { additional_seats: number }) | null>(null);

  async function preview_seats(e: SubmitEvent) {
    e.preventDefault();
    if (!buying) return;
    const n = whole(buying.seats);
    extra_error = typeof n === 'number' && n >= 1 && n <= 10_000 ? '' : 'Use a whole number from 1 to 10,000.';
    if (extra_error || typeof n !== 'number') return;
    busy = 'preview';
    try {
      preview = { ...(await api.preview_seats(workspace, n)), additional_seats: n };
    } catch (err) {
      extra_error = error_text(err);
    }
    busy = '';
  }

  async function buy() {
    if (!preview || !buying) return;
    busy = 'buy';
    try {
      const key = `${buying.nonce}-${preview.additional_seats}-${preview.expected_purchase_version}`;
      await api.buy_seats(workspace, { ...preview, idempotency_key: key });
      toast('success', `${preview.additional_seats} ${preview.additional_seats === 1 ? 'seat' : 'seats'} added`);
      buying = null;
      preview = null;
      await load_billing();
    } catch (err) {
      toast('danger', 'Seats not bought', error_text(err));
      // The seats or price moved on: the old preview can't be bought.
      if (err instanceof ApiFailure && err.code === 'stale_seat_preview') preview = null;
    }
    busy = '';
  }
</script>

<section class="sun-row" aria-labelledby="plan-head">
  <span class="sun-tile"><Icon name="shield-check" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="plan-head">
      Plan
      {#if current}<Badge tone={tone(current)} dot>{PLAN[current.plan]}</Badge>{/if}
    </div>
    {#if load_error}
      <div class="sun-empty" role="alert">{load_error}</div>
    {:else if current}
      {@const caps = limit_text(current)}
      {#if current.plan === 'free' && !caps}
        <p class="sun-row__meta plan__copy">
          Free plan, no limits. Bring your own runtime (a machine running <code>sun host</code>) and your own API keys.
        </p>
      {:else}
        <p class="sun-row__meta plan__copy">{detail(current)}</p>
        {#if caps}<p class="sun-row__meta plan__copy plan__limits">Plan limits: {caps}. Sun refuses more and says why.</p>{/if}
      {/if}
    {:else}
      <div class="sun-empty" role="status">Loading…</div>
    {/if}
  </div>
</section>

{#if billing}
  <section class="sun-row" aria-labelledby="billing-head">
    <span class="sun-tile"><Icon name="credit-card" size={18} /></span>
    <div class="sun-row__body">
      <div class="sun-row__title" id="billing-head">
        Billing
        {#if summary}
          {@const [status_tone, status_label] = STATUS[summary.entitlement.status]}
          <Badge tone={status_tone} dot>{status_label}</Badge>
        {/if}
      </div>
      {#if billing_error}
        <div class="sun-empty" role="alert">{billing_error}</div>
      {:else if summary}
        {@const seats = summary.seat_capacity}
        {#if summary.entitlement.status === 'none'}
          <p class="sun-row__meta plan__copy">No seat limit on the free plan · {seats.used} {seats.used === 1 ? 'person' : 'people'} in this workspace.</p>
        {:else}
          <dl class="seats" aria-label="Seats">
            <div><dt>Purchased</dt><dd>{seats.purchased}</dd></div>
            <div><dt>Used</dt><dd>{seats.used}</dd></div>
            <div><dt>Available</dt><dd>{seats.available ?? '—'}</dd></div>
          </dl>
          {#if seats.overcommitted}
            <p class="sun-row__meta plan__copy" role="alert">More people than seats: buy seats or remove someone.</p>
          {/if}
        {/if}

        {#if may_pay}
          <div class="billing__actions">
            {#if can_upgrade}<Button variant="ice" iconLeft="sparkles" onclick={open_upgrade}>Upgrade</Button>{/if}
            {#if summary.available_actions.portal}
              <Button variant="secondary" iconRight="external-link" loading={busy === 'portal'} onclick={manage}>Manage billing</Button>
            {/if}
            {#if summary.available_actions.purchase_seats}
              <Button variant="secondary" iconLeft="plus" onclick={() => ((buying = { seats: '1', nonce: nonce() }), (preview = null))}>Buy seats</Button>
            {/if}
            {#if summary.entitlement.status === 'active' || summary.entitlement.status === 'past_due'}
              <Button variant="ghost" onclick={() => (confirming_cancel = true)}>Cancel plan</Button>
            {/if}
          </div>
        {:else}
          <p class="sun-row__meta plan__copy">Only the workspace’s owners and admins change its plan and seats.</p>
        {/if}

        {#if upgrading}
          <form class="billing__form" aria-label="Upgrade" onsubmit={checkout} novalidate>
            <Select label="Plan" bind:value={upgrading.plan} options={offered.map((p) => ({ value: p, label: PLAN[p] }))} />
            <Select label="Billed" bind:value={upgrading.interval} options={INTERVALS} />
            {#if currencies.length > 1}
              <Select
                label="Currency"
                value={currency}
                options={CURRENCIES.filter((c) => currencies.includes(c.value))}
                onchange={(e: Event) => switch_currency((e.currentTarget as HTMLSelectElement).value)}
              />
            {/if}
            <Input label="Seats" type="number" inputmode="numeric" min="1" max="10000" hint="One per person in the workspace." bind:value={upgrading.seats} error={seats_error} />
            {#if total}<p class="billing__total">Total: <strong>{total}</strong></p>{/if}
            <div class="billing__actions">
              <Button type="submit" variant="ice" iconRight="external-link" loading={busy === 'checkout'}>Continue to checkout</Button>
              <Button variant="ghost" onclick={() => (upgrading = null)}>Cancel</Button>
            </div>
          </form>
        {/if}

        {#if buying}
          <form class="billing__form" aria-label="Buy seats" onsubmit={preview_seats} novalidate>
            <Input label="Extra seats" type="number" inputmode="numeric" min="1" max="10000" bind:value={buying.seats} error={extra_error} oninput={() => (preview = null)} />
            {#if preview}
              <p class="billing__total" role="status">
                {preview.additional_seats} more {preview.additional_seats === 1 ? 'seat' : 'seats'}: <strong>{price(preview.currency, preview.amount_minor)}</strong> now, then with the plan.
              </p>
            {/if}
            <div class="billing__actions">
              {#if preview}
                <Button variant="ice" loading={busy === 'buy'} onclick={buy}>Buy {preview.additional_seats} {preview.additional_seats === 1 ? 'seat' : 'seats'}</Button>
              {:else}
                <Button type="submit" variant="secondary" loading={busy === 'preview'}>Preview price</Button>
              {/if}
              <Button variant="ghost" onclick={() => ((buying = null), (preview = null))}>Cancel</Button>
            </div>
          </form>
        {/if}
      {:else}
        <div class="sun-empty" role="status">Loading…</div>
      {/if}
    </div>
  </section>
{/if}

<Dialog open={confirming_cancel} onclose={() => (confirming_cancel = false)} title="Cancel the plan?">
  <p class="plan__copy">
    The plan stops renewing and stays on until the end of the period already paid. Then the workspace is on Free: no
    limits, bring your own runtime and API keys.
  </p>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (confirming_cancel = false)}>Keep the plan</Button>
    <Button variant="danger" loading={busy === 'cancel'} onclick={cancel_plan}>Cancel plan</Button>
  {/snippet}
</Dialog>

<style>
  .plan__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
  .plan__copy code {
    font-family: var(--font-mono);
    font-size: 0.92em;
  }
  .seats {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3) var(--space-5);
    margin: var(--space-2) 0 0;
  }
  .seats div {
    display: flex;
    flex-direction: column;
  }
  .seats dt {
    font-size: 12px;
    color: var(--fg-3);
  }
  .seats dd {
    margin: 0;
    font-family: var(--font-mono);
    font-size: 16px;
    color: var(--fg-1);
  }
  .billing__actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin-top: var(--space-4);
  }
  .billing__form {
    display: grid;
    gap: var(--space-3);
    margin-top: var(--space-4);
    max-width: 480px;
  }
  .billing__form .billing__actions {
    margin-top: 0;
  }
  .billing__total {
    margin: 0;
    font-size: 14px;
  }
</style>
