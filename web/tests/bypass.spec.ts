// BLO-164 against the in-memory API: Settings → Security → Bypass permissions (owner only), its disabled state
// while sun.yaml `runtimes.allow_bypass` is off, and the Agent dialog showing “Anything” as unavailable for a
// member the policy doesn't let.
import { expect, test, type Page } from '@playwright/test';

const settings = (page: Page, query = '') => page.goto(`/?mock=1${query}#/settings/bypass`);
const agents = (page: Page, query = '') => page.goto(`/?mock=1${query}#/agents`);
const row = (page: Page, name: string) => page.getByRole('article', { name, exact: true });

test.use({ viewport: { width: 1280, height: 800 } });

test('the owner picks who may bypass: owners only, picked members, or everyone', async ({ page }) => {
  await settings(page);
  const card = page.getByTestId('bypass-policy');
  await expect(card).toContainText('Owners always may');
  await expect(page.getByTestId('bypass-ceiling')).toHaveCount(0);
  await expect(card.getByRole('radio', { name: /Owners only/ })).toBeChecked();
  const save = card.getByRole('button', { name: 'Save' });
  await expect(save).toBeDisabled();

  await card.getByText('Owners and the members picked below').click();
  const people = card.getByRole('group', { name: 'Members who may' });
  await expect(people).toBeVisible();
  await people.locator('label').first().click();
  await expect(people.getByRole('checkbox').first()).toBeChecked();
  await save.click();
  await expect(page.getByText('Bypass permissions saved')).toBeVisible();
  await expect(save).toBeDisabled();

  await card.getByText('Every member').click();
  await expect(people).toBeHidden();
  await save.click();
  await expect(page.getByText('Bypass permissions saved').first()).toBeVisible();
});

test('with sun.yaml allow_bypass off the control is shown disabled with the reason', async ({ page }) => {
  await settings(page, '&no_bypass');
  await expect(page.getByTestId('bypass-ceiling')).toContainText(
    'Turn on runtimes.allow_bypass in sun.yaml first (container or VM sessions only).',
  );
  const card = page.getByTestId('bypass-policy');
  for (const radio of await card.getByRole('radio').all()) await expect(radio).toBeDisabled();
  await expect(card.getByRole('button', { name: 'Save' })).toHaveCount(0);
});

test('an admin the policy does not let sees “Anything” as unavailable, with the reason', async ({ page }) => {
  await agents(page, '&mock_role=admin&bypass_agent');
  await page.getByRole('button', { name: 'New agent' }).click();
  const create = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'New agent' }) });
  await expect(create.getByLabel('Permissions').locator('option', { hasText: 'Anything' })).toHaveCount(0);
  await expect(create).toContainText('“Anything” is not enabled for you; the owner can enable it in Settings → Security.');
  await create.getByRole('button', { name: 'Cancel' }).click();

  // An agent already in bypass keeps showing it, and says the admin's own runs of it go as Edit files.
  await row(page, 'Night Owl').getByRole('button', { name: 'Edit Night Owl' }).click();
  const edit = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Edit Night Owl' }) });
  await expect(edit.getByLabel('Permissions')).toHaveValue('bypass_permissions');
  await expect(edit).toContainText('your runs of this agent go as Edit files');
});

test('with the policy at everyone the admin may pick “Anything”', async ({ page }) => {
  await agents(page, '&mock_role=admin&bypass=everyone');
  await page.getByRole('button', { name: 'New agent' }).click();
  const create = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'New agent' }) });
  await create.getByLabel('Permissions').selectOption({ label: 'Anything' });
  await expect(create).toContainText('Runs any command without asking.');
});
