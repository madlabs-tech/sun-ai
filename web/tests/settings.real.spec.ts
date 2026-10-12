// Against the real sun-api (see playwright.config.ts): the plan from the (stubbed) cloud policy,
// and a secret that goes in but never comes back — not in the DOM, not in any response.
import { expect, test } from '@playwright/test';

test('plan badge and write-only secrets on the real kernel', async ({ page }) => {
  const value = 'real-secret-value-9876';
  const bodies: string[] = [];
  page.on('response', async (r) => {
    if (r.url().includes('/api/v1/')) bodies.push(await r.text().catch(() => ''));
  });

  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/');
  // The owner signs in with their email and the kernel token as the password.
  await page.getByLabel('Email').fill('me@example.com');
  await page.getByLabel('Password', { exact: true }).fill('test-token');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await page.goto('/#/settings/plan');

  const plan = page.getByRole('region', { name: /Plan/ });
  await expect(plan.getByText('Business')).toBeVisible();
  await expect(plan.getByText(/From Sun Cloud, paid/)).toBeVisible();

  await page.goto('/#/settings/secrets');
  const secrets = page.getByRole('region', { name: 'Secrets' });
  await expect(secrets.getByText('No secrets here yet.')).toBeVisible();
  await secrets.getByLabel('Name').fill('UI_TOKEN');
  await secrets.getByLabel('Value').fill(value);
  await secrets.getByRole('button', { name: 'Save secret' }).click();
  await expect(secrets.getByText('UI_TOKEN', { exact: true })).toBeVisible();
  await expect(secrets.getByLabel('Value')).toHaveValue('');
  expect(await page.content()).not.toContain(value);

  await secrets.getByRole('button', { name: 'Delete UI_TOKEN' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Delete secret' }).click();
  await expect(secrets.getByText('No secrets here yet.')).toBeVisible();
  expect(bodies.length).toBeGreaterThan(3);
  expect(bodies.filter((b) => b.includes(value))).toEqual([]);
});
