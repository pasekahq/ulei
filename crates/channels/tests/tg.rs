//! `ulei-tg` and the transport's `FileSource` against the fake Bot API on loopback.

#[allow(
    dead_code,
    reason = "tests/bot_api.rs uses other parts of the shared support"
)]
mod support {
    pub(crate) mod fake_bot_api;
}

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::fake_bot_api::{FakeBotApi, Recorded, Reply, fail, ok};
use ulei_channels::bot_api::{Client, Config};
use ulei_channels::ingress::{
    FileSource, Ingress, IngressConfig, Output as IngressOutput, Ports, ReactionConfig, RemoteFile,
    Timing, Update, Zone,
};
use ulei_channels::tg::Settings;

const TOKEN: &str = "123456:SECRET-TOKEN-OF-THE-TEST-BOT";
const OWNER: i64 = 42;
const CHAT: i64 = 1001;
const WAIT: Duration = Duration::from_secs(5);

/// A private scratch directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ulei-tg-{}-{n}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes the token file (mode `token_mode`) and `ulei.toml` pointing at `fake`; `owner_ids`
/// goes in only when `with_owner` holds.
fn write_config(dir: &Scratch, fake: &FakeBotApi, token_mode: u32, with_owner: bool) -> PathBuf {
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
         base_url = \"{base}\"\nfile_base_url = \"{files}\"\npoll_timeout = 1\nread_timeout = 2\n",
        socket = dir.path("c6.sock").display(),
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

#[test]
fn token_file_open_to_others_is_a_startup_error() {
    let fake = FakeBotApi::start();
    let dir = Scratch::new("mode");
    let config = write_config(&dir, &fake, 0o644, true);
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
    let config = write_config(&dir, &fake, 0o600, false);
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
    let config = write_config(&dir, &fake, 0o600, true);
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
    let config = write_config(&dir, &fake, 0o600, true);
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
    let config = write_config(&dir, &fake, 0o600, true);
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
    let config = write_config(&dir, &fake, 0o600, true);
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
