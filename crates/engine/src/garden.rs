//! Client of the garden's C7 `forget`, `pointers`, `propose` and `search`: HTTP/1.1 with JSON
//! bodies over its Unix socket, one request per connection, every connect, read and write within
//! 5 s; a `pointers` call ends within [`POINTERS_DEADLINE`] (ADR-0047 section 3b). A `propose`
//! that times out is sent once more with [`PROPOSE_RETRY_TIMEOUT`]: the garden serves one
//! connection at a time, so a busy daemon can write the record and answer after the client gave
//! up, and the seedbed answers the same candidate again with the record it already holds.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ulei_contracts::generated::c7::{
    Error as GardenError, ForgetRequest, ForgetResponse, PointersRequest, PointersResponse,
    ProposeRequest, ProposeResponse, ReconcileReviewRequest, ReconcileReviewResponse,
    SearchRequest, SearchResponse,
};

use crate::gateway::{self, Malformed};

/// The C7 deadline of every read and write of a connection, used for the connect too.
const TIMEOUT: Duration = Duration::from_secs(5);
/// The whole `pointers` call, connect to last byte: a later answer starts the turn without
/// pointers (ADR-0047 section 3b).
pub const POINTERS_DEADLINE: Duration = Duration::from_millis(150);
/// The connect and every read and write of the second `propose` after the first timed out.
pub const PROPOSE_RETRY_TIMEOUT: Duration = Duration::from_secs(30);
/// Most bytes of a C7 body.
const MAX_BODY: usize = 1024 * 1024;

/// Why a garden call failed.
#[derive(Debug)]
pub enum Error {
    /// Connecting, writing or reading failed, or the answer breaks the wire format.
    Unreachable(gateway::Error),
    /// The garden answered with a C7 `Error`.
    Rejected {
        /// HTTP status of the answer.
        status: u16,
        /// The answer body.
        error: GardenError,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(gateway::Error::Io(e)) => write!(f, "garden socket: {e}"),
            Self::Unreachable(gateway::Error::Timeout) => f.write_str("garden timed out"),
            Self::Unreachable(gateway::Error::Malformed(what)) => {
                write!(f, "malformed garden answer: {what}")
            }
            Self::Unreachable(e) => write!(f, "garden: {e}"),
            Self::Rejected { status, error } => write!(
                f,
                "garden refused the call: {status} {}: {}",
                error.code.as_str(),
                error.message
            ),
        }
    }
}

impl std::error::Error for Error {}

/// A client of the garden at one socket; every call opens its own connection.
#[derive(Debug, Clone)]
pub struct Client {
    http: gateway::Client,
    /// The same socket with the connect and every read and write bounded by
    /// [`POINTERS_DEADLINE`].
    quick: gateway::Client,
    /// The same socket with the connect and every read and write bounded by
    /// [`PROPOSE_RETRY_TIMEOUT`], for the second `propose`.
    patient: gateway::Client,
}

impl Client {
    /// A client of the garden listening at `socket`.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        let socket = socket.into();
        Self {
            http: gateway::Client::new(socket.clone(), TIMEOUT, TIMEOUT),
            quick: gateway::Client::new(socket.clone(), POINTERS_DEADLINE, POINTERS_DEADLINE),
            patient: gateway::Client::new(socket, PROPOSE_RETRY_TIMEOUT, PROPOSE_RETRY_TIMEOUT),
        }
    }

    /// C7 `pointers`, the whole call within [`POINTERS_DEADLINE`].
    ///
    /// # Errors
    /// [`Error`]; a call past the deadline is [`Error::Unreachable`] with
    /// [`gateway::Error::Timeout`].
    pub fn pointers(&self, request: &PointersRequest) -> Result<PointersResponse, Error> {
        let deadline = Instant::now() + POINTERS_DEADLINE;
        let body = serde_json::to_vec(request)
            .map_err(|e| Error::Unreachable(gateway::Error::Io(e.into())))?;
        let answer = answer(self.quick.exchange(
            "POST /v1/pointers",
            Some(&body),
            Some(deadline),
            MAX_BODY,
        ))?;
        serde_json::from_slice(&answer).map_err(|_| malformed("not a PointersResponse"))
    }

    /// C7 `forget`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn forget(&self, request: &ForgetRequest) -> Result<ForgetResponse, Error> {
        let body = serde_json::to_vec(request)
            .map_err(|e| Error::Unreachable(gateway::Error::Io(e.into())))?;
        let answer = self.call("POST /v1/forget", &body)?;
        serde_json::from_slice(&answer).map_err(|_| malformed("not a ForgetResponse"))
    }

    /// C7 `propose`; a call that times out is sent once more, the same body, with
    /// [`PROPOSE_RETRY_TIMEOUT`]. The seedbed answers a candidate it already holds unprocessed
    /// with that record (`Edge::seed`), so the second call writes no second record and returns
    /// the ref of the one a late first call wrote.
    ///
    /// # Errors
    /// [`Error`]; [`Error::Unreachable`] with [`gateway::Error::Timeout`] only when the second
    /// call timed out too.
    pub fn propose(&self, request: &ProposeRequest) -> Result<ProposeResponse, Error> {
        const LINE: &str = "POST /v1/propose";
        let body = serde_json::to_vec(request)
            .map_err(|e| Error::Unreachable(gateway::Error::Io(e.into())))?;
        let answer = match self.call(LINE, &body) {
            Err(Error::Unreachable(gateway::Error::Timeout)) => {
                answer(self.patient.exchange(LINE, Some(&body), None, MAX_BODY))?
            }
            other => other?,
        };
        serde_json::from_slice(&answer).map_err(|_| malformed("not a ProposeResponse"))
    }

    /// C7 `search`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResponse, Error> {
        let body = serde_json::to_vec(request)
            .map_err(|e| Error::Unreachable(gateway::Error::Io(e.into())))?;
        let answer = self.call("POST /v1/search", &body)?;
        serde_json::from_slice(&answer).map_err(|_| malformed("not a SearchResponse"))
    }

    /// C7 `reconcile_review`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn reconcile_review(
        &self,
        request: &ReconcileReviewRequest,
    ) -> Result<ReconcileReviewResponse, Error> {
        let body = serde_json::to_vec(request)
            .map_err(|e| Error::Unreachable(gateway::Error::Io(e.into())))?;
        let answer = self.call("POST /v1/reconcile_review", &body)?;
        serde_json::from_slice(&answer).map_err(|_| malformed("not a ReconcileReviewResponse"))
    }

    /// The body of the `200` answer to `line` with `body`.
    fn call(&self, line: &str, body: &[u8]) -> Result<Vec<u8>, Error> {
        answer(self.http.exchange(line, Some(body), None, MAX_BODY))
    }
}

/// The body of a `200` answer of `exchanged`, else the C7 `Error` it carries.
fn answer(exchanged: Result<(u16, Vec<u8>), gateway::Error>) -> Result<Vec<u8>, Error> {
    let (status, answer) = exchanged.map_err(Error::Unreachable)?;
    if status == 200 {
        return Ok(answer);
    }
    match serde_json::from_slice(&answer) {
        Ok(error) => Err(Error::Rejected { status, error }),
        Err(_) => Err(malformed("non-200 body is not an Error")),
    }
}

fn malformed(what: &'static str) -> Error {
    Error::Unreachable(gateway::Error::Malformed(Malformed::Body(what)))
}
