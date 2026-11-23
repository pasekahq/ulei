//! The owner's Telegram flow end to end: the fake Bot API drives the real `ulei-tg`, which runs
//! its turns over C6 on the real `ulei-engine serve`, whose model is the fake gateway.

#[allow(
    dead_code,
    reason = "the other test crates use other parts of the shared support"
)]
mod support {
    pub(crate) mod fake_bot_api;
    pub(crate) mod fake_gateway;
    pub(crate) mod files;
    pub(crate) mod process;
    pub(crate) mod stack;
}

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use support::fake_bot_api::{Recorded, Reply, fail, ok, texts, typing_calls};
use support::fake_gateway::{Answer, user_texts};
use support::files::saved;
use support::process::{dead, pid_file};
use support::stack::{CHAT, OWNER, OWNER_SESSION, STT_KEY, Stack};
use ulei_channels::c6::Client;
use ulei_channels::ingress::Zone;
use ulei_channels::render::{TYPING_INTERVAL, to_markdown_v2};
use ulei_contracts::generated::c4g::{Error as GatewayError, ErrorClass};
use ulei_contracts::generated::c6::{Delivery, SetRouteRequest, StatusRequest};

const DATE: i64 = 1_790_000_000;
const STRANGER: i64 = 77;
const JPEG: &[u8] = b"\xff\xd8\xff\xe0jpeg";
const OGG: &[u8] = b"OggS\x00\x02voice";
const ANSWER: &str = "Готово, записал";

/// An update of message `id` in the private chat of user `from` with `fields`; the owner's
/// private chat is [`CHAT`].
fn message_from(id: i64, from: i64, fields: &Value) -> Value {
    let chat = if from == OWNER { CHAT } else { from };
    let mut message = json!({
        "message_id": id,
        "date": DATE,
        "chat": { "id": chat, "type": "private" },
        "from": { "id": from, "is_bot": false, "first_name": "Owner" },
    });
    let fields = fields.as_object().expect("message fields").clone();
    message.as_object_mut().expect("message").extend(fields);
    json!({ "update_id": id, "message": message })
}

fn owner_message(id: i64, fields: &Value) -> Value {
    message_from(id, OWNER, fields)
}

fn owner_text(id: i64, text: &str) -> Value {
    owner_message(id, &json!({ "text": text }))
}

/// The owner's command `text`, marked as Telegram marks a leading command.
fn owner_command(id: i64, text: &str) -> Value {
    let name = text.split(' ').next().expect("command name");
    let entity =
        json!({ "type": "bot_command", "offset": 0, "length": name.encode_utf16().count() });
    owner_message(id, &json!({ "text": text, "entities": [entity] }))
}

/// Makes the next `sendMessage` calls succeed as messages `ids`.
fn sends(stack: &Stack, ids: &[i64]) {
    for id in ids {
        stack
            .bot
            .script("sendMessage", ok(json!({ "message_id": id })));
    }
}

/// Every `sendMessage` body so far.
fn sent(stack: &Stack) -> Vec<Value> {
    stack
        .bot
        .calls("sendMessage")
        .iter()
        .map(Recorded::json)
        .collect()
}

/// Scripts the download of `file_id` as `file_path` with `data`.
fn file(stack: &Stack, file_id: &str, file_path: &str, data: &[u8]) {
    stack.bot.script(
        "getFile",
        ok(json!({ "file_id": file_id, "file_size": data.len(), "file_path": file_path })),
    );
    let name = file_path.rsplit('/').next().expect("file name");
    stack.bot.script(name, Reply::Bytes(data.to_vec()));
}

fn voice_note(id: i64) -> Value {
    owner_message(
        id,
        &json!({ "voice": {
            "file_id": "voice-1", "file_unique_id": "v1", "duration": 7,
            "mime_type": "audio/ogg", "file_size": OGG.len()
        }}),
    )
}

