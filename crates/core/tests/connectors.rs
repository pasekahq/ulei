//! `ulei-connectors` against a fake C10 socket, with `ulei-connector-test`, scripted stdio
//! connectors and scripted MCP servers over pipes; `serve` under SIGTERM. A fake `systemd-run` on `PATH` records its argv, skips the launcher,
//! which refuses outside a unit, runs the connector under `timeout` for `RuntimeMaxSec=` and
//! prints the real one's start and result lines, or fails to start under a marker file; a fake
//! `systemctl` records its argv and stops the run. A scripted connector stands in for the
//! launcher's stderr line where a test needs it.

mod common;

use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use ulei_contracts::generated::c11::{RunOutcome, RunRecord};

use common::http::{json_answer, read_request};
use common::{Scratch, unix_now, write_mode};

const RUNNER: &str = env!("CARGO_BIN_EXE_ulei-connectors");
const ITEM_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The binary `name` of this build: in the target directory that holds this test.
fn built(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test executable");
    exe.ancestors()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "no {name} above {}; build the workspace (cargo test --workspace)",
                exe.display()
            )
        })
}

/// One `put` the fake archive received, with the cursor stored when it arrived.
struct Put {
    raw: Vec<u8>,
    body: Value,
    cursor_then: Option<String>,
}

