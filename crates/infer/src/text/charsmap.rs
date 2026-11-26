// Port: THIRD_PARTY_NOTICES.md#hf-tokenizers
//! The precompiled character map of a `SentencePiece` model (`nmt_nfkc` and its kin) and the
//! normalisation the reference `tokenizers` library does with it.
//!
//! The map is the model's own blob, split at conversion into its two parts and read in place:
//!
//! - a double-array trie (darts-clone units, `u32`) whose keys are UTF-8 strings and whose
//!   values are byte offsets into
//! - the normalised strings, each ended by a NUL byte.
//!
//! A lookup returns the value of the **shortest** key that is a prefix of the input (the first
//! hit of a common-prefix search, which stops at a NUL byte of the input), as the reference
//! does. Normalisation goes by extended grapheme cluster: a cluster shorter than 6 bytes is
//! looked up whole, and if that finds a key the whole cluster becomes its value (even when the
//! key covers only its first characters); otherwise each character is looked up alone and kept
//! when it has no key.

use super::corrupt;
use super::grapheme::Graphemes;
use crate::Result;

/// The character map, borrowed from the weight file. An empty trie maps nothing.
#[derive(Debug, Clone, Copy)]
pub(super) struct Charsmap<'f> {
    pub(super) units: &'f [u32],
    pub(super) normalized: &'f [u8],
}

fn has_leaf(unit: u32) -> bool {
    (unit >> 8) & 1 == 1
}

fn value(unit: u32) -> usize {
    (unit & 0x7FFF_FFFF) as usize
}

fn label(unit: u32) -> u32 {
    unit & 0x8000_00FF
}

fn offset(unit: u32) -> usize {
    ((unit >> 10) as usize) << ((unit & (1 << 9)) >> 6)
}

impl<'f> Charsmap<'f> {
    fn unit(&self, pos: usize) -> Result<u32> {
        self.units
            .get(pos)
            .copied()
            .ok_or_else(|| corrupt("character map trie index out of range"))
    }

