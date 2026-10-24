// The editor's terminal panel (over a faked kernel socket: the mock has no host), quick open and its commands,
// and Settings → Editor access: an owner grants a role read, and a member of it gets a read-only editor with
// no terminal.
import { expect, test, type Page, type WebSocketRoute } from '@playwright/test';

const API_REPO = '01JAR000100000000000000000';
/** The editor, loaded: its tree shows. */
async function open(page: Page, query = '') {
  await page.goto(`/?mock=1${query}#/editor/${API_REPO}`);
  await expect(page.getByRole('navigation', { name: 'Files' }).getByRole('button', { name: 'README.md', exact: true })).toBeVisible();
}
const panel = (page: Page) => page.getByRole('region', { name: 'Terminal' });
const rows = (page: Page) => panel(page).locator('.xterm-rows > div');

test.use({ viewport: { width: 1280, height: 800 } });

/** A shell that echoes what is typed and answers `echo <x>` with `<x>`. Records each connect's URL. */
async function fake_shell(page: Page, on_socket: (ws: WebSocketRoute) => void = () => {}) {
  const urls: string[] = [];
  await page.routeWebSocket(/\/api\/v1\/workspaces\/[^/]+\/repos\/[^/]+\/terminal\?/, (ws) => {
    urls.push(ws.url());
    let line = '';
    ws.send(JSON.stringify({ kind: 'open', pty_id: 'pty_1' }));
    ws.onMessage((message) => {
      if (typeof message === 'string') return; // resize
      for (const ch of message.toString()) {
        if (ch !== '\r') {
          line += ch;
          ws.send(Buffer.from(ch));
          continue;
        }
        const said = line.match(/^echo (.*)$/)?.[1];
        ws.send(Buffer.from(`\r\n${said !== undefined ? said + '\r\n' : ''}$ `));
        line = '';
      }
    });
    on_socket(ws);
  });
  return urls;
}

test('the terminal runs a command, toggles with Ctrl+` and shows why a shell ended', async ({ page }) => {
  let socket: WebSocketRoute | undefined;
  const urls = await fake_shell(page, (ws) => (socket = ws));
  await open(page);
  await expect(panel(page)).toBeHidden();
  await page.getByRole('button', { name: 'Terminal', exact: true }).click();
  await expect(panel(page)).toBeVisible();
  await expect(panel(page).getByRole('tab', { name: 'Terminal 1' })).toHaveAttribute('aria-selected', 'true');
  expect(urls[0]).toMatch(/host_id=[^&]+&cols=\d+&rows=\d+/);
  await expect(panel(page).getByRole('status')).toHaveCount(0);

  await page.keyboard.type('echo sun-ok');
  await page.keyboard.press('Enter');
  await expect(rows(page).filter({ hasText: /^\s*sun-ok\s*$/ })).toHaveCount(1);

  // Ctrl+` from inside the terminal hides it, and brings it back with the same screen.
  await page.keyboard.press('Control+Backquote');
  await expect(panel(page)).toBeHidden();
  await page.keyboard.press('Control+Backquote');
  await expect(panel(page)).toBeVisible();
  await expect(rows(page).filter({ hasText: /^\s*sun-ok\s*$/ })).toHaveCount(1);

  socket?.send(JSON.stringify({ kind: 'exit', code: 3 }));
  await expect(panel(page).getByRole('status')).toContainText('Session ended (exit 3)');
});

test('a dropped socket reattaches to the same shell and keeps the screen', async ({ page }) => {
  let socket: WebSocketRoute | undefined;
  const urls = await fake_shell(page, (ws) => (socket = ws));
  await open(page);
  await page.keyboard.press('Control+Backquote');
  await expect(panel(page).getByRole('status')).toHaveCount(0);
  await page.keyboard.type('echo before');
  await page.keyboard.press('Enter');
  await expect(rows(page).filter({ hasText: /^\s*before\s*$/ })).toHaveCount(1);

  await socket?.close();
  await expect(panel(page).getByRole('status')).toHaveText('Connection lost. Reconnecting…');
  await expect(panel(page).getByRole('status')).toHaveCount(0);
  expect(urls[1]).toMatch(/pty_id=pty_1/);
  await expect(rows(page).filter({ hasText: /^\s*before\s*$/ })).toHaveCount(1);
});

test('at most five terminals; closing the last one hides the panel', async ({ page }) => {
  await fake_shell(page);
  await open(page);
  await page.keyboard.press('Control+Backquote');
  const add = panel(page).getByRole('button', { name: 'New terminal' });
  for (let i = 0; i < 4; i++) await add.click();
  await expect(panel(page).getByRole('tab')).toHaveCount(5);
  await expect(panel(page).getByRole('button', { name: 'At most 5 terminals per host' })).toBeDisabled();
  await panel(page).getByRole('button', { name: 'Close terminal 5' }).click();
  await expect(add).toBeEnabled();
  for (let i = 0; i < 4; i++) await panel(page).getByRole('button', { name: 'Close terminal 1' }).click();
  await expect(panel(page)).toBeHidden();
});

