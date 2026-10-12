//! Knowledge pipeline end to end: doc in → normalize → chunks with heading
//! path → `chunk_fts` (unicode61 + trigram) → BM25 search with citations.
#![allow(clippy::unwrap_used)]

use sun_context::knowledge::{DocContent, Ingested, Scope, SourceDoc, ingest, search};
use sun_core::model::SecretScope;
use sun_store::{KnowledgeRepo, SecretRepo, Store};

#[tokio::test]
async fn knowledge_stored_secret_never_lands_in_chunks_or_fts() {
    // Shaped like no known token: only the secret-store pass can catch it.
    const VALUE: &str = "correct-horse-battery-staple-42";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    tx.set_secret(&SecretScope::Mod("jira".into()), "JIRA_TOKEN", VALUE)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let source = sun_context::knowledge::add_source(&store, "upload", "ops", serde_json::json!({}))
        .await
        .unwrap();

    let mut doc = prd(&format!("Log in to Jira with {VALUE} first."));
    doc.title = format!("Runbook {VALUE}");
    doc.url = Some(format!("https://jira.test/?t={VALUE}"));
    let Ingested::Indexed { doc_id, .. } = ingest(&store, &source, doc).await.unwrap() else {
        panic!("not indexed")
    };
    let chunks = store
        .read()
        .await
        .unwrap()
        .doc_chunks(&doc_id)
        .await
        .unwrap();
    assert!(
        chunks[0]
            .body
            .contains("Log in to Jira with [REDACTED] first.")
    );
    assert!(chunks.iter().all(|c| !c.body.contains(VALUE)));
    // Neither FTS index (words, trigram) holds any part of it; nor do citations.
    for query in [VALUE, "horse battery staple", "battery"] {
        assert!(
            search(&store, query, Scope::default(), 50)
                .await
                .unwrap()
                .is_empty(),
            "{query}"
        );
    }
    let hits = search(&store, "jira", Scope::default(), 5).await.unwrap();
    assert!(!hits.is_empty() && hits.iter().all(|h| !h.citation.contains(VALUE)));
}

/// About `tokens` tokens of filler, so a section stays its own chunk.
fn filler(tokens: usize) -> String {
    "lorem ipsum dolor sit amet. ".repeat(tokens * 4 / 28)
}

fn prd(pricing: &str) -> SourceDoc {
    let markdown = format!(
        "# PRD\n\n## Pricing\n\n### IDR\n\n{pricing}\n\nAPI_KEY=supersecretvalue123\n\n{}\n\n\
         ## Pembayaran\n\nPembayaran lewat transfer bank.\n\n{}\n",
        filler(450),
        filler(450),
    );
    SourceDoc {
        external_id: "docs/prd.md".into(),
        title: "Billing PRD".into(),
        version: "v3".into(),
        updated_at: Some("2026-10-01T08:00:00Z".parse().unwrap()),
        url: Some("https://example.test/prd".into()),
        acl: Vec::new(),
        content: DocContent::Markdown(markdown),
    }
}

