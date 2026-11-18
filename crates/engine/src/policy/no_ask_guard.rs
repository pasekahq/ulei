//! `no-ask-guard`: the final reply of a top-level owner turn that ends with a question goes to a
//! tool-less classifier call through qb; a needless question does not end the turn, which
//! continues with the configured note, at most [`MAX_CONSECUTIVE`] times per chain.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use ulei_contracts::generated::c4g::{Message, Role, TurnRequest};

use super::{Origin, Policy, StopDecision};
use crate::agent::{self, text, text_message};
use crate::gateway::Client;
use crate::tools::{Interrupt, is_py_space, last_chars, py_splitlines, py_strip, random_hex};

/// Most suppressions of one chain; the reply after the last one is delivered.
const MAX_CONSECUTIVE: u32 = 2;
/// Deadline of one classifier call when none is configured.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(45);
/// Placeholders of the continuation note.
pub const NOTE_FIELDS: &[&str] = &["chain"];
/// Placeholders of the classifier instructions: the one-time tag, the owner's request and a
/// part of the reply.
pub const INSTRUCTION_FIELDS: &[&str] = &["tag", "owner", "reply"];

/// Characters of the reply's end that the pre-filter reads and every long-reply part repeats.
const TAIL_CHARS: usize = 600;
/// Characters of the owner's request the classifier gets.
const OWNER_CHARS: usize = 1800;
/// Most characters of one classified part of the reply.
const REPLY_PART_CHARS: usize = 8000;
/// Characters of the reply's end kept in a log record.
const LOG_TAIL_CHARS: usize = 300;

/// A text whose `{name}` placeholders are filled at render; `{{` and `}}` stand for braces, as
/// in Python's `str.format`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextTemplate {
    pieces: Vec<Piece>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    Field(usize),
}

impl TextTemplate {
    /// The template of `text`, whose placeholders are exactly the names of `fields`.
    ///
    /// # Errors
    /// The reason when a brace is unbalanced, a placeholder is unknown or a field is unused.
    pub fn parse(text: &str, fields: &[&str]) -> Result<Self, String> {
        let mut pieces = Vec::new();
        let mut literal = String::new();
        let mut used = vec![false; fields.len()];
        let mut rest = text;
        while let Some(at) = rest.find(['{', '}']) {
            literal.push_str(&rest[..at]);
            let brace = &rest[at..=at];
            let after = &rest[at + 1..];
            if let Some(next) = after.strip_prefix(brace) {
                literal.push_str(brace);
                rest = next;
                continue;
            }
            if brace == "}" {
                return Err("a single '}' outside a placeholder".to_owned());
            }
            let end = after
                .find('}')
                .ok_or_else(|| "a '{' without its '}'".to_owned())?;
            let name = &after[..end];
            let field = fields
                .iter()
                .position(|f| *f == name)
                .ok_or_else(|| format!("unknown placeholder {{{name}}}"))?;
            used[field] = true;
            if !literal.is_empty() {
                pieces.push(Piece::Text(std::mem::take(&mut literal)));
            }
            pieces.push(Piece::Field(field));
            rest = &after[end + 1..];
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            pieces.push(Piece::Text(literal));
        }
        match used.iter().position(|u| !u) {
            Some(n) => Err(format!("no {{{}}} placeholder", fields[n])),
            None => Ok(Self { pieces }),
        }
    }

    /// The text with `values[n]` in place of the `n`-th field.
    pub fn render(&self, values: &[&str]) -> String {
        self.pieces
            .iter()
            .map(|piece| match piece {
                Piece::Text(text) => text.as_str(),
                Piece::Field(n) => values[*n],
            })
            .collect()
    }
}

/// The tool-less classifier call through qb.
#[derive(Debug, Clone)]
pub struct Classifier {
    /// The qb gateway.
    pub client: Client,
    /// Route alias of the call.
    pub route: String,
    /// The instructions with the [`INSTRUCTION_FIELDS`].
    pub instructions: TextTemplate,
    /// Upper bound of output tokens of the call.
    pub max_tokens: u64,
    /// Deadline of one call.
    pub timeout: Duration,
}

/// A verdict: `extra`, `legit` or `error`, with the classifier's category and reason.
struct Ruling {
    verdict: &'static str,
    category: String,
    reason: String,
}

impl Classifier {
    /// The verdict on `reply` to `owner`; any failure, an interrupt included, is the `error`
    /// verdict.
    fn classify(&self, owner: &str, reply: &str, interrupt: &Interrupt) -> Ruling {
        self.verdicts(owner, reply, interrupt)
            .unwrap_or_else(|reason| Ruling {
                verdict: "error",
                category: String::new(),
                reason: reason.chars().take(150).collect(),
            })
    }

