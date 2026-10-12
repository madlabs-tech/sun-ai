<!-- Settings → Ecosystem → Overview: how this UI reaches the kernel, and which built-in features sun.yaml turned on. -->
<script lang="ts">
  import { Badge, Icon } from '@hryer/madlabs-design-system/svelte';
  import { use_mock } from '../lib/api/client';
  import { app, feature_on } from '../lib/app_state.svelte';

  /** Built-in features, as sun.yaml `features:` names them. */
  const FEATURES = [
    { id: 'knowledge', label: 'Knowledge', icon: 'library', what: 'Sources, search and embeddings for agent context.' },
    { id: 'canvas', label: 'Design canvas', icon: 'monitor-smartphone', what: 'Live previews of a task’s app, with pins.' },
    { id: 'evals', label: 'Evals', icon: 'flask-conical', what: 'Replays past tasks against a memory or workflow change.' },
    { id: 'market', label: 'Marketplace', icon: 'store', what: 'Leases runtime capacity on Sun Cloud.' },
    { id: 'catalog', label: 'Catalog', icon: 'sparkles', what: 'Skills and MCP servers to install.' },
  ];
</script>

<article class="sun-row">
  <span class="sun-tile"><Icon name="plug-zap" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title">Kernel connection</div>
    <div class="sun-row__meta">
      {#if use_mock}
        Sample data in this browser tab. Remove <span class="ml-mono">?mock=1</span> from the URL to use the kernel.
      {:else}
        REST at <span class="ml-mono">/api/v1</span> on this host.
      {/if}
    </div>
  </div>
  {#if use_mock}
    <Badge tone="info" dot>Sample data</Badge>
  {:else if app.error}
    <Badge tone="danger" dot>Unreachable</Badge>
  {:else}
    <Badge tone="success" dot>Connected</Badge>
  {/if}
</article>

<section class="sun-row" aria-labelledby="features-head">
  <span class="sun-tile"><Icon name="toggle-right" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="features-head">Features</div>
    <p class="sun-row__meta features__copy">
      Turn one off in <span class="ml-mono">sun.yaml</span> under <span class="ml-mono">features:</span> (for example
      <span class="ml-mono">knowledge: false</span>) and restart. Its pages, API routes and background work go away.
    </p>
    <ul class="features" aria-label="Features">
      {#each FEATURES as f (f.id)}
        {@const on = feature_on(f.id)}
        <li class="features__item">
          <Icon name={f.icon} size={16} />
          <div class="features__text">
            <span class="features__name">{f.label} <span class="ml-mono features__key">{f.id}</span></span>
            <span class="features__what">{f.what}</span>
          </div>
          <Badge tone={on ? 'success' : 'neutral'} dot>{on ? 'On' : 'Off'}</Badge>
        </li>
      {/each}
    </ul>
  </div>
</section>

<style>
  .features {
    display: grid;
    gap: var(--space-3);
    margin: var(--space-4) 0 0;
    padding: 0;
    list-style: none;
    max-width: 560px;
  }
  .features__item {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    color: var(--fg-2);
  }
  .features__text {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
  }
  .features__name {
    font-size: 14px;
    font-weight: 600;
    color: var(--fg-1);
  }
  .features__key {
    font-size: 11px;
    font-weight: 400;
    color: var(--fg-3);
  }
  .features__what {
    font-size: 13px;
    line-height: 1.5;
  }
  .features__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
</style>
