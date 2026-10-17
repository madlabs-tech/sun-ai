// Chat: pick an agent, send a message, watch it reply (status line and live words), start a new chat; chats
// stay off the board; at 360 px the list and the thread take turns. Without an agent: the picker (tools that
// can't run greyed with why, host, model, effort, mode, who pays, the bypass gate, the remembered choice), a
// tool's reply, and switching model, effort or mode mid-chat.
import { expect, test, type Locator, type Page } from '@playwright/test';

const open = (page: Page, route = 'chat') => page.goto(`/?mock=1#/${route}`);
const main = (page: Page) => page.getByRole('main');
/** Checks a tool in the picker by its label (the radio itself is drawn by the design system). */
const pick_tool = (picker: Locator, name: string) => picker.locator('label').filter({ hasText: name }).click();

test('chat: an agent answers in the thread, and New chat starts over', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 860 });
  await open(page);
  await expect(main(page).getByRole('heading', { level: 1 })).toHaveText('Chat');
  const agents = main(page).getByRole('navigation', { name: 'Agents' });
  const ada = agents.getByRole('button', { name: /^Ada/ });
  await expect(ada).toContainText('No messages yet');
  await ada.click();
  await expect(page).toHaveURL(/#\/chat\/.+/);

  const thread = main(page).getByRole('region', { name: 'Chat with Ada' });
  const box = thread.getByRole('textbox', { name: 'Message' });
  await expect(thread).toContainText('Ada answers here.');
  await box.fill('What are you working on?');
  await box.press('ControlOrMeta+Enter');
  const messages = thread.getByRole('region', { name: 'Messages' });
  await expect(messages.getByText('What are you working on?')).toBeVisible();
  await expect(thread.getByRole('status')).toContainText('Ada is replying');
  await expect(thread.getByText('Looking into it…')).toBeVisible();
  await expect(messages).toContainText('Ada replied');
  await expect(messages).toContainText('Ada here: noted');
  await expect(thread.getByRole('status')).toHaveText('');
  await expect(ada).toContainText('Ada here: noted');

  await thread.getByRole('button', { name: 'New chat' }).click();
  await expect(main(page).getByRole('region', { name: 'Messages' }).getByText('Nothing yet.')).toBeVisible();

  // A chat is no task.
  await open(page, 'tasks');
  await expect(main(page)).not.toContainText('Chat with Ada');
});

test('chat at 360 px: the list, then the thread with a way back', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 760 });
  await open(page);
  const agents = main(page).getByRole('navigation', { name: 'Agents' });
  await agents.getByRole('button', { name: /^Ada/ }).click();
  await expect(agents).toBeHidden();
  const thread = main(page).getByRole('region', { name: 'Chat with Ada' });
  await expect(thread.getByRole('textbox', { name: 'Message' })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(360);
  await thread.getByRole('link', { name: 'Chats' }).click();
  await expect(agents).toBeVisible();
});

test('chat without an agent: pick a tool, talk to it, switch its effort and mode', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 860 });
  await open(page);
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  const picker = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Chat with a tool' }) });

  // Tools that can't take a chat now are greyed, with why; key-backed ones go by their own name.
  await expect(picker.getByRole('radio', { name: /^Claude Code/ })).toBeEnabled();
  await expect(picker.getByRole('radio', { name: /^OpenCode/ })).toBeDisabled();
  await expect(picker).toContainText('Limit reached, back at');
  await expect(picker.getByRole('radio', { name: /^kiro-cli/ })).toBeDisabled();
  await expect(picker.getByRole('radio', { name: /^lan-qwen/ })).toBeEnabled();

  await pick_tool(picker, 'lan-qwen');
  await expect(picker).toContainText('Paid by the workspace key lan-qwen.');
  await pick_tool(picker, 'Claude Code');
  await expect(picker).toContainText('Runs on its host’s Claude Code login.');
  await picker.getByLabel('Effort').selectOption({ label: 'High' });
  const mode = picker.getByLabel('Permissions');
  await mode.selectOption({ label: 'Plan only' });
  await picker.getByRole('button', { name: 'Start chat' }).click();
  await expect(picker).toBeHidden();

  const thread = main(page).getByRole('region', { name: 'Chat with Claude Code' });
  await expect(thread).toContainText('Default model · high · Plan only');
  await expect(thread).toContainText('Runs on its host’s Claude Code login.');
  const box = thread.getByRole('textbox', { name: 'Message' });
  await box.fill('Hello there');
  await box.press('ControlOrMeta+Enter');
  const messages = thread.getByRole('region', { name: 'Messages' });
  await expect(messages).toContainText('Claude Code replied');
  await expect(main(page).getByRole('navigation', { name: 'Without an agent' })).toContainText('Claude Code here: noted');

  // Another effort and mode: the same thread, a switch line, a fresh session.
  await thread.getByRole('button', { name: 'Switch model, effort or mode' }).click();
  const switcher = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Switch Claude Code' }) });
  await switcher.getByLabel('Effort').selectOption({ label: 'Low' });
  await switcher.getByLabel('Permissions').selectOption({ label: 'Edit files' });
  await switcher.getByRole('button', { name: 'Switch' }).click();
  await expect(switcher).toBeHidden();
  await expect(messages).toContainText('switched to Default model · low · Edit files. A new session reads the last 20 messages.');
  await expect(thread).toContainText('Default model · low · Edit files');

  // Another host is another chat; the picker remembers the last choice.
  await thread.getByRole('button', { name: 'Switch model, effort or mode' }).click();
  await switcher.getByLabel('Host').selectOption({ label: 'hery-mbp' });
  await expect(switcher).toContainText('Another tool or host starts a new chat');
  await expect(switcher.getByRole('button', { name: 'Start chat' })).toBeVisible();
  await switcher.getByRole('button', { name: 'Cancel' }).click();
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  await expect(picker.getByRole('radio', { name: /^Claude Code/ })).toBeChecked();
  await expect(picker.getByLabel('Effort')).toHaveValue('low');
  await expect(picker.getByLabel('Permissions')).toHaveValue('accept_edits');
});

test('chat without an agent: bypass only with a container or VM host, allow_bypass and manage_runtimes', async ({ page }) => {
  await open(page);
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  const picker = page.getByRole('dialog');
  await pick_tool(picker, 'Claude Code');
  const mode = picker.getByLabel('Permissions');
  // Any host: hery-mbp has a container ready.
  await expect(mode.getByRole('option', { name: 'Anything' })).toHaveCount(1);
  // build-box-01 has none.
  await picker.getByLabel('Host').selectOption({ label: 'build-box-01' });
  await expect(mode.getByRole('option', { name: 'Anything' })).toHaveCount(0);
  await expect(picker).toContainText('“Anything” needs an online host with a container or VM ready.');

  await page.goto('/?mock=1&no_bypass#/chat');
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  await pick_tool(picker, 'Claude Code');
  await expect(mode.getByRole('option', { name: 'Anything' })).toHaveCount(0);
  await expect(picker).toContainText('“Anything” is off on this deployment');

  await page.goto('/?mock=1&mock_role=member#/chat');
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  await pick_tool(picker, 'Claude Code');
  await expect(mode.getByRole('option', { name: 'Anything' })).toHaveCount(0);
  await expect(picker).toContainText('“Anything” needs the manage runtimes permission.');
});
