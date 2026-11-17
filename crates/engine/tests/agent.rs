//! The tool loop against the scripted fake gateway: tool calls through the policy chain,
//! interrupts, the gateway error classes and the step bound.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use fake_gateway::{
    Answer, FakeGateway, STREAM_HEAD, Scratch, call, chunk, error_event, gateway_error, line,
    route, route_info, stop, text, text_block, text_step, thinking, tool_step, turns, usage,
};
use serde_json::json;
use ulei_engine::agent::{Agent, Ended, MAX_OVERLOADED_RETRIES, Outcome, Sleeper, Template};
use ulei_engine::gateway::{Client, Error};
use ulei_engine::policy::{Chain, Policy, StopDecision, Verdict};
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{Interrupt, Registry};
use ulei_engine::wire::c4g::{
    Block, BlockType, CancelRequest, Error as GatewayError, ErrorClass, Message, Role, StopReason,
    ToolCall, TurnRequest,
};

const SLOW: Duration = Duration::from_secs(2);

/// Records every wait instead of sleeping.
#[derive(Default)]
struct Recorder(Vec<Duration>);

impl Sleeper for Recorder {
    fn sleep(&mut self, duration: Duration) {
        self.0.push(duration);
    }
}

/// Denies calls of one tool name.
struct DenyTool(&'static str);

impl Policy for DenyTool {
    fn name(&self) -> &'static str {
        "deny-tool"
    }

    fn pre_tool(&mut self, call: &ToolCall, _cwd: &Path) -> Verdict {
        if call.name == self.0 {
            Verdict::Deny("not this tool".into())
        } else {
            Verdict::Allow
        }
    }
}

/// Continues the first `left` turn ends with a fixed message.
struct Nudge {
    left: usize,
}

impl Policy for Nudge {
    fn name(&self) -> &'static str {
        "nudge"
    }

    fn on_stop(&mut self, _turn: &Message) -> StopDecision {
        if self.left == 0 {
            return StopDecision::Stop;
        }
        self.left -= 1;
        StopDecision::Continue("keep going".into())
    }
}

fn template() -> Template {
    Template {
        route: "chat".into(),
        system: Some("You are a test.".into()),
        max_tokens: 1024,
        effort: None,
        session_key: Some("session-1".into()),
    }
}

fn request_id(step: usize) -> String {
    format!("req-{step}")
}

fn history() -> Vec<Message> {
    vec![Message {
        role: Role::User,
        content: vec![text_block("Start.")],
    }]
}

struct Run {
    cwd: PathBuf,
    chain: Chain,
    interrupt: Interrupt,
    max_steps: usize,
    sleeper: Recorder,
    idle: Duration,
}

impl Run {
    fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_owned(),
            chain: Chain::default(),
            interrupt: Interrupt::new(),
            max_steps: 16,
            sleeper: Recorder::default(),
            idle: SLOW,
        }
    }

    fn go(&mut self, fake: &FakeGateway) -> Outcome {
        let client = Client::new(fake.socket(), SLOW, self.idle);
        let tools = Registry::builtin(&self.cwd, Redactor { enabled: true });
        let template = template();
        let mut agent = Agent {
            client: &client,
            tools: &tools,
            chain: &mut self.chain,
            cwd: &self.cwd,
            template: &template,
        };
        agent.run(
            &history(),
            &request_id,
            &self.interrupt,
            self.max_steps,
            &mut self.sleeper,
        )
    }
}

/// `request_id`s of the recorded cancel requests.
fn cancels(fake: &FakeGateway) -> Vec<String> {
    fake.requests()
        .into_iter()
        .filter(|r| r.target() == "POST /v1/model/cancel")
        .map(|r| {
            serde_json::from_slice::<CancelRequest>(&r.body)
                .unwrap()
                .request_id
        })
        .collect()
}

/// The exact body of step `step` after the appended `messages`.
fn expected_body(step: usize, messages: &[Message]) -> Vec<u8> {
    let template = template();
    let mut all = history();
    all.extend_from_slice(messages);
    serde_json::to_vec(&TurnRequest {
        route: template.route,
        system: template.system,
        messages: all,
        tools: Some(
            Registry::builtin(Path::new("spill"), Redactor { enabled: true }).definitions(),
        ),
        max_tokens: template.max_tokens,
        effort: template.effort,
        session_key: template.session_key,
        cache: None,
        request_id: request_id(step),
    })
    .unwrap()
}

