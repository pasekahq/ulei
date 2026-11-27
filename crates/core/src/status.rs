// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `ulei-inbox status` (ADR-0040 section 5): the inbox (C1), the engine (C6) and the qb gateway
//! (C4G) probed at once over their sockets, one thread each, and the status lines in their order;
//! then the `hive-mcp` line of `ulei-hive-mcp` (ADR-0074), which never decides the exit status.

use std::fmt::Write as _;
use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::Value;
use ulei_contracts::config::{Config, Value as ConfigValue};
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

/// The four keys `status` requires, and where it finds `ulei-hive-mcp`.
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
    /// `hive_mcp.listen`, else `engine.mcp.hive.url`.
    pub hive_mcp: HiveMcp,
}

/// Where `status` finds `ulei-hive-mcp` (ADR-0074).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HiveMcp {
    /// The config has neither `hive_mcp.listen` nor `engine.mcp.hive.url`.
    NotConfigured,
    /// A loopback address to probe.
    Probe {
        /// The address connected to.
        addr: SocketAddr,
        /// The `Host` header and the address the line names.
        host: String,
        /// The request target.
        target: String,
        /// The key the address came from when it is not `hive_mcp.listen`.
        via: Option<&'static str>,
    },
    /// The key that names no loopback HTTP address, and why.
    Unusable(String),
}

const LISTEN: &str = "hive_mcp.listen";
const ENGINE_URL: &str = "engine.mcp.hive.url";
/// The endpoint of `ulei-hive-mcp` (ADR-0071 section 2).
const MCP_PATH: &str = "/mcp";

impl HiveMcp {
    /// `hive_mcp.listen` when the config has the key, else the engine's `engine.mcp.hive.url`,
    /// so a host whose `[hive_mcp]` lives in the server's own config file is still probed.
    #[must_use]
    pub fn of(config: &Config) -> Self {
        let text = |key: &str| {
            config.get(key).map(|entry| match &entry.value {
                ConfigValue::String(s) => Ok(s.trim().to_owned()),
                _ => Err(format!("{key} must be a string")),
            })
        };
        if let Some(listen) = text(LISTEN) {
            return listen
                .and_then(|s| probe_listen(&s))
                .unwrap_or_else(Self::Unusable);
        }
        if config.sections.iter().any(|s| s.name == "hive_mcp") {
            return Self::Unusable(format!("{LISTEN} is missing"));
        }
        match text(ENGINE_URL) {
            Some(url) => url
                .and_then(|s| probe_url(&s))
                .unwrap_or_else(Self::Unusable),
            None => Self::NotConfigured,
        }
    }
}

/// `hive_mcp.listen`: `127.0.0.1:<port>` or `[::1]:<port>`, as the server takes it.
fn probe_listen(text: &str) -> Result<HiveMcp, String> {
    match text.parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() && addr.port() != 0 => Ok(HiveMcp::Probe {
            addr,
            host: text.to_owned(),
            target: MCP_PATH.to_owned(),
            via: None,
        }),
        _ => Err(format!("{LISTEN} is not a loopback address")),
    }
}

/// `engine.mcp.hive.url`: `http://` with the host `127.0.0.1`, `[::1]` or `localhost` (taken as
/// 127.0.0.1, so no resolver file is read), an optional port and a path.
fn probe_url(url: &str) -> Result<HiveMcp, String> {
    let rule = || format!("{ENGINE_URL} is not an http loopback URL");
    let rest = url
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
        .map(|_| &url[7..])
        .ok_or_else(rule)?;
    let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (inner, after) = v6.split_once(']').ok_or_else(rule)?;
            (format!("[{inner}]"), after.strip_prefix(':'))
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (authority.to_owned(), None),
        },
    };
    let port = match port {
        None => 80,
        Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => digits
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(rule)?,
        Some(_) => return Err(rule()),
    };
    let ip = match host.to_ascii_lowercase().as_str() {
        "127.0.0.1" | "localhost" => IpAddr::V4(Ipv4Addr::LOCALHOST),
        "[::1]" => IpAddr::V6(Ipv6Addr::LOCALHOST),
        _ => return Err(rule()),
    };
    let path = tail.split(['?', '#']).next().unwrap_or_default();
    Ok(HiveMcp::Probe {
        addr: SocketAddr::new(ip, port),
        host: authority.to_owned(),
        target: if path.is_empty() {
            "/".to_owned()
        } else {
            path.to_owned()
        },
        via: Some(ENGINE_URL),
    })
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

