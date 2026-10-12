<!-- Catalog: installed and suggested skills, MCP servers (GET /catalog) and role templates
     (GET /templates). Suggestions are a ranking only; nothing installs until the user reads the
     files and clicks Install, and a template's mods only with their own checkbox. -->
<script lang="ts">
  import { Badge, Button, Checkbox, Dialog, Icon, Input, Radio, Select, Switch, Tabs, Tag } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import McpServerDialog from '../lib/ui/McpServerDialog.svelte';
  import { ECOSYSTEM_SCOPE, api } from '../lib/api/client';
  import type { CatalogEntry, CatalogFile, CatalogView, McpServer, McpServerAdded, ModInstall, Repo, SkillPr, WorkflowTemplate } from '../lib/api/types';
  import { current_workspace, error_text, toast } from '../lib/app_state.svelte';
  import { repo_name, safe_url } from '../lib/format';

  type Kind = CatalogEntry['kind'] | 'workflow';
  /** A catalog entry, a role template or a custom MCP server, shown the same way. */
  type Row = Omit<CatalogEntry, 'kind'> & { kind: Kind; meta: string; mods: string[]; source_yaml?: string; custom?: boolean; pr?: SkillPr };
  const KINDS: { id: Kind; label: string }[] = [
    { id: 'skill', label: 'Skills' },
    { id: 'mcp', label: 'MCP servers' },
    { id: 'workflow', label: 'Workflows' },
  ];
  const ICON: Record<Kind, string> = { skill: 'sparkles', mcp: 'plug', workflow: 'list-ordered' };
  const WHERE: Record<Kind, string> = {
    skill: 'Copied into each picked agent’s own skills folder when a session starts. The copies stay out of your commits.',
    mcp: 'Registers the server for this ecosystem. Turn it on per workflow or task; only read-only tools are on by default.',
    workflow: 'Saves it as a workflow you can run and edit.',
  };

  /** Runtimes with a native skills folder (sun-runtime `skills::native_dir`). */
  const SKILL_RUNTIMES = [
    { id: 'claude-code', label: 'Claude Code', folder: '.claude/skills' },
    { id: 'codex', label: 'Codex', folder: '.agents/skills' },
    { id: 'opencode', label: 'OpenCode', folder: '.opencode/skills' },
  ];
  const ALL_RUNTIMES = SKILL_RUNTIMES.map((r) => r.id);
  const runtime_label = (id: string) => SKILL_RUNTIMES.find((r) => r.id === id)?.label ?? id;

  /** A repo-scope install's PR, by its `open_pr` delivery. */
  const PR_STATE = {
    pending: 'Branch pushed. Asking the code host for the PR…',
    unknown: 'No answer from the code host yet. Check the repo’s pull requests.',
    failed: 'The code host refused the PR. Check the git mod’s token, then try again.',
    delivered: 'PR opened.',
  } as const;
  /** Status reads while a PR is pending, 1.5 s apart. */
  const PR_POLLS = 40;

  const CHANGE_LABEL = { added: 'New', changed: 'Changed', removed: 'Removed' } as const;
  const CHANGE_TONE = { added: 'success', changed: 'info', removed: 'danger' } as const;

  // Per-viewer conveniences only; the page works without storage.
  const read = (key: string) => {
    try {
      return localStorage.getItem(key);
    } catch {
      return null;
    }
  };
  const save = (key: string, value: string) => {
    try {
      localStorage.setItem(key, value);
    } catch {
      /* private window */
    }
  };

  let kind = $state<Kind>('skill');
  let tab = $state('installed');
  let repo = $state(read('sun.catalog.repo') ?? '');
  let repo_error = $state('');
  let view = $state<CatalogView | null>(null);
  let templates = $state<WorkflowTemplate[]>([]);
  let workflow_names = $state<string[]>([]);
  let mods = $state<ModInstall[]>([]);
  let mod_picks = $state<string[]>([]);
  /** Skill dialog: the runtimes that get it. */
  let runtime_picks = $state<string[]>([]);
  /** Skill dialog: the ecosystem store, or one repo by PR. */
  let scope = $state<'ecosystem' | 'repo'>('ecosystem');
  let repos = $state<Repo[]>([]);
  let repo_pick = $state('');
  let skill_pr = $state<SkillPr | null>(null);
  /** Bumped whenever the dialog's skill or repo changes, so an older poll stops. */
  let poll_token = 0;
  /** Write-only values typed in the install dialog, by secret name. */
  let secret_values = $state<Record<string, string>>({});
  /** Ecosystem secrets already stored (names only). */
  let stored_secrets = $state<string[]>([]);
  let load_error = $state('');
  /** The ecosystem's dismissed suggestions (server-side, so every browser agrees). */
  let dismissed = $state<string[]>([]);
  /** Installed skills: where to look (the ecosystem store or one repo by id) and for which agent. */
  let installed_in = $state('ecosystem');
  let agent = $state('');
  let repo_installs = $state<SkillPr[] | null>(null);
  let show_suggestions = $state(read('sun.catalog.suggestions') !== 'off');
  let picked = $state<Row | null>(null);
  let files = $state<CatalogFile[] | null>(null);
  let busy = $state('');

  // MCP servers added by hand live in the same registry as catalog installs.
  let mcp_servers = $state<McpServer[]>([]);
  let adding_mcp = $state(false);

  async function load() {
    const folder = repo.trim();
    try {
      [view, mcp_servers] = await Promise.all([api.list_catalog(folder), api.list_mcp_servers().catch(() => [])]);
      dismissed = view.dismissed;
      repo_error = load_error = '';
      save('sun.catalog.repo', folder);
    } catch (e) {
      if (folder) repo_error = error_text(e);
      else load_error = error_text(e);
    }
  }
  load();

  async function load_templates() {
    try {
      const [t, w, m] = await Promise.all([api.list_templates(), api.list_workflows(current_workspace()), api.list_mods()]);
      [templates, workflow_names, mods] = [t, w.map((x) => x.name), m];
    } catch (e) {
      load_error = error_text(e);
    }
  }
  load_templates();
  api.list_repos(current_workspace()).then((r) => (repos = r), () => (repos = []));

  /** The picked repo's by-PR skill installs. */
  async function load_repo_installs() {
    const repo_id = installed_in;
    repo_installs = null;
    if (repo_id === 'ecosystem') return;
    try {
      const installs = (await api.list_catalog('', repo_id)).repo_installs ?? [];
      if (installed_in === repo_id) repo_installs = installs;
    } catch (e) {
      load_error = error_text(e);
    }
  }
  $effect(() => {
    void installed_in;
    load_repo_installs();
  });

  const entry_row = (e: CatalogEntry): Row => ({ ...e, meta: `${e.source} · ${e.author} · ${e.license}`, mods: [] });
  const template_row = (t: WorkflowTemplate): Row => ({
    id: t.id, kind: 'workflow', description: t.description, source: 'Sun templates', author: 'Sun', license: '',
    version: '1', stacks: [], reviewed: true, installed: workflow_names.includes(t.id) ? '1' : null,
    matches: [], in_workflows: false, hosts: [], secrets: [], mods: t.mods, source_yaml: t.source_yaml,
    meta: `${t.title} · ${t.role}${t.mods.length ? ` · uses the ${t.mods.join(', ')} mod${t.mods.length > 1 ? 's' : ''}` : ''}`,
  });

  const custom_row = (s: McpServer): Row => {
    const t = s.transport;
    const secrets = t.type === 'url' ? (t.bearer_secret ? [t.bearer_secret] : []) : t.secrets;
    return {
      id: s.name, kind: 'mcp', custom: true, reviewed: false, installed: 'custom', version: '',
      description: t.type === 'url' ? t.url : [t.command, ...t.args].join(' '),
      source: 'Added by hand', author: '', license: '', stacks: [], matches: [], in_workflows: false, mods: [], secrets,
      hosts: t.type === 'url' ? [] : t.network, meta: t.type === 'url' ? 'Added by hand · URL' : 'Added by hand · runs sandboxed',
    };
  };

  $effect(() => save('sun.catalog.suggestions', show_suggestions ? 'on' : 'off'));

  const catalog_ids = $derived(new Set(view?.entries.map((e) => e.id) ?? []));
  const of_kind = $derived(
    kind === 'workflow'
      ? templates.map(template_row)
      : [
          ...(view?.entries.filter((e) => e.kind === kind) ?? []).map(entry_row),
          ...(kind === 'mcp' ? mcp_servers.filter((s) => !catalog_ids.has(s.name)).map(custom_row) : []),
        ],
  );

  async function mcp_added(added: McpServerAdded) {
    tab = 'installed';
    toast('success', `${added.server.name} added`, `${added.tools.length} tools found. Turn it on per task.`);
    await load();
  }
  const missing_mods = $derived(picked?.mods.filter((m) => !mods.some((x) => x.installed && x.name === m)) ?? []);
  const asks_of = (name: string) => mods.find((m) => m.name === name)?.permissions ?? [];
  /** Skills only: the ecosystem store per agent, or one repo's by-PR installs. */
  const by_repo = $derived(kind === 'skill' && installed_in !== 'ecosystem');
  const for_agent = (e: Row) => !agent || e.kind !== 'skill' || !e.runtimes || e.runtimes.includes(agent);
  const installed = $derived(
    by_repo
      ? (repo_installs ?? []).flatMap((pr) => {
          const e = view?.entries.find((x) => x.id === pr.skill);
          return e ? [{ ...entry_row(e), installed: pr.version, pr }] : [];
        })
      : of_kind.filter((e) => e.installed && for_agent(e)),
  );
  const suggested = $derived(of_kind.filter((e) => !e.installed && !dismissed.includes(e.id)));
  const rows = $derived(tab === 'installed' ? installed : show_suggestions ? suggested : []);
  const empty = $derived(
    tab === 'installed'
      ? by_repo
        ? repo_installs
          ? 'No skills added to this repo yet. Install one from Suggested and pick “A repo, by pull request”.'
          : 'Loading…'
        : agent && kind === 'skill'
          ? `No skills installed for ${runtime_label(agent)}.`
          : 'Nothing installed yet. Pick one under Suggested.'
      : show_suggestions
        ? 'No suggestions right now.'
        : 'Suggestions are hidden.',
  );

  /** Hidden for the whole ecosystem at once; put back if the kernel refuses. */
  async function dismiss(e: Row) {
    dismissed = [...dismissed, e.id];
    try {
      await api.dismiss_catalog(e.id);
    } catch (err) {
      dismissed = dismissed.filter((id) => id !== e.id);
      toast('danger', `Couldn’t dismiss ${e.id}`, error_text(err));
    }
  }

  async function restore() {
    try {
      await Promise.all(dismissed.map((id) => api.restore_catalog(id)));
    } catch (err) {
      toast('danger', 'Couldn’t show the dismissed suggestions', error_text(err));
    }
    await load();
  }

  /** Shows `pr` and reads it again while the code host hasn't answered. */
  async function follow(pr: SkillPr, token: number, polls = 0) {
    skill_pr = pr;
    if (pr.pr?.status !== 'pending' || polls >= PR_POLLS) return;
    await new Promise((done) => setTimeout(done, 1500));
    if (token !== poll_token) return;
    try {
      await follow(await api.skill_pr(pr.skill, pr.repo_id), token, polls + 1);
    } catch {
      /* keep the last state shown */
    }
  }

  // The picked repo's earlier install of this skill, if any.
  $effect(() => {
    const [e, repo_id] = [picked, repo_pick];
    skill_pr = null;
    const token = ++poll_token;
    if (!e || e.kind !== 'skill' || scope !== 'repo' || !repo_id) return;
    api.skill_pr(e.id, repo_id).then(
      (pr) => {
        if (token === poll_token) follow(pr, token);
      },
      () => {}, // 404: never installed there
    );
  });

  /** Opens the install dialog; with `repo_id`, preset to update that repo's PR. */
  async function pick(e: Row, repo_id = '') {
    picked = e;
    files = null;
    mod_picks = [];
    runtime_picks = e.runtimes ?? ALL_RUNTIMES;
    scope = repo_id ? 'repo' : 'ecosystem';
    repo_pick = repo_id;
    if (e.kind === 'skill') api.list_repos(current_workspace()).then((r) => (repos = r), () => (repos = []));
    secret_values = {};
    if (e.secrets.length) {
      api.list_secrets(ECOSYSTEM_SCOPE).then(
        (s) => (stored_secrets = s.map((x) => x.name)),
        () => (stored_secrets = []), // members can't list; saving still reports the error
      );
    }
    if (e.kind === 'workflow') {
      files = [{ path: `${e.id}.yaml`, text: e.source_yaml ?? '', change: null }];
      return;
    }
    try {
      files = await api.catalog_files(e.id);
    } catch (err) {
      picked = null;
      toast('danger', 'Couldn’t load the files', error_text(err));
    }
  }

  async function install() {
    if (!picked) return;
    const e = picked;
    busy = e.id;
    try {
      if (e.kind === 'workflow') {
        // Only the mods the user ticked; a failed mod stops before the workflow is added.
        for (const m of mod_picks) await api.install_mod(m, asks_of(m));
        await api.install_template(e.id, current_workspace());
      } else {
        // Every runtime: send none, so runtimes added later get it too.
        const all = ALL_RUNTIMES.every((r) => runtime_picks.includes(r));
        const runtimes = e.kind === 'skill' && !all ? $state.snapshot(runtime_picks) : undefined;
        if (e.kind === 'skill' && scope === 'repo') {
          // The dialog stays open to show the PR link once the code host answers.
          const pr = await api.install_skill_pr(e.id, repo_pick, runtimes);
          toast('success', `Pushed ${pr.branch}`, 'Review and merge the PR to add the skill to the repo.');
          follow(pr, ++poll_token);
          if (by_repo) load_repo_installs();
          busy = '';
          return;
        }
        await api.install_catalog(e.id, runtimes);
        // Ecosystem scope: the MCP server reads them when Sun connects. Values never come back.
        for (const name of e.secrets) if (secret_values[name]) await api.set_secret(ECOSYSTEM_SCOPE, name, secret_values[name]);
      }
      secret_values = {};
      picked = null;
      tab = 'installed';
      toast('success', `${e.id} ${e.installed ? `updated to v${e.version}` : 'installed'}`, WHERE[e.kind]);
      await Promise.all([load(), load_templates()]);
    } catch (err) {
      toast('danger', `Couldn’t install ${e.id}`, error_text(err));
    }
    busy = '';
  }

  async function remove(e: Row) {
    busy = e.id;
    try {
      await (e.custom ? api.remove_mcp_server(e.id) : api.uninstall_catalog(e.id));
      toast('success', `${e.id} removed`, 'Agents stop getting it from their next session.');
      await load();
    } catch (err) {
      toast('danger', `Couldn’t remove ${e.id}`, error_text(err));
    }
    busy = '';
  }
