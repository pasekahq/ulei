//! `ulei-connectors` against a fake C10 socket, with `ulei-connector-test` and scripted stdio
//! connectors as its children over pipes.

mod common;

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};
use ulei_contracts::generated::c11::{RunOutcome, RunRecord};

use common::http::{json_answer, read_request};
use common::{Scratch, write_mode};

const RUNNER: &str = env!("CARGO_BIN_EXE_ulei-connectors");
const ITEM_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The `ulei-connector-test` binary of this build: in the target directory that holds this test.
fn test_connector_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable");
    exe.ancestors()
        .map(|dir| dir.join("ulei-connector-test"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "no ulei-connector-test above {}; build the workspace (cargo test --workspace)",
                exe.display()
            )
        })
}

/// One `put` the fake archive received, with the cursor stored when it arrived.
struct Put {
    body: Value,
    cursor_then: Option<String>,
}

/// A C10 socket that answers `put`: `stored` for a new `(source, external_id)`, `duplicate`
/// for a repeat, and the status and code of `answers` for the external ids it names.
struct FakeArchive {
    puts: Arc<Mutex<Vec<Put>>>,
    answers: Arc<Mutex<HashMap<String, (u16, &'static str)>>>,
}

impl FakeArchive {
    fn start(socket: &Path, state_file: PathBuf) -> Self {
        let listener = UnixListener::bind(socket).expect("bind the fake archive");
        let puts = Arc::new(Mutex::new(Vec::<Put>::new()));
        let answers = Arc::new(Mutex::new(HashMap::new()));
        let (seen, rules) = (Arc::clone(&puts), Arc::clone(&answers));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some((target, body)) = read_request(&mut stream) else {
                    continue;
                };
                assert_eq!(target, "POST /v1/put");
                let body: Value = serde_json::from_slice(&body).expect("put body");
                let cursor_then = fs::read(&state_file).ok().and_then(|bytes| {
                    let state: Value = serde_json::from_slice(&bytes).expect("runner.json");
                    state["cursor"].as_str().map(str::to_owned)
                });
                let id = body["external_id"]
                    .as_str()
                    .expect("external_id")
                    .to_owned();
                let rule = rules.lock().expect("answers").get(&id).copied();
                let mut seen = seen.lock().expect("puts");
                let repeat = seen
                    .iter()
                    .any(|p| p.body["external_id"] == body["external_id"]);
                let answer = match rule {
                    Some((status, code)) => {
                        json_answer(status, &json!({"code": code, "message": "fake"}))
                    }
                    None => json_answer(
                        200,
                        &json!({"id": ITEM_ID, "status": if repeat { "duplicate" } else { "stored" }}),
                    ),
                };
                seen.push(Put { body, cursor_then });
                drop(seen);
                let _ = stream.write_all(&answer);
            }
        });
        Self { puts, answers }
    }

    fn answer(&self, external_id: &str, status: u16, code: &'static str) {
        self.answers
            .lock()
            .expect("answers")
            .insert(external_id.to_owned(), (status, code));
    }

    fn clear_answers(&self) {
        self.answers.lock().expect("answers").clear();
    }

    /// The external ids put so far, in order.
    fn ids(&self) -> Vec<String> {
        self.puts
            .lock()
            .expect("puts")
            .iter()
            .map(|p| p.body["external_id"].as_str().expect("id").to_owned())
            .collect()
    }
}

/// A connectors directory, state, log and C10 socket path under one scratch directory.
struct Bed {
    scratch: Scratch,
}

impl Bed {
    fn new() -> Self {
        let bed = Self {
            scratch: Scratch::new(),
        };
        fs::create_dir(bed.dir()).expect("connectors dir");
        bed
    }

    fn dir(&self) -> PathBuf {
        self.scratch.0.join("connectors")
    }

    fn state(&self) -> PathBuf {
        self.scratch.0.join("state")
    }

    fn log(&self) -> PathBuf {
        self.scratch.0.join("connectors.log")
    }

    fn socket(&self) -> PathBuf {
        self.scratch.0.join("c10")
    }

    fn state_file(&self, name: &str) -> PathBuf {
        self.state().join(name).join("runner.json")
    }

