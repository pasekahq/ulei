//! The archive store through its public operations, on synthetic items in a scratch file.

#![allow(missing_docs, clippy::unwrap_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use ulei_memory::archive::{
    Archive, Begun, Caller, DEFAULT_LIST_TEXT_CHARS, Error, Item, MAX_LIST_BODY, MAX_LIST_LIMIT,
    MAX_LIST_TEXT_CHARS, MAX_PART, MAX_RANGE, MAX_UPLOAD_TEXT, Page, Skip, Upload,
};
use ulei_memory::wire::c10::{ErrorCode, ListRequest, ListResponse, PutStatus, SearchRequest};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ulei-archive-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join("store").join("memory.archive")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn own() -> u32 {
    rustix::process::geteuid().as_raw()
}

const CALLER: Caller = Caller {
    pid: 4242,
    uid: 1000,
};

fn item<'a>(source: &'a str, external_id: &'a str, text: &'a str, bytes: &'a [u8]) -> Item<'a> {
    Item {
        source,
        external_id,
        who: None,
        at: None,
        kind: "text/plain",
        text,
        bytes,
    }
}

fn search(q: &str) -> SearchRequest {
    SearchRequest {
        q: q.to_owned(),
        source: None,
        who: None,
        kind: None,
        since: None,
        until: None,
        limit: None,
    }
}

fn list(source: &str) -> ListRequest {
    ListRequest {
        source: source.to_owned(),
        after: None,
        limit: None,
        text_chars: None,
    }
}

/// The `(seq, external_id)` of every entry of `source`, walked in pages of `limit`.
fn walk(archive: &Archive, source: &str, limit: u64) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = Vec::new();
    loop {
        let page = archive
            .list(&ListRequest {
                after: out.last().map(|(seq, _)| *seq),
                limit: Some(limit),
                ..list(source)
            })
            .unwrap();
        assert!(page.entries.len() as u64 <= limit);
        out.extend(page.entries.into_iter().map(|e| (e.seq, e.external_id)));
        if !page.more {
            return out;
        }
    }
}

/// The `external_id`s of `listed`, in order.
fn names(listed: &[(u64, String)]) -> Vec<&str> {
    listed.iter().map(|(_, name)| name.as_str()).collect()
}

/// Bytes deflate cannot shorten.
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[0]
        })
        .collect()
}

