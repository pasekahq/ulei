// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The reference's FTS5 query sanitizer, so a query with grammar characters reaches `MATCH`
//! as plain terms instead of failing to parse.

use std::fmt::Write as _;

/// Longest query the sanitizer reads, in chars; the rest is cut off.
const MAX_QUERY_CHARS: usize = 2_048;

/// Characters the FTS5 query grammar rejects outside a quoted phrase.
const SPECIAL: &[char] = &[
    '+', '{', '}', '(', ')', ':', '"', '^', '@', '/', '#', '&', '|', '~', '[', ']', '<', '>', ',',
    ';', '!', '?', '$', '=', '\\', '\'',
];
const OPERATORS: [&str; 3] = ["AND", "OR", "NOT"];

/// `query` made safe for FTS5 `MATCH` as the reference makes it: cut to [`MAX_QUERY_CHARS`],
/// balanced double-quoted phrases kept, every other grammar character and `%` blanked, a
/// leading `*` and dangling `AND`, `OR`, `NOT` dropped, dotted and hyphenated terms quoted.
pub(super) fn fts5_query(query: &str) -> String {
    let query: Vec<char> = query.chars().take(MAX_QUERY_CHARS).collect();
    let mut phrases: Vec<String> = Vec::new();
    let mut text = String::with_capacity(query.len());
    let mut i = 0;
    while i < query.len() {
        if query[i] != '"' {
            text.push(query[i]);
            i += 1;
            continue;
        }
        match query[i + 1..].iter().position(|&c| c == '"') {
            None => {
                text.push(' ');
                i += 1;
            }
            Some(len) => {
                let end = i + 1 + len;
                phrases.push(query[i..=end].iter().collect());
                let _ = write!(text, "\0Q{}\0", phrases.len() - 1);
                i = end + 1;
            }
        }
    }
    let text: String = text
        .chars()
        .map(|c| {
            if SPECIAL.contains(&c) || c == '%' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut text = quote_compounds(&drop_operators(&drop_stars(&text)));
    for (i, phrase) in phrases.iter().enumerate() {
        text = text.replace(&format!("\0Q{i}\0"), phrase);
    }
    py_strip(&text).to_owned()
}

/// Runs of `*` collapsed to one, then a `*` at the start or after whitespace dropped.
fn drop_stars(text: &str) -> String {
    let mut collapsed: Vec<char> = Vec::with_capacity(text.len());
    for c in text.chars() {
        if !(c == '*' && collapsed.last() == Some(&'*')) {
            collapsed.push(c);
        }
    }
    collapsed
        .iter()
        .enumerate()
        .filter(|&(i, &c)| !(c == '*' && (i == 0 || is_py_space(collapsed[i - 1]))))
        .map(|(_, &c)| c)
        .collect()
}

/// `text` trimmed without one leading operator word and one trailing whitespace-led operator.
fn drop_operators(text: &str) -> String {
    let mut text = py_strip(text);
    for op in OPERATORS {
        let rest = text
            .get(..op.len())
            .filter(|head| head.eq_ignore_ascii_case(op))
            .map(|_| &text[op.len()..]);
        if let Some(rest) = rest.filter(|rest| !rest.starts_with(word_char)) {
            text = py_strip(rest);
            break;
        }
    }
    for op in OPERATORS {
        let before = text
            .len()
            .checked_sub(op.len())
            .and_then(|at| Some((text.get(..at)?, text.get(at..)?)))
            .filter(|(_, tail)| tail.eq_ignore_ascii_case(op))
            .map(|(before, _)| before);
        if let Some(before) = before.filter(|before| before.ends_with(is_py_space)) {
            text = before.trim_end_matches(is_py_space);
            break;
        }
    }
    text.to_owned()
}

/// Every word joined to further words by `.`, `_` or `-` wrapped in double quotes, so FTS5
/// reads it as a phrase instead of splitting it into separate terms.
fn quote_compounds(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let word_end = |from: usize| {
        chars[from..]
            .iter()
            .position(|&c| !word_char(c))
            .map_or(chars.len(), |len| from + len)
    };
    let mut out = String::with_capacity(text.len() + 2);
    let mut i = 0;
    while i < chars.len() {
        if !word_char(chars[i]) || (i > 0 && word_char(chars[i - 1])) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut end = word_end(i);
        let mut joined = false;
        while end + 1 < chars.len()
            && matches!(chars[end], '.' | '_' | '-')
            && word_char(chars[end + 1])
        {
            end = word_end(end + 1);
            joined = true;
        }
        let compound: String = chars[i..end].iter().collect();
        if joined {
            let _ = write!(out, "\"{compound}\"");
        } else {
            out.push_str(&compound);
        }
        i = end;
    }
    out
}

/// A character of the reference's `\w`.
fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A character Python's `str.strip` removes.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `text` without the leading and trailing whitespace Python's `str.strip` removes.
fn py_strip(text: &str) -> &str {
    text.trim_matches(is_py_space)
}
