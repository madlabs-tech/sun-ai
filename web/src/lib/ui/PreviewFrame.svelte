<!-- One preview at one breakpoint: the app at its real CSS width, scaled to fit the canvas.
     The stage keeps its scaled size before the URL is known (no layout shift); `children` fill it meanwhile.
     Seam for pins: `overlay` renders over the iframe in unscaled page pixels (it gets the same
     transform), and `onframe` hands out the iframe. The app runs sandboxed in an opaque origin
     (no allow-same-origin, also enforced by the kernel's CSP), so pins must talk to it by postMessage. -->
<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    url,
    title,
    width,
    height,
    scale,
    reload = 0,
    children,
    overlay,
    onframe,
  }: {
    url: string | null;
    title: string;
    width: number;
    height: number;
    scale: number;
    /** Bump to reload the iframe. */
    reload?: number;
    children?: Snippet;
    overlay?: Snippet;
    onframe?: (frame: HTMLIFrameElement) => void;
  } = $props();

  let frame = $state<HTMLIFrameElement>();
  $effect(() => {
    if (frame) onframe?.(frame);
  });
</script>

<div class="stage" style="width:{width * scale}px;height:{height * scale}px" aria-busy={url ? undefined : true}>
  {#if url}
    <div class="stage__page" style="width:{width}px;height:{height}px;transform:scale({scale})">
      {#key reload}
        <iframe bind:this={frame} src={url} {title} sandbox="allow-scripts allow-forms allow-popups allow-modals"></iframe>
      {/key}
      {@render overlay?.()}
    </div>
  {:else}
    {@render children?.()}
  {/if}
</div>

<style>
  .stage {
    position: relative;
    overflow: hidden;
    border-radius: var(--radius-lg);
    border: 2px solid var(--border-default);
    background: var(--surface-card);
    box-sizing: content-box;
  }
  .stage__page {
    position: absolute;
    inset: 0 auto auto 0;
    transform-origin: top left;
  }
  iframe {
    display: block;
    width: 100%;
    height: 100%;
    border: 0;
  }
</style>
