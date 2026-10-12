//! The permission-matrix upgrade fixture for the migration test in `acl.rs`.
#![allow(clippy::unwrap_used)]

use sun_core::model::{Group, Permission};

/// The first migration after the groups it rewrites.
pub const VERSION: i64 = 20261016090000;

/// Groups as the pre-matrix schema stored them: id, key (also the name), permissions.
pub const OLD_GROUPS: [(&str, &str, &str); 4] = [
    (
        "00000000000000000000000010",
        "keepers",
        r#"["manage_access"]"#,
    ),
    (
        "00000000000000000000000011",
        "builders",
        r#"["edit_board", "run","create_board"]"#,
    ),
    ("00000000000000000000000012", "runners", r#"["run"]"#),
    // Hand-edited JSON: spaces, both old permissions, a duplicate.
    (
        "00000000000000000000000013",
        "mixed",
        r#"[ "edit_board" , "manage_access","approve" ,"manage_access" ]"#,
    ),
];

/// The groups after the permission-matrix upgrade: the seeded three, then [`OLD_GROUPS`].
pub fn assert_split(groups: &[Group]) {
    use Permission::*;
    let by_key = |key: &str| {
        groups
            .iter()
            .find(|g| g.key == key)
            .unwrap()
            .permissions
            .clone()
    };
    let members = [
        ManageSettings,
        InviteMembers,
        CreateMembers,
        ViewMembers,
        EditMembers,
        DeactivateMembers,
        ManageGroups,
    ];
    assert_eq!(by_key("admin"), Permission::ALL, "Admin keeps everything");
    assert_eq!(by_key("keepers"), members);
    assert_eq!(
        by_key("builders"),
        [Run, CreateWorkspace, EditWorkspace, DeleteWorkspace]
    );
    assert_eq!(by_key("runners"), [Run]);
    let mixed: Vec<Permission> = Permission::ALL
        .iter()
        .copied()
        .filter(|p| members.contains(p) || [Approve, EditWorkspace, DeleteWorkspace].contains(p))
        .collect();
    assert_eq!(by_key("mixed"), mixed);
    assert!(by_key("viewer").is_empty());
}
