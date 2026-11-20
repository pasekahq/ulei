//! `ulei_contracts::env_file` on synthetic files.

#![allow(missing_docs, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use ulei_contracts::env_file;

/// A file under a directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn with(bytes: &[u8]) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ulei-env-file-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("synthetic.env"), bytes).unwrap();
        Self(dir)
    }

    fn read(&self) -> Vec<(String, String)> {
        env_file::read(&self.0.join("synthetic.env")).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn assignments_come_in_file_order_with_comments_blanks_and_bare_words_skipped() {
    let file =
        Scratch::with(b"# synthetic\n\nA=1\nnot an assignment\n  # indented\nB = two \nA=3\n");
    assert_eq!(file.read(), pairs(&[("A", "1"), ("B", "two"), ("A", "3")]));
}

#[test]
fn a_leading_bom_and_export_are_dropped() {
    let file = Scratch::with("\u{feff}export KEY=value\nexport  SPACED=x\n".as_bytes());
    assert_eq!(file.read(), pairs(&[("KEY", "value"), ("SPACED", "x")]));
}

#[test]
fn quotes_are_removed_and_double_quotes_unescape_quote_and_backslash() {
    let file =
        Scratch::with(b"D=\"a \\\"b\\\" \\\\ \\n\"\nS='a \\\"b'\nU=a\"b\"\nH=\"\nE=\nM=a=b\n");
    assert_eq!(
        file.read(),
        pairs(&[
            ("D", "a \"b\" \\ \\n"),
            ("S", "a \\\"b"),
            ("U", "a\"b\""),
            ("H", "\""),
            ("E", ""),
            ("M", "a=b"),
        ])
    );
}

#[test]
fn names_and_values_are_stripped_of_python_whitespace() {
    let file = Scratch::with("\u{1c} K \u{a0}= \u{1f}v\u{2003}\n".as_bytes());
    assert_eq!(file.read(), pairs(&[("K", "v")]));
}

#[test]
fn invalid_utf8_is_read_lossily() {
    let file = Scratch::with(b"K=a\xffb\n");
    assert_eq!(file.read(), pairs(&[("K", "a\u{fffd}b")]));
}

#[test]
fn a_missing_file_is_the_read_error() {
    let file = Scratch::with(b"");
    let err = env_file::read(&file.0.join("absent.env")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}