#[test]
fn a_text_message_gets_the_models_answer_as_a_reply() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    sends(&stack, &[100]);
    stack.bot.queue_updates(&[owner_text(5, "Запиши молоко")]);
    stack.bot.wait_sent(1);
    stack.stop();
    let sent = sent(&stack);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["chat_id"], json!(CHAT));
    assert_eq!(sent[0]["text"], json!(ANSWER));
    assert_eq!(sent[0]["parse_mode"], json!("MarkdownV2"));
    assert_eq!(sent[0]["reply_parameters"]["message_id"], json!(5));
    assert_eq!(stack.gateway.prompts(), ["Запиши молоко"]);
}

#[test]
fn two_quick_messages_make_one_turn() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    sends(&stack, &[100]);
    stack
        .bot
        .queue_updates(&[owner_text(5, "Запиши молоко"), owner_text(6, "и хлеб")]);
    stack.bot.wait_sent(1);
    stack.stop();
    let sent = sent(&stack);
    assert_eq!(texts(&sent), [ANSWER]);
    assert_eq!(stack.gateway.prompts(), ["Запиши молоко\nи хлеб"]);
}

#[test]
fn a_photo_is_saved_privately_and_its_note_reaches_the_model() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    sends(&stack, &[100]);
    file(&stack, "photo-a", "photos/a.jpg", JPEG);
    let photo = json!({ "photo": [{ "file_id": "photo-a", "file_unique_id": "a", "width": 9, "height": 9 }] });
    stack.bot.queue_updates(&[owner_message(5, &photo)]);
    stack.bot.wait_sent(1);
    stack.stop();
    let path = saved(&stack.inbox(), JPEG);
    let mode = fs::metadata(&path).expect("meta").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        stack.gateway.prompts(),
        [format!(
            "[The user sent an image. It is saved at: {}. You cannot view images yet, so do not \
             describe its content.]",
            path.display()
        )]
    );
    assert_eq!(texts(&sent(&stack)), [ANSWER]);
}

#[test]
fn a_voice_note_is_echoed_then_answered() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    sends(&stack, &[100, 101]);
    file(&stack, "voice-1", "voice/file_1.oga", OGG);
    stack
        .bot
        .script("transcriptions", Reply::Bytes("купи молоко\n".into()));
    stack.bot.queue_updates(&[voice_note(5)]);
    stack.bot.wait_sent(2);
    stack.stop();
    let sent = sent(&stack);
    let upload = &stack.bot.calls("transcriptions")[0];
    assert_eq!(upload.headers["authorization"], format!("Bearer {STT_KEY}"));
    assert_eq!(texts(&sent), ["🎙️ \"купи молоко\"", ANSWER]);
    assert_eq!(sent[0]["disable_notification"], json!(true));
    assert_eq!(sent[0].get("reply_parameters"), None);
    assert_eq!(sent[1]["reply_parameters"]["message_id"], json!(5));
    assert_eq!(stack.gateway.prompts(), ["\"купи молоко\""]);
}

#[test]
fn a_voice_note_the_stt_fails_on_reaches_the_model_as_its_file_without_an_echo() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    sends(&stack, &[100]);
    file(&stack, "voice-1", "voice/file_1.oga", OGG);
    stack.bot.script(
        "transcriptions",
        Reply::Json(500, json!({ "error": { "message": "boom" } })),
    );
    stack.bot.queue_updates(&[voice_note(5)]);
    stack.bot.wait_sent(1);
    stack.stop();
    let path = saved(&stack.inbox(), OGG);
    assert_eq!(texts(&sent(&stack)), [ANSWER]);
    assert_eq!(
        stack.gateway.prompts(),
        [format!(
            "[voice message could not be transcribed automatically; the audio is available at: {}]",
            path.display()
        )]
    );
}

