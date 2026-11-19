//! The Bot API transport against a scripted fake server on loopback.

#[allow(
    dead_code,
    reason = "tests/tg.rs uses other parts of the shared support"
)]
mod support {
    pub(crate) mod fake_bot_api;
    pub(crate) mod fake_clock;
}

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::fake_bot_api::{FakeBotApi, Reply, fail, ok};
use support::fake_clock::FakeClock;
use ulei_channels::bot_api::{
    Client, Config, DELIVERY_FAILED_NOTICE, ErrorKind, FileInfo, FloodLimiter, MediaKind, Poller,
    Sender, Stop, SystemClock, TextMessage, Upload,
};

const TOKEN: &str = "123456:TEST";
const CHAT: i64 = 1001;

fn config(fake: &FakeBotApi) -> Config {
    let mut config = Config::new(TOKEN, Some(&fake.base_url()));
    config.file_base_url = fake.file_base_url();
    config.read_timeout = Duration::from_secs(2);
    config.poll_timeout = Duration::from_secs(1);
    config
}

fn client(fake: &FakeBotApi) -> Client {
    Client::new(config(fake))
}

fn sender(fake: &FakeBotApi) -> (Sender, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new(Duration::from_millis(250)));
    (Sender::new(client(fake), clock.clone()), clock)
}

fn poller(fake: &FakeBotApi) -> (Poller, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new(Duration::ZERO));
    (Poller::new(client(fake), clock.clone(), Stop::new()), clock)
}

fn sent(id: i64) -> Reply {
    ok(json!({ "message_id": id, "chat": { "id": CHAT } }))
}

fn update(id: i64) -> Value {
    json!({ "update_id": id, "message": { "message_id": id, "text": "hi" } })
}

fn temp_path(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("ulei-bot-api-{}-{n}-{tag}", std::process::id()))
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

#[test]
fn get_me_posts_json_to_the_token_path() {
    let fake = FakeBotApi::start();
    fake.script(
        "getMe",
        ok(json!({ "id": 42, "is_bot": true, "username": "test_bot" })),
    );
    let me = client(&fake).get_me().expect("getMe");
    assert_eq!((me.id, me.username.as_deref()), (42, Some("test_bot")));
    let call = &fake.calls("getMe")[0];
    assert_eq!(call.path, "/bot123456:TEST/getMe");
    assert_eq!(call.headers["content-type"], "application/json");
}

#[test]
fn error_envelopes_map_to_kinds_and_keep_descriptions() {
    let fake = FakeBotApi::start();
    let cases = [
        (
            fail(400, "Bad Request: can't parse entities", None),
            ErrorKind::BadRequest,
        ),
        (fail(401, "Unauthorized", None), ErrorKind::Unauthorized),
        (fail(404, "Not Found", None), ErrorKind::Unauthorized),
        (
            fail(403, "Forbidden: bot was blocked by the user", None),
            ErrorKind::Forbidden,
        ),
        (
            fail(
                409,
                "Conflict: terminated by other getUpdates request",
                None,
            ),
            ErrorKind::Conflict,
        ),
        (
            fail(
                429,
                "Too Many Requests: retry after 3",
                Some(json!({ "retry_after": 3 })),
            ),
            ErrorKind::RetryAfter(secs(3)),
        ),
        (
            fail(
                400,
                "Bad Request: group chat was upgraded",
                Some(json!({ "migrate_to_chat_id": -100_123 })),
            ),
            ErrorKind::ChatMigrated(-100_123),
        ),
        (
            fail(
                429,
                "Too Many Requests",
                Some(json!({ "retry_after": 1e300 })),
            ),
            ErrorKind::Server(429),
        ),
        (fail(502, "Bad Gateway", None), ErrorKind::Server(502)),
        (
            Reply::Json(500, json!("not an envelope")),
            ErrorKind::Server(500),
        ),
        (
            Reply::Json(200, json!({ "ok": false })),
            ErrorKind::InvalidResponse,
        ),
    ];
    let client = client(&fake);
    for (reply, kind) in cases {
        fake.script("sendChatAction", reply);
        let err = client
            .send_chat_action(CHAT, "typing")
            .expect_err("must fail");
        assert_eq!(err.kind, kind, "{err}");
    }
    fake.script(
        "sendChatAction",
        fail(400, "Bad Request: can't parse entities", None),
    );
    let err = client
        .send_chat_action(CHAT, "typing")
        .expect_err("must fail");
    assert_eq!(err.description, "Bad Request: can't parse entities");
    assert!(err.is_bad_request() && !err.is_network_error());
}

