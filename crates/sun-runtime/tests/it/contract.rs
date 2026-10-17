//! Contract tests: the same checks over every adapter's fixtures,
//! for every recorded CLI version (`fixtures/<cli>/<version>/`). No real CLI
//! is called; the weekly `cli-matrix` CI job records new versions.
#![allow(clippy::unwrap_used)]

use crate::common;

use std::sync::Arc;

use common::{CLIS, cli_dir, collect, recorded_versions, replay_in, spec};
use sun_core::{HostCtx, HostId, LimitKind, RuntimeAdapter, SunEvent};
use sun_runtime::ModRuntimeAdapter;
use sun_runtime::contract::{Case, violations};
use sun_runtime::process::{is_tested, tested_versions};

#[test]
fn every_adapter_honors_the_contract_on_every_version_fixture() {
    let mut failures = Vec::new();
    for cli in CLIS {
        for version in recorded_versions(cli) {
            let dir = format!("{}/{version}", cli_dir(cli));
            for case in Case::ALL {
                let events = replay_in(cli, &dir, case.name());
                for rule in violations(case, &events) {
                    failures.push(format!(
                        "{cli} {version}/{}: {rule}\n{events:#?}",
                        case.name()
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The same contract over an extension's runtime adapter (mod API 1.2): the
/// real echo test mod, spawned by the mod host, replays each case.
#[tokio::test]
async fn every_adapter_honors_the_contract_as_an_extension() {
    let mods = Arc::new(echo_extension().await);
    let adapter = ModRuntimeAdapter::new(mods.clone(), "echo", "echo");
    assert_eq!(adapter.id(), "echo.echo");
    let host = HostCtx {
        host_id: HostId::generate(),
    };
    assert!(adapter.readiness(&host).await.logged_in);
    let dir = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for case in Case::ALL {
        let mut spec = spec(dir.path(), &[("SUN_TOKEN", "s3cret-token-42")]);
        spec.envelope = case.name().into();
        let session = adapter.start(spec).await.unwrap();
        assert!(session.native_id.is_some());
        let events = collect(adapter.events(&session)).await;
        let given = SunEvent::Text {
            role: sun_core::Role::System,
            text: "env vars: 0".into(),
        };
        assert_eq!(events.first(), Some(&given), "the extension got no env");
        for rule in violations(case, &events) {
            failures.push(format!("echo.echo/{}: {rule}\n{events:#?}", case.name()));
        }
        let transcript = adapter.export(&session).await.unwrap();
        assert_eq!(transcript.events, events);
        adapter
            .stop(&session, sun_core::StopMode::Graceful)
            .await
            .unwrap();
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    mods.shutdown().await;
}

/// The echo test mod (`sun-mod`'s `sun-echo-mod`), built and loaded as an
/// extension with runtime adapter `echo`.
async fn echo_extension() -> sun_mod::ModHost {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut args = vec!["build", "-q", "-p", "sun-mod", "--bin", "sun-echo-mod"];
    if !cfg!(debug_assertions) {
        args.push("--release");
    }
    let built = std::process::Command::new(cargo).args(&args).status();
    assert!(built.unwrap().success(), "cargo {args:?} failed");
    // `target/<profile>/`, next to this test's `deps/`.
    let exe = std::env::current_exe().unwrap();
    let bin = exe.parent().unwrap().parent().unwrap().join("sun-echo-mod");
    let yaml = format!(
        "name: echo\nkind: extension\nversion: 0.1.0\napi: \">=1.2 <2.0\"\n\
         entry: {{ command: \"{}\" }}\nprovides: {{ runtime_adapters: [echo] }}\n",
        bin.display()
    );
    let host = sun_mod::ModHost::new(sun_mod::HostConfig::default());
    let manifest = sun_mod::parse_manifest(&yaml).unwrap();
    host.add(manifest, std::env::temp_dir()).await.unwrap();
    host
}

#[test]
fn limit_hits_are_classified_per_cli_and_version() {
    for (cli, want) in [
        ("claude-code", LimitKind::Quota),
        ("codex", LimitKind::Quota),
        ("opencode", LimitKind::RateLimit),
    ] {
        for version in recorded_versions(cli) {
            let dir = format!("{}/{version}", cli_dir(cli));
            let events = replay_in(cli, &dir, "limit-hit");
            let kinds: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    SunEvent::LimitHit { limit, .. } => Some(*limit),
                    _ => None,
                })
                .collect();
            assert_eq!(kinds, [want], "{cli} {version}: {events:#?}");
        }
    }
}

/// Every tested version in `versions.txt` has its recordings, so the pinned
/// fallback is always one the contract tests passed.
#[test]
fn every_tested_version_has_recordings() {
    for cli in CLIS {
        let tested = tested_versions(cli);
        assert!(!tested.is_empty(), "{cli} has no line in versions.txt");
        let recorded = recorded_versions(cli);
        for version in tested {
            assert!(
                recorded.iter().any(|r| r == version),
                "{cli} {version} is listed but has no fixtures/{cli}/{version}/"
            );
            assert!(is_tested(cli, version));
        }
    }
    // Patch releases of a tested minor count; other minors don't.
    assert!(is_tested("claude-code", "2.1.300"));
    assert!(!is_tested("claude-code", "2.10.0"));
    assert!(!is_tested("claude-code", "2.2.0"));
    assert!(!is_tested("gemini", "1.0.0"));
}

/// BLO-163: every recorded Claude Code and Codex version's own `--help`
/// (`help.txt`, recorded with the fixtures) offers the probe every mode the
/// CLI enforces exactly: all five on Claude Code, all but `accept_edits` on
/// Codex. A version that drops one fails the matrix and stays unpinned.
/// OpenCode answers permission requests over ACP: no flag to read.
#[test]
fn every_recorded_cli_offers_its_permission_modes() {
    use sun_core::PermissionMode::{self, *};
    type Probe = fn(&str) -> Vec<PermissionMode>;
    let probes: [(&str, Probe, &[PermissionMode], &[PermissionMode]); 2] = [
        (
            "claude-code",
            sun_runtime::claude::parse_permission_modes_help,
            &PermissionMode::ALL,
            &PermissionMode::LEGACY,
        ),
        (
            "codex",
            sun_runtime::codex::parse_permission_modes_help,
            &[Default, Plan, Auto, BypassPermissions],
            &[Default, Plan, BypassPermissions],
        ),
    ];
    for (cli, probe, every, before_auto) in probes {
        for version in recorded_versions(cli) {
            let path = format!("{}/{version}/help.txt", cli_dir(cli));
            let help = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(probe(&help), every, "{cli} {version}");
        }
        // A CLI from before `auto` keeps the modes it could always take.
        assert_eq!(
            probe("  --permission-mode <mode>  (choices: \"plan\")\n"),
            before_auto,
            "{cli}"
        );
    }
}
