<!-- Marketplace runtime capacity. Rent: filter capacity, rent it for a task that
     hasn't started (disclosure + per-secret consent), then Accept, Dispute and rate the
     work. Sell: pause or resume your listings, pay out what you earned. Policy: which
     workflows may use marketplace runtimes, and the failover price cap. -->
<script lang="ts">
  import { Badge, Button, Checkbox, Dialog, Icon, Input, Radio, Select, Tabs } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { api } from '../lib/api/client';
  import type { Escrow, ListingFilter, MarketLease, MarketListing, MarketPayouts, MarketPolicy, MarketUnit, TaskSummary } from '../lib/api/types';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { clock } from '../lib/format';

  let tab = $state('rent');
  const TABS = [
    { id: 'rent', label: 'Rent' },
    { id: 'sell', label: 'Sell' },
    { id: 'policy', label: 'Policy' },
  ];

  let listings = $state<MarketListing[] | null>(null);
  let leases = $state<MarketLease[]>([]);
  let tasks = $state<TaskSummary[]>([]);
  let secret_names = $state<string[]>([]);
  let failure = $state('');
  let filter = $state<ListingFilter>({ region: '', offering: '', capability: '' });

  async function load() {
    try {
      [listings, leases, tasks] = await Promise.all([api.market_listings(filter), api.market_leases(), api.list_tasks()]);
      failure = '';
    } catch (e) {
      failure = error_text(e);
    }
  }
  load();

  let filter_timer: ReturnType<typeof setTimeout> | undefined;
  /** Refetches with the filters, debounced for typing in Region. */
  function refilter() {
    clearTimeout(filter_timer);
    filter_timer = setTimeout(load, 250);
  }

  /** Tasks that haven't started: only these can move to a seller's machine. */
  const waiting = $derived(tasks.filter((t) => t.status === 'backlog' || t.status === 'todo'));
  const title = (ref: string) => tasks.find((t) => t.id === ref)?.title ?? ref;
  const model = (id: string) => listings?.find((l) => l.id === id)?.model;

  const OFFERING: Record<MarketListing['offering'], { label: string; icon: string }> = {
    machine: { label: 'Machine time', icon: 'server' },
    open_model: { label: 'Open model', icon: 'cpu' },
    api: { label: 'API capacity', icon: 'plug' },
    subscription: { label: 'Subscription', icon: 'key-round' },
  };
  const ESCROW: Record<Escrow, { tone: 'info' | 'warning' | 'success' | 'neutral'; label: string }> = {
    held: { tone: 'info', label: 'Points held' },
    disputed: { tone: 'warning', label: 'Disputed' },
    released: { tone: 'success', label: 'Paid to seller' },
    refunded: { tone: 'neutral', label: 'Refunded' },
  };
  const CAPABILITY: Record<string, string> = { gpu: 'GPU', ios_simulator: 'iOS Simulator', kvm: 'KVM' };
  const capability = (c: string) => CAPABILITY[c] ?? c;
  const per = (unit: MarketUnit, hours: number | null) => (unit === 'contract' ? `contract (${hours ?? '?'} h)` : unit);

  // Rent dialog
  let renting = $state<MarketListing | null>(null);
  let rent_open = $state(false);
  let task_id = $state('');
  let max_price = $state('');
  let disclosed = $state(false);
  let allowed = $state<string[]>([]);
  let leasing = $state(false);

  async function rent(listing: MarketListing) {
    renting = listing;
    task_id = waiting[0]?.id ?? '';
    max_price = String(listing.price_points);
    disclosed = false;
    allowed = [];
    rent_open = true;
    try {
      secret_names = [...new Set((await api.list_secrets()).map((s) => s.name))].sort();
    } catch {
      secret_names = [];
    }
  }

  async function lease() {
    if (!renting) return;
    leasing = true;
    try {
      await api.lease({
        task_id,
        runtime: renting.runtime,
        unit: renting.unit,
        max_price_points: Number(max_price),
        accept_disclosure: disclosed,
        secrets: allowed,
      });
      rent_open = false;
      toast('success', 'Capacity rented', `${title(task_id)} runs on the seller’s machine once it joins.`);
      await load();
    } catch (e) {
      toast('danger', 'Couldn’t rent it', error_text(e));
    }
    leasing = false;
  }

  // Accept / dispute / rate
  let busy = $state('');
  let disputing = $state<MarketLease | null>(null);
  let dispute_open = $state(false);
  let reason = $state('');
  let rating = $state<MarketLease | null>(null);
  let rate_open = $state(false);
  let stars = $state('5');
  let comment = $state('');

  async function accept(lease: MarketLease) {
    busy = lease.id;
    try {
      await api.accept_lease(lease.id);
      toast('success', 'Work accepted', 'The seller is paid now.');
      await load();
    } catch (e) {
      toast('danger', 'Couldn’t accept it', error_text(e));
    }
    busy = '';
  }

  async function dispute() {
    if (!disputing) return;
    busy = disputing.id;
    try {
      await api.dispute_lease(disputing.id, reason);
      dispute_open = false;
      toast('success', 'Dispute sent', 'The points stay held until Sun decides.');
      await load();
    } catch (e) {
      toast('danger', 'Couldn’t send the dispute', error_text(e));
    }
    busy = '';
  }

  async function rate() {
    if (!rating) return;
    busy = rating.id;
    try {
      await api.rate_lease(rating.id, Number(stars), comment.trim() || undefined);
      rate_open = false;
      toast('success', 'Thanks for rating', 'Other buyers see it on the listing.');
      await load();
    } catch (e) {
      toast('danger', 'Couldn’t save the rating', error_text(e));
    }
    busy = '';
  }

  // Sell
  let mine = $state<MarketListing[] | null>(null);
  let earnings = $state<MarketPayouts | null>(null);
  let sell_failure = $state('');
  let payout_open = $state(false);
  let payout_points = $state('');
  let currency = $state<'usd' | 'idr'>('usd');
  let destination = $state('');

  async function load_sell() {
    try {
      [mine, earnings] = await Promise.all([api.my_listings(), api.market_payouts()]);
      sell_failure = '';
    } catch (e) {
      sell_failure = error_text(e);
    }
  }

  async function toggle(listing: MarketListing) {
    busy = listing.id;
    const next = listing.status === 'paused' ? 'active' : 'paused';
    try {
      await api.set_listing_status(listing.id, next);
      toast('success', next === 'paused' ? 'Listing paused' : 'Listing live again', next === 'paused' ? 'No new rentals; running ones finish.' : 'Buyers can rent it now.');
      await load_sell();
    } catch (e) {
      toast('danger', 'Couldn’t change the listing', error_text(e));
    }
    busy = '';
  }

  async function pay_out() {
    busy = 'payout';
    try {
      await api.request_payout(Number(payout_points), currency, destination.trim());
      payout_open = false;
      toast('success', 'Payout requested', 'It’s on its way to your account.');
      await load_sell();
    } catch (e) {
      toast('danger', 'Couldn’t request the payout', error_text(e));
    }
    busy = '';
  }

  // Policy
  let policy = $state<MarketPolicy | null>(null);
  let workflow_names = $state<string[]>([]);
  let failover_cap = $state('');
  let policy_failure = $state('');

  async function load_policy() {
    try {
      const [p, workflows] = await Promise.all([api.market_policy(), api.list_workflows()]);
      policy = p;
      workflow_names = workflows.map((w) => w.name).sort();
      failover_cap = p.failover_max_price_points ? String(p.failover_max_price_points) : '';
      policy_failure = '';
    } catch (e) {
      policy_failure = error_text(e);
    }
  }

  async function save_policy() {
    if (!policy) return;
    busy = 'policy';
    try {
      policy = await api.set_market_policy({ ...$state.snapshot(policy), failover_max_price_points: Number(failover_cap) > 0 ? Number(failover_cap) : null });
      toast('success', 'Policy saved', 'It applies to the next rental.');
    } catch (e) {
      toast('danger', 'Couldn’t save the policy', error_text(e));
    }
    busy = '';
  }

  $effect(() => {
    if (tab === 'sell' && !mine) load_sell();
    if (tab === 'policy' && !policy) load_policy();
  });
