//! The C7 calls of the places bed's clients: the daemon's socket ([`Socket`]) and a rehearsal
//! of the writes on a scratch in-memory garden ([`Rehearsal`]), so a client can check a write by
//! the bed's own rules before it sends it.

use std::path::PathBuf;
use std::time::Duration;

use crate::api::{ApiConfig, ApiError, Garden, Hybrid, wire_refusal};
use crate::client::post;
use crate::miss::{KEY_LEN, ProducerKey};
use crate::model::UnixTime;
use crate::rerank::NoReranker;
use crate::store::Store;
use crate::wire::c7;

/// The connect, every read and every write of one C7 call.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes of a C7 answer at most.
const MAX_ANSWER: u64 = 4 * 1024 * 1024;

/// A C7 call the daemon or the rehearsal answered with an error.
#[derive(Debug)]
pub enum CallError {
    /// The request was refused: the rule, then the message.
    Refused(String, String),
    /// Anything else; the client stops.
    Failed(String),
}

/// The rule and message of a refused place or attribute of a `places_write` answer; only a
/// daemon that breaks C7 leaves out the rule or the message.
pub(crate) fn write_refusal(
    rule: Option<c7::PlaceRefusal>,
    message: Option<&str>,
) -> (&'static str, &str) {
    let rule = rule.map_or("refused", c7::PlaceRefusal::as_str);
    (rule, message.unwrap_or(rule))
}

/// The bed's C7 writes, served by the daemon or rehearsed in process.
pub trait BedWire {
    /// C7 `places_write`.
    ///
    /// # Errors
    ///
    /// [`CallError`].
    fn places_write(
        &mut self,
        req: &c7::PlacesWriteRequest,
    ) -> Result<c7::PlacesWriteResponse, CallError>;

    /// C7 `places_verdict`.
    ///
    /// # Errors
    ///
    /// [`CallError`].
    fn places_verdict(
        &mut self,
        req: &c7::PlacesVerdictRequest,
    ) -> Result<c7::PlacesVerdictResponse, CallError>;

    /// C7 `places_profile`.
    ///
    /// # Errors
    ///
    /// [`CallError`].
    fn places_profile(
        &mut self,
        req: &c7::PlacesProfileRequest,
    ) -> Result<c7::PlacesProfileResponse, CallError>;
}

/// The daemon's C7 at `garden.socket`, one connection per call.
pub struct Socket(pub PathBuf);

impl Socket {
    /// The body of the `200` answer of `method` to `body`.
    fn call(
        &self,
        method: &str,
        body: Result<Vec<u8>, serde_json::Error>,
    ) -> Result<Vec<u8>, CallError> {
        let body = body.map_err(|e| CallError::Failed(e.to_string()))?;
        let (status, answer) = post(&self.0, method, &body, TIMEOUT, MAX_ANSWER)
            .map_err(|e| CallError::Failed(format!("{}: {e}", self.0.display())))?;
        if status == 200 {
            return Ok(answer);
        }
        let error: c7::Error = serde_json::from_slice(&answer)
            .map_err(|_| CallError::Failed(format!("{method}: answered {status}")))?;
        Err(match error.code {
            c7::ErrorCode::InvalidRequest | c7::ErrorCode::NotFound => {
                CallError::Refused(error.code.as_str().to_owned(), error.message)
            }
            code => CallError::Failed(format!("{method}: {}: {}", code.as_str(), error.message)),
        })
    }

    /// C7 `places_search`: the answer's body as the daemon sent it, checked to be a
    /// `PlacesSearchResponse`.
    ///
    /// # Errors
    ///
    /// [`CallError`].
    pub fn places_search(&self, req: &c7::PlacesSearchRequest) -> Result<Vec<u8>, CallError> {
        let answer = self.call("places_search", serde_json::to_vec(req))?;
        serde_json::from_slice::<c7::PlacesSearchResponse>(&answer)
            .map_err(|_| malformed("places_search"))?;
        Ok(answer)
    }
}

fn malformed(method: &str) -> CallError {
    CallError::Failed(format!("{method}: malformed answer"))
}

impl BedWire for Socket {
    fn places_write(
        &mut self,
        req: &c7::PlacesWriteRequest,
    ) -> Result<c7::PlacesWriteResponse, CallError> {
        let answer = self.call("places_write", serde_json::to_vec(req))?;
        serde_json::from_slice(&answer).map_err(|_| malformed("places_write"))
    }

    fn places_verdict(
        &mut self,
        req: &c7::PlacesVerdictRequest,
    ) -> Result<c7::PlacesVerdictResponse, CallError> {
        let answer = self.call("places_verdict", serde_json::to_vec(req))?;
        serde_json::from_slice(&answer).map_err(|_| malformed("places_verdict"))
    }

    fn places_profile(
        &mut self,
        req: &c7::PlacesProfileRequest,
    ) -> Result<c7::PlacesProfileResponse, CallError> {
        let answer = self.call("places_profile", serde_json::to_vec(req))?;
        serde_json::from_slice(&answer).map_err(|_| malformed("places_profile"))
    }
}

/// The C7 writes on a fresh in-memory garden at `now`, so the bed's own rules check them.
pub struct Rehearsal {
    garden: Garden,
    now: UnixTime,
}

impl Rehearsal {
    /// A rehearsal on an empty garden at `now`.
    ///
    /// # Errors
    ///
    /// Why the scratch garden cannot be opened.
    pub fn new(now: UnixTime) -> Result<Self, String> {
        let failed = |e: &dyn std::fmt::Display| format!("rehearsal: {e}");
        let store = Store::open_in_memory().map_err(|e| failed(&e))?;
        // The rehearsal never searches, so no miss is ever keyed with this key.
        let key = ProducerKey::from_bytes(&[0; KEY_LEN]).map_err(|e| failed(&e))?;
        let retriever = Hybrid::new(None, Box::new(NoReranker));
        Ok(Self {
            garden: Garden::new(store, retriever, ApiConfig::default(), key),
            now,
        })
    }
}

fn rehearsed<T>(r: Result<T, ApiError>) -> Result<T, CallError> {
    r.map_err(|e| match e {
        ApiError::Refused(r) => {
            CallError::Refused(wire_refusal(&r).as_str().to_owned(), r.to_string())
        }
        ApiError::Invalid(m) => CallError::Refused("invalid_request".to_owned(), m),
        e => CallError::Failed(format!("rehearsal: {e}")),
    })
}

impl BedWire for Rehearsal {
    fn places_write(
        &mut self,
        req: &c7::PlacesWriteRequest,
    ) -> Result<c7::PlacesWriteResponse, CallError> {
        rehearsed(self.garden.garden_places_write(req, self.now))
    }

    fn places_verdict(
        &mut self,
        req: &c7::PlacesVerdictRequest,
    ) -> Result<c7::PlacesVerdictResponse, CallError> {
        rehearsed(self.garden.garden_places_verdict(req, self.now))
    }

    fn places_profile(
        &mut self,
        req: &c7::PlacesProfileRequest,
    ) -> Result<c7::PlacesProfileResponse, CallError> {
        rehearsed(self.garden.garden_places_profile(req))
    }
}
