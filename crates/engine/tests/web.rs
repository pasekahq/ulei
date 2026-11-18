//! `web_search` and `web_extract` through the `ulei-web` helper against a local fake Exa on
//! loopback plain http, which the helper takes only under its test switch.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fake_gateway::{
    FakeGateway, Recorder, Request, Scratch, call, read_request, text_step, tool_step,
};
use serde_json::{Map, Value, json};
use ulei_engine::agent::{Agent, DEFAULT_CONCURRENT_TOOL_TIMEOUT, Template};
use ulei_engine::gateway::Client;
use ulei_engine::journal::Journal;
use ulei_engine::policy::Chain;
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::web::{Op, Request as HelperRequest, WEB_EXTRACT, WEB_SEARCH, Web};
use ulei_engine::tools::{Context, Interrupt, Output, Registry};
use ulei_engine::wire::c4g::{Message, Role};

const HELPER: &str = env!("CARGO_BIN_EXE_ulei-web");
const KEY: &str = "synthetic-exa-key-for-tests";
const PRIVATE_TARGET: &str = "Blocked: URL targets a private or internal network address";
const MISSING_KEY: &str =
    "EXA_API_KEY environment variable not set. Get your API key at https://exa.ai";

/// How the fake answers one connection.
enum Answer {
    Json(u16, Value),
    Raw(Vec<u8>),
    /// Reads the request and never answers.
    Hang,
    /// A `200` announcing and sending one byte more than the helper's reply cap.
    Oversized,
}

/// A scripted Exa API on a loopback port; every request is recorded.
struct FakeExa {
    port: u16,
    requests: Arc<Mutex<Vec<Request>>>,
    stopping: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl FakeExa {
    fn start(script: Vec<Answer>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let accept = {
            let (requests, stopping) = (Arc::clone(&requests), Arc::clone(&stopping));
            thread::spawn(move || {
                let mut script = script.into_iter();
                let mut workers = Vec::new();
                for stream in listener.incoming() {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    let (Ok(stream), Some(answer)) = (stream, script.next()) else {
                        continue;
                    };
                    let (requests, stopping) = (Arc::clone(&requests), Arc::clone(&stopping));
                    workers.push(thread::spawn(move || {
                        serve(stream, answer, &requests, &stopping);
                    }));
                }
                for worker in workers {
                    let _ = worker.join();
                }
            })
        };
        Self {
            port,
            requests,
            stopping,
            accept: Some(accept),
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for FakeExa {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        drop(TcpStream::connect(("127.0.0.1", self.port)));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn serve(mut stream: TcpStream, answer: Answer, requests: &Mutex<Vec<Request>>, stop: &AtomicBool) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    requests.lock().unwrap().push(request);
    let head = |status: u16, length: usize| {
        format!(
            "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        )
    };
    match answer {
        Answer::Json(status, body) => {
            let body = body.to_string();
            let _ = stream.write_all(format!("{}{body}", head(status, body.len())).as_bytes());
        }
        Answer::Raw(bytes) => {
            let _ = stream.write_all(&bytes);
        }
        Answer::Hang => {
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(20));
            }
        }
        Answer::Oversized => {
            let length = 16 * 1024 * 1024 + 1;
            let _ = stream.write_all(head(200, length).as_bytes());
            let chunk = vec![b' '; 64 * 1024];
            let mut sent = 0;
            while sent < length {
                let n = chunk.len().min(length - sent);
                if stream.write_all(&chunk[..n]).is_err() {
                    break;
                }
                sent += n;
            }
        }
    }
}

/// Web settings for `fake`, the key in an env-file of `dir`, full texts in `dir/store`.
fn web(dir: &Scratch, base_url: &str) -> Web {
    let env_file = dir.path("exa.env");
    fs::write(&env_file, format!("# exa\nexport EXA_API_KEY=\"{KEY}\"\n")).unwrap();
    Web {
        helper: PathBuf::from(HELPER),
        base_url: base_url.to_owned(),
        env_file,
        store_dir: dir.path("store"),
        char_limit: 15_000,
        timeout: Duration::from_secs(20),
    }
}

fn input(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn run_with(web: &Web, interrupt: &Interrupt, tool: &str, args: Value) -> Output {
    let dir = Scratch::new();
    let registry = Registry::builtin(
        &dir.path("spill"),
        Redactor { enabled: true },
        None,
        &fake_gateway::skills(&dir.0),
        Some(web),
    );
    let ctx = Context {
        cwd: &dir.0,
        interrupt,
    };
    registry.execute(tool, &input(args), ctx)
}

fn run(web: &Web, tool: &str, args: Value) -> Output {
    run_with(web, &Interrupt::new(), tool, args)
}

/// The result inside the untrusted-data frame of `tool`.
fn inner(tool: &str, out: &Output) -> String {
    let open = format!("<untrusted_tool_result source=\"{tool}\">\n");
    let rest = out
        .content
        .strip_prefix(&open)
        .unwrap_or_else(|| panic!("unframed: {}", out.content));
    let (notice, rest) = rest.split_once("\n\n").unwrap();
    assert!(notice.starts_with("The following content was retrieved from an external source."));
    rest.strip_suffix("\n</untrusted_tool_result>")
        .unwrap()
        .to_owned()
}

fn json_of(tool: &str, out: &Output) -> Value {
    serde_json::from_str(&inner(tool, out)).unwrap()
}

fn body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

fn header(request: &Request, name: &str) -> Option<String> {
    String::from_utf8_lossy(&request.head)
        .split("\r\n")
        .find_map(|line| {
            let (key, value) = line.split_once(": ")?;
            key.eq_ignore_ascii_case(name).then(|| value.to_owned())
        })
}

fn pages(results: &[(&str, &str)]) -> Value {
    let results: Vec<Value> = results
        .iter()
        .map(|(url, text)| json!({"url": url, "title": format!("T {url}"), "text": text}))
        .collect();
    json!({"requestId": "r1", "results": results, "statuses": []})
}

#[test]
fn search_sends_the_sdk_request_and_answers_the_reference_shape() {
    let dir = Scratch::new();
    let fake = FakeExa::start(vec![Answer::Json(
        200,
        json!({"results": [
            {"url": "https://one.example/", "title": "One", "highlights": ["first", "second"]},
            {"url": "https://two.example/", "title": null, "highlights": []}
        ]}),
    )]);
    let out = run(
        &web(&dir, &fake.base_url()),
        WEB_SEARCH,
        json!({"query": "rust ureq"}),
    );
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        inner(WEB_SEARCH, &out),
        r#"{
  "success": true,
  "data": {
    "web": [
      {
        "url": "https://one.example/",
        "title": "One",
        "description": "first second",
        "position": 1
      },
      {
        "url": "https://two.example/",
        "title": "",
        "description": "",
        "position": 2
      }
    ]
  }
}"#
    );
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].target(), "POST /search");
    assert_eq!(header(&requests[0], "x-api-key").as_deref(), Some(KEY));
    assert!(
        header(&requests[0], "user-agent")
            .unwrap()
            .starts_with("ulei-web/")
    );
    assert_eq!(header(&requests[0], "accept-encoding"), None);
    assert_eq!(
        body(&requests[0]),
        json!({"query": "rust ureq", "contents": {"highlights": true}, "numResults": 5})
    );
}

