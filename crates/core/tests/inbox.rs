//! `ulei_core::inbox` through its public API on temporary roots. The lease cases replay the
//! store calls and asserted rows of the Python inbox tests, with the clock as a parameter.

use std::fmt::Write;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use ulei_core::inbox::{
    DEFER_SECONDS, Error, EventRow, Guarded, Inbox, Invalid, LEASE_TTL, MAX_OFFERS, RETRY_DELAY,
    canonical,
};

const NOW: f64 = 1_763_290_000.0;
const TEST_KEY: [u8; 32] = [7; 32];

/// A fresh owner-only directory under the system temp dir, removed on drop. `root()` is a
/// path inside it that does not exist yet.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ulei-inbox-it-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .expect("scratch dir");
        Self(dir)
    }

    fn root(&self) -> PathBuf {
        self.0.join("inbox")
    }

    /// The root created 0700 with `producer.key` holding `key`.
    fn root_with_key(&self, key: &[u8]) -> PathBuf {
        let root = self.root();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .expect("root");
        write_mode(&root.join("producer.key"), key, 0o600);
        root
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_mode(path: &Path, bytes: &[u8], mode: u32) {
    fs::write(path, bytes).expect("write");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o777
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        write!(out, "{b:02x}").expect("write to String");
        out
    })
}

/// The packet `body` with `mac` computed over its canonical JSON under `key`.
fn sign(key: &[u8], body: &Value) -> Value {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(&canonical(body).expect("canonical"));
    let tag = hex(&mac.finalize().into_bytes());
    let mut packet = body.clone();
    packet["mac"] = Value::String(tag);
    packet
}

/// A valid packet body without `mac` and with `id` = `case:<name>`.
fn body(name: &str) -> Map<String, Value> {
    let Value::Object(fields) = json!({
        "version": 1,
        "kind": "task_result",
        "id": format!("case:{name}"),
        "payload": "Card finished.",
        "source_ts": 1_763_290_000,
        "trust": "untrusted-source-data",
    }) else {
        unreachable!("object literal")
    };
    fields
}

fn signed_with(name: &str, field: &str, value: Value) -> Value {
    let mut fields = body(name);
    fields.insert(field.to_owned(), value);
    sign(&TEST_KEY, &Value::Object(fields))
}

fn open_test_inbox(scratch: &Scratch) -> Inbox {
    Inbox::open(scratch.root_with_key(&TEST_KEY)).expect("open")
}

#[test]
fn open_creates_owner_only_root_key_and_database() {
    let scratch = Scratch::new();
    let root = scratch.root();
    Inbox::open(&root).expect("open");
    assert_eq!(mode(&root), 0o700);
    assert_eq!(mode(&root.join("producer.key")), 0o600);
    assert_eq!(fs::read(root.join("producer.key")).expect("key").len(), 32);
    assert_eq!(mode(&root.join("events.sqlite3")), 0o600);
}

#[test]
fn second_open_reuses_the_key() {
    let scratch = Scratch::new();
    let root = scratch.root();
    drop(Inbox::open(&root).expect("first open"));
    let key = fs::read(root.join("producer.key")).expect("key");
    let mut inbox = Inbox::open(&root).expect("second open");
    assert_eq!(fs::read(root.join("producer.key")).expect("key"), key);
    let packet = sign(&key, &Value::Object(body("reused-key")));
    assert_eq!(
        inbox.publish(&packet, NOW).expect("publish"),
        "case:reused-key"
    );
}

#[test]
fn symlinked_root_is_refused() {
    let scratch = Scratch::new();
    let real = scratch.0.join("real");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&real)
        .expect("dir");
    symlink(&real, scratch.root()).expect("symlink");
    assert!(matches!(
        Inbox::open(scratch.root()),
        Err(Error::Symlink(Guarded::Root))
    ));
}

#[test]
fn root_with_group_or_other_bits_is_refused() {
    for bits in [0o740, 0o710, 0o704, 0o701] {
        let scratch = Scratch::new();
        let root = scratch.root();
        fs::create_dir(&root).expect("dir");
        fs::set_permissions(&root, fs::Permissions::from_mode(bits)).expect("chmod");
        assert!(
            matches!(
                Inbox::open(&root),
                Err(Error::GroupOrOtherMode(Guarded::Root))
            ),
            "mode {bits:o}"
        );
    }
}

#[test]
fn symlinked_key_is_refused() {
    for target_exists in [true, false] {
        let scratch = Scratch::new();
        let root = scratch.root();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .expect("root");
        let target = scratch.0.join("elsewhere.key");
        if target_exists {
            write_mode(&target, &TEST_KEY, 0o600);
        }
        symlink(&target, root.join("producer.key")).expect("symlink");
        assert!(
            matches!(Inbox::open(&root), Err(Error::Symlink(Guarded::Key))),
            "target exists: {target_exists}"
        );
        assert_eq!(
            target.exists(),
            target_exists,
            "the key is not written through the link"
        );
    }
}

#[test]
fn group_or_other_accessible_key_is_refused() {
    for bits in [0o640, 0o620, 0o604, 0o602] {
        let scratch = Scratch::new();
        let root = scratch.root_with_key(&TEST_KEY);
        fs::set_permissions(root.join("producer.key"), fs::Permissions::from_mode(bits))
            .expect("chmod");
        assert!(
            matches!(
                Inbox::open(&root),
                Err(Error::GroupOrOtherMode(Guarded::Key))
            ),
            "mode {bits:o}"
        );
    }
}

#[test]
fn key_of_wrong_length_is_refused() {
    for len in [0, 31, 33] {
        let scratch = Scratch::new();
        let root = scratch.root_with_key(&vec![1; len]);
        assert!(
            matches!(Inbox::open(&root), Err(Error::KeyLength(n)) if n == len),
            "length {len}"
        );
    }
}

#[test]
fn symlinked_database_is_refused() {
    let scratch = Scratch::new();
    let root = scratch.root_with_key(&TEST_KEY);
    symlink(
        scratch.0.join("elsewhere.sqlite3"),
        root.join("events.sqlite3"),
    )
    .expect("symlink");
    assert!(matches!(
        Inbox::open(&root),
        Err(Error::Symlink(Guarded::Database))
    ));
}

