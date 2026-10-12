<!-- Live device view: the screen of the simulator or emulator the task's verify step or
     agent is running, in a phone frame (wallet kit `Phone`), relayed by the kernel over a WebSocket:
     H.264 video decoded with WebCodecs when the browser can, else pictures a few times a second.
     Taps on the screen and the keys/text below go to the device when it takes input (Android, or iOS
     with idb on its host; member role). iOS has no Back key. -->
<script lang="ts">
  import { Badge, Button, Input } from '@hryer/madlabs-design-system/svelte';
  import IconButton from './IconButton.svelte';
  import { watch_device } from '../api/client';
  import type { DeviceInput, DeviceState } from '../api/types';
  import { toast } from '../app_state.svelte';
  import { can_decode_video, video_player } from '../device_video';

  let { task_id }: { task_id: string } = $props();
  // The parent passes `task.id` of a task it reloads on every task list refresh: a derived id only changes
  // when the id does, so the socket isn't reopened (and the screen dropped) on each reload.
  const id = $derived(task_id);

  let device = $state<DeviceState>({ kind: 'connecting' });
  let src = $state<string | null>(null);
  let shown: string | null = null;
  /** The video canvas has a picture. */
  let playing = $state(false);
  let text = $state('');
  let tap = $state<{ x: number; y: number; n: number } | null>(null);
  let link: ReturnType<typeof watch_device> | null = null;
  // Made here so decoding can start before Svelte mounts it.
  const canvas = document.createElement('canvas');
  let player: ReturnType<typeof video_player> | null = null;

  function stop_video() {
    player?.close();
    player = null;
    playing = false;
  }

  $effect(() => {
    link = watch_device(
      id,
      can_decode_video(),
      (s) => {
        device = s;
        if (s.kind !== 'live') src = null;
        if (s.kind !== 'live' || s.mime !== 'video/h264') stop_video();
      },
      (data, mime) => {
        if (mime !== 'video/h264') return (src = URL.createObjectURL(new Blob([data], { type: mime })));
        player ??= video_player(
          canvas,
          () => (playing = true),
          () => {
            stop_video();
            link?.no_video(); // this browser can't decode it: pictures instead
          },
        );
        player.push(data);
      },
      (message) => toast('danger', 'The device did not take that', message),
    );
    return () => {
      link?.close();
      link = null;
      stop_video();
      for (const url of [src, shown]) if (url) URL.revokeObjectURL(url);
      src = shown = null;
    };
  });

  const mount = (node: HTMLElement) => {
    node.append(canvas);
  };

  /** Frees the previous picture once the new one is on screen. */
  function loaded() {
    if (shown && shown !== src) URL.revokeObjectURL(shown);
    shown = src;
  }

  const live = $derived(device.kind === 'live' ? device : null);
  const can_input = $derived(!!live?.input);
  const status = $derived(
    device.kind === 'live'
      ? `Live · ${device.device}`
      : device.kind === 'idle'
        ? 'No device running'
        : device.kind === 'unavailable'
          ? 'Device unavailable'
          : 'Connecting…',
  );

  const send = (input: DeviceInput) => link?.send(input);

  /** A pointer tap on the picture, mapped to device pixels. Keyboard "clicks" (detail 0) carry no point. */
  function on_tap(e: MouseEvent & { currentTarget: HTMLButtonElement }) {
    const img = e.currentTarget.querySelector('img');
    const [screen, width, height] = img ? [img, img.naturalWidth, img.naturalHeight] : [canvas, canvas.width, canvas.height];
    if (!width || e.detail === 0) return;
    const r = screen.getBoundingClientRect();
    const px = e.clientX - r.left;
    const py = e.clientY - r.top;
    send({
      kind: 'tap',
      x: Math.round((px * width) / r.width),
      y: Math.round((py * height) / r.height),
    });
    tap = { x: px, y: py, n: (tap?.n ?? 0) + 1 };
  }

  function type_text(e: SubmitEvent) {
    e.preventDefault();
    if (!text.trim()) return;
    send({ kind: 'text', text });
    text = '';
  }

  const KEYS = [
    ['back', 'arrow-left', 'Back'],
    ['home', 'house', 'Home'],
    ['delete', 'delete', 'Delete'],
    ['enter', 'corner-down-left', 'Enter'],
  ] as const;
  const keys = $derived(live?.platform === 'ios' ? KEYS.filter(([key]) => key !== 'back') : KEYS);