#[test]
fn search_limit_is_clamped_and_falls_back_to_five() {
    let dir = Scratch::new();
    let answers = (0..3)
        .map(|_| Answer::Json(200, json!({"results": []})))
        .collect();
    let fake = FakeExa::start(answers);
    let web = web(&dir, &fake.base_url());
    for limit in [json!(500), json!("many"), json!(0)] {
        let out = run(&web, WEB_SEARCH, json!({"query": "q", "limit": limit}));
        assert_eq!(
            json_of(WEB_SEARCH, &out),
            json!({"success": true, "data": {"web": []}})
        );
    }
    let sent: Vec<Value> = fake
        .requests()
        .iter()
        .map(|r| body(r)["numResults"].clone())
        .collect();
    assert_eq!(sent, [json!(100), json!(5), json!(1)]);
}

#[test]
fn an_exa_error_status_answers_the_sdk_failure_text() {
    let dir = Scratch::new();
    let fake = FakeExa::start(vec![Answer::Json(401, json!({"error": "bad key"}))]);
    let out = run(
        &web(&dir, &fake.base_url()),
        WEB_SEARCH,
        json!({"query": "q"}),
    );
    assert!(out.is_error);
    assert_eq!(
        json_of(WEB_SEARCH, &out),
        json!({"success": false, "error": "Request failed with status code 401: {\"error\":\"bad key\"}"})
    );
}

