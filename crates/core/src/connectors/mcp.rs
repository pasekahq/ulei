//! C11 `x-mcp`: the connector as an MCP server over stdio, newline-delimited JSON-RPC 2.0. The
//! runner initializes it, calls `ulei_pull` page by page and hands each page to the run's sink as
//! its items, one at a time while they are parsed, then a checkpoint with its cursor.

use std::cell::Cell;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write as _};
use std::mem;
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use ulei_contracts::generated::c11::{ErrorCode, Item, Request};

use super::manifest::json_fault;
use super::stderr::Seen;
use super::stdio::{self, Event, MAX_LINE, MAX_RAW_ID};
use super::{Sink, Stop};

/// The protocol version and client info of the `initialize` request (ADR-0026).
const PROTOCOL: &str = "2025-11-25";
const CLIENT: (&str, &str) = ("mcp", "0.1.0");
/// The versions an `initialize` answer may name: ADR-0026's handshake versions.
const VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const TOOL: &str = "ulei_pull";
/// JSON-RPC "method not found", the answer to every request of the server.
const METHOD_NOT_FOUND: i64 = -32601;
/// How long the server may run after its last answer and the end of its stdin before the runner
/// stops its unit.
pub(super) const LINGER: Duration = Duration::from_secs(3);
/// Most raw bytes of a JSON number: no number a server needs is longer, and the parser buffers a
/// long one whole.
const MAX_NUMBER: usize = 1024;

/// Runs the MCP session over the child's pipes into `sink` and closes both pipes when it ends.
pub(super) fn feed(child: &mut Child, request: &Request, sink: &mut Sink<'_>) -> Stop {
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Stop::Protocol("no stdin or stdout pipe".to_owned());
    };
    let mut session = Session {
        stdin,
        stdout: BufReader::with_capacity(64 * 1024, stdout),
        gauge: Gauge::default(),
        last_id: 0,
        messages: 0,
        limit: request.limit,
    };
    session.pull(request, sink)
}

/// Waits up to `wait` for the unit's end on `watch` once the session ended; past it `stop` ends
/// the unit and gives why its stop failed. Gives what stderr saw when the unit ended in time,
/// and the reason when it had to be stopped.
pub(super) fn linger(
    watch: Option<&Receiver<Seen>>,
    wait: Duration,
    stop: impl FnOnce() -> Option<String>,
) -> (Option<Seen>, Option<String>) {
    match watch.map(|rx| rx.recv_timeout(wait)) {
        Some(Err(RecvTimeoutError::Timeout)) => {
            let lingered = format!(
                "stopped: the server did not exit within {} s of its last answer",
                wait.as_secs_f64()
            );
            let reason = match stop() {
                Some(failed) => format!("{lingered}; {failed}"),
                None => lingered,
            };
            (None, Some(reason))
        }
        Some(Ok(ended)) => (Some(ended), None),
        Some(Err(RecvTimeoutError::Disconnected)) | None => (None, None),
    }
}

struct Session {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    gauge: Gauge,
    last_id: u64,
    /// Messages read from stdout.
    messages: u64,
    /// Most items of a page: the request's `limit`.
    limit: u64,
}

/// The answer a call waits for.
#[derive(Clone, Copy)]
enum Pending {
    Initialize,
    /// `tools/call` of `ulei_pull` for page `n`.
    Page(u64),
}

impl fmt::Display for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Initialize => f.write_str("initialize"),
            Self::Page(n) => write!(f, "ulei_pull page {n}"),
        }
    }
}

/// A response's result as far as the runner reads it.
enum Answer {
    /// `initialize`: its `protocolVersion`.
    Version(Option<String>),
    Page(Pulled),
}

/// One message read while a call is pending.
enum Message {
    Notification,
    /// A request of the server, by its id.
    Request(Value),
    Answer(Answer),
}

