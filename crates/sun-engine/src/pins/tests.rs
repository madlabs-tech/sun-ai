#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use sun_core::HostId;
use sun_core::model::{PinRect, SourceLocation};
use sun_store::{RepoRepo, Store};

use super::*;
use crate::gates::tests::{LocalHost, events, mods, seed, seed_with, until_run_ends};
use crate::{EngineConfig, WatchdogConfig};

const AGENT: &str = "name: fix\nsteps:\n  - id: implement\n    type: agent\n    prompt: fix it\n";

async fn start(dir: &Path, store: &Store) -> Engine {
    let config = EngineConfig {
        allow_bypass: false,
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: WatchdogConfig::default(),
    };
    let host = LocalHost::new(dir, |_, _| {});
    Engine::start(store.clone(), host, mods(json!({})), config)
        .await
        .unwrap()
        .0
}

fn target(source: Option<SourceLocation>) -> PinTarget {
    PinTarget {
        selector: "#buy".into(),
        tag: "button".into(),
        text: "Buy".into(),
        width: 360,
        rect: PinRect {
            x: 20.0,
            y: 30.0,
            width: 10.0,
            height: 5.0,
        },
        styles: Default::default(),
        source,
        path: "/pricing".into(),
    }
}

async fn pin_of(engine: &Engine, task_id: TaskId, id: PinId) -> Pin {
    let pins = engine.pins(task_id).await.unwrap();
    pins.into_iter().find(|p| p.id == id).unwrap()
}

/// Waits until `ready(pin)` holds.
async fn until_pin(engine: &Engine, task_id: TaskId, id: PinId, ready: impl Fn(&Pin) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready(&pin_of(engine, task_id, id).await) {
        assert!(Instant::now() < deadline, "pin never got there");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A pin sent to the agent is done once its agent step succeeds, with no
/// canvas open; an open pin stays open.
#[tokio::test(flavor = "multi_thread")]
async fn pins_with_the_agent_are_done_when_its_step_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let engine = start(dir.path(), &store).await;
    let run = seed(&store, AGENT).await;
    let task_id = engine.run(run).await.unwrap().task_id;
    let sent = engine
        .create_pin(task_id, target(None), "bigger".into())
        .await
        .unwrap();
    let change = PinChange {
        status: Some(PinStatus::InProgress),
        ..PinChange::default()
    };
    engine.update_pin(task_id, sent.id, change).await.unwrap();
    let open = engine
        .create_pin(task_id, target(None), String::new())
        .await
        .unwrap();

    engine.start_run(run).await.unwrap();
    until_run_ends(&store, run).await;
    assert_eq!(
        pin_of(&engine, task_id, sent.id).await.status,
        PinStatus::Done
    );
    assert_eq!(
        pin_of(&engine, task_id, open.id).await.status,
        PinStatus::Open
    );
    // The agent's change landed: the done pin gets its after crop.
    until_pin(&engine, task_id, sent.id, |p| p.after_crop.is_some()).await;
}

/// A pin carries a crop of its element from the task branch's
/// preview when pinned, and another once it is done.
#[tokio::test(flavor = "multi_thread")]
async fn pins_carry_a_screenshot_crop() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let engine = start(dir.path(), &store).await;
    let run = seed(&store, AGENT).await;
    let task_id = engine.run(run).await.unwrap().task_id;
    let pin = engine
        .create_pin(task_id, target(None), String::new())
        .await
        .unwrap();
    assert_eq!(pin.before_crop, None, "taken in the background");

    until_pin(&engine, task_id, pin.id, |p| p.before_crop.is_some()).await;
    let png = engine
        .pin_crop(task_id, pin.id, PreviewSide::Before)
        .await
        .unwrap();
    // The 10×5 box at (20, 30) plus 8 px of context on every side.
    let crop = screenshot::decode(&png).unwrap();
    assert_eq!((crop.width, crop.height), (26, 21));
    assert_eq!(&crop.rgba[..4], &[12, 22, 0, 255]);
    assert!(matches!(
        engine.pin_crop(task_id, pin.id, PreviewSide::After).await,
        Err(Error::NotFound(_))
    ));

    let done = PinChange {
        status: Some(PinStatus::Done),
        ..PinChange::default()
    };
    engine.update_pin(task_id, pin.id, done).await.unwrap();
    until_pin(&engine, task_id, pin.id, |p| p.after_crop.is_some()).await;
    let after = engine
        .pin_crop(task_id, pin.id, PreviewSide::After)
        .await
        .unwrap();
    assert_eq!(screenshot::decode(&after).unwrap().width, 26);
}