#[test]
fn a_forwarded_message_and_a_reply_reach_the_model_in_the_ingress_formats() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    stack.gateway.script(Answer::Text("Понял".into()));
    sends(&stack, &[100, 101]);
    let forwarded = json!({
        "text": "чужой текст",
        "forward_origin": {
            "type": "user",
            "date": DATE,
            "sender_user": { "id": 555, "is_bot": false, "first_name": "Test", "last_name": "Person", "username": "test_person" }
        }
    });
    stack.bot.queue_updates(&[owner_message(5, &forwarded)]);
    stack.bot.wait_sent(1);
    let reply = json!({
        "text": "а молоко?",
        "reply_to_message": {
            "message_id": 100,
            "date": DATE,
            "chat": { "id": CHAT, "type": "private" },
            "from": { "id": 7, "is_bot": true, "first_name": "Bot", "username": "test_bot" },
            "text": ANSWER
        }
    });
    stack.bot.queue_updates(&[owner_message(6, &reply)]);
    stack.bot.wait_sent(2);
    stack.stop();
    let sent = sent(&stack);
    assert_eq!(texts(&sent), [ANSWER, "Понял"]);
    assert_eq!(
        stack.gateway.prompts(),
        [
            format!(
                "[Переслано от Test Person (@test_person), {}]\nчужой текст",
                host_zone().local(DATE).dmy_hm()
            ),
            format!("[Replying to: \"{ANSWER}\"]\n\nа молоко?"),
        ]
    );
}

#[test]
fn a_long_answer_arrives_split_into_messages_within_the_limit() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text("слово ".repeat(1200)));
    sends(&stack, &[100, 101]);
    stack
        .bot
        .queue_updates(&[owner_text(5, "Расскажи подробно")]);
    stack.bot.wait_sent(2);
    stack.stop();
    let sent = sent(&stack);
    let parts = texts(&sent);
    assert_eq!(parts.len(), 2);
    for part in &parts {
        let units = part.encode_utf16().count();
        assert!(units <= 4096, "{units} UTF-16 units");
    }
    assert!(parts[0].ends_with("\\(1/2\\)"), "{}", parts[0]);
    assert!(parts[1].ends_with("\\(2/2\\)"), "{}", parts[1]);
    let words: usize = parts.iter().map(|t| t.matches("слово").count()).sum();
    assert_eq!(words, 1200);
    assert_eq!(sent[0]["reply_parameters"]["message_id"], json!(5));
    assert_eq!(sent[1].get("reply_parameters"), None);
}

#[test]
fn an_answer_whose_markup_is_rejected_is_resent_as_plain_text() {
    let mut stack = Stack::start();
    stack
        .gateway
        .script(Answer::Text("**Готово** к 1.5".into()));
    stack.bot.script(
        "sendMessage",
        fail(
            400,
            "Bad Request: can't parse entities: unexpected end",
            None,
        ),
    );
    sends(&stack, &[100]);
    stack.bot.queue_updates(&[owner_text(5, "Когда?")]);
    stack.bot.wait_sent(2);
    stack.stop();
    let sent = sent(&stack);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["parse_mode"], json!("MarkdownV2"));
    assert_eq!(sent[1].get("parse_mode"), None);
    assert_eq!(sent[1]["text"], json!("Готово к 1.5"));
    assert_eq!(sent[1]["reply_parameters"]["message_id"], json!(5));
}

#[test]
fn typing_repeats_while_the_turn_runs_and_stops_after_it() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Held(ANSWER.into()));
    sends(&stack, &[100]);
    stack.bot.queue_updates(&[owner_text(5, "Подумай")]);
    stack.gateway.wait_held();
    stack
        .bot
        .wait_until("two typing actions", |r| typing_calls(r, CHAT) >= 2);
    // A running indicator sends its next action one interval after this one.
    let quiet_until = Instant::now() + TYPING_INTERVAL + Duration::from_millis(500);
    assert!(stack.bot.calls("sendMessage").is_empty());
    stack.gateway.release();
    stack.bot.wait_sent(1);
    let typed = typing_calls(&stack.bot.requests(), CHAT);
    while Instant::now() < quiet_until {
        assert_eq!(typing_calls(&stack.bot.requests(), CHAT), typed);
        std::thread::sleep(Duration::from_millis(50));
    }
    stack.stop();
    let requests = stack.bot.requests();
    let answered = requests
        .iter()
        .position(|r| r.method() == "sendMessage")
        .expect("the answer");
    assert!(typing_calls(&requests[..answered], CHAT) >= 2);
    assert_eq!(typing_calls(&requests[answered..], CHAT), 0);
    assert_eq!(
        stack.bot.calls("sendChatAction")[0].json()["action"],
        json!("typing")
    );
}

