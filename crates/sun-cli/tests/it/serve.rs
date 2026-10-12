//! End to end: `sun serve --with-host` (in process) with a fake runtime and
//! the default workflow — implement → gates → open PR → review — reaches
//! `done` through the REST API, and the web UI is served at `/`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_cli::client::Client;
use sun_cli::serve::{self, ServeOptions};
use sun_core::{
    BoxStream, HostCtx, Readiness, Result, RuntimeAdapter, SessionHandle, SessionSpec, Steer,
    StopMode, SunEvent, Transcript,
};
use sun_runtime::FakeRuntime;

/// The fake runtime, plus what a real agent does: change a file in its
/// worktree (for the `diff_not_empty` gate), then read the brief with
/// `sun.task` and finish with `sun.done` through the session's MCP config —
/// the real `sun mcp` shim dialing the kernel. The script itself never
/// claims done. `mcp` collects each session's `sun.task` answer.
struct Editing(FakeRuntime, Arc<Mutex<Vec<String>>>);

#[async_trait]
impl RuntimeAdapter for Editing {
    fn id(&self) -> &str {
        self.0.id()
    }
    async fn readiness(&self, host: &HostCtx) -> Readiness {
        self.0.readiness(host).await
    }
    async fn start(&self, spec: SessionSpec) -> Result<SessionHandle> {
        std::fs::write(
            spec.worktree.join("src/lib.rs"),
            "//! Changed by the agent.\n",
        )
        .unwrap();
        let config = spec
            .mcp_config
            .clone()
            .expect("every session gets `sun` MCP");
        let briefs = self.1.clone();
        std::thread::spawn(move || briefs.lock().unwrap().push(call_sun_mcp(&config)));
        self.0.start(spec).await
    }
    async fn send(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        self.0.send(session, msg).await
    }
    async fn stop(&self, session: &SessionHandle, how: StopMode) -> Result<()> {
        self.0.stop(session, how).await
    }
    fn events(&self, session: &SessionHandle) -> BoxStream<'static, SunEvent> {
        self.0.events(session)
    }
    async fn export(&self, session: &SessionHandle) -> Result<Transcript> {
        self.0.export(session).await
    }
    fn context_window(&self, model: &str) -> Option<u32> {
        self.0.context_window(model)
    }
}

/// Runs the session's `sun` MCP server as its config says and speaks raw
/// JSON-RPC to it: `initialize`, `sun.task`, then `sun.done` (retried while
/// the step is still `dispatched`). Returns the `sun.task` text.
fn call_sun_mcp(config: &Path) -> String {
    let file: Value = serde_json::from_str(&std::fs::read_to_string(config).unwrap()).unwrap();
    let sun = &file["mcpServers"]["sun"];
    let mut child = std::process::Command::new(sun["command"].as_str().unwrap())
        .args(
            sun["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap()),
        )
        .envs(
            sun["env"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k, v.as_str().unwrap())),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut id = 0;
    let mut rpc = |method: &str, params: Value| -> Value {
        id += 1;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(stdin, "{request}").unwrap();
        if method == "initialize" {
            let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
            writeln!(stdin, "{note}").unwrap();
        }
        loop {
            let reply: Value = serde_json::from_str(&stdout.next().unwrap().unwrap()).unwrap();
            if reply["id"] == id {
                return reply;
            }
        }
    };
    let init = json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": { "name": "e2e", "version": "0" },
    });
    assert_eq!(
        rpc("initialize", init)["result"]["serverInfo"]["name"],
        "sun"
    );
    let tool = |name: &str, arguments: Value| json!({ "name": name, "arguments": arguments });
    let task = rpc("tools/call", tool("task", json!({})));
    let brief = task["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    for _ in 0..100 {
        let done = rpc("tools/call", tool("done", json!({ "summary": "finished" })));
        if done["result"]["isError"] != true {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // EOF on stdin ends the shim.
    drop(stdin);
    child.wait().unwrap();
    brief
}

fn cargo(args: &[&str]) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let built = std::process::Command::new(cargo)
        .args(args)
        .status()
        .unwrap();
    assert!(built.success(), "cargo {args:?} failed");
}

/// `target/<profile>/<name>`, next to this test's `deps/`.
fn built(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().join(name)
}

