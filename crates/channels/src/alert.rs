//! The uncertain alerter (ADR-0097 section 5): one run reads the owner inbox's `uncertain`
//! events over C1 and tells the owner through [`notify::notify`] when they are new, every
//! 24 h while any stay, and when the inbox cannot be read; its state file holds what was told.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ulei_contracts::generated::c1::UncertainEvent;

use crate::bot_api::Sender;
use crate::c1;
use crate::ingress::Zone;
use crate::notify::{self, NotifyError, Receipt};

/// Most events one run asks the inbox for: C1's largest `limit`.
pub const LIMIT: u64 = 100;
/// Seconds after a notice before the same notice is sent again.
pub const REPEAT_SECS: u64 = 24 * 60 * 60;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// What the owner was told, as the state file holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    /// Ids of the uncertain events the last notice named that are still uncertain.
    reported: Vec<String>,
    /// Unix seconds of the last notice of uncertain events while any stay.
    notice_at: Option<u64>,
    /// Unix seconds of the last notice that the inbox could not be read.
    fault_at: Option<u64>,
}

/// What one run did.
#[derive(Debug)]
pub struct Run {
    /// The receipt of the notice sent, if one was due.
    pub receipt: Option<Receipt>,
    /// Why the inbox could not be read, if it could not.
    pub inbox_error: Option<String>,
}

/// Why a run ended without doing its duty. No variant carries the token or a notice's text.
#[derive(Debug)]
pub enum AlertError {
    /// The state file is unreadable or malformed, or its directory is missing; nothing was sent.
    State(String),
    /// The notice failed; the state is unchanged, so the next run sends it again.
    Send(NotifyError),
    /// The state could not be written; a notice may have gone out (`receipt`).
    Save {
        /// The notice sent before the write failed, if any.
        receipt: Option<Receipt>,
        /// The write's failure.
        error: String,
    },
}

impl AlertError {
    /// Whether the run stopped before any message went out.
    pub fn is_refusal(&self) -> bool {
        match self {
            Self::State(_) => true,
            Self::Send(e) => e.is_refusal(),
            Self::Save { .. } => false,
        }
    }
}

impl fmt::Display for AlertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::State(line) => f.write_str(line),
            Self::Send(e) => e.fmt(f),
            Self::Save { receipt, error } => {
                let sent = if receipt.is_some() {
                    "notice sent, "
                } else {
                    ""
                };
                write!(f, "{sent}state not written: {error}")
            }
        }
    }
}

impl std::error::Error for AlertError {}

/// One owner inbox, its owner's chats and the state file of what they were told.
pub struct Alerter<'a> {
    inbox: c1::Client,
    sender: &'a Sender,
    owner_ids: &'a [i64],
    state: PathBuf,
}

impl<'a> Alerter<'a> {
    /// An alerter of the inbox at `inbox_socket`, which must run as this process's uid, telling
    /// `owner_ids` through `sender` and keeping its state in the file `state`.
    ///
    /// # Errors
    /// The effective uid cannot be read.
    pub fn new(
        inbox_socket: PathBuf,
        sender: &'a Sender,
        owner_ids: &'a [i64],
        state: PathBuf,
    ) -> io::Result<Self> {
        Ok(Self {
            inbox: c1::Client::new(inbox_socket, CONNECT_TIMEOUT, CALL_TIMEOUT)?,
            sender,
            owner_ids,
            state,
        })
    }

    /// One run at Unix time `now`: reads the state, asks the inbox, sends the notice that is due
    /// and writes the new state, which a failed send leaves as it was.
    ///
    /// # Errors
    /// [`AlertError`].
    pub fn run(&self, now: u64) -> Result<Run, AlertError> {
        let state = load(&self.state).map_err(AlertError::State)?;
        let read = self.inbox.uncertain(LIMIT);
        let inbox_error = read.as_ref().err().map(ToString::to_string);
        let (text, next) = decide(&state, read, now);
        let receipt = match text {
            Some(text) => {
                Some(notify::notify(self.sender, self.owner_ids, &text).map_err(AlertError::Send)?)
            }
            None => None,
        };
        if next != state {
            save(&self.state, &next).map_err(|e| AlertError::Save {
                receipt: receipt.clone(),
                error: format!("{}: {e}", self.state.display()),
            })?;
        }
        Ok(Run {
            receipt,
            inbox_error,
        })
    }
}

