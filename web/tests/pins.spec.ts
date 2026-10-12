import { expect, test, type Page } from '@playwright/test';

// Canvas pins on the mock API. The test serves the dev server's pages and, like
// the kernel proxy does, appends the bridge script (web/public/preview_bridge.js).
const PAGE = (side: string) => `<!doctype html>
<main data-sun-source="src/Pricing.svelte:42:5" style="font-family:sans-serif">
  <h1 style="margin:0 0 16px">${side}</h1>
  <button style="color:#123456;padding:13px 16px;font-size:16px">Buy now</button>
</main><script src="/preview_bridge.js"></script>`;

// The bridge is fulfilled too: Chrome's local-network check blocks a routed (address-less) page
// from loading a loopback script. On the kernel both come from the same server.
const serve_previews = async (page: Page) => {
  await page.route('**/preview_bridge.js', (route) => route.fulfill({ path: 'public/preview_bridge.js', contentType: 'text/javascript' }));
  await page.route('**/preview/**', (route) =>
    route.fulfill({ contentType: 'text/html', body: PAGE(route.request().url().includes('-after/') ? 'After build' : 'Before build') }),
  );
};

const phone_frame = (page: Page) => page.getByRole('region', { name: /360 px/ }).frameLocator('iframe');
const pin_card = (page: Page, n: number) => page.getByRole('article').filter({ has: page.getByRole('heading', { name: new RegExp(`^Pin ${n}:`) }) });
const overflow = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - innerWidth);

async function pin(page: Page, element: (frame: ReturnType<typeof phone_frame>) => ReturnType<Page['locator']>) {
  const toggle = page.getByRole('button', { name: 'Pin an element' });
  await expect(toggle).toBeEnabled();
  await toggle.click();
  await expect(toggle).toHaveAttribute('aria-pressed', 'true');
  await expect(page.getByText('Click an element in an After frame to pin it.')).toBeVisible();
  await element(phone_frame(page)).click();
  await expect(toggle).toHaveAttribute('aria-pressed', 'false');
}

test('pin an element, ask the agent, change it yourself and batch a request', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 900 });
  await serve_previews(page);
  await page.goto('/?mock=1#/canvas/SUN-139');
  await expect(phone_frame(page).getByRole('heading', { name: 'After build' })).toBeVisible();
  await expect(page.getByText('No pins yet.')).toBeVisible();

  // The bridge reports the clicked element: selector, source from data attributes, computed styles.
  await pin(page, (f) => f.getByRole('button', { name: 'Buy now' }));
  const first = pin_card(page, 1);
  await expect(first).toBeVisible();
  await expect(first.getByText('body > main > button')).toBeVisible();
  await expect(first.getByText('src/Pricing.svelte:42')).toBeVisible();
  await expect(first.getByText('360 px')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Pin 1: button “Buy now”' })).toBeVisible();
  // The click pinned instead of acting, and a click after pinning acts again.
  await expect(page.getByRole('button', { name: 'Pin an element' })).toHaveAttribute('aria-pressed', 'false');

  // Token check against the repo's tokens: the hard-coded colour and 13px padding aren't
  // tokens; 16px is (--space-4), and so is the 16px font size (--text-base: 1rem).
  const tokens = first.getByRole('note');
  await expect(tokens).toContainText('values aren’t design tokens');
  await expect(tokens).toContainText('Checked against src/styles/tokens.css');
  await expect(tokens).toContainText('color: rgb(18, 52, 86) → use --ink');
  await expect(tokens).not.toContainText('font-size');
  await expect(tokens).toContainText('padding-top: 13px');
  await expect(tokens).not.toContainText('padding-left');

  // Ask AI → steer now (SUN-139 runs an agent) → with the agent → done.
  await first.getByLabel('What should change?').fill('Make this the primary button');
  await first.getByRole('button', { name: 'Send now' }).click();
  await expect(page.getByText('Sent to the agent')).toBeVisible();
  await expect(first.getByText('With the agent')).toBeVisible();
  await first.getByRole('button', { name: 'Mark done' }).click();
  await expect(first.getByText('Done', { exact: true })).toBeVisible();

  // Change it myself: the text changes live in the preview and is recorded as a suggested diff.
  await pin(page, (f) => f.getByRole('heading', { name: 'After build' }));
  const second = pin_card(page, 2);
  await second.getByRole('tab', { name: 'Change it myself' }).click();
  await expect(second.getByLabel('Property')).toHaveValue('text');
  await second.getByLabel('New value').fill('Pricing');
  await expect(phone_frame(page).getByRole('heading', { name: 'Pricing' })).toBeVisible();
  await second.getByRole('button', { name: 'Save edit' }).click();
  const diff = second.getByLabel('Suggested diff');
  await expect(diff).toContainText('- text: After build');
  await expect(diff).toContainText('+ text: Pricing');
  await expect(second.getByText('Suggested diff · src/Pricing.svelte:42')).toBeVisible();
  await second.getByRole('button', { name: 'Undo live change' }).click();
  await expect(phone_frame(page).getByRole('heading', { name: 'After build' })).toBeVisible();

  // Batch it, then Send all.
  await second.getByRole('tab', { name: 'Ask AI' }).click();
  await second.getByLabel('What should change?').fill('Use the display face for this heading');
  await second.getByRole('button', { name: 'Add to batch' }).click();
  await expect(second.getByText('In batch')).toBeVisible();
  await page.getByRole('button', { name: 'Send all (1)' }).click();
  await expect(second.getByText('With the agent')).toBeVisible();
  await expect(page.getByRole('button', { name: /Send all/ })).toHaveCount(0);

  // Remove a pin.
  await first.getByRole('button', { name: 'Remove pin 1' }).click();
  await expect(page.getByRole('article')).toHaveCount(1);
});

