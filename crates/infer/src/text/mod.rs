// Port: THIRD_PARTY_NOTICES.md#hf-tokenizers
//! Text to token ids (cargo feature `text`, ADR-0086 E2): the `SentencePiece` Unigram tokenizer
//! of the XLM-RoBERTa family, read in place from the model's weight file.
//!
//! The ids are those of the reference, the `tokenizers` library as the `transformers`
//! tokenizer class of the checkpoint drives it, with truncation to the model's maximum:
//!
//! 1. **Added tokens.** The text is split at the literal added tokens (`<s>`, `</s>`, `<pad>`,
//!    `<unk>`, `<mask>`), leftmost first and longest at one position, on the raw text; a token
//!    with `lstrip` / `rstrip` also takes the whitespace before / after it. Each added token is
//!    its id; each text between them is a section, run through steps 2 to 4 on its own.
//! 2. **Normaliser.** The precompiled character map ([`charsmap`]), by extended grapheme
//!    cluster ([`grapheme`]).
//! 3. **Pre-tokeniser**, one of two [`Pipeline`]s:
//!    - [`Pipeline::WhitespaceSplit`] (what `transformers` 5 builds for XLM-RoBERTa, whatever
//!      `tokenizer.json` says): the normalised section is split at every whitespace character,
//!      which is dropped; each word gets `▁` in front unless it starts with one;
//!    - [`Pipeline::CollapseSpaces`] (the pipeline `tokenizer.json` of these checkpoints
//!      writes, which the `tokenizers` library alone runs): runs of two or more spaces become
//!      one, every space becomes `▁`, and the section gets `▁` in front unless it starts with
//!      one.
//!
//!    Either way the text is then split before every `▁`.
//! 4. **Model.** Each piece is segmented by the Unigram Viterbi of [`unigram`].
//! 5. **Post-processor and truncation.** `<s>` and `</s>` around the first `max_tokens - 2`
//!    ids.
//!
//! The work is bounded by the budget, not by the text. Only the first
//! [`Tokenizer::max_text_bytes`] bytes of the text are read (cut at a character boundary):
//! `max(64 KiB, max_tokens x longest piece x 4)`, 96 KiB for e5-small and bge-m3 (512 tokens,
//! pieces of at most 48 bytes). Every id stands for at least one character of a piece of at most
//! the longest piece's bytes, so ordinary text fills the budget long before the cut and its
//! ids are those of the whole text; a pathological text whose content starts past the cut
//! (megabytes of whitespace or of characters the normaliser drops, then the words) gets the
//! ids of its head, which differ from the reference's. Within the cut, sections, characters
//! and pieces are handled in order and the walk stops once `max_tokens - 2` ids exist, which
//! leaves the ids unchanged because nothing before a piece depends on what follows it. One
//! piece (a run without whitespace or `▁`) is segmented whole, as the reference does: its
//! Viterbi table takes 24 bytes per byte of the piece, so at most 24 times the cut.
//!
//! The tokenizer's data is a section of the weight file written by [`convert`]: the character
//! map's trie and strings, and the pieces sorted by their bytes with their ids and scores
//! (tensors `tokenizer.*`), plus `meta.tokenizer` (pipeline, special ids, added tokens,
//! maximum length). Nothing is built or copied at load, so binding costs a few header reads,
//! and a lookup pages in only what it touches.

mod charsmap;
pub mod convert;
mod grapheme;
#[rustfmt::skip]
mod grapheme_table;
mod unigram;

use std::sync::OnceLock;

use serde_json::Value;

use crate::weights::file::WeightFile;
use crate::{Error, Result};
use charsmap::Charsmap;
use unigram::Pieces;

