//! The selection eval of the related-fact hints (ADR-0096, report E15): on the frozen synthetic
//! base set of `tests/fixtures/reconcile-eval/` (60 fictional people, their earlier facts and
//! 419 new facts, 299 of them related to one earlier fact, written by `ci/reconcile_eval/gen.py`),
//! the share of related cases whose earlier fact is among the [`HINTS`] hints the garden gives
//! the night's judge. Each new fact is hinted against its person's earlier facts only, as the
//! worker does when the new facts arrive one by one.
//!
//! The deterministic fake embedder and the lexical ranking run in every test run and must not
//! fall below their measured recall. The real e5-small file runs in the `encoder.yml` job only
//! (`ULEI_GARDEN_EVAL_MODEL`, ignored otherwise) and must stay within [`E5_TOLERANCE`] of the
//! 0.886 E15 measured.

#![allow(missing_docs, clippy::unwrap_used)]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde_json::Value;
use ulei_garden::edge::HINTS;
use ulei_garden::embed::{Embedder, HashEmbedder};
use ulei_garden::model::{
    Attrs, Entity, EntityId, EntityKind, Fact, FactId, Provenance, Sensitivity, SourceKind, TtlKind,
};
use ulei_garden::search::{Basis, nearest};
use ulei_garden::store::Store;

/// Recall at 5 of the fake embedder (`HashEmbedder` of 256 buckets, character trigrams) on the
/// set: 0.599 (179 of 299) when measured, rounded down; a change of the selection that loses
/// hints fails here.
const FAKE_FLOOR: f64 = 0.595;
/// Recall at 5 of the lexical ranking on the set: 0.629 (188 of 299) when measured, the value
/// E15's re-implementation of the query found too, rounded down.
const LEXICAL_FLOOR: f64 = 0.625;
/// Recall at 5 of e5-small E15 measured on this set.
const E5_MEASURED: f64 = 0.886;
/// How far below [`E5_MEASURED`] the real model may land.
const E5_TOLERANCE: f64 = 0.02;

const NOW: i64 = 1_800_000_000;

struct Case {
    id: String,
    entity: String,
    gold: String,
}

/// The garden holding every earlier fact and every new fact of the set, and the cases.
fn garden() -> (Store, Vec<Case>) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/reconcile-eval");
    let lines = |name: &str| -> Vec<Value> {
        std::fs::read_to_string(dir.join(name))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let mut store = Store::open_in_memory().unwrap();
    let mut seq = 0;
    let mut fact = |id: &str, entity: &str, text: &str| {
        seq += 1;
        Fact {
            id: FactId(id.into()),
            entity: EntityId(entity.into()),
            text: text.into(),
            domain: None,
            attribute: None,
            ttl: TtlKind::Personal,
            valid_from: 0,
            valid_to: None,
            recorded_at: NOW - 100_000 + seq,
            superseded_at: None,
            superseded_by: None,
            provenance: Provenance {
                kind: SourceKind::Owner,
                source_ref: format!("eval:{id}"),
                observed_at: NOW - 100_000 + seq,
                quote: None,
                proof_count: 1,
            },
            expires_at: None,
            sensitivity: Sensitivity::Public,
            confidence: 1.0,
        }
    };
    let mut facts = Vec::new();
    for e in lines("entities.jsonl") {
        let id = e["id"].as_str().unwrap();
        store
            .upsert_entity(&Entity {
                id: EntityId(id.into()),
                kind: EntityKind::Person,
                name: e["name"].as_str().unwrap().into(),
                aliases: Vec::new(),
                domain: None,
                attrs: Attrs::None,
            })
            .unwrap();
        for f in e["facts"].as_array().unwrap() {
            facts.push(fact(f[0].as_str().unwrap(), id, f[1].as_str().unwrap()));
        }
    }
    let mut cases = Vec::new();
    for c in lines("cases.jsonl") {
        let (id, entity) = (c["id"].as_str().unwrap(), c["entity"].as_str().unwrap());
        facts.push(fact(id, entity, c["text"].as_str().unwrap()));
        cases.push(Case {
            id: id.into(),
            entity: entity.into(),
            gold: c["gold"].as_str().unwrap().into(),
        });
    }
    store.insert_facts(&facts).unwrap();
    (store, cases)
}

