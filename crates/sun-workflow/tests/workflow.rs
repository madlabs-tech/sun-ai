use serde_json::Value;
use sun_workflow::*;

/// The reference example, verbatim.
const EXAMPLE: &str = r#"
# .sun/workflows/bugfix.yaml
name: bugfix
version: 3                     # set by Sun on save
on: [manual, jira.issue_labeled:agent]
runtime:
  chain: [claude-code, codex, opencode-openrouter]   # failover order
  on_limit: switch             # switch | pause_and_notify | wait_until_reset
budget: { wall: 2h, turns: 200, cost_usd: 5 }
steps:
  - id: implement
    type: agent
    prompt: prompts/implement.md
    skills: [tdd]
    context: { sources: [repo:docs/, confluence:ENG], budget_tokens: 12000 }
  - id: verify
    type: gate
    gates: [diff_not_empty, test, lint, diagnostics_clean]
    on_fail: { send_back_to: implement, max: 2 }
  - id: pr
    type: action
    use: github.open_pr        # plugin action, idempotent
  - id: review
    type: approval
    who: [role:admin]
    timeout: { after: 24h, then: remind }
"#;

fn errors(source: &str) -> Vec<String> {
    match parse(source) {
        Ok(_) => panic!("expected errors for:\n{source}"),
        Err(errors) => errors.iter().map(ToString::to_string).collect(),
    }
}

fn assert_error(source: &str, expected: &str) {
    let errors = errors(source);
    assert!(
        errors.iter().any(|e| e.starts_with(expected)),
        "no error starting with `{expected}` in {errors:#?}"
    );
}

#[test]
fn trd_example_parses() {
    let wf = parse(EXAMPLE).unwrap();
    assert_eq!(wf.name, "bugfix");
    assert_eq!(wf.on, ["manual", "jira.issue_labeled:agent"]);
    let runtime = wf.runtime.as_ref().unwrap();
    assert_eq!(
        runtime.chain,
        ["claude-code", "codex", "opencode-openrouter"]
    );
    assert_eq!(runtime.on_limit, Some(OnLimit::Switch));
    assert!(runtime.review_after_failover.is_empty() && !runtime.switch_back);
    let opt_in = "name: wf\nruntime: {chain: [a, b], review_after_failover: [role:lead], switch_back: true}\nsteps: [{id: a, type: agent, prompt: p}]\n";
    let runtime = parse(opt_in).unwrap().runtime.unwrap();
    assert_eq!(
        runtime.on_limit, None,
        "unset: the ecosystem default applies"
    );
    assert_eq!(runtime.review_after_failover, ["role:lead"]);
    assert!(runtime.switch_back);
    let bad = "name: wf\nruntime: {chain: [a], review_after_failover: [lead]}\nsteps: []\n";
    assert!(
        parse(bad).is_err(),
        "approvers are role:<name> or user:<name>"
    );
    assert_eq!(wf.budget.wall_secs, Some(7_200));
    assert_eq!(wf.budget.turns, Some(200));
    assert_eq!(wf.budget.cost_usd, Some(5.0));

    let ids: Vec<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["implement", "verify", "pr", "review"]);
    assert!(wf.steps[0].needs.is_empty());
    assert_eq!(wf.steps[1].needs[0].as_str(), "implement");
    assert!(matches!(
        &wf.steps[0].kind,
        StepKind::Agent { context_budget_tokens: Some(12_000), context_sources, .. } if context_sources.len() == 2
    ));
    assert!(matches!(
        &wf.steps[1].kind,
        StepKind::Gate { gates, on_fail: Some(OnFail { max: 2, .. }) } if gates.len() == 4
    ));
    assert!(
        matches!(&wf.steps[2].kind, StepKind::Action { target, .. } if target == "github.open_pr")
    );
    assert_eq!(
        wf.steps[3].kind,
        StepKind::Approval {
            who: vec!["role:admin".into()],
            timeout: Some(ApprovalTimeout {
                after_secs: 86_400,
                then: TimeoutAction::Remind
            }),
            when: None,
        }
    );
    assert_eq!(wf.hash.len(), 64);
}

