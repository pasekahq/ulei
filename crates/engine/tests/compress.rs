//! Compaction of a long session on the scripted fake gateway: the summary turn on its alias,
//! the pruned, kept and stored messages, the request after a restart, the update of an earlier
//! summary, the fallback summary with its cooldown, the threshold, and the turns that compact
//! before a step or after `context_too_long`.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::path::Path;
use std::time::Duration;

use fake_gateway::{
    Answer, FakeGateway, InterruptMidWait, Recorder, Scratch, error_event, gateway_error, line,
    request, route, stop, text_block, text_step, turns,
};
use serde_json::json;
use ulei_engine::agent::{
    Agent, COMPRESSION_RETRY_WAIT, DEFAULT_CONCURRENT_TOOL_TIMEOUT, Ended, Outcome, Sleeper, Step,
    Template,
};
use ulei_engine::compress::text::{SUMMARY_END_MARKER, SUMMARY_PREFIX};
use ulei_engine::compress::{
    Compaction, Compression, Compressor, Error, Settings, Skip, Summary, Window,
};
use ulei_engine::gateway::Client;
use ulei_engine::journal::Journal;
use ulei_engine::policy::{Chain, Origin};
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{Interrupt, Registry};
use ulei_engine::wire::c4g::{
    Block, BlockType, ErrorClass, Message, Role, StopReason, TurnRequest, Usage,
};
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
        threshold: 0.3,
        threshold_tokens: Some(300_000),
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
        compacted: None,
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
        compaction: None,
        context: None,
        interim: None,
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
fn a_route_no_vendor_answers_stores_the_fallback_summary_and_holds_the_next_call_for_60_seconds() {
    let error = Answer::Events(vec![
        route(),
        error_event(gateway_error(ErrorClass::Unavailable)),
    ]);
    let (first, _, after) = cooldown_rung(error, SLOW, 60);
    assert_fallback(&first, &after, "gateway error unavailable: no message");
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

#[test]
fn the_threshold_is_the_reference_share_of_the_input_budget_with_its_floors_and_cap() {
    let cap = Some(300_000);
    // (context length, max_tokens, threshold, cap, threshold tokens)
    let table = [
        (1_000_000, 16_384, 0.3, cap, 295_084),
        (2_000_000, 16_384, 0.3, cap, 300_000),
        (2_000_000, 16_384, 0.3, None, 595_084),
        (512_000, 16_384, 0.3, cap, 148_684),
        (500_000, 16_384, 0.3, cap, 300_000),
        (200_000, 16_384, 0.3, cap, 137_712),
        (200_000, 16_384, 0.9, cap, 165_254),
        (100_000, 16_384, 0.3, Some(50_000), 50_000),
        (64_000, 16_384, 0.3, cap, 40_473),
        (64_000, 1_024, 0.3, cap, 53_529),
        (200_000, 65_536, 0.3, cap, 100_848),
        (32_000, 64_000, 0.3, cap, 27_200),
    ];
    for (context_length, max_tokens, threshold, cap, want) in table {
        assert_eq!(
            Window::new(context_length, max_tokens, threshold, cap),
            Window {
                context_length,
                threshold_tokens: want
            },
            "{context_length} {max_tokens} {threshold} {cap:?}"
        );
    }
}

/// A window whose threshold no rough count of these sessions reaches.
fn wide() -> Window {
    Window::new(200_000, 1024, 0.3, Some(300_000))
}

fn too_long() -> Answer {
    Answer::Events(vec![
        route(),
        error_event(gateway_error(ErrorClass::ContextTooLong)),
    ])
}

/// Appends a short turn whose one step reported `input` prompt tokens.
fn append_usage_turn(journal: &mut Journal, session: u64, input: u64) {
    let turn = journal.next_turn(session).unwrap();
    let outcome = Outcome {
        messages: vec![assistant(vec![text_block("Fine.")])],
        steps: vec![Step {
            request_id: turn.request_id(0),
            route: None,
            usage: Some(Usage {
                input,
                output: 5,
                cache_read: 0,
                cache_write: 0,
            }),
        }],
        ended: Ended::Stopped(StopReason::End),
        compacted: None,
    };
    journal.append(turn, &[user("Status?")], &outcome).unwrap();
}

/// One turn of `prompt` on the key's session as `ulei-engine run` runs it: the compressor bound
/// to the session and given its last step usage, compaction on `window`, then journaled.
fn compacting_turn(
    journal: &mut Journal,
    compressor: &mut Compressor,
    fake: &FakeGateway,
    window: Option<Window>,
    prompt: &str,
    cwd: &Path,
    sleeper: &mut dyn Sleeper,
) -> Outcome {
    let session = journal.session(KEY).unwrap().id;
    let turn = journal.next_turn(session).unwrap();
    compressor.bind(session);
    if let Some(usage) = journal.last_usage(session).unwrap() {
        compressor.observe(&usage, window);
    }
    let mut history = journal.messages(session).unwrap();
    history.push(user(prompt));
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
    let clock = || NOW;
    let out = Agent {
        client: &client,
        tools: &tools,
        chain: &mut chain,
        cwd,
        template: &template,
        batch_timeout: Some(DEFAULT_CONCURRENT_TOOL_TIMEOUT),
        empty_guard: true,
        origin: Origin::Other,
        compaction: Some(Compaction {
            compressor,
            window,
            clock: &clock,
        }),
        context: None,
        interim: None,
    }
    .run(
        &history,
        &|n| turn.request_id(n),
        &Interrupt::new(),
        16,
        sleeper,
    );
    journal.append(turn, &[user(prompt)], &out).unwrap();
    out
}

/// `(seq, turn, active, compacted)` of every stored message of `session`.
fn flags(journal: &Journal, session: u64) -> Vec<(u64, u64, bool, bool)> {
    journal
        .records(session)
        .unwrap()
        .into_iter()
        .map(|r| (r.seq, r.turn, r.active.unwrap(), r.compacted.unwrap()))
        .collect()
}

#[test]
fn a_session_whose_last_step_reached_the_threshold_compacts_before_the_next_step_and_replays() {
    let dir = Scratch::new();
    let cwd = Scratch::new();
    let path = dir.path("journal.sqlite3");
    let (mut journal, session) = long_session(&path, 14);
    append_usage_turn(&mut journal, session, WINDOW.threshold_tokens);
    let before = journal.messages(session).unwrap();
    let stored = before.len();
    let fake = FakeGateway::start(vec![text_step(MODEL_SUMMARY), text_step("Next answer.")]);
    let mut compressor = compressor(&fake, SLOW);

    let out = compacting_turn(
        &mut journal,
        &mut compressor,
        &fake,
        Some(WINDOW),
        "What next?",
        &cwd.0,
        &mut Recorder::default(),
    );

    let sent = turns(&fake);
    assert_eq!(sent.len(), 2);
    assert_eq!(request(&sent[0]).route, "compact");
    let step = request(&sent[1]);
    assert_eq!(
        (step.route.as_str(), step.request_id.as_str()),
        ("chat", "s1-t16-0")
    );
    let compacted = out.compacted.clone().unwrap();
    assert_eq!(compacted.after, 0);
    assert_eq!(step.messages, compacted.live);
    assert_eq!(compacted.live[..3], before[..3]);
    assert_eq!(compacted.live.last(), Some(&user("What next?")));
    assert!(summary_of(&compacted.live).contains("## Goal\nKeep the build notes current."));
    assert!(matches!(out.ended, Ended::Stopped(StopReason::End)));

    let mut live = compacted.live.clone();
    live.extend(out.messages[compacted.after..].iter().cloned());
    assert_eq!(journal.messages(session).unwrap(), live);
    let rows = flags(&journal, session);
    assert!(rows[..=stored].iter().all(|r| !r.2 && r.3));
    assert_eq!(rows[stored], (stored as u64, 16, false, true));
    assert!(rows[stored + 1..].iter().all(|r| r.1 == 16 && r.2 && !r.3));
    assert_eq!(rows.len(), stored + 1 + live.len());

    let never_stopped = next_turn(&journal, session, &live, &cwd.0);
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
    assert_eq!(request(&restarted[0]).request_id, "s1-t17-0");
}

#[test]
fn a_session_under_the_threshold_or_without_a_window_never_sends_a_summary_request() {
    let dir = Scratch::new();
    let cwd = Scratch::new();
    for (input, window) in [
        (WINDOW.threshold_tokens - 1, Some(WINDOW)),
        (WINDOW.threshold_tokens * 10, None),
    ] {
        let (mut journal, session) =
            long_session(&dir.path(&format!("journal-{input}.sqlite3")), 14);
        append_usage_turn(&mut journal, session, input);
        let fake = FakeGateway::start(vec![text_step("One."), text_step("Two.")]);
        let mut compressor = compressor(&fake, SLOW);

        for prompt in ["First?", "Second?"] {
            let out = compacting_turn(
                &mut journal,
                &mut compressor,
                &fake,
                window,
                prompt,
                &cwd.0,
                &mut Recorder::default(),
            );
            assert_eq!(out.compacted, None);
        }

        let routes: Vec<String> = turns(&fake).iter().map(|t| request(t).route).collect();
        assert_eq!(routes, ["chat", "chat"]);
    }
}

#[test]
fn a_step_answered_context_too_long_is_compacted_and_retried_once() {
    let dir = Scratch::new();
    let cwd = Scratch::new();
    let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 14);
    append_usage_turn(&mut journal, session, 100);
    let mut before = journal.messages(session).unwrap();
    before.push(user("What next?"));
    let fake = FakeGateway::start(vec![
        too_long(),
        text_step(MODEL_SUMMARY),
        text_step("Fits now."),
    ]);
    let mut compressor = compressor(&fake, SLOW);
    let mut recorder = Recorder::default();

    let out = compacting_turn(
        &mut journal,
        &mut compressor,
        &fake,
        Some(wide()),
        "What next?",
        &cwd.0,
        &mut recorder,
    );

    let sent: Vec<_> = turns(&fake).iter().map(|t| request(t)).collect();
    let ids: Vec<(&str, &str)> = sent
        .iter()
        .map(|r| (r.route.as_str(), r.request_id.as_str()))
        .collect();
    assert_eq!(ids[0], ("chat", "s1-t16-0"));
    assert_eq!(ids[1].0, "compact");
    assert_eq!(ids[2], ("chat", "s1-t16-1"));
    assert_eq!(ids.len(), 3);
    assert_eq!(sent[0].messages, before);
    let compacted = out.compacted.clone().unwrap();
    assert_eq!(sent[2].messages, compacted.live);
    assert!(compacted.live.len() < before.len());
    assert_eq!(recorder.0, [COMPRESSION_RETRY_WAIT]);
    assert!(matches!(out.ended, Ended::Stopped(StopReason::End)));
    let steps: Vec<&str> = out.steps.iter().map(|s| s.request_id.as_str()).collect();
    assert_eq!(steps, ["s1-t16-0", "s1-t16-1"]);
    assert_eq!(out.messages, [assistant(vec![text_block("Fits now.")])]);
}

