import { expect, test, type Page } from '@playwright/test';

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

// Stale and conflicting sources on the task page, mock API.
test('task page flags stale and conflicting sources with citations', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-139');
  const drawer = page.getByRole('main');
  const flags = drawer.getByRole('region', { name: /Sources to check/ });
  await expect(flags).toBeVisible();
  await expect(flags.getByRole('heading')).toContainText('2');

  const items = flags.locator('.flag');
  await expect(items).toHaveCount(2);
  await expect(items.nth(0).locator('.ml-badge--warning')).toHaveText('Stale');
  await expect(items.nth(0)).toContainText('238 days ago (the limit is 180 days)');
  await expect(items.nth(1).locator('.ml-badge--danger')).toHaveText('Conflict');
  await expect(items.nth(1)).toContainText('Pricing PRD says 99.000; Sales deck Q4 says 120.000');

  // Citations: title > heading · version · date, linked to the source in a new tab.
  const deck = items.nth(1).getByRole('link', { name: 'Sales deck Q4 > Plans > Pro · rev 31 · 2026-09-30' });
  await expect(deck).toHaveAttribute('href', 'https://docs.google.com/document/d/1q2w3e');
  await expect(deck).toHaveAttribute('target', '_blank');
  await expect(deck).toHaveAttribute('rel', 'noopener noreferrer');

  // A task without flagged sources shows no section.
  await page.goto('/?mock=1#/tasks/SUN-142');
  await expect(drawer.getByRole('heading', { name: 'Fix login timeout on mobile' })).toBeVisible();
  await expect(drawer.getByRole('region', { name: /Sources to check/ })).toHaveCount(0);
});

test('flagged sources fit a phone without horizontal scroll', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/tasks/SUN-139');
  await expect(page.getByRole('region', { name: /Sources to check/ }).locator('.flag')).toHaveCount(2);
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
});

// Knowledge screen against the in-memory API: sources, docs, chunks, search, embeddings, task context.
const source = (page: Page, name: string) => page.getByRole('article', { name, exact: true });

test('knowledge: sources show totals, a failed sync and the files not indexed', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/knowledge');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Knowledge');

  await expect(source(page, 'Runbooks')).toContainText('Uploads · 2 docs · 3 chunks · indexed 2026-10-07');
  const wiki = source(page, 'Engineering wiki');
  await expect(wiki).toContainText('Confluence · space key ENG');
  await expect(wiki.locator('.ml-badge--warning')).toHaveText('2 files not indexed');
  await expect(wiki.getByRole('list', { name: 'Not indexed from Engineering wiki' })).toContainText('architecture.drawio: not text');
  const tickets = source(page, 'Tickets');
  await expect(tickets.locator('.ml-badge--danger')).toHaveText('Sync failed');
  await expect(tickets).toContainText('the token expired');

  // Re-index reports what it did; a failing connector says why.
  await wiki.getByRole('button', { name: 'Re-index' }).click();
  await expect(page.getByText('Engineering wiki re-indexed')).toBeVisible();
  await expect(page.getByText('0 indexed · 2 unchanged · 0 removed')).toBeVisible();
  await tickets.getByRole('button', { name: 'Re-index' }).click();
  await expect(page.getByText('Tickets not indexed')).toBeVisible();

  // Upload: markdown files become docs titled by their first heading.
  const chooser = page.waitForEvent('filechooser');
  await source(page, 'Runbooks').getByRole('button', { name: 'Upload' }).click();
  await (await chooser).setFiles({ name: 'release.md', mimeType: 'text/markdown', buffer: Buffer.from('# Release notes\n\nShip it on Tuesdays.') });
  await expect(page.getByText('1 file uploaded to Runbooks')).toBeVisible();
  await expect(source(page, 'Runbooks')).toContainText('3 docs');
});