#[test]
fn default_workflow_is_valid() {
    let wf = parse(DEFAULT_WORKFLOW).unwrap();
    let ids: Vec<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["implement", "verify", "pr", "review"]);
    assert_eq!(wf.on, ["manual"]);
    assert!(wf.runtime.is_none());
}

#[test]
fn every_step_type_parses() {
    let wf = parse(
        r#"
name: all-types
steps:
  - { id: a, type: agent, prompt: "do it" }
  - { id: g, type: gate, gates: [test, { length: { max: 2000 } }, acme.brand_check] }
  - { id: w, type: wait, for: 1h30m }
  - { id: n, type: notify, to: ["slack:#eng"], message: done }
  - { id: x, type: action, use: github.open_pr, with: { draft: true } }
  - { id: s, type: browser.screenshot, with: { url: "http://localhost" } }
  - { id: p, type: approval, who: [user:ana] }
  - id: d
    type: decide
    decides: triage
    question: { type: choice, instructions: which team?, criteria: { billing: refunds, tech: bugs } }
    threshold: 0.8
    who: [role:lead]
"#,
    )
    .unwrap();
    assert!(
        matches!(&wf.steps[7].kind, StepKind::Decide { decides: DecisionUse::Triage, question, threshold, who }
        if question.kind == QuestionKind::Choice && *threshold == 0.8 && who == &["role:lead"])
    );
    assert!(
        matches!(&wf.steps[1].kind, StepKind::Gate { gates, on_fail: None }
        if gates[1].name == "length" && gates[1].params.is_some() && gates[2].name == "acme.brand_check")
    );
    assert_eq!(wf.steps[2].kind, StepKind::Wait { for_secs: 5_400 });
    assert!(
        matches!(&wf.steps[5].kind, StepKind::Extension { name, with: Some(_) } if name == "browser.screenshot")
    );
}

#[test]
fn decide_steps_are_validated() {
    let errors = parse(
        r#"
name: bad-decide
steps:
  - id: d
    type: decide
    decides: flag
    question: { type: score, instructions: how urgent?, criteria: { a: b } }
    threshold: 1.5
    who: [admin]
"#,
    )
    .unwrap_err();
    let text: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(errors.len(), 3, "{text:?}");
    assert!(text.iter().any(|e| e.contains("invalid approver `admin`")));
    assert!(text.iter().any(|e| e.contains("list of levels for score")));
    assert!(
        text.iter()
            .any(|e| e.contains("`threshold` must be between 0 and 1"))
    );
    // A decision can never pass or approve: `decides: pass` is not a value.
    let errors =
        parse("name: x\nsteps:\n  - { id: d, type: decide, decides: pass }\n").unwrap_err();
    assert!(
        errors[0].message.contains("unknown variant `pass`"),
        "{errors:?}"
    );
}

/// An explicit empty prompt is allowed (the session gets its task and
/// context only); a missing one is an error (`semantic_errors_…`).
#[test]
fn an_agent_prompt_may_be_explicitly_empty() {
    let wf = parse("name: bare\nsteps:\n  - { id: a, type: agent, prompt: \"\" }\n").unwrap();
    let StepKind::Agent { prompt, .. } = &wf.steps[0].kind else {
        panic!("{:?}", wf.steps[0].kind)
    };
    assert_eq!(prompt, "");
}

