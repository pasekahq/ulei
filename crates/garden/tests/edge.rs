//! The edge through its public API: every decision and every resolution. All people, places and texts
//! are fictional.

#![allow(missing_docs, clippy::unwrap_used, clippy::cast_precision_loss)]

use ulei_garden::edge::{Edge, EdgeConfig, EdgeError, Outcome, Rejection};
use ulei_garden::model::{Attrs, Entity, EntityId, EntityKind, FactId, Sensitivity, SourceKind};
use ulei_garden::store::Store;
use ulei_garden::wire::c7;

fn cand(entity: &str, fact: &str) -> c7::ProposeRequest {
    c7::ProposeRequest {
        fact: fact.into(),
        entity: Some(entity.into()),
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
        pin: None,
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
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
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
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
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
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
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

fn texts_at(store: &Store, valid_at: i64, known_at: i64) -> Vec<String> {
    store
        .facts_of(&EntityId("ada-example".into()), valid_at, known_at)
        .unwrap()
        .into_iter()
        .map(|f| f.text)
        .collect()
}

#[test]
fn an_equal_source_with_an_older_overlapping_version_is_a_contradiction() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let old = accepted(
        &edge
            .propose(
                &address("Ada lives at 3 Old Road.", c7::SourceKind::Raw, 300.0),
                300,
            )
            .unwrap(),
    );
    let c = c7::ProposeRequest {
        valid_to: Some(400.0),
        ..address("Ada lives at 7 Sample Lane.", c7::SourceKind::Raw, 200.0)
    };
    let (_, reason, with) = queued(&edge.propose(&c, 400).unwrap());
    assert_eq!((reason, with), (c7::DigestReason::Contradiction, Some(old)));
}

#[test]
fn an_older_open_ended_version_fills_in_history_and_keeps_the_present() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    edge.propose(
        &address("Ada lives at 3 Old Road.", c7::SourceKind::Raw, 300.0),
        300,
    )
    .unwrap();
    let id = accepted(
        &edge
            .propose(
                &address("Ada lives at 7 Sample Lane.", c7::SourceKind::Raw, 200.0),
                400,
            )
            .unwrap(),
    );
    assert_eq!(store.fact(&id).unwrap().unwrap().valid_to, Some(300));
    assert_eq!(current(&store, 600), ["Ada lives at 3 Old Road."]);
    assert_eq!(texts_at(&store, 250, 600), ["Ada lives at 7 Sample Lane."]);
}

#[test]
fn a_weaker_source_cannot_rewrite_stronger_history() {
    // The owner's past address, a newer address from the internet, then raw material that
    // contradicts the owner's past while outranking the present.
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let owners = accepted(
        &edge
            .propose(
                &c7::ProposeRequest {
                    valid_to: Some(500.0),
                    ..address("Ada lives at 1 Old Road.", c7::SourceKind::Owner, 100.0)
                },
                100,
            )
            .unwrap(),
    );
    accepted(
        &edge
            .propose(
                &address("Ada lives at 2 New Road.", c7::SourceKind::Internet, 500.0),
                600,
            )
            .unwrap(),
    );
    let wrong = c7::ProposeRequest {
        valid_to: Some(400.0),
        ..address("Ada lives at 9 Wrong Road.", c7::SourceKind::Raw, 200.0)
    };
    let (_, reason, with) = queued(&edge.propose(&wrong, 700).unwrap());
    assert_eq!(
        (reason, with),
        (c7::DigestReason::Contradiction, Some(owners))
    );
    assert_eq!(texts_at(&store, 300, 800), ["Ada lives at 1 Old Road."]);
    assert_eq!(current(&store, 800), ["Ada lives at 2 New Road."]);
}

#[test]
fn past_history_that_overlaps_nothing_is_no_contradiction() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    accepted(
        &edge
            .propose(
                &address("Ada lives at 3 Old Road.", c7::SourceKind::Owner, 500.0),
                500,
            )
            .unwrap(),
    );
    let past = c7::ProposeRequest {
        valid_to: Some(100.0),
        ..address("Ada lived at 5 First Street.", c7::SourceKind::Owner, 10.0)
    };
    accepted(&edge.propose(&past, 600).unwrap());
    assert_eq!(edge.digest(10).unwrap().len(), 0);
    assert_eq!(texts_at(&store, 50, 700), ["Ada lived at 5 First Street."]);
    assert_eq!(current(&store, 700), ["Ada lives at 3 Old Road."]);
}

