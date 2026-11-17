// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Fuzzy find-and-replace of `patch`: nine matching strategies tried in order, the guards on
//! what a match may write, and the hint for an `old_string` that matches nothing.
//!
//! Spans are byte offsets into the content; the reference counts characters, which picks the
//! same text.

use std::cmp::Reverse;
use std::fmt::Write as _;

use super::difflib::{self, Tag};
use super::repr::repr;
use super::{is_py_space, py_splitlines, py_strip};

/// `start..end` of a match in the content.
type Span = (usize, usize);

type Strategy = fn(&str, &str) -> Vec<Span>;

const UNICODE_MAP: [(char, &str); 23] = [
    ('\u{201c}', "\""),
    ('\u{201d}', "\""),
    ('\u{2018}', "'"),
    ('\u{2019}', "'"),
    ('\u{2014}', "--"),
    ('\u{2013}', "-"),
    ('\u{2026}', "..."),
    ('\u{a0}', " "),
    ('\u{2212}', "-"),
    ('\u{2000}', " "),
    ('\u{2001}', " "),
    ('\u{2002}', " "),
    ('\u{2003}', " "),
    ('\u{2004}', " "),
    ('\u{2005}', " "),
    ('\u{2006}', " "),
    ('\u{2007}', " "),
    ('\u{2008}', " "),
    ('\u{2009}', " "),
    ('\u{200a}', " "),
    ('\u{202f}', " "),
    ('\u{205f}', " "),
    ('\u{3000}', " "),
];

const IDENTICAL_STRINGS_ERROR: &str = "No edit was applied because old_string and new_string are \
                                       identical. Provide the existing text to replace in \
                                       old_string and the changed replacement text in new_string.";

/// The strategies in the order they are tried.
const STRATEGIES: [(&str, Strategy); 9] = [
    ("exact", exact),
    ("line_trimmed", line_trimmed),
    ("whitespace_normalized", whitespace_normalized),
    ("indentation_flexible", indentation_flexible),
    ("escape_normalized", escape_normalized),
    ("trimmed_boundary", trimmed_boundary),
    ("unicode_normalized", unicode_normalized),
    ("block_anchor", block_anchor),
    ("context_aware", context_aware),
];

/// Strategies that accept a region only resembling `old_string`; never applied to several.
const SIMILARITY_STRATEGIES: [&str; 2] = ["block_anchor", "context_aware"];

/// The ASCII stand-in of `c`, if it has one.
fn ascii_of(c: char) -> Option<&'static str> {
    UNICODE_MAP.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

fn unicode_normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match ascii_of(c) {
            Some(ascii) => out.push_str(ascii),
            None => out.push(c),
        }
    }
    out
}

/// The edit is already in `content`: `new_string` of at least 8 non-blank characters is there
/// verbatim, and `old_string` is gone or equal to it.
pub(super) fn is_already_applied(content: &str, old: &str, new: &str) -> bool {
    if new.is_empty() || py_strip(new).chars().count() < 8 || !content.contains(new) {
        return false;
    }
    old == new || !content.contains(old)
}

/// Up to five `  L<line>: <snippet>` rows for the matches, then `  ... and N more`.
fn format_match_locations(content: &str, matches: &[Span]) -> String {
    const CAP: usize = 5;
    let mut rows: Vec<String> = matches
        .iter()
        .take(CAP)
        .map(|&(start, _)| {
            let line_no = content[..start].matches('\n').count() + 1;
            let line_start = content[..start].rfind('\n').map_or(0, |at| at + 1);
            let line_end = content[line_start..]
                .find('\n')
                .map_or(content.len(), |at| line_start + at);
            let snippet = py_strip(&content[line_start..line_end]);
            let snippet = if snippet.chars().count() > 80 {
                let cut: String = snippet.chars().take(77).collect();
                format!("{cut}...")
            } else {
                snippet.to_owned()
            };
            format!("  L{line_no}: {snippet}")
        })
        .collect();
    if matches.len() > CAP {
        rows.push(format!("  ... and {} more", matches.len() - CAP));
    }
    rows.join("\n")
}

