import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, route = '') => page.goto(`/?mock=1#/${route}`);

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test('login shows only email, password and the provider, then lands on Tasks', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'login');
  await expect(page.getByRole('heading', { name: 'Sign in to this kernel' })).toBeVisible();
  await expect(page.getByRole('link', { name: 'Continue with Keycloak' })).toHaveAttribute('href', '/api/v1/auth/sso/keycloak/login');
  await expect(page.getByRole('tab')).toHaveCount(0);
  await expect(page.getByLabel('Password', { exact: true })).toBeVisible();
  await no_overflow(page);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByText('Enter your email.')).toBeVisible();
  await page.getByLabel('Email').fill('ana@acme.io');
  await page.getByLabel('Password', { exact: true }).fill('correct horse battery');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Sign in to this kernel' })).toBeHidden();
  await expect(page.getByRole('link', { name: /SUN-151/ })).toBeVisible();
});

test('a failed provider round trip explains itself', async ({ page }) => {
  await open(page, 'login/denied');
  await expect(page.getByRole('alert')).toContainText('email domain isn’t allowed');
});

test('members page changes a role in the workspace and the audit log records it', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'settings/members');
  await expect(page.getByRole('heading', { name: 'Members', exact: true })).toBeVisible();
  const people = page.getByRole('region', { name: 'People in Main' });
  await expect(people.getByRole('article')).toHaveCount(4);
  await expect(people.getByRole('combobox', { name: 'Role of cho' })).toHaveValue('viewer');
  await people.getByRole('combobox', { name: 'Role of cho' }).selectOption('member');
  await expect(page.getByText('cho is member now')).toBeVisible();
  await expect(people.getByRole('combobox', { name: 'Role of cho' })).toHaveValue('member');
  await page.getByRole('navigation', { name: 'Settings' }).getByRole('link', { name: 'Audit log' }).click();
  await expect(page.getByRole('article').first()).toContainText('Main: member');
});

test('360 px: members page fits and is reachable from More, then Settings', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page);
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'More' }).click();
  await page.getByRole('navigation', { name: 'More' }).getByRole('link', { name: 'Settings' }).click();
  await page.getByRole('navigation', { name: 'Settings' }).getByRole('link', { name: 'Members' }).click();
  await expect(page.getByRole('heading', { name: 'Members', exact: true })).toBeVisible();
  await expect(page.getByRole('combobox', { name: 'Role of ana' })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Invite people' })).toBeVisible();
  await no_overflow(page);
});

test('an invited member signs in with email and password; a wrong one gets one plain error', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'login');
  await page.getByLabel('Email').fill('ana@acme.io');
  await page.getByLabel('Password', { exact: true }).fill('not her password');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByText('That email, password or code is not right.')).toBeVisible();
  await expect(page.getByText('Forgot it? Ask an admin to email you a reset code.')).toBeVisible();
  await page.getByLabel('Password', { exact: true }).fill('correct horse battery');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('link', { name: /SUN-151/ })).toBeVisible();
  await no_overflow(page);
});

test('the emailed code goes in the password box; a code sign-in must choose a password first', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'login');
  await page.getByLabel('Email').fill('ben@acme.io');
  await page.getByLabel('Password', { exact: true }).fill('12345');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByText('That email, password or code is not right.')).toBeVisible();
  await page.getByLabel('Password', { exact: true }).fill('123456');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Choose a password' })).toBeVisible();
  await page.getByLabel('New password').fill('short');
  await page.getByLabel('Type it again').fill('short');
  await page.getByRole('button', { name: 'Save password' }).click();
  await expect(page.getByText('Use at least 12 characters.')).toBeVisible();
  await page.getByLabel('New password').fill('a long new password');
  await page.getByLabel('Type it again').fill('a long new password');
  await page.getByRole('button', { name: 'Save password' }).click();
  await expect(page.getByRole('link', { name: /SUN-151/ })).toBeVisible();
  await no_overflow(page);
});

test('uninvited provider sign-ins are told to ask for an invite', async ({ page }) => {
  await open(page, 'login/invite');
  await expect(page.getByRole('alert')).toContainText('Ask an admin for an invite');
});

test('first run asks the owner for their email before anything else', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1&owner_email=0#/');
  await expect(page.getByRole('heading', { name: 'Add your email' })).toBeVisible();
  await page.getByRole('textbox', { name: 'Email' }).fill('owner@acme.io');
  await page.getByRole('button', { name: 'Save email' }).click();
  await expect(page.getByRole('link', { name: /SUN-151/ })).toBeVisible();
});

