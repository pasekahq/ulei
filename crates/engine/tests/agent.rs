//! The tool loop against the scripted fake gateway: tool calls through the policy chain,
//! interrupts, the gateway error classes, the summary at the step limit, empty replies, tool-use
//! stops without a call and the `no-ask-guard` and `tool-guardrails` policies.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use fake_gateway::{
    Answer, FakeGateway, InterruptMidWait, Recorder, STREAM_HEAD, SUMMARY_REQUEST, Scratch, call,
    cancels, chunk, dead, empty_step, error_event, gateway_error, json_bytes, line, request, route,
    route_info, search_step, stop, text, text_block, text_step, thinking, tool_step, turns, usage,
};
use serde_json::{Map, Value, json};
use ulei_engine::agent::{
    Agent, DEFAULT_CONCURRENT_TOOL_TIMEOUT, Ended, MAX_OVERLOADED_RETRIES, Outcome, Sleeper,
    Template, text_message,
};
use ulei_engine::gateway::{Client, Error};
use ulei_engine::policy::{
    Chain, Classifier, GuardrailConfig, INSTRUCTION_FIELDS, NOTE_FIELDS, NoAskGuard, Origin,
    Policy, StopDecision, TextTemplate, ToolGuardrails, TurnReminder, Verdict, ends_with_question,
};
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{Context, Interrupt, Output, Registry, Tool};
use ulei_engine::trace::Trace;
use ulei_engine::wire::c4g::{
    self, Block, BlockType, Error as GatewayError, ErrorClass, Message, Role, StopReason, ToolCall,
    TurnRequest,
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

    fn on_stop(&mut self, _turn: &Message, _interrupt: &Interrupt) -> StopDecision {
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
        cache: None,
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
    history: Vec<Message>,
    chain: Chain,
    interrupt: Interrupt,
    max_steps: usize,
    sleeper: Recorder,
    idle: Duration,
    batch_timeout: Option<Duration>,
    empty_guard: bool,
    origin: Origin,
    /// Tools registered after the built-in ones.
    extra: Vec<Box<dyn Tool>>,
    /// Blocks attached after the input in every request.
    images: Vec<Block>,
    /// The debug trace of the turn.
    trace: Option<Trace>,
}

impl Run {
    fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_owned(),
            history: history(),
            chain: Chain::default(),
            interrupt: Interrupt::new(),
            max_steps: 16,
            sleeper: Recorder::default(),
            idle: SLOW,
            batch_timeout: Some(DEFAULT_CONCURRENT_TOOL_TIMEOUT),
            empty_guard: true,
            origin: Origin::Other,
            extra: Vec::new(),
            images: Vec::new(),
            trace: None,
        }
    }

    fn go(&mut self, fake: &FakeGateway) -> Outcome {
        let mut recorder = std::mem::take(&mut self.sleeper);
        let out = self.go_waiting(fake, &mut recorder);
        self.sleeper = recorder;
        out
    }

    /// As [`Run::go`], waiting through `sleeper`.
    fn go_waiting(&mut self, fake: &FakeGateway, sleeper: &mut dyn Sleeper) -> Outcome {
        let client = Client::new(fake.socket(), SLOW, self.idle);
        let mut tools = Registry::builtin(
            &self.cwd,
            Redactor { enabled: true },
            None,
            &fake_gateway::skills(&self.cwd),
            None,
        );
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
            origin: self.origin,
            compaction: None,
            context: None,
            images: &self.images,
            interim: None,
            trace: self.trace.as_ref(),
        };
        agent.run(
            &self.history,
            &request_id,
            &self.interrupt,
            self.max_steps,
            sleeper,
        )
    }
}

/// The exact body of step `step` after the appended `messages`.
fn expected_body(step: usize, messages: &[Message]) -> Vec<u8> {
    expected_body_after(history(), step, messages)
}

/// The exact body of step `step` sending `sent` as the history, then `messages`.
fn expected_body_after(sent: Vec<Message>, step: usize, messages: &[Message]) -> Vec<u8> {
    let template = template();
    let mut all = sent;
    all.extend_from_slice(messages);
    serde_json::to_vec(&TurnRequest {
        route: template.route,
        system: template.system,
        messages: all,
        tools: Some(
            Registry::builtin(
                Path::new("spill"),
                Redactor { enabled: true },
                None,
                &fake_gateway::skills(Path::new("spill")),
                None,
            )
            .definitions(),
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
fn overloaded_answered_from_the_head_of_a_large_request_is_retried() {
    let dir = Scratch::new();
    let overloaded = GatewayError {
        retry_after: Some(7),
        ..gateway_error(ErrorClass::Overloaded)
    };
    let fake = FakeGateway::start(vec![text_step("Fine.")]);
    fake.answer_after_head(json_bytes(429, &serde_json::to_vec(&overloaded).unwrap()));
    let mut run = Run::new(&dir.0);
    run.history = vec![text_message(Role::User, &"x".repeat(8 << 20))];
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(run.sleeper.0, [Duration::from_secs(7)]);
    let sent = turns(&fake);
    assert_eq!(sent.len(), 2);
    assert_eq!(request(&sent[1]).messages, run.history);
}

#[test]
fn an_interrupt_during_the_overloaded_wait_ends_the_turn_at_once() {
    let dir = Scratch::new();
    let overloaded = GatewayError {
        retry_after: Some(30),
        ..gateway_error(ErrorClass::Overloaded)
    };
    let fake = FakeGateway::start(vec![
        Answer::Reject(503, overloaded),
        text_step("unreachable"),
    ]);
    let mut waits = InterruptMidWait::default();
    let out = Run::new(&dir.0).go_waiting(&fake, &mut waits);

    assert!(matches!(out.ended, Ended::Interrupted), "{out:?}");
    let (asked, took) = waits.0[0];
    assert_eq!((asked, waits.0.len()), (Duration::from_secs(30), 1));
    assert!(took < Duration::from_secs(1), "{took:?}");
    assert_eq!(turns(&fake).len(), 1);
}

#[test]
fn overloaded_past_the_retry_bound_ends_with_the_gateway_error() {
    let dir = Scratch::new();
    let overloaded = GatewayError {
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

/// Gives a fixed turn context for every origin.
struct Note(&'static str);

impl Policy for Note {
    fn name(&self) -> &'static str {
        "note"
    }

    fn turn_context(&self, _origin: Origin) -> &str {
        self.0
    }
}

#[test]
fn the_trace_holds_the_turn_context_requests_and_tool_calls() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        tool_step(vec![call(
            "c1",
            "write_file",
            json!({"path": "a.txt", "content": "a"}),
        )]),
        text_step("Done."),
    ]);
    let path = dir.0.join("trace.ndjson");
    let mut run = Run::new(&dir.0);
    run.origin = Origin::TopLevelOwner;
    run.chain = Chain::new(vec![Box::new(TurnReminder::new(
        "Synthetic reminder.".into(),
    ))]);
    run.trace = Some(Trace::new(path.clone(), 3, 9));
    let out = run.go(&fake);
    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );

    let lines: Vec<Value> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let kinds: Vec<&str> = lines.iter().map(|l| l["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["context", "tools", "request", "tool", "request", "turn_end"],
        "{lines:?}"
    );
    assert_eq!(lines[5]["steps"], json!(2));
    assert!(
        lines[5]["ended"]
            .as_str()
            .unwrap()
            .starts_with("Stopped(End)")
    );
    for line in &lines {
        assert_eq!(
            (
                line["v"].clone(),
                line["session"].clone(),
                line["turn"].clone()
            ),
            (json!(1), json!(3), json!(9))
        );
    }
    assert_eq!(lines[0]["text"], json!("\n\nSynthetic reminder."));
    assert_eq!(lines[0]["origin"], json!("TopLevelOwner"));
    let hash = lines[1]["hash"].clone();
    assert!(
        lines[1]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == json!("write_file"))
    );
    assert_eq!(
        (
            lines[2]["request_id"].clone(),
            lines[2]["stop"].clone(),
            lines[2]["tools_hash"].clone()
        ),
        (json!("req-0"), json!("tool_use"), hash.clone())
    );
    assert_eq!(lines[3]["request_id"], json!("req-0"));
    assert_eq!(
        (lines[3]["id"].clone(), lines[3]["name"].clone()),
        (json!("c1"), json!("write_file"))
    );
    assert!(lines[3]["ms"].as_f64().unwrap() >= 0.0);
    assert!(lines[3]["result_chars"].as_u64().unwrap() > 0);
    assert_eq!(lines[3]["spilled"], json!(false));
    assert_eq!(
        (
            lines[4]["request_id"].clone(),
            lines[4]["stop"].clone(),
            lines[4]["tools_hash"].clone()
        ),
        (json!("req-1"), json!("end"), hash)
    );
    // The journal side is unchanged: the context never reaches the outcome.
    assert!(
        !serde_json::to_string(&out.messages)
            .unwrap()
            .contains("Synthetic")
    );
}

#[test]
fn the_turn_context_reaches_the_input_of_every_request_and_never_the_outcome() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        tool_step(vec![call(
            "c1",
            "write_file",
            json!({"path": "a.txt", "content": "a"}),
        )]),
        text_step("First."),
        text_step("Second."),
    ]);
    let mut run = Run::new(&dir.0);
    run.origin = Origin::TopLevelOwner;
    run.chain = Chain::new(vec![
        Box::new(TurnReminder::new("Synthetic reminder.".into())),
        Box::new(Note("Synthetic note.")),
        Box::new(Nudge { left: 1 }),
    ]);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert!(
        !serde_json::to_string(&out.messages)
            .unwrap()
            .contains("Synthetic"),
        "{out:?}"
    );
    let sent = vec![text_message(
        Role::User,
        "Start.\n\nSynthetic reminder.\n\nSynthetic note.",
    )];
    assert_eq!(
        turns(&fake),
        vec![
            expected_body_after(sent.clone(), 0, &[]),
            expected_body_after(sent.clone(), 1, &out.messages[..2]),
            expected_body_after(sent, 2, &out.messages[..4]),
        ]
    );
}

