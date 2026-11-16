//! `ulei-inbox shadow` over synthetic live roots built from the golden Python databases, with
//! the README test key as `producer.key` and the clock as a parameter.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use common::{
    GOLDEN_KEY_HEX, Scratch, api_values, golden, golden_rows, golden_values, hex, sign, unhex,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ulei_core::inbox::canonical;
use ulei_core::inbox::shadow::{self, Failure, Pass};

const INBOX: &str = env!("CARGO_BIN_EXE_ulei-inbox");
const NOW: f64 = 1_763_400_000.5;
const BEFORE: &str = "events.before.sqlite3";
const AFTER: &str = "events.after.sqlite3";

struct Live {
    /// Holds the temporary root until the test ends.
    _scratch: Scratch,
    root: PathBuf,
    state: PathBuf,
    out: PathBuf,
}

impl Live {
    /// An owner-only live root holding the golden key and a copy of the golden `db`.
    fn new(db: &str) -> Self {
        let scratch = Scratch::new();
        let root = scratch.root_with_key(&unhex(GOLDEN_KEY_HEX));
        let live = Self {
            state: scratch.0.join("state"),
            out: scratch.0.join("divergences.jsonl"),
            root,
            _scratch: scratch,
        };
        live.replace(db);
        live
    }

    fn db(&self) -> PathBuf {
        self.root.join("events.sqlite3")
    }

    /// The golden `db` as the next live state.
    fn replace(&self, db: &str) {
        fs::copy(golden(db), self.db()).expect("copy golden database");
    }

    fn pass(&self) -> Pass {
        shadow::run(&self.root, &self.state, &self.out, NOW).expect("shadow pass")
    }

    fn mutate(&self, sql: &str, args: impl rusqlite::Params) {
        let conn = Connection::open(self.db()).expect("open live copy");
        assert_eq!(conn.execute(sql, args).expect("mutate"), 1, "{sql}");
    }

    fn packet(&self, id: &str) -> Value {
        let conn = Connection::open(self.db()).expect("open live copy");
        let bytes: Vec<u8> = conn
            .query_row("SELECT packet FROM events WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .expect("packet");
        serde_json::from_slice(&bytes).expect("packet json")
    }

    fn set_packet(&self, id: &str, bytes: &[u8]) {
        self.mutate(
            "UPDATE events SET packet = ?1 WHERE id = ?2",
            params![bytes, id],
        );
    }

    /// Every appended record, parsed.
    fn records(&self) -> Vec<Value> {
        match fs::read_to_string(&self.out) {
            Ok(text) => text
                .lines()
                .map(|line| serde_json::from_str(line).expect("record json"))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => panic!("read out: {e}"),
        }
    }

    fn shadow_rows(&self) -> Vec<Value> {
        shadow::snapshot(&self.state)
            .expect("snapshot")
            .iter()
            .map(api_values)
            .collect()
    }

    fn shadow_row(&self, id: &str) -> Value {
        self.shadow_rows()
            .into_iter()
            .find(|row| row["id"] == id)
            .expect("snapshot row")
    }
}

fn values_of(rows_file: &str) -> Vec<Value> {
    golden_rows(rows_file).iter().map(golden_values).collect()
}

/// The `check` and `id` of every record.
fn checks(records: &[Value]) -> Vec<(String, String)> {
    records
        .iter()
        .map(|r| {
            (
                r["check"].as_str().expect("check").to_owned(),
                r["id"].as_str().expect("id").to_owned(),
            )
        })
        .collect()
}

fn one(check: &str, id: &str) -> Vec<(String, String)> {
    vec![(check.to_owned(), id.to_owned())]
}

#[test]
fn golden_before_then_after_replays_without_divergence() {
    let live = Live::new(BEFORE);
    assert_eq!(
        live.pass(),
        Pass {
            rows: 11,
            divergences: 0
        }
    );
    assert_eq!(live.shadow_rows(), values_of("rows.before.json"));
    live.replace(AFTER);
    assert_eq!(
        live.pass(),
        Pass {
            rows: 11,
            divergences: 0
        }
    );
    assert_eq!(live.shadow_rows(), values_of("rows.after.json"));
    assert!(!live.out.exists(), "no divergence appends nothing");
}

/// Name, bytes and mtime of every entry of `dir`.
fn listing(dir: &Path) -> Vec<(PathBuf, Vec<u8>, SystemTime)> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| {
            let path = entry.expect("entry").path();
            let meta = fs::metadata(&path).expect("stat");
            (
                path.clone(),
                fs::read(&path).expect("read"),
                meta.modified().expect("mtime"),
            )
        })
        .collect();
    entries.sort();
    entries
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