#[test]
fn equal_text_with_a_new_interval_is_an_update() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let first = accepted(
        &edge
            .propose(&cand("Ada Example", "Ada works at Sample Works."), 100)
            .unwrap(),
    );
    let ended = c7::ProposeRequest {
        valid_to: Some(500.0),
        ..cand("Ada Example", "Ada works at Sample Works.")
    };
    let id = accepted(&edge.propose(&ended, 400).unwrap());
    // Saying it again is a duplicate.
    assert!(matches!(
        edge.propose(&ended, 700).unwrap(),
        Outcome::Duplicate { .. }
    ));
    let f = store.fact(&id).unwrap().unwrap();
    assert_eq!((f.valid_from, f.valid_to), (100, Some(500)));
    assert_eq!(
        store.fact(&first).unwrap().unwrap().superseded_at,
        Some(400)
    );
    assert_eq!(current(&store, 300), ["Ada works at Sample Works."]);
    assert_eq!(current(&store, 600), Vec::<String>::new());
}

#[test]
fn low_confidence_wins_over_contradiction_and_keeps_the_conflict() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let old = accepted(
        &edge
            .propose(
                &address("Ada lives at 3 Old Road.", c7::SourceKind::Owner, 100.0),
                100,
            )
            .unwrap(),
    );
    let c = c7::ProposeRequest {
        confidence: Some(0.2),
        ..address("Ada lives at 7 Sample Lane.", c7::SourceKind::Agent, 500.0)
    };
    let o = edge.propose(&c, 500).unwrap();
    let (_, reason, with) = queued(&o);
    assert_eq!(
        (reason, with),
        (c7::DigestReason::LowConfidence, Some(old.clone()))
    );
    let wire = o.to_wire();
    assert_eq!(wire.reason, Some(c7::ProposalReason::LowConfidence));
    assert_eq!(wire.conflicts_with, Some(old.0));
}

#[test]
fn resolving_a_doubt_without_a_conflict_writes_it() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let c = c7::ProposeRequest {
        confidence: Some(0.1),
        ..cand("Ada Example", "Ada might play the cello.")
    };
    let (pid, ..) = queued(&edge.propose(&c, 100).unwrap());
    let item = &edge.digest(10).unwrap()[0];
    assert_eq!(
        (
            item.conflicts_with.as_deref(),
            item.conflicting_fact.as_deref()
        ),
        (None, None)
    );
    accepted(
        &edge
            .resolve(&pid, c7::ResolveAction::Accept, None, None, 200)
            .unwrap(),
    );
    assert_eq!(current(&store, 300), ["Ada might play the cello."]);
}

#[test]
fn low_confidence_goes_to_the_digest() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(
        &mut store,
        EdgeConfig {
            min_confidence: 0.7,
            ..EdgeConfig::default()
        },
    );
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
    let id = accepted(&edge.propose(&c, 100).unwrap());
    assert!((edge.store().fact(&id).unwrap().unwrap().confidence - 0.7).abs() < 1e-6);
    assert_eq!(edge.digest(10).unwrap().len(), 1);
    assert_eq!(current(&store, 200), ["Ada plays chess."]);
}

#[test]
fn a_newer_or_stronger_source_replaces_without_asking_and_keeps_history() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
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

    // The owner outranks raw material even with an older valid_from; the open-ended
    // version ends where the newer one begins.
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    accepted(
        &edge
            .propose(
                &address("Ada lives at 9 Owner Way.", c7::SourceKind::Owner, 500.0),
                3000,
            )
            .unwrap(),
    );
    assert_eq!(texts_at(&store, 700, 3000), ["Ada lives at 9 Owner Way."]);
    assert_eq!(current(&store, 3000), ["Ada lives at 7 Sample Lane."]);
}

fn with_contradiction() -> (Store, String, FactId) {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
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
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let o = edge
        .resolve(&pid, c7::ResolveAction::Accept, None, None, 900)
        .unwrap();
    accepted(&o);
    assert_eq!(edge.digest(10).unwrap().len(), 0);
    assert!(matches!(
        edge.resolve(&pid, c7::ResolveAction::Accept, None, None, 901),
        Err(EdgeError::NotOpen(_))
    ));
    assert_eq!(current(&store, 1000), ["Ada lives at 7 Sample Lane."]);
    assert_eq!(store.fact(&old).unwrap().unwrap().superseded_at, Some(900));
}

#[test]
fn resolve_reject_drops_the_candidate() {
    let (mut store, pid, _) = with_contradiction();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let o = edge
        .resolve(&pid, c7::ResolveAction::Reject, None, None, 900)
        .unwrap();
    assert_eq!(o.to_wire().reason, Some(c7::ProposalReason::OwnerRejected));
    assert_eq!(edge.digest(10).unwrap().len(), 0);
    assert_eq!(current(&store, 1000), ["Ada lives at 3 Old Road."]);
}

