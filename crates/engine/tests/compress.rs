//! Compaction of a long session on the scripted fake gateway: the summary turn on its alias,
//! the pruned, kept and stored messages, the request after a restart, the update of an earlier
//! summary, and the fallback summary with its cooldown.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::path::Path;
use std::time::Duration;

use fake_gateway::{
    Answer, FakeGateway, Recorder, Scratch, error_event, gateway_error, line, request, route, stop,
    text_block, text_step, turns,
};
use serde_json::json;
use ulei_engine::agent::{Agent, DEFAULT_CONCURRENT_TOOL_TIMEOUT, Ended, Outcome, Step, Template};
use ulei_engine::compress::text::{SUMMARY_END_MARKER, SUMMARY_PREFIX};
use ulei_engine::compress::{Compression, Compressor, Error, Settings, Skip, Summary, Window};
use ulei_engine::gateway::Client;
use ulei_engine::journal::Journal;
use ulei_engine::policy::{Chain, Origin};
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{Interrupt, Registry};
use ulei_engine::wire::c4g::{Block, BlockType, ErrorClass, Message, Role, StopReason};
use ulei_engine::zone::Zone;

const SLOW: Duration = Duration::from_secs(2);
const KEY: &str = "main";
/// 2027-01-15 in UTC.
const NOW: i64 = 1_800_000_000;
const WINDOW: Window = Window {
    context_length: 200_000,
    threshold_tokens: THRESHOLD,
};
const THRESHOLD: u64 = 8_000;
const MODEL_SUMMARY: &str = "## Historical Task Snapshot\nUser asked: 'something else'\n\n## Goal\nKeep the build notes current.\n\n## Completed Actions\n1. RAN make in /srv/build — ok [tool: terminal]";

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![text_block(text)],
    }
}

fn assistant(content: Vec<Block>) -> Message {
    Message {
        role: Role::Assistant,
        content,
    }
}

fn block(value: serde_json::Value) -> Block {
    serde_json::from_value(value).unwrap()
}

fn settings(timeout: Duration) -> Settings {
    Settings {
        route: "compact".into(),
        timeout: Some(timeout),
        max_tokens: 16_384,
        protect_first_n: 3,
        protect_last_n: 20,
        target_ratio: 0.2,
    }
}

fn compressor(fake: &FakeGateway, timeout: Duration) -> Compressor {
    Compressor::new(
        Client::new(fake.socket(), SLOW, SLOW),
        settings(timeout),
        Redactor { enabled: true },
        Zone::utc(),
    )
}

/// Output of call `n`: short for the first turn, else a distinct body of 1,200 characters.
fn output(n: usize) -> String {
    if n == 1 {
        return "ok".into();
    }
    format!("step {n:02} output: {}", "x".repeat(1200 - 17))
}

/// Appends turn `n`: the prompt, a call of `terminal`, its result and a closing reply.
fn append_turn(journal: &mut Journal, session: u64, n: usize) {
    append_call(journal, session, n, &json!(format!("make step{n}")));
}

/// Appends turn `n` whose `terminal` call has `command` as its argument.
fn append_call(journal: &mut Journal, session: u64, n: usize, command: &serde_json::Value) {
    let turn = journal.next_turn(session).unwrap();
    let id = format!("c{n}");
    let messages = vec![
        assistant(vec![
            text_block(&format!("Running step {n}.")),
            block(
                json!({"type": "tool_use", "id": id, "name": "terminal", "input": {"command": command}}),
            ),
        ]),
        Message {
            role: Role::User,
            content: vec![block(
                json!({"type": "tool_result", "id": id, "content": output(n), "is_error": false}),
            )],
        },
        assistant(vec![text_block(&format!("Step {n} is done."))]),
    ];
    let outcome = Outcome {
        messages,
        steps: vec![Step {
            request_id: turn.request_id(0),
            route: None,
            usage: None,
        }],
        ended: Ended::Stopped(StopReason::End),
    };
    journal
        .append(turn, &[user(&format!("Please run step {n}."))], &outcome)
        .unwrap();
}

/// A journal at `path` whose session holds turns `1..=count`.
fn long_session(path: &Path, count: usize) -> (Journal, u64) {
    let mut journal = Journal::open(path).unwrap();
    let session = journal.session(KEY).unwrap().id;
    for n in 1..=count {
        append_turn(&mut journal, session, n);
    }
    (journal, session)
}

