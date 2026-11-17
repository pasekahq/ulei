// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Standard Markdown -> Telegram `MarkdownV2`.
//!
//! Same pass order as the reference: tables, fenced code, inline code, custom emoji, links,
//! headers, bold, italic, strikethrough, spoilers, blockquotes, then escaping of the rest and a
//! safety net for bare parentheses and braces. Converted spans hide behind placeholders so
//! later passes never touch them.

use super::pytext::{
    at_line_start, greedy_delimited, is_space, lazy_delimited, line_end, strip, substitute,
};
use super::tables::tables_to_bullets;

/// Characters `MarkdownV2` requires to be backslash-escaped outside code.
pub(super) const SPECIAL: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!', '\\',
];

/// Escapes every `MarkdownV2` special character with a backslash.
pub fn escape_markdown_v2(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for c in text.chars() {
        if SPECIAL.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Converts the agent's Markdown reply to Telegram `MarkdownV2` text. Empty input stays empty.
pub fn to_markdown_v2(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }
    // NUL is the stash key delimiter. The reference lets literal `\0PH<n>\0` text in a reply
    // (echoed tool output, web content) be expanded by restore, and a few hundred bytes of
    // repeated look-alikes double the output at every level until memory runs out. Telegram
    // shows nothing useful for NUL, so it becomes U+FFFD and no input can name a stash key.
    let content = content.replace('\0', "\u{FFFD}");
    let mut stash = Stash::default();
    let text = tables_to_bullets(&content);
    let text = substitute(&text, |t, at| {
        fenced_code(t, at).map(|(end, value)| (end, stash.put(value)))
    });
    let text = substitute(&text, |t, at| {
        inline_code(t, at).map(|end| (end, stash.put(t[at..end].replace('\\', "\\\\"))))
    });
    let text = substitute(&text, |t, at| {
        match_custom_emoji(t, at).map(|(end, id, fallback)| {
            let value = format!(
                "![{}](tg://emoji?id={id})",
                escape_markdown_v2(&html_unescape_emoji(fallback))
            );
            (end, stash.put(value))
        })
    });
    let text = substitute(&text, |t, at| {
        link(t, at).map(|(end, display, url)| {
            let url = url.replace('\\', "\\\\").replace(')', "\\)");
            (
                end,
                stash.put(format!("[{}]({url})", escape_markdown_v2(display))),
            )
        })
    });
    let text = substitute(&text, |t, at| {
        header(t, at).map(|(end, inner)| {
            let inner = substitute(strip(inner), |s, i| {
                lazy_delimited(s, i, "**").map(|(g, e)| (e, s[i + 2..g].to_owned()))
            });
            (end, stash.put(format!("*{}*", escape_markdown_v2(&inner))))
        })
    });
    let text = wrap_pass(&text, &mut stash, "**", "*", |t, at| {
        lazy_delimited(t, at, "**")
    });
    let text = wrap_pass(&text, &mut stash, "*", "_", |t, at| {
        greedy_delimited(t, at, "*", &['\n'])
    });
    let text = wrap_pass(&text, &mut stash, "~~", "~", |t, at| {
        lazy_delimited(t, at, "~~")
    });
    let text = wrap_pass(&text, &mut stash, "||", "||", |t, at| {
        lazy_delimited(t, at, "||")
    });
    let text = substitute(&text, |t, at| {
        blockquote(t, at).map(|(end, prefix, content)| {
            let value = match content.strip_suffix("||") {
                Some(open) if prefix.starts_with("**") => {
                    format!("{prefix} {}||", escape_markdown_v2(open))
                }
                _ => format!("{prefix} {}", escape_markdown_v2(content)),
            };
            (end, stash.put(value))
        })
    });
    let text = escape_markdown_v2(&text);
    escape_bare_brackets(&stash.restore(&text))
}

/// Converted spans keyed by `\0PH<n>\0` tokens, which escaping leaves intact.
#[derive(Default)]
struct Stash {
    values: Vec<String>,
}

impl Stash {
    fn put(&mut self, value: String) -> String {
        let key = format!("\0PH{}\0", self.values.len());
        self.values.push(value);
        key
    }

    /// Replaces each token once, newest first, as the reference does. Only tokens made by
    /// [`Self::put`] can be present: `to_markdown_v2` replaces NUL in its input first.
    fn restore(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for (n, value) in self.values.iter().enumerate().rev() {
            let key = format!("\0PH{n}\0");
            if text.contains(&key) {
                text = text.replace(&key, value);
            }
        }
        text
    }
}

/// `open(group)close` -> stashed `marker + escaped(group) + marker`.
fn wrap_pass(
    text: &str,
    stash: &mut Stash,
    open: &str,
    marker: &str,
    matcher: impl Fn(&str, usize) -> Option<(usize, usize)>,
) -> String {
    substitute(text, |t, at| {
        matcher(t, at).map(|(group_end, end)| {
            let group = &t[at + open.len()..group_end];
            (
                end,
                stash.put(format!("{marker}{}{marker}", escape_markdown_v2(group))),
            )
        })
    })
}

/// ```` ```[lang\n]body``` ````: returns the match end and its protected form, where `\` and
/// `` ` `` in the body are escaped as `MarkdownV2` requires inside `pre`.
fn fenced_code(text: &str, at: usize) -> Option<(usize, String)> {
    if !text[at..].starts_with("```") {
        return None;
    }
    let after = at + 3;
    let with_lang_line = text[after..].find('\n').and_then(|nl| {
        let body = after + nl + 1;
        text[body..].find("```").map(|k| body + k + 3)
    });
    let end = with_lang_line.or_else(|| text[after..].find("```").map(|k| after + k + 3))?;
    let raw = &text[at..end];
    let open_end = if raw[3..].contains('\n') {
        raw.find('\n').map_or(3, |k| k + 1)
    } else {
        3
    };
    let body = raw.get(open_end..raw.len() - 3).unwrap_or_default();
    let body = body.replace('\\', "\\\\").replace('`', "\\`");
    Some((end, format!("{}{body}```", &raw[..open_end])))
}

/// `` `[^`]+` ``: returns the match end.
fn inline_code(text: &str, at: usize) -> Option<usize> {
    greedy_delimited(text, at, "`", &[]).map(|(_, end)| end)
}

/// `<tg-emoji emoji-id="ID">FALLBACK</tg-emoji>`: returns the match end, id and fallback.
pub(super) fn match_custom_emoji(text: &str, at: usize) -> Option<(usize, &str, &str)> {
    const OPEN: &str = "<tg-emoji emoji-id=\"";
    const CLOSE: &str = "</tg-emoji>";
    let rest = text[at..].strip_prefix(OPEN)?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let id = &rest[..digits];
    let rest = rest[digits..].strip_prefix("\">").filter(|_| digits > 0)?;
    let fallback_len = rest.find(['<', '>'])?;
    let fallback = &rest[..fallback_len];
    let tail = rest[fallback_len..]
        .strip_prefix(CLOSE)
        .filter(|_| fallback_len > 0)?;
    Some((text.len() - tail.len(), id, fallback))
}

/// `[display](url)` where the url holds at most one level of balanced parentheses: returns
/// the match end, display text and url.
fn link(text: &str, at: usize) -> Option<(usize, &str, &str)> {
    let rest = text[at..].strip_prefix('[')?;
    let display_len = rest.find(']')?;
    if display_len == 0 {
        return None;
    }
    let url_start = at + 1 + display_len + 2;
    if !rest[display_len + 1..].starts_with('(') {
        return None;
    }
    let bytes = text.as_bytes();
    let skip = |mut p: usize| {
        while p < bytes.len() && bytes[p] != b'(' && bytes[p] != b')' {
            p += 1;
        }
        p
    };
    let mut p = skip(url_start);
    loop {
        match bytes.get(p) {
            Some(b')') => return Some((p + 1, &rest[..display_len], &text[url_start..p])),
            Some(b'(') => {
                p = skip(p + 1);
                if bytes.get(p) != Some(&b')') {
                    return None;
                }
                p = skip(p + 1);
            }
            _ => return None,
        }
    }
}

/// `^#{1,6}\s+(.+)$` (multiline; `\s+` may run across blank lines): returns the match end and
/// the header text.
fn header(text: &str, at: usize) -> Option<(usize, &str)> {
    if !at_line_start(text, at) {
        return None;
    }
    let hashes = text[at..].bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let ws_start = at + hashes;
    let ws_len = text[ws_start..]
        .find(|c: char| !is_space(c))
        .unwrap_or(text.len() - ws_start);
    if ws_len == 0 {
        return None;
    }
    let ws_end = ws_start + ws_len;
    if ws_end < text.len() {
        let end = line_end(text, ws_end);
        return Some((end, &text[ws_end..end]));
    }
    // Whitespace runs to the end: backtrack `\s+` so `.+` takes a trailing non-newline char.
    let first_len = text[ws_start..].chars().next().map_or(1, char::len_utf8);
    let (start, _) = text[ws_start + first_len..ws_end]
        .char_indices()
        .rev()
        .find(|&(_, c)| c != '\n')?;
    let start = ws_start + first_len + start;
    let end = line_end(text, start);
    Some((end, &text[start..end]))
}

/// `^((?:\*\*)?>{1,3}) (.+)$` (multiline): returns the match end, prefix and content.
fn blockquote(text: &str, at: usize) -> Option<(usize, &str, &str)> {
    if !at_line_start(text, at) {
        return None;
    }
    let stars = if text[at..].starts_with("**>") { 2 } else { 0 };
    let quotes = text[at + stars..]
        .bytes()
        .take(3)
        .take_while(|&b| b == b'>')
        .count();
    if quotes == 0 {
        return None;
    }
    let prefix_end = at + stars + quotes;
    if text.as_bytes().get(prefix_end) != Some(&b' ') {
        return None;
    }
    let end = line_end(text, prefix_end + 1);
    if end == prefix_end + 1 {
        return None;
    }
    Some((end, &text[at..prefix_end], &text[prefix_end + 1..end]))
}

/// Inverse of the HTML escaping the inbound side applies to custom emoji fallbacks: the
/// five named references `&amp; &lt; &gt; &quot; &apos;` and numeric references.
pub(super) fn html_unescape_emoji(text: &str) -> String {
    substitute(text, |t, at| {
        let rest = t[at..].strip_prefix('&')?;
        for (name, value) in [
            ("amp", '&'),
            ("lt", '<'),
            ("gt", '>'),
            ("quot", '"'),
            ("apos", '\''),
        ] {
            if let Some(after) = rest.strip_prefix(name) {
                let semi = usize::from(after.starts_with(';'));
                return Some((at + 1 + name.len() + semi, value.to_string()));
            }
        }
        let num = rest.strip_prefix('#')?;
        let (radix, digits_at) = match num.as_bytes().first() {
            Some(b'x' | b'X') => (16, 1),
            _ => (10, 0),
        };
        let digits = num[digits_at..]
            .bytes()
            .take_while(|b| {
                if radix == 16 {
                    b.is_ascii_hexdigit()
                } else {
                    b.is_ascii_digit()
                }
            })
            .count();
        if digits == 0 {
            return None;
        }
        let body = &num[digits_at..digits_at + digits];
        let value = u32::from_str_radix(body, radix)
            .ok()
            .filter(|&v| v != 0)
            .and_then(char::from_u32)
            .unwrap_or('\u{fffd}');
        let semi = usize::from(num[digits_at + digits..].starts_with(';'));
        Some((at + 2 + digits_at + digits + semi, value.to_string()))
    })
}

/// Safety net: outside code spans, escape `( ) { }` that are neither escaped nor part of a
/// `[text](url)` link.
fn escape_bare_brackets(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 16);
    let mut seg_start = 0;
    let mut i = 0;
    while i < chars.len() {
        if let Some(end) = code_span(&chars, i) {
            escape_segment(&chars[seg_start..i], &mut out);
            out.extend(&chars[i..end]);
            i = end;
            seg_start = end;
        } else {
            i += 1;
        }
    }
    escape_segment(&chars[seg_start..], &mut out);
    out
}

