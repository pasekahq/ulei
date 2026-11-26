//! The content digest of the encoder engine's weight file (ADR-0087 E6c): the SHA-256 of the
//! whole file with the digest's own 32 bytes read as zeros (`ulei_infer::weights::file`'s
//! module docs). `ulei-garden-embed convert` hashes the bytes as it writes them and seals the
//! digest into the preamble ([`Hashing`], [`seal`]); the helper takes the sealed digest as the
//! model's identity at start without reading the blobs; `ulei-garden-embed verify` hashes the
//! whole file again and compares ([`verify`]).

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use sha2::{Digest, Sha256};
use ulei_infer::weights::file::{self, WeightFile};

use crate::miss::lower_hex;

/// A writer that hashes every byte it passes on: over the output of
/// `ulei_infer::weights::file::write`, whose digest bytes are zero, its hash is the content
/// digest.
pub struct Hashing<W> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> Hashing<W> {
    /// Hashes what goes to `inner`.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    /// The SHA-256 of everything written, and the inner writer.
    pub fn finish(self) -> ([u8; 32], W) {
        (self.hasher.finalize().into(), self.inner)
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Seals `digest` into the weight file `file` (written with the digest's bytes zero) and
/// flushes it to the disk.
///
/// # Errors
///
/// Seeking, writing or syncing fails.
pub fn seal(file: &mut File, digest: &[u8; 32]) -> io::Result<()> {
    file::seal(file, digest)?;
    file.sync_all()
}

/// The content digest of the file behind `file`, hashed from its first byte to its end
/// whatever its offset, and the bytes read.
///
/// # Errors
///
/// Reading fails.
pub fn content_sha256(file: &File) -> io::Result<([u8; 32], u64)> {
    let mut hasher = Sha256::new();
    let n = file::canonical(ReadAt(file, 0), |c| hasher.update(c))?;
    Ok((hasher.finalize().into(), n))
}

/// Reads `.0` by position from `.1` on, so the file's own offset does not matter.
struct ReadAt<'a>(&'a File, u64);

impl io::Read for ReadAt<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = std::os::unix::fs::FileExt::read_at(self.0, buf, self.1)?;
        self.1 += n as u64;
        Ok(n)
    }
}

/// A weight file whose content digest matches its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The content digest, lowercase hex: the helper's identity for this file.
    pub sha256: String,
    /// The file's length.
    pub bytes: u64,
    /// `meta.format` of its header, empty if it names none.
    pub format: String,
}

/// Why [`verify`] did not pass a weight file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// The file cannot be opened or read.
    Io(String),
    /// A weight file of a format version this build does not read: convert the model again.
    Format(String),
    /// The bytes are not those the converter wrote: the model file is corrupted.
    Corrupt(String),
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(m) | Self::Format(m) => f.write_str(m),
            Self::Corrupt(m) => write!(f, "model file corrupted: {m}"),
        }
    }
}

/// Opens the weight file at `path` and checks it the way the helper does at start (preamble,
/// length, header, tensor table, a sealed digest), returning the sealed digest.
///
/// # Errors
///
/// [`VerifyError`] as for [`verify`], without the digest comparison.
pub fn open_sealed(path: &Path) -> Result<(File, WeightFile, [u8; 32]), VerifyError> {
    let file = File::open(path).map_err(|e| VerifyError::Io(e.to_string()))?;
    let weights = WeightFile::from_file(&file).map_err(|e| match e {
        ulei_infer::Error::Corrupt(m) => VerifyError::Corrupt(m),
        ulei_infer::Error::Io(e) => VerifyError::Io(e.to_string()),
        other => VerifyError::Format(other.to_string()),
    })?;
    let digest = *weights.digest().ok_or_else(|| {
        VerifyError::Corrupt(
            "no content digest in the preamble (a conversion that did not finish)".into(),
        )
    })?;
    Ok((file, weights, digest))
}

/// Verifies the weight file at `path`: the checks of the helper's start, then the SHA-256 of the
/// whole file against the digest sealed in its preamble.
///
/// # Errors
///
/// [`VerifyError::Io`] if it cannot be read, [`VerifyError::Format`] for a file of an older
/// format version, [`VerifyError::Corrupt`] for a file that is truncated, too long, malformed,
/// unsealed, or whose bytes do not hash to its digest (both digests in the message).
pub fn verify(path: &Path) -> Result<Verified, VerifyError> {
    let (file, weights, sealed) = open_sealed(path)?;
    let (actual, bytes) = content_sha256(&file).map_err(|e| VerifyError::Io(e.to_string()))?;
    let (sealed, actual) = (lower_hex(&sealed), lower_hex(&actual));
    if sealed != actual {
        return Err(VerifyError::Corrupt(format!(
            "content SHA-256 is {actual}, the preamble says {sealed}"
        )));
    }
    Ok(Verified {
        sha256: sealed,
        bytes,
        format: weights
            .meta()
            .get("format")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}