#[test]
fn needs_allows_parallel_branches() {
    let wf = parse(
        "name: fan\nsteps:\n  - { id: a, type: wait, for: 1s }\n  - { id: b, type: wait, for: 1s, needs: [a] }\n  - { id: c, type: wait, for: 1s, needs: [a] }\n  - { id: d, type: wait, for: 1s, needs: [b, c] }\n  - { id: e, type: wait, for: 1s, needs: [] }\n",
    )
    .unwrap();
    let needs: Vec<Vec<&str>> = wf
        .steps
        .iter()
        .map(|s| s.needs.iter().map(|n| n.as_str()).collect())
        .collect();
    assert_eq!(
        needs,
        [vec![], vec!["a"], vec!["a"], vec!["b", "c"], vec![]]
    );
}

#[test]
fn semantic_errors_have_line_and_column() {
    let base = "name: wf\nsteps:\n";
    let cases = [
        // (steps yaml, expected `line:col: message` prefix)
        (
            "  - { id: a, type: agent, prompt: x }\n  - { id: a, type: agent, prompt: y }\n",
            "4:11: duplicate step id `a` (first at 3:11)",
        ),
        (
            "  - { id: a, type: agent, prompt: x, needs: [zz] }\n",
            "3:46: `needs` names unknown step `zz`",
        ),
        (
            "  - { id: Bad, type: agent, prompt: x }\n",
            "3:11: invalid step id `Bad`",
        ),
        (
            "  - { id: a, type: deploy }\n",
            "3:20: unknown step type `deploy`",
        ),
        (
            "  - { id: a, type: agent }\n",
            "3:11: `agent` steps need a `prompt`",
        ),
        (
            "  - { id: a, type: agent, prompt: x, gates: [test] }\n",
            "3:45: `gates` is not allowed on `agent` steps",
        ),
        (
            "  - { id: a, type: gate, gates: [tests] }\n",
            "3:34: unknown gate `tests`",
        ),
        (
            "  - { id: a, type: gate, gates: [test], on_fail: { send_back_to: nope, max: 1 } }\n",
            "3:66: `send_back_to` names unknown step `nope`",
        ),
        (
            "  - { id: a, type: wait, for: 5 }\n",
            "3:31: invalid duration `5`",
        ),
        (
            "  - { id: a, type: approval, who: [admin] }\n",
            "3:36: invalid approver `admin`",
        ),
        (
            "  - { id: a, type: action, use: open_pr }\n",
            "3:33: invalid action `open_pr`",
        ),
    ];
    for (steps, expected) in cases {
        assert_error(&format!("{base}{steps}"), expected);
    }
}

#[test]
fn device_verify_needs_a_pr_before_it() {
    let verify = "  - { id: v, type: device.verify_on_device, needs: [a], with: {} }\n";
    assert_error(
        &format!("name: wf\nsteps:\n  - {{ id: a, type: wait, for: 1s }}\n{verify}"),
        "4:20: `device.verify_on_device` attaches screenshots to the PR",
    );
    // A PR on a parallel branch doesn't count; one before it, through `needs`, does.
    let pr = "  - { id: pr, type: action, use: github.open_pr, needs: [] }\n";
    assert_error(
        &format!("name: wf\nsteps:\n  - {{ id: a, type: wait, for: 1s }}\n{pr}{verify}"),
        "5:20: `device.verify_on_device`",
    );
    let after_pr = "  - { id: a, type: wait, for: 1s, needs: [pr] }\n";
    parse(&format!("name: wf\nsteps:\n{pr}{after_pr}{verify}")).unwrap();
}

#[test]
fn cycles_are_rejected() {
    assert_error(
        "name: wf\nsteps:\n  - { id: a, type: wait, for: 1s, needs: [b] }\n  - { id: b, type: wait, for: 1s }\n",
        "3:42: `needs` forms a cycle through steps a, b",
    );
    assert_error(
        "name: wf\nsteps:\n  - { id: a, type: wait, for: 1s, needs: [a] }\n",
        "3:42: `needs` forms a cycle through steps a;",
    );
}

