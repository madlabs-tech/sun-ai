//! The repo editor's boundaries through REST, against the real `sun-host`:
//! no path leaves the caller's worktree (and a refused one leaves the disk
//! as it was), two members never share or reach each other's worktree, the
//! write precondition holds when the file changed or vanished, and every
//! route is closed to anyone but the owner (until editor grants land).
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::WorkspaceId;
use sun_core::model::Role;

use crate::common::{self, Kernel, SECRET, TOKEN, kernel};
use crate::workspaces::{member_token, member_token_in};

/// A kernel with the real host attached and a repo whose `main` holds
/// `README.md`, `src/lib.rs`, and two committed symlinks out of the repo:
/// `escape` (a directory) and `escape.txt` (a file), both in `outside`.
struct Editor {
    k: Kernel,
    base: String,
    outside: PathBuf,
    host_dir: PathBuf,
    _dir: tempfile::TempDir,
}

impl Editor {
    async fn start() -> Self {
        let k = kernel().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/host", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, k.app.clone()).into_future());

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        let outside = outside.canonicalize().unwrap();

        let origin = dir.path().join("origin");
        std::fs::create_dir_all(origin.join("src")).unwrap();
        std::fs::write(origin.join("README.md"), "hi\n").unwrap();
        std::fs::write(origin.join("src/lib.rs"), "fn main() {}\n").unwrap();
        std::os::unix::fs::symlink(&outside, origin.join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), origin.join("escape.txt")).unwrap();
        git(&origin, &["init", "-q", "-b", "main"]);
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-q", "-m", "base"]);

        let host_dir = dir.path().join("host");
        let host = sun_host::Host::open(sun_host::HostConfig::new(&host_dir), vec![]).unwrap();
        let key: String = Sha256::digest(format!("{SECRET}:{}", k.host).as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        tokio::spawn(host.run(sun_host::Credentials {
            kernel_url: url,
            host_id: k.host,
            host_key: key,
        }));
        let repo = common::local_repo(&k, WorkspaceId::MAIN, origin.to_str().unwrap()).await;
        let base = format!(
            "/api/v1/workspaces/{}/repos/{repo}/editor",
            WorkspaceId::MAIN
        );
        Self {
            k,
            base,
            outside,
            host_dir,
            _dir: dir,
        }
    }

    /// `…/editor/<route>?host_id=&path=<path>` (`path` already URL-encoded).
    fn at(&self, route: &str, path: &str) -> String {
        format!("{}/{route}?host_id={}&path={path}", self.base, self.k.host)
    }

    async fn call(&self, token: &str, method: &str, uri: &str, body: Body) -> (StatusCode, Value) {
        self.send(token, method, uri, body, None).await
    }

