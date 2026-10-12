//! Hybrid retrieval: opt-in int8 vectors in `chunk_vec` scored by
//! sqlite-vec, RRF with BM25, per-task source scope, cited P4 for `compile`.
#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_context::knowledge::{
    DocContent, Embedder, HttpEmbedder, Scope, SourceDoc, add_source, embed_pending, ingest,
    retrieve_for_step, search,
};
use sun_context::{LedgerView, compile};
use sun_core::{StepBrief, StepKey, TaskId};
use sun_store::{KnowledgeRepo, Store};

/// Words of one concept share a dimension, across languages.
const CONCEPTS: &[&[&str]] = &[
    &["price", "pricing", "harga", "biaya", "cost", "tarif"],
    &["deploy", "deployment", "rilis", "release"],
    &["webhook", "retry", "retries"],
];

fn concept_vector(text: &str) -> Vec<f32> {
    let mut v = vec![0.0; CONCEPTS.len() + 1];
    v[CONCEPTS.len()] = 0.05; // never a zero vector
    for word in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        for (dim, group) in CONCEPTS.iter().enumerate() {
            if group.contains(&word) {
                v[dim] += 1.0;
            }
        }
    }
    v
}

/// A local test double: no network.
struct Concepts {
    model: &'static str,
    calls: AtomicUsize,
}

