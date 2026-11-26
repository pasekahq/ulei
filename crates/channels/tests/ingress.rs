//! Ingress behaviour through its public API: fixtures of every update kind, owner allowlist,
//! coalescing timelines, attachments, STT, reactions and commands against a fake engine.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use ulei_channels::ingress::{
    Author, Command, Engine, EngineError, EngineStatus, FileSource, Ingress, IngressConfig,
    Interrupted, MessageKind, MessageTexts, NewSession, Output, Ports, ReactionConfig, RemoteFile,
    RouteSwitch, Timing, Transcriber, Turn, Update, UpdateKind, Zone, ZoneError, run_command,
};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0000";
const OGG: &[u8] = b"OggS\x00\x02rest";

fn fixture(name: &str) -> Update {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/ingress/{name}.json"));
    let bytes = std::fs::read(&path).expect("fixture");
    Update::from_slice(&bytes).expect("update")
}

fn empty<T: std::fmt::Debug>(items: &[T]) {
    assert_eq!(items.len(), 0, "{items:?}");
}

/// The pointer after the prompt's text to the image saved at `path`.
fn image_hint(path: &Path) -> String {
    format!("[Image attached at: {}]", path.display())
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

struct FakeFiles(HashMap<String, (String, Vec<u8>)>);

impl FakeFiles {
    fn standard() -> Self {
        let mut m = HashMap::new();
        for (id, path, data) in [
            ("photo-big", "photos/file_1.png", PNG),
            (
                "photo-2",
                "photos/file_2.jpg",
                b"\xff\xd8\xff\xe0jpeg" as &[u8],
            ),
            ("voice-1", "voice/file_3.oga", OGG),
            ("audio-1", "music/file_4.mp3", b"ID3\x04tag"),
            ("video-1", "videos/file_5.mp4", b"\x00\x00\x00\x18ftypmp42"),
            ("doc-pdf", "documents/file_6.pdf", b"%PDF-1.4"),
            ("doc-txt", "documents/file_7.txt", b"hello world"),
            ("not-image", "photos/file_8.jpg", b"<html>oops</html>"),
        ] {
            m.insert(id.to_owned(), (path.to_owned(), data.to_vec()));
        }
        Self(m)
    }
}

impl FileSource for FakeFiles {
    fn locate(&self, file_id: &str) -> io::Result<RemoteFile> {
        let (path, _) = self
            .0
            .get(file_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "fake"))?;
        Ok(RemoteFile {
            file_path: path.clone(),
        })
    }

    fn download(&self, file: &RemoteFile, max_bytes: u64, out: &mut dyn Write) -> io::Result<u64> {
        let (_, data) = self
            .0
            .values()
            .find(|(p, _)| *p == file.file_path)
            .expect("located file");
        if data.len() as u64 > max_bytes {
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, "fake"));
        }
        out.write_all(data)?;
        Ok(data.len() as u64)
    }
}

enum Stt {
    Says(&'static str),
    Fails,
    Absent,
}

struct FakeStt(Option<&'static str>);

impl Transcriber for FakeStt {
    fn transcribe(&self, path: &Path) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        assert!(path.exists());
        self.0.map(str::to_owned).ok_or_else(|| "stt down".into())
    }
}

struct FakeTexts;

impl MessageTexts for FakeTexts {
    fn text_of(&self, _chat_id: i64, message_id: i64) -> Option<(String, Author)> {
        (message_id == 10).then(|| ("Привет,\n как дела?".to_owned(), Author::Owner))
    }
}

struct Inbox(PathBuf);

impl Inbox {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ulei-ingress-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        Self(dir.join("inbox"))
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

struct Setup {
    stt: Stt,
    zone: Zone,
    max_file_bytes: u64,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            stt: Stt::Says("привет из голоса"),
            zone: Zone::utc(),
            max_file_bytes: 2 * 1024 * 1024,
        }
    }
}

struct H {
    ingress: Ingress,
    t0: Instant,
    inbox: Inbox,
}

impl H {
    fn new() -> Self {
        Self::with(Setup::default())
    }

    fn with(setup: Setup) -> Self {
        let inbox = Inbox::new();
        let config = IngressConfig {
            owner_ids: vec![42],
            bot_username: Some("test_bot".to_owned()),
            inbox_dir: inbox.0.clone(),
            zone: setup.zone,
            max_file_bytes: setup.max_file_bytes,
            max_media_bytes: 128 * 1024 * 1024,
            timing: Timing::default(),
            reactions: ReactionConfig::default(),
        };
        let ports = Ports {
            files: Box::new(FakeFiles::standard()),
            stt: match setup.stt {
                Stt::Says(t) => Some(Box::new(FakeStt(Some(t))) as Box<dyn Transcriber>),
                Stt::Fails => Some(Box::new(FakeStt(None)) as Box<dyn Transcriber>),
                Stt::Absent => None,
            },
            texts: Some(Box::new(FakeTexts)),
        };
        Self {
            ingress: Ingress::new(config, ports),
            t0: Instant::now(),
            inbox,
        }
    }

    fn at(&self, offset: Duration) -> Instant {
        self.t0 + offset
    }

    fn send(&mut self, update: &Update, offset: Duration) -> Vec<Output> {
        let now = self.at(offset);
        self.ingress.accept(update, now)
    }

    fn poll(&mut self, offset: Duration) -> Vec<Output> {
        let now = self.at(offset);
        self.ingress.poll(now)
    }