#[test]
fn a_message_from_another_user_is_dropped() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    sends(&stack, &[100]);
    stack.bot.queue_updates(&[
        message_from(5, STRANGER, &json!({ "text": "чужой" })),
        owner_text(6, "свой"),
    ]);
    stack.bot.wait_sent(1);
    stack.stop();
    let sent = sent(&stack);
    assert_eq!(stack.gateway.prompts(), ["свой"]);
    assert_eq!(texts(&sent), [ANSWER]);
    assert_eq!(sent[0]["reply_parameters"]["message_id"], json!(6));
    for request in stack.bot.requests() {
        if request.method().starts_with("send") {
            assert_eq!(request.json()["chat_id"], json!(CHAT), "{request:?}");
        }
    }
}

#[test]
fn new_answers_the_reset_banner_and_the_next_turn_carries_no_earlier_history() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    stack.gateway.script(Answer::Text("Начнём".into()));
    sends(&stack, &[100, 101, 102]);
    stack.bot.queue_updates(&[owner_text(5, "Запиши молоко")]);
    stack.bot.wait_sent(1);
    stack.bot.queue_updates(&[owner_command(6, "/new")]);
    stack.bot.wait_sent(2);
    stack.bot.queue_updates(&[owner_text(7, "Что я просил?")]);
    stack.bot.wait_sent(3);
    stack.stop();
    let sent = sent(&stack);
    let banner = to_markdown_v2("✨ Сеанс сброшен! Начинаем с чистого листа.");
    assert_eq!(texts(&sent), [ANSWER, banner.as_str(), "Начнём"]);
    assert_eq!(sent[1]["reply_parameters"]["message_id"], json!(6));
    let turns = stack.gateway.turns();
    assert_eq!(turns.len(), 2);
    assert_eq!(user_texts(&turns[0]), ["Запиши молоко"]);
    assert_eq!(turns[1]["messages"].as_array().expect("messages").len(), 1);
    assert_eq!(user_texts(&turns[1]), ["Что я просил?"]);
}

#[test]
fn stop_during_a_running_tool_kills_it_and_the_next_message_goes_on_in_the_session() {
    let mut stack = Stack::start();
    let pid = stack.file("shell.pid");
    let command = format!("echo $$ > {}; sleep 30", pid.display());
    stack.gateway.script(Answer::Call(
        "terminal".into(),
        json!({ "command": command }),
    ));
    stack.gateway.script(Answer::Text(ANSWER.into()));
    sends(&stack, &[100, 101]);
    stack.bot.queue_updates(&[owner_text(5, "Запусти сборку")]);
    let shell = pid_file(&pid);
    stack.bot.queue_updates(&[owner_command(6, "/stop")]);
    // The tool sleeps 30 s; the reply comes within the 5 s bound of the wait.
    stack.bot.wait_sent(1);
    assert!(dead(&shell), "the shell {shell}");
    stack.bot.queue_updates(&[owner_text(7, "Что случилось?")]);
    stack.bot.wait_sent(2);
    stack.stop();
    let sent = sent(&stack);
    let stopped = to_markdown_v2("⚡ Остановлено. Вы можете продолжить этот сеанс.");
    assert_eq!(texts(&sent), [stopped.as_str(), ANSWER]);
    assert_eq!(sent[0]["reply_parameters"]["message_id"], json!(6));
    assert_eq!(sent[1]["reply_parameters"]["message_id"], json!(7));
    let turns = stack.gateway.turns();
    assert_eq!(turns.len(), 2);
    assert_eq!(user_texts(&turns[1]), ["Запусти сборку", "Что случилось?"]);
}