#[test]
fn golden_vectors_reproduce_or_refuse() {
    let vectors_file = fs::read(golden("vectors.json")).expect("vectors");
    let golden: Value = serde_json::from_slice(&vectors_file).expect("json");
    let scratch = Scratch::new();
    let key = unhex(golden["key_hex"].as_str().expect("key_hex"));
    let mut inbox = Inbox::open(scratch.root_with_key(&key)).expect("open");
    let vectors = golden["vectors"].as_array().expect("vectors");
    assert_eq!(vectors.len(), 12);
    for v in vectors {
        let name = v["name"].as_str().expect("name");
        let mut packet = v["packet"].clone();
        packet["mac"] = v["mac"].clone();
        let id = v["packet"]["id"].as_str().expect("id");
        match v["rust"].as_str() {
            Some("reproduce") => {
                let canonical_hex = unhex(v["canonical_hex"].as_str().expect("canonical_hex"));
                assert_eq!(
                    canonical(&v["packet"]).expect(name),
                    canonical_hex,
                    "{name}"
                );
                assert_eq!(inbox.publish(&packet, NOW).expect(name), id, "{name}");
                let row = inbox.get(id).expect(name).expect(name);
                assert_eq!(
                    row.packet,
                    unhex(v["stored_hex"].as_str().expect("stored_hex")),
                    "{name}"
                );
                assert_eq!(row.digest, v["digest"].as_str().expect("digest"), "{name}");
            }
            Some("refuse") => {
                assert!(
                    matches!(inbox.publish(&packet, NOW), Err(Error::InvalidPacket(_))),
                    "{name}"
                );
                assert!(inbox.get(id).expect(name).is_none(), "{name}");
            }
            other => panic!("{name}: unknown marking {other:?}"),
        }
    }
}

enum Expect {
    Stored,
    Unauthenticated,
    Invalid(Invalid),
}

/// Publishes each `(name, packet, expect)` at `NOW` into one fresh store.
fn assert_cases(cases: Vec<(&str, Value, Expect)>) {
    let scratch = Scratch::new();
    let mut inbox = open_test_inbox(&scratch);
    for (name, packet, expect) in cases {
        let got = inbox.publish(&packet, NOW);
        match expect {
            Expect::Stored => {
                assert_eq!(
                    got.expect(name),
                    packet["id"].as_str().expect(name),
                    "{name}"
                );
            }
            Expect::Unauthenticated => {
                assert!(
                    matches!(got, Err(Error::Unauthenticated)),
                    "{name}: {got:?}"
                );
            }
            Expect::Invalid(why) => assert!(
                matches!(got, Err(Error::InvalidPacket(w)) if w == why),
                "{name}: {got:?}"
            ),
        }
    }
}

fn invalid(why: Invalid) -> Expect {
    Expect::Invalid(why)
}

#[test]
fn fields_are_checked_before_the_mac_and_the_mac_before_values() {
    let mut extra = body("extra");
    extra.insert("note".to_owned(), json!("x"));
    let mut extra_bad_mac = sign(&TEST_KEY, &Value::Object(extra.clone()));
    extra_bad_mac["mac"] = json!("0");
    let mut missing = body("missing");
    missing.remove("trust");
    let mut tampered = signed_with("tampered", "payload", json!("Original."));
    tampered["payload"] = json!("Changed.");
    let mut upper = signed_with("upper-mac", "payload", json!("Card finished."));
    upper["mac"] = json!(upper["mac"].as_str().expect("mac").to_uppercase());
    let mut numeric_mac = signed_with("numeric-mac", "payload", json!("Card finished."));
    numeric_mac["mac"] = json!(1);
    let mut bad_version_bad_mac = signed_with("bad-version", "version", json!(2));
    bad_version_bad_mac["payload"] = json!("Changed.");
    let mut exponent_bad_mac = Value::Object(body("ts1e5"));
    exponent_bad_mac["source_ts"] = json!(1e-5);
    exponent_bad_mac["mac"] = json!("0");
    assert_cases(vec![
        (
            "not an object",
            json!(["version", 1]),
            invalid(Invalid::Fields),
        ),
        (
            "extra field",
            sign(&TEST_KEY, &Value::Object(extra)),
            invalid(Invalid::Fields),
        ),
        (
            "extra field, bad MAC",
            extra_bad_mac,
            invalid(Invalid::Fields),
        ),
        (
            "missing field",
            sign(&TEST_KEY, &Value::Object(missing)),
            invalid(Invalid::Fields),
        ),
        ("tampered payload", tampered, Expect::Unauthenticated),
        ("uppercase MAC", upper, Expect::Unauthenticated),
        ("MAC not a string", numeric_mac, Expect::Unauthenticated),
        (
            "bad version, bad MAC",
            bad_version_bad_mac,
            Expect::Unauthenticated,
        ),
        (
            "ts 1e-5, bad MAC",
            exponent_bad_mac,
            invalid(Invalid::ExponentFloat),
        ),
    ]);
}

#[test]
fn version_kind_and_trust_are_checked() {
    assert_cases(vec![
        (
            "version 2",
            signed_with("v2", "version", json!(2)),
            invalid(Invalid::Version),
        ),
        (
            "version 1.0",
            signed_with("v1f", "version", json!(1.0)),
            invalid(Invalid::Version),
        ),
        (
            "version true",
            signed_with("vt", "version", json!(true)),
            invalid(Invalid::Version),
        ),
        (
            "unknown kind",
            signed_with("kind", "kind", json!("nope")),
            invalid(Invalid::Kind),
        ),
        (
            "C1-only kind",
            signed_with("c1", "kind", json!("garden_miss")),
            Expect::Stored,
        ),
        (
            "trust",
            signed_with("trust", "trust", json!("trusted")),
            invalid(Invalid::Trust),
        ),
    ]);
}

#[test]
fn id_must_match_the_python_pattern() {
    let id = |text: &str| signed_with("", "id", json!(text));
    assert_cases(vec![
        ("96-char id", id(&"i".repeat(96)), Expect::Stored),
        ("97-char id", id(&"i".repeat(97)), invalid(Invalid::Id)),
        ("empty id", id(""), invalid(Invalid::Id)),
        ("id with space", id("a b"), invalid(Invalid::Id)),
        ("non-ASCII id", id("caf\u{e9}"), invalid(Invalid::Id)),
    ]);
}