    /// Runs every timer until idle and returns all turns.
    fn drain(&mut self) -> Vec<Turn> {
        let mut turns = Vec::new();
        while let Some(at) = self.ingress.next_deadline() {
            for out in self.ingress.poll(at) {
                match out {
                    Output::Turn(t) => turns.push(t),
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
        turns
    }

    fn only_turn(&mut self, update: &Update) -> Turn {
        empty(&self.send(update, ms(0)));
        let mut turns = self.drain();
        assert_eq!(turns.len(), 1, "{turns:?}");
        turns.remove(0)
    }
}

fn text_update(update_id: i64, message_id: i64, text: &str) -> Update {
    let mut message = json!({
        "message_id": message_id, "date": 1_790_000_000,
        "chat": {"id": 1001, "type": "private"}, "from": {"id": 42, "first_name": "Owner"},
        "text": text,
    });
    if text.starts_with('/') {
        let len = text
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .encode_utf16()
            .count();
        message["entities"] = json!([{"type": "bot_command", "offset": 0, "length": len}]);
    }
    Update::from_value(json!({"update_id": update_id, "message": message})).expect("update")
}

fn with_message(base: &str, update_id: i64, patch: Value) -> Update {
    let mut raw = fixture_json(base);
    raw["update_id"] = json!(update_id);
    if let (Some(msg), Value::Object(patch)) = (raw["message"].as_object_mut(), patch) {
        msg.extend(patch);
    }
    Update::from_value(raw).expect("update")
}

fn fixture_json(name: &str) -> Value {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/ingress/{name}.json"));
    serde_json::from_slice(&std::fs::read(path).expect("fixture")).expect("json")
}

/// A zone with only a POSIX rule: UTC+2, UTC+3 from the last Sunday of March to the last of October.
fn eastern_european_zone() -> Zone {
    Zone::from_tzif(&eastern_european_tzif()).expect("tzif")
}

/// The `TZif` bytes of [`eastern_european_zone`].
fn eastern_european_tzif() -> Vec<u8> {
    let mut block = Vec::new();
    let counts = [0u32, 0, 0, 0, 1, 4];
    let header = |out: &mut Vec<u8>, version: u8| {
        out.extend_from_slice(b"TZif");
        out.push(version);
        out.extend_from_slice(&[0; 15]);
        for c in counts {
            out.extend_from_slice(&c.to_be_bytes());
        }
    };
    let data = |out: &mut Vec<u8>| {
        out.extend_from_slice(&7200i32.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(b"EET\0");
    };
    header(&mut block, b'2');
    data(&mut block);
    header(&mut block, b'2');
    data(&mut block);
    block.extend_from_slice(b"\nEET-2EEST,M3.5.0/3,M10.5.0/4\n");
    block
}

// --- Update kinds -> text the engine sees -------------------------------------------------

#[test]
fn golden_texts_of_text_like_updates() {
    let cases = [
        ("text", "Привет, как дела?"),
        (
            "custom_emoji",
            "ok <tg-emoji emoji-id=\"5368324170671202286\">😀</tg-emoji> done",
        ),
        ("rich_message", "План\n- купить хлеб\n[x] позвонить\na | b"),
        (
            "forward_user",
            "[Переслано от Test Person (@test_person), 21.09.2026 14:13]\nчужой текст",
        ),
        (
            "forward_channel",
            "[Переслано от Test Channel (@test_channel), автор Editor, 21.09.2026 14:13]\nпост",
        ),
        (
            "forward_hidden",
            "[Переслано от Hidden Sender, 21.09.2026 14:13]\nскрытый",
        ),
        (
            "forward_legacy",
            "[Переслано от Legacy, 21.09.2026 14:13]\nстарый",
        ),
        ("reply_quote", "[Replying to: \"вторая строка\"]\n\nда, это"),
        (
            "reply_text",
            "[Replying to: \"исходное сообщение\"]\n\nответ",
        ),
        ("edited_text", "исправленный текст"),
        (
            "sticker",
            "[The user sent a sticker 👍 from \"TestPack\"~ It shows: \"a sticker with emoji 👍\" (=^.w.^=)]",
        ),
        (
            "sticker_animated",
            "[The user sent an animated sticker 🔥~ I can't see animated ones yet, but the emoji suggests: 🔥]",
        ),
        (
            "location",
            "[The user shared a location pin.]\nlatitude: 35.0\nlongitude: 33.25\n\
             Map: https://www.google.com/maps/search/?api=1&query=35.0,33.25\n\
             Ask what they'd like to find nearby (restaurants, cafes, etc.) and any preferences.",
        ),
        (
            "venue",
            "[The user shared a location pin.]\nVenue: Test Cafe\nAddress: 1 Test Street\nlatitude: 35.5\n\
             longitude: 33.5\nMap: https://www.google.com/maps/search/?api=1&query=35.5,33.5\n\
             Ask what they'd like to find nearby (restaurants, cafes, etc.) and any preferences.",
        ),
        (
            "unknown_field",
            "[Telegram: пришло сообщение неподдерживаемого формата (поля: future_content); текст извлечь \
             не удалось — попроси владельца переслать его обычным текстом]",
        ),
    ];
    for (name, expected) in cases {
        let turn = H::new().only_turn(&fixture(name));
        assert_eq!(turn.text, expected, "{name}");
        assert_eq!(
            (turn.chat_id, turn.internal, turn.context.as_deref()),
            (1001, false, None),
            "{name}"
        );
    }
}

/// Inline code reaches the engine whole: as a Telegram `code` entity, as literal backticks and
/// as a code node of a rich message, the text after the code is kept.
#[test]
fn text_around_inline_code_is_kept() {
    let plain =
        "Use the terminal tool to run sleep 120; echo late, then reply with its output only.";
    let ticks =
        "Use the terminal tool to run `sleep 120; echo late`, then reply with its output only.";
    let message = |fields: Value| {
        let mut message = json!({
            "message_id": 1, "date": 1_790_000_000,
            "chat": {"id": 1001, "type": "private"}, "from": {"id": 42, "first_name": "Owner"},
        });
        message
            .as_object_mut()
            .expect("message")
            .extend(fields.as_object().expect("fields").clone());
        Update::from_value(json!({"update_id": 1, "message": message})).expect("update")
    };
    let entity = message(json!({
        "text": plain,
        "entities": [{"type": "code", "offset": 29, "length": 20}],
    }));
    let rich = message(
        json!({"rich_message": {"blocks": [{"type": "paragraph", "text": [
            "Use the terminal tool to run ",
            {"type": "code", "text": "sleep 120; echo late"},
            ", then reply with its output only.",
        ]}]}}),
    );
    assert_eq!(H::new().only_turn(&entity).text, plain);
    assert_eq!(H::new().only_turn(&text_update(1, 1, ticks)).text, ticks);
    assert_eq!(H::new().only_turn(&rich).text, plain);
}

#[test]
fn reply_and_ids_are_kept_for_answers() {
    let turn = H::new().only_turn(&fixture("reply_quote"));
    assert_eq!(turn.message_id, Some(17));
    assert_eq!(turn.update_id, 8);
    assert_eq!(turn.date, 1_790_000_000);
    let reply = turn.reply.expect("reply");
    assert_eq!(
        (reply.message_id, reply.text.as_deref()),
        (9, Some("вторая строка"))
    );
}

#[test]
fn forward_dates_follow_the_configured_zone() {
    let mut h = H::with(Setup {
        zone: eastern_european_zone(),
        ..Setup::default()
    });
    let turn = h.only_turn(&fixture("forward_user"));
    assert_eq!(
        turn.text,
        "[Переслано от Test Person (@test_person), 21.09.2026 17:13]\nчужой текст"
    );
    let winter = with_message(
        "forward_user",
        2,
        json!({"forward_origin": {"type": "user", "date": 1_800_000_000, "sender_user": {"id": 77, "first_name": "Test"}}}),
    );
    let turn = H::with(Setup {
        zone: eastern_european_zone(),
        ..Setup::default()
    })
    .only_turn(&winter);
    assert_eq!(
        turn.text,
        "[Переслано от Test, 15.01.2027 10:00]\nчужой текст"
    );
}

#[test]
fn local_time_is_exact_at_both_ends_of_the_unix_time_range() {
    let zone = eastern_european_zone();
    let latest = zone.local(i64::MAX);
    assert_eq!(
        (latest.dmy_hm(), latest.second, latest.offset),
        ("04.12.292277026596 17:30".to_owned(), 7, 7200)
    );
    let earliest = zone.local(i64::MIN);
    assert_eq!(
        (earliest.dmy_hm(), earliest.second, earliest.offset),
        ("27.01.-292277022657 10:29".to_owned(), 52, 7200)
    );
}

#[test]
fn a_named_zone_is_read_from_the_first_directory_that_holds_it() {
    let inbox = Inbox::new();
    let root = inbox.0.parent().expect("scratch").to_path_buf();
    let (empty_dir, second) = (root.join("first"), root.join("second"));
    std::fs::create_dir_all(second.join("Test")).expect("dirs");
    std::fs::write(second.join("Test/East"), eastern_european_tzif()).expect("zone file");
    let dirs = [empty_dir.as_path(), second.as_path()];
    assert_eq!(
        Zone::named("Test/East", &dirs).expect("zone"),
        eastern_european_zone()
    );
    assert!(matches!(
        Zone::named("Test/West", &dirs),
        Err(ZoneError::NotFound)
    ));
}

#[test]
fn messages_no_handler_reads_are_counted_not_delivered() {
    for name in ["contact", "video_note"] {
        let mut h = H::new();
        empty(&h.send(&fixture(name), ms(0)));
        assert_eq!(h.ingress.next_deadline(), None, "{name}");
        assert_eq!(h.ingress.stats().unhandled, 1, "{name}");
    }
    // A button press is a `Callback`; the ingress makes nothing of it (the guest gate's buttons
    // went to the gate before, ADR-0083) and counts it skipped.
    let mut h = H::new();
    let button = fixture("callback_query");
    assert!(matches!(button.kind(), Some(UpdateKind::Callback(_))));
    empty(&h.send(&button, ms(0)));
    assert_eq!(h.ingress.stats().skipped_updates, 1);
    let mut h = H::new();
    empty(&h.send(&fixture("callback_query"), ms(0)));
    empty(&h.send(&fixture("callback_query"), ms(10)));
    assert_eq!(h.ingress.stats().skipped_updates, 2);
    assert_eq!(h.ingress.stats().strangers, 0);
}

#[test]
fn an_edited_text_becomes_a_new_turn() {
    let mut h = H::new();
    let first = h.only_turn(&text_update(22, 32, "исходный текст"));
    assert_eq!(
        (first.text.as_str(), first.message_id),
        ("исходный текст", Some(32))
    );
    let edited = h.only_turn(&fixture("edited_text"));
    assert_eq!(
        (edited.text.as_str(), edited.message_id, edited.update_id),
        ("исправленный текст", Some(32), 23)
    );
}

#[test]
fn an_edited_photo_is_dropped_without_a_download() {
    let mut raw = fixture_json("photo");
    let mut message = raw["message"].take();
    message["edit_date"] = json!(1_790_000_100);
    raw = json!({"update_id": raw["update_id"].clone(), "edited_message": message});
    let mut h = H::new();
    empty(&h.send(&Update::from_value(raw).expect("update"), ms(0)));
    assert_eq!(h.ingress.next_deadline(), None);
    assert_eq!(h.ingress.stats().unhandled, 1);
    assert!(!h.inbox.0.exists());
}

// --- Allowlist ------------------------------------------------------------------------------

#[test]
fn only_the_owner_reaches_the_engine() {
    let mut h = H::new();
    empty(&h.send(&fixture("stranger"), ms(0)));
    empty(&h.send(&fixture("group"), ms(0)));
    let mut reaction = fixture_json("reaction");
    reaction["message_reaction"]["user"]["id"] = json!(4242);
    empty(&h.send(&Update::from_value(reaction).expect("update"), ms(0)));
    assert_eq!(h.ingress.next_deadline(), None);
    let stats = h.ingress.stats();
    assert_eq!((stats.strangers, stats.other_chats), (2, 1));
    empty(&h.drain());
    assert_eq!(h.only_turn(&fixture("text")).text, "Привет, как дела?");
}

// --- Coalescing -----------------------------------------------------------------------------

#[test]
fn short_text_waits_for_the_quiet_window() {
    let mut h = H::new();
    empty(&h.send(&text_update(1, 1, "один"), ms(0)));
    assert_eq!(h.ingress.next_deadline(), Some(h.at(ms(180))));
    empty(&h.poll(ms(180)));
    assert_eq!(h.ingress.next_deadline(), Some(h.at(ms(2180))));
    empty(&h.poll(ms(2179)));
    let out = h.poll(ms(2180));
    assert!(matches!(&out[..], [Output::Turn(t)] if t.text == "один"));
    assert_eq!(h.ingress.next_deadline(), None);
}

#[test]
fn messages_within_two_seconds_become_one_turn() {
    let mut h = H::new();
    h.send(&text_update(1, 1, "первое"), ms(0));
    h.poll(ms(180));
    h.send(&text_update(2, 2, "второе"), ms(1500));
    h.poll(ms(1680));
    assert_eq!(h.ingress.next_deadline(), Some(h.at(ms(3680))));
    let turns = h.drain();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].text, "первое\n\nвторое");
    assert_eq!(turns[0].message_id, Some(2));
}

#[test]
fn a_coalesced_turn_is_capped_at_seven_seconds() {
    let mut h = H::new();
    let mut turns = Vec::new();
    for i in 0..6 {
        let offset = ms(1500 * i);
        h.send(
            &text_update(
                i64::try_from(i).expect("i"),
                i64::try_from(i).expect("i"),
                &format!("m{i}"),
            ),
            offset,
        );
        for out in h.poll(offset + ms(180)) {
            if let Output::Turn(t) = out {
                turns.push(t.text);
            }
        }
    }
    turns.extend(h.drain().into_iter().map(|t| t.text));
    assert_eq!(turns, ["m0\n\nm1\n\nm2\n\nm3\n\nm4", "m5"]);
}

#[test]
fn split_long_message_is_joined_by_the_text_batcher() {
    let mut h = H::new();
    let long = "я".repeat(4000);
    h.send(&text_update(1, 1, &long), ms(0));
    assert_eq!(h.ingress.next_deadline(), Some(h.at(ms(1000))));
    h.send(&text_update(2, 2, "хвост"), ms(600));
    let turns = h.drain();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].text, format!("{long}\nхвост"));
}

