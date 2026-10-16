import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, route = '') => page.goto(`/?mock=1#/${route}`);

test('tasks show a column per status, Cancelled hidden', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page);
  // The board is the default view (task_views.spec.ts covers the others).
  await expect(page.getByRole('button', { name: 'View: Board' })).toBeVisible();
  for (const name of ['Backlog', 'Todo', 'In progress', 'Verifying', 'Needs you', 'Done', 'Blocked', 'Failed']) {
    await expect(page.getByRole('heading', { name, exact: true })).toBeVisible();
  }
  await expect(page.getByRole('heading', { name: 'Cancelled', exact: true })).toHaveCount(0);
  await expect(page.getByRole('link', { name: /SUN-142/ })).toBeVisible();
  // Two approvals, and SUN-133: its run had no approval step, so a person marks it Done.
  await expect(page.getByRole('heading', { name: 'Needs you' }).locator('..')).toContainText('3');
});

test('task page shows steps, timeline with collapsed output, and steering', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page);
  await page.getByRole('link', { name: /SUN-142/ }).click();
  const panel = page.getByRole('main');
  await expect(panel.getByRole('heading', { name: 'Fix login timeout on mobile' })).toBeVisible();
  await expect(panel.getByRole('heading', { name: 'Steps' })).toBeVisible();
  await expect(panel.getByText('api/auth/session.go:41')).toBeHidden();
  await panel.getByRole('button', { name: 'Show output' }).click();
  await expect(panel.getByText('api/auth/session.go:41')).toBeVisible();
  await panel.getByRole('textbox', { name: 'Message to the agent' }).fill('Keep the diff small');
  await panel.getByRole('button', { name: 'Send' }).click();
  await expect(panel.getByText('You: Keep the diff small')).toBeVisible();
  // A full page: the breadcrumb goes back to the board.
  await panel.getByRole('navigation', { name: 'Breadcrumb' }).getByRole('link', { name: 'Tasks' }).click();
  await expect(page.getByRole('link', { name: /SUN-142/ })).toBeVisible();
});

test('approve from the approvals page moves the task to done', async ({ page }) => {
  await open(page, 'approvals');
  // Task approvals; memory proposals wait below them as their own cards.
  const tasks = page.getByRole('article').filter({ hasText: /SUN-\d+/ });
  await expect(tasks).toHaveCount(2);
  await tasks.filter({ hasText: 'SUN-140' }).getByRole('button', { name: 'Approve' }).click();
  await expect(tasks).toHaveCount(1);
  await page.getByRole('link', { name: /^Tasks/ }).click();
  const done = page.getByRole('region', { name: 'Done' });
  await expect(done.getByRole('link', { name: /SUN-140/ })).toBeVisible();
});

test('new task lands in Todo', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page);
  await page.getByRole('complementary', { name: 'Main' }).getByRole('button', { name: 'New task' }).click();
  await page.getByRole('textbox', { name: 'Title' }).fill('Draft the November newsletter');
  await page.getByRole('button', { name: 'Create task' }).click();
  await expect(page.getByRole('main').getByRole('button', { name: 'Status: Todo' })).toBeVisible();
  await page.getByRole('main').getByRole('link', { name: 'Tasks', exact: true }).click();
  await expect(page.getByRole('region', { name: 'Todo' }).getByText('Draft the November newsletter')).toBeVisible();
});

// Settings opens on Account (a form); its rows live on Settings → General.
for (const route of ['runtimes', 'hosts', 'mods', 'catalog', 'settings/overview']) {
  test(`${route} page renders rows`, async ({ page }) => {
    await open(page, route);
    await expect(page.getByRole('heading', { level: 1 })).toHaveText(new RegExp(route.split('/')[0], 'i'));
    await expect(page.getByRole('article').first()).toBeVisible();
  });
}

test('self-hosted: icons and fonts come from the origin, none fail', async ({ page, baseURL }) => {
  const foreign: string[] = [];
  const broken: string[] = [];
  page.on('request', (r) => !r.url().startsWith(baseURL!) && !r.url().startsWith('data:') && foreign.push(r.url()));
  page.on('response', (r) => r.status() >= 400 && broken.push(`${r.status()} ${r.url()}`));
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page);
  await page.getByRole('link', { name: /SUN-142/ }).click();
  await expect(page.getByRole('main').getByText('Live')).toBeVisible();
  await page.waitForLoadState('networkidle');
  expect(foreign).toEqual([]);
  expect(broken).toEqual([]);
});

test('stop, resume and queue a message from the task page', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'tasks/SUN-142');
  const panel = page.getByRole('main');
  await panel.getByRole('textbox', { name: 'Message to the agent' }).fill('Add a changelog line');
  await panel.getByRole('button', { name: 'Queue for next step' }).click();
  await expect(panel.getByText('Queued for the next step: Add a changelog line')).toBeVisible();
  // One way to Blocked: Stop in the header (the steer bar has no Pause).
  await expect(panel.getByRole('button', { name: 'Pause' })).toHaveCount(0);
  await panel.getByRole('button', { name: 'Stop' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Stop' }).click();
  await expect(panel.getByText('Paused by a person')).toBeVisible();
  await expect(panel.getByRole('button', { name: 'Status: Blocked' })).toBeVisible();
  await expect(panel.getByRole('button', { name: 'Approve' })).toBeHidden();
  // A paused run is not an approval: the count stays at the two seeded ones.
  await expect(page.getByRole('complementary', { name: 'Main' }).getByRole('link', { name: /Approvals/ })).toHaveText(/2$/);
  await panel.getByRole('button', { name: 'Resume' }).click();
  await expect(panel.getByText('Paused by a person')).toBeHidden();
  await expect(panel.getByRole('textbox', { name: 'Message to the agent' })).toBeVisible();
});

test('cancel asks first', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await open(page, 'tasks/SUN-142');
  const panel = page.getByRole('main');
  await panel.getByRole('button', { name: 'Cancel', exact: true }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog).toContainText('can’t be undone');
  await dialog.getByRole('button', { name: 'Cancel task' }).click();
  // It stays open, now Cancelled.
  await expect(page).toHaveURL(/#\/tasks\/SUN-142/);
  await expect(panel.getByRole('button', { name: 'Status: Cancelled' })).toBeVisible();
});

test('add host shows a single-use join command', async ({ page }) => {
  await open(page, 'hosts');
  await page.getByRole('button', { name: 'Add host' }).click();
  const command = page.getByLabel('Run this on the machine');
  await expect(command).toHaveValue(/^sun host join --kernel http:\/\/\S+ sun_enrol_\w+$/);
  await expect(page.getByText(/Works once, for (59|60) min/)).toBeVisible();
  await page.getByLabel('Host name').fill("harry's mac");
  await expect(command).toHaveValue(/ --name 'harry'\\''s mac' sun_enrol_/);
});

test('360 px: bottom tabs, no page overflow, task opens full screen', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page);
  const tabs = page.getByRole('navigation', { name: 'Main' });
  await expect(tabs).toBeVisible();
  await expect(page.getByRole('complementary', { name: 'Main' })).toBeHidden();
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(0);
  await page.getByRole('link', { name: /SUN-151/ }).click();
  const panel = page.getByRole('main');
  await expect(panel.getByRole('heading', { name: 'Weekly summary for the founders' })).toBeVisible();
  expect((await panel.boundingBox())?.width).toBeCloseTo(360, 0);
  await tabs.getByRole('link', { name: 'More' }).click();
  await page.getByRole('link', { name: 'Hosts' }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Hosts');
});