#[test]
fn resolve_replace_writes_the_replacement() {
    let (mut store, pid, _) = with_contradiction();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    assert!(matches!(
        edge.resolve(&pid, c7::ResolveAction::Replace, None, None, 900),
        Err(EdgeError::MissingReplacement)
    ));
    let leak = address(
        "Ada's card is 4111 1111 1111 1111",
        c7::SourceKind::Owner,
        800.0,
    );
    assert!(matches!(
        edge.resolve(&pid, c7::ResolveAction::Replace, Some(&leak), None, 900),
        Err(EdgeError::ReplacementRefused(Rejection::SecretValue))
    ));
    assert_eq!(edge.digest(10).unwrap().len(), 1, "still open");
    let fix = address("Ada lives at 8 Sample Lane.", c7::SourceKind::Owner, 800.0);
    accepted(
        &edge
            .resolve(&pid, c7::ResolveAction::Replace, Some(&fix), None, 900)
            .unwrap(),
    );
    assert_eq!(current(&store, 1000), ["Ada lives at 8 Sample Lane."]);
}

#[test]
fn secret_values_are_rejected_and_never_stored() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    for text in [
        "Ada's card number is 4111-1111-1111-1111.",
        "Ada's IBAN is GB82 WEST 1234 5698 7654 32.",
        "Ada's wifi password: Sunflower77",
        "Ada's wifi password for the cottage: Sunflower77",
        "пароль от почты: qwerty12",
        "PIN 4821",
        "door code 4821",
        "код подъезда 4821",
        "the code is 4821",
        "wifi password - Sunflower77",
        "my password, by the way, is hunter2",
        "pass: hunter2x",
        "pw hunter2x",
        "CVV 123",
        "PIN: 482",
        "door code: 123",
        "seed phrase: apple banana cherry",
        "password: vaultkeeper77",
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
    // The store refuses digits in a secret pointer; the edge says so before writing anything.
    let c = c7::ProposeRequest {
        secret_ref: Some("vault:bo-passport".into()),
        ..cand("Bo Sample", "Bo's passport from 2019 is in the vault.")
    };
    assert!(matches!(
        edge.propose(&c, 100).unwrap(),
        Outcome::Rejected {
            reason: Rejection::SecretValue,
            ..
        }
    ));
    // A value in place of the reference.
    let c = c7::ProposeRequest {
        secret_ref: Some("hunter2".into()),
        ..cand("Cy Sample", "Cy's mail password exists.")
    };
    assert!(matches!(
        edge.propose(&c, 100).unwrap(),
        Outcome::Rejected {
            reason: Rejection::SecretValue,
            ..
        }
    ));
    assert_eq!(edge.digest(10).unwrap().len(), 0);
    for name in ["Bo Sample", "Cy Sample"] {
        assert!(store.find_entities(name).unwrap().is_empty(), "{name}");
    }
    assert!(
        store.find_entities("Ada Example").unwrap().is_empty(),
        "nothing written"
    );
}

#[test]
fn a_duplicate_keeps_its_source() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    edge.propose(&cand("Ada Example", "Ada likes tea."), 100)
        .unwrap();
    let again = c7::ProposeRequest {
        source_ref: "msg:2".into(),
        ..cand("Ada Example", "Ada likes tea.")
    };
    let Outcome::Duplicate { proposal, .. } = edge.propose(&again, 200).unwrap() else {
        panic!("expected a duplicate");
    };
    let source: String = store
        .conn()
        .query_row(
            "SELECT source_ref FROM edge_proposal WHERE 'p' || id = ?1",
            [proposal],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(source, "msg:2");
}

#[test]
fn a_pointer_to_a_secret_is_accepted_as_secret() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
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
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
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
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
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
    let no_source = c7::ProposeRequest {
        source_ref: " ".into(),
        ..cand("Ada Example", "Ada was in Sample Town.")
    };
    for c in [
        cand("Ada Example", "   "),
        no_source,
        cand("  ", "Ada was in Sample Town."),
    ] {
        assert!(matches!(
            edge.propose(&c, 300).unwrap(),
            Outcome::Rejected {
                reason: Rejection::Invalid,
                ..
            }
        ));
    }
    let rows: i64 = store
        .conn()
        .query_row(
            "SELECT (SELECT count(*) FROM entities) + (SELECT count(*) FROM facts)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0, "nothing written");
}

