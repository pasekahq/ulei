// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The live link to one MCP server (ADR-0026): the first connect with its retries, the
//! reconnect ladder with jittered backoff and the parked state that the calls of a turn drive,
//! the circuit breaker, and the one retry of a call whose session was lost.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher as _;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::Server;
use super::client::{self, Failure, Session};
use crate::tools::{Interrupt, Output};

const MAX_RECONNECT_RETRIES: u32 = 5;
const MAX_INITIAL_CONNECT_RETRIES: u32 = 3;
const MAX_BACKOFF_SECONDS: f64 = 60.0;
const BACKOFF_JITTER: f64 = 0.2;
const PARKED_RETRY_INTERVAL: Duration = Duration::from_secs(300);
const CIRCUIT_BREAKER_THRESHOLD: u32 = 3;
const CIRCUIT_BREAKER_COOLDOWN: Duration = Duration::from_secs(60);
/// How long a call whose session was lost waits for a new one.
const RECONNECT_WAIT: Duration = Duration::from_secs(15);
/// Longest wait of a call that finds no session.
const SESSION_WAIT: Duration = Duration::from_secs(5);

/// The time source of MCP recovery; tests inject one that records its waits.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
    /// Blocks for `duration` unless `interrupt` fires first; false when it did.
    fn wait(&self, duration: Duration, interrupt: &Interrupt) -> bool;
    /// A uniform sample of `[0, 1]` that spreads the backoff waits.
    fn jitter(&self) -> f64;
}

/// The [`Clock`] of the running engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wait(&self, duration: Duration, interrupt: &Interrupt) -> bool {
        interrupt.sleep(duration)
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "53 bits of the hash are plenty for a jitter fraction"
    )]
    fn jitter(&self) -> f64 {
        (RandomState::new().hash_one(0_u8) >> 11) as f64 / (1_u64 << 53) as f64
    }
}

/// `seconds` spread by up to [`BACKOFF_JITTER`] either way.
fn jittered(clock: &dyn Clock, seconds: f64) -> Duration {
    let factor = 1.0 - BACKOFF_JITTER + 2.0 * BACKOFF_JITTER * clock.jitter();
    Duration::from_secs_f64((seconds * factor).max(0.0))
}

/// Why a call has no session.
enum Gone {
    /// The next attempt lies past the wait of the call.
    Down,
    /// The ladder ran out; the next attempt is the parked probe.
    Parked,
    /// The interrupt fired during a wait or an attempt.
    Interrupted,
}

struct State {
    session: Option<Arc<Session>>,
    /// Every session id the server handed out.
    ids: Vec<String>,
    /// Consecutive failed reconnects.
    retries: u32,
    backoff: f64,
    /// The earliest start of the next attempt while there is no session.
    next: Instant,
    parked: bool,
    /// Whether the session served a call since it opened.
    proven: bool,
    /// Consecutive failed calls, which open the breaker at the threshold.
    errors: u32,
    opened: Option<Instant>,
}

/// The session of one server and its recovery.
pub(crate) struct Link {
    pub(crate) server: Server,
    headers: Vec<(String, String)>,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

impl Link {
    /// Connects `server` and lists its tools, retrying a failed connect up to
    /// [`MAX_INITIAL_CONNECT_RETRIES`] times with backoff within its connect timeout, until
    /// `interrupt` fires; the failure text has credentials and secret header values masked.
    pub(crate) fn start(
        server: &Server,
        clock: Arc<dyn Clock>,
        interrupt: &Interrupt,
    ) -> Result<(Self, Vec<Value>), String> {
        let headers = match &server.env_file {
            None => Vec::new(),
            Some(path) => crate::env_file::read(path)
                .map_err(|e| format!("cannot read the env-file {}: {e}", path.display()))
                .and_then(|entries| {
                    client::check_headers(entries)
                        .map_err(|e| format!("{e} in the env-file {}", path.display()))
                })?,
        };
        let now = clock.now();
        let link = Self {
            server: server.clone(),
            headers,
            clock,
            state: Mutex::new(State {
                session: None,
                ids: Vec::new(),
                retries: 0,
                backoff: 1.0,
                next: now,
                parked: false,
                proven: false,
                errors: 0,
                opened: None,
            }),
        };
        let listed = link.connect(now, interrupt).map_err(|e| link.mask(&e))?;
        Ok((link, listed))
    }

