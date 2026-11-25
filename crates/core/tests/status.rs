//! `ulei-inbox status` and its `ulei status` forward against fake C1, C6 and C4G daemons on Unix
//! sockets that answer with the generated types, and a fake `ulei-hive-mcp` on loopback TCP.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::Scratch;
use common::http::{json_answer, read_request};
use ulei_contracts::generated::c1::{
    Counts, Error as InboxError, ErrorCode as InboxCode, Health as InboxHealth,
};
use ulei_contracts::generated::c4g::{
    Error as GatewayError, ErrorClass, Health as GatewayHealth, HealthStatus, RouteState,
    RouteStatus, RoutesAnswer,
};
use ulei_contracts::generated::c6::{
    Error as EngineError, ErrorCode as EngineCode, IdleStatus, SessionStatus, TurnOutcome,
    TurnReport, TurnState,
};

const INBOX: &str = env!("CARGO_BIN_EXE_ulei-inbox");
const ULEI: &str = env!("CARGO_BIN_EXE_ulei");
const SESSION: &str = "owner-session-1";
/// 2023-11-14T22:13:20Z.
const T0: u64 = 1_700_000_000;
/// 2000-02-29T00:00:00Z.
const LEAP_DAY: u64 = 951_782_400;
/// The `hive-mcp` line of a config without `[hive_mcp]` and `[engine.mcp.hive]`.
const HIVE_NONE: &str = "hive-mcp: not configured\n";

/// How a fake daemon meets a connection.
enum Fake {
    /// Answers each `METHOD /path` with its bytes, and 404 for any other.
    Answers(Vec<(&'static str, Vec<u8>)>),
    /// No socket file.
    Missing,
    /// A socket file nobody listens on.
    Refused,
    /// Listens and never accepts, so a connect lands in the backlog and no answer comes.
    Silent,
}

/// A fake daemon on its socket; `seen` lists the `METHOD /path` of each request it read.
struct Daemon {
    socket: PathBuf,
    seen: Arc<Mutex<Vec<String>>>,
    _held: Option<UnixListener>,
}

impl Daemon {
    fn start(socket: PathBuf, fake: Fake) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let held = match fake {
            Fake::Missing => None,
            Fake::Refused => {
                drop(UnixListener::bind(&socket).expect("bind"));
                None
            }
            Fake::Silent => Some(UnixListener::bind(&socket).expect("bind")),
            Fake::Answers(answers) => {
                let listener = UnixListener::bind(&socket).expect("bind");
                let seen = Arc::clone(&seen);
                thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(mut stream) = stream else { return };
                        let Some((target, _)) = read_request(&mut stream) else {
                            continue;
                        };
                        let answer = answers
                            .iter()
                            .find(|(t, _)| *t == target)
                            .map_or_else(not_found, |(_, bytes)| bytes.clone());
                        seen.lock().expect("seen").push(target);
                        let _ = stream.write_all(&answer);
                    }
                });
                None
            }
        };
        Self {
            socket,
            seen,
            _held: held,
        }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("seen").clone()
    }
}

fn not_found() -> Vec<u8> {
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
}

fn malformed() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot json!"
        .to_vec()
}

fn inbox_health() -> Vec<u8> {
    json_answer(
        200,
        &InboxHealth {
            ok: true,
            version: Some("ulei-inbox 9.9.9".to_owned()),
        },
    )
}

fn counts() -> Vec<u8> {
    json_answer(
        200,
        &Counts {
            pending: 3,
            leased: 1,
            processing: 1,
            deferred: 2,
            uncertain: 0,
        },
    )
}

fn inbox_error(status: u16, code: InboxCode) -> Vec<u8> {
    json_answer(
        status,
        &InboxError {
            code,
            message: "synthetic".to_owned(),
        },
    )
}

fn idle(channel: Option<bool>) -> Vec<u8> {
    json_answer(
        200,
        &IdleStatus {
            idle: true,
            since: Some(1.0),
            channel,
        },
    )
}

