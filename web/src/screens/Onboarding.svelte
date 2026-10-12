<!-- First-run set-up over Tasks: your role and the ecosystem limit policy.
     Opens once per browser while the ecosystem has no limit policy yet; Skip or Save closes it for good. -->
<script lang="ts">
  import { Button, Dialog, Radio, Select } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../lib/api/client';
  import { auth } from '../lib/api/auth';
  import type { OnLimit } from '../lib/api/types';
  import { app, error_text, local_get, local_set, set_role, toast } from '../lib/app_state.svelte';
  import { ON_LIMIT, ROLE } from '../lib/format';

  const DONE_KEY = 'sun_onboarded';
  const ROLES = Object.entries(ROLE).map(([value, label]) => ({ value, label }));

  let open = $state(false);
  let can_set = $state(false);
  let role = $state(app.role || 'engineering');
  let policy = $state<OnLimit>('switch');
  let saving = $state(false);

  if (!local_get(DONE_KEY)) {
    // Advisory: an older kernel without the route, or a failed call, just skips set-up.
    Promise.all([api.get_on_limit(), auth.me().catch(() => null)]).then(([ecosystem, me]) => {
      can_set = !!me?.is_owner;
      open = ecosystem.on_limit === null;
    }, () => {});
  }

  function finish() {
    local_set(DONE_KEY, '1');
    open = false;
  }

  async function save(e: SubmitEvent) {
    e.preventDefault();
    saving = true;
    try {
      if (can_set) await api.set_on_limit(policy);
      await set_role(role);
      finish();
      toast('success', 'You’re set up', 'Change these any time in Settings and Workflows.');
    } catch (err) {
      toast('danger', 'Not saved', error_text(err));
    }
    saving = false;
  }
</script>

<Dialog bind:open onclose={finish} title="Set up Sun">
  <form id="setup" class="setup" aria-label="Set up Sun" onsubmit={save}>
    <p class="setup__lead">Two quick questions. You can change both later.</p>
    <Select label="Your role" hint="Marketing, product and leadership see plain words: no repos, branches or runtimes." options={ROLES} bind:value={role} />
    {#if can_set}
      <fieldset class="setup__policy">
        <legend class="ml-field__label">When a runtime hits its usage limit</legend>
        {#each ON_LIMIT as option (option.value)}
          <Radio bind:group={policy} name="on_limit" value={option.value} label={option.label} description={option.description} />
        {/each}
      </fieldset>
    {:else}
      <p class="setup__lead">An admin picks what happens when a runtime hits its usage limit.</p>
    {/if}
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={finish}>Skip for now</Button>
    <Button variant="ice" type="submit" form="setup" loading={saving}>Save</Button>
  {/snippet}
</Dialog>

<style>
  .setup {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .setup__lead {
    margin: 0;
    font-size: 14px;
    color: var(--fg-2);
  }
  .setup__policy {
    border: 0;
    margin: 0;
    padding: 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .setup__policy legend {
    padding: 0;
    margin-bottom: var(--space-1);
  }
</style>
