//! The engine link of `ulei-tg` against a fake C6 engine on a Unix socket and the fake Bot API:
//! turns and commands reach their C6 methods, each accepted turn is read to its end, and its
//! replies and failures reach the owner.

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
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use support::fake_bot_api::{self, FakeBotApi, Recorded, typing_calls};
use support::fake_c6::{Answer, FakeC6, Gate, Socket, end, error, ok, page, start, text};
use support::fake_clock::FakeClock;
use ulei_channels::bot_api::{Client as BotClient, Config, Sender};
use ulei_channels::c6::Client;
use ulei_channels::ingress::{Command, MessageKind, Output, Turn, Zone};
use ulei_channels::render::{SendOptions, TYPING_INTERVAL, plan_reply};
use ulei_channels::tg::{
    Dispatch, EMPTY_ANSWER, ENGINE_FAILED, Link, Outbound, SentTexts, spawn_vital,
};

const CHAT: i64 = 1001;
const OTHER_CHAT: i64 = 1002;
/// Chats whose typing keeps the typing thread busy.
const BUSY_CHATS: [i64; 4] = [1003, 1004, 1005, 1006];
const KEY: &str = "agent:main:telegram:dm:1001";
const SINCE: f64 = 1_790_003_600.0;

/// The link and delivery threads of `ulei-tg` over the fakes, on virtual time.
struct Harness {
    bot: FakeBotApi,
    clock: Arc<FakeClock>,
    outbound: Outbound,
    link: Link,
    socket: Socket,
}