test('bridge messages from anywhere but a preview frame, or of the wrong shape, are ignored', async ({ page }) => {
  await serve_previews(page);
  await page.goto('/?mock=1#/canvas/SUN-139');
  await expect(phone_frame(page).getByRole('heading', { name: 'After build' })).toBeVisible();
  const target = { selector: 'h1', tag: 'h1', text: 'x', width: 360, rect: { x: 0, y: 0, width: 1, height: 1 }, styles: {}, source: null };
  await page.evaluate((t) => window.postMessage({ sun: 'picked', target: t }, '*'), target);
  const frame = page.frames().find((f) => f.url().includes('/preview/'))!;
  await frame.evaluate((t) => parent.postMessage({ sun: 'picked', target: { ...t, selector: 'x'.repeat(5000) } }, '*'), target);
  await frame.evaluate((t) => parent.postMessage({ sun: 'picked', target: { ...t, onclick: 1, rect: null } }, '*'), target);
  await page.waitForTimeout(300);
  await expect(page.getByText('No pins yet.')).toBeVisible();
  // The real one goes through.
  await frame.evaluate((t) => parent.postMessage({ sun: 'picked', target: t }, '*'), target);
  await expect(pin_card(page, 1)).toBeVisible();
});

test('without a running agent the request is queued for the next agent step; phones fit', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await serve_previews(page);
  await page.goto('/?mock=1#/canvas/SUN-145');
  await expect(phone_frame(page).getByRole('heading', { name: 'After build' })).toBeVisible();
  await pin(page, (f) => f.getByRole('button', { name: 'Buy now' }));
  const card = pin_card(page, 1);
  await card.getByLabel('What should change?').fill('Primary colour please');
  await card.getByRole('button', { name: 'Send now' }).click();
  await expect(page.getByText('Pin queued for the next agent step')).toBeVisible();
  await expect(card.getByText('With the agent')).toBeVisible();
  await expect(page.getByRole('button', { name: /Send all/ })).toHaveCount(0);
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

// A 2×2 PNG for the crops the host would take (the mock answers with blob names, the test with pixels).
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGP4z8DAwMDAxMDAwMDAAAANHQEDasKb6QAAAABJRU5ErkJggg==', 'base64');
const loaded = (img: ReturnType<Page['locator']>) => img.evaluate((el) => (el as HTMLImageElement).complete && (el as HTMLImageElement).naturalWidth > 0);

test('a done pin shows both crops; an edit commits to the branch or goes to the AI; screens attach to review', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 900 });
  await serve_previews(page);
  await page.route('**/api/v1/tasks/*/pins/*/crops/*', (route) => route.fulfill({ contentType: 'image/png', body: PNG }));
  await page.goto('/?mock=1#/canvas/SUN-139');
  await expect(phone_frame(page).getByRole('heading', { name: 'After build' })).toBeVisible();

  // The pin carries a crop of the element; no After until it's done.
  await pin(page, (f) => f.getByRole('button', { name: 'Buy now' }));
  const first = pin_card(page, 1);
  const before = first.getByRole('img', { name: 'Pin 1 when it was pinned' });
  await expect(before).toBeVisible();
  await expect.poll(() => loaded(before)).toBe(true);
  await expect(first.getByRole('img', { name: 'Pin 1 after the change' })).toHaveCount(0);

  // A text edit maps to its source line, so it's committed under your name; the pin is done.
  await first.getByRole('tab', { name: 'Change it myself' }).click();
  await first.getByLabel('New value').fill('Buy it now');
  await first.getByRole('button', { name: 'Save edit' }).click();
  await first.getByRole('button', { name: 'Commit to branch' }).click();
  await expect(page.getByText(/Committed [0-9a-f]{7} to the task branch/)).toBeVisible();
  await expect(first.getByText('Done', { exact: true })).toBeVisible();
  // The done pin shows the element before and after.
  const after = first.getByRole('img', { name: 'Pin 1 after the change' });
  await expect(after).toBeVisible();
  await expect.poll(() => loaded(after)).toBe(true);
  await expect(first.getByRole('group', { name: 'Pin 1 before and after' }).getByRole('figure')).toHaveCount(2);

  // A computed colour isn't in the source as-is: Sun says so and the AI takes it.
  await pin(page, (f) => f.getByRole('heading', { name: 'After build' }));
  const second = pin_card(page, 2);
  await second.getByRole('tab', { name: 'Change it myself' }).click();
  await second.getByLabel('Property').selectOption('color');
  await second.getByLabel('New value').fill('rgb(1, 2, 3)');
  await second.getByRole('button', { name: 'Save edit' }).click();
  await second.getByRole('button', { name: 'Commit to branch' }).click();
  await expect(page.getByText('Sun can’t commit this edit as it is')).toBeVisible();
  await expect(page.getByText(/Hand the edit to the agent instead/)).toBeVisible();
  await second.getByRole('button', { name: 'Hand to AI' }).click();
  await expect(second.getByText('With the agent')).toBeVisible();

  // The shown breakpoints, before and after, go to the task and its PR.
  await page.getByRole('button', { name: 'Attach to review' }).click();
  await expect(page.getByText('6 screenshots attached')).toBeVisible();

  // Phones: the crops fit.
  await page.setViewportSize({ width: 360, height: 740 });
  await expect(before).toBeVisible();
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});
