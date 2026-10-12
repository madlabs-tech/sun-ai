// Passkeys on the mock API with Chrome's virtual authenticator. Browsers allow passkeys on https or localhost,
// not 127.0.0.1, so these tests open the preview at localhost. The mock checks no signature: a passkey of yours passes.
import { expect, test, type Page, type TestInfo } from '@playwright/test';

/** The page at `localhost` (same server as the base URL) with `route` open. */
const open_local = (page: Page, info: TestInfo, route: string) => {
  const url = new URL(info.project.use.baseURL!);
  url.hostname = 'localhost';
  return page.goto(`${url.origin}/?mock=1#/${route}`);
};

/** A platform authenticator that says yes at once (touch and fingerprint simulated). */
async function virtual_authenticator(page: Page) {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('WebAuthn.enable');
  await cdp.send('WebAuthn.addVirtualAuthenticator', {
    options: { protocol: 'ctap2', transport: 'internal', hasResidentKey: true, hasUserVerification: true, isUserVerified: true },
  });
}

/** Adds a passkey named `name` from the "2-step verification" section. */
async function add_passkey(page: Page, name: string) {
  const mine = page.getByRole('region', { name: '2-step verification' });
  await mine.getByRole('button', { name: 'Add passkey' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Add a passkey' });
  await dialog.getByLabel('Name').fill(name);
  await dialog.getByRole('button', { name: 'Create passkey' }).click();
  await expect(page.getByText('Passkey added')).toBeVisible();
  await expect(dialog).toBeHidden();
  await expect(mine.getByRole('list', { name: 'Your 2-step methods' })).toContainText(name);
}

test('passkeys need https or localhost, and a browser that has them', async ({ page }) => {
  await page.goto('/?mock=1#/settings/account');
  const mine = page.getByRole('region', { name: '2-step verification' });
  await expect(mine.getByRole('button', { name: 'Add passkey' })).toBeDisabled();
  await expect(mine.getByText('Passkeys need Sun on https or localhost.')).toBeVisible();

  await page.addInitScript(() => Reflect.deleteProperty(window, 'PublicKeyCredential'));
  await page.reload();
  await expect(mine.getByRole('button', { name: 'Add authenticator app' })).toBeVisible();
  await expect(mine.getByRole('button', { name: 'Add passkey' })).toHaveCount(0);
});

test('add a passkey in Settings, then a password change asks for it', async ({ page }, info) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await virtual_authenticator(page);
  await open_local(page, info, 'settings/account');
  await add_passkey(page, 'Laptop');
  await page.evaluate(() => (location.hash = '#/settings/sign-in'));
  const rules = page.getByRole('region', { name: '2-step rules' });
  await rules.getByRole('switch', { name: 'Ask for it to change a password' }).locator('xpath=..').click();
  await expect(page.getByText('2-step rules saved')).toBeVisible();

  await page.evaluate(() => (location.hash = '#/settings/account'));
  await page.getByLabel('Current password').fill('correct horse battery');
  await page.getByLabel('New password').fill('a brand new password');
  await page.getByLabel('Type it again').fill('a brand new password');
  await page.getByRole('button', { name: 'Change password' }).click();
  const confirm = page.getByRole('dialog').filter({ hasText: 'Confirm it’s you' });
  await expect(confirm.getByLabel('6-digit code')).toHaveCount(0);
  await confirm.getByRole('button', { name: 'Use passkey' }).click();
  await expect(page.getByText('Password changed')).toBeVisible();
});

test('360 px: sign in with a password, then a passkey', async ({ page }, info) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await virtual_authenticator(page);
  await open_local(page, info, 'login');
  const sign_in = async () => {
    await page.getByLabel('Email').fill('ana@acme.io');
    await page.getByLabel('Password', { exact: true }).fill('correct horse battery');
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  };
  await sign_in();
  await expect(page.getByRole('link', { name: /SUN-151/ })).toBeVisible();
  await page.evaluate(() => (location.hash = '#/settings/account'));
  await add_passkey(page, 'Phone');
  await page.getByRole('button', { name: 'Log out' }).click();

  await sign_in();
  await expect(page.getByRole('heading', { name: 'Verify it’s you' })).toBeVisible();
  await expect(page.getByText('Use the passkey you added for Sun')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
  await page.getByRole('button', { name: 'Use passkey' }).click();
  await expect(page.getByRole('heading', { name: 'Verify it’s you' })).toBeHidden();
  // Back where she signed out.
  await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeVisible();
});