impl Harness {
    fn new(call_timeout: Duration) -> Self {
        let bot = FakeBotApi::start();
        let mut config = Config::new("123456:TEST", Some(&bot.base_url()));
        config.read_timeout = Duration::from_secs(2);
        let clock = Arc::new(FakeClock::new(Duration::ZERO));
        let sender = Sender::new(BotClient::new(config), Arc::clone(&clock) as Arc<_>);
        let outbound = Outbound::start(Arc::new(sender), SentTexts::new()).expect("outbound");
        let socket = Socket::new("c6");
        let client = Client::new(&socket.0, Duration::from_secs(1), call_timeout).expect("client");
        let link = Link::start(client, outbound.outbox(), Zone::utc(), true).expect("link");
        Self {
            bot,
            clock,
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
            message_id: Some(message_id),
            update_id: message_id,
            date: 1_790_000_000,
            kind: MessageKind::Text,
            text: text.to_owned(),
            context: context.map(str::to_owned),
            attachments: Vec::new(),
            transcripts: Vec::new(),
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

    /// Waits until `count` typing actions went out to the chat.
    fn wait_for_typing(&self, count: usize) {
        self.bot
            .wait_until(&format!("{count} typing actions"), |r| {
                typing_calls(r, CHAT) >= count
            });
    }

    /// Stops both threads once their queues are done; the messages sent.
    fn finish(self) -> Vec<(Option<i64>, String)> {
        self.link.finish().expect("link finished");
        self.outbound.finish().expect("outbound finished");
        replies(&self.bot)
    }

    /// [`Self::finish`] that also shows the chat's typing stopped with its turns: once the
    /// readers are done, time passes and the chat gets no further `typing`.
    fn finish_with_typing_stopped(self) -> Vec<(Option<i64>, String)> {
        let Self {
            bot,
            clock,
            outbound,
            link,
            socket: _socket,
        } = self;
        let outbox = outbound.outbox();
        link.finish().expect("link finished");
        // The other chat's first action follows every typing job of the readers.
        outbox.turn_started(OTHER_CHAT).expect("queued");
        bot.wait_until("the other chat's typing", |r| {
            typing_calls(r, OTHER_CHAT) == 1
        });
        let typed = typing_calls(&bot.requests(), CHAT);
        outbox.turn_ended(OTHER_CHAT).expect("queued");
        clock.advance(TYPING_INTERVAL * 2);
        outbox.turn_ended(OTHER_CHAT).expect("queued");
        drop(outbox);
        outbound.finish().expect("outbound finished");
        assert_eq!(typing_calls(&bot.requests(), CHAT), typed);
        replies(&bot)
    }
}

/// The messages sent, as `(reply_to, text)`.
fn replies(bot: &FakeBotApi) -> Vec<(Option<i64>, String)> {
    bot.calls("sendMessage")
        .iter()
        .map(Recorded::json)
        .map(|m| {
            (
                m["reply_parameters"]["message_id"].as_i64(),
                m["text"].as_str().expect("text").to_owned(),
            )
        })
        .collect()
}

/// The text Telegram gets for the reply `markdown` to `reply_to`.
fn sent(reply_to: i64, markdown: &str) -> (Option<i64>, String) {
    let plan = plan_reply(markdown, Some(reply_to), true, &SendOptions::default());
    (Some(reply_to), plan.messages[0].text.clone())
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

/// The whole event list of a turn that ran to `completed` without an answer.
fn completed(turn: &str) -> Answer {
    page(
        turn,
        "ended",
        &[
            json!({ "seq": 1, "type": "start" }),
            json!({ "seq": 2, "type": "end", "outcome": "completed" }),
        ],
    )
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
    engine.script("turn_events", completed("t-0"));
    h.turn(7, "запиши задачу", Some("- 👍 на моё сообщение #5"));
    assert_eq!(h.finish(), []);
    assert_eq!(
        bodies(&engine, "submit_turn"),
        [json!({
            "session": KEY,
            "prompt": "запиши задачу",
            "channel": "telegram",
            "context": "- 👍 на моё сообщение #5",
            "message_id": 7,
        })]
    );
}

#[test]
fn a_reaction_event_sets_the_emoji_on_the_named_message_before_the_answer() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    engine.script("submit_turn", accepted(0));
    engine.script(
        "turn_events",
        page(
            "t-0",
            "ended",
            &[
                json!({ "seq": 1, "type": "start" }),
                json!({ "seq": 2, "type": "reaction", "text": "👍", "message_id": 7 }),
                json!({ "seq": 3, "type": "final", "text": "Записал." }),
                json!({ "seq": 4, "type": "end", "outcome": "completed" }),
            ],
        ),
    );
    h.turn(7, "запиши задачу", None);
    h.bot.wait_sent(1);
    let reactions: Vec<Value> = h
        .bot
        .calls("setMessageReaction")
        .iter()
        .map(Recorded::json)
        .collect();
    assert_eq!(
        reactions,
        [json!({
            "chat_id": CHAT,
            "message_id": 7,
            "reaction": [{ "type": "emoji", "emoji": "👍" }],
        })]
    );
    let order: Vec<String> = h
        .bot
        .requests()
        .iter()
        .map(|r| r.method().to_owned())
        .filter(|m| m == "setMessageReaction" || m == "sendMessage")
        .collect();
    assert_eq!(order, ["setMessageReaction", "sendMessage"]);
    assert_eq!(h.finish(), [sent(7, "Записал.")]);
}

#[test]
fn a_second_turn_while_the_session_is_busy_is_submitted_for_the_engine_to_queue() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    engine.script("submit_turn", accepted(0));
    engine.script("submit_turn", accepted(1));
    engine.script("turn_events", completed("t-0"));
    engine.script("turn_events", completed("t-1"));
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
    engine.script("turn_events", completed("t-0"));
    engine.script("turn_events", completed("t-0"));
    h.command(7, Command::New);
    h.turn(8, "привет", None);
    h.turn(9, "ещё", None);
    let sent_texts = h.finish();
    assert_eq!(
        sent_texts,
        [sent(7, "✨ Сеанс сброшен! Начинаем с чистого листа.")]
    );
    let methods: Vec<String> = engine
        .methods()
        .into_iter()
        .filter(|m| m != "turn_events")
        .collect();
    assert_eq!(
        methods,
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
    h.command(
        7,
        Command::Model {
            alias: Some("fast".to_owned()),
            global: false,
        },
    );
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
    h.command(
        7,
        Command::Model {
            alias: Some("Fast!".to_owned()),
            global: false,
        },
    );
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
    h.bot.wait_sent(1);
    let engine = h.engine();
    engine.script("submit_turn", accepted(0));
    engine.script("turn_events", completed("t-0"));
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
    h.bot.wait_sent(1);
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

/// The `after` of every `turn_events` request, with its turn.
fn cursors(engine: &FakeC6) -> Vec<(String, u64)> {
    bodies(engine, "turn_events")
        .iter()
        .map(|b| {
            (
                b["turn"].as_str().expect("turn").to_owned(),
                b["after"].as_u64().expect("after"),
            )
        })
        .collect()
}

#[test]
fn start_interim_final_and_end_reach_telegram_as_typing_the_interim_and_the_reply() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 2);
    let held = Gate::new();
    engine.script("submit_turn", accepted(0));
    engine.script("turn_events", page("t-0", "running", &[start()]));
    engine.script("turn_events", Answer::Held(held.clone()));
    engine.script(
        "turn_events",
        page(
            "t-0",
            "ended",
            &[text(3, "final", "**Готово**"), end(4, "completed", None)],
        ),
    );
    h.turn(7, "сделай", None);
    h.wait_for_typing(1);
    held.open(page(
        "t-0",
        "running",
        &[text(2, "interim", "Смотрю файлы")],
    ));
    h.bot.wait_sent(2);
    let requests: Vec<Recorded> = h
        .bot
        .requests()
        .into_iter()
        .filter(|r| r.method() != "getMe")
        .collect();
    let methods: Vec<&str> = requests.iter().map(Recorded::method).collect();
    assert_eq!(
        methods,
        [
            "sendChatAction",
            "sendMessage",
            "sendChatAction",
            "sendMessage"
        ]
    );
    assert_eq!(requests[1].json()["disable_notification"], json!(true));
    assert_eq!(requests[3].json().get("disable_notification"), None);
    assert_eq!(
        cursors(&engine),
        [
            ("t-0".to_owned(), 0),
            ("t-0".to_owned(), 1),
            ("t-0".to_owned(), 2)
        ]
    );
    let interim = plan_reply("Смотрю файлы", None, false, &SendOptions::default());
    assert_eq!(
        h.finish_with_typing_stopped(),
        [
            (None, interim.messages[0].text.clone()),
            sent(7, "**Готово**")
        ]
    );
}

#[test]
fn an_empty_final_sends_nothing() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    engine.script("submit_turn", accepted(0));
    engine.script("turn_events", completed("t-0"));
    h.turn(7, "ничего не отвечай", None);
    h.wait_for_typing(1);
    assert_eq!(h.finish_with_typing_stopped(), []);
}