/// The summary in `messages`, from its prefix on.
fn summary_of(messages: &[Message]) -> String {
    let text = messages
        .iter()
        .map(text_of)
        .find(|t| t.contains(SUMMARY_PREFIX))
        .unwrap();
    text[text.find(SUMMARY_PREFIX).unwrap()..].to_owned()
}

/// The text of the one user message of the summary request `body`.
fn prompt(body: &[u8]) -> String {
    let request = request(body);
    assert_eq!(request.messages.len(), 1);
    assert_eq!(request.messages[0].role, Role::User);
    request.messages[0].content[0].text.clone().unwrap()
}

fn text_of(message: &Message) -> String {
    ulei_engine::agent::text(message)
}

/// The line that replaces the result of call `cN`, a `terminal` call of `make stepN`.
fn one_liner(id: &str) -> String {
    format!(
        "[terminal] ran `make step{}` -> exit ?, 1 lines output",
        &id[1..]
    )
}

/// `message` with each tool result longer than 200 characters replaced by its one-liner.
fn pruned(message: &Message) -> Message {
    let mut message = message.clone();
    for b in &mut message.content {
        if b.content.as_ref().is_some_and(|c| c.chars().count() > 200) {
            b.content = Some(one_liner(b.id.as_deref().unwrap()));
        }
    }
    message
}

