// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! ECMA-48 escape sequences removed from command output: CSI with private-mode prefixes and
//! intermediates, OSC ended by BEL or ST, DCS/SOS/PM/APC strings, nF escapes, single-byte
//! Fp/Fe/Fs escapes, the 8-bit CSI and OSC forms and the other C1 controls.

use std::borrow::Cow;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// UTF-8 of U+009C, the 8-bit string terminator.
const ST8: [u8; 2] = [0xc2, 0x9c];

/// `text` without escape sequences; borrowed when it holds no ESC and no C1 control.
///
/// Sequences match leftmost-first in the order listed in the module doc, and an unterminated
/// string sequence falls back to removing its two introducing characters.
pub(super) fn strip_ansi(text: &str) -> Cow<'_, str> {
    if !text.chars().any(|c| c == '\u{1b}' || is_c1(c)) {
        return Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut terminators = Terminators::default();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(c) = text[at..].chars().next() {
        match escape_len(bytes, at, &mut terminators) {
            0 => {
                out.push(c);
                at += c.len_utf8();
            }
            n => at += n,
        }
    }
    Cow::Owned(out)
}

fn is_c1(c: char) -> bool {
    ('\u{80}'..='\u{9f}').contains(&c)
}

/// Bytes of the escape sequence starting at `at`, or 0.
fn escape_len(bytes: &[u8], at: usize, terminators: &mut Terminators) -> usize {
    let rest = &bytes[at..];
    match rest {
        [ESC, next, ..] => {
            let body = at + 2;
            let string = match next {
                b'[' => csi(&bytes[body..]),
                b']' => terminators.find(Kind::Osc, bytes, body),
                b'P' | b'X' | b'^' | b'_' => terminators.find(Kind::St, bytes, body),
                _ => None,
            };
            if let Some(len) = string {
                return 2 + len;
            }
            let intermediates = run(&rest[1..], 0x20..=0x2f);
            match rest.get(1 + intermediates) {
                Some(b) if intermediates > 0 && (0x30..=0x7e).contains(b) => 2 + intermediates,
                _ if (0x30..=0x7e).contains(next) => 2,
                _ => 0,
            }
        }
        [0xc2, 0x9b, ..] => 2 + csi(&rest[2..]).unwrap_or(0),
        [0xc2, 0x9d, ..] => 2 + terminators.find(Kind::Osc8, bytes, at + 2).unwrap_or(0),
        [0xc2, 0x80..=0x9f, ..] => 2,
        _ => 0,
    }
}

/// Parameter, intermediate and final bytes of a CSI body.
fn csi(body: &[u8]) -> Option<usize> {
    let params = run(body, 0x30..=0x3f);
    let intermediates = run(&body[params..], 0x20..=0x2f);
    let end = params + intermediates;
    body.get(end)
        .filter(|b| (0x40..=0x7e).contains(*b))
        .map(|_| end + 1)
}

fn run(bytes: &[u8], range: std::ops::RangeInclusive<u8>) -> usize {
    bytes.iter().take_while(|b| range.contains(b)).count()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// BEL or ESC `\`.
    Osc,
    /// ESC `\`.
    St,
    /// BEL or U+009C.
    Osc8,
}

/// The nearest terminator of each kind found so far, so text full of unterminated strings is
/// scanned once per kind instead of once per string.
#[derive(Debug, Default)]
struct Terminators {
    found: [Option<Search>; 3],
}

/// Where a scan for a terminator started and the position and length of the one it found.
#[derive(Debug, Clone, Copy)]
struct Search {
    from: usize,
    hit: Option<(usize, usize)>,
}

impl Terminators {
    /// Bytes from `from` to the end of the nearest terminator of `kind`.
    fn find(&mut self, kind: Kind, bytes: &[u8], from: usize) -> Option<usize> {
        let slot = &mut self.found[kind as usize];
        let hit = match *slot {
            Some(search) if search.from <= from && search.hit.is_none_or(|(at, _)| at >= from) => {
                search.hit
            }
            _ => {
                let hit = scan(kind, bytes, from);
                *slot = Some(Search { from, hit });
                hit
            }
        };
        hit.map(|(at, len)| at + len - from)
    }
}

/// Position and length of the first terminator of `kind` at or after `from`.
fn scan(kind: Kind, bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..bytes.len()).find_map(|i| {
        let rest = &bytes[i..];
        match kind {
            Kind::Osc | Kind::Osc8 if rest[0] == BEL => Some((i, 1)),
            Kind::Osc | Kind::St if rest.starts_with(&[ESC, b'\\']) => Some((i, 2)),
            Kind::Osc8 if rest.starts_with(&ST8) => Some((i, 2)),
            _ => None,
        }
    })
}
