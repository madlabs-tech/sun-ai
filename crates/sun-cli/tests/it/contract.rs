//! The real `sun` binary's kernel contract (Sun Cloud's driver) and the
//! operator commands: `serve --port 0` + `<data_dir>/address`,
//! `GET /healthz`, `status --json` (with where plans come from), Main's
//! plan, the secrets API (no value ever leaves), `secret` and
//! `ecosystem export|import`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const SECRET: &str = "s3cret-value-xyz";

fn sun(data_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sun"));
    command
        .arg("--data-dir")
        .arg(data_dir)
        .env_remove("SUN_CLOUD_URL")
        .env_remove("SUN_CLOUD_TOKEN")
        .env_remove("SUN_MASTER_KEY")
        .env_remove("SUN_DATABASE_URL");
    command
}

fn run(data_dir: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = sun(data_dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn ok(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// Kills the kernel even when an assert fails.
struct Kernel(std::process::Child);

impl Drop for Kernel {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn kernel_contract_secrets_and_ecosystem_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("a");
    std::fs::create_dir_all(&data).unwrap();

    // serve --port 0: the bound address lands in <data_dir>/address.
    let kernel = Kernel(
        sun(&data)
            .args(["serve", "--port", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let address = loop {
        if let Ok(a) = std::fs::read_to_string(data.join("address")) {
            break a;
        }
        assert!(Instant::now() < deadline, "no address file");
        std::thread::sleep(Duration::from_millis(50));
    };
    let base = format!("http://{}", address.trim());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    assert_eq!(
        agent
            .get(format!("{base}/healthz"))
            .call()
            .unwrap()
            .status(),
        200
    );

    let status: Value = serde_json::from_str(&ok(&run(&data, &["status", "--json"], ""))).unwrap();
    assert_eq!(status["kernel"], "running");
    assert_eq!(status["url"], base);
    assert_eq!(status["entitlement"], "community", "no Sun Cloud here");
    assert!(status.get("sign_in_url").is_none(), "no login links");

    let token: Value =
        serde_json::from_str(&std::fs::read_to_string(data.join("kernel.json")).unwrap()).unwrap();
    let auth = format!("Bearer {}", token["token"].as_str().unwrap());
    let call = |method: &str, path: &str, body: Option<Value>| -> (u16, String) {
        let url = format!("{base}/api/v1{path}");
        let request = ureq::http::Request::builder()
            .method(method)
            .uri(url)
            .header("authorization", &auth)
            .header("content-type", "application/json");
        let request = request
            .body(body.map(|b| b.to_string()).unwrap_or_default())
            .unwrap();
        let mut response = agent.run(request).unwrap();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        (response.status().as_u16(), text)
    };

    // Off Sun Cloud, Main is on Community: no limits.
    let (code, body) = call("GET", "/entitlement", None);
    assert_eq!(code, 200);
    let entitlement: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (&entitlement["plan"], &entitlement["status"]),
        (&json!("community"), &json!("community"))
    );

    // Secrets: set, listed without the value, audited, deleted. A person's
    // secret is a workspace's; the ecosystem keeps the deployment's own.
    let ecosystem = "/secrets/ecosystem/00000000000000000000000000/API_TOKEN";
    let (code, body) = call("PUT", ecosystem, Some(json!({"value": SECRET})));
    assert_eq!(code, 422, "{body}");
    let path = "/secrets/workspace/00000000000000000000000001/API_TOKEN";
    let (code, body) = call("PUT", path, Some(json!({"value": SECRET})));
    assert_eq!(code, 200, "{body}");
    assert!(!body.contains(SECRET));
    let (_, body) = call("GET", "/secrets?workspace=00000000000000000000000001", None);
    assert!(
        body.contains("API_TOKEN") && !body.contains(SECRET),
        "{body}"
    );
    let (_, body) = call("GET", "/audit", None);
    assert!(
        body.contains("secret_change") && !body.contains(SECRET),
        "{body}"
    );
    assert_eq!(call("DELETE", path, None).0, 204);
    assert_eq!(call("DELETE", path, None).0, 404);
    assert_eq!(
        call("PUT", "/secrets/nope/x/A", Some(json!({"value": SECRET}))).0,
        422
    );
    drop(kernel);

    // CLI secrets: Main's by default, or a mod's.
    ok(&run(&data, &["secret", "set", "WS_KEY"], SECRET));
    ok(&run(
        &data,
        &["secret", "set", "GITHUB_TOKEN", "--mod", "github"],
        SECRET,
    ));
    let listed = ok(&run(&data, &["secret", "list"], ""));
    assert!(listed.contains("WS_KEY") && !listed.contains("GITHUB_TOKEN"));
    assert!(ok(&run(&data, &["secret", "list", "--mod", "github"], "")).contains("GITHUB_TOKEN"));

    // Ecosystem export | import into another data dir, with the key.
    let archive = tmp.path().join("w.sunpack");
    let archive = archive.to_str().unwrap();
    ok(&run(
        &data,
        &["ecosystem", "export", archive, "--include-master-key"],
        "",
    ));
    let other = tmp.path().join("b");
    ok(&run(&other, &["ecosystem", "import", archive], ""));
    assert!(ok(&run(&other, &["secret", "list"], "")).contains("WS_KEY"));
    let again = run(&other, &["ecosystem", "import", archive], "");
    assert_eq!(again.status.code(), Some(2), "a second import is refused");
}

/// Postgres support was removed: a `postgres://` `SUN_DATABASE_URL`
/// stops `sun` before it touches anything, and the URL is never printed.
#[test]
fn a_postgres_database_url_refuses_to_start() {
    let tmp = tempfile::tempdir().unwrap();
    let output = sun(tmp.path())
        .arg("migrate")
        .env("SUN_DATABASE_URL", "postgresql://sun:hunter2@db/sun")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!(
            "Sun stores its data in SQLite only; Postgres support was removed. \
             Unset SUN_DATABASE_URL to use the SQLite store in {}.",
            tmp.path().display()
        )),
        "{stderr}"
    );
    assert!(!stderr.contains("hunter2"));
    assert!(!tmp.path().join("sun.db").exists());
}

/// Every spelling of a Postgres URL is refused, on `serve` too, before
/// the data dir is even created; the password never leaks.
#[test]
fn every_postgres_url_spelling_refuses_before_creating_the_data_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("never-created");
    for url in [
        "postgres://sun:hunter2@db/sun",
        "POSTGRES://sun:hunter2@db/sun",
        "PostgreSQL://sun:hunter2@db:5432/sun?sslmode=require",
        "postgresql://sun:hunter2@%2Fvar%2Frun%2Fpostgresql/sun",
    ] {
        for args in [&["migrate"][..], &["serve", "--port", "0"], &["doctor"]] {
            let output = sun(&dir)
                .args(args)
                .env("SUN_DATABASE_URL", url)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{url} {args:?} started");
            assert!(stderr.contains("SQLite only"), "{url} {args:?}: {stderr}");
            assert!(!stderr.contains("hunter2"), "{url} {args:?}: {stderr}");
            assert!(!dir.exists(), "{url} {args:?} created the data dir");
        }
    }
}

/// A URL with surrounding whitespace (a pasted or quoted `.env` value)
/// is still a Postgres URL and must be refused, not silently ignored in
/// favour of a brand-new SQLite store.
#[test]
fn a_postgres_url_with_surrounding_whitespace_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let output = sun(tmp.path())
        .arg("migrate")
        .env("SUN_DATABASE_URL", " postgres://sun:hunter2@db/sun\n")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!tmp.path().join("sun.db").exists());
}