/// A C10 socket that answers `put`: `stored` for a new `(source, external_id)`, `duplicate`
/// for a repeat, and the status and code of `answers` for the external ids it names; and
/// `sync`, which it counts.
struct FakeArchive {
    puts: Arc<Mutex<Vec<Put>>>,
    answers: Arc<Mutex<HashMap<String, (u16, &'static str)>>>,
    syncs: Arc<AtomicUsize>,
}

impl FakeArchive {
    fn start(socket: &Path, state_file: PathBuf) -> Self {
        let listener = UnixListener::bind(socket).expect("bind the fake archive");
        let puts = Arc::new(Mutex::new(Vec::<Put>::new()));
        let answers = Arc::new(Mutex::new(HashMap::new()));
        let syncs = Arc::new(AtomicUsize::new(0));
        let (seen, rules, synced) = (Arc::clone(&puts), Arc::clone(&answers), Arc::clone(&syncs));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some((target, body)) = read_request(&mut stream) else {
                    continue;
                };
                if target == "POST /v1/sync" {
                    assert_eq!(body, b"{}");
                    synced.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(&json_answer(200, &json!({"put": 0, "synced": true})));
                    continue;
                }
                assert_eq!(target, "POST /v1/put");
                let raw = body;
                let body: Value = serde_json::from_slice(&raw).expect("put body");
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
                seen.push(Put {
                    raw,
                    body,
                    cursor_then,
                });
                drop(seen);
                let _ = stream.write_all(&answer);
            }
        });
        Self {
            puts,
            answers,
            syncs,
        }
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
        fs::create_dir(bed.fakes()).expect("fakes dir");
        let calls = bed.scratch.0.join("calls");
        write_mode(
            &bed.fakes().join("systemd-run"),
            format!(
                r#"#!/bin/sh
printf 'systemd-run' >> '{calls}'
printf ' %s' "$@" >> '{calls}'
printf '\n' >> '{calls}'
max=
unit=
while [ "$1" != -- ]; do
  case "$1" in
    --property=RuntimeMaxSec=*) max=${{1#--property=RuntimeMaxSec=}} ;;
    --unit=*) unit=${{1#--unit=}}.service ;;
    --property=OpenFile=*)
      broker=${{1#--property=OpenFile=}}
      if [ -S "${{broker%:broker}}" ]; then echo "${{broker%:broker}}" >> '{brokers}'; fi ;;
  esac
  shift
done
if [ -e '{fakes}/fail-start' ]; then
  echo "Failed to start transient service unit: Unit $unit was already loaded or has a fragment file." >&2
  exit 1
fi
shift
while [ "$1" != -- ]; do shift; done
shift
echo "Running as unit: $unit; invocation ID: 0f" >&2
sh -c 'echo $$ > "$0"; exec timeout "$@"' '{fakes}/'"$unit.pid" "$max" "$@"
code=$?
case $code in
  0) result=success ;;
  124) result=timeout code=1 ;;
  *) result=exit-code ;;
esac
echo "Finished with result: $result" >&2
echo "Main processes terminated with: code=exited/status=$code" >&2
exit $code
"#,
                calls = calls.display(),
                fakes = bed.fakes().display(),
                brokers = bed.scratch.0.join("brokers").display(),
            )
            .as_bytes(),
            0o755,
        );
        write_mode(
            &bed.fakes().join("systemctl"),
            format!(
                "#!/bin/sh\nprintf 'systemctl %s\\n' \"$*\" >> '{calls}'\nif [ \"$2\" = stop ]; then kill \"$(cat '{fakes}/'\"$3.pid\")\"; fi\n",
                calls = calls.display(),
                fakes = bed.fakes().display(),
            )
            .as_bytes(),
            0o755,
        );
        bed
    }

    fn fakes(&self) -> PathBuf {
        self.scratch.0.join("fakes")
    }

    /// The fakes' calls, one line each: the program and its argv.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.scratch.0.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The broker sockets the fake `systemd-run` found listening when it started, one per line.
    fn brokers(&self) -> Vec<String> {
        fs::read_to_string(self.scratch.0.join("brokers"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
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

    fn run_dir(&self) -> PathBuf {
        self.scratch.0.join("run")
    }

    /// The files left in `run_dir`.
    fn left_in_run_dir(&self) -> Vec<String> {
        fs::read_dir(self.run_dir())
            .map(|dir| {
                dir.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
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
            "[memory]\nsocket = \"{}\"\n\n[connectors]\ndir = \"{}\"\nstate = \"{}\"\nlog = \"{}\"\nrun_dir = \"{}\"\nenabled = \"{enabled}\"\n\n{extra}",
            self.socket().display(),
            self.dir().display(),
            self.state().display(),
            self.log().display(),
            self.run_dir().display(),
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
        symlink(
            built("ulei-connector-test"),
            bin.join("ulei-connector-test"),
        )
        .expect("symlink");
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

    fn command(&self, args: &[&str]) -> Command {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.fakes()];
        paths.extend(std::env::split_paths(&path));
        let mut command = Command::new(RUNNER);
        command
            .env("PATH", std::env::join_paths(paths).expect("PATH"))
            .args(args)
            .arg("--config")
            .arg(self.scratch.0.join("ulei.toml"))
            .current_dir(&self.scratch.0);
        command
    }

    fn runner(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run ulei-connectors")
    }

    /// A script connector `name` that blocks reading the FIFO it returns until the test opens
    /// it for writing, then ends `done`; that open returns once the connector runs.
    fn blocking(&self, name: &str) -> PathBuf {
        let fifo = self.scratch.0.join(format!("{name}.fifo"));
        let made = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
        assert!(made.success());
        self.script(
            name,
            &format!(
                "read line < '{}'\nprintf '%s\\n' '{{\"type\":\"done\"}}'",
                fifo.display()
            ),
        );
        fifo
    }

    fn runner_state(&self, name: &str) -> Value {
        serde_json::from_slice(&fs::read(self.state_file(name)).expect("runner.json"))
            .expect("runner.json")
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

impl Bed {
    /// A `/bin/sh` MCP server as connector `name` running `body`, with `$log` naming the file of
    /// the messages it reads.
    fn mcp(&self, name: &str, body: &str) {
        let mut manifest = manifest(name, "run.sh");
        manifest["transport"] = json!("mcp");
        self.manifest(name, &manifest);
        write_mode(
            &self.dir().join(name).join("run.sh"),
            format!(
                "#!/bin/sh\nlog='{}'\n{body}\n",
                self.mcp_log_path(name).display()
            )
            .as_bytes(),
            0o755,
        );
    }

    fn mcp_log_path(&self, name: &str) -> PathBuf {
        self.dir().join(name).join("in.log")
    }

    /// The messages the MCP server `name` read, in order.
    fn mcp_log(&self, name: &str) -> Vec<Value> {
        fs::read_to_string(self.mcp_log_path(name))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON message"))
            .collect()
    }
}

/// Shell that reads one message of the runner and logs it.
const READ_LINE: &str = "read -r m; printf '%s\\n' \"$m\" >> \"$log\"";

/// Shell that writes `message` as one line.
fn say(message: &Value) -> String {
    let text = message.to_string();
    assert!(!text.contains('\''), "{text}");
    format!("printf '%s\\n' '{text}'")
}

/// Shell that reads one request and answers it with `message`.
fn answer(message: &Value) -> String {
    format!("{READ_LINE}\n{}", say(message))
}

fn initialized(version: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "result": {
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "scripted", "version": "0.0.1"},
    }})
}

/// Shell of the handshake: `initialize` answered with protocol `2025-11-25`, then
/// `notifications/initialized` read.
fn handshake() -> String {
    format!("{}\n{READ_LINE}", answer(&initialized("2025-11-25")))
}

fn mcp_item(id: &str) -> Value {
    json!({"external_id": id, "kind": "text/plain", "text": "t", "bytes_b64": ""})
}

/// The answer to call `id` with a page of `items`; its `content` repeats them as text.
fn page(id: u64, items: &[&str], cursor: Option<&str>, more: bool) -> Value {
    let items: Vec<Value> = items.iter().map(|id| mcp_item(id)).collect();
    json!({"jsonrpc": "2.0", "id": id, "result": {
        "content": [{"type": "text", "text": Value::from(items.clone()).to_string()}],
        "structuredContent": {"items": items, "cursor": cursor, "more": more},
    }})
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
    let unit = record.unit.expect("the run's unit");
    let at = unit
        .strip_prefix("ulei-connector-test-")
        .and_then(|rest| rest.strip_suffix(".service"))
        .expect("unit name");
    assert!(at.bytes().all(|b| b.is_ascii_digit()), "{unit}");
    let calls = bed.calls();
    let [call] = calls.as_slice() else {
        panic!("{calls:?}")
    };
    let (options, launcher) = call.split_once(" -- ").expect("launcher after --");
    assert!(
        options.starts_with(&format!(
            "systemd-run --user --wait --pipe --collect --unit=ulei-connector-test-{at} --property=NoNewPrivileges=yes "
        )),
        "{options}"
    );
    assert!(
        options.ends_with(" --property=TimeoutStopSec=10s --property=RuntimeMaxSec=300s"),
        "{options}"
    );
    assert_eq!(
        launcher,
        format!(
            "{RUNNER} exec test {} {} -- {}",
            bed.state().join("test").join("data").display(),
            bed.dir().join("test").display(),
            bed.dir()
                .join("test")
                .join("bin")
                .join("ulei-connector-test")
                .display()
        )
    );

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
    let record = bed.last();
    assert_eq!(record.outcome, RunOutcome::Archive);
    assert_eq!(
        bed.calls().last().map(String::as_str),
        Some(format!("systemctl --user stop {}", record.unit.expect("unit")).as_str())
    );

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
    bed.script(
        "launcher",
        "echo 'ulei-connectors: exec refused seccomp: NoNewPrivs 0, Seccomp 0' >&2; exit 125",
    );
    bed.script(
        "execfailed",
        "echo 'ulei-connectors: exec landlock abi 8' >&2; echo 'ulei-connectors: exec refused exec_failed: /srv/c/bin/run: No such file or directory (os error 2)' >&2; exit 125",
    );
    bed.script(
        "forged",
        "echo 'ulei-connectors: exec landlock abi 8' >&2; echo 'ulei-connectors: exec refused seccomp: x' >&2; exit 125",
    );
    bed.script(
        "fakeresult",
        "echo 'Finished with result: timeout' >&2; exit 3",
    );
    bed.test_connector("stored", None);
    bed.set_cursor("stored", &long_cursor);
    bed.config(
        "longid,longcursor,notjson,failing,crashing,launcher,execfailed,forged,fakeresult,stored",
        "",
    );
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
            "launcher",
            RunOutcome::Refused,
            Some("seccomp"),
            "launcher refused seccomp: stored 0",
        ),
        (
            "execfailed",
            RunOutcome::Refused,
            Some("exec_failed"),
            "execfailed refused exec_failed: stored 0",
        ),
        ("forged", RunOutcome::Exit, None, "exit status: 125"),
        ("fakeresult", RunOutcome::Exit, None, "exit status: 3"),
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
            "too_large_ids",
            "unit"
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

#[test]
fn a_run_past_its_timeout_ends_timeout_and_keeps_its_stored_cursor() {
    let bed = Bed::new();
    let mut sleeper = manifest("sleeper", "run.sh");
    sleeper["timeout"] = json!(1);
    bed.manifest("sleeper", &sleeper);
    write_mode(
        &bed.dir().join("sleeper").join("run.sh"),
        b"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"item\",\"external_id\":\"s-1\",\"kind\":\"text/plain\",\"text\":\"t\",\"bytes_b64\":\"\"}' '{\"type\":\"checkpoint\",\"cursor\":\"c1\"}'\nexec sleep 30\n",
        0o755,
    );
    bed.config("sleeper", "");
    let archive = bed.archive("sleeper");

    let out = bed.run("sleeper");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("sleeper timeout: stored 1")
            && stderr(&out).contains("ran past its timeout of 1 s"),
        "{}",
        stderr(&out)
    );
    let record = bed.last();
    assert_eq!(record.outcome, RunOutcome::Timeout);
    assert!(record.cursor_moved);
    assert!(record.duration_ms < 10_000, "{}", record.duration_ms);
    assert_eq!(bed.cursor("sleeper").as_deref(), Some("c1"));
    assert_eq!(archive.ids(), ["s-1"]);
    assert!(bed.calls()[0].contains(" --property=RuntimeMaxSec=1s "));
}

#[test]
fn exec_outside_a_run_unit_is_refused_before_it_executes_anything() {
    let bed = Bed::new();
    let marker = bed.scratch.0.join("marker");
    let data = bed.scratch.0.join("data");
    let marker_text = marker.to_str().expect("UTF-8 path");
    let data_text = data.to_str().expect("UTF-8 path");
    let out = Command::new(RUNNER)
        .args([
            "exec",
            "test",
            data_text,
            "/usr",
            "--",
            "/usr/bin/touch",
            marker_text,
        ])
        .output()
        .expect("run the launcher");
    assert_eq!(out.status.code(), Some(125), "{}", stderr(&out));
    let line = stderr(&out);
    // A test host that itself runs under NoNewPrivs and a filter is still no run unit.
    assert!(
        line.starts_with("ulei-connectors: exec refused seccomp: NoNewPrivs ")
            || line.starts_with("ulei-connectors: exec refused unit: cgroup "),
        "{line}"
    );
    assert_eq!(line.lines().count(), 1, "{line}");
    assert!(!marker.exists());
    assert!(!data.exists());

    for args in [
        &["exec", "Bad", data_text, "--", "/usr/bin/touch"][..],
        &["exec", "test", data_text, "/usr/bin/touch", marker_text],
        &["exec", "test", data_text, "--"],
    ] {
        let out = Command::new(RUNNER).args(args).output().expect("run");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
    assert!(!marker.exists());
}

#[test]
fn a_unit_that_fails_to_start_is_refused_with_the_line_of_systemd_run() {
    let bed = Bed::new();
    bed.test_connector("test", None);
    bed.config("test", "");
    let archive = bed.archive("test");
    fs::write(bed.fakes().join("fail-start"), "").expect("marker");

    let out = bed.run("test");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains(
            "test refused unit_start_failed: stored 0, duplicate 0, conflict 0, too_large 0, cursor unchanged, "
        ) && stderr(&out).contains("; systemd-run: Failed to start transient service unit: Unit ulei-connector-test-"),
        "{}",
        stderr(&out)
    );
    let record = bed.last();
    assert_eq!(record.outcome, RunOutcome::Refused);
    assert_eq!(record.code.as_deref(), Some("unit_start_failed"));
    assert_eq!(record.unit, None);
    assert_eq!(archive.ids(), Vec::<String>::new());
}

#[test]
fn the_launcher_gets_the_owner_paths_and_the_record_names_the_landlock_used() {
    let bed = Bed::new();
    let key_file = bed.scratch.0.join("notes.env");
    write_mode(&key_file, b"NOTES_TOKEN=x\n", 0o600);
    bed.test_connector("keyed", Some("notes"));
    bed.script(
        "landlocked",
        "echo 'ulei-connectors: exec landlock abi 8' >&2; echo said >&2; printf '%s\\n' '{\"type\":\"done\"}'",
    );
    bed.script("unconfined", "printf '%s\\n' '{\"type\":\"done\"}'");
    bed.script("relative", "printf '%s\\n' '{\"type\":\"done\"}'");
    bed.config(
        "keyed,landlocked,unconfined,relative",
        &format!(
            "[connectors.keyed]\nkey_file = \"{}\"\nread = \"/srv/shared, /srv/export\"\nwrite = \"/srv/outbox\"\n\n\
             [connectors.unconfined]\nlandlock = \"off\"\n\n\
             [connectors.relative]\nwrite = \"/srv/outbox, outbox\"\n",
            key_file.display()
        ),
    );
    let _archive = bed.archive("keyed");

    let out = bed.run("keyed");
    assert!(out.status.success(), "{}", stderr(&out));
    let launcher = |name: &str| {
        bed.calls()
            .iter()
            .filter(|call| call.starts_with("systemd-run "))
            .find_map(|call| {
                let (_, launcher) = call.split_once(" -- ")?;
                launcher
                    .contains(&format!(" exec {name} "))
                    .then(|| launcher.to_owned())
            })
            .expect("a systemd-run call")
    };
    let dir = bed.dir().join("keyed");
    assert_eq!(
        launcher("keyed"),
        format!(
            "{RUNNER} exec keyed {} {} --key {} --read /srv/shared --read /srv/export --write /srv/outbox -- {}",
            bed.state().join("keyed").join("data").display(),
            dir.display(),
            key_file.display(),
            dir.join("bin").join("ulei-connector-test").display()
        )
    );
    assert_eq!(
        bed.last().grants.read.as_deref(),
        Some(&["/srv/shared".to_owned(), "/srv/export".to_owned()][..])
    );

    let out = bed.run("landlocked");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(" ms, landlock abi 8\n"),
        "{}",
        stderr(&out)
    );
    let record = bed.last();
    assert_eq!(record.landlock_abi, Some(8));
    assert_eq!(record.stderr, "said\n");

    let out = bed.run("unconfined");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(" ms, landlock off\n"),
        "{}",
        stderr(&out)
    );
    assert!(launcher("unconfined").contains(" --landlock-off -- "));
    let log = fs::read_to_string(bed.log()).expect("log");
    let line: Value = serde_json::from_str(log.lines().last().expect("line")).expect("record");
    assert_eq!(line["grants"], json!({"landlock": "off"}));
    assert_eq!(line.get("landlock_abi"), None);

    let out = bed.run("relative");
    assert!(
        stderr(&out).contains("relative refused owner_record")
            && stderr(&out).contains("connectors.relative.write: outbox is not absolute"),
        "{}",
        stderr(&out)
    );
    assert_eq!(bed.last().unit, None);
}

