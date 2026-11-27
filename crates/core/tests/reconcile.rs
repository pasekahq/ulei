//! `ulei inbox reconcile` and `ulei-inbox reconcile` through the built binaries against the real
//! `ulei-inbox serve` daemon on a real store holding one event in every status and every
//! `uncertain` reason; rows are read over C1 `get`, audit lines from `events.sqlite3`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::daemon::{Daemon, Started, call, launch};
use common::{Scratch, sign, unix_now};
use serde_json::{Value, json};
use ulei_contracts::generated::c1::{
    Counts, ErrorCode, GetResponse, Row, RowOutcome, Status, UncertainList,
};
use ulei_core::inbox::{Inbox, MAX_OFFERS, own_uid};

const INBOX: &str = env!("CARGO_BIN_EXE_ulei-inbox");
const ULEI: &str = env!("CARGO_BIN_EXE_ulei");
const KEY: [u8; 32] = [9; 32];
const HOUR: f64 = 3600.0;
const OUTCOME: &str = "reconciled-delivered";
/// The events of the stand: `(id, status, outcome)` as it starts.
const EVENTS: [(&str, Status, Option<RowOutcome>); 8] = [
    ("rec:done", Status::Done, Some(RowOutcome::Success)),
    (
        "rec:uncertain",
        Status::Uncertain,
        Some(RowOutcome::Uncertain),
    ),
    (
        "rec:expired",
        Status::Uncertain,
        Some(RowOutcome::ExpiredProcessing),
    ),
    (
        "rec:exhausted",
        Status::Uncertain,
        Some(RowOutcome::DeferredExhausted),
    ),
    ("rec:deferred", Status::Deferred, Some(RowOutcome::Deferred)),
    ("rec:processing", Status::Processing, None),
    ("rec:leased", Status::Leased, None),
    ("rec:pending", Status::Pending, None),
];

/// One audit line as stored: id, outcome, reason, note, uid, pid, time.
type AuditLine = (String, String, String, String, i64, i64, f64);

/// A store with the [`EVENTS`] served by the real daemon.
struct Stand {
    scratch: Scratch,
    root: PathBuf,
    daemon: Daemon,
}

impl Stand {
    fn new() -> Self {
        let scratch = Scratch::new();
        let root = scratch.root_with_key(&KEY);
        fill(&mut Inbox::open(&root).expect("open"), unix_now());
        let daemon = match launch(INBOX, &[], &root, &scratch.socket(), None, &[]) {
            Started::Ready(daemon) => daemon,
            Started::Exited(status, stderr) => panic!("daemon exited {status}: {stderr}"),
        };
        let stand = Self {
            scratch,
            root,
            daemon,
        };
        for (id, status, outcome) in EVENTS {
            let row = stand.row(id);
            assert_eq!((row.status, row.outcome), (status, outcome), "{id}");
        }
        stand
    }

    fn socket(&self) -> &Path {
        &self.daemon.socket
    }

    /// The config `launch` wrote: `inbox.root` and `inbox.socket`.
    fn config(&self) -> PathBuf {
        self.socket().with_extension("toml")
    }

    fn row(&self, id: &str) -> Row {
        call::<GetResponse>(self.socket(), "get", &json!({ "id": id }), 200)
            .row
            .expect("stored row")
    }

    fn rows(&self) -> Vec<Row> {
        EVENTS.iter().map(|(id, _, _)| self.row(id)).collect()
    }