#[test]
fn classifiers_follow_the_reference() {
    let fake = FakeBotApi::start();
    let client = client(&fake);
    fake.script(
        "sendChatAction",
        fail(
            409,
            "Conflict: terminated by other getUpdates request",
            None,
        ),
    );
    let conflict = client.send_chat_action(CHAT, "typing").expect_err("409");
    assert!(conflict.is_polling_conflict() && !conflict.is_network_error());
    fake.script("sendChatAction", fail(403, "Forbidden", None));
    assert!(
        client
            .send_chat_action(CHAT, "typing")
            .expect_err("403")
            .is_auth_error()
    );
    fake.script("sendChatAction", Reply::Drop);
    let dropped = client.send_chat_action(CHAT, "typing").expect_err("drop");
    assert_eq!(dropped.kind, ErrorKind::Network);
    assert!(dropped.is_network_error() && dropped.is_retryable());
}

#[test]
fn errors_and_debug_never_show_the_token() {
    let fake = FakeBotApi::start();
    fake.script(
        "getMe",
        fail(401, &format!("Unauthorized for /bot{TOKEN}/getMe"), None),
    );
    let err = client(&fake).get_me().expect_err("401");
    assert!(!err.to_string().contains(TOKEN) && !format!("{err:?}").contains(TOKEN));
    assert!(err.description.contains("123456:***"));

    let closed = TcpListenerGone::port();
    let mut cfg = Config::new(TOKEN, Some(&format!("http://127.0.0.1:{closed}/bot")));
    cfg.connect_timeout = secs(2);
    let err = Client::new(cfg.clone()).get_me().expect_err("refused");
    assert!(err.is_network_error());
    assert!(!format!("{err} {err:?} {cfg:?}").contains(TOKEN));
}

/// A loopback port with nothing listening on it.
struct TcpListenerGone;

impl TcpListenerGone {
    fn port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    }
}

#[test]
fn send_message_sends_the_reference_fields() {
    let fake = FakeBotApi::start();
    fake.script("sendMessage", sent(7));
    let mut cfg = config(&fake);
    cfg.disable_link_previews = true;
    let message = TextMessage {
        parse_mode: Some("HTML".to_owned()),
        reply_to: Some(5),
        disable_notification: true,
        ..TextMessage::plain(CHAT, "<b>hi</b>")
    };
    let sent = Client::new(cfg).send_message(&message).expect("send");
    assert_eq!(sent.message_id, 7);
    let body = fake.calls("sendMessage")[0].json();
    assert_eq!(
        body,
        json!({
            "chat_id": CHAT,
            "text": "<b>hi</b>",
            "parse_mode": "HTML",
            "reply_parameters": { "message_id": 5 },
            "link_preview_options": { "is_disabled": true },
            "disable_notification": true,
        })
    );
}

#[test]
fn reaction_sets_one_emoji_or_clears() {
    let fake = FakeBotApi::start();
    let client = client(&fake);
    client
        .set_message_reaction(CHAT, 9, Some("👀"))
        .expect("set");
    client.set_message_reaction(CHAT, 9, None).expect("clear");
    let calls = fake.calls("setMessageReaction");
    assert_eq!(
        calls[0].json()["reaction"],
        json!([{ "type": "emoji", "emoji": "👀" }])
    );
    assert!(calls[1].json().get("reaction").is_none());
}

#[test]
fn send_retries_transport_errors_with_back_off() {
    let fake = FakeBotApi::start();
    fake.script("sendMessage", Reply::Drop);
    fake.script("sendMessage", fail(502, "Bad Gateway", None));
    fake.script("sendMessage", sent(3));
    let (sender, clock) = sender(&fake);
    let sent = sender
        .send_message(&TextMessage::plain(CHAT, "x"))
        .expect("third try");
    assert_eq!(sent.message_id, 3);
    assert_eq!(clock.slept(), vec![secs(1), secs(2)]);
}

