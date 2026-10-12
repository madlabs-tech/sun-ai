<!-- Mods: installed plugins and extensions with the permissions granted, and the ones
     listed in the mod registry (official or community) with the permissions they ask for (GET /mods),
     each tab split into Plugins and Extensions. Install and update open a consent dialog showing
     every permission (new ones marked); secrets are entered there, write-only. Remove confirms,
     names the extensions that need the plugin, and keeps data and secrets unless asked. -->
<script lang="ts">
  import { Badge, Button, Checkbox, Dialog, Icon, Input, Tabs, Tag } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { api } from '../lib/api/client';
  import type { ModInstall } from '../lib/api/types';
  import { error_text, toast } from '../lib/app_state.svelte';

  const KIND_ICON: Record<ModInstall['kind'], string> = { plugin: 'plug', extension: 'puzzle', mcp: 'network' };
  const PERMISSION_ICON: Record<string, string> = {
    network: 'globe',
    secret: 'key-round',
    device: 'smartphone',
    path: 'folder',
    process: 'terminal',
    plugin: 'plug',
  };
  const SECTIONS = [
    { kind: 'plugin', label: 'Plugins', what: 'plugin' },
    { kind: 'extension', label: 'Extensions', what: 'extension' },
  ] as const;
  const COMMUNITY_WARNING = 'Community mod: not reviewed by Madlabs, use at your own risk.';
  const icon_of = (permission: string) => PERMISSION_ICON[permission.split(':')[0]] ?? 'shield';
  const command_of = (m: ModInstall) => `sun ${m.kind === 'extension' ? 'extension' : 'plugin'} add ${m.name}`;
  const secrets_in = (permissions: string[]) => permissions.filter((p) => p.startsWith('secret:')).map((p) => p.slice(7));
  const and_list = (names: string[]) => (names.length > 1 ? `${names.slice(0, -1).join(', ')} and ${names.at(-1)}` : names[0]);

  /** A consent dialog: a new install, or an update to the version the kernel lists. */
  type Consent = { mod: ModInstall; update: boolean };
  /** What the user approves: the update's permissions for an update. */
  const asks = (c: Consent) => (c.update && c.mod.update ? c.mod.update.permissions : c.mod.permissions);
  /** Permissions an update adds to what is granted now. */
  const added = (c: Consent) => (c.update ? asks(c).filter((p) => !c.mod.permissions.includes(p)) : []);

  let load = $state(api.list_mods());
  let tab = $state('installed');
  let picked = $state<Consent | null>(null);
  let removing = $state<ModInstall | null>(null);
  let purge = $state(false);
  let values = $state<Record<string, string>>({});
  let busy = $state(false);

  function pick(mod: ModInstall, update = false) {
    picked = { mod, update };
    values = {};
  }

  function ask_remove(m: ModInstall) {
    removing = m;
    purge = false;
  }

  async function install() {
    if (!picked) return;
    const consent = picked;
    const { mod: m, update } = consent;
    const permissions = asks(consent);
    busy = true;
    try {
      if (update) await api.update_mod(m.name, permissions);
      else await api.install_mod(m.name, permissions);
    } catch (e) {
      busy = false;
      toast('danger', `Couldn’t ${update ? 'update' : 'install'} ${m.name}`, error_text(e));
      return;
    }
    // Secrets go to the mod's own scope and are never shown again; an update asks only for new ones.
    const failed: string[] = [];
    for (const name of secrets_in(update ? added(consent) : permissions)) {
      if (!values[name]) continue;
      try {
        await api.set_secret({ kind: 'mod', id: m.name }, name, values[name]);
      } catch {
        failed.push(name);
      }
    }
    busy = false;
    picked = null;
    values = {};
    tab = 'installed';
    load = api.list_mods();
    const done = update ? `${m.name} updated` : `${m.name} installed`;
    if (failed.length) toast('warning', done, `Couldn’t save ${failed.join(', ')}. Add ${failed.length > 1 ? 'them' : 'it'} in Settings.`);
    else if (update) toast('success', done, `Now on v${m.update?.version}, with the permissions you approved.`);
    else toast('success', done, 'It is running now with the permissions you approved.');
  }

  async function remove() {
    if (!removing) return;
    const m = removing;
    busy = true;
    try {
      await api.remove_mod(m.name, purge);
    } catch (e) {
      busy = false;
      toast('danger', `Couldn’t remove ${m.name}`, error_text(e));
      return;
    }
    busy = false;
    removing = null;
    load = api.list_mods();
    toast('success', `${m.name} removed`, purge ? 'Its data and secrets are deleted too.' : 'Its data and secrets are kept for a reinstall.');
  }
