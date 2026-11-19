//! The engine link of `ulei-tg` against a fake C6 engine on a Unix socket and the fake Bot API:
//! turns and commands reach their C6 methods, replies and failures reach the owner.

#[allow(
    dead_code,
    reason = "tests/bot_api.rs and tests/tg.rs use other parts of the shared support"
)]
mod support {
    pub(crate) mod fake_bot_api;
    pub(crate) mod fake_c6;
    pub(crate) mod fake_clock;
}

use std::fs;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::fake_bot_api::{self, FakeBotApi, Recorded};
use support::fake_c6::{Answer, FakeC6, error, ok};
use support::fake_clock::FakeClock;
use ulei_channels::bot_api::{Client as BotClient, Config, Sender};
use ulei_channels::c6::Client;
use ulei_channels::ingress::{Command, MessageKind, Output, Turn, Zone};
use ulei_channels::render::{SendOptions, plan_reply};
use ulei_channels::tg::{Dispatch, ENGINE_FAILED, Link, Outbound, SentTexts};

const CHAT: i64 = 1001;
const KEY: &str = "agent:main:telegram:dm:1001";
const SINCE: f64 = 1_790_003_600.0;

/// A socket path of its own in the temporary directory, removed on drop.
struct Socket(PathBuf);

impl Socket {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        Self(std::env::temp_dir().join(format!("c6-{}-{n}.sock", std::process::id())))
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The link and delivery threads of `ulei-tg` over the fakes.
struct Harness {
    bot: FakeBotApi,
    outbound: Outbound,
    link: Link,
    socket: Socket,
}

impl Harness {
    fn new(call_timeout: Duration) -> Self {
        let bot = FakeBotApi::start();
        let mut config = Config::new("123456:TEST", Some(&bot.base_url()));
        config.read_timeout = Duration::from_secs(2);
        let sender = Sender::new(
            BotClient::new(config),
            Arc::new(FakeClock::new(Duration::ZERO)),
        );
        let outbound = Outbound::start(Arc::new(sender), SentTexts::new()).expect("outbound");
        let socket = Socket::new();
        let client = Client::new(&socket.0, Duration::from_secs(1), call_timeout).expect("client");
        let link = Link::start(client, outbound.outbox(), Zone::utc()).expect("link");
        Self {
            bot,
            outbound,
            link,
            socket,
        }
    }

    fn engine(&self) -> FakeC6 {
        FakeC6::start(&self.socket.0)
    }

    fn turn(&mut self, message_id: i64, text: &str, context: Option<&str>) {
        self.link.dispatch(Output::Turn(Turn {
            chat_id: CHAT,
            message_id,
            update_id: message_id,
            date: 1_790_000_000,
            kind: MessageKind::Text,
            text: text.to_owned(),
            context: context.map(str::to_owned),
            attachments: Vec::new(),
            reply: None,
            internal: false,
        }));
    }

    fn command(&mut self, message_id: i64, command: Command) {
        self.link.dispatch(Output::Command {
            chat_id: CHAT,
            message_id,
            command,
        });
    }

    /// Waits until `count` messages went out.
    fn wait_for_sent(&self, count: usize) {
        let until = Instant::now() + Duration::from_secs(5);
        while self.bot.calls("sendMessage").len() < count {
            assert!(Instant::now() < until, "fewer than {count} messages sent");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Stops both threads once their queues are done; the messages sent as `(reply_to, text)`.
    fn finish(self) -> Vec<(i64, String)> {
        self.link.finish().expect("link finished");
        self.outbound.finish().expect("outbound finished");
        self.bot
            .calls("sendMessage")
            .iter()
            .map(Recorded::json)
            .map(|m| {
                (
                    m["reply_parameters"]["message_id"]
                        .as_i64()
                        .expect("reply target"),
                    m["text"].as_str().expect("text").to_owned(),
                )
            })
            .collect()
    }
}

/// The text Telegram gets for the reply `markdown` to `reply_to`.
fn sent(reply_to: i64, markdown: &str) -> (i64, String) {
    let plan = plan_reply(markdown, Some(reply_to), true, &SendOptions::default());
    (reply_to, plan.messages[0].text.clone())
}

fn script_sends(bot: &FakeBotApi, count: i64) {
    for id in 0..count {
        bot.script(
            "sendMessage",
            fake_bot_api::ok(json!({ "message_id": 500 + id })),
        );
    }
}

fn accepted(position: u64) -> Answer {
    ok(json!({
        "accepted": true,
        "turn": format!("t-{position}"),
        "position": position,
        "queue_depth": position,
    }))
}

fn status(running: bool, queue_depth: u64, last_turn: bool) -> Answer {
    let mut body = json!({
        "session": KEY,
        "running": running,
        "since": SINCE,
        "queue_depth": queue_depth,
        "route": "main",
    });
    if last_turn {
        body["last_turn"] = json!({ "turn": "t-0", "state": "ended" });
    }
    ok(body)
}

fn interrupted(yes: bool) -> Answer {
    let mut body = json!({ "interrupted": yes, "queue_depth": 0 });
    if yes {
        body["turn"] = json!("t-0");
    }
    ok(body)
}

fn bodies(engine: &FakeC6, method: &str) -> Vec<Value> {
    engine
        .calls()
        .into_iter()
        .filter(|(m, _)| m == method)
        .map(|(_, body)| body)
        .collect()
}

#[test]
fn a_text_turn_reaches_submit_turn_with_its_context_on_the_chats_session() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    engine.script("submit_turn", accepted(0));
    h.turn(7, "запиши задачу", Some("- 👍 на моё сообщение #5"));
    assert_eq!(h.finish(), []);
    assert_eq!(
        engine.calls(),
        [(
            "submit_turn".to_owned(),
            json!({
                "session": KEY,
                "prompt": "запиши задачу",
                "channel": "telegram",
                "context": "- 👍 на моё сообщение #5",
            })
        )]
    );
}

#[test]
fn a_second_turn_while_the_session_is_busy_is_submitted_for_the_engine_to_queue() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    engine.script("submit_turn", accepted(0));
    engine.script("submit_turn", accepted(1));
    h.turn(7, "first", None);
    h.turn(8, "second", None);
    assert_eq!(h.finish(), []);
    let prompts: Vec<Value> = bodies(&engine, "submit_turn")
        .into_iter()
        .map(|b| b["prompt"].clone())
        .collect();
    assert_eq!(prompts, [json!("first"), json!("second")]);
}

#[test]
fn new_stops_the_running_turn_and_the_next_turn_starts_a_new_session() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    engine.script("status", status(true, 0, true));
    engine.script("interrupt", interrupted(true));
    engine.script("submit_turn", accepted(0));
    engine.script("submit_turn", accepted(0));
    h.command(7, Command::New);
    h.turn(8, "привет", None);
    h.turn(9, "ещё", None);
    let sent_texts = h.finish();
    assert_eq!(
        sent_texts,
        [sent(7, "✨ Сеанс сброшен! Начинаем с чистого листа.")]
    );
    assert_eq!(
        engine.methods(),
        ["status", "interrupt", "submit_turn", "submit_turn"]
    );
    let turns = bodies(&engine, "submit_turn");
    assert_eq!(turns[0]["new"], json!(true));
    assert_eq!(turns[1].get("new"), None);
}