    fn audit(&self) -> Vec<AuditLine> {
        let db = rusqlite::Connection::open_with_flags(
            self.root.join("events.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open read-only");
        let mut query = db
            .prepare("SELECT id, outcome, reason, note, uid, pid, time FROM reconciled ORDER BY id")
            .expect("prepare");
        query
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("audit lines")
    }

    /// Runs `exe` with `args`, then `--config` of the stand; its output and pid.
    fn run(&self, exe: &str, args: &[&str]) -> (Output, u32) {
        let child = Command::new(exe)
            .args(args)
            .arg("--config")
            .arg(self.config())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn");
        let pid = child.id();
        (child.wait_with_output().expect("wait"), pid)
    }

    /// `ulei-inbox reconcile <id> --outcome <outcome> --note <note>`.
    fn reconcile(&self, id: &str, outcome: &str, note: &str) -> (Output, u32) {
        self.run(
            INBOX,
            &["reconcile", id, "--outcome", outcome, "--note", note],
        )
    }
}

/// Publishes the [`EVENTS`] one at a time at about `t` and drives each into its state.
fn fill(inbox: &mut Inbox, t: f64) {
    let publish = |inbox: &mut Inbox, id: &str, at: f64| {
        let body = json!({
            "version": 1,
            "kind": "task_result",
            "id": id,
            "payload": "Synthetic event.",
            "source_ts": 1_700_000_000,
            "trust": "untrusted-source-data",
        });
        inbox.publish(&sign(&KEY, &body), at).expect("publish");
    };
    let started = |inbox: &mut Inbox, at: f64, ttl: f64| {
        let row = inbox.claim(at, ttl).expect("claim").expect("claimable");
        let lease = row.lease.clone().expect("lease");
        assert!(inbox.start(&row.id, &lease, "session").expect("start"));
        (row.id, lease)
    };
    publish(inbox, "rec:done", t);
    let (id, lease) = started(inbox, t, HOUR);
    assert!(inbox.ack(&id, &lease, "success", t).expect("ack"));
    publish(inbox, "rec:uncertain", t);
    let (id, lease) = started(inbox, t, HOUR);
    assert!(inbox.ack(&id, &lease, "uncertain", t).expect("ack"));
    publish(inbox, "rec:expired", t);
    started(inbox, t, 1.0);
    inbox.recover(t + 2.0).expect("recover");
    let mut at = t + 2.0;
    publish(inbox, "rec:exhausted", at);
    for _ in 0..MAX_OFFERS {
        let (id, lease) = started(inbox, at, HOUR);
        assert!(inbox.defer(&id, &lease, at + 1.0).expect("defer"));
        at += 1.0;
    }
    inbox.recover(at).expect("recover");
    publish(inbox, "rec:deferred", at);
    let (id, lease) = started(inbox, at, HOUR);
    assert!(inbox.defer(&id, &lease, t + 10.0 * HOUR).expect("defer"));
    publish(inbox, "rec:processing", at);
    started(inbox, at, 10.0 * HOUR);
    publish(inbox, "rec:leased", at);
    inbox
        .claim(at, 10.0 * HOUR)
        .expect("claim")
        .expect("claimable");
    publish(inbox, "rec:pending", at);
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("UTF-8")
}

/// The one stderr line of a refused run with exit `code`, and that stdout is empty.
fn refusal(out: &Output, code: i32) -> String {
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(code), "{stderr}");
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
    assert_eq!(stderr.matches('\n').count(), 1, "{stderr}");
    assert!(stderr.ends_with('\n'), "{stderr}");
    stderr
}

