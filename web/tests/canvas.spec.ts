import { expect, test, type Page } from '@playwright/test';

// Design canvas on the mock API. `vite preview` has no /preview/ route, so the test serves
// the dev servers: each side answers with its own page, which names its path.
const serve_previews = async (page: Page) => {
  const hits: string[] = [];
  await page.route('**/preview/**', (route) => {
    const url = route.request().url();
    hits.push(url);
    const side = url.includes('-after/') ? 'After build' : 'Before build';
    const path = '/' + new URL(url).pathname.split('/').slice(3).join('/');
    return route.fulfill({ contentType: 'text/html', body: `<!doctype html><h1>${side}</h1><p>page ${path}</p>` });
  });
  return hits;
};

const overflow = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - innerWidth);
const frames = (page: Page) => page.locator('iframe');

test('canvas shows the task branch at three widths, then before and after side by side', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  const hits = await serve_previews(page);
  await page.goto('/?mock=1#/tasks/SUN-139');
  await page.getByRole('link', { name: 'Open canvas' }).click();
  await expect(page).toHaveURL(/#\/canvas\//);
  await expect(page.getByRole('heading', { name: 'Canvas' })).toBeVisible();
  await expect(page.getByText('SUN-139 · Sync pricing page with new plans')).toBeVisible();

  // After only, every breakpoint, each at its real CSS width and sandboxed without same-origin.
  await expect(frames(page)).toHaveCount(3);
  for (const width of [360, 768, 1280]) {
    const group = page.getByRole('region', { name: new RegExp(`${width} px`) });
    await expect(group.frameLocator('iframe').getByRole('heading', { name: 'After build' })).toBeVisible();
    expect(await group.locator('iframe').evaluate((f) => f.clientWidth)).toBe(width);
  }
  const sandbox = await frames(page).first().getAttribute('sandbox');
  expect(sandbox).toContain('allow-scripts');
  expect(sandbox).not.toContain('allow-same-origin');
  expect(await overflow(page)).toBeLessThanOrEqual(0);

  // Side by side: before (base branch) next to after for each width.
  await page.getByRole('tab', { name: 'Side by side' }).click();
  await expect(frames(page)).toHaveCount(6);
  const phone = page.getByRole('region', { name: /360 px/ });
  await expect(phone.frameLocator('iframe').nth(0).getByRole('heading', { name: 'Before build' })).toBeVisible();
  await expect(phone.frameLocator('iframe').nth(1).getByRole('heading', { name: 'After build' })).toBeVisible();
  const [left, right] = await phone.locator('iframe').evaluateAll((fs) => fs.map((f) => f.getBoundingClientRect().x));
  expect(left).toBeLessThan(right);
  expect(await overflow(page)).toBeLessThanOrEqual(0);

  // Breakpoints toggle; the last one stays on.
  const tablet = page.getByRole('button', { name: 'Tablet 768 px' });
  await tablet.click();
  await expect(tablet).toHaveAttribute('aria-pressed', 'false');
  await expect(frames(page)).toHaveCount(4);

  // Reload fetches the previews again.
  const before_reload = hits.length;
  await page.getByRole('button', { name: 'Reload previews' }).click();
  await expect.poll(() => hits.length).toBeGreaterThanOrEqual(before_reload + 4);

  // Stopping the servers leaves a way to start each side again.
  await page.getByRole('button', { name: 'Stop servers' }).click();
  await expect(page.getByText('Preview servers stopped')).toBeVisible();
  await expect(frames(page)).toHaveCount(0);
  await page.getByRole('button', { name: 'Start' }).first().click();
  await expect(frames(page)).toHaveCount(2);
});

test('canvas shows the repo’s screens side by side: 2 paths × 3 widths', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  const hits = await serve_previews(page);
  await page.goto('/?mock=1#/canvas/SUN-139');
  // `.sun-preview` lists Home (/) and Pricing (/pricing); the first one shows.
  const picker = page.getByRole('group', { name: 'Screens' });
  await expect(picker.getByRole('button', { name: 'Home' })).toHaveAttribute('aria-pressed', 'true');
  await expect(frames(page)).toHaveCount(3);
  await picker.getByRole('button', { name: 'Pricing' }).click();
  await expect(frames(page)).toHaveCount(6);

  for (const [name, path] of [['Home', '/'], ['Pricing', '/pricing']]) {
    const row = page.getByRole('region', { name: `${name} ${path}`, exact: true });
    for (const width of [360, 768, 1280]) {
      const group = row.getByRole('region', { name: new RegExp(`${width} px`) });
      await expect(group.frameLocator('iframe').getByText(`page ${path}`, { exact: true })).toBeVisible();
      expect(await group.locator('iframe').evaluate((f) => f.clientWidth)).toBe(width);
    }
  }
  expect(hits.some((url) => url.endsWith('-after/pricing'))).toBe(true);
  expect(await overflow(page)).toBeLessThanOrEqual(0);

  // Review evidence covers the shown screens: 2 paths × 3 widths, before and after.
  await page.getByRole('button', { name: 'Attach to review' }).click();
  await expect(page.getByText('12 screenshots attached')).toBeVisible();

  // The last screen stays on.
  await picker.getByRole('button', { name: 'Home' }).click();
  await expect(frames(page)).toHaveCount(3);
  await picker.getByRole('button', { name: 'Pricing' }).click();
  await expect(picker.getByRole('button', { name: 'Pricing' })).toHaveAttribute('aria-pressed', 'true');
  await expect(frames(page)).toHaveCount(3);
});

test('canvas on a phone: one 360 px frame that fits the screen', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await serve_previews(page);
  await page.goto('/?mock=1#/canvas/SUN-139');
  await expect(page.getByRole('group', { name: 'Screens' })).toBeVisible();
  await expect(frames(page)).toHaveCount(1);
  await expect(page.frameLocator('iframe').getByRole('heading', { name: 'After build' })).toBeVisible();
  expect(await overflow(page)).toBeLessThanOrEqual(0);
  await page.getByRole('tab', { name: 'Side by side' }).click();
  await expect(frames(page)).toHaveCount(2);
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

test('canvas explains a task without a repo', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-147');
  await expect(page.getByRole('main')).toBeVisible();
  await expect(page.getByRole('link', { name: 'Open canvas' })).toHaveCount(0);
  await page.goto('/?mock=1#/canvas/SUN-147');
  await expect(page.getByText('This task has no repo, so there’s no app to preview.')).toBeVisible();
});