/// `content` with `old` replaced by `new` through the first strategy that matches; the error
/// text the model sees when no edit is made.
pub(super) fn fuzzy_find_and_replace(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_string cannot be empty".into());
    }
    if py_strip(old).is_empty() {
        return Err(
            "old_string is only whitespace \u{2014} provide non-blank text to match".into(),
        );
    }
    if old == new {
        return Err(IDENTICAL_STRINGS_ERROR.into());
    }
    for (name, strategy) in STRATEGIES {
        let matches = strategy(content, old);
        debug_assert!(
            matches.iter().all(|&(start, end)| start <= end
                && content.is_char_boundary(start)
                && content.is_char_boundary(end)),
            "strategy {name} returned a span off a character boundary"
        );
        if matches.is_empty() {
            continue;
        }
        if matches.len() > 1 && !replace_all {
            return Err(format!(
                "Found {} matches for old_string. Provide more context to make it unique, or \
                 use replace_all=True. Matches:\n{}",
                matches.len(),
                format_match_locations(content, &matches)
            ));
        }
        if replace_all && matches.len() > 1 && SIMILARITY_STRATEGIES.contains(&name) {
            return Err(format!(
                "Found {} approximate matches via the '{name}' strategy; replace_all only \
                 applies to exact matches. Provide the precise text (whitespace included) so an \
                 exact/line-trimmed match can be made.",
                matches.len()
            ));
        }
        if name != "exact"
            && let Some(drift) = detect_escape_drift(content, &matches, old, new)
        {
            return Err(drift);
        }
        let mut effective = maybe_unescape_new_string(new, content, &matches);
        if name == "unicode_normalized" {
            effective = preserve_unicode_in_replacement(content, &matches, old, &effective);
        }
        let reindent_from = (name != "exact").then_some(old);
        return Ok(apply_replacements(
            content,
            &matches,
            &effective,
            reindent_from,
        ));
    }
    Err("Could not find a match for old_string in the file".into())
}

fn matched_regions(content: &str, matches: &[Span]) -> String {
    matches.iter().map(|&(s, e)| &content[s..e]).collect()
}

/// `\'` or `\"` in both strings but not in the matched file text, or every backslash run of
/// `old_string` twice its file counterpart: arguments escaped once too often in transport.
fn detect_escape_drift(content: &str, matches: &[Span], old: &str, new: &str) -> Option<String> {
    let has_quote_suspects = new.contains("\\'") || new.contains("\\\"");
    if !has_quote_suspects && !old.contains('\\') {
        return None;
    }
    let regions = matched_regions(content, matches);
    if has_quote_suspects {
        for suspect in ["\\'", "\\\""] {
            if new.contains(suspect) && old.contains(suspect) && !regions.contains(suspect) {
                let plain = &suspect[1..];
                return Some(format!(
                    "Escape-drift detected: old_string and new_string contain the literal \
                     sequence {} but the matched region of the file does not. This is almost \
                     always a tool-call serialization artifact where an apostrophe or quote got \
                     prefixed with a spurious backslash. Re-read the file with read_file and \
                     pass old_string/new_string without backslash-escaping {} characters.",
                    repr(suspect),
                    repr(plain)
                ));
            }
        }
    }
    detect_backslash_doubling(&regions, old, new)
}

fn backslash_runs(text: &str) -> Vec<usize> {
    let mut runs = Vec::new();
    let mut n = 0;
    for c in text.chars() {
        if c == '\\' {
            n += 1;
        } else if n > 0 {
            runs.push(n);
            n = 0;
        }
    }
    if n > 0 {
        runs.push(n);
    }
    runs
}

fn detect_backslash_doubling(regions: &str, old: &str, new: &str) -> Option<String> {
    let old_runs = backslash_runs(old);
    let file_runs = backslash_runs(regions);
    if old_runs.is_empty() || file_runs.is_empty() || old_runs.len() != file_runs.len() {
        return None;
    }
    if old_runs == file_runs || old_runs.iter().zip(&file_runs).any(|(o, f)| *o != f * 2) {
        return None;
    }
    if !(file_runs.iter().any(|f| *f >= 2) || file_runs.len() >= 2) {
        return None;
    }
    if backslash_runs(new) == file_runs {
        return None;
    }
    Some(
        "Escape-drift detected: every backslash run in old_string is exactly twice as long as \
         in the matched region of the file (e.g. the file has `\\\\` where old_string has \
         `\\\\\\\\`). The tool-call arguments were JSON-escaped one extra time; applying \
         new_string verbatim would double every backslash in the file. Re-read the file with \
         read_file and resend old_string/new_string with the backslash counts exactly as they \
         appear in the file."
            .into(),
    )
}

