import { expect, test, type Page } from '@playwright/test';

// At 360 px, mock API: plans with checkout (IDR QRIS / transfer / card, USD card), credit top-ups,
// support from people with the plan's answer target, and a points estimate before a run.
test.use({ viewport: { width: 360, height: 740 } });

const open = async (page: Page, route: string) => {
  await page.goto(`/?mock=1#/${route}`);
  await page.waitForLoadState('networkidle');
};
const overflow = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - innerWidth);

test('plans: upgrade to Business with seats, pay by QRIS on the hosted checkout; USD takes card only', async ({ page }) => {
  await open(page, 'settings/billing');
  const billing = page.getByRole('region', { name: /Subscription and credits/ });
  await expect(billing.getByText('2,450,000 points').first()).toBeVisible();
  const plans = billing.getByRole('list', { name: 'Plans' });
  await expect(plans.getByText('Current plan')).toBeVisible();
  await expect(plans).toContainText('Premium models: Claude and GPT');
  await expect(plans).toContainText('Support from people within 24 hours');

  await billing.getByRole('button', { name: 'Upgrade to Business' }).click();
  const checkout = billing.getByRole('form', { name: 'Checkout' });
  await checkout.getByLabel('Seats').fill('0');
  await checkout.getByRole('button', { name: 'Continue to payment' }).click();
  await expect(checkout.getByText('Use a whole number from 1 to 10,000.')).toBeVisible();
  await checkout.getByLabel('Seats').fill('3');
  await expect(checkout).toContainText(/Total: Rp\s?1\.437\.000 for the first 3 months/);
  const pay_with = checkout.getByLabel('Pay with');
  await expect(pay_with.locator('option')).toHaveText(['QRIS', 'Bank transfer (virtual account)', 'Card']);

  await checkout.getByLabel('Currency').selectOption('usd');
  await expect(pay_with.locator('option')).toHaveText(['Card']);
  await checkout.getByLabel('Currency').selectOption('idr');
  await pay_with.selectOption('qris');
  await checkout.getByRole('button', { name: 'Continue to payment' }).click();

  const ready = billing.getByRole('status').filter({ hasText: 'is ready' });
  await expect(ready).toContainText('Pay by QRIS on the checkout page. The plan switches on once the payment clears.');
  const link = ready.getByRole('link', { name: 'Open checkout' });
  await expect(link).toHaveAttribute('href', /^https:\/\/checkout\.example\/inv_/);
  await expect(link).toHaveAttribute('target', '_blank');
  await expect(billing.getByRole('list', { name: 'Invoices' }).getByText('Waiting for payment')).toBeVisible();
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

test('credits: buy a points pack', async ({ page }) => {
  await open(page, 'settings/billing');
  const buy = page.getByRole('form', { name: 'Buy Sun credits' });
  await buy.getByLabel('Pack').selectOption('500000:usd');
  await expect(buy.getByLabel('Pay with').locator('option')).toHaveText(['Card']);
  await buy.getByRole('button', { name: 'Buy points' }).click();
  await expect(page.getByRole('status').filter({ hasText: 'is ready' })).toContainText('The points arrive once the payment clears.');
});

test('support: the plan’s answer target, a question sent, earlier answers shown', async ({ page }) => {
  await open(page, 'settings/support');
  const support = page.getByRole('region', { name: /Support/ });
  await expect(support.getByText('Answer within 48 h')).toBeVisible();
  const asked = support.getByRole('list', { name: 'Your questions' });
  await expect(asked.getByText('Answered')).toBeVisible();
  await expect(asked).toContainText('We raised the upstream timeout');

  const form = support.getByRole('form', { name: 'Ask Sun' });
  await form.getByRole('button', { name: 'Send to Sun' }).click();
  await expect(form.getByText('Say in a few words what it is about.')).toBeVisible();
  await expect(form.getByText('Describe what happened and what you expected.')).toBeVisible();
  await form.getByLabel('Subject').fill('Approvals never reach Slack');
  await form.getByLabel('Message').fill('SUN-142 waits for approval but no Slack message arrives.');
  await form.getByRole('button', { name: 'Send to Sun' }).click();
  const fresh = asked.getByRole('listitem').filter({ hasText: 'Approvals never reach Slack' }).first();
  await expect(fresh.getByText('Waiting for Sun')).toBeVisible();
  await expect(fresh.getByText(/Answer due by \d{4}-\d{2}-\d{2}/)).toBeVisible();
  await expect(form.getByLabel('Subject')).toHaveValue('');
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

test('dry run: the cost estimate in Sun credit points before a run', async ({ page }) => {
  await open(page, 'workflows/new');
  await page.getByRole('tab', { name: 'YAML' }).click();
  await page.getByLabel('Workflow file').fill('name: ship\nsteps:\n  - id: work\n    type: agent\n    prompt: Fix it.\n');
  await page.getByRole('button', { name: 'Dry run' }).click();
  const dry = page.getByRole('region', { name: 'Dry run' });
  await expect(dry).toContainText('About $0.62 for 1 agent step');
  await expect(dry).toContainText('On Sun credits: up to 744 points');
});

// Credits runtimes from the UI when Sun Cloud is connected, and spend caps.
test('credits runtime can be added with Sun Cloud', async ({ page }) => {
  await open(page, 'runtimes');
  await page.getByRole('button', { name: 'Add runtime' }).click();
  const dialog = page.getByRole('dialog');
  const credits = dialog.getByRole('radio', { name: /Sun credits/ });
  await expect(credits).toBeEnabled();
  await dialog.locator('label', { hasText: 'Sun credits' }).click();
  await expect(credits).toBeChecked();
  await dialog.getByLabel('Name', { exact: true }).fill('qwen-credits');
  await dialog.getByLabel('Runs on').selectOption('opencode');
  await dialog.getByRole('button', { name: 'Add runtime' }).click();
  await expect(dialog.getByText('OpenCode on Sun credits needs a model from the price table')).toBeVisible();
  await dialog.getByLabel('Model', { exact: true }).fill('qwen3-coder');
  await dialog.getByRole('button', { name: 'Add runtime' }).click();
  await expect(dialog).toBeHidden();
  const row = page.getByRole('article', { name: 'qwen-credits' });
  await expect(row).toContainText('Sun credits');
  await expect(row).toContainText('qwen3-coder through the Sun gateway');
  await row.getByRole('button', { name: 'Check' }).click();
  await expect(row.getByRole('status')).toContainText('Sun Cloud is connected');
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

test('self-hosted: the credits source is off and says why', async ({ page }) => {
  await page.goto('/?mock=1&self_hosted=1#/runtimes');
  await page.waitForLoadState('networkidle');
  await page.getByRole('button', { name: 'Add runtime' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('radio', { name: /Sun credits/ })).toBeDisabled();
  await expect(dialog).toContainText('Needs Sun Cloud. A self-hosted install uses your subscription or API key.');
});

test('spend cap: the month’s spend against the cap, saved from Settings', async ({ page }) => {
  await open(page, 'settings/usage');
  const cap = page.getByRole('form', { name: /Monthly spend cap/ });
  await expect(cap.getByRole('status')).toContainText('$12.40 of $50.00 spent this month');
  await cap.getByLabel('Cap (USD per month)').fill('-3');
  await cap.getByRole('button', { name: 'Save' }).click();
  await expect(cap.getByText('Use an amount in dollars, 0 or more.')).toBeVisible();
  await cap.getByLabel('Cap (USD per month)').fill('10');
  await cap.getByRole('button', { name: 'Save' }).click();
  await expect(cap.getByText('Cap reached')).toBeVisible();
  await cap.getByRole('button', { name: 'Remove cap' }).click();
  await expect(cap.getByRole('status')).toContainText('spent this month, no cap');
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

test('task budget: saved from the task panel, the smaller budget shown', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-142');
  await page.waitForLoadState('networkidle');
  const drawer = page.getByRole('main');
  const budget = drawer.getByRole('spinbutton', { name: 'Cost budget' });
  await budget.fill('0.5');
  await drawer.locator('form.budget').getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Budget saved')).toBeVisible();
  await expect(drawer).toContainText('/ $0.50 budget');
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});
