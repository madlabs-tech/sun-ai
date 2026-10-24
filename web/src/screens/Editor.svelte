<!-- Repo editor (#/editor/<repo_id>): the caller's worktree of a workspace repo on one of its online hosts.
     The file tree loads one level per expand; files open in CodeMirror tabs (a dot marks unsaved ones) and
     ⌘S / Ctrl+S saves. A save sends the hash the file was read with, so a file changed on disk since asks to
     reload or overwrite. Files over 2 MB and binary files show a notice instead. With more than one online
     host a picker shows; the last host per repo is remembered. Without `editor.write` (`GET …/editor/me`)
     it is a viewer: no save, create, rename or delete, and a Read-only badge. With `terminal`, Ctrl+` (or the
     Terminal button) toggles the bottom terminal panel. ⌘/Ctrl+P opens quick open, ⌘/Ctrl+Shift+P its commands. -->
<script lang="ts">
  import { SvelteSet } from 'svelte/reactivity';
  import { Badge, Button, Dialog, Icon, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import type { EditorState, Extension, Text } from '@codemirror/state';
  import { EditorView } from '@codemirror/view';
  import IconButton from '../lib/ui/IconButton.svelte';
  import Menu, { type MenuItem } from '../lib/ui/Menu.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { ApiFailure, api, type EditorAt } from '../lib/api/client';
  import type { FsEntry, FsFile, Host, Repo } from '../lib/api/types';
  import { current_workspace, error_text, local_get, local_set, toast } from '../lib/app_state.svelte';
  import { repo_name } from '../lib/format';
  import { href } from '../lib/router.svelte';
  import { editor_state, language_of } from '../lib/code_editor';
  import EditorTerminal from './EditorTerminal.svelte';
  import QuickOpen, { type Command } from './QuickOpen.svelte';

  let { repo_id }: { repo_id: string } = $props();

  interface Tab {
    path: string;
    file: FsFile;
    /** `null` for a file shown as a notice (too large or binary). The active tab's live state is the view's. */
    state: EditorState | null;
    saved: Text | null;
    dirty: boolean;
  }

  const workspace = current_workspace();
  const host_key = $derived(`sun_editor_host:${repo_id}`);
  const join = (dir: string, name: string) => (dir ? `${dir}/${name}` : name);
  const parent_of = (path: string) => (path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '');
  const base_name = (path: string) => path.slice(path.lastIndexOf('/') + 1);
  const under = (path: string, root: string) => path === root || path.startsWith(root + '/');

  let repo = $state<Repo>();
  let hosts = $state<Host[]>([]);
  let host = $state('');
  let can_write = $state(false);
  let can_terminal = $state(false);
  let load_error = $state('');
  let opened = $state<Promise<void>>();
  let dirs = $state<Record<string, { entries: FsEntry[]; truncated: boolean }>>({});
  const expanded = new SvelteSet<string>();
  let tabs = $state<Tab[]>([]);
  let active = $state('');
  const current = $derived(tabs.find((t) => t.path === active));
  const unsaved = $derived(tabs.some((t) => t.dirty));
  let view: EditorView | undefined;
  const at = (): EditorAt => ({ workspace, repo: repo_id, host });

  async function load() {
    try {
      const [repos, all, me] = await Promise.all([api.list_repos(workspace), api.list_hosts(workspace), api.editor_me(workspace, repo_id)]);
      repo = repos.find((r) => r.id === repo_id);
      if (!repo) return void (load_error = 'This repository isn’t on the workspace. Pick one in Settings → Code.');
      if (!me.capabilities.includes('editor.read'))
        return void (load_error = 'You don’t have access to this repository’s editor. Ask a workspace owner or admin to grant it.');
      hosts = all.filter((h) => h.status === 'online');
      can_write = me.capabilities.includes('editor.write');
      can_terminal = me.capabilities.includes('terminal');
      const remembered = local_get(host_key);
      pick_host(hosts.find((h) => h.id === remembered)?.id ?? hosts[0]?.id ?? '');
    } catch (e) {
      load_error = error_text(e);
    }
  }
  load();

  function pick_host(id: string) {
    host = id;
    tabs = [];
    active = '';
    dirs = {};
    expanded.clear();
    if (!id) return;
    local_set(host_key, id);
    opened = api.editor_open(at()).then(() => load_dir(''));
  }

  async function load_dir(path: string) {
    try {
      dirs[path] = await api.editor_tree(at(), path);
    } catch (e) {
      toast('danger', `Couldn’t open ${path || 'the repository'}`, error_text(e));
    }
  }

  function toggle(path: string) {
    if (expanded.delete(path)) return;
    expanded.add(path);
    load_dir(path);
  }

  const fetch_file = (path: string) => Promise.all([api.editor_read(at(), path), language_of(path)]);

  function set_text(tab: Tab, content: string, language: Extension) {
    tab.state = editor_state(content, language, !can_write, (doc) => (tab.dirty = !doc.eq(tab.saved as Text)));
    tab.saved = tab.state.doc;
    tab.dirty = false;
  }

  async function open_file(path: string) {
    if (tabs.some((t) => t.path === path)) return show(path);
    try {
      const [file, language] = await fetch_file(path);
      if (tabs.some((t) => t.path === path)) return show(path);
      tabs.push({ path, file, state: null, saved: null, dirty: false });
      const tab = tabs[tabs.length - 1];
      if (file.content !== null) set_text(tab, file.content, language);
      show(path);
    } catch (e) {
      toast('danger', `Couldn’t open ${path}`, error_text(e));
    }
  }

  function show(path: string) {
    if (view && current?.state) current.state = view.state;
    active = path;
    const next = tabs.find((t) => t.path === path);
    if (view && next?.state) {
      view.setState(next.state);
      view.focus();
    }
  }

  let closing = $state<Tab | null>(null);
  function close(tab: Tab, discard = false) {
    if (tab.dirty && !discard) return void (closing = tab);
    closing = null;
    const i = tabs.indexOf(tab);
    tabs.splice(i, 1);
    if (tab.path !== active) return;
    active = '';
    const next = tabs[Math.min(i, tabs.length - 1)];
    if (next) show(next.path);
  }

  const doc_of = (tab: Tab) => (tab.path === active && view ? view.state.doc : (tab.state as EditorState).doc);

  /** The path of the tab whose save hit a file changed on disk. */
  let conflict = $state('');
  async function save(tab: Tab, overwrite = false) {
    if (!can_write || !tab.state) return;
    const doc = doc_of(tab);
    try {
      const stat = await api.editor_write(at(), tab.path, doc.toString(), overwrite ? undefined : (tab.file.hash ?? undefined));
      Object.assign(tab.file, stat);
      tab.saved = doc;
      tab.dirty = !doc_of(tab).eq(doc);
      conflict = '';
    } catch (e) {
      if (e instanceof ApiFailure && e.code === 'conflict') conflict = tab.path;
      else toast('danger', `Couldn’t save ${tab.path}`, error_text(e));
    }
  }

  async function reload(tab: Tab) {
    conflict = '';
    try {
      const [file, language] = await fetch_file(tab.path);
      tab.file = file;
      if (file.content === null) tab.state = null;
      else set_text(tab, file.content, language);
      if (tab.path === active && tab.state) view?.setState(tab.state);
    } catch (e) {
      toast('danger', `Couldn’t reload ${tab.path}`, error_text(e));
    }
  }

  /** The editor's own keys; the terminal hands them over too. */
  function shortcut_of(e: KeyboardEvent): 'save' | 'files' | 'commands' | 'terminal' | null {
    if (e.altKey) return null;
    if (e.ctrlKey && !e.metaKey && e.code === 'Backquote') return 'terminal';
    if (!(e.metaKey || e.ctrlKey)) return null;
    const key = e.key.toLowerCase();
    return key === 's' ? 'save' : key === 'p' ? (e.shiftKey ? 'commands' : 'files') : null;
  }

  function keydown(e: KeyboardEvent) {
    const action = shortcut_of(e);
    if (!action) return;
    // Stops the browser's print dialog for ⌘/Ctrl+P (Firefox keeps ⌘/Ctrl+Shift+P whatever a page does).
    e.preventDefault();
    if (action === 'save') {
      if (current) save(current);
    } else if (action === 'terminal') toggle_terminal();
    else if (host) quick_open(action === 'commands' ? '>' : '');
  }

  let terminal = $state<EditorTerminal>();
  const toggle_terminal = () => can_terminal && terminal?.toggle();

  /** Quick open's starting text while it shows. */
  let palette = $state<string | null>(null);
  let index = $state<Promise<string[]>>(Promise.resolve([]));
  function quick_open(query: string) {
    // Every time: the terminal may have made or removed files since.
    index = api.editor_index(at()).then((r) => r.paths);
    palette = query;
  }
  const commands = $derived<Command[]>([
    ...(can_write ? [{ label: 'New file', run: () => new_entry('file', parent_of(active)) }] : []),
    ...(can_terminal ? [{ label: 'Toggle terminal', run: toggle_terminal }] : []),
    ...(can_write && current?.state ? [{ label: 'Save', run: () => current && save(current) }] : []),
  ]);

  function beforeunload(e: BeforeUnloadEvent) {
    if (unsaved) e.preventDefault();
  }

  function mount(element: HTMLElement) {
    view = new EditorView({ parent: element });
    return () => view?.destroy();
  }

  // Create, rename and delete.
  let naming = $state<{ kind: 'file' | 'dir' | 'rename'; dir: string; path: string; name: string; error: string } | null>(null);
  let removing = $state<{ path: string; dir: boolean } | null>(null);
  let busy = $state(false);
  const new_entry = (kind: 'file' | 'dir', dir: string) => (naming = { kind, dir, path: '', name: '', error: '' });

  function menu(path: string, entry: FsEntry): (MenuItem | null)[] {
    const dir = entry.kind === 'dir' ? path : parent_of(path);
    return [
      { label: 'New file', icon: 'file-plus', onselect: () => new_entry('file', dir) },
      { label: 'New folder', icon: 'folder-plus', onselect: () => new_entry('dir', dir) },
      null,
      { label: 'Rename', icon: 'pencil', onselect: () => (naming = { kind: 'rename', dir: parent_of(path), path, name: entry.name, error: '' }) },
      { label: 'Delete', icon: 'trash-2', danger: true, onselect: () => (removing = { path, dir: entry.kind === 'dir' }) },
    ];
  }

  /** Right-click opens the row's own actions menu. */
  function context_menu(e: MouseEvent) {
    const trigger = (e.currentTarget as HTMLElement).querySelector<HTMLElement>('[popovertarget]');
    if (!trigger) return;
    e.preventDefault();
    // Opened while the button is still down (Linux, macOS), the popover's light dismiss closes it on pointerup.
    if (e.buttons) addEventListener('pointerup', () => setTimeout(() => trigger.click()), { once: true });
    else trigger.click();
  }

  /** Points open tabs at their new path after a rename of them or a folder above them. */
  function moved(from: string, to: string) {
    for (const tab of tabs) {
      if (!under(tab.path, from)) continue;
      const path = to + tab.path.slice(from.length);
      if (tab.path === active) active = path;
      tab.path = path;
    }
    for (const path of [...expanded]) if (under(path, from)) expanded.delete(path);
  }

  async function submit_name(e: SubmitEvent) {
    e.preventDefault();
    if (!naming) return;
    const { kind, dir, path } = naming;
    const name = naming.name.trim();
    if (!name) return void (naming.error = 'Enter a name.');
    const target = join(dir, name);
    busy = true;
    try {
      if (kind === 'rename') {
        await api.editor_rename(at(), path, target);
        moved(path, target);
      } else {
        await api.editor_create(at(), target, kind);
      }
      naming = null;
      if (dir) expanded.add(dir);
      await load_dir(dir);
      if (kind === 'file') open_file(target);
    } catch (e) {
      if (naming) naming.error = error_text(e);
    } finally {
      busy = false;
    }
  }

  async function remove(path: string) {
    busy = true;
    try {
      await api.editor_delete(at(), path);
      removing = null;
      for (const tab of tabs.filter((t) => under(t.path, path))) close(tab, true);
      await load_dir(parent_of(path));
    } catch (e) {
      toast('danger', `Couldn’t delete ${path}`, error_text(e));
    } finally {
      busy = false;
    }
  }
</script>

<svelte:window onkeydown={keydown} onbeforeunload={beforeunload} />

<PageHeader title="Editor" subtitle={repo ? repo_name(repo.url) : undefined}>
  {#if hosts.length > 1}
    <!-- A switch closes the tabs: save first. -->
    <Select aria-label="Host" size="sm" value={host} disabled={unsaved} title={unsaved ? 'Save or close your changes to switch hosts' : undefined} options={hosts.map((h) => ({ value: h.id, label: h.name }))} onchange={(e) => pick_host(e.currentTarget.value)} />
  {/if}
  {#if host}
    <IconButton icon="search" label="Go to file" shortcut="⌘P" size="sm" aria-keyshortcuts="Meta+P Control+P" onclick={() => quick_open('')} />
    {#if can_terminal}
      <Button size="sm" variant="secondary" iconLeft="terminal" aria-keyshortcuts="Control+`" onclick={toggle_terminal}>Terminal</Button>
    {/if}
  {/if}
  {#if repo && !can_write}
    <Badge tone="neutral"><Icon name="lock" size={12} /> Read-only</Badge>
  {:else if current?.state}
    <Button size="sm" variant="secondary" iconLeft="save" disabled={!current.dirty} aria-keyshortcuts="Meta+S Control+S" onclick={() => current && save(current)}>Save</Button>
  {/if}
</PageHeader>

{#if load_error}
  <div class="sun-page"><div class="sun-empty" role="alert">{load_error}</div></div>
{:else if !repo}
  <div class="sun-page"><div class="sun-empty" role="status">Loading…</div></div>
{:else if !host}
  <div class="sun-page">
    <div class="sun-empty">No host of this workspace is online. Start sun on a machine attached in <a href={href('hosts')}>Hosts</a>.</div>
  </div>
{:else}
  <div class="editor">
    <nav class="tree" aria-label="Files">
      <div class="tree__bar">
        <span class="tree__title">Files</span>
        {#if can_write}
          <IconButton icon="file-plus" label="New file" variant="ghost" size="sm" onclick={() => new_entry('file', '')} />
          <IconButton icon="folder-plus" label="New folder" variant="ghost" size="sm" onclick={() => new_entry('dir', '')} />
        {/if}
      </div>
      {#await opened}
        <div class="sun-empty" role="status">Preparing the worktree…</div>
      {:then}
        <ul class="tree__list">{@render listing('', 0)}</ul>
      {:catch e}
        <div class="sun-empty" role="alert">
          {error_text(e)}
          <Button size="sm" variant="secondary" onclick={() => pick_host(host)}>Try again</Button>
        </div>
      {/await}
    </nav>

    <section class="pane" aria-label="Editor">
      {#if tabs.length}
        <div class="tabs" role="tablist" aria-label="Open files">
          {#each tabs as tab (tab.path)}
            <div class="tab" class:tab--active={tab.path === active}>
              <button type="button" role="tab" class="tab__name" aria-selected={tab.path === active} title={tab.path} onclick={() => show(tab.path)}>
                {base_name(tab.path)}
                {#if tab.dirty}<span class="tab__dot" role="img" aria-label="Unsaved changes"></span>{/if}
              </button>
              <IconButton icon="x" label="Close {base_name(tab.path)}" variant="ghost" size="sm" onclick={() => close(tab)} />
            </div>
          {/each}
        </div>
      {/if}
      {#if current && !current.state}
        <div class="sun-empty" role="status">
          {current.file.too_large ? 'This file is over 2 MB, too large to open here.' : 'This is a binary file, which the editor can’t show.'}
        </div>
      {:else if !current}
        <div class="sun-empty">Pick a file on the left to open it.</div>
      {/if}
      <div class="code" hidden={!current?.state} {@attach mount}></div>
      {#if can_terminal}
        {#key host}<EditorTerminal bind:this={terminal} at={at()} ready={opened ?? Promise.resolve()} app_key={(e) => !!shortcut_of(e)} />{/key}
      {/if}
    </section>
  </div>
{/if}

{#snippet listing(dir: string, depth: number)}
  {#each dirs[dir]?.entries ?? [] as entry (entry.name)}
    {@const path = join(dir, entry.name)}
    {@const open = expanded.has(path)}
    <li>
      <!-- svelte-ignore a11y_no_static_element_interactions: the actions button is the keyboard way to the same menu -->
      <div class="tree__row" class:tree__row--active={path === active} style:--depth={depth} oncontextmenu={can_write ? context_menu : undefined}>
        <button
          type="button"
          class="tree__item"
          aria-expanded={entry.kind === 'dir' ? open : undefined}
          aria-current={path === active ? 'true' : undefined}
          onclick={() => (entry.kind === 'dir' ? toggle(path) : open_file(path))}
        >
          <Icon name={entry.kind === 'dir' ? (open ? 'folder-open' : 'folder') : 'file'} size={16} />
          <span class="tree__name">{entry.name}</span>
        </button>
        {#if can_write}<Menu label="Actions for {entry.name}" icon="ellipsis" items={menu(path, entry)} />{/if}
      </div>
      {#if open}<ul class="tree__list">{@render listing(path, depth + 1)}</ul>{/if}
    </li>
  {/each}
  {#if dirs[dir]?.truncated}<li class="tree__note">Only the first entries are shown.</li>{/if}
{/snippet}

{#if naming}
  {@const what = naming.kind === 'rename' ? 'Rename' : naming.kind === 'dir' ? 'New folder' : 'New file'}
  <Dialog open onclose={() => (naming = null)} title={what}>
    <form id="editor-name" onsubmit={submit_name}>
      <Input label="Name" autocomplete="off" bind:value={naming.name} error={naming.error || undefined} hint={naming.dir ? `In ${naming.dir}` : undefined} />
    </form>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (naming = null)}>Cancel</Button>
      <Button type="submit" form="editor-name" variant="ice" loading={busy}>{naming?.kind === 'rename' ? 'Rename' : 'Create'}</Button>
    {/snippet}
  </Dialog>
{/if}

{#if removing}
  {@const target = removing}
  <Dialog open onclose={() => (removing = null)} title={target.dir ? 'Delete folder?' : 'Delete file?'}>
    <p class="note">{target.dir ? 'The folder and everything in it are deleted from the worktree.' : 'The file is deleted from the worktree.'}</p>
    <p class="note ml-mono">{target.path}</p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (removing = null)}>Cancel</Button>
      <Button variant="danger" loading={busy} onclick={() => remove(target.path)}>Delete</Button>
    {/snippet}
  </Dialog>
{/if}

{#if palette !== null}
  <QuickOpen query={palette} paths={index} {commands} onopen={open_file} onclose={() => (palette = null)} />
{/if}

{#if conflict}
  {@const tab = tabs.find((t) => t.path === conflict)}
  <Dialog open onclose={() => (conflict = '')} title="The file changed on disk">
    <p class="note">{conflict} changed since you opened it. Reload it and lose your edits, or overwrite it with yours.</p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => tab && reload(tab)}>Reload</Button>
      <Button variant="danger" onclick={() => tab && save(tab, true)}>Overwrite</Button>
    {/snippet}
  </Dialog>
{/if}

{#if closing}
  {@const tab = closing}
  <Dialog open onclose={() => (closing = null)} title="Discard unsaved changes?">
    <p class="note">{tab.path} has changes that aren’t saved.</p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (closing = null)}>Keep editing</Button>
      <Button variant="danger" onclick={() => close(tab, true)}>Discard</Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .editor {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-rows: minmax(0, 35%) minmax(0, 1fr);
    border-top: 1px solid var(--border-subtle);
  }
  @media (min-width: 768px) {
    .editor {
      grid-template-rows: none;
      grid-template-columns: 260px minmax(0, 1fr);
    }
  }
  .tree {
    min-height: 0;
    overflow-y: auto;
    padding: var(--space-2);
    border-right: 1px solid var(--border-subtle);
    border-bottom: 1px solid var(--border-subtle);
  }
  .tree__bar {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    padding: 0 var(--space-1) var(--space-2);
  }
  .tree__title {
    flex: 1;
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .tree__list {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .tree__row {
    display: flex;
    align-items: center;
    border-radius: var(--radius-sm);
    padding-left: calc(var(--depth) * var(--space-3));
  }
  .tree__row:hover {
    background: var(--bg-subtle);
  }
  .tree__row--active {
    background: var(--accent-soft);
  }
  /* The actions button shows on hover and focus; a touch screen has no hover, so there it always shows. */
  @media (hover: hover) {
    .tree__row :global(.sun-tip) {
      opacity: 0;
    }
    .tree__row:hover :global(.sun-tip),
    .tree__row:focus-within :global(.sun-tip) {
      opacity: 1;
    }
  }
  .tree__item {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-height: 30px;
    padding: 0 var(--space-2);
    border: 0;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--fg-1);
    font: inherit;
    font-size: 13px;
    text-align: left;
    cursor: pointer;
  }
  .tree__item :global(svg),
  .tree__item :global(.ml-icon) {
    flex: none;
    color: var(--fg-2);
  }
  .tree__item:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
  }
  .tree__name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .tree__note {
    padding: var(--space-1) var(--space-2);
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .pane {
    min-height: 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  .tabs {
    display: flex;
    flex: none;
    overflow-x: auto;
    border-bottom: 1px solid var(--border-subtle);
  }
  .tab {
    display: flex;
    flex: none;
    align-items: center;
    padding-right: var(--space-1);
    border-right: 1px solid var(--border-subtle);
    color: var(--fg-2);
  }
  .tab--active {
    color: var(--fg-1);
    background: var(--surface-sunken);
    box-shadow: inset 0 -2px 0 var(--accent);
  }
  .tab__name {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-height: 36px;
    padding: 0 var(--space-2) 0 var(--space-3);
    border: 0;
    background: transparent;
    color: inherit;
    font: inherit;
    font-size: 13px;
    cursor: pointer;
  }
  .tab__name:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
  }
  .tab__dot {
    width: 8px;
    height: 8px;
    border-radius: var(--radius-pill);
    background: var(--warning);
  }
  .code {
    flex: 1;
    min-height: 0;
  }
  .code[hidden] {
    display: none;
  }
  .note {
    margin: 0;
    color: var(--fg-2);
    font-size: var(--size-sm);
    line-height: 1.5;
    overflow-wrap: anywhere;
  }
</style>