fn results(message: &Message) -> Vec<(String, bool, String)> {
    assert_eq!(message.role, Role::User);
    message
        .content
        .iter()
        .map(|b| {
            assert_eq!(b.r#type, BlockType::ToolResult, "{b:?}");
            (
                b.id.clone().unwrap(),
                b.is_error.unwrap(),
                b.content.clone().unwrap(),
            )
        })
        .collect()
}

/// The process is gone or a zombie waiting for its reaper.
fn dead(pid: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"));
        let gone = stat.as_ref().map_or(true, |s| {
            s.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        });
        if gone || Instant::now() > deadline {
            return gone;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_text_turn_appends_one_assistant_message_in_stream_order() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![Answer::Events(vec![
        route(),
        thinking("sig-1"),
        text("Hel"),
        text("lo."),
        usage(2),
        stop(StopReason::End),
    ])]);
    let out = Run::new(&dir.0).go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    let thinking: Block = serde_json::from_value(
        json!({"type": "thinking", "vendor": "vendor-a", "opaque": "sig-1"}),
    )
    .unwrap();
    assert_eq!(
        out.messages,
        vec![Message {
            role: Role::Assistant,
            content: vec![thinking, text_block("Hello.")],
        }]
    );
    assert_eq!(out.steps.len(), 1);
    assert_eq!(out.steps[0].request_id, "req-0");
    assert_eq!(out.steps[0].route, Some(route_info()));
    assert_eq!(out.steps[0].usage.as_ref().map(|u| u.output), Some(2));
    assert_eq!(turns(&fake), vec![expected_body(0, &[])]);
}

#[test]
fn a_chain_of_builtin_tool_calls_runs_in_order_and_feeds_each_result_back() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        tool_step(vec![call(
            "c1",
            "write_file",
            json!({"path": "notes.txt", "content": "alpha\n"}),
        )]),
        tool_step(vec![
            call("c2", "read_file", json!({"path": "notes.txt"})),
            call(
                "c3",
                "patch",
                json!({"path": "notes.txt", "old_string": "alpha", "new_string": "beta"}),
            ),
        ]),
        tool_step(vec![call("c4", "search_files", json!({"pattern": "beta"}))]),
        tool_step(vec![call(
            "c5",
            "search_files",
            json!({"pattern": "*.txt", "target": "files"}),
        )]),
        tool_step(vec![call(
            "c6",
            "terminal",
            json!({"command": "cat notes.txt"}),
        )]),
        text_step("Done."),
    ]);
    let out = Run::new(&dir.0).go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.0.join("notes.txt")).unwrap(),
        "beta\n"
    );
    assert_eq!(out.steps.len(), 6);
    assert_eq!(out.messages.len(), 11);
    let roles: Vec<Role> = out.messages.iter().map(|m| m.role).collect();
    for (i, role) in roles.iter().enumerate() {
        let want = if i % 2 == 0 {
            Role::Assistant
        } else {
            Role::User
        };
        assert_eq!(*role, want, "message {i}");
    }
    let mut ids = Vec::new();
    for pair in out.messages[..10].chunks(2) {
        let uses: Vec<String> = pair[0]
            .content
            .iter()
            .map(|b| {
                assert_eq!(b.r#type, BlockType::ToolUse, "{b:?}");
                b.id.clone().unwrap()
            })
            .collect();
        let got = results(&pair[1]);
        assert_eq!(
            got.iter().map(|r| r.0.clone()).collect::<Vec<_>>(),
            uses,
            "each tool_use has its tool_result"
        );
        for (id, is_error, content) in got {
            assert!(!is_error, "{id}: {content}");
            ids.push((id, content));
        }
    }
    assert_eq!(
        ids.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["c1", "c2", "c3", "c4", "c5", "c6"]
    );
    assert!(ids[1].1.contains("alpha"), "{:?}", ids[1]);
    assert!(ids[3].1.contains("beta"), "{:?}", ids[3]);
    assert!(ids[4].1.contains("notes.txt"), "{:?}", ids[4]);
    assert!(ids[5].1.contains("beta"), "{:?}", ids[5]);

    let sent = turns(&fake);
    assert_eq!(sent.len(), 6);
    for (step, body) in sent.iter().enumerate() {
        assert_eq!(
            *body,
            expected_body(step, &out.messages[..2 * step]),
            "step {step}"
        );
    }
}

#[test]
fn failing_and_denied_tools_reach_the_next_request_as_is_error() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        tool_step(vec![
            call("c1", "read_file", json!({"path": "missing.txt"})),
            call("c2", "terminal", json!({"command": "touch ran"})),
        ]),
        text_step("Noted."),
    ]);
    let mut run = Run::new(&dir.0);
    run.chain = Chain::new(vec![Box::new(DenyTool("terminal"))]);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert!(!dir.0.join("ran").exists());
    let got = results(&out.messages[1]);
    assert_eq!((got[0].0.as_str(), got[0].1), ("c1", true));
    assert_eq!((got[1].0.as_str(), got[1].1), ("c2", true));
    assert!(got[1].2.contains("denied by policy deny-tool"), "{got:?}");

    let sent = turns(&fake);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1], expected_body(1, &out.messages[..2]));
    let body = String::from_utf8(sent[1].clone()).unwrap();
    let denied = r#"{"type":"tool_result","id":"c2","content":"{\"error\": \"denied by policy deny-tool: not this tool\"}","is_error":true}"#;
    assert!(body.contains(denied), "{body}");
    assert!(
        body.contains(r#""id":"c1","content":"{\"error\": \"cannot read "#),
        "{body}"
    );
}