    async fn send(
        &self,
        token: &str,
        method: &str,
        uri: &str,
        body: Body,
        if_match: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        request = match if_match {
            Some(hash) => request
                .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                .header(header::IF_MATCH, format!("\"{hash}\"")),
            None => request.header(header::CONTENT_TYPE, "application/json"),
        };
        let response = common::respond(&self.k.app, request.body(body).unwrap()).await;
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Opens `token`'s worktree, waiting for the host to dial in → its root.
    async fn open(&self, token: &str) -> PathBuf {
        for _ in 0..500 {
            let (status, body) = self
                .call(token, "POST", &self.at("open", ""), Body::empty())
                .await;
            if status == StatusCode::OK {
                return PathBuf::from(body["root"].as_str().unwrap());
            }
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the host never served editor.open");
    }
}

fn json_body(value: Value) -> Body {
    Body::from(value.to_string())
}

fn git(cwd: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(ok.success(), "git {args:?}");
}

/// Every entry under `dir` (symlinks not followed) → its content, `<dir>`
/// or `-> target`: what "the disk is untouched" compares.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for item in std::fs::read_dir(&next).unwrap() {
            let path = item.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let what = if meta.file_type().is_symlink() {
                format!("-> {}", std::fs::read_link(&path).unwrap().display())
            } else if meta.is_dir() {
                stack.push(path.clone());
                "<dir>".to_owned()
            } else {
                String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned()
            };
            out.insert(path.strip_prefix(dir).unwrap().to_path_buf(), what);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn no_path_leads_out_of_the_worktree_and_a_refused_one_writes_nothing() {
    let e = Editor::start().await;
    let owner = member_token(&e.k.store, "olive", Role::Owner).await;
    let root = e.open(&owner).await;
    assert!(
        root.join("escape").is_symlink(),
        "fixture: symlink checked out"
    );
    let worktree_before = snapshot(&root);
    let outside_before = snapshot(&e.outside);
    let host_before = snapshot(&e.host_dir.join("editor"));

    // Paths no call may take: `..` (plain and URL-encoded), absolute (the
    // host's and the worktree's own), `.git` in any case and at any depth,
    // and through the committed symlinks out of the repo.
    let bad = [
        "..",
        "../README.md",
        "src%2F..%2F..%2FREADME.md",
        "%2e%2e%2F%2e%2e%2Fsecret.txt",
        "..%2F..%2F..%2Foutside",
        "%2Fetc%2Fpasswd",
        &urlencode(&root.join("README.md")),
        &urlencode(&e.outside.join("secret.txt")),
        ".git%2Fconfig",
        ".GIT%2Fconfig",
        ".Git%2Fhooks%2Fpre-commit",
        "src%2F.gIt%2Fconfig",
        "escape.txt",
        "escape%2Fsecret.txt",
    ];
    for path in bad {
        // Reads.
        for route in ["tree", "file"] {
            let (status, body) = e
                .call(&owner, "GET", &e.at(route, path), Body::empty())
                .await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "GET {route} {path}: {body}"
            );
        }
        // Writes, conditional or not.
        for if_match in [None, Some("0".repeat(64))] {
            let (status, body) = e
                .send(
                    &owner,
                    "PUT",
                    &e.at("file", path),
                    Body::from("pwned\n"),
                    if_match.as_deref(),
                )
                .await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "PUT {path} ({if_match:?}): {body}"
            );
        }
        for kind in ["file", "dir"] {
            let (status, body) = e
                .call(
                    &owner,
                    "POST",
                    &e.at("file", path),
                    json_body(json!({ "kind": kind })),
                )
                .await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "POST {kind} {path}: {body}"
            );
        }
    }

    // Deletes, and renames from or to each (decoded: rename takes JSON).
    let decoded = [
        "..",
        "../README.md",
        "src/../../README.md",
        "../../../outside",
        "/etc/passwd",
        root.join("README.md").to_str().unwrap(),
        e.outside.join("moved.txt").to_str().unwrap(),
        ".git/config",
        ".GIT/HEAD",
        ".Git/hooks/pre-commit",
        "src/.gIt/config",
        "escape/secret.txt",
        "escape/moved.txt",
        "",
    ]
    .map(str::to_owned);
    for path in &decoded {
        let (status, body) = e
            .call(
                &owner,
                "DELETE",
                &e.at("file", &urlencode_str(path)),
                Body::empty(),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "DELETE {path:?}: {body}"
        );
        let to = json_body(json!({ "from": "README.md", "to": path }));
        let (status, body) = e.call(&owner, "POST", &e.at("rename", ""), to).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "rename README.md -> {path:?}: {body}"
        );
        let from = json_body(json!({ "from": path, "to": "stolen.txt" }));
        let (status, body) = e.call(&owner, "POST", &e.at("rename", ""), from).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "rename {path:?} -> stolen.txt: {body}"
        );
    }

    assert_eq!(snapshot(&root), worktree_before, "the worktree changed");
    assert_eq!(
        snapshot(&e.outside),
        outside_before,
        "outside the repo changed"
    );
    assert_eq!(
        snapshot(&e.host_dir.join("editor")),
        host_before,
        "another worktree changed"
    );
    assert_eq!(
        std::fs::read_to_string(e.outside.join("secret.txt")).unwrap(),
        "secret\n"
    );
}

