//! `multipart/form-data` body that streams its files between small text parts.

use std::fs::File;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Cursor, Read};
use std::time::Instant;

/// One file of a form: its field, the file name and type it is announced with, the open file.
pub(crate) struct FilePart {
    pub(crate) field: String,
    pub(crate) name: String,
    pub(crate) file_type: &'static str,
    pub(crate) file: File,
}

/// A form with text fields and its files, readable once, of a known length.
pub(crate) struct Form {
    boundary: String,
    head: Vec<u8>,
    /// Each file with the part head before it and its length at build time.
    files: Vec<(Vec<u8>, File, u64)>,
    tail: Vec<u8>,
}

impl Form {
    /// Builds the form; the boundary is random and checked against the field texts and names.
    pub(crate) fn new(fields: &[(&str, String)], files: Vec<FilePart>) -> io::Result<Self> {
        let state = RandomState::new();
        let mut seed = Instant::now();
        let boundary = loop {
            let candidate = format!("ulei-{:016x}", state.hash_one(seed));
            let clash = fields.iter().any(|(_, value)| value.contains(&candidate))
                || files.iter().any(|part| part.name.contains(&candidate));
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
        let mut parts = Vec::with_capacity(files.len());
        for (n, part) in files.into_iter().enumerate() {
            let len = part.file.metadata()?.len();
            let quoted = part.name.replace(['"', '\r', '\n'], "_");
            let lead = if n == 0 { "" } else { "\r\n" };
            let part_head = format!(
                "{lead}--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; \
                 filename=\"{quoted}\"\r\nContent-Type: {kind}\r\n\r\n",
                field = part.field,
                kind = part.file_type,
            );
            parts.push((part_head.into_bytes(), part.file, len));
        }
        let tail = format!("\r\n--{boundary}--\r\n").into_bytes();
        Ok(Self {
            boundary,
            head,
            files: parts,
            tail,
        })
    }

    /// Value of the `Content-Type` header.
    pub(crate) fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    /// Exact body length in bytes.
    pub(crate) fn len(&self) -> u64 {
        let files: u64 = self
            .files
            .iter()
            .map(|(head, _, len)| head.len() as u64 + len)
            .sum();
        self.head.len() as u64 + files + self.tail.len() as u64
    }

    /// The body; each file part is capped at the length measured at build time.
    pub(crate) fn into_reader(self) -> impl Read {
        let mut body: Box<dyn Read + Send> = Box::new(Cursor::new(self.head));
        for (head, file, len) in self.files {
            body = Box::new(body.chain(Cursor::new(head)).chain(file.take(len)));
        }
        body.chain(Cursor::new(self.tail))
    }
}
