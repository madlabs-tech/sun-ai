//! The permission-matrix upgrade fixture for the migration test in `acl.rs`.
#![allow(clippy::unwrap_used)]

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

/// The groups after the permission-matrix upgrade (key, permissions as
/// stored, read before groups became roles): the seeded three, then
/// [`OLD_GROUPS`]. Order and duplicates are ignored, as reads normalised
/// them then.
pub fn assert_split(groups: &[(String, Vec<String>)]) {
    let by_key = |key: &str| {
        let mut permissions = groups.iter().find(|g| g.0 == key).unwrap().1.clone();
        permissions.sort();
        permissions.dedup();
        permissions
    };
    let sorted = |list: &[&str]| {
        let mut list: Vec<String> = list.iter().map(|p| (*p).to_owned()).collect();
        list.sort();
        list
    };
    let members = [
        "manage_settings",
        "invite_members",
        "create_members",
        "view_members",
        "edit_members",
        "deactivate_members",
        "manage_groups",
    ];
    assert_eq!(by_key("keepers"), sorted(&members));
    assert_eq!(
        by_key("builders"),
        sorted(&[
            "run",
            "create_workspace",
            "edit_workspace",
            "delete_workspace"
        ])
    );
    assert_eq!(by_key("runners"), ["run"]);
    let mut mixed = members.to_vec();
    mixed.extend(["approve", "edit_workspace", "delete_workspace"]);
    assert_eq!(by_key("mixed"), sorted(&mixed));
    assert!(by_key("viewer").is_empty());
    for permission in ["access_all_workspaces", "manage_groups", "delete_workspace"] {
        assert!(
            by_key("admin").iter().any(|p| p == permission),
            "{permission}"
        );
    }
}
