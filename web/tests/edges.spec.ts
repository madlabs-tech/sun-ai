// Edge paths across the workspaces refactor: keyboard-only use of the task page and the create
// dialog, ⌘B ignored while typing, Escape closing only the topmost layer, every new screen at
// 360 px, and old links redirecting.
import { expect, test, type Locator, type Page } from '@playwright/test';

const MAIN = '00000000000000000000000001';
const open = (page: Page, route = '') => page.goto(`/?mock=1#/${route}`);
const main = (page: Page) => page.getByRole('main');
const sidebar = (page: Page) => page.getByRole('complementary', { name: 'Main' });

test.use({ viewport: { width: 1280, height: 860 } });

/** Presses ArrowDown until `item` has the focus. */
async function arrow_to(page: Page, item: Locator) {
  for (let i = 0; i < 12; i++) {
    if (await item.evaluate((el) => el === document.activeElement)) return;
    await page.keyboard.press('ArrowDown');
  }
  throw new Error('the item never got the focus');
}

async function no_page_overflow(page: Page, what: string) {
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow, `${what} scrolls sideways`).toBeLessThanOrEqual(0);
}

test('keyboard only: the status menu on the task page moves the task', async ({ page }) => {
  await open(page, 'tasks/SUN-142');
  await main(page).getByRole('button', { name: 'Status: In progress' }).focus();
  await page.keyboard.press('Enter');
  const menu = page.getByRole('menu', { name: 'Status' });
  await expect(menu).toBeVisible();
  await arrow_to(page, menu.getByRole('menuitemradio', { name: 'Blocked' }));
  await page.keyboard.press('Enter');
  await expect(menu).toBeHidden();
  const blocked = main(page).getByRole('button', { name: 'Status: Blocked' });
  await expect(blocked).toBeVisible();
  await expect(main(page).getByRole('region', { name: 'Activity' })).toContainText('You moved it from In progress to Blocked');
  await expect(blocked).toBeFocused();
});

test('keyboard only: a task is created from the keyboard with ⌘↵', async ({ page }) => {
  await open(page, 'tasks');
  await sidebar(page).getByRole('button', { name: 'New task' }).focus();
  await page.keyboard.press('Enter');
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('textbox', { name: 'Title' })).toBeFocused();
  await page.keyboard.type('Typed, never clicked');
  await page.keyboard.press('ControlOrMeta+Enter');
  await expect(page.getByText(/SUN-\d+ created/)).toBeVisible();
  await expect(dialog).toBeHidden();
});

