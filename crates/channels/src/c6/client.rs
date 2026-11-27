//! The channel's side of the C6 wire (ADR-0008, ADR-0033): one synchronous HTTP/1.1 JSON request
//! per connection to the engine's Unix socket, with a connect timeout and a call timeout.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use ulei_contracts::generated::c6::{
    Ack, DeliveryReport, Error as WireError, ErrorCode, InboxTurns, InboxTurnsRequest,
    InterruptRequest, InterruptResponse, RouteSet, SessionStatus, SetRouteRequest, StatusRequest,
    SubmitTurnRequest, SubmitTurnResponse, TurnEvent, TurnEventType, TurnEventsPage,
    TurnEventsRequest, TurnOutcome, TurnState,
};

use crate::uds::{Cancel, Endpoint, Fault, MAX_BODY, Wait};

/// Why a C6 call failed.
#[derive(Debug)]
pub enum Error {
    /// The engine could not be reached: no socket, connection refused, a socket failure, or
    /// the end of the stream before any byte of the answer.
    Unreachable(io::Error),
    /// The connect or the call timeout passed.
    Timeout,
    /// The engine runs as another uid; nothing was sent.
    PeerUid {
        /// Uid the client requires of the engine.
        expected: u32,
        /// `SO_PEERCRED` uid of the engine.
        actual: u32,
    },
    /// The request body is over 512 KiB; nothing was sent.
    RequestTooLarge(usize),
    /// The answer is not HTTP/1.1 JSON within the limits, or not valid for its definition.
    Malformed(&'static str),
    /// The engine answered with a C6 `Error`.
    Remote {
        /// Error class.
        code: ErrorCode,
        /// Diagnostic of the engine.
        message: String,
    },
    /// The call's [`Cancel`] fired.
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "C6 socket: {e}"),
            Self::Timeout => f.write_str("C6 call timed out"),
            Self::PeerUid { expected, actual } => {
                write!(f, "C6 engine runs as uid {actual}, expected {expected}")
            }
            Self::RequestTooLarge(len) => {
                write!(f, "C6 request body has {len} bytes, at most {MAX_BODY}")
            }
            Self::Malformed(what) => write!(f, "malformed C6 answer: {what}"),
            Self::Remote { code, message } => {
                write!(f, "C6 engine error {}: {message}", code.as_str())
            }
            Self::Cancelled => f.write_str("C6 call cancelled"),
        }
    }
}

impl From<Fault> for Error {
    fn from(fault: Fault) -> Self {
        match fault {
            Fault::Unreachable(e) => Self::Unreachable(e),
            Fault::Timeout => Self::Timeout,
            Fault::PeerUid { expected, actual } => Self::PeerUid { expected, actual },
            Fault::RequestTooLarge(len) => Self::RequestTooLarge(len),
            Fault::Malformed(what) => Self::Malformed(what),
            Fault::Cancelled => Self::Cancelled,
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreachable(e) => Some(e),
            _ => None,
        }
    }
}

/// A page of C6 `turn_events`: where the turn is, and its events after the request's cursor,
/// oldest first; the last one's `seq` is the cursor plus their number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventsPage {
    /// Where the turn is.
    pub state: TurnState,
    /// The events.
    pub events: Vec<Event>,
}

/// One event of a turn with the payload its type carries (ADR-0031 section 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The turn started running.
    Start,
    /// An assistant message while the turn runs.
    Interim(String),
    /// The turn's answer.
    Final(String),
    /// The agent set emoji `emoji` as its reaction on message `message_id` of the chat
    /// (ADR-0088).
    Reaction {
        /// The chat's message.
        message_id: i64,
        /// The emoji.
        emoji: String,
    },
    /// The turn ended; `line` is present for `quota_exhausted`, `context_exhausted` and `failed`.
    End {
        /// How it ended.
        outcome: TurnOutcome,
        /// Its one line.
        line: Option<String>,
    },
    /// The turn left the queue without running.
    Dropped,
}

impl Event {
    fn of(event: TurnEvent) -> Result<Self, Error> {
        let lacks = Error::Malformed("TurnEvent lacks the payload of its type");
        if event.r#type == TurnEventType::Reaction {
            let message_id = event.message_id.and_then(|id| i64::try_from(id).ok());
            return match (message_id, event.text) {
                (Some(message_id), Some(emoji)) => Ok(Self::Reaction { message_id, emoji }),
                _ => Err(lacks),
            };
        }
        Ok(match (event.r#type, event.text, event.outcome) {
            (TurnEventType::Start, ..) => Self::Start,
            (TurnEventType::Interim, Some(text), _) => Self::Interim(text),
            (TurnEventType::Final, Some(text), _) => Self::Final(text),
            (TurnEventType::End, _, Some(outcome)) => {
                let lined = matches!(
                    outcome,
                    TurnOutcome::QuotaExhausted
                        | TurnOutcome::ContextExhausted
                        | TurnOutcome::Failed
                );
                if lined != event.line.is_some() {
                    return Err(lacks);
                }
                Self::End {
                    outcome,
                    line: event.line,
                }
            }
            (TurnEventType::Dropped, ..) => Self::Dropped,
            _ => return Err(lacks),
        })
    }
}

