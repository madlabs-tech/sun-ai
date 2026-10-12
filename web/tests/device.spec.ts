import { readFileSync } from 'node:fs';
import { expect, test, type Page, type WebSocketRoute } from '@playwright/test';

// At 360 px: the task page shows the device of a running verify step live in a phone frame,
// and taps, keys and text go back over the same socket. The kernel's socket is faked per test.
test.use({ viewport: { width: 360, height: 740 } });

/** A 1080 × 2340 "screen" (device pixels), so tap coordinates can be checked. */
const SCREEN = '<svg xmlns="http://www.w3.org/2000/svg" width="1080" height="2340"><rect width="1080" height="2340" fill="#0b1630"/></svg>';
const LIVE = { kind: 'live', device: 'Pixel_7_API_34', platform: 'android', input: true, mime: 'image/svg+xml' };

async function fake_device(page: Page, script: (ws: WebSocketRoute, inputs: unknown[]) => void) {
  const inputs: unknown[] = [];
  await page.routeWebSocket(/\/api\/v1\/tasks\/[^/]+\/device(\?|$)/, (ws) => {
    ws.onMessage((m) => inputs.push(JSON.parse(String(m))));
    script(ws, inputs);
  });
  await page.goto('/?mock=1#/tasks/SUN-145');
  return inputs;
}

const overflow = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - innerWidth);

test('a running verify step streams its device and takes taps, keys and text', async ({ page }) => {
  let socket: WebSocketRoute | undefined;
  let connects = 0;
  const inputs = await fake_device(page, (ws) => {
    connects += 1;
    socket = ws;
    ws.send(JSON.stringify(LIVE));
    ws.send(Buffer.from(SCREEN));
  });
  const panel = page.getByRole('main');
  const section = panel.getByRole('region', { name: 'Device', exact: true });
  await expect(section.getByRole('status')).toHaveText(/Live\s*Live · Pixel_7_API_34/);
  const screen = section.getByRole('img', { name: 'Live screen of Pixel_7_API_34' });
  await expect(screen).toBeVisible();
  // Visible is not decoded: until the picture loads it has no natural size (a tap is dropped) and its
  // box still grows to the picture's height, so wait for the decode before measuring.
  await expect.poll(() => screen.evaluate((img: HTMLImageElement) => img.complete && img.naturalWidth)).toBe(1080);

  // A tap in the middle of the picture lands in the middle of the device.
  const box = (await screen.boundingBox())!;
  await screen.click({ position: { x: box.width / 2, y: box.height / 2 } });
  await expect.poll(() => inputs.at(-1)).toEqual({ kind: 'tap', x: expect.closeTo(540, -0.5), y: expect.closeTo(1170, -1) }); // ± 5 device px, about 1.5 CSS px (sub-pixel layout)

  await section.getByRole('button', { name: 'Back' }).click();
  await expect.poll(() => inputs.at(-1)).toEqual({ kind: 'key', key: 'back' });
  await section.getByLabel('Type on the device').fill('hello there');
  await section.getByRole('button', { name: 'Send' }).click();
  await expect.poll(() => inputs.at(-1)).toEqual({ kind: 'text', text: 'hello there' });
  await expect(section.getByLabel('Type on the device')).toHaveValue('');

  // A rejected input is said plainly.
  socket!.send(JSON.stringify({ kind: 'input_failed', message: 'text must be 1–500 printable ASCII characters' }));
  await expect(page.getByText('The device did not take that')).toBeVisible();
  // The panel reloads its task on every task list refresh; the device socket stays the one it opened.
  expect(connects).toBe(1);

  // Phone width: nothing scrolls sideways and the controls are thumb-sized.
  expect(await overflow(page)).toBeLessThanOrEqual(0);
  for (const control of await section.getByRole('button').all()) {
    const b = (await control.boundingBox())!;
    expect(Math.min(b.width, b.height), await control.getAttribute('aria-label') ?? '').toBeGreaterThanOrEqual(43.5);
  }
});

test('no device yet, then a view-only iOS Simulator', async ({ page }) => {
  let socket: WebSocketRoute | undefined;
  let connects = 0;
  await fake_device(page, (ws) => {
    connects += 1;
    socket = ws;
    ws.send(JSON.stringify({ kind: 'idle' }));
  });
  const section = page.getByRole('region', { name: 'Device', exact: true });
  await expect(section.getByRole('status')).toHaveText('No device running');
  // One socket per view: a second one (a remount on load) would take the messages below.
  await page.waitForLoadState('networkidle');
  expect(connects).toBe(1);
  await expect(section.getByText('The screen shows here while the agent or the verify step')).toBeVisible();

  socket!.send(JSON.stringify({ ...LIVE, device: 'iPhone 16 (iOS 18.2)', platform: 'ios', input: false, mime: 'image/svg+xml' }));
  socket!.send(Buffer.from(SCREEN));
  await expect(section.getByRole('img', { name: 'Live screen of iPhone 16 (iOS 18.2)' })).toBeVisible();
  await expect(section.getByText('View only: tapping the iOS Simulator needs idb on its host')).toBeVisible();
  await expect(section.getByRole('button')).toHaveCount(0);

  // With idb on the host the simulator takes taps and keys; iOS has no Back key.
  socket!.send(JSON.stringify({ ...LIVE, device: 'iPhone 16 (iOS 18.2)', platform: 'ios', input: true, mime: 'image/svg+xml' }));
  await expect(section.getByRole('button', { name: 'Tap the device screen' })).toBeVisible();
  await expect(section.getByRole('button', { name: 'Home' })).toBeVisible();
  await expect(section.getByRole('button', { name: 'Back' })).toHaveCount(0);

  socket!.send(JSON.stringify({ kind: 'unavailable', message: 'host mac-mini did not answer' }));
  await expect(section.getByText('host mac-mini did not answer')).toBeVisible();
});

