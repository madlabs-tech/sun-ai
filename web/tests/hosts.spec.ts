// Hosts screen: kernel use apart from sessions, RSS per session, device fixes, disk, kill, mock API.
import { expect, test, type Page } from '@playwright/test';

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test('hosts: kernel use, RSS per session, missing device tools and disk', async ({ page }) => {
  await page.goto('/?mock=1#/hosts');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Hosts');
  await expect(page.getByRole('article', { name: 'Kernel' })).toContainText('46.0 MB RAM · 0.6% CPU · up 1d 2h');

  const mbp = page.getByRole('article', { name: 'hery-mbp' });
  const sessions = mbp.getByRole('list', { name: 'Live sessions on hery-mbp' }).getByRole('listitem');
  await expect(sessions).toHaveCount(2);
  await expect(sessions.first()).toContainText('612 MB RAM · 38.5% CPU · up 24m');
  await expect(sessions.nth(1)).toContainText('248 MB RAM · 4.2% CPU · up 5m');
  await expect(mbp).toContainText('1.4 GB in 2 task folders');
  await expect(mbp).toContainText('iPhone 16 (iOS 18.2)');
  await expect(mbp).toContainText('Android emulator: No Android system image: run `sdkmanager');
  await expect(page.getByRole('article', { name: 'build-box-01' })).toContainText('410 MB RAM · 0.4% CPU · up 2h 3m');
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
