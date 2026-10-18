// BYO key credentials against the in-memory API: add an Ollama endpoint without a key,
// see it listed and ready, and get the kernel's "no endpoint" error under the Endpoint field.
// The key's secret name is write-only: it never comes back, only whether one is set. Delete confirms first.
import { expect, test, type Page } from '@playwright/test';

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

/** The `.ml-field` wrapping the input labelled `label` (its hint/error is inside). */
const field = (page: Page, label: string) => page.getByRole('dialog').locator('.ml-field', { has: page.getByLabel(label, { exact: true }) });

async function add_ollama(page: Page, name: string) {
  await page.goto('/?mock=1#/runtimes');
  await page.getByRole('button', { name: 'Add credential' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('heading', { name: 'Add a credential' })).toBeVisible();

  // This deployment sells no Sun credits yet, so they can't be picked; BYO key is the default.
  await expect(dialog.getByRole('radio', { name: /Sun credits/ })).toBeDisabled();
  await expect(dialog.getByText('Sun credits are not available on this deployment yet; bring your own key or runtime.')).toBeVisible();
  await expect(dialog.getByRole('radio', { name: /BYO key/ })).toBeChecked();
  await expect(field(page, 'Endpoint')).toContainText('OpenAI-compatible base URL, e.g. http://localhost:11434/v1 for Ollama');

  await dialog.getByLabel('Name', { exact: true }).fill(name);
  await dialog.getByLabel('Endpoint', { exact: true }).fill('http://localhost:11434/v1');
  await dialog.getByLabel('Model', { exact: true }).fill('qwen2.5-coder:7b');
  await expect(dialog.getByLabel('API key secret')).toHaveValue('');
  await expect(dialog.getByLabel('API key secret').locator('option:checked')).toHaveText('None (local endpoint)');
  return dialog;
}

test('byo key: an Ollama credential without a key is listed and ready; no endpoint shows under the field', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  const dialog = await add_ollama(page, 'ollama');

  // Clearing the endpoint: the kernel's 422 lands under Endpoint, nowhere else.
  await dialog.getByLabel('Endpoint', { exact: true }).fill('');
  await dialog.getByRole('button', { name: 'Add credential' }).click();
  await expect(field(page, 'Endpoint')).toContainText(/no endpoint/i);
  await expect(dialog.getByLabel('Endpoint', { exact: true })).toHaveAttribute('aria-invalid', 'true');
  await expect(dialog.getByLabel('Model', { exact: true })).not.toHaveAttribute('aria-invalid', 'true');

  await dialog.getByLabel('Endpoint', { exact: true }).fill('http://localhost:11434/v1');
  await dialog.getByRole('button', { name: 'Add credential' }).click();
  await expect(dialog).toBeHidden();

  const row = page.getByRole('article', { name: 'ollama' });
  await expect(row).toContainText('Ready');
  await expect(row).toContainText('qwen2.5-coder:7b');
  await expect(row).toContainText('no key');
  await row.getByRole('button', { name: 'Check' }).click();
  await expect(page.getByText('ollama: ready')).toBeVisible();
  await expect(page.getByText('OpenCode can run on it.')).toBeVisible();
});

test('byo key: a keyed endpoint names its secret; a taken name errors under Name', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  const dialog = await add_ollama(page, 'openrouter');
  await dialog.getByLabel('Endpoint', { exact: true }).fill('https://openrouter.ai/api/v1');
  // The key goes in write-only through the picker's "Add secret…".
  await dialog.getByLabel('API key secret').selectOption('__add');
  await dialog.getByLabel('Secret name').fill('OPENROUTER_KEY');
  await dialog.getByLabel('Value').fill('sk-or-sample-key');
  await dialog.getByRole('button', { name: 'Save secret' }).click();
  await expect(dialog.getByLabel('API key secret')).toHaveValue('OPENROUTER_KEY');
  await dialog.getByRole('button', { name: 'Add credential' }).click();
  await expect(dialog).toBeHidden();
  const row = page.getByRole('article', { name: 'openrouter' });
  await expect(row).toContainText('key set');
  await expect(row).toContainText('Ready');
  // Write-only: the secret's name never comes back from GET /credentials, so the page can't show it.
  await expect(page.getByRole('main')).not.toContainText('OPENROUTER_KEY');

  await page.getByRole('button', { name: 'Add credential' }).click();
  await page.getByRole('dialog').getByLabel('Name', { exact: true }).fill('openrouter');
  await page.getByRole('dialog').getByLabel('Endpoint', { exact: true }).fill('https://openrouter.ai/api/v1');
  await page.getByRole('dialog').getByLabel('Model', { exact: true }).fill('m');
  await page.getByRole('dialog').getByRole('button', { name: 'Add credential' }).click();
  await expect(field(page, 'Name')).toContainText('already exists');
});

