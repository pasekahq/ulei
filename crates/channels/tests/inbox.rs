//! The `inbox` thread of `ulei-tg` against a fake C6 engine on a Unix socket and the fake Bot
//! API: it holds `inbox_turns` for the owner's direct chat, reads each handed-over turn
//! quietly, sends a consumed answer once and reports the delivery.

#[allow(
    dead_code,
    reason = "the other test crates use other parts of the shared support"
)]
mod support {
    pub(crate) mod fake_bot_api;
    pub(crate) mod fake_c6;
    pub(crate) mod fake_clock;
}

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::fake_bot_api::{self, FakeBotApi, Recorded};
use support::fake_c6::{Answer, FakeC6, Gate, error, ok};
use support::fake_clock::FakeClock;
use ulei_channels::bot_api::{Client as BotClient, Config, Sender};
use ulei_channels::c6::Client;
use ulei_channels::render::{SendOptions, plan_reply};
use ulei_channels::tg::{Inbox, Outbound, SentTexts};

const OWNER: i64 = 42;
const OTHER_OWNER: i64 = 43;
const KEY: &str = "agent:main:telegram:dm:42";
const OTHER_KEY: &str = "agent:main:telegram:dm:43";
/// The wait before a failed connect or a lost hold is tried again, less scheduling slack.
const RETRY_AT_LEAST: Duration = Duration::from_millis(1900);

/// A socket path of its own in the temporary directory, removed on drop.
struct Socket(PathBuf);

impl Socket {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        Self(std::env::temp_dir().join(format!("inbox-{}-{n}.sock", std::process::id())))
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The inbox and delivery threads of `ulei-tg` over the fakes.
struct Harness {
    bot: FakeBotApi,
    outbound: Outbound,
    inbox: Inbox,
    socket: Socket,
}

impl Harness {
    /// Starts the threads for `owners`, before the engine listens.
    fn new(owners: &[i64], call_timeout: Duration) -> Self {
        let bot = FakeBotApi::start();
        let mut config = Config::new("123456:TEST", Some(&bot.base_url()));
        config.read_timeout = Duration::from_secs(2);
        let clock = Arc::new(FakeClock::new(Duration::ZERO));
        let sender = Sender::new(BotClient::new(config), clock as Arc<_>);
        let outbound = Outbound::start(Arc::new(sender), SentTexts::new()).expect("outbound");
        let socket = Socket::new();
        let client = Client::new(&socket.0, Duration::from_secs(1), call_timeout).expect("client");
        let inbox = Inbox::start(client, outbound.outbox(), owners).expect("inbox");
        Self {
            bot,
            outbound,
            inbox,
            socket,
        }
    }

    fn engine(&self) -> FakeC6 {
        FakeC6::start(&self.socket.0)
    }

