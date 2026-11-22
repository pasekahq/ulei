//! The runner's C10 client: one `put` per connection to the archive socket.

use std::path::Path;
use std::time::Duration;

use ulei_contracts::generated::c10::{Error as WireError, ErrorCode, PutResponse, PutStatus};

use crate::c6::{self, exchange};

/// C10 `x-transport.max_body_bytes`.
pub(super) const MAX_BODY: usize = 1_048_576;
/// Bound of the connect to the archive.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound of one `put` from connect to the last answer byte: above C10's 5 s per read and write.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

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
    .map_err(|e| reason(&e))?;
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

/// The text of a failed exchange.
fn reason(e: &c6::Error) -> String {
    match e {
        c6::Error::Io {
            connected: false,
            error,
        } => format!("connect: {error}"),
        c6::Error::Io {
            connected: true,
            error,
        } => format!("socket: {error}"),
        c6::Error::Timeout => format!("no answer within {} s", CALL_TIMEOUT.as_secs()),
        c6::Error::PeerUid { expected, actual } => {
            format!("the archive runs as uid {actual}, expected {expected}")
        }
        c6::Error::Malformed(what) | c6::Error::Invalid(what) => {
            format!("malformed answer: {what}")
        }
        c6::Error::RequestTooLarge(_) | c6::Error::Remote { .. } => e.to_string(),
    }
}
