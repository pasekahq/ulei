//! Client of the archive's C10 `search` and `read`: HTTP/1.1 with JSON bodies over its Unix
//! socket, one request per connection, every connect, read and write within 5 s.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use ulei_contracts::generated::c10::{
    Error as ArchiveError, ReadRequest, ReadResponse, SearchRequest, SearchResponse,
};

use crate::gateway::{self, Malformed};

/// The C10 deadline of every read and write of a connection, used for the connect too.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Most bytes of a C10 body.
const MAX_BODY: usize = 1024 * 1024;
/// Most bytes of a `read` answer: an original of at most 1 MiB in base64 with its entries.
const MAX_READ_BODY: usize = 2 * 1024 * 1024;

/// Why an archive call failed.
#[derive(Debug)]
pub enum Error {
    /// Connecting, writing or reading failed, or the answer breaks the wire format.
    Unreachable(gateway::Error),
    /// The archive answered with a C10 `Error`.
    Rejected {
        /// HTTP status of the answer.
        status: u16,
        /// The answer body.
        error: ArchiveError,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(gateway::Error::Io(e)) => write!(f, "archive socket: {e}"),
            Self::Unreachable(gateway::Error::Timeout) => f.write_str("archive timed out"),
            Self::Unreachable(gateway::Error::Malformed(what)) => {
                write!(f, "malformed archive answer: {what}")
            }
            Self::Unreachable(e) => write!(f, "archive: {e}"),
            Self::Rejected { status, error } => write!(
                f,
                "archive refused the call: {status} {}: {}",
                error.code.as_str(),
                error.message
            ),
        }
    }
}

impl std::error::Error for Error {}

/// A client of the archive at one socket; every call opens its own connection.
#[derive(Debug, Clone)]
pub struct Client {
    http: gateway::Client,
}

impl Client {
    /// A client of the archive listening at `socket`.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            http: gateway::Client::new(socket, TIMEOUT, TIMEOUT),
        }
    }

    /// C10 `search`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResponse, Error> {
        let body = serde_json::to_vec(request)
            .map_err(|e| Error::Unreachable(gateway::Error::Io(e.into())))?;
        let answer = self.call("POST /v1/search", &body, MAX_BODY)?;
        serde_json::from_slice(&answer).map_err(|_| malformed("not a SearchResponse"))
    }

    /// C10 `read`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn read(&self, request: &ReadRequest) -> Result<ReadResponse, Error> {
        let body = serde_json::to_vec(request)
            .map_err(|e| Error::Unreachable(gateway::Error::Io(e.into())))?;
        let answer = self.call("POST /v1/read", &body, MAX_READ_BODY)?;
        serde_json::from_slice(&answer).map_err(|_| malformed("not a ReadResponse"))
    }

    /// The body of the `200` answer to `line` with `body`, at most `max` bytes.
    fn call(&self, line: &str, body: &[u8], max: usize) -> Result<Vec<u8>, Error> {
        let (status, answer) = self
            .http
            .exchange(line, Some(body), None, max)
            .map_err(Error::Unreachable)?;
        if status == 200 {
            return Ok(answer);
        }
        match serde_json::from_slice(&answer) {
            Ok(error) => Err(Error::Rejected { status, error }),
            Err(_) => Err(malformed("non-200 body is not an Error")),
        }
    }
}

fn malformed(what: &'static str) -> Error {
    Error::Unreachable(gateway::Error::Malformed(Malformed::Body(what)))
}