/// Every `tool_use` of `messages` is answered by a `tool_result` in the next message, and
/// every `tool_result` answers a `tool_use` of the message before.
fn tool_pairs_whole(messages: &[Message]) -> bool {
    let ids = |m: &Message, kind: BlockType| -> Vec<Option<String>> {
        m.content
            .iter()
            .filter(|b| b.r#type == kind)
            .map(|b| b.id.clone())
            .collect()
    };
    messages.iter().enumerate().all(|(i, m)| {
        let uses = ids(m, BlockType::ToolUse);
        let answered = uses.is_empty()
            || messages
                .get(i + 1)
                .is_some_and(|next| ids(next, BlockType::ToolResult) == uses);
        let results = ids(m, BlockType::ToolResult);
        let called = results.is_empty()
            || i.checked_sub(1)
                .is_some_and(|p| ids(&messages[p], BlockType::ToolUse) == results);
        answered && called
    })
}

/// Runs one turn of `prompt` after `history` with the ids of the session's next turn.
fn next_turn(journal: &Journal, session: u64, history: &[Message], cwd: &Path) -> Vec<Vec<u8>> {
    let fake = FakeGateway::start(vec![text_step("Next answer.")]);
    let client = Client::new(fake.socket(), SLOW, SLOW);
    let tools = Registry::builtin(
        cwd,
        Redactor { enabled: true },
        None,
        &fake_gateway::skills(cwd),
        None,
    );
    let mut chain = Chain::default();
    let template = Template {
        route: "chat".into(),
        system: Some("You are a test.".into()),
        max_tokens: 1024,
        effort: None,
        session_key: Some(KEY.into()),
    };
    let turn = journal.next_turn(session).unwrap();
    let mut history = history.to_vec();
    history.push(user("What next?"));
    Agent {
        client: &client,
        tools: &tools,
        chain: &mut chain,
        cwd,
        template: &template,
        batch_timeout: Some(DEFAULT_CONCURRENT_TOOL_TIMEOUT),
        empty_guard: true,
        origin: Origin::Other,
    }
    .run(
        &history,
        &|n| turn.request_id(n),
        &Interrupt::new(),
        16,
        &mut Recorder::default(),
    );
    turns(&fake)
}

#[test]
fn a_long_session_is_stored_as_head_summary_and_tail_and_replays_the_same_bytes_after_a_restart() {
    let dir = Scratch::new();
    let cwd = Scratch::new();
    let path = dir.path("journal.sqlite3");
    let (mut journal, session) = long_session(&path, 14);
    let before = journal.messages(session).unwrap();
    let fake = FakeGateway::start(vec![text_step(MODEL_SUMMARY)]);

    let compression = compressor(&fake, SLOW)
        .compact(&mut journal, session, WINDOW, NOW, &Interrupt::new())
        .unwrap();

    let Compression::Compacted { messages, summary } = compression else {
        panic!("not compacted: {compression:?}");
    };
    assert_eq!(summary, Summary::Model);
    let sent = turns(&fake);
    assert_eq!(sent.len(), 1);
    let req = request(&sent[0]);
    assert_eq!(req.route, "compact");
    assert_eq!(req.tools, None);
    assert_eq!(req.system, None);
    let text = prompt(&sent[0]);
    assert!(text.starts_with("You are a summarization agent creating a context checkpoint."));
    assert!(text.contains("\n\nTURNS TO SUMMARIZE:\n[ASSISTANT]: Step 1 is done.\n\n[USER]: Please run step 2.\n\n[ASSISTANT]: Running step 2.\n[Tool calls:\n  terminal({\"command\":\"make step2\"})\n]\n\n[TOOL RESULT c2]: "));
    assert!(text.contains(
        "Use this exact structure:\n\n## Historical Task Snapshot\n[THE SINGLE MOST IMPORTANT FIELD."
    ));
    assert!(text.contains("TEMPORAL ANCHORING: The current date is 2027-01-15."));
    assert!(text.contains(&format!("[TOOL RESULT c2]: {}", one_liner("c2"))));
    assert!(!text.contains(&output(2)));

    let stored = journal.messages(session).unwrap();
    assert_eq!(stored, messages);
    assert_eq!(stored[..3], before[..3]);
    let summary_text = text_of(&stored[3]);
    assert_eq!(stored[3].role, Role::Assistant);
    assert!(summary_text.starts_with(&format!(
        "{SUMMARY_PREFIX}\n## Historical Task Snapshot\nUser asked (deterministic, from compacted turns): 'Please run step "
    )));
    assert!(summary_text.ends_with(&format!(
        "## Goal\nKeep the build notes current.\n\n## Completed Actions\n1. RAN make in /srv/build — ok [tool: terminal]\n\n{SUMMARY_END_MARKER}"
    )));
    let tail = &stored[4..];
    assert!(tail.len() >= 4);
    let original = &before[before.len() - tail.len()..];
    for (kept, was) in tail.iter().zip(original) {
        assert!(
            kept == was || pruned(was) == *kept,
            "{kept:?} is not {was:?}"
        );
    }
    assert_eq!(
        tail[0],
        user(&format!("Please run step {}.", 14 - tail.len() / 4 + 1))
    );
    assert!(tool_pairs_whole(&stored));
    let last_result = &tail[tail.len() - 2].content[0];
    assert_eq!(last_result.content.as_deref(), Some(output(14).as_str()));

    let never_stopped = next_turn(&journal, session, &messages, &cwd.0);
    drop(journal);
    let journal = Journal::open(&path).unwrap();
    let restarted = next_turn(
        &journal,
        session,
        &journal.messages(session).unwrap(),
        &cwd.0,
    );
    assert_eq!(restarted.len(), 1);
    assert_eq!(restarted, never_stopped);
    let body = request(&restarted[0]);
    assert_eq!(body.messages[..stored.len()], stored[..]);
}

#[test]
fn a_second_compaction_updates_the_earlier_summary_in_memory_and_after_a_restart() {
    let dir = Scratch::new();
    let path = dir.path("journal.sqlite3");
    let (mut journal, session) = long_session(&path, 14);
    let fake = FakeGateway::start(vec![
        text_step(MODEL_SUMMARY),
        text_step("## Goal\nSecond."),
        text_step("## Goal\nSecond."),
    ]);
    let mut running = compressor(&fake, SLOW);
    running
        .compact(&mut journal, session, WINDOW, NOW, &Interrupt::new())
        .unwrap();
    let first = summary_of(&journal.messages(session).unwrap());
    let body = first
        .strip_prefix(&format!("{SUMMARY_PREFIX}\n"))
        .and_then(|b| b.strip_suffix(&format!("\n\n{SUMMARY_END_MARKER}")))
        .unwrap()
        .to_owned();
    for n in 15..=24 {
        append_turn(&mut journal, session, n);
    }
    drop(journal);
    let copy = dir.path("copy.sqlite3");
    fs::copy(&path, &copy).unwrap();

    let mut journal = Journal::open(&path).unwrap();
    let in_memory = running
        .compact(&mut journal, session, WINDOW, NOW + 60, &Interrupt::new())
        .unwrap();
    let mut journal = Journal::open(&copy).unwrap();
    let restarted = compressor(&fake, SLOW)
        .compact(&mut journal, session, WINDOW, NOW + 60, &Interrupt::new())
        .unwrap();

    let sent = turns(&fake);
    assert_eq!(sent.len(), 3);
    let update = prompt(&sent[1]);
    assert!(update.contains(&format!(
        "You are updating a context compaction summary. A previous compaction produced the summary below. New conversation turns have occurred since then and need to be incorporated.\n\nPREVIOUS SUMMARY:\n{body}\n\nNEW TURNS TO INCORPORATE:\n[USER]: Please run step 1."
    )));
    assert!(!update.contains(SUMMARY_PREFIX));
    assert_eq!(prompt(&sent[2]), update);
    assert_eq!(in_memory, restarted);
    let Compression::Compacted { messages, .. } = in_memory else {
        panic!("not compacted");
    };
    let summaries = messages
        .iter()
        .filter(|m| text_of(m).contains(SUMMARY_PREFIX))
        .count();
    assert_eq!(summaries, 1);
    assert!(summary_of(&messages).contains("## Goal\nSecond."));
    assert!(tool_pairs_whole(&messages));
}

/// The first compaction of a fresh long session whose summary turn gets `answer`; then, with
/// more turns, one compaction a second before the cooldown of `seconds` ends, which must not
/// ask the gateway, and one when it ends, which gets a model summary. Returns the first result
/// and the live sets before and after it.
fn cooldown_rung(
    answer: Answer,
    timeout: Duration,
    seconds: i64,
) -> (Compression, Vec<Message>, Vec<Message>) {
    let dir = Scratch::new();
    let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 14);
    let before = journal.messages(session).unwrap();
    let fake = FakeGateway::start(vec![answer, text_step(MODEL_SUMMARY)]);
    let mut compressor = compressor(&fake, timeout);

    let first = compressor
        .compact(&mut journal, session, WINDOW, NOW, &Interrupt::new())
        .unwrap();
    let after = journal.messages(session).unwrap();
    assert_eq!(turns(&fake).len(), 1);

    for n in 15..=24 {
        append_turn(&mut journal, session, n);
    }
    let inside = compressor
        .compact(
            &mut journal,
            session,
            WINDOW,
            NOW + seconds - 1,
            &Interrupt::new(),
        )
        .unwrap();
    assert!(matches!(
        inside,
        Compression::Compacted {
            summary: Summary::Fallback { reason: None },
            ..
        } | Compression::Aborted { reason: None }
    ));
    assert_eq!(
        matches!(inside, Compression::Aborted { .. }),
        matches!(first, Compression::Aborted { .. })
    );
    assert_eq!(turns(&fake).len(), 1);

    for n in 25..=34 {
        append_turn(&mut journal, session, n);
    }
    let ended = compressor
        .compact(
            &mut journal,
            session,
            WINDOW,
            NOW + seconds,
            &Interrupt::new(),
        )
        .unwrap();
    assert!(matches!(
        ended,
        Compression::Compacted {
            summary: Summary::Model,
            ..
        }
    ));
    assert_eq!(turns(&fake).len(), 2);
    (first, before, after)
}