#[test]
fn the_images_follow_the_input_of_every_request_and_never_the_outcome() {
    let dir = Scratch::new();
    let png = dir.0.join("a.png");
    fs::write(&png, b"\x89PNG\r\n\x1a\nxyz").unwrap();
    let fake = FakeGateway::start(vec![
        tool_step(vec![call(
            "c1",
            "write_file",
            json!({"path": "a.txt", "content": "a"}),
        )]),
        text_step("Seen."),
    ]);
    let mut run = Run::new(&dir.0);
    run.images = ulei_engine::images::blocks(&[
        png.display().to_string(),
        dir.0.join("missing.jpg").display().to_string(),
    ]);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    let kept = serde_json::to_string(&out.messages).unwrap();
    assert!(!kept.contains("iVBORw0KGgp4eXo="), "{kept}");
    assert!(!kept.contains("missing.jpg"), "{kept}");
    let image: Block = serde_json::from_value(
        json!({"type": "image", "media_type": "image/png", "base64": "iVBORw0KGgp4eXo="}),
    )
    .unwrap();
    let [
        Block {
            text: Some(note), ..
        },
    ] = &run.images[1..]
    else {
        panic!("{:?}", run.images)
    };
    assert!(
        note.starts_with(&format!(
            "[The user sent an image saved at {}, but it could not be attached (cannot open it: ",
            dir.0.join("missing.jpg").display()
        )),
        "{note}"
    );
    let sent = vec![Message {
        role: Role::User,
        content: vec![text_block("Start."), image, text_block(note)],
    }];
    assert_eq!(
        turns(&fake),
        vec![
            expected_body_after(sent.clone(), 0, &[]),
            expected_body_after(sent, 1, &out.messages[..2]),
        ]
    );
}

#[test]
fn the_reminder_is_off_for_another_origin_and_for_an_empty_text() {
    for (origin, text) in [
        (Origin::Other, "Synthetic reminder."),
        (Origin::TopLevelOwner, ""),
    ] {
        let dir = Scratch::new();
        let fake = FakeGateway::start(vec![text_step("Hi.")]);
        let mut run = Run::new(&dir.0);
        run.origin = origin;
        run.chain = Chain::new(vec![Box::new(TurnReminder::new(text.into()))]);
        run.go(&fake);
        assert_eq!(turns(&fake), vec![expected_body(0, &[])], "{origin:?}");
    }
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
            native: None,
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
            native: None,
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

/// A tool of the name `name` answering `chars` characters of `fill`.
struct Big {
    name: &'static str,
    chars: usize,
    fill: char,
}

impl Tool for Big {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: self.name.into(),
            description: String::new(),
            input_schema: Map::new(),
            native: None,
        }
    }

    fn run(&self, _input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        Output::ok(self.fill.to_string().repeat(self.chars))
    }
}

#[test]
fn a_large_mcp_result_reaches_the_model_as_a_preview_with_the_path_of_its_file() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        tool_step(vec![
            call("c1", "mcp__hive__hive_health", json!({})),
            call("c2", "mcp__hive__hive_list", json!({})),
        ]),
        text_step("Done."),
    ]);
    let mut run = Run::new(&dir.0);
    run.extra.push(Box::new(Big {
        name: "mcp__hive__hive_health",
        chars: 63_520,
        fill: 'h',
    }));
    run.extra.push(Box::new(Big {
        name: "mcp__hive__hive_list",
        chars: 50_000,
        fill: 'l',
    }));
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    let got = results(&out.messages[1]);
    let path = dir.0.join("spillover").join("c1.txt");
    assert_eq!(fs::read_to_string(&path).unwrap(), "h".repeat(63_520));
    assert!(!got[0].1, "{:?}", got[0]);
    assert!(
        got[0].2.starts_with(&format!(
            "<persisted-output>\nThis tool result was too large (63,520 characters, 62.0 KB).\n\
             Full output saved to: {}\n",
            path.display()
        )),
        "{:?}",
        got[0].2
    );
    assert!(got[0].2.contains(&format!(
        "Preview (first 1500 chars):\n{}\n...\n",
        "h".repeat(1500)
    )));
    assert!(got[0].2.chars().count() < 2_400, "{}", got[0].2.len());
    // At the threshold a result stays whole.
    assert_eq!(got[1].2, "l".repeat(50_000));
    assert!(!dir.0.join("spillover").join("c2.txt").exists());
    // The request of the next step carries the preview, not the result.
    assert_eq!(sent_messages(&fake, 1), out.messages[..2]);
}

#[test]
fn the_largest_results_of_a_step_over_its_budget_go_to_files() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        tool_step(vec![
            call("c1", "memory", json!({})),
            call("c2", "web_search", json!({"query": "q1"})),
            call("c3", "mcp__hive__hive_list", json!({})),
        ]),
        text_step("Done."),
    ]);
    let mut run = Run::new(&dir.0);
    run.extra.push(Box::new(Big {
        name: "memory",
        chars: 98_000,
        fill: 'm',
    }));
    run.extra.push(Box::new(Big {
        name: "web_search",
        chars: 99_000,
        fill: 'w',
    }));
    run.extra.push(Box::new(Big {
        name: "mcp__hive__hive_list",
        chars: 40_000,
        fill: 'l',
    }));
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    // Each is under its own threshold; together they pass 200,000 characters, and saving the
    // largest one brings them under it.
    let got = results(&out.messages[1]);
    assert_eq!(got[0].2, "m".repeat(98_000));
    assert!(
        got[1].2.starts_with("<persisted-output>\n"),
        "{:?}",
        got[1].2
    );
    assert_eq!(
        fs::read_to_string(dir.0.join("spillover").join("c2.txt")).unwrap(),
        "w".repeat(99_000)
    );
    assert!(got[1].2.chars().count() < 2_400, "{}", got[1].2.len());
    assert_eq!(got[2].2, "l".repeat(40_000));
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

