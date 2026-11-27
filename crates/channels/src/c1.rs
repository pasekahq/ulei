//! The channel's side of the C1 wire: the read-only `uncertain` request to the owner inbox.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use ulei_contracts::generated::c1::{
    Error as WireError, ErrorCode, UncertainEvent, UncertainList, UncertainRequest,
};

use crate::uds::{Endpoint, Fault, Wait};

/// Why a C1 call gave no answer the caller can use.
#[derive(Debug)]
pub(crate) enum Error {
    /// The call failed below C1.
    Wire(Fault),
    /// The inbox answered a status other than 200; `code` when its body is a C1 `Error`.
    Status {
        status: u16,
        code: Option<ErrorCode>,
    },
    /// The 200 body is not valid for the method output.
    Malformed(&'static str),
}

impl From<Fault> for Error {
    fn from(fault: Fault) -> Self {
        Self::Wire(fault)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(Fault::Unreachable(e)) => write!(f, "C1 socket: {e}"),
            Self::Wire(Fault::Timeout) => f.write_str("C1 call timed out"),
            Self::Wire(Fault::PeerUid { expected, actual }) => {
                write!(f, "C1 inbox runs as uid {actual}, expected {expected}")
            }
            Self::Wire(Fault::RequestTooLarge(len)) => {
                write!(f, "C1 request body has {len} bytes")
            }
            Self::Wire(Fault::Malformed(what)) | Self::Malformed(what) => {
                write!(f, "malformed C1 answer: {what}")
            }
            Self::Wire(Fault::Cancelled) => f.write_str("C1 call cancelled"),
            Self::Status { status, code } => {
                write!(f, "C1 inbox answered {status}")?;
                match code {
                    Some(code) => write!(f, " {}", code.as_str()),
                    None => Ok(()),
                }
            }
        }
    }
}

/// A C1 client of one inbox socket; every call opens its own connection.
#[derive(Debug, Clone)]
pub(crate) struct Client(Endpoint);

impl Client {
    /// A client of the inbox at `socket`, which must run as this process's effective uid.
    pub(crate) fn new(
        socket: PathBuf,
        connect_timeout: Duration,
        call_timeout: Duration,
    ) -> io::Result<Self> {
        Endpoint::new(socket, connect_timeout, call_timeout).map(Self)
    }

    /// C1 `uncertain` with `limit`: at most `limit` uncertain events, oldest first. The error
    /// message of a non-200 answer is dropped: it may quote a stored row.
    pub(crate) fn uncertain(&self, limit: u64) -> Result<Vec<UncertainEvent>, Error> {
        let request = UncertainRequest { limit: Some(limit) };
        let (status, body) = self
            .0
            .post("uncertain", &request, None, Wait::CallTimeout)?;
        if status != 200 {
            let code = serde_json::from_slice::<WireError>(&body)
                .ok()
                .map(|e| e.code);
            return Err(Error::Status { status, code });
        }
        let list: UncertainList = serde_json::from_slice(&body)
            .map_err(|_| Error::Malformed("200 body is not an UncertainList"))?;
        if list.events.len() as u64 > limit {
            return Err(Error::Malformed(
                "UncertainList has more events than the limit",
            ));
        }
        Ok(list.events)
    }
}