fn git(cwd: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// A bare remote holding a tiny Rust crate, so the default `test` and `lint`
/// gates have work and the PR step has somewhere to push.
fn origin_repo(dir: &Path) -> PathBuf {
    let seed = dir.join("seed");
    std::fs::create_dir_all(seed.join("src")).unwrap();
    std::fs::write(
        seed.join("Cargo.toml"),
        "[package]\nname = \"e2e\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(seed.join("src/lib.rs"), "//! E2E.\n").unwrap();
    std::fs::write(seed.join(".gitignore"), "target/\nCargo.lock\n").unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "base"]);
    git(dir, &["clone", "-q", "--bare", "seed", "origin.git"]);
    dir.join("origin.git")
}

fn wait_for(what: &str, mut ready: impl FnMut() -> Option<Value>) -> Value {
    for _ in 0..1500 {
        if let Some(value) = ready() {
            return value;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// Installs the fixture mod as `github`, the way `sun plugin add --yes` does.
fn install_fake_github(rt: &tokio::runtime::Runtime, dir: &Path, data: &Path) {
    let package = dir.join("github-mod");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("mod.yaml"),
        format!(
            "name: github\nkind: plugin\nversion: 0.1.0\napi: \">=1.0 <2.0\"\n\
             entry: {{ command: {:?} }}\nprovides: {{ actions: [open_pr, reconcile, config] }}\n",
            env!("CARGO_BIN_EXE_sun-e2e-mod")
        ),
    )
    .unwrap();
    rt.block_on(sun_cli::mods::install(
        data,
        package.to_str().unwrap(),
        sun_mod::ModKind::Plugin,
        &[],
        true,
        false,
    ))
    .unwrap();
}

#[test]
fn installed_mods_load_with_their_install_config() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("sun.yaml"),
        "mods: {github: {config: {api_url: \"https://code.acme.com/api/v3\"}}}\n",
    )
    .unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    install_fake_github(&rt, dir.path(), &data);
    let config = sun_cli::config::Config::load(&data).unwrap();
    rt.block_on(async {
        let plugins = sun_core::model::ModKind::Plugin;
        let listed = sun_cli::mods::list(&data, plugins).await.unwrap();
        assert_eq!(listed[0].name, "github");
        assert!(listed[0].package_digest.is_some());
        let store = sun_store::Store::open(&data).await.unwrap();
        let host = sun_mod::ModHost::new(sun_mod::HostConfig::default());
        let loaded = sun_cli::mods::load_installed(&store, &data, &config, &host, false)
            .await
            .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "github");
        let call = sun_core::ModCall {
            mod_name: "github".into(),
            method: "config".into(),
            params: Value::Null,
            idem_key: None,
        };
        let got = sun_core::ModLink::call(&host, call).await.unwrap();
        assert_eq!(got, json!({ "api_url": "https://code.acme.com/api/v3" }));
        host.shutdown().await;
        drop(store);
        sun_cli::mods::remove(&data, "github", true).await.unwrap();
        assert!(
            sun_cli::mods::list(&data, plugins)
                .await
                .unwrap()
                .is_empty()
        );
    });
}