#[test]
fn names_resolve_within_the_candidates_kind_and_two_matches_are_ambiguous() {
    let mut store = Store::open_in_memory().unwrap();
    let entity = |id: &str, kind, name: &str, aliases: &[&str]| Entity {
        id: EntityId(id.into()),
        kind,
        name: name.into(),
        aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
        domain: None,
        attrs: Attrs::None,
    };
    for e in [
        entity("p-ada", EntityKind::Person, "Ada Example", &["Ada"]),
        entity("pl-ada", EntityKind::Place, "Ada", &[]),
        entity("p-ae", EntityKind::Person, "A. Example", &[]),
    ] {
        store.upsert_entity(&e).unwrap();
    }
    store
        .merge_entities(&EntityId("p-ae".into()), &EntityId("p-ada".into()), 50)
        .unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let place = c7::ProposeRequest {
        kind: Some(c7::EntityKind::Place),
        ..cand("ADA", "Ada serves soup.")
    };
    let cases = [
        (cand("  ada   EXAMPLE ", "Ada likes tea."), "p-ada"),
        (cand("ADA", "Ada likes rain."), "p-ada"),
        (cand("p-ae", "Ada likes snow."), "p-ada"),
        (place, "pl-ada"),
    ];
    let mut facts = Vec::new();
    for (c, want) in cases {
        facts.push((accepted(&edge.propose(&c, 100).unwrap()), want));
    }
    let any_kind = c7::ProposeRequest {
        kind: None,
        ..cand("Ada", "Ada is busy.")
    };
    let (_, reason, _) = queued(&edge.propose(&any_kind, 100).unwrap());
    assert_eq!(reason, c7::DigestReason::AmbiguousEntity);
    assert_eq!(
        candidate_ids(&edge.digest(10).unwrap()[0]),
        ["p-ada", "pl-ada"]
    );
    for (f, want) in facts {
        assert_eq!(store.fact(&f).unwrap().unwrap().entity.0, want);
    }
    assert_eq!(
        store.find_entities("ada").unwrap().len(),
        2,
        "no entity created"
    );
}

fn entity(id: &str, kind: EntityKind, name: &str) -> Entity {
    Entity {
        id: EntityId(id.into()),
        kind,
        name: name.into(),
        aliases: Vec::new(),
        domain: None,
        attrs: Attrs::None,
    }
}

/// A store holding the people `(id, name)`.
fn people(names: &[(&str, &str)]) -> Store {
    let mut store = Store::open_in_memory().unwrap();
    for (id, name) in names {
        store
            .upsert_entity(&entity(id, EntityKind::Person, name))
            .unwrap();
    }
    store
}

fn entity_of(store: &Store, fact: &FactId) -> String {
    store.fact(fact).unwrap().unwrap().entity.0
}

fn candidate_ids(item: &c7::DigestItem) -> Vec<String> {
    item.entity_candidates
        .iter()
        .flatten()
        .map(|c| c.entity.clone())
        .collect()
}

#[test]
fn a_latin_spelling_resolves_to_the_cyrillic_entity() {
    let mut store = people(&[("p-mira", "Мира Вельде")]);
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let fact = accepted(
        &edge
            .propose(&cand("Mira Velde", "Mira plays the cello."), 100)
            .unwrap(),
    );
    // The last letter of this spelling is Cyrillic.
    let mixed = accepted(
        &edge
            .propose(&cand("Mira Veld\u{435}", "Mira tunes pianos."), 100)
            .unwrap(),
    );
    assert_eq!(entity_of(&store, &fact), "p-mira");
    assert_eq!(entity_of(&store, &mixed), "p-mira");
    assert_eq!(rows(&store, "entities"), 1);
}

#[test]
fn one_typo_resolves_a_name_of_five_characters_or_more_but_not_a_shorter_one() {
    let mut store = people(&[("p-mira", "Мира Вельде"), ("p-lena", "Lena")]);
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let typo = accepted(
        &edge
            .propose(&cand("Mira Veldo", "Mira tunes pianos."), 100)
            .unwrap(),
    );
    let short = accepted(
        &edge
            .propose(&cand("Lina", "Lina keeps bees."), 100)
            .unwrap(),
    );
    assert_eq!(entity_of(&store, &typo), "p-mira");
    assert_eq!(entity_of(&store, &short), "lina");
    assert_eq!(rows(&store, "entities"), 3);
}

#[test]
fn the_first_level_that_finds_a_name_decides() {
    let mut store = people(&[("p-latin", "Mira Velde"), ("p-cyr", "Мира Вельде")]);
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let exact = accepted(
        &edge
            .propose(&cand("mira velde", "Mira sings."), 100)
            .unwrap(),
    );
    assert_eq!(entity_of(&store, &exact), "p-latin");
}