</script>

<PageHeader title="Catalog" subtitle="Skills, MCP servers and workflow templates. Nothing installs without a click.">
  <Tabs variant="underline" bind:value={kind} items={KINDS} />
</PageHeader>

<div class="sun-page">
  <form class="catalog-repo" onsubmit={(e) => (e.preventDefault(), load())}>
    <Input
      label="Suggest for a repo"
      placeholder="/path/to/your/checkout"
      iconLeft="folder"
      mono
      bind:value={repo}
      error={repo_error || undefined}
      hint={repo_error ? undefined : 'Absolute path on the kernel’s machine. Suggestions rank by the stack found there.'}
    />
    <Button type="submit" variant="secondary" iconLeft="scan-search">Detect stack</Button>
  </form>
  {#if view?.stacks.length}
    <div class="sun-chips" aria-label="Detected stack">
      {#each view.stacks as s (s)}<Tag size="sm">{s}</Tag>{/each}
    </div>
  {/if}

  <div class="catalog-bar">
    <Tabs
      bind:value={tab}
      items={[
        { id: 'installed', label: 'Installed', badge: installed.length },
        { id: 'suggested', label: 'Suggested', badge: show_suggestions ? suggested.length : undefined },
      ]}
    />
    <span class="sun-header__spacer"></span>
    {#if kind === 'mcp'}
      <Button size="sm" variant="secondary" iconLeft="plus" onclick={() => (adding_mcp = true)}>Add MCP server</Button>
    {/if}
    {#if tab === 'suggested'}
      {#if dismissed.length}<Button size="sm" variant="ghost" onclick={restore}>Show {dismissed.length} dismissed</Button>{/if}
      <Switch label="Show suggestions" bind:checked={show_suggestions} />
    {/if}
  </div>
  {#if kind === 'skill' && tab === 'installed'}
    <div class="catalog-filters">
      <Select
        label="Installed in"
        size="sm"
        bind:value={installed_in}
        options={[
          { value: 'ecosystem', label: 'Ecosystem' },
          ...repos.map((r) => ({ value: r.id, label: `${repo_name(r.url)} (by PR)` })),
        ]}
      />
      {#if !by_repo}
        <Select
          label="Agent"
          size="sm"
          bind:value={agent}
          options={[{ value: '', label: 'All agents' }, ...SKILL_RUNTIMES.map((r) => ({ value: r.id, label: r.label }))]}
        />
      {/if}
    </div>
  {/if}

  {#if load_error}
    <div class="sun-empty" role="alert">{load_error}</div>
  {:else if !view}
    <div class="sun-empty" role="status">Loading…</div>
  {:else}
    {#each rows as e (e.id)}
      <article class="sun-row">
        <span class="sun-tile" class:sun-tile--ice={e.installed}><Icon name={ICON[e.kind]} size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title">
            <span class="ml-mono">{e.id}</span>
            {#if e.kind !== 'workflow' && !e.custom}<span class="ml-mono catalog-version">v{e.installed ?? e.version}</span>{/if}
            {#if !e.reviewed}<Badge tone="warning">Unreviewed</Badge>{/if}
          </div>
          <div class="catalog-desc">{e.description}</div>
          <div class="sun-row__meta">{e.meta}</div>
          {#if e.pr}
            {@const url = safe_url(e.pr.pr?.url)}
            <div class="sun-row__meta catalog-pr">
              <Icon name="git-pull-request" size={14} />
              {#if url}
                <a href={url} target="_blank" rel="noopener noreferrer">Review the PR for {e.pr.branch}</a>
              {:else}
                {e.pr.pr ? PR_STATE[e.pr.pr.status] : `Pushing ${e.pr.branch}…`}
              {/if}
            </div>
          {:else if e.kind === 'skill' && e.installed && e.runtimes}
            <div class="sun-row__meta">For {e.runtimes.map(runtime_label).join(', ')} only</div>
          {/if}
          {#if e.kind === 'mcp'}
            <div class="sun-row__meta ml-mono">
              {#if !e.custom}{e.tools?.length ?? 0} tools{/if}{#if e.hosts.length} · reaches {e.hosts.join(', ')}{/if}{#if e.secrets.length} · needs {e.secrets.join(', ')}{/if}
            </div>
          {/if}
          {#if !e.installed && e.in_workflows}
            <div class="catalog-why"><Icon name="list-ordered" size={14} />Named in your workflows</div>
          {/if}
          {#if !e.installed && e.matches.length}
            <div class="catalog-why"><Icon name="scan-search" size={14} />Fits this repo: {e.matches.join(', ')}</div>
          {/if}
        </div>
        <div class="sun-row__aside">
          {#if e.pr}
            {#if e.pr.version !== e.version}
              <Button size="sm" variant="ice" iconLeft="git-pull-request" onclick={() => pick(e, e.pr?.repo_id)}>Update the PR to v{e.version}</Button>
            {:else}
              <Badge tone="info" dot>In the repo by PR</Badge>
            {/if}
          {:else if e.installed}
            {#if !e.custom && e.installed !== e.version}
              <Button size="sm" variant="ice" iconLeft="rotate-ccw" onclick={() => pick(e)}>Update to v{e.version}</Button>
            {/if}
            {#if e.kind === 'workflow'}
              <Badge tone="success" dot>Added</Badge>
            {:else}
              <Button size="sm" variant="secondary" loading={busy === e.id} onclick={() => remove(e)}>Remove</Button>
            {/if}
          {:else}
            <div class="catalog-actions">
              <Button size="sm" variant="ice" iconLeft="download" onclick={() => pick(e)}>{e.kind === 'workflow' ? 'Add' : 'Install'}</Button>
              <IconButton icon="eye-off" label="Dismiss {e.id}" variant="ghost" size="sm" onclick={() => dismiss(e)} />
            </div>
          {/if}
        </div>
      </article>
    {:else}
      <div class="sun-empty">{empty}</div>
    {/each}
  {/if}
</div>

{#if adding_mcp}
  <McpServerDialog onclose={() => (adding_mcp = false)} onadded={mcp_added} />
{/if}

{#if picked}
  {@const e = picked}
  <Dialog open onclose={() => (picked = null)} title="{e.installed ? 'Update' : 'Install'} {e.id}" size="lg">
    <div class="catalog-dialog">
      {#if !e.reviewed}
        <div class="catalog-warning"><Icon name="triangle-alert" size={16} />Not on the curated list. Read every file before installing.</div>
      {/if}
      {#if e.kind === 'skill'}
        <fieldset class="catalog-mods">
          <legend class="catalog-file">Install into</legend>
          <Radio bind:group={scope} name="scope" value="ecosystem" label="Whole ecosystem" description={WHERE.skill} />
          <Radio
            bind:group={scope}
            name="scope"
            value="repo"
            label="A repo, by pull request"
            description="Sun commits it to the repo’s skills folder on sun/skill-{e.id}. Everyone working in the repo gets it once you merge."
          />
        </fieldset>
        {#if scope === 'repo'}
          <Select
            label="Repo"
            placeholder="Pick a repo"
            bind:value={repo_pick}
            options={repos.map((r) => ({ value: r.id, label: repo_name(r.url) }))}
            hint={repos.length ? undefined : 'No repos yet. Add one when you create a task.'}
          />
          <p class="catalog-pr" role="status">
            {#if skill_pr?.pr}
              {@const url = safe_url(skill_pr.pr.url)}
              <Icon name="git-pull-request" size={16} />
              {#if url}
                <a href={url} target="_blank" rel="noopener noreferrer">Review the PR for {skill_pr.branch}</a>
              {:else}
                {PR_STATE[skill_pr.pr.status]}
              {/if}
              {#if skill_pr.version !== e.version}<span class="catalog-version ml-mono">carries v{skill_pr.version}</span>{/if}
            {/if}
          </p>
        {/if}
      {:else}
        <p class="catalog-desc">{WHERE[e.kind]}</p>
      {/if}
      {#if e.kind === 'skill'}
        <fieldset class="catalog-mods" aria-describedby="catalog-runtimes-hint">
          <legend class="catalog-file">Agents that get it</legend>
          {#each SKILL_RUNTIMES as r (r.id)}
            <Checkbox
              label={r.label}
              checked={runtime_picks.includes(r.id)}
              onchange={() =>
                (runtime_picks = runtime_picks.includes(r.id) ? runtime_picks.filter((x) => x !== r.id) : [...runtime_picks, r.id])}
            />
            <p class="sun-row__meta ml-mono catalog-asks">{r.folder}</p>
          {/each}
          <p id="catalog-runtimes-hint" class="sun-row__meta" class:catalog-error={!runtime_picks.length} role={runtime_picks.length ? undefined : 'alert'}>
            {!runtime_picks.length
              ? 'Pick at least one agent.'
              : scope === 'repo'
                ? 'Committed once to the folders these agents read.'
                : 'Sessions on the other agents won’t see it.'}
          </p>
        </fieldset>
      {/if}
      {#if e.secrets.length}
        <fieldset class="catalog-mods">
          <legend class="catalog-file">Secrets it needs</legend>
          {#each e.secrets as name (name)}
            <Input
              label={name}
              type="password"
              autocomplete="off"
              hint={stored_secrets.includes(name)
                ? 'Already set for the ecosystem. Leave empty to keep it.'
                : 'Write-only, ecosystem-wide. Leave empty to add it later in Settings.'}
              bind:value={secret_values[name]}
            />
          {/each}
        </fieldset>
      {/if}
      {#if missing_mods.length}
        <fieldset class="catalog-mods">
          <legend class="catalog-file">Mods this workflow uses</legend>
          <p class="sun-row__meta">Not installed yet. Tick the ones to install with it; the workflow can’t run its actions without them.</p>
          {#each missing_mods as m (m)}
            <Checkbox
              label="Install the {m} mod"
              checked={mod_picks.includes(m)}
              onchange={() => (mod_picks = mod_picks.includes(m) ? mod_picks.filter((x) => x !== m) : [...mod_picks, m])}
            />
            <p class="sun-row__meta ml-mono catalog-asks">Asks for: {asks_of(m).join(', ') || 'nothing'}</p>
          {/each}
        </fieldset>
      {/if}
      {#if files}
        {@const changed = files.filter((f) => f.change && f.change !== 'unchanged')}
        {#if e.installed && files.some((f) => f.change)}
          <p class="catalog-desc">
            {changed.length
              ? `${changed.length} file${changed.length > 1 ? 's' : ''} changed since v${e.installed}. Unchanged files are folded.`
              : `No file changed since v${e.installed}.`}
          </p>
        {/if}
        {#each files as f (f.path)}
          <section>
            <h3 class="catalog-file ml-mono">
              {f.path}
              {#if f.change && f.change !== 'unchanged'}<Badge tone={CHANGE_TONE[f.change]}>{CHANGE_LABEL[f.change]}</Badge>{/if}
            </h3>
            {#if f.change === 'removed'}
              <p class="sun-row__meta">Removed in v{e.version}.</p>
            {:else if f.change === 'unchanged' && changed.length}
              <p class="sun-row__meta">Unchanged.</p>
            {:else if f.diff}
              <!-- Each line keeps its ' ', '-' or '+' so the change never depends on colour alone. -->
              <pre class="catalog-pre catalog-diff ml-mono" aria-label="Changes to {f.path}: lines starting with - are removed, + are added">{#each f.diff as line, i (i)}<span data-op={line[0]}>{line}</span>{/each}</pre>
            {:else}
              <pre class="catalog-pre ml-mono">{f.text}</pre>
            {/if}
          </section>
        {/each}
      {:else}
        <div class="sun-empty" role="status">Loading files…</div>
      {/if}
    </div>
    {#snippet footer()}
      {@const by_pr = e.kind === 'skill' && scope === 'repo'}
      {@const asked = by_pr && !!skill_pr?.pr && skill_pr.version === e.version}
      <Button variant="secondary" onclick={() => (picked = null)}>{asked ? 'Done' : 'Cancel'}</Button>
      <Button
        variant="ice"
        iconLeft={by_pr ? 'git-pull-request' : undefined}
        disabled={!files || (e.kind === 'skill' && !runtime_picks.length) || (by_pr && (!repo_pick || asked))}
        loading={busy === e.id}
        onclick={install}
      >
        {by_pr
          ? asked
            ? 'PR requested'
            : skill_pr?.pr
              ? `Update the PR to v${e.version}`
              : 'Open PR'
          : e.installed
            ? `Update to v${e.version}`
            : e.kind === 'workflow'
              ? `Add workflow${mod_picks.length ? ` + ${mod_picks.length} mod${mod_picks.length > 1 ? 's' : ''}` : ''}`
              : 'Install'}
      </Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .catalog-repo {
    display: flex;
    align-items: flex-start;
    gap: var(--space-2);
    flex-wrap: wrap;
  }
  .catalog-repo :global(.ml-field) {
    flex: 1 1 260px;
  }
  .catalog-repo :global(button) {
    margin-top: 26px;
  }
  .catalog-bar {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    flex-wrap: wrap;
  }
  .catalog-filters {
    display: flex;
    gap: var(--space-2);
    flex-wrap: wrap;
  }
  .catalog-filters :global(.ml-field) {
    flex: 1 1 140px;
    max-width: 240px;
    min-width: 0;
  }
  .catalog-diff span {
    display: block;
    padding: 0 6px;
    margin: 0 -6px;
  }
  .catalog-diff span[data-op='-'] {
    background: color-mix(in srgb, var(--red-400) 16%, transparent);
    color: var(--red-400);
  }
  .catalog-diff span[data-op='+'] {
    background: color-mix(in srgb, var(--mint-300) 14%, transparent);
    color: var(--mint-300);
  }
  .catalog-version {
    font-size: 11px;
    color: var(--fg-3);
  }
  .catalog-desc {
    font-size: 14px;
    color: var(--fg-1);
    margin: 4px 0 6px;
    overflow-wrap: anywhere;
  }
  .catalog-why {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--ice-300);
    margin-top: 6px;
  }
  .catalog-actions {
    display: flex;
    align-items: center;
    gap: var(--space-1);
  }
  .catalog-dialog {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .catalog-warning {
    display: flex;
    gap: 10px;
    padding: 12px;
    border-radius: var(--radius-md);
    background: var(--amber-100);
    color: var(--amber-700);
    font-size: 13px;
    font-weight: 500;
  }
  .catalog-file {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-2);
    font-size: 13px;
    font-weight: 600;
    margin: 0 0 6px;
  }
  .catalog-mods {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    border: 0;
    margin: 0;
    padding: 0;
    min-width: 0;
  }
  .catalog-pr {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px;
    margin: 0;
    font-size: 13px;
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .catalog-pr:empty {
    display: none;
  }
  .catalog-pr a {
    color: var(--fg-1);
    min-height: 44px;
    display: inline-flex;
    align-items: center;
  }
  .catalog-error {
    color: var(--danger-fg);
  }
  .catalog-asks {
    margin: -4px 0 0 28px;
    overflow-wrap: anywhere;
  }
  .catalog-pre {
    margin: 0;
    padding: 12px 14px;
    border-radius: var(--radius-md);
    background: var(--slate-950);
    color: var(--slate-100);
    font-size: 12px;
    line-height: 1.55;
    white-space: pre-wrap;
    max-height: 240px;
    overflow: auto;
  }
</style>