fn engine_error(status: u16, code: EngineCode) -> Vec<u8> {
    json_answer(
        status,
        &EngineError {
            code,
            message: "synthetic".to_owned(),
        },
    )
}

fn session(running: bool, queue_depth: u64, last_turn: Option<TurnReport>) -> Vec<u8> {
    json_answer(
        200,
        &SessionStatus {
            session: SESSION.to_owned(),
            running,
            #[allow(clippy::cast_precision_loss, reason = "a 2023 time is exact in f64")]
            since: T0 as f64 + 0.75,
            queue_depth,
            route: "main".to_owned(),
            last_turn,
            turn: None,
        },
    )
}

fn report(state: TurnState, outcome: Option<TurnOutcome>, ended: Option<f64>) -> TurnReport {
    TurnReport {
        turn: "turn-7".to_owned(),
        state,
        outcome,
        ended,
        line: None,
        delivery: None,
    }
}

#[allow(clippy::cast_precision_loss, reason = "a 2023 time is exact in f64")]
fn completed() -> TurnReport {
    report(
        TurnState::Ended,
        Some(TurnOutcome::Completed),
        Some((T0 + 60) as f64),
    )
}

fn qb_health() -> Vec<u8> {
    json_answer(
        200,
        &GatewayHealth {
            status: HealthStatus::Ok,
            api_version: "v1".to_owned(),
            version: "qb 1.2.3".to_owned(),
            uptime_s: 5,
        },
    )
}

fn route(alias: &str, state: RouteState, pinned: bool, resets_at: Option<u64>) -> RouteStatus {
    RouteStatus {
        alias: alias.to_owned(),
        state,
        vendor: pinned.then(|| "vendor-a".to_owned()),
        model: pinned.then(|| "model-a".to_owned()),
        resets_at,
    }
}

fn routes(routes: Vec<RouteStatus>) -> Vec<u8> {
    json_answer(200, &RoutesAnswer { routes })
}

fn gateway_error(status: u16, class: ErrorClass) -> Vec<u8> {
    json_answer(
        status,
        &GatewayError {
            class,
            retryable: false,
            retry_after: None,
            resets_at: None,
            tried: None,
            message: None,
        },
    )
}

fn inbox_up() -> Fake {
    Fake::Answers(vec![
        ("POST /v1/health", inbox_health()),
        ("POST /v1/counts", counts()),
    ])
}

fn engine_with(idle_answer: Vec<u8>, status: Vec<u8>) -> Fake {
    Fake::Answers(vec![
        ("POST /v1/is_idle", idle_answer),
        ("POST /v1/status", status),
    ])
}

fn engine_up() -> Fake {
    engine_with(idle(Some(true)), session(false, 0, Some(completed())))
}

fn qb_with(routes_answer: Vec<u8>) -> Fake {
    Fake::Answers(vec![
        ("GET /v1/health", qb_health()),
        ("GET /v1/model/routes", routes_answer),
    ])
}

fn qb_up() -> Fake {
    qb_with(routes(vec![route("main", RouteState::Ready, true, None)]))
}

/// Three fake daemons in one scratch directory and the config that names them.
struct Stand {
    scratch: Scratch,
    inbox: Daemon,
    engine: Daemon,
    qb: Daemon,
}

impl Stand {
    fn new(inbox: Fake, engine: Fake, qb: Fake) -> Self {
        let scratch = Scratch::new();
        let inbox = Daemon::start(scratch.0.join("inbox.sock"), inbox);
        let engine = Daemon::start(scratch.0.join("engine.sock"), engine);
        let qb = Daemon::start(scratch.0.join("qb.sock"), qb);
        let stand = Self {
            scratch,
            inbox,
            engine,
            qb,
        };
        stand.write_config(&[]);
        stand
    }

    fn config(&self) -> PathBuf {
        self.scratch.0.join("ulei.toml")
    }

    /// Writes the config with every key but those in `without`.
    fn write_config(&self, without: &[&str]) {
        self.write_config_with(without, "");
    }