/// Probes the three daemons and `ulei-hive-mcp` of `settings` at once; the engine must run as
/// `uid`.
///
/// # Errors
/// A probe thread that cannot be started.
pub fn probe(settings: &Settings, uid: u32) -> io::Result<Report> {
    thread::scope(|scope| {
        let thread = |name: &str| thread::Builder::new().name(name.to_owned());
        let inbox = thread("inbox").spawn_scoped(scope, || inbox(settings))?;
        let engine = thread("engine").spawn_scoped(scope, || engine(settings, uid))?;
        let qb = thread("qb").spawn_scoped(scope, || qb(settings))?;
        let hive = thread("hive-mcp").spawn_scoped(scope, || hive_mcp(&settings.hive_mcp))?;
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
        let line = hive
            .join()
            .map_err(|_| io::Error::other("a probe thread panicked"))?;
        report.text.push_str(&line);
        Ok(report)
    })
}

/// C1 `health`, then `counts`.
fn inbox(settings: &Settings) -> Part {
    let socket = settings.inbox.display();
    let call = |method: &str| {
        call(
            &settings.inbox,
            &format!("POST /v1/{method}"),
            Some(b"{}"),
            None,
        )
    };
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
            let _ = write!(
                text,
                "queue: pending {}, leased {}, processing {}, deferred {}, uncertain {}",
                c.pending, c.leased, c.processing, c.deferred, c.uncertain
            );
            if let Some(reconciled) = c.reconciled {
                let _ = write!(text, ", reconciled {reconciled}");
            }
            text.push('\n');
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
    let health = call(&settings.qb, "GET /v1/health", None, None)
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
    match call(&settings.qb, "GET /v1/model/routes", None, None)
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

/// The `hive-mcp` line: an unauthenticated `GET` of the endpoint answered `401` proves the
/// server listens there without reading its token.
fn hive_mcp(hive: &HiveMcp) -> String {
    let (addr, host, target, via) = match hive {
        HiveMcp::NotConfigured => return "hive-mcp: not configured\n".to_owned(),
        HiveMcp::Unusable(reason) => return format!("hive-mcp: unknown: {reason}\n"),
        HiveMcp::Probe {
            addr,
            host,
            target,
            via,
        } => (addr, host, target, via),
    };
    let named = via.map_or_else(|| host.clone(), |key| format!("{host} ({key})"));
    match unauthorized(addr, host, target) {
        Ok(()) => format!("hive-mcp: up {named}\n"),
        Err(reason) => format!("hive-mcp: down {named}: {reason}\n"),
    }
}

/// `Ok` when `GET target` without a token is answered `401`; else the reason.
fn unauthorized(addr: &SocketAddr, host: &str, target: &str) -> Result<(), String> {
    let deadline = Instant::now() + CALL_TIMEOUT;
    let late = || format!("no answer within {} s", CALL_TIMEOUT.as_secs());
    let mut stream = TcpStream::connect_timeout(addr, c6::CONNECT_TIMEOUT)
        .map_err(|e| format!("connect: {}", io_text(&e)))?;
    let request = format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
    };
    stream
        .set_write_timeout(Some(left().ok_or_else(late)?))
        .and_then(|()| stream.write_all(request.as_bytes()))
        .map_err(|e| socket_reason(&e, late))?;
    let mut head = Vec::with_capacity(64);
    let mut piece = [0u8; 64];
    while !head.windows(2).any(|w| w == b"\r\n") && head.len() < 256 {
        stream
            .set_read_timeout(Some(left().ok_or_else(late)?))
            .map_err(|e| socket_reason(&e, late))?;
        match stream.read(&mut piece) {
            Ok(0) => break,
            Ok(n) => head.extend_from_slice(&piece[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(socket_reason(&e, late)),
        }
    }
    let line = head.split(|b| *b == b'\r').next().unwrap_or_default();
    let mut words = line.split(|b| *b == b' ');
    let status = match (words.next(), words.next()) {
        (Some(version), Some(code)) if version.starts_with(b"HTTP/1.") && code.len() == 3 => {
            std::str::from_utf8(code)
                .ok()
                .and_then(|c| c.parse::<u16>().ok())
        }
        _ => None,
    };
    match status {
        Some(401) => Ok(()),
        Some(other) => Err(format!("answered {other}, not 401")),
        None => Err(MALFORMED.to_owned()),
    }
}

/// The reason of a failed write or read on a connected socket.
fn socket_reason(e: &io::Error, late: impl Fn() -> String) -> String {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => late(),
        _ => format!("socket: {}", io_text(e)),
    }
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

pub(crate) const MALFORMED: &str = "malformed answer";

/// ` (<version>)` when the daemon names one.
fn version(version: Option<&str>) -> String {
    version.map_or_else(String::new, |v| format!(" ({v})"))
}

/// One call on a new connection to a peer of `uid`, of any uid when `None`; the failure is its
/// reason.
pub(crate) fn call(
    socket: &Path,
    line: &str,
    body: Option<&[u8]>,
    uid: Option<u32>,
) -> Result<(u16, Value), String> {
    exchange(socket, line, body, (c6::CONNECT_TIMEOUT, CALL_TIMEOUT), uid)
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
pub(crate) fn seconds(time: f64) -> u64 {
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
pub(crate) fn utc(secs: u64) -> String {
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
