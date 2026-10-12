// MCP servers against the in-memory API: add a URL server with a new bearer secret and
// see its tools; turn it on for a task, pick one write tool, and find the grant kept on reopening.
import { expect, test, type Page } from '@playwright/test';

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

for (const [label, width, height] of [['desktop', 1280, 800], ['360 px', 360, 740]] as const) {
  test(`${label}: adds an MCP server, then grants one write tool to a task`, async ({ page }) => {
    await page.setViewportSize({ width, height });
    await page.goto('/?mock=1#/catalog');
    await page.getByRole('tab', { name: 'MCP servers' }).click();
    await page.getByRole('button', { name: 'Add MCP server' }).click();
    const dialog = page.getByRole('dialog');

    // Errors show under their field.
    await dialog.getByLabel('Name').fill('Docs');
    await dialog.getByLabel('URL').fill('ftp://docs.example.com');
    await dialog.getByRole('button', { name: 'Connect and add' }).click();
    await expect(dialog.getByText('Lowercase letters, digits, - or _, starting with a letter.')).toBeVisible();
    await expect(dialog.getByText('Use an http:// or https:// address.')).toBeVisible();
    await no_overflow(page);

    await dialog.getByLabel('Name').fill('docs');
    await dialog.getByLabel('URL').fill('https://docs.example.com/mcp');
    await dialog.getByRole('combobox', { name: 'Bearer token' }).selectOption('__add');
    await dialog.getByLabel('Secret name').fill('DOCS_TOKEN');
    await dialog.getByLabel('Value').fill('docs-s3cret');
    await dialog.getByRole('button', { name: 'Save secret' }).click();
    await expect(dialog.getByRole('combobox', { name: 'Bearer token' })).toHaveValue('DOCS_TOKEN');
    await dialog.getByRole('button', { name: 'Connect and add' }).click();

    const found = dialog.getByRole('list', { name: 'Tools of docs' });
    await expect(found.getByRole('listitem')).toHaveCount(3);
    await expect(found.getByRole('listitem').filter({ hasText: 'search' })).toContainText('read-only');
    await expect(found.getByRole('listitem').filter({ hasText: 'create_page' })).not.toContainText('read-only');
    await no_overflow(page);
    await dialog.getByRole('button', { name: 'Done' }).click();
    await expect(page.getByRole('article').filter({ hasText: 'docs.example.com' })).toBeVisible();

    // Per task: on = read-only tools; ticking a write tool makes the grant an explicit list.
    await page.goto('/?mock=1#/tasks/SUN-142');
    const docs = page.getByRole('switch', { name: 'docs' });
    await docs.locator('..').click(); // the DS hides the input itself
    await expect(page.getByText('Saved. The next agent session gets these tools.')).toBeVisible();
    await page.getByRole('button', { name: 'Tools of docs' }).click();
    const tools = page.getByRole('group', { name: 'Tools of docs' });
    await expect(tools.getByRole('checkbox', { name: /^search/ })).toBeChecked();
    await expect(tools.getByRole('checkbox', { name: /^create_page/ })).not.toBeChecked();
    await tools.getByRole('checkbox', { name: /^create_page/ }).locator('..').click();
    await expect(page.getByText('3 picked')).toBeVisible();
    await no_overflow(page);

    // Reopen the task: the grant came back from the API.
    await page.goto('/?mock=1#/');
    await page.goto('/?mock=1#/tasks/SUN-142');
    await expect(page.getByRole('switch', { name: 'docs' })).toBeChecked();
    await page.getByRole('button', { name: 'Tools of docs' }).click();
    await expect(page.getByRole('group', { name: 'Tools of docs' }).getByRole('checkbox', { name: /^create_page/ })).toBeChecked();
    await no_overflow(page);
  });
}
