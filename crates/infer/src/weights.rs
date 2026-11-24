//! Weights of a model: a `safetensors` checkpoint ([`Checkpoint`], fp32, the source of the
//! converter and of the E1 forward pass) and the engine's own weight file ([`file`], int8,
//! ADR-0086 section 2).
//!
//! The `safetensors` checkpoint:
//!
//! The format: 8 bytes little-endian header length `n`, `n` bytes of JSON mapping each tensor
//! name to `{"dtype", "shape", "data_offsets": [begin, end]}` (offsets into the byte buffer that
//! follows the header) plus an optional `__metadata__` entry, then the raw little-endian data.
//!
//! E1 keeps `F32` tensors as owned `Vec<f32>` on the heap and records the names of the others
//! (the `I64` `position_ids` buffer, for one). The forward pass sees only [`View`]s, so a
//! mapped or quantised backing (ADR-0086 sections 2 and E4) replaces [`Checkpoint`]'s storage
//! without touching it.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use serde_json::Value;

use crate::{Error, Result};

pub mod file;

/// Largest header accepted, as in the reference implementation of the format.
const MAX_HEADER: u64 = 100 * 1024 * 1024;

/// Bytes converted per read while loading a tensor.
const CHUNK: usize = 1 << 20;

/// One fp32 tensor with its shape, row-major.
#[derive(Debug, Clone, PartialEq)]
pub struct Tensor {
    shape: Vec<usize>,
    data: Vec<f32>,
}

impl Tensor {
    /// Borrows the tensor.
    pub fn view(&self) -> View<'_> {
        View {
            shape: &self.shape,
            data: &self.data,
        }
    }
}

/// A borrowed tensor (fp32 unless said otherwise): its shape and its row-major data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View<'a, T = f32> {
    /// Dimensions, outermost first.
    pub shape: &'a [usize],
    /// `shape.iter().product()` values, row-major.
    pub data: &'a [T],
}

/// The tensors of one checkpoint.
#[derive(Debug, Default)]
pub struct Checkpoint {
    tensors: BTreeMap<String, Tensor>,
    other: BTreeMap<String, String>,
}

struct Entry {
    name: String,
    dtype: String,
    shape: Vec<usize>,
    begin: u64,
    end: u64,
}

impl Checkpoint {
    /// Reads a `safetensors` file.
    ///
    /// # Errors
    /// [`Error::Io`] if the file cannot be read, [`Error::Weights`] if it is malformed.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        Self::read(BufReader::with_capacity(CHUNK, file))
    }

    /// Reads a `safetensors` stream from its first byte.
    ///
    /// # Errors
    /// [`Error::Io`] on a read failure or a truncated stream, [`Error::Weights`] if the header
    /// is malformed, a tensor's size disagrees with its shape or two tensors overlap.
    pub fn read<R: Read>(mut r: R) -> Result<Self> {
        let mut len = [0u8; 8];
        r.read_exact(&mut len)?;
        let len = u64::from_le_bytes(len);
        if len > MAX_HEADER {
            return Err(Error::Weights(format!(
                "header of {len} bytes is too large"
            )));
        }
        let mut header = Vec::new();
        r.by_ref().take(len).read_to_end(&mut header)?;
        if header.len() as u64 != len {
            return Err(Error::Weights("truncated header".into()));
        }
        let mut entries = parse_header(&header)?;
        entries.sort_by_key(|e| (e.begin, e.end));
        let mut out = Self::default();
        let mut pos = 0u64;
        let mut buf = vec![0u8; CHUNK];
        for e in entries {
            if e.begin < pos {
                return Err(Error::Weights(format!(
                    "tensor `{}` overlaps another",
                    e.name
                )));
            }
            skip(&mut r, e.begin - pos)?;
            pos = e.end;
            if e.dtype != "F32" {
                skip(&mut r, e.end - e.begin)?;
                out.other.insert(e.name, e.dtype);
                continue;
            }
            let count: usize = e.shape.iter().product();
            if (count as u64).checked_mul(4) != Some(e.end - e.begin) {
                return Err(Error::Weights(format!(
                    "tensor `{}`: {} bytes for shape {:?}",
                    e.name,
                    e.end - e.begin,
                    e.shape
                )));
            }
            let mut data = Vec::with_capacity(count);
            let mut left = count * 4;
            while left > 0 {
                let n = left.min(CHUNK);
                let chunk = &mut buf[..n];
                r.read_exact(chunk)?;
                data.extend(
                    chunk
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b)),
                );
                left -= n;
            }
            out.tensors.insert(
                e.name,
                Tensor {
                    shape: e.shape,
                    data,
                },
            );
        }
        Ok(out)
    }

    /// The fp32 tensor `name`.
    ///
    /// # Errors
    /// [`Error::Weights`] if the checkpoint has no such tensor or it is not `F32`.
    pub fn get(&self, name: &str) -> Result<View<'_>> {
        if let Some(t) = self.tensors.get(name) {
            return Ok(t.view());
        }
        Err(Error::Weights(match self.other.get(name) {
            Some(dtype) => format!("tensor `{name}` is {dtype}, not F32"),
            None => format!("no tensor `{name}`"),
        }))
    }

    /// Names of every tensor in the file, fp32 or not, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .tensors
            .keys()
            .chain(self.other.keys())
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        names
    }
}

