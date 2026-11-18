//! Helpers shared by the `ulei-runtime-adapters` integration tests: an owner-only scratch
//! directory for sockets and the framing of a JSON answer.

// Each test crate compiles this module and uses only a subset of it.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A fresh 0700 directory for sockets, removed on drop.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let temp = std::env::temp_dir();
        // A socket path holds at most 107 bytes.
        let base = if temp.as_os_str().len() <= 48 {
            temp
        } else {
            PathBuf::from("/tmp")
        };
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("ulei-c6-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .expect("scratch dir");
        Self(dir)
    }

    /// `name` inside the directory.
    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// The C6 socket path inside the directory.
    pub(crate) fn socket(&self) -> PathBuf {
        self.path("c6.sock")
    }

    /// The uid that owns the directory.
    pub(crate) fn uid(&self) -> u32 {
        fs::metadata(&self.0).expect("scratch metadata").uid()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An HTTP/1.1 answer with `status` and the JSON `body`, closing the connection.
pub(crate) fn response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}
