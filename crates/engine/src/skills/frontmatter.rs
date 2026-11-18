// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The frontmatter of a skill file: the YAML between a leading `---` and the next `---` line,
//! read in the subset of YAML that skill files use (ADR-0016). A block outside the subset is
//! read by the reference's own fallback, one `key: value` per line.

use crate::tools::{is_py_space, py_strip};

/// A frontmatter value; scalars other than null stay text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Yaml {
    /// An empty value, `~` or `null`.
    Null,
    /// A plain, quoted or block scalar.
    Str(String),
    /// A block or flow sequence.
    List(Vec<Yaml>),
    /// A block mapping, keys in file order.
    Map(Mapping),
}

/// A mapping in file order; a repeated key keeps its first place and its last value.
pub type Mapping = Vec<(String, Yaml)>;

/// The frontmatter of `content` and the body after it. Without a leading `---`, a closing
/// `---` line or a mapping between them, the frontmatter is empty.
pub fn parse(content: &str) -> (Mapping, &str) {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let Some(rest) = content.strip_prefix("---") else {
        return (Vec::new(), content);
    };
    let Some((start, end)) = closing(rest) else {
        return (Vec::new(), content);
    };
    let block = &rest[..start];
    let frontmatter = subset(block).unwrap_or_else(|| fallback(block));
    (frontmatter, &rest[end..])
}

/// The value of `key` in `mapping`.
pub fn get<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a Yaml> {
    mapping.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Start and end of the first match of `\n---\s*\n` in `text`.
fn closing(text: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(found) = text[from..].find("\n---") {
        let at = from + found;
        let after = at + 4;
        let spaces = text[after..]
            .find(|c: char| !is_py_space(c))
            .map_or(text.len(), |n| after + n);
        if let Some(newline) = text[after..spaces].rfind('\n') {
            return Some((at, after + newline + 1));
        }
        from = at + 1;
    }
    None
}

/// The reference's reading of a block YAML rejects: each line with a `:` is a stripped key and
/// value.
fn fallback(block: &str) -> Mapping {
    let mut mapping = Vec::new();
    for line in py_strip(block).split('\n') {
        if let Some((key, value)) = line.split_once(':') {
            set(
                &mut mapping,
                py_strip(key).to_owned(),
                Yaml::Str(py_strip(value).to_owned()),
            );
        }
    }
    mapping
}

fn set(mapping: &mut Mapping, key: String, value: Yaml) {
    match mapping.iter_mut().find(|(k, _)| *k == key) {
        Some(entry) => entry.1 = value,
        None => mapping.push((key, value)),
    }
}

/// The mapping of the block read in the subset, empty when the block is; `None` when it is
/// outside the subset.
fn subset(block: &str) -> Option<Mapping> {
    if block.contains(['\r', '\u{85}', '\u{2028}', '\u{2029}', '\u{feff}']) {
        return None;
    }
    let mut parser = Parser {
        lines: block.split('\n').collect(),
        at: 0,
    };
    let Some(first) = parser.peek() else {
        return Some(Vec::new());
    };
    let mapping = parser.mapping(first.indent)?;
    parser.peek().is_none().then_some(mapping)
}

/// A content line: its indentation in spaces and the text after it.
#[derive(Clone, Copy)]
struct Line<'a> {
    indent: usize,
    text: &'a str,
}

struct Parser<'a> {
    lines: Vec<&'a str>,
    at: usize,
}

