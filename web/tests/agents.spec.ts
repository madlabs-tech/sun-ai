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

test('a member sees agents but no write controls', async ({ page }) => {
  await open(page, '&mock_role=member');
  await expect(row(page, 'Ada')).toBeVisible();
  await expect(page.getByRole('button', { name: 'New agent' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Edit Ada' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Archive Ada' })).toHaveCount(0);
});

test('a new workspace starts with no agents of Main’s', async ({ page }) => {
  await open(page);
  await expect(row(page, 'Ada')).toBeVisible();
  const sidebar = page.getByRole('complementary', { name: 'Main' });
  await sidebar.getByRole('button', { name: /^Workspace: / }).click();
  await page.getByRole('menuitem', { name: 'New workspace' }).click();
  await page.getByRole('dialog').getByLabel('Name').fill('Lab');
  await page.getByRole('dialog').getByRole('button', { name: 'Create workspace' }).click();
  await expect(page).toHaveURL(/workspace=lab$/);
  await expect(row(page, 'Ada')).toHaveCount(0);
});

test('360 px: agent rows fit', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page);
  await expect(row(page, 'Night Owl')).toContainText('no host reports it');
  await no_overflow(page);
});

// TRD BLO-117 item 8: each chain entry picks its host, model and effort; the effort options are exactly
// what that tool reported (per model where it scopes them), and grey out where it reports none.
test('agent form: host, model and effort per chain entry, from the hosts’ catalogs', async ({ page }) => {
  await open(page);
  await page.getByRole('button', { name: 'New agent' }).click();
  const dialog = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'New agent' }) });
  await dialog.getByLabel('Name', { exact: true }).fill('Tuner');
  const chain = dialog.getByRole('combobox', { name: 'Add to Runtimes, in failover order' });
  for (const name of ['codex', 'claude-code', 'lan-qwen']) {
    await chain.fill(name);
    await chain.press('Enter');
  }
  const settings = (name: string) => dialog.getByRole('group', { name: `Settings of ${name}` });
  const options = (name: string, field: string) => settings(name).getByLabel(field).locator('option');
  const codex = settings('codex');

  // Switching tool swaps the options: Codex's union across its hosts, Claude Code's own list.
  await expect(options('codex', 'Effort')).toHaveText(['CLI default', 'Low', 'Medium', 'High', 'Extra high']);
  await expect(options('claude-code', 'Effort')).toHaveText(['CLI default', 'Low', 'Medium', 'High']);
  await expect(options('codex', 'Host')).toHaveText(['Any host', 'build-box-01', 'hery-mbp']);
  // Offline hosts aren't offered.
  await expect(options('claude-code', 'Host')).toHaveText(['Any host', 'hery-mbp', 'build-box-01']);
  // A tool with no catalog greys the picker out and says why.
  await expect(settings('lan-qwen').getByLabel('Effort')).toBeDisabled();
  await expect(settings('lan-qwen')).toContainText('OpenCode reports no effort levels.');

  // Switching model swaps them again where the tool scopes effort per model.
  await codex.getByLabel('Model').fill('gpt-5-mini');
  await codex.getByLabel('Model').press('Tab');
  await expect(options('codex', 'Effort')).toHaveText(['CLI default', 'Low', 'Medium']);
  await codex.getByLabel('Model').fill('gpt-5-codex');
  await codex.getByLabel('Model').press('Tab');
  await codex.getByLabel('Effort').selectOption('xhigh');
  // The older Codex on build-box-01 has no Extra high: pinning there drops it.
  await codex.getByLabel('Host').selectOption({ label: 'build-box-01' });
  await expect(options('codex', 'Effort')).toHaveText(['CLI default', 'Low', 'Medium', 'High']);
  await expect(codex.getByLabel('Effort')).toHaveValue('');
  await codex.getByLabel('Host').selectOption({ label: 'hery-mbp' });
  await codex.getByLabel('Effort').selectOption('xhigh');

  await dialog.getByRole('button', { name: 'Create agent' }).click();
  await expect(dialog).toBeHidden();
  await expect(row(page, 'Tuner')).toContainText('gpt-5-codex · xhigh');

  // Edit opens with each entry's settings; untouched entries stay plain names.
  await row(page, 'Tuner').getByRole('button', { name: 'Edit Tuner' }).click();
  const edit = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Edit Tuner' }) });
  const edited = edit.getByRole('group', { name: 'Settings of codex' });
  await expect(edited.getByLabel('Effort')).toHaveValue('xhigh');
  await expect(edited.getByLabel('Model')).toHaveValue('gpt-5-codex');
  await expect(edit.getByRole('group', { name: 'Settings of claude-code' }).getByLabel('Host')).toHaveValue('');
});

test('360 px: the chain settings fit', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page);
  await page.getByRole('button', { name: 'New agent' }).click();
  const dialog = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'New agent' }) });
  const chain = dialog.getByRole('combobox', { name: 'Add to Runtimes, in failover order' });
  await chain.fill('codex');
  await chain.press('Enter');
  await expect(dialog.getByRole('group', { name: 'Settings of codex' }).getByLabel('Effort')).toBeVisible();
  await no_overflow(page);
});