    fn archive(&self, name: &str) -> FakeArchive {
        FakeArchive::start(&self.socket(), self.state_file(name))
    }

    /// Writes `ulei.toml` with `enabled`, the paths above and the `extra` lines.
    fn config(&self, enabled: &str, extra: &str) {
        let text = format!(
            "[memory]\nsocket = \"{}\"\n\n[connectors]\ndir = \"{}\"\nstate = \"{}\"\nlog = \"{}\"\nenabled = \"{enabled}\"\n\n{extra}",
            self.socket().display(),
            self.dir().display(),
            self.state().display(),
            self.log().display(),
        );
        fs::write(self.scratch.0.join("ulei.toml"), text).expect("ulei.toml");
    }

    fn manifest(&self, name: &str, manifest: &Value) {
        let dir = self.dir().join(name);
        fs::create_dir_all(&dir).expect("connector dir");
        fs::write(dir.join("connector.json"), manifest.to_string()).expect("manifest");
    }

    /// `ulei-connector-test` as connector `name`, needing `key` when given.
    fn test_connector(&self, name: &str, key: Option<&str>) {
        let bin = self.dir().join(name).join("bin");
        fs::create_dir_all(&bin).expect("bin dir");
        symlink(test_connector_binary(), bin.join("ulei-connector-test")).expect("symlink");
        let mut manifest = manifest(name, "bin/ulei-connector-test");
        if let Some(key) = key {
            manifest["key"] = json!(key);
        }
        self.manifest(name, &manifest);
    }

    /// A `/bin/sh` script as connector `name`.
    fn script(&self, name: &str, body: &str) {
        self.manifest(name, &manifest(name, "run.sh"));
        write_mode(
            &self.dir().join(name).join("run.sh"),
            format!("#!/bin/sh\n{body}\n").as_bytes(),
            0o755,
        );
    }

    fn runner(&self, args: &[&str]) -> Output {
        Command::new(RUNNER)
            .args(args)
            .arg("--config")
            .arg(self.scratch.0.join("ulei.toml"))
            .current_dir(&self.scratch.0)
            .output()
            .expect("run ulei-connectors")
    }

    fn run(&self, name: &str) -> Output {
        self.runner(&["run", name])
    }

    fn records(&self) -> Vec<RunRecord> {
        fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("a C11 RunRecord line"))
            .collect()
    }

    fn last(&self) -> RunRecord {
        self.records().pop().expect("a run record")
    }

    fn cursor(&self, name: &str) -> Option<String> {
        let bytes = fs::read(self.state_file(name)).ok()?;
        let state: Value = serde_json::from_slice(&bytes).expect("runner.json");
        state["cursor"].as_str().map(str::to_owned)
    }

    fn set_cursor(&self, name: &str, cursor: &str) {
        fs::create_dir_all(self.state().join(name)).expect("state dir");
        fs::write(
            self.state_file(name),
            json!({ "cursor": cursor }).to_string(),
        )
        .expect("runner.json");
    }
}

fn manifest(name: &str, program: &str) -> Value {
    json!({
        "name": name,
        "version": "0.1.0",
        "contract": "0.1.0",
        "transport": "stdio",
        "command": [program],
        "schedule": {"every": 60},
        "source": {"addresses": []}
    })
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn items(from: u64, to: u64) -> Vec<String> {
    (from..=to).map(|n| format!("item-{n}")).collect()
}

#[test]
fn items_arrive_under_the_manifest_name_and_a_second_run_continues_from_the_cursor() {
    let bed = Bed::new();
    bed.test_connector("test", None);
    bed.config("test", "");
    let archive = bed.archive("test");

    let out = bed.run("test");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("ulei-connectors: test ok: stored 100, duplicate 0"),
        "{}",
        stderr(&out)
    );
    assert_eq!(archive.ids(), items(1, 100));
    let puts = archive.puts.lock().expect("puts");
    for (n, put) in (1u64..).zip(puts.iter()) {
        assert_eq!(put.body["source"], "test");
        let before = (n > 1).then(|| (n - 1).to_string());
        assert_eq!(put.cursor_then, before, "cursor when item {n} was put");
    }
    drop(puts);
    assert_eq!(bed.cursor("test").as_deref(), Some("100"));
    let record = bed.last();
    assert_eq!((record.outcome, record.stored), (RunOutcome::Ok, 100));
    assert!(record.cursor_moved);

    let out = bed.run("test");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(archive.ids()[100..], items(101, 200)[..]);
    assert_eq!(bed.cursor("test").as_deref(), Some("200"));
}

