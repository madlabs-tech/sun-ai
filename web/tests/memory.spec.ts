import { expect, test } from '@playwright/test';

// Long-term memory: proposals, review, the repeat-mistake rate and stars, mock API.
test('memory screen shows the repeat rate and reviews proposals', async ({ page }) => {
  await page.goto('/?mock=1#/memory');
  const metric = page.getByRole('region', { name: 'Repeat mistakes' });
  await expect(metric).toContainText('41%');
  await expect(metric).toContainText('(17 of 41)');
  await expect(metric).toContainText('Down from 61% to 26% over the last 4 weeks.');
  await expect(metric.getByRole('list', { name: /Repeat rate by week/ }).getByRole('listitem')).toHaveCount(8);
  await expect(metric).toContainText('fix-bug');
  await expect(metric).toContainText('claude-code');

  // First-pass gates per runtime, and review rejections over time.
  const first_pass = page.getByRole('region', { name: 'Gates passed first time' });
  const fp_runtime = first_pass.getByRole('list', { name: 'First-pass rate by runtime' });
  await expect(fp_runtime.getByRole('listitem').first()).toContainText('claude-code73%30 gate steps');
  const rejections = page.getByRole('region', { name: 'Review rejections' });
  await expect(rejections).toContainText('9');
  await expect(rejections).toContainText('Down from 5 to 4 over the last 4 weeks.');
  await expect(rejections.getByRole('list', { name: /Review rejections by week/ }).getByRole('listitem')).toHaveCount(8);
  await expect(rejections.getByRole('list', { name: 'Review rejections by runtime' }).getByRole('listitem').first()).toContainText('codex5rejections');

  const proposals = page.getByRole('region', { name: /Proposals/ });
  const active = page.getByRole('region', { name: /Active/ });
  await expect(proposals.getByRole('article')).toHaveCount(2);
  await expect(active.getByRole('article')).toHaveCount(2);

  // Every proposal opens a PR to the file it edits: the repo one to CLAUDE.md,
  // the workflow one to its prompt file.
  const repo = proposals.getByRole('article', { name: /Run the tests/ });
  await expect(repo).toContainText('Sun suggests');
  await expect(repo).toContainText('Repo · acme/api');
  await expect(repo.getByRole('link', { name: 'Review the PR to CLAUDE.md' })).toHaveAttribute('href', 'https://github.com/acme/api/pull/418');

  const starred = proposals.getByRole('article', { name: /sync scheduler/ });
  await expect(starred).toContainText('Starred by ana');
  await expect(starred).toContainText('Opening a PR to .sun/prompts/fix-bug.md');
  await starred.getByRole('button', { name: 'Approve' }).click();
  await expect(page.getByText('Agents get it in their context from their next step.')).toBeVisible();
  await expect(proposals.getByRole('article')).toHaveCount(1);
  await expect(active.getByRole('article')).toHaveCount(3);
  await expect(active.getByRole('article', { name: /sync scheduler/ })).toContainText('Approved by you');

  await active.getByRole('article', { name: /YYYY-MM-DD/ }).getByRole('button', { name: 'Retire' }).click();
  await expect(page.getByText('Agents no longer get it.')).toBeVisible();
  await expect(active.getByRole('article')).toHaveCount(2);
});

test('starring a correction proposes it, also on the approvals inbox', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-145');
  const corrections = page.getByRole('region', { name: /Corrections/ });
  const star = corrections.getByRole('button', { name: 'Propose as memory' }).first();
  await star.click();
  await expect(page.getByText('Proposed as memory', { exact: true })).toBeVisible();
  await expect(star).toHaveAttribute('aria-pressed', 'true');

  await page.goto('/?mock=1#/approvals');
  const inbox = page.getByRole('region', { name: /Memory proposals/ });
  await expect(inbox.getByRole('article')).toHaveCount(3);
  const mine = inbox.getByRole('article', { name: /offline_test\.go/ });
  await expect(mine).toContainText('Starred by you');
  await mine.getByRole('button', { name: 'Reject' }).click();
  await expect(page.getByText('Sun won’t propose it again.')).toBeVisible();
  await expect(inbox.getByRole('article')).toHaveCount(2);
});

test('memory fits a phone without horizontal scroll', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/memory');
  await expect(page.getByRole('region', { name: /Proposals/ }).getByRole('article')).toHaveCount(2);
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
  const approve = page.getByRole('button', { name: 'Approve' }).first();
  expect((await approve.boundingBox())!.height).toBeGreaterThanOrEqual(36);
});
