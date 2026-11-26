//! `ulei-tg`, its delivery and the transport's `FileSource` against the fake Bot API on
//! loopback.

#[allow(
    dead_code,
    reason = "tests/bot_api.rs and tests/c6.rs use other parts of the shared support"
)]
mod support {
    pub(crate) mod fake_bot_api;
    pub(crate) mod fake_c6;
    pub(crate) mod fake_clock;
    pub(crate) mod files;
    pub(crate) mod process;
}

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::fake_bot_api::{FakeBotApi, Recorded, Reply, fail, ok, texts, typing_calls};
use support::fake_c6::{Answer, FakeC6, Gate};
use support::fake_clock::FakeClock;
use support::files::saved;
use support::process::{finish, sigterm, stderr};
use ulei_channels::bot_api::{Client, Config, Sender, Stop};
use ulei_channels::ingress::{
    FileSource, Ingress, IngressConfig, Output as IngressOutput, Ports, ReactionConfig, RemoteFile,
    Timing, Turn, Update, Zone,
};
use ulei_channels::render::TYPING_INTERVAL;
use ulei_channels::stt::{MAX_FILE_BYTES, Stt, SttConfig};
use ulei_channels::tg::{Channel, Dispatch, Outbound, Outbox, SentTexts, Settings};

const TOKEN: &str = "123456:SECRET-TOKEN-OF-THE-TEST-BOT";
const OWNER: i64 = 42;
const CHAT: i64 = 1001;
const OTHER_CHAT: i64 = 1002;
const WAIT: Duration = Duration::from_secs(5);
const JPEG: &[u8] = b"\xff\xd8\xff\xe0jpeg";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\npng";

/// A private scratch directory and an engine socket path beside it, removed on drop.
struct Scratch {
    dir: PathBuf,
    socket: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("ulei-tg-{pid}-{n}-{tag}"));
        fs::create_dir_all(&dir).expect("scratch dir");
        // Outside `dir`: a Unix socket path has at most 107 bytes.
        let socket = std::env::temp_dir().join(format!("tg-{pid}-{n}.sock"));
        Self { dir, socket }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
        let _ = fs::remove_file(&self.socket);
    }
}

/// Writes the token file (mode `token_mode`) and `ulei.toml` pointing at `fake`; `owner_ids`
/// goes in only when `with_owner` holds, `extra` lines close the `[telegram]` section.
fn write_config(
    dir: &Scratch,
    fake: &FakeBotApi,
    token_mode: u32,
    with_owner: bool,
    extra: &str,
) -> PathBuf {
    let token = dir.path("token");
    fs::write(&token, format!("{TOKEN}\n")).expect("token");
    fs::set_permissions(&token, fs::Permissions::from_mode(token_mode)).expect("chmod");
    let owner = if with_owner {
        format!("owner_ids = \"{OWNER}\"\n")
    } else {
        String::new()
    };
    let text = format!(
        "[engine]\nc6_socket = \"{socket}\"\n\n[telegram]\n\
         token_file = \"{token}\"\n{owner}attachments_dir = \"{inbox}\"\n\
         base_url = \"{base}\"\nfile_base_url = \"{files}\"\npoll_timeout = 1\nread_timeout = 2\n{extra}",
        socket = dir.socket.display(),
        token = token.display(),
        inbox = dir.path("inbox").display(),
        base = fake.base_url(),
        files = fake.file_base_url(),
    );
    let path = dir.path("ulei.toml");
    fs::write(&path, text).expect("config");
    path
}

fn bot_user(fake: &FakeBotApi) {
    fake.script(
        "getMe",
        ok(json!({ "id": 7, "is_bot": true, "username": "test_bot" })),
    );
}

fn text_update(id: i64) -> Value {
    json!({
        "update_id": id,
        "message": {
            "message_id": id,
            "date": 1_790_000_000,
            "chat": { "id": CHAT, "type": "private" },
            "from": { "id": OWNER, "is_bot": false, "first_name": "Owner" },
            "text": "hi"
        }
    })
}

fn spawn(config: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_ulei-tg"))
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ulei-tg")
}

/// Waits until the fake saw a `getUpdates` with `offset`, which the poller sends only after the
/// batch before it was handed to the loop.
fn wait_for_offset(fake: &FakeBotApi, offset: i64) {
    let until = Instant::now() + WAIT;
    while !fake
        .calls("getUpdates")
        .iter()
        .any(|r| r.json()["offset"] == json!(offset))
    {
        assert!(Instant::now() < until, "no getUpdates with offset {offset}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A fake engine on the socket of `dir`'s config that accepts the next `turns` turns, each of
/// which ends completed without an answer.
fn engine_taking(dir: &Scratch, turns: usize) -> FakeC6 {
    let engine = FakeC6::start(&dir.socket);
    for n in 0..turns {
        engine.script(
            "submit_turn",
            support::fake_c6::ok(
                json!({ "accepted": true, "turn": format!("t-{n}"), "position": 0, "queue_depth": 0 }),
            ),
        );
        engine.script(
            "turn_events",
            support::fake_c6::ok(json!({
                "turn": format!("t-{n}"),
                "state": "ended",
                "events": [
                    { "seq": 1, "type": "start" },
                    { "seq": 2, "type": "end", "outcome": "completed" }
                ]
            })),
        );
    }
    engine
}

#[test]
fn token_file_open_to_others_is_a_startup_error() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("mode");
    let config = write_config(&dir, &fake, 0o644, true, "");
    let output = finish(spawn(&config));
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(
        err.contains("telegram.token_file") && err.contains("0600"),
        "{err}"
    );
    assert!(fake.requests().is_empty());
}

#[test]
fn missing_required_key_is_a_startup_error_naming_it() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("missing");
    let config = write_config(&dir, &fake, 0o600, false, "");
    let output = finish(spawn(&config));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("missing telegram.owner_ids"),
        "{}",
        stderr(&output)
    );
    assert!(fake.requests().is_empty());
}

/// The exit status and stderr of `ulei-tg --config` on the config `extra` closes; nothing may
/// reach the Bot API.
fn refused(tag: &str, extra: &str) -> (Option<i32>, String, PathBuf) {
    let fake = FakeBotApi::start();
    let dir = Scratch::new(tag);
    let config = write_config(&dir, &fake, 0o600, true, extra);
    let output = finish(spawn(&config));
    assert!(fake.requests().is_empty());
    (output.status.code(), stderr(&output), config)
}

#[test]
fn an_unknown_telegram_key_stops_it_with_the_decoders_line() {
    let (code, err, config) = refused("unknown-tg", "colour = \"blue\"\n");
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!(
            "ulei-tg: config {} line 12: unknown key telegram.colour\n",
            config.display()
        )
    );
}

#[test]
fn an_unknown_key_of_a_section_it_does_not_read_stops_it_too() {
    let (code, err, config) = refused("unknown-qb", "\n[qb]\nport = 8080\n");
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!(
            "ulei-tg: config {} line 14: unknown key qb.port\n",
            config.display()
        )
    );
}

#[test]
fn without_the_flag_the_file_named_by_ulei_config_is_read() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("ulei-config");
    let config = write_config(&dir, &fake, 0o600, false, "");
    let child = Command::new(env!("CARGO_BIN_EXE_ulei-tg"))
        .env_clear()
        .env("ULEI_CONFIG", &config)
        .env("HOME", dir.path("home"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ulei-tg");
    let output = finish(child);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "ulei-tg: missing telegram.owner_ids\n");
    assert!(fake.requests().is_empty());
}

#[test]
fn debug_and_startup_errors_never_show_the_token() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("secret");
    let config = write_config(&dir, &fake, 0o600, true, "");
    let settings = Settings::load(Some(&config)).expect("settings");
    assert_eq!(settings.bot.token, TOKEN);
    assert!(!format!("{settings:?}").contains("SECRET"));

    fake.script("getMe", fail(401, &format!("Unauthorized: {TOKEN}"), None));
    let output = finish(spawn(&config));
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(err.contains("getMe") && !err.contains("SECRET"), "{err}");
}