</script>

<PageHeader title="Marketplace" subtitle="Rent runtime capacity from other Sun users, or sell yours. Rented tasks run in a fresh VM on the seller’s machine." />

<Dialog bind:open={rent_open} onclose={() => (rent_open = false)} title="Rent {renting?.model ?? ''}">
  {#if renting}
    <div class="form">
      {#if waiting.length}
        <Select label="Task" bind:value={task_id} options={waiting.map((t) => ({ value: t.id, label: t.title }))} hint="Only tasks that haven’t started can move." />
      {:else}
        <p class="note" role="status">No task is waiting. Create one on the Workspace first; it can run here before it starts.</p>
      {/if}
      <Input label="Most you pay per {per(renting.unit, renting.contract_hours)}" type="number" min="1" inputmode="numeric" bind:value={max_price} hint="In points. The cheapest matching listing wins." />
      {#if renting.warning}
        <p class="note note--warn" role="note"><Icon name="triangle-alert" size={16} /> {renting.warning}</p>
      {/if}
      <fieldset class="secrets">
        <legend class="ml-field__label">Secrets the seller’s machine may use</legend>
        {#each secret_names as name (name)}
          <Checkbox
            checked={allowed.includes(name)}
            onchange={(e: Event) => {
              const on = (e.currentTarget as HTMLInputElement).checked;
              allowed = on ? [...allowed, name] : allowed.filter((n) => n !== name);
            }}
            label={name}
          />
        {:else}
          <p class="note">No secrets stored.</p>
        {/each}
        <span class="ml-field__hint">Unticked secrets never leave your kernel. A push needs its token (e.g. GITHUB_TOKEN).</span>
      </fieldset>
      <Checkbox
        bind:checked={disclosed}
        label="The seller’s machine can see this task’s code and data"
        description="It runs in a fresh VM that only reaches the model API and the task’s git host, and is deleted afterwards. Your points are held until you accept the work."
      />
    </div>
  {/if}
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (rent_open = false)}>Cancel</Button>
    <Button variant="ice" loading={leasing} disabled={!disclosed || !task_id || !(Number(max_price) > 0)} onclick={lease}>Rent</Button>
  {/snippet}
</Dialog>

<Dialog bind:open={dispute_open} onclose={() => (dispute_open = false)} title="Dispute this work">
  <label class="ml-field">
    <span class="ml-field__label">What went wrong?</span>
    <span class="ml-control area"><textarea rows="4" maxlength="2000" bind:value={reason} placeholder="e.g. The task ended without a pull request."></textarea></span>
    <span class="ml-field__hint">The points stay held while Sun looks at it.</span>
  </label>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (dispute_open = false)}>Cancel</Button>
    <Button variant="danger" loading={busy === disputing?.id} disabled={!reason.trim()} onclick={dispute}>Send dispute</Button>
  {/snippet}
</Dialog>

<Dialog bind:open={rate_open} onclose={() => (rate_open = false)} title="Rate this rental">
  <div class="form">
    <Select
      label="Stars"
      bind:value={stars}
      options={[5, 4, 3, 2, 1].map((n) => ({ value: String(n), label: `${n} of 5` }))}
      hint="Shown on the seller’s listing, averaged with other buyers’."
    />
    <label class="ml-field">
      <span class="ml-field__label">Comment (optional)</span>
      <span class="ml-control area"><textarea rows="3" maxlength="500" bind:value={comment} placeholder="e.g. Fast, and the VM had everything the task needed."></textarea></span>
    </label>
  </div>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (rate_open = false)}>Cancel</Button>
    <Button variant="ice" loading={busy === rating?.id} onclick={rate}>Save rating</Button>
  {/snippet}
</Dialog>

<Dialog bind:open={payout_open} onclose={() => (payout_open = false)} title="Pay out earnings">
  <div class="form">
    <Input label="Points" type="number" min="1" inputmode="numeric" bind:value={payout_points} hint="Up to {earnings?.payable ?? 0} points you earned. USD pays in multiples of 10 points." />
    <Select
      label="Currency"
      bind:value={currency}
      options={[
        { value: 'usd', label: 'USD (Stripe)' },
        { value: 'idr', label: 'IDR (bank transfer)' },
      ]}
    />
    <Input
      label="Destination"
      bind:value={destination}
      hint={currency === 'usd' ? 'Your Stripe Connect account, e.g. acct_1Abc…' : 'Bank code, account number and holder, e.g. ID_BCA:1234567890:Budi'}
    />
  </div>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (payout_open = false)}>Cancel</Button>
    <Button variant="ice" loading={busy === 'payout'} disabled={!(Number(payout_points) > 0) || !destination.trim()} onclick={pay_out}>Request payout</Button>
  {/snippet}
