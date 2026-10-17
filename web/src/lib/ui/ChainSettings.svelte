<!-- Per-entry settings of a runtime chain: the host it must run on, the model and the effort level.
     Hosts are those that reported the tool; models and effort levels are exactly what the CLI on those
     hosts reported (GET /runtimes `models`, `effort_options`), per model where the CLI scopes them.
     Nothing is hardcoded: a tool with no catalog greys the effort picker out. An entry with no
     setting stays a bare name, so an older chain saves back unchanged. -->
<script lang="ts">
  import { Input, Select } from '@hryer/madlabs-design-system/svelte';
  import type { ChainEntry, ChainSpec, EffortLevel, Runtime } from '../api/types';
  import { entry_name, tool_label } from '../format';

  let { chain = $bindable([]), runtimes }: { chain?: ChainEntry[]; runtimes: Runtime[] } = $props();
  const uid = $props.id();

  /** The tool (and paying key) an entry runs; `null` when Sun knows none (market, extension, unreported). */
  function target(e: ChainEntry): { tool: string; credential?: string } | null {
    if (typeof e !== 'string') return { tool: e.tool, credential: e.credential };
    const r = runtimes.find((x) => x.name === e);
    if (!r || e.startsWith('market:')) return null;
    return r.host ? { tool: r.tool } : { tool: r.adapter, credential: r.name };
  }

  const spec_of = (e: ChainEntry): ChainSpec | null => {
    if (typeof e !== 'string') return e;
    const t = target(e);
    return t && { tool: t.tool, ...(t.credential ? { credential: t.credential } : {}) };
  };

  /** The hosts' reports of `tool`: the pinned host's, else every host's. */
  const reports = (s: ChainSpec) => runtimes.filter((r) => r.host && r.tool === s.tool && (!s.host || r.host.id === s.host));

  /** Effort levels for the entry's model: each report's per-model list when it has the model, else its tool-wide one. */
  function efforts(s: ChainSpec): EffortLevel[] {
    const out: EffortLevel[] = [];
    for (const r of reports(s)) {
      const levels = r.models.find((m) => m.id === s.model)?.effort_options ?? r.effort_options;
      for (const l of levels) if (!out.some((o) => o.value === l.value)) out.push(l);
    }
    return out;
  }

  const models = (s: ChainSpec) => [...new Set(reports(s).flatMap((r) => r.models.map((m) => m.id)))];

  function host_options(s: ChainSpec) {
    const hosts = runtimes.filter((r) => r.host && r.tool === s.tool && (r.presence === 'online' || r.host.id === s.host));
    return [
      { value: '', label: 'Any host' },
      ...hosts.map((r) => ({ value: r.host!.id, label: r.presence === 'online' ? r.host!.name : `${r.host!.name} (offline)` })),
    ];
  }

  /** Sets (or with '' clears) one field of entry `i`; an effort the new host or model doesn't offer is dropped. */
  function set(i: number, field: 'host' | 'model' | 'effort', value: string) {
    const s = spec_of(chain[i]);
    if (!s) return;
    const next: ChainSpec = { ...s };
    if (value.trim()) next[field] = value.trim();
    else delete next[field];
    if (next.effort && !efforts(next).some((l) => l.value === next.effort)) delete next.effort;
    const plain = !next.host && !next.model && !next.effort;
    chain = chain.map((e, j) => (j === i ? (plain ? entry_name(next) : next) : e));
  }
</script>

{#each chain as entry, i (entry_name(entry))}
  {@const s = spec_of(entry)}
  {@const name = entry_name(entry)}
  <fieldset class="chain-entry" aria-label="Settings of {name}">
    <legend class="chain-entry__name"><span class="ml-mono">{name}</span>{#if s && s.tool !== name} <span class="chain-entry__tool">on {tool_label(s.tool)}</span>{/if}</legend>
    {#if s}
      {@const levels = efforts(s)}
      <div class="chain-entry__fields">
        <Select label="Host" size="sm" value={s.host ?? ''} options={host_options(s)} onchange={(e: Event) => set(i, 'host', (e.currentTarget as HTMLSelectElement).value)} />
        <Input
          label="Model"
          size="sm"
          mono
          placeholder="CLI default"
          autocomplete="off"
          maxlength={128}
          list="{uid}-models-{i}"
          value={s.model ?? ''}
          onchange={(e: Event) => set(i, 'model', (e.currentTarget as HTMLInputElement).value)}
        />
        <datalist id="{uid}-models-{i}">{#each models(s) as m (m)}<option value={m}></option>{/each}</datalist>
        <Select
          label="Effort"
          size="sm"
          value={s.effort ?? ''}
          disabled={!levels.length}
          options={[{ value: '', label: 'CLI default' }, ...levels.map((l) => ({ value: l.value, label: l.label }))]}
          hint={levels.length ? (levels.find((l) => l.value === s.effort)?.description ?? '') : `${tool_label(s.tool)} reports no effort levels${s.host ? ' on this host' : ''}.`}
          onchange={(e: Event) => set(i, 'effort', (e.currentTarget as HTMLSelectElement).value)}
        />
      </div>
    {:else}
      <p class="chain-entry__none">No host reports it yet, so it runs with its defaults.</p>
    {/if}
  </fieldset>
{/each}

<style>
  .chain-entry {
    margin: 0;
    padding: var(--space-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    min-width: 0;
  }
  .chain-entry__name {
    padding: 0 var(--space-1);
    font-size: var(--size-xs);
    color: var(--fg-1);
  }
  .chain-entry__tool,
  .chain-entry__none {
    color: var(--fg-2);
  }
  .chain-entry__none {
    margin: 0;
    font-size: var(--size-xs);
  }
  .chain-entry__fields {
    display: grid;
    gap: var(--space-3);
    grid-template-columns: 1fr;
  }
</style>