#[test]
fn two_near_names_queue_the_candidate_as_ambiguous_and_write_nothing() {
    let mut store = people(&[("p-velde", "Mira Velde"), ("p-valde", "Мира Вальде")]);
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let o = edge
        .propose(&cand("Mira Vilde", "Mira rows on Sundays."), 100)
        .unwrap();
    let wire = o.to_wire();
    assert_eq!(
        (wire.status, wire.reason),
        (
            c7::ProposalStatus::Queued,
            Some(c7::ProposalReason::AmbiguousEntity)
        )
    );
    let digest = edge.digest(10).unwrap();
    assert_eq!(digest.len(), 1);
    assert_eq!(digest[0].reason, c7::DigestReason::AmbiguousEntity);
    assert_eq!(digest[0].candidate.entity.as_deref(), Some("Mira Vilde"));
    let person = |id: &str, name: &str| c7::EntityCandidate {
        entity: id.into(),
        name: name.into(),
        kind: c7::EntityKind::Person,
    };
    assert_eq!(
        digest[0].entity_candidates,
        Some(vec![
            person("p-valde", "Мира Вальде"),
            person("p-velde", "Mira Velde")
        ])
    );
    assert_eq!(rows(&store, "facts"), 0);
    assert_eq!(rows(&store, "entities"), 2);
}

#[test]
fn resolve_accepts_an_ambiguous_item_for_the_chosen_candidate_only() {
    let mut store = people(&[("p-velde", "Mira Velde"), ("p-valde", "Мира Вальде")]);
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let (pid, ..) = queued(
        &edge
            .propose(&cand("Mira Vilde", "Mira rows on Sundays."), 100)
            .unwrap(),
    );
    let accept = c7::ResolveAction::Accept;
    assert!(matches!(
        edge.resolve(&pid, accept, None, None, 200),
        Err(EdgeError::MissingEntity)
    ));
    for (action, chosen) in [(accept, "p-other"), (c7::ResolveAction::Reject, "p-velde")] {
        assert!(matches!(
            edge.resolve(&pid, action, None, Some(chosen), 200),
            Err(EdgeError::NotACandidate(e)) if e == chosen
        ));
    }
    assert_eq!(edge.digest(10).unwrap().len(), 1, "the item stays open");
    let fact = accepted(
        &edge
            .resolve(&pid, accept, None, Some("p-velde"), 200)
            .unwrap(),
    );
    assert_eq!(entity_of(&store, &fact), "p-velde");
    assert_eq!(rows(&store, "entities"), 2);
}

#[test]
fn reject_closes_an_ambiguous_item_without_a_write() {
    let mut store = people(&[("p-velde", "Mira Velde"), ("p-valde", "Мира Вальде")]);
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let (pid, ..) = queued(
        &edge
            .propose(&cand("Mira Vilde", "Mira rows on Sundays."), 100)
            .unwrap(),
    );
    assert!(matches!(
        edge.resolve(&pid, c7::ResolveAction::Reject, None, None, 200)
            .unwrap(),
        Outcome::Rejected {
            reason: Rejection::OwnerRejected,
            ..
        }
    ));
    assert_eq!(edge.digest(10).unwrap().len(), 0);
    assert_eq!(rows(&store, "facts"), 0);
}

#[test]
fn a_candidate_without_a_kind_never_lands_on_a_visit_of_its_name() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let visit = c7::ProposeRequest {
        kind: Some(c7::EntityKind::Visit),
        ..cand("Lunch at Lumen", "The owner rated the lunch 4.")
    };
    let kindless = c7::ProposeRequest {
        kind: None,
        ..cand("Lunch at Lumen", "Lunch at Lumen costs twelve.")
    };
    let visit = accepted(&edge.propose(&visit, 100).unwrap());
    let other = accepted(&edge.propose(&kindless, 100).unwrap());
    let (visit, other) = (entity_of(&store, &visit), entity_of(&store, &other));
    assert_ne!(visit, other);
    let kind = |id: String| store.entity(&EntityId(id)).unwrap().unwrap().kind;
    assert_eq!(
        (kind(visit), kind(other)),
        (EntityKind::Visit, EntityKind::Item)
    );
}

/// Proposes `text` from 100 (open-ended), then the same text over `later`, both by the owner.
fn restate(
    attribute: Option<&str>,
    text: &str,
    later: (f64, Option<f64>),
) -> (Store, FactId, FactId) {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let first = c7::ProposeRequest {
        attribute: attribute.map(Into::into),
        valid_from: Some(100.0),
        ..cand("Ada Example", text)
    };
    let old = accepted(&edge.propose(&first, 100).unwrap());
    let again = c7::ProposeRequest {
        valid_from: Some(later.0),
        valid_to: later.1,
        ..first
    };
    let new = accepted(&edge.propose(&again, 400).unwrap());
    (store, old, new)
}

