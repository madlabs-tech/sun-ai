// Passkeys against the real sun-api with Chrome's virtual authenticator: the browser's `toJSON()` against the
// kernel's WebAuthn checks. The kernel's public URL is http://localhost:<port> (browsers refuse passkeys on
// 127.0.0.1), so this test opens it there, as a member of its own (the owner's sign-in stays one step).
import { expect, test } from '@playwright/test';

const MEMBER_GROUP = '00000000000000000000000002';

test('a member adds a passkey, then signs in with password and passkey', async ({ page, playwright }, info) => {
  const url = new URL(info.project.use.baseURL!);
  url.hostname = 'localhost';
  const email = `pat${Date.now()}@example.com`;
  const password = 'pat’s long password';

  // The owner creates the member (the token is the owner's password in this kernel).
  const owner = await playwright.request.newContext({ baseURL: url.origin });
  expect((await owner.post('/api/v1/session', { data: { email: 'me@example.com', password: 'test-token' } })).status()).toBe(204);
  const created = await owner.post('/api/v1/members', {
    data: { email, username: `pat${Date.now()}`, group_ids: [MEMBER_GROUP], password, must_change: false },
  });
  expect(created.status()).toBe(201);
  await owner.dispose();

  const cdp = await page.context().newCDPSession(page);
  await cdp.send('WebAuthn.enable');
  await cdp.send('WebAuthn.addVirtualAuthenticator', {
    options: { protocol: 'ctap2', transport: 'internal', hasResidentKey: true, hasUserVerification: true, isUserVerified: true },
  });
  const sign_in = async () => {
    await page.getByLabel('Email').fill(email);
    await page.getByLabel('Password', { exact: true }).fill(password);
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  };

  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto(`${url.origin}/`);
  await sign_in();
  await expect(page.getByRole('heading', { name: 'Sign in to this kernel' })).toBeHidden();
  await page.evaluate(() => (location.hash = '#/settings/account'));
  const mine = page.getByRole('region', { name: '2-step verification' });
  await mine.getByRole('button', { name: 'Add passkey' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Add a passkey' });
  await dialog.getByLabel('Name').fill('Laptop');
  await dialog.getByRole('button', { name: 'Create passkey' }).click();
  await expect(page.getByText('Passkey added')).toBeVisible();
  await expect(mine.getByRole('list', { name: 'Your 2-step methods' })).toContainText('Laptop');

  await page.context().clearCookies();
  await page.goto(`${url.origin}/`);
  await sign_in();
  await expect(page.getByRole('heading', { name: 'Verify it’s you' })).toBeVisible();
  await page.getByRole('button', { name: 'Use passkey' }).click();
  await expect(page.getByRole('heading', { name: 'Verify it’s you' })).toBeHidden();
  await expect(page.getByRole('heading', { name: 'Sign in to this kernel' })).toBeHidden();
});