fn count(db: &Path, table: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn stored_codec(db: &Path, id: &str) -> (String, usize) {
    let (codec, len): (String, i64) = Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT codec, length(body) FROM blobs WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    (codec, usize::try_from(len).unwrap())
}

#[test]
fn compressible_and_incompressible_originals_read_back_byte_for_byte() {
    let dir = Scratch::new("roundtrip");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let large = "The quarterly ledger lists every invoice of the orchard cooperative.\n"
        .repeat(4_000)
        .into_bytes();
    let binary = noise(64 * 1024);
    let short = b"hi".to_vec();
    let cases = [
        (
            item("notes", "ledger", "quarterly ledger orchard", &large),
            "deflate",
        ),
        (
            item("files", "photo.bin", "photo of the orchard gate", &binary),
            "none",
        ),
        (item("chat", "m-1", "hi", &short), "none"),
    ];
    for (put, codec) in &cases {
        let answer = archive.put(put).unwrap();
        assert_eq!(answer.status, PutStatus::Stored);
        let read = archive.read(&answer.id).unwrap();
        assert_eq!(read.bytes, put.bytes);
        assert_eq!(read.len, put.bytes.len() as u64);
        assert_eq!(read.kind, "text/plain");
        let (stored, body) = stored_codec(&dir.file(), &answer.id);
        assert_eq!(stored, *codec);
        if *codec == "deflate" {
            assert!(body < put.bytes.len() / 10, "{body} bytes stored");
        } else {
            assert_eq!(body, put.bytes.len());
        }
    }
    drop(archive);
    let reopened = Archive::open(&dir.file(), own()).unwrap();
    assert_eq!(reopened.items().unwrap(), 3);
}

#[test]
fn a_body_that_does_not_hash_to_its_id_is_refused_on_read() {
    let dir = Scratch::new("corrupt");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let large = "orchard ".repeat(2_000).into_bytes();
    let binary = noise(4_096);
    let deflated = archive
        .put(&item("notes", "a", "orchard", &large))
        .unwrap()
        .id;
    let raw = archive
        .put(&item("files", "b", "noise", &binary))
        .unwrap()
        .id;
    let db = Connection::open(dir.file()).unwrap();
    db.execute_batch("DROP TRIGGER blobs_immutable").unwrap();
    let other = miniz_oxide::deflate::compress_to_vec("meadows ".repeat(2_000).as_bytes(), 6);
    db.execute(
        "UPDATE blobs SET body = ?1 WHERE id = ?2",
        rusqlite::params![other, deflated],
    )
    .unwrap();
    db.execute(
        "UPDATE blobs SET body = CAST(zeroblob(4096) AS BLOB) WHERE id = ?1",
        [&raw],
    )
    .unwrap();
    for id in [&deflated, &raw] {
        let refused = archive.read(id).unwrap_err();
        assert!(matches!(refused, Error::Corrupt(_)), "{refused}");
        assert_eq!(refused.code(), ErrorCode::StoreFailed);
    }
}

#[test]
fn a_repeated_put_stores_nothing_and_answers_duplicate() {
    let dir = Scratch::new("duplicate");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let put = item(
        "chat",
        "m-7",
        "the gate code changed",
        b"the gate code changed",
    );
    let first = archive.put(&put).unwrap();
    let second = archive.put(&put).unwrap();
    assert_eq!(second.status, PutStatus::Duplicate);
    assert_eq!(second.id, first.id);
    for table in ["blobs", "entries", "item_fts"] {
        assert_eq!(count(&dir.file(), table), 1, "{table}");
    }
    assert_eq!(archive.read(&first.id).unwrap().entries.len(), 1);
}

#[test]
fn changed_bytes_under_a_stored_external_id_are_refused_as_conflict() {
    let dir = Scratch::new("conflict");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let first = archive
        .put(&item("chat", "m-7", "first words", b"first words"))
        .unwrap();
    let refused = archive
        .put(&item("chat", "m-7", "other words", b"other words"))
        .unwrap_err();
    assert!(matches!(&refused, Error::Conflict { stored } if *stored == first.id));
    assert_eq!(refused.code(), ErrorCode::Conflict);
    assert_eq!(archive.items().unwrap(), 1);
    assert_eq!(archive.read(&first.id).unwrap().bytes, b"first words");
    assert_eq!(archive.search(&search("other")).unwrap().len(), 0);
}

#[test]
fn a_new_pair_with_stored_bytes_adds_an_entry_and_no_blob() {
    let dir = Scratch::new("new-pair");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let bytes = b"minutes of the cooperative meeting";
    let first = archive
        .put(&item("mail", "msg-1", "minutes cooperative meeting", bytes))
        .unwrap();
    let second = archive
        .put(&Item {
            who: Some("treasurer"),
            kind: "message/rfc822",
            ..item("drive", "doc-9", "ignored text", bytes)
        })
        .unwrap();
    assert_eq!(second.status, PutStatus::Stored);
    assert_eq!(second.id, first.id);
    assert_eq!(count(&dir.file(), "blobs"), 1);
    assert_eq!(count(&dir.file(), "item_fts"), 1);
    let read = archive.read(&first.id).unwrap();
    let pairs: Vec<_> = read
        .entries
        .iter()
        .map(|e| (e.source.as_str(), e.external_id.as_str(), e.who.as_deref()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("mail", "msg-1", None),
            ("drive", "doc-9", Some("treasurer"))
        ]
    );
    assert_eq!(read.kind, "text/plain");
}

#[test]
fn fts5_and_every_metadata_filter_find_the_item() {
    let dir = Scratch::new("filters");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let target = archive
        .put(&Item {
            who: Some("alice"),
            at: Some(1_000.0),
            kind: "text/markdown",
            ..item(
                "notes",
                "n-1",
                "Pruning the pear trees before the frost",
                b"n-1",
            )
        })
        .unwrap()
        .id;
    let other = archive
        .put(&Item {
            who: Some("bob"),
            at: Some(5_000.0),
            ..item("chat", "c-1", "The pear harvest was late", b"c-1")
        })
        .unwrap()
        .id;
    archive
        .put(&item("chat", "c-2", "The pear jam has no date", b"c-2"))
        .unwrap();
    let hits = archive.search(&search("pruning")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, target);
    assert!(
        hits[0].snippet.contains(">>>Pruning<<<"),
        "{}",
        hits[0].snippet
    );
    assert_eq!(hits[0].entries[0].external_id, "n-1");

    let only = |request: SearchRequest| -> Vec<String> {
        archive
            .search(&request)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect()
    };
    let pear = search("pear");
    assert_eq!(only(pear.clone()).len(), 3);
    let filtered = [
        SearchRequest {
            source: Some("notes".into()),
            ..pear.clone()
        },
        SearchRequest {
            who: Some("alice".into()),
            ..pear.clone()
        },
        SearchRequest {
            kind: Some("text/markdown".into()),
            ..pear.clone()
        },
        SearchRequest {
            until: Some(1_000.0),
            ..pear.clone()
        },
        SearchRequest {
            since: Some(500.0),
            until: Some(2_000.0),
            ..pear.clone()
        },
    ];
    for request in filtered {
        assert_eq!(
            only(request.clone()),
            std::slice::from_ref(&target),
            "{request:?}"
        );
    }
    assert_eq!(
        only(SearchRequest {
            since: Some(1_001.0),
            ..pear.clone()
        }),
        [other]
    );
    assert_eq!(
        only(SearchRequest {
            limit: Some(2),
            ..pear
        })
        .len(),
        2
    );
}

#[test]
fn a_query_with_fts5_syntax_characters_does_not_error() {
    let dir = Scratch::new("syntax");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let id = archive
        .put(&item(
            "notes",
            "n-1",
            "send the quarterly-report to the orchard",
            b"n-1",
        ))
        .unwrap()
        .id;
    for q in [
        "\"unbalanced",
        "(orchard",
        "orchard AND OR report",
        "NEAR(orchard",
        "text:orchard",
        "^orchard",
        "*",
        "-",
        "AND",
        "a:b:c {x} [y] <z>",
        "'; DROP TABLE blobs; --",
    ] {
        archive.search(&search(q)).unwrap();
    }
    for q in ["quarterly-report?", "(orchard)", "\"the orchard\""] {
        let hits = archive.search(&search(q)).unwrap();
        assert_eq!(hits.iter().map(|h| &h.id).collect::<Vec<_>>(), [&id], "{q}");
    }
}

#[test]
fn only_delete_removes_an_item_and_its_audit_row_is_kept() {
    let dir = Scratch::new("delete");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let bytes = b"a letter the owner wants gone";
    let id = archive
        .put(&Item {
            at: Some(42.0),
            ..item("mail", "msg-1", "letter owner gone", bytes)
        })
        .unwrap()
        .id;
    archive
        .put(&item("drive", "doc-1", "letter", bytes))
        .unwrap();
    archive
        .put(&item("mail", "msg-1", "letter", bytes))
        .unwrap();
    archive.search(&search("letter")).unwrap();
    archive.read(&id).unwrap();
    assert_eq!(archive.items().unwrap(), 1);

    let deletion = archive.delete(&id, "owner asked", CALLER).unwrap();
    assert_eq!(
        (deletion.id.as_str(), deletion.len),
        (id.as_str(), bytes.len() as u64)
    );
    assert_eq!((deletion.pid, deletion.uid), (CALLER.pid, CALLER.uid));
    assert_eq!(deletion.reason, "owner asked");
    let removed: Vec<_> = deletion
        .entries
        .iter()
        .map(|e| (e.source.as_str(), e.at))
        .collect();
    assert_eq!(removed, [("mail", Some(42.0)), ("drive", None)]);
    assert!(matches!(archive.read(&id), Err(Error::NotFound(_))));
    assert_eq!(archive.search(&search("letter")).unwrap().len(), 0);
    for table in ["blobs", "entries", "item_fts"] {
        assert_eq!(count(&dir.file(), table), 0, "{table}");
    }
    assert_eq!(
        archive.deletions().unwrap(),
        std::slice::from_ref(&deletion)
    );
    let again = archive.delete(&id, "twice", CALLER).unwrap_err();
    assert_eq!(again.code(), ErrorCode::NotFound);

    let db = Connection::open(dir.file()).unwrap();
    for sql in [
        "DELETE FROM deletions",
        "UPDATE deletions SET reason = 'edited'",
    ] {
        assert!(db.execute(sql, []).is_err(), "{sql}");
    }
    assert_eq!(archive.deletions().unwrap(), [deletion]);
}

#[test]
fn a_stored_row_cannot_be_updated() {
    let dir = Scratch::new("immutable");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let id = archive
        .put(&item("notes", "n-1", "kept as it came", b"kept as it came"))
        .unwrap()
        .id;
    let db = Connection::open(dir.file()).unwrap();
    for sql in [
        "UPDATE blobs SET len = 0",
        "UPDATE entries SET who = 'someone else'",
    ] {
        assert!(db.execute(sql, []).is_err(), "{sql}");
    }
    assert_eq!(archive.read(&id).unwrap().bytes, b"kept as it came");
}

#[test]
fn an_external_id_over_512_utf8_bytes_is_refused() {
    let dir = Scratch::new("external-id");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let at_limit = "ж".repeat(256);
    let over = "ж".repeat(257);
    assert_eq!(over.chars().count(), 257);
    archive
        .put(&item("chat", &at_limit, "fits", b"fits"))
        .unwrap();
    let refused = archive
        .put(&item("chat", &over, "too long", b"too long"))
        .unwrap_err();
    assert_eq!(refused.code(), ErrorCode::InvalidRequest);
    assert_eq!(archive.items().unwrap(), 1);
}

#[test]
fn malformed_inputs_are_invalid_requests() {
    let dir = Scratch::new("invalid");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let refusals = [
        archive.put(&item("Chat", "m-1", "x", b"x")).unwrap_err(),
        archive.put(&item("chat", "", "x", b"x")).unwrap_err(),
        archive
            .put(&Item {
                at: Some(-1.0),
                ..item("chat", "m-1", "x", b"x")
            })
            .unwrap_err(),
        archive.search(&search("")).unwrap_err(),
        archive
            .search(&SearchRequest {
                limit: Some(51),
                ..search("x")
            })
            .unwrap_err(),
        archive.read("ABC").unwrap_err(),
        archive.list(&list("Chat")).unwrap_err(),
        archive
            .list(&ListRequest {
                limit: Some(0),
                ..list("chat")
            })
            .unwrap_err(),
        archive
            .list(&ListRequest {
                limit: Some(MAX_LIST_LIMIT + 1),
                ..list("chat")
            })
            .unwrap_err(),
        archive
            .list(&ListRequest {
                text_chars: Some(0),
                ..list("chat")
            })
            .unwrap_err(),
        archive
            .list(&ListRequest {
                text_chars: Some(MAX_LIST_TEXT_CHARS + 1),
                ..list("chat")
            })
            .unwrap_err(),
        archive
            .list(&ListRequest {
                after: Some(u64::MAX),
                ..list("chat")
            })
            .unwrap_err(),
        archive.delete(&"0".repeat(64), "", CALLER).unwrap_err(),
    ];
    for refused in refusals {
        assert_eq!(refused.code(), ErrorCode::InvalidRequest, "{refused}");
    }
    assert_eq!(
        archive.read(&"0".repeat(64)).unwrap_err().code(),
        ErrorCode::NotFound
    );
}

#[test]
fn the_file_is_0600_in_a_0700_directory_in_wal_mode() {
    let dir = Scratch::new("modes");
    let archive = Archive::open(&dir.file(), own()).unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir.file()), 0o600);
    assert_eq!(mode(dir.file().parent().unwrap()), 0o700);
    let journal_mode: String = Connection::open(dir.file())
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal_mode, "wal");
    drop(archive);
}

