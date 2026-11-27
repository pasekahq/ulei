//! The proactive prune of old tool results between compactions (ADR-0098): its trigger, the
//! protected tail of rows, the result floor, the reclaim gate and the runway in memory, and a
//! turn after tool calls whose route has no window, sending the pruned set without a model turn
//! and storing it with its runway.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::path::Path;
use std::time::Duration;

use fake_gateway::{
    FakeGateway, Recorder, Scratch, request, search_step, text_block, text_step, turns,
};
use serde_json::json;
use ulei_engine::agent::{Agent, DEFAULT_CONCURRENT_TOOL_TIMEOUT, Ended, Outcome, Step, Template};
use ulei_engine::compress::{Compaction, Compressor, Prune, Settings};
use ulei_engine::gateway::Client;
use ulei_engine::journal::Journal;
use ulei_engine::policy::{Chain, Origin};
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{Interrupt, Registry};
use ulei_engine::wire::c4g::{Block, Message, Role, StopReason, TurnRequest, Usage};
use ulei_engine::zone::Zone;

const SLOW: Duration = Duration::from_secs(2);
const KEY: &str = "main";
const NOW: i64 = 1_800_000_000;
/// The prune of the owner's profile: 120000 tokens, 8000 characters, 4096 tokens.
const PRUNE: Prune = Prune {
    tokens: 120_000,
    min_result_chars: 8000,
    min_reclaim_tokens: 4096,
};
/// Characters of a tool result the prune replaces.
const BIG: usize = 12_000;

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

fn settings(prune: Prune) -> Settings {
    Settings {
        route: "compact".into(),
        timeout: Some(SLOW),
        max_tokens: 16_384,
        cache: None,
        protect_first_n: 3,
        protect_last_n: 20,
        target_ratio: 0.2,
        threshold: 0.3,
        threshold_tokens: Some(300_000),
        prune,
    }
}

fn compressor(fake: &FakeGateway, prune: Prune) -> Compressor {
    Compressor::new(
        Client::new(fake.socket(), SLOW, SLOW),
        settings(prune),
        Redactor { enabled: true },
        Zone::utc(),
    )
}

/// The `terminal` output of call `n`: `chars` characters, distinct per call.
fn output(n: usize, chars: usize) -> String {
    let head = format!("step {n:02} output: ");
    let tail = "x".repeat(chars - head.len());
    format!("{head}{tail}")
}

/// Exchange `n`: the prompt, a `terminal` call of `make stepN`, its result of `chars`
/// characters and a closing reply; four rows.
fn exchange(n: usize, chars: usize) -> Vec<Message> {
    let id = format!("c{n}");
    vec![
        user(&format!("Please run step {n}.")),
        assistant(vec![
            text_block(&format!("Running step {n}.")),
            block(
                json!({"type": "tool_use", "id": id, "name": "terminal", "input": {"command": format!("make step{n}")}}),
            ),
        ]),
        Message {
            role: Role::User,
            content: vec![block(
                json!({"type": "tool_result", "id": id, "content": output(n, chars), "is_error": false}),
            )],
        },
        assistant(vec![text_block(&format!("Step {n} is done."))]),
    ]
}

/// Exchanges `from..=to`, each with a result of `chars` characters.
fn exchanges(from: usize, to: usize, chars: usize) -> Vec<Message> {
    (from..=to).flat_map(|n| exchange(n, chars)).collect()
}

/// The result of call `cN` in `messages`.
fn result_of(messages: &[Message], n: usize) -> String {
    let id = format!("c{n}");
    messages
        .iter()
        .flat_map(|m| &m.content)
        .find(|b| b.id.as_deref() == Some(id.as_str()) && b.content.is_some())
        .and_then(|b| b.content.clone())
        .unwrap()
}

/// A request whose prompt size the reading of the last step gives.
fn request_of(messages: &[Message]) -> TurnRequest {
    TurnRequest {
        route: "chat".into(),
        system: None,
        messages: messages.to_vec(),
        tools: None,
        max_tokens: 1024,
        effort: None,
        session_key: None,
        cache: None,
        request_id: String::new(),
    }
}

fn usage(input: u64) -> Usage {
    Usage {
        input,
        output: 5,
        cache_read: 0,
        cache_write: 0,
    }
}

/// `compressor` after a step whose prompt was `input` tokens.
fn reading(mut compressor: Compressor, input: u64) -> Compressor {
    compressor.observe(&usage(input), None);
    compressor
}

#[test]
fn old_tool_results_outside_the_last_twenty_rows_become_one_liners_without_a_model_turn() {
    let fake = FakeGateway::start(vec![]);
    let mut compressor = reading(compressor(&fake, PRUNE), 130_000);
    let messages = exchanges(1, 10, BIG);

    let pruned = compressor.prune(&request_of(&messages), &messages).unwrap();

    assert_eq!(pruned.len(), messages.len());
    for n in 1..=5 {
        let line = result_of(&pruned, n);
        assert!(
            line.starts_with(&format!("[terminal] ran `make step{n}`")),
            "{n}: {line}"
        );
    }
    for n in 6..=10 {
        assert_eq!(result_of(&pruned, n), output(n, BIG), "{n}");
    }
    let changed: Vec<usize> = (0..messages.len())
        .filter(|&i| pruned[i] != messages[i])
        .collect();
    assert_eq!(changed, vec![2, 6, 10, 14, 18]);
    assert!(fake.requests().is_empty());
}

