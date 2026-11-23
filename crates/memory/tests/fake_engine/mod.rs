//! A fake engine serving C6 `raw_sessions` on a Unix socket from synthetic rows, with the
//! engine's paging: rows after the cursor, at most the smaller of the asked `limit` and its own
//! page size, a cursor only when more follow.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};
use ulei_memory::wire::c6::{RawMessage, RawSessionsPage, RawSessionsQuery, RawSkipped};

/// One journal row: a served message or a message the engine skips.
#[derive(Clone, Debug)]
pub(crate) enum Row {
    Message(RawMessage),
    Skipped(RawSkipped),
}

#[derive(Default)]
struct State {
    rows: Vec<Row>,
    queries: Vec<RawSessionsQuery>,
    failing: Vec<usize>,
}

pub(crate) struct FakeEngine {
    socket: PathBuf,
    state: Arc<Mutex<State>>,
}

impl FakeEngine {
    /// Serves `rows` at `socket`, at most `page` rows per answer.
    pub(crate) fn start(socket: &Path, page: usize, rows: Vec<Row>) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        let state = Arc::new(Mutex::new(State {
            rows,
            ..State::default()
        }));
        let served = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head_end, length) = loop {
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0, "request cut short");
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                        assert!(head.starts_with("post /v1/raw_sessions http/1.1"), "{head}");
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .unwrap()
                            .trim()
                            .parse::<usize>()
                            .unwrap();
                        break (end + 4, length);
                    }
                };
                while buf.len() < head_end + length {
                    let n = stream.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let query: RawSessionsQuery = serde_json::from_slice(&buf[head_end..]).unwrap();
                let (status, body) = served.lock().unwrap().answer(query, page);
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                out.extend_from_slice(&body);
                let _ = stream.write_all(&out);
            }
        });
        Self {
            socket: socket.to_owned(),
            state,
        }
    }

    /// Appends rows to the journal.
    pub(crate) fn append(&self, rows: Vec<Row>) {
        self.state.lock().unwrap().rows.extend(rows);
    }

    /// Answers the `request`th request from now on, counted from 1 over the fake's life, with
    /// `500`.
    pub(crate) fn fail(&self, request: usize) {
        self.state.lock().unwrap().failing.push(request);
    }

    /// The cursors of the queries answered so far, in order.
    pub(crate) fn cursors(&self) -> Vec<Option<String>> {
        let state = self.state.lock().unwrap();
        state.queries.iter().map(|q| q.cursor.clone()).collect()
    }
}

impl Drop for FakeEngine {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

impl State {
    fn answer(&mut self, query: RawSessionsQuery, page: usize) -> (u16, Vec<u8>) {
        let after: usize = query.cursor.as_deref().map_or(0, |c| c.parse().unwrap());
        let limit = usize::try_from(query.limit.unwrap_or(10_000)).unwrap();
        self.queries.push(query);
        if self.failing.contains(&self.queries.len()) {
            let body = json!({"code": "internal", "message": "synthetic failure"});
            return (500, body.to_string().into_bytes());
        }
        let take = limit.min(page);
        let rows: Vec<&Row> = self.rows.iter().skip(after).take(take).collect();
        let more = after + rows.len() < self.rows.len();
        let mut out = RawSessionsPage {
            messages: Vec::new(),
            skipped: None,
            cursor: more.then(|| (after + rows.len()).to_string()),
        };
        for row in rows {
            match row {
                Row::Message(m) => out.messages.push(m.clone()),
                Row::Skipped(s) => out.skipped.get_or_insert_with(Vec::new).push(s.clone()),
            }
        }
        (200, serde_json::to_vec(&out).unwrap())
    }
}

/// The canonical C4G serialization of a one-text message.
pub(crate) fn c4g(role: &str, text: &str) -> String {
    let message: Value = json!({"role": role, "content": [{"type": "text", "text": text}]});
    message.to_string()
}

/// A served message of session `session` keyed `key` at `seq`.
pub(crate) fn message(key: &str, session: u64, seq: u64, role: &str, text: &str) -> Row {
    #[allow(clippy::cast_precision_loss)]
    let ts = Some(1_700_000_000.0 + (session * 100 + seq) as f64);
    Row::Message(RawMessage {
        session_id: session.to_string(),
        session_key: Some(key.to_owned()),
        seq: Some(seq),
        ts,
        role: role.to_owned(),
        content: text.to_owned(),
        message: Some(c4g(role, text)),
    })
}

/// A message the engine skips, of `len` bytes.
pub(crate) fn skipped(key: &str, session: u64, seq: u64, len: u64) -> Row {
    Row::Skipped(RawSkipped {
        session_id: session.to_string(),
        session_key: Some(key.to_owned()),
        seq: Some(seq),
        ts: Some(1_700_000_500.0),
        len,
    })
}
