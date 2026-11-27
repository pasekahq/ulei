//! The night's clients of the garden's C7 night methods and `propose`, and of the archive's C10
//! `list` and `forest_put`: HTTP/1.1 with JSON bodies over their Unix sockets, one request per connection, through
//! the engine's gateway reader.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::gateway::{self, Malformed};
use crate::wire::{c7, c10};

/// The connect deadline of a call.
const CONNECT: Duration = Duration::from_secs(5);
/// The deadline of every read and write: `seedbed_done` of 100 records answers after the edge
/// has decided them all.
const IDLE: Duration = Duration::from_secs(120);
/// Most bytes of an answer: a C7 or C10 body.
const MAX_BODY: usize = 1024 * 1024;

/// Why a call failed.
#[derive(Debug)]
pub enum Error {
    /// Connecting, writing or reading failed, or the answer breaks the wire format.
    Unreachable(&'static str, gateway::Error),
    /// The daemon answered with an `Error` body.
    Rejected {
        /// Which daemon.
        daemon: &'static str,
        /// HTTP status.
        status: u16,
        /// The error code.
        code: String,
        /// The message.
        message: String,
    },
}

impl Error {
    /// The HTTP status of a rejection, `None` for a transport failure.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Rejected { status, .. } => Some(*status),
            Self::Unreachable(..) => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(daemon, e) => write!(f, "{daemon}: {e}"),
            Self::Rejected {
                daemon,
                status,
                code,
                message,
            } => write!(f, "{daemon} refused the call: {status} {code}: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// One daemon at one socket.
#[derive(Debug, Clone)]
struct Http {
    name: &'static str,
    client: gateway::Client,
}

impl Http {
    fn new(name: &'static str, socket: PathBuf) -> Self {
        Self {
            name,
            client: gateway::Client::new(socket, CONNECT, IDLE),
        }
    }

    /// The body of the `200` answer to `POST /v1/<method>` with `body`, else the `Error` body's
    /// code and message.
    fn call(&self, method: &str, body: serde_json::Result<Vec<u8>>) -> Result<Vec<u8>, Error> {
        let body = body.map_err(|e| Error::Unreachable(self.name, gateway::Error::Io(e.into())))?;
        let line = format!("POST /v1/{method}");
        let (status, answer) = self
            .client
            .exchange(&line, Some(&body), None, MAX_BODY)
            .map_err(|e| Error::Unreachable(self.name, e))?;
        if status == 200 {
            return Ok(answer);
        }
        let error: serde_json::Value = serde_json::from_slice(&answer).map_err(|_| {
            Error::Unreachable(
                self.name,
                gateway::Error::Malformed(Malformed::Body("non-200 body is not an Error")),
            )
        })?;
        let field = |name| {
            error
                .get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Err(Error::Rejected {
            daemon: self.name,
            status,
            code: field("code"),
            message: field("message"),
        })
    }

    fn malformed(&self, what: &'static str) -> Error {
        Error::Unreachable(self.name, gateway::Error::Malformed(Malformed::Body(what)))
    }
}

/// The garden's C7 methods the night calls.
#[derive(Debug, Clone)]
pub struct Garden(Http);

impl Garden {
    /// A client of the garden at `socket`.
    #[must_use]
    pub fn new(socket: PathBuf) -> Self {
        Self(Http::new("garden", socket))
    }

    /// C7 `seedbed_take`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn seedbed_take(
        &self,
        request: &c7::SeedbedTakeRequest,
    ) -> Result<c7::SeedbedTakeResponse, Error> {
        let answer = self.0.call("seedbed_take", serde_json::to_vec(request))?;
        serde_json::from_slice(&answer).map_err(|_| self.0.malformed("not a SeedbedTakeResponse"))
    }

    /// C7 `seedbed_done`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn seedbed_done(
        &self,
        request: &c7::SeedbedDoneRequest,
    ) -> Result<c7::SeedbedDoneResponse, Error> {
        let answer = self.0.call("seedbed_done", serde_json::to_vec(request))?;
        serde_json::from_slice(&answer).map_err(|_| self.0.malformed("not a SeedbedDoneResponse"))
    }

    /// C7 `propose`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn propose(&self, request: &c7::ProposeRequest) -> Result<c7::ProposeResponse, Error> {
        let answer = self.0.call("propose", serde_json::to_vec(request))?;
        serde_json::from_slice(&answer).map_err(|_| self.0.malformed("not a ProposeResponse"))
    }

    /// C7 `forget_take`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn forget_take(
        &self,
        request: &c7::ForgetTakeRequest,
    ) -> Result<c7::ForgetTakeResponse, Error> {
        let answer = self.0.call("forget_take", serde_json::to_vec(request))?;
        serde_json::from_slice(&answer).map_err(|_| self.0.malformed("not a ForgetTakeResponse"))
    }

    /// C7 `forget_done`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn forget_done(
        &self,
        request: &c7::ForgetDoneRequest,
    ) -> Result<c7::ForgetDoneResponse, Error> {
        let answer = self.0.call("forget_done", serde_json::to_vec(request))?;
        serde_json::from_slice(&answer).map_err(|_| self.0.malformed("not a ForgetDoneResponse"))
    }
}

impl Garden {
    /// C7 `reconcile_take`: the facts waiting with their hints for the judge.
    ///
    /// # Errors
    /// [`Error`].
    pub fn reconcile_take(
        &self,
        request: &c7::ReconcileTakeRequest,
    ) -> Result<c7::ReconcileTakeResponse, Error> {
        let answer = self.0.call("reconcile_take", serde_json::to_vec(request))?;
        serde_json::from_slice(&answer).map_err(|_| self.0.malformed("not a ReconcileTakeResponse"))
    }

    /// C7 `reconcile_done`: the judge's verdicts and counts.
    ///
    /// # Errors
    /// [`Error`].
    pub fn reconcile_done(
        &self,
        request: &c7::ReconcileDoneRequest,
    ) -> Result<c7::ReconcileDoneResponse, Error> {
        let answer = self.0.call("reconcile_done", serde_json::to_vec(request))?;
        serde_json::from_slice(&answer).map_err(|_| self.0.malformed("not a ReconcileDoneResponse"))
    }
}

/// The archive's C10 `list` and the forest's `forest_put`.
#[derive(Debug, Clone)]
pub struct Archive(Http);

impl Archive {
    /// A client of the archive at `socket`.
    #[must_use]
    pub fn new(socket: PathBuf) -> Self {
        Self(Http::new("archive", socket))
    }

    /// C10 `list`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn list(&self, request: &c10::ListRequest) -> Result<c10::ListResponse, Error> {
        let answer = self.0.call("list", serde_json::to_vec(request))?;
        serde_json::from_slice(&answer).map_err(|_| self.0.malformed("not a ListResponse"))
    }

    /// C10 `forest_put`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn forest_put(
        &self,
        request: &c10::ForestPutRequest,
    ) -> Result<c10::ForestPutResponse, Error> {
        let answer = self.0.call("forest_put", serde_json::to_vec(request))?;
        serde_json::from_slice(&answer).map_err(|_| self.0.malformed("not a ForestPutResponse"))
    }
}
