<!-- The plan badge: which plan this ecosystem runs on and why, plus "Install license"
     whenever no valid license is active. -->
<script lang="ts">
  import { Badge, Button, Icon, Input } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../lib/api/client';
  import type { Entitlement } from '../lib/api/types';
  import { can, error_text, toast } from '../lib/app_state.svelte';
  import { day } from '../lib/format';

  const PLAN = { community: 'Self-hosted', free: 'Free', individual: 'Individual', business: 'Business' };

  let current = $state<Entitlement | null>(null);
  let load_error = $state('');
  let token = $state('');
  let token_error = $state('');
  let saving = $state(false);

  api.get_entitlement().then((e) => (current = e), (e) => (load_error = error_text(e)));

  const tone = (e: Entitlement) =>
    ({ unlicensed: 'neutral', active: 'success', grace: 'warning', expired: 'danger', invalid: 'danger' } as const)[e.status.state];

  function detail(e: Entitlement): string {
    const s = e.status;
    switch (s.state) {
      case 'unlicensed':
        return 'Community: every engine feature, no limits. A license adds Sun Cloud plans and exclusive mods.';
      case 'active':
        return `Licensed to ${e.license?.licensee ?? '—'}${e.license ? `, renews by ${day(e.license.expires_at)}` : ''}.`;
      case 'grace':
        return `License expired, works offline until ${day(s.ends_at)}. Connect to renew it.`;
      case 'expired':
        return `License expired ${day(s.ended_at)}; back on Community. Install a new one.`;
      case 'invalid':
        return `The installed license didn’t verify (${s.reason}); back on Community.`;
    }
  }

  /** The caps people meet: seats, hosts and agents at once (`runtimes` counts hosts). */
  function limit_text(e: Entitlement): string {
    const l = e.limits ?? {};
    return [
      [l.seats, 'seats'],
      [l.runtimes, 'hosts'],
      [l.agents, 'agents at once'],
    ]
      .filter(([n]) => typeof n === 'number')
      .map(([n, what]) => `${n} ${what}`)
      .join(' · ');
  }

  async function install(e: SubmitEvent) {
    e.preventDefault();
    token_error = token.trim() ? '' : 'Paste the license token.';
    if (token_error) return;
    saving = true;
    try {
      current = await api.install_license(token.trim());
      token = '';
      toast('success', 'License installed', `Plan: ${PLAN[current.plan]}.`);
    } catch (err) {
      token_error = error_text(err);
    }
    saving = false;
  }
</script>

<section class="sun-row" aria-labelledby="plan-head">
  <span class="sun-tile"><Icon name="shield-check" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="plan-head">
      Plan
      {#if current}<Badge tone={tone(current)} dot>{current.status.state === 'unlicensed' ? PLAN.community : PLAN[current.plan]}</Badge>{/if}
    </div>
    {#if load_error}
      <div class="sun-empty" role="alert">{load_error}</div>
    {:else if current}
      <p class="sun-row__meta plan__copy">{detail(current)}</p>
      {@const caps = limit_text(current)}
      {#if caps}<p class="sun-row__meta plan__copy plan__limits">Plan limits: {caps}. Sun refuses more and says why.</p>{/if}
      {#if current.status.state !== 'active' && can('manage_settings')}
        <form class="plan__form" onsubmit={install} novalidate>
          <Input label="License token" mono autocomplete="off" placeholder="sunlic1.…" bind:value={token} error={token_error} />
          <Button type="submit" variant="ice" loading={saving}>Install license</Button>
        </form>
      {/if}
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
  .plan__form {
    display: grid;
    gap: var(--space-3);
    margin-top: var(--space-4);
    max-width: 480px;
    justify-items: start;
  }
  .plan__form :global(.ml-field) {
    justify-self: stretch;
  }
</style>
