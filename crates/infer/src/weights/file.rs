//! The engine's own weight file (ADR-0087 section 2): one file per model, a header and 64-byte
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
//! `meta` belongs to the model that wrote the file (its config, pooling, tokenizer, provenance);
//! this module does not read it. `dtype` is `f32`, `f64`, `i8`, `u8` or `u32`; `offset` is
//! relative to the data
//! section and a multiple of 64, so every blob is 64-byte aligned in the file; offsets ascend
//! in table order and the file ends with the last blob. The writer is deterministic: equal
//! input gives equal bytes.
//!
//! [`WeightFile::open`] maps the file private and read-only (E4): the accessors return slices
//! into the mapping, checked once at open for range and alignment, so nothing is copied and
//! only touched pages become resident. Every malformed or truncated file is an error, never a
//! panic or an access out of bounds. The `unsafe` of the mapping and of the in-place view is
//! confined to the private module `map` (ADR-0087 section 5).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use serde_json::{Value, json};

use super::View;
use super::map::{Mapping, Plain, cast};
use crate::{Error, Result};

/// The first eight bytes of a weight file.
pub const MAGIC: [u8; 8] = *b"ULEIINFW";

/// The format version this module writes and reads. Version 2 (ADR-0087 E2) added the `u8` and
/// `f64` element types, which the tokenizer section of a model file needs; a file of version 1
/// is refused with a message saying to convert the model again.
pub const VERSION: u32 = 2;

/// Alignment of the data section and of every blob in it.
pub const ALIGN: u64 = 64;

/// Bytes before the JSON header.
const PREAMBLE: u64 = 24;

/// Largest JSON header accepted.
const MAX_HEADER: u64 = 16 * 1024 * 1024;

/// Values encoded per step while writing a blob.
const CHUNK: usize = 1 << 20;