/// "Change it myself" commits the edit to the task branch under
/// the user's name, puts the commit on the timeline and re-runs the gates.
#[tokio::test(flavor = "multi_thread")]
async fn pin_edit_commits_as_user_and_reruns_gates() {
    let dir = tempfile::tempdir().unwrap();
    // The task worktree LocalHost hands out, with the app's source.
    let work = dir.path().join("work/task");
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::write(
        work.join("src/App.svelte"),
        "<main>\n  <button class=\"buy\">Buy</button>\n</main>\n",
    )
    .unwrap();
    let git = |script: &str| {
        let out = std::process::Command::new("sh")
            .args(["-c", script])
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    git(
        "git init -q -b main && git add -A && git -c user.email=t@t -c user.name=t commit -q -m base \
         && git update-ref refs/remotes/origin/main HEAD",
    );

    let store = Store::open(dir.path()).await.unwrap();
    let repo = crate::gates::tests::repo("https://github.com/acme/app.git");
    let mut tx = store.begin().await.unwrap();
    tx.insert_repo(&repo).await.unwrap();
    tx.commit().await.unwrap();
    let yaml = "name: ui\nsteps:\n  - id: implement\n    type: agent\n    prompt: fix it\n\
                \x20 - id: check\n    type: gate\n    needs: [implement]\n    \
                gates: [{test: {command: \"grep -q 'Buy now' src/App.svelte\"}}]\n";
    let run = seed_with(&store, yaml, vec![repo.id]).await;
    let engine = start(dir.path(), &store).await;
    let task_id = engine.run(run).await.unwrap().task_id;
    engine.start_run(run).await.unwrap();
    until_run_ends(&store, run).await;
    let first = events(&store, run, "gate_result").await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["result"]["pass"], false);

    let source = SourceLocation {
        file: "src/App.svelte".into(),
        line: 2,
        column: 3,
    };
    let pin = engine
        .create_pin(task_id, target(Some(source.clone())), String::new())
        .await
        .unwrap();
    let alice = || Author {
        name: "alice".into(),
        email: "alice@example.com".into(),
    };
    // Nothing to commit yet, and a value that isn't on the line goes to the agent.
    assert!(
        engine
            .commit_pin_edit(task_id, pin.id, alice())
            .await
            .is_err()
    );
    let edits = |from: &str| PinChange {
        edits: Some(vec![PinEdit {
            property: "text".into(),
            from: from.into(),
            to: "Buy now".into(),
        }]),
        ..PinChange::default()
    };
    engine
        .update_pin(task_id, pin.id, edits("Sell"))
        .await
        .unwrap();
    let refused = engine.commit_pin_edit(task_id, pin.id, alice()).await;
    assert!(
        matches!(&refused, Err(Error::InvalidInput(m)) if m.contains("Hand the edit to the agent")),
        "{refused:?}"
    );

    engine
        .update_pin(task_id, pin.id, edits("Buy"))
        .await
        .unwrap();
    let commit = engine
        .commit_pin_edit(task_id, pin.id, alice())
        .await
        .unwrap();
    assert_eq!(commit.pin.status, PinStatus::Done);
    assert_eq!(git("git rev-parse HEAD").trim(), commit.sha);
    assert_eq!(
        git("git log -1 --format='%an <%ae> / %cn <%ce>'").trim(),
        "alice <alice@example.com> / alice <alice@example.com>"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("src/App.svelte")).unwrap(),
        "<main>\n  <button class=\"buy\">Buy now</button>\n</main>\n"
    );
    let journal = events(&store, run, "commit").await;
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0]["sha"], commit.sha.as_str());
    assert_eq!(journal[0]["by"], "alice");

    // The gates ran again on the user's commit, and now pass.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let rerun = events(&store, run, "gate_result").await;
        if let Some(result) = rerun.iter().find(|e| e["rerun"] == "canvas_commit") {
            assert_eq!(result["result"]["pass"], true, "{result}");
            break;
        }
        assert!(Instant::now() < deadline, "no gate re-run");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