// BUG(BLO-186): `existing` canonicalizes before the root check, so a
// missing path behind a symlink out of the repo is 404 and an existing one
// is 422: an oracle for which files exist anywhere on the host.
#[tokio::test(flavor = "multi_thread")]
async fn a_symlink_out_does_not_reveal_what_exists_outside() {
    let e = Editor::start().await;
    let owner = member_token(&e.k.store, "olive", Role::Owner).await;
    e.open(&owner).await;
    for (method, route) in [("GET", "file"), ("GET", "tree"), ("PUT", "file")] {
        let mut seen = Vec::new();
        for path in [
            "escape%2Fsecret.txt",
            "escape%2Fmissing",
            "escape%2Fmissing%2Fnew.txt",
        ] {
            let (status, body) = e
                .call(&owner, method, &e.at(route, path), Body::from("x"))
                .await;
            seen.push((path, status, body["error"]["code"].clone()));
        }
        for (path, status, code) in &seen {
            assert_eq!(
                (*status, code),
                (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_input")),
                "{method} {route} {path}: every path behind `escape` must look the same: {seen:?}"
            );
        }
    }
}

// BUG(BLO-186): a NUL can never name a file, but a conditional save of one
// answers 409 `conflict` (as if the file was deleted) instead of 422.
#[tokio::test(flavor = "multi_thread")]
async fn a_nul_in_a_path_is_invalid_input_for_every_call() {
    let e = Editor::start().await;
    let owner = member_token(&e.k.store, "olive", Role::Owner).await;
    let root = e.open(&owner).await;
    let before = snapshot(&root);
    let hash = "0".repeat(64);
    for if_match in [None, Some(hash.as_str())] {
        let (status, body) = e
            .send(
                &owner,
                "PUT",
                &e.at("file", "README.md%00"),
                Body::from("x"),
                if_match,
            )
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "PUT ({if_match:?}): {body}"
        );
    }
    for (method, route, body) in [
        ("GET", "file", Body::empty()),
        ("GET", "tree", Body::empty()),
        ("DELETE", "file", Body::empty()),
        ("POST", "file", json_body(json!({"kind": "file"}))),
    ] {
        let (status, reply) = e
            .call(&owner, method, &e.at(route, "README.md%00"), body)
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{method} {route}: {reply}"
        );
    }
    assert_eq!(snapshot(&root), before);
}

