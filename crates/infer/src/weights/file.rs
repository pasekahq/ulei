//! The engine's own weight file (ADR-0086 section 2): one file per model, a header and 64-byte
//! aligned tensor blobs, written once by the converter and opened read-only.
//!
//! Layout, all integers little-endian:
//!
//! | Bytes | Content |
//! |---|---|
//! | 0..8 | magic `ULEIINFW` |
//! | 8..12 | `u32` format version, [`VERSION`] |
//! | 12..16 | `u32` 0, reserved |
//! | 16..24 | `u64` length `n` of the JSON header |
//! | 24..24+n | JSON header (UTF-8) |
//! | to the next multiple of 64 | zero bytes; the data section starts here |
//! | data section | the tensors' blobs at the offsets of the header, zero bytes between |
//!
//! The JSON header is `{"meta": {...}, "tensors": [{"name", "dtype", "shape", "offset"}, ...]}`.
//! `meta` belongs to the model that wrote the file (its config, pooling, provenance); this
//! module does not read it. `dtype` is `f32`, `i8` or `u32`; `offset` is relative to the data
//! section and a multiple of 64, so every blob is 64-byte aligned in the file; offsets ascend
//! in table order and the file ends with the last blob. The writer is deterministic: equal
//! input gives equal bytes.
//!
//! E3 reads every blob into owned buffers; E4 replaces them by a read-only `mmap` of the file
//! behind the same accessors.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;

use serde_json::{Value, json};

use super::View;
use crate::{Error, Result};

/// The first eight bytes of a weight file.
pub const MAGIC: [u8; 8] = *b"ULEIINFW";

/// The format version this module writes and reads.
pub const VERSION: u32 = 1;

/// Alignment of the data section and of every blob in it.
pub const ALIGN: u64 = 64;

/// Bytes before the JSON header.
const PREAMBLE: u64 = 24;

/// Largest JSON header accepted.
const MAX_HEADER: u64 = 16 * 1024 * 1024;

/// Bytes read per step while loading a blob.
const CHUNK: usize = 1 << 20;

/// Tensor data to write, borrowed.
#[derive(Debug, Clone, Copy)]
pub enum Data<'a> {
    /// fp32 values.
    F32(&'a [f32]),
    /// int8 values.
    I8(&'a [i8]),
    /// `u32` values (index maps).
    U32(&'a [u32]),
}

impl Data<'_> {
    fn dtype(&self) -> &'static str {
        match self {
            Self::F32(_) => "f32",
            Self::I8(_) => "i8",
            Self::U32(_) => "u32",
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::F32(d) => d.len(),
            Self::I8(d) => d.len(),
            Self::U32(d) => d.len(),
        }
    }

    fn bytes(&self) -> u64 {
        (self.len() * width(self.dtype())) as u64
    }

    fn write_to<W: Write>(&self, w: &mut W) -> std::io::Result<()> {
        // Encoded in steps of `CHUNK` values, so a large table needs no second full copy.
        let mut buf = Vec::with_capacity(CHUNK * 4);
        match self {
            Self::F32(d) => {
                for part in d.chunks(CHUNK) {
                    buf.clear();
                    buf.extend(part.iter().flat_map(|v| v.to_le_bytes()));
                    w.write_all(&buf)?;
                }
            }
            Self::I8(d) => {
                for part in d.chunks(CHUNK) {
                    buf.clear();
                    buf.extend(part.iter().map(|v| v.to_le_bytes()[0]));
                    w.write_all(&buf)?;
                }
            }
            Self::U32(d) => {
                for part in d.chunks(CHUNK) {
                    buf.clear();
                    buf.extend(part.iter().flat_map(|v| v.to_le_bytes()));
                    w.write_all(&buf)?;
                }
            }
        }
        Ok(())
    }
}

/// Bytes per element of `dtype`.
fn width(dtype: &str) -> usize {
    match dtype {
        "i8" => 1,
        _ => 4,
    }
}

/// One tensor to write.
#[derive(Debug, Clone)]
pub struct Entry<'a> {
    /// Unique name.
    pub name: String,
    /// Dimensions, outermost first; their product is the data's length.
    pub shape: Vec<usize>,
    /// The values, row-major.
    pub data: Data<'a>,
}

fn align(n: u64) -> u64 {
    n.div_ceil(ALIGN) * ALIGN
}

