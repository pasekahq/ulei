//! One request as a client of a wire the garden speaks: `POST /v1/<method>` with a JSON body
//! over HTTP/1.1 on a new connection to a Unix socket, `Connection: close`, every read and write
//! within its timeout.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Why a request got no answer.
#[derive(Debug)]
pub enum PostError {
    /// Connecting, writing or reading failed or missed the timeout.
    Io(io::Error),
    /// The answer has no HTTP/1.1 head with a status.
    Malformed,
}

impl fmt::Display for PostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Malformed => f.write_str("the answer is not HTTP/1.1"),
        }
    }
}

impl std::error::Error for PostError {}

impl From<io::Error> for PostError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Sends `body` to `method` at `socket` and reads the answer to the end of the connection, at
/// most `max` bytes of it: its status and body.
///
/// # Errors
///
/// [`PostError`].
pub fn post(
    socket: &Path,
    method: &str,
    body: &[u8],
    timeout: Duration,
    max: u64,
) -> Result<(u16, Vec<u8>), PostError> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;
    let mut request = format!(
        "POST /v1/{method} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    stream.write_all(&request)?;
    let mut answer = Vec::new();
    stream.take(max).read_to_end(&mut answer)?;
    let head_end = answer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(PostError::Malformed)?;
    let status = answer
        .strip_prefix(b"HTTP/1.1 ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| std::str::from_utf8(code).ok())
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(PostError::Malformed)?;
    answer.drain(..head_end + 4);
    Ok((status, answer))
}
