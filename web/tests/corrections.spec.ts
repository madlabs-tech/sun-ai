import { expect, test } from '@playwright/test';

// Corrections on the task page, mock API.
test('task page lists corrections with their cause', async ({ page }) => {
  const failed: string[] = [];
  // Device screenshots are kernel blobs the mock doesn't serve (wiring.spec.ts routes them).
  page.on('response', (r) => r.status() >= 400 && !r.url().includes('/screenshots/') && failed.push(r.url()));
  await page.goto('/?mock=1#/tasks/SUN-145');
  const drawer = page.getByRole('main');
  const list = drawer.getByRole('region', { name: /Corrections/ });
  await expect(list).toBeVisible();
  await expect(list.getByRole('heading')).toContainText('2');

  const items = list.locator('.correction');
  await expect(items).toHaveCount(2);
  await expect(items.nth(0)).toContainText('Gate failed');
  await expect(items.nth(0).locator('.ml-badge--danger')).toHaveText('Tests');
  await expect(items.nth(0)).toContainText('Gate `test` failed: FAIL wallet/offline_test.go:58');
  await expect(items.nth(0)).toContainText('verify · claude-code');
  await expect(items.nth(1)).toContainText('Steered');
  await expect(items.nth(1).locator('.ml-badge--warning')).toHaveText('Wrong scope');

  // Multi-line gate output stays folded until asked for.
  const toggle = items.nth(0).getByRole('button', { name: 'Show details' });
  await expect(items.nth(0)).not.toContainText('TestCachedBalance');
  await toggle.click();
  await expect(items.nth(0).getByRole('button', { name: 'Hide details' })).toHaveAttribute('aria-expanded', 'true');
  await expect(items.nth(0)).toContainText('TestCachedBalance: expected 120.50');
  await expect(items.nth(1).getByRole('button', { name: /details/ })).toHaveCount(0);
  // Every kind icon is self-hosted (public/icons).
  expect(failed).toEqual([]);

  // A task nobody corrected shows no section.
  await page.goto('/?mock=1#/tasks/SUN-142');
  await expect(drawer.getByRole('heading', { name: 'Fix login timeout on mobile' })).toBeVisible();
  await expect(drawer.getByRole('region', { name: /Corrections/ })).toHaveCount(0);
});

test('corrections fit a phone without horizontal scroll', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/tasks/SUN-145');
  await expect(page.getByRole('region', { name: /Corrections/ }).locator('.correction')).toHaveCount(2);
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
});