#[test]
fn the_broker_listens_before_the_unit_starts_and_leaves_no_socket_after_ok_exit_or_refused() {
    let bed = Bed::new();
    bed.test_connector("test", None);
    bed.script("failing", "exit 3");
    bed.config("test,failing", "");
    let _archive = bed.archive("test");

    for (name, outcome) in [("test", RunOutcome::Ok), ("failing", RunOutcome::Exit)] {
        bed.run(name);
        let record = bed.last();
        assert_eq!(record.outcome, outcome, "{name}: {}", record.stderr);
        let unit = record.unit.expect("unit");
        let socket = bed.run_dir().join(format!(
            "{}.sock",
            unit.strip_suffix(".service").expect("unit")
        ));
        let calls = bed.calls();
        let call = calls
            .iter()
            .rfind(|c| c.starts_with("systemd-run "))
            .expect("call");
        assert!(
            call.contains(&format!(
                " --property=RestrictAddressFamilies= --property=OpenFile={}:broker ",
                socket.display()
            )),
            "{call}"
        );
        assert!(
            bed.brokers().contains(&socket.display().to_string()),
            "{name}: {:?}",
            bed.brokers()
        );
        assert_eq!(bed.left_in_run_dir(), Vec::<String>::new(), "{name}");
    }

    fs::write(bed.fakes().join("fail-start"), "").expect("marker");
    bed.run("test");
    assert_eq!(bed.last().code.as_deref(), Some("unit_start_failed"));
    assert_eq!(bed.left_in_run_dir(), Vec::<String>::new());
    let mode = fs::metadata(bed.run_dir())
        .expect("run_dir")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn network_direct_opens_inet_and_the_record_names_the_connect_ports_landlock_limits() {
    let bed = Bed::new();
    let addresses = json!(["notes.example.test:443", "notes.example.test:8080"]);
    for (name, landlock) in [
        ("direct", "abi 4 connect 443,8080,9000"),
        ("older", "abi 3"),
    ] {
        let mut direct = manifest(name, "run.sh");
        direct["source"]["addresses"] = addresses.clone();
        bed.manifest(name, &direct);
        write_mode(
            &bed.dir().join(name).join("run.sh"),
            format!(
                "#!/bin/sh\necho 'ulei-connectors: exec landlock {landlock}' >&2\nprintf '%s\\n' '{{\"type\":\"done\"}}'\n"
            )
            .as_bytes(),
            0o755,
        );
    }
    bed.config(
        "direct,older",
        "[connectors.direct]\nnetwork = \"direct\"\nallow = \"127.0.0.1:8080, 127.0.0.1:9000\"\n\n\
         [connectors.older]\nnetwork = \"direct\"\n",
    );

    let out = bed.run("direct");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(", landlock abi 4, direct connect ports [443,8080,9000]\n"),
        "{}",
        stderr(&out)
    );
    let record = bed.last();
    assert_eq!(record.connect_ports, Some(vec![443, 8080, 9000]));
    assert_eq!(record.landlock_abi, Some(4));
    let calls = bed.calls();
    let call = calls.last().expect("call");
    assert!(
        call.contains(" --property=RestrictAddressFamilies=AF_INET AF_INET6 --property=OpenFile="),
        "{call}"
    );
    assert!(
        call.contains(" --direct --port 443 --port 8080 --port 9000 -- "),
        "{call}"
    );

    let out = bed.run("older");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(", landlock abi 3, direct connect ports not limited\n"),
        "{}",
        stderr(&out)
    );
    let log = fs::read_to_string(bed.log()).expect("log");
    let line: Value = serde_json::from_str(log.lines().last().expect("line")).expect("record");
    assert_eq!(line.get("connect_ports"), None);
    assert_eq!(line["grants"], json!({"network": "direct"}));
    assert!(
        bed.calls()
            .last()
            .expect("call")
            .contains(" --direct --port 443 --port 8080 -- ")
    );
}

