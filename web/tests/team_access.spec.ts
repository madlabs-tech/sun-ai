// Team access against the in-memory API: an owner makes a group by email domain (live preview, verified emails
// only) and by hand, shares hosts with it (the kernel's own host only after a confirm) and limits terminals to it;
// a @madlabs.tech member attaches the shared host and sees its runtimes; anyone else sees neither the host nor a
// terminal. The mock keeps groups, shares and the terminal policy in localStorage, so they hold across `mock_role`.
import { expect, test, type Page } from '@playwright/test';

const OWNER = '/?mock=1&mock_team=1';
const TEAM_MEMBER = '/?mock=1&mock_role=admin&mock_email=me@madlabs.tech';
const PUBLIC_MEMBER = '/?mock=1&mock_role=admin';
const API_REPO = '01JAR000100000000000000000';
const sidebar = (page: Page) => page.getByRole('complementary', { name: 'Main' });
const dialog = (page: Page) => page.getByRole('dialog');

test.use({ viewport: { width: 1280, height: 800 } });

/** As the owner: group "Madlabs team" of everyone with a verified @madlabs.tech email. */
async function create_team(page: Page) {
  await page.goto(`${OWNER}#/settings/groups`);
  await expect(page.getByText('Create a group for your team, for example everyone at @yourcompany.com')).toBeVisible();
  await page.getByRole('button', { name: 'Create group' }).click();
  await dialog(page).getByLabel('Name').fill('Madlabs team');
  await dialog(page).getByRole('button', { name: 'Create group' }).click();
  await expect(page.getByRole('heading', { name: 'Madlabs team' })).toBeVisible();
  await page.getByRole('textbox', { name: 'Add to Anyone with a verified email at…' }).fill('madlabs.tech');
  await page.getByRole('button', { name: 'Add to Anyone with a verified email at…' }).click();
  // dee is verified; eli isn't yet.
  await expect(page.getByTestId('group-preview')).toHaveText('1 person matches now: dee');
  await page.getByRole('button', { name: 'Save domains' }).click();
  await expect(page.getByText('Domains saved')).toBeVisible();
}

/** Creates workspace `name` from the workspace menu and opens its Hosts page in-app. */
async function new_workspace_hosts(page: Page, name: string) {
  await sidebar(page).getByRole('button', { name: /^Workspace: / }).click();
  await sidebar(page).getByRole('menu').getByRole('menuitem', { name: 'New workspace' }).click();
  await dialog(page).getByLabel('Name').fill(name);
  await dialog(page).getByRole('button', { name: 'Create workspace' }).click();
  await expect(sidebar(page).getByRole('button', { name: `Workspace: ${name}` })).toBeVisible();
  await sidebar(page).getByRole('link', { name: 'Hosts' }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Hosts');
}

async function share(page: Page, host: string) {
  await page.getByRole('button', { name: `Share ${host}` }).first().click();
  await dialog(page).getByText('Madlabs team', { exact: true }).click();
}

test('the owner creates a group by domain and by hand, and the list shows it', async ({ page }) => {
  await page.goto(`${OWNER}#/tasks`);
  await create_team(page);
  await expect(page.getByText('Only verified emails count.')).toBeVisible();

  const members = page.getByRole('region', { name: 'Members' });
  await expect(members.getByRole('article', { name: 'dee' })).toContainText('Domain');
  await expect(members.getByRole('article', { name: 'dee' })).toContainText('remove the domain to remove them');
  await expect(members.getByRole('article', { name: 'dee' }).getByRole('button', { name: /^Remove/ })).toHaveCount(0);

  await page.getByLabel('Add people by hand').fill('ana');
  await page.getByRole('button', { name: 'Add ana' }).click();
  await expect(members.getByRole('article', { name: 'ana' })).toContainText('Manual');
  await expect(page.getByRole('region', { name: 'Used by' })).toContainText('Nothing uses it yet.');

  await page.getByRole('link', { name: 'All groups' }).click();
  const row = page.getByRole('article', { name: 'Madlabs team' });
  await expect(row).toContainText('2 members');
  await expect(row).toContainText('@madlabs.tech');

  // A domain that isn't one says so before it is saved.
  await row.getByRole('link', { name: 'Madlabs team' }).click();
  await page.getByRole('textbox', { name: 'Add to Anyone with a verified email at…' }).fill('not a domain');
  await page.getByRole('button', { name: 'Add to Anyone with a verified email at…' }).click();
  await expect(page.getByRole('alert')).toContainText('is not an email domain');
  await expect(page.getByRole('button', { name: 'Save domains' })).toBeDisabled();
});

test('the owner shares hosts (the kernel host after a confirm) and limits terminals to the group', async ({ page }) => {
  await create_team(page);

  await page.goto(`${OWNER}#/hosts`);
  await share(page, 'build-box-01');
  await expect(dialog(page).getByRole('alert')).toHaveCount(0);
  await dialog(page).getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('build-box-01 sharing saved')).toBeVisible();

  await share(page, 'hery-mbp');
  await expect(dialog(page).getByRole('alert')).toContainText('SUN_MASTER_KEY');
  await dialog(page).getByRole('button', { name: 'Save' }).click();
  await expect(dialog(page)).toContainText('Tick the box to share the kernel’s own host.');
  await dialog(page).getByText('I understand: its sessions can read SUN_MASTER_KEY').click();
  await dialog(page).getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('hery-mbp sharing saved')).toBeVisible();

  await page.goto(`${OWNER}#/settings/terminal`);
  await expect(page.getByText('You (deployment owner) always have access.')).toBeVisible();
  await page.getByText('Only members of these groups').click();
  await page.getByRole('group', { name: 'Groups whose members may' }).getByText('Madlabs team').click();
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Terminal access saved')).toBeVisible();

  // The group says what uses it, and deleting it warns with the same list.
  await page.goto(`${OWNER}#/settings/groups`);
  await page.getByRole('link', { name: 'Madlabs team' }).click();
  const used = page.getByRole('region', { name: 'Used by' });
  await expect(used).toContainText('Shared host build-box-01');
  await expect(used).toContainText('Shared host hery-mbp');
  await expect(used).toContainText('Terminal access');
  await page.getByRole('button', { name: 'Delete group' }).click();
  await expect(dialog(page)).toContainText('This also removes:');
  await expect(dialog(page)).toContainText('Shared host build-box-01');
});