#[test]
fn reconcile_closes_each_uncertain_reason_with_one_audit_line_and_changes_no_other_row() {
    let stand = Stand::new();
    let uid = own_uid().expect("uid");
    let long = "é".repeat(500);
    let cases = [
        (
            "rec:uncertain",
            "uncertain",
            "answered in a later turn",
            ULEI,
        ),
        ("rec:expired", "expired-processing", long.as_str(), INBOX),
        (
            "rec:exhausted",
            "deferred-exhausted",
            "seen in the chat",
            ULEI,
        ),
    ];
    let mut audit = Vec::new();
    for (id, reason, note, exe) in cases {
        let before = stand.rows();
        let args = ["reconcile", id, "--outcome", OUTCOME, "--note", note];
        let start = unix_now();
        let (out, pid) = if exe == ULEI {
            let mut forwarded = vec!["inbox"];
            forwarded.extend(args);
            stand.run(ULEI, &forwarded)
        } else {
            stand.run(INBOX, &args)
        };
        let end = unix_now();
        let stdout = text(&out.stdout);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
        let prefix =
            format!("reconciled {id}: done, {OUTCOME} (was {reason}), by uid {uid} pid {pid} at ");
        assert!(stdout.starts_with(&prefix), "{stdout}");
        assert_eq!(stdout.matches('\n').count(), 1, "{stdout}");
        assert!(stdout.ends_with("Z\n"), "{stdout}");

        let expected: Vec<Row> = before
            .into_iter()
            .map(|mut row| {
                if row.id == id {
                    row.status = Status::Done;
                    row.outcome = Some(RowOutcome::ReconciledDelivered);
                }
                row
            })
            .collect();
        assert_eq!(stand.rows(), expected, "{id}");

        let lines = stand.audit();
        let line = lines
            .iter()
            .find(|line| line.0 == id)
            .expect("audit line")
            .clone();
        assert!((start..=end).contains(&line.6), "{line:?}");
        audit.push((
            id.to_owned(),
            OUTCOME.to_owned(),
            reason.to_owned(),
            note.to_owned(),
            i64::from(uid),
            i64::from(pid),
            line.6,
        ));
        audit.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(lines, audit, "{id}");
    }
}

#[test]
fn reconcile_refuses_every_other_state_and_unknown_id_and_changes_nothing() {
    let stand = Stand::new();
    let (out, _) = stand.reconcile("rec:uncertain", OUTCOME, "closed once");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let cases = [
        (
            "rec:pending",
            "409 conflict: event is pending, not uncertain",
        ),
        ("rec:leased", "409 conflict: event is leased, not uncertain"),
        (
            "rec:processing",
            "409 conflict: event is processing, not uncertain",
        ),
        (
            "rec:deferred",
            "409 conflict: event is deferred (deferred), not uncertain",
        ),
        (
            "rec:done",
            "409 conflict: event is done (success), not uncertain",
        ),
        (
            "rec:uncertain",
            "409 conflict: event is done (reconciled-delivered), not uncertain",
        ),
        ("rec:unknown", "404 not_found: event not found"),
    ];
    for (id, reason) in cases {
        let (rows, audit) = (stand.rows(), stand.audit());
        let (out, _) = stand.reconcile(id, OUTCOME, "should not close");
        assert_eq!(
            refusal(&out, 1),
            format!("ulei-inbox reconcile: {id}: {reason}\n")
        );
        assert_eq!((stand.rows(), stand.audit()), (rows, audit), "{id}");
    }
}

#[test]
fn reconcile_refuses_a_bad_outcome_or_note_before_any_call() {
    let stand = Stand::new();
    let note_rule = "ulei-inbox reconcile: --note must be 1 to 500 characters, not blank, \
                     without control characters\n";
    let outcome_rule = "ulei-inbox reconcile: --outcome must be reconciled-delivered\n";
    let required =
        "ulei-inbox reconcile: --outcome reconciled-delivered and --note TEXT are required\n";
    let too_long = "n".repeat(501);
    let cases: [(&[&str], &str); 8] = [
        (
            &["--outcome", "reconciled-lost", "--note", "x"],
            outcome_rule,
        ),
        (&["--outcome", "success", "--note", "x"], outcome_rule),
        (&["--outcome", OUTCOME], required),
        (&["--note", "x"], required),
        (&["--outcome", OUTCOME, "--note", ""], note_rule),
        (&["--outcome", OUTCOME, "--note", " \t "], note_rule),
        (&["--outcome", OUTCOME, "--note", "two\nlines"], note_rule),
        (&["--outcome", OUTCOME, "--note", &too_long], note_rule),
    ];
    for (tail, line) in cases {
        let (rows, audit) = (stand.rows(), stand.audit());
        let mut args = vec!["reconcile", "rec:uncertain"];
        args.extend(tail);
        let (out, _) = stand.run(INBOX, &args);
        assert_eq!(refusal(&out, 2), line, "{tail:?}");
        assert_eq!((stand.rows(), stand.audit()), (rows, audit), "{tail:?}");
    }
}

