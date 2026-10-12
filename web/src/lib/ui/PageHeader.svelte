<script lang="ts">
  import type { Snippet } from 'svelte';
  import IconButton from './IconButton.svelte';
  import { SIDEBAR_SHORTCUT, toggle_sidebar } from '../app_state.svelte';

  /** `lead` replaces the title (e.g. a breadcrumb on a page whose h1 is further down). */
  let { title = '', subtitle, lead, children }: { title?: string; subtitle?: string; lead?: Snippet; children?: Snippet } = $props();
</script>

<header class="sun-header">
  <!-- Shown only on a desktop with the sidebar hidden (app.css). -->
  <IconButton
    icon="panel-left"
    label="Toggle sidebar"
    shortcut={SIDEBAR_SHORTCUT}
    variant="ghost"
    size="sm"
    class="sun-sidebar-expand"
    aria-keyshortcuts="Meta+B Control+B"
    data-sidebar-toggle
    onclick={toggle_sidebar}
  />
  {#if lead}
    {@render lead()}
  {:else}
    <div>
      <h1>{title}</h1>
      {#if subtitle}<p>{subtitle}</p>{/if}
    </div>
  {/if}
  <span class="sun-header__spacer"></span>
  {@render children?.()}
</header>