#[test]
fn send_honours_retry_after_and_gives_up_after_three_attempts() {
    let fake = FakeBotApi::start();
    for _ in 0..3 {
        fake.script(
            "sendMessage",
            fail(
                429,
                "Too Many Requests: retry after 4",
                Some(json!({ "retry_after": 4 })),
            ),
        );
    }
    let (sender, clock) = sender(&fake);
    let err = sender
        .send_message(&TextMessage::plain(CHAT, "x"))
        .expect_err("flooded");
    assert_eq!(err.retry_after(), Some(secs(4)));
    assert_eq!(fake.calls("sendMessage").len(), 3);
    assert_eq!(clock.slept(), vec![secs(4), secs(4)]);
}

#[test]
fn send_drops_a_deleted_reply_target_and_stops_on_other_bad_requests() {
    let fake = FakeBotApi::start();
    fake.script(
        "sendMessage",
        fail(400, "Bad Request: message to be replied not found", None),
    );
    fake.script("sendMessage", sent(11));
    let (sender, _) = sender(&fake);
    let message = TextMessage {
        reply_to: Some(5),
        ..TextMessage::plain(CHAT, "x")
    };
    assert_eq!(
        sender.send_message(&message).expect("resent").message_id,
        11
    );
    let calls = fake.calls("sendMessage");
    assert!(calls[0].json().get("reply_parameters").is_some());
    assert!(calls[1].json().get("reply_parameters").is_none());

    fake.script(
        "sendMessage",
        fail(400, "Bad Request: can't parse entities", None),
    );
    let err = sender.send_message(&message).expect_err("parse error");
    assert!(err.is_bad_request());
    assert_eq!(fake.calls("sendMessage").len(), 3);
}

#[test]
fn send_does_not_resend_after_a_response_timeout() {
    let fake = FakeBotApi::start();
    for id in [1, 2] {
        fake.script(
            "sendMessage",
            Reply::Delayed(Duration::from_millis(1500), Box::new(sent(id))),
        );
    }
    let mut cfg = config(&fake);
    cfg.read_timeout = Duration::from_millis(300);
    let sender = Sender::new(Client::new(cfg), Arc::new(FakeClock::new(Duration::ZERO)));
    let err = sender
        .send_message(&TextMessage::plain(CHAT, "x"))
        .expect_err("timeout");
    assert!(err.is_timeout());
    let message = TextMessage::plain(CHAT, "y");
    let outer = sender.send_with_retry(|| sender.send_message(&message), |_| panic!("no notice"));
    assert!(outer.expect_err("timeout").is_timeout());
    assert_eq!(fake.calls("sendMessage").len(), 2);
}

#[test]
fn delivery_retries_twice_then_sends_the_failure_notice() {
    let fake = FakeBotApi::start();
    let (sender, clock) = sender(&fake);
    let mut calls = 0;
    let mut notice = None;
    let result: Result<(), _> = sender.send_with_retry(
        || {
            calls += 1;
            fake.script(
                "sendChatAction",
                fail(403, "Forbidden: bot was blocked by the user", None),
            );
            sender.client().send_chat_action(CHAT, "typing")
        },
        |text| notice = Some(text.to_owned()),
    );
    assert_eq!(result.expect_err("blocked").kind, ErrorKind::Forbidden);
    assert_eq!(calls, 3);
    let jitter = Duration::from_millis(250);
    assert_eq!(clock.slept(), vec![secs(2) + jitter, secs(4) + jitter]);
    assert_eq!(notice.as_deref(), Some(DELIVERY_FAILED_NOTICE));
}

#[test]
fn delivery_returns_a_too_long_failure_at_once() {
    let fake = FakeBotApi::start();
    fake.script(
        "sendMessage",
        fail(400, "Bad Request: message is too long", None),
    );
    let (sender, clock) = sender(&fake);
    let message = TextMessage::plain(CHAT, "x");
    let err = sender
        .send_with_retry(|| sender.send_message(&message), |_| panic!("no notice"))
        .expect_err("too long");
    assert!(err.is_message_too_long());
    assert_eq!(clock.slept(), Vec::<Duration>::new());
}

