// The create dialog and the task page (#/tasks/<KEY>): assignee kinds and the run hint, Create another,
// comments that wake an agent, Stop, inline edits, legal status moves, PRs, the execution log, old links, 360 px.
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, route = '') => page.goto(`/?mock=1#/${route}`);
const main = (page: Page) => page.getByRole('main');

async function new_task(page: Page) {
  await page.getByRole('complementary', { name: 'Main' }).getByRole('button', { name: 'New task' }).click();
  return page.getByRole('dialog');
}

async function pick(page: Page, pill: string, item: string) {
  await page.getByRole('dialog').getByRole('button', { name: new RegExp(`^${pill}:`) }).click();
  await page.getByRole('menuitemradio', { name: new RegExp(`^${item}`) }).click();
}

test.beforeEach(async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 860 });
});

test('create: each assignee kind says what happens, and Create another keeps the dialog open', async ({ page }) => {
  await open(page);
  const dialog = await new_task(page);
  await expect(dialog.getByRole('heading', { name: 'Main › New task' })).toBeVisible();
  const hint = dialog.getByRole('status');

  // No default workflow on Main: a plain task.
  await expect(dialog.getByRole('button', { name: 'Assignee: None' })).toBeVisible();
  await expect(hint).toHaveText('A plain task: nothing runs.');

  // The assignee menu: workflows, then agents (a not-ready one disabled with why), then None.
  await dialog.getByRole('button', { name: 'Assignee: None' }).click();
  const menu = page.getByRole('menu', { name: 'Assignee' });
  await expect(menu.getByText('Workflow', { exact: true })).toBeVisible();
  await expect(menu.getByText('Agents', { exact: true })).toBeVisible();
  await expect(menu.getByRole('menuitemradio', { name: /^Ada/ })).toContainText('Ready');
  const owl = menu.getByRole('menuitemradio', { name: /^Night Owl/ });
  await expect(owl).toHaveAttribute('aria-disabled', 'true');
  await expect(owl).toContainText('gemini-cli: no host reports it');
  await owl.dispatchEvent('click');
  await expect(menu).toBeVisible();
  await menu.getByRole('menuitemradio', { name: /^Ada/ }).click();
  await expect(hint).toHaveText('Ada will start right after creation.');

  await pick(page, 'Status', 'Backlog');
  await expect(hint).toHaveText('Won’t start while in Backlog.');
  await pick(page, 'Status', 'Todo');
  await pick(page, 'Assignee', 'Bug fix → verified PR');
  await expect(hint).toHaveText('Bug fix → verified PR will start right after creation.');
  await pick(page, 'Priority', 'High');

  // Create another: the dialog stays, the title clears, the rest stays.
  await dialog.getByRole('switch', { name: 'Create another' }).locator('xpath=..').click();
  await dialog.getByRole('textbox', { name: 'Title' }).fill('Fix the flaky login test');
  await dialog.getByRole('button', { name: 'Create task' }).click();
  await expect(page.getByText('SUN-152 created')).toBeVisible();
  await expect(dialog.getByRole('textbox', { name: 'Title' })).toHaveValue('');
  await expect(dialog.getByRole('button', { name: 'Priority: High' })).toBeVisible();

  // ⌘↵ / Ctrl+↵ creates, and opens the new task's page.
  await dialog.getByRole('switch', { name: 'Create another' }).locator('xpath=..').click();
  await pick(page, 'Assignee', 'None');
  await dialog.getByRole('textbox', { name: 'Title' }).fill('Write the release notes');
  await dialog.getByRole('textbox', { name: 'Description' }).press('ControlOrMeta+Enter');
  await expect(page).toHaveURL(/#\/tasks\/SUN-153/);
  await expect(main(page).getByRole('heading', { name: 'Write the release notes' })).toBeVisible();
  await expect(main(page).getByRole('region', { name: /Execution log/ })).toContainText('A plain task: nothing runs.');

  // The workflow task waits in Todo for a runtime.
  await page.getByRole('link', { name: 'Tasks' }).first().click();
  await expect(page.getByRole('region', { name: 'Todo' }).getByRole('link', { name: /SUN-152/ })).toContainText('Bug fix → verified PR');
});

test('an agent task: the live chip, a comment wakes the agent, Stop moves it to Blocked', async ({ page }) => {
  await open(page);
  const dialog = await new_task(page);
  await dialog.getByRole('textbox', { name: 'Title' }).fill('Tidy the README');
  await pick(page, 'Assignee', 'Ada');
  await pick(page, 'Status', 'Backlog');
  await dialog.getByRole('button', { name: 'Create task' }).click();
  await expect(page).toHaveURL(/#\/tasks\/SUN-152/);

  // Backlog: nothing runs until a comment wakes Ada.
  await expect(main(page).getByText('Ada is working')).toHaveCount(0);
  const activity = main(page).getByRole('region', { name: 'Activity' });
  await expect(activity).toContainText('You created the task in Backlog');
  await expect(main(page).getByText('A comment wakes Ada')).toBeVisible();
  await main(page).getByRole('textbox', { name: 'Comment' }).fill('Start with the install section.');
  await main(page).getByRole('textbox', { name: 'Comment' }).press('ControlOrMeta+Enter');
  await expect(activity.getByText('Start with the install section.')).toBeVisible();
  await expect(activity).toContainText('Ada started a run for a comment');
  await expect(main(page).getByText('Ada is working')).toBeVisible();
  await expect(main(page).getByRole('button', { name: 'Status: In progress' })).toBeVisible();
  // The comment box is the one way to talk to an agent; its hidden system workflow isn't shown.
  await expect(main(page).getByRole('textbox', { name: 'Message to the agent' })).toHaveCount(0);
  await expect(main(page).getByRole('region', { name: 'Properties' }).locator('dt')).toHaveText(['Status', 'Priority', 'Assignee', 'Project']);

  // A second comment while she works steers her.
  await main(page).getByRole('textbox', { name: 'Comment' }).fill('Keep it short.');
  await main(page).getByRole('button', { name: /^Comment/ }).click();
  await expect(main(page).getByRole('region', { name: /Execution log/ }).getByText('You: Keep it short.')).toBeVisible();

  await main(page).getByRole('button', { name: 'Stop' }).click();
  const confirm = page.getByRole('dialog').filter({ hasText: 'Stop Ada?' });
  await expect(confirm).toContainText('moves to Blocked');
  await confirm.getByRole('button', { name: 'Stop' }).click();
  await expect(main(page).getByRole('button', { name: 'Status: Blocked' })).toBeVisible();
  await expect(main(page).getByText('Ada is working')).toHaveCount(0);
  await expect(activity).toContainText('You moved it from In progress to Blocked');
});

test('title and description edit in place: Enter or blur saves, Escape cancels', async ({ page }) => {
  await open(page, 'tasks/SUN-142');
  const title = main(page).getByRole('heading', { level: 1 });
  await expect(title).toHaveText('Fix login timeout on mobile');
  await title.click();
  await page.keyboard.press('ControlOrMeta+A');
  await page.keyboard.type('Draft, not kept');
  await page.keyboard.press('Escape');
  await expect(title).toHaveText('Fix login timeout on mobile');

  await title.click();
  await page.keyboard.press('ControlOrMeta+A');
  await page.keyboard.type('Fix the login timeout on iOS');
  await page.keyboard.press('Enter');
  const activity = main(page).getByRole('region', { name: 'Activity' });
  await expect(activity).toContainText('You renamed it to “Fix the login timeout on iOS”');
  await expect(title).toHaveText('Fix the login timeout on iOS');

  const body = main(page).getByRole('textbox', { name: 'Description' });
  await body.fill('Repro: open the app after 30 minutes idle.');
  await body.blur();
  await expect(activity).toContainText('You edited the description');
});

test('properties: the status menu offers only legal moves; priority and project save', async ({ page }) => {
  await open(page, 'tasks/SUN-136');
  // Done is final: the menu shows only Done.
  await main(page).getByRole('button', { name: 'Status: Done' }).click();
  await expect(page.getByRole('menu', { name: 'Status' }).getByRole('menuitemradio')).toHaveText(['Done']);
  await page.keyboard.press('Escape');

  await open(page, 'tasks/SUN-142');
  await main(page).getByRole('button', { name: 'Status: In progress' }).click();
  const moves = page.getByRole('menu', { name: 'Status' }).getByRole('menuitemradio');
  // A task that runs a workflow: Verifying is the engine's, and Done needs Needs you first.
  await expect(moves).toHaveText(['In progress', 'Backlog', 'Todo', 'Needs you', 'Blocked', 'Failed', 'Cancelled']);
  await page.keyboard.press('Escape');

  await main(page).getByRole('button', { name: 'Priority: No priority' }).click();
  await page.getByRole('menuitemradio', { name: 'Urgent' }).click();
  await expect(main(page).getByRole('button', { name: 'Priority: Urgent' })).toBeVisible();
  await expect(main(page).getByRole('region', { name: 'Activity' })).toContainText('You set the priority to Urgent');
});

test('pull requests and the execution log', async ({ page }) => {
  await open(page, 'tasks/SUN-136');
  const prs = main(page).getByRole('region', { name: 'Pull requests' });
  await expect(prs.getByRole('link', { name: /Bump wallet SDK to v2\.3/ })).toHaveAttribute('href', 'https://github.com/madlabs/mobile/pull/477');
  await expect(prs).toContainText('Merged');

  await open(page, 'tasks/SUN-142');
  const log = main(page).getByRole('region', { name: /Execution log/ });
  const run = log.getByRole('button', { name: /Run 1/ });
  // The latest run is open: its sections show.
  await expect(run).toHaveAttribute('aria-expanded', 'true');
  await expect(log.getByRole('heading', { name: 'Steps' })).toBeVisible();
  await expect(log.getByRole('heading', { name: 'Timeline' })).toBeVisible();
  await run.click();
  await expect(log.getByRole('heading', { name: 'Steps' })).toHaveCount(0);
  await run.click();
  await expect(log.getByRole('heading', { name: 'Steps' })).toBeVisible();
  await expect(main(page).getByRole('region', { name: 'Details' })).toContainText('Created by');
});

test('an older run shows its own steps, gates and timeline; a PR closed without merging says Closed', async ({ page }) => {
  await open(page, 'tasks/SUN-136');
  const log = main(page).getByRole('region', { name: /Execution log/ });
  await expect(log.getByRole('button', { name: /Run 2/ })).toHaveAttribute('aria-expanded', 'true');
  const first = log.getByRole('button', { name: /Run 1/ });
  await first.click();
  await expect(first).toHaveAttribute('aria-expanded', 'true');
  await expect(log.getByRole('heading', { name: 'Steps' })).toBeVisible();
  // Run 1 stopped on its gates; the latest run went all the way.
  await expect(log.locator('.step--stuck')).toHaveText(/Gates/);
  const gates = log.locator('.gates');
  await expect(gates.getByRole('listitem').filter({ hasText: 'Tests' })).toContainText('failed');
  await expect(log.getByRole('heading', { name: 'Timeline' })).toBeVisible();
  await expect(log).toContainText('Tests failed · 1 of 88');
  // The latest run's own sections stay with it.
  await expect(log.getByRole('heading', { name: 'Cost budget' })).toHaveCount(0);

  await open(page, 'tasks/SUN-134');
  await expect(main(page).getByRole('region', { name: 'Pull requests' })).toContainText('Closed');
});

test('an old ULID link opens the task under its key', async ({ page }) => {
  await open(page, 'tasks/01JAT0142' + '0'.repeat(17));
  await expect(page).toHaveURL(/#\/tasks\/SUN-142\?workspace=main$/);
  await expect(main(page).getByRole('heading', { name: 'Fix login timeout on mobile' })).toBeVisible();
});

test('360 px: one column, no page overflow, the comment box in reach', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'tasks/SUN-142');
  await expect(main(page).getByRole('heading', { name: 'Fix login timeout on mobile' })).toBeVisible();
  await expect(main(page).getByRole('textbox', { name: 'Comment' })).toBeInViewport();
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(0);
  const side = main(page).getByRole('complementary', { name: 'Task details' });
  const article = main(page).getByRole('article');
  expect((await side.boundingBox())!.y).toBeGreaterThan((await article.boundingBox())!.y);
  const scroll = await main(page).locator('.task-page').evaluate((el) => el.scrollWidth - el.clientWidth);
  expect(scroll).toBeLessThanOrEqual(0);
});
