// BLO-166 pass 3 over BLO-162 PR 3 (chat without an agent, web): the picker's edges beyond chat.spec.ts. Every
// state a tool can't run in, says why; who pays for each kind of runtime; nothing picked, a broken or stale remembered
// choice, and the choice kept per workspace; a switch refused mid-reply, a cancelled one, and New chat on a direct
// chat; a missing chat; the viewer; 360 px; the keyboard.
import { expect, test, type Locator, type Page } from '@playwright/test';

const MAIN = '00000000000000000000000001';
const REMEMBER = `sun.direct_chat.${MAIN}`;
const open = (page: Page, query = '', route = 'chat') => page.goto(`/?mock=1${query}#/${route}`);
const main = (page: Page) => page.getByRole('main');
const picker = (page: Page) => page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Chat with a tool' }) });
const without_agent = (page: Page) => main(page).getByRole('navigation', { name: 'Without an agent' });
/** A tool's row in the picker: its radio's label with the line under it (why it can't run, or where it comes from). */
const tool_row = (dialog: Locator, name: string) => dialog.locator('label').filter({ hasText: name });

/** Opens the picker, picks `tool` and starts the chat; returns its thread. */
async function start_chat(page: Page, tool: string, title = tool) {
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  await tool_row(picker(page), tool).click();
  await picker(page).getByRole('button', { name: 'Start chat' }).click();
  await expect(picker(page)).toBeHidden();
  return main(page).getByRole('region', { name: `Chat with ${title}` });
}

test('picker: every tool that can’t take a chat says why, and every runtime says who pays', async ({ page }) => {
  await open(page);
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  const dialog = picker(page);
  // Claude Code is busy on hery-mbp, so it can take a chat even with its other hosts offline or logged out.
  await expect(dialog.getByRole('radio', { name: /^Claude Code/ })).toBeEnabled();
  // An outdated CLI still runs.
  await expect(dialog.getByRole('radio', { name: /^Codex/ })).toBeEnabled();
  await expect(tool_row(dialog, 'OpenCode')).toContainText(/Limit reached, back at .+\./);
  await expect(tool_row(dialog, 'kiro-cli')).toContainText('Not supported yet.');
  // Key-backed runtimes say where they come from.
  for (const [name, source] of [['lan-qwen', 'Your API key'], ['credits', 'Sun credits']] as const) {
    await expect(dialog.getByRole('radio', { name: new RegExp(`^${name}`) })).toBeEnabled();
    await expect(tool_row(dialog, name)).toContainText(source);
  }
  // Who pays, for a host login, a workspace key and credits.
  await tool_row(dialog, 'Codex').click();
  await expect(dialog).toContainText('Runs on its host’s Codex login.');
  await dialog.getByLabel('Host').selectOption({ label: 'hery-mbp' });
  await expect(dialog).toContainText('Runs on hery-mbp’s Codex login.');
  await tool_row(dialog, 'lan-qwen').click();
  await expect(dialog).toContainText('Paid by the workspace key lan-qwen.');
  await tool_row(dialog, 'credits').click();
  await expect(dialog).toContainText('Paid from workspace credits (credits).');
});