test('without email set-up an owner adds a user with a password; email codes point to Settings', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'settings/users');
  const people = page.getByRole('region', { name: 'Everyone on this deployment' });
  await people.getByRole('button', { name: 'Add user' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Add user' });
  // No SMTP: "Set password" is picked, and the email-code option can't send. They join the open workspace.
  await expect(dialog.getByRole('tab', { name: 'Set password' })).toHaveAttribute('aria-selected', 'true');
  await dialog.getByLabel('Email').fill('eve@acme.io');
  await dialog.getByLabel('Username').fill('eve');
  await dialog.getByLabel('Password', { exact: true }).fill('short');
  await dialog.getByRole('button', { name: 'Add user' }).click();
  await expect(dialog.getByText('Use at least 12 characters.')).toBeVisible();
  await dialog.getByLabel('Password', { exact: true }).fill('a long first password');
  await expect(dialog.getByRole('checkbox', { name: 'Ask them to change it at first sign-in' })).toBeChecked();
  await expect(dialog.getByRole('combobox', { name: 'Workspace' })).toHaveValue('00000000000000000000000001');
  await dialog.getByRole('combobox', { name: 'Role there' }).selectOption('viewer');
  await dialog.getByRole('button', { name: 'Add user' }).click();
  await expect(page.getByText('eve added')).toBeVisible();
  await expect(people.getByRole('article').filter({ hasText: 'eve@acme.io' })).toContainText('Active');
  await no_overflow(page);

  // Signing in as eve forces a new password first. Log out lives on the Account page.
  await page.evaluate(() => (location.hash = '#/settings/account'));
  await page.getByRole('button', { name: 'Log out' }).click();
  await page.getByLabel('Email').fill('eve@acme.io');
  await page.getByLabel('Password', { exact: true }).fill('a long first password');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Choose a password' })).toBeVisible();
});

test('email invites wait for email set-up, which Settings saves and tests', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'settings/users');
  await page.getByRole('button', { name: 'Add user' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Add user' });
  await dialog.getByRole('tab', { name: 'Email a code' }).click();
  await expect(dialog.getByRole('button', { name: 'Send invite' })).toBeDisabled();
  await dialog.getByRole('link', { name: 'Set up email in Settings' }).click();
  const email = page.getByRole('form', { name: /Email \(SMTP\)/ });
  await expect(email.getByText('Not set up')).toBeVisible();
  await expect(email.getByRole('button', { name: 'Send test mail' })).toBeDisabled();
  await email.getByLabel('Host').fill('smtp.acme.io');
  await email.getByLabel('Password').fill('smtp-secret-1');
  await email.getByLabel('From address').fill('sun@acme.io');
  await email.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Email saved')).toBeVisible();
  await expect(email.getByText('Set up', { exact: true })).toBeVisible();
  await expect(email.getByLabel('Password')).toHaveValue('');
  await email.getByRole('button', { name: 'Send test mail' }).click();
  await expect(page.getByText('Check the inbox of me@acme.io.')).toBeVisible();
  await no_overflow(page);
});

test('an owner adds a user by code, emails a reset code, sets a password, deactivates and reactivates them', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1&smtp=1#/settings/users');
  const people = page.getByRole('region', { name: 'Everyone on this deployment' });
  await people.getByRole('button', { name: 'Add user' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Send invite' });
  await dialog.getByRole('button', { name: 'Send invite' }).click();
  await expect(dialog.getByText('Enter an email such as ana@company.com.')).toBeVisible();
  await dialog.getByLabel('Email').fill('Dee@Acme.io');
  await dialog.getByLabel('Username').fill('dee');
  // Member of the open workspace by default; make them an admin.
  await expect(dialog.getByRole('combobox', { name: 'Role there' })).toHaveValue('member');
  await dialog.getByRole('combobox', { name: 'Role there' }).selectOption('admin');
  await dialog.getByRole('button', { name: 'Send invite' }).click();
  await expect(page.getByText('Invite sent to Dee@Acme.io')).toBeVisible();
  const dee = people.getByRole('article').filter({ hasText: 'dee@acme.io' });
  await expect(dee).toContainText('Invited');
  await dee.getByRole('button', { name: 'Email a reset code to dee' }).click();
  await expect(page.getByText('Reset code sent to dee@acme.io')).toBeVisible();

  await dee.getByRole('button', { name: 'Set password of dee' }).click();
  const set = page.getByRole('dialog').filter({ hasText: 'Set password of dee' });
  await set.getByLabel('New password').fill('admin chose this one');
  await set.getByRole('button', { name: 'Set password' }).click();
  await expect(page.getByText('dee’s password set')).toBeVisible();
  await expect(dee).toContainText('Active');

  await dee.getByRole('button', { name: 'Deactivate dee' }).click();
  await page.getByRole('dialog').filter({ hasText: 'Deactivate dee?' }).getByRole('button', { name: 'Deactivate' }).click();
  await expect(page.getByText('dee deactivated', { exact: true })).toBeVisible();
  await expect(dee).toContainText('Deactivated');
  await expect(people.getByRole('article')).toHaveCount(5);
  await page.evaluate(() => (location.hash = '#/settings/audit'));
  await expect(page.getByRole('article').first()).toContainText('deactivated');
  await page.evaluate(() => (location.hash = '#/settings/users'));
  await dee.getByRole('button', { name: 'Reactivate dee' }).click();
  await expect(page.getByText('dee reactivated', { exact: true })).toBeVisible();
  await expect(dee).toContainText('Active');
  await no_overflow(page);
});
