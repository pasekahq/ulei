//! The runner's C10 client: one `put` or `sync` per connection to the archive socket.

use std::path::Path;
use std::time::Duration;

use ulei_contracts::generated::c10::{
    Error as WireError, ErrorCode, PutResponse, PutStatus, SyncResponse,
};

use crate::c6::{self, exchange};

/// C10 `x-transport.max_body_bytes`.
pub(super) const MAX_BODY: usize = 1_048_576;
/// Bound of the connect to the archive.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound of one `put` from connect to the last answer byte: above C10's 5 s per read and write.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound of one `sync` from connect to the last answer byte: the archive pulls the dialogue
/// before it answers.
const SYNC_TIMEOUT: Duration = Duration::from_secs(120);

/// The archive's answer to a `put` the run goes on after.
pub(super) enum Answer {
    Stored,
    Duplicate,
    Conflict,
}

/// Sends the `put` body `body` to the archive at `socket`, which must run as `uid`.
///
/// # Errors
/// Every failure but `409 conflict`, or no answer; the text names it.
pub(super) fn put(socket: &Path, uid: u32, body: &[u8]) -> Result<Answer, String> {
    let (status, answer) = exchange(
        socket,
        "POST /v1/put",
        Some(body),
        (CONNECT_TIMEOUT, CALL_TIMEOUT),
        Some(uid),
    )
    .map_err(|e| reason(&e, CALL_TIMEOUT))?;
    match status {
        200 => {
            let answer: PutResponse = serde_json::from_value(answer)
                .map_err(|_| "the answer is not a C10 PutResponse".to_owned())?;
            if answer.id.len() != 64
                || !answer
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err("the answer's id is not a C10 ItemId".to_owned());
            }
            Ok(match answer.status {
                PutStatus::Stored => Answer::Stored,
                PutStatus::Duplicate => Answer::Duplicate,
            })
        }
        _ => match serde_json::from_value::<WireError>(answer) {
            Ok(error) if status == 409 && error.code == ErrorCode::Conflict => Ok(Answer::Conflict),
            Ok(error) => Err(format!("{status} {}", error.code.as_str())),
            Err(_) => Err(format!("{status} without a C10 Error")),
        },
    }
}

/// Calls `sync` on the archive at `socket`, which must run as `uid`.
///
/// # Errors
/// Any answer but `200` with a C10 `SyncResponse`, or none; the text names it.
pub(super) fn sync(socket: &Path, uid: u32) -> Result<SyncResponse, String> {
    let (status, answer) = exchange(
        socket,
        "POST /v1/sync",
        Some(b"{}"),
        (CONNECT_TIMEOUT, SYNC_TIMEOUT),
        Some(uid),
    )
    .map_err(|e| reason(&e, SYNC_TIMEOUT))?;
    if status != 200 {
        return Err(match serde_json::from_value::<WireError>(answer) {
            Ok(error) => format!("{status} {}", error.code.as_str()),
            Err(_) => format!("{status} without a C10 Error"),
        });
    }
    serde_json::from_value(answer).map_err(|_| "the answer is not a C10 SyncResponse".to_owned())
}

/// The text of a failed exchange bounded by `limit`.
fn reason(e: &c6::Error, limit: Duration) -> String {
    match e {
        c6::Error::Io {
            connected: false,
            error,
        } => format!("connect: {error}"),
        c6::Error::Io {
            connected: true,
            error,
        } => format!("socket: {error}"),
        c6::Error::Timeout => format!("no answer within {} s", limit.as_secs()),
        c6::Error::PeerUid { expected, actual } => {
            format!("the archive runs as uid {actual}, expected {expected}")
        }
        c6::Error::Malformed(what) | c6::Error::Invalid(what) => {
            format!("malformed answer: {what}")
        }
        c6::Error::RequestTooLarge(_) | c6::Error::Remote { .. } => e.to_string(),
    }
}