#[test]
fn the_empty_stand_in_of_a_final_shows_the_no_response_notice() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    engine.script("submit_turn", accepted(0));
    engine.script(
        "turn_events",
        page(
            "t-0",
            "ended",
            &[
                start(),
                text(2, "final", "(empty)"),
                end(3, "completed", None),
            ],
        ),
    );
    h.turn(7, "сделай", None);
    h.bot.wait_sent(1);
    assert_eq!(h.finish(), [sent(7, EMPTY_ANSWER)]);
}

/// A turn of `turn` that answers `answer` and completes.
fn answered(turn: &str, answer: &str) -> Answer {
    page(
        turn,
        "ended",
        &[start(), text(2, "final", answer), end(3, "completed", None)],
    )
}

#[test]
fn the_answer_to_a_turn_without_a_message_is_not_a_reply() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    engine.script("submit_turn", accepted(0));
    engine.script("turn_events", answered("t-0", "Вижу реакцию"));
    h.link.dispatch(Output::Turn(Turn {
        chat_id: CHAT,
        message_id: None,
        update_id: 0,
        date: 0,
        kind: MessageKind::Text,
        text: "[reaction]".to_owned(),
        context: Some("Reactions:".to_owned()),
        attachments: Vec::new(),
        transcripts: Vec::new(),
        reply: None,
        internal: true,
    }));
    h.bot.wait_sent(1);
    let plan = plan_reply("Вижу реакцию", None, true, &SendOptions::default());
    assert_eq!(h.finish(), [(None, plan.messages[0].text.clone())]);
}

#[test]
fn a_final_that_is_a_silence_marker_sends_nothing_and_typing_stops() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    let finals = ["NO_REPLY", "[SILENT]", "no reply", "*NO_REPLY*", "готово"];
    for (n, answer) in (0..).zip(finals) {
        engine.script("submit_turn", accepted(n));
        engine.script("turn_events", answered(&format!("t-{n}"), answer));
    }
    for message_id in 7..12 {
        h.turn(message_id, "сделай", None);
    }
    // The turns are read in order, so the last one's answer comes after the four markers.
    h.bot.wait_sent(1);
    assert!(typing_calls(&h.bot.requests(), CHAT) >= 1);
    assert_eq!(h.finish_with_typing_stopped(), [sent(11, "готово")]);
}