/// Tensor data to write, borrowed.
#[derive(Debug, Clone, Copy)]
pub enum Data<'a> {
    /// fp32 values.
    F32(&'a [f32]),
    /// fp64 values (the tokenizer's piece scores).
    F64(&'a [f64]),
    /// int8 values.
    I8(&'a [i8]),
    /// Bytes (strings, opaque blobs).
    U8(&'a [u8]),
    /// `u32` values (index maps).
    U32(&'a [u32]),
}

impl Data<'_> {
    fn dtype(&self) -> &'static str {
        match self {
            Self::F32(_) => "f32",
            Self::F64(_) => "f64",
            Self::I8(_) => "i8",
            Self::U8(_) => "u8",
            Self::U32(_) => "u32",
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::F32(d) => d.len(),
            Self::F64(d) => d.len(),
            Self::I8(d) => d.len(),
            Self::U8(d) => d.len(),
            Self::U32(d) => d.len(),
        }
    }

    fn bytes(&self) -> u64 {
        (self.len() * width(self.dtype())) as u64
    }

    fn write_to<W: Write>(&self, w: &mut W) -> std::io::Result<()> {
        // Encoded in steps of `CHUNK` values, so a large table needs no second full copy.
        let mut buf = Vec::with_capacity(CHUNK * 8);
        match self {
            Self::F32(d) => {
                for part in d.chunks(CHUNK) {
                    buf.clear();
                    buf.extend(part.iter().flat_map(|v| v.to_le_bytes()));
                    w.write_all(&buf)?;
                }
            }
            Self::F64(d) => {
                for part in d.chunks(CHUNK) {
                    buf.clear();
                    buf.extend(part.iter().flat_map(|v| v.to_le_bytes()));
                    w.write_all(&buf)?;
                }
            }
            Self::U8(d) => w.write_all(d)?,
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
        "i8" | "u8" => 1,
        "f64" => 8,
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

/// Element type of a stored tensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dtype {
    F32,
    F64,
    I8,
    U8,
    U32,
}

impl Dtype {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "f32" => Some(Self::F32),
            "f64" => Some(Self::F64),
            "i8" => Some(Self::I8),
            "u8" => Some(Self::U8),
            "u32" => Some(Self::U32),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::I8 => "i8",
            Self::U8 => "u8",
            Self::U32 => "u32",
        }
    }

    fn width(self) -> usize {
        width(self.name())
    }
}

/// Where a tensor lies in the mapping, checked at open.
#[derive(Debug, Clone, PartialEq)]
struct Stored {
    dtype: Dtype,
    shape: Vec<usize>,
    /// Byte range in the whole file.
    start: usize,
    end: usize,
}

/// A weight file mapped read-only (ADR-0087 section 2): the tensors are read in place, so only
/// the pages a forward pass touches become resident, as clean pages of the page cache that the
/// kernel may drop and that every process mapping the same file shares.
#[derive(Debug)]
pub struct WeightFile {
    meta: Value,
    tensors: BTreeMap<String, Stored>,
    map: Mapping,
}

struct Slot {
    name: String,
    dtype: Dtype,
    shape: Vec<usize>,
    offset: u64,
    count: usize,
}

impl WeightFile {
    /// Maps the weight file at `path` read-only and checks its layout. The file must not be
    /// written or truncated while the returned value lives (the converter replaces a file by
    /// renaming a new one over it, which is safe).
    ///
    /// # Errors
    /// [`Error::Io`] if it cannot be opened or mapped, [`Error::Weights`] if it is malformed
    /// (as [`WeightFile::read`]).
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_file(&File::open(path)?)
    }

    /// As [`WeightFile::open`], for a file the caller has opened: the mapping is of the file
    /// behind `file`, so a caller that also reads `file` (to hash it) reads the same bytes even
    /// if the path is renamed over meanwhile. `file`'s offset is not moved.
    ///
    /// # Errors
    /// As [`WeightFile::open`].
    pub fn from_file(file: &File) -> Result<Self> {
        let len = file.metadata()?.len();
        if len < PREAMBLE {
            return Err(Error::Weights(format!("truncated file of {len} bytes")));
        }
        Self::parse(Mapping::of_file(file)?)
    }

    /// Reads a weight file from its first byte to its end into an anonymous read-only mapping
    /// (for files that are not on disk, such as tests).
    ///
    /// # Errors
    /// [`Error::Io`] on a read failure, [`Error::Weights`] for a truncated file, a wrong magic
    /// or version, a malformed header, misaligned, descending or overlapping blobs, a blob past
    /// the end, or bytes after the last blob.
    pub fn read<R: Read>(mut r: R) -> Result<Self> {
        let mut bytes = Vec::new();
        r.read_to_end(&mut bytes)?;
        if (bytes.len() as u64) < PREAMBLE {
            return Err(Error::Weights(format!(
                "truncated file of {} bytes",
                bytes.len()
            )));
        }
        Self::parse(Mapping::copy_of(&bytes)?)
    }

    /// Checks the layout of the mapped file and records where each tensor lies. Every check
    /// is on the bytes and the header's numbers, with checked arithmetic, so a malformed file
    /// is an error, never an access out of bounds.
    fn parse(map: Mapping) -> Result<Self> {
        if cfg!(target_endian = "big") {
            return Err(Error::Weights(
                "weight files are little-endian and read in place".into(),
            ));
        }
        let b = map.bytes();
        let len = b.len();
        let pre = b
            .get(..24)
            .ok_or_else(|| Error::Weights(format!("truncated file of {len} bytes")))?;
        if pre[..8] != MAGIC {
            return Err(Error::Weights("not a weight file (bad magic)".into()));
        }
        let word = |at: usize| u32::from_le_bytes([pre[at], pre[at + 1], pre[at + 2], pre[at + 3]]);
        match word(8) {
            VERSION => {}
            1 => {
                return Err(Error::Weights(format!(
                    "format version 1, expected {VERSION}: the file predates the tokenizer \
                     section (ADR-0087 E2); convert the model again"
                )));
            }
            v => {
                return Err(Error::Weights(format!(
                    "format version {v}, expected {VERSION}"
                )));
            }
        }
        let mut n = [0u8; 8];
        n.copy_from_slice(&pre[16..24]);
        let n = u64::from_le_bytes(n);
        if n > MAX_HEADER {
            return Err(Error::Weights(format!("header of {n} bytes is too large")));
        }
        let truncated = || Error::Weights(format!("truncated file of {len} bytes"));
        // `n <= MAX_HEADER`, so these sums cannot overflow.
        let header_end = usize::try_from(PREAMBLE + n).map_err(|_| truncated())?;
        let header = b.get(24..header_end).ok_or_else(truncated)?;
        let (meta, slots) = parse_header(header)?;
        let data_start = usize::try_from(align(PREAMBLE + n)).map_err(|_| truncated())?;
        if data_start > len {
            return Err(truncated());
        }
        let mut tensors = BTreeMap::new();
        let mut pos = 0u64;
        for s in slots {
            if s.offset < pos || !s.offset.is_multiple_of(ALIGN) {
                return Err(Error::Weights(format!(
                    "tensor `{}`: offset {} is misaligned or overlaps",
                    s.name, s.offset
                )));
            }
            let past = || {
                Error::Weights(format!(
                    "tensor `{}` at offset {} runs past the end of the file",
                    s.name, s.offset
                ))
            };
            let bytes = s.count.checked_mul(s.dtype.width()).ok_or_else(past)?;
            let start = usize::try_from(s.offset)
                .ok()
                .and_then(|o| o.checked_add(data_start))
                .ok_or_else(past)?;
            let end = start
                .checked_add(bytes)
                .filter(|&e| e <= len)
                .ok_or_else(past)?;
            // The data section is 64-byte aligned in the file and the mapping is page aligned,
            // so a well-formed file always passes; checked on the real address all the same.
            let aligned = match s.dtype {
                Dtype::F32 => cast::<f32>(&b[start..end]).is_some(),
                Dtype::F64 => cast::<f64>(&b[start..end]).is_some(),
                Dtype::U32 => cast::<u32>(&b[start..end]).is_some(),
                Dtype::I8 | Dtype::U8 => true,
            };
            if !aligned {
                return Err(Error::Weights(format!(
                    "tensor `{}` is not aligned for {} in memory",
                    s.name,
                    s.dtype.name()
                )));
            }
            pos = s.offset + bytes as u64;
            let stored = Stored {
                dtype: s.dtype,
                shape: s.shape,
                start,
                end,
            };
            if tensors.insert(s.name.clone(), stored).is_some() {
                return Err(Error::Weights(format!("duplicate tensor `{}`", s.name)));
            }
        }
        // `pos` is at most `len - data_start` here: every tensor ended inside the file.
        if data_start as u64 + pos != len as u64 {
            return Err(Error::Weights("bytes after the last tensor".into()));
        }
        Ok(Self { meta, tensors, map })
    }

    /// The `meta` object of the header, as the writer put it.
    pub fn meta(&self) -> &Value {
        &self.meta
    }

    /// Names of every tensor, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.tensors.keys().map(String::as_str).collect()
    }

    fn view<T: Plain>(&self, name: &str, dtype: Dtype) -> Result<View<'_, T>> {
        let s = self
            .tensors
            .get(name)
            .ok_or_else(|| Error::Weights(format!("no tensor `{name}`")))?;
        if s.dtype != dtype {
            return Err(Error::Weights(format!(
                "tensor `{name}` is {}, not {}",
                s.dtype.name(),
                dtype.name()
            )));
        }
        // Range and alignment were checked at open.
        let data = cast(&self.map.bytes()[s.start..s.end])
            .ok_or_else(|| Error::Weights(format!("tensor `{name}` is misaligned")))?;
        Ok(View {
            shape: &s.shape,
            data,
        })
    }

    /// The fp32 tensor `name`, in place.
    ///
    /// # Errors
    /// [`Error::Weights`] if there is no such tensor or it is not `f32`.
    pub fn f32(&self, name: &str) -> Result<View<'_, f32>> {
        self.view(name, Dtype::F32)
    }

    /// The int8 tensor `name`, in place.
    ///
    /// # Errors
    /// [`Error::Weights`] if there is no such tensor or it is not `i8`.
    pub fn i8(&self, name: &str) -> Result<View<'_, i8>> {
        self.view(name, Dtype::I8)
    }

    /// The fp64 tensor `name`, in place.
    ///
    /// # Errors
    /// [`Error::Weights`] if there is no such tensor or it is not `f64`.
    pub fn f64(&self, name: &str) -> Result<View<'_, f64>> {
        self.view(name, Dtype::F64)
    }

    /// The byte tensor `name`, in place.
    ///
    /// # Errors
    /// [`Error::Weights`] if there is no such tensor or it is not `u8`.
    pub fn u8(&self, name: &str) -> Result<View<'_, u8>> {
        self.view(name, Dtype::U8)
    }

    /// The `u32` tensor `name`, in place.
    ///
    /// # Errors
    /// [`Error::Weights`] if there is no such tensor or it is not `u32`.
    pub fn u32(&self, name: &str) -> Result<View<'_, u32>> {
        self.view(name, Dtype::U32)
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
            .and_then(Dtype::parse)
            .ok_or_else(|| bad("dtype is not f32, f64, i8, u8 or u32"))?;
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
            dtype,
            shape,
            offset,
            count,
        });
    }
    Ok((meta, slots))
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

    const BYTES: [u8; 5] = [b'a', 0, 255, 0xE2, 0x96];
    const WIDE: [f64; 3] = [-3.463_542_699_813_842_8, f64::MIN_POSITIVE, -0.0];

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
            Entry {
                name: "text".into(),
                shape: vec![5],
                data: Data::U8(&BYTES),
            },
            Entry {
                name: "scores".into(),
                shape: vec![3],
                data: Data::F64(&WIDE),
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
        assert_eq!(file.names(), vec!["a", "map", "scores", "text", "w"]);
        assert_eq!(file.u8("text").unwrap().data, &BYTES[..]);
        assert_eq!(
            file.f64("scores")
                .unwrap()
                .data
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            WIDE.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
        assert!(file.f64("a").is_err() && file.u8("w").is_err());
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
        version[8] = 3;
        assert!(refused(&version).contains("version 3"));
        // A file of version 1 (before the tokenizer section) is refused with its own message.
        version[8] = 1;
        assert!(refused(&version).contains("convert the model again"));
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

    /// The sample file with its header replaced by `edit` of it, the data section unchanged.
    fn with_header(edit: impl Fn(&mut Value)) -> Vec<u8> {
        let good = sample_file();
        let n = usize::try_from(u64::from_le_bytes(good[16..24].try_into().unwrap())).unwrap();
        let data = &good[usize::try_from(align(PREAMBLE + n as u64)).unwrap()..];
        let mut header: Value = serde_json::from_slice(&good[24..24 + n]).unwrap();
        edit(&mut header);
        let text = serde_json::to_vec(&header).unwrap();
        let mut out = good[..16].to_vec();
        out.extend_from_slice(&(text.len() as u64).to_le_bytes());
        out.extend_from_slice(&text);
        out.resize(usize::try_from(align(out.len() as u64)).unwrap(), 0);
        out.extend_from_slice(data);
        out
    }

    fn refused(bytes: &[u8]) -> String {
        match WeightFile::read(bytes) {
            Err(Error::Weights(m)) => m,
            other => panic!("expected a weights error, got {other:?}"),
        }
    }

    #[test]
    fn every_truncation_is_refused() {
        let good = sample_file();
        assert!(WeightFile::read(good.as_slice()).is_ok());
        for k in 0..good.len() {
            refused(&good[..k]);
        }
    }

    type Edit = Box<dyn Fn(&mut Value)>;

    #[test]
    fn bad_headers_and_offsets_are_refused() {
        // The edit helper itself keeps a good file good.
        assert!(WeightFile::read(with_header(|_| {}).as_slice()).is_ok());
        let cases: Vec<(&str, Edit)> = vec![
            (
                "misaligned",
                Box::new(|h| h["tensors"][1]["offset"] = json!(68)),
            ),
            (
                "misaligned",
                Box::new(|h| h["tensors"][0]["offset"] = json!(1)),
            ),
            (
                "overlaps",
                Box::new(|h| h["tensors"][1]["offset"] = json!(0)),
            ),
            (
                "past the end",
                Box::new(|h| h["tensors"][4]["offset"] = json!(1 << 20)),
            ),
            (
                "past the end",
                Box::new(|h| h["tensors"][4]["offset"] = json!(u64::MAX - 63)),
            ),
            (
                "past the end",
                Box::new(|h| h["tensors"][4]["shape"] = json!([5])),
            ),
            (
                "past the end",
                Box::new(|h| h["tensors"][4]["shape"] = json!([1u64 << 62])),
            ),
            (
                "overflows",
                Box::new(|h| h["tensors"][0]["shape"] = json!([u64::MAX, 2])),
            ),
            (
                "dtype",
                Box::new(|h| h["tensors"][0]["dtype"] = json!("f16")),
            ),
            (
                "missing offset",
                Box::new(|h| h["tensors"][0]["offset"] = json!(-64)),
            ),
            (
                "missing offset",
                Box::new(|h| h["tensors"][0]["offset"] = json!(6.4)),
            ),
            (
                "bad shape",
                Box::new(|h| h["tensors"][0]["shape"] = json!([-1])),
            ),
            (
                "missing name",
                Box::new(|h| h["tensors"][0]["name"] = json!(3)),
            ),
            (
                "duplicate",
                Box::new(|h| h["tensors"][2]["name"] = json!("w")),
            ),
            (
                "bytes after",
                Box::new(|h| h["tensors"][4]["shape"] = json!([2])),
            ),
            (
                "no `meta`",
                Box::new(|h| drop(h.as_object_mut().unwrap().remove("meta"))),
            ),
            ("no `tensors`", Box::new(|h| h["tensors"] = json!({}))),
        ];
        for (want, edit) in cases {
            let m = refused(&with_header(edit));
            assert!(m.contains(want), "{want:?} not in {m:?}");
        }
        // Header length fields that do not fit the file or the limit.
        let good = sample_file();
        for n in [good.len() as u64, MAX_HEADER + 1, u64::MAX] {
            let mut bad = good.clone();
            bad[16..24].copy_from_slice(&n.to_le_bytes());
            refused(&bad);
        }
        let mut not_json = good.clone();
        not_json[24] = b'[';
        assert!(refused(&not_json).contains("not JSON"));
        // A data section that starts past the end: header only, no padding.
        let n = usize::try_from(u64::from_le_bytes(good[16..24].try_into().unwrap())).unwrap();
        assert!(refused(&good[..24 + n]).contains("truncated"));
    }

    #[test]
    fn random_corruption_never_panics() {
        let good = sample_file();
        let n = usize::try_from(u64::from_le_bytes(good[16..24].try_into().unwrap())).unwrap();
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            usize::try_from(s >> 33).unwrap()
        };
        let mut opened = 0;
        for round in 0..20_000 {
            let mut bad = good.clone();
            for _ in 0..=next() % 4 {
                // Mostly the preamble and the header, where the numbers are.
                let at = if next() % 4 == 0 {
                    next() % bad.len()
                } else {
                    next() % (24 + n)
                };
                bad[at] = u8::try_from(next() % 256).unwrap();
            }
            if round % 3 == 0 {
                bad.truncate(next() % (bad.len() + 1));
            }
            if let Ok(file) = WeightFile::read(bad.as_slice()) {
                opened += 1;
                for name in file.names() {
                    let _ = (file.f32(name), file.i8(name), file.u32(name));
                    let _ = (file.f64(name), file.u8(name));
                }
            }
        }
        // Changes to blob bytes and to the JSON's insignificant bytes leave a valid file.
        assert!(opened > 0);
    }
}
