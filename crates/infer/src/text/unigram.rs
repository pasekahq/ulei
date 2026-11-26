// Port: THIRD_PARTY_NOTICES.md#hf-tokenizers
//! The Unigram model: the vocabulary's pieces sorted by their bytes and read in place, and the
//! Viterbi segmentation of the reference `tokenizers` library (its `encode_optimized`, a port
//! of `SentencePiece`'s).
//!
//! The pieces lie in four arrays of the weight file, all in byte order of the pieces: the
//! pieces' bytes back to back, `n + 1` offsets into them, each piece's id and its score. A
//! common-prefix search narrows a range of that order byte by byte with two binary searches;
//! nothing is built at load.

use super::corrupt;
use crate::Result;

/// The pieces, borrowed from the weight file.
#[derive(Debug, Clone, Copy)]
pub(super) struct Pieces<'f> {
    pub(super) bytes: &'f [u8],
    pub(super) offsets: &'f [u32],
    pub(super) ids: &'f [u32],
    pub(super) scores: &'f [f64],
}

/// One cell of the Viterbi table: the best path ending at this byte.
#[derive(Debug, Clone, Copy)]
struct Best {
    score: f64,
    start: usize,
    id: u32,
    set: bool,
}

impl<'f> Pieces<'f> {
    /// Number of pieces.
    pub(super) fn len(&self) -> usize {
        self.ids.len()
    }

