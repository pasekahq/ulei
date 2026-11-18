//! The MCP client against a fake Streamable HTTP server on loopback: settings, the handshake
//! and session, discovery and naming, the call path through the policy chain, answers, the
//! timeout, the interrupt, header secrecy, and the recovery of a lost session on a fake clock.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;
mod fake_mcp;

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use fake_gateway::{FakeGateway, Scratch, call, request, text_step, tool_step, turns};
use fake_mcp::{FakeMcp, Reply, SESSION_ID, Spec, session_id};
use serde_json::{Map, Value, json};
use ulei_engine::agent::{Agent, DEFAULT_CONCURRENT_TOOL_TIMEOUT, Template, text_message};
use ulei_engine::config::Config;
use ulei_engine::gateway::Client;
use ulei_engine::mcp::{self, Clock, Server, SystemClock};
use ulei_engine::policy::{Chain, Origin, Policy, Verdict};
use ulei_engine::tools::{Context, Interrupt, Output, Registry};
use ulei_engine::wire::c4g::{BlockType, Role, ToolCall};

const FRAME_END: &str = "\n</untrusted_tool_result>";

fn no_env(_: &str) -> Option<OsString> {
    None
}

/// The servers a config file of `text` names.
fn servers(dir: &Scratch, text: &str) -> Vec<Server> {
    let path = dir.path("ulei.toml");
    fs::write(&path, text).unwrap();
    Server::from_settings(&Config::load(&path).unwrap(), &no_env).unwrap()
}

/// The config of one server `name` at `url` with `extra` lines in its section.
fn one(name: &str, url: &str, extra: &str) -> String {
    format!("[engine]\nmcp_servers = \"{name}\"\n[engine.mcp.{name}]\nurl = \"{url}\"\n{extra}\n")
}

/// A clock that moves only when it waits or is advanced; it records each wait, takes its
/// jitter samples from a queue (0.5 when empty) and can fire the interrupt of its next wait.
struct FakeClock {
    start: Instant,
    now: Mutex<Duration>,
    waits: Mutex<Vec<Duration>>,
    jitters: Mutex<VecDeque<f64>>,
    trip: AtomicBool,
}

impl FakeClock {
    fn new(jitters: &[f64]) -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            now: Mutex::new(Duration::ZERO),
            waits: Mutex::new(Vec::new()),
            jitters: Mutex::new(jitters.iter().copied().collect()),
            trip: AtomicBool::new(false),
        })
    }

    fn advance(&self, seconds: u64) {
        *self.now.lock().unwrap() += Duration::from_secs(seconds);
    }

    /// The time passed since the clock started.
    fn elapsed(&self) -> Duration {
        *self.now.lock().unwrap()
    }

    /// Each wait so far, in seconds.
    fn waits(&self) -> Vec<f64> {
        self.waits
            .lock()
            .unwrap()
            .iter()
            .map(Duration::as_secs_f64)
            .collect()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.start + self.elapsed()
    }

    fn wait(&self, duration: Duration, interrupt: &Interrupt) -> bool {
        if self.trip.swap(false, Ordering::SeqCst) {
            interrupt.interrupt();
        }
        if interrupt.is_interrupted() {
            return false;
        }
        self.waits.lock().unwrap().push(duration);
        *self.now.lock().unwrap() += duration;
        true
    }

    fn jitter(&self) -> f64 {
        self.jitters.lock().unwrap().pop_front().unwrap_or(0.5)
    }
}

fn dynamic(clock: &Arc<FakeClock>) -> Arc<dyn Clock> {
    Arc::clone(clock) as Arc<dyn Clock>
}

/// A registry of the tools of `servers` connected on a fresh fake clock, and the warnings.
fn connect(servers: &[Server]) -> (Registry, mcp::Sessions, Vec<String>) {
    connect_on(servers, &dynamic(&FakeClock::new(&[])))
}

/// A registry of the tools of `servers` connected on `clock`, and the connect warnings.
fn connect_on(
    servers: &[Server],
    clock: &Arc<dyn Clock>,
) -> (Registry, mcp::Sessions, Vec<String>) {
    let mut registry = Registry::new();
    let (sessions, warnings) = mcp::connect(servers, &mut registry, clock);
    (registry, sessions, warnings)
}

/// The config of one server `fake` at `url` without utilities, with `extra` lines.
fn tools_only(url: &str, extra: &str) -> String {
    one(
        "fake",
        url,
        &format!("{extra}\n[engine.mcp.fake.tools]\nresources = false\nprompts = false"),
    )
}

/// The `{"error": ...}` body of the result `out` of tool `name`.
fn error_of(name: &str, out: &Output) -> String {
    assert!(out.is_error, "{out:?}");
    let body = if out.content.starts_with('<') {
        unframed(name, &out.content)
    } else {
        &out.content
    };
    let body: Value = serde_json::from_str(body).unwrap();
    body["error"].as_str().unwrap().to_owned()
}

/// How many `POST`s of `method` the fake got.
fn count(fake: &FakeMcp, method: &str) -> usize {
    fake.posts().iter().filter(|(m, _)| m == method).count()
}

/// Whether each of `got` is within 1 ms of `want`.
fn close_to(got: &[f64], want: &[f64]) -> bool {
    got.len() == want.len() && got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-3)
}

