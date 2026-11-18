//! The tool loop against the scripted fake gateway: tool calls through the policy chain,
//! interrupts, the gateway error classes, the summary at the step limit and empty replies.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use fake_gateway::{
    Answer, FakeGateway, Recorder, STREAM_HEAD, SUMMARY_REQUEST, Scratch, call, chunk, empty_step,
    error_event, gateway_error, line, request, route, route_info, search_step, stop, text,
    text_block, text_step, thinking, tool_step, turns, usage,
};
use serde_json::{Map, Value, json};
use ulei_engine::agent::{
    Agent, DEFAULT_CONCURRENT_TOOL_TIMEOUT, Ended, MAX_OVERLOADED_RETRIES, Outcome, Template,
    text_message,
};
use ulei_engine::gateway::{Client, Error};
use ulei_engine::policy::{Chain, Policy, StopDecision, Verdict};
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{Context, Interrupt, Output, Registry, Tool};
use ulei_engine::wire::c4g::{
    self, Block, BlockType, CancelRequest, Error as GatewayError, ErrorClass, Message, Role,
    StopReason, ToolCall, TurnRequest,
};

const SLOW: Duration = Duration::from_secs(2);

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
    batch_timeout: Option<Duration>,
    empty_guard: bool,
    /// Tools registered after the built-in ones.
    extra: Vec<Box<dyn Tool>>,
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
            batch_timeout: Some(DEFAULT_CONCURRENT_TOOL_TIMEOUT),
            empty_guard: true,
            extra: Vec::new(),
        }
    }

    fn go(&mut self, fake: &FakeGateway) -> Outcome {
        let client = Client::new(fake.socket(), SLOW, self.idle);
        let mut tools = Registry::builtin(&self.cwd, Redactor { enabled: true });
        for tool in std::mem::take(&mut self.extra) {
            tools.register(tool).unwrap();
        }
        let template = template();
        let mut agent = Agent {
            client: &client,
            tools: &tools,
            chain: &mut self.chain,
            cwd: &self.cwd,
            template: &template,
            batch_timeout: self.batch_timeout,
            empty_guard: self.empty_guard,
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
        body.contains(r#"\"error\": \"File not found: missing.txt\"}","is_error":true"#),
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

const NUDGE: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.";

fn message(role: Role, content: Vec<Block>) -> Message {
    Message { role, content }
}

fn thinking_block(opaque: &str) -> Block {
    serde_json::from_value(json!({"type": "thinking", "vendor": "vendor-a", "opaque": opaque}))
        .unwrap()
}

/// The exact body of the summary request of `step` after the appended `messages`.
fn summary_body(step: usize, messages: &[Message]) -> Vec<u8> {
    let mut body = request(&expected_body(step, messages));
    body.tools = None;
    serde_json::to_vec(&body).unwrap()
}

#[test]
fn at_the_step_limit_a_request_without_tools_asks_for_the_summary_that_answers_the_turn() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        search_step("c1"),
        search_step("c2"),
        text_step("<think>plan</think>\n Found two files."),
        text_step("unreachable"),
    ]);
    let mut run = Run::new(&dir.0);
    run.max_steps = 2;
    let out = run.go(&fake);

    assert!(matches!(out.ended, Ended::MaxSteps), "{out:?}");
    let ids: Vec<&str> = out.steps.iter().map(|s| s.request_id.as_str()).collect();
    assert_eq!(ids, ["req-0", "req-1", "req-2"]);
    assert_eq!(out.messages.len(), 6);
    assert_eq!(
        out.messages[4..],
        [
            text_message(Role::User, SUMMARY_REQUEST),
            text_message(Role::Assistant, "Found two files."),
        ]
    );
    let sent = turns(&fake);
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[2], summary_body(2, &out.messages[..5]));
}

#[test]
fn a_failed_summary_request_answers_with_the_error_text() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        search_step("c1"),
        Answer::Reject(401, gateway_error(ErrorClass::Auth)),
        text_step("unreachable"),
    ]);
    let mut run = Run::new(&dir.0);
    run.max_steps = 1;
    let out = run.go(&fake);

    assert!(matches!(out.ended, Ended::MaxSteps), "{out:?}");
    assert_eq!(
        out.messages.last(),
        Some(&text_message(
            Role::Assistant,
            "I reached the maximum iterations (1) but couldn't summarize. Error: gateway \
                 error auth: no message"
        ))
    );
    let sent = turns(&fake);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1], summary_body(1, &out.messages[..3]));
}