#[test]
fn forward_and_own_question_are_told_apart() {
    let mut h = H::new();
    h.send(&fixture("forward_user"), ms(0));
    h.send(&text_update(2, 50, "что скажешь?"), ms(500));
    let turns = h.drain();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        turns[0].text,
        "[Переслано от Test Person (@test_person), 21.09.2026 14:13]\nчужой текст\n\n\
         [Сообщение владельца]\nчто скажешь?"
    );
}

#[test]
fn a_command_sends_the_pending_turn_first_and_is_not_coalesced() {
    let mut h = H::new();
    h.send(&text_update(1, 1, "сделай отчёт"), ms(0));
    h.poll(ms(180));
    let out = h.send(&text_update(2, 2, "/stop"), ms(500));
    assert!(
        matches!(&out[..], [Output::Turn(t), Output::Command { command: Command::Stop, message_id: 2, .. }] if t.text == "сделай отчёт")
    );
    assert_eq!(h.ingress.next_deadline(), None);
}

#[test]
fn an_album_becomes_one_turn() {
    let mut h = H::new();
    let first = with_message(
        "photo",
        1,
        json!({"media_group_id": "album-1", "caption": "наш отпуск"}),
    );
    let second = with_message(
        "photo",
        2,
        json!({"message_id": 40, "media_group_id": "album-1", "caption": null,
               "photo": [{"file_id": "photo-2", "file_unique_id": "u", "width": 1, "height": 1, "file_size": 10}]}),
    );
    h.send(&first, ms(0));
    h.send(&second, ms(100));
    assert_eq!(h.ingress.next_deadline(), Some(h.at(ms(900))));
    let turns = h.drain();
    assert_eq!(turns.len(), 1);
    let turn = &turns[0];
    let hints = format!(
        "{}\n{}",
        image_hint(&turn.attachments[0].path),
        image_hint(&turn.attachments[1].path)
    );
    assert_eq!(
        (turn.kind, turn.text.clone()),
        (MessageKind::Photo, format!("наш отпуск\n\n{hints}"))
    );
    let paths: Vec<String> = turn
        .attachments
        .iter()
        .map(|a| a.path.display().to_string())
        .collect();
    assert_eq!(turn.image_paths(), paths);
    let mimes: Vec<&str> = turn.attachments.iter().map(|a| a.mime.as_str()).collect();
    assert_eq!(mimes, ["image/png", "image/jpg"]);
}

