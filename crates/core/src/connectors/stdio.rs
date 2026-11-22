//! The connector's stdout under C11 `x-stdio`: lines read with a cap and checked against
//! `Line`.

use std::fmt;
use std::io::{self, BufRead};

use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use ulei_contracts::generated::c11::{ErrorCode, Item, Line, LineType};

use super::manifest::json_fault;

/// C11 `x-stdio.max_line_bytes`: the bytes of a line the runner keeps.
pub(super) const MAX_LINE: usize = 1_052_672;
/// Most UTF-8 bytes of a C11 `ExternalId`.
const MAX_EXTERNAL_ID: usize = 512;
/// Most UTF-8 bytes of a C11 `Cursor`.
const MAX_CURSOR: usize = 4096;

/// What one read of stdout gave; the line's kept bytes are in the caller's buffer.
pub(super) enum Read {
    /// A whole line, its newline dropped.
    Line,
    /// The first [`MAX_LINE`] bytes of a longer line; the rest was discarded.
    Long,
    /// Bytes after the last newline at the end of stdout.
    Unterminated,
    /// The end of stdout.
    End,
}

/// One checked line.
pub(super) enum Event {
    Item(Item),
    Checkpoint(String),
    Done(Option<String>),
    Error(ErrorCode, String),
}

/// Reads the next line into `buf`, keeping at most [`MAX_LINE`] bytes of it.
pub(super) fn read_line(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<Read> {
    buf.clear();
    let mut long = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(if buf.is_empty() && !long {
                Read::End
            } else {
                Read::Unterminated
            });
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];
        let room = MAX_LINE - buf.len();
        if chunk.len() > room {
            long = true;
        }
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        let used = chunk.len() + usize::from(newline.is_some());
        reader.consume(used);
        if newline.is_some() {
            return Ok(if long { Read::Long } else { Read::Line });
        }
    }
}

/// The event of a whole line; the error says what breaks C11 `Line` without quoting content.
pub(super) fn event(bytes: &[u8]) -> Result<Event, String> {
    let line: Line =
        serde_json::from_slice(bytes).map_err(|e| format!("not a C11 Line: {}", json_fault(&e)))?;
    let item_fields = line.external_id.is_some()
        || line.who.is_some()
        || line.at.is_some()
        || line.kind.is_some()
        || line.text.is_some()
        || line.bytes_b64.is_some();
    let error_fields = line.code.is_some() || line.message.is_some() || line.retry_after.is_some();
    match line.r#type {
        LineType::Item => {
            if line.cursor.is_some() || error_fields {
                return Err("an item line carries cursor or error fields".to_owned());
            }
            let (Some(external_id), Some(kind), Some(text), Some(bytes_b64)) =
                (line.external_id, line.kind, line.text, line.bytes_b64)
            else {
                return Err("an item line lacks external_id, kind, text or bytes_b64".to_owned());
            };
            check_external_id(&external_id)?;
            if kind.is_empty() {
                return Err(format!("item {external_id:?}: kind is empty"));
            }
            if line.at.is_some_and(|at| at < 0.0) {
                return Err(format!("item {external_id:?}: at is negative"));
            }
            if !is_base64(&bytes_b64) {
                return Err(format!(
                    "item {external_id:?}: bytes_b64 is not padded standard base64"
                ));
            }
            Ok(Event::Item(Item {
                external_id,
                who: line.who,
                at: line.at,
                kind,
                text,
                bytes_b64,
            }))
        }
        LineType::Checkpoint | LineType::Done if item_fields || error_fields => Err(format!(
            "a {} line carries fields other than cursor",
            line.r#type.as_str()
        )),
        LineType::Checkpoint => {
            let cursor = line.cursor.ok_or("a checkpoint line lacks cursor")?;
            check_cursor(&cursor)?;
            Ok(Event::Checkpoint(cursor))
        }
        LineType::Done => {
            if let Some(cursor) = &line.cursor {
                check_cursor(cursor)?;
            }
            Ok(Event::Done(line.cursor))
        }
        LineType::Error => {
            if item_fields || line.cursor.is_some() {
                return Err("an error line carries item fields or cursor".to_owned());
            }
            let (Some(code), Some(message)) = (line.code, line.message) else {
                return Err("an error line lacks code or message".to_owned());
            };
            if line.retry_after == Some(0) {
                return Err("an error line has retry_after 0".to_owned());
            }
            Ok(Event::Error(code, message))
        }
    }
}

/// The `external_id` of a line cut at [`MAX_LINE`] when the kept bytes are an `item` whose
/// `type` and valid `external_id` come before the cut.
pub(super) fn long_item(prefix: &[u8]) -> Option<String> {
    let mut head = Head::default();
    // The prefix ends inside the object, so the parse always fails at the cut; the fields
    // read before it are what counts.
    let _ = serde_json::Deserializer::from_slice(prefix).deserialize_map(HeadVisitor(&mut head));
    head.external_id
        .filter(|id| head.item && check_external_id(id).is_ok())
}

/// Checks the byte bound of C11 `ExternalId`.
fn check_external_id(id: &str) -> Result<(), String> {
    match id.len() {
        0 => Err("external_id is empty".to_owned()),
        n if n > MAX_EXTERNAL_ID => {
            Err(format!("external_id has {n} bytes, over {MAX_EXTERNAL_ID}"))
        }
        _ => Ok(()),
    }
}

/// Checks the byte bound of C11 `Cursor`.
pub(super) fn check_cursor(cursor: &str) -> Result<(), String> {
    if cursor.len() > MAX_CURSOR {
        return Err(format!(
            "cursor has {} bytes, over {MAX_CURSOR}",
            cursor.len()
        ));
    }
    Ok(())
}

/// Whether `text` matches C11 `Bytes`: standard base64 with padding.
fn is_base64(text: &str) -> bool {
    let bytes = text.as_bytes();
    let body = bytes
        .strip_suffix(b"==")
        .or_else(|| bytes.strip_suffix(b"="))
        .unwrap_or(bytes);
    bytes.len().is_multiple_of(4)
        && body
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/'))
}

/// The fields of a cut line that decide whether it is a too-large item.
#[derive(Default)]
struct Head {
    item: bool,
    external_id: Option<String>,
}

struct HeadVisitor<'a>(&'a mut Head);

impl<'de> Visitor<'de> for HeadVisitor<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a C11 Line object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "type" => self.0.item = map.next_value::<LineType>()? == LineType::Item,
                "external_id" => self.0.external_id = Some(map.next_value()?),
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}