#[test]
fn payload_must_be_non_blank_and_at_most_65536_bytes() {
    let wide = "я".repeat(32_768);
    let payload = |name: &str, value: Value| signed_with(name, "payload", value);
    assert_cases(vec![
        (
            "65536 B",
            payload("p65536", json!("p".repeat(65_536))),
            Expect::Stored,
        ),
        (
            "65537 B",
            payload("p65537", json!("p".repeat(65_537))),
            invalid(Invalid::Payload),
        ),
        (
            "wide 65536 B",
            payload("w65536", json!(wide)),
            Expect::Stored,
        ),
        (
            "wide 65537 B",
            payload("w65537", json!(format!("{wide}p"))),
            invalid(Invalid::Payload),
        ),
        (
            "whitespace",
            payload("ws", json!(" \t\n\u{3000}")),
            invalid(Invalid::Payload),
        ),
        (
            "separators",
            payload("sep", json!("\u{1c}\u{1f}")),
            invalid(Invalid::Payload),
        ),
        (
            "empty",
            payload("empty", json!("")),
            invalid(Invalid::Payload),
        ),
        (
            "not a string",
            payload("num", json!(5)),
            invalid(Invalid::Payload),
        ),
    ]);
}

#[test]
fn source_ts_must_be_a_number_from_zero_to_now_plus_300() {
    let ts = |name: &str, value: Value| signed_with(name, "source_ts", value);
    assert_cases(vec![
        (
            "now + 300",
            ts("i300", json!(1_763_290_300_u64)),
            Expect::Stored,
        ),
        (
            "now + 301",
            ts("i301", json!(1_763_290_301_u64)),
            invalid(Invalid::SourceTs),
        ),
        (
            "float now + 300",
            ts("f300", json!(1_763_290_300.0)),
            Expect::Stored,
        ),
        (
            "float now + 300.5",
            ts("f3005", json!(1_763_290_300.5)),
            invalid(Invalid::SourceTs),
        ),
        ("0", ts("zero", json!(0)), Expect::Stored),
        ("-0.0", ts("neg-zero", json!(-0.0)), Expect::Stored),
        ("-1", ts("minus-one", json!(-1)), invalid(Invalid::SourceTs)),
        (
            "-0.5",
            ts("minus-half", json!(-0.5)),
            invalid(Invalid::SourceTs),
        ),
        ("true", ts("true", json!(true)), invalid(Invalid::SourceTs)),
        (
            "string",
            ts("string", json!("1763290000")),
            invalid(Invalid::SourceTs),
        ),
    ]);
}

#[test]
fn non_finite_float_tokens_are_invalid_packets() {
    let scratch = Scratch::new();
    let mut inbox = open_test_inbox(&scratch);
    for token in ["NaN", "Infinity", "-Infinity", "1e999"] {
        let text = format!(
            r#"{{"id":"case:nan","kind":"weekly","mac":"00","payload":"x","source_ts":{token},"trust":"untrusted-source-data","version":1}}"#
        );
        let got = inbox.publish_json(text.as_bytes(), NOW);
        assert!(
            matches!(got, Err(Error::InvalidPacket(Invalid::Json))),
            "{token}: {got:?}"
        );
    }
    let valid = sign(&TEST_KEY, &Value::Object(body("json-text")));
    let text = serde_json::to_vec(&valid).expect("text");
    assert_eq!(
        inbox.publish_json(&text, NOW).expect("publish"),
        "case:json-text"
    );
}

fn row_count(root: &Path) -> i64 {
    let db = rusqlite::Connection::open(root.join("events.sqlite3")).expect("db");
    db.query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .expect("count")
}

#[test]
fn republishing_the_same_packet_is_idempotent() {
    let scratch = Scratch::new();
    let root = scratch.root_with_key(&TEST_KEY);
    let mut inbox = Inbox::open(&root).expect("open");
    let packet = sign(&TEST_KEY, &Value::Object(body("again")));
    inbox.publish(&packet, NOW).expect("first");
    let first = inbox.get("case:again").expect("get").expect("row");
    assert_eq!(
        inbox.publish(&packet, NOW + 60.0).expect("second"),
        "case:again"
    );
    assert_eq!(inbox.get("case:again").expect("get").expect("row"), first);
    assert_eq!(row_count(&root), 1);
}

#[test]
fn same_id_with_other_content_is_a_conflict_that_keeps_the_row() {
    let scratch = Scratch::new();
    let root = scratch.root_with_key(&TEST_KEY);
    let mut inbox = Inbox::open(&root).expect("open");
    inbox
        .publish(&signed_with("dup", "payload", json!("First.")), NOW)
        .expect("first");
    let first = inbox.get("case:dup").expect("get").expect("row");
    let got = inbox.publish(&signed_with("dup", "payload", json!("Second.")), NOW + 1.0);
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    assert_eq!(inbox.get("case:dup").expect("get").expect("row"), first);
    assert_eq!(inbox.payload("case:dup").expect("payload"), "First.");
    assert_eq!(row_count(&root), 1);
}

#[test]
fn stored_row_survives_reopen() {
    let scratch = Scratch::new();
    let root = scratch.root_with_key(&TEST_KEY);
    let packet = signed_with("durable", "payload", json!("Привет, owner."));
    let mut unsigned = packet.clone();
    unsigned.as_object_mut().expect("object").remove("mac");
    let digest = hex(&Sha256::digest(canonical(&unsigned).expect("canonical")));
    let mut inbox = Inbox::open(&root).expect("open");
    inbox.publish(&packet, NOW + 0.25).expect("publish");
    drop(inbox);

    let inbox = Inbox::open(&root).expect("reopen");
    let expected = EventRow {
        id: "case:durable".to_owned(),
        packet: canonical(&packet).expect("canonical"),
        digest,
        status: "pending".to_owned(),
        attempts: 0,
        lease: None,
        lease_until: None,
        available: 0.0,
        session_id: None,
        outcome: None,
        created: NOW + 0.25,
    };
    assert_eq!(inbox.get("case:durable").expect("get"), Some(expected));
    assert_eq!(
        inbox.payload("case:durable").expect("payload"),
        "Привет, owner."
    );
}

#[test]
fn unknown_id_has_no_row_and_no_payload() {
    let scratch = Scratch::new();
    let inbox = open_test_inbox(&scratch);
    assert_eq!(inbox.get("case:missing").expect("get"), None);
    assert!(matches!(
        inbox.payload("case:missing"),
        Err(Error::NotFound)
    ));
}

fn publish_at(inbox: &mut Inbox, name: &str, at: f64) -> String {
    inbox
        .publish(&sign(&TEST_KEY, &Value::Object(body(name))), at)
        .expect("publish")
}

fn lease(row: &EventRow) -> &str {
    row.lease.as_deref().expect("claimed row has a lease")
}