#[test]
fn the_prune_waits_until_the_rows_regrew_by_the_trigger_after_the_last_one() {
    let fake = FakeGateway::start(vec![]);
    let mut compressor = reading(compressor(&fake, PRUNE), 130_000);
    let messages = exchanges(1, 10, BIG);
    assert_eq!(compressor.prune_rearm(), 0);

    let pruned = compressor.prune(&request_of(&messages), &messages).unwrap();
    let after = u64::try_from(ulei_engine::compress::messages_tokens_rough(&pruned)).unwrap();
    let rearm = compressor.prune_rearm();
    assert!(rearm > 120_000, "{rearm}");
    assert!(rearm < after + 120_000 + 1000, "{rearm} {after}");

    let mut grown = pruned.clone();
    grown.extend(exchanges(11, 16, BIG));
    assert_eq!(compressor.prune(&request_of(&grown), &grown), None);
    assert_eq!(compressor.prune_rearm(), rearm);

    compressor.restore_prune_rearm(0);
    let again = compressor.prune(&request_of(&grown), &grown).unwrap();
    for n in 6..=11 {
        assert!(result_of(&again, n).starts_with("[terminal] ran"), "{n}");
    }
    assert_eq!(result_of(&again, 12), output(12, BIG));
}

#[test]
fn a_prompt_under_the_trigger_or_an_unset_trigger_prunes_nothing() {
    let fake = FakeGateway::start(vec![]);
    let messages = exchanges(1, 10, BIG);
    let mut under = reading(compressor(&fake, PRUNE), 119_999);
    assert_eq!(under.prune(&request_of(&messages), &messages), None);
    let mut off = reading(compressor(&fake, Prune::OFF), 1_000_000);
    assert_eq!(off.prune(&request_of(&messages), &messages), None);
    let mut short = reading(compressor(&fake, PRUNE), 130_000);
    let few = exchanges(1, 6, BIG);
    assert_eq!(short.prune(&request_of(&few), &few), None);
    assert_eq!(short.prune_rearm(), 0);
}

#[test]
fn results_up_to_the_floor_stay_and_a_prune_under_the_reclaim_minimum_is_not_kept() {
    let fake = FakeGateway::start(vec![]);
    let mut messages = exchange(1, 9_000);
    messages.extend(exchange(2, 8_000));
    messages.extend(exchanges(3, 10, 150));

    let mut gated = reading(compressor(&fake, PRUNE), 130_000);
    assert_eq!(gated.prune(&request_of(&messages), &messages), None);
    assert_eq!(gated.prune_rearm(), 0);

    let mut any = reading(
        compressor(
            &fake,
            Prune {
                min_reclaim_tokens: 0,
                ..PRUNE
            },
        ),
        130_000,
    );
    let pruned = any.prune(&request_of(&messages), &messages).unwrap();
    assert!(result_of(&pruned, 1).starts_with("[terminal] ran `make step1`"));
    assert_eq!(result_of(&pruned, 2), output(2, 8_000));
}

#[test]
fn an_equal_result_in_the_tail_leaves_a_back_reference_in_its_older_copy() {
    let fake = FakeGateway::start(vec![]);
    let mut compressor = reading(compressor(&fake, PRUNE), 130_000);
    let mut messages = exchanges(1, 8, BIG);
    let mut twin = exchange(9, BIG);
    let mut last = exchange(10, BIG);
    twin[2].content[0].content = Some(output(10, BIG));
    last[2].content[0].content = Some(output(10, BIG));
    messages.extend(twin);
    messages.extend(last);

    let pruned = compressor.prune(&request_of(&messages), &messages).unwrap();
    assert!(result_of(&pruned, 9).starts_with("[Duplicate tool output"));
    assert_eq!(result_of(&pruned, 10), output(10, BIG));
}

/// Appends exchanges `from..=to` as one stored turn each.
fn store(journal: &mut Journal, session: u64, from: usize, to: usize) {
    for n in from..=to {
        let messages = exchange(n, BIG);
        let turn = journal.next_turn(session).unwrap();
        let outcome = Outcome {
            messages: messages[1..].to_vec(),
            steps: vec![Step {
                request_id: turn.request_ids().unwrap().step(0),
                route: None,
                usage: None,
            }],
            ended: Ended::Stopped(StopReason::End),
            compacted: None,
        };
        journal.append(turn, &messages[..1], &outcome, NOW).unwrap();
    }
}