#[test]
fn cold_start_keeps_the_updates_queued_while_down() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("cold");
    let config = write_config(&dir, &fake, 0o600, true, "");
    let _engine = engine_taking(&dir, 1);
    bot_user(&fake);
    fake.queue_updates(&[text_update(5)]);
    let child = spawn(&config);
    wait_for_offset(&fake, 6);
    sigterm(&child);
    let output = finish(child);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let webhook = fake.calls("deleteWebhook");
    assert_eq!(webhook.len(), 1);
    assert_eq!(webhook[0].json()["drop_pending_updates"], json!(false));
    assert!(stderr(&output).contains("dispatched 1 turns"));
}

#[test]
fn sigterm_confirms_the_offset_and_the_next_start_polls_from_it() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("restart");
    let config = write_config(&dir, &fake, 0o600, true, "");
    let _engine = engine_taking(&dir, 2);
    bot_user(&fake);
    fake.queue_updates(&[text_update(5)]);
    let child = spawn(&config);
    wait_for_offset(&fake, 6);
    sigterm(&child);
    let first = finish(child);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    // The coalescing window was still open: the stop handed the turn on before confirming.
    assert!(stderr(&first).contains("dispatched 1 turns"));
    let polls: Vec<Recorded> = fake.calls("getUpdates");
    let confirm = polls.last().expect("confirm").json();
    assert_eq!(
        (&confirm["offset"], &confirm["timeout"]),
        (&json!(6), &json!(0))
    );

    bot_user(&fake);
    fake.queue_updates(&[text_update(5), text_update(6)]);
    let child = spawn(&config);
    wait_for_offset(&fake, 7);
    sigterm(&child);
    let second = finish(child);
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert!(stderr(&second).contains("dispatched 1 turns"));
}

#[test]
fn fatal_poller_error_exits_non_zero() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("fatal");
    let config = write_config(&dir, &fake, 0o600, true, "");
    bot_user(&fake);
    fake.script("getUpdates", fail(401, "Unauthorized", None));
    let output = finish(spawn(&config));
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("polling"), "{}", stderr(&output));
    assert_eq!(fake.calls("getUpdates").len(), 1);
}

fn client(fake: &FakeBotApi) -> Client {
    let mut config = Config::new(TOKEN, Some(&fake.base_url()));
    config.file_base_url = fake.file_base_url();
    config.read_timeout = Duration::from_secs(2);
    Client::new(config)
}

#[test]
fn file_source_saves_an_owner_photo_privately_in_the_attachments_dir() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("photo");
    let inbox = dir.path("inbox");
    let jpeg = [&b"\xff\xd8\xff\xe0"[..], &[0; 60]].concat();
    fake.script(
        "getFile",
        ok(json!({ "file_id": "photo-big", "file_size": jpeg.len(), "file_path": "photos/p.jpg" })),
    );
    fake.script("p.jpg", Reply::Bytes(jpeg.clone()));
    let config = IngressConfig {
        owner_ids: vec![OWNER],
        bot_username: Some("test_bot".to_owned()),
        inbox_dir: inbox.clone(),
        zone: Zone::utc(),
        max_file_bytes: 20 * 1024 * 1024,
        max_media_bytes: 128 * 1024 * 1024,
        timing: Timing::default(),
        reactions: ReactionConfig::default(),
    };
    let ports = Ports {
        files: Box::new(client(&fake)),
        stt: None,
        texts: None,
    };
    let mut ingress = Ingress::new(config, ports);
    let update = Update::from_value(json!({
        "update_id": 9,
        "message": {
            "message_id": 9,
            "date": 1_790_000_000,
            "chat": { "id": CHAT, "type": "private" },
            "from": { "id": OWNER, "is_bot": false, "first_name": "Owner" },
            "photo": [{ "file_id": "photo-big", "file_unique_id": "u", "width": 9, "height": 9 }]
        }
    }))
    .expect("update");
    let now = Instant::now();
    assert_eq!(ingress.accept(&update, now), Vec::new());
    let outputs = ingress.poll(now + Duration::from_secs(60));
    let [IngressOutput::Turn(turn)] = outputs.as_slice() else {
        panic!("one turn expected: {outputs:?}");
    };
    let saved = &turn.attachments[0].path;
    assert_eq!(saved.parent(), Some(inbox.as_path()));
    assert_eq!(fs::read(saved).expect("saved"), jpeg);
    assert_eq!(
        fs::metadata(saved).expect("meta").permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&inbox).expect("meta").permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fake.calls("p.jpg")[0].path,
        format!("/file/bot{TOKEN}/photos/p.jpg")
    );
}

#[test]
fn file_source_stops_a_download_at_the_cap() {
    let fake = FakeBotApi::start();
    fake.script("big.bin", Reply::Bytes(vec![7; 5000]));
    let file = RemoteFile {
        file_path: "documents/big.bin".to_owned(),
    };
    let mut sink = Vec::new();
    let err =
        FileSource::download(&client(&fake), &file, 100, &mut sink).expect_err("over the cap");
    assert_eq!(err.kind(), std::io::ErrorKind::FileTooLarge);
    assert!(sink.len() <= 100);
}

#[test]
fn owner_files_streamed_past_the_cap_leave_no_part_file_and_are_reported() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("overflow");
    let inbox = dir.path("inbox");
    let cap = 100_000;
    let mut jpeg = b"\xff\xd8\xff\xe0".to_vec();
    jpeg.resize(300_004, 0);
    fake.script(
        "getFile",
        ok(json!({ "file_id": "doc-big", "file_size": 1000, "file_path": "documents/big.pdf" })),
    );
    fake.script("big.pdf", Reply::Bytes(vec![b'%'; 300_000]));
    fake.script(
        "getFile",
        ok(json!({ "file_id": "photo-big", "file_size": 1000, "file_path": "photos/big.jpg" })),
    );
    fake.script("big.jpg", Reply::Bytes(jpeg));
    let config = IngressConfig {
        owner_ids: vec![OWNER],
        bot_username: Some("test_bot".to_owned()),
        inbox_dir: inbox.clone(),
        zone: Zone::utc(),
        max_file_bytes: cap,
        max_media_bytes: cap,
        timing: Timing::default(),
        reactions: ReactionConfig::default(),
    };
    let ports = Ports {
        files: Box::new(client(&fake)),
        stt: None,
        texts: None,
    };
    let mut ingress = Ingress::new(config, ports);
    let message = |id: i64, media: Value| {
        let mut message = json!({
            "message_id": id,
            "date": 1_790_000_000,
            "chat": { "id": CHAT, "type": "private" },
            "from": { "id": OWNER, "is_bot": false, "first_name": "Owner" },
        });
        message
            .as_object_mut()
            .expect("object")
            .extend(media.as_object().expect("object").clone());
        Update::from_value(json!({ "update_id": id, "message": message })).expect("update")
    };
    let document = message(
        9,
        json!({ "document": { "file_id": "doc-big", "file_unique_id": "d", "file_name": "big.pdf",
                              "mime_type": "application/pdf", "file_size": 1000 } }),
    );
    let photo = message(
        10,
        json!({ "photo": [{ "file_id": "photo-big", "file_unique_id": "p", "width": 9, "height": 9,
                            "file_size": 1000 }] }),
    );
    let cases = [
        (
            &document,
            "\u{26a0}\u{fe0f} Couldn't download your attachment (big.pdf) (FileTooLarge). Please try sending it again.",
            "[The user attempted to send a attachment (big.pdf) but it could not be downloaded (FileTooLarge); they have been asked to retry.]",
        ),
        (
            &photo,
            "\u{26a0}\u{fe0f} Couldn't download your photo (FileTooLarge). Please try sending it again.",
            "[The user attempted to send a photo but it could not be downloaded (FileTooLarge); they have been asked to retry.]",
        ),
    ];
    let mut now = Instant::now();
    for (update, notice, note) in cases {
        let outputs = ingress.accept(update, now);
        let [IngressOutput::Notice { text, .. }] = outputs.as_slice() else {
            panic!("one notice expected: {outputs:?}");
        };
        assert_eq!(text, notice);
        now += Duration::from_secs(60);
        let outputs = ingress.poll(now);
        let [IngressOutput::Turn(turn)] = outputs.as_slice() else {
            panic!("one turn expected: {outputs:?}");
        };
        assert_eq!(turn.text, note);
        assert!(turn.attachments.is_empty(), "{turn:?}");
        let left: Vec<_> = fs::read_dir(&inbox)
            .expect("inbox")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
    assert_eq!(fake.calls("big.pdf").len(), 1);
    assert_eq!(fake.calls("big.jpg").len(), 1);
}

