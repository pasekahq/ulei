//! The garden's side of the night's judge (ADR-0096): `reconcile_take`, `reconcile_done`,
//! `reconcile_review` and `reconcile_undo` through the C7 API, on an in-memory garden without
//! an embedder, so the hints are lexical. All people and facts are fictional.

#![allow(missing_docs, clippy::unwrap_used, clippy::cast_precision_loss)]

use ulei_garden::api::{ApiConfig, ApiError, Garden, Hybrid};
use ulei_garden::edge::{Edge, EdgeConfig, Hinted, proof_count};
use ulei_garden::miss::ProducerKey;
use ulei_garden::model::FactId;
use ulei_garden::rerank::NoReranker;
use ulei_garden::store::Store;
use ulei_garden::wire::c7;

const DAY: i64 = 86_400;
const T0: i64 = 1_800_000_000;
const JUDGE: &str = "scripted judge-v1";

fn garden() -> Garden {
    Garden::new(
        Store::open_in_memory().unwrap(),
        Hybrid::new(None, Box::new(NoReranker)),
        ApiConfig::default(),
        ProducerKey::from_bytes(&[0x5a; 32]).unwrap(),
    )
}

fn candidate(fact: &str, source: c7::SourceKind, from: i64) -> c7::ProposeRequest {
    c7::ProposeRequest {
        fact: fact.into(),
        entity: Some("Ada Example".into()),
        source_ref: format!("msg:{from}"),
        valid_from: Some(from as f64),
        kind: Some(c7::EntityKind::Person),
        domain: None,
        attribute: None,
        valid_to: None,
        source_kind: Some(source),
        observed_at: None,
        confidence: None,
        ttl_kind: None,
        secret_ref: None,
        private: None,
        pin: None,
    }
}

/// Writes `fact` through the seedbed and the edge at `now`; the fact's id.
fn write(g: &mut Garden, fact: &str, source: c7::SourceKind, now: i64) -> String {
    let seeded = g
        .garden_propose(&candidate(fact, source, now), now)
        .unwrap();
    let done = c7::SeedbedDoneRequest {
        items: vec![c7::SeedbedDoneItem {
            r#ref: seeded.proposal_id,
            candidates: None,
        }],
    };
    let items = g.garden_seedbed_done(&done, now).unwrap().items;
    let outcome = &items[0].outcomes[0];
    assert_eq!(outcome.status, c7::ProposalStatus::Accepted, "{fact}");
    outcome.fact_id.clone().unwrap()
}

/// Gives `id` its (lexical) hints at `now`; them.
fn hint(g: &mut Garden, id: &str, now: i64) -> Vec<String> {
    match Edge::new(g.store_mut(), EdgeConfig::default())
        .hint(&FactId(id.into()), None, now)
        .unwrap()
    {
        Hinted::Hints(hints, _) => hints.into_iter().map(|h| h.0).collect(),
        other => panic!("{other:?}"),
    }
}

fn verdict(hint: &str, relation: c7::ReconcileRelation, note: &str) -> c7::ReconcileVerdict {
    c7::ReconcileVerdict {
        hint: hint.into(),
        relation,
        note: Some(note.into()),
    }
}

fn done(
    g: &mut Garden,
    items: Vec<(String, Vec<c7::ReconcileVerdict>)>,
    now: i64,
) -> Vec<c7::ReconcileDoneItem> {
    let req = c7::ReconcileDoneRequest {
        judge: JUDGE.into(),
        items: items
            .into_iter()
            .map(|(fact_id, verdicts)| c7::ReconcileJudged { fact_id, verdicts })
            .collect(),
        stats: None,
    };
    let answer = g.garden_reconcile_done(&req, now).unwrap();
    assert!(answer.failed.is_none(), "{:?}", answer.failed);
    answer.items
}

fn current(g: &Garden, id: &str) -> bool {
    g.store()
        .fact(&FactId(id.into()))
        .unwrap()
        .unwrap()
        .superseded_at
        .is_none()
}