#[test]
fn an_empty_reply_never_reaches_on_stop_so_no_continue_follows_it() {
    let dir = Scratch::new();
    let mut script: Vec<Answer> = (0..4).map(|_| empty_step(vec![])).collect();
    script.push(text_step("unreachable"));
    let fake = FakeGateway::start(script);
    let mut run = Run::new(&dir.0);
    run.chain = Chain::new(vec![Box::new(Nudge { left: 1 })]);
    let out = run.go(&fake);
    assert_eq!(out.messages, [text_message(Role::Assistant, "(empty)")]);
    assert_eq!(turns(&fake).len(), 4);

    let fake = FakeGateway::start(vec![
        empty_step(vec![]),
        text_step("Here."),
        text_step("Again."),
    ]);
    let mut run = Run::new(&dir.0);
    run.chain = Chain::new(vec![Box::new(Nudge { left: 1 })]);
    let out = run.go(&fake);
    assert_eq!(
        out.messages,
        [
            text_message(Role::Assistant, "Here."),
            text_message(Role::User, "keep going"),
            text_message(Role::Assistant, "Again."),
        ]
    );
    assert_eq!(
        turns(&fake),
        [
            expected_body(0, &[]),
            expected_body(1, &[]),
            expected_body(2, &out.messages[..2]),
        ]
    );
}

/// A step that says `reply` and calls each of `tools` with a query.
fn said_and_called(reply: &str, tools: &[&str]) -> Answer {
    let mut events = vec![route(), text(reply)];
    for (i, tool) in tools.iter().enumerate() {
        events.push(call(&format!("c{i}"), tool, json!({"query": "q"})));
    }
    events.push(stop(StopReason::ToolUse));
    Answer::Events(events)
}

/// A run with `memory` and `web_search` stand-ins that answer at once.
fn housekeeping_run(dir: &Path) -> Run {
    let mut run = Run::new(dir);
    run.extra.push(Box::new(Meet::new("memory", 1)));
    run.extra.push(Box::new(Meet::new("web_search", 1)));
    run
}

/// The text of each message of `out`, with `+calls` where it calls a tool.
fn shapes(out: &Outcome) -> Vec<String> {
    out.messages
        .iter()
        .map(|m| {
            let calls = m.content.iter().any(|b| b.r#type == BlockType::ToolUse);
            let results = m.content.iter().any(|b| b.r#type == BlockType::ToolResult);
            let text = ulei_engine::agent::text(m);
            match (calls, results) {
                (_, true) => "results".to_owned(),
                (true, _) => format!("{text}+calls"),
                _ => text,
            }
        })
        .collect()
}

#[test]
fn an_empty_reply_after_a_step_that_called_only_memory_answers_with_that_steps_text() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        said_and_called("<think>save it</think> You are welcome. ", &["memory"]),
        said_and_called("", &["memory"]),
        empty_step(vec![thinking("sig-1")]),
        text_step("unreachable"),
    ]);
    let mut run = housekeeping_run(&dir.0);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(
        shapes(&out),
        [
            "<think>save it</think> You are welcome. +calls",
            "results",
            "+calls",
            "results",
            "You are welcome.",
        ]
    );
    assert_eq!(turns(&fake).len(), 3);
    assert!(run.sleeper.0.is_empty(), "{:?}", run.sleeper.0);
}

#[test]
fn the_housekeeping_answer_goes_through_the_stop_policies_and_their_continue_note() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        said_and_called("Saved.", &["memory"]),
        empty_step(vec![]),
        text_step("Anything else?"),
    ]);
    let mut run = housekeeping_run(&dir.0);
    run.chain = Chain::new(vec![Box::new(Nudge { left: 1 })]);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(
        shapes(&out),
        [
            "Saved.+calls",
            "results",
            "Saved.",
            "keep going",
            "Anything else?"
        ]
    );
    let sent = request(&turns(&fake)[2]);
    assert_eq!(sent.messages[sent.messages.len() - 2..], out.messages[2..4]);
    assert_eq!(turns(&fake).len(), 3);

    let fake = FakeGateway::start(vec![
        said_and_called("Saved.", &["memory"]),
        Answer::Events(vec![route(), stop(StopReason::ToolUse)]),
        text_step("unreachable"),
    ]);
    let out = housekeeping_run(&dir.0).go(&fake);
    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::ToolUse)),
        "{out:?}"
    );
    assert_eq!(shapes(&out), ["Saved.+calls", "results", "Saved."]);
    assert_eq!(turns(&fake).len(), 2);
}

#[test]
fn a_step_that_also_calls_another_tool_leaves_an_empty_reply_to_the_nudge() {
    let dir = Scratch::new();
    for script in [
        vec![
            said_and_called("Saved.", &["memory"]),
            said_and_called("", &["web_search"]),
        ],
        vec![said_and_called("Saved.", &["memory", "web_search"])],
    ] {
        let steps = script.len();
        let mut script = script;
        script.push(empty_step(vec![]));
        script.push(text_step("Recovered after nudge."));
        let fake = FakeGateway::start(script);
        let mut run = housekeeping_run(&dir.0);
        let out = run.go(&fake);

        let last = out.messages.last().unwrap();
        assert_eq!(
            *last,
            text_message(Role::Assistant, "Recovered after nudge.")
        );
        let sent = request(&turns(&fake)[steps + 1]);
        assert_eq!(
            sent.messages[sent.messages.len() - 1],
            text_message(Role::User, NUDGE)
        );
        assert_eq!(turns(&fake).len(), steps + 2);
    }
}

#[test]
fn a_bare_tool_marker_beside_a_call_is_dropped_and_never_becomes_the_answer() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        said_and_called(" [memory] ", &["memory"]),
        empty_step(vec![]),
        text_step("Recovered after nudge."),
    ]);
    let out = housekeeping_run(&dir.0).go(&fake);

    assert_eq!(
        shapes(&out),
        ["+calls", "results", "Recovered after nudge."]
    );
    assert!(
        !out.messages[0]
            .content
            .iter()
            .any(|b| b.r#type == BlockType::Text),
        "{out:?}"
    );
    assert_eq!(turns(&fake).len(), 3);
}

const DROPPED_NUDGE: &str = "Your previous turn indicated a tool call but none was included. Do not narrate a plan or restate intent — issue the actual tool call now to continue the task.";

/// A step that says `reply` and stops for tool use without calling a tool.
fn dropped_call_step(reply: &str) -> Answer {
    Answer::Events(vec![route(), text(reply), stop(StopReason::ToolUse)])
}

#[test]
fn a_tool_use_stop_without_a_call_is_nudged_three_times_then_answers_through_the_stop_policies() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        dropped_call_step("Plan 1."),
        dropped_call_step("Plan 2."),
        dropped_call_step("Plan 3."),
        dropped_call_step("Plan 4."),
        text_step("Done."),
    ]);
    let mut run = Run::new(&dir.0);
    run.chain = Chain::new(vec![Box::new(Nudge { left: 1 })]);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(
        out.messages,
        [
            text_message(Role::Assistant, "Plan 4."),
            text_message(Role::User, "keep going"),
            text_message(Role::Assistant, "Done."),
        ]
    );
    let mut nudged = Vec::new();
    let mut want = vec![expected_body(0, &[])];
    for (step, plan) in (1..).zip(["Plan 1.", "Plan 2.", "Plan 3."]) {
        nudged.push(text_message(Role::Assistant, plan));
        nudged.push(text_message(Role::User, DROPPED_NUDGE));
        want.push(expected_body(step, &nudged));
    }
    want.push(expected_body(4, &out.messages[..2]));
    assert_eq!(turns(&fake), want);
}

#[test]
fn a_tool_call_after_a_dropped_one_keeps_the_nudge_out_of_the_history_and_resets_the_count() {
    let dir = Scratch::new();
    let mut script = vec![dropped_call_step("Plan.")];
    script.push(search_step("c1"));
    script.extend((0..3).map(|_| dropped_call_step("Again.")));
    script.push(text_step("Done."));
    let fake = FakeGateway::start(script);
    let out = Run::new(&dir.0).go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(out.messages.len(), 3);
    assert_eq!(out.messages[2], text_message(Role::Assistant, "Done."));
    let pair = [
        text_message(Role::Assistant, "Plan."),
        text_message(Role::User, DROPPED_NUDGE),
    ];
    assert_eq!(sent_messages(&fake, 1), pair);
    let mut after_tools = pair.to_vec();
    after_tools.extend_from_slice(&out.messages[..2]);
    assert_eq!(sent_messages(&fake, 2), after_tools);
    assert_eq!(turns(&fake).len(), 6);
}

const JUDGE: &str = "judge";
const NOTE: &str = "[synthetic guard {chain}] Act on the plan without asking.";
const INSTRUCTIONS: &str = "Synthetic rules; data sits between {tag} marks.\n{tag}\n{owner}\n{tag}\n{tag}\n{reply}\n{tag}\nAnswer {{\"verdict\": \"extra|legit\"}}";

