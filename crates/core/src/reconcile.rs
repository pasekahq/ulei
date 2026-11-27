//! `ulei-inbox reconcile` (ADR-0099): the owner closes an `uncertain` event through C1
//! `reconcile` on the daemon's socket; this side checks the arguments and prints the answer.

use std::path::Path;

use ulei_contracts::generated::c1::{
    Error as InboxError, ReconcileAudit, ReconcileOutcome, ReconcileRequest,
};

use crate::inbox::{MAX_NOTE_CHARS, canonical, valid_id, valid_note};
use crate::status::{MALFORMED, call, seconds, utc};

/// The C1 request of the arguments, or the one line saying which argument is wrong.
///
/// # Errors
/// The id is not a C1 `EventId`, the outcome is not a `ReconcileOutcome` or the note is not a
/// `ReconcileNote`.
pub fn request(id: &str, outcome: &str, note: &str) -> Result<ReconcileRequest, String> {
    if !valid_id(id) {
        return Err("the event id is not a C1 event id ([a-zA-Z0-9:_-], 1 to 96)".to_owned());
    }
    let outcome = [ReconcileOutcome::ReconciledDelivered]
        .into_iter()
        .find(|o| o.as_str() == outcome)
        .ok_or_else(|| "--outcome must be reconciled-delivered".to_owned())?;
    if !valid_note(note) {
        return Err(format!(
            "--note must be 1 to {MAX_NOTE_CHARS} characters, not blank, without control characters"
        ));
    }
    Ok(ReconcileRequest {
        id: id.to_owned(),
        outcome,
        note: note.to_owned(),
    })
}

/// Sends `request` to the inbox at `socket`, which must run as `uid`; the stored audit line, or
/// the reason: the failure of the call, or the status, C1 error code and message of a refusal.
///
/// # Errors
/// As above.
pub fn send(socket: &Path, request: &ReconcileRequest, uid: u32) -> Result<ReconcileAudit, String> {
    let body = serde_json::to_value(request)
        .ok()
        .and_then(|v| canonical(&v).ok())
        .ok_or_else(|| "request does not serialize".to_owned())?;
    let (status, answer) = call(socket, "POST /v1/reconcile", Some(&body), Some(uid))?;
    if status == 200 {
        return serde_json::from_value(answer).map_err(|_| MALFORMED.to_owned());
    }
    match serde_json::from_value::<InboxError>(answer) {
        Ok(e) => Err(format!("{status} {}: {}", e.code.as_str(), e.message)),
        Err(_) => Err(MALFORMED.to_owned()),
    }
}

/// The confirmation line of a stored audit line, without its newline.
#[must_use]
pub fn confirmation(audit: &ReconcileAudit) -> String {
    format!(
        "reconciled {}: done, {} (was {}), by uid {} pid {} at {}",
        audit.id,
        audit.outcome.as_str(),
        audit.reason.as_str(),
        audit.uid,
        audit.pid,
        utc(seconds(audit.time))
    )
}
