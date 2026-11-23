//! `ulei-connector-mcp-example`, a C11 MCP connector that wraps a ready MCP server (ADR-0056
//! section 7). It serves `ulei_pull` over stdio: the first call starts the wrapped server as its
//! child, and each call asks the wrapped server's listing tool for the entries after the cursor
//! and answers them mapped to `Item`s as one `Page`, or an `Error` with `isError`.
//!
//! `ulei-connector-mcp-example --tool NAME --entries FIELD --id FIELD --text FIELD --cursor FIELD
//! --after ARG [--limit ARG] [--who FIELD] [--at FIELD] [--kind FIELD] [--bytes FIELD]
//! -- PROGRAM [ARG...]`
//!
//! - `--tool` is the listing tool, called with `--after` set to the cursor (a string, or null on
//!   the first run) and `--limit` to the request's `limit`.
//! - `--entries` names the array of entries in the tool's `structuredContent`, or in the JSON of
//!   its first text content when it has none.
//! - `--id`, `--text`, `--who`, `--at`, `--kind` and `--bytes` name the fields of an entry that
//!   become the `Item`'s; without `--kind` the kind is `text/plain`, without `--bytes` the bytes
//!   are the text's.
//! - The page's cursor is the `--cursor` field of its last entry, a string or an integer, and a
//!   page that holds `limit` entries has `more`.
//!
//! Errors: a `Request` that is not a C11 `Request` is `bad_request`; an argv that does not parse, a
//! program that does not start, a result without the entries array, or an entry that maps to no
//! C11 `Item`, `internal`; the wrapped server ending or answering anything but its JSON-RPC 2.0
//! result, `source_unavailable`; an `isError` result passes on its `structuredContent` when that
//! is a C11 `Error` and is `source_unavailable` with its text otherwise.

use std::io::{self, BufRead, BufReader, Read as _, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitCode, Stdio};

use listenfd::ListenFd;
use serde_json::{Map, Value, json};
use ulei_contracts::generated::c11::{Error, ErrorCode, Item, Page, Request};

const PROTOCOL: &str = "2025-11-25";
const TOOL: &str = "ulei_pull";
const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The variables of this process the wrapped server gets: the ones the run's launcher sets.
const KEPT_ENV: [&str; 4] = ["PATH", "HOME", "TMPDIR", "LANG"];
/// Most bytes of one message of the wrapped server.
const MAX_MESSAGE: u64 = 64 * 1024 * 1024;
/// Most UTF-8 bytes of a C11 `ExternalId` and `Cursor`.
const MAX_EXTERNAL_ID: usize = 512;
const MAX_CURSOR: usize = 4096;

