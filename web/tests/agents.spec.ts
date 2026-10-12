// Agents page against the in-memory API: the list with readiness, create and edit in one dialog,
// archive with a confirm, and no write controls without manage_runtimes.
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, query = '') => page.goto(`/?mock=1${query}#/agents`);
const row = (page: Page, name: string) => page.getByRole('article', { name, exact: true });
const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test.use({ viewport: { width: 1280, height: 800 } });

test('agents: ready or why not, create, edit, archive', async ({ page }) => {
  await open(page);
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Agents');
  const ada = row(page, 'Ada');
  await expect(ada).toContainText('Ready');
  await expect(ada).toContainText('claude-code');
  await expect(ada).toContainText('2 open tasks');
  await expect(ada).toContainText('tdd');
  const owl = row(page, 'Night Owl');
  await expect(owl).toContainText('Not ready');
  await expect(owl).toContainText('gemini-cli: no host reports it');

  // Create: a runtime is required; the chain keeps the order picked.
  await page.getByRole('button', { name: 'New agent' }).click();
  const dialog = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'New agent' }) });
  await dialog.getByLabel('Name', { exact: true }).fill('Scout');
  await dialog.getByRole('button', { name: 'Create agent' }).click();
  await expect(dialog.getByRole('alert')).toHaveText('Add at least one runtime.');
  const chain = dialog.getByRole('combobox', { name: 'Add to Runtimes, in failover order' });
  await chain.fill('codex');
  await chain.press('Enter');
  await chain.fill('claude-code');
  await chain.press('Enter');
  await dialog.getByLabel('Instructions').fill('Map the codebase before any change.');
  await dialog.getByRole('button', { name: 'Create agent' }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText('Scout created')).toBeVisible();
  await expect(row(page, 'Scout')).toContainText(/codex\s*claude-code/);
  await expect(row(page, 'Scout')).toContainText('Ready');

  // Names are unique ignoring case: the kernel's message shows in the dialog.
  await page.getByRole('button', { name: 'New agent' }).click();
  await dialog.getByLabel('Name', { exact: true }).fill('scout');
  await chain.fill('codex');
  await chain.press('Enter');
  await dialog.getByRole('button', { name: 'Create agent' }).click();
  await expect(dialog.getByRole('alert')).toContainText('There is already an agent named');
  await dialog.getByRole('button', { name: 'Cancel' }).click();

  // Edit opens filled in.
  await row(page, 'Ada').getByRole('button', { name: 'Edit Ada' }).click();
  const edit = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Edit Ada' }) });
  await expect(edit.getByLabel('Instructions')).toHaveValue(/failing test/);
  await edit.getByLabel('Description').fill('Fixes every bug.');
  await edit.getByRole('button', { name: 'Save' }).click();
  await expect(edit).toBeHidden();
  await expect(row(page, 'Ada')).toContainText('Fixes every bug.');

  // Archive asks first.
  await row(page, 'Night Owl').getByRole('button', { name: 'Archive Night Owl' }).click();
  const confirm = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Archive Night Owl?' }) });
  await expect(confirm).toContainText('It has no open tasks.');
  await confirm.getByRole('button', { name: 'Archive' }).click();
  await expect(row(page, 'Night Owl')).toHaveCount(0);
});

test('agents are the deployment owner’s page until they are per workspace', async ({ page }) => {
  await open(page, '&mock_role=member');
  await expect(page.getByRole('heading', { name: 'Tasks', level: 1 })).toBeVisible();
  await expect(page.getByRole('link', { name: 'Agents' })).toHaveCount(0);
  await expect(row(page, 'Ada')).toHaveCount(0);
});

test('360 px: agent rows fit', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page);
  await expect(row(page, 'Night Owl')).toContainText('no host reports it');
  await no_overflow(page);
});
