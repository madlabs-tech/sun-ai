import { expect, test, type Page } from '@playwright/test';

// At 360 px, mock API: Plan & billing per workspace through Sun Cloud (status, seats, Upgrade to the hosted checkout,
// Manage billing, Buy seats after a price preview; members read only; Free with no limits), the checkout's return
// page, support tickets for anyone signed in (in My Account), and spend caps.
test.use({ viewport: { width: 360, height: 740 } });

const open = async (page: Page, route: string, query = '') => {
  await page.goto(`/?mock=1${query}#/${route}`);
  await page.waitForLoadState('networkidle');
};
const overflow = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - innerWidth);
const billing_of = (page: Page) => page.getByRole('region', { name: /^Billing/ });

test('owner: seats, Upgrade to Business yearly on the hosted checkout, back on the return page', async ({ page }) => {
  await open(page, 'settings/plan');
  await expect(page.getByRole('heading', { name: 'Plan & billing', exact: true })).toBeVisible();
  const billing = billing_of(page);
  await expect(billing.getByText('Active')).toBeVisible();
  const seats = billing.getByRole('definition');
  await expect(seats).toHaveText(['5', '3', '2']);

  await billing.getByRole('button', { name: 'Upgrade' }).click();
  const form = billing.getByRole('form', { name: 'Upgrade' });
  // On Pro, Business is what's left to upgrade to.
  await expect(form.getByLabel('Plan').locator('option')).toHaveText(['Business']);
  // Seats start at the people in the workspace.
  await expect(form.getByLabel('Seats')).toHaveValue('3');
  await form.getByLabel('Seats').fill('0');
  await form.getByRole('button', { name: 'Continue to checkout' }).click();
  await expect(form.getByText('Use a whole number from 1 to 10,000.')).toBeVisible();
  await form.getByLabel('Seats').fill('4');
  await expect(form).toContainText('Total: $76.00 a month');
  await form.getByLabel('Billed').selectOption('year');
  await expect(form).toContainText('Total: $760.00 a year');
  // The cloud prices in two currencies: the switch reprices the order.
  const currency = form.getByLabel('Currency');
  await expect(currency.locator('option')).toHaveText(['US dollar (USD)', 'Indonesian rupiah (IDR)']);
  await currency.selectOption('idr');
  await expect(form).toContainText(/Total: Rp\s?11\.960\.000 a year/);
  expect(await overflow(page)).toBeLessThanOrEqual(0);

  await form.getByRole('button', { name: 'Continue to checkout' }).click();
  await expect(page).toHaveURL(/#\/billing\/return\?status=success/);
  const result = page.getByRole('status').filter({ hasText: 'Payment received' });
  await expect(result).toContainText('the plan and seats update within 5 minutes');
  expect(await overflow(page)).toBeLessThanOrEqual(0);
  await page.getByRole('link', { name: 'Back to Plan & billing' }).click();
  await expect(page).toHaveURL(/#\/settings\/plan/);
  await expect(billing_of(page)).toBeVisible();
});

test('owner: buy seats after a price preview; Manage billing opens the portal', async ({ page }) => {
  await open(page, 'settings/plan');
  const billing = billing_of(page);
  await billing.getByRole('button', { name: 'Buy seats' }).click();
  const form = billing.getByRole('form', { name: 'Buy seats' });
  await form.getByLabel('Extra seats').fill('3');
  await form.getByRole('button', { name: 'Preview price' }).click();
  await expect(form.getByRole('status')).toContainText('3 more seats: $27.00 now');
  await form.getByRole('button', { name: 'Buy 3 seats' }).click();
  await expect(page.getByText('3 seats added')).toBeVisible();
  await expect(billing.getByRole('definition')).toHaveText(['8', '3', '5']);
  await expect(form).toBeHidden();

  await page.route('https://billing.example/**', (route) => route.fulfill({ contentType: 'text/html', body: '<h1>Portal</h1>' }));
  await billing.getByRole('button', { name: 'Manage billing' }).click();
  await expect(page).toHaveURL(/^https:\/\/billing\.example\/portal\//);
});

test('owner: cancel the plan after confirming; it stays on until the period ends', async ({ page }) => {
  await open(page, 'settings/plan');
  const billing = billing_of(page);
  await billing.getByRole('button', { name: 'Cancel plan' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Cancel the plan?' });
  await expect(dialog).toContainText('stays on until the end of the period already paid');
  await dialog.getByRole('button', { name: 'Keep the plan' }).click();
  await expect(dialog).toBeHidden();
  await expect(billing.getByText('Active')).toBeVisible();
  await billing.getByRole('button', { name: 'Cancel plan' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Cancel plan' }).click();
  await expect(page.getByText('It stays on until 2026-11-13.')).toBeVisible();
  await expect(billing.getByText('Canceled')).toBeVisible();
  await expect(billing.getByRole('button', { name: 'Cancel plan' })).toHaveCount(0);
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

test('member: reads the plan and seats, no actions', async ({ page }) => {
  await open(page, 'settings/plan', '&mock_role=member');
  const billing = billing_of(page);
  await expect(billing.getByRole('definition')).toHaveText(['5', '3', '2']);
  await expect(billing.getByText('Only the workspace’s owners and admins change its plan and seats.')).toBeVisible();
  await expect(billing.getByRole('button')).toHaveCount(0);
});

test('free: no limits, bring your own runtime; Upgrade only with prices', async ({ page }) => {
  await open(page, 'settings/plan', '&plan=free');
  const plan = page.getByRole('region', { name: /^Plan/ });
  await expect(plan).toContainText('Free plan, no limits. Bring your own runtime (a machine running sun host) and your own API keys.');
  const billing = billing_of(page);
  await expect(billing.getByText('No subscription')).toBeVisible();
  await expect(billing).toContainText('No seat limit on the free plan · 3 people in this workspace.');
  await expect(billing.getByRole('button', { name: 'Manage billing' })).toHaveCount(0);
  await billing.getByRole('button', { name: 'Upgrade' }).click();
  await expect(billing.getByRole('form', { name: 'Upgrade' }).getByLabel('Plan').locator('option')).toHaveText(['Pro', 'Business']);
  expect(await overflow(page)).toBeLessThanOrEqual(0);

  await open(page, 'settings/plan', '&plan=free&prices=none');
  await expect(billing_of(page).getByText('No subscription')).toBeVisible();
  await expect(billing_of(page).getByRole('button', { name: 'Upgrade' })).toHaveCount(0);
});

test('without Sun Cloud billing: the plan only; a cloud that fails says unavailable, never Free', async ({ page }) => {
  await open(page, 'settings/plan', '&features_off=billing');
  await expect(page.getByRole('heading', { name: 'Plan', exact: true })).toBeVisible();
  await expect(page.getByRole('region', { name: /^Plan/ })).toContainText('Pro');
  await expect(billing_of(page)).toHaveCount(0);

  await open(page, 'settings/plan', '&billing=down');
  await expect(billing_of(page).getByRole('alert')).toContainText('Billing is unavailable right now');
  await expect(billing_of(page)).not.toContainText('Free');
});

test('return page: a canceled checkout says nothing was charged', async ({ page }) => {
  await open(page, 'billing/return?status=canceled');
  await expect(page.getByRole('status')).toContainText('Checkout canceled');
  await expect(page.getByRole('status')).toContainText('Nothing was charged.');
  await expect(page.getByRole('link', { name: 'Back to Plan & billing' })).toHaveAttribute('href', /#\/settings\/plan/);
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

test('support: a question sent, earlier answers shown', async ({ page }) => {
  await open(page, 'settings/support');
  const support = page.getByRole('region', { name: /Support/ });
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

test('support: in My Account for a member; without a paid plan Sun Cloud says why', async ({ page }) => {
  await open(page, 'settings/support', '&mock_role=member&plan=free');
  await expect(page.getByRole('heading', { name: 'Support', exact: true })).toBeVisible();
  const support = page.getByRole('region', { name: /Support/ });
  await expect(support).toContainText('Support from people comes with a paid workspace plan');
  const form = support.getByRole('form', { name: 'Ask Sun' });
  await form.getByLabel('Subject').fill('Can I get help?');
  await form.getByLabel('Message').fill('We are on Free.');
  await form.getByRole('button', { name: 'Send to Sun' }).click();
  await expect(page.getByText('support from people comes with the Pro and Business plans')).toBeVisible();
  await expect(form.getByLabel('Subject')).toHaveValue('Can I get help?');
  expect(await overflow(page)).toBeLessThanOrEqual(0);
});

test('credits: not available on this deployment yet', async ({ page }) => {
  await open(page, 'runtimes');
  await page.getByRole('button', { name: 'Add credential' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('radio', { name: /Sun credits/ })).toBeDisabled();
  await expect(dialog).toContainText('Sun credits are not available on this deployment yet; bring your own key or runtime.');
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
