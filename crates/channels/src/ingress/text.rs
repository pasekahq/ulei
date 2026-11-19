// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Agent-visible text of a message: text or caption with custom emoji ids kept, else the
//! plain text of a rich message.

use std::fmt::Write as _;

use serde_json::Value;

use super::update::{Message, MessageEntity};

/// Text of a message as the agent sees it: `text`, else `caption`, else the rich message.
pub(crate) fn message_plain_text(message: &Message) -> String {
    if let Some(text) = message.text.as_deref().filter(|t| !t.is_empty()) {
        return custom_emoji_text(text, &message.entities);
    }
    if let Some(caption) = message.caption.as_deref().filter(|c| !c.is_empty()) {
        return custom_emoji_text(caption, &message.caption_entities);
    }
    message
        .rich_message
        .as_ref()
        .map(rich_message_text)
        .unwrap_or_default()
}

/// Wraps each `custom_emoji` entity as `<tg-emoji emoji-id="ID">E</tg-emoji>` so the id
/// survives into the agent text. Offsets are UTF-16 code units; broken spans are skipped.
pub(crate) fn custom_emoji_text(text: &str, entities: &[MessageEntity]) -> String {
    if text.is_empty() || entities.is_empty() {
        return text.to_owned();
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut spans: Vec<(usize, usize, &str)> = Vec::new();
    for entity in entities.iter().filter(|e| e.kind == "custom_emoji") {
        let Some(id) = entity.custom_emoji_id.as_deref() else {
            continue;
        };
        let (Ok(start), Ok(len)) = (
            usize::try_from(entity.offset),
            usize::try_from(entity.length),
        ) else {
            continue;
        };
        let decimal = !id.is_empty() && id.chars().all(|c| c.is_ascii_digit());
        if !decimal || len == 0 || start + len > units.len() {
            continue;
        }
        // A span that cuts a surrogate pair does not decode; the reference skips it.
        if String::from_utf16(&units[start..start + len]).is_err()
            || String::from_utf16(&units[..start]).is_err()
        {
            continue;
        }
        spans.push((start, start + len, id));
    }
    spans.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end, id) in spans {
        if start < cursor {
            continue;
        }
        out.push_str(&String::from_utf16_lossy(&units[cursor..start]));
        let emoji = String::from_utf16_lossy(&units[start..end]);
        let _ = write!(
            out,
            "<tg-emoji emoji-id=\"{id}\">{}</tg-emoji>",
            html_escape(&emoji)
        );
        cursor = end;
    }
    out.push_str(&String::from_utf16_lossy(&units[cursor..]));
    out
}

/// HTML escaping with quotes, as the reference's `html.escape`.
fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// Markers of media blocks inside a rich message.
fn rich_media_marker(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "photo" => "[photo]",
        "video" => "[video]",
        "animation" => "[animation]",
        "audio" => "[audio]",
        "voice_note" => "[voice]",
        "document" => "[document]",
        "map" => "[map]",
        "collage" => "[collage]",
        "slideshow" => "[slideshow]",
        "divider" => "---",
        _ => return None,
    })
}

/// Plain text of an inbound rich message: one line per block, empty lines dropped.
pub(crate) fn rich_message_text(rich: &Value) -> String {
    let lines = rich_blocks_to_plain(rich.get("blocks"), 0);
    let kept: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| !l.trim().is_empty())
        .collect();
    kept.join("\n").trim().to_owned()
}

/// Flattens a `RichText` value (string, list or typed object).
fn rich_text_to_plain(node: Option<&Value>) -> String {
    match node {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items.iter().map(|n| rich_text_to_plain(Some(n))).collect(),
        Some(Value::Object(map)) => {
            if map.contains_key("text") {
                return rich_text_to_plain(map.get("text"));
            }
            ["email_address", "phone_number", "url", "expression", "name"]
                .iter()
                .find_map(|k| map.get(*k).and_then(Value::as_str))
                .unwrap_or_default()
                .to_owned()
        }
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.to_owned(),
        Some(Value::Number(n)) => n.to_string(),
    }
}

fn rich_blocks_to_plain(blocks: Option<&Value>, depth: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(Value::Array(blocks)) = blocks else {
        return lines;
    };
    let indent = "  ".repeat(depth);
    for block in blocks {
        let Value::Object(map) = block else {
            let text = rich_text_to_plain(Some(block));
            if !text.is_empty() {
                lines.push(format!("{indent}{text}"));
            }
            continue;
        };
        let kind = map.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind == "list" {
            rich_list_lines(map.get("items"), depth, &indent, &mut lines);
        } else if kind == "table" {
            let rows = map.get("cells").or_else(|| map.get("rows"));
            for row in rows.and_then(Value::as_array).into_iter().flatten() {
                let cells = row
                    .as_array()
                    .map_or_else(|| vec![row], |r| r.iter().collect());
                let texts: Vec<String> = cells
                    .into_iter()
                    .map(|c| match c {
                        Value::Object(cell) => rich_text_to_plain(cell.get("text")),
                        other => rich_text_to_plain(Some(other)),
                    })
                    .collect();
                lines.push(format!("{indent}{}", texts.join(" | ")));
            }
        } else if let Some(marker) = rich_media_marker(kind) {
            let caption = match map.get("caption") {
                Some(Value::Object(c)) => rich_text_to_plain(c.get("text")),
                other => rich_text_to_plain(other),
            };
            lines.push(format!("{indent}{}", format!("{marker} {caption}").trim()));
        } else {
            let prefix = if kind.contains("quot") { "> " } else { "" };
            if map.contains_key("text") {
                let text = rich_text_to_plain(map.get("text"));
                if !text.is_empty() {
                    lines.push(format!("{indent}{prefix}{text}"));
                }
            }
            if let Some(inner @ Value::Array(_)) = map.get("blocks") {
                for line in rich_blocks_to_plain(Some(inner), 0) {
                    lines.push(format!("{indent}{prefix}{}", line.trim()));
                }
            }
            for key in ["summary", "credit"] {
                if map.contains_key(key) {
                    let text = rich_text_to_plain(map.get(key));
                    if !text.is_empty() {
                        lines.push(format!("{indent}{text}"));
                    }
                }
            }
        }
    }
    lines
}

