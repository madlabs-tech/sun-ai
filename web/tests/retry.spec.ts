// Step retry and side effects with no answer, on a phone.
import { expect, test, type Page } from '@playwright/test';

test.use({ viewport: { width: 360, height: 740 } });

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test('retry: a failed step starts again and keeps the finished ones', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-134');
  await expect(page.getByText('The 2 finished steps are kept.')).toBeVisible();
  await no_overflow(page);
  await page.getByRole('button', { name: 'Retry “Gates”' }).click();
  await expect(page.getByText('Retrying Gates', { exact: true })).toBeVisible();
  const steps = page.locator('.steps');
  await expect(steps.getByText('Attempt 2')).toBeVisible();
  await expect(page.getByRole('button', { name: /^Retry “/ })).toHaveCount(0);
  await expect(page.getByText('Retrying Gates (attempt 2) · 2 finished steps kept')).toBeVisible();
});

test('unknown side effect: a person says it went through, or sends it again once', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-132');
  const card = page.getByTestId('unknown-effect');
  await expect(card).toContainText('github.open_pr');
  await expect(card).toContainText('so it won’t send this again on its own');
  await no_overflow(page);

  // Sending again asks first; backing out changes nothing.
  await card.getByRole('button', { name: 'Send again' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Send github.open_pr again?' });
  await expect(dialog).toBeVisible();
  await dialog.getByRole('button', { name: 'Not yet' }).click();
  await expect(card).toBeVisible();

  await card.getByRole('button', { name: 'Send again' }).click();
  await dialog.getByRole('button', { name: 'Send again' }).click();
  await expect(page.getByText('Sending it again', { exact: true })).toBeVisible();
  await expect(page.getByTestId('unknown-effect')).toHaveCount(0);
  await expect(page.getByText('Sent github.open_pr again · delivered')).toBeVisible();
  // The PR step moved on to the review.
  await expect(page.getByRole('button', { name: 'Approve' })).toBeVisible();
});