fn texts_at(g: &Garden, known_at: i64) -> Vec<String> {
    let mut t: Vec<String> = g
        .store()
        .facts_of(&"ada-example".into_id(), known_at, known_at)
        .unwrap()
        .into_iter()
        .map(|f| f.text)
        .collect();
    t.sort();
    t
}

trait IntoId {
    fn into_id(self) -> ulei_garden::model::EntityId;
}

impl IntoId for &str {
    fn into_id(self) -> ulei_garden::model::EntityId {
        ulei_garden::model::EntityId(self.into())
    }
}

fn review(g: &Garden, req: &c7::ReconcileReviewRequest) -> Vec<c7::ReconcileChange> {
    g.garden_reconcile_review(req).unwrap().changes
}

fn all() -> c7::ReconcileReviewRequest {
    c7::ReconcileReviewRequest {
        limit: None,
        before: None,
        entity: None,
        relation: None,
        open: None,
    }
}

/// The new fact and the earlier one it repeats, hinted.
fn repeated(g: &mut Garden) -> (String, String) {
    let dog = write(
        g,
        "Ada has a dog called Biscuit.",
        c7::SourceKind::Owner,
        T0,
    );
    let new = write(
        g,
        "Ada keeps a dog named Biscuit.",
        c7::SourceKind::Raw,
        T0 + DAY,
    );
    assert_eq!(hint(g, &new, T0 + DAY + 1), std::slice::from_ref(&dog));
    (dog, new)
}

#[test]
fn take_lists_the_hinted_facts_with_their_hints_in_queue_order() {
    let mut g = garden();
    let (dog, new) = repeated(&mut g);
    let take = |g: &mut Garden, after| {
        g.garden_reconcile_take(
            &c7::ReconcileTakeRequest {
                limit: None,
                after,
                hint: None,
            },
            T0 + DAY + 2,
        )
        .unwrap()
        .items
    };
    let items = take(&mut g, None);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.fact.fact_id.as_deref(), Some(new.as_str()));
    assert_eq!(item.entity, "ada-example");
    assert_eq!(item.entity_name, "Ada Example");
    assert_eq!(item.basis, c7::HintBasis::Lexical);
    assert_eq!(item.embedder, None);
    assert_eq!(
        item.hints
            .iter()
            .map(|h| h.fact_id.clone().unwrap())
            .collect::<Vec<_>>(),
        [dog]
    );
    let seq = item.seq;
    assert_eq!(take(&mut g, Some(seq)), []);
    assert_eq!(
        ulei_garden::judge::stats(g.store()).unwrap().without_hints,
        1,
        "the take hinted the earlier fact lexically: alone in its entity, it left"
    );
}

