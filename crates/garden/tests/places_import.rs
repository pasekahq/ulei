//! `ulei-garden places import` through the built binary against a daemon on a scratch store,
//! read back with plain SQL. Every place, source and date is invented.

#![allow(missing_docs, clippy::unwrap_used)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use ulei_garden::store::Store;

const GARDEN: &str = env!("CARGO_BIN_EXE_ulei-garden");
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/places-fennwick.md"
);

/// A fresh 0700 directory short enough for a socket path, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let temp = std::env::temp_dir();
        let base = if temp.as_os_str().len() <= 48 {
            temp
        } else {
            PathBuf::from("/tmp")
        };
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("ulei-garden-pi-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self(dir)
    }

    fn store(&self) -> PathBuf {
        self.0.join("garden.sqlite3")
    }

    /// `ulei.toml` of a daemon on the scratch store, with a synthetic producer key.
    fn config(&self) -> PathBuf {
        let key = self.0.join("producer.key");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key)
            .unwrap()
            .write_all(&[0x5a; 32])
            .unwrap();
        let path = self.0.join("ulei.toml");
        fs::write(
            &path,
            format!(
                "[garden]\nstore = \"{}\"\nsocket = \"{}\"\nproducer_key_file = \"{}\"\n\
                 [inbox]\nsocket = \"{}\"\n",
                self.store().display(),
                self.0.join("run/garden.sock").display(),
                key.display(),
                self.0.join("inbox.sock").display(),
            ),
        )
        .unwrap();
        path
    }

    /// A file `name` holding `text`.
    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A running daemon, killed on drop.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(config: &Path) -> Daemon {
    let mut child = Command::new(GARDEN)
        .args(["serve", "--config"])
        .arg(config)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("ulei-garden ready /"), "{line:?}");
    Daemon(child)
}

/// The exit code and the one JSON object on stdout of `places import` with `args`.
fn import(args: &[&std::ffi::OsStr]) -> (i32, Value) {
    let out = Command::new(GARDEN)
        .args(["places", "import"])
        .args(args)
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    (
        out.status.code().unwrap(),
        serde_json::from_str(&stdout).unwrap(),
    )
}