#[test]
fn a_failed_put_ends_the_run_archive_at_the_last_answered_checkpoint() {
    let bed = Bed::new();
    bed.test_connector("test", None);
    bed.config("test", "");
    let archive = bed.archive("test");
    archive.answer("item-3", 500, "store_failed");

    let out = bed.run("test");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("test archive: stored 2")
            && stderr(&out).contains("500 store_failed"),
        "{}",
        stderr(&out)
    );
    assert_eq!(archive.ids(), items(1, 3));
    assert_eq!(bed.cursor("test").as_deref(), Some("2"));
    assert_eq!(bed.last().outcome, RunOutcome::Archive);

    archive.clear_answers();
    let out = bed.run("test");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(archive.ids()[3..], items(3, 102)[..]);
}

#[test]
fn an_archive_that_does_not_answer_ends_the_run_archive_with_the_cursor_unchanged() {
    let bed = Bed::new();
    bed.test_connector("test", None);
    bed.config("test", "");
    bed.set_cursor("test", "7");

    let out = bed.run("test");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("test archive: stored 0")
            && stderr(&out).contains("put \"item-8\": connect:"),
        "{}",
        stderr(&out)
    );
    assert_eq!(bed.cursor("test").as_deref(), Some("7"));
    let record = bed.last();
    assert_eq!(record.outcome, RunOutcome::Archive);
    assert!(!record.cursor_moved);
}

#[test]
fn too_large_items_and_conflicts_are_logged_and_the_run_goes_on() {
    let bed = Bed::new();
    // Seventeen lines over the 1 MiB + 4 KiB line cap, one line under it whose `put` body is
    // over 1 MiB, a conflict and two small items.
    let item = |id: &str| {
        format!(
            "printf '%s\\n' '{{\"type\":\"item\",\"external_id\":\"{id}\",\"kind\":\"text/plain\",\"text\":\"t\",\"bytes_b64\":\"\"}}'"
        )
    };
    let script = format!(
        "{small1}
i=1
while [ $i -le 17 ]; do
  printf '{{\"type\":\"item\",\"external_id\":\"big-%s\",\"kind\":\"text/plain\",\"text\":\"' $i
  head -c 1060000 /dev/zero | tr '\\0' a
  printf '\",\"bytes_b64\":\"\"}}\\n'
  i=$((i + 1))
done
printf '{{\"type\":\"item\",\"external_id\":\"body-1\",\"kind\":\"text/plain\",\"text\":\"'
head -c 1048600 /dev/zero | tr '\\0' a
printf '\",\"bytes_b64\":\"\"}}\\n'
{conflict}
{small2}
printf '%s\\n' '{{\"type\":\"checkpoint\",\"cursor\":\"c1\"}}' '{{\"type\":\"done\"}}'",
        small1 = item("small-1"),
        conflict = item("conflict-1"),
        small2 = item("small-2"),
    );
    bed.script("big", &script);
    bed.config("big", "");
    let archive = bed.archive("big");
    archive.answer("conflict-1", 409, "conflict");

    let out = bed.run("big");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(archive.ids(), ["small-1", "conflict-1", "small-2"]);
    let record = bed.last();
    let mut too_large: Vec<String> = (1..=17).map(|n| format!("big-{n}")).collect();
    too_large.push("body-1".to_owned());
    assert_eq!(record.outcome, RunOutcome::Ok);
    assert_eq!(
        (record.stored, record.conflict, record.too_large),
        (2, 1, 18)
    );
    assert_eq!(record.too_large_ids, too_large);
    assert_eq!(record.conflict_ids, ["conflict-1"]);
    assert_eq!(bed.cursor("big").as_deref(), Some("c1"));
}

