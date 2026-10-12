//! Catalog index, suggestions and the ecosystem store.
#![allow(clippy::unwrap_used)]

use std::fs;

use sun_catalog::{Catalog, Kind, McpServer, detect_stacks};
use sun_core::WorkspaceId;

#[test]
fn catalog_index_loads_and_every_skill_has_its_files() {
    let catalog = Catalog::open("/nonexistent").unwrap();
    let list = catalog.list(&[], &[], None).unwrap();
    assert!(list.iter().any(|l| l.entry.kind == Kind::Skill));
    assert!(list.iter().any(|l| l.entry.kind == Kind::Mcp));
    for l in list.iter().filter(|l| l.entry.kind == Kind::Skill) {
        let files = catalog.files(&l.entry.id).unwrap();
        let skill = files.iter().find(|f| f.path == "SKILL.md").unwrap();
        // Frontmatter name must equal the folder name (Codex/OpenCode require it).
        assert!(
            skill.text.contains(&format!("name: {}\n", l.entry.id)),
            "{}",
            l.entry.id
        );
        assert!(
            files.iter().any(|f| f.path.starts_with("LICENSE")),
            "{}",
            l.entry.id
        );
    }
    let github = list.iter().find(|l| l.entry.id == "github").unwrap();
    assert_eq!(github.hosts, ["api.githubcopilot.com"]);
    assert_eq!(github.secrets, ["GITHUB_TOKEN"]);
    assert!(list.iter().all(|l| l.installed.is_none() && l.reviewed));
}

#[test]
fn catalog_detects_stacks_and_ranks_suggestions() {
    let repo = tempfile::tempdir().unwrap();
    fs::write(repo.path().join("Cargo.toml"), "[package]").unwrap();
    fs::write(
        repo.path().join("package.json"),
        r#"{"dependencies":{"svelte":"5"},"devDependencies":{"typescript":"5"}}"#,
    )
    .unwrap();
    fs::create_dir(repo.path().join("App.xcodeproj")).unwrap();
    let stacks = detect_stacks(repo.path()).unwrap();
    assert_eq!(
        stacks,
        [
            "frontend",
            "ios",
            "mobile",
            "node",
            "rust",
            "svelte",
            "typescript"
        ]
    );

    let catalog = Catalog::open("/nonexistent").unwrap();
    let list = catalog.list(&stacks, &[], None).unwrap();
    assert_eq!(list[0].matches, ["frontend"]);
    let backend = catalog.list(&["go".into()], &[], None).unwrap();
    assert!(backend.iter().all(|l| l.matches.is_empty()));

    let empty = tempfile::tempdir().unwrap();
    assert!(detect_stacks(empty.path()).unwrap().is_empty());
}

#[test]
fn ranking_prefers_skills_used_by_workflows() {
    let catalog = Catalog::open("/nonexistent").unwrap();
    let ids = |stacks: &[String], in_use: &[String]| -> Vec<String> {
        let list = catalog.list(stacks, in_use, None).unwrap();
        list.into_iter().map(|l| l.entry.id).collect()
    };
    let frontend = ["frontend".to_owned()];
    // A stack match alone ranks frontend-design first...
    assert_eq!(ids(&frontend, &[])[0], "frontend-design");
    // ...but a skill a workflow uses beats it, and so does a granted server.
    let used = ["diagnosing-bugs".to_owned(), "github".to_owned()];
    assert_eq!(
        ids(&frontend, &used)[..3],
        ["diagnosing-bugs", "github", "frontend-design"]
    );
    let list = catalog.list(&[], &used, None).unwrap();
    assert!(list[0].in_workflows && list[1].in_workflows && !list[2].in_workflows);
}

#[test]
fn dismissals_are_kept_per_ecosystem() {
    let data = tempfile::tempdir().unwrap();
    let catalog = Catalog::open(data.path()).unwrap();
    assert!(catalog.dismissed().unwrap().is_empty());
    catalog.dismiss("tdd", true).unwrap();
    catalog.dismiss("bug-fix", true).unwrap(); // a workflow template id
    catalog.dismiss("tdd", true).unwrap();
    // Another kernel process on the same data dir sees them.
    let again = Catalog::open(data.path()).unwrap();
    assert_eq!(
        again.dismissed().unwrap().into_iter().collect::<Vec<_>>(),
        ["bug-fix", "tdd"]
    );
    again.dismiss("tdd", false).unwrap();
    assert_eq!(catalog.dismissed().unwrap().len(), 1);
    assert!(catalog.dismiss("../x", true).is_err());
}