#[test]
fn a_page_with_its_skips_and_cursor_is_stored_whole_or_not_at_all() {
    let dir = Scratch::new("page");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    archive
        .put(&item("dialogue", "owner/1/1", "kept", b"old"))
        .unwrap();
    let skip = Skip {
        external_id: "owner/1/2",
        len: 900_000,
        at: Some(1_700_000_000.0),
    };
    let mut page = Page {
        source: "dialogue",
        items: vec![
            item("dialogue", "owner/1/0", "tulips", b"first"),
            item("dialogue", "owner/1/1", "kept", b"changed"),
        ],
        skipped: vec![skip],
        cursor: Some(3),
    };
    assert!(matches!(archive.pull(&page), Err(Error::Conflict { .. })));
    assert_eq!(archive.cursor("dialogue").unwrap(), None);
    assert_eq!(archive.items().unwrap(), 1);
    assert_eq!(archive.skipped().unwrap(), []);

    page.items[1] = item("dialogue", "owner/1/1", "kept", b"old");
    assert_eq!(archive.pull(&page).unwrap(), 1);
    assert_eq!(
        archive.pull(&page).unwrap(),
        0,
        "a repeated page adds nothing"
    );
    assert_eq!(archive.cursor("dialogue").unwrap(), Some(3));
    assert_eq!(archive.items().unwrap(), 2);
    let kept = archive.skipped().unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        (kept[0].external_id.as_str(), kept[0].len, kept[0].at),
        ("owner/1/2", 900_000, Some(1_700_000_000.0))
    );

    page.cursor = None;
    page.items.clear();
    archive.pull(&page).unwrap();
    assert_eq!(
        archive.cursor("dialogue").unwrap(),
        Some(3),
        "a page without a cursor keeps the stored one"
    );
}

