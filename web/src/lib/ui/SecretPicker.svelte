<!-- Picks a secret of `scope` by name (values never come back), or adds one write-only: the open workspace's for
     a BYO-key runtime, the ecosystem's (the default) for the MCP server form and catalog installs. -->
<script lang="ts">
  import { Button, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import { ECOSYSTEM_SCOPE, api } from '../api/client';
  import type { SecretScope } from '../api/types';
  import { error_text } from '../app_state.svelte';

  const ADD = '__add';

  let {
    value = $bindable(''),
    label = 'API key secret',
    hint = 'Stored sealed; Sun only keeps its name here.',
    none_label = '',
    error = '',
    scope = ECOSYSTEM_SCOPE,
  }: {
    value?: string;
    label?: string;
    hint?: string;
    /** When set, offers "no secret" under this label (value ''). */
    none_label?: string;
    error?: string;
    scope?: SecretScope;
  } = $props();

  let names = $state<string[]>([]);
  let choice = $state(value);
  let new_name = $state('');
  let new_value = $state('');
  let add_error = $state('');
  let saving = $state(false);

  async function load() {
    try {
      names = (await api.list_secrets(scope)).map((s) => s.name);
    } catch {
      names = []; // members without admin can't list; adding still shows the error
    }
  }
  load();

  const options = $derived([
    ...(none_label ? [{ value: '', label: none_label }] : []),
    ...[...new Set([...names, ...(value ? [value] : [])])].map((n) => ({ value: n, label: n })),
    { value: ADD, label: 'Add secret…' },
  ]);

  $effect(() => {
    if (choice !== ADD) value = choice;
  });

  async function save() {
    saving = true;
    add_error = '';
    try {
      const name = new_name.trim();
      await api.set_secret(scope, name, new_value);
      await load();
      choice = name;
      new_name = new_value = '';
    } catch (err) {
      add_error = error_text(err);
    } finally {
      saving = false;
    }
  }
</script>

<div class="secret-picker">
  <Select {label} {hint} {error} {options} placeholder={none_label ? undefined : 'Choose a secret'} bind:value={choice} />
  {#if choice === ADD}
    <div class="add" role="group" aria-label="Add a secret">
      <Input label="Secret name" mono placeholder="OPENROUTER_KEY" bind:value={new_name} />
      <Input label="Value" type="password" autocomplete="off" hint="Write-only: it can't be read back." error={add_error} bind:value={new_value} />
      <div class="actions">
        <Button variant="ghost" onclick={() => (choice = value)}>Cancel</Button>
        <Button variant="primary" loading={saving} disabled={!new_name.trim() || !new_value} onclick={save}>Save secret</Button>
      </div>
    </div>
  {/if}
</div>

<style>
  .secret-picker { display: grid; gap: var(--space-3); }
  .add {
    display: grid;
    gap: var(--space-3);
    padding: var(--space-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
  }
  .actions { display: flex; gap: var(--space-2); justify-content: flex-end; flex-wrap: wrap; }
</style>
