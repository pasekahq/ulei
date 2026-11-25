//! The HTTP/1.1 of one connection: one request read within its limits, one answer with
//! `Content-Length` and `Connection: close`.

use std::fmt::Write as _;
use std::io::{self, Read, Write};

/// The most bytes of a request head.
pub const MAX_HEAD: usize = 16 * 1024;
/// The most bytes of a request body.
pub const MAX_BODY: usize = 1024 * 1024;

/// One request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The method, as sent.
    pub method: String,
    /// The request target without query.
    pub path: String,
    /// The header fields, names lowercase, in order.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Request {
    /// The value of the first header `name` (lowercase).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// An answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// Extra header fields.
    pub headers: Vec<(&'static str, String)>,
    /// The `Content-Type` of a body.
    pub content_type: Option<&'static str>,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    /// An answer of `status` without body.
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            content_type: None,
            body: Vec::new(),
        }
    }

    /// An `application/json` answer of `status`.
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            content_type: Some("application/json"),
            body: body.to_string().into_bytes(),
        }
    }

    /// The same answer with header `name: value`.
    #[must_use]
    pub fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// Writes the answer to `out`; a `HEAD` request gets no body.
    ///
    /// # Errors
    /// The write error.
    pub fn write(&self, out: &mut impl Write, head_only: bool) -> io::Result<()> {
        let mut text = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        for (name, value) in &self.headers {
            let _ = write!(text, "{name}: {value}\r\n");
        }
        if let Some(ct) = self.content_type {
            let _ = write!(text, "Content-Type: {ct}\r\n");
        }
        let _ = write!(
            text,
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            self.body.len()
        );
        let mut bytes = text.into_bytes();
        if !head_only {
            bytes.extend_from_slice(&self.body);
        }
        out.write_all(&bytes)?;
        out.flush()
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Reads one request from `input`.
///
/// # Errors
/// The status that answers a request outside the limits, or `None` when the peer sent
/// nothing usable (closed, timed out).
pub fn read_request(input: &mut impl Read) -> Result<Request, Option<u16>> {
    let mut buf = Vec::new();
    let mut piece = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err(Some(431));
        }
        match input.read(&mut piece) {
            Ok(0) | Err(_) => return Err(None),
            Ok(n) => buf.extend_from_slice(&piece[..n]),
        }
    };
    if end > MAX_HEAD {
        return Err(Some(431));
    }
    let head = std::str::from_utf8(&buf[..end - 4]).map_err(|_| Some(400))?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (first.next(), first.next(), first.next(), first.next())
    else {
        return Err(Some(400));
    };
    if !version.starts_with("HTTP/1.") || method.is_empty() {
        return Err(Some(400));
    }
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(Some(400))?;
        if name.is_empty() || name.contains([' ', '\t']) {
            return Err(Some(400));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let request = Request {
        method: method.to_owned(),
        path: target.split('?').next().unwrap_or_default().to_owned(),
        headers,
        body: Vec::new(),
    };
    if request.header("transfer-encoding").is_some() {
        return Err(Some(501));
    }
    let length = match request.header("content-length") {
        None => 0,
        Some(v) => v.parse::<usize>().map_err(|_| Some(400))?,
    };
    if length > MAX_BODY {
        return Err(Some(413));
    }
    let mut body = buf.split_off(end);
    while body.len() < length {
        match input.read(&mut piece) {
            Ok(0) | Err(_) => return Err(None),
            Ok(n) => body.extend_from_slice(&piece[..n]),
        }
    }
    if body.len() > length {
        return Err(Some(400));
    }
    Ok(Request { body, ..request })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn reads_a_request_and_refuses_what_is_outside_the_limits() {
        let raw = b"POST /mcp?x=1 HTTP/1.1\r\nHost: 127.0.0.1:1\r\nContent-Length: 2\r\n\r\n{}";
        let r = read_request(&mut &raw[..]).unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/mcp"));
        assert_eq!(r.header("host"), Some("127.0.0.1:1"));
        assert_eq!(r.body, b"{}");
        let big = format!(
            "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert_eq!(read_request(&mut big.as_bytes()), Err(Some(413)));
        let long = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert_eq!(read_request(&mut long.as_bytes()), Err(Some(431)));
        let chunked = b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(read_request(&mut &chunked[..]), Err(Some(501)));
        assert_eq!(read_request(&mut &b"nonsense\r\n\r\n"[..]), Err(Some(400)));
        assert_eq!(
            read_request(&mut &b"POST /mcp HTTP/1.1\r\nContent-Length: 9\r\n\r\n{}"[..]),
            Err(None)
        );
    }
}