#[test]
fn reconciled_events_leave_uncertain_and_show_in_counts_and_status() {
    let stand = Stand::new();
    let listed = || -> Vec<String> {
        call::<UncertainList>(stand.socket(), "uncertain", &json!({}), 200)
            .events
            .into_iter()
            .map(|e| e.id)
            .collect()
    };
    let counts = |uncertain, reconciled| Counts {
        pending: 1,
        leased: 1,
        processing: 1,
        deferred: 1,
        uncertain,
        reconciled: Some(reconciled),
    };
    assert_eq!(listed(), ["rec:expired", "rec:uncertain", "rec:exhausted"]);
    assert_eq!(
        call::<Counts>(stand.socket(), "counts", &json!({}), 200),
        counts(3, 0)
    );
    for id in ["rec:uncertain", "rec:exhausted"] {
        let (out, _) = stand.reconcile(id, OUTCOME, "delivered later");
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    }
    assert_eq!(listed(), ["rec:expired"]);
    assert_eq!(
        call::<Counts>(stand.socket(), "counts", &json!({}), 200),
        counts(1, 2)
    );

    let config = stand.scratch.0.join("status.toml");
    fs::write(
        &config,
        format!(
            "[inbox]\nsocket = \"{}\"\n[engine]\nc6_socket = \"{}\"\nowner_session = \"s\"\n\
             [qb]\nsocket = \"{}\"\n",
            stand.socket().display(),
            stand.scratch.0.join("engine.sock").display(),
            stand.scratch.0.join("qb.sock").display(),
        ),
    )
    .expect("config");
    let out = Command::new(INBOX)
        .args(["status", "--config"])
        .arg(&config)
        .output()
        .expect("status");
    let stdout = text(&out.stdout);
    assert!(
        stdout.lines().any(|line| line
            == "queue: pending 1, leased 1, processing 1, deferred 1, uncertain 1, reconciled 2"),
        "{stdout}"
    );
}

#[test]
fn the_daemon_refuses_a_reconcile_body_outside_c1_and_changes_nothing() {
    let stand = Stand::new();
    let s = stand.socket();
    let (rows, audit) = (stand.rows(), stand.audit());
    let id = "rec:uncertain";
    let bodies: [(Value, &str); 8] = [
        (
            json!({ "id": id, "outcome": "reconciled-lost", "note": "x" }),
            "body",
        ),
        (
            json!({ "id": id, "outcome": "success", "note": "x" }),
            "body",
        ),
        (json!({ "id": id, "outcome": OUTCOME }), "body"),
        (
            json!({ "id": id, "outcome": OUTCOME, "note": "x", "by": "me" }),
            "body",
        ),
        (
            json!({ "id": "bad id", "outcome": OUTCOME, "note": "x" }),
            "body",
        ),
        (json!({ "id": id, "outcome": OUTCOME, "note": "" }), "note"),
        (
            json!({ "id": id, "outcome": OUTCOME, "note": "a\u{7}b" }),
            "note",
        ),
        (
            json!({ "id": id, "outcome": OUTCOME, "note": "n".repeat(501) }),
            "note",
        ),
    ];
    for (body, kind) in bodies {
        let error: ulei_contracts::generated::c1::Error = call(s, "reconcile", &body, 400);
        assert_eq!(error.code, ErrorCode::BadRequest, "{body}");
        let message = if kind == "note" {
            "note must be 1 to 500 characters, not blank, without control characters"
        } else {
            "body is not valid for the method input"
        };
        assert_eq!(error.message, message, "{body}");
    }
    assert_eq!((stand.rows(), stand.audit()), (rows, audit));
}
