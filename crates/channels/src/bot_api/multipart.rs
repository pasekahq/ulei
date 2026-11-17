//! `multipart/form-data` body that streams one file between small text parts.

use std::fs::File;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Cursor, Read};
use std::time::Instant;

/// A form with text fields and one file, readable once, of a known length.
pub(crate) struct Form {
    boundary: String,
    head: Vec<u8>,
    file: File,
    file_len: u64,
    tail: Vec<u8>,
}

impl Form {
    /// Builds the form; the boundary is random and checked against the field texts.
    pub(crate) fn new(
        fields: &[(&str, String)],
        file_field: &str,
        file_name: &str,
        file: File,
    ) -> io::Result<Self> {
        let file_len = file.metadata()?.len();
        let state = RandomState::new();
        let mut seed = Instant::now();
        let boundary = loop {
            let candidate = format!("ulei-{:016x}", state.hash_one(seed));
            let clash = fields.iter().any(|(_, value)| value.contains(&candidate))
                || file_name.contains(&candidate);
            if !clash {
                break candidate;
            }
            seed = Instant::now();
        };
        let mut head = Vec::new();
        for (name, value) in fields {
            head.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        }
        let quoted = file_name.replace(['"', '\r', '\n'], "_");
        head.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; \
                 filename=\"{quoted}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        let tail = format!("\r\n--{boundary}--\r\n").into_bytes();
        Ok(Self {
            boundary,
            head,
            file,
            file_len,
            tail,
        })
    }

    /// Value of the `Content-Type` header.
    pub(crate) fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    /// Exact body length in bytes.
    pub(crate) fn len(&self) -> u64 {
        self.head.len() as u64 + self.file_len + self.tail.len() as u64
    }

    /// The body; the file part is capped at the length measured at build time.
    pub(crate) fn into_reader(self) -> impl Read {
        Cursor::new(self.head)
            .chain(self.file.take(self.file_len))
            .chain(Cursor::new(self.tail))
    }
}