/// The fallback body of the compacted turns of a fresh 14-turn session.
const FALLBACK_BODY: &str = "## Historical Task Snapshot
User asked: 'Please run step 3.'

## Goal
Recovered from a deterministic fallback because the LLM context summarizer was unavailable. Continue from the protected recent messages after this summary and use current file/system state for exact details.

## Constraints & Preferences
- This fallback was generated locally without an LLM summary call.
- Secrets and credentials were redacted before preservation.
- The summary may be incomplete; prefer verifying current files, git state, processes, and test results instead of assuming omitted details.

## Completed Actions
1. Step 1 is done.
2. Called tool(s): terminal
3. Step 2 is done.
4. Called tool(s): terminal
5. Step 3 is done.
6. [terminal] ran `make step2` -> exit ?, 1 lines output
7. [terminal] ran `make step3` -> exit ?, 1 lines output

## Active State
Unknown from deterministic fallback. Inspect current repository/session state if needed.

## Blocked
None.

## Key Decisions
None recoverable from deterministic fallback.

## Resolved Questions
None recoverable from deterministic fallback.

## Relevant Files
None.

## Last Dropped Turns
- USER: Please run step 2.
- ASSISTANT: tool calls: terminal; Running step 2.
- TOOL: [terminal] ran `make step2` -> exit ?, 1 lines output
- ASSISTANT: Step 2 is done.
- USER: Please run step 3.
- ASSISTANT: tool calls: terminal; Running step 3.
- TOOL: [terminal] ran `make step3` -> exit ?, 1 lines output
- ASSISTANT: Step 3 is done.

