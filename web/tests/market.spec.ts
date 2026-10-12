import { expect, test, type Page } from '@playwright/test';

// Marketplace runtime capacity, buyer side, mock API.
const overflow = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - innerWidth);

test('rent capacity for a waiting task after the disclosure, then accept', async ({ page }) => {
  await page.goto('/?mock=1#/market');
  const sale = page.getByRole('region', { name: 'Capacity for sale' });
  await expect(sale.getByRole('article')).toHaveCount(4);
  await expect(sale.getByRole('article').filter({ hasText: 'claude-opus-4.1' })).toContainText('May break the provider’s terms');

  await sale.getByRole('article').filter({ hasText: 'claude-sonnet-4.5' }).getByRole('button', { name: 'Rent' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Rent claude-sonnet-4.5' });
  await dialog.getByLabel('Task', { exact: true }).selectOption({ label: 'Refactor checkout button states' });
  await expect(dialog.getByLabel('Most you pay per task')).toHaveValue('120');
  const rent = dialog.getByRole('button', { name: 'Rent' });
  await expect(rent).toBeDisabled();
  await dialog.getByText('The seller’s machine can see this task’s code and data').click();
  await rent.click();
  await expect(page.getByText('Capacity rented')).toBeVisible();

  const rentals = page.getByRole('region', { name: 'Your rentals' });
  const row = rentals.getByRole('article').filter({ hasText: 'Refactor checkout button states' });
  await expect(row).toContainText('Running');
  await expect(row).toContainText('Points held');
  await row.getByRole('button', { name: 'Accept' }).click();
  await expect(row).toContainText('Paid to seller');
  await expect(row.getByRole('button', { name: 'Accept' })).toHaveCount(0);
});

test('dispute a finished rental at 360 px', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 780 });
  await page.goto('/?mock=1#/market');
  const row = page.getByRole('region', { name: 'Your rentals' }).getByRole('article').first();
  await expect(row).toContainText('Bump wallet SDK to v2.3');
  await expect(row).toContainText('unless you dispute');
  await row.getByRole('button', { name: 'Dispute' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Dispute this work' });
  const send = dialog.getByRole('button', { name: 'Send dispute' });
  await expect(send).toBeDisabled();
  await dialog.getByLabel('What went wrong?').fill('No pull request was opened.');
  await send.click();
  await expect(row).toContainText('Disputed');
  await expect(row).toContainText('Dispute: No pull request was opened.');
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

test('filter capacity, allow one secret, rate the rental', async ({ page }) => {
  await page.goto('/?mock=1#/market');
  const sale = page.getByRole('region', { name: 'Capacity for sale' });
  await expect(sale.getByRole('article')).toHaveCount(4);
  await expect(sale.getByRole('article').filter({ hasText: 'qwen3-coder-30b' })).toContainText('Verified host');
  await page.getByLabel('Capability').selectOption({ label: 'iOS Simulator' });
  await expect(sale.getByRole('article')).toHaveCount(1);
  await expect(sale.getByRole('article')).toContainText('pts / contract (72 h)');
  await page.getByLabel('Capability').selectOption({ label: 'Any' });
  await page.getByLabel('Rating').selectOption({ label: '4.5 and up' });
  await expect(sale.getByRole('article')).toHaveCount(2);
  await page.getByLabel('Rating').selectOption({ label: 'Any' });

  await sale.getByRole('article').filter({ hasText: 'claude-opus-4.1' }).getByRole('button', { name: 'Rent' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Rent claude-opus-4.1' });
  const token = dialog.getByLabel('GITHUB_TOKEN');
  await expect(token).not.toBeChecked();
  await dialog.getByText('GITHUB_TOKEN', { exact: true }).click();
  await expect(token).toBeChecked();
  await dialog.getByText('The seller’s machine can see this task’s code and data').click();
  await dialog.getByRole('button', { name: 'Rent' }).click();
  await expect(page.getByText('Capacity rented')).toBeVisible();

  const done = page.getByRole('region', { name: 'Your rentals' }).getByRole('article').filter({ hasText: 'Bump wallet SDK to v2.3' });
  await done.getByRole('button', { name: 'Rate' }).click();
  const rate = page.getByRole('dialog').filter({ hasText: 'Rate this rental' });
  await rate.getByLabel('Stars').selectOption({ label: '4 of 5' });
  await rate.getByRole('button', { name: 'Save rating' }).click();
  await expect(done).toContainText('You rated 4 of 5');
  await expect(done.getByRole('button', { name: 'Rate' })).toHaveCount(0);
});

test('sell: pause a listing, pay out, and set the ecosystem policy at 360 px', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 780 });
  await page.goto('/?mock=1#/market');
  await page.getByRole('tab', { name: 'Sell' }).click();
  const mine = page.getByRole('region', { name: 'Your listings' }).getByRole('article');
  await expect(mine).toContainText('Live');
  await mine.getByRole('button', { name: 'Pause' }).click();
  await expect(mine).toContainText('Paused');
  await mine.getByRole('button', { name: 'Resume' }).click();
  await expect(mine).toContainText('Live');

  const earnings = page.getByRole('region', { name: 'Earnings' });
  await expect(earnings).toContainText('1840 points ready to pay out');
  await earnings.getByRole('button', { name: 'Pay out' }).click();
  const payout = page.getByRole('dialog').filter({ hasText: 'Pay out earnings' });
  await payout.getByLabel('Points').fill('1000');
  await payout.getByLabel('Destination').fill('acct_1Sample');
  await payout.getByRole('button', { name: 'Request payout' }).click();
  await expect(earnings).toContainText('840 points ready to pay out');
  await expect(earnings).toContainText('1000 pts → 100 USD');
  expect(await overflow(page)).toBeLessThanOrEqual(0);

  await page.getByRole('tab', { name: 'Policy' }).click();
  await expect(page.getByRole('group', { name: 'Workflows', exact: true })).toHaveCount(0);
  await page.getByText('Only tasks of the workflows ticked below.').click();
  await expect(page.getByRole('group', { name: 'Workflows', exact: true })).toBeVisible();
  await page.getByLabel('Failover price cap, points per task').fill('150');
  await page.getByRole('button', { name: 'Save policy' }).click();
  await expect(page.getByText('Policy saved')).toBeVisible();
  // Saved: leaving the tab and coming back keeps it.
  await page.getByRole('tab', { name: 'Rent' }).click();
  await page.getByRole('tab', { name: 'Policy' }).click();
  await expect(page.getByLabel('Failover price cap, points per task')).toHaveValue('150');
  await expect(page.getByRole('radio', { name: /^Chosen workflows/ })).toBeChecked();
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});