#[test]
fn an_empty_summary_is_asked_for_once_more_then_answered_with_the_reference_text() {
    let no_summary = "I reached the iteration limit and couldn't generate a summary.";
    let cases = [
        (
            vec![empty_step(vec![]), text_step("Late summary.")],
            "Late summary.",
        ),
        (vec![empty_step(vec![]), empty_step(vec![])], no_summary),
        (vec![text_step("<think>only thought</think>")], no_summary),
    ];
    for (script, want) in cases {
        let tries = script.len();
        let dir = Scratch::new();
        let mut all = vec![search_step("c1")];
        all.extend(script);
        all.push(text_step("unreachable"));
        let fake = FakeGateway::start(all);
        let mut run = Run::new(&dir.0);
        run.max_steps = 1;
        let out = run.go(&fake);

        assert!(matches!(out.ended, Ended::MaxSteps), "{out:?}");
        assert_eq!(out.steps.len(), 1 + tries, "{want}");
        assert_eq!(
            out.messages[2..],
            [
                text_message(Role::User, SUMMARY_REQUEST),
                text_message(Role::Assistant, want),
            ]
        );
        let sent = turns(&fake);
        let want_sent: Vec<Vec<u8>> = (1..=tries)
            .map(|step| summary_body(step, &out.messages[..3]))
            .collect();
        assert_eq!(sent[1..], want_sent, "{want}");
    }
}

#[test]
fn an_empty_reply_is_retried_three_times_with_backoff_then_answers_empty() {
    let dir = Scratch::new();
    let mut script: Vec<Answer> = (0..4).map(|_| empty_step(vec![])).collect();
    script.push(text_step("unreachable"));
    let fake = FakeGateway::start(script);
    let mut run = Run::new(&dir.0);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(out.messages, [text_message(Role::Assistant, "(empty)")]);
    let sent = turns(&fake);
    let want: Vec<Vec<u8>> = (0..4).map(|step| expected_body(step, &[])).collect();
    assert_eq!(sent, want);
    let waits: Vec<f64> = run.sleeper.0.iter().map(Duration::as_secs_f64).collect();
    assert_eq!(waits.len(), 3, "{waits:?}");
    for (wait, base) in waits.iter().zip([5.0, 10.0, 20.0]) {
        assert!(*wait >= base && *wait <= base * 1.5, "{waits:?}");
    }
}

#[test]
fn a_reply_with_text_after_an_empty_one_answers_the_turn() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![empty_step(vec![text(" \n")]), text_step("Here.")]);
    let mut run = Run::new(&dir.0);
    let out = run.go(&fake);

    assert_eq!(out.messages, [text_message(Role::Assistant, "Here.")]);
    assert_eq!(turns(&fake), [expected_body(0, &[]), expected_body(1, &[])]);
    assert_eq!(run.sleeper.0.len(), 1);
}

#[test]
fn two_zero_output_empty_replies_of_one_route_end_the_retries_unless_the_guard_is_off() {
    for (guard, requests) in [(true, 2), (false, 4)] {
        let dir = Scratch::new();
        let mut script: Vec<Answer> = (0..4).map(|_| empty_step(vec![usage(0)])).collect();
        script.push(text_step("unreachable"));
        let fake = FakeGateway::start(script);
        let mut run = Run::new(&dir.0);
        run.empty_guard = guard;
        let out = run.go(&fake);

        assert_eq!(out.messages, [text_message(Role::Assistant, "(empty)")]);
        assert_eq!(turns(&fake).len(), requests, "guard {guard}");
        assert_eq!(run.sleeper.0.len(), requests - 1, "guard {guard}");
    }

    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        empty_step(vec![usage(0)]),
        empty_step(vec![usage(1)]),
        text_step("Here."),
    ]);
    let out = Run::new(&dir.0).go(&fake);
    assert_eq!(out.messages, [text_message(Role::Assistant, "Here.")]);
    assert_eq!(turns(&fake).len(), 3);
}

#[test]
fn an_empty_reply_to_tool_results_gets_one_nudge_kept_out_of_the_history() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        search_step("c1"),
        empty_step(vec![thinking("sig-1")]),
        empty_step(vec![]),
        text_step("Done."),
    ]);
    let mut run = Run::new(&dir.0);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(out.messages.len(), 3);
    assert_eq!(out.messages[2], text_message(Role::Assistant, "Done."));
    let mut nudged = out.messages[..2].to_vec();
    nudged.push(message(
        Role::Assistant,
        vec![thinking_block("sig-1"), text_block("(empty)")],
    ));
    nudged.push(text_message(Role::User, NUDGE));
    let sent = turns(&fake);
    assert_eq!(
        sent,
        [
            expected_body(0, &[]),
            expected_body(1, &out.messages[..2]),
            expected_body(2, &nudged),
            expected_body(3, &nudged),
        ]
    );
    assert_eq!(run.sleeper.0.len(), 1);
}