/// Writes a weight file with `meta` and `tensors` (in this order) to `w`; returns the bytes
/// written.
///
/// # Errors
/// [`Error::Weights`] for a duplicate name or a shape that disagrees with its data,
/// [`Error::Io`] if writing fails.
pub fn write<W: Write>(mut w: W, meta: &Value, tensors: &[Entry<'_>]) -> Result<u64> {
    let mut table = Vec::with_capacity(tensors.len());
    let mut seen = std::collections::BTreeSet::new();
    let mut offset = 0u64;
    for t in tensors {
        if !seen.insert(t.name.as_str()) {
            return Err(Error::Weights(format!("duplicate tensor `{}`", t.name)));
        }
        if t.shape.iter().product::<usize>() != t.data.len() {
            return Err(Error::Weights(format!(
                "tensor `{}`: {} values for shape {:?}",
                t.name,
                t.data.len(),
                t.shape
            )));
        }
        table.push(json!({
            "name": t.name,
            "dtype": t.data.dtype(),
            "shape": t.shape,
            "offset": offset,
        }));
        offset = align(offset + t.data.bytes());
    }
    let header = serde_json::to_vec(&json!({"meta": meta, "tensors": table}))
        .map_err(|e| Error::Weights(format!("header: {e}")))?;
    let n = header.len() as u64;
    let data_start = align(PREAMBLE + n);
    w.write_all(&MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?;
    w.write_all(&n.to_le_bytes())?;
    w.write_all(&header)?;
    write_zeros(&mut w, data_start - PREAMBLE - n)?;
    let mut pos = 0u64;
    for t in tensors {
        let begin = align(pos);
        write_zeros(&mut w, begin - pos)?;
        t.data.write_to(&mut w)?;
        pos = begin + t.data.bytes();
    }
    w.flush()?;
    Ok(data_start + pos)
}

fn write_zeros<W: Write>(w: &mut W, n: u64) -> std::io::Result<()> {
    std::io::copy(&mut std::io::repeat(0).take(n), w).map(|_| ())
}

/// Owned tensor data read from a file.
#[derive(Debug, Clone, PartialEq)]
enum Owned {
    F32(Vec<f32>),
    I8(Vec<i8>),
    U32(Vec<u32>),
}

impl Owned {
    fn dtype(&self) -> &'static str {
        match self {
            Self::F32(_) => "f32",
            Self::I8(_) => "i8",
            Self::U32(_) => "u32",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Stored {
    shape: Vec<usize>,
    data: Owned,
}

/// A weight file read into memory.
#[derive(Debug)]
pub struct WeightFile {
    meta: Value,
    tensors: BTreeMap<String, Stored>,
}

struct Slot {
    name: String,
    dtype: String,
    shape: Vec<usize>,
    offset: u64,
    count: usize,
}

impl WeightFile {
    /// Reads the weight file at `path`.
    ///
    /// # Errors
    /// [`Error::Io`] if it cannot be read, [`Error::Weights`] if it is malformed.
    pub fn open(path: &Path) -> Result<Self> {
        Self::read(BufReader::with_capacity(CHUNK, File::open(path)?))
    }

    /// Reads a weight file from its first byte to its end.
    ///
    /// # Errors
    /// [`Error::Io`] on a read failure or a truncated stream, [`Error::Weights`] for a wrong
    /// magic or version, a malformed header, misaligned, descending or overlapping blobs, or
    /// bytes after the last blob.
    pub fn read<R: Read>(mut r: R) -> Result<Self> {
        let mut pre = [0u8; 24];
        r.read_exact(&mut pre)?;
        if pre[..8] != MAGIC {
            return Err(Error::Weights("not a weight file (bad magic)".into()));
        }
        let word = |at: usize| u32::from_le_bytes([pre[at], pre[at + 1], pre[at + 2], pre[at + 3]]);
        if word(8) != VERSION {
            return Err(Error::Weights(format!(
                "format version {}, expected {VERSION}",
                word(8)
            )));
        }
        let mut n = [0u8; 8];
        n.copy_from_slice(&pre[16..24]);
        let n = u64::from_le_bytes(n);
        if n > MAX_HEADER {
            return Err(Error::Weights(format!("header of {n} bytes is too large")));
        }
        let mut header = Vec::new();
        r.by_ref().take(n).read_to_end(&mut header)?;
        if header.len() as u64 != n {
            return Err(Error::Weights("truncated header".into()));
        }
        let (meta, slots) = parse_header(&header)?;
        skip(&mut r, align(PREAMBLE + n) - PREAMBLE - n)?;
        let mut tensors = BTreeMap::new();
        let mut pos = 0u64;
        for s in slots {
            if s.offset < pos || !s.offset.is_multiple_of(ALIGN) {
                return Err(Error::Weights(format!(
                    "tensor `{}`: offset {} is misaligned or overlaps",
                    s.name, s.offset
                )));
            }
            skip(&mut r, s.offset - pos)?;
            let data = read_data(&mut r, &s.dtype, s.count)?;
            pos = s.offset + (s.count * width(&s.dtype)) as u64;
            let stored = Stored {
                shape: s.shape,
                data,
            };
            if tensors.insert(s.name.clone(), stored).is_some() {
                return Err(Error::Weights(format!("duplicate tensor `{}`", s.name)));
            }
        }
        if r.read(&mut [0u8; 1])? != 0 {
            return Err(Error::Weights("bytes after the last tensor".into()));
        }
        Ok(Self { meta, tensors })
    }

    /// The `meta` object of the header, as the writer put it.
    pub fn meta(&self) -> &Value {
        &self.meta
    }

    /// Names of every tensor, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.tensors.keys().map(String::as_str).collect()
    }

    fn stored(&self, name: &str, dtype: &str) -> Result<&Stored> {
        let s = self
            .tensors
            .get(name)
            .ok_or_else(|| Error::Weights(format!("no tensor `{name}`")))?;
        if s.data.dtype() == dtype {
            Ok(s)
        } else {
            Err(Error::Weights(format!(
                "tensor `{name}` is {}, not {dtype}",
                s.data.dtype()
            )))
        }
    }

    /// The fp32 tensor `name`.
    ///
    /// # Errors
    /// [`Error::Weights`] if there is no such tensor or it is not `f32`.
    pub fn f32(&self, name: &str) -> Result<View<'_, f32>> {
        let s = self.stored(name, "f32")?;
        match &s.data {
            Owned::F32(d) => Ok(View {
                shape: &s.shape,
                data: d,
            }),
            _ => unreachable!("dtype checked"),
        }
    }

    /// The int8 tensor `name`.
    ///
    /// # Errors
    /// [`Error::Weights`] if there is no such tensor or it is not `i8`.
    pub fn i8(&self, name: &str) -> Result<View<'_, i8>> {
        let s = self.stored(name, "i8")?;
        match &s.data {
            Owned::I8(d) => Ok(View {
                shape: &s.shape,
                data: d,
            }),
            _ => unreachable!("dtype checked"),
        }
    }

    /// The `u32` tensor `name`.
    ///
    /// # Errors
    /// [`Error::Weights`] if there is no such tensor or it is not `u32`.
    pub fn u32(&self, name: &str) -> Result<View<'_, u32>> {
        let s = self.stored(name, "u32")?;
        match &s.data {
            Owned::U32(d) => Ok(View {
                shape: &s.shape,
                data: d,
            }),
            _ => unreachable!("dtype checked"),
        }
    }
}

