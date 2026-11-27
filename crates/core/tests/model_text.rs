//! The model text of a claimed row against golden vectors that the reference expression
//! printed under `CPython` for synthetic rows (`tests/fixtures/model_text.json`).

use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use ulei_core::inbox::{Error, EventRow};
use ulei_core::prompt::model_text;

fn row(id: &str, digest: &str, packet: &[u8]) -> EventRow {
    EventRow {
        id: id.into(),
        packet: packet.to_vec(),
        digest: digest.into(),
        status: "leased".into(),
        attempts: 1,
        lease: Some("0123456789abcdef0123456789abcdef".into()),
        lease_until: Some(1_767_225_720.0),
        available: 0.0,
        session_id: None,
        outcome: None,
        created: 1_767_225_600.0,
    }
}

fn vectors() -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/model_text.json");
    serde_json::from_slice(&fs::read(path).expect("vectors")).expect("vectors are JSON")
}

#[test]
fn each_golden_vector_is_built_byte_for_byte() {
    let vectors = vectors();
    assert_ne!(vectors.len(), 0);
    for vector in vectors {
        let text = |field: &str| vector[field].as_str().expect(field).to_owned();
        let id = text("id");
        let row = row(&id, &text("digest"), text("packet").as_bytes());
        assert_eq!(model_text(&row).expect(&id), text("text"), "{id}");
    }
}

#[test]
fn a_stored_packet_without_a_numeric_source_ts_is_refused() {
    let packet = json!({"kind": "task_result", "source_ts": "1767225600", "payload": "p"});
    let out = model_text(&row(
        "ev-1",
        &"cd".repeat(32),
        packet.to_string().as_bytes(),
    ));
    assert!(matches!(out, Err(Error::StoredPacket)), "{out:?}");
}

/// The model text of a synthetic stored packet of `kind`.
fn text_of(kind: &str) -> String {
    let payload = r#"{"source": "hive", "card_id": "h_0000cafe", "text": "Synthetic reminder."}"#;
    let packet = json!({"id": "ev-1", "kind": kind, "mac": "ab".repeat(32), "payload": payload,
        "source_ts": 1_767_225_600, "trust": "untrusted-source-data", "version": 1});
    model_text(&row(
        "ev-1",
        &"cd".repeat(32),
        packet.to_string().as_bytes(),
    ))
    .expect(kind)
}

#[test]
fn a_due_reminder_or_alert_is_sent_or_withdrawn_only_through_the_tool() {
    let due = text_of("reminder_due");
    assert_eq!(
        due,
        text_of("watchdog_alert").replace("watchdog_alert", "reminder_due")
    );
    for line in [
        "Decide now with the hive_reminder tool: claim the card, then send it as is, send it edited, or withdraw it with a reason",
        "The tool's send is the only send: never post the text, a paraphrase or a confirmation in your own reply",
        "After the tool answers, return exactly NO_REPLY_DONE",
    ] {
        assert!(due.contains(line), "{line}: {due}");
    }
    assert!(!due.contains("durable task status/result"), "{due}");
    assert!(due.ends_with(r#""kind": "reminder_due", "source_ts": 1767225600, "payload_ref": "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd", "trust": "untrusted-source-data", "source_text": "{\"source\": \"hive\", \"card_id\": \"h_0000cafe\", \"text\": \"Synthetic reminder.\"}"}"#), "{due}");
}

#[test]
fn a_fallback_records_the_direct_send_and_sends_nothing() {
    let fallback = text_of("reminder_fallback");
    for line in [
        "the hive already delivered a reminder or watchdog alert to the owner directly",
        "do not send it again or repeat it, via tools or in your reply. Return exactly NO_REPLY_DONE.",
    ] {
        assert!(fallback.contains(line), "{line}: {fallback}");
    }
    assert!(!fallback.contains("hive_reminder tool"), "{fallback}");
    assert!(
        !fallback.contains("durable task status/result"),
        "{fallback}"
    );
}