/// The leading spaces and tabs of `line`.
fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn first_meaningful_line(text: &str) -> Option<&str> {
    text.split('\n').find(|line| !py_strip(line).is_empty())
}

/// `new` moved from the base indent of `old` onto that of `file_region`, keeping the indent of
/// each line relative to it; blank lines stay as they are.
fn reindent_replacement(file_region: &str, old: &str, new: &str) -> String {
    if new.is_empty() {
        return String::new();
    }
    let (Some(old_first), Some(file_first)) = (
        first_meaningful_line(old),
        first_meaningful_line(file_region),
    ) else {
        return new.to_owned();
    };
    let old_indent = leading_whitespace(old_first);
    let file_indent = leading_whitespace(file_first);
    if old_indent == file_indent {
        return new.to_owned();
    }
    let lines: Vec<String> = new
        .split('\n')
        .map(|line| {
            if py_strip(line).is_empty() {
                line.to_owned()
            } else if let Some(rest) = line.strip_prefix(old_indent) {
                format!("{file_indent}{rest}")
            } else {
                format!("{file_indent}{}", line.trim_start_matches([' ', '\t']))
            }
        })
        .collect();
    lines.join("\n")
}

/// `\t` and `\r` in `new` become a tab and a carriage return where the matched file text holds
/// that control character.
fn maybe_unescape_new_string(new: &str, content: &str, matches: &[Span]) -> String {
    if !new.contains("\\t") && !new.contains("\\r") {
        return new.to_owned();
    }
    let regions = matched_regions(content, matches);
    let mut out = new.to_owned();
    if out.contains("\\t") && regions.contains('\t') {
        out = out.replace("\\t", "\t");
    }
    if out.contains("\\r") && regions.contains('\r') {
        out = out.replace("\\r", "\r");
    }
    out
}

/// `new` with the spans it keeps from `old` taken from the file, so its Unicode punctuation
/// survives an edit sent in ASCII.
fn preserve_unicode_in_replacement(
    content: &str,
    matches: &[Span],
    old: &str,
    new: &str,
) -> String {
    let region = matched_regions(content, matches);
    let norm_old = unicode_normalize(old);
    if norm_old != unicode_normalize(&region) {
        return new.to_owned();
    }
    let file: Vec<char> = region.chars().collect();
    let mut orig_to_norm = Vec::with_capacity(file.len() + 1);
    let mut norm_pos = 0;
    for c in &file {
        orig_to_norm.push(norm_pos);
        norm_pos += ascii_of(*c).map_or(1, |ascii| ascii.chars().count());
    }
    orig_to_norm.push(norm_pos);
    let a: Vec<char> = norm_old.chars().collect();
    let b: Vec<char> = new.chars().collect();
    let mut out = String::with_capacity(new.len());
    for code in difflib::opcodes(&a, &b) {
        match code.tag {
            Tag::Equal => {
                let start = orig_to_norm[..file.len()]
                    .iter()
                    .position(|&n| n == code.i1)
                    .unwrap_or(0);
                let mut end = start;
                while end < file.len() && orig_to_norm[end] < code.i2 {
                    end += 1;
                }
                out.extend(&file[start..end]);
            }
            Tag::Replace | Tag::Insert => out.extend(&b[code.j1..code.j2]),
            Tag::Delete => {}
        }
    }
    out
}

/// Byte offset of character `n` of `text`, its length past the end.
fn char_byte(text: &str, n: usize) -> usize {
    text.char_indices().nth(n).map_or(text.len(), |(at, _)| at)
}

