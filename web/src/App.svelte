<!-- App chrome: sidebar (desktop), page (a task's own page at #/tasks/<KEY>), bottom tab bar (phone), new-task dialog, toasts. -->
<script lang="ts">
  import { Badge, Icon, Toast } from '@hryer/madlabs-design-system/svelte';
  import { app, dismiss, feature_on, load_workspace, toasts, toggle_sidebar, workspace_of } from './lib/app_state.svelte';
  import { TAB_COUNT, href, page_on, route, visible_nav } from './lib/router.svelte';
  import { needs_approval } from './lib/format';
  import Sidebar from './screens/Sidebar.svelte';
  import Tasks from './screens/Tasks.svelte';
  import TaskPage from './screens/TaskPage.svelte';
  import CreateTask from './screens/CreateTask.svelte';
  import Approvals from './screens/Approvals.svelte';
  import Runtimes from './screens/Runtimes.svelte';
  import Hosts from './screens/Hosts.svelte';
  import Mods from './screens/Mods.svelte';
  import Workflows from './screens/Workflows.svelte';
  import Catalog from './screens/Catalog.svelte';
  import Knowledge from './screens/Knowledge.svelte';
  import Memory from './screens/Memory.svelte';
  import Evals from './screens/Evals.svelte';
  import Market from './screens/Market.svelte';
  import Settings from './screens/Settings.svelte';
  import More from './screens/More.svelte';
  import SignIn from './screens/SignIn.svelte';
  import Invite from './screens/Invite.svelte';
  import Welcome from './screens/Welcome.svelte';
  import QuickAction from './screens/QuickAction.svelte';
  import Canvas from './screens/Canvas.svelte';
  import Projects from './screens/Projects.svelte';
  import Agents from './screens/Agents.svelte';

  load_workspace();

  // Switching workspaces (the sidebar, a link, back/forward) loads that workspace's tasks.
  $effect(() => {
    // A link without `?workspace=` gets the remembered one back.
    if (app.loaded && (route.workspace ?? '') !== (workspace_of(app.workspace)?.slug ?? '')) load_workspace();
  });

  // ponytail: the kernel has no workspace-wide stream, so the task list polls; switch to SSE when it has one.
  $effect(() => {
    const timer = setInterval(() => document.visibilityState === 'visible' && !app.signed_out && load_workspace(), 5000);
    return () => clearInterval(timer);
  });

  const PAGES = { projects: Projects, agents: Agents, approvals: Approvals, runtimes: Runtimes, hosts: Hosts, workflows: Workflows, mods: Mods, catalog: Catalog, knowledge: Knowledge, memory: Memory, evals: Evals, market: Market, settings: Settings, more: More };
  // A turned-off feature's page, or a deployment page for someone who isn't its owner (an old link), falls back to Tasks.
  const Page = $derived(page_on(route.page) ? PAGES[route.page as keyof typeof PAGES] : undefined);
  const tabs = $derived([...visible_nav().slice(0, TAB_COUNT), { id: 'more', label: 'More', icon: 'ellipsis' }]);
  const tab_active = $derived(
    route.page === 'tasks' || route.page === 'approve' || route.page === 'canvas' ? 'tasks' : tabs.some((n) => n.id === route.page) ? route.page : 'more',
  );
  const waiting = $derived(app.tasks.filter(needs_approval).length);
  /** The open task: #/tasks/<key>, or #/projects/<project>/<key> inside a project. */
  const task_ref = $derived(route.page === 'tasks' ? route.id : route.page === 'projects' ? route.sub : null);

  /** ⌘B / Ctrl+B shows or hides the sidebar, except while typing. */
  function shortcut(e: KeyboardEvent) {
    if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey || e.key.toLowerCase() !== 'b') return;
    const target = e.target as HTMLElement;
    if (target.isContentEditable || target.matches?.('input, textarea, select')) return;
    e.preventDefault();
    toggle_sidebar();
  }

  let new_open = $state(false);
  const open_new = () => (new_open = true);
</script>

<svelte:window onkeydown={shortcut} />

{#if route.page === 'invite'}
  <Invite token={route.id ?? ''} />
{:else if app.signed_out || app.gate || route.page === 'login'}
  <SignIn />
{:else if app.loaded && !app.error && !app.workspaces.length}
  <Welcome />
{:else}
<div class="sun-app" class:sun-app--collapsed={app.sidebar_collapsed}>
  <Sidebar on_new={open_new} />

  <main class="sun-main">
    <!-- Pages wait for the workspaces: the URL names one by slug, and they load its data by id. -->
    {#if !app.loaded}
      <div class="sun-page" role="status">Loading…</div>
    {:else if route.page === 'canvas' && route.id && feature_on('canvas')}
      <Canvas id={route.id} />
    {:else if task_ref}
      <TaskPage id={task_ref} project_id={route.page === 'projects' ? (route.id ?? undefined) : undefined} />
    {:else if route.page === 'projects' && route.id && Page}
      <Tasks on_new={open_new} project_id={route.id} />
    {:else if Page}
      <Page />
    {:else}
      <Tasks on_new={open_new} />
    {/if}
  </main>

  {#if route.page === 'approve' && route.id}
    <QuickAction run_id={route.id} step_key={route.sub} />
  {/if}

  <nav class="sun-tabbar" aria-label="Main">
    {#each tabs as item (item.id)}
      <a href={href(item.id)} aria-current={tab_active === item.id ? 'page' : undefined}>
        <span class="sun-tabbar__pill"><Icon name={item.icon} size={20} /></span>
        {item.label}
        {#if item.id === 'approvals' && waiting}
          <span class="sun-tabbar__count"><Badge tone="warning" count aria-label="{waiting} waiting">{waiting}</Badge></span>
        {/if}
      </a>
    {/each}
  </nav>
</div>

{/if}

<CreateTask bind:open={new_open} project_id={route.page === 'projects' ? (route.id ?? undefined) : undefined} />

<div class="sun-toasts" aria-live="polite">
  {#each toasts as t (t.id)}
    <Toast tone={t.tone} title={t.title} description={t.description} onclose={() => dismiss(t.id)} />
  {/each}
</div>