#[test]
fn an_audio_file_never_shares_a_turn_with_other_media() {
    let mut h = H::new();
    h.send(&fixture("audio"), ms(0));
    h.send(&fixture("photo"), ms(100));
    let kinds: Vec<(MessageKind, usize)> = h
        .drain()
        .iter()
        .map(|t| (t.kind, t.attachments.len()))
        .collect();
    assert_eq!(kinds, [(MessageKind::Audio, 1), (MessageKind::Photo, 1)]);

    let mut h = H::new();
    h.send(&fixture("audio"), ms(0));
    h.send(&text_update(30, 30, "послушай"), ms(100));
    let turns = h.drain();
    assert_eq!(turns.len(), 1, "{turns:?}");
    assert_eq!(turns[0].kind, MessageKind::Audio);

    let mut h = H::new();
    h.send(&fixture("video"), ms(0));
    h.send(&fixture("photo"), ms(100));
    let turns = h.drain();
    assert_eq!(turns.len(), 1, "{turns:?}");
    assert_eq!(turns[0].attachments.len(), 2);
}

// --- Attachments ----------------------------------------------------------------------------

#[test]
fn photo_is_saved_privately_in_the_inbox() {
    let mut h = H::new();
    let turn = h.only_turn(&fixture("photo"));
    let [photo] = &turn.attachments[..] else {
        panic!("one attachment")
    };
    assert_eq!(
        (turn.kind, turn.text.clone()),
        (
            MessageKind::Photo,
            format!("смотри\n\n{}", image_hint(&photo.path))
        )
    );
    assert_eq!(photo.path.parent(), Some(h.inbox.0.as_path()));
    assert!(
        photo
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("img_"))
    );
    let meta = std::fs::metadata(&photo.path).expect("saved");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(&photo.path).expect("read"), PNG);
    let leftovers = std::fs::read_dir(&h.inbox.0).expect("inbox").count();
    assert_eq!(leftovers, 1, "no part files left behind");
}

