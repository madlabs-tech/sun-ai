<!-- Desktop sidebar: the account/workspace menu and New task, then the nav by section (Approvals on its
     own, Work, AI Team), then the workspace's spend and Settings. Hidden with the toggle or ⌘B / Ctrl+B. -->
<script lang="ts">
  import { Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import WorkspaceSwitcher from '../lib/ui/WorkspaceSwitcher.svelte';
  import { SIDEBAR_SHORTCUT, app, toggle_sidebar } from '../lib/app_state.svelte';
  import { href, route, visible_nav, type NavSection } from '../lib/router.svelte';
  import { money, needs_approval } from '../lib/format';

  let { on_new }: { on_new: () => void } = $props();

  const counts = $derived<Record<string, number>>({
    tasks: app.tasks.filter((t) => t.status !== 'done' && t.status !== 'cancelled').length,
    approvals: app.tasks.filter(needs_approval).length,
  });
  const spend = $derived(app.tasks.reduce((sum, t) => sum + (t.run?.cost_usd ?? 0), 0));
  const items = (section: NavSection) => visible_nav().filter((n) => n.section === section);
</script>

{#snippet links(section: NavSection)}
  {#each items(section) as item (item.id)}
    <a href={href(item.id)} aria-current={route.page === item.id ? 'page' : undefined}>
      <Icon name={item.icon} size={18} />
      <span class="sun-nav__label">{item.label}</span>
      {#if counts[item.id]}<span class="ml-mono" style="font-size:11px">{counts[item.id]}</span>{/if}
    </a>
  {/each}
{/snippet}

<aside class="sun-sidebar" aria-label="Main">
  <div class="sidebar__top">
    <div class="sun-wordmark">
      <span class="sun-wordmark__sun">SUN</span>
      <span class="sun-wordmark__by">by <b>MADLABS</b></span>
    </div>
    <IconButton
      icon="panel-left"
      label="Toggle sidebar"
      shortcut={SIDEBAR_SHORTCUT}
      variant="ghost"
      size="sm"
      aria-keyshortcuts="Meta+B Control+B"
      data-sidebar-toggle
      onclick={toggle_sidebar}
    />
  </div>
  <WorkspaceSwitcher />
  <Button variant="ice" iconLeft="plus" block onclick={on_new}>New task</Button>

  <nav class="sun-nav sidebar__pages" aria-label="Pages">
    {@render links('')}
    {#each ['Work', 'AI Team'] as const as section (section)}
      {#if items(section).length}
        <h2 class="sun-label sidebar__section">{section}</h2>
        {@render links(section)}
      {/if}
    {/each}
  </nav>

  <div class="sidebar__footer">
    <div class="sidebar__spend">
      <span>Workspace spend</span>
      <span class="ml-mono">{money(spend)}</span>
    </div>
    <div class="sun-nav">{@render links('footer')}</div>
  </div>
</aside>

<style>
  .sidebar__top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
  }
  /* Header and footer keep their size; the pages scroll between them on a short window. */
  .sun-sidebar > :global(*) {
    flex: none;
  }
  .sun-sidebar > .sidebar__pages {
    flex: 0 1 auto;
    min-height: 0;
    overflow-y: auto;
    /* room for the focus ring inside the scroller */
    padding: 4px;
    margin: -4px;
  }
  .sidebar__section {
    margin: var(--space-4) 0 var(--space-1);
    padding: 0 var(--space-3);
    font-size: 15px;
  }
  .sidebar__footer {
    margin-top: auto;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding-top: var(--space-3);
    border-top: 1.5px solid var(--border-subtle);
  }
  .sidebar__spend {
    display: flex;
    justify-content: space-between;
    gap: var(--space-2);
    padding: 0 var(--space-3);
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .sidebar__spend .ml-mono {
    color: var(--fg-1);
  }
</style>