#[test]
fn read_only_live_root_is_left_byte_for_byte() {
    let live = Live::new(BEFORE);
    chmod(&live.db(), 0o400);
    chmod(&live.root.join("producer.key"), 0o400);
    chmod(&live.root, 0o500);
    let root_mtime = fs::metadata(&live.root).expect("stat").modified();
    let before = listing(&live.root);
    let pass = shadow::run(&live.root, &live.state, &live.out, NOW);
    let after = listing(&live.root);
    let root_mtime_after = fs::metadata(&live.root).expect("stat").modified();
    chmod(&live.root, 0o700);
    assert_eq!(pass.expect("pass").divergences, 0);
    assert_eq!(after, before);
    assert_eq!(root_mtime_after.expect("mtime"), root_mtime.expect("mtime"));
}

#[test]
fn state_inside_the_live_root_is_refused_before_any_write() {
    let live = Live::new(BEFORE);
    let before = listing(&live.root);
    let state = live.root.join("shadow");
    let pass = shadow::run(&live.root, &state, &live.out, NOW);
    assert!(matches!(pass, Err(Failure::InsideLive(path)) if path == state));
    assert_eq!(listing(&live.root), before);
}

#[test]
fn locked_live_database_exits_retry_later_and_keeps_the_state() {
    let live = Live::new(BEFORE);
    live.pass();
    let state_db = live.state.join("events.sqlite3");
    let state_bytes = fs::read(&state_db).expect("state db");
    live.replace(AFTER);
    let lock = Connection::open(live.db()).expect("open live copy");
    lock.execute_batch("BEGIN EXCLUSIVE").expect("write lock");
    let cli = || {
        Command::new(INBOX)
            .arg("shadow")
            .arg("--live-root")
            .arg(&live.root)
            .arg("--state")
            .arg(&live.state)
            .arg("--out")
            .arg(&live.out)
            .output()
            .expect("run ulei-inbox")
    };

    let locked = cli();
    assert_eq!(locked.status.code(), Some(75));
    let stderr = String::from_utf8(locked.stderr).expect("utf-8");
    assert!(stderr.contains("retry later"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&locked.stdout), "");
    assert_eq!(fs::read(&state_db).expect("state db"), state_bytes);
    assert!(!live.out.exists());

    drop(lock);
    let free = cli();
    assert_eq!(free.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(free.stdout).expect("utf-8"),
        "ulei-inbox shadow: 11 rows, 0 divergences\n"
    );
    assert_eq!(live.shadow_rows(), values_of("rows.after.json"));
}

#[test]
fn non_canonical_stored_bytes_are_a_packet_bytes_divergence() {
    let live = Live::new(BEFORE);
    let id = "golden:pending-a";
    let stored = canonical(&live.packet(id)).expect("canonical");
    live.set_packet(id, &[b"{ ".as_slice(), &stored[1..]].concat());
    assert_eq!(live.pass().divergences, 1);
    assert_eq!(checks(&live.records()), one("packet_bytes", id));
}

#[test]
fn wrong_digest_column_is_a_digest_divergence() {
    let live = Live::new(BEFORE);
    let id = "golden:pending-b";
    live.mutate(
        "UPDATE events SET digest = ?1 WHERE id = ?2",
        params!["0".repeat(64), id],
    );
    live.pass();
    assert_eq!(checks(&live.records()), one("digest", id));
}

#[test]
fn packet_signed_with_another_key_is_a_mac_divergence() {
    let live = Live::new(BEFORE);
    let id = "golden:leased";
    let mut body = live.packet(id);
    body.as_object_mut().expect("object").remove("mac");
    let resigned = canonical(&sign(&[9; 32], &body)).expect("canonical");
    live.set_packet(id, &resigned);
    live.pass();
    assert_eq!(checks(&live.records()), one("mac", id));
}

#[test]
fn row_invalid_for_c1_row_is_a_row_divergence() {
    let live = Live::new(BEFORE);
    let id = "golden:retried";
    live.mutate("UPDATE events SET outcome = 'bogus' WHERE id = ?1", [id]);
    live.pass();
    assert_eq!(checks(&live.records()), one("row", id));
    assert!(live.shadow_rows().iter().all(|row| row["id"] != id));
}

#[test]
fn done_back_to_pending_is_a_transition_divergence_and_is_not_applied() {
    let live = Live::new(BEFORE);
    live.pass();
    live.replace(AFTER);
    let id = "golden:done-success";
    live.mutate("UPDATE events SET status = 'pending' WHERE id = ?1", [id]);
    live.pass();
    let records = live.records();
    assert_eq!(
        records,
        [json!({
            "check": "transition",
            "detail": "state change follows no edge of the lease state machine",
            "id": id,
            "live": "pending",
            "rust": "done",
            "ts": NOW,
        })]
    );
    assert_eq!(live.shadow_row(id)["status"], "done");
}

