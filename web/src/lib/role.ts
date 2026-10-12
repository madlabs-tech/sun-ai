// Workspace roles and what each may do. A copy of `Role::permissions` and `Role::covers` in
// crates/sun-core/src/model.rs: the API exposes no role matrix, so keep this table in step with it by hand.
import type { Permission } from './api/auth';

export type Role = 'owner' | 'admin' | 'member' | 'viewer';

/** Highest first: each role covers the ones after it. */
export const ROLES: Role[] = ['owner', 'admin', 'member', 'viewer'];

export const ROLE_LABEL: Record<Role, string> = { owner: 'Owner', admin: 'Admin', member: 'Member', viewer: 'Viewer' };

/** Every permission, in the kernel's order (`Permission::ALL`). */
export const PERMISSIONS: Permission[] = [
  'run',
  'approve',
  'manage_plugins',
  'manage_extensions',
  'manage_secrets',
  'manage_runtimes',
  'manage_settings',
  'invite_members',
  'create_members',
  'view_members',
  'edit_members',
  'deactivate_members',
  'edit_workspace',
  'delete_workspace',
  'create_workflow',
  'edit_workflow',
];

const MEMBER: Permission[] = ['run', 'approve', 'create_workflow', 'edit_workflow'];

/** What `role` may do in its workspace. */
export const PERMISSIONS_OF: Record<Role, Permission[]> = {
  owner: PERMISSIONS,
  admin: PERMISSIONS,
  member: MEMBER,
  viewer: [],
};

/** True when `role` is `other` or above it: who may grant `other` (only an owner grants owner). */
export const covers = (role: Role | undefined, other: Role) => !!role && ROLES.indexOf(role) <= ROLES.indexOf(other);
