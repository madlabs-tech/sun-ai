// The app shell: the grouped sidebar, the account/workspace menu, hiding the sidebar (toggle and
// ⌘B / Ctrl+B), and the phone tab bar + More page.
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, route = 'tasks', query = '') => page.goto(`/?mock=1${query}#/${route}`);
const sidebar = (page: Page) => page.getByRole('complementary', { name: 'Main' });
const trigger = (page: Page) => sidebar(page).getByRole('button', { name: /^Workspace: / });
const menu = (page: Page) => page.getByRole('menu', { name: 'Account and workspaces' });

test.use({ viewport: { width: 1280, height: 800 } });

test('sidebar: sections in order, Projects and Agents open their pages, no runtime list', async ({ page }) => {
  await open(page);
  const side = sidebar(page);
  await expect(side.getByRole('heading')).toHaveText(['Work', 'AI Team']);
  await expect(side.getByRole('link')).toHaveText([
    /Approvals\s*2$/,
    /Tasks\s*\d+$/,
    'Projects',
    'Workflows',
    'Knowledge',
    'Memory',
    'Evals',
    'Agents',
    'Chat',
    'Runtimes',
    'Hosts',
    'Catalog',
    'Mods',
    'Marketplace',
    'Settings',
  ]);
  await expect(side).toContainText('Workspace spend');
  await expect(side.getByRole('heading', { name: 'Runtimes' })).toHaveCount(0);

  for (const name of ['Agents', 'Projects']) {
    await side.getByRole('link', { name }).click();
    await expect(page.getByRole('heading', { level: 1 })).toHaveText(name);
    await expect(page.getByText('Coming in this release')).toHaveCount(0);
    await expect(side.getByRole('link', { name })).toHaveAttribute('aria-current', 'page');
  }
});

test('menu: account, workspaces with the current one checked, switching, keyboard', async ({ page }) => {
  await open(page);
  const button = trigger(page);
  await expect(button).toHaveAccessibleName('Workspace: Main');
  await expect(button).toHaveAttribute('aria-haspopup', 'menu');
  await expect(button).toHaveAttribute('aria-expanded', 'false');

  // Create a second workspace through the menu.
  await button.click();
  await expect(button).toHaveAttribute('aria-expanded', 'true');
  await expect(page.getByText('me@acme.io')).toBeVisible();
  await expect(menu(page).getByRole('menuitemradio')).toHaveText([/Main/]);
  await expect(menu(page).getByRole('menuitemradio', { name: 'Main' })).toHaveAttribute('aria-checked', 'true');
  await menu(page).getByRole('menuitem', { name: 'New workspace' }).click();
  await expect(menu(page)).toBeHidden();
  const dialog = page.getByRole('dialog');
  await dialog.getByLabel('Name').fill('Growth');
  await dialog.getByRole('button', { name: 'Create workspace' }).click();
  await expect(button).toHaveAccessibleName('Workspace: Growth');

  // Switch back with the mouse.
  await button.click();
  await expect(menu(page).getByRole('menuitemradio', { name: 'Growth' })).toHaveAttribute('aria-checked', 'true');
  await expect(menu(page).getByRole('menuitemradio', { name: 'Main' })).toHaveAttribute('aria-checked', 'false');
  await menu(page).getByRole('menuitemradio', { name: 'Main' }).click();
  await expect(menu(page)).toBeHidden();
  await expect(button).toHaveAccessibleName('Workspace: Main');
  await expect(page).toHaveURL(/workspace=main$/);

  // Keyboard: arrows open and move, wrap around, Esc closes back onto the trigger.
  await button.focus();
  await page.keyboard.press('ArrowDown');
  await expect(menu(page).getByRole('menuitemradio', { name: 'Main' })).toBeFocused();
  await page.keyboard.press('ArrowDown');
  await expect(menu(page).getByRole('menuitemradio', { name: 'Growth' })).toBeFocused();
  await page.keyboard.press('ArrowUp');
  await page.keyboard.press('ArrowUp');
  await expect(menu(page).getByRole('menuitem', { name: 'Log out' })).toBeFocused();
  await page.keyboard.press('Home');
  await page.keyboard.press('ArrowDown');
  await page.keyboard.press('Enter');
  await expect(button).toHaveAccessibleName('Workspace: Growth');
  await button.focus();
  await page.keyboard.press('Enter');
  await expect(menu(page)).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(menu(page)).toBeHidden();
  await expect(button).toBeFocused();
  await expect(button).toHaveAttribute('aria-expanded', 'false');
});