fn parse_header(header: &[u8]) -> Result<Vec<Entry>> {
    let v: Value = serde_json::from_slice(header)
        .map_err(|e| Error::Weights(format!("header is not JSON: {e}")))?;
    let map = v
        .as_object()
        .ok_or_else(|| Error::Weights("header is not a JSON object".into()))?;
    let mut entries = Vec::with_capacity(map.len());
    for (name, t) in map {
        if name == "__metadata__" {
            continue;
        }
        let bad = |what: &str| Error::Weights(format!("tensor `{name}`: {what}"));
        let dtype = t
            .get("dtype")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("missing dtype"))?;
        let shape = t
            .get("shape")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("missing shape"))?
            .iter()
            .map(|d| d.as_u64().and_then(|d| usize::try_from(d).ok()))
            .collect::<Option<Vec<usize>>>()
            .ok_or_else(|| bad("bad shape"))?;
        let offsets = t
            .get("data_offsets")
            .and_then(Value::as_array)
            .and_then(|a| match a.as_slice() {
                [b, e] => Some((b.as_u64()?, e.as_u64()?)),
                _ => None,
            })
            .ok_or_else(|| bad("bad data_offsets"))?;
        if offsets.1 < offsets.0 {
            return Err(bad("data_offsets end before begin"));
        }
        entries.push(Entry {
            name: name.clone(),
            dtype: dtype.to_string(),
            shape,
            begin: offsets.0,
            end: offsets.1,
        });
    }
    Ok(entries)
}

fn skip<R: Read>(r: &mut R, n: u64) -> Result<()> {
    let skipped = std::io::copy(&mut r.by_ref().take(n), &mut std::io::sink())?;
    if skipped == n {
        Ok(())
    } else {
        Err(Error::Weights("truncated data".into()))
    }
}

/// Encodes fp32 tensors as a `safetensors` byte buffer, in the given order (tests only).
#[cfg(test)]
pub(crate) fn encode(tensors: &[(&str, &[usize], &[f32])]) -> Vec<u8> {
    let mut header = serde_json::Map::new();
    let mut data = Vec::new();
    for (name, shape, values) in tensors {
        let begin = data.len();
        for v in *values {
            data.extend_from_slice(&v.to_le_bytes());
        }
        header.insert(
            (*name).to_string(),
            serde_json::json!({"dtype": "F32", "shape": shape, "data_offsets": [begin, data.len()]}),
        );
    }
    header.insert("__metadata__".into(), serde_json::json!({"format": "pt"}));
    let header = serde_json::to_vec(&Value::Object(header)).unwrap();
    let mut out = (header.len() as u64).to_le_bytes().to_vec();
    out.extend_from_slice(&header);
    out.extend_from_slice(&data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_f32_tensors_and_records_others() {
        let mut bytes = encode(&[
            ("a", &[2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, -6.5]),
            ("b", &[1], &[0.25]),
        ]);
        // Rewrite as a file with an extra I64 tensor after the f32 data.
        let n = usize::try_from(u64::from_le_bytes(bytes[..8].try_into().unwrap())).unwrap();
        let mut header: Value = serde_json::from_slice(&bytes[8..8 + n]).unwrap();
        let data_len = bytes.len() - 8 - n;
        header["ids"] = serde_json::json!({"dtype": "I64", "shape": [2],
            "data_offsets": [data_len, data_len + 16]});
        let header = serde_json::to_vec(&header).unwrap();
        let data = bytes.split_off(8 + n);
        let mut file = (header.len() as u64).to_le_bytes().to_vec();
        file.extend_from_slice(&header);
        file.extend_from_slice(&data);
        file.extend_from_slice(&[7u8; 16]);

        let c = Checkpoint::read(file.as_slice()).unwrap();
        let a = c.get("a").unwrap();
        assert_eq!(a.shape, &[2, 3]);
        assert_eq!(a.data, &[1.0, 2.0, 3.0, 4.0, 5.0, -6.5]);
        assert_eq!(c.get("b").unwrap().data, &[0.25]);
        assert!(matches!(c.get("ids"), Err(Error::Weights(m)) if m.contains("I64")));
        assert!(c.get("missing").is_err());
        assert_eq!(c.names(), vec!["a", "b", "ids"]);
    }

    #[test]
    fn malformed_files_are_refused() {
        let good = encode(&[("a", &[2], &[1.0, 2.0])]);
        // Truncated data.
        assert!(Checkpoint::read(&good[..good.len() - 1]).is_err());
        // Size disagreeing with the shape.
        let bad = encode(&[("a", &[3], &[1.0, 2.0])]);
        assert!(Checkpoint::read(bad.as_slice()).is_err());
        // Header length beyond the limit.
        let mut huge = (MAX_HEADER + 1).to_le_bytes().to_vec();
        huge.extend_from_slice(b"{}");
        assert!(Checkpoint::read(huge.as_slice()).is_err());
        // Not JSON.
        let mut junk = 3u64.to_le_bytes().to_vec();
        junk.extend_from_slice(b"{x}");
        assert!(Checkpoint::read(junk.as_slice()).is_err());
    }
}
