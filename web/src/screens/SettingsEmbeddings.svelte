<!-- Settings → Embeddings: the provider that adds search by meaning to Knowledge. The shell shows it with knowledge on and manage_settings. -->
<script lang="ts">
  import { Badge, Button, Icon, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../lib/api/client';
  import type { EmbeddingProvider, Embeddings } from '../lib/api/types';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { whole } from '../lib/format';

  const PROVIDERS: { value: EmbeddingProvider; label: string }[] = [
    { value: 'none', label: 'Off — keyword search only' },
    { value: 'openai', label: 'OpenAI-compatible' },
    { value: 'ollama', label: 'Ollama' },
  ];
  const URL_HINT: Record<EmbeddingProvider, string> = { none: '', openai: 'https://api.openai.com/v1', ollama: 'http://127.0.0.1:11434' };
  const MODEL_HINT: Record<EmbeddingProvider, string> = { none: '', openai: 'text-embedding-3-small', ollama: 'nomic-embed-text' };

  let saved = $state<Embeddings | null>(null);
  let load_error = $state('');
  let form = $state({ provider: 'none' as EmbeddingProvider, base_url: '', model: '', dimensions: '', api_key: '' });
  let errors = $state<{ model?: string; dimensions?: string }>({});
  let saving = $state(false);

  function fill(view: Embeddings) {
    saved = view;
    form = { provider: view.provider, base_url: view.base_url, model: view.model, dimensions: view.dimensions?.toString() ?? '', api_key: '' };
  }

  api.get_embeddings().then(fill, (e) => (load_error = error_text(e)));

  async function save(e: SubmitEvent) {
    e.preventDefault();
    const on = form.provider !== 'none';
    const openai = form.provider === 'openai';
    const dimensions = whole(form.dimensions);
    errors = {
      model: on && !form.model.trim() ? 'Name the embedding model.' : undefined,
      dimensions: openai && Number.isNaN(dimensions) ? 'Use a whole number, or leave it empty.' : undefined,
    };
    if (errors.model || errors.dimensions) return;
    saving = true;
    try {
      fill(
        await api.set_embeddings({
          provider: form.provider,
          base_url: on ? form.base_url.trim() : '',
          model: on ? form.model.trim() : '',
          dimensions: openai ? dimensions : null,
          ...(openai && form.api_key ? { api_key: form.api_key } : {}),
        }),
      );
      toast('success', 'Embeddings saved', on ? 'New chunks are embedded after each sync.' : 'Search uses keywords only.');
    } catch (err) {
      toast('danger', 'Embeddings not saved', error_text(err));
    }
    saving = false;
  }
</script>

<form class="sun-row embed" aria-labelledby="embed-head" onsubmit={save} novalidate>
  <span class="sun-tile"><Icon name="sparkles" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="embed-head">
      Embeddings
      {#if saved}<Badge tone={saved.provider === 'none' ? 'neutral' : 'success'} dot>{saved.provider === 'none' ? 'Off' : 'On'}</Badge>{/if}
    </div>
    <p class="sun-row__meta embed__copy">
      Adds search by meaning on top of keyword search. With a provider on, the text of every chunk is sent to it to be embedded,
      so pick one you trust with your docs.
    </p>
    {#if load_error}
      <div class="sun-empty" role="alert">{load_error}</div>
    {:else if saved}
      <div class="embed__fields">
        <Select label="Provider" bind:value={form.provider} options={PROVIDERS} />
        {#if form.provider !== 'none'}
          <Input label="Base URL" mono placeholder={URL_HINT[form.provider]} hint="Empty uses {URL_HINT[form.provider]}." bind:value={form.base_url} />
          <Input label="Model" mono placeholder={MODEL_HINT[form.provider]} bind:value={form.model} error={errors.model} />
        {/if}
        {#if form.provider === 'openai'}
          <Input label="Dimensions (optional)" type="number" inputmode="numeric" min="1" placeholder="1536" hint="Shorter vectors, for models that support it." bind:value={form.dimensions} error={errors.dimensions} />
          <div class="embed__key">
            <Input
              label="API key"
              type="password"
              autocomplete="off"
              mono
              placeholder={saved.has_api_key ? 'Leave empty to keep the saved key' : 'sk-…'}
              hint="Stored as an ecosystem secret. It is never shown again."
              bind:value={form.api_key}
            />
            {#if saved.has_api_key}<Badge tone="success" dot>Key saved</Badge>{/if}
          </div>
        {/if}
      </div>
      <div class="embed__actions"><Button type="submit" variant="ice" loading={saving}>Save</Button></div>
    {:else}
      <div class="sun-empty" role="status">Loading…</div>
    {/if}
  </div>
</form>

<style>
  .embed__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
  .embed__fields {
    display: grid;
    gap: 14px;
    margin-top: var(--space-4);
    max-width: 480px;
  }
  .embed__key {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-2);
  }
  .embed__key :global(.ml-field) {
    align-self: stretch;
  }
  .embed__actions {
    margin-top: var(--space-4);
  }
</style>