    /// Writes the config with every key but those in `without`, then `extra`.
    fn write_config_with(&self, without: &[&str], extra: &str) {
        let line = |key: &str, value: &str| {
            let name = key.rsplit_once('.').expect("dotted key").1;
            if without.contains(&key) {
                String::new()
            } else {
                format!("{name} = \"{value}\"\n")
            }
        };
        let text = format!(
            "[inbox]\n{}[engine]\n{}{}[qb]\n{}{extra}",
            line("inbox.socket", &self.inbox.socket.display().to_string()),
            line(
                "engine.c6_socket",
                &self.engine.socket.display().to_string()
            ),
            line("engine.owner_session", SESSION),
            line("qb.socket", &self.qb.socket.display().to_string()),
        );
        fs::write(self.config(), text).expect("config");
    }

    fn run(&self) -> Output {
        run(INBOX, &self.config())
    }

    /// The lines of the inbox, the engine and qb when each is up as its `*_up` fake answers.
    fn up_lines(&self) -> [String; 3] {
        [
            format!(
                "inbox: up {} (ulei-inbox 9.9.9)\n\
                 queue: pending 3, leased 1, processing 1, deferred 2, uncertain 0\n",
                self.inbox.socket.display()
            ),
            format!(
                "engine: up {}\nchannel: up\n\
                 session: {SESSION} idle since 2023-11-14T22:13:20Z, queue 0, route main\n\
                 last turn: turn-7 completed at 2023-11-14T22:14:20Z\n",
                self.engine.socket.display()
            ),
            format!(
                "qb: up {} (qb 1.2.3)\nroute main: ready vendor-a/model-a\n",
                self.qb.socket.display()
            ),
        ]
    }
}

fn run(exe: &str, config: &Path) -> Output {
    Command::new(exe)
        .arg("status")
        .arg("--config")
        .arg(config)
        .output()
        .expect("run status")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("UTF-8 stdout")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn every_line_up_exits_0() {
    let stand = Stand::new(inbox_up(), engine_up(), qb_up());
    let out = stand.run();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), stand.up_lines().concat() + HIVE_NONE);
    assert_eq!(stand.inbox.seen(), ["POST /v1/health", "POST /v1/counts"]);
    assert_eq!(stand.engine.seen(), ["POST /v1/is_idle", "POST /v1/status"]);
    assert_eq!(stand.qb.seen(), ["GET /v1/health", "GET /v1/model/routes"]);
}

#[test]
fn ulei_status_execs_the_sibling_with_its_arguments() {
    let stand = Stand::new(inbox_up(), engine_up(), qb_up());
    let out = run(ULEI, &stand.config());
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), stand.up_lines().concat() + HIVE_NONE);
}

/// Each way a daemon is down, as its fake and the reason its line carries.
fn down_ways(first: &'static str, error: Vec<u8>, code: &str) -> Vec<(Fake, String, bool)> {
    vec![
        (
            Fake::Missing,
            "connect: No such file or directory".to_owned(),
            false,
        ),
        (
            Fake::Refused,
            "connect: Connection refused".to_owned(),
            false,
        ),
        (Fake::Answers(vec![(first, error)]), code.to_owned(), true),
        (
            Fake::Answers(vec![(first, malformed())]),
            "malformed answer".to_owned(),
            true,
        ),
    ]
}

#[test]
fn a_down_inbox_is_its_line_with_the_reason_and_the_others_are_still_reported() {
    let ways = down_ways(
        "POST /v1/health",
        inbox_error(500, InboxCode::Internal),
        "500 internal",
    );
    for (fake, reason, answered) in ways {
        let stand = Stand::new(fake, engine_up(), qb_up());
        let out = stand.run();
        let [_, engine, qb] = stand.up_lines();
        let inbox = format!(
            "inbox: down {}: {reason}\nqueue: unknown: inbox down\n",
            stand.inbox.socket.display()
        );
        assert_eq!(
            stdout(&out),
            format!("{inbox}{engine}{qb}{HIVE_NONE}"),
            "{reason}"
        );
        assert_eq!(out.status.code(), Some(1), "{reason}");
        let seen: &[&str] = if answered { &["POST /v1/health"] } else { &[] };
        assert_eq!(stand.inbox.seen(), seen, "{reason}");
    }
}