impl<'a> Parser<'a> {
    /// The next line that is neither blank nor a comment, skipping to it.
    fn peek(&mut self) -> Option<Line<'a>> {
        while let Some(raw) = self.lines.get(self.at) {
            let text = raw.trim_start_matches(' ');
            if !text.trim_start_matches('\t').is_empty() && !text.starts_with('#') {
                return Some(Line {
                    indent: raw.len() - text.len(),
                    text,
                });
            }
            self.at += 1;
        }
        None
    }

    fn mapping(&mut self, indent: usize) -> Option<Mapping> {
        let mut mapping = Vec::new();
        while let Some(line) = self.peek() {
            if line.indent < indent {
                break;
            }
            if line.indent > indent || line.text.starts_with('\t') || is_item(line.text) {
                return None;
            }
            let (key, rest) = split_key(line.text)?;
            self.at += 1;
            let value = self.value(rest, indent)?;
            set(&mut mapping, key.to_owned(), value);
        }
        Some(mapping)
    }

    /// The value after the key of a mapping at `indent`: inline, or the block below it.
    fn value(&mut self, rest: &'a str, indent: usize) -> Option<Yaml> {
        let text = rest.trim_start_matches([' ', '\t']);
        if !text.is_empty() && !text.starts_with('#') {
            return self.inline(text, indent);
        }
        match self.peek() {
            Some(next) if next.indent > indent && is_item(next.text) => self.sequence(next.indent),
            Some(next) if next.indent > indent => Some(Yaml::Map(self.mapping(next.indent)?)),
            Some(next) if next.indent == indent && is_item(next.text) => self.sequence(indent),
            _ => Some(Yaml::Null),
        }
    }

    /// A value on the line of its key or item, where the enclosing block is at `indent`.
    fn inline(&mut self, text: &'a str, indent: usize) -> Option<Yaml> {
        let (value, rest) = match text.chars().next()? {
            '[' => flow_sequence(text)?,
            '"' => double_quoted(text).map(|(s, rest)| (Yaml::Str(s), rest))?,
            '\'' => single_quoted(text).map(|(s, rest)| (Yaml::Str(s), rest))?,
            '|' | '>' => return self.block_scalar(text, indent),
            '{' | '&' | '*' | '!' | '%' | '@' | '`' | '?' | ',' | ']' | '}' => return None,
            _ if is_item(text) => return None,
            _ => return self.plain(text, indent),
        };
        trailing(rest).then_some(value)
    }

    fn sequence(&mut self, indent: usize) -> Option<Yaml> {
        let mut items = Vec::new();
        while let Some(line) = self.peek() {
            if line.indent < indent || !is_item(line.text) {
                break;
            }
            if line.indent > indent {
                return None;
            }
            self.at += 1;
            let item = line.text[1..].trim_start_matches(' ');
            if item.is_empty() || item.starts_with('#') {
                if self.peek().is_some_and(|next| next.indent > indent) {
                    return None;
                }
                items.push(Yaml::Null);
                continue;
            }
            if !item.starts_with(['"', '\'', '[']) && split_key(item).is_some() {
                return None;
            }
            items.push(self.inline(item, indent)?);
        }
        Some(Yaml::List(items))
    }

    /// A plain scalar and its continuation lines, indented deeper than `indent`, folded into
    /// one line; each blank line between them becomes a line break.
    fn plain(&mut self, first: &'a str, indent: usize) -> Option<Yaml> {
        let first = plain_text(first)?;
        let mut text = first.to_owned();
        let mut breaks = 0;
        let mut multiline = false;
        while let Some(raw) = self.lines.get(self.at) {
            let line = raw.trim_start_matches(' ');
            if line.trim_start_matches('\t').is_empty() {
                breaks += 1;
                self.at += 1;
                continue;
            }
            if raw.len() - line.len() <= indent || line.starts_with('#') {
                break;
            }
            let more = plain_text(line)?;
            if line.contains(" #") || is_item(line) {
                return None;
            }
            if breaks == 0 {
                text.push(' ');
            } else {
                text.push_str(&"\n".repeat(breaks));
            }
            text.push_str(more);
            breaks = 0;
            multiline = true;
            self.at += 1;
        }
        if !multiline && matches!(first, "" | "~" | "null" | "Null" | "NULL") {
            return Some(Yaml::Null);
        }
        Some(Yaml::Str(text))
    }

    /// A literal (`|`) or folded (`>`) block scalar with an optional chomping indicator, its
    /// lines indented deeper than `indent`.
    fn block_scalar(&mut self, header: &str, indent: usize) -> Option<Yaml> {
        let folded = header.starts_with('>');
        let (chomp, rest) = match header[1..].chars().next() {
            Some(c @ ('-' | '+')) => (c, &header[2..]),
            _ => (' ', &header[1..]),
        };
        if !trailing(rest) {
            return None;
        }
        let mut lines: Vec<(usize, &str)> = Vec::new();
        let mut block_indent = None;
        while let Some(raw) = self.lines.get(self.at) {
            let text = raw.trim_start_matches(' ');
            let spaces = raw.len() - text.len();
            if text.is_empty() {
                let rest = raw.get(block_indent.unwrap_or(usize::MAX)..).unwrap_or("");
                lines.push((self.at, rest));
                self.at += 1;
                continue;
            }
            if spaces <= indent || block_indent.is_some_and(|at| spaces < at) {
                break;
            }
            let at = *block_indent.get_or_insert(spaces);
            lines.push((self.at, &raw[at..]));
            self.at += 1;
        }
        // A line has a break unless it is the last of the block, which the closing `---` ends.
        let has_break = |(index, _): &(usize, &str)| index + 1 < self.lines.len();
        let kept = lines
            .iter()
            .rposition(|(_, line)| !line.is_empty())
            .map_or(0, |n| n + 1);
        let core: Vec<&str> = lines[..kept].iter().map(|(_, line)| *line).collect();
        let final_break = !core.is_empty() && has_break(&lines[kept - 1]);
        let blank_breaks = lines[kept..].iter().filter(|line| has_break(line)).count();
        let mut text = if folded {
            fold(&core)?
        } else {
            core.join("\n")
        };
        match chomp {
            '-' => {}
            '+' => {
                if final_break {
                    text.push('\n');
                }
                text.push_str(&"\n".repeat(blank_breaks));
            }
            _ if final_break => text.push('\n'),
            _ => {}
        }
        Some(Yaml::Str(text))
    }
}

