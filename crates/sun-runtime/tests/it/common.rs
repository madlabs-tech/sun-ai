//! Helpers shared by the adapter tests: fixture replay, fake CLIs, streams.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sun_core::{BoxStream, RunId, SessionId, SessionSpec, StepKey, SunEvent};
use sun_runtime::claude::ClaudeStream;
use sun_runtime::codex::CodexStream;
use sun_runtime::opencode::AcpStream;
use sun_runtime::process::replay;

/// The adapter ids that have fixtures.
pub const CLIS: [&str; 3] = ["claude-code", "codex", "opencode"];

/// `fixtures/<cli>`: one dir of recordings per CLI version.
pub fn cli_dir(cli: &str) -> String {
    format!("{}/fixtures/{cli}", env!("CARGO_MANIFEST_DIR"))
}

/// The recordings the exact-replay tests read. Fixed on purpose, not the
/// pinned version: those tests assert values from one recording (session ids,
/// token counts, exit codes, paths), so pinning a new version in versions.txt
/// must not move them. `contract::` covers every recorded version.
pub fn exact_dir(cli: &str) -> String {
    let version = match cli {
        "claude-code" => "2.1.291",
        "codex" => "0.160.0",
        "opencode" => "1.18.34",
        _ => panic!("no exact recording for {cli}"),
    };
    format!("{}/{version}", cli_dir(cli))
}

/// Every version of `cli` with recordings (sorted), listed in versions.txt or
/// not: a fresh recording is tested before it is pinned.
pub fn recorded_versions(cli: &str) -> Vec<String> {
    let mut versions: Vec<String> = std::fs::read_dir(cli_dir(cli))
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().unwrap().is_dir())
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    versions.sort();
    versions
}

/// A recorded fixture in [`exact_dir`]: stdout lines, exit code and stderr tail.
pub fn fixture(cli: &str, case: &str) -> (String, Option<i32>, String) {
    fixture_in(&exact_dir(cli), case)
}

/// A recorded fixture in `dir`.
pub fn fixture_in(dir: &str, case: &str) -> (String, Option<i32>, String) {
    let jsonl = std::fs::read_to_string(format!("{dir}/{case}.jsonl")).unwrap();
    let meta: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{case}.meta.json")).unwrap())
            .unwrap();
    let code = meta["exit_code"]
        .as_i64()
        .map(|c| i32::try_from(c).unwrap());
    let stderr = meta["stderr_tail"].as_str().unwrap_or_default().to_owned();
    (jsonl, code, stderr)
}

/// Replays `cli`'s fixture `case` in [`exact_dir`] through that adapter's normalizer.
pub fn replay_case(cli: &str, case: &str) -> Vec<SunEvent> {
    replay_in(cli, &exact_dir(cli), case)
}

/// Replays fixture `case` in `dir` through `cli`'s normalizer. The
/// recordings ran in `/work`.
pub fn replay_in(cli: &str, dir: &str, case: &str) -> Vec<SunEvent> {
    let (jsonl, code, stderr) = fixture_in(dir, case);
    let cwd = Some(PathBuf::from("/work"));
    match cli {
        "claude-code" => replay(cli, ClaudeStream::default(), &jsonl, code, &stderr),
        "codex" => replay(cli, CodexStream::new(cwd, None), &jsonl, code, &stderr),
        "opencode" => replay(
            cli,
            AcpStream::new(String::new(), cwd, None),
            &jsonl,
            code,
            &stderr,
        ),
        _ => panic!("no normalizer for {cli}"),
    }
}

/// A fresh dir holding `script` as an executable named `bin`. The script
/// must exit at once on `--version`: [`warm`] runs it that way.
pub fn sandbox(bin: &str, script: &str, name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{bin}-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(bin);
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    warm(&path);
    dir
}

/// Runs `path --version` until exec stops failing with ETXTBSY.
///
/// ponytail: rust-lang/rust#114554. A fork in another test thread can inherit
/// our write fd of the fresh script until its exec closes it (O_CLOEXEC), so
/// an exec in that window fails with "Text file busy". Retrying here keeps the
/// adapters' real exec out of the window; drop this once std closes the race.
fn warm(path: &Path) {
    for _ in 0..50 {
        match std::process::Command::new(path).arg("--version").output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => return,
        }
    }
}

/// A session spec in `dir`; `SUN_OUT` tells the fake CLI where to write.
pub fn spec(dir: &Path, env: &[(&str, &str)]) -> SessionSpec {
    let mut vars: BTreeMap<String, String> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    vars.insert("SUN_OUT".into(), dir.join("out").display().to_string());
    SessionSpec {
        skills: None,
        session_id: SessionId::generate(),
        run_id: RunId::generate(),
        step_key: StepKey::new("implement").unwrap(),
        worktree: dir.to_path_buf(),
        envelope: "Create hello.txt".into(),
        envelope_parts: None,
        model: None,
        effort: None,
        mcp_config: None,
        env: vars,
        exec_env: Default::default(),
        resume: None,
        permission_mode: None,
        byo_key: None,
    }
}

/// Envelope parts as the engine sends them.
pub fn parts() -> sun_core::Envelope {
    sun_core::Envelope {
        task: "Create hello.txt".into(),
        constraints: vec!["No new dependencies.".into()],
        gates: vec!["cargo test".into()],
        context: vec![],
        handoff: None,
    }
}

/// Every event of `stream`, failing after 20 s.
pub async fn collect(mut stream: BoxStream<'static, SunEvent>) -> Vec<SunEvent> {
    let all = async {
        let mut out = Vec::new();
        while let Some(event) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            out.push(event);
        }
        out
    };
    tokio::time::timeout(Duration::from_secs(20), all)
        .await
        .unwrap()
}