#[test]
fn audio_video_and_documents_get_the_reference_notes() {
    let audio = H::new().only_turn(&fixture("audio"));
    let path = &audio.attachments[0].path;
    let name = path.file_name().and_then(|n| n.to_str()).expect("name");
    assert!(name.starts_with("audio_"));
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mp3"));
    assert_eq!(audio.kind, MessageKind::Audio);
    assert_eq!(
        audio.text,
        format!(
            "[The user sent an audio file attachment: '{name}'. It is saved at: {}. Its content is not inlined \
             here. If the user's request involves what the audio contains, transcribe or process it yourself — \
             for example by passing the path to a transcription or media tool — instead of asking the user to \
             describe it. Only ask what to do with it if their intent is genuinely unclear.]\n\n",
            path.display()
        )
    );

    let video = H::new().only_turn(&fixture("video"));
    let path = &video.attachments[0].path;
    let name = path.file_name().and_then(|n| n.to_str()).expect("name");
    assert!(video.text.starts_with(&format!(
        "[The user sent a video attachment: '{name}'. It is saved at: {}.",
        path.display()
    )));
    assert!(
        video
            .text
            .ends_with("their intent is genuinely unclear.]\n\nвидео")
    );

    let pdf = H::new().only_turn(&fixture("document_pdf"));
    let a = &pdf.attachments[0];
    assert_eq!(
        (a.mime.as_str(), pdf.kind),
        ("application/pdf", MessageKind::Document)
    );
    assert!(
        a.path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("doc_") && n.ends_with("_report.pdf"))
    );
    assert_eq!(
        pdf.text,
        format!(
            "[The user sent a document: 'report.pdf'. It is saved at: {}. Its text is not inlined here (it's a \
             binary format such as PDF or DOCX). To read it, extract the document's text yourself — for example \
             with the terminal tool or the ocr-and-documents skill — before answering, instead of asking the \
             user to paste the contents.]\n\nпрочитай",
            a.path.display()
        )
    );

    let txt = H::new().only_turn(&fixture("document_text"));
    assert_eq!(
        txt.text,
        format!(
            "[The user sent a text document: 'notes.txt'. Its content has been included below. The file is also \
             saved at: {}]\n\n[Content of notes.txt]:\nhello world",
            txt.attachments[0].path.display()
        )
    );
}