/// The notice due at `now`, if any, and the state once it is sent.
fn decide(
    state: &State,
    read: Result<Vec<UncertainEvent>, c1::Error>,
    now: u64,
) -> (Option<String>, State) {
    let events = match read {
        Ok(events) => events,
        Err(e) if due(state.fault_at, now) => {
            let next = State {
                fault_at: Some(now),
                ..state.clone()
            };
            return (Some(fault_notice(&e)), next);
        }
        Err(_) => return (None, state.clone()),
    };
    let ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
    let quiet = State {
        reported: ids.clone(),
        notice_at: state.notice_at.filter(|_| !ids.is_empty()),
        fault_at: state.fault_at,
    };
    let reported: HashSet<&str> = state.reported.iter().map(String::as_str).collect();
    let new: HashSet<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| !reported.contains(id))
        .collect();
    if ids.is_empty() || (new.is_empty() && !due(state.notice_at, now)) {
        return (None, quiet);
    }
    let next = State {
        notice_at: Some(now),
        ..quiet
    };
    (Some(events_notice(&events, &new)), next)
}

/// Whether a notice last sent at `last` is due again at `now`; a clock that went back makes it
/// due rather than silent until the clock catches up.
fn due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|last| now < last || now - last >= REPEAT_SECS)
}

/// The count and one line per event, oldest first; never the payload, which C1 does not give.
fn events_notice(events: &[UncertainEvent], new: &HashSet<&str>) -> String {
    let count = if events.len() as u64 >= LIMIT {
        format!(
            "at least {} uncertain events, the oldest listed",
            events.len()
        )
    } else if events.len() == 1 {
        "1 uncertain event".to_owned()
    } else {
        format!("{} uncertain events", events.len())
    };
    let mut text = format!(
        "ulei inbox: {count}, {} new. They may not have reached the agent.\n",
        new.len()
    );
    for event in events {
        let mark = if new.contains(event.id.as_str()) {
            " | new"
        } else {
            ""
        };
        text += &[
            "\n",
            &event.id,
            " | ",
            event.kind.as_str(),
            " | from ",
            &event.source,
            " | ",
            event.reason.as_str(),
            " | ",
            &utc(event.time),
            mark,
        ]
        .concat();
    }
    text
}

fn fault_notice(error: &c1::Error) -> String {
    format!(
        "ulei inbox: cannot read the inbox ({error}); uncertain events, if any, are not known. \
         This notice repeats every 24 h while it lasts."
    )
}

/// `yyyy-mm-dd HH:MM UTC` of the Unix time `secs`.
fn utc(secs: f64) -> String {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "C1 times are whole Unix seconds far inside i64"
    )]
    let secs = secs.floor() as i64;
    format!("{} UTC", Zone::utc().local(secs).ymd_hm())
}

/// The state in `path`: empty when the file does not exist in its existing directory.
fn load(path: &Path) -> Result<State, String> {
    let at = |e: &dyn fmt::Display| format!("{}: {e}", path.display());
    let dir = parent(path);
    if !dir.is_dir() {
        return Err(format!("{}: no such directory", dir.display()));
    }
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| at(&e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(at(&e)),
    }
}

/// Writes `state` to `path` atomically: a temporary file of mode 0600 beside it, synced and
/// renamed over it, then the directory synced.
fn save(path: &Path, state: &State) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(state).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_owned();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    File::open(parent(path))?.sync_all()
}

fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}