fn stored(inbox: &Inbox, id: &str) -> EventRow {
    inbox.get(id).expect("get").expect("row")
}

fn claim(inbox: &mut Inbox, at: f64, ttl: f64) -> EventRow {
    inbox.claim(at, ttl).expect("claim").expect("claimable row")
}

/// Claims the oldest claimable event at `at` and starts it.
fn processing(inbox: &mut Inbox, at: f64, ttl: f64) -> EventRow {
    let row = claim(inbox, at, ttl);
    assert!(inbox.start(&row.id, lease(&row), "session").expect("start"));
    row
}

/// Claims, starts and defers the only event `offers` times, each claim when the previous
/// deferral is due; returns the time the last one is due.
fn deferred(inbox: &mut Inbox, offers: i64, mut at: f64) -> f64 {
    for _ in 0..offers {
        let row = processing(inbox, at, LEASE_TTL);
        assert!(
            inbox
                .ack(&row.id, lease(&row), "deferred", at)
                .expect("ack")
        );
        at += DEFER_SECONDS;
    }
    at
}

fn settle(inbox: &mut Inbox, at: f64, outcome: &str) {
    let row = processing(inbox, at, LEASE_TTL);
    assert!(inbox.ack(&row.id, lease(&row), outcome, at).expect("ack"));
}

