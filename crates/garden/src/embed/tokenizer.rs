//! The `XLM-R` `SentencePiece` pipeline of bge-m3 read from its `tokenizer.json`: the precompiled
//! character-map normaliser, runs of spaces collapsed, Metaspace (`▁`, always prepended, split
//! before each `▁`), Unigram Viterbi with fused unknowns, `<s> … </s>` around at most
//! `max_tokens - 2` pieces. Matches the reference `tokenizers` crate on this model.

use std::collections::HashMap;

use serde_json::Value;
use spm_precompiled::Precompiled;

use super::EmbedError;

const SPACE: char = '▁';
/// Score handicap of an unknown character below the lowest piece score.
const UNK_PENALTY: f64 = 10.0;

/// Unigram tokenizer of one `tokenizer.json`.
pub struct Tokenizer {
    pieces: HashMap<String, (u32, f64)>,
    max_piece_bytes: usize,
    unk_id: u32,
    unk_score: f64,
    bos_id: u32,
    eos_id: u32,
    normalizer: Option<Precompiled>,
}

impl Tokenizer {
    /// Reads a Unigram `tokenizer.json` without byte fallback.
    ///
    /// # Errors
    ///
    /// [`EmbedError::Load`] when the file is not such a tokenizer.
    pub fn from_json(text: &str) -> Result<Self, EmbedError> {
        let bad = |m: &str| EmbedError::Load(format!("tokenizer.json: {m}"));
        let v: Value = serde_json::from_str(text).map_err(|e| bad(&e.to_string()))?;
        let model = &v["model"];
        if model["type"] != "Unigram" || model["byte_fallback"] == true {
            return Err(bad(
                "only a Unigram model without byte fallback is supported",
            ));
        }
        if !matches!(&v["pre_tokenizer"]["type"], Value::String(t) if t == "Metaspace") {
            return Err(bad("only the Metaspace pre-tokenizer is supported"));
        }
        let vocab = model["vocab"]
            .as_array()
            .ok_or_else(|| bad("model.vocab is not a list"))?;
        let mut pieces = HashMap::with_capacity(vocab.len());
        let mut min_score = f64::INFINITY;
        let mut max_piece_bytes = 0;
        for (id, entry) in vocab.iter().enumerate() {
            let (Some(piece), Some(score)) = (entry[0].as_str(), entry[1].as_f64()) else {
                return Err(bad("vocab entries must be [piece, score]"));
            };
            let id = u32::try_from(id).map_err(|_| bad("vocab too large"))?;
            min_score = min_score.min(score);
            max_piece_bytes = max_piece_bytes.max(piece.len());
            pieces.insert(piece.to_owned(), (id, score));
        }
        let unk_id = model["unk_id"]
            .as_u64()
            .and_then(|u| u32::try_from(u).ok())
            .ok_or_else(|| bad("model.unk_id missing"))?;
        let id_of = |p: &str| pieces.get(p).map(|(id, _)| *id);
        let (Some(bos_id), Some(eos_id)) = (id_of("<s>"), id_of("</s>")) else {
            return Err(bad("vocab lacks <s> or </s>"));
        };
        let normalizer = precompiled_of(&v["normalizer"])
            // The charsmap deserialiser borrows its base64 text, so it needs JSON text.
            .map(|n| serde_json::from_str::<Precompiled>(&n.to_string()))
            .transpose()
            .map_err(|e| bad(&format!("precompiled charsmap: {e}")))?;
        Ok(Self {
            pieces,
            max_piece_bytes,
            unk_id,
            unk_score: min_score - UNK_PENALTY,
            bos_id,
            eos_id,
            normalizer,
        })
    }

    /// Piece ids of `text` framed by `<s>` and `</s>`, at most `max_tokens` in total.
    pub fn encode(&self, text: &str, max_tokens: usize) -> Vec<u32> {
        let normalized = match &self.normalizer {
            Some(n) => n.normalize_string(text),
            None => text.to_owned(),
        };
        let mut collapsed = String::with_capacity(normalized.len() + 3);
        let mut prev_space = false;
        for c in normalized.chars() {
            if c == ' ' && prev_space {
                continue;
            }
            prev_space = c == ' ';
            collapsed.push(if c == ' ' { SPACE } else { c });
        }
        let mut ids = vec![self.bos_id];
        let budget = max_tokens.saturating_sub(2);
        if !collapsed.is_empty() {
            if !collapsed.starts_with(SPACE) {
                collapsed.insert(0, SPACE);
            }
            for word in split_before_space(&collapsed) {
                if ids.len() > budget {
                    break;
                }
                self.viterbi(word, &mut ids);
            }
        }
        ids.truncate(budget + 1);
        ids.push(self.eos_id);
        ids
    }

