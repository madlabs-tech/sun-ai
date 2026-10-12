// Groups replace roles. Admins create, rename and delete groups, pick each group's permissions (a
// resource × verb matrix) and members; the app hides what the signed-in person's groups don't allow.
import { expect, test, type Locator, type Page } from '@playwright/test';

/** Ticks a Madlabs checkbox by clicking its label (the input itself is 0×0). */
const tick = (scope: Locator, name: string) => scope.getByRole('checkbox', { name }).locator('xpath=..').click();

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test('groups: an owner creates, customizes, staffs, renames and deletes a group', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/settings/members');
  await page.getByRole('tab', { name: 'Groups' }).click();
  const region = page.getByRole('region', { name: 'Groups' });
  for (const name of ['Viewer', 'User', 'Admin']) await expect(region.getByRole('article', { name })).toBeVisible();
  // After install User (key `member`) can't touch workspaces.
  const member = region.getByRole('article', { name: 'User' });
  await expect(member.getByRole('table', { name: 'What User may do' })).toBeVisible();
  await expect(member.getByRole('checkbox', { name: 'Workspaces: create' })).not.toBeChecked();
  await expect(member.getByRole('checkbox', { name: 'Workflows: update' })).toBeChecked();
  // Workflows have no read or delete permission: those cells say so.
  await expect(member.getByRole('row', { name: /^Workflows/ }).getByRole('img', { name: 'Doesn’t apply' })).toHaveCount(2);

  await region.getByRole('button', { name: 'New group' }).click();
  let dialog = page.getByRole('dialog').filter({ hasText: 'Create group' });
  await dialog.getByRole('button', { name: 'Create group' }).click();
  await expect(dialog.getByText('Give the group a name.')).toBeVisible();
  await dialog.getByLabel('Name').fill('Admin');
  await tick(dialog, 'Workspaces: create');
  await dialog.getByRole('button', { name: 'Create group' }).click();
  await expect(dialog.getByText('A group named `Admin` already exists.')).toBeVisible();
  await dialog.getByLabel('Name').fill('Builders');
  await tick(dialog, 'Workflows: update');
  await dialog.getByRole('button', { name: 'Create group' }).click();
  await expect(page.getByText('Builders created')).toBeVisible();

  const builders = region.getByRole('article', { name: 'Builders' });
  await expect(builders).toContainText('role:builders');
  await expect(builders.getByRole('checkbox', { name: 'Workspaces: create' })).toBeChecked();
  await expect(builders.getByRole('checkbox', { name: 'Workflows: update' })).toBeChecked();
  await tick(builders, 'Workspaces: read all');
  await expect(page.getByText('Builders can see every workspace now')).toBeVisible();
  await expect(builders.getByRole('checkbox', { name: 'Workspaces: read all' })).toBeChecked();

  await builders.getByRole('button', { name: 'Edit members of Builders' }).click();
  dialog = page.getByRole('dialog').filter({ hasText: 'Save members' });
  await tick(dialog, 'ben');
  await dialog.getByRole('button', { name: 'Save members' }).click();
  await expect(page.getByText('Builders members saved')).toBeVisible();
  await expect(builders).toContainText('1 person');
  await expect(builders.getByText('ben', { exact: true })).toBeVisible();

  // Renaming keeps the key approvals use.
  await builders.getByRole('button', { name: 'Rename Builders' }).click();
  dialog = page.getByRole('dialog').filter({ hasText: 'Rename Builders' });
  await dialog.getByLabel('Name').fill('Group B');
  await dialog.getByRole('button', { name: 'Rename' }).click();
  const group_b = region.getByRole('article', { name: 'Group B' });
  await expect(group_b).toContainText('role:builders');
  await no_overflow(page);

  // People shows the new tag; the audit log has every change.
  await page.getByRole('tab', { name: 'People' }).click();
  await expect(page.getByLabel('Groups of ben', { exact: true })).toContainText('Group B');
  const settings_nav = page.getByRole('navigation', { name: 'Settings' });
  await settings_nav.getByRole('link', { name: 'Audit log' }).click();
  for (const detail of ['created as role:builders', 'members: ben', 'renamed from Builders', 'permissions: create_workspace, edit_workflow, access_all_workspaces']) {
    await expect(page.getByRole('article').filter({ hasText: detail }).first()).toBeVisible();
  }

  await settings_nav.getByRole('link', { name: 'Members' }).click();
  await page.getByRole('tab', { name: 'Groups' }).click();
  await group_b.getByRole('button', { name: 'Delete Group B' }).click();
  await page.getByRole('dialog').filter({ hasText: 'Delete Group B?' }).getByRole('button', { name: 'Delete group' }).click();
  await expect(page.getByText('Group B deleted')).toBeVisible();
  await expect(region.getByRole('article')).toHaveCount(3);
});