fn names(registry: &Registry) -> Vec<String> {
    registry.definitions().into_iter().map(|d| d.name).collect()
}

fn run(registry: &Registry, name: &str, input: Value) -> Output {
    let Value::Object(input) = input else {
        panic!("input must be an object")
    };
    registry.execute(
        name,
        &input,
        Context {
            cwd: Path::new("."),
            interrupt: &Interrupt::new(),
        },
    )
}

/// `content` without the untrusted-data frame of tool `name`.
fn unframed<'a>(name: &str, content: &'a str) -> &'a str {
    let head = format!("<untrusted_tool_result source=\"{name}\">\n");
    let rest = content
        .strip_prefix(&head)
        .and_then(|rest| rest.strip_suffix(FRAME_END))
        .unwrap_or_else(|| panic!("not framed: {content}"));
    rest.split_once("\n\n").map_or(rest, |(_, body)| body)
}

fn echo_tool() -> Value {
    json!({"name": "echo", "description": "Echoes.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}})
}

fn text_result(text: &str) -> Reply {
    Reply::Json(json!({"content": [{"type": "text", "text": text}]}))
}

#[test]
fn settings_default_to_the_reference_timeouts_and_take_overrides_from_file_and_environment() {
    let dir = Scratch::new();
    let got = servers(&dir, &one("alpha", "http://localhost:8123/mcp", ""));
    assert_eq!(got.len(), 1);
    let alpha = &got[0];
    assert_eq!(alpha.name, "alpha");
    assert_eq!(alpha.endpoint.url, "http://localhost:8123/mcp");
    assert_eq!(alpha.timeout, Some(Duration::from_secs(300)));
    assert_eq!(alpha.connect_timeout, Some(Duration::from_secs(60)));
    assert!(alpha.include.is_empty() && alpha.resources && alpha.prompts);
    assert_eq!(alpha.env_file, None);

    let text = one(
        "alpha",
        "http://[::1]:9/x",
        "env_file = \"/run/alpha.env\"\ntimeout = 12\nconnect_timeout = \"2.5\"\n\
         [engine.mcp.alpha.tools]\ninclude = \"a, b_*\"\nresources = false",
    );
    fs::write(dir.path("ulei.toml"), &text).unwrap();
    let config = Config::load(&dir.path("ulei.toml")).unwrap();
    let env =
        |name: &str| (name == "ULEI_ENGINE_MCP_ALPHA_TOOLS_PROMPTS").then(|| OsString::from("off"));
    let alpha = &Server::from_settings(&config, &env).unwrap()[0];
    assert_eq!(alpha.env_file.as_deref(), Some(Path::new("/run/alpha.env")));
    assert_eq!(alpha.timeout, Some(Duration::from_secs(12)));
    assert_eq!(alpha.connect_timeout, Some(Duration::from_millis(2500)));
    assert_eq!(alpha.include, ["a", "b_*"]);
    assert!(!alpha.resources && !alpha.prompts);
}

#[test]
fn a_url_off_loopback_or_without_http_is_a_settings_error_naming_the_server() {
    let dir = Scratch::new();
    for url in [
        "http://10.0.0.1/mcp",
        "https://127.0.0.1/mcp",
        "http://user@127.0.0.1/mcp",
        "http://127.0.0.2/mcp",
        "http://127.0.0.1:0/mcp",
    ] {
        fs::write(dir.path("ulei.toml"), one("remote", url, "")).unwrap();
        let config = Config::load(&dir.path("ulei.toml")).unwrap();
        let error = Server::from_settings(&config, &no_env)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "engine.mcp.remote.url (ULEI_ENGINE_MCP_REMOTE_URL) must be an http:// URL whose \
             host is 127.0.0.1, [::1] or localhost resolving only to loopback",
            "{url}"
        );
    }
    fs::write(dir.path("ulei.toml"), "[engine]\nmcp_servers = \"gone\"\n").unwrap();
    let config = Config::load(&dir.path("ulei.toml")).unwrap();
    assert_eq!(
        Server::from_settings(&config, &no_env)
            .unwrap_err()
            .to_string(),
        "missing engine.mcp.gone.url: set it in the config file or in ULEI_ENGINE_MCP_GONE_URL"
    );
}

#[test]
fn the_handshake_comes_first_and_the_session_id_goes_on_every_later_request() {
    let dir = Scratch::new();
    let fake = FakeMcp::start(Spec::new(vec![echo_tool()]).reply("echo", text_result("hi")));
    let servers = servers(&dir, &one("fake", &fake.url(), ""));
    let (registry, sessions, warnings) = connect(&servers);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        run(&registry, "mcp__fake__echo", json!({"text": "x"})).content,
        r#"{"result": "hi"}"#
    );
    sessions.close();

    let seen = fake.seen();
    let order: Vec<String> = seen
        .iter()
        .map(|s| format!("{} {}", s.target, s.method()))
        .collect();
    assert_eq!(
        order,
        [
            "HEAD /mcp ",
            "GET /mcp ",
            "POST /mcp initialize",
            "POST /mcp notifications/initialized",
            "POST /mcp tools/list",
            "POST /mcp tools/call",
            "DELETE /mcp ",
        ]
    );
    let initialize = &seen[2];
    assert_eq!(initialize.header("mcp-session-id"), None);
    assert_eq!(
        initialize.header("mcp-protocol-version"),
        Some("2025-11-25")
    );
    assert_eq!(
        initialize.header("accept"),
        Some("application/json, text/event-stream")
    );
    assert_eq!(
        initialize.body["params"],
        json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "mcp", "version": "0.1.0"}})
    );
    for later in &seen[3..] {
        assert_eq!(
            later.header("mcp-session-id"),
            Some(SESSION_ID),
            "{later:?}"
        );
        assert_eq!(
            later.header("mcp-protocol-version"),
            Some("2025-06-18"),
            "{later:?}"
        );
    }
    assert_eq!(
        seen[5].body["params"],
        json!({"name": "echo", "arguments": {"text": "x"}})
    );
}

