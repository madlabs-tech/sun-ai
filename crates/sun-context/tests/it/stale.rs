//! Stale and conflicting source flags and connector
//! tombstones: flags ride on the hits, land in the pack as P0 warnings with
//! citations, and retired docs leave the index.
#![allow(clippy::unwrap_used)]

use chrono::{Duration, Utc};
use serde_json::json;
use sun_context::knowledge::{
    DocContent, FlagKind, Ingested, Scope, SourceDoc, add_source, ingest, retire, retrieve, search,
    unique_flags,
};
use sun_context::{LedgerView, compile};
use sun_core::{StepBrief, StepKey, TaskId};
use sun_store::{KnowledgeRepo, Store};

fn doc(external_id: &str, title: &str, days_old: i64, body: &str) -> SourceDoc {
    SourceDoc {
        external_id: external_id.into(),
        title: title.into(),
        version: "v1".into(),
        updated_at: Some(Utc::now() - Duration::days(days_old)),
        url: None,
        acl: Vec::new(),
        content: DocContent::Markdown(body.into()),
    }
}

const QUERY: &str = "pricing plan per month";

#[tokio::test]
async fn stale_and_conflicting_sources_are_flagged_with_citations() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let product = add_source(&store, "upload", "product", json!({}))
        .await
        .unwrap();
    let sales = add_source(&store, "upload", "sales", json!({"max_age_days": 30}))
        .await
        .unwrap();
    let policy = add_source(&store, "upload", "policy", json!({"max_age_days": 0}))
        .await
        .unwrap();
    for bad in [json!({"max_age_days": -1}), json!({"max_age_days": "90"})] {
        assert!(add_source(&store, "upload", "bad", bad).await.is_err());
    }

    // Older than the 180-day default → stale.
    let prd = "# PRD\n\n## Pricing\n\nThe plan costs IDR 99.000 per month from 2026-01-01.\n";
    ingest(&store, &product, doc("prd.md", "PRD", 200, prd))
        .await
        .unwrap();
    // Fresh, agrees with the PRD.
    let faq = "# FAQ\n\n## Pricing\n\nThe plan is IDR 99.000 per month from 2026-01-01.\n";
    ingest(&store, &product, doc("faq.md", "FAQ", 5, faq))
        .await
        .unwrap();
    // Older than its source's 30 days → stale; disagrees on the price.
    let deck =
        "# Sales deck\n\n## Pricing\n\nThe plan costs IDR 120.000 per month from 2026-01-01.\n";
    ingest(&store, &sales, doc("deck.md", "Sales deck", 60, deck))
        .await
        .unwrap();
    // Ancient, but its source never goes stale; other heading → no conflict.
    let terms = "# Terms\n\n## Billing\n\nThe plan is billed per month, 30 days notice.\n";
    ingest(&store, &policy, doc("terms.md", "Terms", 1000, terms))
        .await
        .unwrap();

    let hits = retrieve(&store, QUERY, Scope::default(), 10).await.unwrap();
    assert_eq!(hits.len(), 4);
    let flags = unique_flags(&hits);
    let stale: Vec<_> = flags.iter().filter(|f| f.kind == FlagKind::Stale).collect();
    let mut stale_titles: Vec<&str> = stale
        .iter()
        .map(|f| f.citations[0].split(" > ").next().unwrap())
        .collect();
    stale_titles.sort_unstable();
    assert_eq!(stale_titles, ["PRD", "Sales deck"]);
    assert!(stale.iter().any(|f| f.message.contains("limit is 30 days")));
    assert!(
        stale
            .iter()
            .any(|f| f.message.contains("limit is 180 days"))
    );

    let conflicts: Vec<_> = flags
        .iter()
        .filter(|f| f.kind == FlagKind::Conflict)
        .collect();
    assert_eq!(conflicts.len(), 1, "{flags:#?}");
    let conflict = conflicts[0];
    assert_eq!(conflict.citations.len(), 3);
    assert!(
        conflict
            .citations
            .iter()
            .any(|c| c.starts_with("Sales deck > Sales deck > Pricing · v1 · "))
    );
    assert!(conflict.message.contains("Sales deck says 120.000"));
    assert!(conflict.message.contains("PRD says 99.000"));
    assert!(
        !conflict.message.contains("2026-01-01"),
        "shared values are not quoted"
    );
    // Each hit carries the flags it is part of; the Terms doc has none.
    let terms_hit = hits
        .iter()
        .find(|h| h.citation.starts_with("Terms"))
        .unwrap();
    assert!(terms_hit.flags.is_empty());
    // Plain search stays unflagged.
    assert!(
        search(&store, QUERY, Scope::default(), 5)
            .await
            .unwrap()
            .iter()
            .all(|h| h.flags.is_empty())
    );

    // The pack: a P0 warnings item after the task, kept even when the budget
    // trims every source, quoted in the first prompt with citations.
    let step = StepBrief {
        task_id: TaskId::generate(),
        title: "Update the pricing page".into(),
        body: String::new(),
        step_key: StepKey::new("implement").unwrap(),
        goal: "Show the plan price".into(),
        constraints: Vec::new(),
        gates: Vec::new(),
        acceptance: Vec::new(),
        context_budget_tokens: Some(60),
    };
    let pack = compile(&step, &LedgerView::default(), &[], &hits, 200_000);
    let warnings = &pack.items[1];
    assert_eq!(
        (warnings.priority, warnings.label.as_str()),
        (0, "Knowledge warnings")
    );
    assert!(warnings.included);
    assert!(
        pack.items
            .iter()
            .filter(|i| i.priority == 4)
            .all(|i| !i.included)
    );
    let envelope = pack.envelope();
    assert!(envelope.contains("Don't pick one silently"));
    assert!(envelope.contains("- **Conflict:** Sources disagree on \"Pricing\""));
    assert!(envelope.contains("  - PRD > PRD > Pricing · v1 · "));
    assert!(pack.render_markdown().contains("## Knowledge warnings"));

    // No flags → no warnings item.
    let clean = retrieve(
        &store,
        "billed notice",
        Scope {
            sources: Some(&[policy]),
            ..Scope::default()
        },
        10,
    )
    .await
    .unwrap();
    assert_eq!(clean.len(), 1);
    let pack = compile(&step, &LedgerView::default(), &[], &clean, 200_000);
    assert!(pack.items.iter().all(|i| i.label != "Knowledge warnings"));
}

