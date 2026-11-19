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
