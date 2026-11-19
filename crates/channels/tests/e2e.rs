//! The owner's Telegram flow end to end: the fake Bot API drives the real `ulei-tg`, which runs
//! its turns over C6 on the real `ulei-engine serve`, whose model is the fake gateway.

#[allow(
    dead_code,
    reason = "the other test crates use other parts of the shared support"
)]
mod support {
    pub(crate) mod fake_bot_api;
    pub(crate) mod fake_gateway;
    pub(crate) mod process;
    pub(crate) mod stack;
}

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::fake_bot_api::{Recorded, Reply, fail, ok, typing_calls};
use support::fake_gateway::Answer;
use support::stack::{CHAT, OWNER, STT_KEY, Stack};
use ulei_channels::ingress::Zone;
use ulei_channels::render::TYPING_INTERVAL;

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

/// Makes the next `sendMessage` calls succeed as messages `ids`.
fn sends(stack: &Stack, ids: &[i64]) {
    for id in ids {
        stack
            .bot
            .script("sendMessage", ok(json!({ "message_id": id })));
    }
}

/// Waits until `count` messages went out.
fn wait_sent(stack: &Stack, count: usize) {
    stack.bot.wait_until(&format!("{count} messages"), |r| {
        r.iter().filter(|r| r.method() == "sendMessage").count() >= count
    });
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

fn texts(bodies: &[Value]) -> Vec<&str> {
    bodies
        .iter()
        .map(|b| b["text"].as_str().expect("text"))
        .collect()
}

/// The file in `dir` whose content is `data`.
fn saved(dir: &Path, data: &[u8]) -> PathBuf {
    fs::read_dir(dir)
        .expect("attachments dir")
        .map(|entry| entry.expect("entry").path())
        .find(|path| fs::read(path).expect("saved") == data)
        .expect("the saved file")
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
    wait_sent(&stack, 1);
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
    wait_sent(&stack, 1);
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
    wait_sent(&stack, 1);
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
    wait_sent(&stack, 2);
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
    wait_sent(&stack, 1);
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
    wait_sent(&stack, 1);
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
    wait_sent(&stack, 2);
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
    wait_sent(&stack, 2);
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
    wait_sent(&stack, 2);
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
    wait_sent(&stack, 1);
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
    wait_sent(&stack, 1);
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

/// The zone `ulei-tg` dates in without `engine.timezone`: the host's.
fn host_zone() -> Zone {
    let localtime = Path::new("/etc/localtime");
    if localtime.exists() {
        Zone::from_file(localtime).expect("host zone")
    } else {
        Zone::utc()
    }
}
