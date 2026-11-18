// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The Streamable HTTP session of one MCP server over loopback TCP (ADR-0015 section 3): the
//! content-type preflight, the initialize handshake, JSON-RPC requests answered by
//! `application/json` or by a `text/event-stream`, the session id on every later request and
//! the session's end.

use std::fmt::{self, Write as _};
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::gateway::{self, Chunk, Conn, Head, MAX_LINE, feed, next_line};
use crate::tools::{Interrupt, Running};

/// Protocol revisions the initialize handshake accepts from a server, oldest first.
const HANDSHAKE_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
/// The revision the client offers in `initialize`.
const LATEST_HANDSHAKE_VERSION: &str = "2025-11-25";
/// Longest wait for one socket read or write of a request.
const READ_TIMEOUT: Duration = Duration::from_secs(300);
/// Most bytes of an `application/json` answer body.
const MAX_JSON_BODY: usize = 4 * 1024 * 1024;
/// Most pages of one paginated list.
const MAX_LIST_PAGES: usize = 50;

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);
const MCP_CONTENT_TYPES: [&str; 2] = ["application/json", "text/event-stream"];
const URL_RULE: &str = "must be an http:// URL whose host is 127.0.0.1, [::1] or localhost \
                        resolving only to loopback";
/// Header names the transport writes itself.
const TRANSPORT_HEADERS: [&str; 7] = [
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "accept",
    "content-type",
    "mcp-session-id",
];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INTERNAL_ERROR: i64 = -32603;
const CONNECTION_CLOSED: i64 = -32000;

/// Why a request to an MCP server failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// A JSON-RPC error answer, or one the transport stands in for the answer.
    Rpc {
        /// The JSON-RPC error code.
        code: i64,
        /// The error message.
        message: String,
    },
    /// The HTTP exchange failed; `kind` names the class of the failure.
    Transport {
        /// `ConnectError`, `ConnectTimeout`, `ReadTimeout`, `ReadError`, `WriteError`,
        /// `RemoteProtocolError` or `HTTPStatusError`.
        kind: &'static str,
        /// What happened.
        text: String,
    },
    /// The deadline of the request passed.
    TimedOut,
    /// The interrupt handle of the call fired.
    Interrupted,
}

impl Failure {
    /// The class name the reference's message shows for this failure.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Rpc { .. } => "MCPError",
            Self::Transport { kind, .. } => kind,
            Self::TimedOut => "TimeoutError",
            Self::Interrupted => "InterruptedError",
        }
    }

    fn rpc(code: i64, message: impl Into<String>) -> Self {
        Self::Rpc {
            code,
            message: message.into(),
        }
    }

    fn protocol(text: impl Into<String>) -> Self {
        Self::Transport {
            kind: "RemoteProtocolError",
            text: text.into(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc { message, .. } => f.write_str(message),
            Self::Transport { text, .. } => f.write_str(text),
            Self::TimedOut => f.write_str("TimeoutError"),
            Self::Interrupted => f.write_str("User sent a new message"),
        }
    }
}

/// Where an MCP server listens: a loopback address, its `Host` and its request target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The URL as configured.
    pub url: String,
    host: String,
    target: String,
    addrs: Vec<SocketAddr>,
}

impl Endpoint {
    /// The endpoint of `url`: `http://` with the host `127.0.0.1`, `[::1]` or `localhost`, the
    /// last resolving only to loopback addresses, an optional port and no userinfo.
    ///
    /// # Errors
    /// The rule the URL breaks, as `must be ...`.
    pub fn parse(url: &str) -> Result<Self, &'static str> {
        let rest = url
            .get(..7)
            .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
            .map(|_| &url[7..])
            .ok_or(URL_RULE)?;
        let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
        if authority.contains('@') {
            return Err(URL_RULE);
        }
        let (host, port) = match authority.strip_prefix('[') {
            Some(v6) => {
                let (inner, after) = v6.split_once(']').ok_or(URL_RULE)?;
                (format!("[{inner}]"), after.strip_prefix(':'))
            }
            None => match authority.split_once(':') {
                Some((host, port)) => (host.to_owned(), Some(port)),
                None => (authority.to_owned(), None),
            },
        };
        let port = match port {
            None => 80,
            Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
                digits
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p > 0)
                    .ok_or(URL_RULE)?
            }
            Some(_) => return Err(URL_RULE),
        };
        let host = host.to_ascii_lowercase();
        let addrs = match host.as_str() {
            "127.0.0.1" => vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)],
            "[::1]" => vec![SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)],
            "localhost" => {
                let addrs: Vec<SocketAddr> = ("localhost", port)
                    .to_socket_addrs()
                    .map_err(|_| URL_RULE)?
                    .collect();
                if addrs.is_empty() || !addrs.iter().all(|a| a.ip().is_loopback()) {
                    return Err(URL_RULE);
                }
                addrs
            }
            _ => return Err(URL_RULE),
        };
        let tail = tail.split('#').next().unwrap_or_default();
        let target = if tail.starts_with('/') {
            tail.to_owned()
        } else {
            format!("/{tail}")
        };
        Ok(Self {
            url: url.to_owned(),
            host: authority.to_owned(),
            target,
            addrs,
        })
    }
}

