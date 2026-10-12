<!-- Settings → <workspace> → Code (`edit_workspace`), multica's Code tab: how the workspace reaches its
     git hosts, the repositories its agents work in, and the pull request rules.
     - Code hosting: GitHub (the App's install page) and GitLab (OAuth) connect in a new tab, one click
       each. The kernel says whether the operator configured each app (`sun secret set`); until then the
       button is disabled and the row says what to set. Connecting lands back here with
       `github_connected=1` / `gitlab_connected=1` (or `<provider>_error=<reason>`), plus
       `return_to=repositories` when the picker should open (router `vcs_landing`).
     - Repositories: `{url, description}` rows, typed in or picked from a connection.
     - Pull requests & issues: the workspace's git settings (sidebar, auto-link, move on merge). -->
<script lang="ts">
  import { Badge, Button, Checkbox, Dialog, Icon, Input, Select, Switch } from '@hryer/madlabs-design-system/svelte';
  import Menu from '../lib/ui/Menu.svelte';
  import { api } from '../lib/api/client';
  import type { GitSettings, RemoteRepo, Repo, VcsConnection, VcsList, VcsProvider, Workspace } from '../lib/api/types';
  import { app, current_workspace, error_text, keep_workspace, toast, workspace_of } from '../lib/app_state.svelte';
  import { actor_name, repo_icon, repo_identity, repo_name } from '../lib/format';
  import { vcs_landing } from '../lib/router.svelte';

  const id = (workspace_of(current_workspace()) as Workspace).id;
  const PROVIDER: Record<VcsProvider, string> = { github: 'GitHub', gitlab: 'GitLab' };
  /** The kernel's picker page size: a full page means there may be more. */
  const PAGE = 50;

  let vcs = $state<VcsList | null>(null);
  let repos = $state<Repo[] | null>(null);
  let load_error = $state('');
  const connections = $derived(vcs?.connections ?? []);
  const apps = $derived(vcs?.apps ?? { github: true, gitlab_oauth: true });
  const configured = (provider: VcsProvider) => (provider === 'github' ? apps.github : apps.gitlab_oauth);
  /** Connecting and disconnecting; the kernel decides (`manage_secrets` too, since connections hold tokens). */
  const manage = $derived(vcs?.can_manage ?? false);
  const github = $derived(connections.find((c) => c.provider === 'github'));
  const gitlabs = $derived(connections.filter((c) => c.provider === 'gitlab'));

  async function reload() {
    try {
      [vcs, repos] = await Promise.all([api.list_vcs(id), api.list_repos(id)]);
      load_error = '';
    } catch (e) {
      load_error = error_text(e);
    }
  }
  reload();

  /** A connect started in another tab finishes there: catch up when this tab is back in front. */
  function refocused() {
    if (document.visibilityState === 'visible') reload();
  }

  // How the connect flow came back, in plain words; the router already took it out of the URL.
  const LANDING_ERROR: Record<string, [tone: 'info' | 'danger', title: string, description: string]> = {
    missing_params: ['danger', 'Not connected', 'The git host sent Sun back without the details it needs. Start again here.'],
    invalid_state: ['danger', 'Not connected', 'That connect link expired or was opened in another browser. Start again here.'],
    bad_installation_id: ['danger', 'Not connected', 'The git host named an installation Sun doesn’t recognise. Start again here.'],
    bad_workspace: ['danger', 'Not connected', 'That connect link belongs to another workspace. Start again here.'],
    persist_failed: ['danger', 'Not connected', 'Sun couldn’t save the connection. Try again in a minute.'],
    requested: ['info', 'Waiting for approval', 'An owner of the GitHub organization must approve the install. It connects once they do.'],
    denied: ['info', 'Nothing connected', 'You cancelled on the git host.'],
    refused: ['danger', 'Not connected', 'GitLab refused the sign-in. Check that GITLAB_OAUTH_CLIENT_ID and GITLAB_OAUTH_CLIENT_SECRET match the OAuth app (Settings → Applications on GitLab), then connect again.'],
    unreachable: ['danger', 'Not connected', 'Sun couldn’t reach the git host. Try again in a minute.'],
    failed: ['danger', 'Not connected', 'Something went wrong while connecting. Try again in a minute.'],
  };
  $effect(() => {
    const { provider, outcome, return_to } = vcs_landing;
    if (!provider || !outcome) return;
    Object.assign(vcs_landing, { provider: '', outcome: '', return_to: '' });
    if (outcome !== 'connected') {
      toast(...(LANDING_ERROR[outcome] ?? LANDING_ERROR.failed));
      return;
    }
    toast('success', `${PROVIDER[provider]} connected`, return_to === 'repositories' ? 'Pick the repositories agents may work in.' : 'Add its repositories below.');
    reload().then(() => return_to === 'repositories' && open_picker(provider));
  });

  // Connect, disconnect, rotate.
  let connecting = $state<VcsProvider | ''>('');
  const NOT_CONFIGURED: Record<VcsProvider, string> = {
    github: 'Operators must set GITHUB_APP_SLUG, GITHUB_APP_WEBHOOK_SECRET, GITHUB_APP_ID and GITHUB_APP_PRIVATE_KEY.',
    gitlab: 'Operators must set GITLAB_OAUTH_CLIENT_ID and GITLAB_OAUTH_CLIENT_SECRET (and GITLAB_OAUTH_URL for a self-hosted instance).',
  };

  /** Opens the host's page in a new tab; it lands back on this page. */
  async function connect(provider: VcsProvider, return_to?: 'repositories') {
    connecting = provider;
    try {
      const { url } = await (provider === 'github' ? api.connect_github(id, return_to) : api.connect_gitlab(id, return_to));
      if (url) window.open(url, '_blank', 'noopener');
      else toast('warning', `${PROVIDER[provider]} integration is not configured for this deployment`, NOT_CONFIGURED[provider]);
    } catch (e) {
      toast('danger', `Couldn’t open ${PROVIDER[provider]}`, error_text(e));
    }
    connecting = '';
  }

  let disconnecting = $state<VcsConnection | null>(null);
  let busy = $state(false);

  async function disconnect(connection: VcsConnection) {
    busy = true;
    try {
      await api.disconnect_vcs(connection.id);
      toast('success', `${PROVIDER[connection.provider]} disconnected`);
    } catch (e) {
      toast('danger', 'Still connected', error_text(e));
    }
    busy = false;
    disconnecting = null;
    reload();
  }

  async function rotate(connection: VcsConnection) {
    try {
      const { updated } = await api.rotate_vcs_webhook(connection.id);
      toast('success', 'Webhook secret rotated', `${updated} ${updated === 1 ? 'webhook' : 'webhooks'} updated.`);
    } catch (e) {
      toast('danger', 'Not rotated', error_text(e));
    }
  }

  const connection_menu = (c: VcsConnection) => [
    ...(c.provider === 'gitlab' ? [{ label: 'Rotate webhook', icon: 'refresh-cw', onselect: () => rotate(c) }] : []),
    { label: 'Disconnect', icon: 'unplug', danger: true, onselect: () => (disconnecting = c) },
  ];
  const host_of = (url: string) => url.replace(/^https?:\/\//, '');
  const connected_line = (c: VcsConnection) => `Connected to ${c.account_login}${c.provider === 'gitlab' ? ` on ${host_of(c.instance_url)}` : ''} · Connected by ${actor_name(c.connected_by, app.me?.user)}`;

  // Repositories: one dialog adds by URL or edits the description.
  let draft = $state<{ repo: Repo | null; url: string; description: string } | null>(null);
  let draft_error = $state('');
  let saving = $state(false);
  const draft_identity = $derived(draft && !draft.repo ? repo_identity(draft.url) : null);
  const duplicate = $derived(!!draft_identity && (repos ?? []).some((r) => repo_identity(r.url) === draft_identity));

  function open_draft(repo: Repo | null) {
    draft = { repo, url: repo?.url ?? '', description: repo?.description ?? '' };
    draft_error = '';
  }

  async function save_draft(e: Event) {
    e.preventDefault();
    if (!draft || saving || duplicate || !draft.url.trim()) return;
    const [url, description] = [draft.url.trim(), draft.description.trim()];
    saving = true;
    draft_error = '';
    try {
      if (draft.repo) await api.update_repo(draft.repo.id, { description });
      else await api.add_repo(id, { url, description });
      toast('success', `${repo_name(url)} ${draft.repo ? 'saved' : 'added'}`);
      draft = null;
      await reload();
    } catch (err) {
      draft_error = error_text(err);
    }
    saving = false;
  }

  let removing = $state<Repo | null>(null);

  async function remove(repo: Repo) {
    busy = true;
    try {
      await api.remove_repo(id, repo.id);
      toast('success', `${repo_name(repo.url)} deleted`);
    } catch (e) {
      toast('danger', 'Not deleted', error_text(e));
    }
    busy = false;
    removing = null;
    reload();
  }

  // The picker: a connection's repositories, searched, 50 a page, ticked and added together.
  let picker = $state<VcsProvider | ''>('');
  let picker_connection = $state('');
  let query = $state('');
  let remote = $state<RemoteRepo[]>([]);
  let page = $state(1);
  let more = $state(false);
  let picker_busy = $state(false);
  let picker_error = $state('');
  let picked = $state<Record<string, RemoteRepo>>({});
  let timer: ReturnType<typeof setTimeout> | undefined;
  const picked_count = $derived(Object.keys(picked).length);
  const added = $derived(new Set((repos ?? []).map((r) => repo_identity(r.url))));
  const picker_connections = $derived(connections.filter((c) => c.provider === picker));

  function open_picker(provider: VcsProvider) {
    const first = connections.find((c) => c.provider === provider);
    if (!first) return;
    [picker, picker_connection, query, remote, picked] = [provider, first.id, '', [], {}];
    search(true);
  }

  async function search(reset: boolean) {
    if (!picker_connection) return;
    picker_busy = true;
    picker_error = '';
    const next = reset ? 1 : page + 1;
    try {
      const found = await api.list_remote_repos(id, picker_connection, query.trim(), next);
      remote = reset ? found : [...remote, ...found];
      page = next;
      more = found.length >= PAGE;
    } catch (e) {
      picker_error = error_text(e);
    }
    picker_busy = false;
  }

  function typed() {
    clearTimeout(timer);
    timer = setTimeout(() => search(true), 300);
  }

  function tick(r: RemoteRepo, on: boolean) {
    if (on) picked[r.url] = r;
    else delete picked[r.url];
  }

  async function add_picked() {
    saving = true;
    let added_count = 0;
    for (const r of Object.values(picked)) {
      try {
        await api.add_repo(id, { url: r.url, description: r.description, default_branch: r.default_branch });
        added_count++;
      } catch (e) {
        toast('danger', `${r.name} not added`, error_text(e));
      }
    }
    saving = false;
    picker = '';
    if (added_count) toast('success', `${added_count} ${added_count === 1 ? 'repository' : 'repositories'} added`);
    reload();
  }

  const add_items = $derived([
    ...(['github', 'gitlab'] as const).map((p) =>
      connections.some((c) => c.provider === p)
        ? { label: `Choose from ${PROVIDER[p]}`, icon: p, onselect: () => open_picker(p) }
        : { label: `Connect ${PROVIDER[p]}`, icon: p, disabled: !manage || !configured(p), hint: configured(p) ? undefined : 'Not configured for this deployment', onselect: () => connect(p, 'repositories') },
    ),
    { label: 'Enter a URL…', icon: 'pencil', onselect: () => open_draft(null) },
  ]);

  // Pull request rules: the workspace's git settings, saved one switch at a time.
  const settings = $derived(workspace_of(id)?.git_settings as GitSettings);
  let saving_setting = $state<keyof GitSettings | ''>('');
  const RULES: { key: keyof GitSettings; label: string; description: string }[] = $derived([
    { key: 'pr_sidebar', label: 'Pull Request sidebar', description: 'Show linked pull requests inside the task detail sidebar.' },
    { key: 'auto_link', label: 'Auto-link issues and PRs', description: `Links a PR to the task whose ID, e.g. ${workspace_of(id)?.issue_prefix ?? 'SUN'}-123, is in its title or branch name, or follows “Closes” in its description.` },
    { key: 'move_on_merge', label: 'Move tasks when their PRs merge', description: 'Workflow tasks move to Done, agent tasks to Needs you, once every linked PR has merged.' },
  ]);

  async function set_rule(key: keyof GitSettings, value: boolean) {
    saving_setting = key;
    try {
      keep_workspace(await api.update_workspace(id, { git_settings: { ...settings, [key]: value } }));
    } catch (e) {
      toast('danger', 'Setting not saved', error_text(e));
    }
    saving_setting = '';
  }
</script>

<svelte:window onfocus={refocused} />
<svelte:document onvisibilitychange={refocused} />

<section class="sun-row code" aria-labelledby="hosting-head">
  <h3 class="code__title" id="hosting-head">Code hosting</h3>
  {#if load_error}<p class="ml-field__hint ml-field__hint--error" role="alert">{load_error}</p>{/if}
  {#if vcs && !manage}<p class="code__note">Connecting a git host needs “Manage secrets” as well. Ask someone who has it.</p>{/if}

  <ul class="hosts" aria-label="Git hosts">
    <!-- GitHub: one installation per workspace. -->
    <li class="host">
      <span class="sun-tile"><Icon name="github" size={18} /></span>
      <div class="host__body">
        <div class="host__name">GitHub {#if github}<Badge tone="success" dot>Connected</Badge>{/if}</div>
        <div class="host__meta">
          {#if github}{connected_line(github)}{:else if !vcs}Loading…{:else if !apps.github}
            GitHub integration is not configured for this deployment. Operators must set <code>GITHUB_APP_SLUG</code>, <code>GITHUB_APP_WEBHOOK_SECRET</code>, <code>GITHUB_APP_ID</code>
            and <code>GITHUB_APP_PRIVATE_KEY</code> with <code>sun secret set</code> or <code>scripts/load-git-apps.sh</code>.
          {:else}Not connected{/if}
        </div>
      </div>
      {#if manage}
        {#if github}
          <Menu label="GitHub actions" icon="ellipsis" items={connection_menu(github)} />
        {:else}
          <Button variant="secondary" size="sm" iconRight="external-link" disabled={!apps.github} loading={connecting === 'github'} onclick={() => connect('github')}>Connect GitHub</Button>
        {/if}
      {/if}
    </li>

    <!-- GitLab: one row per connected instance, else the connect row. -->
    {#each gitlabs as c (c.id)}
      <li class="host">
        <span class="sun-tile"><Icon name="gitlab" size={18} /></span>
        <div class="host__body">
          <div class="host__name">GitLab <Badge tone="success" dot>Connected</Badge></div>
          <div class="host__meta">{connected_line(c)}</div>
        </div>
        {#if manage}<Menu label="GitLab {host_of(c.instance_url)} actions" icon="ellipsis" items={connection_menu(c)} />{/if}
      </li>
    {:else}
      <li class="host">
        <span class="sun-tile"><Icon name="gitlab" size={18} /></span>
        <div class="host__body">
          <div class="host__name">GitLab</div>
          <div class="host__meta">
            {#if !vcs}Loading…{:else if !apps.gitlab_oauth}
              GitLab integration is not configured for this deployment. Operators must set <code>GITLAB_OAUTH_CLIENT_ID</code> and <code>GITLAB_OAUTH_CLIENT_SECRET</code>
              (and <code>GITLAB_OAUTH_URL</code> for a self-hosted instance) with <code>sun secret set</code> or <code>scripts/load-git-apps.sh</code>.
            {:else}Not connected{/if}
          </div>
        </div>
        {#if manage}
          <Button variant="secondary" size="sm" iconRight="external-link" disabled={!apps.gitlab_oauth} loading={connecting === 'gitlab'} onclick={() => connect('gitlab')}>Connect GitLab</Button>
        {/if}
      </li>
    {/each}
  </ul>
</section>

<section class="sun-row code" aria-labelledby="repos-head">
  <div class="code__bar">
    <div>
      <h3 class="code__title" id="repos-head">Repositories</h3>
      <p class="code__note">Agents can use these repositories to work on code.</p>
    </div>
    <Menu label="Repositories" text="Add repository" variant="secondary" icon="plus" items={add_items} />
  </div>

  {#if repos?.length}
    <ul class="repos" aria-label="Repositories">
      {#each repos as repo (repo.id)}
        <li class="repo">
          <span class="repo__icon" aria-hidden="true"><Icon name={repo_icon(repo.url)} size={16} /></span>
          <div class="repo__body">
            <div class="repo__url ml-mono" title={repo.url}>{repo.url}</div>
            {#if repo.description}<div class="repo__meta">{repo.description}</div>{/if}
          </div>
          <Menu
            label="Actions for {repo.url}"
            icon="ellipsis"
            items={[
              { label: 'Edit', icon: 'pencil', onselect: () => open_draft(repo) },
              { label: 'Delete repository', icon: 'trash-2', danger: true, onselect: () => (removing = repo) },
            ]}
          />
        </li>
      {/each}
    </ul>
  {:else if repos}
    <div class="sun-empty">No repositories yet.</div>
  {/if}
</section>

<section class="sun-row code" aria-labelledby="rules-head">
  <h3 class="code__title" id="rules-head">Pull requests & issues</h3>
  <ul class="rules" aria-label="Pull request rules">
    {#each RULES as rule (rule.key)}
      <li class="rule">
        <div class="rule__text">
          <div class="rule__label">{rule.label}</div>
          <p class="code__note">{rule.description}</p>
        </div>
        <Switch aria-label={rule.label} checked={settings[rule.key]} disabled={saving_setting === rule.key} onchange={(e: Event) => set_rule(rule.key, (e.currentTarget as HTMLInputElement).checked)} />
      </li>
    {/each}
  </ul>
</section>

<!-- Dialogs -->
{#if disconnecting}
  {@const c = disconnecting}
  <Dialog open onclose={() => (disconnecting = null)} title={c.provider === 'github' ? 'Disconnect GitHub App' : 'Disconnect GitLab'}>
    <p class="code__note">
      Sun will stop receiving webhooks for this {c.provider === 'github' ? 'installation' : 'GitLab'}. Existing PR mirrors and links are kept; reconnect any time to resume updates.
    </p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (disconnecting = null)}>Cancel</Button>
      <Button variant="danger" loading={busy} onclick={() => disconnect(c)}>Disconnect</Button>
    {/snippet}
  </Dialog>
{/if}

{#if draft}
  <Dialog open onclose={() => (draft = null)} title={draft.repo ? 'Edit repository' : 'Add repository'}>
    <form id="repo-draft" class="code__form" onsubmit={save_draft} novalidate>
      <p class="code__note">Agents can use these repositories to work on code.</p>
      <Input
        label="Repository URL"
        mono
        autocomplete="off"
        spellcheck={false}
        placeholder="https://git.example.com/org/repo.git or git@git.example.com:org/repo.git"
        disabled={!!draft.repo}
        bind:value={draft.url}
        error={duplicate ? 'This repository is already added.' : draft_error || undefined}
      />
      <Input label="Description" autocomplete="off" placeholder="Description (e.g. Go backend + Next.js frontend)" maxlength={500} bind:value={draft.description} />
    </form>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (draft = null)}>Cancel</Button>
      <Button variant="ice" type="submit" form="repo-draft" loading={saving} disabled={!draft?.url.trim() || duplicate}>Save</Button>
    {/snippet}
  </Dialog>
{/if}

{#if picker}
  {@const provider = picker}
  <Dialog open onclose={() => (picker = '')} title="Choose {PROVIDER[provider]} repositories">
    <div class="code__form">
      <p class="code__note">Existing manual entries are unchanged.</p>
      {#if picker_connections.length > 1}
        <Select label="{PROVIDER[provider]} account" options={picker_connections.map((c) => ({ value: c.id, label: c.account_login }))} bind:value={picker_connection} onchange={() => search(true)} />
      {:else if picker_connections[0]}
        <p class="code__note">{PROVIDER[provider]} account: <strong>{picker_connections[0].account_login}</strong></p>
      {/if}
      <Input aria-label="Search loaded repositories" iconLeft="search" autocomplete="off" placeholder="Search loaded repositories" bind:value={query} oninput={typed} />
      {#if picker_error}<p class="ml-field__hint ml-field__hint--error" role="alert">{picker_error}</p>{/if}
      <ul class="picker" aria-label="Repositories you can add" aria-busy={picker_busy}>
        {#each remote as r (r.url)}
          {@const done = added.has(repo_identity(r.url))}
          <li class="picker__row">
            <Checkbox label={r.name} description={r.description || undefined} checked={done || r.url in picked} disabled={done} onchange={(e: Event) => tick(r, (e.currentTarget as HTMLInputElement).checked)} />
            <span class="picker__tags">
              {#if r.private}<Badge>Private</Badge>{/if}
              {#if done}<Badge tone="success">Added</Badge>{/if}
            </span>
          </li>
        {:else}
          <li class="sun-empty" role="status">{picker_busy ? 'Loading repositories…' : query ? 'No loaded repositories match this search.' : 'This connection has no accessible repositories.'}</li>
        {/each}
      </ul>
      {#if more}<div><Button variant="secondary" size="sm" loading={picker_busy} onclick={() => search(false)}>Load more</Button></div>{/if}
    </div>
    {#snippet footer()}
      <p class="code__note picker__count" role="status">{picked_count} selected</p>
      <Button variant="secondary" onclick={() => (picker = '')}>Cancel</Button>
      <Button variant="ice" loading={saving} disabled={!picked_count} onclick={add_picked}>Add repositories</Button>
    {/snippet}
  </Dialog>
{/if}

{#if removing}
  {@const repo = removing}
  <Dialog open onclose={() => (removing = null)} title="Delete repository?">
    <p class="code__note">This repository will be removed from the workspace immediately.</p>
    <p class="code__note ml-mono">{repo.url}</p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (removing = null)}>Cancel</Button>
      <Button variant="danger" loading={busy} onclick={() => remove(repo)}>Delete repository</Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .code {
    flex-direction: column;
    align-items: stretch;
    flex-wrap: nowrap;
    gap: var(--space-3);
  }
  .code__title {
    margin: 0;
    font-size: 15px;
    font-weight: 600;
    color: var(--fg-1);
  }
  .code__note {
    margin: 0;
    color: var(--fg-2);
    font-size: var(--size-sm);
    line-height: 1.5;
  }
  .code__bar {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: var(--space-3);
  }
  .code__form {
    display: grid;
    gap: var(--space-3);
  }
  .hosts,
  .repos,
  .rules,
  .picker {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .host,
  .repo,
  .rule,
  .picker__row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-3) 0;
    border-top: 1px solid var(--border-subtle);
  }
  .host:first-child,
  .repo:first-child,
  .rule:first-child,
  .picker__row:first-child {
    border-top: 0;
  }
  .host__body,
  .repo__body,
  .rule__text {
    flex: 1;
    min-width: 0;
    display: grid;
    gap: 2px;
  }
  /* A phone puts the row's button under its text instead of squeezing the text. */
  .host {
    flex-wrap: wrap;
  }
  .host__body {
    flex-basis: 180px;
  }
  .host__name,
  .rule__label {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-2);
    font-size: 14px;
    font-weight: 600;
    color: var(--fg-1);
  }
  .host__meta,
  .repo__meta {
    font-size: var(--size-xs);
    color: var(--fg-2);
    line-height: 1.5;
    overflow-wrap: anywhere;
  }
  .host__meta code {
    font-family: var(--font-mono);
    font-size: var(--size-xs);
    padding: 1px 4px;
    border-radius: var(--radius-sm);
    background: var(--bg-subtle);
    color: var(--fg-1);
  }
  .repo__icon {
    display: grid;
    place-items: center;
    width: 20px;
    flex: none;
    color: var(--fg-2);
  }
  .repo__url {
    font-size: 13px;
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
  .rule {
    align-items: flex-start;
  }
  .rule :global(.ml-switch) {
    flex: none;
    margin-top: 2px;
  }
  .picker {
    max-height: 320px;
    overflow-y: auto;
  }
  .picker__row {
    justify-content: space-between;
  }
  .picker__row :global(.ml-check) {
    min-width: 0;
  }
  .picker__tags {
    display: flex;
    flex: none;
    gap: var(--space-1);
  }
  .picker__count {
    margin-right: auto;
    align-self: center;
  }
  :global(.ml-dialog__footer > .picker__count) {
    flex: none;
  }
</style>
