// Tasks views: Board, List, Table, Swimlane; the Display options; drag and drop with the kernel's move
// rules; "Move to…" for keyboard and touch; the view kept per workspace in this browser.
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, route = 'tasks') => page.goto(`/?mock=1#/${route}`);
const card = (page: Page, key: string) => page.locator('.task-card').filter({ has: page.getByRole('link', { name: new RegExp(`^${key} `) }) });
const column = (page: Page, name: string) => page.getByRole('region', { name, exact: true });

async function pick_view(page: Page, name: string) {
  await page.getByRole('button', { name: /^View: / }).click();
  await page.getByRole('menu', { name: 'View' }).getByRole('menuitemradio', { name }).click();
}

async function display(page: Page) {
  await page.getByRole('button', { name: 'Display: Display' }).click();
  return page.getByRole('dialog', { name: 'Display' });
}

test.use({ viewport: { width: 1280, height: 800 } });

test('switch all four views; the choice survives a reload', async ({ page }) => {
  await open(page);
  await expect(page.getByRole('button', { name: 'View: Board' })).toBeVisible();
  await expect(column(page, 'Todo')).toBeVisible();

  await pick_view(page, 'List');
  await expect(page.getByRole('button', { name: /^Todo\s*2$/ })).toHaveAttribute('aria-expanded', 'true');
  await expect(page.getByRole('link', { name: /^SUN-151 / })).toBeVisible();
  // Groups collapse.
  await page.getByRole('button', { name: /^Todo\s*2$/ }).click();
  await expect(page.getByRole('link', { name: /^SUN-151 / })).toBeHidden();

  await pick_view(page, 'Table');
  const table = page.getByRole('table');
  await expect(table.locator('thead').getByRole('columnheader')).toHaveText([/^Key/, /^Title/, /^Status/, /^Priority/, /^Assignee/, /^Project/, /^Created/, /^Updated/]);
  // Grouping None, then sort by title: the header says so, the rows follow.
  await (await display(page)).getByLabel('Grouping').selectOption('none');
  await page.keyboard.press('Escape');
  await table.getByRole('button', { name: 'Title' }).click();
  await expect(table.getByRole('columnheader', { name: 'Title' })).toHaveAttribute('aria-sort', 'ascending');
  const titles = await table.locator('tbody .table__title').allTextContents();
  expect(titles).toEqual([...titles].sort((a, b) => a.localeCompare(b)));

  await pick_view(page, 'Swimlane');
  await expect(page.getByRole('region', { name: /^Bug fix/ }).first()).toBeVisible();

  await page.reload();
  await expect(page.getByRole('button', { name: 'View: Swimlane' })).toBeVisible();
  await pick_view(page, 'Table');
  await page.reload();
  await expect(page.getByRole('table').getByRole('columnheader', { name: 'Title' })).toHaveAttribute('aria-sort', 'ascending');
});

test('a broken saved view falls back to the board', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('sun_task_view:00000000000000000000000001', '{"view":"nope'));
  await open(page);
  await expect(page.getByRole('button', { name: 'View: Board' })).toBeVisible();
});

// Wide enough for every column at once: Playwright doesn't scroll a board while a drag is held.
test.describe('drag and drop', () => {
  test.use({ viewport: { width: 2700, height: 900 } });

  test('drag Todo → Blocked moves the task', async ({ page }) => {
    await open(page);
    await card(page, 'SUN-151').dragTo(column(page, 'Blocked'));
    await expect(column(page, 'Blocked').getByRole('link', { name: /^SUN-151 / })).toBeVisible();
    await expect(column(page, 'Todo').getByRole('link', { name: /^SUN-151 / })).toHaveCount(0);
  });

  test('an illegal drag snaps back with the kernel’s reason', async ({ page }) => {
    await open(page);
    await card(page, 'SUN-134').dragTo(column(page, 'In progress'));
    await expect(page.locator('.ml-toast')).toContainText('A task can\'t move from Failed to In progress.');
    await expect(column(page, 'Failed').getByRole('link', { name: /^SUN-134 / })).toBeVisible();
    await expect(column(page, 'In progress').getByRole('link', { name: /^SUN-134 / })).toHaveCount(0);
  });

  test('manual order: drag inside a column, and Move up from the menu', async ({ page }) => {
    await open(page);
    const todo = column(page, 'Todo').locator('.task-card__title');
    await expect(todo).toHaveText(['Refactor checkout button states', 'Weekly summary for the founders']);
    await card(page, 'SUN-151').dragTo(card(page, 'SUN-150'), { targetPosition: { x: 20, y: 4 } });
    await expect(todo).toHaveText(['Weekly summary for the founders', 'Refactor checkout button states']);

    await page.getByRole('button', { name: 'Move SUN-150 to…' }).click();
    await page.getByRole('menuitem', { name: 'Move up' }).click();
    await expect(todo).toHaveText(['Refactor checkout button states', 'Weekly summary for the founders']);
    await expect(page.getByRole('button', { name: 'Move SUN-150 to…' })).toBeFocused();
  });
});

