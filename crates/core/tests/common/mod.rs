//! Helpers shared by the `ulei-core` integration tests: owner-only scratch roots, hex and
//! packet signing, the golden fixture paths and the `serve` daemon.

// Each test crate compiles this module and uses only a subset of it.
#![allow(dead_code)]

pub(crate) mod daemon;

use std::fmt::Write;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use ulei_core::inbox::{EventRow, canonical};

/// Test key of `contracts/fixtures/README.md`, the key of the golden databases.
pub(crate) const GOLDEN_KEY_HEX: &str =
    "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

/// A fresh 0700 directory, removed on drop. `root()` is a path inside it that does not
/// exist yet.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    /// Under the temp dir, or under `/tmp` when the temp dir path leaves too little of the
    /// 107 bytes a socket path may have.
    pub(crate) fn new() -> Self {
        let temp = std::env::temp_dir();
        if temp.as_os_str().len() <= 48 {
            Self::under(&temp)
        } else {
            Self::under(Path::new("/tmp"))
        }
    }

    /// A new directory named by the pid, the start time of this test binary and a counter,
    /// so concurrent runs on one host never share it even when a pid is reused.
    pub(crate) fn under(base: &Path) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        static STARTED: OnceLock<u128> = OnceLock::new();
        let started = STARTED.get_or_init(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after the epoch")
                .as_nanos()
        });
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("ulei-it-{}-{started}-{n}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .expect("scratch dir");
        Self(dir)
    }

    pub(crate) fn root(&self) -> PathBuf {
        self.0.join("inbox")
    }

    pub(crate) fn socket(&self) -> PathBuf {
        self.0.join("s")
    }

    /// The root created 0700 with `producer.key` holding `key`.
    pub(crate) fn root_with_key(&self, key: &[u8]) -> PathBuf {
        let root = self.root();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .expect("root");
        write_mode(&root.join("producer.key"), key, 0o600);
        root
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn write_mode(path: &Path, bytes: &[u8], mode: u32) {
    fs::write(path, bytes).expect("write");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

pub(crate) fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        write!(out, "{b:02x}").expect("write to String");
        out
    })
}

/// The packet `body` with `mac` computed over its canonical JSON under `key`.
pub(crate) fn sign(key: &[u8], body: &Value) -> Value {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(&canonical(body).expect("canonical"));
    let tag = hex(&mac.finalize().into_bytes());
    let mut packet = body.clone();
    packet["mac"] = Value::String(tag);
    packet
}

/// A file of `contracts/fixtures/c1/golden`.
pub(crate) fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/c1/golden")
        .join(name)
}

/// The `rows` of a golden `rows.*.json`.
pub(crate) fn golden_rows(name: &str) -> Vec<Value> {
    let rows: Value =
        serde_json::from_slice(&fs::read(golden(name)).expect("rows file")).expect("json");
    rows["rows"].as_array().expect("rows").clone()
}

/// The column values of `row` as the public API returns them, a blob as lowercase hex.
pub(crate) fn api_values(row: &EventRow) -> Value {
    json!({
        "id": row.id,
        "packet": hex(&row.packet),
        "digest": row.digest,
        "status": row.status,
        "attempts": row.attempts,
        "lease": row.lease,
        "lease_until": row.lease_until,
        "available": row.available,
        "session_id": row.session_id,
        "outcome": row.outcome,
        "created": row.created,
    })
}

/// The column values of a golden row, without their SQLite types.
pub(crate) fn golden_values(row: &Value) -> Value {
    let values = row
        .as_object()
        .expect("row")
        .iter()
        .map(|(column, cell)| (column.clone(), cell["value"].clone()))
        .collect();
    Value::Object(values)
}