/// A duplicate merges the new fact into the earlier one: the new one is retracted in favour of
/// it, the earlier wording stays, and its proof count rises. Silent: nothing in the digest. The
/// change is listed with both texts and is reversible: the undo writes the new fact back as a
/// current copy, takes the proof back and keeps the pair from the judge.
#[test]
fn a_duplicate_merges_into_the_earlier_fact_and_an_undo_brings_it_back() {
    let mut g = garden();
    let (dog, new) = repeated(&mut g);
    let at = T0 + 2 * DAY;
    let items = done(
        &mut g,
        vec![(
            new.clone(),
            vec![verdict(&dog, c7::ReconcileRelation::Duplicate, "same dog")],
        )],
        at,
    );
    let outcome = &items[0].outcomes[0];
    assert_eq!(outcome.applied, c7::ReconcileApplied::Merged);
    let change = outcome.change_id.unwrap();
    assert!(!current(&g, &new));
    assert!(current(&g, &dog));
    assert_eq!(proof_count(g.store(), &FactId(dog.clone())).unwrap(), 2);
    assert_eq!(texts_at(&g, at), ["Ada has a dog called Biscuit."]);
    assert_eq!(
        texts_at(&g, at - 1).len(),
        2,
        "as_of before the verdict answers as before"
    );
    assert_eq!(
        g.garden_digest(&c7::DigestRequest { limit: None })
            .unwrap()
            .items,
        []
    );

    let changes = review(&g, &all());
    assert_eq!(changes.len(), 1);
    let c = &changes[0];
    assert_eq!(c.change_id, change);
    assert_eq!(c.state, c7::ChangeState::Applied);
    assert_eq!(c.new.fact, "Ada keeps a dog named Biscuit.");
    assert_eq!(c.earlier.fact, "Ada has a dog called Biscuit.");
    assert_eq!(c.note.as_deref(), Some("same dog"));
    assert_eq!(c.judge, JUDGE);
    assert_eq!(c.basis, c7::HintBasis::Lexical);

    let undone = g
        .garden_reconcile_undo(&c7::ReconcileUndoRequest { change_id: change }, at + 10)
        .unwrap()
        .change;
    assert_eq!(undone.state, c7::ChangeState::Undone);
    let copy = undone.restored.clone().unwrap();
    assert!(current(&g, &copy));
    assert_eq!(proof_count(g.store(), &FactId(dog.clone())).unwrap(), 1);
    assert_eq!(texts_at(&g, at + 10).len(), 2);
    assert_eq!(texts_at(&g, at + 5), ["Ada has a dog called Biscuit."]);
    let again = g.garden_reconcile_undo(&c7::ReconcileUndoRequest { change_id: change }, at + 20);
    assert!(matches!(again, Err(ApiError::Invalid(_))), "{again:?}");
    // The pair is kept: a later fact of the entity never gets the restored copy and the
    // earlier fact as a pair to judge again.
    g.store_mut()
        .conn()
        .execute(
            "INSERT INTO reconcile_queue (fact, candidate) VALUES (?1, '{}')",
            [&copy],
        )
        .unwrap();
    assert_eq!(
        Edge::new(g.store_mut(), EdgeConfig::default())
            .hint(&FactId(copy), None, at + 30)
            .unwrap(),
        Hinted::Alone
    );
}

/// A supersede applies by the source-rank rule: an equal source with a later start retracts
/// the earlier fact in favour of the new one, which stays believed outside it. The undo writes
/// the earlier fact back whole and closes the slice the retraction kept.
#[test]
fn a_supersede_the_rank_allows_retracts_the_earlier_fact_and_an_undo_restores_it() {
    let mut g = garden();
    let riga = write(&mut g, "Ada lives in Riga.", c7::SourceKind::Raw, T0);
    let porto = write(&mut g, "Ada moved to Porto.", c7::SourceKind::Raw, T0 + DAY);
    assert_eq!(
        hint(&mut g, &porto, T0 + DAY + 1),
        std::slice::from_ref(&riga)
    );
    let at = T0 + 2 * DAY;
    let items = done(
        &mut g,
        vec![(
            porto.clone(),
            vec![verdict(&riga, c7::ReconcileRelation::Supersedes, "moved")],
        )],
        at,
    );
    let change = items[0].outcomes[0].change_id.unwrap();
    assert_eq!(
        items[0].outcomes[0].applied,
        c7::ReconcileApplied::Superseded
    );
    assert!(!current(&g, &riga));
    let held = |g: &Garden, valid_at: i64, known_at: i64| -> Vec<String> {
        g.store()
            .facts_of(&"ada-example".into_id(), valid_at, known_at)
            .unwrap()
            .into_iter()
            .map(|f| f.text)
            .collect()
    };
    assert_eq!(held(&g, at, at), ["Ada moved to Porto."]);
    assert_eq!(
        held(&g, T0 + 1, at),
        ["Ada lives in Riga."],
        "the earlier fact stays believed before the new one starts"
    );
    let undone = g
        .garden_reconcile_undo(&c7::ReconcileUndoRequest { change_id: change }, at + 10)
        .unwrap()
        .change;
    let copy = undone.restored.unwrap();
    assert!(current(&g, &copy));
    assert_eq!(held(&g, at + 10, at + 10).len(), 2, "both hold again");
    assert_eq!(
        held(&g, T0 + 1, at + 10),
        ["Ada lives in Riga."],
        "the kept slice is closed, the copy holds"
    );
    assert_eq!(
        held(&g, at, at + 5),
        ["Ada moved to Porto."],
        "as_of before the undo"
    );
}