#[test]
fn top_level_errors() {
    let step = "steps:\n  - { id: a, type: wait, for: 1s }\n";
    assert_error(
        &format!("name: Not Valid\n{step}"),
        "1:7: invalid workflow name",
    );
    assert_error(
        &format!("name: wf\non: [nightly]\n{step}"),
        "2:6: unknown trigger `nightly`",
    );
    assert_error(
        &format!("name: wf\nruntime: {{ chain: [codex, codex] }}\n{step}"),
        "2:27: runtime `codex` appears twice",
    );
    assert_error(
        &format!("name: wf\nruntime: {{ chain: [a.b.c] }}\n{step}"),
        "2:20: invalid runtime name `a.b.c`",
    );
    let extension = format!("name: wf\nruntime: {{ chain: [claude-code, echo.echo] }}\n{step}");
    let chain = sun_workflow::parse(&extension)
        .unwrap()
        .runtime
        .unwrap()
        .chain;
    assert_eq!(chain, ["claude-code", "echo.echo"]);
    assert_error(
        &format!("name: wf\nruntime: {{ chain: [] }}\n{step}"),
        "2:19: `runtime.chain` needs at least one runtime",
    );
    assert_error(
        &format!("name: wf\nbudget: {{ wall: soon }}\n{step}"),
        "2:17: invalid duration `soon`",
    );
    assert_error(
        &format!("name: wf\nbudget: {{ turns: 0 }}\n{step}"),
        "2:18: `budget.turns` must be at least 1",
    );
    assert_error(
        "name: wf\nsteps: []\n",
        "2:8: a workflow needs at least one step",
    );
}

#[test]
fn yaml_shape_errors_have_line_and_column() {
    // Unknown field, bad enum value, syntax error: reported by serde-saphyr.
    for (source, expected) in [
        (
            "name: wf\nstepz: []\n",
            "2:1: unknown field `stepz`, expected one of name",
        ),
        (
            "name: wf\nruntime: { chain: [a], on_limit: explode }\nsteps: []\n",
            "2:34: unknown variant `explode`, expected one of switch, pause_and_notify, wait_until_reset",
        ),
        (
            "name: wf\nsteps:\n  - { id: a, type: wait, for: 1s, colour: red }\n",
            "3:35: unknown field `colour`",
        ),
        ("name: wf\nsteps: [\n", "2:8: unclosed bracket"),
        (
            "name: wf\nname: again\nsteps: []\n",
            "2:1: duplicate mapping key: name",
        ),
        ("name: wf\n", "1:1: missing field `steps`"),
    ] {
        let errors = errors(source);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].starts_with(expected),
            "{} vs {expected}",
            errors[0]
        );
        assert!(
            !errors[0].contains(" at line "),
            "location repeated: {}",
            errors[0]
        );
    }
}

#[test]
fn alias_bombs_are_rejected() {
    let mut source = String::from("a0: &a0 [x, x, x, x, x, x, x, x, x, x]\n");
    for i in 1..10 {
        let p = i - 1;
        source.push_str(&format!(
            "a{i}: &a{i} [*a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}]\n"
        ));
    }
    source.push_str("name: wf\nsteps: [*a9]\n");
    assert_eq!(parse(&source).unwrap_err().len(), 1);
}

#[test]
fn all_semantic_errors_are_reported_at_once() {
    let errors = errors(
        "name: wf\nsteps:\n  - { id: a, type: agent }\n  - { id: a, type: gate, gates: [nope] }\n  - { id: c, type: wait, for: never }\n",
    );
    assert_eq!(errors.len(), 4, "{errors:#?}");
}

#[test]
fn hash_ignores_formatting_and_tracks_meaning() {
    let base = parse(EXAMPLE).unwrap().hash;
    let reformatted = EXAMPLE
        .replace("version: 3", "version: 4")
        .replace("# failover order", "")
        .replace(
            "  - id: verify\n    type: gate\n",
            "  - type: gate\n    id: verify\n    needs: [implement]\n",
        );
    assert_ne!(reformatted, EXAMPLE);
    assert_eq!(parse(&reformatted).unwrap().hash, base);

    let changed = EXAMPLE.replace("max: 2", "max: 3");
    assert_ne!(parse(&changed).unwrap().hash, base);
}