#[test]
fn an_interrupt_during_the_compression_retry_wait_ends_the_turn_at_once() {
    let dir = Scratch::new();
    let cwd = Scratch::new();
    let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 14);
    append_usage_turn(&mut journal, session, 100);
    let fake = FakeGateway::start(vec![
        too_long(),
        text_step(MODEL_SUMMARY),
        text_step("unreachable"),
    ]);
    let mut compressor = compressor(&fake, SLOW);
    let mut waits = InterruptMidWait::default();

    let out = compacting_turn(
        &mut journal,
        &mut compressor,
        &fake,
        Some(wide()),
        "What next?",
        &cwd.0,
        &mut waits,
    );

    assert!(matches!(out.ended, Ended::Interrupted), "{out:?}");
    let (asked, took) = waits.0[0];
    assert_eq!((asked, waits.0.len()), (COMPRESSION_RETRY_WAIT, 1));
    assert!(took < COMPRESSION_RETRY_WAIT / 4, "{took:?}");
    let routes: Vec<String> = turns(&fake).iter().map(|t| request(t).route).collect();
    assert_eq!(routes, ["chat", "compact"]);
}

#[test]
fn a_second_context_too_long_ends_the_turn_with_one_line_and_no_further_request() {
    let dir = Scratch::new();
    let cwd = Scratch::new();
    let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 14);
    append_usage_turn(&mut journal, session, 100);
    let fake = FakeGateway::start(vec![
        too_long(),
        text_step(MODEL_SUMMARY),
        too_long(),
        text_step("unreachable"),
    ]);
    let mut compressor = compressor(&fake, SLOW);

    let out = compacting_turn(
        &mut journal,
        &mut compressor,
        &fake,
        Some(wide()),
        "What next?",
        &cwd.0,
        &mut Recorder::default(),
    );

    let Ended::ContextExhausted { line, error } = &out.ended else {
        panic!("{:?}", out.ended);
    };
    assert_eq!(
        line,
        "Context length exceeded: max compression attempts (1) reached."
    );
    assert_eq!(out.ended.to_string(), *line);
    assert_eq!(error.class, ErrorClass::ContextTooLong);
    assert_eq!(turns(&fake).len(), 3);
    assert_eq!(out.steps.len(), 2);
    let compacted = out.compacted.clone().unwrap();
    assert_eq!(journal.messages(session).unwrap(), compacted.live);

    let short = FakeGateway::start(vec![too_long(), text_step("unreachable")]);
    let mut journal = Journal::open(&dir.path("short.sqlite3")).unwrap();
    let session = journal.session(KEY).unwrap().id;
    append_usage_turn(&mut journal, session, 100);
    let out = compacting_turn(
        &mut journal,
        &mut compressor,
        &short,
        Some(wide()),
        "What next?",
        &cwd.0,
        &mut Recorder::default(),
    );
    let Ended::ContextExhausted { line, .. } = &out.ended else {
        panic!("{:?}", out.ended);
    };
    assert!(line.starts_with("Context length exceeded ("), "{line}");
    assert!(
        line.ends_with(" tokens). Cannot compress further."),
        "{line}"
    );
    assert_eq!(turns(&short).len(), 1);
    assert_eq!(out.compacted, None);
}