/// The instructions as the classifier must get them.
fn prompt(tag: &str, owner: &str, reply: &str) -> String {
    format!(
        "Synthetic rules; data sits between {tag} marks.\n{tag}\n{owner}\n{tag}\n{tag}\n{reply}\n{tag}\nAnswer {{\"verdict\": \"extra|legit\"}}"
    )
}

fn no_ask_guard(fake: &FakeGateway, log: &Path, timeout: Duration) -> NoAskGuard {
    NoAskGuard::new(
        Classifier {
            client: Client::new(fake.socket(), SLOW, timeout),
            route: JUDGE.into(),
            instructions: TextTemplate::parse(INSTRUCTIONS, INSTRUCTION_FIELDS).unwrap(),
            max_tokens: 256,
            cache: None,
            timeout,
        },
        TextTemplate::parse(NOTE, NOTE_FIELDS).unwrap(),
        log.to_owned(),
        "session-1".into(),
    )
}

/// A run of a top-level owner turn with only `no-ask-guard`, logging to `logs/guard.jsonl`.
fn guarded(dir: &Path, fake: &FakeGateway, timeout: Duration) -> Run {
    let mut run = Run::new(dir);
    run.origin = Origin::TopLevelOwner;
    run.chain = Chain::new(vec![Box::new(no_ask_guard(
        fake,
        &dir.join("logs/guard.jsonl"),
        timeout,
    ))]);
    run
}

fn verdict(verdict: &str) -> Answer {
    text_step(&format!(
        "Verdict: {{\"verdict\": \"{verdict}\", \"category\": \"permission\", \"reason\": \"synthetic\"}}"
    ))
}

/// The guard's log records without their time.
fn records(dir: &Path) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(dir.join("logs/guard.jsonl")) else {
        return Vec::new();
    };
    text.lines()
        .map(|line| {
            let mut record: Value = serde_json::from_str(line).unwrap();
            let ts = record.as_object_mut().unwrap().remove("ts").unwrap();
            assert!(ts.as_str().unwrap().ends_with(" UTC"), "{ts}");
            record
        })
        .collect()
}

/// The recorded turn requests on `route`.
fn requests_on(fake: &FakeGateway, route: &str) -> Vec<TurnRequest> {
    turns(fake)
        .iter()
        .map(|body| request(body))
        .filter(|r| r.route == route)
        .collect()
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Asserts that `request` is the tool-less classifier call on `reply` to `owner`.
fn assert_classified(request: &TurnRequest, owner: &str, reply: &str) {
    let text = match request.messages.as_slice() {
        [message] if message.role == Role::User => match message.content.as_slice() {
            [block] if block.r#type == BlockType::Text => block.text.clone().unwrap(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let start = text.find("⟦DATA-").unwrap();
    let end = start + text[start..].find('⟧').unwrap() + '⟧'.len_utf8();
    let tag = &text[start..end];
    assert!(
        is_hex(&tag["⟦DATA-".len()..tag.len() - '⟧'.len_utf8()], 12),
        "{tag}"
    );
    let id = request.request_id.strip_prefix("ask-").unwrap();
    assert!(is_hex(id, 16), "{}", request.request_id);
    assert_eq!(
        *request,
        TurnRequest {
            route: JUDGE.into(),
            system: None,
            messages: vec![text_message(Role::User, &prompt(tag, owner, reply))],
            tools: None,
            max_tokens: 256,
            effort: None,
            session_key: None,
            cache: None,
            request_id: request.request_id.clone(),
        }
    );
}

/// The chain id of a suppression record, checked to be 16 hex digits.
fn chain_of(record: &Value) -> String {
    let chain = record["chain"].as_str().unwrap();
    assert!(is_hex(chain, 16), "{chain}");
    chain.to_owned()
}

fn note(chain: &str) -> Message {
    text_message(
        Role::User,
        &format!("[synthetic guard {chain}] Act on the plan without asking."),
    )
}

#[test]
fn a_needless_closing_question_is_held_back_and_the_turn_continues_with_the_note() {
    let dir = Scratch::new();
    let question = "The draft is ready. Shall I format it?";
    let fake = FakeGateway::start(vec![
        text_step(question),
        verdict("extra"),
        text_step("Formatted the draft."),
    ]);
    let mut run = guarded(&dir.0, &fake, SLOW);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    let log = records(&dir.0);
    let chain = chain_of(&log[0]);
    assert_eq!(
        out.messages,
        [
            text_message(Role::Assistant, question),
            note(&chain),
            text_message(Role::Assistant, "Formatted the draft."),
        ]
    );
    let judged = requests_on(&fake, JUDGE);
    assert_eq!(judged.len(), 1);
    assert_classified(&judged[0], "Start.", question);
    let main: Vec<Vec<u8>> = requests_on(&fake, "chat")
        .iter()
        .map(|r| serde_json::to_vec(r).unwrap())
        .collect();
    assert_eq!(
        main,
        [expected_body(0, &[]), expected_body(1, &out.messages[..2])]
    );
    let ms = &log[0]["ms"];
    assert!(ms.is_u64(), "{ms}");
    assert_eq!(
        log,
        [
            json!({
                "session_id": "session-1",
                "reply_tail": question,
                "ms": ms,
                "verdict": "extra",
                "reason": "synthetic",
                "category": "permission",
                "chain": chain,
                "streak": 1,
                "action": "suppressed_continue",
            }),
            json!({
                "session_id": "session-1",
                "chain": chain,
                "action": "continuation_done",
                "reply_tail": "Formatted the draft.",
            }),
        ]
    );
}

#[test]
fn the_third_question_of_a_chain_is_delivered_and_a_new_owner_input_starts_a_new_chain() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        text_step("Shall I start?"),
        verdict("extra"),
        text_step("Ready. Go on?"),
        verdict("extra"),
        text_step("Really go on?"),
        verdict("extra"),
        text_step("Next one?"),
        verdict("extra"),
        text_step("Done."),
    ]);
    let mut run = guarded(&dir.0, &fake, SLOW);
    let out = run.go(&fake);

    let log = records(&dir.0);
    let actions: Vec<&str> = log.iter().map(|r| r["action"].as_str().unwrap()).collect();
    assert_eq!(
        actions,
        [
            "suppressed_continue",
            "suppressed_continue",
            "pass_loop_cap",
            "continuation_done"
        ]
    );
    let (first, second) = (chain_of(&log[0]), chain_of(&log[1]));
    assert_ne!(first, second);
    assert_eq!(
        out.messages,
        [
            text_message(Role::Assistant, "Shall I start?"),
            note(&first),
            text_message(Role::Assistant, "Ready. Go on?"),
            note(&second),
            text_message(Role::Assistant, "Really go on?"),
        ]
    );
    assert_eq!(log[1]["parent"], json!(first));
    assert_eq!(log[1]["streak"], json!(2));
    assert_eq!(log[2]["parent"], json!(second));
    assert_eq!(log[2]["streak"], json!(2));
    assert_eq!(log[3]["chain"], json!(second));
    let judged = requests_on(&fake, JUDGE);
    for (request, reply) in judged
        .iter()
        .zip(["Shall I start?", "Ready. Go on?", "Really go on?"])
    {
        assert_classified(request, "Start.", reply);
    }

    let out = run.go(&fake);
    let log = records(&dir.0);
    assert_eq!(log.len(), 6);
    assert_eq!(log[4]["action"], "suppressed_continue");
    assert_eq!(log[4]["streak"], 1);
    assert_eq!(log[4].get("parent"), None);
    let third = chain_of(&log[4]);
    assert!(third != first && third != second);
    assert_eq!(
        out.messages,
        [
            text_message(Role::Assistant, "Next one?"),
            note(&third),
            text_message(Role::Assistant, "Done."),
        ]
    );
}