test('groups: 360 px fits, and controls follow the signed-in person’s permissions', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/settings/members');
  await page.getByRole('tab', { name: 'Groups' }).click();
  await expect(page.getByRole('article', { name: 'Admin' })).toBeVisible();
  // Below 640 px the matrix is one fieldset per resource, with the same checkbox names.
  const user = page.getByRole('article', { name: 'User' });
  await expect(user.getByRole('table')).toHaveCount(0);
  const members = user.getByRole('group', { name: 'Members' });
  for (const verb of ['invite', 'create with password', 'read', 'update', 'deactivate']) {
    await expect(members.getByRole('checkbox', { name: `Members: ${verb}` })).not.toBeChecked();
  }
  await tick(user.getByRole('group', { name: 'Workspaces' }), 'Workspaces: delete');
  await expect(page.getByText('User can delete workspaces now')).toBeVisible();
  await expect(user.getByRole('checkbox', { name: 'Workspaces: delete' })).toBeChecked();
  // A member write brings "Members: read" along, so the Members page can list people.
  await tick(members, 'Members: deactivate');
  await expect(page.getByText('User can deactivate members now')).toBeVisible();
  await expect(members.getByRole('checkbox', { name: 'Members: read' })).toBeChecked();
  await expect(members.getByRole('checkbox', { name: 'Members: deactivate' })).toBeChecked();
  await no_overflow(page);

  // A member (Viewer and User) has no view_members or manage_settings: no Members page, no audit log.
  await page.goto('/?mock=1&mock_role=member#/settings/members');
  await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Invite user' })).toHaveCount(0);
  await no_overflow(page);
  await page.getByRole('link', { name: 'Settings', exact: true }).click();
  const settings_nav = page.getByRole('navigation', { name: 'Settings' });
  await expect(settings_nav.getByRole('link', { name: 'Account' })).toBeVisible();
  for (const name of ['Members', 'Audit log']) await expect(settings_nav.getByRole('link', { name })).toHaveCount(0);

  // A viewer (only the Viewer group) gets no workflow, workspace or import controls.
  await page.goto('/?mock=1&mock_role=viewer#/workflows');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Workflows');
  await expect(page.getByText('A ready workflow for this role.').first()).toBeVisible();
  await expect(page.getByRole('link', { name: 'New workflow' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: /^Use / })).toHaveCount(0);
  await page.goto('/?mock=1&mock_role=viewer#/knowledge');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Knowledge');
  await expect(page.getByRole('button', { name: 'Import', exact: true })).toHaveCount(0);

  // A member edits workflows but still can't change workspaces.
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1&mock_role=member#/workflows');
  await expect(page.getByRole('link', { name: 'New workflow' })).toBeVisible();
  const sidebar = page.getByRole('complementary', { name: 'Main' });
  await sidebar.getByRole('button', { name: /^Workspace: / }).click();
  await expect(sidebar.getByRole('menuitemradio').first()).toBeVisible();
  await expect(sidebar.getByRole('menuitem')).toHaveText(['Log out']);
});
