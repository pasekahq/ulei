//! The call of the guest gate (ADR-0083): the owner's protocol updates of
//! [`crate::ingress::guest`] go to the gate's `/internal/telegram` on a worker thread of their
//! own, in order, so a slow gate holds up neither polling nor the owner's other messages. What
//! the gate does not take comes back to the loop thread and goes the usual way.
//!
//! The reference's `guest-gate-ingress` plugin, rule for rule: a POST of the payload with
//! `Authorization: Bearer <token>`, 145 s for the whole call, no proxy from the environment, no
//! redirect; the gate took the update when its JSON answer has `"handled": true`.

use std::fmt;
use std::io;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use ureq::Agent;

use crate::ingress::Update;
use crate::ingress::guest::{Forward, Protocol};

/// Bound of one gate call, connect to the last byte of the answer, as in the reference.
pub const GATE_TIMEOUT: Duration = Duration::from_secs(145);
/// The most bytes of the gate's answer read.
const MAX_ANSWER_BYTES: u64 = 64 * 1024;

/// Where the gate listens and its bearer token. `Debug` shows neither.
#[derive(Clone, PartialEq, Eq)]
pub struct GuestGateConfig {
    /// The gate's endpoint, such as `http://127.0.0.1:8792/internal/telegram`.
    pub url: String,
    /// The bearer token, read from `telegram.guest_gate_token_file` at start.
    pub token: String,
    /// Bound of one call.
    pub timeout: Duration,
}

impl fmt::Debug for GuestGateConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuestGateConfig")
            .field("url", &"<set>")
            .field("token", &"<hidden>")
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Why a gate call gave no verdict; never carries the URL, the token or a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// No connection or a broken one.
    Unreachable,
    /// The call took longer than its bound.
    Timeout,
    /// An answer status outside 2xx.
    Status(u16),
    /// The answer is not a JSON object.
    Answer,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable => f.write_str("unreachable"),
            Self::Timeout => f.write_str("timeout"),
            Self::Status(code) => write!(f, "status {code}"),
            Self::Answer => f.write_str("unreadable answer"),
        }
    }
}

/// The gate's verdict on one update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// `"handled": true`: the gate took it and answered the owner itself.
    Handled,
    /// Any other answer object: not the gate's.
    NotHandled,
    /// No verdict.
    Failed(Failure),
}

/// Where an update goes after the gate's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Nowhere: the gate took it, or it is a button or command of a gate that failed.
    Eaten,
    /// The usual way through the ingress (a button there is counted as skipped).
    Usual,
}

/// The reference's routing: `handled` eats; a failure eats a button or a command and lets a reply
/// through; anything else goes the usual way.
pub fn route(protocol: Protocol, verdict: Verdict) -> Route {
    match verdict {
        Verdict::Handled => Route::Eaten,
        Verdict::Failed(_) if protocol.eaten_on_failure() => Route::Eaten,
        Verdict::Failed(_) | Verdict::NotHandled => Route::Usual,
    }
}

/// The gate's HTTP client.
pub struct GuestGate {
    agent: Agent,
    config: GuestGateConfig,
}

impl GuestGate {
    /// A client for `config`; it opens no connection until the first call.
    pub fn new(config: GuestGateConfig) -> Self {
        let agent = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .timeout_global(Some(config.timeout))
            .build()
            .new_agent();
        Self { agent, config }
    }

    /// Posts `payload` and reads the verdict.
    pub fn ask(&self, payload: &Value) -> Verdict {
        let body = payload.to_string();
        let sent = self
            .agent
            .post(&self.config.url)
            .header("Authorization", &format!("Bearer {}", self.config.token))
            .content_type("application/json")
            .send(body.as_bytes());
        let mut response = match sent {
            Ok(response) => response,
            Err(e) => return Verdict::Failed(failure(&e)),
        };
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Verdict::Failed(Failure::Status(status));
        }
        let bytes = match response
            .body_mut()
            .with_config()
            .limit(MAX_ANSWER_BYTES)
            .read_to_vec()
        {
            Ok(bytes) => bytes,
            Err(e) => return Verdict::Failed(failure(&e)),
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(answer)) if answer.get("handled") == Some(&Value::Bool(true)) => {
                Verdict::Handled
            }
            Ok(Value::Object(_)) => Verdict::NotHandled,
            _ => Verdict::Failed(Failure::Answer),
        }
    }
}

fn failure(err: &ureq::Error) -> Failure {
    match err {
        ureq::Error::Timeout(_) => Failure::Timeout,
        ureq::Error::Io(e) if e.kind() == io::ErrorKind::TimedOut => Failure::Timeout,
        ureq::Error::BodyExceedsLimit(_) => Failure::Answer,
        _ => Failure::Unreachable,
    }
}

/// One update for the gate: what to send, and the parsed update for the usual way.
struct Job {
    forward: Forward,
    update: Box<Update>,
}

/// The worker thread of the gate calls. Dropping it lets the thread end after its queue; the
/// channel's stop does not wait for it.
pub struct GateWorker {
    jobs: Sender<Job>,
}

impl GateWorker {
    /// Starts the worker; `back` hands an update that goes the usual way to the loop thread and
    /// answers false once the loop is gone.
    ///
    /// # Errors
    /// The thread could not be started.
    pub fn start(
        gate: GuestGate,
        back: impl Fn(Box<Update>) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let (jobs, queue) = channel();
        thread::Builder::new()
            .name("guest-gate".into())
            .spawn(move || work(&gate, &queue, &back))?;
        Ok(Self { jobs })
    }

