<!-- The plan badge: which plan Main runs on, where it comes from, and its limits. -->
<script lang="ts">
  import { Badge, Icon } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../lib/api/client';
  import type { Entitlement } from '../lib/api/types';
  import { error_text } from '../lib/app_state.svelte';
  import { clock } from '../lib/format';

  const PLAN = { community: 'Self-hosted', free: 'Free', pro: 'Pro', business: 'Business' };

  let current = $state<Entitlement | null>(null);
  let load_error = $state('');

  api.get_entitlement().then((e) => (current = e), (e) => (load_error = error_text(e)));

  const tone = (e: Entitlement) =>
    ({ cloud: 'success', stale: 'warning', unavailable: 'warning', community: 'neutral' } as const)[e.status];

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
      <p class="sun-row__meta plan__copy">{detail(current)}</p>
      {@const caps = limit_text(current)}
      {#if caps}<p class="sun-row__meta plan__copy plan__limits">Plan limits: {caps}. Sun refuses more and says why.</p>{/if}
    {:else}
      <div class="sun-empty" role="status">Loading…</div>
    {/if}
  </div>
</section>

<style>
  .plan__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
</style>
