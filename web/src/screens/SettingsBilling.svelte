<!-- Plan and credits: the Sun Cloud plans with what each includes, a
     checkout for plan + seats or a credit pack (IDR: QRIS, bank transfer, card; USD: card), the points
     balance and invoices. Payment itself happens on the provider's hosted page. -->
<script lang="ts">
  import { Badge, Button, Card, Icon, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../lib/api/client';
  import type { Account, Currency, Invoice, PaidPlan, PayMethod } from '../lib/api/types';
  import { can, error_text, toast } from '../lib/app_state.svelte';
  import { day, points, price, whole } from '../lib/format';

  const PLANS: { id: PaidPlan; name: string; points: number; includes: string[] }[] = [
    {
      id: 'pro',
      name: 'Pro',
      points: 1_000_000,
      includes: ['Open models on Sun credits (DeepSeek, Kimi, Qwen)', 'Exclusive mods', 'Support from people within 48 hours'],
    },
    {
      id: 'business',
      name: 'Business',
      points: 2_000_000,
      includes: ['Everything in Pro', 'Premium models: Claude and GPT through Claude Code and Codex', 'Seats for your team', 'Support from people within 24 hours'],
    },
  ];
  const METHODS: Record<Currency, { value: PayMethod; label: string }[]> = {
    idr: [
      { value: 'qris', label: 'QRIS' },
      { value: 'bank_transfer', label: 'Bank transfer (virtual account)' },
      { value: 'card', label: 'Card' },
    ],
    usd: [{ value: 'card', label: 'Card' }],
  };
  const METHOD_NAME: Record<PayMethod, string> = { qris: 'QRIS', bank_transfer: 'bank transfer', card: 'card' };
  const CURRENCY = { idr: 'Rupiah (IDR)', usd: 'US dollar (USD)' };
  const INVOICE_BADGE = { open: ['warning', 'Waiting for payment'], paid: ['success', 'Paid'], void: ['neutral', 'Void'] } as const;

  let account = $state<Account | null>(null);
  let unavailable = $state('');
  let invoices = $state<Invoice[]>([]);
  let chosen = $state<PaidPlan | null>(null);
  let order = $state({ currency: 'idr' as Currency, method: 'qris' as PayMethod, seats: '1' });
  let seats_error = $state('');
  let pack = $state({ key: '', method: 'qris' as PayMethod });
  let busy = $state<'plan' | 'pack' | null>(null);
  let pending = $state<Invoice | null>(null);

  async function load() {
    try {
      account = await api.get_account();
      if (account.cloud) invoices = await api.list_invoices();
      pack.key ||= pack_key(account.catalog.packs[0]);
    } catch (e) {
      unavailable = error_text(e);
    }
  }
  load();

  const pack_key = (p?: { points: number; currency: Currency }) => (p ? `${p.points}:${p.currency}` : '');
  const plan_prices = (plan: PaidPlan) => account?.catalog.plans.filter((p) => p.plan === plan) ?? [];
  const currencies = (plan: PaidPlan) => plan_prices(plan).map((p) => ({ value: p.currency, label: CURRENCY[p.currency] }));
  const period = (months: number) => (months === 1 ? 'month' : `${months} months`);
  const current = (plan: PaidPlan) => account?.plan === plan && account.subscription?.status !== 'pending';

  /** "Rp 199.000 / 3 months", "+ Rp 149.000 per seat". */
  function price_lines(plan: PaidPlan): string[] {
    return plan_prices(plan).map(
      (p) => `${price(p.currency, p.base)} / ${period(p.period_months)}` + (p.per_seat ? ` + ${price(p.currency, p.per_seat)} per seat` : ''),
    );
  }

  const chosen_price = $derived(chosen ? plan_prices(chosen).find((p) => p.currency === order.currency) : undefined);
  const seat_count = $derived(chosen === 'business' ? whole(order.seats) : 1);
  const total = $derived(
    chosen_price && typeof seat_count === 'number' && seat_count >= 1
      ? price(chosen_price.currency, chosen_price.base + chosen_price.per_seat * seat_count)
      : null,
  );

  function choose(plan: PaidPlan) {
    chosen = plan;
    pending = null;
    const first = plan_prices(plan)[0];
    if (first && !plan_prices(plan).some((p) => p.currency === order.currency)) order.currency = first.currency;
    order.method = METHODS[order.currency][0].value;
  }

  function fix_method(currency: Currency, method: PayMethod): PayMethod {
    return METHODS[currency].some((m) => m.value === method) ? method : METHODS[currency][0].value;
  }

  async function checkout(e: SubmitEvent) {
    e.preventDefault();
    if (!chosen) return;
    const seats = seat_count;
    seats_error = typeof seats === 'number' && seats >= 1 && seats <= 10_000 ? '' : 'Use a whole number from 1 to 10,000.';
    if (seats_error || typeof seats !== 'number') return;
    busy = 'plan';
    try {
      pending = await api.subscribe({ plan: chosen, seats, currency: order.currency, method: fix_method(order.currency, order.method) });
      invoices = [pending, ...invoices];
    } catch (err) {
      toast('danger', 'Checkout not started', error_text(err));
    }
    busy = null;
  }

  async function buy(e: SubmitEvent) {
    e.preventDefault();
    const found = account?.catalog.packs.find((p) => pack_key(p) === pack.key);
    if (!found) return;
    busy = 'pack';
    try {
      pending = await api.top_up({ points: found.points, currency: found.currency, method: fix_method(found.currency, pack.method) });
      invoices = [pending, ...invoices];
    } catch (err) {
      toast('danger', 'Top-up not started', error_text(err));
    }
    busy = null;
  }

  const pack_currency = $derived(account?.catalog.packs.find((p) => pack_key(p) === pack.key)?.currency ?? 'idr');
</script>

<section class="sun-row" aria-labelledby="billing-head">
  <span class="sun-tile"><Icon name="credit-card" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="billing-head">
      Subscription and credits
      {#if account?.balance != null}<Badge tone="info">{points(account.balance)}</Badge>{/if}
    </div>
    {#if unavailable}
      <p class="sun-row__meta billing__copy">
        Plans and Sun credits are for deployments connected to Sun Cloud. Self-hosted, every engine feature stays free.
      </p>
    {:else if account && !account.cloud}
      <p class="sun-row__meta billing__copy">This deployment isn’t billed through Sun Cloud. Plans, invoices and Sun credits are managed there.</p>
    {:else if account}
      <p class="sun-row__meta billing__copy">
        {#if account.subscription && account.subscription.status !== 'pending'}
          {account.subscription.seats} {account.subscription.seats === 1 ? 'seat' : 'seats'}, paid
          {account.subscription.period_end ? `through ${day(account.subscription.period_end)}` : ''} by {METHOD_NAME[account.subscription.method]}.
        {:else}
          Free: Sun hosts the ecosystem and agents run on your own machines. A plan adds Sun credits, Sun-run models and support.
        {/if}
      </p>

      <ul class="plans" aria-label="Plans">
        {#each PLANS as plan (plan.id)}
          {#if plan_prices(plan.id).length}
            <li>
              <Card variant={current(plan.id) ? 'tint' : 'outline'} padding="md">
                <div class="plan__head">
                  <h3 class="plan__name">{plan.name}</h3>
                  {#if current(plan.id)}<Badge tone="success" dot>Current plan</Badge>{/if}
                </div>
                {#each price_lines(plan.id) as line (line)}<p class="plan__price">{line}</p>{/each}
                <ul class="plan__includes">
                  <li><Icon name="check" size={14} />{points(plan.points)} of Sun credits a month</li>
                  {#each plan.includes as item (item)}<li><Icon name="check" size={14} />{item}</li>{/each}
                </ul>
                {#if !current(plan.id) && can('manage_settings')}
                  <Button variant={plan.id === 'business' ? 'ice' : 'secondary'} block onclick={() => choose(plan.id)}>
                    {account.plan === 'pro' && plan.id === 'business' ? 'Upgrade to Business' : `Choose ${plan.name}`}
                  </Button>
                {/if}
              </Card>
            </li>
          {/if}
        {/each}
      </ul>

      {#if chosen}
        <form class="billing__form" aria-label="Checkout" onsubmit={checkout} novalidate>
          <h3 class="billing__sub">Check out {PLANS.find((p) => p.id === chosen)?.name}</h3>
          <Select label="Currency" bind:value={order.currency} options={currencies(chosen)} onchange={() => (order.method = fix_method(order.currency, order.method))} />
          <Select label="Pay with" bind:value={order.method} options={METHODS[order.currency]} />
          {#if chosen === 'business'}
            <Input label="Seats" type="number" inputmode="numeric" min="1" max="10000" hint="One per person who signs in." bind:value={order.seats} error={seats_error} />
          {/if}
          {#if total}<p class="billing__total">Total: <strong>{total}</strong> for the first {period(chosen_price?.period_months ?? 1)}</p>{/if}
          <div class="billing__actions">
            <Button type="submit" variant="ice" loading={busy === 'plan'}>Continue to payment</Button>
            <Button variant="ghost" onclick={() => (chosen = null)}>Cancel</Button>
          </div>
        </form>
      {/if}

      {#if pending}
        <div class="billing__pending" role="status">
          <p>
            Invoice for <strong>{price(pending.currency, pending.amount)}</strong> is ready. Pay by {METHOD_NAME[pending.method]} on the
            checkout page. {pending.kind === 'subscription' ? 'The plan switches on' : 'The points arrive'} once the payment clears.
          </p>
          {#if pending.checkout_url}
            <Button href={pending.checkout_url} target="_blank" rel="noopener noreferrer" variant="ice" iconRight="external-link">Open checkout</Button>
          {/if}
        </div>
      {/if}

      {#if account.catalog.packs.length && can('manage_settings')}
        <form class="billing__form" aria-label="Buy Sun credits" onsubmit={buy} novalidate>
          <h3 class="billing__sub">Buy Sun credits</h3>
          <p class="sun-row__meta billing__copy">Points never expire. Each model step costs provider price plus a visible margin.</p>
          <Select
            label="Pack"
            bind:value={pack.key}
            options={account.catalog.packs.map((p) => ({ value: pack_key(p), label: `${points(p.points)} · ${price(p.currency, p.amount)}` }))}
          />
          <Select label="Pay with" bind:value={pack.method} options={METHODS[pack_currency]} />
          <div class="billing__actions"><Button type="submit" variant="secondary" loading={busy === 'pack'}>Buy points</Button></div>
        </form>
      {/if}

      {#if invoices.length}
        <h3 class="billing__sub">Invoices</h3>
        <ul class="invoices" aria-label="Invoices">
          {#each invoices as invoice (invoice.id)}
            {@const [tone, label] = INVOICE_BADGE[invoice.status]}
            <li class="invoices__item">
              <span class="invoices__what">
                {invoice.kind === 'subscription' ? `Plan, ${period(invoice.period_months)}` : points(invoice.points)}
                <span class="invoices__meta">{day(invoice.created_at)} · {price(invoice.currency, invoice.amount)}</span>
              </span>
              <Badge {tone} dot>{label}</Badge>
            </li>
          {/each}
        </ul>
      {/if}
    {:else}
      <div class="sun-empty" role="status">Loading…</div>
    {/if}
  </div>
</section>

<style>
  .billing__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
  .plans {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: var(--space-3);
    margin: var(--space-4) 0 0;
    padding: 0;
    list-style: none;
    max-width: 720px;
  }
  .plans :global(.ml-card) {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    height: 100%;
  }
  .plan__head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
  }
  .plan__name {
    margin: 0;
    font-size: 16px;
  }
  .plan__price {
    margin: 0;
    font-size: 14px;
    font-weight: 600;
    color: var(--fg-1);
  }
  .plan__includes {
    display: grid;
    gap: var(--space-2);
    margin: var(--space-2) 0 var(--space-3);
    padding: 0;
    list-style: none;
    font-size: 13px;
    line-height: 1.5;
    color: var(--fg-2);
    flex: 1;
  }
  .plan__includes li {
    display: flex;
    gap: var(--space-2);
    align-items: baseline;
  }
  .billing__form {
    display: grid;
    gap: var(--space-3);
    margin-top: var(--space-5);
    max-width: 480px;
  }
  .billing__sub {
    margin: var(--space-5) 0 0;
    font-size: 14px;
  }
  .billing__form .billing__sub {
    margin: 0;
  }
  .billing__total {
    margin: 0;
    font-size: 14px;
  }
  .billing__actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .billing__pending {
    display: grid;
    gap: var(--space-3);
    justify-items: start;
    margin-top: var(--space-4);
    padding: var(--space-4);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    max-width: 480px;
    line-height: 1.5;
  }
  .billing__pending p {
    margin: 0;
  }
  .invoices {
    display: grid;
    gap: var(--space-2);
    margin: var(--space-2) 0 0;
    padding: 0;
    list-style: none;
    max-width: 480px;
  }
  .invoices__item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    font-size: 14px;
  }
  .invoices__what {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .invoices__meta {
    font-size: 12px;
    color: var(--fg-3);
  }
</style>