/// The delivery threads over `fake` with virtual time for its flood limits and retries.
fn outbound(fake: &FakeBotApi, texts: &SentTexts) -> Outbound {
    outbound_on(fake, texts, &Arc::new(FakeClock::new(Duration::ZERO)))
}

/// The delivery threads over `fake` on `clock`.
fn outbound_on(fake: &FakeBotApi, texts: &SentTexts, clock: &Arc<FakeClock>) -> Outbound {
    let sender = Sender::new(client(fake), Arc::clone(clock) as Arc<_>);
    Outbound::start(Arc::new(sender), texts.clone()).expect("outbound thread")
}

/// Moves `clock` on by the typing interval until `done` holds for the fake's requests; ending a
/// turn of a chat without one changes nothing but makes the typing thread read the clock.
fn advance_until(
    fake: &FakeBotApi,
    clock: &FakeClock,
    outbox: &Outbox,
    what: &str,
    done: impl Fn(&[Recorded]) -> bool,
) {
    let until = Instant::now() + WAIT;
    while !done(&fake.requests()) {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        clock.advance(TYPING_INTERVAL);
        outbox.turn_ended(OTHER_CHAT).expect("queued");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_failed_download_notice_reaches_the_owner_as_a_reply() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("notice");
    let config = write_config(&dir, &fake, 0o600, true, "");
    let _engine = engine_taking(&dir, 1);
    bot_user(&fake);
    fake.script("getFile", fail(400, "Bad Request: invalid file_id", None));
    fake.script("sendMessage", ok(json!({ "message_id": 10 })));
    fake.queue_updates(&[json!({
        "update_id": 9,
        "message": {
            "message_id": 9,
            "date": 1_790_000_000,
            "chat": { "id": CHAT, "type": "private" },
            "from": { "id": OWNER, "is_bot": false, "first_name": "Owner" },
            "photo": [{ "file_id": "photo-big", "file_unique_id": "u", "width": 9, "height": 9 }]
        }
    })]);
    let child = spawn(&config);
    fake.wait_until("the notice", |r| {
        r.iter().any(|r| r.method() == "sendMessage")
    });
    sigterm(&child);
    let output = finish(child);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let sent = fake.calls("sendMessage")[0].json();
    assert_eq!(sent["chat_id"], json!(CHAT));
    assert_eq!(sent["reply_parameters"]["message_id"], json!(9));
    assert_eq!(sent["parse_mode"], json!("MarkdownV2"));
    let text = sent["text"].as_str().expect("text");
    assert!(text.contains("Couldn't download your photo"), "{text}");
    assert!(stderr(&output).contains("1 notices"), "{}", stderr(&output));
}

#[test]
fn a_rejected_markup_is_resent_as_plain_text() {
    let fake = FakeBotApi::start();
    fake.script(
        "sendMessage",
        fail(
            400,
            "Bad Request: can't parse entities: unexpected end",
            None,
        ),
    );
    fake.script("sendMessage", ok(json!({ "message_id": 50 })));
    let out = outbound(&fake, &SentTexts::new());
    out.outbox()
        .deliver(CHAT, Some(7), "**Done** at 1.5", true)
        .expect("queued");
    out.finish().expect("finished");
    let sent: Vec<Value> = fake
        .calls("sendMessage")
        .iter()
        .map(Recorded::json)
        .collect();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["parse_mode"], json!("MarkdownV2"));
    assert_eq!(sent[1].get("parse_mode"), None);
    assert_eq!(sent[1]["text"], json!("Done at 1.5"));
    assert_eq!(sent[1]["reply_parameters"]["message_id"], json!(7));
}

#[test]
fn a_long_text_is_split_with_only_the_first_part_replying() {
    let fake = FakeBotApi::start();
    fake.script("sendMessage", ok(json!({ "message_id": 50 })));
    fake.script("sendMessage", ok(json!({ "message_id": 51 })));
    let out = outbound(&fake, &SentTexts::new());
    let text = "word ".repeat(1200);
    out.outbox()
        .deliver(CHAT, Some(7), text, true)
        .expect("queued");
    out.finish().expect("finished");
    let sent: Vec<Value> = fake
        .calls("sendMessage")
        .iter()
        .map(Recorded::json)
        .collect();
    assert_eq!(sent.len(), 2);
    for part in &sent {
        let text = part["text"].as_str().expect("text");
        assert!(text.encode_utf16().count() <= 4096);
    }
    assert!(
        sent[0]["text"]
            .as_str()
            .expect("text")
            .ends_with("\\(1/2\\)")
    );
    assert_eq!(sent[0]["reply_parameters"]["message_id"], json!(7));
    assert_eq!(sent[1].get("reply_parameters"), None);
}

#[test]
fn a_blank_text_sends_nothing() {
    let fake = FakeBotApi::start();
    let out = outbound(&fake, &SentTexts::new());
    out.outbox()
        .deliver(CHAT, Some(7), " \n\t ", false)
        .expect("queued");
    out.finish().expect("finished");
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());
}

#[test]
fn typing_repeats_on_the_clock_while_a_turn_runs_and_stops_when_it_ends() {
    let fake = FakeBotApi::start();
    let clock = Arc::new(FakeClock::new(Duration::ZERO));
    let out = outbound_on(&fake, &SentTexts::new(), &clock);
    let outbox = out.outbox();
    outbox.turn_started(CHAT).expect("queued");
    advance_until(&fake, &clock, &outbox, "a second typing action", |r| {
        typing_calls(r, CHAT) >= 2
    });
    outbox.turn_ended(CHAT).expect("queued");
    // The other chat's first action follows the end of this turn on the typing thread.
    outbox.turn_started(OTHER_CHAT).expect("queued");
    fake.wait_until("the other chat's typing", |r| {
        typing_calls(r, OTHER_CHAT) == 1
    });
    let typed = typing_calls(&fake.requests(), CHAT);
    outbox.turn_ended(OTHER_CHAT).expect("queued");
    clock.advance(TYPING_INTERVAL * 2);
    outbox.turn_ended(OTHER_CHAT).expect("queued");
    drop(outbox);
    out.finish().expect("finished");
    let requests = fake.requests();
    assert_eq!(typing_calls(&requests, CHAT), typed);
    assert_eq!(typing_calls(&requests, OTHER_CHAT), 1);
    let action = fake.calls("sendChatAction")[0].json();
    assert_eq!(action["action"], json!("typing"));
}