/// The request headers of a server from its env-file, checked.
///
/// # Errors
/// A header the transport writes itself, or a name or value HTTP cannot carry; the value is
/// never named.
pub(crate) fn check_headers(
    entries: Vec<(String, String)>,
) -> Result<Vec<(String, String)>, String> {
    for (name, value) in &entries {
        let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
        if name.is_empty() || !name.bytes().all(token) {
            return Err(format!("invalid header name {name:?}"));
        }
        if TRANSPORT_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(format!("header {name} is set by the MCP transport"));
        }
        if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(format!(
                "the value of header {name} holds a line break or NUL"
            ));
        }
    }
    Ok(entries)
}

/// Each header value whole, then each of its words of 8 characters or more, so a token is
/// found without its scheme.
pub(crate) fn secrets(headers: &[(String, String)]) -> Vec<String> {
    let mut secrets: Vec<String> = headers.iter().map(|(_, v)| v.clone()).collect();
    for (_, value) in headers {
        secrets.extend(
            value
                .split_whitespace()
                .filter(|word| word.chars().count() >= 8 && *word != value)
                .map(str::to_owned),
        );
    }
    secrets
}

/// One request: its method line, extra headers and body.
struct Exchange<'a> {
    method: &'static str,
    headers: &'a [(&'a str, &'a str)],
    body: &'a [u8],
}

/// An open session with one server; calls from several threads take ids from one counter.
pub(crate) struct Session {
    /// The configured server name.
    pub(crate) server: String,
    endpoint: Endpoint,
    headers: Vec<(String, String)>,
    connect_timeout: Option<Duration>,
    id: Option<String>,
    protocol: String,
    next_id: AtomicU64,
    /// The `capabilities` of the server's initialize answer.
    pub(crate) capabilities: Map<String, Value>,
}

