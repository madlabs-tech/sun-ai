import { expect, test } from '@playwright/test';

// Data touched and post-merge quality on the task page, mock API.
test('task page shows the data an agent touched with a badge', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-139');
  const drawer = page.getByRole('main');
  await expect(drawer.getByRole('heading', { name: /^Execution log/ }).locator('.ml-badge--warning', { hasText: 'Data touched' })).toBeVisible();

  const data = drawer.getByRole('region', { name: /Data touched/ });
  await expect(data.getByRole('heading')).toContainText('2');
  await expect(data).toContainText('Detected from the diff, best effort');
  const rows = data.locator('.touched li');
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(0)).toContainText('Migration');
  await expect(rows.nth(0)).toContainText('db/migrations/20261002_plan_prices.sql');
  await expect(rows.nth(1)).toContainText('Data config');

  // Not merged yet: no after-merge section.
  await expect(drawer.getByRole('region', { name: /After merge/ })).toHaveCount(0);
});

test('merged task shows how it held up and re-checks on demand', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-136');
  const drawer = page.getByRole('main');
  await expect(drawer.getByRole('heading', { name: 'Bump wallet SDK to v2.3' })).toBeVisible();
  await expect(drawer.getByRole('region', { name: /Data touched/ })).toHaveCount(0);

  const merge = drawer.getByRole('region', { name: /After merge/ });
  await expect(merge.locator('.ml-badge--danger')).toHaveText('Needed follow-up');
  await expect(merge).toContainText('9f3c2e1 into main · PR #412');
  await expect(merge).toContainText('1 within 14 days');
  await expect(merge.getByRole('list', { name: 'Fix commits' })).toContainText('fix: price rounding for IDR');

  // The issue that started the task, and a bug reported after the merge.
  const bugs = merge.getByRole('list', { name: 'Bug issues' }).locator('li');
  await expect(bugs).toHaveCount(2);
  await expect(bugs.nth(0)).toContainText('Started task');
  await expect(bugs.nth(0)).toContainText('Wallet SDK v2.2 drops IDR decimals');
  await expect(bugs.nth(1)).toContainText('Regression');
  await expect(bugs.nth(1).getByRole('link', { name: 'madlabs/sun#431' })).toHaveAttribute(
    'href',
    'https://github.com/madlabs/sun/issues/431',
  );
  await merge.getByRole('button', { name: 'Check now' }).click();
  await expect(page.locator('.sun-toasts').getByText('Checked')).toBeVisible();

  // The Tasks header carries the ecosystem metric.
  await page.goto('/?mock=1#/tasks');
  await expect(page.locator('.sun-header')).toContainText('67% of merged work held up (2 of 3)');
});

test('data touched fits a phone without horizontal scroll', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/tasks/SUN-139');
  await expect(page.getByRole('region', { name: /Data touched/ }).locator('.touched li')).toHaveCount(2);
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
});

test('linked bug issues fit a phone without horizontal scroll', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/tasks/SUN-136');
  await expect(page.getByRole('list', { name: 'Bug issues' }).locator('li')).toHaveCount(2);
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
});

// An unverified task suggests the test gate Sun found in its repo, at 360 px too.
test('unverified task suggests a test gate', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/tasks/SUN-133');
  const drawer = page.getByRole('main');
  await expect(drawer).toContainText('Done without a test gate or approval');
  const note = drawer.getByRole('note').filter({ hasText: 'Suggested gate' });
  await expect(note).toContainText('npm test');
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
});
