//! `extract_media` on synthetic replies: which `MEDIA:` tags name files to deliver, with the
//! directives, and the text left to show.

use ulei_channels::render::{MediaFile, MediaReply, extract_media};

/// A checker under which no path exists.
fn nothing(_: &str) -> Option<String> {
    None
}

fn file(path: &str) -> MediaFile {
    MediaFile {
        path: path.to_owned(),
        is_voice: false,
    }
}

fn reply(files: Vec<MediaFile>, text: &str) -> MediaReply {
    MediaReply {
        files,
        text: text.to_owned(),
        as_document: false,
    }
}

#[test]
fn tags_in_marks_quotes_glued_or_ending_a_sentence_are_found_and_removed() {
    let cases = [
        ("**MEDIA:/r/a.pdf**", vec![file("/r/a.pdf")], ""),
        (
            "Файл: MEDIA:\"/r/my file.pdf\" готов",
            vec![file("/r/my file.pdf")],
            "Файл:  готов",
        ),
        ("MEDIA:/r/map data.kmz", vec![file("/r/map data.kmz")], ""),
        (
            "MEDIA:/r/b.pngMEDIA:/r/c.png",
            vec![file("/r/b.png"), file("/r/c.png")],
            "",
        ),
        ("See MEDIA:/r/data.csv.", vec![file("/r/data.csv")], "See"),
        (
            "MEDIA:/r/x/archive.tar.gz",
            vec![file("/r/x/archive.tar.gz")],
            "",
        ),
        ("media: /r/low.PDF", vec![file("/r/low.PDF")], ""),
        (
            "MEDIA:/r/早报.pdf（782 KB）",
            vec![file("/r/早报.pdf")],
            "（782 KB）",
        ),
        (
            "A\n\n\nMEDIA:/r/a.pdf\n\n\nB",
            vec![file("/r/a.pdf")],
            "A\n\nB",
        ),
    ];
    for (content, files, text) in cases {
        assert_eq!(
            extract_media(content, None, &nothing),
            reply(files, text),
            "{content:?}"
        );
    }
}

#[test]
fn examples_in_code_quotes_and_stored_json_stay_in_the_text() {
    for content in [
        "```\nMEDIA:/r/f.pdf\n```",
        "> MEDIA:/r/q.pdf",
        r#"{"result": "MEDIA:/r/s.png"}"#,
        "Пример: `MEDIA:/r/t.pdf`",
        "MEDIA:/r/unknown.weird",
        "MEDIA:relative/x.pdf",
    ] {
        assert_eq!(
            extract_media(content, None, &nothing),
            reply(Vec::new(), content),
            "{content:?}"
        );
    }
}

#[test]
fn a_whole_tag_in_inline_code_is_delivered_when_the_file_exists() {
    let exists = |path: &str| (path == "/r/t.pdf").then(|| path.to_owned());
    assert_eq!(
        extract_media("Вот `MEDIA:/r/t.pdf`", None, &exists),
        reply(vec![file("/r/t.pdf")], "Вот")
    );
}

#[test]
fn the_voice_directive_marks_only_audio_and_the_document_directive_is_reported() {
    let extracted = extract_media(
        "[[audio_as_voice]]\nMEDIA:/r/v.ogg MEDIA:/r/p.png\n[[as_document]] Готово",
        None,
        &nothing,
    );
    assert_eq!(
        extracted,
        MediaReply {
            files: vec![
                MediaFile {
                    path: "/r/v.ogg".into(),
                    is_voice: true
                },
                file("/r/p.png"),
            ],
            text: "Готово".into(),
            as_document: true,
        }
    );
    let plain = extract_media("[[as_document]]Без файлов", None, &nothing);
    assert_eq!(
        plain,
        MediaReply {
            files: Vec::new(),
            text: "Без файлов".into(),
            as_document: true,
        }
    );
}

#[test]
fn a_file_named_twice_goes_once_and_a_tilde_is_the_home() {
    assert_eq!(
        extract_media(
            "MEDIA:~/r.pdf and again MEDIA:/h/r.pdf",
            Some("/h/"),
            &nothing
        ),
        reply(vec![file("/h/r.pdf")], "and again")
    );
    assert_eq!(
        extract_media("MEDIA:~/r.pdf", None, &nothing),
        reply(vec![file("~/r.pdf")], "")
    );
}

#[test]
fn a_path_without_a_known_extension_needs_the_file_check() {
    let exists = |path: &str| {
        ["/r/Caddyfile", "/r/my notes.log"]
            .contains(&path)
            .then(|| format!("/real{path}"))
    };
    assert_eq!(
        extract_media(
            "MEDIA:/r/Caddyfile, MEDIA:/r/gone.weird and MEDIA:/r/my notes.log here",
            None,
            &exists
        ),
        reply(
            vec![file("/real/r/Caddyfile"), file("/real/r/my notes.log")],
            ", MEDIA:/r/gone.weird and  here"
        )
    );
}
