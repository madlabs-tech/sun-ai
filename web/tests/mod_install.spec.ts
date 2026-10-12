// Installing mods from the Mods screen (POST /mods/{name}/install) against the in-memory API:
// a consent dialog with every permission, write-only secret entry, and official and community registry mods.
import { expect, test, type Page } from '@playwright/test';

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test('install: the consent dialog lists every permission and takes the mod’s secrets', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/mods');
  await page.getByRole('tab', { name: /Available/ }).click();
  await page.getByRole('article').filter({ hasText: 'gitlab' }).getByRole('button', { name: 'Install' }).click();

  const dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('heading', { name: 'Install gitlab' })).toBeVisible();
  const asks = dialog.getByRole('list', { name: 'Permissions gitlab asks for' });
  await expect(asks).toContainText('network:gitlab.com');
  await expect(asks).toContainText('secret:GITLAB_TOKEN');
  const secret = dialog.getByLabel('GITLAB_TOKEN');
  await expect(secret).toHaveAttribute('type', 'password');
  await secret.fill('glpat-very-secret-value');
  await dialog.getByRole('button', { name: 'Install with these permissions' }).click();

  await expect(dialog).toBeHidden();
  await expect(page.getByText('gitlab installed')).toBeVisible();
  await expect(page.getByText('It is running now with the permissions you approved.')).toBeVisible();
  await expect(page.getByRole('tab', { name: /Installed/ })).toHaveAttribute('aria-selected', 'true');
  const gitlab = page.getByRole('article').filter({ hasText: 'gitlab' });
  await expect(gitlab.getByRole('list', { name: 'Granted to gitlab' })).toContainText('secret:GITLAB_TOKEN');
  await expect(gitlab).not.toContainText('glpat');
});

test('registry: official and community mods install from their release; community ones carry a warning', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/mods');
  await page.getByRole('tab', { name: /Available/ }).click();

  const linear = page.getByRole('article').filter({ hasText: 'linear' });
  await expect(linear).toContainText('Community');
  await expect(linear).not.toContainText('Official');
  await expect(linear).toContainText('Community mod: not reviewed by Madlabs, use at your own risk.');
  await expect(linear).toContainText('sun plugin add linear');
  await expect(linear.getByRole('link', { name: 'Source on GitHub' })).toHaveAttribute('href', 'https://github.com/linear-fans/sun-plugin-linear');
  await linear.getByRole('button', { name: 'Install' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog).toContainText('linear v2.1.0 from its GitHub release.');
  await expect(dialog.getByRole('note')).toContainText('not reviewed');
  await expect(dialog.getByRole('list', { name: 'Permissions linear asks for' })).toContainText('network:api.linear.app');
  // The secret is optional: it can be added later.
  await dialog.getByRole('button', { name: 'Install with these permissions' }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByRole('article').filter({ hasText: 'linear' }).getByRole('list', { name: 'Granted to linear' })).toBeVisible();

  // Official ones carry no warning; every mod installs with the same plain Install.
  await page.getByRole('tab', { name: /Available/ }).click();
  const jira = page.getByRole('article').filter({ hasText: 'jira' });
  await expect(jira).toContainText('Official');
  await expect(jira).not.toContainText('not reviewed');
  const figma = page.getByRole('article').filter({ hasText: 'figma-sync' });
  await expect(figma).toContainText('Community');
  await figma.getByRole('button', { name: 'Install', exact: true }).click();
  await expect(dialog.getByRole('heading', { name: 'Install figma-sync' })).toBeVisible();
  await expect(dialog.getByRole('list', { name: 'Permissions figma-sync asks for' })).toContainText('secret:FIGMA_TOKEN');
  await dialog.getByRole('button', { name: 'Install with these permissions' }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText('figma-sync installed')).toBeVisible();
  await expect(page.getByRole('article').filter({ hasText: 'figma-sync' }).getByRole('list', { name: 'Granted to figma-sync' })).toBeVisible();
});

test('360 px: mod listings and the install dialog fit without sideways scroll', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/mods');
  await page.getByRole('tab', { name: /Available/ }).click();
  await expect(page.getByRole('article').filter({ hasText: 'figma-sync' }).getByRole('button', { name: 'Install' })).toBeVisible();
  await no_overflow(page);
  await page.getByRole('article').filter({ hasText: 'linear' }).getByRole('button', { name: 'Install' }).click();
  await expect(page.getByRole('dialog').getByLabel('LINEAR_API_KEY')).toBeVisible();
  await no_overflow(page);
});
