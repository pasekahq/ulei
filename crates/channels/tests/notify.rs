//! `ulei-tg notify` and `notify::notify` against the fake Bot API on loopback.

#[allow(
    dead_code,
    reason = "tests/tg.rs and tests/bot_api.rs use other parts of the shared support"
)]
mod support {
    pub(crate) mod fake_bot_api;
    pub(crate) mod fake_clock;
}

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use support::fake_bot_api::{FakeBotApi, Reply, fail, ok};
use support::fake_clock::FakeClock;
use ulei_channels::bot_api::{Client, Config, ErrorKind, Sender};
use ulei_channels::notify::{ChatReceipt, MAX_TEXT_BYTES, NotifyError, Receipt, notify};
use ulei_channels::render::utf16_len;

const TOKEN: &str = "123456:SECRET-TOKEN-OF-THE-NOTIFY-BOT";
const OWNER: i64 = 42;
const SECOND_OWNER: i64 = 43;
const NOTICE: &str = "synthetic notice: 2 uncertain inbox events";

/// A private scratch directory and the C6 and C1 socket paths beside it, removed on drop.
struct Scratch {
    dir: PathBuf,
    c6: PathBuf,
    c1: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let tmp = std::env::temp_dir();
        let dir = tmp.join(format!("ulei-notify-{pid}-{n}-{tag}"));
        fs::create_dir_all(&dir).expect("scratch dir");
        // Outside `dir`: a Unix socket path has at most 107 bytes.
        let c6 = tmp.join(format!("nt-{pid}-{n}-6.sock"));
        let c1 = tmp.join(format!("nt-{pid}-{n}-1.sock"));
        Self { dir, c6, c1 }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
        let _ = fs::remove_file(&self.c6);
        let _ = fs::remove_file(&self.c1);
    }
}

/// Writes the token file and a `ulei.toml` naming the engine's C6 socket, the inbox's C1 socket
/// and the fake Bot API; `telegram` is the `[telegram]` section after `base_url`.
fn write_config(dir: &Scratch, fake: &FakeBotApi, telegram: &str) -> PathBuf {
    let token = dir.path("token");
    fs::write(&token, format!("{TOKEN}\n")).expect("token");
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).expect("chmod");
    let text = format!(
        "[engine]\nc6_socket = \"{c6}\"\n\n[inbox]\nsocket = \"{c1}\"\n\n[telegram]\n\
         base_url = \"{base}\"\n{telegram}",
        c6 = dir.c6.display(),
        c1 = dir.c1.display(),
        base = fake.base_url(),
        telegram = telegram.replace("TOKEN_FILE", &token.display().to_string()),
    );
    let path = dir.path("ulei.toml");
    fs::write(&path, text).expect("config");
    path
}

fn owners(ids: &str) -> String {
    format!("token_file = \"TOKEN_FILE\"\nowner_ids = \"{ids}\"\n")
}

/// Runs `ulei-tg notify --config CONFIG` with `extra` arguments and `stdin` as its input.
fn run(config: &Path, extra: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ulei-tg"))
        .arg("notify")
        .arg("--config")
        .arg(config)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ulei-tg notify");
    let mut input = child.stdin.take().expect("stdin");
    match input.write_all(stdin) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
        written => written.expect("write stdin"),
    }
    drop(input);
    child.wait_with_output().expect("wait")
}