#[test]
fn a_json_answer_and_a_chunked_event_stream_after_a_notification_both_answer_the_call() {
    let dir = Scratch::new();
    let slow = json!({"name": "slow", "inputSchema": {"type": "object"}});
    let fake = FakeMcp::start(
        Spec::new(vec![echo_tool(), slow])
            .reply("echo", text_result("from json"))
            .reply(
                "slow",
                Reply::Stream(
                    json!({"content": [{"type": "text", "text": "streamed after a notification"}]}),
                ),
            ),
    );
    let (registry, _sessions, _) = connect(&servers(&dir, &one("fake", &fake.url(), "")));

    let quick = run(&registry, "mcp__fake__echo", json!({}));
    assert_eq!(quick, Output::ok(r#"{"result": "from json"}"#));
    let streamed = run(&registry, "mcp__fake__slow", json!({}));
    assert!(!streamed.is_error);
    assert_eq!(
        unframed("mcp__fake__slow", &streamed.content),
        r#"{"result": "streamed after a notification"}"#
    );
}

#[test]
fn tools_register_as_sanitized_prefixed_names_across_pages_and_the_include_filter_keeps_its_matches()
 {
    let dir = Scratch::new();
    let tools = vec![
        json!({"name": "get-weather.v2", "inputSchema": {}}),
        json!({"name": "keep_me", "inputSchema": {}}),
        json!({"name": "drop_me", "inputSchema": {}}),
    ];
    let fake = FakeMcp::start(Spec::new(tools.clone()));
    let all = servers(&dir, &one("fake-srv.1", &fake.url(), ""));
    let (registry, _s, _) = connect(&all);
    assert_eq!(
        names(&registry),
        [
            "mcp__fake_srv_1__get_weather_v2",
            "mcp__fake_srv_1__keep_me",
            "mcp__fake_srv_1__drop_me",
            "mcp__fake_srv_1__list_resources",
            "mcp__fake_srv_1__read_resource",
            "mcp__fake_srv_1__list_prompts",
            "mcp__fake_srv_1__get_prompt",
        ]
    );
    let cursors: Vec<Value> = fake
        .seen()
        .iter()
        .filter(|s| s.method() == "tools/list")
        .map(|s| s.body["params"]["cursor"].clone())
        .collect();
    assert_eq!(cursors, [Value::Null, json!("page-2")]);

    let filtered = servers(
        &dir,
        &one(
            "fake-srv.1",
            &fake.url(),
            "[engine.mcp.fake-srv.1.tools]\ninclude = \"get-weather.v2, keep_*\"",
        ),
    );
    let (registry, _s, _) = connect(&filtered);
    let got = names(&registry);
    assert_eq!(
        &got[..2],
        [
            "mcp__fake_srv_1__get_weather_v2",
            "mcp__fake_srv_1__keep_me"
        ]
    );
    assert!(!got.iter().any(|n| n.ends_with("drop_me")), "{got:?}");
}

#[test]
fn resource_and_prompt_utilities_register_when_on_and_advertised_and_answer_as_the_reference() {
    let dir = Scratch::new();
    let fake = FakeMcp::start(Spec::new(Vec::new()));
    let (registry, _s, _) = connect(&servers(&dir, &one("fake", &fake.url(), "")));
    let definitions = registry.definitions();
    let described: Vec<(&str, &str)> = definitions
        .iter()
        .map(|d| (d.name.as_str(), d.description.as_str()))
        .collect();
    assert_eq!(
        described,
        [
            (
                "mcp__fake__list_resources",
                "List available resources from MCP server 'fake'"
            ),
            (
                "mcp__fake__read_resource",
                "Read a resource by URI from MCP server 'fake'"
            ),
            (
                "mcp__fake__list_prompts",
                "List available prompts from MCP server 'fake'"
            ),
            (
                "mcp__fake__get_prompt",
                "Get a prompt by name from MCP server 'fake'"
            ),
        ]
    );
    assert_eq!(
        Value::Object(definitions[3].input_schema.clone()),
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Name of the prompt to retrieve"},
                "arguments": {
                    "type": "object",
                    "description": "Optional arguments to pass to the prompt",
                    "properties": {},
                    "additionalProperties": true
                }
            },
            "required": ["name"]
        })
    );

    let listed = run(&registry, "mcp__fake__list_resources", json!({}));
    assert_eq!(
        unframed("mcp__fake__list_resources", &listed.content),
        r#"{"resources": [{"uri": "notes://today", "name": "today", "mimeType": "text/plain"}, {"uri": "notes://empty", "name": "empty"}]}"#
    );
    let read = run(
        &registry,
        "mcp__fake__read_resource",
        json!({"uri": "notes://today"}),
    );
    assert_eq!(
        unframed("mcp__fake__read_resource", &read.content),
        r#"{"result": "line one\n[MCP embedded resource received (3 bytes, application/octet-stream) but document cache unavailable in this process]"}"#
    );
    let missing = run(&registry, "mcp__fake__read_resource", json!({}));
    assert_eq!(
        unframed("mcp__fake__read_resource", &missing.content),
        r#"{"error": "Missing required parameter 'uri'"}"#
    );
    assert!(missing.is_error);
    let prompts = run(&registry, "mcp__fake__list_prompts", json!({}));
    assert_eq!(
        unframed("mcp__fake__list_prompts", &prompts.content),
        r#"{"prompts": [{"name": "greet", "description": "Greets", "arguments": [{"name": "who", "required": true}]}]}"#
    );
    let prompt = run(
        &registry,
        "mcp__fake__get_prompt",
        json!({"name": "greet", "arguments": {"who": "Ann"}}),
    );
    assert_eq!(
        unframed("mcp__fake__get_prompt", &prompt.content),
        r#"{"messages": [{"role": "user", "content": "Hello Ann"}]}"#
    );

    let off = one(
        "fake",
        &fake.url(),
        "[engine.mcp.fake.tools]\nresources = false\nprompts = false",
    );
    let (registry, _s, _) = connect(&servers(&dir, &off));
    assert_eq!(names(&registry), Vec::<String>::new());

    let tools_only = FakeMcp::start(Spec {
        capabilities: json!({"tools": {}}),
        ..Spec::new(vec![echo_tool()])
    });
    let (registry, _s, _) = connect(&servers(&dir, &one("fake", &tools_only.url(), "")));
    assert_eq!(names(&registry), ["mcp__fake__echo"]);
}