#[test]
fn retry_policy_is_per_step() {
    let wf = parse(
        "name: r\nsteps:\n  - { id: a, type: action, use: github.open_pr, retry: { max: 3, backoff: 10s } }\n  - { id: b, type: wait, for: 1s }\n",
    )
    .unwrap();
    let policy = wf.steps[0].retry.unwrap();
    assert_eq!((policy.max, policy.backoff_secs), (3, 10));
    // Doubles per failed attempt; none left after the third.
    let delays: Vec<_> = (1..=3).map(|n| policy.delay_after(n)).collect();
    assert_eq!(delays, [Some(10), Some(20), None]);
    assert_eq!(wf.steps[1].retry, None);
    let errors = parse("name: r\nsteps:\n  - { id: a, type: wait, for: 1s, retry: { max: 0 } }\n")
        .unwrap_err();
    assert!(errors[0].to_string().contains("retry.max"), "{errors:?}");
}

#[test]
fn json_schema_matches_the_parser() {
    let schema: Value = serde_json::from_str(JSON_SCHEMA).unwrap();
    let text = schema.to_string();
    for name in STEP_TYPES.iter().chain(CODE_GATES).chain(CONTENT_GATES) {
        assert!(
            text.contains(&format!("\"{name}\"")),
            "schema misses `{name}`"
        );
    }
    assert_eq!(schema_hash().len(), 64);
}

#[test]
fn role_templates_parse_and_list_their_mods() {
    let ids: Vec<&str> = TEMPLATES.iter().map(|t| t.id).collect();
    assert_eq!(
        ids,
        [
            "bug-fix",
            "feature-across-repos",
            "ui-refactor",
            "marketing-campaign",
            "prd",
            "client-deployment",
            "weekly-summary"
        ]
    );
    for template in TEMPLATES {
        let wf = parse(template.source_yaml)
            .unwrap_or_else(|e| panic!("template {} is invalid: {e:?}", template.id));
        assert_eq!(wf.name, template.id, "template name must match its id");
    }
    let mods = |id: &str| TEMPLATES.iter().find(|t| t.id == id).unwrap().mods();
    assert_eq!(mods("bug-fix"), ["github"]);
    assert_eq!(mods("marketing-campaign"), ["social"]);
    assert_eq!(mods("prd"), ["jira"]);
    assert_eq!(mods("client-deployment"), ["github", "slack"]);
    assert_eq!(mods("feature-across-repos"), ["github"]);
    assert_eq!(mods("weekly-summary"), ["gdrive", "slack"]);
    assert_eq!(parse(EXAMPLE).unwrap().mods(), ["github", "jira"]);
}

/// A `who: role:<name>` no member holds can never be decided: every
/// template names a seeded group key (or `owner`).
#[test]
fn role_templates_approvers_are_member_roles() {
    for template in TEMPLATES {
        let wf = parse(template.source_yaml).unwrap();
        let failover = wf.runtime.iter().flat_map(|r| &r.review_after_failover);
        let steps = wf.steps.iter().flat_map(|s| match &s.kind {
            StepKind::Approval { who, .. } | StepKind::Decide { who, .. } => who.as_slice(),
            _ => &[],
        });
        for entry in failover.chain(steps) {
            if let Some(role) = entry.strip_prefix("role:") {
                assert!(
                    ["viewer", "member", "admin", "owner"].contains(&role),
                    "{}: `{entry}` matches no seeded group",
                    template.id
                );
            }
        }
    }
}