/// ```` ```[\s\S]*?``` ```` or `` `[^`]+` `` at `i`: returns the span end.
fn code_span(chars: &[char], i: usize) -> Option<usize> {
    let fence = |p: usize| chars.get(p..p + 3) == Some(&['`'; 3][..]);
    if fence(i)
        && let Some(close) = (i + 3..chars.len()).find(|&p| fence(p))
    {
        return Some(close + 3);
    }
    if chars[i] != '`' || chars.get(i + 1).is_none_or(|&c| c == '`') {
        return None;
    }
    (i + 2..chars.len())
        .find(|&p| chars[p] == '`')
        .map(|p| p + 1)
}

fn escape_segment(seg: &[char], out: &mut String) {
    let link_open = seg.windows(2).position(|w| w == [']', '(']);
    for (s, &c) in seg.iter().enumerate() {
        if matches!(c, '(' | ')' | '{' | '}') && !keeps_bracket(seg, s, link_open) {
            out.push('\\');
        }
        out.push(c);
    }
}

fn keeps_bracket(seg: &[char], s: usize, link_open: Option<usize>) -> bool {
    let c = seg[s];
    let prev = s.checked_sub(1).map(|p| seg[p]);
    if prev == Some('\\') || (c == '(' && prev == Some(']')) {
        return true;
    }
    // A `)` closes a link url when its matching `(` follows a `]` within 2000 chars.
    if c == ')' && link_open.is_some_and(|k| k + 2 <= s) {
        let mut depth = 0i32;
        for j in (s.saturating_sub(1999)..s).rev() {
            match seg[j] {
                '(' => {
                    depth -= 1;
                    if depth < 0 {
                        return j > 0 && seg[j - 1] == ']';
                    }
                }
                ')' => depth += 1,
                _ => {}
            }
        }
    }
    false
}