/// A supersede the rank rule forbids (an agent against the owner) is recorded as a
/// contradiction: an open digest item with the judge's note, which a search marks on both facts,
/// and which nobody is pushed; an undo withdraws it and keeps the pair.
#[test]
fn a_supersede_the_rank_forbids_is_recorded_as_a_contradiction_search_shows() {
    let mut g = garden();
    let tea = write(&mut g, "Ada drinks only tea.", c7::SourceKind::Owner, T0);
    let coffee = write(
        &mut g,
        "Ada now drinks coffee.",
        c7::SourceKind::Agent,
        T0 + DAY,
    );
    assert_eq!(
        hint(&mut g, &coffee, T0 + DAY + 1),
        std::slice::from_ref(&tea)
    );
    let items = done(
        &mut g,
        vec![(
            coffee.clone(),
            vec![verdict(
                &tea,
                c7::ReconcileRelation::Supersedes,
                "tea only vs coffee",
            )],
        )],
        T0 + 2 * DAY,
    );
    let outcome = &items[0].outcomes[0];
    assert_eq!(outcome.applied, c7::ReconcileApplied::Contradiction);
    assert!(current(&g, &tea) && current(&g, &coffee));
    let digest = g
        .garden_digest(&c7::DigestRequest { limit: None })
        .unwrap()
        .items;
    assert_eq!(digest.len(), 1);
    assert_eq!(digest[0].reason, c7::DigestReason::Contradiction);
    assert_eq!(digest[0].conflicts_with.as_deref(), Some(tea.as_str()));
    assert_eq!(digest[0].note.as_deref(), Some("tea only vs coffee"));

    let found = g
        .garden_search(
            &c7::SearchRequest {
                q: "Ada tea coffee".into(),
                k: Some(5),
                as_of: None,
                private: None,
                no_miss: Some(true),
            },
            T0 + 2 * DAY,
        )
        .unwrap()
        .response
        .facts;
    for f in &found {
        let other = if f.fact_id.as_deref() == Some(tea.as_str()) {
            &coffee
        } else {
            &tea
        };
        assert_eq!(f.disputed, Some(true), "{f:?}");
        assert_eq!(f.conflicts_with.as_deref(), Some(other.as_str()));
        assert_eq!(f.conflict_note.as_deref(), Some("tea only vs coffee"));
    }
    assert_eq!(found.len(), 2);

    let open = review(
        &g,
        &c7::ReconcileReviewRequest {
            open: Some(true),
            ..all()
        },
    );
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].state, c7::ChangeState::Open);
    assert_eq!(open[0].relation, c7::ReconcileRelation::Supersedes);
    assert_eq!(
        open[0].proposal_id.as_deref(),
        Some(digest[0].proposal_id.as_str())
    );
    g.garden_reconcile_undo(
        &c7::ReconcileUndoRequest {
            change_id: open[0].change_id,
        },
        T0 + 3 * DAY,
    )
    .unwrap();
    assert_eq!(
        g.garden_digest(&c7::DigestRequest { limit: None })
            .unwrap()
            .items,
        []
    );
    assert_eq!(
        review(
            &g,
            &c7::ReconcileReviewRequest {
                open: Some(true),
                ..all()
            }
        ),
        []
    );
}