#[derive(Debug)]
enum Change {
    Unchanged,
    ToPending,
    ToUncertain(&'static str),
}

#[test]
fn recover_applies_the_four_python_rules() {
    type Setup = fn(&mut Inbox) -> f64;
    let cases: [(&str, Setup, Change); 12] = [
        (
            "expired leased",
            |q| claim(q, NOW + 10.0, 10.0).lease_until.expect("until") + 1.0,
            Change::ToPending,
        ),
        (
            "leased at its expiry instant",
            |q| claim(q, NOW + 10.0, 10.0).lease_until.expect("until"),
            Change::Unchanged,
        ),
        (
            "expired processing",
            |q| processing(q, NOW + 10.0, 10.0).lease_until.expect("until") + 1.0,
            Change::ToUncertain("expired-processing"),
        ),
        (
            "processing at its expiry instant",
            |q| processing(q, NOW + 10.0, 10.0).lease_until.expect("until"),
            Change::Unchanged,
        ),
        (
            "due deferred after 3 offers",
            |q| deferred(q, 3, NOW + 10.0),
            Change::ToUncertain("deferred-exhausted"),
        ),
        (
            "due deferred after 2 offers",
            |q| deferred(q, 2, NOW + 10.0),
            Change::ToPending,
        ),
        (
            "due deferred after 1 offer",
            |q| deferred(q, 1, NOW + 10.0),
            Change::ToPending,
        ),
        (
            "deferred before due, 3 offers",
            |q| deferred(q, 3, NOW + 10.0) - 1.0,
            Change::Unchanged,
        ),
        (
            "deferred before due, 1 offer",
            |q| deferred(q, 1, NOW + 10.0) - 1.0,
            Change::Unchanged,
        ),
        ("pending", |_| NOW + 1e6, Change::Unchanged),
        (
            "done",
            |q| {
                settle(q, NOW + 10.0, "success");
                NOW + 1e6
            },
            Change::Unchanged,
        ),
        (
            "uncertain",
            |q| {
                settle(q, NOW + 10.0, "uncertain");
                NOW + 1e6
            },
            Change::Unchanged,
        ),
    ];
    for (name, setup, change) in cases {
        let scratch = Scratch::new();
        let mut inbox = open_test_inbox(&scratch);
        let id = publish_at(&mut inbox, "recover", NOW);
        let at = setup(&mut inbox);
        let mut expected = stored(&inbox, &id);
        inbox.recover(at).expect(name);
        match change {
            Change::Unchanged => {}
            Change::ToPending => {
                expected.status = "pending".to_owned();
                expected.lease = None;
            }
            Change::ToUncertain(outcome) => {
                expected.status = "uncertain".to_owned();
                expected.outcome = Some(outcome.to_owned());
            }
        }
        assert_eq!(stored(&inbox, &id), expected, "{name}: {change:?}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Call {
    Start,
    Renew,
    Retry,
    Ack,
}

const CALLS: [Call; 4] = [Call::Start, Call::Renew, Call::Retry, Call::Ack];

fn call(inbox: &mut Inbox, call: Call, id: &str, lease: &str, at: f64) -> bool {
    match call {
        Call::Start => inbox.start(id, lease, "session"),
        Call::Renew => inbox.renew(id, lease, at, LEASE_TTL),
        Call::Retry => inbox.retry(id, lease, at, RETRY_DELAY),
        Call::Ack => inbox.ack(id, lease, "success", at),
    }
    .expect("lease call")
}

/// `before` after `call` at `at` was applied.
fn applied(before: &EventRow, call: Call, at: f64) -> EventRow {
    let mut want = before.clone();
    match call {
        Call::Start => {
            "processing".clone_into(&mut want.status);
            want.session_id = Some("session".to_owned());
        }
        Call::Renew => want.lease_until = Some(at + LEASE_TTL),
        Call::Retry => {
            "pending".clone_into(&mut want.status);
            want.lease = None;
            want.available = at + RETRY_DELAY;
        }
        Call::Ack => {
            "done".clone_into(&mut want.status);
            want.outcome = Some("success".to_owned());
        }
    }
    want
}

type GuardSetup = fn(&mut Inbox) -> EventRow;

/// Each state with the calls its own claim token passes; any other token passes none.
fn guard_states() -> [(&'static str, GuardSetup, &'static [Call]); 9] {
    [
        (
            "leased",
            |q| claim(q, NOW + 990.0, LEASE_TTL),
            &[Call::Start, Call::Renew, Call::Retry],
        ),
        (
            "processing",
            |q| processing(q, NOW + 990.0, LEASE_TTL),
            &[Call::Renew, Call::Ack],
        ),
        (
            "leased past its expiry, not yet recovered",
            |q| claim(q, NOW + 10.0, 10.0),
            &[Call::Start, Call::Renew, Call::Retry],
        ),
        (
            "processing past its expiry, not yet recovered",
            |q| processing(q, NOW + 10.0, 10.0),
            &[Call::Renew, Call::Ack],
        ),
        (
            "expired lease recovered to pending",
            |q| {
                let row = claim(q, NOW + 10.0, 10.0);
                q.recover(NOW + 500.0).expect("recover");
                row
            },
            &[],
        ),
        (
            "expired processing recovered to uncertain",
            |q| {
                let row = processing(q, NOW + 10.0, 10.0);
                q.recover(NOW + 500.0).expect("recover");
                row
            },
            &[],
        ),
        (
            "retried to pending",
            |q| {
                let row = claim(q, NOW + 990.0, LEASE_TTL);
                assert!(
                    q.retry(&row.id, lease(&row), NOW + 990.0, 0.0)
                        .expect("retry")
                );
                row
            },
            &[],
        ),
        (
            "done",
            |q| {
                let row = processing(q, NOW + 990.0, LEASE_TTL);
                assert!(
                    q.ack(&row.id, lease(&row), "silent", NOW + 990.0)
                        .expect("ack")
                );
                row
            },
            &[],
        ),
        (
            "deferred",
            |q| {
                let row = processing(q, NOW + 990.0, LEASE_TTL);
                assert!(
                    q.ack(&row.id, lease(&row), "deferred", NOW + 990.0)
                        .expect("ack")
                );
                row
            },
            &[],
        ),
    ]
}

#[test]
fn lease_calls_require_the_token_and_the_status() {
    let foreign = "0123456789abcdef0123456789abcdef";
    for (name, setup, passes) in guard_states() {
        for c in CALLS {
            let scratch = Scratch::new();
            let mut inbox = open_test_inbox(&scratch);
            let id = publish_at(&mut inbox, "guard", NOW);
            let held = setup(&mut inbox);
            let before = stored(&inbox, &id);
            let at = NOW + 1000.0;
            for (token, target) in [(foreign, id.as_str()), (lease(&held), "case:unknown")] {
                assert!(
                    !call(&mut inbox, c, target, token, at),
                    "{name}: {c:?} {token} {target}"
                );
                assert_eq!(
                    stored(&inbox, &id),
                    before,
                    "{name}: {c:?} refused yet changed"
                );
            }
            let passes = passes.contains(&c);
            assert_eq!(
                call(&mut inbox, c, &id, lease(&held), at),
                passes,
                "{name}: {c:?}"
            );
            let want = if passes {
                applied(&before, c, at)
            } else {
                before
            };
            assert_eq!(stored(&inbox, &id), want, "{name}: {c:?}");
        }
    }
}

#[test]
fn claim_skips_rows_not_available_or_under_a_valid_lease() {
    type Setup = fn(&mut Inbox) -> (f64, Option<f64>);
    // Each setup returns the last instant nothing is claimable and, if any, the first
    // instant the row is claimable again.
    let cases: [(&str, Setup); 6] = [
        ("retried with a delay", |q| {
            let row = claim(q, NOW + 10.0, LEASE_TTL);
            assert!(
                q.retry(&row.id, lease(&row), NOW + 10.0, 5.0)
                    .expect("retry")
            );
            (NOW + 14.999, Some(NOW + 15.0))
        }),
        ("deferred", |q| {
            let due = deferred(q, 1, NOW + 10.0);
            (due - 0.001, Some(due))
        }),
        ("leased", |q| {
            let until = claim(q, NOW + 10.0, 10.0).lease_until.expect("until");
            (until, Some(until + 0.001))
        }),
        ("processing", |q| {
            let until = processing(q, NOW + 10.0, 10.0).lease_until.expect("until");
            (until, None)
        }),
        ("done", |q| {
            settle(q, NOW + 10.0, "success");
            (NOW + 1e6, None)
        }),
        ("uncertain", |q| {
            settle(q, NOW + 10.0, "uncertain");
            (NOW + 1e6, None)
        }),
    ];
    for (name, setup) in cases {
        let scratch = Scratch::new();
        let mut inbox = open_test_inbox(&scratch);
        let id = publish_at(&mut inbox, "claimable", NOW);
        let (blocked, open_at) = setup(&mut inbox);
        let before = stored(&inbox, &id);
        assert_eq!(inbox.claim(blocked, LEASE_TTL).expect(name), None, "{name}");
        assert_eq!(stored(&inbox, &id).attempts, before.attempts, "{name}");
        let Some(at) = open_at else {
            assert_eq!(
                inbox.claim(NOW + 1e7, LEASE_TTL).expect(name),
                None,
                "{name}"
            );
            continue;
        };
        let row = claim(&mut inbox, at, LEASE_TTL);
        assert_eq!(row.attempts, before.attempts + 1, "{name}");
        assert_ne!(row.lease, before.lease, "{name}");
    }
}

#[test]
fn claim_leases_the_oldest_by_created_then_id_and_returns_the_full_row() {
    let scratch = Scratch::new();
    let mut inbox = open_test_inbox(&scratch);
    let late_b = publish_at(&mut inbox, "b", NOW + 1.0);
    let late_a = publish_at(&mut inbox, "a", NOW + 1.0);
    let early_c = publish_at(&mut inbox, "c", NOW);
    let mut leases = Vec::new();
    for want in [&early_c, &late_a, &late_b] {
        let before = stored(&inbox, want);
        let row = claim(&mut inbox, NOW + 2.0, 30.0);
        assert_eq!(&row.id, want);
        assert_eq!(Some(&row), inbox.get(want).expect("get").as_ref());
        let token = lease(&row);
        assert!(
            token.len() == 32
                && token
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{token}"
        );
        leases.push(token.to_owned());
        let mut expected = before;
        expected.status = "leased".to_owned();
        expected.lease = row.lease.clone();
        expected.lease_until = Some(NOW + 32.0);
        expected.attempts = 1;
        assert_eq!(row, expected);
    }
    leases.sort();
    leases.dedup();
    assert_eq!(leases.len(), 3, "every claim draws a fresh token");
    assert_eq!(inbox.claim(NOW + 2.0, 30.0).expect("claim"), None);
}

#[test]
fn ack_settles_by_outcome_and_keeps_available_unless_deferred() {
    for (outcome, status) in [
        ("success", "done"),
        ("silent", "done"),
        ("uncertain", "uncertain"),
        ("deferred", "deferred"),
    ] {
        let scratch = Scratch::new();
        let mut inbox = open_test_inbox(&scratch);
        let id = publish_at(&mut inbox, "ack", NOW);
        let first = claim(&mut inbox, NOW + 10.0, LEASE_TTL);
        assert!(
            inbox
                .retry(&id, lease(&first), NOW + 10.0, 5.0)
                .expect("retry")
        );
        let row = processing(&mut inbox, NOW + 15.0, LEASE_TTL);
        let before = stored(&inbox, &id);
        assert!(
            inbox
                .ack(&id, lease(&row), outcome, NOW + 40.0)
                .expect(outcome)
        );
        let mut expected = before;
        expected.status = status.to_owned();
        expected.outcome = Some(outcome.to_owned());
        expected.available = if outcome == "deferred" {
            NOW + 40.0 + DEFER_SECONDS
        } else {
            NOW + 15.0
        };
        assert_eq!(stored(&inbox, &id), expected, "{outcome}");
    }
}

#[test]
fn ack_refuses_an_outcome_outside_c1_before_any_check() {
    let scratch = Scratch::new();
    let mut inbox = open_test_inbox(&scratch);
    let id = publish_at(&mut inbox, "bad-ack", NOW);
    let row = processing(&mut inbox, NOW + 10.0, LEASE_TTL);
    let before = stored(&inbox, &id);
    for outcome in ["done", "failure", "SUCCESS", "", "expired-processing"] {
        for token in [lease(&row), "0123456789abcdef0123456789abcdef"] {
            assert!(
                matches!(
                    inbox.ack(&id, token, outcome, NOW + 20.0),
                    Err(Error::InvalidOutcome)
                ),
                "{outcome:?}"
            );
        }
    }
    assert_eq!(stored(&inbox, &id), before);
}

#[test]
fn crash_restart_leases_retry_and_uncertain_do_not_replay() {
    let scratch = Scratch::new();
    let root = scratch.root_with_key(&TEST_KEY);
    let mut q = Inbox::open(&root).expect("open");
    let one = publish_at(&mut q, "one", NOW);
    let first = claim(&mut q, NOW + 10.0, 10.0);
    assert_eq!(q.claim(NOW + 11.0, LEASE_TTL).expect("claim"), None);
    let mut again = Inbox::open(&root).expect("restart");
    let second = claim(&mut again, NOW + 21.0, LEASE_TTL);
    assert_eq!((second.id.as_str(), second.attempts), (one.as_str(), 2));
    assert_ne!(first.lease, second.lease);
    assert!(
        !q.start(&one, lease(&first), "stale-session")
            .expect("start")
    );
    assert!(
        q.start(&one, lease(&second), "current-session")
            .expect("start")
    );
    again.recover(NOW + 1000.0).expect("recover");
    let row = stored(&q, &one);
    assert_eq!(
        (row.status.as_str(), row.outcome.as_deref()),
        ("uncertain", Some("expired-processing"))
    );
    assert_eq!(q.claim(NOW + 2000.0, LEASE_TTL).expect("claim"), None);
    assert!(
        !q.ack(&one, lease(&second), "success", NOW + 2000.0)
            .expect("ack")
    );
    let two = publish_at(&mut q, "two", NOW);
    let third = claim(&mut q, NOW + 30.0, LEASE_TTL);
    assert_eq!(third.id, two);
    assert!(
        q.retry(&two, lease(&third), NOW + 30.0, 5.0)
            .expect("retry")
    );
    assert_eq!(q.claim(NOW + 31.0, LEASE_TTL).expect("claim"), None);
    let fourth = claim(&mut q, NOW + 36.0, LEASE_TTL);
    assert!(q.start(&two, lease(&fourth), "new-session").expect("start"));
    assert!(
        q.ack(&two, lease(&fourth), "silent", NOW + 36.0)
            .expect("ack")
    );
    assert_eq!(stored(&q, &two).status, "done");
    assert_eq!(q.claim(NOW + 9999.0, LEASE_TTL).expect("claim"), None);
}

#[test]
fn malformed_packets_leave_no_row() {
    let scratch = Scratch::new();
    let root = scratch.root_with_key(&TEST_KEY);
    let mut inbox = Inbox::open(&root).expect("open");
    for extra in ["session_id", "session_key", "owner_id", "payload_ref"] {
        let mut fields = body("id");
        fields.insert(extra.to_owned(), json!("../../other-session"));
        let got = inbox.publish(&sign(&TEST_KEY, &Value::Object(fields)), NOW);
        assert!(
            matches!(got, Err(Error::InvalidPacket(Invalid::Fields))),
            "{extra}"
        );
    }
    for (field, value, why) in [
        ("kind", json!("news"), Invalid::Kind),
        ("id", json!("../id"), Invalid::Id),
        ("payload", json!("x".repeat(65_537)), Invalid::Payload),
        ("payload", json!(""), Invalid::Payload),
    ] {
        let got = inbox.publish(&signed_with("a", field, value), NOW);
        assert!(
            matches!(got, Err(Error::InvalidPacket(w)) if w == why),
            "{field}: {got:?}"
        );
    }
    let nan = br#"{"id":"a","kind":"weekly","mac":"00","payload":"facts","source_ts":NaN,"trust":"untrusted-source-data","version":1}"#;
    assert!(matches!(
        inbox.publish_json(nan, NOW),
        Err(Error::InvalidPacket(Invalid::Json))
    ));
    assert_eq!(row_count(&root), 0);
}

#[test]
fn rejected_turn_retry_returns_the_event_to_pending_until_the_delay_passes() {
    let scratch = Scratch::new();
    let mut inbox = open_test_inbox(&scratch);
    let id = publish_at(&mut inbox, "one", NOW);
    let row = claim(&mut inbox, NOW + 10.0, LEASE_TTL);
    assert!(
        inbox
            .retry(&id, lease(&row), NOW + 10.0, RETRY_DELAY)
            .expect("retry")
    );
    let after = stored(&inbox, &id);
    assert_eq!(
        (
            after.status.as_str(),
            after.lease.as_deref(),
            after.attempts,
            after.available
        ),
        ("pending", None, 1, NOW + 10.0 + RETRY_DELAY)
    );
    assert_eq!(inbox.claim(NOW + 24.0, LEASE_TTL).expect("claim"), None);
    assert_eq!(claim(&mut inbox, NOW + 25.0, LEASE_TTL).attempts, 2);
}

#[test]
fn success_and_silent_settle_done_and_are_not_claimed_again() {
    for outcome in ["success", "silent"] {
        let scratch = Scratch::new();
        let mut inbox = open_test_inbox(&scratch);
        let id = publish_at(&mut inbox, "one", NOW);
        let row = claim(&mut inbox, NOW + 10.0, LEASE_TTL);
        assert!(
            inbox
                .start(&id, lease(&row), "new-current-session")
                .expect("start")
        );
        assert!(
            inbox
                .ack(&id, lease(&row), outcome, NOW + 20.0)
                .expect("ack")
        );
        let done = stored(&inbox, &id);
        assert_eq!(
            (
                done.status.as_str(),
                done.outcome.as_deref(),
                done.session_id.as_deref()
            ),
            ("done", Some(outcome), Some("new-current-session"))
        );
        assert_eq!(inbox.claim(NOW + 1e6, LEASE_TTL).expect("claim"), None);
        assert_eq!(stored(&inbox, &id), done, "{outcome}");
    }
}

#[test]
fn uncertain_stays_uncertain() {
    let scratch = Scratch::new();
    let mut inbox = open_test_inbox(&scratch);
    let id = publish_at(&mut inbox, "one", NOW);
    settle(&mut inbox, NOW + 10.0, "uncertain");
    let row = stored(&inbox, &id);
    assert_eq!(
        (row.status.as_str(), row.outcome.as_deref()),
        ("uncertain", Some("uncertain"))
    );
    for at in [NOW + 200.0, NOW + DEFER_SECONDS * 10.0, NOW + 1e7] {
        inbox.recover(at).expect("recover");
        assert_eq!(inbox.claim(at, LEASE_TTL).expect("claim"), None);
        assert_eq!(stored(&inbox, &id), row);
    }
}

#[test]
fn deferred_is_reoffered_until_max_offers_then_exhausted() {
    let scratch = Scratch::new();
    let mut inbox = open_test_inbox(&scratch);
    let id = publish_at(&mut inbox, "ev1", NOW);
    let mut at = NOW + 10.0;
    for offer in 1..=MAX_OFFERS {
        let row = processing(&mut inbox, at, LEASE_TTL);
        assert_eq!(row.attempts, offer);
        assert!(inbox.ack(&id, lease(&row), "deferred", at).expect("ack"));
        let held = stored(&inbox, &id);
        assert_eq!(
            (
                held.status.as_str(),
                held.outcome.as_deref(),
                held.available
            ),
            ("deferred", Some("deferred"), at + DEFER_SECONDS)
        );
        assert_eq!(
            inbox
                .claim(at + DEFER_SECONDS - 1.0, LEASE_TTL)
                .expect("claim"),
            None
        );
        at += DEFER_SECONDS;
    }
    assert_eq!(inbox.claim(at, LEASE_TTL).expect("claim"), None);
    let exhausted = stored(&inbox, &id);
    assert_eq!(
        (
            exhausted.status.as_str(),
            exhausted.outcome.as_deref(),
            exhausted.attempts
        ),
        ("uncertain", Some("deferred-exhausted"), MAX_OFFERS)
    );
}

#[test]
fn renew_keeps_an_inflight_lease_alive() {
    let scratch = Scratch::new();
    let mut inbox = open_test_inbox(&scratch);
    let a = publish_at(&mut inbox, "a", NOW);
    let b = publish_at(&mut inbox, "b", NOW);
    let row = processing(&mut inbox, NOW + 10.0, 5.0);
    assert_eq!(row.id, a);
    let before = stored(&inbox, &a).lease_until.expect("until");
    assert!(
        inbox
            .renew(&a, lease(&row), NOW + 12.0, LEASE_TTL)
            .expect("renew")
    );
    let until = stored(&inbox, &a).lease_until.expect("until");
    assert_eq!(until, NOW + 12.0 + LEASE_TTL);
    assert!(until > before + 60.0);
    inbox.recover(NOW + 60.0).expect("recover");
    assert_eq!(stored(&inbox, &a).status, "processing");
    let other = stored(&inbox, &b);
    assert_eq!((other.status.as_str(), other.attempts), ("pending", 0));
}

/// Test key of `contracts/fixtures/README.md`, the key of the golden databases.
const GOLDEN_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
/// Base clock of the short window of the golden generator.
const U: f64 = 1_763_301_000.123_456;
const EVENTS_COLUMNS: [&str; 11] = [
    "id",
    "packet",
    "digest",
    "status",
    "attempts",
    "lease",
    "lease_until",
    "available",
    "session_id",
    "outcome",
    "created",
];
/// `sqlite_master.sql` of the Python `CREATE TABLE IF NOT EXISTS events` statement.
const PYTHON_EVENTS_SQL: &str = "CREATE TABLE events (
                id TEXT PRIMARY KEY, packet BLOB NOT NULL, digest TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
                lease TEXT, lease_until REAL, available REAL NOT NULL DEFAULT 0,
                session_id TEXT, outcome TEXT, created REAL NOT NULL)";

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/c1/golden")
        .join(name)
}

/// The `rows` of a golden `rows.*.json`.
fn golden_rows(name: &str) -> Vec<Value> {
    let rows: Value =
        serde_json::from_slice(&fs::read(golden(name)).expect("rows file")).expect("json");
    rows["rows"].as_array().expect("rows").clone()
}

/// A fresh owner-only root holding the golden key and a copy of `events.before.sqlite3`.
fn golden_root(scratch: &Scratch) -> PathBuf {
    let root = scratch.root_with_key(&unhex(GOLDEN_KEY_HEX));
    fs::copy(golden("events.before.sqlite3"), root.join("events.sqlite3")).expect("copy");
    root
}

fn read_only(db: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open read-only")
}

fn events_sql(db: &Path) -> String {
    read_only(db)
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='events'",
            [],
            |r| r.get(0),
        )
        .expect("events sql")
}

