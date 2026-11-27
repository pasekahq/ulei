//! The judge of stage d (ADR-0096): the new facts the garden hinted, grouped by entity into
//! calls, rendered as JSON lines under the system prompt `judge.txt`, and the reading of the
//! model's answer, fact by fact, into the verdicts `reconcile_done` applies. The garden decides
//! nothing by itself and calls no model; the night is the only judge.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::night::extract::{Malformed, object, only};
use crate::night::spend::estimate_tokens;
use crate::wire::c7::{Fact, ReconcileItem, ReconcileJudged, ReconcileRelation, ReconcileVerdict};
use crate::zone;

/// The system prompt of stage d.
pub const JUDGE_SYSTEM: &str = include_str!("judge.txt");

/// The version of [`JUDGE_SYSTEM`], kept with every verdict the garden records.
pub const PROMPT_VERSION: &str = "judge-v2";

/// Characters of a verdict's note kept at most (C7 `ReconcileVerdict.note`).
pub const MAX_NOTE_CHARS: usize = 500;

/// The date of a Unix time, `YYYY-MM-DD`.
fn date(at: f64) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let at = zone::rfc3339(at.max(0.0) as u64);
    at[..10].to_owned()
}

/// A JSON object with its keys in the order given, whatever order `serde_json`'s map keeps
/// (a workspace build may turn on `preserve_order`): the prompt is the same in every build and
/// in `ci/judge_eval.py`, which writes the keys sorted.
fn object_of(pairs: &[(&str, String)]) -> String {
    let fields: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}:{v}", Value::from(*k)))
        .collect();
    format!("{{{}}}", fields.join(","))
}

fn text(s: &str) -> String {
    Value::from(s).to_string()
}

/// The fields of a fact line, keys sorted, `hints` given for a new fact.
fn fact_line(fact: &Fact, hints: Option<&[&str]>) -> String {
    let mut pairs = Vec::with_capacity(5);
    if let Some(hints) = hints {
        pairs.push(("hints", Value::from(hints.to_vec()).to_string()));
    }
    pairs.push(("ref", text(fact.fact_id.as_deref().unwrap_or_default())));
    if let Some(kind) = fact.source_kind {
        pairs.push(("source", text(kind.as_str())));
    }
    pairs.push(("text", text(&fact.fact)));
    pairs.push(("valid_from", text(&date(fact.valid_from))));
    object_of(&pairs)
}

/// The `fact_id` of a fact as the take answered it.
fn id(fact: &Fact) -> &str {
    fact.fact_id.as_deref().unwrap_or_default()
}

/// The prompt of one call: per entity, in the order the batch first names it, one `entity`
/// line, its items' hints once each as `earlier` lines, then its items as `new` lines with the
/// refs of their own hints.
#[must_use]
pub fn render(batch: &[&ReconcileItem]) -> String {
    let mut entities: Vec<&str> = Vec::new();
    for item in batch {
        if !entities.contains(&item.entity.as_str()) {
            entities.push(&item.entity);
        }
    }
    let mut out = String::new();
    let mut push = |kind: &str, body: String| {
        out.push_str(&object_of(&[(kind, body)]));
        out.push('\n');
    };
    for entity in entities {
        let items: Vec<&&ReconcileItem> = batch.iter().filter(|i| i.entity == entity).collect();
        let name = items.first().map_or("", |i| i.entity_name.as_str());
        push(
            "entity",
            object_of(&[("id", text(entity)), ("name", text(name))]),
        );
        let mut shown: HashSet<&str> = HashSet::new();
        for hint in items.iter().flat_map(|i| &i.hints) {
            if shown.insert(id(hint)) {
                push("earlier", fact_line(hint, None));
            }
        }
        for item in items {
            let hints: Vec<&str> = item.hints.iter().map(id).collect();
            push("new", fact_line(&item.fact, Some(&hints)));
        }
    }
    out
}

