//! Standard base64 with padding (RFC 4648 section 4), the encoding of C10 `bytes_b64`.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `bytes` in standard base64 with padding.
pub(super) fn encode(bytes: &[u8]) -> String {
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
pub(super) fn decode(text: &str) -> Option<Vec<u8>> {
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
