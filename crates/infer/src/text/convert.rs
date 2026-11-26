//! The tokenizer section of a weight file, built from a checkpoint's `tokenizer.json` (pure
//! computation; the caller reads and writes the files).
//!
//! Read from `tokenizer.json`: the Unigram vocabulary (pieces, scores, `unk_id`), the
//! precompiled character map of the normaliser (base64), the added tokens and the special ids
//! of the post-processor (`TemplateProcessing` `<s> $A </s>` or `RobertaProcessing`). For
//! [`Pipeline::CollapseSpaces`] the file's normaliser and pre-tokeniser must be exactly that
//! pipeline (`Precompiled`, `Replace` of `" {2,}"` by `" "`, `Metaspace` with `▁` prepended
//! always and split); for [`Pipeline::WhitespaceSplit`] they are ignored but for the character
//! map, as the `transformers` tokenizer class ignores them. Anything this module cannot
//! reproduce exactly (another model, byte fallback, added tokens matched on normalised text or
//! as single words) is refused.
//!
//! The pieces are sorted by their bytes; of two equal pieces the later id wins, as in the
//! reference's lookup table. The output is a function of the inputs only.

use std::io::Write;

use serde_json::{Value, json};

use super::{
    CHARSMAP_STRINGS, CHARSMAP_TRIE, FORMAT, META_TOKENIZER, PIECE_BYTES, PIECE_IDS, PIECE_OFFSETS,
    PIECE_SCORES, Pipeline,
};
use crate::weights::file::{self, Data, Entry};
use crate::{Error, Result};

/// One array of the section, owned until it is written.
#[derive(Debug, Clone, PartialEq)]
pub enum Owned {
    /// Bytes.
    U8(Vec<u8>),
    /// `u32` values.
    U32(Vec<u32>),
    /// fp64 values.
    F64(Vec<f64>),
}

/// The tokenizer section: `meta.tokenizer` and its arrays.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// The object to store under `meta.tokenizer`.
    pub meta: Value,
    /// Name, shape and data of each array, in file order.
    pub tensors: Vec<(String, Vec<usize>, Owned)>,
}

impl Section {
    /// The arrays as weight file entries, borrowed.
    pub fn entries(&self) -> Vec<Entry<'_>> {
        self.tensors
            .iter()
            .map(|(name, shape, data)| Entry {
                name: name.clone(),
                shape: shape.clone(),
                data: match data {
                    Owned::U8(d) => Data::U8(d),
                    Owned::U32(d) => Data::U32(d),
                    Owned::F64(d) => Data::F64(d),
                },
            })
            .collect()
    }

    /// Writes a weight file that holds only this section (tests and tools that need the
    /// tokenizer without a model); returns the bytes written.
    ///
    /// # Errors
    /// As [`file::write`].
    pub fn write_alone<W: Write>(&self, w: W) -> Result<u64> {
        file::write(w, &json!({ META_TOKENIZER: self.meta }), &self.entries())
    }
}

fn bad(what: &str) -> Error {
    Error::Config(format!("tokenizer.json: {what}"))
}

/// Builds the section from the text of `tokenizer.json` for `pipeline`, truncating to
/// `max_tokens` ids (`<s>` and `</s>` included, at least 2).
///
/// # Errors
/// [`Error::Config`] if the file is not JSON, not a Unigram tokenizer this module reproduces,
/// or `max_tokens` is below 2.
pub fn section(tokenizer_json: &str, pipeline: Pipeline, max_tokens: usize) -> Result<Section> {
    let v: Value = serde_json::from_str(tokenizer_json).map_err(|e| bad(&e.to_string()))?;
    if max_tokens < 2 {
        return Err(bad("max_tokens below 2"));
    }
    let model = &v["model"];
    let (unk_id, pieces, min_score, mut vocab_size) = unigram(model)?;
    let Pieces {
        bytes,
        offsets,
        ids,
        scores,
    } = pieces;

    let charsmap = charsmap(&v["normalizer"], pipeline)?;
    if pipeline == Pipeline::CollapseSpaces {
        check_collapse_spaces(&v["normalizer"], &v["pre_tokenizer"])?;
    }
    let (trie, strings) = match charsmap {
        Some(b64) => split_charsmap(&base64(b64)?)?,
        None => (Vec::new(), Vec::new()),
    };

    let mut added = Vec::new();
    for a in v["added_tokens"].as_array().into_iter().flatten() {
        let content = a["content"]
            .as_str()
            .filter(|c| !c.is_empty() && !c.starts_with(char::is_whitespace))
            .ok_or_else(|| bad("an added token is empty or starts with whitespace"))?;
        let id = a["id"]
            .as_u64()
            .and_then(|u| u32::try_from(u).ok())
            .ok_or_else(|| bad("an added token has no id"))?;
        if a["single_word"] == true || a["normalized"] == true {
            return Err(bad(&format!(
                "added token {content:?}: single_word or normalized is not supported"
            )));
        }
        vocab_size = vocab_size.max(id as usize + 1);
        added.push(json!({
            "content": content,
            "id": id,
            "lstrip": a["lstrip"] == true,
            "rstrip": a["rstrip"] == true,
        }));
    }
    let (bos_id, eos_id) = specials(&v["post_processor"])?;
    if [unk_id, bos_id, eos_id]
        .iter()
        .any(|&i| i as usize >= vocab_size)
    {
        return Err(bad("a special id is outside the vocabulary"));
    }

    let meta = json!({
        "format": FORMAT,
        "pipeline": pipeline.name(),
        "vocab_size": vocab_size,
        "unk_id": unk_id,
        "bos_id": bos_id,
        "eos_id": eos_id,
        "min_score_bits": min_score.to_bits(),
        "max_tokens": max_tokens,
        "added": added,
    });
    let n = ids.len();
    let tensors = vec![
        (CHARSMAP_TRIE.to_owned(), vec![trie.len()], Owned::U32(trie)),
        (
            CHARSMAP_STRINGS.to_owned(),
            vec![strings.len()],
            Owned::U8(strings),
        ),
        (PIECE_BYTES.to_owned(), vec![bytes.len()], Owned::U8(bytes)),
        (PIECE_OFFSETS.to_owned(), vec![n + 1], Owned::U32(offsets)),
        (PIECE_IDS.to_owned(), vec![n], Owned::U32(ids)),
        (PIECE_SCORES.to_owned(), vec![n], Owned::F64(scores)),
    ];
    Ok(Section { meta, tensors })
}