#[test]
fn discovery_schedules_only_enabled_names_with_valid_manifests() {
    let bed = Bed::new();
    bed.test_connector("test", None);
    bed.test_connector("other", None);
    let mut bad_port = manifest("badport", "run.sh");
    bad_port["source"]["addresses"] = json!(["source.example.org:65536"]);
    bed.manifest("badport", &bad_port);
    let mut unknown_field = manifest("badjson", "run.sh");
    unknown_field["network"] = json!("direct");
    bed.manifest("badjson", &unknown_field);
    bed.manifest("renamed", &manifest("test", "run.sh"));
    bed.manifest(".git", &manifest("git", "run.sh"));
    bed.config("test, badport,badjson,renamed,missing,Bad,.git", "");

    let out = bed.runner(&["list"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "test stdio every 60s\n"
    );
    let lines = stderr(&out);
    for expected in [
        "ulei-connectors: badport: source.addresses: port 65536 of \"source.example.org:65536\" is outside 1..65535",
        "ulei-connectors: badjson: connector.json is not a C11 Manifest: wrong field or value",
        "ulei-connectors: renamed: manifest name \"test\" is not its directory name",
        "ulei-connectors: missing: no connector.json in",
        "ulei-connectors: Bad: not a connector name",
        "ulei-connectors: .git: not a connector name",
    ] {
        assert!(lines.contains(expected), "{expected:?} in {lines}");
    }
    assert_eq!(lines.lines().count(), 6, "{lines}");

    let out = bed.run("badport");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("is outside 1..65535"),
        "{}",
        stderr(&out)
    );
    let out = bed.run("other");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("other: not in connectors.enabled"),
        "{}",
        stderr(&out)
    );
    assert_eq!(bed.records(), Vec::<RunRecord>::new());
}

#[test]
fn a_relative_key_file_or_data_dir_is_refused_before_the_connector_starts() {
    let bed = Bed::new();
    bed.test_connector("keyed", Some("notes"));
    bed.config(
        "keyed",
        "[connectors.keyed]\nkey_file = \"keys/notes.env\"\n",
    );
    let archive = bed.archive("keyed");

    let out = bed.run("keyed");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("keyed refused key_file_not_absolute")
            && stderr(&out).contains("connectors.keyed.key_file keys/notes.env is not absolute"),
        "{}",
        stderr(&out)
    );
    let record = bed.last();
    assert_eq!(record.outcome, RunOutcome::Refused);
    assert_eq!(record.code.as_deref(), Some("key_file_not_absolute"));

    let text = fs::read_to_string(bed.scratch.0.join("ulei.toml")).expect("config");
    let relative = text.replace(
        &format!("state = \"{}\"", bed.state().display()),
        "state = \"state\"",
    );
    fs::write(bed.scratch.0.join("ulei.toml"), relative).expect("config");
    let out = bed.run("keyed");
    assert!(
        stderr(&out).contains("keyed refused data_dir_not_absolute")
            && stderr(&out).contains("data_dir state/keyed/data is not absolute"),
        "{}",
        stderr(&out)
    );
    assert_eq!(bed.last().code.as_deref(), Some("data_dir_not_absolute"));
    assert!(!bed.scratch.0.join("state").exists());
    assert_eq!(archive.ids(), Vec::<String>::new());
}