#[tokio::test]
async fn stale_tombstones_retire_chunks_and_revive_on_ingest() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let product = add_source(&store, "upload", "product", json!({}))
        .await
        .unwrap();
    let sales = add_source(&store, "upload", "sales", json!({}))
        .await
        .unwrap();
    let prd = doc(
        "prd.md",
        "PRD",
        1,
        "# PRD\n\n## Pricing\n\nThe plan costs IDR 99.000 per month.\n",
    );
    let Ingested::Indexed { doc_id, .. } = ingest(&store, &product, prd.clone()).await.unwrap()
    else {
        panic!("indexed");
    };
    let deck = "# Deck\n\n## Pricing\n\nThe plan costs IDR 120.000 per month.\n";
    ingest(&store, &sales, doc("deck.md", "Deck", 1, deck))
        .await
        .unwrap();
    let hits = retrieve(&store, QUERY, Scope::default(), 10).await.unwrap();
    assert_eq!(unique_flags(&hits).len(), 1, "a conflict while both exist");

    // The connector reports prd.md deleted: its chunks leave every index.
    let gone = vec!["prd.md".to_owned(), "never-seen.md".to_owned()];
    assert_eq!(retire(&store, &product, &gone).await.unwrap(), 1);
    assert_eq!(
        retire(&store, &product, &gone).await.unwrap(),
        0,
        "idempotent"
    );
    assert!(
        store
            .read()
            .await
            .unwrap()
            .doc_chunks(&doc_id)
            .await
            .unwrap()
            .is_empty()
    );
    let hits = retrieve(&store, QUERY, Scope::default(), 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].citation.starts_with("Deck"));
    assert!(
        unique_flags(&hits).is_empty(),
        "no conflict with a deleted doc"
    );
    assert!(retire(&store, &product, &[String::new()]).await.is_err());

    // Restored at the source with the same content: re-indexed under the same id.
    assert_eq!(
        ingest(&store, &product, prd).await.unwrap(),
        Ingested::Indexed {
            doc_id: doc_id.clone(),
            chunks: 1
        }
    );
    let hits = retrieve(&store, QUERY, Scope::default(), 10).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().any(|h| h.pointer == format!("doc:{doc_id}")));
}