    /// One call per part of the reply, each with a fresh tag; any `legit` part wins.
    fn verdicts(&self, owner: &str, reply: &str, interrupt: &Interrupt) -> Result<Ruling, String> {
        let (owner, parts) = prepare(owner, reply);
        let mut rulings = Vec::with_capacity(parts.len());
        for part in &parts {
            let tag = format!("⟦DATA-{}⟧", random_hex(6).map_err(|e| e.to_string())?);
            if part.contains(&tag) || owner.contains(&tag) {
                return Ok(Ruling {
                    verdict: "legit",
                    category: String::new(),
                    reason: "tag in the data".to_owned(),
                });
            }
            let prompt = self.instructions.render(&[&tag, &owner, part]);
            rulings.push(parse(&self.ask(&prompt, interrupt)?)?);
        }
        let first = rulings
            .iter()
            .position(|r| r.verdict == "legit")
            .unwrap_or(0);
        Ok(rulings.swap_remove(first))
    }

    /// The text of the answer to `prompt`; a call past the deadline or under a fired
    /// `interrupt` is cancelled.
    fn ask(&self, prompt: &str, interrupt: &Interrupt) -> Result<String, String> {
        let request = TurnRequest {
            route: self.route.clone(),
            system: None,
            messages: vec![text_message(Role::User, prompt)],
            tools: None,
            max_tokens: self.max_tokens,
            effort: None,
            session_key: None,
            cache: None,
            request_id: format!("ask-{}", random_hex(8).map_err(|e| e.to_string())?),
        };
        agent::side_turn(&self.client, &request, Some(self.timeout), interrupt)
            .map_err(|ended| ended.to_string())
    }
}

/// The owner's request of a chain, the chain id of the suppression the current reply follows
/// and the suppressions so far.
struct Thread {
    owner: String,
    chain: Option<String>,
    streak: u32,
}

/// Holds back a needless closing question of a top-level owner turn and continues the turn.
pub struct NoAskGuard {
    classifier: Classifier,
    note: TextTemplate,
    log: PathBuf,
    session: String,
    thread: Option<Thread>,
    chain_id: fn() -> io::Result<String>,
}

impl NoAskGuard {
    /// The policy asking `classifier`, continuing with `note` and appending its records to the
    /// JSON-lines file `log` under the session key `session`.
    pub fn new(classifier: Classifier, note: TextTemplate, log: PathBuf, session: String) -> Self {
        Self {
            classifier,
            note,
            log,
            session,
            thread: None,
            chain_id: || random_hex(8),
        }
    }

    /// The policy drawing each chain id from `chain_id` instead of 8 bytes of `/dev/urandom`.
    #[must_use]
    pub fn with_chain_id(self, chain_id: fn() -> io::Result<String>) -> Self {
        Self { chain_id, ..self }
    }

    /// The new chain id when `reply` is suppressed; every verdict is logged, and a suppression
    /// that cannot be logged does not happen.
    fn check(
        &self,
        thread: &Thread,
        reply: &str,
        parent: Option<&str>,
        interrupt: &Interrupt,
    ) -> Option<String> {
        let started = Instant::now();
        let ruling = self.classifier.classify(&thread.owner, reply, interrupt);
        let mut record = json!({
            "session_id": self.session,
            "reply_tail": last_chars(py_strip(reply), LOG_TAIL_CHARS),
            "ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "verdict": ruling.verdict,
            "reason": ruling.reason,
            "category": ruling.category,
        });
        if let Some(parent) = parent {
            record["parent"] = parent.into();
        }
        let action = if ruling.verdict != "extra" {
            "pass"
        } else if thread.streak >= MAX_CONSECUTIVE {
            record["streak"] = thread.streak.into();
            "pass_loop_cap"
        } else if let Ok(chain) = (self.chain_id)() {
            record["chain"] = chain.as_str().into();
            record["streak"] = (thread.streak + 1).into();
            record["action"] = "suppressed_continue".into();
            return self.write(record).then_some(chain);
        } else {
            "pass_inject_failed"
        };
        record["action"] = action.into();
        self.write(record);
        None
    }

    /// Appends `record` with its time to the log and syncs it; whether that worked.
    fn write(&self, mut record: Value) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        record["ts"] = agent::utc(now).into();
        let mut bytes = record.to_string().into_bytes();
        bytes.push(b'\n');
        let append = || -> io::Result<()> {
            if let Some(dir) = self.log.parent() {
                fs::create_dir_all(dir)?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.log)?;
            file.write_all(&bytes)?;
            file.sync_all()
        };
        append().is_ok()
    }
}