#[test]
fn skill_md_needs_a_matching_name_and_a_description() {
    let ok = "---\nname: my-skill\ndescription: \"Use when: testing.\"\n---\n\n# Hi\n";
    sun_catalog::check_skill("my-skill", ok).unwrap();
    for bad in [
        "# no frontmatter\n",
        "---\nname: other\ndescription: d\n---\n",
        "---\nname: my-skill\n---\n",
        "---\nname: my-skill\ndescription: \"  \"\n---\n",
    ] {
        assert!(sun_catalog::check_skill("my-skill", bad).is_err(), "{bad}");
    }
    assert!(sun_catalog::check_skill("My_Skill", ok).is_err());
}

#[test]
fn catalog_installs_only_what_is_picked_and_uninstalls() {
    let data = tempfile::tempdir().unwrap();
    let catalog = Catalog::open(data.path()).unwrap();
    catalog.list(&["rust".into()], &[], None).unwrap();
    assert!(
        !data.path().join("skills").exists(),
        "listing installs nothing"
    );

    catalog.install("tdd", None).unwrap();
    assert!(data.path().join("skills/tdd/SKILL.md").is_file());
    assert!(data.path().join("skills/tdd/tests.md").is_file());
    catalog.install("playwright", None).unwrap();
    let server: serde_json::Value =
        serde_json::from_slice(&fs::read(data.path().join("mcp_servers/playwright.json")).unwrap())
            .unwrap();
    assert_eq!(server["name"], "playwright");

    let list = catalog.list(&[], &[], None).unwrap();
    let installed: Vec<_> = list
        .iter()
        .filter(|l| l.installed.is_some())
        .map(|l| l.entry.id.as_str())
        .collect();
    assert_eq!(installed, ["tdd", "playwright"]);

    // Diff on update: each file against the installed copy.
    assert!(
        catalog.files("diagnosing-bugs").unwrap()[0]
            .change
            .is_none()
    );
    let skill = data.path().join("skills/tdd");
    let current = fs::read_to_string(skill.join("SKILL.md")).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        current.replace("name: tdd\n", "name: tdd-old\n"),
    )
    .unwrap();
    fs::remove_file(skill.join("tests.md")).unwrap();
    fs::write(skill.join("old.md"), "gone upstream\n").unwrap();
    let files = catalog.files("tdd").unwrap();
    let change = |path: &str| files.iter().find(|f| f.path == path).unwrap().change;
    assert_eq!(change("SKILL.md"), Some("changed"));
    // The one changed line shows as -/+, the rest as context.
    let diffs: Vec<_> = files.iter().filter_map(|f| f.diff.as_ref()).collect();
    let [diff] = diffs[..] else {
        panic!("one diff: {files:?}")
    };
    assert_eq!(diff[..3], [" ---", "-name: tdd-old", "+name: tdd"]);
    assert_eq!(diff.len(), current.lines().count() + 1);
    assert_eq!(change("tests.md"), Some("added"));
    assert_eq!(change("old.md"), Some("removed"));
    assert!(
        files
            .iter()
            .filter(|f| !["SKILL.md", "tests.md", "old.md"].contains(&f.path.as_str()))
            .all(|f| f.change == Some("unchanged")),
        "{files:?}"
    );
    assert_eq!(
        catalog.files("playwright").unwrap()[0].change,
        Some("unchanged")
    );

    // Re-install (update) is idempotent; it may limit the skill's runtimes.
    let codex = ["codex".to_owned()];
    catalog.install("tdd", Some(&codex)).unwrap();
    assert!(!skill.join("old.md").exists());
    let tdd = |c: &Catalog| {
        c.list(&[], &[], None)
            .unwrap()
            .into_iter()
            .find(|l| l.entry.id == "tdd")
    };
    assert_eq!(tdd(&catalog).unwrap().runtimes, Some(codex.to_vec()));
    assert!(
        catalog.install("playwright", Some(&codex)).is_err(),
        "skills only"
    );
    for bad in [
        vec![],
        vec!["Codex".to_owned()],
        vec!["codex".into(), "codex".into()],
    ] {
        assert!(catalog.install("tdd", Some(&bad)).is_err(), "{bad:?}");
    }
    catalog.install("tdd", None).unwrap();
    assert_eq!(tdd(&catalog).unwrap().runtimes, None);
    catalog.install("tdd", Some(&codex)).unwrap();
    catalog.uninstall("tdd").unwrap();
    catalog.uninstall("playwright").unwrap();
    assert!(!data.path().join("skills/tdd").exists());
    assert_eq!(
        fs::read_to_string(data.path().join("skills/.runtimes.json")).unwrap(),
        "{}",
        "uninstall forgets the runtimes"
    );
    assert!(!data.path().join("mcp_servers/playwright.json").exists());
    assert!(catalog.uninstall("tdd").is_err());
    assert!(catalog.install("no-such-thing", None).is_err());

    // A repo-scope install reads the bundle and writes nothing here.
    let (version, files) = catalog.skill_bundle("tdd", Some(&codex)).unwrap();
    assert_eq!(version, catalog.entry("tdd").unwrap().version);
    assert!(files.iter().any(|(path, _)| path == "SKILL.md"));
    assert!(!data.path().join("skills/tdd").exists());
    assert!(
        catalog.skill_bundle("playwright", None).is_err(),
        "skills only"
    );
    assert!(catalog.skill_bundle("tdd", Some(&[])).is_err());
}

