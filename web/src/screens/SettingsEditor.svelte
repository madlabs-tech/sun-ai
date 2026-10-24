<!-- Settings → <workspace> → Editor access (owners and admins: `edit_members`): who may read, write and open a
     terminal in the repo editor, on every repository or one, for everyone, a role, a group (deployment owners pick one) or a member. Workspace owners hold all three always: not a grant,
     so it can't be revoked. Write includes read; terminal is its own grant, never implied, because it is a shell
     on the host. One grant per capability; a revoke applies from the holder's next request. -->
<script lang="ts">
  import { Badge, Button, Checkbox, Dialog, Icon, Select } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import { api } from '../lib/api/client';
  import { auth, type WorkspaceMember } from '../lib/api/auth';
  import type { EditorCapability, EditorGrant, GrantSubject, MemberGroup, Repo } from '../lib/api/types';
  import { current_workspace, error_text, is_owner, toast } from '../lib/app_state.svelte';
  import { repo_name } from '../lib/format';
  import { ROLE_LABEL, type Role } from '../lib/role';

  const workspace = current_workspace();
  const CAPABILITIES: { value: EditorCapability; label: string; description: string }[] = [
    { value: 'editor.read', label: 'Read', description: 'Browse, open and quick-open files.' },
    { value: 'editor.write', label: 'Write', description: 'Save, create, rename and delete files. Includes Read.' },
    { value: 'terminal', label: 'Terminal', description: 'Open a shell in the repository’s worktree on the host.' },
  ];
  const label_of = (capability: EditorCapability) => CAPABILITIES.find((c) => c.value === capability)?.label ?? capability;
  const ROLE_OPTIONS = (['admin', 'member', 'viewer'] as const).map((r) => ({ value: r, label: `${ROLE_LABEL[r]}s and above` }));

  let grants = $state(api.editor_grants(workspace));
  let repos = $state<Repo[]>([]);
  let people = $state<WorkspaceMember[]>([]);
  Promise.all([api.list_repos(workspace), auth.workspace_members(workspace)]).then(
    ([r, p]) => ([repos, people] = [r, p]),
    (e) => toast('danger', 'Couldn’t load repositories and members', error_text(e)),
  );
  const reload = () => (grants = api.editor_grants(workspace));
  /** Groups are the deployment's: only its owners list them, so only they grant to one. */
  let groups = $state<MemberGroup[]>([]);
  if (is_owner()) auth.list_groups().then((list) => (groups = list), () => {});

  function subject_of(g: GrantSubject): string {
    if (g.subject_type === 'all') return 'All members';
    if (g.subject_type === 'role') return `${ROLE_LABEL[g.subject_id]}s and above`;
    if (g.subject_type === 'group') return groups.find((x) => x.id === g.subject_id)?.name ?? 'A group';
    return people.find((p) => p.member_id === g.subject_id)?.username ?? 'A member no longer here';
  }
  const scope_of = (repo_id: string | null) => {
    if (!repo_id) return 'Every repository';
    const repo = repos.find((r) => r.id === repo_id);
    return repo ? repo_name(repo.url) : 'A removed repository';
  };

  let subject_type = $state<GrantSubject['subject_type']>('all');
  let role = $state<Role>('member');
  let member = $state('');
  let group = $state('');
  let repo_id = $state('');
  let picked = $state<Record<EditorCapability, boolean>>({ 'editor.read': true, 'editor.write': false, terminal: false });
  let form_error = $state('');
  let saving = $state(false);
  const members = $derived(people.filter((p) => p.role !== 'owner').map((p) => ({ value: p.member_id, label: `${p.username} (${ROLE_LABEL[p.role]})` })));

  async function grant(e: SubmitEvent) {
    e.preventDefault();
    const capabilities = CAPABILITIES.map((c) => c.value).filter((c) => picked[c]);
    form_error = !capabilities.length ? 'Pick at least one of Read, Write or Terminal.' : subject_type === 'member' && !member ? 'Pick a member.' : subject_type === 'group' && !group ? 'Pick a group.' : '';
    if (form_error) return;
    const subject: GrantSubject =
      subject_type === 'all'
        ? { subject_type }
        : subject_type === 'role'
          ? { subject_type, subject_id: role }
          : { subject_type, subject_id: subject_type === 'group' ? group : member };
    saving = true;
    const failed: string[] = [];
    for (const capability of capabilities) {
      try {
        await api.grant_editor(workspace, { ...subject, repo_id: repo_id || null, capability });
      } catch (err) {
        failed.push(`${label_of(capability)}: ${error_text(err)}`);
      }
    }
    saving = false;
    form_error = failed.join(' ');
    if (failed.length < capabilities.length) toast('success', `Editor access granted to ${subject_of(subject).toLowerCase()}`);
    reload();
  }

  let revoking = $state<EditorGrant | null>(null);
  let busy = $state(false);
  async function revoke(g: EditorGrant) {
    busy = true;
    try {
      await api.revoke_editor_grant(workspace, g.id);
      toast('success', `${label_of(g.capability)} revoked`, 'It applies from their next request.');
    } catch (e) {
      toast('danger', 'Not revoked', error_text(e));
    }
    revoking = null;
    busy = false;
    reload();
  }
