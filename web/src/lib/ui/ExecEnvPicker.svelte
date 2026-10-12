<!-- Where a task's agent sessions run. Offers only the kinds an online host
     reports ready; says why the others are missing. -->
<script lang="ts">
  import { Input, Select } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../api/client';
  import { current_workspace } from '../app_state.svelte';
  import type { ExecEnvCapability, ExecEnvKind, ExecEnvSpec } from '../api/types';

  let { value = $bindable(null), error = '' }: { value?: ExecEnvSpec | null; error?: string } = $props();

  const LABEL: Record<ExecEnvKind, string> = {
    bare: 'On the host (no isolation)',
    container: 'Container',
    vm: 'Linux VM',
  };

  let reported = $state<ExecEnvCapability[]>([]);
  let kind = $state<'' | ExecEnvKind>('');
  let image = $state('');
  let template = $state('default');

  api
    .list_hosts(current_workspace())
    .then((hosts) => {
      reported = hosts.filter((h) => h.status === 'online').flatMap((h) => h.capabilities.exec_envs ?? []);
    })
    .catch(() => (reported = []));

  const ready = $derived(new Set(reported.filter((e) => e.ready).map((e) => e.kind)));
  const options = $derived([
    { value: '', label: 'Workflow default' },
    ...(['bare', 'container', 'vm'] as const).filter((k) => ready.has(k)).map((k) => ({ value: k, label: LABEL[k] })),
  ]);
  const missing = $derived(
    (['container', 'vm'] as const)
      .filter((k) => !ready.has(k))
      .map((k) => `${LABEL[k]}: ${reported.find((e) => e.kind === k)?.detail ?? 'no online host reports it'}`),
  );

  $effect(() => {
    value =
      kind === 'container' ? { kind, image: image.trim() } : kind === 'vm' ? { kind, template: template.trim() } : kind === 'bare' ? { kind } : null;
  });
</script>

<div class="exec-env">
  <Select label="Runs in" {options} bind:value={kind} hint={kind ? undefined : 'Uses the workflow’s exec_env (on the host if it sets none).'} />
  {#if kind === 'container'}
    <Input label="Image" mono placeholder="ghcr.io/acme/toolchain:1.4" hint="Must contain the agent CLI." {error} bind:value={image} />
  {:else if kind === 'vm'}
    <Input label="Lima template" mono placeholder="default" {error} bind:value={template} />
  {/if}
  {#if missing.length}
    <details class="why">
      <summary>Why not {missing.length === 2 ? 'containers or VMs' : missing[0].split(':')[0].toLowerCase()}?</summary>
      <ul>
        {#each missing as line (line)}<li>{line}</li>{/each}
      </ul>
    </details>
  {/if}
</div>

<style>
  .exec-env { display: grid; gap: var(--space-3); min-width: 0; }
  .why { font-size: var(--size-sm); color: var(--fg-2); }
  .why summary { cursor: pointer; min-height: 44px; display: flex; align-items: center; }
  .why ul { margin: 0; padding-left: var(--space-4); display: grid; gap: var(--space-1); overflow-wrap: anywhere; }
</style>
