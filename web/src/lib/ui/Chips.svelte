<!-- A list of short values (skills, gates, approvers, runtimes) as removable tags plus an add field.
     Items that aren't plain strings (a gate with params) show by `name` and are kept as they are. -->
<script lang="ts" generics="T">
  import { Input, Tag } from '@hryer/madlabs-design-system/svelte';
  import IconButton from './IconButton.svelte';

  let {
    label,
    items = $bindable([]),
    placeholder = '',
    hint = '',
    suggestions = [],
    name = (item: T) => String(item),
  }: {
    label: string;
    items?: T[];
    placeholder?: string;
    hint?: string;
    suggestions?: string[];
    name?: (item: T) => string;
  } = $props();

  const uid = $props.id();
  let draft = $state('');

  function add() {
    const value = draft.trim();
    if (value && !items.some((i) => name(i) === value)) items = [...items, value as T];
    draft = '';
  }
</script>

<div class="ml-field chips">
  <span class="ml-field__label" id="{uid}-label">{label}</span>
  {#if items.length}
    <ul class="sun-chips" aria-labelledby="{uid}-label">
      <!-- index key: a hand-written file may repeat a value, and tags hold no state -->
      {#each items as item, i (i)}
        <li><Tag size="sm" onremove={() => (items = items.filter((_, j) => j !== i))}><span class="ml-mono">{name(item)}</span></Tag></li>
      {/each}
    </ul>
  {/if}
  <div class="chips__add">
    <Input
      mono
      {placeholder}
      aria-label="Add to {label}"
      list={suggestions.length ? `${uid}-list` : undefined}
      bind:value={draft}
      onkeydown={(e: KeyboardEvent) => e.key === 'Enter' && (e.preventDefault(), add())}
      style="flex:1;min-width:0"
    />
    <IconButton icon="plus" label="Add to {label}" variant="secondary" onclick={add} />
  </div>
  {#if hint}<div class="ml-field__hint">{hint}</div>{/if}
  {#if suggestions.length}
    <datalist id="{uid}-list">{#each suggestions as s (s)}<option value={s}></option>{/each}</datalist>
  {/if}
</div>

<style>
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .chips__add {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
</style>