#[test]
fn a_down_engine_is_its_line_with_the_reason_and_the_others_are_still_reported() {
    let ways = down_ways(
        "POST /v1/is_idle",
        engine_error(500, EngineCode::Internal),
        "500 internal",
    );
    for (fake, reason, answered) in ways {
        let stand = Stand::new(inbox_up(), fake, qb_up());
        let out = stand.run();
        let [inbox, _, qb] = stand.up_lines();
        let engine = format!(
            "engine: down {}: {reason}\nchannel: unknown: engine down\n\
             session: unknown: engine down\nlast turn: unknown: engine down\n",
            stand.engine.socket.display()
        );
        assert_eq!(
            stdout(&out),
            format!("{inbox}{engine}{qb}{HIVE_NONE}"),
            "{reason}"
        );
        assert_eq!(out.status.code(), Some(1), "{reason}");
        let seen: &[&str] = if answered { &["POST /v1/is_idle"] } else { &[] };
        assert_eq!(stand.engine.seen(), seen, "{reason}");
    }
}

#[test]
fn a_down_qb_is_its_line_with_the_reason_and_the_others_are_still_reported() {
    let ways = down_ways(
        "GET /v1/health",
        gateway_error(503, ErrorClass::Unavailable),
        "503 unavailable",
    );
    for (fake, reason, answered) in ways {
        let stand = Stand::new(inbox_up(), engine_up(), fake);
        let out = stand.run();
        let [inbox, engine, _] = stand.up_lines();
        let qb = format!(
            "qb: down {}: {reason}\nroutes: unknown: qb down\n",
            stand.qb.socket.display()
        );
        assert_eq!(
            stdout(&out),
            format!("{inbox}{engine}{qb}{HIVE_NONE}"),
            "{reason}"
        );
        assert_eq!(out.status.code(), Some(1), "{reason}");
        let seen: &[&str] = if answered { &["GET /v1/health"] } else { &[] };
        assert_eq!(stand.qb.seen(), seen, "{reason}");
    }
}