#[test]
fn stop_interrupts_the_running_turn() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    engine.script("interrupt", interrupted(true));
    h.command(7, Command::Stop);
    assert_eq!(
        h.finish(),
        [sent(7, "⚡ Остановлено. Вы можете продолжить этот сеанс.")]
    );
    assert_eq!(
        engine.calls(),
        [("interrupt".to_owned(), json!({ "session": KEY }))]
    );
}

#[test]
fn status_shows_the_session_state() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    engine.script("status", status(true, 1, false));
    h.command(7, Command::Status);
    assert_eq!(
        h.finish(),
        [sent(
            7,
            "📊 **Состояние канала**\n\n**ID сеанса:** `agent:main:telegram:dm:1001`\n\
             **Последняя активность:** 2026-09-21 15:13\n**Model:** `main`\n\
             **Агент активен:** Да ⚡\n**Очередь продолжений:** 1\n\n\
             **Подключённые платформы:** telegram"
        )]
    );
    assert_eq!(
        engine.calls(),
        [("status".to_owned(), json!({ "session": KEY }))]
    );
}

#[test]
fn model_with_an_alias_sets_the_route_of_the_session() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    engine.script("set_route", ok(json!({ "session": KEY, "route": "fast" })));
    h.command(7, Command::Model(Some("fast".to_owned())));
    assert_eq!(
        h.finish(),
        [sent(
            7,
            "Модель изменена на `fast`\n\
             _(только для этого сеанса — добавьте `--global`, чтобы сохранить)_"
        )]
    );
    assert_eq!(
        engine.calls(),
        [(
            "set_route".to_owned(),
            json!({ "session": KEY, "route": "fast" })
        )]
    );
}

#[test]
fn a_refused_alias_shows_the_engines_message() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    engine.script(
        "set_route",
        error(400, "bad_request", "body is not valid for the method input"),
    );
    h.command(7, Command::Model(Some("Fast!".to_owned())));
    assert_eq!(
        h.finish(),
        [sent(7, "Ошибка: body is not valid for the method input")]
    );
}

#[test]
fn engine_down_gives_one_notice_and_the_next_turn_goes_through_once_it_is_up() {
    let mut h = Harness::new(Duration::from_secs(2));
    script_sends(&h.bot, 1);
    h.turn(7, "first", None);
    h.wait_for_sent(1);
    let engine = h.engine();
    engine.script("submit_turn", accepted(0));
    h.turn(8, "second", None);
    assert_eq!(h.finish(), [sent(7, ENGINE_FAILED)]);
    assert_eq!(bodies(&engine, "submit_turn")[0]["prompt"], json!("second"));
}

#[test]
fn a_refused_connection_a_timeout_or_an_error_answer_gives_one_notice_each() {
    let mut h = Harness::new(Duration::from_millis(300));
    script_sends(&h.bot, 3);
    drop(UnixListener::bind(&h.socket.0).expect("stale socket"));
    h.turn(7, "refused", None);
    h.wait_for_sent(1);
    fs::remove_file(&h.socket.0).expect("remove the stale socket");
    let engine = h.engine();
    engine.script("submit_turn", Answer::Silent);
    engine.script("status", error(500, "internal", "journal read failed"));
    h.turn(8, "timed out", None);
    h.command(9, Command::Status);
    assert_eq!(
        h.finish(),
        [
            sent(7, ENGINE_FAILED),
            sent(8, ENGINE_FAILED),
            sent(9, ENGINE_FAILED)
        ]
    );
    assert_eq!(engine.methods(), ["submit_turn", "status"]);
}