fn main() -> ExitCode {
    let mut server = Server {
        wrap: close_broker().and_then(|()| Wrap::parse(std::env::args().skip(1))),
        wrapped: None,
    };
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                eprintln!("{NAME}: stdin: {e}");
                return ExitCode::FAILURE;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.answer(&line)
            && let Err(e) = send(&mut out, &reply)
        {
            eprintln!("{NAME}: stdout: {e}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// What argv says about the wrapped server and its listing tool.
struct Wrap {
    tool: String,
    entries: String,
    id: String,
    text: String,
    cursor: String,
    after: String,
    limit: Option<String>,
    who: Option<String>,
    at: Option<String>,
    kind: Option<String>,
    bytes: Option<String>,
    program: String,
    args: Vec<String>,
}

impl Wrap {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut named = Vec::new();
        let program = loop {
            let flag = args.next().ok_or("argv has no -- PROGRAM")?;
            if flag == "--" {
                break args.next().ok_or("argv names no PROGRAM after --")?;
            }
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            named.push((flag, value));
        };
        let mut take = |flag: &str| {
            let at = named.iter().position(|(name, _)| name == flag)?;
            Some(named.remove(at).1)
        };
        let mut need = |flag: &str| take(flag).ok_or_else(|| format!("argv lacks {flag}"));
        let wrap = Self {
            tool: need("--tool")?,
            entries: need("--entries")?,
            id: need("--id")?,
            text: need("--text")?,
            cursor: need("--cursor")?,
            after: need("--after")?,
            limit: take("--limit"),
            who: take("--who"),
            at: take("--at"),
            kind: take("--kind"),
            bytes: take("--bytes"),
            program,
            args: args.collect(),
        };
        match named.first() {
            Some((flag, _)) => Err(format!("unknown or repeated argument {flag}")),
            None => Ok(wrap),
        }
    }
}

/// The connector's side of the session with the runner.
struct Server {
    /// The wrap argv names, or why there is none.
    wrap: Result<Wrap, String>,
    /// The wrapped server once a call started it.
    wrapped: Option<Wrapped>,
}

impl Server {
    /// The answer to one message of the runner; none for a notification or a response.
    fn answer(&mut self, line: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(e) => {
                return Some(rpc_error(
                    &Value::Null,
                    -32700,
                    &format!("Parse error: {e}"),
                ));
            }
        };
        let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id")) else {
            return None;
        };
        let params = &message["params"];
        Some(match method {
            "initialize" => reply(
                id,
                &json!({
                    "protocolVersion": PROTOCOL,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": NAME, "version": VERSION},
                }),
            ),
            "ping" => reply(id, &json!({})),
            "tools/list" => reply(
                id,
                &json!({"tools": [{
                    "name": TOOL,
                    "description": "The wrapped server's entries after the cursor as a C11 Page.",
                    "inputSchema": {"type": "object"},
                }]}),
            ),
            "tools/call" if params["name"] == TOOL => reply(id, &self.pull(&params["arguments"])),
            "tools/call" => rpc_error(id, -32602, &format!("Unknown tool: {}", params["name"])),
            _ => rpc_error(id, -32601, "Method not found"),
        })
    }

    /// The `tools/call` result of `ulei_pull` with `arguments`.
    fn pull(&mut self, arguments: &Value) -> Value {
        match self.page(arguments) {
            Ok(page) => json!({
                "content": [{"type": "text", "text": format!("{} items", page.items.len())}],
                "structuredContent": page,
            }),
            Err(error) => json!({
                "content": [{"type": "text", "text": error.message}],
                "structuredContent": error,
                "isError": true,
            }),
        }
    }

    fn page(&mut self, arguments: &Value) -> Result<Page, Error> {
        let request = request(arguments).map_err(|m| error(ErrorCode::BadRequest, m))?;
        let wrap = self
            .wrap
            .as_ref()
            .map_err(|m| error(ErrorCode::Internal, m.clone()))?;
        let wrapped = match &mut self.wrapped {
            Some(wrapped) => wrapped,
            None => self.wrapped.insert(Wrapped::start(wrap)?),
        };
        let listed = wrapped.list(wrap, &request);
        if listed.is_err() {
            // A server that failed once is started afresh by the next call.
            self.wrapped = None;
        }
        page(wrap, &request, &listed?)
    }
}

/// The C11 `Request` in `arguments`, with the bounds its Rust type does not carry.
fn request(arguments: &Value) -> Result<Request, String> {
    let fields = ["contract", "cursor", "limit", "key_file", "data_dir"];
    if let Some(missing) = fields.iter().find(|field| arguments.get(field).is_none()) {
        return Err(format!("the request lacks {missing}"));
    }
    let request: Request = serde_json::from_value(arguments.clone())
        .map_err(|e| format!("the request is not a C11 Request: {e}"))?;
    if request.limit == 0 {
        return Err("the request's limit is 0".to_owned());
    }
    if request
        .cursor
        .as_ref()
        .is_some_and(|c| c.len() > MAX_CURSOR)
    {
        return Err(format!("the request's cursor is over {MAX_CURSOR} bytes"));
    }
    if request.data_dir.is_empty() || request.key_file.as_deref() == Some("") {
        return Err("the request has an empty data_dir or key_file".to_owned());
    }
    Ok(request)
}

/// The wrapped server, a child over its stdin and stdout; dropping it kills it.
struct Wrapped {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    last_id: u64,
}