impl Session {
    /// Probes the endpoint, then runs the initialize handshake within `deadline`.
    ///
    /// # Errors
    /// The text of the failure.
    pub(crate) fn open(
        server: &str,
        endpoint: Endpoint,
        headers: Vec<(String, String)>,
        connect_timeout: Option<Duration>,
        deadline: Option<Instant>,
    ) -> Result<Self, String> {
        let protocol = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("mcp-protocol-version"))
            .map_or_else(|| LATEST_HANDSHAKE_VERSION.to_owned(), |(_, v)| v.clone());
        let headers: Vec<(String, String)> = headers
            .into_iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("mcp-protocol-version"))
            .collect();
        let mut session = Self {
            server: server.to_owned(),
            endpoint,
            headers,
            connect_timeout,
            id: None,
            protocol,
            next_id: AtomicU64::new(0),
            capabilities: Map::new(),
        };
        session.preflight(deadline)?;
        let params = json!({
            "protocolVersion": LATEST_HANDSHAKE_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "mcp", "version": "0.1.0"},
        });
        let (result, head) = session
            .round_trip("initialize", Some(params), deadline, None)
            .map_err(|f| f.to_string())?;
        if let Some(id) = head.session_id.filter(|id| !id.is_empty()) {
            session.id = Some(id);
        }
        let version = result.get("protocolVersion").and_then(Value::as_str);
        match version {
            Some(v) if HANDSHAKE_VERSIONS.contains(&v) => v.clone_into(&mut session.protocol),
            other => {
                return Err(format!(
                    "Unsupported protocol version from the server: {}",
                    other.unwrap_or("None")
                ));
            }
        }
        session.capabilities = match result.get("capabilities") {
            Some(Value::Object(caps)) => caps.clone(),
            _ => return Err("initialize result has no capabilities".into()),
        };
        session
            .notify("notifications/initialized", deadline)
            .map_err(|f| f.to_string())?;
        Ok(session)
    }

    /// Whether the server advertises the request family `capability`.
    pub(crate) fn advertises(&self, capability: &str) -> bool {
        self.capabilities
            .get(capability)
            .is_some_and(|c| !c.is_null())
    }

    /// The header values and the session id, which never leave the request.
    pub(crate) fn secrets(&self) -> Vec<String> {
        let mut secrets = secrets(&self.headers);
        secrets.extend(self.id.iter().cloned());
        secrets
    }

    /// Sends request `method` with `params` and returns its result; `interrupt` ends it early.
    ///
    /// # Errors
    /// [`Failure`] of the exchange or the JSON-RPC error answer.
    pub(crate) fn request(
        &self,
        method: &str,
        params: Option<Value>,
        deadline: Option<Instant>,
        interrupt: Option<&Interrupt>,
    ) -> Result<Value, Failure> {
        self.round_trip(method, params, deadline, interrupt)
            .map(|(result, _)| result)
    }

    /// The items of `method` across all its pages, following `nextCursor` for at most
    /// [`MAX_LIST_PAGES`] pages.
    ///
    /// # Errors
    /// [`Failure`] of any page, or a page whose `items` is not a list.
    pub(crate) fn list(
        &self,
        method: &str,
        items: &str,
        deadline: Option<Instant>,
        interrupt: Option<&Interrupt>,
    ) -> Result<Vec<Value>, Failure> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = cursor.take().map(|c| json!({ "cursor": c }));
            let page = self.request(method, params, deadline, interrupt)?;
            match page.get(items) {
                Some(Value::Array(list)) => all.extend(list.iter().cloned()),
                None | Some(Value::Null) => {}
                Some(_) => {
                    return Err(Failure::rpc(
                        INVALID_REQUEST,
                        format!("{method} result has a {items} field that is not a list"),
                    ));
                }
            }
            match page.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
                _ => break,
            }
        }
        Ok(all)
    }

    /// Ends the session on the server with `DELETE`, if it has an id; a failure is not reported.
    pub(crate) fn close(&self) {
        const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
        if self.id.is_none() {
            return;
        }
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let headers = self.mcp_headers();
        let exchange = Exchange {
            method: "DELETE",
            headers: &headers,
            body: b"",
        };
        if let Ok((mut conn, _)) = self.send(&exchange, Some(deadline), self.connect_timeout, None)
        {
            // The status is informative only.
            let _ = conn.head();
        }
    }

    /// The headers of every request after the handshake started: the server's own, then the
    /// transport's.
    fn mcp_headers(&self) -> Vec<(&str, &str)> {
        let mut headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        headers.push(("accept", "application/json, text/event-stream"));
        headers.push(("content-type", "application/json"));
        if let Some(id) = &self.id {
            headers.push(("mcp-session-id", id));
        }
        headers.push(("mcp-protocol-version", &self.protocol));
        headers
    }

    /// Posts the notification `method`; any answer but a transport failure is accepted.
    fn notify(&self, method: &str, deadline: Option<Instant>) -> Result<(), Failure> {
        let body = json!({"jsonrpc": "2.0", "method": method}).to_string();
        let headers = self.mcp_headers();
        let exchange = Exchange {
            method: "POST",
            headers: &headers,
            body: body.as_bytes(),
        };
        let (mut conn, _) = self.send(&exchange, deadline, self.connect_timeout, None)?;
        conn.head()
            .map(|_| ())
            .map_err(|e| transport(e, conn.deadline))
    }

    /// Posts request `method` and reads its answer and head.
    fn round_trip(
        &self,
        method: &str,
        params: Option<Value>,
        deadline: Option<Instant>,
        interrupt: Option<&Interrupt>,
    ) -> Result<(Value, Head), Failure> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        let body = message.to_string();
        let headers = self.mcp_headers();
        let exchange = Exchange {
            method: "POST",
            headers: &headers,
            body: body.as_bytes(),
        };
        let (mut conn, running) =
            self.send(&exchange, deadline, self.connect_timeout, interrupt)?;
        let answer = self.answer(&mut conn);
        if let (Some(interrupt), Some(running)) = (interrupt, &running) {
            interrupt.detach_socket(running);
            if answer.is_err() && interrupt.is_interrupted() {
                return Err(Failure::Interrupted);
            }
        }
        answer
    }

    /// The result of the answer on `conn` and its head.
    fn answer(&self, conn: &mut Conn<TcpStream>) -> Result<(Value, Head), Failure> {
        let fail = |e, conn: &Conn<TcpStream>| transport(e, conn.deadline);
        let head = conn.head().map_err(|e| fail(e, conn))?;
        let content_type = media_type(head.content_type.as_deref());
        if (300..400).contains(&head.status) {
            return Err(Failure::Transport {
                kind: "HTTPStatusError",
                text: format!("redirect not followed: status code {}", head.status),
            });
        }
        if head.status == 202 {
            return Err(Failure::rpc(
                INVALID_REQUEST,
                "server answered a request with 202 Accepted",
            ));
        }
        if head.status >= 400 {
            if content_type == "application/json"
                && let Ok(body) = whole_body(conn, &head)
                && let Ok(Value::Object(message)) = serde_json::from_slice::<Value>(&body)
                && let Some(error) = message.get("error")
            {
                return Err(rpc_error(error));
            }
            return Err(match head.status {
                404 if self.id.is_some() => Failure::rpc(INVALID_REQUEST, "Session terminated"),
                404 => Failure::rpc(METHOD_NOT_FOUND, "Not Found"),
                _ => Failure::rpc(INTERNAL_ERROR, "Server returned an error response"),
            });
        }
        let result = match content_type.as_str() {
            "application/json" => {
                let body = whole_body(conn, &head).map_err(|e| fail(e, conn))?;
                let message = serde_json::from_slice::<Value>(&body).map_err(|e| {
                    Failure::rpc(PARSE_ERROR, format!("Failed to parse JSON response: {e}"))
                })?;
                response(&message).unwrap_or_else(|| {
                    Err(Failure::rpc(
                        PARSE_ERROR,
                        "Failed to parse JSON response: not a JSON-RPC response",
                    ))
                })?
            }
            "text/event-stream" => event_stream(conn, &head)?,
            _ => {
                return Err(Failure::rpc(
                    INVALID_REQUEST,
                    format!("Unexpected content type: {content_type}"),
                ));
            }
        };
        Ok((result, head))
    }

    /// Fails fast when the URL answers a successful request with a content type no MCP
    /// endpoint uses; every transport failure passes.
    fn preflight(&self, deadline: Option<Instant>) -> Result<(), String> {
        let limit = Instant::now() + PREFLIGHT_TIMEOUT;
        let deadline = Some(deadline.map_or(limit, |d| d.min(limit)));
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        let probe = |method, extra: &[(&str, &str)], body: &[u8]| {
            let mut all = headers.clone();
            all.extend_from_slice(extra);
            let exchange = Exchange {
                method,
                headers: &all,
                body,
            };
            let (mut conn, _) = self
                .send(&exchange, deadline, Some(PREFLIGHT_TIMEOUT), None)
                .ok()?;
            conn.head().ok()
        };
        let Some(mut head) = probe("HEAD", &[], b"") else {
            return Ok(());
        };
        if head.status == 405 || head.status == 501 {
            let Some(get) = probe("GET", &[], b"") else {
                return Ok(());
            };
            head = get;
        }
        let ct = media_type(head.content_type.as_deref());
        if !ct.is_empty()
            && !MCP_CONTENT_TYPES.contains(&ct.as_str())
            && (200..300).contains(&head.status)
        {
            let body = br#"{"jsonrpc":"2.0","id":"_probe","method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"ulei-probe","version":"0.1"}}}"#;
            let extra = [
                ("Content-Type", "application/json"),
                ("Accept", "application/json, text/event-stream"),
            ];
            let Some(post) = probe("POST", &extra, body) else {
                return Ok(());
            };
            if (200..300).contains(&post.status)
                && MCP_CONTENT_TYPES.contains(&media_type(post.content_type.as_deref()).as_str())
            {
                head = post;
            }
        }
        let ct = media_type(head.content_type.as_deref());
        if !(200..300).contains(&head.status)
            || ct.is_empty()
            || MCP_CONTENT_TYPES.contains(&ct.as_str())
        {
            return Ok(());
        }
        Err(format!(
            "MCP server '{}' at {} returned Content-Type '{ct}', not an MCP response (expected \
             one of: {}). The URL most likely points at a web page rather than an MCP endpoint \
             \u{2014} check it resolves to a Streamable HTTP / SSE endpoint (e.g. \
             https://host/mcp, not https://host/).",
            self.server,
            self.endpoint.url,
            MCP_CONTENT_TYPES.join(", ")
        ))
    }

    /// Connects within `connect` and the deadline, writes `exchange` and registers the socket
    /// with `interrupt`.
    fn send(
        &self,
        exchange: &Exchange<'_>,
        deadline: Option<Instant>,
        connect: Option<Duration>,
        interrupt: Option<&Interrupt>,
    ) -> Result<(Conn<TcpStream>, Option<Running>), Failure> {
        let left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let bound = |limit: Duration| left().map_or(limit, |l| l.min(limit));
        if left().is_some_and(|l| l.is_zero()) {
            return Err(Failure::TimedOut);
        }
        let stream = self.connect(
            bound(connect.unwrap_or(crate::config::MAX_TIMEOUT)),
            deadline,
        )?;
        let io = |kind| {
            move |e: std::io::Error| Failure::Transport {
                kind,
                text: e.to_string(),
            }
        };
        stream
            .set_write_timeout(Some(bound(READ_TIMEOUT)))
            .map_err(io("WriteError"))?;
        stream
            .set_read_timeout(Some(bound(READ_TIMEOUT)))
            .map_err(io("ReadError"))?;
        let running = match interrupt {
            Some(interrupt) => {
                let clone = stream.try_clone().map_err(io("ConnectError"))?;
                Some(interrupt.attach_socket(clone).ok_or(Failure::Interrupted)?)
            }
            None => None,
        };
        let mut request = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\n",
            exchange.method, self.endpoint.target, self.endpoint.host
        );
        for (name, value) in exchange.headers {
            let _ = write!(request, "{name}: {value}\r\n");
        }
        if exchange.method == "POST" {
            let _ = write!(request, "Content-Length: {}\r\n", exchange.body.len());
        }
        request.push_str("Connection: close\r\n\r\n");
        let mut bytes = request.into_bytes();
        bytes.extend_from_slice(exchange.body);
        (&stream)
            .write_all(&bytes)
            .map_err(|e| match gateway::io_error(e) {
                gateway::Error::Timeout if left().is_some_and(|l| l.is_zero()) => Failure::TimedOut,
                gateway::Error::Timeout => Failure::Transport {
                    kind: "WriteTimeout",
                    text: "timed out".into(),
                },
                other => Failure::Transport {
                    kind: "WriteError",
                    text: other.to_string(),
                },
            })?;
        let mut conn = Conn::new(stream, Arc::new(AtomicBool::new(false)), READ_TIMEOUT);
        conn.deadline = deadline;
        Ok((conn, running))
    }

    /// A stream to the first address that accepts within `limit`.
    fn connect(&self, limit: Duration, deadline: Option<Instant>) -> Result<TcpStream, Failure> {
        let mut last = None;
        for addr in &self.endpoint.addrs {
            match TcpStream::connect_timeout(addr, limit) {
                Ok(stream) => return Ok(stream),
                Err(e) => last = Some(e),
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Failure::TimedOut);
        }
        Err(match last {
            Some(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                Failure::Transport {
                    kind: "ConnectTimeout",
                    text: "timed out".into(),
                }
            }
            Some(e) => Failure::Transport {
                kind: "ConnectError",
                text: e.to_string(),
            },
            None => Failure::Transport {
                kind: "ConnectError",
                text: "no address to connect to".into(),
            },
        })
    }
}