#[test]
fn changed_created_is_an_immutable_divergence() {
    let live = Live::new(BEFORE);
    live.pass();
    live.replace(AFTER);
    let id = "golden:done-silent";
    live.mutate(
        "UPDATE events SET created = created + 1 WHERE id = ?1",
        [id],
    );
    live.pass();
    assert_eq!(checks(&live.records()), one("immutable", id));
}

#[test]
fn decreased_attempts_is_an_immutable_divergence() {
    let live = Live::new(BEFORE);
    live.pass();
    live.replace(AFTER);
    let id = "golden:leased";
    live.mutate("UPDATE events SET attempts = 0 WHERE id = ?1", [id]);
    live.pass();
    assert_eq!(checks(&live.records()), one("immutable", id));
}

#[test]
fn deleted_row_is_a_row_divergence_on_every_pass() {
    let live = Live::new(BEFORE);
    live.pass();
    live.replace(AFTER);
    let id = "golden:pending-b";
    live.mutate("DELETE FROM events WHERE id = ?1", [id]);
    live.pass();
    assert_eq!(checks(&live.records()), one("row", id));
    live.pass();
    assert_eq!(
        checks(&live.records()),
        [one("row", id), one("row", id)].concat()
    );
}

/// Every payload of the golden rows, raw and as escaped inside a JSON string.
fn golden_payloads() -> Vec<String> {
    golden_rows("rows.before.json")
        .iter()
        .flat_map(|row| {
            let packet: Value = serde_json::from_slice(&unhex(
                row["packet"]["value"].as_str().expect("packet hex"),
            ))
            .expect("packet json");
            let payload = packet["payload"].as_str().expect("payload").to_owned();
            let escaped = Value::from(payload.as_str()).to_string();
            [payload, escaped.trim_matches('"').to_owned()]
        })
        .collect()
}

#[test]
fn records_are_canonical_lines_without_payload_text() {
    let live = Live::new(BEFORE);
    let stored = canonical(&live.packet("golden:pending-a")).expect("canonical");
    live.set_packet(
        "golden:pending-a",
        &[b"{ ".as_slice(), &stored[1..]].concat(),
    );
    live.mutate(
        "UPDATE events SET digest = ?1 WHERE id = 'golden:pending-b'",
        ["f".repeat(64)],
    );
    let mut body = live.packet("golden:leased");
    body.as_object_mut().expect("object").remove("mac");
    let resigned = canonical(&sign(&[9; 32], &body)).expect("canonical");
    live.set_packet("golden:leased", &resigned);
    // Python accepts version 1.0, the Rust store refuses it.
    let mut body = live.packet("golden:processing");
    body.as_object_mut().expect("object").remove("mac");
    body["version"] = json!(1.0);
    let float_version = canonical(&sign(&unhex(GOLDEN_KEY_HEX), &body)).expect("canonical");
    live.set_packet("golden:processing", &float_version);
    let digest = hex(&Sha256::digest(canonical(&body).expect("canonical")));
    live.mutate(
        "UPDATE events SET digest = ?1 WHERE id = 'golden:processing'",
        [digest],
    );
    live.pass();
    live.replace(AFTER);
    live.mutate(
        "UPDATE events SET status = 'pending' WHERE id = 'golden:done-success'",
        params![],
    );
    live.mutate(
        "UPDATE events SET created = created + 1 WHERE id = 'golden:done-silent'",
        params![],
    );
    live.mutate(
        "DELETE FROM events WHERE id = 'golden:uncertain'",
        params![],
    );
    live.pass();

    let text = fs::read_to_string(&live.out).expect("out");
    let mut seen = BTreeSet::new();
    for line in text.lines() {
        let record: Value = serde_json::from_str(line).expect("record json");
        let keys: Vec<_> = record.as_object().expect("object").keys().collect();
        assert_eq!(keys.len(), 6, "{line}");
        assert_eq!(canonical(&record).expect("canonical"), line.as_bytes());
        assert_eq!(record["ts"], json!(NOW));
        seen.insert(record["check"].as_str().expect("check").to_owned());
    }
    let expected: BTreeSet<_> = [
        "packet_bytes",
        "digest",
        "mac",
        "publish",
        "row",
        "transition",
        "immutable",
    ]
    .map(str::to_owned)
    .into();
    assert_eq!(seen, expected);
    for payload in golden_payloads() {
        assert!(
            !text.contains(&payload),
            "payload text {payload:?} in records"
        );
    }
}