#[test]
fn a_legit_verdict_and_a_classifier_error_timeout_or_quota_answer_deliver_the_reply_unchanged() {
    let quick = Duration::from_millis(200);
    let cases = [
        ("legit", verdict("legit"), "legit", SLOW),
        (
            "unavailable",
            Answer::Reject(503, gateway_error(ErrorClass::Unavailable)),
            "error",
            SLOW,
        ),
        ("no verdict", text_step("Cannot tell."), "error", SLOW),
        (
            "quota",
            Answer::Reject(429, gateway_error(ErrorClass::QuotaExhausted)),
            "error",
            SLOW,
        ),
        ("timeout", Answer::Hang(vec![route()]), "error", quick),
    ];
    for (name, answer, want, timeout) in cases {
        let dir = Scratch::new();
        let question = "Shall I start?";
        let fake = FakeGateway::start(vec![text_step(question), answer]);
        let mut run = guarded(&dir.0, &fake, timeout);
        let started = Instant::now();
        let out = run.go(&fake);

        assert!(
            matches!(out.ended, Ended::Stopped(StopReason::End)),
            "{name}: {out:?}"
        );
        assert_eq!(
            out.messages,
            [text_message(Role::Assistant, question)],
            "{name}"
        );
        let log = records(&dir.0);
        assert_eq!(log.len(), 1, "{name}: {log:?}");
        assert_eq!(log[0]["action"], "pass", "{name}");
        assert_eq!(log[0]["verdict"], want, "{name}");
        let judged = requests_on(&fake, JUDGE);
        assert_eq!(judged.len(), 1, "{name}");
        assert_classified(&judged[0], "Start.", question);
        if name == "timeout" {
            assert!(started.elapsed() < SLOW, "{:?}", started.elapsed());
            assert_eq!(cancels(&fake), [judged[0].request_id.clone()]);
        }
    }
}

#[test]
fn an_interrupt_during_the_classifier_call_cancels_it_at_once_and_delivers_the_reply() {
    let dir = Scratch::new();
    let question = "Shall I start?";
    let fake = FakeGateway::start(vec![text_step(question), Answer::Hang(vec![route()])]);
    let timeout = Duration::from_secs(30);
    let mut run = guarded(&dir.0, &fake, timeout);
    let remote = run.interrupt.clone();
    let started = Instant::now();
    let out = thread::scope(|scope| {
        scope.spawn(|| {
            assert!(fake.wait_hanging("ask-"));
            remote.interrupt();
        });
        run.go(&fake)
    });
    let elapsed = started.elapsed();

    assert!(
        elapsed < SLOW,
        "{elapsed:?} against a timeout of {timeout:?}"
    );
    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(out.messages, [text_message(Role::Assistant, question)]);
    let judged = requests_on(&fake, JUDGE);
    assert_eq!(judged.len(), 1);
    assert_eq!(cancels(&fake), [judged[0].request_id.clone()]);
    assert_eq!(requests_on(&fake, "chat").len(), 1);
    let log = records(&dir.0);
    let ms = &log[0]["ms"];
    assert_eq!(
        log,
        [json!({
            "session_id": "session-1",
            "reply_tail": question,
            "ms": ms,
            "verdict": "error",
            "reason": "the turn was interrupted",
            "category": "",
            "action": "pass",
        })]
    );
}

#[test]
fn an_extra_verdict_without_a_chain_id_delivers_the_reply_unchanged() {
    let dir = Scratch::new();
    let question = "Shall I start?";
    let fake = FakeGateway::start(vec![text_step(question), verdict("extra")]);
    let mut run = Run::new(&dir.0);
    run.origin = Origin::TopLevelOwner;
    let guard = no_ask_guard(&fake, &dir.0.join("logs/guard.jsonl"), SLOW)
        .with_chain_id(|| Err(io::Error::other("synthetic entropy failure")));
    run.chain = Chain::new(vec![Box::new(guard)]);
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(out.messages, [text_message(Role::Assistant, question)]);
    assert_eq!(requests_on(&fake, JUDGE).len(), 1);
    assert_eq!(requests_on(&fake, "chat").len(), 1);
    let log = records(&dir.0);
    let ms = &log[0]["ms"];
    assert_eq!(
        log,
        [json!({
            "session_id": "session-1",
            "reply_tail": question,
            "ms": ms,
            "verdict": "extra",
            "reason": "synthetic",
            "category": "permission",
            "action": "pass_inject_failed",
        })]
    );
}

#[test]
fn another_origin_and_a_reply_without_a_closing_question_make_no_classifier_call() {
    for (origin, reply) in [
        (Origin::Other, "Shall I start?"),
        (
            Origin::TopLevelOwner,
            "Was it red? It was.\nPainted it.\nFinished.",
        ),
    ] {
        let dir = Scratch::new();
        let fake = FakeGateway::start(vec![text_step(reply)]);
        let mut run = guarded(&dir.0, &fake, SLOW);
        run.origin = origin;
        let out = run.go(&fake);

        assert_eq!(out.messages, [text_message(Role::Assistant, reply)]);
        assert_eq!(turns(&fake).len(), 1, "{origin:?}");
        assert_eq!(records(&dir.0), Vec::<Value>::new());
    }
}

#[test]
fn the_pre_filter_reads_the_last_two_lines_outside_code_blocks_and_urls() {
    let yes = [
        "Ready?",
        "Shall I paint the fence blue and the gate green?",
        "Summary above.\n\nWant me to water the plants too?",
        "Is the colour fine? :)",
        "Line one.\nAsked here?\nand this closes it",
    ];
    let no = [
        "Done.",
        "",
        "Link https://example.test/?a=1",
        "```\nwhat?\n```\nDone.",
        "Is it red? It is.\nPainted.\nAll good.",
        "Ready?\nMEDIA: /tmp/picture.png",
    ];
    for text in yes {
        assert!(ends_with_question(text), "{text}");
    }
    for text in no {
        assert!(!ends_with_question(text), "{text}");
    }
}

#[test]
fn a_note_or_instructions_template_needs_each_placeholder_at_least_once_and_balanced_braces() {
    let note = TextTemplate::parse("a {{b}} {chain} c", NOTE_FIELDS).unwrap();
    assert_eq!(note.render(&["0123"]), "a {b} 0123 c");
    for (text, reason) in [
        ("no placeholder", "no {chain} placeholder"),
        ("{chain} {other}", "unknown placeholder {other}"),
        ("{chain} }", "a single '}' outside a placeholder"),
        ("{chain} {", "a '{' without its '}'"),
    ] {
        assert_eq!(
            TextTemplate::parse(text, NOTE_FIELDS),
            Err(reason.to_owned()),
            "{text}"
        );
    }
}

const EXACT_FAILURE_2: &str = "\n\n[Tool loop warning: repeated_exact_failure_warning; count=2; \
terminal has failed 2 times with identical arguments. This looks like a loop; inspect the error \
and change strategy instead of retrying it unchanged.]";
const HINT_COMMON_3: &str = " has failed 3 times this turn. This looks like a loop. Do not switch \
to text-only replies; keep using tools, but diagnose before retrying. First inspect the latest \
error/output and verify your assumptions. ";
const TERMINAL_HINT: &str = "For terminal failures, run a small diagnostic such as `pwd && ls -la` \
in the same tool, then try an absolute path, a simpler command, a different working directory, \
or a different tool such as read_file/write_file/patch.]";
const GENERIC_HINT: &str = "Try different arguments, a narrower query/path, an absolute path when \
relevant, or a different tool that can make progress. If the blocker is external, report the \
blocker after one diagnostic attempt instead of repeating the same failing path.]";
const NO_PROGRESS_2: &str = "\n\n[Tool loop warning: idempotent_no_progress_warning; count=2; \
search_files returned the same result 2 times. Use the result already provided or change the \
query instead of repeating it unchanged.]";
const FAILING: &str = "echo x >> ran; exit 3";

/// A run whose chain holds only `tool-guardrails`, hard stop on or off.
fn guarded_run(dir: &Path, hard_stop: bool) -> Run {
    guarded_with(
        dir,
        GuardrailConfig {
            hard_stop_enabled: hard_stop,
            ..GuardrailConfig::default()
        },
    )
}

/// A run whose chain holds only `tool-guardrails` under `config`.
fn guarded_with(dir: &Path, config: GuardrailConfig) -> Run {
    let mut run = Run::new(dir);
    run.chain = Chain::new(vec![Box::new(ToolGuardrails::new(config))]);
    run
}

/// One step of one call of `tool` with `input`.
fn one_call(n: usize, tool: &str, input: Value) -> Answer {
    tool_step(vec![call(&format!("c{n}"), tool, input)])
}

