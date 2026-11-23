//! The connector's stdout under C11 `x-stdio`: lines read with a cap and checked against
//! `Line`.

use std::io::{self, BufRead};

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
    /// The first [`MAX_LINE`] bytes of a longer line; the rest was discarded after a scan for
    /// the item's `external_id`, given here, or why the line is not an item with one.
    Long(Result<String, String>),
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
    /// An `Error` with its `retry_after`.
    Error(ErrorCode, String, Option<u64>),
}

/// Reads the next line into `buf`, keeping at most [`MAX_LINE`] bytes of it.
pub(super) fn read_line(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<Read> {
    buf.clear();
    let mut scan: Option<Scan> = None;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(if buf.is_empty() && scan.is_none() {
                Read::End
            } else {
                Read::Unterminated
            });
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];
        if let Some(scan) = &mut scan {
            scan.feed(chunk);
        } else {
            let room = MAX_LINE - buf.len();
            buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if chunk.len() > room {
                let mut long = Scan::default();
                long.feed(buf);
                long.feed(&chunk[room..]);
                scan = Some(long);
            }
        }
        let used = chunk.len() + usize::from(newline.is_some());
        reader.consume(used);
        if newline.is_some() {
            return Ok(match scan {
                Some(scan) => Read::Long(scan.item_id()),
                None => Read::Line,
            });
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
            Ok(Event::Error(code, message, line.retry_after))
        }
    }
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

/// The top-level field a scan keeps the string of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Type,
    ExternalId,
}

/// A string a scan is inside of.
enum Capture {
    /// A top-level key, kept up to the longest field name.
    Key(Vec<u8>),
    /// The raw value of a kept field; none once it outgrew the field's bound.
    Value(Field, Option<Vec<u8>>),
    /// Any other string.
    Skip,
}

/// What a scan found for one field: its raw JSON string, or why it has none.
enum Found {
    Raw(Vec<u8>),
    TooLong,
    NotString,
}

/// A streaming read of a JSON object's top-level `type` and `external_id`, wherever they
/// stand in a line too long to keep. It tracks nesting and strings only; the values of other
/// fields are skipped unparsed.
#[derive(Default)]
struct Scan {
    depth: usize,
    /// Inside a string, with its capture.
    string: Option<Capture>,
    escaped: bool,
    /// At depth 1 the next string is a key.
    key_next: bool,
    /// The top-level key whose value comes next, when it is a kept field.
    field: Option<Field>,
    r#type: Option<Found>,
    external_id: Option<Found>,
}

/// Longest raw JSON string of an `external_id` within C11's bound: each byte of the id takes
/// at most a six-byte `\u` escape.
const MAX_RAW_ID: usize = 6 * MAX_EXTERNAL_ID;
/// Longest raw `type` value kept: every `LineType` is shorter.
const MAX_RAW_TYPE: usize = 16;