#[test]
fn flood_limiter_paces_per_chat_and_globally() {
    let start = Instant::now();
    let mut limiter = FloodLimiter::new(secs(1), 3, secs(1));
    assert_eq!(limiter.delay(1, start), Duration::ZERO);
    limiter.record(1, start);
    assert_eq!(
        limiter.delay(1, start + Duration::from_millis(400)),
        Duration::from_millis(600)
    );
    limiter.record(2, start);
    limiter.record(3, start + Duration::from_millis(100));
    assert_eq!(
        limiter.delay(4, start + Duration::from_millis(200)),
        Duration::from_millis(800)
    );
    assert_eq!(limiter.delay(4, start + secs(1)), Duration::ZERO);
}

#[test]
fn sender_waits_a_second_between_messages_to_one_chat() {
    let fake = FakeBotApi::start();
    fake.script("sendMessage", sent(1));
    fake.script("sendMessage", sent(2));
    let (sender, clock) = sender(&fake);
    sender
        .send_message(&TextMessage::plain(CHAT, "a"))
        .expect("first");
    sender
        .send_message(&TextMessage::plain(CHAT, "b"))
        .expect("second");
    assert_eq!(clock.slept(), vec![secs(1)]);
}

#[test]
fn poller_confirms_each_batch_and_shutdown_marks_it_read() {
    let fake = FakeBotApi::start();
    fake.script("getUpdates", ok(json!([update(10), update(11)])));
    fake.script("getUpdates", ok(json!([])));
    fake.script("getUpdates", ok(json!([update(12)])));
    let (mut poller, _) = poller(&fake);
    poller.start(true).expect("deleteWebhook");
    let first = poller.next_batch().expect("poll").expect("batch");
    assert_eq!(
        first.iter().map(|u| u.update_id).collect::<Vec<_>>(),
        [10, 11]
    );
    assert_eq!(first[0].payload["message"]["text"], "hi");
    let second = poller.next_batch().expect("poll").expect("batch");
    assert_eq!(second[0].update_id, 12);
    poller.shutdown().expect("confirm");

    assert_eq!(
        fake.calls("deleteWebhook")[0].json(),
        json!({ "drop_pending_updates": true })
    );
    let polls: Vec<Value> = fake
        .calls("getUpdates")
        .iter()
        .map(support::fake_bot_api::Recorded::json)
        .collect();
    assert_eq!(polls[0].get("offset"), None);
    assert_eq!(polls[0]["timeout"], 1);
    assert!(
        polls[0]["allowed_updates"]
            .as_array()
            .expect("list")
            .contains(&json!("message_reaction"))
    );
    assert_eq!(polls[1]["offset"], 12);
    assert_eq!(polls[2]["offset"], 12);
    assert_eq!(
        (polls[3]["offset"].clone(), polls[3]["timeout"].clone()),
        (json!(13), json!(0))
    );
}

#[test]
fn poller_backs_off_and_re_bootstraps_after_network_errors() {
    let fake = FakeBotApi::start();
    fake.script("getUpdates", ok(json!([update(1)])));
    fake.script("getUpdates", Reply::Drop);
    fake.script("getUpdates", fail(502, "Bad Gateway", None));
    fake.script("getUpdates", ok(json!([update(2)])));
    let (mut poller, clock) = poller(&fake);
    poller.next_batch().expect("poll");
    let batch = poller.next_batch().expect("recovered").expect("batch");
    assert_eq!(batch[0].update_id, 2);
    assert_eq!(clock.slept(), vec![secs(5), secs(10)]);
    let rebootstraps = fake.calls("deleteWebhook");
    assert_eq!(rebootstraps.len(), 2);
    assert_eq!(
        rebootstraps[0].json(),
        json!({ "drop_pending_updates": false })
    );
    assert_eq!(
        fake.calls("getUpdates").last().expect("poll").json()["offset"],
        2
    );
}