type ColumnInfo = (i64, String, String, i64, Option<String>, i64);

fn table_info(db: &Path) -> Vec<ColumnInfo> {
    let conn = read_only(db);
    let mut stmt = conn.prepare("PRAGMA table_info(events)").expect("prepare");
    stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("table_info")
}

/// Every `events` row ordered by id, in the shape of the golden rows files: each column as
/// `{"type": typeof(column), "value": value}`, a blob as lowercase hex.
fn stored_rows(db: &Path) -> Vec<Value> {
    use rusqlite::types::ValueRef;
    let select = EVENTS_COLUMNS
        .iter()
        .map(|c| format!("{c}, typeof({c})"))
        .collect::<Vec<_>>()
        .join(", ");
    let conn = read_only(db);
    let mut stmt = conn
        .prepare(&format!("SELECT {select} FROM events ORDER BY id"))
        .expect("prepare");
    stmt.query_map([], |r| {
        let mut row = Map::new();
        for (i, column) in EVENTS_COLUMNS.iter().enumerate() {
            let value = match r.get_ref(2 * i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(x) => json!(x),
                ValueRef::Text(t) => json!(String::from_utf8(t.to_vec()).expect("utf-8")),
                ValueRef::Blob(b) => json!(hex(b)),
            };
            let kind: String = r.get(2 * i + 1)?;
            row.insert((*column).to_owned(), json!({"type": kind, "value": value}));
        }
        Ok(Value::Object(row))
    })
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("rows")
}

