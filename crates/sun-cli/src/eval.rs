//! `sun eval run|list|show`: replay past tasks against a candidate change
//! through the running kernel.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::client::Client;

#[derive(Subcommand)]
pub enum EvalCommand {
    /// Replay past tasks in a sandbox (outbox actions are recorded, never
    /// sent) and compare with their original runs.
    Run {
        /// A name for this eval.
        name: String,
        /// Pick the tasks of this workflow id.
        #[arg(long, group = "pick")]
        workflow: Option<String>,
        /// Pick tasks corrected for this cause (test_failure, lint,
        /// missing_requirement, wrong_scope, style, other).
        #[arg(long, group = "pick")]
        cause: Option<String>,
        /// Pick a saved eval set.
        #[arg(long, group = "pick")]
        set: Option<String>,
        /// Pick these task ids.
        #[arg(long = "task", group = "pick")]
        tasks: Vec<String>,
        /// Candidate instruction line, added as active memory (repeatable).
        #[arg(long = "memory")]
        memory: Vec<String>,
        /// Candidate workflow file replayed instead of the latest version.
        #[arg(long)]
        workflow_file: Option<PathBuf>,
        /// Run agents on this runtime for real (uses quota). Default: replay
        /// the recorded transcripts.
        #[arg(long)]
        runtime: Option<String>,
        /// Most tasks (1–50).
        #[arg(long)]
        limit: Option<u32>,
        /// Approve every approval instead of replaying the original
        /// decisions (a human rejection then does not recur).
        #[arg(long)]
        auto_approve: bool,
        /// Wait for the report and print it.
        #[arg(long)]
        wait: bool,
    },
    /// List evals, newest first.
    List,
    /// Show one eval's report.
    Show {
        /// Eval id.
        id: String,
    },
}

pub fn run(client: &Client, command: EvalCommand) -> Result<()> {
    match command {
        EvalCommand::Run {
            name,
            workflow,
            cause,
            set,
            tasks,
            memory,
            workflow_file,
            runtime,
            limit,
            auto_approve,
            wait,
        } => {
            let pick = match (workflow, cause, set) {
                (Some(id), _, _) => json!({ "by": "workflow", "workflow_id": id }),
                (_, Some(cause), _) => json!({ "by": "cause", "cause": cause }),
                (_, _, Some(id)) => json!({ "by": "set", "set_id": id }),
                _ if !tasks.is_empty() => json!({ "by": "tasks", "task_ids": tasks }),
                _ => bail!("pick tasks: --workflow, --cause, --set or --task"),
            };
            let workflow = match workflow_file {
                Some(path) => Some(
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("reading {}", path.display()))?,
                ),
                None => None,
            };
            let eval = client.post(
                "/evals",
                &json!({
                    "name": name,
                    "pick": pick,
                    "candidate": { "workflow": workflow, "memory": memory },
                    "runtime": runtime,
                    "limit": limit,
                    "auto_approve": auto_approve,
                }),
            )?;
            let id = eval["id"].as_str().context("the kernel sent no id")?;
            if !wait {
                println!("{id}  running  (sun eval show {id})");
                return Ok(());
            }
            loop {
                let eval = client.get(&format!("/evals/{id}"))?;
                if eval["status"] != "running" {
                    print_eval(&eval);
                    return Ok(());
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        EvalCommand::List => {
            for eval in client.get("/evals")?.as_array().into_iter().flatten() {
                let delta = match eval["report"]["pass_delta"].as_f64() {
                    Some(d) => format!("{:+.0} pts", d * 100.0),
                    None => "-".into(),
                };
                println!(
                    "{}  {:<8}  {:>8}  {}",
                    eval["id"].as_str().unwrap_or("?"),
                    eval["status"].as_str().unwrap_or("?"),
                    delta,
                    eval["name"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        EvalCommand::Show { id } => {
            print_eval(&client.get(&format!("/evals/{id}"))?);
            Ok(())
        }
    }
}

fn print_eval(eval: &Value) {
    println!(
        "{}  {}  {}",
        eval["name"].as_str().unwrap_or(""),
        eval["status"].as_str().unwrap_or("?"),
        eval["id"].as_str().unwrap_or("")
    );
    if let Some(error) = eval["error"].as_str() {
        println!("error: {error}");
    }
    let report = &eval["report"];
    if report.is_null() {
        return;
    }
    let (o, r) = (&report["original"], &report["replay"]);
    let pct = |v: &Value| format!("{:.0}%", v.as_f64().unwrap_or(0.0) * 100.0);
    println!(
        "runtime {}  ·  {} tasks replayed",
        report["runtime"].as_str().unwrap_or("?"),
        report["replayed"]
    );
    println!(
        "{:<12} {:>9} {:>9} {:>9}",
        "", "original", "replay", "delta"
    );
    println!(
        "{:<12} {:>9} {:>9} {:>9}",
        "pass rate",
        pct(&o["pass_rate"]),
        pct(&r["pass_rate"]),
        format!(
            "{:+.0} pts",
            report["pass_delta"].as_f64().unwrap_or(0.0) * 100.0
        )
    );
    println!(
        "{:<12} {:>9} {:>9} {:>9}",
        "repeats",
        o["repeats"],
        r["repeats"],
        format!("{:+}", report["repeat_delta"])
    );
    for task in report["tasks"].as_array().into_iter().flatten() {
        let side = |s: &Value| match s.is_null() {
            true => "-".to_owned(),
            false => format!(
                "{} ({} corrections)",
                s["status"].as_str().unwrap_or("?"),
                s["causes"].as_array().map_or(0, Vec::len)
            ),
        };
        println!(
            "{}  {} → {}  {}",
            task["task_id"].as_str().unwrap_or("?"),
            side(&task["original"]),
            side(&task["replay"]),
            task["title"].as_str().unwrap_or("")
        );
        if let Some(error) = task["error"].as_str() {
            println!("    replay failed: {error}");
        }
    }
}