#[tokio::test]
async fn knowledge_ingest_index_and_search_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let source = sun_context::knowledge::add_source(
        &store,
        "repo_markdown",
        "billing repo",
        serde_json::json!({}),
    )
    .await
    .unwrap();

    // Doc in → chunks with heading paths, secrets redacted before storage.
    let Ingested::Indexed { doc_id, chunks } = ingest(
        &store,
        &source,
        prd("Harga langganan Rp 50.000 per pengguna."),
    )
    .await
    .unwrap() else {
        panic!("not indexed")
    };
    assert_eq!(chunks, 2);
    let stored = store
        .read()
        .await
        .unwrap()
        .doc_chunks(&doc_id)
        .await
        .unwrap();
    let paths: Vec<&str> = stored.iter().map(|c| c.heading_path.as_str()).collect();
    assert_eq!(paths, ["PRD > Pricing > IDR", "PRD > Pembayaran"]);
    assert!(stored[0].body.contains("API_KEY=[REDACTED]"));
    assert!(stored.iter().all(|c| !c.body.contains("supersecret")));

    // BM25 on words returns the cited chunk.
    let hits = search(&store, "harga langganan", Scope::default(), 5)
        .await
        .unwrap();
    assert_eq!(
        hits[0].citation,
        "Billing PRD > PRD > Pricing > IDR · v3 · 2026-10-01 · https://example.test/prd"
    );
    assert_eq!(hits[0].pointer, format!("doc:{doc_id}"));
    assert!(hits[0].text.contains("Rp 50.000"));
    // Trigram finds the Bahasa root inside an affixed word.
    let hits = search(&store, "bayar", Scope::default(), 5).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].citation.contains("PRD > Pembayaran"));
    // Redacted text is not searchable; FTS syntax in a query is inert.
    assert!(
        search(&store, "supersecretvalue123", Scope::default(), 5)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        search(&store, "\"NEAR( * :", Scope::default(), 5)
            .await
            .unwrap()
            .is_empty()
    );

    // Same doc again: version-hash skip.
    assert_eq!(
        ingest(
            &store,
            &source,
            prd("Harga langganan Rp 50.000 per pengguna.")
        )
        .await
        .unwrap(),
        Ingested::Unchanged {
            doc_id: doc_id.clone()
        }
    );
    // Changed doc: same id, chunks and index replaced.
    let Ingested::Indexed { doc_id: again, .. } =
        ingest(&store, &source, prd("Harga baru Rp 75.000."))
            .await
            .unwrap()
    else {
        panic!("not re-indexed")
    };
    assert_eq!(again, doc_id);
    assert!(
        search(&store, "langganan", Scope::default(), 5)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        search(&store, "75.000", Scope::default(), 5).await.unwrap()[0]
            .text
            .contains("Harga baru")
    );

    // Denylist: nothing stored, nothing found.
    let mut env = prd("x");
    env.external_id = "app/.env".into();
    env.content = DocContent::Markdown("DATABASE_URL=postgres://u:p@db/zebra".into());
    assert!(matches!(
        ingest(&store, &source, env).await.unwrap(),
        Ingested::Denied { .. }
    ));
    assert!(
        search(&store, "zebra", Scope::default(), 5)
            .await
            .unwrap()
            .is_empty()
    );

    // Sheets stay tables: a hit carries the header row.
    let sheet = SourceDoc {
        external_id: "sheets/plans".into(),
        title: "Plans".into(),
        version: "rev 7".into(),
        updated_at: None,
        url: None,
        acl: Vec::new(),
        content: DocContent::Sheet {
            headers: vec!["plan".into(), "price_idr".into()],
            rows: vec![
                vec!["starter".into(), "50000".into()],
                vec!["enterprise".into(), "900000".into()],
            ],
        },
    };
    ingest(&store, &source, sheet).await.unwrap();
    let hits = search(&store, "enterprise", Scope::default(), 5)
        .await
        .unwrap();
    assert_eq!(hits[0].citation, "Plans > rows 1-2 · rev 7");
    assert!(
        hits[0]
            .text
            .starts_with("| plan | price_idr |\n| --- | --- |\n")
    );

    // Bad connector input is refused at the boundary.
    let mut bad = prd("x");
    bad.external_id = " ".into();
    assert!(ingest(&store, &source, bad).await.is_err());
}

#[tokio::test]
async fn knowledge_secret_set_later_is_swept_from_indexed_chunks() {
    const VALUE: &str = "late-arriving-secret-value-77";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let source = sun_context::knowledge::add_source(&store, "upload", "ops", serde_json::json!({}))
        .await
        .unwrap();
    // A mod's `sync` output: JSON, with fields the kernel ignores (`meta`).
    let doc: SourceDoc = serde_json::from_value(serde_json::json!({
        "external_id": "PROJ-1",
        "title": "Deploy",
        "version": "7",
        "updated_at": "2026-10-01T08:00:00.000Z",
        "content": { "markdown": format!("Deploy with {VALUE} today.") },
        "meta": { "status": "Done" },
    }))
    .unwrap();
    let Ingested::Indexed { doc_id, .. } = ingest(&store, &source, doc.clone()).await.unwrap()
    else {
        panic!("not indexed")
    };
    let found = |q: &'static str| {
        let store = store.clone();
        async move { search(&store, q, Scope::default(), 5).await.unwrap().len() }
    };
    assert_eq!(found("arriving").await, 1);

    let mut tx = store.begin().await.unwrap();
    tx.set_secret(&SecretScope::Mod("jira".into()), "JIRA_TOKEN", VALUE)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let sweep = sun_context::knowledge::redact_indexed;
    assert_eq!(sweep(&store, VALUE).await.unwrap(), 1);
    let chunks = store
        .read()
        .await
        .unwrap()
        .doc_chunks(&doc_id)
        .await
        .unwrap();
    assert!(chunks[0].body.contains("Deploy with [REDACTED] today."));
    assert_eq!(found("arriving").await, 0);
    // The same doc re-synced is still a hash skip, so the redaction stays.
    assert!(matches!(
        ingest(&store, &source, doc).await.unwrap(),
        Ingested::Unchanged { .. }
    ));
    assert_eq!(sweep(&store, VALUE).await.unwrap(), 0);
}
