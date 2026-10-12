<!-- Account and workspace menu (the shared Menu with a custom trigger): who is signed in, the workspaces they
     belong to (the current one checked), their pending invitations (a count on the trigger; the dialog joins or
     declines), "New workspace" (anyone, unless the deployment lets only its owners) and Log out. A workspace's
     own settings and delete live in Settings → <workspace>. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon } from '@hryer/madlabs-design-system/svelte';
  import { app, workspace_of, can_create_workspace, current_workspace, sign_out } from '../app_state.svelte';
  import { show_workspace } from '../router.svelte';
  import Invitations from './Invitations.svelte';
  import Menu, { type MenuItem } from './Menu.svelte';
  import WorkspaceForm from './WorkspaceForm.svelte';
  import WorkspaceLogo from './WorkspaceLogo.svelte';

  const workspace = $derived(workspace_of(current_workspace()));
  const initial = (name: string | undefined) => (name?.trim()[0] ?? '?').toUpperCase();
  const invites = $derived(app.invitations.length);

  const items = $derived<(MenuItem | null)[]>([
    ...app.workspaces.map((w) => ({ id: w.id, lead: logo, label: w.name, checked: w.id === workspace?.id, onselect: () => show_workspace(w.slug) })),
    ...(invites ? [{ label: 'Invitations', icon: 'mail', badge: String(invites), onselect: () => (answering = true) }] : []),
    ...(can_create_workspace() ? [{ label: 'New workspace', icon: 'plus', onselect: () => (creating = true) }] : []),
    null,
    { label: 'Log out', icon: 'log-out', danger: true, onselect: sign_out },
  ]);

  let creating = $state(false);
  let answering = $state(false);
  let busy = $state(false);
</script>

{#snippet logo(item: MenuItem)}<WorkspaceLogo workspace={workspace_of(item.id ?? '')} size={20} />{/snippet}

<div class="switcher">
  <Menu label="Account and workspaces" {items}>
    {#snippet trigger(attrs)}
      <button type="button" class="switcher__trigger" aria-label="Workspace: {workspace?.name ?? 'none'}" {...attrs}>
        <WorkspaceLogo {workspace} size={28} />
        <span class="switcher__name">{workspace?.name ?? 'Workspace'}</span>
        {#if invites}<Badge tone="warning" count aria-label="{invites} pending {invites === 1 ? 'invitation' : 'invitations'}">{invites}</Badge>{/if}
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
  {#if creating}<WorkspaceForm id="workspace-form" bind:busy oncreated={() => (creating = false)} />{/if}
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (creating = false)}>Cancel</Button>
    <Button variant="ice" type="submit" form="workspace-form" loading={busy}>Create workspace</Button>
  {/snippet}
</Dialog>

<Dialog open={answering} onclose={() => (answering = false)} title="Invitations">
  <Invitations ondone={() => (answering = !!app.invitations.length)} />
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (answering = false)}>Close</Button>
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
</style>