test('keyboard: "Move to…" lists only legal moves and keeps focus on the card', async ({ page }) => {
  await open(page);
  const button = page.getByRole('button', { name: 'Move SUN-134 to…' });
  await button.focus();
  await page.keyboard.press('Enter');
  const menu = page.getByRole('menu', { name: 'Move SUN-134 to…' });
  // Failed reopens to Backlog or Todo only.
  await expect(menu.getByRole('menuitem')).toHaveText(['Backlog', 'Todo']);
  await expect(menu.getByRole('menuitem', { name: 'Backlog' })).toBeFocused();
  await page.keyboard.press('ArrowDown');
  await page.keyboard.press('Enter');
  await expect(column(page, 'Todo').getByRole('link', { name: /^SUN-134 / })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Move SUN-134 to…' })).toBeFocused();
  await expect(page.locator('.ml-toast')).toContainText('SUN-134 moved to Todo');

  // A workflow task never offers Verifying (the engine's), and Escape closes the menu.
  await page.getByRole('button', { name: 'Move SUN-151 to…' }).click();
  await expect(page.getByRole('menu', { name: 'Move SUN-151 to…' }).getByRole('menuitem', { name: 'Verifying' })).toHaveCount(0);
  await page.keyboard.press('Escape');
  await expect(page.getByRole('menu', { name: 'Move SUN-151 to…' })).toBeHidden();
  await expect(page.getByRole('button', { name: 'Move SUN-151 to…' })).toBeFocused();
});

test.describe('swimlane drag and drop', () => {
  test.use({ viewport: { width: 2700, height: 1600 } });

  test('swimlane by priority: dragging across lanes sets the priority', async ({ page }) => {
    await open(page);
    await pick_view(page, 'Swimlane');
    await (await display(page)).getByLabel('Swimlane rows').selectOption('priority');
    await page.keyboard.press('Escape');
    await expect(page.locator('.lane__title')).toHaveText([/^Urgent/, /^High/, /^Medium/, /^Low/, /^No priority/]);
    await card(page, 'SUN-151').dragTo(page.getByRole('region', { name: 'Urgent · Todo' }));
    await expect(page.getByRole('region', { name: 'Urgent · Todo' }).getByRole('link', { name: /^SUN-151 / })).toBeVisible();
    await expect(card(page, 'SUN-151')).toContainText('Priority Urgent');
  });

  test('swimlane by assignee: a workflow lane takes only its own cards; an agent lane assigns its agent', async ({ page }) => {
    // Tall enough for every lane: a drag that has to scroll the page drops nowhere.
    await page.setViewportSize({ width: 2700, height: 3000 });
    await open(page);
    await pick_view(page, 'Swimlane');
    const own = page.getByRole('region', { name: /^Weekly summary of work and cost · Blocked$/ });
    await card(page, 'SUN-151').dragTo(page.getByRole('region', { name: /^Bug fix → verified PR · Blocked$/ }));
    await expect(page.getByRole('region', { name: /^Bug fix → verified PR · Blocked$/ }).getByRole('link', { name: /^SUN-151 / })).toHaveCount(0);
    await card(page, 'SUN-151').dragTo(own);
    await expect(own.getByRole('link', { name: /^SUN-151 / })).toBeVisible();

    // Every agent has a row, empty ones too; dropping there assigns it (and its run starts).
    await expect(page.locator('.lane__title')).toHaveText([/^Ada/, /^Night Owl/, /^Bug fix/, /^Campaign/, /^Interview/, /^Launch/, /^Mobile/, /^UI refactor/, /^Weekly/]);
    await card(page, 'SUN-151').dragTo(page.getByRole('region', { name: 'Ada · Todo' }));
    await expect(page.getByRole('region', { name: /^Ada · / }).getByRole('link', { name: /^SUN-151 / })).toBeVisible();
    await expect(card(page, 'SUN-151')).toContainText('Ada');
  });
});

test('show cancelled', async ({ page }) => {
  await open(page);
  await page.getByRole('button', { name: 'Move SUN-151 to…' }).click();
  await page.getByRole('menuitem', { name: 'Cancelled' }).click();
  await expect(page.getByRole('link', { name: /^SUN-151 / })).toHaveCount(0);
  const panel = await display(page);
  await panel.getByText('Show cancelled').click();
  await expect(panel.getByRole('switch', { name: 'Show cancelled' })).toBeChecked();
  await expect(column(page, 'Cancelled').getByRole('link', { name: /^SUN-151 / })).toBeVisible();
});

test('360 px: the board scrolls sideways, menus fit, no page overflow', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page);
  const overflow = () => page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(await overflow()).toBeLessThanOrEqual(0);
  const board = page.locator('.task-columns');
  expect(await board.evaluate((el) => el.scrollWidth > el.clientWidth)).toBe(true);

  await page.getByRole('button', { name: 'Move SUN-150 to…' }).click();
  const box = await page.getByRole('menu', { name: 'Move SUN-150 to…' }).boundingBox();
  expect(box!.x).toBeGreaterThanOrEqual(0);
  expect(box!.x + box!.width).toBeLessThanOrEqual(360);
  await page.getByRole('menuitem', { name: 'Blocked' }).click();
  await expect(column(page, 'Blocked').getByRole('link', { name: /^SUN-150 / })).toBeAttached();

  for (const name of ['List', 'Table', 'Swimlane']) {
    await pick_view(page, name);
    expect(await overflow()).toBeLessThanOrEqual(0);
  }
});