#[test]
fn daemons_that_never_answer_are_down_after_the_5_s_bound_probed_at_once() {
    let stand = Stand::new(Fake::Silent, Fake::Silent, Fake::Silent);
    // Listens and never accepts: the connect lands in the backlog and no answer comes.
    let silent = TcpListener::bind("127.0.0.1:0").expect("bind");
    let hive = silent.local_addr().expect("addr").to_string();
    stand.write_config_with(&[], &format!("[hive_mcp]\nlisten = \"{hive}\"\n"));
    let start = Instant::now();
    let out = stand.run();
    let took = start.elapsed();
    let want = format!(
        "inbox: down {}: no answer within 5 s\nqueue: unknown: inbox down\n\
         engine: down {}: no answer within 5 s\nchannel: unknown: engine down\n\
         session: unknown: engine down\nlast turn: unknown: engine down\n\
         qb: down {}: no answer within 5 s\nroutes: unknown: qb down\n\
         hive-mcp: down {hive}: no answer within 5 s\n",
        stand.inbox.socket.display(),
        stand.engine.socket.display(),
        stand.qb.socket.display()
    );
    assert_eq!(stdout(&out), want);
    assert_eq!(out.status.code(), Some(1));
    assert!(took >= Duration::from_secs(5), "{took:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
}

#[test]
fn a_failed_second_call_is_an_unknown_line_and_keeps_exit_0() {
    let inbox = Fake::Answers(vec![
        ("POST /v1/health", inbox_health()),
        ("POST /v1/counts", inbox_error(500, InboxCode::Internal)),
    ]);
    let engine = engine_with(idle(Some(true)), malformed());
    let qb = qb_with(gateway_error(503, ErrorClass::Unavailable));
    let stand = Stand::new(inbox, engine, qb);
    let out = stand.run();
    let want = format!(
        "inbox: up {} (ulei-inbox 9.9.9)\nqueue: unknown: 500 internal\n\
         engine: up {}\nchannel: up\n\
         session: unknown: malformed answer\nlast turn: unknown: malformed answer\n\
         qb: up {} (qb 1.2.3)\nroutes: unknown: 503 unavailable\n{HIVE_NONE}",
        stand.inbox.socket.display(),
        stand.engine.socket.display(),
        stand.qb.socket.display()
    );
    assert_eq!(stdout(&out), want);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn channel_false_or_absent_exits_1() {
    for (channel, line) in [
        (Some(false), "channel: down: no channel holds inbox_turns\n"),
        (None, "channel: unknown: the runtime does not report it\n"),
    ] {
        let engine = engine_with(idle(channel), session(false, 0, Some(completed())));
        let stand = Stand::new(inbox_up(), engine, qb_up());
        let out = stand.run();
        let [inbox, engine, qb] = stand.up_lines();
        let engine = engine.replace("channel: up\n", line);
        assert_eq!(
            stdout(&out),
            format!("{inbox}{engine}{qb}{HIVE_NONE}"),
            "{line}"
        );
        assert_eq!(out.status.code(), Some(1), "{line}");
    }
}

#[test]
#[allow(clippy::cast_precision_loss, reason = "a 2023 time is exact in f64")]
fn session_and_last_turn_lines_follow_the_status_answer() {
    let mut failed = report(
        TurnState::Ended,
        Some(TurnOutcome::Failed),
        Some((T0 + 3_600) as f64),
    );
    failed.line = Some("synthetic reason".to_owned());
    let cases = [
        (
            session(true, 2, None),
            "session: owner-session-1 running since 2023-11-14T22:13:20Z, queue 2, route main\n\
             last turn: none\n",
        ),
        (
            session(false, 0, Some(report(TurnState::Ended, None, None))),
            "session: owner-session-1 idle since 2023-11-14T22:13:20Z, queue 0, route main\n\
             last turn: turn-7 ended, outcome not kept\n",
        ),
        (
            session(false, 1, Some(failed)),
            "session: owner-session-1 idle since 2023-11-14T22:13:20Z, queue 1, route main\n\
             last turn: turn-7 failed at 2023-11-14T23:13:20Z: synthetic reason\n",
        ),
        (
            session(
                false,
                0,
                Some(report(TurnState::Dropped, None, Some(LEAP_DAY as f64))),
            ),
            "session: owner-session-1 idle since 2023-11-14T22:13:20Z, queue 0, route main\n\
             last turn: turn-7 dropped at 2000-02-29T00:00:00Z\n",
        ),
    ];
    for (answer, lines) in cases {
        let stand = Stand::new(inbox_up(), engine_with(idle(Some(true)), answer), qb_up());
        let out = stand.run();
        let text = stdout(&out);
        assert!(text.contains(lines), "{text}");
        assert_eq!(out.status.code(), Some(0), "{text}");
    }
}

#[test]
fn one_route_line_per_alias_in_qb_order_with_vendor_model_and_resets_when_given() {
    let qb = qb_with(routes(vec![
        route("zeta", RouteState::Ready, true, None),
        route("alpha", RouteState::Degraded, true, None),
        route("ladder", RouteState::Exhausted, false, Some(LEAP_DAY + 61)),
        route("pinned", RouteState::Exhausted, true, Some(T0)),
        route("off", RouteState::Unavailable, false, None),
    ]));
    let stand = Stand::new(inbox_up(), engine_up(), qb);
    let out = stand.run();
    let want = format!(
        "qb: up {} (qb 1.2.3)\n\
         route zeta: ready vendor-a/model-a\n\
         route alpha: degraded vendor-a/model-a\n\
         route ladder: exhausted, resets 2000-02-29T00:01:01Z\n\
         route pinned: exhausted vendor-a/model-a, resets 2023-11-14T22:13:20Z\n\
         route off: unavailable\n",
        stand.qb.socket.display()
    );
    assert!(
        stdout(&out).ends_with(&format!("{want}{HIVE_NONE}")),
        "{}",
        stdout(&out)
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_config_without_a_key_exits_2_with_nothing_probed() {
    let stand = Stand::new(inbox_up(), engine_up(), qb_up());
    for key in [
        "inbox.socket",
        "engine.c6_socket",
        "engine.owner_session",
        "qb.socket",
    ] {
        stand.write_config(&[key]);
        let out = stand.run();
        assert_eq!(stderr(&out), format!("ulei-inbox: missing {key}\n"));
        assert!(out.stdout.is_empty(), "{key}");
        assert_eq!(out.status.code(), Some(2), "{key}");
    }
    let absent = stand.scratch.0.join("absent.toml");
    let out = run(INBOX, &absent);
    assert!(
        stderr(&out).starts_with(&format!("ulei-inbox: config {}: ", absent.display())),
        "{}",
        stderr(&out)
    );
    assert_eq!(out.status.code(), Some(2));
    for daemon in [&stand.inbox, &stand.engine, &stand.qb] {
        assert_eq!(daemon.seen(), Vec::<String>::new());
    }
}

#[test]
fn a_status_argument_other_than_config_is_a_usage_error() {
    let out = Command::new(INBOX)
        .args(["status", "--root", "x"])
        .output()
        .expect("run status");
    assert!(
        stderr(&out).starts_with("usage: ulei-inbox"),
        "{}",
        stderr(&out)
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn ulei_status_without_the_sibling_names_the_expected_path() {
    let scratch = Scratch::under(Path::new(env!("CARGO_TARGET_TMPDIR")));
    let bin = scratch.0.join("bin");
    fs::create_dir(&bin).expect("bin");
    // A copy is written through a descriptor a concurrent test's fork may inherit until its exec,
    // which makes running the copy fail with ETXTBSY; a link opens nothing for writing.
    fs::hard_link(ULEI, bin.join("ulei")).expect("link ulei");
    let out = run(
        bin.join("ulei").to_str().expect("UTF-8 path"),
        Path::new("c"),
    );
    let expected = fs::canonicalize(&bin).expect("bin").join("ulei-inbox");
    assert!(
        stderr(&out).contains(&format!("cannot run {}", expected.display())),
        "{}",
        stderr(&out)
    );
    assert_eq!(out.status.code(), Some(1));
}

/// A fake `ulei-hive-mcp` on a loopback TCP port that answers every request with `answer`; it
/// keeps the head of each request it read.
struct HiveFake {
    addr: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl HiveFake {
    fn start(answer: &'static [u8]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&seen);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut head = Vec::new();
                let mut piece = [0u8; 512];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut piece) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&piece[..n]),
                    }
                }
                kept.lock()
                    .expect("seen")
                    .push(String::from_utf8_lossy(&head).into_owned());
                let _ = stream.write_all(answer);
            }
        });
        Self { addr, seen }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("seen").clone()
    }
}