#[test]
fn a_store_directory_of_another_uid_is_refused_and_nothing_is_created() {
    let dir = Scratch::new("foreign");
    fs::create_dir_all(dir.file().parent().unwrap()).unwrap();
    fs::set_permissions(
        dir.file().parent().unwrap(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let other = own().wrapping_add(1);
    let refused = Archive::open(&dir.file(), other).unwrap_err();
    assert!(
        matches!(&refused, Error::ForeignOwner { uid, .. } if *uid == own()),
        "{refused}"
    );
    assert!(refused.to_string().contains("belongs to uid"), "{refused}");
    assert!(!dir.file().exists());
}

#[test]
fn a_version_1_file_gains_the_skipped_table_and_keeps_its_items() {
    let dir = Scratch::new("upgrade");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let id = archive
        .put(&item("notes", "note-1", "orchard", b"kept"))
        .unwrap()
        .id;
    drop(archive);
    Connection::open(dir.file())
        .unwrap()
        .execute_batch(
            "DROP TABLE skipped; DROP TABLE chunked; DROP TABLE parts; DROP TABLE uploads; \
             DROP TRIGGER entries_seq; DROP TRIGGER entries_seq_gone; DROP TABLE entry_seq; \
             PRAGMA user_version = 1;",
        )
        .unwrap();

    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    assert_eq!(archive.read(&id).unwrap().bytes, b"kept");
    let page = Page {
        source: "dialogue",
        items: vec![],
        skipped: vec![Skip {
            external_id: "owner/1/0",
            len: 1,
            at: None,
        }],
        cursor: Some(1),
    };
    archive.pull(&page).unwrap();
    assert_eq!(archive.skipped().unwrap().len(), 1);
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn upload<'a>(external_id: &'a str, text: &'a str, original: &[u8], id: &'a str) -> Upload<'a> {
    Upload {
        source: "library",
        external_id,
        who: Some("owner"),
        at: Some(1_700_000_000.0),
        kind: "application/pdf",
        text,
        len: original.len() as u64,
        id,
    }
}

/// Opens an upload of `original` and sends it in parts of `part` bytes; the upload's number.
fn send(archive: &mut Archive, up: &Upload<'_>, original: &[u8], part: usize) -> u64 {
    let Begun::Upload(n) = archive.begin(up).unwrap() else {
        panic!("no upload opened");
    };
    for (i, chunk) in original.chunks(part).enumerate() {
        archive.part(n, i as u64, chunk).unwrap();
    }
    n
}

/// Five and a half MiB, half of it compressible.
fn big() -> Vec<u8> {
    let mut out = noise(11 * MAX_PART / 2);
    out.extend(std::iter::repeat_n(b"orchard ", 11 * MAX_PART / 16).flatten());
    out
}

/// The whole original of `id` read back by ranges of `step` bytes.
fn by_ranges(archive: &Archive, id: &str, step: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let range = archive.read_range(id, out.len() as u64, step).unwrap();
        assert_eq!(range.offset, Some(out.len() as u64));
        if range.bytes.is_empty() {
            return out;
        }
        out.extend_from_slice(&range.bytes);
    }
}

#[test]
fn an_original_in_parts_is_stored_once_whole_and_read_back_by_ranges() {
    let dir = Scratch::new("parts");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let original = big();
    let id = sha(&original);
    let up = upload("library/manual.pdf", "orchard manual", &original, &id);
    let n = send(&mut archive, &up, &original, MAX_PART);
    assert_eq!(
        archive.items().unwrap(),
        0,
        "nothing is stored before the end"
    );
    let put = archive.end(n).unwrap();
    assert_eq!(
        (put.id.as_str(), put.status),
        (id.as_str(), PutStatus::Stored)
    );
    assert_eq!(archive.items().unwrap(), 1);
    assert_eq!(archive.uploads().unwrap(), 0);
    assert_eq!(count(&dir.file(), "parts"), 11);

    let whole = archive.read(&id).unwrap_err();
    assert_eq!(whole.code(), ErrorCode::TooLarge, "{whole}");
    for step in [MAX_RANGE, 300_001, 65_537] {
        let back = by_ranges(&archive, &id, step);
        assert_eq!(sha(&back), id, "step {step}");
    }
    let middle = archive.read_range(&id, MAX_PART as u64 - 10, 20).unwrap();
    assert_eq!(middle.bytes, &original[MAX_PART - 10..MAX_PART + 10]);
    assert_eq!(middle.len, original.len() as u64);
    assert_eq!(middle.kind, "application/pdf");
    assert_eq!(middle.entries.len(), 1);
    let past = archive
        .read_range(&id, original.len() as u64 + 1, 1)
        .unwrap_err();
    assert_eq!(past.code(), ErrorCode::InvalidRequest);
    let over = archive.read_range(&id, 0, MAX_RANGE + 1).unwrap_err();
    assert_eq!(over.code(), ErrorCode::InvalidRequest);
    let hits = archive.search(&search("orchard")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, id);

    // A small item reads whole as before, and by range too.
    let small = archive
        .put(&item("notes", "n-1", "pear", b"0123456789"))
        .unwrap()
        .id;
    assert_eq!(archive.read(&small).unwrap().bytes, b"0123456789");
    assert_eq!(archive.read_range(&small, 3, 4).unwrap().bytes, b"3456");
    assert_eq!(archive.read_range(&small, 10, 4).unwrap().bytes, b"");
}

