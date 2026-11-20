// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `web_search` and `web_extract`: the reference schemas and argument handling; each call runs
//! the `ulei-web` helper once, in its own process group with an empty environment, and its
//! reply is the result the model sees, framed as untrusted data.

use std::io::{self, Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::terminal::{End, Event, spawn_waiter, terminate, wait_for_end};
use super::{
    Context, Interrupt, Output, Tool, json_object, json_str, object_schema, py_int, untrusted,
};

/// Name of the web search tool.
pub const WEB_SEARCH: &str = "web_search";
/// Name of the web page extraction tool.
pub const WEB_EXTRACT: &str = "web_extract";

/// Characters per page a `web_extract` call returns whole when it names no `char_limit`.
pub const DEFAULT_EXTRACT_CHAR_LIMIT: usize = 15_000;
/// Smallest per-page character budget; a lower one is raised to it.
pub const MIN_EXTRACT_CHAR_LIMIT: usize = 2_000;
/// Largest per-page character budget; a higher one is cut to it.
pub const MAX_EXTRACT_CHAR_LIMIT: usize = 500_000;
/// Most URLs one `web_extract` call takes; the rest are dropped.
pub const MAX_EXTRACT_URLS: usize = 5;
/// Overall limit of one Exa call, enforced by the helper.
pub const EXA_CALL_TIMEOUT: Duration = Duration::from_secs(300);
/// How long past the Exa call limit the engine waits before killing the helper.
pub const HELPER_GRACE: Duration = Duration::from_secs(5);
/// Most bytes of helper reply the engine reads.
pub const MAX_HELPER_REPLY: usize = 16 * 1024 * 1024;

const SEARCH_DESCRIPTION: &str = "Search the web for information. Returns up to 5 results by \
default with titles, URLs, and descriptions. The query is passed through to the configured \
backend, so operators such as site:domain, filetype:pdf, intitle:word, -term, and \"exact \
phrase\" may work when the backend supports them.";

const EXTRACT_DESCRIPTION: &str = "Extract content from web page URLs. Returns clean page content \
in markdown/text (no LLM summarization \u{2014} fast). Also works with PDF URLs (arxiv papers, \
documents) \u{2014} pass the PDF link directly. Pages within the char budget (default 15000) \
return whole; larger pages return a head+tail window with a footer telling you the full text's \
saved file path and the read_file call to page through the omitted middle. Inline images appear \
as [IMAGE: alt] placeholders; real image URLs are kept as links. If a URL fails or times out, use \
the browser tool instead.";

/// Where the web tools reach Exa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Web {
    /// The `ulei-web` helper binary.
    pub helper: PathBuf,
    /// Base URL of the Exa API; `/search` and `/contents` are appended.
    pub base_url: String,
    /// Env-file holding `EXA_API_KEY`; the helper reads it, the engine passes only the path.
    pub env_file: PathBuf,
    /// Directory of the full texts of truncated pages.
    pub store_dir: PathBuf,
    /// Per-page character budget of a call that names none, within the clamp range.
    pub char_limit: usize,
    /// Overall limit of one Exa call; the engine kills the helper [`HELPER_GRACE`] after it.
    pub timeout: Duration,
}

/// What one helper process is asked to do: read on its stdin as one JSON object.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// The Exa base URL.
    pub base_url: String,
    /// The env-file holding `EXA_API_KEY`.
    pub env_file: PathBuf,
    /// Directory of the stored full texts.
    pub store_dir: PathBuf,
    /// Overall limit of the Exa call.
    pub timeout: Duration,
    /// The operation and its arguments.
    pub op: Op,
}

/// The operation of a [`Request`].
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// `web_search`: the query as the model sent it, and the clamped result count.
    Search {
        /// The `query` value, `""` when absent.
        query: Value,
        /// Results asked for, 1 to 100.
        limit: i64,
    },
    /// `web_extract`: at most [`MAX_EXTRACT_URLS`] items as the model sent them, and the
    /// clamped per-page budget.
    Extract {
        /// URL strings or search-result objects.
        urls: Vec<Value>,
        /// Characters per page.
        char_limit: usize,
    },
}

