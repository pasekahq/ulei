// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `MarkdownV2` -> plain text, the fallback Telegram gets when it rejects a chunk's markup.

use super::markdown::{SPECIAL, html_unescape_emoji, match_custom_emoji};
use super::pytext::{greedy_delimited, is_word, substitute};

/// Strips `MarkdownV2` escapes and formatting markers (custom emoji keep their Unicode
/// fallback) so the text reads cleanly without a parse mode.
pub fn strip_markdown_v2(text: &str) -> String {
    let text = substitute(text, |t, at| {
        match_custom_emoji(t, at).map(|(end, _, fallback)| (end, html_unescape_emoji(fallback)))
    });
    let text = substitute(&text, |t, at| {
        custom_emoji_link(t, at).map(|(end, fallback)| (end, fallback.to_owned()))
    });
    let text = substitute(&text, |t, at| {
        let mut rest = t[at..].chars();
        let escaped = rest.next().filter(|&c| c == '\\').and(rest.next());
        escaped
            .filter(|c| SPECIAL.contains(c))
            .map(|c| (at + 1 + c.len_utf8(), c.to_string()))
    });
    let text = unwrap_pass(&text, "**", |t, at| greedy_delimited(t, at, "**", &[]));
    let text = unwrap_pass(&text, "*", |t, at| greedy_delimited(t, at, "*", &[]));
    let text = unwrap_pass(&text, "_", |t, at| {
        let before = t[..at].chars().next_back();
        let (group_end, end) = greedy_delimited(t, at, "_", &[])?;
        let after = t[end..].chars().next();
        (!before.is_some_and(is_word) && !after.is_some_and(is_word)).then_some((group_end, end))
    });
    let text = unwrap_pass(&text, "~", |t, at| greedy_delimited(t, at, "~", &[]));
    unwrap_pass(&text, "||", |t, at| greedy_delimited(t, at, "||", &[]))
}

/// `open(group)close` -> `group`.
fn unwrap_pass(
    text: &str,
    open: &str,
    matcher: impl Fn(&str, usize) -> Option<(usize, usize)>,
) -> String {
    substitute(text, |t, at| {
        matcher(t, at).map(|(group_end, end)| (end, t[at + open.len()..group_end].to_owned()))
    })
}

/// `![fallback](tg://emoji?id=DIGITS)`: returns the match end and the fallback.
fn custom_emoji_link(text: &str, at: usize) -> Option<(usize, &str)> {
    let rest = text[at..].strip_prefix("![")?;
    let fallback_len = rest.find(']')?;
    let tail = rest[fallback_len..].strip_prefix("](tg://emoji?id=")?;
    let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
    let tail = tail[digits..].strip_prefix(')').filter(|_| digits > 0)?;
    Some((text.len() - tail.len(), &rest[..fallback_len]))
}