fn sent(id: i64) -> Reply {
    ok(json!({ "message_id": id, "chat": { "id": OWNER } }))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Each `sendMessage` body the fake received, in order.
fn bodies(fake: &FakeBotApi) -> Vec<Value> {
    fake.requests()
        .iter()
        .map(support::fake_bot_api::Recorded::json)
        .collect()
}

/// A listener on `path` that never accepts; `Ok` from `accept` means a client connected.
fn listening(path: &Path) -> UnixListener {
    let listener = UnixListener::bind(path).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    listener
}

fn untouched(listener: &UnixListener) -> bool {
    matches!(listener.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
}

#[test]
fn a_notice_goes_to_every_owner_as_plain_text_without_the_engine_or_the_inbox() {
    let dir = Scratch::new("send");
    let fake = FakeBotApi::start();
    fake.script("sendMessage", sent(10));
    fake.script("sendMessage", sent(11));
    let config = write_config(&dir, &fake, &owners(&format!("{OWNER}, {SECOND_OWNER}")));
    let engine = listening(&dir.c6);
    let inbox = listening(&dir.c1);

    let output = run(&config, &[], format!("{NOTICE}\n").as_bytes());

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    assert_eq!(
        stdout(&output),
        format!(
            "{{\"chats\":[{{\"chat_id\":{OWNER},\"message_ids\":[10]}},\
             {{\"chat_id\":{SECOND_OWNER},\"message_ids\":[11]}}]}}\n"
        )
    );
    let methods: Vec<String> = fake
        .requests()
        .iter()
        .map(|r| r.method().to_owned())
        .collect();
    assert_eq!(methods, ["sendMessage", "sendMessage"]);
    assert_eq!(
        bodies(&fake),
        [
            json!({ "chat_id": OWNER, "text": NOTICE }),
            json!({ "chat_id": SECOND_OWNER, "text": NOTICE }),
        ]
    );
    assert!(untouched(&engine), "the engine's C6 socket was called");
    assert!(untouched(&inbox), "the inbox's C1 socket was called");
}

#[test]
fn a_long_text_from_a_file_is_split_and_every_part_has_its_receipt() {
    let dir = Scratch::new("split");
    let fake = FakeBotApi::start();
    fake.script("sendMessage", sent(20));
    fake.script("sendMessage", sent(21));
    let config = write_config(&dir, &fake, &owners(&OWNER.to_string()));
    let text = "line of a synthetic stall report\n".repeat(200);
    let file = dir.path("notice.txt");
    fs::write(&file, &text).expect("notice");

    let output = run(&config, &["--file", file.to_str().expect("path")], b"");

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let receipt: Value = serde_json::from_str(&stdout(&output)).expect("one JSON line");
    assert_eq!(
        receipt,
        json!({ "chats": [{ "chat_id": OWNER, "message_ids": [20, 21] }] })
    );
    let parts = bodies(&fake);
    assert_eq!(parts.len(), 2);
    for part in &parts {
        let part = part["text"].as_str().expect("text");
        assert!(
            utf16_len(part) <= 4096,
            "a part of {} units",
            utf16_len(part)
        );
        assert!(part.starts_with("line of a synthetic stall report"));
    }
}

#[test]
fn a_send_failure_exits_one_with_one_line_naming_neither_token_nor_text() {
    let dir = Scratch::new("fail");
    let fake = FakeBotApi::start();
    // A response timeout is not retried: the message may have arrived.
    fake.script(
        "sendMessage",
        Reply::Delayed(Duration::from_secs(3), Box::new(sent(30))),
    );
    let config = write_config(&dir, &fake, &format!("{}read_timeout = 1\n", owners("42")));

    let output = run(&config, &[], NOTICE.as_bytes());

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    let line = stderr(&output);
    assert_eq!(line.lines().count(), 1, "{line}");
    assert!(
        line.starts_with("ulei-tg notify: sent 0 of 1 messages, then telegram Timeout"),
        "{line}"
    );
    assert!(!line.contains("SECRET") && !line.contains(NOTICE), "{line}");
    assert_eq!(fake.calls("sendMessage").len(), 1);
}

#[test]
fn an_empty_text_is_refused_before_any_send() {
    let dir = Scratch::new("empty");
    let fake = FakeBotApi::start();
    let config = write_config(&dir, &fake, &owners("42"));

    let output = run(&config, &[], b" \n\t\n");

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "ulei-tg notify: the text is empty\n");
    assert!(fake.requests().is_empty());
}

#[test]
fn a_text_over_the_limit_is_refused_before_any_send() {
    let dir = Scratch::new("long");
    let fake = FakeBotApi::start();
    let config = write_config(&dir, &fake, &owners("42"));
    let mut text = vec![b'a'; MAX_TEXT_BYTES];
    text.extend_from_slice(b"\nb");

    let output = run(&config, &[], &text);

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        stderr(&output),
        format!("ulei-tg notify: the text is over {MAX_TEXT_BYTES} bytes\n")
    );
    assert!(fake.requests().is_empty());
}