/// Percent-encodes every byte but unreserved ones, for a `?path=`.
fn urlencode_str(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn urlencode(path: &Path) -> String {
    urlencode_str(path.to_str().unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn two_members_get_separate_worktrees_and_neither_reaches_the_other() {
    let e = Editor::start().await;
    let olive = member_token(&e.k.store, "olive", Role::Owner).await;
    let otto = member_token(&e.k.store, "otto", Role::Owner).await;
    let olive_root = e.open(&olive).await;
    let otto_root = e.open(&otto).await;

    // `editor/<repo>/<member>`: one repo directory, a member directory each.
    let editor = e.host_dir.join("editor").canonicalize().unwrap();
    assert_ne!(olive_root, otto_root);
    assert_eq!(olive_root.parent(), otto_root.parent());
    assert_eq!(
        olive_root.parent().unwrap().parent(),
        Some(editor.as_path())
    );
    let repo_id = e.base.rsplit('/').nth(1).unwrap();
    assert_eq!(
        olive_root.parent().unwrap().file_name().unwrap(),
        repo_id,
        "worktrees sit under the repo id"
    );
    // Opening again is the same worktree, not a fresh clone.
    assert_eq!(e.open(&olive).await, olive_root);

    // Olive's save is in Olive's worktree only.
    let (status, file) = e
        .call(&olive, "GET", &e.at("file", "README.md"), Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK, "{file}");
    let original_hash = file["hash"].as_str().unwrap().to_owned();
    let (status, body) = e
        .send(
            &olive,
            "PUT",
            &e.at("file", "README.md"),
            Body::from("olive was here\n"),
            Some(&original_hash),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = e
        .call(
            &olive,
            "POST",
            &e.at("file", "olive.txt"),
            json_body(json!({"kind": "file"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, theirs) = e
        .call(&otto, "GET", &e.at("file", "README.md"), Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK, "{theirs}");
    assert_eq!(theirs["content"], "hi\n");
    assert_eq!(theirs["hash"], original_hash.as_str());
    let (status, _) = e
        .call(&otto, "GET", &e.at("file", "olive.txt"), Body::empty())
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, index) = e
        .call(&otto, "GET", &e.at("index", ""), Body::empty())
        .await;
    assert!(
        !index["paths"]
            .as_array()
            .unwrap()
            .contains(&json!("olive.txt")),
        "{index}"
    );

    // Otto can't name Olive's worktree: relative climbs and absolute paths
    // are refused, for reads and writes, and Olive's files are untouched.
    let olive_member = olive_root.file_name().unwrap().to_str().unwrap();
    let olive_before = snapshot(&olive_root);
    let reach = [
        format!("../{olive_member}/README.md"),
        format!("../../{repo_id}/{olive_member}/README.md"),
        olive_root.join("README.md").to_str().unwrap().to_owned(),
    ];
    for path in &reach {
        let q = urlencode_str(path);
        let (status, body) = e.call(&otto, "GET", &e.at("file", &q), Body::empty()).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "GET {path}: {body}"
        );
        let (status, body) = e
            .send(&otto, "PUT", &e.at("file", &q), Body::from("otto\n"), None)
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "PUT {path}: {body}"
        );
        let (status, body) = e
            .call(&otto, "DELETE", &e.at("file", &q), Body::empty())
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "DELETE {path}: {body}"
        );
        let into = json_body(json!({"from": "README.md", "to": path}));
        let (status, body) = e.call(&otto, "POST", &e.at("rename", ""), into).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "rename -> {path}: {body}"
        );
    }
    let (status, body) = e
        .call(
            &otto,
            "GET",
            &e.at("tree", &urlencode_str(&format!("../{olive_member}"))),
            Body::empty(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(snapshot(&olive_root), olive_before);
    assert_eq!(
        std::fs::read_to_string(otto_root.join("README.md")).unwrap(),
        "hi\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_or_vanished_version_is_a_conflict_and_oversize_is_413() {
    let e = Editor::start().await;
    let owner = member_token(&e.k.store, "olive", Role::Owner).await;
    let root = e.open(&owner).await;

    // Changed on disk since it was read (another tab, a terminal, git).
    let (_, file) = e
        .call(&owner, "GET", &e.at("file", "README.md"), Body::empty())
        .await;
    let hash = file["hash"].as_str().unwrap().to_owned();
    std::fs::write(root.join("README.md"), "from the terminal\n").unwrap();
    let (status, conflict) = e
        .send(
            &owner,
            "PUT",
            &e.at("file", "README.md"),
            Body::from("from the editor\n"),
            Some(&hash),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "conflict");
    let expected: String = Sha256::digest(b"from the terminal\n")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(conflict["current"]["hash"], expected.as_str());
    assert_eq!(conflict["current"]["size"], 18);
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        "from the terminal\n"
    );

    // Deleted since it was read: a conflict with no current version, and
    // the save does not bring it back.
    let (_, file) = e
        .call(&owner, "GET", &e.at("file", "src%2Flib.rs"), Body::empty())
        .await;
    let hash = file["hash"].as_str().unwrap().to_owned();
    std::fs::remove_file(root.join("src/lib.rs")).unwrap();
    let (status, conflict) = e
        .send(
            &owner,
            "PUT",
            &e.at("file", "src%2Flib.rs"),
            Body::from("fn main() { revived() }\n"),
            Some(&hash),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "conflict");
    assert!(conflict["current"].is_null(), "{conflict}");
    assert!(!root.join("src/lib.rs").exists());

    // Exactly 2 MiB saves; one byte more is 413 and writes nothing, with
    // or without `If-Match`.
    let max = sun_core::host_protocol::MAX_EDITOR_FILE as usize;
    let (status, saved) = e
        .send(
            &owner,
            "PUT",
            &e.at("file", "max.txt"),
            Body::from("a".repeat(max)),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["size"], max);
    let max_hash = saved["hash"].as_str().unwrap().to_owned();
    for if_match in [None, Some(max_hash.as_str())] {
        let (status, body) = e
            .send(
                &owner,
                "PUT",
                &e.at("file", "max.txt"),
                Body::from("b".repeat(max + 1)),
                if_match,
            )
            .await;
        assert_eq!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{if_match:?}: {body}"
        );
        let (status, body) = e
            .send(
                &owner,
                "PUT",
                &e.at("file", "new-big.txt"),
                Body::from("b".repeat(max + 1)),
                if_match,
            )
            .await;
        assert_eq!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{if_match:?}: {body}"
        );
    }
    let on_disk = std::fs::read(root.join("max.txt")).unwrap();
    assert_eq!(on_disk.len(), max);
    assert!(on_disk.iter().all(|b| *b == b'a'));
    assert!(!root.join("new-big.txt").exists());
}

/// A method, a URI and how to make its body (once per caller).
type Route = (&'static str, String, fn() -> Body);

#[tokio::test(flavor = "multi_thread")]
async fn every_editor_route_is_closed_to_all_but_the_owner() {
    let e = Editor::start().await;
    let owner = member_token(&e.k.store, "olive", Role::Owner).await;
    let root = e.open(&owner).await;
    let before = snapshot(&root);

    let routes: Vec<Route> = vec![
        ("POST", e.at("open", ""), Body::empty),
        ("GET", e.at("tree", ""), Body::empty),
        ("GET", e.at("index", ""), Body::empty),
        ("GET", e.at("file", "README.md"), Body::empty),
        ("PUT", e.at("file", "README.md"), || Body::from("pwned\n")),
        ("POST", e.at("file", "new.txt"), || {
            json_body(json!({"kind": "file"}))
        }),
        ("POST", e.at("rename", ""), || {
            json_body(json!({"from": "new.txt", "to": "moved.md"}))
        }),
        ("DELETE", e.at("file", "README.md"), Body::empty),
        ("POST", e.at("rename", ""), || {
            json_body(json!({"from": "moved.md", "to": "README.md"}))
        }),
    ];

    let admin = member_token(&e.k.store, "adam", Role::Admin).await;
    let member = member_token(&e.k.store, "mia", Role::Member).await;
    let viewer = member_token(&e.k.store, "vic", Role::Viewer).await;
    let stranger = member_token_in(&e.k.store, "stan", &[]).await;
    let callers = [
        ("admin", admin.as_str(), StatusCode::FORBIDDEN),
        ("member", member.as_str(), StatusCode::FORBIDDEN),
        ("viewer", viewer.as_str(), StatusCode::FORBIDDEN),
        ("deployment token", TOKEN, StatusCode::FORBIDDEN),
        ("non-member", stranger.as_str(), StatusCode::NOT_FOUND),
    ];
    for (who, token, expected) in callers {
        for (method, uri, body) in &routes {
            let (status, reply) = e.call(token, method, uri, body()).await;
            assert_eq!(status, expected, "{who}: {method} {uri}: {reply}");
            if expected == StatusCode::FORBIDDEN {
                assert_eq!(reply["error"]["code"], "forbidden", "{who}: {method} {uri}");
            }
        }
    }
    assert_eq!(
        snapshot(&root),
        before,
        "a refused caller changed the worktree"
    );
    // Nobody but the owner got a worktree.
    let members = std::fs::read_dir(root.parent().unwrap()).unwrap().count();
    assert_eq!(members, 1);

    // The same routes are open to the owner (the matrix is not just 403s).
    for (method, uri, body) in &routes {
        let (status, reply) = e.call(&owner, method, uri, body()).await;
        assert!(
            status.is_success(),
            "owner: {method} {uri}: {status} {reply}"
        );
    }
}