#[test]
fn typing_keeps_its_cadence_while_a_delivery_waits_for_its_retry() {
    let fake = FakeBotApi::start();
    fake.script("sendMessage", fail(502, "Bad Gateway", None));
    fake.script("sendMessage", ok(json!({ "message_id": 50 })));
    let clock = Arc::new(FakeClock::new(Duration::ZERO));
    let out = outbound_on(&fake, &SentTexts::new(), &clock);
    let outbox = out.outbox();
    outbox.turn_started(CHAT).expect("queued");
    fake.wait_until("the first typing action", |r| typing_calls(r, CHAT) == 1);
    clock.hold_sleeps();
    outbox
        .deliver(CHAT, Some(7), "answer", true)
        .expect("queued");
    let until = Instant::now() + WAIT;
    while clock.slept().is_empty() {
        assert!(
            Instant::now() < until,
            "the delivery never waited for its retry"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    advance_until(&fake, &clock, &outbox, "typing during the back-off", |r| {
        typing_calls(r, CHAT) >= 2
    });
    assert_eq!(fake.calls("sendMessage").len(), 1);
    clock.release_sleeps();
    outbox.turn_ended(CHAT).expect("queued");
    drop(outbox);
    out.finish().expect("finished");
    assert_eq!(fake.calls("sendMessage").len(), 2);
}

#[test]
fn a_typing_cooldown_carries_into_the_next_turn_of_the_chat() {
    let fake = FakeBotApi::start();
    fake.script(
        "sendChatAction",
        fail(
            429,
            "Too Many Requests: retry after 30",
            Some(json!({ "retry_after": 30 })),
        ),
    );
    fake.script("sendMessage", ok(json!({ "message_id": 50 })));
    let out = outbound(&fake, &SentTexts::new());
    let outbox = out.outbox();
    outbox.turn_started(CHAT).expect("queued");
    outbox.turn_ended(CHAT).expect("queued");
    outbox.turn_started(CHAT).expect("queued");
    // An interim reply sends `typing` again unless the chat cools down.
    outbox
        .deliver(CHAT, None, "step one", false)
        .expect("queued");
    outbox.turn_ended(CHAT).expect("queued");
    drop(outbox);
    out.finish().expect("finished");
    assert_eq!(fake.calls("sendMessage").len(), 1);
    assert_eq!(typing_calls(&fake.requests(), CHAT), 1);
}

/// Collects turns; the first one stops the channel.
struct Turns {
    stop: Stop,
    turns: Vec<Turn>,
}

impl Dispatch for Turns {
    fn dispatch(&mut self, output: IngressOutput) {
        if let IngressOutput::Turn(turn) = output {
            self.turns.push(turn);
            self.stop.request();
        }
    }
}

fn reaction_to(message_id: i64) -> Value {
    json!({
        "update_id": 30,
        "message_reaction": {
            "chat": { "id": CHAT, "type": "private" },
            "message_id": message_id,
            "user": { "id": OWNER, "is_bot": false, "first_name": "Owner" },
            "date": 1_790_000_000,
            "old_reaction": [],
            "new_reaction": [{ "type": "emoji", "emoji": "👍" }]
        }
    })
}

/// Runs the channel of `config` with `texts` until its first turn.
fn first_turn(fake: &FakeBotApi, config: &Path, texts: SentTexts) -> Turn {
    bot_user(fake);
    let settings = Settings::load(Some(config)).expect("settings");
    let stop = Stop::new();
    let channel = Channel::start(settings, stop.clone(), Box::new(texts)).expect("start");
    let watchdog = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(WAIT);
        watchdog.request();
    });
    let mut turns = Turns {
        stop,
        turns: Vec::new(),
    };
    channel.run(&mut turns).expect("run");
    turns.turns.into_iter().next().expect("a turn")
}

#[test]
fn a_reaction_to_a_sent_reply_shows_its_text_to_the_agent() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("reaction");
    let config = write_config(&dir, &fake, 0o600, true, "reaction_wake_delay = 0\n");
    fake.script("sendMessage", ok(json!({ "message_id": 77 })));
    let texts = SentTexts::new();
    let out = outbound(&fake, &texts);
    out.outbox()
        .deliver(CHAT, Some(5), "Report **ready**", true)
        .expect("queued");
    out.finish().expect("finished");
    fake.queue_updates(&[reaction_to(77)]);
    let turn = first_turn(&fake, &config, texts);
    let context = turn.context.expect("reaction context");
    assert!(
        context.contains("👍 на моё сообщение #77 «Report ready»"),
        "{context}"
    );
}

#[test]
fn configured_reaction_wording_reaches_the_wake_turn() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("wording");
    let extra = "reaction_wake_delay = \"0.01\"\nreaction_wake_text = \"[reaction event]\"\n\
                 reaction_context_header = \"Reactions:\"\n";
    let config = write_config(&dir, &fake, 0o600, true, extra);
    fake.queue_updates(&[reaction_to(10)]);
    let turn = first_turn(&fake, &config, SentTexts::new());
    assert_eq!(turn.text, "[reaction event]");
    let context = turn.context.expect("reaction context");
    assert!(context.starts_with("Reactions:\n"), "{context}");
}

#[test]
fn an_invalid_reaction_delay_is_an_error_naming_the_key() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("delay");
    let config = write_config(&dir, &fake, 0o600, true, "reaction_wake_delay = true\n");
    let err = Settings::load(Some(&config)).expect_err("invalid delay");
    assert!(
        err.to_string().contains("telegram.reaction_wake_delay"),
        "{err}"
    );
}

#[test]
fn an_owner_message_reaches_the_engine_as_a_turn_of_the_chats_session() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("engine");
    let config = write_config(&dir, &fake, 0o600, true, "coalesce_quiet = 0\n");
    bot_user(&fake);
    let engine = engine_taking(&dir, 1);
    fake.queue_updates(&[text_update(5)]);
    let child = spawn(&config);
    engine.wait_for(1);
    sigterm(&child);
    let output = finish(child);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let (method, body) = &engine.calls()[0];
    assert_eq!(method, "submit_turn");
    assert_eq!(
        body["session"],
        json!(format!("agent:main:telegram:dm:{CHAT}"))
    );
    assert_eq!(body["prompt"], json!("hi"));
    assert!(fake.calls("sendMessage").is_empty());
}

#[test]
fn engine_timeouts_come_from_the_config() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("c6-timeouts");
    let extra = "engine_connect_timeout = \"0.5\"\nengine_call_timeout = 30\n";
    let config = write_config(&dir, &fake, 0o600, true, extra);
    let settings = Settings::load(Some(&config)).expect("settings");
    assert_eq!(settings.engine_connect_timeout, Duration::from_millis(500));
    assert_eq!(settings.engine_call_timeout, Duration::from_secs(30));
}

#[test]
fn the_write_timeout_is_its_own_key_defaulting_to_twenty_seconds() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("write-timeout");
    let config = write_config(&dir, &fake, 0o600, true, "");
    let settings = Settings::load(Some(&config)).expect("settings");
    assert_eq!(
        (settings.bot.read_timeout, settings.bot.write_timeout),
        (Duration::from_secs(2), Duration::from_secs(20))
    );
    let config = write_config(&dir, &fake, 0o600, true, "write_timeout = \"0.5\"\n");
    let settings = Settings::load(Some(&config)).expect("settings");
    assert_eq!(
        (settings.bot.read_timeout, settings.bot.write_timeout),
        (Duration::from_secs(2), Duration::from_millis(500))
    );
}

#[test]
fn sigterm_while_a_turn_is_followed_exits_zero() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("followed");
    let config = write_config(&dir, &fake, 0o600, true, "coalesce_quiet = 0\n");
    bot_user(&fake);
    let engine = FakeC6::start(&dir.socket);
    engine.script(
        "submit_turn",
        support::fake_c6::ok(
            json!({ "accepted": true, "turn": "t-0", "position": 0, "queue_depth": 0 }),
        ),
    );
    engine.script(
        "turn_events",
        support::fake_c6::ok(
            json!({ "turn": "t-0", "state": "running", "events": [{ "seq": 1, "type": "start" }] }),
        ),
    );
    engine.script("turn_events", Answer::Held(Gate::new()));
    fake.queue_updates(&[text_update(5)]);
    let child = spawn(&config);
    engine.wait_for(3);
    sigterm(&child);
    let output = finish(child);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(fake.calls("sendMessage").is_empty());
}

