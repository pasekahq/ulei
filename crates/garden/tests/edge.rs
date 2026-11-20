//! The edge through its public API: every decision and every resolution. All people, places and texts
//! are fictional.

#![allow(missing_docs, clippy::unwrap_used, clippy::cast_precision_loss)]

use ulei_garden::edge::{Edge, EdgeConfig, EdgeError, Outcome, Rejection};
use ulei_garden::model::{EntityId, FactId, Sensitivity, SourceKind};
use ulei_garden::store::Store;
use ulei_garden::wire::c7;

fn cand(entity: &str, fact: &str) -> c7::ProposeRequest {
    c7::ProposeRequest {
        fact: fact.into(),
        entity: entity.into(),
        source_ref: "msg:1".into(),
        valid_from: None,
        kind: Some(c7::EntityKind::Person),
        domain: None,
        attribute: None,
        valid_to: None,
        source_kind: Some(c7::SourceKind::Owner),
        observed_at: None,
        confidence: None,
        ttl_kind: None,
        secret_ref: None,
        private: None,
    }
}

fn address(fact: &str, source: c7::SourceKind, from: f64) -> c7::ProposeRequest {
    c7::ProposeRequest {
        attribute: Some("address".into()),
        source_kind: Some(source),
        valid_from: Some(from),
        ..cand("Ada Example", fact)
    }
}

fn accepted(o: &Outcome) -> FactId {
    match o {
        Outcome::Accepted { fact, .. } => fact.clone(),
        other => panic!("expected accepted, got {other:?}"),
    }
}

fn queued(o: &Outcome) -> (String, c7::DigestReason, Option<FactId>) {
    match o {
        Outcome::Queued {
            proposal,
            reason,
            conflicts_with,
        } => (proposal.clone(), *reason, conflicts_with.clone()),
        other => panic!("expected queued, got {other:?}"),
    }
}

fn current(store: &Store, now: i64) -> Vec<String> {
    let mut v: Vec<String> = store
        .facts_of(&EntityId("ada-example".into()), now, now)
        .unwrap()
        .into_iter()
        .map(|f| f.text)
        .collect();
    v.sort();
    v
}

#[test]
fn accepts_a_new_fact_with_provenance_and_creates_the_entity() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    let o = edge
        .propose(&cand("Ada Example", "Ada Example likes green tea."), 100)
        .unwrap();
    let id = accepted(&o);
    assert_eq!(o.to_wire().status, c7::ProposalStatus::Accepted);
    let f = store.fact(&id).unwrap().unwrap();
    assert_eq!(f.provenance.kind, SourceKind::Owner);
    assert_eq!(f.provenance.source_ref, "msg:1");
    assert_eq!(
        (f.valid_from, f.recorded_at, f.expires_at),
        (100, 100, None)
    );
    let e = store.entity(&f.entity).unwrap().unwrap();
    assert_eq!(e.name, "Ada Example");
    assert_eq!(e.kind.as_str(), "person");
}

#[test]
fn equal_text_of_a_current_fact_is_a_duplicate() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    let id = accepted(
        &edge
            .propose(&cand("Ada Example", "Ada likes tea."), 100)
            .unwrap(),
    );
    let o = edge
        .propose(&cand("ada-example", "  ADA likes   tea "), 200)
        .unwrap();
    assert_eq!(
        o,
        Outcome::Duplicate {
            proposal: "p2".into(),
            fact: id
        }
    );
    assert_eq!(current(&store, 300).len(), 1);
}

#[test]
fn a_weaker_source_contradicting_a_current_fact_goes_to_the_digest() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    let old = accepted(
        &edge
            .propose(
                &address("Ada lives at 3 Old Road.", c7::SourceKind::Owner, 100.0),
                100,
            )
            .unwrap(),
    );
    let o = edge
        .propose(
            &address("Ada lives at 7 Sample Lane.", c7::SourceKind::Agent, 500.0),
            500,
        )
        .unwrap();
    let (pid, reason, with) = queued(&o);
    assert_eq!(reason, c7::DigestReason::Contradiction);
    assert_eq!(with, Some(old.clone()));
    let wire = o.to_wire();
    assert_eq!(wire.reason, Some(c7::ProposalReason::Contradiction));
    assert_eq!(wire.conflicts_with.as_deref(), Some(old.0.as_str()));

    let digest = edge.digest(10).unwrap();
    assert_eq!(digest.len(), 1);
    assert_eq!(digest[0].proposal_id, pid);
    assert_eq!(digest[0].candidate.fact, "Ada lives at 7 Sample Lane.");
    assert_eq!(
        digest[0].conflicting_fact.as_deref(),
        Some("Ada lives at 3 Old Road.")
    );
    assert_eq!(digest[0].queued_at, 500.0);
    assert_eq!(current(&store, 600), ["Ada lives at 3 Old Road."]);
}

