// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Splitting long messages at Telegram's length limit, measured in UTF-16 code units.

use super::pytext::{is_space, lstrip, strip};

/// Telegram's maximum message length in UTF-16 code units.
pub const MAX_MESSAGE_LEN: usize = 4096;

/// Room kept free in every chunk for a ` (NN/NN)` indicator.
const INDICATOR_RESERVE: usize = 10;
const FENCE_CLOSE: &str = "\n```";

/// Length as Telegram counts it: UTF-16 code units (a surrogate pair counts two).
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

fn units(chars: &[char]) -> usize {
    chars.iter().map(|c| c.len_utf16()).sum()
}

/// Splits `content` into chunks of at most `max_len` UTF-16 units, preferring newlines, then
/// spaces, never cutting a char or an inline code span when avoidable. A fenced code block cut
/// by a split is closed and reopened with its language tag; several chunks get ` (i/n)`.
///
/// A budget smaller than one char still emits that char whole rather than dropping it.
pub fn split_message(content: &str, max_len: usize) -> Vec<String> {
    if utf16_len(content) <= max_len {
        return vec![content.to_owned()];
    }
    let all: Vec<char> = content.chars().collect();
    let mut start = 0;
    let mut chunks: Vec<String> = Vec::new();
    // Language tag ("" when none) of a code block the previous chunk left open.
    let mut carry_lang: Option<String> = None;
    while start < all.len() {
        let remaining = &all[start..];
        let prefix = carry_lang
            .as_ref()
            .map_or_else(String::new, |l| format!("```{l}\n"));
        let prefix_len = utf16_len(&prefix);
        let mut headroom =
            max_len.saturating_sub(INDICATOR_RESERVE + prefix_len + utf16_len(FENCE_CLOSE));
        if headroom < 1 {
            headroom = (max_len / 2).max(1);
        }

        let remaining_len = units(remaining);
        if max_len >= INDICATOR_RESERVE && prefix_len + remaining_len <= max_len - INDICATOR_RESERVE
        {
            let body: String = remaining.iter().collect();
            let mut last = prefix + &body;
            if let Some(lang) = carry_lang.take()
                && fence_state(&body, Some(lang)).is_some()
            {
                last.push_str(FENCE_CLOSE);
            }
            chunks.push(last);
            break;
        }

        let cp_limit = chars_within(remaining, headroom);
        let split_at = split_point(remaining, cp_limit);
        let body: String = remaining[..split_at].iter().collect();
        start += split_at;
        while start < all.len() && is_space(all[start]) {
            start += 1;
        }
        let mut chunk = prefix + &body;
        carry_lang = fence_state(&body, carry_lang);
        if carry_lang.is_some() {
            chunk.push_str(FENCE_CLOSE);
        }
        chunks.push(chunk);
    }
    if chunks.len() > 1 {
        let total = chunks.len();
        for (i, chunk) in chunks.iter_mut().enumerate() {
            *chunk = format!("{chunk} ({}/{total})", i + 1);
        }
    }
    chunks
}

/// Largest number of leading chars whose UTF-16 length fits `budget`.
fn chars_within(chars: &[char], budget: usize) -> usize {
    let mut used = 0;
    for (i, c) in chars.iter().enumerate() {
        used += c.len_utf16();
        if used > budget {
            return i;
        }
    }
    chars.len()
}

/// Where to cut `remaining` given that its first `cp_limit` chars fit: the last newline in
/// the second half of that region, else its last space, else the whole region (at least one
/// char); moved back before an inline code span the cut would leave open.
fn split_point(remaining: &[char], cp_limit: usize) -> usize {
    let rfind = |chars: &[char], c: char| chars.iter().rposition(|&x| x == c);
    let region = &remaining[..cp_limit];
    let natural = match rfind(region, '\n') {
        Some(i) if i >= cp_limit / 2 => Some(i),
        _ => rfind(region, ' '),
    };
    let mut split_at = natural.filter(|&i| i >= 1).unwrap_or(cp_limit.max(1));

    let candidate = &remaining[..split_at.min(remaining.len())];
    let ticks = candidate.iter().filter(|&&c| c == '`').count();
    let escaped = candidate.windows(2).filter(|w| w == &['\\', '`']).count();
    if (ticks - escaped) % 2 == 1 {
        let mut last = rfind(candidate, '`');
        while let Some(i) = last.filter(|&i| i > 0 && candidate[i - 1] == '\\') {
            last = rfind(&candidate[..i], '`');
        }
        if let Some(last) = last.filter(|&i| i > 0) {
            let safe = [
                rfind(&candidate[..last], ' '),
                rfind(&candidate[..last], '\n'),
            ]
            .into_iter()
            .flatten()
            .max();
            if let Some(safe) = safe.filter(|&s| s > cp_limit / 4) {
                split_at = safe;
            }
        }
    }
    split_at.min(remaining.len())
}

/// Walks the fence lines of `body`, starting inside a block tagged `open` when given; returns
/// the tag of the block still open at the end.
fn fence_state(body: &str, mut open: Option<String>) -> Option<String> {
    for line in body.split('\n') {
        let stripped = strip(line);
        if let Some(tag) = stripped.strip_prefix("```") {
            open = if open.is_some() {
                None
            } else {
                Some(
                    lstrip(tag)
                        .split(is_space)
                        .next()
                        .unwrap_or_default()
                        .to_owned(),
                )
            };
        }
    }
    open
}

/// The reference's per-chunk post-processing before sending: the ` (i/n)` indicator gets its
/// parentheses escaped for `MarkdownV2` and is moved off a closing code fence line.
pub fn split_markdown_v2(formatted: &str) -> Vec<String> {
    let chunks = split_message(formatted, MAX_MESSAGE_LEN);
    if chunks.len() < 2 {
        return chunks;
    }
    chunks
        .into_iter()
        .map(|c| separate_indicator_from_fence(&escape_indicator(&c)))
        .collect()
}

/// ` (i/n)` at the end -> ` \(i/n\)`.
fn escape_indicator(chunk: &str) -> String {
    match indicator_at_end(chunk) {
        Some((at, i, n)) => format!("{} \\({i}/{n}\\)", &chunk[..at]),
        None => chunk.to_owned(),
    }
}

/// Byte offset of a trailing ` (i/n)` and its two numbers.
fn indicator_at_end(chunk: &str) -> Option<(usize, &str, &str)> {
    let inner = chunk.strip_suffix(')')?;
    let at = inner.rfind(" (")?;
    let (i, n) = inner[at + 2..].split_once('/')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(i) && digits(n)).then_some((at, i, n))
}

/// A line that is exactly ```` ``` \(i/n\) ```` (or unescaped) becomes the fence line followed
/// by the indicator on its own line, so Telegram sees a clean closing fence.
fn separate_indicator_from_fence(chunk: &str) -> String {
    chunk
        .split('\n')
        .map(|line| match line.strip_prefix("``` ") {
            Some(ind) if is_indicator(ind) => format!("```\n{ind}"),
            _ => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `(i/n)` with optionally backslash-escaped parentheses.
fn is_indicator(s: &str) -> bool {
    let s = s.strip_prefix('\\').unwrap_or(s);
    let Some(s) = s.strip_prefix('(') else {
        return false;
    };
    let s = s.strip_suffix(')').unwrap_or_default();
    let s = s.strip_suffix('\\').unwrap_or(s);
    s.split_once('/').is_some_and(|(i, n)| {
        let digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
        digits(i) && digits(n)
    })
}
