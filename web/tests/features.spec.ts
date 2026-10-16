import { expect, test, type Page } from '@playwright/test';

// Feature toggles, timeline filter and usage per runtime.
const open = (page: Page, route = '', query = '') => page.goto(`/?mock=1${query}#/${route}`);

test('the tool_call filter shows only tool calls, and a search can come up empty', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page);
  await page.getByRole('link', { name: /SUN-142/ }).click();
  const panel = page.getByRole('main');
  const items = panel.locator('ol.timeline > li');
  await expect(items.first()).toBeVisible();
  const all = await items.count();
  expect(all).toBeGreaterThan(1);

  await panel.getByRole('combobox', { name: 'Event kind' }).selectOption('tool_call');
  await expect(items).toHaveCount(1);
  await expect(items.first()).toContainText('grep "session_timeout" api/');
  await expect(panel.getByRole('status').filter({ hasText: `Showing 1 of ${all} events` })).toBeVisible();

  await panel.getByRole('combobox', { name: 'Event kind' }).selectOption('');
  await panel.getByRole('searchbox', { name: 'Search the timeline' }).fill('SESSION.GO');
  await expect(items).toHaveCount(2); // the tool output and the file change
  await panel.getByRole('searchbox', { name: 'Search the timeline' }).fill('nothing like this');
  await expect(items).toHaveCount(0);
  await panel.getByRole('button', { name: 'Clear the filter' }).click();
  await expect(items).toHaveCount(all);
});

test('with knowledge off, the sidebar and settings drop it', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'knowledge', '&features_off=knowledge');
  const nav = page.getByRole('complementary', { name: 'Main' });
  await expect(nav.getByRole('link', { name: 'Evals' })).toBeVisible();
  await expect(nav.getByRole('link', { name: 'Knowledge' })).toHaveCount(0);
  // An old link to the page lands on Tasks.
  await expect(page.getByRole('heading', { name: 'Needs you', exact: true })).toBeVisible();

  await nav.getByRole('link', { name: 'Settings' }).click();
  const settings_nav = page.getByRole('navigation', { name: 'Settings' });
  await settings_nav.getByRole('link', { name: 'Overview' }).click();
  const features = page.getByRole('list', { name: 'Features' });
  await expect(features.getByRole('listitem').filter({ hasText: 'Knowledge' })).toContainText('Off');
  await expect(features.getByRole('listitem').filter({ hasText: 'Evals' })).toContainText('On');
  await expect(settings_nav.getByRole('link', { name: 'Embeddings' })).toHaveCount(0);
});

test('runtimes show usage, and a subscription its quota window', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'runtimes');
  const claude = page.getByRole('article', { name: 'Claude Code on hery-mbp' });
  await expect(claude).toContainText('All hosts: 1.2M in · 115.5K out · $19.20');
  await expect(claude).toContainText('Quota window, last 5 h: 183.6K in · 16.2K out');
  await expect(page.getByRole('article', { name: 'lan-qwen' })).not.toContainText('Quota window');
});

test('the timeline filter fits a 360 px phone', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page);
  await page.getByRole('link', { name: /SUN-142/ }).first().click();
  const panel = page.getByRole('main');
  await expect(panel.getByRole('searchbox', { name: 'Search the timeline' })).toBeVisible();
  await expect(panel.getByRole('combobox', { name: 'Event kind' })).toBeVisible();
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(0);
});