test('menu: Esc closes only the menu, not the task panel behind it', async ({ page }) => {
  await open(page, 'tasks/SUN-142');
  const panel = page.getByRole('main');
  await expect(panel).toBeVisible();
  await trigger(page).click();
  await expect(menu(page)).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(menu(page)).toBeHidden();
  await expect(panel).toBeVisible();
});

test('menu: anyone creates a workspace, unless the deployment lets only its owners', async ({ page }) => {
  await open(page, 'tasks', '&mock_role=member');
  await trigger(page).click();
  await expect(menu(page).getByRole('menuitemradio')).not.toHaveCount(0);
  await expect(menu(page).getByRole('menuitem')).toHaveText(['New workspace', 'Log out']);
  await open(page, 'tasks', '&mock_role=member&creation_disabled=1');
  await trigger(page).click();
  await expect(menu(page).getByRole('menuitem')).toHaveText(['Log out']);
});

test('menu: Log out shows sign-in, and the next sign-in shows the new account', async ({ page }) => {
  await open(page);
  await trigger(page).click();
  await menu(page).getByRole('menuitem', { name: 'Log out' }).click();
  await page.getByLabel('Email').fill('ana@acme.io');
  await page.getByLabel('Password', { exact: true }).fill('correct horse battery');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await trigger(page).click();
  await expect(page.getByText('ana@acme.io')).toBeVisible();
  await expect(page.getByText('me@acme.io')).toHaveCount(0);
});

test('collapse: the toggle and ⌘B / Ctrl+B hide and show the sidebar; typing is left alone', async ({ page }) => {
  await open(page);
  const side = sidebar(page);
  const main = page.getByRole('main');
  const expand = main.getByRole('button', { name: 'Toggle sidebar' });
  await expect(expand).toBeHidden();

  // The button: focus moves to the toggle still on screen, and the page takes the full width.
  await side.getByRole('button', { name: 'Toggle sidebar' }).click();
  await expect(side).toBeHidden();
  await expect(expand).toBeFocused();
  expect((await main.boundingBox())?.width).toBe(1280);
  await expand.click();
  await expect(side).toBeVisible();
  await expect(side.getByRole('button', { name: 'Toggle sidebar' })).toBeFocused();

  // The shortcut.
  await page.keyboard.press('ControlOrMeta+b');
  await expect(side).toBeHidden();
  await page.keyboard.press('ControlOrMeta+b');
  await expect(side).toBeVisible();

  // Not while typing.
  const search = main.getByRole('textbox').first();
  await search.focus();
  await page.keyboard.press('ControlOrMeta+b');
  await expect(side).toBeVisible();

  // Not kept: a reload shows the sidebar.
  await search.blur();
  await page.keyboard.press('ControlOrMeta+b');
  await expect(side).toBeHidden();
  await page.reload();
  await expect(side).toBeVisible();
});

test('360 px: tab bar and More keep the menu and gain Log out; the menu fits', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page);
  const tabs = page.getByRole('navigation', { name: 'Main' });
  await expect(tabs.getByRole('link')).toHaveText(['Tasks', /Approvals/, 'Agents', 'More']);
  await expect(sidebar(page)).toBeHidden();
  // ⌘B does nothing on a phone layout.
  await page.keyboard.press('ControlOrMeta+b');
  await expect(tabs).toBeVisible();

  await tabs.getByRole('link', { name: 'More' }).click();
  const more = page.getByRole('navigation', { name: 'More' });
  await expect(more.getByRole('button', { name: 'Log out' })).toBeVisible();
  await more.getByRole('button', { name: /^Workspace: / }).click();
  const box = (await menu(page).locator('..').boundingBox())!;
  expect(box.x).toBeGreaterThanOrEqual(0);
  expect(box.x + box.width).toBeLessThanOrEqual(360);
  for (const item of await menu(page).locator('[role^="menuitem"]').all()) expect((await item.boundingBox())!.height).toBeGreaterThanOrEqual(44);
  await page.keyboard.press('Escape');

  await more.getByRole('button', { name: 'Log out' }).click();
  await expect(page.getByLabel('Email')).toBeVisible();
});