#[test]
fn poller_waits_out_a_conflict_and_ends_the_other_session() {
    let fake = FakeBotApi::start();
    fake.script(
        "getUpdates",
        fail(
            409,
            "Conflict: terminated by other getUpdates request",
            None,
        ),
    );
    fake.script("getUpdates", ok(json!([update(1)])));
    let (mut poller, clock) = poller(&fake);
    assert!(poller.next_batch().expect("recovered").is_some());
    assert_eq!(clock.slept(), vec![secs(20)]);
    assert_eq!(
        fake.calls("deleteWebhook")[0].json(),
        json!({ "drop_pending_updates": true })
    );
}

#[test]
fn poller_gives_up_after_five_conflict_retries() {
    let fake = FakeBotApi::start();
    for _ in 0..6 {
        fake.script(
            "getUpdates",
            fail(
                409,
                "Conflict: terminated by other getUpdates request",
                None,
            ),
        );
    }
    let (mut poller, clock) = poller(&fake);
    let err = poller.next_batch().expect_err("fatal");
    assert!(err.is_polling_conflict());
    assert_eq!(
        clock.slept(),
        vec![secs(20), secs(30), secs(40), secs(50), secs(60)]
    );
}

#[test]
fn poller_stops_on_an_invalid_token_and_sleeps_on_flood_control() {
    let fake = FakeBotApi::start();
    fake.script(
        "getUpdates",
        fail(429, "Too Many Requests", Some(json!({ "retry_after": 3 }))),
    );
    fake.script("getUpdates", fail(401, "Unauthorized", None));
    let (mut poller, clock) = poller(&fake);
    let err = poller.next_batch().expect_err("fatal");
    assert_eq!(err.kind, ErrorKind::Unauthorized);
    assert_eq!(clock.slept(), vec![Duration::from_millis(3500)]);
}

#[test]
fn poller_backs_off_other_errors_by_half_again_up_to_thirty_seconds() {
    let fake = FakeBotApi::start();
    for _ in 0..3 {
        fake.script("getUpdates", fail(403, "Forbidden", None));
    }
    fake.script("getUpdates", ok(json!([update(1)])));
    let (mut poller, clock) = poller(&fake);
    poller.next_batch().expect("recovered");
    assert_eq!(
        clock.slept(),
        vec![
            secs(1),
            Duration::from_millis(1500),
            Duration::from_millis(2250)
        ]
    );
}

#[test]
fn stop_cuts_a_back_off_short_and_leaves_the_batch_unconfirmed() {
    let fake = FakeBotApi::start();
    fake.script("getUpdates", Reply::Drop);
    let stop = Stop::new();
    let mut poller = Poller::new(client(&fake), Arc::new(SystemClock), stop.clone());
    let started = Instant::now();
    let waiter = std::thread::spawn(move || poller.next_batch().map(|b| b.is_none()));
    std::thread::sleep(Duration::from_millis(200));
    stop.request();
    assert!(waiter.join().expect("thread").expect("no error"));
    assert!(started.elapsed() < secs(3));

    let fake = FakeBotApi::start();
    fake.script(
        "getUpdates",
        Reply::Delayed(Duration::from_millis(300), Box::new(ok(json!([update(5)])))),
    );
    let stop = Stop::new();
    let mut poller = Poller::new(client(&fake), Arc::new(SystemClock), stop.clone());
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        stop.request();
    });
    assert!(poller.next_batch().expect("poll").is_none());
    stopper.join().expect("thread");
    assert_eq!(poller.offset(), None);
}

#[test]
fn a_stop_during_a_long_poll_waits_for_it_and_the_next_start_gets_its_batch() {
    let fake = FakeBotApi::start();
    fake.queue_updates(&[update(1)]);
    let stop = Stop::new();
    let mut first_run = Poller::new(client(&fake), Arc::new(SystemClock), stop.clone());
    let first = first_run.next_batch().expect("poll").expect("batch");
    assert_eq!(first[0].update_id, 1);

    fake.queue_updates(&[update(2)]);
    fake.delay_polls(Duration::from_millis(300));
    let started = Instant::now();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        stop.request();
    });
    assert!(first_run.next_batch().expect("poll").is_none());
    stopper.join().expect("thread");
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(first_run.offset(), Some(2));
    first_run.shutdown().expect("confirm");

    let (mut restarted, _) = poller(&fake);
    let again = restarted.next_batch().expect("poll").expect("batch");
    assert_eq!(again.iter().map(|u| u.update_id).collect::<Vec<_>>(), [2]);
}