test('a @madlabs.tech member attaches the shared host and sees its runtimes', async ({ page }) => {
  await create_team(page);
  await page.goto(`${OWNER}#/hosts`);
  await share(page, 'build-box-01');
  await dialog(page).getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('build-box-01 sharing saved')).toBeVisible();

  await page.goto(`${TEAM_MEMBER}#/tasks`);
  await new_workspace_hosts(page, 'Lab');
  const others = page.getByRole('region', { name: 'Hosts you can attach' });
  const shared = others.getByRole('article', { name: 'build-box-01' });
  await expect(shared).toContainText('Shared by Madlabs team');
  await expect(others.getByRole('article', { name: 'hery-mbp' })).toHaveCount(0);
  await shared.getByRole('button', { name: 'Attach to this workspace' }).click();
  await expect(page.getByRole('status').filter({ hasText: 'is attached' })).toContainText('build-box-01 is attached');

  await page.getByRole('link', { name: 'Runtimes', exact: true }).last().click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Runtimes');
  await expect(page.getByRole('main')).toContainText('build-box-01');
  await expect(page.getByRole('main')).not.toContainText('hery-mbp');
});

test('a member outside the group sees neither the shared host nor the terminal', async ({ page }) => {
  await create_team(page);
  await page.goto(`${OWNER}#/hosts`);
  await share(page, 'build-box-01');
  await dialog(page).getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('build-box-01 sharing saved')).toBeVisible();
  await page.goto(`${OWNER}#/settings/terminal`);
  await page.getByText('Only members of these groups').click();
  await page.getByRole('group', { name: 'Groups whose members may' }).getByText('Madlabs team').click();
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Terminal access saved')).toBeVisible();
  // Everyone holds the Terminal grant; the policy still leaves them out.
  await page.evaluate(() =>
    localStorage.setItem('sun_mock_editor_grants', JSON.stringify([
      { id: 'g1', workspace_id: '00000000000000000000000001', repo_id: null, subject_type: 'all', capability: 'editor.read' },
      { id: 'g2', workspace_id: '00000000000000000000000001', repo_id: null, subject_type: 'all', capability: 'terminal' },
    ])),
  );

  await page.goto(`${PUBLIC_MEMBER}#/editor/${API_REPO}`);
  await expect(page.getByRole('note')).toContainText('Terminal access is limited to certain groups. Ask a deployment owner.');
  await expect(page.getByRole('button', { name: 'Terminal' })).toHaveCount(0);

  await page.goto(`${PUBLIC_MEMBER}#/tasks`);
  await new_workspace_hosts(page, 'Lab');
  const others = page.getByRole('region', { name: 'Hosts you can attach' });
  await expect(others.getByRole('article', { name: 'rina-laptop' })).toBeVisible();
  await expect(others.getByRole('article', { name: 'build-box-01' })).toHaveCount(0);

  // Someone in the group gets it.
  await page.goto(`${TEAM_MEMBER}#/editor/${API_REPO}`);
  await expect(page.getByRole('button', { name: 'Terminal' })).toBeVisible();
});

test('Editor access grants to a group', async ({ page }) => {
  await create_team(page);
  await page.goto(`${OWNER}#/settings/editor`);
  await page.getByLabel('Who', { exact: true }).selectOption('group');
  await page.getByLabel('Group', { exact: true }).selectOption({ label: 'Madlabs team' });
  await page.getByRole('button', { name: 'Grant' }).click();
  await expect(page.getByRole('article', { name: 'Read for Madlabs team' })).toBeVisible();
});
