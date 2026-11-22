//! The `MEDIA:` files of a final reply through the delivery threads on the fake Bot API: the
//! upload method by extension and directive, albums, the reference's fallbacks, the notice of a
//! refused file, and one upload per file whatever the text needed.

#[allow(
    dead_code,
    reason = "the other test crates use other parts of the shared support"
)]
mod support {
    pub(crate) mod fake_bot_api;
    pub(crate) mod fake_clock;
}

use std::fs::{self, File, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use support::fake_bot_api::{FakeBotApi, Recorded, Reply, fail, ok};
use support::fake_clock::FakeClock;
use ulei_channels::bot_api::{Client, Config, Sender};
use ulei_channels::render::{SendOptions, plan_reply};
use ulei_channels::tg::{Outbound, SentTexts};

const CHAT: i64 = 1001;
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsynthetic-png";
const OGG: &[u8] = b"OggS\x00\x02synthetic-opus";
const PDF: &[u8] = b"%PDF-1.4\nsynthetic\n%%EOF\n";

/// A scratch directory of synthetic files, removed on drop.
struct Files(PathBuf);

impl Files {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ulei-media-{}-{n}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }

    /// Writes `name` with `data`; its path as text.
    fn write(&self, name: &str, data: &[u8]) -> String {
        let path = self.0.join(name);
        fs::write(&path, data).expect("write");
        path.display().to_string()
    }

    /// A sparse file `name` of `len` bytes.
    fn sized(&self, name: &str, len: u64) -> String {
        let path = self.0.join(name);
        File::create(&path)
            .and_then(|f| f.set_len(len))
            .expect("sized file");
        path.display().to_string()
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).display().to_string()
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Delivers `text` as the final answer to `CHAT` replying to message 7 over `config`; every
/// upload and message request in order.
fn deliver_with(fake: &FakeBotApi, config: Config, text: &str) -> Vec<Recorded> {
    let clock = Arc::new(FakeClock::new(Duration::ZERO));
    let sender = Sender::new(Client::new(config), clock as Arc<_>);
    let out = Outbound::start(Arc::new(sender), SentTexts::new()).expect("outbound");
    out.outbox()
        .deliver(CHAT, Some(7), text, true)
        .expect("queued");
    out.finish().expect("finished");
    fake.requests()
        .into_iter()
        .filter(|r| r.method().starts_with("send"))
        .collect()
}

fn deliver(fake: &FakeBotApi, text: &str) -> Vec<Recorded> {
    deliver_with(
        fake,
        Config::new("123456:TEST", Some(&fake.base_url())),
        text,
    )
}

/// Answers the next `count` calls of each of `methods` as sent messages.
fn accept(fake: &FakeBotApi, methods: &[&str], count: usize) {
    for method in methods {
        for n in 0..count {
            let id = i64::try_from(n).expect("small") + 100;
            fake.script(method, ok(json!({ "message_id": id })));
        }
    }
}

fn methods(requests: &[Recorded]) -> Vec<&str> {
    requests.iter().map(Recorded::method).collect()
}

/// The file field `field` of an upload to `CHAT`: its name and bytes.
fn uploaded(request: &Recorded, field: &str) -> (String, Vec<u8>) {
    let mut form = request.form();
    assert_eq!(form["chat_id"].1, CHAT.to_string().into_bytes());
    assert!(!form.contains_key("reply_parameters"));
    let (name, data) = form.remove(field).expect("file field");
    (name.expect("file name"), data)
}

/// The `MarkdownV2` text the owner gets for `line`.
fn shown(line: &str) -> Value {
    json!(plan_reply(line, None, true, &SendOptions::default()).messages[0].text)
}

#[test]
fn the_text_goes_first_then_photos_then_the_other_files_in_tag_order() {
    let files = Files::new("order");
    let (png, ogg, pdf) = (
        files.write("chart.png", PNG),
        files.write("note.ogg", OGG),
        files.write("report.pdf", PDF),
    );
    let fake = FakeBotApi::start();
    accept(
        &fake,
        &["sendMessage", "sendPhoto", "sendVoice", "sendDocument"],
        1,
    );
    let requests = deliver(
        &fake,
        &format!("Вот отчёт\nMEDIA:{pdf}\n[[audio_as_voice]]\nMEDIA:{ogg}\nи график MEDIA:{png}"),
    );
    assert_eq!(
        methods(&requests),
        ["sendMessage", "sendPhoto", "sendDocument", "sendVoice"]
    );
    let text = requests[0].json();
    assert_eq!(text["text"], shown("Вот отчёт\n\nи график"));
    assert_eq!(text["reply_parameters"]["message_id"], json!(7));
    assert_eq!(
        uploaded(&requests[1], "photo"),
        ("chart.png".into(), PNG.to_vec())
    );
    assert_eq!(
        uploaded(&requests[2], "document"),
        ("report.pdf".into(), PDF.to_vec())
    );
    assert_eq!(
        uploaded(&requests[3], "voice"),
        ("note.ogg".into(), OGG.to_vec())
    );
}

#[test]
fn the_upload_method_follows_the_extension_and_the_directives() {
    let files = Files::new("route");
    let song = files.write("song.mp3", b"ID3synthetic");
    let clip = files.write("clip.mp4", b"synthetic-mp4");
    let ogg = files.write("note.ogg", OGG);
    let bmp = files.write("scan.bmp", b"BMsynthetic");
    let big = files.sized("poster.png", 10_000_001);
    let fake = FakeBotApi::start();
    accept(&fake, &["sendAudio", "sendVideo", "sendDocument"], 3);
    let requests = deliver(
        &fake,
        &format!("MEDIA:{song} MEDIA:{clip} MEDIA:{ogg} MEDIA:{bmp} MEDIA:{big}"),
    );
    assert_eq!(
        methods(&requests),
        [
            "sendDocument",
            "sendAudio",
            "sendVideo",
            "sendDocument",
            "sendDocument"
        ]
    );
    assert_eq!(uploaded(&requests[0], "document").0, "poster.png");
    assert_eq!(uploaded(&requests[3], "document").0, "note.ogg");

    let fake = FakeBotApi::start();
    let png = files.write("chart.png", PNG);
    accept(&fake, &["sendDocument", "sendVoice", "sendAudio"], 1);
    let requests = deliver(
        &fake,
        &format!("[[as_document]] MEDIA:{png} [[audio_as_voice]] MEDIA:{ogg} MEDIA:{song}"),
    );
    assert_eq!(
        methods(&requests),
        ["sendDocument", "sendVoice", "sendAudio"]
    );
    assert_eq!(
        uploaded(&requests[0], "document"),
        ("chart.png".into(), PNG.to_vec())
    );
}

#[test]
fn photos_go_as_one_album_and_a_refused_album_photo_by_photo() {
    let files = Files::new("album");
    let (a, b) = (
        files.write("a.png", PNG),
        files.write("b.jpg", b"\xff\xd8\xffjpeg"),
    );
    let fake = FakeBotApi::start();
    fake.script(
        "sendMediaGroup",
        ok(json!([{ "message_id": 1 }, { "message_id": 2 }])),
    );
    let requests = deliver(&fake, &format!("MEDIA:{a} MEDIA:{b}"));
    assert_eq!(methods(&requests), ["sendMediaGroup"]);
    let form = requests[0].form();
    let media: Value = serde_json::from_slice(&form["media"].1).expect("media JSON");
    assert_eq!(
        media,
        json!([
            { "type": "photo", "media": "attach://photo0" },
            { "type": "photo", "media": "attach://photo1" },
        ])
    );
    assert_eq!(
        uploaded(&requests[0], "photo0"),
        ("a.png".into(), PNG.to_vec())
    );
    assert_eq!(uploaded(&requests[0], "photo1").0, "b.jpg");

    let fake = FakeBotApi::start();
    fake.script("sendMediaGroup", fail(400, "Bad Request: wrong file", None));
    accept(&fake, &["sendPhoto"], 2);
    let requests = deliver(&fake, &format!("MEDIA:{a} MEDIA:{b}"));
    assert_eq!(
        methods(&requests),
        ["sendMediaGroup", "sendPhoto", "sendPhoto"]
    );
    assert_eq!(uploaded(&requests[1], "photo").0, "a.png");
    assert_eq!(uploaded(&requests[2], "photo").0, "b.jpg");
}

#[test]
fn a_photo_telegram_refuses_goes_again_as_a_document_and_then_as_a_notice() {
    let files = Files::new("fallback");
    let png = files.write("wide.png", PNG);
    let fake = FakeBotApi::start();
    fake.script(
        "sendPhoto",
        fail(400, "Bad Request: PHOTO_INVALID_DIMENSIONS", None),
    );
    accept(&fake, &["sendDocument"], 1);
    let requests = deliver(&fake, &format!("MEDIA:{png}"));
    assert_eq!(methods(&requests), ["sendPhoto", "sendDocument"]);
    assert_eq!(
        uploaded(&requests[1], "document"),
        ("wide.png".into(), PNG.to_vec())
    );

    let fake = FakeBotApi::start();
    fake.script("sendPhoto", fail(400, "Bad Request: wrong file", None));
    fake.script("sendDocument", fail(400, "Bad Request: wrong file", None));
    accept(&fake, &["sendMessage"], 1);
    let requests = deliver(&fake, &format!("MEDIA:{png}"));
    assert_eq!(
        methods(&requests),
        ["sendPhoto", "sendDocument", "sendMessage"]
    );
    assert_eq!(
        requests[2].json()["text"],
        shown("⚠️ Couldn't deliver the file attachment (wide.png).")
    );
}

#[test]
fn each_refused_file_gets_one_line_and_the_text_and_other_files_still_go() {
    let files = Files::new("refused");
    let gone = files.path("gone.pdf");
    let gone_voice = files.path("gone.ogg");
    let locked = files.write("locked.pdf", PDF);
    fs::set_permissions(&locked, Permissions::from_mode(0o000)).expect("chmod");
    assert!(
        File::open(&locked).is_err(),
        "the test needs a non-root user"
    );
    let big = files.sized("big.pdf", 50_000_001);
    let fine = files.write("fine.pdf", PDF);
    let fake = FakeBotApi::start();
    accept(&fake, &["sendMessage"], 6);
    accept(&fake, &["sendDocument"], 1);
    let requests = deliver(
        &fake,
        &format!(
            "Готово\nMEDIA:{gone}\nMEDIA:{locked}\nMEDIA:{big}\nMEDIA:{fine}\n\
             [[audio_as_voice]] MEDIA:{gone_voice}"
        ),
    );
    assert_eq!(
        methods(&requests),
        [
            "sendMessage",
            "sendMessage",
            "sendMessage",
            "sendMessage",
            "sendDocument",
            "sendMessage"
        ]
    );
    let texts: Vec<Value> = requests
        .iter()
        .filter(|r| r.method() == "sendMessage")
        .map(|r| r.json()["text"].clone())
        .collect();
    assert_eq!(
        texts,
        [
            shown("Готово"),
            shown("⚠️ Couldn't deliver the file attachment (gone.pdf)."),
            shown("⚠️ Couldn't deliver the file attachment (locked.pdf)."),
            shown("⚠️ Couldn't deliver the file attachment (big.pdf)."),
            shown("⚠️ Couldn't deliver the audio attachment."),
        ]
    );
    assert_eq!(
        uploaded(&requests[4], "document"),
        ("fine.pdf".into(), PDF.to_vec())
    );
    fs::set_permissions(&locked, Permissions::from_mode(0o600)).expect("chmod back");
}

#[test]
fn a_retried_text_sends_each_file_once() {
    let files = Files::new("retry");
    let pdf = files.write("report.pdf", PDF);
    let fake = FakeBotApi::start();
    fake.script("sendMessage", fail(502, "Bad Gateway", None));
    accept(&fake, &["sendMessage", "sendDocument"], 1);
    let requests = deliver(&fake, &format!("Отчёт MEDIA:{pdf}"));
    assert_eq!(
        methods(&requests),
        ["sendMessage", "sendMessage", "sendDocument"]
    );
}

#[test]
fn an_upload_without_an_answer_is_not_sent_again() {
    let files = Files::new("timeout");
    let png = files.write("chart.png", PNG);
    let fake = FakeBotApi::start();
    fake.script(
        "sendPhoto",
        Reply::Delayed(
            Duration::from_secs(1),
            Box::new(ok(json!({ "message_id": 1 }))),
        ),
    );
    accept(&fake, &["sendMessage"], 1);
    let mut config = Config::new("123456:TEST", Some(&fake.base_url()));
    config.media_read_timeout = Duration::from_millis(200);
    let requests = deliver_with(&fake, config, &format!("MEDIA:{png}"));
    assert_eq!(methods(&requests), ["sendPhoto", "sendMessage"]);
    assert_eq!(
        requests[1].json()["text"],
        shown("⚠️ Couldn't deliver the file attachment (chart.png).")
    );
}

#[test]
fn an_interim_text_hides_its_tags_and_sends_no_file() {
    let files = Files::new("interim");
    let pdf = files.write("draft.pdf", PDF);
    let fake = FakeBotApi::start();
    accept(&fake, &["sendMessage", "sendDocument"], 1);
    let clock = Arc::new(FakeClock::new(Duration::ZERO));
    let config = Config::new("123456:TEST", Some(&fake.base_url()));
    let sender = Sender::new(Client::new(config), clock as Arc<_>);
    let out = Outbound::start(Arc::new(sender), SentTexts::new()).expect("outbound");
    out.outbox()
        .deliver(
            CHAT,
            None,
            format!("Пишу черновик\n[[audio_as_voice]]\n\n\nMEDIA:{pdf}\n"),
            false,
        )
        .expect("queued");
    out.finish().expect("finished");
    let requests: Vec<Recorded> = fake
        .requests()
        .into_iter()
        .filter(|r| r.method().starts_with("send") && r.method() != "sendChatAction")
        .collect();
    assert_eq!(methods(&requests), ["sendMessage"]);
    let interim = plan_reply("Пишу черновик", None, false, &SendOptions::default());
    assert_eq!(requests[0].json()["text"], json!(interim.messages[0].text));
}