fn rich_list_lines(items: Option<&Value>, depth: usize, indent: &str, lines: &mut Vec<String>) {
    for item in items.and_then(Value::as_array).into_iter().flatten() {
        let Value::Object(item) = item else { continue };
        let mut label = match item.get("label") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => "-".to_owned(),
        };
        if item.get("has_checkbox").is_some_and(truthy) {
            if item.get("is_checked").is_some_and(truthy) {
                "[x]"
            } else {
                "[ ]"
            }
            .clone_into(&mut label);
        }
        let inner = rich_blocks_to_plain(item.get("blocks"), depth + 1);
        match inner.split_first() {
            Some((first, rest)) => {
                lines.push(format!("{indent}{label} {}", first.trim()));
                lines.extend(rest.iter().cloned());
            }
            None => lines.push(format!("{indent}{label}")),
        }
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Plain text Telegram echoes for a replied-to rich message, or `None`.
pub(crate) fn rich_reply_text(rich: &Value) -> Option<String> {
    let text = flatten_rich_blocks(rich.get("blocks"));
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn flatten_rich_inline(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items.iter().map(|v| flatten_rich_inline(Some(v))).collect(),
        Some(Value::Object(map)) => {
            if let Some(text) = map.get("text").filter(|v| !v.is_null()) {
                flatten_rich_inline(Some(text))
            } else if let Some(children) = map.get("children").filter(|v| !v.is_null()) {
                flatten_rich_inline(Some(children))
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

fn flatten_rich_blocks(blocks: Option<&Value>) -> String {
    let Some(Value::Array(blocks)) = blocks else {
        return String::new();
    };
    let mut lines: Vec<String> = Vec::new();
    for block in blocks {
        let Value::Object(map) = block else { continue };
        if map.get("type").and_then(Value::as_str) == Some("list") {
            for item in map
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Value::Object(item) = item else { continue };
                let item_text = flatten_rich_blocks(item.get("blocks"));
                let mut item_lines = item_text.lines();
                let Some(first) = item_lines.next() else {
                    continue;
                };
                let first = match item
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|l| !l.is_empty())
                {
                    Some(label) => format!("{label} {first}").trim().to_owned(),
                    None => first.to_owned(),
                };
                lines.push(first);
                lines.extend(item_lines.map(str::to_owned));
            }
            continue;
        }
        let text = flatten_rich_inline(map.get("text"));
        lines.extend(text.lines().map(str::to_owned));
    }
    let kept: Vec<&str> = lines
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_end())
        .collect();
    kept.join("\n")
}

/// Removes `@botname` mentions (with trailing `,:-` and spaces) from the text; returns the
/// original text when nothing would be left.
pub(crate) fn clean_bot_trigger_text(text: &str, bot_username: Option<&str>) -> String {
    let Some(name) = bot_username.filter(|n| !n.is_empty()) else {
        return text.to_owned();
    };
    let needle: Vec<char> = format!("@{name}").to_lowercase().chars().collect();
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let end = i + needle.len();
        let matches = end <= chars.len()
            && chars[i..end]
                .iter()
                .flat_map(|c| c.to_lowercase())
                .eq(needle.iter().copied())
            && chars.get(end).is_none_or(|c| !is_word_char(*c));
        if matches {
            i = end;
            while chars.get(i).is_some_and(|c| matches!(c, ',' | ':' | '-')) {
                i += 1;
            }
            while chars.get(i).is_some_and(|c| c.is_whitespace()) {
                i += 1;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    let cleaned = out.trim();
    if cleaned.is_empty() {
        text.to_owned()
    } else {
        cleaned.to_owned()
    }
}

/// A slash command for the engine: the `@bot` suffix of the command word is dropped, as
/// `get_command` reads the name, then mentions are cleaned from the rest of the text.
pub(crate) fn clean_command_text(text: &str, bot_username: Option<&str>) -> String {
    let body = text.trim_start();
    let (head, rest) = body.split_at(body.find(char::is_whitespace).unwrap_or(body.len()));
    let name = head.split_once('@').map_or(head, |(name, _)| name);
    clean_bot_trigger_text(&format!("{name}{rest}"), bot_username)
}

/// A word character of the reference's regular expressions (`\w` with Unicode).
pub(crate) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Replaces every character outside `[\w.\- ]` with `_`, as the reference does for file names
/// it shows to the model.
pub(crate) fn sanitize_display_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if is_word_char(c) || matches!(c, '.' | '-' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect()
}
