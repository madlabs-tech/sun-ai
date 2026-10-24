<!-- One group (#/settings/groups/<id>, deployment owners): who's in it — people added by hand, and anyone whose
     verified email is on one of its domains, previewed live as the domains change — and what uses it: editor
     grants, host shares and the terminal policy. Domain members can't be removed one by one; removing the domain
     does. Deleting the group removes what uses it, so the dialog lists that first. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input } from '@hryer/madlabs-design-system/svelte';
  import Chips from '../lib/ui/Chips.svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import { api } from '../lib/api/client';
  import { auth, type Member } from '../lib/api/auth';
  import type { EditorGrant, Host, MemberGroup } from '../lib/api/types';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { go, href } from '../lib/router.svelte';

  let { id }: { id: string } = $props();

  let group = $state<MemberGroup | null>(null);
  let load_error = $state('');
  let people = $state<Member[]>([]);
  /** Who its saved domains bring in now. */
  let by_domain = $state<Member[]>([]);
  /** The domains being edited. */
  let domains = $state<string[]>([]);

  async function refresh() {
    const found = (await auth.list_groups()).find((g) => g.id === id);
    if (!found) throw new Error('This group doesn’t exist any more.');
    by_domain = found.domains.length ? await auth.group_preview(id, found.domains) : [];
    group = found;
  }
  Promise.all([refresh(), auth.list_members().then((list) => (people = list))]).then(
    () => (domains = [...group!.domains]),
    (e) => (load_error = error_text(e)),
  );

  let preview = $state<Member[]>([]);
  let preview_error = $state('');
  $effect(() => {
    const draft = [...domains];
    if (!group) return;
    let stale = false;
    auth.group_preview(id, draft).then(
      (list) => !stale && ([preview, preview_error] = [list, '']),
      (e) => !stale && (preview_error = error_text(e)),
    );
    return () => (stale = true);
  });
  const domains_changed = $derived(!!group && domains.join() !== group.domains.join());

  let busy = $state('');
  /** True once `work` is saved and the group read again. */
  async function act(key: string, work: () => Promise<unknown>, done: string) {
    busy = key;
    try {
      await work();
      await refresh();
      toast('success', done);
      return true;
    } catch (e) {
      toast('danger', 'Not saved', error_text(e));
      return false;
    } finally {
      busy = '';
    }
  }
  async function save_domains() {
    if (await act('domains', () => auth.update_group(id, { domains }), 'Domains saved')) domains = [...group!.domains];
  }

  let query = $state('');
  const matches = $derived.by(() => {
    const q = query.trim().toLowerCase();
    if (!q || !group) return [];
    return people
      .filter((p) => !p.deactivated_at && !group!.member_ids.includes(p.id) && `${p.username} ${p.email ?? ''}`.toLowerCase().includes(q))
      .slice(0, 5);
  });

  /** Everyone in it now, and how. */
  const rows = $derived.by(() => {
    if (!group) return [];
    const ids = [...new Set([...group.member_ids, ...by_domain.map((m) => m.id)])];
    return ids.map((member_id) => ({
      id: member_id,
      person: people.find((p) => p.id === member_id) ?? by_domain.find((p) => p.id === member_id),
      manual: group!.member_ids.includes(member_id),
      domain: by_domain.some((p) => p.id === member_id),
    }));
  });
  const domain_of = (p?: Member) => p?.email?.split('@').pop() ?? '';

  interface Uses {
    grants: { workspace: string; grant: EditorGrant }[];
    hosts: Host[];
    terminal: boolean;
  }
  /** What names the group: deleting it removes these. */
  async function find_uses(): Promise<Uses> {
    const [workspaces, hosts, terminal] = await Promise.all([api.list_workspaces(), api.list_hosts(), auth.get_terminal()]);
    const [grants, shared] = await Promise.all([
      Promise.all(
        workspaces.map(async (w) =>
          (await api.editor_grants(w.id)).filter((g) => g.subject_type === 'group' && g.subject_id === id).map((grant) => ({ workspace: w.name, grant })),
        ),
      ),
      Promise.all(hosts.map(async (h) => ((await api.host_shares(h.id)).shares.some((s) => s.group_id === id) ? [h] : []))),
    ]);
    return { grants: grants.flat(), hosts: shared.flat(), terminal: terminal.groups.includes(id) };
  }
  const uses = find_uses();
  const unused = (u: Uses) => !u.grants.length && !u.hosts.length && !u.terminal;

  let deleting = $state(false);
  async function remove() {
    busy = 'delete';
    try {
      await auth.delete_group(id);
      toast('success', `${group?.name} deleted`);
      go('settings', 'groups');
    } catch (e) {
      toast('danger', 'Not deleted', error_text(e));
    }
    busy = '';
  }
</script>

