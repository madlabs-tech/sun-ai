// Step-list workflow builder and role templates against the real kernel:
// its sun-workflow validates live with line:col, and Save makes a new version.
import { expect, test, type Page } from '@playwright/test';

async function sign_in(page: Page) {
  expect((await page.request.post('/api/v1/session', { data: { email: 'me@example.com', password: 'test-token' } })).ok()).toBe(true);
}

const steps = (page: Page) => page.locator('ol.blocks > li');
const yaml = (page: Page) => page.getByLabel('Workflow file');

test('template → reorder, add, validate, remove, save a version, edit as YAML', async ({ page }) => {
  const broken: string[] = [];
  page.on('response', (r) => r.status() >= 400 && !r.url().includes('/api/v1/') && broken.push(`${r.status()} ${r.url()}`));
  await sign_in(page);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto('/#/workflows');
  await expect(page.getByRole('heading', { name: 'Workflows', level: 1 })).toBeVisible();

  // Role templates are listed with the mods they need; using one opens it in the builder.
  const templates = page.getByRole('region', { name: 'Start from a template' }).getByRole('article');
  await expect(templates).toHaveCount(7);
  const bug_fix = templates.filter({ hasText: 'Bug fix' });
  await expect(bug_fix).toContainText('Uses github');
  // A kernel reused across runs already has it: open it instead.
  await bug_fix.getByRole('button', { name: /Use Bug fix/ }).or(bug_fix.getByRole('link', { name: 'Open' })).click();
  await expect(page.getByRole('heading', { name: 'bug-fix', level: 1 })).toBeVisible();
  await expect(page.getByRole('status').getByText('Valid')).toBeVisible();
  const version = Number((await page.locator('.sun-header p').textContent())?.match(/Version (\d+)/)?.[1]);
  expect(version).toBeGreaterThan(0);
  const save = page.getByRole('button', { name: 'Save' });
  await expect(save).toBeDisabled();

  // One block per step, in file order.
  await expect(steps(page)).toHaveCount(5);
  await expect(steps(page).first()).toContainText('reproduce');

  // Reorder with buttons (no drag needed): fix moves above reproduce.
  await page.getByRole('button', { name: 'Move fix up' }).click();
  await expect(steps(page).first()).toContainText('fix');

  // Add an agent step: it has no instructions, so the kernel reports it at its line.
  await page.getByRole('button', { name: /^Agent/ }).click();
  await expect(steps(page)).toHaveCount(6);
  const added = steps(page).last();
  await expect(added.getByText(/Line \d+:\d+ · .*prompt/)).toBeVisible();
  await expect(page.getByRole('status').getByText('1 problem')).toBeVisible();
  await expect(save).toBeDisabled();
  await added.getByLabel('Instructions').fill('Write a short changelog entry for the fix.');
  await expect(page.getByRole('status').getByText('Valid')).toBeVisible();

  // Gates and approvals are configurable blocks.
  await steps(page).filter({ hasText: 'Approval' }).getByRole('button', { name: /^review/ }).click();
  const review = steps(page).filter({ hasText: 'Approval' });
  await review.getByLabel('Time limit').fill('48h');
  await review.getByLabel('When time runs out').selectOption('auto_reject');

  // Remove a step.
  const pr = steps(page).filter({ hasText: 'Plugin action' });
  await pr.getByRole('button', { name: /^pr/ }).click();
  await pr.getByRole('button', { name: 'Remove step' }).click();
  await expect(steps(page)).toHaveCount(5);
  await expect(page.getByRole('status').getByText('Valid')).toBeVisible();

  // The YAML view is the same workflow.
  await page.getByRole('tab', { name: 'YAML' }).click();
  const file = await yaml(page).inputValue();
  expect(file.indexOf('id: fix')).toBeLessThan(file.indexOf('id: reproduce'));
  expect(file).toContain('Write a short changelog entry');
  expect(file).toContain('then: auto_reject');
  expect(file).not.toContain('github.open_pr');

  // Save creates the next version.
  await save.click();
  await expect(page.getByText(`Saved as version ${version + 1}`)).toBeVisible();
  await page.reload();
  await expect(page.locator('.sun-header p')).toHaveText(`Version ${version + 1}`);

  // Editing the file shows in the blocks, and a broken file gets line:col errors.
  await page.getByRole('tab', { name: 'YAML' }).click();
  await yaml(page).fill((await yaml(page).inputValue()).replace('id: verify', 'id: checks'));
  await page.getByRole('tab', { name: 'Steps' }).click();
  await expect(page.locator('ol.blocks')).toContainText('checks');
  await page.getByRole('tab', { name: 'YAML' }).click();
  await yaml(page).fill((await yaml(page).inputValue()).replace('name: bug-fix', 'name: bug-fix\nbogus: 1'));
  // Line 4: the template's two comment lines survived the block edits above.
  await expect(page.getByRole('region', { name: /Fix this before saving/ })).toContainText(/Line 4:\d+.*bogus/);
  await expect(save).toBeDisabled();
  expect(broken, 'self-hosted icons (copy new ones into public/icons)').toEqual([]);
});

test('new workflow on a 360 px phone: no sideways scroll, add blocks, save version 1', async ({ page }) => {
  await sign_in(page);
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/#/workflows/new');
  await expect(page.getByRole('heading', { name: 'new-workflow', level: 1 })).toBeVisible();
  await expect(page.getByRole('status').getByText('Valid')).toBeVisible();

  const name = `phone-${Date.now()}`;
  await page.getByText('Workflow settings').click();
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByRole('button', { name: /^Approval/ }).click();
  await page.getByRole('button', { name: /^Notify/ }).click();
  await expect(steps(page).last().getByText(/Line \d+:\d+ · .*`to`/)).toBeVisible();
  await page.getByRole('textbox', { name: 'Add to Send to' }).fill('slack:#team');
  await page.getByRole('button', { name: 'Add to Send to' }).click();
  await expect(page.getByRole('status').getByText('Valid')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(360);

  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Saved as version 1')).toBeVisible();
  await expect(page).toHaveURL(/#\/workflows\/[0-9A-Z]{26}(\?workspace=\w+)?$/);
  await page.goto('/#/workflows');
  await expect(page.getByRole('link', { name: new RegExp(name) })).toBeVisible();
});
