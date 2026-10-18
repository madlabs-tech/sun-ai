// BLO-167 against the real sun-api (see playwright.config.ts), whose sun.yaml leaves `runtimes.allow_bypass`
// off: the owner's Bypass permissions control shows disabled with the reason, and the API agrees.
import { expect, test } from '@playwright/test';

test('bypass permissions show disabled under the operator ceiling on the real kernel', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/');
  await page.getByLabel('Email').fill('me@example.com');
  await page.getByLabel('Password', { exact: true }).fill('test-token');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('button', { name: 'Sign in' })).toHaveCount(0);

  const policy = page.waitForResponse((r) => r.url().endsWith('/api/v1/settings/bypass') && r.request().method() === 'GET');
  await page.goto('/#/settings/bypass');
  expect(await (await policy).json()).toEqual({ mode: 'off', member_ids: [], allow_bypass: false });
  await expect(page.getByTestId('bypass-ceiling')).toContainText('Turn on runtimes.allow_bypass in sun.yaml first');
  const card = page.getByTestId('bypass-policy');
  await expect(card.getByRole('radio', { name: /Owners only/ })).toBeChecked();
  for (const radio of await card.getByRole('radio').all()) await expect(radio).toBeDisabled();
  await expect(card.getByRole('button', { name: 'Save' })).toHaveCount(0);

  const me = await page.request.get('/api/v1/me');
  expect((await me.json()).can_bypass).toBe(false);
});