#[test]
fn a_compaction_inside_the_first_turn_is_stored_by_its_append_with_its_ids_and_seq() {
    let dir = Scratch::new();
    let cwd = Scratch::new();
    let path = dir.path("journal.sqlite3");
    let mut journal = Journal::open(&path).unwrap();
    let session = journal.session(KEY).unwrap().id;
    let mut answers: Vec<Answer> = (1..=8)
        .map(|n| {
            fake_gateway::tool_step(vec![fake_gateway::call(
                &format!("c{n}"),
                "write_file",
                json!({"path": format!("n{n}.txt"), "content": format!("{n}\n")}),
            )])
        })
        .collect();
    answers.extend([too_long(), text_step(MODEL_SUMMARY), text_step("Done.")]);
    let fake = FakeGateway::start(answers);
    let mut compressor = compressor(&fake, SLOW);

    let out = compacting_turn(
        &mut journal,
        &mut compressor,
        &fake,
        Some(wide()),
        "Write eight files.",
        &cwd.0,
        &mut Recorder::default(),
    );

    assert_eq!(turns(&fake).len(), 11);
    assert!(matches!(out.ended, Ended::Stopped(StopReason::End)));
    let compacted = out.compacted.clone().unwrap();
    assert_eq!(compacted.after, 16);
    assert_eq!(out.messages.len(), 17);
    let ids: Vec<(u64, u64, String)> = journal
        .steps(session)
        .unwrap()
        .into_iter()
        .map(|s| (s.turn, s.step, s.request_id))
        .collect();
    let want: Vec<(u64, u64, String)> = (0..10).map(|n| (1, n, format!("s1-t1-{n}"))).collect();
    assert_eq!(ids, want);

    let rows = flags(&journal, session);
    let live = compacted.live.len() + 1;
    assert_eq!(rows.len(), 17 + live);
    for (i, row) in rows.iter().enumerate() {
        let archived = i < 17;
        assert_eq!(*row, (i as u64, 1, !archived, archived));
    }
    let records = journal.records(session).unwrap();
    assert_eq!(
        serde_json::from_value::<Message>(serde_json::Value::Object(records[0].message.clone()))
            .unwrap(),
        user("Write eight files.")
    );
    let mut want = compacted.live.clone();
    want.push(assistant(vec![text_block("Done.")]));
    assert_eq!(journal.messages(session).unwrap(), want);
    assert!(tool_pairs_whole(&want));
    assert_eq!(journal.next_turn(session).unwrap().number, 2);

    drop(journal);
    let journal = Journal::open(&path).unwrap();
    let next = next_turn(
        &journal,
        session,
        &journal.messages(session).unwrap(),
        &cwd.0,
    );
    let body = request(&next[0]);
    assert_eq!(body.request_id, "s1-t2-0");
    want.push(user("What next?"));
    assert_eq!(body.messages, want);
}