#[test]
fn a_second_run_of_a_connector_while_one_runs_is_busy_and_starts_no_unit() {
    let bed = Bed::new();
    let fifo = bed.blocking("slow");
    bed.config("slow", "");
    let first = bed
        .command(&["run", "slow"])
        .stderr(Stdio::piped())
        .spawn()
        .expect("first run");
    let mut release = File::options()
        .write(true)
        .open(&fifo)
        .expect("the first run's connector reads the FIFO");

    let out = bed.run("slow");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("slow busy: stored 0, duplicate 0, conflict 0, too_large 0, cursor unchanged, 0 ms; its last run still runs"),
        "{}",
        stderr(&out)
    );
    let busy = bed.last();
    assert_eq!((busy.outcome, busy.unit), (RunOutcome::Busy, None));
    assert_eq!(
        bed.calls()
            .iter()
            .filter(|c| c.starts_with("systemd-run "))
            .count(),
        1
    );

    writeln!(release, "go").expect("release");
    drop(release);
    let first = first.wait_with_output().expect("first run");
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(bed.last().outcome, RunOutcome::Ok);

    let again = bed
        .command(&["run", "slow"])
        .stderr(Stdio::piped())
        .spawn()
        .expect("third run");
    let mut release = File::options().write(true).open(&fifo).expect("FIFO");
    writeln!(release, "go").expect("release");
    drop(release);
    let again = again.wait_with_output().expect("third run");
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(bed.last().outcome, RunOutcome::Ok);
}