#[test]
fn over_long_ids_and_cursors_and_broken_lines_end_or_refuse_the_run() {
    let bed = Bed::new();
    let long_id = "é".repeat(257);
    bed.script(
        "longid",
        &format!(
            "printf '%s\\n' '{{\"type\":\"item\",\"external_id\":\"{long_id}\",\"kind\":\"text/plain\",\"text\":\"t\",\"bytes_b64\":\"\"}}'"
        ),
    );
    let long_cursor = "x".repeat(4097);
    bed.script(
        "longcursor",
        &format!("printf '%s\\n' '{{\"type\":\"checkpoint\",\"cursor\":\"{long_cursor}\"}}'"),
    );
    bed.script("notjson", "printf 'hello\\n'");
    bed.script(
        "failing",
        "printf '%s\\n' '{\"type\":\"error\",\"code\":\"auth_failed\",\"message\":\"refused\"}'",
    );
    bed.script("crashing", "echo boom >&2; exit 3");
    bed.test_connector("stored", None);
    bed.set_cursor("stored", &long_cursor);
    bed.config("longid,longcursor,notjson,failing,crashing,stored", "");
    let archive = bed.archive("stored");

    for (name, outcome, code, line) in [
        (
            "longid",
            RunOutcome::Protocol,
            None,
            "line 1: external_id has 514 bytes, over 512",
        ),
        (
            "longcursor",
            RunOutcome::Protocol,
            None,
            "line 1: cursor has 4097 bytes, over 4096",
        ),
        (
            "notjson",
            RunOutcome::Protocol,
            None,
            "line 1: not a C11 Line: JSON syntax error",
        ),
        ("failing", RunOutcome::Error, Some("auth_failed"), "refused"),
        ("crashing", RunOutcome::Exit, None, "exit status: 3"),
        (
            "stored",
            RunOutcome::Refused,
            Some("cursor_too_long"),
            "stored cursor has 4097 bytes, over 4096",
        ),
    ] {
        let out = bed.run(name);
        assert_eq!(out.status.code(), Some(1), "{name}");
        assert!(stderr(&out).contains(line), "{name}: {}", stderr(&out));
        let record = bed.last();
        assert_eq!(record.connector, name);
        assert_eq!(record.outcome, outcome, "{name}");
        assert_eq!(record.code.as_deref(), code, "{name}");
    }
    assert_eq!(bed.last().stderr, "");
    let crashed = bed
        .records()
        .into_iter()
        .find(|r| r.connector == "crashing");
    assert_eq!(crashed.expect("crashing record").stderr, "boom\n");
    assert_eq!(archive.ids(), Vec::<String>::new());
}

#[test]
fn reset_clears_the_cursor_and_the_next_run_starts_over() {
    let bed = Bed::new();
    bed.test_connector("test", None);
    bed.config("test", "");
    let archive = bed.archive("test");
    assert!(bed.run("test").status.success());
    assert_eq!(bed.cursor("test").as_deref(), Some("100"));

    let out = bed.runner(&["reset", "test"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stderr(&out), "ulei-connectors: test: cursor cleared\n");
    assert_eq!(bed.cursor("test"), None);
    assert!(bed.state_file("test").is_file());

    assert!(bed.run("test").status.success());
    assert_eq!(archive.ids()[100..], items(1, 100)[..]);
    let record = bed.last();
    assert_eq!((record.stored, record.duplicate), (0, 100));

    let out = bed.runner(&["reset", "../test"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("is not a connector name"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn the_run_record_lists_its_fields_and_grants_without_key_value_or_content() {
    let bed = Bed::new();
    let key_value = "KEYVALUE-5f1c9e";
    let key_file = bed.scratch.0.join("notes.env");
    write_mode(
        &key_file,
        format!("NOTES_TOKEN={key_value}\n").as_bytes(),
        0o600,
    );
    bed.test_connector("test", Some("notes"));
    bed.config(
        "test",
        &format!(
            "[connectors.test]\nkey_file = \"{}\"\nallow = \"files.example.org:443, 10.0.0.2:8080\"\n",
            key_file.display()
        ),
    );
    let archive = bed.archive("test");

    let out = bed.run("test");
    assert!(out.status.success(), "{}", stderr(&out));
    let puts = archive.puts.lock().expect("puts");
    assert!(
        puts[0].body["text"]
            .as_str()
            .expect("text")
            .contains("key sha256:")
    );
    drop(puts);
    let log = fs::read_to_string(bed.log()).expect("log");
    let mode = fs::metadata(bed.log()).expect("log").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(log.lines().count(), 1);
    let line: Value = serde_json::from_str(log.trim_end()).expect("record");
    let mut keys: Vec<&str> = line
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "at",
            "conflict",
            "conflict_ids",
            "connector",
            "cursor_moved",
            "duplicate",
            "duration_ms",
            "grants",
            "outcome",
            "stderr",
            "stored",
            "too_large",
            "too_large_ids"
        ]
    );
    assert_eq!(
        line["grants"],
        json!({"allow": ["files.example.org:443", "10.0.0.2:8080"]})
    );
    for text in [log.as_str(), &stderr(&out)] {
        assert!(!text.contains(key_value));
        assert!(!text.contains("synthetic item"));
        assert!(!text.contains("key sha256"));
    }
}
