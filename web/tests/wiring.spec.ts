import { expect, test, type Page } from '@playwright/test';

// At 360 px, mock API: device results on the task, starring a learning note, the builder's dry run
// and comment-keeping block edits, role suggestions for templates, and the plan's limits.
test.use({ viewport: { width: 360, height: 740 } });

const open = async (page: Page, route: string) => {
  await page.goto(`/?mock=1#/${route}`);
  await page.waitForLoadState('networkidle');
};
const overflow = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - innerWidth);
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==', 'base64');

test('device results: device, verdict, lazy screenshots, collapsed output, PR attachment', async ({ page }) => {
  await page.route('**/screenshots/**', (route) => route.fulfill({ contentType: 'image/png', body: PNG }));
  await open(page, 'tasks/SUN-145');
  const card = page.getByRole('article', { name: 'Device check' });
  await expect(card).toContainText('iPhone 15 (iOS 17.0)');
  await expect(card).toContainText('Failed');
  await expect(card).toContainText('Attached to the PR');
  const shots = card.getByRole('img');
  await expect(shots).toHaveCount(2);
  await expect(shots.first()).toHaveAttribute('alt', 'wallet.png');
  await expect(shots.first()).toHaveAttribute('loading', 'lazy');
  await expect(card.getByRole('link', { name: 'wallet.png' })).toHaveAttribute('href', /\/api\/v1\/runs\/01JAR0145\w+\/screenshots\/a{64}$/);
  await expect(card.locator('.event__detail')).toHaveCount(0);
  await card.getByRole('button', { name: 'Show output' }).click();
  await expect(card.locator('.event__detail')).toContainText('Element not found');
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

test('a learning note in the timeline can be starred into a memory proposal', async ({ page }) => {
  await open(page, 'tasks/SUN-145');
  const star = page.getByRole('button', { name: 'Propose this learning as memory' });
  await star.click();
  await expect(page.getByText('Proposed as memory')).toBeVisible();
  await expect(star).toHaveAttribute('aria-pressed', 'true');
  await open(page, 'memory');
  await expect(page.getByRole('region', { name: /Proposals/ })).toContainText('Wallet balances need a cache TTL');
  // The first-pass gate rate sits beside the repeat rate.
  const first = page.getByRole('region', { name: 'Gates passed first time' });
  await expect(first).toContainText('65%');
  await expect(first).toContainText('(34 of 52)');
  await expect(first).toContainText('Up from 56% to 71% over the last 4 weeks.');
});

test('builder: block edits keep comments, and a dry run shows order, mods and cost', async ({ page }) => {
  await open(page, 'workflows/new');
  await page.getByRole('tab', { name: 'YAML' }).click();
  await page.getByLabel('Workflow file').fill(
    '# Ship fixes\nname: ship\nsteps:\n  # the agent does the work\n  - id: work\n    type: agent\n    prompt: Fix it.\n  - id: tell\n    type: notify\n    to: ["teams:#eng"]\n',
  );
  await page.getByRole('tab', { name: 'Steps' }).click();
  await page.getByRole('button', { name: /^Approval/ }).click();
  await page.getByRole('tab', { name: 'YAML' }).click();
  const file = page.getByLabel('Workflow file');
  await expect(file).toHaveValue(/# Ship fixes\nname: ship\nsteps:\n  # the agent does the work\n  - id: work/);
  await expect(file).toHaveValue(/- id: approval\n\s+type: approval/);

  await expect(page.getByText('Valid')).toBeVisible();
  await page.getByRole('button', { name: 'Dry run' }).click();
  const dry = page.getByRole('region', { name: 'Dry run' });
  const order = dry.getByRole('list', { name: 'Run order' }).getByRole('listitem');
  await expect(order).toHaveCount(3);
  await expect(order.first()).toContainText('work');
  // The steps actually ran in the sandbox: each one's result, in order.
  const sim = dry.getByRole('region', { name: 'Simulated run' });
  await expect(sim).toContainText('Passed');
  const ran = sim.getByRole('listitem');
  await expect(ran).toHaveCount(3);
  await expect(ran.nth(0)).toContainText('work · Passed');
  await expect(ran.nth(0)).toContainText('Scripted fake runtime; no model ran');
  await expect(ran.filter({ hasText: 'approval' })).toContainText('Approved automatically');
  await expect(dry).toContainText('teams · not installed');
  await expect(dry).toContainText('The ecosystem default');
  await expect(dry).toContainText('About $0.62 for 1 agent step');
  expect(await overflow(page)).toBeLessThanOrEqual(0);
  await file.fill((await file.inputValue()).replace('Fix it.', 'Fix it well.'));
  await expect(dry).toContainText('Out of date');
});

test('picking a role puts its templates first, installing nothing', async ({ page }) => {
  await open(page, 'workflows');
  await page.getByLabel('Your role').selectOption({ label: 'Marketing' });
  const first = page.locator('article.sun-row').first();
  await expect(first).toContainText('Campaign');
  await expect(first).toContainText('Suggested');
  await expect(first.getByRole('button', { name: /^Use / })).toBeVisible();
  await page.reload();
  await expect(page.getByLabel('Your role')).toHaveValue('marketing');
});

test('the plan shows its limits once a license sets them', async ({ page }) => {
  await open(page, 'settings/billing');
  const plan = page.getByRole('region', { name: /Plan/ });
  await expect(plan).not.toContainText('Plan limits');
  await plan.getByLabel('License token').fill('sunlic1.sample');
  await plan.getByRole('button', { name: 'Install license' }).click();
  await expect(plan).toContainText('Plan limits: 10 seats · 2 hosts · 5 agents at once.');
});