#[test]
fn a_bounded_restatement_keeps_the_rest_of_the_statement() {
    for (attribute, text) in [
        (Some("address"), "Ada lives at 3 Old Road."),
        (None, "Ada works at Sample Works."),
    ] {
        let (store, old, new) = restate(attribute, text, (200.0, Some(300.0)));
        for at in [150, 250, 500] {
            assert_eq!(texts_at(&store, at, 400), [text], "{attribute:?} at {at}");
        }
        assert_eq!(store.fact(&old).unwrap().unwrap().superseded_by, Some(new));
    }
}

#[test]
fn a_later_open_restatement_is_a_duplicate_and_keeps_history() {
    for (attribute, text) in [
        (Some("address"), "Ada lives at 3 Old Road."),
        (None, "Ada works at Sample Works."),
    ] {
        let mut store = Store::open_in_memory().unwrap();
        let mut edge = Edge::new(&mut store, EdgeConfig::default());
        let first = c7::ProposeRequest {
            attribute: attribute.map(Into::into),
            valid_from: Some(100.0),
            ..cand("Ada Example", text)
        };
        let old = accepted(&edge.propose(&first, 100).unwrap());
        let later = c7::ProposeRequest {
            valid_from: Some(200.0),
            ..first
        };
        match edge.propose(&later, 400).unwrap() {
            Outcome::Duplicate { fact, .. } => assert_eq!(fact, old),
            other => panic!("expected duplicate, got {other:?}"),
        }
        for at in [150, 250, 500] {
            assert_eq!(texts_at(&store, at, 400), [text], "{attribute:?} at {at}");
        }
    }
}

#[test]
fn resolve_accept_over_a_restated_fact_leaves_one_copy() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let text = "Ada works at Sample Works.";
    let owner = c7::ProposeRequest {
        valid_from: Some(300.0),
        ..cand("Ada Example", text)
    };
    let old = accepted(&edge.propose(&owner, 300).unwrap());
    let wider = c7::ProposeRequest {
        valid_from: Some(100.0),
        source_kind: Some(c7::SourceKind::Agent),
        ..cand("Ada Example", text)
    };
    let (pid, reason, with) = queued(&edge.propose(&wider, 400).unwrap());
    assert_eq!(
        (reason, with),
        (c7::DigestReason::Contradiction, Some(old.clone()))
    );
    let new = accepted(
        &edge
            .resolve(&pid, c7::ResolveAction::Accept, None, None, 500)
            .unwrap(),
    );
    assert_eq!(texts_at(&store, 600, 600), [text]);
    assert_eq!(texts_at(&store, 200, 600), [text]);
    assert_eq!(store.fact(&old).unwrap().unwrap().superseded_by, Some(new));
}

#[test]
fn a_store_refusal_of_a_pointer_fact_is_invalid_not_a_secret() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    accepted(
        &edge
            .propose(
                &address("Ada lives at 3 Old Road.", c7::SourceKind::Agent, 100.0),
                500,
            )
            .unwrap(),
    );
    // Proposed at a transaction time before the agent's version was recorded.
    let c = c7::ProposeRequest {
        secret_ref: Some("vault:ada/addr".into()),
        ..address(
            "Ada's address is in the vault.",
            c7::SourceKind::Owner,
            100.0,
        )
    };
    match edge.propose(&c, 400).unwrap() {
        Outcome::Rejected { reason, .. } => assert_eq!(reason, Rejection::Invalid),
        other => panic!("expected rejected, got {other:?}"),
    }
}

#[test]
fn visit_and_preference_candidates_sharing_a_name_are_new_entities() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let of = |kind, fact: &str| c7::ProposeRequest {
        kind: Some(kind),
        ..cand("Lunch at Lumen", fact)
    };
    let facts = [
        of(c7::EntityKind::Visit, "The owner rated the lunch 4."),
        of(c7::EntityKind::Visit, "The owner rated the lunch 2."),
        of(c7::EntityKind::Preference, "The owner likes the lunch."),
        of(c7::EntityKind::Preference, "Jo dislikes the lunch."),
    ]
    .map(|c| accepted(&edge.propose(&c, 100).unwrap()));
    let entities: Vec<EntityId> = facts
        .iter()
        .map(|f| store.fact(f).unwrap().unwrap().entity)
        .collect();
    for (i, e) in entities.iter().enumerate() {
        assert!(!entities[..i].contains(e), "{e:?} reused");
        assert_eq!(store.facts_of(e, 100, 100).unwrap().len(), 1);
    }
}

