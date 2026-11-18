// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The plan of the tool calls of one model step: ordered segments whose calls run concurrently
//! or one after another.

use std::ops::Range;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use ulei_contracts::generated::c4g::ToolCall;

use super::{PATCH, READ_FILE, SEARCH_FILES, WRITE_FILE, py_strip, realpath, resolve, v4a};

/// Interactive tools; a call of one always runs alone.
const NEVER_PARALLEL_TOOLS: [&str; 1] = ["clarify"];

/// Read-only tools with no shared mutable session state.
const PARALLEL_SAFE_TOOLS: [&str; 12] = [
    "ha_get_state",
    "ha_list_entities",
    "ha_list_services",
    "image_generate",
    READ_FILE,
    SEARCH_FILES,
    "session_search",
    "skill_view",
    "skills_list",
    "vision_analyze",
    "web_extract",
    "web_search",
];

/// File tools that may share an overlapping path with other readers in one parallel run.
const PATH_SCOPED_READERS: [&str; 2] = [READ_FILE, SEARCH_FILES];
/// File tools whose path conflicts with any overlapping path in one parallel run.
const PATH_SCOPED_WRITERS: [&str; 2] = [WRITE_FILE, PATCH];

/// Contiguous calls of a step, by index: run concurrently when `parallel`, else in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The calls run concurrently.
    pub parallel: bool,
    /// Indices of the calls.
    pub calls: Range<usize>,
}

/// Splits `calls` into segments in call order whose paths resolve against `cwd`. A parallel
/// segment is a run of at least two parallel-safe calls none of whose paths conflict: readers
/// may overlap readers, a writer overlaps nothing. Every other call joins a sequential segment.
pub fn plan(calls: &[ToolCall], cwd: &Path) -> Vec<Segment> {
    let mut planner = Planner::default();
    for (i, call) in calls.iter().enumerate() {
        let name = call.name.as_str();
        if NEVER_PARALLEL_TOOLS.contains(&name) {
            planner.sequential(i);
        } else if PATH_SCOPED_READERS.contains(&name) || PATH_SCOPED_WRITERS.contains(&name) {
            let scoped = scope_paths(name, &call.input, cwd);
            if scoped.is_empty() {
                planner.sequential(i);
                continue;
            }
            let writer = PATH_SCOPED_WRITERS.contains(&name);
            let conflict = scoped.iter().any(|path| {
                planner
                    .reserved
                    .iter()
                    .any(|(held, held_writer)| (writer || *held_writer) && overlap(path, held))
            });
            if conflict {
                planner.close(i);
            }
            planner
                .reserved
                .extend(scoped.into_iter().map(|path| (path, writer)));
        } else if !PARALLEL_SAFE_TOOLS.contains(&name) {
            planner.sequential(i);
        }
    }
    planner.close(calls.len());

    let mut segments: Vec<Segment> = Vec::new();
    for mut segment in planner.segments {
        segment.parallel &= segment.calls.len() >= 2;
        match segments.last_mut() {
            Some(last) if !last.parallel && !segment.parallel => last.calls.end = segment.calls.end,
            _ => segments.push(segment),
        }
    }
    segments
}

/// Segments planned so far, the start of the open parallel run and the paths it holds.
#[derive(Default)]
struct Planner {
    segments: Vec<Segment>,
    start: usize,
    reserved: Vec<(PathBuf, bool)>,
}

impl Planner {
    /// Ends the open parallel run before call `at`; `at` opens the next one.
    fn close(&mut self, at: usize) {
        if self.start < at {
            self.segments.push(Segment {
                parallel: true,
                calls: self.start..at,
            });
        }
        self.start = at;
        self.reserved.clear();
    }

    /// Call `at` runs in order after everything before it.
    fn sequential(&mut self, at: usize) {
        self.close(at);
        match self.segments.last_mut() {
            Some(last) if !last.parallel => last.calls.end = at + 1,
            _ => self.segments.push(Segment {
                parallel: false,
                calls: at..at + 1,
            }),
        }
        self.start = at + 1;
    }
}

/// The canonical paths a call of the path-scoped tool `name` holds, without duplicates: the
/// header paths of a V4A `patch`, else `path`, else `.` for `search_files`. Empty when the
/// planner cannot tell.
fn scope_paths(name: &str, input: &Map<String, Value>, cwd: &Path) -> Vec<PathBuf> {
    let raw: Vec<&str> =
        if name == PATCH && input.get("mode").and_then(Value::as_str) == Some("patch") {
            match input.get("patch") {
                Some(Value::String(body)) => v4a::header_paths(body),
                _ => Vec::new(),
            }
        } else {
            match input.get("path") {
                Some(Value::String(path)) if !py_strip(path).is_empty() => vec![path.as_str()],
                _ if name == SEARCH_FILES => vec!["."],
                _ => Vec::new(),
            }
        };
    let mut scoped: Vec<PathBuf> = Vec::new();
    for path in raw.into_iter().filter(|p| !py_strip(p).is_empty()) {
        let canonical = realpath(&resolve(cwd, path));
        if !scoped.contains(&canonical) {
            scoped.push(canonical);
        }
    }
    scoped
}

/// Whether one canonical path lies under the other, or they are equal.
fn overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}