#[test]
fn a_repeated_original_in_parts_is_settled_at_begin_without_parts() {
    let dir = Scratch::new("parts-dup");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let original = noise(MAX_PART + 7);
    let id = sha(&original);
    let up = upload("library/a.bin", "a", &original, &id);
    let n = send(&mut archive, &up, &original, MAX_PART);
    archive.end(n).unwrap();

    let again = archive.begin(&up).unwrap();
    let Begun::Done(put) = again else {
        panic!("{again:?}")
    };
    assert_eq!(put.status, PutStatus::Duplicate);
    let copy = upload("library/copy-of-a.bin", "a", &original, &id);
    let Begun::Done(put) = archive.begin(&copy).unwrap() else {
        panic!("the copy opened an upload")
    };
    assert_eq!(put.status, PutStatus::Stored);
    assert_eq!(archive.items().unwrap(), 1);
    assert_eq!(count(&dir.file(), "parts"), 2);
    assert_eq!(count(&dir.file(), "entries"), 2);

    let other = noise(MAX_PART + 8);
    let other_id = sha(&other);
    let conflict = archive
        .begin(&upload("library/a.bin", "a", &other, &other_id))
        .unwrap_err();
    assert_eq!(conflict.code(), ErrorCode::Conflict);

    // The same bytes by `put` add an entry and no blob.
    let put = archive.put(&item("notes", "n-a", "a", &original)).unwrap();
    assert_eq!((put.id, put.status), (id.clone(), PutStatus::Stored));
    assert_eq!(archive.items().unwrap(), 1);
    assert_eq!(sha(&by_ranges(&archive, &id, MAX_RANGE)), id);
}

#[test]
fn parts_that_miss_bytes_or_hash_to_another_id_store_nothing() {
    let dir = Scratch::new("parts-bad");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let original = noise(2 * MAX_PART + 1);
    let id = sha(&original);
    let up = upload("library/b.bin", "b", &original, &id);

    let mut wrong = original.clone();
    wrong[MAX_PART + 3] ^= 1;
    let n = send(&mut archive, &up, &wrong, MAX_PART);
    let refused = archive.end(n).unwrap_err();
    assert_eq!(refused.code(), ErrorCode::InvalidRequest);
    assert!(refused.to_string().contains("not to the id"), "{refused}");

    let Begun::Upload(n) = archive.begin(&up).unwrap() else {
        panic!()
    };
    archive.part(n, 0, &original[..MAX_PART]).unwrap();
    let refused = archive.end(n).unwrap_err();
    assert_eq!(refused.code(), ErrorCode::InvalidRequest);
    assert!(refused.to_string().contains("holds 524288 of"), "{refused}");

    for table in [
        "blobs", "entries", "item_fts", "chunked", "parts", "uploads",
    ] {
        assert_eq!(count(&dir.file(), table), 0, "{table}");
    }
    let gone = archive.end(n).unwrap_err();
    assert_eq!(gone.code(), ErrorCode::NotFound);
}

#[test]
fn parts_go_in_order_within_the_length_and_the_size_bound() {
    let dir = Scratch::new("parts-order");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let original = noise(MAX_PART + 2);
    let id = sha(&original);
    let Begun::Upload(n) = archive
        .begin(&upload("library/c.bin", "c", &original, &id))
        .unwrap()
    else {
        panic!()
    };
    let refuse = |e: Error| assert_eq!(e.code(), ErrorCode::InvalidRequest, "{e}");
    refuse(archive.part(n, 1, b"x").unwrap_err());
    refuse(archive.part(n, 0, b"").unwrap_err());
    refuse(archive.part(n, 0, &vec![0; MAX_PART + 1]).unwrap_err());
    archive.part(n, 0, &original[..MAX_PART]).unwrap();
    refuse(archive.part(n, 1, b"abc").unwrap_err());
    let got = archive.part(n, 1, &original[MAX_PART..]).unwrap();
    assert_eq!((got.parts, got.bytes), (2, original.len() as u64));
    assert_eq!(
        archive.part(n + 1, 0, b"x").unwrap_err().code(),
        ErrorCode::NotFound
    );
    let mut bad = upload("library/c.bin", "c", &original, &id);
    bad.len = 0;
    refuse(archive.begin(&bad).unwrap_err());
    let mut bad = upload("library/c.bin", "c", &original, "ABC");
    bad.len = 1;
    refuse(archive.begin(&bad).unwrap_err());
    assert_eq!(archive.end(n).unwrap().status, PutStatus::Stored);
}

