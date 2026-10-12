// Settings sub-nav (multica-style): "My Account", the open workspace's group and "Ecosystem", one page per entry at
// #/settings/<id>, a scope badge per page, admin-only entries hidden, and #/members moved into Settings.
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, route: string, query = '') => page.goto(`/?mock=1${query}#/${route}`);
const settings_nav = (page: Page) => page.getByRole('navigation', { name: 'Settings' });
/** The open page's title row: its heading and scope badge. */
const page_head = (page: Page, title: string) => page.getByRole('heading', { name: title, exact: true }).locator('..');

// Every entry for the owner, in nav order (General is the workspace's page, Overview the Ecosystem's).
const ALL = ['Account', 'General', 'Access', 'Code', 'Overview', 'Members', 'Audit log', 'Plan & billing', 'Support', 'Sign-in & 2-step', 'Email', 'Secrets', 'Usage limits', 'Embeddings'];

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test('desktop: the groups with sub-headings, Account first, deep links to each page', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'settings');
  const nav = settings_nav(page);
  // The workspace group is named after the open workspace; the Ecosystem never says "workspace".
  await expect(nav.getByRole('heading', { level: 2 })).toHaveText(['My Account', 'Main', 'Ecosystem']);
  await expect(nav.getByRole('heading', { level: 3 })).toHaveText(['Security', 'Agents']);
  await expect(nav.getByRole('link')).toHaveText(ALL);

  // No id opens Account, with Log out and who is signed in.
  await expect(nav.getByRole('link', { name: 'Account' })).toHaveAttribute('aria-current', 'page');
  await expect(page_head(page, 'Account')).not.toContainText('Ecosystem');
  await expect(page.getByRole('form', { name: 'Account' }).getByText('Signed in as me@acme.io (owner).')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Log out' })).toBeVisible();
  await expect(page.getByRole('main').getByRole('link', { name: 'Settings', exact: true })).toBeHidden(); // the phone back link

  await nav.getByRole('link', { name: 'Members' }).click();
  await expect(page).toHaveURL(/#\/settings\/members(\?|$)/);
  await expect(nav.getByRole('link', { name: 'Members' })).toHaveAttribute('aria-current', 'page');
  await expect(page_head(page, 'Members')).toContainText('Ecosystem');
  await expect(page.getByRole('region', { name: 'People' })).toBeVisible();

  await open(page, 'settings/code');
  await expect(page_head(page, 'Code')).toContainText('Workspace');
  await expect(nav.getByRole('link', { name: 'Code' })).toHaveAttribute('aria-current', 'page');
  await open(page, 'settings/billing');
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
  await expect(nav.getByRole('link', { name: 'Secrets' })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeHidden();
  await no_overflow(page);

  await nav.getByRole('link', { name: 'Secrets' }).click();
  await expect(page_head(page, 'Secrets')).toContainText('Ecosystem');
  await expect(page.getByRole('region', { name: 'Secrets' })).toBeVisible();
  await expect(nav).toBeHidden();
  await no_overflow(page);

  await page.getByRole('link', { name: 'Settings', exact: true }).click();
  await expect(page).toHaveURL(/#\/settings(\?|$)/);
  await expect(nav.getByRole('link', { name: 'Secrets' })).toBeVisible();
});

test('a viewer with view_members sees Members but no admin-only entries; their deep links fall back to Account', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'settings/audit', '&mock_role=viewer&viewer_edits_members=1');
  await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeVisible();
  const nav = settings_nav(page);
  await expect(nav.getByRole('link', { name: 'Members' })).toBeVisible();
  for (const name of ['Access', 'Code', 'Audit log', 'Sign-in & 2-step', 'Email', 'Secrets', 'Usage limits', 'Embeddings']) await expect(nav.getByRole('link', { name })).toHaveCount(0);
  // The workspace's General page is there for everyone (read-only without edit_workspace).
  await expect(nav.getByRole('link', { name: 'General' })).toHaveCount(1);
});

const IDS: Record<string, string> = {
  Access: 'access',
  Code: 'code',
  Members: 'members',
  'Audit log': 'audit',
  'Sign-in & 2-step': 'sign-in',
  Email: 'email',
  Secrets: 'secrets',
  'Usage limits': 'usage',
  Embeddings: 'embeddings',
};
const without = (...hidden: string[]) => ALL.filter((l) => !hidden.includes(l));
// Gated by edit_workspace, manage_settings, manage_secrets or manage_runtimes, which only the mock Admin group grants.
const ADMIN_ONLY = ['Access', 'Code', 'Audit log', 'Sign-in & 2-step', 'Email', 'Secrets', 'Usage limits', 'Embeddings'];

const ROLES: [who: string, query: string, visible: string[]][] = [
  ['a viewer', '&mock_role=viewer', without('Members', ...ADMIN_ONLY)],
  ['a member', '&mock_role=member', without('Members', ...ADMIN_ONLY)],
  ['a viewer with view_members', '&mock_role=viewer&viewer_edits_members=1', without(...ADMIN_ONLY)],
  ['an admin with knowledge off', '&mock_role=admin&features_off=knowledge', without('Embeddings')],
];

for (const [who, query, visible] of ROLES) {
  test(`${who}: the nav lists only usable pages; each hidden page's deep link opens Account`, async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    const nav = settings_nav(page);
    const hidden = ALL.filter((l) => !visible.includes(l));
    for (const label of hidden) {
      await open(page, `settings/${IDS[label]}`, query);
      await expect(nav.getByRole('link')).toHaveText(visible);
      await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeVisible();
      await expect(nav.getByRole('link', { name: 'Account' })).toHaveAttribute('aria-current', 'page');
      await expect(page.getByRole('heading', { name: label, exact: true })).toHaveCount(0);
    }
    // Same in-app (hash change, no reload), from another page.
    await nav.getByRole('link', { name: 'Support' }).click();
    await expect(page.getByRole('heading', { name: 'Support', exact: true })).toBeVisible();
    await page.evaluate((id) => (location.hash = `#/settings/${id}`), IDS[hidden.at(-1)!]);
    await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeVisible();
  });
}

test('an admin deep link to a gated page opens it once /me arrives', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'settings/audit', '&mock_role=admin');
  await expect(page.getByRole('heading', { name: 'Audit log', exact: true })).toBeVisible();
  await expect(settings_nav(page).getByRole('link', { name: 'Audit log' })).toHaveAttribute('aria-current', 'page');
  await expect(settings_nav(page).getByRole('link')).toHaveText(ALL);
});

test('#/members for someone who cannot see members lands on Settings → Account', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'members', '&mock_role=viewer');
  await expect(page).toHaveURL(/#\/settings\/members(\?|$)/);
  await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeVisible();
  await expect(page.getByRole('region', { name: 'People' })).toHaveCount(0);
});

test('360 px: #/members opens the Members page with a back link; browser Back returns to the list', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'members');
  await expect(page).toHaveURL(/#\/settings\/members(\?|$)/);
  await expect(page.getByRole('region', { name: 'People' })).toBeVisible();
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
  for (const id of ['account', 'workspace', 'access', 'code', 'overview', 'members', 'audit', 'billing', 'support', 'sign-in', 'email', 'secrets', 'usage', 'embeddings']) {
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
