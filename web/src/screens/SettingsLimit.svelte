<!-- Settings: the ecosystem's limit policy, used when neither the task nor its workflow sets one. -->
<script lang="ts">
  import { Badge, Button, Icon, Radio } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../lib/api/client';
  import { auth } from '../lib/api/auth';
  import type { OnLimit } from '../lib/api/types';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { ON_LIMIT } from '../lib/format';

  let saved = $state<OnLimit | null | undefined>(undefined);
  let policy = $state<OnLimit>('switch');
  let can_set = $state(false);
  let load_error = $state('');
  let saving = $state(false);

  Promise.all([api.get_on_limit(), auth.me().catch(() => null)]).then(([view, me]) => {
    [saved, policy] = [view.on_limit, view.on_limit ?? 'switch'];
    can_set = !!me?.permissions.includes('manage_runtimes');
  }, (e) => (load_error = error_text(e)));

  async function save(e: SubmitEvent) {
    e.preventDefault();
    saving = true;
    try {
      saved = (await api.set_on_limit(policy)).on_limit;
      toast('success', 'Limit policy saved', 'Tasks and workflows that set their own keep it.');
    } catch (err) {
      toast('danger', 'Not saved', error_text(err));
    }
    saving = false;
  }
</script>

<form class="sun-row" aria-labelledby="limit-head" onsubmit={save} data-testid="limit-policy">
  <span class="sun-tile"><Icon name="gauge" size={18} /></span>
  <div class="sun-row__body limit">
    <div class="sun-row__title" id="limit-head">
      When a runtime hits its usage limit
      {#if saved === null}<Badge tone="neutral">Not picked yet</Badge>{/if}
    </div>
    <p class="sun-row__meta">The ecosystem default. A workflow’s <span class="ml-mono">runtime.on_limit</span> and a task’s own setting come first.</p>
    {#if load_error}
      <div class="sun-empty" role="alert">{load_error}</div>
    {:else if saved !== undefined}
      <fieldset class="limit__options" aria-labelledby="limit-head" disabled={!can_set}>
        {#each ON_LIMIT as option (option.value)}
          <Radio bind:group={policy} name="ecosystem_on_limit" value={option.value} label={option.label} description={option.description} />
        {/each}
      </fieldset>
      {#if can_set}
        <div class="limit__actions"><Button type="submit" variant="ice" loading={saving} disabled={policy === saved}>Save</Button></div>
      {:else}
        <p class="sun-row__meta">Your role can’t change this. Ask someone with the “manage runtimes” permission.</p>
      {/if}
    {:else}
      <div class="sun-empty" role="status">Loading…</div>
    {/if}
  </div>
</form>

<style>
  .limit__options {
    border: 0;
    margin: var(--space-3) 0 0;
    padding: 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    max-width: 560px;
  }
  .limit__actions {
    margin-top: var(--space-3);
  }
</style>
