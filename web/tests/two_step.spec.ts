// 2-step verification on the mock API: its authenticator and mailed codes accept 123456 only;
// cho has an authenticator.
import { expect, test, type Locator, type Page } from '@playwright/test';

const open = (page: Page, route = '', query = '') => page.goto(`/?mock=1${query}#/${route}`);

/** Flips a Madlabs switch by clicking its label (the input itself is hidden). */
const flip = (scope: Locator, name: string) => scope.getByRole('switch', { name }).locator('xpath=..').click();

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

async function sign_in(page: Page, email: string) {
  await page.getByLabel('Email').fill(email);
  await page.getByLabel('Password', { exact: true }).fill('correct horse battery');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
}

/** Adds an authenticator in the open "Add an authenticator app" dialog. */
async function add_authenticator(page: Page) {
  const dialog = page.getByRole('dialog').filter({ hasText: 'Add an authenticator app' });
  await expect(dialog.getByRole('img', { name: 'QR code to scan with your authenticator app' })).toBeVisible();
  await expect(dialog.getByLabel('Or type this key')).toHaveValue('JBSWY3DPEHPK3PXP');
  const code = dialog.getByLabel('6-digit code');
  await expect(code).toHaveAttribute('autocomplete', 'one-time-code');
  await expect(code).toHaveAttribute('inputmode', 'numeric');
  await code.fill('000000');
  await dialog.getByRole('button', { name: 'Turn on' }).click();
  await expect(dialog.getByText('That code is not right.')).toBeVisible();
  await code.fill('123456');
  await dialog.getByRole('button', { name: 'Turn on' }).click();
  await expect(page.getByText('Authenticator added')).toBeVisible();
  await expect(dialog).toBeHidden();
}

test('360 px: a member with an authenticator types its code after the password', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'login');
  await sign_in(page, 'cho@acme.io');
  await expect(page.getByRole('heading', { name: 'Verify it’s you' })).toBeVisible();
  await expect(page.getByRole('tab')).toHaveCount(0);
  const code = page.getByLabel('6-digit code');
  await expect(code).toHaveAttribute('autocomplete', 'one-time-code');
  await no_overflow(page);
  await code.fill('000000');
  await page.getByRole('button', { name: 'Verify' }).click();
  await expect(page.getByText('That email, password or code is not right.')).toBeVisible();
  await code.fill('123456');
  await page.getByRole('button', { name: 'Verify' }).click();
  await expect(page.getByRole('heading', { name: 'Verify it’s you' })).toBeHidden();
  await expect(page.getByRole('link', { name: /SUN-151/ })).toBeVisible();
});

test('add an authenticator in Settings, then a password change asks for it', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'settings/account');
  const mine = page.getByRole('region', { name: '2-step verification' });
  await expect(mine.getByText('No 2-step method yet.')).toBeVisible();
  // Email codes need a mail server.
  await expect(mine.getByRole('button', { name: 'Turn on email codes' })).toBeDisabled();
  await mine.getByRole('button', { name: 'Add authenticator app' }).click();
  await add_authenticator(page);
  await expect(mine.getByRole('list', { name: 'Your 2-step methods' })).toContainText('Authenticator');

  await open(page, 'settings/sign-in');
  const rules = page.getByRole('region', { name: '2-step rules' });
  await flip(rules, 'Ask for it to change a password');
  await expect(page.getByText('2-step rules saved')).toBeVisible();
  await expect(rules.getByRole('switch', { name: 'Ask for it to change a password' })).toBeChecked();

  await open(page, 'settings/account');
  await page.getByLabel('Current password').fill('correct horse battery');
  await page.getByLabel('New password').fill('a brand new password');
  await page.getByLabel('Type it again').fill('a brand new password');
  await page.getByRole('button', { name: 'Change password' }).click();
  const confirm = page.getByRole('dialog').filter({ hasText: 'Confirm it’s you' });
  await confirm.getByLabel('6-digit code').fill('123456');
  await confirm.getByRole('button', { name: 'Change password' }).click();
  await expect(page.getByText('Password changed')).toBeVisible();
  await expect(confirm).toBeHidden();

  await mine.getByRole('button', { name: 'Remove Authenticator' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Remove', exact: true }).click();
  await expect(mine.getByText('No 2-step method yet.')).toBeVisible();
});

test('a required second step: someone without one adds it before going on', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  // The mock is signed in as the owner, who has no second step yet.
  await open(page, '', '&require_two_step=1');
  await expect(page.getByRole('heading', { name: 'Turn on 2-step verification' })).toBeVisible();
  const go_on = page.getByRole('button', { name: 'Continue' });
  await expect(go_on).toBeDisabled();
  await page.getByRole('button', { name: 'Add authenticator app' }).click();
  await add_authenticator(page);
  await no_overflow(page);
  await go_on.click();
  await expect(page.getByRole('heading', { name: 'Turn on 2-step verification' })).toBeHidden();
  await expect(page.getByRole('link', { name: /SUN-151/ })).toBeVisible();
});

test('a deployment owner resets the 2-step of someone who lost their phone', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'settings/users');
  const people = page.getByRole('region', { name: 'Everyone on this deployment' });
  const cho = people.getByRole('article').filter({ hasText: 'cho@acme.io' });
  await expect(cho).toContainText('2-step');
  await people.getByRole('button', { name: 'Reset 2-step of cho' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Reset 2-step of cho?' });
  await dialog.getByRole('button', { name: 'Reset 2-step' }).click();
  await expect(page.getByText('cho’s 2-step reset')).toBeVisible();
  await expect(people.getByRole('button', { name: 'Reset 2-step of cho' })).toBeHidden();
  await expect(cho).not.toContainText('2-step');
  await page.getByRole('navigation', { name: 'Settings' }).getByRole('link', { name: 'Audit log' }).click();
  await expect(page.getByRole('article').first()).toContainText('2-step reset by an admin');
});

