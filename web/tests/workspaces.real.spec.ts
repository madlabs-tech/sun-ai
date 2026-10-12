// The workspaces refactor on the real kernel: a plain task's lifecycle by keyboard (Done only from
// Needs you), comments, a new workspace whose prefix change renumbers its keys, and old links.
import { expect, test, type Page } from '@playwright/test';

const MAIN = '00000000000000000000000001';
const main = (page: Page) => page.getByRole('main');
const sidebar = (page: Page) => page.getByRole('complementary', { name: 'Main' });

async function sign_in(page: Page) {
  await page.setViewportSize({ width: 1280, height: 860 });
  await page.goto('/');
  await page.getByLabel('Email').fill('me@example.com');
  await page.getByLabel('Password', { exact: true }).fill('test-token');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('heading', { name: 'Needs you', exact: true })).toBeVisible();
  // A kernel with no limit policy asks the set-up questions; skipping stores nothing.
  const setup = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Set up Sun' }) });
  await setup.getByRole('button', { name: 'Skip for now' }).click({ timeout: 3_000 }).catch(() => {});
}

/** Picks `to` in the task page's Status menu with the keyboard only. */
async function move(page: Page, to: string) {
  await main(page).getByRole('button', { name: /^Status: / }).focus();
  await page.keyboard.press('Enter');
  const item = page.getByRole('menu', { name: 'Status' }).getByRole('menuitemradio', { name: to, exact: true });
  for (let i = 0; i < 12 && !(await item.evaluate((el) => el === document.activeElement)); i++) {
    await page.keyboard.press('ArrowDown');
  }
  await page.keyboard.press('Enter');
  await expect(main(page).getByRole('button', { name: `Status: ${to}` })).toBeVisible();
}

test('a plain task moves by keyboard, reaches Done only through Needs you, and keeps its comments', async ({ page }) => {
  await sign_in(page);
  const title = `Plain ${Date.now()}`;
  await sidebar(page).getByRole('button', { name: 'New task' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('textbox', { name: 'Title' }).fill(title);
  await dialog.getByRole('button', { name: /^Status:/ }).click();
  await page.getByRole('menuitemradio', { name: /^Backlog/ }).click();
  await expect(dialog.getByRole('status')).toHaveText('A plain task: nothing runs.');
  await dialog.getByRole('button', { name: 'Create task' }).click();
  await expect(main(page).getByRole('heading', { name: title })).toBeVisible();
  await expect(page).toHaveURL(/#\/tasks\/[A-Z0-9]+-\d+/);

  await move(page, 'Todo');
  // Done is not offered before Needs you.
  await main(page).getByRole('button', { name: 'Status: Todo' }).click();
  await expect(page.getByRole('menu', { name: 'Status' }).getByRole('menuitemradio', { name: 'Done', exact: true })).toHaveCount(0);
  await page.keyboard.press('Escape');
  await expect(main(page).getByRole('heading', { name: title })).toBeVisible();

  const comment = main(page).getByRole('textbox', { name: 'Comment' });
  await comment.fill('Checked on the real kernel.');
  await comment.press('ControlOrMeta+Enter');
  const activity = main(page).getByRole('region', { name: 'Activity' });
  await expect(activity.getByText('Checked on the real kernel.')).toBeVisible();

  await move(page, 'Needs you');
  await move(page, 'Done');
  await page.reload();
  await expect(main(page).getByRole('button', { name: 'Status: Done' })).toBeVisible();
  await expect(activity.getByText('Checked on the real kernel.')).toBeVisible();
});

test('a new workspace: its own prefix, renumbered by a change; old ?board= links land on the slug', async ({ page }) => {
  await sign_in(page);
  const name = `Ops ${Date.now() % 100000}`;
  await sidebar(page).getByRole('button', { name: /^Workspace: / }).click();
  await page.getByRole('menuitem', { name: 'New workspace' }).click();
  const create = page.getByRole('dialog');
  await create.getByLabel('Name').fill(name);
  await create.getByRole('button', { name: 'Create workspace' }).click();
  await expect(sidebar(page).getByRole('button', { name: /^Workspace: / })).toHaveAccessibleName(`Workspace: ${name}`);
  const slug = new URL(page.url().replace('#/', '')).searchParams.get('workspace') ?? '';
  expect(slug).toMatch(/^ops-\d+$/);

  await sidebar(page).getByRole('button', { name: 'New task' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('textbox', { name: 'Title' }).fill('Renumber me');
  await dialog.getByRole('button', { name: 'Create task' }).click();
  await expect(main(page).getByRole('heading', { name: 'Renumber me' })).toBeVisible();
  const old_key = page.url().match(/#\/tasks\/([A-Z0-9]+-1)\b/)?.[1];
  expect(old_key, page.url()).toBeTruthy();

  const prefix = `R${Date.now() % 1000000}`;
  await page.goto(`/#/settings/workspace?workspace=${slug}`);
  const details = page.getByRole('region', { name: 'Workspace details' });
  await details.getByRole('button', { name: 'Change prefix…' }).click();
  const change = page.getByRole('dialog').filter({ hasText: 'Change the task prefix' });
  await change.getByLabel('New prefix').fill(prefix);
  await change.getByRole('button', { name: `Change to ${prefix}` }).click();
  await expect(change).toBeHidden();

  await page.goto(`/#/tasks/${prefix}-1`);
  await expect(main(page).getByRole('heading', { name: 'Renumber me' })).toBeVisible();
  await page.goto(`/#/tasks/${old_key}`);
  await expect(main(page).getByRole('heading', { name: 'Renumber me' })).toHaveCount(0);

  await page.goto(`/#/tasks?board=${MAIN}`);
  await expect(page).toHaveURL(/#\/tasks\?workspace=main$/);
  await expect(sidebar(page).getByRole('button', { name: /^Workspace: / })).toHaveAccessibleName('Workspace: Main');
});
