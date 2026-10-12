<!-- Await a list request: loading line, error with a plain next step, empty state, or the rows. -->
<script lang="ts" generics="T">
  import type { Snippet } from 'svelte';
  import { error_text } from '../app_state.svelte';

  let { load, empty, row }: { load: Promise<T[]>; empty: string; row: Snippet<[T]> } = $props();
</script>

{#await load}
  <div class="sun-empty" role="status">Loading…</div>
{:then items}
  {#each items as item, i (i)}
    {@render row(item)}
  {:else}
    <div class="sun-empty">{empty}</div>
  {/each}
{:catch e}
  <div class="sun-empty" role="alert">{error_text(e)}</div>
{/await}
