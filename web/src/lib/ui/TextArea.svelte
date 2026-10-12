<!-- Multi-line text field: the DS has no textarea, so this is the DS Input's markup (ml-field / ml-control)
     around a <textarea>. Without `label`, a label elsewhere names it through `id`. -->
<script lang="ts">
  import type { HTMLTextareaAttributes } from 'svelte/elements';

  let {
    value = $bindable(''),
    label,
    hint,
    error,
    mono = false,
    id,
    ...rest
  }: Omit<HTMLTextareaAttributes, 'value'> & { value?: string; label?: string; hint?: string; error?: string; mono?: boolean } = $props();
  const uid = $props.id();
  const fid = $derived(id || uid);
</script>

<div class="ml-field">
  {#if label}<label class="ml-field__label" for={fid}>{label}</label>{/if}
  <div class="ml-control area" class:ml-control--error={error} class:ml-control--mono={mono}>
    <textarea id={fid} class="ml-control__input" bind:value aria-invalid={!!error || undefined} aria-describedby={error || hint ? `${fid}-hint` : undefined} {...rest}></textarea>
  </div>
  {#if error || hint}<div id="{fid}-hint" class="ml-field__hint" class:ml-field__hint--error={error}>{error || hint}</div>{/if}
</div>

<style>
  .area {
    height: auto;
    padding: var(--space-2) var(--space-3);
  }
  .area textarea {
    width: 100%;
    resize: vertical;
    line-height: 1.5;
  }
</style>
