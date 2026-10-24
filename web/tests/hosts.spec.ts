// Hosts screen: kernel use apart from sessions, RSS per session, device fixes, disk, kill, mock API; the workspaces each
// host serves and who enrolled it; attach and detach for owners, enrollers and members.
import { expect, test, type Page } from '@playwright/test';

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
const sidebar = (page: Page) => page.getByRole('complementary', { name: 'Main' });
/** Creates workspace `name` from the workspace menu and opens its Hosts page in-app (the mock's data lives in the page). */
async function new_workspace_hosts(page: Page, name: string) {
  await sidebar(page).getByRole('button', { name: /^Workspace: / }).click();
  await sidebar(page).getByRole('menu').getByRole('menuitem', { name: 'New workspace' }).click();
  await page.getByRole('dialog').getByLabel('Name').fill(name);
  await page.getByRole('dialog').getByRole('button', { name: 'Create workspace' }).click();
  await expect(sidebar(page).getByRole('button', { name: `Workspace: ${name}` })).toBeVisible();
  await sidebar(page).getByRole('link', { name: 'Hosts' }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Hosts');
}

test('hosts: kernel use, RSS per session, missing device tools and disk', async ({ page }) => {
  await page.goto('/?mock=1#/hosts');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Hosts');
  await expect(page.getByRole('article', { name: 'Kernel' })).toContainText('46.0 MB RAM · 0.6% avg CPU · up 1d 2h');

  const mbp = page.getByRole('article', { name: 'hery-mbp' });
  const sessions = mbp.getByRole('list', { name: 'Live sessions on hery-mbp' }).getByRole('listitem');
  await expect(sessions).toHaveCount(2);
  await expect(sessions.first()).toContainText('612 MB RAM · 38.5% avg CPU · up 24m');
  await expect(sessions.nth(1)).toContainText('248 MB RAM · 4.2% avg CPU · up 5m');
  await expect(mbp).toContainText('1.4 GB in 2 task folders');
  await expect(mbp).toContainText('iPhone 16 (iOS 18.2)');
  await expect(mbp).toContainText('Android emulator: No Android system image: run `sdkmanager');
  await expect(page.getByRole('article', { name: 'build-box-01' })).toContainText('410 MB RAM · 0.4% avg CPU · up 2h 3m');
});

test('hosts: kill a session with a reason', async ({ page }) => {
  await page.goto('/?mock=1#/hosts');
  const mbp = page.getByRole('article', { name: 'hery-mbp' });
  await mbp.getByRole('button', { name: 'Kill session fix on hery-mbp' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('heading', { name: 'Kill this session?' })).toBeVisible();
  await expect(dialog).toContainText('frees 612 MB of RAM');
  await dialog.getByRole('button', { name: 'Kill session' }).click();
  await expect(dialog).toContainText('Say why.');
  await dialog.getByLabel('Reason').fill('Box is out of RAM');
  await dialog.getByRole('button', { name: 'Kill session' }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText('Session killed')).toBeVisible();
  await expect(mbp.getByRole('listitem')).toHaveCount(1);
  await expect(mbp).toContainText('1 session ·');
});

test('task page shows its worktree disk use', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-142');
  const drawer = page.getByRole('main');
  await expect(drawer).toContainText('1.4 GB on hery-mbp');
});

test('360 px: hosts and sessions fit', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/hosts');
  await expect(page.getByRole('article', { name: 'hery-mbp' }).getByRole('listitem').first()).toBeVisible();
  await no_overflow(page);
});

test('hosts: each names the workspaces it serves and who enrolled it', async ({ page }) => {
  await page.goto('/?mock=1#/hosts');
  await expect(page.getByRole('article', { name: 'hery-mbp' })).toContainText('Serves Main · no enroller on record');
  await expect(page.getByRole('article', { name: 'rina-laptop' })).toContainText('Serves Main · enrolled by you');
});

test('hosts: an owner attaches any host to a new workspace and detaches it again', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/tasks');
  await new_workspace_hosts(page, 'Lab');
  await expect(page.getByText('No hosts serve this workspace yet. Add one, or attach one below.')).toBeVisible();
  const others = page.getByRole('region', { name: 'Hosts you can attach' });
  await expect(others.getByRole('article')).toHaveCount(3);
  await others.getByRole('article', { name: 'build-box-01' }).getByRole('button', { name: 'Attach to this workspace' }).click();
  await expect(page.getByText('build-box-01 attached')).toBeVisible();
  const attached = page.getByRole('article', { name: 'build-box-01' }).first();
  await expect(attached).toContainText('Serves Main, Lab');
  await expect(others.getByRole('article')).toHaveCount(2);

  await page.getByRole('button', { name: 'Detach build-box-01' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog).toContainText('Only a deployment owner, whoever enrolled it, or a member of a group it’s shared with can attach it again.');
  await dialog.getByRole('button', { name: 'Detach' }).click();
  await expect(page.getByText('build-box-01 detached')).toBeVisible();
  await expect(others.getByRole('article')).toHaveCount(3);
});

test('hosts: a workspace admin attaches only the hosts they enrolled (360 px)', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1&mock_role=admin#/tasks');
  // In Main an admin adds and detaches hosts, but the kernel's use and killing sessions are the owners'.
  await sidebar(page).getByRole('link', { name: 'Hosts' }).click();
  await expect(page.getByRole('button', { name: 'Add host' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Detach hery-mbp' })).toBeVisible();
  await expect(page.getByRole('article', { name: 'Kernel' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: /^Kill session/ })).toHaveCount(0);

  await new_workspace_hosts(page, 'Lab');
  const others = page.getByRole('region', { name: 'Hosts you can attach' });
  await expect(others).toContainText('Hosts you enrolled, and hosts shared with a group you’re in.');
  await expect(others.getByRole('article')).toHaveCount(1);
  await page.setViewportSize({ width: 360, height: 740 });
  await expect(others.getByRole('article', { name: 'rina-laptop' })).toContainText('serves Main · enrolled by you');
  await no_overflow(page);
  await others.getByRole('button', { name: 'Attach to this workspace' }).click();
  await expect(page.getByRole('article', { name: 'rina-laptop' })).toContainText('Serves Main, Lab · enrolled by you');
  await expect(others).toHaveCount(0);
  await no_overflow(page);
});

test('hosts: a member reads the workspace’s hosts and changes nothing', async ({ page }) => {
  await page.goto('/?mock=1&mock_role=member#/hosts');
  await expect(page.getByRole('article', { name: 'hery-mbp' })).toContainText('Serves Main');
  for (const name of ['Add host', 'Detach hery-mbp', /^Kill session/]) await expect(page.getByRole('button', { name })).toHaveCount(0);
  await expect(page.getByRole('region', { name: 'Hosts you can attach' })).toHaveCount(0);
});

test('hosts: add host shows the install line for Linux (with sudo) and macOS', async ({ page }) => {
  await page.goto('/?mock=1#/hosts');
  await page.getByRole('button', { name: 'Add host' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByLabel('No sun on the machine yet? Install it on Linux')).toHaveValue(
    'curl -fsSL https://get-sun.madlabs.tech | sudo sh -s -- --no-service',
  );
  await expect(dialog.getByLabel('Or on macOS')).toHaveValue('curl -fsSL https://get-sun.madlabs.tech | sh -s -- --no-service');
});
