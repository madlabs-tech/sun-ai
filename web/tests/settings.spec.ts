import { expect, test } from '@playwright/test';

// At 360 px: the plan badge with license install, and write-only secrets per scope.
test.use({ viewport: { width: 360, height: 740 } });

test('plan badge: self-hosted, a bad license is refused with its reason, a good one shows the plan', async ({ page }) => {
  await page.goto('/?mock=1#/settings/billing');
  const plan = page.getByRole('region', { name: /Plan/ });
  await expect(plan.getByText('Self-hosted')).toBeVisible();
  await plan.getByRole('button', { name: 'Install license' }).click();
  await expect(plan.getByText('Paste the license token.')).toBeVisible();
  await plan.getByLabel('License token').fill('sunlic1.forged');
  await plan.getByRole('button', { name: 'Install license' }).click();
  await expect(plan.getByText('license: not a Sun license token')).toBeVisible();
  await plan.getByLabel('License token').fill('sunlic1.sample');
  await plan.getByRole('button', { name: 'Install license' }).click();
  await expect(plan.getByText('Business')).toBeVisible();
  await expect(plan.getByText(/Licensed to Sample Co/)).toBeVisible();
  await expect(plan.getByRole('button', { name: 'Install license' })).toHaveCount(0);
});

test('secrets: set one, listed without its value, per-mod tab, delete after confirming', async ({ page }) => {
  const value = 'super-secret-value-123';
  await page.goto('/?mock=1#/settings/secrets');
  const secrets = page.getByRole('region', { name: 'Secrets' });
  await expect(secrets.getByText('No secrets here yet.')).toBeVisible();

  await secrets.getByLabel('Name').fill('1BAD');
  await secrets.getByLabel('Value').fill(value);
  await secrets.getByRole('button', { name: 'Save secret' }).click();
  await expect(secrets.getByText(/Use letters, digits and _/)).toBeVisible();
  await secrets.getByLabel('Name').fill('API_TOKEN');
  await secrets.getByRole('button', { name: 'Save secret' }).click();
  await expect(secrets.getByText('API_TOKEN', { exact: true })).toBeVisible();
  await expect(secrets.getByLabel('Value')).toHaveValue('');
  expect(await page.content()).not.toContain(value);

  await secrets.getByRole('tab', { name: 'github' }).click();
  await expect(secrets.getByText('GITHUB_TOKEN', { exact: true })).toBeVisible();
  await expect(secrets.getByText('API_TOKEN')).toHaveCount(0);
  await secrets.getByRole('tab', { name: 'Ecosystem' }).click();

  await secrets.getByRole('button', { name: 'Delete API_TOKEN' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByText('Delete API_TOKEN?')).toBeVisible();
  await dialog.getByRole('button', { name: 'Keep it' }).click();
  await expect(secrets.getByText('API_TOKEN', { exact: true })).toBeVisible();
  await secrets.getByRole('button', { name: 'Delete API_TOKEN' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Delete secret' }).click();
  await expect(secrets.getByText('No secrets here yet.')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
});
