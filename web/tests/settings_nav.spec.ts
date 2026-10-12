// Settings sub-nav: "My Account", the open workspace's group and "Deployment" (deployment owners only), one page per
// entry at #/settings/<id>, a scope badge per page, entries a role can't use hidden, and #/members moved into Settings.
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, route: string, query = '') => page.goto(`/?mock=1${query}#/${route}`);
const settings_nav = (page: Page) => page.getByRole('navigation', { name: 'Settings' });
/** The open page's title row: its heading and scope badge. */
const page_head = (page: Page, title: string) => page.getByRole('heading', { name: title, exact: true }).locator('..');

// Every entry for the deployment owner, in nav order (General and Members are the workspace's, Overview the deployment's).
const ALL = ['Account', 'General', 'Members', 'Code', 'Secrets', 'Plan', 'Overview', 'Users', 'Audit log', 'Billing', 'Support', 'Sign-in & 2-step', 'Email', 'Deployment secrets', 'Usage limits', 'Embeddings'];
/** The deployment's pages: nobody but its owners sees them. */
const DEPLOYMENT = ['Overview', 'Users', 'Audit log', 'Billing', 'Support', 'Sign-in & 2-step', 'Email', 'Deployment secrets', 'Usage limits', 'Embeddings'];

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test('desktop: the groups with sub-headings, Account first, deep links to each page', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'settings');
  const nav = settings_nav(page);
  // The workspace group is named after the open workspace.
  await expect(nav.getByRole('heading', { level: 2 })).toHaveText(['My Account', 'Main', 'Deployment']);
  await expect(nav.getByRole('heading', { level: 3 })).toHaveText(['Security', 'Agents']);
  await expect(nav.getByRole('link')).toHaveText(ALL);

  // No id opens Account, with Log out and who is signed in.
  await expect(nav.getByRole('link', { name: 'Account' })).toHaveAttribute('aria-current', 'page');
  await expect(page_head(page, 'Account')).not.toContainText('Deployment');
  await expect(page.getByRole('form', { name: 'Account' }).getByText('Signed in as me@acme.io (deployment owner).')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Log out' })).toBeVisible();
  await expect(page.getByRole('main').getByRole('link', { name: 'Settings', exact: true })).toBeHidden(); // the phone back link

  await nav.getByRole('link', { name: 'Members' }).click();
  await expect(page).toHaveURL(/#\/settings\/members(\?|$)/);
  await expect(nav.getByRole('link', { name: 'Members' })).toHaveAttribute('aria-current', 'page');
  await expect(page_head(page, 'Members')).toContainText('Workspace');
  await expect(page.getByRole('region', { name: 'People in Main' })).toBeVisible();
  await nav.getByRole('link', { name: 'Users' }).click();
  await expect(page_head(page, 'Users')).toContainText('Deployment');

  await open(page, 'settings/code');
  await expect(page_head(page, 'Code')).toContainText('Workspace');
  await expect(nav.getByRole('link', { name: 'Code' })).toHaveAttribute('aria-current', 'page');
  await open(page, 'settings/plan');
  await expect(page_head(page, 'Plan')).toContainText('Workspace');
  await expect(page.getByRole('region', { name: /Plan/ })).toBeVisible();
  await open(page, 'settings/nope');
  await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeVisible();
  // Members is no longer in the main sidebar; Settings sits in its footer, marked as the open page.
  const sidebar = page.getByRole('complementary', { name: 'Main' });
  await expect(sidebar.getByRole('link', { name: 'Members' })).toHaveCount(0);
  await expect(sidebar.getByRole('link', { name: 'Settings' })).toHaveAttribute('aria-current', 'page');
});

test('360 px: the list, then a page with a back link, then the list again', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'settings');
  const nav = settings_nav(page);
  await expect(nav.getByRole('link', { name: 'Secrets', exact: true })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeHidden();
  await no_overflow(page);

  await nav.getByRole('link', { name: 'Secrets', exact: true }).click();
  await expect(page_head(page, 'Secrets')).toContainText('Workspace');
  await expect(page.getByRole('region', { name: 'Secrets' })).toBeVisible();
  await expect(nav).toBeHidden();
  await no_overflow(page);

  await page.getByRole('link', { name: 'Settings', exact: true }).click();
  await expect(page).toHaveURL(/#\/settings(\?|$)/);
  await expect(nav.getByRole('link', { name: 'Secrets', exact: true })).toBeVisible();
});

const IDS: Record<string, string> = {
  Code: 'code',
  Overview: 'overview',
  Users: 'users',
  'Audit log': 'audit',
  Billing: 'billing',
  Support: 'support',
  'Sign-in & 2-step': 'sign-in',
  Email: 'email',
  Secrets: 'secrets',
  'Deployment secrets': 'deployment-secrets',
  'Usage limits': 'usage',
  Embeddings: 'embeddings',
};
const without = (...hidden: string[]) => ALL.filter((l) => !hidden.includes(l));

