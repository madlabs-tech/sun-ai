<!-- Phone-only overflow for nav items that don't fit the bottom tab bar, plus the workspace menu and Log out. -->
<script lang="ts">
  import { Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import WorkspaceSwitcher from '../lib/ui/WorkspaceSwitcher.svelte';
  import { sign_out } from '../lib/app_state.svelte';
  import { TAB_COUNT, href, visible_nav } from '../lib/router.svelte';
</script>

<PageHeader title="More" />

<nav class="sun-page" aria-label="More">
  <WorkspaceSwitcher />
  {#each visible_nav().slice(TAB_COUNT) as item (item.id)}
    <a href={href(item.id)} class="sun-row" style="align-items:center;color:var(--fg-1);font-weight:600;text-decoration:none">
      <span class="sun-tile"><Icon name={item.icon} size={18} /></span>
      <span style="flex:1">{item.label}</span>
      <Icon name="chevron-right" size={18} />
    </a>
  {/each}
  <Button variant="secondary" iconLeft="log-out" block style="flex:none" onclick={sign_out}>Log out</Button>
</nav>