    /// Queues `update` for the gate.
    ///
    /// # Errors
    /// The worker is gone: the update comes back to go the usual way.
    pub fn send(&self, forward: Forward, update: Box<Update>) -> Result<(), Box<Update>> {
        self.jobs
            .send(Job { forward, update })
            .map_err(|e| e.0.update)
    }
}

fn work(gate: &GuestGate, queue: &Receiver<Job>, back: &dyn Fn(Box<Update>) -> bool) {
    while let Ok(job) = queue.recv() {
        let protocol = job.forward.protocol;
        let verdict = gate.ask(&job.forward.payload);
        let routed = route(protocol, verdict);
        let id = job.update.update_id;
        let said = match verdict {
            Verdict::Handled => "handled".to_owned(),
            Verdict::NotHandled => "not handled".to_owned(),
            Verdict::Failed(why) => format!("failed ({why})"),
        };
        let then = match routed {
            Route::Eaten => "dropped",
            Route::Usual => "usual way",
        };
        // Category and outcome only: never the text, the URL or the token.
        eprintln!(
            "ulei-tg: guest gate: update {id} {}: {said}, {then}",
            protocol.as_str()
        );
        if routed == Route::Usual && !back(job.update) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use serde_json::json;

    use super::*;

    /// A one-request HTTP server: answers `status` and `body` after `delay`; the request's head
    /// and body come back on the receiver.
    fn server(
        status: u16,
        body: &'static str,
        delay: Duration,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/internal/telegram",
            listener.local_addr().unwrap()
        );
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut sent = vec![0; length];
            reader.read_exact(&mut sent).unwrap();
            tx.send(format!("{head}\r\n{}", String::from_utf8(sent).unwrap()))
                .unwrap();
            thread::sleep(delay);
            let mut stream = reader.into_inner();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        (url, rx)
    }

    fn gate(url: String, timeout: Duration) -> GuestGate {
        GuestGate::new(GuestGateConfig {
            url,
            token: "synthetic-token".into(),
            timeout,
        })
    }

    #[test]
    fn the_answer_decides() {
        let payload = json!({"update_id": 7, "message": {"message_id": 1, "text": "шли"}});
        for (status, body, want) in [
            (200, r#"{"handled": true, "x": 1}"#, Verdict::Handled),
            (200, r#"{"handled": false}"#, Verdict::NotHandled),
            (200, r#"{"handled": "true"}"#, Verdict::NotHandled),
            (200, r"{}", Verdict::NotHandled),
            (200, r"[true]", Verdict::Failed(Failure::Answer)),
            (200, r"handled", Verdict::Failed(Failure::Answer)),
            (
                500,
                r#"{"handled": true}"#,
                Verdict::Failed(Failure::Status(500)),
            ),
            (
                401,
                r#"{"error": "token"}"#,
                Verdict::Failed(Failure::Status(401)),
            ),
            (
                302,
                r#"{"handled": true}"#,
                Verdict::Failed(Failure::Status(302)),
            ),
        ] {
            let (url, seen) = server(status, body, Duration::ZERO);
            assert_eq!(
                gate(url, GATE_TIMEOUT).ask(&payload),
                want,
                "{status} {body}"
            );
            let request = seen.recv().unwrap();
            assert!(
                request.starts_with("POST /internal/telegram HTTP/1.1\r\n"),
                "{request}"
            );
            assert!(
                request.contains("authorization: Bearer synthetic-token\r\n")
                    || request.contains("Authorization: Bearer synthetic-token\r\n"),
                "{request}"
            );
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("content-type: application/json")
            );
            let sent: Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert_eq!(sent, payload);
        }
    }

    #[test]
    fn a_slow_or_absent_gate_fails() {
        let (url, _seen) = server(200, r#"{"handled": true}"#, Duration::from_secs(3));
        assert_eq!(
            gate(url, Duration::from_millis(300)).ask(&json!({})),
            Verdict::Failed(Failure::Timeout)
        );
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/internal/telegram", closed.local_addr().unwrap());
        drop(closed);
        assert_eq!(
            gate(url, GATE_TIMEOUT).ask(&json!({})),
            Verdict::Failed(Failure::Unreachable)
        );
    }

    #[test]
    fn routes_follow_the_reference() {
        let failed = Verdict::Failed(Failure::Timeout);
        for protocol in [Protocol::Button, Protocol::Command, Protocol::Reply] {
            assert_eq!(route(protocol, Verdict::Handled), Route::Eaten);
            assert_eq!(route(protocol, Verdict::NotHandled), Route::Usual);
        }
        assert_eq!(route(Protocol::Button, failed), Route::Eaten);
        assert_eq!(route(Protocol::Command, failed), Route::Eaten);
        assert_eq!(route(Protocol::Reply, failed), Route::Usual);
    }

    #[test]
    fn the_config_shows_no_secret() {
        let config = GuestGateConfig {
            url: "http://127.0.0.1:1/x?k=secret-url".into(),
            token: "secret-token".into(),
            timeout: GATE_TIMEOUT,
        };
        let shown = format!("{config:?}");
        assert!(!shown.contains("secret"), "{shown}");
    }
}