impl Session {
    fn pull(&mut self, request: &Request, sink: &mut Sink<'_>) -> Stop {
        let initialize = json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": CLIENT.0, "version": CLIENT.1},
        });
        let version = match self.call("initialize", &initialize, Pending::Initialize, sink) {
            Ok(Answer::Version(version)) => version,
            Ok(Answer::Page(_)) => return Stop::Protocol("initialize answered a page".to_owned()),
            Err(stop) => return stop,
        };
        match version.as_deref() {
            Some(version) if VERSIONS.contains(&version) => {}
            Some(version) => {
                return Stop::Protocol(format!(
                    "initialize answered protocol version {version:?}, which the runner does not speak"
                ));
            }
            None => {
                return Stop::Protocol("initialize answered no protocolVersion".to_owned());
            }
        }
        if let Err(stop) = self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        })) {
            return stop;
        }
        let mut request = request.clone();
        let mut page = 0;
        loop {
            page += 1;
            let params = match serde_json::to_value(&request) {
                Ok(arguments) => json!({"name": TOOL, "arguments": arguments}),
                Err(e) => return Stop::Protocol(format!("request: {e}")),
            };
            let pulled = match self.call("tools/call", &params, Pending::Page(page), sink) {
                Ok(Answer::Page(pulled)) => pulled,
                Ok(Answer::Version(_)) => {
                    return Stop::Protocol(format!("page {page} answered a version"));
                }
                Err(stop) => return stop,
            };
            let (cursor, more) = match pulled.check() {
                Ok(Checked::Page(cursor, more)) => (cursor, more),
                Ok(Checked::Error(code, message, retry_after)) => {
                    return Stop::Error(code.as_str().to_owned(), message, retry_after);
                }
                Err(reason) => return Stop::Protocol(format!("page {page}: {reason}")),
            };
            if more && cursor == request.cursor {
                return Stop::Protocol(format!(
                    "page {page} has more: true with the cursor it was called with"
                ));
            }
            let end = if more {
                cursor.clone().map(Event::Checkpoint)
            } else {
                Some(Event::Done(cursor.clone()))
            };
            if let Some(stop) = end.and_then(|event| sink.handle(event)) {
                return stop;
            }
            request.cursor = cursor;
        }
    }

    /// Sends request `method` and reads messages until its answer, answering the server's
    /// requests on the way; a page's items go to `sink` while its answer is read.
    fn call(
        &mut self,
        method: &str,
        params: &Value,
        pending: Pending,
        sink: &mut Sink<'_>,
    ) -> Result<Answer, Stop> {
        self.last_id += 1;
        let id = self.last_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            match self.read(method, id, pending, sink)? {
                Message::Notification => {}
                Message::Request(request) => self.send(&json!({
                    "jsonrpc": "2.0",
                    "id": request,
                    "error": {"code": METHOD_NOT_FOUND, "message": "Method not found"},
                }))?,
                Message::Answer(answer) => return Ok(answer),
            }
        }
    }

    /// Writes one message line on the server's stdin.
    fn send(&mut self, message: &Value) -> Result<(), Stop> {
        let mut line = serde_json::to_vec(message).map_err(|e| Stop::Protocol(e.to_string()))?;
        line.push(b'\n');
        match self.stdin.write_all(&line) {
            // A server that closed its stdin is judged by its stdout and its unit's result.
            Err(e) if e.kind() != io::ErrorKind::BrokenPipe => {
                Err(Stop::Protocol(format!("stdin: {e}")))
            }
            _ => Ok(()),
        }
    }

    /// Reads the next message while `method` with `id` is pending.
    fn read(
        &mut self,
        method: &str,
        id: u64,
        pending: Pending,
        sink: &mut Sink<'_>,
    ) -> Result<Message, Stop> {
        let ended = || {
            Stop::End(Some(format!(
                "the MCP server ended its stdout before it answered {pending}"
            )))
        };
        loop {
            match self.stdout.fill_buf() {
                Ok([]) => return Err(ended()),
                Ok(available) => {
                    let blank = available
                        .iter()
                        .take_while(|b| matches!(b, b'\n' | b'\r' | b' ' | b'\t'))
                        .count();
                    if blank == 0 {
                        break;
                    }
                    self.stdout.consume(blank);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Stop::Protocol(format!("stdout: {e}"))),
            }
        }
        self.messages += 1;
        let n = self.messages;
        self.gauge.item.set(None);
        self.gauge.long_number.set(false);
        let mut frame = Frame {
            reader: &mut self.stdout,
            gauge: &self.gauge,
            lex: Lex::default(),
            newline: false,
            eof: false,
        };
        let mut pull = Pull {
            sink,
            gauge: &self.gauge,
            limit: self.limit,
            fault: None,
            place: Place::Message,
        };
        let mut de = serde_json::Deserializer::from_reader(&mut frame);
        let parsed = Envelope {
            pending,
            pull: &mut pull,
        }
        .deserialize(&mut de)
        .and_then(|envelope| de.end().map(|()| envelope));
        drop(de);
        let envelope = match parsed {
            Ok(envelope) => envelope,
            Err(e) => {
                return Err(match pull.fault {
                    Some(stop) => stop,
                    None if self.gauge.long_number.get() => {
                        Stop::Protocol(format!("message {n} has a number over {MAX_NUMBER} bytes"))
                    }
                    None if frame.eof => ended(),
                    None if e.is_io() => Stop::Protocol(format!("stdout: {e}")),
                    None => Stop::Protocol(format!(
                        "message {n} is not a JSON-RPC 2.0 message as the runner expects{}: {}",
                        pull.place,
                        json_fault(&e)
                    )),
                });
            }
        };
        envelope
            .message(method, id)
            .map_err(|reason| Stop::Protocol(format!("message {n} {reason}")))
    }
}

