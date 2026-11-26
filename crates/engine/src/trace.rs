//! The debug trace of the engine's turns (ADR-0092).
//!
//! With `engine.trace_file` set, the engine appends one JSON object per line for each turn, its
//! turn context, each model request, each tool call and each compaction: what the journal does
//! not keep (the text appended to the sent copy of the input, the garden's `pointers` call, the
//! tool definitions, times) and nothing it keeps (no message, tool argument or result text).
//! The model never sees the file. Unset, nothing is written; tool calls are timed either way
//! (two clock reads each). A write that fails is one stderr line per process and never fails or
//! delays the turn beyond the write itself.
//!
//! Every line carries `v` ([`VERSION`]), `at` (Unix seconds with milliseconds), `kind`,
//! `session` and `turn`; the other fields per kind are listed in ADR-0092. Past
//! [`MAX_BYTES`] the file is renamed to `<file>.1`, replacing an older one, before the next
//! line.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use ulei_contracts::generated::c4g::Tool;

use crate::tools::hex;

/// Version of the line format; a reader skips the lines of another version.
pub const VERSION: u64 = 1;
/// Size at which the file is renamed to `<file>.1` before the next line.
pub const MAX_BYTES: u64 = 64 << 20;
/// Longest text of a `turn_end` line's `ended`.
const ENDED_CHARS: usize = 300;

/// Serializes the appends and the rotation of this process: the C6 server runs turns on threads.
static FILE: Mutex<()> = Mutex::new(());
/// Hashes of the tool sets this process has already written.
static TOOL_SETS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Set once a failure has been reported.
static WARNED: AtomicBool = AtomicBool::new(false);

/// The trace of one turn: the file and the turn's numbers every line carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    path: PathBuf,
    max_bytes: u64,
    session: u64,
    turn: u64,
}

impl Trace {
    /// The trace of turn `turn` of session `session` in the file `path`.
    #[must_use]
    pub fn new(path: PathBuf, session: u64, turn: u64) -> Self {
        Self {
            path,
            max_bytes: MAX_BYTES,
            session,
            turn,
        }
    }

    /// The same trace rotating at `max_bytes` instead of [`MAX_BYTES`].
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// The file the lines go to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one line of `kind` with the fields of the object `fields`.
    pub fn record(&self, kind: &str, fields: Value) {
        let mut line = Map::new();
        line.insert("v".to_owned(), json!(VERSION));
        line.insert("at".to_owned(), json!(now()));
        line.insert("kind".to_owned(), json!(kind));
        line.insert("session".to_owned(), json!(self.session));
        line.insert("turn".to_owned(), json!(self.turn));
        if let Value::Object(fields) = fields {
            for (key, value) in fields {
                line.insert(key, value);
            }
        }
        let mut bytes = match serde_json::to_vec(&Value::Object(line)) {
            Ok(bytes) => bytes,
            Err(e) => return warn(&e),
        };
        bytes.push(b'\n');
        if let Err(e) = self.append(&bytes) {
            warn(&format_args!("{}: {e}", self.path.display()));
        }
    }

    /// The hash of `tools` as a request sends them; the first time this process meets the set,
    /// also a `tools` line with the definitions.
    pub fn tools(&self, tools: &[Tool]) -> Option<String> {
        let bytes = serde_json::to_vec(tools).ok()?;
        let hash = tool_hash(&bytes);
        let first = {
            let mut seen = TOOL_SETS.lock().unwrap_or_else(PoisonError::into_inner);
            let first = !seen.contains(&hash);
            if first {
                seen.push(hash.clone());
            }
            first
        };
        if first {
            self.record(
                "tools",
                json!({"hash": hash, "count": tools.len(), "bytes": bytes.len(), "tools": tools}),
            );
        }
        Some(hash)
    }

    /// The `turn_end` line: how many steps sent a request and how the loop ended, its debug
    /// text cut to [`ENDED_CHARS`] characters.
    pub fn turn_end(&self, steps: usize, ended: &dyn std::fmt::Debug) {
        let ended: String = format!("{ended:?}").chars().take(ENDED_CHARS).collect();
        self.record("turn_end", json!({"steps": steps, "ended": ended}));
    }