#[test]
fn an_equal_source_with_an_older_valid_from_is_a_contradiction() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    edge.propose(
        &address("Ada lives at 3 Old Road.", c7::SourceKind::Raw, 300.0),
        300,
    )
    .unwrap();
    let o = edge
        .propose(
            &address("Ada lives at 7 Sample Lane.", c7::SourceKind::Raw, 200.0),
            400,
        )
        .unwrap();
    assert_eq!(queued(&o).1, c7::DigestReason::Contradiction);
}

#[test]
fn low_confidence_goes_to_the_digest() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(
        &mut store,
        EdgeConfig {
            min_confidence: 0.7,
        },
    )
    .unwrap();
    let c = c7::ProposeRequest {
        confidence: Some(0.5),
        ..cand("Ada Example", "Ada might be vegetarian.")
    };
    let (_, reason, with) = queued(&edge.propose(&c, 100).unwrap());
    assert_eq!((reason, with), (c7::DigestReason::LowConfidence, None));
    let c = c7::ProposeRequest {
        confidence: Some(0.7),
        ..cand("Ada Example", "Ada plays chess.")
    };
    accepted(&edge.propose(&c, 100).unwrap());
    assert_eq!(edge.digest(10).unwrap().len(), 1);
    assert_eq!(current(&store, 200), ["Ada plays chess."]);
}

#[test]
fn a_newer_or_stronger_source_replaces_without_asking_and_keeps_history() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    let old = accepted(
        &edge
            .propose(
                &address("Ada lives at 3 Old Road.", c7::SourceKind::Raw, 100.0),
                100,
            )
            .unwrap(),
    );
    accepted(
        &edge
            .propose(
                &address("Ada lives at 7 Sample Lane.", c7::SourceKind::Raw, 1000.0),
                1000,
            )
            .unwrap(),
    );
    assert_eq!(edge.digest(10).unwrap().len(), 0);
    assert_eq!(current(&store, 2000), ["Ada lives at 7 Sample Lane."]);
    // The old address is still the answer for a time before the move, as known now.
    assert_eq!(
        store
            .facts_of(&EntityId("ada-example".into()), 500, 2000)
            .unwrap()
            .iter()
            .map(|f| f.text.as_str())
            .collect::<Vec<_>>(),
        ["Ada lives at 3 Old Road."]
    );
    assert_eq!(store.fact(&old).unwrap().unwrap().superseded_at, Some(1000));

    // The owner outranks raw material even with an older valid_from.
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    accepted(
        &edge
            .propose(
                &address("Ada lives at 9 Owner Way.", c7::SourceKind::Owner, 500.0),
                3000,
            )
            .unwrap(),
    );
    assert_eq!(current(&store, 3000), ["Ada lives at 9 Owner Way."]);
}

fn with_contradiction() -> (Store, String, FactId) {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    let old = accepted(
        &edge
            .propose(
                &address("Ada lives at 3 Old Road.", c7::SourceKind::Owner, 100.0),
                100,
            )
            .unwrap(),
    );
    let (pid, ..) = queued(
        &edge
            .propose(
                &address("Ada lives at 7 Sample Lane.", c7::SourceKind::Agent, 500.0),
                500,
            )
            .unwrap(),
    );
    (store, pid, old)
}

#[test]
fn resolve_accept_writes_the_candidate_and_closes_the_conflict() {
    let (mut store, pid, old) = with_contradiction();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    let o = edge
        .resolve(&pid, c7::ResolveAction::Accept, None, 900)
        .unwrap();
    accepted(&o);
    assert_eq!(edge.digest(10).unwrap().len(), 0);
    assert!(matches!(
        edge.resolve(&pid, c7::ResolveAction::Accept, None, 901),
        Err(EdgeError::NotOpen(_))
    ));
    assert_eq!(current(&store, 1000), ["Ada lives at 7 Sample Lane."]);
    assert_eq!(store.fact(&old).unwrap().unwrap().superseded_at, Some(900));
}

#[test]
fn resolve_reject_drops_the_candidate() {
    let (mut store, pid, _) = with_contradiction();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    let o = edge
        .resolve(&pid, c7::ResolveAction::Reject, None, 900)
        .unwrap();
    assert_eq!(o.to_wire().reason, Some(c7::ProposalReason::OwnerRejected));
    assert_eq!(edge.digest(10).unwrap().len(), 0);
    assert_eq!(current(&store, 1000), ["Ada lives at 3 Old Road."]);
}