    fn key(&self, i: usize) -> Result<&'f [u8]> {
        let at = |j: usize| {
            self.offsets
                .get(j)
                .map(|&o| o as usize)
                .ok_or_else(|| corrupt("piece index out of range"))
        };
        self.bytes
            .get(at(i)?..at(i + 1)?)
            .ok_or_else(|| corrupt("piece offsets out of order or out of range"))
    }

    /// First index in `lo..hi` whose key's byte `d` is not below `c` (or, with `above`, is
    /// above `c`). Every key in the range is longer than `d` and they agree on bytes `..d`.
    fn bound(&self, mut lo: usize, mut hi: usize, d: usize, c: u8, above: bool) -> Result<usize> {
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let b = self
                .key(mid)?
                .get(d)
                .copied()
                .ok_or_else(|| corrupt("pieces out of order"))?;
            if b < c || (above && b == c) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// Calls `f(len, index)` for every piece that is a prefix of `s`, shortest first.
    fn prefixes(&self, s: &[u8], mut f: impl FnMut(usize, usize)) -> Result<()> {
        let (mut lo, mut hi) = (0, self.len());
        for d in 0..=s.len() {
            // Here the keys in lo..hi are those that start with s[..d], in byte order: a key
            // of exactly d bytes, if any, comes first.
            if lo < hi && self.key(lo)?.len() == d {
                if d > 0 {
                    f(d, lo);
                }
                lo += 1;
            }
            if d == s.len() || lo >= hi {
                break;
            }
            let c = s[d];
            let first = self.bound(lo, hi, d, c, false)?;
            hi = self.bound(first, hi, d, c, true)?;
            lo = first;
        }
        Ok(())
    }

    /// The index of the piece equal to `s`.
    fn find(&self, s: &[u8]) -> Result<Option<usize>> {
        let mut hit = None;
        self.prefixes(s, |len, i| {
            if len == s.len() {
                hit = Some(i);
            }
        })?;
        Ok(hit)
    }

    fn id(&self, i: usize) -> Result<u32> {
        self.ids
            .get(i)
            .copied()
            .ok_or_else(|| corrupt("piece id out of range"))
    }

    /// Appends the ids of the best segmentation of `word` to `out`: every piece that starts at
    /// a character boundary is a candidate, a character without a piece of its own length is
    /// an unknown scored `unk_score`; ties keep the earlier candidate; runs of unknowns fuse into
    /// one, whose id is the piece equal to the run's text if there is one, else `unk_id`.
    pub(super) fn viterbi(
        &self,
        word: &str,
        unk_id: u32,
        unk_score: f64,
        out: &mut Vec<u32>,
    ) -> Result<()> {
        let s = word.as_bytes();
        let empty = Best {
            score: 0.0,
            start: 0,
            id: 0,
            set: false,
        };
        let mut best = vec![empty; s.len() + 1];
        for (start, c) in word.char_indices() {
            let base = best[start].score;
            let width = c.len_utf8();
            let mut single = false;
            let mut relax = |end: usize, score: f64, id: u32| {
                let cand = score + base;
                let cell = &mut best[end];
                if !cell.set || cand > cell.score {
                    *cell = Best {
                        score: cand,
                        start,
                        id,
                        set: true,
                    };
                }
            };
            let mut found = Vec::new();
            self.prefixes(&s[start..], |len, i| found.push((len, i)))?;
            for (len, i) in found {
                let score = self
                    .scores
                    .get(i)
                    .copied()
                    .ok_or_else(|| corrupt("piece score out of range"))?;
                relax(start + len, score, self.id(i)?);
                single |= len == width;
            }
            if !single {
                relax(start + width, unk_score, unk_id);
            }
        }
        // Back from the end; spans in reverse order, consecutive unknowns fused.
        let mut spans: Vec<(usize, usize, Option<u32>)> = Vec::new();
        let mut end = s.len();
        while end > 0 {
            let cell = best[end];
            if !cell.set || cell.start >= end {
                return Err(corrupt("no segmentation"));
            }
            if cell.id == unk_id {
                match spans.last_mut() {
                    Some((from, _, None)) => *from = cell.start,
                    _ => spans.push((cell.start, end, None)),
                }
            } else {
                spans.push((cell.start, end, Some(cell.id)));
            }
            end = cell.start;
        }
        for (from, to, id) in spans.into_iter().rev() {
            let id = match id {
                Some(id) => id,
                None => match self.find(&s[from..to])? {
                    Some(i) => self.id(i)?,
                    None => unk_id,
                },
            };
            out.push(id);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// The four arrays of `vocab` (piece, score; id = position), as the converter sorts them.
    pub(in crate::text) fn arrays(
        vocab: &[(&str, f64)],
    ) -> (Vec<u8>, Vec<u32>, Vec<u32>, Vec<f64>) {
        let mut order: Vec<usize> = (0..vocab.len()).collect();
        order.sort_by(|&a, &b| vocab[a].0.as_bytes().cmp(vocab[b].0.as_bytes()));
        let (mut bytes, mut offsets, mut ids, mut scores) =
            (Vec::new(), vec![0], Vec::new(), Vec::new());
        for i in order {
            bytes.extend_from_slice(vocab[i].0.as_bytes());
            offsets.push(u32::try_from(bytes.len()).unwrap());
            ids.push(u32::try_from(i).unwrap());
            scores.push(vocab[i].1);
        }
        (bytes, offsets, ids, scores)
    }

    const VOCAB: [(&str, f64); 13] = [
        ("<s>", 0.0),
        ("<pad>", 0.0),
        ("</s>", 0.0),
        ("<unk>", 0.0),
        ("\u{2581}", -2.0),
        ("\u{2581}ab", -1.0),
        ("a", -3.0),
        ("b", -3.0),
        ("c", -3.0),
        ("\u{2581}abc", -5.0),
        ("bc", -1.0),
        ("xy", -30.0),
        ("zw", -100.0),
    ];

    fn seg(word: &str) -> Vec<u32> {
        let (bytes, offsets, ids, scores) = arrays(&VOCAB);
        let p = Pieces {
            bytes: &bytes,
            offsets: &offsets,
            ids: &ids,
            scores: &scores,
        };
        let mut out = Vec::new();
        p.viterbi(word, 3, -40.0, &mut out).unwrap();
        out
    }

    #[test]
    fn prefix_search_finds_every_prefix_piece() {
        let (bytes, offsets, ids, scores) = arrays(&VOCAB);
        let p = Pieces {
            bytes: &bytes,
            offsets: &offsets,
            ids: &ids,
            scores: &scores,
        };
        let mut hits = Vec::new();
        p.prefixes("\u{2581}abcd".as_bytes(), |len, i| hits.push((len, ids[i])))
            .unwrap();
        assert_eq!(hits, vec![(3, 4), (5, 5), (6, 9)]);
        assert_eq!(p.find(b"bc").unwrap().map(|i| ids[i]), Some(10));
        assert_eq!(p.find(b"b").unwrap().map(|i| ids[i]), Some(7));
        assert_eq!(p.find(b"bcd").unwrap(), None);
        assert_eq!(p.find(b"").unwrap(), None);
    }

    #[test]
    fn viterbi_picks_the_best_and_fuses_unknowns() {
        // "▁ab"+"c" = -4 beats "▁abc" = -5 and "▁"+"a"+"bc" = -6.
        assert_eq!(seg("\u{2581}abc"), vec![5, 8]);
        // Unknowns fuse; a fused run equal to a piece takes its id.
        assert_eq!(seg("\u{2581}zz"), vec![4, 3]);
        assert_eq!(seg("\u{2581}xy"), vec![4, 11], "xy is a piece of its own");
        // Two unknowns (-80) beat the piece "zw" (-100); the fused run is that piece again.
        assert_eq!(seg("zw"), vec![12]);
        assert_eq!(seg("x"), vec![3]);
        assert_eq!(seg(""), Vec::<u32>::new());
    }

    #[test]
    fn corrupt_pieces_are_errors_not_panics() {
        let (bytes, offsets, ids, scores) = arrays(&VOCAB);
        let mut bad_offsets = offsets.clone();
        bad_offsets[3] = 1000;
        let p = Pieces {
            bytes: &bytes,
            offsets: &bad_offsets,
            ids: &ids,
            scores: &scores,
        };
        let mut out = Vec::new();
        // Some lookups reach the bad offset, some do not; none panics.
        for w in ["\u{2581}abc", "<s>", "bc", "zzz"] {
            let _ = p.viterbi(w, 3, -40.0, &mut out);
        }
        let short = Pieces {
            bytes: &bytes,
            offsets: &offsets,
            ids: &ids,
            scores: &scores[..2],
        };
        assert!(short.viterbi("\u{2581}abc", 3, -40.0, &mut out).is_err());
    }
}