impl Request {
    /// The JSON object the helper reads.
    pub fn to_json(&self) -> Value {
        let timeout_ms = u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX);
        let mut out = json!({
            "base_url": self.base_url,
            "env_file": self.env_file.to_string_lossy(),
            "store_dir": self.store_dir.to_string_lossy(),
            "timeout_ms": timeout_ms,
        });
        let fields = match &self.op {
            Op::Search { query, limit } => json!({"op": "search", "query": query, "limit": limit}),
            Op::Extract { urls, char_limit } => {
                json!({"op": "extract", "urls": urls, "char_limit": char_limit})
            }
        };
        if let (Value::Object(out), Value::Object(fields)) = (&mut out, fields) {
            out.extend(fields);
        }
        out
    }

    /// The request in `input`.
    ///
    /// # Errors
    /// The first field that is missing or of the wrong type.
    pub fn from_json(input: &[u8]) -> Result<Self, String> {
        let value: Value = serde_json::from_slice(input).map_err(|e| e.to_string())?;
        let object = value.as_object().ok_or("the request is not an object")?;
        let text = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("field `{name}` must be a string"))
        };
        let number = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("field `{name}` must be a non-negative integer"))
        };
        let op = match text("op")? {
            "search" => Op::Search {
                query: object.get("query").cloned().unwrap_or_else(|| "".into()),
                limit: object
                    .get("limit")
                    .and_then(Value::as_i64)
                    .ok_or("field `limit` must be an integer")?,
            },
            "extract" => Op::Extract {
                urls: object
                    .get("urls")
                    .and_then(Value::as_array)
                    .ok_or("field `urls` must be an array")?
                    .clone(),
                char_limit: usize::try_from(number("char_limit")?).map_err(|e| e.to_string())?,
            },
            other => return Err(format!("unknown op `{other}`")),
        };
        Ok(Self {
            base_url: text("base_url")?.to_owned(),
            env_file: PathBuf::from(text("env_file")?),
            store_dir: PathBuf::from(text("store_dir")?),
            timeout: Duration::from_millis(number("timeout_ms")?),
            op,
        })
    }
}

/// `value` as the reference's `int()` and clamp of an extract budget read it: `fallback` when
/// it is no integer.
pub fn clamp_char_limit(value: Option<&Value>, fallback: usize) -> usize {
    py_int(value).map_or(fallback, |n| {
        usize::try_from(n.max(0))
            .unwrap_or(usize::MAX)
            .clamp(MIN_EXTRACT_CHAR_LIMIT, MAX_EXTRACT_CHAR_LIMIT)
    })
}

/// The `web_search` tool.
pub(super) struct WebSearch {
    pub(super) web: Web,
}

/// The `web_extract` tool.
pub(super) struct WebExtract {
    pub(super) web: Web,
}

impl Tool for WebSearch {
    fn definition(&self) -> c4g::Tool {
        definition(
            WEB_SEARCH,
            SEARCH_DESCRIPTION,
            json!({
                "query": {
                    "type": "string",
                    "description": "The search query to look up on the web. You may include \
                        backend-supported operators such as site:example.com, filetype:pdf, \
                        intitle:word, -term, or \"exact phrase\"."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return. Defaults to 5.",
                    "minimum": 1,
                    "maximum": 100,
                    "default": 5
                }
            }),
            "query",
        )
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let limit = py_int(Some(input.get("limit").unwrap_or(&Value::from(5))))
            .unwrap_or(5)
            .clamp(1, 100);
        let op = Op::Search {
            query: input.get("query").cloned().unwrap_or_else(|| "".into()),
            limit,
        };
        untrusted(
            WEB_SEARCH,
            call(&self.web, op, ctx.interrupt, "Error searching web"),
        )
    }
}

impl Tool for WebExtract {
    fn definition(&self) -> c4g::Tool {
        definition(
            WEB_EXTRACT,
            EXTRACT_DESCRIPTION,
            json!({
                "urls": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "List of URLs to extract content from (max 5 URLs per call)",
                    "maxItems": 5
                },
                "char_limit": {
                    "type": "integer",
                    "description": "Optional per-page character budget sent back (default \
                        15000). Pages larger than this are head+tail truncated with the full \
                        text stored to disk. Raise it when you need more of a long page \
                        inline.",
                    "minimum": 2000
                }
            }),
            "urls",
        )
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let urls = match input.get("urls") {
            Some(Value::Array(items)) => items.iter().take(MAX_EXTRACT_URLS).cloned().collect(),
            _ => Vec::new(),
        };
        let char_limit = match input.get("char_limit") {
            None | Some(Value::Null) => self.web.char_limit,
            given => clamp_char_limit(given, DEFAULT_EXTRACT_CHAR_LIMIT),
        };
        let op = Op::Extract { urls, char_limit };
        untrusted(
            WEB_EXTRACT,
            call(&self.web, op, ctx.interrupt, "Error extracting content"),
        )
    }
}