#[test]
fn a_broken_marker_and_prose_that_mentions_one_are_delivered() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 2);
    let prose = "I would answer NO_REPLY if there were nothing to say.";
    engine.script("submit_turn", accepted(0));
    engine.script("submit_turn", accepted(1));
    engine.script("turn_events", answered("t-0", "[SILENT"));
    engine.script("turn_events", answered("t-1", prose));
    h.turn(7, "первый", None);
    h.turn(8, "второй", None);
    h.bot.wait_sent(2);
    assert_eq!(h.finish(), [sent(7, "[SILENT"), sent(8, prose)]);
}

#[test]
fn no_reply_done_sends_nothing_and_prose_that_mentions_it_is_delivered() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    let prose = "Answer NO_REPLY_DONE when the task needs no text.";
    engine.script("submit_turn", accepted(0));
    engine.script("submit_turn", accepted(1));
    engine.script("turn_events", answered("t-0", " NO_REPLY_DONE\n"));
    engine.script("turn_events", answered("t-1", prose));
    h.turn(7, "первый", None);
    h.turn(8, "второй", None);
    h.bot.wait_sent(1);
    assert_eq!(h.finish_with_typing_stopped(), [sent(8, prose)]);
}

#[test]
fn no_typing_reaches_telegram_after_the_final_answer() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    let held = Gate::new();
    engine.script("submit_turn", accepted(0));
    engine.script("turn_events", page("t-0", "running", &[start()]));
    engine.script("turn_events", Answer::Held(held.clone()));
    h.turn(7, "сделай", None);
    h.wait_for_typing(1);
    let outbox = h.outbound.outbox();
    for chat in BUSY_CHATS {
        outbox.turn_started(chat).expect("queued");
    }
    h.bot.wait_until("the busy chats' typing", |r| {
        BUSY_CHATS.iter().all(|&chat| typing_calls(r, chat) == 1)
    });
    // In the next tick every chat is due and each call is slow, so the final arrives while
    // the typing thread is still in it.
    for _ in 0..=BUSY_CHATS.len() {
        h.bot.script(
            "sendChatAction",
            fake_bot_api::Reply::Delayed(
                Duration::from_millis(200),
                Box::new(fake_bot_api::ok(json!(true))),
            ),
        );
    }
    h.clock.advance(TYPING_INTERVAL);
    outbox.turn_ended(OTHER_CHAT).expect("tick started");
    held.open(page(
        "t-0",
        "ended",
        &[text(2, "final", "готово"), end(3, "completed", None)],
    ));
    h.bot.wait_sent(1);
    for chat in BUSY_CHATS {
        outbox.turn_ended(chat).expect("ended");
    }
    drop(outbox);
    let Harness {
        bot,
        outbound,
        link,
        socket: _socket,
        ..
    } = h;
    link.finish().expect("link finished");
    outbound.finish().expect("outbound finished");
    assert_eq!(replies(&bot), [sent(7, "готово")]);
    let requests = bot.requests();
    let final_at = requests
        .iter()
        .position(|r| r.method() == "sendMessage")
        .expect("the final");
    assert_eq!(typing_calls(&requests[final_at..], CHAT), 0);
}

#[test]
fn quota_exhausted_gives_exactly_its_line_and_nothing_is_retried() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    let line = "quota exhausted on route main; nearest reset 2026-09-21 18:00:00 UTC";
    engine.script("submit_turn", accepted(0));
    engine.script(
        "turn_events",
        page(
            "t-0",
            "ended",
            &[start(), end(2, "quota_exhausted", Some(line))],
        ),
    );
    h.turn(7, "сделай", None);
    h.bot.wait_sent(1);
    assert_eq!(h.finish_with_typing_stopped(), [sent(7, line)]);
    assert_eq!(engine.methods(), ["submit_turn", "turn_events"]);
}

#[test]
fn a_failed_turn_shows_the_request_failed_text_with_its_line() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    engine.script("submit_turn", accepted(0));
    engine.script(
        "turn_events",
        page(
            "t-0",
            "ended",
            &[
                start(),
                end(2, "failed", Some("gateway error internal: upstream reset")),
            ],
        ),
    );
    h.turn(7, "сделай", None);
    h.bot.wait_sent(1);
    assert_eq!(
        h.finish(),
        [sent(
            7,
            "The request failed: gateway error internal: upstream reset\n\
             Try again or use /reset to start a fresh session."
        )]
    );
}