impl Scan {
    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if let Some(capture) = &mut self.string {
                if self.escaped {
                    self.escaped = false;
                } else if b == b'\\' {
                    self.escaped = true;
                } else if b == b'"' {
                    let capture = self.string.take();
                    self.end_string(capture);
                    continue;
                }
                match capture {
                    Capture::Key(raw) if raw.len() <= MAX_RAW_TYPE => raw.push(b),
                    Capture::Value(field, value) => {
                        let bound = match field {
                            Field::Type => MAX_RAW_TYPE,
                            Field::ExternalId => MAX_RAW_ID,
                        };
                        match value {
                            Some(raw) if raw.len() < bound => raw.push(b),
                            _ => *value = None,
                        }
                    }
                    Capture::Key(_) | Capture::Skip => {}
                }
                continue;
            }
            match b {
                b'"' => self.start_string(),
                b'{' | b'[' => {
                    self.value_not_string();
                    self.depth += 1;
                    self.key_next = self.depth == 1;
                }
                b'}' | b']' => self.depth = self.depth.saturating_sub(1),
                b':' if self.depth == 1 => self.key_next = false,
                b',' if self.depth == 1 => {
                    self.key_next = true;
                    self.field = None;
                }
                b' ' | b'\t' | b'\r' => {}
                _ => self.value_not_string(),
            }
        }
    }

    fn start_string(&mut self) {
        self.string = Some(match (self.depth, self.key_next, self.field) {
            (1, true, _) => Capture::Key(Vec::new()),
            (1, false, Some(field)) => Capture::Value(field, Some(Vec::new())),
            _ => Capture::Skip,
        });
    }

    fn end_string(&mut self, capture: Option<Capture>) {
        match capture {
            Some(Capture::Key(key)) => {
                self.field = match key.as_slice() {
                    b"type" => Some(Field::Type),
                    b"external_id" => Some(Field::ExternalId),
                    _ => None,
                };
            }
            Some(Capture::Value(field, value)) => {
                self.field = None;
                self.set(field, value.map_or(Found::TooLong, Found::Raw));
            }
            Some(Capture::Skip) | None => {}
        }
    }

    /// A top-level value that starts with anything but a quote.
    fn value_not_string(&mut self) {
        if self.depth == 1
            && let Some(field) = self.field.take()
        {
            self.set(field, Found::NotString);
        }
    }

    fn set(&mut self, field: Field, found: Found) {
        match field {
            Field::Type => self.r#type = Some(found),
            Field::ExternalId => self.external_id = Some(found),
        }
    }

    /// The `external_id` of the item the line is, or why it is not one with a readable id.
    fn item_id(self) -> Result<String, String> {
        let decode = |raw: &[u8]| {
            let mut quoted = Vec::with_capacity(raw.len() + 2);
            quoted.push(b'"');
            quoted.extend_from_slice(raw);
            quoted.push(b'"');
            serde_json::from_slice::<String>(&quoted).ok()
        };
        match self.r#type {
            Some(Found::Raw(raw)) if decode(&raw).as_deref() == Some("item") => {}
            Some(_) => return Err("is not an item".to_owned()),
            None => return Err("has no top-level type".to_owned()),
        }
        let raw = match self.external_id {
            Some(Found::Raw(raw)) => raw,
            Some(Found::TooLong) => {
                return Err(format!("has an external_id over {MAX_EXTERNAL_ID} bytes"));
            }
            Some(Found::NotString) => {
                return Err("has an external_id that is not a string".to_owned());
            }
            None => return Err("has no top-level external_id".to_owned()),
        };
        let id = decode(&raw).ok_or("has an external_id that is not a JSON string")?;
        check_external_id(&id).map_err(|e| format!("its {e}"))?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use super::*;

    /// The reads of `input` through a small buffer, so lines span many fills.
    fn reads(input: &[u8]) -> Vec<(String, usize)> {
        let mut reader = BufReader::with_capacity(4096, Cursor::new(input.to_vec()));
        let mut buf = Vec::new();
        let mut out = Vec::new();
        loop {
            let read = read_line(&mut reader, &mut buf).expect("read");
            let shown = match read {
                Read::Line => "line".to_owned(),
                Read::Long(Ok(id)) => format!("long {id}"),
                Read::Long(Err(why)) => format!("long: {why}"),
                Read::Unterminated => "unterminated".to_owned(),
                Read::End => break,
            };
            out.push((shown, buf.len()));
        }
        out
    }

    fn big_item(head: &str, tail: &str) -> Vec<u8> {
        let mut line = format!("{{\"type\":\"item\",{head}\"text\":\"").into_bytes();
        line.resize(line.len() + 2 * 1024 * 1024, b'a');
        line.extend_from_slice(
            format!("\",\"kind\":\"text/plain\",\"bytes_b64\":\"\"{tail}}}\n").as_bytes(),
        );
        line
    }

    #[test]
    fn a_two_mib_item_with_external_id_as_its_last_field_keeps_its_id() {
        let mut input = big_item(
            "",
            r#","nested":{"external_id":"inner"},"external_id":"big-\u00e9\"1""#,
        );
        input.extend_from_slice(b"{\"type\":\"done\"}\n");
        assert_eq!(
            reads(&input),
            [
                ("long big-\u{e9}\"1".to_owned(), MAX_LINE),
                ("line".to_owned(), 15)
            ]
        );
        let before = big_item(r#""external_id":"first","#, "");
        assert_eq!(reads(&before), [("long first".to_owned(), MAX_LINE)]);
    }

    #[test]
    fn a_cut_line_without_a_readable_item_id_says_why() {
        let long_id = "x".repeat(MAX_EXTERNAL_ID + 1);
        let raw_id = "x".repeat(MAX_RAW_ID + 1);
        for (head, tail, why) in [
            ("", "", "has no top-level external_id"),
            (
                "",
                r#","nested":{"external_id":"inner"}"#,
                "has no top-level external_id",
            ),
            (
                "",
                r#","external_id":7"#,
                "has an external_id that is not a string",
            ),
            (
                "",
                r#","external_id":["a"]"#,
                "has an external_id that is not a string",
            ),
            (
                "",
                &format!(r#","external_id":"{long_id}""#),
                "its external_id has 513 bytes, over 512",
            ),
            (
                "",
                &format!(r#","external_id":"{raw_id}""#),
                "has an external_id over 512 bytes",
            ),
            ("", r#","external_id":"""#, "its external_id is empty"),
        ] {
            assert_eq!(
                reads(&big_item(head, tail)),
                [(format!("long: {why}"), MAX_LINE)],
                "{tail}"
            );
        }
        let mut checkpoint = b"{\"type\":\"checkpoint\",\"cursor\":\"".to_vec();
        checkpoint.resize(checkpoint.len() + MAX_LINE, b'c');
        checkpoint.extend_from_slice(b"\",\"external_id\":\"x\"}\n");
        assert_eq!(
            reads(&checkpoint),
            [("long: is not an item".to_owned(), MAX_LINE)]
        );
    }
}