## Critical Context
Summary generation was unavailable, so this is a best-effort deterministic fallback for 9 compacted message(s).";

/// `first` stored the fallback summary of [`FALLBACK_BODY`] naming `reason`.
fn assert_fallback(first: &Compression, stored: &[Message], reason: &str) {
    let Compression::Compacted {
        summary: Summary::Fallback { reason: Some(got) },
        messages,
    } = first
    else {
        panic!("no fallback: {first:?}");
    };
    assert_eq!(got, reason);
    assert_eq!(messages, stored);
    assert_eq!(
        summary_of(stored),
        format!(
            "{SUMMARY_PREFIX}\n{FALLBACK_BODY} Summary failure reason: {reason}.\n\n{SUMMARY_END_MARKER}"
        )
    );
}

/// `first` aborted for a reason holding `reason` and left the live set as it was.
fn assert_aborted(first: &Compression, before: &[Message], after: &[Message], reason: &str) {
    let Compression::Aborted { reason: Some(got) } = first else {
        panic!("not aborted: {first:?}");
    };
    assert!(got.contains(reason), "{got}");
    assert_eq!(after, before);
}

#[test]
fn an_empty_summary_stores_the_fallback_summary_and_holds_the_next_call_for_60_seconds() {
    let empty = Answer::Events(vec![route(), stop(StopReason::End)]);
    let (first, _, after) = cooldown_rung(empty, SLOW, 60);
    assert_fallback(
        &first,
        &after,
        "Context compression LLM returned empty content (provider=qb model=compact)",
    );
}

#[test]
fn a_stream_error_stores_the_fallback_summary_and_holds_the_next_call_for_60_seconds() {
    let error = Answer::Events(vec![
        route(),
        error_event(gateway_error(ErrorClass::BadRequest)),
    ]);
    let (first, _, after) = cooldown_rung(error, SLOW, 60);
    assert_fallback(&first, &after, "gateway error bad_request: no message");
}

#[test]
fn a_malformed_answer_stores_the_fallback_summary_and_holds_the_next_call_for_30_seconds() {
    let malformed = Answer::Chunks(vec![line(&route()), b"not json\n".to_vec()]);
    let (first, _, after) = cooldown_rung(malformed, SLOW, 30);
    assert_fallback(
        &first,
        &after,
        "malformed gateway answer: event line is not JSON",
    );
}

#[test]
fn a_route_no_vendor_answers_stores_the_fallback_summary_and_holds_the_next_call_for_600_seconds() {
    let error = Answer::Events(vec![
        route(),
        error_event(gateway_error(ErrorClass::Unavailable)),
    ]);
    let (first, _, after) = cooldown_rung(error, SLOW, 600);
    assert_fallback(&first, &after, "no auxiliary LLM provider configured");
}

#[test]
fn an_exhausted_quota_aborts_with_the_session_unchanged_and_holds_the_next_call_for_60_seconds() {
    let rejected = Answer::Reject(429, gateway_error(ErrorClass::QuotaExhausted));
    let (first, before, after) = cooldown_rung(rejected, SLOW, 60);
    assert_aborted(&first, &before, &after, "quota_exhausted");
}

#[test]
fn a_stream_cut_short_aborts_with_the_session_unchanged_and_holds_the_next_call_for_30_seconds() {
    let cut = Answer::Chunks(vec![line(&route())]);
    let (first, before, after) = cooldown_rung(cut, SLOW, 30);
    assert_aborted(
        &first,
        &before,
        &after,
        "turn stream ended without a terminal event",
    );
}