#[test]
fn oversized_files_become_notes_not_downloads() {
    let mut h = H::with(Setup {
        max_file_bytes: 2 * 1024 * 1024,
        ..Setup::default()
    });
    let voice = with_message(
        "voice",
        1,
        json!({"voice": {"file_id": "voice-1", "file_size": 3 * 1024 * 1024 + 100_000}}),
    );
    let turn = h.only_turn(&voice);
    assert_eq!(
        turn.text,
        "[Telegram voice message skipped: file size 3.1 MB exceeds the 2 MB limit. Ask the user to send a smaller file.]"
    );
    empty(&turn.attachments);
    let doc = with_message(
        "document_pdf",
        2,
        json!({"document": {"file_id": "doc-pdf", "file_name": "big.pdf", "file_size": 5 * 1024 * 1024}}),
    );
    let turn = H::with(Setup {
        max_file_bytes: 2 * 1024 * 1024,
        ..Setup::default()
    })
    .only_turn(&doc);
    assert_eq!(
        turn.text,
        "The document is too large or its size could not be verified. Maximum: 2 MB."
    );
    assert!(std::fs::read_dir(&h.inbox.0).map_or(true, |d| d.count() == 0));
}

#[test]
fn a_failed_download_is_surfaced_to_owner_and_agent() {
    let mut h = H::new();
    let broken = with_message(
        "photo",
        1,
        json!({"photo": [{"file_id": "missing", "file_unique_id": "x", "width": 1, "height": 1}]}),
    );
    let out = h.send(&broken, ms(0));
    assert_eq!(
        out,
        [Output::Notice {
            chat_id: 1001,
            reply_to: 19,
            text: "⚠️ Couldn't download your photo (TimedOut). Please try sending it again."
                .to_owned()
        }]
    );
    let turns = h.drain();
    assert_eq!(
        turns[0].text,
        "смотри\n\n[The user attempted to send a photo but it could not be downloaded (TimedOut); they have been asked to retry.]"
    );
    let not_image = with_message(
        "photo",
        2,
        json!({"photo": [{"file_id": "not-image", "file_unique_id": "y", "width": 1, "height": 1}]}),
    );
    let mut h = H::new();
    assert!(
        matches!(&h.send(&not_image, ms(0))[..], [Output::Notice { text, .. }] if text.contains("(ValueError)"))
    );
    assert_eq!(std::fs::read_dir(&h.inbox.0).expect("inbox").count(), 0);
}

#[test]
fn a_document_whose_saved_name_the_filesystem_refuses_leaves_no_file_behind() {
    let name = format!("{}.pdf", "x".repeat(260));
    let doc = with_message(
        "document_pdf",
        1,
        json!({"document": {"file_id": "doc-pdf", "file_name": name, "file_size": 8, "mime_type": "application/pdf"}}),
    );
    let mut h = H::new();
    let out = h.send(&doc, ms(0));
    assert!(
        matches!(&out[..], [Output::Notice { text, .. }] if text.starts_with("⚠️ Couldn't download your attachment")),
        "{out:?}"
    );
    let turns = h.drain();
    assert!(
        turns[0]
            .text
            .contains("[The user attempted to send a attachment"),
        "{}",
        turns[0].text
    );
    empty(&turns[0].attachments);
    assert_eq!(std::fs::read_dir(&h.inbox.0).expect("inbox").count(), 0);
}

#[test]
fn replied_to_media_is_saved_and_noted() {
    let mut raw = fixture_json("text");
    raw["message"] = json!({
        "message_id": 60, "date": 1_790_000_000, "chat": {"id": 1001, "type": "private"},
        "from": {"id": 42, "first_name": "Owner"}, "text": "что на фото?",
        "reply_to_message": fixture_json("photo")["message"].clone(),
    });
    let turn = H::new().only_turn(&Update::from_value(raw).expect("update"));
    let path = &turn.attachments[0].path;
    assert_eq!(turn.kind, MessageKind::Photo);
    assert_eq!(
        turn.text,
        format!(
            "[Replying to: \"смотри\"]\n\nчто на фото?\n\n[Replied-to image 'file_1.png' saved at: {}]\n\n{}",
            path.display(),
            image_hint(path)
        )
    );
}

// --- Voice ----------------------------------------------------------------------------------

#[test]
fn voice_is_transcribed_or_kept_as_file_with_note() {
    let turn = H::new().only_turn(&fixture("voice"));
    assert_eq!(
        (turn.kind, turn.text.as_str()),
        (MessageKind::Voice, "\"привет из голоса\"")
    );
    assert_eq!(
        turn.attachments[0]
            .path
            .extension()
            .and_then(|e| e.to_str()),
        Some("ogg")
    );

    let turn = H::with(Setup {
        stt: Stt::Fails,
        ..Setup::default()
    })
    .only_turn(&fixture("voice"));
    assert_eq!(
        turn.text,
        format!(
            "[voice message could not be transcribed automatically; the audio is available at: {}]",
            turn.attachments[0].path.display()
        )
    );

    let turn = H::with(Setup {
        stt: Stt::Absent,
        ..Setup::default()
    })
    .only_turn(&fixture("voice"));
    assert_eq!(
        turn.text,
        format!(
            "[The user sent a voice message: {} (duration: 0:07)]",
            turn.attachments[0].path.display()
        )
    );

    let turn = H::with(Setup {
        stt: Stt::Says("  "),
        ..Setup::default()
    })
    .only_turn(&fixture("voice"));
    assert!(
        turn.text
            .starts_with("[The user sent a voice message but it came through empty or inaudible")
    );
}

#[test]
fn forwarded_voice_keeps_its_label_above_the_transcript_and_text_joins() {
    let mut h = H::new();
    let forwarded = with_message(
        "voice",
        1,
        fixture_json("forward_user")["message"]
            .as_object()
            .map(|m| json!({"forward_origin": m["forward_origin"].clone()}))
            .expect("origin"),
    );
    h.send(&forwarded, ms(0));
    h.send(&text_update(2, 70, "перескажи"), ms(400));
    let turns = h.drain();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].kind, MessageKind::Voice);
    assert_eq!(
        turns[0].text,
        "[Переслано от Test Person (@test_person), 21.09.2026 14:13]\n\"привет из голоса\"\n\n\
         [Сообщение владельца]\nперескажи"
    );
}