#[test]
fn a_batch_with_an_update_without_update_id_is_left_unconfirmed_and_polled_again() {
    let fake = FakeBotApi::start();
    let broken = json!({ "message": { "message_id": 4, "text": "hi" } });
    fake.script("getUpdates", ok(json!([broken, update(5)])));
    fake.script("getUpdates", ok(json!([update(5)])));
    let (mut poller, clock) = poller(&fake);
    let batch = poller.next_batch().expect("recovered").expect("batch");
    assert_eq!(batch.iter().map(|u| u.update_id).collect::<Vec<_>>(), [5]);
    assert_eq!(clock.slept(), vec![secs(1)]);
    let polls = fake.calls("getUpdates");
    assert_eq!(polls.len(), 2);
    assert_eq!(polls[1].json().get("offset"), None);
}

#[test]
fn download_streams_a_file_to_a_private_path() {
    let fake = FakeBotApi::start();
    let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    fake.script(
        "getFile",
        ok(json!({ "file_id": "F1", "file_size": bytes.len(), "file_path": "voice/file_1.oga" })),
    );
    fake.script("file_1.oga", Reply::Bytes(bytes.clone()));
    let client = client(&fake);
    let file = client.get_file("F1").expect("getFile");
    let dest = temp_path("voice.oga");
    let n = client.download_to_path(&file, &dest).expect("download");
    assert_eq!(n, bytes.len() as u64);
    assert_eq!(fs::read(&dest).expect("read"), bytes);
    assert_eq!(
        fs::metadata(&dest).expect("meta").permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fake.calls("file_1.oga")[0].path,
        "/file/bot123456:TEST/voice/file_1.oga"
    );
    fs::remove_file(dest).expect("cleanup");
}

#[test]
fn download_refuses_files_over_the_limit() {
    let fake = FakeBotApi::start();
    let mut cfg = config(&fake);
    cfg.max_file_bytes = 1000;
    let client = Client::new(cfg);
    let declared = FileInfo {
        file_id: "F".to_owned(),
        file_size: Some(1001),
        file_path: Some("documents/big.bin".to_owned()),
    };
    let mut sink = Vec::new();
    assert_eq!(
        client
            .download(&declared, &mut sink)
            .expect_err("declared")
            .kind,
        ErrorKind::TooLarge
    );
    assert!(fake.calls("big.bin").is_empty());

    fake.script("big.bin", Reply::Bytes(vec![7; 5000]));
    let undeclared = FileInfo {
        file_size: None,
        ..declared
    };
    let dest = temp_path("big.bin");
    let err = client
        .download_to_path(&undeclared, &dest)
        .expect_err("streamed");
    assert_eq!(err.kind, ErrorKind::TooLarge);
    assert!(!dest.exists());
}