test('360 px: add a BYO key credential with no sideways scroll', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  const dialog = await add_ollama(page, 'ollama-phone');
  await no_overflow(page);
  await dialog.getByLabel('Endpoint', { exact: true }).fill('');
  await dialog.getByRole('button', { name: 'Add credential' }).click();
  await expect(field(page, 'Endpoint')).toContainText(/no endpoint/i);
  await no_overflow(page);

  await dialog.getByLabel('Endpoint', { exact: true }).fill('http://localhost:11434/v1');
  await dialog.getByRole('button', { name: 'Add credential' }).click();
  const row = page.getByRole('article', { name: 'ollama-phone' });
  await expect(row).toContainText('Ready');
  await row.getByRole('button', { name: 'Check' }).click();
  await expect(page.getByText('ollama-phone: ready')).toBeVisible();
  const check = row.getByRole('button', { name: 'Check' });
  expect((await check.boundingBox())?.height ?? 0).toBeGreaterThanOrEqual(32);
  await no_overflow(page);
});

test('byo key: an Anthropic key runs on Claude Code without an endpoint (360 px)', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  const dialog = await add_ollama(page, 'claude-key');
  const runs_on = dialog.getByLabel('Runs on');
  // OpenAI-compatible keys run on OpenCode or Codex.
  await expect(runs_on.locator('option')).toHaveText(['OpenCode', 'Codex']);

  await dialog.getByLabel('Provider').selectOption('anthropic');
  await expect(runs_on.locator('option')).toHaveText(['OpenCode', 'Claude Code']);
  await runs_on.selectOption('claude-code');
  await expect(field(page, 'Runs on')).toContainText('Claude Code uses the key as its own login.');
  await expect(field(page, 'Endpoint')).toContainText('Leave empty for api.anthropic.com');
  await dialog.getByLabel('Endpoint', { exact: true }).fill('');
  await dialog.getByLabel('Model', { exact: true }).fill('claude-sonnet-4-5');
  await dialog.getByLabel('API key secret').selectOption('__add');
  await dialog.getByLabel('Secret name').fill('ANTHROPIC_KEY');
  await dialog.getByLabel('Value').fill('sk-ant-sample-key');
  await dialog.getByRole('button', { name: 'Save secret' }).click();
  await no_overflow(page);

  // Google runs only on OpenCode: switching moves the CLI back.
  await dialog.getByLabel('Provider').selectOption('google');
  await expect(runs_on).toHaveValue('opencode');
  await dialog.getByLabel('Provider').selectOption('anthropic');
  await runs_on.selectOption('claude-code');

  await dialog.getByRole('button', { name: 'Add credential' }).click();
  await expect(dialog).toBeHidden();
  const row = page.getByRole('article', { name: 'claude-key' });
  await expect(row).toContainText('claude-code');
  await expect(row).toContainText('claude-sonnet-4-5 at Anthropic');
  await expect(row).toContainText('key set');
  await expect(row).not.toContainText('ANTHROPIC_KEY');
  await row.getByRole('button', { name: 'Check' }).click();
  await expect(page.getByText('Claude Code can run on it.')).toBeVisible();
  await no_overflow(page);
});

test('credentials: the sample ones are listed apart from the host runtimes; delete confirms, refused while an agent names it', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/agents');
  // An agent whose chain names lan-qwen.
  await page.getByRole('button', { name: 'New agent' }).click();
  const form = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'New agent' }) });
  await form.getByLabel('Name', { exact: true }).fill('Local');
  const chain = form.getByRole('combobox', { name: 'Add to Runtimes, in failover order' });
  await chain.fill('lan-qwen');
  await chain.press('Enter');
  await form.getByRole('button', { name: 'Create agent' }).click();
  await expect(form).toBeHidden();

  await page.evaluate(() => (location.hash = '#/runtimes'));
  const credentials = page.getByRole('region', { name: 'Credentials' });
  await expect(credentials.getByRole('article')).toHaveCount(2);
  const lan = credentials.getByRole('article', { name: 'lan-qwen' });
  await expect(lan).toContainText('Your API key · qwen2.5-coder:7b at http://localhost:11434/v1 · no key');
  await expect(credentials.getByRole('article', { name: 'credits' })).toContainText('Sun credits · qwen3-coder through the Sun gateway');

  // In use: the kernel's 409 names the agent, and it stays.
  await lan.getByRole('button', { name: 'Delete lan-qwen' }).click();
  const confirm = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Delete lan-qwen?' }) });
  await confirm.getByRole('button', { name: 'Delete' }).click();
  await expect(page.getByText('lan-qwen not deleted')).toBeVisible();
  await expect(page.getByText(/is in the chain of Local/)).toBeVisible();
  await expect(lan).toBeVisible();

  // Unused: Cancel keeps it, Delete removes it.
  const credits = credentials.getByRole('article', { name: 'credits' });
  await credits.getByRole('button', { name: 'Delete credits' }).click();
  const confirm_credits = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Delete credits?' }) });
  await confirm_credits.getByRole('button', { name: 'Cancel' }).click();
  await expect(confirm_credits).toBeHidden();
  await expect(credits).toBeVisible();
  await credits.getByRole('button', { name: 'Delete credits' }).click();
  await confirm_credits.getByRole('button', { name: 'Delete' }).click();
  await expect(page.getByText('credits deleted')).toBeVisible();
  await expect(credentials.getByRole('article')).toHaveCount(1);
});

test('credentials: a viewer sees them but can’t add or delete', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1&mock_role=viewer#/runtimes');
  const credentials = page.getByRole('region', { name: 'Credentials' });
  await expect(credentials.getByRole('article', { name: 'lan-qwen' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Add credential' })).toHaveCount(0);
  await expect(credentials.getByRole('button', { name: /^Delete / })).toHaveCount(0);
});
