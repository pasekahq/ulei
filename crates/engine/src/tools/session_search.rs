// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The `session_search` tool over the journal's visible history in four shapes chosen by its
//! arguments: discovery, scroll, read and browse. Discovery and scroll leave out the current
//! session's live messages and find its archived ones.

use std::path::PathBuf;

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::{
    Args, Context, Output, SESSION_SEARCH, is_py_space, json_array, json_object, json_str,
    object_schema, py_int, py_json, py_strip, record_id, strip_ansi,
};
use crate::journal::history::{Around, Entry};
use crate::journal::search::{Hit, MessageRole, Search, Sort};
use crate::journal::{Error, Journal};
use crate::zone::rfc3339;

const DEFAULT_LIMIT: i64 = 3;
const MAX_LIMIT: i64 = 10;
const DEFAULT_WINDOW: i64 = 5;
const MAX_WINDOW: i64 = 20;
/// Index rows discovery reads before keeping the best hit of each session.
const DISCOVER_SCAN_LIMIT: u32 = 300;
/// Messages on each side of a discovery hit.
const HIT_WINDOW: usize = 5;
/// User and assistant messages at each end of a full discovery result.
const BOOKEND: usize = 3;
const MESSAGE_CHARS: usize = 4_000;
const BOOKEND_CHARS: usize = 1_200;
const READ_HEAD: usize = 20;
const READ_TAIL: usize = 10;
/// Characters of the first user message a preview starts from, and the most it keeps.
const PREVIEW_HEAD_CHARS: usize = 63;
const PREVIEW_MAX_CHARS: usize = 60;
/// Starts of a generated compaction handoff, kept out of bookends.
const COMPACTION_PREFIXES: [&str; 2] = ["[CONTEXT COMPACTION", "[CONTEXT SUMMARY]:"];

const DESCRIPTION: &str = concat!(
    "Search past sessions stored in the local session DB, or scroll inside one. ",
    "FTS5-backed retrieval over the SQLite message store. No LLM calls \u{2014} every ",
    "shape returns actual messages from the DB.\n\n",
    "SOURCE-FIRST LIMIT\n\n",
    "  This tool searches conversation history only. It is not evidence ",
    "about the current contents of external sources. If the user provided a ",
    "direct source such as a URL, phone number/contact, app/thread, file path, ",
    "account, website, or live system, inspect that original source before or ",
    "instead of session_search when accessible. Use session_search as secondary ",
    "context for what was previously said, not as primary proof of what the ",
    "source currently contains. If the original source is inaccessible, say so ",
    "and why before falling back to session history. Do not conclude 'not found' ",
    "or 'no prior correspondence' from session_search alone when a direct source ",
    "was provided.\n\n",
    "FOUR CALLING SHAPES\n\n",
    "  1) DISCOVERY \u{2014} pass `query`:\n",
    "     session_search(query=\"auth refactor\", limit=3)\n",
    "     Runs FTS5, dedupes hits by session, and returns the top N ",
    "sessions. Adaptive detail is the default: the top-ranked result carries ",
    "full context, while lower-ranked results stay compact. Pass `detail=\"full\"` ",
    "to fully hydrate every result. Pass `session_id` to search inside one session ",
    "and `since`/`until` (RFC 3339 or YYYY-MM-DD, UTC) to bound the time of the ",
    "match. Every result carries:\n",
    "       - session_id, session_key, when\n",
    "       - snippet: FTS5-highlighted match excerpt\n",
    "       - detail: `full` or `compact`\n",
    "       - bookend_start/bookend_end: the first/last 3 user+assistant messages ",
    "for full results; empty lists for compact results\n",
    "       - messages: ±5 messages around the FTS5 match for full results; only ",
    "the flagged anchor message for compact results\n",
    "       - match_message_id, messages_before, messages_after\n",
    "     The top result's bookends + window let you reconstruct goal → match → ",
    "resolution immediately. Scroll a compact result when another session looks ",
    "more promising.\n\n",
    "  2) SCROLL \u{2014} pass `session_id` + `around_message_id`:\n",
    "     session_search(session_id=\"...\", around_message_id=12345, window=10)\n",
    "     Returns a window of ±`window` messages centered on the anchor. No FTS5, ",
    "no bookends \u{2014} just the slice. Use after a discovery call when you need more ",
    "context than the ±5 default window.\n",
    "       - To scroll FORWARD: pass messages[-1].id back as around_message_id.\n",
    "       - To scroll BACKWARD: pass messages[0].id back as around_message_id.\n",
    "       - The boundary message appears in both windows \u{2014} orientation marker.\n",
    "       - When messages_before or messages_after is < window, you're at the ",
    "start or end of the session.\n\n",
    "  3) READ \u{2014} pass `session_id` only (no around_message_id, no query):\n",
    "     session_search(session_id=\"...\")\n",
    "     Dumps the whole session by id (first 20 + last 10 messages when ",
    "large).\n\n",
    "  4) BROWSE \u{2014} no args:\n",
    "     session_search()\n",
    "     Returns recent sessions chronologically: session keys, previews, timestamps. ",
    "Use when the user asks \"what was I working on\" without naming a topic.\n\n",
    "FTS5 SYNTAX\n\n",
    "  AND is the default \u{2014} multi-word queries require all terms. Use OR explicitly ",
    "for broader recall (`alpha OR beta OR gamma`), quoted phrases for exact match ",
    "(`\"docker networking\"`), boolean (`python NOT java`), or prefix wildcards ",
    "(`deploy*`).\n\n",
    "WHEN TO USE\n\n",
    "  Reach for this on questions about conversation history itself, such ",
    "as \"what did we do about X\", \"where did we leave Y\", or \"find the ",
    "session where Z\". If the user provided a direct source identifier, inspect ",
    "that source first when accessible; session_search can then supply historical ",
    "context. The session DB carries what was said when; external tools show ",
    "current source/world state."
);