/// The lowercase media type of a `Content-Type` value, without parameters.
fn media_type(value: Option<&str>) -> String {
    value
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// The whole body of an answer: by `Content-Length`, chunked, or up to the end of the stream.
fn whole_body(conn: &mut Conn<TcpStream>, head: &Head) -> Result<Vec<u8>, gateway::Error> {
    if !head.chunked && head.length.is_some() {
        return conn.body(head.length, MAX_JSON_BODY);
    }
    let mut chunk = if head.chunked {
        Chunk::Size
    } else {
        Chunk::Close
    };
    let mut body = Vec::new();
    loop {
        match feed(conn, &mut chunk, &mut body) {
            Ok(()) if body.len() > MAX_JSON_BODY => {
                return Err(gateway::Error::Malformed(gateway::Malformed::Body(
                    "body over the limit",
                )));
            }
            Ok(()) => {}
            Err(gateway::Error::Cut) => return Ok(body),
            Err(e) => return Err(e),
        }
    }
}

/// Reads server-sent events until the JSON-RPC response of the request, skipping
/// notifications, requests, priming events and events other than `message`.
fn event_stream(conn: &mut Conn<TcpStream>, head: &Head) -> Result<Value, Failure> {
    if head.length.is_some() && !head.chunked {
        return Err(Failure::protocol("event stream with a Content-Length"));
    }
    let mut chunk = if head.chunked {
        Chunk::Size
    } else {
        Chunk::Close
    };
    let (mut line, mut scanned) = (Vec::new(), 0);
    let (mut data, mut event): (Option<String>, Option<String>) = (None, None);
    loop {
        let raw = match next_line(conn, &mut chunk, &mut line, &mut scanned) {
            Ok(raw) => raw,
            Err(gateway::Error::Cut) => {
                return Err(Failure::rpc(
                    CONNECTION_CLOSED,
                    "SSE stream ended without a response",
                ));
            }
            Err(gateway::Error::Malformed(gateway::Malformed::LineTooLarge)) => {
                return Err(Failure::protocol(format!(
                    "event line over {MAX_LINE} bytes"
                )));
            }
            Err(e) => return Err(transport(e, conn.deadline)),
        };
        let text = String::from_utf8_lossy(&raw);
        let text = text.strip_suffix('\r').unwrap_or(&text);
        if text.is_empty() {
            let (data, event) = (data.take(), event.take());
            if event.as_deref().unwrap_or("message") != "message" {
                continue;
            }
            let Some(data) = data.filter(|d| !d.is_empty()) else {
                continue;
            };
            let message = serde_json::from_str::<Value>(&data).map_err(|e| {
                Failure::rpc(PARSE_ERROR, format!("Failed to parse SSE message: {e}"))
            })?;
            if let Some(result) = response(&message) {
                return result;
            }
            continue;
        }
        if text.starts_with(':') {
            continue;
        }
        let (field, value) = text.split_once(':').unwrap_or((text, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                let data = data.get_or_insert_with(String::new);
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value);
            }
            "event" => event = Some(value.to_owned()),
            _ => {}
        }
    }
}