{#snippet uses_list(u: Uses)}
  <ul class="group__uses">
    {#each u.grants as { workspace, grant } (grant.id)}
      <li><Icon name="file-code" size={14} />Editor access in {workspace}: <span class="ml-mono">{grant.capability}</span></li>
    {/each}
    {#each u.hosts as h (h.id)}
      <li><Icon name="server" size={14} />Shared host <span class="ml-mono">{h.name}</span></li>
    {/each}
    {#if u.terminal}<li><Icon name="terminal" size={14} />Terminal access</li>{/if}
  </ul>
{/snippet}

<a class="group__back" href={href('settings', 'groups')}><Icon name="chevron-left" size={16} />All groups</a>

{#if load_error}
  <div class="sun-empty" role="alert">{load_error}</div>
{:else if !group}
  <div class="sun-empty" role="status">Loading…</div>
{:else}
  <header class="group__head">
    <div>
      <h3>{group.name}</h3>
      {#if group.description}<p class="group__note">{group.description}</p>{/if}
    </div>
    <Button size="sm" variant="secondary" iconLeft="trash-2" onclick={() => (deleting = true)}>Delete group</Button>
  </header>

  <section class="group__section" aria-labelledby="group-who">
    <h3 id="group-who">Who’s in it</h3>
    <Chips label="Anyone with a verified email at…" bind:items={domains} placeholder="madlabs.tech" hint="Only verified emails count." />
    {#if preview_error}
      <p class="group__error" role="alert">{preview_error}</p>
    {:else}
      <p class="group__note" role="status" aria-live="polite" data-testid="group-preview">
        {#if !domains.length}No domains: only the people added by hand.{:else}{preview.length}
          {preview.length === 1 ? 'person matches' : 'people match'} now{preview.length ? `: ${preview.map((p) => p.username).join(', ')}` : '.'}{/if}
      </p>
    {/if}
    {#if domains_changed}
      <div><Button size="sm" variant="ice" loading={busy === 'domains'} disabled={!!preview_error} onclick={save_domains}>Save domains</Button></div>
    {/if}
    <Input label="Add people by hand" iconLeft="search" bind:value={query} placeholder="Name or email" />
    {#each matches as p (p.id)}
      <div class="group__match">
        <span>{p.username} <span class="group__note">{p.email ?? ''}</span></span>
        <Button size="sm" variant="secondary" iconLeft="plus" loading={busy === p.id} aria-label="Add {p.username}"
          onclick={() => act(p.id, () => auth.add_group_member(id, p.id), `${p.username} added`)}>Add</Button>
      </div>
    {/each}
  </section>

  <section class="group__section" aria-labelledby="group-members">
    <h3 id="group-members">Members</h3>
    {#each rows as r (r.id)}
      <article class="sun-row" aria-label={r.person?.username ?? 'A removed member'} style="align-items:center">
        <span class="sun-tile"><Icon name="user" size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title">
            {r.person?.username ?? 'A removed member'}
            {#if r.manual}<Badge>Manual</Badge>{/if}
            {#if r.domain}<Badge tone="info">Domain</Badge>{/if}
          </div>
          <div class="sun-row__meta">
            {r.person?.email ?? ''}
            {#if r.domain && !r.manual}· In by their verified @{domain_of(r.person)} email: remove the domain to remove them.{/if}
          </div>
        </div>
        {#if r.manual}
          <IconButton icon="user-x" label="Remove {r.person?.username ?? 'member'}" size="sm"
            onclick={() => act(r.id, () => auth.remove_group_member(id, r.id), `${r.person?.username ?? 'Member'} removed`)} />
        {/if}
      </article>
    {:else}
      <div class="sun-empty">Nobody yet. Add people by hand, or a domain.</div>
    {/each}
  </section>

  <section class="group__section" aria-labelledby="group-uses">
    <h3 id="group-uses">Used by</h3>
    {#await uses}
      <div class="sun-empty" role="status">Loading…</div>
    {:then u}
      {#if unused(u)}<p class="group__note">Nothing uses it yet.</p>{:else}{@render uses_list(u)}{/if}
    {:catch e}
      <div class="sun-empty" role="alert">{error_text(e)}</div>
    {/await}
  </section>

  {#if deleting}
    <Dialog open onclose={() => (deleting = false)} title="Delete {group.name}?">
      {#await uses then u}
        {#if unused(u)}
          <p class="group__note">Nothing uses it.</p>
        {:else}
          <p class="group__note">This also removes:</p>
          {@render uses_list(u)}
        {/if}
      {/await}
      {#snippet footer()}
        <Button variant="secondary" onclick={() => (deleting = false)}>Cancel</Button>
        <Button variant="danger" loading={busy === 'delete'} onclick={remove}>Delete group</Button>
      {/snippet}
    </Dialog>
  {/if}
{/if}

<style>
  .group__back {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    align-self: flex-start;
    min-height: 44px;
    font-size: 14px;
    font-weight: 600;
    color: var(--fg-2);
    text-decoration: none;
  }
  .group__head {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-3);
  }
  h3 {
    margin: 0;
    font-family: var(--font-heading);
    font-weight: 400;
    font-size: 20px;
    color: var(--fg-1);
  }
  .group__section {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    margin-top: var(--space-3);
  }
  .group__note {
    margin: 0;
    font-size: var(--size-sm);
    line-height: 1.5;
    color: var(--fg-2);
  }
  .group__error {
    margin: 0;
    font-size: var(--size-sm);
    color: var(--danger-fg);
  }
  .group__match {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-md);
    background: var(--surface-raised);
  }
  .group__uses {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    font-size: var(--size-sm);
    color: var(--fg-2);
  }
  .group__uses li {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
</style>