#[test]
fn an_abandoned_upload_does_not_block_a_new_one_and_expires_with_its_parts() {
    let dir = Scratch::new("parts-expire");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let original = noise(MAX_PART + 9);
    let id = sha(&original);
    let up = upload("library/d.bin", "d", &original, &id);
    let Begun::Upload(abandoned) = archive.begin(&up).unwrap() else {
        panic!()
    };
    archive.part(abandoned, 0, &original[..MAX_PART]).unwrap();

    let n = send(&mut archive, &up, &original, MAX_PART);
    assert_ne!(n, abandoned);
    assert_eq!(archive.end(n).unwrap().status, PutStatus::Stored);
    assert_eq!(archive.uploads().unwrap(), 1);

    archive.set_upload_ttl(std::time::Duration::from_secs(1));
    std::thread::sleep(std::time::Duration::from_millis(1_200));
    assert_eq!(
        archive
            .part(abandoned, 1, &original[MAX_PART..])
            .unwrap_err()
            .code(),
        ErrorCode::NotFound
    );
    let other = noise(MAX_PART + 10);
    let other_id = sha(&other);
    let next = send(
        &mut archive,
        &upload("library/e.bin", "e", &other, &other_id),
        &other,
        MAX_PART,
    );
    assert_eq!(
        archive.uploads().unwrap(),
        1,
        "the abandoned upload expired"
    );
    assert!(next > n, "an upload's number is never given twice");
    archive.end(next).unwrap();
    assert_eq!(count(&dir.file(), "parts"), 4);
}

#[test]
fn a_finished_upload_of_an_entry_another_upload_settled_adds_nothing() {
    let dir = Scratch::new("parts-race");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let original = noise(MAX_PART + 11);
    let id = sha(&original);
    let up = upload("library/f.bin", "f", &original, &id);
    let first = send(&mut archive, &up, &original, MAX_PART);
    let second = send(&mut archive, &up, &original, MAX_PART);
    assert_eq!(archive.end(second).unwrap().status, PutStatus::Stored);
    assert_eq!(archive.end(first).unwrap().status, PutStatus::Duplicate);
    assert_eq!(count(&dir.file(), "parts"), 2);
    assert_eq!(archive.uploads().unwrap(), 0);
}

