//! The evaluation harness over the synthetic corpus and gold set: lexical and fake-embedder
//! hybrid modes meet their recall and latency floors and the default pointer gate points
//! related rows at their files and unrelated rows nowhere; hybrid search and passive pointers
//! work through the public API. All people, places and texts are fictional.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use ulei_garden::api::{ApiConfig, Garden, Hybrid};
use ulei_garden::embed::{Embedder, HashEmbedder};
use ulei_garden::eval::{self, Corpus, GoldRow};
use ulei_garden::miss::ProducerKey;
use ulei_garden::model::FactId;
use ulei_garden::rerank::NoReranker;
use ulei_garden::search::{PointerGate, SearchParams, Searcher, index_missing, pointers};
use ulei_garden::store::Store;
use ulei_garden::wire::c7;

const NOW: i64 = 1_800_000_000;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn setup(embedder: Option<&dyn Embedder>) -> (Store, Corpus, Vec<GoldRow>) {
    let root = fixtures().join("garden-corpus");
    let files = eval::markdown_files(&root).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let corpus = eval::ingest(&mut store, &root, &files, 1200, 0).unwrap();
    if let Some(e) = embedder {
        let n = index_missing(&mut store, e, 8).unwrap();
        assert_eq!(n, corpus.fact_paths.len());
        assert_eq!(index_missing(&mut store, e, 8).unwrap(), 0, "idempotent");
    }
    let gold_text = std::fs::read_to_string(fixtures().join("gold-synthetic.jsonl")).unwrap();
    (store, corpus, eval::parse_gold(&gold_text).unwrap())
}

#[test]
fn fixture_shape() {
    let (_, corpus, gold) = setup(None);
    assert_eq!(corpus.files, 15, "INDEX.md is not ingested");
    assert!(gold.len() >= 30);
    let cats: std::collections::BTreeSet<&str> = gold.iter().map(|r| r.category.as_str()).collect();
    assert_eq!(
        cats.len(),
        9,
        "eight scored categories and the unrelated rows"
    );
    assert_eq!(gold.iter().filter(|r| r.unrelated()).count(), 10);
    assert!(gold.iter().filter(|r| r.scored()).count() >= 30);
}

#[test]
fn lexical_mode_meets_floor() {
    let (store, corpus, gold) = setup(None);
    let out = eval::evaluate(
        &store,
        None,
        &corpus,
        &gold,
        &SearchParams::at(NOW),
        &PointerGate::default(),
    )
    .unwrap();
    let m = &out.lexical;
    assert_eq!(m.n, 31);
    assert!(out.hybrid.is_none());
    assert!(m.recall >= LEXICAL_RECALL_FLOOR, "recall@3 {}", m.recall);
    assert!(m.mrr >= MRR_FLOOR, "mrr {}", m.mrr);
    assert!(
        m.latency_percentile(0.5) < P50_CEILING_MS,
        "p50 {}",
        m.latency_percentile(0.5)
    );
}

#[test]
fn fake_hybrid_mode_meets_floor() {
    let e = HashEmbedder::new(256);
    let (store, corpus, gold) = setup(Some(&e));
    let out = eval::evaluate(
        &store,
        Some(&e),
        &corpus,
        &gold,
        &SearchParams::at(NOW),
        &PointerGate::default(),
    )
    .unwrap();
    let h = out.hybrid.unwrap();
    assert_eq!(h.n, 31);
    assert!(h.recall >= HYBRID_RECALL_FLOOR, "recall@3 {}", h.recall);
    assert!(h.mrr >= MRR_FLOOR && h.mrr <= 1.0, "mrr {}", h.mrr);
    assert!(
        h.latency_percentile(0.5) < P50_CEILING_MS,
        "p50 {}",
        h.latency_percentile(0.5)
    );
    let json = h.to_json(3);
    assert_eq!(json["per_category"].as_object().unwrap().len(), 8);
}

#[test]
fn hybrid_search_ranks_the_matching_fact_first() {
    let e = HashEmbedder::new(256);
    let (store, corpus, _) = setup(Some(&e));
    let s = Searcher::new(&store, Some(&e), &NoReranker);
    let hits = s
        .search("Linden Lane Dental opening hours", &SearchParams::at(NOW))
        .unwrap();
    assert!(!hits.is_empty() && hits.len() <= 3);
    assert_eq!(
        corpus.fact_paths[hits[0].id.as_fact().unwrap()],
        "places/linden-dental.md"
    );
    assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
}

#[test]
fn aliases_count_for_lexical_matching() {
    let (store, corpus, _) = setup(None);
    let s = Searcher::new(&store, None, &NoReranker);
    // "Херон" appears only among the aliases of the Heron project.
    let hits = s.search("Херон", &SearchParams::at(NOW)).unwrap();
    assert_eq!(
        corpus.fact_paths[hits[0].id.as_fact().unwrap()],
        "work/project-heron.md"
    );
}

#[test]
fn search_sees_nothing_before_the_facts_were_recorded() {
    let (store, _, _) = setup(None);
    let s = Searcher::new(&store, None, &NoReranker);
    let mut p = SearchParams::at(NOW);
    p.known_at = -1;
    assert_eq!(s.search("passport", &p).unwrap(), Vec::new());
}

#[test]
fn pointers_name_entities_not_texts() {
    let e = HashEmbedder::new(256);
    let (store, _, _) = setup(Some(&e));
    let s = Searcher::new(&store, Some(&e), &NoReranker);
    let gate = PointerGate::default();
    let ps = pointers(&s, "where is my passport", &SearchParams::at(NOW), &gate).unwrap();
    assert!(!ps.is_empty() && ps.len() <= 3);
    assert!(ps[0].line.contains("Passport"), "{}", ps[0].line);
    assert!(!ps[0].line.contains("blue folder"));
    let strict = PointerGate {
        min_cosine: 2.0,
        min_lexical: f32::INFINITY,
        ..gate
    };
    let none = pointers(&s, "where is my passport", &SearchParams::at(NOW), &strict).unwrap();
    assert!(none.is_empty(), "the gate applies");
}