/// Folded lines: neighbours joined by a space, each blank line between them a line break.
fn fold(lines: &[&str]) -> Option<String> {
    let mut text = String::new();
    let mut breaks = 0;
    let mut started = false;
    for line in lines {
        if line.is_empty() {
            breaks += 1;
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return None;
        }
        if started && breaks == 0 {
            text.push(' ');
        } else {
            text.push_str(&"\n".repeat(breaks));
        }
        text.push_str(line);
        breaks = 0;
        started = true;
    }
    Some(text)
}

/// A block sequence entry: `-` alone or followed by a space.
fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

/// The key of a mapping line and the text after its `:`.
fn split_key(text: &str) -> Option<(&str, &str)> {
    if text.starts_with([
        '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@',
        '`',
    ]) {
        return None;
    }
    let colon = text.char_indices().find_map(|(at, c)| {
        (c == ':' && matches!(text[at + 1..].chars().next(), None | Some(' ' | '\t'))).then_some(at)
    })?;
    let key = text[..colon].trim_end_matches([' ', '\t']);
    (!key.is_empty() && !key.contains(" #")).then_some((key, &text[colon + 1..]))
}

/// The text of a plain scalar line without its comment; `None` where YAML would read a mapping.
fn plain_text(line: &str) -> Option<&str> {
    let end = [" #", "\t#"]
        .iter()
        .filter_map(|marker| line.find(marker))
        .min()
        .unwrap_or(line.len());
    let text = line[..end].trim_end_matches([' ', '\t']);
    let mapping = text.ends_with(':') || text.contains(": ") || text.contains(":\t");
    (!mapping).then_some(text)
}

/// Whether `rest` after a value is blank or a comment.
fn trailing(rest: &str) -> bool {
    let rest = rest.trim_start_matches([' ', '\t']);
    rest.is_empty() || rest.starts_with('#')
}

/// A flow sequence of scalars on one line and the text after its `]`.
fn flow_sequence(text: &str) -> Option<(Yaml, &str)> {
    let mut items = Vec::new();
    let mut rest = &text[1..];
    loop {
        rest = rest.trim_start_matches([' ', '\t']);
        if let Some(after) = rest.strip_prefix(']') {
            return Some((Yaml::List(items), after));
        }
        let (item, after) = match rest.chars().next()? {
            '"' => double_quoted(rest).map(|(s, after)| (Yaml::Str(s), after))?,
            '\'' => single_quoted(rest).map(|(s, after)| (Yaml::Str(s), after))?,
            '[' | '{' | ',' | '#' => return None,
            _ => {
                let end = rest.find([',', ']', '[', '{', '}'])?;
                let item = rest[..end].trim_end_matches([' ', '\t']);
                if item.contains(": ") || item.contains(" #") || item.ends_with(':') {
                    return None;
                }
                let value = match item {
                    "~" | "null" | "Null" | "NULL" => Yaml::Null,
                    _ => Yaml::Str(item.to_owned()),
                };
                (value, &rest[end..])
            }
        };
        items.push(item);
        rest = after.trim_start_matches([' ', '\t']);
        if let Some(after) = rest.strip_prefix(',') {
            rest = after;
        } else if !rest.starts_with(']') {
            return None;
        }
    }
}

/// A single-quoted scalar closed on its line, `''` an escaped quote, and the text after it.
fn single_quoted(text: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut rest = &text[1..];
    loop {
        let at = rest.find('\'')?;
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        match rest.strip_prefix('\'') {
            Some(after) => {
                out.push('\'');
                rest = after;
            }
            None => return Some((out, rest)),
        }
    }
}

/// A double-quoted scalar closed on its line with its escapes resolved, and the text after it.
fn double_quoted(text: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = text.char_indices().skip(1);
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return Some((out, &text[at + 1..])),
            '\\' => {
                let (_, escape) = chars.next()?;
                let digits = match escape {
                    'x' => 2,
                    'u' => 4,
                    'U' => 8,
                    _ => {
                        out.push(simple_escape(escape)?);
                        continue;
                    }
                };
                let mut code = 0;
                for _ in 0..digits {
                    code = code * 16 + chars.next()?.1.to_digit(16)?;
                }
                out.push(char::from_u32(code)?);
            }
            c => out.push(c),
        }
    }
    None
}

fn simple_escape(escape: char) -> Option<char> {
    Some(match escape {
        '0' => '\0',
        'a' => '\u{7}',
        'b' => '\u{8}',
        't' | '\t' => '\t',
        'n' => '\n',
        'v' => '\u{b}',
        'f' => '\u{c}',
        'r' => '\r',
        'e' => '\u{1b}',
        ' ' => ' ',
        '"' => '"',
        '/' => '/',
        '\\' => '\\',
        'N' => '\u{85}',
        '_' => '\u{a0}',
        'L' => '\u{2028}',
        'P' => '\u{2029}',
        _ => return None,
    })
}