    /// The normalised string of the shortest key that is a prefix of `key`, if any.
    pub(super) fn lookup(&self, key: &[u8]) -> Result<Option<&'f str>> {
        if self.units.is_empty() {
            return Ok(None);
        }
        let mut pos = offset(self.unit(0)?);
        for &c in key {
            if c == 0 {
                break;
            }
            pos ^= usize::from(c);
            let unit = self.unit(pos)?;
            if label(unit) != u32::from(c) {
                return Ok(None);
            }
            pos ^= offset(unit);
            if has_leaf(unit) {
                return self.string_at(value(self.unit(pos)?)).map(Some);
            }
        }
        Ok(None)
    }

    fn string_at(&self, start: usize) -> Result<&'f str> {
        let rest = self
            .normalized
            .get(start..)
            .ok_or_else(|| corrupt("character map value out of range"))?;
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        std::str::from_utf8(&rest[..end]).map_err(|_| corrupt("character map value is not UTF-8"))
    }

    /// Normalises `s` (one section of the input), handing each output character to `out` in
    /// order; stops early when `out` returns `false`. Returns whether it ran to the end.
    pub(super) fn normalize(
        &self,
        s: &str,
        out: &mut impl FnMut(char) -> Result<bool>,
    ) -> Result<bool> {
        for cluster in Graphemes::new(s) {
            if cluster.len() < 6
                && let Some(n) = self.lookup(cluster.as_bytes())?
            {
                for c in n.chars() {
                    if !out(c)? {
                        return Ok(false);
                    }
                }
                continue;
            }
            let mut buf = [0u8; 4];
            for c in cluster.chars() {
                match self.lookup(c.encode_utf8(&mut buf).as_bytes())? {
                    Some(n) => {
                        for c in n.chars() {
                            if !out(c)? {
                                return Ok(false);
                            }
                        }
                    }
                    None => {
                        if !out(c)? {
                            return Ok(false);
                        }
                    }
                }
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Builds a darts-clone double array for `keys` (UTF-8 key, value offset): every node gets
    /// its own block of 256 units, which the format allows and keeps the builder trivial.
    pub(in crate::text) fn build(keys: &[(&str, u32)]) -> Vec<u32> {
        #[derive(Default)]
        struct Node {
            children: std::collections::BTreeMap<u8, usize>,
            value: Option<u32>,
        }
        let mut nodes = vec![Node::default()];
        for (k, v) in keys {
            let mut n = 0;
            for &b in k.as_bytes() {
                let next = nodes.len();
                n = *nodes[n].children.entry(b).or_insert(next);
                if n == next {
                    nodes.push(Node::default());
                }
            }
            nodes[n].value = Some(*v);
        }
        // Node i sits at unit position pos[i]; its children at base(i) ^ label, base(i) = 256 * (i + 1).
        let mut units = vec![0u32; 256 * (nodes.len() + 1)];
        let mut pos = vec![0usize; nodes.len()];
        for i in 0..nodes.len() {
            let base = 256 * (i + 1);
            let off = u32::try_from(pos[i] ^ base).unwrap();
            assert!(off < 1 << 21);
            let leaf = if nodes[i].value.is_some() { 1 << 8 } else { 0 };
            units[pos[i]] = (units[pos[i]] & 0xFF) | (off << 10) | leaf;
            if let Some(v) = nodes[i].value {
                units[base] = 0x8000_0000 | v;
            }
            for (&b, &child) in &nodes[i].children {
                pos[child] = base ^ usize::from(b);
                units[pos[child]] = u32::from(b);
            }
        }
        units
    }

    fn map(keys: &[(&str, &str)]) -> (Vec<u32>, Vec<u8>) {
        let mut normalized = Vec::new();
        let mut entries = Vec::new();
        for (k, v) in keys {
            entries.push((*k, u32::try_from(normalized.len()).unwrap()));
            normalized.extend_from_slice(v.as_bytes());
            normalized.push(0);
        }
        (build(&entries), normalized)
    }

    fn run(m: &Charsmap<'_>, s: &str) -> String {
        let mut out = String::new();
        m.normalize(s, &mut |c| {
            out.push(c);
            Ok(true)
        })
        .unwrap();
        out
    }

    #[test]
    fn shortest_key_wins_and_clusters_go_whole() {
        let (units, normalized) = map(&[
            ("\u{FF21}", "A"),
            ("\u{2460}", "1"),
            ("e\u{301}", "\u{E9}"),
            ("\t", " "),
            ("\u{200B}", ""),
        ]);
        let m = Charsmap {
            units: &units,
            normalized: &normalized,
        };
        assert_eq!(m.lookup("\u{FF21}x".as_bytes()).unwrap(), Some("A"));
        assert_eq!(m.lookup(b"e").unwrap(), None);
        assert_eq!(m.lookup(b"\0\t").unwrap(), None, "a NUL ends the key");
        assert_eq!(run(&m, "\u{FF21}b\te\u{301}\u{200B}"), "Ab \u{E9}");
        // A short cluster whose first character is a key becomes that key's value whole.
        assert_eq!(run(&m, "\u{2460}\u{301}"), "1");
        // A cluster of 6 bytes or more goes character by character.
        assert_eq!(run(&m, "\u{2460}\u{301}\u{301}"), "1\u{301}\u{301}");
        // Early stop.
        let mut n = 0;
        let done = m
            .normalize("abc", &mut |_| {
                n += 1;
                Ok(n < 2)
            })
            .unwrap();
        assert!(!done);
        assert_eq!(n, 2);
    }

    #[test]
    fn corrupt_maps_are_errors() {
        let (units, normalized) = map(&[("x", "yy"), ("a", "b")]);
        let short = Charsmap {
            units: &units[..10],
            normalized: &normalized,
        };
        assert!(short.lookup(b"a").is_err());
        let bad_value = Charsmap {
            units: &units,
            normalized: &[],
        };
        assert!(bad_value.lookup(b"a").is_err());
        let not_utf8 = Charsmap {
            units: &units,
            normalized: &[b'y', b'y', 0, 0xFF, 0],
        };
        assert!(not_utf8.lookup(b"a").is_err());
        let none = Charsmap {
            units: &[],
            normalized: &[],
        };
        assert_eq!(run(&none, "a\u{301}"), "a\u{301}");
    }
}