const SCROLL_REJECTED: &str =
    "scroll rejected: anchor lives in the current session lineage (already in your active context)";

/// The tool of one turn: the journal it reads and the session the turn runs in.
pub struct SessionSearch {
    journal: PathBuf,
    current: u64,
}

impl SessionSearch {
    /// The tool of a turn of session `current` over the journal file `journal`.
    pub fn new(journal: PathBuf, current: u64) -> Self {
        Self { journal, current }
    }

    fn call(&self, journal: &Journal, args: &Args<'_>) -> Output {
        let text = |name: &str| {
            args.present(name)
                .and_then(Value::as_str)
                .map(py_strip)
                .filter(|text| !text.is_empty())
        };
        let session_id = text("session_id");
        let query = text("query");
        if let Some(session_id) = session_id {
            if let Some(anchor) = args.present("around_message_id") {
                return self.scroll(journal, session_id, anchor, args.present("window"));
            }
            if query.is_none() {
                return read(journal, session_id);
            }
        }
        let limit = py_int(args.present("limit"))
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);
        let Some(query) = query else {
            return self.browse(journal, limit);
        };
        let roles: Vec<&str> = match text("role_filter") {
            Some(filter) => filter
                .split(',')
                .map(py_strip)
                .filter(|r| !r.is_empty())
                .collect(),
            None => vec!["user", "assistant"],
        };
        let sort = match text("sort").map(str::to_lowercase).as_deref() {
            Some("newest") => Sort::Newest,
            Some("oldest") => Sort::Oldest,
            _ => Sort::Rank,
        };
        let full = text("detail").is_some_and(|d| d.to_lowercase() == "full");
        let (since, until) = match (args.opt_utc("since", false), args.opt_utc("until", true)) {
            (Ok(since), Ok(until)) => (since, until),
            (Err(e), _) | (_, Err(e)) => return failure(e),
        };
        let discovery = Discovery {
            query,
            roles: roles.iter().filter_map(|r| role(r)).collect(),
            session: session_id.and_then(record_id),
            void: session_id.is_some_and(|id| record_id(id).is_none()),
            since,
            until,
            sort,
            limit: usize::try_from(limit).unwrap_or(1),
            full,
        };
        self.discover(journal, &discovery)
            .unwrap_or_else(|e| failure(format!("Search failed: {e}")))
    }

    fn discover(&self, journal: &Journal, d: &Discovery<'_>) -> Result<Output, Error> {
        let detail = if d.full { "full" } else { "adaptive" };
        let hits = if d.roles.is_empty() || d.void {
            Vec::new()
        } else {
            journal.search(&Search {
                query: d.query,
                roles: &d.roles,
                session: d.session,
                since: d.since,
                until: d.until,
                sort: d.sort,
                limit: DISCOVER_SCAN_LIMIT,
            })?
        };
        if hits.is_empty() {
            return Ok(Output::ok(json_object([
                ("success", "true".into()),
                ("mode", json_str("discover")),
                ("query", json_str(d.query)),
                ("detail", json_str(detail)),
                ("results", "[]".into()),
                ("count", "0".into()),
                ("message", json_str("No matching sessions found.")),
            ])));
        }
        let mut seen: Vec<Hit> = Vec::new();
        for hit in hits {
            if seen.len() >= d.limit {
                break;
            }
            if hit.session == self.current {
                let archived = journal.placement(hit.n)?.is_some_and(|(_, live)| !live);
                if !archived {
                    continue;
                }
            }
            if !seen.iter().any(|s| s.session == hit.session) {
                seen.push(hit);
            }
        }
        let mut results = Vec::with_capacity(seen.len());
        for hit in &seen {
            let full = d.full || results.is_empty();
            results.push(result(journal, hit, full)?);
        }
        let count = results.len();
        Ok(Output::ok(json_object([
            ("success", "true".into()),
            ("mode", json_str("discover")),
            ("query", json_str(d.query)),
            ("detail", json_str(detail)),
            ("results", json_array(results)),
            ("count", count.to_string()),
            ("sessions_searched", seen.len().to_string()),
        ])))
    }

    fn scroll(
        &self,
        journal: &Journal,
        session_id: &str,
        anchor: &Value,
        window: Option<&Value>,
    ) -> Output {
        let Some(anchor) = py_int(Some(anchor)) else {
            return failure("scroll requires integer around_message_id");
        };
        let window = py_int(window)
            .unwrap_or(DEFAULT_WINDOW)
            .clamp(1, MAX_WINDOW);
        self.window(journal, session_id, anchor, window)
            .unwrap_or_else(|e| failure(format!("failed to load messages: {e}")))
    }

    fn window(
        &self,
        journal: &Journal,
        session_id: &str,
        anchor: i64,
        window: i64,
    ) -> Result<Output, Error> {
        let session = record_id(session_id);
        let n = u64::try_from(anchor).ok();
        let placement = n.map(|n| journal.placement(n)).transpose()?.flatten();
        let owner = placement.map(|(owner, _)| owner).or(session);
        if owner == Some(self.current) && !matches!(placement, Some((_, false))) {
            return Ok(failure(SCROLL_REJECTED));
        }
        let Some(session) = known(journal, session)? else {
            return Ok(failure(format!("session_id not found: {session_id}")));
        };
        let view = match n {
            Some(n) => journal.around(session, n, usize::try_from(window).unwrap_or(1))?,
            None => Around::default(),
        };
        if view.window.is_empty() {
            return Ok(failure(format!(
                "around_message_id {anchor} not in session_id {session_id}"
            )));
        }
        Ok(Output::ok(json_object([
            ("success", "true".into()),
            ("mode", json_str("scroll")),
            ("session_id", json_str(&session.to_string())),
            ("around_message_id", anchor.to_string()),
            ("session_meta", meta(journal, session)?),
            ("window", window.to_string()),
            (
                "messages",
                json_array(
                    view.window
                        .iter()
                        .map(|m| shape(m, n, None))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            ),
            ("messages_before", view.before.to_string()),
            ("messages_after", view.after.to_string()),
        ])))
    }

    fn browse(&self, journal: &Journal, limit: i64) -> Output {
        let listed = match journal.recent(self.current, usize::try_from(limit).unwrap_or(1)) {
            Ok(listed) => listed,
            Err(e) => return failure(format!("Failed to list recent sessions: {e}")),
        };
        let at = |at: Option<u64>| at.map_or_else(|| "null".into(), |at| at.to_string());
        let results: Vec<String> = listed
            .iter()
            .map(|s| {
                json_object([
                    ("session_id", json_str(&s.session.to_string())),
                    ("session_key", json_str(&s.key)),
                    ("started_at", at(s.started)),
                    ("last_active", at(s.last_active)),
                    ("message_count", s.count.to_string()),
                    ("preview", json_str(&preview(s.opening.as_deref()))),
                ])
            })
            .collect();
        let count = results.len();
        Output::ok(json_object([
            ("success", "true".into()),
            ("mode", json_str("browse")),
            ("results", json_array(results)),
            ("count", count.to_string()),
            (
                "message",
                json_str(&format!(
                    "Showing {count} most recent sessions. Pass a query= to search, or session_id+around_message_id to scroll."
                )),
            ),
        ]))
    }
}

