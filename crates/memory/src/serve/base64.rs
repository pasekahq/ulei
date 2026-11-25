//! Standard base64 with padding (RFC 4648 section 4), the encoding of C10 `bytes_b64`.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `bytes` in standard base64 with padding.
pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(group >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The bytes of `text` when it matches the C10 `bytes_b64` pattern: whole groups of four from
/// the standard alphabet, padding only in the last group; `None` otherwise.
pub(crate) fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let groups = text.len() / 4;
    for (n, group) in text.chunks(4).enumerate() {
        let pad = match group {
            [.., b'=', b'='] => 2,
            [.., b'='] => 1,
            _ => 0,
        };
        if pad > 0 && n + 1 != groups {
            return None;
        }
        let mut acc = 0u32;
        for &c in &group[..4 - pad] {
            acc = acc << 6 | u32::from(sextet(c)?);
        }
        acc <<= 6 * pad;
        let [_, b0, b1, b2] = acc.to_be_bytes();
        out.extend_from_slice(&[b0, b1, b2][..3 - pad]);
    }
    Some(out)
}

fn sextet(c: u8) -> Option<u8> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}

/// A decoder of standard base64 with padding that takes the text in pieces, so that a long text
/// is never held whole: it keeps at most three characters between two pieces.
#[derive(Debug, Default)]
pub(crate) struct Decoder {
    held: [u8; 4],
    count: usize,
    ended: bool,
}

impl Decoder {
    /// Appends the bytes of `text` to `out`; `None` when the text so far is no prefix of the
    /// C10 `bytes_b64` pattern: a character outside the alphabet, or anything after padding.
    pub(crate) fn feed(&mut self, text: &[u8], out: &mut Vec<u8>) -> Option<()> {
        for &c in text {
            if self.ended {
                return None;
            }
            self.held[self.count] = c;
            self.count += 1;
            if self.count == 4 {
                self.count = 0;
                let pad = match self.held {
                    [_, _, b'=', b'='] => 2,
                    [_, _, _, b'='] => 1,
                    _ => 0,
                };
                let mut acc = 0u32;
                for &c in &self.held[..4 - pad] {
                    acc = acc << 6 | u32::from(sextet(c)?);
                }
                acc <<= 6 * pad;
                let [_, b0, b1, b2] = acc.to_be_bytes();
                out.extend_from_slice(&[b0, b1, b2][..3 - pad]);
                self.ended = pad > 0;
            }
        }
        Some(())
    }

    /// `Some` when the text ended at a whole group of four.
    pub(crate) const fn finish(&self) -> Option<()> {
        if self.count == 0 { Some(()) } else { None }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_decoder_in_pieces_gives_what_decode_gives_and_refuses_what_it_refuses() {
        let bytes: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        for len in [0, 1, 2, 3, 998, 999, 1000] {
            let text = encode(&bytes[..len]);
            for piece in [1, 3, 4, 7, 64] {
                let mut d = Decoder::default();
                let mut out = Vec::new();
                for chunk in text.as_bytes().chunks(piece) {
                    d.feed(chunk, &mut out).unwrap();
                }
                d.finish().unwrap();
                assert_eq!(out, &bytes[..len], "len {len} piece {piece}");
            }
        }
        for bad in ["A", "AB=A", "AA==AAAA", "A!AA", "AAA=AAAA", "=AAA"] {
            let mut d = Decoder::default();
            let mut out = Vec::new();
            let ok = d.feed(bad.as_bytes(), &mut out).is_some() && d.finish().is_some();
            assert!(!ok, "{bad}");
            assert!(decode(bad).is_none(), "{bad}");
        }
    }
}