// --- Reactions ------------------------------------------------------------------------------

#[test]
fn an_owner_reaction_wakes_the_agent_after_fifteen_seconds() {
    let mut h = H::new();
    empty(&h.send(&fixture("reaction"), ms(0)));
    assert_eq!(
        h.ingress.next_deadline(),
        Some(h.at(Duration::from_secs(15)))
    );
    let out = h.poll(Duration::from_secs(15));
    let [Output::Turn(turn)] = &out[..] else {
        panic!("{out:?}")
    };
    assert!(turn.internal);
    assert_eq!(turn.text, ReactionConfig::default().wake_text);
    assert_eq!(
        turn.context.as_deref(),
        Some(
            "Реакции владельца в Telegram с прошлого хода (текстом не писали, только поставили реакцию):\n\
             - 2026-09-21T14:13 👍 премиум-эмодзи (id 5368324170671202286, картинку не вижу) на его же сообщение \
             #10 «Привет, как дела?»"
        )
    );
    assert_eq!(h.ingress.next_deadline(), None);
}

#[test]
fn the_reaction_wake_up_turn_has_no_message_to_reply_to() {
    let mut h = H::new();
    empty(&h.send(&fixture("reaction"), ms(0)));
    let out = h.poll(Duration::from_secs(15));
    let [Output::Turn(turn)] = &out[..] else {
        panic!("{out:?}")
    };
    assert_eq!((turn.chat_id, turn.message_id), (1001, None));
}

#[test]
fn an_owner_reaction_outside_the_private_chat_is_dropped() {
    let mut h = H::new();
    let mut group = fixture_json("reaction");
    group["message_reaction"]["chat"] = json!({"id": -1002, "type": "supergroup"});
    empty(&h.send(&Update::from_value(group).expect("update"), ms(0)));
    assert_eq!(h.ingress.next_deadline(), None, "no wake-up into a group");
    assert_eq!(h.ingress.stats().other_chats, 1);
    empty(&h.poll(Duration::from_secs(15)));
}

#[test]
fn a_reaction_seen_by_an_owner_turn_does_not_wake_again() {
    let mut h = H::new();
    let mut removal = fixture_json("reaction");
    removal["message_reaction"]["new_reaction"] = json!([]);
    removal["message_reaction"]["message_id"] = json!(5);
    h.send(&Update::from_value(removal).expect("update"), ms(0));
    assert_eq!(
        h.ingress.next_deadline(),
        None,
        "removing a reaction does not wake"
    );
    h.send(&fixture("reaction"), ms(100));
    h.send(&text_update(3, 80, "и ещё"), ms(200));
    let first = h.poll(ms(2380));
    let [Output::Turn(turn)] = &first[..] else {
        panic!("{first:?}")
    };
    let context = turn.context.as_deref().expect("context");
    assert!(
        context
            .contains("- 2026-09-21T14:13 (снял реакцию) на сообщение #5 (текст не найден, id 5)")
    );
    empty(&h.drain());
}

// --- Commands -------------------------------------------------------------------------------

#[derive(Default)]
struct FakeEngine {
    calls: Rc<RefCell<Vec<String>>>,
    running: bool,
}

impl Engine for FakeEngine {
    fn submit_turn(&mut self, turn: &Turn) -> Result<String, EngineError> {
        self.calls
            .borrow_mut()
            .push(format!("submit_turn {}", turn.text));
        Ok("t-1".to_owned())
    }

    fn interrupt(&mut self) -> Result<Interrupted, EngineError> {
        self.calls.borrow_mut().push("interrupt".to_owned());
        Ok(if std::mem::take(&mut self.running) {
            Interrupted::Running
        } else {
            Interrupted::Nothing
        })
    }

    fn status(&mut self) -> Result<EngineStatus, EngineError> {
        self.calls.borrow_mut().push("status".to_owned());
        Ok(EngineStatus {
            session_id: "s-1".to_owned(),
            title: None,
            created: Some(1_790_000_000),
            last_activity: 1_790_003_600,
            model: Some("main".to_owned()),
            context_used: 12_345,
            context_total: 200_000,
            tokens_billed: Some(1_234_567),
            running: self.running,
            queued: 2,
        })
    }

    fn set_route(&mut self, alias: &str) -> Result<RouteSwitch, EngineError> {
        self.calls.borrow_mut().push(format!("set_route {alias}"));
        if alias == "fast" {
            Ok(RouteSwitch {
                alias: alias.to_owned(),
                context_total: Some(128_000),
            })
        } else {
            Err(EngineError {
                message: format!("unknown alias {alias}"),
            })
        }
    }

    fn new_session(&mut self) -> Result<NewSession, EngineError> {
        self.calls.borrow_mut().push("new_session".to_owned());
        Ok(NewSession {
            reset_existing: true,
            model: Some("main".to_owned()),
            context_total: Some(200_000),
        })
    }
}

fn command_of(text: &str) -> Command {
    let mut h = H::new();
    match h.send(&text_update(1, 1, text), ms(0)).as_slice() {
        [
            Output::Command {
                command,
                chat_id: 1001,
                ..
            },
        ] => command.clone(),
        other => panic!("{text}: {other:?}"),
    }
}