    /// The preflight, then attempts until one connects, the retries run out, the next wait
    /// would end past the connect timeout counted from `start` or `interrupt` fires.
    fn connect(&self, start: Instant, interrupt: &Interrupt) -> Result<Vec<Value>, String> {
        let left = || {
            self.server
                .connect_timeout
                .map(|t| (start + t).saturating_duration_since(self.clock.now()))
        };
        let deadline = || left().map(|l| Instant::now() + l);
        self.session().preflight(deadline())?;
        let (mut retries, mut backoff) = (0, 1.0);
        loop {
            let error = match self.open(deadline(), interrupt) {
                Ok((session, listed)) => {
                    let mut state = self.lock();
                    state.ids.extend(session.id().map(str::to_owned));
                    state.session = Some(Arc::new(session));
                    return Ok(listed);
                }
                Err(error) => error,
            };
            retries += 1;
            if retries > MAX_INITIAL_CONNECT_RETRIES || interrupt.is_interrupted() {
                return Err(error);
            }
            let wait = jittered(&*self.clock, backoff);
            if left().is_some_and(|l| l < wait) || !self.clock.wait(wait, interrupt) {
                return Err(error);
            }
            backoff = (backoff * 2.0).min(MAX_BACKOFF_SECONDS);
        }
    }

    /// An unopened session of the server.
    fn session(&self) -> Session {
        Session::new(
            &self.server.name,
            self.server.endpoint.clone(),
            self.headers.clone(),
            self.server.connect_timeout,
        )
    }