/// The result of the single call of each tool step of `out`, in order.
fn step_results(out: &Outcome) -> Vec<(bool, String)> {
    out.messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| {
            let got = results(m);
            assert_eq!(got.len(), 1, "{got:?}");
            (got[0].1, got[0].2.clone())
        })
        .collect()
}

/// The text of the last message of `out`, an assistant message.
fn answer(out: &Outcome) -> String {
    let last = out.messages.last().unwrap();
    assert_eq!(last.role, Role::Assistant);
    ulei_engine::agent::text(last)
}

fn ran_lines(dir: &Path) -> usize {
    fs::read_to_string(dir.join("ran")).unwrap().lines().count()
}

#[test]
fn a_second_identical_failure_gets_the_repeated_exact_failure_warning() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        one_call(1, "terminal", json!({"command": "exit 3"})),
        one_call(2, "terminal", json!({"command": "exit 3"})),
        text_step("Done."),
    ]);
    let out = guarded_run(&dir.0, false).go(&fake);

    let got = step_results(&out);
    assert!(!got[0].1.contains("[Tool loop"), "{got:?}");
    assert!(got[1].1.ends_with(EXACT_FAILURE_2), "{got:?}");
    assert!(
        got[1].1.starts_with(r#"{"output": "", "exit_code": 3"#),
        "{got:?}"
    );
    let sent = String::from_utf8(turns(&fake)[2].clone()).unwrap();
    assert!(sent.contains("[Tool loop warning: repeated_exact_failure_warning; count=2;"));
}

#[test]
fn a_third_failure_of_one_tool_gets_the_recovery_hint_of_that_tool_unless_a_success_came_between() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        one_call(1, "terminal", json!({"command": "exit 1"})),
        one_call(2, "terminal", json!({"command": "exit 2"})),
        one_call(3, "terminal", json!({"command": "exit 3"})),
        one_call(4, "read_file", json!({"path": "a.txt"})),
        one_call(5, "read_file", json!({"path": "b.txt"})),
        one_call(6, "terminal", json!({"command": "true"})),
        one_call(7, "read_file", json!({"path": "c.txt"})),
        one_call(8, "terminal", json!({"command": "exit 4"})),
        text_step("Done."),
    ]);
    let out = guarded_run(&dir.0, false).go(&fake);

    let got = step_results(&out);
    let terminal = format!(
        "\n\n[Tool loop warning: same_tool_failure_warning; count=3; terminal{HINT_COMMON_3}\
         {TERMINAL_HINT}"
    );
    let generic = format!(
        "\n\n[Tool loop warning: same_tool_failure_warning; count=3; read_file{HINT_COMMON_3}\
         {GENERIC_HINT}"
    );
    assert!(got[2].1.ends_with(&terminal), "{got:?}");
    assert!(got[6].1.ends_with(&generic), "{got:?}");
    assert!(got[6].0, "{got:?}");
    for i in [0, 1, 3, 4, 5, 7] {
        assert!(!got[i].1.contains("[Tool loop"), "result {i}: {got:?}");
    }
}

#[test]
fn an_identical_read_only_call_with_the_same_result_warns_until_the_result_changes() {
    let dir = Scratch::new();
    fs::write(dir.0.join("f.txt"), "needle\n").unwrap();
    let search = || json!({"pattern": "needle"});
    let fake = FakeGateway::start(vec![
        one_call(1, "search_files", search()),
        one_call(2, "search_files", search()),
        one_call(
            3,
            "write_file",
            json!({"path": "f.txt", "content": "needle\nneedle\n"}),
        ),
        one_call(4, "search_files", search()),
        one_call(5, "search_files", search()),
        text_step("Done."),
    ]);
    let out = guarded_run(&dir.0, false).go(&fake);

    let got = step_results(&out);
    assert!(!got[0].1.contains("[Tool loop"), "{got:?}");
    assert!(got[1].1.ends_with(NO_PROGRESS_2), "{got:?}");
    assert_ne!(got[3].1, got[0].1);
    assert!(!got[3].1.contains("[Tool loop"), "{got:?}");
    assert!(got[4].1.ends_with(NO_PROGRESS_2), "{got:?}");
}

#[test]
fn without_hard_stop_no_call_is_blocked_and_the_turn_runs_on_after_nine_failures() {
    let dir = Scratch::new();
    let mut script: Vec<Answer> = (1..=9)
        .map(|n| one_call(n, "terminal", json!({"command": FAILING})))
        .collect();
    script.push(text_step("Done."));
    let fake = FakeGateway::start(script);
    let out = guarded_run(&dir.0, false).go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(ran_lines(&dir.0), 9);
    assert_eq!(answer(&out), "Done.");
    let got = step_results(&out);
    assert!(
        got[8]
            .1
            .contains("[Tool loop warning: repeated_exact_failure_warning; count=9;"),
        "{got:?}"
    );
    assert!(got.iter().all(|r| !r.1.contains("hard stop")), "{got:?}");
}

#[test]
fn every_turn_starts_its_counts_from_zero() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        one_call(1, "terminal", json!({"command": "exit 3"})),
        text_step("First."),
        one_call(2, "terminal", json!({"command": "exit 3"})),
        text_step("Second."),
    ]);
    let mut run = guarded_run(&dir.0, false);
    let first = run.go(&fake);
    let second = run.go(&fake);

    for out in [first, second] {
        let got = step_results(&out);
        assert!(!got[0].1.contains("[Tool loop"), "{got:?}");
    }
}

#[test]
fn with_hard_stop_the_call_after_five_identical_failures_is_blocked_and_the_turn_halts() {
    let dir = Scratch::new();
    let script: Vec<Answer> = (1..=6)
        .map(|n| one_call(n, "terminal", json!({"timeout": 30, "command": FAILING})))
        .collect();
    let fake = FakeGateway::start(script);
    let out = guarded_run(&dir.0, true).go(&fake);

    assert!(matches!(out.ended, Ended::Halted), "{out:?}");
    assert_eq!(turns(&fake).len(), 6);
    assert_eq!(ran_lines(&dir.0), 5);
    let message = "Blocked terminal: the same tool call failed 5 times with identical arguments. \
                   Stop retrying it unchanged; change strategy or explain the blocker.";
    let blocked = format!(
        r#"{{"error": "{message}", "guardrail": {{"action": "block", "code": "repeated_exact_failure_block", "message": "{message}", "tool_name": "terminal", "count": 5, "signature": {{"tool_name": "terminal", "args_hash": "43313909534ae7a5b2d9718c8709f6c50cc518446b8fbaeb99c9baec33857e8d"}}}}}}"#
    );
    let got = step_results(&out);
    assert_eq!(got[5], (true, blocked));
    assert_eq!(
        answer(&out),
        "I stopped retrying terminal because it hit the tool-call guardrail \
         (repeated_exact_failure_block) after 5 repeated non-progressing attempts. The last tool \
         result explains the blocker; the next step is to change strategy instead of repeating \
         the same call."
    );
}

#[test]
fn with_hard_stop_the_eighth_failure_of_one_tool_halts_the_turn_after_its_batch() {
    let dir = Scratch::new();
    fs::write(dir.0.join("f.txt"), "kept\n").unwrap();
    let mut script: Vec<Answer> = (1..=7)
        .map(|n| one_call(n, "terminal", json!({"command": format!("exit {n}")})))
        .collect();
    script.push(tool_step(vec![
        call("c8", "terminal", json!({"command": "exit 8"})),
        call("c9", "read_file", json!({"path": "f.txt"})),
    ]));
    let fake = FakeGateway::start(script);
    let out = guarded_run(&dir.0, true).go(&fake);

    assert!(matches!(out.ended, Ended::Halted), "{out:?}");
    assert_eq!(turns(&fake).len(), 8);
    let batch = results(&out.messages[15]);
    assert!(
        batch[0].2.ends_with(
            "\n\n[Tool loop hard stop: same_tool_failure_halt; count=8; Stopped terminal: it \
             failed 8 times this turn. Stop retrying the same failing tool path and choose a \
             different approach.]"
        ),
        "{batch:?}"
    );
    assert!(!batch[1].1 && batch[1].2.contains("kept"), "{batch:?}");
    assert_eq!(
        answer(&out),
        "I stopped retrying terminal because it hit the tool-call guardrail \
         (same_tool_failure_halt) after 8 repeated non-progressing attempts. The last tool \
         result explains the blocker; the next step is to change strategy instead of repeating \
         the same call."
    );
}