#[test]
fn unrelated_queries_get_no_pointers() {
    let e = HashEmbedder::new(256);
    let (store, _, _) = setup(Some(&e));
    let s = Searcher::new(&store, Some(&e), &NoReranker);
    for q in [
        "what is the capital of Mongolia",
        "xylophone quantum",
        "погода завтра в Лиме",
        "how do I cook rice",
    ] {
        let ps = pointers(&s, q, &SearchParams::at(NOW), &PointerGate::default()).unwrap();
        assert!(ps.is_empty(), "{q}: {ps:?}");
    }
    // The active search still ranks something: the gate is only on the passive tier.
    let q = "what is the capital of Mongolia";
    assert_ne!(s.search(q, &SearchParams::at(NOW)).unwrap().len(), 0);
}

#[test]
fn default_gate_points_related_rows_at_their_files_and_unrelated_rows_nowhere() {
    let e = HashEmbedder::new(256);
    let (store, corpus, gold) = setup(Some(&e));
    let gate = PointerGate::default();
    let out = eval::evaluate(
        &store,
        Some(&e),
        &corpus,
        &gold,
        &SearchParams::at(NOW),
        &gate,
    )
    .unwrap();
    let hybrid = out.hybrid.unwrap().pointers;
    let lexical = out.lexical.pointers;
    for m in [&hybrid, &lexical] {
        assert_eq!(
            (m.unrelated, m.unrelated_pointed, m.wrong),
            (10, 0, 0),
            "{m:?}"
        );
    }
    assert!(lexical.right >= LEXICAL_POINTED_FLOOR, "{lexical:?}");
    assert!(
        hybrid.right > lexical.right,
        "the dense side adds pointers: {hybrid:?}"
    );
}

#[test]
fn garden_pointers_with_the_fake_embedder_name_the_right_fact_and_nothing_for_unrelated() {
    let (store, corpus, _) = setup(Some(&HashEmbedder::new(256)));
    let g = Garden::new(
        store,
        Hybrid::new(Some(Box::new(HashEmbedder::new(256))), Box::new(NoReranker)),
        ApiConfig::default(),
        ProducerKey::from_bytes(&[0x5a; 32]).unwrap(),
    );
    let ask = |q: &str| {
        let req = c7::PointersRequest {
            q: q.into(),
            k: None,
            as_of: None,
            private: None,
        };
        g.garden_pointers(&req, NOW).unwrap().pointers
    };
    for (q, path) in [
        ("where is my passport", "docs/passport.md"),
        (
            "Linden Lane Dental opening hours on Saturday",
            "places/linden-dental.md",
        ),
        ("when is Ilsa's birthday", "people/ilsa-varn.md"),
        (
            "sourdough hydration and flour amount",
            "knowledge/sourdough.md",
        ),
    ] {
        let ps = ask(q);
        let paths: Vec<&str> = ps
            .iter()
            .map(|p| corpus.fact_paths[&FactId(p.fact_id.clone())].as_str())
            .collect();
        assert!(paths.contains(&path), "{q}: {paths:?}");
    }
    for q in [
        "what is the capital of Mongolia",
        "xylophone quantum",
        "погода завтра в Лиме",
        "how do I cook rice",
    ] {
        assert_eq!(ask(q), [], "{q}");
    }
}

// Measured on the fixture: lexical recall@3 0.97, MRR 0.97; fake hybrid recall@3 1.0, MRR 0.98;
// p50 about 2 ms in a debug build. The floors leave margin for slow CI hosts.
const LEXICAL_RECALL_FLOOR: f64 = 0.85;
const HYBRID_RECALL_FLOOR: f64 = 0.85;
const MRR_FLOOR: f64 = 0.8;
const P50_CEILING_MS: f64 = 50.0;
// The default gate gives 22 of the 31 scored rows a pointer to a gold file lexically.
const LEXICAL_POINTED_FLOOR: usize = 20;

#[test]
fn documents_sharing_a_name_stay_their_own_entities() {
    let root = std::env::temp_dir().join(format!("ulei-garden-same-name-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    let docs = [
        (
            "a/notes.md",
            "---\nname: Cafe Moss\n---\nCafe Moss serves nettle soup.\n",
        ),
        (
            "b/notes.md",
            "# Cafe Moss\n\nCafe Moss closes on Mondays.\n",
        ),
        ("Cafe Moss.md", "Cafe Moss has a garden table.\n"),
    ];
    for (rel, text) in docs {
        std::fs::write(root.join(rel), text).unwrap();
    }
    let files: Vec<String> = docs.iter().map(|(rel, _)| (*rel).to_owned()).collect();
    let mut store = Store::open_in_memory().unwrap();
    let corpus = eval::ingest(&mut store, &root, &files, 1200, 0).unwrap();
    std::fs::remove_dir_all(&root).unwrap();

    assert_eq!((corpus.files, corpus.rejected), (3, 0));
    let mut entities = std::collections::BTreeMap::new();
    for (fact, rel) in &corpus.fact_paths {
        let f = store.fact(fact).unwrap().unwrap();
        assert_eq!(f.provenance.source_ref, *rel);
        let e = store.entity(&f.entity).unwrap().unwrap();
        assert_eq!(e.name, "Cafe Moss");
        assert_eq!(entities.insert(f.entity, rel.clone()), None, "{rel}");
    }
    assert_eq!(entities.len(), 3);
}