#[test]
fn a_slow_download_succeeds_while_each_gap_stays_under_the_read_timeout() {
    let fake = FakeBotApi::start();
    let mut cfg = config(&fake);
    cfg.read_timeout = Duration::from_millis(400);
    let client = Client::new(cfg);
    let parts: Vec<Vec<u8>> = (0..6u8).map(|i| vec![i; 1000]).collect();
    fake.script(
        "slow.bin",
        Reply::Trickle(Duration::from_millis(100), parts.clone()),
    );
    let file = FileInfo {
        file_id: "F".to_owned(),
        file_size: Some(6000),
        file_path: Some("documents/slow.bin".to_owned()),
    };
    let mut sink = Vec::new();
    let started = Instant::now();
    let n = client.download(&file, &mut sink).expect("slow download");
    assert!(
        started.elapsed() > Duration::from_millis(400),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(n, 6000);
    assert_eq!(sink, parts.concat());
}

#[test]
fn download_to_an_existing_path_keeps_that_file_and_fetches_nothing() {
    let fake = FakeBotApi::start();
    let client = client(&fake);
    let dest = temp_path("taken.txt");
    fs::write(&dest, b"owner's file").expect("write");
    let file = FileInfo {
        file_id: "F".to_owned(),
        file_size: Some(3),
        file_path: Some("documents/new.txt".to_owned()),
    };
    let err = client
        .download_to_path(&file, &dest)
        .expect_err("existing dest");
    assert_eq!(err.kind, ErrorKind::Io);
    assert_eq!(fs::read(&dest).expect("read"), b"owner's file");
    assert!(fake.requests().is_empty());
    fs::remove_file(dest).expect("cleanup");
}

#[test]
fn a_request_body_the_server_stops_reading_fails_after_the_write_timeout() {
    // The kernel completes the handshake into the backlog, but nobody reads the body.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}/bot", silent.local_addr().expect("addr"));
    let mut cfg = Config::new(TOKEN, Some(&base));
    cfg.read_timeout = secs(30);
    cfg.write_timeout = Duration::from_millis(200);
    let client = Client::new(cfg);
    let message = TextMessage::plain(CHAT, "x".repeat(16 * 1024 * 1024));
    let started = Instant::now();
    let err = client.send_message(&message).expect_err("stalled write");
    assert_eq!(err.kind, ErrorKind::Timeout);
    assert!(started.elapsed() < secs(10), "{:?}", started.elapsed());
}

#[test]
fn upload_streams_a_multipart_form() {
    let fake = FakeBotApi::start();
    fake.script("sendDocument", sent(21));
    let path = temp_path("report.pdf");
    let content = b"%PDF-1.4 synthetic\r\n--not-a-boundary\r\n".repeat(100);
    fs::write(&path, &content).expect("write");
    let upload = Upload {
        kind: MediaKind::Document,
        chat_id: CHAT,
        path: path.clone(),
        file_name: None,
        caption: Some("c".repeat(1100)),
        parse_mode: None,
        reply_to: Some(4),
        disable_notification: false,
    };
    let (sender, _) = sender(&fake);
    assert_eq!(sender.send_media(&upload).expect("upload").message_id, 21);
    fs::remove_file(path).expect("cleanup");

    let call = &fake.calls("sendDocument")[0];
    assert_eq!(call.headers["content-length"], call.body.len().to_string());
    let boundary = call.headers["content-type"]
        .strip_prefix("multipart/form-data; boundary=")
        .expect("multipart");
    let parts = split_form(&call.body, boundary);
    assert_eq!(parts["chat_id"].1, b"1001");
    assert_eq!(parts["caption"].1.len(), 1024);
    assert_eq!(parts["reply_parameters"].1, br#"{"message_id":4}"#);
    let (filename, body) = &parts["document"];
    let expected = upload.path.file_name().expect("name").to_string_lossy();
    assert_eq!(filename.as_deref(), Some(expected.as_ref()));
    assert_eq!(body, &content);
}

/// Parts of a form body by field name: optional file name and raw content.
fn split_form(
    body: &[u8],
    boundary: &str,
) -> std::collections::HashMap<String, (Option<String>, Vec<u8>)> {
    let delimiter = format!("--{boundary}");
    let mut parts = std::collections::HashMap::new();
    let mut rest = body;
    while let Some(start) = find(rest, delimiter.as_bytes()) {
        rest = &rest[start + delimiter.len()..];
        if rest.starts_with(b"--") {
            break;
        }
        let rest_after_crlf = &rest[2..];
        let head_end = find(rest_after_crlf, b"\r\n\r\n").expect("part head");
        let head = String::from_utf8_lossy(&rest_after_crlf[..head_end]).into_owned();
        let content_start = head_end + 4;
        let next = find(
            &rest_after_crlf[content_start..],
            format!("\r\n{delimiter}").as_bytes(),
        )
        .expect("part end");
        let content = rest_after_crlf[content_start..content_start + next].to_vec();
        let name = attribute(&head, "name").expect("field name");
        parts.insert(name, (attribute(&head, "filename"), content));
        rest = &rest_after_crlf[content_start + next + 2..];
    }
    parts
}

fn attribute(head: &str, key: &str) -> Option<String> {
    let marker = format!(" {key}=\"");
    let start = head.find(&marker)? + marker.len();
    let end = head[start..].find('"')?;
    Some(head[start..start + end].to_owned())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