/// The parts of a message the runner checks once it was read whole.
#[derive(Default)]
struct Parsed {
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: Option<String>,
    error: Option<RpcError>,
    result: Option<Answer>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct Initialized {
    #[serde(rename = "protocolVersion")]
    protocol_version: Option<String>,
}

impl Parsed {
    /// What the message is while `method` with `id` is pending, or what makes it a fault.
    fn message(self, method: &str, id: u64) -> Result<Message, String> {
        if self.jsonrpc.as_deref() != Some("2.0") {
            return Err("is not JSON-RPC 2.0: its jsonrpc is not \"2.0\"".to_owned());
        }
        if self.method.is_some() {
            if self.result.is_some() || self.error.is_some() {
                return Err("carries a method and a result or error".to_owned());
            }
            return Ok(self.id.map_or(Message::Notification, Message::Request));
        }
        match &self.id {
            Some(Value::Number(answered)) if answered.as_u64() == Some(id) => {}
            Some(other) => {
                return Err(format!("answers id {other}, not the pending {method} {id}"));
            }
            None => return Err(format!("is no response to the pending {method} {id}")),
        }
        match (self.error, self.result) {
            (Some(error), _) => Err(format!(
                "answers {method} with JSON-RPC error {}: {}",
                error.code, error.message
            )),
            (None, Some(answer)) => Ok(Message::Answer(answer)),
            (None, None) => Err(format!("answers {method} without result or error")),
        }
    }
}

/// What a `ulei_pull` result held besides its items, which went to the sink while it was read.
#[derive(Default)]
struct Pulled {
    is_error: Option<bool>,
    structured: Option<Structured>,
}

/// The fields of a `structuredContent`: a C11 `Page` or `Error`.
#[derive(Default)]
struct Structured {
    /// The items read, when `items` was there.
    items: Option<u64>,
    /// Whether `cursor` was there, null or not.
    has_cursor: bool,
    cursor: Option<String>,
    more: Option<bool>,
    code: Option<ErrorCode>,
    message: Option<String>,
    retry_after: Option<u64>,
    /// A field neither type has.
    other: bool,
}

/// A checked `ulei_pull` result.
enum Checked {
    /// A page's cursor and `more`.
    Page(Option<String>, bool),
    /// The `Error` of an `isError` result.
    Error(ErrorCode, String, Option<u64>),
}

impl Pulled {
    /// The checked result, or what makes it neither a page nor an error.
    fn check(self) -> Result<Checked, String> {
        let s = self
            .structured
            .ok_or("the ulei_pull result has no structuredContent")?;
        if self.is_error == Some(true) {
            let not_error = s.other || s.items.is_some() || s.has_cursor || s.more.is_some();
            let (Some(code), Some(message), false) = (s.code, s.message, not_error) else {
                return Err("the structuredContent of an isError result is not a C11 Error".into());
            };
            if s.retry_after == Some(0) {
                return Err("its Error has retry_after 0".to_owned());
            }
            return Ok(Checked::Error(code, message, s.retry_after));
        }
        let not_page =
            s.other || s.code.is_some() || s.message.is_some() || s.retry_after.is_some();
        let (Some(_), true, Some(more), false) = (s.items, s.has_cursor, s.more, not_page) else {
            return Err("the structuredContent is not a C11 Page".to_owned());
        };
        let cursor = s.cursor;
        if let Some(cursor) = &cursor {
            stdio::check_cursor(cursor)?;
        }
        Ok(Checked::Page(cursor, more))
    }
}

/// What a message's parse shares with its reader.
#[derive(Default)]
struct Gauge {
    /// Raw bytes read since the current item began; none outside an item.
    item: Cell<Option<usize>>,
    /// Whether the last string read lost its tail.
    cut: Cell<bool>,
    /// Whether the message holds a number over [`MAX_NUMBER`] bytes, which ends its read.
    long_number: Cell<bool>,
}

impl Gauge {
    fn over(&self) -> bool {
        self.item.get().is_some_and(|n| n > MAX_LINE)
    }
}

/// One message line of stdout as a reader that ends at its newline. A string is cut short, its
/// closing quote kept, at [`MAX_LINE`] raw bytes, and in an item over the cap at [`MAX_RAW_ID`],
/// and a number over [`MAX_NUMBER`] bytes fails the read, so nothing the parser buffers outgrows
/// the cap.
struct Frame<'a> {
    reader: &'a mut BufReader<ChildStdout>,
    gauge: &'a Gauge,
    lex: Lex,
    newline: bool,
    /// stdout ended.
    eof: bool,
}

