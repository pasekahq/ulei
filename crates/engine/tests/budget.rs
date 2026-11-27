//! The owner call budget of `ulei-engine serve` (ADR-0102) through the built binary, its turns on
//! the fake gateway with a budget of half a second: offload of a long `terminal` call into a
//! background job, the job's result handed to a fake owner inbox through C1 `pull` and C6
//! `job_results` and `jobs_done`, owner input during a long call, the model's terminal timeout,
//! a call stopped at the budget, the job's own limit, a restart while a job runs and the
//! parallel segment.

#![allow(missing_docs, clippy::unwrap_used)]

mod c6_wire;
mod fake_exa;
mod fake_gateway;
mod fake_mcp;

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use c6_wire::{Engine, config, ended, ok, post, start, submit};
use fake_exa::FakeExa;
use fake_gateway::{
    Answer, FakeGateway, Scratch, call, read_request, request, text_step, tool_step, turns,
};
use fake_mcp::{FakeMcp, Reply, Spec};
use serde_json::{Value, json};
use ulei_contracts::generated::c4g::BlockType;
use ulei_contracts::generated::c6::{JobOutcome, JobResults, TurnOutcome};

const HELPER: &str = env!("CARGO_BIN_EXE_ulei-web");
/// The budget every engine of these tests runs with.
const BUDGET: Duration = Duration::from_millis(500);
/// Room past the budget for a turn's start, its second model step and a loaded host.
const MARGIN: Duration = Duration::from_millis(1500);
const CHANNEL: &str = "tg";
const SESSION: &str = "chat";

/// A fake owner inbox: it answers C1 `pull` with `ok`, then takes the engine's ended jobs over
/// C6 `job_results`, keeps each as one event per job id and confirms them with `jobs_done`; it
/// also records the job id of every result it was handed, repeats included.
struct FakeInbox {
    events: Arc<Mutex<Vec<Value>>>,
    handed: Arc<Mutex<Vec<String>>>,
    pulls: Arc<Mutex<usize>>,
}

impl FakeInbox {
    fn start(socket: &Path, engine: PathBuf) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let handed = Arc::new(Mutex::new(Vec::new()));
        let pulls = Arc::new(Mutex::new(0));
        let (kept, seen, counted) = (Arc::clone(&events), Arc::clone(&handed), Arc::clone(&pulls));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                assert!(
                    String::from_utf8_lossy(&request.head).starts_with("POST /v1/pull "),
                    "{}",
                    String::from_utf8_lossy(&request.head)
                );
                *counted.lock().unwrap() += 1;
                let body = br#"{"ok":true}"#;
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.write_all(body);
                drop(stream);
                take_jobs(&engine, &kept, &seen);
            }
        });
        Self {
            events,
            handed,
            pulls,
        }
    }

    /// The events once there are `n`, waiting at most 15 s.
    fn wait_events(&self, n: usize) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let events = self.events.lock().unwrap().clone();
            if events.len() >= n {
                return events;
            }
            assert!(Instant::now() < deadline, "{} of {n} events", events.len());
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn events(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }
}

