// Accounts and workspace membership on the in-memory API: sign-up (open, closed, an email that has an account), the
// invitation link (join, decline, another email, expired, signed out), onboarding for someone in no workspace, the
// workspace menu's invitations, and a workspace's Members page (role ladder, last owner, leave, invite by link).
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, route = '', query = '') => page.goto(`/?mock=1${query}#/${route}`);
const sidebar = (page: Page) => page.getByRole('complementary', { name: 'Main' });
const switcher = (page: Page) => sidebar(page).getByRole('button', { name: /^Workspace: / });
const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test.describe('360 px', () => {
  test.use({ viewport: { width: 360, height: 740 } });

  test('sign-up: email, the mailed code, a password, then the first workspace', async ({ page }) => {
    await open(page, '', '&smtp=1&signed_out=1');
    await page.getByRole('button', { name: 'Create account' }).click();
    await expect(page.getByRole('heading', { name: 'Create your account' })).toBeVisible();
    await page.getByLabel('Email').fill('new.person@acme.io');
    await page.getByRole('button', { name: 'Email me a code' }).click();
    await expect(page.getByRole('heading', { name: 'Check your email' })).toBeVisible();
    await expect(page.getByRole('status')).toContainText('new.person@acme.io');
    await no_overflow(page);
    await page.getByLabel('Code').fill('12');
    await page.getByRole('button', { name: 'Continue' }).click();
    await expect(page.getByText('Enter the 6 digits from the mail.')).toBeVisible();
    await page.getByLabel('Code').fill('123456');
    await page.getByRole('button', { name: 'Continue' }).click();
    await expect(page.getByRole('heading', { name: 'Choose a password' })).toBeVisible();
    await page.getByLabel('New password').fill('a brand new password');
    await page.getByLabel('Type it again').fill('a brand new password');
    await page.getByRole('button', { name: 'Save password' }).click();

    // In no workspace yet: onboarding, with the slug and task keys following the name.
    await expect(page.getByRole('heading', { name: 'Create your first workspace' })).toBeVisible();
    await page.getByLabel('Name').fill('Night Shift');
    await expect(page.getByLabel('Workspace URL')).toHaveValue('night-shift');
    await expect(page.getByText('Task keys: NIGH-1')).toBeVisible();
    await no_overflow(page);
    await page.getByRole('button', { name: 'Create workspace' }).click();
    await expect(page).toHaveURL(/workspace=night-shift$/);
    await expect(page.getByRole('heading', { name: 'Tasks', level: 1 })).toBeVisible();
  });

  test('sign-up with an email that has an account says so and goes back to sign-in', async ({ page }) => {
    await open(page, 'login', '&smtp=1');
    await page.getByRole('button', { name: 'Create account' }).click();
    await page.getByLabel('Email').fill('ana@acme.io');
    await page.getByRole('button', { name: 'Email me a code' }).click();
    const alert = page.getByRole('alert');
    await expect(alert).toHaveText('This email already has an account. Sign in instead.');
    await no_overflow(page);
    await alert.getByRole('button', { name: 'Sign in instead.' }).click();
    await expect(page.getByRole('heading', { name: 'Sign in to this kernel' })).toBeVisible();
    await expect(page.getByLabel('Email')).toHaveValue('ana@acme.io');
  });

  test('closed sign-up: no Create account, and the invite-only copy', async ({ page }) => {
    await open(page, 'login', '&allow_signup=0');
    await expect(page.getByRole('heading', { name: 'Sign in to this kernel' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Create account' })).toHaveCount(0);
    await expect(page.getByText('New here? Only invited people can sign in. Ask a workspace admin for an invitation.')).toBeVisible();
  });

  test('invitation link: what it is, then Join opens the workspace', async ({ page }) => {
    await open(page, 'invite/tok_design', '&invited=1&mock_role=none');
    await expect(page.getByRole('heading', { name: 'Join Design' })).toBeVisible();
    await expect(page.getByText('ana invited me@acme.io to join as Member.')).toBeVisible();
    await no_overflow(page);
    await page.getByRole('button', { name: 'Join Design' }).click();
    await expect(page).toHaveURL(/#\/tasks\?workspace=design$/);
    await expect(page.getByText('You joined Design')).toBeVisible();
  });

  test('invitation link, signed out: sign in below it with the invited email, then join', async ({ page }) => {
    await open(page, 'invite/tok_design', '&invited=1&signed_out=1');
    await expect(page.getByRole('heading', { name: 'Join Design' })).toBeVisible();
    await expect(page.getByLabel('Email')).toHaveValue('me@acme.io');
    // An invitation lets its email sign up even when the deployment's sign-up is closed.
    await expect(page.getByRole('button', { name: 'Create account' })).toBeVisible();
    await no_overflow(page);
    await page.getByLabel('Password', { exact: true }).fill('correct horse battery');
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    await expect(page.getByRole('button', { name: 'Join Design' })).toBeVisible();
    await expect(page).toHaveURL(/#\/invite\/tok_design/);
  });

  test('invitation link: decline, another email, and an expired link', async ({ page }) => {
    await open(page, 'invite/tok_design', '&invited=1');
    await page.getByRole('button', { name: 'Decline' }).click();
    await expect(page.getByText('Invitation declined')).toBeVisible();
    await expect(page).toHaveURL(/#\/tasks/);

    await open(page, 'invite/tok_other', '&invited=1');
    await expect(page.getByRole('heading', { name: 'Join Main' })).toBeVisible();
    await page.getByRole('button', { name: 'Join Main' }).click();
    await expect(page.getByRole('alert')).toHaveText('This invitation is for another email address.');
    await expect(page.getByRole('button', { name: 'Sign in with another account' })).toBeVisible();
    await no_overflow(page);

    await open(page, 'invite/tok_expired');
    await expect(page.getByRole('heading', { name: 'This invitation doesn’t work' })).toBeVisible();
    await expect(page.getByText('Ask whoever invited you for a new link.')).toBeVisible();
    await no_overflow(page);
  });

  test('onboarding: pending invitations first, or a note when only owners create workspaces', async ({ page }) => {
    await open(page, '', '&mock_role=none&invited=1');
    await expect(page.getByRole('heading', { name: 'Create your first workspace' })).toBeVisible();
    await expect(page.getByRole('list', { name: 'Your invitations' })).toContainText('Design');
    await expect(page.getByText('As Member, from ana')).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Or start your own' })).toBeVisible();
    await no_overflow(page);
    await page.getByRole('button', { name: 'Join Design' }).click();
    await expect(page).toHaveURL(/workspace=design/);
    await expect(page.getByRole('heading', { name: 'Tasks', level: 1 })).toBeVisible();

    await open(page, '', '&mock_role=none&creation_disabled=1');
    await expect(page.getByRole('heading', { name: 'Join a workspace' })).toBeVisible();
    await expect(page.getByRole('status')).toContainText('Ask a workspace admin to invite me@acme.io.');
    await expect(page.getByLabel('Name')).toHaveCount(0);
    await no_overflow(page);
  });

  test('members: invite by link without mail, copy it, withdraw it; members are invited once', async ({ page }) => {
    await open(page, 'settings/members');
    const invite = page.getByRole('region', { name: 'Invite people' });
    await invite.getByLabel('Email').fill('ben@acme.io');
    await invite.getByRole('button', { name: 'Invite' }).click();
    await expect(invite.getByText('ben@acme.io is already a member of Main.')).toBeVisible();
    await invite.getByLabel('Email').fill('Zed@Acme.io');
    await invite.getByLabel('Role').selectOption('viewer');
    await invite.getByRole('button', { name: 'Invite' }).click();
    const link = page.getByRole('dialog').filter({ hasText: 'Send the link yourself' });
    await expect(link).toContainText('zed@acme.io didn’t get a mail');
    await expect(link.getByLabel('Invitation link')).toHaveValue(/#\/invite\/tok_\d+$/);
    await no_overflow(page);
    await link.getByRole('button', { name: 'Done' }).click();
    const pending = page.getByRole('region', { name: 'Pending invitations' });
    const zed = pending.getByRole('article', { name: 'zed@acme.io' });
    await expect(zed).toContainText('Viewer');
    await expect(zed.getByRole('button', { name: 'Copy link for zed@acme.io' })).toBeVisible();
    await zed.getByRole('button', { name: 'Withdraw invitation to zed@acme.io' }).click();
    await expect(page.getByText('Invitation to zed@acme.io withdrawn')).toBeVisible();
    await expect(pending.getByRole('article')).toHaveCount(0);
    await no_overflow(page);
  });
});

test.describe('desktop', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('the workspace menu counts invitations and joins from its dialog', async ({ page }) => {
    await open(page, 'tasks', '&invited=1');
    await expect(switcher(page).getByLabel('1 pending invitation')).toBeVisible();
    await switcher(page).click();
    await page.getByRole('menu', { name: 'Account and workspaces' }).getByRole('menuitem', { name: /Invitations/ }).click();
    const dialog = page.getByRole('dialog').filter({ hasText: 'Invitations' });
    await dialog.getByRole('button', { name: 'Join Design' }).click();
    await expect(page.getByText('You joined Design')).toBeVisible();
    await expect(switcher(page)).toHaveAccessibleName('Workspace: Design');
    await expect(switcher(page).getByLabel(/pending invitation/)).toHaveCount(0);
  });

  test('members: an admin grants only roles up to their own', async ({ page }) => {
    await open(page, 'settings/members', '&mock_role=admin');
    const people = page.getByRole('region', { name: 'People in Main' });
    const role = people.getByRole('combobox', { name: 'Role of cho' });
    await expect(role.getByRole('option')).toHaveText(['Admin', 'Member', 'Viewer']);
    await role.selectOption('admin');
    await expect(page.getByText('cho is admin now')).toBeVisible();
    await expect(page.getByRole('region', { name: 'Invite people' }).getByLabel('Role').getByRole('option')).toHaveText(['Admin', 'Member', 'Viewer']);
    // Your own row: your role and Leave, no picker.
    const mine = people.getByRole('article', { name: 'me' });
    await expect(mine).toContainText('Admin');
    await expect(mine.getByRole('combobox')).toHaveCount(0);
  });

  test('members: the last owner can’t leave until someone else is owner; a member can only read', async ({ page }) => {
    await open(page, 'settings/members');
    const people = page.getByRole('region', { name: 'People in Main' });
    const leave = async () => {
      await people.getByRole('article', { name: 'me' }).getByRole('button', { name: 'Leave' }).click();
      await page.getByRole('dialog').filter({ hasText: 'Leave Main?' }).getByRole('button', { name: 'Leave' }).click();
    };
    await leave();
    await expect(page.getByText('A workspace keeps at least one owner: make someone else owner first.')).toBeVisible();
    await people.getByRole('combobox', { name: 'Role of ana' }).selectOption('owner');
    await expect(page.getByText('ana is owner now')).toBeVisible();
    await leave();
    await expect(page.getByText('You left Main')).toBeVisible();

    await open(page, 'settings/members', '&mock_role=member');
    await expect(page.getByRole('region', { name: 'People in Main' }).getByRole('article')).toHaveCount(4);
    await expect(page.getByRole('combobox')).toHaveCount(0);
    await expect(page.getByRole('button', { name: /^Remove / })).toHaveCount(0);
    await expect(page.getByRole('region', { name: 'Invite people' })).toHaveCount(0);
  });

  test('a workspace member sees its agents, runtimes and memory, but no deployment pages', async ({ page }) => {
    await open(page, 'tasks', '&mock_role=admin');
    await expect(sidebar(page).getByRole('heading')).toHaveText(['Work', 'AI Team']);
    for (const name of ['Agents', 'Memory', 'Runtimes']) await expect(sidebar(page).getByRole('link', { name })).toHaveCount(1);
    for (const name of ['Hosts', 'Catalog', 'Mods', 'Marketplace']) await expect(sidebar(page).getByRole('link', { name })).toHaveCount(0);
    await open(page, 'hosts', '&mock_role=admin');
    await expect(page.getByRole('heading', { name: 'Tasks', level: 1 })).toBeVisible();
    await sidebar(page).getByRole('link', { name: 'Settings' }).click();
    await expect(page.getByRole('navigation', { name: 'Settings' }).getByRole('heading', { level: 2 })).toHaveText(['My Account', 'Main']);
  });
});