#[test]
fn resolve_replace_writes_the_replacement() {
    let (mut store, pid, _) = with_contradiction();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    assert!(matches!(
        edge.resolve(&pid, c7::ResolveAction::Replace, None, 900),
        Err(EdgeError::MissingReplacement)
    ));
    let leak = address(
        "Ada's card is 4111 1111 1111 1111",
        c7::SourceKind::Owner,
        800.0,
    );
    let o = edge
        .resolve(&pid, c7::ResolveAction::Replace, Some(&leak), 900)
        .unwrap();
    assert_eq!(
        o,
        Outcome::Rejected {
            proposal: pid.clone(),
            reason: Rejection::SecretValue
        }
    );
    assert_eq!(edge.digest(10).unwrap().len(), 1, "still open");
    let fix = address("Ada lives at 8 Sample Lane.", c7::SourceKind::Owner, 800.0);
    accepted(
        &edge
            .resolve(&pid, c7::ResolveAction::Replace, Some(&fix), 900)
            .unwrap(),
    );
    assert_eq!(current(&store, 1000), ["Ada lives at 8 Sample Lane."]);
}

#[test]
fn secret_values_are_rejected_and_never_stored() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    for text in [
        "Ada's card number is 4111-1111-1111-1111.",
        "Ada's IBAN is GB82 WEST 1234 5698 7654 32.",
        "Ada's wifi password: Sunflower77",
        &["Ada's api key is sk-", "proj-abcdefghijklmnopqrstuvwx"].concat(),
    ] {
        let o = edge.propose(&cand("Ada Example", text), 100).unwrap();
        assert!(
            matches!(
                o,
                Outcome::Rejected {
                    reason: Rejection::SecretValue,
                    ..
                }
            ),
            "{text}"
        );
        assert_eq!(o.to_wire().status, c7::ProposalStatus::Rejected);
    }
    // A secret value with a vault reference is still a secret value.
    let c = c7::ProposeRequest {
        secret_ref: Some("vault:ada-card".into()),
        ..cand("Ada Example", "Ada's card is 4111 1111 1111 1111.")
    };
    assert!(matches!(
        edge.propose(&c, 100).unwrap(),
        Outcome::Rejected { .. }
    ));
    assert_eq!(edge.digest(10).unwrap().len(), 0);
    assert!(
        store.find_entities("Ada Example").unwrap().is_empty(),
        "nothing written"
    );
}

#[test]
fn a_pointer_to_a_secret_is_accepted_as_secret() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    let c = c7::ProposeRequest {
        secret_ref: Some("vault:ada-passport".into()),
        ..cand("Ada Example", "Ada's passport number is in the vault.")
    };
    let id = accepted(&edge.propose(&c, 100).unwrap());
    assert_eq!(
        store.fact(&id).unwrap().unwrap().sensitivity,
        Sensitivity::Secret {
            vault_ref: "vault:ada-passport".into()
        }
    );
}

#[test]
fn internet_facts_expire_by_ttl_kind_and_live_data_is_refused() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    let c = c7::ProposeRequest {
        kind: Some(c7::EntityKind::Place),
        source_kind: Some(c7::SourceKind::Internet),
        ttl_kind: Some(c7::TtlKind::Weather),
        observed_at: Some(1000.0),
        ..cand("Sample Town", "Sample Town: light rain, 12 degrees.")
    };
    let id = accepted(&edge.propose(&c, 1100).unwrap());
    assert_eq!(
        edge.store().fact(&id).unwrap().unwrap().expires_at,
        Some(1000 + 3 * 3600)
    );
    let live = c7::ProposeRequest {
        ttl_kind: Some(c7::TtlKind::NeverCache),
        ..c
    };
    assert!(matches!(
        edge.propose(&live, 1100).unwrap(),
        Outcome::Rejected {
            reason: Rejection::LiveData,
            ..
        }
    ));
}

#[test]
fn malformed_candidates_are_invalid() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default()).unwrap();
    let c = c7::ProposeRequest {
        valid_from: Some(200.0),
        valid_to: Some(100.0),
        ..cand("Ada Example", "Ada was in Sample Town.")
    };
    assert!(matches!(
        edge.propose(&c, 300).unwrap(),
        Outcome::Rejected {
            reason: Rejection::Invalid,
            ..
        }
    ));
    assert!(matches!(
        edge.propose(&cand("Ada Example", "   "), 300).unwrap(),
        Outcome::Rejected {
            reason: Rejection::Invalid,
            ..
        }
    ));
}