#[test]
fn a_result_keeps_its_text_and_structured_content_and_is_error_becomes_the_reference_tool_error() {
    let dir = Scratch::new();
    let tools = vec![
        json!({"name": "fails"}),
        json!({"name": "blank_failure"}),
        json!({"name": "structured"}),
    ];
    let fake = FakeMcp::start(
        Spec::new(tools)
            .reply("fails", Reply::Json(json!({"content": [{"type": "text", "text": "bad "}, {"type": "resource", "resource": {"uri": "x://y", "text": "input"}}], "isError": true})))
            .reply("blank_failure", Reply::Json(json!({"content": [], "isError": true})))
            .reply("structured", Reply::Json(json!({"content": [{"type": "text", "text": "42"}, {"type": "resource_link", "uri": "x://doc", "name": "doc"}], "structuredContent": {"answer": 42}, "_meta": {"modelcontextprotocol.io/progress": 1, "com.example.mcp/trace": "t"}}))),
    );
    let (registry, _s, _) = connect(&servers(&dir, &one("fake", &fake.url(), "")));

    assert_eq!(
        run(&registry, "mcp__fake__fails", json!({})),
        Output {
            content: r#"{"error": "bad input"}"#.into(),
            is_error: true
        }
    );
    let blank = run(&registry, "mcp__fake__blank_failure", json!({}));
    assert!(blank.is_error);
    assert_eq!(
        unframed("mcp__fake__blank_failure", &blank.content),
        r#"{"error": "MCP tool returned an error"}"#
    );
    let structured = run(&registry, "mcp__fake__structured", json!({}));
    assert!(!structured.is_error);
    assert_eq!(
        unframed("mcp__fake__structured", &structured.content),
        "{\"result\": \"42\\n[MCP resource link: uri=x://doc, name=doc \u{2014} fetch it with \
         mcp__fake__read_resource]\", \"structuredContent\": {\"answer\": 42}, \"_meta\": \
         {\"com.example.mcp/trace\": \"t\"}}"
    );
}