/// The calls of one page of items: whole entities, at most `per_call` new facts and `budget`
/// estimated prompt tokens per call; an entity with more new facts than `per_call` is cut into
/// parts of `per_call`. An item alone over the budget still gets its own call.
#[must_use]
pub fn batches(items: &[ReconcileItem], per_call: usize, budget: u64) -> Vec<Vec<&ReconcileItem>> {
    let per_call = per_call.max(1);
    let mut groups: Vec<Vec<&ReconcileItem>> = Vec::new();
    for item in items {
        match groups.iter_mut().find(|g| g[0].entity == item.entity) {
            Some(group) => group.push(item),
            None => groups.push(vec![item]),
        }
    }
    let parts = groups
        .into_iter()
        .flat_map(|g| g.chunks(per_call).map(<[_]>::to_vec).collect::<Vec<_>>());
    let mut calls: Vec<Vec<&ReconcileItem>> = Vec::new();
    for part in parts {
        if let Some(last) = calls.last_mut() {
            let mut joined = last.clone();
            joined.extend(&part);
            if joined.len() <= per_call && estimate_tokens(&render(&joined)) <= budget {
                *last = joined;
                continue;
            }
        }
        calls.push(part);
    }
    calls
}

/// What [`parse`] reads from an answer: the verdicts of the new facts it decided, and how many
/// of the batch it left undecided.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    /// The decided new facts, in the batch's order.
    pub judged: Vec<ReconcileJudged>,
    /// New facts of the batch left waiting: named by no well-formed item, or by a defective one.
    pub dropped: u64,
}

/// The JSON object of an answer: the whole text, a code fence around it, else the span from its
/// first `{` to its last `}` (prose or a fence around the object).
fn answer_object(text: &str) -> Result<Map<String, Value>, Malformed> {
    object(text).or_else(|why| {
        let (Some(from), Some(to)) = (text.find('{'), text.rfind('}')) else {
            return Err(why);
        };
        if from < to {
            object(&text[from..=to]).map_err(|_| why)
        } else {
            Err(why)
        }
    })
}

/// The verdicts of one item for `fact`: only on its own hints, at most one per hint, each a
/// relation `duplicate`, `supersedes` or `contradiction` (a `compatible` one is dropped) and an
/// optional note, cut at [`MAX_NOTE_CHARS`]; `None` for any other key, hint or relation.
fn verdicts_of(item: &Map<String, Value>, fact: &ReconcileItem) -> Option<Vec<ReconcileVerdict>> {
    only(item, &["ref", "verdicts"], "").ok()?;
    let Some(Value::Array(list)) = item.get("verdicts") else {
        return None;
    };
    let mut verdicts = Vec::new();
    let mut named = HashSet::new();
    for v in list {
        let Value::Object(v) = v else { return None };
        only(v, &["hint", "relation", "note"], "").ok()?;
        let Some(Value::String(hint)) = v.get("hint") else {
            return None;
        };
        if !fact.hints.iter().any(|h| id(h) == hint) || !named.insert(hint.clone()) {
            return None;
        }
        let relation = match v.get("relation").and_then(Value::as_str) {
            Some("duplicate") => ReconcileRelation::Duplicate,
            Some("supersedes") => ReconcileRelation::Supersedes,
            Some("contradiction") => ReconcileRelation::Contradiction,
            Some("compatible") => continue,
            _ => return None,
        };
        let note = match v.get("note") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.trim().is_empty() => None,
            Some(Value::String(s)) => Some(cut(s.trim(), MAX_NOTE_CHARS)),
            Some(_) => return None,
        };
        verdicts.push(ReconcileVerdict {
            hint: hint.clone(),
            relation,
            note,
        });
    }
    Some(verdicts)
}

