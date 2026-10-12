<!-- The new-workspace form: name, URL slug (from the name until edited) and the task-key prefix it implies. The
     caller puts the submit button (`form={id}`). On success the person owns it and it opens. Used by the
     workspace menu's dialog and by onboarding. -->
<script lang="ts">
  import { Input } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../api/client';
  import { error_text, load_memberships, load_workspace } from '../app_state.svelte';
  import { SLUG, prefix_from_slug, slug_from_name } from '../format';
  import { show_workspace } from '../router.svelte';

  let { id, busy = $bindable(false), oncreated }: { id: string; busy?: boolean; oncreated?: () => void } = $props();

  let name = $state('');
  /** The slug as typed; `null` until then, while it follows the name. */
  let slug_typed = $state<string | null>(null);
  const slug = $derived(slug_typed ?? (name.trim() ? slug_from_name(name) : ''));
  let errors = $state<{ name?: string; slug?: string }>({});

  async function create(e: SubmitEvent) {
    e.preventDefault();
    errors = {
      name: name.trim() ? undefined : 'Give the workspace a name.',
      slug: SLUG.test(slug) ? undefined : 'Use lowercase letters and digits, in words joined by single hyphens.',
    };
    if (errors.name || errors.slug) return;
    busy = true;
    try {
      const created = await api.create_workspace(name.trim(), slug);
      await load_memberships();
      await load_workspace();
      show_workspace(created.slug);
      oncreated?.();
    } catch (err) {
      const text = error_text(err);
      errors = /slug/i.test(text) ? { slug: text } : { name: text };
    }
    busy = false;
  }
</script>

<form {id} class="workspace-form" onsubmit={create} novalidate>
  <Input label="Name" placeholder="e.g. Marketing" maxlength={64} bind:value={name} error={errors.name} hint="Each workspace has its own members, workflows, tasks, repos and knowledge." />
  <Input
    label="Workspace URL"
    mono
    maxlength={48}
    autocomplete="off"
    spellcheck={false}
    value={slug}
    oninput={(e: Event) => (slug_typed = (e.currentTarget as HTMLInputElement).value.trim().toLowerCase())}
    error={errors.slug}
    hint="Links use ?workspace={slug || '…'}. It can't be changed later."
  />
  <p class="workspace-form__prefix" aria-live="polite">
    Task keys: <span class="ml-mono">{prefix_from_slug(slug) || '…'}-1</span>, <span class="ml-mono">{prefix_from_slug(slug) || '…'}-2</span>, … The prefix can be changed in Settings.
  </p>
</form>

<style>
  .workspace-form {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .workspace-form__prefix {
    margin: calc(-1 * var(--space-2)) 0 0;
    color: var(--fg-2);
    font-size: var(--size-sm);
    line-height: 1.5;
  }
</style>
