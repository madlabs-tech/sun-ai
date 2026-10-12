import { expect, test } from '@playwright/test';

// Eval replay: past tasks replayed against a candidate change, mock API.
test('an eval report shows original beside replay', async ({ page }) => {
  await page.goto('/?mock=1#/evals');
  const list = page.getByRole('region', { name: /Evals/ });
  const row = list.getByRole('link', { name: 'Tests before done' });
  await expect(row).toContainText('+33 pts');
  await expect(row).toContainText('Done');
  await row.click();

  const result = page.getByRole('region', { name: 'Result' });
  await expect(result).toContainText('33% → 67%');
  await expect(result).toContainText('+33 pts · better');
  await expect(result).toContainText('4 → 1');
  await expect(result).toContainText('−3 · better');
  await expect(page.getByText('recorded transcripts, no model ran')).toBeVisible();
  await expect(page.getByText('Run the tests and make them pass before you finish a step.')).toBeVisible();

  const tasks = page.getByRole('region', { name: 'Tasks' });
  await expect(tasks.getByRole('article')).toHaveCount(3);
  const wallet = tasks.getByRole('article', { name: /wallet balance/ });
  await expect(wallet).toContainText('3 corrections · 2 repeated');
  await expect(wallet).toContainText('Recorded, not sent: github.open_pr');

  await tasks.getByRole('button', { name: 'Save as eval set' }).click();
  await expect(page.getByText('Saved as an eval set')).toBeVisible();

  // The saved set can be picked for the next eval.
  await page.getByRole('link', { name: 'All evals' }).click();
  await page.getByLabel('Tasks to replay', { exact: true }).selectOption('set');
  await expect(page.getByLabel('Eval set')).toContainText('Tests before done (3)');
});

test('starting an eval replays and then reports', async ({ page }) => {
  await page.goto('/?mock=1#/evals');
  await page.getByLabel('Name').fill('Scope rule');
  await page.getByLabel('Cause').selectOption('wrong_scope');
  await page.getByLabel('Candidate instructions').fill('Change only what the task asks for.');
  await expect(page.getByLabel('Run on')).toContainText('Recorded transcripts (free, no model runs)');
  await page.getByRole('button', { name: 'Start eval' }).click();

  await expect(page.getByText('Eval started')).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Scope rule' })).toBeVisible();
  await expect(page.getByText('Replaying', { exact: true })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Result' })).toContainText('33% → 67%', { timeout: 10_000 });
  await expect(page.getByText('Done', { exact: true })).toBeVisible();
  await expect(page.getByText('Change only what the task asks for.')).toBeVisible();
});

test('evals fit a phone without horizontal scroll', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/evals');
  await expect(page.getByRole('button', { name: 'Start eval' })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
  expect((await page.getByRole('button', { name: 'Start eval' }).boundingBox())!.height).toBeGreaterThanOrEqual(36);

  await page.getByRole('link', { name: 'Tests before done' }).click();
  await expect(page.getByRole('region', { name: 'Tasks' }).getByRole('article')).toHaveCount(3);
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
});