impl super::Tool for SessionSearch {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: SESSION_SEARCH.into(),
            description: DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "Search query (discovery shape). Keywords, phrases, or boolean expressions to find in past sessions. Omit to browse recent sessions. Ignored when session_id + around_message_id are set (scroll shape)."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Discovery shape only. Max sessions to return (default 3, max 10). Bump to 5–10 when the topic likely spans several sessions and you want to pick the right one to scroll into.",
                        "default": 3
                    },
                    "sort": {
                        "type": "string",
                        "enum": ["newest", "oldest"],
                        "description": "Discovery shape only. Temporal bias on top of FTS5 ranking. Omit to keep relevance-only ordering (suitable for exploratory recall \u{2014} \"what do we know about X\"). Set 'newest' for recency-shaped questions (\"where did we leave X\"). Set 'oldest' for origin-shaped questions (\"how did X start\"). Ignored in scroll and browse shapes."
                    },
                    "detail": {
                        "type": "string",
                        "enum": ["adaptive", "full"],
                        "description": "Discovery shape only. 'adaptive' (default) fully hydrates the top-ranked result and returns only the exact anchor message for lower-ranked results. 'full' returns bookends and the complete anchored window for every result.",
                        "default": "adaptive"
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Scroll shape. Session to read inside. Use the session_id returned from a prior discovery call. Must be paired with around_message_id. With query and no around_message_id, discovery searches only this session."
                    },
                    "around_message_id": {
                        "type": "integer",
                        "description": "Scroll shape. Message id to center the window on. From a discovery result use match_message_id, or any id seen in a prior window. To scroll forward pass the last window message's id; to scroll backward pass the first."
                    },
                    "window": {
                        "type": "integer",
                        "description": "Scroll shape only. Messages to return on each side of the anchor (anchor itself always included). Clamped to [1, 20]. Default 5.",
                        "default": 5
                    },
                    "role_filter": {
                        "type": "string",
                        "description": "Optional. Comma-separated roles to include. Discovery defaults to 'user,assistant' (tool output is usually noise). Pass 'user,assistant,tool' to include tool output (debugging tool behaviour) or 'tool' to search tool output only."
                    },
                    "since": {
                        "type": "string",
                        "description": "Discovery shape only. Earliest time of a match: RFC 3339 or YYYY-MM-DD, UTC."
                    },
                    "until": {
                        "type": "string",
                        "description": "Discovery shape only. Latest time of a match: RFC 3339 or YYYY-MM-DD (the whole day), UTC."
                    }
                }),
                &[],
            ),
            native: None,
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        match Journal::open(&self.journal) {
            Ok(journal) => self.call(&journal, &Args(input)),
            Err(e) => failure(format!("Session database not available: {e}.")),
        }
    }
}

