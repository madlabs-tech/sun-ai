<!-- Knowledge: the current workspace's sources agents cite, uploaded or synced by a connector mod,
     with what the last sync skipped or why it failed; search (this workspace only) with citations; a
     source's docs and a doc's chunks live at #/knowledge/<source>/<doc> (KnowledgeDocs). Admins import
     another workspace's source. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import ImportPicker from '../lib/ui/ImportPicker.svelte';
  import KnowledgeDocs from './KnowledgeDocs.svelte';
  import { api } from '../lib/api/client';
  import type { KnowledgeKind, KnowledgeSource, SearchHit, SourceSettings, SyncReport, UploadDoc, UploadFile } from '../lib/api/types';
  import { workspace_of, can, current_workspace, error_text, toast } from '../lib/app_state.svelte';
  import { cited, day, whole } from '../lib/format';
  import { href, route } from '../lib/router.svelte';

  type Field = { key: 'folder' | 'space' | 'project' | 'channel' | 'path'; label: string; hint: string; placeholder: string; required: boolean };
  const KINDS: Record<KnowledgeKind, { name: string; icon: string; field?: Field }> = {
    upload: { name: 'Uploads', icon: 'upload' },
    repo: { name: 'Repo folder', icon: 'git-branch', field: { key: 'path', label: 'Folder path', placeholder: '/srv/checkouts/handbook', hint: 'An absolute folder on the kernel’s machine. Its .md files are indexed.', required: true } },
    gdrive: { name: 'Google Drive', icon: 'folder', field: { key: 'folder', label: 'Folder id (optional)', placeholder: '1AbC…', hint: 'The part after /folders/ in the folder’s URL. Empty = every file the connector can read.', required: false } },
    confluence: { name: 'Confluence', icon: 'book-open', field: { key: 'space', label: 'Space key', placeholder: 'ENG', hint: 'The short key in the space’s URL, after /spaces/.', required: true } },
    jira: { name: 'Jira', icon: 'ticket', field: { key: 'project', label: 'Project key', placeholder: 'MAD', hint: 'The prefix of its issue keys, e.g. MAD in MAD-88.', required: true } },
    slack: { name: 'Slack', icon: 'slack', field: { key: 'channel', label: 'Channel id', placeholder: 'C0123ABCD', hint: 'In the channel’s details, at the bottom. Not its #name.', required: true } },
  };
  const KIND_HINT: Partial<Record<KnowledgeKind, string>> = {
    upload: 'Markdown, text, Excel, Word and PDF files you upload from this page.',
    repo: 'Re-read every few minutes; removed files leave the index.',
  };
  const KIND_OPTIONS = (Object.keys(KINDS) as KnowledgeKind[]).map((k) => ({ value: k, label: k === 'upload' ? 'Upload files' : KINDS[k].name }));
  const MAX_AGE_DAYS = 36_500;
  /** Same rule as the kernel: https, or http to localhost. */
  const WEBHOOK = /^(https:\/\/\S+|http:\/\/(localhost|127\.0\.0\.1|\[::1\])[:/]\S*)$/;
  const is_connector = (kind: KnowledgeKind) => kind !== 'upload' && kind !== 'repo';

  let load = $state<Promise<KnowledgeSource[]>>(new Promise(() => {}));
  const reload = () => (load = api.list_sources(current_workspace() || undefined));
  // Each workspace has its own sources: switching workspaces reloads them and clears the last search.
  $effect(() => {
    current_workspace();
    reload();
    hits = null;
  });
  const workspace_name = $derived(workspace_of(current_workspace())?.name ?? 'this workspace');

  // Admin import: sources on other workspaces, not yet on this one.
  let importing = $state(false);
  let importable = $state<{ value: string; label: string }[]>([]);
  async function open_import() {
    importing = true;
    try {
      const [all, here] = await Promise.all([api.list_sources(), load]);
      importable = all.filter((s) => !here.some((h) => h.id === s.id)).map((s) => ({ value: s.id, label: s.name }));
    } catch (err) {
      importing = false;
      toast('danger', 'Couldn’t load the other workspaces’ sources', error_text(err));
    }
  }
  async function import_source(id: string) {
    await api.import_source(current_workspace(), id);
    importing = false;
    reload();
  }

  // Search
  let query = $state('');
  let hits = $state<SearchHit[] | null>(null);
  let searched = $state('');
  let searching = $state(false);

  async function search(e: SubmitEvent) {
    e.preventDefault();
    const q = query.trim();
    if (!q) return;
    searching = true;
    try {
      hits = await api.search(q, current_workspace() || undefined);
      searched = q;
    } catch (err) {
      toast('danger', 'Search didn’t work', error_text(err));
    }
    searching = false;
  }

  // Add source
  let adding = $state(false);
  let saving = $state(false);
  let form = $state({ kind: 'confluence' as KnowledgeKind, name: '', param: '', max_age: '', webhook: '' });
  let errors = $state<{ name?: string; param?: string; max_age?: string; webhook?: string; form?: string }>({});
  const field = $derived(KINDS[form.kind].field);

  function open_add() {
    form = { kind: 'confluence', name: '', param: '', max_age: '', webhook: '' };
    errors = {};
    adding = true;
  }

  async function add(e: SubmitEvent) {
    e.preventDefault();
    const name = form.name.trim();
    const param = form.param.trim();
    const max_age = whole(form.max_age);
    const webhook = is_connector(form.kind) ? form.webhook.trim() : '';
    errors = {
      name: name ? undefined : 'Give the source a name.',
      param: field?.required && !param ? `Enter the ${field.label.toLowerCase()}.` : undefined,
      max_age: Number.isNaN(max_age) || (max_age ?? 0) > MAX_AGE_DAYS ? 'Use a whole number of days.' : undefined,
      webhook: webhook && !WEBHOOK.test(webhook) ? 'Use an https URL, or http to localhost.' : undefined,
    };
    if (errors.name || errors.param || errors.max_age || errors.webhook) return;
    const settings: SourceSettings = {};
    if (field && param) settings[field.key] = param;
    if (max_age) settings.max_age_days = max_age;
    if (webhook) settings.export_webhook = webhook;
    saving = true;
    try {
      await api.add_source({ kind: form.kind, name, settings, workspace_id: current_workspace() || undefined });
      adding = false;
      toast('success', `${name} added`, form.kind === 'upload' ? 'Upload files to index them.' : 'It syncs within a few minutes, or press Re-index.');
      reload();
    } catch (err) {
      errors = { form: error_text(err) };
    }
    saving = false;
  }

  // Row actions
  let busy = $state('');
  let deleting = $state<KnowledgeSource | null>(null);
  let file_input = $state<HTMLInputElement>();
  let upload_to = $state<KnowledgeSource | null>(null);

  function report_text(r: SyncReport) {
    return `${r.indexed} indexed · ${r.unchanged} unchanged · ${r.retired} removed` + (r.skipped.length ? ` · not indexed: ${r.skipped.join('; ')}` : '');
  }

  async function run(s: KnowledgeSource, work: () => Promise<SyncReport>, title: string) {
    busy = s.id;
    try {
      const r = await work();
      toast(r.skipped.length ? 'warning' : 'success', title, report_text(r));
    } catch (err) {
      toast('danger', `${s.name} not indexed`, error_text(err));
    }
    busy = '';
    reload();
  }

  const reindex = (s: KnowledgeSource) => run(s, () => api.reindex_source(s.id), `${s.name} re-indexed`);

  function pick_files(s: KnowledgeSource) {
    upload_to = s;
    file_input?.click();
  }

  async function to_doc(f: File): Promise<UploadDoc> {
    const markdown = await f.text();
    const title = markdown.match(/^# +(.+)$/m)?.[1].trim() || f.name;
    return { external_id: f.name, title, version: String(f.lastModified), updated_at: new Date(f.lastModified).toISOString(), content: { markdown } };
  }

  /** Excel, Word and PDF go up as base64 `files`; the kernel extracts their text. */
  const BINARY = /\.(xlsx|docx|pdf)$/i;
  const MAX_FILE_BYTES = 8 * 1024 * 1024;

  function to_file(f: File): Promise<UploadFile> {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve({
        external_id: f.name, title: f.name.replace(/\.[^.]+$/, ''), version: String(f.lastModified),
        updated_at: new Date(f.lastModified).toISOString(), content_base64: String(reader.result).split(',')[1] ?? '',
      });
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(f);
    });
  }

  async function send(s: KnowledgeSource, files: File[]): Promise<SyncReport> {
    const too_big = files.filter((f) => BINARY.test(f.name) && f.size > MAX_FILE_BYTES);
    const binary = files.filter((f) => BINARY.test(f.name) && f.size <= MAX_FILE_BYTES);
    const text = files.filter((f) => !BINARY.test(f.name));
    const report = text.length || binary.length
      ? await api.upload_docs(s.id, { docs: await Promise.all(text.map(to_doc)), files: await Promise.all(binary.map(to_file)) })
      : { indexed: 0, unchanged: 0, skipped: [], retired: 0 };
    return { ...report, skipped: [...report.skipped, ...too_big.map((f) => `${f.name}: larger than 8 MB`)] };
  }

  async function upload(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const files = [...(input.files ?? [])];
    const s = upload_to;
    input.value = '';
    if (!s || !files.length) return;
    await run(s, () => send(s, files), `${files.length} ${files.length === 1 ? 'file' : 'files'} uploaded to ${s.name}`);
  }

  async function remove(s: KnowledgeSource) {
    busy = s.id;
    try {
      await api.delete_source(s.id, current_workspace() || undefined);
      toast('success', `${s.name} removed from ${workspace_name}`);
    } catch (err) {
      toast('danger', `${s.name} not removed`, error_text(err));
    }
    deleting = null;
    busy = '';
    reload();
  }

  const param_of = (s: KnowledgeSource) => {
    const f = KINDS[s.kind].field;
    return f && s.settings[f.key] ? `${f.label.replace(' (optional)', '').toLowerCase()} ${s.settings[f.key]}` : '';
  };
