// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `ulei-inbox status` (ADR-0040 section 5): the inbox (C1), the engine (C6) and the qb gateway
//! (C4G) probed at once over their sockets, one thread each, and the status lines in their order.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;
use ulei_contracts::generated::c1::{Counts, Error as InboxError, Health as InboxHealth};
use ulei_contracts::generated::c4g::{
    Error as GatewayError, Health as GatewayHealth, RouteStatus, RoutesAnswer,
};
use ulei_contracts::generated::c6::{
    IdleStatus, SessionStatus, StatusRequest, TurnReport, TurnState,
};

use crate::c6::{self, Client, exchange};

/// Bound of each call from connect to the last answer byte.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The four keys `status` reads.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `inbox.socket`.
    pub inbox: PathBuf,
    /// `engine.c6_socket`.
    pub engine: PathBuf,
    /// `engine.owner_session`.
    pub owner_session: String,
    /// `qb.socket`.
    pub qb: PathBuf,
}

/// The status lines and whether `inbox`, `engine`, `channel` and `qb` all read `up`.
#[derive(Debug)]
pub struct Report {
    /// The lines, each ending in a newline.
    pub text: String,
    /// `inbox`, `engine`, `channel` and `qb` read `up`.
    pub up: bool,
}

/// Lines of one daemon and whether its lines that decide the exit status read `up`.
struct Part {
    text: String,
    up: bool,
}

/// Probes the three daemons of `settings` at once; the engine must run as `uid`.
///
/// # Errors
/// A probe thread that cannot be started.
pub fn probe(settings: &Settings, uid: u32) -> io::Result<Report> {
    thread::scope(|scope| {
        let thread = |name: &str| thread::Builder::new().name(name.to_owned());
        let inbox = thread("inbox").spawn_scoped(scope, || inbox(settings))?;
        let engine = thread("engine").spawn_scoped(scope, || engine(settings, uid))?;
        let qb = thread("qb").spawn_scoped(scope, || qb(settings))?;
        let mut report = Report {
            text: String::new(),
            up: true,
        };
        for handle in [inbox, engine, qb] {
            let part = handle
                .join()
                .map_err(|_| io::Error::other("a probe thread panicked"))?;
            report.text.push_str(&part.text);
            report.up &= part.up;
        }
        Ok(report)
    })
}

/// C1 `health`, then `counts`.
fn inbox(settings: &Settings) -> Part {
    let socket = settings.inbox.display();
    let call = |method: &str| call(&settings.inbox, &format!("POST /v1/{method}"), Some(b"{}"));
    let health = call("health").and_then(|answer| {
        decode::<InboxHealth, InboxError>(answer, |e| e.code.as_str()).and_then(|h| {
            if h.ok {
                Ok(h)
            } else {
                Err(MALFORMED.to_owned())
            }
        })
    });
    let health = match health {
        Ok(health) => health,
        Err(reason) => {
            return Part {
                text: format!("inbox: down {socket}: {reason}\nqueue: unknown: inbox down\n"),
                up: false,
            };
        }
    };
    let mut text = format!("inbox: up {socket}{}\n", version(health.version.as_deref()));
    match call("counts")
        .and_then(|answer| decode::<Counts, InboxError>(answer, |e| e.code.as_str()))
    {
        Ok(c) => {
            let _ = writeln!(
                text,
                "queue: pending {}, leased {}, processing {}, deferred {}, uncertain {}",
                c.pending, c.leased, c.processing, c.deferred, c.uncertain
            );
        }
        Err(reason) => {
            let _ = writeln!(text, "queue: unknown: {reason}");
        }
    }
    Part { text, up: true }
}

/// C6 `is_idle`, then `status` of the owner session.
fn engine(settings: &Settings, uid: u32) -> Part {
    let client = Client::with_limits(&settings.engine, c6::CONNECT_TIMEOUT, CALL_TIMEOUT, uid);
    let idle: IdleStatus = match client.is_idle() {
        Ok(idle) => idle,
        Err(e) => {
            return Part {
                text: format!(
                    "engine: down {}: {}\nchannel: unknown: engine down\n\
session: unknown: engine down\nlast turn: unknown: engine down\n",
                    settings.engine.display(),
                    c6_reason(&e)
                ),
                up: false,
            };
        }
    };
    let mut text = format!("engine: up {}\n", settings.engine.display());
    let channel = idle.channel == Some(true);
    text.push_str(match idle.channel {
        Some(true) => "channel: up\n",
        Some(false) => "channel: down: no channel holds inbox_turns\n",
        None => "channel: unknown: the runtime does not report it\n",
    });
    let request = StatusRequest {
        session: settings.owner_session.clone(),
        turn: None,
    };
    match client.status(&request) {
        Ok(status) => session_lines(&mut text, &status),
        Err(e) => {
            let reason = c6_reason(&e);
            let _ = write!(
                text,
                "session: unknown: {reason}\nlast turn: unknown: {reason}\n"
            );
        }
    }
    Part { text, up: channel }
}

/// The `session` and `last turn` lines of `status`.
fn session_lines(text: &mut String, status: &SessionStatus) {
    let _ = writeln!(
        text,
        "session: {} {} since {}, queue {}, route {}",
        status.session,
        if status.running { "running" } else { "idle" },
        utc(seconds(status.since)),
        status.queue_depth,
        status.route
    );
    let Some(report) = &status.last_turn else {
        text.push_str("last turn: none\n");
        return;
    };
    match last_turn(report) {
        Some(line) => {
            let _ = writeln!(text, "last turn: {} {line}", report.turn);
        }
        None => {
            let _ = writeln!(text, "last turn: unknown: {MALFORMED}");
        }
    }
}