</script>

<section aria-labelledby="editor-access-heading" class="sun-section">
  <h3 id="editor-access-heading">Who has access</h3>
  <article class="sun-row" aria-label="Workspace owners">
    <span class="sun-tile"><Icon name="shield-check" size={18} /></span>
    <div class="sun-row__body">
      <div class="sun-row__title">Workspace owners</div>
      <div class="sun-row__meta">Read, Write and Terminal on every repository. Always held, so it can’t be revoked.</div>
    </div>
    <Badge>Implicit</Badge>
  </article>
  <Loaded load={grants} empty="Nobody else has access yet.">
    {#snippet row(g: EditorGrant)}
      <article class="sun-row" aria-label="{label_of(g.capability)} for {subject_of(g)}">
        <span class="sun-tile"><Icon name={g.subject_type === 'member' ? 'user' : 'users'} size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title">{subject_of(g)}</div>
          <div class="sun-row__meta">{scope_of(g.repo_id)}</div>
        </div>
        <Badge tone={g.capability === 'terminal' ? 'warning' : 'neutral'}>{label_of(g.capability)}</Badge>
        <IconButton icon="trash-2" label="Revoke {label_of(g.capability)} for {subject_of(g)}" size="sm" onclick={() => (revoking = g)} />
      </article>
    {/snippet}
  </Loaded>
</section>

<section aria-labelledby="editor-grant-heading" class="sun-section">
  <h3 id="editor-grant-heading">Grant access</h3>
  <form class="grant" onsubmit={grant} novalidate>
    <div class="grant__fields">
      <Select
        label="Who"
        bind:value={subject_type}
        options={[
          { value: 'all', label: 'All members' },
          { value: 'role', label: 'A role' },
          ...(is_owner() ? [{ value: 'group', label: 'A group' }] : []),
          { value: 'member', label: 'A member' },
        ]}
      />
      {#if subject_type === 'role'}
        <Select label="Role" bind:value={role} options={ROLE_OPTIONS} />
      {:else if subject_type === 'group'}
        <Select label="Group" bind:value={group} placeholder="Pick a group" options={groups.map((g) => ({ value: g.id, label: g.name }))} />
      {:else if subject_type === 'member'}
        <Select label="Member" bind:value={member} placeholder="Pick a member" options={members} />
      {/if}
      <Select label="Repository" bind:value={repo_id} options={[{ value: '', label: 'Every repository' }, ...repos.map((r) => ({ value: r.id, label: repo_name(r.url) }))]} />
    </div>
    <fieldset class="grant__capabilities">
      <legend class="sun-label">Can</legend>
      {#each CAPABILITIES as c (c.value)}
        <Checkbox label={c.label} description={c.description} bind:checked={picked[c.value]} />
      {/each}
    </fieldset>
    {#if picked.terminal}
      <p class="grant__warning" role="note">
        <Icon name="triangle-alert" size={16} />
        <span>
          Terminal gives shell access to the host as the Sun service user: its files, network and secrets, not only this
          repository. Grant it only to people you would give a shell on that machine.
        </span>
      </p>
    {/if}
    {#if form_error}<p class="grant__error" role="alert">{form_error}</p>{/if}
    <div><Button type="submit" variant="ice" loading={saving}>Grant</Button></div>
  </form>
</section>

{#if revoking}
  {@const g = revoking}
  <Dialog open onclose={() => (revoking = null)} title="Revoke {label_of(g.capability)}?">
    <p class="note">{subject_of(g)}, {scope_of(g.repo_id)}.</p>
    <p class="note">It applies from their next request, unless another grant still gives it.</p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (revoking = null)}>Cancel</Button>
      <Button variant="danger" loading={busy} onclick={() => revoke(g)}>Revoke</Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .grant {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .grant__fields {
    display: grid;
    gap: var(--space-3);
  }
  @media (min-width: 768px) {
    .grant__fields {
      grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    }
  }
  .grant__capabilities {
    border: 0;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .grant__warning {
    display: flex;
    gap: var(--space-2);
    margin: 0;
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-sm);
    background: var(--warning-soft);
    color: var(--warning-fg);
    font-size: var(--size-sm);
    line-height: 1.5;
  }
  .grant__warning :global(.ml-icon),
  .grant__warning :global(svg) {
    flex: none;
    margin-top: 2px;
  }
  .grant__error {
    margin: 0;
    color: var(--danger-fg);
    font-size: var(--size-sm);
  }
  .note {
    margin: 0;
    color: var(--fg-2);
    font-size: var(--size-sm);
    line-height: 1.5;
  }
</style>