/// The arguments of one discovery call.
struct Discovery<'a> {
    query: &'a str,
    /// Known roles a hit may have; empty when the filter names none.
    roles: Vec<MessageRole>,
    /// The only session a hit may come from.
    session: Option<u64>,
    /// A `session_id` that names no session: nothing matches.
    void: bool,
    since: Option<i64>,
    until: Option<i64>,
    sort: Sort,
    limit: usize,
    full: bool,
}

/// The read shape: the visible messages of the session, its first and last ones when long.
fn read(journal: &Journal, session_id: &str) -> Output {
    let loaded = known(journal, record_id(session_id)).and_then(|session| {
        let Some(session) = session else {
            return Ok(None);
        };
        let shaped = journal
            .history(session)?
            .iter()
            .map(|m| shape(m, None, None))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some((session, shaped, meta(journal, session)?)))
    });
    let (session, shaped, meta) = match loaded {
        Ok(Some(read)) => read,
        Ok(None) => return failure(format!("session_id not found: {session_id}")),
        Err(e) => return failure(format!("failed to load session: {e}")),
    };
    let total = shaped.len();
    let truncated = total > READ_HEAD + READ_TAIL;
    let messages = if truncated {
        shaped[..READ_HEAD]
            .iter()
            .chain(&shaped[total - READ_TAIL..])
            .cloned()
            .collect()
    } else {
        shaped
    };
    let mut fields = vec![
        ("success", "true".into()),
        ("mode", json_str("read")),
        ("session_id", json_str(&session.to_string())),
        ("session_meta", meta),
        ("message_count", total.to_string()),
        ("truncated", truncated.to_string()),
        ("messages", json_array(messages)),
    ];
    if truncated {
        fields.push((
            "message",
            json_str(&format!(
                "Session has {total} messages; showing first {READ_HEAD} + last {READ_TAIL}. Pass around_message_id (any id above) to scroll the middle."
            )),
        ));
    }
    Output::ok(json_object(fields))
}

