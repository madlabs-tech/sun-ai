// Runtimes screen: permission mode per runtime (PUT /runtimes/{id}/policy) against the in-memory API.
import { expect, test, type Page } from '@playwright/test';

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test('runtimes: set a permission mode; bypass needs a second click', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/runtimes');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Runtimes');

  // No mode set: the host's own default.
  const codex = page.getByRole('combobox', { name: 'Permissions for Codex' });
  await expect(codex).toHaveValue('');
  await expect(page.getByRole('article').filter({ hasText: 'Codex' })).toContainText('From each host’s sun.yaml.');
  await codex.selectOption('plan');
  await expect(page.getByText('Codex: plan only')).toBeVisible();
  await expect(codex).toHaveValue('plan');
  await expect(page.getByRole('article').filter({ hasText: 'Codex' })).toContainText('Reads and plans; changes nothing.');

  // Bypass asks first; cancelling keeps the old mode.
  const claude = page.getByRole('combobox', { name: 'Permissions for Claude Code' });
  await expect(claude).toHaveValue('accept_edits');
  await claude.selectOption('bypass_permissions');
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('heading', { name: 'Let Claude Code run anything?' })).toBeVisible();
  await dialog.getByRole('button', { name: 'Cancel' }).click();
  await expect(dialog).toBeHidden();
  await expect(claude).toHaveValue('accept_edits');

  await claude.selectOption('bypass_permissions');
  await page.getByRole('dialog').getByRole('button', { name: 'Allow anything' }).click();
  await expect(page.getByRole('dialog')).toBeHidden();
  await expect(claude).toHaveValue('bypass_permissions');
});

test('360 px: runtimes fit, the mode select stays usable', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/runtimes');
  const select = page.getByRole('combobox', { name: 'Permissions for OpenCode' });
  await expect(select).toBeVisible();
  await expect(select).toHaveValue('plan');
  expect((await select.boundingBox())?.height ?? 0).toBeGreaterThanOrEqual(32);
  await no_overflow(page);
});