impl Drop for Wrapped {
    fn drop(&mut self) {
        // Already exited is the only failure, and the wait reaps it either way.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Wrapped {
    /// Starts the server with a cleared environment and initializes it.
    fn start(wrap: &Wrap) -> Result<Self, Error> {
        let mut command = Command::new(&wrap.program);
        command
            .args(&wrap.args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for name in KEPT_ENV {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|e| error(ErrorCode::Internal, format!("{}: {e}", wrap.program)))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(error(
                ErrorCode::Internal,
                "no pipes to the wrapped server".into(),
            ));
        };
        let mut wrapped = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            last_id: 0,
        };
        let initialize = json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": NAME, "version": VERSION},
        });
        wrapped.call("initialize", &initialize)?;
        wrapped.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        Ok(wrapped)
    }

    /// The listing tool's result for the entries after the request's cursor.
    fn list(&mut self, wrap: &Wrap, request: &Request) -> Result<Value, Error> {
        let mut arguments = Map::new();
        arguments.insert(wrap.after.clone(), request.cursor.clone().into());
        if let Some(limit) = &wrap.limit {
            arguments.insert(limit.clone(), request.limit.into());
        }
        let result = self.call(
            "tools/call",
            &json!({"name": wrap.tool, "arguments": arguments}),
        )?;
        if result["isError"] != true {
            return Ok(result);
        }
        Err(
            match serde_json::from_value::<Error>(result["structuredContent"].clone()) {
                Ok(passed) if passed.retry_after != Some(0) => passed,
                _ => unavailable(format!(
                    "{} answered an error: {}",
                    wrap.tool,
                    text(&result)
                )),
            },
        )
    }

    /// Sends request `method` and reads until its result, answering the server's own requests.
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, Error> {
        self.last_id += 1;
        let id = self.last_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let mut message = self.read(method)?;
            if message.get("method").is_some() {
                if let Some(asked) = message.get("id") {
                    self.send(&rpc_error(asked, -32601, "Method not found"))?;
                }
                continue;
            }
            if message["jsonrpc"] != "2.0" || message["id"] != id {
                return Err(unavailable(format!(
                    "the wrapped server's answer to {method} is not the JSON-RPC 2.0 response to id {id}"
                )));
            }
            if let Some(e) = message.get("error") {
                return Err(unavailable(format!(
                    "the wrapped server answered {method} with JSON-RPC error {}: {}",
                    e["code"], e["message"]
                )));
            }
            return message.get_mut("result").map(Value::take).ok_or_else(|| {
                unavailable(format!(
                    "the wrapped server answered {method} with no result"
                ))
            });
        }
    }

    /// The next message of the server, skipping blank lines.
    fn read(&mut self, method: &str) -> Result<Value, Error> {
        loop {
            let mut line = String::new();
            let read = (&mut self.stdout).take(MAX_MESSAGE).read_line(&mut line);
            match read {
                Ok(0) => {
                    return Err(unavailable(format!(
                        "the wrapped server ended its stdout before it answered {method}"
                    )));
                }
                Ok(_) if !line.ends_with('\n') && line.len() as u64 == MAX_MESSAGE => {
                    return Err(unavailable(format!(
                        "the wrapped server's answer to {method} is over {MAX_MESSAGE} bytes"
                    )));
                }
                Ok(_) if line.trim().is_empty() => {}
                Ok(_) => {
                    return serde_json::from_str(&line).map_err(|e| {
                        unavailable(format!(
                            "the wrapped server's answer to {method} is not JSON: {e}"
                        ))
                    });
                }
                Err(e) => return Err(unavailable(format!("the wrapped server's stdout: {e}"))),
            }
        }
    }

    fn send(&mut self, message: &Value) -> Result<(), Error> {
        send(&mut self.stdin, message)
            .map_err(|e| unavailable(format!("the wrapped server's stdin: {e}")))
    }
}