#[test]
fn an_interrupt_during_a_stream_cancels_the_gateway_turn() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![Answer::Hang(vec![route(), text("partial")])]);
    let mut run = Run::new(&dir.0);
    let remote = run.interrupt.clone();
    let trigger = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        remote.interrupt();
    });
    let started = Instant::now();
    let out = run.go(&fake);
    let elapsed = started.elapsed();
    trigger.join().unwrap();

    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    assert!(matches!(out.ended, Ended::Interrupted), "{out:?}");
    assert!(out.messages.is_empty(), "{out:?}");
    assert_eq!(turns(&fake).len(), 1);
    assert_eq!(cancels(&fake), ["req-0"]);
}

#[test]
fn an_interrupt_during_a_terminal_tool_kills_its_group_and_closes_every_call() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        tool_step(vec![
            call(
                "c1",
                "terminal",
                json!({"command": "(sleep 30 & echo $! > pid; wait) & wait"}),
            ),
            call(
                "c2",
                "write_file",
                json!({"path": "after.txt", "content": "x"}),
            ),
        ]),
        text_step("unreachable"),
    ]);
    let mut run = Run::new(&dir.0);
    let remote = run.interrupt.clone();
    let pid_file = dir.0.join("pid");
    let trigger = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !fs::read_to_string(&pid_file).is_ok_and(|s| s.ends_with('\n'))
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        remote.interrupt();
    });
    let started = Instant::now();
    let out = run.go(&fake);
    let elapsed = started.elapsed();
    trigger.join().unwrap();

    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    assert!(matches!(out.ended, Ended::Interrupted), "{out:?}");
    assert_eq!(out.messages.len(), 2);
    let got = results(&out.messages[1]);
    assert_eq!(
        got.iter().map(|r| (r.0.as_str(), r.1)).collect::<Vec<_>>(),
        [("c1", true), ("c2", true)]
    );
    assert!(got[0].2.contains("interrupted"), "{got:?}");
    assert!(!dir.0.join("after.txt").exists());
    assert!(dead(fs::read_to_string(dir.0.join("pid")).unwrap().trim()));
    assert_eq!(turns(&fake).len(), 1);
}

#[test]
fn quota_exhausted_sends_one_request_and_names_the_route_and_reset() {
    let quota = |resets_at| GatewayError {
        resets_at: Some(resets_at),
        ..gateway_error(ErrorClass::QuotaExhausted)
    };
    let cases = [
        (
            Answer::Events(vec![route(), error_event(quota(1_794_924_000))]),
            "quota exhausted on route chat; nearest reset 2026-11-17 14:00:00 UTC",
        ),
        (
            Answer::Reject(429, quota(1_835_481_599)),
            "quota exhausted on route chat; nearest reset 2028-02-29 23:59:59 UTC",
        ),
    ];
    for (answer, want) in cases {
        let dir = Scratch::new();
        let fake = FakeGateway::start(vec![answer, text_step("unreachable")]);
        let mut run = Run::new(&dir.0);
        let out = run.go(&fake);

        match &out.ended {
            Ended::QuotaExhausted { line, error } => {
                assert_eq!(line, want);
                assert!(!line.contains('\n'));
                assert_eq!(error.class, ErrorClass::QuotaExhausted);
            }
            other => panic!("{other:?}"),
        }
        assert!(out.messages.is_empty(), "{out:?}");
        assert_eq!(turns(&fake).len(), 1);
        assert!(run.sleeper.0.is_empty(), "{:?}", run.sleeper.0);
    }
}

#[test]
fn overloaded_is_retried_after_retry_after_with_the_same_request() {
    let dir = Scratch::new();
    let overloaded = GatewayError {
        retryable: true,
        retry_after: Some(7),
        ..gateway_error(ErrorClass::Overloaded)
    };
    let fake = FakeGateway::start(vec![
        Answer::Reject(503, overloaded.clone()),
        Answer::Events(vec![route(), text("par"), error_event(overloaded)]),
        text_step("Fine."),
    ]);
    let mut run = Run::new(&dir.0);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(
        out.messages,
        vec![Message {
            role: Role::Assistant,
            content: vec![text_block("Fine.")],
        }]
    );
    assert_eq!(run.sleeper.0, [Duration::from_secs(7); 2]);
    assert_eq!(out.steps.len(), 1);
    let sent = turns(&fake);
    assert_eq!(sent, vec![expected_body(0, &[]); 3]);
}