#[test]
fn a_config_without_a_token_or_an_owner_is_refused_before_any_send() {
    for (tag, telegram, key) in [
        (
            "token",
            "owner_ids = \"42\"\n",
            "missing telegram.token_file",
        ),
        (
            "owner",
            "token_file = \"TOKEN_FILE\"\n",
            "missing telegram.owner_ids",
        ),
    ] {
        let dir = Scratch::new(tag);
        let fake = FakeBotApi::start();
        let config = write_config(&dir, &fake, telegram);

        let output = run(&config, &[], NOTICE.as_bytes());

        assert_eq!(output.status.code(), Some(3), "{tag}");
        assert_eq!(stdout(&output), "", "{tag}");
        assert_eq!(stderr(&output), format!("ulei-tg notify: {key}\n"), "{tag}");
        assert!(fake.requests().is_empty(), "{tag}");
    }
}

fn sender(fake: &FakeBotApi) -> (Sender, Arc<FakeClock>) {
    let mut config = Config::new(TOKEN, Some(&fake.base_url()));
    config.read_timeout = Duration::from_secs(2);
    let clock = Arc::new(FakeClock::new(Duration::from_millis(250)));
    (Sender::new(Client::new(config), clock.clone()), clock)
}

#[test]
fn the_library_returns_the_receipt_of_each_owner_chat() {
    let fake = FakeBotApi::start();
    fake.script("sendMessage", sent(40));
    fake.script("sendMessage", sent(41));
    let (sender, _) = sender(&fake);

    let receipt = notify(&sender, &[OWNER, SECOND_OWNER], NOTICE).expect("sent");

    assert_eq!(
        receipt,
        Receipt {
            chats: vec![
                ChatReceipt {
                    chat_id: OWNER,
                    message_ids: vec![40]
                },
                ChatReceipt {
                    chat_id: SECOND_OWNER,
                    message_ids: vec![41]
                },
            ]
        }
    );
}

#[test]
fn the_library_returns_the_error_that_outlasts_the_delivery_retries() {
    let fake = FakeBotApi::start();
    fake.script("sendMessage", sent(50));
    for _ in 0..3 {
        fake.script(
            "sendMessage",
            fail(403, "Forbidden: bot was blocked by the user", None),
        );
    }
    let (sender, clock) = sender(&fake);

    let err = notify(&sender, &[OWNER, SECOND_OWNER], NOTICE).expect_err("blocked");

    match err {
        NotifyError::Send { sent, total, error } => {
            assert_eq!((sent, total), (1, 2));
            assert_eq!(error.kind, ErrorKind::Forbidden);
        }
        other => panic!("not a send failure: {other}"),
    }
    let jitter = Duration::from_millis(250);
    let secs = Duration::from_secs;
    assert_eq!(clock.slept(), vec![secs(2) + jitter, secs(4) + jitter]);
    assert_eq!(fake.calls("sendMessage").len(), 4);
}

#[test]
fn the_library_refuses_a_call_without_an_owner_before_any_send() {
    let fake = FakeBotApi::start();
    let (sender, _) = sender(&fake);

    let err = notify(&sender, &[], NOTICE).expect_err("no owner");

    assert!(matches!(err, NotifyError::NoOwner) && err.is_refusal());
    assert!(fake.requests().is_empty());
}