#[test]
fn stop_during_a_held_read_is_answered_at_once_and_the_turn_ends_with_typing_stopped() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    let held = Gate::new();
    engine.script("submit_turn", accepted(0));
    engine.script("turn_events", page("t-0", "running", &[start()]));
    engine.script("turn_events", Answer::Held(held.clone()));
    engine.script("interrupt", interrupted(true));
    h.turn(7, "долгая задача", None);
    h.wait_for_typing(1);
    engine.wait_for(3);
    h.command(8, Command::Stop);
    h.bot.wait_sent(1);
    held.open(page("t-0", "ended", &[end(2, "interrupted", None)]));
    assert_eq!(
        h.finish_with_typing_stopped(),
        [sent(8, "⚡ Остановлено. Вы можете продолжить этот сеанс.")]
    );
    assert_eq!(
        engine.methods(),
        ["submit_turn", "turn_events", "turn_events", "interrupt"]
    );
}

#[test]
fn two_queued_turns_each_get_their_own_reply_in_order() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 2);
    engine.script("submit_turn", accepted(0));
    engine.script("submit_turn", accepted(1));
    for (turn, answer) in [("t-0", "первый ответ"), ("t-1", "второй ответ")] {
        engine.script(
            "turn_events",
            page(
                turn,
                "ended",
                &[start(), text(2, "final", answer), end(3, "completed", None)],
            ),
        );
    }
    h.turn(7, "первый", None);
    h.turn(8, "второй", None);
    h.bot.wait_sent(2);
    assert_eq!(
        h.finish(),
        [sent(7, "первый ответ"), sent(8, "второй ответ")]
    );
    assert_eq!(
        cursors(&engine),
        [("t-0".to_owned(), 0), ("t-1".to_owned(), 0)]
    );
}

#[test]
fn the_engine_dropping_mid_turn_gives_one_notice_and_ends_typing() {
    let mut h = Harness::new(Duration::from_secs(2));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    engine.script("submit_turn", accepted(0));
    engine.script("turn_events", page("t-0", "running", &[start()]));
    engine.script("turn_events", Answer::Close);
    h.turn(7, "сделай", None);
    h.bot.wait_sent(1);
    assert_eq!(h.finish_with_typing_stopped(), [sent(7, ENGINE_FAILED)]);
    assert_eq!(
        engine.methods(),
        ["submit_turn", "turn_events", "turn_events"]
    );
}

#[test]
fn a_read_that_reaches_the_call_timeout_is_made_again_with_the_same_cursor() {
    let mut h = Harness::new(Duration::from_millis(300));
    let engine = h.engine();
    script_sends(&h.bot, 1);
    engine.script("submit_turn", accepted(0));
    engine.script("turn_events", page("t-0", "running", &[start()]));
    engine.script("turn_events", Answer::Silent);
    engine.script(
        "turn_events",
        page(
            "t-0",
            "ended",
            &[text(2, "final", "готово"), end(3, "completed", None)],
        ),
    );
    h.turn(7, "сделай", None);
    h.bot.wait_sent(1);
    assert_eq!(h.finish(), [sent(7, "готово")]);
    assert_eq!(
        cursors(&engine),
        [
            ("t-0".to_owned(), 0),
            ("t-0".to_owned(), 1),
            ("t-0".to_owned(), 1)
        ]
    );
}

/// Set in the child run of the test below, which re-runs it in a process of its own.
const PANIC_CHILD: &str = "ULEI_TG_TEST_PANIC_CHILD";

#[test]
fn a_panic_of_the_engine_thread_exits_the_process_with_status_one_and_one_line() {
    let name = "a_panic_of_the_engine_thread_exits_the_process_with_status_one_and_one_line";
    if std::env::var_os(PANIC_CHILD).is_some() {
        let worker = spawn_vital("engine", || panic!("synthetic worker fault")).unwrap();
        let joined = worker.join();
        panic!("the process outlived the panic: {joined:?}");
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(PANIC_CHILD, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    let line = "ulei-tg: engine thread panicked: synthetic worker fault";
    assert_eq!(stderr.lines().filter(|l| *l == line).count(), 1, "{stderr}");
}
