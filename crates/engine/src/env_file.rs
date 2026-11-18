// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Env-files read as the reference reads its `.env`: `NAME=value` lines, an optional `export `,
//! comments, blank lines and a leading BOM; values unquoted.

use std::fs;
use std::io;
use std::path::Path;

use crate::tools::py_strip;

/// The assignments of the env-file at `path` in file order, names and values stripped.
///
/// # Errors
/// The read error of the file.
pub(crate) fn read(path: &Path) -> io::Result<Vec<(String, String)>> {
    let raw = fs::read(path)?;
    let text = String::from_utf8_lossy(&raw);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = py_strip(line);
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (name, value) = line.split_once('=').unwrap_or((line, ""));
        entries.push((py_strip(name).to_owned(), value_of(value)));
    }
    Ok(entries)
}

/// The reference's `_parse_env_value`: quotes removed, `\"` and `\\` unescaped in double quotes.
fn value_of(raw: &str) -> String {
    let value = py_strip(raw);
    let quoted = |q: char| value.len() >= 2 && value.starts_with(q) && value.ends_with(q);
    if quoted('"') {
        let mut out = String::new();
        let mut chars = value[1..value.len() - 1].chars().peekable();
        while let Some(c) = chars.next() {
            match (c, chars.peek()) {
                ('\\', Some(&next @ ('"' | '\\'))) => {
                    out.push(next);
                    chars.next();
                }
                _ => out.push(c),
            }
        }
        out
    } else if quoted('\'') {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}