/// Key of the tokenizer's object in the weight file's `meta`.
pub const META_TOKENIZER: &str = "tokenizer";
/// The value of `meta.tokenizer.format` this module reads.
pub const FORMAT: &str = "ulei-infer/unigram/1";
/// The character map's double-array trie (`u32` darts-clone units).
pub const CHARSMAP_TRIE: &str = "tokenizer.charsmap.trie";
/// The character map's normalised strings, each ended by NUL (`u8`).
pub const CHARSMAP_STRINGS: &str = "tokenizer.charsmap.strings";
/// The pieces' bytes back to back, in byte order of the pieces (`u8`).
pub const PIECE_BYTES: &str = "tokenizer.pieces.bytes";
/// `n + 1` offsets into [`PIECE_BYTES`] (`u32`).
pub const PIECE_OFFSETS: &str = "tokenizer.pieces.offsets";
/// The id of each piece (`u32`).
pub const PIECE_IDS: &str = "tokenizer.pieces.ids";
/// The score of each piece (`f64`).
pub const PIECE_SCORES: &str = "tokenizer.pieces.scores";

/// What `▁`-pieces the normalised text is cut into (module docs, step 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pipeline {
    /// Split at whitespace, `▁` before each word: the `transformers` tokenizer class.
    WhitespaceSplit,
    /// Collapse runs of spaces, spaces to `▁`, `▁` before each section: `tokenizer.json`.
    CollapseSpaces,
}

impl Pipeline {
    /// `whitespace-split` or `collapse-spaces`.
    ///
    /// # Errors
    /// [`Error::Config`] for any other name.
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "whitespace-split" => Ok(Self::WhitespaceSplit),
            "collapse-spaces" => Ok(Self::CollapseSpaces),
            other => Err(Error::Config(format!(
                "tokenizer pipeline {other:?}: expected whitespace-split or collapse-spaces"
            ))),
        }
    }

    /// The name [`Pipeline::parse`] reads.
    pub fn name(self) -> &'static str {
        match self {
            Self::WhitespaceSplit => "whitespace-split",
            Self::CollapseSpaces => "collapse-spaces",
        }
    }
}

fn corrupt(what: &str) -> Error {
    Error::Weights(format!("tokenizer section: {what}"))
}

/// One added token as the split of step 1 uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Added {
    content: String,
    id: u32,
    lstrip: bool,
    rstrip: bool,
}

/// The tokenizer bound to a weight file's tokenizer section.
#[derive(Debug)]
pub struct Tokenizer<'f> {
    pipeline: Pipeline,
    charsmap: Charsmap<'f>,
    pieces: Pieces<'f>,
    added: Vec<Added>,
    vocab_size: usize,
    unk_id: u32,
    bos_id: u32,
    eos_id: u32,
    unk_score: f64,
    max_tokens: usize,
    /// Bytes of the longest piece, found at the first encode (one pass over the offsets).
    longest: OnceLock<usize>,
}

/// Penalty of an unknown character below the lowest piece score, as in the reference.
const UNK_PENALTY: f64 = 10.0;