// Code needs edit_workspace and Secrets manage_secrets (an admin); the Deployment group needs a deployment owner.
const ROLES: [who: string, query: string, visible: string[]][] = [
  ['a viewer', '&mock_role=viewer', without('Code', 'Secrets', ...DEPLOYMENT)],
  ['a member', '&mock_role=member', without('Code', 'Secrets', ...DEPLOYMENT)],
  ['a workspace admin', '&mock_role=admin', without(...DEPLOYMENT)],
  ['a workspace admin with knowledge off', '&mock_role=admin&features_off=knowledge', without(...DEPLOYMENT)],
];

for (const [who, query, visible] of ROLES) {
  test(`${who}: the nav lists only usable pages, no deployment ones; each hidden page's deep link opens Account`, async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    const nav = settings_nav(page);
    const hidden = ALL.filter((l) => !visible.includes(l));
    for (const label of hidden) {
      await open(page, `settings/${IDS[label]}`, query);
      await expect(nav.getByRole('link')).toHaveText(visible);
      await expect(nav.getByRole('heading', { level: 2 })).toHaveText(['My Account', 'Main']);
      await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeVisible();
      await expect(nav.getByRole('link', { name: 'Account' })).toHaveAttribute('aria-current', 'page');
      await expect(page.getByRole('heading', { name: label, exact: true })).toHaveCount(0);
    }
    // Same in-app (hash change, no reload), from another page.
    await nav.getByRole('link', { name: 'Members' }).click();
    await expect(page.getByRole('heading', { name: 'Members', exact: true })).toBeVisible();
    await page.evaluate((id) => (location.hash = `#/settings/${id}`), IDS[hidden.at(-1)!]);
    await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeVisible();
  });
}

test('a deployment owner deep link to a gated page opens it once /me arrives', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'settings/audit');
  await expect(page.getByRole('heading', { name: 'Audit log', exact: true })).toBeVisible();
  await expect(settings_nav(page).getByRole('link', { name: 'Audit log' })).toHaveAttribute('aria-current', 'page');
  await expect(settings_nav(page).getByRole('link')).toHaveText(ALL);
});

test('#/members for a viewer opens the workspace members, read only', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'members', '&mock_role=viewer');
  await expect(page).toHaveURL(/#\/settings\/members(\?|$)/);
  const people = page.getByRole('region', { name: 'People in Main' });
  await expect(people.getByRole('article')).toHaveCount(4);
  await expect(people.getByRole('combobox')).toHaveCount(0);
  await expect(page.getByRole('region', { name: 'Invite people' })).toHaveCount(0);
});

test('360 px: #/members opens the Members page with a back link; browser Back returns to the list', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'members');
  await expect(page).toHaveURL(/#\/settings\/members(\?|$)/);
  await expect(page.getByRole('region', { name: 'People in Main' })).toBeVisible();
  await page.getByRole('link', { name: 'Settings', exact: true }).click();
  const nav = settings_nav(page);
  await nav.getByRole('link', { name: 'Usage limits' }).click();
  await expect(page.getByRole('heading', { name: 'Usage limits', exact: true })).toBeVisible();
  await page.goBack();
  await expect(page).toHaveURL(/#\/settings(\?|$)/);
  await expect(nav.getByRole('link', { name: 'Usage limits' })).toBeVisible();
  await no_overflow(page);
});

test('every Settings page loads its icons (no /icons/ miss)', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  const bad: string[] = [];
  page.on('response', (r) => {
    if (r.url().includes('/icons/') && (!r.ok() || !(r.headers()['content-type'] ?? '').includes('svg'))) bad.push(`${r.status()} ${r.url()}`);
  });
  page.on('requestfailed', (r) => {
    if (r.url().includes('/icons/')) bad.push(`failed ${r.url()}`);
  });
  for (const id of ['account', 'workspace', 'members', 'code', 'plan', 'overview', 'users', 'audit', 'billing', 'support', 'sign-in', 'email', 'secrets', 'deployment-secrets', 'usage', 'embeddings']) {
    await open(page, `settings/${id}`);
    await page.waitForLoadState('networkidle');
    await expect(settings_nav(page).locator('[aria-current="page"]')).toHaveAttribute('href', new RegExp(`^#/settings/${id}(\\?|$)`));
  }
  // The phone list shows the chevrons the desktop hides.
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'settings');
  await page.waitForLoadState('networkidle');
  expect(bad).toEqual([]);
});

test('#/members redirects to Settings → Members in place of its history entry', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'approvals');
  await page.evaluate(() => (location.hash = '#/members'));
  await expect(page).toHaveURL(/#\/settings\/members(\?|$)/);
  await expect(page.getByRole('heading', { name: 'Members', exact: true })).toBeVisible();
  await page.goBack();
  await expect(page).toHaveURL(/#\/approvals(\?|$)/);
});