#[test]
fn an_unreachable_gateway_aborts_with_the_session_unchanged() {
    let dir = Scratch::new();
    let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 14);
    let before = journal.messages(session).unwrap();
    let mut compressor = Compressor::new(
        Client::new(dir.path("no-gateway.sock"), SLOW, SLOW),
        settings(SLOW),
        Redactor { enabled: true },
        Zone::utc(),
    );

    let first = compressor
        .compact(&mut journal, session, WINDOW, NOW, &Interrupt::new())
        .unwrap();

    assert_aborted(
        &first,
        &before,
        &journal.messages(session).unwrap(),
        "gateway socket",
    );
}

#[test]
fn timeouts_in_a_row_abort_and_hold_the_next_call_for_60_300_and_900_seconds() {
    let dir = Scratch::new();
    let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 14);
    let before = journal.messages(session).unwrap();
    let hang = || Answer::Hang(vec![route()]);
    let fake = FakeGateway::start(vec![hang(), hang(), hang(), text_step(MODEL_SUMMARY)]);
    let mut compressor = compressor(&fake, Duration::from_millis(200));
    let mut compact = |at: i64| {
        compressor
            .compact(&mut journal, session, WINDOW, at, &Interrupt::new())
            .unwrap()
    };

    let mut failed_at = NOW;
    let first = compact(failed_at);
    assert_eq!(
        first,
        Compression::Aborted {
            reason: Some("gateway timed out".into())
        }
    );
    for (requests, seconds) in [(1, 60), (2, 300), (3, 900)] {
        assert_eq!(
            compact(failed_at + seconds - 1),
            Compression::Aborted { reason: None }
        );
        assert_eq!(turns(&fake).len(), requests);
        failed_at += seconds;
        let next = compact(failed_at);
        assert_eq!(turns(&fake).len(), requests + 1);
        if requests < 3 {
            assert!(matches!(next, Compression::Aborted { reason: Some(_) }));
        } else {
            assert!(matches!(
                next,
                Compression::Compacted {
                    summary: Summary::Model,
                    ..
                }
            ));
        }
    }
    drop(compact);
    let live = journal.messages(session).unwrap();
    assert_eq!(live[..3], before[..3]);
    assert_ne!(live, before);
}

#[test]
fn the_one_liner_renders_list_arguments_as_python_and_null_ones_as_the_default() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
    let session = journal.session(KEY).unwrap().id;
    for n in 1..=14 {
        let command = match n {
            2 => json!(["make", "step2"]),
            3 => json!(null),
            n => json!(format!("make step{n}")),
        };
        append_call(&mut journal, session, n, &command);
    }
    let fake = FakeGateway::start(vec![text_step(MODEL_SUMMARY)]);

    compressor(&fake, SLOW)
        .compact(&mut journal, session, WINDOW, NOW, &Interrupt::new())
        .unwrap();

    let text = prompt(&turns(&fake)[0]);
    assert!(text.contains(
        "[TOOL RESULT c2]: [terminal] ran `['make', 'step2']` -> exit ?, 1 lines output"
    ));
    assert!(text.contains("[TOOL RESULT c3]: [terminal] ran `` -> exit ?, 1 lines output"));
}

#[test]
fn an_interrupted_summary_turn_leaves_the_session_unchanged() {
    let dir = Scratch::new();
    let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 14);
    let before = journal.messages(session).unwrap();
    let fake = FakeGateway::start(vec![text_step(MODEL_SUMMARY)]);
    let interrupt = Interrupt::new();
    interrupt.interrupt();

    let result = compressor(&fake, SLOW).compact(&mut journal, session, WINDOW, NOW, &interrupt);

    assert!(matches!(result, Err(Error::Interrupted)));
    assert_eq!(journal.messages(session).unwrap(), before);
}

#[test]
fn a_short_session_is_left_as_it_is_without_a_summary_turn() {
    let dir = Scratch::new();
    let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 1);
    let before = journal.messages(session).unwrap();
    let fake = FakeGateway::start(Vec::new());

    let result = compressor(&fake, SLOW)
        .compact(&mut journal, session, WINDOW, NOW, &Interrupt::new())
        .unwrap();

    assert_eq!(result, Compression::Skipped(Skip::TooFewMessages));
    assert_eq!(journal.messages(session).unwrap(), before);
    assert_eq!(turns(&fake).len(), 0);
}