    /// The handshake within `deadline`, then the tools the server lists.
    fn open(
        &self,
        deadline: Option<Instant>,
        interrupt: &Interrupt,
    ) -> Result<(Session, Vec<Value>), String> {
        let session = self.session().open(deadline, interrupt)?;
        let listed = if session.advertises("tools") {
            session
                .list("tools/list", "tools", deadline, Some(interrupt))
                .map_err(|f| f.to_string())?
        } else {
            Vec::new()
        };
        if listed
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str).is_none())
        {
            return Err("tools/list result has a tool without a string name".to_owned());
        }
        Ok((session, listed))
    }

    /// Whether the current session advertises the request family `capability`.
    pub(crate) fn advertises(&self, capability: &str) -> bool {
        self.lock()
            .session
            .as_ref()
            .is_some_and(|session| session.advertises(capability))
    }

    /// The secret header values and every session id, which never leave a request.
    pub(crate) fn secrets(&self) -> Vec<String> {
        let mut secrets = client::secrets(&self.headers);
        secrets.extend(self.lock().ids.iter().cloned());
        secrets
    }

    /// `text` with credentials and [`Link::secrets`] masked.
    pub(crate) fn mask(&self, text: &str) -> String {
        use crate::tools::redact::{mask_credentials, mask_values};
        mask_values(&mask_credentials(text), &self.secrets()).into_owned()
    }

    /// Ends the current session on the server.
    pub(crate) fn close(&self) {
        let session = self.lock().session.take();
        if let Some(session) = session {
            session.close();
        }
    }

    /// Runs `call` on the session as the reference's handlers do: a listed tool (`tool`) passes
    /// the breaker and drives the reconnect ladder when there is no session; a call whose
    /// session was lost is retried once on a new one.
    pub(crate) fn call(
        &self,
        tool: bool,
        timeout: Option<Duration>,
        interrupt: &Interrupt,
        call: impl Fn(&Session, Option<Instant>) -> Result<Output, Failure>,
    ) -> Output {
        let name = &self.server.name;
        if tool && let Some(text) = self.tripped() {
            return Output::error(text);
        }
        let current = self.lock().session.clone();
        let session = match current {
            Some(session) => session,
            None if !tool => return not_connected(name),
            None => {
                let wait = timeout.map_or(SESSION_WAIT, |t| t.min(SESSION_WAIT));
                match self.drive(self.clock.now() + wait, interrupt) {
                    Ok(session) => session,
                    Err(Gone::Interrupted) => return interrupted(),
                    Err(gone) => {
                        self.bump();
                        return match gone {
                            Gone::Down => Output::error(format!(
                                "MCP server '{name}' transport is down; reconnect requested. Do \
                                 NOT retry this tool immediately \u{2014} give it a few seconds to \
                                 come back."
                            )),
                            _ => not_connected(name),
                        };
                    }
                }
            }
        };
        let failure = match timed(timeout, |deadline| call(&session, deadline)) {
            Ok(output) => {
                self.proven();
                if tool {
                    if output.is_error {
                        self.bump();
                    } else {
                        self.reset();
                    }
                }
                return output;
            }
            Err(Failure::Interrupted) => return interrupted(),
            Err(failure) => failure,
        };
        if failure.lost() {
            match self.renew(&session, interrupt) {
                Err(Gone::Interrupted) => return interrupted(),
                Err(_) => {}
                Ok(fresh) => {
                    if let Ok(output) = timed(timeout, |deadline| call(&fresh, deadline)) {
                        self.proven();
                        if !output.is_error {
                            self.reset();
                            return output;
                        }
                    }
                }
            }
        }
        if tool {
            self.bump();
        }
        Output::error(self.mask(&format!("MCP call failed: {}: {failure}", failure.kind())))
    }

    /// The breaker's answer while it is open; `None` once its cooldown has passed.
    fn tripped(&self) -> Option<String> {
        let state = self.lock();
        let opened = state
            .opened
            .filter(|_| state.errors >= CIRCUIT_BREAKER_THRESHOLD)?;
        let age = self.clock.now().saturating_duration_since(opened);
        let remaining = CIRCUIT_BREAKER_COOLDOWN
            .checked_sub(age)
            .filter(|r| !r.is_zero())?;
        Some(format!(
            "MCP server '{}' is unreachable after {} consecutive failures. Auto-retry available \
             in ~{}s. Do NOT retry this tool yet \u{2014} use alternative approaches or ask the \
             user to check the MCP server.",
            self.server.name,
            state.errors,
            remaining.as_secs().max(1)
        ))
    }

    fn bump(&self) {
        let mut state = self.lock();
        state.errors += 1;
        if state.errors >= CIRCUIT_BREAKER_THRESHOLD {
            state.opened = Some(self.clock.now());
        }
    }

    fn reset(&self) {
        let mut state = self.lock();
        state.errors = 0;
        state.opened = None;
    }

    /// Records that the session served a call, which clears the reconnect budget.
    fn proven(&self) {
        let mut state = self.lock();
        if !state.proven {
            state.proven = true;
            state.retries = 0;
        }
    }

    /// Drops the lost session `old` and rebuilds one at once, waiting at most
    /// [`RECONNECT_WAIT`]; a session another call rebuilt already is taken as it is.
    fn renew(&self, old: &Arc<Session>, interrupt: &Interrupt) -> Result<Arc<Session>, Gone> {
        let stale = {
            let mut state = self.lock();
            if let Some(current) = &state.session
                && !Arc::ptr_eq(current, old)
            {
                return Ok(Arc::clone(current));
            }
            if state.proven {
                state.retries = 0;
                state.backoff = 1.0;
            } else {
                state.retries += 1;
            }
            if state.retries > MAX_RECONNECT_RETRIES {
                self.park(&mut state);
            } else {
                state.next = self.clock.now();
            }
            state.session.take()
        };
        if let Some(stale) = stale {
            stale.close();
        }
        self.drive(self.clock.now() + RECONNECT_WAIT, interrupt)
    }

    /// Attempts reconnects on the ladder's schedule until one succeeds or the next one would
    /// start after `until`.
    fn drive(&self, until: Instant, interrupt: &Interrupt) -> Result<Arc<Session>, Gone> {
        loop {
            let next = {
                let state = self.lock();
                if let Some(session) = &state.session {
                    return Ok(Arc::clone(session));
                }
                if state.next > until {
                    return Err(if state.parked {
                        Gone::Parked
                    } else {
                        Gone::Down
                    });
                }
                state.next
            };
            let wait = next.saturating_duration_since(self.clock.now());
            if !wait.is_zero() && !self.clock.wait(wait, interrupt) {
                return Err(Gone::Interrupted);
            }
            let deadline = self.server.connect_timeout.map(|t| Instant::now() + t);
            let opened = self.open(deadline, interrupt);
            let mut state = self.lock();
            match opened {
                Ok((session, _)) => {
                    let session = Arc::new(session);
                    state.parked = false;
                    state.proven = false;
                    state.errors = 0;
                    state.opened = None;
                    state.ids.extend(session.id().map(str::to_owned));
                    state.session = Some(Arc::clone(&session));
                    return Ok(session);
                }
                Err(_) if interrupt.is_interrupted() => return Err(Gone::Interrupted),
                Err(_) => {
                    state.retries += 1;
                    if state.retries > MAX_RECONNECT_RETRIES {
                        self.park(&mut state);
                    } else {
                        state.next = self.clock.now() + jittered(&*self.clock, state.backoff);
                        state.backoff = (state.backoff * 2.0).min(MAX_BACKOFF_SECONDS);
                    }
                }
            }
        }
    }

    /// Parks the server until its probe, which gets one attempt.
    fn park(&self, state: &mut State) {
        state.parked = true;
        state.next = self.clock.now() + PARKED_RETRY_INTERVAL;
        state.retries = MAX_RECONNECT_RETRIES;
        state.backoff = 1.0;
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Runs `call` under the call deadline of `timeout`; a passed deadline is the reference's
/// timeout failure.
fn timed(
    timeout: Option<Duration>,
    call: impl FnOnce(Option<Instant>) -> Result<Output, Failure>,
) -> Result<Output, Failure> {
    let start = Instant::now();
    call(timeout.map(|t| start + t)).map_err(|failure| match failure {
        Failure::TimedOut => Failure::Transport {
            kind: "TimeoutError",
            text: format!(
                "MCP call timed out after {:.1}s (configured timeout: {:.1}s)",
                start.elapsed().as_secs_f64(),
                timeout.map_or(0.0, |t| t.as_secs_f64())
            ),
        },
        other => other,
    })
}

fn not_connected(server: &str) -> Output {
    Output::error(format!("MCP server '{server}' is not connected"))
}

fn interrupted() -> Output {
    Output::error("MCP call interrupted: user sent a new message")
}