/// What follows the turn id on the `last turn` line; `None` for a state a last turn cannot be in.
fn last_turn(report: &TurnReport) -> Option<String> {
    match (report.state, report.outcome, report.ended) {
        (TurnState::Ended, Some(outcome), Some(ended)) => {
            let mut line = format!("{} at {}", outcome.as_str(), utc(seconds(ended)));
            if let Some(said) = &report.line {
                let _ = write!(line, ": {said}");
            }
            Some(line)
        }
        (TurnState::Ended, None, None) => Some("ended, outcome not kept".to_owned()),
        (TurnState::Dropped, None, Some(ended)) => {
            Some(format!("dropped at {}", utc(seconds(ended))))
        }
        _ => None,
    }
}

/// C4G `GET /v1/health`, then `GET /v1/model/routes`.
fn qb(settings: &Settings) -> Part {
    let socket = settings.qb.display();
    let health = call(&settings.qb, "GET /v1/health", None)
        .and_then(|answer| decode::<GatewayHealth, GatewayError>(answer, |e| e.class.as_str()));
    let health = match health {
        Ok(health) => health,
        Err(reason) => {
            return Part {
                text: format!("qb: down {socket}: {reason}\nroutes: unknown: qb down\n"),
                up: false,
            };
        }
    };
    let mut text = format!("qb: up {socket}{}\n", version(Some(&health.version)));
    match call(&settings.qb, "GET /v1/model/routes", None)
        .and_then(|answer| decode::<RoutesAnswer, GatewayError>(answer, |e| e.class.as_str()))
    {
        Ok(answer) => answer
            .routes
            .iter()
            .for_each(|route| route_line(&mut text, route)),
        Err(reason) => {
            let _ = writeln!(text, "routes: unknown: {reason}");
        }
    }
    Part { text, up: true }
}

/// `route <alias>: <state>`, then ` <vendor>/<model>` and `, resets <time>` when given.
fn route_line(text: &mut String, route: &RouteStatus) {
    let _ = write!(text, "route {}: {}", route.alias, route.state.as_str());
    if let (Some(vendor), Some(model)) = (&route.vendor, &route.model) {
        let _ = write!(text, " {vendor}/{model}");
    }
    if let Some(resets) = route.resets_at {
        let _ = write!(text, ", resets {}", utc(resets));
    }
    text.push('\n');
}

const MALFORMED: &str = "malformed answer";

/// ` (<version>)` when the daemon names one.
fn version(version: Option<&str>) -> String {
    version.map_or_else(String::new, |v| format!(" ({v})"))
}

/// One call on a new connection to a peer of any uid; the failure is its reason.
fn call(socket: &Path, line: &str, body: Option<&[u8]>) -> Result<(u16, Value), String> {
    exchange(
        socket,
        line,
        body,
        (c6::CONNECT_TIMEOUT, CALL_TIMEOUT),
        None,
    )
    .map_err(|e| c6_reason(&e))
}

/// The `200` body as `T`, or the reason of a non-2xx answer: its status and the error code `code`
/// reads from the `E` it carries.
fn decode<T: DeserializeOwned, E: DeserializeOwned>(
    (status, body): (u16, Value),
    code: impl Fn(&E) -> &'static str,
) -> Result<T, String> {
    if status == 200 {
        return serde_json::from_value(body).map_err(|_| MALFORMED.to_owned());
    }
    match serde_json::from_value::<E>(body) {
        Ok(error) if !(200..300).contains(&status) => Err(format!("{status} {}", code(&error))),
        _ => Err(MALFORMED.to_owned()),
    }
}

/// The reason of a failed call.
fn c6_reason(e: &c6::Error) -> String {
    match e {
        c6::Error::Io {
            connected: false,
            error,
        } => format!("connect: {}", io_text(error)),
        c6::Error::Io {
            connected: true,
            error,
        } => format!("socket: {}", io_text(error)),
        c6::Error::Timeout => format!("no answer within {} s", CALL_TIMEOUT.as_secs()),
        c6::Error::PeerUid { expected, actual } => {
            format!("runs as uid {actual}, expected {expected}")
        }
        c6::Error::Remote { code, .. } => format!("{} {}", c6::status_of(*code), code.as_str()),
        c6::Error::Malformed(_) | c6::Error::Invalid(_) => MALFORMED.to_owned(),
        c6::Error::RequestTooLarge(_) => e.to_string(),
    }
}

/// The text of `error` without its ` (os error N)` suffix.
fn io_text(error: &io::Error) -> String {
    let text = error.to_string();
    match text.rsplit_once(" (os error ") {
        Some((head, _)) => head.to_owned(),
        None => text,
    }
}

/// Whole seconds of a non-negative Unix time.
fn seconds(time: f64) -> u64 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the client refuses a negative time; a time past u64 saturates"
    )]
    let whole = time.floor() as u64;
    whole
}

/// `YYYY-MM-DDTHH:MM:SSZ` of a Unix time, by the civil-from-days algorithm of the proleptic
/// Gregorian calendar, so no zone file is read.
fn utc(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}
