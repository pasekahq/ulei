//! `ulei_core::inbox` through its public API on temporary roots.

use std::fmt::Write;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use ulei_core::inbox::{Error, EventRow, Guarded, Inbox, Invalid, canonical};

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
    let path = format!(
        "{}/../../contracts/fixtures/c1/golden/vectors.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let golden: Value = serde_json::from_slice(&fs::read(path).expect("vectors")).expect("json");
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
