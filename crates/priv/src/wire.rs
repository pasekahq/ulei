//! The module-internal wire between `ulei-priv` and `ulei-priv serve` (ADR-0073 section 7).
//!
//! One request line per connection, at most [`MAX_REQUEST`] bytes with its newline:
//! `{"op":"list"}` or `{"op":"run","id":"<id>","args":["<value>"]}`. The reply is one JSON
//! header line, `{"decision":"refused","reason":"<reason>"}`, `{"decision":"ok","commands":
//! [{"id":..,"arg":[..]}]}` for `list`, or for `run` `{"decision":"ok","exit":..,"signal":..,
//! "timed_out":..,"stdout_bytes":N,"stderr_bytes":M,"truncated":..}` followed by exactly N bytes
//! of stdout and M bytes of stderr.

use std::io::{self, Read};

use serde_json::{Map, Value};

/// Bytes of a request line, its newline included.
pub const MAX_REQUEST: usize = 4096;

/// A decoded request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// The ids and argument choices of the policy.
    List,
    /// One command.
    Run {
        /// Its id.
        id: String,
        /// The arguments asked for; the policy allows none or one.
        args: Vec<String>,
    },
}

/// Why a request line is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Longer than [`MAX_REQUEST`].
    TooLarge,
    /// Not one line of a request object, or not read in time.
    Malformed,
}

impl Fault {
    /// The audit's reason.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::Malformed => "malformed",
        }
    }
}

/// Reads one request line of at most [`MAX_REQUEST`] bytes from `r`.
///
/// # Errors
/// [`Fault::TooLarge`] past the cap, [`Fault::Malformed`] on EOF before the newline or a read
/// error (the socket's timeout included).
pub fn read_line(r: &mut impl Read) -> Result<Vec<u8>, Fault> {
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 1024];
    loop {
        let n = match r.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Fault::Malformed),
        };
        if n == 0 {
            return Err(Fault::Malformed);
        }
        if let Some(end) = chunk[..n].iter().position(|&b| b == b'\n') {
            if buf.len() + end + 1 > MAX_REQUEST {
                return Err(Fault::TooLarge);
            }
            buf.extend_from_slice(&chunk[..end]);
            return Ok(buf);
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() >= MAX_REQUEST {
            return Err(Fault::TooLarge);
        }
    }
}

/// Decodes a request line without its newline.
///
/// # Errors
/// [`Fault::Malformed`] for anything but the two shapes, unknown keys included.
pub fn decode(line: &[u8]) -> Result<Request, Fault> {
    let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(line) else {
        return Err(Fault::Malformed);
    };
    let only = |keys: &[&str]| obj.keys().all(|k| keys.contains(&k.as_str()));
    match obj.get("op").and_then(Value::as_str) {
        Some("list") if only(&["op"]) => Ok(Request::List),
        Some("run") if only(&["op", "id", "args"]) => {
            let id = obj
                .get("id")
                .and_then(Value::as_str)
                .ok_or(Fault::Malformed)?;
            let args = match obj.get("args") {
                None => Vec::new(),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or(Fault::Malformed)?,
                Some(_) => return Err(Fault::Malformed),
            };
            Ok(Request::Run {
                id: id.to_owned(),
                args,
            })
        }
        _ => Err(Fault::Malformed),
    }
}

/// The request line of `req`, with its newline.
pub fn encode(req: &Request) -> Vec<u8> {
    let mut obj = Map::new();
    match req {
        Request::List => {
            obj.insert("op".into(), "list".into());
        }
        Request::Run { id, args } => {
            obj.insert("op".into(), "run".into());
            obj.insert("id".into(), id.as_str().into());
            obj.insert("args".into(), args.clone().into());
        }
    }
    let mut line = Value::Object(obj).to_string().into_bytes();
    line.push(b'\n');
    line
}

/// The header of a refusal, with its newline.
pub fn refused(reason: &str) -> Vec<u8> {
    let mut line = serde_json::json!({"decision": "refused", "reason": reason})
        .to_string()
        .into_bytes();
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for req in [
            Request::List,
            Request::Run {
                id: "a".into(),
                args: vec!["x".into()],
            },
            Request::Run {
                id: "b".into(),
                args: vec![],
            },
        ] {
            let line = encode(&req);
            let mut r = line.as_slice();
            assert_eq!(decode(&read_line(&mut r).expect("line")), Ok(req));
        }
    }

    #[test]
    fn refuses_malformed_and_oversized() {
        for bad in [
            &b"not json"[..],
            b"[]",
            b"{}",
            b"{\"op\":\"exec\"}",
            b"{\"op\":\"list\",\"x\":1}",
            b"{\"op\":\"run\"}",
            b"{\"op\":\"run\",\"id\":1}",
            b"{\"op\":\"run\",\"id\":\"a\",\"args\":\"x\"}",
            b"{\"op\":\"run\",\"id\":\"a\",\"args\":[1]}",
            b"{\"op\":\"run\",\"id\":\"a\",\"env\":{\"LD_PRELOAD\":\"x\"}}",
        ] {
            assert_eq!(
                decode(bad),
                Err(Fault::Malformed),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let mut no_newline: &[u8] = b"{\"op\":\"list\"}";
        assert_eq!(read_line(&mut no_newline), Err(Fault::Malformed));
        let big = vec![b' '; MAX_REQUEST];
        assert_eq!(read_line(&mut big.as_slice()), Err(Fault::TooLarge));
        let mut just = vec![b' '; MAX_REQUEST - 1];
        just.push(b'\n');
        assert_eq!(
            read_line(&mut just.as_slice()).map(|l| l.len()),
            Ok(MAX_REQUEST - 1)
        );
    }
}