impl io::Read for Frame<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < out.len() && !self.newline {
            let available = match self.reader.fill_buf() {
                Ok(available) => available,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if available.is_empty() {
                self.eof = true;
                break;
            }
            let mut used = 0;
            for &b in available {
                if n == out.len() {
                    break;
                }
                used += 1;
                if b == b'\n' {
                    self.newline = true;
                    break;
                }
                if let Some(count) = self.gauge.item.get() {
                    self.gauge.item.set(Some(count + 1));
                }
                if self.lex.keep(b, self.gauge) {
                    out[n] = b;
                    n += 1;
                }
                if self.gauge.long_number.get() {
                    self.reader.consume(used);
                    return Err(io::Error::other("number over the bound"));
                }
            }
            self.reader.consume(used);
        }
        Ok(n)
    }
}

/// Where the reader stands in the JSON text: enough to cut a string only between characters
/// and to measure a number.
#[derive(Default)]
enum Lex {
    #[default]
    Outside,
    String(Str),
    /// In a number, with its bytes so far.
    Number(usize),
}

/// The state inside a string.
#[derive(Default)]
struct Str {
    at: At,
    /// The last escape was a high surrogate, which the next escape completes.
    high: bool,
    /// Raw bytes kept.
    kept: usize,
    dropping: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum At {
    #[default]
    Plain,
    /// After a backslash.
    Escape,
    /// In a `\u` escape: hex digits still to come and the value so far.
    Hex(u8, u32),
}

impl Lex {
    /// Whether `b` goes to the parser.
    fn keep(&mut self, b: u8, gauge: &Gauge) -> bool {
        if let Self::Number(len) = self {
            if !matches!(b, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                *self = Self::Outside;
            } else if *len >= MAX_NUMBER {
                gauge.long_number.set(true);
                return false;
            } else {
                *len += 1;
                return true;
            }
        }
        let Self::String(s) = self else {
            if b == b'"' {
                *self = Self::String(Str::default());
                gauge.cut.set(false);
            } else if matches!(b, b'0'..=b'9' | b'-') {
                *self = Self::Number(1);
            }
            return true;
        };
        if s.at == At::Plain && b == b'"' {
            *self = Self::Outside;
            return true;
        }
        let boundary = s.at == At::Plain && b & 0xC0 != 0x80 && !(s.high && b == b'\\');
        let limit = if gauge.over() { MAX_RAW_ID } else { MAX_LINE };
        if !s.dropping && boundary && s.kept >= limit {
            s.dropping = true;
            gauge.cut.set(true);
        }
        let digit = || char::from(b).to_digit(16).unwrap_or(0);
        s.at = match s.at {
            At::Escape if b == b'u' => At::Hex(4, 0),
            At::Hex(1, value) => {
                s.high = (0xD800..0xDC00).contains(&(value * 16 + digit()));
                At::Plain
            }
            At::Hex(left, value) => At::Hex(left - 1, value * 16 + digit()),
            At::Plain if b == b'\\' => At::Escape,
            At::Escape | At::Plain => {
                s.high = false;
                At::Plain
            }
        };
        if s.dropping {
            return false;
        }
        s.kept += 1;
        true
    }
}

/// The run's side of a message's parse.
struct Pull<'p, 's> {
    sink: &'p mut Sink<'s>,
    gauge: &'p Gauge,
    /// Most items of a page.
    limit: u64,
    /// Why the parse was given up, when the runner gave it up.
    fault: Option<Stop>,
    /// Where the parse stands, for a parse error.
    place: Place,
}

#[derive(Clone, Copy)]
enum Place {
    Message,
    Result,
    Structured,
    /// The item of a page, counted from 1.
    Item(u64),
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message => Ok(()),
            Self::Result => f.write_str(" in the ulei_pull result"),
            Self::Structured => f.write_str(" in its structuredContent"),
            Self::Item(k) => write!(f, " at item {k} of its structuredContent"),
        }
    }
}