/// The verdicts of an answer to `batch`. The answer is one JSON object `{"items": [...]}`,
/// alone, in a code fence or within prose. Each item names one new fact of the batch by its
/// `ref` with its `verdicts` (see [`verdicts_of`]). The answer is read per new fact: a fact
/// named by a defective item (another key, a hint that is not one of its own, a hint twice, an
/// unknown relation or a note that is no text), named twice, or not named stays undecided and
/// waits, with none of its verdicts applied; the other facts are decided. An item naming no new
/// fact of the batch is ignored.
///
/// # Errors
///
/// [`Malformed`] when the answer holds no such object, or decides no new fact of the batch.
pub fn parse(text: &str, batch: &[&ReconcileItem]) -> Result<Answer, Malformed> {
    let root = answer_object(text)?;
    only(&root, &["items"], "answer has keys other than items")?;
    let Some(Value::Array(items)) = root.get("items") else {
        return Err("answer lacks the items list");
    };
    let mut judged: Vec<Option<ReconcileJudged>> = vec![None; batch.len()];
    let mut named = vec![false; batch.len()];
    let mut defective = vec![false; batch.len()];
    for value in items {
        let Value::Object(item) = value else { continue };
        let Some(Value::String(r)) = item.get("ref") else {
            continue;
        };
        let Some(at) = batch.iter().position(|i| id(&i.fact) == r) else {
            continue;
        };
        if std::mem::replace(&mut named[at], true) {
            defective[at] = true;
            continue;
        }
        match verdicts_of(item, batch[at]) {
            Some(verdicts) => {
                judged[at] = Some(ReconcileJudged {
                    fact_id: r.clone(),
                    verdicts,
                });
            }
            None => defective[at] = true,
        }
    }
    let judged: Vec<ReconcileJudged> = judged
        .into_iter()
        .zip(defective)
        .filter_map(|(j, bad)| j.filter(|_| !bad))
        .collect();
    if judged.is_empty() {
        return Err("the answer decides no new fact");
    }
    Ok(Answer {
        dropped: (batch.len() - judged.len()) as u64,
        judged,
    })
}

