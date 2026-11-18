// Port: THIRD_PARTY_NOTICES.md#ref-agent
// Port: THIRD_PARTY_NOTICES.md#psf
//! Python's `difflib.SequenceMatcher(None, a, b)` with its popular-element junking: the
//! similarity ratio, the opcodes, and `unified_diff` over lines.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::Hash;

/// How a span of `a` turns into a span of `b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tag {
    Replace,
    Delete,
    Insert,
    Equal,
}

/// `a[i1..i2]` becomes `b[j1..j2]` by `tag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Opcode {
    pub(super) tag: Tag,
    pub(super) i1: usize,
    pub(super) i2: usize,
    pub(super) j1: usize,
    pub(super) j2: usize,
}

struct Matcher<'a, T> {
    a: &'a [T],
    b: &'a [T],
    b2j: HashMap<&'a T, Vec<usize>>,
}

impl<'a, T: Eq + Hash> Matcher<'a, T> {
    /// Indexes `b`; from 200 elements on, one occurring more than once per hundred is junk.
    fn new(a: &'a [T], b: &'a [T]) -> Self {
        let mut b2j: HashMap<&T, Vec<usize>> = HashMap::new();
        for (j, item) in b.iter().enumerate() {
            b2j.entry(item).or_default().push(j);
        }
        if b.len() >= 200 {
            let most = b.len() / 100 + 1;
            b2j.retain(|_, at| at.len() <= most);
        }
        Self { a, b, b2j }
    }

    /// The longest common run of `a[alo..ahi]` and `b[blo..bhi]`, earliest in `a` then in
    /// `b`, extended over junk elements.
    fn longest_match(
        &self,
        (alo, ahi): (usize, usize),
        (blo, bhi): (usize, usize),
    ) -> (usize, usize, usize) {
        let (a, b) = (self.a, self.b);
        let (mut best_i, mut best_j, mut best) = (alo, blo, 0);
        let mut lengths: HashMap<usize, usize> = HashMap::new();
        for (i, item) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut next = HashMap::new();
            for &j in self.b2j.get(item).map_or(&[][..], Vec::as_slice) {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j
                    .checked_sub(1)
                    .and_then(|prev| lengths.get(&prev))
                    .copied()
                    .unwrap_or(0)
                    + 1;
                next.insert(j, k);
                if k > best {
                    (best_i, best_j, best) = (i + 1 - k, j + 1 - k, k);
                }
            }
            lengths = next;
        }
        while best_i > alo && best_j > blo && a[best_i - 1] == b[best_j - 1] {
            best_i -= 1;
            best_j -= 1;
            best += 1;
        }
        while best_i + best < ahi && best_j + best < bhi && a[best_i + best] == b[best_j + best] {
            best += 1;
        }
        (best_i, best_j, best)
    }

    /// `get_matching_blocks`: adjacent blocks merged, ending with `(len(a), len(b), 0)`.
    fn matching_blocks(&self) -> Vec<(usize, usize, usize)> {
        let (la, lb) = (self.a.len(), self.b.len());
        let mut queue = vec![(0, la, 0, lb)];
        let mut blocks = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.longest_match((alo, ahi), (blo, bhi));
            if k > 0 {
                blocks.push((i, j, k));
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        crate::sort::sort(&mut blocks);
        let mut merged = Vec::with_capacity(blocks.len() + 1);
        let (mut i1, mut j1, mut k1) = (0, 0, 0);
        for (i2, j2, k2) in blocks {
            if i1 + k1 == i2 && j1 + k1 == j2 {
                k1 += k2;
            } else {
                if k1 > 0 {
                    merged.push((i1, j1, k1));
                }
                (i1, j1, k1) = (i2, j2, k2);
            }
        }
        if k1 > 0 {
            merged.push((i1, j1, k1));
        }
        merged.push((la, lb, 0));
        merged
    }

    fn opcodes(&self) -> Vec<Opcode> {
        let (mut i, mut j) = (0, 0);
        let mut out = Vec::new();
        for (ai, bj, size) in self.matching_blocks() {
            let tag = match (i < ai, j < bj) {
                (true, true) => Some(Tag::Replace),
                (true, false) => Some(Tag::Delete),
                (false, true) => Some(Tag::Insert),
                (false, false) => None,
            };
            if let Some(tag) = tag {
                out.push(Opcode {
                    tag,
                    i1: i,
                    i2: ai,
                    j1: j,
                    j2: bj,
                });
            }
            (i, j) = (ai + size, bj + size);
            if size > 0 {
                out.push(Opcode {
                    tag: Tag::Equal,
                    i1: ai,
                    i2: i,
                    j1: bj,
                    j2: j,
                });
            }
        }
        out
    }
}

