<!-- Admin: import a repo or knowledge source from another workspace into the current one. It stays
     one row (one clone, one index), now on both workspaces. `options` are the items not on this workspace. -->
<script lang="ts">
  import { Button, Select } from '@hryer/madlabs-design-system/svelte';
  import { act } from '../app_state.svelte';

  let { label, options, on_import }: {
    label: string;
    options: { value: string; label: string }[];
    /** Imports `id`; the caller reloads its list afterwards. */
    on_import: (id: string) => Promise<unknown>;
  } = $props();

  let picked = $state('');
  let busy = $state(false);

  async function run() {
    const option = options.find((o) => o.value === picked);
    if (!option) return;
    busy = true;
    if (await act(() => on_import(option.value), `${option.label} imported`)) picked = '';
    busy = false;
  }
</script>

<div class="import">
  <Select
    {label}
    size="sm"
    placeholder={options.length ? 'Pick one from another workspace' : 'Nothing to import'}
    disabled={!options.length}
    {options}
    bind:value={picked}
    style="flex:1;min-width:0"
  />
  <Button type="button" size="sm" variant="secondary" iconLeft="download" disabled={!picked} loading={busy} onclick={run}>Import</Button>
</div>
<p class="import__hint">It stays one copy, now on both workspaces.</p>

<style>
  .import__hint {
    margin: 4px 0 0;
    font-size: 13px;
    color: var(--fg-2);
  }
  .import {
    display: flex;
    align-items: flex-end;
    gap: var(--space-2);
    flex-wrap: wrap;
  }
</style>