/// Makes every later insert into `facts` fail, after the proposal row and a new entity.
fn refuse_facts(store: &Store) {
    store
        .conn()
        .execute_batch(
            "CREATE TRIGGER refuse_facts BEFORE INSERT ON facts
             BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
        )
        .unwrap();
}

fn rows(store: &Store, table: &str) -> i64 {
    store
        .conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_store_failure_mid_propose_leaves_no_proposal_entity_or_fact() {
    let mut store = Store::open_in_memory().unwrap();
    refuse_facts(&store);
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let c = cand("Ada Example", "Ada likes tea.");
    let e = edge.propose(&c, 100).unwrap_err();
    assert!(matches!(e, EdgeError::Store(_)), "{e:?}");
    assert_eq!(e.class(), "sqlite_constraint");
    for table in ["edge_proposal", "entities", "facts"] {
        assert_eq!(rows(&store, table), 0, "{table}");
    }
    store
        .conn()
        .execute_batch("DROP TRIGGER refuse_facts")
        .unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    assert_eq!(
        edge.propose(&c, 200).unwrap(),
        Outcome::Accepted {
            proposal: "p1".into(),
            fact: FactId("f1".into())
        }
    );
}

#[test]
fn a_store_failure_mid_restatement_keeps_the_rival_current() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let old = accepted(
        &edge
            .propose(&cand("Ada Example", "Ada works at Sample Works."), 100)
            .unwrap(),
    );
    refuse_facts(edge.store());
    let ended = c7::ProposeRequest {
        valid_to: Some(300.0),
        ..cand("Ada Example", "Ada works at Sample Works.")
    };
    assert!(matches!(
        edge.propose(&ended, 200).unwrap_err(),
        EdgeError::Store(_)
    ));
    let f = store.fact(&old).unwrap().unwrap();
    assert_eq!((f.superseded_at, f.superseded_by), (None, None));
    assert_eq!(rows(&store, "facts"), 1);
    assert_eq!(rows(&store, "edge_proposal"), 1);
}

#[test]
fn a_store_failure_in_resolve_leaves_the_item_open() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    edge.propose(
        &address("Ada lives at 3 Old Road.", c7::SourceKind::Owner, 100.0),
        100,
    )
    .unwrap();
    let (proposal, ..) = queued(
        &edge
            .propose(
                &address("Ada lives at 7 Sample Lane.", c7::SourceKind::Agent, 200.0),
                200,
            )
            .unwrap(),
    );
    // The fact is written; closing the digest item fails.
    edge.store()
        .conn()
        .execute_batch(
            "CREATE TRIGGER refuse_close BEFORE UPDATE ON edge_proposal
             BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
        )
        .unwrap();
    assert!(matches!(
        edge.resolve(&proposal, c7::ResolveAction::Accept, None, None, 300)
            .unwrap_err(),
        EdgeError::Sqlite(_)
    ));
    assert_eq!(current(&store, 300), ["Ada lives at 3 Old Road."]);
    assert_eq!(rows(&store, "facts"), 1);
    let edge = Edge::new(&mut store, EdgeConfig::default());
    let open = edge.digest(10).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].proposal_id, proposal);
}

/// The facts waiting in `reconcile_queue` with their candidates, in queue order.
fn reconcile_queue(store: &Store) -> Vec<(String, c7::ProposeRequest)> {
    store
        .conn()
        .prepare("SELECT fact, candidate FROM reconcile_queue ORDER BY rowid")
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(|row| {
            let (fact, candidate) = row.unwrap();
            (fact, serde_json::from_str(&candidate).unwrap())
        })
        .collect()
}

/// Reconciles every waiting fact that has a vector of `model`, at `now`.
fn reconcile_all(store: &mut Store, model: &str, now: i64) {
    let mut edge = Edge::new(store, EdgeConfig::default());
    while let Some(id) = edge.unreconciled(model).unwrap() {
        edge.reconcile_similar(&id, model, now).unwrap();
    }
}

