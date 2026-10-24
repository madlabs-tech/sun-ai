// The repo editor (#/editor/<repo_id>) against the in-memory API: open it from Settings → Code, browse the
// lazily loaded tree, edit and save a file that is still there after a reload, create and delete files, a
// save conflict, the 2 MB / binary notices, and a viewer who may only read.
import { expect, test, type BrowserContext, type Page } from '@playwright/test';

const API_REPO = '01JAR000100000000000000000';
const open = (page: Page, query = '') => page.goto(`/?mock=1${query}#/editor/${API_REPO}`);
const tree = (page: Page) => page.getByRole('navigation', { name: 'Files' });
const entry = (page: Page, name: string) => tree(page).getByRole('button', { name, exact: true });
const code = (page: Page) => page.locator('.cm-content');
const tab = (page: Page, name: string) => page.getByRole('tab', { name });
const dialog = (page: Page, title: string) => page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: title }) });

test.use({ viewport: { width: 1280, height: 800 } });

test('Settings → Code opens a repo in the editor', async ({ page }) => {
  await page.goto('/?mock=1#/settings/code');
  await page.getByRole('button', { name: 'Actions for https://github.com/madlabs/api.git' }).click();
  await page.getByRole('menuitem', { name: 'Open editor' }).click();
  await expect(page).toHaveURL(new RegExp(`#/editor/${API_REPO}`));
  await expect(page.getByRole('heading', { name: 'Editor' })).toBeVisible();
  await expect(entry(page, 'README.md')).toBeVisible();
});

test('expand a folder, edit and save a file, and the edit is there after a reload', async ({ page }) => {
  await open(page);
  // Two online hosts: the picker shows, and the pick is remembered.
  await page.getByRole('combobox', { name: 'Host' }).selectOption({ label: 'build-box-01' });
  await expect(entry(page, 'src')).toHaveAttribute('aria-expanded', 'false');
  await entry(page, 'src').click();
  await expect(entry(page, 'src')).toHaveAttribute('aria-expanded', 'true');
  await entry(page, 'greet.ts').click();
  await expect(entry(page, 'greet.ts')).toHaveAttribute('aria-current', 'true');
  await expect(code(page)).toContainText('export const greet');

  await code(page).click();
  await page.keyboard.press('ControlOrMeta+End');
  await page.keyboard.type('// saved from the editor');
  await expect(tab(page, 'greet.ts').getByRole('img', { name: 'Unsaved changes' })).toBeVisible();
  await page.keyboard.press('ControlOrMeta+s');
  await expect(tab(page, 'greet.ts').getByRole('img', { name: 'Unsaved changes' })).toHaveCount(0);

  await page.reload();
  await expect(page.getByRole('combobox', { name: 'Host' })).toHaveValue(/H0002/);
  await entry(page, 'src').click();
  await entry(page, 'greet.ts').click();
  await expect(code(page)).toContainText('// saved from the editor');
});

test('create a file and a folder, rename and delete', async ({ page }) => {
  await open(page);
  await tree(page).getByRole('button', { name: 'New file' }).click();
  await dialog(page, 'New file').getByLabel('Name').fill('notes.md');
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(entry(page, 'notes.md')).toBeVisible();
  await expect(tab(page, 'notes.md')).toHaveAttribute('aria-selected', 'true');

  // Right-click opens the same menu as the actions button.
  await entry(page, 'notes.md').click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Rename' }).click();
  await dialog(page, 'Rename').getByLabel('Name').fill('todo.md');
  await page.getByRole('button', { name: 'Rename' }).click();
  await expect(entry(page, 'todo.md')).toBeVisible();
  await expect(entry(page, 'notes.md')).toHaveCount(0);
  await expect(tab(page, 'todo.md')).toBeVisible();

  await page.getByRole('button', { name: 'Actions for todo.md' }).click();
  await page.getByRole('menuitem', { name: 'Delete' }).click();
  await dialog(page, 'Delete file?').getByRole('button', { name: 'Delete' }).click();
  await expect(entry(page, 'todo.md')).toHaveCount(0);
  await expect(tab(page, 'todo.md')).toHaveCount(0);

  await page.getByRole('button', { name: 'Actions for src' }).click();
  await page.getByRole('menuitem', { name: 'New folder' }).click();
  await dialog(page, 'New folder').getByLabel('Name').fill('lib');
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(entry(page, 'lib')).toBeVisible();
});