/// The page of the listing tool's `result` for `request`.
fn page(wrap: &Wrap, request: &Request, result: &Value) -> Result<Page, Error> {
    let internal = |message| error(ErrorCode::Internal, message);
    let content = match result.get("structuredContent") {
        Some(content) => content.clone(),
        None => serde_json::from_str(text(result)).map_err(|_| {
            internal(format!(
                "{} answered neither structuredContent nor JSON text",
                wrap.tool
            ))
        })?,
    };
    let entries = content[&wrap.entries].as_array().ok_or_else(|| {
        internal(format!(
            "{} answered no array {:?}",
            wrap.tool, wrap.entries
        ))
    })?;
    let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
    let kept = &entries[..entries.len().min(limit)];
    let mut items = Vec::with_capacity(kept.len());
    for (k, entry) in kept.iter().enumerate() {
        items.push(item(wrap, entry).map_err(|m| internal(format!("entry {}: {m}", k + 1)))?);
    }
    let cursor = match kept.last() {
        Some(last) => {
            let cursor = scalar(last, &wrap.cursor)
                .map_err(|m| internal(format!("entry {}: {m}", kept.len())))?;
            if cursor.len() > MAX_CURSOR {
                return Err(internal(format!("its cursor is over {MAX_CURSOR} bytes")));
            }
            Some(cursor)
        }
        None => request.cursor.clone(),
    };
    Ok(Page {
        items,
        cursor,
        more: entries.len() >= limit,
    })
}

/// The `Item` of one entry, by the field names of `wrap`.
fn item(wrap: &Wrap, entry: &Value) -> Result<Item, String> {
    let external_id = scalar(entry, &wrap.id)?;
    if external_id.is_empty() || external_id.len() > MAX_EXTERNAL_ID {
        return Err(format!("{} is not 1 to {MAX_EXTERNAL_ID} bytes", wrap.id));
    }
    let text = string(entry, &wrap.text)?;
    let who = match &wrap.who {
        Some(field) if !entry[field].is_null() => Some(string(entry, field)?),
        _ => None,
    };
    let at = match &wrap.at {
        Some(field) if !entry[field].is_null() => Some(
            entry[field]
                .as_f64()
                .filter(|at| *at >= 0.0)
                .ok_or_else(|| format!("{field} is not a Unix time"))?,
        ),
        _ => None,
    };
    let kind = match &wrap.kind {
        Some(field) => string(entry, field).and_then(|kind| {
            if kind.is_empty() {
                Err(format!("{field} is empty"))
            } else {
                Ok(kind)
            }
        })?,
        None => "text/plain".to_owned(),
    };
    let bytes_b64 = match &wrap.bytes {
        Some(field) => string(entry, field).and_then(|bytes| {
            if is_base64(&bytes) {
                Ok(bytes)
            } else {
                Err(format!("{field} is not padded standard base64"))
            }
        })?,
        None => base64(text.as_bytes()),
    };
    Ok(Item {
        external_id,
        who,
        at,
        kind,
        text,
        bytes_b64,
    })
}

/// The string `field` of `entry`.
fn string(entry: &Value, field: &str) -> Result<String, String> {
    entry[field]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{field} is not a string"))
}

/// The string or integer `field` of `entry` as a string.
fn scalar(entry: &Value, field: &str) -> Result<String, String> {
    match &entry[field] {
        Value::String(text) => Ok(text.clone()),
        Value::Number(n) if n.is_i64() || n.is_u64() => Ok(n.to_string()),
        _ => Err(format!("{field} is neither a string nor an integer")),
    }
}

/// The first text content of a tool result.
fn text(result: &Value) -> &str {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|block| block["text"].as_str())
        .unwrap_or("no text")
}

/// Closes the run's broker socket, the fd named `broker` systemd passed to this process
/// (`sd_listen_fds(3)`), so the wrapped server does not inherit it.
fn close_broker() -> Result<(), String> {
    let Ok(names) = std::env::var("LISTEN_FDNAMES") else {
        return Ok(());
    };
    let pid = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok());
    let Some(index) = names
        .split(':')
        .position(|name| name == "broker")
        .filter(|_| pid == Some(std::process::id()))
    else {
        return Ok(());
    };
    // `listenfd` checks the fd is an AF_UNIX stream socket and owns it; the drop closes it.
    ListenFd::from_env()
        .take_unix_listener(index)
        .map(drop)
        .map_err(|e| format!("broker fd {index}: {e}"))
}

fn error(code: ErrorCode, message: String) -> Error {
    Error {
        code,
        message,
        retry_after: None,
    }
}

fn unavailable(message: String) -> Error {
    error(ErrorCode::SourceUnavailable, message)
}

fn reply(id: &Value, result: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Writes `message` as one line and flushes it.
fn send(out: &mut impl Write, message: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, message)?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Standard base64 with padding (RFC 4648 section 4).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(group >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Whether `text` is standard base64 with padding.
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