/// `content` with each span replaced from the last to the first, as Python slices strings by
/// character; `reindent_from` re-indents `new` to each matched region.
fn apply_replacements(
    content: &str,
    matches: &[Span],
    new: &str,
    reindent_from: Option<&str>,
) -> String {
    let mut sorted = matches.to_vec();
    sorted.sort_by_key(|&(start, _)| Reverse(start));
    let mut result = content.to_owned();
    for (start, end) in sorted {
        let adjusted = match reindent_from {
            Some(old) => reindent_replacement(&content[start..end], old, new),
            None => new.to_owned(),
        };
        // Overlapping spans index the already edited text by character, as the reference does.
        let head = char_byte(&result, content[..start].chars().count());
        let tail = char_byte(&result, content[..end].chars().count());
        result = format!("{}{adjusted}{}", &result[..head], &result[tail..]);
    }
    result
}

fn exact(content: &str, pattern: &str) -> Vec<Span> {
    if pattern.is_empty() {
        return Vec::new();
    }
    content
        .match_indices(pattern)
        .map(|(at, m)| (at, at + m.len()))
        .collect()
}

/// Start of each line of `lines` and one past the end, each line counted with its `\n`.
fn line_starts(lines: &[&str]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(lines.len() + 1);
    let mut at = 0;
    starts.push(0);
    for line in lines {
        at += line.len() + 1;
        starts.push(at);
    }
    starts
}

/// The span of lines `start..end` without the `\n` after the last.
fn line_span(starts: &[usize], start: usize, end: usize, content_len: usize) -> Span {
    (starts[start], (starts[end] - 1).min(content_len))
}

/// Windows of `content` whose lines, each passed through `norm`, equal the lines of `pattern`
/// passed through it.
fn normalized_line_matches(content: &str, pattern: &str, norm: fn(&str) -> &str) -> Vec<Span> {
    let lines: Vec<&str> = content.split('\n').collect();
    let normalized: Vec<&str> = lines.iter().map(|l| norm(l)).collect();
    let wanted: Vec<&str> = pattern.split('\n').map(norm).collect();
    let starts = line_starts(&lines);
    let Some(last) = normalized.len().checked_sub(wanted.len()) else {
        return Vec::new();
    };
    (0..=last)
        .filter(|&i| normalized[i..i + wanted.len()] == wanted[..])
        .map(|i| line_span(&starts, i, i + wanted.len(), content.len()))
        .collect()
}

fn line_trimmed(content: &str, pattern: &str) -> Vec<Span> {
    normalized_line_matches(content, pattern, py_strip)
}

fn whitespace_normalized(content: &str, pattern: &str) -> Vec<Span> {
    fn collapse(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut in_run = false;
        for c in text.chars() {
            if c == ' ' || c == '\t' {
                if !in_run {
                    out.push(' ');
                }
                in_run = true;
            } else {
                out.push(c);
                in_run = false;
            }
        }
        out
    }
    let normalized = collapse(content);
    let found = exact(&normalized, &collapse(pattern));
    if found.is_empty() {
        return found;
    }
    map_normalized_positions(content, &normalized, &found)
}

fn indentation_flexible(content: &str, pattern: &str) -> Vec<Span> {
    normalized_line_matches(content, pattern, |l| l.trim_start_matches(is_py_space))
}

fn escape_normalized(content: &str, pattern: &str) -> Vec<Span> {
    let unescaped = pattern
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\r", "\r");
    if unescaped == pattern {
        return Vec::new();
    }
    exact(content, &unescaped)
}

fn trimmed_boundary(content: &str, pattern: &str) -> Vec<Span> {
    fn trim_ends(lines: &mut [&str]) {
        if let Some(first) = lines.first_mut() {
            *first = py_strip(first);
        }
        if lines.len() > 1
            && let Some(last) = lines.last_mut()
        {
            *last = py_strip(last);
        }
    }
    let mut wanted: Vec<&str> = pattern.split('\n').collect();
    trim_ends(&mut wanted);
    let lines: Vec<&str> = content.split('\n').collect();
    let starts = line_starts(&lines);
    let Some(last) = lines.len().checked_sub(wanted.len()) else {
        return Vec::new();
    };
    (0..=last)
        .filter(|&i| {
            let mut block = lines[i..i + wanted.len()].to_vec();
            trim_ends(&mut block);
            block == wanted
        })
        .map(|i| line_span(&starts, i, i + wanted.len(), content.len()))
        .collect()
}