/** Someone saves README.md in another tab: the same worktree, so it changes under this tab. */
async function save_elsewhere(context: BrowserContext, text: string) {
  const other = await context.newPage();
  await open(other);
  await entry(other, 'README.md').click();
  await code(other).click();
  await other.keyboard.press('ControlOrMeta+Home');
  await other.keyboard.type(text);
  await other.keyboard.press('ControlOrMeta+s');
  await expect(tab(other, 'README.md').getByRole('img', { name: 'Unsaved changes' })).toHaveCount(0);
  await other.close();
}

test('a file changed on disk since it was opened asks to reload or overwrite', async ({ page, context }) => {
  await open(page);
  await entry(page, 'README.md').click();
  await expect(code(page)).toContainText('# Sample repo');
  const conflict = dialog(page, 'The file changed on disk');

  await save_elsewhere(context, 'Theirs. ');
  await code(page).click();
  await page.keyboard.press('ControlOrMeta+Home');
  await page.keyboard.type('Mine. ');
  await page.keyboard.press('ControlOrMeta+s');
  await conflict.getByRole('button', { name: 'Reload' }).click();
  await expect(code(page)).toContainText('Theirs. # Sample repo');
  await expect(code(page)).not.toContainText('Mine.');

  await save_elsewhere(context, 'Again. ');
  await code(page).click();
  await page.keyboard.press('ControlOrMeta+Home');
  await page.keyboard.type('Mine. ');
  await page.keyboard.press('ControlOrMeta+s');
  await conflict.getByRole('button', { name: 'Overwrite' }).click();
  await expect(conflict).toHaveCount(0);
  await expect(tab(page, 'README.md').getByRole('img', { name: 'Unsaved changes' })).toHaveCount(0);
  await page.reload();
  await entry(page, 'README.md').click();
  await expect(code(page)).toContainText('Mine. Theirs. # Sample repo');
});

test('files over 2 MB and binary files show a read-only notice', async ({ page }) => {
  await open(page);
  await entry(page, 'dump.sql').click();
  await expect(page.getByText('This file is over 2 MB, too large to open here.')).toBeVisible();
  await entry(page, 'logo.png').click();
  await expect(page.getByText('This is a binary file, which the editor can’t show.')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Save' })).toHaveCount(0);
});

test('a viewer reads only: no save, create, rename or delete, and a Read-only badge', async ({ page }) => {
  await open(page, '&mock_role=viewer');
  await expect(page.getByText('Read-only')).toBeVisible();
  await entry(page, 'README.md').click();
  await expect(code(page)).toContainText('# Sample repo');
  await expect(page.getByRole('button', { name: 'Save' })).toHaveCount(0);
  await expect(tree(page).getByRole('button', { name: 'New file' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Actions for README.md' })).toHaveCount(0);
  await code(page).click();
  await page.keyboard.type('nope');
  await expect(code(page)).not.toContainText('nope');
});

test('a 10k-line file opens in under a second and scrolls to its end', async ({ page }) => {
  await open(page);
  await entry(page, 'src').click();
  const started = Date.now();
  await entry(page, 'big.ts').click();
  await expect(code(page)).toContainText('export const line_0 = 0;');
  const took = Date.now() - started;
  console.log(`big.ts (10 000 lines) opened in ${took} ms`);
  expect(took).toBeLessThan(1000);
  await code(page).click();
  await page.keyboard.press('ControlOrMeta+End');
  await expect(code(page)).toContainText('export const line_9999 = 9999;');
});
