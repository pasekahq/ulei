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
}

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::fake_bot_api::{FakeBotApi, Recorded, Reply, fail, ok};
use support::fake_c6::FakeC6;
use support::fake_clock::FakeClock;
use ulei_channels::bot_api::{Client, Config, Sender, Stop};
use ulei_channels::ingress::{
    FileSource, Ingress, IngressConfig, Output as IngressOutput, Ports, ReactionConfig, RemoteFile,
    Timing, Turn, Update, Zone,
};
use ulei_channels::tg::{Channel, Dispatch, Outbound, SentTexts, Settings};

const TOKEN: &str = "123456:SECRET-TOKEN-OF-THE-TEST-BOT";
const OWNER: i64 = 42;
const CHAT: i64 = 1001;
const OTHER_CHAT: i64 = 1002;
const WAIT: Duration = Duration::from_secs(5);

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

/// The output of `child` once it exits; panics after [`WAIT`].
fn finish(child: Child) -> Output {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    rx.recv_timeout(WAIT)
        .expect("ulei-tg did not exit")
        .expect("wait")
}

fn sigterm(child: &Child) {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(status.success());
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A fake engine on the socket of `dir`'s config that accepts the next `turns` turns.
fn engine_taking(dir: &Scratch, turns: usize) -> FakeC6 {
    let engine = FakeC6::start(&dir.socket);
    for n in 0..turns {
        engine.script(
            "submit_turn",
            support::fake_c6::ok(
                json!({ "accepted": true, "turn": format!("t-{n}"), "position": 0, "queue_depth": 0 }),
            ),
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

/// Waits until `done` holds for the fake's requests; panics after [`WAIT`].
fn wait_until(fake: &FakeBotApi, what: &str, done: impl Fn(&[Recorded]) -> bool) {
    let until = Instant::now() + WAIT;
    while !done(&fake.requests()) {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `sendChatAction` calls to `chat`.
fn typing_calls(requests: &[Recorded], chat: i64) -> usize {
    requests
        .iter()
        .filter(|r| r.method() == "sendChatAction" && r.json()["chat_id"] == json!(chat))
        .count()
}

/// The delivery thread over `fake` with virtual time for its flood limits and retries.
fn outbound(fake: &FakeBotApi, texts: &SentTexts) -> Outbound {
    let sender = Sender::new(client(fake), Arc::new(FakeClock::new(Duration::ZERO)));
    Outbound::start(Arc::new(sender), texts.clone()).expect("outbound thread")
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
    wait_until(&fake, "the notice", |r| {
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
fn typing_repeats_while_a_turn_runs_and_stops_when_it_ends() {
    let fake = FakeBotApi::start();
    let out = outbound(&fake, &SentTexts::new());
    let outbox = out.outbox();
    // The ended turn's next tick falls before the running turn's second one.
    outbox.turn_started(OTHER_CHAT).expect("queued");
    outbox.turn_ended(OTHER_CHAT).expect("queued");
    outbox.turn_started(CHAT).expect("queued");
    wait_until(&fake, "a second typing action", |r| {
        typing_calls(r, CHAT) == 2
    });
    outbox.turn_ended(CHAT).expect("queued");
    drop(outbox);
    out.finish().expect("finished");
    let requests = fake.requests();
    assert_eq!(typing_calls(&requests, OTHER_CHAT), 1);
    assert_eq!(typing_calls(&requests, CHAT), 2);
    let action = fake.calls("sendChatAction")[0].json();
    assert_eq!(action["action"], json!("typing"));
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