impl Pull<'_, '_> {
    /// Gives up the parse with `stop`.
    fn fail<E: de::Error>(&mut self, stop: Stop) -> E {
        self.fault = Some(stop);
        E::custom("given up")
    }
}

/// A JSON-RPC message, whose `result` is read as `pending` expects.
struct Envelope<'a, 'p, 's> {
    pending: Pending,
    pull: &'a mut Pull<'p, 's>,
}

impl<'de> DeserializeSeed<'de> for Envelope<'_, '_, '_> {
    type Value = Parsed;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Parsed, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Envelope<'_, '_, '_> {
    type Value = Parsed;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON-RPC 2.0 message")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Parsed, A::Error> {
        let mut parsed = Parsed::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "jsonrpc" => parsed.jsonrpc = Some(map.next_value()?),
                "id" => parsed.id = Some(map.next_value::<Id>()?.0),
                "method" => parsed.method = Some(map.next_value()?),
                "error" => parsed.error = Some(map.next_value()?),
                "result" => {
                    parsed.result = Some(match self.pending {
                        Pending::Initialize => {
                            Answer::Version(map.next_value::<Initialized>()?.protocol_version)
                        }
                        Pending::Page(page) => {
                            self.pull.place = Place::Result;
                            let pulled = map.next_value_seed(Called {
                                page,
                                pull: &mut *self.pull,
                            })?;
                            self.pull.place = Place::Message;
                            Answer::Page(pulled)
                        }
                    });
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(parsed)
    }
}

/// A JSON-RPC id: a number, a string or null.
struct Id(Value);

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(IdVisitor).map(Id)
    }
}

struct IdVisitor;

impl Visitor<'_> for IdVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a number, a string or null")
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
}

/// The `result` of a `ulei_pull` call for `page`.
struct Called<'a, 'p, 's> {
    page: u64,
    pull: &'a mut Pull<'p, 's>,
}

impl<'de> DeserializeSeed<'de> for Called<'_, '_, '_> {
    type Value = Pulled;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Pulled, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Called<'_, '_, '_> {
    type Value = Pulled;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a tools/call result")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Pulled, A::Error> {
        let mut pulled = Pulled::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "structuredContent" => {
                    self.pull.place = Place::Structured;
                    pulled.structured = Some(map.next_value_seed(Content {
                        page: self.page,
                        pull: &mut *self.pull,
                    })?);
                    self.pull.place = Place::Result;
                }
                "isError" => pulled.is_error = map.next_value()?,
                // `content` often repeats the page as text; it is skipped unread.
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(pulled)
    }
}