/// The column values of `row` as the public API returns them, a blob as lowercase hex.
fn api_values(row: &EventRow) -> Value {
    json!({
        "id": row.id,
        "packet": hex(&row.packet),
        "digest": row.digest,
        "status": row.status,
        "attempts": row.attempts,
        "lease": row.lease,
        "lease_until": row.lease_until,
        "available": row.available,
        "session_id": row.session_id,
        "outcome": row.outcome,
        "created": row.created,
    })
}

fn golden_values(row: &Value) -> Value {
    let values = row
        .as_object()
        .expect("row")
        .iter()
        .map(|(column, cell)| (column.clone(), cell["value"].clone()))
        .collect();
    Value::Object(values)
}

/// `rows` with every present lease checked as 32 lowercase hex characters and replaced by
/// a marker, since lease tokens are random on both sides.
fn lease_presence(mut rows: Vec<Value>) -> Vec<Value> {
    for row in &mut rows {
        let lease = &mut row["lease"]["value"];
        if let Some(token) = lease.as_str() {
            assert!(
                token.len() == 32
                    && token
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "lease {token:?}"
            );
            *lease = json!("present");
        }
    }
    rows
}

fn golden_claim(inbox: &mut Inbox, id: &str, at: f64) -> EventRow {
    let row = claim(inbox, at, LEASE_TTL);
    assert_eq!(row.id, id);
    row
}