// A canned H.264 stream (Chromium's WebCodecs encoder, 540 × 1170): one access unit per message, red key
// frame, then blue and green pictures.
const UNITS = (JSON.parse(readFileSync(new URL('./fixtures/device_h264.json', import.meta.url), 'utf8')) as string[]).map((b64) =>
  Buffer.from(b64, 'base64'),
);
const VIDEO = { ...LIVE, mime: 'video/h264' };

/** The dominant colour at the middle of the decoded video. */
const centre = (page: Page) =>
  page.evaluate(() => {
    const canvas = document.querySelector<HTMLCanvasElement>('.phone__video canvas');
    if (!canvas?.width) return 'none';
    const [r, g, b] = canvas.getContext('2d')!.getImageData(canvas.width / 2, canvas.height / 2, 1, 1).data;
    return r > g && r > b ? 'red' : b > g ? 'blue' : 'green';
  });

test('the device streams as H.264 video, decoded in the browser within a frame budget', async ({ page }) => {
  const urls: string[] = [];
  let socket: WebSocketRoute | undefined;
  const inputs = await fake_device(page, (ws) => {
    urls.push(ws.url());
    socket = ws;
    ws.send(JSON.stringify(VIDEO));
    ws.send(UNITS[0]);
  });
  const section = page.getByRole('region', { name: 'Device', exact: true });
  const screen = section.getByRole('img', { name: 'Live screen of Pixel_7_API_34' });
  await expect(screen).toBeVisible();
  expect(urls.at(-1)).toMatch(/\/device\?video=h264$/);
  await expect.poll(() => centre(page)).toBe('red');

  // Latency: from the unit leaving the socket to its picture on screen.
  for (const [unit, colour] of [[UNITS[1], 'blue'], [UNITS[2], 'green']] as const) {
    const sent = performance.now();
    socket!.send(unit);
    await expect.poll(() => centre(page), { intervals: [5] }).toBe(colour);
    const latency = performance.now() - sent;
    test.info().annotations.push({ type: 'latency_ms', description: latency.toFixed(0) });
    expect(latency).toBeLessThan(500);
  }

  // Taps land in video pixels.
  const box = (await screen.boundingBox())!;
  await screen.click({ position: { x: box.width / 2, y: box.height / 2 } });
  await expect.poll(() => inputs.at(-1)).toEqual({ kind: 'tap', x: expect.closeTo(270, -0.5), y: expect.closeTo(585, -0.5) });
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

test('a browser without WebCodecs gets pictures', async ({ page }) => {
  // A browser that can't decode never asks for video.
  const urls: string[] = [];
  await page.addInitScript(() => delete (window as { VideoDecoder?: unknown }).VideoDecoder);
  await fake_device(page, (ws) => {
    urls.push(ws.url());
    ws.send(JSON.stringify(LIVE));
    ws.send(Buffer.from(SCREEN));
  });
  const section = page.getByRole('region', { name: 'Device', exact: true });
  await expect(section.getByRole('img', { name: 'Live screen of Pixel_7_API_34' })).toBeVisible();
  expect(urls.filter((url) => url.includes('video'))).toEqual([]);
});

test('a stream the browser cannot decode reconnects for pictures', async ({ page }) => {
  const urls: string[] = [];
  await fake_device(page, (ws) => {
    urls.push(ws.url());
    if (ws.url().endsWith('?video=h264')) {
      ws.send(JSON.stringify(VIDEO));
      // A key frame whose SPS names no real profile.
      ws.send(Buffer.from([0, 0, 0, 1, 0x67, 0xff, 0xff, 0xff, 0, 0, 0, 1, 0x65, 0x88, 0x84]));
    } else {
      ws.send(JSON.stringify(LIVE));
      ws.send(Buffer.from(SCREEN));
    }
  });
  const section = page.getByRole('region', { name: 'Device', exact: true });
  await expect(section.locator('img[alt="Live screen of Pixel_7_API_34"]')).toBeVisible();
  expect(urls[0]).toMatch(/\?video=h264$/);
  expect(urls.at(-1)).toMatch(/\/device$/);
});

test('the device section only shows while a device step runs', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-142');
  await expect(page.getByRole('main').getByRole('heading', { name: 'Steps' })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Device', exact: true })).toHaveCount(0);
});