fn unicode_normalized(content: &str, pattern: &str) -> Vec<Span> {
    let norm_pattern = unicode_normalize(pattern);
    let norm_content = unicode_normalize(content);
    if norm_content == content && norm_pattern == pattern {
        return Vec::new();
    }
    let mut found = exact(&norm_content, &norm_pattern);
    if found.is_empty() {
        found = line_trimmed(&norm_content, &norm_pattern);
    }
    if found.is_empty() {
        return found;
    }
    // (byte offset in `content`, byte offset in the normalised text) of each character, then
    // of the end.
    let mut orig_to_norm: Vec<(usize, usize)> = Vec::with_capacity(content.len() + 1);
    let mut norm_pos = 0;
    for (at, c) in content.char_indices() {
        orig_to_norm.push((at, norm_pos));
        norm_pos += ascii_of(c).map_or(c.len_utf8(), str::len);
    }
    let chars = orig_to_norm.len();
    orig_to_norm.push((content.len(), norm_pos));
    found
        .into_iter()
        .filter_map(|(norm_start, norm_end)| {
            let first = orig_to_norm[..chars]
                .iter()
                .position(|&(_, n)| n == norm_start)?;
            let mut end = first;
            while end < chars && orig_to_norm[end].1 < norm_end {
                end += 1;
            }
            Some((orig_to_norm[first].0, orig_to_norm[end].0))
        })
        .collect()
}

fn block_anchor(content: &str, pattern: &str) -> Vec<Span> {
    let norm_pattern = unicode_normalize(pattern);
    let norm_content = unicode_normalize(content);
    let wanted: Vec<&str> = norm_pattern.split('\n').collect();
    if wanted.len() < 2 {
        return Vec::new();
    }
    let first = py_strip(wanted[0]);
    let last = py_strip(wanted[wanted.len() - 1]);
    let norm_lines: Vec<&str> = norm_content.split('\n').collect();
    let orig_lines: Vec<&str> = content.split('\n').collect();
    let starts = line_starts(&orig_lines);
    let count = wanted.len();
    let Some(last_start) = norm_lines.len().checked_sub(count) else {
        return Vec::new();
    };
    let candidates: Vec<usize> = (0..=last_start)
        .filter(|&i| {
            py_strip(norm_lines[i]) == first && py_strip(norm_lines[i + count - 1]) == last
        })
        .collect();
    let threshold = if candidates.len() == 1 { 0.50 } else { 0.70 };
    candidates
        .into_iter()
        .filter(|&i| {
            count <= 2
                || difflib::ratio(
                    &norm_lines[i + 1..i + count - 1].join("\n"),
                    &wanted[1..count - 1].join("\n"),
                ) >= threshold
        })
        .map(|i| line_span(&starts, i, i + count, content.len()))
        .collect()
}

fn context_aware(content: &str, pattern: &str) -> Vec<Span> {
    const THRESHOLD: f64 = 0.80;
    let similar = |a: &str, b: &str| a == b || difflib::ratio(a, b) >= THRESHOLD;
    let wanted: Vec<&str> = pattern.split('\n').collect();
    let lines: Vec<&str> = content.split('\n').collect();
    let starts = line_starts(&lines);
    let Some(last_start) = lines.len().checked_sub(wanted.len()) else {
        return Vec::new();
    };
    let first = py_strip(wanted[0]);
    let last = py_strip(wanted[wanted.len() - 1]);
    (0..=last_start)
        .filter(|&i| {
            let block = &lines[i..i + wanted.len()];
            similar(first, py_strip(block[0]))
                && similar(last, py_strip(block[block.len() - 1]))
                && wanted.iter().zip(block).all(|(p, c)| {
                    let p = py_strip(p);
                    p.is_empty() || similar(p, py_strip(c))
                })
        })
        .map(|i| line_span(&starts, i, i + wanted.len(), content.len()))
        .collect()
}