/// Several verdicts on one fact: every contradiction is recorded, and then no supersede or
/// duplicate is applied, so a new fact is never both merged and disputed.
#[test]
fn a_contradiction_takes_precedence_and_the_rest_is_recorded_not_applied() {
    let mut g = garden();
    let a = write(
        &mut g,
        "Ada swims every morning.",
        c7::SourceKind::Owner,
        T0,
    );
    let b = write(&mut g, "Ada hates swimming.", c7::SourceKind::Owner, T0 + 1);
    let new = write(
        &mut g,
        "Ada swims each morning.",
        c7::SourceKind::Raw,
        T0 + DAY,
    );
    let hints = hint(&mut g, &new, T0 + DAY + 1);
    assert_eq!(hints.len(), 2);
    let items = done(
        &mut g,
        vec![(
            new.clone(),
            vec![
                verdict(&a, c7::ReconcileRelation::Duplicate, "same"),
                verdict(
                    &b,
                    c7::ReconcileRelation::Contradiction,
                    "swims vs hates it",
                ),
            ],
        )],
        T0 + 2 * DAY,
    );
    let applied: Vec<_> = items[0]
        .outcomes
        .iter()
        .map(|o| (o.hint.clone(), o.applied))
        .collect();
    assert_eq!(
        applied,
        [
            (b.clone(), c7::ReconcileApplied::Contradiction),
            (a.clone(), c7::ReconcileApplied::NotApplied),
        ]
    );
    assert!(current(&g, &new));
}

/// `reconcile_done` is idempotent: the second call answers the stored outcomes and changes
/// nothing; a hint closed before the verdict makes it stale; an unknown fact or a hint of
/// another fact is refused before any item is applied.
#[test]
fn done_twice_changes_nothing_stale_hints_and_refusals() {
    let mut g = garden();
    let (dog, new) = repeated(&mut g);
    let cat = write(&mut g, "Ada has a grey cat.", c7::SourceKind::Owner, T0 + 2);
    let kitten = write(
        &mut g,
        "Ada's grey cat is a kitten.",
        c7::SourceKind::Raw,
        T0 + DAY,
    );
    assert_eq!(hint(&mut g, &kitten, T0 + DAY + 1).first(), Some(&cat));

    let refused = g.garden_reconcile_done(
        &c7::ReconcileDoneRequest {
            judge: JUDGE.into(),
            items: vec![
                c7::ReconcileJudged {
                    fact_id: new.clone(),
                    verdicts: vec![verdict(&dog, c7::ReconcileRelation::Duplicate, "x")],
                },
                c7::ReconcileJudged {
                    fact_id: "f-unknown".into(),
                    verdicts: vec![],
                },
            ],
            stats: None,
        },
        T0 + 2 * DAY,
    );
    assert!(matches!(refused, Err(ApiError::Unknown(_))), "{refused:?}");
    let refused = g.garden_reconcile_done(
        &c7::ReconcileDoneRequest {
            judge: JUDGE.into(),
            items: vec![c7::ReconcileJudged {
                fact_id: new.clone(),
                verdicts: vec![verdict(&cat, c7::ReconcileRelation::Duplicate, "x")],
            }],
            stats: None,
        },
        T0 + 2 * DAY,
    );
    assert!(matches!(refused, Err(ApiError::Invalid(_))), "{refused:?}");
    assert!(current(&g, &new), "nothing applied");

    g.store_mut()
        .supersede(&FactId(cat.clone()), T0 + DAY + 5)
        .unwrap();
    let verdicts = || {
        vec![
            (
                new.clone(),
                vec![verdict(&dog, c7::ReconcileRelation::Duplicate, "same dog")],
            ),
            (
                kitten.clone(),
                vec![verdict(&cat, c7::ReconcileRelation::Supersedes, "kitten")],
            ),
        ]
    };
    let first = done(&mut g, verdicts(), T0 + 2 * DAY);
    assert_eq!(first[1].outcomes[0].applied, c7::ReconcileApplied::Stale);
    let rows = |g: &Garden| -> i64 {
        g.store()
            .conn()
            .query_row("SELECT count(*) FROM edge_proposal", [], |r| r.get(0))
            .unwrap()
    };
    let before = rows(&g);
    let second = done(&mut g, verdicts(), T0 + 3 * DAY);
    assert_eq!(first, second);
    assert_eq!(rows(&g), before);
    assert_eq!(proof_count(g.store(), &FactId(dog)).unwrap(), 2);
}