fn definition(name: &str, description: &str, properties: Value, required: &str) -> c4g::Tool {
    c4g::Tool {
        name: name.into(),
        description: description.into(),
        input_schema: object_schema(properties, &[required]),
        native: None,
    }
}

/// Why a helper run gave no reply.
enum Failure {
    Interrupted,
    TimedOut(Duration),
    Io(io::Error),
}

/// Runs the helper on `op`; a failure of the run is a tool error after `prefix`.
fn call(web: &Web, op: Op, interrupt: &Interrupt, prefix: &str) -> Output {
    let request = Request {
        base_url: web.base_url.clone(),
        env_file: web.env_file.clone(),
        store_dir: web.store_dir.clone(),
        timeout: web.timeout,
        op,
    };
    let deadline = web.timeout.saturating_add(HELPER_GRACE);
    match run_helper(web, &request.to_json().to_string(), deadline, interrupt) {
        Ok((reply, _)) if reply.len() > MAX_HELPER_REPLY => Output::error(format!(
            "{prefix}: the web helper reply exceeds {MAX_HELPER_REPLY} bytes"
        )),
        Ok((reply, status)) => match (String::from_utf8(reply), status.code()) {
            (Ok(text), Some(0)) => Output::ok(text),
            (Ok(text), Some(1)) => Output {
                content: text,
                is_error: true,
            },
            (Ok(_), _) => Output::error(format!("{prefix}: the web helper ended with {status}")),
            (Err(_), _) => Output::error(format!("{prefix}: the web helper reply is not UTF-8")),
        },
        Err(Failure::Interrupted) => Output {
            content: json_object([
                ("error", json_str("Interrupted")),
                ("success", "false".into()),
            ]),
            is_error: true,
        },
        Err(Failure::TimedOut(after)) => Output::error(format!(
            "{prefix}: the web helper did not answer within {}s",
            after.as_secs()
        )),
        Err(Failure::Io(e)) => Output::error(format!("{prefix}: {e}")),
    }
}

/// The helper's stdout, at most one byte past [`MAX_HELPER_REPLY`], and its exit status.
fn run_helper(
    web: &Web,
    request: &str,
    within: Duration,
    interrupt: &Interrupt,
) -> Result<(Vec<u8>, ExitStatus), Failure> {
    let mut child = Command::new(&web.helper)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(Failure::Io)?;
    let group = Pid::from_child(&child);
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = kill_process_group(group, Signal::KILL);
        let _ = child.wait();
        return Err(Failure::Io(io::Error::other(
            "the helper pipes are missing",
        )));
    };
    let (events, inbox) = mpsc::channel();
    let deadline = Instant::now() + within;
    let request = request.to_owned();
    let eof = events.clone();
    let started = thread::Builder::new()
        .name("web-helper-io".into())
        .spawn(move || {
            // A helper that exits without reading its request still answers on stdout.
            let _ = stdin.write_all(request.as_bytes());
            drop(stdin);
            let mut reply = Vec::new();
            let limit = u64::try_from(MAX_HELPER_REPLY).unwrap_or(u64::MAX) + 1;
            let read = stdout.take(limit).read_to_end(&mut reply);
            let _ = eof.send(Event::Eof);
            read.map(|_| reply)
        })
        .and_then(|io| spawn_waiter(group, &events).map(|waiter| (io, waiter)));
    let (io, waiter) = match started {
        Ok(threads) => threads,
        Err(e) => {
            // ESRCH only: the helper already exited.
            let _ = kill_process_group(group, Signal::KILL);
            let _ = child.wait();
            return Err(Failure::Io(e));
        }
    };
    let end = match interrupt.attach(events) {
        Some(running) => {
            let end = wait_for_end(&inbox, deadline);
            interrupt.detach(&running);
            end
        }
        None => End::Interrupted,
    };
    match end {
        // ESRCH only: every member already exited; the unreaped leader keeps the id reserved.
        End::Exited => {
            let _ = kill_process_group(group, Signal::KILL);
        }
        End::TimedOut | End::Interrupted => terminate(group, &mut child),
    }
    let _ = waiter.join();
    let status = child.wait().map_err(Failure::Io)?;
    let reply = io
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("the reply reader panicked")));
    match end {
        End::Exited => reply.map(|reply| (reply, status)).map_err(Failure::Io),
        End::TimedOut => Err(Failure::TimedOut(within)),
        End::Interrupted => Err(Failure::Interrupted),
    }
}