#[test]
fn exec_env_is_parsed_checked_and_bare_keeps_the_hash() {
    let base = parse(EXAMPLE).unwrap();
    assert!(base.exec_env.is_bare());
    let bare = EXAMPLE.replace("steps:\n", "exec_env: { kind: bare }\nsteps:\n");
    assert_eq!(parse(&bare).unwrap().hash, base.hash);

    let container = EXAMPLE.replace(
        "steps:\n",
        "exec_env: { kind: container, image: \"ghcr.io/acme/tool:1.4\", memory_mb: 2048 }\nsteps:\n",
    );
    let wf = parse(&container).unwrap();
    assert_ne!(wf.hash, base.hash);
    assert_eq!(
        serde_json::to_value(&wf.exec_env).unwrap(),
        serde_json::json!({"kind": "container", "image": "ghcr.io/acme/tool:1.4",
            "memory_mb": 2048, "cpu_millis": null, "network": null})
    );
    assert_error(
        &EXAMPLE.replace(
            "steps:\n",
            "exec_env: { kind: vm, template: \"--privileged\" }\nsteps:\n",
        ),
        "10:11: exec_env: invalid input: bad vm template",
    );
}

#[test]
fn mcp_grants_are_parsed_per_workflow_and_step_and_checked() {
    let base = parse(EXAMPLE).unwrap();
    let source = EXAMPLE
        .replace("steps:\n", "mcp: [{ server: docs }]\nsteps:\n")
        .replace(
            "    skills: [tdd]\n",
            "    skills: [tdd]\n    mcp: [{ server: github, tools: [create_issue] }]\n",
        );
    let wf = parse(&source).unwrap();
    assert_ne!(wf.hash, base.hash);
    let grant = |server: &str, tools: Option<Vec<String>>| sun_core::McpGrant {
        server: server.into(),
        tools,
    };
    assert_eq!(wf.mcp, [grant("docs", None)]);
    match &wf.steps[0].kind {
        StepKind::Agent { mcp, .. } => {
            assert_eq!(mcp, &[grant("github", Some(vec!["create_issue".into()]))])
        }
        other => panic!("{other:?}"),
    }
    assert_error(
        &EXAMPLE.replace(
            "steps:\n",
            "mcp: [{ server: docs }, { server: docs }]\nsteps:\n",
        ),
        "10:25: mcp server `docs` is granted twice",
    );
    for (yaml, message) in [
        (
            "mcp: [{ server: docs }, { server: docs }]",
            "mcp server `docs` is granted twice",
        ),
        ("mcp: [{ server: sun }]", "`sun` is Sun's own MCP server"),
        ("mcp: [{ server: Docs }]", "invalid mcp server name `Docs`"),
    ] {
        let errors = errors(&EXAMPLE.replace("steps:\n", &format!("{yaml}\nsteps:\n")));
        assert!(errors.iter().any(|e| e.contains(message)), "{errors:#?}");
    }
    let errors = errors(&EXAMPLE.replace(
        "    use: github.open_pr",
        "    use: github.open_pr\n    mcp: []",
    ));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("`mcp` is not allowed on `action` steps")),
        "{errors:#?}"
    );
}

#[test]
fn watchdog_cron_and_send_back_defaults_are_parsed() {
    let base = parse(EXAMPLE).unwrap();
    assert_eq!(base.watchdog, None);
    let watched = EXAMPLE.replace(
        "steps:\n",
        "watchdog: { stall_after: 5m, on_stall: restart }\nsteps:\n",
    );
    let wf = parse(&watched).unwrap();
    assert_eq!(
        wf.watchdog,
        Some(WatchdogPolicy {
            stall_after_secs: Some(300),
            on_stall: Some(StallPolicy::Restart),
        })
    );
    assert_ne!(wf.hash, base.hash);
    let bad = errors(&EXAMPLE.replace("steps:\n", "watchdog: { stall_after: never }\nsteps:\n"));
    assert!(
        bad.iter().any(|e| e.contains("invalid duration `never`")),
        "{bad:#?}"
    );

    let defaulted = parse(&EXAMPLE.replace(", max: 2 }", " }")).unwrap();
    assert_eq!(defaulted.hash, base.hash, "max defaults to 2");

    let cron = EXAMPLE.replace("on: [manual,", "on: [\"cron:*/30 9-17 * * 1-5\",");
    assert!(parse(&cron).is_ok());
    let errors = errors(&EXAMPLE.replace("on: [manual,", "on: [\"cron:61 * * * *\","));
    assert!(
        errors.iter().any(|e| e.contains("bad minute `61`")),
        "{errors:#?}"
    );
}