/// A store failure while one item is applied leaves the items before it applied and that item
/// and its fact as they were, waiting for the next night.
#[test]
fn a_failed_item_rolls_back_alone() {
    let mut g = garden();
    let (dog, new) = repeated(&mut g);
    let cat = write(&mut g, "Ada has a grey cat.", c7::SourceKind::Owner, T0 + 2);
    let kitten = write(
        &mut g,
        "Ada's grey cat is named Miso.",
        c7::SourceKind::Raw,
        T0 + DAY,
    );
    assert_eq!(hint(&mut g, &kitten, T0 + DAY + 1).first(), Some(&cat));
    g.store()
        .conn()
        .execute_batch(&format!(
            "CREATE TRIGGER refuse BEFORE INSERT ON reconcile_judged WHEN NEW.fact = '{kitten}'
             BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;"
        ))
        .unwrap();
    let answer = g
        .garden_reconcile_done(
            &c7::ReconcileDoneRequest {
                judge: JUDGE.into(),
                items: vec![
                    c7::ReconcileJudged {
                        fact_id: new.clone(),
                        verdicts: vec![verdict(&dog, c7::ReconcileRelation::Duplicate, "dog")],
                    },
                    c7::ReconcileJudged {
                        fact_id: kitten.clone(),
                        verdicts: vec![verdict(&cat, c7::ReconcileRelation::Duplicate, "cat")],
                    },
                ],
                stats: None,
            },
            T0 + 2 * DAY,
        )
        .unwrap();
    assert_eq!(answer.items.len(), 1);
    assert!(answer.failed.is_some());
    assert!(!current(&g, &new));
    assert!(current(&g, &kitten), "the failed item changed nothing");
    assert_eq!(proof_count(g.store(), &FactId(cat)).unwrap(), 1);
    let waiting: String = g
        .store()
        .conn()
        .query_row(
            "SELECT state FROM reconcile_queue WHERE fact = ?1",
            [&kitten],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(waiting, "hinted");
}

/// The counters: no text and no ids, only numbers, the judge's own from `stats` included.
#[test]
fn the_counters_count_hints_verdicts_undos_the_judge_and_the_wait() {
    let mut g = garden();
    let (dog, new) = repeated(&mut g);
    g.garden_reconcile_done(
        &c7::ReconcileDoneRequest {
            judge: JUDGE.into(),
            items: vec![c7::ReconcileJudged {
                fact_id: new,
                verdicts: vec![verdict(&dog, c7::ReconcileRelation::Duplicate, "dog")],
            }],
            stats: Some(c7::JudgeStats {
                calls: Some(2),
                failures: Some(1),
                timeouts: Some(0),
                tokens_in: Some(900),
                tokens_out: Some(80),
            }),
        },
        T0 + DAY + 2 * 3_600,
    )
    .unwrap();
    let s = ulei_garden::judge::stats(g.store()).unwrap();
    assert_eq!(
        (s.waiting, s.hinted),
        (1, 0),
        "the earlier fact still waits"
    );
    assert_eq!((s.hinted_lexical, s.hints_offered, s.judged), (1, 1, 1));
    assert_eq!(s.judged_lexical, 1);
    assert_eq!(s.verdicts.duplicate, 1);
    assert_eq!(s.applied.duplicate, 1);
    assert_eq!(s.compatible, 0);
    assert_eq!(
        (s.judge_calls, s.judge_failures, s.judge_tokens_in),
        (2, 1, 900)
    );
    let latency: Vec<u64> = s.latency.iter().map(|b| b.count).collect();
    assert_eq!(
        latency,
        [0, 1, 0, 0, 0, 0],
        "judged two hours after the fact"
    );
}