#[test]
fn serve_with_host_runs_the_default_workflow_to_done() {
    cargo(&[
        "build",
        "-q",
        "-p",
        "sun-runtime",
        "--bin",
        "sun-fake-runtime",
    ]);
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let origin = origin_repo(dir.path());
    let script = dir.path().join("script.jsonl");
    std::fs::write(
        &script,
        // The agent finishes through `sun.done`; the script only stays alive.
        "{\"kind\":\"text\",\"role\":\"assistant\",\"text\":\"on it\"}\n\
         {\"delay_ms\":120000,\"kind\":\"text\",\"role\":\"assistant\",\"text\":\"still here\"}\n",
    )
    .unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("sun.yaml"), "default_runtime: fake\n").unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    // The PR step's `github` mod, installed the way `sun plugin add` does.
    install_fake_github(&rt, dir.path(), &data);

    let fake = FakeRuntime::new("fake", built("sun-fake-runtime"), &script);
    let briefs = Arc::new(Mutex::new(Vec::new()));
    let kernel = rt
        .block_on(serve::start(
            ServeOptions {
                data_dir: data.clone(),
                with_host: true,
                listen: Some("127.0.0.1:0".into()),
                sandbox_mods: false,
                sun_exe: env!("CARGO_BIN_EXE_sun").into(),
                exec_env_tools: Default::default(),
            },
            vec![Arc::new(Editing(fake, briefs.clone()))],
        ))
        .unwrap();
    let base = format!("http://{}", kernel.addr);
    let api = Client::new(&base, &kernel.token);
    // First run seeds the owner: admin@sun.local, the token as the password.
    assert_eq!(kernel.first_admin.as_deref(), Some("admin@sun.local"));
    let sign_in = json!({ "email": "admin@sun.local", "password": kernel.token });
    api.post("/session", &sign_in).unwrap();
    assert!(
        api.post("/session", &json!({ "token": kernel.token }))
            .is_err()
    );
    // The catalog is wired (without it `/catalog` answers 503).
    assert!(api.get("/catalog").unwrap()["entries"].is_array());

    // The in-process host enrolled itself and came online over loopback.
    wait_for("the local host", || {
        let hosts = api.get("/hosts").unwrap();
        hosts
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["status"] == "online")
            .then_some(hosts)
    });

    // The workspace's repo, seeded in the store: the API takes only git
    // host URLs, and the bare origin is a local path. The run resolves it
    // from the task title (`origin`, the URL's last segment).
    let workspace: sun_core::WorkspaceId = api.workspace_id("main").unwrap().parse().unwrap();
    rt.block_on(async {
        use sun_store::RepoRepo;
        let store = sun_store::Store::open(&data).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        tx.insert_repo(&sun_core::model::Repo {
            id: sun_core::RepoId::generate(),
            workspace_id: workspace,
            url: origin.to_str().unwrap().into(),
            default_branch: "main".into(),
            gate_config: json!({}),
            description: String::new(),
            hook_id: None,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
    });
    let workflows = api.get("/workflows").unwrap();
    let default = workflows
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "default")
        .expect("the default workflow is seeded on first boot");
    let task = api
        .post(
            "/tasks",
            &json!({ "title": "E2E in origin", "workflow_id": default["id"] }),
        )
        .unwrap();
    let path = format!("/tasks/{}", task["id"].as_str().unwrap());

    let review = |task: &Value| {
        task["run"]["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["step_key"] == "review" && s["status"] == "waiting_approval")
    };
    let waiting = wait_for("the review step", || {
        let task = api.get(&path).unwrap();
        let ended = matches!(
            task["status"].as_str(),
            Some("failed" | "cancelled" | "done")
        );
        (review(&task) || ended).then_some(task)
    });
    if !review(&waiting) {
        let run = waiting["run"]["id"].as_str().unwrap_or_default();
        let events = api.get(&format!("/runs/{run}/events")).unwrap();
        panic!("{waiting:#}\nevents: {events:#}");
    }
    api.post(&format!("{path}/approve"), &json!({})).unwrap();
    let done = wait_for("done", || {
        let task = api.get(&path).unwrap();
        (task["status"] == "done").then_some(task)
    });
    assert_eq!(done["run"]["status"], "done", "{done:#}");
    assert!(
        done["run"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["status"] == "succeeded"),
        "{done:#}"
    );
    // The PR step pushed the task branch (`sun/<KEY>-<slug>`), with the
    // agent's edit, first.
    let branch = format!("sun/{}-e2e-in-origin", task["key"].as_str().unwrap());
    let pushed = std::process::Command::new("git")
        .args(["show", &format!("{branch}:src/lib.rs")])
        .current_dir(&origin)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&pushed.stdout),
        "//! Changed by the agent.\n",
        "{pushed:?}"
    );
    // The implement step finished through `sun.done`, after `sun.task`.
    let briefs = briefs.lock().unwrap();
    assert!(
        briefs
            .iter()
            .any(|b| b.contains("\"title\": \"E2E in origin\"")),
        "{briefs:?}"
    );

    // The web UI is the router's fallback at `/` (when web/dist was built).
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist/index.html");
    let response = ureq::get(&base).call();
    if ui.exists() {
        let mut response = response.unwrap();
        let html = response.body_mut().read_to_string().unwrap();
        assert!(html.contains("<html"), "{html}");
    }
    kernel.done.abort();
}