/** Presses Tab until `target` has the focus (at most 20 times). */
async function tab_to(page: Page, target: Locator) {
  for (let i = 0; i < 20; i++) {
    if (await target.evaluate((el) => el === document.activeElement)) return;
    await page.keyboard.press('Tab');
  }
  await expect(target).toBeFocused();
}

test('keyboard only: email, password, then the authenticator code', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'login');
  await tab_to(page, page.getByLabel('Email'));
  await page.keyboard.type('cho@acme.io');
  await tab_to(page, page.getByLabel('Password', { exact: true }));
  await page.keyboard.type('correct horse battery');
  await page.keyboard.press('Enter');
  await expect(page.getByRole('heading', { name: 'Verify it’s you' })).toBeVisible();
  const code = page.getByLabel('6-digit code');
  await tab_to(page, code);
  await page.keyboard.type('123456');
  await page.keyboard.press('Enter');
  await expect(page.getByRole('heading', { name: 'Verify it’s you' })).toBeHidden();
  await expect(page.getByRole('link', { name: /SUN-151/ })).toBeVisible();
});

// The password step's button is gone once the second step shows, so the focus moves to the code field rather than
// falling back to the page top (WCAG 2.4.3).
test('the second step moves the focus to its code field', async ({ page }) => {
  await open(page, 'login');
  await sign_in(page, 'cho@acme.io');
  await expect(page.getByRole('heading', { name: 'Verify it’s you' })).toBeVisible();
  await expect(page.getByLabel('6-digit code')).toBeFocused();
});

test('360 px: the 2-step rules and sign-in providers in Settings fit', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'settings/sign-in');
  const rules = page.getByRole('region', { name: '2-step rules' });
  await rules.scrollIntoViewIfNeeded();
  await flip(rules, 'Ask for it to change a password');
  await expect(page.getByText('2-step rules saved')).toBeVisible();
  await expect(rules.getByRole('switch', { name: 'Ask for it to change a password' })).toBeChecked();
  const sign_in_section = page.getByRole('region', { name: 'Sign-in' });
  await expect(sign_in_section.getByRole('list', { name: 'Sign-in providers' })).toContainText('Keycloak');
  await no_overflow(page);
  await sign_in_section.getByRole('button', { name: 'Edit Keycloak' }).click();
  const edit = page.getByRole('dialog').filter({ hasText: 'Edit Keycloak' });
  await expect(edit.getByRole('textbox', { name: 'Callback URL' })).toBeVisible();
  await no_overflow(page);
  await edit.getByRole('button', { name: 'Cancel' }).click();
  await expect(edit).toBeHidden();
});

test('a viewer keeps their own 2-step but sees no deployment sign-in or account controls', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  // An admin-only page falls back to the viewer's own account.
  await open(page, 'settings/sign-in', '&mock_role=viewer');
  await expect(page.getByRole('region', { name: '2-step verification' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Add authenticator app' })).toBeEnabled();
  await expect(page.getByRole('form', { name: 'Account' })).toBeVisible();
  for (const name of ['2-step rules', 'Sign-in']) await expect(page.getByRole('region', { name })).toHaveCount(0);
  await expect(page.getByRole('form', { name: /Email \(SMTP\)/ })).toHaveCount(0);
  await no_overflow(page);

  // The deployment's Users page is hidden too: its deep link opens the viewer's account.
  await open(page, 'settings/users', '&mock_role=viewer');
  await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: /^Reset 2-step of / })).toHaveCount(0);
  await page.getByRole('link', { name: 'Settings', exact: true }).click();
  const nav = page.getByRole('navigation', { name: 'Settings' });
  await expect(nav.getByRole('link', { name: 'Account', exact: true })).toBeVisible();
  for (const name of ['Users', 'Audit log', 'Sign-in & 2-step', 'Email']) await expect(nav.getByRole('link', { name })).toHaveCount(0);
  await no_overflow(page);
});

test('switching the 2-step method moves the focus to the code field', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'login', '&cho_email_code=1');
  await sign_in(page, 'cho@acme.io');
  await expect(page.getByRole('heading', { name: 'Verify it’s you' })).toBeVisible();
  const code = page.getByLabel('6-digit code');
  await expect(code).toBeFocused();
  // By keyboard: focus the Email code tab and press Enter.
  const email = page.getByRole('tab', { name: 'Email code' });
  await email.focus();
  await page.keyboard.press('Enter');
  await expect(email).toHaveAttribute('aria-selected', 'true');
  await expect(page.getByRole('button', { name: 'Email me a code' })).toBeVisible();
  await expect(code).toBeFocused();
  await page.keyboard.type('12');
  // By pointer, back to the authenticator: a fresh, focused field.
  await page.getByRole('tab', { name: 'Authenticator app' }).click();
  await expect(code).toBeFocused();
  await expect(code).toHaveValue('');
  await page.keyboard.type('123456');
  await page.keyboard.press('Enter');
  await expect(page.getByRole('heading', { name: 'Verify it’s you' })).toBeHidden();
  await expect(page.getByRole('link', { name: /SUN-151/ })).toBeVisible();
});