fn golden_settle(inbox: &mut Inbox, row: &EventRow, outcome: &str, at: f64) {
    let session = format!("session:{}", row.id);
    assert!(inbox.start(&row.id, lease(row), &session).expect("start"));
    assert!(inbox.ack(&row.id, lease(row), outcome, at).expect("ack"));
}

#[test]
fn python_written_database_reads_and_replays_like_python() {
    let scratch = Scratch::new();
    let root = golden_root(&scratch);
    let db = root.join("events.sqlite3");
    let mut inbox = Inbox::open(&root).expect("open");
    assert_eq!(events_sql(&db), PYTHON_EVENTS_SQL);

    let before = golden_rows("rows.before.json");
    assert_eq!(before.len(), 11);
    for expected in &before {
        let id = expected["id"]["value"].as_str().expect("id");
        let row = stored(&inbox, id);
        assert_eq!(api_values(&row), golden_values(expected), "{id}");
        let mut packet: Value = serde_json::from_slice(&row.packet).expect("packet");
        assert_eq!(
            inbox.payload(id).expect("payload"),
            packet["payload"],
            "{id}"
        );
        assert_eq!(canonical(&packet).expect("canonical"), row.packet, "{id}");
        packet.as_object_mut().expect("object").remove("mac");
        let digest = hex(&Sha256::digest(canonical(&packet).expect("canonical")));
        assert_eq!(digest, row.digest, "{id}");
    }
    assert_eq!(
        inbox.payload("golden:pending-a").expect("payload"),
        "Привет, неделя прошла 🦀 спокойно"
    );
    assert_eq!(stored_rows(&db), before);

    let republished = stored(&inbox, "golden:pending-a").packet;
    assert_eq!(
        inbox
            .publish_json(&republished, U + 40.0)
            .expect("republish"),
        "golden:pending-a"
    );
    let leased = stored(&inbox, "golden:leased");
    let processing = stored(&inbox, "golden:processing");
    assert!(
        inbox
            .renew(&leased.id, lease(&leased), U + 40.0, 60.0)
            .expect("renew")
    );
    assert!(
        inbox
            .renew(&processing.id, lease(&processing), U + 40.0, LEASE_TTL)
            .expect("renew")
    );
    golden_settle(&mut inbox, &leased, "success", U + 41.0);
    assert!(
        inbox
            .ack(&processing.id, lease(&processing), "uncertain", U + 42.0)
            .expect("ack")
    );
    let row = golden_claim(&mut inbox, "golden:retried", U + 43.0);
    golden_settle(&mut inbox, &row, "silent", U + 44.0);
    let row = golden_claim(&mut inbox, "golden:pending-a", U + 45.0);
    assert!(
        inbox
            .retry(&row.id, lease(&row), U + 45.0, RETRY_DELAY)
            .expect("retry")
    );
    let row = golden_claim(&mut inbox, "golden:pending-b", U + 46.0);
    golden_settle(&mut inbox, &row, "deferred", U + 47.0);
    inbox.recover(U + 3612.0).expect("recover");

    assert_eq!(
        lease_presence(stored_rows(&db)),
        lease_presence(golden_rows("rows.after.json"))
    );
    assert_eq!(events_sql(&db), PYTHON_EVENTS_SQL);
}

#[test]
fn rust_created_database_has_the_python_schema_and_rows() {
    let scratch = Scratch::new();
    let root = scratch.root_with_key(&unhex(GOLDEN_KEY_HEX));
    let db = root.join("events.sqlite3");
    let mut inbox = Inbox::open(&root).expect("open");
    let golden_rows = golden_rows("rows.before.json");
    for row in &golden_rows {
        let packet = unhex(row["packet"]["value"].as_str().expect("packet"));
        let created = row["created"]["value"].as_f64().expect("created");
        let id = inbox.publish_json(&packet, created).expect("publish");
        assert_eq!(id, row["id"]["value"], "{id}");
    }

    let golden_db = golden("events.before.sqlite3");
    assert_eq!(events_sql(&db), events_sql(&golden_db));
    assert_eq!(table_info(&db), table_info(&golden_db));
    let rows = stored_rows(&db);
    assert_eq!(rows.len(), golden_rows.len());
    for (ours, theirs) in rows.iter().zip(&golden_rows) {
        let id = &theirs["id"]["value"];
        for column in ["id", "packet", "digest", "created"] {
            assert_eq!(ours[column], theirs[column], "{id} {column}");
        }
        if theirs["status"]["value"] == "pending" && theirs["attempts"]["value"] == 0 {
            assert_eq!(ours, theirs, "{id}");
        }
    }
    assert_eq!(
        golden_rows
            .iter()
            .filter(|r| r["status"]["value"] == "pending" && r["attempts"]["value"] == 0)
            .count(),
        2
    );
}
