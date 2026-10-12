import { expect, test } from '@playwright/test';

// Work for every role on the mock API, at 360 px.
test.use({ viewport: { width: 360, height: 740 } });

const TECHNICAL = /branch|worktree|\brepos?\b|Codex|Claude Code|runtime/i;
const fits = async (page: import('@playwright/test').Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);

test('onboarding asks the role and the limit policy once; the marketer view shows no branch text', async ({ page }) => {
  // `fresh`: a new install, before anyone picked a limit policy.
  await page.goto('/?mock=1&fresh=1#/');
  const setup = page.getByRole('form', { name: 'Set up Sun' });
  await expect(setup).toBeVisible();
  await fits(page);
  await setup.getByLabel('Your role').selectOption('marketing');
  await setup.locator('label', { hasText: 'Pause and tell me' }).click(); // the DS radio hides its input behind the label
  await expect(setup.getByRole('radio', { name: /Pause and tell me/ })).toBeChecked();
  await page.getByRole('dialog').getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('You’re set up')).toBeVisible();
  await expect(setup).toBeHidden();

  // A repo-less card keeps runtimes and "No repo" out; code tasks still show their repos.
  const card = page.getByRole('link', { name: /SUN-147/ });
  await expect(card).toBeVisible();
  await expect(card).not.toContainText(TECHNICAL);
  await expect(card).not.toContainText('No repo');
  await expect(page.getByRole('link', { name: /SUN-142/ })).toContainText('2 repos');

  await card.click();
  const drawer = page.getByRole('main');
  await expect(drawer.getByRole('heading', { name: 'October campaign: channel drafts' })).toBeVisible();
  await expect(drawer.getByText('Draft saved · x-thread.md v1')).toBeVisible();
  await expect(drawer).not.toContainText(TECHNICAL);
  await expect(drawer.getByRole('button', { name: 'Switch runtime' })).toHaveCount(0);
  await expect(drawer.getByRole('link', { name: /Download x-thread\.md/ })).toBeVisible();
  await fits(page);

  // Set-up is asked once per browser, even while the policy is still unset.
  await page.goto('/?mock=1&fresh=1#/');
  await expect(page.getByRole('heading', { name: 'Tasks' })).toBeVisible();
  await expect(page.getByRole('form', { name: 'Set up Sun' })).toHaveCount(0);
});

test('engineers see repos and runtimes, and set a task’s own limit policy', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-147');
  const drawer = page.getByRole('main');
  await expect(drawer.getByText('No linked pull requests.')).toBeVisible();
  await expect(drawer.getByText('Dispatched to Claude Code · task folder')).toBeVisible();
  const policy = drawer.getByLabel('On limit');
  await expect(policy).toHaveValue('');
  await policy.selectOption('wait_until_reset');
  await expect(page.getByText('Limit policy saved')).toBeVisible();
  await expect(policy).toHaveValue('wait_until_reset');
  await fits(page);
});

test('the role view follows the member to a new browser', async ({ page }) => {
  // This browser never picked a role; the kernel says the member chose marketing elsewhere.
  await page.goto('/?mock=1&member_view=marketing#/tasks/SUN-147');
  const drawer = page.getByRole('main');
  await expect(drawer.getByRole('heading', { name: 'October campaign: channel drafts' })).toBeVisible();
  await expect(drawer).not.toContainText(TECHNICAL);
  expect(await page.evaluate(() => localStorage.getItem('sun_role'))).toBe('marketing');
  await fits(page);
});

test('settings: the ecosystem limit policy saves', async ({ page }) => {
  await page.goto('/?mock=1#/settings/usage');
  const form = page.getByTestId('limit-policy');
  await expect(form.getByRole('radio', { name: /Switch to the next runtime/ })).toBeChecked();
  const save = form.getByRole('button', { name: 'Save' });
  await expect(save).toBeDisabled();
  await form.locator('label', { hasText: 'Wait for the reset' }).click();
  await save.click();
  await expect(page.getByText('Limit policy saved')).toBeVisible();
  await expect(save).toBeDisabled();
});