</script>

<PageHeader title="Mods" subtitle="Plugins and extensions, and exactly what each one may reach." />

{#snippet row(m: ModInstall)}
  <article class="sun-row" aria-label={m.name}>
    <span class="sun-tile" class:sun-tile--ice={m.installed}><Icon name={KIND_ICON[m.kind]} size={18} /></span>
    <div class="sun-row__body">
      <div class="sun-row__title">
        <span class="ml-mono">{m.name}</span>
        <span class="ml-mono mods__version">v{m.version}</span>
        {#if m.installed}
          {#if !m.signed}<Badge tone="warning">Unsigned</Badge>{/if}
        {:else if m.tier === 'community'}
          <Badge tone="neutral">Community</Badge>
        {:else if m.tier === 'official'}
          <Badge tone="info">Official</Badge>
        {/if}
        {#if m.update}<Badge tone="info">v{m.update.version} available</Badge>{/if}
      </div>
      {#if m.description}<p class="mods__description">{m.description}</p>{/if}
      <div class="sun-row__meta">
        {m.installed ? 'Granted' : 'Asks for'}{m.permissions.length ? ':' : ' nothing'}
      </div>
      {#if m.permissions.length}
        <ul class="sun-chips mods__permissions" aria-label="{m.installed ? 'Granted to' : 'Asked for by'} {m.name}">
          {#each m.permissions as p (p)}<li><Tag size="sm" icon={icon_of(p)}>{p}</Tag></li>{/each}
        </ul>
      {/if}
      {#if m.needed_by?.length}
        <div class="sun-row__meta">Needed by {and_list(m.needed_by)}</div>
      {/if}
      {#if !m.installed && m.tier === 'community'}
        <div class="mods__warning mods__row-warning"><Icon name="triangle-alert" size={16} />{COMMUNITY_WARNING}</div>
      {/if}
      {#if !m.installed}
        <div class="sun-row__meta">Or from a terminal: <code class="ml-mono">{command_of(m)}</code></div>
      {/if}
      {#if m.repo}
        <a class="mods__source" href={m.repo} target="_blank" rel="noopener noreferrer">
          <Icon name="external-link" size={14} />Source on GitHub
        </a>
      {/if}
    </div>
    {#if m.installed}
      <div class="sun-row__aside mods__actions">
        {#if m.update}
          <Button size="sm" variant="ice" iconLeft="refresh-cw" onclick={() => pick(m, true)}>Update to v{m.update.version}</Button>
        {/if}
        <Button size="sm" variant="secondary" iconLeft="trash-2" onclick={() => ask_remove(m)}>Remove</Button>
      </div>
    {:else}
      <div class="sun-row__aside">
        <Button size="sm" variant="ice" iconLeft="download" onclick={() => pick(m)}>Install</Button>
      </div>
    {/if}
  </article>
{/snippet}

<div class="sun-page">
  {#await load}
    <div class="sun-empty" role="status">Loading…</div>
  {:then mods}
    {@const installed = mods.filter((m) => m.installed)}
    {@const available = mods.filter((m) => !m.installed)}
    {@const shown = tab === 'installed' ? installed : available}
    <Tabs
      bind:value={tab}
      items={[
        { id: 'installed', label: 'Installed', badge: installed.length },
        { id: 'available', label: 'Available', badge: available.length },
      ]}
    />
    {#each SECTIONS as s (s.kind)}
      {@const rows = shown.filter((m) => m.kind === s.kind)}
      <section class="mods__section" aria-labelledby="mods-{s.kind}">
        <h2 class="sun-label mods__heading" id="mods-{s.kind}">{s.label} <Badge tone="neutral" count>{rows.length}</Badge></h2>
        {#each rows as m (m.name)}
          {@render row(m)}
        {:else}
          <div class="sun-empty">
            {tab === 'installed' ? `No ${s.what}s installed. Pick one under Available.` : `Every available ${s.what} is installed.`}
          </div>
        {/each}
      </section>
    {/each}
  {:catch e}
    <div class="sun-empty" role="alert">{error_text(e)}</div>
  {/await}
</div>

{#if picked}
  {@const consent = picked}
  {@const m = consent.mod}
  {@const permissions = asks(consent)}
  {@const fresh = added(consent)}
  {@const secrets = secrets_in(consent.update ? fresh : permissions)}
  {@const verb = consent.update ? 'Update' : 'Install'}
  <Dialog open onclose={() => (picked = null)} title="{verb} {m.name}">
    <div class="mods__dialog">
      <p class="mods__lead">
        {#if consent.update}
          {m.name} v{m.version} → v{m.update?.version}. Its data and secrets stay.
        {:else}
          <span class="ml-mono">{m.name} v{m.version}</span> from its GitHub release.
        {/if}
        It runs sandboxed and may reach only what is listed here.
      </p>
      {#if m.tier === 'community' && !consent.update}
        <div class="mods__warning" role="note"><Icon name="triangle-alert" size={16} />{COMMUNITY_WARNING}</div>
      {/if}
      <ul class="sun-chips mods__permissions" aria-label="Permissions {m.name} asks for">
        {#each permissions as p (p)}
          <li class="mods__permission">
            <Tag size="sm" icon={icon_of(p)}>{p}</Tag>{#if fresh.includes(p)}<Badge tone="warning">New</Badge>{/if}
          </li>
        {:else}<li class="sun-row__meta">Nothing</li>{/each}
      </ul>
      {#if secrets.length}
        <fieldset class="mods__secrets">
          <legend class="mods__legend">{consent.update ? 'New secrets' : 'Secrets'}</legend>
          {#each secrets as name (name)}
            <Input
              label={name}
              type="password"
              autocomplete="off"
              hint="Write-only, for {m.name} only. Leave empty to add it later in Settings."
              bind:value={values[name]}
            />
          {/each}
        </fieldset>
      {/if}
    </div>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (picked = null)}>Cancel</Button>
      <Button variant="ice" loading={busy} onclick={install}>{verb} with these permissions</Button>
    {/snippet}
  </Dialog>
{/if}

{#if removing}
  {@const m = removing}
  {@const needed_by = m.needed_by ?? []}
  <Dialog open onclose={() => (removing = null)} title="Remove {m.name}?">
    <div class="mods__dialog">
      <p class="mods__lead">{m.name} stops now. Runs that don’t use it carry on.</p>
      {#if needed_by.length}
        <div class="mods__warning" role="alert">
          <Icon name="triangle-alert" size={16} />
          <span>
            {and_list(needed_by)}
            {needed_by.length > 1 ? 'need' : 'needs'} it: {needed_by.length > 1 ? 'their' : 'its'} calls to {m.name} fail until you reinstall it.
          </span>
        </div>
      {/if}
      <Checkbox label="Also delete its data and secrets" bind:checked={purge} />
      <p class="sun-row__meta mods__purge-hint">
        {purge ? 'This can’t be undone.' : 'Kept, so a reinstall picks up where it left off.'}
      </p>
    </div>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (removing = null)}>Keep it</Button>
      <Button variant="danger" loading={busy} onclick={remove}>Remove {m.name}</Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .mods__section {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    margin-top: var(--space-4);
  }
  .mods__heading {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0;
  }
  .mods__version {
    font-size: 11px;
    color: var(--fg-3);
  }
  .mods__permissions {
    list-style: none;
    margin: 8px 0 0;
    padding: 0;
  }
  .mods__permission {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .mods__actions {
    flex-direction: row;
    flex-wrap: wrap;
    justify-content: flex-end;
  }
  code {
    font-size: 12px;
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
  .mods__dialog {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .mods__lead {
    font-size: 14px;
    color: var(--fg-1);
    margin: 0;
  }
  .mods__warning {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 12px;
    border-radius: var(--radius-md);
    background: var(--amber-100);
    color: var(--amber-700);
    font-size: 13px;
    font-weight: 500;
  }
  .mods__description {
    margin: 4px 0 0;
    font-size: 13px;
    color: var(--fg-2);
  }
  .mods__source {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    margin-top: 8px;
    font-size: 13px;
    min-height: 24px;
  }
  .mods__row-warning {
    margin-top: 8px;
  }
  .mods__purge-hint {
    margin: 0;
  }
  .mods__secrets {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    border: 0;
    margin: 0;
    padding: 0;
    min-width: 0;
  }
  .mods__legend {
    font-size: 13px;
    font-weight: 600;
    margin-bottom: 6px;
  }
</style>