fn parse_header(header: &[u8]) -> Result<(Value, Vec<Slot>)> {
    let mut v: Value = serde_json::from_slice(header)
        .map_err(|e| Error::Weights(format!("header is not JSON: {e}")))?;
    let meta = v
        .get_mut("meta")
        .map(Value::take)
        .ok_or_else(|| Error::Weights("header has no `meta`".into()))?;
    let table = v
        .get("tensors")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Weights("header has no `tensors` array".into()))?;
    let mut slots = Vec::with_capacity(table.len());
    for (i, t) in table.iter().enumerate() {
        let bad = |what: &str| Error::Weights(format!("tensor #{i}: {what}"));
        let name = t
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("missing name"))?;
        let dtype = t
            .get("dtype")
            .and_then(Value::as_str)
            .filter(|d| matches!(*d, "f32" | "i8" | "u32"))
            .ok_or_else(|| bad("dtype is not f32, i8 or u32"))?;
        let shape = t
            .get("shape")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("missing shape"))?
            .iter()
            .map(|d| d.as_u64().and_then(|d| usize::try_from(d).ok()))
            .collect::<Option<Vec<usize>>>()
            .ok_or_else(|| bad("bad shape"))?;
        let count = shape
            .iter()
            .try_fold(1usize, |a, &d| a.checked_mul(d))
            .ok_or_else(|| bad("shape overflows"))?;
        let offset = t
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| bad("missing offset"))?;
        slots.push(Slot {
            name: name.to_string(),
            dtype: dtype.to_string(),
            shape,
            offset,
            count,
        });
    }
    Ok((meta, slots))
}