/// What the owner inbox does on a `pull`: each ended job of the engine at `engine` is recorded in
/// `handed` and becomes the event of its id unless one is kept already, then all are confirmed.
fn take_jobs(engine: &Path, events: &Mutex<Vec<Value>>, handed: &Mutex<Vec<String>>) {
    let page: JobResults =
        serde_json::from_slice(ok(&post(engine, "job_results", &json!({})))).unwrap();
    handed
        .lock()
        .unwrap()
        .extend(page.results.iter().map(|r| r.job.clone()));
    if page.results.is_empty() {
        return;
    }
    let mut kept = events.lock().unwrap();
    for result in &page.results {
        if !kept.iter().any(|event| event["job"] == result.job.as_str()) {
            kept.push(serde_json::to_value(result).unwrap());
        }
    }
    drop(kept);
    let ids: Vec<&str> = page.results.iter().map(|r| r.job.as_str()).collect();
    let acked = post(engine, "jobs_done", &json!({"jobs": ids}));
    assert_eq!(ok(&acked), br#"{"ok":true}"#);
}

/// A gateway with `script`, a fake inbox and an engine whose owner turns run on channel
/// [`CHANNEL`] with a budget of [`BUDGET`]; `extra` adds settings to `[engine]`.
fn setup(script: Vec<Answer>, extra: &str) -> (FakeGateway, Scratch, FakeInbox, Engine) {
    let fake = FakeGateway::start(script);
    let dir = Scratch::new();
    let (engine, inbox) = (dir.path("c6.sock"), dir.path("inbox.sock"));
    let path = settings(&dir, &fake, extra);
    let inbox = FakeInbox::start(&inbox, engine);
    let engine = start(&dir.0, &path);
    (fake, dir, inbox, engine)
}

/// The settings file of an engine in `dir` on `fake` with `extra` in `[engine]`.
fn settings(dir: &Scratch, fake: &FakeGateway, extra: &str) -> PathBuf {
    let path = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let mut text = fs::read_to_string(&path).unwrap();
    write!(
        text,
        "main_channel = \"{CHANNEL}\"\nowner_call_budget = \"{}\"\n{extra}\n[inbox]\nsocket = \"{}\"\n",
        BUDGET.as_secs_f64(),
        dir.path("inbox.sock").display(),
    )
    .unwrap();
    fs::write(&path, text).unwrap();
    path
}

/// Submits `prompt` to [`SESSION`], on the owner's channel when `owner`; its turn id.
fn turn(socket: &Path, prompt: &str, owner: bool) -> String {
    let mut body = json!({"session": SESSION, "prompt": prompt});
    if owner {
        body["channel"] = CHANNEL.into();
    }
    let answer = submit(socket, &body);
    assert!(answer.accepted);
    answer.turn.unwrap()
}

/// A step calling `terminal` with `input` as call `id`.
fn terminal(id: &str, input: Value) -> Answer {
    tool_step(vec![call(id, "terminal", input)])
}

/// The content and error flag of the result of call `id` in the `n`th turn request of `fake`.
fn result(fake: &FakeGateway, n: usize, id: &str) -> (Value, bool) {
    let sent = request(&turns(fake)[n]);
    let block = sent
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find(|b| b.r#type == BlockType::ToolResult && b.id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no result of {id}"));
    let content = block.content.clone().unwrap();
    let value = serde_json::from_str(&content).unwrap_or(Value::String(content));
    (value, block.is_error.unwrap())
}

/// Asserts that `elapsed` is within the budget and the margin.
fn within_budget(elapsed: Duration) {
    assert!(elapsed < BUDGET + MARGIN, "{elapsed:?}");
}

#[test]
fn a_call_past_the_budget_is_offloaded_and_its_result_reaches_the_inbox_once() {
    let (fake, _dir, inbox, engine) = setup(
        vec![
            terminal("t1", json!({"command": "sleep 3; echo built"})),
            text_step("It goes on in the background."),
        ],
        "",
    );
    let begun = Instant::now();
    let id = turn(&engine.socket, "build it", true);
    let report = ended(&engine.socket, SESSION, &id);
    within_budget(begun.elapsed());
    assert_eq!(report.outcome, Some(TurnOutcome::Completed));
    let (offloaded, is_error) = result(&fake, 1, "t1");
    assert!(!is_error, "{offloaded}");
    assert_eq!(offloaded["status"], "offloaded");
    assert_eq!(offloaded["tool"], "terminal");
    let job = offloaded["job_id"].as_str().unwrap().to_owned();
    assert_eq!(inbox.events(), Vec::<Value>::new());

    let events = inbox.wait_events(1);
    assert!(begun.elapsed() >= Duration::from_secs(3));
    assert_eq!(events[0]["job"], job.as_str());
    assert_eq!(events[0]["tool"], "terminal");
    assert_eq!(events[0]["outcome"], "ok");
    let output: Value = serde_json::from_str(events[0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(
        (&output["output"], &output["exit_code"]),
        (&json!("built"), &json!(0))
    );

    thread::sleep(Duration::from_millis(300));
    assert_eq!(inbox.events().len(), 1);
    assert_eq!(*inbox.handed.lock().unwrap(), std::slice::from_ref(&job));
    assert_eq!(*inbox.pulls.lock().unwrap(), 1);
    let left: JobResults =
        serde_json::from_slice(ok(&post(&engine.socket, "job_results", &json!({})))).unwrap();
    assert_eq!(left.results, Vec::new());
}

#[test]
fn an_owner_message_during_a_long_call_starts_its_turn_before_the_tool_ends() {
    let (fake, _dir, inbox, engine) = setup(
        vec![
            terminal("t1", json!({"command": "sleep 4"})),
            text_step("Still running."),
            text_step("Here is the other answer."),
        ],
        "",
    );
    let first = turn(&engine.socket, "run the long thing", true);
    thread::sleep(Duration::from_millis(100));
    let begun = Instant::now();
    let second = turn(&engine.socket, "and a quick question", true);
    let report = ended(&engine.socket, SESSION, &second);
    within_budget(begun.elapsed());
    assert_eq!(report.outcome, Some(TurnOutcome::Completed));
    assert_eq!(
        ended(&engine.socket, SESSION, &first).outcome,
        Some(TurnOutcome::Completed)
    );
    assert_eq!(turns(&fake).len(), 3);
    assert_eq!(inbox.events(), Vec::<Value>::new(), "the tool ended first");
}

#[test]
fn a_model_timeout_over_the_budget_does_not_extend_it_and_a_non_owner_turn_waits() {
    let (fake, _dir, _inbox, engine) = setup(
        vec![
            terminal(
                "t1",
                json!({"command": "sleep 2; echo done", "timeout": 30}),
            ),
            text_step("Offloaded."),
            terminal(
                "t2",
                json!({"command": "sleep 2; echo done", "timeout": 30}),
            ),
            text_step("Done."),
        ],
        "",
    );
    let begun = Instant::now();
    let owner = turn(&engine.socket, "owner asks", true);
    ended(&engine.socket, SESSION, &owner);
    within_budget(begun.elapsed());
    assert_eq!(result(&fake, 1, "t1").0["status"], "offloaded");

    let begun = Instant::now();
    let other = turn(&engine.socket, "another channel asks", false);
    ended(&engine.socket, SESSION, &other);
    assert!(begun.elapsed() >= Duration::from_secs(2));
    let (inline, is_error) = result(&fake, 3, "t2");
    assert!(!is_error);
    assert_eq!(
        inline,
        json!({"output": "done", "exit_code": 0, "error": null})
    );
}

#[test]
fn a_call_within_the_budget_returns_its_result_inline_unchanged() {
    let (fake, _dir, inbox, engine) = setup(
        vec![
            terminal("t1", json!({"command": "echo quick"})),
            text_step("Quick."),
            terminal("t2", json!({"command": "echo quick"})),
            text_step("Quick."),
        ],
        "",
    );
    let owner = turn(&engine.socket, "owner asks", true);
    ended(&engine.socket, SESSION, &owner);
    let other = turn(&engine.socket, "another channel asks", false);
    ended(&engine.socket, SESSION, &other);
    let (inline, is_error) = result(&fake, 1, "t1");
    assert!(!is_error);
    assert_eq!(inline, result(&fake, 3, "t2").0);
    assert_eq!(inline["output"], "quick");
    thread::sleep(Duration::from_millis(200));
    assert_eq!(inbox.events(), Vec::<Value>::new());
}

#[test]
fn a_call_that_cannot_be_offloaded_is_stopped_at_the_budget_with_a_timeout_result() {
    let mcp = FakeMcp::start(Spec::new(vec![json!({"name": "stuck"})]).reply("stuck", Reply::Hang));
    let extra = format!(
        "mcp_servers = \"files\"\n[engine.mcp.files]\nurl = \"{}\"\ntimeout = 60",
        mcp.url()
    );
    let (fake, _dir, inbox, engine) = setup(
        vec![
            tool_step(vec![call("m1", "mcp__files__stuck", json!({}))]),
            text_step("It did not finish."),
        ],
        &extra,
    );
    let begun = Instant::now();
    let id = turn(&engine.socket, "ask the server", true);
    ended(&engine.socket, SESSION, &id);
    within_budget(begun.elapsed());
    let (stopped, is_error) = result(&fake, 1, "m1");
    assert!(is_error);
    assert_eq!(stopped["status"], "timeout");
    assert_eq!(stopped["reason"], "stopped");
    assert_eq!(stopped["tool"], "mcp__files__stuck");
    assert_eq!(stopped["budget_s"], 0.5);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(inbox.events(), Vec::<Value>::new());
}

#[test]
fn a_job_past_its_own_limit_ends_with_a_failure_event() {
    let (fake, _dir, inbox, engine) = setup(
        vec![
            terminal("t1", json!({"command": "sleep 30", "timeout": 2})),
            text_step("Offloaded."),
        ],
        "",
    );
    let id = turn(&engine.socket, "wait long", true);
    ended(&engine.socket, SESSION, &id);
    let job = result(&fake, 1, "t1").0["job_id"].clone();
    let events = inbox.wait_events(1);
    assert_eq!(events[0]["job"], job);
    assert_eq!(events[0]["outcome"], "error");
    let output: Value = serde_json::from_str(events[0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(output["exit_code"], 124);
    assert!(
        output["output"]
            .as_str()
            .unwrap()
            .ends_with("[Command timed out after 2s]"),
        "{output}"
    );
}

#[test]
fn a_job_running_when_the_engine_stops_ends_lost_after_the_restart() {
    let (fake, dir, inbox, engine) = setup(
        vec![
            terminal("t1", json!({"command": "sleep 3"})),
            text_step("Offloaded."),
        ],
        "",
    );
    let id = turn(&engine.socket, "start it", true);
    ended(&engine.socket, SESSION, &id);
    let job = result(&fake, 1, "t1").0["job_id"].clone();
    drop(engine);
    assert_eq!(inbox.events(), Vec::<Value>::new());

    let _engine = start(&dir.0, &dir.path("ulei.toml"));
    let events = inbox.wait_events(1);
    assert_eq!(events[0]["job"], job);
    assert_eq!(
        serde_json::from_value::<JobOutcome>(events[0]["outcome"].clone()).unwrap(),
        JobOutcome::Lost
    );
}

#[test]
fn the_parallel_segment_of_an_owner_turn_ends_within_the_budget() {
    let exa = FakeExa::start(vec![fake_exa::Answer::Hang, fake_exa::Answer::Hang]);
    let dir = Scratch::new();
    let env_file = dir.path("exa.env");
    fs::write(&env_file, "EXA_API_KEY=synthetic-exa-key-for-tests\n").unwrap();
    let fake = FakeGateway::start(vec![
        tool_step(vec![
            call("w1", "web_search", json!({"query": "first"})),
            call("w2", "web_search", json!({"query": "second"})),
        ]),
        text_step("Neither finished."),
    ]);
    let extra = format!(
        "web_helper = \"{HELPER}\"\nweb_base_url = \"{}\"\nweb_env_file = \"{}\"\nweb_store = \"{}\"",
        exa.base_url(),
        env_file.display(),
        dir.path("web-store").display(),
    );
    let path = settings(&dir, &fake, &extra);
    let _inbox = FakeInbox::start(&dir.path("inbox.sock"), dir.path("c6.sock"));
    let engine = start(&dir.0, &path);
    let begun = Instant::now();
    let id = turn(&engine.socket, "search both", true);
    ended(&engine.socket, SESSION, &id);
    within_budget(begun.elapsed());
    for id in ["w1", "w2"] {
        let (stopped, is_error) = result(&fake, 1, id);
        assert!(is_error);
        assert_eq!(stopped["status"], "timeout", "{stopped}");
    }
}