</script>

{#if route.id}
  <KnowledgeDocs source_id={route.id} doc_id={route.sub} />
{:else}
  <PageHeader title="Knowledge" subtitle="Docs agents on {workspace_name} can read and cite, synced from your tools or uploaded.">
    {#if can('manage_settings')}<Button variant="secondary" iconLeft="download" onclick={open_import}>Import</Button>{/if}
    <Button variant="ice" iconLeft="plus" onclick={open_add}>Add source</Button>
  </PageHeader>

  <div class="sun-page">
    <form class="search" role="search" onsubmit={search}>
      <Input iconLeft="search" aria-label="Search knowledge" placeholder="Search this workspace’s sources, e.g. session timeout" bind:value={query} style="flex:1;min-width:0" />
      <Button type="submit" variant="secondary" loading={searching}>Search</Button>
    </form>

    {#if hits}
      <section aria-labelledby="hits-head">
        <h2 class="sun-label" id="hits-head">{hits.length} {hits.length === 1 ? 'match' : 'matches'} for “{searched}”</h2>
        <ul class="hits">
          {#each hits as h, i (i)}
            {@const c = cited(h.citation)}
            <li class="hit">
              <div class="hit__cite ml-mono">
                <Icon name="file-text" size={12} />
                {#if c.url}<a href={c.url} target="_blank" rel="noopener noreferrer">{c.text}</a>{:else}{c.text}{/if}
                {#each h.flags as f, j (j)}<Badge tone={f.kind === 'stale' ? 'warning' : 'danger'}>{f.kind === 'stale' ? 'Stale' : 'Conflict'}</Badge>{/each}
              </div>
              <p class="hit__text">{h.text}</p>
            </li>
          {:else}
            <li class="sun-empty">No matches. Try other words, or check the source is indexed.</li>
          {/each}
        </ul>
      </section>
    {/if}

    <h2 class="sun-label" style="margin:var(--space-2) 0 0">Sources</h2>
    <Loaded {load} empty="No sources yet. Add one to give agents your docs.">
      {#snippet row(s: KnowledgeSource)}
        {@const kind = KINDS[s.kind]}
        {@const skipped = s.last_sync?.report?.skipped ?? []}
        <article class="sun-row source" aria-label={s.name}>
          <span class="sun-tile"><Icon name={kind.icon} size={18} /></span>
          <div class="sun-row__body">
            <div class="sun-row__title">
              <a class="source__name" href={href('knowledge', s.id)}>{s.name}</a>
              {#if s.last_sync?.error}<Badge tone="danger" dot>Sync failed</Badge>{/if}
              {#if skipped.length}<Badge tone="warning">{skipped.length} {skipped.length === 1 ? 'file' : 'files'} not indexed</Badge>{/if}
            </div>
            <div class="sun-row__meta">
              {[kind.name, param_of(s)].filter(Boolean).join(' · ')} ·
              <span class="ml-mono">{s.docs}</span> docs · <span class="ml-mono">{s.chunks}</span> chunks
              {#if s.last_indexed_at} · indexed <span class="ml-mono">{day(s.last_indexed_at)}</span>{/if}
            </div>
            {#if s.last_sync?.error}<p class="source__problem source__problem--error">{s.last_sync.error}</p>{/if}
            {#if skipped.length}
              <ul class="source__problem" aria-label="Not indexed from {s.name}">
                {#each skipped as reason, i (i)}<li class="ml-mono">{reason}</li>{/each}
              </ul>
            {/if}
          </div>
          <div class="source__actions">
            {#if s.kind === 'upload'}
              <Button size="sm" variant="secondary" iconLeft="upload" loading={busy === s.id} onclick={() => pick_files(s)}>Upload</Button>
            {:else}
              <Button size="sm" variant="secondary" iconLeft="refresh-cw" loading={busy === s.id} onclick={() => reindex(s)}>Re-index</Button>
            {/if}
            <IconButton icon="trash-2" label="Remove {s.name}" variant="ghost" size="sm" onclick={() => (deleting = s)} />
          </div>
        </article>
      {/snippet}
    </Loaded>
  </div>

  <input bind:this={file_input} type="file" accept=".md,.markdown,.txt,.xlsx,.docx,.pdf" multiple hidden aria-label="Files to upload" onchange={upload} />
{/if}

<Dialog bind:open={adding} onclose={() => (adding = false)} title="Add a knowledge source">
  <form id="add-source" class="add" onsubmit={add} novalidate>
    <Select label="Kind" bind:value={form.kind} options={KIND_OPTIONS} hint={KIND_HINT[form.kind] ?? 'Synced by its mod every few minutes.'} />
    <Input label="Name" placeholder="e.g. Engineering wiki" bind:value={form.name} error={errors.name} />
    {#if field}
      <Input label={field.label} mono placeholder={field.placeholder} hint={field.hint} bind:value={form.param} error={errors.param} />
    {/if}
    {#if is_connector(form.kind)}
      <Input label="Export webhook (optional)" type="url" mono placeholder="https://example.com/sun-sync" hint="Each synced page is also sent here as JSON." bind:value={form.webhook} error={errors.webhook} />
    {/if}
    <Input label="Flag docs older than (days)" type="number" inputmode="numeric" min="1" placeholder="180" hint="Optional. Agents are warned before using older docs." bind:value={form.max_age} error={errors.max_age} />
    {#if errors.form}<p class="add__error" role="alert">{errors.form}</p>{/if}
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (adding = false)}>Cancel</Button>
    <Button variant="ice" type="submit" form="add-source" loading={saving}>Add source</Button>
  {/snippet}
</Dialog>

{#if deleting}
  {@const s = deleting}
  <Dialog open onclose={() => (deleting = null)} title="Remove {s.name} from {workspace_name}?">
    <p style="margin:0;color:var(--fg-2);line-height:1.5">
      Agents on this workspace stop citing it. If no other workspace has it, its {s.docs} docs and {s.chunks} chunks leave the
      index for good.
    </p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (deleting = null)}>Keep it</Button>
      <Button variant="danger" loading={busy === s.id} onclick={() => remove(s)}>Remove source</Button>
    {/snippet}
  </Dialog>
{/if}

<Dialog bind:open={importing} onclose={() => (importing = false)} title="Import a source into {workspace_name}">
  {#if importing}<ImportPicker label="Source from another workspace" options={importable} on_import={import_source} />{/if}
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (importing = false)}>Done</Button>
  {/snippet}
</Dialog>

<style>
  .search {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .hits {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .hit {
    padding: var(--space-3) var(--space-4);
    border-radius: var(--radius-lg);
    background: var(--surface-card);
    border: 1.5px solid var(--border-subtle);
  }
  .hit__cite {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .hit__cite a {
    color: inherit;
    text-decoration: underline;
    text-underline-offset: 2px;
  }
  .hit__text {
    margin: 6px 0 0;
    font-size: 14px;
    line-height: 1.5;
    color: var(--fg-1);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .source {
    align-items: center;
  }
  .source__name {
    display: inline-flex;
    align-items: center;
    min-height: 32px;
    color: var(--fg-1);
    text-decoration: underline;
    text-decoration-color: var(--border-default);
    text-underline-offset: 3px;
  }
  .source__name:hover {
    text-decoration-color: currentColor;
  }
  .source__problem {
    margin: 6px 0 0;
    font-size: 12px;
    line-height: 1.45;
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .source__problem--error {
    color: var(--danger-fg);
  }
  .source__actions {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    margin-left: auto;
  }
  .add {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .add__error {
    margin: 0;
    font-size: 13px;
    color: var(--danger-fg);
  }
  /* Touch: the source name is a 44 px target too. */
  @media (max-width: 1023px) {
    .source__name {
      min-height: 44px;
    }
  }
</style>