#[test]
fn rate_limited_sets_not_before_in_the_state_and_other_errors_keep_the_schedule() {
    let bed = Bed::new();
    bed.script(
        "limited",
        "printf '%s\\n' '{\"type\":\"error\",\"code\":\"rate_limited\",\"message\":\"slow down\",\"retry_after\":300}'",
    );
    bed.script(
        "refused",
        "printf '%s\\n' '{\"type\":\"error\",\"code\":\"auth_failed\",\"message\":\"no\"}'",
    );
    bed.config("limited,refused", "");
    bed.set_cursor("limited", "c7");

    let before = unix_now();
    let out = bed.run("limited");
    let after = unix_now();
    assert_eq!(out.status.code(), Some(1));
    let record = bed.last();
    assert_eq!(
        (record.outcome, record.code.as_deref()),
        (RunOutcome::Error, Some("rate_limited"))
    );
    let state = bed.runner_state("limited");
    assert_eq!(state["cursor"], "c7");
    let start = state["last_start"].as_f64().expect("last_start");
    assert!((record.at - start).abs() < 1e-6, "{start} {}", record.at);
    let not_before = state["not_before"].as_f64().expect("not_before");
    assert!(
        not_before >= before + 300.0 && not_before <= after + 300.0,
        "{before} {not_before} {after}"
    );

    let out = bed.run("refused");
    assert_eq!(out.status.code(), Some(1));
    let state = bed.runner_state("refused");
    assert!(state["last_start"].as_f64().is_some(), "{state}");
    assert_eq!(state.get("not_before"), None);
}

#[test]
fn a_two_mib_item_with_its_external_id_last_is_too_large_and_a_cut_line_without_one_is_protocol() {
    let bed = Bed::new();
    let big = "printf '{\"type\":\"item\",\"kind\":\"text/plain\",\"bytes_b64\":\"\",\"text\":\"'
head -c 2097152 /dev/zero | tr '\\0' a
printf '\",\"meta\":{\"external_id\":\"inner\"},\"external_id\":\"last-1\"}\\n'";
    bed.script(
        "big",
        &format!("{big}\nprintf '%s\\n' '{{\"type\":\"done\",\"cursor\":\"c1\"}}'"),
    );
    bed.script(
        "noid",
        "printf '{\"type\":\"item\",\"text\":\"'\nhead -c 2097152 /dev/zero | tr '\\0' a\nprintf '\"}\\n{\"type\":\"done\"}\\n'",
    );
    bed.config("big,noid", "");
    let archive = bed.archive("big");

    let out = bed.run("big");
    assert!(out.status.success(), "{}", stderr(&out));
    let record = bed.last();
    assert_eq!(record.too_large_ids, ["last-1"]);
    assert_eq!((record.too_large, record.stored), (1, 0));
    assert_eq!(bed.cursor("big").as_deref(), Some("c1"));

    let out = bed.run("noid");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains(
            "noid protocol: stored 0, duplicate 0, conflict 0, too_large 0, cursor unchanged, "
        ) && stderr(&out).contains(
            "; line 1 is over 1052672 bytes and has no top-level external_id, so no too_large id can be kept"
        ),
        "{}",
        stderr(&out)
    );
    assert_eq!(archive.ids(), Vec::<String>::new());
}

#[test]
fn serve_runs_due_connectors_calls_sync_and_on_sigterm_stops_its_units_with_their_records() {
    let bed = Bed::new();
    bed.test_connector("test", None);
    let fifo = bed.blocking("slow");
    bed.config("test,slow,missing", "sync_every = 600\n");
    let archive = bed.archive("test");

    let mut serve = bed
        .command(&["serve"])
        .stderr(Stdio::piped())
        .spawn()
        .expect("serve");
    let (lines, said) = mpsc::channel();
    let pipe = serve.stderr.take().expect("stderr");
    let reader = thread::spawn(move || {
        for line in BufReader::new(pipe).lines() {
            let line = line.expect("stderr line");
            lines.send(line).expect("line");
        }
    });
    let wait_for = |needle: &str| -> Vec<String> {
        let mut seen = Vec::new();
        loop {
            let line = said
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|e| panic!("{e} before {needle:?}: {seen:?}"));
            let found = line.contains(needle);
            seen.push(line);
            if found {
                return seen;
            }
        }
    };
    let mut seen = wait_for("ulei-connectors: test ok: stored 100");
    let release = File::options()
        .write(true)
        .open(&fifo)
        .expect("the slow connector reads the FIFO");

    let manual = bed.run("slow");
    assert_eq!(manual.status.code(), Some(1));
    assert!(
        stderr(&manual).contains("slow busy: "),
        "{}",
        stderr(&manual)
    );

    let killed = Command::new("kill")
        .args(["-TERM", &serve.id().to_string()])
        .status()
        .expect("kill");
    assert!(killed.success());
    seen.extend(wait_for("ulei-connectors: stopped"));
    let status = serve.wait().expect("serve");
    drop(release);
    reader.join().expect("reader");
    assert_eq!(status.code(), Some(0), "{seen:?}");
    let seen = seen.join("\n");
    for line in [
        "ulei-connectors: missing: no connector.json in",
        "ulei-connectors: ready: 2 connectors, sync every 600 s",
        "ulei-connectors: stopping: 1 runs in flight",
        "stopped at the runner's shutdown",
    ] {
        assert!(seen.contains(line), "{line:?} in {seen}");
    }

    let records = bed.records();
    let of = |name: &str| -> Vec<&RunRecord> {
        records.iter().filter(|r| r.connector == name).collect()
    };
    assert_eq!(of("test").len(), 1);
    assert_eq!(of("test")[0].outcome, RunOutcome::Ok);
    let slow = of("slow");
    assert_eq!(slow.len(), 2, "{slow:?}");
    assert_eq!(
        (slow[0].outcome, slow[0].unit.as_ref()),
        (RunOutcome::Busy, None)
    );
    let unit = slow[1].unit.clone().expect("the stopped run's unit");
    assert_eq!(slow[1].outcome, RunOutcome::Exit);
    assert!(
        bed.calls()
            .contains(&format!("systemctl --user stop {unit}")),
        "{:?}",
        bed.calls()
    );
    assert_eq!(archive.syncs.load(Ordering::SeqCst), 1);
    assert_eq!(archive.ids(), items(1, 100));
    assert!(bed.runner_state("test")["last_start"].as_f64().is_some());
    assert_eq!(bed.left_in_run_dir(), Vec::<String>::new());
}