#[test]
fn a_compressor_keeps_no_summary_count_or_cooldown_of_another_session() {
    let dir = Scratch::new();
    let path = dir.path("journal.sqlite3");
    let (mut journal, first) = long_session(&path, 14);
    let second = journal.new_session("other").unwrap().id;
    for n in 1..=14 {
        append_turn(&mut journal, second, n);
    }
    let before = journal.messages(second).unwrap();
    let fake = FakeGateway::start(vec![text_step(MODEL_SUMMARY), text_step(MODEL_SUMMARY)]);
    let mut running = compressor(&fake, SLOW);

    running
        .compact(&mut journal, first, WINDOW, NOW, &Interrupt::new())
        .unwrap();
    let other = running
        .compact(&mut journal, second, WINDOW, NOW, &Interrupt::new())
        .unwrap();

    assert!(matches!(
        other,
        Compression::Compacted {
            summary: Summary::Model,
            ..
        }
    ));
    let sent = turns(&fake);
    assert!(!prompt(&sent[1]).contains("PREVIOUS SUMMARY"));
    assert!(!prompt(&sent[1]).contains("Keep the build notes current."));
    assert_eq!(journal.messages(second).unwrap()[..3], before[..3]);

    let dir = Scratch::new();
    let (mut journal, first) = long_session(&dir.path("journal.sqlite3"), 14);
    let second = journal.new_session("other").unwrap().id;
    for n in 1..=14 {
        append_turn(&mut journal, second, n);
    }
    let empty = Answer::Events(vec![route(), stop(StopReason::End)]);
    let fake = FakeGateway::start(vec![empty, text_step(MODEL_SUMMARY)]);
    let mut running = compressor(&fake, SLOW);
    running
        .compact(&mut journal, first, WINDOW, NOW, &Interrupt::new())
        .unwrap();
    let other = running
        .compact(&mut journal, second, WINDOW, NOW + 1, &Interrupt::new())
        .unwrap();
    assert!(matches!(
        other,
        Compression::Compacted {
            summary: Summary::Model,
            ..
        }
    ));
    assert_eq!(turns(&fake).len(), 2);
}