/// An owner photo update; `extra` adds caption or album fields to the message.
fn photo_update(id: i64, file_id: &str, extra: &Value) -> Value {
    let mut message = json!({
        "message_id": id,
        "date": 1_790_000_000,
        "chat": { "id": CHAT, "type": "private" },
        "from": { "id": OWNER, "is_bot": false, "first_name": "Owner" },
        "photo": [{ "file_id": file_id, "file_unique_id": file_id, "width": 9, "height": 9 }]
    });
    let fields = extra.as_object().expect("message fields").clone();
    message.as_object_mut().expect("message").extend(fields);
    json!({ "update_id": id, "message": message })
}

/// Scripts the download of `file_id` as `name` with `data`.
fn photo_file(fake: &FakeBotApi, file_id: &str, name: &str, data: &[u8]) {
    fake.script(
        "getFile",
        ok(json!({ "file_id": file_id, "file_size": data.len(), "file_path": format!("photos/{name}") })),
    );
    fake.script(name, Reply::Bytes(data.to_vec()));
}

/// The prompt note of the image saved at `path`.
fn image_note(path: &Path) -> String {
    format!(
        "[The user sent an image. It is saved at: {}. You cannot view images yet, so do not \
         describe its content.]",
        path.display()
    )
}

/// Runs `ulei-tg` over `updates` until the engine read its one turn; the turn's prompt.
fn prompt_of(fake: &FakeBotApi, dir: &Scratch, updates: &[Value]) -> String {
    let config = write_config(dir, fake, 0o600, true, "coalesce_quiet = 0\n");
    bot_user(fake);
    let engine = engine_taking(dir, 1);
    fake.queue_updates(updates);
    let child = spawn(&config);
    engine.wait_for(2);
    sigterm(&child);
    let output = finish(child);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(engine.methods(), ["submit_turn", "turn_events"]);
    assert!(fake.calls("sendMessage").is_empty());
    let (_, body) = &engine.calls()[0];
    body["prompt"].as_str().expect("prompt").to_owned()
}

#[test]
fn a_photo_without_caption_is_a_turn_whose_prompt_is_the_image_note() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("photo-note");
    photo_file(&fake, "photo-a", "a.jpg", JPEG);
    let prompt = prompt_of(&fake, &dir, &[photo_update(9, "photo-a", &json!({}))]);
    let path = saved(&dir.path("inbox"), JPEG);
    assert_eq!(prompt, image_note(&path));
    assert_eq!(
        fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn a_photo_with_a_caption_gives_the_note_then_the_caption() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("photo-caption");
    photo_file(&fake, "photo-a", "a.jpg", JPEG);
    let update = photo_update(9, "photo-a", &json!({ "caption": "что тут?" }));
    let prompt = prompt_of(&fake, &dir, &[update]);
    assert_eq!(
        prompt,
        format!(
            "{}\n\nчто тут?",
            image_note(&saved(&dir.path("inbox"), JPEG))
        )
    );
}

#[test]
fn two_images_of_one_turn_give_two_notes_in_arrival_order() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("photo-album");
    photo_file(&fake, "photo-a", "a.jpg", JPEG);
    photo_file(&fake, "photo-b", "b.png", PNG);
    let album = json!({ "media_group_id": "album-1" });
    let updates = [
        photo_update(9, "photo-a", &album),
        photo_update(10, "photo-b", &album),
    ];
    let prompt = prompt_of(&fake, &dir, &updates);
    assert_eq!(
        prompt,
        format!(
            "{}\n\n{}",
            image_note(&saved(&dir.path("inbox"), JPEG)),
            image_note(&saved(&dir.path("inbox"), PNG))
        )
    );
}

// --- Speech to text --------------------------------------------------------------------------

const STT_KEY: &str = "gsk_synthetic-stt-key-for-tests";
const OGG: &[u8] = b"OggS\x00\x02voice";

fn voice_update(id: i64) -> Value {
    json!({
        "update_id": id,
        "message": {
            "message_id": id,
            "date": 1_790_000_000,
            "chat": { "id": CHAT, "type": "private" },
            "from": { "id": OWNER, "is_bot": false, "first_name": "Owner" },
            "voice": {
                "file_id": "voice-1", "file_unique_id": "v1", "duration": 7,
                "mime_type": "audio/ogg", "file_size": OGG.len()
            }
        }
    })
}

/// Scripts the download of the voice note of [`voice_update`].
fn voice_file(fake: &FakeBotApi) {
    fake.script(
        "getFile",
        ok(json!({ "file_id": "voice-1", "file_size": OGG.len(), "file_path": "voice/file_1.oga" })),
    );
    fake.script("file_1.oga", Reply::Bytes(OGG.to_vec()));
}

/// An env-file holding `lines`.
fn env_file(dir: &Scratch, lines: &str) -> PathBuf {
    let path = dir.path("stt.env");
    fs::write(&path, lines).expect("env-file");
    path
}

/// A transcriber posting to `fake` with the test key and `timeout`.
fn stt_on(fake: &FakeBotApi, dir: &Scratch, timeout: Duration) -> Stt {
    let mut config = SttConfig::new(env_file(dir, &format!("GROQ_API_KEY={STT_KEY}\n")));
    config.base_url = fake.base_url();
    config.timeout = timeout;
    Stt::new(config)
}

/// A file of `len` bytes named `name` in `dir`.
fn audio_file(dir: &Scratch, name: &str, len: u64) -> PathBuf {
    let path = dir.path(name);
    fs::File::create(&path)
        .and_then(|f| f.set_len(len))
        .expect("audio file");
    path
}

/// The turn of one owner voice note downloaded from `fake` and transcribed by `stt`.
fn voice_turn(fake: &FakeBotApi, dir: &Scratch, stt: Stt) -> Turn {
    voice_file(fake);
    let config = IngressConfig {
        owner_ids: vec![OWNER],
        bot_username: Some("test_bot".to_owned()),
        inbox_dir: dir.path("inbox"),
        zone: Zone::utc(),
        max_file_bytes: 20 * 1024 * 1024,
        max_media_bytes: 128 * 1024 * 1024,
        timing: Timing::default(),
        reactions: ReactionConfig::default(),
    };
    let ports = Ports {
        files: Box::new(client(fake)),
        stt: Some(Box::new(stt)),
        texts: None,
    };
    let mut ingress = Ingress::new(config, ports);
    let update = Update::from_value(voice_update(9)).expect("update");
    let now = Instant::now();
    let mut outputs = ingress.accept(&update, now);
    outputs.extend(ingress.poll(now + Duration::from_secs(60)));
    match outputs.as_slice() {
        [IngressOutput::Turn(turn)] => turn.clone(),
        _ => panic!("one turn expected: {outputs:?}"),
    }
}

/// The note of a voice note that was not transcribed.
fn untranscribed(turn: &Turn) -> String {
    format!(
        "[voice message could not be transcribed automatically; the audio is available at: {}]",
        turn.attachments[0].path.display()
    )
}

