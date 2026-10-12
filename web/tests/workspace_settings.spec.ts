// Settings → <workspace> → General against the in-memory API: profile fields saved on blur, the logo's
// checks, the prefix dialog, the danger zone, and a read-only view without `edit_workspace`.
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, query = '') => page.goto(`/?mock=1${query}#/settings/workspace`);
const details = (page: Page) => page.getByRole('region', { name: 'Workspace details' });
const status = (page: Page, text: string) => details(page).getByRole('status').filter({ hasText: text });
const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
/** A tiny valid PNG (1×1). */
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==', 'base64');

test.use({ viewport: { width: 1280, height: 800 } });

test('edit the profile: each field saves on blur, the default workflow on change', async ({ page }) => {
  await open(page);
  await expect(page.getByRole('heading', { name: 'General', exact: true, level: 2 })).toBeVisible();
  const head = page.getByRole('heading', { name: 'General', exact: true, level: 2 }).locator('..');
  await expect(head).toContainText('Workspace');

  const name = page.getByLabel('Name', { exact: true });
  await expect(name).toHaveValue('Main');
  await name.fill('Core');
  await name.blur();
  await expect(status(page, 'Saved')).toHaveCount(1);
  await expect(page.getByRole('complementary', { name: 'Main' }).getByRole('button', { name: /^Workspace: / })).toHaveAccessibleName('Workspace: Core');

  // An empty name is refused before any request.
  await name.fill('  ');
  await name.blur();
  await expect(details(page).getByText('Give the workspace a name.')).toBeVisible();
  await name.fill('Core');
  await name.blur();

  const description = page.getByLabel('Description');
  await expect(description).toHaveAttribute('placeholder', 'What does this workspace focus on?');
  await expect(description).toHaveAttribute('rows', '3');
  await description.fill('Product engineering for the web app.');
  await description.blur();
  const context = page.getByLabel('Agent context');
  await expect(context).toHaveAttribute('rows', '4');
  await expect(details(page)).toContainText('Given to every agent in this workspace on every run.');
  await context.fill('Use pnpm. Never push to main.');
  await context.blur();
  await expect(status(page, 'Saved')).toHaveCount(3);

  // Read-only slug with a copy button; the prefix with its hint.
  await expect(details(page).getByRole('group', { name: 'Slug' })).toContainText('main');
  await expect(details(page).getByRole('button', { name: 'Copy slug' })).toBeVisible();
  await expect(details(page)).toContainText("Part of every workspace URL. It can't be changed.");
  await expect(details(page)).toContainText('Used in task numbers like SUN-123.');

  const workflow = page.getByLabel('Default workflow');
  await expect(workflow.locator('option').first()).toHaveText('None');
  await workflow.selectOption({ label: 'Bug fix → verified PR' });
  await expect(status(page, 'Saved')).toHaveCount(4);

  // Kept: back on the page (in-app), the fields show what was saved.
  await page.getByRole('navigation', { name: 'Settings' }).getByRole('link', { name: 'Account' }).click();
  await page.getByRole('navigation', { name: 'Settings' }).getByRole('link', { name: 'General' }).click();
  await expect(page.getByLabel('Description')).toHaveValue('Product engineering for the web app.');
  await expect(page.getByLabel('Agent context')).toHaveValue('Use pnpm. Never push to main.');
  await expect(page.getByLabel('Default workflow')).toHaveValue('wf_bugfix');
});

test('logo: type and size are checked next to it; a good image shows in the switcher', async ({ page }) => {
  await open(page);
  const file = page.locator('input[type="file"]');
  await file.setInputFiles({ name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('hello') });
  await expect(details(page).getByRole('alert')).toHaveText('Use a PNG, JPEG or WebP image.');
  await file.setInputFiles({ name: 'big.png', mimeType: 'image/png', buffer: Buffer.alloc(1024 * 1024 + 1) });
  await expect(details(page).getByRole('alert')).toHaveText('That image is over 1 MB. Use a smaller one.');
  await file.setInputFiles({ name: 'logo.png', mimeType: 'image/png', buffer: PNG });
  await expect(status(page, 'Saved')).toHaveCount(1);
  await expect(details(page).getByRole('alert')).toHaveCount(0);
  await expect(page.getByRole('complementary', { name: 'Main' }).locator('.switcher__trigger img')).toBeVisible();
  await details(page).getByRole('button', { name: 'Remove' }).click();
  await expect(page.getByRole('complementary', { name: 'Main' }).locator('.switcher__trigger img')).toHaveCount(0);
});

test('prefix dialog: the rule, a struck-through preview, the warning, then the change', async ({ page }) => {
  await open(page);
  await details(page).getByRole('button', { name: 'Change prefix…' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Change the task prefix' });
  await expect(dialog).toContainText('Letters and numbers only, up to 10 characters.');
  await expect(dialog).toContainText('Every task is renumbered, including done and cancelled ones.');
  await expect(dialog.locator('s')).toHaveText(/^SUN-\d+$/);

  const input = dialog.getByLabel('New prefix');
  await input.fill('web-1');
  await dialog.getByRole('button', { name: 'Change to WEB-1' }).click();
  await expect(dialog.locator('.ml-field__hint--error')).toHaveText('Letters and numbers only, up to 10 characters.');

  await input.fill('core');
  await expect(dialog.locator('.prefix__new')).toHaveText(/^CORE-\d+$/);
  await expect(dialog).toContainText(/Task keys change from\s*SUN-\d+\s*→?\s*to\s*CORE-\d+/);
  await dialog.getByRole('button', { name: 'Change to CORE' }).click();
  await expect(dialog).toBeHidden();
  await expect(details(page).getByRole('group', { name: 'Task prefix' })).toContainText('CORE');
  await expect(details(page)).toContainText('Used in task numbers like CORE-123.');
});

test('a viewer reads the profile but changes nothing', async ({ page }) => {
  await open(page, '&mock_role=viewer');
  await expect(details(page)).toContainText('You can see these settings.');
  await expect(page.getByLabel('Name', { exact: true })).toHaveAttribute('readonly', '');
  await expect(page.getByLabel('Description')).toHaveAttribute('readonly', '');
  await expect(page.getByLabel('Agent context')).toHaveAttribute('readonly', '');
  await expect(page.getByLabel('Default workflow')).toHaveText('None');
  await expect(details(page).getByRole('button', { name: 'Change prefix…' })).toHaveCount(0);
  await expect(details(page).getByRole('button', { name: /Upload|Replace/ })).toHaveCount(0);
  await expect(page.locator('input[type="file"]')).toHaveCount(0);
  await expect(page.getByRole('region', { name: 'Danger zone' })).toHaveCount(0);
  const nav = page.getByRole('navigation', { name: 'Settings' });
  await expect(nav.getByRole('link', { name: 'Access' })).toHaveCount(0);
  await expect(nav.getByRole('link', { name: 'Code' })).toHaveCount(0);
});

test('360 px: the General page fits, labels above their fields', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page);
  await expect(details(page)).toBeVisible();
  await expect(page.getByRole('region', { name: 'Danger zone' })).toBeVisible();
  await no_overflow(page);
  await details(page).getByRole('button', { name: 'Change prefix…' }).click();
  await expect(page.getByRole('dialog')).toBeVisible();
  await no_overflow(page);
});