#[test]
fn status_shows_the_ported_state_with_the_route_of_the_session() {
    let started = unix_now();
    let mut stack = Stack::start();
    sends(&stack, &[100, 101]);
    stack.bot.queue_updates(&[owner_command(5, "/model deep")]);
    stack.bot.wait_sent(1);
    stack.bot.queue_updates(&[owner_command(6, "/status")]);
    stack.bot.wait_sent(2);
    let asked = unix_now();
    stack.stop();
    let sent = sent(&stack);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["reply_parameters"]["message_id"], json!(6));
    let zone = host_zone();
    let expected: Vec<String> = (started..=asked)
        .map(|since| {
            to_markdown_v2(&format!(
                "📊 **Состояние канала**\n\n**ID сеанса:** `agent:main:telegram:dm:{CHAT}`\n\
                 **Последняя активность:** {}\n**Model:** `deep`\n**Агент активен:** Нет\n\n\
                 **Подключённые платформы:** telegram",
                zone.local(since).ymd_hm()
            ))
        })
        .collect();
    let text = sent[1]["text"].as_str().expect("text");
    assert!(expected.iter().any(|e| e == text), "{text}");
    assert_eq!(stack.gateway.turns().len(), 0);
}

#[test]
fn model_with_an_alias_answers_the_switch_and_the_next_turn_uses_the_route() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    sends(&stack, &[100, 101]);
    stack.bot.queue_updates(&[owner_command(5, "/model deep")]);
    stack.bot.wait_sent(1);
    stack.bot.queue_updates(&[owner_text(6, "Запиши молоко")]);
    stack.bot.wait_sent(2);
    stack.stop();
    let sent = sent(&stack);
    let switched = to_markdown_v2(
        "Модель изменена на `deep`\n\
         _(только для этого сеанса — добавьте `--global`, чтобы сохранить)_",
    );
    assert_eq!(texts(&sent), [switched.as_str(), ANSWER]);
    assert_eq!(sent[0]["reply_parameters"]["message_id"], json!(5));
    let routes: Vec<Value> = stack
        .gateway
        .turns()
        .iter()
        .map(|t| t["route"].clone())
        .collect();
    assert_eq!(routes, [json!("deep")]);
}

#[test]
fn an_exhausted_quota_gives_the_owner_only_the_engines_quota_line() {
    let mut stack = Stack::start();
    let quota = GatewayError {
        class: ErrorClass::QuotaExhausted,
        retryable: true,
        retry_after: None,
        resets_at: Some(1_700_000_000),
        tried: None,
        message: None,
    };
    stack.gateway.script(Answer::Reject(429, quota));
    sends(&stack, &[100]);
    stack.bot.queue_updates(&[owner_text(5, "Запиши молоко")]);
    stack.bot.wait_sent(1);
    stack.stop();
    let sent = sent(&stack);
    let line =
        to_markdown_v2("quota exhausted on route main; nearest reset 2023-11-14 22:13:20 UTC");
    assert_eq!(texts(&sent), [line.as_str()]);
    assert_eq!(sent[0]["reply_parameters"]["message_id"], json!(5));
    assert_eq!(stack.gateway.turns().len(), 1);
}

fn unix_now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch");
    i64::try_from(elapsed.as_secs()).expect("unix time")
}

/// The zone `ulei-tg` dates in without `engine.timezone`: the host's.
fn host_zone() -> Zone {
    let localtime = Path::new("/etc/localtime");
    if localtime.exists() {
        Zone::from_file(localtime).expect("host zone")
    } else {
        Zone::utc()
    }
}

