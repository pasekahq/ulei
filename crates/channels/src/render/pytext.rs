// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Character classes and scanning helpers with the semantics of the reference's string and
//! regex primitives (`str.isspace`, `str.strip`, `\s`, `\w`), shared by the render passes.

/// Whitespace as `str.isspace` and regex `\s` see it: Unicode `White_Space` plus the
/// information separators U+001C..U+001F.
pub(super) fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Word character as regex `\w` sees it (alphanumeric or `_`).
pub(super) fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `str.strip()`.
pub(super) fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `str.lstrip()`.
pub(super) fn lstrip(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

/// End of the line that contains byte `at`: the next `\n` or the end of `s`.
pub(super) fn line_end(s: &str, at: usize) -> usize {
    s[at..].find('\n').map_or(s.len(), |k| at + k)
}

/// True when byte `at` starts a line (multiline `^`).
pub(super) fn at_line_start(s: &str, at: usize) -> bool {
    at == 0 || s.as_bytes()[at - 1] == b'\n'
}

/// Left-to-right substitution in the manner of `re.sub`: at every char boundary `matcher`
/// may claim the non-empty span `at..end` and its replacement; unclaimed chars are copied.
pub(super) fn substitute(
    text: &str,
    mut matcher: impl FnMut(&str, usize) -> Option<(usize, String)>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(c) = text[at..].chars().next() {
        match matcher(text, at) {
            Some((end, replacement)) if end > at => {
                out.push_str(&replacement);
                at = end;
            }
            _ => {
                out.push(c);
                at += c.len_utf8();
            }
        }
    }
    out
}

/// Lazy `D(.+?)D` at `at` for an ASCII delimiter `D`: the group is at least one char,
/// stays on one line and ends at the first following `D`. Returns `(group_end, match_end)`.
pub(super) fn lazy_delimited(text: &str, at: usize, delim: &str) -> Option<(usize, usize)> {
    if !text[at..].starts_with(delim) {
        return None;
    }
    let start = at + delim.len();
    let first = text[start..].chars().next().filter(|&c| c != '\n')?;
    let from = start + first.len_utf8();
    let eol = line_end(text, start);
    if from > eol {
        return None;
    }
    let close = from + text[from..eol].find(delim)?;
    Some((close, close + delim.len()))
}

/// Greedy `D([^D<extra>]+)D` at `at` for a single ASCII delimiter `D`: the group runs to the
/// next `D` and must be non-empty and free of `forbidden` chars. Returns `(group_end, match_end)`.
pub(super) fn greedy_delimited(
    text: &str,
    at: usize,
    delim: &str,
    forbidden: &[char],
) -> Option<(usize, usize)> {
    if !text[at..].starts_with(delim) {
        return None;
    }
    let start = at + delim.len();
    let lead = delim.chars().next()?;
    let stop = text[start..].find(|c: char| c == lead || forbidden.contains(&c))?;
    let close = start + stop;
    if stop == 0 || !text[close..].starts_with(delim) {
        return None;
    }
    Some((close, close + delim.len()))
}