#[test]
fn a_hanging_call_ends_at_the_configured_tool_timeout_with_the_reference_text() {
    let dir = Scratch::new();
    let fake =
        FakeMcp::start(Spec::new(vec![json!({"name": "stuck"})]).reply("stuck", Reply::Hang));
    let (registry, _s, _) = connect(&servers(
        &dir,
        &one("fake", &fake.url(), "timeout = \"0.3\""),
    ));
    let start = Instant::now();
    let out = run(&registry, "mcp__fake__stuck", json!({}));
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    assert!(out.is_error);
    let body = unframed("mcp__fake__stuck", &out.content);
    assert!(
        body.starts_with(
            r#"{"error": "MCP call failed: TimeoutError: MCP call timed out after 0."#
        ) && body.ends_with(r#"s (configured timeout: 0.3s)"}"#),
        "{body}"
    );
}

#[test]
fn an_interrupt_ends_a_running_call_with_the_reference_text() {
    let dir = Scratch::new();
    let fake =
        FakeMcp::start(Spec::new(vec![json!({"name": "stuck"})]).reply("stuck", Reply::Hang));
    let (registry, _s, _) = connect(&servers(&dir, &one("fake", &fake.url(), "")));
    let interrupt = Interrupt::new();
    let out = thread::scope(|scope| {
        scope.spawn(|| {
            assert!(fake.wait_for("tools/call"));
            interrupt.interrupt();
        });
        registry.execute(
            "mcp__fake__stuck",
            &Map::new(),
            Context {
                cwd: Path::new("."),
                interrupt: &interrupt,
            },
        )
    });
    assert!(out.is_error);
    assert_eq!(
        unframed("mcp__fake__stuck", &out.content),
        r#"{"error": "MCP call interrupted: user sent a new message"}"#
    );
}

#[test]
fn credential_header_values_reach_the_server_and_never_a_result_or_a_warning() {
    let dir = Scratch::new();
    let token = "synthetic-token-5f1c";
    let trace = "synthetic-key-9a2e";
    fs::write(
        dir.path("mcp.env"),
        format!(
            "# synthetic\nAuthorization=\"Bearer {token}\"\nexport X-Api-Key={trace}\nX-Flag=1\n"
        ),
    )
    .unwrap();
    let leak = format!("denied: Authorization Bearer {token}, key {trace}, attempt 1 of 1");
    let fake = FakeMcp::start(Spec::new(vec![json!({"name": "leak"})]).reply(
        "leak",
        Reply::Json(json!({"content": [{"type": "text", "text": leak}], "isError": true})),
    ));
    let refusing = FakeMcp::start(Spec {
        reject_initialize: Some(format!("bad credentials {trace}")),
        ..Spec::new(Vec::new())
    });
    let env_line = format!("env_file = \"{}\"", dir.path("mcp.env").display());
    let config = format!(
        "[engine]\nmcp_servers = \"fake, refusing\"\n[engine.mcp.fake]\nurl = \"{}\"\n{env_line}\n\
         [engine.mcp.refusing]\nurl = \"{}\"\n{env_line}\n",
        fake.url(),
        refusing.url()
    );
    let (registry, _s, warnings) = connect(&servers(&dir, &config));

    let out = run(&registry, "mcp__fake__leak", json!({}));
    assert!(out.is_error);
    assert!(
        !out.content.contains(token) && !out.content.contains(trace),
        "{out:?}"
    );
    assert!(out.content.contains("[REDACTED]"), "{out:?}");
    assert!(out.content.contains("attempt 1 of 1"), "{out:?}");
    assert_eq!(
        warnings,
        ["Failed to connect to MCP server 'refusing': bad credentials [REDACTED]"]
    );
    for seen in fake.seen().iter().filter(|s| s.target.starts_with("POST")) {
        assert_eq!(
            seen.header("Authorization"),
            Some(format!("Bearer {token}").as_str())
        );
        assert_eq!(seen.header("X-Api-Key"), Some(trace));
        assert_eq!(seen.header("X-Flag"), Some("1"));
    }
}

#[test]
fn an_unreachable_server_or_a_web_page_is_a_warning_and_the_others_still_register() {
    let dir = Scratch::new();
    let page = FakeMcp::start(Spec {
        web_page: true,
        ..Spec::new(Vec::new())
    });
    let fake = FakeMcp::start(Spec::new(vec![echo_tool()]));
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!(
        "http://127.0.0.1:{}/mcp",
        closed.local_addr().unwrap().port()
    );
    drop(closed);
    let config = format!(
        "[engine]\nmcp_servers = \"page, dead, fake\"\n[engine.mcp.page]\nurl = \"{}\"\n\
         [engine.mcp.dead]\nurl = \"{dead_url}\"\n[engine.mcp.fake]\nurl = \"{}\"\n\
         [engine.mcp.fake.tools]\nresources = false\nprompts = false\n",
        page.url(),
        fake.url()
    );
    let (registry, _s, warnings) = connect(&servers(&dir, &config));
    assert_eq!(names(&registry), ["mcp__fake__echo"]);
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert_eq!(
        warnings[0],
        format!(
            "Failed to connect to MCP server 'page': MCP server 'page' at {} returned \
             Content-Type 'text/html', not an MCP response (expected one of: application/json, \
             text/event-stream). The URL most likely points at a web page rather than an MCP \
             endpoint \u{2014} check it resolves to a Streamable HTTP / SSE endpoint (e.g. \
             https://host/mcp, not https://host/).",
            page.url()
        )
    );
    assert!(
        warnings[1].starts_with("Failed to connect to MCP server 'dead': "),
        "{warnings:?}"
    );
}

/// Denies calls of one tool name.
struct DenyTool(&'static str);

impl Policy for DenyTool {
    fn name(&self) -> &'static str {
        "deny-tool"
    }

    fn pre_tool(&mut self, call: &ToolCall, _cwd: &Path) -> Verdict {
        if call.name == self.0 {
            Verdict::Deny("not this tool".into())
        } else {
            Verdict::Allow
        }
    }
}

/// One turn on `gateway` with the tools of `registry` and the policies of `chain`; the tool
/// results the model got back.
fn turn(gateway: &FakeGateway, registry: &Registry, chain: &mut Chain) -> Vec<(bool, String)> {
    let client = Client::new(
        gateway.socket(),
        Duration::from_secs(2),
        Duration::from_secs(2),
    );
    let template = Template {
        route: "chat".into(),
        system: None,
        max_tokens: 1024,
        effort: None,
        session_key: None,
    };
    let out = Agent {
        client: &client,
        tools: registry,
        chain,
        cwd: Path::new("."),
        template: &template,
        batch_timeout: Some(DEFAULT_CONCURRENT_TOOL_TIMEOUT),
        empty_guard: true,
        origin: Origin::Other,
        compaction: None,
    }
    .run(
        &[text_message(Role::User, "Go.")],
        &|step| format!("req-{step}"),
        &Interrupt::new(),
        8,
        &mut fake_gateway::Recorder::default(),
    );
    out.messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| b.r#type == BlockType::ToolResult)
        .map(|b| (b.is_error.unwrap(), b.content.clone().unwrap()))
        .collect()
}

#[test]
fn the_model_sees_the_reference_definitions_and_its_call_passes_the_policy_chain() {
    let dir = Scratch::new();
    let tool = json!({
        "name": "lookup",
        "description": "",
        "inputSchema": {
            "type": "object",
            "properties": {
                "when": {"anyOf": [{"$ref": "#/definitions/Day"}, {"type": "null"}], "default": null},
                "unit": {"anyOf": [{"const": "c"}, {"const": "f"}]},
                "definitions": {"type": "array"}
            },
            "required": ["unit", "missing"],
            "definitions": {"Day": {"type": "string"}}
        }
    });
    let fake = FakeMcp::start(Spec::new(vec![tool]).reply("lookup", text_result("sunny")));
    let config = one(
        "fake",
        &fake.url(),
        "[engine.mcp.fake.tools]\nresources = false\nprompts = false",
    );
    let (registry, _s, _) = connect(&servers(&dir, &config));
    let gateway = FakeGateway::start(vec![
        tool_step(vec![call("c1", "mcp__fake__lookup", json!({"unit": "c"}))]),
        text_step("Done."),
    ]);

    let results = turn(&gateway, &registry, &mut Chain::default());
    assert_eq!(results, [(false, r#"{"result": "sunny"}"#.to_owned())]);
    let sent = request(&turns(&gateway)[0]);
    let tools = sent.tools.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "mcp__fake__lookup");
    assert_eq!(tools[0].description, "MCP tool lookup from fake");
    assert_eq!(
        Value::Object(tools[0].input_schema.clone()),
        json!({
            "type": "object",
            "properties": {
                "when": {"$ref": "#/$defs/Day", "nullable": true},
                "unit": {"type": "string", "enum": ["c", "f"]},
                "definitions": {"type": "array"}
            },
            "required": ["unit"],
            "$defs": {"Day": {"type": "string"}}
        })
    );

    let calls_before = fake
        .seen()
        .iter()
        .filter(|s| s.method() == "tools/call")
        .count();
    let gateway = FakeGateway::start(vec![
        tool_step(vec![call("c1", "mcp__fake__lookup", json!({"unit": "c"}))]),
        text_step("Done."),
    ]);
    let mut chain = Chain::new(vec![Box::new(DenyTool("mcp__fake__lookup"))]);
    let results = turn(&gateway, &registry, &mut chain);
    assert_eq!(
        results,
        [(
            true,
            r#"{"error": "denied by policy deny-tool: not this tool"}"#.to_owned()
        )]
    );
    let calls_after = fake
        .seen()
        .iter()
        .filter(|s| s.method() == "tools/call")
        .count();
    assert_eq!(calls_before, calls_after);
}

#[test]
fn a_session_the_server_forgot_between_two_calls_is_rebuilt_and_the_call_retried_once() {
    let dir = Scratch::new();
    let fake = FakeMcp::start(Spec {
        forget_after: Some(1),
        ..Spec::new(vec![echo_tool()]).reply("echo", text_result("hi"))
    });
    let (registry, sessions, _) = connect(&servers(&dir, &tools_only(&fake.url(), "")));
    let gateway = FakeGateway::start(vec![
        tool_step(vec![call("c1", "mcp__fake__echo", json!({}))]),
        tool_step(vec![call("c2", "mcp__fake__echo", json!({}))]),
        text_step("Done."),
    ]);

    let results = turn(&gateway, &registry, &mut Chain::default());
    sessions.close();
    let hi = (false, r#"{"result": "hi"}"#.to_owned());
    assert_eq!(results, [hi.clone(), hi]);
    let old = Some(SESSION_ID.to_owned());
    let new = Some(session_id(1));
    let order: Vec<String> = fake
        .seen()
        .iter()
        .map(|s| format!("{} {}", s.target, s.method()))
        .collect();
    assert_eq!(
        order[5..],
        [
            "POST /mcp tools/call",
            "POST /mcp tools/call",
            "DELETE /mcp ",
            "POST /mcp initialize",
            "POST /mcp notifications/initialized",
            "POST /mcp tools/list",
            "POST /mcp tools/call",
            "DELETE /mcp ",
        ]
    );
    let posts = fake.posts();
    assert_eq!(
        posts[3..],
        [
            ("tools/call".to_owned(), old.clone()),
            ("tools/call".to_owned(), old),
            ("initialize".to_owned(), None),
            ("notifications/initialized".to_owned(), new.clone()),
            ("tools/list".to_owned(), new.clone()),
            ("tools/call".to_owned(), new),
        ]
    );
}

#[test]
fn the_first_connect_retries_three_times_with_jittered_backoff_within_its_timeout() {
    let dir = Scratch::new();
    let fake = FakeMcp::start(Spec::new(vec![echo_tool()]));
    fake.set_down(usize::MAX);
    let clock = FakeClock::new(&[0.0, 1.0, 0.5]);
    let (registry, _s, warnings) = connect_on(
        &servers(&dir, &tools_only(&fake.url(), "")),
        &dynamic(&clock),
    );
    assert_eq!(
        warnings,
        ["Failed to connect to MCP server 'fake': Server disconnected without sending a response."]
    );
    assert_eq!(names(&registry), Vec::<String>::new());
    assert_eq!(count(&fake, "initialize"), 4);
    assert!(
        close_to(&clock.waits(), &[0.8, 2.4, 4.0]),
        "{:?}",
        clock.waits()
    );

    fake.set_down(2);
    let clock = FakeClock::new(&[]);
    let (registry, _s, warnings) = connect_on(
        &servers(&dir, &tools_only(&fake.url(), "")),
        &dynamic(&clock),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(names(&registry), ["mcp__fake__echo"]);
    assert_eq!(count(&fake, "initialize"), 7);
    assert!(close_to(&clock.waits(), &[1.0, 2.0]), "{:?}", clock.waits());

    fake.set_down(usize::MAX);
    let clock = FakeClock::new(&[]);
    let short = tools_only(&fake.url(), "connect_timeout = 2");
    let (_, _s, warnings) = connect_on(&servers(&dir, &short), &dynamic(&clock));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(count(&fake, "initialize"), 9);
    assert!(close_to(&clock.waits(), &[1.0]), "{:?}", clock.waits());
}

#[test]
fn a_lost_server_backs_off_parks_after_five_reconnects_and_a_probe_after_300_s_revives_it() {
    let dir = Scratch::new();
    let fake = FakeMcp::start(Spec::new(vec![echo_tool()]).reply("echo", text_result("hi")));
    let clock = FakeClock::new(&[]);
    let (registry, _s, _) = connect_on(
        &servers(&dir, &tools_only(&fake.url(), "")),
        &dynamic(&clock),
    );
    let echo = |registry: &Registry| run(registry, "mcp__fake__echo", json!({}));
    let hi = Output::ok(r#"{"result": "hi"}"#);
    assert_eq!(echo(&registry), hi);
    fake.set_down(usize::MAX);

    let lost = echo(&registry);
    assert_eq!(
        error_of("mcp__fake__echo", &lost),
        "MCP call failed: RemoteProtocolError: Server disconnected without sending a response."
    );
    assert_eq!(count(&fake, "initialize"), 1 + 5);
    assert!(
        close_to(&clock.waits(), &[1.0, 2.0, 4.0, 8.0]),
        "{:?}",
        clock.waits()
    );

    assert_eq!(
        error_of("mcp__fake__echo", &echo(&registry)),
        "MCP server 'fake' transport is down; reconnect requested. Do NOT retry this tool \
         immediately \u{2014} give it a few seconds to come back."
    );
    assert_eq!(count(&fake, "initialize"), 6);

    clock.advance(15);
    assert_eq!(
        error_of("mcp__fake__echo", &echo(&registry)),
        "MCP server 'fake' is not connected"
    );
    assert_eq!(count(&fake, "initialize"), 1 + 6);
    assert_eq!(clock.elapsed(), Duration::from_secs(31));
    let requests = fake.seen().len();
    assert_eq!(
        error_of("mcp__fake__echo", &echo(&registry)),
        "MCP server 'fake' is unreachable after 3 consecutive failures. Auto-retry available in \
         ~60s. Do NOT retry this tool yet \u{2014} use alternative approaches or ask the user to \
         check the MCP server."
    );
    assert_eq!(fake.seen().len(), requests);

    clock.advance(200);
    assert_eq!(
        error_of("mcp__fake__echo", &echo(&registry)),
        "MCP server 'fake' is not connected"
    );
    assert_eq!(count(&fake, "initialize"), 7);

    fake.set_down(0);
    clock.advance(99);
    clock.trip.store(true, Ordering::SeqCst);
    assert_eq!(
        error_of("mcp__fake__echo", &echo(&registry)),
        "MCP call interrupted: user sent a new message"
    );
    assert_eq!(count(&fake, "initialize"), 7);
    assert_eq!(echo(&registry), hi);
    assert_eq!(count(&fake, "initialize"), 8);
    assert_eq!(clock.elapsed(), Duration::from_secs(331));
    assert!(
        close_to(&clock.waits(), &[1.0, 2.0, 4.0, 8.0, 1.0, 1.0]),
        "{:?}",
        clock.waits()
    );
}

#[test]
fn an_interrupt_during_a_reconnect_backoff_ends_the_call_at_once() {
    let dir = Scratch::new();
    let fake = FakeMcp::start(Spec::new(vec![echo_tool()]).reply("echo", text_result("hi")));
    let config = servers(&dir, &tools_only(&fake.url(), ""));
    let (registry, _s, _) = connect_on(&config, &(Arc::new(SystemClock) as Arc<dyn Clock>));
    assert!(!run(&registry, "mcp__fake__echo", json!({})).is_error);
    fake.set_down(usize::MAX);
    let interrupt = Interrupt::new();
    let start = Instant::now();
    let out = thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(300));
            interrupt.interrupt();
        });
        registry.execute(
            "mcp__fake__echo",
            &Map::new(),
            Context {
                cwd: Path::new("."),
                interrupt: &interrupt,
            },
        )
    });
    assert!(
        start.elapsed() < Duration::from_millis(750),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(
        error_of("mcp__fake__echo", &out),
        "MCP call interrupted: user sent a new message"
    );
    assert_eq!(count(&fake, "initialize"), 2);
}

#[test]
fn three_failed_calls_open_the_breaker_for_60_s_and_a_success_after_the_cooldown_closes_it() {
    let dir = Scratch::new();
    let fails = json!({"name": "fails"});
    let fake = FakeMcp::start(
        Spec::new(vec![echo_tool(), fails])
            .reply("echo", text_result("hi"))
            .reply(
                "fails",
                Reply::Json(json!({"content": [{"type": "text", "text": "no"}], "isError": true})),
            ),
    );
    let clock = FakeClock::new(&[]);
    let (registry, _s, _) = connect_on(
        &servers(&dir, &tools_only(&fake.url(), "")),
        &dynamic(&clock),
    );
    let no = Output::error("no");
    for _ in 0..3 {
        assert_eq!(run(&registry, "mcp__fake__fails", json!({})), no);
    }
    let open = |remaining: u64| {
        format!(
            "MCP server 'fake' is unreachable after 3 consecutive failures. Auto-retry available \
             in ~{remaining}s. Do NOT retry this tool yet \u{2014} use alternative approaches or \
             ask the user to check the MCP server."
        )
    };
    let echo = |registry: &Registry| run(registry, "mcp__fake__echo", json!({}));
    assert_eq!(error_of("mcp__fake__echo", &echo(&registry)), open(60));
    clock.advance(30);
    assert_eq!(error_of("mcp__fake__echo", &echo(&registry)), open(30));
    assert_eq!(count(&fake, "tools/call"), 3);

    clock.advance(30);
    assert_eq!(echo(&registry), Output::ok(r#"{"result": "hi"}"#));
    assert_eq!(run(&registry, "mcp__fake__fails", json!({})), no);
    assert_eq!(echo(&registry), Output::ok(r#"{"result": "hi"}"#));
    assert_eq!(count(&fake, "tools/call"), 6);
}

#[test]
fn an_answer_under_another_request_id_is_skipped() {
    let dir = Scratch::new();
    let fake = FakeMcp::start(Spec::new(vec![json!({"name": "stray"})]).reply(
        "stray",
        Reply::Stray(json!({"content": [{"type": "text", "text": "x"}]})),
    ));
    let (registry, _s, _) = connect(&servers(&dir, &tools_only(&fake.url(), "")));
    assert_eq!(
        error_of(
            "mcp__fake__stray",
            &run(&registry, "mcp__fake__stray", json!({}))
        ),
        "MCP call failed: MCPError: Failed to parse JSON response: not a JSON-RPC response"
    );
}

#[test]
fn a_call_whose_request_write_fails_leaves_no_socket_with_the_interrupt() {
    let dir = Scratch::new();
    let fake = FakeMcp::start(Spec {
        deaf_from: Some(5),
        ..Spec::new(vec![echo_tool()])
    });
    let (registry, _s, _) = connect(&servers(
        &dir,
        &tools_only(&fake.url(), "timeout = \"2.5\""),
    ));
    let Value::Object(input) = json!({"text": "x".repeat(16 * 1024 * 1024)}) else {
        unreachable!()
    };
    let interrupt = Interrupt::new();
    let out = registry.execute(
        "mcp__fake__echo",
        &input,
        Context {
            cwd: Path::new("."),
            interrupt: &interrupt,
        },
    );
    assert!(
        error_of("mcp__fake__echo", &out).starts_with("MCP call failed: TimeoutError: "),
        "{out:?}"
    );
    assert_eq!(fake.listen(), Some(true));
    drop(interrupt);
}

#[test]
fn an_audio_block_is_known_by_its_media_type_in_any_case_and_with_parameters() {
    let dir = Scratch::new();
    let audio = json!({"content": [
        {"type": "audio", "data": "AAAA", "mimeType": " Audio/OGG; codecs=opus", "uri": "x://a"},
        {"type": "text", "text": "after"}
    ]});
    let fake = FakeMcp::start(
        Spec::new(vec![json!({"name": "speak"})]).reply("speak", Reply::Json(audio)),
    );
    let (registry, _s, _) = connect(&servers(&dir, &tools_only(&fake.url(), "")));
    assert_eq!(
        run(&registry, "mcp__fake__speak", json!({})),
        Output::ok(r#"{"result": "after"}"#)
    );
}

#[test]
fn two_server_names_that_read_the_same_environment_variable_are_a_settings_error() {
    let dir = Scratch::new();
    for (first, second, env) in [
        ("a-b", "a_b", "ULEI_ENGINE_MCP_A_B_URL"),
        ("a", "a_connect", "ULEI_ENGINE_MCP_A_CONNECT_TIMEOUT"),
    ] {
        let text = format!(
            "[engine]\nmcp_servers = \"{first}, {second}\"\n[engine.mcp.{first}]\nurl = \
             \"http://127.0.0.1:9/mcp\"\n[engine.mcp.{second}]\nurl = \"http://127.0.0.1:9/mcp\"\n"
        );
        fs::write(dir.path("ulei.toml"), text).unwrap();
        let config = Config::load(&dir.path("ulei.toml")).unwrap();
        assert_eq!(
            Server::from_settings(&config, &no_env)
                .unwrap_err()
                .to_string(),
            format!(
                "engine.mcp_servers (ULEI_ENGINE_MCP_SERVERS) names '{first}' and '{second}', \
                 which both read {env}"
            )
        );
    }
}
