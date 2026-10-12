//! End-to-end: the malicious test mod (`tests/fixtures/evil_mod.rs`) inside
//! the OS sandbox tries undeclared files, network and secrets; all must fail
//! while what it declared keeps working. macOS: sandbox-exec;
//! Linux: the `sun-mod-sandbox` launcher (runs in CI).
#![cfg(any(target_os = "macos", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use sun_core::{ModCall, ModLink};
use sun_mod::{HostConfig, ModHost, Sandbox, parse_manifest};

/// A loopback server that answers `pong` to one line per connection.
fn pong_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let _ = (&stream).write_all(b"pong\n");
            }
        }
    });
    port
}

struct Fixture {
    host: ModHost,
    root: PathBuf,
    pong_port: u16,
}

impl Fixture {
    /// The evil mod copied into its own folder (as an installed package
    /// would be), declaring `127.0.0.1:<pong>` and secret `SUN_TEST_TOKEN`.
    async fn new(name: &str, sandboxed: bool) -> Self {
        let root = std::env::temp_dir().join(format!("sun-sandbox-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mod_dir = root.join("mod");
        std::fs::create_dir_all(&mod_dir).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_sun-evil-mod"), mod_dir.join("evil")).unwrap();
        std::fs::write(root.join("secret.txt"), "kernel secret").unwrap();
        let pong_port = pong_server();
        let yaml = format!(
            r#"
name: evil
kind: plugin
version: 0.1.0
api: ">=1.0 <2.0"
entry: {{ command: "./evil" }}
provides:
  actions: [read_file, write_file, env, connect, tunnel, exec]
permissions:
  network: ["127.0.0.1:{pong_port}"]
  secrets: [SUN_TEST_TOKEN]
  processes: [/bin/echo]
"#
        );
        let manifest = parse_manifest(&yaml).unwrap();
        let host = ModHost::new(HostConfig {
            call_timeout: Duration::from_secs(20),
            ..HostConfig::default()
        });
        if sandboxed {
            let secrets = vec![("SUN_TEST_TOKEN".into(), "granted".into())];
            let mut sandbox = Sandbox::for_mod(&manifest, &root.join("data"), secrets).unwrap();
            if cfg!(target_os = "linux") {
                sandbox.launcher = env!("CARGO_BIN_EXE_sun-mod-sandbox").into();
            }
            host.add_sandboxed(manifest, mod_dir, sandbox)
                .await
                .unwrap();
        } else {
            host.add(manifest, mod_dir).await.unwrap();
        }
        Self {
            host,
            root,
            pong_port,
        }
    }

    /// Runs a probe; returns `(ok, detail)`.
    async fn probe(&self, action: &str, params: Value) -> (bool, String) {
        let out = self
            .host
            .call(ModCall {
                mod_name: "evil".into(),
                method: action.into(),
                params,
                idem_key: None,
            })
            .await
            .unwrap_or_else(|e| panic!("{action}: {e}"));
        let detail = out["detail"].as_str().unwrap_or_default().to_owned();
        (out["ok"] == json!(true), detail)
    }

    fn path(&self, rel: &str) -> String {
        path_str(&self.root.join(rel))
    }
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_control_unsandboxed_probes_do_reach_things() {
    // Proves the probes work, so the denials below mean the sandbox held.
    let f = Fixture::new("control", false).await;
    let secret = f.path("secret.txt");
    assert_eq!(
        f.probe("read_file", json!({"path": secret})).await,
        (true, "kernel secret".into())
    );
    let addr = format!("127.0.0.1:{}", f.pong_port);
    assert!(f.probe("connect", json!({"addr": addr})).await.0);
    assert!(
        f.probe("write_file", json!({"path": f.path("pwned")}))
            .await
            .0
    );
    f.host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_blocks_undeclared_files_network_and_secrets() {
    let f = Fixture::new("evil", true).await;

    // Files: the kernel's secret, this repo and the real home are out of reach.
    for path in [
        f.path("secret.txt"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml").to_owned(),
    ] {
        let (ok, detail) = f.probe("read_file", json!({"path": path})).await;
        assert!(!ok, "read {path}: {detail}");
    }
    if let Some(home) = std::env::home_dir() {
        let (ok, detail) = f
            .probe("exec", json!({"program": "/bin/ls", "args": [home]}))
            .await;
        assert!(
            !ok || detail.trim().is_empty(),
            "listed real HOME: {detail}"
        );
    }
    let (ok, _) = f
        .probe("write_file", json!({"path": f.path("pwned")}))
        .await;
    assert!(!ok, "wrote outside its data dir");
    assert!(!f.root.join("pwned").exists());

    // Its own data dir (also its HOME) works.
    let (ok, detail) = f
        .probe("write_file", json!({"path": f.path("data/state.json")}))
        .await;
    assert!(ok, "own data dir: {detail}");
    let (ok, home) = f.probe("env", json!({"name": "HOME"})).await;
    assert!(ok && home.ends_with("data"), "HOME={home}");

    // Secrets: only the granted one; nothing from the kernel's environment,
    // neither directly nor by reading the kernel process.
    assert_eq!(
        f.probe("env", json!({"name": "SUN_TEST_TOKEN"})).await,
        (true, "granted".into())
    );
    assert!(
        !f.probe("env", json!({"name": "CARGO_MANIFEST_DIR"}))
            .await
            .0
    );
    let pid = std::process::id().to_string();
    let (_, kernel_env) = if cfg!(target_os = "macos") {
        let args = json!(["-E", "-ww", "-o", "command=", "-p", pid]);
        f.probe("exec", json!({"program": "/bin/ps", "args": args}))
            .await
    } else {
        f.probe("read_file", json!({"path": format!("/proc/{pid}/environ")}))
            .await
    };
    assert!(
        !kernel_env.contains("CARGO_MANIFEST_DIR"),
        "read the kernel's env: {kernel_env}"
    );

    // Processes: the declared one runs; anything else is blocked.
    assert_eq!(
        f.probe("exec", json!({"program": "/bin/echo", "args": ["hi"]}))
            .await,
        (true, "hi\n".into())
    );
    let (ok, detail) = f
        .probe("exec", json!({"program": "/bin/date", "args": []}))
        .await;
    assert!(!ok, "ran an undeclared process: {detail}");

    // Network: no direct connections, not even to loopback.
    let pong = format!("127.0.0.1:{}", f.pong_port);
    for addr in [pong.as_str(), "1.1.1.1:443"] {
        let (ok, detail) = f.probe("connect", json!({"addr": addr})).await;
        assert!(!ok, "direct connect to {addr}: {detail}");
    }
    // The proxy tunnels to the declared host only.
    assert_eq!(
        f.probe("tunnel", json!({"target": pong})).await,
        (true, "pong".into())
    );
    for target in [
        format!("localhost:{}", f.pong_port),
        "example.com:443".into(),
    ] {
        let (ok, detail) = f.probe("tunnel", json!({"target": target})).await;
        assert!(
            !ok && detail.contains("403"),
            "tunnel to {target}: {detail}"
        );
    }

    f.host.shutdown().await;
    let _ = std::fs::remove_dir_all(&f.root);
}

/// Node >= 22.18 strips TypeScript types natively; older nodes can't run the SDK.
fn modern_node() -> bool {
    let Ok(out) = std::process::Command::new("node").arg("--version").output() else {
        return false;
    };
    let version = String::from_utf8_lossy(&out.stdout);
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    major > 22 || (major == 22 && minor >= 18)
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_runs_the_typescript_template_under_node() {
    if !modern_node() {
        eprintln!("skipped: needs node >= 22.18 on PATH");
        return;
    }
    // The template, with its SDK import pointed at a copy inside the folder.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = std::env::temp_dir().join(format!("sun-sandbox-ts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mod_dir = root.join("mod");
    std::fs::create_dir_all(&mod_dir).unwrap();
    let template = repo.join("templates/plugin-typescript");
    let source = std::fs::read_to_string(template.join("mod.ts")).unwrap();
    let source = source.replace("from \"sun-mod-sdk\"", "from \"./sdk.ts\"");
    std::fs::write(mod_dir.join("mod.ts"), source).unwrap();
    std::fs::copy(
        repo.join("sdk/typescript/src/index.ts"),
        mod_dir.join("sdk.ts"),
    )
    .unwrap();
    std::fs::copy(template.join("package.json"), mod_dir.join("package.json")).unwrap();
    let manifest = sun_mod::load_manifest(&template).unwrap();

    let host = ModHost::new(HostConfig::default());
    let mut sandbox = Sandbox::for_mod(&manifest, &root.join("data"), vec![]).unwrap();
    if cfg!(target_os = "linux") {
        sandbox.launcher = env!("CARGO_BIN_EXE_sun-mod-sandbox").into();
    }
    host.add_sandboxed(manifest, mod_dir, sandbox)
        .await
        .unwrap();
    let out = host
        .call(ModCall {
            mod_name: "my-plugin".into(),
            method: "ping".into(),
            params: json!({}),
            idem_key: None,
        })
        .await
        .unwrap();
    assert_eq!(out, json!({ "pong": true }));
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// An entry that is a `#!/usr/bin/env node` script runs sandboxed (node is
/// its runtime; `/usr/bin/env` would be an undeclared process), and a
/// TS-SDK extension reaches its declared plugin with `kernel.call_plugin`.
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_runs_an_env_shebang_typescript_extension() {
    if !modern_node() {
        eprintln!("skipped: needs node >= 22.18 on PATH");
        return;
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = std::env::temp_dir().join(format!("sun-sandbox-shebang-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mod_dir = root.join("relay");
    std::fs::create_dir_all(&mod_dir).unwrap();
    std::fs::copy(
        repo.join("sdk/typescript/src/index.ts"),
        mod_dir.join("sdk.ts"),
    )
    .unwrap();
    std::fs::write(mod_dir.join("package.json"), r#"{"type":"module"}"#).unwrap();
    let entry = mod_dir.join("relay.ts");
    std::fs::write(
        &entry,
        "#!/usr/bin/env node\nimport { run } from \"./sdk.ts\";\n\
         await run({ call: ({ params }, kernel) => kernel.call_plugin(\"echo\", \"echo\", params) });\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let relay = parse_manifest(
        "name: relay\nkind: extension\nversion: 0.1.0\napi: '>=1.0 <2.0'\n\
         entry: { command: ./relay.ts }\nprovides: { step_types: [relay] }\n\
         permissions: { plugins: [echo] }\n",
    )
    .unwrap();
    let echo = parse_manifest(&format!(
        "name: echo\nkind: plugin\nversion: 0.1.0\napi: '>=1.0 <2.0'\n\
         entry: {{ command: \"{}\" }}\nprovides: {{ actions: [echo] }}\n",
        env!("CARGO_BIN_EXE_sun-echo-mod")
    ))
    .unwrap();

    let host = ModHost::new(HostConfig::default());
    host.add(echo, std::env::temp_dir()).await.unwrap();
    let mut sandbox = Sandbox::for_mod(&relay, &root.join("data"), vec![]).unwrap();
    if cfg!(target_os = "linux") {
        sandbox.launcher = env!("CARGO_BIN_EXE_sun-mod-sandbox").into();
    }
    host.add_sandboxed(relay, mod_dir, sandbox).await.unwrap();
    let out = host
        .call(ModCall {
            mod_name: "relay".into(),
            method: "relay".into(),
            params: json!({ "x": 1 }),
            idem_key: None,
        })
        .await
        .unwrap();
    assert_eq!(out["params"], json!({ "x": 1 }));
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}