/// The `structuredContent` of a `ulei_pull` result for `page`.
struct Content<'a, 'p, 's> {
    page: u64,
    pull: &'a mut Pull<'p, 's>,
}

impl<'de> DeserializeSeed<'de> for Content<'_, '_, '_> {
    type Value = Structured;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Structured, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Content<'_, '_, '_> {
    type Value = Structured;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a C11 Page or Error")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Structured, A::Error> {
        let mut s = Structured::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "items" => {
                    s.items = Some(map.next_value_seed(Items {
                        page: self.page,
                        pull: &mut *self.pull,
                    })?);
                    self.pull.place = Place::Structured;
                }
                "cursor" => {
                    s.has_cursor = true;
                    s.cursor = map.next_value()?;
                }
                "more" => s.more = Some(map.next_value()?),
                "code" => s.code = Some(map.next_value()?),
                "message" => s.message = Some(map.next_value()?),
                "retry_after" => s.retry_after = Some(map.next_value()?),
                _ => {
                    s.other = true;
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(s)
    }
}

/// The `items` of a page, each handed to the sink once read.
struct Items<'a, 'p, 's> {
    page: u64,
    pull: &'a mut Pull<'p, 's>,
}

impl<'de> DeserializeSeed<'de> for Items<'_, '_, '_> {
    type Value = u64;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<u64, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Items<'_, '_, '_> {
    type Value = u64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of C11 Items")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<u64, A::Error> {
        let mut k = 0;
        loop {
            k += 1;
            self.pull.place = Place::Item(k);
            let Some(event) = seq.next_element_seed(ItemSeed(self.pull.gauge))? else {
                return Ok(k - 1);
            };
            if k > self.pull.limit {
                let stop = Stop::Protocol(format!(
                    "page {} item {k} is over the request's limit of {} items",
                    self.page, self.pull.limit
                ));
                return Err(self.pull.fail(stop));
            }
            let event = match event {
                Ok(event) => event,
                Err(reason) => {
                    let stop = Stop::Protocol(format!("page {} item {k}: {reason}", self.page));
                    return Err(self.pull.fail(stop));
                }
            };
            if let Some(stop) = self.pull.sink.handle(event) {
                return Err(self.pull.fail(stop));
            }
        }
    }
}

/// The fields of C11 `Item`.
const ITEM_FIELDS: [&str; 6] = ["external_id", "who", "at", "kind", "text", "bytes_b64"];

/// One item of a page: its event, or what makes it neither an item nor one over the cap with a
/// readable id.
struct ItemSeed<'g>(&'g Gauge);

impl<'de> DeserializeSeed<'de> for ItemSeed<'_> {
    type Value = Result<Event, String>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        let gauge = self.0;
        gauge.item.set(Some(0));
        let read = deserializer.deserialize_map(self);
        let over = gauge.over();
        gauge.item.set(None);
        let fields: Fields = read?;
        Ok(if over {
            fields.too_large()
        } else {
            fields.item()
        })
    }
}

