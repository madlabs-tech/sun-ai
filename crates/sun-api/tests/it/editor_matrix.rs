//! The editor's permission matrix against the real host: every editor
//! route and the terminal connect, for each kind of caller (non-member,
//! viewer, member without a grant, with read, write or terminal, owner);
//! a revoked grant refusing the very next request; and the path cases
//! `editor_boundaries` leaves: control characters in names, double
//! encoding, backslashes and `.git` look-alikes.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::model::Role;

use crate::common::{self, kernel};
use crate::editor_grants::member_id;
use crate::terminal::{Screen, World, opened, read_until, refused, world_with};
use crate::workspaces::{call_as, member_token, member_token_in};

const GRANTS: &str = "/api/v1/workspaces/00000000000000000000000001/editor/grants";

/// Grants `capability` on every repo to member `name`; the grant's id.
async fn grant(w: &World, name: &str, capability: &str) -> String {
    let subject = member_id(&w.k.store, name).await;
    let body = json!({"subject_type": "member", "subject_id": subject, "capability": capability});
    let (status, made) = call_as(&w.k.app, common::TOKEN, "POST", GRANTS, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{name} {capability}: {made}");
    made["id"].as_str().unwrap().to_owned()
}

async fn revoke(w: &World, grant: &str) {
    let path = format!("{GRANTS}/{grant}");
    let (status, body) = call_as(&w.k.app, common::TOKEN, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

/// What one editor route needs: the capability a caller must hold.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Needs {
    Member,
    Read,
    Write,
}

/// Every editor REST route, in an order a caller who may can run them in
/// (open first, then reads, then writes that clean up after themselves).
fn routes(w: &World) -> Vec<(Needs, &'static str, String, Option<Value>)> {
    let at = |route: &str, path: &str| {
        format!("{}/editor/{route}?host_id={}&path={path}", w.base, w.k.host)
    };
    vec![
        (Needs::Member, "GET", format!("{}/editor/me", w.base), None),
        (Needs::Read, "POST", at("open", ""), None),
        (Needs::Read, "GET", at("tree", ""), None),
        (Needs::Read, "GET", at("index", ""), None),
        (Needs::Read, "GET", at("file", "README.md"), None),
        (
            Needs::Write,
            "PUT",
            at("file", "saved.txt"),
            Some(json!("text")),
        ),
        (
            Needs::Write,
            "POST",
            at("file", "made.txt"),
            Some(json!({"kind": "file"})),
        ),
        (
            Needs::Write,
            "POST",
            at("rename", ""),
            Some(json!({"from": "made.txt", "to": "moved.txt"})),
        ),
        (Needs::Write, "DELETE", at("file", "moved.txt"), None),
    ]
}

/// The status a route answers when the caller may.
fn allowed(method: &str, uri: &str) -> StatusCode {
    match method {
        "POST" if uri.contains("/editor/file") => StatusCode::CREATED,
        "DELETE" => StatusCode::NO_CONTENT,
        "POST" if uri.contains("/editor/rename") => StatusCode::NO_CONTENT,
        _ => StatusCode::OK,
    }
}

/// A caller of the matrix: how they're made, then what they hold.
struct Caller {
    who: &'static str,
    token: String,
    /// `None`: not in the workspace (every route 404).
    holds: Option<&'static [Needs]>,
    terminal: bool,
}

#[tokio::test(flavor = "multi_thread")]
async fn every_editor_route_and_the_terminal_answer_each_kind_of_caller() {
    let w = world_with(kernel().await, Duration::from_secs(1)).await;
    let store = &w.k.store;
    let owner = member_token(store, "olive", Role::Owner).await;
    // The host dials in once; every later open is immediate.
    w.open_editor(&owner).await;

    let callers = vec![
        Caller {
            who: "non-member",
            token: member_token_in(store, "nick", &[]).await,
            holds: None,
            terminal: false,
        },
        Caller {
            who: "viewer",
            token: member_token(store, "vic", Role::Viewer).await,
            holds: Some(&[Needs::Member]),
            terminal: false,
        },
        Caller {
            who: "member without a grant",
            token: member_token(store, "mo", Role::Member).await,
            holds: Some(&[Needs::Member]),
            terminal: false,
        },
        Caller {
            who: "member with read",
            token: member_token(store, "rea", Role::Member).await,
            holds: Some(&[Needs::Member, Needs::Read]),
            terminal: false,
        },
        Caller {
            who: "member with write",
            token: member_token(store, "wri", Role::Member).await,
            holds: Some(&[Needs::Member, Needs::Read, Needs::Write]),
            terminal: false,
        },
        Caller {
            who: "member with read and terminal",
            token: member_token(store, "ter", Role::Member).await,
            holds: Some(&[Needs::Member, Needs::Read]),
            terminal: true,
        },
        Caller {
            who: "admin without a grant",
            token: member_token(store, "adam", Role::Admin).await,
            holds: Some(&[Needs::Member]),
            terminal: false,
        },
        Caller {
            who: "owner",
            token: owner.clone(),
            holds: Some(&[Needs::Member, Needs::Read, Needs::Write]),
            terminal: true,
        },
        Caller {
            who: "deployment token",
            token: common::TOKEN.to_owned(),
            holds: Some(&[Needs::Member]),
            terminal: false,
        },
    ];
    grant(&w, "rea", "editor.read").await;
    grant(&w, "wri", "editor.write").await;
    grant(&w, "ter", "editor.read").await;
    grant(&w, "ter", "terminal").await;

    let mut table = Vec::new();
    for caller in &callers {
        for (needs, method, uri, body) in routes(&w) {
            let (status, reply) = call_as(&w.k.app, &caller.token, method, &uri, body).await;
            let expected = match caller.holds {
                None => StatusCode::NOT_FOUND,
                Some(holds) if holds.contains(&needs) => allowed(method, &uri),
                Some(_) => StatusCode::FORBIDDEN,
            };
            // The deployment token is no member: `me` answers, holding nothing.
            let expected = match (caller.who, needs) {
                ("deployment token", Needs::Member) => StatusCode::OK,
                _ => expected,
            };
            table.push(format!("{} {method} {uri} → {status}", caller.who));
            assert_eq!(status, expected, "{}: {method} {uri}: {reply}", caller.who);
        }

        // The terminal connect: an upgrade and a shell, or refused before
        // anything opens.
        let socket = w.connect(&caller.token, "").await;
        match (caller.holds, caller.terminal) {
            (_, true) => {
                let mut ws = socket.unwrap_or_else(|e| panic!("{}: {e}", caller.who));
                opened(&mut ws).await;
            }
            (None, _) => assert_eq!(refused(socket), StatusCode::NOT_FOUND, "{}", caller.who),
            (Some(_), false) => {
                assert_eq!(refused(socket), StatusCode::FORBIDDEN, "{}", caller.who)
            }
        }
    }
    assert_eq!(table.len(), callers.len() * routes(&w).len());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_only_grant_passes_the_capability_check_but_has_no_worktree() {
    // Through the API (the Settings page always adds read): `terminal`
    // alone. The connect is allowed; the host has no worktree to open it in.
    let w = world_with(kernel().await, Duration::from_secs(1)).await;
    let owner = member_token(&w.k.store, "olive", Role::Owner).await;
    w.open_editor(&owner).await;
    let token = member_token(&w.k.store, "tia", Role::Member).await;
    grant(&w, "tia", "terminal").await;
    let open = format!("{}/editor/open?host_id={}", w.base, w.k.host);
    let (status, _) = call_as(&w.k.app, &token, "POST", &open, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let mut ws = w.connect(&token, "").await.unwrap();
    let mut screen = Screen::default();
    read_until(&mut ws, &mut screen, |s| !s.controls.is_empty()).await;
    assert_eq!(screen.controls[0]["kind"], "error", "{:?}", screen.controls);
    assert!(
        screen.controls[0]["message"]
            .as_str()
            .unwrap()
            .contains("open the editor first"),
        "{:?}",
        screen.controls
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_grant_refuses_the_very_next_request() {
    let w = world_with(kernel().await, Duration::from_secs(1)).await;
    let owner = member_token(&w.k.store, "olive", Role::Owner).await;
    w.open_editor(&owner).await;
    let token = member_token(&w.k.store, "wri", Role::Member).await;
    let read = grant(&w, "wri", "editor.read").await;
    let write = grant(&w, "wri", "editor.write").await;
    let app = &w.k.app;
    let at = |route: &str, path: &str| {
        format!("{}/editor/{route}?host_id={}&path={path}", w.base, w.k.host)
    };
    let (status, _) = call_as(app, &token, "POST", &at("open", ""), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call_as(app, &token, "PUT", &at("file", "a.txt"), Some(json!("1"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Write revoked: the next save, create, rename and delete are 403;
    // reading still works on the read grant.
    revoke(&w, &write).await;
    let writes = [
        ("PUT", at("file", "a.txt"), Some(json!("2"))),
        ("POST", at("file", "b.txt"), Some(json!({"kind": "file"}))),
        (
            "POST",
            at("rename", ""),
            Some(json!({"from": "a.txt", "to": "c.txt"})),
        ),
        ("DELETE", at("file", "a.txt"), None),
    ];
    for (method, uri, body) in writes {
        let (status, reply) = call_as(app, &token, method, &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {reply}");
    }
    let (status, file) = call_as(app, &token, "GET", &at("file", "a.txt"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(file["content"], "\"1\"", "the refused save wrote nothing");

    // Read revoked too: the next read is 403.
    revoke(&w, &read).await;
    for route in ["tree", "index", "file"] {
        let (status, reply) = call_as(app, &token, "GET", &at(route, "a.txt"), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{route}: {reply}");
    }

    // A grant to the member's role works the same way.
    let body =
        json!({"subject_type": "role", "subject_id": "member", "capability": "editor.write"});
    let (status, made) = call_as(app, common::TOKEN, "POST", GRANTS, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, _) = call_as(app, &token, "PUT", &at("file", "a.txt"), Some(json!("3"))).await;
    assert_eq!(status, StatusCode::OK);
    revoke(&w, made["id"].as_str().unwrap()).await;
    let (status, _) = call_as(app, &token, "PUT", &at("file", "a.txt"), Some(json!("4"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Percent-encodes every byte but unreserved ones.
fn encode(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn odd_names_stay_inside_the_worktree_and_git_stays_out_of_reach() {
    let w = world_with(kernel().await, Duration::from_secs(1)).await;
    let owner = member_token(&w.k.store, "olive", Role::Owner).await;
    w.open_editor(&owner).await;
    let app = &w.k.app;
    let at = |route: &str, path: &str| {
        format!(
            "{}/editor/{route}?host_id={}&path={}",
            w.base,
            w.k.host,
            encode(path)
        )
    };
    let (_, opened) = call_as(app, &owner, "POST", &at("open", ""), None).await;
    let root = std::path::PathBuf::from(opened["root"].as_str().unwrap());
    let editor_dir = root.parent().unwrap().parent().unwrap().to_path_buf();
    let git_config = std::fs::read(root.join(".git")).ok();

    // A newline, a CR or a tab is a legal name on disk: the file is made,
    // written, listed, indexed, renamed and deleted inside the worktree.
    for name in ["line\nbreak.txt", "carriage\rreturn.txt", "tab\tbed.txt"] {
        let (status, body) = call_as(
            app,
            &owner,
            "POST",
            &at("file", name),
            Some(json!({"kind": "file"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{name:?}: {body}");
        assert!(root.join(name).is_file(), "{name:?} made inside the root");
        let (status, _) = call_as(app, &owner, "PUT", &at("file", name), Some(json!("x"))).await;
        assert_eq!(status, StatusCode::OK, "{name:?}");
        let (_, tree) = call_as(app, &owner, "GET", &at("tree", ""), None).await;
        assert!(
            tree["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == name),
            "{name:?} listed"
        );
        let (_, index) = call_as(app, &owner, "GET", &at("index", ""), None).await;
        assert!(
            index["paths"].as_array().unwrap().contains(&json!(name)),
            "{name:?} indexed: {index}"
        );
        let moved = json!({"from": name, "to": format!("{name}.moved")});
        let (status, _) = call_as(app, &owner, "POST", &at("rename", ""), Some(moved)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{name:?}");
        let (status, _) = call_as(
            app,
            &owner,
            "DELETE",
            &at("file", &format!("{name}.moved")),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{name:?}");
        assert!(!root.join(name).exists() && !root.join(format!("{name}.moved")).exists());
    }

    // Double-encoded `..` and backslashes are names, not climbs: anything
    // made lands inside the worktree, never beside it.
    for name in [
        "%2e%2e%2fescaped.txt",
        "..\\..\\escaped.txt",
        "\u{2024}\u{2024}/x.txt",
    ] {
        let (status, _) = call_as(app, &owner, "PUT", &at("file", name), Some(json!("x"))).await;
        assert!(
            status == StatusCode::OK
                || status == StatusCode::NOT_FOUND
                || status == StatusCode::UNPROCESSABLE_ENTITY,
            "{name:?}: {status}"
        );
    }
    let siblings: Vec<_> = std::fs::read_dir(root.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(siblings.len(), 1, "only the owner's worktree: {siblings:?}");
    assert!(!editor_dir.join("escaped.txt").exists());
    assert!(!root.parent().unwrap().join("escaped.txt").exists());

    // `.git` by every spelling the host normalizes: refused for every write.
    for path in [
        ".git/config",
        "./.git/config",
        ".//.git/HEAD",
        "src/../.git/config",
        ".GIT/hooks/pre-commit",
    ] {
        let (status, body) =
            call_as(app, &owner, "PUT", &at("file", path), Some(json!("pwned"))).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "PUT {path}: {body}"
        );
        let make = Some(json!({"kind": "file"}));
        let (status, body) = call_as(app, &owner, "POST", &at("file", path), make).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "POST {path}: {body}"
        );
        let into = json!({"from": "README.md", "to": path});
        let (status, body) = call_as(app, &owner, "POST", &at("rename", ""), Some(into)).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "rename -> {path}: {body}"
        );
    }
    // Names that only look like `.git` are ordinary files.
    for name in [".gitignore", ".git.bak", ".gitx"] {
        let (status, body) = call_as(app, &owner, "PUT", &at("file", name), Some(json!("x"))).await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
    }
    assert_eq!(
        std::fs::read(root.join(".git")).ok(),
        git_config,
        "the worktree's .git changed"
    );
    assert!(root.join("README.md").is_file(), "README.md was moved");
}