impl Policy for NoAskGuard {
    fn name(&self) -> &'static str {
        "no-ask-guard"
    }

    fn on_turn_start(&mut self, origin: Origin, input: &Message) {
        self.thread = (origin == Origin::TopLevelOwner).then(|| Thread {
            owner: text(input),
            chain: None,
            streak: 0,
        });
    }

    fn on_stop(&mut self, turn: &Message, interrupt: &Interrupt) -> StopDecision {
        let Some(mut thread) = self.thread.take() else {
            return StopDecision::Stop;
        };
        let reply = text(turn);
        let parent = thread.chain.take();
        let suppressed = if ends_with_question(&reply) {
            self.check(&thread, &reply, parent.as_deref(), interrupt)
        } else {
            None
        };
        let decision = if let Some(chain) = suppressed {
            let note = self.note.render(&[&chain]);
            thread.chain = Some(chain);
            thread.streak += 1;
            StopDecision::Continue(note)
        } else {
            if let Some(parent) = parent {
                self.write(json!({
                    "session_id": self.session,
                    "chain": parent,
                    "action": "continuation_done",
                    "reply_tail": last_chars(py_strip(&reply), LOG_TAIL_CHARS),
                }));
            }
            StopDecision::Stop
        };
        self.thread = Some(thread);
        decision
    }
}

/// Whether one of the last two non-blank lines of `text`, outside code blocks and URLs, has a
/// `?` in its last 600 characters; a text naming `MEDIA:` never does.
pub fn ends_with_question(text: &str) -> bool {
    let stripped = strip_markup(text);
    let text = py_strip(&stripped);
    if text.is_empty() || text.contains("MEDIA:") {
        return false;
    }
    let lines: Vec<&str> = py_splitlines(text, false)
        .map(py_strip)
        .filter(|line| !line.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(2)..].join(" ");
    last_chars(&tail, TAIL_CHARS).contains('?')
}

/// `text` with each ```` ``` ```` block and each `http://` or `https://` URL replaced by a space.
fn strip_markup(text: &str) -> String {
    let mut blocks = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let Some(close) = rest[open + 3..].find("```") else {
            break;
        };
        blocks.push_str(&rest[..open]);
        blocks.push(' ');
        rest = &rest[open + 3 + close + 3..];
    }
    blocks.push_str(rest);
    let mut out = String::with_capacity(blocks.len());
    let mut rest = blocks.as_str();
    while let Some(at) = rest.find("http") {
        let after = &rest[at + 4..];
        let body = after
            .strip_prefix("://")
            .or_else(|| after.strip_prefix("s://"));
        let length = body.map_or(0, |b| b.find(is_py_space).unwrap_or(b.len()));
        match body {
            Some(body) if length > 0 => {
                out.push_str(&rest[..at]);
                out.push(' ');
                rest = &body[length..];
            }
            _ => {
                out.push_str(&rest[..at + 4]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `text` stripped, its last `limit` characters after `…` when it is longer.
fn tail_of(text: &str, limit: usize) -> String {
    let text = py_strip(text);
    let tail = last_chars(text, limit);
    if tail.len() == text.len() {
        text.to_owned()
    } else {
        format!("…{tail}")
    }
}

/// The owner's request cut to its end, and the stripped reply in parts of at most 8000
/// characters, every part but the last followed by the reply's end.
fn prepare(owner: &str, reply: &str) -> (String, Vec<String>) {
    let owner = tail_of(owner, OWNER_CHARS);
    let reply = py_strip(reply);
    let chars: Vec<char> = reply.chars().collect();
    if chars.len() <= REPLY_PART_CHARS {
        return (owner, vec![reply.to_owned()]);
    }
    let suffix = format!("\n…\n{}", tail_of(reply, TAIL_CHARS));
    let size = REPLY_PART_CHARS - suffix.chars().count();
    let count = chars.len().div_ceil(size);
    let parts = chars
        .chunks(size)
        .enumerate()
        .map(|(n, part)| {
            let mut part = String::from_iter(part);
            if n + 1 < count {
                part.push_str(&suffix);
            }
            part
        })
        .collect();
    (owner, parts)
}

/// The verdict object spanning the first `{` to the last `}` of `answer`.
fn parse(answer: &str) -> Result<Ruling, String> {
    let span = answer
        .find('{')
        .zip(answer.rfind('}'))
        .filter(|(start, end)| start < end)
        .ok_or_else(|| "no JSON object".to_owned())?;
    let data: Value = serde_json::from_str(&answer[span.0..=span.1]).map_err(|e| e.to_string())?;
    let verdict = match data.get("verdict") {
        Some(Value::String(v)) if v == "extra" => "extra",
        Some(Value::String(v)) if v == "legit" => "legit",
        other => return Err(format!("bad verdict {other:?}")),
    };
    let field = |key: &str, limit: usize| -> String {
        let text = match data.get(key) {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
        };
        text.chars().take(limit).collect()
    };
    Ok(Ruling {
        verdict,
        category: field("category", 20),
        reason: field("reason", 200),
    })
}