#[test]
fn the_text_of_an_upload_is_cut_to_its_bound_at_a_character() {
    let dir = Scratch::new("parts-text");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let original = noise(MAX_PART + 1);
    let id = sha(&original);
    let text = format!("{}é tailword", "x".repeat(MAX_UPLOAD_TEXT - 1));
    let n = send(
        &mut archive,
        &upload("library/g.bin", &text, &original, &id),
        &original,
        MAX_PART,
    );
    archive.end(n).unwrap();
    let kept: i64 = Connection::open(dir.file())
        .unwrap()
        .query_row("SELECT length(CAST(text AS BLOB)) FROM item_fts", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(kept, i64::try_from(MAX_UPLOAD_TEXT - 1).unwrap());
    assert_eq!(archive.search(&search("tailword")).unwrap().len(), 0);
}

#[test]
fn delete_removes_an_item_in_parts_with_every_part() {
    let dir = Scratch::new("parts-delete");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let original = noise(2 * MAX_PART + 5);
    let id = sha(&original);
    let n = send(
        &mut archive,
        &upload("library/h.bin", "h", &original, &id),
        &original,
        MAX_PART,
    );
    archive.end(n).unwrap();
    let kept = archive.put(&item("notes", "n-1", "k", b"kept")).unwrap().id;
    let deletion = archive.delete(&id, "test", CALLER).unwrap();
    assert_eq!(deletion.len, original.len() as u64);
    for table in ["chunked", "parts", "item_fts"] {
        let left = i64::from(table == "item_fts");
        assert_eq!(count(&dir.file(), table), left, "{table}");
    }
    assert_eq!(archive.items().unwrap(), 1);
    assert_eq!(archive.read(&kept).unwrap().bytes, b"kept");
    assert_eq!(
        archive.read_range(&id, 0, 1).unwrap_err().code(),
        ErrorCode::NotFound
    );
}

/// The schema of version 2, as the archive wrote it before parts.
const VERSION_2: &str = "
CREATE TABLE blobs (row INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, len INTEGER NOT NULL,
  codec TEXT NOT NULL CHECK (codec IN ('deflate', 'none')), body BLOB NOT NULL);
CREATE TABLE entries (source TEXT NOT NULL, external_id TEXT NOT NULL,
  id TEXT NOT NULL REFERENCES blobs (id), who TEXT, at REAL, kind TEXT NOT NULL,
  put_at REAL NOT NULL, UNIQUE (source, external_id));
CREATE INDEX entries_id ON entries (id);
CREATE VIRTUAL TABLE item_fts USING fts5(text, tokenize = 'unicode61 remove_diacritics 2');
CREATE TABLE cursors (source TEXT PRIMARY KEY, cursor INTEGER NOT NULL);
CREATE TABLE deletions (row INTEGER PRIMARY KEY, deleted_at REAL NOT NULL, id TEXT NOT NULL,
  len INTEGER NOT NULL, entries TEXT NOT NULL, reason TEXT NOT NULL, pid INTEGER NOT NULL,
  uid INTEGER NOT NULL);
CREATE TRIGGER blobs_immutable BEFORE UPDATE ON blobs
  BEGIN SELECT RAISE(ABORT, 'archived rows are immutable'); END;
CREATE TRIGGER entries_immutable BEFORE UPDATE ON entries
  BEGIN SELECT RAISE(ABORT, 'archived rows are immutable'); END;
CREATE TRIGGER deletions_immutable BEFORE UPDATE ON deletions
  BEGIN SELECT RAISE(ABORT, 'the audit is never changed'); END;
CREATE TRIGGER deletions_kept BEFORE DELETE ON deletions
  BEGIN SELECT RAISE(ABORT, 'the audit is never removed'); END;
CREATE TABLE skipped (row INTEGER PRIMARY KEY, source TEXT NOT NULL, external_id TEXT NOT NULL,
  len INTEGER NOT NULL, at REAL, skipped_at REAL NOT NULL, UNIQUE (source, external_id));
CREATE TRIGGER skipped_immutable BEFORE UPDATE ON skipped
  BEGIN SELECT RAISE(ABORT, 'skipped references are immutable'); END;
CREATE TRIGGER skipped_kept BEFORE DELETE ON skipped
  BEGIN SELECT RAISE(ABORT, 'skipped references are never removed'); END;
PRAGMA user_version = 2;
";

#[test]
fn a_version_2_file_opens_without_a_rewrite_and_reads_and_takes_parts() {
    let dir = Scratch::new("upgrade-2");
    fs::create_dir_all(dir.file().parent().unwrap()).unwrap();
    fs::set_permissions(
        dir.file().parent().unwrap(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let id = sha(b"old item");
    {
        let conn = Connection::open(dir.file()).unwrap();
        conn.execute_batch(VERSION_2).unwrap();
        conn.execute(
            "INSERT INTO blobs (id, len, codec, body) VALUES (?1, 8, 'none', CAST('old item' AS BLOB))",
            [&id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO item_fts (rowid, text) VALUES (1, 'heirloom')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entries (source, external_id, id, kind, put_at) \
             VALUES ('notes', 'old-1', ?1, 'text/plain', 1)",
            [&id],
        )
        .unwrap();
    }
    fs::set_permissions(dir.file(), fs::Permissions::from_mode(0o600)).unwrap();
    let before: Vec<u8> = Connection::open(dir.file())
        .unwrap()
        .query_row("SELECT body FROM blobs", [], |r| r.get(0))
        .unwrap();

    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    assert_eq!(archive.read(&id).unwrap().bytes, b"old item");
    assert_eq!(archive.search(&search("heirloom")).unwrap()[0].id, id);
    let original = noise(MAX_PART + 3);
    let big_id = sha(&original);
    let n = send(
        &mut archive,
        &upload("library/i.bin", "i", &original, &big_id),
        &original,
        MAX_PART,
    );
    archive.end(n).unwrap();
    assert_eq!(archive.items().unwrap(), 2);
    drop(archive);
    let conn = Connection::open(dir.file()).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 4);
    let after: Vec<u8> = conn
        .query_row("SELECT body FROM blobs WHERE id = ?1", [&id], |r| r.get(0))
        .unwrap();
    assert_eq!(after, before);
}

#[test]
fn list_walks_a_source_by_seq_in_pages_and_leaves_other_sources_out() {
    let dir = Scratch::new("list");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let mut expected = Vec::new();
    for n in 0..7 {
        let name = format!("n-{n}");
        let text = format!("note {n}");
        archive
            .put(&Item {
                who: Some("owner"),
                at: Some(1_700_000_000.0 + f64::from(n)),
                ..item("notes", &name, &text, text.as_bytes())
            })
            .unwrap();
        expected.push(name);
        if n % 3 == 0 {
            let other = format!("m-{n}");
            archive
                .put(&item("mail", &other, "mail", other.as_bytes()))
                .unwrap();
        }
    }
    // An entry of stored bytes, one settled at `begin`, one stored at `end` and a pulled one.
    archive
        .put(&item("notes", "again", "other text", b"note 2"))
        .unwrap();
    let stored = sha(b"note 3");
    let settled = Upload {
        source: "notes",
        ..upload("settled", "t", b"note 3", &stored)
    };
    assert!(matches!(archive.begin(&settled).unwrap(), Begun::Done(_)));
    let original = noise(MAX_PART + 1);
    let big_id = sha(&original);
    let up = Upload {
        source: "notes",
        ..upload("uploaded", "big", &original, &big_id)
    };
    let n = send(&mut archive, &up, &original, MAX_PART);
    archive.end(n).unwrap();
    let pulled = Page {
        source: "notes",
        items: vec![item("notes", "pulled", "pulled", b"pulled")],
        skipped: vec![],
        cursor: None,
    };
    archive.pull(&pulled).unwrap();
    expected.extend(["again", "settled", "uploaded", "pulled"].map(str::to_owned));

    for limit in [1, 3, 100] {
        let walked = walk(&archive, "notes", limit);
        assert_eq!(names(&walked), expected, "limit {limit}");
        assert!(walked.windows(2).all(|w| w[0].0 < w[1].0), "{walked:?}");
    }
    let page = archive.list(&list("notes")).unwrap();
    assert!(!page.more);
    let first = &page.entries[0];
    assert_eq!(
        (
            first.external_id.as_str(),
            first.id.as_str(),
            first.who.as_deref(),
            first.at,
            first.kind.as_str(),
            first.text.as_str(),
            first.text_truncated
        ),
        (
            "n-0",
            sha(b"note 0").as_str(),
            Some("owner"),
            Some(1_700_000_000.0),
            "text/plain",
            "note 0",
            None
        )
    );
    assert!(first.put_at > 0.0);
    let again = page.entries.iter().find(|e| e.external_id == "again");
    assert_eq!(again.unwrap().text, "note 2", "the text is the item's");

    let mail = walk(&archive, "mail", 2);
    assert_eq!(names(&mail), ["m-0", "m-3", "m-6"]);
    let last = page.entries.last().unwrap().seq;
    let after = archive
        .list(&ListRequest {
            after: Some(last),
            ..list("notes")
        })
        .unwrap();
    assert_eq!(
        after,
        ListResponse {
            entries: vec![],
            more: false
        }
    );
    assert_eq!(archive.list(&list("nobody")).unwrap().entries, []);
}

#[test]
fn list_cuts_each_text_to_its_characters_and_says_when_it_cut() {
    let dir = Scratch::new("list-text");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let bound = usize::try_from(DEFAULT_LIST_TEXT_CHARS).unwrap();
    let exact = "ö".repeat(bound);
    let long = "ö".repeat(bound + 1);
    archive.put(&item("notes", "exact", &exact, b"1")).unwrap();
    archive.put(&item("notes", "long", &long, b"2")).unwrap();
    archive
        .put(&item("notes", "short", "orchard", b"3"))
        .unwrap();

    let page = archive.list(&list("notes")).unwrap();
    let texts: Vec<_> = page
        .entries
        .iter()
        .map(|e| (e.text.chars().count(), e.text_truncated))
        .collect();
    assert_eq!(texts, [(bound, None), (bound, Some(true)), (7, None)]);
    assert_eq!(page.entries[1].text, exact);

    let page = archive
        .list(&ListRequest {
            text_chars: Some(5),
            ..list("notes")
        })
        .unwrap();
    assert_eq!(page.entries[2].text, "orcha");
    assert_eq!(page.entries[2].text_truncated, Some(true));
}

#[test]
fn a_list_page_ends_before_its_body_would_pass_the_byte_cap() {
    let dir = Scratch::new("list-cap");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let chars = usize::try_from(MAX_LIST_TEXT_CHARS).unwrap();
    for n in 0..20 {
        let text = format!("{n:02}{}", "\u{1f333}".repeat(chars));
        let name = format!("tree-{n:02}");
        archive
            .put(&item("notes", &name, &text, name.as_bytes()))
            .unwrap();
    }
    let request = ListRequest {
        limit: Some(MAX_LIST_LIMIT),
        text_chars: Some(MAX_LIST_TEXT_CHARS),
        ..list("notes")
    };
    let page = archive.list(&request).unwrap();
    let body = serde_json::to_vec(&page).unwrap().len();
    assert!(page.more);
    assert!(
        (1..20).contains(&page.entries.len()),
        "{}",
        page.entries.len()
    );
    assert!(body <= MAX_LIST_BODY, "{body}");
    assert!(page.entries.iter().all(|e| e.text_truncated == Some(true)));
    let next = archive
        .list(&ListRequest {
            after: Some(page.entries.last().unwrap().seq),
            limit: Some(1),
            ..request.clone()
        })
        .unwrap();
    let one_more = serde_json::to_vec(&next.entries[0]).unwrap().len();
    assert!(body + 1 + one_more > MAX_LIST_BODY, "{body} + {one_more}");
    assert_eq!(walk(&archive, "notes", MAX_LIST_LIMIT).len(), 20);
}

#[test]
fn a_version_3_file_numbers_its_entries_in_put_order_and_new_ones_after() {
    let dir = Scratch::new("upgrade-3");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let a = archive.put(&item("notes", "a", "a", b"a")).unwrap().id;
    archive.put(&item("notes", "b", "b", b"b")).unwrap();
    archive.put(&item("mail", "c", "c", b"c")).unwrap();
    drop(archive);
    let conn = Connection::open(dir.file()).unwrap();
    conn.execute_batch(
        "DROP TRIGGER entries_seq; DROP TRIGGER entries_seq_gone; DROP TABLE entry_seq; \
         PRAGMA user_version = 3;",
    )
    .unwrap();
    // A later row put earlier: the upgrade numbers by `put_at`, then by row.
    conn.execute(
        "INSERT INTO entries (source, external_id, id, kind, put_at) \
         VALUES ('notes', 'early', ?1, 'text/plain', 1)",
        [&a],
    )
    .unwrap();
    let rows = |conn: &Connection| -> Vec<(String, String, f64)> {
        let mut st = conn
            .prepare("SELECT external_id, id, put_at FROM entries ORDER BY rowid")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before = rows(&conn);
    drop(conn);

    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let notes = walk(&archive, "notes", 2);
    assert_eq!(names(&notes), ["early", "a", "b"]);
    let mail = walk(&archive, "mail", 2);
    assert_eq!(mail, [(4, "c".to_owned())]);
    assert_eq!(
        notes.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    archive.put(&item("mail", "d", "d", b"d")).unwrap();
    archive.put(&item("notes", "e", "e", b"a")).unwrap();
    assert_eq!(
        walk(&archive, "mail", 9),
        [(4, "c".to_owned()), (5, "d".to_owned())]
    );
    assert_eq!(
        walk(&archive, "notes", 9).last().unwrap(),
        &(6, "e".to_owned())
    );
    drop(archive);

    let conn = Connection::open(dir.file()).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 4);
    assert_eq!(rows(&conn)[..before.len()], before[..]);
    assert!(conn.execute("UPDATE entry_seq SET seq = 99", []).is_err());
}

#[test]
fn a_deleted_items_entries_leave_the_list_and_no_seq_is_given_twice() {
    let dir = Scratch::new("list-delete");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    archive.put(&item("notes", "a", "a", b"a")).unwrap();
    archive.put(&item("notes", "c", "c", b"c")).unwrap();
    let gone = archive.put(&item("notes", "b", "b", b"b")).unwrap().id;
    archive.put(&item("mail", "b", "b", b"b")).unwrap();
    let before = walk(&archive, "notes", 9);
    assert_eq!(names(&before), ["a", "c", "b"]);

    archive.delete(&gone, "owner asked", CALLER).unwrap();
    let after = walk(&archive, "notes", 1);
    assert_eq!(after, before[..2]);
    assert_eq!(walk(&archive, "mail", 9), []);
    assert_eq!(count(&dir.file(), "entry_seq"), 2);

    // The deleted entries had the seqs 3 and 4; neither is given again.
    archive.put(&item("notes", "b", "b again", b"b2")).unwrap();
    let again = walk(&archive, "notes", 9);
    assert_eq!(again[..2], before[..2]);
    assert_eq!(again[2], (5, "b".to_owned()));
}