#[test]
fn an_mcp_connector_pages_through_ulei_pull_and_stores_each_cursor_after_its_items() {
    let bed = Bed::new();
    let pages = [
        answer(&page(2, &["m-1", "m-2"], Some("c1"), true)),
        answer(&page(3, &["m-3"], Some("c2"), true)),
        answer(&page(4, &["m-4"], Some("c3"), false)),
    ];
    bed.mcp("mcp", &format!("{}\n{}", handshake(), pages.join("\n")));
    bed.config("mcp", "");
    bed.set_cursor("mcp", "c0");
    let archive = bed.archive("mcp");
    archive.answer("m-2", 409, "conflict");

    let out = bed.run("mcp");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out)
            .contains("mcp ok: stored 3, duplicate 0, conflict 1, too_large 0, cursor moved"),
        "{}",
        stderr(&out)
    );
    let puts = archive.puts.lock().expect("puts");
    let seen: Vec<(&str, Option<&str>)> = puts
        .iter()
        .map(|p| {
            assert_eq!(p.body["source"], "mcp");
            (
                p.body["external_id"].as_str().expect("id"),
                p.cursor_then.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("m-1", Some("c0")),
            ("m-2", Some("c0")),
            ("m-3", Some("c1")),
            ("m-4", Some("c2"))
        ]
    );
    drop(puts);
    assert_eq!(bed.cursor("mcp").as_deref(), Some("c3"));
    let record = bed.last();
    assert_eq!(record.outcome, RunOutcome::Ok);
    assert_eq!(record.conflict_ids, ["m-2"]);
    assert!(
        bed.calls()
            .iter()
            .all(|call| !call.starts_with("systemctl")),
        "{:?}",
        bed.calls()
    );

    let read = bed.mcp_log("mcp");
    assert_eq!(read.len(), 5, "{read:?}");
    assert_eq!(
        read[0],
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "mcp", "version": "0.1.0"},
        }})
    );
    assert_eq!(
        read[1],
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    );
    let contract = read[2]["params"]["arguments"]["contract"].clone();
    assert!(
        contract.as_str().is_some_and(|v| v.starts_with("0.")),
        "{contract}"
    );
    for (index, (id, cursor)) in [(2u64, "c0"), (3, "c1"), (4, "c2")].into_iter().enumerate() {
        assert_eq!(
            read[index + 2],
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
                "name": "ulei_pull",
                "arguments": {
                    "contract": contract,
                    "cursor": cursor,
                    "limit": 100,
                    "key_file": null,
                    "data_dir": bed.state().join("mcp").join("data"),
                },
            }})
        );
    }
}

#[test]
fn a_server_request_gets_method_not_found_and_a_notification_is_ignored() {
    let bed = Bed::new();
    let body = format!(
        "{}\n{READ_LINE}\n{}\n{}\n{READ_LINE}\n{}",
        handshake(),
        say(&json!({"jsonrpc": "2.0", "method": "notifications/message",
            "params": {"level": "info", "data": "synthetic"}})),
        say(&json!({"jsonrpc": "2.0", "id": "r-1", "method": "roots/list"})),
        say(&page(2, &["m-1"], Some("c1"), false)),
    );
    bed.mcp("asks", &body);
    bed.config("asks", "");
    let archive = bed.archive("asks");

    let out = bed.run("asks");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(archive.ids(), ["m-1"]);
    let read = bed.mcp_log("asks");
    assert_eq!(read.len(), 4, "{read:?}");
    assert_eq!(
        read[3],
        json!({"jsonrpc": "2.0", "id": "r-1",
            "error": {"code": -32601, "message": "Method not found"}})
    );
}

#[test]
fn an_is_error_result_ends_the_run_error_and_rate_limited_sets_not_before() {
    let bed = Bed::new();
    let limited = json!({"jsonrpc": "2.0", "id": 2, "result": {
        "content": [{"type": "text", "text": "slow down"}],
        "structuredContent": {"code": "rate_limited", "message": "slow down", "retry_after": 300},
        "isError": true,
    }});
    bed.mcp("limited", &format!("{}\n{}", handshake(), answer(&limited)));
    bed.config("limited", "");
    bed.set_cursor("limited", "c7");

    let before = unix_now();
    let out = bed.run("limited");
    let after = unix_now();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("limited error rate_limited: stored 0")
            && stderr(&out).contains("; slow down"),
        "{}",
        stderr(&out)
    );
    let state = bed.runner_state("limited");
    assert_eq!(state["cursor"], "c7");
    let not_before = state["not_before"].as_f64().expect("not_before");
    assert!(
        not_before >= before + 300.0 && not_before <= after + 300.0,
        "{before} {not_before} {after}"
    );
}