#[test]
fn with_hard_stop_a_read_only_call_after_five_identical_results_is_blocked() {
    let dir = Scratch::new();
    fs::write(dir.0.join("f.txt"), "needle\nhay\n").unwrap();
    let mut script = Vec::new();
    for n in 0..5 {
        script.push(one_call(
            2 * n,
            "search_files",
            json!({"pattern": "needle"}),
        ));
        script.push(one_call(
            2 * n + 1,
            "search_files",
            json!({"pattern": "hay"}),
        ));
    }
    script.push(one_call(10, "search_files", json!({"pattern": "needle"})));
    let fake = FakeGateway::start(script);
    let out = guarded_run(&dir.0, true).go(&fake);

    assert!(matches!(out.ended, Ended::Halted), "{out:?}");
    let got = step_results(&out);
    assert!(
        got[8]
            .1
            .contains("idempotent_no_progress_warning; count=5;"),
        "{got:?}"
    );
    let (is_error, blocked) = &got[10];
    assert!(is_error);
    let blocked: Value = serde_json::from_str(blocked).unwrap();
    let message = "Blocked search_files: this read-only call returned the same result 5 times. \
                   Stop repeating it unchanged; use the result already provided or try a \
                   different query.";
    assert_eq!(blocked["error"], message);
    assert_eq!(blocked["guardrail"]["code"], "idempotent_no_progress_block");
    assert_eq!(blocked["guardrail"]["count"], 5);
    assert_eq!(
        answer(&out),
        "I stopped retrying search_files because it hit the tool-call guardrail \
         (idempotent_no_progress_block) after 5 repeated non-progressing attempts. The last \
         tool result explains the blocker; the next step is to change strategy instead of \
         repeating the same call."
    );
}

/// The identical-call notice of the `n`th call of `tool`.
fn stall_notice(n: &str, tool: &str) -> String {
    format!(
        "\n\n[note: this is the {n} consecutive identical call to {tool} with identical arguments \
         returning the same result. Do not repeat it \u{2014} change arguments, use a different \
         tool, or proceed with what you have.]"
    )
}

/// The reference stub of a `web_search` result identical to the result of call `id`.
fn result_stub(id: &str, args: &str) -> String {
    format!(
        "[note: this result is byte-identical to the web_search result earlier this turn \
         (tool_call_id {id}). Refer to that result; it has not changed. Args: {args}]"
    )
}

/// The no-progress warning of the `n`th identical `web_search` result.
fn web_no_progress(n: usize) -> String {
    format!(
        "\n\n[Tool loop warning: idempotent_no_progress_warning; count={n}; web_search returned \
         the same result {n} times. Use the result already provided or change the query instead \
         of repeating it unchanged.]"
    )
}

/// How many calls `meet` ran.
fn meet_runs(meet: &Arc<(Mutex<usize>, Condvar)>) -> usize {
    *meet.0.lock().unwrap()
}

#[test]
fn the_third_and_fourth_identical_calls_with_an_identical_result_get_the_stall_notice() {
    let dir = Scratch::new();
    let script: Vec<Answer> = (1..=4)
        .map(|n| one_call(n, "terminal", json!({"command": "echo same"})))
        .chain([text_step("Done.")])
        .collect();
    let fake = FakeGateway::start(script);
    let out = guarded_run(&dir.0, false).go(&fake);

    let got = step_results(&out);
    let raw = &got[0].1;
    assert!(raw.contains("same") && !raw.contains("[note"), "{got:?}");
    assert_eq!(&got[1].1, raw);
    assert_eq!(
        got[2].1,
        format!("{raw}{}", stall_notice("3rd", "terminal"))
    );
    assert_eq!(
        got[3].1,
        format!("{raw}{}", stall_notice("4th", "terminal"))
    );
}

#[test]
fn a_different_call_or_a_changed_result_in_between_restarts_the_streak() {
    let dir = Scratch::new();
    fs::write(dir.0.join("f.txt"), "kept\n").unwrap();
    let changes_on_third = || json!({"command": "echo x >> n; [ \"$(wc -l < n)\" -eq 3 ] && echo changed || echo same"});
    let mut script: Vec<Answer> = (1..=5)
        .map(|n| one_call(n, "terminal", changes_on_third()))
        .collect();
    script.push(one_call(6, "read_file", json!({"path": "f.txt"})));
    script.extend((7..=9).map(|n| one_call(n, "terminal", changes_on_third())));
    script.push(text_step("Done."));
    let fake = FakeGateway::start(script);
    let out = guarded_run(&dir.0, false).go(&fake);

    let got = step_results(&out);
    assert!(got[2].1.contains("changed"), "{got:?}");
    for (i, result) in got.iter().enumerate().take(8) {
        assert!(!result.1.contains("[note"), "result {i}: {got:?}");
    }
    assert!(
        got[8].1.ends_with(&stall_notice("3rd", "terminal")),
        "{got:?}"
    );
}

#[test]
fn from_the_second_identical_call_a_long_successful_result_is_replaced_by_the_reference_stub() {
    let dir = Scratch::new();
    let query = "q".repeat(600);
    let script: Vec<Answer> = (1..=3)
        .map(|n| one_call(n, "web_search", json!({"query": query})))
        .chain([text_step("Done.")])
        .collect();
    let fake = FakeGateway::start(script);
    let mut run = guarded_run(&dir.0, false);
    run.extra.push(Box::new(Meet::new("web_search", 1)));
    let out = run.go(&fake);

    let got = step_results(&out);
    assert_eq!(got[0], (false, format!("{query} met")));
    let args = format!("{{\"query\":\"{}\u{2026}", "q".repeat(110));
    let stub = result_stub("c1", &args);
    assert_eq!(got[1], (false, format!("{stub}{}", web_no_progress(2))));
    assert_eq!(
        got[2],
        (
            false,
            format!(
                "{stub}{}{}",
                web_no_progress(3),
                stall_notice("3rd", "web_search")
            )
        )
    );
}

#[test]
fn a_result_under_512_characters_or_a_failed_result_is_never_stubbed() {
    let dir = Scratch::new();
    let short = "s".repeat(507);
    let long = "l".repeat(508);
    let failing = || json!({"command": "printf '%0600d' 0; exit 1"});
    let mut script = Vec::new();
    for (n, query) in [(1, &short), (2, &short), (3, &long), (4, &long)] {
        script.push(one_call(n, "web_search", json!({"query": query})));
    }
    script.extend((5..=7).map(|n| one_call(n, "terminal", failing())));
    script.push(text_step("Done."));
    let fake = FakeGateway::start(script);
    let mut run = guarded_run(&dir.0, false);
    run.extra.push(Box::new(Meet::new("web_search", 1)));
    let out = run.go(&fake);

    let got = step_results(&out);
    assert_eq!(got[1].1, format!("{short} met{}", web_no_progress(2)));
    let stub = result_stub("c3", &format!("{{\"query\":\"{}\u{2026}", "l".repeat(110)));
    assert_eq!(got[3].1, format!("{stub}{}", web_no_progress(2)));
    for (i, (_, result)) in got[4..].iter().enumerate() {
        assert!(result.contains("\"exit_code\": 1"), "result {i}: {got:?}");
        assert!(result.contains(&"0".repeat(600)), "result {i}: {got:?}");
        assert!(!result.contains("byte-identical"), "result {i}: {got:?}");
    }
    assert!(
        got[6].1.ends_with(&stall_notice("3rd", "terminal")),
        "{got:?}"
    );
}

#[test]
fn with_stall_guards_off_there_is_no_notice_and_no_stub() {
    let dir = Scratch::new();
    let query = "q".repeat(600);
    let script: Vec<Answer> = (1..=3)
        .map(|n| one_call(n, "web_search", json!({"query": query})))
        .chain([text_step("Done.")])
        .collect();
    let fake = FakeGateway::start(script);
    let mut run = guarded_with(
        &dir.0,
        GuardrailConfig {
            stall_guards: false,
            ..GuardrailConfig::default()
        },
    );
    run.extra.push(Box::new(Meet::new("web_search", 1)));
    let out = run.go(&fake);

    let got = step_results(&out);
    assert_eq!(got[2].1, format!("{query} met{}", web_no_progress(3)));
}