/// The `200` body of C6 `method` with `body` on the engine at `socket`.
fn c6_call(socket: &Path, method: &str, body: &Value) -> Value {
    let body = body.to_string();
    let mut stream = UnixStream::connect(socket).expect("connect to the engine");
    write!(
        stream,
        "POST /v1/{method} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("request");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("answer");
    let (head, body) = answer.split_once("\r\n\r\n").expect("head");
    assert!(head.starts_with("HTTP/1.1 200 "), "{answer}");
    serde_json::from_str(body).expect("JSON body")
}

/// Injects inbox event `prompt` once the owner session is free, which takes the channel's
/// held `inbox_turns`; the turn it started.
fn inject_when_free(socket: &Path, prompt: &str) -> String {
    let request = json!({
        "event_id": "evt-1", "kind": "task", "prompt": prompt, "lease": "lease-1", "attempt": 1,
    });
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let answer = c6_call(socket, "inject", &request);
        if answer["accepted"] == json!(true) {
            return answer["turn"].as_str().expect("turn").to_owned();
        }
        assert!(Instant::now() < until, "the owner session never got free");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_injected_event_reaches_the_owner_chat_once_and_its_turn_reports_sent() {
    const EVENT: &str = "[INTERNAL BACKGROUND EVENT]\nThe nightly backup finished.";
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    sends(&stack, &[100]);
    let socket = stack.file("c6.sock");
    let engine = Client::new(&socket, Duration::from_secs(1), Duration::from_secs(5)).expect("C6");
    engine
        .set_route(&SetRouteRequest {
            session: OWNER_SESSION.to_owned(),
            route: "main".to_owned(),
        })
        .expect("the owner's session in the journal");
    let turn = inject_when_free(&socket, EVENT);
    let status = StatusRequest {
        session: OWNER_SESSION.to_owned(),
        turn: Some(turn.clone()),
    };
    let until = Instant::now() + Duration::from_secs(10);
    let delivery = loop {
        let report = engine
            .status(&status)
            .expect("status")
            .turn
            .expect("the turn");
        if let Some(delivery) = report.delivery {
            break delivery;
        }
        assert!(Instant::now() < until, "no delivery reported for {turn}");
        std::thread::sleep(Duration::from_millis(20));
    };
    stack.stop();
    assert_eq!(delivery, Delivery::Sent);
    assert_eq!(
        sent(&stack),
        [json!({ "chat_id": OWNER, "text": ANSWER, "parse_mode": "MarkdownV2" })]
    );
    assert_eq!(typing_calls(&stack.bot.requests(), OWNER), 0);
    assert_eq!(stack.gateway.prompts(), [EVENT]);
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsynthetic-png";
const OPUS: &[u8] = b"OggS\x00\x02synthetic-opus";
const PDF: &[u8] = b"%PDF-1.4\nsynthetic-pdf-body\n%%EOF\n";

/// Every message and upload `ulei-tg` sent, in order.
fn deliveries(stack: &Stack) -> Vec<Recorded> {
    stack
        .bot
        .requests()
        .into_iter()
        .filter(|r| r.method().starts_with("send") && r.method() != "sendChatAction")
        .collect()
}

/// Makes the next call of `method` succeed as message `id`.
fn accepts(stack: &Stack, method: &str, id: i64) {
    stack.bot.script(method, ok(json!({ "message_id": id })));
}

/// The file field `field` of an upload to the owner's chat, with no reply target: name and bytes.
fn upload_to_owner(request: &Recorded, field: &str) -> (String, Vec<u8>) {
    let mut form = request.form();
    assert_eq!(form["chat_id"].1, CHAT.to_string().into_bytes());
    assert!(!form.contains_key("reply_parameters"));
    let (name, data) = form.remove(field).expect("file field");
    (name.expect("file name"), data)
}

#[test]
fn an_answer_with_three_media_tags_sends_its_text_then_a_photo_a_voice_and_a_document() {
    let mut stack = Stack::start();
    fs::write(stack.file("chart.png"), PNG).expect("png");
    fs::write(stack.file("note.ogg"), OPUS).expect("ogg");
    fs::write(stack.file("report.pdf"), PDF).expect("pdf");
    let answer = format!(
        "Отчёт готов.\nMEDIA:~/chart.png\n[[audio_as_voice]]\nMEDIA:{}\nMEDIA:{}",
        stack.file("note.ogg").display(),
        stack.file("report.pdf").display()
    );
    stack.gateway.script(Answer::Text(answer));
    sends(&stack, &[100]);
    accepts(&stack, "sendPhoto", 101);
    accepts(&stack, "sendVoice", 102);
    accepts(&stack, "sendDocument", 103);
    stack.bot.queue_updates(&[owner_text(5, "Пришли отчёт")]);
    stack.bot.wait_until("the document", |r| {
        r.iter().any(|r| r.method() == "sendDocument")
    });
    stack.stop();
    let sent = deliveries(&stack);
    let methods: Vec<&str> = sent.iter().map(Recorded::method).collect();
    assert_eq!(
        methods,
        ["sendMessage", "sendPhoto", "sendVoice", "sendDocument"]
    );
    let text = sent[0].json();
    assert_eq!(text["chat_id"], json!(CHAT));
    assert_eq!(text["text"], json!(to_markdown_v2("Отчёт готов.")));
    assert_eq!(text["reply_parameters"]["message_id"], json!(5));
    assert_eq!(
        upload_to_owner(&sent[1], "photo"),
        ("chart.png".into(), PNG.to_vec())
    );
    assert_eq!(
        upload_to_owner(&sent[2], "voice"),
        ("note.ogg".into(), OPUS.to_vec())
    );
    assert_eq!(
        upload_to_owner(&sent[3], "document"),
        ("report.pdf".into(), PDF.to_vec())
    );
}

#[test]
fn refused_files_get_one_line_each_while_the_rest_goes_and_the_log_names_only_paths() {
    const LOCKED: &[u8] = b"locked-body-that-never-reaches-a-log";
    let mut stack = Stack::start();
    let gone = stack.file("gone.pdf");
    let locked = stack.file("locked.pdf");
    fs::write(&locked, LOCKED).expect("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
    assert!(
        fs::File::open(&locked).is_err(),
        "the test needs a non-root user"
    );
    fs::create_dir(stack.file(".config")).expect("config dir");
    let denied = stack.file(".config/token.pdf");
    fs::write(&denied, PDF).expect("denied file");
    fs::write(stack.file("fine.pdf"), PDF).expect("pdf");
    let answer = format!(
        "{ANSWER}\nMEDIA:{}\nMEDIA:{}\nMEDIA:{}\nMEDIA:~/fine.pdf",
        gone.display(),
        locked.display(),
        denied.display(),
    );
    stack.gateway.script(Answer::Text(answer));
    sends(&stack, &[100, 101, 102, 103]);
    accepts(&stack, "sendDocument", 104);
    stack.bot.queue_updates(&[owner_text(5, "Пришли файлы")]);
    // Telegram's pace of one message a second per chat spreads the five sends over 4 s.
    let until = Instant::now() + Duration::from_secs(15);
    while stack.bot.calls("sendDocument").is_empty() {
        assert!(Instant::now() < until, "no document sent");
        std::thread::sleep(Duration::from_millis(20));
    }
    let log = stack.stop();
    let sent = deliveries(&stack);
    let methods: Vec<&str> = sent.iter().map(Recorded::method).collect();
    assert_eq!(
        methods,
        [
            "sendMessage",
            "sendMessage",
            "sendMessage",
            "sendMessage",
            "sendDocument"
        ]
    );
    let texts: Vec<Value> = sent[..4].iter().map(|r| r.json()["text"].clone()).collect();
    let notice = |name: &str| {
        json!(to_markdown_v2(&format!(
            "⚠️ Couldn't deliver the file attachment ({name})."
        )))
    };
    assert_eq!(
        texts,
        [
            json!(ANSWER),
            notice("gone.pdf"),
            notice("locked.pdf"),
            notice("token.pdf"),
        ]
    );
    assert_eq!(
        upload_to_owner(&sent[4], "document"),
        ("fine.pdf".into(), PDF.to_vec())
    );
    for path in [&gone, &locked, &denied] {
        assert!(log.contains(&path.display().to_string()), "{log}");
    }
    let shown = String::from_utf8_lossy(PDF);
    for body in [String::from_utf8_lossy(LOCKED), shown] {
        assert!(!log.contains(body.trim()), "{log}");
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).expect("chmod back");
}

#[test]
fn a_redelivered_inbox_event_sends_its_files_once() {
    const EVENT: &str = "[INTERNAL BACKGROUND EVENT]\nThe nightly report is ready.";
    let mut stack = Stack::start();
    fs::write(stack.file("night.pdf"), PDF).expect("pdf");
    let answer = format!("Ночной отчёт.\nMEDIA:{}", stack.file("night.pdf").display());
    stack.gateway.script(Answer::Text(answer));
    sends(&stack, &[100]);
    accepts(&stack, "sendDocument", 101);
    let socket = stack.file("c6.sock");
    let engine = Client::new(&socket, Duration::from_secs(1), Duration::from_secs(5)).expect("C6");
    engine
        .set_route(&SetRouteRequest {
            session: OWNER_SESSION.to_owned(),
            route: "main".to_owned(),
        })
        .expect("the owner's session in the journal");
    let turn = inject_when_free(&socket, EVENT);
    let status = StatusRequest {
        session: OWNER_SESSION.to_owned(),
        turn: Some(turn.clone()),
    };
    let until = Instant::now() + Duration::from_secs(10);
    let delivery = loop {
        let report = engine
            .status(&status)
            .expect("status")
            .turn
            .expect("the turn");
        if let Some(delivery) = report.delivery {
            break delivery;
        }
        assert!(Instant::now() < until, "no delivery reported for {turn}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(delivery, Delivery::Sent);
    let again = c6_call(
        &socket,
        "inject",
        &json!({
            "event_id": "evt-1", "kind": "task", "prompt": EVENT, "lease": "lease-2", "attempt": 2,
        }),
    );
    assert_eq!(again["accepted"], json!(true));
    assert_eq!(again["turn"], json!(turn));
    stack.stop();
    let methods: Vec<String> = deliveries(&stack)
        .iter()
        .map(|r| r.method().to_owned())
        .collect();
    assert_eq!(methods, ["sendMessage", "sendDocument"]);
    assert_eq!(stack.gateway.prompts(), [EVENT]);
}

#[test]
fn text_after_a_code_entity_reaches_the_model() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    sends(&stack, &[100]);
    let text =
        "Use the terminal tool to run sleep 120; echo late, then reply with its output only.";
    let code = json!({ "type": "code", "offset": 29, "length": 20 });
    stack.bot.queue_updates(&[owner_message(
        5,
        &json!({ "text": text, "entities": [code] }),
    )]);
    stack.bot.wait_sent(1);
    stack.stop();
    assert_eq!(stack.gateway.prompts(), [text]);
}

#[test]
fn text_after_literal_backticks_reaches_the_model() {
    let mut stack = Stack::start();
    stack.gateway.script(Answer::Text(ANSWER.into()));
    sends(&stack, &[100]);
    let text =
        "Use the terminal tool to run `sleep 120; echo late`, then reply with its output only.";
    stack
        .bot
        .queue_updates(&[owner_message(5, &json!({ "text": text }))]);
    stack.bot.wait_sent(1);
    stack.stop();
    assert_eq!(stack.gateway.prompts(), [text]);
}