#[test]
fn every_protocol_fault_of_an_mcp_server_ends_the_run_protocol_naming_it() {
    let bed = Bed::new();
    let init = |answer_line: &Value| answer(answer_line);
    let call = |answer_line: &Value| format!("{}\n{}", handshake(), answer(answer_line));
    let cases = [
        (
            "notjson",
            format!("{READ_LINE}\nprintf '%s\\n' 'not json'"),
            "message 1 is not a JSON-RPC 2.0 message as the runner expects: JSON syntax error at line 1 column 2",
        ),
        (
            "nojsonrpc",
            init(&json!({"id": 1, "result": {"protocolVersion": "2025-11-25"}})),
            "message 1 is not JSON-RPC 2.0: its jsonrpc is not \"2.0\"",
        ),
        (
            "otherid",
            init(&json!({"jsonrpc": "2.0", "id": 7, "result": {"protocolVersion": "2025-11-25"}})),
            "message 1 answers id 7, not the pending initialize 1",
        ),
        (
            "version",
            init(&initialized("2023-01-01")),
            "initialize answered protocol version \"2023-01-01\", which the runner does not speak",
        ),
        (
            "rpcerror",
            call(&json!({"jsonrpc": "2.0", "id": 2,
                "error": {"code": -32602, "message": "unknown tool"}})),
            "message 2 answers tools/call with JSON-RPC error -32602: unknown tool",
        ),
        (
            "nostructured",
            call(&json!({"jsonrpc": "2.0", "id": 2,
                "result": {"content": [{"type": "text", "text": "[]"}]}})),
            "page 1: the ulei_pull result has no structuredContent",
        ),
        (
            "notpage",
            call(&json!({"jsonrpc": "2.0", "id": 2,
                "result": {"structuredContent": {"items": [], "more": true}}})),
            "page 1: the structuredContent is not a C11 Page",
        ),
        (
            "baditem",
            call(
                &json!({"jsonrpc": "2.0", "id": 2, "result": {"structuredContent": {
                "items": [{"external_id": "x", "kind": "", "text": "t", "bytes_b64": ""}],
                "cursor": null, "more": false}}}),
            ),
            "page 1 item 1: item \"x\": kind is empty",
        ),
        (
            "longnumber",
            format!(
                "{}\n{READ_LINE}\nprintf '{{\"jsonrpc\":\"2.0\",\"id\":'\nhead -c 4096 /dev/zero | tr '\\0' 1\nprintf '}}\\n'",
                handshake()
            ),
            "message 2 has a number over 1024 bytes",
        ),
        (
            "duplicate",
            format!(
                "{}\n{READ_LINE}\nprintf '%s\\n' '{}'",
                handshake(),
                r#"{"jsonrpc":"2.0","id":2,"result":{"structuredContent":{"items":[{"external_id":"x","kind":"text/plain","text":"t","bytes_b64":"","kind":"text/html"}],"cursor":null,"more":false}}}"#
            ),
            "page 1 item 1: has the field kind twice",
        ),
        (
            "loops",
            format!(
                "{}\n{}\n{}",
                handshake(),
                answer(&page(2, &[], Some("c1"), true)),
                answer(&page(3, &[], Some("c1"), true))
            ),
            "page 2 has more: true with the cursor it was called with",
        ),
    ];
    let names: Vec<&str> = cases.iter().map(|(name, ..)| *name).collect();
    for (name, body, _) in &cases {
        bed.mcp(name, body);
    }
    bed.config(&names.join(","), "");

    for (name, _, reason) in &cases {
        let out = bed.run(name);
        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr(&out));
        assert!(
            stderr(&out).contains(&format!("{name} protocol: stored 0"))
                && stderr(&out).contains(&format!("; {reason}")),
            "{name}: {}",
            stderr(&out)
        );
        assert_eq!(bed.last().outcome, RunOutcome::Protocol);
    }
    assert_eq!(bed.cursor("loops").as_deref(), Some("c1"));
}

#[test]
fn a_two_mib_item_with_its_id_after_the_cut_is_too_large_and_its_page_goes_on() {
    let bed = Bed::new();
    let small = |id: &str| mcp_item(id).to_string();
    let big = |tail: &str| {
        format!(
            "printf '{{\"kind\":\"text/plain\",\"bytes_b64\":\"\",\"text\":\"'\nhead -c 2097152 /dev/zero | tr '\\0' a\nprintf '\"{tail}}}'"
        )
    };
    let body = format!(
        "{handshake}\n{READ_LINE}
printf '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"'
head -c 2097152 /dev/zero | tr '\\0' a
printf '\"}}],\"structuredContent\":{{\"items\":[{s1},'
{big}
printf ',{s2}],\"cursor\":\"c1\",\"more\":false}}}}}}\\n'",
        handshake = handshake(),
        s1 = small("small-1"),
        s2 = small("small-2"),
        big = big(",\"external_id\":\"big-1\""),
    );
    bed.mcp("big", &body);
    let noid = format!(
        "{handshake}\n{READ_LINE}
printf '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"structuredContent\":{{\"items\":['
{big}
printf '],\"cursor\":\"c1\",\"more\":false}}}}}}\\n'",
        handshake = handshake(),
        big = big(""),
    );
    bed.mcp("noid", &noid);
    bed.config("big,noid", "");
    let archive = bed.archive("big");

    let out = bed.run("big");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(archive.ids(), ["small-1", "small-2"]);
    let record = bed.last();
    assert_eq!(
        (record.stored, record.too_large, record.too_large_ids),
        (2, 1, vec!["big-1".to_owned()])
    );
    assert_eq!(bed.cursor("big").as_deref(), Some("c1"));

    let out = bed.run("noid");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains(
            "; page 1 item 1: is over 1052672 bytes and has no external_id, so no too_large id can be kept"
        ),
        "{}",
        stderr(&out)
    );
    assert_eq!(bed.last().outcome, RunOutcome::Protocol);
}

#[test]
fn blank_lines_between_mcp_messages_are_skipped() {
    let bed = Bed::new();
    let body = format!(
        "{READ_LINE}\nprintf '\\n \\r\\n\\n'\n{}\n{READ_LINE}\n{READ_LINE}\nprintf '\\n\\n'\n{}",
        say(&initialized("2025-11-25")),
        say(&page(2, &["m-1"], Some("c1"), false)),
    );
    bed.mcp("blank", &body);
    bed.config("blank", "");
    let archive = bed.archive("blank");

    let out = bed.run("blank");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(archive.ids(), ["m-1"]);
    assert_eq!(bed.cursor("blank").as_deref(), Some("c1"));
}

