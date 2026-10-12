<!-- Members → Groups: groups are roles admins can rename and customize. Create, rename and delete
     them, tick what each may do in the resource × verb matrix (each tick saves at once), and pick who
     is in each. Approvals match a group by its key (`role:<key>`), which never changes, so renaming never
     breaks a workflow. -->
<script lang="ts">
  import { Button, Checkbox, Dialog, Icon, Input, Tag } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import GroupPermissions, { PERMISSION } from './GroupPermissions.svelte';
  import { auth, type Group, type Member, type Permission } from '../lib/api/auth';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { toggle } from '../lib/format';

  let { groups, members, changed }: { groups: Group[]; members: Member[]; changed: () => void } = $props();

  const members_of = (g: Group) => members.filter((m) => m.groups.includes(g.id));

  async function grant(g: Group, permissions: Permission[], permission: Permission, on: boolean) {
    try {
      await auth.update_group(g.id, { permissions });
      toast('success', `${g.name} ${on ? 'can' : 'can’t'} ${PERMISSION[permission].does} now`);
    } catch (e) {
      toast('danger', 'Permission not changed', error_text(e));
    }
    changed();
  }

  // New group / rename: one name dialog.
  let naming = $state<Group | 'new' | null>(null);
  let name = $state('');
  let picked = $state<Permission[]>([]);
  let name_error = $state('');
  let busy = $state(false);

  function open_name(target: Group | 'new') {
    naming = target;
    name = target === 'new' ? '' : target.name;
    picked = [];
    name_error = '';
  }

  async function save_name(e: SubmitEvent) {
    e.preventDefault();
    const clean = name.trim();
    if (!clean) {
      name_error = 'Give the group a name.';
      return;
    }
    busy = true;
    try {
      if (naming === 'new') {
        const g = await auth.create_group(clean, picked);
        toast('success', `${g.name} created`, `Approvals can name it as role:${g.key}.`);
      } else if (naming) {
        await auth.update_group(naming.id, { name: clean });
        toast('success', `Renamed to ${clean}`, `It is still role:${naming.key} in approvals.`);
      }
      naming = null;
      changed();
    } catch (err) {
      name_error = error_text(err);
    }
    busy = false;
  }

  // Who is in a group.
  let staffing = $state<Group | null>(null);
  let chosen = $state<string[]>([]);

  function open_members(g: Group) {
    staffing = g;
    chosen = members_of(g).map((m) => m.id);
  }

  async function save_members() {
    if (!staffing) return;
    const g = staffing;
    busy = true;
    try {
      await auth.set_group_members(g.id, chosen);
      toast('success', `${g.name} members saved`);
      staffing = null;
      changed();
    } catch (err) {
      toast('danger', 'Members not saved', error_text(err));
    }
    busy = false;
  }

  let deleting = $state<Group | null>(null);

  async function remove() {
    if (!deleting) return;
    const g = deleting;
    busy = true;
    try {
      await auth.delete_group(g.id);
      toast('success', `${g.name} deleted`, 'Its members lost what it allowed.');
      deleting = null;
      changed();
    } catch (err) {
      toast('danger', 'Not deleted', error_text(err));
    }
    busy = false;
  }

</script>

<section aria-labelledby="groups-heading" class="sun-section">
  <div class="section-head">
    <h3 id="groups-heading">Groups</h3>
    <Button variant="ice" size="sm" iconLeft="plus" onclick={() => open_name('new')}>New group</Button>
  </div>
  <p class="sun-row__meta" style="margin:0">
    A group is a role you can rename and customize. People get everything their groups allow; owners can do everything.
    Approvals name a group as <span class="ml-mono">role:&lt;key&gt;</span>, and the key stays the same when you rename it.
  </p>
  {#each groups as g (g.id)}
    {@const people = members_of(g)}
    <article class="sun-row group" aria-label={g.name}>
      <div class="group__head">
        <span class="sun-tile"><Icon name="users" size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title">{g.name}</div>
          <div class="sun-row__meta"><span class="ml-mono">role:{g.key}</span> · {people.length} {people.length === 1 ? 'person' : 'people'}</div>
        </div>
        <IconButton icon="pencil" label="Rename {g.name}" size="sm" onclick={() => open_name(g)} />
        <IconButton icon="trash-2" label="Delete {g.name}" size="sm" onclick={() => (deleting = g)} />
      </div>
      <GroupPermissions caption="What {g.name} may do" granted={g.permissions} onchange={(all, p, on) => grant(g, all, p, on)} />
      <div class="group__people">
        {#each people as m (m.id)}<Tag size="sm" icon="user">{m.username}</Tag>{:else}<span class="sun-row__meta">No one yet.</span>{/each}
        <Button variant="secondary" size="sm" iconLeft="user-plus" aria-label="Edit members of {g.name}" onclick={() => open_members(g)}>Members</Button>
      </div>
    </article>
  {:else}
    <div class="sun-empty">No groups. Create one to give people access.</div>
  {/each}
</section>

<Dialog open={naming !== null} onclose={() => (naming = null)} title={naming === 'new' ? 'New group' : `Rename ${naming?.name ?? ''}`}>
  <form id="group-name" class="dialog-form" onsubmit={save_name} novalidate>
    <Input
      label="Name"
      placeholder="e.g. Builders"
      maxlength={64}
      bind:value={name}
      error={name_error || undefined}
      hint={naming === 'new' ? 'Its approval key is made from this name and never changes.' : 'The approval key stays the same.'}
    />
    {#if naming === 'new'}
      <GroupPermissions caption="What it may do" stacked granted={picked} onchange={(all) => (picked = all)} />
    {/if}
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (naming = null)}>Cancel</Button>
    <Button variant="ice" type="submit" form="group-name" loading={busy}>{naming === 'new' ? 'Create group' : 'Rename'}</Button>
  {/snippet}
</Dialog>

<Dialog open={!!staffing} onclose={() => (staffing = null)} title="Members of {staffing?.name ?? ''}">
  <fieldset class="perms">
    <legend class="sun-row__meta">Who is in it</legend>
    {#each members as m (m.id)}
      <Checkbox label={m.username} description={m.email ?? undefined} checked={chosen.includes(m.id)} onchange={(e: Event) => (chosen = toggle(chosen, m.id, (e.currentTarget as HTMLInputElement).checked))} />
    {/each}
  </fieldset>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (staffing = null)}>Cancel</Button>
    <Button variant="ice" loading={busy} onclick={save_members}>Save members</Button>
  {/snippet}
</Dialog>

<Dialog open={!!deleting} onclose={() => (deleting = null)} title="Delete {deleting?.name ?? ''}?">
  <p class="sun-row__meta" style="margin:0">
    Its members lose what it allows, and approvals that name <span class="ml-mono">role:{deleting?.key}</span> admit no one through it.
  </p>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (deleting = null)}>Cancel</Button>
    <Button variant="danger" iconLeft="trash-2" loading={busy} onclick={remove}>Delete group</Button>
  {/snippet}
</Dialog>

<style>
  .sun-section {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .section-head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .group {
    flex-direction: column;
    flex-wrap: nowrap;
    align-items: stretch;
  }
  .group__head {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .group__people {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  .perms {
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
    gap: var(--space-2) var(--space-4);
  }
  .perms legend {
    margin-bottom: var(--space-2);
  }
  .dialog-form {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  h3 {
    margin: var(--space-2) 0 0;
    font-family: var(--font-heading);
    font-weight: 400;
    font-size: 20px;
    color: var(--fg-1);
  }
</style>