    /// Appends the best Unigram segmentation of `word` to `out`.
    fn viterbi(&self, word: &str, out: &mut Vec<u32>) {
        // best[i] = (score, start, id) of the best path covering word[..i].
        let mut best: Vec<Option<(f64, usize, u32)>> = vec![None; word.len() + 1];
        best[0] = Some((0.0, 0, 0));
        for (start, c) in word.char_indices() {
            let Some((base, _, _)) = best[start] else {
                continue;
            };
            let mut single = false;
            let relax =
                |end: usize, score: f64, id: u32, best: &mut [Option<(f64, usize, u32)>]| {
                    let cand = base + score;
                    if best[end].is_none_or(|(s, _, _)| cand > s) {
                        best[end] = Some((cand, start, id));
                    }
                };
            for (off, ch) in word[start..].char_indices() {
                let len = off + ch.len_utf8();
                if len > self.max_piece_bytes {
                    break;
                }
                let end = start + len;
                if let Some(&(id, score)) = self.pieces.get(&word[start..end]) {
                    relax(end, score, id, &mut best);
                    single |= end == start + c.len_utf8();
                }
            }
            if !single {
                relax(start + c.len_utf8(), self.unk_score, self.unk_id, &mut best);
            }
        }
        let mut path = Vec::new();
        let mut end = word.len();
        while end > 0 {
            let Some((_, start, id)) = best[end] else {
                break;
            };
            // Consecutive unknowns fuse into one piece.
            if !(id == self.unk_id && path.last() == Some(&self.unk_id)) {
                path.push(id);
            }
            end = start;
        }
        out.extend(path.into_iter().rev());
    }
}

/// The `Precompiled` node of a normaliser (alone or inside a `Sequence`).
fn precompiled_of(n: &Value) -> Option<&Value> {
    match n["type"].as_str()? {
        "Precompiled" => Some(n),
        "Sequence" => n["normalizers"].as_array()?.iter().find_map(precompiled_of),
        _ => None,
    }
}

/// Splits before every `▁`, keeping it with the following text.
fn split_before_space(s: &str) -> impl Iterator<Item = &str> {
    let mut starts: Vec<usize> = s.match_indices(SPACE).map(|(i, _)| i).collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    let ends: Vec<usize> = starts.iter().skip(1).copied().chain([s.len()]).collect();
    starts
        .into_iter()
        .zip(ends)
        .map(move |(a, b)| &s[a..b])
        .filter(|w| !w.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny() -> Tokenizer {
        let json = r#"{"normalizer": null,
          "pre_tokenizer": {"type": "Metaspace", "replacement": "▁", "prepend_scheme": "always"},
          "model": {"type": "Unigram", "unk_id": 3, "byte_fallback": false, "vocab": [
            ["<s>", 0.0], ["<pad>", 0.0], ["</s>", 0.0], ["<unk>", 0.0],
            ["▁", -2.0], ["▁ab", -1.0], ["a", -3.0], ["b", -3.0], ["c", -3.0],
            ["▁abc", -5.0], ["bc", -1.0], ["▁c", -1.5]]}}"#;
        Tokenizer::from_json(json).unwrap()
    }

    #[test]
    fn viterbi_picks_the_best_segmentation() {
        let t = tiny();
        // "▁abc": "▁ab"+"c" = -4, "▁abc" = -5, "▁"+"a"+"bc" = -6 → ▁ab c
        assert_eq!(t.encode("abc", 512), vec![0, 5, 8, 2]);
        // spaces collapse, words split before ▁
        assert_eq!(t.encode("ab   c", 512), vec![0, 5, 11, 2]);
    }

    #[test]
    fn unknowns_fuse_and_truncation_keeps_the_frame() {
        let t = tiny();
        assert_eq!(t.encode("axyz", 512), vec![0, 4, 6, 3, 2]);
        assert_eq!(t.encode("ab ab ab ab", 4), vec![0, 5, 5, 2]);
        assert_eq!(t.encode("", 512), vec![0, 2]);
    }

    #[test]
    fn piece_length_limit_stops_at_char_boundaries() {
        // Longest piece is 3 bytes; "б" is 2, so a byte limit would split a character.
        let json = r#"{"pre_tokenizer": {"type": "Metaspace"},
          "model": {"type": "Unigram", "unk_id": 3, "vocab": [
            ["<s>", 0.0], ["<pad>", 0.0], ["</s>", 0.0], ["<unk>", 0.0],
            ["▁", -1.0], ["б", -1.0]]}}"#;
        let t = Tokenizer::from_json(json).unwrap();
        assert_eq!(t.encode("ббб", 512), vec![0, 4, 5, 5, 5, 2]);
    }

    #[test]
    fn rejects_other_models() {
        let err = Tokenizer::from_json(r#"{"model": {"type": "BPE"}}"#)
            .err()
            .map(|e| e.to_string());
        assert!(err.is_some_and(|e| e.contains("Unigram")));
    }
}
