<!-- Settings: the ecosystem's monthly spend cap. Reaching it holds new agent runs in the queue and
     stops running agent steps; it counts the cost runtimes report (API keys, Sun credits), not subscriptions. -->
<script lang="ts">
  import { Badge, Button, Icon, Input } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../lib/api/client';
  import { auth } from '../lib/api/auth';
  import type { Spend } from '../lib/api/types';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { money, usd } from '../lib/format';

  let spend = $state<Spend | null>(null);
  let cap = $state('');
  let can_set = $state(false);
  let load_error = $state('');
  let saving = $state(false);
  const parsed = $derived(usd(cap));
  const cap_error = $derived(Number.isNaN(parsed) ? 'Use an amount in dollars, 0 or more.' : '');
  const reached = $derived(!!spend && spend.monthly_usd !== null && spend.spent_usd >= spend.monthly_usd);

  function fill(view: Spend) {
    spend = view;
    cap = view.monthly_usd?.toString() ?? '';
  }

  Promise.all([api.get_spend(), auth.me().catch(() => null)]).then(([view, me]) => {
    fill(view);
    can_set = !!me?.permissions.includes('manage_runtimes');
  }, (e) => (load_error = error_text(e)));

  async function save(value: number | null) {
    saving = true;
    try {
      fill(await api.set_spend_cap(value));
      toast('success', value === null ? 'Spend cap removed' : 'Spend cap saved', value === null ? 'Agents spend without a monthly limit.' : 'Runs waiting on the cap start once there is room.');
    } catch (err) {
      toast('danger', 'Not saved', error_text(err));
    }
    saving = false;
  }

  function submit(e: SubmitEvent) {
    e.preventDefault();
    if (!cap_error) void save(parsed);
  }
</script>

<form class="sun-row" aria-labelledby="spend-head" onsubmit={submit} novalidate>
  <span class="sun-tile"><Icon name="wallet" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="spend-head">
      Monthly spend cap
      {#if reached}<Badge tone="danger" dot>Cap reached</Badge>{/if}
    </div>
    <p class="sun-row__meta spend__copy">
      Once reached, new agent runs wait in the queue and running agent steps stop. It counts what API keys and Sun credits
      report; subscriptions report no cost. Resets on the 1st (UTC).
    </p>
    {#if load_error}
      <div class="sun-empty" role="alert">{load_error}</div>
    {:else if spend}
      <p class="spend__used" role="status">
        <span class="ml-mono">{money(spend.spent_usd)}</span>
        {spend.monthly_usd === null ? 'spent this month, no cap' : `of ${money(spend.monthly_usd)} spent this month`}
      </p>
      {#if can_set}
        <div class="spend__fields">
          <Input
            label="Cap (USD per month)"
            type="number"
            inputmode="decimal"
            min="0"
            step="0.01"
            mono
            placeholder="No cap"
            hint="Leave empty for no cap."
            bind:value={cap}
            error={cap_error || undefined}
          />
        </div>
        <div class="spend__actions">
          <Button type="submit" variant="ice" loading={saving}>Save</Button>
          {#if spend.monthly_usd !== null}
            <Button variant="secondary" disabled={saving} onclick={() => save(null)}>Remove cap</Button>
          {/if}
        </div>
      {:else}
        <p class="sun-row__meta">Your role can’t change this. Ask someone with the “manage runtimes” permission.</p>
      {/if}
    {:else}
      <div class="sun-empty" role="status">Loading…</div>
    {/if}
  </div>
</form>

<style>
  .spend__copy {
    line-height: 1.5;
    max-width: 60ch;
  }
  .spend__used {
    margin: var(--space-3) 0 0;
    font-size: 14px;
    color: var(--fg-1);
  }
  .spend__fields {
    margin-top: var(--space-3);
    max-width: 240px;
  }
  .spend__actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin-top: var(--space-3);
  }
</style>