/// Appends a short turn whose one step reported `input` prompt tokens.
fn store_usage(journal: &mut Journal, session: u64, input: u64) {
    let turn = journal.next_turn(session).unwrap();
    let outcome = Outcome {
        messages: vec![assistant(vec![text_block("Fine.")])],
        steps: vec![Step {
            request_id: turn.request_ids().unwrap().step(0),
            route: None,
            usage: Some(usage(input)),
        }],
        ended: Ended::Stopped(StopReason::End),
        compacted: None,
    };
    journal
        .append(turn, &[user("Status?")], &outcome, NOW)
        .unwrap();
}

/// One turn of `prompt` as `ulei-engine run` runs it on a route without a window: the
/// compressor bound to the session with its stored runway and last usage, then journaled.
fn turn(journal: &mut Journal, fake: &FakeGateway, prompt: &str, cwd: &Path) -> Outcome {
    let session = journal.session(KEY).unwrap().id;
    let turn = journal.next_turn(session).unwrap();
    let ids = turn.request_ids().unwrap();
    let mut compressor = compressor(fake, PRUNE);
    compressor.bind(session);
    compressor.restore_prune_rearm(journal.prune_rearm(session).unwrap());
    if let Some(usage) = journal.last_usage(session).unwrap() {
        compressor.observe(&usage, None);
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
        cache: None,
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
            compressor: &mut compressor,
            window: None,
            clock: &clock,
        }),
        context: None,
        images: &[],
        interim: None,
        trace: None,
    }
    .run(
        &history,
        &|n| ids.step(n),
        &Interrupt::new(),
        16,
        &mut Recorder::default(),
    );
    journal.append(turn, &[user(prompt)], &out, NOW).unwrap();
    out
}

#[test]
fn a_step_after_tool_calls_on_a_route_without_a_window_sends_and_stores_the_pruned_set() {
    let dir = Scratch::new();
    let cwd = Scratch::new();
    let mut journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
    let session = journal.session(KEY).unwrap().id;
    store(&mut journal, session, 1, 10);
    store_usage(&mut journal, session, 130_000);
    let before = journal.messages(session).unwrap();
    let fake = FakeGateway::start(vec![search_step("s1"), text_step("Done.")]);

    let out = turn(&mut journal, &fake, "Look around.", &cwd.0);

    let sent = turns(&fake);
    assert_eq!(sent.len(), 2, "no summary turn");
    let first = request(&sent[0]);
    assert_eq!(first.messages[..before.len()], before[..]);
    let second = request(&sent[1]);
    for n in 1..=6 {
        assert!(
            result_of(&second.messages, n).starts_with(&format!("[terminal] ran `make step{n}`")),
            "{n}"
        );
    }
    for n in 7..=10 {
        assert_eq!(result_of(&second.messages, n), output(n, BIG), "{n}");
    }
    assert!(matches!(out.ended, Ended::Stopped(StopReason::End)));

    let compacted = out.compacted.clone().unwrap();
    assert_eq!(compacted.after, 2);
    assert_eq!(second.messages, compacted.live);
    assert!(compacted.prune_rearm > 120_000);
    assert_eq!(journal.prune_rearm(session).unwrap(), compacted.prune_rearm);
    let mut live = compacted.live.clone();
    live.extend(out.messages[compacted.after..].iter().cloned());
    assert_eq!(journal.messages(session).unwrap(), live);
    let archived = journal
        .records(session)
        .unwrap()
        .into_iter()
        .filter(|r| r.compacted == Some(true))
        .count();
    assert_eq!(archived, before.len() + 3);

    store(&mut journal, session, 11, 14);
    store_usage(&mut journal, session, 130_000);
    let fake = FakeGateway::start(vec![search_step("s2"), text_step("Done again.")]);
    let history = journal.messages(session).unwrap();
    let out = turn(&mut journal, &fake, "Once more.", &cwd.0);
    let sent = turns(&fake);
    assert_eq!(sent.len(), 2);
    assert_eq!(request(&sent[1]).messages[..history.len()], history[..]);
    assert!(out.compacted.is_none(), "the runway holds the next prune");
    assert_eq!(journal.prune_rearm(session).unwrap(), compacted.prune_rearm);
}

#[test]
fn a_compaction_removes_the_stored_runway() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
    let session = journal.session(KEY).unwrap().id;
    store(&mut journal, session, 1, 2);
    assert_eq!(journal.prune_rearm(session).unwrap(), 0);
    let turn = journal.next_turn(session).unwrap();
    let outcome = Outcome {
        messages: vec![assistant(vec![text_block("Fine.")])],
        steps: vec![],
        ended: Ended::Stopped(StopReason::End),
        compacted: Some(ulei_engine::agent::Compacted {
            live: exchange(1, 300),
            after: 0,
            prune_rearm: 150_000,
        }),
    };
    journal.append(turn, &[user("Go.")], &outcome, NOW).unwrap();
    assert_eq!(journal.prune_rearm(session).unwrap(), 150_000);

    journal.compact(session, &[user("Summary.")], NOW).unwrap();
    assert_eq!(journal.prune_rearm(session).unwrap(), 0);
}