</script>

<div class="device">
  <p class="device__status" role="status" aria-atomic="true">
    {#if live}<Badge tone="info" dot live>Live</Badge>{/if}
    <span>{status}</span>
  </p>

  {#snippet screen(name: string)}
    {#if playing}
      <div class="phone__video" role="img" aria-label={name} {@attach mount}></div>
    {:else}
      <img {src} alt={name} onload={loaded} />
    {/if}
  {/snippet}

  <div class="phone" aria-busy={device.kind === 'connecting' || (live && !src && !playing) ? true : undefined}>
    <div class="phone__screen">
      {#if live && (src || playing)}
        {#if can_input}
          <button type="button" class="phone__tap" aria-label="Tap the device screen" onclick={on_tap}>
            {@render screen(`Live screen of ${live.device}`)}
            {#if tap}
              {#key tap.n}<span class="phone__ripple" style="left:{tap.x}px;top:{tap.y}px" aria-hidden="true"></span>{/key}
            {/if}
          </button>
        {:else}
          {@render screen(`Live screen of ${live.device}`)}
        {/if}
      {:else}
        <p class="phone__empty">
          {#if device.kind === 'idle'}
            The screen shows here while the agent or the verify step has a simulator or emulator up.
          {:else if device.kind === 'unavailable'}
            {device.message}
          {:else}
            Looking for the device…
          {/if}
        </p>
      {/if}
    </div>
  </div>

  {#if live && can_input}
    <div class="device__keys">
      {#each keys as [key, icon, label] (key)}
        <IconButton {icon} {label} size="sm" variant="secondary" onclick={() => send({ kind: 'key', key })} />
      {/each}
    </div>
    <form class="device__type" onsubmit={type_text}>
      <Input label="Type on the device" bind:value={text} maxlength={500} autocomplete="off" size="sm" />
      <Button type="submit" size="sm" variant="secondary" iconLeft="send" disabled={!text.trim()}>Send</Button>
    </form>
  {:else if live}
    <p class="dim device__note">
      View only{live.platform === 'ios'
        ? ': tapping the iOS Simulator needs idb on its host and the member role.'
        : ': your role can watch but not use the device.'}
    </p>
  {/if}
</div>

<style>
  .device {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
  }
  .device__status {
    align-self: stretch;
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0;
    font-size: 13px;
    color: var(--fg-2);
  }
  /* The wallet kit's phone: slate-950 body, 10 px bezel, rounded screen. */
  .phone {
    width: min(240px, 100%);
    padding: 8px;
    border-radius: 40px;
    background: var(--slate-950);
    box-shadow: var(--shadow-lg);
    box-sizing: border-box;
  }
  .phone__screen {
    position: relative;
    aspect-ratio: 9 / 19.5;
    border-radius: 32px;
    overflow: hidden;
    background: var(--surface-card);
    display: grid;
    place-items: center;
  }
  .phone__screen:has(img, .phone__video) {
    aspect-ratio: auto;
    background: var(--slate-950);
  }
  .phone__tap {
    position: relative;
    display: block;
    width: 100%;
    padding: 0;
    border: 0;
    background: none;
    cursor: pointer;
  }
  .phone__tap:focus-visible {
    outline: none;
    box-shadow: inset var(--ring-focus);
  }
  img,
  .phone__video :global(canvas) {
    display: block;
    width: 100%;
    height: auto;
    user-select: none;
    -webkit-user-drag: none;
  }
  .phone__ripple {
    position: absolute;
    width: 28px;
    height: 28px;
    margin: -14px 0 0 -14px;
    border-radius: 50%;
    background: var(--ice-300);
    opacity: 0;
    pointer-events: none;
    animation: ripple 420ms var(--ease-out);
  }
  @keyframes ripple {
    from { transform: scale(0.4); opacity: 0.8; }
    to { transform: scale(1.6); opacity: 0; }
  }
  .phone__empty {
    margin: 0;
    padding: 24px;
    text-align: center;
    font-size: 13px;
    color: var(--fg-2);
  }
  .device__keys {
    display: flex;
    gap: 8px;
  }
  .device__type {
    align-self: stretch;
    display: flex;
    align-items: flex-end;
    gap: 8px;
  }
  .device__type :global(.ml-field) {
    flex: 1;
    min-width: 0;
  }
  .device__note {
    margin: 0;
    font-size: 12px;
    text-align: center;
  }
</style>