const UNAUTHORIZED: &[u8] =
    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// A free loopback port nobody listens on.
fn closed_port() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").to_string()
}

#[test]
fn hive_mcp_answering_401_without_a_token_is_up_and_keeps_exit_0() {
    let hive = HiveFake::start(UNAUTHORIZED);
    let stand = Stand::new(inbox_up(), engine_up(), qb_up());
    stand.write_config_with(&[], &format!("[hive_mcp]\nlisten = \"{}\"\n", hive.addr));
    let out = stand.run();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let want = format!("hive-mcp: up {}\n", hive.addr);
    assert_eq!(stdout(&out), stand.up_lines().concat() + &want);
    let seen = hive.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].starts_with(&format!("GET /mcp HTTP/1.1\r\nHost: {}\r\n", hive.addr)),
        "{seen:?}"
    );
    assert!(
        !seen[0].to_ascii_lowercase().contains("authorization"),
        "{seen:?}"
    );
}

#[test]
fn hive_mcp_down_is_its_line_with_the_reason_and_never_changes_the_exit_status() {
    let refused = closed_port();
    let other = HiveFake::start(
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let garbage = HiveFake::start(b"SSH-2.0-synthetic\r\n");
    for (addr, reason) in [
        (refused, "connect: Connection refused"),
        (other.addr.clone(), "answered 404, not 401"),
        (garbage.addr.clone(), "malformed answer"),
    ] {
        let stand = Stand::new(inbox_up(), engine_up(), qb_up());
        stand.write_config_with(&[], &format!("[hive_mcp]\nlisten = \"{addr}\"\n"));
        let out = stand.run();
        let want = format!("hive-mcp: down {addr}: {reason}\n");
        assert_eq!(stdout(&out), stand.up_lines().concat() + &want, "{reason}");
        assert_eq!(out.status.code(), Some(0), "{reason}");
    }
    let stand = Stand::new(Fake::Refused, engine_up(), qb_up());
    let addr = closed_port();
    stand.write_config_with(&[], &format!("[hive_mcp]\nlisten = \"{addr}\"\n"));
    let out = stand.run();
    assert!(
        stdout(&out).ends_with(&format!(
            "hive-mcp: down {addr}: connect: Connection refused\n"
        )),
        "{}",
        stdout(&out)
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn no_hive_mcp_in_the_config_is_a_neutral_line_and_keeps_exit_0() {
    let stand = Stand::new(inbox_up(), engine_up(), qb_up());
    let out = stand.run();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).ends_with(&format!("\n{HIVE_NONE}")),
        "{}",
        stdout(&out)
    );
    assert!(out.stderr.is_empty(), "{}", stderr(&out));
}

#[test]
fn without_hive_mcp_listen_the_engine_url_of_the_hive_server_is_probed() {
    let hive = HiveFake::start(UNAUTHORIZED);
    let port = hive.addr.rsplit_once(':').expect("port").1.to_owned();
    for (url, host, target) in [
        (
            format!("http://{}/mcp", hive.addr),
            hive.addr.clone(),
            "/mcp",
        ),
        (
            format!("http://localhost:{port}/hive?x=1"),
            format!("localhost:{port}"),
            "/hive",
        ),
    ] {
        let stand = Stand::new(inbox_up(), engine_up(), qb_up());
        stand.write_config_with(
            &[],
            &format!("[engine.mcp.hive]\nurl = \"{url}\"\nenv_file = \"/nonexistent/hive.env\"\n"),
        );
        let out = stand.run();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        let want = format!("hive-mcp: up {host} (engine.mcp.hive.url)\n");
        assert_eq!(stdout(&out), stand.up_lines().concat() + &want, "{url}");
        let last = hive.seen().pop().expect("a request");
        assert!(
            last.starts_with(&format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n")),
            "{last:?}"
        );
    }
    let stand = Stand::new(inbox_up(), engine_up(), qb_up());
    let addr = closed_port();
    stand.write_config_with(
        &[],
        &format!("[engine.mcp.hive]\nurl = \"http://{addr}/mcp\"\n"),
    );
    let out = stand.run();
    let want =
        format!("hive-mcp: down {addr} (engine.mcp.hive.url): connect: Connection refused\n");
    assert_eq!(stdout(&out), stand.up_lines().concat() + &want);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn an_unusable_hive_mcp_address_is_an_unknown_line_with_nothing_probed() {
    for (extra, reason) in [
        (
            "[hive_mcp]\nlisten = \"192.0.2.1:8740\"\n",
            "hive_mcp.listen is not a loopback address",
        ),
        (
            "[hive_mcp]\ntoken_file = \"/nonexistent/hive-mcp.token\"\n",
            "hive_mcp.listen is missing",
        ),
        (
            "[engine.mcp.hive]\nurl = \"https://example.test/mcp\"\n",
            "engine.mcp.hive.url is not an http loopback URL",
        ),
    ] {
        let stand = Stand::new(inbox_up(), engine_up(), qb_up());
        stand.write_config_with(&[], extra);
        let out = stand.run();
        let want = format!("hive-mcp: unknown: {reason}\n");
        assert_eq!(stdout(&out), stand.up_lines().concat() + &want, "{extra}");
        assert_eq!(out.status.code(), Some(0), "{extra}");
    }
}
