// Catalog screen against the in-memory API.
import { expect, test } from '@playwright/test';

test('catalog: suggestions rank by stack, install shows the files first, nothing installs on its own', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/catalog');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Catalog');

  // Installed: pinned versions, an update is offered but not applied.
  const tdd = page.getByRole('article').filter({ hasText: 'tdd' });
  await expect(tdd).toContainText('v1.2.2');
  await expect(tdd.getByRole('button', { name: 'Update to v1.2.3' })).toBeVisible();

  // Suggested, ranked for the detected stack.
  await page.getByRole('tab', { name: /Suggested/ }).click();
  await page.getByLabel('Suggest for a repo').fill('/work/app');
  await page.getByRole('button', { name: 'Detect stack' }).click();
  await expect(page.getByLabel('Detected stack')).toContainText('svelte');
  await expect(page.getByRole('article').first()).toContainText('frontend-design');
  await expect(page.getByRole('article').first()).toContainText('Fits this repo: frontend');

  // Dismiss hides one; it can come back.
  await page.getByRole('button', { name: 'Dismiss frontend-design' }).click();
  await expect(page.getByRole('article').filter({ hasText: 'frontend-design' })).toHaveCount(0);
  await page.getByRole('button', { name: 'Show 1 dismissed' }).click();
  await expect(page.getByRole('article').filter({ hasText: 'frontend-design' })).toHaveCount(1);

  // Install: read the full text, then confirm.
  await page.getByRole('article').filter({ hasText: 'diagnosing-bugs' }).getByRole('button', { name: 'Install' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('heading', { name: 'Install diagnosing-bugs' })).toBeVisible();
  await expect(dialog.getByText('SKILL.md')).toBeVisible();
  await expect(dialog).toContainText('.claude/skills');
  await dialog.getByRole('button', { name: 'Install' }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByRole('article').filter({ hasText: 'diagnosing-bugs' })).toBeVisible();
  await expect(page.getByRole('tab', { name: /Installed/ })).toHaveAttribute('aria-selected', 'true');

  // Suggestions can be hidden entirely.
  await page.getByRole('tab', { name: /Suggested/ }).click();
  await page.getByText('Show suggestions').click();
  await expect(page.getByText('Suggestions are hidden.')).toBeVisible();

  // MCP servers show hosts and secrets.
  await page.getByRole('tab', { name: 'MCP servers' }).click();
  await page.getByRole('tab', { name: /Installed/ }).click();
  await expect(page.getByRole('article').filter({ hasText: 'github' })).toContainText('needs GITHUB_TOKEN');

  // Role templates (GET /templates): their mods install only when ticked.
  await page.getByRole('tab', { name: 'Workflows' }).click();
  await page.getByRole('tab', { name: /Suggested/ }).click();
  await page.getByText('Show suggestions').click();
  await page.getByRole('article').filter({ hasText: 'marketing-campaign' }).getByRole('button', { name: 'Add' }).click();
  await expect(dialog.getByText('name: marketing-campaign')).toBeVisible();
  const consent = dialog.getByRole('checkbox', { name: 'Install the social mod' });
  await expect(consent).not.toBeChecked();
  await expect(dialog).toContainText('Asks for: network:api.x.com, secret:SOCIAL_TOKEN');
  await expect(dialog.getByRole('button', { name: 'Add workflow', exact: true })).toBeVisible();
  await dialog.getByText('Install the social mod').click();
  await dialog.getByRole('button', { name: 'Add workflow + 1 mod' }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByRole('article').filter({ hasText: 'marketing-campaign' })).toContainText('Added');
});

test('catalog: an MCP server’s secrets are entered write-only in the install dialog', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/catalog');
  await page.getByRole('tab', { name: 'MCP servers' }).click();
  await page.getByRole('tab', { name: /Suggested/ }).click();
  await page.getByRole('article').filter({ hasText: 'context7' }).getByRole('button', { name: 'Install' }).click();
  const dialog = page.getByRole('dialog');
  const secret = dialog.getByLabel('CONTEXT7_API_KEY');
  await expect(secret).toHaveAttribute('type', 'password');
  await expect(dialog).toContainText('Write-only, ecosystem-wide');
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
  await secret.fill('ctx7-very-secret-value');
  await dialog.getByRole('button', { name: 'Install' }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText('context7 installed')).toBeVisible();
  const row = page.getByRole('article').filter({ hasText: 'context7' });
  await expect(row).toContainText('needs CONTEXT7_API_KEY');
  await expect(row).not.toContainText('ctx7-very');
});

test('catalog: the Update dialog shows which files changed since the installed version', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/catalog');
  await page.getByRole('article').filter({ hasText: 'tdd' }).getByRole('button', { name: 'Update to v1.2.3' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('heading', { name: 'Update tdd' })).toBeVisible();
  await expect(dialog).toContainText('3 files changed since v1.2.2');
  await expect(dialog.getByRole('heading', { name: 'SKILL.md Changed' })).toBeVisible();
  await expect(dialog.getByRole('heading', { name: 'tests.md New' })).toBeVisible();
  await expect(dialog.getByRole('heading', { name: 'mocking.md Removed' })).toBeVisible();
  await expect(dialog.getByRole('heading', { name: 'LICENSE', exact: true })).toBeVisible();
  await expect(dialog.getByText('Unchanged.')).toBeVisible();
  // The changed file shows its line diff: the old line as -, the new one as +, the rest as context.
  const diff = dialog.getByLabel(/^Changes to SKILL\.md/);
  await expect(diff.locator('[data-op="-"]')).toHaveText('-description: tdd, older wording');
  await expect(diff.locator('[data-op="+"]')).toHaveText(/^\+description: Test-driven development/);
  await expect(diff.locator('[data-op=" "]').first()).toHaveText(' ---');
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
});