#[test]
fn inline_thinking_skips_the_nudge_and_thinking_only_replies_are_resent_without_them_twice() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        search_step("c1"),
        empty_step(vec![text("<think>still going")]),
        empty_step(vec![thinking("sig-2")]),
        empty_step(vec![thinking("sig-3")]),
        empty_step(vec![thinking("sig-4")]),
        text_step("Done."),
    ]);
    let mut run = Run::new(&dir.0);
    let out = run.go(&fake);

    assert_eq!(out.messages.len(), 3);
    assert_eq!(out.messages[2], text_message(Role::Assistant, "Done."));
    let tools = &out.messages[..2];
    let mut nudged = tools.to_vec();
    nudged.push(message(
        Role::Assistant,
        vec![thinking_block("sig-2"), text_block("(empty)")],
    ));
    nudged.push(text_message(Role::User, NUDGE));
    let sent = turns(&fake);
    assert_eq!(
        sent[1..],
        [
            expected_body(1, tools),
            expected_body(2, tools),
            expected_body(3, &nudged),
            expected_body(4, &nudged),
            expected_body(5, &nudged),
        ]
    );
    assert_eq!(run.sleeper.0.len(), 1);
}

#[test]
fn a_summary_after_a_prefill_leaves_it_out_and_merges_the_user_messages_left_adjacent() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        empty_step(vec![thinking("sig-1")]),
        text_step("Summary."),
    ]);
    let mut run = Run::new(&dir.0);
    run.max_steps = 1;
    let out = run.go(&fake);

    assert!(matches!(out.ended, Ended::MaxSteps), "{out:?}");
    assert_eq!(
        out.messages,
        [
            text_message(Role::User, SUMMARY_REQUEST),
            text_message(Role::Assistant, "Summary."),
        ]
    );
    let sent = turns(&fake);
    assert_eq!(sent[0], expected_body(0, &[]));
    let mut want = request(&expected_body(1, &[]));
    want.tools = None;
    want.messages[0].content.push(text_block(SUMMARY_REQUEST));
    assert_eq!(sent[1], serde_json::to_vec(&want).unwrap());
}

#[test]
fn an_exhausted_quota_on_the_summary_request_ends_the_turn_with_the_quota_line() {
    let dir = Scratch::new();
    let quota = GatewayError {
        resets_at: Some(1_794_924_000),
        ..gateway_error(ErrorClass::QuotaExhausted)
    };
    let fake = FakeGateway::start(vec![
        search_step("c1"),
        Answer::Reject(429, quota),
        text_step("unreachable"),
    ]);
    let mut run = Run::new(&dir.0);
    run.max_steps = 1;
    let out = run.go(&fake);

    match &out.ended {
        Ended::QuotaExhausted { line, .. } => assert_eq!(
            line,
            "quota exhausted on route chat; nearest reset 2026-11-17 14:00:00 UTC"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(out.messages.len(), 3);
    assert_eq!(out.messages[2], text_message(Role::User, SUMMARY_REQUEST));
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

/// A tool advertised under a parallel-safe name: each call waits until `want` calls are inside
/// at once and answers `<query> met`, or `<query> alone` after [`SLOW`].
struct Meet {
    name: &'static str,
    inside: Arc<(Mutex<usize>, Condvar)>,
    want: usize,
}

impl Meet {
    fn new(name: &'static str, want: usize) -> Self {
        Self {
            name,
            inside: Arc::default(),
            want,
        }
    }
}

impl Tool for Meet {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: self.name.into(),
            description: String::new(),
            input_schema: Map::new(),
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        let (count, arrived) = &*self.inside;
        let mut inside = count.lock().unwrap();
        *inside += 1;
        arrived.notify_all();
        let (inside, wait) = arrived
            .wait_timeout_while(inside, SLOW, |n| *n < self.want)
            .unwrap();
        drop(inside);
        let how = if wait.timed_out() { "alone" } else { "met" };
        Output::ok(format!("{} {how}", input["query"].as_str().unwrap()))
    }
}

/// The messages of the request of `step` after the history.
fn sent_messages(fake: &FakeGateway, step: usize) -> Vec<Message> {
    let body: TurnRequest = serde_json::from_slice(&turns(fake)[step]).unwrap();
    body.messages[history().len()..].to_vec()
}

#[test]
fn the_calls_of_a_parallel_segment_overlap_and_their_results_keep_call_order() {
    let dir = Scratch::new();
    fs::write(dir.0.join("a.txt"), "alpha\n").unwrap();
    fs::write(dir.0.join("b.txt"), "beta\n").unwrap();
    let fake = FakeGateway::start(vec![
        tool_step(vec![
            call("c1", "web_search", json!({"query": "q1"})),
            call("c2", "read_file", json!({"path": "a.txt"})),
            call("c3", "web_search", json!({"query": "q2"})),
            call("c4", "read_file", json!({"path": "b.txt"})),
            call(
                "c5",
                "write_file",
                json!({"path": "c.txt", "content": "gamma\n"}),
            ),
            call("c6", "read_file", json!({"path": "c.txt"})),
        ]),
        text_step("Done."),
    ]);
    let mut run = Run::new(&dir.0);
    run.extra.push(Box::new(Meet::new("web_search", 2)));
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    let got = results(&out.messages[1]);
    let ids: Vec<&str> = got.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(ids, ["c1", "c2", "c3", "c4", "c5", "c6"]);
    assert!(got.iter().all(|r| !r.1), "{got:?}");
    assert_eq!(got[0].2, "q1 met");
    assert!(got[1].2.contains("1|alpha"), "{:?}", got[1]);
    assert_eq!(got[2].2, "q2 met");
    assert!(got[3].2.contains("1|beta"), "{:?}", got[3]);
    assert!(got[4].2.contains("\"bytes_written\": 6"), "{:?}", got[4]);
    assert!(got[5].2.contains("1|gamma"), "{:?}", got[5]);
    assert_eq!(sent_messages(&fake, 1), out.messages[..2]);
}

/// A tool advertised under a parallel-safe name that answers after `delay`.
struct Slow {
    delay: Duration,
}

impl Tool for Slow {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: "web_extract".into(),
            description: String::new(),
            input_schema: Map::new(),
        }
    }

    fn run(&self, _input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        thread::sleep(self.delay);
        Output::ok("late")
    }
}