/// The sorted piece arrays.
struct Pieces {
    bytes: Vec<u8>,
    offsets: Vec<u32>,
    ids: Vec<u32>,
    scores: Vec<f64>,
}

/// `unk_id`, the pieces in byte order (equal pieces: the last id kept), the lowest score and
/// the vocabulary's length, from `model`.
fn unigram(model: &Value) -> Result<(u32, Pieces, f64, usize)> {
    if model["type"] != "Unigram" {
        return Err(bad("the model is not Unigram"));
    }
    if model["byte_fallback"] == true {
        return Err(bad("byte fallback is not supported"));
    }
    let unk_id = model["unk_id"]
        .as_u64()
        .and_then(|u| u32::try_from(u).ok())
        .ok_or_else(|| bad("model.unk_id is missing"))?;
    let vocab = model["vocab"]
        .as_array()
        .ok_or_else(|| bad("model.vocab is not a list"))?;
    let mut entries: Vec<(&str, u32, f64)> = Vec::with_capacity(vocab.len());
    let mut min_score = f64::INFINITY;
    for (id, e) in vocab.iter().enumerate() {
        let (Some(piece), Some(score)) = (e[0].as_str(), e[1].as_f64()) else {
            return Err(bad("vocab entries must be [piece, score]"));
        };
        min_score = min_score.min(score);
        let id = u32::try_from(id).map_err(|_| bad("vocabulary too large"))?;
        entries.push((piece, id, score));
    }
    if entries.is_empty() || !min_score.is_finite() {
        return Err(bad("empty vocabulary or a non-finite score"));
    }
    // Byte order; equal pieces by id, the last of them kept.
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()).then(a.1.cmp(&b.1)));
    let mut kept: Vec<(&str, u32, f64)> = Vec::with_capacity(entries.len());
    for e in entries {
        match kept.last_mut() {
            Some(last) if last.0 == e.0 => *last = e,
            _ => kept.push(e),
        }
    }
    let mut bytes = Vec::new();
    let mut offsets = vec![0u32];
    let mut ids = Vec::with_capacity(kept.len());
    let mut scores = Vec::with_capacity(kept.len());
    for (piece, id, score) in &kept {
        bytes.extend_from_slice(piece.as_bytes());
        offsets.push(u32::try_from(bytes.len()).map_err(|_| bad("pieces too long"))?);
        ids.push(*id);
        scores.push(*score);
    }
    let pieces = Pieces {
        bytes,
        offsets,
        ids,
        scores,
    };
    Ok((unk_id, pieces, min_score, vocab.len()))
}

/// The base64 character map of the normaliser (alone or in a `Sequence`), if any.
fn charsmap(n: &Value, pipeline: Pipeline) -> Result<Option<&str>> {
    let found = match n["type"].as_str() {
        Some("Precompiled") => Some(n),
        Some("Sequence") => n["normalizers"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["type"] == "Precompiled")),
        _ => None,
    };
    match (found, pipeline) {
        (Some(p), _) => p["precompiled_charsmap"]
            .as_str()
            .map(Some)
            .ok_or_else(|| bad("Precompiled without precompiled_charsmap")),
        (None, Pipeline::WhitespaceSplit) if n.is_null() => Ok(None),
        (None, _) => Err(bad("the normaliser holds no precompiled character map")),
    }
}

