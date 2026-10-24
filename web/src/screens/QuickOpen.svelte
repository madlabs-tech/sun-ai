<!-- The editor's quick open (⌘/Ctrl+P): fuzzy-matches the worktree's file paths (`GET …/editor/index`); arrow keys
     move, Enter opens. A leading `>` lists commands instead, as VS Code's ⌘/Ctrl+Shift+P does. Firefox keeps
     Ctrl+Shift+P for a private window and pages can't take it, so there ⌘/Ctrl+P and a typed `>` are the way. -->
<script lang="ts" module>
  export interface Command {
    label: string;
    run: () => void;
  }
</script>

<script lang="ts">
  import { Dialog, Input } from '@hryer/madlabs-design-system/svelte';
  import { error_text } from '../lib/app_state.svelte';

  let { query = '', paths, commands, onopen, onclose }: {
    query?: string;
    paths: Promise<string[]>;
    commands: Command[];
    onopen: (path: string) => void;
    onclose: () => void;
  } = $props();

  const LIMIT = 50;
  let files = $state<string[] | null>(null);
  let load_error = $state('');
  $effect(() => {
    paths.then((p) => (files = p), (e) => (load_error = error_text(e)));
  });

  /** Higher is better, -1 when the letters of `query` aren't all in `text` in order. Letters in the file
   *  name, at word starts and in runs score more; the name holding `query` whole beats everything. */
  function score(query: string, text: string): number {
    const [q, t] = [query.toLowerCase().replaceAll(' ', ''), text.toLowerCase()];
    const name = t.lastIndexOf('/') + 1;
    // ponytail: greedy leftmost match, not the best alignment; enough to rank paths by a typed fragment.
    let total = t.slice(name).includes(q) ? 100 : 0;
    let at = -1;
    for (const ch of q) {
      const i = t.indexOf(ch, at + 1);
      if (i < 0) return -1;
      total += (i >= name ? 2 : 0) + (i === at + 1 ? 3 : 0) + (i === 0 || '/._-'.includes(t[i - 1]) ? 2 : 0);
      at = i;
    }
    return total - t.length / 1000;
  }

  function ranked<T>(items: T[], text: (item: T) => string, typed: string): T[] {
    if (!typed.trim()) return items.slice(0, LIMIT);
    return items
      .map((item) => [score(typed, text(item)), item] as const)
      .filter(([s]) => s >= 0)
      .sort((a, b) => b[0] - a[0])
      .slice(0, LIMIT)
      .map(([, item]) => item);
  }

  const in_commands = $derived(query.startsWith('>'));
  const results = $derived<(Command | string)[]>(
    in_commands ? ranked(commands, (c) => c.label, query.slice(1)) : ranked(files ?? [], (p) => p, query),
  );
  // Reset on typing only: the index can arrive after an arrow key moved the selection.
  let selected = $state(0);

  function pick(item: Command | string | undefined) {
    if (item === undefined) return;
    onclose();
    if (typeof item === 'string') onopen(item);
    else item.run();
  }

  function keydown(e: KeyboardEvent) {
    const step = { ArrowDown: 1, ArrowUp: -1 }[e.key];
    if (step) {
      e.preventDefault();
      selected = (selected + step + results.length) % Math.max(results.length, 1);
      document.getElementById(`quick-open-${selected}`)?.scrollIntoView({ block: 'nearest' });
    } else if (e.key === 'Enter') {
      e.preventDefault();
      pick(results[selected]);
    }
  }

  const label = (item: Command | string) => (typeof item === 'string' ? item.slice(item.lastIndexOf('/') + 1) : item.label);
  const folder = (item: Command | string) => (typeof item === 'string' && item.includes('/') ? item.slice(0, item.lastIndexOf('/')) : '');
</script>

<Dialog open {onclose} title={in_commands ? 'Run a command' : 'Go to file'}>
  <Input
    aria-label={in_commands ? 'Command' : 'File name'}
    role="combobox"
    aria-expanded="true"
    aria-controls="quick-open-list"
    aria-activedescendant={results.length ? `quick-open-${selected}` : undefined}
    autocomplete="off"
    spellcheck="false"
    iconLeft={in_commands ? 'command' : 'search'}
    bind:value={query}
    oninput={() => (selected = 0)}
    onkeydown={keydown}
    {@attach (input: HTMLInputElement) => input.focus()}
  />
  <ul class="results" id="quick-open-list" role="listbox" aria-label={in_commands ? 'Commands' : 'Files'}>
    {#each results as item, i (typeof item === 'string' ? item : item.label)}
      <!-- svelte-ignore a11y_click_events_have_key_events: the input above drives the list from the keyboard -->
      <li
        id="quick-open-{i}"
        role="option"
        class="result"
        aria-selected={i === selected}
        onclick={() => pick(item)}
        onpointermove={() => (selected = i)}
      >
        <span class="result__name">{label(item)}</span>
        {#if folder(item)}<span class="result__dir">{folder(item)}</span>{/if}
      </li>
    {:else}
      <li class="sun-empty" role={load_error ? 'alert' : 'status'}>
        {load_error || (!in_commands && files === null ? 'Listing files…' : in_commands ? 'No command matches.' : 'No file matches.')}
      </li>
    {/each}
  </ul>
  <p class="hint">Arrow keys to move, Enter to {in_commands ? 'run' : 'open'}. {in_commands ? 'Delete the > to find files.' : 'Type > for commands.'}</p>
</Dialog>

<style>
  .results {
    list-style: none;
    margin: var(--space-2) 0 0;
    padding: 0;
    max-height: min(50vh, 420px);
    overflow-y: auto;
  }
  .result {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    min-height: 32px;
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-sm);
    font-size: 13px;
    color: var(--fg-1);
    cursor: pointer;
  }
  .result[aria-selected='true'] {
    background: var(--accent-soft);
  }
  .result__name {
    flex: none;
  }
  .result__dir {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--fg-2);
    font-size: var(--size-xs);
  }
  .hint {
    margin: var(--space-2) 0 0;
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
</style>