/// A chain entry is a name or `{tool, credential, host, model, effort}`;
/// both shapes come back out of the parse as they were written, so a
/// committed YAML chain round-trips unchanged, and a string-only chain
/// keeps its hash.
#[test]
fn structured_chain_entries_parse_and_round_trip() {
    let step = "steps: [{id: a, type: agent, prompt: p}]\n";
    let yaml = format!(
        "name: wf\nruntime:\n  chain:\n    - claude-code\n    - {{tool: opencode, credential: ollama-qwen, model: \"qwen2.5-coder:7b\"}}\n    - {{tool: codex, host: 01JHOSTID, model: gpt-5-codex, effort: high}}\n{step}"
    );
    let chain = parse(&yaml).unwrap().runtime.unwrap().chain;
    let names: Vec<&str> = chain.iter().map(|e| e.name()).collect();
    assert_eq!(names, ["claude-code", "ollama-qwen", "codex"]);
    let json = serde_json::to_value(&chain).unwrap();
    assert_eq!(
        json,
        serde_json::json!([
            "claude-code",
            {"tool": "opencode", "credential": "ollama-qwen", "model": "qwen2.5-coder:7b"},
            {"tool": "codex", "host": "01JHOSTID", "model": "gpt-5-codex", "effort": "high"},
        ])
    );
    // The same chain written back as YAML parses to the same chain.
    let again = format!(
        "name: wf\nruntime:\n  chain: {}\n{step}",
        serde_json::to_string(&chain).unwrap()
    );
    assert_eq!(parse(&again).unwrap().runtime.unwrap().chain, chain);
    // A string-only chain hashes as it did before entries had a shape.
    let plain = format!("name: wf\nruntime: {{chain: [claude-code, codex]}}\n{step}");
    let bare = parse(&plain).unwrap();
    assert_eq!(
        serde_json::to_value(&bare.runtime.as_ref().unwrap().chain).unwrap(),
        serde_json::json!(["claude-code", "codex"])
    );
}

#[test]
fn structured_chain_entries_are_checked() {
    let step = "steps: [{id: a, type: agent, prompt: p}]\n";
    let chain = |entry: &str| format!("name: wf\nruntime: {{chain: [{entry}]}}\n{step}");
    assert_error(
        &chain("{tool: codex, permission_mode: plan}"),
        "2:33: unknown field `permission_mode`, expected one of tool, credential",
    );
    assert_error(&chain("{model: opus}"), "2:20: missing field `tool`");
    assert_error(&chain("{tool: Codex}"), "2:19: invalid `tool` `Codex`");
    assert_error(
        &chain("{tool: codex, effort: \"--yolo\"}"),
        "2:19: invalid `effort` `--yolo`",
    );
    assert_error(
        &chain("{tool: claude-code, model: \"opus; rm -rf /\"}"),
        "2:19: invalid `model`",
    );
    assert_error(
        &chain("{tool: opencode, credential: Bad Key}"),
        "2:19: invalid `credential`",
    );
    assert_error(
        &chain("codex, {tool: codex, effort: high}"),
        "2:26: runtime `codex` appears twice",
    );
    assert_error(
        &chain("ollama-qwen, {tool: opencode, credential: ollama-qwen}"),
        "2:32: runtime `ollama-qwen` appears twice",
    );
}