/// Stores the vector `embedder` gives every fact's passage, as the worker does.
fn embed(store: &mut Store, embedder: &dyn Embedder) {
    let model = embedder.model_id().to_owned();
    let passages = store.missing_vectors(&model).unwrap();
    for chunk in passages.chunks(16) {
        let texts: Vec<&str> = chunk.iter().map(|(_, t)| t.as_str()).collect();
        let vectors = embedder.embed_passages(&texts).unwrap();
        for ((id, _), v) in chunk.iter().zip(vectors) {
            store.put_vector(id, &model, &v, NOW).unwrap();
        }
    }
}

/// Recall at [`HINTS`] over the related cases, under `model` (dense) or without (lexical).
fn recall(store: &Store, cases: &[Case], model: Option<&str>) -> f64 {
    let mut new_of: HashMap<&str, HashSet<String>> = HashMap::new();
    for c in cases {
        new_of.entry(&c.entity).or_default().insert(c.id.clone());
    }
    let (mut related, mut found) = (0_u32, 0_u32);
    for c in cases.iter().filter(|c| !c.gold.is_empty()) {
        related += 1;
        let fact = store.fact(&FactId(c.id.clone())).unwrap().unwrap();
        let others = &new_of[c.entity.as_str()];
        let skip = |id: &FactId| others.contains(&id.0);
        let (hints, basis) = nearest(store, &fact, model, HINTS, NOW, NOW, &skip).unwrap();
        assert_eq!(
            basis,
            if model.is_some() {
                Basis::Dense
            } else {
                Basis::Lexical
            }
        );
        assert!(hints.len() <= HINTS);
        found += u32::from(hints.iter().any(|h| h.0 == c.gold));
    }
    assert_eq!(related, 299, "the frozen base set");
    f64::from(found) / f64::from(related)
}

#[test]
fn hint_recall_of_the_fake_embedder_and_the_lexical_ranking_does_not_fall() {
    let (mut store, cases) = garden();
    let fake = HashEmbedder::new(256);
    embed(&mut store, &fake);
    let dense = recall(&store, &cases, Some(fake.model_id()));
    let lexical = recall(&store, &cases, None);
    println!("reconcile eval: recall@{HINTS} fake {dense:.3}, lexical {lexical:.3}");
    assert!(dense >= FAKE_FLOOR, "fake embedder recall {dense:.3}");
    assert!(lexical >= LEXICAL_FLOOR, "lexical recall {lexical:.3}");
}

/// The real e5-small weight file of `ULEI_GARDEN_EVAL_MODEL` (the `encoder.yml` job).
#[cfg(feature = "native")]
#[test]
#[ignore = "needs the e5-small weight file in ULEI_GARDEN_EVAL_MODEL"]
fn hint_recall_of_e5_small_stays_near_the_measured_one() {
    let path = std::env::var_os("ULEI_GARDEN_EVAL_MODEL").expect("ULEI_GARDEN_EVAL_MODEL");
    let model =
        ulei_garden::embed::native::NativeEmbedder::load(std::path::Path::new(&path)).unwrap();
    let (mut store, cases) = garden();
    embed(&mut store, &model);
    let dense = recall(&store, &cases, Some(model.model_id()));
    println!("reconcile eval: recall@{HINTS} e5-small {dense:.3} (E15 measured {E5_MEASURED})");
    assert!(
        dense >= E5_MEASURED - E5_TOLERANCE,
        "e5-small recall {dense:.3} below {E5_MEASURED} - {E5_TOLERANCE}"
    );
}