test('picker: nothing picked asks for a tool and starts nothing', async ({ page }) => {
  await open(page);
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  await expect(picker(page).getByRole('radio', { checked: true })).toHaveCount(0);
  await expect(picker(page).getByLabel('Permissions')).toHaveCount(0);
  await picker(page).getByRole('button', { name: 'Start chat' }).click();
  await expect(picker(page).getByRole('alert')).toHaveText('Pick a tool.');
  await picker(page).getByRole('button', { name: 'Cancel' }).click();
  await expect(without_agent(page).locator('button.chat__agent')).toHaveCount(0);
  await expect(page).toHaveURL(/#\/chat(\?|$)/);
});

test('picker: a broken remembered choice opens it empty', async ({ page }) => {
  await open(page);
  await page.evaluate((key) => localStorage.setItem(key, '{not json'), REMEMBER);
  await page.reload();
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  await expect(picker(page).getByRole('radio', { checked: true })).toHaveCount(0);
  await expect(picker(page).getByRole('alert')).toHaveCount(0);
});

// BUG (BLO-162 PR 3): `pick()` restores the remembered tool without checking that it can still take a chat, so the
// picker opens with a greyed radio checked and Start chat opens a chat on a tool that's at its limit.
test('picker: a remembered tool that can’t take a chat now is not picked for you', async ({ page }) => {
  await open(page);
  await page.evaluate((key) => localStorage.setItem(key, JSON.stringify({ runtime: { tool: 'opencode' }, permission_mode: null })), REMEMBER);
  await page.reload();
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  const opencode = picker(page).getByRole('radio', { name: /^OpenCode/ });
  await expect(opencode).toBeDisabled();
  await expect(opencode).not.toBeChecked();
  await picker(page).getByRole('button', { name: 'Start chat' }).click();
  await expect(picker(page).getByRole('alert')).toHaveText('Pick a tool.');
  await expect(without_agent(page)).not.toContainText('OpenCode');
});

test('picker: the remembered choice is per workspace', async ({ page }) => {
  await open(page, '&invited=1');
  const thread = await start_chat(page, 'Claude Code');
  await expect(thread).toBeVisible();
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  await expect(picker(page).getByRole('radio', { name: /^Claude Code/ })).toBeChecked();
  await picker(page).getByRole('button', { name: 'Cancel' }).click();

  await page.goto('/?mock=1&invited=1#/chat?workspace=design');
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  await expect(picker(page).getByRole('radio', { checked: true })).toHaveCount(0);
  // Main's chat isn't listed on Design either.
  await expect(without_agent(page)).not.toContainText('Claude Code');
});

test('picker: a mode the newly picked tool can’t enforce falls back to the host default', async ({ page }) => {
  await open(page);
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  const dialog = picker(page);
  await tool_row(dialog, 'Claude Code').click();
  await dialog.getByLabel('Permissions').selectOption({ label: 'Edit files' });
  // No host reports an exact `accept_edits` for Codex.
  await tool_row(dialog, 'Codex').click();
  await expect(dialog.getByLabel('Permissions')).toHaveValue('');
  await dialog.getByRole('button', { name: 'Start chat' }).click();
  await expect(main(page).getByRole('region', { name: 'Chat with Codex' })).toContainText('Default model · Host default mode');
});

test('switch: refused while the tool replies, and nothing changes', async ({ page }) => {
  await open(page);
  const thread = await start_chat(page, 'Claude Code');
  const box = thread.getByRole('textbox', { name: 'Message' });
  await box.fill('Take your time');
  await box.press('ControlOrMeta+Enter');
  await expect(thread.getByRole('status')).toContainText('Claude Code is replying');
  await thread.getByRole('button', { name: 'Switch model, effort or mode' }).click();
  const switcher = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Switch Claude Code' }) });
  await switcher.getByLabel('Permissions').selectOption({ label: 'Plan only' });
  await switcher.getByRole('button', { name: 'Switch' }).click();
  await expect(switcher.getByRole('alert')).toContainText('Wait for the reply or stop it, then switch.');
  await expect(switcher).toBeVisible();
  await switcher.getByRole('button', { name: 'Cancel' }).click();
  await expect(thread).toContainText('Default model · Host default mode');
  await expect(thread.getByRole('region', { name: 'Messages' })).not.toContainText('switched to');
});

test('switch: Cancel keeps the chat as it was', async ({ page }) => {
  await open(page);
  const thread = await start_chat(page, 'Claude Code');
  await thread.getByRole('button', { name: 'Switch model, effort or mode' }).click();
  const switcher = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Switch Claude Code' }) });
  await switcher.getByLabel('Effort').selectOption({ label: 'High' });
  await switcher.getByRole('button', { name: 'Cancel' }).click();
  await expect(switcher).toBeHidden();
  await expect(thread).toContainText('Default model · Host default mode');
  await expect(thread.getByRole('region', { name: 'Messages' })).not.toContainText('switched to');
  // Reopened, it shows the chat's settings again, not the cancelled edit.
  await thread.getByRole('button', { name: 'Switch model, effort or mode' }).click();
  await expect(switcher.getByLabel('Effort')).toHaveValue('');
});

test('New chat on a chat without an agent starts a second one with the same settings', async ({ page }) => {
  await open(page);
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  await tool_row(picker(page), 'lan-qwen').click();
  await picker(page).getByRole('button', { name: 'Start chat' }).click();
  const thread = main(page).getByRole('region', { name: 'Chat with lan-qwen' });
  await expect(thread).toContainText('Paid by the workspace key lan-qwen.');
  const first = page.url();

  await thread.getByRole('button', { name: 'New chat' }).click();
  await expect(picker(page).getByRole('radio', { name: /^lan-qwen/ })).toBeChecked();
  await picker(page).getByRole('button', { name: 'Start chat' }).click();
  await expect(picker(page)).toBeHidden();
  await expect(page).not.toHaveURL(first);
  await expect(without_agent(page).getByRole('button', { name: /^lan-qwen/ })).toHaveCount(2);
});

test('a chat id that isn’t yours, or isn’t there, shows no chat', async ({ page }) => {
  await open(page, '', 'chat/01JAH0NOTAREALCHAT00000000');
  await expect(main(page)).toContainText('This chat isn\'t here.');
  await expect(main(page).getByRole('textbox', { name: 'Message' })).toHaveCount(0);
});

test('a viewer can’t start or switch a chat with a tool', async ({ page }) => {
  await open(page, '&mock_role=viewer');
  await expect(main(page).getByRole('heading', { level: 1 })).toHaveText('Chat');
  await expect(without_agent(page)).toBeVisible();
  await expect(main(page).getByRole('button', { name: 'Chat with a tool' })).toHaveCount(0);
});

test('360 px: the picker and a direct chat’s header fit', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page);
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  await tool_row(picker(page), 'Claude Code').click();
  await picker(page).getByLabel('Host').selectOption({ label: 'hery-mbp' });
  await expect(picker(page)).toContainText('Runs on hery-mbp’s Claude Code login.');
  const overflow = () => page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(await overflow()).toBeLessThanOrEqual(0);
  const box = await picker(page).boundingBox();
  expect(box!.x).toBeGreaterThanOrEqual(0);
  expect(box!.x + box!.width).toBeLessThanOrEqual(360);
  await picker(page).getByRole('button', { name: 'Start chat' }).click();
  const thread = main(page).getByRole('region', { name: 'Chat with Claude Code' });
  await expect(thread).toContainText('Runs on hery-mbp’s Claude Code login.');
  expect(await overflow()).toBeLessThanOrEqual(0);
  // The switch button is a touch target.
  const target = await thread.getByRole('button', { name: 'Switch model, effort or mode' }).boundingBox();
  expect(Math.min(target!.width, target!.height)).toBeGreaterThanOrEqual(24);
});

test('keyboard: the picker takes a tool from the keyboard and Escape closes it', async ({ page }) => {
  await open(page);
  const start = main(page).getByRole('button', { name: 'Chat with a tool' });
  await start.focus();
  await page.keyboard.press('Enter');
  await expect(picker(page)).toBeVisible();
  await picker(page).getByRole('radio', { name: /^Claude Code/ }).focus();
  await page.keyboard.press('Space');
  await expect(picker(page).getByRole('radio', { name: /^Claude Code/ })).toBeChecked();
  await expect(picker(page).getByLabel('Permissions')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(picker(page)).toBeHidden();
  await expect(without_agent(page).locator('button.chat__agent')).toHaveCount(0);
});