impl<'de> Visitor<'de> for ItemSeed<'_> {
    type Value = Fields;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a C11 Item")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Fields, A::Error> {
        let mut fields = Fields::default();
        let mut seen = [false; ITEM_FIELDS.len()];
        while let Some(key) = map.next_key::<String>()? {
            if let Some(at) = ITEM_FIELDS.iter().position(|field| *field == key)
                && mem::replace(&mut seen[at], true)
            {
                fields.duplicate.get_or_insert(ITEM_FIELDS[at]);
                map.next_value::<IgnoredAny>()?;
                continue;
            }
            match key.as_str() {
                "external_id" => {
                    let id: String = map.next_value()?;
                    fields.external_id = Some((id, self.0.cut.get()));
                }
                "who" => fields.who = map.next_value()?,
                "at" => fields.at = map.next_value()?,
                "kind" => fields.kind = Some(map.next_value()?),
                "text" => fields.text = Some(map.next_value()?),
                "bytes_b64" => fields.bytes_b64 = Some(map.next_value()?),
                _ => {
                    fields.other = true;
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(fields)
    }
}

/// The fields of an item; its `external_id` with whether it was cut.
#[derive(Default)]
struct Fields {
    external_id: Option<(String, bool)>,
    who: Option<String>,
    at: Option<f64>,
    kind: Option<String>,
    text: Option<String>,
    bytes_b64: Option<String>,
    /// A field C11 `Item` does not have.
    other: bool,
    /// The first field of C11 `Item` the item holds twice.
    duplicate: Option<&'static str>,
}

impl Fields {
    fn item(self) -> Result<Event, String> {
        if let Some(key) = &self.duplicate {
            return Err(format!("has the field {key} twice"));
        }
        if self.other {
            return Err("has a field C11 Item does not have".to_owned());
        }
        let (Some((external_id, _)), Some(kind), Some(text), Some(bytes_b64)) =
            (self.external_id, self.kind, self.text, self.bytes_b64)
        else {
            return Err("lacks external_id, kind, text or bytes_b64".to_owned());
        };
        stdio::check_item(Item {
            external_id,
            who: self.who,
            at: self.at,
            kind,
            text,
            bytes_b64,
        })
        .map(Event::Item)
    }

    /// The item over the cap by its id, which every such item must carry readably.
    fn too_large(self) -> Result<Event, String> {
        let over = format!("is over {MAX_LINE} bytes and");
        if let Some(key) = &self.duplicate {
            return Err(format!("{over} has the field {key} twice"));
        }
        match self.external_id {
            Some((id, false)) => match stdio::check_external_id(&id) {
                Ok(()) => Ok(Event::TooLarge(id)),
                Err(e) => Err(format!("{over} its {e}, so no too_large id can be kept")),
            },
            Some((_, true)) => Err(format!(
                "{over} has an external_id over 512 bytes, so no too_large id can be kept"
            )),
            None => Err(format!(
                "{over} has no external_id, so no too_large id can be kept"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::super::stderr::Stderr;
    use super::*;

    const WAIT: Duration = Duration::from_millis(20);

    #[test]
    fn a_unit_still_running_past_the_linger_is_stopped_and_one_that_ended_is_not() {
        let (running, rx) = mpsc::channel::<Seen>();
        let mut stops = 0;
        let (seen, reason) = linger(Some(&rx), WAIT, || {
            stops += 1;
            Some("systemctl --user stop u: exit status: 5".to_owned())
        });
        assert!(seen.is_none());
        assert_eq!(
            reason.as_deref(),
            Some(
                "stopped: the server did not exit within 0.02 s of its last answer; \
                 systemctl --user stop u: exit status: 5"
            )
        );
        assert_eq!(stops, 1);
        drop(running);

        let (ended, rx) = mpsc::channel();
        ended.send(Stderr::default().end()).expect("send");
        let (seen, reason) = linger(Some(&rx), WAIT, || panic!("stopped an ended unit"));
        assert!(seen.is_some() && reason.is_none());
        let (seen, reason) = linger(None, WAIT, || panic!("stopped without a watch"));
        assert!(seen.is_none() && reason.is_none());
    }

    /// The bytes `Lex` passes of `input` outside an item, and whether it cut a string.
    fn kept(input: &[u8]) -> (Vec<u8>, bool) {
        let gauge = Gauge::default();
        let mut lex = Lex::default();
        let out = input
            .iter()
            .copied()
            .filter(|&b| lex.keep(b, &gauge))
            .collect();
        (out, gauge.cut.get())
    }

    #[test]
    fn a_cut_string_ends_on_a_character_and_stays_valid_json() {
        for unit in [
            "a",
            "\u{e9}",
            "\\n",
            "\\u00e9",
            "\\ud83d\\ude00",
            "\u{1f600}",
        ] {
            let mut text = b"\"".to_vec();
            while text.len() < MAX_LINE + 64 {
                text.extend_from_slice(unit.as_bytes());
            }
            text.push(b'"');
            let (out, cut) = kept(&text);
            assert!(cut, "{unit}");
            assert!(out.len() <= MAX_LINE + 16, "{unit}: {}", out.len());
            let parsed: String = serde_json::from_slice(&out).expect(unit);
            assert!(!parsed.is_empty(), "{unit}");
        }
    }
}