#[test]
fn extract_returns_a_page_within_the_default_budget_whole() {
    let dir = Scratch::new();
    let text = "x".repeat(15_000);
    let fake = FakeExa::start(vec![Answer::Json(
        200,
        pages(&[("https://a.example/", &text)]),
    )]);
    let out = run(
        &web(&dir, &fake.base_url()),
        WEB_EXTRACT,
        json!({"urls": ["https://93.184.215.14/a"]}),
    );
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        json_of(WEB_EXTRACT, &out),
        json!({"results": [{"url": "https://a.example/", "title": "T https://a.example/", "content": text, "error": null}]})
    );
    assert_eq!(
        body(&fake.requests()[0]),
        json!({"urls": ["https://93.184.215.14/a"], "text": true})
    );
    assert!(!dir.path("store").exists());
}

/// The cut page of a single-line text of `total` characters under `limit`, and the stored file.
fn cut(total: usize, char_limit: &Value) -> (String, PathBuf, String) {
    let dir = Scratch::new();
    let text: String = (0..total)
        .map(|i| char::from(b'a' + u8::try_from(i % 26).unwrap()))
        .collect();
    let fake = FakeExa::start(vec![Answer::Json(
        200,
        pages(&[("https://cut.example:8443/p?q=1", &text)]),
    )]);
    let out = run(
        &web(&dir, &fake.base_url()),
        WEB_EXTRACT,
        json!({"urls": ["https://93.184.215.14/"], "char_limit": char_limit}),
    );
    let content = json_of(WEB_EXTRACT, &out)["results"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let stored: Vec<PathBuf> = fs::read_dir(dir.path("store"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(stored.len(), 1);
    let file = fs::read_to_string(&stored[0]).unwrap();
    assert_eq!(file, text);
    (content, stored[0].clone(), text)
}

fn expected_cut(
    text: &str,
    head: usize,
    tail: usize,
    path: &Path,
    shown: (&str, &str, &str),
) -> String {
    let rule = "\u{2500}".repeat(8);
    let path = path.display();
    format!(
        "{}\n\n[... middle omitted \u{2014} see footer ...]\n\n{}\n\n{rule} [TRUNCATED] {rule}\nShowing {} chars (head) + {} chars (tail) of {} total clean characters.\nFull text saved to: {path}\nTo read the omitted middle: read_file path=\"{path}\" offset=2 limit=200  (the file is the complete page; raise/lower offset to page through it).\n{}",
        &text[..head],
        &text[text.len() - tail..],
        shown.0,
        shown.1,
        shown.2,
        "\u{2500}".repeat(29)
    )
}

#[test]
fn extract_cuts_at_the_lower_edge_of_the_budget_range() {
    let (content, path, text) = cut(3_000, &json!(500));
    let name = path.file_name().unwrap().to_str().unwrap();
    assert_eq!(name.len(), "cut.example-".len() + 10 + ".md".len());
    assert!(
        name.starts_with("cut.example-") && path.extension() == Some("md".as_ref()),
        "{name}"
    );
    assert_eq!(
        content,
        expected_cut(&text, 1_500, 500, &path, ("1,500", "500", "3,000"))
    );
}

#[test]
fn extract_cuts_at_the_upper_edge_of_the_budget_range() {
    let (content, path, text) = cut(600_000, &json!(900_000));
    assert_eq!(
        content,
        expected_cut(
            &text,
            375_000,
            125_000,
            &path,
            ("375,000", "125,000", "600,000")
        )
    );
}

#[test]
fn extract_snaps_the_cut_to_line_breaks_and_points_past_the_head() {
    let dir = Scratch::new();
    let lines: Vec<String> = (0..100)
        .map(|i| format!("line {i:03} {}", "z".repeat(30)))
        .collect();
    let text = lines.join("\n");
    let fake = FakeExa::start(vec![Answer::Json(
        200,
        pages(&[("https://lines.example/", &text)]),
    )]);
    let out = run(
        &web(&dir, &fake.base_url()),
        WEB_EXTRACT,
        json!({"urls": ["https://93.184.215.14/"], "char_limit": 2000}),
    );
    let content = json_of(WEB_EXTRACT, &out)["results"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let (head, rest) = content
        .split_once("\n\n[... middle omitted \u{2014} see footer ...]\n\n")
        .unwrap();
    let tail = rest.split_once("\n\n\u{2500}").unwrap().0;
    // 1500 head characters end inside line 37, 500 tail characters start inside line 87.
    assert_eq!(head, lines[..37].join("\n"));
    assert_eq!(tail, lines[88..].join("\n"));
    assert!(content.contains("offset=38 limit=200"), "{content}");
}

#[test]
fn extract_takes_at_most_five_urls() {
    let dir = Scratch::new();
    let fake = FakeExa::start(vec![Answer::Json(200, pages(&[]))]);
    let urls: Vec<String> = (1..=7)
        .map(|i| format!("https://93.184.215.14/{i}"))
        .collect();
    let out = run(
        &web(&dir, &fake.base_url()),
        WEB_EXTRACT,
        json!({"urls": urls}),
    );
    assert_eq!(body(&fake.requests()[0])["urls"], json!(urls[..5]));
    assert_eq!(
        inner(WEB_EXTRACT, &out),
        r#"{"error": "Content was inaccessible or not found"}"#
    );
}

#[test]
fn extract_refuses_private_targets_before_any_backend_call() {
    let dir = Scratch::new();
    let fake = FakeExa::start(Vec::new());
    let urls = [
        "http://10.0.0.1/a",
        "http://192.168.1.1/",
        "http://127.0.0.1:8080/b",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::1]/c",
        "http://[fe80::1]/d",
        "http://localhost/e",
        "http://metadata.google.internal/",
        "ftp://93.184.215.14/f",
    ];
    let out = run(
        &web(&dir, &fake.base_url()),
        WEB_EXTRACT,
        json!({"urls": urls[..5]}),
    );
    let out2 = run(
        &web(&dir, &fake.base_url()),
        WEB_EXTRACT,
        json!({"urls": urls[5..]}),
    );
    assert!(out.is_error && out2.is_error);
    let results: Vec<Value> = [out, out2]
        .iter()
        .flat_map(|o| {
            json_of(WEB_EXTRACT, o)["results"]
                .as_array()
                .unwrap()
                .clone()
        })
        .collect();
    let expected: Vec<Value> = urls
        .iter()
        .map(|u| json!({"url": u, "title": "", "content": "", "error": PRIVATE_TARGET}))
        .collect();
    assert_eq!(results, expected);
    assert_eq!(fake.requests(), Vec::new());
}

#[test]
fn extract_keeps_input_order_around_refused_and_invalid_items() {
    let dir = Scratch::new();
    let fake = FakeExa::start(vec![Answer::Json(
        200,
        pages(&[("https://93.184.215.14/ok", "fine")]),
    )]);
    let out = run(
        &web(&dir, &fake.base_url()),
        WEB_EXTRACT,
        json!({"urls": [42, "http://10.0.0.1/", {"href": "https://93.184.215.14/ok"}, "https://93.184.215.14/gone"]}),
    );
    let results = json_of(WEB_EXTRACT, &out)["results"].clone();
    assert_eq!(
        results,
        json!([
            {"url": "", "title": "", "content": "", "error": "Invalid URL item at index 0: expected a URL string or an object with a string 'url' or 'href' field"},
            {"url": "http://10.0.0.1/", "title": "", "content": "", "error": PRIVATE_TARGET},
            {"url": "https://93.184.215.14/ok", "title": "T https://93.184.215.14/ok", "content": "fine", "error": null},
            {"url": "https://93.184.215.14/gone", "title": "", "content": "", "error": "Extract backend returned no result for this URL"}
        ])
    );
}

#[test]
fn extract_refuses_a_whole_call_with_a_credential_in_a_url() {
    let dir = Scratch::new();
    let fake = FakeExa::start(Vec::new());
    let web = web(&dir, &fake.base_url());
    let out = run(
        &web,
        WEB_EXTRACT,
        json!({"urls": ["https://93.184.215.14/?q=%73k-abcdefghijklmnop"]}),
    );
    assert_eq!(
        inner(WEB_EXTRACT, &out),
        r#"{"success": false, "error": "Blocked: URL contains what appears to be an API key or token. Secrets must not be sent in URLs."}"#
    );
    let out = run(
        &web,
        WEB_EXTRACT,
        json!({"urls": ["https://93.184.215.14/cb?Access_Token=opaque"]}),
    );
    assert_eq!(
        json_of(WEB_EXTRACT, &out)["error"],
        "Blocked: URL contains a credential-like query parameter (Access_Token). Web extract backends are third-party readers; remove the sensitive query parameter or use a local browser session when this access is explicitly required."
    );
    assert_eq!(fake.requests(), Vec::new());
}

#[test]
fn a_redirect_from_exa_is_not_followed() {
    let dir = Scratch::new();
    let fake = FakeExa::start(vec![Answer::Raw(
        b"HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    )]);
    let out = run(
        &web(&dir, &fake.base_url()),
        WEB_SEARCH,
        json!({"query": "q"}),
    );
    assert_eq!(
        json_of(WEB_SEARCH, &out),
        json!({"success": false, "error": "Exa search failed: redirect not followed: status code 302"})
    );
    assert_eq!(fake.requests().len(), 1);
}

#[test]
fn an_exa_reply_over_the_size_cap_fails_every_page() {
    let dir = Scratch::new();
    let fake = FakeExa::start(vec![Answer::Oversized]);
    let out = run(
        &web(&dir, &fake.base_url()),
        WEB_EXTRACT,
        json!({"urls": ["https://93.184.215.14/big"]}),
    );
    assert!(out.is_error);
    let page = &json_of(WEB_EXTRACT, &out)["results"][0];
    assert_eq!(page["url"], "https://93.184.215.14/big");
    let error = page["error"].as_str().unwrap();
    assert!(
        error.starts_with("Exa extract failed: ") && error.contains("16777216"),
        "{error}"
    );
}

#[test]
fn an_exa_call_past_its_time_limit_fails() {
    let dir = Scratch::new();
    let fake = FakeExa::start(vec![Answer::Hang]);
    let mut web = web(&dir, &fake.base_url());
    web.timeout = Duration::from_millis(500);
    let started = Instant::now();
    let out = run(&web, WEB_SEARCH, json!({"query": "q"}));
    assert!(started.elapsed() < Duration::from_secs(4));
    let error = json_of(WEB_SEARCH, &out)["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        error.starts_with("Exa search failed: ") && error.contains("timeout"),
        "{error}"
    );
}

#[test]
fn an_interrupt_kills_the_helper_and_answers_interrupted() {
    let dir = Scratch::new();
    let fake = FakeExa::start(vec![Answer::Hang]);
    let mut web = web(&dir, &fake.base_url());
    web.helper = wrapper(&dir);
    let interrupt = Interrupt::new();
    let stopper = {
        let interrupt = interrupt.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            interrupt.interrupt();
        })
    };
    let started = Instant::now();
    let out = run_with(&web, &interrupt, WEB_SEARCH, json!({"query": "q"}));
    stopper.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(out.is_error);
    assert_eq!(
        inner(WEB_SEARCH, &out),
        r#"{"error": "Interrupted", "success": false}"#
    );
    let pid = fs::read_to_string(dir.path("pid")).unwrap();
    assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists());
}

#[test]
fn a_missing_key_answers_the_reference_error_without_a_backend_call() {
    let dir = Scratch::new();
    let fake = FakeExa::start(Vec::new());
    let web = web(&dir, &fake.base_url());
    fs::write(&web.env_file, "OTHER=1\nEXA_API_KEY=  \n").unwrap();
    let out = run(&web, WEB_SEARCH, json!({"query": "q"}));
    assert_eq!(
        json_of(WEB_SEARCH, &out),
        json!({"success": false, "error": MISSING_KEY})
    );
    let out = run(
        &web,
        WEB_EXTRACT,
        json!({"urls": ["https://93.184.215.14/"]}),
    );
    assert_eq!(
        json_of(WEB_EXTRACT, &out),
        json!({"results": [{"url": "https://93.184.215.14/", "title": "", "content": "", "error": MISSING_KEY}]})
    );
    assert_eq!(fake.requests(), Vec::new());
}

#[test]
fn a_base_url_that_is_not_https_or_loopback_is_refused() {
    let dir = Scratch::new();
    let out = run(
        &web(&dir, "http://exa.invalid"),
        WEB_SEARCH,
        json!({"query": "q"}),
    );
    assert_eq!(
        json_of(WEB_SEARCH, &out),
        json!({"success": false, "error": "Exa search failed: the Exa base URL must be an https:// URL"})
    );
}

/// A helper wrapper that records its argv, its initial environment and its pid in `dir`,
/// then becomes the real helper.
fn wrapper(dir: &Scratch) -> PathBuf {
    let path = dir.path("ulei-web-wrapper");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$0\" \"$@\" > {argv}\ncat /proc/$$/environ > {environ}\necho $$ > {pid}\nexec {HELPER}\n",
        argv = dir.path("argv").display(),
        environ = dir.path("environ").display(),
        pid = dir.path("pid").display(),
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn the_key_reaches_only_the_exa_header() {
    let dir = Scratch::new();
    let fake = FakeExa::start(vec![
        Answer::Json(
            200,
            json!({"results": [{"url": "https://k.example/", "title": "K", "highlights": ["h"]}]}),
        ),
        Answer::Json(403, json!({"error": "forbidden"})),
    ]);
    let mut settings = web(&dir, &fake.base_url());
    settings.helper = wrapper(&dir);
    let gateway = FakeGateway::start(vec![
        tool_step(vec![
            call("c1", WEB_SEARCH, json!({"query": "first"})),
            call("c2", WEB_SEARCH, json!({"query": "second"})),
            call("c3", WEB_EXTRACT, json!({"urls": ["http://10.0.0.1/"]})),
        ]),
        text_step("done"),
    ]);
    let db = dir.path("journal.sqlite3");
    let mut journal = Journal::open(&db).unwrap();
    let session = journal.session("main").unwrap();
    let turn = journal.next_turn(session.id).unwrap();
    let registry = Registry::builtin(
        &dir.path("spill"),
        Redactor { enabled: true },
        None,
        &fake_gateway::skills(&dir.0),
        Some(&settings),
    );
    let client = Client::new(
        gateway.socket(),
        Duration::from_secs(5),
        Duration::from_secs(30),
    );
    let template = Template {
        route: "chat".into(),
        system: Some("You are a test.".into()),
        max_tokens: 1024,
        effort: None,
        session_key: Some("main".into()),
    };
    let prompt = Message {
        role: Role::User,
        content: vec![fake_gateway::text_block("look it up")],
    };
    let outcome = Agent {
        client: &client,
        tools: &registry,
        chain: &mut Chain::default(),
        cwd: &dir.0,
        template: &template,
        batch_timeout: Some(DEFAULT_CONCURRENT_TOOL_TIMEOUT),
        empty_guard: true,
    }
    .run(
        std::slice::from_ref(&prompt),
        &|n| turn.request_id(n),
        &Interrupt::new(),
        8,
        &mut Recorder::default(),
    );
    journal.append(turn, &[prompt], &outcome).unwrap();
    drop(journal);

    let sent: Vec<Option<String>> = fake
        .requests()
        .iter()
        .map(|r| header(r, "x-api-key"))
        .collect();
    assert_eq!(sent, [Some(KEY.to_owned()), Some(KEY.to_owned())]);
    let results = serde_json::to_string(&outcome.messages).unwrap();
    assert!(
        results.contains("Request failed with status code 403"),
        "{results}"
    );
    assert!(results.contains(PRIVATE_TARGET), "{results}");
    for (place, bytes) in [
        ("tool results", results.into_bytes()),
        ("journal", fs::read(&db).unwrap()),
        ("argv", fs::read(dir.path("argv")).unwrap()),
        ("environment", fs::read(dir.path("environ")).unwrap()),
    ] {
        assert!(
            !bytes.windows(KEY.len()).any(|w| w == KEY.as_bytes()),
            "the key reached the {place}"
        );
    }
    assert_eq!(fs::read(dir.path("environ")).unwrap(), b"");
    assert_eq!(
        fs::read_to_string(dir.path("argv"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    for request in gateway.requests() {
        assert!(!request.body.windows(KEY.len()).any(|w| w == KEY.as_bytes()));
    }
}

#[test]
fn the_helper_writes_nothing_but_its_reply_even_when_exa_is_down() {
    let dir = Scratch::new();
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://127.0.0.1:{}", closed.local_addr().unwrap().port());
    drop(closed);
    let settings = web(&dir, &base_url);
    let request = HelperRequest {
        base_url: settings.base_url.clone(),
        env_file: settings.env_file.clone(),
        store_dir: settings.store_dir.clone(),
        timeout: Duration::from_secs(5),
        op: Op::Search {
            query: "q".into(),
            limit: 5,
        },
    };
    let mut child = Command::new(HELPER)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_json().to_string().as_bytes())
        .unwrap();
    let done = child.wait_with_output().unwrap();
    assert_eq!(done.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&done.stderr), "");
    let reply: Value = serde_json::from_slice(&done.stdout).unwrap();
    let error = reply["error"].as_str().unwrap();
    assert!(error.starts_with("Exa search failed: "), "{error}");
    assert!(!error.contains(KEY));
}

#[test]
fn a_malformed_request_fails_the_helper() {
    let mut child = Command::new(HELPER)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"op\": \"fetch\"}")
        .unwrap();
    let done = child.wait_with_output().unwrap();
    assert_eq!(done.status.code(), Some(1));
    let reply: Value = serde_json::from_slice(&done.stdout).unwrap();
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid web helper request: ")
    );
}
