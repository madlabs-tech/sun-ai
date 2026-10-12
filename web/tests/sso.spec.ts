import { expect, test, type Page } from '@playwright/test';

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test('settings adds a GitHub sign-in provider that the login page offers (360 px)', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/settings/sign-in');
  const section = page.getByRole('region', { name: 'Sign-in' });
  const list = section.getByRole('list', { name: 'Sign-in providers' });
  await expect(list.getByRole('listitem')).toHaveCount(1);
  await expect(list).toContainText('Keycloak');
  await expect(list).toContainText('Only acme.io');

  await section.getByRole('button', { name: 'Add provider' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Add sign-in provider' });
  await dialog.getByLabel('Provider').selectOption('github');
  await expect(dialog.getByLabel('Button name')).toHaveValue('GitHub');
  await expect(dialog.getByLabel('Issuer URL')).toHaveCount(0);
  await expect(dialog.getByRole('textbox', { name: 'Callback URL' })).toHaveValue(/\/api\/v1\/auth\/sso\/github\/callback$/);
  await dialog.getByRole('button', { name: 'Save provider' }).click();
  await expect(dialog.getByText('Enter the client id from the provider.')).toBeVisible();
  await expect(dialog.getByText('Enter the client secret from the provider.')).toBeVisible();
  await dialog.getByLabel('Client id').fill('Iv1.abc');
  await dialog.getByLabel('Client secret').fill('shh');
  await no_overflow(page);
  await dialog.getByRole('button', { name: 'Save provider' }).click();
  await expect(dialog).toBeHidden();
  await expect(list.getByRole('listitem')).toHaveCount(2);
  await expect(list).toContainText('Any invited email');

  // Editing keeps the saved secret; the generic preset asks for its issuer.
  await section.getByRole('button', { name: 'Edit Keycloak' }).click();
  const edit = page.getByRole('dialog').filter({ hasText: 'Edit Keycloak' });
  await expect(edit.getByLabel('Client secret')).toHaveAttribute('placeholder', 'Leave empty to keep the saved secret');
  await expect(edit.getByRole('textbox', { name: 'Callback URL' })).toHaveValue(/\/api\/v1\/auth\/sso\/keycloak\/callback$/);
  await edit.getByLabel('Issuer URL').fill('not a url');
  await edit.getByRole('button', { name: 'Save provider' }).click();
  await expect(edit.getByText('Enter the issuer URL')).toBeVisible();
  await edit.getByRole('button', { name: 'Cancel' }).click();

  await section.getByRole('button', { name: 'Remove Keycloak' }).click();
  await page.getByRole('dialog').filter({ hasText: 'Remove Keycloak?' }).getByRole('button', { name: 'Remove provider' }).click();
  await expect(list.getByRole('listitem')).toHaveCount(1);

  await page.evaluate(() => (location.hash = '#/login'));
  await expect(page.getByRole('link', { name: 'Continue with GitHub' })).toHaveAttribute('href', '/api/v1/auth/sso/github/login');
  await expect(page.getByRole('link', { name: /Continue with Keycloak/ })).toHaveCount(0);
  await no_overflow(page);
});
