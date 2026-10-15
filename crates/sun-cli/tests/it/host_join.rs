//! `sun host join` with a token minted inside a workspace: the host it
//! enrols is that workspace's, not Main's.
#![allow(clippy::unwrap_used)]

use std::process::Command;

use serde_json::json;
use sun_cli::serve::{self, ServeOptions};

#[test]
fn host_join_with_a_workspace_token_attaches_the_host_there() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let kernel = rt
        .block_on(serve::start(
            ServeOptions {
                data_dir: data.clone(),
                with_host: false,
                listen: Some("127.0.0.1:0".into()),
                sandbox_mods: false,
                sun_exe: env!("CARGO_BIN_EXE_sun").into(),
                exec_env_tools: Default::default(),
            },
            vec![],
        ))
        .unwrap();
    let base = format!("http://{}", kernel.addr);
    let client = sun_cli::client::Client::new(&base, &kernel.token);
    let ops = client
        .post("/workspaces", &json!({ "name": "Ops" }))
        .unwrap();
    let ops = ops["id"].as_str().unwrap();
    let issued = client
        .post(&format!("/workspaces/{ops}/hosts/tokens"), &json!({}))
        .unwrap();

    let host_data = dir.path().join("host");
    let out = Command::new(env!("CARGO_BIN_EXE_sun"))
        .args(["host", "join", issued["token"].as_str().unwrap()])
        // Enrol only: a test never installs a real launchd/systemd service.
        .args(["--kernel", &base, "--name", "ops-box", "--no-service"])
        .env("SUN_DATA_DIR", &host_data)
        .output()
        .unwrap();
    let said =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("enrolled as ops-box"), "{said}");
    assert!(host_data.join("host/host.json").is_file());

    let names = |workspace: &str| -> Vec<String> {
        let hosts = client
            .get(&format!("/hosts?workspace={workspace}"))
            .unwrap();
        let hosts = hosts.as_array().unwrap().iter();
        hosts
            .map(|h| h["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(names(ops), ["ops-box"]);
    assert!(
        !names("00000000000000000000000001").contains(&"ops-box".to_owned()),
        "not Main's"
    );
    // The token is spent.
    let again = Command::new(env!("CARGO_BIN_EXE_sun"))
        .args(["host", "join", issued["token"].as_str().unwrap()])
        .args(["--kernel", &base])
        .env("SUN_DATA_DIR", dir.path().join("again"))
        .output()
        .unwrap();
    assert!(!again.status.success());
}