    /// Stops both threads; every request Telegram got.
    fn finish(self) -> Vec<Recorded> {
        self.inbox.finish().expect("inbox finished");
        self.outbound.finish().expect("outbound finished");
        self.bot.requests()
    }
}

/// The engine of a started harness with `turns` scripted, each handed over in order and read
/// in one page, and every report stored.
fn engine_with(h: &Harness, turns: &[Answer]) -> FakeC6 {
    let engine = h.engine();
    for (n, page) in turns.iter().enumerate() {
        engine.script("inbox_turns", handed(&format!("t-{n}")));
        engine.script("turn_events", page.clone());
        engine.script("report_delivery", ok(json!({ "ok": true })));
    }
    engine
}

fn handed(turn: &str) -> Answer {
    ok(json!({ "turn": turn }))
}

/// A `turn_events` page of `turn` in `state` with `events`.
fn page(turn: &str, state: &str, events: &[Value]) -> Answer {
    ok(json!({ "turn": turn, "state": state, "events": events }))
}

fn start() -> Value {
    json!({ "seq": 1, "type": "start" })
}

fn text(seq: u64, kind: &str, text: &str) -> Value {
    json!({ "seq": seq, "type": kind, "text": text })
}

fn end(seq: u64, outcome: &str, line: Option<&str>) -> Value {
    let mut event = json!({ "seq": seq, "type": "end", "outcome": outcome });
    if let Some(line) = line {
        event["line"] = json!(line);
    }
    event
}

/// The whole event list of turn `n` that answers `answer` and ends with `outcome`.
fn answered(n: usize, answer: &str, outcome: &str) -> Answer {
    page(
        &format!("t-{n}"),
        "ended",
        &[start(), text(2, "final", answer), end(3, outcome, None)],
    )
}

/// The deliveries reported so far, as `(turn, delivery)` of the owner's session.
fn reports(engine: &FakeC6) -> Vec<(String, String)> {
    engine
        .calls()
        .into_iter()
        .filter(|(method, _)| method == "report_delivery")
        .map(|(_, body)| {
            assert_eq!(body["session"], json!(KEY));
            let field = |name: &str| body[name].as_str().expect(name).to_owned();
            (field("turn"), field("delivery"))
        })
        .collect()
}

/// `(turn, delivery)` of turns `t-0`, `t-1`, ... with `deliveries`.
fn expected(deliveries: &[&str]) -> Vec<(String, String)> {
    deliveries
        .iter()
        .enumerate()
        .map(|(n, delivery)| (format!("t-{n}"), (*delivery).to_owned()))
        .collect()
}

fn methods(requests: &[Recorded]) -> Vec<&str> {
    requests.iter().map(Recorded::method).collect()
}

#[test]
fn a_consumed_answer_is_sent_once_ringing_to_no_message_and_reported_sent() {
    let h = Harness::new(&[OWNER], Duration::from_secs(2));
    h.bot.script(
        "sendMessage",
        fake_bot_api::ok(json!({ "message_id": 500 })),
    );
    h.bot.script(
        "sendMessage",
        fake_bot_api::ok(json!({ "message_id": 501 })),
    );
    let engine = engine_with(
        &h,
        &[
            page(
                "t-0",
                "ended",
                &[
                    start(),
                    text(2, "interim", "Смотрю почту"),
                    text(3, "final", "Пришло письмо от банка"),
                    end(4, "completed", None),
                ],
            ),
            answered(1, "Задача закрыта", "halted"),
        ],
    );
    engine.wait_holds(3);
    let requests = h.finish();
    assert_eq!(reports(&engine), expected(&["sent", "sent"]));
    assert_eq!(methods(&requests), ["sendMessage", "sendMessage"]);
    let options = SendOptions::default();
    for (request, answer) in requests
        .iter()
        .zip(["Пришло письмо от банка", "Задача закрыта"])
    {
        let plan = plan_reply(answer, None, true, &options);
        assert_eq!(
            request.json(),
            json!({ "chat_id": OWNER, "text": plan.messages[0].text, "parse_mode": "MarkdownV2" })
        );
    }
    assert_eq!(engine.holds(), vec![json!({ "session": KEY }); 3]);
}

#[test]
fn silence_markers_send_nothing_and_no_reply_done_alone_is_silent_done() {
    let h = Harness::new(&[OWNER], Duration::from_secs(2));
    let finals = [
        "NO_REPLY_DONE",
        "  NO_REPLY_DONE\n",
        "*no_reply_done*",
        "NO_REPLY",
        "[SILENT]",
    ];
    let turns: Vec<Answer> = (0..)
        .zip(finals)
        .map(|(n, answer)| answered(n, answer, "completed"))
        .collect();
    let engine = engine_with(&h, &turns);
    engine.wait_holds(turns.len() + 1);
    let requests = h.finish();
    assert_eq!(
        reports(&engine),
        expected(&["silent_done", "silent_done", "silent", "silent", "silent"])
    );
    assert_eq!(methods(&requests), Vec::<&str>::new());
}

#[test]
fn a_turn_that_did_not_consume_its_event_sends_nothing_and_reports_no_answer() {
    let h = Harness::new(&[OWNER], Duration::from_secs(2));
    let ended = |n: usize, outcome: &str, line: Option<&str>| {
        page(
            &format!("t-{n}"),
            "ended",
            &[start(), text(2, "final", "Готово"), end(3, outcome, line)],
        )
    };
    let turns = [
        ended(0, "failed", Some("gateway error")),
        ended(1, "context_exhausted", Some("context window exceeded")),
        ended(
            2,
            "quota_exhausted",
            Some("main: quota resets at 12:00 UTC"),
        ),
        page("t-3", "ended", &[start(), end(2, "interrupted", None)]),
        ended(4, "max_steps", None),
        page("t-5", "ended", &[start(), end(2, "completed", None)]),
        answered(6, " \n\t", "completed"),
        answered(7, " (empty) ", "completed"),
        answered(
            8,
            "⚠️ The model returned no response after processing tool results.",
            "completed",
        ),
        answered(
            9,
            "⚠️ The model produced only internal reasoning",
            "completed",
        ),
        answered(10, "\n❌ Model returned no content", "halted"),
    ];
    let engine = engine_with(&h, &turns);
    engine.wait_holds(turns.len() + 1);
    let requests = h.finish();
    assert_eq!(reports(&engine), expected(&["no_answer"; 11]));
    assert_eq!(methods(&requests), Vec::<&str>::new());
}

#[test]
fn a_part_telegram_answers_without_a_message_id_reports_send_failed_without_a_retry() {
    let h = Harness::new(&[OWNER], Duration::from_secs(2));
    h.bot.script("sendMessage", fake_bot_api::ok(json!({})));
    h.bot.script(
        "sendMessage",
        fake_bot_api::ok(json!({ "message_id": 500 })),
    );
    h.bot.script("sendMessage", fake_bot_api::ok(json!({})));
    let long = "слово ".repeat(1000);
    assert_eq!(
        plan_reply(&long, None, true, &SendOptions::default())
            .messages
            .len(),
        2
    );
    let engine = engine_with(
        &h,
        &[
            answered(0, "Готово", "completed"),
            answered(1, &long, "completed"),
        ],
    );
    engine.wait_holds(3);
    let requests = h.finish();
    assert_eq!(reports(&engine), expected(&["send_failed", "send_failed"]));
    assert_eq!(
        methods(&requests),
        ["sendMessage", "sendMessage", "sendMessage"]
    );
}

#[test]
fn a_read_that_ends_without_end_sends_and_reports_nothing_and_the_hold_is_taken_again() {
    let h = Harness::new(&[OWNER], Duration::from_secs(2));
    let engine = engine_with(
        &h,
        &[
            page("t-0", "unknown", &[]),
            page("t-1", "dropped", &[json!({ "seq": 1, "type": "dropped" })]),
            Answer::Close,
        ],
    );
    engine.wait_holds(4);
    let requests = h.finish();
    assert_eq!(reports(&engine), []);
    assert_eq!(methods(&requests), Vec::<&str>::new());
}

#[test]
fn the_held_call_outlasts_the_call_timeout() {
    let h = Harness::new(&[OWNER], Duration::from_millis(200));
    let engine = h.engine();
    let held = Gate::new();
    engine.script("inbox_turns", Answer::Held(held.clone()));
    engine.script("turn_events", answered(0, "NO_REPLY_DONE", "completed"));
    engine.script("report_delivery", ok(json!({ "ok": true })));
    engine.wait_holds(1);
    thread::sleep(Duration::from_millis(600));
    held.open(handed("t-0"));
    engine.wait_holds(2);
    h.finish();
    assert_eq!(reports(&engine), expected(&["silent_done"]));
}

#[test]
fn a_refused_key_ends_its_hold_and_the_next_owner_is_held() {
    let h = Harness::new(&[OTHER_OWNER, OWNER], Duration::from_secs(2));
    let engine = h.engine();
    engine.script(
        "inbox_turns",
        error(400, "bad_request", "session is not the owner session"),
    );
    engine.wait_holds(2);
    h.finish();
    assert_eq!(
        engine.holds(),
        [json!({ "session": OTHER_KEY }), json!({ "session": KEY })]
    );
}

#[test]
fn an_engine_not_yet_listening_is_held_two_seconds_later() {
    let started = Instant::now();
    let h = Harness::new(&[OWNER], Duration::from_secs(2));
    thread::sleep(Duration::from_millis(300));
    let engine = h.engine();
    engine.wait_holds(1);
    assert!(
        started.elapsed() >= RETRY_AT_LEAST,
        "{:?}",
        started.elapsed()
    );
    h.finish();
}

#[test]
fn a_lost_hold_is_taken_again_two_seconds_later() {
    let h = Harness::new(&[OWNER], Duration::from_secs(2));
    let engine = h.engine();
    engine.script("inbox_turns", Answer::Close);
    engine.wait_holds(1);
    let lost = Instant::now();
    engine.wait_holds(2);
    assert!(lost.elapsed() >= RETRY_AT_LEAST, "{:?}", lost.elapsed());
    h.finish();
}