/// Ratcliff-Obershelp similarity of two strings by characters, `SequenceMatcher.ratio()`.
pub(super) fn ratio(left: &str, right: &str) -> f64 {
    let a: Vec<char> = left.chars().collect();
    let b: Vec<char> = right.chars().collect();
    let matched: usize = Matcher::new(&a, &b)
        .matching_blocks()
        .iter()
        .map(|(_, _, k)| k)
        .sum();
    let total = a.len() + b.len();
    #[allow(clippy::cast_precision_loss)]
    let ratio = if total == 0 {
        1.0
    } else {
        2.0 * matched as f64 / total as f64
    };
    ratio
}

/// `SequenceMatcher.get_opcodes()` of two character sequences.
pub(super) fn opcodes(a: &[char], b: &[char]) -> Vec<Opcode> {
    Matcher::new(a, b).opcodes()
}

/// `difflib.unified_diff(a, b, fromfile, tofile)` of lines that keep their line ends, joined;
/// empty when the lines are equal.
pub(super) fn unified_diff(a: &[&str], b: &[&str], fromfile: &str, tofile: &str) -> String {
    let mut out = String::new();
    for (n, group) in grouped_opcodes(Matcher::new(a, b).opcodes(), 3)
        .iter()
        .enumerate()
    {
        if n == 0 {
            let _ = write!(out, "--- {fromfile}\n+++ {tofile}\n");
        }
        let (first, last) = (group[0], group[group.len() - 1]);
        let _ = writeln!(
            out,
            "@@ -{} +{} @@",
            unified_range(first.i1, last.i2),
            unified_range(first.j1, last.j2)
        );
        for code in group {
            if code.tag == Tag::Equal {
                for line in &a[code.i1..code.i2] {
                    out.push(' ');
                    out.push_str(line);
                }
                continue;
            }
            if matches!(code.tag, Tag::Replace | Tag::Delete) {
                for line in &a[code.i1..code.i2] {
                    out.push('-');
                    out.push_str(line);
                }
            }
            if matches!(code.tag, Tag::Replace | Tag::Insert) {
                for line in &b[code.j1..code.j2] {
                    out.push('+');
                    out.push_str(line);
                }
            }
        }
    }
    out
}

/// `get_grouped_opcodes(n)`: the changes with up to `n` lines of context, split where more
/// than `2 * n` equal lines separate them.
fn grouped_opcodes(mut codes: Vec<Opcode>, n: usize) -> Vec<Vec<Opcode>> {
    if codes.is_empty() {
        codes.push(Opcode {
            tag: Tag::Equal,
            i1: 0,
            i2: 1,
            j1: 0,
            j2: 1,
        });
    }
    if let Some(c) = codes.first_mut().filter(|c| c.tag == Tag::Equal) {
        c.i1 = c.i1.max(c.i2.saturating_sub(n));
        c.j1 = c.j1.max(c.j2.saturating_sub(n));
    }
    if let Some(c) = codes.last_mut().filter(|c| c.tag == Tag::Equal) {
        c.i2 = c.i2.min(c.i1 + n);
        c.j2 = c.j2.min(c.j1 + n);
    }
    let mut groups = Vec::new();
    let mut group = Vec::new();
    for mut c in codes {
        if c.tag == Tag::Equal && c.i2 - c.i1 > 2 * n {
            group.push(Opcode {
                i2: c.i2.min(c.i1 + n),
                j2: c.j2.min(c.j1 + n),
                ..c
            });
            groups.push(std::mem::take(&mut group));
            c.i1 = c.i1.max(c.i2.saturating_sub(n));
            c.j1 = c.j1.max(c.j2.saturating_sub(n));
        }
        group.push(c);
    }
    if !(group.is_empty() || group.len() == 1 && group[0].tag == Tag::Equal) {
        groups.push(group);
    }
    groups
}

/// `_format_range_unified`: `start,length` with a 1-based start, the length left out when 1.
fn unified_range(start: usize, stop: usize) -> String {
    let length = stop - start;
    match length {
        1 => format!("{}", start + 1),
        0 => format!("{start},0"),
        _ => format!("{},{length}", start + 1),
    }
}