#[test]
fn overloaded_past_the_retry_bound_ends_with_the_gateway_error() {
    let dir = Scratch::new();
    let overloaded = GatewayError {
        retryable: true,
        retry_after: Some(1),
        ..gateway_error(ErrorClass::Overloaded)
    };
    let tries = MAX_OVERLOADED_RETRIES as usize + 1;
    let mut script: Vec<Answer> = (0..tries)
        .map(|_| Answer::Events(vec![route(), error_event(overloaded.clone())]))
        .collect();
    script.push(text_step("unreachable"));
    let fake = FakeGateway::start(script);
    let mut run = Run::new(&dir.0);
    let out = run.go(&fake);

    match &out.ended {
        Ended::Gateway(error) => assert_eq!(error.class, ErrorClass::Overloaded),
        other => panic!("{other:?}"),
    }
    assert_eq!(turns(&fake).len(), tries);
    assert_eq!(run.sleeper.0.len(), tries - 1);
}

#[test]
fn other_error_classes_cut_and_timeout_end_without_a_retry() {
    let dir = Scratch::new();

    let fake = FakeGateway::start(vec![
        Answer::Reject(401, gateway_error(ErrorClass::Auth)),
        text_step("unreachable"),
    ]);
    let out = Run::new(&dir.0).go(&fake);
    assert!(
        matches!(&out.ended, Ended::Gateway(e) if e.class == ErrorClass::Auth),
        "{out:?}"
    );
    assert_eq!(turns(&fake).len(), 1);

    let fake = FakeGateway::start(vec![
        Answer::Events(vec![
            route(),
            error_event(gateway_error(ErrorClass::ContextTooLong)),
        ]),
        text_step("unreachable"),
    ]);
    let out = Run::new(&dir.0).go(&fake);
    assert!(
        matches!(&out.ended, Ended::Gateway(e) if e.class == ErrorClass::ContextTooLong),
        "{out:?}"
    );
    assert_eq!(turns(&fake).len(), 1);

    let cut = [
        STREAM_HEAD.to_vec(),
        chunk(&line(&route())),
        b"0\r\n\r\n".to_vec(),
    ];
    let fake = FakeGateway::start(vec![Answer::Raw(cut.concat()), text_step("unreachable")]);
    let out = Run::new(&dir.0).go(&fake);
    assert!(matches!(out.ended, Ended::Transport(Error::Cut)), "{out:?}");
    assert_eq!(turns(&fake).len(), 1);

    let fake = FakeGateway::start(vec![Answer::Hang(vec![route()]), text_step("unreachable")]);
    let mut run = Run::new(&dir.0);
    run.idle = Duration::from_millis(30);
    let out = run.go(&fake);
    assert!(
        matches!(out.ended, Ended::Transport(Error::Timeout)),
        "{out:?}"
    );
    assert_eq!(turns(&fake).len(), 1);
}

#[test]
fn the_max_steps_bound_ends_the_turn_without_another_request() {
    let dir = Scratch::new();
    let step = || {
        tool_step(vec![call(
            "c",
            "search_files",
            json!({"pattern": "*", "target": "files"}),
        )])
    };
    let fake = FakeGateway::start(vec![step(), step(), step()]);
    let mut run = Run::new(&dir.0);
    run.max_steps = 2;
    let out = run.go(&fake);

    assert!(matches!(out.ended, Ended::MaxSteps), "{out:?}");
    assert_eq!(out.steps.len(), 2);
    assert_eq!(out.messages.len(), 4);
    assert_eq!(results(&out.messages[3]).len(), 1);
    assert_eq!(turns(&fake).len(), 2);
}

#[test]
fn an_on_stop_continue_sends_its_message_and_runs_another_step() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![text_step("First."), text_step("Second.")]);
    let mut run = Run::new(&dir.0);
    run.chain = Chain::new(vec![Box::new(Nudge { left: 1 })]);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    let assistant = |t: &str| Message {
        role: Role::Assistant,
        content: vec![text_block(t)],
    };
    assert_eq!(
        out.messages,
        vec![
            assistant("First."),
            Message {
                role: Role::User,
                content: vec![text_block("keep going")],
            },
            assistant("Second."),
        ]
    );
    let sent = turns(&fake);
    assert_eq!(
        sent,
        vec![expected_body(0, &[]), expected_body(1, &out.messages[..2])]
    );
}
