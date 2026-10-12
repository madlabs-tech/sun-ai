<!-- Account and workspace menu (the shared Menu with a custom trigger): who is signed in, the workspaces
     their groups see (the current one checked), and Log out. Whoever holds `create_workspace` creates
     workspaces here: name, URL slug (from the name until edited), the task-key prefix it implies, and who
     sees it. A workspace's own settings and delete live in Settings → <workspace>. -->
<script lang="ts">
  import { Button, Dialog, Icon, Input } from '@hryer/madlabs-design-system/svelte';
  import type { Group } from '../api/auth';
  import { api } from '../api/client';
  import { app, workspace_of, can, current_workspace, error_text, load_workspace, sign_out } from '../app_state.svelte';
  import { SLUG, prefix_from_slug, slug_from_name } from '../format';
  import { show_workspace } from '../router.svelte';
  import Menu, { type MenuItem } from './Menu.svelte';
  import WorkspaceAccess, { access_choices } from './WorkspaceAccess.svelte';
  import WorkspaceLogo from './WorkspaceLogo.svelte';

  const workspace = $derived(workspace_of(current_workspace()));
  const initial = (name: string | undefined) => (name?.trim()[0] ?? '?').toUpperCase();

  const items = $derived<(MenuItem | null)[]>([
    ...app.workspaces.map((w) => ({ id: w.id, lead: logo, label: w.name, checked: w.id === workspace?.id, onselect: () => show_workspace(w.slug) })),
    ...(can('create_workspace') ? [{ label: 'Create workspace', icon: 'plus', onselect: open_create }] : []),
    null,
    { label: 'Log out', icon: 'log-out', danger: true, onselect: sign_out },
  ]);

  let creating = $state(false);
  let name = $state('');
  /** The slug as typed; `null` until then, while it follows the name. */
  let slug_typed = $state<string | null>(null);
  const slug = $derived(slug_typed ?? (name.trim() ? slug_from_name(name) : ''));
  let errors = $state<{ name?: string; slug?: string }>({});
  /** The groups this person may give or take access for, and the ticked ones. */
  let choices = $state<Group[]>([]);
  let picked = $state<string[]>([]);
  let busy = $state(false);

  async function open_create() {
    creating = true;
    [name, slug_typed, errors] = ['', null, {}];
    choices = await access_choices();
    picked = app.groups.map((g) => g.id).filter((g) => choices.some((c) => c.id === g));
  }

  async function create(e?: Event) {
    e?.preventDefault();
    errors = {
      name: name.trim() ? undefined : 'Give the workspace a name.',
      slug: SLUG.test(slug) ? undefined : 'Use lowercase letters and digits, in words joined by single hyphens.',
    };
    if (errors.name || errors.slug) return;
    busy = true;
    try {
      const created = await api.create_workspace(name.trim(), [...picked], slug);
      creating = false;
      await load_workspace();
      show_workspace(created.slug);
    } catch (err) {
      const text = error_text(err);
      errors = /slug/i.test(text) ? { slug: text } : { name: text };
    }
    busy = false;
  }
</script>

{#snippet logo(item: MenuItem)}<WorkspaceLogo workspace={workspace_of(item.id ?? '')} size={20} />{/snippet}

<div class="switcher">
  <Menu label="Account and workspaces" {items}>
    {#snippet trigger(attrs)}
      <button type="button" class="switcher__trigger" aria-label="Workspace: {workspace?.name ?? 'none'}" {...attrs}>
        <WorkspaceLogo {workspace} size={28} />
        <span class="switcher__name">{workspace?.name ?? 'Workspace'}</span>
        <Icon name="chevrons-up-down" size={16} />
      </button>
    {/snippet}
    {#snippet header()}
      {#if app.me}
        <div class="switcher__account">
          <span class="switcher__avatar" aria-hidden="true">{initial(app.me.user)}</span>
          <div class="switcher__who">
            <div class="switcher__user">{app.me.user}</div>
            {#if app.me.email}<div class="switcher__email">{app.me.email}</div>{/if}
          </div>
        </div>
      {/if}
      <div class="switcher__label" aria-hidden="true">Workspaces</div>
    {/snippet}
  </Menu>
</div>

<Dialog open={creating} onclose={() => (creating = false)} title="New workspace">
  <form id="workspace-form" class="workspace-form" onsubmit={create} novalidate>
    <Input label="Name" placeholder="e.g. Marketing" maxlength={64} bind:value={name} error={errors.name} hint="Each workspace has its own workflows, tasks, repos and knowledge." />
    <Input
      label="Workspace URL"
      mono
      maxlength={48}
      autocomplete="off"
      spellcheck={false}
      value={slug}
      oninput={(e: Event) => (slug_typed = (e.currentTarget as HTMLInputElement).value.trim().toLowerCase())}
      error={errors.slug}
      hint="Links use ?workspace={slug || '…'}. It can't be changed later."
    />
    <p class="workspace-form__prefix" aria-live="polite">
      Task keys: <span class="ml-mono">{prefix_from_slug(slug) || '…'}-1</span>, <span class="ml-mono">{prefix_from_slug(slug) || '…'}-2</span>, … The prefix can be changed in Settings.
    </p>
    <WorkspaceAccess {choices} bind:picked />
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (creating = false)}>Cancel</Button>
    <Button variant="ice" type="submit" form="workspace-form" loading={busy}>Create workspace</Button>
  {/snippet}
</Dialog>

<style>
  .switcher__trigger {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    min-height: var(--control-md);
    padding: 0 var(--space-2);
    border: 1.5px solid var(--border-subtle);
    border-radius: var(--radius-md);
    background: transparent;
    color: var(--fg-1);
    font: inherit;
    font-size: 14px;
    font-weight: 600;
    text-align: left;
    cursor: pointer;
    transition: background var(--dur-fast) var(--ease-out);
  }
  .switcher__trigger:hover,
  .switcher__trigger[aria-expanded='true'] {
    background: var(--accent-soft);
  }
  .switcher__trigger:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
  }
  .switcher__trigger > :global(.ml-icon) {
    color: var(--fg-2);
  }
  .switcher__name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .switcher__account {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: var(--space-2);
    margin-bottom: var(--space-1);
    border-bottom: 1.5px solid var(--border-subtle);
  }
  .switcher__avatar {
    flex: none;
    display: grid;
    place-items: center;
    width: 32px;
    height: 32px;
    border-radius: var(--radius-pill);
    background: var(--surface-raised);
    font-weight: 700;
  }
  .switcher__who {
    min-width: 0;
  }
  .switcher__user,
  .switcher__email {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .switcher__user {
    font-size: 14px;
    font-weight: 600;
  }
  .switcher__email {
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .switcher__label {
    padding: var(--space-1) var(--space-2);
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .workspace-form {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .workspace-form__prefix {
    margin: calc(-1 * var(--space-2)) 0 0;
    color: var(--fg-2);
    font-size: var(--size-sm);
    line-height: 1.5;
  }
</style>
