<!-- Workflows: the saved ones, role templates to start from, and the builder at #/workflows/<id|new>. -->
<script lang="ts">
  import { Badge, Button, Icon, Select, Tag } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import { api } from '../lib/api/client';
  import type { Workflow, WorkflowTemplate } from '../lib/api/types';
  import { app, can, current_workspace, error_text, set_role, toast } from '../lib/app_state.svelte';
  import { ROLE } from '../lib/format';
  import { go, href, route } from '../lib/router.svelte';

  let workflows = $state<Promise<Workflow[]>>(new Promise(() => {}));
  let templates = $state<Promise<WorkflowTemplate[]>>(new Promise(() => {}));
  // Reload the lists each time the page comes back from the builder.
  $effect(() => {
    if (route.id) return;
    workflows = api.list_workflows(current_workspace() || undefined);
    templates = api.list_templates();
  });
  let installing = $state('');

  // Onboarding: the role you pick puts its templates first; nothing installs until you click Use.
  const role = $derived(app.role);
  const pick_role = (r: string) => set_role(r).catch((e) => toast('danger', 'Role not saved', error_text(e)));
  const ROLE_OPTIONS = [{ value: '', label: 'Any role' }, ...Object.entries(ROLE).map(([value, label]) => ({ value, label }))];
  /** `for_role` is read when the template renders, so picking a role re-sorts. */
  const suggested = (for_role: string) =>
    templates.then((all) => [...all].sort((a, b) => Number(b.role === for_role) - Number(a.role === for_role)));

  async function install(t: WorkflowTemplate) {
    installing = t.id;
    try {
      const w = await api.install_template(t.id, current_workspace());
      toast('success', `${t.title} added`, t.mods.length ? `It uses ${t.mods.join(', ')}. Install those from the catalog.` : undefined);
      go('workflows', w.id);
    } catch (e) {
      toast('danger', 'Not added', error_text(e));
      workflows = api.list_workflows(current_workspace());
    }
    installing = '';
  }
</script>

{#if route.id}
  <!-- Loaded on demand: the YAML library stays out of the workspace's bundle. -->
  {#await import('./Builder.svelte') then { default: Builder }}
    <Builder id={route.id} />
  {/await}
{:else}
  <PageHeader title="Workflows" subtitle="The steps a task goes through. Build them from blocks or edit the file.">
    {#if can('create_workflow')}<Button variant="ice" iconLeft="plus" href={href('workflows', 'new')}>New workflow</Button>{/if}
  </PageHeader>

  <div class="sun-page">
    <section aria-labelledby="yours" class="list">
      <h2 class="sun-label" id="yours">Your workflows</h2>
      <Loaded load={workflows} empty="No workflows yet. Start from a template below.">
        {#snippet row(w: Workflow)}
          <a class="sun-row link" href={href('workflows', w.id)}>
            <span class="sun-tile"><Icon name="workflow" size={18} /></span>
            <span class="sun-row__body">
              <span class="sun-row__title"><span class="ml-mono">{w.name}</span></span>
              <span class="sun-row__meta">Version {w.version}</span>
            </span>
            <Icon name="chevron-right" size={18} />
          </a>
        {/snippet}
      </Loaded>
    </section>

    <section aria-labelledby="templates" class="list">
      <h2 class="sun-label" id="templates">Start from a template</h2>
      <div class="role">
        <Select label="Your role" hint="Its templates come first." options={ROLE_OPTIONS} bind:value={() => role, pick_role} />
      </div>
      {#await workflows then saved}
        <Loaded load={suggested(role)} empty="No templates in this build.">
          {#snippet row(t: WorkflowTemplate)}
            {@const have = saved.find((w) => w.name === t.id)}
            <article class="sun-row">
              <span class="sun-tile"><Icon name="list-ordered" size={18} /></span>
              <div class="sun-row__body">
                <div class="sun-row__title">
                  {t.title} <Tag size="sm">{ROLE[t.role]}</Tag>
                  {#if role && t.role === role}<Badge tone="info">Suggested</Badge>{/if}
                </div>
                <div class="desc">{t.description}</div>
                {#if t.mods.length}
                  <div class="sun-row__meta">Uses {t.mods.join(', ')}. Nothing installs without your click.</div>
                {/if}
              </div>
              {#if have}
                <Badge tone="success" dot>Added</Badge>
                <Button size="sm" variant="secondary" href={href('workflows', have.id)}>Open</Button>
              {:else}
                {#if can('create_workflow')}
                  <Button size="sm" variant="ice" iconLeft="plus" loading={installing === t.id} aria-label="Use {t.title}" onclick={() => install(t)}>Use</Button>
                {/if}
              {/if}
            </article>
          {/snippet}
        </Loaded>
      {/await}
    </section>
  </div>
{/if}

<style>
  .list {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .sun-label {
    margin: 0 0 4px;
  }
  .link {
    align-items: center;
    color: var(--fg-1);
    text-decoration: none;
    flex-wrap: nowrap;
  }
  .link:hover {
    border-color: var(--border-strong);
    text-decoration: none;
  }
  .link:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
  }
  .link .sun-row__meta {
    display: block;
  }
  .role {
    max-width: 320px;
  }
  .desc {
    font-size: 14px;
    color: var(--fg-1);
    margin: 4px 0 6px;
  }
</style>
