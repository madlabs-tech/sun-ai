<!-- A workspace's logo, or the first letter of its name on ice when it has none. Decorative: the name is always beside it. -->
<script lang="ts">
  import type { Workspace } from '../api/types';

  let { workspace, size }: { workspace: Workspace | undefined; size: number } = $props();
</script>

<span class="logo" style="--logo-size:{size}px" aria-hidden="true">
  {#if workspace?.logo_url}
    <img src={workspace.logo_url} alt="" />
  {:else}
    {(workspace?.name.trim()[0] ?? '?').toUpperCase()}
  {/if}
</span>

<style>
  .logo {
    flex: none;
    display: grid;
    place-items: center;
    width: var(--logo-size);
    height: var(--logo-size);
    overflow: hidden;
    border-radius: var(--radius-sm);
    background: var(--ice-400);
    color: var(--on-ice);
    font-size: calc(var(--logo-size) * 0.46);
    font-weight: 700;
  }
  .logo img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
</style>