/// A C6 client of one engine socket; every call opens its own connection.
#[derive(Debug, Clone)]
pub struct Client(Endpoint);

impl Client {
    /// A client of the engine at `socket`, which must run as this process's effective uid.
    /// `connect_timeout` bounds the connect, `call_timeout` the whole call from connect to the
    /// last answer byte.
    ///
    /// # Errors
    /// The effective uid cannot be read.
    pub fn new(
        socket: impl Into<PathBuf>,
        connect_timeout: Duration,
        call_timeout: Duration,
    ) -> io::Result<Self> {
        Endpoint::new(socket.into(), connect_timeout, call_timeout).map(Self)
    }

    /// C6 `submit_turn`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn submit_turn(&self, request: &SubmitTurnRequest) -> Result<SubmitTurnResponse, Error> {
        let answer: SubmitTurnResponse = self.call("submit_turn", request)?;
        if answer.accepted != answer.turn.is_some() {
            return Err(Error::Malformed(
                "SubmitTurnResponse turn is not present exactly when accepted",
            ));
        }
        Ok(answer)
    }

    /// C6 `interrupt`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn interrupt(&self, request: &InterruptRequest) -> Result<InterruptResponse, Error> {
        self.call("interrupt", request)
    }

    /// C6 `status`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn status(&self, request: &StatusRequest) -> Result<SessionStatus, Error> {
        let status: SessionStatus = self.call("status", request)?;
        if !status.since.is_finite() || status.since < 0.0 {
            return Err(Error::Malformed("SessionStatus since is not a Unix time"));
        }
        Ok(status)
    }

    /// C6 `set_route`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn set_route(&self, request: &SetRouteRequest) -> Result<RouteSet, Error> {
        self.call("set_route", request)
    }

    /// C6 `turn_events`: the page the engine holds until the turn has an event after
    /// `request.after` or will have none, waited for at most the call timeout ([`Error::Timeout`],
    /// after which the caller calls again with the same `after`) or until `cancel` fires.
    ///
    /// # Errors
    /// [`Error`]; a page whose turn or event numbers do not follow the request, or whose
    /// events lack the payload of their type, is [`Error::Malformed`].
    pub fn turn_events(
        &self,
        request: &TurnEventsRequest,
        cancel: &Cancel,
    ) -> Result<EventsPage, Error> {
        let page: TurnEventsPage = self
            .call_with("turn_events", request, Some(cancel), Wait::CallTimeout)
            .map_err(|e| cancelled(cancel, e))?;
        if page.turn != request.turn {
            return Err(Error::Malformed(
                "TurnEventsPage turn differs from the request",
            ));
        }
        let mut events = Vec::with_capacity(page.events.len());
        for (event, seq) in page.events.into_iter().zip(request.after + 1..) {
            if event.seq != seq {
                return Err(Error::Malformed(
                    "TurnEventsPage events do not follow after",
                ));
            }
            events.push(Event::of(event)?);
        }
        Ok(EventsPage {
            state: page.state,
            events,
        })
    }

    /// C6 `inbox_turns`: the next inbox turn of the owner session, held without a read timeout
    /// until the engine names one, a newer request releases this one, the engine goes away, or
    /// `cancel` fires.
    ///
    /// # Errors
    /// [`Error`].
    pub fn inbox_turns(
        &self,
        request: &InboxTurnsRequest,
        cancel: &Cancel,
    ) -> Result<InboxTurns, Error> {
        self.call_with("inbox_turns", request, Some(cancel), Wait::Unbounded)
            .map_err(|e| cancelled(cancel, e))
    }

    /// C6 `report_delivery`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn report_delivery(&self, report: &DeliveryReport) -> Result<Ack, Error> {
        self.call("report_delivery", report)
    }

    /// Sends `input` to `POST /v1/<method>` and reads the `200` answer as `T`.
    fn call<T: DeserializeOwned>(&self, method: &str, input: &impl Serialize) -> Result<T, Error> {
        self.call_with(method, input, None, Wait::CallTimeout)
    }

    /// [`Self::call`] whose stream `cancel` can shut down while the call waits `wait` for its
    /// answer.
    fn call_with<T: DeserializeOwned>(
        &self,
        method: &str,
        input: &impl Serialize,
        cancel: Option<&Cancel>,
        wait: Wait,
    ) -> Result<T, Error> {
        let (status, body) = self.0.post(method, input, cancel, wait)?;
        if status == 200 {
            return serde_json::from_slice(&body)
                .map_err(|_| Error::Malformed("200 body is not valid for the method output"));
        }
        let error: WireError = serde_json::from_slice(&body)
            .map_err(|_| Error::Malformed("non-200 body is not a C6 Error"))?;
        Err(Error::Remote {
            code: error.code,
            message: error.message,
        })
    }
}

/// The error of a call `cancel` may have ended: [`Error::Cancelled`] once it fired.
fn cancelled(cancel: &Cancel, error: Error) -> Error {
    if cancel.is_cancelled() {
        Error::Cancelled
    } else {
        error
    }
}