#[test]
fn a_voice_note_reaches_the_turn_as_its_quoted_transcript() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("stt-ok");
    let env = env_file(&dir, &format!("export GROQ_API_KEY=\"{STT_KEY}\"\n"));
    let extra = format!(
        "coalesce_quiet = 0\nstt_env_file = \"{}\"\nstt_base_url = \"{}\"\nstt_model = \"whisper-large-v3\"\n",
        env.display(),
        fake.base_url()
    );
    let config = write_config(&dir, &fake, 0o600, true, &extra);
    voice_file(&fake);
    fake.script("transcriptions", Reply::Bytes("привет из голоса\n".into()));
    fake.queue_updates(&[voice_update(9)]);
    let turn = first_turn(&fake, &config, SentTexts::new());
    assert_eq!(turn.text, "\"привет из голоса\"");
    let [request] = fake.calls("transcriptions").try_into().expect("one upload");
    assert_eq!(request.path, "/bot/audio/transcriptions");
    assert_eq!(
        request.headers["authorization"],
        format!("Bearer {STT_KEY}")
    );
    let body = String::from_utf8_lossy(&request.body);
    let saved = turn.attachments[0].path.file_name().expect("name");
    for part in [
        "name=\"model\"\r\n\r\nwhisper-large-v3\r\n".to_owned(),
        "name=\"response_format\"\r\n\r\ntext\r\n".to_owned(),
        format!(
            "name=\"file\"; filename=\"{}\"\r\nContent-Type: audio/ogg\r\n\r\nOggS",
            saved.to_string_lossy()
        ),
    ] {
        assert!(body.contains(&part), "{part:?} not in {body:?}");
    }
}

#[test]
fn stt_settings_default_to_the_reference_endpoint_and_need_the_env_file() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("stt-settings");
    let config = write_config(
        &dir,
        &fake,
        0o600,
        true,
        "stt_model = \"whisper-large-v3\"\n",
    );
    assert_eq!(Settings::load(Some(&config)).expect("settings").stt, None);
    let env = env_file(&dir, "");
    let extra = format!("stt_env_file = \"{}\"\n", env.display());
    let config = write_config(&dir, &fake, 0o600, true, &extra);
    let settings = Settings::load(Some(&config)).expect("settings");
    assert_eq!(settings.stt, Some(SttConfig::new(env)));
    let config = write_config(&dir, &fake, 0o600, true, "stt_base_url = \"ftp://x\"\n");
    let err = Settings::load(Some(&config)).expect_err("invalid base");
    assert!(err.to_string().contains("telegram.stt_base_url"), "{err}");
}

#[test]
fn an_http_error_gives_the_reference_error_text_and_the_file_note() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("stt-http");
    // Keys in sorted order, so the wire order does not depend on serde_json features.
    let body = json!({ "error": {
        "code": "invalid_api_key", "message": "Invalid API Key", "type": "invalid_request_error"
    }});
    fake.script("transcriptions", Reply::Json(401, body.clone()));
    fake.script("transcriptions", Reply::Json(401, body));
    let stt = stt_on(&fake, &dir, Duration::from_secs(2));
    let err = stt
        .transcribe_file(&audio_file(&dir, "a.ogg", 10))
        .expect_err("401");
    assert_eq!(
        err.to_string(),
        "API error: Error code: 401 - {'error': {'code': 'invalid_api_key', 'message': \
         'Invalid API Key', 'type': 'invalid_request_error'}}"
    );
    let turn = voice_turn(&fake, &dir, stt);
    assert_eq!(turn.text, untranscribed(&turn));
}

#[test]
fn a_timed_out_transcription_falls_back_to_the_file_note() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("stt-timeout");
    for _ in 0..2 {
        let late = Reply::Bytes(b"late".to_vec());
        fake.script(
            "transcriptions",
            Reply::Delayed(Duration::from_secs(1), Box::new(late)),
        );
    }
    let stt = stt_on(&fake, &dir, Duration::from_millis(200));
    let err = stt
        .transcribe_file(&audio_file(&dir, "a.ogg", 10))
        .expect_err("timeout");
    assert_eq!(err.to_string(), "Connection error: Request timed out.");
    let turn = voice_turn(&fake, &dir, stt);
    assert_eq!(turn.text, untranscribed(&turn));
}

#[test]
fn a_dropped_connection_is_a_connection_error() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("stt-drop");
    fake.script("transcriptions", Reply::Drop);
    let err = stt_on(&fake, &dir, Duration::from_secs(2))
        .transcribe_file(&audio_file(&dir, "a.ogg", 10))
        .expect_err("dropped");
    assert_eq!(err.to_string(), "Connection error: Connection error.");
}

#[test]
fn an_empty_transcript_gives_the_reference_empty_voice_note() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("stt-empty");
    fake.script("transcriptions", Reply::Bytes(b" \n".to_vec()));
    let turn = voice_turn(&fake, &dir, stt_on(&fake, &dir, Duration::from_secs(2)));
    assert_eq!(
        turn.text,
        "[The user sent a voice message but it came through empty or inaudible — \
         speech-to-text returned no words. Do not guess at the content; ask the user to \
         resend or type it out.]"
    );
}

#[test]
fn a_file_over_the_size_cap_is_rejected_before_upload() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("stt-big");
    let err = stt_on(&fake, &dir, Duration::from_secs(2))
        .transcribe_file(&audio_file(&dir, "big.ogg", MAX_FILE_BYTES + 1))
        .expect_err("too large");
    assert_eq!(err.to_string(), "File too large: 25.0MB (max 25MB)");
    assert!(fake.calls("transcriptions").is_empty());
}

#[test]
fn file_checks_and_a_missing_key_fail_with_the_reference_texts() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("stt-checks");
    let stt = stt_on(&fake, &dir, Duration::from_secs(2));
    let audio = audio_file(&dir, "a.ogg", 10);
    let link = dir.path("link.ogg");
    std::os::unix::fs::symlink(&audio, &link).expect("symlink");
    let folder = dir.path("folder.ogg");
    fs::create_dir(&folder).expect("dir");
    let missing = dir.path("missing.ogg");
    let text = audio_file(&dir, "notes.TXT", 10);
    let cases = [
        (
            &link,
            format!("Path is a symbolic link: {}", link.display()),
        ),
        (
            &missing,
            format!("Audio file not found: {}", missing.display()),
        ),
        (&folder, format!("Path is not a file: {}", folder.display())),
        (
            &text,
            "Unsupported format: .TXT. Supported: .aac, .caf, .flac, .m4a, .mp3, .mp4, .mpeg, \
             .mpga, .oga, .ogg, .opus, .wav, .webm"
                .to_owned(),
        ),
    ];
    for (path, expected) in cases {
        let err = stt.transcribe_file(path).expect_err("refused");
        assert_eq!(err.to_string(), expected);
    }
    env_file(&dir, "OTHER=1\nGROQ_API_KEY=  \n");
    let err = stt.transcribe_file(&audio).expect_err("no key");
    assert_eq!(err.to_string(), "GROQ_API_KEY not set");
    assert!(fake.calls("transcriptions").is_empty());
}

#[test]
fn the_key_never_appears_in_errors_or_debug() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("stt-key");
    let echo = json!({ "error": { "message": format!("Invalid API Key {STT_KEY}") } });
    fake.script("transcriptions", Reply::Json(401, echo));
    let stt = stt_on(&fake, &dir, Duration::from_secs(2));
    let err = stt
        .transcribe_file(&audio_file(&dir, "a.ogg", 10))
        .expect_err("401");
    assert_eq!(
        err.to_string(),
        "API error: Error code: 401 - {'error': {'message': 'Invalid API Key ***'}}"
    );
    assert!(!format!("{stt:?}").contains(STT_KEY));
}

// --- Transcript echo -------------------------------------------------------------------------

const SPOKEN: &str = "привет из голоса";
const ANSWER: &str = "Готово";

/// What the owner voice notes of one turn produced in `ulei-tg`.
struct VoiceRun {
    /// `sendMessage` bodies recorded by the time `submit_turn` reached the engine.
    before_submit: Vec<Value>,
    /// Every `sendMessage` body, once the turn's answer went out.
    sent: Vec<Value>,
    /// The prompt the engine received.
    prompt: String,
}

