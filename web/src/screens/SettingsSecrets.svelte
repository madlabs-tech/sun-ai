<!-- Secrets, admins only: names and update times per scope (the ecosystem, or one
     installed mod). Values are write-only: they go in through a password field and never
     come back from the kernel. -->
<script lang="ts">
  import { Button, Dialog, Icon, Input, Tabs } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import { api } from '../lib/api/client';
  import type { Secret, SecretScope } from '../lib/api/types';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { day } from '../lib/format';

  /** The single ecosystem's id (the nil ULID until ecosystems have rows). */
  const ECOSYSTEM = '00000000000000000000000000';
  const NAME = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;

  let tab = $state('ecosystem');
  let mods = $state<string[]>([]);
  api.list_mods().then((all) => (mods = all.filter((m) => m.installed).map((m) => m.name)), () => {});
  const tabs = $derived([{ id: 'ecosystem', label: 'Ecosystem' }, ...mods.map((m) => ({ id: `mod:${m}`, label: m }))]);
  const scope = $derived<SecretScope>(tab === 'ecosystem' ? { kind: 'ecosystem', id: ECOSYSTEM } : { kind: 'mod', id: tab.slice(4) });

  let load = $state<Promise<Secret[]>>(new Promise(() => {}));
  function reload() {
    load = api.list_secrets(scope);
  }
  $effect(() => reload());

  let form = $state({ name: '', value: '' });
  let errors = $state<{ name?: string; value?: string }>({});
  let saving = $state(false);
  let deleting = $state<Secret | null>(null);
  let busy = $state(false);

  async function save(e: SubmitEvent) {
    e.preventDefault();
    const name = form.name.trim();
    errors = {
      name: NAME.test(name) ? undefined : 'Use letters, digits and _ (not starting with a digit), up to 64.',
      value: form.value.length >= 6 ? undefined : 'Use at least 6 characters.',
    };
    if (errors.name || errors.value) return;
    saving = true;
    try {
      await api.set_secret(scope, name, form.value);
      form = { name: '', value: '' };
      toast('success', `${name} saved`, 'Agents and mods get it; it is never shown again.');
      reload();
    } catch (err) {
      toast('danger', 'Secret not saved', error_text(err));
    }
    saving = false;
  }

  async function remove(s: Secret) {
    busy = true;
    try {
      await api.delete_secret(s.scope, s.name);
      toast('success', `${s.name} deleted`);
    } catch (err) {
      toast('danger', 'Secret not deleted', error_text(err));
    }
    busy = false;
    deleting = null;
    reload();
  }
</script>

<section class="sun-row" aria-labelledby="secrets-head">
  <span class="sun-tile"><Icon name="key-round" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="secrets-head">Secrets</div>
    <p class="sun-row__meta secrets__copy">
      Sealed in the store and redacted from every log, event and context pack. Mod secrets reach only that mod.
    </p>
    {#if tabs.length > 1}<div class="secrets__tabs"><Tabs variant="underline" bind:value={tab} items={tabs} /></div>{/if}
    <div class="secrets__list">
      <Loaded {load} empty="No secrets here yet.">
        {#snippet row(s)}
          <div class="secrets__item">
            <span class="ml-mono secrets__name">{s.name}</span>
            <span class="sun-row__meta">updated {day(s.updated_at)}</span>
            <IconButton icon="trash-2" label="Delete {s.name}" variant="ghost" size="sm" onclick={() => (deleting = s)} />
          </div>
        {/snippet}
      </Loaded>
    </div>
    <form class="secrets__form" aria-label="Add or replace a secret" onsubmit={save} novalidate>
      <Input label="Name" mono autocomplete="off" placeholder="API_TOKEN" bind:value={form.name} error={errors.name} />
      <Input label="Value" type="password" mono autocomplete="new-password" hint="Write-only. Saving an existing name replaces it." bind:value={form.value} error={errors.value} />
      <Button type="submit" variant="ice" loading={saving}>Save secret</Button>
    </form>
  </div>
</section>

{#if deleting}
  {@const s = deleting}
  <Dialog open onclose={() => (deleting = null)} title="Delete {s.name}?">
    <p style="margin:0;color:var(--fg-2);line-height:1.5">
      Runs and mods that use it lose access. Its value is gone for good; you would have to enter it again.
    </p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (deleting = null)}>Keep it</Button>
      <Button variant="danger" loading={busy} onclick={() => remove(s)}>Delete secret</Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .secrets__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
  .secrets__tabs {
    margin-top: var(--space-3);
    overflow-x: auto;
  }
  .secrets__list {
    margin: var(--space-3) 0 0;
    padding: 0;
    max-width: 480px;
  }
  .secrets__item {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    min-height: 44px;
    border-bottom: 1px solid var(--border-subtle);
  }
  .secrets__name {
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .secrets__form {
    display: grid;
    gap: var(--space-3);
    margin-top: var(--space-4);
    max-width: 480px;
    justify-items: start;
  }
  .secrets__form :global(.ml-field) {
    justify-self: stretch;
  }
</style>