    fn append(&self, bytes: &[u8]) -> io::Result<()> {
        let _file = FILE.lock().unwrap_or_else(PoisonError::into_inner);
        if fs::metadata(&self.path).is_ok_and(|m| m.len() >= self.max_bytes) {
            let mut rotated = self.path.clone().into_os_string();
            rotated.push(".1");
            fs::rename(&self.path, rotated)?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?
            .write_all(bytes)
    }
}

/// The first 16 hex digits of the SHA-256 of a tool set's JSON.
fn tool_hash(json: &[u8]) -> String {
    hex(Sha256::digest(json).get(..8).unwrap_or_default())
}

/// The time now as Unix seconds with milliseconds; 0 before the epoch.
#[must_use]
pub fn now() -> f64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    // Milliseconds since 1970 stay far below 2^53, so the conversion is exact.
    #[allow(clippy::cast_precision_loss)]
    let seconds = millis as f64 / 1000.0;
    seconds
}

/// Milliseconds between two [`now`] values, never negative.
#[must_use]
pub fn millis(start: f64, end: f64) -> f64 {
    ((end - start) * 1000.0).max(0.0).round()
}

fn warn(error: &dyn std::fmt::Display) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        let _ = writeln!(
            io::stderr().lock(),
            "ulei-engine: trace: {error}; later trace failures are not reported"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn lines(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ulei-trace-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("trace.ndjson")
    }

    #[test]
    fn a_line_carries_the_common_fields_and_its_own() {
        let path = temp("fields");
        Trace::new(path.clone(), 2, 7).record("tool", json!({"name": "terminal", "ms": 12.0}));
        let got = lines(&path);
        assert_eq!(got.len(), 1);
        let line = &got[0];
        assert_eq!(line["v"], json!(VERSION));
        assert_eq!(line["kind"], json!("tool"));
        assert_eq!(
            (line["session"].clone(), line["turn"].clone()),
            (json!(2), json!(7))
        );
        assert_eq!(line["name"], json!("terminal"));
        assert!(line["at"].as_f64().unwrap() > 1.7e9);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_tool_set_is_written_once_per_process() {
        let path = temp("tools");
        let trace = Trace::new(path.clone(), 1, 1);
        let tools = vec![Tool {
            name: "trace_test_tool".to_owned(),
            description: "d".to_owned(),
            input_schema: Map::new(),
            native: None,
        }];
        let first = trace.tools(&tools).unwrap();
        let again = trace.tools(&tools).unwrap();
        assert_eq!(first, again);
        assert_eq!(first.len(), 16);
        let got = lines(&path);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["kind"], json!("tools"));
        assert_eq!(got[0]["tools"][0]["name"], json!("trace_test_tool"));
    }

    #[test]
    fn the_file_rotates_past_its_size() {
        let path = temp("rotate");
        let trace = Trace::new(path.clone(), 1, 1).with_max_bytes(200);
        for i in 0..10 {
            trace.record("request", json!({"i": i}));
        }
        let mut rotated = path.clone().into_os_string();
        rotated.push(".1");
        let kept = lines(&path);
        let older = lines(Path::new(&rotated));
        assert!(!kept.is_empty() && !older.is_empty());
        assert_eq!(kept.last().unwrap()["i"], json!(9));
        assert!(fs::metadata(&path).unwrap().len() < 400);
    }

    #[test]
    fn a_failed_write_does_not_panic() {
        let trace = Trace::new(PathBuf::from("/nonexistent-dir/trace.ndjson"), 1, 1);
        trace.record("turn", json!({}));
        trace.turn_end(0, &"Interrupted");
    }

    #[test]
    fn millis_is_never_negative() {
        assert_eq!(millis(10.0, 10.25), 250.0);
        assert_eq!(millis(10.0, 9.0), 0.0);
    }
}