</Dialog>

<div class="sun-page">
  <Tabs variant="underline" items={TABS} bind:value={tab} />

  {#if tab === 'rent'}
    {#if failure}
      <div class="sun-empty" role="alert">{failure}</div>
    {:else if !listings}
      <div class="sun-empty" role="status">Loading…</div>
    {:else}
      <section aria-labelledby="leases-title">
        <h2 class="sun-label" id="leases-title">Your rentals</h2>
        <div class="list">
          {#each leases as l (l.id)}
            <article class="sun-row">
              <span class="sun-tile"><Icon name="store" size={18} /></span>
              <div class="sun-row__body">
                <div class="sun-row__title">{title(l.task_ref)}</div>
                <div class="sun-row__meta">
                  {#if model(l.listing_id)}<span class="ml-mono">{model(l.listing_id)}</span> ·{/if}
                  {l.price_points} pts / {l.unit} · started <span class="ml-mono">{clock(l.started_at)}</span>
                  {#if l.escrow === 'held' && l.release_at} · paid to the seller <span class="ml-mono">{clock(l.release_at)}</span> unless you dispute{/if}
                </div>
                {#if l.dispute}<div class="sun-row__meta">Dispute: {l.dispute}</div>{/if}
              </div>
              <div class="sun-row__aside">
                <div class="sun-chips">
                  {#if l.status === 'open'}<Badge tone="info" dot live>Running</Badge>{/if}
                  <Badge tone={ESCROW[l.escrow].tone}>{ESCROW[l.escrow].label}</Badge>
                  {#if l.rating}<Badge tone="neutral"><Icon name="star" size={12} /> You rated {l.rating} of 5</Badge>{/if}
                </div>
                <div class="sun-chips">
                  {#if l.status === 'closed' && !l.rating}
                    <Button
                      size="sm"
                      variant="secondary"
                      iconLeft="star"
                      onclick={() => {
                        rating = l;
                        stars = '5';
                        comment = '';
                        rate_open = true;
                      }}>Rate</Button
                    >
                  {/if}
                  {#if l.escrow === 'held'}
                    <Button
                      size="sm"
                      variant="secondary"
                      onclick={() => {
                        disputing = l;
                        reason = '';
                        dispute_open = true;
                      }}>Dispute</Button
                    >
                    <Button size="sm" variant="ice" iconLeft="check" loading={busy === l.id} onclick={() => accept(l)}>Accept</Button>
                  {/if}
                </div>
              </div>
            </article>
          {:else}
            <div class="sun-empty">Nothing rented yet. Pick capacity below for a task that’s waiting.</div>
          {/each}
        </div>
      </section>

      <section aria-labelledby="listings-title">
        <h2 class="sun-label" id="listings-title">Capacity for sale</h2>
        <div class="filters" role="search" aria-label="Filter capacity">
          <Input label="Region" placeholder="e.g. id or sg-sin" bind:value={filter.region} oninput={refilter} />
          <Select
            label="Offering"
            bind:value={filter.offering}
            onchange={refilter}
            options={[{ value: '', label: 'Any' }, ...Object.entries(OFFERING).map(([value, o]) => ({ value, label: o.label }))]}
          />
          <Select
            label="Capability"
            bind:value={filter.capability}
            onchange={refilter}
            options={[{ value: '', label: 'Any' }, ...Object.entries(CAPABILITY).map(([value, label]) => ({ value, label }))]}
          />
          <Select
            label="Rating"
            value={String(filter.min_rating ?? '')}
            onchange={(e: Event) => {
              const v = (e.currentTarget as HTMLSelectElement).value;
              filter.min_rating = v ? Number(v) : undefined;
              refilter();
            }}
            options={[
              { value: '', label: 'Any' },
              { value: '4', label: '4 and up' },
              { value: '4.5', label: '4.5 and up' },
            ]}
          />
        </div>
        <div class="list">
          {#each listings as l (l.id)}
            <article class="sun-row">
              <span class="sun-tile"><Icon name={OFFERING[l.offering].icon} size={18} /></span>
              <div class="sun-row__body">
                <div class="sun-row__title"><span class="ml-mono">{l.model}</span></div>
                <div class="sun-row__meta">
                  {OFFERING[l.offering].label} · {l.runtime} · <span class="ml-mono">{l.region}</span> · {l.concurrency} at once
                </div>
                <div class="sun-chips chips">
                  {#if l.rating}
                    <Badge tone="neutral"><Icon name="star" size={12} /> {l.rating.toFixed(1)} ({l.rating_count})</Badge>
                  {:else}
                    <Badge tone="neutral">No ratings yet</Badge>
                  {/if}
                  {#if l.host_verified}<Badge tone="success"><Icon name="shield-check" size={12} /> Verified host</Badge>{/if}
                  {#each l.capabilities as c (c)}<Badge tone="neutral">{capability(c)}</Badge>{/each}
                  {#if l.warning}<Badge tone="warning">May break the provider’s terms</Badge>{/if}
                </div>
              </div>
              <div class="sun-row__aside">
                <span class="price"><strong>{l.price_points}</strong> pts / {per(l.unit, l.contract_hours)}</span>
                <Button size="sm" variant="ice" onclick={() => rent(l)}>Rent</Button>
              </div>
            </article>
          {:else}
            <div class="sun-empty">No capacity matches. Loosen the filters, or check back later.</div>
          {/each}
        </div>
      </section>
    {/if}
  {:else if tab === 'sell'}
    {#if sell_failure}
      <div class="sun-empty" role="alert">{sell_failure}</div>
    {:else if !mine || !earnings}
      <div class="sun-empty" role="status">Loading…</div>
    {:else}
      <section aria-labelledby="earnings-title">
        <h2 class="sun-label" id="earnings-title">Earnings</h2>
        <article class="sun-row">
          <span class="sun-tile"><Icon name="wallet" size={18} /></span>
          <div class="sun-row__body">
            <div class="sun-row__title"><strong>{earnings.payable}</strong> points ready to pay out</div>
            <div class="sun-row__meta">Rentals pay out once the buyer accepts, or after the dispute window.</div>
          </div>
          <div class="sun-row__aside">
            <Button
              size="sm"
              variant="ice"
              disabled={earnings.payable <= 0}
              onclick={() => {
                payout_points = String(earnings?.payable ?? '');
                destination = '';
                payout_open = true;
              }}>Pay out</Button
            >
          </div>
        </article>
        {#each earnings.payouts as p (p.id)}
          <div class="sun-row__meta payout">
            {p.points} pts → {p.amount} {p.currency.toUpperCase()} · <span class="ml-mono">{clock(p.created_at)}</span> · {p.sent_at ? 'Sent' : 'Pending'}
          </div>
        {/each}
      </section>

      <section aria-labelledby="mine-title">
        <h2 class="sun-label" id="mine-title">Your listings</h2>
        <div class="list">
          {#each mine as l (l.id)}
            <article class="sun-row">
              <span class="sun-tile"><Icon name={OFFERING[l.offering].icon} size={18} /></span>
              <div class="sun-row__body">
                <div class="sun-row__title"><span class="ml-mono">{l.model}</span></div>
                <div class="sun-row__meta">
                  {l.runtime} · <span class="ml-mono">{l.region}</span> · {l.price_points} pts / {per(l.unit, l.contract_hours)} · {l.concurrency} at once
                </div>
                <div class="sun-chips chips">
                  <Badge tone={l.status === 'paused' ? 'neutral' : 'success'}>{l.status === 'paused' ? 'Paused' : 'Live'}</Badge>
                  {#if l.host_verified}<Badge tone="success"><Icon name="shield-check" size={12} /> Verified host</Badge>{:else}<Badge tone="warning">Host not verified</Badge>{/if}
                  {#if l.rating}<Badge tone="neutral"><Icon name="star" size={12} /> {l.rating.toFixed(1)} ({l.rating_count})</Badge>{/if}
                </div>
              </div>
              <div class="sun-row__aside">
                <Button size="sm" variant="secondary" iconLeft={l.status === 'paused' ? 'play' : 'pause'} loading={busy === l.id} onclick={() => toggle(l)}>
                  {l.status === 'paused' ? 'Resume' : 'Pause'}
                </Button>
              </div>
            </article>
          {:else}
            <div class="sun-empty">
              Nothing listed. On the machine you rent out, run <code class="ml-mono">sun market list</code>: it proves the host is yours and lists it.
            </div>
          {/each}
        </div>
      </section>
    {/if}
  {:else if policy_failure}
    <div class="sun-empty" role="alert">{policy_failure}</div>
  {:else if !policy}
    <div class="sun-empty" role="status">Loading…</div>
  {:else}
    <section aria-labelledby="policy-title" class="form">
      <h2 class="sun-label" id="policy-title">Who may use marketplace runtimes</h2>
      <fieldset class="secrets">
        <legend class="sr-only">Allowed workflows</legend>
        <Radio bind:group={policy.allow} name="allow" value="all" label="Every workflow" description="Any task can be rented out." />
        <Radio bind:group={policy.allow} name="allow" value="workflows" label="Chosen workflows" description="Only tasks of the workflows ticked below." />
        <Radio bind:group={policy.allow} name="allow" value="none" label="No workflow" description="The marketplace is off for this ecosystem." />
      </fieldset>
      {#if policy.allow === 'workflows'}
        <fieldset class="secrets">
          <legend class="ml-field__label">Workflows</legend>
          {#each workflow_names as name (name)}
            <Checkbox
              checked={policy.workflows.includes(name)}
              onchange={(e: Event) => {
                if (!policy) return;
                const on = (e.currentTarget as HTMLInputElement).checked;
                policy.workflows = on ? [...policy.workflows, name] : policy.workflows.filter((n) => n !== name);
              }}
              label={name}
            />
          {:else}
            <p class="note">No workflows yet.</p>
          {/each}
        </fieldset>
      {/if}
      <Input
        label="Failover price cap, points per task"
        type="number"
        min="1"
        inputmode="numeric"
        bind:value={failover_cap}
        hint="A workflow chain entry like market:claude-code rents capacity when your own runtimes hit their limits, up to this price. Empty: never automatically."
      />
      <div>
        <Button variant="ice" loading={busy === 'policy'} onclick={save_policy}>Save policy</Button>
      </div>
    </section>
  {/if}
</div>

<style>
  .list {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  section {
    margin-top: var(--space-4);
  }
  .form {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .filters {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
    gap: var(--space-3);
    margin-bottom: var(--space-3);
  }
  .chips {
    margin-top: var(--space-2);
    flex-wrap: wrap;
  }
  .secrets {
    border: 0;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .payout {
    margin-top: var(--space-2);
  }
  .note {
    margin: 0;
    font-size: var(--size-sm);
    color: var(--fg-2);
    display: flex;
    gap: var(--space-2);
    align-items: flex-start;
  }
  .note--warn {
    color: var(--fg-1);
  }
  .price {
    font-size: var(--size-sm);
    color: var(--fg-2);
    white-space: nowrap;
  }
  .price strong {
    color: var(--fg-1);
    font-size: 15px;
  }
  .area {
    height: auto;
    padding: var(--space-2) var(--space-3);
  }
  .area textarea {
    width: 100%;
    border: 0;
    outline: 0;
    resize: vertical;
    background: transparent;
    color: inherit;
    font: inherit;
    font-size: var(--size-body);
  }
  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
  }
</style>