/// Runs `ulei-tg` over owner voice notes sent together, one per entry of `transcriptions`, the
/// fake STT endpoint's answers to their uploads in order; `extra` closes the `[telegram]`
/// section. The notes make one turn; the engine holds its `submit_turn` until the messages sent
/// before it are read, then answers it with [`ANSWER`].
fn voice_run(tag: &str, transcriptions: Vec<Reply>, extra: &str) -> VoiceRun {
    let fake = FakeBotApi::start();
    let dir = Scratch::new(tag);
    let env = env_file(&dir, &format!("GROQ_API_KEY={STT_KEY}\n"));
    let extra = format!(
        "coalesce_quiet = \"0.3\"\nstt_env_file = \"{}\"\nstt_base_url = \"{}\"\n{extra}",
        env.display(),
        fake.base_url()
    );
    let config = write_config(&dir, &fake, 0o600, true, &extra);
    bot_user(&fake);
    let notes: Vec<Value> = (9..)
        .zip(&transcriptions)
        .map(|(id, _)| voice_update(id))
        .collect();
    for transcription in transcriptions {
        voice_file(&fake);
        fake.script("transcriptions", transcription);
    }
    for id in [100, 101, 102] {
        fake.script("sendMessage", ok(json!({ "message_id": id })));
    }
    let engine = FakeC6::start(&dir.socket);
    let submitted = Gate::new();
    engine.script("submit_turn", Answer::Held(submitted.clone()));
    engine.script(
        "turn_events",
        support::fake_c6::ok(json!({
            "turn": "t-0",
            "state": "ended",
            "events": [
                { "seq": 1, "type": "start" },
                { "seq": 2, "type": "final", "text": ANSWER },
                { "seq": 3, "type": "end", "outcome": "completed" }
            ]
        })),
    );
    fake.queue_updates(&notes);
    let child = spawn(&config);
    engine.wait_for(1);
    let before_submit = fake
        .calls("sendMessage")
        .iter()
        .map(Recorded::json)
        .collect();
    submitted.open(support::fake_c6::ok(
        json!({ "accepted": true, "turn": "t-0", "position": 0, "queue_depth": 0 }),
    ));
    fake.wait_until("the answer", |requests| {
        requests
            .iter()
            .any(|r| r.method() == "sendMessage" && r.json()["text"] == json!(ANSWER))
    });
    sigterm(&child);
    let output = finish(child);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let (method, body) = &engine.calls()[0];
    assert_eq!(method, "submit_turn");
    VoiceRun {
        before_submit,
        sent: fake
            .calls("sendMessage")
            .iter()
            .map(Recorded::json)
            .collect(),
        prompt: body["prompt"].as_str().expect("prompt").to_owned(),
    }
}

#[test]
fn a_transcript_is_echoed_silently_before_its_turn_and_stays_out_of_the_prompt() {
    let run = voice_run("echo", vec![Reply::Bytes(format!("{SPOKEN}\n").into())], "");
    let echo = format!("🎙️ \"{SPOKEN}\"");
    assert_eq!(texts(&run.before_submit), [echo.as_str()]);
    assert_eq!(texts(&run.sent), [echo.as_str(), ANSWER]);
    assert_eq!(run.sent[0]["chat_id"], json!(CHAT));
    assert_eq!(run.sent[0].get("reply_parameters"), None);
    assert_eq!(run.sent[0]["disable_notification"], json!(true));
    assert_eq!(run.prompt, format!("\"{SPOKEN}\""));
}

#[test]
fn notes_of_one_turn_are_echoed_once_each_in_order_skipping_an_empty_one() {
    let run = voice_run(
        "echo-order",
        vec![
            Reply::Bytes("один".into()),
            Reply::Bytes("".into()),
            Reply::Bytes("два".into()),
        ],
        "",
    );
    assert_eq!(texts(&run.before_submit), ["🎙️ \"один\"", "🎙️ \"два\""]);
    assert_eq!(texts(&run.sent)[2..], [ANSWER]);
}

#[test]
fn a_failed_transcription_is_not_echoed() {
    let run = voice_run(
        "echo-failed",
        vec![Reply::Json(500, json!({ "error": { "message": "boom" } }))],
        "",
    );
    assert_eq!(texts(&run.sent), [ANSWER]);
    assert!(
        run.prompt
            .starts_with("[voice message could not be transcribed automatically;"),
        "{}",
        run.prompt
    );
}

#[test]
fn an_empty_transcription_is_not_echoed() {
    let run = voice_run("echo-empty", vec![Reply::Bytes(" \n".into())], "");
    assert_eq!(texts(&run.sent), [ANSWER]);
    assert!(
        run.prompt
            .starts_with("[The user sent a voice message but it came through empty"),
        "{}",
        run.prompt
    );
}

#[test]
fn with_the_echo_off_the_turn_is_the_same_and_nothing_is_echoed() {
    let run = voice_run(
        "echo-off",
        vec![Reply::Bytes(SPOKEN.into())],
        "stt_echo_transcripts = false\n",
    );
    assert_eq!(texts(&run.sent), [ANSWER]);
    assert_eq!(run.prompt, format!("\"{SPOKEN}\""));
}

#[test]
fn the_echo_key_defaults_to_on_and_a_non_boolean_is_an_error_naming_it() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("echo-key");
    let config = write_config(&dir, &fake, 0o600, true, "");
    assert!(
        Settings::load(Some(&config))
            .expect("settings")
            .stt_echo_transcripts
    );
    let config = write_config(&dir, &fake, 0o600, true, "stt_echo_transcripts = \"no\"\n");
    let err = Settings::load(Some(&config)).expect_err("not a boolean");
    assert!(
        err.to_string().contains("telegram.stt_echo_transcripts"),
        "{err}"
    );
}

/// A fake guest gate on loopback: records each request's `Authorization` and JSON body and
/// answers what `decide` says for the body: a status and a JSON text.
struct FakeGate {
    url: String,
    seen: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
}

impl FakeGate {
    fn start(decide: fn(&Value) -> (u16, &'static str)) -> Self {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!(
            "http://{}/internal/telegram",
            listener.local_addr().expect("addr")
        );
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut reader = BufReader::new(stream);
                let (mut length, mut auth) = (0, String::new());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("authorization:") {
                        line["authorization:".len()..].trim().clone_into(&mut auth);
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    continue;
                }
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let (status, answer) = decide(&body);
                record.lock().expect("lock").push((auth, body));
                let _ = write!(
                    reader.into_inner(),
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
            }
        });
        Self { url, seen }
    }

    fn bodies(&self) -> Vec<Value> {
        self.seen
            .lock()
            .expect("lock")
            .iter()
            .map(|(_, b)| b.clone())
            .collect()
    }
}

fn owner_message(id: i64, from: i64, text: &str, reply_to: Option<i64>) -> Value {
    let mut update = json!({
        "update_id": id,
        "message": {
            "message_id": id, "date": 1_790_000_000,
            "chat": { "id": CHAT, "type": "private" },
            "from": { "id": from, "is_bot": false, "first_name": "Owner" },
            "text": text
        }
    });
    if text.starts_with('/') {
        update["message"]["entities"] =
            json!([{ "type": "bot_command", "offset": 0, "length": text.len() }]);
    }
    if let Some(reply_to) = reply_to {
        update["message"]["reply_to_message"] = json!({
            "message_id": reply_to, "date": 1_790_000_000,
            "chat": { "id": CHAT, "type": "private" },
            "from": { "id": 7, "is_bot": true, "first_name": "Bot" },
            "text": "Synthetic preview of a guest request"
        });
    }
    update
}

fn button(id: i64, data: &str) -> Value {
    json!({
        "update_id": id,
        "callback_query": {
            "id": format!("cb-{id}"), "chat_instance": "synthetic",
            "from": { "id": OWNER, "is_bot": false, "first_name": "Owner" },
            "data": data,
            "message": {
                "message_id": 500, "date": 1_790_000_000,
                "chat": { "id": CHAT, "type": "private" },
                "from": { "id": 7, "is_bot": true, "first_name": "Bot" },
                "text": "Synthetic preview"
            }
        }
    })
}