#[test]
fn a_call_past_the_batch_deadline_gets_the_timeout_text_and_the_others_their_results() {
    let dir = Scratch::new();
    fs::write(dir.0.join("a.txt"), "alpha\n").unwrap();
    let fake = FakeGateway::start(vec![
        tool_step(vec![
            call(
                "c1",
                "web_extract",
                json!({"urls": ["https://example.com"]}),
            ),
            call("c2", "read_file", json!({"path": "a.txt"})),
        ]),
        text_step("Done."),
    ]);
    let mut run = Run::new(&dir.0);
    run.batch_timeout = Some(Duration::from_millis(100));
    run.extra.push(Box::new(Slow {
        delay: Duration::from_millis(300),
    }));
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    let got = results(&out.messages[1]);
    assert_eq!(
        got[0],
        (
            "c1".to_owned(),
            true,
            "Error executing tool 'web_extract': timed out after 0.1s".to_owned()
        )
    );
    assert_eq!((got[1].0.as_str(), got[1].1), ("c2", false));
    assert!(got[1].2.contains("1|alpha"), "{:?}", got[1]);
    assert_eq!(sent_messages(&fake, 1), out.messages[..2]);
}

#[test]
fn repeated_reads_and_searches_in_one_parallel_batch_count_as_one_after_another() {
    let dir = Scratch::new();
    fs::write(dir.0.join("f.txt"), "needle\n").unwrap();
    let read = |id: &str| call(id, "read_file", json!({"path": "f.txt"}));
    let search = |id: &str| call(id, "search_files", json!({"pattern": "needle"}));
    let fake = FakeGateway::start(vec![
        tool_step(vec![read("r1")]),
        tool_step(vec![read("r2"), read("r3"), read("r4")]),
        tool_step(vec![search("s1"), search("s2"), search("s3"), search("s4")]),
        text_step("Done."),
    ]);
    let out = Run::new(&dir.0).go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    let first = results(&out.messages[1]);
    assert!(first[0].2.contains("1|needle"), "{first:?}");

    // One stub, then BLOCKED for the third and fourth read, whichever call drew which count.
    let mut reads: Vec<(bool, String)> = results(&out.messages[3])
        .into_iter()
        .map(|(_, is_error, content)| (is_error, content))
        .collect();
    reads.sort();
    assert!(
        !reads[0].0 && reads[0].1.contains("\"status\": \"unchanged\""),
        "{reads:?}"
    );
    for (read, times) in reads[1..].iter().zip([3, 4]) {
        assert!(read.0, "{read:?}");
        assert!(
            read.1.contains(&format!(
                "exact region {times} times and the file has NOT changed"
            )),
            "{read:?}"
        );
    }

    // Counts 1 to 4: two plain answers, the warning of the third, BLOCKED for the fourth.
    let searches = results(&out.messages[5]);
    let warned = searches.iter().filter(|s| s.2.contains("_warning")).count();
    let blocked: Vec<_> = searches.iter().filter(|s| s.1).collect();
    assert_eq!(warned, 1, "{searches:?}");
    assert_eq!(blocked.len(), 1, "{searches:?}");
    assert!(
        blocked[0].2.contains("\"already_searched\": 4"),
        "{searches:?}"
    );
}