/// One step of `n` `web_search` calls with distinct queries.
fn searches(n: usize) -> Answer {
    tool_step(
        (1..=n)
            .map(|i| {
                call(
                    &format!("s{i}"),
                    "web_search",
                    json!({"query": format!("q{i}")}),
                )
            })
            .collect(),
    )
}

#[test]
fn without_hard_stop_the_fifty_first_web_search_of_a_turn_is_blocked_and_the_turn_halts() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![searches(51)]);
    let meet = Meet::new("web_search", 1);
    let runs = Arc::clone(&meet.inside);
    let mut run = guarded_run(&dir.0, false);
    run.extra.push(Box::new(meet));
    let out = run.go(&fake);

    assert!(matches!(out.ended, Ended::Halted), "{out:?}");
    assert_eq!(turns(&fake).len(), 1);
    assert_eq!(meet_runs(&runs), 50);
    let batch = results(&out.messages[1]);
    assert_eq!(batch.len(), 51);
    assert!(batch[..50].iter().all(|r| !r.1), "{batch:?}");
    assert_eq!(batch[50].0, "s51");
    assert!(batch[50].1);
    let blocked: Value = serde_json::from_str(&batch[50].2).unwrap();
    let message = "Blocked web_search: this turn has already made 50 web searches, the per-turn \
                   limit. This looks like a runaway search loop. Work with the results you \
                   already have and give the user your answer.";
    assert_eq!(blocked["error"], message);
    assert_eq!(
        blocked["guardrail"],
        json!({
            "action": "block",
            "code": "loop_web_search_cap",
            "message": message,
            "tool_name": "web_search",
            "count": 50,
            "signature": blocked["guardrail"]["signature"],
        })
    );
    assert_eq!(blocked["guardrail"]["signature"]["tool_name"], "web_search");
    assert_eq!(
        answer(&out),
        "I stopped retrying web_search because it hit the tool-call guardrail \
         (loop_web_search_cap) after 50 repeated non-progressing attempts. The last tool result \
         explains the blocker; the next step is to change strategy instead of repeating the same \
         call."
    );
}

#[test]
fn a_web_search_cap_of_zero_is_no_limit() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![searches(51), text_step("Done.")]);
    let meet = Meet::new("web_search", 1);
    let runs = Arc::clone(&meet.inside);
    let mut run = guarded_with(
        &dir.0,
        GuardrailConfig {
            max_web_searches: 0,
            ..GuardrailConfig::default()
        },
    );
    run.extra.push(Box::new(meet));
    let out = run.go(&fake);

    assert_eq!(answer(&out), "Done.");
    assert_eq!(meet_runs(&runs), 51);
}

#[test]
fn every_turn_starts_its_web_search_count_from_zero() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        searches(50),
        text_step("First."),
        searches(1),
        text_step("Second."),
    ]);
    let mut run = guarded_run(&dir.0, false);
    run.extra.push(Box::new(Meet::new("web_search", 1)));
    assert_eq!(answer(&run.go(&fake)), "First.");
    run.extra.push(Box::new(Meet::new("web_search", 1)));
    let second = run.go(&fake);

    assert_eq!(answer(&second), "Second.");
    assert_eq!(step_results(&second), vec![(false, "q1 met".to_owned())]);
}

/// The vendor's refusal of a thinking block replayed after the system prompt changed.
const BOUND_ELSEWHERE: &str = "vendor answered HTTP 400: messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation. Remove the block, or set `thinking.block_binding.prefix_mismatch_behavior` to \"drop_block\". That setting requires the `thinking-binding-controls-2026-08-01` value in the `anthropic-beta` header.";

fn bad_request(message: &str) -> GatewayError {
    GatewayError {
        message: Some(message.into()),
        ..gateway_error(ErrorClass::BadRequest)
    }
}

/// A history whose earlier assistant reply carries a signed thinking block.
fn signed_history() -> Vec<Message> {
    vec![
        text_message(Role::User, "Earlier."),
        message(
            Role::Assistant,
            vec![thinking_block("sig-old"), text_block("Earlier answer.")],
        ),
        text_message(Role::User, "Start."),
    ]
}

fn thinking_count(body: &[u8]) -> usize {
    request(body)
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| b.r#type == BlockType::Thinking)
        .count()
}

#[test]
fn a_thinking_signature_bad_request_resends_the_step_without_thinking_for_the_rest_of_the_turn() {
    let dir = Scratch::new();
    fs::write(dir.0.join("a.txt"), "a").unwrap();
    let fake = FakeGateway::start(vec![
        Answer::Reject(400, bad_request(BOUND_ELSEWHERE)),
        tool_step(vec![
            thinking("sig-new"),
            call(
                "c1",
                "search_files",
                json!({"pattern": "*", "target": "files"}),
            ),
        ]),
        text_step("Done."),
    ]);
    let mut run = Run::new(&dir.0);
    run.history = signed_history();
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    assert_eq!(out.steps.len(), 2);
    assert_eq!(run.sleeper.0, Vec::<Duration>::new());
    let sent = turns(&fake);
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[0], expected_body_after(signed_history(), 0, &[]));
    let mut stripped = signed_history();
    stripped[1].content.remove(0);
    assert_eq!(sent[1], expected_body_after(stripped.clone(), 0, &[]));
    assert_eq!(thinking_count(&sent[2]), 0);
    assert_eq!(request(&sent[2]).messages[..3], stripped[..]);
    // The turn keeps the thinking blocks it got; the history it was given is untouched.
    assert_eq!(run.history, signed_history());
    assert_eq!(out.messages[0].content[0], thinking_block("sig-new"));
    assert_eq!(out.messages[2], text_message(Role::Assistant, "Done."));
}

#[test]
fn a_resent_step_refused_again_ends_the_turn_with_that_error_after_two_requests() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        Answer::Reject(400, bad_request(BOUND_ELSEWHERE)),
        Answer::Events(vec![
            route(),
            error_event(bad_request("thinking blocks cannot be modified")),
        ]),
        text_step("unreachable"),
    ]);
    let mut run = Run::new(&dir.0);
    run.history = signed_history();
    let out = run.go(&fake);

    match &out.ended {
        Ended::Gateway(error) => {
            assert_eq!(error.class, ErrorClass::BadRequest);
            assert_eq!(
                error.message.as_deref(),
                Some("thinking blocks cannot be modified")
            );
        }
        other => panic!("{other:?}"),
    }
    let sent = turns(&fake);
    assert_eq!(sent.len(), 2);
    assert_eq!(thinking_count(&sent[0]), 1);
    assert_eq!(thinking_count(&sent[1]), 0);
}

#[test]
fn another_bad_request_or_one_without_thinking_in_the_request_is_not_resent() {
    let dir = Scratch::new();
    for (history, error) in [
        (
            signed_history(),
            bad_request("messages: roles must alternate"),
        ),
        (
            signed_history(),
            bad_request("Invalid `signature` in tool block"),
        ),
        (history(), bad_request(BOUND_ELSEWHERE)),
    ] {
        let fake = FakeGateway::start(vec![
            Answer::Reject(400, error.clone()),
            text_step("unreachable"),
        ]);
        let mut run = Run::new(&dir.0);
        run.history = history;
        let out = run.go(&fake);
        assert!(
            matches!(&out.ended, Ended::Gateway(e) if *e == error),
            "{out:?}"
        );
        assert_eq!(turns(&fake).len(), 1);
    }
}

#[test]
fn a_thinking_only_message_is_left_out_of_the_resent_request_and_the_users_around_it_merged() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(vec![
        Answer::Reject(400, bad_request("redacted_thinking block is INVALID")),
        text_step("Done."),
    ]);
    let mut run = Run::new(&dir.0);
    run.history = vec![
        text_message(Role::User, "Earlier."),
        message(Role::Assistant, vec![thinking_block("sig-old")]),
        text_message(Role::User, "Start."),
    ];
    let out = run.go(&fake);

    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );
    let sent = turns(&fake);
    assert_eq!(sent.len(), 2);
    assert_eq!(
        request(&sent[1]).messages,
        vec![message(
            Role::User,
            vec![text_block("Earlier."), text_block("Start.")]
        )]
    );
}