#[test]
fn commands_parse_with_or_without_the_bot_suffix() {
    assert_eq!(command_of("/new"), Command::New);
    assert_eq!(command_of("/reset@test_bot"), Command::New);
    assert_eq!(command_of("/STOP@test_bot"), Command::Stop);
    assert_eq!(command_of("/status"), Command::Status);
    assert_eq!(
        command_of("/model"),
        Command::Model {
            alias: None,
            global: false
        }
    );
    assert_eq!(
        command_of("/model@test_bot fast"),
        Command::Model {
            alias: Some("fast".to_owned()),
            global: false
        }
    );
}

#[test]
fn model_flags_are_split_from_the_alias_as_the_reference_does() {
    let model = |alias: Option<&str>, global| Command::Model {
        alias: alias.map(str::to_owned),
        global,
    };
    for (text, expected) in [
        ("/model opus --global", model(Some("opus"), true)),
        ("/model --global opus", model(Some("opus"), true)),
        // The messenger turns `--` into an em or en dash.
        ("/model opus \u{2014}global", model(Some("opus"), true)),
        ("/model opus \u{2013}global", model(Some("opus"), true)),
        ("/model opus --session", model(Some("opus"), false)),
        ("/model opus --provider any", model(Some("opus"), false)),
        ("/model --global", model(None, true)),
        (
            "/model opus --globally",
            model(Some("opus --globally"), false),
        ),
    ] {
        assert_eq!(command_of(text), expected, "{text}");
    }
}

#[test]
fn other_slash_commands_go_to_the_engine_as_a_turn() {
    let mut h = H::new();
    let out = h.send(&text_update(1, 1, "/digest today"), ms(0));
    assert!(
        matches!(&out[..], [Output::Turn(t)] if t.text == "/digest today" && t.kind == MessageKind::Command)
    );
}

#[test]
fn a_slash_command_for_the_engine_loses_only_the_bot_suffix_of_its_name() {
    for (text, expected) in [
        ("/digest@test_bot today", "/digest today"),
        ("/digest@test_bot", "/digest"),
        ("/digest @test_bot today", "/digest today"),
    ] {
        let mut h = H::new();
        let out = h.send(&text_update(1, 1, text), ms(0));
        let [Output::Turn(turn)] = &out[..] else {
            panic!("{text}: {out:?}")
        };
        assert_eq!(turn.text, expected, "{text}");
    }
}

#[test]
fn command_replies_follow_the_reference() {
    let zone = Zone::utc();
    let mut engine = FakeEngine {
        running: true,
        ..FakeEngine::default()
    };
    let reply = |e: &mut FakeEngine, c: Command| run_command(e, &c, &zone).expect("reply");
    assert_eq!(
        reply(&mut engine, Command::New),
        "✨ Сеанс сброшен! Начинаем с чистого листа.\n\n◆ Model: `main`\n◆ Context: 200K tokens"
    );
    assert_eq!(
        reply(&mut engine, Command::Status),
        "📊 **Состояние канала**\n\n**ID сеанса:** `s-1`\n**Создано:** 2026-09-21 14:13\n\
         **Последняя активность:** 2026-09-21 15:13\n**Model:** `main`\n**Context:** 12,345 / 200,000 (6%)\n\
         **Lifetime tokens billed:** 1,234,567 _(not your current context size; use `/context`)_\n\
         **Агент активен:** Да ⚡\n**Очередь продолжений:** 2\n\n**Подключённые платформы:** telegram"
    );
    assert_eq!(
        reply(&mut engine, Command::Stop),
        "⚡ Остановлено. Вы можете продолжить этот сеанс."
    );
    assert_eq!(
        reply(&mut engine, Command::Stop),
        "Нет активной задачи для остановки."
    );
    assert_eq!(
        reply(
            &mut engine,
            Command::Model {
                alias: Some("fast".to_owned()),
                global: false
            }
        ),
        "Модель изменена на `fast`\nКонтекст: 128,000 токенов\n\
         _(только для этого сеанса — добавьте `--global`, чтобы сохранить)_"
    );
    assert_eq!(
        reply(
            &mut engine,
            Command::Model {
                alias: Some("fast".to_owned()),
                global: true
            }
        ),
        "Модель изменена на `fast`\nКонтекст: 128,000 токенов\n\
         _(`--global` пока не поддерживается: модель действует до `/new`, \
         затем снова маршрут по умолчанию)_"
    );
    assert_eq!(
        reply(
            &mut engine,
            Command::Model {
                alias: Some("bad".to_owned()),
                global: false
            }
        ),
        "Ошибка: unknown alias bad"
    );
    assert_eq!(
        reply(
            &mut engine,
            Command::Model {
                alias: None,
                global: false
            }
        ),
        "Текущая: `main`\n\n`/model <name>` — сменить модель"
    );
    assert_eq!(
        *engine.calls.borrow(),
        [
            "new_session",
            "status",
            "interrupt",
            "interrupt",
            "set_route fast",
            "set_route fast",
            "set_route bad",
            "status"
        ]
    );
}

#[test]
fn turns_reach_the_engine_through_the_seam() {
    let mut h = H::new();
    let mut engine = FakeEngine::default();
    let calls = Rc::clone(&engine.calls);
    h.send(&text_update(1, 1, "запиши задачу"), ms(0));
    for turn in h.drain() {
        engine.submit_turn(&turn).expect("submit");
    }
    assert_eq!(*calls.borrow(), ["submit_turn запиши задачу"]);
}