impl<'f> Tokenizer<'f> {
    /// Binds the tokenizer section of `file`: reads `meta.tokenizer` and checks the section's
    /// array lengths and end offsets (no pass over the arrays).
    ///
    /// # Errors
    /// [`Error::Weights`] if the section is missing, of another format, or its sizes disagree.
    pub fn from_file(file: &'f WeightFile) -> Result<Self> {
        let meta = file
            .meta()
            .get(META_TOKENIZER)
            .ok_or_else(|| corrupt("missing (convert the model again, ADR-0086 E2)"))?;
        let format = meta.get("format").and_then(Value::as_str);
        if format != Some(FORMAT) {
            return Err(corrupt(&format!("format {format:?}, expected {FORMAT:?}")));
        }
        let num = |key: &str| {
            meta.get(key)
                .and_then(Value::as_u64)
                .ok_or_else(|| corrupt(&format!("`{key}` is missing")))
        };
        let id = |key: &str| {
            num(key)
                .and_then(|v| u32::try_from(v).map_err(|_| corrupt(&format!("`{key}` too large"))))
        };
        let pipeline = meta
            .get("pipeline")
            .and_then(Value::as_str)
            .ok_or_else(|| corrupt("`pipeline` is missing"))
            .and_then(|p| Pipeline::parse(p).map_err(|e| corrupt(&e.to_string())))?;
        let vocab_size =
            usize::try_from(num("vocab_size")?).map_err(|_| corrupt("vocabulary too large"))?;
        let max_tokens =
            usize::try_from(num("max_tokens")?).map_err(|_| corrupt("`max_tokens` too large"))?;
        let min_score = f64::from_bits(num("min_score_bits")?);
        let (unk_id, bos_id, eos_id) = (id("unk_id")?, id("bos_id")?, id("eos_id")?);
        let mut added = Vec::new();
        for a in meta
            .get("added")
            .and_then(Value::as_array)
            .ok_or_else(|| corrupt("`added` is missing"))?
        {
            let flag = |k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
            let content = a
                .get("content")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .ok_or_else(|| corrupt("an added token has no content"))?;
            let id = a
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| corrupt("an added token has no id"))?;
            added.push(Added {
                content: content.to_owned(),
                id,
                lstrip: flag("lstrip"),
                rstrip: flag("rstrip"),
            });
        }
        let in_vocab = |id: u32| (id as usize) < vocab_size;
        if !(in_vocab(unk_id)
            && in_vocab(bos_id)
            && in_vocab(eos_id)
            && added.iter().all(|a| in_vocab(a.id)))
        {
            return Err(corrupt("a special id is outside the vocabulary"));
        }
        if max_tokens < 2 || !min_score.is_finite() {
            return Err(corrupt(
                "`max_tokens` below 2 or a non-finite minimum score",
            ));
        }
        let charsmap = Charsmap {
            units: file.u32(CHARSMAP_TRIE)?.data,
            normalized: file.u8(CHARSMAP_STRINGS)?.data,
        };
        let pieces = Pieces {
            bytes: file.u8(PIECE_BYTES)?.data,
            offsets: file.u32(PIECE_OFFSETS)?.data,
            ids: file.u32(PIECE_IDS)?.data,
            scores: file.f64(PIECE_SCORES)?.data,
        };
        let n = pieces.ids.len();
        if n == 0
            || pieces.scores.len() != n
            || pieces.offsets.len() != n + 1
            || pieces.offsets.first() != Some(&0)
            || pieces.offsets.last().map(|&o| o as usize) != Some(pieces.bytes.len())
        {
            return Err(corrupt("piece arrays of different lengths"));
        }
        Ok(Self {
            pipeline,
            charsmap,
            pieces,
            added,
            vocab_size,
            unk_id,
            bos_id,
            eos_id,
            unk_score: min_score - UNK_PENALTY,
            max_tokens,
            longest: OnceLock::new(),
        })
    }

    /// The most ids [`Tokenizer::encode`] returns, `<s>` and `</s>` included.
    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    /// The bytes of a text [`Tokenizer::encode`] reads: `max(64 KiB, max_tokens x the longest
    /// piece's bytes x 4)` (module docs). The first call reads the pieces' offsets once.
    pub fn max_text_bytes(&self) -> usize {
        const FLOOR: usize = 64 * 1024;
        let longest = *self.longest.get_or_init(|| {
            self.pieces
                .offsets
                .windows(2)
                .map(|w| w[1].saturating_sub(w[0]) as usize)
                .max()
                .unwrap_or(0)
        });
        self.max_tokens
            .saturating_mul(longest)
            .saturating_mul(4)
            .max(FLOOR)
    }

    /// Every id is below this.
    pub fn vocab_size(&self) -> usize {
        self.vocab_size
    }

    /// The pipeline the section was written for.
    pub fn pipeline(&self) -> Pipeline {
        self.pipeline
    }

    /// The token ids of `text` (already prefixed, if the model wants a prefix): `<s>`, at most
    /// `max_tokens - 2` piece ids, `</s>`. Only the first [`Tokenizer::max_text_bytes`] bytes
    /// of `text` are read, cut at a character boundary: for ordinary text the ids are those of
    /// the whole text, but a pathological one (megabytes of whitespace before its content, say)
    /// may get other ids than the uncut text would (module docs).
    ///
    /// # Errors
    /// [`Error::Weights`] if the walk meets a corrupt part of the section (an index out of
    /// range, a string that is not UTF-8, an id outside the vocabulary).
    pub fn encode(&self, text: &str) -> Result<Vec<u32>> {
        let mut cut = self.max_text_bytes().min(text.len());
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let text = &text[..cut];
        let budget = self.max_tokens - 2;
        let mut walk = Walk {
            tok: self,
            out: vec![self.bos_id],
            limit: budget + 1,
            piece: String::new(),
            word_start: true,
            prev_space: false,
        };
        for (section, added) in self.sections(text) {
            if walk.full() {
                break;
            }
            if let Some(id) = added {
                walk.out.push(id);
                continue;
            }
            walk.word_start = true;
            walk.prev_space = false;
            if self.charsmap.normalize(section, &mut |c| walk.push(c))? {
                walk.flush()?;
            }
        }
        let mut ids = walk.out;
        ids.truncate(budget + 1);
        ids.push(self.eos_id);
        if ids.iter().any(|&id| id as usize >= self.vocab_size) {
            return Err(corrupt("a piece id is outside the vocabulary"));
        }
        Ok(ids)
    }

    /// The text split at the added tokens: each part with the added token's id, or `None` for
    /// text between them. Empty texts are left out.
    fn sections<'t>(&self, text: &'t str) -> Vec<(&'t str, Option<u32>)> {
        let mut parts = Vec::new();
        let mut done = 0;
        let mut at = 0;
        while at < text.len() {
            // Leftmost match first; at one position the longest added token.
            let hit = self
                .added
                .iter()
                .filter(|a| text[at..].starts_with(a.content.as_str()))
                .max_by_key(|a| a.content.len());
            let Some(a) = hit else {
                at += text[at..].chars().next().map_or(1, char::len_utf8);
                continue;
            };
            let (mut start, mut stop) = (at, at + a.content.len());
            if a.lstrip {
                let kept = text[..start].trim_end_matches(char::is_whitespace).len();
                start = kept.max(done);
            }
            if a.rstrip {
                stop +=
                    text[stop..].len() - text[stop..].trim_start_matches(char::is_whitespace).len();
            }
            if done < start {
                parts.push((&text[done..start], None));
            }
            parts.push((&text[start..stop], Some(a.id)));
            done = stop;
            at = stop;
        }
        if done < text.len() {
            parts.push((&text[done..], None));
        }
        parts
    }
}

