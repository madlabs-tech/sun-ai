<!-- A group's permissions as a resource × verb matrix: one row per resource, columns Create, Read,
     Update, Delete; a cell that doesn't apply shows "—", and a resource with one permission for every verb
     spans the row. Each checkbox is named "<Resource>: <verb>" (e.g. "Workspaces: delete"). Below 640 px, or
     with `stacked` (in a dialog), it is one fieldset per resource. Ticking a member write also ticks
     "Members: read" (a convenience here; the API stays per verb). -->
<script lang="ts" module>
  import { PERMISSIONS, type Permission } from '../lib/api/auth';

  type Verb = 'create' | 'read' | 'update' | 'delete';
  export const VERBS: [Verb, string][] = [
    ['create', 'Create'],
    ['read', 'Read'],
    ['update', 'Update'],
    ['delete', 'Delete'],
  ];

  /** Per permission: its verb in the checkbox name, the label beside it when a cell needs one, and what it lets people do. */
  export const PERMISSION: Record<Permission, { verb: string; label?: string; does: string }> = {
    run: { verb: 'run', does: 'run tasks' },
    approve: { verb: 'approve', does: 'approve' },
    create_workspace: { verb: 'create', does: 'create workspaces' },
    access_all_workspaces: { verb: 'read all', label: 'All', does: 'see every workspace' },
    edit_workspace: { verb: 'update', does: 'edit workspaces' },
    delete_workspace: { verb: 'delete', does: 'delete workspaces' },
    create_workflow: { verb: 'create', does: 'create workflows' },
    edit_workflow: { verb: 'update', does: 'edit workflows' },
    invite_members: { verb: 'invite', label: 'Invite', does: 'invite people by email' },
    create_members: { verb: 'create with password', label: 'With password', does: 'add people with a password' },
    view_members: { verb: 'read', does: 'see members' },
    edit_members: { verb: 'update', does: 'edit members' },
    deactivate_members: { verb: 'deactivate', label: 'Deactivate', does: 'deactivate members' },
    manage_groups: { verb: 'manage', does: 'manage groups' },
    manage_settings: { verb: 'manage', does: 'manage settings' },
    manage_plugins: { verb: 'manage', does: 'manage plugins' },
    manage_extensions: { verb: 'manage', does: 'manage extensions' },
    manage_secrets: { verb: 'manage', does: 'manage secrets' },
    manage_runtimes: { verb: 'manage', does: 'manage runtimes' },
  };

  /** Member writes: ticking one also ticks `view_members`, so the Members page can list who they act on. */
  const MEMBER_WRITES: Permission[] = ['invite_members', 'create_members', 'edit_members', 'deactivate_members'];

  /** A resource: a permission per verb, or one permission for all of it (`all`, with what it covers). */
  interface Row {
    resource: string;
    verbs?: Partial<Record<Verb, Permission[]>>;
    all?: Permission;
    covers?: string;
  }
  const ROWS: Row[] = [
    { resource: 'Tasks', all: 'run', covers: 'Create, steer, pause, retry and cancel tasks' },
    { resource: 'Approvals', all: 'approve', covers: 'Decide approval steps the workflow’s who: allows' },
    { resource: 'Workspaces', verbs: { create: ['create_workspace'], read: ['access_all_workspaces'], update: ['edit_workspace'], delete: ['delete_workspace'] } },
    { resource: 'Workflows', verbs: { create: ['create_workflow'], update: ['edit_workflow'] } },
    {
      resource: 'Members',
      verbs: { create: ['invite_members', 'create_members'], read: ['view_members'], update: ['edit_members'], delete: ['deactivate_members'] },
    },
    { resource: 'Groups', all: 'manage_groups', covers: 'Can grant any permission: create, rename and delete groups, set their permissions and members, share any workspace' },
    { resource: 'Settings', all: 'manage_settings', covers: 'Email, sign-in, audit log, hosts, license, embeddings, billing; import into workspaces' },
    { resource: 'Plugins', all: 'manage_plugins', covers: 'Install plugins and MCP servers' },
    { resource: 'Extensions', all: 'manage_extensions', covers: 'Install extensions' },
    { resource: 'Secrets', all: 'manage_secrets', covers: 'List, set and delete secrets' },
    { resource: 'Runtimes', all: 'manage_runtimes', covers: 'Add runtimes and set their permission modes' },
  ];