/// Checks that the file's own pipeline is the one [`Pipeline::CollapseSpaces`] reproduces.
fn check_collapse_spaces(normalizer: &Value, pre: &Value) -> Result<()> {
    let steps = normalizer["normalizers"].as_array();
    let replace = |r: &Value| {
        r["type"] == "Replace" && r["pattern"]["Regex"] == " {2,}" && r["content"] == " "
    };
    let norm_ok = normalizer["type"] == "Sequence"
        && steps.is_some_and(|s| s.len() == 2 && s[0]["type"] == "Precompiled" && replace(&s[1]));
    let prepend_always = pre["prepend_scheme"] == "always"
        || (pre["prepend_scheme"].is_null() && pre["add_prefix_space"] == true);
    let pre_ok = pre["type"] == "Metaspace"
        && pre["replacement"] == "\u{2581}"
        && prepend_always
        && (pre["split"].is_null() || pre["split"] == true);
    if norm_ok && pre_ok {
        Ok(())
    } else {
        Err(bad(
            "collapse-spaces needs normaliser [Precompiled, Replace \" {2,}\" -> \" \"] and \
             pre-tokeniser Metaspace (▁, prepended always, split)",
        ))
    }
}

/// `<s>` and `</s>` ids of the post-processor.
fn specials(p: &Value) -> Result<(u32, u32)> {
    let id = |v: &Value| v.as_u64().and_then(|u| u32::try_from(u).ok());
    match p["type"].as_str() {
        Some("TemplateProcessing") => {
            let single = p["single"].as_array().filter(|s| s.len() == 3);
            let Some(s) = single.filter(|s| s[1]["Sequence"]["id"] == "A") else {
                return Err(bad("post_processor.single is not [special, $A, special]"));
            };
            let special = |item: &Value| {
                let name = item["SpecialToken"]["id"].as_str()?;
                let ids = p["special_tokens"][name]["ids"].as_array()?;
                if ids.len() == 1 { id(&ids[0]) } else { None }
            };
            special(&s[0])
                .zip(special(&s[2]))
                .ok_or_else(|| bad("post_processor special tokens are not single ids"))
        }
        Some("RobertaProcessing") => id(&p["cls"][1])
            .zip(id(&p["sep"][1]))
            .ok_or_else(|| bad("RobertaProcessing without cls and sep ids")),
        _ => Err(bad(
            "post_processor is neither TemplateProcessing nor RobertaProcessing",
        )),
    }
}

/// The trie units and the strings of a precompiled character map blob: `u32` trie size in
/// bytes, the trie, then the NUL-ended strings (which must be UTF-8, as the reference checks).
fn split_charsmap(blob: &[u8]) -> Result<(Vec<u32>, Vec<u8>)> {
    let size = blob
        .first_chunk::<4>()
        .map(|b| u32::from_le_bytes(*b) as usize)
        .ok_or_else(|| bad("character map shorter than its size field"))?;
    let units = size / 4;
    let trie_end = 4 + units * 4;
    let trie_bytes = blob
        .get(4..trie_end)
        .ok_or_else(|| bad("character map shorter than its trie"))?;
    let trie: Vec<u32> = trie_bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect();
    let strings = blob[trie_end..].to_vec();
    std::str::from_utf8(&strings).map_err(|_| bad("character map strings are not UTF-8"))?;
    Ok((trie, strings))
}

/// Standard base64 with padding.
fn base64(s: &str) -> Result<Vec<u8>> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let s = s.as_bytes();
    if !s.len().is_multiple_of(4) {
        return Err(bad("character map is not base64"));
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (k, q) in s.as_chunks::<4>().0.iter().enumerate() {
        let last = k + 1 == s.len() / 4;
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return Err(bad("character map is not base64"));
        }
        let mut n = 0u32;
        for &c in &q[..4 - pad] {
            n = (n << 6) | u32::from(val(c).ok_or_else(|| bad("character map is not base64"))?);
        }
        n <<= 6 * pad;
        let b = n.to_be_bytes();
        out.extend_from_slice(&b[1..4 - pad]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes_and_refuses_garbage() {
        assert_eq!(base64("").unwrap(), b"");
        assert_eq!(base64("TWFu").unwrap(), b"Man");
        assert_eq!(base64("TWE=").unwrap(), b"Ma");
        assert_eq!(base64("TQ==").unwrap(), b"M");
        assert_eq!(base64("+/8A").unwrap(), [0xFB, 0xFF, 0x00]);
        for b in ["TWF", "TW=u", "T===", "TQ==TWFu", "TW!u"] {
            assert!(base64(b).is_err(), "{b}");
        }
    }

    #[test]
    fn charsmap_blob_splits() {
        let mut blob = 8u32.to_le_bytes().to_vec();
        blob.extend_from_slice(&[1, 0, 0, 0, 2, 0, 0, 0]);
        blob.extend_from_slice(b"x\0");
        assert_eq!(
            split_charsmap(&blob).unwrap(),
            (vec![1, 2], b"x\0".to_vec())
        );
        assert!(split_charsmap(&blob[..6]).is_err());
        assert!(split_charsmap(&blob[..2]).is_err());
        let mut bad_utf8 = blob.clone();
        bad_utf8.push(0xFF);
        assert!(split_charsmap(&bad_utf8).is_err());
    }
}