fn cut(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((at, _)) => s[..at].to_owned(),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::wire::c7::{EntityKind, HintBasis, SourceKind};

    fn fact(id: &str, text: &str) -> Fact {
        Fact {
            fact: text.into(),
            entity: "ada-example".into(),
            valid_from: 1_764_115_200.0,
            valid_to: None,
            source_ref: "msg:1".into(),
            score: 0.0,
            fact_id: Some(id.into()),
            kind: Some(EntityKind::Person),
            domain: None,
            source_kind: Some(SourceKind::Owner),
            expires_at: None,
            pinned: None,
            proof_count: Some(1),
            recheck_before_acting: None,
            unprocessed: None,
            disputed: None,
            conflicts_with: None,
            conflict_note: None,
        }
    }

    fn item(seq: u64, entity: &str, new: &str, hints: &[&str]) -> ReconcileItem {
        ReconcileItem {
            seq,
            entity: entity.into(),
            entity_name: format!("{entity} name"),
            fact: fact(new, &format!("text of {new}")),
            hints: hints
                .iter()
                .map(|h| fact(h, &format!("text of {h}")))
                .collect(),
            basis: HintBasis::Dense,
            embedder: Some("e5".into()),
        }
    }

    #[test]
    fn the_prompt_names_each_entity_once_with_its_earlier_facts_once_then_its_new_ones() {
        let first = item(1, "ada", "f9", &["f1", "f2"]);
        let other = item(2, "bob", "f10", &["f3"]);
        let third = item(3, "ada", "f11", &["f2", "f4"]);
        let prompt = render(&[&first, &other, &third]);
        let lines: Vec<Value> = prompt
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let kinds: Vec<String> = lines
            .iter()
            .map(|l| {
                let (k, v) = l.as_object().unwrap().iter().next().unwrap();
                format!(
                    "{k}:{}",
                    v.get("ref").or(v.get("id")).unwrap().as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "entity:ada",
                "earlier:f1",
                "earlier:f2",
                "earlier:f4",
                "new:f9",
                "new:f11",
                "entity:bob",
                "earlier:f3",
                "new:f10"
            ]
        );
        assert_eq!(lines[4]["new"]["hints"], json!(["f1", "f2"]));
        assert_eq!(lines[4]["new"]["valid_from"], "2025-11-26");
        assert_eq!(lines[4]["new"]["source"], "owner");
        assert_eq!(lines[0]["entity"]["name"], "ada name");
    }

    #[test]
    fn batches_keep_entities_whole_and_cut_large_ones_into_parts() {
        let items: Vec<ReconcileItem> = (1..=5)
            .map(|n| {
                item(
                    n,
                    if n <= 3 { "ada" } else { "bob" },
                    &format!("f{n}"),
                    &["f0"],
                )
            })
            .collect();
        let sizes = |calls: Vec<Vec<&ReconcileItem>>| -> Vec<Vec<u64>> {
            calls
                .iter()
                .map(|c| c.iter().map(|i| i.seq).collect())
                .collect()
        };
        assert_eq!(sizes(batches(&items, 25, 100_000)), [vec![1, 2, 3, 4, 5]]);
        assert_eq!(
            sizes(batches(&items, 3, 100_000)),
            [vec![1, 2, 3], vec![4, 5]]
        );
        assert_eq!(
            sizes(batches(&items, 2, 100_000)),
            [vec![1, 2], vec![3], vec![4, 5]]
        );
        assert_eq!(
            sizes(batches(&items, 25, 1)),
            [vec![1, 2, 3], vec![4, 5]],
            "an entity over the budget still goes whole, alone"
        );
    }

    #[test]
    fn an_answer_names_every_new_fact_once_with_verdicts_on_its_own_hints() {
        let a = item(1, "ada", "f9", &["f1", "f2"]);
        let b = item(2, "ada", "f10", &["f1"]);
        let batch = [&a, &b];
        let answer = r#"```json
{"items":[{"ref":"f10","verdicts":[]},
 {"ref":"f9","verdicts":[{"hint":"f2","relation":"contradiction","note":" tea vs coffee "},
  {"hint":"f1","relation":"compatible"}]}]}
```"#;
        let read = parse(answer, &batch).unwrap();
        assert_eq!(read.dropped, 0);
        let judged = read.judged;
        assert_eq!(judged[0].fact_id, "f9");
        assert_eq!(
            judged[0].verdicts,
            [ReconcileVerdict {
                hint: "f2".into(),
                relation: ReconcileRelation::Contradiction,
                note: Some("tea vs coffee".into()),
            }]
        );
        assert_eq!(judged[1].fact_id, "f10");
        assert_eq!(judged[1].verdicts, []);
        let long = format!(
            r#"{{"items":[{{"ref":"f9","verdicts":[{{"hint":"f1","relation":"duplicate","note":"{}"}}]}},{{"ref":"f10","verdicts":[]}}]}}"#,
            "x".repeat(600)
        );
        let judged = parse(&long, &batch).unwrap().judged;
        assert_eq!(
            judged[0].verdicts[0].note.as_ref().unwrap().len(),
            MAX_NOTE_CHARS
        );
    }

    /// The facts `parse` decides, by ref, and how many it left.
    fn decided(answer: &str, batch: &[&ReconcileItem]) -> (Vec<String>, u64) {
        let read = parse(answer, batch).unwrap();
        let refs = read.judged.iter().map(|j| j.fact_id.clone()).collect();
        (refs, read.dropped)
    }

    #[test]
    fn a_defective_item_leaves_only_its_own_fact_undecided() {
        let a = item(1, "ada", "f9", &["f1", "f2"]);
        let b = item(2, "ada", "f10", &["f1"]);
        let batch = [&a, &b];
        let ok = r#"{"ref":"f9","verdicts":[{"hint":"f1","relation":"duplicate"}]}"#;
        for (bad, why) in [
            (
                r#"{"ref":"f10","verdicts":[{"hint":"f2","relation":"duplicate"}]}"#,
                "a hint of another fact of the call",
            ),
            (
                r#"{"ref":"f10","verdicts":[{"hint":"f7","relation":"supersedes"}]}"#,
                "a hint of no fact",
            ),
            (
                r#"{"ref":"f10","verdicts":[{"hint":"f1","relation":"same"}]}"#,
                "an unknown relation",
            ),
            (r#"{"ref":"f10","verdicts":[{"hint":"f1"}]}"#, "no relation"),
            (
                r#"{"ref":"f10","verdicts":[{"hint":"f1","relation":"duplicate"},{"hint":"f1","relation":"supersedes"}]}"#,
                "a hint twice",
            ),
            (
                r#"{"ref":"f10","verdicts":[{"hint":"f1","relation":"contradiction","note":7}]}"#,
                "a note that is no text",
            ),
            (
                r#"{"ref":"f10","verdicts":[{"hint":"f1","relation":"duplicate","why":"x"}]}"#,
                "a verdict with another key",
            ),
            (
                r#"{"ref":"f10","verdicts":[],"why":"x"}"#,
                "an item with another key",
            ),
            (r#"{"ref":"f10","verdicts":"none"}"#, "verdicts not a list"),
            (
                r#"{"ref":"f10","verdicts":["f1"]}"#,
                "a verdict not an object",
            ),
        ] {
            let answer = format!(r#"{{"items":[{ok},{bad}]}}"#);
            assert_eq!(
                decided(&answer, &batch),
                (vec!["f9".to_owned()], 1),
                "{why}"
            );
            let answer = format!(r#"{{"items":[{bad},{ok}]}}"#);
            assert_eq!(decided(&answer, &batch).1, 1, "{why}, first");
        }
        let read = parse(
            &format!(
                r#"{{"items":[{ok},{{"ref":"f10","verdicts":[{{"hint":"f1","relation":"duplicate"}},{{"hint":"f2","relation":"duplicate"}}]}}]}}"#
            ),
            &batch,
        )
        .unwrap();
        assert_eq!(
            read.judged.len(),
            1,
            "a good verdict beside a stray one waits too"
        );
        assert_eq!(read.judged[0].verdicts[0].hint, "f1");
    }

    #[test]
    fn a_fact_left_out_or_named_twice_waits_and_an_unknown_ref_is_ignored() {
        let a = item(1, "ada", "f9", &["f1", "f2"]);
        let b = item(2, "ada", "f10", &["f1"]);
        let batch = [&a, &b];
        let ok = r#"{"ref":"f9","verdicts":[]}"#;
        assert_eq!(
            decided(&format!(r#"{{"items":[{ok}]}}"#), &batch),
            (vec!["f9".to_owned()], 1),
            "a fact left out"
        );
        assert_eq!(
            decided(
                &format!(
                    r#"{{"items":[{ok},{{"ref":"f10","verdicts":[]}},{{"ref":"f10","verdicts":[{{"hint":"f1","relation":"duplicate"}}]}}]}}"#
                ),
                &batch
            ),
            (vec!["f9".to_owned()], 1),
            "a fact named twice decides neither"
        );
        assert_eq!(
            decided(
                &format!(
                    r#"{{"items":[{{"ref":"f7","verdicts":[]}},{ok},"f10",{{"ref":10}},{{"ref":"f10","verdicts":[]}}]}}"#
                ),
                &batch
            ),
            (vec!["f9".to_owned(), "f10".to_owned()], 0),
            "an unknown ref or a stray value"
        );
    }

    #[test]
    fn the_object_is_found_in_a_fence_or_prose() {
        let a = item(1, "ada", "f9", &["f1"]);
        let batch = [&a];
        let body = r#"{"items":[{"ref":"f9","verdicts":[{"hint":"f1","relation":"duplicate"}]}]}"#;
        for answer in [
            format!("```json\n{body}\n```"),
            format!("Here are the verdicts:\n```json\n{body}\n```\nDone."),
            format!("Sure. {body} Hope this helps."),
            format!("  {body}\n"),
        ] {
            assert_eq!(
                decided(&answer, &batch),
                (vec!["f9".to_owned()], 0),
                "{answer}"
            );
        }
    }

    #[test]
    fn an_answer_that_decides_nothing_is_refused() {
        let a = item(1, "ada", "f9", &["f1", "f2"]);
        let b = item(2, "ada", "f10", &["f1"]);
        let batch = [&a, &b];
        for (answer, why) in [
            ("not json", "not json"),
            ("I cannot judge these facts.", "prose only"),
            ("{\"items\": [", "cut off"),
            ("[]", "not an object"),
            (r#"{"items":[]}"#, "no item"),
            (r#"{"items":{}}"#, "items not a list"),
            (r#"{"verdicts":[]}"#, "no items"),
            (r#"{"items":[],"extra":1}"#, "an unknown top key"),
            (
                r#"{"items":[{"ref":"f7","verdicts":[]}]}"#,
                "only an unknown ref",
            ),
            (
                r#"{"items":[{"ref":"f9","verdicts":[{"hint":"f9","relation":"duplicate"}]},{"ref":"f10","verdicts":[{"hint":"f2","relation":"duplicate"}]}]}"#,
                "every item defective",
            ),
        ] {
            assert!(parse(answer, &batch).is_err(), "{why}");
        }
    }

    #[test]
    fn the_prompt_is_the_shared_file() {
        assert!(JUDGE_SYSTEM.starts_with("You check new facts of a personal knowledge garden"));
        assert!(JUDGE_SYSTEM.contains("\"relation\":\"duplicate\""));
    }
}