for (const width of [1280, 360]) {
  test(`catalog: the skill install dialog picks which agents get it (${width}px)`, async ({ page }) => {
    await page.setViewportSize({ width, height: 800 });
    await page.goto('/?mock=1#/catalog');
    await page.getByRole('tab', { name: /Suggested/ }).click();
    await page.getByRole('article').filter({ hasText: 'diagnosing-bugs' }).getByRole('button', { name: 'Install' }).click();
    const dialog = page.getByRole('dialog');
    const agents = dialog.getByRole('group', { name: 'Agents that get it' });
    for (const name of ['Claude Code', 'Codex', 'OpenCode']) await expect(agents.getByLabel(name)).toBeChecked();
    await expect(agents).toContainText('.agents/skills');

    // None picked: install is blocked with a reason.
    for (const name of ['Claude Code', 'Codex', 'OpenCode']) await agents.getByText(name, { exact: true }).click();
    await expect(dialog.getByRole('alert')).toHaveText('Pick at least one agent.');
    await expect(dialog.getByRole('button', { name: 'Install' })).toBeDisabled();

    await agents.getByText('Codex', { exact: true }).click();
    await dialog.getByRole('button', { name: 'Install' }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByRole('article').filter({ hasText: 'diagnosing-bugs' })).toContainText('For Codex only');
    expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

    // The installed view per agent: Claude Code sessions don't get it; every-runtime skills stay.
    await page.getByLabel('Agent', { exact: true }).selectOption({ label: 'Claude Code' });
    await expect(page.getByRole('article').filter({ hasText: 'diagnosing-bugs' })).toHaveCount(0);
    await expect(page.getByRole('article').filter({ hasText: 'tdd' })).toBeVisible();
    await page.getByLabel('Agent', { exact: true }).selectOption({ label: 'Codex' });
    await expect(page.getByRole('article').filter({ hasText: 'diagnosing-bugs' })).toBeVisible();
  });
}

for (const width of [1280, 360]) {
  test(`catalog: a skill installs into a repo by PR and shows the PR link (${width}px)`, async ({ page }) => {
    await page.setViewportSize({ width, height: 800 });
    await page.goto('/?mock=1#/catalog');
    await page.getByRole('tab', { name: /Suggested/ }).click();
    const row = page.getByRole('article').filter({ hasText: 'diagnosing-bugs' });
    await row.getByRole('button', { name: 'Install' }).click();
    const dialog = page.getByRole('dialog');
    const scope = dialog.getByRole('group', { name: 'Install into' });
    await expect(scope.getByLabel('Whole ecosystem')).toBeChecked();

    await scope.getByText('A repo, by pull request').click();
    await expect(dialog).toContainText('sun/skill-diagnosing-bugs');
    const open = dialog.getByRole('button', { name: 'Open PR' });
    await expect(open).toBeDisabled();
    await dialog.getByLabel('Repo', { exact: true }).selectOption({ label: 'madlabs/web' });
    await dialog.getByRole('group', { name: 'Agents that get it' }).getByText('OpenCode', { exact: true }).click();
    await open.click();

    await expect(page.getByText('Pushed sun/skill-diagnosing-bugs')).toBeVisible();
    const link = dialog.getByRole('link', { name: 'Review the PR for sun/skill-diagnosing-bugs' });
    await expect(link).toHaveAttribute('href', 'https://github.com/madlabs/web/pull/42');
    await expect(dialog.getByRole('button', { name: 'PR requested' })).toBeDisabled();
    expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

    // The ecosystem store is untouched; reopening shows the same PR.
    await dialog.getByRole('button', { name: 'Done' }).click();
    await expect(dialog).toBeHidden();
    await row.getByRole('button', { name: 'Install' }).click();
    await scope.getByText('A repo, by pull request').click();
    await dialog.getByLabel('Repo', { exact: true }).selectOption({ label: 'madlabs/web' });
    await expect(link).toBeVisible();

    // The repo's installed view lists it with its PR; the ecosystem view doesn't.
    await dialog.getByRole('button', { name: 'Done' }).click();
    await page.getByRole('tab', { name: /Installed/ }).click();
    await expect(page.getByRole('article').filter({ hasText: 'diagnosing-bugs' })).toHaveCount(0);
    await page.getByLabel('Installed in').selectOption({ label: 'madlabs/web (by PR)' });
    const in_repo = page.getByRole('article').filter({ hasText: 'diagnosing-bugs' });
    await expect(in_repo.getByRole('link', { name: 'Review the PR for sun/skill-diagnosing-bugs' })).toBeVisible();
    await expect(in_repo).toContainText('In the repo by PR');
    await expect(page.getByRole('article').filter({ hasText: 'tdd' })).toHaveCount(0);
    await expect(page.getByLabel('Agent', { exact: true })).toHaveCount(0);
    expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
  });
}