test('⌘B is ignored while typing in the comment box, the description and the create dialog', async ({ page }) => {
  await open(page, 'tasks/SUN-142');
  await expect(sidebar(page)).toBeVisible();
  const comment = main(page).getByRole('textbox', { name: 'Comment' });
  await comment.fill('half a thought');
  await comment.press('ControlOrMeta+b');
  await expect(sidebar(page)).toBeVisible();
  await expect(comment).toHaveValue('half a thought');
  await main(page).getByRole('textbox', { name: 'Description' }).press('ControlOrMeta+b');
  await expect(sidebar(page)).toBeVisible();

  await sidebar(page).getByRole('button', { name: 'New task' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('textbox', { name: 'Title' }).press('ControlOrMeta+b');
  await expect(sidebar(page)).toBeVisible();
  await expect(dialog).toBeVisible();
});

test('Escape closes only the topmost layer: a menu, then the dialog; never the page behind', async ({ page }) => {
  await open(page, 'tasks/SUN-142');
  await main(page).getByRole('button', { name: 'Priority: No priority' }).click();
  await expect(page.getByRole('menu', { name: 'Priority' })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.getByRole('menu', { name: 'Priority' })).toBeHidden();
  await expect(page).toHaveURL(/#\/tasks\/SUN-142/);
  await expect(main(page).getByRole('heading', { level: 1 })).toHaveText('Fix login timeout on mobile');
  await expect(main(page).getByRole('button', { name: 'Priority: No priority' })).toBeFocused();

  // In the create dialog: the pill menu goes first, then the dialog, and the page stays.
  await sidebar(page).getByRole('button', { name: 'New task' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('textbox', { name: 'Title' }).fill('Not lost to Escape');
  await dialog.getByRole('button', { name: /^Priority:/ }).click();
  await expect(page.getByRole('menu', { name: 'Priority' })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.getByRole('menu', { name: 'Priority' })).toBeHidden();
  await expect(dialog).toBeVisible();
  await expect(dialog.getByRole('textbox', { name: 'Title' })).toHaveValue('Not lost to Escape');
  await page.keyboard.press('Escape');
  await expect(dialog).toBeHidden();
  await expect(page).toHaveURL(/#\/tasks\/SUN-142/);
});

test('old links: ?board= and id ?workspace= become the slug, a ULID task link its key', async ({ page }) => {
  await open(page, `knowledge?board=${MAIN}`);
  await expect(page).toHaveURL(/#\/knowledge\?workspace=main$/);
  await expect(page.getByRole('heading', { level: 1 })).toBeVisible();

  await open(page, `tasks?workspace=${MAIN}`);
  await expect(page).toHaveURL(/#\/tasks\?workspace=main$/);

  // The other params of an old link survive both rewrites.
  await open(page, `knowledge?board=${MAIN}&keep=1`);
  await expect(page).toHaveURL(/#\/knowledge\?keep=1&workspace=main$/);

  await open(page, `tasks/01JAT0142${'0'.repeat(17)}?board=${MAIN}`);
  await expect(page).toHaveURL(/#\/tasks\/SUN-142\?workspace=main$/);
  await expect(main(page).getByRole('heading', { level: 1 })).toHaveText('Fix login timeout on mobile');
});

test.describe('360 px', () => {
  test.use({ viewport: { width: 360, height: 740 } });

  for (const route of [
    'tasks',
    'projects',
    'agents',
    'tasks/SUN-142',
    'settings/workspace',
    'settings/access',
    'settings/code',
  ]) {
    test(`#/${route} fits without sideways scrolling`, async ({ page }) => {
      await open(page, route);
      await expect(page.getByRole('heading', { level: 1 }).first()).toBeVisible();
      await no_page_overflow(page, route);
    });
  }

  test('every task view and a project page fit', async ({ page }) => {
    await open(page, 'tasks');
    for (const view of ['List', 'Table', 'Swimlane', 'Board']) {
      await page.getByRole('button', { name: /^View: / }).click();
      await page.getByRole('menu', { name: 'View' }).getByRole('menuitemradio', { name: view }).click();
      await expect(page.getByRole('button', { name: `View: ${view}` })).toBeVisible();
      await no_page_overflow(page, `the ${view} view`);
    }
    await open(page, 'projects');
    await page.getByRole('button', { name: 'New project' }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Name').fill('A project with a rather long name for a phone');
    await dialog.getByRole('button', { name: 'Create project' }).click();
    await expect(dialog).toBeHidden();
    await no_page_overflow(page, 'the projects list');
    await main(page).getByRole('link', { name: 'A project with a rather long name for a phone' }).click();
    await expect(page).toHaveURL(/#\/projects\/[^/?]+/);
    await expect(page.getByRole('heading', { level: 1 }).first()).toBeVisible();
    await no_page_overflow(page, 'a project');
  });

  test('the create dialog and its menus fit', async ({ page }) => {
    await open(page, 'tasks');
    await page.getByRole('button', { name: 'New task' }).first().click();
    const dialog = page.getByRole('dialog');
    await expect(dialog.getByRole('textbox', { name: 'Title' })).toBeVisible();
    const box = (await dialog.boundingBox())!;
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(360);
    await dialog.getByRole('button', { name: /^Assignee:/ }).click();
    const menu = (await page.getByRole('menu', { name: 'Assignee' }).boundingBox())!;
    expect(menu.x).toBeGreaterThanOrEqual(0);
    expect(menu.x + menu.width).toBeLessThanOrEqual(360);
    await no_page_overflow(page, 'the create dialog');
  });
});