fn read_data<R: Read>(r: &mut R, dtype: &str, count: usize) -> Result<Owned> {
    let mut left = count
        .checked_mul(width(dtype))
        .ok_or_else(|| Error::Weights("tensor too large".into()))?;
    let mut buf = vec![0u8; CHUNK.min(left)];
    let mut out = match dtype {
        "f32" => Owned::F32(Vec::with_capacity(count)),
        "i8" => Owned::I8(Vec::with_capacity(count)),
        _ => Owned::U32(Vec::with_capacity(count)),
    };
    while left > 0 {
        let chunk = &mut buf[..left.min(CHUNK)];
        r.read_exact(chunk)?;
        match &mut out {
            Owned::F32(v) => v.extend(
                chunk
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b)),
            ),
            Owned::U32(v) => v.extend(
                chunk
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_le_bytes(*b)),
            ),
            Owned::I8(v) => v.extend(chunk.iter().map(|b| i8::from_le_bytes([*b]))),
        }
        left -= chunk.len();
    }
    Ok(out)
}

fn skip<R: Read>(r: &mut R, n: u64) -> Result<()> {
    let skipped = std::io::copy(&mut r.by_ref().take(n), &mut std::io::sink())?;
    if skipped == n {
        Ok(())
    } else {
        Err(Error::Weights("truncated file".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Value, Vec<f32>, Vec<i8>, Vec<u32>) {
        (
            json!({"model": "tiny", "block": 64}),
            vec![1.5, -0.0, f32::MIN_POSITIVE, 3.0e38, -7.25, 0.1],
            vec![-128, -127, 0, 1, 127, 5, -3],
            vec![2, 0, 1, u32::MAX],
        )
    }

    fn sample_file() -> Vec<u8> {
        let (meta, f, i, u) = sample();
        let tensors = [
            Entry {
                name: "w".into(),
                shape: vec![7],
                data: Data::I8(&i),
            },
            Entry {
                name: "a".into(),
                shape: vec![2, 3],
                data: Data::F32(&f),
            },
            Entry {
                name: "map".into(),
                shape: vec![4],
                data: Data::U32(&u),
            },
        ];
        let mut out = Vec::new();
        let n = write(&mut out, &meta, &tensors).unwrap();
        assert_eq!(n, out.len() as u64);
        out
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn round_trip_is_exact_and_aligned() {
        let bytes = sample_file();
        let (meta, f, i, u) = sample();
        let file = WeightFile::read(bytes.as_slice()).unwrap();
        assert_eq!(file.meta(), &meta);
        assert_eq!(file.names(), vec!["a", "map", "w"]);
        let a = file.f32("a").unwrap();
        assert_eq!(a.shape, &[2, 3]);
        assert_eq!(
            a.data.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            f.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
        assert_eq!(file.i8("w").unwrap().data, i.as_slice());
        assert_eq!(file.u32("map").unwrap().data, u.as_slice());
        // Wrong dtype and missing names are errors.
        assert!(file.f32("w").is_err());
        assert!(file.i8("nope").is_err());
        // Every blob starts at a multiple of 64 in the file.
        let n = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let start = align(PREAMBLE + n);
        let header: Value =
            serde_json::from_slice(&bytes[24..24 + usize::try_from(n).unwrap()]).unwrap();
        for t in header["tensors"].as_array().unwrap() {
            assert_eq!((start + t["offset"].as_u64().unwrap()) % ALIGN, 0);
        }
        // The int8 blob is the first one: its bytes sit at the data section's start.
        let s = usize::try_from(start).unwrap();
        assert_eq!(
            bytes[s..s + 7],
            i.iter().map(|v| v.to_le_bytes()[0]).collect::<Vec<_>>()
        );
        // The writer is deterministic.
        assert_eq!(sample_file(), bytes);
    }

    #[test]
    fn malformed_files_are_refused() {
        let good = sample_file();
        assert!(WeightFile::read(&good[..good.len() - 1]).is_err());
        let mut longer = good.clone();
        longer.push(0);
        assert!(WeightFile::read(longer.as_slice()).is_err());
        let mut magic = good.clone();
        magic[0] = b'X';
        assert!(WeightFile::read(magic.as_slice()).is_err());
        let mut version = good.clone();
        version[8] = 2;
        assert!(WeightFile::read(version.as_slice()).is_err());
        // A shape that disagrees with the data is refused by the writer.
        let bad = [Entry {
            name: "x".into(),
            shape: vec![3],
            data: Data::F32(&[1.0, 2.0]),
        }];
        assert!(write(Vec::new(), &json!({}), &bad).is_err());
        let dup = [
            Entry {
                name: "x".into(),
                shape: vec![1],
                data: Data::F32(&[1.0]),
            },
            Entry {
                name: "x".into(),
                shape: vec![1],
                data: Data::I8(&[1]),
            },
        ];
        assert!(write(Vec::new(), &json!({}), &dup).is_err());
    }
}