/// One discovery result for `hit`: its session and the window around it, with bookends and
/// every user and assistant message of the window when `full`, only the hit otherwise.
fn result(journal: &Journal, hit: &Hit, full: bool) -> Result<String, Error> {
    let view = journal.around(hit.session, hit.n, HIT_WINDOW)?;
    let messages = view
        .window
        .iter()
        .filter(|m| {
            m.n == hit.n || (full && matches!(m.role, MessageRole::User | MessageRole::Assistant))
        })
        .map(|m| shape(m, Some(hit.n), Some(MESSAGE_CHARS)))
        .collect::<Result<Vec<_>, _>>()?;
    let (start, end) = match (full, view.window.first(), view.window.last()) {
        (true, Some(first), Some(last)) => {
            journal.bookends(hit.session, first.n, last.n, BOOKEND)?
        }
        _ => (Vec::new(), Vec::new()),
    };
    let bookend = |rows: &[Entry]| -> Result<String, Error> {
        Ok(json_array(
            rows.iter()
                .filter(|m| !is_compaction_summary(&m.content))
                .map(|m| shape(m, None, Some(BOOKEND_CHARS)))
                .collect::<Result<Vec<_>, _>>()?,
        ))
    };
    let mut fields = vec![("session_id", json_str(&hit.session.to_string()))];
    if let Some(at) = journal.started(hit.session)? {
        fields.push(("when", json_str(&rfc3339(at))));
    }
    fields.extend([
        ("session_key", key_json(journal, hit.session)?),
        ("matched_role", json_str(hit.role.as_str())),
        ("match_message_id", hit.n.to_string()),
        ("snippet", json_str(&hit.snippet)),
        ("bookend_start", bookend(&start)?),
        ("messages", json_array(messages)),
        ("bookend_end", bookend(&end)?),
        ("messages_before", view.before.to_string()),
        ("messages_after", view.after.to_string()),
        ("detail", json_str(if full { "full" } else { "compact" })),
    ]);
    Ok(json_object(fields))
}

