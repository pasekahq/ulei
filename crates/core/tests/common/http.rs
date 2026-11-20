//! HTTP/1.1 framing of the fake daemons the integration tests serve on Unix sockets.

use std::io::Read;
use std::os::unix::net::UnixStream;

/// An HTTP/1.1 answer with `status` and the JSON of `body`, closing the connection.
pub(crate) fn json_answer(status: u16, body: &impl serde::Serialize) -> Vec<u8> {
    let body = serde_json::to_string(body).expect("answer JSON");
    format!(
        "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// `METHOD /path` and the `Content-Length` body of one request.
pub(crate) fn read_request(stream: &mut UnixStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut piece = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut piece) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&piece[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let length = head
        .split("\r\n")
        .find_map(|l| {
            let (name, value) = l.split_once(": ")?;
            name.eq_ignore_ascii_case("content-length").then_some(value)
        })
        .map_or(Some(0), |v| v.parse::<usize>().ok())?;
    while buf.len() < end + length {
        match stream.read(&mut piece) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&piece[..n]),
        }
    }
    let target = head.split("\r\n").next()?.rsplit_once(' ')?.0.to_owned();
    Some((target, buf.split_off(end)))
}