impl Concepts {
    fn new(model: &'static str) -> Self {
        Self {
            model,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Embedder for Concepts {
    fn model(&self) -> &str {
        self.model
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, sun_core::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(texts.iter().map(|t| concept_vector(t)).collect())
    }
}

fn doc(external_id: &str, title: &str, body: &str) -> SourceDoc {
    SourceDoc {
        external_id: external_id.into(),
        title: title.into(),
        version: "v3".into(),
        updated_at: Some("2026-10-01T08:00:00Z".parse().unwrap()),
        url: Some(format!("https://example.test/{external_id}")),
        acl: Vec::new(),
        content: DocContent::Markdown(body.into()),
    }
}

async fn seeded() -> (tempfile::TempDir, Store, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let team = add_source(&store, "upload", "team", json!({}))
        .await
        .unwrap();
    let legacy = add_source(&store, "upload", "legacy", json!({}))
        .await
        .unwrap();
    let docs = [
        (
            &team,
            doc(
                "tarif.md",
                "Tarif",
                "# Tarif\n\nBiaya langganan Rp 50.000 per pengguna.",
            ),
        ),
        (
            &team,
            doc(
                "rilis.md",
                "Rilis",
                "# Rilis\n\nRilis ke produksi setiap Jumat.",
            ),
        ),
        (
            &legacy,
            doc("lama.md", "Tarif lama", "# Lama\n\nHarga lama Rp 40.000."),
        ),
    ];
    for (source, d) in docs {
        ingest(&store, source, d).await.unwrap();
    }
    (dir, store, team, legacy)
}

#[tokio::test]
async fn retrieval_vectors_are_opt_in_scoped_and_cited() {
    let (_dir, store, team, legacy) = seeded().await;
    let embedder = Concepts::new("fake:concepts");

    // No embedder: BM25 only, nothing embedded, no lexical match for English.
    assert!(
        search(&store, "pricing", Scope::default(), 5)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(embedder.calls.load(Ordering::SeqCst), 0);

    // Opt in: embed the backlog in batches until done.
    assert_eq!(embed_pending(&store, &embedder, 2).await.unwrap(), 2);
    assert_eq!(embed_pending(&store, &embedder, 2).await.unwrap(), 1);
    assert_eq!(embed_pending(&store, &embedder, 2).await.unwrap(), 0);

    // Cross-language: "pricing" finds "Biaya"/"Harga" by vector, cited.
    let all = Scope {
        sources: None,
        embedder: Some(&embedder),
    };
    let hits = search(&store, "pricing", all, 5).await.unwrap();
    let top: Vec<&str> = hits[..2].iter().map(|h| h.citation.as_str()).collect();
    assert!(top.contains(&"Tarif > Tarif · v3 · 2026-10-01 · https://example.test/tarif.md"));
    assert!(top.contains(&"Tarif lama > Lama · v3 · 2026-10-01 · https://example.test/lama.md"));
    assert!(hits[0].pointer.starts_with("doc:"));

    // The task's selected sources scope vectors and BM25 alike.
    let team_only = [team];
    let scoped = Scope {
        sources: Some(&team_only),
        embedder: Some(&embedder),
    };
    let hits = search(&store, "pricing", scoped, 5).await.unwrap();
    assert!(hits[0].citation.starts_with("Tarif > Tarif"));
    assert!(hits.iter().all(|h| !h.citation.starts_with("Tarif lama")));
    let legacy_only = [legacy];
    let lexical = |sources| Scope {
        sources: Some(sources),
        embedder: None,
    };
    assert!(
        search(&store, "rilis", lexical(&legacy_only), 5)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        search(&store, "rilis", lexical(&team_only), 5)
            .await
            .unwrap()
            .len(),
        1
    );

    // RRF: a chunk found by both BM25 and vectors outranks vector-only ones.
    let hits = search(&store, "harga", all, 5).await.unwrap();
    assert!(hits[0].citation.starts_with("Tarif lama"));
}

#[tokio::test]
async fn retrieval_vectors_follow_reindex_and_model() {
    let (_dir, store, team, _) = seeded().await;
    let v1 = Concepts::new("fake:v1");
    while embed_pending(&store, &v1, 10).await.unwrap() > 0 {}

    // A changed doc drops its old vectors (cascade) and is re-embedded.
    let pending = |model: &'static str| {
        let store = store.clone();
        async move {
            let mut tx = store.read().await.unwrap();
            tx.chunks_without_vector(model, 100).await.unwrap()
        }
    };
    assert!(pending("fake:v1").await.is_empty());
    let changed = doc("tarif.md", "Tarif", "# Tarif\n\nBiaya naik jadi Rp 60.000.");
    ingest(&store, &team, changed.clone()).await.unwrap();
    let stale = pending("fake:v1").await;
    assert_eq!(stale.len(), 1);

    // A vector for a chunk replaced while it was being embedded is skipped.
    ingest(
        &store,
        &team,
        doc("tarif.md", "Tarif", "# Tarif\n\nBiaya Rp 70.000."),
    )
    .await
    .unwrap();
    let mut tx = store.begin().await.unwrap();
    let vector = vec![(stale[0].clone(), vec![1_i8, 0, 0, 0])];
    assert_eq!(tx.put_chunk_vectors("fake:v1", &vector).await.unwrap(), 0);
    tx.commit().await.unwrap();
    assert_eq!(embed_pending(&store, &v1, 10).await.unwrap(), 1);

    // Another model never reads v1's vectors: lexical hits only until embedded.
    let v2 = Concepts::new("fake:v2");
    assert_eq!(pending("fake:v2").await.len(), 3);
    let scope = Scope {
        sources: None,
        embedder: Some(&v2),
    };
    assert!(
        search(&store, "pricing", scope, 5)
            .await
            .unwrap()
            .is_empty()
    );
    while embed_pending(&store, &v2, 10).await.unwrap() > 0 {}
    assert!(
        !search(&store, "pricing", scope, 5)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn retrieval_http_embedders_speak_openai_and_ollama() {
    // OpenAI-compatible: bearer key, `dimensions`, replies re-ordered by index.
    let seen = Arc::new(Mutex::new(Vec::<(Option<String>, Value)>::new()));
    let log = seen.clone();
    let (url, _) = sun_mod_sdk::http::mock("/v1", move |r| {
        let auth = r.header("authorization").map(str::to_owned);
        log.lock().unwrap().push((auth, r.body.clone()));
        match (r.target.as_str(), r.body["input"][0].as_str()) {
            (_, Some("busy")) => (503, json!({ "error": "later" })),
            (_, Some("refused")) => (401, json!({ "error": "bad key sk-test" })),
            ("/embeddings", _) => (
                200,
                json!({ "data": [
                    { "index": 1, "embedding": [0.0, 1.0] },
                    { "index": 0, "embedding": [1.0, 0.0] },
                ] }),
            ),
            _ => (404, json!({})),
        }
    })
    .unwrap();
    let openai = HttpEmbedder::openai(
        &url,
        Some("sk-test".into()),
        "text-embedding-3-small",
        Some(768),
    )
    .unwrap();
    assert_eq!(openai.model(), "openai:text-embedding-3-small@768");
    assert!(!format!("{openai:?}").contains("sk-test"));
    let vectors = openai.embed(&["a".into(), "b".into()]).await.unwrap();
    assert_eq!(vectors, [vec![1.0, 0.0], vec![0.0, 1.0]]);
    assert_eq!(
        seen.lock().unwrap()[0],
        (
            Some("Bearer sk-test".into()),
            json!({ "model": "text-embedding-3-small", "input": ["a", "b"], "dimensions": 768 })
        )
    );
    let busy = openai.embed(&["busy".into()]).await.unwrap_err();
    assert!(matches!(busy, sun_core::Error::Unavailable(_)), "{busy}");
    let refused = openai.embed(&["refused".into()]).await.unwrap_err();
    assert!(matches!(refused, sun_core::Error::InvalidInput(_)));
    assert!(!refused.to_string().contains("sk-test"));
    // A key never goes out over plain http to a remote host.
    assert!(HttpEmbedder::openai("http://example.com/v1", Some("k".into()), "m", None).is_err());

    // Ollama end to end: HTTP → int8 → sqlite-vec → RRF.
    let (url, log) = sun_mod_sdk::http::mock("", |r| {
        assert_eq!(r.header("authorization"), None);
        let input = r.body["input"].as_array().cloned().unwrap_or_default();
        let vectors: Vec<Vec<f32>> = input
            .iter()
            .map(|t| concept_vector(t.as_str().unwrap_or_default()))
            .collect();
        (200, json!({ "embeddings": vectors }))
    })
    .unwrap();
    let ollama = HttpEmbedder::ollama(&url, "bge-m3").unwrap();
    assert_eq!(ollama.model(), "ollama:bge-m3");
    let (_dir, store, _, _) = seeded().await;
    assert_eq!(embed_pending(&store, &ollama, 10).await.unwrap(), 3);
    let scope = Scope {
        sources: None,
        embedder: Some(&ollama),
    };
    let hits = search(&store, "deployment", scope, 1).await.unwrap();
    assert!(hits[0].citation.starts_with("Rilis > Rilis · v3"));
    assert!(log.lock().unwrap().iter().all(|l| l == "POST /api/embed"));
}

#[tokio::test]
async fn retrieval_step_hits_fill_p4_within_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let source = add_source(&store, "upload", "ops", json!({}))
        .await
        .unwrap();
    let filler = "lorem ipsum dolor sit amet. ".repeat(70);
    let runbook = format!(
        "# Webhooks\n\n## Retry\n\nWebhook retry uses backoff.\n\n{filler}\n\n\
         ## Timeouts\n\nWebhook timeouts are 10 s.\n\n{filler}\n"
    );
    ingest(&store, &source, doc("runbook.md", "Runbook", &runbook))
        .await
        .unwrap();
    ingest(
        &store,
        &source,
        doc(
            "adr.md",
            "ADR 7",
            "# Queue\n\nWebhook deliveries queue in SQLite.",
        ),
    )
    .await
    .unwrap();
    let step = |budget| StepBrief {
        task_id: TaskId::generate(),
        title: "Add retry to webhook".into(),
        body: "Webhooks fail on timeouts.".into(),
        step_key: StepKey::new("implement").unwrap(),
        goal: "Retry failed webhook deliveries".into(),
        constraints: Vec::new(),
        gates: Vec::new(),
        acceptance: Vec::new(),
        context_budget_tokens: Some(budget),
    };

    // Query = step goal + task title; one chunk per doc (dedupe).
    let hits = retrieve_for_step(&store, &step(4000), "", Scope::default(), 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_ne!(hits[0].pointer, hits[1].pointer);

    // P4 within budget: all fit at 4k; at 80 tokens only P0 + pointers.
    let roomy = compile(&step(4000), &LedgerView::default(), &[], &hits, 200_000);
    assert!(roomy.summary().contains("2 docs · 0 trimmed"));
    assert!(
        roomy
            .render_markdown()
            .contains("## Source: ADR 7 > Queue · v3")
    );
    let tight = compile(&step(80), &LedgerView::default(), &[], &hits, 200_000);
    let p4: Vec<_> = tight.items.iter().filter(|i| i.priority == 4).collect();
    assert_eq!(p4.len(), 2);
    assert!(p4.iter().all(|i| !i.included));
    assert!(tight.render_markdown().contains("## Pointers"));
}