test('knowledge: add a Confluence source; the space key is required', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/knowledge');
  await page.getByRole('button', { name: 'Add source' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('combobox', { name: 'Kind' }).selectOption('confluence');
  await dialog.getByRole('textbox', { name: 'Name' }).fill('Product wiki');
  await dialog.getByRole('button', { name: 'Add source' }).click();
  await expect(dialog.getByText('Enter the space key.')).toBeVisible();

  await dialog.getByRole('textbox', { name: 'Space key' }).fill('PROD');
  await dialog.getByRole('spinbutton', { name: 'Flag docs older than (days)' }).fill('90');
  await dialog.getByRole('button', { name: 'Add source' }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText('Product wiki added')).toBeVisible();
  await expect(source(page, 'Product wiki')).toContainText('Confluence · space key PROD · 0 docs · 0 chunks');
});

test('knowledge: Excel and PDF uploads go up as files and become docs', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/knowledge');
  const chooser = page.waitForEvent('filechooser');
  await source(page, 'Runbooks').getByRole('button', { name: 'Upload' }).click();
  const files = await chooser;
  expect(await files.element().getAttribute('accept')).toContain('.xlsx,.docx,.pdf');
  await files.setFiles([
    { name: 'pricing.xlsx', mimeType: 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet', buffer: Buffer.from('PK\x03\x04 sheet') },
    { name: 'contract.pdf', mimeType: 'application/pdf', buffer: Buffer.from('%PDF-1.4 contract') },
  ]);
  await expect(page.getByText('2 files uploaded to Runbooks')).toBeVisible();
  await expect(page.getByText('2 indexed · 0 unchanged · 0 removed')).toBeVisible();
  await expect(source(page, 'Runbooks')).toContainText('4 docs');

  // An xlsx becomes a doc per sheet tab.
  await page.getByRole('link', { name: 'Runbooks' }).click();
  await expect(page.getByRole('article', { name: 'pricing / Sheet1' })).toBeVisible();
  await expect(page.getByRole('article', { name: 'contract' })).toBeVisible();
});

test('knowledge: add a repo folder; connectors take an export webhook', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/knowledge');
  await page.getByRole('button', { name: 'Add source' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('combobox', { name: 'Kind' }).selectOption('jira');
  const webhook = dialog.getByRole('textbox', { name: 'Export webhook (optional)' });
  await webhook.fill('http://example.com/hook');
  await dialog.getByRole('textbox', { name: 'Name' }).fill('Jira PAY');
  await dialog.getByRole('textbox', { name: 'Project key' }).fill('PAY');
  await dialog.getByRole('button', { name: 'Add source' }).click();
  await expect(dialog.getByText('Use an https URL, or http to localhost.')).toBeVisible();

  await dialog.getByRole('combobox', { name: 'Kind' }).selectOption('repo');
  await expect(webhook).toHaveCount(0);
  await dialog.getByRole('textbox', { name: 'Name' }).fill('Handbook');
  await dialog.getByRole('textbox', { name: 'Folder path' }).fill('/srv/checkouts/handbook');
  await dialog.getByRole('button', { name: 'Add source' }).click();
  await expect(dialog).toBeHidden();
  await expect(source(page, 'Handbook')).toContainText('Repo folder · folder path /srv/checkouts/handbook');
  await expect(source(page, 'Handbook').getByRole('button', { name: 'Re-index' })).toBeVisible();
});

test('knowledge: a source opens its docs, a doc its chunks with citations', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/knowledge');
  await page.getByRole('link', { name: 'Engineering wiki' }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Engineering wiki');
  const doc = page.getByRole('article', { name: 'Payments architecture' });
  await expect(doc).toContainText('v14 · 2 chunks · updated 2026-09-30');
  await expect(doc.getByRole('link', { name: 'Open the original of Payments architecture' })).toHaveAttribute('href', 'https://acme.atlassian.net/wiki/spaces/ENG/pages/101');

  await doc.getByRole('link', { name: 'Payments architecture', exact: true }).click();
  await expect(page).toHaveURL(/#\/knowledge\/src_wiki\/doc_payments(\?workspace=\w+)?$/);
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Payments architecture');
  const chunk = page.getByRole('article', { name: 'Chunk 1' });
  await expect(chunk).toContainText('Mobile sessions expire after 15 minutes idle.');
  await expect(chunk).toContainText('Payments architecture > Session timeout · v14');
  await expect(chunk).toContainText(/~\d+ tokens/);

  await page.getByRole('link', { name: 'All docs' }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Engineering wiki');
  await page.getByRole('link', { name: 'All sources' }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Knowledge');
});

test('knowledge: search shows cited hits; delete asks first', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/knowledge');
  await page.getByRole('textbox', { name: 'Search knowledge' }).fill('refresh token');
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  const hits = page.getByRole('region', { name: /matches for “refresh token”/ });
  const cite = hits.getByRole('link', { name: 'Payments architecture > Session timeout · v14 · 2026-09-30' });
  await expect(cite).toHaveAttribute('href', 'https://acme.atlassian.net/wiki/spaces/ENG/pages/101');
  await expect(hits.getByText('The refresh token is valid for 30 days', { exact: false })).toBeVisible();

  await source(page, 'Tickets').getByRole('button', { name: 'Remove Tickets' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('heading', { name: 'Remove Tickets from Main?' })).toBeVisible();
  await dialog.getByRole('button', { name: 'Keep it' }).click();
  await expect(source(page, 'Tickets')).toBeVisible();
  await source(page, 'Tickets').getByRole('button', { name: 'Remove Tickets' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Remove source' }).click();
  await expect(page.getByText('Tickets removed from Main')).toBeVisible();
  await expect(source(page, 'Tickets')).toHaveCount(0);
});

test('settings: embeddings save keeps the API key write-only', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/settings/embeddings');
  const form = page.getByRole('form', { name: /Embeddings/ });
  await expect(form).toContainText('chunk is sent to it');
  await form.getByRole('combobox', { name: 'Provider' }).selectOption('openai');
  await form.getByRole('button', { name: 'Save' }).click();
  await expect(form.getByText('Name the embedding model.')).toBeVisible();

  await form.getByRole('textbox', { name: 'Model' }).fill('text-embedding-3-small');
  await form.getByLabel('API key').fill('sk-test-123');
  await form.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Embeddings saved')).toBeVisible();
  await expect(form.getByText('Key saved')).toBeVisible();
  await expect(form.getByLabel('API key')).toHaveValue('');
  await expect(form.getByLabel('API key')).toHaveAttribute('placeholder', 'Leave empty to keep the saved key');
});

test('task context: sources and budget change the pack preview', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/tasks/SUN-142');
  const context = page.getByRole('main').getByRole('region', { name: 'Context' });
  const summary = context.locator('.context__summary');
  const numbers = async () => {
    const [, tokens, docs] = (await summary.textContent())!.match(/^(\d+) tokens · (\d+) docs/)!;
    return { tokens: Number(tokens), docs: Number(docs) };
  };
  await expect(summary).toContainText('5 docs · 0 trimmed');
  const before = await numbers();

  await context.getByRole('group', { name: 'Knowledge sources' }).getByText('Engineering wiki', { exact: true }).click();
  await expect(context.getByRole('checkbox', { name: 'Engineering wiki' })).not.toBeChecked();
  await expect(summary).toContainText('3 docs');
  expect((await numbers()).tokens).toBeLessThan(before.tokens);

  await context.getByRole('spinbutton', { name: 'Token budget' }).fill('60');
  await expect(summary).toContainText(/[1-9] trimmed/);
  await expect(context.getByRole('list', { name: 'Context pack' }).getByText('Pointer').first()).toBeVisible();

  await context.getByRole('button', { name: 'Save context' }).click();
  await expect(page.getByText('Context saved')).toBeVisible();
  // Saved: reopening the task brings the same selection back.
  await page.getByRole('main').getByRole('link', { name: 'Tasks', exact: true }).click();
  await page.getByRole('link', { name: /SUN-142/ }).click();
  await expect(context.getByRole('checkbox', { name: 'Engineering wiki' })).not.toBeChecked();
  await expect(context.getByRole('checkbox', { name: 'Runbooks' })).toBeChecked();
  await expect(context.getByRole('spinbutton', { name: 'Token budget' })).toHaveValue('60');
});

test('task context: suggested sources, cost estimate and block mode', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/tasks/SUN-142');
  const context = page.getByRole('main').getByRole('region', { name: 'Context' });
  await expect(context.locator('.context__cost')).toHaveText(/≈ \$\d+\.\d{2} per agent step/);

  // A suggested source not in the selection gets an Add button that selects it.
  await context.getByRole('group', { name: 'Knowledge sources' }).getByText('Engineering wiki', { exact: true }).click();
  const wiki = context.getByRole('checkbox', { name: 'Engineering wiki' });
  await expect(wiki).not.toBeChecked();
  const suggested = context.getByRole('list', { name: 'Suggested sources' });
  await suggested.getByRole('button', { name: 'Add Engineering wiki' }).click();
  await expect(wiki).toBeChecked();
  await expect(suggested.getByRole('button', { name: 'Add Engineering wiki' })).toHaveCount(0);

  // Block mode: a pack that doesn't fit says why the step won't start; trim mode doesn't.
  await context.getByRole('spinbutton', { name: 'Token budget' }).fill('60');
  await expect(context.locator('.context__summary')).toContainText(/[1-9] trimmed/);
  await expect(context.getByText('Blocked', { exact: true })).toHaveCount(0);
  await context.locator('label:has(input[value="block"])').click();
  await expect(context.getByRole('status')).toContainText('over budget');

  await context.getByRole('button', { name: 'Save context' }).click();
  await expect(page.getByText('Context saved')).toBeVisible();
  await page.getByRole('main').getByRole('link', { name: 'Tasks', exact: true }).click();
  await page.getByRole('link', { name: /SUN-142/ }).click();
  await expect(context.getByRole('radio', { name: /Block/ })).toBeChecked();
});

test('360 px: knowledge, chunks and settings fit without sideways scroll', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/knowledge');
  await expect(source(page, 'Engineering wiki')).toContainText('architecture.drawio');
  await no_overflow(page);
  expect((await source(page, 'Runbooks').getByRole('button', { name: 'Upload' }).boundingBox())?.height ?? 0).toBeGreaterThanOrEqual(44);

  await page.goto('/?mock=1#/knowledge/src_wiki/doc_payments');
  await expect(page.getByRole('article', { name: 'Chunk 2' })).toContainText('three times with backoff');
  await no_overflow(page);

  await page.goto('/?mock=1#/tasks/SUN-142');
  const context = page.getByRole('region', { name: 'Context' });
  await expect(context.getByRole('list', { name: 'Suggested sources' })).toBeVisible();
  await context.locator('label:has(input[value="block"])').click();
  await no_overflow(page);

  await page.goto('/?mock=1#/settings/embeddings');
  await page.getByRole('combobox', { name: 'Provider' }).selectOption('openai');
  await expect(page.getByLabel('API key')).toBeVisible();
  await no_overflow(page);
});