test('the host’s refusal shows in its words; a dropped host says so', async ({ page }) => {
  const answers = [{ kind: 'error', message: 'You already have 5 terminals on this host.' }, { kind: 'dead' }];
  await page.routeWebSocket(/\/terminal\?/, (ws) => ws.send(JSON.stringify(answers.shift())));
  await open(page);
  await page.keyboard.press('Control+Backquote');
  await expect(panel(page).getByRole('status')).toContainText('You already have 5 terminals on this host.');
  await panel(page).getByRole('status').getByRole('button', { name: 'New terminal' }).click();
  await expect(panel(page).getByRole('status')).toContainText('Host disconnected.');
});

test('⌘/Ctrl+P finds a file by part of its name; Enter opens it', async ({ page }) => {
  await open(page);
  await page.keyboard.press('ControlOrMeta+p');
  const palette = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Go to file' }) });
  await page.keyboard.type('gree');
  await expect(palette.getByRole('option').first()).toContainText('greet.ts');
  await expect(palette.getByRole('option').first()).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('Enter');
  await expect(palette).toHaveCount(0);
  await expect(page.getByRole('tab', { name: 'greet.ts' })).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('.cm-content')).toContainText('export const greet');

  // Arrow keys move; Escape closes. The keys go to the palette's input, not the file just opened.
  await page.keyboard.press('ControlOrMeta+p');
  await expect(palette.getByRole('combobox', { name: 'File name' })).toBeFocused();
  await page.keyboard.type('ts');
  await expect(palette.getByRole('combobox', { name: 'File name' })).toHaveValue('ts');
  await expect(palette.getByRole('option').nth(1)).toBeVisible();
  await page.keyboard.press('ArrowDown');
  await expect(palette.getByRole('option').nth(1)).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('Escape');
  await expect(palette).toHaveCount(0);
  await expect(page.getByRole('tab', { name: 'greet.ts' }).getByRole('img', { name: 'Unsaved changes' })).toHaveCount(0);
});

test('⌘/Ctrl+Shift+P lists commands; a typed > does the same', async ({ page }) => {
  await fake_shell(page);
  await open(page);
  await page.keyboard.press('ControlOrMeta+Shift+p');
  const palette = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Run a command' }) });
  await expect(palette.getByRole('option')).toHaveText(['New file', 'Toggle terminal']);
  await page.keyboard.type('term');
  await page.keyboard.press('Enter');
  await expect(panel(page)).toBeVisible();

  await page.getByRole('button', { name: 'Go to file' }).click();
  await page.keyboard.type('>new');
  await page.keyboard.press('Enter');
  await expect(page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'New file' }) })).toBeVisible();
});

test('an owner grants a role read; a member of it reads only and has no terminal', async ({ page, context }) => {
  await page.goto('/?mock=1#/settings/editor');
  await expect(page.getByRole('article', { name: 'Workspace owners' })).toContainText('can’t be revoked');
  await expect(page.getByRole('article', { name: 'Workspace owners' }).getByRole('button')).toHaveCount(0);
  await expect(page.getByText('Nobody else has access yet.')).toBeVisible();

  await page.getByLabel('Who', { exact: true }).selectOption({ label: 'A role' });
  await page.getByLabel('Role', { exact: true }).selectOption({ label: 'Members and above' });
  await page.locator('label', { has: page.getByRole('checkbox', { name: /^Terminal/ }) }).click();
  await expect(page.getByRole('note')).toContainText('shell access to the host as the Sun service user');
  await page.locator('label', { has: page.getByRole('checkbox', { name: /^Terminal/ }) }).click();
  await expect(page.getByRole('note')).toHaveCount(0);
  await page.getByRole('button', { name: 'Grant' }).click();
  await expect(page.getByRole('article', { name: 'Read for Members and above' })).toContainText('Every repository');

  const member = await context.newPage();
  await open(member, '&mock_role=member');
  await expect(member.getByText('Read-only')).toBeVisible();
  await member.getByRole('navigation', { name: 'Files' }).getByRole('button', { name: 'README.md', exact: true }).click();
  await expect(member.locator('.cm-content')).toContainText('# Sample repo');
  await expect(member.getByRole('button', { name: 'Terminal', exact: true })).toHaveCount(0);
  await member.keyboard.press('Control+Backquote');
  await expect(panel(member)).toHaveCount(0);

  // Revoked: the member has no editor at all.
  await page.getByRole('button', { name: 'Revoke Read for Members and above' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Revoke' }).click();
  await expect(page.getByText('Nobody else has access yet.')).toBeVisible();
  await member.reload();
  await expect(member.getByText('You don’t have access to this repository’s editor.')).toBeVisible();
});