/// A failed content check drops the vectors computed since the last passed one, and with them
/// the reconciliation decisions made from them, in the same unit of work: the open
/// `contradiction` items that reconciliation wrote in that window from a dropped vector, of the
/// fact or of its rival, are withdrawn, and their facts and every reconciled fact whose vector
/// is dropped wait in `reconcile_queue` again, so they are weighed once more when the vectors
/// are back. An item from before the window stays (review E13r, finding 2).
#[test]
fn a_failed_model_check_withdraws_the_reconciliation_made_from_the_dropped_vectors() {
    let mut store = Store::open_in_memory().unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let mut propose = |entity: &str, text: &str, now: i64| {
        accepted(&edge.propose(&cand(entity, text), now).unwrap())
    };
    // Before the window: a contradiction from trusted vectors.
    let meat = propose("Ada Example", "Ada eats meat.", 50);
    let vegetarian = propose("Ada Example", "Ada is vegetarian.", 51);
    // Facts of the window, and one fact of before it whose rival is of the window.
    let early = propose("Dee Example", "Dee works nights.", 52);
    let coffee = propose("Bob Example", "Bob drinks coffee.", 110);
    let no_coffee = propose("Bob Example", "Bob never drinks coffee.", 111);
    let alone = propose("Cy Example", "Cy plays chess.", 112);
    let late = propose("Dee Example", "Dee works days.", 113);

    for id in [&meat, &vegetarian] {
        store.put_vector(id, "m", &[1.0, 0.0], 60).unwrap();
    }
    reconcile_all(&mut store, "m", 70);
    store.put_vector(&early, "m", &[1.0, 0.0], 90).unwrap();
    let edge = Edge::new(&mut store, EdgeConfig::default());
    let before = edge.digest(10).unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].candidate.fact, "Ada is vegetarian.");

    store.model_check_passed("ab", 100).unwrap();
    for id in [&coffee, &no_coffee, &alone, &late] {
        store.put_vector(id, "m", &[1.0, 0.0], 130).unwrap();
    }
    // The late fact of Dee first, then the early one, reconciled in the window (as after a
    // backlog) from its own trusted vector against the late one's, of the window.
    {
        let mut edge = Edge::new(&mut store, EdgeConfig::default());
        edge.reconcile_similar(&late, "m", 140).unwrap();
        edge.reconcile_similar(&early, "m", 140).unwrap();
    }
    reconcile_all(&mut store, "m", 140);
    let edge = Edge::new(&mut store, EdgeConfig::default());
    let window = edge.digest(10).unwrap();
    assert_eq!(window.len(), 3, "{window:?}");
    let withdrawn: Vec<String> = window[1..].iter().map(|i| i.proposal_id.clone()).collect();
    assert_eq!(window[1].candidate.fact, "Dee works nights.");
    assert_eq!(window[1].conflicts_with.as_deref(), Some(late.0.as_str()));
    let no_coffee_candidate = window[2].candidate.clone();
    assert_eq!(no_coffee_candidate.fact, "Bob never drinks coffee.");
    assert_eq!(reconcile_queue(&store).len(), 0);

    assert_eq!(
        store
            .model_check_failed("model file corrupted", 200)
            .unwrap(),
        4
    );

    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let after = edge.digest(10).unwrap();
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0].proposal_id, before[0].proposal_id);
    for id in &withdrawn {
        let e = edge
            .resolve(id, c7::ResolveAction::Accept, None, None, 210)
            .unwrap_err();
        assert!(matches!(e, EdgeError::NotOpen(_)), "{e:?}");
    }
    let queued = reconcile_queue(&store);
    let mut facts: Vec<&str> = queued.iter().map(|(f, _)| f.as_str()).collect();
    facts.sort_unstable();
    let mut expected = vec![
        coffee.0.as_str(),
        no_coffee.0.as_str(),
        alone.0.as_str(),
        early.0.as_str(),
        late.0.as_str(),
    ];
    expected.sort_unstable();
    assert_eq!(facts, expected);
    for (fact, candidate) in &queued {
        let written = store.fact(&FactId(fact.clone())).unwrap().unwrap();
        assert_eq!(candidate.fact, written.text);
        assert_eq!(candidate.attribute, None);
        if *fact == no_coffee.0 {
            assert_eq!(candidate, &no_coffee_candidate, "the item's own candidate");
        }
    }

    // Vectors back: the facts are weighed again and the contradictions come again, once each.
    store.model_check_passed("ab", 300).unwrap();
    for id in [&coffee, &no_coffee, &alone, &late] {
        store.put_vector(id, "m", &[1.0, 0.0], 310).unwrap();
    }
    reconcile_all(&mut store, "m", 320);
    assert_eq!(reconcile_queue(&store).len(), 0);
    let edge = Edge::new(&mut store, EdgeConfig::default());
    let again = edge.digest(10).unwrap();
    assert_eq!(again.len(), 3, "{again:?}");
    let mut firsts: Vec<&str> = again
        .iter()
        .map(|i| i.candidate.fact.split(' ').next().unwrap())
        .collect();
    firsts.sort_unstable();
    assert_eq!(firsts, ["Ada", "Bob", "Dee"]);
}
