// Projects: list with counts, create, edit, delete; a project's page shows the Tasks views of its tasks.
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, route = 'projects') => page.goto(`/?mock=1#/${route}`);
const row = (page: Page, name: string) => page.getByRole('article').filter({ has: page.getByRole('link', { name, exact: true }) });

test.use({ viewport: { width: 1280, height: 800 } });

test('create, edit, fill, open and delete a project', async ({ page }) => {
  await open(page);
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Projects');
  await expect(page.getByText('No projects yet')).toBeVisible();

  // Create: a name is required, and unique ignoring case.
  await page.getByRole('button', { name: 'New project' }).click();
  let dialog = page.getByRole('dialog');
  await dialog.getByRole('button', { name: 'Create project' }).click();
  await expect(dialog).toContainText('Give the project a name.');
  await dialog.getByLabel('Name').fill('Mobile launch');
  await dialog.getByLabel('Description').fill('Everything for the 2.4 app release');
  await dialog.getByRole('button', { name: 'Create project' }).click();
  await expect(dialog).toBeHidden();
  await expect(row(page, 'Mobile launch')).toContainText('Everything for the 2.4 app release');
  await expect(row(page, 'Mobile launch')).toContainText('0 open · 0 done');

  await page.getByRole('button', { name: 'New project' }).click();
  dialog = page.getByRole('dialog');
  await dialog.getByLabel('Name').fill('mobile LAUNCH');
  await dialog.getByRole('button', { name: 'Create project' }).click();
  await expect(dialog).toContainText('already has a project named');
  await dialog.getByRole('button', { name: 'Cancel' }).click();

  // Edit from the row menu.
  await page.getByRole('button', { name: 'Actions for Mobile launch' }).click();
  await page.getByRole('menuitem', { name: 'Edit' }).click();
  dialog = page.getByRole('dialog');
  await dialog.getByLabel('Name').fill('Mobile 2.4');
  await dialog.getByRole('button', { name: 'Save' }).click();
  await expect(row(page, 'Mobile 2.4')).toBeVisible();

  // Put two tasks in it from the swimlane by project.
  await page.getByRole('complementary', { name: 'Main' }).getByRole('link', { name: /^Tasks/ }).click();
  await page.getByRole('button', { name: /^View: / }).click();
  await page.getByRole('menuitemradio', { name: 'Swimlane' }).click();
  await page.getByRole('button', { name: 'Display: Display' }).click();
  await page.getByRole('dialog', { name: 'Display' }).getByLabel('Swimlane rows').selectOption('project');
  await page.keyboard.press('Escape');
  for (const [key, status] of [['SUN-151', 'Todo'], ['SUN-136', 'Done']]) {
    const card = page.locator('.task-card').filter({ has: page.getByRole('link', { name: new RegExp(`^${key} `) }) });
    await card.dragTo(page.getByRole('region', { name: `Mobile 2.4 · ${status}` }));
    await expect(page.getByRole('region', { name: `Mobile 2.4 · ${status}` }).getByRole('link', { name: new RegExp(`^${key} `) })).toBeVisible();
  }

  await page.getByRole('complementary', { name: 'Main' }).getByRole('link', { name: 'Projects' }).click();
  await expect(row(page, 'Mobile 2.4')).toContainText('1 open · 1 done');

  // Its page: the same views, its tasks only; a task opens on its page, whose breadcrumb leads back to the project.
  await row(page, 'Mobile 2.4').getByRole('link', { name: 'Mobile 2.4' }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Mobile 2.4');
  await expect(page).toHaveURL(/#\/projects\/[^/?]+/);
  await expect(page.locator('.task-card')).toHaveCount(2);
  await page.getByRole('link', { name: /^SUN-151 / }).click();
  const panel = page.getByRole('main');
  await expect(panel.getByRole('heading', { name: 'Weekly summary for the founders' })).toBeVisible();
  await expect(page).toHaveURL(/#\/projects\/[^/?]+\/SUN-151/);
  await expect(panel.getByRole('button', { name: 'Project: Mobile 2.4' })).toBeVisible();
  await panel.getByRole('navigation', { name: 'Breadcrumb' }).getByRole('link', { name: 'Mobile 2.4' }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Mobile 2.4');

  // Delete keeps its tasks, outside any project.
  await page.getByRole('complementary', { name: 'Main' }).getByRole('link', { name: 'Projects' }).click();
  await page.getByRole('button', { name: 'Actions for Mobile 2.4' }).click();
  await page.getByRole('menuitem', { name: 'Delete' }).click();
  dialog = page.getByRole('dialog');
  await expect(dialog).toContainText('Its 2 tasks stay on the workspace, outside any project.');
  await dialog.getByRole('button', { name: 'Delete project' }).click();
  await expect(row(page, 'Mobile 2.4')).toHaveCount(0);
  await page.getByRole('complementary', { name: 'Main' }).getByRole('link', { name: /^Tasks/ }).click();
  await expect(page.getByRole('link', { name: /^SUN-151 / })).toBeVisible();
});

test('360 px: projects list fits', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page);
  await page.getByRole('button', { name: 'New project' }).click();
  await page.getByRole('dialog').getByLabel('Name').fill('A project with a rather long name for a phone');
  await page.getByRole('dialog').getByRole('button', { name: 'Create project' }).click();
  await expect(row(page, 'A project with a rather long name for a phone')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
});
