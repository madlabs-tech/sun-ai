// Mods screen (GET /mods): installed with granted permissions, available with what they ask for.
import { expect, test, type Page } from '@playwright/test';

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test('mods: installed show what was granted, available what they ask for and how to install', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/mods');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Mods');

  await expect(page.getByRole('tab', { name: /Installed/ })).toHaveAttribute('aria-selected', 'true');
  const github = page.getByRole('article', { name: 'github', exact: true });
  await expect(github.getByRole('list', { name: 'Granted to github' })).toContainText('secret:GITHUB_TOKEN');
  await expect(github).not.toContainText('Unsigned');
  await expect(page.getByRole('article', { name: 'device-farm' })).toContainText('Unsigned');
  await expect(page.getByRole('article').filter({ hasText: 'gitlab' })).toHaveCount(0);

  await page.getByRole('tab', { name: /Available/ }).click();
  const gitlab = page.getByRole('article').filter({ hasText: 'gitlab' });
  await expect(gitlab.getByRole('list', { name: 'Asked for by gitlab' })).toContainText('network:gitlab.com');
  await expect(gitlab).toContainText('sun plugin add gitlab');
  await expect(gitlab).toContainText('Official');
  await expect(page.getByRole('article', { name: 'github', exact: true })).toHaveCount(0);
});

test('lifecycle: plugins and extensions apart, update with new permissions, remove warns about dependents', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/mods');
  const plugins = page.getByRole('region', { name: /Plugins/ });
  const extensions = page.getByRole('region', { name: /Extensions/ });
  await expect(plugins.getByRole('article', { name: 'github', exact: true })).toBeVisible();
  await expect(extensions.getByRole('article', { name: 'device-farm' })).toBeVisible();
  await expect(plugins.getByRole('article', { name: 'device-farm' })).toHaveCount(0);
  await expect(plugins.getByRole('article', { name: 'github', exact: true })).toContainText('Needed by device-farm');

  // Update: the dialog marks what the new version adds.
  const slack = plugins.getByRole('article').filter({ hasText: 'slack' });
  await slack.getByRole('button', { name: 'Update to v1.0.0' }).click();
  let dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('heading', { name: 'Update slack' })).toBeVisible();
  await expect(dialog).toContainText('v0.9.1 → v1.0.0');
  const added = dialog.getByRole('listitem').filter({ hasText: 'network:files.slack.com' });
  await expect(added).toContainText('New');
  await expect(dialog.getByRole('listitem').filter({ hasText: 'network:slack.com' }).first()).not.toContainText('New');
  await dialog.getByRole('button', { name: 'Update with these permissions' }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText('slack updated')).toBeVisible();
  await expect(slack).toContainText('v1.0.0');
  await expect(slack.getByRole('button', { name: /Update to/ })).toHaveCount(0);

  // Remove: names the extension that needs it; data stays unless asked.
  await plugins.getByRole('article', { name: 'github', exact: true }).getByRole('button', { name: 'Remove' }).click();
  dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('heading', { name: 'Remove github?' })).toBeVisible();
  await expect(dialog.getByRole('alert')).toContainText('device-farm needs it');
  await expect(dialog).toContainText('Kept, so a reinstall picks up where it left off.');
  const purge = dialog.getByRole('checkbox', { name: 'Also delete its data and secrets' });
  await expect(purge).not.toBeChecked();
  await dialog.getByText('Also delete its data and secrets').click();
  await expect(purge).toBeChecked();
  await expect(dialog).toContainText('This can’t be undone.');
  await dialog.getByRole('button', { name: 'Remove github' }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText('github removed')).toBeVisible();
  await expect(plugins.getByRole('article', { name: 'github', exact: true })).toHaveCount(0);
  await page.getByRole('tab', { name: /Available/ }).click();
  await expect(page.getByRole('region', { name: /Plugins/ }).getByRole('article', { name: 'github', exact: true })).toContainText('sun plugin add github');
});

test('360 px: mods fit without sideways scroll', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/mods');
  await expect(page.getByRole('article').filter({ hasText: 'slack' })).toContainText('secret:SLACK_SIGNING_SECRET');
  await no_overflow(page);
  await page.getByRole('article', { name: 'github', exact: true }).getByRole('button', { name: 'Remove' }).click();
  await expect(page.getByRole('dialog').getByRole('alert')).toBeVisible();
  await no_overflow(page);
  await page.getByRole('dialog').getByRole('button', { name: 'Keep it' }).click();
  await page.getByRole('tab', { name: /Available/ }).click();
  await expect(page.getByText('sun plugin add jira')).toBeVisible();
  await no_overflow(page);
});