/// The protocol traffic of the gate tests: two buttons, two gate commands, two replies, a plain
/// message and a stranger's gate word.
fn protocol_updates() -> Vec<Value> {
    vec![
        button(1, "ap:1"),
        button(2, "bi:2"),
        owner_message(3, OWNER, "шли", None),
        owner_message(4, OWNER, "/requests", None),
        owner_message(5, OWNER, "answer one", Some(400)),
        owner_message(6, OWNER, "answer two", Some(401)),
        owner_message(7, OWNER, "plain hello", None),
        owner_message(8, 5, "шли", None),
    ]
}

/// Every output of the channel until `stop` is requested.
struct Everything {
    turns: Vec<Turn>,
    others: usize,
}

impl Dispatch for Everything {
    fn dispatch(&mut self, output: IngressOutput) {
        match output {
            IngressOutput::Turn(turn) => self.turns.push(turn),
            _ => self.others += 1,
        }
    }
}

/// Runs the channel of `config` on `updates`, stopping it once `done` holds or after `WAIT`.
fn run_until(
    fake: &FakeBotApi,
    config: &Path,
    updates: &[Value],
    done: impl Fn() -> bool + Send + 'static,
) -> Everything {
    bot_user(fake);
    fake.queue_updates(updates);
    let settings = Settings::load(Some(config)).expect("settings");
    let stop = Stop::new();
    let channel =
        Channel::start(settings, stop.clone(), Box::new(SentTexts::new())).expect("start");
    let watchdog = stop.clone();
    std::thread::spawn(move || {
        let started = Instant::now();
        while !done() && started.elapsed() < WAIT {
            std::thread::sleep(Duration::from_millis(20));
        }
        // Room for the updates the gate gave back and the coalescer's quiet window.
        std::thread::sleep(Duration::from_millis(600));
        watchdog.request();
    });
    let mut all = Everything {
        turns: Vec::new(),
        others: 0,
    };
    channel.run(&mut all).expect("run");
    all
}

fn gate_config(dir: &Scratch, fake: &FakeBotApi, gate: &FakeGate, mode: u32) -> PathBuf {
    let token = dir.path("gate-token");
    fs::write(&token, "synthetic-gate-token\n").expect("gate token");
    fs::set_permissions(&token, fs::Permissions::from_mode(mode)).expect("chmod");
    let extra = format!(
        "coalesce_quiet = \"0.05\"\nguest_gate_url = \"{}\"\nguest_gate_token_file = \"{}\"\n",
        gate.url,
        token.display()
    );
    write_config(dir, fake, 0o600, true, &extra)
}

#[test]
fn the_owners_protocol_updates_go_to_the_guest_gate_first() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("guest-gate");
    let gate = FakeGate::start(|body| {
        let data = body.pointer("/callback_query/data").and_then(Value::as_str);
        let text = body.pointer("/message/text").and_then(Value::as_str);
        match (data, text) {
            (Some("ap:1"), _) | (_, Some("шли")) => (200, r#"{"handled": true}"#),
            (Some(_), _) | (_, Some("answer one")) => (200, r#"{"handled": false}"#),
            _ => (500, r#"{"error": "synthetic"}"#),
        }
    });
    let config = gate_config(&dir, &fake, &gate, 0o600);
    let seen = Arc::clone(&gate.seen);
    let all = run_until(&fake, &config, &protocol_updates(), move || {
        seen.lock().expect("lock").len() >= 6
    });

    // In order, only the owner's protocol updates, each with the bearer token.
    let bodies = gate.bodies();
    let ids: Vec<i64> = bodies
        .iter()
        .map(|b| b["update_id"].as_i64().unwrap_or(0))
        .collect();
    assert_eq!(ids, [1, 2, 3, 4, 5, 6]);
    for (auth, _) in gate.seen.lock().expect("lock").iter() {
        assert_eq!(auth, "Bearer synthetic-gate-token");
    }
    assert_eq!(
        bodies[4],
        json!({"update_id": 5, "message": {
            "message_id": 5, "text": "answer one",
            "chat": { "id": CHAT, "type": "private" },
            "from": { "id": OWNER, "is_bot": false, "first_name": "Owner" },
            "reply_to_message": {"message_id": 400}
        }})
    );
    assert_eq!(bodies[0]["callback_query"]["data"], "ap:1");
    assert_eq!(bodies[0]["callback_query"]["message"]["message_id"], 500);

    // What the gate did not take went the usual way: both replies (not handled, and a failed
    // call) and the plain message; never the eaten word, the failed command or a button.
    let text: Vec<&str> = all.turns.iter().map(|t| t.text.as_str()).collect();
    let joined = text.join("\n");
    for want in ["plain hello", "answer one", "answer two"] {
        assert!(joined.contains(want), "{want} not in {joined}");
    }
    assert!(!joined.contains("/requests"), "{joined}");
    assert!(!joined.contains("шли"), "{joined}");
    assert_eq!(all.others, 0);
}

#[test]
fn without_the_gate_settings_protocol_words_reach_the_engine() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("no-gate");
    let config = write_config(&dir, &fake, 0o600, true, "coalesce_quiet = \"0.05\"\n");
    assert_eq!(
        Settings::load(Some(&config)).expect("settings").guest_gate,
        None
    );
    let all = run_until(&fake, &config, &protocol_updates(), || false);
    let joined: Vec<&str> = all.turns.iter().map(|t| t.text.as_str()).collect();
    let joined = joined.join("\n");
    for want in ["шли", "answer one", "answer two", "plain hello"] {
        assert!(joined.contains(want), "{want} not in {joined}");
    }
}

#[test]
fn guest_gate_settings_come_in_pairs_and_keep_their_secret() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("gate-settings");
    let gate = FakeGate::start(|_| (200, "{}"));
    let config = gate_config(&dir, &fake, &gate, 0o600);
    let settings = Settings::load(Some(&config)).expect("settings");
    let shown = format!("{settings:?}");
    assert!(
        !shown.contains("synthetic-gate-token") && !shown.contains(&gate.url),
        "{shown}"
    );
    let loaded = settings.guest_gate.expect("gate");
    assert_eq!(loaded.token, "synthetic-gate-token");
    assert_eq!(loaded.url, gate.url);
    assert_eq!(loaded.timeout, Duration::from_secs(145));

    let config = gate_config(&dir, &fake, &gate, 0o640);
    let err = Settings::load(Some(&config)).expect_err("group-readable token");
    assert!(
        err.to_string().contains("telegram.guest_gate_token_file"),
        "{err}"
    );
    assert!(err.to_string().contains("must have mode 0600"), "{err}");

    for (extra, want) in [
        (
            "guest_gate_url = \"http://127.0.0.1:8792/internal/telegram\"\n",
            "telegram.guest_gate_url needs telegram.guest_gate_token_file",
        ),
        (
            "guest_gate_token_file = \"/nonexistent\"\n",
            "telegram.guest_gate_token_file needs telegram.guest_gate_url",
        ),
    ] {
        let config = write_config(&dir, &fake, 0o600, true, extra);
        let err = Settings::load(Some(&config)).expect_err(want);
        assert_eq!(err.to_string(), want);
    }
    let token = dir.path("gate-token");
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).expect("chmod");
    let extra = format!(
        "guest_gate_url = \"http://192.0.2.1:8792/internal/telegram?secret=1\"\nguest_gate_token_file = \"{}\"\n",
        token.display()
    );
    let config = write_config(&dir, &fake, 0o600, true, &extra);
    let err = Settings::load(Some(&config)).expect_err("remote http");
    assert!(
        err.to_string()
            .starts_with("telegram.guest_gate_url must be"),
        "{err}"
    );
    assert!(!err.to_string().contains("secret"), "{err}");
}