</script>

<script lang="ts">
  import { Checkbox } from '@hryer/madlabs-design-system/svelte';
  import { MediaQuery } from 'svelte/reactivity';

  let {
    caption,
    granted,
    onchange,
    stacked = false,
  }: {
    caption: string;
    granted: Permission[];
    /** The whole new set, and the permission the person ticked or cleared. */
    onchange: (permissions: Permission[], permission: Permission, on: boolean) => void;
    stacked?: boolean;
  } = $props();

  function change(permission: Permission, on: boolean) {
    const add = on ? [permission, ...(MEMBER_WRITES.includes(permission) ? (['view_members'] as Permission[]) : [])] : [];
    onchange(PERMISSIONS.filter((p) => add.includes(p) || (p !== permission && granted.includes(p))), permission, on);
  }

  const wide = new MediaQuery('min-width: 640px');
  const capital = (text: string) => text[0].toUpperCase() + text.slice(1);
</script>

{#snippet box(row: Row, p: Permission, label?: string, description?: string)}
  <Checkbox
    {label}
    {description}
    aria-label="{row.resource}: {PERMISSION[p].verb}"
    checked={granted.includes(p)}
    onchange={(e: Event) => change(p, (e.currentTarget as HTMLInputElement).checked)}
  />
{/snippet}

{#if wide.current && !stacked}
  <table class="matrix">
    <caption class="sun-row__meta">{caption}</caption>
    <thead>
      <tr>
        <th scope="col">Resource</th>
        {#each VERBS as [verb, label] (verb)}<th scope="col">{label}</th>{/each}
      </tr>
    </thead>
    <tbody>
      {#each ROWS as row (row.resource)}
        <tr>
          <th scope="row">{row.resource}</th>
          {#if row.all}
            <td colspan="4">{@render box(row, row.all, capital(PERMISSION[row.all].verb), row.covers)}</td>
          {:else}
            {#each VERBS as [verb] (verb)}
              {@const cell = row.verbs?.[verb] ?? []}
              <td>
                {#each cell as p (p)}
                  {@render box(row, p, PERMISSION[p].label)}
                {:else}
                  <span class="matrix__na" role="img" aria-label="Doesn’t apply" title="Doesn’t apply">—</span>
                {/each}
              </td>
            {/each}
          {/if}
        </tr>
      {/each}
    </tbody>
  </table>
{:else}
  <div class="stack" role="group" aria-label={caption}>
    <p class="sun-row__meta stack__caption">{caption}</p>
    {#each ROWS as row (row.resource)}
      <fieldset class="resource">
        <legend>{row.resource}</legend>
        {#if row.all}
          {@render box(row, row.all, capital(PERMISSION[row.all].verb), row.covers)}
        {:else}
          {#each VERBS as [verb] (verb)}
            {#each row.verbs?.[verb] ?? [] as p (p)}{@render box(row, p, capital(PERMISSION[p].verb))}{/each}
          {/each}
        {/if}
      </fieldset>
    {/each}
  </div>
{/if}

<style>
  .matrix {
    width: 100%;
    table-layout: fixed;
    border-collapse: collapse;
    font-size: var(--size-sm);
  }
  .matrix caption {
    text-align: left;
    margin-bottom: var(--space-2);
  }
  .matrix th,
  .matrix td {
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--border-subtle);
    text-align: left;
    vertical-align: middle;
  }
  .matrix thead th {
    color: var(--fg-2);
    font-weight: 500;
  }
  .matrix thead th:first-child {
    width: 120px;
  }
  .matrix tbody th {
    color: var(--fg-1);
    font-weight: 500;
  }
  /* Two permissions in one cell (Members: create) stack. */
  .matrix td :global(.ml-check) {
    display: flex;
  }
  .matrix td :global(.ml-check + .ml-check) {
    margin-top: var(--space-2);
  }
  .matrix__na {
    color: var(--fg-2);
  }
  .stack {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .stack__caption {
    margin: 0;
  }
  .resource {
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2) var(--space-4);
  }
  .resource legend {
    padding: 0;
    margin-bottom: var(--space-2);
    font-weight: 600;
    color: var(--fg-1);
  }
</style>
