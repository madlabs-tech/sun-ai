//! `scripts/load-git-apps.sh` against a temp data dir with dummy values:
//! every key lands in the sealed store, nothing secret is printed, a
//! group-readable `.env` is refused, and running it again is safe.
#![allow(clippy::unwrap_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use sun_core::model::SecretScope;
use sun_store::{SecretRepo, Store};

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/load-git-apps.sh"
);
const KEYS: &[&str] = &[
    "GITHUB_APP_ID",
    "GITHUB_APP_SLUG",
    "GITHUB_APP_PRIVATE_KEY_PATH",
    "GITHUB_APP_WEBHOOK_SECRET",
    "GITLAB_OAUTH_CLIENT_ID",
    "GITLAB_OAUTH_CLIENT_SECRET",
    "GITLAB_OAUTH_URL",
];
const PEM: &str =
    "-----BEGIN RSA PRIVATE KEY-----\ndummy-pem-body-0123456789\n-----END RSA PRIVATE KEY-----\n";

fn load(data: &Path, env_file: &Path, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new("bash");
    command
        .arg(SCRIPT)
        .arg(env_file)
        .env("SUN", env!("CARGO_BIN_EXE_sun"))
        .env("SUN_DATA_DIR", data);
    // Only the test's own values: never the developer's environment.
    for key in KEYS {
        command.env_remove(key);
    }
    command.envs(env.iter().copied()).output().unwrap()
}

/// The failure paths print no value either, and the file is parsed, never
/// run: a `$(...)` in it stays text.
#[test]
fn load_git_apps_failures_print_no_value_and_run_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let marker = dir.path().join("ran");
    let env_file = dir.path().join(".env");
    std::fs::write(
        &env_file,
        format!(
            "GITHUB_APP_ID=$(touch {})\nGITLAB_OAUTH_CLIENT_SECRET=dummy-secret-on-failure-9\n\
             GITHUB_APP_PRIVATE_KEY_PATH={}/missing.pem\n",
            marker.display(),
            dir.path().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let printed = |out: &Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };

    // The PEM path points nowhere: refused, nothing about the values printed.
    let out = load(&data, &env_file, &[]);
    let text = printed(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("GITHUB_APP_PRIVATE_KEY_PATH"), "{text}");
    assert!(!text.contains("dummy-secret-on-failure-9"), "{text}");
    assert!(!marker.exists(), "a `$(...)` in the file ran");

    // `sun` itself fails: the key is named, its value never.
    let mut command = Command::new("bash");
    command
        .arg(SCRIPT)
        .arg(&env_file)
        .env("SUN", "false")
        .env("SUN_DATA_DIR", &data);
    for key in KEYS {
        command.env_remove(key);
    }
    let out = command.output().unwrap();
    let text = printed(&out);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("sun secret set GITHUB_APP_ID failed"),
        "{text}"
    );
    assert!(
        !text.contains("dummy-secret-on-failure-9") && !text.contains("touch"),
        "{text}"
    );
    assert!(!marker.exists(), "a `$(...)` in the file ran");
}

#[test]
fn load_git_apps_seals_each_key_and_prints_no_value() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let pem = dir.path().join("app.pem");
    std::fs::write(&pem, PEM).unwrap();
    let env_file = dir.path().join(".env");
    std::fs::write(
        &env_file,
        format!(
            "# git apps\nGITHUB_APP_ID=123456\nexport GITHUB_APP_SLUG=\"sun-dummy\"\n\
             GITHUB_APP_PRIVATE_KEY_PATH={}\nGITHUB_APP_WEBHOOK_SECRET=\n\
             GITLAB_OAUTH_CLIENT_ID='dummy-client-id'\nGITLAB_OAUTH_CLIENT_SECRET=dummy-client-secret-42\n\
             GITLAB_OAUTH_URL=https://gitlab.file.test\n",
            pem.display()
        ),
    )
    .unwrap();
    let values = [
        "123456",
        "sun-dummy",
        "dummy-pem-body",
        "dummy-client-id",
        "dummy-client-secret-42",
        "gitlab.file.test",
        "gitlab.env.test",
    ];
    let quiet = |out: &Output| {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        for value in values {
            assert!(!text.contains(value), "{value} printed: {text}");
        }
        text
    };

    // Readable by others: refused, nothing set.
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = load(&data, &env_file, &[]);
    assert!(!out.status.success());
    assert!(quiet(&out).contains("chmod 600"));

    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    // The environment wins over the file; an empty key is skipped.
    let env = [("GITLAB_OAUTH_URL", "https://gitlab.env.test")];
    for _ in 0..2 {
        let out = load(&data, &env_file, &env);
        let text = quiet(&out);
        assert!(out.status.success(), "{text}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "set GITHUB_APP_ID\nset GITHUB_APP_SLUG\nset GITHUB_APP_PRIVATE_KEY\n\
             set GITLAB_OAUTH_CLIENT_ID\nset GITLAB_OAUTH_CLIENT_SECRET\nset GITLAB_OAUTH_URL\n"
        );
    }

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let store = Store::open(&data).await.unwrap();
        let scope = SecretScope::from_parts("ecosystem", "00000000000000000000000000").unwrap();
        let mut tx = store.read().await.unwrap();
        for (name, value) in [
            ("GITHUB_APP_ID", "123456"),
            ("GITHUB_APP_SLUG", "sun-dummy"),
            ("GITHUB_APP_PRIVATE_KEY", PEM.trim_end()),
            ("GITLAB_OAUTH_CLIENT_ID", "dummy-client-id"),
            ("GITLAB_OAUTH_CLIENT_SECRET", "dummy-client-secret-42"),
            ("GITLAB_OAUTH_URL", "https://gitlab.env.test"),
        ] {
            assert_eq!(
                tx.secret_value(&scope, name).await.unwrap(),
                value,
                "{name}"
            );
        }
        assert!(
            tx.secret_value(&scope, "GITHUB_APP_WEBHOOK_SECRET")
                .await
                .is_err()
        );
    });
}