/// A message as the shapes return it, flagged when it is `anchor`, its text cut to `max`
/// characters followed by `…` when longer.
fn shape(m: &Entry, anchor: Option<u64>, max: Option<usize>) -> Result<String, Error> {
    let content = if m.content.contains('\u{1b}') {
        strip_ansi(&m.content)
    } else {
        m.content.as_str().into()
    };
    let chars = content.chars().count();
    let cut = max.filter(|max| chars > *max);
    let mut fields = vec![("id", m.n.to_string()), ("role", json_str(m.role.as_str()))];
    match cut {
        Some(max) => {
            let head: String = content.chars().take(max).collect();
            fields.push(("content", json_str(&format!("{head}…"))));
        }
        None => fields.push(("content", json_str(&content))),
    }
    if let Some(at) = m.at {
        fields.push(("timestamp", at.to_string()));
    }
    if let Some(name) = m.tool_name.as_deref().filter(|n| !n.is_empty()) {
        fields.push(("tool_name", json_str(name)));
    }
    if let Some(calls) = &m.tool_calls {
        let calls: Value = serde_json::from_str(calls)?;
        fields.push(("tool_calls", py_json(&calls)));
    }
    if anchor == Some(m.n) {
        fields.push(("anchor", "true".into()));
    }
    if cut.is_some() {
        fields.push(("content_truncated", "true".into()));
        fields.push(("original_content_chars", chars.to_string()));
    }
    Ok(json_object(fields))
}

/// `session_meta` of the read and scroll shapes.
fn meta(journal: &Journal, session: u64) -> Result<String, Error> {
    let mut fields = Vec::new();
    if let Some(at) = journal.started(session)? {
        fields.push(("when", json_str(&rfc3339(at))));
    }
    fields.push(("session_key", key_json(journal, session)?));
    Ok(json_object(fields))
}

fn key_json(journal: &Journal, session: u64) -> Result<String, Error> {
    Ok(journal
        .session_key(session)?
        .map_or_else(|| "null".into(), |key| json_str(&key)))
}

/// `session` when the journal has it.
fn known(journal: &Journal, session: Option<u64>) -> Result<Option<u64>, Error> {
    match session {
        Some(id) if journal.session_key(id)?.is_some() => Ok(Some(id)),
        _ => Ok(None),
    }
}

fn role(name: &str) -> Option<MessageRole> {
    match name {
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        "tool" => Some(MessageRole::Tool),
        "system" => Some(MessageRole::System),
        _ => None,
    }
}

/// Whether `content` starts, after whitespace, like a generated compaction handoff.
fn is_compaction_summary(content: &str) -> bool {
    let stripped = content.trim_start_matches(is_py_space);
    COMPACTION_PREFIXES.iter().any(|p| stripped.starts_with(p))
}

/// The browse preview of a session's first user message: its head on one line, cut to
/// [`PREVIEW_MAX_CHARS`] characters followed by `...` when longer.
fn preview(opening: Option<&str>) -> String {
    let head: String = opening
        .unwrap_or_default()
        .chars()
        .map(|c| if matches!(c, '\n' | '\r') { ' ' } else { c })
        .take(PREVIEW_HEAD_CHARS)
        .collect();
    let text = py_strip(&head);
    if text.chars().count() > PREVIEW_MAX_CHARS {
        format!(
            "{}...",
            text.chars().take(PREVIEW_MAX_CHARS).collect::<String>()
        )
    } else {
        text.to_owned()
    }
}

/// A `{"error": <message>, "success": false}` answer.
fn failure(message: impl std::fmt::Display) -> Output {
    Output::error_with(message, [("success", "false".into())])
}
