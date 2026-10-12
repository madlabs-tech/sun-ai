// Catalog against the real sun-api (embedded catalog, ecosystem store).
import { expect, test } from '@playwright/test';

test('catalog: install a curated skill on the real kernel', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  expect((await page.request.post('/api/v1/session', { data: { email: 'me@example.com', password: 'test-token' } })).ok()).toBe(true);
  await page.request.delete('/api/v1/catalog/resolving-merge-conflicts'); // a reused server may have it

  await page.goto('/#/catalog');
  await page.getByRole('tab', { name: /Suggested/ }).click();
  const row = page.getByRole('article').filter({ hasText: 'resolving-merge-conflicts' });
  await row.getByRole('button', { name: 'Install' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByText('name: resolving-merge-conflicts')).toBeVisible();
  await expect(dialog.getByText('LICENSE', { exact: true })).toBeVisible();
  await dialog.getByRole('button', { name: 'Install' }).click();
  await expect(row.getByRole('button', { name: 'Remove' })).toBeVisible();

  await page.getByRole('tab', { name: 'MCP servers' }).click();
  await page.getByRole('tab', { name: /Suggested/ }).click();
  await expect(page.getByRole('article').filter({ has: page.getByText('github', { exact: true }) })).toContainText('api.githubcopilot.com');

  // A role template from GET /templates, added without its mods (none ticked).
  await page.getByRole('tab', { name: 'Workflows' }).click();
  await page.getByRole('tab', { name: /Suggested/ }).click();
  const prd = page.getByRole('article').filter({ hasText: 'prd' });
  if (await prd.isVisible()) {
    // A reused kernel may already have it.
    await prd.getByRole('button', { name: 'Add' }).click();
    await page.getByRole('dialog').getByRole('button', { name: 'Add workflow', exact: true }).click();
    await expect(page.getByRole('dialog')).toBeHidden();
  }
  await page.getByRole('tab', { name: /Installed/ }).click();
  await expect(prd).toContainText('Added');
});

test('catalog: a dismissal is the ecosystem’s and survives a fresh browser', async ({ page, browser }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  expect((await page.request.post('/api/v1/session', { data: { email: 'me@example.com', password: 'test-token' } })).ok()).toBe(true);
  await page.request.delete('/api/v1/catalog/diagnosing-bugs/dismissed'); // a reused server may have it
  await page.request.delete('/api/v1/catalog/diagnosing-bugs');

  await page.goto('/#/catalog');
  await page.getByRole('tab', { name: /Suggested/ }).click();
  const row = (p: typeof page) => p.getByRole('article').filter({ hasText: 'diagnosing-bugs' });
  await page.getByRole('button', { name: 'Dismiss diagnosing-bugs' }).click();
  await expect(row(page)).toHaveCount(0);

  // A new browser context has no storage of its own: the kernel remembers.
  const fresh = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const other = await fresh.newPage();
  expect((await other.request.post('/api/v1/session', { data: { email: 'me@example.com', password: 'test-token' } })).ok()).toBe(true);
  await other.goto('/#/catalog');
  await other.getByRole('tab', { name: /Suggested/ }).click();
  await expect(other.getByRole('article').first()).toBeVisible();
  await expect(row(other)).toHaveCount(0);
  await other.getByRole('button', { name: /^Show \d+ dismissed$/ }).click();
  await expect(row(other)).toHaveCount(1);
  await fresh.close();

  await page.reload();
  await page.getByRole('tab', { name: /Suggested/ }).click();
  await expect(row(page)).toHaveCount(1);
});