fn count(s: &Store, sql: &str) -> i64 {
    s.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Rows of every table the import writes.
fn rows(store: &Path) -> Vec<i64> {
    let s = Store::open(store).unwrap();
    [
        "entities",
        "bed_entity",
        "bed_attr",
        "bed_attr_history",
        "bed_verdict",
        "bed_profile",
    ]
    .iter()
    .map(|t| count(&s, &format!("SELECT count(*) FROM {t}")))
    .collect()
}

/// The rows the fixture leaves in a fresh store `s`.
fn fixture_rows(s: &Store) {
    assert!(count(s, "SELECT count(*) FROM bed_entity") >= 14);
    assert_eq!(count(s, "SELECT count(*) FROM bed_attr"), 41);
    assert_eq!(
        count(
            s,
            "SELECT count(*) FROM bed_attr WHERE checked_at > 0 AND trim(source) <> ''"
        ),
        41
    );
    let place = |name: &str| -> String {
        s.conn()
            .query_row(
                "SELECT b.entity FROM bed_entity b JOIN entities e ON e.id = b.entity
                 WHERE e.name = ?1",
                [name],
                |r| r.get(0),
            )
            .unwrap()
    };
    let lantern = place("Lantern Noodle Bar");
    let (value, checked_at): (String, i64) = s
        .conn()
        .query_row(
            "SELECT value, checked_at FROM bed_attr WHERE entity = ?1 AND key = 'rating'",
            [&lantern],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    // 2031-03-08 and 2031-02-08 at 00:00 UTC.
    assert_eq!((value.as_str(), checked_at), ("4.5", 1_930_694_400));
    let old: (String, i64, String) = s
        .conn()
        .query_row(
            "SELECT value, checked_at, source FROM bed_attr_history
             WHERE entity = ?1 AND key = 'rating'",
            [&lantern],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(old, ("4.3".into(), 1_928_275_200, "fixture-maps".into()));
    // "17.03" before an `as_of` of 2031-03-20 is 2031-03-17.
    assert_eq!(
        count(
            s,
            &format!(
                "SELECT checked_at FROM bed_attr WHERE entity = '{lantern}' AND key = 'hours'"
            )
        ),
        1_931_472_000
    );
    let verdicts: Vec<(String, i64, String)> = s
        .conn()
        .prepare("SELECT entity, verdict, source_ref FROM bed_verdict ORDER BY verdict")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        verdicts,
        [
            (place("Saltmarsh Grill"), -1, "chat:msg-9002".into()),
            (lantern.clone(), 1, "chat:msg-9001".into()),
        ]
    );
    let status = |name: &str| -> String {
        s.conn()
            .query_row(
                "SELECT status FROM bed_entity WHERE entity = ?1",
                [place(name)],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(status("Old Pier Diner"), "gone");
    assert_eq!(status("Tidepool Kitchen"), "candidate");
    assert_eq!(status("Чайхана «Тёплый двор»"), "active");
    let profile: Vec<(String, String)> = s
        .conn()
        .prepare("SELECT key, value FROM bed_profile ORDER BY key, value")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let want: Vec<(String, String)> = [
        ("ban", "celery"),
        ("ban", "кинза"),
        ("budget_for_two", "40-60"),
        ("currency", "EUR"),
        ("home", "0.5000, -25.0000"),
        ("min_rating", "4.2"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
    .collect();
    assert_eq!(profile, want);
}

#[test]
fn the_fixture_imports_every_field_with_its_check_and_source_and_a_second_import_changes_nothing() {
    let scratch = Scratch::new();
    let config = scratch.config();
    let _daemon = start(&config);
    let file = Path::new(FIXTURE).as_os_str();
    let config = config.as_os_str();

    let (code, dry) = import(&[file, "--dry-run".as_ref()]);
    assert_eq!(code, 0, "{dry}");
    assert_eq!(
        dry,
        json!({
            "dry_run": true,
            "places": {"active": 10, "candidate": 4, "gone": 1},
            "attrs": 42, "unconfirmed": 2, "verdicts": 2, "profile": 6,
        })
    );
    assert_eq!(
        rows(&scratch.store()),
        [0, 0, 0, 0, 0, 0],
        "a dry run writes nothing"
    );

    let (code, first) = import(&[file, "--config".as_ref(), config]);
    assert_eq!(code, 0, "{first}");
    assert_eq!(
        first,
        json!({
            "dry_run": false,
            "places": {"created": 15, "adopted": 0, "found": 0},
            "attrs": {"written": 41, "replaced": 1, "history": 0, "unchanged": 0},
            "unconfirmed": 2,
            "verdicts": {"written": 2, "repeated": 0},
            "profile": {"written": 6, "unchanged": 0},
            "refused": [],
        })
    );
    fixture_rows(&Store::open(&scratch.store()).unwrap());
    let written = rows(&scratch.store());

    let (code, second) = import(&[file, "--config".as_ref(), config]);
    assert_eq!(code, 0, "{second}");
    assert_eq!(
        second,
        json!({
            "dry_run": false,
            "places": {"created": 0, "adopted": 0, "found": 15},
            "attrs": {"written": 0, "replaced": 0, "history": 0, "unchanged": 42},
            "unconfirmed": 2,
            "verdicts": {"written": 0, "repeated": 2},
            "profile": {"written": 0, "unchanged": 6},
            "refused": [],
        })
    );
    assert_eq!(
        rows(&scratch.store()),
        written,
        "a second import writes nothing"
    );
}

#[test]
fn a_file_with_two_bad_lines_reports_both_and_writes_nothing_with_and_without_dry_run() {
    let scratch = Scratch::new();
    let config = scratch.config();
    let _daemon = start(&config);
    let bad = scratch.file(
        "bad-places.md",
        "# Places\nas_of: 2031-03-20\n\n## Active\n### Lantern Noodle Bar\n\
         - cuisine: noodles [fixture-listing, 2031-02-08]\n\
         - rating: 4.5\n\
         - review_count: many [fixture-maps, 2031-03-08]\n",
    );
    let want = json!({
        "error": "refused",
        "lines": [
            {"line": 7, "rule": "no_bracket"},
            {"line": 8, "rule": "bad_value"},
        ],
    });
    for args in [
        vec![bad.as_os_str(), "--dry-run".as_ref()],
        vec![bad.as_os_str(), "--config".as_ref(), config.as_os_str()],
    ] {
        let (code, mut answer) = import(&args);
        assert_eq!(code, 1, "{answer}");
        for line in answer["lines"].as_array_mut().unwrap() {
            assert!(!line["message"].as_str().unwrap().is_empty(), "{line}");
            line.as_object_mut().unwrap().remove("message");
        }
        assert_eq!(answer, want);
        assert_eq!(rows(&scratch.store()), [0, 0, 0, 0, 0, 0]);
    }
}