#[test]
fn an_mcp_server_that_ends_mid_session_names_the_answer_it_owed() {
    let bed = Bed::new();
    bed.mcp(
        "quits",
        &format!(
            "{}\n{}\n{READ_LINE}",
            handshake(),
            answer(&page(2, &["m-1"], Some("c1"), true))
        ),
    );
    bed.mcp("crashes", &format!("{READ_LINE}\nexit 3"));
    bed.config("quits,crashes", "");
    let archive = bed.archive("quits");

    let out = bed.run("quits");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("quits protocol: stored 1")
            && stderr(&out)
                .contains("; the MCP server ended its stdout before it answered ulei_pull page 2"),
        "{}",
        stderr(&out)
    );
    assert_eq!(archive.ids(), ["m-1"]);
    assert_eq!(bed.cursor("quits").as_deref(), Some("c1"));

    let out = bed.run("crashes");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("crashes exit: stored 0")
            && stderr(&out).contains(
                "; exit status: 3; the MCP server ended its stdout before it answered initialize"
            ),
        "{}",
        stderr(&out)
    );
    assert_eq!(bed.last().outcome, RunOutcome::Exit);
}

#[test]
fn an_item_past_the_limit_before_its_cursor_ends_the_run_protocol_undelivered() {
    let bed = Bed::new();
    let many: Vec<String> = items(1, 101);
    let ids: Vec<&str> = many.iter().map(String::as_str).collect();
    bed.mcp(
        "wide",
        &format!(
            "{}\n{}",
            handshake(),
            answer(&page(2, &ids, Some("c1"), false))
        ),
    );
    let line = |id: &str| {
        format!(
            "printf '%s\\n' '{}'",
            json!({"type": "item", "external_id": id, "kind": "text/plain", "text": "t",
                "bytes_b64": ""})
        )
    };
    let checkpoint = "printf '%s\\n' '{\"type\":\"checkpoint\",\"cursor\":\"100\"}'";
    let paged: Vec<String> = (1..=150)
        .map(|n| {
            let item = line(&format!("item-{n}"));
            if n == 100 {
                format!("{item}\n{checkpoint}")
            } else {
                item
            }
        })
        .collect();
    bed.script(
        "paged",
        &format!(
            "{}\nprintf '%s\\n' '{{\"type\":\"done\",\"cursor\":\"150\"}}'",
            paged.join("\n")
        ),
    );
    let unpaged: Vec<String> = (1..=101).map(|n| line(&format!("item-{n}"))).collect();
    bed.script(
        "unpaged",
        &format!(
            "{}\nprintf '%s\\n' '{{\"type\":\"done\"}}'",
            unpaged.join("\n")
        ),
    );
    bed.config("wide,paged,unpaged", "");
    let archive = bed.archive("wide");

    let out = bed.run("wide");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("wide protocol: stored 100")
            && stderr(&out).contains("; page 1 item 101 is over the request's limit of 100 items"),
        "{}",
        stderr(&out)
    );
    assert_eq!(archive.ids(), items(1, 100));
    assert_eq!(bed.cursor("wide"), None);

    archive.puts.lock().expect("puts").clear();
    let out = bed.run("paged");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(archive.ids(), items(1, 150));
    assert_eq!(bed.cursor("paged").as_deref(), Some("150"));

    archive.puts.lock().expect("puts").clear();
    let out = bed.run("unpaged");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("unpaged protocol: stored 100")
            && stderr(&out).contains(
                "; line 101: item 101 since the last checkpoint is over the request's limit of 100 items"
            ),
        "{}",
        stderr(&out)
    );
    assert_eq!(archive.ids(), items(1, 100));
}

#[test]
fn an_mcp_server_that_stops_answering_ends_at_the_manifest_timeout() {
    let bed = Bed::new();
    bed.mcp(
        "hangs",
        &format!("{}\n{READ_LINE}\nexec sleep 30", handshake()),
    );
    let mut manifest: Value = serde_json::from_slice(
        &fs::read(bed.dir().join("hangs").join("connector.json")).expect("manifest"),
    )
    .expect("manifest");
    manifest["timeout"] = json!(1);
    bed.manifest("hangs", &manifest);
    bed.config("hangs", "");

    let out = bed.run("hangs");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("hangs timeout: stored 0")
            && stderr(&out).contains("ran past its timeout of 1 s"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn the_mcp_example_puts_the_test_connectors_bodies_byte_for_byte_for_the_same_records() {
    let bodies = |transport: &str| {
        let bed = Bed::new();
        if transport == "mcp" {
            let bin = bed.dir().join("synthetic").join("bin");
            fs::create_dir_all(&bin).expect("bin dir");
            let example = bin.join("ulei-connector-mcp-example");
            symlink(built("ulei-connector-mcp-example"), &example).expect("symlink");
            let mut manifest = manifest("synthetic", "bin/ulei-connector-mcp-example");
            manifest["transport"] = json!("mcp");
            manifest["command"] = json!([
                "bin/ulei-connector-mcp-example",
                "--tool",
                "list_records",
                "--entries",
                "records",
                "--id",
                "id",
                "--text",
                "body",
                "--cursor",
                "seq",
                "--after",
                "after",
                "--limit",
                "limit",
                "--",
                built("ulei-connector-mcp-synthetic"),
                "--count",
                "100",
            ]);
            bed.manifest("synthetic", &manifest);
        } else {
            bed.test_connector("synthetic", None);
        }
        bed.config("synthetic", "");
        let archive = bed.archive("synthetic");
        let out = bed.run("synthetic");
        assert!(out.status.success(), "{transport}: {}", stderr(&out));
        assert_eq!(
            bed.cursor("synthetic").as_deref(),
            Some("100"),
            "{transport}"
        );
        let puts = archive.puts.lock().expect("puts");
        puts.iter().map(|put| put.raw.clone()).collect::<Vec<_>>()
    };
    let (stdio, mcp) = (bodies("stdio"), bodies("mcp"));
    assert_eq!(stdio.len(), 100);
    assert_eq!(
        String::from_utf8_lossy(&stdio[0]),
        r#"{"source":"synthetic","external_id":"item-1","kind":"text/plain","text":"synthetic item 1","bytes_b64":"c3ludGhldGljIGl0ZW0gMQ=="}"#
    );
    assert!(stdio == mcp, "the bodies differ");
}