#[test]
fn exhausted_quotas_in_a_row_abort_and_hold_the_next_call_for_60_then_300_seconds() {
    let dir = Scratch::new();
    let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 14);
    let before = journal.messages(session).unwrap();
    let quota = || Answer::Reject(429, gateway_error(ErrorClass::QuotaExhausted));
    let fake = FakeGateway::start(vec![quota(), quota(), text_step(MODEL_SUMMARY)]);
    let mut compressor = compressor(&fake, SLOW);
    let mut compact = |at: i64| {
        compressor
            .compact(&mut journal, session, WINDOW, at, &Interrupt::new())
            .unwrap()
    };

    assert!(matches!(
        compact(NOW),
        Compression::Aborted { reason: Some(_) }
    ));
    assert_eq!(compact(NOW + 59), Compression::Aborted { reason: None });
    assert!(matches!(
        compact(NOW + 60),
        Compression::Aborted { reason: Some(_) }
    ));
    assert_eq!(turns(&fake).len(), 2);
    assert_eq!(compact(NOW + 359), Compression::Aborted { reason: None });
    assert_eq!(turns(&fake).len(), 2);
    assert!(matches!(
        compact(NOW + 360),
        Compression::Compacted {
            summary: Summary::Model,
            ..
        }
    ));
    drop(compact);
    assert_ne!(journal.messages(session).unwrap(), before);
}

fn prompt_of(input: u64) -> Usage {
    Usage {
        input,
        output: 5,
        cache_read: 0,
        cache_write: 0,
    }
}

/// A step request; the gate reads the last reported usage, not this.
fn step_request() -> TurnRequest {
    TurnRequest {
        route: "chat".into(),
        system: None,
        messages: vec![user("Next?")],
        tools: None,
        max_tokens: 1024,
        effort: None,
        session_key: None,
        cache: None,
        request_id: "s1-t1-0".into(),
    }
}

#[test]
fn compactions_that_leave_the_prompt_over_the_threshold_twice_hold_compaction_back_until_a_probe() {
    let dir = Scratch::new();
    let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 14);
    let fake = FakeGateway::start(vec![
        text_step(MODEL_SUMMARY),
        text_step(MODEL_SUMMARY),
        text_step(MODEL_SUMMARY),
    ]);
    let mut running = compressor(&fake, SLOW);
    let (request, high) = (step_request(), prompt_of(WINDOW.threshold_tokens));
    running.observe(&high, Some(WINDOW));
    assert!(running.should_compress(&request, WINDOW, NOW));

    for n in [15, 25] {
        running
            .compact(&mut journal, session, WINDOW, NOW, &Interrupt::new())
            .unwrap();
        assert!(!running.should_compress(&request, WINDOW, NOW));
        running.observe(&high, Some(WINDOW));
        for m in n..n + 10 {
            append_turn(&mut journal, session, m);
        }
    }
    assert!(!running.should_compress(&request, WINDOW, NOW));
    assert!(!running.should_compress(&request, WINDOW, NOW + 299));
    assert!(running.should_compress(&request, WINDOW, NOW + 300));

    running
        .compact(&mut journal, session, WINDOW, NOW + 300, &Interrupt::new())
        .unwrap();
    running.observe(&prompt_of(WINDOW.threshold_tokens - 1), Some(WINDOW));
    running.observe(&high, Some(WINDOW));
    assert!(running.should_compress(&request, WINDOW, NOW + 300));
    assert_eq!(turns(&fake).len(), 3);
}