/// Spans in `original` of the `matches` found in its whitespace-collapsed copy `normalized`,
/// by the reference's character walk.
fn map_normalized_positions(original: &str, normalized: &str, matches: &[Span]) -> Vec<Span> {
    let o = original.as_bytes();
    let n = normalized.as_bytes();
    let blank = |b: u8| b == b' ' || b == b'\t';
    let mut orig_to_norm = Vec::with_capacity(o.len());
    let (mut oi, mut ni) = (0, 0);
    while oi < o.len() && ni < n.len() {
        orig_to_norm.push(ni);
        let collapsed = blank(o[oi]) && n[ni] == b' ' && oi + 1 < o.len() && !blank(o[oi + 1]);
        if o[oi] == n[ni] || collapsed {
            ni += 1;
        }
        oi += 1;
    }
    orig_to_norm.resize(o.len(), n.len());
    matches
        .iter()
        .filter_map(|&(norm_start, norm_end)| {
            let start = orig_to_norm.iter().position(|&p| p >= norm_start)?;
            let mut end = orig_to_norm
                .iter()
                .rposition(|&p| p == norm_end - 1)
                .map_or(start + (norm_end - norm_start), |at| at + 1);
            if norm_end < n.len() && n[norm_end - 1] == b' ' {
                while end < o.len() && blank(o[end]) {
                    end += 1;
                }
            }
            Some((start, end.min(o.len())))
        })
        .collect()
}

/// `→` for each leading tab and `·` for each leading space of `line`.
fn visualize_whitespace(line: &str) -> String {
    let lead = leading_whitespace(line);
    let mut out: String = lead
        .chars()
        .map(|c| if c == '\t' { '→' } else { '·' })
        .collect();
    out.push_str(&line[lead.len()..]);
    out
}

/// Up to three numbered sections of `content` most like the first non-blank line of `old`,
/// two lines of context around each, and a whitespace diagnosis when the best line differs
/// only in whitespace; empty when no line is similar enough.
fn find_closest_lines(old: &str, content: &str) -> String {
    const CONTEXT: usize = 2;
    const MAX_RESULTS: usize = 3;
    let old_lines: Vec<&str> = py_splitlines(old, false).collect();
    let content_lines: Vec<&str> = py_splitlines(content, false).collect();
    if old_lines.is_empty() || content_lines.is_empty() {
        return String::new();
    }
    let Some(anchor) = old_lines
        .iter()
        .map(|l| py_strip(l))
        .find(|l| !l.is_empty())
    else {
        return String::new();
    };
    let mut scored: Vec<(f64, usize)> = content_lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let stripped = py_strip(line);
            if stripped.is_empty() {
                return None;
            }
            let ratio = difflib::ratio(anchor, stripped);
            (ratio > 0.3).then_some((ratio, i))
        })
        .collect();
    if scored.is_empty() {
        return String::new();
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(MAX_RESULTS);
    let mut parts = Vec::new();
    let mut seen = Vec::new();
    for &(_, at) in &scored {
        let start = at.saturating_sub(CONTEXT);
        let end = content_lines.len().min(at + old_lines.len() + CONTEXT);
        if seen.contains(&(start, end)) {
            continue;
        }
        seen.push((start, end));
        let rows: Vec<String> = (start..end)
            .map(|i| format!("{:4}| {}", i + 1, content_lines[i]))
            .collect();
        parts.push(rows.join("\n"));
    }
    let mut result = parts.join("\n---\n");
    let best = content_lines[scored[0].1];
    if py_strip(best) == anchor && best != old_lines[0] {
        let _ = write!(
            result,
            "\n\nWhitespace difference detected (→ = tab, · = space):\n  file has: {}\n  you \
             sent: {}\nUse the exact whitespace shown in 'file has'.",
            visualize_whitespace(best),
            visualize_whitespace(old_lines[0])
        );
    }
    result
}

/// `\n\nDid you mean one of these sections?\n` and the closest sections, for an error that
/// says the old text was not found; empty otherwise.
pub(super) fn format_no_match_hint(error: &str, old: &str, content: &str) -> String {
    if !error.starts_with("Could not find") {
        return String::new();
    }
    let hint = find_closest_lines(old, content);
    if hint.is_empty() {
        return hint;
    }
    format!("\n\nDid you mean one of these sections?\n{hint}")
}