#[test]
fn custom_mcp_servers_share_the_registry_with_catalog_installs() {
    let data = tempfile::tempdir().unwrap();
    let catalog = Catalog::open(data.path()).unwrap();
    catalog.install("github", None).unwrap();
    let docs: McpServer = serde_json::from_value(serde_json::json!(
        {"name": "docs", "transport": {"type": "url", "url": "https://docs.example.com/mcp"}}
    ))
    .unwrap();
    catalog.add_mcp_server(&docs).unwrap();
    assert!(catalog.add_mcp_server(&docs).is_err(), "names are unique");
    let names: Vec<_> = catalog
        .mcp_servers()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["docs", "github"]);
    assert_eq!(catalog.mcp_server("docs").unwrap(), docs);
    catalog.remove_mcp_server("github").unwrap();
    assert!(
        catalog
            .list(&[], &[], None)
            .unwrap()
            .iter()
            .all(|l| l.installed.is_none())
    );
    catalog.remove_mcp_server("docs").unwrap();
    assert!(catalog.mcp_servers().unwrap().is_empty());
    assert!(catalog.mcp_server("docs").is_err());
}

/// A workspace has the catalog installs it made and its own custom servers;
/// one from before names no workspace and is Main's.
#[test]
fn mcp_servers_and_installs_are_per_workspace() {
    let data = tempfile::tempdir().unwrap();
    let catalog = Catalog::open(data.path()).unwrap();
    catalog.install("github", None).unwrap();
    catalog.install("tdd", None).unwrap();
    let ops: WorkspaceId = "01K0000000000000000000000W".parse().unwrap();
    let server = |name: &str, workspace: Option<WorkspaceId>| McpServer {
        workspace_id: workspace,
        ..serde_json::from_value(serde_json::json!(
            {"name": name, "transport": {"type": "url", "url": "https://docs.example.com/mcp"}}
        ))
        .unwrap()
    };
    catalog.add_mcp_server(&server("old", None)).unwrap();
    catalog
        .add_mcp_server(&server("ops-docs", Some(ops)))
        .unwrap();
    let names = |workspace, installs: &[String]| -> Vec<String> {
        let servers = catalog.mcp_servers_in(workspace, installs).unwrap();
        servers.into_iter().map(|s| s.name).collect()
    };
    let main_installs = ["github".to_owned(), "tdd".to_owned()];
    assert_eq!(names(WorkspaceId::MAIN, &main_installs), ["github", "old"]);
    assert_eq!(names(ops, &[]), ["ops-docs"]);
    assert_eq!(names(ops, &["github".to_owned()]), ["github", "ops-docs"]);
    assert!(catalog.is_catalog_install("github").unwrap());
    assert!(!catalog.is_catalog_install("ops-docs").unwrap());
    let installed = |installs: Option<&[String]>| -> Vec<String> {
        let list = catalog.list(&[], &[], installs).unwrap();
        let mut ids: Vec<String> = list
            .into_iter()
            .filter(|l| l.installed.is_some())
            .map(|l| l.entry.id)
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(installed(None), ["github", "tdd"]);
    assert_eq!(installed(Some(&["tdd".to_owned()])), ["tdd"]);
    assert!(installed(Some(&[])).is_empty());
}