/// The state of one [`Tokenizer::encode`]: the ids so far and the piece being collected.
struct Walk<'t, 'f> {
    tok: &'t Tokenizer<'f>,
    out: Vec<u32>,
    limit: usize,
    piece: String,
    /// The next character starts a word (`WhitespaceSplit`) or the section.
    word_start: bool,
    /// The previous character was a space (`CollapseSpaces`).
    prev_space: bool,
}

impl Walk<'_, '_> {
    fn full(&self) -> bool {
        self.out.len() >= self.limit
    }

    /// Segments the collected piece.
    fn flush(&mut self) -> Result<()> {
        if !self.piece.is_empty() {
            let t = self.tok;
            t.pieces
                .viterbi(&self.piece, t.unk_id, t.unk_score, &mut self.out)?;
            self.piece.clear();
        }
        Ok(())
    }

    /// Takes one normalised character; `false` once the budget is full.
    fn push(&mut self, c: char) -> Result<bool> {
        const SPACE: char = '\u{2581}';
        let c = match self.tok.pipeline {
            Pipeline::WhitespaceSplit => {
                if c.is_whitespace() {
                    self.flush()?;
                    self.word_start = true;
                    return Ok(!self.full());
                }
                c
            }
            Pipeline::CollapseSpaces => {
                if c == ' ' {
                    if self.prev_space {
                        return Ok(true);
                    }
                    self.prev_space = true;
                    SPACE
                } else {
                    self.prev_space = false;
                    c
                }
            }
        };
        if self.word_start {
            self.word_start = false;
            if c != SPACE {
                self.flush()?;
                self.piece.push(SPACE);
            }
        }
        if c == SPACE {
            self.flush()?;
        }
        self.piece.push(c);
        Ok(!self.full())
    }
}

#[cfg(test)]
pub(crate) mod tests;