#[test]
fn fallback_compactions_hold_compaction_back_during_their_cooldown_and_after_two_in_a_row() {
    let dir = Scratch::new();
    let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 14);
    let empty = || Answer::Events(vec![route(), stop(StopReason::End)]);
    let fake = FakeGateway::start(vec![empty(), empty()]);
    let mut running = compressor(&fake, SLOW);
    let (request, high) = (step_request(), prompt_of(WINDOW.threshold_tokens));

    for at in [NOW, NOW + 60] {
        let first = running
            .compact(&mut journal, session, WINDOW, at, &Interrupt::new())
            .unwrap();
        assert!(matches!(
            first,
            Compression::Compacted {
                summary: Summary::Fallback { reason: Some(_) },
                ..
            }
        ));
        running.observe(&prompt_of(1), Some(WINDOW));
        running.observe(&high, Some(WINDOW));
        assert!(!running.should_compress(&request, WINDOW, at + 59));
        for n in 0..10 {
            append_turn(&mut journal, session, 100 + n);
        }
    }
    assert!(!running.should_compress(&request, WINDOW, NOW + 120));
    assert!(!running.should_compress(&request, WINDOW, NOW + 419));
    assert!(running.should_compress(&request, WINDOW, NOW + 420));
    assert_eq!(turns(&fake).len(), 2);
}

/// A step that calls `write_file` once, reporting `usage` when given.
fn write_step(usage: Option<u64>) -> Answer {
    let mut events = vec![
        route(),
        fake_gateway::call(
            "w1",
            "write_file",
            json!({"path": "w.txt", "content": "w\n"}),
        ),
    ];
    events.extend(usage.map(fake_gateway::usage));
    events.push(stop(StopReason::ToolUse));
    Answer::Events(events)
}

#[test]
fn a_step_whose_usage_fits_after_a_compaction_lets_a_later_overflow_compact_and_retry() {
    let cwd = Scratch::new();
    for (usage, requests) in [(Some(3), 5), (None, 3)] {
        let dir = Scratch::new();
        let (mut journal, session) = long_session(&dir.path("journal.sqlite3"), 14);
        append_usage_turn(&mut journal, session, WINDOW.threshold_tokens);
        let fake = FakeGateway::start(vec![
            text_step(MODEL_SUMMARY),
            write_step(usage),
            too_long(),
            text_step(MODEL_SUMMARY),
            text_step("Done."),
        ]);
        let mut compressor = compressor(&fake, SLOW);
        let mut recorder = Recorder::default();

        let out = compacting_turn(
            &mut journal,
            &mut compressor,
            &fake,
            Some(WINDOW),
            "Write it.",
            &cwd.0,
            &mut recorder,
        );

        let sent: Vec<_> = turns(&fake).iter().map(|t| request(t)).collect();
        let ids: Vec<(&str, &str)> = sent
            .iter()
            .map(|r| (r.route.as_str(), r.request_id.as_str()))
            .take(3)
            .collect();
        assert_eq!(
            ids.iter().map(|i| i.0).collect::<Vec<_>>(),
            ["compact", "chat", "chat"]
        );
        assert_eq!((ids[1].1, ids[2].1), ("s1-t16-0", "s1-t16-1"));
        assert_eq!(sent.len(), requests, "usage {usage:?}");
        if usage.is_some() {
            assert_eq!(sent[3].route, "compact");
            assert_eq!(sent[4].request_id, "s1-t16-2");
            assert_eq!(sent[4].messages, out.compacted.clone().unwrap().live);
            assert_eq!(recorder.0, [COMPRESSION_RETRY_WAIT]);
            assert!(matches!(out.ended, Ended::Stopped(StopReason::End)));
        } else {
            let Ended::ContextExhausted { line, .. } = &out.ended else {
                panic!("{:?}", out.ended);
            };
            assert_eq!(
                line,
                "Context length exceeded: max compression attempts (1) reached."
            );
        }
    }
}