/// The outcome of a JSON-RPC response, `None` for any other message.
fn response(message: &Value) -> Option<Result<Value, Failure>> {
    let message = message.as_object()?;
    if let Some(error) = message.get("error") {
        return Some(Err(rpc_error(error)));
    }
    message.get("result").map(|result| Ok(result.clone()))
}

/// The failure of a JSON-RPC `error` object.
fn rpc_error(error: &Value) -> Failure {
    Failure::Rpc {
        code: error
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or(INTERNAL_ERROR),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

/// The failure of a read; a passed `deadline` is [`Failure::TimedOut`].
fn transport(e: gateway::Error, deadline: Option<Instant>) -> Failure {
    match e {
        gateway::Error::Timeout if deadline.is_some_and(|d| Instant::now() >= d) => {
            Failure::TimedOut
        }
        gateway::Error::Timeout => Failure::Transport {
            kind: "ReadTimeout",
            text: "timed out".into(),
        },
        gateway::Error::Io(e) => Failure::Transport {
            kind: "ReadError",
            text: e.to_string(),
        },
        gateway::Error::Cut => Failure::protocol("Server disconnected without sending a response."),
        gateway::Error::Malformed(what) => Failure::protocol(what.to_string()),
        other => Failure::protocol(other.to_string()),
    }
}
